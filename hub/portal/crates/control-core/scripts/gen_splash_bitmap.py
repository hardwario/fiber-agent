#!/usr/bin/env python3
"""Generate the ProXimos boot splash, portal/branding/proximos/splash.pbm, from that brand's logo.svg.

Usage:
    python3 gen_splash_bitmap.py > ../../branding/proximos/splash.pbm   # preview on stderr
    python3 gen_splash_bitmap.py --preview splash.png  # also write a 4x-magnified PNG

The splash that ships is data, not code: `render_splash` iterates whatever
[[u8; 128]; 64] it is handed, and proximos-brand's build script makes that
array from the build's brand directory (plan/portal/branding.md). This script
produces the ProXimos one from the SAME asset the web portal serves as its
logo, so the panel and the browser cannot drift apart. Other brands bring their
own splash.pbm (the FIBER one: portal/branding/scripts/gen-fiber-assets.py). The previous bitmap was the HARDWARIO glyph mark, hand-committed
from the v1 codebase with no way to regenerate it; that is the situation this
script exists to not repeat.

No SVG rasteriser is assumed to be installed (there is none on the usual dev
boxes here, and none in the Yocto build either). It is not needed: logo.svg is
pure outline geometry -- moveto/lineto/horizontal/vertical/cubic/closepath, no
arcs, no strokes, no embedded raster -- so the few hundred lines below cover it
exactly rather than approximately. numpy and Pillow are the only dependencies,
and Pillow only for --preview.
"""
import argparse
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path

import numpy as np

SVG_NS = "{http://www.w3.org/2000/svg}"
XLINK_HREF = "{http://www.w3.org/1999/xlink}href"

SOURCE_SVG = Path(__file__).resolve().parents[3] / "branding" / "proximos" / "logo.svg"

# THE WORDMARK ONLY. logo.svg is the "ProXimos lite" lockup: the wordmark, a
# baseline rule broken into two segments by the X's descender, and the word
# "lite" set small under the right-hand segment. Scaled to a 128 px panel the
# rule lands at 2 px and "lite" at 5 px tall, which on a 1-bit display with no
# grey is a smear rather than a word. Dropping both is what lets the wordmark
# itself have the full width -- and it is the wordmark that says which product
# this is.
SKIP_IDS = frozenset({"path522", "path526", "g536"})

PANEL_W, PANEL_H = 128, 64  # ST7920 geometry; see drivers/display.rs

# The wordmark is 3.71:1, so width is what binds -- fitting the panel's height
# would need 238 px of width. 124 rather than 128 leaves 2 px of quiet zone on
# each side; at 128 the final "s" ends in the last column and reads as clipped
# against the bezel.
TARGET_W = 124

# Coverage of a supersampled cell above which the output pixel is lit. 0.5 is
# the neutral choice and it thins the X's outline where the two strokes cross;
# 0.40 blobs the counters in "imos" shut. 0.45 keeps both, and it is the value
# the committed bitmap was generated with -- change it and you get a different,
# equally deliberate-looking bitmap, so change it on purpose.
THRESHOLD = 0.45

SUPERSAMPLE = 8   # 8x8 = 64 coverage levels per output pixel
CUBIC_STEPS = 24  # flattening steps per cubic; sub-pixel at 1024 px wide

_NUM = re.compile(r"[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?")
_CMD = re.compile(r"([MmLlHhVvCcSsZz])")


def _flatten_cubic(p0, p1, p2, p3, out, steps):
    for i in range(1, steps + 1):
        t = i / steps
        u = 1.0 - t
        out.append((
            u*u*u*p0[0] + 3*u*u*t*p1[0] + 3*u*t*t*p2[0] + t*t*t*p3[0],
            u*u*u*p0[1] + 3*u*u*t*p1[1] + 3*u*t*t*p2[1] + t*t*t*p3[1],
        ))


def parse_path(d, cubic_steps=CUBIC_STEPS):
    """Flatten a path's `d` into a list of subpaths, each a list of (x, y)."""
    toks = _CMD.split(d)
    subpaths, cur = [], []
    x = y = start_x = start_y = 0.0
    prev_ctrl = None
    i = 1
    while i < len(toks):
        cmd, args = toks[i], [float(v) for v in _NUM.findall(toks[i + 1])]
        i += 2
        rel, op = cmd.islower(), cmd.upper()

        if op == "Z":
            if cur:
                subpaths.append(cur)
                cur = []
            x, y = start_x, start_y
            prev_ctrl = None
            continue

        k, first = 0, True
        while k < len(args):
            if op == "M":
                nx, ny = args[k], args[k + 1]
                k += 2
                if rel:
                    nx, ny = x + nx, y + ny
                if first:
                    if cur:
                        subpaths.append(cur)
                    cur = [(nx, ny)]
                    start_x, start_y = nx, ny
                else:
                    cur.append((nx, ny))  # repeated moveto args are implicit linetos
                x, y = nx, ny
                prev_ctrl = None
            elif op == "L":
                nx, ny = args[k], args[k + 1]
                k += 2
                if rel:
                    nx, ny = x + nx, y + ny
                cur.append((nx, ny))
                x, y, prev_ctrl = nx, ny, None
            elif op == "H":
                nx = args[k]
                k += 1
                if rel:
                    nx += x
                cur.append((nx, y))
                x, prev_ctrl = nx, None
            elif op == "V":
                ny = args[k]
                k += 1
                if rel:
                    ny += y
                cur.append((x, ny))
                y, prev_ctrl = ny, None
            elif op == "C":
                x1, y1, x2, y2, nx, ny = args[k:k + 6]
                k += 6
                if rel:
                    x1, y1, x2, y2, nx, ny = x+x1, y+y1, x+x2, y+y2, x+nx, y+ny
                _flatten_cubic((x, y), (x1, y1), (x2, y2), (nx, ny), cur, cubic_steps)
                x, y, prev_ctrl = nx, ny, (x2, y2)
            elif op == "S":
                x2, y2, nx, ny = args[k:k + 4]
                k += 4
                if rel:
                    x2, y2, nx, ny = x + x2, y + y2, x + nx, y + ny
                x1, y1 = (2*x - prev_ctrl[0], 2*y - prev_ctrl[1]) if prev_ctrl else (x, y)
                _flatten_cubic((x, y), (x1, y1), (x2, y2), (nx, ny), cur, cubic_steps)
                x, y, prev_ctrl = nx, ny, (x2, y2)
            else:
                raise ValueError(f"unsupported path command {cmd!r}")
            if op == "M":
                op = "L"
            first = False

    if cur:
        subpaths.append(cur)
    return subpaths


def _parse_transform(spec):
    if not spec:
        return (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)
    m = re.match(r"\s*translate\(\s*([-\d.eE+]+)[,\s]+([-\d.eE+]+)\s*\)", spec)
    if m:
        return (1.0, 0.0, 0.0, 1.0, float(m.group(1)), float(m.group(2)))
    m = re.match(r"\s*matrix\(([^)]*)\)", spec)
    if m:
        return tuple(float(v) for v in _NUM.findall(m.group(1)))
    raise ValueError(f"unsupported transform {spec!r}")


def _mul(t1, t2):
    a1, b1, c1, d1, e1, f1 = t1
    a2, b2, c2, d2, e2, f2 = t2
    return (a1*a2 + c1*b2, b1*a2 + d1*b2,
            a1*c2 + c1*d2, b1*c2 + d1*d2,
            a1*e2 + c1*f2 + e1, b1*e2 + d1*f2 + f1)


def collect(root, skip_ids=frozenset()):
    """Walk the SVG, resolving <use>, returning [(points, fill_rule), ...]."""
    by_id = {el.get("id"): el for el in root.iter() if el.get("id")}
    out = []

    def walk(el, tf, rule):
        if el.get("id") in skip_ids:
            return
        tf = _mul(tf, _parse_transform(el.get("transform")))
        rule = el.get("fill-rule") or rule
        tag = el.tag.replace(SVG_NS, "")
        if tag == "path":
            a, b, c, d, e, f = tf
            for sp in parse_path(el.get("d")):
                out.append(([(a*px + c*py + e, b*px + d*py + f) for px, py in sp], rule))
        elif tag == "use":
            target = by_id.get((el.get(XLINK_HREF) or el.get("href", "")).lstrip("#"))
            if target is not None:
                shifted = _mul(tf, (1, 0, 0, 1, float(el.get("x", 0)), float(el.get("y", 0))))
                kids = [target] if target.tag.replace(SVG_NS, "") == "path" else list(target)
                for child in kids:
                    walk(child, shifted, rule)
        elif tag in ("g", "svg"):
            for child in el:
                walk(child, tf, rule)

    for child in root:
        if SVG_NS in child.tag and child.tag.replace(SVG_NS, "") != "defs":
            walk(child, (1.0, 0.0, 0.0, 1.0, 0.0, 0.0), "nonzero")
    return out


def rasterise(items, width, height, x0, y0, x1, y1):
    """Scanline-fill `items` into a boolean (height, width) mask.

    Winding is accumulated per fill rule and the two results OR'd, because
    logo.svg mixes them: the letterforms are `evenodd` but the "roXimos" run is
    `nonzero`, and evaluating one under the other rule punches holes in the
    wrong places.
    """
    sx, sy = width / (x1 - x0), height / (y1 - y0)
    mask = np.zeros((height, width), dtype=bool)
    columns = np.arange(width) + 0.5

    for rule in ("nonzero", "evenodd"):
        edges = []
        for points, item_rule in items:
            if item_rule != rule or len(points) < 2:
                continue
            pts = [((px - x0) * sx, (py - y0) * sy) for px, py in points]
            if pts[0] != pts[-1]:
                pts.append(pts[0])  # an unclosed subpath still bounds an area
            for (ax, ay), (bx, by) in zip(pts, pts[1:]):
                if ay != by:  # horizontal edges never cross a scanline centre
                    edges.append((ax, ay, bx, by))
        if not edges:
            continue

        e = np.asarray(edges, dtype=np.float64)
        ax, ay, bx, by = e[:, 0], e[:, 1], e[:, 2], e[:, 3]
        y_lo, y_hi = np.minimum(ay, by), np.maximum(ay, by)
        winding = np.where(by > ay, 1, -1)
        slope = (bx - ax) / (by - ay)

        for row in range(height):
            yc = row + 0.5
            hit = (y_lo <= yc) & (yc < y_hi)  # half-open, so shared vertices count once
            if not hit.any():
                continue
            xs = ax[hit] + (yc - ay[hit]) * slope[hit]
            order = np.argsort(xs, kind="stable")
            xs, w = xs[order], winding[hit][order]
            if rule == "evenodd":
                inside = (np.arange(1, len(xs) + 1) % 2) == 1
            else:
                inside = np.cumsum(w) != 0
            span = np.zeros(width, dtype=bool)
            for idx in np.nonzero(inside)[0]:
                if idx + 1 < len(xs):
                    span |= (columns >= xs[idx]) & (columns < xs[idx + 1])
            mask[row] |= span
    return mask


def build_panel():
    """Rasterise the wordmark and centre it in a PANEL_H x PANEL_W boolean panel."""
    root = ET.parse(SOURCE_SVG).getroot()
    items = collect(root, SKIP_IDS)
    xs = [p[0] for pts, _ in items for p in pts]
    ys = [p[1] for pts, _ in items for p in pts]
    x0, y0, x1, y1 = min(xs), min(ys), max(xs), max(ys)

    ss_w = TARGET_W * SUPERSAMPLE
    ss_h = int(round(ss_w * (y1 - y0) / (x1 - x0)))
    ss_h -= ss_h % SUPERSAMPLE  # whole output rows only
    fine = rasterise(items, ss_w, ss_h, x0, y0, x1, y1)

    h, w = ss_h // SUPERSAMPLE, TARGET_W
    coverage = fine.reshape(h, SUPERSAMPLE, w, SUPERSAMPLE).mean(axis=(1, 3))
    bits = coverage >= THRESHOLD

    if h > PANEL_H or w > PANEL_W:
        raise SystemExit(f"logo is {w}x{h}, does not fit {PANEL_W}x{PANEL_H}")
    panel = np.zeros((PANEL_H, PANEL_W), dtype=bool)
    top, left = (PANEL_H - h) // 2, (PANEL_W - w) // 2
    panel[top:top + h, left:left + w] = bits
    return panel


def pbm(panel):
    rows = "\n".join("".join("1" if v else "0" for v in row) for row in panel)
    return (
        "P1\n"
        "# ProXimos boot splash, 128x64, 1 = lit. Rasterised from logo.svg by\n"
        "# control-core/scripts/gen_splash_bitmap.py (the wordmark only).\n"
        "%d %d\n%s\n" % (PANEL_W, PANEL_H, rows)
    )


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--preview", metavar="PNG", help="write a 4x-magnified PNG of the panel")
    args = ap.parse_args()

    panel = build_panel()

    art = "\n".join("".join("#" if v else "." for v in row) for row in panel)
    print(f"{SOURCE_SVG.name}: {panel.sum()} lit of {panel.size}, "
          f"threshold {THRESHOLD}, skipped {sorted(SKIP_IDS)}", file=sys.stderr)
    print(art, file=sys.stderr)

    if args.preview:
        from PIL import Image
        img = Image.fromarray((~panel * 255).astype("uint8"))
        img.resize((PANEL_W * 4, PANEL_H * 4), Image.NEAREST).save(args.preview)
        print(f"preview written to {args.preview}", file=sys.stderr)

    sys.stdout.write(pbm(panel))


if __name__ == "__main__":
    main()
