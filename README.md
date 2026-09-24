# rjson

**High-performance JSON library for Python, backed by Rust**

Fast JSON serialization/deserialization for Python, implemented in Rust against the CPython C API.

## Performance

rjson time ÷ orjson time on standard corpora (**< 1.00 = rjson faster**), geomean over
twitter, citm_catalog, canada, github and six synthetic cases. Full table, methodology and
remaining gaps: [docs/PERFORMANCE_REVIEW.md](docs/PERFORMANCE_REVIEW.md).

| | CPython 3.11 (PGO build) | CPython 3.13 (plain release build) |
|---|---|---|
| `loads` | **0.74x** | **0.98x** |
| `dumps_bytes` (→ `bytes`, like `orjson.dumps`) | **0.82x** | **0.71x** |
| `dumps` (→ `str`) | 1.00x | **0.83x** |

Largest gaps remaining: `dumps` → `str` on non-ASCII text (a `str` result must be built
in UCS2/UCS4), `loads` of escape-heavy strings and float arrays.

Reproduce with `python benches/corpus_benchmark.py` (see the review doc for the corpora).

## API

```python
import rjson

rjson.loads(data)        # data: str | bytes | bytearray | memoryview
rjson.dumps(obj)         # -> str   (compact, non-ASCII kept as-is)
rjson.dumps_bytes(obj)   # -> bytes (UTF-8; fastest, same type as orjson.dumps)
```

- Supported types: `dict` (str keys), `list`, `tuple`, `str`, `int` (arbitrary size),
  `float`, `bool`, `None`, and their subclasses.
- Errors: `json.JSONDecodeError` (a `ValueError`) from `loads`; `ValueError`/`TypeError`
  from `dumps` for unsupported types, non-str keys, NaN/Infinity, nesting deeper than
  254 (e.g. circular references).
- `loads` nesting limit: 1024. Integers beyond 64 bits are parsed exactly.

## Installation

Ensure you have Rust and Python (3.11–3.13 tested) installed and that your Python interpreter matches your system architecture (e.g., arm64 for Apple Silicon Macs).

1. **Install Maturin**:

   ```bash
   pip install maturin
   ```

2. **Build and install the package**:

   From the root of the project directory, run:

   ```bash
   maturin develop --release --interpreter $(which python3)
   ```

   > **Note:** If you are on Apple Silicon (arm64), ensure you are using the arm64 Python (e.g., `/opt/homebrew/bin/python3`).
   > If you encounter linker errors about missing Python symbols, see the troubleshooting section below.

   Or, to build a wheel for distribution:

   ```bash
   maturin build --release
   ```

## Usage

```python
from rjson import loads, dumps

def main():
    print("Hello from rjson!")
    dict_data = {'a': 1}
    dumps_data = dumps(dict_data)
    print(dumps_data)
    loads_data = loads(dumps_data)
    print(loads_data)
    assert loads_data == dict_data

if __name__ == "__main__":
    main()
```

## Troubleshooting

### Linker errors (e.g., `ld: symbol(s) not found for architecture arm64`)

- Ensure your Python and Rust toolchains are both for the same architecture (arm64 or x86_64).

- Check your Python version and architecture:

  ```bash
  python3 -c "import sys; print(sys.version); import platform; print(platform.machine())"
  ```
  Should print `arm64` for Apple Silicon.

- If using Homebrew Python, prefer `/opt/homebrew/bin/python3` on Apple Silicon.

- Clean and rebuild if you switch Python versions:

  ```bash
  cargo clean
  maturin develop --release --interpreter $(which python3)
  ```

- If issues persist, ensure Python development headers are installed (e.g., `brew install python`).

## Project Structure

- `/src/parser.rs`, `/src/lemire.rs`: `loads`
- `/src/ser.rs`: `dumps` / `dumps_bytes`
- `/src/entry.rs`: raw C-API entry points, module registration
- `/tests/`: pytest suites
- `/docs/`: performance review and roadmap
- `/benches/`: benchmarks (`corpus_benchmark.py` is the reference)
- `/scripts/build_pgo.sh`: PGO wheel build
- `Cargo.toml`: Rust package manifest
- `pyproject.toml`: Python project configuration

## Features

- High-performance JSON serialization and deserialization
- Rust-backed core for speed and safety
- Pythonic API: `loads` and `dumps` functions
- Tested on CPython 3.11, 3.12, 3.13
- Supports `dict`, `list`, `tuple`, `str`, `int`, `float`, `bool`, `None` and subclasses
- Simple installation with Maturin

## Status

- Experimental: APIs and behavior may change
- Core serialization/deserialization stable for basic types
- Advanced features (custom encoders, streaming, etc.) not yet implemented
- Limited error handling; edge cases may not be fully covered
- Seeking feedback and contributions

## Planned Features

- Support for custom encoders and decoders
- Streaming (incremental) parsing and serialization
- Improved error messages and diagnostics
- Optional type validation and schema support
- Support for datetime and other complex types
- CLI tool for quick JSON processing
- Async API for non-blocking operations
- Extended benchmarking and profiling tools
- Documentation improvements and usage examples
