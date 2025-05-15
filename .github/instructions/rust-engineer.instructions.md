---
applyTo: '**/*.rs'
---
# Senior Rust Engineering Guidelines

## Code Style and Organization
- Follow the official Rust style guide and idioms from rust-lang/rust
- Use snake_case for variables and functions, CamelCase for types and traits
- Organize code with clear module hierarchy and separation of concerns
- Keep functions focused and concise (ideally under 50 lines)
- Use meaningful variable and function names that convey intent

## Safety and Error Handling
- Prefer Result over panic/unwrap in public APIs
- Use `?` operator for clean error propagation
- Implement custom error types for libraries using thiserror or similar
- Handle all error cases explicitly; avoid ignoring errors
- Use `#[must_use]` for Results that shouldn't be ignored

## Performance
- Minimize heap allocations where possible
- Consider using Cow<T> for flexible ownership
- Use iterators and avoid unnecessary collecting
- Profile before optimizing; focus on measured bottlenecks
- Use appropriate data structures for the task (e.g., HashMap vs BTreeMap)

## Memory Management
- Leverage Rust's ownership system; avoid unnecessary Rc/Arc
- Minimize use of unsafe code; document and isolate when necessary
- Prefer references over smart pointers when appropriate
- Be explicit about lifetimes when necessary for clarity

## Documentation
- Document all public APIs with rustdoc comments
- Include examples in documentation for complex functionality
- Document SAFETY considerations for unsafe functions
- Add comments for complex algorithms or non-obvious implementations

## Testing
- Write unit tests for all public functions
- Include integration tests for major features
- Use property-based testing where appropriate (e.g., proptest)
- Test edge cases and error paths explicitly

## Dependencies
- Be conservative with dependencies; evaluate their maintenance status
- Prefer crates from the rust-lang org when available
- Consider MSRV (Minimum Supported Rust Version) requirements

## Async Code
- Use appropriate executor for the project (tokio, async-std, etc.)
- Follow structured concurrency patterns
- Be mindful of cancellation and resource cleanup
- Document blocking operations clearly

## General
- Prefer immutable variables (let vs let mut)
- Use strong typing over runtime checks
- Leverage the type system to prevent invalid states
- Follow API design principles from the Rust API Guidelines