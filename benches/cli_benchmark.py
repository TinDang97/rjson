"""The rjson command line vs ``python -m json.tool``, end to end (process start
included), on the benchmark corpora and on a large generated file.

    python benches/cli_benchmark.py [--data benches/data] [--runs 7] [--output-json out.json]

Each case checks that both tools print identical bytes first, then reports the
median wall time of fresh processes and json.tool's time / rjson's time.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def rjson_cmd() -> list[str]:
    exe = shutil.which("rjson", path=os.path.dirname(sys.executable))
    return [exe] if exe else [sys.executable, "-m", "rjson"]


def run(cmd: list[str], stdin: str | None, capture: bool) -> bytes:
    with open(stdin, "rb") if stdin else open(os.devnull, "rb") as f:
        out = subprocess.run(cmd, stdin=f, capture_output=capture, check=True,
                             stdout=None if capture else subprocess.DEVNULL)
    return out.stdout or b""


def timed(cmd: list[str], stdin: str | None, runs: int) -> float:
    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        run(cmd, stdin, capture=False)
        times.append(time.perf_counter() - t0)
    return statistics.median(times)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", default=os.path.join(HERE, "data"))
    ap.add_argument("--runs", type=int, default=7)
    ap.add_argument("--output-json")
    args = ap.parse_args()

    import rjson

    tmp = tempfile.mkdtemp(prefix="rjson-cli-bench-")
    with open(os.path.join(args.data, "twitter.json"), "rb") as f:
        twitter = json.loads(f.read())
    big = os.path.join(tmp, "big.json")  # ~60 MB: 100 copies of twitter.json
    with open(big, "w", encoding="utf-8") as f:
        json.dump([twitter] * 100, f, ensure_ascii=False)
    lines = os.path.join(tmp, "events.jsonl")  # 200k JSON Lines records
    with open(lines, "w", encoding="utf-8") as f:
        for i in range(200_000):
            f.write(json.dumps({"id": i, "user": f"u{i % 997}", "ok": i % 3 == 0,
                                "tags": ["a", "é"], "score": i * 0.5}) + "\n")

    cases = [
        ("twitter.json (0.6 MB)", [], os.path.join(args.data, "twitter.json")),
        ("citm_catalog.json (1.7 MB)", [], os.path.join(args.data, "citm_catalog.json")),
        ("canada.json (2.2 MB, floats)", [], os.path.join(args.data, "canada.json")),
        ("github.json (55 KB)", [], os.path.join(args.data, "github.json")),
        ("60 MB file, pretty-print", [], big),
        ("60 MB file, --sort-keys", ["--sort-keys"], big),
        ("60 MB file, --compact", ["--compact"], big),
        # JSON Lines from stdin: json.tool --json-lines FILE fails on CPython
        # 3.13+ ("I/O operation on closed file"; it reads after closing).
        ("200k JSON Lines, --compact", ["--json-lines", "--compact"], "<" + lines),
    ]
    json_tool = [sys.executable, "-m", "json.tool"]
    rj = rjson_cmd()
    print(f"rjson {rjson.__version__} CLI vs python -m json.tool · CPython "
          f"{platform.python_version()} · median of {args.runs} runs, process start included")
    print(f"{'case':32} {'json.tool':>10} {'rjson':>9} {'speedup':>8}")
    results = []
    for name, flags, path in cases:
        stdin = path[1:] if path.startswith("<") else None
        files = [] if stdin else [path]
        want = run([*json_tool, *flags, *files], stdin, capture=True)
        got = run([*rj, *flags, *files], stdin, capture=True)
        same = want == got
        t_json = timed([*json_tool, *flags, *files], stdin, args.runs)
        t_rjson = timed([*rj, *flags, *files], stdin, args.runs)
        results.append({"case": name, "json_tool_s": t_json, "rjson_s": t_rjson,
                        "speedup": t_json / t_rjson, "same_output": same})
        print(f"{name:32} {t_json * 1000:8.0f}ms {t_rjson * 1000:7.0f}ms {t_json / t_rjson:7.1f}x"
              f"{'' if same else '  OUTPUT DIFFERS'}")
    shutil.rmtree(tmp, ignore_errors=True)
    if args.output_json:
        with open(args.output_json, "w") as f:
            json.dump({"python": platform.python_version(), "rjson": rjson.__version__,
                       "results": results}, f, indent=2)


if __name__ == "__main__":
    main()
