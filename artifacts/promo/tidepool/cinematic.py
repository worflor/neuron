# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Tidepool cinematic: one slow camera move along a keyboard lit by Neuron's own Tidepool frames.

Run through the showcase harness's bpy runner, which supplies the keyboard fixture:

    python .local/showcase/tools/run_bpy.py --script artifacts/promo/tidepool/cinematic.py \
        --frames <tidepool-frames.json> --output D:/build-cache/promo/tidepool/cinematic

The keyboard, its key map, its materials and the per-frame painting come from
`.local/showcase/tools/render_showcase.py`; every LED colour is the frame file's RGB, unchanged.
This script adds only staging: a perspective camera on a dolly, a shallow focus and a glow in the
compositor; the board stands on the fixture's own studio floor. Options: --start/--count to render
part, --scale and --samples for tests, --still N for one frame.
"""
import argparse
import json
import math
import os
import sys

import bpy
from mathutils import Vector

SHOWCASE_TOOLS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "..", ".local", "showcase", "tools")
sys.path.insert(0, os.path.abspath(SHOWCASE_TOOLS))
import render_showcase as fixture  # noqa: E402


def arguments():
    argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
    p = argparse.ArgumentParser()
    p.add_argument("--frames", required=True)
    p.add_argument("--output", required=True)
    p.add_argument("--samples", type=int, default=48)
    p.add_argument("--width", type=int, default=1920)
    p.add_argument("--scale", type=float, default=1.0)
    p.add_argument("--start", type=int, default=0)
    p.add_argument("--count", type=int, default=0)
    p.add_argument("--still", type=int, default=-1, help="render one frame index only")
    p.add_argument("--led-power", type=float, default=3.4, help="fixture optical output; never changes the RGB")
    p.add_argument("--resume", action="store_true")
    return p.parse_args(argv)


def smooth(t):
    """Ease in and out: the camera starts and settles without a jolt."""
    t = min(1.0, max(0.0, t))
    return t * t * (3 - 2 * t)


def main():
    args = arguments()
    data = json.load(open(args.frames, encoding="utf-8"))
    os.makedirs(args.output, exist_ok=True)
    fixture.clear_scene()
    scene = bpy.context.scene
    scene["showcase_led_power"] = args.led_power
    scene["showcase_camera"] = "hero"
    scene["showcase_upper_transmission"] = 0.10
    keys, _ = fixture.build_keyboard(scene, data)
    fixture.configure_scene(scene, data["fps"], args.output, args.samples, round(args.width * args.scale), "OPTIX")

    # the board's extent, from its keys (the fixture's studio backdrop is far larger)
    points = [key["object"].matrix_world @ Vector(c) for key in keys for c in key["object"].bound_box]
    lo = Vector((min(p.x for p in points), min(p.y for p in points), min(p.z for p in points)))
    hi = Vector((max(p.x for p in points), max(p.y for p in points), max(p.z for p in points)))
    centre, width = (lo + hi) / 2, hi.x - lo.x

    # a perspective camera gliding from low over the left of the board to a high three-quarter view
    cam_data = bpy.data.cameras.new("Cinematic")
    cam_data.lens = 50
    cam_data.dof.use_dof = True
    cam_data.dof.aperture_fstop = 4.0
    camera = bpy.data.objects.new("Cinematic camera", cam_data)
    scene.collection.objects.link(camera)
    scene.camera = camera
    focus = bpy.data.objects.new("Focus", None)
    scene.collection.objects.link(focus)
    cam_data.dof.focus_object = focus
    start_eye = centre + Vector((-width * 0.62, -width * 0.48, width * 0.16))
    end_eye = centre + Vector((width * 0.12, -width * 1.38, width * 0.96))
    start_look = centre + Vector((-width * 0.30, 0.0, 0.0))
    end_look = centre + Vector((-width * 0.02, width * 0.03, 0.0))

    def place(i, n):
        t = smooth(i / max(1, n - 1))
        eye = start_eye.lerp(end_eye, t)
        look = start_look.lerp(end_look, t)
        camera.location = eye
        camera.rotation_euler = (look - eye).to_track_quat("-Z", "Y").to_euler()
        focus.location = look

    # a soft glow around the lit keys, as a lens sees bright light in the dark
    scene.use_nodes = True
    tree = scene.node_tree
    tree.nodes.clear()
    layers = tree.nodes.new("CompositorNodeRLayers")
    glare = tree.nodes.new("CompositorNodeGlare")
    glare.glare_type = "FOG_GLOW"
    glare.quality = "HIGH"
    glare.threshold = 0.35
    glare.size = 7
    glare.mix = -0.55
    out = tree.nodes.new("CompositorNodeComposite")
    tree.links.new(layers.outputs["Image"], glare.inputs["Image"])
    tree.links.new(glare.outputs["Image"], out.inputs["Image"])
    scene.view_settings.view_transform = "AgX"
    scene.view_settings.look = "AgX - Medium High Contrast"

    total = len(data["frames"])
    if args.still >= 0:
        frames = [args.still]
    else:
        last = total if args.count <= 0 else min(total, args.start + args.count)
        frames = list(range(args.start, last))

    def paint(index):
        cells = data["frames"][index]["rgb"]
        for key in keys:
            rgb = cells[key["index"]]
            color = tuple(fixture.srgb_to_linear(c) for c in rgb) + (1.0,)
            key["gap_object"].color = color
            key["object"].color = color
            if key["text"] is not None:
                key["text"].color = color

    for i in frames:
        path = os.path.join(args.output, f"tidepool_{i:04d}.png")
        if args.resume and os.path.exists(path):
            continue
        paint(i)
        place(i, total)
        scene.render.filepath = path
        bpy.ops.render.render(write_still=True)
        print(f"frame {i} -> {path}", flush=True)


if __name__ == "__main__":
    main()
