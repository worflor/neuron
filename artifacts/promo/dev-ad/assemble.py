# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Composite the end card over the rendered frames, mux audio.wav and encode the ad.

    python assemble.py            # render/ -> final/ -> neuron-dev-ad.mp4
"""
import os
import subprocess
import sys

from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, os.path.dirname(__file__))
import timeline as tl

SRC = os.path.join(tl.OUT, os.environ.get("PROMO_RENDER", "render2"))
DST = os.path.join(tl.OUT, "final")
MP4 = os.path.join(tl.OUT, "neuron-dev-ad.mp4")
WORD = ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", 132)
TAG = ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", 46)


def rgb(h):
    h = h.lstrip("#")
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def ease(k):
    k = max(0.0, min(1.0, k))
    return k * k * (3 - 2 * k)


def tracked(d, xy, text, font, fill, track):
    """Draw text with extra letter spacing; xy is the centre."""
    widths = [font.getlength(c) for c in text]
    total = sum(widths) + track * (len(text) - 1)
    x = xy[0] - total / 2
    for c, w in zip(text, widths):
        d.text((x, xy[1]), c, font=font, fill=fill, anchor="lm")
        x += w + track
    return total


def card(img, k_dim, k_word, k_tag):
    if k_dim > 0:
        img = Image.blend(img, Image.new("RGB", img.size, rgb(tl.BG)), 0.55 * k_dim)
    over = Image.new("RGBA", img.size, (0, 0, 0, 0))
    d = ImageDraw.Draw(over)
    cy = int(tl.H * 0.31)
    if k_word > 0:
        a = int(255 * k_word)
        rise = (1 - k_word) * 18
        total = tracked(d, (tl.W / 2, cy + rise), "NEURON", WORD, (*rgb(tl.TEXT), a), 46)
        sq = 30
        x0 = tl.W / 2 - total / 2 + 6
        d.rectangle([x0, cy - 130 + rise, x0 + sq, cy - 130 + sq + rise], fill=(*rgb(tl.ACCENT), a))
    if k_tag > 0:
        a = int(255 * k_tag)
        d.text((tl.W / 2, cy + 130 + (1 - k_tag) * 12), tl.TAGLINE, font=TAG, fill=(*rgb(tl.TEXT_MID), a), anchor="mm")
    img = img.convert("RGBA")
    img.alpha_composite(over)
    return img.convert("RGB")


def main():
    os.makedirs(DST, exist_ok=True)
    n = tl.frames()
    for f in range(1, n + 1):
        src = os.path.join(SRC, f"{f:05d}.png")
        if not os.path.exists(src):
            raise SystemExit(f"missing {src}")
        t = (f - 1) / tl.FPS
        img = Image.open(src).convert("RGB")
        if img.size != (tl.W, tl.H):
            img = img.resize((tl.W, tl.H), Image.LANCZOS)
        k_dim = ease((t - tl.ENDCARD_T) / 0.9)
        k_word = ease((t - tl.ENDCARD_T - 0.25) / 0.7)
        k_tag = ease((t - tl.ENDCARD_T - 0.85) / 0.6)
        if k_dim > 0:
            img = card(img, k_dim, k_word, k_tag)
        img.save(os.path.join(DST, f"{f:05d}.png"), compress_level=1)
    subprocess.run([
        "ffmpeg", "-y", "-loglevel", "error", "-framerate", str(tl.FPS),
        "-i", os.path.join(DST, "%05d.png"),
        "-i", os.path.join(tl.OUT, "audio.wav"),
        "-c:a", "aac", "-b:a", "192k", "-shortest",
        "-c:v", "libx264", "-preset", "slow", "-crf", "15", "-pix_fmt", "yuv420p",
        "-profile:v", "high", "-movflags", "+faststart", MP4,
    ], check=True)
    print("wrote", MP4)


if __name__ == "__main__":
    main()
