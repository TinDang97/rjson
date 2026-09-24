//! Hand-written single-pass JSON parser that builds CPython objects directly.
//!
//! Replaces the serde_json Deserializer + Visitor path for `loads`. Design:
//!
//! * Input is borrowed: `str` via `PyUnicode_AsUTF8AndSize` (zero-copy for
//!   compact ASCII strings, cached UTF-8 otherwise), `bytes`/`bytearray`
//!   directly, `memoryview` via `PyObject_GetBuffer`. Bytes input is validated
//!   once up front with `simdutf8`.
//! * Recursive descent with one value stack (`Vec<*mut PyObject>`, pooled
//!   across calls). Array elements and object key/value pairs are pushed on
//!   the stack; the container is created at the closing bracket with its
//!   exact size (`PyList_New(n)` + memcpy of the item pointers). No
//!   per-container heap allocation besides the Python object itself. On
//!   error, every reference still on the stack is released: no leaks.
//! * Strings: SSE2 scan for `"`, `\\` and control characters that also
//!   detects non-ASCII bytes. ASCII strings are `PyUnicode_New(len, 127)` plus
//!   memcpy. Non-ASCII strings are decoded from UTF-8 straight into the final
//!   UCS1/UCS2/UCS4 buffer (one SIMD counting pass, one write pass).
//! * Object keys go through a direct-mapped cache (`KEY_CACHE`) keyed on the
//!   raw key bytes. Cached keys are reused across calls, carry a precomputed
//!   hash, and are compared byte-for-byte on lookup (no false hits).
//! * Numbers: SWAR digit parsing (8 digits per step). Integers up to 19
//!   digits are machine ints; longer ones become exact Python ints via
//!   `PyLong_FromString`. Floats use Clinger's exact fast path, then
//!   Eisel-Lemire (`crate::lemire`), then `fast_float` for >19 digits; all
//!   are correctly rounded.
//! * Nesting depth is limited to 1024 (same as orjson).
//! * On CPython 3.10/3.11 the cyclic GC is paused while parsing (see
//!   `pause_gc`).

use pyo3::exceptions::PyTypeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::GILOnceCell;
use pyo3::types::PyType;
use std::ptr;

const MAX_DEPTH: u32 = 1024;

// ============================================================================
// Key cache
// ============================================================================

const KEY_CACHE_SIZE: usize = 2048;
const KEY_CACHE_MAX_LEN: usize = 64;

#[derive(Clone, Copy)]
struct KeyEntry {
    obj: *mut ffi::PyObject,
    len: u32,
    bytes: [u8; KEY_CACHE_MAX_LEN],
}

const EMPTY_KEY: KeyEntry = KeyEntry {
    obj: ptr::null_mut(),
    len: 0,
    bytes: [0; KEY_CACHE_MAX_LEN],
};

/// Direct-mapped cache of dict-key strings. Only touched with the GIL held
/// (every `loads` call holds it), so no further synchronization is needed on
/// GIL-enabled CPython builds.
static mut KEY_CACHE: [KeyEntry; KEY_CACHE_SIZE] = [EMPTY_KEY; KEY_CACHE_SIZE];

#[inline(always)]
fn hash_key(b: &[u8]) -> u64 {
    // FxHash-style word-at-a-time hash; keys are <= 64 bytes.
    const K: u64 = 0x517c_c1b7_2722_0a95;
    let mut h: u64 = b.len() as u64;
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
        h = (h.rotate_left(5) ^ w).wrapping_mul(K);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut w = 0u64;
        for (i, &x) in rem.iter().enumerate() {
            w |= (x as u64) << (i * 8);
        }
        h = (h.rotate_left(5) ^ w).wrapping_mul(K);
    }
    h ^ (h >> 29)
}

// ============================================================================
// Error type
// ============================================================================

/// Zero-sized failure marker: the message and position are stored in the
/// parser (`Parser::error`) so `PResult<*mut PyObject>` stays register-sized.
#[derive(Debug)]
struct Fail;

type PResult<T> = Result<T, Fail>;

static JSON_DECODE_ERROR: GILOnceCell<Py<PyType>> = GILOnceCell::new();

#[cold]
#[inline(never)]
fn raise_decode_error(py: Python<'_>, msg: &str, doc: &[u8], pos: usize) -> PyErr {
    // Raise json.JSONDecodeError (a ValueError subclass, like orjson) with a
    // character position. Fall back to ValueError if json can't be imported.
    let doc_str = String::from_utf8_lossy(doc);
    let pos = pos.min(doc.len());
    let char_pos = doc[..pos].iter().filter(|&&b| (b & 0xC0) != 0x80).count();
    let full = format!("JSON parsing error: {msg}");
    let ty = JSON_DECODE_ERROR.get_or_try_init(py, || -> PyResult<Py<PyType>> {
        let m = py.import("json")?;
        Ok(m.getattr("JSONDecodeError")?.downcast_into::<PyType>()?.unbind())
    });
    match ty {
        Ok(ty) => PyErr::from_type(ty.bind(py).clone(), (full, doc_str.into_owned(), char_pos)),
        Err(_) => pyo3::exceptions::PyValueError::new_err(full),
    }
}

// ============================================================================
// Parser
// ============================================================================

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    /// True when `buf` is known to be valid UTF-8 (input was a `str`).
    utf8_valid: bool,
    depth: u32,
    /// Owned references of not-yet-assembled container members.
    stack: Vec<*mut ffi::PyObject>,
    /// Scratch space for unescaping strings.
    scratch: Vec<u8>,
    /// Message and byte offset of the first error.
    error: std::cell::Cell<(&'static str, usize)>,
}

#[inline(always)]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\n' | b'\r' | b'\t')
}

impl<'a> Parser<'a> {
    #[inline(always)]
    fn peek(&self) -> u8 {
        if self.pos < self.buf.len() {
            unsafe { *self.buf.get_unchecked(self.pos) }
        } else {
            0
        }
    }

    #[inline(always)]
    fn skip_ws(&mut self) {
        // All JSON whitespace is <= b' '; anything above can't be whitespace.
        let b = self.peek();
        if b > b' ' {
            return;
        }
        // Common compact-with-spaces case (", " / ": ") without a call.
        if b == b' ' {
            self.pos += 1;
            if self.peek() > b' ' {
                return;
            }
        }
        self.skip_ws_slow();
    }

    /// Whitespace skipping for pretty-printed input: SSE2, 16 bytes at a time.
    #[inline(never)]
    fn skip_ws_slow(&mut self) {
        let buf = self.buf;
        let len = buf.len();
        let mut i = self.pos;
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            let sp = _mm_set1_epi8(b' ' as i8);
            let nl = _mm_set1_epi8(b'\n' as i8);
            let cr = _mm_set1_epi8(b'\r' as i8);
            let tb = _mm_set1_epi8(b'\t' as i8);
            while i + 16 <= len {
                let v = _mm_loadu_si128(buf.as_ptr().add(i) as *const __m128i);
                let ws = _mm_or_si128(
                    _mm_or_si128(_mm_cmpeq_epi8(v, sp), _mm_cmpeq_epi8(v, nl)),
                    _mm_or_si128(_mm_cmpeq_epi8(v, cr), _mm_cmpeq_epi8(v, tb)),
                );
                let non_ws = !(_mm_movemask_epi8(ws) as u32) & 0xFFFF;
                if non_ws != 0 {
                    self.pos = i + non_ws.trailing_zeros() as usize;
                    return;
                }
                i += 16;
            }
        }
        while i < len && is_ws(unsafe { *buf.get_unchecked(i) }) {
            i += 1;
        }
        self.pos = i;
    }

    #[cold]
    #[inline(never)]
    fn err<T>(&self, msg: &'static str, pos: usize) -> PResult<T> {
        self.error.set((msg, pos));
        Err(Fail)
    }

    #[cold]
    #[inline(never)]
    fn err_unexpected<T>(&self, expected: &'static str) -> PResult<T> {
        if self.pos >= self.buf.len() {
            self.err("unexpected end of data", self.pos)
        } else {
            self.err(expected, self.pos)
        }
    }

    // ---------------------------------------------------------------- values

    /// Parse one value at the current position (no leading whitespace).
    /// Returns a new (owned) reference.
    #[inline(always)]
    fn parse_value(&mut self) -> PResult<*mut ffi::PyObject> {
        match self.peek() {
            b'"' => self.parse_str(false),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            b't' => self.parse_literal(b"true", unsafe { ffi::Py_True() }),
            b'f' => self.parse_literal(b"false", unsafe { ffi::Py_False() }),
            b'n' => self.parse_literal(b"null", unsafe { ffi::Py_None() }),
            _ => self.err_unexpected("unexpected character, expected a JSON value"),
        }
    }

    #[inline(always)]
    fn parse_literal(&mut self, lit: &'static [u8], obj: *mut ffi::PyObject) -> PResult<*mut ffi::PyObject> {
        let end = self.pos + lit.len();
        if end <= self.buf.len() && &self.buf[self.pos..end] == lit {
            self.pos = end;
            unsafe { ffi::Py_INCREF(obj) };
            Ok(obj)
        } else {
            self.err_literal(lit)
        }
    }

    #[cold]
    #[inline(never)]
    fn err_literal<T>(&self, lit: &'static [u8]) -> PResult<T> {
        let mut p = self.pos;
        for &c in lit {
            if p >= self.buf.len() {
                return self.err("unexpected end of data", p);
            }
            if self.buf[p] != c {
                return self.err("invalid literal", p);
            }
            p += 1;
        }
        self.err("invalid literal", self.pos)
    }

    #[inline(always)]
    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return self.err("depth limit exceeded", self.pos);
        }
        Ok(())
    }

    #[inline(never)]
    fn parse_array(&mut self) -> PResult<*mut ffi::PyObject> {
        self.enter()?;
        self.pos += 1; // '['
        self.skip_ws();
        let base = self.stack.len();
        if self.peek() == b']' {
            self.pos += 1;
        } else {
            loop {
                let v = self.parse_value()?;
                self.stack.push(v);
                self.skip_ws();
                match self.peek() {
                    b',' => {
                        self.pos += 1;
                        self.skip_ws();
                        if self.peek() == b']' {
                            return self.err("trailing comma is not allowed", self.pos);
                        }
                    }
                    b']' => {
                        self.pos += 1;
                        break;
                    }
                    _ => return self.err_unexpected("unexpected character, expected ',' or ']'"),
                }
            }
        }
        self.depth -= 1;
        let n = self.stack.len() - base;
        unsafe {
            let list = ffi::PyList_New(n as ffi::Py_ssize_t);
            if list.is_null() {
                return self.err("out of memory", self.pos);
            }
            if n > 0 {
                let items = (*(list as *mut ffi::PyListObject)).ob_item;
                ptr::copy_nonoverlapping(self.stack.as_ptr().add(base), items, n);
                self.stack.set_len(base);
            }
            Ok(list)
        }
    }

    #[inline(never)]
    fn parse_object(&mut self) -> PResult<*mut ffi::PyObject> {
        self.enter()?;
        self.pos += 1; // '{'
        self.skip_ws();
        let base = self.stack.len();
        if self.peek() == b'}' {
            self.pos += 1;
        } else {
            loop {
                if self.peek() != b'"' {
                    return self.err_unexpected("unexpected character, expected a string key");
                }
                let k = self.parse_str(true)?;
                self.stack.push(k);
                self.skip_ws();
                if self.peek() != b':' {
                    return self.err_unexpected("unexpected character, expected ':' after key");
                }
                self.pos += 1;
                self.skip_ws();
                let v = self.parse_value()?;
                self.stack.push(v);
                self.skip_ws();
                match self.peek() {
                    b',' => {
                        self.pos += 1;
                        self.skip_ws();
                        if self.peek() == b'}' {
                            return self.err("trailing comma is not allowed", self.pos);
                        }
                    }
                    b'}' => {
                        self.pos += 1;
                        break;
                    }
                    _ => return self.err_unexpected("unexpected character, expected ',' or '}'"),
                }
            }
        }
        self.depth -= 1;
        let n = (self.stack.len() - base) / 2;
        unsafe {
            // `_PyDict_NewPresized` (3.11-3.13) always allocates the generic
            // key table (24-byte entries with a stored hash) instead of the
            // str-only table (16-byte entries) that `PyDict_New` + str-key
            // inserts produce. Presizing small dicts made 8-key records ~80
            // bytes larger each (and later lookups take the generic path), so
            // only presize large dicts where avoiding repeated resizes pays
            // (same threshold as orjson).
            let dict = if n > 8 {
                ffi::_PyDict_NewPresized(n as ffi::Py_ssize_t)
            } else {
                ffi::PyDict_New()
            };
            if dict.is_null() {
                return self.err("out of memory", self.pos);
            }
            // Insert in document order so the last duplicate key wins.
            let mut i = base;
            let end = self.stack.len();
            let mut failed = false;
            while i < end {
                let k = *self.stack.get_unchecked(i);
                let v = *self.stack.get_unchecked(i + 1);
                if !failed && ffi::PyDict_SetItem(dict, k, v) != 0 {
                    failed = true;
                }
                ffi::Py_DECREF(k);
                ffi::Py_DECREF(v);
                i += 2;
            }
            self.stack.set_len(base);
            if failed {
                ffi::Py_DECREF(dict);
                ffi::PyErr_Clear();
                return self.err("failed to insert into dict", self.pos);
            }
            Ok(dict)
        }
    }

    // --------------------------------------------------------------- strings

    /// Scan from `i` for the first `"`, `\\` or control character.
    /// Returns (index of that byte or buf.len(), saw non-ASCII before it).
    #[inline(always)]
    fn scan_special(&self, mut i: usize) -> (usize, bool) {
        let buf = self.buf;
        let len = buf.len();
        let mut non_ascii = false;
        // SIMD scan (SSE2 is baseline on x86_64).
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::*;
            let quote = _mm_set1_epi8(b'"' as i8);
            let bslash = _mm_set1_epi8(b'\\' as i8);
            let ctl = _mm_set1_epi8(0x1F);
            while i + 16 <= len {
                let v = _mm_loadu_si128(buf.as_ptr().add(i) as *const __m128i);
                let m = _mm_or_si128(
                    _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, bslash)),
                    _mm_cmpeq_epi8(_mm_max_epu8(v, ctl), ctl),
                );
                let mask = _mm_movemask_epi8(m) as u32;
                let hi = _mm_movemask_epi8(v) as u32;
                if mask != 0 {
                    let tz = mask.trailing_zeros();
                    non_ascii |= (hi & ((1u32 << tz) - 1)) != 0;
                    return (i + tz as usize, non_ascii);
                }
                non_ascii |= hi != 0;
                i += 16;
            }
        }
        // Scalar tail (and non-x86 path).
        while i < len {
            let b = unsafe { *buf.get_unchecked(i) };
            if b == b'"' || b == b'\\' || b < 0x20 {
                break;
            }
            non_ascii |= b >= 0x80;
            i += 1;
        }
        (i, non_ascii)
    }

    /// Parse a string whose opening quote is at `self.pos`. Returns a new
    /// reference. `cache` enables the key cache for escape-free strings.
    #[inline(always)]
    fn parse_str(&mut self, cache: bool) -> PResult<*mut ffi::PyObject> {
        let start = self.pos + 1;
        let (i, non_ascii) = self.scan_special(start);
        let b = self.peek_at(i);
        if b == b'"' {
            self.pos = i + 1;
            if cache && i - start <= KEY_CACHE_MAX_LEN {
                self.cached_key(start, i, non_ascii)
            } else {
                self.make_str(start, i, non_ascii)
            }
        } else if b == b'\\' {
            self.parse_escaped(start, i, non_ascii)
        } else {
            self.err_string(i)
        }
    }

    #[inline(always)]
    fn peek_at(&self, i: usize) -> u8 {
        if i < self.buf.len() {
            unsafe { *self.buf.get_unchecked(i) }
        } else {
            0
        }
    }

    #[cold]
    #[inline(never)]
    fn err_string<T>(&self, i: usize) -> PResult<T> {
        if i >= self.buf.len() {
            self.err("unexpected end of data in string", i)
        } else {
            self.err("unexpected control character in string", i)
        }
    }

    #[inline(always)]
    fn cached_key(&mut self, start: usize, end: usize, non_ascii: bool) -> PResult<*mut ffi::PyObject> {
        let raw = unsafe { self.buf.get_unchecked(start..end) };
        unsafe {
            let idx = (hash_key(raw) as usize) & (KEY_CACHE_SIZE - 1);
            let entry = &mut *ptr::addr_of_mut!(KEY_CACHE[idx]);
            if !entry.obj.is_null() && entry.len as usize == raw.len() && entry.bytes.get_unchecked(..raw.len()) == raw {
                ffi::Py_INCREF(entry.obj);
                return Ok(entry.obj);
            }
            let s = self.make_str(start, end, non_ascii)?;
            // Precompute and cache the hash so dict insertion doesn't need to.
            if ffi::PyObject_Hash(s) == -1 {
                ffi::PyErr_Clear();
            }
            let old = entry.obj;
            ffi::Py_INCREF(s);
            entry.obj = s;
            entry.len = raw.len() as u32;
            entry.bytes.get_unchecked_mut(..raw.len()).copy_from_slice(raw);
            if !old.is_null() {
                ffi::Py_DECREF(old);
            }
            Ok(s)
        }
    }

    /// Create a str from buf[start..end] which contains no escapes.
    #[inline(always)]
    fn make_str(&self, start: usize, end: usize, non_ascii: bool) -> PResult<*mut ffi::PyObject> {
        let bytes = unsafe { self.buf.get_unchecked(start..end) };
        let s = if !non_ascii {
            unsafe { new_ascii_str(bytes) }
        } else {
            if !self.utf8_valid && std::str::from_utf8(bytes).is_err() {
                return self.err("str is not valid UTF-8", start);
            }
            unsafe { new_utf8_str(bytes) }
        };
        if s.is_null() {
            return self.err_oom();
        }
        Ok(s)
    }

    #[cold]
    #[inline(never)]
    fn err_oom<T>(&self) -> PResult<T> {
        unsafe { ffi::PyErr_Clear() };
        self.err("out of memory", self.pos)
    }

    /// Slow path for strings containing escapes. `start` is the first content
    /// byte, `i` the first backslash. Decodes in a single pass into the
    /// scratch buffer and leaves `self.pos` after the closing quote. Plain runs
    /// are copied with unconditional 16-byte stores and the output is written
    /// through a raw pointer; capacity is checked once per escape / chunk.
    #[inline(never)]
    fn parse_escaped(&mut self, start: usize, mut i: usize, mut non_ascii: bool) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
        let len = buf.len();
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        out.extend_from_slice(&buf[start..i]);
        let mut olen = out.len();
        let r = 'outer: loop {
            // Invariant: buf[i] == b'\\'. Ensure room for the escape (<= 4
            // bytes) plus one 16-byte store.
            if out.capacity() - olen < 32 {
                unsafe { out.set_len(olen) };
                out.reserve(64);
            }
            let c = self.peek_at(i + 1);
            let e = ESCAPE_LUT[c as usize];
            if e != 0 {
                unsafe { *out.as_mut_ptr().add(olen) = e };
                olen += 1;
                i += 2;
            } else if c == b'u' {
                unsafe { out.set_len(olen) };
                match self.unescape_unicode(i, &mut out) {
                    Ok((next, na)) => {
                        i = next;
                        non_ascii |= na;
                        olen = out.len();
                    }
                    Err(e) => break Err(e),
                }
            } else if i + 1 >= len {
                break self.err("unexpected end of data in string", i + 1);
            } else {
                break self.err("invalid escaped sequence in string", i);
            }
            // Copy the following plain run.
            #[cfg(target_arch = "x86_64")]
            unsafe {
                use std::arch::x86_64::*;
                let quote = _mm_set1_epi8(b'"' as i8);
                let bslash = _mm_set1_epi8(b'\\' as i8);
                let ctl = _mm_set1_epi8(0x1F);
                while i + 16 <= len {
                    let v = _mm_loadu_si128(buf.as_ptr().add(i) as *const __m128i);
                    _mm_storeu_si128(out.as_mut_ptr().add(olen) as *mut __m128i, v);
                    let m = _mm_or_si128(
                        _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, bslash)),
                        _mm_cmpeq_epi8(_mm_max_epu8(v, ctl), ctl),
                    );
                    let mask = _mm_movemask_epi8(m) as u32;
                    let hi = _mm_movemask_epi8(v) as u32;
                    if mask == 0 {
                        non_ascii |= hi != 0;
                        olen += 16;
                        i += 16;
                        if out.capacity() - olen < 32 {
                            out.set_len(olen);
                            out.reserve(64);
                        }
                        continue;
                    }
                    let tz = mask.trailing_zeros();
                    non_ascii |= (hi & ((1u32 << tz) - 1)) != 0;
                    olen += tz as usize;
                    i += tz as usize;
                    match *buf.get_unchecked(i) {
                        b'"' => {
                            self.pos = i + 1;
                            break 'outer Ok(());
                        }
                        b'\\' => continue 'outer,
                        _ => break 'outer self.err_string(i),
                    }
                }
            }
            // Scalar tail near the end of the input (and non-x86 path).
            unsafe { out.set_len(olen) };
            while i < len {
                let b = unsafe { *buf.get_unchecked(i) };
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                non_ascii |= b >= 0x80;
                out.push(b);
                i += 1;
            }
            olen = out.len();
            match self.peek_at(i) {
                b'"' => {
                    self.pos = i + 1;
                    break Ok(());
                }
                b'\\' => {}
                _ => break self.err_string(i),
            }
        };
        unsafe { out.set_len(olen) };
        let res = r.and_then(|()| {
            let s = if !non_ascii {
                unsafe { new_ascii_str(&out) }
            } else {
                if !self.utf8_valid && std::str::from_utf8(&out).is_err() {
                    return self.err("str is not valid UTF-8", start);
                }
                unsafe { new_utf8_str(&out) }
            };
            if s.is_null() {
                return self.err_oom();
            }
            Ok(s)
        });
        self.scratch = out;
        res
    }

    #[inline(never)]
    fn unescape_unicode(&self, i: usize, out: &mut Vec<u8>) -> PResult<(usize, bool)> {
        let cp = self.parse_hex4(i + 2)?;
        let mut next = i + 6;
        let ch = if (0xD800..0xDC00).contains(&cp) {
            // High surrogate: must be followed by \uDC00-\uDFFF.
            if self.peek_at(next) == b'\\' && self.peek_at(next + 1) == b'u' {
                let lo = self.parse_hex4(next + 2)?;
                if !(0xDC00..0xE000).contains(&lo) {
                    return self.err("invalid low surrogate in string", next);
                }
                next += 6;
                0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
            } else {
                return self.err("no low surrogate in string", i);
            }
        } else if (0xDC00..0xE000).contains(&cp) {
            return self.err("invalid high surrogate in string", i);
        } else {
            cp
        };
        let ch = unsafe { char::from_u32_unchecked(ch) };
        let mut tmp = [0u8; 4];
        let s = ch.encode_utf8(&mut tmp);
        out.extend_from_slice(s.as_bytes());
        Ok((next, s.len() > 1))
    }

    fn parse_hex4(&self, i: usize) -> PResult<u32> {
        if i + 4 > self.buf.len() {
            return self.err("unexpected end of data in string", self.buf.len());
        }
        let mut v = 0u32;
        for k in 0..4 {
            let d = match self.buf[i + k] {
                c @ b'0'..=b'9' => c - b'0',
                c @ b'a'..=b'f' => c - b'a' + 10,
                c @ b'A'..=b'F' => c - b'A' + 10,
                _ => return self.err("invalid escaped sequence in string", i + k),
            };
            v = (v << 4) | d as u32;
        }
        Ok(v)
    }

    // --------------------------------------------------------------- numbers

    /// Accumulate decimal digits starting at `*i` into `mant` (first 19
    /// significant digits only); `nd` counts all digits seen.
    #[inline(always)]
    fn digits(&self, i: &mut usize, mant: &mut u64, nd: &mut usize) {
        let buf = self.buf;
        let len = buf.len();
        // SWAR: classify 8 bytes at once and convert the leading digit run
        // (1..=8 digits) with a single multiply-based conversion.
        while *i + 8 <= len {
            let w = u64::from_le(unsafe { ptr::read_unaligned(buf.as_ptr().add(*i) as *const u64) });
            let non_digit = (w.wrapping_sub(0x3030_3030_3030_3030) | w.wrapping_add(0x4646_4646_4646_4646))
                & 0x8080_8080_8080_8080;
            if non_digit == 0 {
                if *nd + 8 > 19 {
                    break;
                }
                *mant = *mant * 100_000_000 + parse_8digits(w);
                *nd += 8;
                *i += 8;
                continue;
            }
            let n = (non_digit.trailing_zeros() / 8) as usize; // 0..=7
            if n == 0 {
                return;
            }
            if *nd + n > 19 {
                break;
            }
            let shift = 8 * (8 - n) as u32;
            let v = (w << shift) | (0x3030_3030_3030_3030u64 >> (64 - shift));
            *mant = *mant * POW10_U64[n] + parse_8digits(v);
            *nd += n;
            *i += n;
            return;
        }
        while *i < len {
            let d = unsafe { *buf.get_unchecked(*i) }.wrapping_sub(b'0');
            if d > 9 {
                break;
            }
            if *nd < 19 {
                *mant = *mant * 10 + d as u64;
            }
            *nd += 1;
            *i += 1;
        }
    }

    #[inline(always)]
    fn parse_number(&mut self) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
        let start = self.pos;
        let mut i = start;
        let neg = unsafe { *buf.get_unchecked(i) } == b'-';
        i += neg as usize;
        let mut mant: u64 = 0;
        let mut nd: usize = 0;
        // Integer part.
        let first = self.peek_at(i);
        if first == b'0' {
            i += 1;
            if self.peek_at(i).is_ascii_digit() {
                return self.err("number with leading zero is not allowed", start);
            }
        } else if first.is_ascii_digit() {
            self.digits(&mut i, &mut mant, &mut nd);
        } else {
            return self.err_num_digit(i, "no digit after sign");
        }
        let c = self.peek_at(i);
        if c != b'.' && (c | 0x20) != b'e' {
            // Integer.
            self.pos = i;
            unsafe {
                let r = if nd <= 18 {
                    let v = mant as i64;
                    ffi::PyLong_FromLongLong(if neg { -v } else { v })
                } else if nd == 19 && !neg {
                    ffi::PyLong_FromUnsignedLongLong(mant)
                } else if nd == 19 && mant <= (i64::MAX as u64) + 1 {
                    ffi::PyLong_FromLongLong((mant as i64).wrapping_neg())
                } else {
                    return self.parse_big_int(start, i);
                };
                if r.is_null() {
                    return self.err_oom();
                }
                return Ok(r);
            }
        }
        self.parse_float_tail(start, i, neg, mant, nd)
    }

    #[inline(always)]
    fn parse_float_tail(&mut self, start: usize, mut i: usize, neg: bool, mut mant: u64, mut nd: usize) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
        let len = buf.len();
        let mut frac_digits: i64 = 0;
        if self.peek_at(i) == b'.' {
            i += 1;
            let fs = i;
            if mant == 0 {
                // Leading zeros of "0.000123" don't count towards the 19
                // significant digit budget.
                while i < len && buf[i] == b'0' {
                    i += 1;
                }
            }
            let zeros = i - fs;
            let nd0 = nd;
            self.digits(&mut i, &mut mant, &mut nd);
            if i == fs {
                return self.err_num_digit(i, "no digit after decimal point");
            }
            frac_digits = (zeros + (nd - nd0)) as i64;
        }
        let mut exp: i64 = 0;
        if (self.peek_at(i) | 0x20) == b'e' {
            i += 1;
            let mut eneg = false;
            let s = self.peek_at(i);
            if s == b'+' || s == b'-' {
                eneg = s == b'-';
                i += 1;
            }
            let es = i;
            while i < len {
                let d = buf[i].wrapping_sub(b'0');
                if d > 9 {
                    break;
                }
                if exp < 100_000 {
                    exp = exp * 10 + d as i64;
                }
                i += 1;
            }
            if i == es {
                return self.err_num_digit(i, "no digit in exponent");
            }
            if eneg {
                exp = -exp;
            }
        }
        self.pos = i;
        if nd <= 19 {
            let e = exp - frac_digits;
            // Clinger's fast path: exact mantissa and power of ten, so a
            // single correctly rounded operation.
            let v = if mant <= (1u64 << 53) && (-22..=22).contains(&e) {
                let m = mant as f64;
                Some(if e >= 0 { m * POW10[e as usize] } else { m / POW10[(-e) as usize] })
            } else {
                crate::lemire::compute_float64(e, mant)
            };
            if let Some(v) = v {
                if v.is_infinite() {
                    return self.err("number is infinity when parsed as double", start);
                }
                let r = unsafe { ffi::PyFloat_FromDouble(if neg { -v } else { v }) };
                if r.is_null() {
                    return self.err_oom();
                }
                return Ok(r);
            }
        }
        self.parse_float_slow(start, i)
    }

    #[cold]
    #[inline(never)]
    fn err_num_digit<T>(&self, i: usize, msg: &'static str) -> PResult<T> {
        if i >= self.buf.len() {
            self.err("unexpected end of data", i)
        } else {
            self.err(msg, i)
        }
    }

    #[inline(never)]
    fn parse_float_slow(&self, start: usize, end: usize) -> PResult<*mut ffi::PyObject> {
        let s = &self.buf[start..end];
        match fast_float::parse::<f64, _>(s) {
            Ok(v) if v.is_finite() => unsafe { Ok(ffi::PyFloat_FromDouble(v)) },
            Ok(_) => self.err("number is infinity when parsed as double", start),
            Err(_) => self.err("invalid number", start),
        }
    }

    /// Integers that don't fit in 64 bits: exact Python int (like stdlib json).
    #[cold]
    #[inline(never)]
    fn parse_big_int(&self, start: usize, end: usize) -> PResult<*mut ffi::PyObject> {
        let s = &self.buf[start..end];
        // 20 digits may still fit in a u64.
        if s.len() == 20 && s[0] != b'-' {
            if let Some(v) = std::str::from_utf8(s).ok().and_then(|t| t.parse::<u64>().ok()) {
                return unsafe { Ok(ffi::PyLong_FromUnsignedLongLong(v)) };
            }
        }
        let mut tmp = Vec::with_capacity(s.len() + 1);
        tmp.extend_from_slice(s);
        tmp.push(0);
        unsafe {
            let r = ffi::PyLong_FromString(tmp.as_ptr() as *const std::os::raw::c_char, ptr::null_mut(), 10);
            if r.is_null() {
                ffi::PyErr_Clear();
                return self.err("invalid number", start);
            }
            Ok(r)
        }
    }
}

#[inline(always)]
fn parse_8digits(mut v: u64) -> u64 {
    const MASK: u64 = 0x0000_00FF_0000_00FF;
    const MUL1: u64 = 0x000F_4240_0000_0064;
    const MUL2: u64 = 0x0000_2710_0000_0001;
    v -= 0x3030_3030_3030_3030;
    v = (v * 10) + (v >> 8);
    let v1 = (v & MASK).wrapping_mul(MUL1);
    let v2 = ((v >> 16) & MASK).wrapping_mul(MUL2);
    ((v1.wrapping_add(v2) >> 32) as u32) as u64
}

/// Single-character JSON escapes: escape letter -> decoded byte (0 = not a
/// single-character escape).
const ESCAPE_LUT: [u8; 256] = {
    let mut t = [0u8; 256];
    t[b'"' as usize] = b'"';
    t[b'\\' as usize] = b'\\';
    t[b'/' as usize] = b'/';
    t[b'b' as usize] = 0x08;
    t[b'f' as usize] = 0x0C;
    t[b'n' as usize] = b'\n';
    t[b'r' as usize] = b'\r';
    t[b't' as usize] = b'\t';
    t
};

const POW10_U64: [u64; 9] = [1, 10, 100, 1_000, 10_000, 100_000, 1_000_000, 10_000_000, 100_000_000];

const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

// ============================================================================
// String construction
// ============================================================================

/// Create a compact ASCII str. `bytes` must be pure ASCII.
#[inline(always)]
unsafe fn new_ascii_str(bytes: &[u8]) -> *mut ffi::PyObject {
    let s = ffi::PyUnicode_New(bytes.len() as ffi::Py_ssize_t, 127);
    if !s.is_null() {
        ptr::copy_nonoverlapping(bytes.as_ptr(), ffi::PyUnicode_DATA(s) as *mut u8, bytes.len());
    }
    s
}

/// Create a str from valid, non-ASCII UTF-8, decoding directly into the
/// narrowest CPython representation (UCS1/UCS2/UCS4).
#[inline(never)]
unsafe fn new_utf8_str(bytes: &[u8]) -> *mut ffi::PyObject {
    // Pass 1: character count (bytes minus continuation bytes) and widest
    // byte, which determines the narrowest representation.
    let (nchars, max_lead) = utf8_count_and_max(bytes);
    let maxchar: u32 = if max_lead >= 0xF0 {
        0x10FFFF
    } else if max_lead >= 0xE0 {
        0xFFFF
    } else if max_lead >= 0xC4 {
        0x7FF
    } else {
        0xFF
    };
    let s = ffi::PyUnicode_New(nchars as ffi::Py_ssize_t, maxchar);
    if s.is_null() {
        return s;
    }
    let data = ffi::PyUnicode_DATA(s);
    match ffi::PyUnicode_KIND(s) {
        1 => decode_into(bytes, data as *mut u8),
        2 => decode_into(bytes, data as *mut u16),
        _ => decode_into(bytes, data as *mut u32),
    }
    s
}

#[inline(always)]
fn utf8_count_and_max(bytes: &[u8]) -> (usize, u8) {
    let n = bytes.len();
    let mut i = 0;
    let mut cont = 0usize;
    #[allow(unused_mut, unused_assignments)]
    let mut max_lead = 0u8;
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use std::arch::x86_64::*;
        let lim = _mm_set1_epi8(-64); // bytes 0x80..=0xBF are < -64 as i8
        let mut vmax = _mm_setzero_si128();
        while i + 16 <= n {
            let v = _mm_loadu_si128(bytes.as_ptr().add(i) as *const __m128i);
            cont += (_mm_movemask_epi8(_mm_cmplt_epi8(v, lim)) as u32).count_ones() as usize;
            vmax = _mm_max_epu8(vmax, v);
            i += 16;
        }
        let mut lanes = [0u8; 16];
        _mm_storeu_si128(lanes.as_mut_ptr() as *mut __m128i, vmax);
        max_lead = lanes.iter().copied().max().unwrap_or(0);
    }
    for &b in &bytes[i..] {
        cont += ((b & 0xC0) == 0x80) as usize;
        max_lead = max_lead.max(b);
    }
    (n - cont, max_lead)
}

trait CodeUnit: Copy {
    fn from_u32(c: u32) -> Self;
}
impl CodeUnit for u8 {
    #[inline(always)]
    fn from_u32(c: u32) -> Self {
        c as u8
    }
}
impl CodeUnit for u16 {
    #[inline(always)]
    fn from_u32(c: u32) -> Self {
        c as u16
    }
}
impl CodeUnit for u32 {
    #[inline(always)]
    fn from_u32(c: u32) -> Self {
        c
    }
}

#[inline(always)]
unsafe fn decode_into<T: CodeUnit>(bytes: &[u8], mut out: *mut T) {
    let mut i = 0;
    let n = bytes.len();
    let p = bytes.as_ptr();
    while i < n {
        let b0 = *p.add(i) as u32;
        if b0 < 0x80 {
            // ASCII: try a run of 8 at once, else a single unit.
            if i + 8 <= n {
                let w = ptr::read_unaligned(p.add(i) as *const u64);
                if w & 0x8080_8080_8080_8080 == 0 {
                    for k in 0..8 {
                        *out.add(k) = T::from_u32(*p.add(i + k) as u32);
                    }
                    out = out.add(8);
                    i += 8;
                    continue;
                }
            }
            *out = T::from_u32(b0);
            out = out.add(1);
            i += 1;
            continue;
        }
        let c = if b0 < 0xE0 {
            let c = ((b0 & 0x1F) << 6) | (*p.add(i + 1) as u32 & 0x3F);
            i += 2;
            c
        } else if b0 < 0xF0 {
            let c = ((b0 & 0x0F) << 12) | ((*p.add(i + 1) as u32 & 0x3F) << 6) | (*p.add(i + 2) as u32 & 0x3F);
            i += 3;
            c
        } else {
            let c = ((b0 & 0x07) << 18)
                | ((*p.add(i + 1) as u32 & 0x3F) << 12)
                | ((*p.add(i + 2) as u32 & 0x3F) << 6)
                | (*p.add(i + 3) as u32 & 0x3F);
            i += 4;
            c
        };
        *out = T::from_u32(c);
        out = out.add(1);
    }
}

// ============================================================================
// Entry point
// ============================================================================

/// Borrowed view of the input document plus whatever keeps it alive.
pub(crate) struct Input {
    pub(crate) ptr: *const u8,
    pub(crate) len: usize,
    pub(crate) utf8_valid: bool,
    view: Option<Box<ffi::Py_buffer>>,
}

impl Drop for Input {
    fn drop(&mut self) {
        if let Some(view) = self.view.as_mut() {
            unsafe { ffi::PyBuffer_Release(&mut **view) };
        }
    }
}

pub(crate) unsafe fn get_input(py: Python<'_>, obj: *mut ffi::PyObject) -> PyResult<Input> {
    if ffi::PyUnicode_Check(obj) != 0 {
        let mut size: ffi::Py_ssize_t = 0;
        let p = ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);
        if p.is_null() {
            ffi::PyErr_Clear();
            return Err(raise_decode_error(py, "str is not valid UTF-8: surrogates not allowed", b"", 0));
        }
        return Ok(Input { ptr: p as *const u8, len: size as usize, utf8_valid: true, view: None });
    }
    if ffi::PyBytes_Check(obj) != 0 {
        return Ok(Input {
            ptr: ffi::PyBytes_AsString(obj) as *const u8,
            len: ffi::PyBytes_Size(obj) as usize,
            utf8_valid: false,
            view: None,
        });
    }
    if ffi::PyByteArray_Check(obj) != 0 {
        return Ok(Input {
            ptr: ffi::PyByteArray_AsString(obj) as *const u8,
            len: ffi::PyByteArray_Size(obj) as usize,
            utf8_valid: false,
            view: None,
        });
    }
    if ffi::PyMemoryView_Check(obj) != 0 {
        let mut view: Box<ffi::Py_buffer> = Box::new(std::mem::zeroed());
        if ffi::PyObject_GetBuffer(obj, &mut *view, ffi::PyBUF_C_CONTIGUOUS) == 0 {
            return Ok(Input {
                ptr: view.buf as *const u8,
                len: view.len as usize,
                utf8_valid: false,
                view: Some(view),
            });
        }
        return Err(PyErr::fetch(py));
    }
    Err(PyTypeError::new_err("Input must be bytes, bytearray, memoryview, or str"))
}

#[inline(always)]
fn pause_gc() -> bool {
    #[cfg(all(Py_3_10, not(Py_3_12)))]
    {
        unsafe { ffi::PyGC_Disable() != 0 }
    }
    #[cfg(not(all(Py_3_10, not(Py_3_12))))]
    {
        false
    }
}

#[inline(always)]
fn resume_gc(was_enabled: bool) {
    #[cfg(all(Py_3_10, not(Py_3_12)))]
    if was_enabled {
        unsafe { ffi::PyGC_Enable() };
    }
    #[cfg(not(all(Py_3_10, not(Py_3_12))))]
    let _ = was_enabled;
}

// Pooled buffers, only accessed with the GIL held.
static mut STACK_POOL: Vec<*mut ffi::PyObject> = Vec::new();
static mut SCRATCH_POOL: Vec<u8> = Vec::new();
const MAX_POOLED_STACK: usize = 1 << 17;
const MAX_POOLED_SCRATCH: usize = 1 << 20;

/// Core entry point, independent of how the input bytes were obtained.
///
/// `utf8_valid`: the bytes came from a Python `str` (known-valid UTF-8);
/// otherwise they are validated. Returns a new reference, or raises
/// `json.JSONDecodeError` (a `ValueError`).
pub(crate) fn parse(py: Python<'_>, buf: &[u8], utf8_valid: bool) -> PyResult<*mut ffi::PyObject> {
    let mut p = Parser {
        buf,
        pos: 0,
        // bytes-like input: one SIMD validation pass up front is much cheaper
        // than validating each non-ASCII string. If it fails, per-string
        // validation pinpoints the error position.
        utf8_valid: utf8_valid || simdutf8::basic::from_utf8(buf).is_ok(),
        depth: 0,
        // Reuse the value stack / scratch buffer across calls. `take` leaves
        // an empty Vec behind, so a re-entrant call (e.g. from a finalizer run
        // by the GC during allocation) simply allocates its own.
        stack: unsafe { std::mem::take(&mut *ptr::addr_of_mut!(STACK_POOL)) },
        scratch: unsafe { std::mem::take(&mut *ptr::addr_of_mut!(SCRATCH_POOL)) },
        error: std::cell::Cell::new(("", 0)),
    };
    p.skip_ws();
    // Pause the cyclic GC while building the document (CPython < 3.12 only).
    // Allocating tens of thousands of lists/dicts otherwise triggers many
    // gen0/gen1(/gen2) collections that traverse objects which are all
    // reachable. The previous state is restored, and a GC the user disabled
    // stays disabled. On 3.12+ collections are deferred to the eval loop, so
    // this is unnecessary there.
    let gc_was_enabled = pause_gc();
    let result = p.parse_value().and_then(|v| {
        p.skip_ws();
        if p.pos != buf.len() {
            unsafe { ffi::Py_DECREF(v) };
            p.err("unexpected content after document", p.pos)
        } else {
            Ok(v)
        }
    });
    resume_gc(gc_was_enabled);
    // Release every partially-built value (non-empty only on error).
    for &o in p.stack.iter() {
        unsafe { ffi::Py_DECREF(o) };
    }
    p.stack.clear();
    unsafe {
        if p.stack.capacity() <= MAX_POOLED_STACK {
            *ptr::addr_of_mut!(STACK_POOL) = std::mem::take(&mut p.stack);
        }
        if p.scratch.capacity() <= MAX_POOLED_SCRATCH {
            *ptr::addr_of_mut!(SCRATCH_POOL) = std::mem::take(&mut p.scratch);
        }
    }
    match result {
        Ok(v) => Ok(v),
        Err(Fail) => {
            let (emsg, epos) = p.error.get();
            let msg = if buf.is_empty() || buf.iter().all(|&b| is_ws(b)) {
                "input data is empty"
            } else if epos == 0 && buf.starts_with(b"\xEF\xBB\xBF") {
                "UTF-8 byte order mark (BOM) is not supported"
            } else {
                emsg
            };
            Err(raise_decode_error(py, msg, buf, epos))
        }
    }
}
