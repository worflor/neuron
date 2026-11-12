# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
"""Regenerate the reference vectors for the learned colour engine (src/assist/learned.rs).

    python colour-model.py <luma checkout>

Builds a small random-weight palette core, then uses Luma's own bridge tools to write it as a
bundle and to record what the Python runtime answers for its refine requests. The results land
beside this script:

    colour-model.golden.lumacore   the small core, in the bundle format Neuron loads
    colour-model.golden.jsonl      the requests and the runtime's answers

The Rust port is tested against these, so its arithmetic is checked without trained weights in
this repository. A trained bundle is checked the same way against Luma's own vectors; see the
`matches_luma_vectors` test.
"""

import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent


def tiny(folder):
    """A layout 3 core small enough to commit, with margins loose enough that both answers occur."""
    rng = np.random.default_rng(20261112)
    margins = {"spoil": -8.0, "keep": -5.0}
    config = {"width": 16, "heads": 2, "blocks": 2, "components": 3, "sigma": [.008, .25], "merge": .02,
              "order": "weight", "layout": 3,
              "policy": {"measure": "density", "joint": "pseudo", "duplicate": .02,
                         "masked": margins, "refine": margins, "follow": margins}}
    width, k = config["width"], config["components"]
    shapes = {"embed.0.weight": (width, 40), "embed.0.bias": (width,), "embed.2.weight": (width, width),
              "embed.2.bias": (width,)}
    for i in range(config["blocks"]):
        p = f"blocks.{i}."
        shapes.update({p + "norm1.weight": (width,), p + "norm1.bias": (width,), p + "qkv.weight": (3 * width, width),
                       p + "qkv.bias": (3 * width,), p + "proj.weight": (width, width), p + "proj.bias": (width,),
                       p + "norm2.weight": (width,), p + "norm2.bias": (width,), p + "wide.weight": (2 * width, width),
                       p + "wide.bias": (2 * width,), p + "narrow.weight": (width, 2 * width), p + "narrow.bias": (width,)})
    shapes.update({"norm.weight": (width,), "norm.bias": (width,), "head.0.weight": (width, width), "head.0.bias": (width,),
                   "head.2.weight": (7 * k + 4, width), "head.2.bias": (7 * k + 4,)})
    weights = {key: (rng.normal(size=shape) * (.5 if len(shape) == 2 else .2)
                     + (1. if "norm" in key and key.endswith("weight") else 0.)).astype(np.float32)
               for key, shape in shapes.items()}
    np.savez(folder / "tiny.npz", **weights)
    digest = hashlib.sha256((folder / "tiny.npz").read_bytes()).hexdigest()
    manifest = folder / "tiny.json"
    manifest.write_text(json.dumps({"schema": "luma.palette-core.v1", "name": "tiny", "weights": "tiny.npz",
                                    "sha256": digest, "config": config}), encoding="utf-8")
    return manifest


def main(luma):
    luma = Path(luma).resolve()
    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        manifest = tiny(folder)
        run = lambda *args: subprocess.run([sys.executable, *map(str, args)], cwd=luma, check=True)
        run(luma / "tools" / "export_core_bridge.py", manifest, "--output", folder / "bridge")
        run(luma / "tools" / "verify_core_bridge.py", "--manifest", manifest, "--golden", folder / "all.jsonl", "--write")
        bundle, = (folder / "bridge").glob("*.lumacore")
        (HERE / "colour-model.golden.lumacore").write_bytes(bundle.read_bytes())
        kept, keeps = [], 0
        for line in (folder / "all.jsonl").read_text(encoding="utf-8").splitlines():
            row = json.loads(line)
            if row["task"] != "refine":
                continue
            active = row["input"]["active"]
            answer = row["output"]
            keeps += answer["keep"]
            kept.append({"id": row["id"], "input": row["input"], "offerable": answer["offerable"], "keep": answer["keep"],
                         "options": [{"rgb": option["rgb"][active], "score": option["score"]} for option in answer["options"]]})
    with open(HERE / "colour-model.golden.jsonl", "w", encoding="utf-8", newline="\n") as out:
        for row in kept:
            out.write(json.dumps(row, separators=(",", ":")) + "\n")
    print(f"{len(kept)} refine requests, {keeps} keep, {sum(len(row['options']) for row in kept)} options")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
