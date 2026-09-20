# contributing to neuron

neuron is a one-person project with room for focused contributions. you can work on a device, lighting pattern, preset, or protocol adapter without learning the whole tree.

## where the work lives

the board is the front door: **[neuron · orbit](https://github.com/users/worflor/projects/1)**.

- anything in **Up for grabs** is blessed and ready to claim. that's an easy pile.
- **claim it** by commenting on the issue so we don't double-work it. for anything marked `epic`, talk to me about the shape first, those are real projects and i'd rather save you a wasted weekend if it doesn't align with the bigger picture.
- rather work by the kind of thing you love than a single ticket? filter by **Track** (Performance, Memory & safety, Rust tidiness, Protocol & devices, UX & feel, Docs & onboarding, Tooling & repo health, ...). cross it with an **Area** and you get e.g. "lighting performance" work. the `Lane:` cards are standing invitations, jump into any of them.

no board card for your idea? open an issue. bug reports, "this feels wrong", and "have you thought about X" are all welcome.

## what's easy to own

the honest map, graded by how much groundwork is already done.

### pieces you can pick up cleanly

- **the parts of a razer device no getter reveals.** `synth.rs` probes known getter space, keeps supported commands, detects the lighting dialect, measures round-trip time, and writes a device definition to `devices/auto/razer-<pid>.toml`. Hardware still needs a person to verify commands without getters: scroll-stage writes, side-plate reports, transaction IDs, or an unfamiliar lighting dialect. A curated definition in `crates/neuron-core/devices/` shadows an auto one once verified. `neuron adopt --dry-run` prints synthesized TOML without writing it. Unknown writes stay gated; implemented writes read back and report mismatches.
- **a lighting pattern.** (a new shape and motion, not a firmware effect: no protocol work involved.) one entry in the pattern registry plus the generator (a `field()` that returns brightness per cell). the factory, the tuning knobs, and the gallery tile all derive from that single entry, and a half-registration won't compile. pure math, fully self-contained, a good first PR. claimable as [#13](https://github.com/worflor/neuron/issues/13).
- **a preset (a look).** pure data: an existing pattern plus a spectrum, a one-line blurb, and the name of the feed it reads (blank if it reads nothing, which also decides its catalog shelf). paint fire with an ocean gradient and it's a new look with zero code. claimable as [#12](https://github.com/worflor/neuron/issues/12).
- **a protocol adapter for neuron-host.** a small codec that talks to the internal bus. OpenRGB, Chroma (REST and native shared memory) and OBS already exist; wanted next are things like MQTT, WLED, MIDI. well-scoped, with a capture-and-replay harness to prove it.

### bigger pieces, if you want to own a real chunk

- **the linux runtime and mac port.** Linux has a hidraw transport and a Slint GUI with device settings, lighting editor, GTK tray, and best-effort global hotkeys. The CLI builds and passes local tests, but v0.1.0 has no Linux package and no Razer hardware verification. Live input, overlays, and audio are being developed on [`codex/linux-runtime-parity`](https://github.com/worflor/neuron/tree/codex/linux-runtime-parity); the overlay still needs visual parity with Windows. A native Linux hardware run is the most useful next check. Mac has no backend yet. Discuss the scope on [#1](https://github.com/worflor/neuron/issues/1) before starting platform work.
- **another vendor.** Device protocols use a `Dialect` trait. Razer and Logitech HID++ 2.0 dialects exist, but no Logitech device has been tested on hardware; its byte layouts come from libratbag, Solaar and Logitech documentation. If you can test one, start with [#2](https://github.com/worflor/neuron/issues/2). Other-brand lighting can use a future OpenRGB *client* adapter ([#9](https://github.com/worflor/neuron/issues/9)); neuron-host currently provides the server side only. New vendor dialects need design discussion before implementation. Avoid per-vendor LED SDK DLLs because they add driver and maintenance risk.

## the layout

so you know where things live:

- `neuron-core` is the headless engine (protocol, registry, lighting, the trigger/action spine, gestures, macros) and it's portable by construction.
- `neuron-app` is the GUI and live driver on Windows; its Linux GUI has partial runtime backends.
- `neuron-cli` is a thin front-end.
- `neuron-host` is a separate protocol hub (an OpenRGB / Chroma-REST bridge, so neuron can drive a mixed-brand rig).
- `engram` is the Woflo Labs gesture codec, included under its own terms.

the rule everywhere: semantics live in `neuron-core` as typed, tested code; device wiring lives in data.

## the gates

every change runs the same suite i do. green before you open the PR:

```
.\validate.ps1             # the suite. green, or it doesn't go in.
```

CI is configured to call the same script instead of maintaining separate cargo commands. `-Mode full` adds Clippy, the feature matrix, a release build, and ignored tests that need no hardware. A local pass checks the code gates; it cannot prove behavior on hardware you did not test.

The CI build jobs are configured for code changes; docs-only changes skip them.

**on linux?** Install PowerShell 7 and the Slint fontconfig/X11/Wayland and GTK/AppIndicator development libraries, then run `pwsh ./validate.ps1`. The Linux gate includes `neuron-app`. It cannot prove Windows behavior or real-device HID and input behavior. For input, overlays, tray, or audio changes, name the platform and hardware you tested. `-Mode lint` runs the formatting and PowerShell parse checks. Mac has no implementation yet.

**don't run `cargo fmt --all`.** the source is hand-formatted and there's no `rustfmt.toml` pinning that style, so a blanket format rewrites ~1900 sites across the repo and buries your change in noise. match the surrounding style. Clippy is a gate in full mode; address warnings in code you change.

three hard invariants:

1. **tests may never arm input.** there's a test whose only job is enforcing that. don't break it.
2. **device writes read-back-verify, or they stay gated.** a write either confirms itself against the device's own bytes, or it lives behind a `NEURON_*_WRITE` gate until a capture proves it. a mismatch must fail loudly; do not infer hardware safety from a passing unit test.
3. **more, never bloat.** neuron does a lot on purpose, but every addition must justify its complexity and background cost.

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
