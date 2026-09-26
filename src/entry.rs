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

use crate::ser::DumpsOpts;

unsafe fn loads_body(py: Python<'_>, _m: *mut ffi::PyObject, arg: *mut ffi::PyObject) -> PyResult<*mut ffi::PyObject> {
    // str / bytes / bytearray / memoryview; see `parser::get_input`.
    let input = crate::parser::get_input(py, arg)?;
    let buf: &[u8] = if input.len == 0 {
        &[]
    } else {
        std::slice::from_raw_parts(input.ptr, input.len)
    };
    crate::parser::parse(py, buf, input.utf8_valid)
}

unsafe fn dumps_body(
    py: Python<'_>,
    _m: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    let (obj, opts) = dumps_args(py, "dumps", args, nargs, kwnames)?;
    crate::ser::dumps_raw(py, obj, false, &opts)
}

unsafe fn dumps_str_body(
    py: Python<'_>,
    _m: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    let (obj, opts) = dumps_args(py, "dumps_str", args, nargs, kwnames)?;
    crate::ser::dumps_raw(py, obj, true, &opts)
}

/// Parses `(obj, /, *, default=None, passthrough=0, non_str_keys=False)` from
/// a vectorcall: returns `obj` and the options. `obj` and the `default`
/// callable (null when absent or None) are borrowed from the call's
/// arguments, which the caller keeps alive for the whole call.
///
/// The common call, one positional argument and no keywords, is a single
/// compare. Errors follow CPython's argument-clinic wording.
#[inline(always)]
unsafe fn dumps_args(
    py: Python<'_>,
    name: &str,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
) -> PyResult<(*mut ffi::PyObject, DumpsOpts)> {
    // PY_VECTORCALL_ARGUMENTS_OFFSET may be set in nargs.
    let n = ffi::PyVectorcall_NARGS(nargs as usize);
    if n == 1 && kwnames.is_null() {
        return Ok((*args, DumpsOpts::NONE));
    }
    dumps_args_slow(py, name, args, n, kwnames)
}

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
    Ok((
        *args,
        DumpsOpts {
            default,
            passthrough,
            non_str_keys,
        },
    ))
}

/// `passthrough=`: None or an int made of `rjson.PASSTHROUGH_*` flags.
unsafe fn passthrough_flags(py: Python<'_>, name: &str, value: *mut ffi::PyObject) -> PyResult<u32> {
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

/// `loads(data: str | bytes | bytearray | memoryview) -> Any`
unsafe extern "C" fn loads(module: *mut ffi::PyObject, arg: *mut ffi::PyObject) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::get_trampoline_function!(binaryfunc, loads_body)(module, arg)
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
struct MethodDefs([ffi::PyMethodDef; 4]);
// SAFETY: the table is immutable after construction and only read by CPython.
unsafe impl Sync for MethodDefs {}

static METHODS: MethodDefs = MethodDefs([
    ffi::PyMethodDef {
        ml_name: c"loads".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunction: loads },
        ml_flags: ffi::METH_O,
        ml_doc: c"loads(data, /)\n--\n\nDeserialize JSON (str, bytes, bytearray or memoryview) to Python objects.".as_ptr(),
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
const ALL: [&str; 11] = [
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
