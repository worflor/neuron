# neuron

a lean, mean, do-what-i-say control layer for razer gear, built by Woflo Labs
as an *anti-synapse*. (woflo labs is a publishing name, not a company - it's me,
one person, obsessed with research in my free time)

two small executables, one shared core. no account, no cloud, no telemetry. no "please update razer central." it talks to your mouse and keyboard directly: the same `razer_report` HID bytes synapse sends, worked out from wire captures (USBPcap), the open-source [openrazer](https://github.com/openrazer/openrazer) driver, and a lot of live probing. it does exactly what you tell it then fricks off. no kernel driver, no vendor SDK 😳

| | |
|---|---|
| **what** | a tray-resident app and a CLI built to replace razer synapse |
| **platform** | windows app + CLI in v0.1.1. linux app + CLI build from source, but the linux release is deferred and hardware control is unverified; no mac |
| **hardware** | razer mice + keyboards over raw HID; daily-driven and hardware-verified on a Naga V2 Pro + BlackWidow Chroma V2. other `razer_report` devices need their own checks. experimental Logitech HID++ discovery is read-only and has no hardware verification |
| **install** | use the windows installer or unpack its portable zip anywhere writable, or build from source: `cargo build --release` |
| **footprint** | no vendor driver, account, or cloud; your config is plain TOML |
| **license** | most of Neuron is GPL-3.0-or-later with a linking exception; Engram and the eigenmotion research modules have separate Woflo Labs community-source terms. [the exact split](LICENSE.md) |

> **status: beta mk1.** windows-first, single dev, very much a personal project with too much ambition. mk1 is the same release language my other tools use, and it means exactly this: until now the only eyes and hands on this thing were mine. it works on my desk every day but *obviously* hasn't been tested on yours, and that gap is the whole definition. that goes for getting it onto your desk too: the windows install path has been run end to end here, the linux GUI has opened under WSL2 and its CLI has run on Ubuntu 22.04, but no razer device has ever been plugged into a linux box running neuron. once it has survived desks that aren't mine, it graduates to mk2. where each feature actually stands, and what evidence is behind each one, is tracked in [state of the project](docs/STATUS.md).

---

## contents

- [why this exists](#why-this-exists)
- [the idea: `trigger → action`](#the-idea-trigger--action)
- [what it does](#what-it-does)
- [won't surprise you](#wont-surprise-you)
- [get it](#get-it)
- [contributing](#contributing)
- [honesty: proven, gated, absent](#honesty-proven-gated-absent)
- [philosophy / non-goals](#philosophy--non-goals)

the full feature depth — every subsystem, up close — lives in
**[the feature design doc](docs/GDD.md)**. how it's built is
**[the technical design doc](docs/TDD.md)**. what actually works today, graded
honestly, is **[state of the project](docs/STATUS.md)**.

---

## why this exists

i ran synapse 1. then 2... and hated it but survived with it. then 3 came out and i reluctantly downloaded it, but it's *still synapse*. i figured 3 was the ceiling. then **4**??? and one day the app told me i *had* to update!? i said nuh uh. neuron started roughly there. (the other half of the spark: i wanted to use my mouse's onboard storage like a little usb drive. and cast spells.)

to be clear, i love my razer hardware. this is anti-*synapse*: a multi-process, account-gated, cloud-synced ~2GB install that re-enables features you turned off, forgets settings, phones home, and bolts a login screen onto your *mouse dpi*. a fine idea drowned in shittification.

neuron is the opposite design, on purpose:

- **one resident app process, with a hidden window in tray mode.** the live remap loop runs *inside* it. Python workers start on the first macro, a manual arm, or an editor action; they are not separate device-control daemons.
- **no cloud, no account.** your config is plain TOML on your disk. you can read it, diff it, and check it into git if you want :P
- **deterministic.** it changes what you ask and nothing else. no surprise re-enables, no "smart" anything you turned off three separate times. new Python macros start BOUND to Neuron's capability surface; add `# neuron: raw` when you deliberately want full Python to LITERALLY do whatever you want. true freedom, with the escalation visible in source.
- **on-device first.** push settings to the mouse's onboard memory and you can uninstall *everything*. the dream is no software at all. (older hardware has no onboard storage, so we make do.)

i'll be honest about the name, though: neuron isn't a faithful, minimal re-implementation of synapse. it's closer to *synapse+++*: it does **more** (gestures, an open effects engine, real-code macros, a whiteboard tool built in, a suite of hand-built actions). all of it is built low-level, and every decision has a reason i can point at. it's meant for *me*, and replacing slop with slop would defeat the whole point. more, sure. bloat, never.


## the idea: `trigger → action`

everything in neuron is **one primitive**. *something happened* (a `Trigger`) so *do this* (an `Action`). that's the magic. a button press, a drawn glyph, a radial flick, the foreground app changing, a tap on your mic, a held hypershift layer are all `Trigger`s; a keystroke, a macro, a dpi cycle, a profile switch, a python script, "teleport to the other monitor" are all `Action`s.

that isn't a description of the architecture, it's the reason the thing is worth building. because a drawn glyph and a button press are *the same kind of event*, anything you can bind to one you can bind to the other, and a new input source doesn't need a new pipeline to hang off. there is exactly one dispatch engine in here.

**press-to-bind anything and everything**: don't like my setup? make it yours.

[the full trigger and action tables →](docs/GDD.md#the-idea-trigger--action)

## what it does

the short tour. each line links into [the feature doc](docs/GDD.md), which has the real depth.

| | |
|---|---|
| **[talks to your gear](docs/GDD.md#talks-to-your-gear)** | dpi (single or the whole stage table), polling to 8000 Hz where the hardware has it, brightness, idle timer, scroll stage, lift-off distance, onboard storage, battery. reads decode the device's own bytes and match synapse byte-for-byte. no driver, no vendor SDK: razer's control collection answers feature reports on an access-zero handle, and that one trick is the whole foundation. |
| **[lighting](docs/GDD.md#lighting)** | native firmware effects invoked by their real effect-id, per-key frames painted at the device's true LED count, and an open effects engine where a look is a **pattern** (shape and motion) composed with a **spectrum** (colour as a whole program). a preset is pure data. live vitals drop in as another layer in the same stack. |
| **[audio](docs/GDD.md#audio)** | mute and gain for any mic or output, and flipping your default device, from a binding. your mic's mute is itself a trigger. |
| **[the spine](docs/GDD.md#the-spine)** | binds, hypershift layers with four stances (hold · latch · smart · one-shot), side-plate layers that scope binds to the plate actually seated on the mouse, and app-aware profile switching with a fallback so closing a game puts you back. |
| **[spellweaving](docs/GDD.md#spellweaving)** | hold, weave a stroke, release, and it fires as a real keybind. recognised by **eigenmotion** — a stroke fit as a damped complex oscillator, where the eigenvalues are the stroke's identity, so it's invariant to where you drew it, how big, and how fast. the same capture drives a family of instruments: teleport, tether, whiteboard, glance, window verbs, dial, knockback, control. |
| **[macros & beacons](docs/GDD.md#macros--beacons)** | bundled CPython starts on demand and stays **warm** in two authority domains: new macros are BOUND to Neuron's brokered capabilities, while `# neuron: raw` is the explicit full-Python escape hatch. every macro gets a frozen trigger snapshot, persistent state, macro composition, and beacons; RAW and BOUND never share an interpreter. don't want to write Python? the block builder edits the same source document and preserves module-level code/metadata while you work visually. |
| **[life after synapse](docs/GDD.md#life-after-synapse)** | import your synapse export (it's a zip of plaintext XML), purge synapse off the machine properly, and point `neuron discover` at hardware it's never seen to fingerprint it into a TOML. |
| **[the app](docs/GDD.md#the-app)** | a tray-resident GUI in four sections — device, lighting, input, system — plus profiles as one object: its settings, its lighting, and its binds. |

## won't surprise you

this is a tool whose whole job is injecting input and writing to your hardware, so the safety has to be real. three mechanisms, always on:

- **[the arm gate](docs/GDD.md#the-arm-gate).** every synthesised keystroke, click, and process-spawn goes through one process-wide switch that's **disarmed by default**. only the running daemon or GUI ever flips it, arming takes a deliberate confirm, and disarming is instant. tests can't arm it — there's a test whose only job is enforcing that. reads are always safe.
- **[writes that verify themselves](docs/GDD.md#writes-that-verify-themselves).** writes are volatile (`NOSTORE`) unless you ask for storage, with one exception: `scroll` stores the stage onboard by default, the way synapse does (`--volatile` opts out). every write then re-reads the matching getter and confirms the bytes it sent actually landed. a mismatch is a hard error, never a silent success. when there's no opcode it trusts, it refuses rather than guessing at your hardware.
- **[confirmations](docs/GDD.md#confirmations).** nothing changes silently. every committed change pops a small card *after* it actually lands — and unlike the OSD this replaces, you can move it, batch it, or turn it off.

a thing that remaps your buttons and runs python on a keypress is, by definition, an input hook, which is exactly why you get the full source. read it, build it yourself, throw the binary at virustotal.

## get it

### with an agent

if you'd rather not install the windows release by hand, point your AI agent at [`skills/neuron-lazy-update`](skills/neuron-lazy-update/SKILL.md). it's in the repo and the windows packages. the linux updater waits for a linux release asset.

it can install or update neuron, roll back a bad update, run CLI commands for you, and answer questions from the docs. when something doesn't work, it looks into it first, and offers to draft a proper issue only if it turns out to be a real, unreported bug. it checks every download before installing and never touches your config. it's written to be followed step by step, so it doesn't need a frontier model.

for something that looks broken, start with [troubleshooting and bug reports](skills/neuron-lazy-update/issues.md): known limits, setup checks, existing reports, then a useful issue if it is new. for writing Python macros, [macros in neuron](skills/neuron-macros/SKILL.md) covers the API, authority modes, beacons, and how to check a macro before running it.

the script under it does the risky parts and reports plain status lines, one per platform — `neuron-update.ps1` for windows, `neuron-update.sh` for linux. you can run either yourself without an agent anywhere in the loop; `--action check` (or `-Action check`) only reads.

### build

v0.1.1 provides a windows installer and portable zip, each with the app and CLI. both include the license bundle, the agent skills, and a `SOURCE.txt` naming the source commit and build method. linux packaging is deferred; the source still builds there.

the windows builds are **not code-signed**. windows SmartScreen may warn on first run. check `SHA256SUMS.txt` against the downloaded installer or zip, then read the packaged `SOURCE.txt` for the exact commit and build method. github-built artifacts may also carry a provenance attestation; locally built artifacts do not. when an attestation is attached, verify it with:

```
gh attestation verify neuron-<version>-windows-x86_64.zip --repo worflor/neuron
```

the installer places neuron in `%LOCALAPPDATA%\Programs\Neuron` and adds a Start menu shortcut. the zip stays portable: extract it anywhere writable and run `neuron-app.exe`.

packages built from current source also carry an optional native Chroma broker. its
[one-time protected setup](docs/PROTOCOL-HOST.md) needs an administrator PowerShell under the
same Windows account; the tray and macros stay at limited privilege. Chroma REST and OpenRGB
do not need that setup.

```
cargo build --release      # -> target/release/neuron.exe (CLI) + neuron-app.exe (GUI)
.\validate.ps1             # the gates: the suite, disarmed. the same ones CI runs.
```

if the window cannot open on a VM, remote desktop, or an older graphics driver, launch the app with `NEURON_RENDERER=software`. that selects Slint's software UI renderer for that run; it does not change device control or the Windows overlay instruments.

### where your config lives

both binaries resolve every runtime path against one **run root**, so the tray app (autostarted from `System32`) and a CLI you type from anywhere read the same config. it is never the working directory. the same three rules decide it on both platforms:

- **the binary's own folder**, for a normal install somewhere writable — the portable layout: copy the folder, keep your setup. this is what a release archive gives you, unpacked anywhere you like, on either platform.
- **the per-user data dir** when the binary's folder isn't a home we may write to — `%LOCALAPPDATA%\neuron` on windows, `$XDG_DATA_HOME/neuron` (usually `~/.local/share/neuron`) on linux. that's what you get building from source, because the binary sits in `target/`, and config kept *there* is one `cargo clean` from gone, with the `backups/` folder going down with it. it moves out of the build tree so the build tree stays disposable.
- **`NEURON_RUN_DIR`** whenever you set it, which wins over both. pin it if you want your config somewhere specific.

a symlink on your `PATH` doesn't change any of this: the binary resolves its real location, so config stays in the install folder and `~/.local/bin/neuron` is safe.

upgrading from a build that kept config in `target/release`? the first launch carries it forward and prints where it went. it copies rather than moves, so the old folder stays put as a backup until you clean it. if that copy fails, neuron reports the error and stops before using partial config; the next launch retries.

the normal `--release` build is tuned for snappy runtime (ThinLTO, stripped). use `--profile release-size` if you want it small, `--profile release-fast` if you want it quick, and `RUSTFLAGS="-C target-cpu=native"` outside the repo for native codegen. we keep cargo's default `unwind` (not `abort`), deliberately (see the comment in `Cargo.toml`): cleanup still runs when something panics, so the app never leaves your gear in a state you didn't ask for. every panic gets logged.

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
neuron macro add lift my.py          register a python macro
neuron macro prelude                 the `neuron` module reference (ctx + helpers + ask/notify)
```

<details>
<summary><b>the full command tree</b> (every subcommand takes <code>--help</code>)</summary>

every name is a top-level command: `neuron dpi`, never `neuron device dpi`. the left column is just a grouping. a name followed by `( … )` takes a subcommand of its own, so it's `neuron profile apply`, `neuron lighting effect`.

```
device      list · info · battery · dpi · dpi-stages · polling · scroll · brightness ·
            sniper · lod · storage · mode · game-mode · remap · backup · verify · watch · probe
lighting    lighting (run · effect · mirror · keytest · cellsweep · cells)
input       bind (list · init) · radial (map · pick) · cast (show · init · run) ·
            gesture (selftest · record · match · list · tune) · run [--safe]
macros      macro (list · add · run · check · prelude)
audio       audio (list · monitor · mic · out)
profiles    profile (list · show · save · apply · capture · rename · delete · autoswitch)
migrate     import · import-export · discover [--emit] · adopt [--dry-run]
instruments twin (demo · stats · sigil · stage) · pocket
diagnostics prof (pump)
gui         neuron-app  [--safe · --tray · --purge-synapse · --scan-synapse]
```

</details>

## contributing

neuron is one person, one desk, one vendor gone deep. that's the point, but it also means a handful of pieces are wide open, and some are shaped so you can own one cleanly without reading the whole tree.

the honest map of what's pickable — a device's wire-captured leftovers (scroll stages, side-plate maps), a lighting pattern (one registry entry plus a `field()` generator), a preset (pure data, zero code), a neuron-host protocol adapter, or one of the bigger chunks like the linux/mac port — is in **[CONTRIBUTING.md](CONTRIBUTING.md)**, graded by how much groundwork is already done. the board, **[neuron · orbit](https://github.com/users/worflor/projects/1)**, is the front door: anything in *Up for grabs* is blessed and ready to claim.

new to the tree entirely? **[AGENTS.md](AGENTS.md)** is the fast orientation — the layout, the three hard invariants, and what "verified" has to mean here before you claim it.

the short version of the gates: `cargo test --workspace` is green or it doesn't go in; tests may never arm input; device writes read-back-verify or stay gated. and please don't run `cargo fmt --all` — the source is hand-formatted, and a blanket format would bury your change in ~1900 lines of noise.

### the legal bit

most changes land under GPL-3.0-or-later with the Neuron-Woflo exception. Engram and three reusable codec/eigenmotion modules keep their Woflo Labs community-source terms because they're research components rather than neuron-specific application code. [the license file](LICENSE.md) shows the exact four paths and how the combined build works.

what you write stays yours. the checkbox on a pull request gives Woflo Labs enough permission to ship, maintain, relicense, and commercially license an accepted contribution, with a promise that work accepted into the public project stays available in source form under a public project license. the full agreement is [here](LICENSES/CONTRIBUTOR-AGREEMENT-1.0.md); if an employer or client might own your work, please clear it with them before sending it.

## honesty: proven, gated, absent

the full per-feature status (solid to barely-started) lives in [state of the project](docs/STATUS.md); this section is just the device-write ledger.

**linux is in progress**: the CLI builds and passes local tests, and a GUI opens, but v0.1.1 has no linux download. the [runtime parity branch](https://github.com/worflor/neuron/tree/codex/linux-runtime-parity) has work in progress on live input, overlays, and audio; its overlay still needs the windows renderer's full look. no razer device has tested the linux HID or input paths yet. if you try a source build, [tell me what happened](https://github.com/worflor/neuron/issues). mac seam is unwritten.

on linux, [openrazer](https://github.com/openrazer/openrazer) is the mature option while neuron's hardware path gets real-device testing. if you want one panel for every RGB brand under the sun, that's [OpenRGB](https://openrgb.org). neuron's verified writes remain Razer-focused; the experimental HID++ dialect only discovers read-only Logitech capabilities so far.

every device write is sorted by how sure i am of it:

| capability | status |
|---|---|
| reads: dpi, battery, polling, brightness, storage, lighting state | **proven** on hardware |
| dpi · polling · lighting writes | **proven** on hardware; current setters require matching read-back |
| brightness write | **verified** only on devices with a matching getter; legacy BlackWidow brightness has no paired getter and now requires `NEURON_BRIGHTNESS_WRITE=1` for an explicitly unverified write |
| dpi-stage table · scroll-stage select | **wire-confirmed** off synapse (USBPcap) + round-tripped |
| symmetric lift-off distance | **proven**: reads back clean on the Naga |
| asymmetric lift-off distance (split lift/landing) | **proven**: set/read round-trip on the Naga (the `0x0B/0x85` getter echoes mode=async + the lift/landing pair; the physical split confirmed by feel) |
| idle/sleep timer | **proven**: set/read round-trip on the naga (write echoes back on the getter); behind the `idle-power-write` feature, which both shipped binaries turn on (a bare `neuron-core` library build leaves it off) |
| thumb-grid button remap (`neuron remap`) | **proven**: reverse-engineered live on the Naga V2 Pro (class `0x15`), every write round-trip verified against the `15/82` getter. device-side and **volatile**: it holds while neuron keeps the mouse in driver mode, and the mouse falls back to its onboard profile without a host |
| in-game hi-res polling · scroll *stage* table · snap-tap (SOCD) | **gated** behind `NEURON_*_WRITE` until a capture confirms; payloads unit-tested, still read-back-verified |
| debounce · saving a button remap into onboard memory | **no known opcode**: bails with a "needs RE" note, never a blind write |

things it flat-out doesn't do, so you know before you install:

- **doesn't crack synapse's encrypted cloud profiles.** the AES'd account cache is the lock-in, and we don't touch it. the plaintext in-app *export* is what migration can read.
- **doesn't pretend CPython is a hostile-code sandbox.** BOUND is the safer default capability tier, not an adversarial isolation promise. RAW remains full unsandboxed Python one explicit source line away; if you choose RAW, it can do anything your user account can.
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
