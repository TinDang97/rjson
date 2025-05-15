---
applyTo: '**/*.py'
---
# Senior Python Engineer Coding Standards

## General Approach
- Write code as a senior Python engineer would: prioritize readability, maintainability, and efficiency
- Use Python 3.11+ syntax and features when appropriate
- Follow a systematic problem-solving approach: understand requirements, plan implementation, code, test, and refactor
- Before coding, outline a clear strategy considering edge cases, performance implications, and maintainability

## Code Style & Organization
- Follow PEP 8 style guidelines consistently
- Use meaningful variable/function names that convey purpose
- Organize code into logical modules and packages
- Limit function/method size to maintain readability (typically under 50 lines)
- Use appropriate design patterns where they simplify the code

## Documentation & Type Hints
- Write clear, concise docstrings for all public functions, classes, and modules
- Use type hints consistently to improve code understanding and static analysis
- Include examples in docstrings for complex functions
- Add inline comments only for non-obvious code sections

## Best Practices
- Prefer built-in functions and standard library modules over third-party packages for simple tasks
- Write idiomatic Python (e.g., use list comprehensions, generators where appropriate)
- Handle errors gracefully using try/except with specific exception types
- Use context managers for resource management
- Implement appropriate logging rather than print statements
- Write modular, reusable, and testable code
- Practice defensive programming for robust error handling

## Performance & Security
- Avoid premature optimization; focus on correct and clear code first
- Be mindful of performance implications for large datasets
- Use appropriate data structures for the problem at hand
- Validate all inputs, especially in public-facing interfaces
- Follow security best practices when handling sensitive data