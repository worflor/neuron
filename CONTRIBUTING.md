# contributing to neuron

neuron is one person, one desk, one vendor gone deep. that's the point, but it also means whole pieces are wide open, and some are shaped so you can own one cleanly without reading the entire tree. if you want to join me in making this actually useful, there's plenty of room. here's how to jump in without friction.

## where the work lives

the board is the front door: **[neuron · orbit](https://github.com/users/worflor/projects/1)**.

- anything in **Up for grabs** is blessed and ready to claim. that's an easy pile.
- **claim it** by commenting on the issue so we don't double-work it. for anything marked `epic`, talk to me about the shape first, those are real projects and i'd rather save you a wasted weekend if it doesn't align with the bigger picture.
- rather work by the kind of thing you love than a single ticket? filter by **Track** (Performance, Memory & safety, Rust tidiness, Protocol & devices, UX & feel, Docs & onboarding, Tooling & repo health, ...). cross it with an **Area** and you get e.g. "lighting performance" work. the `Lane:` cards are standing invitations, jump into any of them.

no board card for your idea? open an issue. bug reports, "this feels wrong", and "have you thought about X" are all welcome.

## what's easy to own

the honest map, graded by how much groundwork is already done.

### pieces you can pick up cleanly

- **a new razer device.** run `neuron discover --emit` and it drops a starter TOML into the run root's `devices/auto/`, which the registry loads at runtime with no recompile (move it to `crates/neuron-core/devices/` to make it a curated def). fill in the command names and matrix dims and you have a device. the limit: a fixed opcode is just data, but a computed payload (a dpi-stage table, a lift-off handshake) or a lighting dialect that isn't the legacy or matrix one needs rust in `writes.rs`. read-back verify guards every write, so a wrong guess fails loud instead of bricking anything.
- **a lighting pattern.** (a new shape and motion, not a firmware effect: no protocol work involved.) one entry in the pattern registry plus the generator (a `field()` that returns brightness per cell). the factory, the tuning knobs, and the gallery tile all derive from that single entry, and a half-registration won't compile. pure math, fully self-contained, a good first PR. claimable as [#13](https://github.com/worflor/neuron/issues/13).
- **a preset (a look).** pure data: an existing pattern plus a spectrum, a one-line blurb, and the name of the feed it reads (blank if it reads nothing, which also decides its catalog shelf). paint fire with an ocean gradient and it's a new look with zero code. claimable as [#12](https://github.com/worflor/neuron/issues/12).
- **a protocol adapter for neuron-host.** a small codec that talks to the internal bus. OpenRGB, Chroma (REST and native shared memory) and OBS already exist; wanted next are things like MQTT, WLED, MIDI. well-scoped, with a capture-and-replay harness to prove it.

### bigger pieces, if you want to own a real chunk

- **the linux / mac port.** the platform-specific parts (HID, audio, the layered overlays, raw input, the window manager) sit behind seams, so porting is a matter of filling those in: a transport, an ALSA/PipeWire/CoreAudio control, a layered-surface backend, an input source, a window-manager impl.

on linux the first of those is done. the whole workspace compiles there, the entire suite passes there on every push (CI runs the same script it runs on windows), and there's a hidraw transport written against the kernel's own interfaces — sysfs enumeration, `HIDIOCGFEATURE`/`HIDIOCSFEATURE`, `flock` for the wire lock — plus a udev rule in `packaging/linux/`. releases ship a linux CLI tarball.

so the two most valuable things you can do here are unglamorous. **one: plug a razer device into a linux box and run it.** nothing in that transport has ever touched a wire; every linux test is against mocks and parsed descriptors. a single "it enumerated, here's what `neuron info` said" is worth more than any amount of further code. **two: the linux GUI** — the app builds but its tray, overlays, input and audio are stubs, so it opens a window that does nothing. that's the welded chunk now, and it's a real project: comment on [#1](https://github.com/worflor/neuron/issues/1) first and we'll agree the shape. mac is a green field; same seams, an IOKit transport instead.
- **another vendor entirely (logitech and friends).** further along than you'd think: devices speak through a `Dialect` trait, razer is dialect #1, and logitech HID++ 2.0 is already dialect #2 — it claims its pipes, probes, and synthesizes a device def. what it has never had is a real logitech device: every byte layout came from libratbag / Solaar / Logitech's docs and none of it is wire-verified, because there's no logitech gear on this desk. so if you own some, that's the most valuable thing here: [#2](https://github.com/worflor/neuron/issues/2). it's still in *Triage* on the board rather than *Up for grabs*, so comment there first and we'll agree the scope before you start. a whole *new* vendor means writing another `Dialect` impl — a real project, so talk to me about the shape first. if what you actually want is other-brand *lighting*, the far better path, once it exists, is driving those devices through a neuron-host OpenRGB *client* and letting neuron be the sync hub, instead of reverse-engineering each vendor. today neuron-host only runs the OpenRGB *server* side (other tools drive neuron); the client half that would reach out to another OpenRGB-speaking app or device is planned, not built, and it's a well-scoped chunk ([#9](https://github.com/worflor/neuron/issues/9), also still in *Triage*, so comment before starting). one thing to avoid outright: the per-vendor LED-SDK DLLs (the razer/corsair/logitech "chroma-like" SDKs). they're anti-cheat bait and a maintenance sinkhole.

## the layout

so you know where things live:

- `neuron-core` is the headless engine (protocol, registry, lighting, the trigger/action spine, gestures, macros) and it's portable by construction.
- `neuron-app` is the windows GUI and the live driver.
- `neuron-cli` is a thin front-end.
- `neuron-host` is a separate protocol hub (an OpenRGB / Chroma-REST bridge, so neuron can drive a mixed-brand rig).
- `engram` is the Woflo Labs gesture codec, included under its own terms.

the rule everywhere: semantics live in `neuron-core` as typed, tested code; device wiring lives in data.

## the gates

every change runs the same suite i do. green before you open the PR:

```
.\validate.ps1             # the suite. green, or it doesn't go in.
```

that's the whole gate, and it's one script on purpose: CI *calls it* instead of listing its own cargo commands, so what you run locally and what runs on your PR can't drift apart. `-Mode full` adds clippy, the feature matrix, a release build, and the ignored tests that don't need hardware. both CI test jobs — windows and linux — run the plain form above, so either one is reproducible locally.

CI runs on every push and PR — though only when code actually changed, so a docs-only PR won't sit there building rust for ten minutes.

**on linux?** install PowerShell 7 and run `pwsh ./validate.ps1` — the same command, the same suite, and it passes. what it can't prove is windows behaviour: the win32 halves are inert stubs off windows, so a test that exercises one is asserting the stub. if you touched input, the overlays, the tray or audio, say in your PR that you couldn't run the windows suite and CI will run it for you. (`-Mode lint` is the rustfmt + shipped-script lane, if you want just that.) on mac, nothing is written yet.

**don't run `cargo fmt --all`.** the source is hand-formatted and there's no `rustfmt.toml` pinning that style, so a blanket format rewrites ~1900 sites across the repo and buries your actual change in noise. match the style of the code around you instead. same story with clippy: there are ~120 existing warnings, mostly pedantic, so it's a thing to read rather than a wall to clear — just don't add new ones in code you touch.

three hard invariants:

1. **tests may never arm input.** there's a test whose only job is enforcing that. don't break it.
2. **device writes read-back-verify, or they stay gated.** a write either confirms itself against the device's own bytes, or it lives behind a `NEURON_*_WRITE` gate until a capture proves it. a wrong guess should fail loud, never brick anything.
3. **more, never bloat.** neuron does a lot on purpose, but every addition earns its place. replacing slop with slop defeats the whole point.

the diagnostics bench on the system page is the "prove it works" surface: read-only, always safe, and the fastest way to show a change actually landed on real hardware instead of just compiling.

## building it (windows gotchas)

- kill the running tray instance before a release build. windows locks the live `.exe` and the build fails.
- don't run two builds at once. it corrupts the incremental cache (you'll get bogus `LNK2019 anon.*.llvm`); clear `target/debug/incremental` to recover.
- `cargo clean` is safe. your config does not live in `target/` — a source build puts the run root in `%LOCALAPPDATA%\neuron` for exactly that reason (see [where your config lives](README.md#where-your-config-lives)). test binaries are the exception on purpose: they live in `target/*/deps`, so their run root stays beside them and the suite can never touch your real profiles.
- editing source on windows: don't round-trip repo files through `Get/Set-Content` in powershell 5.1. it re-encodes UTF-8 as ANSI and leaves mojibake. use an editor that keeps UTF-8.

## sending a change

1. fork, then branch off `main` (`git checkout -b area/short-description`).
2. keep commits focused and the message honest about what changed and why.
3. run the gates above.
4. open a PR against `main`. describe what you did, how you verified it, and (for device or lighting work) what hardware you tested on. link the issue you're closing.
5. read and check the contributor-agreement box in the pull request template. i can't merge a contribution without that record.
6. if it touches a board card, mention it so i can move it along the pipeline.

small, self-contained PRs get reviewed fastest. if you're planning something big, open an issue first so we can agree on the shape.

## the legal bit

most of neuron is GPL-3.0-or-later with the Neuron-Woflo exception. Engram and the three reusable research modules named in [LICENSE.md](LICENSE.md) use the Woflo Labs Community Source License instead; a contribution follows the license of the path where it lands.

you keep ownership of what you write. checking the pull request box accepts the [Woflo Labs Contributor Agreement 1.0](LICENSES/CONTRIBUTOR-AGREEMENT-1.0.md) for that contribution, which gives Woflo Labs room to maintain and license the project while keeping accepted public work available in source form. if an employer, client, or school might control the work, make sure you have permission before submitting it.

thanks for being here. build something you'd want to use.
