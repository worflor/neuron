# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""The dev ad's single source of truth: what is typed, when, what it causes, where we look, and
what the music does about it.

Everything runs on a 120 BPM grid (a beat is 0.5 s, a bar 2 s): thread landings sit on beats so
the picture cuts like a music video. Every other script reads from here.
"""
import os

FPS = int(os.environ.get("PROMO_FPS", "30"))
BPM = 120
BEAT = 60.0 / BPM
BAR = 4 * BEAT
DURATION = 42.0
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

AGENT_LABEL = "your coding agent"

# Terminal script. kind: user (typed on the board), voice (spoken: transcribes beside a waveform,
# no keys move), agent (streamed), tool (a neuron call; `event` names what it does to the rig),
# out (the CLI's real reply), cont (a call's wrapped continuation). `grid` quantizes a typed line
# to the beat. Every tool line and reply was run against v0.1.4 in a scratch run root.
SCRIPT = [
    dict(t=0.50, kind="user", text="i'm lazy. set up my devices."),
    dict(t=2.45, kind="tool", text="neuron control list"),
    dict(t=2.95, kind="out", text="Naga V2 Pro           mouse", event="wake_mouse"),
    dict(t=3.20, kind="out", text="BlackWidow Chroma V2  keyboard", event="wake_keyboard"),
    dict(t=3.45, kind="out", text="Seiren V3 Mini        mic", event="wake_mic"),
    dict(t=4.90, kind="agent", text="you code a lot."),
    dict(t=5.70, kind="user", text="unfortunately."),
    dict(t=7.45, kind="tool", text="neuron profile new dev", event="profile"),
    dict(t=7.65, kind="out", text="created profile 'dev'"),
    dict(t=8.90, kind="agent", text="5 dpi stages. you need 2."),
    dict(t=9.95, kind="tool", text="neuron feel stages 800 1600", event="dpi"),
    dict(t=12.45, kind="tool", text="neuron bind add --trigger mouse:5", event="mute"),
    dict(t=12.60, kind="cont", text="--action mute:mic"),
    dict(t=12.80, kind="out", text="added: [0] Mouse 5 (thumb 2)  ->  mic mute [toggle]"),
    dict(t=14.70, kind="user", text='also the "is this true?" thing'),
    dict(t=17.00, kind="tool", text="neuron macro add fact-check"),
    dict(t=17.45, kind="tool", text="neuron cast wedge set 4", event="radial"),
    dict(t=17.60, kind="cont", text="--action macro:fact-check"),
    dict(t=20.00, kind="user", text="now make it pretty."),
    dict(t=21.45, kind="tool", text="neuron light stack add --profile dev", event="fire"),
    dict(t=21.60, kind="cont", text="--preset fire"),
    dict(t=21.85, kind="out", text="added [0] heat (fire)  blend=normal"),
    dict(t=23.50, kind="agent", text="how's fire?"),
    dict(t=24.10, kind="user", text="too animated."),
    dict(t=25.40, kind="agent", text="right. you're a vampire who never leaves his room."),
    dict(t=26.80, kind="agent", text="this one's more your speed."),
    dict(t=27.45, kind="tool", text="neuron light stack rm 0 --profile dev", event="fire_off"),
    dict(t=27.95, kind="tool", text="neuron light stack add --profile dev", event="heat"),
    dict(t=28.10, kind="cont", text="--preset typingheat"),
    dict(t=29.30, kind="voice", text="what's the gimmick? looks basic."),
    dict(t=31.50, kind="agent", text="type a bit."),
    dict(t=32.25, kind="user", text="ok. that's actually sick.", grid=BEAT / 4),
    dict(t=36.00, kind="agent", text="done. go do something."),
]

TYPE_CPS = 16.5    # user typing speed, jittered per char
STREAM_CPS = 48.0  # agent text streaming speed
VOICE_CPS = 19.0   # how fast a spoken line transcribes
VOICE_LEAD = 0.35  # the waveform moves this long before the first word lands
WRAP = 40          # terminal columns before a line wraps

THREAD_TRAVEL = 0.55  # seconds a light thread takes from its terminal line to the rig

COLLAPSE_T = 37.00   # terminal folds away
DROP_T = 37.45       # its last light falls...
DROP_LAND = 38.00    # ...and presses Enter on the downbeat
ENDCARD_T = 38.60    # end card fades in
TAGLINE = "your devices can figure it out."

RADIAL_SECTORS = 8
RADIAL_WEDGE = 4     # 0 = north, clockwise
RADIAL_LABEL = "fact-check"

# Lighting is the real pattern engine, switched the way the commands above switch it.
LIGHT_SEGMENTS = [(0.0, "off"), ("fire", "fire"), ("fire_off", "off"), ("heat", "typingheat")]

# Camera: (time, shot). Equal neighbours hold; moves ease between them. The camera arrives
# before an effect lands and holds after it.
SHOTS = {
    "term": ((0.0, -0.70, 0.44), (0.0, 0.24, 0.27), 30),
    "wide": ((0.0, -1.05, 0.62), (0.0, 0.08, 0.22), 34),
    "mouse": ((0.42, -0.30, 0.34), (0.285, -0.01, 0.0), 42),
    "mute": ((-0.02, -0.92, 0.50), (0.0, 0.03, 0.06), 30),
    "top": ((0.285, -0.06, 0.62), (0.285, -0.03, 0.0), 34),
    "convo": ((0.0, -0.80, 0.30), (0.0, 0.16, 0.20), 28),
    "hero0": ((-0.16, -0.30, 0.17), (-0.06, 0.01, 0.0), 32),
    "hero1": ((0.04, -0.31, 0.16), (0.10, 0.01, 0.0), 32),
    "end": ((0.0, -0.80, 0.40), (0.0, 0.08, 0.08), 28),
}
CAM_KEYS = [
    (0.0, "term"), (2.40, "term"), (4.20, "wide"),
    (9.50, "wide"), (10.35, "mouse"), (12.00, "mouse"), (12.80, "mute"),
    (14.90, "mute"), (15.50, "wide"),
    (17.00, "wide"), (17.85, "top"), (19.60, "top"), (20.50, "convo"),
    (31.45, "convo"), (32.15, "hero0"), (35.40, "hero1"), (36.20, "wide"),
    (37.30, "wide"), (39.60, "end"), (DURATION, "end"),
]

# Which device the eye should be on: (time, subject). Spotlights and focus follow it.
SUBJECTS = [
    (0.0, None), (3.50, "mouse"), (3.75, "keyboard"), (4.00, "mic"), (4.80, "all"),
    (9.90, "mouse"), (12.30, "all"), (13.85, "mic"), (15.20, "all"), (17.40, "mouse"),
    (20.20, "keyboard"), (29.20, "mic"), (31.40, "keyboard"), (36.80, "all"),
]


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


def light_segments():
    """[(seconds, preset)] with event names resolved to their arrival."""
    return [(when if isinstance(when, float) else arrival(when), preset) for when, preset in LIGHT_SEGMENTS]


def line_times(line):
    """When each char of a typed or spoken line lands."""
    import random
    if line["kind"] == "voice":
        return [line["t"] + VOICE_LEAD + i / VOICE_CPS for i in range(len(line["text"]))]
    if "grid" in line:
        return [line["t"] + i * line["grid"] for i in range(len(line["text"]))]
    rnd = random.Random(int(line["t"] * 1000))
    out, t = [], line["t"]
    for ch in line["text"]:
        out.append(t)
        gap = 1.0 / TYPE_CPS * rnd.uniform(0.6, 1.45)
        if ch == " ":
            gap *= 1.25
        if ch in ".?":
            gap *= 1.6
        t += gap
    return out


def typed_chars():
    """(time, char) for every keystroke. Spoken lines press nothing."""
    out = []
    for line in SCRIPT:
        if line["kind"] == "user":
            out.extend(zip(line_times(line), line["text"]))
    return out


def commit_time(line):
    """When a typed or spoken line leaves the prompt for the history."""
    return line_times(line)[-1] + 0.18


def wrap(text, width=WRAP):
    """Word-wrap a line into terminal rows, keeping the source's own spacing."""
    import re
    rows, cur = [], ""
    for tok in re.findall(r"\S+\s*", text):
        if cur and len(cur + tok.rstrip()) > width:
            rows.append(cur.rstrip())
            cur = tok
        else:
            cur += tok
    rows.append(cur.rstrip())
    return rows
