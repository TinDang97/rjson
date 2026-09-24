"""Head-to-head benchmark of rjson vs orjson vs stdlib json on standard corpora.

Corpora (not vendored; fetch into ``benches/data/`` or pass ``--data DIR``):
    twitter.json, citm_catalog.json, canada.json   (serde-rs/json-benchmark)
    github.json                                    (ijl/orjson data/github.json.xz)

Plus synthetic cases for per-call overhead and string/number heavy payloads.

Usage:
    python benches/corpus_benchmark.py [--data DIR] [--repeat N] [--only NAME]

Reports the median time per call and the ratio rjson/orjson (< 1.0 means
rjson is faster). For dumps, ``rjson`` is ``rjson.dumps`` (-> str) and
``rjson_b`` is ``rjson.dumps_bytes`` (-> bytes, the same type orjson returns).
"""

import argparse
import json
import os
import statistics
import sys
import time

import orjson
import rjson

HERE = os.path.dirname(os.path.abspath(__file__))


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
    }


def load_corpora(data_dir):
    out = {}
    for name in ("twitter", "citm_catalog", "canada", "github"):
        path = os.path.join(data_dir, name + ".json")
        if os.path.exists(path):
            with open(path, "rb") as f:
                out[name] = f.read().decode("utf-8")
    return out


def bench(fn, arg, repeat, min_time=0.2):
    # calibrate inner loop count so each sample takes ~min_time/repeat
    n = 1
    while True:
        t0 = time.perf_counter()
        for _ in range(n):
            fn(arg)
        dt = time.perf_counter() - t0
        if dt > min_time / 5:
            break
        n *= 2
    samples = []
    for _ in range(repeat):
        t0 = time.perf_counter()
        for _ in range(n):
            fn(arg)
        samples.append((time.perf_counter() - t0) / n)
    return statistics.median(samples)


def fmt(t):
    if t < 1e-3:
        return f"{t * 1e6:8.2f}us"
    return f"{t * 1e3:8.2f}ms"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.environ.get("RJSON_BENCH_DATA", os.path.join(HERE, "data")))
    ap.add_argument("--repeat", type=int, default=7)
    ap.add_argument("--only")
    ap.add_argument("--json", action="store_true", help="also time stdlib json")
    args = ap.parse_args()

    cases = {}  # name -> (python object, json text)
    for name, text in load_corpora(args.data).items():
        cases[name] = (json.loads(text), text)
    for name, obj in synthetic().items():
        cases[name] = (obj, json.dumps(obj, ensure_ascii=False))

    if not cases:
        sys.exit("no cases")

    libs = [("rjson", rjson.loads, rjson.dumps), ("orjson", orjson.loads, orjson.dumps)]
    if args.json:
        libs.append(("json", json.loads, json.dumps))
    dumps_bytes = getattr(rjson, "dumps_bytes", None)

    print(f"{'case':18} {'op':6} " + " ".join(f"{n:>10}" for n, _, _ in libs) + "   rjson/orjson")
    geo = {"loads": [], "dumps": [], "dumps_bytes": []}
    for name, (obj, text) in cases.items():
        if args.only and args.only not in name:
            continue
        for op in ("loads", "dumps"):
            times = []
            for _, lf, df in libs:
                if op == "loads":
                    times.append(bench(lf, text, args.repeat))
                else:
                    times.append(bench(df, obj, args.repeat))
            ratio = times[0] / times[1]
            geo[op].append(ratio)
            flag = "WIN " if ratio < 1 else "    "
            print(f"{name:18} {op:6} " + " ".join(f"{fmt(t):>10}" for t in times) + f"   {ratio:5.2f}x {flag}")
            if op == "dumps" and dumps_bytes is not None:
                tb = bench(dumps_bytes, obj, args.repeat)
                rb = tb / times[1]
                geo["dumps_bytes"].append(rb)
                flag = "WIN " if rb < 1 else "    "
                print(f"{name:18} {'bytes':6} {fmt(tb):>10} {'':>10}" + " " * (11 * (len(libs) - 2)) + f"   {rb:5.2f}x {flag}")
    for op, rs in geo.items():
        if rs:
            print(f"geomean rjson/orjson {op}: {statistics.geometric_mean(rs):.2f}x")


if __name__ == "__main__":
    main()
