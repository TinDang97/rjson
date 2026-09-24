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

    // Direct dict-entry iteration in dumps relies on the private
    // PyDictKeysObject layout, verified for CPython 3.11-3.13 GIL builds
    // (plus an init-time self-test). Other versions use PyDict_Next.
    println!("cargo::rustc-check-cfg=cfg(rjson_dict_direct)");
    let cfg = pyo3_build_config::get();
    let minor = if cfg.implementation == pyo3_build_config::PythonImplementation::CPython
        && cfg.version.major == 3
    {
        cfg.version.minor
    } else {
        0
    };
    let free_threaded = cfg
        .build_flags
        .0
        .contains(&pyo3_build_config::BuildFlag::Py_GIL_DISABLED);
    if (11..=13).contains(&minor) && !free_threaded && !cfg.abi3 {
        println!("cargo:rustc-cfg=rjson_dict_direct");
    }
}
