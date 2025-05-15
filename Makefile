\
.PHONY: build dev test rust-test python-test

# Build the release wheel using Maturin
# This creates a distributable wheel in target/wheels/
build:
	@echo "Building Rust extension and Python wheel (release mode)..."
	maturin build --release
	@echo "Build complete. Wheel file created in target/wheels/"

# Set up the development environment using Maturin
# This command compiles the Rust extension and installs the Python package
# in editable mode in the current Python environment.
# Make sure your virtual environment is activated and maturin is installed.
dev:
	@echo "Setting up development environment (installing in editable mode)..."
	maturin develop
	@echo "Development environment ready. The package is installed in editable mode."
	@echo "Ensure you have development tools like pytest, black, flake8 installed if needed."
	@echo "For example: 'pip install pytest black flake8' or 'uv pip install pytest black flake8'"

# Run all tests
test: rust-test python-test

# Run Rust tests
rust-test:
	@echo "Running Rust tests..."
	cargo test

# Run Python tests
# Assumes pytest is used and tests are located in the 'tests/' directory.
python-test:
	@echo "Running Python tests..."
	@echo "Make sure pytest is installed (e.g., 'pip install pytest' or 'uv pip install pytest')"
	python -m pytest tests/

# Run Python benchmarks
pybench: build
	$(PYTHON_INTERPRETER) benches/python_benchmark.py

# Run benchmarks
bench: build
	cargo bench --all-features
	$(MAKE) pybench

# Consider adding other useful targets like 'lint', 'format', or 'clean' as needed.
# Example 'clean' target:
clean:
	@echo "Cleaning up Rust build artifacts..."
	cargo clean
	@echo "Removing Python build artifacts..."
	@rm -rf target/wheels
	@rm -rf target/rjson_*.so target/rjson_*.pyd # Platform-dependent shared library names
	@rm -rf python/rjson.egg-info build dist *.egg-info .pytest_cache
	@find . -path ./python/.venv -prune -o -name "__pycache__" -type d -exec rm -rf {} +
	@find . -path ./python/.venv -prune -o -name "*.pyc" -delete

activate:
	uv venv