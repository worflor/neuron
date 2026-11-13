# Palettes: saving, sharing, serialization

Status: design in flight, not built. Everything below is a target or a finding, not a
shipped capability. Line references are to `main` at commit `8fc580c` with a dirty
working tree; nothing was compiled or run when this was written.

## Scope

One palette system, built whole: a palette can be saved, named, shared with another
person and round-tripped through serialization without loss. It is not shipped in
slices that each leave a different half of the story out.

The existing user model stays: effect + spectrum = layer, then stack composition. A
saved palette is a spectrum (or a set of spectrum stops) with an identity. It does not
become a fourth lighting concept next to effect, spectrum and layer.

## What already exists (verified by reading the code)

- **Sparse profiles.** Every profile setting is an `Option`, and empty lighting does not
  touch lighting (`profile.rs:24-40`). A palette system does not need overlay profiles;
  the sparseness is already there.
- **Dump/apply.** `neuron dump` and `neuron apply --dry-run` report sections as
  unchanged, updated or would-update; apply refuses RAW and shell actions without
  `--allow-code`; preferences are dumped with secrets removed (CLI.md:288-306). Palette
  sharing should extend this path, not add a second import route. Only a GUI wrapper
  and stripping serials and paths from bundles are new.
- **Layer spectrum is per layer.** `LayerDef.spectrum` carries the colour source for one
  layer. There is no named palette token that two layers can reference, so a colour
  change today means editing each layer.
- **Roll already takes a library.** `roll::Mode` is Around, Fresh, Mine, Wildcard
  (`assist/roll.rs:15-25`); Mine passes the person's own palettes as `Roll.library`
  (`roll.rs:89`, `:210`). Saved palettes are the natural source for that library.
- **Region is already an arbitrary cell list** (`pattern.rs:409`). Only the GUI drag is
  rectangular.

## Decisions to make before building

1. **Named palette token or copy.** Layers either reference a named palette (a change
   updates every layer that uses it) or keep a copy of its stops. The token needs a
   schema change to `LayerDef.spectrum` that ripples into dump/apply and the GUI. Copy
   is simpler and loses the "change one colour, retheme the stack" behaviour. This must
   be settled first because serialization, sharing and diffing all depend on it.
2. **Identity and collisions.** What a palette is keyed by (name, content hash, both),
   and what apply does when an incoming name already exists with different stops.
3. **Shareable form.** A bundle containing palettes alone, or palettes plus the saved
   `effects/*.toml` that use them. The bundle must carry no serials, paths or secrets,
   and importing must show a diff before it writes.
4. **Colour space on disk.** Stops are authored and compared in OKLab/OKLCh (Luma-T
   works there). The file stores one canonical form so a round trip is byte-stable.

## Palette sources worth wiring in

- **Luma-T "screen" roll mode.** A new `Mode::Screen` that anchors on dominant colours
  pulled from `screen_ambient::grid()` (`screen_ambient.rs:160`) and passes them as
  `Roll.library`, the way Mine does. The extraction is a pure clustering function with a
  fixed initialisation, so it is deterministic and testable on synthetic grids. Open
  check: whether the ambient grid resolution is enough for good dominant colours.
  Files: `assist/roll.rs`, a new pure helper in `assist`, `glue/colorsense.rs`, the roll
  drawer, and a frame in `apptest.rs`/`uifit`. Size S-M.
- **Screen eyedropper.** A single-pixel sample is a separate overlay from the grid
  extraction above. Not committed here.

## Testing the system deterministically

- Serialize, parse, serialize again: the two outputs are byte-identical.
- Golden files for a handful of palettes, including stops outside sRGB gamut.
- Import diff tables: unchanged, added, renamed, conflicting.
- Old files with no palette section load unchanged (serde defaults, skip-if-none).
- Bundles from `dump` contain no serial, path or secret.
- Seeded `roll` goldens for any new mode (rolls are already seed-deterministic).

## Triage of the wider idea list

A read-only review of ~35 candidate features against the code produced the following.
It is kept here as the record of what was weighed and why.

### Do next, in build order (separate from the palette system)

**Bundle: trigger expansion.** Picks 1 to 3 share one change set: `engine.rs` (Trigger,
`describe`, `matches`), `authoring.rs` (`parse_trigger` near :1887, validation near
:2123, examples near :1727), the trigger-kind labels at `neuron-app/src/glue.rs:8822`
and `runtime.rs:1977`, the trigger table in GDD.md, and CLI.md. Producers use the
existing `crate::dispatch::inject_trigger` (as GameLight does, `chroma_lab.rs:553`), so
there is no second dispatch path.

1. **Window-title predicate on `AppFocus`** (S-M). `AppFocus { app }` matches the exe
   substring only (`engine.rs:57`, `:437`); `Context.window_title` already exists
   (`macros/context.rs:39`). Add `title: Option<String>` with serde default and
   skip-if-none. The probe in `app_focus.rs` returns (exe, title). Add an optional
   `title` to `AppRule` (`profile.rs:1133`) and fix the struct patterns at
   `dispatch.rs:1207`, `neuron-cli/src/main.rs:3568`, `controls.rs:2788` and
   `executor.rs:307`. Tests: `matches()` tables, `poll_with` with an injected closure,
   serde round trip of an old rule, parsing `app:chrome title:Meet`. Profile routing
   must switch only when the resolved rule changes, not on every title change.
2. **Battery-edge trigger** (S core, M overall). The low/critical/charging/full edges
   and their toasts already exist (`vitals.rs:24` `WARN_AT=[20,10,5,2]`, `observe` at
   :99). Expose them as `Trigger::Battery { pid: Option<CanonicalPid>, edge: Low |
   Critical | Charging | OnBattery | Full }`. Discrete edges keep `Trigger`'s derived
   `Eq + Hash` (`engine.rs:32`). Callers: `hidwatch.rs:594`/`:970`,
   `runtime.rs:1946`/`:2388`. Battery to `LightingLayer` gives a software-only "while
   charging" look and covers the safe half of the planned charging row in STATUS; the
   dock override stays blocked on hardware.
3. **Bus-signal trigger** (M, after 1 and 2). The bus retains values
   (`neuron-host/src/bus.rs`), but the only producers are `obs.*` (`net.rs:713-822`) and
   `host.chroma.*`/`host.openrgb.*`. `gpu.temp` and `cs2.*` are doc-comment examples
   (`bus.rs:7`, `lib.rs:20`). A threshold cannot be an `f64` under `Eq + Hash`; use a
   discrete test enum (`Is(bool)`, `Equals(String)`, `AboveMilli(i64)`,
   `BelowMilli(i64)`) that fires on the edge into true, with a pure evaluator in core.
   neuron-core cannot depend on neuron-host, so the app converts values (its subscriber
   is at `neuron-app/src/host.rs:218`). GUI only: the CLI daemon has no host.
4. **Semantic key-zone placements** (S, independent). WASD, F-row, numpad, arrows,
   modifiers as a pure function from zone name to `Vec<u32>` over `razer_key_cell`
   (`lighting.rs:569`), a CLI `--zone` beside `--region`/`--rect` (CLI.md:216), and a
   chip row in the placement drawer. `razer_key_cell` describes the 6x22 BlackWidow
   matrix only (`lighting.rs:564`): offer zones on boards of that shape only. The Naga
   thumb grid is not a key-cell map.
5. **Luma-T screen roll mode.** See above.
6. **Spectrum bars as a `meter` style** (M). `audio_spectrum.rs` computes 24 log bands
   but publishes 4 regions plus a tone value (`:126-140`, `:205`). Publish per-band
   levels, add `style: bars` on `meter` (`pattern.rs:695`) with a pure painter
   reusing `meter_step_bar` (`:3344`). How it looks on a keyboard is unverified until
   run on hardware.
7. **Bus source on the existing `signal` pattern**, after pick 3. Add a
   `source = "<bus path>"` parameter rather than a new pattern. Today `signal` reads
   only the four macro-set channels (`pattern.rs:3838`, `lighting.rs:1157`). M.

### Sound, blocked

- **Per-key tap/hold/double-tap rule property.** Smart stance already does tap vs hold
  for HyperShift layers (`controls.rs:1567-1680`), but not per rule. Deciding tap vs
  hold means holding a key back and replaying it, which touches HID interception and
  input synthesis: risky work that needs the owner first (AGENTS.md).
- **Macro Workshop execution trace.** A mock/dry-run path exists (`macro_host.rs:1408`)
  and the panel has a log (`bindings.slint:1495`). Python-bridge work, so risky.
- **More `ctx` fields** (`side_plate`, `monitor`, `idle_ms`) and **`neuron.after`
  timers**: Python bridge, risky. `neuron.active_profile()` and `neuron.battery()`
  already exist (MACROS.md:224-245; `macro_host.rs:2692`, `:2702`).
- **`neuron.bus_get` / `neuron.bus_publish`.** After the bus-signal trigger, through a
  sink in core like `obs_hook.rs`. Publishing must be confined to a `macro.*`
  namespace so a BOUND macro cannot fake `host.*` or `obs.*`.
- **Valve GSI adapter.** A deterministic parser with fixtures; a loopback HTTP listener
  exists for Chroma. PROTOCOL-HOST.md:144 asks for a concrete consumer first, which the
  bus-signal trigger would be.
- **Knockback audio duet.** Planned in the `tone.rs:5` header. The voice mapping is
  pure and WAV goldens are possible; what it sounds like must be judged by ear.
- **Pocket / transform quick-pick wheel.** Reuses the beacon choose wheel; overlay work
  that cannot be verified headless.
- **Idle / lock fade.** The state machine is testable; session-lock and idle detection
  are Win32, and it overlaps the firmware idle-off (`profile.rs` `idle_secs`, gated
  behind `idle-power-write`).
- **cm/360 / eDPI converter.** Pure and S, but UI scope. Only as one inline readout.
- **Nested radial rings.** Geometry is pure (`radial.rs:117-142`), but `cast.toml` and
  the overlay change, and STATUS grades spellweaving at scale as under-tested.
- **Charging / dock override.** Awaiting the owner's dock hardware.
- **HID++ receiver slot enumeration.** Premise true (`hidpp.rs:87-116`); new HID++ 1.0
  requests need ledger entries and there is no Logitech hardware to verify against.
- Analog curves and continuous axis bindings (risk of a second continuous dispatch
  path; needs pad hardware), gyro (GameInput motion only), polling-rate readout (OS
  timing is not deterministic), live report inspector in the GUI.

### Skip, already done, or in conflict

- **Side-plate trigger:** rejected by design; a seated plate is a latched state, not an
  event (`engine.rs:192-200`, GDD:153).
- **Composable overlay profiles:** profiles are already sparse; always-live `rules` keep
  base binds across profiles (CLI.md:290).
- **Shareable bundle with diff preview:** mostly done by dump/apply (see above).
- **Dynamic macro-filled radial:** a sector can run a macro that calls `neuron.choose`
  (MACROS.md:117-147); it is a two-step flick today.
- **Low-battery toast:** done (`vitals.rs:21-27`).
- **Passive report inspector:** the core exists as a consent-gated passive capture in the
  Compatibility Lab (`compat.rs:6`, `:313`, COMPATIBILITY-LAB.md:110).
- **Glance pin-to-corner:** `Action::Pin` (always on top) exists; a DWM picture-in-picture
  window is new Win32 overlay work that cannot be tested headless.
- **Window layout snapshots, WLED/MQTT, MIDI, SMTC/GPU temperature, Windows Dynamic
  Lighting (LampArray), multi-device desk map:** need hardware, new dependencies or
  WinRT, and visual checks; size L. PROTOCOL-HOST.md:141-150 lists the protocol ideas as
  "ideas, not commitments".
- **OpenRGB client:** the adapter is server only (`openrgb.rs:5`, PROTOCOL-HOST.md:26,
  :141). Needs other-brand hardware to verify. L.

### Premises in the original idea list that were wrong

- `cs2.health` and `gpu.temp` are doc examples; real bus producers are `obs.*` and
  `host.*`.
- `signal` reads four macro-set channels, not bus paths.
- `neuron.active_profile()` and `neuron.battery()` already exist.
- `region` is an arbitrary cell list; only the GUI drag is rectangular.
- Profiles are sparse; switching does not replace a full bundle.
- `audio_spectrum` publishes 4 region levels plus a tone value, not FFT bands.
- Battery threshold toasts exist.
- A side-plate trigger was rejected on purpose.
- Smart stance already has tap vs hold for layers.
- Bundle export exists as dump/apply.
- Dynamic radial is covered by `neuron.choose`.
- A float comparison in a trigger is incompatible with `Trigger`'s derived `Eq + Hash`
  without a discrete encoding.

### Open product decisions from the triage

1. **What may a signal trigger run?** Default: any action for local OBS and `host.*`
   paths; feedback-only (the GameLight rule, `action.rs:812`) for any future
   network-fed adapter.
2. **Should a title change re-route `apps.toml` profiles, or only fire `AppFocus`
   binds?** Default: route only when the matching rule changes.
3. **Shared palettes: token or copy?** Now part of this brief (decision 1 above).
4. **Are spectrum bars and a cm/360 readout earned under "more, never bloat"?** Default:
   bars as a `meter` style, no new pattern; the converter only as one inline readout.
5. **Allow risky input-hook work for per-key tap/hold?** Default: not now.

## Unverified

- Whether every non-6x22 board gets wrong cells from `razer_key_cell`.
- Whether the `screen_ambient` grid resolution is enough for good dominant colours.
- The GUI rule-editor file for the title field (probably `bindings.slint` plus
  `glue.rs`).
- Line numbers may have moved if the dirty working tree changes the files cited.
