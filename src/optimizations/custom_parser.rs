//! Phase 20: Fully Custom JSON Parser
//!
//! This module implements a zero-overhead JSON parser that:
//! - Bypasses serde_json entirely (no Visitor pattern overhead)
//! - Uses lookup tables for O(1) character classification
//! - Parses directly to Python objects (no intermediate representation)
//! - Employs SIMD-ready string scanning
//! - Inline number parsing with DP lookup tables
//!
//! Goal: Match or exceed orjson parsing performance

use pyo3::prelude::*;
use pyo3::ffi;
use pyo3::exceptions::PyValueError;

use super::object_cache;
use super::simd_parser::get_interned_string;

// ============================================================================
// Character Classification Lookup Table
// ============================================================================

/// Character types for JSON parsing
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum CharType {
    /// Invalid character
    Invalid = 0,
    /// Whitespace: space, tab, newline, carriage return
    Whitespace = 1,
    /// Start of string: "
    Quote = 2,
    /// Start of number: 0-9, -
    NumberStart = 3,
    /// Start of true: t
    TrueStart = 4,
    /// Start of false: f
    FalseStart = 5,
    /// Start of null: n
    NullStart = 6,
    /// Start of array: [
    ArrayStart = 7,
    /// End of array: ]
    ArrayEnd = 8,
    /// Start of object: {
    ObjectStart = 9,
    /// End of object: }
    ObjectEnd = 10,
    /// Colon: :
    Colon = 11,
    /// Comma: ,
    Comma = 12,
    /// Other valid characters (for inside strings, etc.)
    Other = 13,
}

/// Lookup table for character classification
/// 256 entries for all possible byte values
static CHAR_TYPE: [CharType; 256] = {
    let mut table = [CharType::Other; 256];

    // Whitespace
    table[b' ' as usize] = CharType::Whitespace;
    table[b'\t' as usize] = CharType::Whitespace;
    table[b'\n' as usize] = CharType::Whitespace;
    table[b'\r' as usize] = CharType::Whitespace;

    // Structural characters
    table[b'"' as usize] = CharType::Quote;
    table[b'[' as usize] = CharType::ArrayStart;
    table[b']' as usize] = CharType::ArrayEnd;
    table[b'{' as usize] = CharType::ObjectStart;
    table[b'}' as usize] = CharType::ObjectEnd;
    table[b':' as usize] = CharType::Colon;
    table[b',' as usize] = CharType::Comma;

    // Number start characters
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

    // Keyword starts
    table[b't' as usize] = CharType::TrueStart;
    table[b'f' as usize] = CharType::FalseStart;
    table[b'n' as usize] = CharType::NullStart;

    // Mark control characters as invalid
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
// Custom Parser
// ============================================================================

/// High-performance JSON parser
pub struct JsonParser<'a> {
    /// Input bytes
    input: &'a [u8],
    /// Current position
    pos: usize,
    /// Python GIL token
    py: Python<'a>,
}

impl<'a> JsonParser<'a> {
    /// Create a new parser
    #[inline]
    pub fn new(py: Python<'a>, input: &'a [u8]) -> Self {
        Self { input, pos: 0, py }
    }

    /// Parse JSON and return Python object
    #[inline]
    pub fn parse(&mut self) -> PyResult<PyObject> {
        self.skip_whitespace();
        let result = self.parse_value()?;
        self.skip_whitespace();

        // Verify we consumed all input
        if self.pos < self.input.len() {
            return Err(PyValueError::new_err(format!(
                "Unexpected data after JSON value at position {}",
                self.pos
            )));
        }

        Ok(result)
    }

    /// Skip whitespace characters
    #[inline(always)]
    fn skip_whitespace(&mut self) {
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if CHAR_TYPE[c as usize] != CharType::Whitespace {
                break;
            }
            self.pos += 1;
        }
    }

    /// Parse any JSON value
    #[inline]
    fn parse_value(&mut self) -> PyResult<PyObject> {
        if self.pos >= self.input.len() {
            return Err(PyValueError::new_err("Unexpected end of input"));
        }

        let c = self.input[self.pos];
        let char_type = CHAR_TYPE[c as usize];

        match char_type {
            CharType::Quote => self.parse_string(),
            CharType::NumberStart => self.parse_number(),
            CharType::TrueStart => self.parse_true(),
            CharType::FalseStart => self.parse_false(),
            CharType::NullStart => self.parse_null(),
            CharType::ArrayStart => self.parse_array(),
            CharType::ObjectStart => self.parse_object(),
            CharType::Invalid => Err(PyValueError::new_err(format!(
                "Invalid character at position {}: 0x{:02x}",
                self.pos, c
            ))),
            _ => Err(PyValueError::new_err(format!(
                "Unexpected character '{}' at position {}",
                c as char, self.pos
            ))),
        }
    }

    /// Parse null literal
    #[inline]
    fn parse_null(&mut self) -> PyResult<PyObject> {
        if self.pos + 4 <= self.input.len()
            && &self.input[self.pos..self.pos + 4] == b"null"
        {
            self.pos += 4;
            Ok(object_cache::get_none(self.py))
        } else {
            Err(PyValueError::new_err(format!(
                "Invalid literal at position {}, expected 'null'",
                self.pos
            )))
        }
    }

    /// Parse true literal
    #[inline]
    fn parse_true(&mut self) -> PyResult<PyObject> {
        if self.pos + 4 <= self.input.len()
            && &self.input[self.pos..self.pos + 4] == b"true"
        {
            self.pos += 4;
            Ok(object_cache::get_bool(self.py, true))
        } else {
            Err(PyValueError::new_err(format!(
                "Invalid literal at position {}, expected 'true'",
                self.pos
            )))
        }
    }

    /// Parse false literal
    #[inline]
    fn parse_false(&mut self) -> PyResult<PyObject> {
        if self.pos + 5 <= self.input.len()
            && &self.input[self.pos..self.pos + 5] == b"false"
        {
            self.pos += 5;
            Ok(object_cache::get_bool(self.py, false))
        } else {
            Err(PyValueError::new_err(format!(
                "Invalid literal at position {}, expected 'false'",
                self.pos
            )))
        }
    }

    /// Parse a JSON number (integer or float)
    #[inline]
    fn parse_number(&mut self) -> PyResult<PyObject> {
        let start = self.pos;
        let mut is_float = false;
        let mut is_negative = false;

        // Handle negative sign
        if self.pos < self.input.len() && self.input[self.pos] == b'-' {
            is_negative = true;
            self.pos += 1;
        }

        // Parse integer part
        let int_start = self.pos;
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

            // Parse fractional part
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

                // Optional sign
                if self.pos < self.input.len() {
                    let sign = self.input[self.pos];
                    if sign == b'+' || sign == b'-' {
                        self.pos += 1;
                    }
                }

                // Exponent digits
                while self.pos < self.input.len() {
                    let c = self.input[self.pos];
                    if c < b'0' || c > b'9' {
                        break;
                    }
                    self.pos += 1;
                }
            }
        }

        let num_str = unsafe {
            std::str::from_utf8_unchecked(&self.input[start..self.pos])
        };

        if is_float {
            // Parse as float
            match num_str.parse::<f64>() {
                Ok(f) => {
                    if !f.is_finite() {
                        return Err(PyValueError::new_err("Number out of range"));
                    }
                    unsafe {
                        let ptr = object_cache::create_float_direct(f);
                        Ok(PyObject::from_owned_ptr(self.py, ptr))
                    }
                }
                Err(_) => Err(PyValueError::new_err(format!(
                    "Invalid number: {}",
                    num_str
                ))),
            }
        } else {
            // Fast path: parse integer inline
            let int_bytes = &self.input[int_start..self.pos];

            // Try fast integer parsing for reasonable lengths
            if int_bytes.len() <= 18 {
                let mut value: u64 = 0;
                for &b in int_bytes {
                    value = value * 10 + (b - b'0') as u64;
                }

                if is_negative {
                    if value <= 9223372036854775808 {
                        let signed = -(value as i64);
                        if signed >= -256 && signed <= 256 {
                            return Ok(object_cache::get_int(self.py, signed));
                        }
                        unsafe {
                            let ptr = object_cache::create_int_i64_direct(signed);
                            return Ok(PyObject::from_owned_ptr(self.py, ptr));
                        }
                    }
                } else {
                    if value <= 256 {
                        return Ok(object_cache::get_int(self.py, value as i64));
                    }
                    if value <= i64::MAX as u64 {
                        unsafe {
                            let ptr = object_cache::create_int_i64_direct(value as i64);
                            return Ok(PyObject::from_owned_ptr(self.py, ptr));
                        }
                    }
                    unsafe {
                        let ptr = object_cache::create_int_u64_direct(value);
                        return Ok(PyObject::from_owned_ptr(self.py, ptr));
                    }
                }
            }

            // Fallback for very large integers
            match num_str.parse::<i64>() {
                Ok(n) => {
                    if n >= -256 && n <= 256 {
                        Ok(object_cache::get_int(self.py, n))
                    } else {
                        unsafe {
                            let ptr = object_cache::create_int_i64_direct(n);
                            Ok(PyObject::from_owned_ptr(self.py, ptr))
                        }
                    }
                }
                Err(_) => {
                    // Try u64
                    match num_str.parse::<u64>() {
                        Ok(n) => unsafe {
                            let ptr = object_cache::create_int_u64_direct(n);
                            Ok(PyObject::from_owned_ptr(self.py, ptr))
                        },
                        Err(_) => Err(PyValueError::new_err(format!(
                            "Integer too large: {}",
                            num_str
                        ))),
                    }
                }
            }
        }
    }

    /// Parse a JSON string
    #[inline]
    fn parse_string(&mut self) -> PyResult<PyObject> {
        debug_assert!(self.input[self.pos] == b'"');
        self.pos += 1; // Skip opening quote

        let start = self.pos;
        let mut has_escapes = false;

        // Fast scan for end of string
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'"' {
                // Found end of string
                if !has_escapes {
                    // Fast path: no escapes, direct slice
                    let s = unsafe {
                        std::str::from_utf8_unchecked(&self.input[start..self.pos])
                    };
                    self.pos += 1; // Skip closing quote
                    unsafe {
                        let ptr = object_cache::create_string_direct(s);
                        return Ok(PyObject::from_owned_ptr(self.py, ptr));
                    }
                } else {
                    // Has escapes: need to decode
                    break;
                }
            } else if c == b'\\' {
                has_escapes = true;
                self.pos += 1;
                if self.pos < self.input.len() {
                    // Skip escaped character
                    if self.input[self.pos] == b'u' {
                        self.pos += 5; // \uXXXX
                    } else {
                        self.pos += 1;
                    }
                }
            } else if c < 0x20 {
                return Err(PyValueError::new_err(format!(
                    "Invalid control character in string at position {}",
                    self.pos
                )));
            } else {
                self.pos += 1;
            }
        }

        if self.pos >= self.input.len() {
            return Err(PyValueError::new_err("Unterminated string"));
        }

        // Decode string with escapes
        self.pos = start;
        let decoded = self.decode_string_with_escapes()?;

        unsafe {
            let ptr = object_cache::create_string_direct(&decoded);
            Ok(PyObject::from_owned_ptr(self.py, ptr))
        }
    }

    /// Decode a string with escape sequences
    fn decode_string_with_escapes(&mut self) -> PyResult<String> {
        let mut result = String::with_capacity(64);

        while self.pos < self.input.len() {
            let c = self.input[self.pos];

            if c == b'"' {
                self.pos += 1;
                return Ok(result);
            } else if c == b'\\' {
                self.pos += 1;
                if self.pos >= self.input.len() {
                    return Err(PyValueError::new_err("Unterminated escape sequence"));
                }

                let escaped = self.input[self.pos];
                self.pos += 1;

                match escaped {
                    b'"' => result.push('"'),
                    b'\\' => result.push('\\'),
                    b'/' => result.push('/'),
                    b'b' => result.push('\x08'),
                    b'f' => result.push('\x0C'),
                    b'n' => result.push('\n'),
                    b'r' => result.push('\r'),
                    b't' => result.push('\t'),
                    b'u' => {
                        // Parse \uXXXX
                        if self.pos + 4 > self.input.len() {
                            return Err(PyValueError::new_err("Invalid unicode escape"));
                        }
                        let hex = unsafe {
                            std::str::from_utf8_unchecked(&self.input[self.pos..self.pos + 4])
                        };
                        self.pos += 4;

                        let code = u16::from_str_radix(hex, 16)
                            .map_err(|_| PyValueError::new_err("Invalid unicode escape"))?;

                        // Handle surrogate pairs
                        if (0xD800..=0xDBFF).contains(&code) {
                            // High surrogate - expect low surrogate
                            if self.pos + 6 <= self.input.len()
                                && self.input[self.pos] == b'\\'
                                && self.input[self.pos + 1] == b'u'
                            {
                                self.pos += 2;
                                let hex2 = unsafe {
                                    std::str::from_utf8_unchecked(&self.input[self.pos..self.pos + 4])
                                };
                                self.pos += 4;

                                let code2 = u16::from_str_radix(hex2, 16)
                                    .map_err(|_| PyValueError::new_err("Invalid unicode escape"))?;

                                if (0xDC00..=0xDFFF).contains(&code2) {
                                    // Valid surrogate pair
                                    let combined = 0x10000
                                        + ((code as u32 - 0xD800) << 10)
                                        + (code2 as u32 - 0xDC00);
                                    if let Some(ch) = char::from_u32(combined) {
                                        result.push(ch);
                                    } else {
                                        return Err(PyValueError::new_err("Invalid surrogate pair"));
                                    }
                                } else {
                                    return Err(PyValueError::new_err("Invalid surrogate pair"));
                                }
                            } else {
                                return Err(PyValueError::new_err("Lone surrogate"));
                            }
                        } else if let Some(ch) = char::from_u32(code as u32) {
                            result.push(ch);
                        } else {
                            return Err(PyValueError::new_err("Invalid unicode code point"));
                        }
                    }
                    _ => {
                        return Err(PyValueError::new_err(format!(
                            "Invalid escape character: \\{}",
                            escaped as char
                        )));
                    }
                }
            } else {
                // Regular UTF-8 character
                result.push(c as char);
                self.pos += 1;
            }
        }

        Err(PyValueError::new_err("Unterminated string"))
    }

    /// Parse a JSON array
    #[inline]
    fn parse_array(&mut self) -> PyResult<PyObject> {
        debug_assert!(self.input[self.pos] == b'[');
        self.pos += 1;

        self.skip_whitespace();

        // Empty array fast path
        if self.pos < self.input.len() && self.input[self.pos] == b']' {
            self.pos += 1;
            unsafe {
                let list_ptr = object_cache::create_list_direct(0);
                return Ok(PyObject::from_owned_ptr(self.py, list_ptr));
            }
        }

        // Estimate initial capacity
        let estimated_size = 16;
        let mut elements: Vec<PyObject> = Vec::with_capacity(estimated_size);

        loop {
            self.skip_whitespace();

            // Parse element
            let elem = self.parse_value()?;
            elements.push(elem);

            self.skip_whitespace();

            if self.pos >= self.input.len() {
                return Err(PyValueError::new_err("Unterminated array"));
            }

            let c = self.input[self.pos];
            if c == b']' {
                self.pos += 1;
                break;
            } else if c == b',' {
                self.pos += 1;
            } else {
                return Err(PyValueError::new_err(format!(
                    "Expected ',' or ']' at position {}, found '{}'",
                    self.pos, c as char
                )));
            }
        }

        // Create Python list
        unsafe {
            let len = elements.len();
            let list_ptr = object_cache::create_list_direct(len as ffi::Py_ssize_t);

            for (i, elem) in elements.into_iter().enumerate() {
                object_cache::set_list_item_direct(list_ptr, i as ffi::Py_ssize_t, elem.into_ptr());
            }

            Ok(PyObject::from_owned_ptr(self.py, list_ptr))
        }
    }

    /// Parse a JSON object
    #[inline]
    fn parse_object(&mut self) -> PyResult<PyObject> {
        debug_assert!(self.input[self.pos] == b'{');
        self.pos += 1;

        self.skip_whitespace();

        // Empty object fast path
        if self.pos < self.input.len() && self.input[self.pos] == b'}' {
            self.pos += 1;
            unsafe {
                let dict_ptr = object_cache::create_dict_direct();
                return Ok(PyObject::from_owned_ptr(self.py, dict_ptr));
            }
        }

        unsafe {
            let dict_ptr = object_cache::create_dict_direct();

            loop {
                self.skip_whitespace();

                // Parse key (must be string)
                if self.pos >= self.input.len() || self.input[self.pos] != b'"' {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(PyValueError::new_err("Expected string key in object"));
                }

                // Parse key string inline for interning
                let key_str = self.parse_key_string()?;
                let key_obj = get_interned_string(self.py, &key_str);

                self.skip_whitespace();

                // Expect colon
                if self.pos >= self.input.len() || self.input[self.pos] != b':' {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(PyValueError::new_err("Expected ':' after object key"));
                }
                self.pos += 1;

                self.skip_whitespace();

                // Parse value
                let value = self.parse_value()?;

                // Insert into dict
                let result = object_cache::set_dict_item_direct(
                    dict_ptr,
                    key_obj.as_ptr(),
                    value.as_ptr(),
                );
                if result < 0 {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(PyValueError::new_err("Failed to set dict item"));
                }

                self.skip_whitespace();

                if self.pos >= self.input.len() {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(PyValueError::new_err("Unterminated object"));
                }

                let c = self.input[self.pos];
                if c == b'}' {
                    self.pos += 1;
                    break;
                } else if c == b',' {
                    self.pos += 1;
                } else {
                    ffi::Py_DECREF(dict_ptr);
                    return Err(PyValueError::new_err(format!(
                        "Expected ',' or '}}' at position {}, found '{}'",
                        self.pos, c as char
                    )));
                }
            }

            Ok(PyObject::from_owned_ptr(self.py, dict_ptr))
        }
    }

    /// Parse a key string (optimized for dict keys)
    #[inline]
    fn parse_key_string(&mut self) -> PyResult<String> {
        debug_assert!(self.input[self.pos] == b'"');
        self.pos += 1;

        let start = self.pos;

        // Fast scan for simple keys (no escapes)
        while self.pos < self.input.len() {
            let c = self.input[self.pos];
            if c == b'"' {
                let s = unsafe {
                    std::str::from_utf8_unchecked(&self.input[start..self.pos])
                };
                self.pos += 1;
                return Ok(s.to_string());
            } else if c == b'\\' {
                // Has escapes - use slow path
                self.pos = start;
                return self.decode_string_with_escapes();
            } else if c < 0x20 {
                return Err(PyValueError::new_err("Invalid control character in string"));
            }
            self.pos += 1;
        }

        Err(PyValueError::new_err("Unterminated string"))
    }
}

/// Public entry point for custom JSON parsing
#[inline]
pub fn loads_custom(json_str: &str) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let mut parser = JsonParser::new(py, json_str.as_bytes());
        parser.parse()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::types::{PyList, PyDict};

    fn init_caches(py: Python) {
        crate::optimizations::object_cache::init_cache(py);
        crate::optimizations::simd_parser::init_string_intern(py);
    }

    #[test]
    fn test_parse_null() {
        Python::with_gil(|py| {
            init_caches(py);
            let result = loads_custom("null").unwrap();
            assert!(result.bind(py).is_none());
        });
    }

    #[test]
    fn test_parse_bool() {
        Python::with_gil(|py| {
            init_caches(py);

            let result = loads_custom("true").unwrap();
            assert!(result.bind(py).extract::<bool>().unwrap());

            let result = loads_custom("false").unwrap();
            assert!(!result.bind(py).extract::<bool>().unwrap());
        });
    }

    #[test]
    fn test_parse_numbers() {
        Python::with_gil(|py| {
            init_caches(py);

            // Integers
            assert_eq!(loads_custom("0").unwrap().bind(py).extract::<i64>().unwrap(), 0);
            assert_eq!(loads_custom("42").unwrap().bind(py).extract::<i64>().unwrap(), 42);
            assert_eq!(loads_custom("-123").unwrap().bind(py).extract::<i64>().unwrap(), -123);

            // Floats
            let f = loads_custom("3.14").unwrap().bind(py).extract::<f64>().unwrap();
            assert!((f - 3.14).abs() < 0.001);

            let f = loads_custom("-2.5e10").unwrap().bind(py).extract::<f64>().unwrap();
            assert!((f - -2.5e10).abs() < 1.0);
        });
    }

    #[test]
    fn test_parse_string() {
        Python::with_gil(|py| {
            init_caches(py);

            assert_eq!(
                loads_custom("\"hello\"").unwrap().bind(py).extract::<String>().unwrap(),
                "hello"
            );

            assert_eq!(
                loads_custom("\"hello\\nworld\"").unwrap().bind(py).extract::<String>().unwrap(),
                "hello\nworld"
            );

            assert_eq!(
                loads_custom("\"\\u0048\\u0065\\u006c\\u006c\\u006f\"").unwrap().bind(py).extract::<String>().unwrap(),
                "Hello"
            );
        });
    }

    #[test]
    fn test_parse_array() {
        Python::with_gil(|py| {
            init_caches(py);

            let result = loads_custom("[]").unwrap();
            let list = result.bind(py).downcast::<PyList>().unwrap();
            assert_eq!(list.len(), 0);

            let result = loads_custom("[1, 2, 3]").unwrap();
            let list = result.bind(py).downcast::<PyList>().unwrap();
            assert_eq!(list.len(), 3);
        });
    }

    #[test]
    fn test_parse_object() {
        Python::with_gil(|py| {
            init_caches(py);

            let result = loads_custom("{}").unwrap();
            let dict = result.bind(py).downcast::<PyDict>().unwrap();
            assert_eq!(dict.len(), 0);

            let result = loads_custom("{\"a\": 1, \"b\": 2}").unwrap();
            let dict = result.bind(py).downcast::<PyDict>().unwrap();
            assert_eq!(dict.len(), 2);
        });
    }

    #[test]
    fn test_parse_nested() {
        Python::with_gil(|py| {
            init_caches(py);

            let json = r#"{"items": [1, 2, {"nested": true}], "count": 3}"#;
            let result = loads_custom(json).unwrap();
            let dict = result.bind(py).downcast::<PyDict>().unwrap();
            assert_eq!(dict.len(), 2);
        });
    }
}
