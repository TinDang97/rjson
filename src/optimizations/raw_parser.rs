//! Phase 21: Raw C API JSON Parser (Zero PyO3 Overhead)
//!
//! This module implements the fastest possible JSON parser by:
//! - Using raw *mut ffi::PyObject pointers (no PyO3 wrapper overhead)
//! - Direct CPython C API calls (no abstraction layers)
//! - Zero intermediate allocations where possible
//! - Inline everything for maximum performance
//!
//! Phase 50: SIMD whitespace skipping (AVX2/SSE2)
//! Phase 51: SIMD string scanning for quote/backslash
//!
//! WARNING: This is highly unsafe code. Use with caution.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::exceptions::PyValueError;
use smallvec::SmallVec;

use super::simd_parser::get_interned_string;
use super::object_cache;

// ============================================================================
// Phase 50: SIMD Whitespace Skipping
// ============================================================================

use std::sync::atomic::{AtomicU8, Ordering};

/// CPU feature level cache: 0=uninitialized, 1=SSE2 only, 2=AVX2
static CPU_LEVEL: AtomicU8 = AtomicU8::new(0);

#[inline]
fn get_cpu_level() -> u8 {
    let level = CPU_LEVEL.load(Ordering::Relaxed);
    if level != 0 {
        return level;
    }
    #[cfg(target_arch = "x86_64")]
    {
        let detected = if is_x86_feature_detected!("avx2") { 2 } else { 1 };
        CPU_LEVEL.store(detected, Ordering::Relaxed);
        detected
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        CPU_LEVEL.store(1, Ordering::Relaxed);
        1
    }
}

/// Skip whitespace using SIMD - returns the position of the first non-whitespace byte
#[cfg(target_arch = "x86_64")]
#[inline]
fn skip_whitespace_simd(input: &[u8], pos: usize) -> usize {
    let len = input.len();

    // Use scalar for small remainders
    if pos + 16 > len {
        return skip_whitespace_scalar(input, pos);
    }

    let cpu = get_cpu_level();
    if cpu == 2 && pos + 32 <= len {
        unsafe { skip_whitespace_avx2(input, pos, len) }
    } else {
        unsafe { skip_whitespace_sse2(input, pos, len) }
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn skip_whitespace_simd(input: &[u8], pos: usize) -> usize {
    skip_whitespace_scalar(input, pos)
}

#[inline]
fn skip_whitespace_scalar(input: &[u8], mut pos: usize) -> usize {
    while pos < input.len() {
        let c = input[pos];
        if c != b' ' && c != b'\n' && c != b'\r' && c != b'\t' {
            break;
        }
        pos += 1;
    }
    pos
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn skip_whitespace_sse2(input: &[u8], mut pos: usize, len: usize) -> usize {
    use std::arch::x86_64::*;

    let space = _mm_set1_epi8(b' ' as i8);
    let tab = _mm_set1_epi8(b'\t' as i8);
    let newline = _mm_set1_epi8(b'\n' as i8);
    let cr = _mm_set1_epi8(b'\r' as i8);

    while pos + 16 <= len {
        let chunk = _mm_loadu_si128(input.as_ptr().add(pos) as *const __m128i);

        // Check for whitespace characters
        let is_space = _mm_cmpeq_epi8(chunk, space);
        let is_tab = _mm_cmpeq_epi8(chunk, tab);
        let is_newline = _mm_cmpeq_epi8(chunk, newline);
        let is_cr = _mm_cmpeq_epi8(chunk, cr);

        // Combine all whitespace checks
        let is_ws = _mm_or_si128(_mm_or_si128(is_space, is_tab), _mm_or_si128(is_newline, is_cr));

        // Find first non-whitespace
        let mask = _mm_movemask_epi8(is_ws) as u32;

        if mask == 0xFFFF {
            // All 16 bytes are whitespace
            pos += 16;
        } else {
            // Found non-whitespace - find first 0 bit
            let first_non_ws = (!mask).trailing_zeros() as usize;
            return pos + first_non_ws;
        }
    }

    // Check remaining bytes with scalar
    skip_whitespace_scalar(input, pos)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn skip_whitespace_avx2(input: &[u8], mut pos: usize, len: usize) -> usize {
    use std::arch::x86_64::*;

    let space = _mm256_set1_epi8(b' ' as i8);
    let tab = _mm256_set1_epi8(b'\t' as i8);
    let newline = _mm256_set1_epi8(b'\n' as i8);
    let cr = _mm256_set1_epi8(b'\r' as i8);

    while pos + 32 <= len {
        let chunk = _mm256_loadu_si256(input.as_ptr().add(pos) as *const __m256i);

        let is_space = _mm256_cmpeq_epi8(chunk, space);
        let is_tab = _mm256_cmpeq_epi8(chunk, tab);
        let is_newline = _mm256_cmpeq_epi8(chunk, newline);
        let is_cr = _mm256_cmpeq_epi8(chunk, cr);

        let is_ws = _mm256_or_si256(_mm256_or_si256(is_space, is_tab), _mm256_or_si256(is_newline, is_cr));
        let mask = _mm256_movemask_epi8(is_ws) as u32;

        if mask == 0xFFFFFFFF {
            pos += 32;
        } else {
            let first_non_ws = (!mask).trailing_zeros() as usize;
            return pos + first_non_ws;
        }
    }

    // Check remaining bytes with SSE2 or scalar
    if pos + 16 <= len {
        return skip_whitespace_sse2(input, pos, len);
    }
    skip_whitespace_scalar(input, pos)
}

// ============================================================================
// Phase 51: SIMD String Scanning
// ============================================================================

/// Scan for quote or backslash using SIMD - returns position of found char or end
#[cfg(target_arch = "x86_64")]
#[inline]
fn find_string_end_simd(input: &[u8], pos: usize) -> (usize, bool) {
    let len = input.len();

    // Use scalar for small strings
    if pos + 16 > len {
        return find_string_end_scalar(input, pos);
    }

    let cpu = get_cpu_level();
    if cpu == 2 && pos + 32 <= len {
        unsafe { find_string_end_avx2(input, pos, len) }
    } else {
        unsafe { find_string_end_sse2(input, pos, len) }
    }
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
fn find_string_end_simd(input: &[u8], pos: usize) -> (usize, bool) {
    find_string_end_scalar(input, pos)
}

/// Returns (position, found) where found=true if quote/backslash/control was found
#[inline]
fn find_string_end_scalar(input: &[u8], mut pos: usize) -> (usize, bool) {
    while pos < input.len() {
        let c = input[pos];
        if c == b'"' || c == b'\\' || c < 0x20 {
            return (pos, true);
        }
        pos += 1;
    }
    (pos, false)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
unsafe fn find_string_end_sse2(input: &[u8], mut pos: usize, len: usize) -> (usize, bool) {
    use std::arch::x86_64::*;

    let quote = _mm_set1_epi8(b'"' as i8);
    let backslash = _mm_set1_epi8(b'\\' as i8);
    let space = _mm_set1_epi8(0x20);

    while pos + 16 <= len {
        let chunk = _mm_loadu_si128(input.as_ptr().add(pos) as *const __m128i);

        // Check for quote and backslash
        let is_quote = _mm_cmpeq_epi8(chunk, quote);
        let is_backslash = _mm_cmpeq_epi8(chunk, backslash);

        // Check for control characters (< 0x20)
        let control_mask = _mm_cmplt_epi8(chunk, space);
        let is_positive = _mm_cmpgt_epi8(chunk, _mm_set1_epi8(-1));
        let is_control = _mm_and_si128(control_mask, is_positive);

        // Combine all terminating conditions
        let terminator = _mm_or_si128(_mm_or_si128(is_quote, is_backslash), is_control);
        let mask = _mm_movemask_epi8(terminator) as u32;

        if mask != 0 {
            let first = mask.trailing_zeros() as usize;
            return (pos + first, true);
        }

        pos += 16;
    }

    // Check remaining bytes with scalar
    find_string_end_scalar(input, pos)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn find_string_end_avx2(input: &[u8], mut pos: usize, len: usize) -> (usize, bool) {
    use std::arch::x86_64::*;

    let quote = _mm256_set1_epi8(b'"' as i8);
    let backslash = _mm256_set1_epi8(b'\\' as i8);
    let space = _mm256_set1_epi8(0x20);

    while pos + 32 <= len {
        let chunk = _mm256_loadu_si256(input.as_ptr().add(pos) as *const __m256i);

        let is_quote = _mm256_cmpeq_epi8(chunk, quote);
        let is_backslash = _mm256_cmpeq_epi8(chunk, backslash);

        // Control char detection
        let control_mask = _mm256_cmpgt_epi8(space, chunk);
        let is_positive = _mm256_cmpgt_epi8(chunk, _mm256_set1_epi8(-1));
        let is_control = _mm256_and_si256(control_mask, is_positive);

        let terminator = _mm256_or_si256(_mm256_or_si256(is_quote, is_backslash), is_control);
        let mask = _mm256_movemask_epi8(terminator) as u32;

        if mask != 0 {
            let first = mask.trailing_zeros() as usize;
            return (pos + first, true);
        }

        pos += 32;
    }

    // Check remaining bytes (fall through to SSE2 or scalar)
    if pos + 16 <= len {
        return find_string_end_sse2(input, pos, len);
    }
    find_string_end_scalar(input, pos)
}

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
        // Note: SIMD tested but scalar is faster for typical JSON (short/no whitespace)
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
            // Phase 24: Direct pointer with INCREF (no PyObject wrapper overhead)
            Ok(object_cache::get_none_ptr_incref())
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
            // Phase 24: Direct pointer with INCREF (no PyObject wrapper overhead)
            Ok(object_cache::get_true_ptr_incref())
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
            // Phase 24: Direct pointer with INCREF (no PyObject wrapper overhead)
            Ok(object_cache::get_false_ptr_incref())
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
            // Phase 24: Use fast_float for ~4x faster float parsing
            let num_bytes = &self.input[start..self.pos];
            match fast_float::parse::<f64, _>(num_bytes) {
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
                        // Phase 24: Use direct pointer for cached integers
                        if signed >= -256 {
                            Ok(object_cache::get_int_ptr(signed))
                        } else {
                            Ok(object_cache::create_int_i64_direct(signed))
                        }
                    } else {
                        Err("Integer overflow")
                    }
                } else if value <= 256 {
                    // Phase 24: Use direct pointer for cached integers
                    Ok(object_cache::get_int_ptr(value as i64))
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

        // Note: SIMD tested but scalar is faster for typical short JSON strings
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

        // Scalar scan for end of string (faster for typical short keys)
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
        // Use SmallVec for stack allocation of short escaped strings
        let mut result: SmallVec<[u8; 64]> = SmallVec::new();

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
        // Use SmallVec for stack allocation of short escaped strings
        let mut result: SmallVec<[u8; 128]> = SmallVec::new();

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

        // Use SmallVec for stack allocation of small arrays (< 32 elements)
        // This avoids heap allocation for common small arrays
        let mut elements: SmallVec<[*mut ffi::PyObject; 32]> = SmallVec::new();

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
