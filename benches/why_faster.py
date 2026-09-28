"""Evidence for each design difference between rjson and orjson, measured
directly rather than only as end-to-end time.

    python benches/why_faster.py [--data benches/data] [--output-json out.json] [--no-valgrind]

Each experiment isolates one mechanism:

1. loads: one pass vs parse-to-tree-then-convert -> peak memory above the input and
   the result (fresh process per library, ru_maxrss).
2. dumps to str: written directly vs bytes + .decode() -> bytes allocated per call
   (tracemalloc peak) and time.
3. escaping: AVX2/AVX-512 kernels vs SSE2 -> throughput on escape-heavy text.
4. timezone-aware datetimes: cached offset vs attribute probes + utcoffset() per
   value -> time per datetime.
5. call overhead: instructions per call (valgrind; deterministic, independent of
   the host's noise), small documents.

Output must be identical for both libraries (checked first); every time is the
median of interleaved rounds in one process unless noted.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import textwrap
import time

import orjson
import rjson

HERE = os.path.dirname(os.path.abspath(__file__))
LIBS = ("rjson", "orjson")


def best_ratio(fns: dict[str, object], rounds: int = 15, target: float = 0.02) -> dict[str, float]:
    """Median time per call for each function, interleaved rounds."""
    n = 1
    first = next(iter(fns.values()))
    while True:
        t0 = time.perf_counter()
        for _ in range(n):
            first()  # type: ignore[operator]
        if time.perf_counter() - t0 > target:
            break
        n *= 2
    times: dict[str, list[float]] = {k: [] for k in fns}
    for i in range(rounds):
        order = list(fns.items()) if i % 2 == 0 else list(fns.items())[::-1]
        for name, fn in order:
            t0 = time.perf_counter()
            for _ in range(n):
                fn()  # type: ignore[operator]
            times[name].append((time.perf_counter() - t0) / n)
    return {k: statistics.median(v) for k, v in times.items()}


def fresh(code: str) -> dict[str, float]:
    """Run `code` in a new interpreter; it prints one JSON object."""
    out = subprocess.run([sys.executable, "-c", textwrap.dedent(code)], capture_output=True,
                         text=True, check=True)
    return json.loads(out.stdout.strip().splitlines()[-1])


def exp_loads_memory(big: str) -> dict[str, dict[str, float]]:
    code = """
        import resource, json, time
        lib = __import__("{lib}")
        data = open({big!r}, "rb").read()
        base = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        t0 = time.perf_counter()
        obj = lib.loads(data)
        t = time.perf_counter() - t0
        peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        print(json.dumps({{"peak_mb": (peak - base) / 1024, "time_s": t}}))
    """
    # Both build the same Python objects, so the difference in peak memory above
    # the input is what a library holds besides the result (orjson: the tree).
    return {lib: fresh(code.format(lib=lib, big=big)) for lib in LIBS}


def exp_dumps_str(obj: object) -> dict[str, dict[str, float]]:
    import tracemalloc

    fns = {"rjson": lambda: rjson.dumps_str(obj), "orjson": lambda: orjson.dumps(obj).decode()}
    assert fns["rjson"]() == fns["orjson"]()
    times = best_ratio(fns)
    out = {}
    for name, fn in fns.items():
        fn()
        tracemalloc.start()
        fn()
        _, peak = tracemalloc.get_traced_memory()
        tracemalloc.stop()
        out[name] = {"peak_alloc_kb": peak / 1024, "time_us": times[name] * 1e6}
    return out


def exp_escaping() -> dict[str, dict[str, float]]:
    # 1 MB of text with a quote, backslash or newline every 12 characters.
    chunk = 'abcdefghij"\\' + "klmnopqrst\n"
    text = chunk * (1_000_000 // len(chunk))
    doc = [text]
    fns = {"rjson": lambda: rjson.dumps(doc), "orjson": lambda: orjson.dumps(doc)}
    assert fns["rjson"]() == fns["orjson"]()
    times = best_ratio(fns)
    mb = len(text.encode()) / 1e6
    return {k: {"gb_per_s": mb / 1000 / v, "time_us": v * 1e6} for k, v in times.items()}


def exp_datetimes() -> dict[str, dict[str, float]]:
    base = dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc)
    events = [base + dt.timedelta(seconds=i) for i in range(10_000)]
    fns = {"rjson": lambda: rjson.dumps(events), "orjson": lambda: orjson.dumps(events)}
    assert fns["rjson"]() == fns["orjson"]()
    times = best_ratio(fns)
    return {k: {"ns_per_datetime": v / len(events) * 1e9} for k, v in times.items()}


def exp_instructions(twitter: str) -> dict[str, dict[str, float]]:
    """Instructions per call under valgrind: N calls minus 0 calls, divided by N."""
    if shutil.which("valgrind") is None:
        return {}
    code = """
        import sys, json
        lib = __import__(sys.argv[1]); n = int(sys.argv[2]); case = sys.argv[3]
        small = {"id": 1, "name": "rjson", "tags": ["fast", "safe"], "ok": True}
        small_b = b'{"id":1,"name":"rjson","tags":["fast","safe"],"ok":true}'
        tw_b = open(%r, "rb").read(); tw = json.loads(tw_b)
        f = {"dumps small dict": lambda: lib.dumps(small),
             "loads small dict": lambda: lib.loads(small_b),
             "dumps twitter.json": lambda: lib.dumps(tw),
             "loads twitter.json": lambda: lib.loads(tw_b)}[case]
        f()
        for _ in range(n):
            f()
    """ % twitter
    script = os.path.join(tempfile.mkdtemp(prefix="rjson-why-"), "run.py")
    with open(script, "w") as f:
        f.write(textwrap.dedent(code))
    cases = {"dumps small dict": 2000, "loads small dict": 2000,
             "dumps twitter.json": 10, "loads twitter.json": 10}

    def irefs(lib: str, n: int, case: str) -> int:
        out = subprocess.run(
            ["valgrind", "--tool=cachegrind", "--cache-sim=no", "--cachegrind-out-file=/dev/null",
             sys.executable, script, lib, str(n), case],
            capture_output=True, text=True, check=True)
        for line in out.stderr.splitlines():
            if "I" in line and "refs:" in line:
                return int(line.split(":")[1].replace(",", "").strip())
        raise RuntimeError(out.stderr)

    res: dict[str, dict[str, float]] = {}
    for case, n in cases.items():
        res[case] = {lib: (irefs(lib, n, case) - irefs(lib, 0, case)) / n for lib in LIBS}
    shutil.rmtree(os.path.dirname(script), ignore_errors=True)
    return res


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(HERE, "data"))
    ap.add_argument("--output-json")
    ap.add_argument("--no-valgrind", action="store_true")
    args = ap.parse_args()
    tw_path = os.path.join(args.data, "twitter.json")
    with open(tw_path, "rb") as f:
        twitter = orjson.loads(f.read())
    tmp = tempfile.mkdtemp(prefix="rjson-why-")
    big = os.path.join(tmp, "big.json")
    with open(big, "wb") as f:
        f.write(orjson.dumps([twitter] * 100))  # ~60 MB

    results: dict[str, object] = {
        "python": platform.python_version(),
        "rjson": rjson.__version__,
        "orjson": orjson.__version__,
    }
    print(f"rjson {rjson.__version__} vs orjson {orjson.__version__} · CPython "
          f"{platform.python_version()} · {platform.machine()}\n")

    r = exp_loads_memory(big)
    results["loads_memory"] = r
    print("1. loads, 60 MB file: peak memory above the input (fresh process)")
    for lib in LIBS:
        print(f"   {lib:7} {r[lib]['peak_mb']:7.0f} MB   {r[lib]['time_s'] * 1000:6.0f} ms")

    r = exp_dumps_str(twitter)
    results["dumps_str"] = r
    print("2. twitter.json to str: rjson.dumps_str vs orjson.dumps().decode()")
    for lib in LIBS:
        print(f"   {lib:7} {r[lib]['peak_alloc_kb']:7.0f} KB allocated   {r[lib]['time_us']:7.0f} µs")

    r = exp_escaping()
    results["escaping"] = r
    print("3. 1 MB string, an escape every 12 characters")
    for lib in LIBS:
        print(f"   {lib:7} {r[lib]['gb_per_s']:7.2f} GB/s")

    r = exp_datetimes()
    results["datetimes"] = r
    print("4. 10,000 UTC datetimes")
    for lib in LIBS:
        print(f"   {lib:7} {r[lib]['ns_per_datetime']:7.1f} ns per datetime")

    if not args.no_valgrind:
        r = exp_instructions(tw_path)
        results["instructions"] = r
        if r:
            print("5. instructions per call (valgrind)")
            for case, v in r.items():
                print(f"   {case:20} rjson {v['rjson']:>11,.0f}   orjson {v['orjson']:>11,.0f}"
                      f"   ({v['orjson'] / v['rjson']:.2f}x)")
    shutil.rmtree(tmp, ignore_errors=True)
    if args.output_json:
        with open(args.output_json, "w") as f:
            json.dump(results, f, indent=2)


if __name__ == "__main__":
    main()
