# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Per-frame light for the rig: the 6x22 keyboard grid and the mouse/mic zones.

The wake, typing, profile and drop beats are visualisation, not a Neuron preset.
The `light stack add --preset aurora` beat uses a real capture when one is given
(`--capture frames.json`, schema neuron.showcase.frames.v1, recorded by
.local/showcase/tools/capture_controller.py); otherwise a stand-in field.

Output: lighting.npz with `kb` (frames, 132, 3) and zone arrays (frames, 3),
all sRGB in 0..1.
"""
import argparse
import json
import math
import os
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import timeline as tl

ROWS, COLS = 6, 22
HERE = os.path.dirname(__file__)


def hexrgb(h):
    h = h.lstrip("#")
    return np.array([int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4)])


ACCENT = hexrgb(tl.ACCENT)
LISTEN = hexrgb(tl.LISTEN)
WHITE = np.array([0.92, 0.95, 1.0])

KEYMAP = json.load(open(os.path.join(HERE, "keymap.json")))["key_map"]
CELL = {k["name"]: k["row"] * COLS + k["col"] for k in KEYMAP}
RR, CC = np.meshgrid(np.arange(ROWS), np.arange(COLS), indexing="ij")
RR = RR.reshape(-1).astype(float)
CC = CC.reshape(-1).astype(float)
# Rows are ~1 key apart, columns ~1 key apart: a ring in key units.
CENTER = (3.0, 10.0)


def key_for(ch):
    """Key names pressed for a typed char (shift first when needed)."""
    if ch.isalpha():
        return ["LSHIFT", ch.upper()] if ch.isupper() else [ch.upper()]
    if ch.isdigit():
        return [ch]
    return {
        " ": ["SPACE"], "'": ["QUOTE"], '"': ["LSHIFT", "QUOTE"], ".": ["PERIOD"],
        ",": ["COMMA"], "?": ["LSHIFT", "SLASH"], "-": ["DASH"], ":": ["LSHIFT", "SEMICOLON"],
    }.get(ch, [])


def smooth(x):
    x = np.clip(x, 0.0, 1.0)
    return x * x * (3 - 2 * x)


def ring(t, t0, r0c, speed, width, life):
    """A ring of light expanding from a cell; returns per-cell intensity."""
    dt = t - t0
    if dt < 0 or dt > life:
        return np.zeros(ROWS * COLS)
    d = np.hypot((RR - r0c[0]) * 1.15, CC - r0c[1])
    front = dt * speed
    k = np.exp(-((d - front) / width) ** 2)
    return k * (1 - dt / life) ** 1.5


def aurora_field(t, t0):
    """Stand-in aurora: slow ribbons in the brand palette."""
    x = CC / (COLS - 1)
    y = RR / (ROWS - 1)
    s = t - t0
    a = np.sin(x * 5.1 + s * 0.9 + np.sin(y * 2.0 + s * 0.6) * 1.4)
    b = np.sin(x * 2.3 - s * 0.55 + y * 1.7)
    v = 0.5 + 0.5 * (0.6 * a + 0.4 * b)
    stops = [hexrgb("#0b1440"), hexrgb("#1d5fd1"), ACCENT, hexrgb("#3fd0ff"), LISTEN]
    p = v * (len(stops) - 1)
    i = np.clip(p.astype(int), 0, len(stops) - 2)
    f = (p - i)[:, None]
    col = np.array(stops)[i] * (1 - f) + np.array(stops)[i + 1] * f
    lum = 0.55 + 0.45 * (0.5 + 0.5 * np.sin(x * 3.3 + s * 1.3 + y))
    return col * lum[:, None]


def load_capture(path):
    if not path:
        return None
    d = json.load(open(path))
    if d.get("schema") != "neuron.showcase.frames.v1" or d["rows"] != ROWS or d["cols"] != COLS:
        raise SystemExit("capture must be neuron.showcase.frames.v1 on the 6x22 grid")
    return d


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--capture", help="real frames for the aurora beat")
    ap.add_argument("--capture-offset", type=float, default=0.0, help="capture seconds at the beat's start")
    args = ap.parse_args()
    cap = load_capture(args.capture)

    n = tl.frames()
    kb = np.zeros((n, ROWS * COLS, 3))
    zones = {z: np.zeros((n, 3)) for z in ("wheel", "logo", "plate", "mic")}

    presses = []
    for t, ch in tl.typed_chars():
        for name in key_for(ch):
            if name in CELL:
                presses.append((t, CELL[name]))

    a_wake_kb = tl.arrival("wake_keyboard")
    a_profile = tl.arrival("profile")
    a_aurora = tl.arrival("aurora")
    a_drop = tl.DROP_T + 0.42
    idle = ACCENT * 0.05

    for f in range(n):
        t = f / tl.FPS
        c = np.zeros((ROWS * COLS, 3))

        # Resting state once the board has been woken.
        woke = smooth((t - a_wake_kb - 0.25) / 0.6)
        c += idle * woke

        # Wake: one accent ring from the board's centre.
        c += ring(t, a_wake_kb, CENTER, 26.0, 1.6, 0.9)[:, None] * ACCENT * 1.1

        # Profile: a clean band sweeping left to right.
        dt = t - a_profile
        if 0 <= dt < 1.0:
            band = np.exp(-((CC - dt * 30.0) / 1.4) ** 2) * (1 - dt)
            c += band[:, None] * ACCENT * 0.9

        # Aurora reveal: wipes on from the left, then holds.
        if t >= a_aurora:
            wipe = smooth((((t - a_aurora) * 28.0) - CC) / 4.0)[:, None]
            if cap is not None:
                ci = min(len(cap["frames"]) - 1, int((t - a_aurora + args.capture_offset) * cap["fps"]))
                look = np.array(cap["frames"][ci]["rgb"], dtype=float) / 255.0
            else:
                look = aurora_field(t, a_aurora)
            c = c * (1 - wipe) + look * wipe
            # The reveal's leading edge.
            edge = np.exp(-(((t - a_aurora) * 28.0 - CC) / 1.2) ** 2)[:, None]
            c += edge * WHITE * 0.5 * (t - a_aurora < 1.2)

        # Final drop: the terminal's last light lands at the centre.
        c += ring(t, a_drop, CENTER, 20.0, 1.8, 1.4)[:, None] * WHITE * 0.9

        # Reactive typing on top: listen-purple flash, faint before the wake.
        gain = 0.35 + 0.65 * woke
        for tp, cell in presses:
            d = t - tp
            if 0 <= d < 0.6:
                k = math.exp(-d / 0.16) * gain
                c[cell] += LISTEN * 1.1 * k
        kb[f] = np.clip(c, 0, 1)

        # Zones.
        def flash(t0, life=0.5):
            d = t - t0
            return math.exp(-d / (life / 3)) if 0 <= d < life * 2 else 0.0

        wm = smooth((t - tl.arrival("wake_mouse")) / 0.5)
        mouse = ACCENT * 0.07 * wm
        if t >= a_aurora:
            # The mouse sits right of the numpad: take the board's rightmost columns.
            right = kb[f].reshape(ROWS, COLS, 3)[:, 18:22].mean(axis=(0, 1)) * 1.6
            mouse = mouse * (1 - smooth((t - a_aurora - 0.6) / 0.5)) + right * smooth((t - a_aurora - 0.6) / 0.5)
        zones["wheel"][f] = mouse + ACCENT * (flash(tl.arrival("wake_mouse")) + flash(tl.arrival("dpi"), 0.7))
        zones["logo"][f] = mouse + ACCENT * flash(tl.arrival("radial"), 0.8)
        zones["plate"][f] = mouse * 0.8 + ACCENT * flash(tl.arrival("mute"), 0.6) + LISTEN * 0.0

        wmic = smooth((t - tl.arrival("wake_mic")) / 0.5)
        mic = ACCENT * 0.10 * wmic + ACCENT * flash(tl.arrival("wake_mic")) + ACCENT * flash(tl.arrival("mute") + 0.45, 0.7)
        if t >= a_aurora:
            left = kb[f].reshape(ROWS, COLS, 3)[:, 0:4].mean(axis=(0, 1)) * 1.6
            mic = mic * (1 - smooth((t - a_aurora - 0.4) / 0.5)) + left * smooth((t - a_aurora - 0.4) / 0.5)
        zones["mic"][f] = mic

    out = os.path.join(tl.OUT, "lighting.npz")
    os.makedirs(tl.OUT, exist_ok=True)
    np.savez_compressed(out, kb=np.clip(kb, 0, 1).astype(np.float32),
                        **{k: np.clip(v, 0, 1).astype(np.float32) for k, v in zones.items()},
                        presses=np.array(presses, dtype=np.float64))
    print("wrote", out, kb.shape, "capture" if cap else "stand-in aurora")


if __name__ == "__main__":
    main()
