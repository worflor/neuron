# SPDX-FileCopyrightText: 2026 Woflo Labs
# SPDX-License-Identifier: GPL-3.0-or-later
"""Synthesize the ad's sound from the same timeline the picture uses.

Every sound is caused by something on screen: a keystroke, a call, a thread leaving and landing,
a mechanism locking, the board's real heat level. Blips use Neuron's own soft-pulse FM voice
(crates/neuron-core/src/tone.rs). Writes audio.wav (48 kHz stereo) and prints its levels; it is
never played back here.
"""
import json
import math
import os
import sys

import numpy as np
from scipy import signal
from scipy.io import wavfile

sys.path.insert(0, os.path.dirname(__file__))
import timeline as tl
from gen_lighting import presses

SR = 48000
N = int(tl.DURATION * SR)
RNG = np.random.default_rng(11)
HERE = os.path.dirname(os.path.abspath(__file__))
KEYMAP = {k["name"]: k for k in json.load(open(os.path.join(HERE, "keymap.json")))["key_map"]}

MIX = np.zeros((2, N))
PAN = {"mouse": 0.65, "keyboard": 0.0, "mic": -0.65, "center": 0.0}


def place(buf, t, pan=0.0, gain=1.0):
    """Add a mono buffer at time t with constant-power pan (-1 left .. 1 right)."""
    i = int(t * SR)
    if i >= N or i + len(buf) <= 0:
        return
    j = min(N, i + len(buf))
    seg = buf[max(0, -i):j - i] * gain
    a = (pan + 1) * math.pi / 4
    MIX[0, max(0, i):j] += seg * math.cos(a)
    MIX[1, max(0, i):j] += seg * math.sin(a)


def env(n, attack, t60):
    t = np.arange(n) / SR
    a = np.clip(t / max(attack, 1e-4), 0, 1)
    a = 0.5 - 0.5 * np.cos(np.pi * a)
    return a * np.exp(-6.907755 * t / t60)


def fm(freq, dur, ratio=1.0, index=0.6, index_tau=0.09, attack=0.008, t60=0.30):
    """Two-operator FM, the tone.rs construction: decaying index, raised-cosine attack."""
    n = int(dur * SR)
    t = np.arange(n) / SR
    idx = index * np.exp(-t / index_tau)
    mod = np.sin(2 * np.pi * freq * ratio * t)
    return np.sin(2 * np.pi * freq * t + idx * mod) * env(n, attack, t60)


def hz(semis):
    return 440.0 * 2 ** (semis / 12)


PENTA = [0, 2, 4, 7, 9]


def penta(deg, root=-9):
    o, d = divmod(deg, 5)
    return hz(root + 12 * o + PENTA[d])


def bandnoise(n, lo, hi):
    b, a = signal.butter(2, [lo / (SR / 2), min(hi, SR / 2 - 100) / (SR / 2)], "band")
    return signal.lfilter(b, a, RNG.standard_normal(n))


def click(heavy=1.0):
    """A key switch: a bright tick, the cap's thock, a faint upstroke."""
    n = int(0.09 * SR)
    t = np.arange(n) / SR
    tick = bandnoise(n, 2500, 9000) * np.exp(-t / 0.0035) * 0.55
    f = RNG.uniform(150, 210) / heavy
    thock = np.sin(2 * np.pi * f * t) * np.exp(-t / (0.018 * heavy)) * 0.9
    body = bandnoise(n, 400, 1800) * np.exp(-t / 0.008) * 0.35 * heavy
    return tick + thock + body


def whoosh(dur, rising=True):
    n = int(dur * SR)
    t = np.arange(n) / SR
    out = np.zeros(n)
    noise = RNG.standard_normal(n)
    # Sweep a resonant band by filtering in short blocks.
    blk = 512
    zi = None
    for s in range(0, n, blk):
        k = s / n
        fc = 500 + (2600 if rising else -0) * (k if rising else 1 - k)
        b, a = signal.butter(2, [fc * 0.7 / (SR / 2), fc * 1.4 / (SR / 2)], "band")
        if zi is None:
            zi = signal.lfilter_zi(b, a) * 0
        out[s:s + blk], zi = signal.lfilter(b, a, noise[s:s + blk], zi=zi)
    shape = np.sin(np.pi * np.clip(t / dur, 0, 1)) ** 1.5
    return out * shape


def bell(freq, t60=1.1):
    return fm(freq, t60 + 0.1, ratio=1.4142, index=2.2, index_tau=0.12, attack=0.004, t60=t60)


# --- room tone -----------------------------------------------------------------------------------
t_all = np.arange(N) / SR
room = signal.lfilter(*signal.butter(1, 220 / (SR / 2)), RNG.standard_normal(N)) * 0.020
room += np.sin(2 * np.pi * 55 * t_all) * 0.008
fade_in = np.clip(t_all / 1.5, 0, 1)
MIX += np.vstack([room, np.roll(room, 997)]) * fade_in

# --- speech: a muted "muh" per syllable ------------------------------------------------------------
# Not a voice and not a voice effect: a harmonic buzz through a low-pass that closes on an "m" and
# opens on an "uh". Same schedule as the waveform and the mic ring (timeline.voice_syllables).


def muh(dur, f0, glide):
    n = max(8, int(dur * SR))
    t = np.arange(n) / SR
    u = t / dur
    f = f0 * 2 ** (glide * u ** 1.6 / 12) * (1 + 0.012 * np.sin(2 * np.pi * 5.5 * t))
    ph = np.cumsum(f) / SR
    src = np.zeros(n)
    for k in range(1, 27):
        src += np.sin(2 * np.pi * k * ph) / k
    opening = np.sin(np.pi * np.clip(u * 1.15, 0, 1))
    cut = 280 + 2300 * opening ** 1.3
    out = np.zeros(n)
    zi = None
    blk = 96
    for s in range(0, n, blk):
        b, a = signal.butter(2, min(float(cut[s]), 9000) / (SR / 2))
        if zi is None:
            zi = signal.lfilter_zi(b, a) * src[s]
        out[s:s + blk], zi = signal.lfilter(b, a, src[s:s + blk], zi=zi)
    out += np.sin(2 * np.pi * ph) * (1 - opening ** 2) * 0.9
    out = np.tanh(1.7 * out / max(1e-6, np.abs(out).max()))
    return out * np.sin(np.pi * np.clip(u, 0, 1)) ** 0.6 * np.minimum(1, t / 0.012)


for line in tl.voice_lines():
    for syl in tl.voice_syllables(line):
        place(muh(syl["dur"], 128.0 * 2 ** (syl["semis"] / 12), syl["glide"]), syl["t"], -0.30, 0.19)

# --- typing (only the line after "type a bit.") --------------------------------------------------
for down, up, name in presses():
    k = KEYMAP.get(name)
    pan = ((k["col"] - 10.5) / 10.5 * 0.35) if k else 0.0
    heavy = 1.6 if name == "SPACE" else (0.8 if name == "LSHIFT" else 1.0)
    gain = 0.30 * RNG.uniform(0.85, 1.1) * (0.5 if name == "LSHIFT" else 1.0)
    place(click(heavy), down, pan, gain)
    place(bandnoise(int(0.02 * SR), 3000, 8000) * np.exp(-np.arange(int(0.02 * SR)) / SR / 0.003), up, pan, 0.10)

# --- calls, threads, landings --------------------------------------------------------------------
TARGET = {"wake_mouse": "mouse", "wake_keyboard": "keyboard", "wake_mic": "mic", "profile": "keyboard",
          "dpi": "mouse", "mute": "mouse", "radial": "mouse", "fire": "keyboard", "fire_off": "keyboard",
          "heat": "keyboard"}
deg = 0
for line in tl.SCRIPT:
    if line["kind"] == "tool":
        place(fm(penta(5 + deg % 4), 0.35), line["t"], 0.0, 0.16)
        deg += 1
    elif line["kind"] == "agent":
        place(fm(penta(3), 0.25, index=0.3, t60=0.2), line["t"], 0.0, 0.07)
    ev = line.get("event")
    if ev:
        p = PAN[TARGET[ev]]
        w = whoosh(tl.THREAD_TRAVEL + 0.1)
        # The whoosh travels with the thread: pan from centre to the device.
        n = len(w)
        pans = np.linspace(0, p, n)
        i0 = int(line["t"] * SR)
        j = min(N, i0 + n)
        a = (pans[: j - i0] + 1) * math.pi / 4
        MIX[0, i0:j] += w[: j - i0] * np.cos(a) * 0.11
        MIX[1, i0:j] += w[: j - i0] * np.sin(a) * 0.11
        note = {"wake_mouse": 7, "wake_keyboard": 8, "wake_mic": 9}.get(ev, 7)
        place(bell(penta(note)), tl.arrival(ev), p, 0.28)

# The bind's second hop, thumb to mic.
place(click(0.7), tl.arrival("mute") + 0.02, PAN["mouse"], 0.35)
w = whoosh(0.7)
i0 = int((tl.arrival("mute") + 0.25) * SR)
pans = np.linspace(PAN["mouse"], PAN["mic"], len(w))
a = (pans + 1) * math.pi / 4
MIX[0, i0:i0 + len(w)] += w * np.cos(a) * 0.11
MIX[1, i0:i0 + len(w)] += w * np.sin(a) * 0.11
place(bell(penta(9), 1.4), tl.arrival("mute") + 0.85, PAN["mic"], 0.28)

# DPI: five stages tick on, three tick off.
da = tl.arrival("dpi")
for i in range(5):
    place(fm(penta(10 + i), 0.12, index=1.2, index_tau=0.02, t60=0.08), da + i * 0.06 + 0.15, PAN["mouse"], 0.13)
for i in (2, 3, 4):
    place(fm(penta(10 + i) / 2, 0.12, index=0.8, index_tau=0.02, t60=0.08), da + 0.70 + i * 0.05, PAN["mouse"], 0.10)

# Radial: each wedge locks with a small mechanical tick.
ra = tl.arrival("radial")
for i in range(tl.RADIAL_SECTORS):
    place(click(0.6), ra - 0.35 + i * 0.055 + 0.42, PAN["mouse"] + (i - 3.5) * 0.04, 0.16)

# Profile: the rig squaring up, a low settle.
place(fm(penta(0), 0.9, ratio=0.5, index=1.0, index_tau=0.2, attack=0.02, t60=0.8), tl.arrival("profile"), 0.0, 0.18)

# --- the board's own state -----------------------------------------------------------------------
L = np.load(os.path.join(tl.OUT, "lighting.npz"))
level = L["kb"].mean(axis=(1, 2))
lvl = np.interp(t_all, (np.arange(len(level))) / tl.FPS, level)
segs = tl.light_segments()
fire_on, fire_off, heat_on = segs[1][0], segs[2][0], segs[3][0]

# Fire: crackle and rumble while it runs.
fire = np.zeros(N)
m = (t_all >= fire_on) & (t_all < fire_off)
roar = signal.lfilter(*signal.butter(2, [80 / (SR / 2), 600 / (SR / 2)], "band"), RNG.standard_normal(N)) * 0.10
fire += roar
pops = (RNG.random(N) < 18 / SR).astype(float) * RNG.uniform(0.2, 1.0, N)
crackle = signal.lfilter(*signal.butter(2, [1500 / (SR / 2), 7000 / (SR / 2)], "band"), pops) * 0.9
fire += crackle
gate = np.clip(np.minimum((t_all - fire_on) / 0.25, (fire_off - t_all) / 0.12), 0, 1) * m
MIX += np.vstack([fire, np.roll(fire, 311)]) * gate * 0.55

# Typing heat: a warm hum that follows the real board's heat.
hm = t_all >= heat_on
hum = (np.sin(2 * np.pi * 82.4 * t_all) + 0.5 * np.sin(2 * np.pi * 123.6 * t_all) + 0.25 * np.sin(2 * np.pi * 164.8 * t_all))
MIX += np.vstack([hum, hum]) * np.clip(lvl * 2.2, 0, 0.6) * hm * 0.09

# --- the ending ----------------------------------------------------------------------------------
ct = tl.COLLAPSE_T
n = int(0.42 * SR)
tt = np.arange(n) / SR
sweep = np.sin(2 * np.pi * np.cumsum(np.geomspace(1400, 70, n)) / SR) * env(n, 0.005, 0.5)
place(sweep, ct, 0.0, 0.16)
n = int((tl.DROP_LAND - tl.DROP_T) * SR)
fall = np.sin(2 * np.pi * np.cumsum(np.geomspace(900, 300, n)) / SR) * np.linspace(0.2, 1, n) ** 2
place(fall, tl.DROP_T, 0.0, 0.05)
place(bell(penta(5), 2.2), tl.DROP_LAND, 0.2, 0.22)
place(fm(penta(-5), 2.5, ratio=0.5, index=1.5, index_tau=0.4, attack=0.01, t60=2.0), tl.DROP_LAND, 0.0, 0.20)


# --- space and master ----------------------------------------------------------------------------
ir_n = int(1.3 * SR)
ir_t = np.arange(ir_n) / SR
irs = [RNG.standard_normal(ir_n) * np.exp(-6.9 * ir_t / 1.3) for _ in range(2)]
for ir in irs:
    ir[: int(0.012 * SR)] = 0
wet = np.vstack([signal.fftconvolve(MIX[c], irs[c])[:N] for c in range(2)])
wet *= 0.18 / np.abs(wet).max() * np.abs(MIX).max()
out = MIX + wet

# --- the score (gen_music.py, Harmonia) ----------------------------------------------------------
MUSIC_GAIN = 0.62
TAPE_STOP_S = 0.45
music_path = os.path.join(tl.OUT, "music.wav")
if os.path.exists(music_path):
    msr, m = wavfile.read(music_path)
    m = m.astype(np.float64)
    if np.abs(m).max() > 2:
        m /= 32767.0
    m = signal.resample_poly(m, SR, msr, axis=0).T
    m = np.pad(m, ((0, 0), (0, max(0, N - m.shape[1]))))[:, :N]
    # The score gives way a little under speech.
    _tt = np.arange(0, tl.DURATION, 0.01)
    _env = np.array([tl.voice_env(x) for x in _tt])
    m *= 1.0 - 0.32 * np.interp(t_all, _tt, _env) ** 0.8
    # "too animated.": the tape slows to a stop, then a breath before the vamp.
    too = next(l for l in tl.SCRIPT if l["text"] == "too animated.")
    stop = tl.commit_time(too)
    i0, i1, i2 = int(stop * SR), int((stop + TAPE_STOP_S) * SR), int((stop + 0.5) * SR)
    u = np.arange(i1 - i0) / (i1 - i0)
    pos = i0 + np.cumsum((1 - u) ** 2)
    for c in range(2):
        m[c, i0:i1] = np.interp(pos, np.arange(N), m[c]) * (1 - u ** 3)
    m[:, i1:i2] = 0.0
    out = out + m * MUSIC_GAIN
else:
    print("no music.wav: run gen_music.py with Harmonia's python first")
fade =np.clip((tl.DURATION - t_all) / 0.6, 0, 1)
out *= fade
# A gentle soft clip gives the quiet sections body without hard peaks.
out *= 1.7 / np.abs(out).max()
out = np.tanh(out) / np.tanh(1.7)
out *= 10 ** (-1.0 / 20) / np.abs(out).max()

rms = 20 * np.log10(np.sqrt((out ** 2).mean()))
peak = 20 * np.log10(np.abs(out).max())
path = os.path.join(tl.OUT, "audio.wav")
wavfile.write(path, SR, (out.T * 32767).astype(np.int16))
print(f"wrote {path}: {tl.DURATION:.1f}s, peak {peak:.1f} dBFS, rms {rms:.1f} dBFS")
