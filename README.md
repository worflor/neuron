# neuron

a lean, mean, do-what-i-say control layer for razer gear. an *anti-synapse*.

one small binary. no account, no cloud, no telemetry. no "please update razer central." it talks to your mouse and keyboard directly: the same `razer_report` HID bytes synapse sends, worked out from wire captures (USBPcap), the open-source [openrazer](https://github.com/openrazer/openrazer) driver, and a lot of live probing. it does exactly what you tell it then fricks off. no kernel driver, no vendor SDK 😳

| | |
|---|---|
| **what** | one tray-resident binary (CLI + GUI) built to replace razer synapse |
| **platform** | windows today (linux/mac kept behind seams, not yet implemented) |
| **hardware** | razer mice + keyboards over raw HID; daily-driven on a Naga V2 Pro + BlackWidow Chroma V2, any other `razer_report` device |
| **install** | build from source: `cargo build --release` |
| **footprint** | no driver, no account, no runtime, no cloud; your config is plain TOML |
| **license** | Whisper Protocol License (source-available): free for individuals, nonprofits, education, open-source projects, and bootstrapped shops; commercial license for funded companies |

> **status: public beta mk1.** windows-first, single dev, very much a personal project that got out of hand. mk1 is the same release language my other tools use, and it means exactly this: until now the only eyes and hands on this thing were mine. it works on my desk every day but *obviously* hasn't been tested on yours, and that gap is the whole definition. once it has survived desks that aren't mine, it graduates to mk2. where each feature actually stands, solid to barely-started, is tracked in [state of the project](docs/STATUS.md).

---

## contents

**the pitch**
- [why this exists](#why-this-exists)
- [the idea: `trigger → action`](#the-idea-trigger--action)

**what it does**
- [talks to your gear](#talks-to-your-gear) (device · lighting · audio)
- [what you bind](#what-you-bind) (the spine · spellweaving · macros & beacons)

**how it treats you**
- [won't surprise you](#wont-surprise-you) (the arm gate · safe writes · confirmations)
- [life after synapse](#life-after-synapse) (import · purge · discover)

**the rest**
- [the app](#the-app)
- [get it](#get-it)
- [contributing](#contributing)
- [honesty: proven, gated, absent](#honesty-proven-gated-absent)
- [philosophy / non-goals](#philosophy--non-goals)

---

## why this exists

i ran synapse 1. then 2... and hated it but survived with it. then 3 came out and i reluctantly downloaded it, but it's *still synapse*. i figured 3 was the floor. then **4**??? and one day the app told me i *had* to update!? i said nuh uh. neuron started roughly there. (the other half of the spark: i wanted to use my mouse's onboard storage like a little usb drive. and cast spells.)

to be clear, i love my razer hardware. this is anti-*synapse*: a multi-process, account-gated, cloud-synced ~2GB install that re-enables features you turned off, forgets settings, phones home, and bolts a login screen onto your *mouse dpi*. a fine idea drowned in shittification.

neuron is the opposite design, on purpose:

- **one process, lazily windowed.** tray-resident, single-digit-MB idle. the live remap loop runs *inside* it. there's no second, third, fourth daemon.
- **no cloud, no account.** your config is plain TOML on your disk. you can read it, diff it, and check it into git if you want :P
- **deterministic.** it changes what you ask and nothing else. no surprise re-enables, no "smart" anything you turned off three separate times. plus you can add custom macros using raw python to LITERALLY do whatever you want. true freedom (be safe)
- **on-device first.** push settings to the mouse's onboard memory and you can uninstall *everything*. the dream is no software at all. (older hardware has no onboard storage, so we make do.)

i'll be honest about the name, though: neuron isn't a faithful, minimal re-implementation of synapse. it's closer to *synapse+++*: it does **more** (gestures, an open effects engine, real-code macros, a whiteboard tool built in, a suite of hand-built actions). all of it is built low-level, and every decision has a reason i can point at. it's meant for *me*, and replacing slop with slop would defeat the whole point. more, sure. bloat, never.

## the idea: `trigger → action`

everything in neuron is **one primitive**. *something happened* (a `Trigger`) so *do this* (an `Action`). that's the magic. a button press, a drawn glyph, a radial flick, the foreground app changing, a tap on your mic, a held hypershift layer are all `Trigger`s; a keystroke, a macro, a dpi cycle, a profile switch, a python script, "teleport to the other monitor" are all `Action`s.

**the triggers**, *something happened:*

| trigger | fires when |
|---|---|
| `Input` | a hardware button or key goes down |
| `Gesture` | a drawn glyph is recognised |
| `RadialSector` | a held flick lands in a wheel sector |
| `AppFocus` | the foreground app matches (e.g. `valorant.exe`) |
| `MicTap` | the mic's mute is toggled at the device |
| `Hold` | a hypershift layer is held |
| `Cast` | the spellweave activation rhythm fires |

**the actions**, *do this:*

| group | what's in it |
|---|---|
| **input** | key / chord, mouse button, media key, autofire (turbo), ghost-paste, echo-last |
| **device** | dpi set + stage-cycle, scroll stage, polling, brightness, profile switch + cycle |
| **audio** | mic & output mute/gain, output flip |
| **macros** | run a sequence, run python, invoke another macro |
| **instruments** | teleport, tether, whiteboard, glance, window verbs, dial, knockback, control |
| **system** | lock, sleep, curtain, pocket |

**press-to-bind anything and everything**: don't like my setup? make it yours

## talks to your gear

first, the part that makes any of it possible: how neuron reaches a protected device without a driver. razer's vendor control collection answers `HidD_Get/SetFeature`, and those IOCTLs are `FILE_ANY_ACCESS`, so neuron opens the device with `dwDesiredAccess = 0`. windows blocks `GENERIC_READ/WRITE` on a mouse; it doesn't block access-zero feature reports. that one trick is the whole foundation: same bytes as synapse, no kernel anything. it's also why neuron can't get swept into the windows-defender "vulnerable driver" quarantine that bricked OpenRGB, SignalRGB, and FanControl in 2025 (the WinRing0 mess): plain HID feature reports skip the signed-driver circus entirely.

### device control

the bread and butter: DPI (a single value or the full stage cycle), polling rate, brightness, idle/sleep timer, scroll stage, lift-off distance, onboard-storage accounting, battery and charge state. reads decode the device's own bytes and match synapse byte-for-byte: DPI comes back as a big-endian X/Y pair, the onboard pool reports the same "% remaining" math synapse shows (`free = available + recycle`).

some of it the device just *volunteers*. press the onboard DPI button, toggle the scroll stage, or snap a magnetic side-plate onto a Naga and the mouse pushes its own HID report saying so; neuron hears it on a separate read channel and turns it into a live readout, the same instant-OSD path synapse listens on without polling.

polling covers both the legacy divisor path (1000/500/250/125 Hz) and the hi-res "HyperPolling" path up to 8000 Hz where the device exposes it. DPI stages write the whole table at once (`0x04/0x06`, openrazer-confirmed, verified live on a Naga V2 Pro) with a chosen active stage. sniper / on-the-fly DPI (a hold-to-drop-to-precision binding razer never put on MY mouse) is here too.

### lighting

razer's lighting hardware speaks two dialects, and neuron confirmed both live: the **legacy** keyboard class `0x03` (effect-first, key-grid) and the **matrix** modern class `0x0F` (per-LED). neuron's model covers both. semantics in code, every opcode / matrix dimension / effect-id in device TOML, so a new device is a new file, not a recompile. three things run on that model:

- **native firmware effects** (off / static / breathing / spectrum / wave / reactive / starlight, where the firmware has them) invoked by their real effect-id byte and run on-device. they survive synapse being uninstalled, and on matrix devices persist to onboard memory.
- **per-key custom frames** painted at the device's *true* LED count, one report per matrix row, never downsampled. you can paint directly on the device in the GUI.
- **an open effects engine** that computes frames host-side for anything the firmware lacks.

that last one is the fun part, and it just got rebuilt from the ground up. a custom effect is two halves that compose, a **pattern** and a **spectrum**:

```rust
trait Pattern { fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field; }
```

the **pattern** is the shape and motion (a heat sim, a scroll, a keypress ripple, an aurora flow field), and for each cell it emits just a position and a brightness (or, for the screen ambilight, colour directly). the **spectrum** is the colour, and it's a whole *program*: a gradient of N stops, with its own motion, played across a keyframe timeline. the pattern says *where* light lands and how bright; the spectrum says *what colour* at that spot and time; compose them (`cell = spectrum.at(t, u) × intensity`) and a small set of shapes × colour programs covers every look.

a preset is just a pattern plus a spectrum, pure data, no code (adding your own pattern or preset is covered in [contributing](#contributing)). and a spectrum is only as complicated as you make it: one stop is a solid colour, two is a gradient, add motion (drift / cycle / breathe / flow) and it animates, add keyframes and it sequences over time. it serialises down to the tightest shape that still describes it (a bare hex for a solid, an array for a gradient, a table only when you ask for more). the built-in presets:

| preset | pattern × spectrum |
|---|---|
| `fire` | a real upward heat sim, run through a *recolourable* ember→white gradient; paint it blue and it's cold fire |
| `typing heat` | every keypress deposits radial heat that cools the way heat actually does (radiatively, lingering) over an incandescent ramp |
| `aurora` | a multi-octave flow field under an animated, settable aurora palette |
| `wave` / `cycle` | a rolling, or board-wide, hue: a two-colour gradient or the full spectrum, your call |
| `cascade` / `comet` | rain and shooting streaks with real head→tail gradients (type the key a comet's head sits on to *break* it) |
| `starlight` / `reactive` / `ripple` | stars twinkling and fading, the key you pressed lighting up, rings spreading from each strike |
| `audio meter` / `pulse` | your live output *or mic* peak, or live CPU and RAM, painted low→high |
| `ambient` | the whole board as an ambilight, mirroring your screen |

and since a frame is just data, a data readout is just another layer. your device's live vitals (battery and charge) render as a `vitals` layer in the same stack as any effect, so you drop it wherever you want on the board, at whatever size, and stack it over a running effect with the effect showing through around it. edits stream to the board as you make them, there's no apply button. the CLI keeps the cross-device version too: **`lighting mirror`** paints one device's state onto another's LEDs, your mouse's battery gauge across the keyboard's number row. synapse silos every device and openrazer has no cross-device layer :P

### audio

turn the volume up or down and mute any mic or output (headphones, speakers, a USB dongle, whatever) and switch your default output from one device to another, all from a binding. you pick the device by name, so it works on anything without hard-coding.

there's nothing to reverse-engineer here: windows already lets you control every mic and speaker, and neuron just uses that (the same controls the volume mixer does). one nice side effect: your mic's mute *is* a trigger (`MicTap`), so toggling it can fire an action. and "output flip" swaps your default speakers exactly like you would in sound settings, skipping anything that's unplugged.

## what you bind

the trigger side, up close: everything you can make *fire* an action, from a plain remap to a drawn glyph to real python.

### the spine

**bindings** are the floor: a control event → an action, stored as plain rules you can hand-edit. the shipped defaults are intentionally empty. some razer controls (the BlackShark's knob, for one) are handled inside the hardware and never reach the host, so any universal default would be a lie. you bind what your hardware actually sends.

**hypershift layers.** razer ships hypershift as hold-only; neuron makes the layer first-class and gives it four *stances*:

| stance | behaviour |
|---|---|
| **hold** | active only while held |
| **latch** | a press toggles it on, another toggles it off |
| **smart** | holds immediately, then on release decides tap-vs-hold for you |
| **one-shot** | arms for exactly the next trigger, then drops |

releasing one input drops only *its* layer, so two held layers don't stomp each other. this is the fix razer never shipped.

**app-aware switching** is a tiny read-only query of the foreground exe (no hooks): a rule like `valorant → game profile` fires on `valorant.exe` by substring. it's just an `AppFocus` trigger feeding a `ProfileSwitch` action. same spine.

**the radial menu** is the simple end of spellweaving: hold, flick a direction, release, with a custom hand-motion engine tracking your hand in the air as you cast.

### spellweaving

hold a trigger, weave a stroke, release; it fires an action: **live, as a real keybind**. one engine across a continuum. at the simple end it's a **radial** flick (direction only, bucketed into N sectors); at the rich end a full **glyph** (any drawn shape).

while you weave, the real cursor is pinned (clipped to a 1px box, hidden) so drawing the shape doesn't drag windows or fire stray clicks.

glyphs are recognised by **eigenmotion**. a stroke is the complex sequence `z[n] = x + iy`; each segment is fit by a damped complex oscillator, `z[n] = K·z[n-1] − G·z[n-2]`, and **the eigenvalues of that recurrence are the stroke's identity**: its natural frequency and decay, not its pixels. magnitude is damping (an open arc vs a sustained loop), and the *signed* rotation is handedness, so clockwise ≠ counter-clockwise falls out for free. the fit is on velocity and arc-length-resampled, so it's invariant to where you drew it, how big, and how fast. do wizard shit.

the recognition core is its own crate, `engram`, a general trajectory codec. the same oscillator math also does lossy compression (a real cascaded encoder, plus a streaming mode) and will embed any byte stream, not just pen strokes. it's reused beyond gestures: it's the engine behind the rhythm familiar below, and the reason the twin knocks back *your* rhythm instead of a canned loop.

because a resolved weave is just another `Trigger`, it dispatches through the same engine as a hardware button, which is what lets it drive a whole family of **new-input instruments** off the same capture:

- **teleport**: tap-hold pops a live minimap of your real monitor layout with your open windows as blobs; drag a ghost and release to warp the cursor (and focus a window if you land on it). left-click *summons* a window to your hand; right-click *grabs* one and drops it on another monitor or virtual desktop. dwell on a window and a live DWM-thumbnail portal blooms without stealing focus.
- **tether**, the "warpstone": drop a spot, warp back to it later at the same relative pixel. a second mode is a **wormhole** between two fixed anchors: one press swaps you A⇄B.
- **whiteboard**, a virtual-screen, click-through annotation canvas: custom brushes, command-strokes (loop = lasso, `~` = tidy, slash = delete), lasso-select-and-edit (recolour, rebrush, resize), undo/redo, and a laser presentation mode.
- **glance**: cast a target and every matching window opens as a live thumbnail tile in a magnetic collage; a right-drag crops a tile, a double-click steps you through the portal.
- **window verbs**: summon / banish (with submodes: the hovered window, or any window behind others) / kill / pin.
- **dial**: the next hold becomes an analog slide where speed *is* sensitivity, mapped to output or mic volume.
- **knockback**: a rhythm familiar. you drum on the cast trigger while idle or in queue, and a spectral twin knocks your rhythm back with a small flourish for you to finish. there's no difficulty: you play, and the twin adapts to your rhythm.
- **control**: a quick wheel of system state. which network you're on (ethernet/wifi + SSID + whether you're actually online), your current output device, and a bluetooth toggle, all from instant win32 reads.

and since every one of these is just an `Action`, the palette is full of plainer ones you can hang off *any* trigger, weave or not: autofire/turbo, media keys, lock, sleep, **echo** ("do that again", replay the last action), **ghost-paste** (the clipboard typed as real keystrokes, so it lands in game chats and RDP), a portable **pocket** clipboard that carries every format and can persist to disk, and **curtain**, a panic privacy overlay across every monitor.

new input is the whole reason for building an engine instead of a settings panel.

### macros & beacons

a macro is just an `Action` that happens to be a sequence of steps, or a whole python script.

python is the fun tier. write a file with `def macro(ctx):` and neuron runs it in a bundled CPython that stays **warm in the background** (the *Macro Host*): loaded once, imports already paid for, so firing it is basically a function call, well under a frame. (plain key→key remaps never touch python at all; those are free.)

it's **unsandboxed on purpose.** `ctypes` into raw win32, `subprocess`, sockets, files: whatever a program can do, your macro can do. the friendly helpers (`neuron.key` / `type_text` / `click` / `clipboard` / `run`) respect the [arm gate](#the-arm-gate) and quietly do nothing while input's disarmed; reach past them into raw `ctypes` and you're on your own. that's the trade.

it runs in its own process, so it can't take the app down with it. a macro that segfaults (easy to do with raw `ctypes`) only kills the sidecar, which respawns in the background while the app holding your hardware never flinches. one that crashes on *every* press hits a circuit breaker instead of pinning a core forever.

every macro gets a snapshot of **where you were when you fired it**: foreground app, window title, working dir, clipboard, the window you came from. it's frozen at trigger time, so the whole run reasons about one consistent moment (focus discord, type, alt-tab back). each macro also runs on its own queue: spam one and its fires stay in order; a slow one waiting on the network blocks nobody else.

a macro can drive neuron itself, too. through the same path a bound trigger uses it can set DPI, flip a profile, nudge brightness, mute the mic, read the battery, check which profile is live, each change popping the same confirmation card a button press would. it gets a little **key-value store** that survives restarts, and it can **call another macro** like a subroutine (with a guard so nothing loops forever). macros compose.

don't want to write python? the GUI has a **block builder**: drag typed nodes (type, click, open, ask, notify, plus `if` / `repeat` / `for-each`) and it writes the source for you, losslessly both ways. blocks or code, same macro.

> this replaced an earlier engine that compiled your *Rust* to a dll at trigger time and hot-loaded it. genuinely sub-microsecond, genuinely a pain: whole toolchain, compile-at-press lag. the Macro Host keeps the speed without the jank.

**beacons: a macro can stop and ask you something.** `neuron.ask("deploy it?")` pauses just that one macro and floats a quiet line at the top of your monitor: no focus steal, no dialog box. hold your cast trigger and an answer wheel appears under the cursor: flick west for yes, east for no, up or down to pass (the macro takes its default). bailing costs nothing. answering doesn't synthesise any input, so `ask` and `notify` work even in SAFE mode. a macro can talk to you while every keystroke is disarmed. and on the CLI the same question just shows up in your terminal (`[beacon] asks: deploy it?  [y/n]`), so it works headless too.

## won't surprise you

this is a tool whose whole job is injecting input and writing to your hardware, so the safety has to be real. three mechanisms, always on.

### the arm gate

every synthesised keystroke, click, and process-spawn (and every macro helper) goes through one process-wide switch that's **disarmed by default.** tests, the verify pass, anything that isn't the actual running app fires nothing. it's a single boolean that starts safe, and only the daemon or the GUI ever flips it on. (there's a test whose only job is making sure the test suite can never arm input.)

the running app hands the same switch to the Macro Host, so helpers obey it too. `neuron run --safe` is fully read-only: input disarmed, writes paused; `neuron-app --safe` boots the app the same way; and the GUI keeps a separate switch that just pauses device writes. arming takes a deliberate confirm; disarming is instant. reads are always safe.

a thing that remaps your buttons and runs python on a keypress is, by definition, an input hook, which is exactly why you get the full source. read it, build it yourself, throw the binary at virustotal. and a small unsigned binary will sometimes trip defender's smartscreen; that's the tax on indie exes.

### writes that verify themselves

reads are free and always safe. writes are where people get burned, so every one runs the same gauntlet:

1. **backup first.** before any "safe" write, neuron can snapshot the device's entire getter space (every class × id) to `backups/*.json`, the known-good reference you diff a fresh read against (`neuron verify`) when you want to prove nothing drifted.
2. **driver mode.** host control only renders in device-mode `0x03`: razer gates it, synapse flips it, so does `neuron mode driver`. it's idempotent and reverts when you reopen synapse or power-cycle.
3. **volatile first.** writes default to `NOSTORE`: they take effect now but aren't flashed to onboard memory unless you ask (`--persist`). nothing permanent happens until a clean round-trip proves the opcode.
4. **read-back verify.** after the write, neuron re-reads the matching getter and confirms the bytes it sent actually landed. a mismatch is a hard error (`VERIFY FAILED, write NOT trusted`), never a silent success.

and when there's no opcode it actually trusts, it just refuses. no guessing at your hardware. the full ledger of what's proven, gated, and missing is down in [honesty ↓](#honesty-proven-gated-absent).

### confirmations

neuron never changes anything silently. every committed change (dpi, scroll stage, polling, brightness, a profile or layer flip, a macro's `notify`, a swapped side-plate, a dying battery) pops a small **card** on screen, but only *after* the change actually lands. they stack, collapse to just-the-latest, or batch into a digest (your call), and you drag a little mock-monitor to say where they show up or mute the kinds you don't care about. they can make noise, too: a soft chime per kind, and a sharper one as the battery crosses 20 / 10 / 5 / 2%. it's the OSD synapse pops at you, minus the part where you can't turn it off.

## life after synapse

getting your settings off synapse, getting synapse off your machine, and teaching neuron hardware it's never seen.

### import a profile

synapse's export files (`.synapse3` / `.ChromaEffects`) are just zips of plaintext XML with a funny extension. no crypto. (its *cloud* cache is properly AES-encrypted, and we leave that alone, but the in-app Export is wide open.)

```
neuron import-export your-profile.synapse3 --apply
```

neuron unzips it and reads each capability by its **feature-GUID** rather than its name, which happens to be stable across devices and synapse versions, so it doesn't care which synapse made the file, and anything it doesn't recognise gets logged and skipped instead of blowing up. it also throws out the **noise**: a synapse keymap exports ~100 keys, most of them still bound to their own default, so neuron diffs against the standard key table and keeps only the ones you actually changed. a handful of real rules instead of a hundred. DPI, stages, polling, brightness, idle-off, gaming-mode, your binds (base *and* hypershift, kept apart), lighting: it all comes over. animated lighting becomes live compositor layers, static frames are captured per-key. run it without `--apply` and it just shows you what it *would* import and what it dropped.

what you get is a clean neuron profile (plain TOML in `profiles/`) where every field is optional, so applying it only touches what it sets, and a write that doesn't take just shows up as `skipped` rather than a faked success.

### purge synapse

importing is half of it. the other half is getting synapse *off the machine*:

```
neuron-app --scan-synapse      # dry run: list every razer service + process it would touch
neuron-app --purge-synapse     # demote the services, stop the respawn engine, end the tree
```

it works out which services are razer's by asking windows who installed them (not a hardcoded list), sets them to manual so they stop resurrecting, then walks the process tree and ends the whole razer branch, asking for admin once to do it. there's a button for it on the system page. synapse and neuron can't really share a device anyway, so this is just the clean break. (and if you'd rather pull your settings out of a live install first, `neuron import` does that.)

### discover new devices

```
neuron discover
```

point it at any `razer_report` device and it pokes the whole command space (finding the right pipe by vendor id and a 91-byte feature report, wherever it lives) then sorts each reply by its *shape*: an enum, a level, an x/y pair, a table, a string. line two devices up and the pattern falls out: a command they both answer is shared protocol, one only a single device answers is that device's own trick. nothing hardcoded.

it's already fingerprinted a keyboard it had no entry for, and `discover --emit` drops a starter TOML for each unknown device into `devices/`. adding a device is writing a file, not writing code.

## the app

there's a GUI. dark, monospace, no decoration. the green accent means something is live.

it lives in the tray. the window is built once at startup and just hidden when you close it, so the live `trigger → action` loop always has something to talk to, and it's still one process, no separate daemon. the tray menu handles most of the day-to-day (profile and effect quick-picks, hypershift, brightness and dpi nudges, pause-writes), with a few global hotkeys for the rest (ctrl+alt+h hypershift, ctrl+alt+p pause-writes, ctrl+alt+n open). if it crashes it relaunches itself and writes a plain, readable crash log so you can see what actually happened.

four sections, because that's what a user actually needs:

- **device**: dpi (with the stage table on the slider), polling, brightness, sniper, battery, and a clearly-marked *gated* shelf for the writes that aren't hardware-confirmed yet.
- **lighting**: a render of *your* board, generated from what the registry actually knows (rows × cols + device kind). you paint per-key directly on it, and effects run on it with the same frame math the hardware gets. effect tiles with live previews and auto-generated knobs, plus a stack strip for layering. a mouse with a few LEDs falls back to zones, and says so.
- **input**: direct binds and spellweaving, sharing one action palette. a lamp lights on each rule when its trigger fires, so you can check a bind just by pressing it.
- **system**: the gates, migration and the synapse purge, appearance (accents + cast materials), a reliability bench (uptime, worker heartbeats, auto-restart, the crash log), the beacon registry, the mechanical-advantage toggles, notification settings, and the diagnostics bench: nine real probes (enumerate HID, load the registry, round-trip a device, build a lighting frame, resolve the effect engine, assemble the spine, check the macro runtime, load the gesture vault, resolve a mic endpoint), each reported pass, fail, or skip. read-only, always safe to run, and the same thing CI runs.

profiles open as a sheet from the header wherever you are, saving and restoring the other sections as one bundle (read back from the live device, not from the sliders).

## get it

### build

```
cargo build --release      # -> target/release/neuron.exe (CLI) + neuron-app.exe (GUI)
cargo test  --workspace    # the suite: runs disarmed, never injects
cargo clippy --workspace
```

the normal `--release` build is tuned for snappy runtime (ThinLTO, stripped). use `--profile release-size` if you want it small, `--profile release-fast` if you want it quick, and `RUSTFLAGS="-C target-cpu=native"` outside the repo for native codegen. panic is `unwind`, not `abort`, on purpose: cleanup still runs when something panics, so the app never leaves your gear in a state you didn't ask for. every panic gets logged.

### the CLI

two binaries: `neuron` (the CLI) and `neuron-app` (the GUI). the stuff you'll actually type:

```
neuron list                          recognized devices
neuron dpi 1600                      set DPI (read-back verified)
neuron dpi-stages 800 1600 3200      set the DPI cycle (the whole table)
neuron lighting effect fire          dry-run the exact bytes per device
neuron lighting mirror               paint one device's vitals onto another's LEDs
neuron backup                        snapshot every device's full state
neuron import-export prof.synapse3 --apply   eat a synapse export
neuron run                           the remap daemon (esc to stop; --safe = observe only)
neuron macro add lift my.py          register a python macro into the warm sidecar
neuron macro prelude                 the `neuron` module reference (ctx + helpers + ask/notify)
```

<details>
<summary><b>the full command tree</b> (every subcommand takes <code>--help</code>)</summary>

```
device      list · info · battery · dpi · polling · dpi-stages · scroll ·
            brightness · sniper · storage · mode · backup · verify · watch · probe
lighting    effect · run · mirror · keytest · cellsweep · cells
input       bind · radial · cast · gesture
macros      macro (list · add · run · check · prelude)
audio       audio (list · monitor · mic · out)
profiles    profile (list · show · save · apply · capture · autoswitch)
migrate     import · import-export · discover [--emit]
instruments twin (knockback: demo · stats · sigil · stage) · pocket
gui         neuron-app  [--safe · --tray · --purge-synapse · --scan-synapse]
```

</details>

## contributing

neuron is one person, one desk, one vendor gone deep. that's the point, but it also means a handful of pieces are wide open, and some are shaped so you can own one cleanly without reading the whole tree. here's the honest map, graded by how much groundwork is already done.

the layout, so you know where things live: `neuron-core` is the headless engine (protocol, registry, lighting, the trigger/action spine, gestures, macros) and it's portable by construction. `neuron-app` is the windows GUI and the live driver. `neuron-cli` is a thin front-end. `neuron-host` is a separate protocol hub (an OpenRGB / Chroma-REST bridge, so neuron can drive a mixed-brand rig). `engram` is the gesture codec, vendored under its own terms. the rule everywhere: semantics live in core as typed, tested code; device wiring lives in data.

### pieces you can pick up cleanly

- **a new razer device.** run `neuron discover --emit` and it drops a starter TOML in `devices/`, which the registry loads at runtime with no recompile. fill in the command names and matrix dims and you have a device. the limit: a fixed opcode is just data, but a computed payload (a dpi-stage table, a lift-off handshake) or a lighting dialect that isn't the legacy or matrix one needs rust in `writes.rs`. read-back verify guards every write, so a wrong guess fails loud instead of bricking anything.
- **a lighting effect.** one entry in the pattern registry plus the generator (a `field()` that returns brightness per cell). the factory, the tuning knobs, and the gallery tile all derive from that single entry, and a half-registration won't compile. pure math, fully self-contained, a good first PR.
- **a preset (a look).** pure data: an existing pattern plus a spectrum. paint fire with an ocean gradient and it's a new look with zero code.
- **a protocol adapter for neuron-host.** a small codec that talks to the internal bus. OpenRGB and Chroma REST already exist; wanted next are things like OBS, MQTT, WLED, MIDI. well-scoped, with a capture-and-replay harness to prove it.

### bigger pieces, if you want to own a real chunk

- **the linux / mac port.** the core already ports. the platform-specific parts (HID, audio, the layered overlays, raw input, the window manager) already sit behind seams that compile as inert stubs off windows, so porting is a matter of filling those in: a hidraw or IOKit transport, an ALSA/PipeWire/CoreAudio control, a layered-surface backend, an input source, a window-manager impl. the hidraw transport alone lights up all of device control and the whole CLI on linux. the one genuinely welded chunk left is the overlay instruments (teleport, whiteboard, glance, curtain), which still need their windows bodies factored out first. the map: the cross-platform readiness scorecard in `docs/TDD.md` (§9) and `docs/PROTOCOL-HOST-RND.md`.
- **another vendor entirely (logitech and friends).** the straight answer: there's no vendor abstraction yet. the wire format is razer-specific above the HID transport, so native logitech (HID++) means first extracting a device-protocol trait and then writing an implementation of it. that's a real project, so talk to me about the shape before you start. if what you actually want is other-brand *lighting*, the far better path is driving those devices through the neuron-host OpenRGB client and letting neuron be the sync hub, instead of reverse-engineering each vendor. one thing to avoid outright: the per-vendor LED-SDK DLLs (the razer/corsair/logitech "chroma-like" SDKs). they're anti-cheat bait and a maintenance sinkhole.

### the gates

every change runs the same suite i do: `cargo test --workspace`, `cargo clippy --workspace`, and the one hard invariant, that tests may never arm input (there's a test whose only job is enforcing that). device writes either read-back-verify or stay behind a `NEURON_*_WRITE` gate until a capture confirms them. the diagnostics bench on the system page (also what CI runs) is the "prove it works" surface, read-only and always safe.

### license, for contributors

neuron is under the Whisper Protocol License (`LICENSE.md`), the same license as the rest of my tools: free for individuals, nonprofits, education, other open-source projects, and bootstrapped shops, with a commercial license for funded companies. by sending a contribution you agree to the contribution terms in that license (you grant me the right to ship your change under it). if that's a dealbreaker, no hard feelings, better to know up front.

## honesty: proven, gated, absent

the full per-feature status (solid to barely-started) lives in [state of the project](docs/STATUS.md); this section is just the device-write ledger.

it's **windows-only right now** because that's what i'm on. the guts are written to port (HID, audio, raw input, overlays, foreground detection all sit behind seams), but the code behind those seams is Windows today; everywhere else honestly says "not supported". the linux/mac backends still need writing.

and i'm not pretending this is the most mature or the broadest thing in the space. on linux, [openrazer](https://github.com/openrazer/openrazer) is the real, decade-hardened answer (kernel driver, a couple hundred devices, an actual community), so use it. if you want one panel for every RGB brand under the sun, that's [OpenRGB](https://openrgb.org). neuron is deliberately narrow: one vendor, one desk, gone deep. that narrowness is the point.

every device write is sorted by how sure i am of it:

| capability | status |
|---|---|
| reads: dpi, battery, polling, brightness, storage, lighting state | **proven** on hardware |
| dpi · polling · brightness · lighting writes | **proven** on hardware |
| dpi-stage table · scroll-stage select | **wire-confirmed** off synapse (USBPcap) + round-tripped |
| symmetric lift-off distance | **proven**: reads back clean on the Naga |
| asymmetric lift-off distance (split lift/landing) | **proven**: set/read round-trip on the Naga (the `0x0B/0x85` getter echoes mode=async + the lift/landing pair; the physical split confirmed by feel) |
| idle/sleep timer | **proven**: set/read round-trip on the naga (write echoes back on the getter) |
| in-game hi-res polling · scroll *curve* table · snap-tap (SOCD) | **gated** behind `NEURON_*_WRITE` until a capture confirms; payloads unit-tested, still read-back-verified |
| debounce · onboard button-remap | **no known opcode**: bails with a "needs RE" note, never a blind write |

things it flat-out doesn't do, so you know before you install:

- **doesn't crack synapse's encrypted cloud profiles.** the AES'd account cache is the lock-in, and we don't touch it. the plaintext in-app *export* is what migration can read.
- **doesn't sandbox your macros.** full unsandboxed CPython is the point: a macro can do anything a program can. the arm gate guards the *convenience* helpers, not the raw APIs you reach past them into.
- **no telemetry, no account, no cloud.**

## philosophy / non-goals

- no cloud. no account. no telemetry. ever.
- do what's asked, change nothing else.
- the device's own numbers are the truth: no weird filters or translations.
- novelty done well (spellweaving, radial, the open effects engine) is the fun. reheated gimmicks are not, and won't be added.
- lean is a feature. every background cost has to earn itself.
- more than synapse is fine; bloat is not. every extra thing here is deliberate and low-level, or it doesn't ship.
- built for one desk: mine. if it fits yours too, great; that was never the requirement.
- when neuron doesn't know something about your hardware, it says so instead of guessing.

built from first principles because the alternative is a 2GB login screen for a mouse.
