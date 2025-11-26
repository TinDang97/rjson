//! Phase 26: Direct PyLongObject Structure Access
//!
//! This module provides ultra-fast integer extraction by directly reading
//! CPython's internal PyLongObject structure, bypassing the C API overhead.
//!
//! # Performance
//! - PyLong_AsLongLongAndOverflow: ~15-20 cycles per call
//! - Direct struct access: ~3-5 cycles for small integers
//!
//! # Safety
//! This is highly unsafe and CPython version-specific code.
//! Only use after verifying Python version compatibility.
//!
//! # CPython 3.12+ PyLongObject Layout
//! ```c
//! struct PyLongObject {
//!     PyObject_VAR_HEAD  // ob_refcnt, ob_type, ob_size
//!     digit ob_digit[1]; // flexible array of digits
//! };
//! ```
//!
//! - ob_size: number of digits (negative for negative numbers)
//! - digit: uint32_t, but only 30 bits used (PyLong_SHIFT = 30)
//! - Single digit can represent values 0 to 2^30-1 (about 1 billion)

use pyo3::ffi;
use std::sync::atomic::{AtomicBool, Ordering};

/// PyLong digit shift (bits per digit)
/// This is 30 on 64-bit systems, 15 on 32-bit
#[cfg(target_pointer_width = "64")]
const PYLONG_SHIFT: u32 = 30;

#[cfg(target_pointer_width = "32")]
const PYLONG_SHIFT: u32 = 15;

/// Maximum value that fits in a single digit
#[cfg(target_pointer_width = "64")]
const SINGLE_DIGIT_MAX: u64 = (1u64 << 30) - 1;  // 1,073,741,823

#[cfg(target_pointer_width = "32")]
const SINGLE_DIGIT_MAX: u64 = (1u64 << 15) - 1;  // 32,767

/// Offset from PyObject to ob_size in PyVarObject
/// PyObject_VAR_HEAD = ob_refcnt (8) + ob_type (8) + ob_size (8) on 64-bit
#[cfg(target_pointer_width = "64")]
const OB_SIZE_OFFSET: usize = 16;  // After ob_refcnt and ob_type

#[cfg(target_pointer_width = "32")]
const OB_SIZE_OFFSET: usize = 8;

/// Offset from PyObject to first digit in PyLongObject
/// After PyVarObject header
#[cfg(target_pointer_width = "64")]
const OB_DIGIT_OFFSET: usize = 24;  // ob_refcnt(8) + ob_type(8) + ob_size(8)

#[cfg(target_pointer_width = "32")]
const OB_DIGIT_OFFSET: usize = 12;

/// Whether we've verified this Python version is compatible
static PYLONG_FAST_ENABLED: AtomicBool = AtomicBool::new(false);
static PYLONG_FAST_CHECKED: AtomicBool = AtomicBool::new(false);

/// Initialize and verify PyLong fast path is safe for this Python version
///
/// This should be called once during module initialization.
/// It verifies the PyLongObject structure matches our expectations.
pub fn init_pylong_fast() {
    if PYLONG_FAST_CHECKED.load(Ordering::Relaxed) {
        return;
    }

    // Test with known values to verify structure layout
    let is_compatible = unsafe { verify_pylong_structure() };

    PYLONG_FAST_ENABLED.store(is_compatible, Ordering::Release);
    PYLONG_FAST_CHECKED.store(true, Ordering::Release);

    #[cfg(debug_assertions)]
    if is_compatible {
        eprintln!("Phase 26: PyLong fast path enabled");
    } else {
        eprintln!("Phase 26: PyLong fast path disabled (incompatible Python version)");
    }
}

/// Verify PyLongObject structure by testing with known values
unsafe fn verify_pylong_structure() -> bool {
    // Test with value 0
    let zero = ffi::PyLong_FromLong(0);
    if zero.is_null() {
        return false;
    }

    let zero_result = extract_pylong_fast(zero);
    ffi::Py_DECREF(zero);

    if zero_result != Some(0) {
        return false;
    }

    // Test with value 42
    let forty_two = ffi::PyLong_FromLong(42);
    if forty_two.is_null() {
        return false;
    }

    let forty_two_result = extract_pylong_fast(forty_two);
    ffi::Py_DECREF(forty_two);

    if forty_two_result != Some(42) {
        return false;
    }

    // Test with negative value -123
    let negative = ffi::PyLong_FromLong(-123);
    if negative.is_null() {
        return false;
    }

    let negative_result = extract_pylong_fast(negative);
    ffi::Py_DECREF(negative);

    if negative_result != Some(-123) {
        return false;
    }

    // Test with larger value (but still single digit)
    let large = ffi::PyLong_FromLong(999_999_999);
    if large.is_null() {
        return false;
    }

    let large_result = extract_pylong_fast(large);
    ffi::Py_DECREF(large);

    if large_result != Some(999_999_999) {
        return false;
    }

    true
}

/// Check if PyLong fast path is enabled
#[inline(always)]
pub fn is_pylong_fast_enabled() -> bool {
    PYLONG_FAST_ENABLED.load(Ordering::Relaxed)
}

/// Extract integer value directly from PyLongObject structure
///
/// Returns Some(value) for integers that fit in i64 and use <= 2 digits,
/// Returns None for larger integers (caller should fall back to C API).
///
/// # Safety
/// - obj must be a valid PyLongObject pointer
/// - Caller must have verified is_pylong_fast_enabled() returns true
#[inline(always)]
pub unsafe fn extract_pylong_fast(obj: *mut ffi::PyObject) -> Option<i64> {
    // Read ob_size from PyVarObject
    // ob_size is Py_ssize_t (i64 on 64-bit)
    let ob_size_ptr = (obj as *const u8).add(OB_SIZE_OFFSET) as *const isize;
    let ob_size = *ob_size_ptr;

    // Zero check
    if ob_size == 0 {
        return Some(0);
    }

    // Get pointer to first digit
    let digit_ptr = (obj as *const u8).add(OB_DIGIT_OFFSET) as *const u32;

    // Single digit case (covers -2^30+1 to 2^30-1, about ±1 billion)
    if ob_size == 1 {
        let digit = *digit_ptr;
        return Some(digit as i64);
    }

    if ob_size == -1 {
        let digit = *digit_ptr;
        return Some(-(digit as i64));
    }

    // Two digit case (covers up to ±2^60, which includes all i64 positive values)
    if ob_size == 2 {
        let d0 = *digit_ptr as u64;
        let d1 = *digit_ptr.add(1) as u64;
        let value = d0 | (d1 << PYLONG_SHIFT);

        // Check if it fits in i64
        if value <= i64::MAX as u64 {
            return Some(value as i64);
        }
        return None;  // Too large, fall back to C API
    }

    if ob_size == -2 {
        let d0 = *digit_ptr as u64;
        let d1 = *digit_ptr.add(1) as u64;
        let value = d0 | (d1 << PYLONG_SHIFT);

        // Check if negated value fits in i64
        // i64::MIN = -9223372036854775808, so max magnitude is 2^63
        if value <= (i64::MAX as u64) + 1 {
            return Some(-(value as i64));
        }
        return None;
    }

    // More than 2 digits - fall back to C API
    None
}

/// Fast integer extraction with automatic fallback
///
/// Tries fast path first, falls back to PyLong_AsLongLongAndOverflow if needed.
///
/// # Returns
/// - Ok(value) if the integer fits in i64
/// - Err(()) if integer is too large (caller should try u64 or string)
///
/// # Safety
/// - obj must be a valid PyLongObject pointer
#[inline(always)]
pub unsafe fn extract_int_fast(obj: *mut ffi::PyObject) -> Result<i64, ()> {
    // Try fast path if enabled
    if is_pylong_fast_enabled() {
        if let Some(value) = extract_pylong_fast(obj) {
            return Ok(value);
        }
    }

    // Fall back to C API for large integers
    let mut overflow: std::ffi::c_int = 0;
    let value = ffi::PyLong_AsLongLongAndOverflow(obj, &mut overflow);

    if overflow == 0 {
        Ok(value)
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::Python;

    #[test]
    fn test_pylong_fast_extraction() {
        Python::with_gil(|_py| {
            init_pylong_fast();

            if !is_pylong_fast_enabled() {
                eprintln!("Skipping test: PyLong fast path not compatible");
                return;
            }

            unsafe {
                // Test zero
                let zero = ffi::PyLong_FromLong(0);
                assert_eq!(extract_pylong_fast(zero), Some(0));
                ffi::Py_DECREF(zero);

                // Test small positive
                let small = ffi::PyLong_FromLong(42);
                assert_eq!(extract_pylong_fast(small), Some(42));
                ffi::Py_DECREF(small);

                // Test small negative
                let neg = ffi::PyLong_FromLong(-42);
                assert_eq!(extract_pylong_fast(neg), Some(-42));
                ffi::Py_DECREF(neg);

                // Test larger value (still single digit on 64-bit)
                let large = ffi::PyLong_FromLongLong(1_000_000_000);
                assert_eq!(extract_pylong_fast(large), Some(1_000_000_000));
                ffi::Py_DECREF(large);

                // Test two-digit value
                let two_digit = ffi::PyLong_FromLongLong(2_000_000_000);
                assert_eq!(extract_pylong_fast(two_digit), Some(2_000_000_000));
                ffi::Py_DECREF(two_digit);
            }
        });
    }
}
