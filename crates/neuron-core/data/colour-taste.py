# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
# Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.
"""Regenerate the reference vectors for the taste head (src/assist/learned/taste.rs).

    python colour-taste.py <luma checkout>

Builds the same small random-weight core as colour-model.py, a small random-weight taste head bound
to it and a random person, then uses Luma's own bridge tool to write the head as a bundle and to
record what the Python runtime answers for every second request in colour-model.golden.jsonl (and a
profile copy of some refines). The results land beside this script:

    colour-taste.golden.lumataste        the small head, in the bundle format Neuron loads
    colour-taste.golden.jsonl            per request: the readings, the scores, the personal term and the
                                         decision; every eighth also both token groups and each member's logits
    colour-taste.golden-journal.json     a picking journal and the person's weights Luma fits from it

Use a clean checkout of a pushed Luma commit.
"""

import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent


def head(folder, core):
    """A two-member head small enough to commit, with a policy loose enough that it offers and keeps."""
    rng = np.random.default_rng(20261119)
    width, hidden, members = 8, 8, 2
    shapes = {"in_core": (width, 21), "in_request": (width, 22), "embed": (width, width),
              "head_in": (hidden, 3 * width), "head_out": (3, hidden)}
    weights = {}
    for m in range(members):
        for name, shape in shapes.items():
            weights[f"m{m}.{name}.weight"] = rng.normal(size=shape) * .4
            weights[f"m{m}.{name}.bias"] = rng.normal(size=shape[:1]) * .2
        weights[f"m{m}.norm.weight"] = 1. + rng.normal(size=width) * .2
        weights[f"m{m}.norm.bias"] = rng.normal(size=width) * .2
    np.savez(folder / "tiny-taste.npz", **{k: v.astype(np.float32) for k, v in weights.items()})
    digest = hashlib.sha256((folder / "tiny-taste.npz").read_bytes()).hexdigest()
    manifest = folder / "tiny-taste.json"
    manifest.write_text(json.dumps({
        "schema": "luma.taste-head.v1", "name": "tiny-taste", "weights": "tiny-taste.npz", "sha256": digest,
        "members": members, "config": {"groups": ["core", "request"], "width": width, "heads": 2, "blocks": 0, "hidden": hidden},
        "policy": {"margin": -.2, "acceptable": .3, "visible": {t: .03 for t in ("refine", "follow", "repair", "profile")}},
        "core": {"manifest": core.name, "sha256": json.loads(core.read_text())["sha256"], "name": "tiny"}}), encoding="utf-8")
    store = folder / "tiny-people.json"
    store.write_text(json.dumps({"schema": "luma.personal.v1", "name": "tiny-people", "scale": [.05] * 8, "ridge": 3., "weight": 1.,
                                 "people": {"0000000000000001": {"weights": rng.normal(size=8).round(6).tolist()},
                                            "0000000000000002": {"weights": rng.normal(size=8).round(6).tolist()}}}), encoding="utf-8")
    return manifest, store


def main(luma):
    luma = Path(luma).resolve()
    spec = importlib.util.spec_from_file_location("colour_model", HERE / "colour-model.py")
    model = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(model)
    with tempfile.TemporaryDirectory() as folder:
        folder = Path(folder)
        core = model.tiny(folder)
        manifest, store = head(folder, core)
        out = folder / "bridge"
        # every second request of the core's vectors: each task and outcome is still covered many times
        requests = folder / "requests.jsonl"
        lines = (HERE / "colour-model.golden.jsonl").read_text(encoding="utf-8").splitlines()
        requests.write_text("\n".join(lines[::2]) + "\n", encoding="utf-8")
        subprocess.run([sys.executable, str(luma / "tools" / "export_taste_bridge.py"), str(manifest), "--personal", str(store),
                        "--core-golden", str(requests), "--output", str(out),
                        "--checkpoint-every", "8"], cwd=luma, check=True)
        (HERE / "colour-taste.golden.lumataste").write_bytes((out / "tiny-taste.lumataste").read_bytes())
        (HERE / "colour-taste.golden.jsonl").write_bytes((out / "golden-tiny-taste.jsonl").read_bytes())
        (HERE / "colour-taste.golden-journal.json").write_bytes((out / "golden-tiny-taste-journal.json").read_bytes())


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    main(sys.argv[1])
