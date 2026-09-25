#!/usr/bin/env bash
# Cross-build for aarch64 Linux and run the test suite under qemu-user.
#
#   scripts/test_aarch64_qemu.sh [python-version ...]     (default: 3.12)
#
# One-time setup (Debian/Ubuntu x86_64 host):
#   rustup target add aarch64-unknown-linux-gnu
#   apt-get install qemu-user libc6-dev-arm64-cross   # qemu-aarch64 + aarch64 glibc
#   pip install maturin ziglang                       # zig is the cross linker
# CPython for aarch64 comes from `uv python install --no-bin` (the script does
# this; without --no-bin uv adds a ~/.local/bin/python3.X shim for the aarch64
# build that shadows the host interpreter).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
if [[ $# -eq 0 ]]; then set -- 3.12; fi
SYSROOT=${AARCH64_SYSROOT:-/usr/aarch64-linux-gnu}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

for v in "$@"; do
    tag=cp${v/./}
    uv python install -q --no-bin "cpython-$v-linux-aarch64-gnu"
    # (not `uv python find`: it would have to run the aarch64 binary)
    py=$(find "$(uv python dir)" -maxdepth 1 -name "cpython-$v.*-linux-aarch64-gnu" | sort | tail -1)/bin/python$v

    maturin build --release --target aarch64-unknown-linux-gnu --zig -i "python$v" -o "$WORK/wheels-$tag"
    uv pip install -q --target "$WORK/site-$tag" --python-platform aarch64-unknown-linux-gnu \
        --python-version "$v" pytest "$WORK/wheels-$tag"/*.whl
    echo "==> aarch64 CPython $v"
    PYTHONPATH="$WORK/site-$tag" qemu-aarch64 -L "$SYSROOT" "$py" -m pytest tests -q -p no:cacheprovider
done
