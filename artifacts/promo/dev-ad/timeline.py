# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""The dev ad's single source of truth: what is typed, when, and what it causes.

Every other script (terminal frames, lighting sim, Blender scene, assembly) reads
times from here, so retiming a beat is one edit.
"""
import os

FPS = int(os.environ.get("PROMO_FPS", "30"))
DURATION = 23.0
W, H = 1080, 1920

OUT = os.environ.get("PROMO_OUT", r"D:\build-cache\promo\dev-ad")

# Palette: crates/neuron-app/ui/theme.slint.
BG = "#040506"
BG2 = "#070809"
LINE = "#181b21"
TEXT = "#e8ecf1"
TEXT_MID = "#9aa1ac"
TEXT_DIM = "#5d646f"
TEXT_FAINT = "#353b44"
ACCENT = "#4af2b0"
LISTEN = "#a58bff"
WARN = "#f2b34a"

# Terminal script. kind: user (typed, keys press on the board), agent (streamed),
# tool (a neuron call; `event` names what it does to the rig), out (tool output),
# cont (a tool call's wrapped continuation).
SCRIPT = [
    dict(t=0.45, kind="user", text="i'm lazy. set up my devices."),
    dict(t=1.95, kind="tool", text="neuron control list"),
    dict(t=2.15, kind="out", text="Naga V2 Pro           mouse", event="wake_mouse"),
    dict(t=2.32, kind="out", text="BlackWidow Chroma V2  keyboard", event="wake_keyboard"),
    dict(t=2.49, kind="out", text="Seiren V3 Mini        mic", event="wake_mic"),
    dict(t=3.75, kind="agent", text="you code a lot."),
    dict(t=4.30, kind="user", text="unfortunately."),
    dict(t=5.15, kind="tool", text="neuron profile new dev", event="profile"),
    dict(t=6.35, kind="agent", text="5 dpi stages. you need 2."),
    dict(t=7.00, kind="tool", text="neuron feel stages 800 1600", event="dpi"),
    dict(t=8.45, kind="tool", text="neuron bind add --trigger mouse:5", event="mute"),
    dict(t=8.62, kind="cont", text="--action mute:mic"),
    dict(t=9.75, kind="user", text='also the "is this true?" thing'),
    dict(t=11.10, kind="tool", text="neuron macro add fact-check"),
    dict(t=11.45, kind="tool", text="neuron cast wedge set 4", event="radial"),
    dict(t=11.60, kind="cont", text="--action macro:fact-check"),
    dict(t=13.55, kind="user", text="also make it pretty"),
    dict(t=14.55, kind="agent", text="finally, a real requirement."),
    dict(t=15.20, kind="tool", text="neuron light stack add", event="aurora"),
    dict(t=15.36, kind="cont", text="--preset aurora"),
    dict(t=17.55, kind="agent", text="done. go write code."),
]

TYPE_CPS = 24.0   # user typing speed, jittered per char
STREAM_CPS = 70.0  # agent text streaming speed

# Rig-level beats that are not a terminal line.
COLLAPSE_T = 18.35   # terminal folds away
DROP_T = 18.95       # its last light falls into the keyboard
ENDCARD_T = 20.4     # end card fades in
TAGLINE = "your devices can figure it out."

THREAD_TRAVEL = 0.45  # seconds a light thread takes from its terminal line to the rig

RADIAL_SECTORS = 8
RADIAL_WEDGE = 4     # 0 = north, clockwise
RADIAL_LABEL = "fact-check"


def frame(t):
    return int(round(t * FPS)) + 1


def frames():
    return int(DURATION * FPS)


def event_time(name):
    for line in SCRIPT:
        if line.get("event") == name:
            return line["t"]
    raise KeyError(name)


def arrival(name):
    """When the thread for an event lands on the rig."""
    return event_time(name) + THREAD_TRAVEL


def typed_chars():
    """(time, char) for every user keystroke, with deterministic human jitter."""
    import random
    rnd = random.Random(7)
    out = []
    for line in SCRIPT:
        if line["kind"] != "user":
            continue
        t = line["t"]
        for ch in line["text"]:
            out.append((t, ch))
            gap = 1.0 / TYPE_CPS * rnd.uniform(0.6, 1.45)
            if ch == " ":
                gap *= 1.25
            t += gap
    return out


def line_reveal(line):
    """Seconds from a line's start until it is fully shown."""
    n = len(line["text"])
    if line["kind"] == "user":
        return sum(1 for _ in line["text"]) / TYPE_CPS * 1.03
    if line["kind"] == "agent":
        return n / STREAM_CPS
    return 0.12
