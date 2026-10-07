# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Render the terminal pane's texture, one PNG per frame, plus anchors.json.

anchors.json maps each rig event to the (u, v) of the line that caused it at the moment it fires,
so a light thread in the scene leaves from that exact line. The line that is acting carries an
accent bar until its effect has landed, so the eye can follow line -> thread -> device.
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
LINE_H = 78
GROUP_GAP = 26
PROMPT_GAP = 30
FONT = ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", FONT_PX)
FONT_B = ImageFont.truetype(r"C:\Windows\Fonts\consolab.ttf", FONT_PX)
FONT_TAB = ImageFont.truetype(r"C:\Windows\Fonts\consola.ttf", 34)
CHAR_W = FONT.getlength("M")


def rgb(h, a=1.0):
    h = h.lstrip("#")
    return tuple(int(round(int(h[i:i + 2], 16) * a)) for i in (0, 2, 4))


def mix(a, b, k):
    return tuple(int(round(x + (y - x) * k)) for x, y in zip(a, b))


def ease(k):
    k = max(0.0, min(1.0, k))
    return k * k * (3 - 2 * k)


USER_TIMES = {idx: tl.line_times(line) for idx, line in enumerate(tl.SCRIPT) if line["kind"] in ("user", "voice")}


def commit_time(idx):
    line = tl.SCRIPT[idx]
    if line["kind"] in ("user", "voice"):
        return tl.commit_time(line)
    return line["t"]


def speaker(kind):
    return {"user": "user", "voice": "user", "agent": "agent"}.get(kind, "tool")


def indent(kind):
    return {"user": 2, "voice": 2, "agent": 0, "tool": 2, "out": 2, "cont": 4}[kind]


def waveform(d, x, y, t, live, bars=7):
    """Speech bars beside a spoken line: moving while it is heard, still once it is text."""
    w = CHAR_W * 0.26
    for i in range(bars):
        if live:
            a = 0.25 + 0.75 * abs(math.sin(t * (7.0 + i * 1.7) + i * 1.3)) * (0.6 + 0.4 * math.sin(t * 3.1 + i))
        else:
            a = (0.35, 0.6, 0.9, 0.55, 0.75, 0.4, 0.3)[i % 7]
        h = LINE_H * 0.62 * max(0.12, a)
        cx = x + i * w * 1.9
        cy = y + LINE_H * 0.5
        d.rounded_rectangle([cx, cy - h / 2, cx + w, cy + h / 2], radius=w / 2,
                            fill=rgb(tl.LISTEN, 0.9 if live else 0.45))


def rows_of(idx):
    line = tl.SCRIPT[idx]
    return tl.wrap(line["text"], tl.WRAP - indent(line["kind"]) + (0 if line["kind"] != "agent" else 2))


def owner_tool(idx):
    """The tool line a cont/out row belongs to, for shared highlighting."""
    j = idx
    while j > 0 and tl.SCRIPT[j]["kind"] in ("cont", "out") and "event" not in tl.SCRIPT[j]:
        j -= 1
    return j


def highlight(idx, t):
    """0..1 accent highlight for a script line at time t."""
    src = tl.SCRIPT[idx] if "event" in tl.SCRIPT[idx] else tl.SCRIPT[owner_tool(idx)]
    if "event" not in src:
        return 0.0
    start = src["t"]
    end = start + tl.THREAD_TRAVEL + 0.5
    if t < start:
        return 0.0
    return ease((t - start) / 0.12) * (1 - ease((t - end) / 0.6))


def layout(t):
    """[(idx, row_index, y)] for committed history at t, and content height."""
    out = []
    y = 0.0
    prev = None
    for idx, line in enumerate(tl.SCRIPT):
        if commit_time(idx) > t:
            continue
        sp = speaker(line["kind"])
        if prev is not None and sp != prev and line["kind"] not in ("cont", "out"):
            y += GROUP_GAP
        for r, _ in enumerate(rows_of(idx)):
            out.append((idx, r, y))
            y += LINE_H
        prev = sp
    return out, y


VIEW_H = TH - PAD_TOP - PAD_BOT - LINE_H - PROMPT_GAP


def scroll_series():
    n = tl.frames()
    dt = 1.0 / tl.FPS
    pos, vel = 0.0, 0.0
    omega = 14.0
    out = []
    for f in range(n):
        x = max(0.0, layout(f * dt)[1] - VIEW_H)
        acc = omega * omega * (x - pos) - 2 * omega * vel
        vel += acc * dt
        pos += vel * dt
        out.append(pos)
    return out


def row_text(idx, r, t):
    line = tl.SCRIPT[idx]
    rows = rows_of(idx)
    if line["kind"] != "agent":
        return rows[r]
    shown = int((t - line["t"]) * tl.STREAM_CPS) + 1
    before = sum(len(x) + 1 for x in rows[:r])
    return rows[r][:max(0, shown - before)]


def draw_row(d, idx, r, y, t):
    line = tl.SCRIPT[idx]
    kind = line["kind"]
    age = t - commit_time(idx)
    a = ease(age / 0.14) if kind in ("tool", "out", "cont") else 1.0
    dx = (1 - a) * 14
    x = PAD_X + dx
    ty = y + (LINE_H - FONT_PX) * 0.42
    text = row_text(idx, r, t)
    h = highlight(idx, t)
    if h > 0:
        bg = mix(rgb(tl.BG2), rgb(tl.ACCENT), 0.10 * h)
        d.rectangle([PAD_X - 34, y + 4, TW - PAD_X + 20, y + LINE_H - 4], fill=bg)
        d.rectangle([PAD_X - 34, y + 4, PAD_X - 28, y + LINE_H - 4], fill=mix(rgb(tl.BG2), rgb(tl.ACCENT), h))
    if kind in ("user", "voice"):
        if r == 0:
            d.text((x, ty), ">", font=FONT_B, fill=rgb(tl.LISTEN, 0.85))
        d.text((x + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT, 0.92))
        if kind == "voice" and r == len(rows_of(idx)) - 1:
            waveform(d, x + CHAR_W * (3 + len(text)), y, t, live=False)
    elif kind == "agent":
        d.text((x, ty), text, font=FONT, fill=rgb(tl.TEXT))
    elif kind == "tool":
        if r == 0:
            flare = 1.0 + 0.6 * math.exp(-max(0.0, age) / 0.3)
            rad = FONT_PX * 0.17 * flare
            cx, cy = x + CHAR_W * 0.5, y + LINE_H * 0.5
            d.ellipse([cx - rad, cy - rad, cx + rad, cy + rad], fill=rgb(tl.ACCENT, a))
        col = mix(rgb(tl.TEXT_MID, a), rgb(tl.TEXT), h * 0.8)
        d.text((x + CHAR_W * 2, ty), text, font=FONT, fill=col)
    elif kind == "cont":
        d.text((x + CHAR_W * 4, ty), text, font=FONT, fill=mix(rgb(tl.TEXT_DIM, a), rgb(tl.TEXT_MID), h))
    elif kind == "out":
        ind = 2 if r == 0 else 4
        d.text((x + CHAR_W * ind, ty), text, font=FONT, fill=mix(rgb(tl.TEXT_DIM, a), rgb(tl.TEXT_MID), h))


def typing_line(t):
    for idx, times in USER_TIMES.items():
        start = tl.SCRIPT[idx]["t"] if tl.SCRIPT[idx]["kind"] == "voice" else times[0]
        if start - 0.001 <= t < commit_time(idx):
            return idx, sum(1 for ct in times if ct <= t)
    return None, 0


def render(f, scroll, pending, anchors):
    t = f / tl.FPS
    img = Image.new("RGB", (TW, TH), rgb(tl.BG2))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle([3, 3, TW - 4, TH - 4], radius=34, outline=rgb(tl.LINE), width=4)
    d.rectangle([PAD_X, 44, PAD_X + 16, 60], fill=rgb(tl.ACCENT, 0.9))
    d.text((PAD_X + 34, 30), tl.AGENT_LABEL, font=FONT_TAB, fill=rgb(tl.TEXT_FAINT))

    rows, _ = layout(t)
    top = PAD_TOP - scroll
    clip_top = PAD_TOP - 8
    clip_bot = TH - PAD_BOT - LINE_H - PROMPT_GAP + 4
    layer = Image.new("RGB", (TW, TH), rgb(tl.BG2))
    ld = ImageDraw.Draw(layer)
    for idx, r, y in rows:
        yy = top + y
        if yy + LINE_H < clip_top or yy > clip_bot:
            continue
        draw_row(ld, idx, r, yy, t)
        ev = tl.SCRIPT[idx].get("event")
        if ev and r == 0 and ev in pending and t >= tl.SCRIPT[idx]["t"] + 0.02:
            anchors[ev] = [(PAD_X + CHAR_W * 0.5) / TW, (yy + LINE_H * 0.5) / TH]
            pending.discard(ev)
    mask = Image.new("L", (TW, TH), 0)
    md = ImageDraw.Draw(mask)
    for y in range(clip_top, clip_bot):
        md.line([(0, y), (TW, y)], fill=int(255 * min(1.0, (y - clip_top) / 70.0)))
    img.paste(layer, (0, 0), mask)

    # Prompt row: tinted while you type, so the eye knows who is talking.
    py = TH - PAD_BOT - LINE_H
    idx, n = typing_line(t)
    typing = idx is not None
    if typing:
        d.rectangle([PAD_X - 34, py - 6, TW - PAD_X + 20, py + LINE_H + 2], fill=mix(rgb(tl.BG2), rgb(tl.LISTEN), 0.07))
    d.line([(PAD_X, py - PROMPT_GAP * 0.5), (TW - PAD_X, py - PROMPT_GAP * 0.5)], fill=rgb(tl.LINE), width=2)
    ty = py + (LINE_H - FONT_PX) * 0.42
    d.text((PAD_X, ty), ">", font=FONT_B, fill=rgb(tl.LISTEN, 0.85))
    text = tl.SCRIPT[idx]["text"][:n] if typing else ""
    if len(text) > tl.WRAP - 2:
        text = text[-(tl.WRAP - 2):]
    if text:
        d.text((PAD_X + CHAR_W * 2, ty), text, font=FONT, fill=rgb(tl.TEXT))
    cx = PAD_X + CHAR_W * (2 + len(text))
    if typing and tl.SCRIPT[idx]["kind"] == "voice":
        waveform(d, PAD_X + CHAR_W * (3 + len(text)), py, t, live=True)
    elif (typing or (t % 1.0) < 0.55) and t < tl.COLLAPSE_T:
        d.rectangle([cx, py + 12, cx + CHAR_W * 0.92, py + LINE_H - 12], fill=rgb(tl.LISTEN, 0.9))

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
    missing = pending
    print("anchors", len(anchors), "missing", sorted(missing))


if __name__ == "__main__":
    main()
