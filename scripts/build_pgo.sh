#!/usr/bin/env bash
# Build PGO-optimized release wheels, one per interpreter.
#
#   scripts/build_pgo.sh [python-interpreter ...]        (default: python3)
#
# For each interpreter:
# 1. build an instrumented wheel (-Cprofile-generate) and install it in a
#    throwaway venv
# 2. run scripts/pgo_train.py (synthetic workload, no benchmark corpora)
# 3. merge the .profraw files with the toolchain's llvm-profdata
# 4. build the final wheel with -Cprofile-use  -> $OUT_DIR (target/wheels)
#
# Environment:
#   OUT_DIR       output directory for the optimized wheels (default target/wheels)
#   TARGET        Rust target triple (default: rustc host triple)
#   MATURIN_ARGS  extra `maturin build` arguments for both builds
#   MATURIN_FINAL_ARGS  extra arguments for the optimized build only, e.g.
#                 "--zig --compatibility manylinux2014". The instrumented build
#                 cannot link through zig (`__llvm_profile_runtime: unrecognized
#                 file extension`) and does not need to: with fat LTO rustc does
#                 all codegen, so the linker does not affect the profile.
#   MATURIN       maturin command (default: maturin)
#
# The PGO flags go into CARGO_TARGET_<TRIPLE>_RUSTFLAGS, not RUSTFLAGS: RUSTFLAGS
# replaces the rustflags from .cargo/config.toml (target-cpu=x86-64-v2 on
# x86_64), while the per-target env var is merged with them. Passing --target
# explicitly keeps the flags off build scripts and proc-macros.
#
# Requires: maturin, `rustup component add llvm-tools` (llvm-profdata); the
# interpreters need the stdlib `venv` + `ensurepip` modules. Works on Linux,
# macOS and Windows (Git Bash).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"  # maturin builds the crate in the current directory
if [[ $# -eq 0 ]]; then set -- python3; fi
MATURIN=${MATURIN:-maturin}
OUT_DIR=${OUT_DIR:-$ROOT/target/wheels}
TARGET=${TARGET:-$(rustc -vV | sed -n 's/^host: //p')}
read -r -a EXTRA <<< "${MATURIN_ARGS:-}"
read -r -a FINAL <<< "${MATURIN_FINAL_ARGS:-}"
FLAGS_VAR="CARGO_TARGET_$(echo "$TARGET" | tr 'a-z.-' 'A-Z__')_RUSTFLAGS"

if [[ -n "${RUSTFLAGS:-}" ]]; then
    echo "error: RUSTFLAGS is set; it would silently drop .cargo/config.toml's" >&2
    echo "       target rustflags (target-cpu=x86-64-v2). Put extra flags in $FLAGS_VAR." >&2
    exit 1
fi
BASE_FLAGS=${!FLAGS_VAR:-}

# Windows (Git Bash): rustc takes and prints native paths.
native() { if command -v cygpath >/dev/null; then cygpath -m "$1"; else echo "$1"; fi; }
posix() { if command -v cygpath >/dev/null; then cygpath -u "$1"; else echo "$1"; fi; }

PROFDATA=$(find "$(posix "$(rustc --print sysroot)")" -name 'llvm-profdata' -o -name 'llvm-profdata.exe' | head -1)
if [[ -z "$PROFDATA" ]]; then
    echo "llvm-profdata not found: rustup component add llvm-tools" >&2
    exit 1
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for PY in "$@"; do
    tag=$("$PY" -c 'import sys; print("cp%d%d" % sys.version_info[:2])')
    run="$WORK/$tag"
    mkdir -p "$run"
    echo "==> [$tag] instrumented build ($TARGET)"
    env "$FLAGS_VAR=$BASE_FLAGS -Cprofile-generate=$(native "$run/profraw")" \
        "$MATURIN" build --release --target "$TARGET" -i "$PY" -o "$run/wheels-gen" ${EXTRA[@]+"${EXTRA[@]}"}

    echo "==> [$tag] training"
    "$PY" -m venv "$run/venv"
    VPY="$run/venv/bin/python"
    [[ -x "$VPY" ]] || VPY="$run/venv/Scripts/python.exe"
    "$VPY" -m pip install -q --no-index --no-deps "$run"/wheels-gen/*.whl
    "$VPY" "$ROOT/scripts/pgo_train.py"

    echo "==> [$tag] merge profiles"
    "$PROFDATA" merge -o "$run/merged.profdata" "$run/profraw"

    echo "==> [$tag] optimized build"
    env "$FLAGS_VAR=$BASE_FLAGS -Cprofile-use=$(native "$run/merged.profdata") -Cllvm-args=-pgo-warn-missing-function=false" \
        "$MATURIN" build --release --target "$TARGET" -i "$PY" -o "$OUT_DIR" \
        ${EXTRA[@]+"${EXTRA[@]}"} ${FINAL[@]+"${FINAL[@]}"}
done
ls -la "$OUT_DIR"
