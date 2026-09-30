"""Render the README's showcase chart and architecture diagram.

Usage:
    python benches/showcase.py --rounds 15 --fresh --output-json run.json   # x3, aggregated
    python benches/make_showcase_charts.py [docs/showcase-results.json] [--out docs/img]

Writes, each in a light and a dark variant (``<name>-light.svg`` / ``<name>-dark.svg``):
    showcase       speedup over orjson per workload, grouped, from showcase-results.json
    architecture   where rjson's design differs from orjson's (loads, dumps, native
                   types), annotated with the measured effect from the same results
and social-preview.svg (1280x640, dark only) for the repository's social preview; GitHub
wants a PNG: render it with a headless browser at 1280x640 (docs/img/social-preview.png).

Same palette and conventions as ``make_charts.py`` (pure standard library).
"""

import argparse
import json
import math
import os
import sys
from xml.sax.saxutils import escape

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from make_charts import FONT, THEMES, card, text  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.join(HERE, "..", "docs")

GROUPS = [
    ("native types", "Application types"),
    ("str output", "Output as str"),
    ("web api", "Web API"),
    ("strings", "Strings that need escaping"),
    ("corpus", "Standard corpora"),
]

# Colors the base palette lacks: the lane for orjson, and a bar below 1x.
EXTRA = {
    "light": {"other": "#8a8983", "box": "#f1f0ec", "box_rj": "#e8f0fb"},
    "dark": {"other": "#8c8a82", "box": "#242423", "box_rj": "#1d2a3a"},
}

NUM = ' style="font-variant-numeric: tabular-nums"'


def fmt_x(v):
    return f"{v:.2f}×"


def svg_open(w, h, label):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" '
            f'role="img" aria-label="{escape(label)}">')


# -- showcase -------------------------------------------------------------------------


def showcase_svg(t, x, rows, meta):
    w, pad = 880, 24
    label_w = 300
    row_h, bar_h, group_h = 24, 14, 30
    top = 70
    lo, hi = 0.8, 3.5
    plot_x0 = pad + label_w
    plot_w = w - plot_x0 - pad - 52
    sx = lambda v: plot_x0 + (math.log(v) - math.log(lo)) / (math.log(hi) - math.log(lo)) * plot_w

    grouped = [(title, sorted((r for r in rows if r["group"] == key), key=lambda r: -r["speedup"]))
               for key, title in GROUPS]
    n_rows = sum(len(g) for _, g in grouped)
    h = top + len(grouped) * group_h + n_rows * row_h + 72
    wins = sum(r.get("speedup_min", r["speedup"]) > 1 for r in rows)  # faster in every run
    geo = math.exp(sum(math.log(r["speedup"]) for r in rows) / len(rows))

    parts = [svg_open(w, h, f"rjson speedup over orjson on {len(rows)} workloads: faster in {wins}, "
                            f"geometric mean {geo:.2f} times"),
             card(w, h, t),
             text(pad, 34, f"Speed relative to orjson on production-shaped workloads (higher is better)",
                  15, t["ink"], 600),
             text(pad, 54, f"rjson faster in {wins} of {len(rows)} · geomean {fmt_x(geo)}", 13, t["ink2"])]

    body_top, body_bot = top, h - 60
    for k in (1, 1.5, 2, 3):
        gx = sx(k)
        parts.append(f'<line x1="{gx:.1f}" y1="{body_top}" x2="{gx:.1f}" y2="{body_bot}" '
                     f'stroke="{t["axis"] if k == 1 else t["grid"]}" stroke-width="1"/>')
        parts.append(text(gx, body_bot + 16, f"{k:g}×", 11, t["muted"], anchor="middle", extra=NUM))
    parts.append(text(sx(1) + 4, body_top - 4, "orjson", 11, t["muted"]))

    y = top
    for title, grp in grouped:
        parts.append(text(pad, y + 20, title.upper(), 11, t["muted"], 600, extra=' letter-spacing="0.06em"'))
        y += group_h
        for r in grp:
            v = r["speedup"]
            cy = y + row_h / 2
            parts.append(text(pad, cy + 4.5, r["name"], 13, t["ink2"]))
            by = cy - bar_h / 2
            a, b = sorted((sx(1), sx(v)))
            fill = t["accent"] if v >= 1 else x["other"]
            parts.append(f'<rect x="{a:.1f}" y="{by:.1f}" width="{max(b - a, 1):.1f}" height="{bar_h}" '
                         f'rx="3" fill="{fill}"/>')
            parts.append(text(max(sx(v), sx(1)) + 6, cy + 4.5, fmt_x(v), 12,
                              t["ink"] if v >= 1 else t["muted"], 600 if v >= 1 else 400, extra=NUM))
            y += row_h
    note = (f"orjson time ÷ rjson time, median of 3 runs · CPython {meta['python']} · orjson {meta['orjson']} · "
            f"PGO build · x86_64 · benches/showcase.py")
    parts.append(text(pad, h - 16, note, 12, t["muted"]))
    parts.append("</svg>")
    return "\n".join(parts)


# -- architecture ---------------------------------------------------------------------


def box(t, x, bx, y, w, h, label, sub=None, rj=False):
    fill = x["box_rj"] if rj else x["box"]
    stroke = t["accent"] if rj else t["grid"]
    out = [f'<rect x="{bx:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h}" rx="6" fill="{fill}" '
           f'stroke="{stroke}" stroke-width="1"/>']
    ty = y + (h / 2 + 4.5 if sub is None else h / 2 - 3)
    out.append(text(bx + w / 2, ty, label, 12.5, t["ink"], 600 if rj else 500, anchor="middle"))
    if sub:
        out.append(text(bx + w / 2, ty + 16, sub, 11, t["ink2"], anchor="middle"))
    return out


def arrow(t, x0, x1, y):
    return [f'<line x1="{x0:.1f}" y1="{y:.1f}" x2="{x1 - 5:.1f}" y2="{y:.1f}" stroke="{t["axis"]}" stroke-width="1.5"/>',
            f'<path d="M{x1:.1f},{y:.1f} l-7,-4 v8 z" fill="{t["axis"]}"/>']


def lane(t, x, x0, y, h, who, steps, rj):
    """One pipeline: a lane label, then boxes of the given relative widths."""
    out = [text(x0, y + h / 2 + 4.5, who, 12.5, t["accent"] if rj else x["other"], 600)]
    gap = 26
    left = x0 + 64
    total = 880 - 24 - 16 - left
    units = sum(s[2] for s in steps)
    avail = total - gap * (len(steps) - 1)
    bx = left
    for i, (label, sub, u) in enumerate(steps):
        bw = avail * u / units
        out += box(t, x, bx, y, bw, h, label, sub, rj)
        if i + 1 < len(steps):
            out += arrow(t, bx + bw + 3, bx + bw + gap - 3, y + h / 2)
        bx += bw + gap
    return out


def architecture_svg(t, x, eff):
    w, pad = 880, 24
    lane_h, lane_gap = 44, 10
    sections = [
        ("loads", "one pass instead of parse-then-convert",
         [("bytes", None, 1), ("yyjson parse", "tree of every value", 2),
          ("second pass", "tree → Python objects", 2)],
         [("bytes", None, 1), ("one pass", "text → Python objects directly", 4)],
         eff["loads"]),
        ("dumps", "output sized from recent calls, written as bytes or str",
         [("4 KiB buffer", "doubles when full", 2), ("escape: SSE2", "or AVX-512 build", 2),
          ("bytes", ".decode() for str", 1.4)],
         [("sized buffer", "from recent sizes", 2), ("escape: AVX-512", "/ AVX2 / SSE2", 2),
          ("bytes or str", "written directly", 1.4)],
         eff["dumps"]),
        ("datetime · UUID · Enum · dataclass", "what orjson looks up per value, rjson caches",
         [("per aware datetime", "3 hasattr probes + utcoffset() call", 3), ("format", "C fields", 1.2)],
         [("cached once", "tz offset; class facts by type version", 3), ("format", "C fields", 1.2)],
         eff["native"]),
    ]
    sec_h = 30 + 2 * lane_h + lane_gap + 34
    top = 58
    h = top + len(sections) * sec_h + 36
    parts = [svg_open(w, h, "How rjson's design differs from orjson's for loads, dumps and native types"),
             card(w, h, t),
             text(pad, 34, "Where rjson's design differs from orjson's", 15, t["ink"], 600)]
    y = top
    for i, (name, what, orj, rj, effect) in enumerate(sections):
        if i:
            parts.append(f'<line x1="{pad}" y1="{y - 8:.1f}" x2="{w - pad}" y2="{y - 8:.1f}" '
                         f'stroke="{t["grid"]}" stroke-width="1"/>')
        parts.append(text(pad, y + 14, name, 13.5, t["ink"], 600)
                     .replace("</text>", f'<tspan dx="10" font-weight="400" fill="{t["ink2"]}">{escape(what)}</tspan></text>'))
        ly = y + 26
        parts += lane(t, x, pad, ly, lane_h, "orjson", orj, False)
        parts += lane(t, x, pad, ly + lane_h + lane_gap, lane_h, "rjson", rj, True)
        parts.append(text(pad + 64, ly + 2 * lane_h + lane_gap + 22, effect, 12, t["accent"], 600))
        y += sec_h
    parts.append(text(pad, h - 16, "orjson 3.12.0 per its source · measured effects: docs/SHOWCASE.md, "
                                   "docs/PRODUCTION_READINESS.md", 12, t["muted"]))
    parts.append("</svg>")
    return "\n".join(parts)


def social_svg(rows):
    """1280x640 card for the repository's social preview (link unfurls)."""
    t = THEMES["dark"]
    w, h = 1280, 640
    wins = sum(r.get("speedup_min", r["speedup"]) > 1 for r in rows)  # faster in every run
    geo = math.exp(sum(math.log(r["speedup"]) for r in rows) / len(rows))
    best = max(rows, key=lambda r: r["speedup"])
    stats = [(fmt_x(geo), f"vs orjson, {len(rows)} workloads"),
             (fmt_x(best["speedup"]), f"{best['name']} vs orjson"),
             ("15×", "dumps vs stdlib json")]
    parts = [svg_open(w, h, "rjson: fast JSON for Python, written in Rust"),
             f'<rect width="{w}" height="{h}" fill="{t["surface"]}"/>',
             f'<rect x="0" y="0" width="12" height="{h}" fill="{t["accent"]}"/>',
             text(96, 190, "rjson", 132, t["ink"], 700),
             text(100, 256, "Fast JSON for Python, written in Rust", 40, t["ink2"], 500)]
    for i, (big, small) in enumerate(stats):
        x = 100 + i * 380
        parts.append(text(x, 400, big, 72, t["accent"], 700, extra=NUM))
        parts.append(text(x + 2, 440, small, 22, t["ink2"]))
    parts.append(f'<rect x="100" y="505" width="370" height="58" rx="10" fill="{t["grid"]}"/>')
    parts.append(f'<text x="122" y="543" font-family="ui-monospace, SFMono-Regular, Menlo, monospace" '
                 f'font-size="28" font-weight="600" fill="{t["ink"]}">pip install pyrjson</text>')
    parts.append(text(w - 100, 543, "github.com/TinDang97/rjson", 26, t["muted"], anchor="end"))
    parts.append("</svg>")
    return "\n".join(parts)


def effects(rows):
    by = {r["name"]: r["speedup"] for r in rows}

    def span(names):
        vals = [by[n] for n in names]
        return f"{min(vals):.2f}–{max(vals):.2f}×"

    return {
        "loads": (f"loads {span(['twitter.json loads', 'citm_catalog.json loads', 'canada.json loads', 'github.json request', '1,000 small request bodies'])} "
                  f"faster · 30–37% less peak memory on large files (no intermediate tree)"),
        "dumps": (f"dumps {span(['twitter.json dumps', 'citm_catalog.json dumps', 'canada.json dumps', 'github.json response', 'paginated REST page', 'log records with tracebacks', '1,000 small responses'])} "
                  f"faster · as str {span(['github.json as str', 'twitter.json as str', 'log records as str'])}"),
        "native": (f"UTC datetimes {fmt_x(by['UTC timestamps'])} · datetime/UUID/Enum records "
                   f"{fmt_x(by['events with datetime, UUID, Enum'])} · slots dataclasses {fmt_x(by['dataclasses with __slots__'])}"),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results", nargs="?", default=os.path.join(DOCS, "showcase-results.json"))
    ap.add_argument("--out", default=os.path.join(DOCS, "img"))
    args = ap.parse_args()
    with open(args.results) as f:
        data = json.load(f)
    rows, meta = data["rows"], data["meta"]
    eff = effects(rows)
    os.makedirs(args.out, exist_ok=True)
    for mode, t in THEMES.items():
        with open(os.path.join(args.out, f"showcase-{mode}.svg"), "w") as f:
            f.write(showcase_svg(t, EXTRA[mode], rows, meta))
        with open(os.path.join(args.out, f"architecture-{mode}.svg"), "w") as f:
            f.write(architecture_svg(t, EXTRA[mode], eff))
    with open(os.path.join(args.out, "social-preview.svg"), "w") as f:
        f.write(social_svg(rows))
    print("\n".join(f"{k}: {v}" for k, v in eff.items()))


if __name__ == "__main__":
    main()
