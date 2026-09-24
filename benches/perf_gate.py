"""Performance regression gate: compare two sets of corpus_benchmark runs.

Each input file is the ``--output-json`` output of ``benches/corpus_benchmark.py``.
Pass several runs per side (interleaved base/head runs in the same job) so the
comparison uses the per-case median ratio across runs::

    python benches/perf_gate.py --base b1.json b2.json b3.json \\
                                --head h1.json h2.json h3.json [--threshold 0.05]

For every op (loads, dumps, dumps_bytes) the geomean of the rjson/orjson ratio
is computed over the cases present on both sides. Comparing ratios (not raw
times) cancels most of the runner's speed drift, since orjson is timed in the
same process. Exits 1 if any op's geomean got more than ``--threshold`` worse
(default 5%). Writes a Markdown table to ``$GITHUB_STEP_SUMMARY`` when set.
"""

import argparse
import json
import os
import statistics
import sys


def load(paths):
    """{(case, op): [ratio per run]}"""
    out = {}
    for p in paths:
        with open(p) as f:
            data = json.load(f)
        for r in data["results"]:
            out.setdefault((r["case"], r["op"]), []).append(r["ratio"])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", nargs="+", required=True)
    ap.add_argument("--head", nargs="+", required=True)
    ap.add_argument("--threshold", type=float, default=0.05, help="max allowed geomean regression (0.05 = 5%%)")
    args = ap.parse_args()

    base, head = load(args.base), load(args.head)
    keys = sorted(set(base) & set(head))
    if not keys:
        sys.exit("perf_gate: no common (case, op) pairs between base and head")

    lines = [
        "| case | op | base rjson/orjson | head rjson/orjson | change |",
        "|---|---|---|---|---|",
    ]
    per_op = {}
    for case, op in keys:
        b = statistics.median(base[(case, op)])
        h = statistics.median(head[(case, op)])
        per_op.setdefault(op, []).append((b, h))
        lines.append(f"| {case} | {op} | {b:.3f} | {h:.3f} | {100 * (h / b - 1):+.1f}% |")

    lines += ["", "| op | base geomean | head geomean | change | status |", "|---|---|---|---|---|"]
    failed = []
    for op, pairs in per_op.items():
        gb = statistics.geometric_mean([b for b, _ in pairs])
        gh = statistics.geometric_mean([h for _, h in pairs])
        change = gh / gb - 1
        ok = change <= args.threshold
        if not ok:
            failed.append(op)
        lines.append(f"| {op} | {gb:.3f} | {gh:.3f} | {100 * change:+.1f}% | {'ok' if ok else 'REGRESSION'} |")

    runs = f"{len(args.base)} base / {len(args.head)} head runs, threshold {100 * args.threshold:.0f}%"
    report = "\n".join([f"### rjson perf gate ({runs})", ""] + lines) + "\n"
    print(report)
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write(report)
    if failed:
        print(f"perf_gate: geomean regression > {100 * args.threshold:.0f}% in: {', '.join(failed)}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
