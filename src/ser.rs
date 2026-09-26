//! Direct CPython-API JSON serializer backing `dumps` (-> `bytes`, also
//! exported as `dumps_bytes`) and `dumps_str` (-> `str`).
//!
//! Design notes
//! - Type dispatch is a chain of exact `ob_type` pointer comparisons against the
//!   builtin type objects; no PyO3 wrappers, no per-element refcounting.
//! - Writers take the output cursor and return the advanced one (null on
//!   error), so it stays in a register; see `Cur`.
//! - Dicts are iterated over their entry array directly on CPython 3.11-3.13
//!   (gated and self-tested, see `dictiter`), else with `PyDict_Next`. Runs of
//!   exact ints/floats in lists use a register-resident loop.
//! - Output is written straight into the result object (`Out`): a `bytes`
//!   object, or a compact ASCII `str`, sized from the recent output lengths on
//!   this thread, grown with realloc and shortened at the end. No final copy,
//!   and no shared buffer that a re-entrant call could trip over.
//! - Every write reserves its worst case before writing through raw pointers, so
//!   no write can go past the allocation (the old SIMD escaper could).
//! - Separators are fused into the next value's write (one length update per
//!   element instead of two); short (<= 16-byte) compact ASCII strings are
//!   written inline with one SSSE3 shuffle.
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
//!   At the end the result `str` is allocated once with the exact kind and
//!   filled by widening the ASCII runs and copying the source strings' native
//!   UCS1/UCS2/UCS4 data, each checked for (and, rarely, copied with) escapes
//!   right before it is copied. This avoids both the UTF-8 encode and the full
//!   UTF-8 decode that `PyUnicode_FromStringAndSize` would do.
//! - Recursion is limited to `RECURSION_LIMIT` nested containers, which also
//!   turns circular references into an error instead of a stack overflow.

use pyo3::ffi;
use pyo3::prelude::*;
use std::cell::Cell;
use std::ptr;

use crate::native;

/// Maximum container nesting depth (same as orjson).
pub const RECURSION_LIMIT: u32 = 254;

/// Strings longer than this are escaped in chunks so that the worst-case
/// reservation (6x) stays bounded.
const ESCAPE_CHUNK: usize = 64 * 1024;

/// Error kind; payloads live in `Serializer::err_*`.
///
/// Every kind except `PyErrSet` is raised as `rjson.JSONEncodeError` (see
/// `to_pyerr`); `PyErrSet` propagates the Python exception as-is.
#[derive(Clone, Copy)]
pub enum SerError {
    NonFinite,
    /// `err_obj` is the offending value (a strong reference).
    Unsupported,
    /// `err_obj` is the offending key (a strong reference).
    KeyNotStr,
    Recursion,
    /// `datetime.time` with a `tzinfo` (orjson rejects it too: a time of day
    /// has no well-defined UTC offset).
    TimeTz,
    /// Serializing a native type would run Python code while unguarded;
    /// `dumps_raw` restarts in guarded mode (see `Serializer::guard`). Never
    /// reaches Python.
    NeedGuard,
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
    /// Number of code points of the source string (escaping, if needed,
    /// happens in the final copy).
    nchars: usize,
}

// ---------------------------------------------------------------------------
// Output buffer
// ---------------------------------------------------------------------------

/// Output buffer that *is* the result object: a `bytes` object (bytes mode) or
/// a compact ASCII `str` (str mode), grown with realloc and shrunk to the final
/// length at the end, so the output is never copied (like orjson's BytesWriter).
/// The initial capacity comes from the recent output sizes on this thread.
struct Out {
    obj: *mut ffi::PyObject,
    data: *mut u8,
    len: usize,
    cap: usize,
    unicode: bool,
    /// Capacity the first growth jumps to (the larger recent output size,
    /// with headroom), or 0: see `reserve`.
    jump: usize,
}

/// Recent output sizes of one mode on one thread: the capacity hints.
#[derive(Clone, Copy)]
struct SizeHistory {
    /// The last two sizes; their minimum is the initial capacity. The
    /// minimum of two rather than the last size: a steady workload still gets
    /// an exactly sized buffer, but one large result (a big page among small
    /// responses) no longer makes the next small call allocate, touch and
    /// free a block of the large size (an ~800 KB malloc/free that glibc may
    /// serve with mmap: 2x the time of a 150 B dumps).
    last: usize,
    prev: usize,
    /// Largest size of the last `PEAK_CALLS` calls: where the buffer jumps
    /// when it first has to grow (`Out::reserve`), so a periodic big result
    /// among small ones gets one allocation of its usual size (which the
    /// heap can reuse) instead of a chain of doublings into fresh memory.
    peak: usize,
    /// Calls since `peak` was set.
    age: usize,
}

const PEAK_CALLS: usize = 64;

impl SizeHistory {
    const EMPTY: SizeHistory = SizeHistory { last: 0, prev: 0, peak: 0, age: 0 };

    #[inline(always)]
    fn push(self, len: usize) -> SizeHistory {
        let (peak, age) = if len >= self.peak || self.age >= PEAK_CALLS {
            (len, 0)
        } else {
            (self.peak, self.age + 1)
        };
        SizeHistory { last: len, prev: self.last, peak, age }
    }
}

thread_local! {
    /// Per mode ([bytes, str]) because a str-mode buffer holds only the
    /// ASCII parts of non-ASCII output, so a shared history made alternating
    /// dumps/dumps_str calls grow and then shrink the buffer.
    static SIZES: [Cell<SizeHistory>; 2] =
        const { [Cell::new(SizeHistory::EMPTY), Cell::new(SizeHistory::EMPTY)] };
}

const MIN_CAPACITY: usize = 128;
/// Shrinking a much larger buffer copies instead of reallocating in place, so
/// a small result never pins a large (possibly mmapped) block.
const SHRINK_COPY_THRESHOLD: usize = 64 * 1024;
/// Growth past this size (without a size estimate from recent results)
/// reserves at least `LARGE_RESERVE`; see `Out::reserve`.
const LARGE_GROWTH: usize = 1 << 20;
/// Above glibc's largest dynamic mmap threshold (32 MiB on 64-bit), so the
/// block is always mmapped: untouched pages cost no memory, and growing or
/// shrinking it is an mremap (no copy). Other allocators treat a request
/// this size the same way.
const LARGE_RESERVE: usize = (32 << 20) + (64 << 10);

/// Capacity for an expected output of `len` bytes: headroom below the
/// shrink threshold in `into_object`, so a steady workload never shrinks.
#[inline(always)]
fn with_headroom(len: usize) -> usize {
    (len + len / 16 + 16).max(MIN_CAPACITY)
}

impl Out {
    unsafe fn new(unicode: bool) -> Out {
        let h = SIZES.with(|c| c[unicode as usize].get());
        let cap = with_headroom(h.last.min(h.prev));
        let jump = with_headroom(h.peak);
        let obj = Self::alloc(cap, unicode);
        Out {
            obj,
            data: Self::data_of(obj, unicode),
            len: 0,
            cap,
            unicode,
            jump: if jump > cap { jump } else { 0 },
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
    ///
    /// Growth policy (a large output that has to grow is where time and
    /// peak memory went, see docs/PERFORMANCE_REVIEW.md):
    /// - The first growth jumps to the larger recent output size when that
    ///   suffices, so a big result after a small one is one realloc to its
    ///   usual size, which is then freed at that size (not shrunk).
    /// - Otherwise the capacity doubles; once that passes `LARGE_GROWTH` it
    ///   jumps to at least `LARGE_RESERVE`, which malloc serves with mmap.
    ///   Doubling on the brk heap copied the buffer at every step that could
    ///   not extend in place, and the step that crossed glibc's dynamic mmap
    ///   threshold moved it to a fresh mapping, stranding the old block on
    ///   the heap (a 21 MB result peaked at 1.8x its size and left 16 MB
    ///   resident). A mapped buffer grows and shrinks by mremap, without
    ///   copying, and only its written pages become resident.
    #[inline(never)]
    fn reserve(&mut self, n: usize) {
        if self.cap - self.len >= n {
            return;
        }
        let need = self.len.checked_add(n).unwrap_or_else(|| Self::oom(isize::MAX as usize));
        let jump = std::mem::replace(&mut self.jump, 0);
        let cap = if jump >= need {
            jump
        } else {
            let cap = need.max(self.cap.saturating_mul(2));
            if cap >= LARGE_GROWTH {
                cap.max(LARGE_RESERVE)
            } else {
                cap
            }
        };
        unsafe { self.resize(cap) };
    }

    unsafe fn resize(&mut self, cap: usize) {
        let rc = if self.unicode {
            ffi::PyUnicode_Resize(&mut self.obj, cap as ffi::Py_ssize_t)
        } else {
            crate::compat::_PyBytes_Resize(&mut self.obj, cap as ffi::Py_ssize_t)
        };
        if rc != 0 {
            Self::oom(cap);
        }
        self.data = Self::data_of(self.obj, self.unicode);
        self.cap = cap;
    }

    /// Records this call's output size for the next call's capacity hint.
    /// Called exactly once per `Out` (by `into_object`, or by `drop` when the
    /// buffer was not handed over).
    #[inline(always)]
    fn record_len(&self) {
        SIZES.with(|c| {
            let c = &c[self.unicode as usize];
            c.set(c.get().push(self.len));
        });
    }

    /// Shrinks to the written length and hands the object over.
    unsafe fn into_object(&mut self) -> *mut ffi::PyObject {
        self.record_len();
        if self.cap > SHRINK_COPY_THRESHOLD
            && self.len < self.cap / 4
            && self.len <= SHRINK_COPY_THRESHOLD
        {
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
            // Shrink to what the next call of this size will request (not
            // to the exact length), for the same reason: a grown buffer
            // then leaves a freed block the next request fits in.
            self.resize(with_headroom(self.len));
        }
        if self.cap != self.len {
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
            // Non-ASCII str results (and errors) end here: the buffer's
            // length is what the next call's buffer will hold.
            self.record_len();
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
    /// Strong reference (set by `fail_obj`): with `default=`, the offending
    /// object may live in a temporary `default` result that is released
    /// before the error message is built.
    err_obj: *mut ffi::PyObject,
    err_float: f64,
    /// `default=` callable (borrowed from the call's arguments), or null.
    /// When set, unsupported objects are replaced by `default(obj)`.
    default: *mut ffi::PyObject,
    /// `passthrough=` flags (`native::PT_*`): native kinds sent to `default`.
    passthrough: u32,
    /// `non_str_keys=True`: bool/None/int/float keys get `json.dumps`'s text,
    /// Enum keys their value, datetime/date/time/UUID keys their native text
    /// (see `key_text`). Otherwise a non-str key raises.
    non_str_keys: bool,
    /// Guarded mode: Python code may run during serialization (`default`, a
    /// dataclass attribute, a Python `tzinfo`), so containers are held by a
    /// strong reference while serialized and dicts are walked with
    /// `PyDict_Next` plus a size check (see `guarded`). Set when `default` is
    /// given; otherwise a native value that needs Python code fails with
    /// `NeedGuard` before running any, and `dumps_raw` starts over in this
    /// mode. Unguarded, no Python code runs, so the fast paths iterate
    /// borrowed references.
    guard: bool,
}

impl Drop for Serializer {
    fn drop(&mut self) {
        for s in self.segs.drain(..) {
            unsafe { ffi::Py_DECREF(s.obj) };
        }
        if !self.err_obj.is_null() {
            unsafe { ffi::Py_DECREF(self.err_obj) };
        }
    }
}

impl Serializer {
    unsafe fn new(str_mode: bool, opts: &DumpsOpts, guard: bool) -> Self {
        let default = opts.default;
        Serializer {
            buf: Out::new(str_mode),
            depth: 0,
            str_mode,
            segs: Vec::new(),
            max_kind: 1,
            seg_chars: 0,
            err: SerError::PyErrSet,
            err_obj: ptr::null_mut(),
            default,
            passthrough: opts.passthrough,
            non_str_keys: opts.non_str_keys,
            guard: guard || !default.is_null(),
            err_float: 0.0,
        }
    }

    #[cold]
    #[inline(never)]
    fn fail(&mut self, e: SerError) -> Cur {
        self.err = e;
        ptr::null_mut()
    }

    /// `fail` that also records the offending object (borrowed; it stays alive
    /// until `to_pyerr` because its container is still referenced by the caller
    /// of `dumps`, and no Python code runs in between).
    #[cold]
    #[inline(never)]
    fn fail_obj(&mut self, e: SerError, obj: *mut ffi::PyObject) -> Cur {
        // A serializer stops at its first error, so this is set at most once.
        unsafe { ffi::Py_INCREF(obj) };
        self.err_obj = obj;
        self.fail(e)
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
        if crate::compat::PyUnicode_IS_COMPACT_ASCII(obj) != 0 {
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
        if !self.str_mode && crate::compat::PyUnicode_IS_COMPACT(obj) != 0 {
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
        if crate::compat::PyUnicode_IS_ASCII(obj) != 0 {
            return self.write_utf8(p, crate::compat::PyUnicode_DATA(obj) as *const u8, len, 0, 0);
        }
        if self.str_mode {
            return self.write_str_segment(p, obj, len);
        }
        // bytes output. A UTF-8 copy CPython already cached is the fastest
        // source. Otherwise encode the native UCS1/2/4 data directly:
        // `PyUnicode_AsUTF8AndSize` would attach a UTF-8 copy to the string
        // for the rest of its life (+98% memory on non-ASCII text that
        // outlives the call).
        if crate::compat::PyUnicode_IS_COMPACT(obj) != 0 {
            let c = obj as *mut ffi::PyCompactUnicodeObject;
            if !(*c).utf8.is_null() {
                return self.write_utf8(p, (*c).utf8 as *const u8, (*c).utf8_length as usize, 0, 0);
            }
        }
        if len < DIRECT_UTF8_MIN_CHARS {
            // Short strings (keys, names, labels) are cheap to cache and
            // often serialized again: let CPython attach the UTF-8 copy.
            let (src, n) = match utf8_of(obj) {
                Ok(v) => v,
                Err(e) => return self.fail(e),
            };
            return self.write_utf8(p, src, n, 0, 0);
        }
        let kind = crate::compat::PyUnicode_KIND(obj);
        if kind == 1 {
            // Latin-1: CPython's UTF-8 encoder is faster than per-character
            // encoding here (accented letters every few characters defeat
            // the 8-unit ASCII blocks). Encode into a temporary bytes object,
            // which is not attached to the string, and escape-copy it.
            let b = ffi::PyUnicode_AsUTF8String(obj);
            if b.is_null() {
                return self.fail(SerError::PyErrSet);
            }
            let q = self.write_utf8(p, ffi::PyBytes_AsString(b) as *const u8, ffi::PyBytes_Size(b) as usize, 0, 0);
            ffi::Py_DECREF(b);
            return q;
        }
        // Worst case 6 output bytes per character (an escape; UTF-8 needs at
        // most 4), two quotes, and 8 bytes for the blind escape-table store.
        let q = self.reserve(p, len * 6 + 2 + 8);
        let data = crate::compat::PyUnicode_DATA(obj);
        *q = b'"';
        let end = if kind == 2 {
            encode_utf8_escaped(q.add(1), data as *const u16, len)
        } else {
            encode_utf8_escaped(q.add(1), data as *const u32, len)
        };
        match end {
            Some(e) => {
                *e = b'"';
                e.add(1)
            }
            // A lone surrogate: let CPython's encoder raise the same
            // UnicodeEncodeError as before (nothing past `q` is kept).
            None => {
                let (src, n) = match utf8_of(obj) {
                    Ok(v) => v,
                    Err(e) => return self.fail(e),
                };
                self.write_utf8(q, src, n, 0, 0)
            }
        }
    }

    /// `str` output: record the non-ASCII string instead of encoding it.
    ///
    /// Like `json.dumps(..., ensure_ascii=False)`, lone surrogates are copied
    /// through (a `str` can hold them); `dumps` (bytes) rejects them because
    /// they cannot be encoded as UTF-8.
    unsafe fn write_str_segment(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        len: usize,
    ) -> CurResult {
        let kind = crate::compat::PyUnicode_KIND(obj);
        // Strong reference to the string whose native data will be copied.
        ffi::Py_INCREF(obj);
        let p = self.put(p, b'"');
        self.segs.push(Segment {
            pos: self.offset(p),
            obj,
            nchars: len,
        });
        self.max_kind = self.max_kind.max(kind);
        self.seg_chars += len;
        self.put(p, b'"')
    }

    // ----- containers -----

    #[inline(never)]
    unsafe fn ser_list(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.guard {
            return self.guarded(p, obj, Self::ser_list_inner);
        }
        self.ser_list_inner(p, obj)
    }

    #[inline(always)]
    unsafe fn ser_list_inner(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
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
        if self.guard {
            return self.guarded(p, obj, Self::ser_tuple_inner);
        }
        self.ser_tuple_inner(p, obj)
    }

    #[inline(always)]
    unsafe fn ser_tuple_inner(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
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
        if self.guard {
            return self.guarded(p, obj, Self::ser_dict_checked);
        }
        self.ser_dict_inner(p, obj)
    }

    /// Guarded mode: `container` is kept alive by a strong reference while
    /// it is serialized. `default` runs arbitrary code, which could drop the
    /// last other reference to a list or dict we are in the middle of
    /// (e.g. clear the list that holds it).
    #[cold]
    #[inline(never)]
    unsafe fn guarded(
        &mut self,
        p: Cur,
        container: *mut ffi::PyObject,
        f: unsafe fn(&mut Self, Cur, *mut ffi::PyObject) -> CurResult,
    ) -> CurResult {
        ffi::Py_INCREF(container);
        let r = f(self, p, container);
        ffi::Py_DECREF(container);
        r
    }

    /// Guarded-mode dicts: `PyDict_Next` (re-validates its position
    /// against the current table on every call) instead of the direct entry
    /// walk, which keeps a pointer into the table that `default` could free
    /// by resizing the dict; and CPython's "dictionary changed size during
    /// iteration" error if `default` adds or removes items.
    unsafe fn ser_dict_checked(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        let size = ffi::PyDict_Size(obj);
        if size == 0 {
            return self.put2(p, b"{}");
        }
        self.depth += 1;
        let mut p = self.put(p, b'{');
        let mut ns = 0;
        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        while ffi::PyDict_Next(obj, &mut pos, &mut key, &mut value) != 0 {
            p = tri!(self.dict_item(p, key, value, ns));
            if ffi::PyDict_Size(obj) != size {
                return self.dict_changed();
            }
            ns = 1;
        }
        self.depth -= 1;
        self.put(p, b'}')
    }

    #[inline(always)]
    unsafe fn ser_dict_inner(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
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
        } else if self.non_str_keys {
            let p = self.put_sep(p, b',', ns);
            tri!(self.key_text(p, key, 0))
        } else {
            return self.fail_obj(SerError::KeyNotStr, key);
        };
        self.ser(p, value, b':', 1)
    }

    /// A non-str dict key as a JSON string (`non_str_keys=True`). bool, None,
    /// int and float get exactly `json.dumps`'s key text (`"true"`, `"null"`,
    /// any-size ints, float `repr` such as `"1e-07"`, `"NaN"`/`"Infinity"`);
    /// types `json` rejects follow orjson's `OPT_NON_STR_KEYS`: an Enum key is
    /// its value's key text, datetime/date/time/UUID keys their native text
    /// (unless passed through). Anything else raises.
    #[cold]
    #[inline(never)]
    unsafe fn key_text(&mut self, p: Cur, key: *mut ffi::PyObject, level: u32) -> CurResult {
        let ty = ffi::Py_TYPE(key);
        if ty == bool_type() {
            return self.put_quoted(p, if key == ffi::Py_True() { b"true" } else { b"false" });
        }
        if key == ffi::Py_None() {
            return self.put_quoted(p, b"null");
        }
        if ffi::PyLong_Check(key) != 0 {
            // int subclasses (IntEnum, IntFlag) too, as their int value.
            let p = self.put(p, b'"');
            let p = tri!(self.write_int_slow(p, key));
            return self.put(p, b'"');
        }
        if ffi::PyFloat_Check(key) != 0 {
            return self.float_key(p, ffi::PyFloat_AS_DOUBLE(key));
        }
        if ffi::PyUnicode_Check(key) != 0 {
            return self.write_str_slow(p, key); // an Enum's str value
        }
        let t = native::types();
        if self.passthrough & native::PT_DATETIME == 0 && !t.datetime.is_null() {
            if ty == t.datetime {
                return self.write_datetime(p, key, t);
            } else if ty == t.date {
                return self.write_date(p, key);
            } else if ty == t.time {
                return self.write_time(p, key);
            }
        }
        if self.passthrough & native::PT_UUID == 0 && ty == t.uuid && !ty.is_null() {
            return self.write_uuid(p, key, t);
        }
        if level == 0
            && self.passthrough & native::PT_ENUM == 0
            && !t.enum_meta.is_null()
            && ffi::PyType_IsSubtype(ffi::Py_TYPE(ty as *mut ffi::PyObject), t.enum_meta) != 0
        {
            if !self.guard && !enum_value_plain(ty) {
                return self.fail(SerError::NeedGuard);
            }
            ffi::Py_INCREF(key);
            let v = ffi::PyObject_GetAttr(key, native::names().value);
            ffi::Py_DECREF(key);
            if v.is_null() {
                return self.fail(SerError::PyErrSet);
            }
            let q = self.key_text(p, v, level + 1);
            ffi::Py_DECREF(v);
            return q;
        }
        self.fail_obj(SerError::KeyNotStr, key)
    }

    /// Float key text as `json.dumps` writes it: `float.__repr__`, or
    /// `NaN`/`Infinity`/`-Infinity` (valid here: a key is a string).
    unsafe fn float_key(&mut self, p: Cur, v: f64) -> CurResult {
        if v.is_nan() {
            return self.put_quoted(p, b"NaN");
        } else if v == f64::INFINITY {
            return self.put_quoted(p, b"Infinity");
        } else if v == f64::NEG_INFINITY {
            return self.put_quoted(p, b"-Infinity");
        }
        let mut b = [0u8; 32];
        let n = native::fmt_float_repr(v, &mut b);
        self.put_quoted(p, &b[..n])
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
            self.ser_other(p, obj)
        }
    }

    /// Everything that is not a JSON builtin or a subclass of one: the native
    /// types (unless passed through), then `default`, else an error.
    #[cold]
    #[inline(never)]
    unsafe fn ser_other(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        let t = native::types();
        let ty = ffi::Py_TYPE(obj);
        let pt = self.passthrough;
        if pt & native::PT_DATETIME == 0 && !t.datetime.is_null() {
            if ty == t.datetime {
                return self.write_datetime(p, obj, t);
            } else if ty == t.date {
                return self.write_date(p, obj);
            } else if ty == t.time {
                return self.write_time(p, obj);
            }
        }
        if pt & native::PT_UUID == 0 && ty == t.uuid && !ty.is_null() {
            return self.write_uuid(p, obj, t);
        }
        if pt & native::PT_ENUM == 0
            && !t.enum_meta.is_null()
            && ffi::PyType_IsSubtype(ffi::Py_TYPE(ty as *mut ffi::PyObject), t.enum_meta) != 0
        {
            return self.ser_enum(p, obj, ty);
        }
        if pt & native::PT_DATACLASS == 0
            && !native::own_attr(ty, native::names().dataclass_fields).is_null()
        {
            return self.ser_dataclass(p, obj, ty, t);
        }
        if !self.default.is_null() {
            self.call_default(p, obj)
        } else {
            self.fail_obj(SerError::Unsupported, obj)
        }
    }

    /// Writes `"` + `b[..n]` (ASCII) + `"`.
    unsafe fn put_quoted(&mut self, p: Cur, b: &[u8]) -> Cur {
        let n = b.len();
        let p = self.reserve(p, n + 2);
        *p = b'"';
        ptr::copy_nonoverlapping(b.as_ptr(), p.add(1), n);
        *p.add(n + 1) = b'"';
        p.add(n + 2)
    }

    /// `datetime` (exact type) as RFC 3339, like orjson: `YYYY-MM-DDTHH:MM:SS`,
    /// `.ffffff` if microseconds are non-zero, and `+HH:MM` if aware.
    /// `utcoffset()` returning None counts as naive (Python's rule; orjson
    /// writes `+00:00`).
    unsafe fn write_datetime(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        t: &native::Types,
    ) -> CurResult {
        let mut b = [0u8; 32];
        let mut n = native::fmt_date(
            &mut b,
            0,
            ffi::PyDateTime_GET_YEAR(obj) as u32,
            ffi::PyDateTime_GET_MONTH(obj) as u32,
            ffi::PyDateTime_GET_DAY(obj) as u32,
        );
        b[n] = b'T';
        n = native::fmt_time(
            &mut b,
            n + 1,
            ffi::PyDateTime_DATE_GET_HOUR(obj) as u32,
            ffi::PyDateTime_DATE_GET_MINUTE(obj) as u32,
            ffi::PyDateTime_DATE_GET_SECOND(obj) as u32,
            ffi::PyDateTime_DATE_GET_MICROSECOND(obj) as u32,
        );
        let tz = ffi::PyDateTime_DATE_GET_TZINFO(obj);
        if tz != ffi::Py_None() {
            let tzt = ffi::Py_TYPE(tz);
            // `timezone` and the C `ZoneInfo` compute the offset in C; any
            // other tzinfo (pytz, dateutil, subclasses) runs Python code.
            let c_tz = tzt == t.timezone || (tzt == t.zoneinfo && !tzt.is_null());
            if !c_tz && !self.guard {
                return self.fail(SerError::NeedGuard);
            }
            match utc_offset(obj, tz, tzt, t) {
                Ok(Some(secs)) => n = native::fmt_offset(&mut b, n, secs),
                Ok(None) => {}
                Err(()) => return self.fail(SerError::PyErrSet),
            }
        }
        self.put_quoted(p, &b[..n])
    }

    /// `date` (exact type) as `YYYY-MM-DD`.
    unsafe fn write_date(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        let mut b = [0u8; 10];
        let n = native::fmt_date(
            &mut b,
            0,
            ffi::PyDateTime_GET_YEAR(obj) as u32,
            ffi::PyDateTime_GET_MONTH(obj) as u32,
            ffi::PyDateTime_GET_DAY(obj) as u32,
        );
        self.put_quoted(p, &b[..n])
    }

    /// `time` (exact type) as `HH:MM:SS[.ffffff]`; with a tzinfo it raises.
    unsafe fn write_time(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if ffi::PyDateTime_TIME_GET_TZINFO(obj) != ffi::Py_None() {
            return self.fail(SerError::TimeTz);
        }
        let mut b = [0u8; 15];
        let n = native::fmt_time(
            &mut b,
            0,
            ffi::PyDateTime_TIME_GET_HOUR(obj) as u32,
            ffi::PyDateTime_TIME_GET_MINUTE(obj) as u32,
            ffi::PyDateTime_TIME_GET_SECOND(obj) as u32,
            ffi::PyDateTime_TIME_GET_MICROSECOND(obj) as u32,
        );
        self.put_quoted(p, &b[..n])
    }

    /// `uuid.UUID` (exact type) as its canonical lowercase hyphenated form,
    /// from its `int` attribute.
    unsafe fn write_uuid(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        t: &native::Types,
    ) -> CurResult {
        if !t.uuid_plain && !self.guard {
            return self.fail(SerError::NeedGuard);
        }
        ffi::Py_INCREF(obj);
        let v = if t.uuid_int.is_null() {
            ffi::PyObject_GetAttr(obj, native::names().int)
        } else {
            // The slot directly: no attribute lookup.
            ffi::PyMember_GetOne(obj as *const std::os::raw::c_char, t.uuid_int)
        };
        ffi::Py_DECREF(obj);
        if v.is_null() {
            return self.fail(SerError::PyErrSet);
        }
        let r = uuid_value(v);
        ffi::Py_DECREF(v);
        let Some(val) = r else {
            return self.fail(SerError::PyErrSet);
        };
        let mut b = [0u8; 36];
        native::fmt_uuid(&mut b, val);
        self.put_quoted(p, &b)
    }

    /// `Enum` member (any Enum subclass; int/str/float mix-ins never get
    /// here): its `_value_`, serialized in place (it may itself be native).
    unsafe fn ser_enum(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        ty: *mut ffi::PyTypeObject,
    ) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        if !self.guard && !enum_value_plain(ty) {
            return self.fail(SerError::NeedGuard);
        }
        ffi::Py_INCREF(obj);
        let v = ffi::PyObject_GetAttr(obj, native::names().value);
        ffi::Py_DECREF(obj);
        if v.is_null() {
            return self.fail(SerError::PyErrSet);
        }
        self.depth += 1;
        let q = self.ser(p, v, 0, 0);
        self.depth -= 1;
        ffi::Py_DECREF(v);
        q
    }

    /// Dataclass instance (its type's own `__dict__` has
    /// `__dataclass_fields__`) as an object, like orjson: without
    /// `__slots__` in the type, the instance `__dict__` in order; else the
    /// fields in definition order (ClassVar/InitVar excluded). Either way,
    /// names starting with `_` are skipped. Reading attributes may run
    /// Python code, so this always runs in guarded mode.
    unsafe fn ser_dataclass(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        ty: *mut ffi::PyTypeObject,
        t: &native::Types,
    ) -> CurResult {
        if !self.guard {
            return self.fail(SerError::NeedGuard);
        }
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        self.depth += 1;
        ffi::Py_INCREF(obj);
        let q = self.ser_dataclass_inner(p, obj, ty, t);
        ffi::Py_DECREF(obj);
        self.depth -= 1;
        q
    }

    unsafe fn ser_dataclass_inner(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        ty: *mut ffi::PyTypeObject,
        t: &native::Types,
    ) -> CurResult {
        let names = native::names();
        if native::own_attr(ty, names.slots).is_null() {
            let d = ffi::PyObject_GetAttr(obj, names.dict);
            if d.is_null() {
                if ffi::PyErr_ExceptionMatches(ffi::PyExc_AttributeError) == 0 {
                    return self.fail(SerError::PyErrSet);
                }
                ffi::PyErr_Clear();
            } else if ffi::PyDict_Check(d) != 0 {
                let q = self.ser_attr_dict(p, d);
                ffi::Py_DECREF(d);
                return q;
            } else {
                ffi::Py_DECREF(d);
            }
        }
        // Fields: re-read, since `__dict__` access may have run Python code.
        let fields = native::own_attr(ty, names.dataclass_fields);
        if fields.is_null() || ffi::PyDict_Check(fields) == 0 {
            return self.fail_obj(SerError::Unsupported, obj);
        }
        ffi::Py_INCREF(fields);
        let q = self.ser_fields(p, obj, fields, t);
        ffi::Py_DECREF(fields);
        q
    }

    /// Instance `__dict__` of a dataclass: `_`-prefixed names skipped.
    unsafe fn ser_attr_dict(&mut self, p: Cur, d: *mut ffi::PyObject) -> CurResult {
        let size = ffi::PyDict_Size(d);
        let mut p = self.put(p, b'{');
        let mut ns = 0;
        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        while ffi::PyDict_Next(d, &mut pos, &mut key, &mut value) != 0 {
            if underscore_name(key) {
                continue;
            }
            // `value` is borrowed from `d`, which Python code run while
            // serializing it could change.
            ffi::Py_INCREF(value);
            let q = self.dict_item(p, key, value, ns);
            ffi::Py_DECREF(value);
            p = tri!(q);
            if ffi::PyDict_Size(d) != size {
                return self.dict_changed();
            }
            ns = 1;
        }
        self.put(p, b'}')
    }

    /// `__dataclass_fields__` entries that are real fields, read with getattr.
    unsafe fn ser_fields(
        &mut self,
        p: Cur,
        obj: *mut ffi::PyObject,
        fields: *mut ffi::PyObject,
        t: &native::Types,
    ) -> CurResult {
        let names = native::names();
        let size = ffi::PyDict_Size(fields);
        let mut p = self.put(p, b'{');
        let mut ns = 0;
        let mut pos: ffi::Py_ssize_t = 0;
        let mut name: *mut ffi::PyObject = ptr::null_mut();
        let mut field: *mut ffi::PyObject = ptr::null_mut();
        while ffi::PyDict_Next(fields, &mut pos, &mut name, &mut field) != 0 {
            if ffi::PyUnicode_Check(name) == 0 || underscore_name(name) {
                continue;
            }
            ffi::Py_INCREF(name);
            let kind = ffi::PyObject_GetAttr(field, names.field_type);
            if kind.is_null() {
                ffi::Py_DECREF(name);
                return self.fail(SerError::PyErrSet);
            }
            ffi::Py_DECREF(kind); // only compared by identity
            if kind != t.dc_field {
                ffi::Py_DECREF(name);
                continue;
            }
            let value = ffi::PyObject_GetAttr(obj, name);
            if value.is_null() {
                ffi::Py_DECREF(name);
                return self.fail(SerError::PyErrSet);
            }
            let q = self.dict_item(p, name, value, ns);
            ffi::Py_DECREF(value);
            ffi::Py_DECREF(name);
            p = tri!(q);
            if ffi::PyDict_Size(fields) != size {
                return self.dict_changed();
            }
            ns = 1;
        }
        self.put(p, b'}')
    }

    #[cold]
    unsafe fn dict_changed(&mut self) -> Cur {
        ffi::PyErr_SetString(
            ffi::PyExc_RuntimeError,
            c"dictionary changed size during iteration".as_ptr(),
        );
        self.fail(SerError::PyErrSet)
    }

    /// Serializes `default(obj)` in place of `obj` (the separator is already
    /// written). The result may itself be unsupported, in which case
    /// `default` is called on it again; each call counts as a nesting level,
    /// so a `default` that never converges (e.g. `lambda o: o`) raises the
    /// nesting-depth error instead of recursing forever. Exceptions raised by
    /// `default` propagate unchanged, as in `json.dumps`.
    #[cold]
    #[inline(never)]
    unsafe fn call_default(&mut self, p: Cur, obj: *mut ffi::PyObject) -> CurResult {
        if self.depth >= RECURSION_LIMIT {
            return self.fail(SerError::Recursion);
        }
        // The call's caller must own its argument: `obj` is borrowed from a
        // container that `default` itself could modify.
        ffi::Py_INCREF(obj);
        let res = ffi::PyObject_CallOneArg(self.default, obj);
        ffi::Py_DECREF(obj);
        if res.is_null() {
            return self.fail(SerError::PyErrSet);
        }
        self.depth += 1;
        let q = self.ser(p, res, 0, 0);
        self.depth -= 1;
        ffi::Py_DECREF(res);
        q
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
        // with the exact kind and drop the buffer (`Out::drop` records its
        // length as the size hint).
        let total = len + self.seg_chars;
        // Escapes in the non-ASCII strings are only found while copying them;
        // leave some room so that a few do not need a realloc.
        let cap = total + self.seg_chars / 32 + 16;
        let maxchar = match self.max_kind {
            1 => 0xff,
            2 => 0xffff,
            _ => 0x10ffff,
        };
        let mut s = ffi::PyUnicode_New(cap as ffi::Py_ssize_t, maxchar);
        if s.is_null() {
            return s;
        }
        let filled = match self.max_kind {
            1 => self.fill::<u8>(&mut s, cap),
            2 => self.fill::<u16>(&mut s, cap),
            _ => self.fill::<u32>(&mut s, cap),
        };
        if filled.is_none() {
            ffi::Py_DECREF(s);
            return ptr::null_mut();
        }
        s
    }

    /// Writes the result into `*s` (allocated with `cap` units): ASCII runs
    /// from the buffer and the segments' native data, checking each segment
    /// for characters that need escaping right before copying it (so its
    /// data is read from cache) and escaping those segments while copying.
    /// Grows `*s` if the escapes do not fit, and finally shortens it in
    /// place to the written length. None (exception set) if growing failed.
    unsafe fn fill<D: Unit>(&self, s: &mut *mut ffi::PyObject, mut cap: usize) -> Option<()> {
        let buf = self.buf.as_ptr();
        let mut out = crate::compat::PyUnicode_DATA(*s) as *mut D;
        let (mut o, mut prev) = (0usize, 0usize);
        // Units still to write, not counting escapes.
        let mut rem = self.buf.len() + self.seg_chars;
        for seg in &self.segs {
            let d = crate::compat::PyUnicode_DATA(seg.obj);
            let kind = crate::compat::PyUnicode_KIND(seg.obj);
            let n = seg.nchars;
            let run = seg.pos - prev;
            widen(buf.add(prev), out.add(o), run);
            o += run;
            prev = seg.pos;
            rem -= run + n;
            // Copy, or find that the segment needs escaping. Large same-kind
            // strings are checked while being copied.
            #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
            let fused =
                std::mem::size_of::<D>() == kind as usize && n * kind as usize >= 512 && HAS_AVX2;
            #[cfg(not(all(target_arch = "x86_64", target_feature = "sse4.1")))]
            let fused = false;
            let escape = if fused {
                #[cfg(all(target_arch = "x86_64", target_feature = "sse4.1"))]
                {
                    let (src, dst, bytes) =
                        (d as *const u8, out.add(o) as *mut u8, n * kind as usize);
                    match kind {
                        ffi::PyUnicode_1BYTE_KIND => kscan::copy_scan_avx2::<1>(src, dst, bytes),
                        ffi::PyUnicode_2BYTE_KIND => kscan::copy_scan_avx2::<2>(src, dst, bytes),
                        _ => kscan::copy_scan_avx2::<4>(src, dst, bytes),
                    }
                }
                #[cfg(not(all(target_arch = "x86_64", target_feature = "sse4.1")))]
                false
            } else if kind_needs_escape(kind, d as *const u8, n) {
                true
            } else {
                match kind {
                    ffi::PyUnicode_1BYTE_KIND => widen(d as *const u8, out.add(o), n),
                    ffi::PyUnicode_2BYTE_KIND => widen(d as *const u16, out.add(o), n),
                    _ => widen(d as *const u32, out.add(o), n),
                }
                false
            };
            if !escape {
                o += n;
                continue;
            }
            let extra = match kind {
                ffi::PyUnicode_1BYTE_KIND => escape_extra(d as *const u8, n),
                ffi::PyUnicode_2BYTE_KIND => escape_extra(d as *const u16, n),
                _ => escape_extra(d as *const u32, n),
            };
            let need = o + n + extra + rem;
            if need > cap {
                cap = need + need / 16;
                // On failure `*s` is left intact (the caller releases it).
                if ffi::PyUnicode_Resize(s, cap as ffi::Py_ssize_t) != 0 {
                    return None;
                }
                out = crate::compat::PyUnicode_DATA(*s) as *mut D;
            }
            let w = match kind {
                ffi::PyUnicode_1BYTE_KIND => widen_escaped(d as *const u8, out.add(o), n),
                ffi::PyUnicode_2BYTE_KIND => widen_escaped(d as *const u16, out.add(o), n),
                _ => widen_escaped(d as *const u32, out.add(o), n),
            };
            debug_assert_eq!(w, n + extra);
            o += w;
        }
        let run = self.buf.len() - prev;
        widen(buf.add(prev), out.add(o), run);
        o += run;
        debug_assert!(o <= cap);
        if o < cap {
            // Shorten in place like `Out::into_object`: a str does not record
            // its allocation size, so a shorter length plus the terminating
            // NUL is valid, and freeing a block of the size that was
            // allocated keeps glibc from mmapping the next one.
            (*(*s as *mut ffi::PyASCIIObject)).length = o as ffi::Py_ssize_t;
            *out.add(o) = D::from_u32(0);
        }
        Some(())
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

/// Non-ASCII strings of at least this many characters are encoded directly
/// by `dumps` (bytes) instead of through CPython's cached UTF-8 copy.
const DIRECT_UTF8_MIN_CHARS: usize = 256;

/// Code unit of a str's native data (UCS2/UCS4; UCS1 goes through CPython's
/// encoder, see `write_str_slow`).
trait StrUnit: Copy {
    fn get(self) -> u32;
    /// Packs 8 units starting at `p` into 8 bytes such that the result is
    /// "all bytes < 0x80 and none needs escaping" only if all 8 units are
    /// ASCII needing no escape (then the bytes are exactly those units).
    unsafe fn pack8(p: *const Self) -> u64;
}
impl StrUnit for u16 {
    #[inline(always)]
    fn get(self) -> u32 {
        self as u32
    }
    #[inline(always)]
    unsafe fn pack8(p: *const u16) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::*;
            // packus treats lanes as signed: 0x100..=0x7FFF saturate to 0xFF
            // (high bit set) and 0x8000..=0xFFFF to 0x00, which the caller's
            // escape check rejects. Either way no non-ASCII unit can pass as
            // an ASCII byte that needs no escaping.
            let v = _mm_loadu_si128(p as *const __m128i);
            _mm_cvtsi128_si64(_mm_packus_epi16(v, v)) as u64
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let mut w = 0u64;
            for k in 0..8 {
                w |= ((*p.add(k)).min(0xFF) as u64) << (8 * k);
            }
            w
        }
    }
}
impl StrUnit for u32 {
    #[inline(always)]
    fn get(self) -> u32 {
        self
    }
    #[inline(always)]
    unsafe fn pack8(p: *const u32) -> u64 {
        #[cfg(target_arch = "x86_64")]
        {
            use std::arch::x86_64::*;
            // Code points are <= 0x10FFFF, so the signed saturation of
            // packs_epi32 keeps them positive; packus then saturates > 0xFF.
            let a = _mm_loadu_si128(p as *const __m128i);
            let b = _mm_loadu_si128(p.add(4) as *const __m128i);
            let w = _mm_packs_epi32(a, b);
            _mm_cvtsi128_si64(_mm_packus_epi16(w, w)) as u64
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            let mut w = 0u64;
            for k in 0..8 {
                w |= ((*p.add(k)).min(0xFF) as u64) << (8 * k);
            }
            w
        }
    }
}

/// Encodes `len` units of native str data as escaped UTF-8 at `dst` (the
/// body of a JSON string, without quotes) and returns the end, or None at a
/// lone surrogate (not encodable; the caller reports it).
///
/// SAFETY: `len * 6 + 8` bytes of room at `dst` (worst case: every unit an
/// escape; `write_escape` stores 8 bytes blindly), `src..src+len` readable.
#[inline(always)]
unsafe fn encode_utf8_escaped<T: StrUnit>(mut dst: *mut u8, src: *const T, len: usize) -> Option<*mut u8> {
    let mut i = 0;
    while i < len {
        let c = (*src.add(i)).get();
        if c < 0x80 {
            // Runs of 8 ASCII units that need no escaping: one 8-byte store.
            // Only tried at an ASCII unit: text of non-ASCII runs (CJK) paid
            // a failed pack and check per character (2x slower).
            if i + 8 <= len {
                let w = T::pack8(src.add(i));
                if w & 0x8080_8080_8080_8080 == 0 && !swar_needs_escape(w) {
                    ptr::write_unaligned(dst as *mut u64, w);
                    dst = dst.add(8);
                    i += 8;
                    continue;
                }
            }
            i += 1;
            if *NEEDS_ESCAPE.get_unchecked(c as usize) != 0 {
                write_escape(c as u8, &mut dst);
            } else {
                *dst = c as u8;
                dst = dst.add(1);
            }
        } else if c < 0x800 {
            i += 1;
            *dst = 0xC0 | (c >> 6) as u8;
            *dst.add(1) = 0x80 | (c & 0x3F) as u8;
            dst = dst.add(2);
        } else if c < 0x10000 {
            i += 1;
            if c & 0xF800 == 0xD800 {
                return None;
            }
            *dst = 0xE0 | (c >> 12) as u8;
            *dst.add(1) = 0x80 | ((c >> 6) & 0x3F) as u8;
            *dst.add(2) = 0x80 | (c & 0x3F) as u8;
            dst = dst.add(3);
        } else {
            i += 1;
            *dst = 0xF0 | (c >> 18) as u8;
            *dst.add(1) = 0x80 | ((c >> 12) & 0x3F) as u8;
            *dst.add(2) = 0x80 | ((c >> 6) & 0x3F) as u8;
            *dst.add(3) = 0x80 | (c & 0x3F) as u8;
            dst = dst.add(4);
        }
    }
    Some(dst)
}

/// UTF-8 view of a (non-ASCII) string, using CPython's cached copy when present.
#[inline(always)]
unsafe fn utf8_of(obj: *mut ffi::PyObject) -> Result<(*const u8, usize), SerError> {
    if crate::compat::PyUnicode_IS_COMPACT(obj) != 0 {
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

/// `rjson.JSONEncodeError`, created once (at module init, see `encode_error_type`).
static JSON_ENCODE_ERROR: pyo3::sync::PyOnceLock<Py<pyo3::types::PyType>> =
    pyo3::sync::PyOnceLock::new();

/// Returns `rjson.JSONEncodeError`, a subclass of both `TypeError` (what
/// `json.dumps`/`orjson.dumps` raise for unsupported values, so `except
/// TypeError` handlers keep working) and `ValueError` (what rjson raised
/// before, so existing `except ValueError` handlers keep working too), like
/// `orjson.JSONEncodeError`.
pub fn encode_error_type(py: Python<'_>) -> PyResult<&Py<pyo3::types::PyType>> {
    JSON_ENCODE_ERROR.get_or_try_init(py, || unsafe {
        let bases = ffi::PyTuple_Pack(2, ffi::PyExc_TypeError, ffi::PyExc_ValueError);
        if bases.is_null() {
            return Err(PyErr::fetch(py));
        }
        let ty = ffi::PyErr_NewExceptionWithDoc(
            c"rjson.JSONEncodeError".as_ptr(),
            c"Raised by dumps/dumps_str/dumps_bytes when an object cannot be serialized: \
unsupported type, non-str dict key, NaN/Infinity, or nesting too deep (circular \
reference). Subclasses both TypeError and ValueError."
                .as_ptr(),
            bases,
            ptr::null_mut(),
        );
        ffi::Py_DECREF(bases);
        // A NULL return has the exception set; `from_owned_ptr_or_err` fetches it.
        Ok(Bound::from_owned_ptr_or_err(py, ty)?
            .cast_into::<pyo3::types::PyType>()?
            .unbind())
    })
}

/// `type(obj)` as `module.QualName` (just `QualName` for builtins), for messages.
/// Last `datetime.timezone` seen and its offset in seconds. A `timezone` is
/// immutable and its offset does not depend on the datetime; the cache holds
/// a strong reference, so the address cannot be reused by another object.
static mut TZ_CACHE: (usize, i64) = (0, 0);

/// UTC offset of an aware datetime in seconds (None if `utcoffset()` is None),
/// or Err with a Python exception set. Runs Python code unless `tzt` is
/// `timezone` or the C `ZoneInfo`.
unsafe fn utc_offset(
    obj: *mut ffi::PyObject,
    tz: *mut ffi::PyObject,
    tzt: *mut ffi::PyTypeObject,
    t: &native::Types,
) -> Result<Option<i64>, ()> {
    let cache = ptr::addr_of_mut!(TZ_CACHE);
    if tzt == t.timezone && (*cache).0 == tz as usize {
        return Ok(Some((*cache).1));
    }
    ffi::Py_INCREF(obj);
    let off = if tzt == t.zoneinfo && !t.zoneinfo_utcoffset.is_null() {
        // ZoneInfo.utcoffset(tz, dt) through its method descriptor.
        let args = [tz, obj];
        ffi::PyObject_Vectorcall(t.zoneinfo_utcoffset, args.as_ptr(), 2, ptr::null_mut())
    } else {
        // `datetime.utcoffset()` calls `tzinfo.utcoffset(dt)` and checks the
        // result: None or a timedelta strictly within one day.
        ffi::PyObject_CallMethodNoArgs(obj, native::names().utcoffset)
    };
    ffi::Py_DECREF(obj);
    if off.is_null() {
        return Err(());
    }
    if off == ffi::Py_None() {
        ffi::Py_DECREF(off);
        return Ok(None);
    }
    if ffi::Py_TYPE(off) != t.timedelta {
        // Only possible from the direct ZoneInfo call (datetime.utcoffset
        // validates); the C ZoneInfo always returns a timedelta.
        ffi::Py_DECREF(off);
        ffi::PyErr_SetString(ffi::PyExc_TypeError, c"utcoffset() must return a timedelta".as_ptr());
        return Err(());
    }
    let secs = ffi::PyDateTime_DELTA_GET_DAYS(off) as i64 * 86400
        + ffi::PyDateTime_DELTA_GET_SECONDS(off) as i64;
    ffi::Py_DECREF(off);
    if !(-86399..=86399).contains(&secs) {
        ffi::PyErr_SetString(ffi::PyExc_ValueError, c"utcoffset() out of range".as_ptr());
        return Err(());
    }
    if tzt == t.timezone {
        let old = (*cache).0 as *mut ffi::PyObject;
        ffi::Py_INCREF(tz);
        *cache = (tz as usize, secs);
        if !old.is_null() {
            ffi::Py_DECREF(old);
        }
    }
    Ok(Some(secs))
}

/// Keyword options of `dumps`/`dumps_str` (parsed in entry.rs).
pub struct DumpsOpts {
    /// `default=` callable (borrowed from the call's arguments), or null.
    pub default: *mut ffi::PyObject,
    /// `passthrough=` flags (`native::PT_*`).
    pub passthrough: u32,
    /// `non_str_keys=`.
    pub non_str_keys: bool,
}

impl DumpsOpts {
    pub const NONE: DumpsOpts = DumpsOpts {
        default: ptr::null_mut(),
        passthrough: 0,
        non_str_keys: false,
    };
}

/// 128-bit value of a UUID's `int` (None with an exception set).
unsafe fn uuid_value(v: *mut ffi::PyObject) -> Option<u128> {
    if ffi::Py_TYPE(v) == int_type() && INLINE_INT {
        // Up to five 30-bit digits (the layout checked at init).
        let (neg, nd) = int_shape(v);
        let d = ptr::addr_of!((*(v as *const LongHeader)).ob_digit) as *const u32;
        // 4 digits hold 120 bits; a 5th may add 8 more (< 2**128).
        if !neg && (nd <= 4 || (nd == 5 && *d.add(4) < 256)) {
            let mut x: u128 = 0;
            for i in (0..nd).rev() {
                x = (x << 30) | *d.add(i) as u128;
            }
            return Some(x);
        }
    }
    if ffi::PyLong_Check(v) == 0 {
        ffi::PyErr_SetString(ffi::PyExc_TypeError, c"UUID.int is not an int".as_ptr());
        return None;
    }
    let lo = ffi::PyLong_AsUnsignedLongLongMask(v);
    if lo == u64::MAX && !ffi::PyErr_Occurred().is_null() {
        return None;
    }
    let h = ffi::PyNumber_Rshift(v, native::names().sixty_four);
    if h.is_null() {
        return None;
    }
    let hi = ffi::PyLong_AsUnsignedLongLongMask(h);
    ffi::Py_DECREF(h);
    if hi == u64::MAX && !ffi::PyErr_Occurred().is_null() {
        return None;
    }
    Some(((hi as u128) << 64) | lo as u128)
}

/// Reading an Enum member's `_value_` runs no Python code: generic attribute
/// lookup, and no class in the MRO defines `_value_` (which could be a
/// descriptor), so it comes from the instance dict.
unsafe fn enum_value_plain(ty: *mut ffi::PyTypeObject) -> bool {
    if !native::generic_getattr(ty) {
        return false;
    }
    let mro = (*ty).tp_mro;
    if mro.is_null() || ffi::PyTuple_Check(mro) == 0 {
        return false;
    }
    for i in 0..ffi::PyTuple_GET_SIZE(mro) {
        let base = ffi::PyTuple_GET_ITEM(mro, i) as *mut ffi::PyTypeObject;
        if !native::own_attr(base, native::names().value).is_null() {
            return false;
        }
    }
    true
}

/// `key` is a str starting with `_` (dataclass names orjson leaves out).
unsafe fn underscore_name(key: *mut ffi::PyObject) -> bool {
    ffi::PyUnicode_Check(key) != 0
        && ffi::PyUnicode_GetLength(key) > 0
        && ffi::PyUnicode_ReadChar(key, 0) == b'_' as u32
}

fn type_name(py: Python<'_>, obj: *mut ffi::PyObject) -> String {
    use pyo3::types::PyTypeMethods;
    if obj.is_null() {
        return "unknown".to_string();
    }
    // SAFETY: `obj` is a live borrowed pointer (see `Serializer::fail_obj`).
    let ty = unsafe { Bound::from_borrowed_ptr(py, obj) }.get_type();
    match ty.fully_qualified_name() {
        Ok(n) => n.to_string(),
        Err(_) => ty.name().map(|n| n.to_string()).unwrap_or_else(|_| "unknown".to_string()),
    }
}

#[cold]
#[inline(never)]
fn to_pyerr(py: Python<'_>, ser: &Serializer, e: SerError) -> PyErr {
    let msg = match e {
        SerError::NonFinite => format!(
            "Cannot serialize non-finite float: {} (JSON has no NaN or Infinity)",
            // Python's repr spelling (Rust would print `NaN`).
            if ser.err_float.is_nan() {
                "nan"
            } else if ser.err_float > 0.0 {
                "inf"
            } else {
                "-inf"
            }
        ),
        SerError::Unsupported => {
            format!("Type is not JSON serializable: {}", type_name(py, ser.err_obj))
        }
        SerError::KeyNotStr if ser.non_str_keys => format!(
            "Dictionary key of type {} is not supported (non_str_keys allows str, int, float, \
             bool, None, Enum, datetime, date, time and UUID keys)",
            type_name(py, ser.err_obj)
        ),
        SerError::KeyNotStr => format!(
            "Dictionary keys must be strings for JSON serialization, not {} \
             (non_str_keys=True converts int, float, bool and None keys)",
            type_name(py, ser.err_obj)
        ),
        SerError::Recursion => format!(
            "Maximum nesting depth ({}) exceeded during JSON serialization (circular reference?)",
            RECURSION_LIMIT
        ),
        SerError::TimeTz => "datetime.time must not have tzinfo set".to_string(),
        SerError::NeedGuard => "internal error: unguarded native value".to_string(),
        SerError::PyErrSet => return PyErr::fetch(py),
    };
    match encode_error_type(py) {
        Ok(ty) => PyErr::from_type(ty.bind(py).clone(), msg),
        // Only if creating the type failed (e.g. MemoryError): still a ValueError.
        Err(_) => pyo3::exceptions::PyValueError::new_err(msg),
    }
}

/// Serializes `obj` (borrowed) and returns a new reference to a `str`
/// (`as_str`) or `bytes`. Entry point for the raw `dumps`/`dumps_str` wrappers;
/// `opts` are the keyword options.
///
/// # Safety
/// `obj` must be a valid object pointer and the GIL must be held.
pub unsafe fn dumps_raw(
    py: Python<'_>,
    obj: *mut ffi::PyObject,
    as_str: bool,
    opts: &DumpsOpts,
) -> PyResult<*mut ffi::PyObject> {
    let mut ser = Serializer::new(as_str, opts, false);
    let start = ser.start();
    let mut end = ser.ser(start, obj, 0, 0);
    if end.is_null() && matches!(ser.err, SerError::NeedGuard) {
        // A native value needs Python code (dataclass, Python tzinfo, ...).
        // Nothing ran yet, so start over in guarded mode.
        ser = Serializer::new(as_str, opts, true);
        let start = ser.start();
        end = ser.ser(start, obj, 0, 0);
    }
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
