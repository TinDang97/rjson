#!/usr/bin/env bash
# Build + test PGO wheels inside a PyPA manylinux/musllinux image (CI helper).
#
#   docker run --rm -v "$PWD":/io -w /io quay.io/pypa/musllinux_1_2_x86_64 \
#       scripts/ci_build_pgo_container.sh 3.9 3.10 3.11 3.12 3.13
#
# Installs a minimal Rust toolchain with llvm-tools, then runs
# scripts/build_pgo.sh for each /opt/python/cp3X-cp3X interpreter (wheels ->
# dist/) and the test suite against each optimized wheel.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if ! command -v cargo >/dev/null; then
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component llvm-tools
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

pyexe() {  # 3.12 -> /opt/python/cp312-cp312/bin/python
    local t=cp${1/./}
    echo "/opt/python/$t-$t/bin/python"
}

TOOLPY=$(pyexe 3.12)
"$TOOLPY" -m pip install -q "maturin==${MATURIN_VERSION:-1.15.0}"
export MATURIN
MATURIN=$(dirname "$TOOLPY")/maturin

PYS=()
for v in "$@"; do PYS+=("$(pyexe "$v")"); done
OUT_DIR="$ROOT/dist" scripts/build_pgo.sh "${PYS[@]}"

for py in "${PYS[@]}"; do
    venv=$(mktemp -d)
    "$py" -m venv "$venv"
    "$venv/bin/python" -m pip install -q pytest
    "$venv/bin/python" -m pip install -q --no-index --find-links "$ROOT/dist" rjson
    "$venv/bin/python" -m pytest tests -q -p no:cacheprovider
done
