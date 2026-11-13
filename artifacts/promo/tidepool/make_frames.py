# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Tidepool LED frames for the cinematic, from Neuron's own engine via artifacts/promo/pattern-export.

Writes the exporter script, runs it, and converts kb.bin into a neuron.showcase.frames.v1 file for
.local/showcase/tools/render_showcase.py. No keys are pressed: the board shows ambient Tidepool.
"""
import json, os, subprocess, sys, hashlib

ROOT = "C:/Users/Micha/Documents/Projects/neuron"
HERE = os.environ.get("PROMO_OUT", "D:/build-cache/promo/tidepool")
os.makedirs(HERE, exist_ok=True)
FPS, WARMUP, SECONDS = 30, 3, 12
TOTAL = (WARMUP + SECONDS) * FPS
EXPORTER = "D:/build-cache/promo/target/release/promo-pattern-export.exe"

script = {"fps": FPS, "frames": TOTAL, "grids": [{"name": "kb", "rows": 6, "cols": 22}],
          "segments": [{"frame": 0, "preset": "tidepool"}], "keys": []}
script_path = os.path.join(HERE, "pattern-script.json")
json.dump(script, open(script_path, "w"), indent=1)
out = os.path.join(HERE, "engine")
os.makedirs(out, exist_ok=True)
exe = EXPORTER if os.path.exists(EXPORTER) else EXPORTER.replace("promo-pattern-export", "pattern-export")
subprocess.run([exe, script_path, out], check=True)

raw = open(os.path.join(out, "kb.bin"), "rb").read()
cells = 6 * 22
assert len(raw) == TOTAL * cells * 3, (len(raw), TOTAL)
key_map = json.load(open(f"{ROOT}/.local/showcase/frames/wildlife-live-v0.1.4-32-40s.json"))["key_map"]
frames = []
for i in range(WARMUP * FPS, TOTAL):
    base = i * cells * 3
    rgb = [list(raw[base + c * 3: base + c * 3 + 3]) for c in range(cells)]
    frames.append({"t": (i - WARMUP * FPS) / FPS, "rgb": rgb})
doc = {
    "schema": "neuron.showcase.frames.v1", "source_preset": "tidepool", "rows": 6, "cols": 22, "fps": FPS,
    "duration_seconds": float(SECONDS), "key_map_source": "neuron::lighting::razer_key_cell", "key_map": key_map,
    "source": "neuron-core pattern::Compositor via artifacts/promo/pattern-export (offline, no device)",
    "live_input": "none: no keys pressed (capture::script_key_reads, all up)",
    "playback_speed": 1,
    "engine_sha256": hashlib.sha256(raw).hexdigest(),
    "frames": frames,
}
dst = os.path.join(HERE, "tidepool-frames.json")
json.dump(doc, open(dst, "w"))
print("frames", len(frames), "->", dst)
