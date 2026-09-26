# Contributing to rjson

Thanks for helping. rjson talks to the CPython C API directly, so a small mistake can crash
the interpreter or corrupt memory instead of raising an exception. The rules below exist
because each of them was learned from a real bug.

## Development setup

You need Rust (stable) and CPython 3.10–3.14 with headers.

```bash
uv venv .venv -p 3.13 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release          # build + install the extension into the venv
python -m pytest tests -q          # full suite, runs in about a second
```

The PyPI distribution is named `pyrjson`, but the import name is `rjson`.

To test another Python version, build a wheel for it and install it into a matching venv:

```bash
maturin build --release -i python3.10 -i python3.12 -o dist/
```

CI runs the suite on CPython 3.10–3.14 on Linux x86_64, plus aarch64, macOS arm64 and
Windows. `scripts/test_aarch64_qemu.sh` runs the aarch64 build locally under qemu.

### Testing the SIMD fallbacks

x86_64 builds pick AVX-512 / AVX2 / SSE2 kernels at runtime. To exercise the fallbacks on
a machine that has AVX-512:

```bash
CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS="--cfg rjson_no_avx512 --cfg rjson_no_avx2" \
  maturin develop --release
```

Never put extra flags in `RUSTFLAGS`. It silently replaces the `x86-64-v2` target from
`.cargo/config.toml`. Use `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, which cargo merges with it.

## Before you open a pull request

- [ ] `python -m pytest tests -q` passes (ideally on the oldest and newest supported Python).
- [ ] `cargo clippy --release` has no new warnings.
- [ ] Every bug fix has a regression test in `tests/`.
- [ ] Output stays byte-identical unless the change is meant to alter it (then document it).
- [ ] For performance changes, include before/after numbers from the same host (see below),
      and update `docs/PERFORMANCE_REVIEW.md` and the README table when numbers move.

## Hard rules for Rust code

The full list, with the bugs behind each rule, is in [`CLAUDE.md`](CLAUDE.md). In short:

- **Never hard-code CPython object layouts.** Use the `ffi` accessors, or a layout gated by
  `#[cfg(Py_3_XY)]` with an import-time self-test.
- **Reserve the worst-case output size before any raw write.** An escape can be up to 6×
  the input.
- **Check the exact type of every element.** Never trust a sample of a container.
- **Check every C-API `NULL` return** and propagate the Python error.
- **Don't `unwrap` on data that came from Python.** `panic = "abort"` kills the interpreter.
- **Version-gate private CPython symbols** and list them in `CLAUDE.md`. Re-verify each one
  against the new version's headers before widening a gate.
- **No `target-cpu=native` or global `+avx2`.** Use `#[target_feature]` with runtime detection.
- Comment every `unsafe` block with the invariant it relies on.

## Benchmarks

Always compare against orjson in the same process and report the ratio. Shared and dev
hosts are noisy (±10%), so repeat a run before believing any change under 10%.

```bash
benches/fetch_corpus.sh                                   # sha256-pinned corpora
python benches/corpus_benchmark.py --json --repeat 11     # reference benchmark
python benches/production_benchmark.py --quick            # production-shaped workloads
```

Label a pull request `perf` to run `.github/workflows/perf.yml`. It builds the base and the
head, runs them interleaved, and fails on a geomean regression of more than 5% against
orjson.

## Code style

- Rust 2021 edition, `cargo fmt`, `cargo clippy`.
- Python: PEP 8, pytest tests, type hints in examples.
- Commit messages explain *why*, not just what.

## License

By contributing you agree that your contributions are licensed under the [MIT License](LICENSE).
