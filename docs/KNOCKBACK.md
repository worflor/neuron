> **🤖 agent-generated · live context doc**
> *not official docs.* an LLM wrote this while building neuron. it may be
> stale, wrong, or slop — or it may be load-bearing and exactly right.
> code is the source of truth; verify before you lean on it.
>
> **kind:** forward build plan / design contract (written for an implementing agent) · **as of:** 2026-07-02 · **trust:** aspirational — parts have shipped, much is still intent; the design brief in the appendix is the binding part, the `file:line` refs drift fast

# KNOCKBACK — implementation plan
### The rhythm familiar, built on the Whisper physics codecs

This is the build document for KNOCKBACK, Neuron's built-in game mode. It is written for an
implementing agent. The high-level vision (the loop, the premise, the emergent systems) is in
the design brief at the bottom of this file — read it first, it is the contract. This document
maps that vision onto Neuron's real architecture and the Whisper codec suite, and specifies the
complete end product in dependency build order. **These are not MVP phases — every movement
below ships toward one complete design; nothing is scoped down, only ordered.**

---

## 0. The architectural thesis (why this is not a minigame bolted on)

The twin is not simulated. The twin **is the codec**.

All three Whisper codecs share one physics: a damped harmonic oscillator
`z[n] = K·z[n−1] − G·z[n−2]` fitted by complex least-squares. That model is *predictive*, and a
predictive model run forward is *generative*. KNOCKBACK exploits this identity three ways:

| Layer | Codec | Role | The trick |
|---|---|---|---|
| **Memory** | **Engram** (256D trajectory, already a full Rust crate) | The twin's brain + save file | Every exchange is encoded as an Engram block stream. The twin's knockback = `decode` + `predict_all` spun **one beat past** your phrase — the "tiny flourish" is literally the model's own continuation of you. The save file, the score, and the art are one `.engram` packet stream. |
| **Body** | **Glyph eigenmotion** (already in `neuron-core/src/glyph.rs`) | How the twin moves and draws | Player-drawn shapes are `GlyphFit`s; their eigenvalues (λ damping, rotation, residual) become instrument *voices*. The twin draws its strokes by forward-running stored `(K,G)` oscillators — procedural drawing with the codec as the sole source. No sprite, no animation data, ever. |
| **Attention** | **Logos 0D** (port from `logos.wat` — new Rust module) | Novelty, sync, flow detection | Play events are quantized to a byte alphabet and fed through the lattice predictor. Per-byte surprise = novelty signal. **Sync = mutual compressibility**: when the twin's stream predicts yours and yours predicts the twin's, you are in flow. Stillpoints, Storms, and Drift timing all fall out of this one signal. |

Nothing in the emergent-systems list (Storms / Stillpoints / Drift / Hauntings / Sigil) is a
feature module. Each is a *rule over codec state* — a few lines reading surprise, energy, and
brain history. Keep it that way.

---

## 1. Where it lives in Neuron

Follow the established session-instrument pattern (whiteboard / dialweave / teleport: **prime →
capture stream → overlay → exit**).

**New files:**

| File | Crate | Contents |
|---|---|---|
| `crates/engram/` | vendored | Copy of the Engram Rust crate from `C:\Users\<user>\Downloads\super knowledge (unsorted)\engram\` (12 modules, ~4.1k lines, has `StreamEncoder`, `predict_all`, `brain_io`). Vendor it — the Downloads path is not a dependency root. Preserve author attribution ("Woflo / MB") and license header; it is the user's own patent-pending work, separate from Neuron's license. |
| `crates/neuron-core/src/logos.rs` | core | Rust port of the Logos 0D coder (see §3). Exposes `encode0d`/`decode0d` **and** the streaming `Surprise` probe. Doubles as the compressed-payload coder the `.gwyph` spec is waiting on (`docs/gwyph-spec.md` mode byte `0x00`). |
| `crates/neuron-core/src/rhythm.rs` | core | Pure rhythm engine: onset detection, motif extraction, quantization, mirror statistics. Zero platform code, fully unit-tested. |
| `crates/neuron-core/src/twin.rs` | core | The familiar: Engram-backed brain, knockback generation, emergent-rule evaluation, persistence. Pure logic + injected clock; fully simulatable headless. |
| `crates/neuron-app/src/knockback.rs` | app | Session orchestrator: beacon instrument, capture loop, state machine, overlay driving. Sibling of `whiteboard.rs`. |
| overlay additions | app | `WeaveMode::Twin { … }` + its renderer in `overlay.rs` (see §6). |
| UI additions | app | TWIN section inside `ui/panels/spellweaving.slint` (it is the cast engine's third instrument, not a new panel — the panel cull stands). |

**Entry points** (all three, none hardcoded — press-to-bind everywhere, per the standing UX rule):

1. `Action::Knockback` app intent (new variant in `action.rs`, routed like `Action::Whiteboard`).
2. A bindable activation rhythm slot in `cast.toml` (`mode_slots` already validates instrument
   rhythm coexistence — knockback joins teleport/whiteboard there).
3. A "TRY" prime button in the spellweaving panel (same as whiteboard's try button).

**Exit:** ESC, or idle session timeout (configurable, default never — the twin waits; only the
overlay dims). Exiting flushes the brain to disk.

---

## 2. Input grammar (the desk-drum)

In-session, the cast trigger button is the drumhead and the mouse body is the drum. Everything
is music; almost nothing is a command.

| Gesture | Meaning |
|---|---|
| **TAP** (trigger click) | A full drum hit. Onset time + click duration = the note. |
| **Wiggle** (motion impulse without click) | A ghost note — soft hit. Motion-energy spike above a small threshold = onset; energy = velocity integral of the impulse. Fidgeting *is* playing. |
| **HOLD + draw** | Carve an **instrument voice**: the stroke's `GlyphFit` eigenvalues become a timbre. λ rotation → pulse-ring spin direction & hue-shift; damping → decay length of the ring; residual → shimmer/grain. The voice colors your subsequent hits until you carve another. This is the EIGEN-RHYTHM verb: drawn shapes literally become how your beats look and feel. The twin learns voices too — it answers a circle-voice phrase with an *almost*-circle of its own. |
| **Flick at a drifting glyph** | Catch (Drift only — a committed straight flick toward it, reuse radial straightness ≥ 0.85). |
| **ESC** | Leave. The weave stays in the brain. |

No tap=undo / tap-tap=menu overloading — taps are notes. The session must never punish a tap.

Onset pipeline: `glyph::capture_held_with` cannot be the loop (it requires a held trigger); the
session instead runs the raw-input capture continuously (the same `raw_input` stream the
whiteboard rides), reading points + `take_click_edges()` each tick. `rhythm.rs` turns
(timestamped points, click edges) into an **onset stream**: `Onset { t_ms, energy, kind:
Tap|Ghost, voice }`. Real timestamps come from an injected clock trait so tests drive virtual
time.

---

## 3. The Logos port (attention organ)

Source of truth: `super knowledge (unsorted)/whisper/live-wasm-logos.ts` + `logos.wat`
(7,252-byte hand-written WASM — small enough to port faithfully). Port the **complete coder**
(8 correlated axes + M/A injection + Born-weight blending + thermodynamic evaporation + range
coder), not a toy approximation — it is also the production `.gwyph` payload coder, so
round-trip fidelity against the TS implementation matters.

Additions beyond the TS API:

```rust
pub struct LogosStream { /* predictor state, no range coder */ }
impl LogosStream {
    pub fn surprise(&mut self, byte: u8) -> f32; // -log2 p(byte) under the blended model, then update
}
```

This is the one new capability KNOCKBACK needs: the predictor *without* the arithmetic coder,
exposing per-byte information content. (The TS wrapper never exposed this; the axes already
compute it internally.)

**Byte alphabet** (one byte per onset, defined in `rhythm.rs`):
`[3 bits log-IOI bucket | 2 bits energy quartile | 3 bits voice class]` — IOI = inter-onset
interval. Player onsets and twin onsets feed **four** `LogosStream`s: player-self, twin-self,
player-predicting-twin, twin-predicting-player. From these four surprise traces derive:

- `novelty` — player-self surprise (are you doing something new?)
- `sync` — 1 − normalized mean of the two cross-surprises (do you predict each other?)
- `heat` — EMA of onset density × energy (how hot are you running?)

**Validation:** round-trip byte-exactness against vectors generated from the TS implementation
(`live-wasm-logos.ts` `runStressTests` distributions: Zipf, random, UTF-8, structured,
residuals). Generate the vectors once with a small Node script checked into `docs/vectors/`,
then the Rust tests are hermetic. When this lands, also flip the `.gwyph` writer in
`whiteboard.rs` from mode `0xFF` to `0x00` — same coder, two products.

---

## 4. The rhythm engine (`rhythm.rs`, pure)

- **Onset detection** — click edges are exact; ghost notes from motion-energy peaks (velocity
  magnitude over a short window, hysteresis so a single wiggle = one onset, not five).
- **Motif extraction** — a motif closes when a gap > `phrase_gap_ms` (default ~700ms, attuned
  to the player's own median IOI over time — the player defines their own grammar). Motif =
  `Vec<Onset>` normalized to its own tempo (IOIs as ratios of the motif's median IOI) so the
  twin learns *shape*, not absolute speed.
- **Embedding for Engram** — each motif becomes a fixed-D trajectory: sample the motif's
  rhythm envelope (onset impulses convolved with the voice's decay kernel) at D/2 complex
  points: real = envelope amplitude, imag = instantaneous tempo deviation. D = 16 (8 complex
  pairs) is plenty — the Engram crate takes any even D; do not cargo-cult 256.
- **Mirror statistics** — trailing reflection of demonstrated peak: EMA pairs (fast attack,
  slow decay) over tempo, density, motif length. The twin's output is *clamped to ≤ these* —
  it can never demand more than you have shown. Slowing down lowers the ceiling within a few
  exchanges. No difficulty variable exists anywhere in the code.

All of it deterministic, all of it tested with synthesized onset streams (see §9).

---

## 5. The twin (`twin.rs`)

**Brain = an Engram packet stream** (`runtime/twin.engram` via `brain_io`), one block per
completed exchange, plus a tiny `runtime/twin.toml` (attunement: thresholds, palette depth,
bound voice classes — config, never state that belongs in the brain).

**KNOCKBACK generation** (the heart — keep it this simple):
1. Take the player's motif embedding, `fit_all` → `(K, G)` per pair.
2. `predict_all` from the motif's last two samples, length = motif length **+ 1..2 beats** —
   the continuation past your data is the flourish. It is familiar (your physics) but new
   (you never played it).
3. Convert the predicted envelope back to onsets (peak-pick), render with the twin's voice =
   a slightly perturbed copy of your current voice's eigenvalues (deterministic per-exchange
   PRNG seeded from block index — overlay already uses fixed seeds; no wall-clock, no
   `rand::thread_rng`).
4. Quantization residual from the encode = micro-timing humanization. Free, honest, his.

**Answer judging:** after the knockback, the player's next motif is compared by the same
eigen-distance used in gesture recognition (`word_distance` family) — close = harmony, far but
*self-consistent* (low player-self surprise) = counterpoint. Both valid, both encoded into the
brain. Only an *unanswered* knockback leaves the gap-slot glowing (the itch). There is no fail.

**Emergent rules** (each is a few lines over §3 signals — resist making them subsystems):
- **Storm**: `heat` high AND player-self surprise low for N exchanges → braid the player's 3
  highest-energy brain motifs (decode them) into one chained phrase. Survival (answer within
  tolerance) permanently increments `palette_depth` in twin.toml.
- **Stillpoint**: `sync` above threshold for M consecutive exchanges → time dilation: overlay
  animation clock divides, lighting breathes at the shared tempo, twin density *holds* (never
  escalates out of flow). Ends silently when sync drops.
- **Drift**: in-session idle > the player's own historical idle median → a stored oscillator
  is spun across the screen periphery as a slow glyph; a committed flick catches it (one extra
  braid thread). No background process — Drift exists only inside an open session.
- **Haunting**: rare (deterministic schedule from brain length), decode an old block — early
  blocks preferred — and replay it in faded light. No input expected.
- **Sigil**: the long-run distribution of the player's `(K,G)` cloud (running moments stored in
  twin.toml) seeds a multi-oscillator forward-run drawing — *the* personal glyph. Rendered in
  the panel; exported as SVG to `runtime/sigil.svg`.

---

## 6. The look (`WeaveMode::Twin`) — AS BUILT: the hard-light stage

Same overlay machinery (layered, click-through, software rasterizer, fixed seeds), in
Neuron's school of magic: **hard light** — Symmetra's sorcery. Nothing is a soft puff;
everything is a *constructed* thing — faceted, crystalline, edge-lit, with a prismatic
cyan/magenta refraction fringe where the light bends. (The same grammar reskins the
spellweaving rune ring, and `neuron::scene` renders it as SVG for export + golden tests.)

The session is **one fixed stage**, anchored where the cursor was at entry (pinned; it never
wanders). Every state of the loop is visible on it:

- **Your strikes appear the instant you press** (the down edge — a drum answers when the
  stick lands). Each is a phosphor hard-light **construct**: facet count from strike force
  (soft tap = triangle, heavy knock = octagon), built left→right on a staff beam where
  **time = distance** at a fixed scale — the geometry IS your rhythm. Wiggle ghost-notes are
  smaller constructs riding just under the beam.
- **The seal-arc**: after your last strike, a thin phosphor arc around it drains over the
  phrase gap. When it empties, your phrase commits — the phrase boundary, learnable without
  a single word.
- **The twin answers in your own tempo**: it takes the staff and rebuilds your rhythm in
  cold moonlight-violet **exactly where your constructs stood** (the mirror, made visible),
  then extends past your last beat — the flourish, larger, warmer, +1 facet. The one
  sanctioned second hue makes *who-is-who* instant.
- **The reply ends in a BLUEPRINT** — an unbuilt dashed wireframe construct, pulsing exactly
  one beat-width past the last violet construct: the gap sits *where the next beat would
  fall in time*. That gap IS the tutorial; there is no other. **Your answering strike
  materializes it** — wireframe flashes into built light with a commitment ring — and your
  next phrase begins.
- **Judgment is colour, not text**: harmony washes the stage hush-green; counterpoint washes
  warm violet; a storm runs amber for its whole braid; a stillpoint dilates the stage's
  visual clocks (the breathing slows — the music never does) under a held hush.
- **The weave strip**: one crystal shard per exchange along the stage's lower edge, hue from
  the exchange's outcome, rotated by earned palette depth — score, save file, art, in view.
- **The familiar itself** stands at the staff's head: a small hexagonal construct that
  materializes (wireframe → built) on entry, breathes while alive, and dims when unfed.
  Deep idle summons a **haunting**: an old phrase replayed in faded grey-violet.
- **Words, exactly two, both temporary**: an entry caption ("drum ⟨button⟩ · wiggle plays
  too · esc leaves") until your first strike, and "answer it — finish the line" under the
  session's first blueprint. After that the stage is wordless.
- A strike during the twin's reply **yields it** (the reply completes instantly and your
  answer begins) — the mirror never pushes, even visually. Input is never blocked by
  animation; the session is a single non-blocking state machine.

First-session beat: the session opens listening. Two taps inside one phrase window → the twin
answers within one beat-interval. Time-to-first-knockback must be under 20 seconds of natural
fidgeting — this is an acceptance test (§9), not a hope.

**Sound-optional, screen-first.** No hardware takeover. KNOCKBACK never touches the user's
device lighting, never grabs the mouse for rumble, never reconfigures the desk — the user has
been explicit that the app must not commandeer hardware. The entire feedback channel is the
on-screen overlay: every beat, every answer, every emergent state is *seen*. The visual
language below carries the whole game. (Optional UI sound may be added later as a pure software
output; it is never required and never default.)

## 8. UI (spellweaving panel, TWIN section)

Knockback is the cast engine's third instrument; it gets a section, not a panel: bound rhythm
(press-to-bind), TRY button, live brain readout in the precision-instrument voice (blocks,
exchanges, sync EMA, palette depth — real numbers, monospace, no gamified bars), the sigil
render + EXPORT, and a single attunement: twin presence (glow intensity ceiling). The weave
itself is the game's UI; the panel just proves the machine, like everything else in Neuron.

---

## 9. Testability (creative, no LLM-eyeball)

- **Codec fidelity**: Logos round-trip vs TS-generated vectors (§3); Engram round-trip via the
  vendored crate's own tests plus a motif→encode→decode→onset-equality test.
- **Simulated players** (the centerpiece): a headless harness in `twin.rs` tests driving the
  full session loop on virtual time with scripted humans — `Chill` (sparse, soft, jittery),
  `Tryhard` (dense, accelerating), `Fader` (tryhard → chill mid-session), `Metronome` (perfect
  sync). Assert: mirror never exceeds demonstrated peak; Fader's ceiling decays within k
  exchanges; Metronome reaches a Stillpoint and density *holds*; Tryhard eventually triggers a
  Storm; Chill never sees one. Determinism: same script + seed → byte-identical brain.
- **First-knockback latency**: simulated natural fidget → twin answers < 20s virtual.
- **Overlay**: golden-frame snapshots of the rasterizer (it is deterministic by construction —
  fixed seeds, frame-counter clock) for ring/braid/gap-slot rendering.
- **Slint UI tests** for the TWIN section (bind, try, export) alongside the existing 24.

## 10. Build order (dependency order of one complete design)

1. **Vendor Engram** (`crates/engram`) + wire into the workspace; its tests green.
2. **Logos port** (`core/logos.rs`) + `LogosStream::surprise` + TS-vector fidelity tests.
   Flip the `.gwyph` writer to mode `0x00` while there.
3. **Rhythm engine** (`core/rhythm.rs`): onsets, motifs, embedding, byte alphabet, mirror
   stats. Pure, injected clock, exhaustively unit-tested.
4. **Twin** (`core/twin.rs`): brain, knockback generation, judging, emergent rules, sigil
   moments, persistence. Simulated-player harness green before any UI exists.
5. **Overlay**: `WeaveMode::Twin` renderer (rings, twin ink, braid, gap slot, drift glyphs,
   haunting fade, stillpoint dilation). Golden frames. Screen is the entire feedback channel.
6. **Session orchestration** (`app/knockback.rs`): beacon instrument, `Action::Knockback`,
   cast rhythm slot, capture loop, state machine, exit/flush. Wire panel section + UI tests.
7. **Attune on real play**: thresholds in twin.toml from live sessions; README + this doc
   updated to as-built; `neuron` CLI gets `neuron twin` (brain stats + sigil export) so the
   engine is provable headless, like every other subsystem.

**Constraints that override convenience** (standing project rules — do not relitigate):
press-to-bind, never hardcode a VK · no background watchers; everything lives inside an open
session · **never commandeer hardware — no device lighting, no rumble, no desk reconfiguration;
the screen overlay is the whole feedback channel** · deterministic rendering (no
wall-clock/thread_rng in render or twin decisions) · the void/precision-instrument aesthetic;
if it looks like a casual-game skin, it is wrong · panels must be warranted: TWIN is a section
of spellweaving · Whisper sources are the user's patent-pending work — vendor with attribution,
keep license separation.

---

## Appendix: the design brief (the contract)

> *(Condensed; tone and intent are binding.)*

Every human finishes "shave-and-a-haircut." Call-and-response is the whole game: one verb,
zero tutorial. Your first idle taps summon a spectral familiar with no rhythm of its own —
your fidget is its heartbeat. **KNOCK → KNOCKBACK → ANSWER → WEAVE.** It claps your motif back
with one tiny flourish; you finish its phrase (match = harmony, twist = counterpoint — both
valid, both teach it); every exchange braids a thread into a living tapestry. No fail state
for absence — the glow only dims, asking gently to be fed. The twin's intensity is a trailing
reflection of your demonstrated peak, never a target: the only way to make the game hard is to
*be* hard. Nothing is a mode: Storms are earned heat, Stillpoints reward flow with *calm* (time
dilates — players will tryhard to relax), Drift turns idleness into fireflies, Hauntings replay
your clumsiest old fidget in faded light, and the Sigil is a glyph no other human could
generate because no other human fidgets like you. Magic, not UI; screen-first, sound-optional;
quiet by default. The make-or-break beat: the first knockback must land within ~20 seconds of
idle fidgeting and feel uncanny — *that's my rhythm… but it answered.*
>
> **One-line pitch:** you drum on your mouse the way you always have. This time, something
> drums back.
