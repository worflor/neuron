# FEEL pane — finishing & expansion roadmap

Working notes (2026-06-23). Scope: finishing the device-write controls we already have, and
the new **MECHANICAL ADVANTAGES** category (Snap Tap first). Written so the next session — with
the user *at the hardware* — is fast. Nothing here was wired blind.

---

## 0. The governing principle — verify-gated writes (don't fight it)

Every neuron device write goes through one gate (`crates/neuron-core/src/writes.rs`):

1. **Driver mode** — flip Razer device-mode 0x03 so host control is accepted (`ensure_driver`).
2. **Volatile first** — write NOSTORE so nothing flashes to onboard until it's proven correct.
3. **Read-back verify** — re-read the matching *getter* and confirm the bytes we set landed
   (`verify_getter`). If the device doesn't echo what we wrote, the call **errors** instead of
   lying that it worked.

**Consequence that shapes everything below:** a write is only "done" once its round-trip
**verifies on real hardware**. Several writes are *already implemented* but held behind a per-
feature env flag until that confirmation happens. So "finish the ones we have" = mostly a
**hardware verify step (you + the Naga)**, not more code. And a feature with **no getter** can't
be verify-gated at all — which is exactly why LOD/debounce are honest stubs, not fake knobs.

---

## 1. Status of every FEEL write (source of truth: `writes.rs`)

| Control | Opcode | Confidence | Gate today | "Finish" = |
| --- | --- | --- | --- | --- |
| DPI / polling / brightness | proven | **hardware-proven** | live | done |
| DPI stages (cycle + active) | SET 0x04/0x06, GET 0x04/0x86 | **hardware-proven** (Naga, verify-gated) | live | done |
| **Wheel mode** (HyperScroll) | class 0x0B | derived layout, verify-gated | env `NEURON_HYPERSCROLL_WRITE` | **verify round-trip on Naga → drop env gate** |
| **Sleep timer** (idle-off) | SET 0x07/0x03 (GET 0x07/0x83 proven) | SET derived, verify-gated | env `NEURON_IDLE_WRITE` | **verify on an *awake* device → drop env gate** |
| **In-game polling** (wired/dongle pair) | SET 0x00/0x05, GET 0x00/0x86 | single-rate proven; the *pair* unconfirmed | env `NEURON_INGAME_POLL_WRITE` | **verify 0x00/0x86 → drop env gate** (this is where 8000Hz lives) |
| **Lift-off distance** | — | **none** (no getter observed) | hard stub (errors) | **RE step — see §3** |
| **Debounce** | — | **none** (no getter; likely firmware-fixed on Razer) | hard stub | drop the stub or replace with an honest "fixed in firmware" line |

---

## 2. The three "almost-done" writes — finish = a 15-min hardware pass (you + Naga)

These are implemented + verify-gated; they just need the round-trip confirmed. Per control:

1. Set the env flag (`NEURON_HYPERSCROLL_WRITE=1` / `NEURON_IDLE_WRITE=1` / `NEURON_INGAME_POLL_WRITE=1`).
2. Change the control in the deck; the built-in `verify_getter` read-back either **confirms** (the
   device echoed our bytes) or **errors** (layout wrong — don't trust it).
3. If it confirms cleanly a few times: **remove the env gate** so it becomes a normal live control.
   If it errors: the derived byte layout is wrong → capture + fix `build_*_payload`, re-verify.

Highest ROI next step. Wheel mode + sleep are most likely to "just confirm"; the in-game polling
*pair* is the riskiest (the separate wired/dongle divisors are an unconfirmed reconstruction).

---

## 3. Lift-off distance — the genuine RE blocker (+ the asymmetric flex)

`set_lift_off_distance` is a stub because **no LOD getter was ever observed**, so there's nothing
to verify against (and we won't fire blind). Two ways forward:

- **Path A — port from razerctl/OpenRazer.** Community tools reportedly expose LOD (and *asymmetric*
  lift-vs-landing) for Focus-Pro-class sensors. **Check whether they decode a GETTER, not just the
  setter.** If yes → implement verify-gated + env-flagged exactly like `set_idle_secs`. If it's
  set-only → it can't be verify-gated, and porting it would violate the "nothing fires blind" rule.
- **Path B — capture.** One USBPcap of Synapse's "Calibration / Lift-off distance" control changing,
  recover `{class, id, payload(mm or raw)}`, implement verify-gated.

**Bonus:** your Naga V2 Pro's Focus Pro 30K sensor supports **asymmetric cut-off** (separate lift vs
landing distance) — a control Synapse buries. Exposing it cleanly (two values) is a real "we show
what Synapse hides" flex once the opcode is in. UI: mm presets + an optional asymmetric pair.

---

## 4. Hyperpolling — keeping it (stubborn :3), honestly

8000Hz lives inside the in-game-polling pair (§1/§2). Finish it with that verify pass. Two honesty
notes already correct in the build: the **dongle** row caps at 4000 (wireless 8000 over the dongle
is genuinely unsolved upstream — USB re-enumeration; OpenRazer marked it wontfix), and the whole
pair stays env-gated until confirmed. Wired 8000 verifies with the rest of the pair.

---

## 5. MECHANICAL ADVANTAGES — the new category (Snap Tap first)

### The concept (per the user)
A SYSTEM-page **preferences category** (sibling to the notification stages) for explicitly opt-in,
edge-giving features. **No text hints yapping up the UI.** Each option is ONE *self-demonstrating
visual stage* — like the notification preview mocks (`NotifStackMock`) — that shows **what it does**
and **why it's an "advantage"** spatially/visually, **locked behind a two-step consent.**

### The reusable shape — `MechAdvantage` component
- A header label for the category + the option name.
- A **stage visual** (procedurally drawn/animated in Slint, the `NotifStackMock` idiom — a Timer +
  `animate`, no fonts/paragraphs) that demonstrates the feature.
- **Two-step consent gate** (the user's flow: "click one to reveal the toggle, then again to toggle"):
  - **Locked:** the visual is obscured (dimmed/blurred + a small lock glyph). You can tell something
    powerful is here, but it's sealed. Click → **reveal**.
  - **Revealed:** the visual plays its demo; a clear **toggle** appears. Click → **enable**.
- Competitive-legality lives *in the gate as a quiet warn accent*, not a paragraph (the consent click
  IS the acknowledgment).

```
MECHANICAL ADVANTAGES
┌─ snap tap ───────────────────────────────┐   ┌─ snap tap ───────────────────────────────┐
│  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓  ⌁ locked  │   │   A ◄────●          ●────► D              │
│  ▓▓▓▓▓  (sealed — gives an edge)  ▓▓▓▓▓▓▓  │ → │   [A]▮  hold both → ●  snaps to last      │
│  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓             │   │   [D]▮  (instant reversal, no stall)      │
│            click to reveal ▸               │   │                          ⚠  ( ) enable   │
└───────────────────────────────────────────┘   └───────────────────────────────────────────┘
        step 1: reveal (consent)                       step 2: toggle (enable)
```

### Snap Tap — the visual stage (what to draw)
A compact **counter-strafe lane**: a player marker `●` on a horizontal track, two keycaps `[A] [D]`.
Animated loop demonstrating last-input-priority:
- tap A → marker glides left; tap D → glides right.
- **the money moment:** hold A, then also press D → with Snap Tap the marker **snaps instantly** to
  D (crisp reversal, zero dead-zone). The "edge" is visually self-evident — the instant, stall-free
  direction flip. (Optionally flash a ghost of the *normal* behavior stalling, for contrast.)
This is the whole pitch, no text needed: "opposing inputs resolve to the last pressed → instant
counter-strafe."

### The reality checks baked in
- **Legality:** banned in CS2 (kick from Valve servers); allowed-but-anti-cheat-gray in Valorant
  (software SOCD is the riskier kind). The two-step consent + a small ⚠ accent carries this.
- **Device support:** Snap Tap is a Synapse-4-era firmware feature (BlackWidow V4 Pro/TKL, Huntsman
  V3). **Your BlackWidow Chroma V2 (2017) does not support it.** On an unsupported device the option
  shows but stays gated ("needs a supporting device") — honest, consistent with our other gates.
- **Bytes:** OpenRazer issue #2754 decoded it — SET class 0x02 / id 0x27, GET id 0xA7, data size
  0x0F, up to 4 key-pairs. **It HAS a getter (0xA7) → it's verify-gatable** like `set_idle_secs`.

### Code shape (when greenlit)
- `writes.rs`: `set_snap_tap(d, pairs, store)` — verify-gated (read back 0xA7), env-flagged
  `NEURON_SNAP_TAP_WRITE`, built from #2754's layout. SOCD key-pair config (default A/D).
- `state.slint` / `prefs.rs` / `glue.rs`: a `snap_tap` pref + the key-pair(s); the reveal/enable state.
- `panels/settings.slint`: a `MechAdvantage` component + the Snap Tap stage visual (procedural,
  `NotifStackMock`-style) in a new MECHANICAL ADVANTAGES section.

### Placement — SYSTEM page (resolved)
The user references "the stages for the notifications in System," so this is a SYSTEM-page category,
not a device-pane control. (It applies to the connected keyboard, like a device-affecting pref.)

---

## 6. What I can pre-build with NO hardware vs what needs YOU

**Pre-buildable now (pure UI / scaffolding, never fires to a device):**
- The MECHANICAL ADVANTAGES SYSTEM section + the `MechAdvantage` component + the two-step consent.
- The Snap Tap **stage visual** (procedural counter-strafe demo) — first pass to iterate on, like we
  did the notif stages.
- The `snap_tap` pref/state plumbing + a verify-gated, **env-flagged** `set_snap_tap` primitive
  (never fires without the flag; never claims success without the 0xA7 read-back).

**Needs YOU + the hardware:**
- Verifying wheel / sleep / in-game-polling round-trips on the Naga → dropping their env gates.
- The LOD getter hunt (razerctl) or USBPcap capture.
- A Snap-Tap-supporting keyboard to actually confirm Snap Tap (the Chroma V2 can't).

## 7. Recommended next-session order
1. **15-min hardware pass:** verify wheel + sleep + in-game-polling on the Naga; un-gate what confirms.
2. **LOD:** check razerctl for a getter → port verify-gated; else scope a capture. Add asymmetric.
3. **MECHANICAL ADVANTAGES:** build the SYSTEM section + the Snap Tap stage visual + consent gate
   (functions on a future V4 board; honest-gated on the Chroma V2).
