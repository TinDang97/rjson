# CLAUDE.md - AI Assistant Guide for rjson

## Project Overview

**rjson** is a JSON library for Python written in Rust directly against the CPython C API (PyO3 is used only for module setup and the entry-point trampoline). Goal: beat orjson on every metric while staying correct on every supported CPython version.

- API: `loads(str | bytes | bytearray | memoryview, *, lenient=False)`, `dumps(obj, *, default=None, passthrough=0, non_str_keys=False) -> bytes` (like orjson), `dumps_str(...) -> str`, `dumps_bytes` = alias of `dumps`; `PASSTHROUGH_DATETIME/_UUID/_DATACLASS/_ENUM` flags; `JSONDecodeError` (= json's), `JSONEncodeError(TypeError, ValueError)`, `__version__`; stub `rjson.pyi` (maturin installs it as `rjson/__init__.pyi` + `py.typed`)
- Packaging: PyPI distribution `pyrjson` (the name `rjson` is taken); import name `rjson`. MIT license.
- Supported: CPython 3.10-3.14 (`requires-python >=3.10`), GIL builds only
- Status: experimental; APIs may change before 1.0
- Current numbers, remaining gaps and roadmap: **`docs/PERFORMANCE_REVIEW.md`** (keep it updated when performance changes); production workloads, migration and adoption blockers: `docs/PRODUCTION_READINESS.md`; async/threading: `docs/ASYNC.md`

## Repository Structure

```
src/
  lib.rs      # module definition only
  entry.rs    # raw entry points (loads, dumps, dumps_str, dumps_bytes alias: FASTCALL|KEYWORDS) + registration
  parser.rs   # loads: single-pass parser building PyObjects directly
  lemire.rs   # Eisel-Lemire float conversion (vendored from fast-float, MIT/Apache)
  ser.rs      # dumps (bytes) / dumps_str (str): direct serializer writing into the result object
  native.rs   # datetime/date/time, UUID, dataclass, Enum for dumps: lazy type lookup + formatting
  compat.rs   # version-portable str accessors (3.14: own bitfield reader + import self-test)
              # and extern decls of private-but-exported C-API symbols
build.rs      # pyo3_build_config::use_pyo3_cfgs() -> Py_3_10/Py_3_12... cfgs
rjson.pyi     # type stub (installed as rjson/__init__.pyi + py.typed)
examples/     # FastAPI, JSON logging/NDJSON, Redis/Kafka codec (tested by tests/test_examples.py)
tests/        # test_rjson.py (general + regressions), test_dumps.py (serializer), test_lenient.py (loads lenient=), test_native.py
              # (datetime/UUID/dataclass/Enum, differential vs orjson), test_keys.py
              # (non_str_keys, vs json), test_examples.py
benches/corpus_benchmark.py   # reference benchmark vs orjson (ratio, same process; --output-json)
benches/production_benchmark.py, prod_workloads.py   # production-shaped workloads (web/logs/big files/codec), time + RSS
benches/fetch_corpus.sh       # download the corpora (sha256-pinned) into benches/data/
benches/perf_gate.py          # compare base/head benchmark runs, fail on >5% geomean regression
benches/make_charts.py        # README charts (docs/img/*.svg) + table from a --json --output-json run
scripts/build_pgo.sh, scripts/pgo_train.py   # PGO wheel build; training is synthetic, disjoint from the benchmark
.github/workflows/            # ci.yml (clippy + tests), wheels.yml (PGO wheels), perf.yml (perf gate, label `perf`)
docs/PERFORMANCE_REVIEW.md    # review findings, results, ranked roadmap
docs/PRODUCTION_READINESS.md  # production workloads report, migration, adoption blockers
docs/ASYNC.md                 # asyncio/threads guidance, free-threading/subinterpreter roadmap
.cargo/config.toml            # x86-64-v2 target (never target-cpu=native)
```

## Architecture Notes

### loads (`parser.rs`)
- Hand-written recursive-descent parser over `&[u8]`; one reusable value stack (pooled across calls); lists created at exact size.
- Dict-key cache (2048 entries, keys ≤ 64 bytes): reuses PyUnicode objects with precomputed hash; byte-compares on hit.
- Numbers: 8-digits-at-a-time ints (big ints exact via PyLong_FromString), floats correctly rounded (exact fast path → Eisel-Lemire → fast_float). The fast path takes up to 19 fraction digits / 19 significant digits (`digits19`, lookahead 48 bytes), so full-precision doubles stay on it; keep its checks branch-free where data mixes shapes (0.0 among other values).
- Own UTF-8 decoder writing into the final str; bytes input validated once with simdutf8. UCS2 results go through `decode_ucs2` (SSSE3, cfg-gated): 8/16-byte ASCII widening, 5×3-byte blocks via one u64 pattern pre-check + masked compare + shuffles, otherwise two chars per trip. Vector paths only for whole blocks with a constant advance (a data-dependent advance serialized the loop); wide stores only when that many units are left (`nchars`).
- Cyclic GC paused during parsing on CPython 3.10/3.11 only (`pause_gc`/`resume_gc`).
- `lenient=True` (issue #7): accepts exactly what `json.loads` accepts. `NaN`/`Infinity`/`-Infinity` (`parse_other`, `nonfinite_literal`), overflow to `inf` (`infinite_number`) and a UTF-8 BOM on bytes (skipped in `entry::loads_impl`) are native, checked only on paths that are errors in strict mode; anything else rejected goes to `json.loads` (`entry::loads_fallback`: lone surrogates, UTF-16/32, depth > 1024). If both reject, rjson's error is raised unless json's `JSONDecodeError.pos` is later; non-ValueError/RecursionError exceptions from the fallback propagate.
- Errors: `json.JSONDecodeError` (exported as `rjson.JSONDecodeError`); `.msg` is the bare reason; positions match json (trailing comma at the comma); depth limit 1024; trailing content rejected.
- Input: the parser needs a readable NUL after the document. memoryview: parsed in place when C-contiguous, >= 4 KiB and ending exactly where its `bytes`/`bytearray` ends (found via `memoryview.obj`; export held in `Input::held`), else copied in C order (any layout). Non-ASCII `str` >= 4096 chars without a cached UTF-8 copy: temporary `bytes` via `PyUnicode_AsUTF8String` (`Input::temp`), so no copy stays attached to the caller's string.

### dumps (`ser.rs`)
- Exact `ob_type` pointer dispatch on raw borrowed pointers; subclasses handled on a slower path.
- Writes straight into a `bytes` object or a compact ASCII `str`; for non-ASCII `str` output, source strings' native UCS1/2/4 data is copied into a result of the exact kind (no UTF-8 round trip), checking/escaping each string right before copying it.
- Writers take and return the output cursor (`Cur`, null = error) so it stays in a register; `Out::len` is only synced on growth/finish.
- Dicts: direct entry iteration on CPython 3.11-3.13 (cfg `rjson_dict_direct` from build.rs + import-time self-test vs `PyDict_Next`); split tables and other versions use `PyDict_Next`. Adding a Python version means checking `struct _dictkeysobject` in its `pycore_dict.h` first.
- Lists: runs of exact ints/floats go through a register-resident loop with a per-item exact type check.
- Floats via zmij (orjson-identical output, `1e+16`), ints via inline digit reader + itoap.
- Escaping: AVX-512VL / AVX2 (runtime detected) / SSE2 kernels; every write reserves its worst case first. 256-bit loads/stores go through `load256`/`store256` (inline asm), because x86-64-v2 tuning makes LLVM split them. Test the fallbacks with `RUSTFLAGS="-C target-cpu=x86-64-v2 --cfg rjson_no_avx512 --cfg rjson_no_avx2"`.
- Output buffer headroom (1/16) must stay below the shrink threshold (1/8): shrinking every call makes glibc mmap and page-fault every large result.
- Capacity: initial = min of the last two output sizes per thread and mode (`SizeHistory`); first growth jumps to the peak of the last 64 calls; growth past 1 MiB reserves ≥ 32 MiB + 64 KiB (always mmapped, shrunk back in `into_object`). Keep a result from ever holding the reservation.
- Recursion limit 254 (also catches circular references); each `default` call counts as a level, so a non-converging `default` ends there.
- Guarded mode (`Serializer::guard`): Python code run mid-serialization may mutate or free what we iterate. In guarded mode every list/tuple/dict is held by a strong ref while serialized (`guarded`), dicts use `PyDict_Next` plus a size check (CPython's "changed size during iteration" RuntimeError), never the direct entry walk; `call_default` and the native writers incref their object; `err_obj` is a strong ref. `default=` sets it. Otherwise no Python code may run: a native value that would run some (dataclass, a tzinfo other than `timezone`/C `ZoneInfo`, an Enum with custom attribute access, a UUID type whose `int` is not a plain slot) returns `SerError::NeedGuard` *before* running any, and `dumps_raw` restarts the whole call in guarded mode. Any new code path that can run Python code must do the same; the tests mutate containers from such code under `PYTHONMALLOC=debug`.
- `non_str_keys=True` (issue #6): `dict_item`'s non-str branch calls cold `key_text`; bool/None/int/float keys get exactly `json.dumps`'s text (float `repr` rebuilt from zmij's digits in `native::fmt_float_repr`, which equal CPython's `repr` digits; Rust's `{:e}` does not), Enum/datetime/date/time/UUID keys follow orjson's `OPT_NON_STR_KEYS`. Str keys never reach it, so the option costs nothing when off. Options travel in `ser::DumpsOpts`.
- Native types (`native.rs`, issue #5): exact `datetime`/`date`/`time`/`uuid.UUID` types, any `Enum` (metaclass check; int/str/float mix-ins are caught earlier as subclasses), dataclasses (`__dataclass_fields__` in the type's own dict). Output is orjson's byte for byte (differential test in `test_native.py`) except orjson's crashes/invalid output (documented there). Types come from `sys.modules` lazily; rjson never imports a module. Checked only after all builtin checks (`ser_other`, cold), so JSON-native documents pay nothing.
- Non-ASCII strings in bytes output: cached UTF-8 copy if present; < 256 chars via `PyUnicode_AsUTF8AndSize` (attaches the copy: fast repeats); longer UCS2/UCS4 via `encode_utf8_escaped` (direct, ASCII 8-blocks only at ASCII units), longer Latin-1 via a temporary `PyUnicode_AsUTF8String`. No copy attached to long strings.
- Every serializer-detected failure raises `rjson.JSONEncodeError` (`ser::to_pyerr`, cold); Python-raised errors (`SerError::PyErrSet`) propagate unchanged.

### Entry points (`entry.rs`)
- Raw builtins through PyO3's `impl_::trampoline` (`get_trampoline_function!(binaryfunc | fastcall_cfunction_with_keywords, ..)`; doc-hidden PyO3 API, keeps panics caught and GIL bookkeeping correct; re-check on every PyO3 upgrade). ~8 ns/call cheaper than `#[pyfunction]`. `loads`/`dumps`/`dumps_str` are `METH_FASTCALL | METH_KEYWORDS` whose one-positional, no-kwnames call is a single compare (`dumps_args`; `loads_body`), the rest in cold `dumps_args_slow`/`loads_args_slow`. Keep one call site of `loads_impl` (`#[inline(always)]`, as is `parser::parse`): with two, LLVM stopped inlining `get_input`/`parse` (+60 instructions per call).
- `ALL` in entry.rs must list every public name (maturin's generated `__init__.py` star-imports from `rjson.rjson`); keep it in sync with `rjson.pyi`. The functions' `__module__` is the package `rjson`.
- New keyword options go into `dumps_args_slow`'s hand-parsed kwnames, not PyO3 `FunctionDescription`.

## Hard Rules (learned from bugs found in review)

- **Never hard-code CPython object layouts.** Use `ffi::PyUnicode_DATA`, `PyUnicode_IS_COMPACT_ASCII`, etc., or `#[cfg(Py_3_12)]`-gated layouts with an init-time self-test. A fixed str data offset broke all of 3.12+.
- **Reserve worst-case output before raw writes** (escapes are up to 6x). A `len + 64` reserve caused a heap overflow.
- **Check the exact type of every element**; never trust a sample of a container.
- **Only take the ASCII fast path for exact, compact ASCII `str`**; subclasses are not compact.
- **Check every C-API NULL return** and propagate the Python error; never return success with an exception set.
- **Never set `target-cpu=native`** or global `+avx2`: use `#[target_feature]` + runtime detection.
- **Never pass extra flags via `RUSTFLAGS`**: it silently replaces `.cargo/config.toml`'s rustflags (x86-64-v2). Use `CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, which cargo merges (the old PGO script built x86-64 v1 wheels this way).
- **Private CPython symbols/layouts are version-gated and listed here**: `_PyBytes_Resize`, `_PyDict_NewPresized` (compat.rs), `_PyDict_FromItems` (parser.rs, 3.13 only), dict keys layout (ser.rs, `rjson_dict_direct`, 3.11-3.13), str state bitfield (compat.rs, 3.14). Public-header structs read directly (re-check on a new version): int digits (`LongHeader`, self-tested; also used for `UUID.int`), `PyMemberDescrObject.d_member` (native.rs, checked by member name), datetime fields via pyo3-ffi's `PyDateTime_*` accessors. Re-verify each against the new version's headers before widening a gate. They must be `PyAPI_FUNC` (exported on Windows); build.rs links the full `python3XY.lib` there because pyo3-ffi's `raw-dylib` imports only its own declarations.
- Keep the module GIL-only (no free-threading declaration) until borrowed list/dict iteration is audited.
- `panic = "abort"` is set: a panic kills the interpreter, so do not `unwrap` on Python-derived data.

## Development Workflow

```bash
uv venv .venv -p 3.11 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release          # build + install into the venv
python -m pytest tests -q          # must pass on 3.10-3.14 (CI runs all of them)
benches/fetch_corpus.sh            # corpora -> benches/data/ (default --data)
python benches/corpus_benchmark.py
scripts/build_pgo.sh python3.11 python3.13   # PGO wheels -> target/wheels/
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
