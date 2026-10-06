# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Render the terminal pane's texture, one PNG per frame, plus anchors.json.

anchors.json maps each rig event to the (u, v) of the line that caused it at the
moment it fires, so a light thread in the scene leaves from that exact line.
"""
import json
import math
import os
import sys

from PIL import Image, ImageDraw, ImageFont

sys.path.insert(0, os.path.dirname(__file__))
import timeline as tl

TW, TH = 1440, 1280
PAD_X, PAD_TOP, PAD_BOT = 78, 96, 70
FONT_PX = 56
LINE_H = 80
GROUP_GAP = 26      # extra space before a new speaker
PROMPT_GAP = 30     # space between history and the prompt row
FONT = ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", FONT_PX)
FONT_B = ImageFont.truetype(r"C:\Windows\Fonts\consolab.ttf", FONT_PX)
CHAR_W = FONT.getlength("M")


def rgb(h, a=1.0):
    h = h.lstrip("#")
    c = [int(h[i:i + 2], 16) for i in (0, 2, 4)]
    return tuple(int(round(v * a)) for v in c)


def lerp(a, b, k):
    return a + (b - a) * k


def ease(k):
    k = max(0.0, min(1.0, k))
    return k * k * (3 - 2 * k)


# Per user line: the time each of its chars lands.
_typed = tl.typed_chars()
_user_times = []
_i = 0
for line in tl.SCRIPT:
    if line["kind"] == "user":
        n = len(line["text"])
        _user_times.append([t for t, _ in _typed[_i:_i + n]])
        _i += n
USER_TIMES = {}
_k = 0
for idx, line in enumerate(tl.SCRIPT):
    if line["kind"] == "user":
        USER_TIMES[idx] = _user_times[_k]
        _k += 1


def commit_time(idx):
    """When a line joins the history (user lines commit after their last keystroke)."""
    line = tl.SCRIPT[idx]
    if line["kind"] == "user":
        return USER_TIMES[idx][-1] + 0.16
    return line["t"]


def speaker(kind):
    return {"user": "user", "agent": "agent"}.get(kind, "tool")


def layout(t):
    """History lines committed by t, with their y (content space)."""
    rows = []
    y = 0.0
    prev = None
    for idx, line in enumerate(tl.SCRIPT):
        if commit_time(idx) > t:
            continue
        sp = speaker(line["kind"])
        if prev is not None and sp != prev and line["kind"] != "cont":
            y += GROUP_GAP
        rows.append((idx, y))
        y += LINE_H
        prev = sp
    return rows, y


VIEW_H = TH - PAD_TOP - PAD_BOT - LINE_H - PROMPT_GAP


def target_scroll(t):
    _, h = layout(t)
    return max(0.0, h - VIEW_H)


def scroll_series():
    """Critically damped scroll, sampled per frame."""
    n = tl.frames()
    dt = 1.0 / tl.FPS
    pos, vel = 0.0, 0.0
    omega = 18.0
    out = []
    for f in range(n):
        t = f * dt
        x = target_scroll(t)
        acc = omega * omega * (x - pos) - 2 * omega * vel
        vel += acc * dt
        pos += vel * dt
        out.append(pos)
    return out


def visible_text(idx, t):
    line = tl.SCRIPT[idx]
    if line["kind"] == "user":
        return line["text"]
    if line["kind"] == "agent":
        n = int((t - line["t"]) * tl.STREAM_CPS) + 1
        return line["text"][:max(0, n)]
    return line["text"]


def draw_bullet(d, x, y, col):
    r = FONT_PX * 0.17
    cx, cy = x + CHAR_W * 0.5, y + LINE_H * 0.5
    d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=col)


def draw_line(d, idx, x, y, t):
    line = tl.SCRIPT[idx]
    age = t - commit_time(idx)
    a = ease(age / 0.14) if line["kind"] in ("tool", "out", "cont") else 1.0
    dx = (1 - a) * 14
    ty = y + (LINE_H - FONT_PX) * 0.42
    kind = line["kind"]
    text = visible_text(idx, t)
    if kind == "user":
        d.text((x, ty), ">", font=FONT_B, fill=rgb(tl.LISTEN, 0.85))
        d.text((x + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT, 0.92))
    elif kind == "agent":
        d.text((x, ty), text, font=FONT, fill=rgb(tl.TEXT))
    elif kind == "tool":
        # The bullet flares when the call lands, then settles.
        flare = 1.0 + 1.2 * math.exp(-max(0.0, age) / 0.25)
        bc = tuple(min(255, int(c * min(flare, 1.6) * a)) for c in rgb(tl.ACCENT))
        draw_bullet(d, x + dx, y, bc)
        d.text((x + dx + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT_MID, a))
    elif kind == "cont":
        d.text((x + dx + CHAR_W * 4, ty), text, font=FONT, fill=rgb(tl.TEXT_DIM, a))
    elif kind == "out":
        d.text((x + dx + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT_DIM, a))


def typing_line(t):
    """The user line currently in the prompt row, and how many chars are in it."""
    for idx, times in USER_TIMES.items():
        if times[0] - 0.001 <= t < commit_time(idx):
            n = sum(1 for ct in times if ct <= t)
            return idx, n
    return None, 0


def render(f, scroll, anchors_pending, anchors):
    t = (f) / tl.FPS
    img = Image.new("RGB", (TW, TH), rgb(tl.BG2))
    d = ImageDraw.Draw(img)

    # Pane chrome: hairline border and one accent square, the brand mark.
    d.rounded_rectangle([3, 3, TW - 4, TH - 4], radius=34, outline=rgb(tl.LINE), width=4)
    d.rectangle([PAD_X, 44, PAD_X + 16, 60], fill=rgb(tl.ACCENT, 0.9))
    d.text((PAD_X + 34, 30), "neuron", font=ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", 34),
           fill=rgb(tl.TEXT_FAINT))

    rows, _ = layout(t)
    top = PAD_TOP - scroll
    clip_top = PAD_TOP - 8
    clip_bot = TH - PAD_BOT - LINE_H - PROMPT_GAP + 4
    layer = Image.new("RGB", (TW, TH), rgb(tl.BG2))
    ld = ImageDraw.Draw(layer)
    for idx, y in rows:
        yy = top + y
        if yy + LINE_H < clip_top or yy > clip_bot:
            continue
        draw_line(ld, idx, PAD_X, yy, t)
        ev = tl.SCRIPT[idx].get("event")
        if ev and ev in anchors_pending and t >= tl.SCRIPT[idx]["t"] + 0.02:
            u = (PAD_X + CHAR_W * 0.5) / TW
            v = (yy + LINE_H * 0.5) / TH
            anchors[ev] = [u, v]
            anchors_pending.discard(ev)
    # Fade history out under the top edge instead of a hard cut.
    mask = Image.new("L", (TW, TH), 0)
    md = ImageDraw.Draw(mask)
    for y in range(clip_top, clip_bot):
        k = min(1.0, (y - clip_top) / 60.0)
        md.line([(0, y), (TW, y)], fill=int(255 * k))
    img.paste(layer, (0, 0), mask)

    # Prompt row.
    py = TH - PAD_BOT - LINE_H
    d.line([(PAD_X, py - PROMPT_GAP * 0.5), (TW - PAD_X, py - PROMPT_GAP * 0.5)], fill=rgb(tl.LINE), width=2)
    ty = py + (LINE_H - FONT_PX) * 0.42
    d.text((PAD_X, ty), ">", font=FONT_B, fill=rgb(tl.LISTEN, 0.85))
    idx, n = typing_line(t)
    text = tl.SCRIPT[idx]["text"][:n] if idx is not None else ""
    if text:
        d.text((PAD_X + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT))
    cx = PAD_X + CHAR_W * (2 + len(text))
    typing = idx is not None
    on = typing or (t % 1.0) < 0.55
    if on and t < tl.COLLAPSE_T:
        d.rectangle([cx, py + 12, cx + CHAR_W * 0.92, py + LINE_H - 12], fill=rgb(tl.LISTEN, 0.9))

    # Fade in from the void.
    k = ease((t - 0.05) / 0.55)
    if k < 1.0:
        img = Image.eval(img, lambda v: int(v * k))
    return img


def main():
    out = os.path.join(tl.OUT, "term")
    os.makedirs(out, exist_ok=True)
    only = set(int(a) for a in sys.argv[1:])
    scroll = scroll_series()
    pending = {l["event"] for l in tl.SCRIPT if "event" in l}
    anchors = {}
    for f in range(tl.frames()):
        img = render(f, scroll[f], pending, anchors)
        if only and (f + 1) not in only:
            continue
        img.save(os.path.join(out, f"{f + 1:05d}.png"), compress_level=1)
    with open(os.path.join(tl.OUT, "anchors.json"), "w") as fh:
        json.dump(anchors, fh, indent=1)
    print("anchors", anchors)


if __name__ == "__main__":
    main()
