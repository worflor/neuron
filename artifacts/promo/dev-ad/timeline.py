# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""The dev ad's single source of truth: what is said, typed, caused, looked at and scored.

Everything runs on a 120 BPM grid (a beat is 0.5 s, a bar 2 s): thread landings sit on beats so
the picture cuts like a music video. Every other script reads from here.

World rule: the user talks to the agent through the Seiren. Nothing is typed, no key moves and no
key sounds until the agent says "type a bit."; from then on typing is the point.
"""
import os
import random
import re

FPS = int(os.environ.get("PROMO_FPS", "30"))
BPM = 120
BEAT = 60.0 / BPM
BAR = 4 * BEAT
DURATION = 44.0
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

# Terminal script. kind: voice (spoken into the mic: transcribes beside a waveform, no keys move),
# user (typed: the only typing in the ad), agent (streamed), tool (a CLI call; `event` names what
# it does to the rig), out (the CLI's real reply), cont (a call's wrapped continuation).
# Every tool line and reply was run against v0.1.4 in a scratch run root, except the verbs that
# write devices (`profile apply`, `feel stages`), whose syntax was checked with --help and whose
# replies are not shown. `neuron control list | findstr Razer` prints exactly the three lines shown.
SCRIPT = [
    dict(t=0.50, kind="voice", text="i'm lazy. set up my devices."),
    dict(t=2.55, kind="tool", text="neuron control list | findstr Razer"),
    dict(t=2.95, kind="out", text="Razer Naga V2 Pro  [mouse]  pid=00a7", event="wake_mouse"),
    dict(t=3.20, kind="out", text="Razer BlackWidow Chroma V2  [keyboard]  pid=0221", event="wake_keyboard"),
    dict(t=3.45, kind="out", text="Razer Seiren V3 Mini  [mic]  pid=056a", event="wake_mic"),
    dict(t=4.90, kind="agent", text="you code a lot."),
    dict(t=5.70, kind="voice", text="unfortunately."),
    dict(t=7.15, kind="tool", text="neuron profile new dev"),
    dict(t=7.35, kind="out", text="created profile 'dev'"),
    dict(t=7.95, kind="tool", text="neuron profile apply dev", event="profile"),
    dict(t=9.00, kind="agent", text="5 dpi stages. you need 2."),
    dict(t=9.95, kind="tool", text="neuron feel stages 800 1600", event="dpi"),
    dict(t=12.45, kind="tool", text="neuron bind add --trigger mouse:5", event="mute"),
    dict(t=12.60, kind="cont", text="--action mute:mic"),
    dict(t=12.80, kind="out", text="added: [0] Mouse 5 (thumb 2)  ->  mic mute [toggle]"),
    dict(t=14.70, kind="voice", text='also the "is this true?" thing'),
    dict(t=16.95, kind="tool", text="neuron macro add fact-check fact-check.py"),
    dict(t=17.15, kind="out", text="added macro 'fact-check' (bound, checked + warm)"),
    dict(t=17.45, kind="tool", text="neuron cast wedge set 4", event="radial"),
    dict(t=17.60, kind="cont", text="--action macro:fact-check"),
    dict(t=17.80, kind="out", text="wedge 4 -> script `fact-check` [python]"),
    dict(t=19.45, kind="voice", text="now make it pretty."),
    dict(t=21.45, kind="tool", text="neuron light stack add --profile dev", event="fire"),
    dict(t=21.60, kind="cont", text="--preset fire"),
    dict(t=21.85, kind="out", text="added [0] heat (fire)  blend=normal"),
    dict(t=23.50, kind="agent", text="how's fire?"),
    dict(t=24.10, kind="voice", text="too animated."),
    dict(t=25.40, kind="agent", text="right. you're a vampire who never leaves his room."),
    dict(t=26.80, kind="agent", text="this one's more your speed."),
    dict(t=27.45, kind="tool", text="neuron light stack rm 0 --profile dev", event="fire_off"),
    dict(t=27.65, kind="out", text="removed [0] heat (fire)  blend=normal"),
    dict(t=27.95, kind="tool", text="neuron light stack add --profile dev", event="heat"),
    dict(t=28.10, kind="cont", text="--preset typingheat"),
    dict(t=28.30, kind="out", text="added [0] thermal (typingheat)  blend=normal"),
    dict(t=29.30, kind="voice", text="what's the gimmick? looks basic."),
    dict(t=31.60, kind="agent", text="type a bit."),
    dict(t=34.00, kind="user", text="ok. that's actually sick.", grid=BEAT / 4),
    dict(t=37.75, kind="agent", text="done. go do something."),
]

TYPE_CPS = 16.5    # typing speed, jittered per char
STREAM_CPS = 48.0  # agent text streaming speed
VOICE_LEAD = 0.35  # the waveform moves this long before the first word lands
WRAP = 50          # terminal columns before a line wraps

THREAD_TRAVEL = 0.55  # seconds a light thread takes from its terminal line to the rig

COLLAPSE_T = 38.75   # terminal folds away
DROP_T = 39.45       # its last light falls...
DROP_LAND = 40.00    # ...and lands on the downbeat (nothing presses a key)
ENDCARD_T = 40.45    # end card fades in
TAGLINE = "your devices can figure it out."
# Under the keyboard; the site is the repo's own homepage (gh repo view worflor/neuron).
END_LINKS = ("www.woflo.dev/neuron", "code on github.com/worflor/neuron")

RADIAL_SECTORS = 8
RADIAL_WEDGE = 4     # 0 = north, clockwise
RADIAL_LABEL = "fact-check"

# Lighting is the real pattern engine, switched the way the commands above switch it.
LIGHT_SEGMENTS = [(0.0, "off"), ("fire", "fire"), ("fire_off", "off"), ("heat", "typingheat")]

# Camera: (time, shot). Equal neighbours hold; moves ease between them. The camera arrives
# before an effect lands and holds after it.
SHOTS = {
    "term": ((-0.08, -0.70, 0.44), (-0.03, 0.24, 0.27), 24),
    "wide": ((0.0, -1.05, 0.62), (0.0, 0.08, 0.22), 34),
    "mouse": ((0.42, -0.30, 0.34), (0.285, -0.01, 0.0), 42),
    "mute": ((-0.02, -0.92, 0.50), (0.0, 0.03, 0.06), 30),
    "top": ((0.285, -0.06, 0.62), (0.285, -0.03, 0.0), 34),
    "convo": ((0.0, -0.80, 0.30), (0.0, 0.16, 0.20), 28),
    # The agent's reply, big enough to read before anything else happens.
    "read": ((0.0, -0.62, 0.40), (0.0, 0.24, 0.26), 26),
    "hero0": ((-0.16, -0.30, 0.17), (-0.06, 0.01, 0.0), 32),
    "hero1": ((0.04, -0.31, 0.16), (0.10, 0.01, 0.0), 32),
    "end": ((0.0, -0.80, 0.40), (0.0, 0.08, 0.08), 28),
}
CAM_KEYS = [
    (0.0, "term"), (2.40, "term"), (4.20, "wide"),
    (9.50, "wide"), (10.35, "mouse"), (12.00, "mouse"), (12.80, "mute"),
    (14.90, "mute"), (15.50, "wide"),
    (17.00, "wide"), (17.85, "top"), (19.60, "top"), (20.50, "convo"),
    (31.25, "convo"), (31.95, "read"), (33.25, "read"),   # push in, hold on "type a bit."
    (34.00, "hero0"), (37.15, "hero1"), (37.95, "wide"),   # swing to the board as typing starts
    (39.05, "wide"), (41.60, "end"), (DURATION, "end"),
]

# Which device the eye should be on: (time, subject). Spotlights and focus follow it. The mic takes
# the spotlight whenever someone speaks (see SUBJECTS below).
_BASE_SUBJECTS = [
    (0.0, None), (3.50, "mouse"), (3.75, "keyboard"), (4.00, "mic"), (4.80, "all"),
    (9.90, "mouse"), (12.30, "all"), (13.85, "mic"), (15.20, "all"), (17.40, "mouse"),
    (20.20, "keyboard"), (31.40, "all"), (33.90, "keyboard"), (38.55, "all"),
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


# --- speech --------------------------------------------------------------------------------------
# Speech is a pseudo-voice: words land as text and each syllable is a muted "muh". No real voice,
# no recognition. One schedule feeds the transcript, the waveform, the mic ring and the sound.


def voice_words(line):
    """[{word, i0, i1, t, dur}] with the char span of each word in the line's text."""
    out, pos, t = [], 0, line["t"] + VOICE_LEAD
    for w in re.findall(r"\S+", line["text"]):
        i0 = line["text"].index(w, pos)
        letters = len(re.sub(r"[^A-Za-z0-9]", "", w))
        dur = 0.08 + 0.030 * letters
        out.append(dict(word=w, i0=i0, i1=i0 + len(w), t=t, dur=dur))
        pos = i0 + len(w)
        t += dur + 0.045 + (0.14 if w[-1] in ".?!" else 0.0)
    return out


def voice_syllables(line):
    """[{t, dur, semis, glide}]: one muted syllable per ~3 letters, a question rising and a
    statement falling on its last."""
    out = []
    words = voice_words(line)
    for wi, w in enumerate(words):
        letters = len(re.sub(r"[^A-Za-z0-9]", "", w["word"]))
        n = max(1, round(letters / 3.2))
        rnd = random.Random(f"{line['text']}|{wi}")
        base = rnd.choice([-2, 0, 2, 3])
        for s in range(n):
            glide = -2.0
            if s == n - 1 and wi == len(words) - 1:
                glide = 5.0 if w["word"].rstrip('"').endswith("?") else -4.0
            out.append(dict(t=w["t"] + s * w["dur"] / n, dur=w["dur"] / n * 0.92,
                            semis=base + rnd.choice([-1, 0, 1]), glide=glide))
    return out


def line_times(line):
    """When each char of a spoken or typed line lands (a spoken line lands word by word)."""
    if line["kind"] == "voice":
        words = voice_words(line)
        out, cur = [], words[0]["t"]
        for i in range(len(line["text"])):
            for w in words:
                if w["i0"] <= i < w["i1"]:
                    cur = w["t"]
            out.append(cur)
        return out
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


def commit_time(line):
    """When a spoken or typed line leaves the prompt for the history."""
    if line["kind"] == "voice":
        w = voice_words(line)[-1]
        return w["t"] + w["dur"] + 0.12
    return line_times(line)[-1] + 0.18


def typed_chars():
    """(time, char) for every keystroke. Spoken lines press nothing."""
    out = []
    for line in SCRIPT:
        if line["kind"] == "user":
            out.extend(zip(line_times(line), line["text"]))
    return out


def voice_lines():
    return [l for l in SCRIPT if l["kind"] == "voice"]


def voice_windows():
    """[(start, end)] of each spoken line, from its waveform's first movement to its commit."""
    return [(l["t"], commit_time(l)) for l in voice_lines()]


_SYLLABLES = [s for l in voice_lines() for s in voice_syllables(l)]


def voice_env(t):
    """0..1 loudness of the speech at time t: a faint hold inside a line, a swell per syllable."""
    lvl = 0.0
    for a, b in voice_windows():
        if a <= t <= b:
            lvl = 0.12
    for s in _SYLLABLES:
        x = (t - s["t"]) / s["dur"]
        if 0.0 <= x <= 1.0:
            lvl = max(lvl, 0.15 + 0.85 * (__import__("math").sin(3.141592653589793 * x) ** 0.8))
    return lvl


def _subjects():
    def at(base, t):
        who = None
        for when, w in base:
            if when <= t:
                who = w
        return who
    out = list(_BASE_SUBJECTS)
    for a, b in voice_windows():
        out.append((a, "mic"))
        out.append((b, at(_BASE_SUBJECTS, b)))
    return sorted(out, key=lambda e: e[0])


SUBJECTS = _subjects()


def wrap(text, width=WRAP):
    """Word-wrap a line into terminal rows, keeping the source's own spacing."""
    rows, cur = [], ""
    for tok in re.findall(r"\S+\s*", text):
        if cur and len(cur + tok.rstrip()) > width:
            rows.append(cur.rstrip())
            cur = tok
        else:
            cur += tok
    rows.append(cur.rstrip())
    return rows
