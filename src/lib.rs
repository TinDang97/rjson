//! rjson: fast JSON for Python, backed by Rust.
//!
//! - `parser.rs`: `loads`, a single-pass parser that builds CPython objects
//!   directly (key cache, exact-size lists, correctly rounded floats).
//! - `lemire.rs`: Eisel-Lemire float conversion used by the parser.
//! - `ser.rs`: `dumps` / `dumps_bytes`, a direct C-API serializer writing
//!   straight into the result `str`/`bytes` object.
//! - `entry.rs`: raw `METH_O` entry points and module registration.

use pyo3::prelude::*;

mod compat;
mod entry;
mod lemire;
mod parser;
mod ser;

/// Python module definition for rjson.
#[pymodule]
fn rjson(py: Python, m: &Bound<'_, PyModule>) -> PyResult<()> {
    compat::self_test(py)?;
    ser::init(py);
    entry::register(m)?; // loads / dumps / dumps_bytes as raw METH_O builtins
    Ok(())
}
