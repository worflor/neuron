> **kind:** feature design + implementation brief — the Wildlife lighting pattern
> (`life`).
>
> **status:** IMPLEMENTED in `neuron-core`, the pattern registry, preset catalog and CLI docs.
> The automated software checks pass. The visual result remains unverified on real
> hardware; this brief records the intended behavior and test contract, not a hardware grade.
>
> **provenance:** drafted with LLM assistance (October 2026), then red-teamed
> against the live sources and independently probed (Life mathematics, input
> sampling limits, activity-threshold arithmetic, calendar arithmetic). Every
> source claim was re-verified against the tree before being written down;
> line references are design-time anchors from commit `60f354f`, not a maintained
> contract. Numerical art-direction values are TUNING CANDIDATES, not
> measurements of a finished effect. The effect is aesthetic: only a run on real
> hardware grades it (see `AGENTS.md`).

# Wildlife — make the keyboard feel inhabited

## Implementation decisions (October 2026)

These refine the brief below and take precedence where it leaves an implementation
choice open. Keep the terrarium, the Enter departure and the quiet seasons; do
not add user controls or a second lighting engine.

- Run transport, envelopes, ecology and weather decisions on a fixed simulation
  tick (start at 1/60 s), with generation accumulation inside it. Rendering
  samples that state; it never draws random decisions. This makes the 6/15/30/60
  fps requirement meaningful. A long pause rebases at the current season, expires
  transients and does bounded recovery rather than replaying missed spectacles.
- Keep an exact read-only visible-region hint on the Pattern seam, defaulting to
  the full bounds for existing patterns. Use it for eligible contact, establishment
  and departure runway; preserve the rectangular logical Conway neighbourhood.
  A patterned mask is not permission to seed an invisible colony to satisfy recovery.
- Initial population uses a few recognizable motifs with staggered maturity.
  Contact stays crisp and short; substrate and ghost light stay subordinate.
  Birth/death envelopes never interpolate the binary occupancy itself.
- Read-side observations contain identity, monotonic time and sequence. Attach at
  head, recover explicitly from overflow and keep readers independent. Feed only
  already-computed physical edges; no interception, injection, dispatch change or
  additional Raw Input registration. Polling remains a conservative fallback with
  honest limits.
- Enter owns its event before generic harvest and gust processing. A departure
  is admitted only if its complete motif and a useful visible runway fit; failed
  placement resolves as a local flutter. It never clears a corridor through life.
- One weather scene and one bounded delayed star wake. Swept effects have per-cell
  admission bits and mortality/area budgets captured at scene start. No queued
  storms; no per-frame probability and no endless recovery reset.
- Keep speed/density as the only parameters. Speed affects biology and ambient
  motion, never the shared 64 s calendar or physical-input thresholds. Density
  affects establishment pressure rather than immediate touch visibility.
- Tests include canonical independent B3/S23 fixtures, identical-time idempotence,
  cadence equivalence, mask/tiny-grid behaviour, event eligibility, quantized RGB
  contrast and several long seeded traces. A green suite still does not grade the
  real keyboard's appearance. Hardware tuning is a separate final review.


## 0. The idea, in one paragraph

A small world lives under your hands. It runs itself: seasons turn, a warm sun
wanders, seeds establish, colonies live and die of old age, and every so often a
front sweeps through and renews the whole crop. You are not its gardener — you
are its weather. When you type, the ground under your fingers gets richer, life
you touch reacts, and a genuine burst of typing raises a small local storm. When
you stop, it keeps going without you, because it was never running for you.

The best version of this is not constantly proving that it is alive. It is alive
enough that you occasionally catch it doing something.

## 1. The constitution

These are the rules above the implementation. An implementation that violates
one of them is wrong even if it looks good on screen.

1. **Your touch is an event. The world has weather. Both change the same
   habitat.** Every eligible, observed press gets immediate, local, spatially
   exact feedback. The ecological consequences follow consistent rules.
   Autonomous conditions continue while you type; large unsummoned spectacles
   simply become less prominent around active hands.
2. **Inhabitants, not activity.** A three-cell oscillator you can recognise for
   a few seconds beats twenty continually reseeded pixels. A departure you can
   follow beats five travelers colliding on frame one. A quiet winter with one
   living pocket beats a governor nervously filling every dark patch.
3. **Contact is immediate; life is discrete; weather is continuous.** A press is
   acknowledged on the next output frame — never after waiting for a generation.
   Births and deaths happen at generation boundaries. Sunlight, nutrient
   transport, ghost decay and front movement evolve in between. "Immediate"
   means the next feasible hardware frame, not a promised sub-frame latency.
4. **The action is guaranteed; the outcome is not scripted.** A release event
   visibly leaves the source. It need not produce a long-distance survivor. A
   press on dead ground leaves a local mark; it need not force a birth. A
   predation press visibly disturbs the living cell and leaves the habitat able
   to recover. We author the gesture; the ecology owns the consequence.
5. **Ecology has priority over spectacle.** At most one large weather feature
   dominates a small board at a time. Contact accents are never delayed behind
   it. A star waits for a quiet opportunity. A second squall enriches the current
   wake instead of starting another front. A scheduled renewal defers when the
   board was just disturbed. This is scene composition, not feature removal.
6. **One world, not a bag of writes.** One live population, one nutrient field,
   one death-memory field. Contact, weather and ecology modify them through a
   small, fixed set of operations — never by every producer mutating arrays
   whenever it feels like it. See §12.4.
7. **Colour is not the only carrier of meaning.** Life must stay legible with a
   single-colour spectrum. Occupancy, local contrast, temporal envelopes and
   shape carry the meaning; the default palette only improves it.
8. **Honest input semantics.** We are a state sampler, not an event log, unless
   the read-side observation surface in §6.1 exists. Never market a guarantee
   the available input cannot uphold.

### 1.1 What this is not

Not a stress test, not a screensaver, not a "look how many pixels I light". A
terrarium. The board is a habitat with inhabitants you can learn, and the
occasional spectacle is rare *because* the quiet is confident.

## 2. What the tree actually gives us

Verified against the tree at design time. `pattern.rs` is
`crates/neuron-core/src/pattern.rs`.

### 2.1 Input reality

- `capture::key_down` (`capture.rs:126`) reads the **current** high bit of
  `GetAsyncKeyState`. It is a sampler. A complete down/up tap between two reads
  is invisible. Microsoft's "pressed since last call" low bit is documented as
  unreliable, so we do not use it.
- `scan_key_presses` (`pattern.rs:1136`) diffs a per-instance `prev[]` array
  against that current state. It produces down-EDGES with no timestamp, no
  device identity, no scan code, and no chronological order — the VK loop runs
  in ascending key-code order. Macro keys (M1..M6) are bridged afterwards from
  the shared `MACRO_HELD` mask (`capture.rs:149`), so each consumer detects its
  own edges off shared stateless held-state.
- `capture::key_transition_generation` (`capture.rs:200`) is an AGGREGATE
  counter bumped by `note_key_transition()`. It tells a consumer that
  *something* changed; it does not say which key. It cannot be used to attribute
  a press to a key after the fact.
- The Raw Input pump in `controls.rs` *does* have identity: it builds a held-set
  and already computes a `changed` flag before calling `note_key_transition()`
  (`controls.rs:1030`–`1072`). The edge diff it has already computed is the
  natural feed for a bounded observation ring (§6.1).
- `capture::suppress_key_reads` (`capture.rs:116`) makes every live read report
  UP on the calling thread. The tile grid uses it to skip ~765 syscalls per
  refresh in the reactive thumbnails. Any new observation surface must honour
  the same rule for its readers.
- `count_unmapped = true` counts keys that have no visible cell (generic
  modifiers, mouse buttons, media keys). Fine for Thermal's "how fast do I
  type" impression, NOT fine as a calibrated activity signal (§6.4).
- A held key is ONE down-edge, not an auto-repeat stream. This is a gift: it
  makes "held Backspace becomes a chainsaw" and "held Enter streams travelers"
  structurally impossible, as long as we do not synthesise repeats ourselves.

### 2.2 Reuse ledger — what each primitive actually does

| primitive | real behaviour | verdict for Wildlife |
|---|---|---|
| `StepClock` (`pattern.rs:1344`) | steps ONCE when `dt == 0` (ten `field(t=0)` calls = ten generations); caps catch-up at 8 | **Do not reuse verbatim.** Write a Wildlife-local accumulator whose `dt == 0` is a no-op. Do not change `StepClock` globally — Rain, Comet and Heat depend on it. |
| `step_rate` (`pattern.rs:2286`) | clamped leaky envelope: `+0.09/press`, release `exp(-0.5·fade·dt)`, saturates at ordinary typing rates at default `fade` | Reuse only for small expressive contact energy. **Never** as the gust/squall classifier: it is a display quantity in palette-dependent units. |
| `deposit_heat` (`pattern.rs:2194`) | radial splat, RADIUS 1.8, quadratic falloff, clamps at 1.6 | Extract the *shape* (splat kernel); re-unit it for nutrients. |
| `cool_field` (`pattern.rs:2222`) | `-(3·T³ + 0.22)·fade` — hot cells shed disproportionately fast | Flash envelopes only. A nutrient reservoir wants a chosen half-life, not a T³ radiative law. |
| `diffuse_field` (`pattern.rs:2244`) | heat-conserving 4-neighbour conduction + permanent UPWARD buoyancy | Do not lift the buoyancy. Nutrients get slow ISOTROPIC diffusion; vertical drift is an explicit, temporary wind impulse (§8.4), not a permanent property. |
| `Comet::paint_burst` | private method mutating Comet's own burst field, radius 2.2 | Extract a tiny PURE radial-shaping helper (`ghost_splat(field, x, y, peak, radius)`); do not instantiate Comet. |
| `Ring` wave sampling | samples a visual Gaussian ring at `t` | Reuse the shape. The front's ecological consequences use swept-region integration (§12.5) — a sampled ring is not a collision detector. |
| `heat_shimmer` (`pattern.rs:2307`) | two phase-seeded sines, depth ramps with temperature | Low-amplitude accents only. Do not pulse every organism in unison. |
| `breathe_shape` | global breath envelope | One gentle board-wide breath, not stacked with spring dawn. |
| `xorshift` | deterministic PRNG | Use two independent streams (weather, immigration) so render cadence cannot reorder decisions. |
| `scan_key_presses` | state-sampler edge detection | Polling fallback only, with best-effort honesty (§6.2). |

### 2.3 The render clock, and why the year is 64 seconds

`render_elapsed()` (`pattern.rs:726`) returns the process epoch's elapsed
seconds **wrapped at 4096 s**, folded in integer milliseconds so f32 precision
survives. Every animated surface — the spectrum sampler, the compositor, the
device stream and our pattern — reads that same wrapped `t`. `quantized_t`
(`pattern.rs:712`) then floors it to whole frames at the surface's fps.

Consequence: any calendar expressed as a spectrum frame sequence is periodic
against a 4096 s clock. `4096 % 60 = 16`, so a 60-second year takes a visible
sixteen-second season-phase skip once every ~68 minutes. `4096 % 64 = 0` — a
**64-second year never skips**.

Therefore:

- The year is **64 s**: four seasons of 16 s, each a spectrum frame of
  `hold: 13.0, fade: 3.0`. The fade is the transition INTO the frame, so the
  year is exactly `4 × (13 + 3) = 64.0` s, seamless at any uptime.
- The pattern's ecological season weights read the SAME wrapped `t` and the SAME
  frame boundaries as the spectrum, so palette and biology cannot desynchronise.
- The `speed` knob scales generation rate and weather motion only. It must never
  scale the calendar.
- On resume after a long pause, enter at the CURRENT season and continue. No
  backlog of missed spring pulses, stars or squalls.

### 2.4 Output: quantization and blend

- `Rgb` is 8-bit per channel and `scale_f` ROUNDS after scaling
  (`lighting.rs:51`). A faint float substrate that only exists in a debug view
  is a bug, not a subtlety. Tests must inspect final bytes.
- `Blend::Normal` is `over`, i.e. opaque (`effects.rs:99`–`101`). Black inside
  a region is NOT transparency. Wildlife is a complete base look and is fine
  under Normal; layering is the user's composition choice, and Wildlife must
  not try to teach itself about layers beneath it.
- `Cell` carries ONE `u` and ONE `intensity` (`pattern.rs:44`). There is no
  per-cell RGBA. A cell with several contributors must choose a dominant one —
  see §10.

## 3. The world model

### 3.1 The logical lattice and its visible projection

- The **habitat** is the bounded rectangle the layer occupies — the same
  rectangle `set_bounds`/`Bounds` already hands patterns (`pattern.rs:104`).
- The **logical lattice** is that rectangle's cells, row-major. Physical LED
  gaps (wide keys spanning several lattice positions) stay logical ground: we do
  NOT invent an irregular-neighbourhood automaton for them.
- The **visible projection** is the subset the user can see — after a region
  mask is applied by the compositor. Visible density metrics, acknowledgement
  eligibility and traveler-legibility scoring use the visible projection.
  A governor counting rectangle cells can be satisfied by hidden organisms; a
  traveler "dying" in a masked hole reads as a bug.
- If an exact-region hint is ever needed (a small defaulted, read-only
  accessor), add it then, under its own change. Do not pretend the bounding box
  is the whole truth today.

### 3.2 Edges

Finite habitat, dead outside. No wrap. A six-row keyboard is a shallow coast,
not a torus; organisms that reach an edge settle, break up, or leave. The
long-axis traveler (§7.4) is how a departure crosses the board honestly.

### 3.3 Fields

| field | type | updated | purpose |
|---|---|---|---|
| `live` / `next_live` | `Vec<bool>` ×2 | generation | the Conway population, double-buffered |
| `age` | `Vec<u8>` | generation | consecutive live generations, drives maturity colour and senescence |
| `lifespan` | `Vec<u8>` | at birth | drawn ONCE per cell from the immigration stream (26..=38 gens nominal 30) — never re-rolled per frame |
| `nutrient` | `Vec<f32>` | frame (dt-scaled) | spatial memory of recent contact; habitat suitability (§5) |
| `scratch` | `Vec<f32>` | frame | nutrient transport accumulator, reused, no per-frame alloc |
| `ghost` | `Vec<f32>` | frame | death memory: embers, harvest flares, front passages, the star's wake |
| `contact` | `Vec<f32>` | frame | SHORT-LIVED, spatially anchored press accents. Not advected by wind. Not a second ecosystem — the bookkeeping that makes a press visible under a moving ghost field. |
| `scene` | at most ONE struct | — | the dominant feature (renewal passage, squall, star), or none |
| `pending_wake` | bounded list | — | the star's delayed establishment opportunity (§8.6) |
| `recurrence` | ring of occupancy bitsets | generation | exact-repeat detection (§4.4) |
| governor signals | scalars | frame | occupancy EMA, congestion, intervention recency, cooldowns |
| activity stats | scalars | frame | fast/slow rates, burst strength, squall state machine (§6.3) |
| sun | waypoint pair + phase | frame | smooth interpolated wander |
| calendar | `t`, season weights | frame | 64 s year (§2.3) |
| `prev[]` | `Vec<bool>` × KEY_SCAN_SLOTS | poll | **polling fallback only** — ring-fed paths don't need it |

## 4. The cellular kernel

### 4.1 Conway at the centre

```
alive_next = (n == 3) || (alive_now && n == 2)        // Moore neighbourhood, 8 cells
```

Synchronous, double-buffered, no in-place updates. Scan order must never be
part of the physics. Outside the habitat counts as dead.

This is Conway **derived**, not pristine Conway, because the operations in §4.2
–§4.4 act on it. Say so honestly wherever the effect is described.

### 4.2 Births do not get a probability

Do NOT turn ordinary B3 births into coin flips. A glider depends on particular
births occurring; withdrawing them unpredictably turns recognisable organisms
into unreliable sparkles and makes a winter traveler release look broken. Any
proposed "birth bias" is therefore interpreted as **immigration opportunity and
site suitability** (§4.3), never as a coin-flip on the truth of an ordinary
birth.

### 4.3 Immigration, senescence, disturbance (the external operations)

Three DISTINCT operations, because conflating them is how a recovery guarantee
turns into a lie:

| operation | what it does | when |
|---|---|---|
| **Nutrient deposition** | `feed(cell, amount, footprint)` — leaves spatial memory, changes future suitability | every contact, every death's bounded share |
| **Viable immigration** | `seed_motif(origin, orientation, template)` — places a VALID small motif into the same population | governor escalation, traveler/gust releases, the star's delayed wake |
| **Ephemeral winter seed** | one isolated birth, allowed to twinkle and die | winter, sparse, for the feel of dormancy |

**Senescence.** Surviving cells increment `age`; newborns start at 1; a cell
whose `age` reaches its drawn `lifespan` dies in a ghost flare scaled by
normalised maturity. The sun grants a modest +1 ageing increment (life is
faster and shorter in the light). Nutrient richness slightly EXTENDS tolerance
— it never cancels senescence. Lifespans are drawn at birth from the
immigration stream, so a freshly seeded cluster doesn't die in lockstep.

### 4.4 Recurrence is separate from senescence

An age cap is texture, not a proof of novelty: a glider survives indefinitely
because no individual position stays live for more than four generations, and
periodic oscillators can beat any consecutive-age cap entirely. So the governor
carries its own recurrence signal: a short ring of exact occupancy bitsets
(hashed for indexing, but EXACTLY compared before a repeat is declared), with a
grace period of roughly 8 s. A detected repeat does not wipe your pet — it opens
a window for a nearby gentle immigration opportunity, at most once per repeat
episode.

### 4.5 Small surfaces

The pattern API accepts anything from a 1×22 strip to a full board. On surfaces
too small for recognisable 2-D motifs (fewer than ~3 usable rows or columns),
keep the habitat's language — local feeding, twinkles, seasons, ghost decay,
bounded disturbance — and make no Conway claims. No new one-dimensional ALIFE
engine; the same pattern degrades honestly. Physical holes in a region are
windows over logical ground, not a different automaton.

## 5. Nutrients

Nutrient is the substrate that lets your typing show up as a slowly changing
map of where you actually work. It is not a life-support system — the world
survives without you — and it is not a blanket: a saturated field is a bug.

Three jobs, all modest:

1. **Site suitability** — biases where viable immigration establishes.
2. **Local maturity** — slightly improves brightness and senescence tolerance
   of nearby life.
3. **Memory** — persists long enough that the shape of a recent sentence
   influences what grows afterwards. Half-life: **~12 s** first-order.

Physics:

- Decay: first-order exponential to the chosen half-life. NOT `cool_field`'s
  T³ law (that would make rich deposits vanish proportionally faster and invert
  the intent). Clamped, dt-scaled; `dt == 0` is a no-op.
- Diffusion: slow ISOTROPIC 4-neighbour conductance, small coefficient. NO
  permanent upward buoyancy (that is `diffuse_field`'s thermal identity, and it
  would pool every nutrient at the top of the board).
- Wind: an explicit, finite, one-shot directional impulse, used by the squall
  (§7.6) and the autumn transport (§8.4). Never a standing bias.
- Births may draw a small amount (order 0.05) from local nutrient; ordinary
  births are NEVER blocked by an empty reservoir. The budget describes habitat
  richness, not a conserved energy model.
- Live cells metabolise a tiny drain per generation (order 0.05) — gardens
  visibly darken the ground they grow in.
- **Recycling is bounded.** A death converts `normalised_maturity × fraction`
  (start at ≤ 0.25) into nutrient at the death site. Otherwise typing creates a
  positive-feedback machine: feed → births → death scatter → more feed →
  saturation. Every death is processed exactly once, ever (§12.5).

## 6. Input observation and activity estimation

### 6.1 The observation ring — the one justified core extension

The activity windows in §6.3 need timestamps and key identity. The current
sampler has neither. The narrow, honest fix is a **bounded, non-consuming
observation ring** fed by the input surfaces that already know what changed:

- The Raw Input pump's held-set diff in `controls.rs` (where `changed` is
  computed before `note_key_transition()`), pushing `(page, usage, pid, down)`
  per edge.
- `set_macro_held`'s change detection for M1..M6 edges.

Required properties (each is a test, not a hope):

1. **Non-consuming.** Multiple consumers observe the same edge; the device
   stream and the GUI hero preview both see it. A consume-once queue is
   explicitly rejected by the existing design (`capture.rs:146`).
2. **Bounded.** A fixed-capacity ring (order 256–512 entries). Overflow drops
   the OLDEST and bumps an explicit drop counter, so a late consumer can see
   "some were missed" instead of being handed forged history.
3. **Attach at head.** A new consumer starts at the current tail and never
   replays a backlog — a resumed effect does not inherit a storm.
4. **Suppression-aware.** A reader on a `suppress_key_reads` thread sees nothing
   (same rule as `key_down`): thumbnails never pay for, or react to, input.
5. **No synthesis, no dispatch, no arm-gate interaction.** This is read-only
   observation of already-observed events.
6. **Keyboard-only eligibility** is applied at the consumer (§6.4).

Scope: `capture.rs` plus the one pump call site. **This touches the input path
and needs the owner's sign-off before implementation** (AGENTS.md: HID-adjacent
changes are `risky`). It is not input interception, injection, or binding
capture — but the gate applies to intent, not to my reading of the boundary.

An equivalent counter-plus-stamp design is acceptable if it demonstrably
preserves counts, per-event identity, timestamps sufficient for the windows,
and dual-consumer observation. A single aggregate counter is NOT equivalent.

### 6.2 Polling fallback (what we may honestly claim without the ring)

Where no live pump feeds the ring (CLI one-shots, some transports, Linux), the
existing sampler semantics apply: a batch of down-edges at receipt, no ordering
guarantee, possible loss of complete taps between samples. Under fallback:

- Every observed press still gets immediate local feedback.
- Multi-press batches use the batch CENTROID as the origin.
- Gust/squall classification is conservative (they need absolute counts and
  windows; absent timestamps they may under-trigger).
- No documentation, blurb, or PR may claim per-key chronological accuracy here.

### 6.3 The activity estimator

Small, local to lighting, in PHYSICAL downs per second. No profiling, no
telemetry, no persistence beyond the session, no "focus score", no emotion or
productivity inference, no visible meter.

```
fast = EMA over ~0.30 s      (response)
slow = EMA over ~1.80 s      (baseline)
burst = max(0, fast - slow)
```

Integrated from event timestamps (ring path) or fixed-time bins (fallback). Not
normalised against a hard ceiling before classification — the classifier works
in absolute downs/sec.

**Gust policy** (the satisfying, event-driven one): require all of
- ≥ 4 eligible downs within ~0.35 s,
- a positive fast−slow rise (a lone first press against a zero baseline does
  NOT qualify),
- a ~2.5 s refractory since the last full gust release.

Origin = the burst's recent-source centroid, never "the last callback".

**Squall policy** (the earned weather): an explicit state machine.

```
Calm --(>= ~18 eligible downs/s held ~2.5 s)--> Charging
Charging --(sustained)--> Squall --(one-shot, bounded)--> Recovering
Recovering --(cooldown ~20-30 s, lower re-arm threshold)--> Calm
```

- Charge does NOT carry across a real pause.
- 18/s is a deliberately conservative candidate chosen to sit far above the
  ~6/s that saturates `step_rate` — not a claim to know the user's intent.
  A very fast typist may cross it; a masher may be missed by the sampler. The
  disturbance must be acceptable in both cases, which is why it is bounded
  (§7.6).
- While charging, track the source continuously; at release, resolve a stable
  recent centroid.

**Rhythm tendency** (the invisible reward): steadiness over fixed-duration
windows (counts per equal window, or a robust interval statistic). Sustained
regular activity earns a modest deposit bonus, capped at ~+20%, with gradual
attack/release; silence relaxes the bonus smoothly. Uneven typing is not
punished, and thinking pauses are not "bad flow". No new public knob.

### 6.4 Eligibility — what counts as an event

- Count only ELIGIBLE physical down events: keyboard usages that resolve to a
  visible cell on this board, plus recognised macro-key edges (M1..M6) through
  the existing safe bridge.
- Exclude auto-repeat (a held key is one edge — the pump's held-set diff makes
  this structural), duplicate generic/specific modifier observations (generic
  modifier VKs are excluded from the visual map precisely because they coexist
  with left/right-specific variants, so counting them alongside would double a
  Shift), mouse buttons (`is_mouse_vk`), and any usage outside the intended
  visual scope.
- A held chord (WASD) contributes its initial downs only — never a continued
  rate. A long think-pause contributes nothing.
- The estimator and every classifier run on the ELIGIBLE stream.
  `scan_key_presses`'s `count_unmapped = true` counts non-visible keys because
  Thermal wants a "how fast do I type" impression; Wildlife must not inherit
  that by accident. Letting non-visible keyboard activity influence a
  NON-keyboard light surface is a separate deliberate fallback decision, never
  a side effect of a shared flag.
- Physical-device attribution (pid) is used only when actually available.
  Global VK state cannot honestly claim "this keyboard only".
- Off Windows the pump does not feed the ring → polling fallback → the
  conservative, best-effort path of §6.2.

**The activity estimator is a lighting internal.** It is not the dispatch
engine's activity, does not see text content, application identity, or which
document is focused, and never leaves the process.

## 7. Contact, rituals, and derived events

Classification happens ONCE per physical down-edge. No contact may trigger two
resolved interactions, and no path may run predation after a ritual.

### 7.1 Press on dead ground — rain, not a compulsory plant

A small crisp local tick at the exact source, then a bounded nutrient splat
(reusing the splat kernel's shape, re-united; asymmetric only in the direction
of any active wind). The tick stays visible over existing ghost light and in
winter: it is the guaranteed acknowledgement, so it outranks every ambient
contributor at that cell. Most presses do NOT produce a birth. The pleasure is
seeing something establish where you were working a moment later.

### 7.2 Press on a living cell — harvest, not hostility

A compact contact flare, one death-memory contribution, and bounded fertile
scatter (§5). One cell, not a colony. The keyboard should not feel like it
dislikes being used: high-contact ground becomes naturally less stable, and
regrowth is the ecology's job. Do not instantly repopulate the killed cell —
that reads as a kill-respawn strobe.

### 7.3 Backspace — pruning

A larger, still bounded destructive gesture: affect live cells within radius
≤ ~1.5 cells of its own cell, with distance falloff and a MATURITY preference
(pruning old growth first). The source always gets its accent, even over empty
ground. One classification per physical edge; a HELD backspace is one edge (no
auto-repeat chainsaw). Reach is judged on the actual keyboard near the
top-right corner, where a symmetric disk clips to a crescent.

### 7.4 Enter — a departure ritual

The best authored detail in the design; preserve it, with honest scope.

- Every physical Enter gets a distinctive local release accent at the source.
- Full traveler release has a ~3 s refractory. Inside it, another Enter folds
  energy into a local flutter + fertile trace rather than queueing more
  departures. (Enter is common in code, terminals, chat, forms and gaming; a
  rare key is not guaranteed by the key itself, so the refractory does the
  work, not rarity.)
- **Traveler choice, by geometry** (independent probes, §B.2):
  - On a wide, shallow board prefer a **horizontal lightweight spaceship**
    (moves 2 columns per 4 generations ≈ 1.5 cols/s at 3 gens/s; needs a
    5-row, 4-generation envelope; population alternates 9/12).
  - Use a **diagonal glider** (5 cells, 1 row + 1 col per 4 gens) for short
    departures and smaller release opportunities.
  - If neither fits a legal pocket, resolve as a small local seed flutter.
  - Never paste a malformed partial spaceship on the edge. Never steer,
    rebound, teleport, or protect a traveler from the user's next keypress.
- **Placement**: score a small set of candidate (origin, orientation) stamps by
  inward runway, existing occupancy, and visible legibility; validate the
  winning candidate with a short deterministic 4-generation rollout so the
  release envelope is proven, not hoped. This is not pathfinding for every
  creature.
- **Identity**: the ring path distinguishes main Enter from numpad Enter
  (distinct HID usages). The VK-only fallback maps 0x0D to main Enter and says
  so. No paragraph detection; no inference from text.

### 7.5 Gust — make a sudden change travel outward

A burst launches one small traveler from the real recent source toward the best
legal open direction (not the "emptiest quadrant" — that may be masked or
absent). Two is a high-energy exception requiring genuinely separate runway.
If an Enter release and a gust qualify in the same small window, the Enter
departure wins and the gust energy improves its source and wake; never two
unrelated launches from one apparent action.

### 7.6 Squall — weather you raised

You raised it; it must not read as a reprimand.

- A source-centred, feathered front, biased AWAY from the recent active-hand
  area.
- Front reach bounded to ~20–25% of VISIBLE habitat, by area, not just angle.
- Mortality bounded to a minority (start ~15–20%) of the pre-squall population,
  preferentially mature; every affected cell handled exactly once by the swept
  region (§12.5).
- A finite one-shot nutrient displacement (wind), not a changed diffusion rule.
- One traveler by default; a second only with separate runway.
- A brighter fertile wake, then a recovery interval that suppresses another
  large disturbance. Younger life and real refuges survive.

The pretty parts (wind, the local front, the departure, what grows afterwards)
must not depend on destroying more organisms.

### 7.7 Typing raises weather over time

Sustained activity leans the climate toward summer. The world does not need to
infer "nobody is typing" to let summer exist — sunlight and seasons continue
regardless, and sustained activity simply biases the seasonal mix a little.
Keep it a small bias on the weights, never a season-skip.

## 8. Weather

### 8.1 The sun — a moving condition, not a spotlight

A broad, subordinate warm focus on a slowly wandering path: smoothly
interpolated waypoints, bounded speed, seasonal and weather-seeded variation so
the exact same route does not repeat every year. Never rerolled per frame, and
never the brightest moving object on the board (if it is, people follow the sun
and ignore the life). Its light: modest ageing acceleration, slight
live-brightness lift, local site-suitability warmth, a fertile wake. It is
weather, not a creature.

### 8.2 Seasons — 64 s year, four 16 s frames

Ecological character, not just hue:

| season | ecology | what you should notice |
|---|---|---|
| Spring | immigration favours fertile ground; moderate turnover; rising activity | small establishments; old contact traces waking |
| Summer | fuller pockets; slightly faster generations; strongest light, not uniformly brightest | recognisable colonies, brief brilliant patches |
| Autumn | reduced establishment; more visible death memory; lateral ghost transport | the living world thinning into drifting remains |
| Winter | sparse immigration; slower but nonzero generations; isolated ephemeral seeds | waiting, not a stopped program |

Generation rate ~2–4 gens/s at default speed, composed as
`BASE × speed × season_multiplier(t)`. Readability at slow hardware rates beats
the exact number.

The default spectrum is the same four segments (`hold 13, fade 3`) with
ROLE-CONSISTENT stops — residue band, living growth, mature growth, fresh
accent — so a role never lands in an unrelated region of the gradient halfway
through a fade. Small within-role seasonal variation only; do NOT stack a large
seasonal `u` offset on top of changing palettes (that double-animates meaning).

### 8.3 Spring arrival — dawn, not a notification

One gentle broad lift per year, weighted toward fertile and living ground
(rather than a white board flash that destroys local contrast), rising and
falling over ~1.5–2 s at a small amplitude. Not multiplied by a simultaneous
global breath. On resume, do not replay missed pulses.

### 8.4 Autumn petal-fall — drift the memory

Advect the GHOST field laterally along a wind direction chosen once per autumn
(bounded transport, bounded accumulation, no teleporting). Never advect
occupancy; never advect contact accents (fresh feedback must stay on the key
you pressed). Critically: keep a dark separation between living cells — if
every death leaves a bright long trail at 3 gens/s, the board becomes luminous
fog.

### 8.5 Winter fireflies — little failed starts are allowed

Sparse isolated births that brighten softly, then become ghosts. Elegant: they
make dormancy visibly alive without filling it. Their transient presence must
NOT convince the governor's established-occupancy signal that a stable
population returned — measure established recovery separately so you never loop
"firefly appeared → recovery cancelled → firefly died → recovery restarted".

### 8.6 The shooting star — rare, quiet-gated, with a defined aftermath

A faint shallow streak near the visually upper part of the habitat, chosen as an
OPPORTUNITY during prolonged low activity — not a per-frame coin flip (that
would make a 60 Hz preview more eventful than a slow board) and not a fixed
global timer. Use an elapsed-time hazard with a cooldown, or a weather-RNG
interval: require a quiet interval (order ≥ 45 s of low activity), then a
random additional delay in minutes. It does not interrupt another dominant
scene, and if the user starts typing mid-streak the streak completes
gracefully. Its wake is the swept segment (not just the current head), and it
leaves a FERTILE WAKE with a defined delayed establishment opportunity on part
of it after 2–4 s. Store a bounded pending wake action; cancel or re-evaluate
on a world reset. No universal delayed-action framework for one cameo.

### 8.7 The renewal passage — autonomous, selective, remembered

The full-board travelling ember front, retained as a distinct autonomous
scene — but board-spanning does not mean board-clearing. Trigger it on
sustained congestion (smoothed occupancy + ghost energy) or prolonged
recurrence, AFTER gentler interventions have had a chance, with a substantial
cooldown (order ≥ 60 s), and reduce or defer its prominence while the user is
actively working (existing weather completes smoothly; the world does not
freeze). It applies selective one-shot mortality (a minority, mature-biased),
preserves refuges, and leaves a fertile wake. A scene that repeatedly erases
the user's newest fertile area is the failure mode; a scene that waits for
quiet is not a punishment for typing.

## 9. The governor — a gardener, not a metronome

### 9.1 Slow signals

- **Established occupancy**: live population averaged over time, normalised
  against relevant habitat/visibility — never multiplied by frame rate. Winter
  is ALLOWED below the band; a spring peak may exceed it.
- **Recurrence**: repeated logical occupancy across a short exact-compare
  history, with a grace period (§4.4).
- **Recent intervention**: time since a viable reseed, squall, major predation
  burst, or renewal passage. This prevents competing recovery mechanisms from
  immediately "fixing" each other.
- **Visible congestion**: whether ghosts and substrate are burying the living
  structure. Congestion first reduces GHOST intensity/persistence; it does not
  kill organisms to solve a rendering problem.

### 9.2 Escalation, least disruptive first

1. Favour an existing fertile opportunity (wait for the next spore that lands on
   suitable ground).
2. Introduce ONE viable local motif.
3. Create a small local renewal opportunity.
4. Admit the rare board-spanning renewal passage (§8.7).

No tight proportional controller forcing occupancy into a band per generation —
that manufactures a visible fill–cull–fill rhythm and fights both winter and the
user.

### 9.3 Hard guarantees worth keeping

- A supported habitat that empties receives a bounded viable recovery
  opportunity within ~1–2 s when recovery is eligible.
- Recovery attempts have their own refractory period.
- World state, energy deposits, and pending scene counts are bounded.
- No event can force permanent darkness or a permanently saturated field.
- Long quiet runs keep producing renewal without a scripted reset loop.

### 9.4 Guarantees worth REMOVING (the false promises)

- "Population is never zero" — a habitat may briefly empty; it gets a bounded
  opportunity, and winter may be sparse without looking broken. A lone
  permanent pixel kept alive against the rules is not a guarantee, it is a
  lie told in f32.
- "The age cap proves all repetition is impossible" — it does not (§4.4).
- "Every Enter always produces another complete traveler" — it attempts a
  release; crowding may resolve as a flutter.
- "A rate threshold knows sentence-typing from mashing" — no threshold knows
  intent; make the false positive harmless and rare instead of pretending.
- "A permanently non-zero nutrient field implies ongoing life" — it doesn't;
  the recovery path is immigration, not residue.

## 10. Rendering

### 10.1 Visual priority (relative separation matters more than exact numbers)

1. **Fresh contact** — small, spatially exact, briefly dominant.
2. **Living structure** — crisp enough to recognise and follow.
3. **Active weather front / traveler source accent** — localised, bounded.
4. **Death memory** — softer and dimmer than comparable living structure.
5. **Habitat warmth / nutrients** — a faint substrate, never an illuminated
   blanket.

### 10.2 One `u`, one intensity — pick a dominant contributor

Several semantic contributors can exist at a cell (live, ghost, contact,
nutrient, front). Do NOT average their `u` values — an average can land in an
arbitrary, unrelated part of a user spectrum. Instead choose the dominant
contributor by the priority above, assign its intended `u`, and combine
intensity with a bounded rule. Use short envelope transitions or dominance
hysteresis so a near-tie doesn't flicker the colour every frame.

### 10.3 Discrete occupancy, interpolated envelopes

Interpolate birth/death appearance over a SHORT portion of a generation (start
~30%) while logical occupancy stays binary. Long crossfades between generations
erase the shapes the whole concept depends on.

### 10.4 Meaning survives monochrome

With any single-colour spectrum, live cells must still read as live (crisp,
local contrast) and ghosts as memory (dimmer, softer). The default palette
deepens that separation; it must not be the only reason it exists.

### 10.5 Test the quantized pipeline

`scale_f` rounds after scaling. Inspect final 8-bit bytes in tests: the
ecological memory must not live only in a float debug view. Do not apply a
second global brightness inside Wildlife — the output controls own that.

## 11. Time, determinism, and reproducibility

- Wildlife owns its accumulator. `dt == 0` is a no-op; repeated `field(same t)`
  with no new input must produce NO extra generations, nutrient loss, ageing or
  RNG draws. (`StepClock` deliberately does the opposite for the old patterns;
  do not copy it here, and do not change it globally.)
- Small forward step: execute exactly the simulated time accrued, capped (start
  8) so a long stall can't run a burst of generations or scenes in one frame.
- Backward `t`: rebase cleanly; never replay an old season event.
- Long pause: expire transient contact/charge state, no backlog of storms, stars
  or spring pulses; re-enter at the current calendar phase.
- Two independent deterministic PRNG streams (weather, immigration), so extra
  rendering cannot change future weather. Seeded tests replay exactly. A fixed
  finite PRNG does not imply eternal non-repetition; the promise is "no obvious
  scripted idle loop".
- `configure()` (param edits) must NOT clear the population. Layer lifecycle
  edits preserve the living world where the lifecycle allows.
- Geometry change (dimensions): deliberate re-initialisation or a documented
  remap — never stale indices, never a false held-key edge.

## 12. Implementation shape

### 12.1 Public surface

- One registered pattern: key `life`, label `Life`.
- One preset: slug `wildlife`, label `Wildlife`, blurb in house voice
  (lowercase, concrete), `source: "keys"`.
- `has_spectrum = true`. `tile.live_input = FALSE` — its documented meaning is
  "needs LIVE input to show anything", and Wildlife shows plenty without any
  (headless previews and the no-dead-knob sweeps should EXERCISE it, not skip
  it). Do not copy Comet's flag just because both react to keys.
- Params (deliberately minimal, house Range style):
  - `speed` — organism/ambient motion pace within a usable range. Does NOT
    alter physical-input thresholds and does NOT move the 64 s calendar.
  - `density` — desired fullness / establishment pressure, bounded by habitat
    size and season. Does NOT multiply direct press feedback into invisibility.
- No checkboxes for pets, fireflies, Enter, sun, autumn or storms. That is the
  authored personality. A reactivity control can follow testing; do not ship a
  laboratory panel preemptively.

### 12.2 Files

- `crates/neuron-core/src/pattern.rs`: the registry entry, the preset, and the
  `life_spectrum()` factory beside the other themed builders (locality: this
  is where `fire_spectrum` etc. already live).
- A private `mod life` submodule for the implementation (one pattern must not
  become one giant section). State per §3.3.
- `crates/neuron-core/src/capture.rs` + the `controls.rs` pump call site: the
  observation ring (§6.1), behind owner sign-off.
- Docs to update IN THE SAME CHANGE as the code: README effect ledger,
  `GDD.md` effects chapter, `CLI.md` preset list, `STATUS.md` grading if effects
  are graded there.

### 12.3 The Life itself

Travelers are CELLS in `live`. They are not separately simulated sprites, and
they get no creature tracking, names, inventories or pathfinding. Only their
initial placement needs a template.

### 12.4 Shared operations (the constitution, made concrete)

Private methods, not an event bus or ECS:

```
feed(center, amount, footprint)                 // nutrient deposition
kill_cells(selection, cause, recycle_fraction)  // ONE death, ONE ghost, ONE share
seed_motif(origin, orientation, template)        // viable immigration
impulse_nutrients(center, direction, strength)  // finite wind
acknowledge_contact(cell, kind, energy)         // the guaranteed local tick
```

Every producer goes through these. No producer mutates the arrays directly, so
ordering bugs, double fertilisation, and ecologically meaningful events being
buried under later writes are structurally impossible.

### 12.5 Front geometry is not front effects

A sampled Gaussian ring is a drawing, not a collision detector: at low frame
rates a moving band jumps over cells; a slow thick band covers one cell for
many frames. For squalls, renewal passages and the star's wake, apply
ecological effects to the SWEPT REGION between the previous and current front
positions, with a per-cell one-shot bitset so nothing is fertilised or killed
twice by the same front, regardless of how many render samples happened.

### 12.6 Transaction order (one `field()` call)

1. Read fresh observations since this consumer's cursor.
2. Advance weather/biology to each due simulation boundary.
3. Resolve CONTACTS against the state they encounter; produce immediate
   accents.
4. Update activity estimates; resolve overlapping derived triggers once
   (Enter > gust arbitration).
5. Apply admitted external mutations at the documented boundary.
6. Advance ordinary Conway, double-buffered, when a generation is due.
7. Apply senescence/disturbance masks; emit each actual death EXACTLY once.
8. Account for nutrient consumption/recycling and ghost contributions.
9. Update slow governor signals; admit at most the necessary intervention.
10. Render with explicit contributor priority.

With the ring, replay events at their relevant boundaries; with polling
fallback, batch at receipt and aggregate order-independently.

## 13. Scope guardrails

- Do NOT touch: input injection/synthesis, action dispatch, write routing,
  hardware authority, arm gates, firmware effect enumeration, device protocol
  behaviour, the python bridge.
- Justified small changes, each as its own reviewable piece: the read-side
  observation ring (§6.1, owner sign-off), tiny PURE shape helpers extracted
  from existing implementations, and (only if region tests demand it) a
  defaulted read-only exact-region hint.
- Wildlife is a host-rendered Pattern × Spectrum effect. It needs no firmware
  effect ID; the two lighting models are deliberately separate.
- This is not `risky` HID work beyond the ring: it reads the same safe key-down
  edges Comet and Thermal already read and writes nothing but light.

## 14. Build order

The order in which the experience becomes coherent. This is not a proposal to
ship half the concept; it is the order that makes all of it cohere.

- **Pass A — prove there are inhabitants.** Cellular kernel, bounded nutrients,
  age/ghost handling, local source acknowledgement, finite habitat projection.
  Seed a few valid motifs with staggered maturity rather than opening with a
  dense random soup. Watch one minute with no seasonal colour tricks: can you
  recognise something living, touch it, and recognise the aftermath? Does an
  empty region recover without a global reset? (If not, more weather hides the
  weakness rather than repairing it.)
- **Pass B — give the world a year.** The 64 s shared seasonal schedule, sun,
  autumn ghost transport, spring arrival, winter seeds. Verify palette timing
  against biological timing at boundaries and after long pauses. Seasons must
  differ with a monochrome palette too.
- **Pass C — make the two rituals delightful.** Enter release placement and
  Backspace pruning, validated against the real logical geometry and visible
  projection. Test repeated Enter in a terminal-like sequence, not one isolated
  dramatic keypress.
- **Pass D — let input raise weather.** The activity estimator, gust
  arbitration, restrained rhythm bonus, bounded squall. Replay NORMAL typing
  first; the event should be impressive when earned without turning normal work
  into constant disturbance.
- **Pass E — tune long quiet life.** Recurrence-aware gentle renewal, the
  autonomous passage, the star cameo. Long seeded sessions across sizes and
  frame cadences. Tune the governor against boredom and congestion, not merely
  average live counts.
- **Pass F — register and document.** Registry round-trip, well-formedness,
  benchmark, README/GDD/CLI/STATUS updates, and a hardware run.

## 15. Acceptance tests

### 15.1 Deterministic logic

| test | required result |
|---|---|
| canonical blinker, glider, horizontal traveler, external forcing disabled | match independently checked B3/S23 evolution |
| a lone isolated seed | dies normally; does not masquerade as stable recovery |
| a completely empty viable habitat | receives a bounded viable establishment opportunity without filling the board |
| repeated `field()` at identical `t`, no input | no extra generations, nutrient loss, ageing, or RNG draws |
| same seeded trace at 6 / 15 / 30 / 60 output fps | same logical ecology at matched simulation boundaries (documented envelope tolerance only) |
| one brief down/up between slow frames | preserved by the ring; explicitly best-effort only on the polling fallback |
| held Enter / Backspace / WASD | one physical down per initial hold; no auto-repeat storm or traveler stream |
| modifier aliases and mouse input | no accidental keyboard-rate inflation |
| input at activation/resume | no replay of already-held keys, no stale cursor backlog |
| preview and hardware consumers | both observe the same eligible input; neither steals it |
| suppressed thumbnail rendering | autonomous life continues; no real-key polling or reaction leaks |
| Enter + burst in one window | one resolved departure, not duplicated spectacle |
| Backspace on an already-empty patch | source feedback remains; no duplicate biomass |
| swept front at low frame rate | no skipped cells; no repeated death/fertilisation per cell |
| sun + squall + natural death coinciding | death accounted once; energy finite |
| four `hold: 13, fade: 3` frames | exactly 64 s; biological weights use the same boundaries and incoming-fade convention |
| density extremes; tiny regions; 0×N and N×0 | finite values, correct field length, bounded work, deliberate fallback |
| region/geometry changes and large time jumps | no panic, no stale traveler index, no storm backlog |
| ring: overflow, attach-at-head, dual consumer, suppression | each §6.1 property, as a test |

### 15.2 Long-run properties (aggregate diagnostics only, never shipped telemetry)

Run multiple seeds and representative geometry/input traces for 10–30 minutes of
SIMULATED time. Record live/visible counts, birth/death counts, recurrence
detections, nutrient sum, ghost energy, event admissions, and finiteness. Look
for: a repeating reseed–cull rhythm; winter permanently overridden by recovery;
repeated disturbance preventing any colony from forming; hidden logical life
satisfying the visible governor; nutrients pooling at the top (the reused-heat
buoyancy bug); star/storm frequency changing with frame rate; growing delayed
queues; most final pixels black or saturated after real quantization.

### 15.3 Human visual tests (cannot be replaced by metrics)

View the real keyboard at normal AND low global brightness, on the slowest
supported transport. Four scenarios: quiet observation; ordinary text/code
entry; editing with frequent Enter/Backspace; deliberate frantic input. Judge
the default at rest before the rare spectacle. Ask: Can I identify the cell I
touched? Can I follow the thing I released? Can I recognise a living pocket
twice? Does winter look intentional? Does the scene still look like Wildlife
without its default colours? Does a squall leave me pleased rather than
interrupted? A beautiful enlarged debug grid is NOT sufficient evidence.

## 16. Tuning candidates (starting values, all disposable)

| knob | start | meaning |
|---|---|---|
| base generation rate | 3.0 gens/s | × `speed` × season multiplier |
| season multipliers | winter 0.65×, spring 1.0×, summer 1.25×, autumn 1.0× | pace, not population clamps |
| year / season | 64 s / 16 s (`hold 13, fade 3`) | divides the 4096 s render wrap exactly |
| nominal lifespan | 30 gens, drawn 26..=38 at birth | senescence texture |
| sun ageing bonus | +1 age per generation in light | life faster and shorter in the sun |
| nutrient half-life | ~12 s first-order | memory of recent contact |
| nutrient diffusion | slow isotropic, small coefficient | no permanent buoyancy |
| recycling fraction | ≤ 0.25 of normalised maturity | bounded, no positive-feedback machine |
| birth nutrient draw | ~0.05 | ordinary births never blocked by an empty reservoir |
| metabolism drain | ~0.05/gen per live cell | gardens darken their ground |
| birth/death envelope | ~30% of a generation | short interpolation, discrete occupancy |
| gust | ≥4 downs in ~0.35 s, refractory ~2.5 s | event-driven, two travelers only with separate runway |
| squall floor | ~18 downs/s held ~2.5 s; cooldown 20–30 s | far above the ~6/s `step_rate` saturation |
| squall reach / mortality | ≤ ~25% of visible habitat / ≤ ~20% mature-biased | bounded by area and count, not angle |
| Enter refractory | ~3 s | full traveler release; else flutter |
| backspace prune radius | ≤ ~1.5 cells, falloff, maturity-first | judged on the real top-right corner |
| recurrence grace | ~8 s | one gentle immigration opportunity per repeat episode |
| recovery | viable opportunity within ~1–2 s of an eligible empty habitat; own refractory | bounded, not a metronome |
| renewal passage | congestion/recurrence driven; cooldown ≥ ~60 s; defers near active hands | selective, refuges, fertile wake |
| star | quiet ≥ ~45 s, then a random delay in minutes; wake establishment +2–4 s | opportunity-gated, bounded pending |
| spring dawn | ~1.5–2 s, small amplitude, weighted to fertile/living ground | dawn, not a notification |
| density band | broad (winter below allowed, spring peak above allowed) | design reference, not a clamp |

## 17. The ledger

| origin | trigger | guaranteed visible response | shared ecological consequence |
|---|---|---|---|
| contact | eligible down on dead ground | local rain tick | bounded nutrient splat |
| contact | eligible down on life | compact harvest accent | one death, one ghost, bounded recycling |
| contact | Backspace | pruning accent (even on empty ground) | small bounded nearby shatter; one classification |
| contact | Enter | distinct departure accent at the source | one valid traveler when admitted, else local flutter + fertile trace |
| derived | genuine burst | directed gust from a real recent source | one admitted traveler + local nutrient impulse |
| derived | sustained exceptional rate | local squall with bounded reach | selective one-shot turnover, finite wind, fertile wake, limited release |
| derived | sustained regular activity | nothing distracting | modestly richer deposits (≤ ~+20%) |
| weather | wandering sun | subtle local warmth + live-brightness lift | site suitability, modest ageing |
| weather | seasons | behaviour + palette on matching 64 s timing | establishment, turnover, ghost behaviour, pace |
| weather | viable immigration | small coherent establishment | a valid motif enters the same population |
| weather | winter ephemeral seed | one soft twinkle | a real isolated birth may die into memory |
| weather | star (quiet-gated) | faint shallow streak | fertile swept wake + defined delayed establishment |
| weather | renewal passage (need + opportunity) | travelling ember front | selective one-shot disturbance, refuges, fertile wake |
| stewardship | low established population or prolonged recurrence | prefer the subtlest local opportunity | bounded recovery/renewal without enforcing constant busyness |

## Appendix A. Verified source anchors

Design-time anchors at `60f354f`; they document what the design was written
against, not a maintained contract.

- Pattern contract: `pattern.rs:44` (`Cell`), `:61` (`Field`), `:84`
  (`render`), `:104` (`Bounds`), `:348` (`TileMeta.live_input` semantics),
  `:388` (`REGISTRY`), `:698`–`:728` (render clock, 4096 s wrap, `quantized_t`).
- Input: `capture.rs:92`–`:140` (suppression, `key_down` high-bit read),
  `:149`–`:236` (`MACRO_HELD` shared/stateless, `KEY_TRANSITIONS`,
  `key_transition_generation`, wake events), `pattern.rs:1120`–`:1196`
  (`scan_key_presses`, `key_scan_due`), `controls.rs:1030`–`:1072` (pump
  held-set diff → `note_key_transition`), `controls.rs:1094`
  (`held_registry_live`).
- Primitives: `pattern.rs:1289` (`value_noise`), `:1309` (`fire_flicker`),
  `:1344` (`StepClock`), `:1365`–`:1822` (Comet + `paint_burst` + streak
  sampler), `:1198`–`1287` (Heat), `:2097`–`:2314` (Thermal: `deposit_heat`,
  `cool_field`, `diffuse_field`, `step_rate`, `deposit_peak`, `heat_shimmer`).
- Spectrum: `spectrum.rs:15` (Frame model), `:391`–`:405` (Frame),
  `:498`–`:532` (`at`, total `Σ(hold+fade)`, incoming-fade convention).
- Output: `lighting.rs:24`–`:59` (`Rgb` 8-bit, `scale_f` rounds),
  `lighting.rs:621`/`:643`/`:743` (ENTER `(3,14)`, NUMENTER `(4,21)`, VK 0x0D →
  "ENTER"), `effects.rs:63`–`:101` (`Blend`, `blend_px`: Normal = over).
- Presets/registration: `pattern.rs:3017`–`:3049` (blend selection), `:3055`
  (`presets()`), `:3700`/`:3764` (catalog + source lists), `:4035` (live-input
  well-formed test to extend), `tests/lighting_bench.rs` (benchmark shape).
- Reference: Botania's Dandelifeon (the Conway-with-age flower this effect
  descends from) — 25×25 board, B3/S23 with air-only births, per-cell age with
  a cap, a consuming centre zone, and generational pacing at 2/s. Upstream
  facts, no code.

## Appendix B. Independent probes (design-time reasoning, not Neuron tests)

Small out-of-tree Python/mathematical checks whose findings constrain the
design. They are reasoning, not measurements of the shipped effect.

- **B.1 `step_rate` saturation.** With ideal captured presses, default `fade`,
  60 Hz, and a storm needing `rate > 0.9` held 2.5 s: 5 presses/s never holds
  the threshold; 6/s storms at ~6.5 s; 8/s at ~4.6 s; 15/s at ~3.4 s. Verdict:
  `step_rate` is not a "frantic typing" classifier — hence §6.3's absolute
  downs/sec estimator with an ~18/s floor.
- **B.2 Traveler geometry.** A glider moves 1 row + 1 col per 4 gens (≈0.75
  cells/s per axis at 3 gens/s) and is not steerable. A horizontal LWSS moves
  2 cols per 4 gens (≈1.5 cols/s) with a 5-row, 4-gen envelope and 9/12
  population — better suited to a keyboard's long axis, at a larger footprint.
  A SE glider seeded near the top of a 6×22 board stabilises into a 2×2 block
  by gen ~12 rather than crossing the keyboard. Verdict: orientation and
  placement are chosen deliberately, with a rollout check (§7.4).
- **B.3 Age caps vs repetition.** A blinker dies when its enduring core ages
  out; a glider survives 300+ gens because no position stays live for >4 gens;
  a period-15 pentadecathlon survives 1000 gens with max consecutive age 5.
  Verdict: senescence is texture; recurrence is tracked separately (§4.4).
- **B.4 Density vs crowding.** For random soup at p = 0.55, expected next
  density ≈ 0.2106. A 55% trigger would not hold for long under ordinary
  Conway, while a low-density repeating pocket never approaches it. Verdict:
  blight keys off smoothed occupancy + congestion + recurrence, not a single
  live-count threshold (§8.7, §9.1).
- **B.5 Calendar arithmetic.** `4096 % 60 = 16`; `4096 % 64 = 0`. Verdict: a
  64 s year (§2.3).
- **B.6 Front sampling.** A ring sampled per rendered frame skips cells at low
  frame rates and re-covers one cell at high rates. Verdict: swept-region,
  one-shot ecological effects (§12.5).

---

The premise, restated because everything above serves it: build the terrarium,
not the feature list. Keep the quiet contradiction that makes the idea
appealing — the user matters immediately, yet the world is not exclusively about
them. Make the important moments reliable. A press lands exactly where it
belongs; something stays recognisable long enough to become a little pet; Enter
sends something away rather than dumping a blob under the key; Backspace
disturbs without making the keyboard hostile; autumn turns death into motion;
winter leaves room to wait; a squall is a local event with a beautiful
aftermath. The rare star matters because most of the time the world is
confident enough to be small.
