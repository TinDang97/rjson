//! Python-facing entry points: raw `METH_O` functions.
//!
//! `loads`/`dumps` are registered as plain CPython `METH_O` builtins instead of
//! PyO3 `#[pyfunction]`s. A `#[pyfunction]` is `METH_FASTCALL|METH_KEYWORDS`
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
//! We keep PyO3's trampoline (`impl_::trampoline::binaryfunc`, present with
//! the same signature in 0.24 .. 0.29): for ~2 ns it maintains PyO3's
//! GIL_COUNT, so dropping a `Py<T>` anywhere below decrefs immediately
//! instead of being deferred to PyO3's global reference pool, and it traps
//! panics at the FFI boundary. It is a `#[doc(hidden)]` API: PyO3 is pinned
//! and the call sites are confined to this file.
//!
//! When keyword options are added (`default=`, `option=`), switch to
//! `METH_FASTCALL | METH_KEYWORDS` with hand-parsed `kwnames` (orjson style),
//! not PyO3's `FunctionDescription`.
//!
//! The work itself lives in `crate::parser::parse` / `crate::dumps_impl`; this
//! file only converts arguments and results.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyCFunction;
use std::os::raw::c_char;

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

unsafe fn dumps_body(py: Python<'_>, _m: *mut ffi::PyObject, arg: *mut ffi::PyObject) -> PyResult<*mut ffi::PyObject> {
    let obj = Bound::from_borrowed_ptr(py, arg);
    crate::dumps_impl(&obj, |bytes| {
        // Straight from the reused buffer into a new str (no Rust String).
        let p = ffi::PyUnicode_FromStringAndSize(bytes.as_ptr() as *const c_char, bytes.len() as ffi::Py_ssize_t);
        if p.is_null() {
            Err(PyErr::fetch(py))
        } else {
            Ok(p)
        }
    })
}

/// `loads(obj: str | bytes | bytearray | memoryview) -> Any`
unsafe extern "C" fn loads(module: *mut ffi::PyObject, arg: *mut ffi::PyObject) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::binaryfunc(module, arg, loads_body)
}

/// `dumps(obj: Any) -> str`
unsafe extern "C" fn dumps(module: *mut ffi::PyObject, arg: *mut ffi::PyObject) -> *mut ffi::PyObject {
    pyo3::impl_::trampoline::binaryfunc(module, arg, dumps_body)
}

// ---------------------------------------------------------------------------
// registration
// ---------------------------------------------------------------------------

/// `PyMethodDef`s must outlive the function objects created from them.
/// Wrapped so the raw pointers inside can live in a `static`.
struct MethodDefs([ffi::PyMethodDef; 2]);
// SAFETY: the table is immutable after construction and only read by CPython.
unsafe impl Sync for MethodDefs {}

static METHODS: MethodDefs = MethodDefs([
    ffi::PyMethodDef {
        ml_name: c"loads".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunction: loads },
        ml_flags: ffi::METH_O,
        ml_doc: c"loads(obj, /)\n--\n\nDeserialize JSON (str, bytes, bytearray or memoryview) to Python objects.".as_ptr(),
    },
    ffi::PyMethodDef {
        ml_name: c"dumps".as_ptr(),
        ml_meth: ffi::PyMethodDefPointer { PyCFunction: dumps },
        ml_flags: ffi::METH_O,
        ml_doc: c"dumps(obj, /)\n--\n\nSerialize a Python object to a JSON str.".as_ptr(),
    },
]);

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = m.py();
    let modname = m.name()?;
    for def in METHODS.0.iter() {
        // CPython never writes through the def pointer.
        let def = def as *const ffi::PyMethodDef as *mut ffi::PyMethodDef;
        unsafe {
            let f = ffi::PyCFunction_NewEx(def, m.as_ptr(), modname.as_ptr());
            let f = Bound::from_owned_ptr_or_err(py, f)?.downcast_into::<PyCFunction>()?;
            let name = std::ffi::CStr::from_ptr((*def).ml_name).to_str().unwrap_or_default();
            m.add(name, f)?;
        }
    }
    Ok(())
}
