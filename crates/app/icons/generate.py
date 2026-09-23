#!/usr/bin/env python3
"""Draws the app's icons from logo.svg with the standard library only.

    python3 crates/app/icons/generate.py

writes, next to this script:

- tray/<theme>-<state>-<size>.png: the tray mark for a light or dark taskbar,
  ready, attention or problem, at 16, 20, 24 and 32 px (100 % to 200 %). The
  logo's own colour is the state. At tray sizes the logo's lines are thinner
  than a pixel, so the mark is drawn half a pixel heavier: outlined with a
  40-unit round-joined stroke in the logo's 1254-unit box, the same shape;
- icon.ico and icon.png: the logo on a light rounded tile, the app's icon.

The outputs are committed; run this again only when the logo or a colour
changes. The oschess name and logo are not covered by the MIT licence (NOTICE).
"""

import math
import os
import re
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
BOX = 1254.0
TRAY_STROKE = 40.0

# Tray mark colours by taskbar theme; each keeps a contrast of 3:1 or more.
TRAY = {
    "light": {"ready": "#1c1c1c", "attention": "#b86e00", "problem": "#d13438"},
    "dark": {"ready": "#ffffff", "attention": "#ffb900", "problem": "#ff6b6b"},
}
TRAY_SIZES = (16, 20, 24, 32)
ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
MARK = "#1d140f"
TILE = "#fffdf9"
TILE_BORDER = "#eadfd3"


def path_data():
    svg = open(os.path.join(HERE, "logo.svg"), encoding="utf-8").read()
    found = re.search(r' d="([^"]+)"', svg)
    if found is None:
        raise ValueError("logo.svg has no path")
    return found.group(1)


def subpaths(d):
    """The path as closed polygons, cubic curves flattened to 32 lines each."""
    tokens = re.findall(r"[MmCcLlZz]|-?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?", d)
    polys, cur = [], []
    x = y = sx = sy = 0.0
    cmd = ""
    i = 0

    def num():
        nonlocal i
        v = float(tokens[i])
        i += 1
        return v

    while i < len(tokens):
        t = tokens[i]
        if t.isalpha():
            cmd = t
            i += 1
            if cmd in "Zz":
                if cur:
                    polys.append(cur)
                cur = []
                x, y = sx, sy
                continue
        if not cmd:
            raise ValueError("the path starts with a number")
        if cmd in "Mm":
            dx, dy = num(), num()
            if cur:
                polys.append(cur)
            x, y = (x + dx, y + dy) if cmd == "m" else (dx, dy)
            sx, sy = x, y
            cur = [(x, y)]
            cmd = "l" if cmd == "m" else "L"  # further pairs are lines
        elif cmd in "Ll":
            dx, dy = num(), num()
            x, y = (x + dx, y + dy) if cmd == "l" else (dx, dy)
            cur.append((x, y))
        elif cmd in "Cc":
            p = [num() for _ in range(6)]
            if cmd == "c":
                p = [p[0] + x, p[1] + y, p[2] + x, p[3] + y, p[4] + x, p[5] + y]
            x0, y0 = x, y
            for k in range(1, 33):
                s = k / 32
                u = 1 - s
                cur.append((
                    u * u * u * x0 + 3 * u * u * s * p[0] + 3 * u * s * s * p[2] + s * s * s * p[4],
                    u * u * u * y0 + 3 * u * u * s * p[1] + 3 * u * s * s * p[3] + s * s * s * p[5],
                ))
            x, y = p[4], p[5]
        else:
            raise ValueError(f"unexpected path command {cmd}")
    if cur:
        polys.append(cur)
    return polys


def edges(polys, scale, dx, dy):
    out = []
    for poly in polys:
        pts = [(px * scale + dx, py * scale + dy) for px, py in poly]
        for a, b in zip(pts, pts[1:] + pts[:1]):
            if a != b:
                out.append((a[0], a[1], b[0], b[1]))
    return out


def segment_distance(px, py, e):
    x0, y0, x1, y1 = e
    vx, vy = x1 - x0, y1 - y0
    t = ((px - x0) * vx + (py - y0) * vy) / (vx * vx + vy * vy)
    t = min(1.0, max(0.0, t))
    return math.hypot(px - x0 - t * vx, py - y0 - t * vy)


def coverage(size, polys, scale, dx, dy, grow=0.0, samples=8):
    """Each pixel's covered fraction of the filled path, grown by `grow`
    pixels in every direction (a round-joined stroke of twice that width)."""
    es = edges(polys, scale, dx, dy)
    cov = [[0.0] * size for _ in range(size)]
    step = 1.0 / samples
    weight = 1.0 / (samples * samples)
    for row in range(size * samples):
        y = (row + 0.5) * step
        crossings = []
        for x0, y0, x1, y1 in es:
            if (y0 <= y < y1) or (y1 <= y < y0):
                crossings.append((x0 + (y - y0) * (x1 - x0) / (y1 - y0), 1 if y1 > y0 else -1))
        crossings.sort()
        near = [e for e in es if min(e[1], e[3]) - grow <= y <= max(e[1], e[3]) + grow] if grow else []
        py = row // samples
        for col in range(size * samples):
            x = (col + 0.5) * step
            wind = 0
            for cx, w in crossings:
                if cx > x:
                    break
                wind += w
            inside = wind != 0
            if not inside and grow:
                inside = any(
                    min(e[0], e[2]) - grow <= x <= max(e[0], e[2]) + grow and segment_distance(x, y, e) <= grow
                    for e in near
                )
            if inside:
                cov[py][col // samples] += weight
    return cov


def rgb(hex_colour):
    h = hex_colour.lstrip("#")
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def png(pixels):
    """A PNG of RGBA rows."""
    h, w = len(pixels), len(pixels[0])
    raw = b"".join(b"\x00" + bytes(c for p in r for c in p) for r in pixels)

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)

    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def ico(images):
    """An .ico of PNG images, largest last."""
    head = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries, data = b"", b""
    for size, blob in images:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    return head + entries + data


def mark_pixels(cov, colour):
    r, g, b = rgb(colour)
    return [[(r, g, b, round(255 * min(1.0, c))) for c in row] for row in cov]


def rounded_tile(size):
    """Coverage of the tile and of its inner area inside a 1-pixel border."""
    radius = max(4.0, round(size * 0.22))
    samples = 4

    def inside(x, y, x0, y0, x1, y1, r):
        cx = min(max(x, x0 + r), x1 - r)
        cy = min(max(y, y0 + r), y1 - r)
        return (x - cx) ** 2 + (y - cy) ** 2 <= r * r

    outer = [[0.0] * size for _ in range(size)]
    inner = [[0.0] * size for _ in range(size)]
    border = 1.0 if size < 64 else size / 64
    for py in range(size):
        for px in range(size):
            for sy in range(samples):
                for sx in range(samples):
                    x = px + (sx + 0.5) / samples
                    y = py + (sy + 0.5) / samples
                    if inside(x, y, 0, 0, size, size, radius):
                        outer[py][px] += 1 / samples ** 2
                        if inside(x, y, border, border, size - border, size - border, radius - border):
                            inner[py][px] += 1 / samples ** 2
    return outer, inner


def app_icon(size, polys):
    outer, inner = rounded_tile(size)
    mark_size = size * 0.78
    offset = (size - mark_size) / 2
    cov = coverage(size, polys, mark_size / BOX, offset, offset, samples=8 if size <= 64 else 4)
    tile, edge, mark = rgb(TILE), rgb(TILE_BORDER), rgb(MARK)
    pixels = []
    for y in range(size):
        row = []
        for x in range(size):
            a_out, a_in, m = outer[y][x], inner[y][x], min(1.0, cov[y][x])
            # The border shows where the tile is covered but its inner area is not.
            base = [e * (a_out - a_in) + t * a_in for e, t in zip(edge, tile)]
            base = [c / a_out if a_out else 0 for c in base]
            colour = [m * k + (1 - m) * c for k, c in zip(mark, base)]
            row.append(tuple(round(c) for c in colour) + (round(255 * a_out),))
        pixels.append(row)
    return pixels


def main():
    polys = subpaths(path_data())
    os.makedirs(os.path.join(HERE, "tray"), exist_ok=True)
    for size in TRAY_SIZES:
        scale = size / BOX
        cov = coverage(size, polys, scale, 0, 0, grow=TRAY_STROKE / 2 * scale)
        for theme, states in TRAY.items():
            for state, colour in states.items():
                with open(os.path.join(HERE, "tray", f"{theme}-{state}-{size}.png"), "wb") as f:
                    f.write(png(mark_pixels(cov, colour)))
    images = [(size, png(app_icon(size, polys))) for size in ICON_SIZES]
    with open(os.path.join(HERE, "icon.ico"), "wb") as f:
        f.write(ico(images))
    with open(os.path.join(HERE, "icon.png"), "wb") as f:
        f.write(dict(images)[256])


if __name__ == "__main__":
    main()
