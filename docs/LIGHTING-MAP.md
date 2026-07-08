> **🤖 agent-generated · live context doc**
> *not official docs.* an LLM wrote this while building neuron. it may be
> stale, wrong, or slop — or it may be load-bearing and exactly right.
> code is the source of truth; verify before you lean on it.
>
> **kind:** as-built architecture map (four-sweep audit) · **as of:** 2026-07-02 · **trust:** high — every claim was cited to live code at the time; §5 honestly lists the seams still open

# The Lighting System — Map & Inspection

A rigorous end-to-end map of every path by which bytes reach a device's LEDs, how the Razer
eras are contained, where the seams were, what was fixed (2026-07-02), and what remains flagged.
Produced by a four-sweep audit (core wire layer / app flows / host stack / rogue writers) with
every claim cited to code at the time of writing.

## 1. The pipes (top → wire)

```
                     ┌──────────────────────────────────────────────────────────┐
   GUI edits         │  Vec<LayerDef>  (the ONE lighting representation:         │
   profiles/prefs ──▶│  pattern × spectrum × blend × region — GUI edits it,      │
   .ChromaEffects    │  profiles carry it, app.toml persists it)                 │
                     └────────────────────────────┬─────────────────────────────┘
                                                  │ runtime::start_layers
                              host active? ───────┤  (local stream stopped FIRST, always)
                          ┌── yes ────────────────┴──────────────── no ──┐
                          ▼                                              ▼
              host::set_lighting (BASE band, Pinned)          Lights::animate thread
                          │                                              │
        Chroma HTTP ──▶ ARBITER (bands: BASE < AMBIENT <                 │
        OpenRGB TCP ──▶ SESSION < OVERRIDE; leases: Pinned/TTL)          │
                          │ resolve() = topmost-wins per cell            │
                          ▼                                              │
              Writer thread (1 per device)                               │
              pace (mirrored math, parity-locked) + frame dedup          │
              + HealPolicy self-heal repaint                             │
                          ▼                                              ▼
              HidSink: row dedup ──────────────▶ row_report / custom_display_report
              (2ms legacy row breathe)                    │  (the LightingDef translation layer:
                                                          │   ALL era knowledge lives here)
                                                          ▼
                                            Device::send_lighting_fast
                                            (SetFeature + drain; wired: no gap;
                                             Naga dongle: 31ms stream_wait_us)
```

Side pipes (ACK'd, on-demand — `Device::apply_lighting`, 10ms-poll exec):
- **Named hardware effect** — `Lights::set_effect` → `native_effect_report`. Errors honestly when
  the effect is neither native nor faithfully emulatable.
- **Custom-frame paint** — `Lights::paint/paint_frame` → per-row ACK'd writes (CLI mirror,
  keytest, emulated effects). This path's 10ms-per-report poll is what spawned the "legacy boards
  cap at ~6fps" folklore, falsified by `device::tests::live_stream_strategy_probe` (30fps clean).
- **Brightness** — `brightness_report` → ACK'd. No era branch.
- **Driver mode** — `Lights::ensure_control` (memoized per handle) — now guaranteed by EVERY
  render path itself, because outside driver mode the firmware ACKs and silently ignores writes.

## 2. Era containment (Legacy 0x03 vs Matrix 0x0F)

All era knowledge lives in **four `LightingDef` methods** — the translation layer:

| Site | What differs |
|---|---|
| `native_effect_report` | Matrix `[varstore, led] + id (+ 00 00 01 + RGB)`, size derived; Legacy per-effect arg layout + FIXED per-effect data_size; **persist/varstore now owned here too** (was leaked into `set_effect`) |
| `row_report` | Legacy fixed `data_size 0x46`; Matrix derived |
| `custom_display_report` | Legacy `[custom_id, varstore]`, size 0x02; Matrix `effect.args + custom_id` |
| `brightness_report` | (no branch — shared) |

Class bytes (0x03/0x0F) and tx cohorts (Chroma V2 lighting = 0x3F, default 0x1F) come from the
device TOMLs, never hardcoded. `Lights`, `animate`, the host writer, and the app are all
era-blind. Legacy's ONE behavioral quirk in the stream path is the sink's 2ms per-row breathe
(burst-drop guard) — **not** an fps cap.

## 3. Rate discipline — one constant

`neuron::lighting::MAX_STREAM_FPS = 30` is THE ceiling, clamped identically at every layer:
`Lights::animate`, `CompositorContent` render quantization, `Bridge::set_fps`, app
`host::set_lighting`, GUI slider. The host writer (pure-std kernel, can't import neuron) mirrors
it as `writer::MAX_WRITER_FPS` together with the pure `pace()` math — both **parity-locked** by
`bridge::tests::writer_mirrors_neuron_cores_ceiling_and_pacing`. Render clock: everything derives
from the process-global `render_epoch` through `quantized_t`, which is why the GUI preview
provably matches the board.

## 4. Fixed in this inspection (2026-07-02)

1. **Double-writer on host transitions** — `start_layers` now stops (and removes) the board's
   local stream BEFORE the host branch, so toggling the host on mid-stream can't leave the old
   anim thread fighting the host writer.
2. **Silent no-op trap** — `ensure_control` is memoized and called by every render path
   (`set_effect`, `paint_px`, `animate`); "caller must remember driver mode" is gone.
3. **Honest effects** — `set_effect` errors on an unavailable effect instead of painting a wrong
   static fill; Starlight is no longer advertised as emulatable (its emulation was a flat fill).
4. **Era leak closed** — persist/varstore folded into `native_effect_report`; no caller pokes
   protocol bytes anymore.
5. **Stuck-lighting hole** — the OpenRGB pump wraps its read loop in `catch_unwind` so a panicking
   packet handler still releases the session's PINNED claims.
6. **One fps clamp domain** — `MAX_STREAM_FPS` everywhere (writer was 1..=60 while content
   quantized at 30: pure kernel-resolve churn); writer pacing now the same math as core's tested
   `pace` (its inline fork discarded all cadence phase on overrun).
7. **Folklore purge** — the falsified "legacy ~6fps" claim removed from the bridge module doc,
   the vitals-surface justification, and the probe blurb; the arbiter's "resolve is pure /
   deterministic" doc now states the Live-content exception honestly; journal.rs mojibake fixed.
8. **Host ownership ELECTION** — a rival neuron process could previously slip past "port taken by
   Synapse or a second instance" and attach its own bridge + writers anyway, because a taken
   protocol port (54235/6742) was the only signal, and Synapse squatting one is the NORMAL hostile
   environment, not proof another host is running. A dedicated machine-wide election
   (`neuron_host::net::HostLock`, loopback `127.0.0.1:47615`, held for the process lifetime — never
   accepted on, holding the bind IS the lock) is now acquired FIRST in `bring_up`, before the
   bridge attaches or any writer spawns; losing the election disables the host outright. Protocol
   ports keep their old, honest per-adapter meaning: a squatter there degrades only that adapter.
9. **Per-physical-device surface identity** — `bridge::discover` used to key surfaces by
   `codename-{pid:04x}` alone and silently drop every device past the first with the same
   (codename, pid) — two identical keyboards collapsed into one. `discover_from` identifies
   physical devices by `path_instance` (now in `neuron::transport` — a heuristic over the
   Windows HID path that collapses one device's several collections but keeps distinct container
   ids apart), and any (codename, pid) seen more than once gets every one of its surfaces a
   hash-disambiguated key instead of the bare one. The app's device model is UNIT-keyed too:
   `scan_devices` emits one row per physical unit (`DeviceState.instance` = `path_instance`,
   `DeviceRow.id` carries it), selection pins `(selected_pid, selected_unit)`, `open_selected`/
   `read_device_state` open the exact unit's control path, and lighting streams (`anim`, host
   base layers) are per-unit — `host::set_lighting/clear_lighting/set_fps/has_lighting/
   board_owner` resolve a named unit through `Bridge::key_for_unit` to exactly its surface.
   `Bridge::keys_for_pid` remains for pid-level ops with no unit in hand (per-model config like
   a persisted stack or a profile), which fan out to every unit of the pid explicitly.

## 5. FLAGGED — known seams, in priority order

🔴 **Control-plane writes race the stream on a second HID handle.** The wire protocol is
SetFeature→GetFeature pairs on ONE firmware control pipe; two handles interleaving pairs can
cross-read replies (`send_lighting_fast` drains blindly). Offenders, all opening their own handle
while the host writer streams: `runtime::apply_effect`/`apply_brightness` (no `host::active()`
guard — unlike `start_layers`), the macro `brightness` verb, the `macrokeys` driver-mode
re-assert thread, profile-apply's brightness write, and the entire CLI cross-process. The ACK'd
path's class/id echo filter + re-arm gives partial protection; the fast path has none.
*Fix shape:* route one-shot control writes through the device's writer (a control queue on the
sink), or a per-DevicePath wire mutex in neuron-core (solves in-process; document the CLI case).

🔴 **Host on/off transitions and profile apply are single-board.** Only the SELECTED board is
re-seated: host-off strands other boards frozen; host-on used to double-write them (the selected
board's case is fixed, the others migrate only when next applied); profile apply hand-rolls a
stop loop (neither clears host bases nor restarts non-selected boards → their lighting goes dark).
*Fix shape:* iterate every persisted `[lighting.*]` stack (not the selection) on host toggle,
restore, and profile apply — one shared "re-seat all boards" helper replacing the three copies.

✅ **FIXED — Chroma custom effects no longer black out the base.** Chroma CUSTOM frames still
decode zeros as BLACK, but the paint-policy rework's merge-mode black rule (`paint.rs::merge_cells`)
now treats a painted-black cell as transparent in every non-Over blend mode — the same rule both
protocol families obey — and the Chroma REST server's default policy blend is `Screen`, not `Over`,
so out of the box a game lighting 3 keys no longer covers the user's animated base at SESSION band.

🟡 **Sink latch-on-release shows a dead session's last frame when no base exists.** The
"all-None → leave the silicon latched" rule means teardown only LOOKS clean when a base layer
repaints. Visually indistinguishable from the stuck-lighting bug in the no-base case.

🟡 **HealPolicy is tick-based** — self-heal wall-time scales with 1/fps (0.4s at 30fps, ~12s at
1fps). Consider time-based scheduling.

🟡 **Replay determinism vs preview parity** — `CompositorContent` ignores the injected `now`
(reads the global render epoch) by design; now documented in the arbiter header. A real fix is an
injectable epoch, only worth it if capture/replay becomes a used workflow.

🟡 **Device-TOML dead weight** — lighting `CommandSpec.size` is load-bearing ONLY for legacy
`custom_frame` (0x46); every other `size` value is ignored (data_size derived). `led_id` is never
read. Either document per-field or delete the inert values. Also: rows wider than ~25 LEDs would
silently truncate at the 80-byte report body (no guard; current boards max 22 cols).

🟡 **Replug on the same pid doesn't resume a dead stream** until the device is re-selected;
Chroma's server mutex is held across kernel round-trips (all Chroma clients stall behind a kernel
rebirth); Chroma stored-effects grow unbounded within a session's 15s lease.

## 6. Probes & guards to keep using

- `device::tests::live_stream_strategy_probe` — the wire-truth tool: measures per-report cost
  under 4 disciplines on real hardware. Run it BEFORE believing any "the board can't do X" claim.
- `bridge::tests::writer_mirrors_neuron_cores_ceiling_and_pacing` — the mirror lock.
- The `lighting_bench` harness and the host's journal/bus for behavioral evidence.
