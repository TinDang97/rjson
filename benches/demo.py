"""A 10-second head-to-head for the README demo: rjson vs orjson, same process.

    python benches/demo.py [--data benches/data]

Checks identical output, then times a few representative jobs: median over
interleaved rounds of orjson's time / rjson's time. benches/make_demo_svg.py turns a recorded run of this
script into the animated docs/img/demo.svg.
"""

import argparse
import datetime as dt
import os
import platform
import statistics
import time
import uuid

import orjson
import rjson

HERE = os.path.dirname(os.path.abspath(__file__))


def ratio(r, o, rounds=15, target=0.01):
    """Median over interleaved rounds of orjson time / rjson time."""
    n = 1
    while True:
        t0 = time.perf_counter()
        for _ in range(n):
            r()
        if time.perf_counter() - t0 > target:
            break
        n *= 2
    ratios = []
    for i in range(rounds):
        times = {}
        for label, fn in ((("r", r), ("o", o)) if i % 2 == 0 else (("o", o), ("r", r))):
            t0 = time.perf_counter()
            for _ in range(n):
                fn()
            times[label] = time.perf_counter() - t0
        ratios.append(times["o"] / times["r"])
    return statistics.median(ratios)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(HERE, "data"))
    args = ap.parse_args()
    with open(os.path.join(args.data, "twitter.json"), "rb") as f:
        twitter = f.read()
    tw = orjson.loads(twitter)
    base = dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc)
    events = [{"id": uuid.UUID(int=i), "at": base + dt.timedelta(seconds=i), "n": i} for i in range(2000)]

    with open(os.path.join(args.data, "github.json"), "rb") as f:
        gh = orjson.loads(f.read())
    cases = [
        ("twitter.json loads", lambda: rjson.loads(twitter), lambda: orjson.loads(twitter)),
        ("twitter.json dumps", lambda: rjson.dumps(tw), lambda: orjson.dumps(tw)),
        ("github.json dumps", lambda: rjson.dumps(gh), lambda: orjson.dumps(gh)),
        ("2k events, datetime+UUID", lambda: rjson.dumps(events), lambda: orjson.dumps(events)),
        ("twitter.json as str", lambda: rjson.dumps_str(tw), lambda: orjson.dumps(tw).decode()),
    ]
    print(f"rjson {rjson.__version__} vs orjson {orjson.__version__} · CPython {platform.python_version()}")
    for name, r, o in cases:
        same = r() == o()
        x = ratio(r, o)
        word = "faster" if x >= 1 else "slower"
        print(f"  {name:<26} {x:4.2f}x {word}   {'same output' if same else 'OUTPUT DIFFERS'}")


if __name__ == "__main__":
    main()
