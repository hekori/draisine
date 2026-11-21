#!/usr/bin/env python3
"""Builds assets/logo_mask.{png,bin} (256x256 alpha mask used by src/icon.rs) from a logo image.

Usage: python3 scripts/make_logo_mask.py [assets/logo_source.png]

The source is a black logo. Real transparency is used if the image has it; otherwise a
light/grey checkerboard painted into the image (as in assets/logo_source.png) is removed.
Needs Pillow.
"""
import sys
from PIL import Image, ImageFilter

src = sys.argv[1] if len(sys.argv) > 1 else "assets/logo_source.png"
img = Image.open(src)
if img.mode in ("RGBA", "LA") and img.getextrema()[-1][0] < 255:
    # Real alpha: coverage = alpha * darkness
    a = Image.eval(img.convert("L"), lambda v: 255 - v)
    a = Image.composite(a, Image.new("L", img.size, 0), img.getchannel("A"))
else:
    im = img.convert("L")
    w, h = im.size
    px = im.load()
    # Painted checkerboard: light/dark squares of size T, top-left square light.
    T, LIGHT, DARK, INK = w / 93, 253.0, 231.0, 13.0
    a = Image.new("L", (w, h))
    ap = a.load()
    for y in range(h):
        for x in range(w):
            bg = LIGHT if (int(x / T) + int(y / T)) % 2 == 0 else DARK
            ap[x, y] = int(round(max(0.0, min(1.0, (bg - px[x, y]) / (bg - INK))) * 255))
    a = a.filter(ImageFilter.MedianFilter(3))
    # Keep only the logo region (near reliably dark pixels), drop faint leftovers.
    region = im.point(lambda v: 255 if v < 200 else 0).filter(ImageFilter.MaxFilter(9))
    a = Image.composite(a, Image.new("L", (w, h), 0), region)
    a = a.point(lambda v: 0 if v < 45 else v)
    # See-through parts: smooth out residue of the checkerboard edges.
    mid = a.point(lambda v: 255 if 60 < v < 225 else 0)
    mostly_mid = mid.filter(ImageFilter.BoxBlur(4)).point(lambda v: 255 if v > 200 else 0)
    a = Image.composite(a.filter(ImageFilter.BoxBlur(5)), a, mostly_mid)

x0, y0, x1, y1 = a.getbbox()
side = max(x1 - x0, y1 - y0)
sq = Image.new("L", (side, side))
sq.paste(a.crop((x0, y0, x1, y1)), ((side - (x1 - x0)) // 2, (side - (y1 - y0)) // 2))
m = sq.resize((256, 256), Image.LANCZOS)
m.save("assets/logo_mask.png")
open("assets/logo_mask.bin", "wb").write(m.tobytes())
print("wrote assets/logo_mask.png and assets/logo_mask.bin")
