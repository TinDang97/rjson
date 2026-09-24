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

/// Error kind; payloads live in `Serializer::err_*`.
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

/// `_mm_shuffle_epi8` controls for `write_short_ascii`: 16 bytes starting at
/// `16 - len` move the last `len` bytes of a vector to the front and zero
/// the rest (0x80 lanes).
static SHIFT_TAB: [u8; 32] = {
    let mut t = [0x80u8; 32];
    let mut i = 0;
    while i < 16 {
        t[i] = i as u8;
        i += 1;
    }
    t
};

const _: () = assert!(std::mem::size_of::<ffi::PyASCIIObject>() >= 16);

/// Writes `sep` (if `ns == 1`) and `"<escaped>"` for a string of `len <= 16`
/// bytes, without a call or a scalar tail: the 16 bytes *ending* at the end
/// of the string are loaded (so the load never reads past it) and shifted
/// down with one shuffle.
///
/// # Safety
/// The 16 bytes before `data + len` must be readable: true for the inline
/// data of a compact ASCII `str`, which follows its (>= 16-byte) header in
/// the same allocation. `16 * 6 + 35` bytes of room at `p`.
#[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
#[inline(always)]
unsafe fn write_short_ascii(
    p: *mut u8,
    data: *const u8,
    len: usize,
    sep: u8,
    ns: usize,
) -> *mut u8 {
    use std::arch::x86_64::*;
    debug_assert!(len <= 16);
    let v = _mm_loadu_si128(data.add(len).sub(16) as *const __m128i);
    let ctl = _mm_loadu_si128(SHIFT_TAB.as_ptr().add(16 - len) as *const __m128i);
    let v = _mm_shuffle_epi8(v, ctl);
    // Zeroed lanes past the end look like control characters: mask them.
    let m = x86::mask16(v) & ((1u32 << len) - 1);
    *p = sep;
    let d = p.add(ns);
    *d = b'"';
    let d = d.add(1);
    let d = if m == 0 {
        _mm_storeu_si128(d as *mut __m128i, v);
        d.add(len)
    } else {
        escape_block(d, data, len, m)
    };
    *d = b'"';
    d.add(1)
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

/// Write cursor: the current end of the output. Writers take the cursor and
/// return the advanced one, so it lives in a register instead of being stored
/// to and reloaded from `Out::len` for every value (that store->load round
/// trip was on the critical path of every element). `Out::len` is only
/// synced at growth, segment and finish points.
type Cur = *mut u8;
/// A cursor, or null on error (the error kind is in `Serializer::err`).
/// A `Result<Cur, SerError>` does not fit in one register, and LLVM spilled
/// it to the stack where paths merge, putting a store->load back on the
/// per-value critical path.
type CurResult = Cur;

/// `?` for `CurResult`.
macro_rules! tri {
    ($e:expr) => {{
        let p = $e;
        if p.is_null() {
            return p;
        }
        p
    }};
}

pub struct Serializer {
    buf: Out,
    depth: u32,
    /// Building a `str` result (see module docs).
    str_mode: bool,
    segs: Vec<Segment>,
    max_kind: u32,
    seg_chars: usize,
    err: SerError,
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
            err: SerError::PyErrSet,
            err_obj: ptr::null_mut(),
            err_float: 0.0,
        }
    }

    #[cold]
    #[inline(never)]
    fn fail(&mut self, e: SerError) -> Cur {
        self.err = e;
        ptr::null_mut()
    }

    #[inline(always)]
    fn start(&mut self) -> Cur {
        unsafe { self.buf.as_mut_ptr().add(self.buf.len()) }
    }

    /// Bytes available after `p`.
    #[inline(always)]
    fn room(&self, p: Cur) -> usize {
        self.buf.as_ptr() as usize + self.buf.capacity() - p as usize
    }

    /// Last cursor position with `n` bytes of room (`n <= MIN_CAPACITY`).
    #[inline(always)]
    fn limit(&self, n: usize) -> Cur {
        unsafe { self.buf.data.add(self.buf.cap - n) }
    }

    /// Offset of `p` in the buffer.
    #[inline(always)]
    fn offset(&self, p: Cur) -> usize {
        p as usize - self.buf.as_ptr() as usize
    }

    /// Records `p` as the end of the written output.
    #[inline(always)]
    unsafe fn sync(&mut self, p: Cur) {
        let len = self.offset(p);
        self.buf.set_len(len);
    }

    /// Ensures `n` writable bytes at the returned cursor (which moves if the
    /// buffer is reallocated).
    #[inline(always)]
    unsafe fn reserve(&mut self, p: Cur, n: usize) -> Cur {
        if self.room(p) < n {
            return self.grow(p, n);
        }
        p
    }

    #[cold]
    #[inline(never)]
    unsafe fn grow(&mut self, p: Cur, n: usize) -> Cur {
        self.sync(p);
        self.buf.reserve(n);
        self.start()
    }

    #[inline(always)]
    unsafe fn put(&mut self, p: Cur, b: u8) -> Cur {
        let p = self.reserve(p, 1);
        *p = b;
        p.add(1)
    }

    #[inline(always)]
    unsafe fn put2(&mut self, p: Cur, s: &[u8; 2]) -> Cur {
        let p = self.reserve(p, 2);
        ptr::copy_nonoverlapping(s.as_ptr(), p, 2);
        p.add(2)
    }

    /// Writes the pending separator (if `ns == 1`) then up to 8 bytes given
    /// as a pattern.
    ///
    /// Separators (',' between items, ':' after keys) are passed down to the
    /// next value writer instead of being written on their own, so small
    /// scalars cost one capacity check.
    #[inline(always)]
    unsafe fn put_word(
        &mut self,
        p: Cur,
        sep: u8,
        ns: usize,
        pattern: &[u8; 8],
        len: usize,
    ) -> Cur {
        let p = self.reserve(p, 9);
        *p = sep;
        ptr::copy_nonoverlapping(pattern.as_ptr(), p.add(ns), 8);
        p.add(ns + len)
    }

    #[inline(always)]
    unsafe fn put_sep(&mut self, p: Cur, sep: u8, ns: usize) -> Cur {
        let p = self.reserve(p, 1);
        *p = sep;
        p.add(ns)
    }

    // ----- scalars -----

    #[inline(always)]
    unsafe fn write_int(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        sep: u8,
        ns: usize,
    ) -> CurResult {
        if INLINE_INT {
            let (neg, nd) = int_shape(obj);
            if nd <= 2 {
                // |value| < 2**60: sign + unsigned digits, using the cheaper
                // 32-bit formatter for single-digit (< 2**30) values.
                let p = self.reserve(p, 25);
                *p = sep;
                let p = p.add(ns);
                *p = b'-';
                let p = p.add(neg as usize);
                let n = if nd <= 1 {
                    write_u32_small(p, int_magnitude(obj, nd) as u32)
                } else {
                    itoap::write_to_ptr(p, int_magnitude(obj, nd))
                };
                return p.add(n);
            }
        }
        let p = self.put_sep(p, sep, ns);
        self.write_int_slow(p, obj)
    }

    #[inline(never)]
    unsafe fn write_int_slow(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        let mut overflow: std::os::raw::c_int = 0;
        let v = ffi::PyLong_AsLongLongAndOverflow(obj, &mut overflow);
        if overflow == 0 {
            if v == -1 && !ffi::PyErr_Occurred().is_null() {
                return self.fail(SerError::PyErrSet);
            }
            let p = self.reserve(p, 24);
            return p.add(itoap::write_to_ptr(p, v));
        }
        if overflow > 0 {
            let u = ffi::PyLong_AsUnsignedLongLong(obj);
            if u != u64::MAX || ffi::PyErr_Occurred().is_null() {
                let p = self.reserve(p, 24);
                return p.add(itoap::write_to_ptr(p, u));
            }
            ffi::PyErr_Clear();
        }
        // Arbitrary precision: format with int's own decimal conversion (does
        // not call user __str__/__repr__ on int subclasses).
        let s = ffi::PyNumber_ToBase(obj, 10);
        if s.is_null() {
            return self.fail(SerError::PyErrSet);
        }
        let mut n: ffi::Py_ssize_t = 0;
        let src = ffi::PyUnicode_AsUTF8AndSize(s, &mut n);
        if src.is_null() {
            ffi::Py_DECREF(s);
            return self.fail(SerError::PyErrSet);
        }
        let n = n as usize;
        let p = self.reserve(p, n);
        ptr::copy_nonoverlapping(src as *const u8, p, n);
        ffi::Py_DECREF(s);
        p.add(n)
    }

    #[inline(always)]
    unsafe fn write_float(&mut self, p: Cur, v: f64, sep: u8, ns: usize) -> CurResult {
        if !v.is_finite() {
            self.err_float = v;
            return self.fail(SerError::NonFinite);
        }
        let p = self.reserve(p, 33);
        *p = sep;
        // Format in place: zmij::Buffer is a plain 24-byte array (asserted
        // above) and 33 bytes are reserved. Formatting into a stack buffer and
        // copying out was ~40% slower (store-forwarding stall on the copy).
        let b = &mut *(p.add(ns) as *mut zmij::Buffer);
        let n = b.format_finite(v).len();
        p.add(ns + n)
    }

    // ----- strings -----

    /// Writes `"<escaped utf8>"`.
    #[inline(always)]
    unsafe fn write_utf8(&mut self, p: Cur, src: *const u8, len: usize, sep: u8, ns: usize) -> Cur {
        // Fast path: room for the worst case (every byte -> \u00XX) plus the
        // kernel's blind vector stores.
        if len <= ESCAPE_CHUNK && self.room(p) >= len * 6 + 35 {
            write_utf8_unchecked(p, src, len, sep, ns)
        } else {
            self.write_utf8_tight(p, src, len, sep, ns)
        }
    }

    /// Not enough room for the worst case: reserve only what this string
    /// needs (the output buffer is sized from the previous result, so
    /// reserving 6x would force needless reallocations).
    #[inline(never)]
    unsafe fn write_utf8_tight(
        &mut self,
        p: Cur,
        src: *const u8,
        len: usize,
        sep: u8,
        ns: usize,
    ) -> Cur {
        if len > ESCAPE_CHUNK {
            let p = self.put_sep(p, sep, ns);
            let p = self.put(p, b'"');
            let p = self.escape_chunked(p, src, len);
            return self.put(p, b'"');
        }
        // Each escape adds at most 5 bytes.
        let p = self.reserve(p, len + 5 * count_escapes(src, len) + 35);
        write_utf8_unchecked(p, src, len, sep, ns)
    }

    unsafe fn escape_chunked(&mut self, mut p: Cur, src: *const u8, len: usize) -> Cur {
        let mut off = 0;
        while off < len {
            let n = (len - off).min(ESCAPE_CHUNK);
            p = self.reserve(p, n * 6 + 32);
            p = escape_body(p, src.add(off), n);
            off += n;
        }
        p
    }

    #[inline(always)]
    unsafe fn write_str(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        sep: u8,
        ns: usize,
    ) -> CurResult {
        if ffi::PyUnicode_IS_COMPACT_ASCII(obj) != 0 {
            let len = (*(obj as *mut ffi::PyASCIIObject)).length as usize;
            let data = (obj as *mut ffi::PyASCIIObject).add(1) as *const u8;
            #[cfg(all(target_arch = "x86_64", target_feature = "ssse3"))]
            if len <= 16 && self.room(p) >= 16 * 6 + 35 {
                // SAFETY: `data` is the inline data of a compact ASCII str,
                // preceded by its PyASCIIObject header (>= 16 bytes).
                return write_short_ascii(p, data, len, sep, ns);
            }
            return self.write_utf8(p, data, len, sep, ns);
        }
        if !self.str_mode && ffi::PyUnicode_IS_COMPACT(obj) != 0 {
            // bytes output, non-ASCII string whose UTF-8 form is cached.
            let c = obj as *mut ffi::PyCompactUnicodeObject;
            if !(*c).utf8.is_null() {
                let (src, len) = ((*c).utf8 as *const u8, (*c).utf8_length as usize);
                return self.write_utf8(p, src, len, sep, ns);
            }
        }
        let p = self.put_sep(p, sep, ns);
        self.write_str_slow(p, obj)
    }

    /// Non-ASCII, non-compact (e.g. subclass) or legacy strings.
    #[inline(never)]
    unsafe fn write_str_slow(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        #[cfg(not(Py_3_12))]
        {
            #[allow(deprecated)]
            if ffi::PyUnicode_READY(obj) != 0 {
                return self.fail(SerError::PyErrSet);
            }
        }
        let len = ffi::PyUnicode_GET_LENGTH(obj) as usize;
        if ffi::PyUnicode_IS_ASCII(obj) != 0 {
            return self.write_utf8(p, ffi::PyUnicode_DATA(obj) as *const u8, len, 0, 0);
        }
        if self.str_mode {
            return self.write_str_segment(p, obj, len);
        }
        let (src, n) = match utf8_of(obj) {
            Ok(v) => v,
            Err(e) => return self.fail(e),
        };
        self.write_utf8(p, src, n, 0, 0)
    }

    /// `str` output: record the non-ASCII string instead of encoding it.
    ///
    /// Like `json.dumps(..., ensure_ascii=False)`, lone surrogates are copied
    /// through (a `str` can hold them); `dumps_bytes` rejects them because
    /// they cannot be encoded as UTF-8.
    unsafe fn write_str_segment(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        len: usize,
    ) -> CurResult {
        let kind = ffi::PyUnicode_KIND(obj);
        let data = ffi::PyUnicode_DATA(obj);
        // Strong reference to the string whose native data will be copied.
        let src = if kind_needs_escape(kind, data as *const u8, len) {
            // Rare: build the escaped text as a new (canonical) str.
            let s = escaped_copy(kind, data as *const u8, len);
            if s.is_null() {
                return self.fail(SerError::PyErrSet);
            }
            s
        } else {
            ffi::Py_INCREF(obj);
            obj
        };
        let nchars = ffi::PyUnicode_GET_LENGTH(src) as usize;
        let p = self.put(p, b'"');
        self.segs.push(Segment {
            pos: self.offset(p),
            obj: src,
            nchars,
        });
        self.max_kind = self.max_kind.max(ffi::PyUnicode_KIND(src));
        self.seg_chars += nchars;
        self.put(p, b'"')
    }

    // ----- containers -----

    #[inline(never)]
    unsafe fn ser_list(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        let n = ffi::PyList_GET_SIZE(obj);
        if n == 0 {
            return self.put2(p, b"[]");
        }
        self.depth += 1;
        let mut p = self.put(p, b'[');
        let mut i = 0;
        // Re-read the size after every generic item: guards against mutation
        // from finalizers.
        while i < ffi::PyList_GET_SIZE(obj) {
            let item = ffi::PyList_GET_ITEM(obj, i);
            let ty = ffi::Py_TYPE(item);
            if ty == int_type() || ty == float_type() {
                let (q, j) = self.list_scalars(p, obj, i);
                p = q;
                if j != i {
                    i = j;
                    continue;
                }
            }
            p = tri!(self.ser(p, item, b',', (i != 0) as usize));
            i += 1;
        }
        self.depth -= 1;
        self.put(p, b']')
    }

    /// Writes the run of list items starting at `i` that are exact small
    /// ints or exact finite floats, with the list size, item array and
    /// capacity limit kept in registers. Returns the cursor and the index of
    /// the first item it did not write (the caller serializes that one
    /// generically, which also produces any error). Every item's exact type
    /// is checked; nothing here can run Python code, so the list cannot
    /// change under us.
    #[inline(always)]
    unsafe fn list_scalars(
        &mut self,
        mut p: Cur,
        obj: *mut ffi::PyObject,
        mut i: ffi::Py_ssize_t,
    ) -> (Cur, ffi::Py_ssize_t) {
        /// Largest scalar write: separator + '-' + 19 digits, or separator +
        /// the 24-byte zmij buffer.
        const MAX: usize = 40;
        let n = ffi::PyList_GET_SIZE(obj);
        let items = (*(obj as *mut ffi::PyListObject)).ob_item;
        let inline_int = INLINE_INT;
        // cap >= MIN_CAPACITY > MAX, so `limit` stays inside the allocation.
        let mut limit = self.limit(MAX);
        while i < n {
            let item = *items.offset(i);
            let ty = ffi::Py_TYPE(item);
            if p > limit {
                p = self.grow(p, MAX);
                limit = self.limit(MAX);
            }
            // The separator is only committed (by advancing past it) once
            // the item is written; otherwise the generic path rewrites it.
            *p = b',';
            let q = p.add((i != 0) as usize);
            if ty == int_type() && inline_int {
                let (neg, nd) = int_shape(item);
                if nd > 2 {
                    break;
                }
                *q = b'-';
                let q = q.add(neg as usize);
                let len = if nd <= 1 {
                    write_u32_small(q, int_magnitude(item, nd) as u32)
                } else {
                    itoap::write_to_ptr(q, int_magnitude(item, nd))
                };
                p = q.add(len);
            } else if ty == float_type() {
                let v = ffi::PyFloat_AS_DOUBLE(item);
                if !v.is_finite() {
                    break;
                }
                // SAFETY: MAX bytes are reserved at `p`; zmij::Buffer is a
                // plain 24-byte array (asserted at the top of the file).
                let b = &mut *(q as *mut zmij::Buffer);
                p = q.add(b.format_finite(v).len());
            } else {
                break;
            }
            i += 1;
        }
        (p, i)
    }

    #[inline(never)]
    unsafe fn ser_tuple(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        let n = ffi::PyTuple_GET_SIZE(obj);
        if n == 0 {
            return self.put2(p, b"[]");
        }
        self.depth += 1;
        let mut p = self.put(p, b'[');
        for i in 0..n {
            p = tri!(self.ser(p, ffi::PyTuple_GET_ITEM(obj, i), b',', (i != 0) as usize));
        }
        self.depth -= 1;
        self.put(p, b']')
    }

    #[inline(never)]
    unsafe fn ser_dict(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        if (*(obj as *mut ffi::PyDictObject)).ma_used == 0 {
            return self.put2(p, b"{}");
        }
        self.depth += 1;
        let mut p = self.put(p, b'{');
        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        let mut ns = 0;
        while ffi::PyDict_Next(obj, &mut pos, &mut key, &mut value) != 0 {
            if ffi::Py_TYPE(key) == str_type() {
                p = tri!(self.write_str(p, key, b',', ns));
            } else if ffi::PyUnicode_Check(key) != 0 {
                p = self.put_sep(p, b',', ns);
                p = tri!(self.write_str_slow(p, key));
            } else {
                return self.fail(SerError::KeyNotStr);
            }
            ns = 1;
            p = tri!(self.ser(p, value, b':', 1));
        }
        self.depth -= 1;
        self.put(p, b'}')
    }

    // ----- dispatch -----

    /// Serializes `obj`, preceded by `sep` if `ns == 1`.
    #[inline(always)]
    unsafe fn ser(&mut self, p: Cur, obj: *mut ffi::PyObject, sep: u8, ns: usize) -> CurResult {
        let ty = ffi::Py_TYPE(obj);
        if ty == str_type() {
            self.write_str(p, obj, sep, ns)
        } else if ty == int_type() {
            self.write_int(p, obj, sep, ns)
        } else if ty == float_type() {
            self.write_float(p, ffi::PyFloat_AS_DOUBLE(obj), sep, ns)
        } else if ty == dict_type() {
            let p = self.put_sep(p, sep, ns);
            self.ser_dict(p, obj)
        } else if ty == list_type() {
            let p = self.put_sep(p, sep, ns);
            self.ser_list(p, obj)
        } else if ty == bool_type() {
            if obj == ffi::Py_True() {
                self.put_word(p, sep, ns, b"true\0\0\0\0", 4)
            } else {
                self.put_word(p, sep, ns, b"false\0\0\0", 5)
            }
        } else if obj == ffi::Py_None() {
            self.put_word(p, sep, ns, b"null\0\0\0\0", 4)
        } else {
            let p = self.put_sep(p, sep, ns);
            if ty == tuple_type() {
                self.ser_tuple(p, obj)
            } else {
                self.ser_subclass(p, obj)
            }
        }
    }

    /// Subclasses of the supported builtins (str/int/float/dict/list/tuple,
    /// e.g. IntEnum, StrEnum, OrderedDict, defaultdict, namedtuple) are
    /// serialized like their base type, as the stdlib `json` module does.
    #[cold]
    #[inline(never)]
    unsafe fn ser_subclass(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        let ty = ffi::Py_TYPE(obj);
        if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_UNICODE_SUBCLASS) != 0 {
            self.write_str_slow(p, obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_LONG_SUBCLASS) != 0 {
            self.write_int_slow(p, obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_DICT_SUBCLASS) != 0 {
            self.ser_dict(p, obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_LIST_SUBCLASS) != 0 {
            self.ser_list(p, obj)
        } else if ffi::PyType_HasFeature(ty, ffi::Py_TPFLAGS_TUPLE_SUBCLASS) != 0 {
            self.ser_tuple(p, obj)
        } else if ffi::PyFloat_Check(obj) != 0 {
            self.write_float(p, ffi::PyFloat_AS_DOUBLE(obj), 0, 0)
        } else {
            self.err_obj = obj;
            self.fail(SerError::Unsupported)
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

/// Writes `sep` (if `ns == 1`) and `"<escaped>"`; the caller guarantees
/// `len * 6 + 35` bytes of room at `p`.
#[inline(always)]
unsafe fn write_utf8_unchecked(p: Cur, src: *const u8, len: usize, sep: u8, ns: usize) -> Cur {
    let mut dst = p;
    *dst = sep;
    dst = dst.add(ns);
    *dst = b'"';
    dst = escape_body(dst.add(1), src, len);
    *dst = b'"';
    dst.add(1)
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
    let start = ser.start();
    let end = ser.ser(start, obj, 0, 0);
    if end.is_null() {
        return Err(to_pyerr(py, &ser, ser.err));
    }
    ser.sync(end);
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
