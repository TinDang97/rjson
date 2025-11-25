# CLAUDE.md - AI Assistant Guide for rjson

## Project Overview

**rjson** is a high-performance JSON parsing library for Python, backed by Rust. It provides a Pythonic API (`loads` and `dumps`) with core functionality implemented in Rust for speed and safety.

### Status
- **Experimental**: APIs and behavior may change without warning
- Core serialization/deserialization stable for basic types
- Advanced features (custom encoders, streaming, etc.) not yet implemented
- Limited error handling; edge cases may not be fully covered

### Performance Profile
- Faster than Python's stdlib `json` module (~3x for dumps, ~1.4x for loads)
- Slower than `orjson` (~2.75x for dumps, ~1.71x for loads)
- Performance gap due to PyO3 and GIL overhead vs orjson's raw buffer/SIMD techniques
- Prioritizes safety, maintainability, and idiomatic Rust over maximum performance

## Technology Stack

### Core Technologies
- **Rust** (2021 edition): Core implementation with strong typing and memory safety
- **Python** (3.7+): User-facing API and bindings
- **PyO3** (v0.24.0): Python bindings for Rust
- **Maturin** (1.0+): Build system for Python extensions written in Rust
- **serde/serde_json** (v1.0): Rust JSON serialization framework

### Development Tools
- **uv**: Python package/environment manager
- **cargo**: Rust package manager and build tool
- **pytest**: Python testing framework
- **rustfmt/clippy**: Rust code formatting and linting

## Repository Structure

```
/home/user/rjson/
├── src/
│   └── lib.rs              # Main Rust implementation (368 lines)
├── benches/
│   ├── python_benchmark.py # Performance benchmarks vs orjson/json
│   └── large_benchmark.py  # Additional benchmarks
├── scripts/
│   └── run_tests.sh        # Test execution script
├── .github/
│   └── instructions/
│       ├── leader.instructions.md        # Project-wide coding standards
│       └── rust-engineer.instructions.md # Rust-specific guidelines
├── .vscode/
│   └── settings.json       # VSCode pytest configuration
├── Cargo.toml              # Rust package manifest
├── pyproject.toml          # Python project configuration
├── Makefile                # Build and development commands
├── README.md               # User-facing documentation
└── uv.lock                 # Python dependency lock file
```

### Missing Directories (Referenced but Not Present)
- `/tests/`: No test directory exists yet (pytest configured in .vscode/settings.json)
- `/examples/`: No examples directory
- `/docs/`: No documentation directory
- `/python/rjson/`: No separate Python interface code (all in Rust via PyO3)

## Architecture

### High-Level Design
1. **Rust Core** (`src/lib.rs`): Single-file implementation containing:
   - PyO3 module definition
   - Direct Python-to-Rust type conversion
   - Custom serde Visitor pattern for efficient deserialization
   - Custom Serialize implementation for Python objects

2. **Python API**: Exposed directly via PyO3 `#[pyfunction]` macros:
   - `loads(json_str: str) -> PyObject`: Parse JSON string to Python object
   - `dumps(data: PyAny) -> str`: Serialize Python object to JSON string

### Key Implementation Details

#### Deserialization (loads)
- Uses custom `PyObjectVisitor` implementing serde's `Visitor` trait
- Constructs Python objects directly during JSON parsing (no intermediate representation)
- Leverages `DeserializeSeed` pattern for efficient streaming
- Preallocates collections when size hints available

#### Serialization (dumps)
- Uses `PyAnySerialize` wrapper implementing serde's `Serialize` trait
- Direct conversion from Python types to JSON without intermediate serde_json::Value
- Supports: dict, list, tuple (as array), str, int, float, bool, None
- Requires dictionary keys to be strings

#### Type Handling
Supported Python → JSON mappings:
- `None` → `null`
- `bool` → `true`/`false`
- `int` → JSON number (handles i64, u64, with fallback for large ints)
- `float` → JSON number (rejects NaN/Infinity)
- `str` → JSON string
- `list`/`tuple` → JSON array
- `dict` → JSON object (keys must be strings)

#### Unused Code
Two helper functions exist but are marked `#[allow(dead_code)]`:
- `serde_value_to_py_object`: Converts serde_json::Value to PyObject
- `py_object_to_serde_value`: Converts PyObject to serde_json::Value

These use intermediate serde_json::Value representation and are retained for potential future use.

## Development Workflows

### Setup and Installation

```bash
# Ensure Rust and Python 3.7+ installed (architecture must match)
# Check Python architecture (should show arm64 on Apple Silicon)
python3 -c "import sys, platform; print(platform.machine())"

# Install Maturin
pip install maturin
# or with uv
uv pip install maturin

# Build and install in development mode
maturin develop --release --interpreter $(which python3)

# Or build wheel for distribution
maturin build --release
```

### Common Commands (via Makefile)

```bash
make dev          # Set up dev environment (maturin develop)
make build        # Build release wheel → target/wheels/
make test         # Run all tests (Rust + Python)
make rust-test    # Run cargo test
make python-test  # Run pytest tests/
make bench        # Run benchmarks (cargo bench + python benchmark)
make pybench      # Run Python benchmarks only
make clean        # Remove all build artifacts
make activate     # Create virtual environment with uv
```

### Testing Strategy

#### Current State
- No test files exist in repository
- pytest configured in .vscode/settings.json
- Makefile has python-test target expecting tests/ directory

#### Expected Test Organization
- Unit tests: `cargo test` for Rust
- Integration tests: pytest for Python (tests/ directory)
- Benchmarks: benches/python_benchmark.py

#### Testing Guidelines
- Test Python/Rust boundary conversions
- Test edge cases: empty collections, None, large integers
- Test error handling: invalid JSON, unsupported types, NaN/Infinity
- Benchmark performance-critical paths

### Building and Packaging

#### Development Build
```bash
maturin develop  # Faster, includes debug symbols
```

#### Release Build
```bash
maturin develop --release  # Optimized build
maturin build --release    # Create distributable wheel
```

#### Build Configuration (Cargo.toml)
```toml
[profile.release]
debug = true       # Debug symbols for profiling
lto = true         # Link-time optimization
codegen-units = 1  # Slower compilation, faster runtime
```

## Code Conventions and Standards

### Rust Code Style (from rust-engineer.instructions.md)

#### Safety and Error Handling
- Prefer `Result` over `panic`/`unwrap` in public APIs
- Use `?` operator for error propagation
- Handle all error cases explicitly
- Convert Rust errors to PyValueError for Python boundary

#### Performance
- Minimize heap allocations
- Use iterators, avoid unnecessary collecting
- Preallocate Vec with capacity when size known
- Profile before optimizing

#### Memory Management
- Leverage ownership system; avoid unnecessary Rc/Arc
- Minimize unsafe code (none currently in codebase)
- Prefer references over smart pointers

#### Documentation
- Document all public APIs with rustdoc
- Include examples for complex functionality
- Add comments for non-obvious implementations

### Python Standards (from leader.instructions.md)
- Follow PEP 8 style guidelines
- Use type hints (PEP 484)
- Format with Black, check with flake8
- Google-style docstrings
- Target Python 3.7+ compatibility

### Integration Guidelines
- Create Pythonic APIs that hide Rust implementation
- Handle memory management carefully at language boundaries
- Convert between Rust and Python types safely
- Propagate errors appropriately (PyValueError)

## Key Files Reference

### Cargo.toml
- Package: `rjson` v0.1.0, edition 2021
- Crate type: `cdylib` (C dynamic library for Python)
- Dependencies: pyo3, serde, serde_json
- Release profile: LTO enabled, single codegen unit

### pyproject.toml
- Build system: Maturin
- Python: >=3.7
- Compatibility: linux (Maturin config)
- Dev dependencies: maturin, orjson (for benchmarks)

### src/lib.rs (368 lines)
Single comprehensive file containing:
- Lines 1-141: Type conversion utilities (currently unused)
- Lines 143-224: PyObjectVisitor for efficient deserialization
- Lines 226-249: DeserializeSeed implementation
- Lines 251-268: `loads` function
- Lines 270-334: PyAnySerialize for efficient serialization
- Lines 336-351: `dumps` function
- Lines 353-367: PyModule definition

### benches/python_benchmark.py
- Compares rjson vs orjson vs stdlib json
- Tests large_array (100k items), large_object (10k keys), nested structures
- Runs 100 repetitions via timeit
- Outputs comparative performance metrics

## Performance Considerations

### Bottlenecks
1. **PyO3 Overhead**: Every Python object creation/access requires GIL
2. **Type Checking**: Downcast operations for each Python object
3. **No SIMD**: Unlike orjson, no low-level SIMD optimizations
4. **No Raw Buffers**: Uses standard serde_json instead of custom parser

### Optimization Opportunities (Future)
- Batch Python object creation
- Reduce GIL acquisition/release cycles
- Custom JSON parser with SIMD
- Direct buffer manipulation
- Memory pooling for allocations

### Current Optimizations
- Preallocate Vec with size_hint()
- Direct visitor pattern (no intermediate Value)
- Batch dict key/value collection
- Release profile with LTO and single codegen unit

## Common Tasks for AI Assistants

### Adding Support for New Python Types

1. Update `PyAnySerialize::serialize` (line 278-333)
2. Update `PyObjectVisitor` if needed for loads
3. Add tests for new type
4. Update documentation
5. Consider JSON spec compatibility

### Improving Performance

1. Profile first: Use Rust profiling tools
2. Identify hotspots in visitor/serialize code
3. Minimize Python object allocations
4. Benchmark changes with benches/python_benchmark.py
5. Update README with new benchmark results

### Adding Tests

1. Create `tests/` directory
2. Add pytest tests for Python API
3. Add Rust unit tests with `#[cfg(test)]`
4. Test error cases and edge conditions
5. Run `make test` to verify

### Fixing Bugs

1. Read src/lib.rs to understand implementation
2. Check type conversion in PyAnySerialize and PyObjectVisitor
3. Verify error handling propagates correctly
4. Add regression test
5. Update documentation if behavior changes

### Updating Dependencies

1. Update Cargo.toml for Rust dependencies
2. Update pyproject.toml for Python dependencies
3. Run `cargo update` and test
4. Check for breaking changes in PyO3 API
5. Update code if needed for new PyO3 patterns

## Troubleshooting

### Linker Errors (Architecture Mismatch)
**Symptom**: `ld: symbol(s) not found for architecture arm64`

**Solution**:
```bash
# Verify Python architecture
python3 -c "import platform; print(platform.machine())"

# On Apple Silicon, use Homebrew Python
/opt/homebrew/bin/python3 -m pip install maturin
maturin develop --release --interpreter /opt/homebrew/bin/python3

# Clean rebuild if switching Python
cargo clean
```

### Import Errors After Build
**Symptom**: `ImportError: No module named 'rjson'`

**Solution**:
```bash
# Ensure built in correct environment
maturin develop --release
# Or install wheel directly
pip install target/wheels/rjson-*.whl
```

### Performance Regression
**Symptom**: Slower than expected after changes

**Solution**:
```bash
# Ensure release build
maturin develop --release

# Run benchmarks
python benches/python_benchmark.py

# Profile Rust code
cargo build --release
# Use perf, flamegraph, or similar tools
```

### Type Conversion Errors
**Symptom**: `PyValueError: Unsupported Python type`

**Solution**:
- Check src/lib.rs lines 278-333 (PyAnySerialize) for supported types
- Verify input data contains only: dict, list, tuple, str, int, float, bool, None
- Check dict keys are strings
- Check floats are not NaN/Infinity

## Git Workflow

### Branch Naming
- Feature branches: `claude/claude-md-*` (for Claude Code sessions)
- Follow the pattern provided in session context

### Commit Guidelines
- Clear, descriptive commit messages
- Focus on "why" rather than "what"
- Run tests before committing
- Use `git add .` then commit with meaningful message

### Push Protocol
```bash
# Always use -u flag for new branches
git push -u origin <branch-name>

# Retry on network errors (up to 4 times with exponential backoff)
# 2s, 4s, 8s, 16s delays
```

## Planned Features (from README.md)

Future development areas:
- Custom encoders and decoders
- Streaming (incremental) parsing/serialization
- Improved error messages and diagnostics
- Type validation and schema support
- datetime and complex type support
- CLI tool for JSON processing
- Async API for non-blocking operations
- Extended benchmarking and profiling
- Documentation improvements

## AI Assistant Best Practices

### Before Making Changes
1. Read src/lib.rs completely (only 368 lines)
2. Understand PyO3 boundary and type conversion
3. Check existing error handling patterns
4. Review benchmark code to understand performance characteristics

### When Adding Features
1. Follow existing code patterns in lib.rs
2. Maintain PyO3 idioms (use Bound<'_, PyAny>, not PyObject directly)
3. Test both Rust and Python sides
4. Update benchmarks if performance-critical
5. Document public APIs

### When Fixing Bugs
1. Identify whether bug is in serialization (dumps) or deserialization (loads)
2. Check type handling in appropriate section
3. Add test case that reproduces bug
4. Verify fix doesn't break existing tests
5. Update error messages if applicable

### Code Quality
- Run `cargo clippy` for Rust linting
- Run `cargo fmt` for Rust formatting
- Follow Rust 2021 idioms
- Keep functions under 50 lines when possible
- Use meaningful variable names

### Documentation
- Update README.md for user-facing changes
- Update CLAUDE.md (this file) for structural changes
- Add rustdoc comments for new public functions
- Update benchmark results if performance changes

## Quick Reference

### File Locations
- Main code: `/home/user/rjson/src/lib.rs`
- Build config: `/home/user/rjson/Cargo.toml`
- Python config: `/home/user/rjson/pyproject.toml`
- Makefile: `/home/user/rjson/Makefile`
- Benchmarks: `/home/user/rjson/benches/python_benchmark.py`

### Important Line Numbers in lib.rs
- `loads` function: 260-268
- `dumps` function: 347-351
- PyObjectVisitor (deserialization): 147-224
- PyAnySerialize (serialization): 274-334
- Module definition: 361-367

### Build Commands
```bash
maturin develop --release   # Dev install
maturin build --release     # Build wheel
cargo test                  # Rust tests
pytest tests/               # Python tests (when added)
python benches/python_benchmark.py  # Benchmark
```

### Current Python Version
- Target: 3.7+
- Development: 3.13 (per .python-version)

---

**Document Version**: 1.0
**Last Updated**: 2025-11-23
**Repository**: /home/user/rjson
**Branch Pattern**: claude/claude-md-*
