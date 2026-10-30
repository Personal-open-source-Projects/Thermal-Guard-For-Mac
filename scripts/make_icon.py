#!/usr/bin/env python3
"""Generate the app icon as PNG files, then hand them to iconutil.

No third-party imaging library is used: the PNG is written by hand with zlib.
"""
import math
import os
import struct
import subprocess
import zlib

SIZES = [16, 32, 64, 128, 256, 512, 1024]
OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "build", "icon")
OUT = os.path.normpath(OUT)


def rounded_square_alpha(x, y, w, h, r):
    dx = min(x, w - 1 - x)
    dy = min(y, h - 1 - y)
    if dx >= r or dy >= r:
        return 1.0
    cx = r - dx
    cy = r - dy
    d = math.hypot(cx, cy)
    return max(0.0, min(1.0, r - d + 0.5))


def make(size):
    w = h = size
    px = bytearray()
    r = size * 0.22
    # colours: deep slate background, cyan->amber dial
    for y in range(h):
        for x in range(w):
            a = rounded_square_alpha(x, y, w, h, r)
            if a <= 0.0:
                px += bytes((0, 0, 0, 0))
                continue
            u = x / max(1, w - 1)
            v = y / max(1, h - 1)
            # background gradient
            br = int(24 + 18 * (1 - v))
            bg = int(30 + 26 * (1 - v))
            bb = int(38 + 40 * (1 - v))
            # thermometer body: vertical capsule centred
            cx = w * 0.5
            bw = w * 0.13
            top = h * 0.20
            bot = h * 0.68
            inside = False
            if abs(x - cx) <= bw and top <= y <= bot:
                inside = True
            bulb_r = bw * 1.85
            if math.hypot(x - cx, y - h * 0.74) <= bulb_r:
                inside = True
            if inside:
                fill = (y - top) / max(1.0, (h * 0.74 - top))
                fill = max(0.0, min(1.0, fill))
                hot = fill
                cr = int(90 + 165 * hot)
                cg = int(215 - 110 * hot)
                cb = int(235 - 170 * hot)
                px += bytes((cr, cg, cb, int(a * 255)))
                continue
            # subtle ring
            d = math.hypot(u - 0.5, v - 0.5)
            if 0.40 < d < 0.425:
                px += bytes((60, 84, 104, int(a * 200)))
                continue
            px += bytes((br, bg, bb, int(a * 255)))

    raw = bytearray()
    stride = w * 4
    for y in range(h):
        raw.append(0)
        raw += px[y * stride:(y + 1) * stride]

    def chunk(tag, data):
        return (struct.pack(">I", len(data)) + tag + data
                + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(bytes(raw), 9))
    png += chunk(b"IEND", b"")
    return png


def main():
    iconset = os.path.join(OUT, "AppIcon.iconset")
    os.makedirs(iconset, exist_ok=True)
    for s in SIZES:
        base = make(s)
        if s <= 512:
            with open(os.path.join(iconset, f"icon_{s}x{s}.png"), "wb") as f:
                f.write(base)
        if s >= 32:
            with open(os.path.join(iconset, f"icon_{s//2}x{s//2}@2x.png"), "wb") as f:
                f.write(base)
    icns = os.path.join(OUT, "AppIcon.icns")
    subprocess.run(["iconutil", "-c", "icns", iconset, "-o", icns], check=True)
    print(icns)


if __name__ == "__main__":
    main()
