#!/usr/bin/env bash
# Build a PGO-optimized release wheel.
#
#   scripts/build_pgo.sh [python-interpreter]
#
# 1. build an instrumented wheel (-Cprofile-generate) and install it in a
#    throwaway venv
# 2. run scripts/pgo_train.py (corpus + synthetic workload)
# 3. merge the .profraw files with the toolchain's llvm-profdata
# 4. build the final wheel with -Cprofile-use  -> target/wheels/
#
# Measured on the corpus benchmark (x86_64, CPython 3.11): -9% loads time,
# -7% dumps time vs a plain release build.
#
# Requires: maturin, uv, `rustup component add llvm-tools-preview`.
# `--target` is passed explicitly so RUSTFLAGS only instrument the cdylib,
# not build scripts / proc-macros.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"  # maturin builds the crate in the current directory
PY=${1:-python3}
TARGET=$(rustc -vV | sed -n 's/^host: //p')
PROFDATA=$(find "$(rustc --print sysroot)" -name llvm-profdata -type f | head -1)
if [[ -z "$PROFDATA" ]]; then
    echo "llvm-profdata not found: rustup component add llvm-tools-preview" >&2
    exit 1
fi
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
BASE_FLAGS=${RUSTFLAGS:-}

echo "==> instrumented build"
RUSTFLAGS="$BASE_FLAGS -Cprofile-generate=$WORK/profraw" \
    maturin build --release --target "$TARGET" -i "$PY" -o "$WORK/wheels-gen" >/dev/null

echo "==> training"
uv venv -q -p "$PY" "$WORK/venv"
uv pip install -q --python "$WORK/venv/bin/python" "$WORK"/wheels-gen/*.whl
"$WORK/venv/bin/python" "$ROOT/scripts/pgo_train.py"

echo "==> merge profiles"
"$PROFDATA" merge -o "$WORK/merged.profdata" "$WORK/profraw"

echo "==> optimized build"
RUSTFLAGS="$BASE_FLAGS -Cprofile-use=$WORK/merged.profdata -Cllvm-args=-pgo-warn-missing-function=false" \
    maturin build --release --target "$TARGET" -i "$PY" -o "$ROOT/target/wheels"
ls -la "$ROOT"/target/wheels/
