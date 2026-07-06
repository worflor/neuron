# contributing to neuron

neuron is one person, one desk, one vendor gone deep. that's the point, but it also means whole pieces are wide open, and some are shaped so you can own one cleanly without reading the entire tree. if you want to join me in making this actually useful, there's plenty of room. here's how to jump in without friction.

## where the work lives

the board is the front door: **[neuron · orbit](https://github.com/users/worflor/projects/1)**.

- anything in **Up for grabs** is blessed and ready to claim. that's an easy pile.
- **claim it** by commenting on the issue so we don't double-work it. for anything marked `epic`, talk to me about the shape first, those are real projects and i'd rather save you a wasted weekend if it doesn't align with the bigger picture.
- rather work by the kind of thing you love than a single ticket? filter by **Track** (Performance, Memory & safety, Rust tidiness, Protocol & devices, UX & feel, Docs & onboarding, Tooling & repo health, ...). cross it with an **Area** and you get e.g. "lighting performance" work. the `Lane:` cards are standing invitations, jump into any of them.

no board card for your idea? open an issue. bug reports, "this feels wrong", and "have you thought about X" are all welcome.

## what's easy to own

the README has an honest map of how much groundwork is already done: **[README → contributing](README.md#contributing)**. the short version:

- **a new razer device** is a TOML file, not a recompile (`neuron discover --emit` drops you a starter).
- **a lighting effect** is one registry entry plus a `field()` generator. pure math, self-contained, a good first PR.
- **a preset (a look)** is pure data: an existing pattern plus a spectrum. zero code.
- **a neuron-host protocol adapter** (OBS, MQTT, WLED, MIDI, ...) is a small codec with a capture-and-replay harness.
- **bigger chunks** (the linux / mac port, a real vendor abstraction) are described in the README and in `docs/AUDIT.md` / `docs/PROTOCOL-HOST-RND.md`. talk to me before starting one.

## the layout

so you know where things live:

- `neuron-core` is the headless engine (protocol, registry, lighting, the trigger/action spine, gestures, macros) and it's portable by construction.
- `neuron-app` is the windows GUI and the live driver.
- `neuron-cli` is a thin front-end.
- `neuron-host` is a separate protocol hub (an OpenRGB / Chroma-REST bridge, so neuron can drive a mixed-brand rig).
- `engram` is the gesture codec, vendored under its own terms.

the rule everywhere: semantics live in `neuron-core` as typed, tested code; device wiring lives in data.

## the gates

every change runs the same suite i do. green before you open the PR:

```
cargo test --workspace
cargo clippy --workspace
cargo fmt --all
```

three hard invariants:

1. **tests may never arm input.** there's a test whose only job is enforcing that. don't break it.
2. **device writes read-back-verify, or they stay gated.** a write either confirms itself against the device's own bytes, or it lives behind a `NEURON_*_WRITE` gate until a capture proves it. a wrong guess should fail loud, never brick anything.
3. **more, never bloat.** neuron does a lot on purpose, but every addition earns its place. replacing slop with slop defeats the whole point.

the diagnostics bench on the system page (also what CI runs) is the "prove it works" surface: read-only and always safe.

## sending a change

1. fork, then branch off `main` (`git checkout -b area/short-description`).
2. keep commits focused and the message honest about what changed and why.
3. run the gates above.
4. open a PR against `main`. describe what you did, how you verified it, and (for device or lighting work) what hardware you tested on. link the issue you're closing.
5. if it touches a board card, mention it so i can move it along the pipeline.

small, self-contained PRs get reviewed fastest. if you're planning something big, open an issue first so we can agree on the shape.

## license, for contributors

neuron is under the Whisper Protocol License (`LICENSE.md`), the same license as the rest of my tools: free for individuals, nonprofits, education, other open-source projects, and bootstrapped shops, with a commercial license for funded companies. by sending a contribution you agree to the contribution terms in that license (you grant me the right to ship your change under it). if that's a dealbreaker, no hard feelings, better to know up front.

thanks for being here. build something you'd want to use.
