# Dev ad: "i'm lazy. set up my devices."

A 44-second vertical (1080×1920, 30 fps) promo for developers. One continuous Blender shot: a
terminal pane floats behind the rig, an agent drives the `neuron` CLI, and each command sends a
thread of light from its line to the device it configures.

Published on X on 2026-10-07 (the 44 s cut, v5). Draft X copy: "synapse wanted an account. / my coding agent
wanted a sentence. / neuron: a razer synapse replacement. no account, no cloud, source available. agents
optional, but fun." with the links (`www.woflo.dev/neuron`, the GitHub repo) in the first reply.

## What is real and what is staged

- Every command in the terminal, and every reply shown under one, was run against v0.1.4 in a
  scratch run root (`NEURON_RUN_DIR`). The devices are the owner's: Naga V2 Pro, BlackWidow
  Chroma V2, Seiren V3 Mini. `feel stages` writes the mouse, so it was checked against `--help`
  only.
- The agent's dialogue is written copy, not a recorded session.
- **Every LED frame is Neuron's own output.** `gen_lighting.py` runs `../pattern-export`, which
  links neuron-core and renders the `fire` and `typingheat` presets through `pattern::Compositor`,
  switched on the frames the commands land and fed the ad's keystrokes through
  `capture::script_key_reads`. Nothing else paints the LEDs; spotlights, threads, rings and labels
  are staging in the scene.
- The keyboard is the generic BlackWidow fixture from `.local/showcase` (core 6×22 key map, not a
  scan). The mouse and mic are stylised stand-ins.
- Sound is synthesized from the same timeline (`gen_audio.py`); tool-call blips use the soft-pulse
  FM construction from `crates/neuron-core/src/tone.rs`.
- The score is a chiptune composed in Harmonia (`gen_music.py`) on the timeline's 120 BPM grid:
  each layer enters when its cause lands, "too animated." stops the tape, and the reveal is the
  loudest section.
- The user talks to the agent through the Seiren: every line is spoken (words land as text beside a
  waveform, each syllable a synthesized muted "muh", a listening ring on the mic on the same
  schedule). Nothing is typed, no key moves and no key sounds until the agent says "type a bit.", so
  the real typingheat stays idle until then. Nothing presses a key at the end either.
- Commands honour what they really do: `profile apply dev` precedes the lighting edits (a new
  profile is not active), and `macro add` passes its source file. `feel stages` and `profile apply`
  write devices, so their syntax was checked with `--help` and their replies are not shown.

## Provenance

`provenance/lighting-v5.npz` and `provenance/pattern-script-v5.json` are the exact LED frames and the
engine script behind the posted cut (`kb` is 1320x132 RGB, `mouse` 1320x3x3, sRGB 0..1). They came
from the exporter built against neuron-core on 2026-10-06, so re-running `gen_lighting.py` after
later engine changes may not reproduce them. Keep these if anyone asks whether the board is real.

## Build

Requirements: see [`skills/neuron-promo`](../../../skills/neuron-promo/SKILL.md#tools) (Blender 5.2
portable, ffmpeg on PATH, Python 3.11 with numpy/Pillow/scipy, Rust, the Harmonia repo, the installed
neuron CLI). `pattern-export` is shared with `../effects/gen_effects.py`.

All timing, shots, attention cues and the beat grid live in `timeline.py`. Direction rules for
this and future ads: [`skills/neuron-promo`](../../../skills/neuron-promo/SKILL.md). Output goes to
`D:\build-cache\promo\dev-ad` (`PROMO_OUT`).

```powershell
$env:CARGO_TARGET_DIR = "D:\build-cache\promo\target"
cargo build --release --manifest-path ..\pattern-export\Cargo.toml
python gen_terminal.py      # term/#####.png + anchors.json
python gen_lighting.py      # lighting.npz, through the real pattern engine
& C:\Users\Micha\Documents\Projects\Harmonia\.venv\Scripts\python.exe gen_music.py   # music.wav
python gen_audio.py         # audio.wav (sound design + the score)
& D:\tools\blender-5.2.2-windows-x64\blender.exe -b --factory-startup --python build_scene.py -- --resume --out D:\build-cache\promo\dev-ad\render4
python assemble.py          # end card + audio -> neuron-dev-ad.mp4
```

`build_scene.py` takes `--frames 1,90,300`, `--range a:b`, `--scale 0.5` and `--samples N` for test
passes; a full render is about 2.6 s/frame on an RTX 3060 (OptiX, 24 samples).
