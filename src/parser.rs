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
        Ok(m.getattr("JSONDecodeError")?
            .downcast_into::<PyType>()?
            .unbind())
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
    fn parse_literal(
        &mut self,
        lit: &'static [u8],
        obj: *mut ffi::PyObject,
    ) -> PResult<*mut ffi::PyObject> {
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
    fn cached_key(
        &mut self,
        start: usize,
        end: usize,
        non_ascii: bool,
    ) -> PResult<*mut ffi::PyObject> {
        let raw = unsafe { self.buf.get_unchecked(start..end) };
        unsafe {
            let idx = (hash_key(raw) as usize) & (KEY_CACHE_SIZE - 1);
            let entry = &mut *ptr::addr_of_mut!(KEY_CACHE[idx]);
            if !entry.obj.is_null()
                && entry.len as usize == raw.len()
                && entry.bytes.get_unchecked(..raw.len()) == raw
            {
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
            entry
                .bytes
                .get_unchecked_mut(..raw.len())
                .copy_from_slice(raw);
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

    /// Strings containing escapes. `start` is the first content byte, `i`
    /// the first backslash, `non_ascii` whether `buf[start..i]` has non-ASCII
    /// bytes. Decodes into the scratch buffer and leaves `self.pos` after the
    /// closing quote.
    ///
    /// The bulk of the string goes through a block kernel (`escape_block_*`)
    /// that classifies 32 input bytes at once and then handles every escape
    /// in the block from the bitmask. Anything unusual (control characters,
    /// invalid escapes, lone surrogates) and the last few bytes of the input
    /// are handed to the scalar `parse_escaped_tail`, which also produces
    /// every error message, so error semantics don't depend on the kernel.
    #[inline(never)]
    fn parse_escaped(
        &mut self,
        start: usize,
        i: usize,
        non_ascii: bool,
    ) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        out.reserve(i - start + 256);
        out.extend_from_slice(&buf[start..i]);
        let mut st = EscState {
            i,
            olen: out.len(),
            non_ascii,
            done: false,
        };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            // SAFETY: the kernels only read `buf` while `i + ESC_LOOKAHEAD <=
            // buf.len()` and reserve their worst-case output first.
            if std::arch::is_x86_feature_detected!("avx2") {
                escape_blocks_avx2(buf, &mut out, &mut st);
            } else {
                escape_blocks_sse2(buf, &mut out, &mut st);
            }
        }
        unsafe { out.set_len(st.olen) };
        let r = if st.done {
            self.pos = st.i;
            Ok(())
        } else {
            self.parse_escaped_tail(st.i, &mut out, &mut st.non_ascii)
        };
        let res = r.and_then(|()| {
            let s = if !st.non_ascii {
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

    /// Scalar continuation of `parse_escaped` from input position `i` (any
    /// byte); `out` holds everything decoded so far. Used near the end of the
    /// input and for every error.
    #[inline(never)]
    fn parse_escaped_tail(
        &mut self,
        mut i: usize,
        out: &mut Vec<u8>,
        non_ascii: &mut bool,
    ) -> PResult<()> {
        let buf = self.buf;
        let len = buf.len();
        loop {
            let b = self.peek_at(i);
            if b == b'"' {
                self.pos = i + 1;
                return Ok(());
            } else if b == b'\\' {
                let c = self.peek_at(i + 1);
                let e = ESCAPE_LUT[c as usize];
                if e != 0 {
                    out.push(e);
                    i += 2;
                } else if c == b'u' {
                    let (next, na) = self.unescape_unicode(i, out)?;
                    i = next;
                    *non_ascii |= na;
                } else if i + 1 >= len {
                    return self.err("unexpected end of data in string", i + 1);
                } else {
                    return self.err("invalid escaped sequence in string", i);
                }
            } else if b < 0x20 {
                return self.err_string(i);
            } else {
                *non_ascii |= b >= 0x80;
                out.push(b);
                i += 1;
            }
        }
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
            let w =
                u64::from_le(unsafe { ptr::read_unaligned(buf.as_ptr().add(*i) as *const u64) });
            let non_digit = (w.wrapping_sub(0x3030_3030_3030_3030)
                | w.wrapping_add(0x4646_4646_4646_4646))
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
        let start = self.pos;
        if start + NUM_FAST_LOOKAHEAD <= self.buf.len() {
            // SAFETY: the fast path reads at most NUM_FAST_LOOKAHEAD bytes
            // from `start`.
            if let Some(r) = unsafe { self.parse_number_fast(start) } {
                return r;
            }
        }
        self.parse_number_general(start)
    }

    /// One-pass fast path for the common number shapes: `-?\d{1,15}` and
    /// `-?\d{1,15}\.\d{1,15}` with at most 19 digits in total and no
    /// exponent. Integer and fraction digits are each read as two 8-byte
    /// words. Returns None (without consuming anything) for every other
    /// shape, including all invalid ones, so the general parser keeps sole
    /// ownership of error reporting and of the rare shapes.
    ///
    /// SAFETY: `start + NUM_FAST_LOOKAHEAD <= self.buf.len()`.
    #[inline(always)]
    unsafe fn parse_number_fast(&mut self, start: usize) -> Option<PResult<*mut ffi::PyObject>> {
        let p = self.buf.as_ptr();
        let neg = *p.add(start) == b'-';
        let i = start + neg as usize;
        let (int, n1) = digits16(p.add(i))?;
        // No digit, or a leading zero ("01"): the general parser reports it.
        if n1 == 0 || (n1 > 1 && *p.add(i) == b'0') {
            return None;
        }
        let mut j = i + n1;
        let c = *p.add(j);
        if c == b'.' {
            let (frac, n2) = digits16(p.add(j + 1))?;
            if n2 == 0 || n1 + n2 > 19 {
                return None;
            }
            j += 1 + n2;
            if (*p.add(j) | 0x20) == b'e' {
                return None;
            }
            // < 10^19: exact in a u64.
            let mant = int * POW10_U64[n2] + frac;
            let v = if mant <= (1u64 << 53) {
                // Clinger: exact mantissa and power of ten (n2 <= 15 <= 22),
                // so one correctly rounded division.
                mant as f64 / POW10[n2]
            } else {
                crate::lemire::compute_float64(-(n2 as i64), mant)?
            };
            self.pos = j;
            let r = ffi::PyFloat_FromDouble(if neg { -v } else { v });
            return Some(if r.is_null() { self.err_oom() } else { Ok(r) });
        }
        if (c | 0x20) == b'e' {
            return None;
        }
        self.pos = j;
        // At most 15 digits: fits an i64.
        let v = int as i64;
        let r = ffi::PyLong_FromLongLong(if neg { -v } else { v });
        Some(if r.is_null() { self.err_oom() } else { Ok(r) })
    }

    #[inline(never)]
    fn parse_number_general(&mut self, start: usize) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
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
    fn parse_float_tail(
        &mut self,
        start: usize,
        mut i: usize,
        neg: bool,
        mut mant: u64,
        mut nd: usize,
    ) -> PResult<*mut ffi::PyObject> {
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
                Some(if e >= 0 {
                    m * POW10[e as usize]
                } else {
                    m / POW10[(-e) as usize]
                })
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
            if let Some(v) = std::str::from_utf8(s)
                .ok()
                .and_then(|t| t.parse::<u64>().ok())
            {
                return unsafe { Ok(ffi::PyLong_FromUnsignedLongLong(v)) };
            }
        }
        let mut tmp = Vec::with_capacity(s.len() + 1);
        tmp.extend_from_slice(s);
        tmp.push(0);
        unsafe {
            let r = ffi::PyLong_FromString(
                tmp.as_ptr() as *const std::os::raw::c_char,
                ptr::null_mut(),
                10,
            );
            if r.is_null() {
                ffi::PyErr_Clear();
                return self.err("invalid number", start);
            }
            Ok(r)
        }
    }
}

/// Bytes `parse_number_fast` may read from the start of the number: sign,
/// 16 integer-digit bytes, '.', 16 fraction-digit bytes, and the byte after.
const NUM_FAST_LOOKAHEAD: usize = 40;

/// Number of leading ASCII digits in the 8 bytes of `w` (little endian).
#[inline(always)]
fn digit_run(w: u64) -> usize {
    let non_digit = (w.wrapping_sub(0x3030_3030_3030_3030) | w.wrapping_add(0x4646_4646_4646_4646))
        & 0x8080_8080_8080_8080;
    (non_digit.trailing_zeros() / 8) as usize
}

/// Value of the first `n` (0..=8) ASCII digits of `w`.
#[inline(always)]
fn parse_digits_prefix(w: u64, n: usize) -> u64 {
    // Convert to digit values first (the low `n` bytes are digits, so no
    // borrow reaches them), then shift the digits to the top: the vacated
    // low bytes become leading zeros. Two shifts keep n == 0 (a total shift
    // of 64) well defined.
    let half = 4 * (8 - n) as u32;
    let d = (w.wrapping_sub(0x3030_3030_3030_3030) << half) << half;
    digits8_value(d)
}

/// Leading decimal digits at `p`: (value, count) for up to 15 digits, None
/// for 16 or more.
///
/// SAFETY: `p..p+16` readable.
#[inline(always)]
unsafe fn digits16(p: *const u8) -> Option<(u64, usize)> {
    let w1 = u64::from_le(ptr::read_unaligned(p as *const u64));
    let na = digit_run(w1);
    if na < 8 {
        return Some((parse_digits_prefix(w1, na), na));
    }
    let w2 = u64::from_le(ptr::read_unaligned(p.add(8) as *const u64));
    let nb = digit_run(w2);
    if nb == 8 {
        return None;
    }
    Some((
        parse_8digits(w1) * POW10_U64[nb] + parse_digits_prefix(w2, nb),
        8 + nb,
    ))
}

#[inline(always)]
fn parse_8digits(v: u64) -> u64 {
    digits8_value(v - 0x3030_3030_3030_3030)
}

/// Value of 8 digit values (0..=9, one per byte, most significant first in
/// memory order).
#[inline(always)]
fn digits8_value(mut v: u64) -> u64 {
    const MASK: u64 = 0x0000_00FF_0000_00FF;
    const MUL1: u64 = 0x000F_4240_0000_0064;
    const MUL2: u64 = 0x0000_2710_0000_0001;
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

/// Hex digit value, or 0xFF for a non-hex byte.
const HEX_LUT: [u8; 256] = {
    let mut t = [0xFFu8; 256];
    let mut c = 0;
    while c < 10 {
        t[b'0' as usize + c] = c as u8;
        c += 1;
    }
    let mut c = 0;
    while c < 6 {
        t[b'a' as usize + c] = 10 + c as u8;
        t[b'A' as usize + c] = 10 + c as u8;
        c += 1;
    }
    t
};

/// State shared between `parse_escaped`, the block kernels and the scalar
/// tail. `i`: next unprocessed input byte; `olen`: bytes written to the
/// output (the Vec's len is not kept in sync inside the kernels); `done`: the
/// closing quote was consumed and `i` is the position after it.
struct EscState {
    i: usize,
    olen: usize,
    non_ascii: bool,
    done: bool,
}

/// Input bytes a kernel block may touch past its start: the 32-byte block,
/// a 32-byte copy starting anywhere in it, and a surrogate-pair escape
/// (12 bytes) starting at its last byte.
#[cfg(target_arch = "x86_64")]
const ESC_LOOKAHEAD: usize = 64;
/// Output bytes a kernel block may write past `olen` at block start: up to
/// 32 + 12 bytes consumed (output never exceeds input), plus a 32-byte copy
/// overrun or a 4-byte store.
#[cfg(target_arch = "x86_64")]
const ESC_OUT_MARGIN: usize = 96;

/// Decode `\uXXXX` (or a surrogate pair) at `p` into UTF-8 at `o`, writing 4
/// bytes unconditionally. Returns (input consumed, output bytes), or None
/// for anything invalid (the scalar tail then reports the error).
///
/// SAFETY: `p..p+12` readable, `o..o+4` writable.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn unescape_u_fast(p: *const u8, o: *mut u8) -> Option<(usize, usize)> {
    #[inline(always)]
    unsafe fn hex4(p: *const u8) -> Option<u32> {
        let a = HEX_LUT[*p as usize] as u32;
        let b = HEX_LUT[*p.add(1) as usize] as u32;
        let c = HEX_LUT[*p.add(2) as usize] as u32;
        let d = HEX_LUT[*p.add(3) as usize] as u32;
        if (a | b | c | d) & 0xF0 != 0 {
            return None;
        }
        Some((a << 12) | (b << 8) | (c << 4) | d)
    }
    let cp = hex4(p.add(2))?;
    let (w, n, consumed): (u32, usize, usize) = if cp < 0x80 {
        (cp, 1, 6)
    } else if cp < 0x800 {
        (0x80C0 | (cp >> 6) | ((cp & 0x3F) << 8), 2, 6)
    } else if !(0xD800..0xE000).contains(&cp) {
        (
            0x8080E0 | (cp >> 12) | (((cp >> 6) & 0x3F) << 8) | ((cp & 0x3F) << 16),
            3,
            6,
        )
    } else {
        if cp >= 0xDC00 || *p.add(6) != b'\\' || *p.add(7) != b'u' {
            return None;
        }
        let lo = hex4(p.add(8))?;
        if !(0xDC00..0xE000).contains(&lo) {
            return None;
        }
        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
        (
            0x808080F0
                | (c >> 18)
                | (((c >> 12) & 0x3F) << 8)
                | (((c >> 6) & 0x3F) << 16)
                | ((c & 0x3F) << 24),
            4,
            12,
        )
    };
    ptr::write_unaligned(o as *mut u32, w.to_le());
    Some((consumed, n))
}

/// Block kernel body shared by the SSE2 and AVX2 variants. `masks(src)`
/// returns (special, high) bitmasks for the 32 bytes at `src`: bit k of
/// `special` is set for `"`, `\\` or a control character at `src+k`, bit k of
/// `high` for a byte >= 0x80. `copy32(src, dst)` copies 32 bytes.
///
/// Every escape in a block is handled from its bitmask without re-scanning:
/// the plain run before it is copied with one unconditional 32-byte copy.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn escape_blocks<M, C>(buf: &[u8], out: &mut Vec<u8>, st: &mut EscState, masks: M, copy32: C)
where
    M: Fn(*const u8) -> (u32, u32),
    C: Fn(*const u8, *mut u8),
{
    let len = buf.len();
    let mut i = st.i;
    let mut olen = st.olen;
    let mut high_acc = 0u32;
    'blocks: while i + ESC_LOOKAHEAD <= len {
        if out.capacity() - olen < ESC_OUT_MARGIN {
            out.set_len(olen);
            out.reserve(ESC_OUT_MARGIN * 4);
        }
        let src = buf.as_ptr().add(i);
        let dst = out.as_mut_ptr();
        let (special, high) = masks(src);
        let special = special as u64;
        let mut p = 0usize; // offset of the next unprocessed byte in the block
        loop {
            let rem = special & (!0u64 << p);
            if rem == 0 {
                if p < 32 {
                    copy32(src.add(p), dst.add(olen));
                    olen += 32 - p;
                    p = 32;
                }
                high_acc |= high;
                i += p;
                continue 'blocks;
            }
            let t = rem.trailing_zeros() as usize;
            copy32(src.add(p), dst.add(olen));
            olen += t - p;
            let c = *src.add(t);
            if c == b'\\' {
                let e = ESCAPE_LUT[*src.add(t + 1) as usize];
                if e != 0 {
                    *dst.add(olen) = e;
                    olen += 1;
                    p = t + 2;
                    continue;
                }
                if *src.add(t + 1) == b'u' {
                    if let Some((consumed, n)) = unescape_u_fast(src.add(t), dst.add(olen)) {
                        st.non_ascii |= n > 1;
                        olen += n;
                        p = t + consumed;
                        continue;
                    }
                }
            } else if c == b'"' {
                high_acc |= high & ((1u32 << t) - 1);
                st.i = i + t + 1;
                st.done = true;
                break 'blocks;
            }
            // Invalid escape or control character: the scalar tail reports it.
            high_acc |= high & ((1u32 << t) - 1);
            i += t;
            break 'blocks;
        }
    }
    if !st.done {
        st.i = i;
    }
    st.olen = olen;
    st.non_ascii |= high_acc != 0;
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
unsafe fn escape_blocks_sse2(buf: &[u8], out: &mut Vec<u8>, st: &mut EscState) {
    use std::arch::x86_64::*;
    let quote = _mm_set1_epi8(b'"' as i8);
    let bslash = _mm_set1_epi8(b'\\' as i8);
    let ctl = _mm_set1_epi8(0x1F);
    let masks = |src: *const u8| {
        let m16 = |v: __m128i| {
            let m = _mm_or_si128(
                _mm_or_si128(_mm_cmpeq_epi8(v, quote), _mm_cmpeq_epi8(v, bslash)),
                _mm_cmpeq_epi8(_mm_max_epu8(v, ctl), ctl),
            );
            (_mm_movemask_epi8(m) as u32, _mm_movemask_epi8(v) as u32)
        };
        let (s0, h0) = m16(_mm_loadu_si128(src as *const __m128i));
        let (s1, h1) = m16(_mm_loadu_si128(src.add(16) as *const __m128i));
        (s0 | (s1 << 16), h0 | (h1 << 16))
    };
    let copy32 = |s: *const u8, d: *mut u8| {
        let a = _mm_loadu_si128(s as *const __m128i);
        let b = _mm_loadu_si128(s.add(16) as *const __m128i);
        _mm_storeu_si128(d as *mut __m128i, a);
        _mm_storeu_si128(d.add(16) as *mut __m128i, b);
    };
    escape_blocks(buf, out, st, masks, copy32)
}

#[cfg(target_arch = "x86_64")]
#[inline(never)]
#[target_feature(enable = "avx2")]
unsafe fn escape_blocks_avx2(buf: &[u8], out: &mut Vec<u8>, st: &mut EscState) {
    use std::arch::x86_64::*;
    let quote = _mm256_set1_epi8(b'"' as i8);
    let bslash = _mm256_set1_epi8(b'\\' as i8);
    let ctl = _mm256_set1_epi8(0x1F);
    let masks = |src: *const u8| {
        let v = _mm256_loadu_si256(src as *const __m256i);
        let m = _mm256_or_si256(
            _mm256_or_si256(_mm256_cmpeq_epi8(v, quote), _mm256_cmpeq_epi8(v, bslash)),
            _mm256_cmpeq_epi8(_mm256_max_epu8(v, ctl), ctl),
        );
        (
            _mm256_movemask_epi8(m) as u32,
            _mm256_movemask_epi8(v) as u32,
        )
    };
    let copy32 = |s: *const u8, d: *mut u8| {
        _mm256_storeu_si256(d as *mut __m256i, _mm256_loadu_si256(s as *const __m256i));
    };
    escape_blocks(buf, out, st, masks, copy32)
}

const POW10_U64: [u64; 20] = {
    let mut t = [1u64; 20];
    let mut k = 1;
    while k < 20 {
        t[k] = t[k - 1] * 10;
        k += 1;
    }
    t
};

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
        ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            ffi::PyUnicode_DATA(s) as *mut u8,
            bytes.len(),
        );
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
            let c = ((b0 & 0x0F) << 12)
                | ((*p.add(i + 1) as u32 & 0x3F) << 6)
                | (*p.add(i + 2) as u32 & 0x3F);
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
            return Err(raise_decode_error(
                py,
                "str is not valid UTF-8: surrogates not allowed",
                b"",
                0,
            ));
        }
        return Ok(Input {
            ptr: p as *const u8,
            len: size as usize,
            utf8_valid: true,
            view: None,
        });
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
    Err(PyTypeError::new_err(
        "Input must be bytes, bytearray, memoryview, or str",
    ))
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
