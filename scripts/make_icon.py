#!/usr/bin/env python3
"""Generate the Printables Manager icon set.

The artwork deliberately stays close to the upstream Printables Offline icon:
the same orange "P" mark on white, with the grey badge in the lower-right corner
replaced by a bold "M".

Run from the repo root:

    python3 scripts/make_icon.py

It reads the original 1024px artwork out of `src-tauri/icons/icon.icns`, paints
over the download badge, draws the M, and writes every size Tauri needs plus a
fresh `.icns`.
"""

from __future__ import annotations

import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ICONS = ROOT / "src-tauri" / "icons"
SOURCE_ICNS = ICONS / "icon.icns"
WORK = Path("/tmp/po-icon-work")

FONT_CANDIDATES = [
    "/System/Library/Fonts/Supplemental/Arial Bold.ttf",
    "/System/Library/Fonts/Supplemental/Arial Rounded Bold.ttf",
    "/System/Library/Fonts/Helvetica.ttc",
    "/System/Library/Fonts/SFNS.ttf",
]


def extract_source() -> Image.Image:
    if not SOURCE_ICNS.exists():
        sys.exit(f"missing source artwork: {SOURCE_ICNS}")
    WORK.mkdir(parents=True, exist_ok=True)
    out = WORK / "original.png"
    subprocess.run(
        ["sips", "-s", "format", "png", str(SOURCE_ICNS), "--out", str(out)],
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return Image.open(out).convert("RGBA")


def find_badge(image: Image.Image) -> tuple[int, int, int, tuple[int, int, int]]:
    """Locate the grey circular badge so the M lands exactly where it was."""
    w, h = image.size
    px = image.load()
    xs: list[int] = []
    ys: list[int] = []
    r_sum = g_sum = b_sum = 0

    for y in range(int(h * 0.45), h):
        for x in range(int(w * 0.40), w):
            r, g, b, a = px[x, y]
            if a < 200:
                continue
            if abs(r - g) < 14 and abs(g - b) < 14 and 110 <= r <= 205:
                xs.append(x)
                ys.append(y)
                r_sum += r
                g_sum += g
                b_sum += b

    if not xs:
        sys.exit("could not find the badge circle in the source artwork")

    cx = (min(xs) + max(xs)) // 2
    cy = (min(ys) + max(ys)) // 2
    radius = ((max(xs) - min(xs)) + (max(ys) - min(ys))) // 4
    colour = (r_sum // len(xs), g_sum // len(ys), b_sum // len(xs))
    return cx, cy, radius, colour


def load_font(size: int) -> ImageFont.FreeTypeFont:
    for path in FONT_CANDIDATES:
        if Path(path).exists():
            try:
                return ImageFont.truetype(path, size)
            except OSError:
                continue
    sys.exit("no usable bold system font found")


def build_master() -> Image.Image:
    image = extract_source()
    cx, cy, radius, colour = find_badge(image)

    draw = ImageDraw.Draw(image)
    # Repaint the badge: identical geometry, no download arrow.
    draw.ellipse(
        [cx - radius, cy - radius, cx + radius, cy + radius],
        fill=(*colour, 255),
    )

    # The M is optically centred: cap height slightly under the circle.
    font = load_font(int(radius * 1.55))
    text = "M"
    left, top, right, bottom = draw.textbbox((0, 0), text, font=font)
    text_w, text_h = right - left, bottom - top
    draw.text(
        (cx - text_w / 2 - left, cy - text_h / 2 - top),
        text,
        font=font,
        fill=(255, 255, 255, 255),
    )
    return image


def write_pngs(master: Image.Image) -> None:
    sizes = {
        "32x32.png": 32,
        "64x64.png": 64,
        "128x128.png": 128,
        "128x128@2x.png": 256,
        "icon.png": 512,
    }
    for name, size in sizes.items():
        master.resize((size, size), Image.LANCZOS).save(ICONS / name, "PNG")


def write_icns(master: Image.Image) -> None:
    iconset = WORK / "PrintablesManager.iconset"
    if iconset.exists():
        shutil.rmtree(iconset)
    iconset.mkdir(parents=True)

    for base in (16, 32, 128, 256, 512):
        master.resize((base, base), Image.LANCZOS).save(
            iconset / f"icon_{base}x{base}.png", "PNG"
        )
        master.resize((base * 2, base * 2), Image.LANCZOS).save(
            iconset / f"icon_{base}x{base}@2x.png", "PNG"
        )

    subprocess.run(
        ["iconutil", "-c", "icns", str(iconset), "-o", str(ICONS / "icon.icns")],
        check=True,
    )


def main() -> None:
    master = build_master()
    master.save(WORK / "master-1024.png", "PNG")
    write_pngs(master)
    write_icns(master)
    print("wrote:", ", ".join(sorted(p.name for p in ICONS.glob("*.*"))))
    print("master preview:", WORK / "master-1024.png")


if __name__ == "__main__":
    main()
