# neuron

**your razer hardware. your rules.**

neuron is my replacement for Razer Synapse: a free Windows app and CLI for DPI, polling, lighting, profiles and button remapping. i reverse-engineered how Synapse talks to my Razer hardware and built my own controls in Rust. no account, cloud sync, telemetry, extra kernel driver or vendor SDK.

i don't run Synapse anymore. neuron handles my Naga V2 Pro and BlackWidow Chroma V2 every day. if something's missing, i build it in.

**more than a replacement.** layered reactive lighting, Python macros, app-aware binds, radial menus and gesture controls. it's the Synapse stuff i needed, plus the things i always wanted.

**a control layer built to grow beyond Razer.** gamepads already feed the same action engine; the hardware layer can grow to other vendors. Razer mouse and keyboard controls are still the focus for now. [here's what actually works](docs/STATUS.md).

**[download for Windows](https://github.com/worflor/neuron/releases/latest)** · [FAQ & manual help](docs/FAQ.md) · [check device support](docs/STATUS.md) · [help test yours](#help-neuron-on-your-hardware)

<p align="center"><img src="docs/media/hero-desktop.webp" alt="The neuron window on a desktop, with a spellweaving stroke mid-cast and three change confirmations in the corner" width="100%"></p>

| | |
|---|---|
| **status** | Windows beta; daily-driven on the Naga V2 Pro and BlackWidow Chroma V2. Other Razer devices need their own testing |
| **platforms** | Windows 10/11 download. Linux app and CLI build from source, but Razer hardware control is unverified there; macOS has no backend yet |
| **under the hood** | shared trigger → action engine; verified hardware reads/writes; an [evidence ledger](crates/neuron-core/protocol/ledger.toml) for the protocol |
| **docs** | [features (GDD)](docs/GDD.md) · [architecture (TDD)](docs/TDD.md) · [project status](docs/STATUS.md) |
| **license** | mostly GPL-3.0-or-later with an exception; some research components have separate community-source terms. [details](LICENSE.md) |

---

## why this exists

i've been through Synapse 1, 2, 3... and then 4 showed up and wanted to tell me when to update my mouse software. i said nuh uh. i like my Razer hardware. i just got tired of what came with controlling it.

i wanted the basic stuff to work without an account, background clutter or having to fight a settings app. then i wanted better lighting. then i wanted to cast spells with my mouse, and somehow ended up training a small neural network to help pick palette colours. you can probably see where this went :P


## the idea: `trigger → action`

everything in neuron is **one primitive**. *something happened* (a `Trigger`) so *do this* (an `Action`). that's the magic. a button press, a drawn glyph, a radial flick, the foreground app changing, a tap on your mic, a held hypershift layer are all `Trigger`s; a keystroke, a macro, a dpi cycle, a profile switch, a python script, "teleport to the other monitor" are all `Action`s.

that isn't a description of the architecture, it's the reason the thing is worth building. because a drawn glyph and a button press are *the same kind of event*, anything you can bind to one you can bind to the other, and a new input source doesn't need a new pipeline to hang off. there is exactly one dispatch engine in here.

**press-to-bind anything and everything**: don't like my setup? make it yours.

<p align="center"><img src="docs/media/binds-direct.webp" alt="The input page: six binds, each a control on the left wired to an action on the right" width="100%"></p>

<p align="center"><img src="docs/media/controllers-badges.webp" alt="Pad, mouse, keyboard and headset binds side by side, with the device badge drawer open on a gamepad" width="100%"></p>

<table>
<tr>
<td width="50%"><img src="docs/media/radial-wheel.webp" alt="The radial wheel over the desktop, flicked toward the teleport wedge"></td>
<td width="50%"><img src="docs/media/weave-glyph-loop.webp" alt="A loop glyph being drawn while the predicted action, teleport, locks in"></td>
</tr>
</table>

[the full trigger and action tables →](docs/GDD.md#the-idea-trigger--action)

## what it does

the short tour. each line links into [the feature doc](docs/GDD.md), which has the real depth.

| | |
|---|---|
| **[device control](docs/GDD.md#talks-to-your-gear)** | DPI and stage cycles, polling, brightness, scroll stages, lift-off, battery and idle settings on supported hardware. no Synapse needed. |
| **[lighting](docs/GDD.md#lighting)** | layer effects (fire, shallow-water ripples, typing heat, audio meters), gradients, blends and typing reactions. build palettes with [Luma-T](#luma-t-autocomplete-for-the-colour-picker), paint per-key lighting, use live system data, and fade smoothly between looks. |
| **[buttons & profiles](docs/GDD.md#the-spine)** | remap supported controls, use HyperShift and Naga side-plate layers, and switch profiles automatically for different apps. |
| **[spellweaving & radial](docs/GDD.md#spellweaving)** | draw a glyph or flick a wheel to trigger the same actions as a button. teleport windows, summon tools, even draw on your screen. |
| **[macros & beacons](docs/GDD.md#macros--beacons)** | Python macros, a visual block editor, persistent state and on-screen questions from your scripts. BOUND by default, RAW when you want full Python. |
| **[audio & other actions](docs/reference/CLI.md#triggers-and-actions)** | bind mic mute/gain, output switching, clipboard cleanup, screenshots, undo and window actions to your controls. |
| **[games & RGB tools](docs/reference/PROTOCOL-HOST.md)** | opt-in Chroma and OpenRGB connections let games paint through Neuron alongside your own lighting, with a Chroma lab to isolate, recolour, hide or react to game effects. early, and still being tested across games. |
| **[life after Synapse](docs/GDD.md#life-after-synapse)** | import supported Synapse exports, clean up its Windows services, and investigate unfamiliar devices. |
| **[gamepads](docs/GDD.md#controllers-and-anything-else-with-buttons)** | buttons, sticks and triggers can become binds. aim the radial with a stick and use rumble where it's been verified on Windows. |
| **[app & CLI](docs/reference/CLI.md)** | use the app to set things up, or the CLI to change the same settings by hand. configs stay on disk as plain TOML. |

### Luma-T: autocomplete for the colour picker

yes, the app built to escape a 2GB mouse driver now has an AI in the colour picker.

**[Luma-T](docs/GDD.md#lighting)** is a 663k-parameter generative colour model built right into neuron, designed to collaborate with you while you edit and play with your lighting. it doesn't even need a GPU or an internet connection. two small models work side by side in real time: a **592k colour model** that imagines where your palette could go next, and a **70k taste model** that decides what's actually worth showing you.

- **see what it's thinking.** colours that feel out of place get an IntelliSense-style amber squiggle. behind the colour square, faint contour rings trace where stronger fits live, and hills along the hue bar point toward matching hues.
- **preview in an instant.** auto-predict suggestions for the colour you're editing land right inside the square as dots; full-palette adjustments appear as pills underneath. hover to try them live across your palette and gear, click to keep (or drive it like Cursor Tab: `Tab` to preview, `Tab` or `Enter` to keep), `Esc` to let go, or `hold` any colour to lock it down.
- **only speaks when it beats yours.** the taste model measures every idea against leaving your palette untouched. if yours already wins, it stays quiet. hit `ask` (`more` for another round) whenever you want second opinions, or `roll` (`R`) to generate a cohesive palette around the colours you held or your own presets.
- **adapts to your taste.** every suggestion you keep or pass over shapes its taste right on your machine. turn it off anytime in preferences, or drop in your own model.

<p align="center"><img src="docs/media/lighting-aurora-loop.webp" alt="The lighting page: an aurora effect running on the keyboard render while every catalog tile plays its own preview" width="100%"></p>

<table>
<tr>
<td width="50%"><img src="docs/media/effect-fire-aurora.webp" alt="Fire running through the aurora gradient on a keyboard"><br><sub><b>fire</b> in aurora</sub></td>
<td width="50%"><img src="docs/media/effect-typing-heat-rainbow.webp" alt="Typing Heat in rainbow, a sentence spreading heat across the keys"><br><sub><b>typing heat</b> in rainbow, a sentence being typed</sub></td>
</tr>
<tr>
<td width="50%"><img src="docs/media/effect-ripple-bubble.webp" alt="Ripple in the bubble gradient, rings spreading from each keystroke"><br><sub><b>ripple</b> in bubble</sub></td>
<td width="50%"><img src="docs/media/effect-cascade-bubble.webp" alt="Cascade in matrix mode with the bubble gradient"><br><sub><b>cascade</b> in matrix mode, in bubble</sub></td>
</tr>
</table>

<p align="center"><sub>each is one chip on its tile. these are neuron's own pattern-engine frames on a generic keyboard model rendered in Blender, not a photo; the optics are an estimate.</sub></p>

<p align="center"><img src="docs/media/device-feel.webp" alt="The device page: dpi with its stage table on the fader, the side plate readout, and the verify-gated shelf for unconfirmed writes" width="100%"></p>

<table>
<tr>
<td width="58%"><img src="docs/media/macro-blocks.webp" alt="The macro workshop in blocks view: press, ask, then a yes branch and a no branch"></td>
<td width="42%"><img src="docs/media/profiles-sheet.webp" alt="The profiles sheet: saved profiles with their dpi, polling, brightness and lighting, and auto-switch by app"></td>
</tr>
</table>

## won't surprise you

neuron can change hardware settings, send input and run Python. the boring safety stuff matters.

<table>
<tr>
<td width="42%"><img src="docs/media/safety-gates.webp" alt="The safety gates: device writes on, input and macros off until armed"></td>
<td width="58%"><img src="docs/media/toasts.webp" alt="Three confirmation cards in the corner of the desktop: profile, polling and dpi"></td>
</tr>
</table>

- **[input has a switch](docs/GDD.md#the-arm-gate).** a normal Windows launch arms live binds when the input engine starts. `--safe` keeps input disarmed and device writes paused. disarming is instant; turning input back on manually asks first. tests can't arm real input.
- **[hardware changes get checked](docs/GDD.md#writes-that-verify-themselves).** settings like DPI and polling check back with your device to confirm what landed. anything neuron isn't sure your hardware supports stays locked unless you explicitly opt in.
- **[notifications are yours](docs/GDD.md#confirmations).** on-screen cards tell you when supported changes land. move them, quiet them, or turn them off entirely.

and if you turn on RAW Python macros, they run with your Windows account's full permissions, not inside a sandbox. [here's the actual security model](SECURITY.md).

## get it

**Windows 10/11 (x86_64): [download the latest release](https://github.com/worflor/neuron/releases/latest).** Grab the installer for a normal setup, or the portable ZIP if you'd rather unpack it yourself. Both have the app and CLI. Run `neuron-app.exe` from the portable ZIP.

Windows may warn about the unsigned build. The release includes `SHA256SUMS.txt`, and `SOURCE.txt` tells you what source commit the binaries came from. [install and build details](#build).

> **steam at 50 stars.** if this repo hits 50 stars, i'll put neuron on Steam for easier updates. It'll still be free and available on GitHub. this would just be another way to get it :P

### just use it

open neuron from Start, or run `neuron-app.exe` from the portable folder. DEVICE handles hardware settings, LIGHTING builds your effects, and INPUT is for buttons and gestures. profiles are in the header; SYSTEM has diagnostics and the safety switches. no terminal needed.

the [FAQ](docs/FAQ.md) covers installing, updating, profiles, macros and troubleshooting. want to do more by hand? the [CLI reference](docs/reference/CLI.md) covers the same engine, including backups and compatibility tools.

<details>
<summary><b>optional: using a coding agent</b></summary>

if you already use OpenCode, Claude Code or something similar, Neuron includes [setup and update](skills/neuron-lazy-update/SKILL.md), [CLI configuration](skills/neuron-cli/SKILL.md), [macros](skills/neuron-macros/SKILL.md) and [compatibility research](skills/neuron-compat/SKILL.md) skills. they're a shortcut for workflows you can also do yourself. check any device changes and review evidence before sharing it.

</details>

### build

```
cargo build --release      # -> target/release/neuron.exe (CLI) + neuron-app.exe (GUI)
.\validate.ps1             # the gates: the suite, disarmed. the same ones CI runs.
```

on a VM, remote desktop, or older GPU driver, pass `NEURON_RENDERER=software` to use Slint's software UI renderer without affecting device control or overlays. Linux builds from source (`cargo build --release`), with packaging deferred.

<details>
<summary><b>release verification, Chroma broker & build profiles</b></summary>

- **release verification:** GitHub-built Windows archives include `SHA256SUMS.txt`, `SOURCE.txt`, and provenance attestations (`gh attestation verify neuron-<version>-windows-x86_64.zip --repo worflor/neuron`).
- **Chroma broker:** Windows packages provision the native Chroma broker after one UAC prompt so the tray and macros stay at limited privilege; Chroma REST and OpenRGB work without elevation.
- **cargo profiles:** `--profile release-size` builds small, `--profile release-fast` builds quick, and `RUSTFLAGS="-C target-cpu=native"` enables native codegen outside the repo. `Cargo.toml` keeps `unwind` (not `abort`) so cleanup runs and panics get logged instead of leaving your gear mid-state.

</details>

### where your config lives

both binaries resolve every runtime path against one **run root**, so the tray app (autostarted from `System32`) and a CLI you type from anywhere read the same config. it is never the working directory. the same three rules decide it on both platforms:

- **the binary's own folder** when the release archive carries `portable.flag`. copy the folder, keep your setup.
- **the per-user data dir** for installer and source builds: `%LOCALAPPDATA%\neuron` on windows, `$XDG_DATA_HOME/neuron` (usually `~/.local/share/neuron`) on linux. config survives installer upgrades and `cargo clean`.
- **`NEURON_RUN_DIR`** whenever you set it, which wins over both.

a symlink on your `PATH` doesn't change any of this: the binary resolves its real location, so a marked portable copy stays portable and `~/.local/bin/neuron` is safe. upgrading from an older beside-the-executable layout copies config forward on first launch and leaves the old folder as a backup.

### the CLI

two binaries: `neuron` (the CLI) and `neuron-app` (the GUI). the stuff you'll actually type:

```
neuron list                          recognized devices
neuron feel dpi 1600                 set DPI (read-back verified)
neuron feel stages 800 1600 3200     set the DPI cycle (the whole table)
neuron lighting effect fire          dry-run the exact bytes per device
neuron lighting mirror               paint one device's vitals onto another's LEDs
neuron backup                        snapshot every device's full state
neuron import prof.synapse3 --apply  eat a synapse export
neuron run                           the remap daemon (esc to stop; --safe = observe only)
neuron macro add lift my.py          register a python macro
neuron macro prelude                 the `neuron` module reference (ctx + helpers + ask/notify)
```

<details>
<summary><b>the full command tree</b> (every subcommand takes <code>--help</code>)</summary>

`neuron <noun> <verb>`, with `--json` on everything. the full reference is **[docs/reference/CLI.md](docs/reference/CLI.md)**; an agent driving it reads **[skills/neuron-cli](skills/neuron-cli/SKILL.md)**.

```
devices   list · info · battery · adopt · storage · mode · backup · verify · badge · button · compat
feel      feel (show · timing · sniper · dpi · polling · stages · scroll · lod · brightness ·
          game-mode · idle)
input     bind · control · action · trigger · cast · gesture · radial · macro · pocket ·
          run [--safe] · watch · pads · rumble · audio · twin
lighting  lighting (catalog · stack · fps · apply · effect · …)
setup     profile · config · dump · apply · reload · status · catalog · import
gui       neuron-app  [--safe · --tray · --purge-synapse · --scan-synapse]
```

</details>

clipboard tools live on the same `Trigger → Action` path: ordered bounded text transforms, full-format pockets, in-session displaced-state history, and screen/window/region screenshots. `neuron pocket --help` lists pocket management and history commands; `neuron action list` shows the typed action forms.

## help neuron on your hardware

i can't own every Razer mouse and keyboard. if you try neuron on yours, tell me what works, what doesn't, and whether you're using it wired, through a dongle or over Bluetooth. even a partial result is useful. [file a device report](https://github.com/worflor/neuron/issues/new?template=device_report.yml).

want to dig deeper? [Compatibility Lab](docs/reference/COMPATIBILITY-LAB.md#try-it-yourself) has read-only CLI steps to inspect your device and export a reviewable report. nothing uploads automatically, and identifying a device isn't proof its controls work. the optional [`neuron-compat` skill](skills/neuron-compat/SKILL.md) can guide the same steps.

## contributing

want to write code or design an effect instead? there's plenty to pick up without learning the whole tree. [CONTRIBUTING.md](CONTRIBUTING.md) has the device work, lighting patterns, presets, protocol adapters and larger ports, with the groundwork for each spelled out. [neuron · orbit](https://github.com/users/worflor/projects/1) is the issue board; anything in *Up for grabs* is ready to claim.

new to the tree? [AGENTS.md](AGENTS.md) is the orientation for people and coding agents alike: the tree, where each kind of change goes, the gates, and which doc owns which topic. tests can't arm input, and unknown hardware commands don't get a free pass. those rules aren't negotiable.

### the legal bit

most changes land under GPL-3.0-or-later with the Neuron-Woflo exception. [Engram](https://github.com/worflor/engram) and three reusable codec/eigenmotion modules keep their Woflo Labs community-source terms because they're research components rather than neuron-specific application code. [the license file](LICENSE.md) shows the exact four paths and how the combined build works.

what you write stays yours. the checkbox on a pull request gives Woflo Labs enough permission to ship, maintain, relicense, and commercially license an accepted contribution, with a promise that work accepted into the public project stays available in source form under a public project license. the full agreement is [here](LICENSES/CONTRIBUTOR-AGREEMENT-1.0.md); if an employer or client might own your work, please clear it with them before sending it.

## honesty: proven, gated, absent

[the full status page](docs/STATUS.md) tracks feature maturity and hardware testing; the [protocol ledger](crates/neuron-core/protocol/ledger.toml) holds the byte-level evidence.

Linux builds from source but still needs native Razer hardware testing. [OpenRazer](https://github.com/openrazer/openrazer) is the mature Linux option for now; [OpenRGB](https://openrgb.org) covers broader RGB hardware. Neuron's Logitech HID++ discovery remains experimental and read-only.

| capability | status |
|---|---|
| reads: dpi, battery, polling, brightness, storage, lighting state | **proven** on hardware |
| dpi · polling · lighting settings | **proven** on named hardware; setters read back and live lighting checks protocol replies |
| brightness write | **proven**: matrix and legacy setters round-trip checked on the Naga V2 Pro and BlackWidow Chroma V2 |
| dpi-stage table · scroll-stage select | **wire-confirmed** off synapse (USBPcap) + round-tripped |
| symmetric & asymmetric lift-off distance | **proven**: set/read round-trip on the Naga (`0x0B/0x85` echoes split lift/landing; confirmed by feel) |
| idle/sleep timer | **proven**: set/read round-trip on the Naga (enabled in shipped binaries via `idle-power-write`) |
| thumb-grid binds in firmware (`02/0C`) | **proven** on the Naga V2 Pro: volatile, **normal mode only**, read back via `02/8C` and re-applied on connect/wake |
| in-game hi-res polling · scroll *stage* table · snap-tap (SOCD) | **gated** behind `NEURON_*_WRITE` until a capture confirms; payloads unit-tested |
| debounce | **no known opcode**: bails with a "needs RE" note, never a blind write |
| saving binds into an onboard slot | **known, unused**: `02/0C` slots 1–5 persist across power cycles, but neuron keeps binds volatile |

things it flat-out doesn't do, so you know before you install:

- **doesn't crack synapse's encrypted cloud profiles.** the AES'd account cache is the lock-in, and we don't touch it. the plaintext in-app *export* is what migration can read.
- **doesn't pretend CPython is a hostile-code sandbox.** BOUND macros run in a restricted Python mode; RAW macros aren't sandboxed and can do anything your Windows account can.
- **no telemetry, account, or cloud.** Luma-T runs completely offline too.

## philosophy / non-goals

- do what's asked, change nothing else; if neuron doesn't know something about your hardware, it says so instead of guessing.
- lean is a feature. every background cost has to earn itself, and extra features only ship if they stay deliberate and low-level.
- novelty done well (spellweaving, radial, the open effects engine, Luma-T) is the fun. reheated gimmicks are not.
- started on my desk. i'm building the tools to make it work on more of yours, one verified device at a time.

built from first principles because the alternative is a 2GB login screen for a mouse.
