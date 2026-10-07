# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Light the rig with Neuron's own pattern engine.

Builds a script from timeline.py (which preset is applied when, and every keystroke), runs
../pattern-export (neuron-core's Compositor, keys fed through capture::script_key_reads) and packs
the result into lighting.npz: `kb` (frames, 132, 3), `mouse` (frames, 3, 3) as sRGB 0..1, and
`presses` [(down, up, cell)] for key travel and sound.
"""
import json
import os
import subprocess
import sys

import numpy as np

sys.path.insert(0, os.path.dirname(__file__))
import timeline as tl

HERE = os.path.dirname(os.path.abspath(__file__))
EXPORTER = os.environ.get("PROMO_EXPORTER", r"D:\build-cache\promo\target\release\promo-pattern-export.exe")
ROWS, COLS = 6, 22

KEYMAP = json.load(open(os.path.join(HERE, "keymap.json")))["key_map"]
CELL = {k["name"]: k["row"] * COLS + k["col"] for k in KEYMAP}

VK = {**{c: ord(c) for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"},
      "SPACE": 0x20, "LSHIFT": 0xA0, "QUOTE": 0xDE, "PERIOD": 0xBE, "COMMA": 0xBC, "SLASH": 0xBF,
      "SEMICOLON": 0xBA, "DASH": 0xBD, "ENTER": 0x0D}


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


def presses():
    """[(down, up, name)] for every key the script presses, with Shift held around its partner."""
    out = []
    for t, ch in tl.typed_chars():
        names = key_for(ch)
        if names[:1] == ["LSHIFT"]:
            out.append((t - 0.03, t + 0.09, "LSHIFT"))
            names = names[1:]
        for name in names:
            out.append((t, t + 0.065, name))
    return out


def main():
    n = tl.frames()
    keys = []
    for down, up, name in presses():
        keys.append([tl.frame(down) - 1, VK[name], True])
        keys.append([max(tl.frame(up) - 1, tl.frame(down)), VK[name], False])
    script = {
        "fps": tl.FPS, "frames": n,
        "grids": [{"name": "kb", "rows": ROWS, "cols": COLS}, {"name": "mouse", "rows": 1, "cols": 3}],
        "segments": [{"frame": tl.frame(t) - 1, "preset": p} for t, p in tl.light_segments()],
        "keys": keys,
    }
    os.makedirs(tl.OUT, exist_ok=True)
    spath = os.path.join(tl.OUT, "pattern-script.json")
    json.dump(script, open(spath, "w"))
    raw = os.path.join(tl.OUT, "patterns")
    subprocess.run([EXPORTER, spath, raw], check=True)
    kb = np.fromfile(os.path.join(raw, "kb.bin"), np.uint8).reshape(n, ROWS * COLS, 3) / 255.0
    mouse = np.fromfile(os.path.join(raw, "mouse.bin"), np.uint8).reshape(n, 3, 3) / 255.0
    pr = np.array([(d, u, CELL[name]) for d, u, name in presses() if name in CELL], dtype=np.float64)
    out = os.path.join(tl.OUT, "lighting.npz")
    np.savez_compressed(out, kb=kb.astype(np.float32), mouse=mouse.astype(np.float32), presses=pr)
    print("wrote", out, "segments", tl.light_segments())


if __name__ == "__main__":
    main()
