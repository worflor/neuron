# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Short looping effect clips for the README: the real pattern engine on the generic keyboard fixture.

Each clip is a preset (optionally one of its suggested pairings, the same chip the lighting page
offers) rendered offline by ../pattern-export, written as a `neuron.showcase.frames.v1` file and
played through `.local/showcase/tools/play_frames.py` (Blender Cycles). Nothing else paints the keys.
Live-input effects are fed a typed sentence through `capture::script_key_reads`; nothing is
synthesized. The keyboard is a generic stand-in, not a scan, and the optics are an estimate.

    python gen_effects.py preview            # one still per clip, to check framing
    python gen_effects.py render [clip ...]  # export, render, encode docs/media/effect-<clip>.webp

Output (frames, PNG sequences) goes to PROMO_OUT, never into the repo; only the WebP loops do.
"""
import json
import os
import subprocess
import sys
from pathlib import Path

from PIL import Image

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
SHOWCASE = REPO / ".local" / "showcase"
OUT = Path(os.environ.get("PROMO_OUT", r"D:\build-cache\promo\effects"))
EXPORTER = Path(os.environ.get("PROMO_EXPORTER", r"D:\build-cache\promo\target\release\promo-pattern-export.exe"))
PYTHON = Path(r"C:\Users\Micha\AppData\Local\Programs\Python\Python311\python.exe")
MEDIA = REPO / "docs" / "media"
ROWS, COLS, FPS = 6, 22, 24
LOOP, BLEND = 72, 12  # frames in the loop, frames cross-faded to close it
SENTENCE = "every key leaves a little warmth"

# slug -> (preset, pairing index or None, warm-up seconds, search seconds, typed sentence or None)
# After the warm-up the clip is the busiest LOOP+BLEND window within the next `search` seconds, so a
# sparse effect (comets, ripples) is not shown mid-lull.
CLIPS = {
    "fire-aurora": ("fire", 0, 4.0, 0.0, None),
    "cascade-bubble": ("cascade", 0, 3.0, 0.0, None),
    "ripple-bubble": ("ripple", 0, 0.5, 2.0, SENTENCE),
    "typing-heat-rainbow": ("typingheat", 0, 0.5, 2.0, SENTENCE),
}

VK = {c: ord(c) for c in "ABCDEFGHIJKLMNOPQRSTUVWXYZ"} | {" ": 0x20}


def keys_for(sentence, start_s, cps=7.0):
    keys = []
    for i, ch in enumerate(sentence.upper()):
        vk = VK.get(ch)
        if vk is None:
            continue
        down = round((start_s + i / cps) * FPS)
        keys.append([down, vk, True])
        keys.append([down + 2, vk, False])
    return keys


def export(slug):
    preset, pairing, warm, search, sentence = CLIPS[slug]
    warm_f = round(warm * FPS)
    total = warm_f + round(search * FPS) + LOOP + BLEND
    seg = {"frame": 0, "preset": preset}
    if pairing is not None:
        seg["pairing"] = pairing
    keys = keys_for(sentence, 0.2) if sentence else []
    # a sentence that outlasts the warm-up keeps typing through the loop; shift it so the loop is busy
    work = OUT / slug
    work.mkdir(parents=True, exist_ok=True)
    script = {"fps": FPS, "frames": total, "grids": [{"name": "kb", "rows": ROWS, "cols": COLS}],
              "segments": [seg], "keys": keys}
    (work / "script.json").write_text(json.dumps(script))
    subprocess.run([str(EXPORTER), str(work / "script.json"), str(work)], check=True)
    raw = (work / "kb.bin").read_bytes()
    n = ROWS * COLS
    assert len(raw) == total * n * 3
    key_map = json.loads((SHOWCASE / "frames" / "wildlife-live-v0.1.4-32-40s.json").read_text())["key_map"]
    levels = [sum(raw[i * n * 3:(i + 1) * n * 3]) for i in range(total)]
    span = LOOP + BLEND
    start = max(range(warm_f, total - span + 1), key=lambda a: sum(levels[a:a + span]))
    frames = []
    for i in range(start, start + span):
        px = raw[i * n * 3:(i + 1) * n * 3]
        frames.append({"t": (i - start) / FPS, "rgb": [list(px[j * 3:j * 3 + 3]) for j in range(n)]})
    data = {"schema": "neuron.showcase.frames.v1", "source_preset": slug, "rows": ROWS, "cols": COLS, "fps": FPS,
            "duration_seconds": len(frames) / FPS, "key_map": key_map,
            "key_map_source": "neuron::lighting::razer_key_cell",
            "source": "neuron-core pattern::Compositor via artifacts/promo/pattern-export",
            "frames": frames}
    path = work / "frames.json"
    path.write_text(json.dumps(data, separators=(",", ":")))
    return path


def play(frames_json, out_dir, preview=None, resolution=640, samples=12):
    cmd = [str(PYTHON), str(SHOWCASE / "tools" / "play_frames.py"), str(frames_json), "--output", str(out_dir),
           "--samples", str(samples), "--resolution", str(resolution), "--camera", "hero", "--bake-animation"]
    if preview is not None:
        cmd += ["--preview-only", "--preview-index", str(preview)]
    subprocess.run(cmd, check=True)


def encode(slug, width=420, quality=62):
    seq = sorted((OUT / slug / "render" / "sequence-0s").glob("wildlife_*.png"))
    assert len(seq) == LOOP + BLEND, len(seq)
    imgs = [Image.open(p).convert("RGB") for p in seq]
    size = (width, round(imgs[0].height * width / imgs[0].width))
    imgs = [im.resize(size, Image.LANCZOS) for im in imgs]
    loop = []
    for i in range(LOOP):
        if i < BLEND:
            a = i / BLEND
            loop.append(Image.blend(imgs[LOOP + i], imgs[i], a))
        else:
            loop.append(imgs[i])
    dest = MEDIA / f"effect-{slug}.webp"
    loop[0].save(dest, save_all=True, append_images=loop[1:], duration=round(1000 / FPS), loop=0,
                 quality=quality, method=6)
    print(f"{dest.name}: {dest.stat().st_size / 1024:.0f} KB")


def main():
    mode = sys.argv[1] if len(sys.argv) > 1 else "preview"
    names = sys.argv[2:] or list(CLIPS)
    for slug in names:
        path = export(slug)
        render = OUT / slug / "render"
        if mode == "preview":
            play(path, render, preview=(LOOP // 2), resolution=900)
        else:
            play(path, render)
            encode(slug)


if __name__ == "__main__":
    main()
