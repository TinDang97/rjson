"""Render a recorded terminal session as an animated SVG (docs/img/demo.svg).

    python benches/demo.py > demo.txt
    python benches/make_demo_svg.py demo.txt [--cmd "python benches/demo.py"] [--out docs/img/demo.svg]

The command is typed out, then each output line appears in turn, the whole
screen holds, and the animation loops. Pure SVG + CSS (no script), so it
plays in GitHub READMEs and on PyPI. The text is the recorded output as is.
"""

import argparse
import os
from xml.sax.saxutils import escape

HERE = os.path.dirname(os.path.abspath(__file__))

BG, BAR, INK, DIM, OK, PROMPT = "#15181c", "#23272e", "#e6e6e3", "#8b949e", "#57c785", "#6cb6ff"
FONT = 'ui-monospace, SFMono-Regular, "JetBrains Mono", Menlo, Consolas, monospace'


def render(cmd, lines, width=820):
    size, lh, pad = 14, 22, 20
    top = 44
    h = top + (len(lines) + 1) * lh + pad
    char_w = size * 0.6
    type_s, line_s, hold_s = 1.4, 0.45, 5.0
    total = 0.3 + type_s + 0.3 + len(lines) * line_s + hold_s
    pct = lambda t: f"{100 * t / total:.2f}%"

    css = [f"text{{font-family:{FONT};font-size:{size}px;white-space:pre}}"]
    # Typing: a clip rect grows over the command in steps (one per char).
    t0 = 0.3
    css.append(f"@keyframes type{{0%,{pct(t0)}{{width:0}}{pct(t0 + type_s)},100%{{width:{len(cmd) * char_w + 4:.0f}px}}}}")
    css.append(f".cmd{{animation:type {total:.2f}s steps({len(cmd)}) infinite}}")
    body = []
    for i, line in enumerate(lines):
        start = t0 + type_s + 0.3 + i * line_s
        css.append(f"@keyframes l{i}{{0%,{pct(start)}{{opacity:0}}{pct(start + 0.05)},97%{{opacity:1}}100%{{opacity:0}}}}")
        y = top + (i + 2) * lh - 6
        fill = OK if "faster" in line else (DIM if line.startswith("rjson") else INK)
        body.append(f'<text x="{pad}" y="{y}" fill="{fill}" style="animation:l{i} {total:.2f}s infinite">'
                    f"{escape(line)}</text>")
    y_cmd = top + lh - 6
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{h}" viewBox="0 0 {width} {h}" '
        f'role="img" aria-label="{escape("Terminal: " + cmd + ". " + " ".join(lines))}">',
        f"<style>{''.join(css)}</style>",
        f'<rect width="{width}" height="{h}" rx="10" fill="{BG}"/>',
        f'<rect width="{width}" height="30" rx="10" fill="{BAR}"/><rect y="20" width="{width}" height="10" fill="{BAR}"/>',
        '<circle cx="18" cy="15" r="5.5" fill="#ff5f57"/><circle cx="36" cy="15" r="5.5" fill="#febc2e"/>'
        '<circle cx="54" cy="15" r="5.5" fill="#28c840"/>',
        '<defs><clipPath id="c"><rect class="cmd" x="0" y="0" height="400"/></clipPath></defs>',
        f'<text x="{pad}" y="{y_cmd}" fill="{PROMPT}">$</text>',
        f'<g clip-path="url(#c)" transform="translate({pad + 2 * char_w:.1f},0)">'
        f'<text x="0" y="{y_cmd}" fill="{INK}">{escape(cmd)}</text></g>',
        *body,
        "</svg>",
    ]
    return "\n".join(svg)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("transcript")
    ap.add_argument("--cmd", default="python benches/demo.py")
    ap.add_argument("--out", default=os.path.join(HERE, "..", "docs", "img", "demo.svg"))
    args = ap.parse_args()
    with open(args.transcript) as f:
        lines = [ln.rstrip("\n") for ln in f if ln.strip()]
    with open(args.out, "w") as f:
        f.write(render(args.cmd, lines))


if __name__ == "__main__":
    main()
