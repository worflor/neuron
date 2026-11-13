# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Compose README images in docs/media from headless renders of the real Neuron window.

The renders come from crates/neuron-app/src/readme_shots.rs (see its header); this script only
crops, frames and encodes them. Inputs and the wallpaper live in D:/build-cache/readme (README_SRC);
outputs are written to docs/media. Nothing here draws UI: every pixel inside a card is the app's.

    python compose.py loop       # lighting-aurora-loop.webp from loop_000..045.png (1x renders)
    python compose.py picker     # picker-v2-*.webp from picker-<moment>.png (2x renders)

The backdrop is the owner's desktop wallpaper, Wallpaper Engine workshop item 2451612805 ("La
Creación de cheems"): its texture is extracted from scene.pkg on first use and never committed.
"""
import glob
import io
import os
import sys

from PIL import Image, ImageDraw, ImageFilter

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
SRC = os.environ.get("README_SRC", "D:/build-cache/readme")
MEDIA = os.path.join(ROOT, "docs", "media")
PKG = "C:/Program Files (x86)/Steam/steamapps/workshop/content/431960/2451612805/scene.pkg"


def wallpaper():
    path = os.path.join(SRC, "wallpaper.png")
    if not os.path.exists(path):
        data = open(PKG, "rb").read()
        best, at = None, 0
        while (at := data.find(b"\xff\xd8\xff", at)) >= 0:
            try:
                im = Image.open(io.BytesIO(data[at:]))
                im.load()
                if best is None or im.width * im.height > best.width * best.height:
                    best = im.copy()
            except Exception:
                pass
            at += 3
        best.convert("RGB").save(path)
    return Image.open(path).convert("RGB")


def rounded(im, radius):
    mask = Image.new("L", im.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, im.width - 1, im.height - 1), radius, fill=255)
    return mask


def card(ui, out_w, wall_box, margin=56, radius=14):
    """`ui` set on a blurred, dimmed crop of the wallpaper, with a rounded frame and a hairline."""
    scale = (out_w - 2 * margin) / ui.width
    inner = ui.resize((round(ui.width * scale), round(ui.height * scale)), Image.LANCZOS)
    out_h = inner.height + 2 * margin
    wall = wallpaper().crop(wall_box).resize((out_w, out_h), Image.LANCZOS)
    wall = wall.filter(ImageFilter.GaussianBlur(14)).point(lambda v: int(v * 0.82))
    canvas = wall.copy()
    shadow = Image.new("L", (out_w, out_h), 0)
    ImageDraw.Draw(shadow).rounded_rectangle((margin - 2, margin + 6, margin + inner.width + 2, margin + inner.height + 10), radius, fill=150)
    canvas.paste(Image.new("RGB", (out_w, out_h), (0, 0, 0)), (0, 0), shadow.filter(ImageFilter.GaussianBlur(12)))
    canvas.paste(inner, (margin, margin), rounded(inner, radius))
    ImageDraw.Draw(canvas).rounded_rectangle((margin, margin, margin + inner.width - 1, margin + inner.height - 1), radius,
                                             outline=(232, 236, 240), width=2)
    return canvas


def loop():
    frames = sorted(glob.glob(os.path.join(SRC, "loop_*.png")))
    assert frames, "render the loop first (readme_shots, NEURON_SHOT_FRAMES=46)"
    # the board, the catalog's first rows and the inspector to the spectrum's picker, as before
    box = (8, 130, 1232, 705)
    images = [Image.open(f).convert("RGB").crop(box) for f in frames]
    out = os.path.join(MEDIA, "lighting-aurora-loop.webp")
    images[0].save(out, save_all=True, append_images=images[1:], duration=120, loop=0, quality=78, method=6)
    print(out, len(images), "frames")


# moment -> (crop of the 2x render, wallpaper region behind the card)
PICKER = {
    "suggest": ((1740, 1050, 2470, 1900), (300, 300, 1500, 1700)),
    "roll": ((1740, 1050, 2470, 1990), (1700, 100, 2900, 1500)),
    "ask": ((1740, 1050, 2470, 1900), (900, 200, 2100, 1600)),
    "guide": ((80, 180, 848, 1018), (2200, 100, 3400, 1500)),
}


def picker():
    for moment, (crop, wall_box) in PICKER.items():
        ui = Image.open(os.path.join(SRC, f"picker-{moment}.png")).convert("RGB").crop(crop)
        out = os.path.join(MEDIA, f"picker-v2-{moment}.webp")
        card(ui, 760, wall_box).save(out, quality=88, method=6)
        print(out)


if __name__ == "__main__":
    for job in sys.argv[1:] or ["loop", "picker"]:
        {"loop": loop, "picker": picker}[job]()
