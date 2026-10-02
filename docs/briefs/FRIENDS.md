> **kind:** feature design + implementation brief — the Friends Along The Way
> lighting pattern (`friends`).
>
> **status:** DESIGN ONLY. No code exists for this yet. The effect has not been
> implemented, registered, preset-cated, reviewed, or run on hardware. Every
> motion, value, and tuning number below is a hypothesis to be validated on a
> real keyboard (see `AGENTS.md`).
>
> **provenance:** the concept brief was red-teamed against the live tree at
> commit `b94df49`. This document records the refined, implementable design.
> Line references are design-time anchors, not a maintained contract. Numerical
> art-direction values are TUNING CANDIDATES, not measurements of a finished
> effect. Only a real keyboard run grades any of it.

# Friends Along The Way — your typing feeds a little traveller

## Implementation decisions (October 2026)

These refine the raw concept and take precedence where it leaves an open choice.

- Keep the DVD-screensaver idle, the reflect-not-wrap world, board-resident
  food, cardinal pursuit, visible growth, head-touch burst, and reunion. Those
  are the fantasy.
- The body is a **logical spine** (a capped history of recent head positions),
  not a decay field and not an occupancy grid. The scalar field remains the
  renderer; the spine gives the light a literal body.
- No self-collision, no death, no score, no game-over, no wrap, no hand orbit,
  no "dead-idle decays to dark", no newest-key cursor.
- Idle must be a complete, satisfying wallpaper on its own. It never goes dark.
- Reads only the passive key-down edge that Comet/Thermal already read. Writes
  nothing but light. Not `risky` HID beyond that existing read.

## 0. The idea, in one paragraph

A small luminous thing lives on the board like an old DVD screensaver. Keys you
press become food it can chase and eat to physically grow. If you manage to
tap the key it is sitting on, the body bursts into a handful of tiny friends —
then they gather back together and the same traveller carries on. Typing feeds
it. The fantasy is "a small thing lives in the lighting, and your hands feed
it" — not "play Snake on your keyboard."

## 1. The constitution

Do not add, at any stage:

- score, wins, death, a reset, game-over text, levels;
- self-collision, a legal-move search, A*, pathfinding;
- torus wrap (idle reflects; this is non-negotiable for the DVD read);
- hand bounding-box orbiting, a hold-key reunion, an "orbit then dive" arc;
- newest-key commit retargeting (typing must not make the head jitter);
- a decay field as the body's only state;
- param soup (only `speed` and `density`);
- a literal mouse-click handler that needs an app/Pattern event seam;
- a fake-key preview that pretends you are typing in the tile;
- a second lighting engine or parallel dispatch path.

## 2. What the tree gives us today

Verified against `crates/neuron-core/src/pattern.rs` at `b94df49`:

- `Pattern` trait: `field(rows, cols, t) -> Field`, with optional
  `configure`, `set_frame`, `set_bounds`, `set_visible_region`
  (`pattern.rs:212-232`).
- `Field::Scalar(Vec<Cell>)` coloured through the layer's `Spectrum`; a
  `has_spectrum: true` pattern keeps the spectrum editable (`pattern.rs:64-96`).
- `StepClock` already exists for fps-independent discrete steps
  (`pattern.rs:1388-1407`); `Comet`/`Heat`/`Rain` use it.
- `scan_key_presses(prev, r, c, count_unmapped, on_press)` already detects
  fresh down-edges, maps VKs to board cells, handles Razer M1-M6, ignores
  unmapped keys for visuals, and respects key-read suppression
  (`pattern.rs:1152-1160`). Use it.
- `scan_key_contacts` additionally tracks held-cell state
  (`pattern.rs:1162`). The earlier note that a held-key helper did not exist
  is stale — it does.
- `Bounds` + `set_bounds` define a placement rect; the compositor passes it
  (`pattern.rs:106-156`, `:225-227`).
- The registry derives everything: `REGISTRY` is the single source of truth
  (`pattern.rs:394-650`), with `PatternDef { key, label, make, params,
  default_spectrum, tile, has_spectrum, readout }` (`:366-381`).
- `presets()` is pure data (`:3317-3366`); `Preset::group()` derives shelf
  from `source`/`live_input`/`readout` (`:3283-3291`).
- `Comet` already implements the on-overlap MAX-intensity streak renderer
  (`pattern.rs:1796-1819`, `draw_comet` `:1828-1864`). Reuse that sediment.
- The idle preview is already enough: tile passes wrap
  `capture::suppress_key_reads()` (`glue.rs:10026`, `:10229`), so a pattern
  with a real autonomous idle renders fine in the tile with zero app branching.

Count corrections from earlier drafts, which the tree supersedes:

- The registry hard-count test currently expects **21** keys
  (`pattern.rs:3976`), not 20. The `REGISTRY` doc-comment still says
  "thirteen shapes" and names only `custom` + `vitals` as the non-effect
  layers — both stale (also omit `onair/miclight/modeheld/gamelight/signal`).
  Adding `friends` takes it to **22** and every count/comment must move with it.
- The preset catalog currently has **24** looks, not 22 — the golden shelf map
  at `pattern.rs:4022-4046` has 24 entries and would gain the 25th.

## 3. Dramaturgy, in seven acts

**Act I — Existence.** At true idle the friend is already there, doing the
DVD thing: constant oblique travel, reflection at the bounds, no teleport, no
random steering, no dark fade. Start small: one clear head, 3-5 cells of
coherent body, head brighter than the tapering body.

**Act II — Notice.** A fresh mapped key-down deposits food at that physical
key. Not a ripple: a compact pellet that lights sharply, settles, and breathes
faintly until eaten or expired. `Reactive` says "you pressed this";
`Ripple` says "your press emitted a wave"; `Typing Heat` says "your press
deposited energy"; Friends says "you left something here".

**Act III — Appetite.** The moment food exists, the friend wakes from
screensaver motion into a Snake-like mode. The loudest visual signal is a
**grammar change**: idle is diagonal; feeding is cardinal (row/column travel,
90° turns). That alone reads as "it switched modes". Food targets the nearest
meal, not the newest one, so real typing reads as foraging, not cursor
gitter.

**Act IV — Growth.** Eating must grow the body, not a glow. The spine length
actually extends. The tail does not pop; the creature simply forgets its path
more slowly. Growth saturates: one keypress is never one permanent segment,
and a capped maximum keeps a long session from becoming an impossibility. At
cap, eating still flashes the head (a "sated" flare) so feedback continues.

**Act V — Contact.** Pressing the key the head is sitting on is the touch.
Not food. A fresh down-edge, exact head cell (or near exact-cell centre —
much tighter than Comet's generous break radius), consumed as the burst
trigger and **not** also deposited as food, with a cooldown. Fallback if
accidental bursts fire too often: only allow it while roaming, not chasing.

**Act VI — Multiplicity.** The burst is the mother-spider image translated
out of horror: one apparent thing turns out to contain several little movers.
The retained body becomes a handful (3-7) of independent mini-friends on a
clean fan of velocities, each a small comet with its own micro-tail, same
edge-reflection physics, no multiplication, no crawl. Not particles, not
confetti, not an infestation. Count derives from fullness (bigger you -> more
of them), capped.

**Act VII — Return.** Not disposable particles. After a short scatter one
friend becomes primary; the rest steer to ordered follow-slots along its fresh
path, merge one-by-one into the body, and the original length reforms. If food
remains it enters Feed; else it settles back into Roam. The event is
punctuation in one life, not a reset.

## 4. The world model

- 2-D bounds-local coordinates for everything the creature does. On `set_bounds`,
  the head and spine are clamped/anchored, food outside is ignored, and burst
  friends respect the new rect. Bounds-aware **in the first version**, not as
  a follow-up: without it a placed layer makes the head teleport out of view.
- Edge rule: **reflect**, never wrap. Clamp the head back inside before the
  next step so a stall or large dt cannot vibrate it on the wall. A corner hit
  reflects both components. A small impact brighten is enough; a corner may get
  a one-frame white-hot wink, but no radial burst.
- The body may cross itself. Self-overlap is light, not an illegal move.

## 5. Food

- Board-sized `food` field (rows × cols of small amounts, ~132 cells on a
  6×22 — negligible). Fresh down deposits/increments at its cell and resets
  that cell's faint age; repeats stack within a cap rather than spawning queue
  nodes.
- Target selection is stable: nearest meal wins; ties break deterministically.
  No random switching.
- Backlog is accepted, not promised away: during fast typing the board is a
  glowing buffet, some food may fade unexpired, and the friend cannot be
  expected to eat one visible animation per keypress. Every press still
  contributes something — a pellet — it just may be mopped up late.
- Target loss/expiry retargets cleanly. No food left -> Feed settles back to
  Roam.

## 6. The spine, growth, and digestion

- `spine: VecDeque<(f32, f32)>` of recent head positions, capped to the max
  body length plus slack. Rendering walks the spine and emits the body; old
  entries beyond the visible length fall off.
- `fullness` rises on eating, saturates. `visible_len` eases toward the
  fullness-derived target — never pops.
- Digestion: a grace period after activity stops, then slow return to the
  baseline. Not instant shrink on key-up, and not permanent: typing leaves a
  memory, not a configuration.
- `density` scales the max retained length and the sated-glow; `speed` scales
  locomotion and feeding pace. Neither changes the state machine.

## 7. Rendering

- Still a normal scalar `Pattern` emitting `Field::Scalar`. Colour belongs to
  the `Spectrum`. Default spectrum: Comet-style tail -> head ramp (body in the
  user's spectrum, head toward hot/white). A custom spectrum works as ever.
- Semantic `u`: oldest tail ~0, body rises to the head ~1, food a stable
  mid/high sample, burst friends sample their own segment of body u.
- Head: one-cell-ish strong centre with a small halo, clearly findable — it is
  the interactive target. Body: coherent, tapered, softer than the head,
  continuous through 90° turns (reuse the streak sampler's sub-cell footprint
  so 90° turns read as bends, not disconnected cells).
- Self-overlap rasterises by MAX intensity, exactly like Comet.
- Food must not read as Ripple: a bright cell plus a faint 1-cell halo and a
  slow pulse. No expanding rings.
- Bounce: tiny impact brighten only.

## 8. Time and determinism

- Roam and feed advance on `StepClock` per `speed`, independent of render fps.
  Same catch-up cap as the others unless playtesting demands otherwise.
- Determinism: same board size + same params + same synthetic food/input
  injection + same time progression -> same output. Extract pure
  deposit/consume/grow/scan/merge helpers so tests never need the OS.
- The tile and the live compositor both render from the same key suppression;
  neither consumes input from the other.

## 9. Scope guardrails

- Do NOT touch: input injection/synthesis, action dispatch, write routing,
  hardware authority, arm gates, firmware effect enumeration, device protocol,
  the python bridge.
- Wildlife needed its own integration of the read-side edge ring; Friends does
  not — it uses `scan_key_presses`, the safe fresh-edge read that
  `Ignite`/`Comet`/`Thermal` already use.
- No app code should be required for the first implementation. The tile
  catalog and inspector are registry/preset-driven. If implementation forces
  app changes, stop and check whether the design is fighting the registry.

## 10. Implementation shape

Put the creature in its own file like `life.rs` is:

- new `crates/neuron-core/src/pattern/friends.rs` with `pub struct Friends`,
  the `Pattern` impl, and pure helpers (`deposit_food`, `consume_food`,
  `nearest_food`, `step_roam`, `step_feed`, `burst`, `gather`, raster).
- one `PatternDef` in `REGISTRY` (`pattern.rs:394`), `has_spectrum: true`,
  `tile.live_input: true` (matches the de-facto treatment of Comet — the
  earlier "live_input means cannot render" comment is a pre-existing taxonomy
  problem; leave a separate rename note rather than ballooning this).
- `params` = `speed_param()` + `density_param()`.
- `default_spectrum` = the accent -> white head ramp.
- one `Preset` in `presets()`: `slug: "friends"`, `label: "Friends"`,
  `pattern: "friends"`, `source: "keys"`, blurb `a little thing eats your
  typing and grows`. ("Friends Along The Way" belongs to docs and the tile
  card; the tile name elides, so keep the visible label short.)
- bump hard-count tests and comments: registry 21 -> 22, "thirteen shapes"
  wording -> fourteen, preset shelf map 24 -> 25 with `("friends", "input")`.
- no wire opcodes, no ledger entry, no chip, no fake preview.

## 11. Red team — known failure shapes

1. **Looks like Comet with a different route.** Fix: persistent reflection,
   never leaves, shorter default body, clearer head, no respawn gaps. The
   lifecycle must read differently before typing even starts.
2. **Looks like Reactive with a dot.** Fix: food persists as an object;
   visible collection; the retained spine genuinely grows.
3. **Pure decay field kills the fantasy.** Fix: the growth is real length.
4. **Newest-key wins.** Fix: board food + nearest meal.
5. **FIFO queue falls behind.** Fix: food grid, stacking, fade, capped speed
   boost.
6. **Becomes a game.** Fix: no collision, no death, no restart.
7. **Idle wraps.** Fix: reflection only.
8. **Idle goes dark.** Fix: the baseline creature never disappears.
9. **Burst is gross.** Fix: a capped handful, clean fan, ballistic, short.
10. **Burst is confetti.** Fix: friends are the conserved body, and reunion
    rebuilds it.
11. **Accidental burst every few seconds.** Fix: exact-cell touch, cooldown,
    then "roaming only" fallback.
12. **Placed region hides it.** Fix: bounds-aware simulation from the start.
13. **Tile looks dead.** It will not: the idle already renders with input
    suppressed. Do not fake input.
14. **Feed does not read as Snake.** Fix: cardinal movement + 90° turns while
    feeding, diagonal while roaming.
15. **Roam does not read as DVD.** Fix: stable velocity + frame reflection;
    anticipation is the charm.
16. **Growth maxes instantly.** Fix: fullness easing, a sated head flash at
    cap, density scaling the cap.
17. **Stays huge forever.** Fix: digestion.
18. **Shrinks the instant typing stops.** Fix: grace + slow return.
19. **Param soup.** Fix: `speed` + `density` only.
20. **GUI click balloons scope.** Fix: the touch is pressing the physical key
    under the head; keep `touch_head()` separable for a future generic seam.
21. **Neon bouncing ball.** Roam uses a stable oblique vector and reflects; it
    does not scatter randomly.
22. **Incomplete reunion.** Gather must restore the pre-burst fullness; the
    primary's followers are the rest of the body.
23. **Hold-key reunion drifts from the fantasy.** The reunion is timed and
    automatic, not hold-gated.
24. **The spine persists across a region/bounds change wrongly.** On bounds
    change, re-anchor or drop cleanly; never carry stale coordinates.
25. **A single huge creature after a marathon session.** Fullness saturates and
    caps; at cap the reads are head flash + a stable long body, not growth.

## 12. Test plan

Deterministic logic, all against pure helpers, no OS:

- registry contract: registers, unique key, valid params, scalar field,
  spectrum enabled, preset resolves, lands on the input shelf, count 21 -> 22,
  comment wording moved.
- idle: non-dark with reads suppressed; head stays inside bounds; x reflects
  left/right, y reflects top/bottom, corner reflects both; no wrap; advances
  over time; `speed` monotonically faster.
- spine: capped; never exceeds max; shrinks only toward baseline; never below
  baseline; self-crossing does not invalidate; samples stay within bounds.
- food: deposit increases, repeats stack within cap, outside-bounds ignored,
  nearest target stable, eat clears food + adds fullness, fullness saturates.
- feed: cardinal steps; a route reaches its target; no two-cell oscillation;
  no immediate reversal unless routing requires it; target loss retargets;
  no food eventually returns to roam.
- burst: touch consumes the press; no food also deposited; friend count
  bounded; monotonic with fullness if that rule is kept; bounded objects;
  friends stay in bounds via reflection; gather completes; fullness after
  gather matches pre-burst within intended rounding; state returns to Feed or
  Roam correctly; cooldown prevents retrigger.
- placement: physics use `Bounds`; head inside the placed rect; food outside
  ignored; 1×N does not panic.
- determinism: same dims/params/time/synthetic inputs -> identical output.

## 13. Tuning candidates (starting values, all disposable)

| knob | start | meaning |
|---|---|---|
| roam speed | a few cells/s | too fast reads as a bouncing ball, too slow reads as stuck |
| feed speed | slightly faster than roam | foraging hustle, never a teleport |
| baseline body | 3-5 cells | the idle silhouette |
| growth per meal | small | a couple meals = visible growth; one meal is a nudge |
| max body | density-scaled | substantial, never the whole board |
| food persist | several seconds | a meal, not a strobe |
| food stack cap | a few | repeats deepen the meal rather than queuing |
| sated flash | brief head flare | feedback at the cap |
| burst count | 3-7 | derived, capped |
| scatter / gather | ~1 s each | a punctuation mark, not a second effect |
| digestion | grace, then slow | typing leaves a memory, not a stain |
| bounce kick | a small brighten; corner gets the wink | the dopamine event is the reflection |
| touch radius | exact cell / near-centre | much tighter than Comet |
| collision cooldown | a moment | one touch, one burst |

## 14. Build order

- **Pass A — prove the world.** Roam reflection, spine, raster, idle preview.
  Watch one minute with no input: is it a satisfying little wanderer?
- **Pass B — feed it.** Food deposit/scan/consume, cardinal routing, real
  growth. One keypress should leave something you can find.
- **Pass C — touch and burst.** Exact-cell touch, a small bounded fan of
  friends, gather that restores the body. The best interaction, earned last.
- **Pass D — settle the loop.** Digestion, target stability, edge cases
  (1×N, region change, bounds change, suppression).
- **Pass E — register and document.** Registry entry, preset, counts, GDD,
  STATUS, and a hardware run before any honesty claim.

## 15. Acceptance criteria

At idle, a glance should plausibly say: "oh lol, it's doing the DVD thing."
One keypress: "that key left something." A few presses: "it's going toward
them." Consumption: "it got longer." Sustained typing: "I fed this little
bastard." Head touch: "OH MY GOD THERE ARE LITTLE ONES." — not "particle
effect", not "why did it glitch", not "ew spider". After a moment: they gather
back and the same creature continues. After inactivity: the small DVD
wanderer returns.

## Appendix A. Verified source anchors

Design-time anchors at `b94df49`; they document what the design was written
against, not a maintained contract.

- `pattern.rs:44` (`Cell`), `:61` (`Field`), `:84` (`render`), `:104`
  (`Bounds`), `:348` (`TileMeta`), `:366-381` (`PatternDef`), `:388`
  (`REGISTRY` doc comment — its "thirteen shapes" + two-layer list is stale),
  `:394` (`REGISTRY`), `:698-728` (render clock, `quantized_t`).
- Input: `capture.rs:92-140` (suppression, `key_down`), `:149-236`
  (`KEY_TRANSITIONS`, `key_transition_generation`), `pattern.rs:1120-1196`
  (`scan_key_presses`, `scan_key_contacts`, `key_scan_due`).
- Primitives: `pattern.rs:1123` (`xorshift`), `:1388-1407` (`StepClock`),
  `:1796-1822` (Comet render), `:1828-1864` (`draw_comet` streak sampler).
- Presets: `pattern.rs:3283-3291` (`group`), `:3317-3366` (`presets`),
  `:3371` (`preset_by_slug`), `:4022-4046` (shelf golden map, 24 today).
- Tests to extend: `pattern.rs:3962-3977` (registry count 21),
  `:3982-4011` (preset validity), `:4021-4076` (shelf map), `:4079`
  (recolourable solids), `:4301` (live-input well-formed).
- Output: `lighting.rs:24-59` (`Rgb`, `scale_f`), `effects.rs:61-101`
  (`Blend`, `blend_px`).
- App: `glue.rs:2286-2371` (hero preview, shared quantized clock),
  `:10026`/`:10229` (`suppress_key_reads`), `:9972-10163` (tile renderer).

## Appendix B. Pre-existing issues this feature should not own

- `TileMeta.live_input` conflates "reads live input" with "cannot render
  without it"; Comet and Friends render fine with input suppressed. Leave a
  separate rename note (`reads_live_input`) rather than scoping that cleanup
  in here.
- `on_preview_tick` runs the live compositor with the same key reads the tile
  suppresses, so a fed-live pattern's preview and board can drift. Same
  pre-existing condition for Comet; not Friends' to fix.
