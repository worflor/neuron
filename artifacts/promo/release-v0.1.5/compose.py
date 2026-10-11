#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

"""Compose a release review card from unmodified Slint screenshot crops."""

from pathlib import Path
from PIL import Image, ImageDraw, ImageFont

HERE = Path(__file__).resolve().parent
source = Image.open(HERE / "source-inspector-fixture.png").convert("RGB")
canvas = Image.new("RGB", (1600, 900), "#050709")
draw = ImageDraw.Draw(canvas)
draw.polygon([(0, 170), (930, 170), (660, 800), (0, 800)], fill="#080e13")
draw.polygon([(930, 170), (1600, 170), (1600, 800), (660, 800)], fill="#090c10")
draw.line([(930, 170), (660, 800)], fill="#00b4db", width=2)

def font(size, mono=False):
    name = "CascadiaMono.ttf" if mono else "bahnschrift.ttf"
    return ImageFont.truetype(str(Path("C:/Windows/Fonts") / name), size)

def text(pos, value, size, colour="#e2e7ec", mono=False):
    draw.text(pos, value, font=font(size, mono), fill=colour)

def crop_panel(box, position, width):
    panel = source.crop(box)
    height = round(panel.height * width / panel.width)
    panel = panel.resize((width, height), Image.Resampling.LANCZOS)
    canvas.paste(panel, position)

text((64, 38), "neuron", 58)
text((1445, 55), "v0.1.5", 24, "#00b4db", True)
text((64, 114), "light, layered", 25, "#8d9ba8", True)

# Coordinates refer to the retained 1000 x 1600 inspector fixture.
crop_panel((644, 1336, 978, 1535), (74, 335), 630)
crop_panel((644, 522, 978, 928), (985, 214), 440)
text((74, 251), "BUILD IN LAYERS", 27, "#00b4db", True)
text((74, 706), "Blend. Fade. Make room for another look.", 19, "#8d9ba8")
text((951, 765), "COLOUR IN MOTION", 27, "#00b4db", True)

draw.line([(64, 828), (1536, 828)], fill="#17212a", width=1)
text((64, 851), "WINDOWS BETA  /  LIGHTING STUDIO UPDATE", 17, "#8d9ba8", True)
text((1110, 851), "Actual UI · disarmed fixture", 16, "#687783", True)
canvas.save(HERE / "diagonal-screenshot-draft.png")
