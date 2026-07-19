# Neuron Performance & Footprint Audit (2026-07-15)

Diagnosis-only pass: CPU, memory, active + background footprint, and input→action
latency. **No capabilities/features/mechanics are to be cut** — every item here is
tighter engineering (buffer reuse, blocking-vs-poll waits, index-vs-scan, ship flags),
not amputation. Findings come from a 7-agent parallel audit (4 internal code reads, 3
external research). Line numbers are as of this commit; re-locate before editing.

---

## 0. Headline

**The single highest-leverage fix solves the latency question and the idle-power
question at the same time.** Both trace to the same root: the input pumps are
`thread::sleep(N)` busy-poll loops instead of blocking waits on the OS message queue.

- Latency: every key/button waits ~2.5 ms avg / 5 ms worst before dispatch even looks
  at it (`controls.rs` 5 ms sleep). That is the *only* meaningful latency neuron adds
  on top of the OS — everything downstream is microseconds.
- Idle power: tray-hidden + idle, neuron issues **~552 wakeups/sec**, dominated by the
  weave idle-arm loop (~333 Hz, 3 ms sleep) and the dispatch pump (~200 Hz, 5 ms sleep).

Converting both loops to a blocking `MsgWaitForMultipleObjectsEx` wait (which winit's
own `ControlFlow::Wait` already does elsewhere in the app) **removes the added input
latency AND collapses idle wakeups toward zero** — one architectural change, two wins.

Everything else is real but secondary.

---

## 1. Input → action latency (the core question)

### Verdict: the architecture is at the user-mode floor. It is correct.
- Reading buttons via **Raw Input (`WM_INPUT`)** + reading the Razer vendor HID
  collection directly + emitting via **`SendInput`** + a **`WH_KEYBOARD_LL`** hook used
  *only* for Alt+Tab/Win/Alt+F4 suppression is the exact split Microsoft documents and
  the pattern PowerToys Keyboard Manager, AutoHotkey, and kanata/kmonad all use.
- MS docs, verbatim: *"In most cases… the application should monitor raw input instead"*
  of low-level hooks. Neuron already does. The LL hook is correctly reserved for the
  rare synchronous suppression case (where it is the *right* tool) and self-hosts its
  own pump thread so it never blocks the data path.
- A kernel driver (Interception-style) is the only thing "lower," and every credible
  source — including the driver author — says it buys **reach and pre-emption
  guarantees, not latency** (author never benchmarked it and expects it may be *slower*
  due to the kernel→user→kernel round trip). It is also closed-source, Secure-Boot-
  fragile, capped at 10 devices, and **launch-blocked by EasyAntiCheat / FACEIT /
  Vanguard**. Off the table for a device-button remapper. Do not pursue.
- `SendInput` has no faster documented user-mode alternative. The output side
  (`action.rs` press/hold/release) is already minimal and correct — one `SendInput` per
  logical action, true press-and-hold, no sleeps. Leave it.

### The real latency floor is USB polling, which you don't control.
125 Hz = 8 ms, 1000 Hz = 1 ms, 8000 Hz = 0.125 ms. Measured end-to-end, 8000 Hz vs
1000 Hz differs by only ~0.2 ms (igorslab LDAT). Software is a small fraction of total.

### What neuron actually adds on top of that floor (ranked):
1. **[P0] The 5 ms sleep-poll in the Raw-Input pump** — `controls.rs:1953-1954`
   (`win::listen`). `PeekMessageW` drains correctly (no event loss/coalescing), but the
   loop then unconditionally `sleep(5ms)`. Every single input waits ~2.5 ms avg here.
   Fix = block on `MsgWaitForMultipleObjects(QS_ALLINPUT + a wake handle)` instead of
   sleeping. This is the whole ballgame for latency.
2. **[P1] Blocking device/COM I/O on the pump thread** for two specific action kinds:
   - Sniper press/release — `dispatch.rs:893-977`: `io_gate_all()` parks lighting
     writers (~35 ms worst case, per the code's own comment) then does a **blocking HID
     read + write** synchronously on the dispatch thread. Stalls all other input for the
     duration (tens of ms on a wireless dongle).
   - Momentary-mic press/release — `dispatch.rs:845-871`: uncached Core-Audio COM
     `open`/`get_mute`/`set_mute` per press, inline on the pump thread. The tick-path
     mic-*tap* detector was already moved to a background cache for exactly this reason
     (`dispatch.rs:301-306`); the momentary path never got the same treatment.
   These are the only places an edge can *stall* the pump. Move the work off the pump
   thread (dedicated device/audio worker + the existing cache pattern).
3. **[P2] Redundant rule resolution** — a single Down edge re-runs `engine.resolve()`
   for the same trigger up to 4–5× (`hold_for_input` → `key_remap_for` → `fire` →
   `momentary_mic_for` → `sniper_dpi_for`), each a fresh `Vec<&Rule>` + linear O(rules)
   scan (`engine.rs:340-353`). Microseconds today, but it's a linear ceiling and
   needless. `Trigger` already derives `Hash+Eq`; add a `HashMap<(page,usage,pid), …>`
   index and resolve once per edge, reusing the result.
4. **[P2] Uncached Win32 metadata per event** — `controls.rs` `device_path()` /
   `preparsed_data()` / `pid_from_path()` do 2 syscalls + a `Vec<u16>`/`String` alloc
   **per event**, re-fetching per-device-constant data. Cache by device handle.

### Verify empirically
- **KB5028185** (Win11 22H2 22621.1992+) throttles/coalesces raw input to *background
  (non-focused)* listener processes. A global remapper is often "background." Log
  `WM_INPUT` timestamps foreground vs not and confirm neuron isn't being throttled.
- Confirm the Raw-Input registration uses `RIDEV_INPUTSINK` on a **dedicated
  message-only window/thread** not sharing a queue with heavy UI work.

---

## 2. Idle / background footprint

Tray-hidden, idle, nothing animating, host servers off: **~552 wakeups/sec**, almost
entirely two fixed-sleep loops:

| Driver | Rate | Root |
|---|---|---|
| Weave/cast idle-arm loop | ~333 Hz | `glyph.rs:1397` `sleep(3ms)` |
| Live-dispatch Raw-Input pump | ~200 Hz | `controls.rs:1954` `sleep(5ms)` |
| UI tick (Slint timer) | 16.7 Hz | `main.rs:429` (sub-work throttled 1–4 Hz) |
| Audio cache (real COM) | 2.5 Hz | `beacon.rs:1331` `sleep(400ms)` — deliberate, keeps COM off hot paths |
| App-focus poll | 0.83 Hz | rides the dispatch tick |
| Everything else | <0.1 Hz or kernel-blocked | negligible |

- **[P0] The two busy-poll loops** are the entire story. Both should block on an OS sync
  object (message queue / event), not `sleep`. HID reader threads already do this right
  (`windows_hid.rs` `WaitForSingleObject`, wakes only on data-or-timeout) — mirror that.
- **[P1] Two competing input pumps for the same class.** The weave idle-arm loop stands
  up *its own* hidden window + Raw-Input **mouse** registration (`glyph.rs:1103-1152`)
  that competes last-registrant-wins with the dispatch listener's registration. Two
  always-running message pumps for one input class — consolidate to one.
- **[verify] Vitals** looks **already fixed** vs the old "1 Hz wakes a sleeping mouse"
  note — current `vitals.rs` is pull-based (60/20/10/5 s battery-aware freshness, only
  reads off real HID activity) and explicitly documents "we never wake a sleeping mouse
  just to check." Confirm, then update the memory note.
- Lighting animation (`lighting.rs`, ≤30 fps, real HID writes/frame) runs while hidden
  **by design** — an animated effect must keep animating. Not idle cost; only present
  when the user selected an animated effect. Static effects collapse to ~zero traffic
  (row-dedup + heal-then-quiescent writer). Leave the mechanic; see §3 for its allocs.

### Also
- **[P1] EcoQoS + timer coalescing** for the surviving periodic threads: `SetWaitable
  TimerEx` with a non-zero `TolerableDelay` (≥50 ms) so the kernel coalesces wakes;
  `SetProcessInformation` EcoQoS on non-interactive helper threads when backgrounded (MS
  measured up to 90% CPU-power reduction). Never hold `timeBeginPeriod(1)` process-wide
  (~0.3 W idle cost) — scope it to an animation loop if ever needed.

---

## 3. Active hot-path CPU + allocation churn

**Correction to prior assumption:** the overlay is *not* a 0.5M-px/frame brute renderer
anymore — it was rewritten to a dirty-tile cover (`overlay.rs:491-614`); cost scales
with stroke geometry, normal frames paint hundreds–low-thousands of px. So the leverage
is **allocation churn, not per-pixel compute**, and **SIMD is low-value** at these
working-set sizes.

Ranked allocation targets (all "reuse a scratch buffer + `.clear()`" fixes):
1. **[P1] Lighting pipeline: ~6+ `Vec` allocs/frame** to move ~100–300 `Rgb` values,
   stacked across `Compositor::render` → `Field::render` → `CompositorContent::render`
   (`bridge.rs:78-89`) → `Arbiter::resolve` (`arbiter.rs:819-869`, two allocs) →
   `WriterCore::offer` `.to_vec()` (`writer.rs:160`). The `lighting.rs` comment claiming
   "only one unavoidable per-frame Vec" holds inside `Lights::animate` but **not** across
   the host bridge/arbiter/writer stack. Thread reusable scratch `Vec`s through, as
   `Lights::animate` already does. Cheap CPU, but the cleanest allocation-count win.
2. **[P1] Audio FFT scratch** — `audio_spectrum.rs:387-388`: two fresh `Vec<f32>` of
   `FFT_N=2048` allocated **every 16 ms tick** (~1 MB/s) for fixed-size buffers. Move to
   scratch fields on the already-stateful `Loudness` struct. **Cleanest one-line fix in
   the whole audit, zero logic risk.** (The FFT itself is fine — O(N log N), in-place.)
3. **[P2] Overlay `TileSet::dilated()`** — `overlay.rs:577-595`: fresh `Vec<bool>`
   (~2500) per dilation, ~6/frame at up to 60 Hz. Bookkeeping only, not payload.
4. **[P1, correctness-adjacent] cpal audio callback must be zero-alloc.** Allocators
   hold internal mutexes → unbounded stall → audible glitches (worsened by the callback's
   raised priority). Audit `sound.rs` for any alloc / `format!` / lock / unbounded
   channel in the callback; pre-size everything, use a lock-free ring for cross-thread.

The device HID write path (wire-lock + per-row buffers) is already dedup-bounded and
dwarfed by the ~1–2 ms USB round-trip it guards. Not a target.

---

## 4. Memory, binary size, startup (measured)

- `neuron-app.exe` = **61.8 MiB**, `neuron.exe` (CLI) = **19.2 MiB**.
- **CPython embed = 13.67 MiB slim** (already trimmed from 44 MiB by `build.rs`), baked
  into **both** binaries (~27 MiB duplicated on disk). **Confirmed lazy: zero idle RSS**
  — demand-paged, only faults in when a Python macro fires, and runs as a separate
  `python.exe` child even then. So CPython is a *binary-size* lever only. Levers: kill
  the cross-binary duplication (CLI could share/fetch rather than re-embed), or gate the
  embed behind a feature for size-sensitive builds.
- **[P1] femtovg GL context held for the whole tray-hidden lifetime.** Slint maintainer,
  verbatim: femtovg jumps **~165 MB the moment a window is realized and never shrinks
  back**; *"if keeping memory low when idle is crucial, use the winit backend with the
  software renderer."* Per-frame render is already correctly gated off — but the context
  itself is the RSS floor. Investigate: default the resident/lightweight window to the
  **software renderer**, reserve femtovg for paths that truly need GPU. (Overlay is
  already software.) Compiling both renderers costs binary size only, ~0 runtime RAM —
  keep both. **Biggest single idle-RSS lever.**
- **[P1] Ship-profile size flags unused.** A `[profile.release-size]` (opt-z/fat-LTO/
  cgu=1) exists in `Cargo.toml` but `release.ps1` never builds it. Realistic wins on the
  actual ship profile: `lto="fat"` + `codegen-units=1` + `strip="debuginfo"` (keep
  backtraces for the unwind-based RAII teardown), plus per-crate `opt-level=3` overrides
  pinned on the hot crates (tone/render/glyph) if the crate default drops to `s`/`z`.
  Measure with `cargo bloat --crates` + `cargo tree -e features` + `cargo-machete` first.
- **[P2] Startup does synchronous HID `transport::enumerate()`** on the critical path to
  tray-visible (`runtime.rs:280-288` via `glue::install`). Consider async/deferred so the
  icon appears instantly and devices populate behind it.
- **[P2] Eager window + GL context** built before `--tray` decides visibility
  (`main.rs:279`). Make window/GL creation lazy on first `show()`.
- **windows-sys** pulled at 6 versions transitively; own feature lists are large but
  pure FFI (mostly zero link cost). Low priority; enumerate features explicitly.

---

## 5. Strategic reframe — get the host out of the loop (OpenRazer model)

The vendor-software research points at the biggest *structural* footprint lever, beyond
any micro-opt: **persist static config (keymap, DPI stages, basic lighting) into the
device's on-board memory**, so the device does the right thing with the host process
*closed*. That is zero host latency, zero background cost — the exact split OpenRazer
uses and what SynapseKiller automates (load once, kill services, config persists).

- Every target vendor supports it: Razer "Hybrid on-board" (~4–5 slots), Logitech
  "On-Board Memory Mode" (macro-save reliability issues reported), Corsair "Hardware
  Profiles". neuron already has the gated device-writes (`writes.rs`: DPI, polling, idle,
  snap-tap) — this extends that philosophy to remaps.
- **Caveat (OpenRazer, confirmed):** do **not** stream dynamic per-frame effects into
  flash — it wears the memory controller (they removed that feature for this reason).
  Persist *static* config only; keep animation host-side.
- This doesn't remove features — it makes neuron optional at runtime for the
  deterministic subset, which is the strongest possible footprint story vs Synapse
  (~400 MB, 15–20 processes, multi-GB/week idle disk churn, mandatory account/telemetry).

---

## 6. Do NOT do these (researched placebos / traps)

- **Swapping the global allocator (mimalloc/jemalloc/snmalloc).** For a burst-then-idle
  resident app this is neutral-to-*harmful*: rustc and Polars both reverted mimalloc on
  Windows for RSS regressions; jemalloc isn't usable on MSVC; snmalloc forces static CRT
  (conflicts with the Slint/CPython stack). Stay on the system allocator. (Resolves the
  memory-audit's "no custom allocator" flag — that's correct, not a gap.) If ever tested,
  the *only* valid test is a 30–60 min idle soak after a burst, not a throughput bench.
- **`EmptyWorkingSet`/`SetProcessWorkingSetSize` on a timer.** Cosmetic — evicts pages to
  disk, doesn't free committed memory, and forces page-fault thrash on next touch. At
  most one call after the startup burst; never periodic.
- **`panic=abort` / `panic_immediate_abort`.** Off the table — the app deliberately uses
  `panic=unwind` for Drop-guard teardown; immediate-abort won't even link with unwind.
- **SmallVec by reflex** (slower per-op than `Vec::with_capacity`+reuse; benchmark or use
  ArrayVec for fixed bounds) and **UPX** (packed exes get AV/EDR-flagged — bad for a
  HID/input tool's reputation).
- **SIMD in the render/lighting/audio paths** — low leverage given the small working sets
  post-tile-fix; eliminate allocations first.

---

## 7. Suggested sequencing (when we move to implementation)

1. **P0 — blocking waits.** Convert `controls.rs` pump and `glyph.rs` idle-arm loop from
   `sleep`-poll to `MsgWaitForMultipleObjects`; consolidate the two input pumps. Fixes
   latency floor + idle power together. *Verify: input still fires, latency drops, idle
   wakeups fall, no missed events.*
2. **P1 — unblock the pump.** Move sniper HID and momentary-mic COM off the dispatch
   thread (worker + cache pattern).
3. **P1 — software renderer for the resident window** (or lazy femtovg). Measure idle RSS
   before/after with a real soak, not Task Manager.
4. **P1 — allocation reuse:** audio FFT scratch (one-liner) → lighting pipeline scratch →
   overlay TileSet. Audit the cpal callback for zero-alloc.
5. **P1 — ship flags:** fat LTO + cgu=1 + strip=debuginfo + per-crate opt overrides;
   `cargo bloat` pass.
6. **P2 — rule index** (resolve once per edge, HashMap by trigger), cache per-event Win32
   metadata, deferred startup HID enumerate, lazy window/GL, EcoQoS + timer coalescing.
7. **Strategic — on-board memory persistence** for static remaps (biggest footprint win;
   static only, never animation).

Empirical checks to run regardless: WPA power trace for idle wakeups; KB5028185
background-throttle test; RIDEV_INPUTSINK dedicated-window confirmation.

---

---

## 8. Follow-on: launch-state sync + the mic echo latch (2026-07-15/16)

Separate campaign that grew out of a user-reported bug (mic pill read "online" while the mic was
physically muted at launch; corrected only after a toggle). Root cause was launch ORDERING + gating,
not missing reads. Fixed at core by a **Reconciler** (Fable-5 design): separate resolve/record/render,
readiness barrier + mandatory timeout, ONE unconditional publisher per property, `Truth{Read/
Asserted/Unknown}` honesty. Shipped: Chunk A (pure scheduler, `neuron-app/src/reconcile.rs`, 10
tests) + Chunk B (mic vertical slice). **Still queued: C** (gaming-mode — its suppression policy is
never re-applied from the persisted active profile at launch, so Alt+Tab/Win suppression is silently
DEAD after every reboot — the most user-harmful item left), **D** (scroll-stage shows a fake
"tactile/free" literal; no getter exists → should be `Asserted`-or-dash), **E** (wake/hotplug: a
device asleep at launch never re-swept; route the existing wake signals into the reconcile queue).

### The echo latch is an INFERENCE that should be deleted, not tuned

`Trigger::MicTap` must fire on EXTERNAL mute changes but not on neuron's own writes (hardware
bridge / app toggle / momentary press+release / `Action::MicMute`). Today dispatch infers "was this
mine?" from a **~400ms-refreshed cached** boolean sampled every ~50ms. That channel structurally
destroys the answer, and it cost three real temporal bugs in three versions:

1. consumed on every poll → latch gone before the write surfaced → phantom tap.
2. value QUEUE + TTL → an unseen intermediate stayed eligible → **swallowed a genuine tap**.
3. current: **convergence model** — newest write SUPERSEDES (the cache reports CURRENT state, so only
   the latest write is what it converges to); sample != expectation = in-flight → suppress; sample ==
   expectation = converged → clear; 2s TTL if it never converges. CORRECT, but admits an irreducible
   ≤400ms ambiguity window (chose to suppress: a missed effect beats a phantom action).

**THE ROOT FIX (next branch): Core-Audio event-context GUID.** `IAudioEndpointVolume::SetMute` takes
`pguidEventContext`, and `IAudioEndpointVolumeCallback::OnNotify` hands that SAME guid back in
`AUDIO_VOLUME_NOTIFICATION_DATA` (with `bMuted`). Pass neuron's own GUID on every write → the
callback reports every change WITH its origin. Definitive, event-driven, zero inference. The guid is
propagated in the WASAPI/audio-service layer (it does NOT round-trip the driver — which is exactly
where we need it); other apps show `GUID_NULL` or their own → both ≠ ours.

- **Dies:** the latch (supersede/TTL/convergence), `mic_tap_decision`'s `echo` param, polling AS the
  tap-detection mechanism, `ctx.last_mute` (cache-sample stream), the ambiguity window.
- **Survives:** the 400ms cache (for its OTHER consumers — `mic_state`/miclight ~8Hz, volume, render
  endpoints); an edge cell REHOMED to the callback consumer (OnNotify fires for VOLUME too — diff
  `bMuted`); `MIC_TAP_BASELINE` as render dedupe; **the launch reconcile's direct read** — keep it:
  callbacks report CHANGES, so **one fresh direct read after every (re)registration** establishes
  initial truth and covers the pre-registration gap.
- **The bridge subtlety (get this right or the bug inverts):** a physical Seiren tap SHOULD fire
  MicTap, but the bridge's OS write is neuron's own and would be suppressed. Give the **bridge writer
  its OWN GUID** and classify per-writer: bridge-GUID → fire, other neuron GUIDs → suppress,
  foreign/NULL → fire. One classification point, no double-fire, per-writer GUIDs are free telemetry.
- **Risks to engineer against:** `OnNotify` arrives on an MMDevice COM worker thread — flag+post ONLY
  (never block, never call `IAudioEndpointVolume` from inside it = documented deadlock, never write
  mute from the notification path = reentrancy). Register/Unregister must pair (`windows-rs`
  `#[implement]` for refcounted IUnknown; unregister before dropping the endpoint). Default-device
  changes → `IMMNotificationClient::OnDefaultDeviceChanged`/`OnDeviceStateChanged` → re-resolve,
  re-register, immediate direct read (state may have moved while deaf). **No-op writes don't notify**
  — never await an echo. Register from the audio worker, NOT the STA main thread. Keep COM off the
  input-dispatch thread (an inline `get_mute()` once hung the whole dispatch loop — that's why the
  cache exists). Elevation is a non-issue (endpoint volume is per-session).
- Cheap honesty check (telemetry, not behavior): the surviving cache can LOG a mute flip that no
  notification explained.

### Commit hygiene (reviewer + Fable agree)
This working tree bundles four unrelated risk profiles: FFT alloc removal, the profiling harness, the
blocking Win32 input-pump rewrite, and mic reconciliation. Verification here is HAND-testing, so an
unsplittable commit means an unattributable regression. **Split before committing** — the pump
rewrite (hottest path) deserves its own commit and its own hand-test session. Also: `reconcile.rs`
is UNTRACKED while `main.rs` declares `mod reconcile` — it MUST be included or the tree won't build.

---

### Source integrity note
The external research discarded fabricated "software adds +410 ms / +8.7 ms per
keystroke" figures traced to AI content farms — there are **no** credible measured
"peripheral software adds N ms latency" numbers. Claims here rest on MS Learn, OpenRazer/
Polychromatic docs, vendor support pages, Slint maintainer statements, rustc/Polars issue
trackers, and named-engineer sources. "As low as possible in user mode" is a sourced
architectural conclusion, not a benchmarked latency delta.
