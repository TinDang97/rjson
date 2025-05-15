---
applyTo: '**/*.md'
---
# AI Coding Leader Instructions for Python-Rust Integration Project

## Project Overview
This project integrates Python and Rust, using Rust for performance-critical operations while providing Pythonic interfaces. The core functionality is implemented in Rust with Python bindings.

## Technology Stack
- **Rust**: Core implementation
- **Python**: User-facing API
- **PyO3**: Python bindings for Rust
- **Maturin**: Build system for Python extensions written in Rust

## Coding Standards

### Rust Standards
- Follow Rust idiomatic practices and official style guide
- Use 2021 edition of Rust
- Leverage strong typing and ownership system
- Use Result/Option for error handling, avoid panics
- Format code with `rustfmt` and lint with `clippy`
- Document all public APIs with rustdoc

### Python Standards
- Follow PEP 8 style guidelines
- Use type hints (PEP 484)
- Format with Black and check with flake8
- Write docstrings in Google style
- Target Python 3.7+ compatibility

## Integration Guidelines
- Create Pythonic APIs that hide Rust implementation details
- Handle memory management carefully at language boundaries
- Convert between Rust and Python types safely
- Propagate errors appropriately between languages

## Project Structure
- `/src/`: Rust implementation
- `/python/`: Python interface code
- `/tests/`: Test suites for both languages
- `/examples/`: Example usage
- `/docs/`: Documentation
- `/benches/`: Performance benchmarks

## Testing Requirements
- Unit tests for both languages
- Integration tests across language boundary
- Benchmark performance-critical code
- Test in CI pipeline

## Documentation
- Document APIs in both languages
- Include examples and usage patterns
- Explain performance characteristics
- Keep README updated with installation and usage instructions

## Performance Considerations
- Profile code to identify bottlenecks
- Minimize Python/Rust boundary crossings
- Consider parallelism for CPU-bound operations
- Benchmark critical paths