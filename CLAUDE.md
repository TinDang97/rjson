# CLAUDE.md - AI Assistant Guide for rjson

## Project Overview

**rjson** is a JSON library for Python written in Rust directly against the CPython C API (PyO3 is used only for module setup and the entry-point trampoline). Goal: beat orjson on every metric while staying correct on every supported CPython version.

- API: `loads(str | bytes | bytearray | memoryview)`, `dumps(obj) -> str`, `dumps_bytes(obj) -> bytes`
- Status: experimental; APIs may change (open decision: whether `dumps` should return `bytes`)
- Current numbers, remaining gaps and roadmap: **`docs/PERFORMANCE_REVIEW.md`** (keep it updated when performance changes)

## Repository Structure

```
src/
  lib.rs      # module definition only
  entry.rs    # raw METH_O entry points (loads, dumps, dumps_bytes) + registration
  parser.rs   # loads: single-pass parser building PyObjects directly
  lemire.rs   # Eisel-Lemire float conversion (vendored from fast-float, MIT/Apache)
  ser.rs      # dumps/dumps_bytes: direct serializer writing into the result object
build.rs      # pyo3_build_config::use_pyo3_cfgs() -> Py_3_10/Py_3_12... cfgs
tests/        # test_rjson.py (general + regressions), test_dumps.py (serializer)
benches/corpus_benchmark.py   # reference benchmark vs orjson (ratio, same process)
scripts/build_pgo.sh, scripts/pgo_train.py   # PGO wheel build
docs/PERFORMANCE_REVIEW.md    # review findings, results, ranked roadmap
.cargo/config.toml            # x86-64-v2 target (never target-cpu=native)
```

## Architecture Notes

### loads (`parser.rs`)
- Hand-written recursive-descent parser over `&[u8]`; one reusable value stack (pooled across calls); lists created at exact size.
- Dict-key cache (2048 entries, keys ≤ 64 bytes): reuses PyUnicode objects with precomputed hash; byte-compares on hit.
- Numbers: 8-digits-at-a-time ints (big ints exact via PyLong_FromString), floats correctly rounded (exact fast path → Eisel-Lemire → fast_float).
- Own UTF-8 decoder writing into the final str; bytes input validated once with simdutf8.
- Cyclic GC paused during parsing on CPython 3.10/3.11 only (`pause_gc`/`resume_gc`).
- Errors: `json.JSONDecodeError` with position; depth limit 1024; trailing content rejected.

### dumps (`ser.rs`)
- Exact `ob_type` pointer dispatch on raw borrowed pointers; subclasses handled on a slower path.
- Writes straight into a `bytes` object or a compact ASCII `str`; for non-ASCII `str` output, source strings' native UCS1/2/4 data is copied into a result of the exact kind (no UTF-8 round trip).
- Floats via zmij (orjson-identical output, `1e+16`), ints via inline digit reader + itoap.
- Escaping: AVX-512VL / AVX2 (runtime detected) / SSE2 kernels; every write reserves its worst case first.
- Recursion limit 254 (also catches circular references).

### Entry points (`entry.rs`)
- `METH_O` functions through `pyo3::impl_::trampoline::binaryfunc` (doc-hidden PyO3 API, keeps panics caught and GIL bookkeeping correct). ~8 ns/call cheaper than `#[pyfunction]`.
- Keyword options in future: use `METH_FASTCALL | METH_KEYWORDS` with hand-parsed kwnames, not PyO3 `FunctionDescription`.

## Hard Rules (learned from bugs found in review)

- **Never hard-code CPython object layouts.** Use `ffi::PyUnicode_DATA`, `PyUnicode_IS_COMPACT_ASCII`, etc., or `#[cfg(Py_3_12)]`-gated layouts with an init-time self-test. A fixed str data offset broke all of 3.12+.
- **Reserve worst-case output before raw writes** (escapes are up to 6x). A `len + 64` reserve caused a heap overflow.
- **Check the exact type of every element**; never trust a sample of a container.
- **Only take the ASCII fast path for exact, compact ASCII `str`**; subclasses are not compact.
- **Check every C-API NULL return** and propagate the Python error; never return success with an exception set.
- **Never set `target-cpu=native`** or global `+avx2`: use `#[target_feature]` + runtime detection.
- Keep the module GIL-only (no free-threading declaration) until borrowed list/dict iteration is audited.
- `panic = "abort"` is set: a panic kills the interpreter, so do not `unwrap` on Python-derived data.

## Development Workflow

```bash
uv venv .venv -p 3.11 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release          # build + install into the venv
python -m pytest tests -q          # must pass on 3.11, 3.12, 3.13
RJSON_BENCH_DATA=<corpus dir> python benches/corpus_benchmark.py
scripts/build_pgo.sh python3.11    # PGO wheel -> target/wheels/
cargo clippy --release
```

- Corpora: twitter/citm_catalog/canada from `serde-rs/json-benchmark` `data/`, `github.json` from `ijl/orjson` `data/github.json.xz`.
- Test other Python versions: `maturin build --release -i python3.12 -i python3.13 -o <dir>` and install each wheel into a matching venv.
- Benchmarks: always compare against orjson in the same process (ratio); the dev host is noisy (±10%), so trust geomeans and repeat before believing <10% changes.
- Performance changes: measure each change separately, keep output byte-identical unless intended, update `docs/PERFORMANCE_REVIEW.md` and the README table.

## Code Conventions

- Rust 2021; `cargo fmt`, `cargo clippy`. Comment unsafe blocks with the invariant relied on.
- Python tests: pytest, PEP 8. Every bug fix gets a regression test.
- Commits: clear messages focused on why; run tests first. Push with `git push -u origin <branch>`.
