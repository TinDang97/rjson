//! Direct CPython-API JSON serializer backing `dumps` (-> `str`) and
//! `dumps_bytes` (-> `bytes`).
//!
//! Design notes
//! - Type dispatch is a chain of exact `ob_type` pointer comparisons against the
//!   builtin type objects; no PyO3 wrappers, no per-element refcounting.
//! - Output is written straight into the result object (`Out`): a `bytes`
//!   object, or a compact ASCII `str`, sized from the previous output length on
//!   this thread, grown with realloc and shortened at the end. No final copy,
//!   and no shared buffer that a re-entrant call could trip over.
//! - Every write reserves its worst case before writing through raw pointers, so
//!   no write can go past the allocation (the old SIMD escaper could).
//! - Separators are fused into the next value's write (one length update per
//!   element instead of two).
//! - Numbers: ints are read inline from the digit array (layout per Python
//!   version, verified at init) and formatted with a small inline formatter or
//!   itoap; floats are formatted in place with zmij (shortest round-trip, same
//!   output as orjson, e.g. `1e+16`).
//! - String escaping: AVX-512VL masked-load kernel when available (runtime
//!   check), else SSE2 with an AVX2 variant for longer strings; escapes are
//!   processed per 16/32-byte block from the compare bitmask.
//! - Strings: compact ASCII `str` objects are read straight from their inline
//!   data (`PyASCIIObject + 1`, which is version-correct because pyo3-ffi's
//!   struct layout follows the target interpreter). Everything else goes through
//!   `PyUnicode_DATA` / the UTF-8 cache, so subclasses and non-compact strings
//!   are handled correctly.
//! - `str` output: when a non-ASCII string is written we do not UTF-8 encode it.
//!   We leave a hole in the (pure ASCII) buffer and remember the source object.
//!   At the end the result `str` is allocated once with the exact kind and length
//!   and filled by widening the ASCII runs and copying the source strings' native
//!   UCS1/UCS2/UCS4 data. This avoids both the UTF-8 encode and the full UTF-8
//!   decode that `PyUnicode_FromStringAndSize` would do.
//! - Recursion is limited to `RECURSION_LIMIT` nested containers, which also
//!   turns circular references into an error instead of a stack overflow.

use pyo3::ffi;
use pyo3::prelude::*;
use std::cell::Cell;
use std::ptr;

/// Maximum container nesting depth (same as orjson).
pub const RECURSION_LIMIT: u32 = 254;

/// Strings longer than this are escaped in chunks so that the worst-case
/// reservation (6x) stays bounded.
const ESCAPE_CHUNK: usize = 64 * 1024;

/// Error kind; kept payload-free (1 byte) so `Result<(), SerError>` is
/// returned in a register. Payloads live in `Serializer::err_*`.
#[derive(Clone, Copy)]
pub enum SerError {
    NonFinite,
    Unsupported,
    KeyNotStr,
    Recursion,
    /// A Python exception is already set (e.g. lone surrogate, int too large to
    /// convert, MemoryError).
    PyErrSet,
}

type SerResult = Result<(), SerError>;

// ---------------------------------------------------------------------------
// Type objects
// ---------------------------------------------------------------------------

macro_rules! type_ptr {
    ($name:ident) => {
        ptr::addr_of_mut!(ffi::$name)
    };
}

#[inline(always)]
fn str_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyUnicode_Type)
}
#[inline(always)]
fn int_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyLong_Type)
}
#[inline(always)]
fn float_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyFloat_Type)
}
#[inline(always)]
fn bool_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyBool_Type)
}
#[inline(always)]
fn list_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyList_Type)
}
#[inline(always)]
fn dict_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyDict_Type)
}
#[inline(always)]
fn tuple_type() -> *mut ffi::PyTypeObject {
    type_ptr!(PyTuple_Type)
}

// ---------------------------------------------------------------------------
// Inline int access
// ---------------------------------------------------------------------------

/// Whether ints can be decoded by reading their digits inline. Verified at
/// module init (30-bit digits stored in u32, the default CPython build).
static mut INLINE_INT: bool = false;

#[cfg(Py_3_12)]
#[repr(C)]
struct LongHeader {
    ob_base: ffi::PyObject,
    lv_tag: usize,
    ob_digit: [u32; 2],
}

#[cfg(not(Py_3_12))]
#[repr(C)]
struct LongHeader {
    ob_base: ffi::PyVarObject,
    ob_digit: [u32; 2],
}

/// (is_negative, number of 30-bit digits) of an exact int.
#[inline(always)]
unsafe fn int_shape(obj: *mut ffi::PyObject) -> (bool, usize) {
    let h = obj as *const LongHeader;
    #[cfg(Py_3_12)]
    {
        let tag = (*h).lv_tag;
        ((tag & 3) == 2, tag >> 3)
    }
    #[cfg(not(Py_3_12))]
    {
        let size = (*h).ob_base.ob_size;
        (size < 0, size.unsigned_abs())
    }
}

/// Magnitude of an int with at most two digits (`ndigits` from `int_shape`).
#[inline(always)]
unsafe fn int_magnitude(obj: *mut ffi::PyObject, ndigits: usize) -> u64 {
    let d = ptr::addr_of!((*(obj as *const LongHeader)).ob_digit) as *const u32;
    match ndigits {
        0 => 0,
        1 => *d as u64,
        _ => (*d as u64) | ((*d.add(1) as u64) << 30),
    }
}

static DIGIT_PAIRS: [u8; 200] = {
    let mut t = [0u8; 200];
    let mut i = 0;
    while i < 100 {
        t[2 * i] = b'0' + (i / 10) as u8;
        t[2 * i + 1] = b'0' + (i % 10) as u8;
        i += 1;
    }
    t
};

/// Inline formatter for `v < 10_000` (the common case for ints in JSON);
/// larger values go to itoap. Returns the number of bytes written.
#[inline(always)]
unsafe fn write_u32_small(dst: *mut u8, v: u32) -> usize {
    if v >= 10_000 {
        // itoap's SIMD path is faster from 5 digits on.
        return itoap::write_to_ptr(dst, v);
    }
    let n = if v < 10 {
        1
    } else if v < 100 {
        2
    } else if v < 1000 {
        3
    } else {
        4
    };
    let mut v = v;
    if v >= 100 {
        let r = (v % 100) as usize;
        v /= 100;
        ptr::copy_nonoverlapping(DIGIT_PAIRS.as_ptr().add(2 * r), dst.add(n - 2), 2);
    }
    if v >= 10 {
        ptr::copy_nonoverlapping(DIGIT_PAIRS.as_ptr().add(2 * v as usize), dst, 2);
    } else {
        *dst = b'0' + v as u8;
    }
    n
}

/// Returns the value of `obj` if it has at most two 30-bit digits.
unsafe fn int_inline(obj: *mut ffi::PyObject) -> Option<i64> {
    let (neg, nd) = int_shape(obj);
    if nd > 2 {
        return None;
    }
    let mag = int_magnitude(obj, nd) as i64;
    Some(if neg { -mag } else { mag })
}

/// Called once at module init: enable inline int decoding only if the
/// interpreter uses 30-bit digits and a self-test passes.
pub fn init(py: Python<'_>) {
    let ok = (|| -> PyResult<bool> {
        let info = py.import("sys")?.getattr("int_info")?;
        let bits: u32 = info.getattr("bits_per_digit")?.extract()?;
        let size: u32 = info.getattr("sizeof_digit")?.extract()?;
        if bits != 30 || size != 4 {
            return Ok(false);
        }
        for v in [
            0i64,
            1,
            -1,
            12345,
            -(1 << 29),
            1 << 30,
            -(1 << 40),
            (1 << 60) - 1,
            -((1 << 60) - 1),
        ] {
            let o = v.into_pyobject(py)?;
            if unsafe { int_inline(o.as_ptr()) } != Some(v) {
                return Ok(false);
            }
        }
        Ok(true)
    })()
    .unwrap_or(false);
    unsafe { INLINE_INT = ok };
    detect_cpu();
}

const _: () =
    assert!(std::mem::size_of::<zmij::Buffer>() == 24 && std::mem::align_of::<zmij::Buffer>() == 1);

// ---------------------------------------------------------------------------
// String escaping kernel
// ---------------------------------------------------------------------------

/// Escape table for bytes < 0x60: up to 6 output bytes, byte 7 holds the length.
static ESCAPE_TAB: [[u8; 8]; 96] = build_escape_tab();

/// 1 if the byte must be escaped.
static NEEDS_ESCAPE: [u8; 256] = build_needs_escape();

const fn build_escape_tab() -> [[u8; 8]; 96] {
    let hex = b"0123456789abcdef";
    let mut t = [[0u8; 8]; 96];
    let mut i = 0;
    while i < 0x20 {
        t[i] = [b'\\', b'u', b'0', b'0', hex[i >> 4], hex[i & 15], 0, 6];
        i += 1;
    }
    t[0x08] = [b'\\', b'b', 0, 0, 0, 0, 0, 2];
    t[0x09] = [b'\\', b't', 0, 0, 0, 0, 0, 2];
    t[0x0a] = [b'\\', b'n', 0, 0, 0, 0, 0, 2];
    t[0x0c] = [b'\\', b'f', 0, 0, 0, 0, 0, 2];
    t[0x0d] = [b'\\', b'r', 0, 0, 0, 0, 0, 2];
    t[0x22] = [b'\\', b'"', 0, 0, 0, 0, 0, 2];
    t[0x5c] = [b'\\', b'\\', 0, 0, 0, 0, 0, 2];
    t
}

const fn build_needs_escape() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 0x20 {
        t[i] = 1;
        i += 1;
    }
    t[0x22] = 1;
    t[0x5c] = 1;
    t
}

/// Writes the escape sequence for `b` (which must need escaping). Writes 8
/// bytes at `*dst` (caller guarantees room) and advances by the real length.
#[inline(always)]
unsafe fn write_escape(b: u8, dst: &mut *mut u8) {
    let e = ESCAPE_TAB.get_unchecked(b as usize);
    ptr::copy_nonoverlapping(e.as_ptr(), *dst, 8);
    *dst = dst.add(e[7] as usize);
}

/// SWAR: does any byte of `v` need escaping (< 0x20, '"' or '\\')?
#[inline(always)]
fn swar_needs_escape(v: u64) -> bool {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGH: u64 = 0x8080_8080_8080_8080;
    let lt20 = v.wrapping_sub(ONES * 0x20) & !v;
    let q = v ^ (ONES * 0x22);
    let q = q.wrapping_sub(ONES) & !q;
    let b = v ^ (ONES * 0x5c);
    let b = b.wrapping_sub(ONES) & !b;
    (lt20 | q | b) & HIGH != 0
}

#[inline(always)]
unsafe fn escape_scalar(mut dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    for i in 0..len {
        let b = *src.add(i);
        if *NEEDS_ESCAPE.get_unchecked(b as usize) != 0 {
            write_escape(b, &mut dst);
        } else {
            *dst = b;
            dst = dst.add(1);
        }
    }
    dst
}

/// Escapes `len` bytes of UTF-8 at `src` into `dst` (no quotes).
/// `dst` must have room for `len * 6 + 32` bytes. Returns the new end.
#[inline(always)]
unsafe fn escape_body(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    #[cfg(target_arch = "x86_64")]
    if HAS_AVX512VL {
        return escape_avx512vl(dst, src, len);
    }
    if len < 16 {
        return escape_short(dst, src, len);
    }
    escape_long(dst, src, len)
}

/// AVX-512BW/VL available (checked once at init). Enables the masked-load
/// kernel, which handles every string length, including the short dict keys
/// that dominate typical documents, without a scalar tail.
#[cfg(target_arch = "x86_64")]
static mut HAS_AVX512VL: bool = false;

#[cfg(target_arch = "x86_64")]
fn detect_cpu() {
    // `--cfg rjson_no_avx512` forces the SSE2/AVX2 kernels (for testing them
    // on AVX-512 machines).
    unsafe {
        HAS_AVX512VL = !cfg!(rjson_no_avx512)
            && std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn detect_cpu() {}

/// 256-bit AVX-512VL kernel (no 512-bit registers, so no AVX-512 frequency
/// penalty). The final partial block uses a masked load, which cannot fault
/// past the end of the string, and a blind 32-byte store into the reserved
/// slack.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw,avx512vl")]
unsafe fn escape_avx512vl(mut dst: *mut u8, mut src: *const u8, len: usize) -> *mut u8 {
    use std::arch::x86_64::*;
    let quote = _mm256_set1_epi8(b'"' as i8);
    let bslash = _mm256_set1_epi8(b'\\' as i8);
    let x20 = _mm256_set1_epi8(0x20);
    let end = src.add(len);
    while end.offset_from(src) >= 32 {
        let v = _mm256_loadu_si256(src as *const __m256i);
        let m = _mm256_cmpeq_epi8_mask(v, quote)
            | _mm256_cmpeq_epi8_mask(v, bslash)
            | _mm256_cmplt_epu8_mask(v, x20);
        if m == 0 {
            _mm256_storeu_si256(dst as *mut __m256i, v);
            dst = dst.add(32);
        } else {
            dst = escape_block(dst, src, 32, m);
        }
        src = src.add(32);
    }
    let rem = end.offset_from(src) as usize;
    if rem != 0 {
        let k = (1u32 << rem) - 1;
        let v = _mm256_maskz_loadu_epi8(k, src as *const i8);
        let m = (_mm256_cmpeq_epi8_mask(v, quote)
            | _mm256_cmpeq_epi8_mask(v, bslash)
            | _mm256_cmplt_epu8_mask(v, x20))
            & k;
        if m == 0 {
            _mm256_storeu_si256(dst as *mut __m256i, v);
            dst = dst.add(rem);
        } else {
            dst = escape_block(dst, src, rem, m);
        }
    }
    dst
}

#[inline(always)]
unsafe fn escape_short(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    if len >= 8 {
        let a = ptr::read_unaligned(src as *const u64);
        let b = ptr::read_unaligned(src.add(len - 8) as *const u64);
        if !swar_needs_escape(a) && !swar_needs_escape(b) {
            ptr::write_unaligned(dst as *mut u64, a);
            ptr::write_unaligned(dst.add(len - 8) as *mut u64, b);
            return dst.add(len);
        }
    } else if len >= 4 {
        let a = ptr::read_unaligned(src as *const u32);
        let b = ptr::read_unaligned(src.add(len - 4) as *const u32);
        if !swar_needs_escape((a as u64) | ((b as u64) << 32)) {
            ptr::write_unaligned(dst as *mut u32, a);
            ptr::write_unaligned(dst.add(len - 4) as *mut u32, b);
            return dst.add(len);
        }
    }
    escape_scalar(dst, src, len)
}

/// Copies 1..=15 bytes with overlapping word moves (no memcpy call).
#[inline(always)]
unsafe fn copy_small(dst: *mut u8, src: *const u8, n: usize) {
    if n >= 8 {
        let a = ptr::read_unaligned(src as *const u64);
        let b = ptr::read_unaligned(src.add(n - 8) as *const u64);
        ptr::write_unaligned(dst as *mut u64, a);
        ptr::write_unaligned(dst.add(n - 8) as *mut u64, b);
    } else if n >= 4 {
        let a = ptr::read_unaligned(src as *const u32);
        let b = ptr::read_unaligned(src.add(n - 4) as *const u32);
        ptr::write_unaligned(dst as *mut u32, a);
        ptr::write_unaligned(dst.add(n - 4) as *mut u32, b);
    } else if n > 0 {
        *dst = *src;
        *dst.add(n / 2) = *src.add(n / 2);
        *dst.add(n - 1) = *src.add(n - 1);
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use std::arch::x86_64::*;

    /// Bitmask of bytes that need escaping (< 0x20, '"', '\\').
    #[inline(always)]
    pub unsafe fn mask16(v: __m128i) -> u32 {
        let quote = _mm_set1_epi8(b'"' as i8);
        let bslash = _mm_set1_epi8(b'\\' as i8);
        let x1f = _mm_set1_epi8(0x1f);
        _mm_movemask_epi8(_mm_or_si128(
            _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, bslash)),
            // v <= 0x1f  <=>  saturating(v - 0x1f) == 0
            _mm_cmpeq_epi8(_mm_subs_epu8(v, x1f), _mm_setzero_si128()),
        )) as u32
    }

    /// Only called from code compiled with AVX2 enabled.
    #[inline(always)]
    pub unsafe fn mask32(v: __m256i) -> u32 {
        let quote = _mm256_set1_epi8(b'"' as i8);
        let bslash = _mm256_set1_epi8(b'\\' as i8);
        let x1f = _mm256_set1_epi8(0x1f);
        _mm256_movemask_epi8(_mm256_or_si256(
            _mm256_or_si256(_mm256_cmpeq_epi8(v, quote), _mm256_cmpeq_epi8(v, bslash)),
            _mm256_cmpeq_epi8(_mm256_subs_epu8(v, x1f), _mm256_setzero_si256()),
        )) as u32
    }
}

/// `len >= 16`. Each vector block is stored unconditionally and the output
/// pointer only advances past bytes that did not need escaping, so the common
/// no-escape case is a plain load/compare/store loop.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn escape_long(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    #[cfg(target_feature = "avx2")]
    {
        escape_long_impl::<true>(dst, src, len)
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        // Runtime dispatch; the detection result is cached by std.
        if len >= 48 && std::arch::is_x86_feature_detected!("avx2") {
            escape_long_avx2(dst, src, len)
        } else {
            escape_long_impl::<false>(dst, src, len)
        }
    }
}

#[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
#[target_feature(enable = "avx2")]
unsafe fn escape_long_avx2(dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    escape_long_impl::<true>(dst, src, len)
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn escape_long_impl<const AVX2: bool>(
    mut dst: *mut u8,
    mut src: *const u8,
    len: usize,
) -> *mut u8 {
    use std::arch::x86_64::*;
    let end = src.add(len);

    if AVX2 {
        while end.offset_from(src) >= 32 {
            let v = _mm256_loadu_si256(src as *const __m256i);
            let m = x86::mask32(v);
            if m == 0 {
                _mm256_storeu_si256(dst as *mut __m256i, v);
                dst = dst.add(32);
            } else {
                dst = escape_block(dst, src, 32, m);
            }
            src = src.add(32);
        }
    }

    while end.offset_from(src) >= 16 {
        let v = _mm_loadu_si128(src as *const __m128i);
        let m = x86::mask16(v);
        if m == 0 {
            _mm_storeu_si128(dst as *mut __m128i, v);
            dst = dst.add(16);
        } else {
            dst = escape_block(dst, src, 16, m);
        }
        src = src.add(16);
    }
    // Tail (< 16 bytes): test the last 16 input bytes (in bounds since
    // len >= 16), keeping only the bits of the not-yet-written bytes.
    let rem = end.offset_from(src) as usize;
    if rem != 0 {
        let v = _mm_loadu_si128(end.sub(16) as *const __m128i);
        let m = x86::mask16(v) >> (16 - rem);
        if m == 0 {
            copy_small(dst, src, rem);
            dst = dst.add(rem);
        } else {
            dst = escape_block(dst, src, rem, m);
        }
    }
    dst
}

/// Number of bytes that need escaping.
unsafe fn count_escapes(src: *const u8, len: usize) -> usize {
    let mut n = 0usize;
    let mut i = 0usize;
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        while i + 16 <= len {
            let v = _mm_loadu_si128(src.add(i) as *const __m128i);
            n += x86::mask16(v).count_ones() as usize;
            i += 16;
        }
    }
    while i < len {
        n += *NEEDS_ESCAPE.get_unchecked(*src.add(i) as usize) as usize;
        i += 1;
    }
    n
}

/// Writes `n` (<= 32) bytes from `src`, escaping the bytes flagged in `m`.
#[inline(always)]
unsafe fn escape_block(mut dst: *mut u8, src: *const u8, n: usize, mut m: u32) -> *mut u8 {
    let mut i = 0;
    while m != 0 {
        let k = m.trailing_zeros() as usize;
        copy_upto32(dst, src.add(i), k - i);
        dst = dst.add(k - i);
        write_escape(*src.add(k), &mut dst);
        i = k + 1;
        m &= m - 1;
    }
    copy_upto32(dst, src.add(i), n - i);
    dst.add(n - i)
}

/// Copies 0..=32 bytes with overlapping moves.
#[inline(always)]
unsafe fn copy_upto32(dst: *mut u8, src: *const u8, n: usize) {
    if n >= 16 {
        let a = ptr::read_unaligned(src as *const u128);
        let b = ptr::read_unaligned(src.add(n - 16) as *const u128);
        ptr::write_unaligned(dst as *mut u128, a);
        ptr::write_unaligned(dst.add(n - 16) as *mut u128, b);
    } else {
        copy_small(dst, src, n);
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline(always)]
unsafe fn escape_long(mut dst: *mut u8, src: *const u8, len: usize) -> *mut u8 {
    let mut i = 0;
    while i + 8 <= len {
        let v = ptr::read_unaligned(src.add(i) as *const u64);
        if swar_needs_escape(v) {
            dst = escape_scalar(dst, src.add(i), 8);
        } else {
            ptr::write_unaligned(dst as *mut u64, v);
            dst = dst.add(8);
        }
        i += 8;
    }
    escape_scalar(dst, src.add(i), len - i)
}

// ---------------------------------------------------------------------------
// Code-unit helpers for the `str` output path
// ---------------------------------------------------------------------------

trait Unit: Copy {
    fn from_u32(v: u32) -> Self;
    fn to_u32(self) -> u32;
}
impl Unit for u8 {
    #[inline(always)]
    fn from_u32(v: u32) -> Self {
        v as u8
    }
    #[inline(always)]
    fn to_u32(self) -> u32 {
        self as u32
    }
}
impl Unit for u16 {
    #[inline(always)]
    fn from_u32(v: u32) -> Self {
        v as u16
    }
    #[inline(always)]
    fn to_u32(self) -> u32 {
        self as u32
    }
}
impl Unit for u32 {
    #[inline(always)]
    fn from_u32(v: u32) -> Self {
        v
    }
    #[inline(always)]
    fn to_u32(self) -> u32 {
        self
    }
}

/// Copies `n` code units, widening from `S` to `D` (`D` is never narrower).
#[inline(always)]
unsafe fn widen<S: Unit, D: Unit>(src: *const S, dst: *mut D, n: usize) {
    if std::mem::size_of::<S>() == std::mem::size_of::<D>() {
        ptr::copy_nonoverlapping(src as *const D, dst, n);
    } else {
        let s = std::slice::from_raw_parts(src, n);
        let d = std::slice::from_raw_parts_mut(dst, n);
        for (o, i) in d.iter_mut().zip(s) {
            *o = D::from_u32(i.to_u32());
        }
    }
}

/// Does any code unit of a UCS1/UCS2/UCS4 buffer need JSON escaping?
unsafe fn kind_needs_escape(kind: u32, data: *const u8, n: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let (bytes, mut i) = (n * kind as usize, 0usize);
        match kind {
            1 => {
                while i + 16 <= bytes {
                    if x86::mask16(_mm_loadu_si128(data.add(i) as *const __m128i)) != 0 {
                        return true;
                    }
                    i += 16;
                }
                units_need_escape(data.add(i), bytes - i)
            }
            2 => {
                let q = _mm_set1_epi16(0x22);
                let b = _mm_set1_epi16(0x5c);
                let x1f = _mm_set1_epi16(0x1f);
                while i + 16 <= bytes {
                    let v = _mm_loadu_si128(data.add(i) as *const __m128i);
                    let m = _mm_or_si128(
                        _mm_or_si128(_mm_cmpeq_epi16(v, q), _mm_cmpeq_epi16(v, b)),
                        _mm_cmpeq_epi16(_mm_subs_epu16(v, x1f), _mm_setzero_si128()),
                    );
                    if _mm_movemask_epi8(m) != 0 {
                        return true;
                    }
                    i += 16;
                }
                units_need_escape(data.add(i) as *const u16, (bytes - i) / 2)
            }
            _ => {
                // Code points are < 0x110000, so signed 32-bit compares work.
                let q = _mm_set1_epi32(0x22);
                let b = _mm_set1_epi32(0x5c);
                let x20 = _mm_set1_epi32(0x20);
                while i + 16 <= bytes {
                    let v = _mm_loadu_si128(data.add(i) as *const __m128i);
                    let m = _mm_or_si128(
                        _mm_or_si128(_mm_cmpeq_epi32(v, q), _mm_cmpeq_epi32(v, b)),
                        _mm_cmplt_epi32(v, x20),
                    );
                    if _mm_movemask_epi8(m) != 0 {
                        return true;
                    }
                    i += 16;
                }
                units_need_escape(data.add(i) as *const u32, (bytes - i) / 4)
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        match kind {
            1 => units_need_escape(data, n),
            2 => units_need_escape(data as *const u16, n),
            _ => units_need_escape(data as *const u32, n),
        }
    }
}

#[inline(always)]
unsafe fn units_need_escape<S: Unit>(src: *const S, n: usize) -> bool {
    let s = std::slice::from_raw_parts(src, n);
    for chunk in s.chunks(32) {
        let mut f = false;
        for &c in chunk {
            let c = c.to_u32();
            f |= (c < 0x20) | (c == 0x22) | (c == 0x5c);
        }
        if f {
            return true;
        }
    }
    false
}

/// Returns a new canonical `str` holding the JSON-escaped text of a
/// UCS1/UCS2/UCS4 buffer (without quotes), or NULL with an exception set.
unsafe fn escaped_copy(kind: u32, data: *const u8, n: usize) -> *mut ffi::PyObject {
    let unit = |i: usize| -> u32 {
        match kind {
            1 => *data.add(i) as u32,
            2 => *(data as *const u16).add(i) as u32,
            _ => *(data as *const u32).add(i),
        }
    };
    let mut out: Vec<u32> = Vec::with_capacity(n + 16);
    for i in 0..n {
        let c = unit(i);
        if c < 0x60 && NEEDS_ESCAPE[c as usize] != 0 {
            let e = &ESCAPE_TAB[c as usize];
            out.extend(e[..e[7] as usize].iter().map(|&b| b as u32));
        } else {
            out.push(c);
        }
    }
    ffi::PyUnicode_FromKindAndData(
        ffi::PyUnicode_4BYTE_KIND as std::os::raw::c_int,
        out.as_ptr() as *const std::os::raw::c_void,
        out.len() as ffi::Py_ssize_t,
    )
}

/// A non-ASCII string whose content is not in the byte buffer.
struct Segment {
    /// Offset in `buf` where the content belongs.
    pos: usize,
    /// Strong reference to the source `str`.
    obj: *mut ffi::PyObject,
    /// Number of code points of the content.
    nchars: usize,
}

// ---------------------------------------------------------------------------
// Output buffer
// ---------------------------------------------------------------------------

/// Output buffer that *is* the result object: a `bytes` object (bytes mode) or
/// a compact ASCII `str` (str mode), grown with realloc and shrunk to the final
/// length at the end, so the output is never copied (like orjson's BytesWriter).
/// The initial capacity comes from the previous output size on this thread.
struct Out {
    obj: *mut ffi::PyObject,
    data: *mut u8,
    len: usize,
    cap: usize,
    unicode: bool,
}

thread_local! {
    /// Size of the previous output on this thread (capacity hint).
    static LAST_LEN: Cell<usize> = const { Cell::new(0) };
}

const MIN_CAPACITY: usize = 128;
/// Shrinking a much larger buffer copies instead of reallocating in place, so
/// a small result never pins a large (possibly mmapped) block.
const SHRINK_COPY_THRESHOLD: usize = 64 * 1024;

impl Out {
    unsafe fn new(unicode: bool) -> Out {
        let hint = LAST_LEN.with(|c| c.get());
        let cap = (hint + hint / 8 + 16).max(MIN_CAPACITY);
        let obj = Self::alloc(cap, unicode);
        Out {
            obj,
            data: Self::data_of(obj, unicode),
            len: 0,
            cap,
            unicode,
        }
    }

    unsafe fn alloc(cap: usize, unicode: bool) -> *mut ffi::PyObject {
        let obj = if unicode {
            ffi::PyUnicode_New(cap as ffi::Py_ssize_t, 127)
        } else {
            ffi::PyBytes_FromStringAndSize(ptr::null(), cap as ffi::Py_ssize_t)
        };
        if obj.is_null() {
            Self::oom(cap);
        }
        obj
    }

    #[cold]
    #[inline(never)]
    fn oom(cap: usize) -> ! {
        // Same behaviour as a failed Vec allocation.
        std::alloc::handle_alloc_error(std::alloc::Layout::from_size_align(cap.max(1), 1).unwrap())
    }

    #[inline(always)]
    unsafe fn data_of(obj: *mut ffi::PyObject, unicode: bool) -> *mut u8 {
        if unicode {
            (obj as *mut ffi::PyASCIIObject).add(1) as *mut u8
        } else {
            ptr::addr_of_mut!((*(obj as *mut ffi::PyBytesObject)).ob_sval) as *mut u8
        }
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    fn capacity(&self) -> usize {
        self.cap
    }
    #[inline(always)]
    fn as_ptr(&self) -> *const u8 {
        self.data
    }
    #[inline(always)]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data
    }
    #[inline(always)]
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= self.cap);
        self.len = len;
    }

    /// Ensures room for `n` more bytes.
    #[inline(never)]
    fn reserve(&mut self, n: usize) {
        if self.cap - self.len >= n {
            return;
        }
        let mut cap = self.cap * 2;
        while cap - self.len < n {
            cap *= 2;
        }
        unsafe { self.resize(cap) };
    }

    unsafe fn resize(&mut self, cap: usize) {
        let rc = if self.unicode {
            ffi::PyUnicode_Resize(&mut self.obj, cap as ffi::Py_ssize_t)
        } else {
            ffi::_PyBytes_Resize(&mut self.obj, cap as ffi::Py_ssize_t)
        };
        if rc != 0 {
            Self::oom(cap);
        }
        self.data = Self::data_of(self.obj, self.unicode);
        self.cap = cap;
    }

    unsafe fn extend_from_slice(&mut self, s: &[u8]) {
        self.reserve(s.len());
        ptr::copy_nonoverlapping(s.as_ptr(), self.data.add(self.len), s.len());
        self.len += s.len();
    }

    /// Shrinks to the written length and hands the object over.
    unsafe fn into_object(&mut self) -> *mut ffi::PyObject {
        LAST_LEN.with(|c| c.set(self.len));
        if self.cap > SHRINK_COPY_THRESHOLD && self.len < self.cap / 4 {
            let small = Self::alloc(self.len, self.unicode);
            ptr::copy_nonoverlapping(self.data, Self::data_of(small, self.unicode), self.len);
            ffi::Py_DECREF(self.obj);
            self.obj = ptr::null_mut();
            return small;
        }
        let slack = self.cap - self.len;
        if slack > 4096 && slack > self.len / 8 {
            self.resize(self.len);
        } else if slack != 0 {
            // Small slack: just shorten the object in place. A bytes/str
            // object does not record its allocation size, so a shorter length
            // (plus the NUL terminator CPython expects) is fully valid and
            // avoids a realloc, which pymalloc turns into malloc+copy+free
            // when shrinking by more than 25%.
            if self.unicode {
                (*(self.obj as *mut ffi::PyASCIIObject)).length = self.len as ffi::Py_ssize_t;
            } else {
                (*(self.obj as *mut ffi::PyVarObject)).ob_size = self.len as ffi::Py_ssize_t;
            }
            *self.data.add(self.len) = 0;
        }
        std::mem::replace(&mut self.obj, ptr::null_mut())
    }
}

impl Drop for Out {
    fn drop(&mut self) {
        if !self.obj.is_null() {
            LAST_LEN.with(|c| c.set(self.len));
            unsafe { ffi::Py_DECREF(self.obj) };
        }
    }
}

// ---------------------------------------------------------------------------
// Serializer
// ---------------------------------------------------------------------------

pub struct Serializer {
    buf: Out,
    depth: u32,
    /// Building a `str` result (see module docs).
    str_mode: bool,
    segs: Vec<Segment>,
    max_kind: u32,
    seg_chars: usize,
    err_obj: *mut ffi::PyObject,
    err_float: f64,
}

impl Drop for Serializer {
    fn drop(&mut self) {
        for s in self.segs.drain(..) {
            unsafe { ffi::Py_DECREF(s.obj) };
        }
    }
}

impl Serializer {
    unsafe fn new(str_mode: bool) -> Self {
        Serializer {
            buf: Out::new(str_mode),
            depth: 0,
            str_mode,
            segs: Vec::new(),
            max_kind: 1,
            seg_chars: 0,
            err_obj: ptr::null_mut(),
            err_float: 0.0,
        }
    }

    #[inline(always)]
    fn reserve(&mut self, n: usize) {
        if self.buf.capacity() - self.buf.len() < n {
            self.grow(n);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, n: usize) {
        self.buf.reserve(n);
    }

    #[inline(always)]
    fn end_ptr(&mut self) -> *mut u8 {
        unsafe { self.buf.as_mut_ptr().add(self.buf.len()) }
    }

    #[inline(always)]
    unsafe fn set_end(&mut self, end: *mut u8) {
        let len = end.offset_from(self.buf.as_ptr()) as usize;
        debug_assert!(len <= self.buf.capacity());
        self.buf.set_len(len);
    }

    #[inline(always)]
    fn put(&mut self, b: u8) {
        self.reserve(1);
        unsafe {
            *self.end_ptr() = b;
            self.buf.set_len(self.buf.len() + 1);
        }
    }

    #[inline(always)]
    fn put2(&mut self, s: &[u8; 2]) {
        self.reserve(2);
        unsafe {
            ptr::copy_nonoverlapping(s.as_ptr(), self.end_ptr(), 2);
            self.buf.set_len(self.buf.len() + 2);
        }
    }

    /// Writes the pending separator (if `ns == 1`) then up to 8 bytes given
    /// as a pattern, with a single length update.
    ///
    /// Separators (',' between items, ':' after keys) are passed down to the
    /// next value writer instead of being written on their own: every update
    /// of the buffer length is a store->load round trip through memory, and
    /// fusing them cut the per-element cost of small scalars by ~1ns.
    #[inline(always)]
    fn put_word(&mut self, sep: u8, ns: usize, pattern: &[u8; 8], len: usize) {
        self.reserve(9);
        unsafe {
            let p = self.end_ptr();
            *p = sep;
            ptr::copy_nonoverlapping(pattern.as_ptr(), p.add(ns), 8);
            self.buf.set_len(self.buf.len() + ns + len);
        }
    }

    #[inline(always)]
    fn put_sep(&mut self, sep: u8, ns: usize) {
        if ns != 0 {
            self.put(sep);
        }
    }

    // ----- scalars -----

    #[inline(always)]
    unsafe fn write_i64(&mut self, v: i64) {
        self.reserve(24);
        let n = itoap::write_to_ptr(self.end_ptr(), v);
        self.buf.set_len(self.buf.len() + n);
    }

    #[inline(always)]
    unsafe fn write_int(&mut self, obj: *mut ffi::PyObject, sep: u8, ns: usize) -> SerResult {
        if INLINE_INT {
            let (neg, nd) = int_shape(obj);
            if nd <= 2 {
                // |value| < 2**60: sign + unsigned digits, using the cheaper
                // 32-bit formatter for single-digit (< 2**30) values.
                self.reserve(25);
                let p = self.end_ptr();
                *p = sep;
                let p = p.add(ns);
                *p = b'-';
                let p = p.add(neg as usize);
                let n = if nd <= 1 {
                    write_u32_small(p, int_magnitude(obj, nd) as u32)
                } else {
                    itoap::write_to_ptr(p, int_magnitude(obj, nd))
                };
                self.buf.set_len(self.buf.len() + ns + neg as usize + n);
                return Ok(());
            }
        }
        self.put_sep(sep, ns);
        self.write_int_slow(obj)
    }

    #[inline(never)]
    unsafe fn write_int_slow(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        let mut overflow: std::os::raw::c_int = 0;
        let v = ffi::PyLong_AsLongLongAndOverflow(obj, &mut overflow);
        if overflow == 0 {
            if v == -1 && !ffi::PyErr_Occurred().is_null() {
                return Err(SerError::PyErrSet);
            }
            self.write_i64(v);
            return Ok(());
        }
        if overflow > 0 {
            let u = ffi::PyLong_AsUnsignedLongLong(obj);
            if !(u == u64::MAX && !ffi::PyErr_Occurred().is_null()) {
                self.reserve(24);
                let n = itoap::write_to_ptr(self.end_ptr(), u);
                self.buf.set_len(self.buf.len() + n);
                return Ok(());
            }
            ffi::PyErr_Clear();
        }
        // Arbitrary precision: format with int's own decimal conversion (does
        // not call user __str__/__repr__ on int subclasses).
        let s = ffi::PyNumber_ToBase(obj, 10);
        if s.is_null() {
            return Err(SerError::PyErrSet);
        }
        let mut n: ffi::Py_ssize_t = 0;
        let p = ffi::PyUnicode_AsUTF8AndSize(s, &mut n);
        if p.is_null() {
            ffi::Py_DECREF(s);
            return Err(SerError::PyErrSet);
        }
        self.buf
            .extend_from_slice(std::slice::from_raw_parts(p as *const u8, n as usize));
        ffi::Py_DECREF(s);
        Ok(())
    }

    #[inline(always)]
    unsafe fn write_float(&mut self, v: f64, sep: u8, ns: usize) -> SerResult {
        if !v.is_finite() {
            self.err_float = v;
            return Err(SerError::NonFinite);
        }
        self.reserve(33);
        let p = self.end_ptr();
        *p = sep;
        // Format in place: zmij::Buffer is a plain 24-byte array (asserted
        // below) and 33 bytes are reserved. Formatting into a stack buffer and
        // copying out was ~40% slower (store-forwarding stall on the copy).
        let b = &mut *(p.add(ns) as *mut zmij::Buffer);
        let n = b.format_finite(v).len();
        self.buf.set_len(self.buf.len() + ns + n);
        Ok(())
    }

    // ----- strings -----

    /// Writes `"<escaped utf8>"`.
    #[inline(always)]
    unsafe fn write_utf8(&mut self, src: *const u8, len: usize, sep: u8, ns: usize) {
        // Fast path: room for the worst case (every byte -> \u00XX) plus the
        // kernel's blind vector stores.
        if len <= ESCAPE_CHUNK && self.buf.capacity() - self.buf.len() >= len * 6 + 35 {
            self.write_utf8_unchecked(src, len, sep, ns);
        } else {
            self.write_utf8_tight(src, len, sep, ns);
        }
    }

    #[inline(always)]
    unsafe fn write_utf8_unchecked(&mut self, src: *const u8, len: usize, sep: u8, ns: usize) {
        let mut dst = self.end_ptr();
        *dst = sep;
        dst = dst.add(ns);
        *dst = b'"';
        dst = escape_body(dst.add(1), src, len);
        *dst = b'"';
        self.set_end(dst.add(1));
    }

    /// Not enough room for the worst case: reserve only what this string
    /// needs (the output buffer is sized from the previous result, so
    /// reserving 6x would force needless reallocations).
    #[inline(never)]
    unsafe fn write_utf8_tight(&mut self, src: *const u8, len: usize, sep: u8, ns: usize) {
        if len > ESCAPE_CHUNK {
            self.put_sep(sep, ns);
            self.put(b'"');
            self.escape_chunked(src, len);
            self.put(b'"');
            return;
        }
        // Each escape adds at most 5 bytes.
        self.reserve(len + 5 * count_escapes(src, len) + 35);
        self.write_utf8_unchecked(src, len, sep, ns);
    }

    unsafe fn escape_chunked(&mut self, src: *const u8, len: usize) {
        let mut off = 0;
        while off < len {
            let n = (len - off).min(ESCAPE_CHUNK);
            self.reserve(n * 6 + 32);
            let dst = escape_body(self.end_ptr(), src.add(off), n);
            self.set_end(dst);
            off += n;
        }
    }

    #[inline(always)]
    unsafe fn write_str(&mut self, obj: *mut ffi::PyObject, sep: u8, ns: usize) -> SerResult {
        if ffi::PyUnicode_IS_COMPACT_ASCII(obj) != 0 {
            let len = (*(obj as *mut ffi::PyASCIIObject)).length as usize;
            let data = (obj as *mut ffi::PyASCIIObject).add(1) as *const u8;
            self.write_utf8(data, len, sep, ns);
            return Ok(());
        }
        if !self.str_mode && ffi::PyUnicode_IS_COMPACT(obj) != 0 {
            // bytes output, non-ASCII string whose UTF-8 form is cached.
            let c = obj as *mut ffi::PyCompactUnicodeObject;
            if !(*c).utf8.is_null() {
                self.write_utf8((*c).utf8 as *const u8, (*c).utf8_length as usize, sep, ns);
                return Ok(());
            }
        }
        self.put_sep(sep, ns);
        self.write_str_slow(obj)
    }

    /// Non-ASCII, non-compact (e.g. subclass) or legacy strings.
    #[inline(never)]
    unsafe fn write_str_slow(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        #[cfg(not(Py_3_12))]
        {
            #[allow(deprecated)]
            if ffi::PyUnicode_READY(obj) != 0 {
                return Err(SerError::PyErrSet);
            }
        }
        let len = ffi::PyUnicode_GET_LENGTH(obj) as usize;
        if ffi::PyUnicode_IS_ASCII(obj) != 0 {
            self.write_utf8(ffi::PyUnicode_DATA(obj) as *const u8, len, 0, 0);
            return Ok(());
        }
        if self.str_mode {
            return self.write_str_segment(obj, len);
        }
        let (p, n) = utf8_of(obj)?;
        self.write_utf8(p, n, 0, 0);
        Ok(())
    }

    /// `str` output: record the non-ASCII string instead of encoding it.
    ///
    /// Like `json.dumps(..., ensure_ascii=False)`, lone surrogates are copied
    /// through (a `str` can hold them); `dumps_bytes` rejects them because
    /// they cannot be encoded as UTF-8.
    unsafe fn write_str_segment(&mut self, obj: *mut ffi::PyObject, len: usize) -> SerResult {
        let kind = ffi::PyUnicode_KIND(obj);
        let data = ffi::PyUnicode_DATA(obj);
        // Strong reference to the string whose native data will be copied.
        let src = if kind_needs_escape(kind, data as *const u8, len) {
            // Rare: build the escaped text as a new (canonical) str.
            let s = escaped_copy(kind, data as *const u8, len);
            if s.is_null() {
                return Err(SerError::PyErrSet);
            }
            s
        } else {
            ffi::Py_INCREF(obj);
            obj
        };
        let nchars = ffi::PyUnicode_GET_LENGTH(src) as usize;
        self.put(b'"');
        self.segs.push(Segment {
            pos: self.buf.len(),
            obj: src,
            nchars,
        });
        self.put(b'"');
        self.max_kind = self.max_kind.max(ffi::PyUnicode_KIND(src));
        self.seg_chars += nchars;
        Ok(())
    }

    // ----- containers -----

    /// Containers nest at most `RECURSION_LIMIT` deep (an empty container
    /// at that depth is rejected too, matching orjson).
    #[inline(always)]
    fn check_depth(&self) -> SerResult {
        if self.depth >= RECURSION_LIMIT {
            return Err(SerError::Recursion);
        }
        Ok(())
    }

    #[inline(never)]
    unsafe fn ser_list(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        self.check_depth()?;
        let n = ffi::PyList_GET_SIZE(obj);
        if n == 0 {
            self.put2(b"[]");
            return Ok(());
        }
        self.depth += 1;
        self.put(b'[');
        let mut i = 0;
        // Re-read the size each iteration: guards against mutation from
        // finalizers.
        while i < ffi::PyList_GET_SIZE(obj) {
            self.ser(ffi::PyList_GET_ITEM(obj, i), b',', (i != 0) as usize)?;
            i += 1;
        }
        self.put(b']');
        self.depth -= 1;
        Ok(())
    }

    #[inline(never)]
    unsafe fn ser_tuple(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        self.check_depth()?;
        let n = ffi::PyTuple_GET_SIZE(obj);
        if n == 0 {
            self.put2(b"[]");
            return Ok(());
        }
        self.depth += 1;
        self.put(b'[');
        for i in 0..n {
            self.ser(ffi::PyTuple_GET_ITEM(obj, i), b',', (i != 0) as usize)?;
        }
        self.put(b']');
        self.depth -= 1;
        Ok(())
    }

    #[inline(never)]
    unsafe fn ser_dict(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        self.check_depth()?;
        if (*(obj as *mut ffi::PyDictObject)).ma_used == 0 {
            self.put2(b"{}");
            return Ok(());
        }
        self.depth += 1;
        self.put(b'{');
        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        let mut ns = 0;
        while ffi::PyDict_Next(obj, &mut pos, &mut key, &mut value) != 0 {
            if ffi::Py_TYPE(key) == str_type() {
                self.write_str(key, b',', ns)?;
            } else if ffi::PyUnicode_Check(key) != 0 {
                self.put_sep(b',', ns);
                self.write_str_slow(key)?;
            } else {
                return Err(SerError::KeyNotStr);
            }
            ns = 1;
            self.ser(value, b':', 1)?;
        }
        self.put(b'}');
        self.depth -= 1;
        Ok(())
    }

    // ----- dispatch -----

    /// Serializes `obj`, preceded by `sep` if `ns == 1`.
    #[inline(always)]
    unsafe fn ser(&mut self, obj: *mut ffi::PyObject, sep: u8, ns: usize) -> SerResult {
        let ty = ffi::Py_TYPE(obj);
        if ty == str_type() {
            self.write_str(obj, sep, ns)
        } else if ty == int_type() {
            self.write_int(obj, sep, ns)
        } else if ty == float_type() {
            self.write_float(ffi::PyFloat_AS_DOUBLE(obj), sep, ns)
        } else if ty == dict_type() {
            self.put_sep(sep, ns);
            self.ser_dict(obj)
        } else if ty == list_type() {
            self.put_sep(sep, ns);
            self.ser_list(obj)
        } else if ty == bool_type() {
            if obj == ffi::Py_True() {
                self.put_word(sep, ns, b"true\0\0\0\0", 4);
            } else {
                self.put_word(sep, ns, b"false\0\0\0", 5);
            }
            Ok(())
        } else if obj == ffi::Py_None() {
            self.put_word(sep, ns, b"null\0\0\0\0", 4);
            Ok(())
        } else {
            self.put_sep(sep, ns);
            if ty == tuple_type() {
                self.ser_tuple(obj)
            } else {
                self.ser_subclass(obj)
            }
        }
    }

    /// Subclasses of the supported builtins (str/int/float/dict/list/tuple,
    /// e.g. IntEnum, StrEnum, OrderedDict, defaultdict, namedtuple) are
    /// serialized like their base type, as the stdlib `json` module does.
    #[cold]
    #[inline(never)]
    unsafe fn ser_subclass(&mut self, obj: *mut ffi::PyObject) -> SerResult {
        let ty = ffi::Py_TYPE(obj);
        if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_UNICODE_SUBCLASS) != 0 {
            self.write_str_slow(obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_LONG_SUBCLASS) != 0 {
            self.write_int_slow(obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_DICT_SUBCLASS) != 0 {
            self.ser_dict(obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_LIST_SUBCLASS) != 0 {
            self.ser_list(obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_TUPLE_SUBCLASS) != 0 {
            self.ser_tuple(obj)
        } else if ffi::PyFloat_Check(obj) != 0 {
            self.write_float(ffi::PyFloat_AS_DOUBLE(obj), 0, 0)
        } else {
            self.err_obj = obj;
            Err(SerError::Unsupported)
        }
    }

    // ----- results -----

    unsafe fn finish_bytes(&mut self) -> *mut ffi::PyObject {
        self.buf.into_object()
    }

    unsafe fn finish_str(&mut self) -> *mut ffi::PyObject {
        let len = self.buf.len();
        if self.segs.is_empty() {
            // Pure ASCII: the buffer already is the result `str`.
            return self.buf.into_object();
        }
        // Non-ASCII: the buffer holds the ASCII parts; build the final string
        // with the exact kind and drop the buffer.
        LAST_LEN.with(|c| c.set(len));
        let total = len + self.seg_chars;
        let maxchar = match self.max_kind {
            1 => 0xff,
            2 => 0xffff,
            _ => 0x10ffff,
        };
        let s = ffi::PyUnicode_New(total as ffi::Py_ssize_t, maxchar);
        if s.is_null() {
            return s;
        }
        let data = ffi::PyUnicode_DATA(s);
        let written = match self.max_kind {
            1 => self.fill(data as *mut u8),
            2 => self.fill(data as *mut u16),
            _ => self.fill(data as *mut u32),
        };
        debug_assert_eq!(written, total);
        let _ = written;
        s
    }

    unsafe fn fill<D: Unit>(&self, out: *mut D) -> usize {
        let buf = self.buf.as_ptr();
        let mut o = 0usize;
        let mut prev = 0usize;
        for seg in &self.segs {
            widen(buf.add(prev), out.add(o), seg.pos - prev);
            o += seg.pos - prev;
            let d = ffi::PyUnicode_DATA(seg.obj);
            match ffi::PyUnicode_KIND(seg.obj) {
                ffi::PyUnicode_1BYTE_KIND => widen(d as *const u8, out.add(o), seg.nchars),
                ffi::PyUnicode_2BYTE_KIND => widen(d as *const u16, out.add(o), seg.nchars),
                _ => widen(d as *const u32, out.add(o), seg.nchars),
            }
            o += seg.nchars;
            prev = seg.pos;
        }
        widen(buf.add(prev), out.add(o), self.buf.len() - prev);
        o + self.buf.len() - prev
    }
}

/// UTF-8 view of a (non-ASCII) string, using CPython's cached copy when present.
#[inline(always)]
unsafe fn utf8_of(obj: *mut ffi::PyObject) -> Result<(*const u8, usize), SerError> {
    if ffi::PyUnicode_IS_COMPACT(obj) != 0 {
        let c = obj as *mut ffi::PyCompactUnicodeObject;
        if !(*c).utf8.is_null() {
            return Ok(((*c).utf8 as *const u8, (*c).utf8_length as usize));
        }
    }
    let mut n: ffi::Py_ssize_t = 0;
    let p = ffi::PyUnicode_AsUTF8AndSize(obj, &mut n);
    if p.is_null() {
        return Err(SerError::PyErrSet);
    }
    Ok((p as *const u8, n as usize))
}

#[cold]
#[inline(never)]
fn to_pyerr(py: Python<'_>, ser: &Serializer, e: SerError) -> PyErr {
    use pyo3::exceptions::PyValueError;
    match e {
        SerError::NonFinite => PyValueError::new_err(format!(
            "Cannot serialize non-finite float: {}",
            ser.err_float
        )),
        SerError::Unsupported => {
            let name = unsafe { Bound::from_borrowed_ptr(py, ser.err_obj) }
                .get_type()
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            PyValueError::new_err(format!(
                "Unsupported Python type for JSON serialization: {}",
                name
            ))
        }
        SerError::KeyNotStr => {
            PyValueError::new_err("Dictionary keys must be strings for JSON serialization")
        }
        SerError::Recursion => PyValueError::new_err(format!(
            "Maximum nesting depth ({}) exceeded during JSON serialization (circular reference?)",
            RECURSION_LIMIT
        )),
        SerError::PyErrSet => PyErr::fetch(py),
    }
}

/// Serializes `obj` (borrowed) and returns a new reference to a `str`
/// (`as_str`) or `bytes`. Entry point for raw METH_O wrappers.
///
/// # Safety
/// `obj` must be a valid object pointer and the GIL must be held.
pub unsafe fn dumps_raw(
    py: Python<'_>,
    obj: *mut ffi::PyObject,
    as_str: bool,
) -> PyResult<*mut ffi::PyObject> {
    let mut ser = Serializer::new(as_str);
    if let Err(e) = ser.ser(obj, 0, 0) {
        return Err(to_pyerr(py, &ser, e));
    }
    let out = if as_str {
        ser.finish_str()
    } else {
        ser.finish_bytes()
    };
    if out.is_null() {
        return Err(PyErr::fetch(py));
    }
    Ok(out)
}
