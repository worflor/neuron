> **kind:** feature design brief — the relationship between your lighting stack and a
> connected visitor's (a Chroma game, an OpenRGB client).
>
> **status:** DESIGN ONLY. No code exists for any of this. Nothing here has been
> implemented, registered, authored through the GUI, or run on hardware. Every
> threshold, tolerance and duration is a hypothesis to be validated on a real
> keyboard with a real game attached (see `AGENTS.md`).
>
> **provenance:** designed against `main` at `c8d18fc`. Line references are
> design-time anchors, not a maintained contract. **Part A changes an existing
> safety contract** — the game-light refusal — and is marked
> **[OWNER DECISION]**; it must not be implemented without the owner saying so
> in the session that ships it.
>
> **this is not a new pattern.** Nothing here adds an entry to `REGISTRY`. The
> subject is a *relation* between two things neuron already composites.

# Visitor Relations — your theme plays off what the game is painting

## 0. The idea, in one paragraph

neuron already reads a Chroma game deeply enough to know that the thing playing
is *the same thing that played four times in the last two minutes*, to know that
the board has been still for a second, to know which keys hold a colour distinct
from the board's resting one, and to let you **name** all of it. Then it throws
almost all of that away and hands the lighting engine two scalars: an ambient
colour and an effect colour. So a game can tell your keyboard what colour it is,
and nothing else. This brief is about the gap between what neuron knows and what
your own lighting is allowed to know — three relations, each with a different and
mostly already-built home: **react to specific game events**, **take over only
where the game isn't painting**, and **match the visitor's brightness and tempo
so the two blend instead of arguing**.

## 1. The constitution

Do not add, at any stage:

- a second dispatch path, a second lighting engine, or a compositor that can see
  the arbiter's resolved output. **One engine, one compositor, one lane.**
- a `REGISTRY` entry. This is a relation, not a shape. If a part of this wants a
  new pattern, that is the design failing, not the pattern list being short.
- a live read of the arbiter's composite back into your stack. That is a
  feedback loop (base → resolve → base) that either rings or needs a frame-delay
  buffer. The provider discipline in `audio_spectrum` / `screen_ambient` /
  `GameFeed` exists precisely so this is never necessary. **Push semantics, pull
  at render time.** Never the other way round.
- the raw per-key game grid widened into the universal feed. One pattern may ask
  for cells; `GameFeed` stays semantic.
- a device brightness write as the mechanism for any of this. See §6.3.
- an `Action` on the game-light whitelist that can synthesize input, click, run a
  macro, or spawn a process. The existing refusal stays; only feedback widens.
- a per-frame or per-tick game read on the render thread. The producer stays a
  sampler thread with an idle auto-stop, as it already is.
- anything that fires while no game is connected, on stale data, or during the
  600 ms after a game leaves.
- a second "visitor" concept. A visitor is a `SourceId` holding a layer at
  `band::SESSION`. That is the whole definition and it does not grow.

## 2. The shape of the thing: three wants, three different homes

This is the load-bearing finding and it should be settled before any code. The
three things you want are **not one feature.** They land in three different
layers of the model, and only one of them needs anything new.

| you want | it is a property of | it lives in | exists? |
|---|---|---|---|
| react to *these* game events / elements | one layer | a `game-light` **rule** — the trigger engine *is* the element selector | ~95% built, gated by one function |
| take over only where the game isn't painting | the **merge** of two stacks | the **arbiter**, not the core compositor | half built; the useful definition is missing |
| match the visitor's **tempo** | one layer | `speed` — an existing pattern param, via `LightingLayer::Params` | built |
| match the visitor's **brightness** | the whole stack on a board | a **post-composite gain** — does not exist in core | new, small |

Three consequences worth being blunt about:

1. **The element-interaction half is a configuration surface, not an engine.**
   `Trigger::GameLight` already resolves `#id`, `@id` and user-assigned names
   (`chroma_lab.rs:549-557`), already matches by app needle and effect name
   (`engine.rs:429-432`), and already fires through the one resolve. The only
   reason it cannot drive your lighting today is that `Action::LightingLayer`
   is not on a six-variant whitelist.
2. **The takeover half belongs to `neuron-host`, not `neuron-core`.** There are
   two compositors in this tree and they are easy to confuse. `Blend::Cut`
   (`effects.rs:67`) is *within* one stack. The visitor lives in the **arbiter**,
   which has its own `BlendMode`, per-layer `alpha`, and a mode-aware black rule
   (`paint.rs:297`). Building takeover in the core compositor would be building
   it in the wrong compositor, where the visitor does not exist.
3. **"Match brightness" is not "match tempo."** One is a parameter write to a
   single layer; the other is a gain on the composite of all of them. They share
   a word and almost nothing else.

## 3. What the tree gives us today

Verified against `main` at `c8d18fc`.

### 3.1 The understanding that already exists

`crates/neuron-host/src/adapters/chroma_scene.rs` is a pure state machine,
`(t_ms, frame)` in, `SceneEvent`s out, calibrated on a 600 s Overwatch capture.
It already knows all of this:

- `Rest { ambient: Option<Rgb>, roles: Vec<Role>, animated: Vec<u16> }`
  (`:180-189`) — the resting picture: the modal colour, every other still cell
  grouped **by exact colour** into a `Role { rgb, cells }` (`:174-178`), and the
  cells that kept changing while the board was otherwise still.
- `Look { id, ambient, colours, count, last_ms }` (`:220-228`) — each distinct
  rest picture the game settles into, with a stable id, so *"the game entered its
  hero-select screen"* is an addressable fact. Matching allows ±6 per channel
  (`Look::SLACK`, `:232`).
- `Template { id, signature, count, last_ms, swatch }` (`:246-254`) — a
  recurring effect, with `Signature { hue: [f32; 14], dur_ms, uniformity }`
  (`:89-97`). `uniformity` is ~1 for a board-wide flash and small for a
  travelling wave front. The module's contract is only *"the same thing gets the
  same id"*; naming is deliberately not its job (`:21-22`).
- `SceneEvent::{Burst, Effect{id,dur_ms,distance,new,truncated}, Ambient, Roles,
  Look}` (`:193-215`) — edges. `Burst` fires early, at `prefix_ms = 250`.
- The user can **name** effects and looks, and hide them and flag them for
  alerts: `EffectNote { id, name, hidden }`, `SceneNote { id, name, alert }`
  (`chroma_lab.rs:86-103`), persisted per game exe.

### 3.2 The width of the channel that carries it

`GameFeed` (`crates/neuron-core/src/lighting.rs:1037`):

```rust
pub struct GameFeed {
    pub ambient: Option<Rgb>,
    pub effect: Option<(Rgb, f32)>,
}
```

Two scalars. Produced by `feed()` (`chroma_lab.rs:807-825`), which takes a
`Game`, six per-class `Watch`es with live `Scene`s, and the `Effects` state, and
returns one ambient and one effect colour. `gamelight` (`pattern.rs:3067`) then
paints that single colour **flat across every lit cell in the placement**
(`:3113-3120`), with a `show` selector (ambient+effects / ambient / effects) and
a `level` lift. The whole of neuron's game-lighting surface is one flat colour.

Two things in that channel are better than they look and should be built on:

- `effect.1` is **already an envelope**: 1.0 while an effect plays, decaying to
  0 across `FEED_DECAY_MS = 600` (`chroma_lab.rs:366`, `:819-822`). That is a
  loudness envelope of the visitor, already computed, already free.
- `latest_game_feed()` stamps its own read time, and `game_feed_wanted()` reports
  whether a `gamelight` layer rendered in the last two seconds (`:1055-1069`),
  so the producer can idle. Any richer feed should keep that exact demand signal
  or the tap starts decoding for nobody.

### 3.3 The merge that already half-exists

`PaintPolicy` (`neuron-host/src/paint.rs:58-71`) carries `blend`, `strength`
(0..=100), `fade_ms` (0..=2500), a surface scope set, and a restyle `Lens`
(hue / saturation / brightness). Chroma's default is `Screen` at 100 % with a
450 ms fade (`:149-161`); OpenRGB's is `opaque()` — `Over`, no fade (`:176-186`),
which is the honest default for config tools that expect a set colour verbatim.

And `merge_cells` (`:297-319`) already implements the mode-aware black rule:

> in any non-`Over` blend a painted pure-black cell becomes `None` — transparent,
> "the game says nothing here"

So **"my lighting shows where the game isn't painting" already half-exists**,
as a side effect of Chroma's default blend. It is just not a thing you can
choose, name, or see.

`band::AMBIENT` (`arbiter.rs:44-48`) is reserved, claimed by nothing, and its
doc calls itself *"the designed slot for a future out-of-app ambient source."*
Note carefully what it does and does not buy: `resolve` composites ascending by
priority, and `AMBIENT` (1000) sits **above** `BASE` (0). Moving a visitor into
it would put it under competing *visitors*, not under your own stack. Getting a
visitor genuinely beneath your base needs a band below `0`, which does not exist.
Do not promise the understory.

### 3.4 The gate

`Action::LightingLayer` exists (`action.rs:189`). `Action::is_feedback()`
(`:807-817`) does not mention it:

```rust
matches!(self, Action::Noop | Action::Obs{..} | Action::ProfileSwitch{..}
    | Action::ProfileCycle{..} | Action::OutputMute{..} | Action::OutputGain{..})
```

Two call sites read it, and they are the two halves of one contract:

- `authoring.rs:2125` — an authoring-time error, so a bad rule is refused where
  it is written.
- `engine.rs:465-470` — the runtime enforcement point, and the comment is the
  reason the whitelist exists at all: *"letting it synthesize input would turn a
  lighting feed into gameplay automation. Enforced here, the one resolve every
  dispatch path goes through, so no rule file can opt out."*

The gate's stated purpose is **input synthesis**. `LightingLayer` swaps the
layers of your own compositor. It cannot press a key, so it does not violate the
reason the gate is there — which makes the omission look like an oversight
rather than a decision. That is the case for widening it, and it is the owner's
call, not this brief's. **[OWNER DECISION]**

### 3.5 Corrections to the tree's own docs

- `docs/GDD.md` has **no section on Chroma, the protocol host, the lab, or the
  Game Light layer.** Its integrations row is OBS-only. Whatever ships here
  should not make that worse; a GDD pass for the host is a separate piece of work.
- "Thirteen shapes" is stale in **four** places — `pattern.rs:387`, `:3253`,
  `:3314` and the registry-section banner at `:3959`. The registry holds **21**,
  and the six patterns none of those lines mention are `onair`, `miclight`,
  `modeheld`, `gamelight`, `signal` and `life` — the entire data-tile family
  plus Wildlife.
- `docs/briefs/REACTIVE-RESERVOIR.md` is marked *"v8 — Fully Approved & Sealed"*
  and describes `ContactReservoirBank`, `ObservationBarrier` and `ObservationPump`.
  None of it exists. Nothing in this brief depends on it.

## 4. Part A — react to specific game events (the cheap ship)

**This is the part that should exist first, and it is mostly a UI and one
whitelist arm.**

### 4.1 The mechanism

Not a new param. `Params` is a flat `BTreeMap<String, f32>` (`pattern.rs:242`)
and cannot hold an id or a name, so element selection can never be a knob. The
repo's native binding verb is a rule, and rules already speak game-light:

```toml
[[rules]]
trigger = "game-light:overwatch/ult"
action = { type = "lighting-layer", op = "replace", index = 0, preset = "ripple" }
```

`LightingLayerOp` is `#[serde(tag = "op")]`-shaped, so the payload deserializes
straight into the variant, and the same op also has a shorthand verb form
(`"replace 0 ripple"`, `authoring.rs:678-680`). It already has the ten
operations this needs (`action.rs:261`): `Push`, `Pop`, `Replace`, `Move`,
`Spectrum`, `Params { index, params }`, `Region`, `Toggle`, `Enable`, `Disable`.
Two of those matter more than the rest:

- `Spectrum { index, spectrum }` — a rule can rewrite a layer's *colour program*
  on a game event without touching a pattern at all.
- `Params { index, params }` — this is how tempo matching is expressed (§6.1).

Both are already undoable (`action.rs:1224`), so a game that fires an effect
mid-edit cannot strand the user in a mutated stack.

So the authoring surface is:

- widen `is_feedback()` to include `Action::LightingLayer` **[OWNER DECISION]**
  — both call sites move together, and the two tests
  (`authoring.rs:3515`, `engine.rs:759`) must keep proving that input
  synthesis is still refused;
- in the Chroma lab, next to each effect and each look, let a binding target a
  lighting layer op the way `fire` (`chroma_lab.rs:549`) already targets a
  feedback action;
- the lab's log already marks when an effect played and how long
  (`Mark::Effect { id, dur_ms }`) and when the board settled into a scene
  (`Mark::Scene { id }`). Those marks are a ready-made timeline to bind against
  and to scrub.

### 4.2 What this gets you immediately

A theme that jumps between layers on game events, with no new pattern, no new
feed, and no new engine: ult fires → your top layer becomes `ripple`; the game
enters `@hero-select` → your spectrum changes; the game returns to rest → push
the ambient layer back. Named ids and user-assigned names both work today.

### 4.3 The failure nobody designs for

A game that fires the same effect id on a 2-second cooldown will drive your
lighting op every 2 seconds forever. `Push`/`Pop` are not idempotent; a rule
that pushes on every burst grows your stack until the profile is unusable. This
is why Part A must ship with a stated idempotence contract — most likely
`Replace` and `Params` only, with `Push` requiring an explicit acknowledge — and
not simply expose all ten ops to a repeating edge.

## 5. Part B — take over only where the game isn't painting

### 5.1 Three meanings, and only one is worth building

"Where the game isn't painting" has to be pinned down, because the three
candidate definitions behave completely differently and only the third is
interesting:

1. **Black.** Where the game wrote `RGB(0,0,0)`. Already implemented, for free,
   under Chroma's default `Screen` blend (`merge_cells`). Nearly useless in
   practice: a modern Chroma game paints a full-board ambient, so almost nothing
   is black and almost nothing of your theme returns.
2. **Ambient.** Where the game shows its modal resting colour. Needs
   `Rest.ambient` published; `ambient_min_share = 0.25` already decides when a
   colour counts as ambient.
3. **At rest — where the game is painting nothing *distinctive*.** The still
   cells that differ from the ambient, i.e. `Rest.roles`: the WASD cluster, the
   health bar, the cooldown keys. **Your theme takes everything else.** This is
   the one worth building, it is the one only neuron's scene module can compute,
   and it is the difference between "your theme and the game's take turns" and
   "the game's actual information stays legible while your theme is beautiful."

Definition 3 is the feature. 1 is what already happens and should only be
labelled honestly.

### 5.2 The hard part is the tolerance, not the plumbing

`Rest.roles` groups by **exact colour** and `Look::matches` allows only ±6 per
channel. A game that dithers an ambient by ±1 per frame will classify half the
board as "distinctive," and your theme will strobe holes through the middle of
the game's picture at whatever rate the dither runs. So "distinctive" needs a
decision this brief does not make: a per-channel tolerance, a minimum role size
(a 1-cell role is noise), and a stability requirement (a cell must hold its
colour across a settle before it counts). Getting this wrong is the single most
likely way this feature ships looking broken.

The honest failure mode to design against: the takeover boundary *breathes*.
Cells drift in and out of "distinctive" as the game's ambient shifts, and the
result is a shimmering edge that reads as a bug, not a design.

### 5.3 Where it must not be built

Not in the core compositor. `Blend::Cut` (`effects.rs:67`) — *"an unlit cell
falls THROUGH to the layer beneath"* — is the right *idea* and the wrong
*location*: it operates between layers of one stack, and the visitor is not in
your stack. The arbiter already has per-LED `None` transparency, `BlendMode`, and
a `PaintPolicy` per family. This is a third `merge_cells`-class rule plus a
definition, in `neuron-host`.

### 5.4 What it looks like when it works

You keep a theme across the whole board. The game's WASD keys, its health bar,
and whatever it happens to be highlighting burn through at true colour because it
put them there. The rest is yours. Nobody else's lighting stack does this, and
it is the single most obviously valuable thing in this brief.

## 6. Part C — match the visitor, so the two blend instead of arguing

### 6.1 Tempo — nearly free, and it should be a rule

The scene module already has what a tempo estimate needs: `Template.count`,
`Template.last_ms`, `Signature.dur_ms`, and a `Burst` edge at 250 ms into every
effect. An inter-onset interval over the recent bursts of one template id is the
game's rhythm for that event, and `count`/`last_ms` are already persisted per
game exe, so it survives a restart.

Tempo maps onto an **existing parameter**: `speed`, present on 11 of the 21
registered patterns. `LightingLayer::Params { index, params }` can already write
it. So "when the ult fires, my flow speeds up for its duration" is a rule, with
a duration problem to solve afterwards (nothing currently schedules a parameter
back). **Do not build a scheduler for this.** If the return-to-rest is awkward,
ship the one-way version honestly and say so.

### 6.2 Brightness — ducking, and the envelope already exists

`GameFeed.effect.1` is 1.0 while the visitor is loud and decays to 0 over 600 ms
(`chroma_lab.rs:819-822`). A gain on your composite that follows it is the
lighting equivalent of ducking in a podcast: your theme drops while the game is
loud and comes back when it settles. None of the three ecosystems in Appendix A
offers any way to relate your lighting to another application's loudness, and the
signal is already computed.

This is a **per-board, per-stack** gain, not a per-layer one, because "my whole
theme dims" is not a statement about any single layer. It does not exist in core
today and is the one genuinely new mechanism in this brief.

The discoverability risk is real and should be stated in the brief rather than
discovered in use: a theme that dims for reasons the user cannot see reads as a
broken keyboard. It must be a visible, explicable control with a live readout of
what it is following, not a hidden coupling.

### 6.3 What brightness matching must never be

`capability::set_brightness` (`capability.rs:287`) is a **real HID write** with a
read-back verifier, and the README grades lighting brightness writes as *proven*
on hardware. It is also a device-level setting that would collide with the user's
own brightness, and following an envelope through it would mean a verified
write path carrying a 30 fps animation. Brightness matching is a gain on the
composite. Full stop. This is the one place where the obvious implementation is
both slower and more dangerous than the correct one.

### 6.4 The colour half, if it is ever wanted

There is a real version of "make my lighting blend better with theirs" that is
not brightness or tempo: make your **spectrum** relative to the visitor's hue —
analogous at ±30°, complement, or tint-toward. Colour lives only in the spectrum,
and there is a test enforcing that patterns carry no colour params
(`pattern.rs:3572-3576`), which makes the spectrum the architecturally correct
home for a colour input. `Spectrum::at(t, u)` (`spectrum.rs:503`) is pure over
two scalars; an added `at_mood(t, u, &mood)` with `at` delegating at a neutral
mood leaves every existing caller and test untouched, and would make **17 of the
21** registered patterns visitor-aware (the four exceptions — `screen`,
`custom`, `vitals`, `gamelight` — are the `has_spectrum: false` colour-emitting
patterns and could not participate).

This is listed last deliberately. It is the most elegant idea here and the least
necessary: hue-shifted-on-the-fly lighting is easy to notice and hard to love, and
§6.2 gets most of the benefit for a fraction of the surface. If it ships, it
ships after takeover works.

## 7. Red team — known failure shapes

1. **"It's just the game's colour, one layer down."** If your theme and the
   visitor's ambient end up the same hue, the relation is invisible and the user
   concludes the feature does nothing. Suggests hue work (§6.4) may not be
   optional after all, or that the relation must be legible in *motion* rather
   than hue.
2. **The takeover boundary shimmers** as cells drift in and out of "distinctive."
   The most likely shipping-blocker; see §5.2.
3. **Rules grow the stack without bound** when a game repeats an effect id
   forever. See §4.3.
4. **A stale visitor.** The producer goes quiet, the game closes, and the feed
   holds its last value. The existing honesty rule — every live pattern renders
   *dark* when its source is absent rather than guessing — is the thing to copy,
   and the 15 s REST / 12 s SHM leases are the backstop. A reactive theme must
   never show a game that has left.
5. **Ducking looks like a fault.** See §6.2.
6. **"Interacting with elements" becomes a rule editor**, and rule authoring is
   the single worst-explained surface in the product. If Part A needs a tutorial
   to be usable, it has failed the "show, don't tell" bar and the lab UI has to
   carry it instead.
7. **Overclaiming calibration.** `chroma_scene.rs` is tuned on **one game** —
   600 s / 750 frames of Overwatch. Every threshold in §5.2 is a guess until a
   second game is captured. Do not describe any of this as game-agnostic.
8. **The 2% failure: it works on the keyboard.** `Rest` is per device class and
   `min_cells_for_effects = 16` disables effect detection on small grids. A mouse
   or headset will behave differently from day one, and STATUS already grades the
   non-keyboard layouts as verified only on static frames.

## 8. Build order

**A — Prove the channel.** Widen the feed to carry semantics only (look id,
effect id, envelope, dominant bin, uniformity) — not cells. Keep the demand
signal. Prove it with a pattern that renders one honest value, in the preview,
with no game and with a game. Establishes the seam before anything depends on it.

**B — Part A, the whitelist.** The `is_feedback()` decision, both tests updated
to still refuse input synthesis, and one hand-written rule driving a layer op
end-to-end from a real game. Nothing else in this brief matters if this is
uncomfortable.

**C — Part A, the surface.** Lab bindings for effects and looks. The timeline
marks already exist; this is a target on them.

**D — Part B.** Definition 3 first, in `neuron-host`. Definition 1 as a labelled
existing behaviour, not new code. Tolerance and stability are the work.

**E — Part C brightness.** The composite gain, then the control, then the readout.

**F — §6.4 only if A–E are honest on hardware.**

Each pass runs `.\validate.ps1`. Only **pass B** touches the game-light refusal
contract, and it wants a human read of that diff rather than a green suite.

## 9. Acceptance criteria

Stated as things that must be *observed*, because per `AGENTS.md` a green suite
proves none of this:

- With a game connected and one effect bound to a layer op, the layer visibly
  changes on that effect and returns afterwards — on a real keyboard, with the
  game in front of you.
- With takeover on, the game's distinctive keys are legible at true colour while
  the rest of the board carries the theme, and the boundary does not shimmer
  while the game sits still.
- With brightness matching on, the theme visibly yields while the game is loud
  and returns within about a second of it stopping, and the user can see *why* at
  the control.
- Closing the game leaves the board in the user's own lighting, with no residue,
  promptly.
- Every one of the above is false when no game is connected — and that is
  asserted, not assumed.

## Appendix A — prior art, for calibration

- **Razer's own guidance** teaches layering as base / active-keys /
  in-game-events, with health and ammo readouts. All three layers are authored by
  the **game developer**, in the game's own Chroma profile. There is no path for a
  user's theme to react to a game — the relationship is authored game-side, in
  advance, per game.
- **SignalRGB** is the closest thing in existence: a game HTTP-posts events and
  they arrive in a lighting effect's `onCanvasApiEvent`, with a state handler that
  tracks which effect is current. That is event bridging into a script — not a
  compositor relation, and no spatial reasoning about where the game is painting.
- **OpenRGB** has no notion of one client reacting to another.

The specific thing neuron has that none of them do: a live, named, stable-id
addressing of another application's lighting *semantics* — its phases and its
recurring effects, identified as the same thing each time they play, with the
user supplying the names. That is the asset. Every part above is a way of
spending it.

## Appendix B — pre-existing issues this must not own

- "Thirteen shapes" is stale at `pattern.rs:387`, `:3253`, `:3314` and `:3959`;
  the registry holds 21.
- `docs/GDD.md` documents no part of the protocol host, the lab, or the Game
  Light layer.
- `REACTIVE-RESERVOIR.md` is marked sealed and is entirely unimplemented.
- `TileMeta.live_input` conflates *"reads live input"* with *"cannot render"*, and
  `on_preview_tick` runs the live compositor with the same key reads the tile
  suppresses. A visitor-reactive pattern's tile will hit this.
- STATUS grades the whole Chroma subsystem 🟠, the scene module as tuned on one
  game, and the lab's live tap as having no automated tests at all. This brief
  adds to that pile; it does not discharge it.
