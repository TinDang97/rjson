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
    #[cfg(rjson_dict_direct)]
    unsafe {
        dictiter::ENABLED = dictiter::self_test();
    }
    detect_cpu();
}

/// Direct iteration over the entries of combined-table dicts, replacing a
/// `PyDict_Next` call per item (about a quarter of the instructions of a
/// small dict item).
///
/// `PyDictKeysObject` is private, so this is gated to the CPython versions
/// whose layout it was written against (3.11-3.13, GIL builds; see
/// `build.rs`) and enabled only if an init-time self-test over dicts with
/// str keys, other keys and deleted entries matches `PyDict_Next`. Split
/// tables (`ma_values != NULL`, e.g. instance dicts) use `PyDict_Next`.
#[cfg(rjson_dict_direct)]
mod dictiter {
    use pyo3::ffi;

    /// `struct _dictkeysobject` up to `dk_indices` (CPython 3.11-3.13).
    #[repr(C)]
    struct DictKeys {
        dk_refcnt: ffi::Py_ssize_t,
        dk_log2_size: u8,
        dk_log2_index_bytes: u8,
        dk_kind: u8,
        dk_version: u32,
        dk_usable: ffi::Py_ssize_t,
        dk_nentries: ffi::Py_ssize_t,
    }

    /// `DICT_KEYS_GENERAL`: entries are `{hash, key, value}`; otherwise
    /// (str keys) `{key, value}`.
    const DICT_KEYS_GENERAL: u8 = 0;

    pub static mut ENABLED: bool = false;

    /// (pointer to the first entry's key slot, number of entries, entry
    /// size in pointers) of a combined-table dict; the value slot follows
    /// the key slot. Entries whose value is NULL are deleted.
    #[inline(always)]
    pub unsafe fn entries(
        d: *mut ffi::PyObject,
    ) -> Option<(*const *mut ffi::PyObject, usize, usize)> {
        if !ENABLED {
            return None;
        }
        raw_entries(d)
    }

    #[inline(always)]
    unsafe fn raw_entries(
        d: *mut ffi::PyObject,
    ) -> Option<(*const *mut ffi::PyObject, usize, usize)> {
        let d = d as *mut ffi::PyDictObject;
        if !(*d).ma_values.is_null() {
            return None;
        }
        let k = (*d).ma_keys as *const DictKeys;
        let indices = (k as *const u8).add(std::mem::size_of::<DictKeys>());
        let entries = indices.add(1usize << (*k).dk_log2_index_bytes) as *const *mut ffi::PyObject;
        let (stride, key_slot) = if (*k).dk_kind == DICT_KEYS_GENERAL {
            (3, 1)
        } else {
            (2, 0)
        };
        Some((entries.add(key_slot), (*k).dk_nentries as usize, stride))
    }

    /// Does direct iteration give exactly `PyDict_Next`'s items for `d`?
    unsafe fn same_as_next(d: *mut ffi::PyObject) -> bool {
        let Some((base, n, stride)) = raw_entries(d) else {
            return false;
        };
        let (mut pos, mut i) = (0, 0);
        let (mut k, mut v) = (std::ptr::null_mut(), std::ptr::null_mut());
        while ffi::PyDict_Next(d, &mut pos, &mut k, &mut v) != 0 {
            while i < n && (*base.add(i * stride + 1)).is_null() {
                i += 1;
            }
            if i == n || *base.add(i * stride) != k || *base.add(i * stride + 1) != v {
                return false;
            }
            i += 1;
        }
        (i..n).all(|j| (*base.add(j * stride + 1)).is_null())
    }

    /// Builds dicts with str keys, non-str keys, deleted entries and 512
    /// slots (2-byte indices), and compares both iterations. Kept cheap
    /// (it runs at import): one-character keys, `None` values.
    pub fn self_test() -> bool {
        unsafe {
            let mut ok = true;
            for (nkeys, str_keys, deleted) in [
                (0usize, true, &[][..]),
                (6, true, &[1usize][..]),
                (200, true, &[0, 7, 150, 199][..]),
                (6, false, &[2][..]),
            ] {
                let d = ffi::PyDict_New();
                if d.is_null() {
                    ffi::PyErr_Clear();
                    return false;
                }
                for i in 0..nkeys {
                    let key = if str_keys || i % 2 == 0 {
                        ffi::PyUnicode_FromOrdinal(0x4e00 + i as std::os::raw::c_int)
                    } else {
                        ffi::PyLong_FromLong(i as _)
                    };
                    ok &= !key.is_null() && ffi::PyDict_SetItem(d, key, ffi::Py_None()) == 0;
                    if !key.is_null() {
                        ffi::Py_DECREF(key);
                    }
                }
                for &i in deleted {
                    let key = ffi::PyUnicode_FromOrdinal(0x4e00 + i as std::os::raw::c_int);
                    ok &= !key.is_null() && ffi::PyDict_DelItem(d, key) == 0;
                    if !key.is_null() {
                        ffi::Py_DECREF(key);
                    }
                }
                ok &= same_as_next(d);
                ffi::Py_DECREF(d);
                if !ok {
                    ffi::PyErr_Clear();
                    return false;
                }
            }
            ok
        }
    }
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
/// `dst` must have room for the escaped length plus 32 bytes of slack for
/// blind vector stores (so `len * 6 + 32` always suffices, and
/// `len + 5 * count_escapes(..) + 32` exactly). Returns the new end.
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

/// AVX2 available (checked once at init).
#[cfg(target_arch = "x86_64")]
static mut HAS_AVX2: bool = false;

#[cfg(target_arch = "x86_64")]
fn detect_cpu() {
    // `--cfg rjson_no_avx512` forces the SSE2/AVX2 kernels and
    // `--cfg rjson_no_avx2` the SSE scans (for testing them on newer CPUs).
    unsafe {
        HAS_AVX512VL = !cfg!(rjson_no_avx512)
            && std::arch::is_x86_feature_detected!("avx512f")
            && std::arch::is_x86_feature_detected!("avx512bw")
            && std::arch::is_x86_feature_detected!("avx512vl");
        HAS_AVX2 = !cfg!(rjson_no_avx2) && std::arch::is_x86_feature_detected!("avx2");
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
        let v = load256(src);
        let m = _mm256_cmpeq_epi8_mask(v, quote)
            | _mm256_cmpeq_epi8_mask(v, bslash)
            | _mm256_cmplt_epu8_mask(v, x20);
        if m == 0 {
            store256(dst, v);
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
            store256(dst, v);
            dst = dst.add(rem);
        } else {
            dst = escape_block(dst, src, rem, m);
        }
    }
    dst
}

/// Unaligned 32-byte load/store as single instructions. The crate is built
/// for x86-64-v2, whose generic tuning makes LLVM split every unaligned
/// 256-bit access into two 128-bit halves plus an insert/extract, even in
/// functions compiled with AVX2/AVX-512 enabled; that doubled the uops of
/// the escape loops.
///
/// Only called from functions with AVX enabled.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx")]
#[inline]
unsafe fn load256(p: *const u8) -> std::arch::x86_64::__m256i {
    let v;
    std::arch::asm!(
        "vmovdqu {v}, ymmword ptr [{p}]",
        p = in(reg) p,
        v = out(ymm_reg) v,
        options(pure, readonly, nostack, preserves_flags)
    );
    v
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx")]
#[inline]
unsafe fn store256(p: *mut u8, v: std::arch::x86_64::__m256i) {
    std::arch::asm!(
        "vmovdqu ymmword ptr [{p}], {v}",
        p = in(reg) p,
        v = in(ymm_reg) v,
        options(nostack, preserves_flags)
    );
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
            let v = load256(src);
            let m = x86::mask32(v);
            if m == 0 {
                store256(dst, v);
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
    let bytes = n * kind as usize;
    #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
    if bytes >= 16 {
        return match kind {
            1 => kscan::scan::<1>(data, bytes),
            2 => kscan::scan::<2>(data, bytes),
            _ => kscan::scan::<4>(data, bytes),
        };
    }
    match kind {
        1 => units_need_escape(data, n),
        2 => units_need_escape(data as *const u16, n),
        _ => units_need_escape(data as *const u32, n),
    }
}

/// SIMD scans for `kind_needs_escape`. UCS2/UCS4 units are narrowed to
/// bytes with saturating packs (a unit above 0xff becomes 0xff, which never
/// needs escaping), so every 64 (SSE) or 128 (AVX2) input bytes cost one
/// byte-vector check. Pack lane order does not matter for an "any" test.
#[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
mod kscan {
    use std::arch::x86_64::*;

    /// Lanes of a byte vector that need escaping (< 0x20, '"', '\\').
    #[inline(always)]
    unsafe fn esc8(v: __m128i) -> __m128i {
        _mm_or_si128(
            _mm_or_si128(
                _mm_cmpeq_epi8(v, _mm_set1_epi8(0x22)),
                _mm_cmpeq_epi8(v, _mm_set1_epi8(0x5c)),
            ),
            _mm_cmpeq_epi8(_mm_min_epu8(v, _mm_set1_epi8(0x1f)), v),
        )
    }

    /// Two vectors of `K`-byte units -> one vector of bytes with the same
    /// escape status (K = 2 or 4).
    #[inline(always)]
    unsafe fn narrow<const K: usize>(a: __m128i, b: __m128i) -> __m128i {
        let ff = _mm_set1_epi16(0xff);
        if K == 2 {
            _mm_packus_epi16(_mm_min_epu16(a, ff), _mm_min_epu16(b, ff))
        } else {
            // Code points are < 0x110000, so the signed saturation of
            // packus_epi32 maps them to 0..=0xffff exactly or 0xffff.
            let w = _mm_packus_epi32(a, b);
            _mm_packus_epi16(_mm_min_epu16(w, ff), _mm_min_epu16(w, ff))
        }
    }

    /// Escape lanes of 64 bytes at `p`.
    #[inline(always)]
    unsafe fn block64<const K: usize>(p: *const u8) -> __m128i {
        let a = _mm_loadu_si128(p as *const __m128i);
        let b = _mm_loadu_si128(p.add(16) as *const __m128i);
        let c = _mm_loadu_si128(p.add(32) as *const __m128i);
        let d = _mm_loadu_si128(p.add(48) as *const __m128i);
        if K == 1 {
            _mm_or_si128(
                _mm_or_si128(esc8(a), esc8(b)),
                _mm_or_si128(esc8(c), esc8(d)),
            )
        } else if K == 2 {
            _mm_or_si128(esc8(narrow::<2>(a, b)), esc8(narrow::<2>(c, d)))
        } else {
            let ab = _mm_packus_epi32(a, b);
            let cd = _mm_packus_epi32(c, d);
            esc8(narrow::<2>(ab, cd))
        }
    }

    /// Escape lanes of 16 bytes at `p`.
    #[inline(always)]
    unsafe fn block16<const K: usize>(p: *const u8) -> __m128i {
        let a = _mm_loadu_si128(p as *const __m128i);
        if K == 1 {
            esc8(a)
        } else {
            esc8(narrow::<K>(a, a))
        }
    }

    /// `bytes >= 16`, a multiple of `K`.
    #[inline(always)]
    pub unsafe fn scan<const K: usize>(p: *const u8, bytes: usize) -> bool {
        if bytes >= 128 && super::HAS_AVX2 {
            return scan_avx2::<K>(p, bytes);
        }
        let mut i = 0;
        if bytes >= 64 {
            while i + 64 <= bytes {
                if _mm_movemask_epi8(block64::<K>(p.add(i))) != 0 {
                    return true;
                }
                i += 64;
            }
            // Final partial block: overlap the previous one.
            return i != bytes && _mm_movemask_epi8(block64::<K>(p.add(bytes - 64))) != 0;
        }
        while i + 16 <= bytes {
            if _mm_movemask_epi8(block16::<K>(p.add(i))) != 0 {
                return true;
            }
            i += 16;
        }
        i != bytes && _mm_movemask_epi8(block16::<K>(p.add(bytes - 16))) != 0
    }

    /// Copies `bytes >= 128` bytes of `K`-byte units from `src` to `dst`
    /// (same kind) while checking them: one pass over the source instead of
    /// a scan and a memcpy. Returns true (with `dst` partially written) if a
    /// unit needs escaping. AVX2 only.
    #[target_feature(enable = "avx2")]
    pub unsafe fn copy_scan_avx2<const K: usize>(
        src: *const u8,
        dst: *mut u8,
        bytes: usize,
    ) -> bool {
        #[inline(always)]
        unsafe fn esc8(v: __m256i) -> __m256i {
            _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0x22)),
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0x5c)),
                ),
                _mm256_cmpeq_epi8(_mm256_min_epu8(v, _mm256_set1_epi8(0x1f)), v),
            )
        }
        #[inline(always)]
        unsafe fn n2(a: __m256i, b: __m256i) -> __m256i {
            let ff = _mm256_set1_epi16(0xff);
            _mm256_packus_epi16(_mm256_min_epu16(a, ff), _mm256_min_epu16(b, ff))
        }
        #[inline(always)]
        unsafe fn block<const K: usize>(s: *const u8, d: *mut u8) -> __m256i {
            let a = super::load256(s);
            let b = super::load256(s.add(32));
            let c = super::load256(s.add(64));
            let e = super::load256(s.add(96));
            super::store256(d, a);
            super::store256(d.add(32), b);
            super::store256(d.add(64), c);
            super::store256(d.add(96), e);
            if K == 1 {
                _mm256_or_si256(
                    _mm256_or_si256(esc8(a), esc8(b)),
                    _mm256_or_si256(esc8(c), esc8(e)),
                )
            } else if K == 2 {
                _mm256_or_si256(esc8(n2(a, b)), esc8(n2(c, e)))
            } else {
                esc8(n2(_mm256_packus_epi32(a, b), _mm256_packus_epi32(c, e)))
            }
        }
        let mut i = 0;
        while i + 128 <= bytes {
            if _mm256_movemask_epi8(block::<K>(src.add(i), dst.add(i))) != 0 {
                return true;
            }
            i += 128;
        }
        i != bytes
            && _mm256_movemask_epi8(block::<K>(src.add(bytes - 128), dst.add(bytes - 128))) != 0
    }

    /// AVX2 version of the 64-byte loop, 128 bytes per check. `bytes >= 128`.
    #[target_feature(enable = "avx2")]
    unsafe fn scan_avx2<const K: usize>(p: *const u8, bytes: usize) -> bool {
        #[inline(always)]
        unsafe fn esc8(v: __m256i) -> __m256i {
            _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0x22)),
                    _mm256_cmpeq_epi8(v, _mm256_set1_epi8(0x5c)),
                ),
                _mm256_cmpeq_epi8(_mm256_min_epu8(v, _mm256_set1_epi8(0x1f)), v),
            )
        }
        #[inline(always)]
        unsafe fn n2(a: __m256i, b: __m256i) -> __m256i {
            let ff = _mm256_set1_epi16(0xff);
            _mm256_packus_epi16(_mm256_min_epu16(a, ff), _mm256_min_epu16(b, ff))
        }
        #[inline(always)]
        unsafe fn block<const K: usize>(p: *const u8) -> __m256i {
            let a = super::load256(p);
            let b = super::load256(p.add(32));
            let c = super::load256(p.add(64));
            let d = super::load256(p.add(96));
            if K == 1 {
                _mm256_or_si256(
                    _mm256_or_si256(esc8(a), esc8(b)),
                    _mm256_or_si256(esc8(c), esc8(d)),
                )
            } else if K == 2 {
                _mm256_or_si256(esc8(n2(a, b)), esc8(n2(c, d)))
            } else {
                esc8(n2(_mm256_packus_epi32(a, b), _mm256_packus_epi32(c, d)))
            }
        }
        let mut i = 0;
        while i + 128 <= bytes {
            if _mm256_movemask_epi8(block::<K>(p.add(i))) != 0 {
                return true;
            }
            i += 128;
        }
        i != bytes && _mm256_movemask_epi8(block::<K>(p.add(bytes - 128))) != 0
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

/// Number of code units that escaping adds to a UCS1/UCS2/UCS4 buffer.
unsafe fn escape_extra<S: Unit>(src: *const S, n: usize) -> usize {
    let mut extra = 0;
    for &c in std::slice::from_raw_parts(src, n) {
        let c = c.to_u32();
        if c < 0x60 && NEEDS_ESCAPE[c as usize] != 0 {
            extra += ESCAPE_TAB[c as usize][7] as usize - 1;
        }
    }
    extra
}

/// Copies `n` code units, JSON-escaping them, widening from `S` to `D`.
/// Writes exactly `n + escape_extra(src, n)` units.
unsafe fn widen_escaped<S: Unit, D: Unit>(src: *const S, dst: *mut D, n: usize) -> usize {
    let mut o = 0;
    for &c in std::slice::from_raw_parts(src, n) {
        let c = c.to_u32();
        if c < 0x60 && NEEDS_ESCAPE[c as usize] != 0 {
            let e = &ESCAPE_TAB[c as usize];
            for (j, &b) in e[..e[7] as usize].iter().enumerate() {
                *dst.add(o + j) = D::from_u32(b as u32);
            }
            o += e[7] as usize;
        } else {
            *dst.add(o) = D::from_u32(c);
            o += 1;
        }
    }
    o
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
    /// Number of code points of the source string.
    nchars: usize,
    /// Code points added by escaping it (0: copied verbatim).
    extra: usize,
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
        // Headroom stays below the shrink threshold in `into_object`, so a
        // steady workload never shrinks (see there).
        let cap = (hint + hint / 16 + 16).max(MIN_CAPACITY);
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
        // Only give memory back when more than 1/8 is unused. Shrinking by
        // the usual headroom made every call free a block smaller than the
        // next call's request; glibc's dynamic mmap threshold only rises to
        // the size of freed blocks, so above ~128 KiB every call then got a
        // fresh mmap and page-faulted its whole output (e.g. 390 faults and
        // +300% for a 1.6 MB result, depending on what the process had
        // freed before). Freeing blocks of the requested size keeps them on
        // the heap.
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
            let mut n = (len - off).min(ESCAPE_CHUNK);
            let room = self.room(p);
            if room < n * 6 + 32 {
                // Escape as much as the room surely holds; only near the end
                // reserve exactly what the rest needs. Reserving the 6x
                // worst case in a buffer sized from the previous result
                // forced a doubling realloc (and a shrink) per call, and
                // counting every chunk's escapes cost an extra pass.
                let safe = room.saturating_sub(32) / 6;
                if safe >= 4096 {
                    n = n.min(safe);
                } else {
                    p = self.reserve(p, n + 5 * count_escapes(src.add(off), n) + 32);
                }
            }
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
        // Strong reference to the string whose native data will be copied.
        ffi::Py_INCREF(obj);
        let p = self.put(p, b'"');
        self.segs.push(Segment {
            pos: self.offset(p),
            obj,
            nchars: len,
            // Escaping is checked (and done) by the final copy, see
            // `finish_str`.
            extra: 0,
        });
        self.max_kind = self.max_kind.max(kind);
        self.seg_chars += len;
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
        let mut ns = 0;
        #[cfg(rjson_dict_direct)]
        if let Some((base, n, stride)) = dictiter::entries(obj) {
            // Nothing below runs Python code, so the table cannot change.
            for i in 0..n {
                let e = base.add(i * stride);
                let value = *e.add(1);
                if value.is_null() {
                    continue; // deleted entry
                }
                p = tri!(self.dict_item(p, *e, value, ns));
                ns = 1;
            }
            self.depth -= 1;
            return self.put(p, b'}');
        }
        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        while ffi::PyDict_Next(obj, &mut pos, &mut key, &mut value) != 0 {
            p = tri!(self.dict_item(p, key, value, ns));
            ns = 1;
        }
        self.depth -= 1;
        self.put(p, b'}')
    }

    /// Writes `,` (if `ns == 1`), `"key":value`.
    #[inline(always)]
    unsafe fn dict_item(
        &mut self,
        p: Cur,
        key: *mut ffi::PyObject,
        value: *mut ffi::PyObject,
        ns: usize,
    ) -> CurResult {
        let p = if ffi::Py_TYPE(key) == str_type() {
            tri!(self.write_str(p, key, b',', ns))
        } else if ffi::PyUnicode_Check(key) != 0 {
            let p = self.put_sep(p, b',', ns);
            tri!(self.write_str_slow(p, key))
        } else {
            return self.fail(SerError::KeyNotStr);
        };
        self.ser(p, value, b':', 1)
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
        let mut total = len + self.seg_chars;
        let maxchar = match self.max_kind {
            1 => 0xff,
            2 => 0xffff,
            _ => 0x10ffff,
        };
        let mut s = ffi::PyUnicode_New(total as ffi::Py_ssize_t, maxchar);
        if s.is_null() {
            return s;
        }
        // Optimistic pass: assume no string needs escaping, checking each one
        // right before copying it (so its data is read from cache).
        let mut st = FillState::default();
        if self.fill_any(s, &mut st, true) {
            debug_assert_eq!(st.o, total);
            return s;
        }
        // A string needs escaping: everything before it is final. Count the
        // escapes of the rest, grow the result and finish with escaping.
        for seg in &mut self.segs[st.seg..] {
            let d = ffi::PyUnicode_DATA(seg.obj) as *const u8;
            let kind = ffi::PyUnicode_KIND(seg.obj);
            if kind_needs_escape(kind, d, seg.nchars) {
                seg.extra = match kind {
                    ffi::PyUnicode_1BYTE_KIND => escape_extra(d, seg.nchars),
                    ffi::PyUnicode_2BYTE_KIND => escape_extra(d as *const u16, seg.nchars),
                    _ => escape_extra(d as *const u32, seg.nchars),
                };
                total += seg.extra;
            }
        }
        if ffi::PyUnicode_Resize(&mut s, total as ffi::Py_ssize_t) != 0 {
            // `s` was released and an exception is set.
            return ptr::null_mut();
        }
        let done = self.fill_any(s, &mut st, false);
        debug_assert!(done && st.o == total);
        let _ = done;
        s
    }

    unsafe fn fill_any(&self, s: *mut ffi::PyObject, st: &mut FillState, check: bool) -> bool {
        let data = ffi::PyUnicode_DATA(s);
        match self.max_kind {
            1 => self.fill(data as *mut u8, st, check),
            2 => self.fill(data as *mut u16, st, check),
            _ => self.fill(data as *mut u32, st, check),
        }
    }

    /// Writes the result from `st` on: ASCII runs from the buffer and the
    /// segments' native data. With `check`, stops (returning false) before
    /// the first segment that needs escaping; otherwise segments with
    /// `extra != 0` are escaped while copying.
    unsafe fn fill<D: Unit>(&self, out: *mut D, st: &mut FillState, check: bool) -> bool {
        let buf = self.buf.as_ptr();
        let FillState {
            seg: mut i,
            mut o,
            mut prev,
        } = *st;
        while i < self.segs.len() {
            let seg = &self.segs[i];
            let d = ffi::PyUnicode_DATA(seg.obj);
            let kind = ffi::PyUnicode_KIND(seg.obj);
            let n = seg.nchars;
            let bytes = n * kind as usize;
            // Large same-kind strings are checked while being copied.
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
            let fused =
                check && std::mem::size_of::<D>() == kind as usize && bytes >= 512 && HAS_AVX2;
            #[cfg(not(all(target_arch = "x86_64", target_feature = "sse4.1")))]
            let fused = false;
            if check && !fused && kind_needs_escape(kind, d as *const u8, n) {
                *st = FillState { seg: i, o, prev };
                return false;
            }
            let before = FillState { seg: i, o, prev };
            widen(buf.add(prev), out.add(o), seg.pos - prev);
            o += seg.pos - prev;
            prev = seg.pos;
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
            if fused {
                let dst = out.add(o) as *mut u8;
                let esc = match kind {
                    ffi::PyUnicode_1BYTE_KIND => {
                        kscan::copy_scan_avx2::<1>(d as *const u8, dst, bytes)
                    }
                    ffi::PyUnicode_2BYTE_KIND => {
                        kscan::copy_scan_avx2::<2>(d as *const u8, dst, bytes)
                    }
                    _ => kscan::copy_scan_avx2::<4>(d as *const u8, dst, bytes),
                };
                if esc {
                    // What was written from `before.o` on is rewritten later.
                    *st = before;
                    return false;
                }
                o += n;
                i += 1;
                continue;
            }
            if seg.extra == 0 {
                match kind {
                    ffi::PyUnicode_1BYTE_KIND => widen(d as *const u8, out.add(o), n),
                    ffi::PyUnicode_2BYTE_KIND => widen(d as *const u16, out.add(o), n),
                    _ => widen(d as *const u32, out.add(o), n),
                }
                o += n;
            } else {
                let w = match kind {
                    ffi::PyUnicode_1BYTE_KIND => widen_escaped(d as *const u8, out.add(o), n),
                    ffi::PyUnicode_2BYTE_KIND => widen_escaped(d as *const u16, out.add(o), n),
                    _ => widen_escaped(d as *const u32, out.add(o), n),
                };
                debug_assert_eq!(w, n + seg.extra);
                o += n + seg.extra;
            }
            i += 1;
        }
        widen(buf.add(prev), out.add(o), self.buf.len() - prev);
        o += self.buf.len() - prev;
        *st = FillState { seg: i, o, prev };
        true
    }
}

/// Progress of `Serializer::fill`: next segment, units written, buffer
/// offset copied up to.
#[derive(Clone, Copy, Default)]
struct FillState {
    seg: usize,
    o: usize,
    prev: usize,
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
