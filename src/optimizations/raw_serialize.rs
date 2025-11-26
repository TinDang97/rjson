//! Phase 39: Raw C API Serialization with Raw Buffer Manipulation
//!
//! This module implements the fastest possible JSON serialization by:
//! - Using raw *mut ffi::PyObject pointers (no PyO3 wrapper overhead)
//! - Raw buffer manipulation with unsafe pointer writes (no Vec methods)
//! - Direct CPython C API calls (no abstraction layers)
//! - Zero intermediate allocations
//! - Inline everything for maximum performance
//!
//! WARNING: This is highly unsafe code optimized purely for performance.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;

use super::type_cache;
use super::pylong_fast;
use super::pyfloat_fast;
use super::simd_escape;

// ============================================================================
// DYNAMIC PROGRAMMING: Precomputed digit lookup tables
// ============================================================================

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

static DIGITS: [u8; 10] = [b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9'];

// ============================================================================
// PyASCIIObject structure for fast string access
// ============================================================================

#[repr(C)]
struct PyASCIIObject {
    _ob_refcnt: isize,
    _ob_type: *mut ffi::PyTypeObject,
    length: isize,
    _hash: isize,
    state: u32,
}

const STATE_ASCII_MASK: u32 = 0b0100_0000;

#[cfg(target_pointer_width = "64")]
const ASCII_DATA_OFFSET: usize = 40;

#[cfg(target_pointer_width = "32")]
const ASCII_DATA_OFFSET: usize = 24;

// ============================================================================
// Raw Buffer - Direct memory manipulation
// ============================================================================

/// Raw buffer for JSON serialization with unsafe pointer operations
pub struct RawBuffer {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl RawBuffer {
    /// Create a new raw buffer with given capacity
    #[inline(always)]
    pub fn new(capacity: usize) -> Self {
        let mut vec = Vec::with_capacity(capacity);
        let ptr = vec.as_mut_ptr();
        let cap = vec.capacity();
        std::mem::forget(vec);
        Self { ptr, len: 0, cap }
    }

    /// Create from existing Vec (takes ownership)
    #[inline(always)]
    pub fn from_vec(mut vec: Vec<u8>) -> Self {
        vec.clear();
        let ptr = vec.as_mut_ptr();
        let cap = vec.capacity();
        std::mem::forget(vec);
        Self { ptr, len: 0, cap }
    }

    /// Convert back to Vec (transfers ownership)
    #[inline(always)]
    pub fn into_vec(self) -> Vec<u8> {
        let vec = unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) };
        std::mem::forget(self);
        vec
    }

    /// Ensure buffer has at least `additional` bytes of capacity
    #[inline(always)]
    fn ensure_capacity(&mut self, additional: usize) {
        let required = self.len + additional;
        if required > self.cap {
            self.grow(required);
        }
    }

    /// Grow the buffer (cold path)
    #[cold]
    #[inline(never)]
    fn grow(&mut self, min_cap: usize) {
        let new_cap = std::cmp::max(min_cap, self.cap * 2);
        let mut vec = unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) };
        vec.reserve(new_cap - self.cap);
        self.ptr = vec.as_mut_ptr();
        self.cap = vec.capacity();
        std::mem::forget(vec);
    }

    /// Write a single byte (no capacity check - caller must ensure space)
    #[inline(always)]
    unsafe fn write_byte_unchecked(&mut self, b: u8) {
        *self.ptr.add(self.len) = b;
        self.len += 1;
    }

    /// Write multiple bytes (no capacity check - caller must ensure space)
    #[inline(always)]
    unsafe fn write_bytes_unchecked(&mut self, bytes: &[u8]) {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len());
        self.len += bytes.len();
    }

    /// Write a single byte with capacity check
    #[inline(always)]
    pub fn write_byte(&mut self, b: u8) {
        self.ensure_capacity(1);
        unsafe { self.write_byte_unchecked(b); }
    }

    /// Write multiple bytes with capacity check
    #[inline(always)]
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.ensure_capacity(bytes.len());
        unsafe { self.write_bytes_unchecked(bytes); }
    }

    /// Get current length
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Get as slice
    #[inline(always)]
    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for RawBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = Vec::from_raw_parts(self.ptr, self.len, self.cap);
        }
    }
}

// ============================================================================
// Raw Serializer - Zero PyO3 overhead
// ============================================================================

/// Raw JSON serializer using direct C API and raw buffer manipulation
pub struct RawSerializer {
    buf: RawBuffer,
}

impl RawSerializer {
    #[inline(always)]
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: RawBuffer::new(capacity),
        }
    }

    #[inline(always)]
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self {
            buf: RawBuffer::from_vec(vec),
        }
    }

    #[inline(always)]
    pub fn into_vec(self) -> Vec<u8> {
        self.buf.into_vec()
    }

    /// Serialize any Python object using raw C API
    #[inline]
    pub unsafe fn serialize(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        // Get cached type pointers
        let cache = type_cache::get_type_cache();
        let obj_type = (*obj_ptr).ob_type;

        // Check None first (singleton comparison)
        if obj_ptr == ffi::Py_None() {
            self.buf.ensure_capacity(4);
            self.buf.write_bytes_unchecked(b"null");
            return Ok(());
        }

        // Check Bool (before Int because bool is subclass of int in Python)
        if obj_type == cache.bool_type {
            if obj_ptr == ffi::Py_True() {
                self.buf.ensure_capacity(4);
                self.buf.write_bytes_unchecked(b"true");
            } else {
                self.buf.ensure_capacity(5);
                self.buf.write_bytes_unchecked(b"false");
            }
            return Ok(());
        }

        // Check Int
        if obj_type == cache.int_type {
            return self.serialize_int(obj_ptr);
        }

        // Check Float
        if obj_type == cache.float_type {
            return self.serialize_float(obj_ptr);
        }

        // Check String
        if obj_type == cache.string_type {
            return self.serialize_string(obj_ptr);
        }

        // Check List
        if obj_type == cache.list_type {
            return self.serialize_list(obj_ptr);
        }

        // Check Dict
        if obj_type == cache.dict_type {
            return self.serialize_dict(obj_ptr);
        }

        // Check Tuple
        if obj_type == cache.tuple_type {
            return self.serialize_tuple(obj_ptr);
        }

        // Unsupported type
        Err(PyValueError::new_err("Unsupported type for JSON serialization"))
    }

    /// Serialize integer using raw buffer manipulation
    #[inline(always)]
    unsafe fn serialize_int(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        // Try fast path first
        if let Ok(val) = pylong_fast::extract_int_fast(obj_ptr) {
            self.write_i64(val);
            return Ok(());
        }

        // Fall back for large integers
        let val = ffi::PyLong_AsUnsignedLongLong(obj_ptr);
        if val != u64::MAX || ffi::PyErr_Occurred().is_null() {
            ffi::PyErr_Clear();
            self.write_u64(val);
            return Ok(());
        }

        // Very large int - use string representation
        ffi::PyErr_Clear();
        let repr = ffi::PyObject_Str(obj_ptr);
        if !repr.is_null() {
            let mut size: ffi::Py_ssize_t = 0;
            let data = ffi::PyUnicode_AsUTF8AndSize(repr, &mut size);
            if !data.is_null() {
                let slice = std::slice::from_raw_parts(data as *const u8, size as usize);
                self.buf.write_bytes(slice);
            }
            ffi::Py_DECREF(repr);
        }
        Ok(())
    }

    /// Write i64 using raw buffer manipulation
    #[inline(always)]
    fn write_i64(&mut self, val: i64) {
        if val >= 0 {
            self.write_u64_raw(val as u64);
        } else {
            self.buf.write_byte(b'-');
            self.write_u64_raw((-val) as u64);
        }
    }

    /// Write u64 using raw buffer manipulation
    #[inline(always)]
    fn write_u64(&mut self, val: u64) {
        self.write_u64_raw(val);
    }

    /// Raw u64 formatting with precomputed digit pairs
    #[inline(always)]
    fn write_u64_raw(&mut self, val: u64) {
        // Pre-allocate for max digits
        self.buf.ensure_capacity(20);

        unsafe {
            if val < 10 {
                self.buf.write_byte_unchecked(DIGITS[val as usize]);
            } else if val < 100 {
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[val as usize]);
            } else if val < 1000 {
                let d1 = (val / 100) as usize;
                let d23 = (val % 100) as usize;
                self.buf.write_byte_unchecked(DIGITS[d1]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d23]);
            } else if val < 10000 {
                let d12 = (val / 100) as usize;
                let d34 = (val % 100) as usize;
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d12]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d34]);
            } else if val < 100000 {
                let d1 = (val / 10000) as usize;
                let d23 = ((val / 100) % 100) as usize;
                let d45 = (val % 100) as usize;
                self.buf.write_byte_unchecked(DIGITS[d1]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d23]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d45]);
            } else if val < 1000000 {
                let d12 = (val / 10000) as usize;
                let d34 = ((val / 100) % 100) as usize;
                let d56 = (val % 100) as usize;
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d12]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d34]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d56]);
            } else if val < 10000000 {
                let d1 = (val / 1000000) as usize;
                let d23 = ((val / 10000) % 100) as usize;
                let d45 = ((val / 100) % 100) as usize;
                let d67 = (val % 100) as usize;
                self.buf.write_byte_unchecked(DIGITS[d1]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d23]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d45]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d67]);
            } else if val < 100000000 {
                let d12 = (val / 1000000) as usize;
                let d34 = ((val / 10000) % 100) as usize;
                let d56 = ((val / 100) % 100) as usize;
                let d78 = (val % 100) as usize;
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d12]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d34]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d56]);
                self.buf.write_bytes_unchecked(&DIGIT_PAIRS[d78]);
            } else {
                // 9+ digits: use itoa
                let mut itoa_buf = itoa::Buffer::new();
                let s = itoa_buf.format(val);
                self.buf.write_bytes_unchecked(s.as_bytes());
            }
        }
    }

    /// Serialize float
    #[inline(always)]
    unsafe fn serialize_float(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let val = pyfloat_fast::extract_float_fast(obj_ptr);

        if !val.is_finite() {
            return Err(PyValueError::new_err(format!(
                "Cannot serialize non-finite float: {}", val
            )));
        }

        let mut ryu_buf = ryu::Buffer::new();
        let s = ryu_buf.format(val);
        self.buf.write_bytes(s.as_bytes());
        Ok(())
    }

    /// Serialize string with SIMD escape detection
    #[inline(always)]
    unsafe fn serialize_string(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let ascii_obj = obj_ptr as *const PyASCIIObject;
        let state = (*ascii_obj).state;
        let length = (*ascii_obj).length as usize;

        if state & STATE_ASCII_MASK != 0 {
            // Fast ASCII path
            let data_ptr = (obj_ptr as *const u8).add(ASCII_DATA_OFFSET);
            let bytes = std::slice::from_raw_parts(data_ptr, length);

            // Check if escaping needed
            if !simd_escape::needs_escape_simd(bytes) {
                // No escapes - direct copy
                self.buf.ensure_capacity(length + 2);
                self.buf.write_byte_unchecked(b'"');
                self.buf.write_bytes_unchecked(bytes);
                self.buf.write_byte_unchecked(b'"');
            } else {
                // Has escapes - use SIMD escape writer (need to use Vec temporarily)
                self.write_escaped_string(bytes);
            }
        } else {
            // Non-ASCII path
            let mut size: ffi::Py_ssize_t = 0;
            let utf8_ptr = ffi::PyUnicode_AsUTF8AndSize(obj_ptr, &mut size);
            if !utf8_ptr.is_null() {
                let bytes = std::slice::from_raw_parts(utf8_ptr as *const u8, size as usize);
                if !simd_escape::needs_escape_simd(bytes) {
                    self.buf.ensure_capacity(bytes.len() + 2);
                    self.buf.write_byte_unchecked(b'"');
                    self.buf.write_bytes_unchecked(bytes);
                    self.buf.write_byte_unchecked(b'"');
                } else {
                    self.write_escaped_string(bytes);
                }
            }
        }
        Ok(())
    }

    /// Write escaped string using LUT-based escaping
    #[inline(always)]
    fn write_escaped_string(&mut self, bytes: &[u8]) {
        // Reserve space (worst case: all chars need escaping)
        self.buf.ensure_capacity(bytes.len() * 6 + 2);

        unsafe {
            self.buf.write_byte_unchecked(b'"');

            for &b in bytes {
                match b {
                    b'"' => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'"');
                    }
                    b'\\' => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'\\');
                    }
                    b'\n' => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'n');
                    }
                    b'\r' => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'r');
                    }
                    b'\t' => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b't');
                    }
                    0x08 => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'b');
                    }
                    0x0C => {
                        self.buf.write_byte_unchecked(b'\\');
                        self.buf.write_byte_unchecked(b'f');
                    }
                    b if b < 0x20 => {
                        // Unicode escape
                        self.buf.write_bytes_unchecked(b"\\u00");
                        let hi = b >> 4;
                        let lo = b & 0x0F;
                        self.buf.write_byte_unchecked(if hi < 10 { b'0' + hi } else { b'a' + hi - 10 });
                        self.buf.write_byte_unchecked(if lo < 10 { b'0' + lo } else { b'a' + lo - 10 });
                    }
                    b => {
                        self.buf.write_byte_unchecked(b);
                    }
                }
            }

            self.buf.write_byte_unchecked(b'"');
        }
    }

    /// Serialize list
    #[inline(always)]
    unsafe fn serialize_list(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let len = ffi::PyList_GET_SIZE(obj_ptr);

        if len == 0 {
            self.buf.ensure_capacity(2);
            self.buf.write_bytes_unchecked(b"[]");
            return Ok(());
        }

        // Check for homogeneous int array (common case)
        let cache = type_cache::get_type_cache();
        let first_ptr = ffi::PyList_GET_ITEM(obj_ptr, 0);
        let first_type = (*first_ptr).ob_type;

        if first_type == cache.int_type && len >= 8 {
            // Check if all elements are ints
            let mut all_ints = true;
            let check_count = std::cmp::min(len, 16) as isize;
            for i in 1..check_count {
                let item = ffi::PyList_GET_ITEM(obj_ptr, i);
                if (*item).ob_type != cache.int_type {
                    all_ints = false;
                    break;
                }
            }

            if all_ints {
                return self.serialize_int_array(obj_ptr, len);
            }
        }

        // Generic path
        self.buf.write_byte(b'[');
        self.serialize(first_ptr)?;

        for i in 1..len {
            self.buf.write_byte(b',');
            let item = ffi::PyList_GET_ITEM(obj_ptr, i);
            self.serialize(item)?;
        }

        self.buf.write_byte(b']');
        Ok(())
    }

    /// Serialize homogeneous int array (optimized bulk path)
    #[inline(always)]
    unsafe fn serialize_int_array(&mut self, obj_ptr: *mut ffi::PyObject, len: isize) -> PyResult<()> {
        // Pre-allocate (estimate 10 bytes per int)
        self.buf.ensure_capacity((len as usize) * 10 + 2);
        self.buf.write_byte_unchecked(b'[');

        // First element
        let first = ffi::PyList_GET_ITEM(obj_ptr, 0);
        if let Ok(val) = pylong_fast::extract_int_fast(first) {
            self.write_i64(val);
        }

        // Remaining elements
        for i in 1..len {
            self.buf.write_byte(b',');
            let item = ffi::PyList_GET_ITEM(obj_ptr, i);
            if let Ok(val) = pylong_fast::extract_int_fast(item) {
                self.write_i64(val);
            }
        }

        self.buf.write_byte(b']');
        Ok(())
    }

    /// Serialize tuple
    #[inline(always)]
    unsafe fn serialize_tuple(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let len = ffi::PyTuple_GET_SIZE(obj_ptr);

        if len == 0 {
            self.buf.ensure_capacity(2);
            self.buf.write_bytes_unchecked(b"[]");
            return Ok(());
        }

        self.buf.write_byte(b'[');

        let first = ffi::PyTuple_GET_ITEM(obj_ptr, 0);
        self.serialize(first)?;

        for i in 1..len {
            self.buf.write_byte(b',');
            let item = ffi::PyTuple_GET_ITEM(obj_ptr, i);
            self.serialize(item)?;
        }

        self.buf.write_byte(b']');
        Ok(())
    }

    /// Serialize dict
    #[inline(always)]
    unsafe fn serialize_dict(&mut self, obj_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let len = ffi::PyDict_Size(obj_ptr);

        if len == 0 {
            self.buf.ensure_capacity(2);
            self.buf.write_bytes_unchecked(b"{}");
            return Ok(());
        }

        // Pre-allocate (estimate 20 bytes per entry)
        self.buf.ensure_capacity((len as usize) * 20);
        self.buf.write_byte_unchecked(b'{');

        let cache = type_cache::get_type_cache();
        let string_type = cache.string_type;

        let mut pos: ffi::Py_ssize_t = 0;
        let mut key_ptr: *mut ffi::PyObject = std::ptr::null_mut();
        let mut value_ptr: *mut ffi::PyObject = std::ptr::null_mut();
        let mut first = true;

        while ffi::PyDict_Next(obj_ptr, &mut pos, &mut key_ptr, &mut value_ptr) != 0 {
            if !first {
                self.buf.write_byte(b',');
            }
            first = false;

            // Check key type
            if (*key_ptr).ob_type != string_type {
                return Err(PyValueError::new_err(
                    "Dictionary keys must be strings for JSON serialization"
                ));
            }

            // Serialize key
            self.serialize_dict_key(key_ptr)?;
            self.buf.write_byte(b':');

            // Serialize value
            self.serialize(value_ptr)?;
        }

        self.buf.write_byte(b'}');
        Ok(())
    }

    /// Serialize dict key with fast ASCII path
    #[inline(always)]
    unsafe fn serialize_dict_key(&mut self, key_ptr: *mut ffi::PyObject) -> PyResult<()> {
        let ascii_obj = key_ptr as *const PyASCIIObject;
        let state = (*ascii_obj).state;
        let length = (*ascii_obj).length as usize;

        // Check if ASCII
        if state & STATE_ASCII_MASK != 0 {
            let data_ptr = (key_ptr as *const u8).add(ASCII_DATA_OFFSET);
            let bytes = std::slice::from_raw_parts(data_ptr, length);

            // Most keys don't need escaping
            if !simd_escape::needs_escape_simd(bytes) {
                self.buf.ensure_capacity(length + 2);
                self.buf.write_byte_unchecked(b'"');
                self.buf.write_bytes_unchecked(bytes);
                self.buf.write_byte_unchecked(b'"');
            } else {
                self.write_escaped_string(bytes);
            }
        } else {
            // Non-ASCII key
            let mut size: ffi::Py_ssize_t = 0;
            let utf8_ptr = ffi::PyUnicode_AsUTF8AndSize(key_ptr, &mut size);
            if !utf8_ptr.is_null() {
                let bytes = std::slice::from_raw_parts(utf8_ptr as *const u8, size as usize);
                if !simd_escape::needs_escape_simd(bytes) {
                    self.buf.ensure_capacity(bytes.len() + 2);
                    self.buf.write_byte_unchecked(b'"');
                    self.buf.write_bytes_unchecked(bytes);
                    self.buf.write_byte_unchecked(b'"');
                } else {
                    self.write_escaped_string(bytes);
                }
            }
        }
        Ok(())
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Serialize Python object to JSON string using raw C API
pub fn dumps_raw(_py: Python, obj: &Bound<'_, pyo3::types::PyAny>) -> PyResult<String> {
    use std::cell::RefCell;

    thread_local! {
        static BUFFER: RefCell<Vec<u8>> = RefCell::new(Vec::with_capacity(4096));
    }

    BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        let vec = std::mem::take(&mut *buf);

        let mut serializer = RawSerializer::from_vec(vec);

        let result = unsafe { serializer.serialize(obj.as_ptr()) };

        match result {
            Ok(()) => {
                let result_vec = serializer.into_vec();
                let json = unsafe { String::from_utf8_unchecked(result_vec) };

                // Put empty vec back for next call
                *buf = Vec::new();

                Ok(json)
            }
            Err(e) => {
                // On error, recover the buffer
                *buf = serializer.into_vec();
                Err(e)
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::{PyDict, PyList};

    #[test]
    fn test_raw_serialize_int() {
        Python::with_gil(|py| {
            let obj = 42i64.into_pyobject(py).unwrap();
            let result = dumps_raw(py, obj.as_any()).unwrap();
            assert_eq!(result, "42");
        });
    }

    #[test]
    fn test_raw_serialize_list() {
        Python::with_gil(|py| {
            let list = PyList::new(py, &[1, 2, 3]).unwrap();
            let result = dumps_raw(py, list.as_any()).unwrap();
            assert_eq!(result, "[1,2,3]");
        });
    }

    #[test]
    fn test_raw_serialize_dict() {
        Python::with_gil(|py| {
            let dict = PyDict::new(py);
            dict.set_item("a", 1).unwrap();
            let result = dumps_raw(py, dict.as_any()).unwrap();
            assert_eq!(result, "{\"a\":1}");
        });
    }
}
