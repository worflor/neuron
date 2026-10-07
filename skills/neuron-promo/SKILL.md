---
name: neuron-promo
description: Direct and produce a Neuron promo or ad video (vertical shorts, outreach, release teasers) with Blender, the real pattern engine, Harmonia music and ffmpeg. Use when asked to make, revise, retime, rescore or extend a Neuron ad, trailer or promo clip, or to start a new one (e.g. the gaming ad) from the dev-ad pipeline.
---

# Making Neuron promo videos

You are the creative and technical director. The owner wants a finished cut, not options: decide,
build, look at it, fix it, then show them. The reference production is
[`artifacts/promo/dev-ad/`](../../artifacts/promo/dev-ad/README.md) ("i'm lazy. set up my
devices."). Start a new ad by copying that folder; read its README for the build order.

## The honesty rules (non-negotiable)

Neuron's brand is honesty about hardware, so the ad cannot fake the product.

- **LEDs only ever show Neuron's own output.** Board and mouse colours come from
  `artifacts/promo/pattern-export`, which links neuron-core and renders presets through
  `pattern::Compositor`, fed the ad's keystrokes via `capture::script_key_reads`. Never paint an
  invented ripple, sweep or flash on the LEDs. Staging (spotlights, light threads, landing rings,
  labels, mechanisms) lives in the scene, never on the keys.
- **Every command and every reply shown is real.** Rehearse each one with the installed CLI in a
  scratch root (`NEURON_RUN_DIR=$env:TEMP\...`, `--no-live`) and copy the reply verbatim. Do not run
  device-writing verbs (`feel stages`, `button apply`, `idle`) for rehearsal; check their syntax with
  `--help` and say so. Rehearsal catches real bugs in the script (`stack set` cannot change a
  layer's pattern; the honest swap is `rm 0` then `add`).
- **Continuity beats convenience.** If a character types, the keys move and any live-input
  effect reacts. When a beat needs the board quiet, change the story (the dev ad's complaint is
  spoken, transcribed beside a waveform), never the physics.
- Use the owner's real rig: Naga V2 Pro, BlackWidow Chroma V2, Seiren V3 Mini (`neuron control list`).
- Say plainly in the handoff what is staged copy, what is a stylised model, and what is engine output.

## Direction that works

- **One timeline owns everything.** `timeline.py` holds the script, beats, shots, attention
  subjects and the BPM grid; terminal, lighting, sound, music and Blender all read it. Retime there.
- **Tell the eye where to look before something happens.** The acting terminal line gets an accent
  bar until its effect lands; threads carry a bright head; a ring blooms where they land; the
  subject device gets its spotlight while the rest dims; the camera arrives first and holds after.
- **Cut to the music.** Put the timeline on a beat grid (dev ad: 120 BPM, 2 s bars) and land
  thread arrivals on beats. Layers enter as the story earns them; a rejection can stop the tape.
- **Comedy is the agent being dry**, never a mascot. Short lines, terminal-first, no yap.
- **No sensory slop.** Every visual or sound answers a question or is caused by something on screen.
- Tab label says `your coding agent`: the agent drives neuron; it is not neuron.

## Tools

| | |
|---|---|
| Blender 5.2 | `D:\tools\blender-5.2.2-windows-x64\blender.exe -b --factory-startup --python build_scene.py -- ...` |
| Pattern engine | `cargo build --release --manifest-path artifacts\promo\pattern-export\Cargo.toml` with `CARGO_TARGET_DIR=D:\build-cache\promo\target` |
| Music | Harmonia at `C:\Users\Micha\Documents\Projects\Harmonia`; run `gen_music.py` with its `.venv\Scripts\python.exe`. Chip voices are `poly_synth` (square/triangle, `noise`, `pitch_env_semitones` for drums) |
| Keyboard model, captures | the ignored harness in `.local/showcase/` (`render_showcase.py`, read-only `capture_controller.py`) |
| Output | `D:\build-cache\promo\<ad>`; never write renders into the repo |

Blender 5.x: the compositor is `scene.compositing_node_group` with a `NodeGroupOutput`; Glare
settings are node inputs. Render with Cycles + OptiX; a 1080x1920 frame is ~2.6 s at 24 samples on
the RTX 3060. Check free space on C: and D: before long renders.

## Verify like a director

- Render 8-15 beat frames at `--scale 0.5 --samples 12` and read a contact sheet before any full
  render. Check the engine data too (mean LED level per beat) for continuity bugs.
- **Never play audio.** Measure it: per-section RMS and peak. The music's loudness should climb
  with the setup; the payoff section is the loudest. Chip voices differ by ~20 dB at equal faders,
  so balance against measurements.
- Send the cut with SendUserFile, say what you could not verify (the sound is unheard), commit
  sources on main, never push or publish.

## Pitfalls already paid for

- Vertical frames: fit width with the **horizontal** FOV (36 mm sensor height -> 20.25 mm width).
- A faded emission object still occludes: key `hide_render` once its colour reaches black.
- A board under a terminal is always in a vertical frame that fits the terminal; hide things with
  story, not framing.
- `PROMO_FPS` 30 with motion blur (shutter 0.25) reads smoother than it sounds; heavy blur smears text.
- Harmonia `delay` takes `delay_ms`, not beats; lint warns on unknown params, read the warnings.
- PowerShell here-strings drop the trailing newline: re-read a file after scripted splices.
