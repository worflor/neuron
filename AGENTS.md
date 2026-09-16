# AGENTS.md

Onboarding for anyone arriving without the history: a contributor, a maintainer, a
person reading out of curiosity, or an AI agent picking up a task. Read this first.
It is the shortest path to being useful here without breaking something that matters.

If you only take three things away, take these:

1. **Tests may never arm input.** A test whose only job is enforcing that exists.
2. **A device write either verifies itself against the hardware's own bytes, or it
   stays gated.** Never guess at someone's hardware.
3. **More, never bloat.** neuron does a lot on purpose. Every addition earns its
   place, or it doesn't ship.

---

## What this is, in four lines

neuron is a tray-resident replacement for Razer Synapse: one small binary (CLI +
GUI) that speaks `razer_report` HID directly. No account, no cloud, no telemetry, no
kernel driver, no vendor SDK. The GUI is Windows-only. Linux runs the CLI: the whole
workspace builds and the whole suite passes there, on a hidraw transport that is
written and tested but has never touched a real device. Mac is unwritten.

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
| `crates/neuron-app` | The Windows GUI (Slint) and the live driver. Tray-resident; the remap loop runs inside it. |
| `crates/neuron-cli` | A thin front-end over core, plus a headless daemon run path. |
| `crates/neuron-host` | The protocol hub: ownership arbiter, signal bus, and the OpenRGB / Chroma adapters other apps drive neuron through. |
| `crates/engram` | Woflo Labs trajectory codec — the eigenmotion math behind gesture recognition. Its own license; see `LICENSE.md`. |
| `crates/neuron-testkit` | Shared test scaffolding: mock transport, fault injection. |

Non-code state that matters: `profiles/*.toml` (a profile is one object — its
settings, its lighting, and its binds), `*.rules.toml` sidecars, `cast.toml`,
`feel.toml`. Device definitions live in `crates/neuron-core/devices/` (curated) and
in the run root's `devices/auto/` (auto-synthesized; a curated file shadows an auto
one).

## Where to read next

| you want | read |
|---|---|
| What it does and why anyone would want it | [`README.md`](README.md) |
| The features in depth — every subsystem, and what it feels like to use | [`docs/GDD.md`](docs/GDD.md) |
| How it's built — runtime model, dispatch flow, safety contracts, risks | [`docs/TDD.md`](docs/TDD.md) |
| The protocol hub's design | [`docs/PROTOCOL-HOST.md`](docs/PROTOCOL-HOST.md) |
| What actually works today, honestly graded | [`docs/STATUS.md`](docs/STATUS.md) |
| How to send a change | [`CONTRIBUTING.md`](CONTRIBUTING.md) |
| The label taxonomy, if you're filing or picking up issues | [`.github/LABELS.md`](.github/LABELS.md) |
| You're helping a *user* install, update, or run neuron, not changing its code | [`skills/neuron-lazy-update/SKILL.md`](skills/neuron-lazy-update/SKILL.md) |

**The code is the source of truth.** `docs/TDD.md` and `docs/PROTOCOL-HOST.md` both
carry a banner saying an LLM wrote them while building neuron. That banner is
accurate, and it is not a disclaimer to skim past: these docs are a map, and a map
can be stale. Verify against the tree before you lean on a detail.

## The gates

**One line. Put it in your plan and run it before you claim you're done:**

```powershell
.\validate.ps1
```

That compiles the whole workspace and runs the suite — the same definition CI runs, because CI
literally invokes this script rather than spelling out its own cargo commands. If it
passes locally it passes in CI, by construction rather than by convention.

```powershell
.\validate.ps1 -Mode full    # + feature matrix, release build, the CI-safe ignored tests
.\validate.ps1 -Locked       # add --locked, exactly reproducing a CI run
```

CI runs exactly this on Windows and on Linux, and only when code actually changed — a
docs-only push skips the build jobs. Lint is deliberately advisory; the reasoning is in the
script next to the command, along with what would have to happen for it to become a gate.

**On Linux**, install PowerShell 7 and run the same thing:

```bash
pwsh ./validate.ps1
```

The whole workspace compiles and the whole suite passes on Linux, so this is a real gate
there, not a smoke test. What it does *not* prove is Windows behaviour: the platform-specific
halves (input synthesis, the overlays, the tray, audio) are inert stubs off Windows, and a
test that exercises them is asserting the stub. If you touched one of those, say in your PR
that the Windows suite was not run locally, and CI will run it.

The platform-free gates — rustfmt and a parse of every shipped `.ps1` — have their own lane:

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

## The invariants, spelled out

**The arm gate.** Every synthesized keystroke, click, and process spawn goes through
one process-wide switch that starts *disarmed*. Only the running daemon or GUI ever
flips it. Tests cannot arm it, and a dedicated test enforces that. If you are
touching anything that synthesizes input, you are touching this, and it is `risky`
work by definition.

**Read-back verification.** Device writes default to volatile (`NOSTORE`) (the one
exception is `scroll`, which stores onboard by default to match Synapse), re-read the
matching getter, and hard-error on a mismatch rather than reporting a silent
success. A capability with no trusted opcode refuses rather than guessing. New writes
stay behind a `NEURON_*_WRITE` feature gate until a wire capture confirms them. A
wrong guess must fail loud; it must never brick anything.

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

## Verifying your work honestly

This is the part agents get wrong most often, so it is stated plainly.

**"It builds and the tests pass" is not verification of a user-facing change.** The
suite runs disarmed, and under `cfg(test)` a deny-by-default transport policy makes
it structurally impossible to reach real hardware. That is exactly what you want from
a test suite. It is also why a green suite says nothing about whether your lighting
effect looks right, your GUI control responds to a click, or your device write landed.

So:

- **GUI changes:** launch `neuron-app` and look at it. A screenshot in the PR is
  worth more than a paragraph.
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
