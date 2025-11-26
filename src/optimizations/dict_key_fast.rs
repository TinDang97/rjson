//! Phase 29: Zero-Copy Dict Key Serialization
//!
//! Most JSON dict keys are short ASCII strings that don't need escaping.
//! This module provides optimized paths for these common cases.
//!
//! # Performance
//! - Standard path: PyUnicode_AsUTF8AndSize + escape check + write
//! - Fast path: Direct ASCII buffer access + inline escape check
//!
//! # Optimization Strategy
//! 1. Check if string is pure ASCII (using PyASCIIObject state flag)
//! 2. For short ASCII keys (<=32 bytes), inline escape check
//! 3. Copy directly to buffer if no escapes needed
//! 4. Fall back to full escape handling otherwise

use pyo3::ffi;

/// PyASCIIObject structure for direct ASCII access
/// CPython uses this for strings that are pure ASCII
#[repr(C)]
struct PyASCIIObject {
    _ob_refcnt: isize,
    _ob_type: *mut ffi::PyTypeObject,
    length: isize,
    _hash: isize,
    state: u32,
}

/// Bit flag indicating string is ASCII-only
const STATE_ASCII_MASK: u32 = 0b0100_0000;  // Bit 6

/// Offset to string data in PyASCIIObject (after the header)
#[cfg(target_pointer_width = "64")]
const ASCII_DATA_OFFSET: usize = 40;  // PyASCIIObject header size on 64-bit

#[cfg(target_pointer_width = "32")]
const ASCII_DATA_OFFSET: usize = 24;

/// Maximum key length for inline processing
const MAX_INLINE_KEY_LEN: usize = 64;

/// Write a dict key with fast ASCII path
///
/// This is optimized for the common case of short ASCII keys without escapes.
///
/// # Safety
/// - key_ptr must be a valid PyUnicodeObject
///
/// # Returns
/// true if fast path was used, false if caller should use standard path
#[inline(always)]
pub unsafe fn write_dict_key_fast(buf: &mut Vec<u8>, key_ptr: *mut ffi::PyObject) -> bool {
    let ascii_obj = key_ptr as *const PyASCIIObject;
    let state = (*ascii_obj).state;

    // Check if string is ASCII
    if state & STATE_ASCII_MASK == 0 {
        return false;  // Not ASCII, use standard path
    }

    let length = (*ascii_obj).length as usize;

    // Only use fast path for short keys
    if length > MAX_INLINE_KEY_LEN {
        return false;
    }

    // Get direct pointer to ASCII data
    let data_ptr = (key_ptr as *const u8).add(ASCII_DATA_OFFSET);
    let data = std::slice::from_raw_parts(data_ptr, length);

    // Inline escape check for short strings
    // Most dict keys don't need escaping (they're identifiers)
    if !needs_escape_inline(data) {
        // Fast path: no escapes needed, direct copy
        buf.reserve(length + 2);
        buf.push(b'"');
        buf.extend_from_slice(data);
        buf.push(b'"');
        return true;
    }

    // Has escapes - fall back to standard path
    false
}

/// Fast inline escape check for short strings
///
/// Checks if any bytes need JSON escaping:
/// - Quote (")
/// - Backslash (\)
/// - Control characters (< 0x20)
#[inline(always)]
fn needs_escape_inline(data: &[u8]) -> bool {
    // Process 8 bytes at a time using u64
    let mut i = 0;
    let len = data.len();

    // Fast path: check 8 bytes at a time
    while i + 8 <= len {
        let chunk = unsafe {
            (data.as_ptr().add(i) as *const u64).read_unaligned()
        };

        // Check for control characters (any byte < 0x20)
        // Using the "has zero byte" trick: subtract 0x20 from each byte,
        // then check if any became "negative" (high bit set)
        let ctrl_check = chunk.wrapping_sub(0x2020_2020_2020_2020);
        let has_ctrl = (ctrl_check & 0x8080_8080_8080_8080) != 0
            && (chunk & 0x8080_8080_8080_8080) == 0;

        // Check for quote (0x22) and backslash (0x5C)
        // XOR with repeated pattern, then check for zero bytes
        let quote_check = chunk ^ 0x2222_2222_2222_2222;
        let backslash_check = chunk ^ 0x5C5C_5C5C_5C5C_5C5C;

        let has_quote = has_zero_byte(quote_check);
        let has_backslash = has_zero_byte(backslash_check);

        if has_ctrl || has_quote || has_backslash {
            return true;
        }

        i += 8;
    }

    // Check remaining bytes
    while i < len {
        let b = data[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            return true;
        }
        i += 1;
    }

    false
}

/// Check if a u64 contains any zero byte
/// Uses the classic "has zero byte" bit trick
#[inline(always)]
fn has_zero_byte(x: u64) -> bool {
    // This magic constant finds zero bytes in a u64
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;

    // If any byte is zero, this will have a high bit set in that byte position
    (x.wrapping_sub(LO) & !x & HI) != 0
}

/// Extract string data from PyUnicodeObject with ASCII fast path
///
/// Returns (data_ptr, length) for the string's UTF-8 representation.
///
/// # Safety
/// - str_ptr must be a valid PyUnicodeObject
#[inline(always)]
pub unsafe fn extract_string_data(str_ptr: *mut ffi::PyObject) -> (*const u8, usize) {
    let ascii_obj = str_ptr as *const PyASCIIObject;
    let state = (*ascii_obj).state;

    if state & STATE_ASCII_MASK != 0 {
        // Fast path: ASCII string
        let length = (*ascii_obj).length as usize;
        let data_ptr = (str_ptr as *const u8).add(ASCII_DATA_OFFSET);
        (data_ptr, length)
    } else {
        // Slow path: non-ASCII, use C API
        let mut size: ffi::Py_ssize_t = 0;
        let data_ptr = ffi::PyUnicode_AsUTF8AndSize(str_ptr, &mut size);
        (data_ptr as *const u8, size as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::prelude::*;
    use pyo3::types::PyString;

    #[test]
    fn test_needs_escape_inline() {
        // No escapes
        assert!(!needs_escape_inline(b"hello"));
        assert!(!needs_escape_inline(b"key_name"));
        assert!(!needs_escape_inline(b"CamelCaseKey"));
        assert!(!needs_escape_inline(b"key123"));

        // Has escapes
        assert!(needs_escape_inline(b"has\"quote"));
        assert!(needs_escape_inline(b"has\\backslash"));
        assert!(needs_escape_inline(b"has\nnewline"));
        assert!(needs_escape_inline(b"has\ttab"));
        assert!(needs_escape_inline(b"\x00null"));
    }

    #[test]
    fn test_write_dict_key_fast() {
        Python::with_gil(|py| {
            let mut buf = Vec::new();

            // Test simple ASCII key
            let key = PyString::new(py, "name");
            let success = unsafe { write_dict_key_fast(&mut buf, key.as_ptr()) };
            assert!(success);
            assert_eq!(String::from_utf8(buf.clone()).unwrap(), "\"name\"");

            // Test key with escape - should return false
            buf.clear();
            let key_escape = PyString::new(py, "has\"quote");
            let success = unsafe { write_dict_key_fast(&mut buf, key_escape.as_ptr()) };
            assert!(!success);

            // Test non-ASCII key - should return false
            buf.clear();
            let key_unicode = PyString::new(py, "日本語");
            let success = unsafe { write_dict_key_fast(&mut buf, key_unicode.as_ptr()) };
            assert!(!success);
        });
    }
}
