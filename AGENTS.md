# AGENTS.md

Onboarding for anyone arriving without the history: a contributor, a maintainer, a
person reading out of curiosity, or an AI agent picking up a task. Read this first.
It is the shortest path to being useful here without breaking something that matters.

If you only take three things away, take these:

1. **Tests may never arm input.** A test whose only job is enforcing that exists.
2. **Hardware settings with getters read back; streaming writes check replies.**
   Unknown or lower-confidence commands stay gated. Never guess at someone's hardware.
3. **More, never bloat.** neuron does a lot on purpose. Every addition earns its
   place, or it doesn't ship.

---

## What this is, in four lines

neuron is a tray-resident replacement for Razer Synapse: an app and CLI over one
core that speaks `razer_report` HID directly. An experimental Logitech HID++ dialect
does read-only adoption without hardware verification. No account, no cloud, no telemetry,
no kernel driver, no vendor SDK. Windows ships the app and CLI. Linux
builds both from source, with a GUI and hidraw transport; its release is deferred
while live input, overlays, audio, and hardware behavior are verified. Runtime
backend work is on `codex/linux-runtime-parity`. Mac is unwritten.

The whole product is one primitive:

```text
Trigger -> Action
```

*Something happened* (a button, a drawn glyph, a radial flick, the foreground app
changing, a mic tap, a held layer) so *do this* (a keystroke, a macro, a DPI change,
a profile switch, an overlay instrument). There is exactly one dispatch pipeline.
**Do not add a second one.** If you find yourself building a parallel path for a new
input source or a new effect, you have taken a wrong turn — the existing engine is
where it goes.

## The tree

Six crates. The rule everywhere: **semantics live in `neuron-core` as typed, tested
code; device wiring lives in data.**

| crate | what it is |
|---|---|
| `crates/neuron-core` | The headless engine: HID protocol, device registry, lighting, the trigger/action spine, gestures, macros, profiles, and the process-wide safety gates. Portable by construction. |
| `crates/neuron-app` | The Slint GUI and live driver. Windows is the release target; Linux has a partial source-build runtime. |
| `crates/neuron-cli` | A thin front-end over core, plus a headless daemon run path. |
| `crates/neuron-host` | The protocol hub: ownership arbiter, signal bus, and the OpenRGB / Chroma adapters other apps drive neuron through. |
| `crates/engram` | Woflo Labs trajectory codec — the eigenmotion math behind gesture recognition. Its own license; see `LICENSE.md`. |
| `crates/neuron-testkit` | Shared test scaffolding: mock transport, fault injection. |

The cargo package of `neuron-core` is named `neuron` (`cargo test -p neuron`); the
CLI binary is also `neuron`, the GUI binary `neuron-app`.

Non-code state that matters lives in the **run root** (`neuron config path`; the
rules are in the README's "where your config lives"; the file table is
[`docs/TDD.md` §7](docs/TDD.md#7-persistence-model)): `profiles/*.toml` (a profile is
one object — its settings, its lighting, and its binds), `*.rules.toml` sidecars,
`cast.toml`, `feel.toml`. Device definitions live in `crates/neuron-core/devices/`
(curated) and in the run root's `devices/auto/` (auto-synthesized; a curated file
shadows an auto one). Tests resolve their own run root beside the test binary and
never touch the user's.

## Where to read next

| you want | read |
|---|---|
| What it does and why anyone would want it | [`README.md`](README.md) |
| The features in depth — every subsystem, and what it feels like to use | [`docs/GDD.md`](docs/GDD.md) |
| How it's built — runtime model, dispatch flow, safety contracts, risks | [`docs/TDD.md`](docs/TDD.md) |
| The protocol hub's design | [`docs/reference/PROTOCOL-HOST.md`](docs/reference/PROTOCOL-HOST.md) |
| What actually works today, honestly graded | [`docs/STATUS.md`](docs/STATUS.md) |
| The wire facts and the evidence behind each | [`crates/neuron-core/protocol/ledger.toml`](crates/neuron-core/protocol/ledger.toml); the user-facing proven / gated / absent table is in the README |
| What changed in each release | [`CHANGELOG.md`](CHANGELOG.md); the trust and threat model is [`SECURITY.md`](SECURITY.md) |
| End-user questions: install, update, uninstall, troubleshooting | [`docs/FAQ.md`](docs/FAQ.md) |
| Every CLI verb | [`docs/reference/CLI.md`](docs/reference/CLI.md) |
| Compatibility Lab: what the read-only collector does and doesn't claim | [`docs/reference/COMPATIBILITY-LAB.md`](docs/reference/COMPATIBILITY-LAB.md) |
| A design in flight, a plan, an investigation | [`docs/briefs/`](docs/briefs) — working memory, not current truth: a brief's own status line says whether it is built |
| Dated research and diagnosis snapshots | [`docs/archive/`](docs/archive/README.md) — not current, not authoritative |
| How to send a change | [`CONTRIBUTING.md`](CONTRIBUTING.md) |
| The label taxonomy, if you're filing or picking up issues | [`.github/LABELS.md`](.github/LABELS.md) |
| Periodic doc health, shaping, and anti-slop passes | [`worflor/agentic-toolkit`](https://github.com/worflor/agentic-toolkit) (`project-shaping`, `anti-slop`); neuron's doc map is [Keeping the docs true](#keeping-the-docs-true) |
| You're helping a *user* install, update, or run neuron, not changing its code | [`skills/neuron-lazy-update/SKILL.md`](skills/neuron-lazy-update/SKILL.md) |
| You're writing a Python macro or using beacons | [`skills/neuron-macros/SKILL.md`](skills/neuron-macros/SKILL.md), API in [`docs/reference/MACROS.md`](docs/reference/MACROS.md) |
| You're setting up or changing a user's binds, lighting, profiles or whole setup through the CLI | [`skills/neuron-cli/SKILL.md`](skills/neuron-cli/SKILL.md), reference in [`docs/reference/CLI.md`](docs/reference/CLI.md) |
| You're investigating hardware compatibility or collecting evidence for an unsupported device | [`skills/neuron-compat/SKILL.md`](skills/neuron-compat/SKILL.md) |

**The code is the source of truth.** `docs/TDD.md` and `docs/reference/PROTOCOL-HOST.md` both
carry a banner saying an LLM wrote them while building neuron. That banner is
accurate, and it is not a disclaimer to skim past: these docs are a map, and a map
can be stale. Verify against the tree before you lean on a detail. When docs and code
disagree, the code says what is true today; whether the code or the doc is the mistake
is the owner's call, so flag a disagreement you cannot settle instead of silently
picking a side.

## Where a change goes

Semantics are typed, tested code in `neuron-core`; device wiring is data. Start from
the existing seam, and read a neighbouring example before writing yours.

| you are adding or changing | it goes in | what proves it |
|---|---|---|
| a trigger or action | `Trigger` in `neuron-core/src/engine.rs`; `Action` and `Intent` in `action.rs`; the picker list `ACTION_PALETTE` in `authoring.rs`; app and window intents in `neuron-app/src/dispatch.rs`. Never a second dispatch path. | engine/executor tests; any input synthesis or device write is `risky` |
| a device or opcode | a curated TOML in `crates/neuron-core/devices/` (or let `synth.rs` write an auto one); a new opcode needs its entry in `protocol/ledger.toml` | `tests/protocol_ledger.rs`; then a hardware read-back and the diagnostics bench, naming the device |
| a lighting pattern or preset | one entry in `REGISTRY` (pattern) or `presets()` in `neuron-core/src/pattern.rs`; factory, knobs and tile derive from it | the suite, then look at it in the running app; real-keyboard behavior stays "unverified" until run |
| a GUI page or control | `.slint` under `neuron-app/ui/panels` and `ui/widgets`; callbacks in `src/glue.rs`; a how-to entry is a `GuideEntry` in `ui/guides.slint` plus a `GuideButton` | `uilint.rs`, `uifit.rs`, a frame in `apptest.rs`, then launch it |
| a CLI verb | `crates/neuron-cli/src`; authoring logic stays in core (`authoring.rs`) so the GUI shares it; document it in `docs/reference/CLI.md` | CLI tests (a test keeps the `--help` groups in step with the command tree) |
| a macro helper | `neuron-core/src/macros`; update `docs/reference/MACROS.md` and `skills/neuron-macros` | the `tests/macro_*` suites; BOUND vs RAW authority is `risky` |
| a protocol-host adapter | `neuron-host/src/adapters` | `neuron-host` tests, a capture-and-replay fixture from `neuron-testkit` |
| a persisted setting or config file | a file under the run root, loaded through the `salvage.rs` pattern so one bad value cannot cost the whole file; add it to the table in `docs/TDD.md` §7 | the suite; a save/reload test |

## The gates

**One line. Put it in your plan and run it before you claim you're done:**

```powershell
.\validate.ps1
```

That compiles the whole workspace and runs the suite. The CI workflows invoke
this script rather than maintaining separate cargo commands. A local pass
checks the same code gates; it cannot replace a platform or hardware run.

```powershell
.\validate.ps1 -Mode full    # + clippy, feature matrix, release build, the CI-safe ignored tests
.\validate.ps1 -Locked       # add --locked, exactly reproducing a CI run
```

The Windows and Linux CI jobs are configured to run this when code changes; a
docs-only push skips the build jobs. Clippy is a gate in full mode; rustfmt
remains advisory because the source uses an unpinned hand-formatting style.

**On Linux**, install PowerShell 7 and run the same thing:

```bash
pwsh ./validate.ps1
```

That runs the complete Linux workspace, including `neuron-app`. Install the Slint
fontconfig/X11/Wayland and GTK/AppIndicator development libraries first. The gate
cannot prove hardware behavior without a real device, or Windows behavior when a
Win32 seam resolves to a Linux stub. If you touched input, overlays, the tray, or
audio, name the platform and hardware you tested in the PR.

The platform-free gates (rustfmt, a parse of every shipped shell and `.ps1` script,
cargo-deny, cargo-audit) have their own lane:

```bash
pwsh ./validate.ps1 -Mode lint
```

### Do not

- **Do not run `cargo fmt --all`.** The source is hand-formatted and no
  `rustfmt.toml` pins that style, so a blanket format rewrites ~1900 sites, buries
  your change in noise, and destroys `git blame` across a codebase whose comments are
  its most valuable asset. Match the surrounding style instead.
- **Do not run two builds at once.** It corrupts the incremental cache and you get
  bogus `LNK2019 anon.*.llvm` errors; clear `target/debug/incremental` to recover.
  (Separate `CARGO_TARGET_DIR` lanes are fine, and are the way to parallelize.)
- **Do not round-trip repo sources through PowerShell 5.1's `Get-Content` /
  `Set-Content`.** It re-decodes UTF-8 as ANSI and leaves mojibake through the tree.
  Use an editor that preserves UTF-8, or .NET file IO with a no-BOM `UTF8Encoding`.
- **Do not leave the tray app running during a release build.** Windows locks the
  live `.exe` and the build fails.

### Launching the Windows desktop app

For a requested visible launch, use the exact executable on the user's interactive
desktop, without `--tray`, `--safe`, or `-WindowStyle Hidden`. `start_minimized`
only applies with `--tray`; it does not explain an invisible normal launch.
Normal startup enables live dispatch; `--safe` is a deliberately different mode.
Before restarting, use `neuron-app.exe --shutdown-existing` and verify the old
process has exited rather than starting a second device owner.

With Computer Use's `sky.launch_app`, use `app: "process:<absolute exe path>"`.
A bare path selected the installed app instead of the requested build in a verified
Windows run. Confirm the running executable path, then inspect the returned Neuron
window: a PID, Session 1, host log, or `MainWindowHandle` alone does not prove the
desktop UI is visible. See [the launch procedure](skills/neuron-lazy-update/issues.md#windows-app-launch-verification).

An automated shell may run on a private desktop even in the user's session.
Launch the app on the interactive desktop; session and process checks alone are
insufficient. See the launch procedure for desktop selection and verification.

## The invariants, spelled out

**The arm gate.** Neuron-mediated input and process actions pass a gate that starts
disarmed. A normal live launch arms after dispatch starts; `--safe` doesn't. Tests
cannot arm real input. BOUND macros use the brokered gate, but RAW Python has normal
account authority and can bypass it. Any change to input synthesis or arming is
`risky` work.

**Hardware writes and evidence.** Use volatile (`NOSTORE`) settings where the
protocol supports them; scroll stage selection stores onboard by default. Where a
getter exists, verify the setting against the device and fail on a mismatch. Live
lighting frames validate their replies, not a separate read-back of every LED.
Unknown write opcodes stay gated or carry the explicit, reviewed `ungated_debt`
allowed by the protocol ledger. Never infer hardware safety from mock success.

**The protocol ledger.** Every wire fact neuron speaks has an entry in
`crates/neuron-core/protocol/ledger.toml`: its grade (live / capture / agreed / single /
derived), dated hardware evidence, and upstream citations pinned to a commit SHA (facts only,
never copied code). `tests/protocol_ledger.rs` fails when an opcode in a device def or in
neuron-core has no entry, when a grade outruns its evidence, or when a write below `capture`
ships without a gate or declared `ungated_debt`. New opcode, new entry. Settling a conflict on
hardware means updating its entry in the same change.

**Honesty over polish.** When neuron doesn't know something about the hardware, it
says so. The README's proven / gated / absent ledger and the grading in
`docs/STATUS.md` are load-bearing product features, not modesty. If you change what a
capability actually does, update the ledger in the same change — a doc that
overstates the code is a bug here, and gets filed as one.

## Comments

Write the comment a maintainer needs, or write none.

**Keep:**
- **Why**, when the code can't say it: a non-obvious constraint, an invariant, what a
  workaround works around.
- **Protocol and hardware facts**: byte layouts, opcodes, and where a value was verified on
  real hardware. One line each.
- **Safety**: `// SAFETY:` on every `unsafe`, and the reasoning behind the arm gate and
  write gates.
- **Public API docs** (`///`): what it does, its contract, how it fails. A sentence or two.
- **License headers** (`SPDX-…`). A test enforces them.

**Cut:**
- Narration of what the next line obviously does.
- History: how it used to work, what was tried, how long a bug took to find. That belongs
  in the commit message.
- Conversation: asides, jokes, "honestly", rhetorical questions, first-person storytelling,
  ALL-CAPS used as a tone of voice.
- Essays defending a design. Link the doc that already explains it.

The test: a comment has to make sense to someone reading the code cold, with no memory of the
session that wrote it. If it only makes sense as part of that story, it goes in the commit
message, not the code.

## Designing UI

A feature's interface stays inside that feature's space. Anything that explains, configures, or
extends a feature lives next to it: an inline drawer, a line of text that changes, the surface
around it reshaping. It never sends the user to another page, a modal, or a window to finish the
thought. If a control belongs to two places, it lives where the thing it controls is, and the other
place links to it.

- **Show, don't tell.** Point at the thing itself: highlight the keys a colour covers, replay the
  effect a row names, preview a setting on the live subject while it is dragged. A sentence is for
  a fact the picture can't carry (a key name, a hex value, why something is unavailable).
- **No yapping.** No instructional paragraphs, no filler captions, no labels that restate the
  control. If the UI needs a paragraph to be understood, the design is wrong.
- **Flow, never teleport.** Things that appear grow into place; things that go collapse out;
  layout that must move, moves smoothly. Nothing jumps, flashes, or re-lays itself out under the
  pointer. Motion settles quickly and never animates at the user (see `Theme` in `theme.slint`).
- **Drive understanding.** Design for what a curious user will ask next and answer it where they
  are looking. The goal is that using the feature teaches how it works.
- **Useful, not gimmicky.** Every visual earns its place by answering a question. Decoration that
  answers nothing is slop, and so is "juice" added for its own sake.
- **Keep row identity stable.** Update live models in place; replacing a model on every tick
  rebuilds its rows, which drops hover state and half-typed text.
- **Stay inside the box.** Slint compiles an element placed past its parent and text with nothing
  bounding it; both only show when the right data arrives. `src/uilint.rs` fails the suite on those
  two shapes. Bound the element or the text; a deliberate exception takes its allow marker and reason.
  Shapes that only break with particular data are caught at runtime: every frame a GUI test in
  `apptest.rs` draws goes through `src/uifit.rs`, which fails when a layout's child lies outside the
  layout (its contents needed more room than it got). Give a new UI state a frame there, with the
  longest data it can show, at the window's minimum width.

## Keeping the docs true

Each topic has one home; everything else links to it instead of restating it.

| topic | home |
|---|---|
| what a feature does and why it is shaped that way | `docs/GDD.md` |
| how it is built: workers, flow, gates, persistence, risks | `docs/TDD.md` |
| what works today, graded, with dated evidence | `docs/STATUS.md` |
| device-write evidence | `protocol/ledger.toml`, summarized in the README table |
| a command, helper or protocol surface | its `docs/reference/` file |
| what changed, and when | `CHANGELOG.md` and git history, never the design docs |

When you change code, update its home in the same change:

- A user-visible behavior: GDD, plus the CLI reference or macro reference if it has a
  command or helper.
- A new worker, config file, or dispatch source: TDD.
- What a capability actually does on hardware, or what you tested it on: STATUS and, for
  a device write, the ledger and the README table. Never upgrade a grade without
  dated evidence; say what is unverified.
- A new opcode: its ledger entry, or `tests/protocol_ledger.rs` fails.

Docs follow the same rules as comments. Write what is true now: no "used to", no what
was tried, no session narration, no hype, no repeated explanations. History goes in the
commit message and changelog. A design note for work not yet built goes in
`docs/briefs/` with a status line saying so; once it ships, move what is durable into
GDD or TDD and leave the brief as a record. A convention worth enforcing belongs in a
test or lint (as `uilint.rs` and the protocol ledger do), then a line here at most.

Keep the root clean — working notes, plans, and investigations belong in `docs/briefs/`
rather than extra top-level files — promote only durable understanding to its canonical
home so the repo stands on its own without the session that wrote it, and keep comments
sparse, non-conversational, and strictly about the code ([`Comments`](#comments)). For
periodic doc health — reconciling docs against what the tree and history actually show
without flattening neuron's voice — use [`project-shaping`](https://github.com/worflor/agentic-toolkit/blob/main/skills/project-shaping/SKILL.md)
and [`anti-slop`](https://github.com/worflor/agentic-toolkit/blob/main/skills/anti-slop/SKILL.md)
in [`worflor/agentic-toolkit`](https://github.com/worflor/agentic-toolkit).

## Verifying your work honestly

This is the part agents get wrong most often, so it is stated plainly.

**"It builds and the tests pass" is not verification of a user-facing change.** The
suite runs disarmed, and under `cfg(test)` a deny-by-default transport policy makes
it structurally impossible to reach real hardware. That is exactly what you want from
a test suite. It is also why a green suite says nothing about whether your lighting
effect looks right, your GUI control responds to a click, or your device write landed.

So:

- **GUI changes:** launch `neuron-app` and look at it. A screenshot in the PR is
  worth more than a paragraph. A Slint binding loop only fails when its element is drawn
  in the state that reads it (hover-gated UI hides one from every unit test), so extend the
  render smoke in `apptest.rs` when you add UI that changes on hover.
- **Device / lighting changes:** the diagnostics bench on the SYSTEM page runs nine
  read-only probes against real hardware and is always safe. That is the "prove it
  works" surface. Name the hardware you tested on.
- **Things you genuinely cannot verify** (audio output, a device you don't own, a
  platform you're not on): say so explicitly. An honest "unverified on hardware" is
  useful. A confident claim that turns out to have been a compile check is not.

## If you are an AI agent

Everything above applies to you. In addition:

- **Stop and ask the owner** before implementing anything that touches HID
  interception, device write paths, or the python bridge. These carry the `risky`
  label for exactly this reason, and `risky` overrides every other signal, including
  `freetime capable`.
- **Never `cargo fmt --all` to "clean up".** See above. It is the single most common
  well-intentioned way to ruin a PR here.
- **Preserve unrelated working-tree changes.** This is an active one-person project;
  the tree is frequently dirty with work in progress that is not yours.
- **Label correctly when filing:** exactly one Tier-1 (type) and exactly one Tier-2
  (`area:*`); Tier-3 flags optional. `.github/LABELS.md` has the rules,
  `.github/labels.yml` has the set as data. Add `autonomously-derived` if the issue
  was born in an unattended session — it tells the owner how much to trust the
  framing before reading it.
- **Evidence belongs in the issue body**, not in a label. The agent-ready issue
  template asks for reproduced observations kept separate from hypotheses. Keep them
  separate.
- **Don't publish anything outward** — releases, public posts, account or
  configuration changes — without the owner saying so in that session.

## Getting a change in

Fork, branch off `main` (`area/short-description`), keep commits focused, run the
gates, and open a PR saying what you did, how you verified it, and what hardware you
tested on. The PR template has a contributor-agreement checkbox that has to be
checked before anything can be merged — see
[`CONTRIBUTING.md`](CONTRIBUTING.md#the-legal-bit) for what it means and which paths
fall under which license.

Small and self-contained gets reviewed fastest. For anything large, open an issue
first so the shape can be agreed before you spend a weekend on it.
