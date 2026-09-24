"""PGO training workload for rjson (used by scripts/build_pgo.sh).

Exercises loads/dumps on the standard corpora (if present in
``$RJSON_BENCH_DATA`` or ``benches/data``) plus synthetic payloads covering
small documents, records, numbers, and ASCII / non-ASCII / escaped strings,
and touches the error paths once so they are laid out as cold.
"""

import json
import os
import sys
import time

import rjson

HERE = os.path.dirname(os.path.abspath(__file__))
DATA = os.environ.get("RJSON_BENCH_DATA", os.path.join(HERE, "..", "benches", "data"))
SECONDS_PER_CASE = float(os.environ.get("RJSON_PGO_SECONDS", "0.25"))


def synthetic():
    return {
        "small_dict": {"id": 123, "name": "alice", "active": True, "score": 9.5, "tags": ["a", "b"]},
        "unicode_strings": ["héllo wörld ünïcödé 日本語テキスト 😀 " * 4 for _ in range(2000)],
        "escaped_strings": ['line1\nline2\t"quoted"\\ back' * 3 for _ in range(2000)],
        "int_array": list(range(-50000, 50000)),
        "float_array": [i * 1.000001 for i in range(50000)],
        "records": [
            {"id": i, "name": f"user_{i}", "email": f"user{i}@example.com", "age": 20 + i % 50,
             "balance": i * 3.25, "active": i % 2 == 0, "roles": ["admin", "dev"], "meta": None}
            for i in range(5000)
        ],
        "scalars": [None, True, False, 0, -1, 2**63 - 1, 2**64 - 1, 2**70, 1.5e300, "", "x"],
    }


def cases():
    out = {}
    for name in ("twitter", "citm_catalog", "canada", "github"):
        path = os.path.join(DATA, name + ".json")
        if os.path.exists(path):
            with open(path, "rb") as f:
                text = f.read().decode("utf-8")
            out[name] = (json.loads(text), text)
    for name, obj in synthetic().items():
        out[name] = (obj, json.dumps(obj, ensure_ascii=False))
    return out


def main():
    all_cases = cases()
    print(f"pgo_train: {len(all_cases)} cases from {DATA}", file=sys.stderr)
    for obj, text in all_cases.values():
        for fn, arg in ((rjson.loads, text), (rjson.dumps, obj)):
            end = time.perf_counter() + SECONDS_PER_CASE
            while time.perf_counter() < end:
                fn(arg)
    for bad in ("[1,", '{"a" 1}', "nan", "[1] x", '"\\ud800', ""):
        try:
            rjson.loads(bad)
        except ValueError:
            pass
    for bad in (float("nan"), {1: 2}, object(), "\ud800"):
        try:
            rjson.dumps(bad)
        except (ValueError, TypeError, UnicodeEncodeError):
            pass


if __name__ == "__main__":
    main()
