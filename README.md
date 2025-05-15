# rjson

> ⚠️ **Experimental:** This project is experimental and APIs may change or break at any time. Use at your own risk.

A Python library for high-performance JSON parsing, backed by Rust.

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

## Benchmarking 

with 100 repetitions...

--- Serialization (dumps) ---
rjson.dumps:  0.324385 seconds
orjson.dumps: 0.091530 seconds
json.dumps:   0.351510 seconds

--- Deserialization (loads) ---
rjson.loads:  0.422533 seconds
orjson.loads: 0.168543 seconds
json.loads:   0.422147 seconds

--- Comparisons ---
orjson.dumps is 3.54x faster than rjson.dumps
rjson.dumps is 1.08x faster than json.dumps
orjson.dumps is 3.84x faster than json.dumps
orjson.loads is 2.51x faster than rjson.loads
json.loads is 1.00x faster than rjson.loads
orjson.loads is 2.50x faster than json.loads

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
