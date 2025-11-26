//! Phase 30: Direct PyFloatObject Structure Access
//!
//! This module provides ultra-fast float extraction by directly reading
//! CPython's internal PyFloatObject structure, bypassing the C API overhead.
//!
//! # Performance
//! - PyFloat_AsDouble: ~8-10 cycles per call (function call + memory access)
//! - Direct struct access: ~2-3 cycles (single memory load)
//!
//! # Safety
//! This is unsafe and CPython version-specific code.
//! Only use after verifying Python version compatibility.
//!
//! # CPython PyFloatObject Layout
//! ```c
//! typedef struct {
//!     PyObject_HEAD  // ob_refcnt (8) + ob_type (8) = 16 bytes on 64-bit
//!     double ob_fval;
//! } PyFloatObject;
//! ```

use pyo3::ffi;
use std::sync::atomic::{AtomicBool, Ordering};

/// Offset from PyObject to ob_fval in PyFloatObject
/// PyObject_HEAD = ob_refcnt (8) + ob_type (8) = 16 on 64-bit
#[cfg(target_pointer_width = "64")]
const OB_FVAL_OFFSET: usize = 16;

#[cfg(target_pointer_width = "32")]
const OB_FVAL_OFFSET: usize = 8;

/// Whether we've verified this Python version is compatible
static PYFLOAT_FAST_ENABLED: AtomicBool = AtomicBool::new(false);
static PYFLOAT_FAST_CHECKED: AtomicBool = AtomicBool::new(false);

/// Initialize and verify PyFloat fast path is safe for this Python version
///
/// This should be called once during module initialization.
/// It verifies the PyFloatObject structure matches our expectations.
pub fn init_pyfloat_fast() {
    if PYFLOAT_FAST_CHECKED.load(Ordering::Relaxed) {
        return;
    }

    // Test with known values to verify structure layout
    let is_compatible = unsafe { verify_pyfloat_structure() };

    PYFLOAT_FAST_ENABLED.store(is_compatible, Ordering::Release);
    PYFLOAT_FAST_CHECKED.store(true, Ordering::Release);

    #[cfg(debug_assertions)]
    if is_compatible {
        eprintln!("Phase 30: PyFloat fast path enabled");
    } else {
        eprintln!("Phase 30: PyFloat fast path disabled (incompatible Python version)");
    }
}

/// Verify PyFloatObject structure by testing with known values
unsafe fn verify_pyfloat_structure() -> bool {
    // Test with value 0.0
    let zero = ffi::PyFloat_FromDouble(0.0);
    if zero.is_null() {
        return false;
    }

    let zero_result = extract_pyfloat_fast(zero);
    ffi::Py_DECREF(zero);

    if zero_result != 0.0 {
        return false;
    }

    // Test with value 3.14159
    let pi = ffi::PyFloat_FromDouble(3.14159);
    if pi.is_null() {
        return false;
    }

    let pi_result = extract_pyfloat_fast(pi);
    ffi::Py_DECREF(pi);

    if (pi_result - 3.14159).abs() > 1e-10 {
        return false;
    }

    // Test with negative value -123.456
    let negative = ffi::PyFloat_FromDouble(-123.456);
    if negative.is_null() {
        return false;
    }

    let negative_result = extract_pyfloat_fast(negative);
    ffi::Py_DECREF(negative);

    if (negative_result - (-123.456)).abs() > 1e-10 {
        return false;
    }

    // Test with very small value
    let small = ffi::PyFloat_FromDouble(1e-300);
    if small.is_null() {
        return false;
    }

    let small_result = extract_pyfloat_fast(small);
    ffi::Py_DECREF(small);

    if (small_result - 1e-300).abs() > 1e-310 {
        return false;
    }

    // Test with very large value
    let large = ffi::PyFloat_FromDouble(1e300);
    if large.is_null() {
        return false;
    }

    let large_result = extract_pyfloat_fast(large);
    ffi::Py_DECREF(large);

    if (large_result - 1e300).abs() > 1e290 {
        return false;
    }

    true
}

/// Check if PyFloat fast path is enabled
#[inline(always)]
pub fn is_pyfloat_fast_enabled() -> bool {
    PYFLOAT_FAST_ENABLED.load(Ordering::Relaxed)
}

/// Extract float value directly from PyFloatObject structure
///
/// # Safety
/// - obj must be a valid PyFloatObject pointer
/// - Caller should verify is_pyfloat_fast_enabled() returns true
#[inline(always)]
pub unsafe fn extract_pyfloat_fast(obj: *mut ffi::PyObject) -> f64 {
    // Read ob_fval directly from PyFloatObject
    let fval_ptr = (obj as *const u8).add(OB_FVAL_OFFSET) as *const f64;
    *fval_ptr
}

/// Fast float extraction with automatic fallback
///
/// Tries fast path first, falls back to PyFloat_AsDouble if needed.
///
/// # Safety
/// - obj must be a valid PyFloatObject pointer
#[inline(always)]
pub unsafe fn extract_float_fast(obj: *mut ffi::PyObject) -> f64 {
    if is_pyfloat_fast_enabled() {
        extract_pyfloat_fast(obj)
    } else {
        ffi::PyFloat_AsDouble(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::Python;

    #[test]
    fn test_pyfloat_fast_extraction() {
        Python::with_gil(|_py| {
            init_pyfloat_fast();

            if !is_pyfloat_fast_enabled() {
                eprintln!("Skipping test: PyFloat fast path not compatible");
                return;
            }

            unsafe {
                // Test zero
                let zero = ffi::PyFloat_FromDouble(0.0);
                assert_eq!(extract_pyfloat_fast(zero), 0.0);
                ffi::Py_DECREF(zero);

                // Test positive
                let pos = ffi::PyFloat_FromDouble(42.5);
                assert_eq!(extract_pyfloat_fast(pos), 42.5);
                ffi::Py_DECREF(pos);

                // Test negative
                let neg = ffi::PyFloat_FromDouble(-42.5);
                assert_eq!(extract_pyfloat_fast(neg), -42.5);
                ffi::Py_DECREF(neg);

                // Test infinity
                let inf = ffi::PyFloat_FromDouble(f64::INFINITY);
                assert!(extract_pyfloat_fast(inf).is_infinite());
                ffi::Py_DECREF(inf);

                // Test NaN
                let nan = ffi::PyFloat_FromDouble(f64::NAN);
                assert!(extract_pyfloat_fast(nan).is_nan());
                ffi::Py_DECREF(nan);
            }
        });
    }
}
