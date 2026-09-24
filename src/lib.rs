use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use pyo3::types::{PyBool, PyFloat, PyInt, PyString, PyList, PyTuple, PyDict, PyAny};
use pyo3::ffi;  // For direct C API access

// Performance optimizations module
mod optimizations;
mod entry;
mod lemire;
mod parser;
mod pystr;
use optimizations::{object_cache, type_cache, bulk, simd_escape, unlikely};
use type_cache::FastType;

// Phase 10.6 fast ASCII string access now lives in `pystr.rs` (version-correct
// via pyo3::ffi::PyUnicode_IS_COMPACT_ASCII / PyUnicode_DATA).

// Note: Phase 10.7 attempted inline UTF-8 encoding by reading PyUnicode_KIND
// and encoding UCS-2/UCS-4 data directly. However, this was slower than
// PyUnicode_AsUTF8AndSize due to:
// 1. Per-byte encoding overhead vs Python's optimized conversion
// 2. No benefit from Python's UTF-8 cache on repeated calls
// The ASCII fast path (Phase 10.6) is retained as it provides significant speedup.

/// Write a JSON string directly from Python's internal Unicode buffer.
/// Uses ASCII fast path when possible, falls back to cached UTF-8 for non-ASCII.
///
/// # Safety
/// Caller must ensure str_ptr is a valid PyUnicode object
#[inline]
unsafe fn write_json_string_direct(buf: &mut Vec<u8>, str_ptr: *mut ffi::PyObject) -> PyResult<()> {
    let py = Python::assume_gil_acquired();
    let bytes = pystr::utf8(py, str_ptr)?;
    simd_escape::write_json_string_simd(buf, std::str::from_utf8_unchecked(bytes));
    Ok(())
}

// Note: Inline UTF-8 encoding functions (write_json_string_latin1, write_json_string_ucs2,
// write_json_string_ucs4) were tested but removed because they were slower than using
// Python's cached UTF-8 via PyUnicode_AsUTF8AndSize. The per-byte encoding overhead
// and lack of caching made them 1.5-2x slower for repeated serialization.

// Dead code removed: serde_value_to_py_object and py_object_to_serde_value
// were never used (150+ lines). This reduces binary size and improves
// compile times. If needed in future, they can be restored from git history.

/// Write a JSON string with proper escaping to a buffer
///
/// PHASE 10 OPTIMIZATION: SIMD-accelerated escape detection and bulk copy
/// - SSE2: Process 16 bytes at a time (baseline for all x86_64)
/// - AVX2: Process 32 bytes at a time (when available)
/// - Scalar fallback for short strings and non-x86
///
/// Key insight: Most strings have NO escapes, so we optimize for bulk copying.
/// The LUT-based escaping is kept for strings that DO need escaping (beats orjson!).
///
/// # Arguments
/// * `buf` - Buffer to write to
/// * `s` - String to serialize
#[inline]
fn write_json_string(buf: &mut Vec<u8>, s: &str) {
    // Use SIMD-accelerated path
    simd_escape::write_json_string_simd(buf, s);
}

/// Phase 2: Custom high-performance JSON serializer
///
/// Uses itoa (10x faster than fmt) and ryu (5x faster than fmt) for number formatting.
/// Writes directly to Vec<u8> buffer, bypassing serde_json overhead.
struct JsonBuffer {
    /// Buffer for JSON output (pub for Phase 14 buffer reuse)
    pub buf: Vec<u8>,
}

impl JsonBuffer {
    #[inline]
    fn write_null(&mut self) {
        self.buf.extend_from_slice(b"null");
    }

    #[inline]
    fn write_bool(&mut self, value: bool) {
        self.buf.extend_from_slice(if value { b"true" } else { b"false" });
    }

    #[inline]
    fn write_int_i64(&mut self, value: i64) {
        // OPTIMIZATION: Use itoa for 10x faster integer formatting
        let mut itoa_buf = itoa::Buffer::new();
        self.buf.extend_from_slice(itoa_buf.format(value).as_bytes());
    }

    #[inline]
    fn write_int_u64(&mut self, value: u64) {
        let mut itoa_buf = itoa::Buffer::new();
        self.buf.extend_from_slice(itoa_buf.format(value).as_bytes());
    }

    #[inline]
    fn write_float(&mut self, value: f64) -> PyResult<()> {
        if unlikely(!value.is_finite()) {
            return Self::float_error(value);
        }
        // OPTIMIZATION: Use ryu for 5x faster float formatting
        let mut ryu_buf = ryu::Buffer::new();
        self.buf.extend_from_slice(ryu_buf.format(value).as_bytes());
        Ok(())
    }

    /// Error path for non-finite floats (cold path)
    #[cold]
    #[inline(never)]
    fn float_error(value: f64) -> PyResult<()> {
        Err(PyValueError::new_err(format!(
            "Cannot serialize non-finite float: {}",
            value
        )))
    }

    fn serialize_pyany(&mut self, obj: &Bound<'_, PyAny>) -> PyResult<()> {
        let fast_type = type_cache::get_fast_type(obj);

        match fast_type {
            FastType::None => {
                self.write_null();
                Ok(())
            }

            FastType::Bool => {
                let b_val = unsafe { obj.downcast_exact::<PyBool>().unwrap_unchecked() };
                self.write_bool(b_val.is_true());
                Ok(())
            }

            FastType::Int => {
                // PHASE 11 OPTIMIZATION: Use direct C API with overflow check
                // This avoids PyO3's extract() overhead and uses PyLong_AsLongLongAndOverflow
                // which is faster than checking PyErr_Occurred() after each call
                unsafe {
                    let int_ptr = obj.as_ptr();
                    let mut overflow: std::ffi::c_int = 0;
                    let val_i64 = ffi::PyLong_AsLongLongAndOverflow(int_ptr, &mut overflow);

                    if overflow == 0 {
                        // Fast path: Value fits in i64 (most common case)
                        self.write_int_i64(val_i64);
                    } else {
                        // Overflow - try u64 for large positive numbers
                        let val_u64 = ffi::PyLong_AsUnsignedLongLong(int_ptr);

                        if val_u64 != u64::MAX || ffi::PyErr_Occurred().is_null() {
                            ffi::PyErr_Clear();
                            self.write_int_u64(val_u64);
                        } else {
                            // Very large int - fall back to string representation
                            ffi::PyErr_Clear();
                            let l_val = obj.downcast_exact::<PyInt>().unwrap_unchecked();
                            let s = l_val.to_string();
                            self.buf.extend_from_slice(s.as_bytes());
                        }
                    }
                }
                Ok(())
            }

            FastType::Float => {
                let f_val = unsafe { obj.downcast_exact::<PyFloat>().unwrap_unchecked() };
                let val_f64 = f_val.extract::<f64>()?;
                self.write_float(val_f64)
            }

            FastType::String => {
                let s_val = unsafe { obj.downcast_exact::<PyString>().unwrap_unchecked() };

                // PHASE 10.7 OPTIMIZATION: Direct Unicode buffer access with inline UTF-8 encoding
                // This avoids PyUnicode_AsUTF8AndSize overhead entirely by:
                // 1. Checking ASCII flag for fast path (direct buffer access)
                // 2. For non-ASCII: Reading PyUnicode_KIND and encoding inline
                unsafe {
                    write_json_string_direct(&mut self.buf, s_val.as_ptr())?;
                }

                Ok(())
            }

            FastType::List => {
                let list_val = unsafe { obj.downcast_exact::<PyList>().unwrap_unchecked() };

                // PHASE 6A OPTIMIZATION: Bulk array processing for homogeneous arrays
                // Detect if the array contains all the same type and use optimized path
                let array_type = bulk::detect_array_type(&list_val);

                match array_type {
                    bulk::ArrayType::AllInts => {
                        // Bulk serialize integer array (Phase 6A: itoa is fastest)
                        unsafe { bulk::serialize_int_array_bulk(&list_val, &mut self.buf)? }
                    }
                    bulk::ArrayType::AllFloats => {
                        // Bulk serialize float array
                        unsafe { bulk::serialize_float_array_bulk(&list_val, &mut self.buf)? }
                    }
                    bulk::ArrayType::AllBools => {
                        // Bulk serialize boolean array
                        unsafe { bulk::serialize_bool_array_bulk(&list_val, &mut self.buf)? }
                    }
                    bulk::ArrayType::AllStrings => {
                        // Bulk serialize string array
                        unsafe {
                            bulk::serialize_string_array_bulk(
                                &list_val,
                                &mut self.buf,
                                write_json_string
                            )?
                        }
                    }
                    bulk::ArrayType::Empty => {
                        // Empty array
                        self.buf.extend_from_slice(b"[]");
                    }
                    bulk::ArrayType::Mixed => {
                        // Fall back to normal per-element serialization
                        // PHASE 3+ OPTIMIZATION: Direct C API list access (no bounds checking)
                        self.buf.push(b'[');

                        unsafe {
                            let list_ptr = list_val.as_ptr();
                            let len = ffi::PyList_GET_SIZE(list_ptr);

                            for i in 0..len {
                                if i > 0 {
                                    self.buf.push(b',');
                                }

                                // SAFETY: PyList_GET_ITEM returns borrowed reference (no refcount)
                                // Index is guaranteed valid (0 <= i < len)
                                let item_ptr = ffi::PyList_GET_ITEM(list_ptr, i);
                                let item = Bound::from_borrowed_ptr(list_val.py(), item_ptr);
                                self.serialize_pyany(&item)?;
                            }
                        }

                        self.buf.push(b']');
                    }
                }

                Ok(())
            }

            FastType::Tuple => {
                let tuple_val = unsafe { obj.downcast_exact::<PyTuple>().unwrap_unchecked() };

                // PHASE 3+ OPTIMIZATION: Direct C API tuple access (no bounds checking)
                self.buf.push(b'[');

                unsafe {
                    let tuple_ptr = tuple_val.as_ptr();
                    let len = ffi::PyTuple_GET_SIZE(tuple_ptr);

                    for i in 0..len {
                        if i > 0 {
                            self.buf.push(b',');
                        }

                        // SAFETY: PyTuple_GET_ITEM returns borrowed reference (no refcount)
                        // Index is guaranteed valid (0 <= i < len)
                        let item_ptr = ffi::PyTuple_GET_ITEM(tuple_ptr, i);
                        let item = Bound::from_borrowed_ptr(tuple_val.py(), item_ptr);
                        self.serialize_pyany(&item)?;
                    }
                }

                self.buf.push(b']');
                Ok(())
            }

            FastType::Dict => {
                let dict_val = unsafe { obj.downcast_exact::<PyDict>().unwrap_unchecked() };
                self.buf.push(b'{');

                // PHASE 3 OPTIMIZATION: Direct C API dict iteration
                // PyDict_Next is 2-3x faster than PyO3's iterator
                // This is the key optimization that orjson uses
                unsafe {
                    let dict_ptr = dict_val.as_ptr();
                    let mut pos: ffi::Py_ssize_t = 0;
                    let mut key_ptr: *mut ffi::PyObject = std::ptr::null_mut();
                    let mut value_ptr: *mut ffi::PyObject = std::ptr::null_mut();

                    let mut first = true;

                    while ffi::PyDict_Next(dict_ptr, &mut pos, &mut key_ptr, &mut value_ptr) != 0 {
                        if !first {
                            self.buf.push(b',');
                        }
                        first = false;

                        // SAFETY: PyDict_Next returns borrowed references (no need to decref)
                        // Convert raw pointers to PyString
                        if ffi::PyUnicode_Check(key_ptr) == 0 {
                            return Err(PyValueError::new_err(
                                "Dictionary keys must be strings for JSON serialization"
                            ));
                        }

                        // PHASE 10.7: Direct Unicode buffer access with inline UTF-8 encoding
                        write_json_string_direct(&mut self.buf, key_ptr)?;
                        self.buf.push(b':');

                        // Serialize value (wrap in Bound for safe handling)
                        // SAFETY: value_ptr is a borrowed reference from PyDict_Next
                        let value = Bound::from_borrowed_ptr(dict_val.py(), value_ptr);
                        self.serialize_pyany(&value)?;
                    }
                }

                self.buf.push(b'}');
                Ok(())
            }

            FastType::Other => Self::unsupported_type_error(obj),
        }
    }

    /// Error path for unsupported types (cold path)
    #[cold]
    #[inline(never)]
    fn unsupported_type_error(obj: &Bound<'_, PyAny>) -> PyResult<()> {
        Err(PyValueError::new_err(format!(
            "Unsupported Python type for JSON serialization: {}",
            obj.get_type()
                .name()
                .and_then(|n| n.to_str().map(|s| s.to_owned()))
                .unwrap_or_else(|_| "unknown".to_string())
        )))
    }
}

/// Estimate JSON output size for buffer pre-allocation.
///
/// Provides a heuristic size estimate to minimize reallocations.
#[inline]
fn estimate_json_size(obj: &Bound<'_, PyAny>) -> usize {
    let fast_type = type_cache::get_fast_type(obj);

    match fast_type {
        FastType::None => 4,                          // "null"
        FastType::Bool => 5,                          // "false"
        FastType::Int => 20,                          // max i64 digits
        FastType::Float => 24,                        // max f64 representation
        FastType::String => {
            if let Ok(s) = obj.downcast_exact::<PyString>() {
                s.len().unwrap_or(0) + 8              // +8 for quotes and potential escapes
            } else {
                32
            }
        }
        FastType::List => {
            if let Ok(list) = obj.downcast_exact::<PyList>() {
                let len = list.len();
                len * 16 + 16                         // heuristic: 16 bytes per element
            } else {
                64
            }
        }
        FastType::Tuple => {
            if let Ok(tuple) = obj.downcast_exact::<PyTuple>() {
                let len = tuple.len();
                len * 16 + 16
            } else {
                64
            }
        }
        FastType::Dict => {
            if let Ok(dict) = obj.downcast_exact::<PyDict>() {
                let len = dict.len();
                len * 32 + 16                         // heuristic: 32 bytes per entry
            } else {
                128
            }
        }
        FastType::Other => 64,
    }
}

/// Serializes a Python object (called from `entry::dumps`).
///
/// Writes into the reused thread-local buffer and hands the bytes to `finish`,
/// which builds the Python result directly from them (no intermediate String;
/// the old `buf.clone()` cost a malloc+memcpy per call and N bytes of peak RSS).
#[inline]
pub(crate) fn dumps_impl<R>(
    data: &Bound<'_, PyAny>,
    finish: impl FnOnce(&[u8]) -> PyResult<R>,
) -> PyResult<R> {
    let capacity = estimate_json_size(data);
    object_cache::get_serialize_buffer(capacity, |buf| {
        let mut buffer = JsonBuffer { buf: std::mem::take(buf) };
        let result = buffer.serialize_pyany(data);
        *buf = buffer.buf;
        result?;
        finish(buf)
    })
}

/// Python module definition for rjson.
///
/// Provides optimized JSON parsing (`loads`) and serialization (`dumps`) functions.
///
/// # Performance Optimizations
/// Phase 1-6: Integer caching, type pointer caching, bulk array processing
/// Phase 7: SIMD-accelerated parsing with simd-json
/// Phase 8: GIL batching (parse to IR, then batch-create Python objects)
/// Phase 9: String interning for common dict keys
///
/// Performance: 8-9x faster dumps, 1.5-2x faster loads vs stdlib json
#[pymodule]
fn rjson(py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    // OPTIMIZATION: Initialize all caches at module load time
    object_cache::init_cache(py);
    type_cache::init_type_cache(py);

    entry::register(m)?; // loads / dumps as raw METH_O builtins
    Ok(())
}
