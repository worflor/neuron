# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Build and render the dev ad's one continuous shot.

    blender -b --factory-startup --python build_scene.py -- [--frames 1,90,300] [--scale 0.5] [--samples 16]

The keyboard is the generic BlackWidow fixture from .local/showcase (core 6x22 key map,
not a scan); the mouse and mic are stylised stand-ins for the Naga V2 Pro and Seiren V3
Mini. Colour comes from lighting.npz and the pane from term/#####.png; this script only
stages and times them.
"""
import argparse
import json
import math
import os
import sys

import bpy
import numpy as np
from mathutils import Matrix, Vector

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import timeline as tl  # noqa: E402

CONSOLAS = r"C:\Windows\Fonts\consola.ttf"

# Rig layout, metres. The keyboard is centred on the origin.
MOUSE = Vector((0.285, -0.010, 0.0))
MIC = Vector((-0.285, 0.075, 0.0))
PANE_C = Vector((0.0, 0.24, 0.31))
PANE_W = 0.62
PANE_H = PANE_W * 1280 / 1440
PANE_TILT = math.radians(8)  # leans back from vertical


def args():
    argv = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
    p = argparse.ArgumentParser()
    p.add_argument("--frames", default="", help="comma list of 1-based frames; empty = all")
    p.add_argument("--range", default="", help="first:last (1-based, inclusive)")
    p.add_argument("--every", type=int, default=1)
    p.add_argument("--scale", type=float, default=1.0)
    p.add_argument("--samples", type=int, default=28)
    p.add_argument("--out", default=os.path.join(tl.OUT, "render"))
    p.add_argument("--led", type=float, default=4.0, help="LED optical output")
    p.add_argument("--resume", action="store_true")
    p.add_argument("--save", action="store_true", help="also save the .blend")
    return p.parse_args(argv)


A = args()

# ----------------------------------------------------------------------------- helpers


def srgb_lin(v):
    v = np.asarray(v, dtype=float)
    return np.where(v <= 0.04045, v / 12.92, ((v + 0.055) / 1.055) ** 2.4)


def hexlin(h, a=1.0):
    h = h.lstrip("#")
    c = srgb_lin([int(h[i:i + 2], 16) / 255.0 for i in (0, 2, 4)])
    return (*c, a)


def fr(t):
    return tl.frame(t)


def link(obj, parent=None):
    bpy.context.scene.collection.objects.link(obj)
    if parent is not None:
        obj.parent = parent
    return obj


def surface(name, color, rough=0.4, metal=0.0, coat=0.0):
    m = bpy.data.materials.new(name)
    m.use_nodes = True
    b = m.node_tree.nodes["Principled BSDF"]
    b.inputs["Base Color"].default_value = (*color, 1)
    b.inputs["Roughness"].default_value = rough
    b.inputs["Metallic"].default_value = metal
    if coat:
        b.inputs["Coat Weight"].default_value = coat
        b.inputs["Coat Roughness"].default_value = 0.12
    return m


def glow(name, strength):
    """Emission whose colour is the object's colour: animate obj.color, not the material."""
    m = bpy.data.materials.new(name)
    m.use_nodes = True
    nt = m.node_tree
    nt.nodes.clear()
    out = nt.nodes.new("ShaderNodeOutputMaterial")
    em = nt.nodes.new("ShaderNodeEmission")
    info = nt.nodes.new("ShaderNodeObjectInfo")
    em.inputs["Strength"].default_value = strength
    nt.links.new(info.outputs["Color"], em.inputs["Color"])
    nt.links.new(em.outputs["Emission"], out.inputs["Surface"])
    return m


def mesh_obj(name, verts, faces, mat, parent=None, smooth=False):
    me = bpy.data.meshes.new(name)
    me.from_pydata(verts, [], faces)
    me.update()
    if smooth:
        for p in me.polygons:
            p.use_smooth = True
    me.materials.append(mat)
    return link(bpy.data.objects.new(name, me), parent)


def box(name, loc, dims, mat, bevel=0.002, parent=None):
    bpy.ops.mesh.primitive_cube_add(size=1, location=loc)
    o = bpy.context.object
    o.name = name
    o.dimensions = dims
    bpy.ops.object.transform_apply(location=False, rotation=False, scale=True)
    o.data.materials.append(mat)
    if bevel:
        m = o.modifiers.new("bevel", "BEVEL")
        m.width, m.segments = bevel, 3
        o.modifiers.new("wn", "WEIGHTED_NORMAL")
    if parent is not None:
        o.parent = parent
    return o


def text(name, body, loc, size, mat, parent=None, align="CENTER", rot=(0, 0, 0)):
    c = bpy.data.curves.new(name, "FONT")
    c.body = body
    c.size = size
    c.align_x = align
    c.align_y = "CENTER"
    c.font = FONT
    c.materials.append(mat)
    o = link(bpy.data.objects.new(name, c), parent)
    o.location = loc
    o.rotation_euler = rot
    return o


def fcurves(obj_or_id):
    ad = obj_or_id.animation_data
    if not ad or not ad.action:
        return []
    act = ad.action
    if hasattr(act, "layers") and len(act.layers):
        out = []
        for layer in act.layers:
            for strip in layer.strips:
                for bag in strip.channelbags:
                    out.extend(bag.fcurves)
        return out
    return list(act.fcurves)


def key(idblock, path, t, value, index=-1):
    """Insert a keyframe at time t (seconds)."""
    target = idblock
    attr = path
    if "." in path:
        head, attr = path.rsplit(".", 1)
        target = idblock.path_resolve(head)
    setattr(target, attr, value)
    idblock.keyframe_insert(data_path=path, frame=fr(t), index=index)


def interp(idblock, t, kind="BEZIER", easing="AUTO"):
    f = fr(t)
    for fc in fcurves(idblock):
        for p in fc.keyframe_points:
            if abs(p.co[0] - f) < 0.5:
                p.interpolation = kind
                p.easing = easing


def pop_in(obj, t, dur=0.18, scale=1.0):
    """Grow an object into place with a small overshoot."""
    key(obj, "scale", t - 0.001, (0.0, 0.0, 0.0))
    key(obj, "scale", t, (0.001, 0.001, 0.001))
    interp(obj, t, "BACK", "EASE_OUT")
    key(obj, "scale", t + dur, (scale, scale, scale))


def gone(obj, t):
    """Drop a faded-out object from the render: black emission still occludes."""
    key(obj, "hide_render", t, False)
    key(obj, "hide_render", t + 0.04, True)


def color_keys(obj, seq):
    """seq: [(t, rgba)] with smooth transitions."""
    for t, c in seq:
        key(obj, "color", t, c)


# ----------------------------------------------------------------------------- scene


def clear():
    bpy.ops.object.select_all(action="SELECT")
    bpy.ops.object.delete()
    for coll in (bpy.data.meshes, bpy.data.materials, bpy.data.curves, bpy.data.lights, bpy.data.cameras, bpy.data.images):
        for b in list(coll):
            coll.remove(b)


clear()
SCN = bpy.context.scene
FONT = bpy.data.fonts.load(CONSOLAS)
BLACK = (0.0, 0.0, 0.0, 1.0)
ACC = hexlin(tl.ACCENT)
DIM = hexlin(tl.TEXT_DIM)
LIS = hexlin(tl.LISTEN)

# ----------------------------------------------------------------------------- keyboard
# Builder adapted from .local/showcase/tools/render_showcase.py: same pitch, rows and core
# key map, so the grid indexes match a capture cell for cell.

KEYMAP = json.load(open(os.path.join(HERE, "keymap.json")))
CELL = {k["name"]: (k["row"], k["col"]) for k in KEYMAP["key_map"]}
COLS = 22
PITCH = 0.0175

KB = link(bpy.data.objects.new("Keyboard", None))
M_CASE = surface("Case · satin dark aluminium", (0.055, 0.064, 0.075), 0.36, 0.62)
M_PLATE = surface("Switch deck", (0.016, 0.02, 0.025), 0.42, 0.24)
M_CAP = surface("Keycap · smoked", (0.020, 0.024, 0.028), 0.46)
M_COLLAR = glow("Cell collar", 1.0)
M_LEGEND = glow("Cell legend", 1.0)
M_RUBBER = surface("Rubber", (0.01, 0.012, 0.014), 0.8)
_cap_info = M_CAP.node_tree.nodes.new("ShaderNodeObjectInfo")
M_CAP.node_tree.links.new(_cap_info.outputs["Color"], M_CAP.node_tree.nodes["Principled BSDF"].inputs["Emission Color"])
M_CAP.node_tree.nodes["Principled BSDF"].inputs["Emission Strength"].default_value = 0.10

KEYS = []  # dicts: name, index, cap, collar, legend, x, y


def tapered_cap(name, x, y, z, w, d, h, bevel):
    inset = min(w, d) * 0.075
    n = 8
    verts, faces = [], []
    for iy in range(n + 1):
        v = -1 + 2 * iy / n
        for ix in range(n + 1):
            u = -1 + 2 * ix / n
            dish = 0.00085 * (1 - u * u) * (1 - v * v)
            verts.append((x + u * (w / 2 - inset), y + v * (d / 2 - inset), z + h - dish))
    for iy in range(n):
        for ix in range(n):
            i = iy * (n + 1) + ix
            faces.append((i, i + 1, i + n + 2, i + n + 1))
    per = list(range(n + 1)) + [iy * (n + 1) + n for iy in range(1, n + 1)]
    per += [n * (n + 1) + ix for ix in range(n - 1, -1, -1)] + [iy * (n + 1) for iy in range(n - 1, 0, -1)]
    bottom = []
    for idx in per:
        vx, vy, _ = verts[idx]
        bottom.append(len(verts))
        verts.append((x + (vx - x) * w / (w - 2 * inset), y + (vy - y) * d / (d - 2 * inset), z))
    for i in range(len(per)):
        j = (i + 1) % len(per)
        faces.append((per[i], bottom[i], bottom[j], per[j]))
    faces.append(tuple(reversed(bottom)))
    o = mesh_obj(name, verts, faces, M_CAP, KB)
    for p in list(o.data.polygons)[:n * n]:
        p.use_smooth = True
    m = o.modifiers.new("bevel", "BEVEL")
    m.width, m.segments, m.limit_method, m.angle_limit = bevel, 3, "ANGLE", 0.45
    o.modifiers.new("wn", "WEIGHTED_NORMAL")
    return o


def label_for(name):
    if name.startswith("NUM") and name[3:].isdigit():
        return name[3:]
    return {
        "BACKTICK": "`", "DASH": "-", "EQUALS": "=", "BACKSPACE": "BKSP", "TAB": "TAB",
        "LEFTBRACKET": "[", "RIGHTBRACKET": "]", "BACKSLASH": "\\", "CAPSLOCK": "CAPS",
        "COMMA": ",", "PERIOD": ".", "SLASH": "/", "SEMICOLON": ";", "QUOTE": "'", "ENTER": "ENTER",
        "LSHIFT": "SHIFT", "RSHIFT": "SHIFT", "LCTRL": "CTRL", "RCTRL": "CTRL", "LALT": "ALT", "RALT": "ALT",
        "WIN": "WIN", "SPACE": "", "MENU": "MENU", "FN": "FN", "PRINTSCREEN": "PRT", "SCROLLLOCK": "SCR",
        "PAUSE": "PSE", "INSERT": "INS", "DELETE": "DEL", "PAGEUP": "PGUP", "PAGEDOWN": "PGDN", "NUMLOCK": "NUM",
        "NUMDIVIDE": "/", "NUMMULTIPLY": "*", "NUMSUBTRACT": "-", "NUMADD": "+", "NUMENTER": "ENT",
        "NUMDECIMAL": ".", "UP": "^", "DOWN": "v", "LEFT": "<", "RIGHT": ">", "HOME": "HOME", "END": "END",
    }.get(name, name)


def add_key(name, x, y, wu, hu):
    row, col = CELL[name]
    w, d = wu * PITCH - 0.0015, hu * PITCH - 0.0015
    z0, h = 0.020, 0.0080
    collar = box(f"collar {name}", (x, y, z0 + 0.0022), (w, d, 0.0044), M_COLLAR, 0.001, KB)
    cap = tapered_cap(f"cap {name}", x, y, z0 + 0.004, w - 0.001, d - 0.001, h, 0.0011)
    lab = label_for(name)
    leg = None
    if lab:
        size = 0.0031 if len(lab) >= 4 else 0.0046
        leg = text(f"legend {name}", lab, (x, y + d * 0.12, z0 + 0.004 + h + 0.0001), size, M_LEGEND, KB)
    KEYS.append(dict(name=name, index=row * COLS + col, cap=cap, collar=collar, legend=leg, x=x, y=y))


def kb_row(defs, y, x0):
    cur = 0.0
    for name, wu in defs:
        add_key(name, x0 + (cur + wu / 2) * PITCH, y, wu, 1.0)
        cur += wu


def build_keyboard():
    main_left = -15.0 * PITCH / 2
    ry = {0: 0.064, 1: 0.037, 2: 0.0185, 3: 0.0, 4: -0.0185, 5: -0.037}
    nav_left = main_left + 15 * PITCH + 0.35 * PITCH
    num_left = nav_left + 3.55 * PITCH
    macro_x = main_left - 0.95 * PITCH
    left, right = macro_x - 0.011, num_left + 4 * PITCH + 0.011
    bw, bd, by, bx = right - left, 0.145, 0.0135, (left + right) / 2
    box("case", (bx, by, 0.008), (bw, bd, 0.016), M_CASE, 0.0025, KB)
    box("deck", (bx, by, 0.0165), (bw - 0.006, bd - 0.006, 0.004), M_PLATE, 0.0015, KB)
    for x in (left + 0.022, right - 0.022):
        for y in (by - 0.053, by + 0.053):
            box("foot", (x, y, -0.002), (0.031, 0.014, 0.006), M_RUBBER, 0.002, KB)
    kb_row([("BACKTICK", 1)] + [(c, 1) for c in "1234567890"] + [("DASH", 1), ("EQUALS", 1), ("BACKSPACE", 2)], ry[1], main_left)
    kb_row([("TAB", 1.5)] + [(c, 1) for c in "QWERTYUIOP"] + [("LEFTBRACKET", 1), ("RIGHTBRACKET", 1), ("BACKSLASH", 1.5)], ry[2], main_left)
    kb_row([("CAPSLOCK", 1.75)] + [(c, 1) for c in "ASDFGHJKL"] + [("SEMICOLON", 1), ("QUOTE", 1), ("ENTER", 2.25)], ry[3], main_left)
    kb_row([("LSHIFT", 2.25)] + [(c, 1) for c in "ZXCVBNM"] + [("COMMA", 1), ("PERIOD", 1), ("SLASH", 1), ("RSHIFT", 2.75)], ry[4], main_left)
    kb_row([("LCTRL", 1.25), ("WIN", 1.25), ("LALT", 1.25), ("SPACE", 6.25), ("RALT", 1.25), ("FN", 1.25), ("MENU", 1.25), ("RCTRL", 1.25)], ry[5], main_left)
    for name, r in [("M6", 0), ("M1", 1), ("M2", 2), ("M3", 3), ("M4", 4), ("M5", 5)]:
        add_key(name, macro_x, ry[r], 1, 1)
    for name, off in [("ESC", 0.45), ("F1", 2.15), ("F2", 3.15), ("F3", 4.15), ("F4", 5.15), ("F5", 6.55), ("F6", 7.55),
                      ("F7", 8.55), ("F8", 9.55), ("F9", 10.95), ("F10", 11.95), ("F11", 12.95), ("F12", 13.95)]:
        add_key(name, main_left + off * PITCH, ry[0], 1, 1)
    for i, name in enumerate(("PRINTSCREEN", "SCROLLLOCK", "PAUSE")):
        add_key(name, nav_left + (i + 0.5) * PITCH, ry[0], 1, 1)
    for r, names in ((1, ("INSERT", "HOME", "PAGEUP")), (2, ("DELETE", "END", "PAGEDOWN"))):
        for c, name in enumerate(names):
            add_key(name, nav_left + (c + 0.5) * PITCH, ry[r], 1, 1)
    add_key("UP", nav_left + 1.5 * PITCH, ry[4], 1, 1)
    for c, name in enumerate(("LEFT", "DOWN", "RIGHT")):
        add_key(name, nav_left + (c + 0.5) * PITCH, ry[5], 1, 1)
    for c, name in enumerate(("NUMLOCK", "NUMDIVIDE", "NUMMULTIPLY", "NUMSUBTRACT")):
        add_key(name, num_left + (c + 0.5) * PITCH, ry[1], 1, 1)
    for r, names in ((2, ("NUM7", "NUM8", "NUM9")), (3, ("NUM4", "NUM5", "NUM6")), (4, ("NUM1", "NUM2", "NUM3"))):
        for c, name in enumerate(names):
            add_key(name, num_left + (c + 0.5) * PITCH, ry[r], 1, 1)
    add_key("NUMADD", num_left + 3.5 * PITCH, (ry[2] + ry[3]) / 2, 1, 2)
    add_key("NUMENTER", num_left + 3.5 * PITCH, (ry[4] + ry[5]) / 2, 1, 2)
    add_key("NUM0", num_left + 1.5 * PITCH, ry[5], 2, 1)
    add_key("NUMDECIMAL", num_left + 3.5 * PITCH, ry[5], 1, 1)
    # Centre the board on the origin.
    for o in KB.children:
        o.location.x -= bx
        o.location.y -= by
    for k in KEYS:
        k["x"] -= bx
        k["y"] -= by
    return bw, bd


BOARD_W, BOARD_D = build_keyboard()

# ----------------------------------------------------------------------------- mouse

M_SHELL = surface("Mouse shell · satin", (0.016, 0.018, 0.021), 0.38, 0.0, coat=0.25)
M_ZONE = glow("Zone light", 1.0)

MOUSE_ROOT = link(bpy.data.objects.new("Mouse", None))
MOUSE_ROOT.location = MOUSE
ML, MW, MH = 0.119, 0.075, 0.043


def mouse_top(s):
    """Shell height at s in [-1 back, 1 front]."""
    return MH * (0.60 + 0.40 * math.exp(-((s + 0.22) / 0.62) ** 2))


def mouse_half_w(s):
    return MW / 2 * (0.80 + 0.20 * math.cos((s + 0.15) * 1.35))


def build_mouse():
    bpy.ops.mesh.primitive_uv_sphere_add(segments=96, ring_count=48, radius=1.0)
    o = bpy.context.object
    o.name = "mouse shell"
    me = o.data
    for v in me.vertices:
        x, y, z = v.co
        s = y
        hw = mouse_half_w(s)
        top = mouse_top(s)
        # Flat-ish sole, domed back, a little thumb-side flare.
        zz = z * top if z > 0 else z * 0.004
        flare = 1.0 + (0.06 if x < 0 else 0.0) * (1 - abs(z))
        v.co = Vector((x * hw * flare, y * ML / 2, zz + 0.004))
    for p in me.polygons:
        p.use_smooth = True
    me.materials.append(M_SHELL)
    o.parent = MOUSE_ROOT
    o.modifiers.new("sub", "SUBSURF").levels = 1
    # Scroll wheel and its light ring.
    sw = 0.48
    wz = mouse_top(sw) + 0.004 - 0.002
    wy = sw * ML / 2
    bpy.ops.mesh.primitive_cylinder_add(vertices=48, radius=0.011, depth=0.0075, location=(0, wy, wz), rotation=(0, math.pi / 2, 0))
    wheel = bpy.context.object
    wheel.name = "wheel"
    wheel.data.materials.append(M_SHELL)
    wheel.parent = MOUSE_ROOT
    bpy.ops.mesh.primitive_torus_add(major_radius=0.0112, minor_radius=0.0009, location=(0, wy, wz), rotation=(0, math.pi / 2, 0))
    ring = bpy.context.object
    ring.name = "zone wheel"
    ring.data.materials.append(M_ZONE)
    ring.parent = MOUSE_ROOT
    # Logo: a soft disc on the palm.
    ls = -0.45
    bpy.ops.mesh.primitive_cylinder_add(vertices=48, radius=0.0075, depth=0.0006,
                                        location=(0, ls * ML / 2, mouse_top(ls) + 0.004 - 0.0004))
    logo = bpy.context.object
    logo.name = "zone logo"
    logo.data.materials.append(M_ZONE)
    logo.parent = MOUSE_ROOT
    # Button split line, as a shallow dark seam.
    # Side plate: 12 buttons, 4 rows x 3 columns, on the thumb (left) side.
    plate, caps = [], []
    for r in range(4):
        for c in range(3):
            s = 0.02 + (c - 1) * 0.165
            z = 0.012 + r * 0.0072
            x = -(mouse_half_w(s) * 1.06) * math.sqrt(max(0.05, 1 - ((z - 0.004) / mouse_top(s)) ** 2)) - 0.0012
            pos = (x, s * ML / 2, z)
            led = box(f"zone plate {r}{c}", pos, (0.0022, 0.0098, 0.0064), M_ZONE, 0.0008, MOUSE_ROOT)
            cap = box(f"plate cap {r}{c}", (x - 0.0012, s * ML / 2, z), (0.0024, 0.0086, 0.0054), M_SHELL, 0.0009, MOUSE_ROOT)
            plate.append(led)
            caps.append(cap)
    return ring, logo, plate, caps


M_WHEEL, M_LOGO, M_PLATE_LEDS, MOUSE_PLATE_CAPS = build_mouse()

# ----------------------------------------------------------------------------- mic

M_GRILLE = surface("Mic grille · dark metal", (0.03, 0.033, 0.037), 0.32, 0.85)
_g = M_GRILLE.node_tree
_vor = _g.nodes.new("ShaderNodeTexVoronoi")
_vor.inputs["Scale"].default_value = 900.0
_vor.feature = "DISTANCE_TO_EDGE"
_ramp = _g.nodes.new("ShaderNodeValToRGB")
_ramp.color_ramp.elements[0].position = 0.02
_ramp.color_ramp.elements[1].position = 0.12
_g.links.new(_vor.outputs["Distance"], _ramp.inputs["Fac"])
_bump = _g.nodes.new("ShaderNodeBump")
_bump.inputs["Strength"].default_value = 0.6
_bump.inputs["Distance"].default_value = 0.0004
_g.links.new(_ramp.outputs["Color"], _bump.inputs["Height"])
_g.links.new(_bump.outputs["Normal"], _g.nodes["Principled BSDF"].inputs["Normal"])
M_MICBODY = surface("Mic body · satin", (0.014, 0.016, 0.019), 0.42, 0.0, coat=0.2)

MIC_ROOT = link(bpy.data.objects.new("Mic", None))
MIC_ROOT.location = MIC


def build_mic():
    bpy.ops.mesh.primitive_cylinder_add(vertices=64, radius=0.042, depth=0.012, location=(0, 0, 0.006))
    base = bpy.context.object
    base.data.materials.append(M_MICBODY)
    base.parent = MIC_ROOT
    m = base.modifiers.new("bevel", "BEVEL")
    m.width, m.segments = 0.004, 4
    bpy.ops.mesh.primitive_cylinder_add(vertices=32, radius=0.007, depth=0.035, location=(0, 0, 0.029))
    neck = bpy.context.object
    neck.data.materials.append(M_MICBODY)
    neck.parent = MIC_ROOT
    # Capsule body: grille on top two thirds.
    bpy.ops.mesh.primitive_cylinder_add(vertices=96, radius=0.026, depth=0.034, location=(0, 0, 0.063))
    lower = bpy.context.object
    lower.data.materials.append(M_MICBODY)
    lower.parent = MIC_ROOT
    m = lower.modifiers.new("bevel", "BEVEL")
    m.width, m.segments = 0.004, 4
    bpy.ops.mesh.primitive_cylinder_add(vertices=96, radius=0.0255, depth=0.066, location=(0, 0, 0.113))
    grille = bpy.context.object
    grille.data.materials.append(M_GRILLE)
    grille.parent = MIC_ROOT
    m = grille.modifiers.new("bevel", "BEVEL")
    m.width, m.segments = 0.008, 6
    for p in grille.data.polygons:
        p.use_smooth = True
    # Tap-to-mute sensor light on the crown.
    bpy.ops.mesh.primitive_torus_add(major_radius=0.012, minor_radius=0.0011, location=(0, 0, 0.1465))
    led = bpy.context.object
    led.name = "zone mic"
    led.data.materials.append(M_ZONE)
    led.parent = MIC_ROOT
    return led


MIC_LED = build_mic()

# ----------------------------------------------------------------------------- floor, light, world

M_FLOOR = surface("Floor · dark gloss", (0.0025, 0.0028, 0.0032), 0.42, 0.0)
bpy.ops.mesh.primitive_plane_add(size=8, location=(0, 0, -0.005))
bpy.context.object.data.materials.append(M_FLOOR)

world = bpy.data.worlds.new("void")
world.use_nodes = True
world.node_tree.nodes["Background"].inputs["Color"].default_value = hexlin(tl.BG)
world.node_tree.nodes["Background"].inputs["Strength"].default_value = 0.02
SCN.world = world


def area(name, loc, target, energy, size, color=(1, 1, 1), shape="DISK", size_y=None):
    d = bpy.data.lights.new(name, "AREA")
    d.energy, d.color, d.shape, d.size = energy, color, shape, size
    if size_y:
        d.size_y = size_y
    o = link(bpy.data.objects.new(name, d))
    o.location = loc
    o.rotation_euler = (Vector(target) - Vector(loc)).to_track_quat("-Z", "Y").to_euler()
    return o


KEY_LIGHT = area("key", (-0.35, -0.45, 0.65), (0, 0, 0), 1.4, 0.6)
RIM_L = area("rim left", (-0.60, 0.55, 0.38), (0.0, 0.0, 0.02), 0.7, 0.9, (0.75, 0.88, 1.0), "RECTANGLE", 0.04)
RIM_R = area("rim right", (0.65, 0.50, 0.36), (0.25, 0.0, 0.02), 0.6, 0.9, (0.75, 0.88, 1.0), "RECTANGLE", 0.04)

# The room comes up once the devices have answered.
_room_up = tl.arrival("wake_mic") + 0.3
for lamp, e in ((KEY_LIGHT, 1.4), (RIM_L, 0.7), (RIM_R, 0.6)):
    key(lamp.data, "energy", 0.0, 0.0)
    key(lamp.data, "energy", tl.arrival("wake_mouse"), 0.0)
    key(lamp.data, "energy", _room_up + 0.8, e)


def spot(name, target, height=0.55, size=0.30):
    """A soft overhead spot on one device: the attention light."""
    d = bpy.data.lights.new(name, "SPOT")
    d.spot_size = math.radians(44)
    d.spot_blend = 0.85
    d.shadow_soft_size = size
    d.color = (0.86, 0.94, 1.0)
    o = link(bpy.data.objects.new(name, d))
    o.location = Vector(target) + Vector((0, -0.18, height))
    o.rotation_euler = (Vector(target) - o.location).to_track_quat("-Z", "Y").to_euler()
    return d


SPOTS = {
    "mouse": (spot("spot mouse", MOUSE), 2.6),
    "keyboard": (spot("spot keyboard", (0, 0, 0), 0.62, 0.4), 5.0),
    "mic": (spot("spot mic", MIC + Vector((0, 0, 0.06))), 2.2),
}
_WAKE = {"mouse": tl.arrival("wake_mouse"), "keyboard": tl.arrival("wake_keyboard"), "mic": tl.arrival("wake_mic")}


def spot_level(dev, t):
    """Energy factor for one device's spot at time t, from timeline.SUBJECTS."""
    if t < _WAKE[dev]:
        return 0.0
    subject = None
    for when, who in tl.SUBJECTS:
        if when <= t:
            subject = who
    if subject == dev:
        return 1.0
    if subject in ("all", None):
        return 0.45
    return 0.12


# Key each spot at every subject change and wake, easing over 0.35 s.
for dev, (data, peak) in SPOTS.items():
    times = sorted({0.0, _WAKE[dev], *[w for w, _ in tl.SUBJECTS], tl.DURATION})
    key(data, "energy", 0.0, 0.0)
    for w in times:
        if w <= 0.0:
            continue
        key(data, "energy", w, spot_level(dev, w - 0.01) * peak)
        key(data, "energy", min(tl.DURATION, w + 0.35), spot_level(dev, w + 0.01) * peak)
    # The wake: a brief overshoot as the device answers.
    key(data, "energy", _WAKE[dev] + 0.12, peak * 1.6)

# ----------------------------------------------------------------------------- terminal pane

M_PANE = bpy.data.materials.new("Terminal pane")
M_PANE.use_nodes = True
_pn = M_PANE.node_tree
_pn.nodes.clear()
_po = _pn.nodes.new("ShaderNodeOutputMaterial")
_pe = _pn.nodes.new("ShaderNodeEmission")
_pt = _pn.nodes.new("ShaderNodeTexImage")
_pn.links.new(_pt.outputs["Color"], _pe.inputs["Color"])
_pn.links.new(_pe.outputs["Emission"], _po.inputs["Surface"])
_pe.inputs["Strength"].default_value = 1.0
PANE_IMG = None

bpy.ops.mesh.primitive_plane_add(size=1, location=PANE_C, rotation=(math.pi / 2 - PANE_TILT, 0, 0))
PANE = bpy.context.object
PANE.name = "terminal"
PANE.scale = (PANE_W, PANE_H, 1)
PANE.data.materials.append(M_PANE)
bpy.context.view_layer.update()
PANE_M = PANE.matrix_world.copy()


def pane_point(u, v):
    """World point of texture (u, v) on the pane, v down."""
    return PANE_M @ Vector((u - 0.5, 0.5 - v, 0.002))


# Collapse: fold to a line, then to a point.
_ct = tl.COLLAPSE_T
key(PANE, "scale", _ct, (PANE_W, PANE_H, 1))
key(PANE, "scale", _ct + 0.22, (PANE_W, PANE_H * 0.012, 1))
interp(PANE, _ct, "EXPO", "EASE_IN")
key(PANE, "scale", _ct + 0.42, (0.0, PANE_H * 0.012, 1))
interp(PANE, _ct + 0.22, "EXPO", "EASE_IN")
key(PANE, "hide_render", 0.0, False)
key(PANE, "hide_render", _ct + 0.41, False)
key(PANE, "hide_render", _ct + 0.43, True)
key(M_PANE.node_tree, 'nodes["Emission"].inputs[1].default_value', _ct, 1.0)
key(M_PANE.node_tree, 'nodes["Emission"].inputs[1].default_value', _ct + 0.22, 6.0)
key(M_PANE.node_tree, 'nodes["Emission"].inputs[1].default_value', _ct + 0.42, 14.0)

# ----------------------------------------------------------------------------- light threads

ANCHORS = json.load(open(os.path.join(tl.OUT, "anchors.json")))
M_THREAD = glow("Thread", 14.0)


M_HEAD = glow("Thread head", 40.0)
M_PING = glow("Ping", 6.0)


def ease_sine(k):
    k = max(0.0, min(1.0, k))
    return 0.5 - 0.5 * math.cos(math.pi * k)


def ping(name, center, t0, r0=0.02, r1=0.10, life=0.7, z=0.0015, squash=1.0, gain=2.0):
    """A ring that blooms where a thread lands: 'here'."""
    bpy.ops.mesh.primitive_torus_add(major_radius=1.0, minor_radius=0.012, major_segments=96, minor_segments=8,
                                     location=(center[0], center[1], z))
    o = bpy.context.object
    o.name = name
    o.data.materials.append(M_PING)
    o.color = BLACK
    key(o, "hide_render", 0.0, True)
    key(o, "hide_render", t0 - 0.01, False)
    key(o, "hide_render", t0 + life + 0.02, True)
    for i in range(int(life * tl.FPS) + 2):
        tt = t0 + i / tl.FPS
        k = min(1.0, i / (life * tl.FPS))
        r = r0 + (r1 - r0) * (1 - (1 - k) ** 3)
        key(o, "scale", tt, (r, r * squash, r))
        a = (1 - k) ** 1.6
        key(o, "color", tt, (ACC[0] * gain * a, ACC[1] * gain * a, ACC[2] * gain * a, 1))
    return o


def thread(name, a, b, t0, travel=tl.THREAD_TRAVEL, lift=0.10, color=tl.ACCENT, tail=0.16):
    """A comet of light from a to b, leaving at t0, with a bright head riding its front."""
    a, b = Vector(a), Vector(b)
    mid = (a + b) / 2 + Vector((0, -0.04, lift))
    pts = [(1 - s) ** 2 * a + 2 * (1 - s) * s * mid + s * s * b for s in (i / 63 for i in range(64))]
    cu = bpy.data.curves.new(name, "CURVE")
    cu.dimensions = "3D"
    cu.bevel_depth = 0.0010
    cu.bevel_resolution = 3
    cu.use_fill_caps = True
    sp = cu.splines.new("POLY")
    sp.points.add(len(pts) - 1)
    for i, p in enumerate(pts):
        sp.points[i].co = (*p, 1)
    cu.materials.append(M_THREAD)
    o = link(bpy.data.objects.new(name, cu))
    o.color = hexlin(color)
    cu.bevel_factor_mapping_start = "SPLINE"
    cu.bevel_factor_mapping_end = "SPLINE"
    key(cu, "bevel_factor_end", t0, 0.0)
    key(cu, "bevel_factor_end", t0 + travel, 1.0)
    key(cu, "bevel_factor_start", t0 + tail, 0.0)
    key(cu, "bevel_factor_start", t0 + travel + tail, 1.0)
    for tt in (t0, t0 + tail):
        interp(cu, tt, "SINE", "EASE_IN_OUT")
    key(o, "hide_render", 0.0, True)
    key(o, "hide_render", t0, False)
    key(o, "hide_render", t0 + travel + tail + 0.02, True)
    # The head: rides the same eased path, so the eye has one point to follow.
    bpy.ops.mesh.primitive_uv_sphere_add(radius=0.0028, segments=16, ring_count=8)
    h = bpy.context.object
    h.name = name + " head"
    h.data.materials.append(M_HEAD)
    h.color = (0.75, 1.0, 0.9, 1)
    key(h, "hide_render", 0.0, True)
    key(h, "hide_render", t0, False)
    key(h, "hide_render", t0 + travel + 0.02, True)
    steps = int(travel * tl.FPS) + 1
    for i in range(steps + 1):
        k = ease_sine(i / steps)
        p = pts[min(len(pts) - 1, int(round(k * (len(pts) - 1))))]
        key(h, "location", t0 + i * travel / steps, p)
    return o


def anchor(ev):
    return pane_point(*ANCHORS[ev])


KEY_BY_NAME = {k["name"]: k for k in KEYS}
KB_CENTER = Vector((0.0, 0.0, 0.03))
MOUSE_TOP = MOUSE + Vector((0, 0.0, 0.048))
MIC_TOP = MIC + Vector((0, 0, 0.15))
ENTER = Vector((KEY_BY_NAME["ENTER"]["x"], KEY_BY_NAME["ENTER"]["y"], 0.032))
# Thumb 2 on the side plate: top row, middle column.
THUMB = MOUSE_PLATE_CAPS[3 * 3 + 1]
THUMB_P = MOUSE + Vector(THUMB.location) + Vector((-0.004, 0, 0))

for ev, target, ring in (("wake_mouse", MOUSE_TOP, 0.07), ("wake_keyboard", KB_CENTER, 0.30), ("wake_mic", MIC_TOP, 0.07)):
    thread(f"thr {ev}", anchor(ev), target, tl.event_time(ev))
    ping(f"ping {ev}", (target.x, target.y), tl.arrival(ev), r1=ring, squash=0.45 if ev == "wake_keyboard" else 1.0)
thread("thr profile", anchor("profile"), KB_CENTER + Vector((0, 0.06, 0.03)), tl.event_time("profile"))
thread("thr dpi", anchor("dpi"), MOUSE_TOP, tl.event_time("dpi"))
thread("thr mute a", anchor("mute"), THUMB_P, tl.event_time("mute"))
ping("ping thumb", (MOUSE.x - 0.045, MOUSE.y), tl.arrival("mute"), r1=0.04)
thread("thr mute b", THUMB_P, MIC_TOP, tl.arrival("mute") + 0.25, travel=0.6, lift=0.08)
ping("ping mic", (MIC.x, MIC.y), tl.arrival("mute") + 0.85, r1=0.09)
_wedge_dir = math.radians(tl.RADIAL_WEDGE * 360 / tl.RADIAL_SECTORS)
WEDGE2 = MOUSE + Vector((math.sin(_wedge_dir) * 0.098, math.cos(_wedge_dir) * 0.098, 0.004))
thread("thr radial", anchor("radial"), WEDGE2, tl.event_time("radial"), lift=0.16)
for ev in ("fire", "fire_off", "heat"):
    thread(f"thr {ev}", anchor(ev), KB_CENTER, tl.event_time(ev))
    ping(f"ping {ev}", (0, 0), tl.arrival(ev), r1=0.27, squash=0.42, gain=0.7, life=0.55)

# Thumb 2 presses in as its bind lands.
_tz = THUMB.location.copy()
for o in (THUMB,):
    key(o, "location", tl.arrival("mute") - 0.02, _tz)
    key(o, "location", tl.arrival("mute") + 0.06, _tz + Vector((0.0018, 0, 0)))
    key(o, "location", tl.arrival("mute") + 0.22, _tz)

# ----------------------------------------------------------------------------- the drop

M_DROP = glow("Drop", 30.0)
bpy.ops.mesh.primitive_uv_sphere_add(radius=0.0035, location=PANE_C)
DROP = bpy.context.object
DROP.data.materials.append(M_DROP)
DROP.color = (0.8, 1.0, 0.92, 1)
_drop_start = PANE_M @ Vector((0, 0, 0.003))
key(DROP, "hide_render", 0.0, True)
key(DROP, "hide_render", _ct + 0.40, False)
key(DROP, "hide_render", tl.DROP_LAND + 0.01, True)
key(DROP, "location", _ct + 0.40, _drop_start)
key(DROP, "location", tl.DROP_T, _drop_start + Vector((0, -0.02, 0.01)))
key(DROP, "location", tl.DROP_LAND, ENTER)
interp(DROP, tl.DROP_T, "QUAD", "EASE_IN")
ping("ping enter", (ENTER.x, ENTER.y), tl.DROP_LAND, r1=0.05, z=0.03)

# ----------------------------------------------------------------------------- mechanisms

M_MECH = glow("Mechanism light", 2.0)


def annular_sector(name, center, r0, r1, a0, a1, z, parent=None, seg=24):
    verts, faces = [], []
    for i in range(seg + 1):
        a = a0 + (a1 - a0) * i / seg
        s, c = math.sin(a), math.cos(a)
        verts += [(r0 * s, r0 * c, 0), (r1 * s, r1 * c, 0)]
    for i in range(seg):
        faces.append((2 * i, 2 * i + 1, 2 * i + 3, 2 * i + 2))
    o = mesh_obj(name, verts, faces, M_MECH, parent)
    o.location = (center[0], center[1], z)
    return o


def mono_label(name, body, origin, size, color, t, rot=(0, 0, 0), stagger=0.03, hold=None, fade=0.4):
    """Characters pop in one by one; optional fade to black after `hold` seconds."""
    objs = []
    adv = size * 0.55
    rot_m = Matrix.Rotation(rot[0], 4, "X") @ Matrix.Rotation(rot[2], 4, "Z")
    for i, ch in enumerate(body):
        if ch == " ":
            continue
        off = rot_m @ Vector(((i - (len(body) - 1) / 2) * adv, 0, 0))
        o = text(f"{name} {i}", ch, Vector(origin) + off, size, M_MECH, rot=rot)
        o.color = color
        pop_in(o, t + i * stagger, 0.16)
        if hold is not None:
            key(o, "color", t + hold, color)
            key(o, "color", t + hold + fade, BLACK)
            gone(o, t + hold + fade)
        objs.append(o)
    return objs


# Profile: the rig squares up and its name resolves above the board.
_pa = tl.arrival("profile")
for root, loc0, rot0 in ((MOUSE_ROOT, MOUSE + Vector((0.025, -0.03, 0)), math.radians(-11)),
                         (MIC_ROOT, MIC + Vector((-0.02, 0.035, 0)), math.radians(14)),
                         (KB, Vector((0.0, -0.01, 0)), math.radians(2.2))):
    final = root.location.copy()
    key(root, "location", 0.0, loc0)
    key(root, "rotation_euler", 0.0, (0, 0, rot0))
    key(root, "location", _pa, loc0)
    key(root, "rotation_euler", _pa, (0, 0, rot0))
    interp(root, _pa, "BACK", "EASE_OUT")
    key(root, "location", _pa + 0.55, final)
    key(root, "rotation_euler", _pa + 0.55, (0, 0, 0))
mono_label("profile", "profile", (0, 0.115, 0.115), 0.012, DIM, _pa + 0.05, rot=(math.radians(78), 0, 0), hold=1.9)
mono_label("dev", "dev", (0, 0.118, 0.090), 0.046, ACC, _pa + 0.15, rot=(math.radians(78), 0, 0), hold=1.8)

# DPI: five stages around the mouse settle to the two you use.
_da = tl.arrival("dpi")
segs = []
for i in range(5):
    a0 = math.radians(i * 72 + 4 - 36)
    a1 = math.radians(i * 72 + 68 - 36)
    s = annular_sector(f"dpi {i}", MOUSE, 0.084, 0.0885, a0, a1, 0.0012)
    s.color = BLACK
    key(s, "color", _da + i * 0.06, BLACK)
    key(s, "color", _da + i * 0.06 + 0.15, hexlin(tl.TEXT_DIM))
    if i < 2:
        key(s, "color", _da + 0.55, hexlin(tl.TEXT_DIM))
        key(s, "color", _da + 0.72, ACC if i == 1 else hexlin(tl.TEXT))
        key(s, "color", _da + 1.9, ACC if i == 1 else hexlin(tl.TEXT))
        key(s, "color", _da + 2.5, BLACK)
        gone(s, _da + 2.5)
    else:
        key(s, "color", _da + 0.45 + i * 0.05, hexlin(tl.TEXT_DIM))
        key(s, "color", _da + 0.70 + i * 0.05, BLACK)
        gone(s, _da + 0.70 + i * 0.05)
    segs.append(s)
for i, lab in enumerate(("800", "1600")):
    a = math.radians(i * 72)
    p = MOUSE + Vector((math.sin(a) * 0.104, math.cos(a) * 0.104, 0.0013))
    mono_label(f"dpi lab {lab}", lab, p, 0.0085, ACC if i == 1 else hexlin(tl.TEXT_MID), _da + 0.7, rot=(0, 0, 0), hold=1.2, fade=0.5)

# Mute: a wide, restless ring at the mic settles into a tight steady band.
_ma = tl.arrival("mute") + 0.85
bpy.ops.mesh.primitive_torus_add(major_radius=0.06, minor_radius=0.0012, location=MIC + Vector((0, 0, 0.0015)), major_segments=96)
MIC_RING = bpy.context.object
MIC_RING.data.materials.append(M_MECH)
MIC_RING.color = BLACK
key(MIC_RING, "color", _ma - 0.01, BLACK)
key(MIC_RING, "color", _ma + 0.1, ACC)
key(MIC_RING, "color", _ma + 1.4, ACC)
key(MIC_RING, "color", _ma + 2.2, (ACC[0] * 0.12, ACC[1] * 0.12, ACC[2] * 0.12, 1))
key(MIC_RING, "color", 20.0, (ACC[0] * 0.12, ACC[1] * 0.12, ACC[2] * 0.12, 1))
key(MIC_RING, "color", 20.6, BLACK)
gone(MIC_RING, 20.6)
_rng = np.random.default_rng(3)
for i in range(0, 22):
    tt = _ma + i * 0.05
    settle = max(0.0, 1 - i / 18)
    sc = 1.0 + 0.7 * settle + 0.12 * settle * _rng.uniform(-1, 1)
    key(MIC_RING, "scale", tt, (sc, sc * (1 + 0.06 * settle * _rng.uniform(-1, 1)), 1))
key(MIC_RING, "scale", _ma - 0.01, (1.7, 1.7, 1))

# Radial: the cast wheel assembles around the mouse; wedge 2 takes the macro.
_ra = tl.arrival("radial")
_r0 = _ra - 0.35
n = tl.RADIAL_SECTORS
for i in range(n):
    span = 2 * math.pi / n
    a0, a1 = -span / 2 + math.radians(2.0), span / 2 - math.radians(2.0)
    w = annular_sector(f"wedge {i}", (0, 0), 0.072, 0.122, a0, a1, 0.0, seg=20)
    pivot = link(bpy.data.objects.new(f"wedge pivot {i}", None))
    pivot.location = MOUSE + Vector((0, 0, 0.0018))
    w.parent = pivot
    w.location = (0, 0, 0)
    ang = -i * span
    t_in = _r0 + i * 0.055
    key(pivot, "rotation_euler", 0.0, (0, 0, ang + math.radians(40)))
    key(pivot, "rotation_euler", t_in, (0, 0, ang + math.radians(40)))
    interp(pivot, t_in, "BACK", "EASE_OUT")
    key(pivot, "rotation_euler", t_in + 0.45, (0, 0, ang))
    key(pivot, "scale", 0.0, (0.0, 0.0, 1))
    key(pivot, "scale", t_in - 0.01, (0.0, 0.0, 1))
    key(pivot, "scale", t_in, (1.5, 1.5, 1))
    interp(pivot, t_in, "BACK", "EASE_OUT")
    key(pivot, "scale", t_in + 0.45, (1, 1, 1))
    base = (0.012, 0.014, 0.018, 1)
    w.color = BLACK
    key(w, "color", t_in, BLACK)
    key(w, "color", t_in + 0.3, base)
    if i == tl.RADIAL_WEDGE:
        key(w, "color", _ra - 0.02, base)
        key(w, "color", _ra + 0.08, (ACC[0] * 2.2, ACC[1] * 2.2, ACC[2] * 2.2, 1))
        key(w, "color", _ra + 0.6, ACC)
    key(w, "color", 19.9, ACC if i == tl.RADIAL_WEDGE else base)
    key(w, "color", 20.5, BLACK)
    gone(w, 20.5)
_lab_dir = Vector((math.sin(_wedge_dir), math.cos(_wedge_dir), 0))
mono_label("macro", tl.RADIAL_LABEL, MOUSE + _lab_dir * 0.142 + Vector((0, 0, 0.0015)), 0.0105, ACC, _ra + 0.25,
           hold=19.9 - (_ra + 0.25), fade=0.5)

# ----------------------------------------------------------------------------- keypresses

# The same presses the pattern engine saw, so a key goes down on the frame its heat lands.
L = np.load(os.path.join(tl.OUT, "lighting.npz"))
KEY_BY_CELL = {k["index"]: k for k in KEYS}
_down = {}
for d, u, cell in L["presses"]:
    k = KEY_BY_CELL.get(int(cell))
    if k:
        _down.setdefault(k["name"], []).append((d, u))
for name, spans in _down.items():
    k = KEY_BY_NAME[name]
    for o in (k["cap"], k["legend"]):
        if o is None:
            continue
        base = o.location.copy()
        last = -1.0
        for d, u in sorted(spans):
            if d - last < 0.08:
                continue
            key(o, "location", max(0.0, d - 0.03), base)
            key(o, "location", d + 0.012, base - Vector((0, 0, 0.0028)))
            key(o, "location", max(d + 0.03, u), base - Vector((0, 0, 0.0028)))
            key(o, "location", max(d + 0.03, u) + 0.05, base)
            last = u + 0.05

# ----------------------------------------------------------------------------- camera

CAM_D = bpy.data.cameras.new("cam")
CAM = link(bpy.data.objects.new("cam", CAM_D))
TARGET = link(bpy.data.objects.new("cam target", None))
tc = CAM.constraints.new("TRACK_TO")
tc.target = TARGET
tc.track_axis, tc.up_axis = "TRACK_NEGATIVE_Z", "UP_Y"
SCN.camera = CAM
CAM_D.sensor_fit = "VERTICAL"
CAM_D.sensor_height = 36
CAM_D.clip_start = 0.01
CAM_D.dof.use_dof = True
CAM_D.dof.aperture_fstop = 4.0
CAM_D.dof.focus_object = TARGET

for t, name in tl.CAM_KEYS:
    loc, tgt, lens = tl.SHOTS[name]
    key(CAM, "location", t, Vector(loc))
    key(TARGET, "location", t, Vector(tgt))
    key(CAM_D, "lens", t, lens)
for idb in (CAM, TARGET, CAM_D):
    for fc in fcurves(idb):
        for p in fc.keyframe_points:
            p.interpolation = "BEZIER"
            p.handle_left_type = p.handle_right_type = "AUTO_CLAMPED"


# ----------------------------------------------------------------------------- render setup

R = SCN.render
R.engine = "CYCLES"
SCN.cycles.device = "GPU"
prefs = bpy.context.preferences.addons["cycles"].preferences
prefs.compute_device_type = "OPTIX"
prefs.get_devices()
for d in prefs.devices:
    d.use = d.type == "OPTIX"
SCN.cycles.samples = A.samples
SCN.cycles.use_denoising = True
SCN.cycles.denoiser = "OPTIX"
SCN.cycles.max_bounces = 6
SCN.cycles.caustics_reflective = False
SCN.cycles.caustics_refractive = False
R.resolution_x, R.resolution_y = tl.W, tl.H
R.resolution_percentage = int(A.scale * 100)
R.fps = tl.FPS
R.use_motion_blur = True
R.motion_blur_shutter = 0.25
R.use_persistent_data = True
R.image_settings.file_format = "PNG"
SCN.frame_start, SCN.frame_end = 1, tl.frames()
try:
    SCN.view_settings.view_transform = "AgX"
    SCN.view_settings.look = "AgX - Medium High Contrast"
except TypeError:
    pass

tree = bpy.data.node_groups.new("comp", "CompositorNodeTree")
SCN.compositing_node_group = tree
tree.interface.new_socket("Image", in_out="OUTPUT", socket_type="NodeSocketColor")
rl = tree.nodes.new("CompositorNodeRLayers")
gl = tree.nodes.new("CompositorNodeGlare")
gl.inputs["Type"].default_value = "Bloom"
gl.inputs["Quality"].default_value = "High"
gl.inputs["Threshold"].default_value = 0.6
gl.inputs["Strength"].default_value = 0.55
gl.inputs["Size"].default_value = 0.7
go = tree.nodes.new("NodeGroupOutput")
tree.links.new(rl.outputs["Image"], gl.inputs["Image"])
tree.links.new(gl.outputs["Image"], go.inputs[0])

# ----------------------------------------------------------------------------- per-frame light

KB_LIN = srgb_lin(L["kb"])
MOUSE_LIN = srgb_lin(L["mouse"])
M_COLLAR.node_tree.nodes["Emission"].inputs["Strength"].default_value = A.led
M_LEGEND.node_tree.nodes["Emission"].inputs["Strength"].default_value = A.led * 1.3
M_ZONE.node_tree.nodes["Emission"].inputs["Strength"].default_value = A.led * 1.2


def paint(f):
    i = min(f - 1, len(KB_LIN) - 1)
    g = KB_LIN[i]
    for k in KEYS:
        c = (*g[k["index"]], 1.0)
        k["collar"].color = c
        k["cap"].color = c
        if k["legend"] is not None:
            k["legend"].color = c
    m = MOUSE_LIN[i]
    M_WHEEL.color = (*m[0], 1)
    M_LOGO.color = (*m[1], 1)
    for o in M_PLATE_LEDS:
        o.color = (*m[2], 1)
    # The Seiren's own status light; Neuron does not drive it.
    MIC_LED.color = (0.05, 0.055, 0.06, 1)
    global PANE_IMG
    path = os.path.join(tl.OUT, "term", f"{f:05d}.png")
    if os.path.exists(path):
        if PANE_IMG is None:
            PANE_IMG = bpy.data.images.load(path)
            _pt.image = PANE_IMG
        else:
            PANE_IMG.filepath = path
            PANE_IMG.reload()


def wanted():
    if A.frames:
        return [int(x) for x in A.frames.split(",")]
    first, last = 1, tl.frames()
    if A.range:
        first, last = (int(x) for x in A.range.split(":"))
    return list(range(first, last + 1, A.every))


os.makedirs(A.out, exist_ok=True)
paint(1)
if A.save:
    bpy.ops.wm.save_as_mainfile(filepath=os.path.join(tl.OUT, "dev-ad.blend"))
for f in wanted():
    path = os.path.join(A.out, f"{f:05d}.png")
    if A.resume and os.path.exists(path):
        continue
    SCN.frame_set(f)
    paint(f)
    R.filepath = path
    bpy.ops.render.render(write_still=True)
    print(f"frame {f} -> {path}", flush=True)
