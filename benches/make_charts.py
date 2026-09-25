"""Render the README charts from a ``corpus_benchmark.py --json --output-json`` run.

Usage:
    python benches/corpus_benchmark.py --json --repeat 11 --output-json results.json
    python benches/make_charts.py results.json [--out docs/img]

Writes, each in a light and a dark variant (``<name>-light.svg`` / ``<name>-dark.svg``):
    headline   four stat tiles: geomean speedup vs orjson and vs stdlib json
    vs-orjson  per-case speed relative to orjson, loads and dumps side by side

Pure standard library; the SVGs use the system sans and fixed hex colors, so
they render the same in GitHub's <picture> light/dark switch.
"""

import argparse
import json
import os
import statistics
from xml.sax.saxutils import escape

# Display order: real-world corpora first, then synthetic cases.
CASES = [
    ("twitter", "twitter.json"),
    ("citm_catalog", "citm_catalog.json"),
    ("canada", "canada.json"),
    ("github", "github.json"),
    ("small_dict", "small dict"),
    ("records", "records"),
    ("unicode_strings", "unicode strings"),
    ("escaped_strings", "escaped strings"),
    ("int_array", "int array"),
    ("float_array", "float array"),
]

FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'

THEMES = {
    "light": {
        "surface": "#fcfcfb",
        "ring": "rgba(11,11,11,0.10)",
        "ink": "#0b0b0b",
        "ink2": "#52514e",
        "muted": "#6f6d68",
        "grid": "#e1e0d9",
        "axis": "#a3a29b",
        "accent": "#2a78d6",
    },
    "dark": {
        "surface": "#1a1a19",
        "ring": "rgba(255,255,255,0.10)",
        "ink": "#ffffff",
        "ink2": "#c3c2b7",
        "muted": "#9a988f",
        "grid": "#2c2c2a",
        "axis": "#5a5955",
        "accent": "#3987e5",
    },
}


def speedups(results, op, base):
    """{case: base_time / rjson_time} for one op."""
    out = {}
    for r in results:
        if r["op"] == op and base in r["times"]:
            out[r["case"]] = r["times"][base] / r["times"]["rjson"]
    return out


def geomean(d):
    return statistics.geometric_mean(d.values())


def fmt_x(v):
    return f"{v:.1f}×" if v >= 10 else f"{v:.2f}×"


def text(x, y, s, size, fill, weight=400, anchor="start", extra=""):
    return (
        f'<text x="{x:.1f}" y="{y:.1f}" font-family=\'{FONT}\' font-size="{size}" '
        f'font-weight="{weight}" fill="{fill}" text-anchor="{anchor}"{extra}>{escape(s)}</text>'
    )


def card(w, h, t):
    return (
        f'<rect x="0.5" y="0.5" width="{w - 1}" height="{h - 1}" rx="12" '
        f'fill="{t["surface"]}" stroke="{t["ring"]}"/>'
    )


def headline_svg(t, tiles, footnote):
    w, h = 880, 150
    pad, gap = 24, 0
    n = len(tiles)
    tw = (w - 2 * pad) / n
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" '
             f'aria-label="rjson speedups: ' + escape("; ".join(f"{l} {v}" for l, v, _ in tiles)) + '">',
             card(w, h, t)]
    for i, (label, value, sub) in enumerate(tiles):
        x = pad + i * tw + (16 if i else 0)
        if i:
            parts.append(f'<line x1="{pad + i * tw:.1f}" y1="26" x2="{pad + i * tw:.1f}" y2="{h - 46}" '
                         f'stroke="{t["grid"]}" stroke-width="1"/>')
        parts.append(f'<rect x="{x}" y="30" width="3" height="16" rx="1.5" fill="{t["accent"]}"/>')
        parts.append(text(x + 11, 43, label, 13, t["ink2"]))
        # the unit word trails the value in the same <text>, so it follows the
        # rendered width of the number in whatever sans the viewer has
        parts.append(
            f'<text x="{x:.1f}" y="94" font-family=\'{FONT}\' font-size="40" font-weight="600" '
            f'fill="{t["ink"]}">{escape(value)}<tspan dx="8" font-size="14" font-weight="400" '
            f'fill="{t["muted"]}">{escape(sub)}</tspan></text>'
        )
    parts.append(text(pad, h - 18, footnote, 12, t["muted"]))
    parts.append("</svg>")
    return "\n".join(parts)


def bar_path(x0, y, x1, hgt, r=4):
    """Horizontal bar: square at the baseline (x0), 4px rounded data end (x1)."""
    r = min(r, (x1 - x0) / 2, hgt / 2)
    return (f"M{x0:.1f},{y:.1f} H{x1 - r:.1f} Q{x1:.1f},{y:.1f} {x1:.1f},{y + r:.1f} "
            f"V{y + hgt - r:.1f} Q{x1:.1f},{y + hgt:.1f} {x1 - r:.1f},{y + hgt:.1f} H{x0:.1f} Z")


def vs_orjson_svg(t, panels, footnote):
    w = 880
    pad = 24
    label_w = 128
    panel_gap = 40
    row_h, bar_h = 28, 16
    top = 74
    n = len(CASES)
    h = top + n * row_h + 58
    pw = (w - 2 * pad - label_w - panel_gap) / 2
    vmax = max(max(p[1].values()) for p in panels)
    xmax = max(2.0, int(vmax * 2 + 0.999) / 2)  # round up to 0.5
    parts = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" role="img" '
             f'aria-label="rjson speed relative to orjson per benchmark case, loads and dumps">',
             card(w, h, t),
             text(pad, 34, "Speed relative to orjson, per case (higher is better)", 15, t["ink"], 600)]
    for r, (_, name) in enumerate(CASES):
        cy = top + r * row_h + row_h / 2
        parts.append(text(pad, cy + 4.5, name, 13, t["ink2"]))
    for i, (title, vals) in enumerate(panels):
        x0 = pad + label_w + i * (pw + panel_gap)
        sx = lambda v: x0 + v / xmax * (pw - 44)  # leave room for the tip label
        parts.append(text(x0, 60, title, 13, t["ink"], 600))
        # gridlines at every 0.5x, hairline; orjson reference at 1x in axis ink
        k = 0.0
        while k <= xmax + 1e-9:
            gx = sx(k)
            is_ref = abs(k - 1.0) < 1e-9
            parts.append(f'<line x1="{gx:.1f}" y1="{top - 4}" x2="{gx:.1f}" y2="{top + n * row_h + 2}" '
                         f'stroke="{t["axis"] if is_ref or k == 0 else t["grid"]}" stroke-width="1"/>')
            if k == int(k):
                parts.append(text(gx, top + n * row_h + 18, f"{k:.0f}×", 11, t["muted"], anchor="middle",
                                  extra=' style="font-variant-numeric: tabular-nums"'))
            k += 0.5
        parts.append(text(sx(1.0) + 4, top - 8, "orjson", 11, t["muted"]))
        for r, (key, _) in enumerate(CASES):
            v = vals.get(key)
            if v is None:
                continue
            y = top + r * row_h + (row_h - bar_h) / 2
            parts.append(f'<path d="{bar_path(sx(0), y, sx(v), bar_h)}" fill="{t["accent"]}"/>')
            parts.append(text(sx(v) + 6, y + bar_h / 2 + 4.5, fmt_x(v), 12, t["ink2"],
                              extra=' style="font-variant-numeric: tabular-nums"'))
    parts.append(text(pad, h - 16, footnote, 12, t["muted"]))
    parts.append("</svg>")
    return "\n".join(parts)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("--out", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "docs", "img"))
    args = ap.parse_args()
    with open(args.results) as f:
        data = json.load(f)
    res, meta = data["results"], data["meta"]

    loads_o, dumps_o = speedups(res, "loads", "orjson"), speedups(res, "dumps", "orjson")
    loads_j, dumps_j = speedups(res, "loads", "json"), speedups(res, "dumps", "json")
    if not loads_j:
        raise SystemExit("results lack stdlib json timings: rerun corpus_benchmark.py with --json")
    n = len(loads_o)
    tiles = [
        ("loads vs orjson", fmt_x(geomean(loads_o)), "faster"),
        ("dumps vs orjson", fmt_x(geomean(dumps_o)), "faster"),
        ("loads vs stdlib json", fmt_x(geomean(loads_j)), "faster"),
        ("dumps vs stdlib json", fmt_x(geomean(dumps_j)), "faster"),
    ]
    env = f"CPython {meta['python']} · orjson {meta['orjson']} · x86_64"
    head_note = f"Geometric mean over {n} cases (4 real-world corpora + 6 synthetic) · {env}"
    case_note = f"Bar = orjson time ÷ rjson time; dumps returns bytes for both · median per call · {env}"

    os.makedirs(args.out, exist_ok=True)
    for mode, t in THEMES.items():
        with open(os.path.join(args.out, f"headline-{mode}.svg"), "w") as f:
            f.write(headline_svg(t, tiles, head_note))
        with open(os.path.join(args.out, f"vs-orjson-{mode}.svg"), "w") as f:
            f.write(vs_orjson_svg(t, [("loads", loads_o), ("dumps", dumps_o)], case_note))

    # markdown table twin for the README (the charts' text equivalent)
    print("| case | loads vs orjson | dumps vs orjson | loads vs json | dumps vs json |")
    print("|---|---|---|---|---|")
    for key, name in CASES:
        if key in loads_o:
            print(f"| {name} | {fmt_x(loads_o[key])} | {fmt_x(dumps_o[key])} | {fmt_x(loads_j[key])} | {fmt_x(dumps_j[key])} |")
    print(f"| **geomean** | **{fmt_x(geomean(loads_o))}** | **{fmt_x(geomean(dumps_o))}** | "
          f"**{fmt_x(geomean(loads_j))}** | **{fmt_x(geomean(dumps_j))}** |")


if __name__ == "__main__":
    main()
