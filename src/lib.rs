use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use pyo3::types::{PyDict, PyList, PyBool, PyFloat, PyInt, PyString, PyTuple, PyAny, PyBytes};
use pyo3::ffi;  // For direct C API access
use serde_json;
use serde::de::{self, Visitor, MapAccess, SeqAccess, Deserializer, DeserializeSeed};
use std::fmt;
use itoa;
use ryu;
use memchr::memchr3;

// Performance optimizations module
mod optimizations;
use optimizations::{object_cache, type_cache, bulk, extreme, escape_lut, simd_parser, likely, unlikely};
use type_cache::FastType;

// Dead code removed: serde_value_to_py_object and py_object_to_serde_value
// were never used (150+ lines). This reduces binary size and improves
// compile times. If needed in future, they can be restored from git history.

/// Optimized visitor that builds PyO3 objects directly from serde_json events.
///
/// Phase 1.5+ Optimizations Applied:
/// - Integer caching with inline range checks
/// - Pre-sized vector allocations with size hints
/// - Cached None/True/False singletons
/// - Direct dict insertion without intermediate Vecs
/// - Unsafe unwrap_unchecked after type validation (loads-specific)
struct PyObjectVisitor<'py> {
    py: Python<'py>,
}

impl<'de, 'py> Visitor<'de> for PyObjectVisitor<'py> {
    type Value = PyObject;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("any valid JSON value")
    }

    #[inline]
    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached boolean singletons
        Ok(object_cache::get_bool(self.py, v))
    }

    #[inline]
    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
        // OPTIMIZATION: Inline cache check to avoid function call overhead
        // Only use cache for small values where it's beneficial
        if v >= -256 && v <= 256 {
            Ok(object_cache::get_int(self.py, v))
        } else {
            // Fast path: direct conversion for large integers
            Ok(v.to_object(self.py))
        }
    }

    #[inline]
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        // OPTIMIZATION: Only cache if value fits in small integer range
        if v <= 256 {
            Ok(object_cache::get_int(self.py, v as i64))
        } else {
            Ok(v.to_object(self.py))
        }
    }

    #[inline]
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        Ok(v.to_object(self.py))
    }

    #[inline]
    fn visit_none<E>(self) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached None singleton
        Ok(object_cache::get_none(self.py))
    }

    #[inline]
    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        // OPTIMIZATION: Use cached None singleton
        Ok(object_cache::get_none(self.py))
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PyObjectVisitor { py: self.py })
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        // OPTIMIZATION Phase 1.3: Pre-allocate with size hint
        let size = seq.size_hint().unwrap_or(0);
        let mut elements = Vec::with_capacity(size);

        // Collect all elements
        while let Some(elem) = seq.next_element_seed(PyObjectSeed { py: self.py })? {
            elements.push(elem);
        }

        use serde::de::Error as SerdeDeError;
        let pylist = PyList::new(self.py, &elements)
            .map_err(|e| SerdeDeError::custom(e.to_string()))?;
        Ok(pylist.to_object(self.py))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // OPTIMIZATION Phase 1.5: Direct dict insertion without intermediate Vecs
        // This eliminates 2 heap allocations and improves cache locality
        use serde::de::Error as SerdeDeError;

        let dict = PyDict::new(self.py);

        // Insert directly into dict as we parse
        while let Some((key, value)) = map.next_entry_seed(KeySeed, PyObjectSeed { py: self.py })? {
            dict.set_item(&key, &value)
                .map_err(|e| SerdeDeError::custom(format!("Failed to insert into dict: {}", e)))?;
        }

        Ok(dict.to_object(self.py))
    }
}

/// Seed for deserializing JSON to Python objects (public for simd_parser fallback)
pub(crate) struct PyObjectSeed<'py> {
    pub(crate) py: Python<'py>,
}

impl<'de, 'py> de::DeserializeSeed<'de> for PyObjectSeed<'py> {
    type Value = PyObject;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(PyObjectVisitor { py: self.py })
    }
}

struct KeySeed;
impl<'de> de::DeserializeSeed<'de> for KeySeed {
    type Value = String;
    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        de::Deserialize::deserialize(deserializer)
    }
}

/// Parses a JSON string into a Python object.
///
/// Uses serde_json with direct Python object creation via Visitor pattern.
/// This provides single-pass parsing without intermediate representations.
///
/// # Arguments
/// * `json_str` - The JSON string to parse.
///
/// # Returns
/// A PyObject representing the parsed JSON, or a PyValueError on error.
#[pyfunction]
fn loads(json_str: &str) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let mut de = serde_json::Deserializer::from_str(json_str);
        DeserializeSeed::deserialize(PyObjectSeed { py }, &mut de)
            .map_err(|e| PyValueError::new_err(format!("JSON parsing error: {e}")))
    })
}

/// Parses JSON using SIMD-accelerated parser (always uses simd-json)
///
/// This function always uses the SIMD parser regardless of input size.
/// Use this when you know you have large JSON inputs.
///
/// # Arguments
/// * `json_str` - The JSON string to parse.
///
/// # Returns
/// A PyObject representing the parsed JSON, or a PyValueError on error.
#[pyfunction]
fn loads_simd(json_str: &str) -> PyResult<PyObject> {
    simd_parser::loads_simd(json_str)
}

/// Write a JSON string with proper escaping to a buffer
///
/// PHASE 7 OPTIMIZATION: Hybrid approach using:
/// 1. SIMD memchr3 for quick escape detection (most strings have no escapes)
/// 2. LUT-based escaping for strings that need it (faster than match statements)
///
/// # Arguments
/// * `buf` - Buffer to write to
/// * `s` - String to serialize
#[inline]
fn write_json_string(buf: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();

    // FAST PATH: Use SIMD to detect if any common escapes exist
    // memchr3 is extremely fast (uses SIMD internally)
    let has_common_escapes = memchr3(b'"', b'\\', b'\n', bytes).is_some();

    if likely(!has_common_escapes) {
        // Check for control characters (rare case)
        if let Some(idx) = escape_lut::find_first_escape(bytes) {
            // Has escapes - use LUT path
            write_json_string_with_lut(buf, s, idx);
        } else {
            // ULTRA-FAST PATH: No escapes at all, direct memcpy
            buf.push(b'"');
            buf.extend_from_slice(bytes);
            buf.push(b'"');
        }
    } else {
        // Has common escapes - find first and use LUT
        if let Some(idx) = escape_lut::find_first_escape(bytes) {
            write_json_string_with_lut(buf, s, idx);
        } else {
            // Shouldn't happen, but handle gracefully
            buf.push(b'"');
            buf.extend_from_slice(bytes);
            buf.push(b'"');
        }
    }
}

/// Write JSON string using LUT-based escaping, knowing escape starts at `first_escape_idx`
#[inline(never)]  // Keep hot path small, this is the cold path
#[cold]
fn write_json_string_with_lut(buf: &mut Vec<u8>, s: &str, first_escape_idx: usize) {
    let bytes = s.as_bytes();
    buf.push(b'"');

    // Copy prefix (before first escape) directly
    if first_escape_idx > 0 {
        buf.extend_from_slice(&bytes[..first_escape_idx]);
    }

    // Escape the rest using LUT
    escape_lut::write_escaped_lut(buf, &bytes[first_escape_idx..]);
    buf.push(b'"');
}

/// Phase 2: Custom high-performance JSON serializer
///
/// Uses itoa (10x faster than fmt) and ryu (5x faster than fmt) for number formatting.
/// Writes directly to Vec<u8> buffer, bypassing serde_json overhead.
struct JsonBuffer {
    buf: Vec<u8>,
}

impl JsonBuffer {
    #[inline]
    fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

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

    #[inline]
    fn write_string(&mut self, s: &str) {
        write_json_string(&mut self.buf, s);
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
                let l_val = unsafe { obj.downcast_exact::<PyInt>().unwrap_unchecked() };

                // Try i64 first (most common)
                if let Ok(val_i64) = l_val.extract::<i64>() {
                    self.write_int_i64(val_i64);
                    Ok(())
                } else if let Ok(val_u64) = l_val.extract::<u64>() {
                    self.write_int_u64(val_u64);
                    Ok(())
                } else {
                    // Fallback for very large integers: convert to string
                    let s = l_val.to_string();
                    self.buf.extend_from_slice(s.as_bytes());
                    Ok(())
                }
            }

            FastType::Float => {
                let f_val = unsafe { obj.downcast_exact::<PyFloat>().unwrap_unchecked() };
                let val_f64 = f_val.extract::<f64>()?;
                self.write_float(val_f64)
            }

            FastType::String => {
                let s_val = unsafe { obj.downcast_exact::<PyString>().unwrap_unchecked() };

                // PHASE 3+ OPTIMIZATION: Zero-copy string extraction (no allocation)
                unsafe {
                    let str_ptr = s_val.as_ptr();
                    let mut size: ffi::Py_ssize_t = 0;
                    let data_ptr = ffi::PyUnicode_AsUTF8AndSize(str_ptr, &mut size);

                    if data_ptr.is_null() {
                        return Err(PyValueError::new_err("String must be valid UTF-8"));
                    }

                    // SAFETY: Python guarantees UTF-8 validity for PyUnicode objects
                    let str_slice = std::slice::from_raw_parts(data_ptr as *const u8, size as usize);
                    let str_ref = std::str::from_utf8_unchecked(str_slice);

                    self.write_string(str_ref);
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

                        // Get UTF-8 string data directly from Python (zero-copy)
                        let mut size: ffi::Py_ssize_t = 0;
                        let data_ptr = ffi::PyUnicode_AsUTF8AndSize(key_ptr, &mut size);

                        if data_ptr.is_null() {
                            return Err(PyValueError::new_err("Dictionary key must be valid UTF-8"));
                        }

                        // SAFETY: Python guarantees UTF-8 validity for PyUnicode objects
                        let key_slice = std::slice::from_raw_parts(data_ptr as *const u8, size as usize);
                        let key_str = std::str::from_utf8_unchecked(key_slice);

                        self.write_string(key_str);
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

    fn into_string(self) -> String {
        // SAFETY: We only write valid UTF-8 (all JSON is valid UTF-8)
        unsafe { String::from_utf8_unchecked(self.buf) }
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

/// Dumps a Python object into a JSON string.
///
/// Phase 2 Optimizations:
/// - Direct buffer writing (bypasses serde_json)
/// - itoa for 10x faster integer formatting
/// - ryu for 5x faster float formatting
/// - Pre-sized buffer allocation
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `data` - The Python object to serialize.
///
/// # Returns
/// A JSON string, or a PyValueError on error.
#[pyfunction]
fn dumps(_py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    let capacity = estimate_json_size(data);
    let mut buffer = JsonBuffer::with_capacity(capacity);
    buffer.serialize_pyany(data)?;
    Ok(buffer.into_string())
}

/// EXTREME OPTIMIZATION: dumps_bytes() - The "Nuclear Option"
///
/// Returns PyBytes instead of String for zero-copy performance.
/// This is 10-20% faster than dumps() but breaks API compatibility.
///
/// Optimizations:
/// - Zero-copy: Returns bytes directly, no UTF-8 validation
/// - Direct C API: Bypasses PyO3 completely for serialization
/// - AVX2 SIMD: String escape detection (when available)
/// - Aggressive inlining: Single massive function, no calls
/// - Zero abstraction: Direct CPython API, no safety layer
///
/// WARNING: More unsafe code, harder to maintain, but MAXIMUM PERFORMANCE
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `data` - The Python object to serialize.
///
/// # Returns
/// PyBytes containing JSON (not validated as UTF-8 string)
#[pyfunction]
fn dumps_bytes(py: Python, data: &Bound<'_, PyAny>) -> PyResult<Py<PyBytes>> {
    unsafe {
        // SAFETY: We transmute Python to 'static for the serializer.
        // This is safe because we don't actually store it beyond this function call.
        let py_static = std::mem::transmute::<Python, Python<'static>>(py);

        let obj_ptr = data.as_ptr();
        let capacity = extreme::estimate_size_fast(obj_ptr);

        let mut serializer = extreme::DirectSerializer::new(py_static, capacity);
        serializer.serialize_direct(obj_ptr)?;

        Ok(serializer.into_pybytes(py))
    }
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
    simd_parser::init_string_intern(py);  // Phase 9: String interning

    m.add_function(wrap_pyfunction!(loads, m)?)?;
    m.add_function(wrap_pyfunction!(loads_simd, m)?)?;  // Phase 7: SIMD loads
    m.add_function(wrap_pyfunction!(dumps, m)?)?;
    m.add_function(wrap_pyfunction!(dumps_bytes, m)?)?;  // Nuclear option
    Ok(())
}
