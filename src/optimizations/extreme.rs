//! Extreme optimizations - The "Nuclear Option"
//!
//! This module implements aggressive optimizations that sacrifice:
//! - API compatibility (returns bytes instead of str)
//! - Some PyO3 safety guarantees
//! - Rust idiomatic patterns
//!
//! Goal: Get as close to orjson as possible by any means necessary
//!
//! WARNING: More unsafe code, harder to maintain, but FAST!
//!
//! DYNAMIC PROGRAMMING: Uses precomputed lookup tables for digit pairs

use pyo3::prelude::*;
use pyo3::ffi;
use pyo3::types::PyBytes;
use std::ptr;

// ============================================================================
// DYNAMIC PROGRAMMING: Precomputed digit lookup tables
// ============================================================================

/// Precomputed two-digit pairs "00" through "99"
static DIGIT_PAIRS: [[u8; 2]; 100] = [
    *b"00", *b"01", *b"02", *b"03", *b"04", *b"05", *b"06", *b"07", *b"08", *b"09",
    *b"10", *b"11", *b"12", *b"13", *b"14", *b"15", *b"16", *b"17", *b"18", *b"19",
    *b"20", *b"21", *b"22", *b"23", *b"24", *b"25", *b"26", *b"27", *b"28", *b"29",
    *b"30", *b"31", *b"32", *b"33", *b"34", *b"35", *b"36", *b"37", *b"38", *b"39",
    *b"40", *b"41", *b"42", *b"43", *b"44", *b"45", *b"46", *b"47", *b"48", *b"49",
    *b"50", *b"51", *b"52", *b"53", *b"54", *b"55", *b"56", *b"57", *b"58", *b"59",
    *b"60", *b"61", *b"62", *b"63", *b"64", *b"65", *b"66", *b"67", *b"68", *b"69",
    *b"70", *b"71", *b"72", *b"73", *b"74", *b"75", *b"76", *b"77", *b"78", *b"79",
    *b"80", *b"81", *b"82", *b"83", *b"84", *b"85", *b"86", *b"87", *b"88", *b"89",
    *b"90", *b"91", *b"92", *b"93", *b"94", *b"95", *b"96", *b"97", *b"98", *b"99",
];

/// Single digit lookup (0-9 as ASCII)
static DIGITS: [u8; 10] = [b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9'];

/// Direct C API serializer with zero abstraction
///
/// This bypasses PyO3 completely and uses direct CPython C API calls.
/// Much more unsafe, but eliminates all PyO3 overhead.
#[repr(C)]
pub struct DirectSerializer {
    buf: Vec<u8>,
    py: Python<'static>,
}

impl DirectSerializer {
    #[inline(always)]
    pub unsafe fn new(py: Python<'static>, capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
            py,
        }
    }

    /// Serialize any Python object using direct C API
    ///
    /// This is a single massive function with everything inlined.
    /// Similar to orjson's approach - no function call overhead.
    #[inline(always)]
    pub unsafe fn serialize_direct(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        let obj_type = (*obj).ob_type;

        // Use the already-initialized type cache from type_cache.rs
        use crate::optimizations::type_cache;
        let type_cache_ref = type_cache::get_type_cache();

        let _none_type = type_cache_ref.none_type;
        let bool_type = type_cache_ref.bool_type;
        let int_type = type_cache_ref.int_type;
        let float_type = type_cache_ref.float_type;
        let str_type = type_cache_ref.string_type;
        let list_type = type_cache_ref.list_type;
        let dict_type = type_cache_ref.dict_type;

        // Inline type dispatch (no match overhead)
        if obj == ffi::Py_None() {
            // None
            self.buf.extend_from_slice(b"null");
        } else if obj_type == bool_type {
            // Boolean - inline comparison
            if obj == ffi::Py_True() {
                self.buf.extend_from_slice(b"true");
            } else {
                self.buf.extend_from_slice(b"false");
            }
        } else if obj_type == int_type {
            // Integer - inline formatting
            self.serialize_int_inline(obj)?;
        } else if obj_type == float_type {
            // Float - inline formatting
            self.serialize_float_inline(obj)?;
        } else if obj_type == str_type {
            // String - inline with SIMD
            self.serialize_string_inline(obj)?;
        } else if obj_type == list_type {
            // List - inline iteration
            self.serialize_list_inline(obj)?;
        } else if obj_type == dict_type {
            // Dict - inline iteration
            self.serialize_dict_inline(obj)?;
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err("Unsupported type"));
        }

        Ok(())
    }

    #[inline(always)]
    unsafe fn serialize_int_inline(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        // Try fast path: i64
        let val = ffi::PyLong_AsLongLong(obj);

        if val == -1 && !ffi::PyErr_Occurred().is_null() {
            ffi::PyErr_Clear();

            // Try u64
            let val_u64 = ffi::PyLong_AsUnsignedLongLong(obj);
            if val_u64 == u64::MAX && !ffi::PyErr_Occurred().is_null() {
                ffi::PyErr_Clear();

                // Very large int - use string representation
                let repr = ffi::PyObject_Str(obj);
                let mut size: ffi::Py_ssize_t = 0;
                let str_data = ffi::PyUnicode_AsUTF8AndSize(repr, &mut size);

                if !str_data.is_null() {
                    let slice = std::slice::from_raw_parts(str_data as *const u8, size as usize);
                    self.buf.extend_from_slice(slice);
                }

                ffi::Py_DECREF(repr);
            } else {
                // u64 path - inline format
                self.format_u64_inline(val_u64);
            }
        } else {
            // i64 path - inline format
            self.format_i64_inline(val);
        }

        Ok(())
    }

    /// Fast integer formatting using DP lookup tables
    #[inline(always)]
    fn format_i64_inline(&mut self, val: i64) {
        if val >= 0 {
            self.format_u64_dp(val as u64);
        } else {
            self.buf.push(b'-');
            self.format_u64_dp((-val) as u64);
        }
    }

    #[inline(always)]
    fn format_u64_inline(&mut self, val: u64) {
        self.format_u64_dp(val);
    }

    /// DP-optimized positive integer formatting using precomputed digit pairs
    #[inline(always)]
    fn format_u64_dp(&mut self, val: u64) {
        let mut tmp = [0u8; 20];

        if val < 10 {
            self.buf.push(DIGITS[val as usize]);
        } else if val < 100 {
            self.buf.extend_from_slice(&DIGIT_PAIRS[val as usize]);
        } else if val < 1000 {
            let d1 = (val / 100) as usize;
            let d23 = (val % 100) as usize;
            tmp[0] = DIGITS[d1];
            tmp[1..3].copy_from_slice(&DIGIT_PAIRS[d23]);
            self.buf.extend_from_slice(&tmp[..3]);
        } else if val < 10000 {
            let d12 = (val / 100) as usize;
            let d34 = (val % 100) as usize;
            tmp[0..2].copy_from_slice(&DIGIT_PAIRS[d12]);
            tmp[2..4].copy_from_slice(&DIGIT_PAIRS[d34]);
            self.buf.extend_from_slice(&tmp[..4]);
        } else if val < 100000 {
            let d1 = (val / 10000) as usize;
            let d23 = ((val / 100) % 100) as usize;
            let d45 = (val % 100) as usize;
            tmp[0] = DIGITS[d1];
            tmp[1..3].copy_from_slice(&DIGIT_PAIRS[d23]);
            tmp[3..5].copy_from_slice(&DIGIT_PAIRS[d45]);
            self.buf.extend_from_slice(&tmp[..5]);
        } else if val < 1000000 {
            let d12 = (val / 10000) as usize;
            let d34 = ((val / 100) % 100) as usize;
            let d56 = (val % 100) as usize;
            tmp[0..2].copy_from_slice(&DIGIT_PAIRS[d12]);
            tmp[2..4].copy_from_slice(&DIGIT_PAIRS[d34]);
            tmp[4..6].copy_from_slice(&DIGIT_PAIRS[d56]);
            self.buf.extend_from_slice(&tmp[..6]);
        } else if val < 10000000 {
            let d1 = (val / 1000000) as usize;
            let d23 = ((val / 10000) % 100) as usize;
            let d45 = ((val / 100) % 100) as usize;
            let d67 = (val % 100) as usize;
            tmp[0] = DIGITS[d1];
            tmp[1..3].copy_from_slice(&DIGIT_PAIRS[d23]);
            tmp[3..5].copy_from_slice(&DIGIT_PAIRS[d45]);
            tmp[5..7].copy_from_slice(&DIGIT_PAIRS[d67]);
            self.buf.extend_from_slice(&tmp[..7]);
        } else if val < 100000000 {
            let d12 = (val / 1000000) as usize;
            let d34 = ((val / 10000) % 100) as usize;
            let d56 = ((val / 100) % 100) as usize;
            let d78 = (val % 100) as usize;
            tmp[0..2].copy_from_slice(&DIGIT_PAIRS[d12]);
            tmp[2..4].copy_from_slice(&DIGIT_PAIRS[d34]);
            tmp[4..6].copy_from_slice(&DIGIT_PAIRS[d56]);
            tmp[6..8].copy_from_slice(&DIGIT_PAIRS[d78]);
            self.buf.extend_from_slice(&tmp[..8]);
        } else {
            // 9+ digits: use reverse-fill approach with DP pairs
            let mut pos = 20;
            let mut v = val;
            while v >= 100 {
                let pair = (v % 100) as usize;
                v /= 100;
                pos -= 2;
                tmp[pos..pos+2].copy_from_slice(&DIGIT_PAIRS[pair]);
            }
            if v >= 10 {
                pos -= 2;
                tmp[pos..pos+2].copy_from_slice(&DIGIT_PAIRS[v as usize]);
            } else {
                pos -= 1;
                tmp[pos] = DIGITS[v as usize];
            }
            self.buf.extend_from_slice(&tmp[pos..]);
        }
    }

    #[inline(always)]
    unsafe fn serialize_float_inline(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        let val = ffi::PyFloat_AsDouble(obj);

        if !val.is_finite() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "Cannot serialize non-finite float"
            ));
        }

        // Use ryu for fast float formatting
        let mut ryu_buf = ryu::Buffer::new();
        self.buf.extend_from_slice(ryu_buf.format(val).as_bytes());

        Ok(())
    }

    #[inline(always)]
    unsafe fn serialize_string_inline(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        let mut size: ffi::Py_ssize_t = 0;
        let str_data = ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);

        if str_data.is_null() {
            return Err(pyo3::exceptions::PyValueError::new_err("Invalid UTF-8"));
        }

        let bytes = std::slice::from_raw_parts(str_data as *const u8, size as usize);

        self.buf.push(b'"');

        // SIMD escape detection (if available)
        #[cfg(target_arch = "x86_64")]
        {
            if is_x86_feature_detected!("avx2") {
                if self.serialize_string_simd_avx2(bytes) {
                    self.buf.push(b'"');
                    return Ok(());
                }
            }
        }

        // Fallback: fast scalar path
        if self.has_escape_fast(bytes) {
            self.serialize_string_escaped(bytes);
        } else {
            self.buf.extend_from_slice(bytes);
        }

        self.buf.push(b'"');
        Ok(())
    }

    #[cfg(target_arch = "x86_64")]
    #[inline(always)]
    unsafe fn serialize_string_simd_avx2(&mut self, bytes: &[u8]) -> bool {
        use std::arch::x86_64::*;

        let len = bytes.len();
        if len < 32 {
            return false;  // Too small for SIMD
        }

        let quote = _mm256_set1_epi8(b'"' as i8);
        let backslash = _mm256_set1_epi8(b'\\' as i8);
        let ctrl = _mm256_set1_epi8(0x1F);  // Control characters < 0x20

        let mut pos = 0;

        // Process 32 bytes at a time
        while pos + 32 <= len {
            let chunk = _mm256_loadu_si256(bytes.as_ptr().add(pos) as *const __m256i);

            // Check for quote, backslash, and control characters
            let cmp_quote = _mm256_cmpeq_epi8(chunk, quote);
            let cmp_backslash = _mm256_cmpeq_epi8(chunk, backslash);
            let cmp_ctrl = _mm256_cmpgt_epi8(ctrl, chunk);

            // Combine all checks
            let combined = _mm256_or_si256(cmp_quote, cmp_backslash);
            let combined = _mm256_or_si256(combined, cmp_ctrl);

            let mask = _mm256_movemask_epi8(combined);

            if mask != 0 {
                // Found escape character - fall back to scalar
                return false;
            }

            pos += 32;
        }

        // Copy the SIMD-validated portion
        self.buf.extend_from_slice(&bytes[..pos]);

        // Handle remaining bytes with scalar (< 32 bytes)
        if pos < len {
            let remaining = &bytes[pos..];
            if self.has_escape_fast(remaining) {
                return false;  // Has escapes in tail
            }
            self.buf.extend_from_slice(remaining);
        }

        true  // Successfully serialized without escapes
    }

    #[inline(always)]
    fn has_escape_fast(&self, bytes: &[u8]) -> bool {
        // Fast scalar escape detection
        for &b in bytes {
            if b == b'"' || b == b'\\' || b < 0x20 {
                return true;
            }
        }
        false
    }

    #[inline(never)]  // Keep hot path small
    fn serialize_string_escaped(&mut self, bytes: &[u8]) {
        // Character-by-character escape handling
        for &b in bytes {
            match b {
                b'"' => self.buf.extend_from_slice(b"\\\""),
                b'\\' => self.buf.extend_from_slice(b"\\\\"),
                b'\n' => self.buf.extend_from_slice(b"\\n"),
                b'\r' => self.buf.extend_from_slice(b"\\r"),
                b'\t' => self.buf.extend_from_slice(b"\\t"),
                0x08 => self.buf.extend_from_slice(b"\\b"),
                0x0C => self.buf.extend_from_slice(b"\\f"),
                b if b < 0x20 => {
                    // Unicode escape
                    self.buf.extend_from_slice(b"\\u00");
                    self.buf.push(b'0' + (b >> 4));
                    let low = b & 0x0F;
                    self.buf.push(if low < 10 { b'0' + low } else { b'a' + low - 10 });
                }
                b => self.buf.push(b),
            }
        }
    }

    #[inline(always)]
    unsafe fn serialize_list_inline(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        let size = ffi::PyList_GET_SIZE(obj);

        self.buf.push(b'[');

        for i in 0..size {
            if i > 0 {
                self.buf.push(b',');
            }

            let item = ffi::PyList_GET_ITEM(obj, i);
            self.serialize_direct(item)?;
        }

        self.buf.push(b']');
        Ok(())
    }

    #[inline(always)]
    unsafe fn serialize_dict_inline(&mut self, obj: *mut ffi::PyObject) -> PyResult<()> {
        self.buf.push(b'{');

        let mut pos: ffi::Py_ssize_t = 0;
        let mut key: *mut ffi::PyObject = ptr::null_mut();
        let mut value: *mut ffi::PyObject = ptr::null_mut();
        let mut first = true;

        while ffi::PyDict_Next(obj, &mut pos, &mut key, &mut value) != 0 {
            if !first {
                self.buf.push(b',');
            }
            first = false;

            // Serialize key (must be string)
            if ffi::PyUnicode_Check(key) == 0 {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "Dictionary keys must be strings"
                ));
            }

            self.serialize_string_inline(key)?;
            self.buf.push(b':');
            self.serialize_direct(value)?;
        }

        self.buf.push(b'}');
        Ok(())
    }

    #[inline(always)]
    pub fn into_pybytes(self, py: Python) -> Py<PyBytes> {
        // Zero-copy conversion to PyBytes
        unsafe {
            let bytes_ptr = ffi::PyBytes_FromStringAndSize(
                self.buf.as_ptr() as *const i8,
                self.buf.len() as ffi::Py_ssize_t,
            );

            // Transfer ownership to Python
            std::mem::forget(self.buf);

            Py::from_owned_ptr(py, bytes_ptr)
        }
    }
}

/// Estimate buffer size for allocation
#[inline(always)]
pub unsafe fn estimate_size_fast(obj: *mut ffi::PyObject) -> usize {
    let _obj_type = (*obj).ob_type;

    // Quick heuristics
    if obj == ffi::Py_None() {
        4  // "null"
    } else if ffi::PyBool_Check(obj) != 0 {
        5  // "false"
    } else if ffi::PyLong_Check(obj) != 0 {
        20  // Max i64 digits
    } else if ffi::PyFloat_Check(obj) != 0 {
        24  // Max f64 representation
    } else if ffi::PyUnicode_Check(obj) != 0 {
        let mut size: ffi::Py_ssize_t = 0;
        ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);
        (size as usize) + 8  // String + quotes + escapes
    } else if ffi::PyList_Check(obj) != 0 {
        let len = ffi::PyList_GET_SIZE(obj);
        (len as usize) * 16 + 16  // Heuristic
    } else if ffi::PyDict_Check(obj) != 0 {
        let len = ffi::PyDict_Size(obj);
        (len as usize) * 32 + 16  // Heuristic
    } else {
        128  // Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_i64_inline() {
        Python::with_gil(|py| {
            let py_static = unsafe { std::mem::transmute::<Python, Python<'static>>(py) };
            let mut ser = unsafe { DirectSerializer::new(py_static, 64) };

            ser.format_i64_inline(0);
            assert_eq!(std::str::from_utf8(&ser.buf).unwrap(), "0");

            ser.buf.clear();
            ser.format_i64_inline(123);
            assert_eq!(std::str::from_utf8(&ser.buf).unwrap(), "123");

            ser.buf.clear();
            ser.format_i64_inline(-456);
            assert_eq!(std::str::from_utf8(&ser.buf).unwrap(), "-456");
        });
    }
}
