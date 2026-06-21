# neuron

an open, lean, do-what-i-say control layer for razer gear. an *anti-synapse*.

one small binary. no account, no cloud, no telemetry, no background tax, no "please update razer central." it talks to your mouse and keyboard directly, does exactly what you tell it, and then shuts up.

it speaks real *razer* — the same `razer_report` HID bytes synapse sends — reverse-engineered from a mix of the wire (USBPcap), the open-source linux driver (openrazer), and a lot of live probing. no kernel driver, no vendor SDK 😳

> **status:** windows-first, single dev, very much a personal project that got out of hand. device reads and the everyday writes are proven on real hardware; the riskier opcodes are honestly gated until they're confirmed (more in [status / honesty](#status--honesty)). it works on my desk every day. it has not been tested on yours.

---

## contents

- [why this exists](#why-this-exists)
- [the one idea: `trigger → action`](#the-one-idea-trigger--action)
- [device control](#device-control)
- [lighting, both chroma eras](#lighting-both-chroma-eras)
- [input: the spine, up close](#input-the-spine-up-close)
- [macros: real code, kept warm](#macros-real-code-kept-warm)
- [beacons: macros can ask](#beacons-macros-can-ask)
- [spellweaving: cast your intent](#spellweaving-cast-your-intent)
- [audio](#audio)
- [migrate off synapse in one command](#migrate-off-synapse-in-one-command)
- [self-emergent discovery](#self-emergent-discovery)
- [the app](#the-app)
- [the arm gate](#the-arm-gate-it-wont-touch-your-keyboard-unless-you-say-so)
- [build & a few CLI starters](#build--a-few-cli-starters)
- [status / honesty](#status--honesty)
- [what it doesn't do](#what-it-doesnt-do)
- [philosophy / non-goals](#philosophy--non-goals)

---

## why this exists

i ran synapse 1. then 2 — hated it, lived with it. then 3 came out and i poked at it, but it's *still synapse*. i figured 3 was the floor. then **4**, and one day the app told me i *had* to update. i said nuh uh. neuron started roughly there. (the other half of the spark: i wanted to use my mouse's onboard storage like a little usb drive. lmfao. genuinely one of the reasons this exists.)

to be clear, i love my razer hardware — this isn't anti-razer, it's anti-*synapse*: a multi-process, account-gated, cloud-synced ~2GB install that re-enables features you turned off, forgets settings, phones home, and bolts a login screen onto your *mouse dpi*. a fine idea drowned in shittification.

neuron is the opposite design, on purpose:

- **one process, lazily windowed.** tray-resident, single-digit-MB idle. the live remap loop runs *inside* it — there's no second daemon.
- **no cloud, no account.** your config is plain TOML on your disk. you can read it, diff it, and check it into git.
- **deterministic.** it changes what you ask and nothing else. no surprise re-enables, no "smart" anything you turned off three separate times.
- **on-device first.** push settings to the mouse's onboard memory and you can uninstall *everything*. the dream is no software at all. (older hardware has no onboard storage, so we make do.)

i'll be honest about the name, though: neuron isn't a faithful, minimal re-implementation of synapse. it's closer to *synapse+++* — it does **more** (gestures, an open effects engine, real-code macros, a whiteboard, a rhythm game i refuse to apologize for). the difference isn't feature count, it's that all of it is built low-level and every decision has a reason i can point at. it's meant for *me*, and replacing slop with slop would defeat the whole point. more, sure. bloat, never.

how it even reaches a protected device without a driver: razer's vendor control collection answers `HidD_Get/SetFeature`, and those IOCTLs are `FILE_ANY_ACCESS`, so neuron opens the device with `dwDesiredAccess = 0`. windows blocks `GENERIC_READ/WRITE` on a mouse; it doesn't block access-zero feature reports. that one trick is the whole foundation — same bytes as synapse, no kernel anything. it's also why neuron never installs a kernel driver, so it can't get swept up in the windows-defender "vulnerable driver" quarantine that bricked OpenRGB, SignalRGB, and FanControl in 2025 (the WinRing0 mess). plain HID feature reports skip the whole signed-driver circus.

## the one idea: `trigger → action`

everything in neuron is one primitive. *something happened* (a `Trigger`) so *do this* (an `Action`). a button press, a global hotkey, a drawn glyph, a radial flick, the foreground app changing, a tap on your mic, a held hypershift layer — all of them are `Trigger`s. a keystroke, a macro, a dpi cycle, a profile switch, a python script, "teleport to the other monitor" — all of them are `Action`s.

there is **one dispatcher**. bindings, gestures, the radial menu, app-aware switching, hypershift layers, and macros aren't six subsystems that happen to look similar; they fold into a single flat list of `trigger → action` rules and run through the same resolver. a resolved glyph dispatches through the exact same path as a hardware button, so it composes with layers and safe-mode identically. there is no second code path for "a gesture" vs "a hotkey."

this is the whole reason the feature list below is as long as it is without becoming a swamp: adding a feature is usually adding a new `Trigger` or a new `Action`, not a new pipeline.

and because nothing is hardcoded: **press-to-bind is everywhere.** you hold the button you want and it captures it. there is, somewhere in the code, a comment that just says *"DONT HARD CODE IT TO FUCKING THUMB 2"*. that's the design rule.

## device control

the bread and butter. DPI (single value and the full DPI-stage cycle), polling rate, lighting brightness, idle/sleep timer, scroll-wheel stage, onboard-storage accounting, battery and charging state. reads decode the device's own bytes and match synapse byte-for-byte — DPI comes back as a big-endian X/Y pair, the onboard pool reports the same "% remaining" math synapse shows (`free = available + recycle`).

writes are the part people get burned by, so they go through a deliberate flow:

1. **backup first.** before any "safe" write, neuron can snapshot the device's entire getter space (every class × id) to `backups/*.json`. it's the known-good reference for a restore or a diff.
2. **driver mode.** host control only renders in device-mode `0x03` — razer gates it, synapse flips it, so does `neuron mode driver`. it's idempotent and reverts when you reopen synapse or power-cycle.
3. **volatile first.** writes default to `NOSTORE` — they take effect now but aren't flashed to onboard memory unless you ask (`--persist`). nothing permanent happens until a clean round-trip proves the opcode works.
4. **read-back verify.** after the write, neuron re-reads the matching getter and confirms the bytes it sent actually landed. a mismatch is a hard error — `VERIFY FAILED, write NOT trusted` — not a silent success.

polling covers both the legacy divisor path (1000/500/250/125 Hz) and the hi-res "HyperPolling" path up to 8000 Hz, where the device exposes it. DPI stages write the whole table at once (`0x04/0x06`, openrazer-confirmed and verified live on a Naga V2 Pro) with a chosen active stage. sniper / on-the-fly DPI is a hold-to-drop-to-precision binding — press the button you want on first run, nothing assumed.

an asleep wireless mouse makes a getter time out, and neuron surfaces that as an honest error rather than inventing a number. it would rather tell you it doesn't know than guess at your hardware.

## lighting, both chroma eras

razer's lighting hardware speaks two dialects, and neuron confirmed both live: the **legacy** keyboard class `0x03` (effect-first, key-grid) and the **matrix** mouse/modern class `0x0F` (per-LED). one model covers both. semantics live in code; every opcode, matrix dimension, and effect-id lives in device TOML, so a new device is a new file, not a recompile.

three things happen under that one model:

- **native firmware effects** (off / static / breathing / spectrum / wave / reactive / starlight, where the firmware has them) get invoked by their real effect-id byte and run on-device — they survive synapse being uninstalled, and on matrix devices they persist to onboard memory.
- **per-key custom frames** are painted at the device's *true* LED count, one report per matrix row, never downsampled. you can paint directly on the device in the GUI.
- **an open effects engine** computes frames host-side for anything the firmware lacks.

that last one is the fun part. an effect is a `FrameGen` — a trait with exactly one method:

```rust
fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb>;
```

it gets the matrix size, a time, and a base colour, and returns one frame. "add an effect" means: write a struct that implements that, add one arm to the registry. that's it. the built-ins include a real upward **fire** simulation (heat seeded hot at the bottom, diffused and cooled per tick, run through a black→red→orange→white ramp — the kind of effect synapse software-locks per device), **starlight**, a keyboard-**reactive** mode (it reads key state and hashes each pressed key to a stable cell, since the device exposes no key→LED map), and an **audio meter** that drives the board off your output device's live peak sample. there's a layer **compositor** on top — a stack of generators with regions and blend modes — and the compositor is itself a `FrameGen`, so the streaming path drives it with zero special-casing.

fire and starlight even ship their own tiny xorshift PRNG rather than pull in a `rand` dependency. lean is a feature down to the crate graph.

## input: the spine, up close

the [`trigger → action`](#the-one-idea-trigger--action) idea, fleshed out into the things you'll actually bind:

**bindings.** a control event → an action. stored as plain rules you can hand-edit. the shipped defaults are intentionally empty — the BlackShark knob and a mute toggle are hardware-internal and emit nothing to the host, so there's no honest universal default to ship. you bind what your hardware actually sends.

**hypershift layers.** razer ships hypershift as hold-only. neuron makes the layer first-class and gives it four *stances*: **hold** (active while held), **latch** (a press toggles it), **smart** (holds immediately, then on release decides tap-vs-hold for you), and **one-shot** (arms for exactly the next trigger). releasing one input only drops *its* layer, so two held layers don't stomp each other. this is the fix razer never shipped.

**app-aware switching.** a tiny read-only query of the foreground exe (no hooks); a rule like `valorant → game profile` fires on `valorant.exe` by substring. it's just an `AppFocus` trigger feeding a `ProfileSwitch` action — same spine.

**the radial menu** is the simple floor of the spellweaving continuum (below): hold, flick a direction, release. the number of reliable wedges is *computed* from your hand's jitter, not capped by hand — the default 8-way wheel can go to ~16. and it resolves by where your hand *meant* to end (recency-weighted over the whole stroke), so "left… no, right" lands on right.

a small thing that matters: while you're capturing a button to bind it, the dispatch loop fires nothing for that button — you don't accidentally trigger its current binding mid-rebind. and the live listener is wrapped so that ESC (the weave-cancel key) and stray panics can never silently kill it. casts must never quietly die.

## macros: real code, kept warm

a macro is just an `Action` that happens to be a sequence — or real python.

the python tier is the power tier. a macro is a file with `def macro(ctx):`, run by a private bundled CPython kept **warm as a sidecar process** (the *Macro Host*). each macro is `exec`'d once, its imports warmed, and a trigger is a tiny framed message to the already-resident function — no per-press spawn, no per-press import. the dispatch is a pipe write and a call into a live interpreter, well under a frame; the native key→key remap path never touches python at all and runs at effectively zero cost.

it's **full unsandboxed power, on purpose.** `ctypes` into raw win32, `subprocess`, sockets, files — anything a program can do. the convenience layer (`neuron.key` / `type_text` / `click` / `clipboard` / `run`) honours the SAFE arm gate and no-ops to a `[disarmed]` marker when input is disarmed; reaching past it into raw `ctypes` does *not* consult the gate. that's your own rope, by design.

it's **crash-isolated.** a macro doing raw `ctypes` is one bad pointer from a segfault, and in-process that would take down the app holding your hardware. so it runs in the sidecar: a segfaulting macro kills only the sidecar, which is respawned and re-registered in the background while the main app never hitches. a circuit breaker stops a macro that crashes on every press from pinning a core respawning forever.

each macro gets the **captured context** — foreground app, window title, working directory, clipboard, and the window you came from (so you can focus discord, type, and alt-tab back). the snapshot is taken once at trigger time and threaded through the whole macro, so it reasons about one consistent world. macros run on **per-macro serial queues**: spam one macro and its fires stay in strict order; a slow macro (or one waiting on a network call) stalls nobody else, because a *different* macro runs on its own thread.

> this replaced an earlier engine that compiled user *Rust* to a cdylib at trigger time via `rustc` and hot-loaded it. it was genuinely sub-microsecond, and also a heavy toolchain dependency with compile-at-press jank. the Macro Host keeps the warmth without any of it. (there are some stale `.dll` files in the tree from that era. ignore them.)

## beacons: macros can ask

a macro can pause mid-flow and ask the human. `neuron.ask("deploy it?")` blocks *that macro's worker thread only* — an llm call, a deploy gate, an agentic-loop checkpoint, or a plain yes/no — and surfaces signal-first: a quiet one-line strip names the question at the top of the cursor's monitor. the wheel never forces itself open. when you hold your cast trigger, an answer wheel materialises at the cursor: flick toward green (east) = yes, red (west) = no, vertical = pass (the macro gets its default). nothing modal, no focus steal, no dialog box; only a deliberate committed flick resolves it, and bailing costs nothing.

answering a beacon synthesises no input, so `ask`/`notify` work even in SAFE mode — a macro can talk to you while every real keystroke is disarmed. and on the CLI, beacons are answered right in the terminal (`[beacon] asks: deploy it?  [y/n]`), so the same prime→ask→activate macro works headless.

## spellweaving: cast your intent

hold a trigger, weave a stroke, release; it fires an action — **live, as a real keybind**. this is one engine across a continuum, not a separate mode. at the simple end it's a **radial** flick (direction only, bucketed into N sectors). at the rich end it's a full **glyph** (any drawn shape). radial is literally the degenerate case of the same system — a one-segment glyph you only read directionally — and they share the capture path, the resolver, and the dispatch.

while you weave, the real cursor is pinned (clipped to a 1px box, hidden) so drawing the shape doesn't drag windows or fire stray clicks; physical motion is still read via Raw Input. a glowing **sigil overlay** follows the stroke — a comet trail with a white-hot head and a procedural rune-ring, ~60fps in a few MB, clamped to your monitor — and it ghosts the predicted shape near the head, solidifying as confidence climbs.

glyphs are recognised by **eigenmotion**, which is the genuinely novel bit. a stroke is the complex sequence `z[n] = x + iy`. each segment is fit by a damped complex oscillator, `z[n] = K·z[n-1] − G·z[n-2]`, and the **eigenvalues of that recurrence are the stroke's identity** — its natural frequency and decay, not its pixels. magnitude is damping (an open arc vs a sustained loop), and the *signed* rotation is handedness, so clockwise ≠ counter-clockwise falls out for free. the fit is on velocity and arc-length-resampled, so it's invariant to where you drew it, how big, and how fast. global invariants (winding number, total bending, closure) separate shapes the local fit would conflate, like a one-loop circle from a two-loop one. there's aim-assist — a near-miss snaps to the closest spell *only* when it's an unambiguous winner. predict intent, snap only when sure, never fight a deliberate miss.

the recognition core is its own crate, `engram` — a general trajectory codec (the same oscillator math also does lossy compression with a real cascaded encoder), and it's reused beyond gestures: it's the engine behind the rhythm familiar below.

because a resolved weave is just another `Trigger`, it dispatches through the same engine as a hardware button — which means it can drive a whole family of **new-input instruments** built on the same capture:

- **teleport** — tap-hold pops a live minimap of your real monitor layout with your open windows as blobs; drag a ghost and release to warp the cursor (and focus a window if you land on it). left-click *summons* a window to your hand; right-click *grabs* one and drops it on another monitor or virtual desktop. dwell on a window and a live DWM-thumbnail portal blooms without stealing focus.
- **tether** — the "warpstone": drop a spot, warp back to it later at the same relative pixel.
- **whiteboard** — a virtual-screen, click-through annotation canvas, shape-snap (a sloppy stroke becomes a clean line/arrow/ellipse/rect), brushes, and a laser presentation mode.
- **glance** — cast a target and every matching window blooms as a live thumbnail tile in a magnetic collage; a right-drag crops a tile, a double-click steps you through the portal.
- **window verbs** — summon / banish (with submodes like, hovered window, and any window behind others) / kill / pin.
- **dial** — the next hold becomes an analog slide where speed *is* sensitivity, mapped to output or mic volume.
- **knockback** — a rhythm familiar. you drum on the cast trigger while idle or in queue, and a spectral twin knocks your rhythm back with a small flourish for you to finish. theres no difficulty. you play, and the twin adapts to your rhythm.

the actual playground here is new input, and it enables new features. that's the point of building the engine instead of a settings panel.

## audio

mic and output (headphones, sound card, in-line dongles) gain and mute, output flip between devices, and momentary/push-to-talk. all of it via the windows Core Audio `IAudioEndpointVolume` API — which is the *exact* path synapse's own audio wrapper (`RSy3_WinAudio.dll`) sits on top of, so this needs **zero vendor reverse-engineering.** same OS API, no HID guessing.

endpoints are enumerated generically — no hardcoded device ids — so any trigger can drive any mic or any output, picked by a name substring. a mic tap on a Seiren reflects into Core Audio's mute state, so neuron polls that and turns it into a `MicTap` trigger you can bind. momentary mic is adaptive by default (held = the opposite of however your mic rests: push-to-talk if you keep it muted, push-to-mute if you keep it live), and a held PTT can never strand the mic flipped — it's restored on release, on config reload, and on teardown. output flip cycles your default device across all three roles, exactly like flipping it in sound settings, and remembers devices by name so an unplugged one is just skipped.

## migrate off synapse in one command

synapse's export files (`.synapse3` / `.ChromaEffects`) are zips of plaintext XML with a fake extension. no crypto. (synapse's *cloud cache* is AES-encrypted, and that lock-in we refuse to crack — but the in-app Export is wide open.)

```
neuron import-export your-profile.synapse3 --apply
```

neuron unzips it, routes by content rather than extension, and ingests each capability by its **feature-GUID** — which is stable across devices and synapse versions, so import is version-agnostic for free, and an unknown feature is logged and skipped, never an error. it drops the **default-fill noise**: synapse exports a ~100-key mapping file where most keys are bound to their own default scancode, and neuron diffs every binding against the standard HID-usage table and throws away the identity binds, so a keyboard import yields a handful of *real* rules instead of a hundred. DPI, stages, polling, in-game polling, brightness, idle-off, gaming-mode, bindings (base *and* hypershift, kept separate), and lighting all come across; animated lighting layers are re-created as live compositor layers, and a static frame is captured per-LED losslessly. without `--apply` it's a dry preview that tells you exactly what would import and what it dropped.

the output is a clean neuron profile (plain TOML in `profiles/`) plus a rules sidecar. a profile is a named bundle where every field is optional — it only touches what it sets — and applying one is idempotent: it only sets what the profile sets, and a wrong opcode surfaces as a `skipped` note, never a fabricated success.

## self-emergent discovery

```
neuron discover
```

probes any `razer_report` device's command space (it identifies the pipe by vendor id + a 91-byte feature report, on whatever interface it lives) and classifies each response by its information *shape* — enum, level, xy-pair, table, string. then it labels each command generic-vs-device-specific by a differential: a class that shows up on more than one device is the shared protocol; a class on exactly one device is that device's distinguishing capability. emerged, not hardcoded.

it has fingerprinted a keyboard it had no registry entry for, and `discover --emit` writes a TOML skeleton per unknown device straight into `devices/`. adding a device is dropping a TOML file, not writing code.

## the app

there's a GUI, and it's built to feel like a precision instrument, not a skinned dashboard. true-void black background (the faintest cool tint so it doesn't read as a dead LCD), monospace data front-and-center (you read DPI and hex all day, so the numbers *are* the design), one restrained phosphor accent (`#4af2b0`, an oscilloscope-trace green) used *only* as a live/active/connected signal and never as decoration, and motion that settles instead of bouncing — nothing eases *at* you. teenage-engineering-meets-oscilloscope, not winamp-skin. the design doc's own words: *"the drama left; the void stayed. the machine IS the design."*

it's **tray-resident with an eagerly-built hidden window** — the event loop can run with no window shown, but the Slint window/runtime are built once at startup and hidden on close so the live `trigger → action` loop always has a stable UI/status handle. no separate daemon. the tray is the 90% surface; global hotkeys drive the quick toggles. it even asks windows to relaunch it after a crash, and writes a flight-recorder narrative into the crash log so a crash is a story instead of a mystery.

four sections, because that's what a user actually needs:

- **device** — the feel surface: dpi (with the stage table as detent ticks on the fader), polling as discrete contacts not a fake-continuous slider, brightness, sniper, battery, and a clearly-marked *gated* shelf for the writes that aren't hardware-confirmed yet.
- **lighting** — an auto-generated render of *your* peripheral, built procedurally from what the registry actually knows (rows × cols + device kind) and nothing it doesn't. lit cells glow, unlit ones vanish into the void; you paint per-key directly on it, and effects run on it with the same frame math the hardware runs. a mouse with a few LEDs falls back to approximate zones, and says so.
- **input** — one spine, two depths: direct binds ⇄ spellweaving, sharing the same action palette. a fire-lamp lights on each rule when you press its trigger, so you can *see* the contact close.
- **system** — gates, migration, appearance (the accent re-tints live), and the diagnostics bench. that bench *is* the test harness: it fires nine real probes — enumerate HID, load the registry, round-trip a device, build a lighting frame, resolve the effect engine, assemble the spine, check the macro runtime, load the gesture vault, resolve a mic endpoint — and shows you each one pass, fail, or honestly skip. it's read-only and always safe to run. it's your "prove it works to me" surface, and it's the same thing CI runs.

profiles aren't a place you go; they're a sheet that opens from the header wherever you are, saving and restoring the other sections as one bundle (read back from the live device, not from the sliders).

## the arm gate (it won't touch your keyboard unless you say so)

this is a tool whose job is injecting input, so the safety has to be real. every synthesised keystroke, click, and process-spawn — and every macro helper call — goes through a process-wide **arm gate that is disarmed by default.** tests, the verify pass, anything that isn't the live app: they fire nothing. mechanically it's a single atomic boolean that starts safe; only the daemon and the GUI ever flip it on, at startup. there's even a test that asserts the test suite can never arm input.

the live app mirrors the gate into the Macro Host with an explicit control frame so helper input respects the same arm state. `neuron run --safe` is fully read-only: input is disarmed and device writes are paused. `neuron-app --safe` starts the resident app with input disarmed; the GUI's write-pause switch remains the independent device-write kill-switch. arming requires a deliberate confirm; disarming is instant.

reads are always safe and never gated. the tool that injects input for a living still can't surprise you.

and yes — let's say the quiet part. a thing that remaps your buttons and runs python on a keypress is, definitionally, an input hook. that's *exactly* why it's open source: read it, build it yourself, throw the binary at virustotal. the arm gate, the disarmed-by-default tests, the backup-first read-back-verified writes — those aren't just safety, they're the receipts. if a small unsigned binary trips defender's smartscreen heuristic, that's the false positive tax on indie exes, not a confession.

## build & a few CLI starters

```
cargo build --release      # -> target/release/neuron.exe  (CLI)  +  neuron-app.exe  (GUI)
cargo test  --workspace    # the suite — runs disarmed, never injects
cargo clippy --workspace
```

the default release binary is tuned for runtime responsiveness with ThinLTO and stripping. for distribution-size builds use `cargo build --profile release-size`; for local performance builds use `cargo build --profile release-fast` and opt into `RUSTFLAGS="-C target-cpu=native"` outside the repo if you want CPU-specific codegen. panic is `unwind`, not `abort` — deliberately — so Drop guards run on a panic and the app never leaves your desk in a state you didn't ask for. every panic still gets logged.

```
neuron list                         recognized devices
neuron discover                     fingerprint any razer_report device
neuron dpi 1600                     set DPI (read-back verified)
neuron dpi-stages 800 1600 3200     set the DPI cycle (the whole table)
neuron lighting effect fire         dry-run the exact bytes per device
neuron backup                       snapshot every device's full state
neuron import-export prof.synapse3 --apply    eat a synapse export
neuron run                          the remap daemon (esc to stop; --safe = observe only)
neuron macro add lift my.py         register a python macro into the warm sidecar
neuron macro run lift               fire it now (beacons answered right in the terminal)
neuron macro prelude                the `neuron` module reference (ctx + helpers + ask/notify)
```

## status / honesty

it's **windows-first** because that's what i'm on. the architecture is meant to be cross-platform — HID transport, audio, raw input, overlays, and foreground detection are being kept behind platform seams — but today the working implementations are Windows. non-Windows paths return honest unsupported/no-op results depending on the subsystem; the Linux/macOS HID and input backends still need to be written.

and i'm not pretending this is the most mature or the broadest thing in the space. if you're on linux, [openrazer](https://github.com/openrazer/openrazer) is the real, decade-hardened answer — kernel driver, a couple hundred devices, an actual community — and you should just use it; neuron isn't out to out-cover it. if you want one panel for every RGB brand under the sun, that's [OpenRGB](https://openrgb.org). neuron is deliberately narrow: one vendor, one desk, gone deep. that focus is the point, not a gap i'm apologizing for.

device **reads** and the simple **writes** (DPI, polling, brightness, lighting) are proven live. the **DPI-stage** and **scroll-stage** writes were confirmed off the wire from synapse (USBPcap) and round-tripped on real hardware. a few opcodes I haven't fully proven yet are **honestly gated** — they exist, their payload builders are unit-tested, but the actual device write is locked behind an env flag (`NEURON_*_WRITE`) until a capture confirms them, and even then they read-back-verify. that's the idle/sleep-timer write, the wired/dongle in-game polling split, and the HyperScroll stage table. and where there's no known opcode at all — lift-off distance, debounce, onboard button-remap — neuron **bails honestly** with a "needs RE" note rather than blind-writing an unknown register on your device. it reports what it can't do; it doesn't fake it.

a representative comment from the write path, because it's the whole ethos in one place:

> we do NOT fabricate an opcode — that would be a blind write to an unknown register on the user's device, exactly what the safety gates forbid.

## what it doesn't do

so you know before you install:

- **not cross-platform yet.** windows only, today. the seams are there; the linux/mac transports aren't written.
- **doesn't crack synapse's encrypted cloud profiles.** the AES'd account cache is the lock-in, and we don't touch it. the plaintext in-app *export* is what migration reads.
- **doesn't fake hardware it can't prove.** the gated/stubbed writes above stay gated until a capture confirms them. you'll see `[gated]` or `PENDING`, not a fake success.
- **doesn't sandbox your macros.** full unsandboxed CPython is the point. a macro can do anything a program can. the arm gate guards the *convenience* helpers, not the raw APIs you reach past them into.
- **scroll-wheel feel curves and onboard *profile-slot* persistence aren't there.** stage writes default to volatile and `--persist` flashes the stage table, but full onboard profile slots need the storage-chunk protocol, which isn't done.
- **the whiteboard can't save its ink to disk yet.** you can draw, command, laser, and ping; the `.gwyph` vector-ink format is specced but the codec port hasn't landed.
- **no telemetry, no account, no cloud, no per-app reactive RGB nobody asked for.** not "off by default." just absent.

## philosophy / non-goals

- no cloud. no account. no telemetry. ever.
- do what's asked, change nothing else.
- the device's own numbers are the truth — no weird filters or translations.
- novelty done well (spellweaving, radial, the open effects engine) is the fun. reheated gimmicks are not, and won't be added.
- lean is a feature. every background cost has to earn itself.
- more than synapse is fine; bloat is not. every extra thing here is deliberate and low-level, or it doesn't ship.
- built for one desk — mine. if it fits yours too, genuinely lovely. that was never the requirement.
- when neuron doesn't know something about your hardware, it says so instead of guessing.

built from first principles because the alternative is a 2GB login screen for a mouse.
