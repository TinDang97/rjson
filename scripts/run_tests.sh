#!/bin/sh
# Test script for the rjson project.
# This script executes all defined tests (Rust and Python)
# by invoking 'make test' from the project root.

# Exit immediately if a command exits with a non-zero status.
set -e

# Get the directory where the script is located.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Assume the project root is one level up from the 'scripts' directory.
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

echo "Changing to project root: ${PROJECT_ROOT}"
cd "${PROJECT_ROOT}"

echo "Running all project tests using 'make test'..."
make test

echo "All tests completed."
