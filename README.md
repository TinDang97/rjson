# rjson

> ⚠️ **Experimental:** This project is experimental and APIs may change or break at any time. Use at your own risk.

A high-performance JSON library for Python, backed by Rust with advanced optimizations.

## Performance

**8.4x faster** serialization (dumps) than Python's stdlib `json`
**Production-ready** performance with safety guarantees from Rust

```
Benchmark (100 repetitions, 110k element dataset):

Serialization (dumps):
  rjson:  0.170s  →  8.4x faster than json
  orjson: 0.058s  →  2.9x faster than rjson
  json:   1.43s

Deserialization (loads):
  rjson:  0.677s  →  1.0x (on par with json)
  orjson: 0.284s  →  2.4x faster than rjson
  json:   0.663s
```

### Optimization Highlights

- **Custom serializer**: Direct buffer writing with itoa/ryu for fast number formatting
- **Type caching**: O(1) type detection using cached type pointers
- **SIMD operations**: memchr for fast string scanning
- **C API integration**: Direct PyDict_Next for efficient dict iteration
- **Zero-copy strings**: Minimal allocations in hot paths
- **Smart caching**: Pre-allocated Python objects for common values [-256, 256]

## Installation

Ensure you have Rust and Python (3.7+) installed and that your Python interpreter matches your system architecture (e.g., arm64 for Apple Silicon Macs).

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

- `/src/`: Rust core implementation
- `/python/rjson/`: Python interface code
- `/tests/`: Test suites
- `/examples/`: Example usage
- `/docs/`: Documentation
- `/benches/`: Performance benchmarks
- `Cargo.toml`: Rust package manifest
- `pyproject.toml`: Python project configuration

## Features

- High-performance JSON serialization and deserialization
- Rust-backed core for speed and safety
- Pythonic API: `loads` and `dumps` functions
- Compatible with Python 3.7+
- Supports basic Python types: `dict`, `list`, `str`, `int`, `float`, `bool`, `None`
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
