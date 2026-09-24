fn main() {
    // Expose Py_3_x cfgs (e.g. Py_3_10, Py_3_12) to version-gate C-API usage.
    pyo3_build_config::use_pyo3_cfgs();
}
