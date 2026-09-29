//! Python-facing entry points: raw CPython builtins.
//!
//! `loads` is a plain `METH_O` builtin; `dumps`/`dumps_str` are
//! `METH_FASTCALL | METH_KEYWORDS` with hand-parsed `kwnames` (for the
//! keyword-only `default=`, `passthrough=` and `non_str_keys=`): the common one-positional-argument call is one
//! compare, everything else goes to a cold path. Neither uses PyO3
//! `#[pyfunction]`s. A `#[pyfunction]` is `METH_FASTCALL|METH_KEYWORDS`
//! and runs PyO3's generic argument extraction (`FunctionDescription`,
//! kwnames handling, output array) on every call. Measured per-call cost on
//! CPython 3.11 (min of 15x200k calls, ns/call, incl. ~18 ns loop overhead):
//!
//! | entry kind                                   | identity fn |
//! |----------------------------------------------|-------------|
//! | `#[pyfunction]` (PyO3 0.24)                  | 25.5        |
//! | `#[pyfunction]` (PyO3 0.29)                  | 32.7        |
//! | raw `METH_O` + PyO3 trampoline (this file)   | 19.6        |
//! | raw `METH_O`, no trampoline                  | 17.6        |
//!
//! We keep PyO3's trampoline (`impl_::trampoline`, reached through the
//! `get_trampoline_function!` macro since 0.29; its shape changes between
//! PyO3 releases, so re-check it on upgrade): for ~2 ns it maintains PyO3's
//! GIL_COUNT, so dropping a `Py<T>` anywhere below decrefs immediately
//! instead of being deferred to PyO3's global reference pool, and it traps
//! panics at the FFI boundary. It is a `#[doc(hidden)]` API: PyO3 is pinned
//! and the call sites are confined to this file.
//!
//! New keyword options (`option=`, ...) go into the hand-parsed `kwnames` of
//! `dumps_args_slow`, not PyO3's `FunctionDescription`.
//!
//! The work itself lives in `crate::parser::parse` / `crate::ser::dumps_raw`; this
//! file only converts arguments and results.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyCFunction;
use std::ptr;

use crate::ser::{DumpsOpts, Layout};

unsafe fn loads_body(
    py: Python<'_>,
    _m: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    // PY_VECTORCALL_ARGUMENTS_OFFSET may be set in nargs.
    let n = ffi::PyVectorcall_NARGS(nargs as usize);
    // One call site for `loads_impl`: with two, LLVM stopped inlining
    // `get_input` and `parse` into it (+~60 instructions per call).
    let lenient = if n == 1 && kwnames.is_null() {
        false
    } else {
        loads_args_slow(py, n, args, kwnames, "loads")?
    };
    loads_impl(py, *args, lenient)
}

#[inline(always)]
unsafe fn loads_impl(
    py: Python<'_>,
    arg: *mut ffi::PyObject,
    lenient: bool,
) -> PyResult<*mut ffi::PyObject> {
    // str / bytes / bytearray / memoryview; see `parser::get_input`.
    let input = match crate::parser::get_input(py, arg) {
        Ok(input) => input,
        // e.g. a str with lone surrogates, which json.loads accepts.
        Err(e) if lenient && is_decode_error(py, &e) => return loads_fallback(py, arg, e),
        Err(e) => return Err(e),
    };
    let mut buf: &[u8] = if input.len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(input.ptr, input.len)
    };
    if lenient && !input.utf8_valid && buf.starts_with(b"\xEF\xBB\xBF") {
        // json.loads decodes bytes with a UTF-8 BOM as utf-8-sig. (A str
        // starting with U+FEFF stays an error there, and here.) Skipping at
        // the front keeps the NUL the parser needs after the end.
        buf = &buf[3..];
    }
    match crate::parser::parse(py, buf, input.utf8_valid, lenient) {
        Err(e) if lenient && fallback_is_safe(py, &e) => {
            drop(input); // release a held buffer export before running Python code
            loads_fallback(py, arg, e)
        }
        r => r,
    }
}

/// Whether the lenient `json.loads` fallback may run for `err`. Never for a
/// stack RecursionError (json would overflow the same stack). For nesting
/// beyond rjson's 1024 levels, json's C scanner on 3.12/3.13 recurses to
/// ~10,000 levels, needing up to ~2 MiB of stack, and segfaults on smaller
/// thread stacks instead of raising: fall back only with 4 MiB left. No
/// limit on 3.14 (json checks the real stack itself) or on Windows (CPython
/// checks the stack there too, USE_STACKCHECK, and its own tests parse
/// 100,000-deep JSON on the 2 MB main thread); 3.10/3.11's json stops at
/// ~1,000 levels anyway.
#[cold]
#[inline(never)]
unsafe fn fallback_is_safe(py: Python<'_>, err: &PyErr) -> bool {
    if err.is_instance_of::<pyo3::exceptions::PyRecursionError>(py) {
        return false;
    }
    if cfg!(Py_3_14) || cfg!(windows) || !is_depth_error(py, err) {
        return true;
    }
    crate::stack::remaining().is_none_or(|left| left >= 4 << 20)
}

#[cold]
#[inline(never)]
fn is_depth_error(py: Python<'_>, err: &PyErr) -> bool {
    err.value(py)
        .getattr("msg")
        .and_then(|m| m.extract::<String>())
        .is_ok_and(|m| m == crate::parser::DEPTH_MSG)
}

#[cold]
#[inline(never)]
unsafe fn is_decode_error(py: Python<'_>, e: &PyErr) -> bool {
    match crate::parser::decode_error_type(py) {
        Ok(t) => e.get_type(py).is(t.bind(py)),
        Err(_) => false,
    }
}

/// Lenient mode, when the native parser rejected the input: what `json.loads`
/// accepts beyond what rjson parses natively (lone surrogates, UTF-16/UTF-32
/// bytes, nesting deeper than 1024) is parsed by `json.loads` itself, so a
/// lenient result always equals `json.loads`'s. If `json.loads` rejects the
/// input too (ValueError, e.g. JSONDecodeError or UnicodeDecodeError, or
/// RecursionError), rjson's own error `err` is raised, unless json's
/// JSONDecodeError is at a later position: then rjson stopped at something
/// lenient mode accepts (a lone surrogate, UTF-16 text) and json's error
/// names the real problem.
#[cold]
#[inline(never)]
unsafe fn loads_fallback(
    py: Python<'_>,
    arg: *mut ffi::PyObject,
    err: PyErr,
) -> PyResult<*mut ffi::PyObject> {
    use pyo3::exceptions::{PyRecursionError, PyValueError};
    let json_loads = match py.import("json").and_then(|m| m.getattr("loads")) {
        Ok(f) => f,
        Err(_) => return Err(err),
    };
    // json.loads takes str, bytes and bytearray; a memoryview is copied.
    let data = if ffi::PyMemoryView_Check(arg) != 0 {
        match Bound::from_owned_ptr_or_err(py, ffi::PyBytes_FromObject(arg)) {
            Ok(b) => b,
            Err(_) => return Err(err),
        }
    } else {
        Bound::from_borrowed_ptr(py, arg)
    };
    match json_loads.call1((data,)) {
        Ok(v) => Ok(v.into_ptr()),
        Err(e2) if is_decode_error(py, &e2) && error_pos(py, &e2) > error_pos(py, &err) => Err(e2),
        Err(e2)
            if e2.is_instance_of::<PyValueError>(py)
                || e2.is_instance_of::<PyRecursionError>(py) =>
        {
            Err(err)
        }
        Err(e2) => Err(e2),
    }
}

/// `pos` of a JSONDecodeError (-1 if unavailable).
fn error_pos(py: Python<'_>, e: &PyErr) -> i64 {
    e.value(py)
        .getattr("pos")
        .and_then(|p| p.extract::<i64>())
        .unwrap_or(-1)
}

/// `loads(data, /, *, lenient=False)` (and `loads_ndjson`, same signature)
/// arguments other than the plain one-positional call.
#[cold]
#[inline(never)]
unsafe fn loads_args_slow(
    py: Python<'_>,
    n: ffi::Py_ssize_t,
    args: *const *mut ffi::PyObject,
    kwnames: *mut ffi::PyObject,
    name: &str,
) -> PyResult<bool> {
    use pyo3::exceptions::PyTypeError;
    if n != 1 {
        return Err(PyTypeError::new_err(format!(
            "rjson.{name}() takes exactly one positional argument ({n} given)"
        )));
    }
    let mut lenient = false;
    let nkw = if kwnames.is_null() {
        0
    } else {
        ffi::PyTuple_GET_SIZE(kwnames)
    };
    for i in 0..nkw {
        let kw = ffi::PyTuple_GET_ITEM(kwnames, i);
        let value = *args.offset(n + i);
        if ffi::PyUnicode_CompareWithASCIIString(kw, c"lenient".as_ptr()) == 0 {
            match ffi::PyObject_IsTrue(value) {
                -1 => return Err(PyErr::fetch(py)),
                v => lenient = v == 1,
            }
        } else {
            let kw_str = pyo3::Bound::from_borrowed_ptr(py, kw).to_string();
            return Err(PyTypeError::new_err(format!(
                "rjson.{name}() got an unexpected keyword argument '{kw_str}'"
            )));
        }
    }
    Ok(lenient)
}

// ---------------------------------------------------------------------------
// loads_ndjson
// ---------------------------------------------------------------------------

unsafe fn loads_ndjson_body(
    py: Python<'_>,
    module: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    let n = ffi::PyVectorcall_NARGS(nargs as usize);
    let lenient = if n == 1 && kwnames.is_null() {
        false
    } else {
        loads_args_slow(py, n, args, kwnames, "loads_ndjson")?
    };
    ndjson_impl(py, module, *args, lenient)
}

/// `loads_ndjson`: the list of the documents of newline-delimited JSON, one
/// per line; blank lines are skipped. Each line gives exactly what
/// `loads(line, lenient=...)` gives, and errors carry their position in the
/// whole input (so `lineno` is the input's line).
///
/// All lines are parsed by one `parser::Lines` (no per-line call, slicing or
/// input setup). A line its fast path does not take goes through
/// `ndjson_line`, which calls `loads` on that line.
#[inline(never)]
unsafe fn ndjson_impl(
    py: Python<'_>,
    module: *mut ffi::PyObject,
    arg: *mut ffi::PyObject,
    lenient: bool,
) -> PyResult<*mut ffi::PyObject> {
    let input = match crate::parser::get_input(py, arg) {
        Ok(input) => input,
        // A str with lone surrogates (only lenient mode accepts them).
        Err(e) if lenient && ffi::PyUnicode_Check(arg) != 0 && is_decode_error(py, &e) => {
            return ndjson_split(py, module, arg);
        }
        Err(e) => return Err(e),
    };
    // `ndjson_line` may run Python code (`json.loads` in lenient mode, a
    // finalizer); hold an export of a bytearray so nothing can resize it
    // under the parser. (bytes and str are immutable; a memoryview input is
    // held or copied by `get_input`.)
    let _export = BufferExport::new(py, arg)?;
    let buf: &[u8] = if input.len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(input.ptr, input.len)
    };
    // json.loads decodes bytes with a UTF-8 BOM as utf-8-sig (as `loads`).
    let mut from = if lenient && !input.utf8_valid && buf.starts_with(b"\xEF\xBB\xBF") { 3 } else { 0 };
    let is_str = input.utf8_valid;
    let mut lines = crate::parser::Lines::new(buf, input.utf8_valid, lenient);
    while let Some((start, end)) = lines.run(from) {
        lines.push(ndjson_line(py, module, buf, is_str, start, end, lenient)?);
        from = end + 1;
    }
    let list = lines.finish();
    if list.is_null() {
        return Err(PyErr::fetch(py));
    }
    Ok(list)
}

/// One line that `Lines::run` did not take (`buf[start..end]`, without its
/// `\n`): parsed by `rjson.loads` on its own, so values and errors are
/// exactly `loads`'s. A JSONDecodeError is raised again with its position in
/// the whole input.
#[cold]
#[inline(never)]
unsafe fn ndjson_line(
    py: Python<'_>,
    module: *mut ffi::PyObject,
    buf: &[u8],
    is_str: bool,
    start: usize,
    end: usize,
    lenient: bool,
) -> PyResult<*mut ffi::PyObject> {
    let line = &buf[start..end];
    let obj = if is_str {
        // Valid UTF-8 (the str's own encoding), cut at ASCII bytes.
        ffi::PyUnicode_DecodeUTF8(line.as_ptr() as *const _, line.len() as ffi::Py_ssize_t, c"strict".as_ptr())
    } else {
        ffi::PyBytes_FromStringAndSize(line.as_ptr() as *const _, line.len() as ffi::Py_ssize_t)
    };
    let obj = Bound::from_owned_ptr_or_err(py, obj)?;
    match call_loads(py, module, &obj, lenient) {
        Ok(v) => Ok(v.into_ptr()),
        Err(e) if is_decode_error(py, &e) => {
            let msg: String = e.value(py).getattr("msg")?.extract()?;
            // `pos` counts characters of the line: the byte offset where that
            // character starts (characters start at non-continuation bytes).
            let chars = error_pos(py, &e).max(0) as usize;
            let off = line
                .iter()
                .enumerate()
                .filter(|&(_, &b)| (b & 0xC0) != 0x80)
                .nth(chars)
                .map_or(line.len(), |(i, _)| i);
            Err(crate::parser::decode_error_at(py, &msg, buf, start + off))
        }
        Err(e) => Err(e),
    }
}

/// `rjson.loads(obj)` / `rjson.loads(obj, lenient=True)` through the module
/// attribute: keeps `loads_impl` at one call site (see `loads_body`).
#[cold]
unsafe fn call_loads<'py>(
    py: Python<'py>,
    module: *mut ffi::PyObject,
    obj: &Bound<'py, PyAny>,
    lenient: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let loads = Bound::from_borrowed_ptr(py, module).getattr("loads")?;
    if lenient {
        let kw = pyo3::types::PyDict::new(py);
        kw.set_item("lenient", true)?;
        loads.call((obj,), Some(&kw))
    } else {
        loads.call1((obj,))
    }
}

/// Lenient mode, a str that is not valid UTF-8 (lone surrogates): split it
/// into lines in Python and parse each with `loads(line, lenient=True)`.
#[cold]
#[inline(never)]
unsafe fn ndjson_split(
    py: Python<'_>,
    module: *mut ffi::PyObject,
    arg: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    let text = Bound::from_borrowed_ptr(py, arg);
    let out = pyo3::types::PyList::empty(py);
    let mut offset: usize = 0; // characters before the current line
    for line in text.call_method1("split", ("\n",))?.try_iter()? {
        let line = line?;
        let n: usize = line.len()?;
        let blank = line.call_method1("strip", (" \t\r",))?.len()? == 0;
        if !blank {
            match call_loads(py, module, &line, true) {
                Ok(v) => out.append(v)?,
                Err(e) if is_decode_error(py, &e) => {
                    let msg: String = e.value(py).getattr("msg")?.extract()?;
                    let pos = offset + error_pos(py, &e).max(0) as usize;
                    let ty = crate::parser::decode_error_type(py)?.bind(py);
                    return Err(PyErr::from_type(ty.clone(), (msg, text.clone().unbind(), pos)));
                }
                Err(e) => return Err(e),
            }
        }
        offset += n + 1;
    }
    Ok(out.into_ptr())
}

/// A buffer export of a `bytearray` argument for the duration of a call
/// (none for other types), so it cannot be resized meanwhile.
struct BufferExport(Option<Box<ffi::Py_buffer>>);

impl BufferExport {
    unsafe fn new(py: Python<'_>, obj: *mut ffi::PyObject) -> PyResult<Self> {
        if ffi::PyByteArray_Check(obj) == 0 {
            return Ok(BufferExport(None));
        }
        let mut view: Box<ffi::Py_buffer> = Box::new(std::mem::zeroed());
        if ffi::PyObject_GetBuffer(obj, &mut *view, ffi::PyBUF_SIMPLE) != 0 {
            return Err(PyErr::fetch(py));
        }
        Ok(BufferExport(Some(view)))
    }
}

impl Drop for BufferExport {
    fn drop(&mut self) {
        if let Some(view) = self.0.as_mut() {
            // SAFETY: obtained by PyObject_GetBuffer, released once, GIL held.
            unsafe { ffi::PyBuffer_Release(&mut **view) };
        }
    }
}

unsafe fn dumps_body(
    py: Python<'_>,
    _m: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    dumps_call(py, "dumps", false, args, nargs, kwnames)
}

unsafe fn dumps_str_body(
    py: Python<'_>,
    _m: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    dumps_call(py, "dumps_str", true, args, nargs, kwnames)
}

/// `dumps`/`dumps_str` for a vectorcall. The common call, one positional
/// argument and no keywords, is a single compare and needs no options value
/// (which owns a `Layout` when given, so building and dropping one on the hot
/// path cost ~5 ns); everything else goes through cold `dumps_with_kwargs`.
#[inline(always)]
unsafe fn dumps_call(
    py: Python<'_>,
    name: &str,
    as_str: bool,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    // PY_VECTORCALL_ARGUMENTS_OFFSET may be set in nargs.
    let n = ffi::PyVectorcall_NARGS(nargs as usize);
    if n == 1 && kwnames.is_null() {
        return crate::ser::dumps_raw(py, *args, as_str, &DumpsOpts::NONE);
    }
    dumps_with_kwargs(py, name, as_str, args, n, kwnames)
}

#[cold]
#[inline(never)]
unsafe fn dumps_with_kwargs(
    py: Python<'_>,
    name: &str,
    as_str: bool,
    args: *const *mut ffi::PyObject,
    n: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    let (obj, opts) = dumps_args_slow(py, name, args, n, kwnames)?;
    crate::ser::dumps_raw(py, obj, as_str, &opts)
}

/// Parses `(obj, /, *, default=None, passthrough=0, non_str_keys=False,
/// indent=None, separators=None, sort_keys=False, ensure_ascii=False,
/// allow_nan=False)` from a vectorcall: returns `obj` and the options. `obj`
/// and the `default` callable (null when absent or None) are borrowed from
/// the call's arguments, which the caller keeps alive for the whole call.
/// Errors follow CPython's argument-clinic wording.
#[cold]
#[inline(never)]
unsafe fn dumps_args_slow(
    py: Python<'_>,
    name: &str,
    args: *const *mut ffi::PyObject,
    n: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<(*mut ffi::PyObject, DumpsOpts)> {
    use pyo3::exceptions::PyTypeError;
    if n != 1 {
        return Err(PyTypeError::new_err(format!(
            "rjson.{name}() takes exactly one positional argument ({n} given)"
        )));
    }
    let mut default: *mut ffi::PyObject = ptr::null_mut();
    let mut passthrough: u32 = 0;
    let mut non_str_keys = false;
    let mut indent: Option<Vec<u8>> = None;
    let mut separators: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut sort_keys = false;
    let mut ensure_ascii = false;
    let mut allow_nan = false;
    let nkw = if kwnames.is_null() {
        0
    } else {
        ffi::PyTuple_GET_SIZE(kwnames)
    };
    for i in 0..nkw {
        let kw = ffi::PyTuple_GET_ITEM(kwnames, i);
        // Keyword values follow the positional arguments in `args`.
        let value = *args.offset(n + i);
        if ffi::PyUnicode_CompareWithASCIIString(kw, c"default".as_ptr()) == 0 {
            default = value;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"passthrough".as_ptr()) == 0 {
            passthrough = passthrough_flags(py, name, value)?;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"non_str_keys".as_ptr()) == 0 {
            // Truthiness, like json.dumps's flags (runs before serializing).
            match ffi::PyObject_IsTrue(value) {
                -1 => return Err(PyErr::fetch(py)),
                v => non_str_keys = v == 1,
            }
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"indent".as_ptr()) == 0 {
            indent = indent_unit(py, name, value)?;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"separators".as_ptr()) == 0 {
            separators = separator_pair(py, name, value)?;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"sort_keys".as_ptr()) == 0 {
            sort_keys = truthy(py, value)?;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"ensure_ascii".as_ptr()) == 0 {
            ensure_ascii = truthy(py, value)?;
        } else if ffi::PyUnicode_CompareWithASCIIString(kw, c"allow_nan".as_ptr()) == 0 {
            allow_nan = truthy(py, value)?;
        } else {
            let kw_str = pyo3::Bound::from_borrowed_ptr(py, kw).to_string();
            return Err(PyTypeError::new_err(format!(
                "rjson.{name}() got an unexpected keyword argument '{kw_str}'"
            )));
        }
    }
    if default == ffi::Py_None() {
        default = ptr::null_mut();
    }
    if !default.is_null() && ffi::PyCallable_Check(default) == 0 {
        let ty = pyo3::Bound::from_borrowed_ptr(py, default).get_type();
        let tn = ty.name().map(|n| n.to_string()).unwrap_or_default();
        return Err(PyTypeError::new_err(format!(
            "rjson.{name}() default must be callable, not {tn}"
        )));
    }
    // json.dumps's defaults: `(",", ": ")` with an indent; rjson stays
    // compact without one.
    let layout = match (indent, separators) {
        (None, None) => None,
        (None, Some((item, key))) if item == b"," && key == b":" => None,
        (indent, seps) => {
            let (item_sep, key_sep) = seps.unwrap_or_else(|| (b",".to_vec(), b": ".to_vec()));
            Some(Box::new(Layout { indent, item_sep, key_sep }))
        }
    };
    Ok((
        *args,
        DumpsOpts {
            default,
            passthrough,
            non_str_keys,
            layout,
            sort_keys,
            ensure_ascii,
            allow_nan,
        },
    ))
}

/// Truthiness, like json.dumps's flags (runs before serializing).
unsafe fn truthy(py: Python<'_>, value: *mut ffi::PyObject) -> PyResult<bool> {
    match ffi::PyObject_IsTrue(value) {
        -1 => Err(PyErr::fetch(py)),
        v => Ok(v == 1),
    }
}

/// UTF-8 bytes of the `str` `value` (the error names `what`).
unsafe fn str_utf8(py: Python<'_>, name: &str, what: &str, value: *mut ffi::PyObject) -> PyResult<Vec<u8>> {
    if ffi::PyUnicode_Check(value) == 0 {
        let ty = pyo3::Bound::from_borrowed_ptr(py, value).get_type();
        let tn = ty.name().map(|n| n.to_string()).unwrap_or_default();
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "rjson.{name}() {what} must be str, not {tn}"
        )));
    }
    let mut n: ffi::Py_ssize_t = 0;
    let p = ffi::PyUnicode_AsUTF8AndSize(value, &mut n);
    if p.is_null() {
        return Err(PyErr::fetch(py));
    }
    Ok(std::slice::from_raw_parts(p as *const u8, n as usize).to_vec())
}

/// `separators=`: None or an `(item_separator, key_separator)` pair of
/// str, as json.dumps.
unsafe fn separator_pair(
    py: Python<'_>,
    name: &str,
    value: *mut ffi::PyObject,
) -> PyResult<Option<(Vec<u8>, Vec<u8>)>> {
    if value == ffi::Py_None() {
        return Ok(None);
    }
    let pair = ffi::PySequence_Tuple(value);
    if pair.is_null() || ffi::PyTuple_GET_SIZE(pair) != 2 {
        if pair.is_null() {
            ffi::PyErr_Clear();
        } else {
            ffi::Py_DECREF(pair);
        }
        return Err(pyo3::exceptions::PyTypeError::new_err(format!(
            "rjson.{name}() separators must be None or an (item_separator, key_separator) pair of str"
        )));
    }
    let seps = str_utf8(py, name, "item_separator", ffi::PyTuple_GET_ITEM(pair, 0)).and_then(|item| {
        str_utf8(py, name, "key_separator", ffi::PyTuple_GET_ITEM(pair, 1)).map(|key| (item, key))
    });
    ffi::Py_DECREF(pair);
    seps.map(Some)
}

/// `indent=`: None (one line), an int from 0 to 1024 (spaces per level, as
/// `json.dumps`; 2 gives orjson's `OPT_INDENT_2` layout) or a str (the unit
/// per level, e.g. `"\t"`, as `json.dumps`).
unsafe fn indent_unit(py: Python<'_>, name: &str, value: *mut ffi::PyObject) -> PyResult<Option<Vec<u8>>> {
    use pyo3::exceptions::{PyTypeError, PyValueError};
    if value == ffi::Py_None() {
        return Ok(None);
    }
    if ffi::PyUnicode_Check(value) != 0 {
        return str_utf8(py, name, "indent", value).map(Some);
    }
    if ffi::PyLong_Check(value) == 0 || ffi::PyBool_Check(value) != 0 {
        let ty = pyo3::Bound::from_borrowed_ptr(py, value).get_type();
        let tn = ty.name().map(|n| n.to_string()).unwrap_or_default();
        return Err(PyTypeError::new_err(format!(
            "rjson.{name}() indent must be None, an int or a str, not {tn}"
        )));
    }
    let v = ffi::PyLong_AsLongLong(value);
    if v == -1 && !ffi::PyErr_Occurred().is_null() {
        ffi::PyErr_Clear();
    } else if (0..=1024).contains(&v) {
        return Ok(Some(vec![b' '; v as usize]));
    }
    Err(PyValueError::new_err(format!(
        "rjson.{name}() indent must be between 0 and 1024"
    )))
}

/// `passthrough=`: None or an int made of `rjson.PASSTHROUGH_*` flags.
unsafe fn passthrough_flags(
    py: Python<'_>,
    name: &str,
    value: *mut ffi::PyObject,
) -> PyResult<u32> {
    use crate::native::PT_ALL;
    use pyo3::exceptions::{PyTypeError, PyValueError};
    if value == ffi::Py_None() {
        return Ok(0);
    }
    if ffi::PyLong_Check(value) == 0 || ffi::PyBool_Check(value) != 0 {
        let ty = pyo3::Bound::from_borrowed_ptr(py, value).get_type();
        let tn = ty.name().map(|n| n.to_string()).unwrap_or_default();
        return Err(PyTypeError::new_err(format!(
            "rjson.{name}() passthrough must be an int of rjson.PASSTHROUGH_* flags, not {tn}"
        )));
    }
    let v = ffi::PyLong_AsLongLong(value);
    if v == -1 && !ffi::PyErr_Occurred().is_null() {
        ffi::PyErr_Clear();
    } else if (0..=PT_ALL as i64).contains(&v) {
        return Ok(v as u32);
    }
    Err(PyValueError::new_err(format!(
        "rjson.{name}() passthrough has unknown flags (valid: 0..{PT_ALL}, a combination of rjson.PASSTHROUGH_*)"
    )))
}

/// `loads(data: str | bytes | bytearray | memoryview, /, *, lenient=False) -> Any`
unsafe extern "C" fn loads(
    module: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::get_trampoline_function!(fastcall_cfunction_with_keywords, loads_body)(
        module, args, nargs, kwnames,
    )
}

/// `loads_ndjson(data, /, *, lenient=False) -> list`
unsafe extern "C" fn loads_ndjson(
    module: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::get_trampoline_function!(fastcall_cfunction_with_keywords, loads_ndjson_body)(
        module, args, nargs, kwnames,
    )
}

/// `dumps(obj, /, *, default=None) -> bytes` (UTF-8, like `orjson.dumps`);
/// also exported as `dumps_bytes`, its name before `dumps` switched from
/// `str` to `bytes`.
unsafe extern "C" fn dumps(
    module: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::get_trampoline_function!(fastcall_cfunction_with_keywords, dumps_body)(
        module, args, nargs, kwnames,
    )
}

/// `dumps_str(obj, /, *, default=None) -> str` (like
/// `json.dumps(obj, ensure_ascii=False)`, compact)
unsafe extern "C" fn dumps_str(
    module: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::get_trampoline_function!(
        fastcall_cfunction_with_keywords,
        dumps_str_body
    )(module, args, nargs, kwnames)
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

/// `PyMethodDef`s must outlive the function objects created from them.
/// Wrapped so the raw pointers inside can live in a `static`.
struct MethodDefs([ffi::PyMethodDef; 5]);
// SAFETY: the table is immutable after construction and only read by CPython.
unsafe impl Sync for MethodDefs {}

static METHODS: MethodDefs = MethodDefs([
    ffi::PyMethodDef {
        ml_name: c"loads".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: loads },
        ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
        ml_doc: c"loads(data, /, *, lenient=False)\n--\n\nDeserialize JSON (str, bytes, bytearray or memoryview) to Python objects.\n\nlenient: also accept what json.loads accepts (NaN/Infinity, a UTF-8 BOM on bytes, numbers overflowing to inf, lone surrogates, UTF-16/32 bytes); the result then equals json.loads's.".as_ptr(),
    },
    ffi::PyMethodDef {
        ml_name: c"loads_ndjson".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: loads_ndjson },
        ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
        ml_doc: c"loads_ndjson(data, /, *, lenient=False)\n--\n\nDeserialize newline-delimited JSON (NDJSON / JSON Lines: one document per line) to a list, in one call.\n\nEach line gives what loads(line) gives; blank lines are skipped; lines end with \\n or \\r\\n. An error's position (pos, lineno, colno) is in the whole input.\nlenient: as in loads.".as_ptr(),
    },
    ffi::PyMethodDef {
        ml_name: c"dumps".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: dumps },
        ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
        ml_doc: c"dumps(obj, /, *, default=None, passthrough=0, non_str_keys=False)\n--\n\nSerialize a Python object to compact UTF-8 JSON bytes (like orjson.dumps). Also serializes datetime/date/time, uuid.UUID, dataclasses and Enum members.\n\ndefault: called with each object that cannot be serialized; its return value is serialized instead.\npassthrough: rjson.PASSTHROUGH_* flags; those types go to default instead.\nnon_str_keys: allow int, float, bool, None, Enum, datetime/date/time and UUID dict keys (as json.dumps writes them for int/float/bool/None).".as_ptr(),
    },
    ffi::PyMethodDef {
        ml_name: c"dumps_str".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: dumps_str },
        ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
        ml_doc: c"dumps_str(obj, /, *, default=None, passthrough=0, non_str_keys=False)\n--\n\nSerialize a Python object to a compact JSON str (non-ASCII kept as-is).\n\ndefault, passthrough, non_str_keys: as in dumps.".as_ptr(),
    },
    ffi::PyMethodDef {
        ml_name: c"dumps_bytes".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: dumps },
        ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
        ml_doc: c"dumps_bytes(obj, /, *, default=None, passthrough=0, non_str_keys=False)\n--\n\nAlias of dumps (returns bytes); kept for compatibility.".as_ptr(),
    },
]);

/// Public names (`from rjson import *`). The wheel installs this extension
/// as `rjson/rjson.*.so` behind a maturin-generated `rjson/__init__.py`
/// that does `from .rjson import *` and copies `__all__`, so a name missing
/// here (notably the underscore-prefixed `__version__`) would not be
/// reachable as `rjson.<name>`. Keep in sync with `rjson.pyi`.
const ALL: [&str; 12] = [
    "JSONDecodeError",
    "JSONEncodeError",
    "PASSTHROUGH_DATACLASS",
    "PASSTHROUGH_DATETIME",
    "PASSTHROUGH_ENUM",
    "PASSTHROUGH_UUID",
    "__version__",
    "dumps",
    "dumps_bytes",
    "dumps_str",
    "loads",
    "loads_ndjson",
];

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    // `__module__` of the functions, which CPython also uses in argument
    // errors ("rjson.dumps() takes no keyword arguments"): the public package
    // `rjson`, not this extension's import name `rjson.rjson`.
    let full_name = m.name()?;
    let modname = match full_name.to_str()?.rsplit_once('.') {
        Some((parent, _)) => pyo3::types::PyString::new(py, parent),
        None => full_name,
    };
    for def in METHODS.0.iter() {
        // CPython never writes through the def pointer.
        let def = def as *const ffi::PyMethodDef as *mut ffi::PyMethodDef;
        unsafe {
            let f = ffi::PyCFunction_NewEx(def, m.as_ptr(), modname.as_ptr());
            let f = Bound::from_owned_ptr_or_err(py, f)?.cast_into::<PyCFunction>()?;
            let name = std::ffi::CStr::from_ptr((*def).ml_name).to_str().unwrap_or_default();
            m.add(name, f)?;
        }
    }
    m.add("JSONEncodeError", crate::ser::encode_error_type(py)?.bind(py))?;
    // The very same class as json.JSONDecodeError (what loads raises). Also
    // warms the lookup `loads` would otherwise do on its first error.
    m.add("JSONDecodeError", crate::parser::decode_error_type(py)?.bind(py))?;
    m.add("PASSTHROUGH_DATETIME", crate::native::PT_DATETIME)?;
    m.add("PASSTHROUGH_UUID", crate::native::PT_UUID)?;
    m.add("PASSTHROUGH_DATACLASS", crate::native::PT_DATACLASS)?;
    m.add("PASSTHROUGH_ENUM", crate::native::PT_ENUM)?;
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add("__all__", pyo3::types::PyList::new(py, ALL)?)?;
    Ok(())
}
