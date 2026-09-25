//! Hand-written single-pass JSON parser that builds CPython objects directly.
//!
//! Replaces the serde_json Deserializer + Visitor path for `loads`. Design:
//!
//! * Input is borrowed: `str` via `PyUnicode_AsUTF8AndSize` (zero-copy for
//!   compact ASCII strings, cached UTF-8 otherwise), `bytes`/`bytearray`
//!   directly; `memoryview` is copied. All of these are followed by a NUL
//!   byte, so `peek()` needs no bounds check (see `parse`). Bytes input is
//!   validated once up front with `simdutf8`.
//! * Recursive descent with one value stack (`Vec<*mut PyObject>`, pooled
//!   across calls). Array elements and object key/value pairs are pushed on
//!   the stack; the container is created at the closing bracket with its
//!   exact size (lists: a `PyMem_Malloc` item array attached to an empty
//!   list; dicts: `_PyDict_FromItems` on 3.13, else `PyDict_SetItem`). On
//!   error, every reference still on the stack is released: no leaks.
//! * Strings: SSE2 scan for `"`, `\\` and control characters that also
//!   detects non-ASCII bytes. ASCII strings are `PyUnicode_New(len, 127)` plus
//!   memcpy. Non-ASCII strings are decoded from UTF-8 straight into the final
//!   UCS1/UCS2/UCS4 buffer (one SIMD counting pass, one write pass). Strings
//!   with escapes go through a 32-byte block kernel (SSE2 / AVX2 at runtime)
//!   that handles every escape of a block from its bitmask.
//! * Object keys go through a direct-mapped cache (`KEY_CACHE`) keyed on the
//!   raw key bytes. Cached keys are reused across calls, carry a precomputed
//!   hash, and are compared byte-for-byte on lookup (no false hits).
//! * Numbers: a fast path for `-?d{1,15}(.d{1,15})?(e[+-]?d{1,4})?` with
//!   <= 19 digits (scalar integer part, 8-byte SWAR fraction words), Clinger's
//!   exact path or Eisel-Lemire (`crate::lemire`); everything else, and every
//!   error, goes through the general parser (big ints exact via
//!   `PyLong_FromString`, >19-digit floats via `fast_float`). All floats are
//!   correctly rounded.
//! * Nesting depth is limited to 1024 (same as orjson).
//! * On CPython 3.10/3.11 the cyclic GC is paused while parsing (see
//!   `pause_gc`).

use pyo3::exceptions::PyTypeError;
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
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

const EMPTY_KEY: KeyEntry = KeyEntry { obj: ptr::null_mut(), len: 0, bytes: [0; KEY_CACHE_MAX_LEN] };

/// Direct-mapped cache of dict-key strings. Only touched with the GIL held
/// (every `loads` call holds it), so no further synchronization is needed on
/// GIL-enabled CPython builds.
static mut KEY_CACHE: [KeyEntry; KEY_CACHE_SIZE] = [EMPTY_KEY; KEY_CACHE_SIZE];

/// Key-cache hash of the `n <= 64` key bytes at `p`: a folded 64x64->128
/// multiply of the first and last 8 bytes (overlapping for n >= 8; zero
/// padded below) and the length. It doesn't cover the middle of keys longer
/// than 16 bytes; lookups compare all bytes, so a collision only costs a
/// cache miss.
///
/// SAFETY: `p..p+KEY_CACHE_MAX_LEN` readable.
#[inline(always)]
unsafe fn hash_key(p: *const u8, n: usize) -> u64 {
    let first = u64::from_le(ptr::read_unaligned(p as *const u64));
    let (a, b) = if n >= 8 {
        (first, u64::from_le(ptr::read_unaligned(p.add(n - 8) as *const u64)))
    } else {
        // Keep the low n bytes (n * 8 < 64).
        (first & ((1u64 << (8 * n)) - 1), 0)
    };
    let x = ((a ^ 0x243F_6A88_85A3_08D3) as u128) * ((b ^ (n as u64) ^ 0x1319_8A2E_0370_7344) as u128);
    (x as u64) ^ ((x >> 64) as u64)
}

/// Whether the `n <= 64` bytes at `a` and `b` are equal.
///
/// SAFETY: `a..a+64` and `b..b+64` readable.
#[inline(always)]
unsafe fn key_bytes_eq(a: *const u8, b: *const u8, n: usize) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::*;
        let mut off = 0;
        loop {
            let eq = _mm_movemask_epi8(_mm_cmpeq_epi8(
                _mm_loadu_si128(a.add(off) as *const __m128i),
                _mm_loadu_si128(b.add(off) as *const __m128i),
            )) as u32;
            if n - off <= 16 {
                let need = (1u32 << (n - off)) - 1;
                return eq & need == need;
            }
            if eq != 0xFFFF {
                return false;
            }
            off += 16;
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        std::slice::from_raw_parts(a, n) == std::slice::from_raw_parts(b, n)
    }
}

// ============================================================================
// Error type
// ============================================================================

/// Zero-sized failure marker: the message and position are stored in the
/// parser (`Parser::error`) so `PResult<*mut PyObject>` stays register-sized.
#[derive(Debug)]
struct Fail;

type PResult<T> = Result<T, Fail>;

static JSON_DECODE_ERROR: PyOnceLock<Py<PyType>> = PyOnceLock::new();

/// `json.JSONDecodeError`, which `loads` raises and the module exports as
/// `rjson.JSONDecodeError` (the same object, so either name catches it).
pub(crate) fn decode_error_type(py: Python<'_>) -> PyResult<&Py<PyType>> {
    JSON_DECODE_ERROR.get_or_try_init(py, || -> PyResult<Py<PyType>> {
        let m = py.import("json")?;
        Ok(m.getattr("JSONDecodeError")?.cast_into::<PyType>()?.unbind())
    })
}

#[cold]
#[inline(never)]
fn raise_decode_error(py: Python<'_>, msg: &str, doc: &[u8], pos: usize) -> PyErr {
    // Raise json.JSONDecodeError (a ValueError subclass, like orjson) with a
    // character position. Fall back to ValueError if json can't be imported.
    let doc_str = String::from_utf8_lossy(doc);
    let pos = pos.min(doc.len());
    let char_pos = doc[..pos].iter().filter(|&&b| (b & 0xC0) != 0x80).count();
    // `msg` is the bare reason, as with json/orjson (`exc.msg`); JSONDecodeError
    // formats str(exc) as "<msg>: line L column C (char P)".
    let ty = decode_error_type(py);
    match ty {
        Ok(ty) => PyErr::from_type(ty.bind(py).clone(), (msg.to_owned(), doc_str.into_owned(), char_pos)),
        Err(_) => pyo3::exceptions::PyValueError::new_err(msg.to_owned()),
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
        // SAFETY: `pos <= buf.len()` always holds and `buf` is followed by a
        // readable NUL byte (see `parse`), so this reads either a document
        // byte or the terminating 0, which no token starts with.
        debug_assert!(self.pos <= self.buf.len());
        unsafe { *self.buf.as_ptr().add(self.pos) }
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

    /// Trailing comma before `]`/`}` (at `self.pos`). Reported at the comma,
    /// the last non-whitespace byte before the bracket, like `json` and
    /// orjson (`[1,]` -> char 2, column 3); found by scanning back so the
    /// success path does not track the comma's position.
    #[cold]
    #[inline(never)]
    fn err_trailing_comma<T>(&self) -> PResult<T> {
        let mut i = self.pos.min(self.buf.len());
        while i > 0 && is_ws(self.buf[i - 1]) {
            i -= 1;
        }
        self.err("trailing comma is not allowed", i.saturating_sub(1))
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
                            return self.err_trailing_comma();
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
            let list = list_from_items(self.stack.as_ptr().add(base), n);
            if list.is_null() {
                // The items are still on the stack and released by `parse`.
                return self.err_oom();
            }
            self.stack.set_len(base);
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
                            return self.err_trailing_comma();
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
            let dict = build_dict(self.stack.as_ptr().add(base), n);
            for &o in self.stack.get_unchecked(base..) {
                ffi::Py_DECREF(o);
            }
            self.stack.set_len(base);
            if dict.is_null() {
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
        let n = end - start;
        unsafe {
            // The hash and compare read 64 bytes from the key start; near the
            // end of the input, work on a zero-padded copy instead (same hash).
            let mut tmp = [0u8; KEY_CACHE_MAX_LEN];
            let p = if start + KEY_CACHE_MAX_LEN <= self.buf.len() {
                self.buf.as_ptr().add(start)
            } else {
                tmp.get_unchecked_mut(..n).copy_from_slice(self.buf.get_unchecked(start..end));
                tmp.as_ptr()
            };
            let idx = (hash_key(p, n) as usize) & (KEY_CACHE_SIZE - 1);
            let entry = &mut *ptr::addr_of_mut!(KEY_CACHE[idx]);
            if !entry.obj.is_null() && entry.len as usize == n && key_bytes_eq(entry.bytes.as_ptr(), p, n) {
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
            entry.len = n as u32;
            entry.bytes.get_unchecked_mut(..n).copy_from_slice(self.buf.get_unchecked(start..end));
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
    fn parse_escaped(&mut self, start: usize, i: usize, non_ascii: bool) -> PResult<*mut ffi::PyObject> {
        let buf = self.buf;
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        out.reserve(i - start + 256);
        out.extend_from_slice(&buf[start..i]);
        let mut st = EscState { i, olen: out.len(), non_ascii, done: false };
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
    fn parse_escaped_tail(&mut self, mut i: usize, out: &mut Vec<u8>, non_ascii: &mut bool) -> PResult<()> {
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
            let w = u64::from_le(unsafe { ptr::read_unaligned(buf.as_ptr().add(*i) as *const u64) });
            let non_digit =
                (w.wrapping_sub(0x3030_3030_3030_3030) | w.wrapping_add(0x4646_4646_4646_4646)) & 0x8080_8080_8080_8080;
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

    /// Fast path for the common number shapes: `-?\d{1,15}(\.\d{1,15})?`
    /// with at most 19 digits in total, optionally followed by an exponent
    /// of at most 4 digits. Returns None (without consuming anything) for
    /// every other shape, including all invalid ones, so the general parser
    /// keeps sole ownership of error reporting and of the rare shapes.
    ///
    /// The integer part uses a scalar loop: it is usually short and its
    /// length predictable, and with predicted branches the rest of the
    /// parse doesn't wait on a data-dependent digit count. The fraction is
    /// read as two 8-byte words (branch-free over its length, which varies
    /// unpredictably in real data).
    ///
    /// SAFETY: `start + NUM_FAST_LOOKAHEAD <= self.buf.len()`, and the input
    /// is followed by a NUL byte (bounds the integer loop).
    #[inline(always)]
    unsafe fn parse_number_fast(&mut self, start: usize) -> Option<PResult<*mut ffi::PyObject>> {
        let p = self.buf.as_ptr();
        let neg = *p.add(start) == b'-';
        let i = start + neg as usize;
        let mut j = i;
        let mut int: u64 = 0;
        loop {
            let d = (*p.add(j)).wrapping_sub(b'0');
            if d >= 10 {
                break;
            }
            int = int.wrapping_mul(10).wrapping_add(d as u64);
            j += 1;
        }
        let n1 = j - i;
        // No digit, too long, or a leading zero ("01"): the general parser
        // handles / reports it. From here on j <= start + 16.
        if n1 == 0 || n1 > 15 || (n1 > 1 && *p.add(i) == b'0') {
            return None;
        }
        let mut c = *p.add(j);
        if c != b'.' && (c | 0x20) != b'e' {
            self.pos = j;
            // At most 15 digits: fits an i64.
            let v = int as i64;
            let r = ffi::PyLong_FromLongLong(if neg { -v } else { v });
            return Some(if r.is_null() { self.err_oom() } else { Ok(r) });
        }
        let mut mant = int;
        let mut n2 = 0;
        if c == b'.' {
            let (frac, n) = digits16(p.add(j + 1))?;
            if n == 0 || n1 + n > 19 {
                return None;
            }
            // <= 19 digits: exact in a u64.
            mant = int * POW10_U64[n] + frac;
            n2 = n;
            j += 1 + n; // <= start + 32
            c = *p.add(j);
        }
        let v = if (c | 0x20) != b'e' {
            if mant <= (1u64 << 53) {
                // Clinger: exact mantissa and power of ten (n2 <= 15 <= 22),
                // so one correctly rounded division.
                mant as f64 / POW10[n2]
            } else {
                // n2 in 1..=15 and mant > 2^53: within the specialised range.
                crate::lemire::compute_float64_small(-(n2 as i64), mant)
            }
        } else {
            // Exponent: optional sign and 1..=4 digits (reads <= start + 38).
            j += 1;
            let s = *p.add(j);
            let eneg = s == b'-';
            j += (s == b'-' || s == b'+') as usize;
            let es = j;
            let mut ev: i64 = 0;
            while j - es < 4 {
                let d = (*p.add(j)).wrapping_sub(b'0');
                if d >= 10 {
                    break;
                }
                ev = ev * 10 + d as i64;
                j += 1;
            }
            if j == es || (*p.add(j)).wrapping_sub(b'0') < 10 {
                return None;
            }
            let e = if eneg { -ev } else { ev } - n2 as i64;
            if mant == 0 {
                0.0
            } else if mant <= (1u64 << 53) && (-22..=22).contains(&e) {
                let m = mant as f64;
                if e >= 0 {
                    m * POW10[e as usize]
                } else {
                    m / POW10[(-e) as usize]
                }
            } else {
                // Undecidable rounding or overflow: the general parser.
                match crate::lemire::compute_float64(e, mant) {
                    Some(v) if v.is_finite() => v,
                    _ => return None,
                }
            }
        };
        self.pos = j;
        let r = new_float(if neg { -v } else { v });
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

/// Bytes `parse_number_fast` may read from the start of the number: sign,
/// up to 16 integer-digit bytes, '.', 16 fraction-digit bytes, and an
/// exponent ('e', sign, 4 digits and the byte after).
const NUM_FAST_LOOKAHEAD: usize = 40;

/// Number of leading ASCII digits in the 8 bytes of `w` (little endian).
#[inline(always)]
fn digit_run(w: u64) -> usize {
    let non_digit =
        (w.wrapping_sub(0x3030_3030_3030_3030) | w.wrapping_add(0x4646_4646_4646_4646)) & 0x8080_8080_8080_8080;
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
    Some((parse_8digits(w1) * POW10_U64[nb] + parse_digits_prefix(w2, nb), 8 + nb))
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
        (0x8080E0 | (cp >> 12) | (((cp >> 6) & 0x3F) << 8) | ((cp & 0x3F) << 16), 3, 6)
    } else {
        if cp >= 0xDC00 || *p.add(6) != b'\\' || *p.add(7) != b'u' {
            return None;
        }
        let lo = hex4(p.add(8))?;
        if !(0xDC00..0xE000).contains(&lo) {
            return None;
        }
        let c = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
        (0x808080F0 | (c >> 18) | (((c >> 12) & 0x3F) << 8) | (((c >> 6) & 0x3F) << 16) | ((c & 0x3F) << 24), 4, 12)
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
        (_mm256_movemask_epi8(m) as u32, _mm256_movemask_epi8(v) as u32)
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
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18, 1e19, 1e20,
    1e21, 1e22,
];

// ============================================================================
// Float construction
// ============================================================================

/// On CPython 3.13+, `PyFloat_FromDouble` without the free-list probe: while
/// building a document the float free list (<= 100 entries) is empty after
/// the first few floats, so every call pays for the probe and then
/// allocates anyway. This does exactly the miss path (`PyObject_Malloc` +
/// `PyObject_Init` + `ob_fval`); float_dealloc frees or recycles the object
/// the same way. `PyFloatObject` is public CPython (non-limited) API.
/// Measured: 3.13 -3..6% on float-heavy documents, 3.12 neutral, 3.11 +9%
/// (its PyFloat_FromDouble is cheap and the extra call costs more), so
/// older versions keep `PyFloat_FromDouble`.
#[cfg(Py_3_13)]
#[inline(always)]
unsafe fn new_float(v: f64) -> *mut ffi::PyObject {
    let op = ffi::PyObject_Malloc(std::mem::size_of::<ffi::PyFloatObject>()) as *mut ffi::PyObject;
    if op.is_null() {
        return op;
    }
    ffi::PyObject_Init(op, ptr::addr_of_mut!(ffi::PyFloat_Type));
    (*(op as *mut ffi::PyFloatObject)).ob_fval = v;
    op
}

#[cfg(not(Py_3_13))]
#[inline(always)]
unsafe fn new_float(v: f64) -> *mut ffi::PyObject {
    ffi::PyFloat_FromDouble(v)
}

// ============================================================================
// List construction
// ============================================================================

/// New list holding the `n` references at `src`, which it takes over on
/// success (on failure they are untouched and NULL is returned).
///
/// `PyList_New(n)` allocates the item array with `PyMem_Calloc` and zeroes
/// it only for us to overwrite every slot; instead, attach a `PyMem_Malloc`
/// array to an empty list, exactly as `PyList_New(n)` would leave it
/// (`ob_item` from the PyMem allocator, `allocated == size == n`), which is
/// what `list_dealloc`/`list_resize` expect. `PyListObject` is part of the
/// public CPython (non-limited) API; free-threaded builds use a different
/// item storage, so they take the plain path.
#[inline(always)]
unsafe fn list_from_items(src: *const *mut ffi::PyObject, n: usize) -> *mut ffi::PyObject {
    #[cfg(not(Py_GIL_DISABLED))]
    {
        if n == 0 {
            return ffi::PyList_New(0);
        }
        let items = ffi::PyMem_Malloc(n * std::mem::size_of::<*mut ffi::PyObject>()) as *mut *mut ffi::PyObject;
        if items.is_null() {
            return ptr::null_mut();
        }
        let list = ffi::PyList_New(0);
        if list.is_null() {
            ffi::PyMem_Free(items as *mut std::ffi::c_void);
            return list;
        }
        ptr::copy_nonoverlapping(src, items, n);
        let l = list as *mut ffi::PyListObject;
        (*l).ob_item = items;
        (*l).allocated = n as ffi::Py_ssize_t;
        (*(list as *mut ffi::PyVarObject)).ob_size = n as ffi::Py_ssize_t; // Py_SET_SIZE
        list
    }
    #[cfg(Py_GIL_DISABLED)]
    {
        let list = ffi::PyList_New(n as ffi::Py_ssize_t);
        if !list.is_null() {
            for i in 0..n {
                ffi::PyList_SET_ITEM(list, i as ffi::Py_ssize_t, *src.add(i));
            }
        }
        list
    }
}

// ============================================================================
// Dict construction
// ============================================================================

#[cfg(all(Py_3_13, not(Py_3_14)))]
extern "C" {
    /// CPython 3.13 (exported, private; the BUILD_MAP implementation):
    /// builds a dict from `length` keys/values read at the given strides,
    /// inserting in order (the last duplicate wins) without stealing
    /// references. It presizes the table and, when every key is an exact
    /// str, uses the compact str-only key table.
    fn _PyDict_FromItems(
        keys: *const *mut ffi::PyObject,
        keys_offset: ffi::Py_ssize_t,
        values: *const *mut ffi::PyObject,
        values_offset: ffi::Py_ssize_t,
        length: ffi::Py_ssize_t,
    ) -> *mut ffi::PyObject;
}

/// Build a dict from `n` interleaved key/value pairs at `kv` (borrowed; the
/// caller releases them). Returns NULL with an exception set on failure.
#[cfg(all(Py_3_13, not(Py_3_14)))]
#[inline(always)]
unsafe fn build_dict(kv: *const *mut ffi::PyObject, n: usize) -> *mut ffi::PyObject {
    _PyDict_FromItems(kv, 2, kv.add(1), 2, n as ffi::Py_ssize_t)
}

#[cfg(not(all(Py_3_13, not(Py_3_14))))]
#[inline(always)]
unsafe fn build_dict(kv: *const *mut ffi::PyObject, n: usize) -> *mut ffi::PyObject {
    // `_PyDict_NewPresized` (3.11-3.13) always allocates the generic key
    // table (24-byte entries with a stored hash) instead of the str-only
    // table (16-byte entries) that `PyDict_New` + str-key inserts produce.
    // Presizing small dicts made 8-key records ~80 bytes larger each (and
    // later lookups take the generic path), so only presize large dicts
    // where avoiding repeated resizes pays (same threshold as orjson).
    let dict = if n > 8 { crate::compat::_PyDict_NewPresized(n as ffi::Py_ssize_t) } else { ffi::PyDict_New() };
    if dict.is_null() {
        return dict;
    }
    // Insert in document order so the last duplicate key wins.
    for i in 0..n {
        if ffi::PyDict_SetItem(dict, *kv.add(2 * i), *kv.add(2 * i + 1)) != 0 {
            ffi::Py_DECREF(dict);
            return ptr::null_mut();
        }
    }
    dict
}

// ============================================================================
// String construction
// ============================================================================

/// Create a compact ASCII str. `bytes` must be pure ASCII.
#[inline(always)]
unsafe fn new_ascii_str(bytes: &[u8]) -> *mut ffi::PyObject {
    let s = ffi::PyUnicode_New(bytes.len() as ffi::Py_ssize_t, 127);
    if !s.is_null() {
        ptr::copy_nonoverlapping(bytes.as_ptr(), crate::compat::PyUnicode_DATA(s) as *mut u8, bytes.len());
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
    let data = crate::compat::PyUnicode_DATA(s);
    match crate::compat::PyUnicode_KIND(s) {
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
/// `ptr[len]` is always a readable NUL byte (required by `parse`): `str`
/// (UTF-8 cache), `bytes` and `bytearray` buffers guarantee one; other
/// buffers are copied into `owned` with one appended.
pub(crate) struct Input {
    pub(crate) ptr: *const u8,
    pub(crate) len: usize,
    pub(crate) utf8_valid: bool,
    #[allow(dead_code)] // only keeps the copy alive
    owned: Option<Vec<u8>>,
}

pub(crate) unsafe fn get_input(py: Python<'_>, obj: *mut ffi::PyObject) -> PyResult<Input> {
    if ffi::PyUnicode_Check(obj) != 0 {
        let mut size: ffi::Py_ssize_t = 0;
        let p = ffi::PyUnicode_AsUTF8AndSize(obj, &mut size);
        if p.is_null() {
            ffi::PyErr_Clear();
            return Err(raise_decode_error(py, "str is not valid UTF-8: surrogates not allowed", b"", 0));
        }
        return Ok(Input { ptr: p as *const u8, len: size as usize, utf8_valid: true, owned: None });
    }
    if ffi::PyBytes_Check(obj) != 0 {
        return Ok(Input {
            ptr: ffi::PyBytes_AsString(obj) as *const u8,
            len: ffi::PyBytes_Size(obj) as usize,
            utf8_valid: false,
            owned: None,
        });
    }
    if ffi::PyByteArray_Check(obj) != 0 {
        return Ok(Input {
            ptr: ffi::PyByteArray_AsString(obj) as *const u8,
            len: ffi::PyByteArray_Size(obj) as usize,
            utf8_valid: false,
            owned: None,
        });
    }
    if ffi::PyMemoryView_Check(obj) != 0 {
        return memoryview_input(py, obj);
    }
    Err(input_type_error(py, obj))
}

/// Copies a memoryview's bytes (in C order, like `mv.tobytes()`) and appends
/// the NUL terminator `parse` needs. Accepts any layout: contiguous views are
/// copied directly, strided (`mv[::2]`), Fortran-ordered or indirect
/// (suboffsets) views are gathered with `PyBuffer_ToContiguous`. Requesting
/// `PyBUF_C_CONTIGUOUS` instead raised `BufferError` for non-contiguous views.
#[inline(never)]
unsafe fn memoryview_input(py: Python<'_>, obj: *mut ffi::PyObject) -> PyResult<Input> {
    let mut view: ffi::Py_buffer = std::mem::zeroed();
    // FULL_RO = strides + suboffsets + format, read-only: the most general
    // request, so every memoryview can satisfy it (a released one raises).
    if ffi::PyObject_GetBuffer(obj, &mut view, ffi::PyBUF_FULL_RO) != 0 {
        return Err(PyErr::fetch(py));
    }
    // `view.len` is the total byte size (product(shape) * itemsize) for any layout.
    let len = view.len as usize;
    let mut owned: Vec<u8> = Vec::with_capacity(len + 1);
    if len > 0 {
        if ffi::PyBuffer_IsContiguous(&view, b'C' as std::os::raw::c_char) != 0 {
            owned.extend_from_slice(std::slice::from_raw_parts(view.buf as *const u8, len));
        } else {
            // SAFETY: `owned` has capacity for `len` bytes; on success
            // PyBuffer_ToContiguous has written exactly `view.len` bytes.
            // A raw `*mut` (which also coerces to `*const`): pyo3-ffi declares
            // `src` as `*mut` before 3.11 and `*const` from 3.11 on; CPython
            // only reads it.
            let len_ssize = view.len;
            let rc = ffi::PyBuffer_ToContiguous(
                owned.as_mut_ptr() as *mut std::os::raw::c_void,
                ptr::addr_of_mut!(view),
                len_ssize,
                b'C' as std::os::raw::c_char,
            );
            if rc != 0 {
                ffi::PyBuffer_Release(&mut view);
                return Err(PyErr::fetch(py));
            }
            owned.set_len(len);
        }
    }
    owned.push(0);
    ffi::PyBuffer_Release(&mut view);
    Ok(Input { ptr: owned.as_ptr(), len, utf8_valid: false, owned: Some(owned) })
}

#[cold]
#[inline(never)]
fn input_type_error(py: Python<'_>, obj: *mut ffi::PyObject) -> PyErr {
    use pyo3::types::PyTypeMethods;
    // SAFETY: `obj` is the (borrowed, live) argument of `loads`.
    let ty = unsafe { Bound::from_borrowed_ptr(py, obj) }.get_type();
    let name = ty
        .fully_qualified_name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "unknown".to_string());
    PyTypeError::new_err(format!(
        "loads() argument must be str, bytes, bytearray or memoryview, not {name}"
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
///
/// # Safety
///
/// Unless `buf` is empty, the byte just past its end (`buf.as_ptr() +
/// buf.len()`) must be readable and 0 (every `Input` from `get_input`
/// guarantees this). The parser peeks at `pos <= len` without bounds checks.
pub(crate) unsafe fn parse(py: Python<'_>, buf: &[u8], utf8_valid: bool) -> PyResult<*mut ffi::PyObject> {
    if buf.is_empty() {
        return Err(raise_decode_error(py, "input data is empty", buf, 0));
    }
    debug_assert_eq!(*buf.as_ptr().add(buf.len()), 0);
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
