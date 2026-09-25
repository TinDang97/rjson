# rjson

**Fast JSON for Python, written in Rust directly against the CPython C API.**
It matches or beats [orjson](https://github.com/ijl/orjson) on every case in our benchmark, and
parses 3.4× / serializes 14.7× faster than the standard library `json`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/headline-dark.svg">
  <img alt="Geometric-mean speedups on CPython 3.13: loads 1.20× faster than orjson, dumps 1.40× faster than orjson, loads 3.36× and dumps 14.7× faster than the standard library json." src="docs/img/headline-light.svg" width="880">
</picture>

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/img/vs-orjson-dark.svg">
  <img alt="Per-case speed relative to orjson. loads: 1.07× to 1.40× faster on all ten cases. dumps: 1.01× to 2.57× faster on all ten cases." src="docs/img/vs-orjson-light.svg" width="880">
</picture>

<details>
<summary>Numbers behind the charts, and how they were measured</summary>

Speedup = other library's time ÷ rjson's time (**higher is better**). Median time per call,
CPython 3.13.12, orjson 3.12.0, x86_64 (Xeon, idle host), plain release build (no PGO).
`dumps` returns `bytes` in both rjson and orjson. Raw results:
[docs/img/benchmark-results.json](docs/img/benchmark-results.json).

| case | loads vs orjson | dumps vs orjson | loads vs json | dumps vs json |
|---|---|---|---|---|
| twitter.json | 1.22× | 1.49× | 2.88× | 13.0× |
| citm_catalog.json | 1.13× | 1.25× | 2.50× | 9.15× |
| canada.json | 1.07× | 1.18× | 4.94× | 18.6× |
| github.json | 1.40× | 1.76× | 2.82× | 16.6× |
| small dict | 1.38× | 1.34× | 4.89× | 17.7× |
| records | 1.20× | 1.50× | 2.20× | 12.9× |
| unicode strings | 1.29× | 1.01× | 1.46× | 44.1× |
| escaped strings | 1.11× | 2.57× | 4.85× | 5.19× |
| int array | 1.10× | 1.14× | 3.06× | 12.8× |
| float array | 1.11× | 1.28× | 7.93× | 18.9× |
| **geomean** | **1.20×** | **1.40×** | **3.36×** | **14.7×** |

Reproduce and redraw:

```bash
benches/fetch_corpus.sh                                   # sha256-pinned corpora -> benches/data/
python benches/corpus_benchmark.py --json --repeat 11 --output-json results.json
python benches/make_charts.py results.json                # -> docs/img/*.svg + this table
```

`dumps_str` (returns `str`) is also faster than orjson on 8 of 10 cases; it trails on
twitter.json and unicode strings, because a `str` holding emoji must be stored at 4 bytes per
character. Methodology, per-version results (3.11 is faster still) and the roadmap:
[docs/PERFORMANCE_REVIEW.md](docs/PERFORMANCE_REVIEW.md).

</details>

## Usage

```python
import rjson

data = rjson.loads('{"name": "rjson", "tags": ["fast", "safe"], "stars": 1e3}')
payload = rjson.dumps(data)        # b'{"name":"rjson","tags":["fast","safe"],"stars":1000.0}'
text = rjson.dumps_str(data)       # same JSON as a str
```

| function | returns | notes |
|---|---|---|
| `loads(data)` | Python object | `data`: `str`, `bytes`, `bytearray` or `memoryview` |
| `dumps(obj)` | `bytes` | compact UTF-8 JSON, like `orjson.dumps` |
| `dumps_str(obj)` | `str` | compact JSON with non-ASCII kept as-is, like `json.dumps(obj, ensure_ascii=False, separators=(",", ":"))` |
| `dumps_bytes(obj)` | `bytes` | alias of `dumps`, kept for compatibility |

- **Types:** `dict` (str keys), `list`, `tuple`, `str`, `int` (any size), `float`, `bool`,
  `None`, and their subclasses (`IntEnum`, `OrderedDict`, `namedtuple`, …).
- **Errors:** `loads` raises `json.JSONDecodeError` (a `ValueError`) with the position.
  `dumps` raises `ValueError`/`TypeError` for unsupported types, non-str keys, NaN/Infinity,
  and nesting deeper than 254 (which also catches circular references). Lone surrogates
  raise `UnicodeEncodeError` in `dumps`; `dumps_str` passes them through.
- **Precision:** floats round-trip exactly (shortest representation, e.g. `1e+16`);
  integers beyond 64 bits are parsed exactly. `loads` accepts nesting up to 1024 levels.

> **Upgrading from an earlier build:** `dumps` used to return `str`. Use `dumps_str` where you
> need a `str`, or `.decode()` the bytes.

## Installation

rjson is not on PyPI yet; build it from source with Rust and Python 3.10–3.14:

```bash
pip install maturin
maturin develop --release        # build and install into the current environment
# or: maturin build --release    # wheel in target/wheels/
```

`.github/workflows/wheels.yml` builds PGO-optimized release wheels (manylinux/musllinux
x86_64 + aarch64, macOS, Windows).

## Compatibility and correctness

- **CPython 3.10–3.14.** The test suite passes on 3.10–3.14 (x86_64) and on aarch64 under
  qemu; CI is set up for Linux x86_64 and aarch64, macOS arm64 and Windows. x86_64 builds
  target x86-64-v2 and pick AVX2/AVX-512 kernels at runtime.
- **Tests:** 250 tests, including a regression test for every bug found during the
  performance review. The review also fuzzed both directions against the standard library
  `json` with no mismatches.
- **Not supported:** PyPy, GraalPy, and free-threaded (no-GIL) CPython builds.
- **Status:** experimental. The API may still change before 1.0.

## How it is fast

- `loads` is a hand-written single-pass parser that builds Python objects directly. It keeps a
  cache of dict keys, sizes lists exactly, parses numbers 8 digits at a time with correctly
  rounded floats, and uses SIMD for strings, escapes and whitespace.
- `dumps` writes straight into the final `bytes`/`str` object. It dispatches on exact types
  with no reference-count traffic, uses AVX-512/AVX2/SSE2 escape kernels, and zmij/itoap
  number formatting.
- A few speedups rely on non-public CPython internals. Each is restricted to the versions it
  was checked against, and the layout reads self-test at import (listed in `CLAUDE.md`).

Details: [docs/PERFORMANCE_REVIEW.md](docs/PERFORMANCE_REVIEW.md).

## Development

```bash
uv venv .venv -p 3.13 && . .venv/bin/activate
uv pip install maturin orjson pytest
maturin develop --release && python -m pytest tests -q
```

| path | contents |
|---|---|
| `src/parser.rs`, `src/lemire.rs` | `loads` |
| `src/ser.rs` | `dumps` / `dumps_str` |
| `src/entry.rs`, `src/compat.rs` | C-API entry points, version-portable helpers |
| `tests/` | pytest suites |
| `benches/` | `corpus_benchmark.py` (reference benchmark), `make_charts.py`, `perf_gate.py` |
| `scripts/` | PGO build, aarch64 test under qemu |
| `.github/workflows/` | CI, PGO release wheels, opt-in performance gate |

### Troubleshooting

- **Linker errors** (`symbol(s) not found for architecture arm64`): your Python and Rust
  toolchains must target the same architecture. Check with
  `python3 -c "import platform; print(platform.machine())"`; on Apple Silicon use an arm64 Python
  such as `/opt/homebrew/bin/python3`, then `cargo clean` and rebuild.
- **Missing Python headers:** install your Python's development package (e.g. `python3-dev`).

## Planned

- Options: indentation, sorted keys, `default=` hook
- datetime, UUID, dataclass and numpy serialization
- NEON kernels for aarch64
- Streaming parsing and serialization
