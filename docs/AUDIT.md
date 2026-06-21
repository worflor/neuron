# Neuron — Pre-1.0 Cleanliness & Maintenance Audit

*Principal-engineer review against the stated ethos: a unified, elegant, cross-platform Rust replacement for Razer Synapse — no pre-public legacy cruft, one idiom across the app, clean OS seams, hot-loop correctness. Read-only audit; every claim cited to code actually read. Six lenses, fanned out in parallel, each finding adversarially re-verified against the real tree before inclusion.*

*Map: 10 subsystems · 9 hot loops · 16 platform sites · 9 config files. Verified findings: 26 (high 1 · medium 6 · low 12 · nit 7).*

---

## 1. Executive summary

Neuron is genuinely good. The spine is the real thing: one `Trigger` enum, one `Engine`, one dispatch loop serving hotkeys, gestures, radial flicks, app-focus, mic-tap, and HyperShift layers without branchy special-casing — and the pure core (`engine.rs`, `tone.rs`, `glyph.rs`, `cast.rs`) is portable by construction. The audio path is already cross-platform via cpal. The overlay, the seqlock flight recorder, and the resolve pipeline are mature. This is a ~90%-done instrument, not a prototype, and the findings below are polish, not rescue.

Two honest gaps stand between this and a clean 1.0. **First, cross-platform is aspirational below the core.** The pure spine ports, but the *live driver* does not: the entire input/dispatch worker is `#[cfg(windows)]` with an inert non-Windows stub, and four window-management modules plus OS audio-control use three different ad-hoc cfg conventions instead of the trait seam the project already proves with `Transport`. The single largest porting tax is the Win32 layered-surface recipe, hand-rolled in 9 inline call sites across 4 files with no shared type. **Second, there is a small amount of textbook pre-public legacy cruft** — a `DEPRECATED kept so old TOMLs parse` field, dead back-compat wrappers, and `#[allow(dead_code)]`-silenced functions — exactly the bloat the ethos says to cut while no public configs exist to protect.

There is one real reliability finding worth fixing before 1.0: the dispatch status mutex uses bare `.unwrap()` while its own sibling statics are poison-tolerant, opening a narrow but nasty "alive but every dispatch panics silently" failure mode. Everything else is low/nit. Verdict: clean house, extract the surface seam, and ship.

---

## 2. Top fixes (highest leverage first)

1. **Extract one `LayeredSurface` Win32 type** — the WNDCLASS + `CreateDIBSection` + `UpdateLayeredWindow` recipe is duplicated across `overlay.rs`/`teleport.rs`/`whiteboard.rs`/`glance.rs` (13 `CreateDIBSection` sites in 4 files). This is *the* cross-platform seam that doesn't exist; collapsing it makes the entire overlay/marker/canvas surface a single file to port.
2. **Make the dispatch status mutex poison-tolerant** — `dispatch.rs:268,375,426,497,618,741,772` use bare `.unwrap()` while `LIVE_TX` (48) and `LAST_ACTION_DESC` (92) already use `unwrap_or_else(PoisonError::into_inner)`; a panic under a held status guard wedges the reopened listener into silent total failure.
3. **Throttle live glyph predict in the capture callback** — `beacon.rs:811-838` re-runs `analyze`+`predict` (fresh DTW matrix per template) every 60–125 Hz drain over a growing stroke; gate to ~50–80 ms / min arc-length, reuse the prior `GlyphHint`. Pure UI hint, no correctness change.
4. **Lift an `InputSource` trait under the live worker** — the whole event loop is `#[cfg(windows)] fn run_worker` (`dispatch.rs:252`) with an inert `cfg(not(windows))` `LiveRuntime::start` (183-193); today the resident app compiles but is *fully dead* off Windows.
5. **Delete the deprecated `straightness` cast field everywhere** — `cast.rs:45-48,134,159,384`, CLI echo `main.rs:1057,1062`, both `cast.toml` templates; `resolve()` never reads it (confirmed at `cast.rs:339-362`). Unknown keys still parse, so existing TOMLs are safe.

---

## 3. Per-lens findings

### Hot-loop performance & math

- **Live glyph recognition recomputed from scratch every drain** *(medium)* — `beacon.rs:811-838` calls `glyph::analyze` then `vault.predict` on every progress tick once `pts.len() >= 8`; `predict` runs DTW (fresh `(n+1)×(m+1)` matrix) per template over the whole growing stroke, at input rate. **Fix:** throttle to ~50–80 ms or a min added arc-length, reuse the last `GlyphHint`. It's a live hint, not the commit verdict.
- **Stroke heap-allocated twice per progress tick** *(low)* — `beacon.rs:800-808` builds a `Vec` via `iter().map().collect()` then `overlay.push(&rel)` does `to_vec()` again to cross the `Cmd` channel (`overlay.rs:612-614`). **Fix:** make `push` take an owned `Vec` and move `rel` straight into `Cmd::Push`; optionally recycle from the render thread.
- **`dbg_cover` full-tile scan computed every frame, used only when profiling** *(nit)* — `overlay.rs:2428` scans the whole 2500-entry tile-cover array unconditionally, but is read only inside the `NEURON_PROFILE`-gated `eprintln` at 2651-2652. (`dbg_painted` is *not* dead — it gates the slow-frame branch at 2638.) **Fix:** compute `dbg_cover` lazily inside the profile branch.

### Cross-platform architecture & seams

- **Live raw-input + dispatch worker is whole-function Windows-welded** *(medium — largest real gap)* — `run_worker` is `#[cfg(windows)]` (`dispatch.rs:252`); `LiveRuntime::start` returns an inert do-nothing runtime off Windows (`183-193`, `handle: None`). Portable loop logic (edge detection, turbo timing, intent routing) is interleaved with the Win32 pump. **Fix:** extract an `InputSource` trait (`next_event() -> Option<RawEvent>`, `RawEvent` already implied by `controls::ControlEvent` + the LL-hook key event) and lift the edge/turbo/routing loop into a platform-neutral fn.
- **Win32 layered surface hand-rolled with no shared seam** *(high — biggest porting tax)* — 13 `CreateDIBSection` sites across `whiteboard.rs`(4), `teleport.rs`(4), `overlay.rs`(3), `glance.rs`(2), each repeating the `WS_EX_LAYERED` + DIB + `UpdateLayeredWindow`/`BLENDFUNCTION` recipe; `glance` even re-declares its own `#[repr(C)] Blend` instead of `BLENDFUNCTION`. **Fix:** one `LayeredSurface` type (`new(class,w,h,ex_style)` + `present(pos,size,alpha)`) owning the boilerplate; that becomes the only file a non-Windows backend reimplements. (DWM-thumbnail host windows in `teleport.rs:677`, `glance.rs:1047` are out of scope.)
- **Transport trait leaks Windows UTF-16 paths into the portable layer** *(medium)* — `HidDeviceInfo.path: Vec<u16>` (`transport.rs:16`) and `open_path(&[u16])` (`:34`) thread a null-terminated wide string (consumed by `CreateFileW`) through `device.rs:18,38,52`, which has no other platform code. A hidraw/IOKit backend keys on a `CString`/`&str` and can't naturally produce a `Vec<u16>`. The line-16 comment already aspires to "platform-opaque handle key" — the type contradicts it. **Fix:** newtype `DevicePath(Vec<u8>)`/`OsString`, keep `Vec<u16>` inside `windows_hid.rs`, convert at the boundary.
- **OS window-management verbs are free-function cfg pairs, not a trait** *(medium)* — `wm.rs` is `#![cfg(windows)]` whole-file (`:13`); `glance.rs`/`teleport.rs`/`whiteboard.rs` use per-fn stub pairs. Portable selection/cycle/ordering logic is buried inside the Windows fn bodies. **Fix:** a `WindowManager` trait (summon/banish/pin/kill/tether/glance) with hwnd as an opaque associated type, portable list/cycle logic above it, one impl per OS.
- **OS audio control is hand-rolled COM behind free-fn stubs, not an `AudioControl` trait** *(low)* — `audio.rs` pairs a `#[cfg(windows)] mod win` (IMMDeviceEnumerator/IAudioEndpointVolume) with ~9 `cfg(not(windows))` per-verb stubs, while the adjacent *synthesis* path (cpal) is fully portable and proves the pattern. **Fix:** group verbs behind `AudioControl` (list_endpoints/volume_ctl/meter/set_default/flip_output); the ~9 stubs collapse to one inert impl.
- **Inconsistent platform-stub discipline; `glance::matches` has no non-Windows stub** *(low)* — three conventions for one concept (whole-file gate / re-export+partial stubs / per-fn pairs); `glance.rs:18` re-exports `matches` on Windows but the `cfg(not(windows))` block (20-33) stubs only `count`/`toggle`/`suggestions`. Compiles green today only because every caller is itself `cfg(windows)`-gated; the first off-Windows caller errors confusingly. **Fix:** fold into the `WindowManager` trait, or minimally add the missing `matches` stub.
- **`curtain.rs` painter seam is half cfg-gated** *(nit)* — the `Painter` alias and live `set_painter` are `#[cfg(windows)]` and `run()` is Windows-only, lacking the `mod imp`/`mod stub` split `overlay.rs` (324-348) established as the house pattern, despite the curtain's doc advertising portability. **Fix:** mirror overlay — neutral `CurtainFrame` + painter contract, Win32 body in `mod imp`, inert `mod stub`.

### Cohesion & idiom

- **Win32 layered surface duplicated 6×** — *(see Cross-platform "biggest porting tax" above; it is both a cohesion and a portability finding — merge there.)*
- **Config `save()` error types are a grab-bag** *(low)* — `bindings.rs:159` `std::io::Result`, `feel.rs:177` & `prefs.rs:155` `Result<(),String>`, `gesture.rs:46`/`profile.rs:86` `anyhow::Result` for one operation (serialize + write). No shared config error type. **Fix:** standardize on `Result<(),String>` (already most common, avoids leaking `anyhow` into the core's public API); convert bindings/gesture/profile.
- **`RuleDoc` on-disk shape declared 4×; `post_status` duplicated 2× verbatim** *(nit)* — identical `struct RuleDoc { rules: Vec<Rule> }` at `controls.rs:704`, `editor.rs:933`, `neuron-cli/main.rs:1838`, `migrate.rs:169` (comments admit they "mirror"); `post_status` byte-identical in `beacon.rs:1921` and `knockback.rs:102`. **Fix:** one `RuleSidecar` next to `Rule` in core; one shared `post_status`. (The ~140 `set_status_line` sites are *not* in scope — they're ordinary direct UI writes, not copies.)

### Bloat & legacy (pre-public — cut, don't preserve)

- **Deprecated `straightness` cast field** *(medium)* — `cast.rs:45-48,134,159,384`; resolve ignores it (`:337-338`, verified `:339-362`); only a CLI echo reads it (`main.rs:1057,1062`); still in both `cast.toml` templates. **Fix:** remove field, `d_straightness`, the Default literal, template lines, and the CLI echo. Parse-safe (no `deny_unknown_fields`).
- **Dead back-compat wrappers in `writes.rs`** *(low)* — `apply_dpi_stages` (`:242-247`) has zero callers (live `Runtime` calls `set_dpi_stages` directly); `apply_hypershift` (`:767-770`) forwards to a bail-only stub. **Fix:** delete both.
- **Build-warned dead fns** *(low)* — `overlay.rs:412 fill_all` (dropped as a perf fix per `:1463`) and `weave.rs:732 ridged` are the two outstanding dead-code warnings. **Fix:** delete both.
- **`knockback::active()` silenced, not deleted** *(low)* — uncalled `pub fn` under `#[allow(dead_code)]` (`knockback.rs:43-46`); live status uses `owned_vk()`. **Fix:** delete the fn and the allow.
- **Dead logos surface** *(low)* — `logos.rs:445 with_stride` has zero callers; `encode`/`decode` (`:506,:537`) are test-only (codec not yet backing `.gwyph`). **Fix:** cut `with_stride`; `cfg(test)`-gate `encode`/`decode` (they're honest forward-work).
- **Redundant `Rule::with_layer` builder** *(nit)* — `engine.rs:116`; zero call sites (verified — all `.with_layer(` hits resolve to `with_layer_rule`, a different used method); callers use `on_layer`. **Fix:** delete it.
- **`controls::media_for_usage` only its own tests call it** *(nit)* — `controls.rs:733`; references only at `:1278-1285` (tests). Doc admits it's unused by the daemon. **Fix:** cut it and its test.
- **Legacy `ichor` brush alias** *(nit)* — `whiteboard.rs:63` accepts `"ichor"` for back-compat; canonical is `"directed intent"` (`:55`); no TOML writes `ichor`. **Fix:** drop the alias arm.
- **`ScrollClick` "kept for round-trip" rationale is false** *(nit)* — the importer *drops* `ScrollClick` (`import.rs:828`), so it behaves as `Middle` everywhere and no round-trip is preserved. **Fix:** cut `ScrollClick`, map the external name to `Middle`.

### Reliability & correctness

- **Status mutex uses bare `.unwrap()` while siblings are poison-tolerant** *(high)* — all 7 `status.lock()` sites (`dispatch.rs:268,375,426,497,618,741,772`) use `.unwrap()`, but `status` is an `Arc<Mutex<LiveStatus>>` created *outside* the immortal `catch_unwind` listener loop. A panic under a held status guard (e.g. the guarded `fire_trigger` block or apply-profile) poisons the mutex; on reopen the first `.unwrap()` re-panics → caught → 250 ms sleep → reopen → re-panic. The listener stays alive but every dispatch silently fails — exactly the "visually works, nothing happens" failure the file's own comments (305-310) exist to prevent. Siblings `LIVE_TX` (51-53) and `LAST_ACTION_DESC` (95-97) already use `unwrap_or_else(PoisonError::into_inner)`. **Fix:** replace all 7 with `unwrap_or_else(std::sync::PoisonError::into_inner)`. `status` holds only display telemetry, so reading past a poison is harmless and strictly better than re-panicking.

### Config / TOML surface

- **Dead `straightness` re-serialized/templated/printed** — *(same field as the Bloat finding; merge. Net action: one deletion clears it from struct, Default, template, and CLI.)*
- **Live feel timing windows are hand-edit-only** *(low)* — `hold_ms`/`gap_ms`/`coyote_ms` (`feel.rs:124-141`, saved at `:177-181`) are consumed knobs (`glyph.rs:1236,1324`; beacon/whiteboard/`glue.rs:2617`), but the only GUI writer of `feel.toml` is the HyperShift stance picker (`glue.rs:2680`). **Fix:** add three sliders next to the stance control backed by one `set-feel-timing` callback mirroring `on_set_hypershift_mode` (load → mutate → save → `request_reload`).
- **`cast init` template omits live `activation` and `assist` keys** *(low)* — `TEMPLATE_TOML` (`cast.rs:366-414`) shows trigger/sectors/mode/deadzone/straightness but not `activation` (`:39`, sets the trigger rhythm) or `assist` (`:54`, spell-assist margin). A user never discovers them. **Fix:** add `activation = "hold"` and `assist = 0.0` with one-line comments. Combined with the `straightness` deletion, the template then exactly matches the live useful surface.

---

## 4. Cross-platform readiness scorecard

| Subsystem | Rating | The one change that unlocks portability |
|---|---|---|
| Engine / Trigger spine (`engine.rs`, `controls.rs` core) | **Ready** | Already pure & portable — leave it. |
| Audio synthesis (`tone.rs`, cpal output `sound.rs`) | **Ready** | Pure FM + cpal; the model to copy. |
| Resolve pipeline (`cast.rs`, `glyph.rs`, `radial.rs`) | **Ready** | Pure math; only `straightness` cleanup. |
| OS audio control (`audio.rs`) | **Needs a seam** | `AudioControl` trait; ~9 stubs → one inert impl. |
| HID transport (`transport.rs`, `windows_hid.rs`) | **Needs a seam** | Opaque `DevicePath` instead of `Vec<u16>`; then drop in hidraw. |
| Window mgmt (`wm.rs`, `glance.rs`, `teleport.rs`, `whiteboard.rs`) | **Windows-welded** | One `WindowManager` trait + portable cycle/order logic above it. |
| Layered overlay surface (overlay/teleport/whiteboard/glance) | **Windows-welded** | Extract `LayeredSurface`; it becomes the single port target. |
| Curtain (`curtain.rs`) | **Needs a seam** | `mod imp`/`mod stub` split like overlay; neutral `CurtainFrame`. |
| Live input + dispatch worker (`dispatch.rs run_worker`) | **Windows-welded** | `InputSource` trait + platform-neutral edge/turbo/routing loop. |

Reading: the *core* is ready; the *driver and surfaces* are welded. The five welded/seam rows all share one remedy shape — a minimal trait with the portable logic hoisted above it — and `Transport`/cpal already prove the house can do it.

---

## 5. Config / TOML assessment

| File | Completeness | Gap |
|---|---|---|
| `cast.toml` | Good | Carries dead `straightness`; template omits live `activation` + `assist`. |
| `feel.toml` | Persistence complete | `hold_ms`/`gap_ms`/`coyote_ms` GUI-unreachable (hand-edit only). |
| `bindings.toml` | Good | `save()` returns `std::io::Result` — odd one out. |
| `apps.toml` | Good | None found. |
| `gestures.json` | Good | `save()` returns `anyhow::Result` (leaks `anyhow` to public API). |
| `app.toml` (prefs) | Good | `Result<(),String>` — the convention to standardize on. |
| `profiles/*.rules.toml` | Good | Schema (`RuleDoc`) declared 4× independently — drift risk. |

**Cosmetic before/after — `cast.toml` template:**

```diff
 trigger = 0x05
 sectors = 8
 mode = "auto"
 deadzone = 40.0
-straightness = 0.85
+activation = "hold"   # trigger rhythm: "hold" | "tap hold" | "tap tap hold" …
+assist = 0.0          # spell-assist margin (0 = off; snaps a near-miss to a clear winner)
```

One deletion + two additions makes the generated file exactly mirror the live, meaningful surface — discoverability restored, dead knob gone.

---

## 6. What's already clean (do not touch)

- **The spine.** One `Trigger` enum, one `Engine`, one dispatch loop serving every input source with no per-source branching — `engine.rs` is pure and portable, and the injected-trigger path (`dispatch.rs:77-88`) routes synthetic and hardware events through the identical pipeline (HyperShift, intents, turbo, SAFE-gate all compose). This is the architecture the whole app earns its elegance from.
- **The poison-tolerant statics and immortal listener.** `LIVE_TX` and `LAST_ACTION_DESC` already do crash-resilience correctly (`unwrap_or_else(PoisonError::into_inner)`), and the `catch_unwind` reopen loop with explicit design comments (305-310) shows real reliability thinking — the status finding is just the one site that didn't get the memo.
- **Audio.** The cpal synthesis path is genuinely cross-platform and is the template the OS-control seam should imitate.
- **Resolve / intent math.** `cast::resolve` reading arc-recency `intent_vector` + net-displacement commit gate (`cast.rs:339-362`) — the "predict intent, snap only when unambiguous, never fight a deliberate miss" model — is thoughtful and the deprecation of `straightness` was the *right* call; only the field's corpse remains.
- **The existing seams.** `Transport` (despite the `Vec<u16>` leak) and `overlay.rs`'s `mod imp`/`mod stub` split are exactly the pattern the remaining welded subsystems should adopt — the house already knows how; it just hasn't applied it uniformly.

---

*Bottom line: a strong, coherent instrument. Fix the one reliability cut (poison-tolerant status), cut the small legacy corpse pile, and treat cross-platform as "extract five trait seams the codebase already knows how to write" rather than a rewrite. None of this is architectural — it's the final mile.*
