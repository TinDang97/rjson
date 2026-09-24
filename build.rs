fn main() {
    // Emit `Py_3_x` cfgs matching the target interpreter so version-specific
    // CPython object layouts (e.g. PyLongObject in 3.12+) can be selected.
    pyo3_build_config::use_pyo3_cfgs();
    // Test-only switches: RUSTFLAGS="--cfg rjson_no_avx512" disables the
    // AVX-512 string kernel so the SSE2/AVX2 paths can be tested, and
    // "--cfg rjson_no_avx2" the AVX2 scans (RUSTFLAGS replaces the
    // .cargo/config.toml flags, so also pass -C target-cpu=x86-64-v2).
    println!("cargo::rustc-check-cfg=cfg(rjson_no_avx512)");
    println!("cargo::rustc-check-cfg=cfg(rjson_no_avx2)");
}
