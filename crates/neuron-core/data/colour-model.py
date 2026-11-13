# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
"""Regenerate the reference vectors for the learned colour engine (src/assist/learned.rs).

    python colour-model.py <luma checkout>

Builds a small random-weight core (layout 5, revision 3 runtime), then uses Luma's own bridge tools
to write it as a bundle and to record what the Python runtime answers for requests on a gradient.
The results land beside this script:

    colour-model.golden.lumacore   the small core, in the bundle format Neuron loads
    colour-model.golden.jsonl      the requests and the runtime's answers

The Rust port is tested against these, so its arithmetic is checked without trained weights in
this repository. A trained bundle is checked the same way against Luma's own vectors; see the
`matches_luma_vectors` test. Use a clean checkout of a pushed Luma commit.
"""

import hashlib
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
ASKS = ("lighter", "darker", "warmer", "cooler", "more chroma", "less chroma")


def tiny(folder):
    """A core small enough to commit, with margins loose enough that every task offers and keeps."""
    rng = np.random.default_rng(20261113)
    margins = {"spoil": -8.0, "keep": -3.0, "visible": .03}
    joint = {"spoil": -60.0, "keep": -25.0}
    config = {"width": 16, "heads": 2, "blocks": 2, "components": 3, "sigma": [.008, .25], "merge": .02,
              "order": "weight", "layout": 5, "cone": .5, "style": 4, "revision": 3, "favour": .5,
              "policy": {"measure": "density", "joint": "pseudo", "duplicate": .02,
                         "masked": margins, "refine": margins, "intent": margins, "follow": joint,
                         "repair": {"stop": .999, "evidence": "keep", "spoil": -14.0, "keep": -6.0, "visible": .03}}}
    width, k, style, lanes = config["width"], config["components"], config["style"], 8
    shapes = {"embed.0": (width, 87 + 5 + style + 1), "embed.2": (width, width),
              "bias.0": (lanes, 14), "bias.2": (config["blocks"] * config["heads"], lanes),
              "head.0": (width, width), "head.2": (7 * k + 4, width),
              "style.token.0": (lanes, 4), "style.token.2": (lanes, lanes), "style.query": (1, lanes),
              "style.palette": (lanes, 2 * lanes), "style.pick": (1, lanes), "style.out": (style, 2 * lanes)}
    for i in range(config["blocks"]):
        shapes.update({f"blocks.{i}.qkv": (3 * width, width), f"blocks.{i}.proj": (width, width),
                       f"blocks.{i}.wide": (2 * width, width), f"blocks.{i}.narrow": (width, 2 * width)})
    weights = {}
    for name, shape in shapes.items():
        weights[name + ".weight"] = rng.normal(size=shape) * .5
        weights[name + ".bias"] = rng.normal(size=shape[:1]) * .2
    for name in ["norm"] + [f"blocks.{i}.norm{j}" for i in range(config["blocks"]) for j in (1, 2)]:
        weights[name + ".weight"] = 1. + rng.normal(size=width) * .2
        weights[name + ".bias"] = rng.normal(size=width) * .2
    np.savez(folder / "tiny.npz", **{key: value.astype(np.float32) for key, value in weights.items()})
    digest = hashlib.sha256((folder / "tiny.npz").read_bytes()).hexdigest()
    manifest = folder / "tiny.json"
    manifest.write_text(json.dumps({"schema": "luma.palette-core.v1", "name": "tiny", "weights": "tiny.npz",
                                    "sha256": digest, "config": config}), encoding="utf-8")
    return manifest


def main(luma):
    luma = Path(luma).resolve()
    sys.path[:0] = [str(luma), str(luma / "tools")]
    import verify_core_bridge as v2
    import verify_core_bridge_v4 as v4
    from luma.color import oklab
    from luma.palette_core import PaletteCore, intent_vector

    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        manifest = tiny(folder)
        subprocess.run([sys.executable, str(luma / "tools" / "export_core_bridge.py"), str(manifest),
                        "--output", str(folder / "bridge")], cwd=luma, check=True)
        bundle, = (folder / "bridge").glob("*.lumacore")
        (HERE / "colour-model.golden.lumacore").write_bytes(bundle.read_bytes())
        core = PaletteCore(str(manifest))
        rows, tally = [], {}
        for number, case in enumerate(v2.cases()[::2]):
            rng = np.random.default_rng([20261113, number])
            inp = dict(case["input"])
            # Neuron's stops all carry weight; a request that hides one is not a request it makes.
            if inp.get("weights") is not None and min(inp["weights"]) <= 0:
                continue
            intent = None
            if inp["task"] in ("refine", "repair") and rng.random() < .5:
                about = oklab(np.asarray(inp["rgb"][inp["active"] if inp["active"] is not None else 0]))
                intent = intent_vector(ASKS[int(rng.integers(len(ASKS)))], float(rng.uniform(.015, .09)), about).tolist()
            support = None
            if rng.random() < .25:
                support = [oklab(v2.palette(rng, int(rng.integers(2, 13)), str(rng.choice(("ramp", "diverging", "random", "vivid"))))).tolist()
                           for _ in range(int(rng.integers(1, 9)))]
            inp.update(weights=None, geometry={"kind": "ordered_gradient"}, intent=intent, support_lab=support)
            checkpoint, output = v4.evaluate(core, inp)
            entry = tally.setdefault(inp["task"], [0, 0, 0])
            entry[0] += 1
            entry[1] += output["keep"]
            entry[2] += len(output["options"])
            rows.append({"id": case["id"], "input": inp, "style": checkpoint and checkpoint["style"],
                         "output": {"offerable": output["offerable"], "keep": output["keep"],
                                    "keep_probability": output.get("keep_probability"),
                                    "options": [{"rgb": option["rgb"], "score": option["score"]} for option in output["options"]]}})
    with open(HERE / "colour-model.golden.jsonl", "w", encoding="utf-8", newline="\n") as out:
        for row in rows:
            out.write(json.dumps(row, separators=(",", ":"), allow_nan=False) + "\n")
    for task, (count, keeps, options) in tally.items():
        print(f"{task}: {count} requests, {keeps} keep, {options} options")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
