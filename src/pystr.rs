//! Version-correct zero-copy access to the UTF-8 bytes of a Python `str`.
//!
//! Replaces the hand-rolled `PyASCIIObject` struct + hard-coded data offset
//! (48 bytes) that was only correct for CPython <= 3.11: 3.12 removed the
//! `wstr` field, so compact-ASCII data lives at offset 40 and the old code
//! read 8 bytes past the start of the payload (garbage output, heap
//! over-read, and a process abort on 3.12+). The free-threaded (3.13t+)
//! object header is different again.
//!
//! `pyo3::ffi::PyUnicode_IS_COMPACT_ASCII` / `PyUnicode_DATA` /
//! `PyUnicode_GET_LENGTH` are `#[inline]` Rust ports of the CPython macros,
//! compiled against the target interpreter's layout, so they cost the same as
//! the hand-rolled version while being correct on every supported version.
//! (They are unavailable under `Py_LIMITED_API`/PyPy; this crate already
//! requires the full CPython API.)

use pyo3::ffi;
use pyo3::prelude::*;

/// Borrow the UTF-8 encoding of `op`.
///
/// * compact ASCII: points straight into the string object (no copy, no cache).
/// * otherwise: CPython's cached UTF-8 (`PyUnicode_AsUTF8AndSize`), which is
///   created on first use and then reused by later calls.
///
/// Returns `Err` (with the Python exception fetched, so none is left pending)
/// for strings that cannot be encoded, e.g. lone surrogates.
///
/// # Safety
/// `op` must be a valid, live `str` object; the returned slice borrows from it.
#[inline(always)]
pub unsafe fn utf8<'a>(py: Python<'_>, op: *mut ffi::PyObject) -> PyResult<&'a [u8]> {
    if ffi::PyUnicode_IS_COMPACT_ASCII(op) != 0 {
        let p = ffi::PyUnicode_DATA(op) as *const u8;
        let n = ffi::PyUnicode_GET_LENGTH(op) as usize;
        return Ok(std::slice::from_raw_parts(p, n));
    }
    utf8_slow(py, op)
}

#[inline(never)]
unsafe fn utf8_slow<'a>(py: Python<'_>, op: *mut ffi::PyObject) -> PyResult<&'a [u8]> {
    let mut n: ffi::Py_ssize_t = 0;
    let p = ffi::PyUnicode_AsUTF8AndSize(op, &mut n);
    if p.is_null() {
        return Err(PyErr::fetch(py));
    }
    Ok(std::slice::from_raw_parts(p as *const u8, n as usize))
}
