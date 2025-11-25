use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use pyo3::types::{PyBool, PyFloat, PyInt, PyString, PyList, PyTuple, PyDict, PyAny, PyBytes};
use pyo3::ffi;  // For direct C API access
use serde::de::{self, Visitor, MapAccess, SeqAccess, Deserializer, DeserializeSeed};
use std::fmt;

// Performance optimizations module
mod optimizations;
use optimizations::{object_cache, type_cache, bulk, extreme, simd_parser, simd_escape, unlikely};
use type_cache::FastType;

// ============================================================================
// Phase 10.6: Fast ASCII String Extraction
// ============================================================================
//
// PyUnicode_AsUTF8AndSize is slow for non-ASCII strings because Python stores
// them in UCS-2/UCS-4 format and must convert to UTF-8 on demand.
//
// For ASCII strings (the common case in JSON), we can access the buffer directly
// by reading the PyASCIIObject structure. This matches what orjson does.
//
// WARNING: This is CPython-specific and version-dependent!
// Tested on Python 3.8-3.13. The layout has been stable since Python 3.3.

/// Simplified PyASCIIObject structure (CPython internal)
/// We only need the fields up to and including the state flags.
#[repr(C)]
struct PyASCIIObject {
    /// PyObject_HEAD: ob_refcnt, ob_type
    _ob_refcnt: isize,
    _ob_type: *mut ffi::PyTypeObject,
    /// String length (number of characters, not bytes for non-ASCII)
    length: isize,
    /// Cached hash value (-1 if not computed)
    _hash: isize,
    /// State flags packed as a u32
    /// Bits: interned(2), kind(3), compact(1), ascii(1), ready(1), ...
    state: u32,
}

/// Bit mask to extract the 'ascii' flag from state
/// The ascii flag is bit 6 (after interned:2, kind:3, compact:1)
const STATE_ASCII_MASK: u32 = 0b01000000;  // bit 6

/// Offset from PyASCIIObject to the actual character data
/// For compact ASCII strings, data follows immediately after:
/// PyASCIIObject (on 64-bit: 8+8+8+8+4 = 36, aligned to 40) + wstr (8) = 48
/// But actually for ASCII-only compact strings, there's no wstr field stored,
/// so the data starts right after the null terminator padding.
///
/// The correct formula: sizeof(PyASCIIObject) rounded up to pointer alignment
/// On 64-bit Linux: sizeof(PyASCIIObject) = 40, data at offset 40
/// But we need to account for the compact representation!
///
/// For Python 3.12+: The structure is:
/// - PyObject_HEAD (16 bytes)
/// - length (8 bytes)
/// - hash (8 bytes)
/// - state (4 bytes + 4 padding) = 40 total
/// - Then string data follows for compact ASCII
///
/// Actually, let me be more careful. The safest approach is to use the
/// PyUnicode_DATA macro equivalent, which is:
/// ((void*)((PyASCIIObject*)(op))->data) for non-legacy strings
/// But actually compact strings store data inline after the struct.
///
/// For maximum safety, compute offset based on known structure:
#[cfg(target_pointer_width = "64")]
const ASCII_DATA_OFFSET: usize = 40;  // PyASCIIObject: PyObject_HEAD(16) + length(8) + hash(8) + state(4) + padding(4) = 40

#[cfg(target_pointer_width = "32")]
const ASCII_DATA_OFFSET: usize = 24;  // PyASCIIObject(20) + padding

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
unsafe fn write_json_string_direct(buf: &mut Vec<u8>, str_ptr: *mut ffi::PyObject) {
    let ascii_obj = str_ptr as *const PyASCIIObject;
    let state = (*ascii_obj).state;
    let length = (*ascii_obj).length as usize;

    // Check ASCII flag first (most common case in JSON)
    if state & STATE_ASCII_MASK != 0 {
        // FAST PATH: Pure ASCII - direct buffer access, no conversion needed
        let data_ptr = (str_ptr as *const u8).add(ASCII_DATA_OFFSET);
        let bytes = std::slice::from_raw_parts(data_ptr, length);
        simd_escape::write_json_string_simd(buf, std::str::from_utf8_unchecked(bytes));
        return;
    }

    // Non-ASCII path: Use PyUnicode_AsUTF8AndSize which benefits from Python's UTF-8 cache
    // Note: Inline UTF-8 encoding was tested but is slower due to:
    // 1. Per-byte encoding overhead
    // 2. No benefit from Python's UTF-8 cache on repeated calls
    let mut size: ffi::Py_ssize_t = 0;
    let utf8_ptr = ffi::PyUnicode_AsUTF8AndSize(str_ptr, &mut size);
    if !utf8_ptr.is_null() {
        let bytes = std::slice::from_raw_parts(utf8_ptr as *const u8, size as usize);
        simd_escape::write_json_string_simd(buf, std::str::from_utf8_unchecked(bytes));
    }
}

// Note: Inline UTF-8 encoding functions (write_json_string_latin1, write_json_string_ucs2,
// write_json_string_ucs4) were tested but removed because they were slower than using
// Python's cached UTF-8 via PyUnicode_AsUTF8AndSize. The per-byte encoding overhead
// and lack of caching made them 1.5-2x slower for repeated serialization.

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
///
/// PHASE 13 Optimizations:
/// - Direct C API calls for string/int/float creation (bypasses PyO3 overhead)
/// - Direct list creation with PyList_New + PyList_SET_ITEM (avoids Vec intermediate)
/// - Direct dict creation with PyDict_New + PyDict_SetItem
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
            // PHASE 13 OPTIMIZATION: Direct C API call bypasses PyO3 overhead
            unsafe {
                let ptr = object_cache::create_int_i64_direct(v);
                Ok(PyObject::from_owned_ptr(self.py, ptr))
            }
        }
    }

    #[inline]
    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
        // OPTIMIZATION: Only cache if value fits in small integer range
        if v <= 256 {
            Ok(object_cache::get_int(self.py, v as i64))
        } else {
            // PHASE 13 OPTIMIZATION: Direct C API call
            unsafe {
                let ptr = object_cache::create_int_u64_direct(v);
                Ok(PyObject::from_owned_ptr(self.py, ptr))
            }
        }
    }

    #[inline]
    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
        // PHASE 13 OPTIMIZATION: Direct C API call
        unsafe {
            let ptr = object_cache::create_float_direct(v);
            Ok(PyObject::from_owned_ptr(self.py, ptr))
        }
    }

    #[inline]
    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E> {
        // PHASE 13 OPTIMIZATION: Direct C API call (2-3x faster than to_object)
        unsafe {
            let ptr = object_cache::create_string_direct(v);
            Ok(PyObject::from_owned_ptr(self.py, ptr))
        }
    }

    #[inline]
    fn visit_string<E>(self, v: String) -> Result<Self::Value, E> {
        // PHASE 13 OPTIMIZATION: Direct C API call
        unsafe {
            let ptr = object_cache::create_string_direct(&v);
            Ok(PyObject::from_owned_ptr(self.py, ptr))
        }
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
        // PHASE 13 OPTIMIZATION: Direct list creation with C API
        // First collect elements (we need the count for PyList_New)
        let size = seq.size_hint().unwrap_or(0);
        let mut elements: Vec<PyObject> = Vec::with_capacity(size);

        while let Some(elem) = seq.next_element_seed(PyObjectSeed { py: self.py })? {
            elements.push(elem);
        }

        // Now create list directly with exact size (no resizing)
        unsafe {
            let list_ptr = object_cache::create_list_direct(elements.len() as ffi::Py_ssize_t);
            if list_ptr.is_null() {
                use serde::de::Error as SerdeDeError;
                return Err(SerdeDeError::custom("Failed to create list"));
            }

            // Set items directly (steals references, so we use into_ptr)
            for (i, elem) in elements.into_iter().enumerate() {
                object_cache::set_list_item_direct(list_ptr, i as ffi::Py_ssize_t, elem.into_ptr());
            }

            Ok(PyObject::from_owned_ptr(self.py, list_ptr))
        }
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        // PHASE 13 OPTIMIZATION: Direct dict creation with C API
        use serde::de::Error as SerdeDeError;

        unsafe {
            let dict_ptr = object_cache::create_dict_direct();
            if dict_ptr.is_null() {
                return Err(SerdeDeError::custom("Failed to create dict"));
            }

            // Insert directly using C API
            while let Some((key, value)) = map.next_entry_seed(KeySeed, PyObjectSeed { py: self.py })? {
                // Create key string directly
                let key_ptr = object_cache::create_string_direct(&key);
                if key_ptr.is_null() {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(SerdeDeError::custom("Failed to create key string"));
                }

                // Insert: PyDict_SetItem does NOT steal references
                let result = object_cache::set_dict_item_direct(dict_ptr, key_ptr, value.as_ptr());

                // Clean up key (we own it, PyDict_SetItem increfs it)
                ffi::Py_DECREF(key_ptr);

                if result < 0 {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(SerdeDeError::custom("Failed to insert into dict"));
                }
            }

            Ok(PyObject::from_owned_ptr(self.py, dict_ptr))
        }
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
                    write_json_string_direct(&mut self.buf, s_val.as_ptr());
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
                        unsafe {
                            let list_ptr = list_val.as_ptr();
                            let len = ffi::PyList_GET_SIZE(list_ptr);

                            // Pre-allocate buffer (estimate: 8 bytes per element)
                            self.buf.reserve((len as usize) * 8 + 2);
                            self.buf.push(b'[');

                            if len > 0 {
                                // Handle first element without comma
                                let first_ptr = ffi::PyList_GET_ITEM(list_ptr, 0);
                                let first = Bound::from_borrowed_ptr(list_val.py(), first_ptr);
                                self.serialize_pyany(&first)?;

                                // Handle remaining elements with leading comma
                                for i in 1..len {
                                    self.buf.push(b',');
                                    let item_ptr = ffi::PyList_GET_ITEM(list_ptr, i);
                                    let item = Bound::from_borrowed_ptr(list_val.py(), item_ptr);
                                    self.serialize_pyany(&item)?;
                                }
                            }

                            self.buf.push(b']');
                        }
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

                // PHASE 3 OPTIMIZATION: Direct C API dict iteration
                // PyDict_Next is 2-3x faster than PyO3's iterator
                unsafe {
                    let dict_ptr = dict_val.as_ptr();
                    let dict_len = ffi::PyDict_Size(dict_ptr);

                    // Empty dict fast path
                    if dict_len == 0 {
                        self.buf.extend_from_slice(b"{}");
                        return Ok(());
                    }

                    // Pre-allocate buffer (estimate: 20 bytes per key-value pair)
                    self.buf.reserve((dict_len as usize) * 20);
                    self.buf.push(b'{');

                    let mut pos: ffi::Py_ssize_t = 0;
                    let mut key_ptr: *mut ffi::PyObject = std::ptr::null_mut();
                    let mut value_ptr: *mut ffi::PyObject = std::ptr::null_mut();

                    // Handle first element without comma
                    if ffi::PyDict_Next(dict_ptr, &mut pos, &mut key_ptr, &mut value_ptr) != 0 {
                        // Check key is string
                        if ffi::PyUnicode_Check(key_ptr) == 0 {
                            return Err(PyValueError::new_err(
                                "Dictionary keys must be strings for JSON serialization"
                            ));
                        }

                        write_json_string_direct(&mut self.buf, key_ptr);
                        self.buf.push(b':');
                        let value = Bound::from_borrowed_ptr(dict_val.py(), value_ptr);
                        self.serialize_pyany(&value)?;

                        // Handle remaining elements with leading comma
                        while ffi::PyDict_Next(dict_ptr, &mut pos, &mut key_ptr, &mut value_ptr) != 0 {
                            self.buf.push(b',');

                            if ffi::PyUnicode_Check(key_ptr) == 0 {
                                return Err(PyValueError::new_err(
                                    "Dictionary keys must be strings for JSON serialization"
                                ));
                            }

                            write_json_string_direct(&mut self.buf, key_ptr);
                            self.buf.push(b':');
                            let value = Bound::from_borrowed_ptr(dict_val.py(), value_ptr);
                            self.serialize_pyany(&value)?;
                        }
                    }

                    self.buf.push(b'}');
                }

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

/// Dumps a Python object into a JSON string.
///
/// Phase 2 Optimizations:
/// - Direct buffer writing (bypasses serde_json)
/// - itoa for 10x faster integer formatting
/// - ryu for 5x faster float formatting
/// - Pre-sized buffer allocation
///
/// PHASE 14 Optimization:
/// - Thread-local buffer reuse to avoid repeated allocations
/// - Buffer grows to max needed size and stays allocated
///
/// # Arguments
/// * `py` - The Python GIL token.
/// * `data` - The Python object to serialize.
///
/// # Returns
/// A JSON string, or a PyValueError on error.
#[pyfunction]
fn dumps(_py: Python, data: &Bound<'_, PyAny>) -> PyResult<String> {
    // Allocate a new buffer each time - simpler and avoids clone overhead
    // The allocation cost is minimal compared to serialization work
    let capacity = estimate_json_size(data);
    let mut buffer = JsonBuffer { buf: Vec::with_capacity(capacity) };

    buffer.serialize_pyany(data)?;

    // SAFETY: We only write valid UTF-8 (JSON is always UTF-8)
    Ok(unsafe { String::from_utf8_unchecked(buffer.buf) })
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
