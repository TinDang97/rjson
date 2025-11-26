//! Phase 21: Raw C API JSON Parser (Zero PyO3 Overhead)
//!
//! This module implements the fastest possible JSON parser by:
//! - Using raw *mut ffi::PyObject pointers (no PyO3 wrapper overhead)
//! - Direct CPython C API calls (no abstraction layers)
//! - Zero intermediate allocations where possible
//! - Inline everything for maximum performance
//!
//! WARNING: This is highly unsafe code. Use with caution.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;

use super::simd_parser::get_interned_string;
use super::object_cache;

// ============================================================================
// Character Classification (same as custom_parser but kept local for inlining)
// ============================================================================

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CharType {
    Invalid = 0,
    Whitespace = 1,
    Quote = 2,
    NumberStart = 3,
    TrueStart = 4,
    FalseStart = 5,
    NullStart = 6,
    ArrayStart = 7,
    ArrayEnd = 8,
    ObjectStart = 9,
    ObjectEnd = 10,
    Colon = 11,
    Comma = 12,
    Other = 13,
}

static CHAR_TYPE: [CharType; 256] = {
    let mut table = [CharType::Other; 256];
    table[b' ' as usize] = CharType::Whitespace;
    table[b'\t' as usize] = CharType::Whitespace;
    table[b'\n' as usize] = CharType::Whitespace;
    table[b'\r' as usize] = CharType::Whitespace;
    table[b'"' as usize] = CharType::Quote;
    table[b'[' as usize] = CharType::ArrayStart;
    table[b']' as usize] = CharType::ArrayEnd;
    table[b'{' as usize] = CharType::ObjectStart;
    table[b'}' as usize] = CharType::ObjectEnd;
    table[b':' as usize] = CharType::Colon;
    table[b',' as usize] = CharType::Comma;
    table[b'-' as usize] = CharType::NumberStart;
    table[b'0' as usize] = CharType::NumberStart;
    table[b'1' as usize] = CharType::NumberStart;
    table[b'2' as usize] = CharType::NumberStart;
    table[b'3' as usize] = CharType::NumberStart;
    table[b'4' as usize] = CharType::NumberStart;
    table[b'5' as usize] = CharType::NumberStart;
    table[b'6' as usize] = CharType::NumberStart;
    table[b'7' as usize] = CharType::NumberStart;
    table[b'8' as usize] = CharType::NumberStart;
    table[b'9' as usize] = CharType::NumberStart;
    table[b't' as usize] = CharType::TrueStart;
    table[b'f' as usize] = CharType::FalseStart;
    table[b'n' as usize] = CharType::NullStart;
    let mut i = 0u8;
    while i < 0x20 {
        if i != b' ' && i != b'\t' && i != b'\n' && i != b'\r' {
            table[i as usize] = CharType::Invalid;
        }
        i += 1;
    }
    table
};

// ============================================================================
// Raw Parser - No PyO3 Overhead
// ============================================================================

/// Raw JSON parser using direct C API calls
pub struct RawJsonParser<'a, 'py> {
    input: &'a [u8],
    pos: usize,
    py: Python<'py>,
}

impl<'a, 'py> RawJsonParser<'a, 'py> {
    #[inline(always)]
    pub fn new(py: Python<'py>, input: &'a [u8]) -> Self {
        Self { input, pos: 0, py }
    }

    /// Parse JSON and return raw PyObject pointer
    /// Caller is responsible for reference counting
    #[inline]
    pub unsafe fn parse(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        self.skip_whitespace();
        let result = self.parse_value()?;
        self.skip_whitespace();

        if self.pos < self.input.len() {
            // Need to decref result before returning error
            ffi::Py_DECREF(result);
            return Err("Unexpected data after JSON value");
        }

        Ok(result)
    }

    #[inline(always)]
    fn skip_whitespace(&mut self) {
        // Direct comparison is faster than table lookup for simple whitespace
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c != b' ' && c != b'\n' && c != b'\r' && c != b'\t' {
                break;
            }
            self.pos += 1;
        }
    }

    #[inline]
    unsafe fn parse_value(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        if self.pos >= self.input.len() {
            return Err("Unexpected end of input");
        }

        let c = self.input[self.pos];
        match CHAR_TYPE[c as usize] {
            CharType::Quote => self.parse_string(),
            CharType::NumberStart => self.parse_number(),
            CharType::TrueStart => self.parse_true(),
            CharType::FalseStart => self.parse_false(),
            CharType::NullStart => self.parse_null(),
            CharType::ArrayStart => self.parse_array(),
            CharType::ObjectStart => self.parse_object(),
            _ => Err("Unexpected character"),
        }
    }

    #[inline(always)]
    unsafe fn parse_null(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        if self.pos + 4 <= self.input.len()
            && self.input[self.pos] == b'n'
            && self.input[self.pos + 1] == b'u'
            && self.input[self.pos + 2] == b'l'
            && self.input[self.pos + 3] == b'l'
        {
            self.pos += 4;
            // Use cached None (returns owned reference)
            Ok(object_cache::get_none(self.py).into_ptr())
        } else {
            Err("Invalid literal, expected 'null'")
        }
    }

    #[inline(always)]
    unsafe fn parse_true(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        if self.pos + 4 <= self.input.len()
            && self.input[self.pos] == b't'
            && self.input[self.pos + 1] == b'r'
            && self.input[self.pos + 2] == b'u'
            && self.input[self.pos + 3] == b'e'
        {
            self.pos += 4;
            // Use cached True (returns owned reference)
            Ok(object_cache::get_bool(self.py, true).into_ptr())
        } else {
            Err("Invalid literal, expected 'true'")
        }
    }

    #[inline(always)]
    unsafe fn parse_false(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        if self.pos + 5 <= self.input.len()
            && self.input[self.pos] == b'f'
            && self.input[self.pos + 1] == b'a'
            && self.input[self.pos + 2] == b'l'
            && self.input[self.pos + 3] == b's'
            && self.input[self.pos + 4] == b'e'
        {
            self.pos += 5;
            // Use cached False (returns owned reference)
            Ok(object_cache::get_bool(self.py, false).into_ptr())
        } else {
            Err("Invalid literal, expected 'false'")
        }
    }

    #[inline]
    unsafe fn parse_number(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        let start = self.pos;
        let mut is_float = false;
        let mut is_negative = false;

        if self.pos < self.input.len() && self.input[self.pos] == b'-' {
            is_negative = true;
            self.pos += 1;
        }

        let int_start = self.pos;

        // Parse integer part
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c < b'0' || c > b'9' {
                break;
            }
            self.pos += 1;
        }

        // Check for decimal point
        if self.pos < self.input.len() && self.input[self.pos] == b'.' {
            is_float = true;
            self.pos += 1;
            while self.pos < self.input.len() {
                let c = self.input[self.pos];
                if c < b'0' || c > b'9' {
                    break;
                }
                self.pos += 1;
            }
        }

        // Check for exponent
        if self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'e' || c == b'E' {
                is_float = true;
                self.pos += 1;
                if self.pos < self.input.len() {
                    let sign = self.input[self.pos];
                    if sign == b'+' || sign == b'-' {
                        self.pos += 1;
                    }
                }
                while self.pos < self.input.len() {
                    let c = self.input[self.pos];
                    if c < b'0' || c > b'9' {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }

        if is_float {
            // Parse as float using direct C API
            let num_str = std::str::from_utf8_unchecked(&self.input[start..self.pos]);
            match num_str.parse::<f64>() {
                Ok(f) if f.is_finite() => Ok(object_cache::create_float_direct(f)),
                _ => Err("Invalid float"),
            }
        } else {
            // Fast integer parsing
            let int_len = self.pos - int_start;

            if int_len <= 18 {
                // Fast path: inline integer accumulation
                let mut value: u64 = 0;
                for i in int_start..self.pos {
                    value = value * 10 + (self.input[i] - b'0') as u64;
                }

                if is_negative {
                    if value <= 9223372036854775808 {
                        let signed = -(value as i64);
                        // Use cache for small integers
                        if signed >= -256 {
                            Ok(object_cache::get_int(self.py, signed).into_ptr())
                        } else {
                            Ok(object_cache::create_int_i64_direct(signed))
                        }
                    } else {
                        Err("Integer overflow")
                    }
                } else if value <= 256 {
                    // Use cache for small positive integers
                    Ok(object_cache::get_int(self.py, value as i64).into_ptr())
                } else if value <= i64::MAX as u64 {
                    Ok(object_cache::create_int_i64_direct(value as i64))
                } else {
                    Ok(object_cache::create_int_u64_direct(value))
                }
            } else {
                // Large integer - use string parsing
                let num_str = std::str::from_utf8_unchecked(&self.input[start..self.pos]);
                match num_str.parse::<i64>() {
                    Ok(n) => Ok(object_cache::create_int_i64_direct(n)),
                    Err(_) => match num_str.parse::<u64>() {
                        Ok(n) => Ok(object_cache::create_int_u64_direct(n)),
                        Err(_) => Err("Integer too large"),
                    },
                }
            }
        }
    }

    #[inline]
    unsafe fn parse_string(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        self.pos += 1; // Skip opening quote
        let start = self.pos;

        // Fast scan for end of string (no escapes)
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'"' {
                // Fast path: no escapes
                let len = self.pos - start;
                let ptr = self.input.as_ptr().add(start) as *const i8;
                self.pos += 1;
                return Ok(ffi::PyUnicode_FromStringAndSize(ptr, len as ffi::Py_ssize_t));
            } else if c == b'\\' {
                // Has escapes - use slow path
                return self.parse_string_with_escapes(start);
            } else if c < 0x20 {
                return Err("Invalid control character in string");
            }
            self.pos += 1;
        }

        Err("Unterminated string")
    }

    /// Parse a string as a dict key with interning support
    /// Returns the interned Python string object
    #[inline]
    unsafe fn parse_key_interned(&mut self) -> Result<PyObject, &'static str> {
        self.pos += 1; // Skip opening quote
        let start = self.pos;

        // Fast scan for end of string (no escapes)
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'"' {
                // Fast path: no escapes - use string interning
                let key_str = std::str::from_utf8_unchecked(&self.input[start..self.pos]);
                self.pos += 1;
                return Ok(get_interned_string(self.py, key_str));
            } else if c == b'\\' {
                // Has escapes - decode and create without interning
                return self.parse_key_with_escapes(start);
            } else if c < 0x20 {
                return Err("Invalid control character in string");
            }
            self.pos += 1;
        }

        Err("Unterminated string")
    }

    #[cold]
    unsafe fn parse_key_with_escapes(&mut self, start: usize) -> Result<PyObject, &'static str> {
        // Reset position to start and decode with escapes
        self.pos = start;
        let mut result = Vec::with_capacity(32);

        while self.pos < self.input.len() {
            let c = self.input[self.pos];

            if c == b'"' {
                self.pos += 1;
                let key_str = std::str::from_utf8_unchecked(&result);
                return Ok(get_interned_string(self.py, key_str));
            } else if c == b'\\' {
                self.pos += 1;
                if self.pos >= self.input.len() {
                    return Err("Unterminated escape");
                }

                let escaped = self.input[self.pos];
                self.pos += 1;

                match escaped {
                    b'"' => result.push(b'"'),
                    b'\\' => result.push(b'\\'),
                    b'/' => result.push(b'/'),
                    b'b' => result.push(0x08),
                    b'f' => result.push(0x0C),
                    b'n' => result.push(b'\n'),
                    b'r' => result.push(b'\r'),
                    b't' => result.push(b'\t'),
                    b'u' => {
                        if self.pos + 4 > self.input.len() {
                            return Err("Invalid unicode escape");
                        }
                        let hex = std::str::from_utf8_unchecked(&self.input[self.pos..self.pos + 4]);
                        self.pos += 4;

                        let code = match u16::from_str_radix(hex, 16) {
                            Ok(c) => c,
                            Err(_) => return Err("Invalid unicode escape"),
                        };

                        if let Some(ch) = char::from_u32(code as u32) {
                            let mut buf = [0u8; 4];
                            let s = ch.encode_utf8(&mut buf);
                            result.extend_from_slice(s.as_bytes());
                        } else {
                            return Err("Invalid unicode code point");
                        }
                    }
                    _ => return Err("Invalid escape character"),
                }
            } else {
                result.push(c);
                self.pos += 1;
            }
        }

        Err("Unterminated string")
    }

    #[cold]
    unsafe fn parse_string_with_escapes(&mut self, start: usize) -> Result<*mut ffi::PyObject, &'static str> {
        // Reset position to start and decode with escapes
        self.pos = start;
        let mut result = Vec::with_capacity(64);

        while self.pos < self.input.len() {
            let c = self.input[self.pos];

            if c == b'"' {
                self.pos += 1;
                let ptr = result.as_ptr() as *const i8;
                let len = result.len() as ffi::Py_ssize_t;
                return Ok(ffi::PyUnicode_FromStringAndSize(ptr, len));
            } else if c == b'\\' {
                self.pos += 1;
                if self.pos >= self.input.len() {
                    return Err("Unterminated escape");
                }

                let escaped = self.input[self.pos];
                self.pos += 1;

                match escaped {
                    b'"' => result.push(b'"'),
                    b'\\' => result.push(b'\\'),
                    b'/' => result.push(b'/'),
                    b'b' => result.push(0x08),
                    b'f' => result.push(0x0C),
                    b'n' => result.push(b'\n'),
                    b'r' => result.push(b'\r'),
                    b't' => result.push(b'\t'),
                    b'u' => {
                        if self.pos + 4 > self.input.len() {
                            return Err("Invalid unicode escape");
                        }
                        let hex = std::str::from_utf8_unchecked(&self.input[self.pos..self.pos + 4]);
                        self.pos += 4;

                        let code = match u16::from_str_radix(hex, 16) {
                            Ok(c) => c,
                            Err(_) => return Err("Invalid unicode escape"),
                        };

                        // Handle surrogate pairs
                        if (0xD800..=0xDBFF).contains(&code) {
                            if self.pos + 6 <= self.input.len()
                                && self.input[self.pos] == b'\\'
                                && self.input[self.pos + 1] == b'u'
                            {
                                self.pos += 2;
                                let hex2 = std::str::from_utf8_unchecked(&self.input[self.pos..self.pos + 4]);
                                self.pos += 4;

                                let code2 = match u16::from_str_radix(hex2, 16) {
                                    Ok(c) => c,
                                    Err(_) => return Err("Invalid unicode escape"),
                                };

                                if (0xDC00..=0xDFFF).contains(&code2) {
                                    let combined = 0x10000
                                        + ((code as u32 - 0xD800) << 10)
                                        + (code2 as u32 - 0xDC00);
                                    let ch = char::from_u32(combined).ok_or("Invalid surrogate pair")?;
                                    let mut buf = [0u8; 4];
                                    let s = ch.encode_utf8(&mut buf);
                                    result.extend_from_slice(s.as_bytes());
                                } else {
                                    return Err("Invalid surrogate pair");
                                }
                            } else {
                                return Err("Lone surrogate");
                            }
                        } else if let Some(ch) = char::from_u32(code as u32) {
                            let mut buf = [0u8; 4];
                            let s = ch.encode_utf8(&mut buf);
                            result.extend_from_slice(s.as_bytes());
                        } else {
                            return Err("Invalid unicode code point");
                        }
                    }
                    _ => return Err("Invalid escape character"),
                }
            } else {
                result.push(c);
                self.pos += 1;
            }
        }

        Err("Unterminated string")
    }

    #[inline]
    unsafe fn parse_array(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        self.pos += 1; // Skip '['
        self.skip_whitespace();

        // Empty array
        if self.pos < self.input.len() && self.input[self.pos] == b']' {
            self.pos += 1;
            return Ok(ffi::PyList_New(0));
        }

        // Collect elements into a Vec first, then create list with exact size
        // Use larger initial capacity to reduce reallocations for large arrays
        let mut elements: Vec<*mut ffi::PyObject> = Vec::with_capacity(128);

        loop {
            self.skip_whitespace();

            let elem = match self.parse_value() {
                Ok(e) => e,
                Err(e) => {
                    // Cleanup on error
                    for ptr in &elements {
                        ffi::Py_DECREF(*ptr);
                    }
                    return Err(e);
                }
            };

            elements.push(elem);

            self.skip_whitespace();

            if self.pos >= self.input.len() {
                for ptr in &elements {
                    ffi::Py_DECREF(*ptr);
                }
                return Err("Unterminated array");
            }

            let c = self.input[self.pos];
            if c == b']' {
                self.pos += 1;
                break;
            } else if c == b',' {
                self.pos += 1;
            } else {
                for ptr in &elements {
                    ffi::Py_DECREF(*ptr);
                }
                return Err("Expected ',' or ']'");
            }
        }

        // Create list with exact size and use SET_ITEM for all elements
        let len = elements.len();
        let list = ffi::PyList_New(len as ffi::Py_ssize_t);
        if list.is_null() {
            for ptr in &elements {
                ffi::Py_DECREF(*ptr);
            }
            return Err("Failed to create list");
        }

        for (i, elem) in elements.into_iter().enumerate() {
            // PyList_SET_ITEM steals the reference
            ffi::PyList_SET_ITEM(list, i as ffi::Py_ssize_t, elem);
        }

        Ok(list)
    }

    #[inline]
    unsafe fn parse_object(&mut self) -> Result<*mut ffi::PyObject, &'static str> {
        self.pos += 1; // Skip '{'
        self.skip_whitespace();

        let dict = ffi::PyDict_New();
        if dict.is_null() {
            return Err("Failed to create dict");
        }

        // Empty object
        if self.pos < self.input.len() && self.input[self.pos] == b'}' {
            self.pos += 1;
            return Ok(dict);
        }

        loop {
            self.skip_whitespace();

            // Parse key with interning
            if self.pos >= self.input.len() || self.input[self.pos] != b'"' {
                ffi::Py_DECREF(dict);
                return Err("Expected string key");
            }

            let key = match self.parse_key_interned() {
                Ok(k) => k,
                Err(e) => {
                    ffi::Py_DECREF(dict);
                    return Err(e);
                }
            };

            self.skip_whitespace();

            // Expect colon
            if self.pos >= self.input.len() || self.input[self.pos] != b':' {
                // key is a PyObject, drop it
                drop(key);
                ffi::Py_DECREF(dict);
                return Err("Expected ':'");
            }
            self.pos += 1;

            self.skip_whitespace();

            // Parse value
            let value = match self.parse_value() {
                Ok(v) => v,
                Err(e) => {
                    drop(key);
                    ffi::Py_DECREF(dict);
                    return Err(e);
                }
            };

            // Insert into dict (does NOT steal references)
            let result = ffi::PyDict_SetItem(dict, key.as_ptr(), value);
            // key will be dropped automatically
            ffi::Py_DECREF(value);

            if result < 0 {
                ffi::Py_DECREF(dict);
                return Err("Failed to set dict item");
            }

            self.skip_whitespace();

            if self.pos >= self.input.len() {
                ffi::Py_DECREF(dict);
                return Err("Unterminated object");
            }

            let c = self.input[self.pos];
            if c == b'}' {
                self.pos += 1;
                break;
            } else if c == b',' {
                self.pos += 1;
            } else {
                ffi::Py_DECREF(dict);
                return Err("Expected ',' or '}'");
            }
        }

        Ok(dict)
    }
}

/// Public entry point for raw JSON parsing
#[inline]
pub fn loads_raw(json_str: &str) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let mut parser = RawJsonParser::new(py, json_str.as_bytes());
        unsafe {
            match parser.parse() {
                Ok(ptr) => Ok(PyObject::from_owned_ptr(py, ptr)),
                Err(msg) => Err(PyValueError::new_err(format!("JSON parsing error: {}", msg))),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::{PyList, PyDict};

    #[test]
    fn test_raw_parse_basic() {
        Python::with_gil(|py| {
            // null
            let result = loads_raw("null").unwrap();
            assert!(result.bind(py).is_none());

            // bool
            let result = loads_raw("true").unwrap();
            assert!(result.bind(py).extract::<bool>().unwrap());

            // number
            let result = loads_raw("42").unwrap();
            assert_eq!(result.bind(py).extract::<i64>().unwrap(), 42);

            // string
            let result = loads_raw("\"hello\"").unwrap();
            assert_eq!(result.bind(py).extract::<String>().unwrap(), "hello");

            // array
            let result = loads_raw("[1, 2, 3]").unwrap();
            let list = result.bind(py).downcast::<PyList>().unwrap();
            assert_eq!(list.len(), 3);

            // object
            let result = loads_raw("{\"a\": 1}").unwrap();
            let dict = result.bind(py).downcast::<PyDict>().unwrap();
            assert_eq!(dict.len(), 1);
        });
    }
}
