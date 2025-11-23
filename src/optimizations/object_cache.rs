//! Object caching for frequently used Python objects
//!
//! This module implements caching strategies to reduce GIL overhead and
//! Python object allocation costs. Key optimizations:
//!
//! 1. Integer caching for small values [-256, 256]
//! 2. Singleton caching for None, True, False
//! 3. Empty collection caching

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::OnceLock;

/// Size of the integer cache (range: -256 to 256)
const INT_CACHE_SIZE: usize = 513;
const INT_CACHE_OFFSET: i64 = 256;

/// Global cache for small integers, singletons, and empty collections
struct ObjectCache {
    /// Cached integers from -256 to 256 (inclusive)
    integers: Vec<PyObject>,

    /// Singleton None
    none: PyObject,

    /// Singleton True
    true_obj: PyObject,

    /// Singleton False
    false_obj: PyObject,

    /// Empty list singleton
    empty_list: PyObject,

    /// Empty dict singleton
    empty_dict: PyObject,
}

/// Global object cache instance
static OBJECT_CACHE: OnceLock<ObjectCache> = OnceLock::new();

/// Initialize the object cache
///
/// This should be called once during module initialization.
/// Subsequent calls are no-ops.
///
/// # Arguments
/// * `py` - Python GIL token
pub fn init_cache(py: Python) {
    // Only initialize once
    if OBJECT_CACHE.get().is_some() {
        return;
    }

    // Pre-allocate integer cache
    let mut integers = Vec::with_capacity(INT_CACHE_SIZE);
    for i in -INT_CACHE_OFFSET..=(INT_CACHE_OFFSET) {
        integers.push(i.to_object(py));
    }

    let cache = ObjectCache {
        integers,
        none: py.None(),
        true_obj: true.to_object(py),
        false_obj: false.to_object(py),
        empty_list: PyList::empty(py).to_object(py),
        empty_dict: PyDict::new(py).to_object(py),
    };

    // Store in global cache
    let _ = OBJECT_CACHE.set(cache);
}

/// Get a cached integer or create a new one
///
/// For integers in range [-256, 256], returns a cached Python object.
/// For integers outside this range, creates a new Python object.
///
/// # Arguments
/// * `py` - Python GIL token
/// * `value` - Integer value
///
/// # Returns
/// Python integer object
///
/// # Performance
/// - Cached integers: O(1) lookup, no allocation
/// - Non-cached integers: Standard PyO3 conversion
#[inline(always)]
pub fn get_int(py: Python, value: i64) -> PyObject {
    // Fast path: check if in cache range
    if value >= -INT_CACHE_OFFSET && value <= INT_CACHE_OFFSET {
        if let Some(cache) = OBJECT_CACHE.get() {
            let index = (value + INT_CACHE_OFFSET) as usize;
            // SAFETY: Index is guaranteed to be in bounds by the if condition above
            return cache.integers[index].clone_ref(py);
        }
    }

    // Slow path: create new object for large integers
    value.to_object(py)
}

/// Get cached None singleton
///
/// # Arguments
/// * `py` - Python GIL token
///
/// # Returns
/// Python None object
#[inline(always)]
pub fn get_none(py: Python) -> PyObject {
    if let Some(cache) = OBJECT_CACHE.get() {
        cache.none.clone_ref(py)
    } else {
        py.None()
    }
}

/// Get cached boolean singleton
///
/// # Arguments
/// * `py` - Python GIL token
/// * `value` - Boolean value
///
/// # Returns
/// Python True or False object
#[inline(always)]
pub fn get_bool(py: Python, value: bool) -> PyObject {
    if let Some(cache) = OBJECT_CACHE.get() {
        if value {
            cache.true_obj.clone_ref(py)
        } else {
            cache.false_obj.clone_ref(py)
        }
    } else {
        value.to_object(py)
    }
}

/// Get cached empty list singleton
///
/// # Arguments
/// * `py` - Python GIL token
///
/// # Returns
/// Python empty list object
#[inline(always)]
pub fn get_empty_list(py: Python) -> PyObject {
    if let Some(cache) = OBJECT_CACHE.get() {
        cache.empty_list.clone_ref(py)
    } else {
        PyList::empty(py).to_object(py)
    }
}

/// Get cached empty dict singleton
///
/// # Arguments
/// * `py` - Python GIL token
///
/// # Returns
/// Python empty dict object
#[inline(always)]
pub fn get_empty_dict(py: Python) -> PyObject {
    if let Some(cache) = OBJECT_CACHE.get() {
        cache.empty_dict.clone_ref(py)
    } else {
        PyDict::new(py).to_object(py)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_int_cache() {
        Python::with_gil(|py| {
            init_cache(py);

            // Test cached values
            let zero = get_int(py, 0);
            let positive = get_int(py, 100);
            let negative = get_int(py, -100);

            // Test cache hit (should be same object)
            let zero2 = get_int(py, 0);
            assert!(zero.is(&zero2));

            // Test large values (outside cache)
            let large = get_int(py, 1000);
            let large2 = get_int(py, 1000);
            // These should be different objects since not cached
            assert!(!large.is(&large2));
        });
    }

    #[test]
    fn test_singleton_cache() {
        Python::with_gil(|py| {
            init_cache(py);

            let none1 = get_none(py);
            let none2 = get_none(py);
            assert!(none1.is(&none2));

            let true1 = get_bool(py, true);
            let true2 = get_bool(py, true);
            assert!(true1.is(&true2));

            let false1 = get_bool(py, false);
            let false2 = get_bool(py, false);
            assert!(false1.is(&false2));
        });
    }
}
