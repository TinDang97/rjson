//! CPython API that `pyo3::ffi` (0.26+) no longer provides.
//!
//! - `_PyBytes_Resize` / `_PyDict_NewPresized`: private, but exported by every
//!   supported CPython (checked with `nm -D` on 3.13 and 3.14).
//! - `PyUnicode_IS_ASCII` / `IS_COMPACT` / `IS_COMPACT_ASCII` / `KIND` /
//!   `DATA`: on 3.14 pyo3-ffi drops the inline bitfield readers (the
//!   free-threaded build changed the `state` layout) and turns `KIND` / `DATA`
//!   into out-of-line calls into libpython. The GIL build keeps the 3.12/3.13
//!   layout, so on 3.14 we read the bitfield ourselves and `self_test` checks
//!   it against libpython's exported functions at import time. Below 3.14
//!   these are plain re-exports of pyo3-ffi's inline versions.
#![allow(non_snake_case)]

use pyo3::ffi::{self, PyObject, Py_ssize_t};
use std::os::raw::c_int;

#[cfg(Py_GIL_DISABLED)]
compile_error!("rjson is GIL-only: the free-threaded build is not supported");

extern "C" {
    pub fn _PyBytes_Resize(bytes: *mut *mut PyObject, newsize: Py_ssize_t) -> c_int;
    pub fn _PyDict_NewPresized(minused: Py_ssize_t) -> *mut PyObject;
}

#[cfg(not(Py_3_14))]
pub use ffi::{PyUnicode_DATA, PyUnicode_IS_ASCII, PyUnicode_IS_COMPACT, PyUnicode_IS_COMPACT_ASCII, PyUnicode_KIND};

#[cfg(Py_3_14)]
mod unicode_state {
    use pyo3::ffi;
    use std::os::raw::{c_uint, c_void};

    // interned:2, kind:3, compact:1, ascii:1, statically_allocated:1 (GIL build)
    const KIND_SHIFT: u32 = 2;
    const COMPACT: u32 = 1 << 5;
    const ASCII: u32 = 1 << 6;

    #[inline(always)]
    unsafe fn state(op: *mut ffi::PyObject) -> u32 {
        (*(op as *mut ffi::PyASCIIObject)).state
    }
    #[inline(always)]
    pub unsafe fn PyUnicode_IS_ASCII(op: *mut ffi::PyObject) -> c_uint {
        (state(op) & ASCII != 0) as c_uint
    }
    #[inline(always)]
    pub unsafe fn PyUnicode_IS_COMPACT(op: *mut ffi::PyObject) -> c_uint {
        (state(op) & COMPACT != 0) as c_uint
    }
    #[inline(always)]
    pub unsafe fn PyUnicode_IS_COMPACT_ASCII(op: *mut ffi::PyObject) -> c_uint {
        (state(op) & (ASCII | COMPACT) == (ASCII | COMPACT)) as c_uint
    }
    #[inline(always)]
    pub unsafe fn PyUnicode_KIND(op: *mut ffi::PyObject) -> c_uint {
        (state(op) >> KIND_SHIFT) & 7
    }
    #[inline(always)]
    pub unsafe fn PyUnicode_DATA(op: *mut ffi::PyObject) -> *mut c_void {
        let s = state(op);
        if s & COMPACT != 0 {
            if s & ASCII != 0 {
                (op as *mut ffi::PyASCIIObject).add(1) as *mut c_void
            } else {
                (op as *mut ffi::PyCompactUnicodeObject).add(1) as *mut c_void
            }
        } else {
            (*(op as *mut ffi::PyUnicodeObject)).data.any
        }
    }
}
#[cfg(Py_3_14)]
pub use unicode_state::*;

/// Import-time check of the 3.14 bitfield readers against libpython.
pub fn self_test(py: pyo3::Python<'_>) -> pyo3::PyResult<()> {
    #[cfg(Py_3_14)]
    unsafe {
        use pyo3::prelude::*;
        let samples = py.eval(
            c"[s for t in ('', 'abc', '\\xe9t\\xe9', '\\u65e5\\u672c', '\\U0001f600x') for s in (t, type('S', (str,), {})(t))]",
            None,
            None,
        )?;
        for s in samples.try_iter()? {
            let s = s?;
            let p = s.as_ptr();
            let kind = ffi::PyUnicode_KIND(p);
            let data = ffi::PyUnicode_DATA(p);
            let ascii = s.extract::<String>()?.is_ascii() as u32;
            if PyUnicode_KIND(p) != kind || PyUnicode_DATA(p) != data || PyUnicode_IS_ASCII(p) != ascii {
                return Err(pyo3::exceptions::PyImportError::new_err(
                    "rjson: unexpected CPython str layout (PyASCIIObject.state)",
                ));
            }
        }
    }
    let _ = py;
    Ok(())
}
