#!/usr/bin/env bash
# Download the benchmark corpora into DIR (default benches/data), verifying
# pinned sha256 digests, for benches/corpus_benchmark.py (RJSON_BENCH_DATA=DIR).
#
#   benches/fetch_corpus.sh [DIR]
set -euo pipefail

DIR=${1:-$(cd "$(dirname "$0")" && pwd)/data}
mkdir -p "$DIR"
SERDE=https://raw.githubusercontent.com/serde-rs/json-benchmark/master/data
ORJSON=https://raw.githubusercontent.com/ijl/orjson/master/data

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

fetch() {  # url file sha256
    local url=$1 out=$DIR/$2 want=$3
    if [[ ! -f "$out" || "$(sha256 "$out")" != "$want" ]]; then
        curl -sSfL --retry 3 -o "$out.tmp" "$url"
        mv "$out.tmp" "$out"
    fi
    local got
    got=$(sha256 "$out")
    if [[ "$got" != "$want" ]]; then
        echo "sha256 mismatch for $2: got $got, want $want" >&2
        exit 1
    fi
}

fetch "$SERDE/twitter.json" twitter.json a08b769f32b95f426cbc3abafcec65c1a19d3eb544d4ddf320eae142c99efc5d
fetch "$SERDE/citm_catalog.json" citm_catalog.json a73e7a883f6ea8de113dff59702975e60119b4b58d451d518a929f31c92e2059
fetch "$SERDE/canada.json" canada.json f83b3b354030d5dd58740c68ac4fecef64cb730a0d12a90362a7f23077f50d78
fetch "$ORJSON/github.json.xz" github.json.xz 6f3c83af7cac5b0159ee4eafb4148efec4af730bda256b3fc7d619bb058bfd76
xz -dc "$DIR/github.json.xz" > "$DIR/github.json"
if [[ "$(sha256 "$DIR/github.json")" != 275e688d9081f1528483eccc7d87290957a67768d4413817e30e092a68cb2124 ]]; then
    echo "sha256 mismatch for github.json" >&2
    exit 1
fi
echo "corpora in $DIR"
