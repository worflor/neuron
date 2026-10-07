# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Score the ad as a chiptune that builds with the setup, compiled by Harmonia.

    <Harmonia>\\.venv\\Scripts\\python.exe gen_music.py

Every layer enters on the beat its cause lands (timeline.py runs on the same 120 BPM grid):
the wake rings, the room brings bass, `profile new dev` brings drums, the DPI stages bring the arp,
the mute bind a counter-line, the radial the backbeat, fire the lead. "too animated." stops the
tape (applied in gen_audio.py), the vampire gets a minor vamp, typing heat a heartbeat, "type a
bit." a beat of silence, and the reveal brings everything back. Writes music.wav.
"""
import os
import sys
from fractions import Fraction

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import timeline as tl

HARMONIA = os.environ.get("HARMONIA_ROOT", r"C:\Users\Micha\Documents\Projects\Harmonia")
sys.path.insert(0, HARMONIA)
from harmonia.generative.compiler import HarmoniaCompiler  # noqa: E402
from harmonia.ir.phrase import Phrase, Tone  # noqa: E402
from harmonia.ir.schema import (ChordDef, FXDef, InstrumentDef, MacroBlock, MasteringConfig,  # noqa: E402
                                MixerDef, Score, SendBusDef, TrackChannel, VoicePocketConfig)
from harmonia.tools.workbench import lint  # noqa: E402


def b(seconds):
    """Seconds on the ad's clock -> beats, exact to a 64th."""
    return Fraction(seconds / tl.BEAT).limit_denominator(64)


def F(x):
    return Fraction(x).limit_denominator(64)


TOTAL = b(tl.DURATION)

# Sections, in seconds, from the picture.
WAKE = [tl.arrival(e) for e in ("wake_mouse", "wake_keyboard", "wake_mic")]
ROOM = 4.0
DRUMS = tl.arrival("profile")
ARP = tl.arrival("dpi")
COUNTER = tl.arrival("mute")
BACKBEAT = tl.arrival("radial")
FIRE = tl.arrival("fire")
_too = next(l for l in tl.SCRIPT if l["text"] == "too animated.")
STOP = tl.commit_time(_too)            # the tape stops here (gen_audio.py)
VAMP = STOP + 0.5
HEART = tl.arrival("heat")
_tab = next(l for l in tl.SCRIPT if l["text"] == "type a bit.")
HUSH = _tab["t"]
REVEAL = 32.0
THIN = 36.0
FALL = tl.COLLAPSE_T
HIT = tl.DROP_LAND

# A minor, one chord a bar: Am F C G.
PROG = [(45, (57, 60, 64)), (41, (53, 57, 60)), (48, (55, 60, 64)), (43, (55, 59, 62))]
VAMP_PROG = [(45, (57, 60, 64)), (40, (56, 59, 64))]   # Am, E: the gothic turn


def chord_at(beat):
    return PROG[int(beat // 4) % 4]


class Line:
    def __init__(self):
        self.tones = []

    def note(self, at_beat, dur, pitch, vel=0.8):
        self.tones.append(Tone(F(at_beat), F(dur), int(pitch), float(vel)))

    def events(self, ns):
        tones = tuple(sorted(self.tones, key=lambda t: (t.start, t.pitch)))
        return Phrase(TOTAL + 8, tones).place(0, namespace=ns)


def span(t0, t1, step):
    """Beats from t0 to t1 (seconds) every `step` beats, aligned to the grid."""
    s = b(t0)
    s = Fraction(-(-s // F(step))) * F(step) if s % F(step) else s
    while s < b(t1):
        yield s
        s += F(step)


bass, arp, counter, lead, pad, kick, hat, snare, chime, vamp, crash = (Line() for _ in range(11))


def groove(t0, t1, *, hats=0.5, kick_step=2, snare_on=True):
    for s in span(t0, t1, kick_step):
        kick.note(s, 0.25, 36, 0.95)
    for s in span(t0, t1, hats):
        if hats == 0.5 and s % 1 == 0:
            continue
        hat.note(s, 0.12, 96, 0.55 if s % 1 else 0.4)
    if snare_on:
        for s in span(t0, t1, 1):
            if int(s) % 2 == 1:
                snare.note(s, 0.25, 60, 0.7)


def bassline(t0, t1, prog=PROG, octave_hop=True, vel=0.8):
    for s in span(t0, t1, 0.5):
        root = prog[int(s // 4) % len(prog)][0]
        hop = 12 if octave_hop and (s * 2) % 2 == 1 else 0
        bass.note(s, 0.45, root + hop, vel)


def arpeggio(t0, t1, vel=0.5):
    pattern = (0, 1, 2, 1)
    for s in span(t0, t1, 0.25):
        _, tri = chord_at(s)
        i = int(s * 4) % 4
        arp.note(s, 0.22, tri[pattern[i]] + 12, vel)


def pads(t0, t1, vel=0.35):
    for s in span(t0, t1, 4):
        _, tri = chord_at(s)
        length = min(F(4), b(t1) - s)
        for p in tri:
            pad.note(s, length, p, vel)


# 1. The devices answer: three notes of the tonic, one per wake.
for t, p in zip(WAKE, (69, 72, 76)):
    chime.note(b(t), 3, p, 0.6)

# 2. The room comes up: triangle bass and a soft pad.
bassline(ROOM, DRUMS, octave_hop=False, vel=0.4)
bassline(DRUMS, STOP, octave_hop=False, vel=0.6)
pads(6.0, STOP, 0.25)

# 3. A profile exists: drums.
groove(DRUMS, BACKBEAT, snare_on=False)

# 4. DPI stages: the arp.
arpeggio(ARP, FIRE, 0.42)

# 5. The mute bind: a counter-line that answers every other bar.
CALL = [(0, 1, 76), (1, 0.5, 74), (1.5, 0.5, 72), (2, 2, 69)]
for bar0 in span(COUNTER, FIRE, 8):
    for off, dur, p in CALL:
        counter.note(bar0 + 4 + F(off), dur, p, 0.45)

# 6. The radial: backbeat, and the bass starts hopping octaves.
groove(BACKBEAT, STOP)
bass.tones = [t for t in bass.tones if t.start < b(BACKBEAT)]
bassline(BACKBEAT, STOP)

# 7. Fire: the lead, the arp up an octave, 16th hats.
MOTIF = [(0, 1, 81), (1, 0.5, 84), (1.5, 0.5, 88), (2, 1, 86), (3, 1, 84),
         (4, 1, 81), (5, 1, 79), (6, 2, 81)]
for bar0 in span(FIRE, STOP, 8):
    for off, dur, p in MOTIF:
        if bar0 + F(off) < b(STOP):
            lead.note(bar0 + F(off), dur, p, 0.6)
arpeggio(FIRE, STOP, 0.5)
for s in span(FIRE, STOP, 0.25):
    hat.note(s, 0.08, 98, 0.35)
crash.note(b(FIRE), 6, 100, 0.6)

# 8. "too animated." The tape stops; then the vampire's minor vamp.
for s in span(VAMP, HEART, 2):
    root, tri = VAMP_PROG[int((s - b(VAMP)) // 2) % 2]
    for p in tri:
        vamp.note(s, 2, p - 12, 0.5)
    bass.note(s, 2, root - 12, 0.7)

# 9. Typing heat: a heartbeat, until "type a bit." asks for quiet.
for s in span(HEART, HUSH, 2):
    kick.note(s, 0.25, 33, 0.7)
    kick.note(s + F(0.5), 0.25, 33, 0.45)
for s in span(HEART, HUSH, 4):
    pad.note(s, min(F(4), b(HUSH) - s), 57, 0.2)

# 10. The reveal: everything, the lead an octave up.
bassline(REVEAL, THIN, vel=0.95)
groove(REVEAL, THIN, hats=0.25)
arpeggio(REVEAL, THIN, 0.6)
for bar0 in span(REVEAL, THIN, 8):
    for off, dur, p in CALL:
        counter.note(bar0 + 4 + F(off), dur, p, 0.55)
pads(REVEAL, THIN, 0.3)
for bar0 in span(REVEAL, THIN, 8):
    for off, dur, p in MOTIF:
        lead.note(bar0 + F(off), dur, p + 12 if off < 4 else p, 0.6)
crash.note(b(REVEAL), 6, 100, 0.7)

# 11. "done.": thin to arp and pad, fall away with the terminal.
arpeggio(THIN, FALL, 0.35)
pads(THIN, FALL + 0.4, 0.25)

# 12. Enter: the last downbeat, then the end card rings out.
for p in (45, 57, 60, 64, 71):
    vamp.note(b(HIT), 6, p, 0.6)
bass.note(b(HIT), 4, 33, 0.9)
kick.note(b(HIT), 0.5, 36, 1.0)
crash.note(b(HIT), 8, 100, 0.8)
for s in span(HIT + 1.0, tl.DURATION - 1.0, 0.5):
    _, tri = PROG[0]
    arp.note(s, 0.4, tri[int(s * 2) % 3] + 12, 0.25)
pads(HIT, tl.DURATION, 0.3)


def chip(wave, **kw):
    base = dict(wave=wave, voices=1, detune_cents=0.0, width=0.0, cutoff_hz=6000.0, resonance=0.5,
                env_amount=0.0, keytrack=0.0, drive=0.0, attack_s=0.002, decay_s=0.15, sustain=0.6,
                release_s=0.05, level=0.5)
    base.update(kw)
    return base


INSTRUMENTS = {
    "pulse": InstrumentDef("pulse", "poly_synth", chip("square", cutoff_hz=3600.0)),
    "tri": InstrumentDef("tri", "poly_synth", chip("triangle", sustain=0.8, release_s=0.03)),
    "lead": InstrumentDef("lead", "poly_synth", chip("square", cutoff_hz=4200.0, sustain=0.7, release_s=0.12,
                                                     lfos=[{"target": "pitch", "rate_hz": 5.5, "depth": 0.15}])),
    "pad": InstrumentDef("pad", "poly_synth", chip("saw", voices=3, detune_cents=9.0, width=0.6, cutoff_hz=1400.0,
                                                   attack_s=0.25, sustain=0.9, release_s=0.6, level=0.3)),
    "organ": InstrumentDef("organ", "poly_synth", chip("square", voices=2, detune_cents=6.0, cutoff_hz=1100.0,
                                                       attack_s=0.08, sustain=0.9, release_s=0.4, level=0.35)),
    "kick": InstrumentDef("kick", "poly_synth", chip("triangle", pitch_env_semitones=30.0, pitch_env_s=0.035,
                                                     decay_s=0.12, sustain=0.0, release_s=0.02, level=0.9)),
    "noise": InstrumentDef("noise", "poly_synth", chip("square", noise=1.0, cutoff_hz=9000.0, decay_s=0.04,
                                                       sustain=0.0, release_s=0.02, level=0.35)),
    "snare": InstrumentDef("snare", "poly_synth", chip("triangle", noise=0.85, cutoff_hz=5000.0, decay_s=0.09,
                                                       sustain=0.0, release_s=0.03, pitch_env_semitones=12.0,
                                                       pitch_env_s=0.02, level=0.55)),
    "crash": InstrumentDef("crash", "poly_synth", chip("square", noise=1.0, cutoff_hz=11000.0, decay_s=1.2,
                                                       sustain=0.0, release_s=0.4, level=0.18)),
    "chime": InstrumentDef("chime", "music_box", dict(ring_s=2.2, brightness=0.6)),
}

CHANNELS = [
    TrackChannel("bass", "tri", bass.events("bass"), volume_db=6.0),
    TrackChannel("arp", "pulse", arp.events("arp"), sends={"room": 0.15}, volume_db=-18.0, pan=0.25),
    TrackChannel("counter", "tri", counter.events("counter"), sends={"room": 0.2}, volume_db=0.0, pan=-0.3),
    TrackChannel("lead", "lead", lead.events("lead"), sends={"room": 0.2, "echo": 0.18}, volume_db=-16.0),
    TrackChannel("pad", "pad", pad.events("pad"), sends={"room": 0.35}, volume_db=-12.0),
    TrackChannel("vamp", "organ", vamp.events("vamp"), sends={"room": 0.4}, volume_db=-17.0),
    TrackChannel("kick", "kick", kick.events("kick"), volume_db=8.0),
    TrackChannel("hat", "noise", hat.events("hat"), volume_db=-14.0, pan=0.15),
    TrackChannel("snare", "snare", snare.events("snare"), sends={"room": 0.12}, volume_db=-2.0),
    TrackChannel("crash", "crash", crash.events("crash"), sends={"room": 0.3}, volume_db=-14.0),
    TrackChannel("chime", "chime", chime.events("chime"), sends={"room": 0.35}, volume_db=-15.0),
]


def compose():
    return Score(
        title="neuron: i'm lazy", seed=20261006, bpm=tl.BPM, key_root="A", scale_type="natural_minor",
        swing_ratio=0.5, chords={"Am": [ChordDef("Am", 45, [], [], [], [])]},
        timeline=[MacroBlock("ad", "Am", int(-(-TOTAL // 4)), has_drums=False, has_bass=False, has_pad=False,
                             has_stabs=False, has_bells=False)],
        instruments=INSTRUMENTS, track_channels=CHANNELS,
        mixer=MixerDef(send_busses={
            "room": SendBusDef("room", [FXDef("reverb", {"decay_time_s": 1.4, "damp_hz": 5200}, 1)]),
            "echo": SendBusDef("echo", [FXDef("delay", {"delay_ms": 375.0, "feedback": 0.3}, 1)]),
        }),
        mastering=MasteringConfig(target_lufs=-18.0, peak_ceiling_db=-2.0, tape_warmth_enabled=True,
                                  sidechain_duck_db=0.0, voice_pocket=VoicePocketConfig(enabled=False)),
        metadata={"artist": "neuron", "comment": "Score for the dev ad; layers follow the setup."},
    )


def main():
    score = compose()
    report = lint(score)
    problems = [d for d in report.get("diagnostics", []) if d.get("severity") == "error"]
    for d in report.get("diagnostics", []):
        print(f"[{d.get('severity')}] {d.get('path')}: {d.get('message')}")
    if problems:
        raise SystemExit("score does not lint")
    out = os.path.join(tl.OUT, "music.wav")
    HarmoniaCompiler(score).compile(out, export_mp3=False)
    print("wrote", out, "stop at", round(STOP, 3), "s")


if __name__ == "__main__":
    main()
