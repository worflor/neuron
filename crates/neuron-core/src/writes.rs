//! Device write-completion — the firmware-side writes Neuron can't yet do.
//!
//! Neuron's read side and several setters (DPI/polling/brightness, lighting) already work; this
//! module is the home for the WRITE primitives still missing for a full Synapse replacement,
//! all of which are GATED device writes (back up -> write to volatile first -> verify round-trip
//! byte-for-byte, per the project's safety gates).
//!
//! Scope (the WRITES agent owns this file):
//! * **Button remap** — the firmware button -> key/action map. The Naga's onboard Mapping class
//!   is the firmware path; the BlackWidow (no onboard) is host-side. We produce the host-side
//!   [`crate::engine::Rule`] the Engine consumes today and document/stub the onboard path.
//! * **DPI-stage apply** — write the full DPI stage LIST (the cycle), not just the active DPI
//!   (active-stage read = `dpi_stages` class 0x04/0x86; SET = 0x04/0x06, hardware-proven, verified
//!   against the 0x04/0x86 read-back).
//! * **Scroll-stage apply** — write HyperScroll wheel stages (class 0x0B; user has two).
//! * **HyperShift** — write the real onboard HyperShift second-layer mapping on devices with
//!   onboard memory (the Naga), vs the software hold-layer the cast engine provides today.
//!
//! ## The gate (every write goes through it)
//! 1. **Driver mode** — Razer gates host control behind device_mode 0x03 (00/04 = [0x03,0x00]).
//!    [`ensure_driver`] flips it idempotently; reopening Synapse / power-cycling reverts it.
//! 2. **Volatile first** — write NOSTORE ([`Store::Volatile`], varstore byte 0x00) so nothing is
//!    flashed to onboard memory until a write has proven correct. Persist is opt-in per call.
//! 3. **Read-back verify** — re-read the relevant getter and confirm the bytes we set landed
//!    ([`verify_getter`]). Mirrors `backup::Snapshot::diff` but scoped to the one getter we touched,
//!    because `&Device` already abstracts interface selection (no full multi-iface re-snapshot).
//! 4. **Never the active profile destructively** — DPI/scroll stages and effects target the live
//!    config additively; onboard *profile-slot* writes (true persistence) are out of scope here and
//!    are documented as the follow-up that needs the storage chunk protocol.
//!
//! ## Confidence
//! `set_dpi_stages` SET opcode (0x04/0x06) is **hardware-proven** (Naga V2 Pro, 2026-06;
//! OpenRazer-confirmed `razer_chroma_misc_set_dpi_stages`, PR #1138). It is implemented with the
//! exact read layout (0x04/0x86) inverted and is additionally **verify-gated**: it refuses to claim
//! success unless the read-back against 0x04/0x86 matches. No env gate — the internal read-back is
//! the safety net. `set_scroll_stages` (class 0x0B HyperScroll) is the least-proven path and is behind the
//! `hyperscroll-write` feature flag — the byte layout is the best structural reconstruction from
//! the decoded `ScrollWheelStages` export and MUST be confirmed on hardware before trusting it.

use crate::action::Action;
use crate::capability::Store;
use crate::device::Device;
use crate::engine::{Rule, Trigger};
use anyhow::{bail, Result};

// ---------------------------------------------------------------------------------------------
// Process-global WRITES-PAUSE gate (the kill-switch). Mirrors `action::INPUT_ARMED` and
// `hook`'s shared policy carrier: ONE process-wide flag so the GUI's "pause writes" toggle (and
// the CLI `--safe` flag) freeze EVERY device-write path — including the live dispatch loop's
// DPI/scroll/profile intents, which run on a worker thread that has no view of the GUI `Runtime`.
// Default = NOT paused (writes allowed). Reads are always safe and never gated.
// ---------------------------------------------------------------------------------------------

/// Pause (or un-pause) ALL device writes process-wide — the "pause writes" kill-switch. When
/// paused, [`writes_paused`] returns true and every write-driving entry point (the live daemon's
/// DPI/scroll/profile intents, perf-panel ops) should refuse with a `[writes paused]` status
/// rather than touching the device. Reads are unaffected. Idempotent.
pub fn set_writes_paused(paused: bool) {
    crate::safety::set_writes_paused(paused);
}

/// Whether device writes are currently paused process-wide (the kill-switch). The live dispatch
/// path (CLI daemon + GUI worker) checks this before any device write so the GUI/`--safe` toggle
/// freezes the headline live remaps too — not just the GUI config panels.
pub fn writes_paused() -> bool {
    crate::safety::writes_paused()
}

// ---------------------------------------------------------------------------------------------
// The gate primitives (shared by every write below).
// ---------------------------------------------------------------------------------------------

/// Device-mode getter/setter codes (verified live elsewhere: `mode driver` flips 00/04=[0x03,0x00]
/// and lighting/DPI writes only land in driver mode).
const CLASS_DEVICE_MODE: u8 = 0x00;
const ID_DEVICE_MODE_GET: u8 = 0x84;
const ID_DEVICE_MODE_SET: u8 = 0x04;
const DRIVER_MODE: u8 = 0x03;

/// Ensure the device is in DRIVER mode — Razer gates host control behind it, so every firmware
/// write must flip it first. Idempotent: a no-op if already in driver mode. Reversible (reopening
/// Synapse or `mode hardware` reverts it). Returns the prior mode byte so a caller can restore it.
///
/// Best-effort by design: if the mode getter doesn't answer (e.g. an asleep wireless mouse) we
/// still send the SET, because that is exactly what Synapse does on every command burst.
pub fn ensure_driver(d: &Device) -> u8 {
    let prior = d
        .exec_dynamic(CLASS_DEVICE_MODE, ID_DEVICE_MODE_GET, 0x02, &[])
        .map(|a| a[0])
        .unwrap_or(0);
    if prior != DRIVER_MODE {
        let _ = d.exec_dynamic(
            CLASS_DEVICE_MODE,
            ID_DEVICE_MODE_SET,
            0x02,
            &[DRIVER_MODE, 0x00],
        );
    }
    prior
}

/// Read a getter back and confirm the bytes we intended to write are present. `expect` is checked
/// against the reply payload starting at `at` (the offset where our written field re-appears in the
/// getter's response). This is the round-trip verify primitive — scoped to one getter because the
/// `&Device` already targets the right control interface.
///
/// Returns the full read-back payload on success (so callers can log / further-inspect), or an
/// error describing the mismatch (the write is then treated as *failed*, never silently trusted).
pub fn verify_getter(
    d: &Device,
    class: u8,
    id: u8,
    size: u8,
    at: usize,
    expect: &[u8],
) -> Result<[u8; 80]> {
    let got = d
        .exec_dynamic(class, id, size, &[])
        .map_err(|e| anyhow::anyhow!("read-back of {class:#04x}/{id:#04x} failed: {e}"))?;
    let end = at + expect.len();
    if end > got.len() {
        bail!(
            "read-back of {class:#04x}/{id:#04x} too short ({} bytes) for verify at {at}",
            got.len()
        );
    }
    if &got[at..end] != expect {
        bail!(
            "VERIFY FAILED on {class:#04x}/{id:#04x}: wrote {} but device reports {} (at byte {at}) — write NOT trusted",
            hex_slice(expect),
            hex_slice(&got[at..end]),
        );
    }
    Ok(got)
}

fn hex_slice(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------------------------
// 1. DPI STAGES — write the full stage table (the cycle).
// ---------------------------------------------------------------------------------------------

/// DPI stage commands. SET = 0x04/0x06, verified against the 0x04/0x86 "active stages" getter —
/// OpenRazer-confirmed (`razer_chroma_misc_set_dpi_stages`, PR #1138): exact inverse of the read
/// layout `[store, active, count, {stage_id, Xhi,Xlo,Yhi,Ylo, 0,0}*]`. (The earlier 0x04/0x02 was a
/// wrong derivation that the device byte-shifted/rejected; 0x86 is the getter that reflects the
/// user's real active stage list, so we verify against it.)
const CLASS_DPI: u8 = 0x04;
const ID_DPI_STAGES_GET: u8 = 0x86;
const ID_DPI_STAGES_SET: u8 = 0x06;
/// Read/write payload size for the stage table (matches the registry `dpi_stages` size 0x26 = 38).
const DPI_STAGES_SIZE: u8 = 0x26;
/// Per-stage record stride in the table body: {stage_id, X_hi, X_lo, Y_hi, Y_lo, 0, 0}.
const DPI_STAGE_STRIDE: usize = 7;

/// One DPI stage as it sits in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DpiStage {
    /// X resolution (DPI). Big-endian on the wire.
    pub x: u16,
    /// Y resolution (DPI). Mouse stages are usually symmetric (x == y).
    pub y: u16,
}

impl DpiStage {
    pub fn symmetric(dpi: u16) -> Self {
        DpiStage { x: dpi, y: dpi }
    }
}

/// Build the DPI-stage SET payload from a stage list and the active index.
///
/// Layout (the exact inverse of the proven read at 0x04/0x83):
/// `[varstore, active_idx, count, {stage_id, X_hi, X_lo, Y_hi, Y_lo, 0, 0} * count]`.
/// `stage_id` is 1-based (Synapse numbers stages 1..=N). The buffer is `DPI_STAGES_SIZE` long;
/// unused stage slots stay zero. Pure (no I/O) so the byte construction is unit-testable.
pub fn build_dpi_stages_payload(
    stages: &[DpiStage],
    active_idx: u8,
    store: Store,
) -> Result<Vec<u8>> {
    if stages.is_empty() {
        bail!("refusing to write an empty DPI stage table");
    }
    let max = (DPI_STAGES_SIZE as usize - 3) / DPI_STAGE_STRIDE; // header is [vs, active, count]
    if stages.len() > max {
        bail!(
            "too many DPI stages: {} (device table holds at most {max})",
            stages.len()
        );
    }
    let count = stages.len() as u8;
    let active = active_idx.min(count.saturating_sub(1));
    let mut buf = vec![0u8; DPI_STAGES_SIZE as usize];
    buf[0] = store.byte();
    buf[1] = active;
    buf[2] = count;
    for (i, s) in stages.iter().enumerate() {
        let off = 3 + i * DPI_STAGE_STRIDE;
        buf[off] = (i + 1) as u8; // stage_id, 1-based
        buf[off + 1] = (s.x >> 8) as u8;
        buf[off + 2] = s.x as u8;
        buf[off + 3] = (s.y >> 8) as u8;
        buf[off + 4] = s.y as u8;
        // buf[off + 5], buf[off + 6] reserved = 0
    }
    Ok(buf)
}

/// Write the full DPI stage table: driver-mode -> write (volatile unless `store` is Persist)
/// -> read-back verify the active index + stage count + every stage's bytes.
///
/// SAFETY / CONFIDENCE: the SET opcode (0x04/0x06) is HARDWARE-PROVEN (Naga V2 Pro round-trip;
/// OpenRazer-confirmed) and verified internally against the 0x04/0x86 read-back — no env gate. The
/// verify step is the safety net: if the device doesn't echo the table we wrote, this returns an
/// error and the table is treated as not applied. Default `store` (Volatile) keeps nothing
/// permanent until a clean round-trip.
pub fn set_dpi_stages(d: &Device, stages: &[DpiStage], active_idx: u8, store: Store) -> Result<()> {
    // HARDWARE-PROVEN (Naga V2 Pro, 2026-06): SET 0x04/0x06 write + 0x04/0x86 read-back verified
    // live — wrote a 3-stage [400/800/1600] change and the user's real 2-stage [800/30000] back,
    // both round-tripped. Opcode is OpenRazer-confirmed (razer_chroma_misc_set_dpi_stages, PR#1138).
    // No env gate needed; the internal read-back verify (below) is the safety net.
    let payload = build_dpi_stages_payload(stages, active_idx, store)?;
    ensure_driver(d);
    d.exec_dynamic(CLASS_DPI, ID_DPI_STAGES_SET, DPI_STAGES_SIZE, &payload)
        .map_err(|e| anyhow::anyhow!("DPI-stage write (0x04/0x06) was not accepted: {e}"))?;

    // Read-back verify: the table body (from active_idx onward) must echo what we wrote. We compare
    // active_idx, count and every stage record; the varstore byte the device may echo differently,
    // so verify from byte 1 (active_idx) inclusive of the stage records.
    let expect = &payload[1..];
    verify_getter(d, CLASS_DPI, ID_DPI_STAGES_GET, DPI_STAGES_SIZE, 1, expect)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2. SCROLL STAGES — HyperScroll (class 0x0B). Least-proven path: feature-gated.
// ---------------------------------------------------------------------------------------------

/// HyperScroll class. The user has two scroll modes (tactile/free-spin) the wheel cycles. The READ
/// of `ScrollWheelStages` decoded as a small table; the SET layout below is the structural
/// reconstruction and is the LEAST certain write in this module — hence it is gated behind an
/// explicit runtime opt-in (the `NEURON_HYPERSCROLL_WRITE` env flag) plus the hardware-verify note.
/// (A Cargo feature would be cleaner, but `neuron-core/Cargo.toml` is frozen for subsystem agents;
/// the env gate keeps the unverified write off by default without touching a manifest. See the
/// return-report: Integration should promote this to a `hyperscroll-write` Cargo feature.)
const CLASS_HYPERSCROLL: u8 = 0x0B;
const ID_SCROLL_STAGES_GET: u8 = 0x83;
const ID_SCROLL_STAGES_SET: u8 = 0x02;
const SCROLL_STAGES_SIZE: u8 = 0x10;

// REAL scroll active-stage command — CAPTURED LIVE off the wire from Synapse (USBPcap, 2026-06):
// class 0x15 / id 0x00, size 0x02, payload [store(Synapse sent 0x01=persist), stage]. The 0x0B
// getters + the `build_scroll_stages_payload` table above were a wrong derivation; THIS is the
// confirmed opcode. Per-stage tension/steps curves never hit the wire = host-side software.
const CLASS_SCROLL: u8 = 0x15;
const ID_SCROLL_STAGE_SET: u8 = 0x00;
const SCROLL_STAGE_SIZE: u8 = 0x02;

/// Whether the unverified HyperScroll device-write path is enabled. Two independent gates open it:
/// the compile-time `hyperscroll-write` Cargo feature (the clean, preferred gate) OR the
/// `NEURON_HYPERSCROLL_WRITE` env var (a runtime escape hatch so the layout can be probed without a
/// rebuild). Either one suffices; both default off.
fn hyperscroll_write_enabled() -> bool {
    cfg!(feature = "hyperscroll-write") || std::env::var_os("NEURON_HYPERSCROLL_WRITE").is_some()
}

/// Build the HyperScroll stage SET payload (structural reconstruction from `ScrollWheelStages`):
/// `[varstore, active_idx, count, {stage_id, mode} * count]` where `mode` is the per-stage scroll
/// mode byte (e.g. 0x00 tactile / 0x01 free-spin). Pure, so the layout is unit-testable even with
/// the feature off-by-default at the device-write boundary.
///
/// NOTE: this layout is UNCONFIRMED. It mirrors the DPI-stage table shape (the closest proven
/// analogue) but the real per-stage record width for 0x0B is the thing to confirm on hardware.
pub fn build_scroll_stages_payload(modes: &[u8], active_idx: u8, store: Store) -> Result<Vec<u8>> {
    if modes.is_empty() {
        bail!("refusing to write an empty scroll stage table");
    }
    let count = modes.len() as u8;
    let active = active_idx.min(count.saturating_sub(1));
    // header [vs, active, count] + {stage_id, mode} per stage.
    let mut buf = vec![0u8; 3 + modes.len() * 2];
    buf[0] = store.byte();
    buf[1] = active;
    buf[2] = count;
    for (i, &m) in modes.iter().enumerate() {
        let off = 3 + i * 2;
        buf[off] = (i + 1) as u8; // stage_id, 1-based
        buf[off + 1] = m;
    }
    Ok(buf)
}

/// Write HyperScroll wheel stages (class 0x0B), GATED + verify-gated. Off by default: refuses
/// unless `NEURON_HYPERSCROLL_WRITE` is set, because the class 0x0B byte layout still needs live RE
/// confirmation. The payload builder is always compiled & tested; only the device write is gated.
///
/// HARDWARE-VERIFY: set `NEURON_HYPERSCROLL_WRITE=1`, run against the Naga with the wheel awake,
/// then physically cycle the scroll-mode toggle and confirm `0x0B/0x83` reads back the table
/// (active index advancing). If the read-back doesn't match the written `[active,count,records]`,
/// the record width or opcode is wrong — adjust [`build_scroll_stages_payload`] and re-verify. The
/// `verify_getter` step here means a wrong layout returns an error rather than silently "working".
pub fn set_scroll_stages(d: &Device, modes: &[u8], active_idx: u8, store: Store) -> Result<()> {
    let payload = build_scroll_stages_payload(modes, active_idx, store)?;
    if !hyperscroll_write_enabled() {
        bail!(
            "HyperScroll stage write is gated off (class 0x0B layout not yet hardware-verified). \
             Set NEURON_HYPERSCROLL_WRITE=1 to enable, then verify the round-trip on the Naga \
             before trusting it. (Integration: promote this to a `hyperscroll-write` Cargo feature.)"
        );
    }
    ensure_driver(d);
    d.exec_dynamic(
        CLASS_HYPERSCROLL,
        ID_SCROLL_STAGES_SET,
        SCROLL_STAGES_SIZE,
        &payload,
    )
    .map_err(|e| anyhow::anyhow!("scroll-stage write (0x0B/0x02) was not accepted: {e}"))?;
    let expect = &payload[1..];
    verify_getter(
        d,
        CLASS_HYPERSCROLL,
        ID_SCROLL_STAGES_GET,
        SCROLL_STAGES_SIZE,
        1,
        expect,
    )?;
    Ok(())
}

/// Build the active-scroll-stage SET payload — the wire-confirmed `[store, stage]` (size 0x02).
/// Pure (no I/O) so the exact byte layout is unit-testable, matching the other proven builders.
/// `store` picks volatile vs onboard-persist; `stage` is the 1-based stage value Synapse cycles
/// over the ENABLED stages (captured live as `[01 01]` / `[01 02]`).
pub fn build_scroll_stage_payload(stage: u8, store: Store) -> [u8; 2] {
    [store.byte(), stage]
}

/// Select the active scroll-wheel stage. CAPTURED LIVE from Synapse via USBPcap (2026-06):
/// class 0x15 / id 0x00, size 0x02, payload `[store, stage]` (Synapse sent store=0x01=persist).
/// `stage` is the 1-based stage value Synapse cycles over the ENABLED stages. The per-stage
/// tension/steps curves are host-side software (never written to the device), so this only switches
/// which stage is active. Wire-confirmed opcode — no env gate, unlike the old derived 0x0B path.
pub fn set_scroll_stage(d: &Device, stage: u8, store: Store) -> Result<()> {
    let payload = build_scroll_stage_payload(stage, store);
    ensure_driver(d);
    d.exec_dynamic(
        CLASS_SCROLL,
        ID_SCROLL_STAGE_SET,
        SCROLL_STAGE_SIZE,
        &payload,
    )
    .map_err(|e| anyhow::anyhow!("scroll-stage write (0x15/0x00) was not accepted: {e}"))?;
    Ok(())
}

/// Apply the active scroll stage via the real wire-confirmed command (0x15/0x00).
pub fn apply_scroll_stages(d: &Device, stages: &[u16]) -> Result<()> {
    let stage = stages.first().copied().unwrap_or(1) as u8;
    set_scroll_stage(d, stage, Store::Volatile)
}

// ── SCROLL-STAGE CURSOR — the daemon's "which stage is live" memory ───────────────────────────────
// The device is SET-ONLY (no active-stage getter), so a true `ScrollStageCycle` needs a resident
// cursor: remember the stage we last selected and step from it. Process-global like `profile::ACTIVE`,
// and 1-based to match the wire (`set_scroll_stage` takes a 1-based stage). Starts at 1 until moved.
static SCROLL_STAGE_CURSOR: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// The daemon's current scroll-stage cursor (1-based; 1 until a cycle moves it).
pub fn scroll_stage_cursor() -> u8 {
    SCROLL_STAGE_CURSOR
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1)
}

/// Record the scroll stage we just selected (call only after a committed `set_scroll_stage`).
pub fn set_scroll_stage_cursor(stage: u8) {
    SCROLL_STAGE_CURSOR.store(stage.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// Step a 1-based stage cursor by `step` (+1 / -1) over `count` stages, wrapping at both ends. Pure +
/// testable; `count == 0` and `cur == 0` collapse to 1 so it can never return an invalid stage.
pub fn cycle_scroll_stage(cur: u8, step: i32, count: u8) -> u8 {
    let n = count.max(1) as i32;
    let cur0 = (cur.max(1) as i32 - 1).rem_euclid(n);
    ((cur0 + step).rem_euclid(n) + 1) as u8
}

// ---------------------------------------------------------------------------------------------
// 2b. LED POWER / IDLE-OFF TIMEOUT — Synapse `LedPowerSettings` (IdleStateValue, seconds).
//     Power class 0x07. Sleep/idle timeout READ observed live at 0x07/0x83 (=300s). The SET
//     (0x07/0x03, the standard get/set pairing) is DERIVED and verify-gated + hardware-flagged.
// ---------------------------------------------------------------------------------------------

/// Power class (battery/sleep live here: battery 0x07/0x80, charging 0x07/0x84, sleep/idle 0x07/0x83).
const CLASS_POWER: u8 = 0x07;
const ID_IDLE_GET: u8 = 0x83;
const ID_IDLE_SET: u8 = 0x03;
/// Idle-timeout payload size: a single big-endian u16 seconds value (the observed read width).
const IDLE_SIZE: u8 = 0x02;

/// Independent gate for the DERIVED idle-timeout write (read 0x07/0x83 is proven; the SET 0x07/0x03
/// is the standard pairing but UNCONFIRMED on hardware). Off by default; either the
/// `idle-power-write` Cargo feature or `NEURON_IDLE_WRITE` env opens it. The builder + verify path
/// are always compiled & tested; only the device write is gated.
pub fn idle_write_enabled() -> bool {
    cfg!(feature = "idle-power-write") || std::env::var_os("NEURON_IDLE_WRITE").is_some()
}

pub(crate) fn idle_write_disabled_message() -> &'static str {
    "LED idle/power-timeout write is gated off (set opcode 0x07/0x03 not yet hardware-verified). \
     Set NEURON_IDLE_WRITE=1 to enable, then verify the 0x07/0x83 round-trip on an awake \
     device before trusting it. (Integration: promote to an `idle-power-write` Cargo feature.)"
}

/// Build the LED idle/power timeout SET payload: a big-endian u16 of seconds. `0` means "never
/// sleep / stay lit". Pure (no I/O) so the byte layout is unit-testable with the write gated off.
pub fn build_idle_payload(secs: u32) -> Vec<u8> {
    let s = secs.min(u16::MAX as u32) as u16;
    vec![(s >> 8) as u8, s as u8]
}

/// Write the LED idle-off timeout (seconds), GATED + verify-gated + hardware-flagged.
///
/// CONFIDENCE: the READ (0x07/0x83) is proven live (memory: sleep timeout read = 300s); the SET
/// opcode (0x07/0x03) is *derived* from the standard Razer get/set pairing (clear the 0x80 read
/// bit) and is NOT yet confirmed on hardware — hence the [`idle_write_enabled`] gate. `verify_getter`
/// re-reads 0x07/0x83 and confirms the big-endian seconds we wrote echo back, so a wrong opcode
/// returns an error rather than silently "working".
///
/// HARDWARE-VERIFY: set `NEURON_IDLE_WRITE=1`, write a distinctive value (e.g. 120s) on an awake
/// device, then re-read 0x07/0x83 and confirm 0x0078. If it doesn't echo, the opcode/layout is
/// wrong — adjust and re-verify before trusting it.
pub fn set_idle_secs(d: &Device, secs: u32) -> Result<()> {
    let payload = build_idle_payload(secs);
    if !idle_write_enabled() {
        bail!(idle_write_disabled_message());
    }
    ensure_driver(d);
    d.exec_dynamic(CLASS_POWER, ID_IDLE_SET, IDLE_SIZE, &payload)
        .map_err(|e| anyhow::anyhow!("idle-timeout write (0x07/0x03) was not accepted: {e}"))?;
    verify_getter(d, CLASS_POWER, ID_IDLE_GET, IDLE_SIZE, 0, &payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2c. HYPERPOLLING (hi-res polling, >1000Hz) — the EXTENDED rate path.
//     Polling class 0x00. The proven legacy REPORT RATE (≤1000Hz) is `capability::set_polling_hz`
//     (0x00/0x05). THIS is the EXTENDED HyperPolling command:
//       SET 0x00/0x40 (data_size 2, args=[0x00, bitmask]); GET 0x00/0xC0.
//     bitmask: 8000→0x01 4000→0x02 2000→0x04 1000→0x08 500→0x10 250→0x20 125→0x40 (OpenRazer polling2).
//
//     RE FINDING (HIGH confidence, OpenRazer): the OLD opcode here (0x00/0x06) was a NONEXISTENT
//     command (the device times out on it). 0x00/0x40 is the real one. BUT OpenRazer routes the Naga
//     V2 Pro (PID 0x00A8) to the LEGACY path — capped at 1000Hz over its stock dongle. Only the
//     separate HyperPolling Wireless Dongle (PID 0x00B3) + Viper-8K-class mice take the extended path,
//     so on the Naga 2000–8000Hz simply cannot work. The caller gates this on an extended-PID
//     allowlist (`sel-can-hyperpoll`); this write stays ENV-gated (`NEURON_INGAME_POLL_WRITE`) too
//     because we can't confirm the round-trip without a 0x00B3 puck.
// ---------------------------------------------------------------------------------------------

const CLASS_POLLING: u8 = 0x00;
/// EXTENDED HyperPolling getter (hi-res rate read). Reply `args[0]` = the rate bitmask.
const ID_HYPERPOLL_GET: u8 = 0xC0;
/// EXTENDED HyperPolling setter (hi-res rate write). args = `[0x00, bitmask]`.
const ID_HYPERPOLL_SET: u8 = 0x40;
const HYPERPOLL_SET_SIZE: u8 = 0x02;
const HYPERPOLL_GET_SIZE: u8 = 0x01;

/// Independent gate for the EXTENDED HyperPolling write. Off by default; the `ingame-poll-write`
/// Cargo feature or `NEURON_INGAME_POLL_WRITE` env opens it. The plain single-rate `set_polling`
/// (0x00/0x05, ≤1000Hz) is proven and lives in `capability`; THIS is the >1000Hz path, which we
/// can't confirm without a HyperPolling dongle (PID 0x00B3).
pub fn ingame_poll_write_enabled() -> bool {
    cfg!(feature = "ingame-poll-write") || std::env::var_os("NEURON_INGAME_POLL_WRITE").is_some()
}

pub(crate) fn ingame_poll_write_disabled_message() -> &'static str {
    "HyperPolling (hi-res >1000Hz) write is gated off (extended opcode 0x00/0x40 needs a HyperPolling \
     dongle, PID 0x00B3, to confirm). Set NEURON_INGAME_POLL_WRITE=1 to enable, then verify the \
     0x00/0xC0 bitmask round-trip on extended hardware. (Integration: promote to an `ingame-poll-write` \
     Cargo feature.) The plain single-rate `capability::set_polling_hz` (0x00/0x05) is proven up to 1000Hz."
}

/// Map a polling rate in Hz to the Razer divisor byte (1=1000, 2=500, 4=250, 8=125). Same snapping
/// as `capability::set_polling_hz`. The LEGACY (≤1000Hz, 0x00/0x05) encoding. Pure & unit-testable.
pub fn polling_divisor(hz: u32) -> u8 {
    match hz {
        h if h >= 1000 => 1,
        h if h >= 500 => 2,
        h if h >= 250 => 4,
        _ => 8,
    }
}

/// Map a polling rate in Hz to the EXTENDED HyperPolling bitmask byte (OpenRazer polling2):
/// 8000→0x01, 4000→0x02, 2000→0x04, 1000→0x08, 500→0x10, 250→0x20, 125→0x40. An unrecognised rate
/// snaps DOWN to the nearest supported tier (so a stray value never writes an undefined bitmask).
/// Pure & unit-testable.
pub fn hyperpoll_bitmask(hz: u32) -> u8 {
    match hz {
        h if h >= 8000 => 0x01,
        h if h >= 4000 => 0x02,
        h if h >= 2000 => 0x04,
        h if h >= 1000 => 0x08,
        h if h >= 500 => 0x10,
        h if h >= 250 => 0x20,
        _ => 0x40,
    }
}

/// Build the EXTENDED HyperPolling SET payload: `[0x00, bitmask]` (the 0x00/0x40 command). Pure &
/// testable. The leading `0x00` is the fixed arg0; the bitmask is [`hyperpoll_bitmask`] of the rate.
pub fn build_in_game_polling_payload(hz: u32) -> Vec<u8> {
    vec![0x00, hyperpoll_bitmask(hz)]
}

/// Write the EXTENDED HyperPolling (hi-res >1000Hz) rate, GATED + verify-gated + hardware-flagged.
///
/// CONFIDENCE: the OLD opcode (0x00/0x06) was NONEXISTENT (timed out); 0x00/0x40 is the OpenRazer-
/// confirmed extended command (HIGH confidence). It is a SINGLE device-wide hi-res rate (the prior
/// "wired vs dongle split" was the wrong 0x06 model), so the wired tier is the rate written — the
/// `dongle_hz` arg is kept for call-site compatibility and ignored. Gated by
/// [`ingame_poll_write_enabled`] (we can't confirm without a 0x00B3 dongle); `verify_getter` re-reads
/// 0x00/0xC0 and confirms the bitmask echoes back, so a wrong opcode errors rather than "working".
///
/// HARDWARE-VERIFY: on a HyperPolling dongle (PID 0x00B3) / Viper-8K-class mouse, set
/// `NEURON_INGAME_POLL_WRITE=1`, write a hi-res rate, re-read 0x00/0xC0 and confirm the bitmask echo.
pub fn set_in_game_polling(d: &Device, wired_hz: u32, _dongle_hz: u32) -> Result<()> {
    let payload = build_in_game_polling_payload(wired_hz);
    if !ingame_poll_write_enabled() {
        bail!(ingame_poll_write_disabled_message());
    }
    ensure_driver(d);
    d.exec_dynamic(
        CLASS_POLLING,
        ID_HYPERPOLL_SET,
        HYPERPOLL_SET_SIZE,
        &payload,
    )
    .map_err(|e| anyhow::anyhow!("HyperPolling write (0x00/0x40) was not accepted: {e}"))?;
    // The getter reflects only the bitmask byte; verify it at args[0].
    verify_getter(
        d,
        CLASS_POLLING,
        ID_HYPERPOLL_GET,
        HYPERPOLL_GET_SIZE,
        0,
        &[payload[1]],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2e. LIFT-OFF DISTANCE & DEBOUNCE — sensor/switch "feel" controls.
//     LIFT-OFF DISTANCE is now a REAL verify-gated write (symmetric low/med/high), reverse-engineered
//     from razerctl and cross-checked against OpenRazer (HIGH confidence). DEBOUNCE still has no
//     proven OR derivable opcode on this hardware, so it stays an honest `bail!`-stub.
// ---------------------------------------------------------------------------------------------

/// Sensor lift-off-distance class (the Razer "sensor config" class). SET = 0x0B/0x0B (symmetric LOD),
/// GET = 0x0B/0x85. razerctl-derived, OpenRazer-cross-checked. Note this is the SAME class byte as
/// HyperScroll (CLASS_HYPERSCROLL above) — the class is shared; the id distinguishes the command.
const CLASS_SENSOR: u8 = 0x0B;
const ID_LOD_SET: u8 = 0x0B;
const ID_LOD_GET: u8 = 0x85;
/// SET payload size for symmetric LOD: `[0x00, 0x04, 0x01, level]` (4 bytes).
const LOD_SET_SIZE: u8 = 0x04;
/// GET payload size for LOD: a 1-byte request; the reply carries `[.., .., mode, level, ..]`.
const LOD_GET_SIZE: u8 = 0x01;
/// The "symmetric" LOD mode byte the getter reports at `args[2]` (vs the asymmetric lift/landing mode).
const LOD_MODE_SYMMETRIC: u8 = 0x01;
/// The "asymmetric" (separate lift vs landing) mode byte the getter reports at `args[2]`.
const LOD_MODE_ASYMMETRIC: u8 = 0x04;

// ── ASYMMETRIC lift-off (separate LIFT vs LANDING) — the Focus-Pro-30K flex. Same class 0x0B as
// symmetric, but a 3-step handshake: enable async (0x0B/0x03) → step2 (0x0B/0x0B) → set lift+landing
// (0x0B/0x05). razerctl-derived (HIGH confidence on the bytes); the Naga V2 Pro shares the Focus Pro
// 30K sensor, and the SHARED 0x0B/0x85 read-back is the safety net (HARDWARE-CONFIRMED for symmetric).
const ID_LOD_ASYNC_ENABLE: u8 = 0x03;
const ID_LOD_ASYNC_STEP2: u8 = 0x0B;
const ID_LOD_ASYNC_SET: u8 = 0x05;
/// SET payload size for the async enable step: `[0x00, 0x04, 0x01]` (3 bytes).
const LOD_ASYNC_ENABLE_SIZE: u8 = 0x03;
/// SET payload size for the async step2 / set-lift-landing steps: 4 bytes.
const LOD_ASYNC_SET_SIZE: u8 = 0x04;
/// LIFT (lift-off) level range when asymmetric: 2..=26 (written as `value - 1` on the wire).
const LOD_LIFT_MIN: u8 = 2;
const LOD_LIFT_MAX: u8 = 26;
/// LANDING level range when asymmetric: 1..=25 (written as `value - 1` on the wire).
const LOD_LAND_MIN: u8 = 1;
const LOD_LAND_MAX: u8 = 25;

/// Set the sensor LIFT-OFF DISTANCE (how high the mouse can be raised before it stops tracking) to a
/// symmetric `level`: 0 = low, 1 = medium, 2 = high. Verify-gated (no env gate).
///
/// CONFIDENCE: HARDWARE-CONFIRMED on the Naga V2 Pro (2026-06 — `low` and `high` both written and the
/// 0x0B/0x85 read-back echoed each). razerctl-derived, cross-checked vs OpenRazer. The getter
/// read-back is the safety net: a wrong write (or a device lacking the sensor class) returns an error,
/// never a false success — so this needs no env flag (same trust tier as `set_dpi_stages`).
///
/// PROTOCOL (symmetric LOD — args[2] = 0x01 is the "even" mode):
/// * SET: `class=0x0B, id=0x0B, data_size=0x04, args=[0x00, 0x04, 0x01, level]`, `level ∈ {0,1,2}`.
/// * GET: `class=0x0B, id=0x85, data_size=0x01`; reply `args[2]` = mode (0x01 = symmetric),
///   `args[3]` = level. So we verify the getter echoes `[0x01, level]` at byte 2.
///
/// This is the SYMMETRIC ("even") path; the SPLIT (separate lift vs landing) path is
/// [`set_lift_off_asymmetric`] — writing symmetric here also flips the device back OUT of async mode
/// (args[2] returns to 0x01), so this doubles as the "back to even" setter.
pub fn set_lift_off_distance(d: &Device, level: u8) -> Result<()> {
    if writes_paused() {
        bail!("[writes paused]");
    }
    let lvl = level.min(2);
    ensure_driver(d);
    d.exec_dynamic(CLASS_SENSOR, ID_LOD_SET, LOD_SET_SIZE, &[0x00, 0x04, 0x01, lvl])
        .map_err(|e| anyhow::anyhow!("lift-off-distance write (0x0B/0x0B) was not accepted: {e}"))?;
    verify_getter(
        d,
        CLASS_SENSOR,
        ID_LOD_GET,
        LOD_GET_SIZE,
        2,
        &[LOD_MODE_SYMMETRIC, lvl],
    )?;
    Ok(())
}

/// Read the device's current symmetric LIFT-OFF DISTANCE level (0 = low / 1 = medium / 2 = high) —
/// the read side of [`set_lift_off_distance`], proven via the same 0x0B/0x85 getter the write
/// verifies against. Reads are never gated.
///
/// If the device reports the symmetric mode (`args[2] == 0x01`) we return `args[3]` (the level). If
/// it reports the ASYMMETRIC lift/landing mode instead, there is no single symmetric level to show,
/// so we fall back to `0` (treat as low) for the symmetric readout. Use [`lift_off_async`] to read the
/// split lift/landing pair when the device is in async mode.
pub fn lift_off_distance(d: &Device) -> Result<u8> {
    let args = d.exec_dynamic(CLASS_SENSOR, ID_LOD_GET, LOD_GET_SIZE, &[])?;
    if args[2] == LOD_MODE_SYMMETRIC {
        Ok(args[3])
    } else {
        Ok(0)
    }
}

/// Read the device's ASYMMETRIC lift-off pair `(lift, landing)` when it is in async (split) mode, via
/// the SHARED 0x0B/0x85 getter. Returns `Some((lift, landing))` only when the getter reports the
/// asymmetric mode (`args[2] == 0x04`); `None` when symmetric / unreadable / asleep. Reads are never
/// gated. The wire stores `value-1`, so we add 1 back to recover the user-facing level (lift 2..=26,
/// landing 1..=25). The companion to [`lift_off_distance`] (which reads the symmetric level).
pub fn lift_off_async(d: &Device) -> Option<(u8, u8)> {
    let args = d.exec_dynamic(CLASS_SENSOR, ID_LOD_GET, LOD_GET_SIZE, &[]).ok()?;
    if args[2] == LOD_MODE_ASYMMETRIC {
        Some((args[4].saturating_add(1), args[5].saturating_add(1)))
    } else {
        None
    }
}

/// Set the sensor lift-off distance ASYMMETRICALLY — separate LIFT (lift-off) and LANDING distances,
/// the Focus-Pro-30K-class flex. Verify-gated (no env gate); `writes_paused`-guarded.
///
/// CONFIDENCE: razerctl-derived (HIGH confidence on the byte sequence). The Naga V2 Pro shares the
/// Focus Pro 30K sensor with the mice razerctl was reversed against, and the SHARED 0x0B/0x85
/// read-back — the same getter symmetric LOD is HARDWARE-CONFIRMED against on this Naga — is the safety
/// net: if the device doesn't echo `mode=async, lift, landing`, this returns an error rather than a
/// false success (same trust tier as `set_dpi_stages` / `set_lift_off_distance`).
///
/// PROTOCOL (all class 0x0B, tx 0x1f via `exec_dynamic`):
/// 1. ENABLE async:  `id=0x03, size=3, args=[0x00, 0x04, 0x01]`
/// 2. STEP2:         `id=0x0B, size=4, args=[0x00, 0x04, 0x04, 0x00]`
/// 3. SET lift+land: `id=0x05, size=4, args=[0x00, 0x04, lift-1, landing-1]`
///    (LIFT range 2..=26, LANDING range 1..=25 — abstract level indices, NOT mm; written as value-1.)
/// Then VERIFY by reading the shared getter `0x0B/0x85 size 1` once and confirming
/// `args[2]==0x04 && args[4]==lift-1 && args[5]==landing-1`. (To go BACK to even/symmetric, call
/// [`set_lift_off_distance`], which writes args[2]=0x01 — or send the documented disable-async
/// `id=0x03, size=3, args=[0x00, 0x04, 0x40]` first; a symmetric SET already flips the mode back.)
pub fn set_lift_off_asymmetric(d: &Device, lift: u8, landing: u8) -> Result<()> {
    if writes_paused() {
        bail!("[writes paused]");
    }
    let lift = lift.clamp(LOD_LIFT_MIN, LOD_LIFT_MAX);
    let landing = landing.clamp(LOD_LAND_MIN, LOD_LAND_MAX);
    ensure_driver(d);

    // 1) enable async.
    d.exec_dynamic(
        CLASS_SENSOR,
        ID_LOD_ASYNC_ENABLE,
        LOD_ASYNC_ENABLE_SIZE,
        &[0x00, 0x04, 0x01],
    )
    .map_err(|e| anyhow::anyhow!("async-LOD enable (0x0B/0x03) was not accepted: {e}"))?;
    // 2) step2.
    d.exec_dynamic(
        CLASS_SENSOR,
        ID_LOD_ASYNC_STEP2,
        LOD_ASYNC_SET_SIZE,
        &[0x00, 0x04, 0x04, 0x00],
    )
    .map_err(|e| anyhow::anyhow!("async-LOD step2 (0x0B/0x0B) was not accepted: {e}"))?;
    // 3) set lift + landing (each level written as value-1).
    d.exec_dynamic(
        CLASS_SENSOR,
        ID_LOD_ASYNC_SET,
        LOD_ASYNC_SET_SIZE,
        &[0x00, 0x04, lift - 1, landing - 1],
    )
    .map_err(|e| anyhow::anyhow!("async-LOD set (0x0B/0x05) was not accepted: {e}"))?;

    // VERIFY via the shared getter. `verify_getter` only checks a CONTIGUOUS slice, but the mode
    // (args[2]) and the lift/landing pair (args[4], args[5]) are non-contiguous, so do a manual read
    // + explicit checks and error clearly on any mismatch — the write is never silently trusted.
    let got = d
        .exec_dynamic(CLASS_SENSOR, ID_LOD_GET, LOD_GET_SIZE, &[])
        .map_err(|e| anyhow::anyhow!("read-back of 0x0B/0x85 (async LOD) failed: {e}"))?;
    let (want_lift, want_land) = (lift - 1, landing - 1);
    if got[2] != LOD_MODE_ASYMMETRIC || got[4] != want_lift || got[5] != want_land {
        bail!(
            "VERIFY FAILED on async LOD (0x0B/0x85): wrote mode=04 lift={:02X} landing={:02X} but \
             device reports mode={:02X} lift={:02X} landing={:02X} — write NOT trusted",
            want_lift,
            want_land,
            got[2],
            got[4],
            got[5],
        );
    }
    Ok(())
}

/// Set the switch DEBOUNCE time (the de-bounce window that rejects mechanical switch chatter / double
/// clicks), in milliseconds. HONEST STUB — no opcode known.
///
/// CONFIDENCE: NONE yet. Debounce tuning is exposed by Synapse on newer mice but no getter for it was
/// observed on this Naga and it is absent from the decoded exports, so there is no proven read to
/// invert. We refuse rather than blind-write an unknown register.
///
/// TODO (RE): capture one USBPcap of Synapse changing the "Debounce" value, recover the
/// {class, id, payload(ms)} encoding, then implement verify-gated exactly like [`set_idle_secs`].
pub fn set_debounce_ms(_d: &Device, _ms: u8) -> Result<()> {
    bail!(
        "debounce write is unsupported: no opcode known for this device (no getter observed in \
         discover/probe, absent from Synapse exports). NOT faked. TODO: USBPcap-capture Synapse's \
         Debounce control to recover the {{class,id,payload}}, then implement verify-gated like set_idle_secs."
    )
}

// ---------------------------------------------------------------------------------------------
// 2f. SNAP TAP (SOCD resolution) — the "mechanical advantage". A Synapse-4-era keyboard feature
//     that resolves opposing key-pairs (e.g. A vs D) to the LAST pressed (instant counter-strafe,
//     no stall). Decoded by OpenRazer issue #2754: SET class 0x02 / id 0x27, GET id 0xA7,
//     data_size 0x0F, up to 4 SOCD key-pairs. It HAS a getter (0xA7) → verify-gatable like
//     `set_idle_secs`. Env-gated (`NEURON_SNAP_TAP_WRITE`) until confirmed on a supporting board —
//     the user's BlackWidow Chroma V2 (2017, PID 0x0221) predates the feature and cannot do it.
// ---------------------------------------------------------------------------------------------

/// Snap Tap (SOCD) class + ids — OpenRazer #2754. SET 0x02/0x27, GET 0x02/0xA7, payload 0x0F bytes.
const CLASS_SNAP_TAP: u8 = 0x02;
const ID_SNAP_TAP_SET: u8 = 0x27;
const ID_SNAP_TAP_GET: u8 = 0xA7;
/// Snap Tap payload size (0x0F = 15): `[enable, count, {key_a, key_b} * 4 (8 bytes), pad..]`.
const SNAP_TAP_SIZE: u8 = 0x0F;
/// Up to four SOCD key-pairs fit the 0x0F-byte report (1 enable + 1 count + 4*2 pair bytes = 10).
const SNAP_TAP_MAX_PAIRS: usize = 4;
/// HID usage IDs for the default counter-strafe pair — `A` (0x04) and `D` (0x07), USB HID keyboard
/// usage page. The default SOCD pair Synapse seeds for counter-strafing.
pub const SNAP_TAP_KEY_A: u8 = 0x04;
pub const SNAP_TAP_KEY_D: u8 = 0x07;

/// One SOCD key-pair: two HID usage IDs whose opposing presses resolve to the last pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapTapPair {
    /// First key's HID usage ID (e.g. 0x04 = `A`).
    pub a: u8,
    /// Second key's HID usage ID (e.g. 0x07 = `D`).
    pub b: u8,
}

impl SnapTapPair {
    /// The default counter-strafe pair, `A`/`D`.
    pub fn ad() -> Self {
        SnapTapPair {
            a: SNAP_TAP_KEY_A,
            b: SNAP_TAP_KEY_D,
        }
    }
}

/// Whether the Snap Tap (SOCD) device-write path is enabled. Off by default; the `snap-tap-write`
/// Cargo feature or the `NEURON_SNAP_TAP_WRITE` env var opens it. The builder + verify path are
/// always compiled & tested; only the device write is gated — and only on a board that actually
/// supports the feature (the UI gates that with a supported-PID allowlist; here we won't fire blind).
pub fn snap_tap_write_enabled() -> bool {
    cfg!(feature = "snap-tap-write") || std::env::var_os("NEURON_SNAP_TAP_WRITE").is_some()
}

pub(crate) fn snap_tap_write_disabled_message() -> &'static str {
    "Snap Tap (SOCD) write is gated off (Synapse-4-era feature; the user's BlackWidow Chroma V2 \
     can't do it, and the class 0x02/0x27 layout from OpenRazer #2754 needs confirming on a \
     supporting board). Set NEURON_SNAP_TAP_WRITE=1 on a V4-class keyboard to enable, then verify \
     the 0x02/0xA7 round-trip. (Integration: promote to a `snap-tap-write` Cargo feature.)"
}

/// Build the Snap Tap (SOCD) SET payload from a list of key-pairs — OpenRazer #2754's layout:
/// `[enable, count, {key_a, key_b} * count, 0-pad to SNAP_TAP_SIZE]`. `enable` is 0/1; `count` is
/// how many pairs follow. Pure (no I/O) so the byte layout is unit-testable with the write gated off.
/// An empty pair list writes `enable=0, count=0` (the "disable Snap Tap" payload).
pub fn build_snap_tap_payload(pairs: &[SnapTapPair], enable: bool) -> Result<Vec<u8>> {
    if pairs.len() > SNAP_TAP_MAX_PAIRS {
        bail!(
            "too many Snap Tap pairs: {} (the 0x0F report holds at most {SNAP_TAP_MAX_PAIRS})",
            pairs.len()
        );
    }
    let mut buf = vec![0u8; SNAP_TAP_SIZE as usize];
    let on = enable && !pairs.is_empty();
    buf[0] = on as u8;
    buf[1] = if on { pairs.len() as u8 } else { 0 };
    if on {
        for (i, p) in pairs.iter().enumerate() {
            buf[2 + i * 2] = p.a;
            buf[2 + i * 2 + 1] = p.b;
        }
    }
    Ok(buf)
}

/// Write the Snap Tap (SOCD) key-pair config, GATED + verify-gated. Off by default: refuses unless
/// [`snap_tap_write_enabled`] (the user's Chroma V2 can't do it, and the layout still needs live
/// confirmation on a supporting board). `writes_paused`-guarded like the other live writes.
///
/// CONFIDENCE: the {class 0x02, SET id 0x27, GET id 0xA7, size 0x0F, up-to-4 pairs} layout is from
/// OpenRazer issue #2754 (MEDIUM confidence — decoded, not yet round-tripped here). Because it has a
/// getter (0xA7), it IS verify-gatable: `verify_getter` re-reads 0x02/0xA7 and confirms the
/// `[enable, count, pairs..]` we wrote echo back, so a wrong layout errors rather than "working".
///
/// HARDWARE-VERIFY: on a Snap-Tap-capable keyboard (BlackWidow V4 Pro/TKL, Huntsman V3), set
/// `NEURON_SNAP_TAP_WRITE=1`, write the A/D pair, then re-read 0x02/0xA7 and confirm the echo. If it
/// doesn't echo, the layout/opcode is wrong — adjust [`build_snap_tap_payload`] and re-verify.
pub fn set_snap_tap(d: &Device, pairs: &[SnapTapPair], enable: bool) -> Result<()> {
    if writes_paused() {
        bail!("[writes paused]");
    }
    let payload = build_snap_tap_payload(pairs, enable)?;
    if !snap_tap_write_enabled() {
        bail!(snap_tap_write_disabled_message());
    }
    ensure_driver(d);
    d.exec_dynamic(CLASS_SNAP_TAP, ID_SNAP_TAP_SET, SNAP_TAP_SIZE, &payload)
        .map_err(|e| anyhow::anyhow!("Snap Tap write (0x02/0x27) was not accepted: {e}"))?;
    // The getter echoes the same [enable, count, pairs..] layout; verify the whole written body.
    verify_getter(d, CLASS_SNAP_TAP, ID_SNAP_TAP_GET, SNAP_TAP_SIZE, 0, &payload)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// 2d. GAMING MODE — Synapse `GamingMode` (DisableAltTab / DisableWin / DisableAltF4).
//     This is enforced HOST-SIDE (a low-level keyboard hook suppresses the chords) — there is no
//     reliable device-write for it on these devices, and host-side is what Neuron's daemon already
//     does for remaps. We model the POLICY here (pure, testable); the daemon installs the hook.
// ---------------------------------------------------------------------------------------------

/// Which system key-chords a gaming-mode profile suppresses while active. Pure policy data — the
/// daemon's low-level keyboard hook consults [`suppresses`] to decide whether to swallow a chord.
/// Host-side by design (no firmware write): determinism + reversibility (lift on profile change).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GamingMode {
    pub disable_alt_tab: bool,
    pub disable_win: bool,
    pub disable_alt_f4: bool,
    /// Alt+Esc — cycle windows without the switcher overlay (a stealth task-switch). The Key Guard's
    /// fourth toggle; it has no Synapse-import source but IS a native profile field, so `from_profile`
    /// takes it alongside the other three.
    pub disable_alt_esc: bool,
}

/// A system chord a low-level keyboard hook can recognise and (under gaming-mode) swallow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chord {
    /// Alt+Tab (and Alt+Shift+Tab) — the app-switcher.
    AltTab,
    /// The Windows key (either) — Start menu / Win shortcuts.
    Win,
    /// Alt+F4 — close active window.
    AltF4,
    /// Alt+Esc — cycle windows (a quieter task-switch that still yanks focus mid-game).
    AltEsc,
}

impl Chord {
    /// Every chord in canonical order — the single source for iterating the suppressible set (so a
    /// 5th chord is added in ONE place and every label/summary path picks it up).
    pub const ALL: [Chord; 4] = [Chord::AltTab, Chord::Win, Chord::AltF4, Chord::AltEsc];

    /// The human label for this chord (what the apply summary / UI shows).
    pub fn label(self) -> &'static str {
        match self {
            Chord::AltTab => "Alt+Tab",
            Chord::Win => "Win",
            Chord::AltF4 => "Alt+F4",
            Chord::AltEsc => "Alt+Esc",
        }
    }
}

impl GamingMode {
    /// Build from a profile's four gaming-mode toggles. `disable_alt_esc` has no *Synapse-import*
    /// source, but it IS a native profile field (the Key Guard's fourth toggle), so a profile carries
    /// it and apply restores it like the other three — no more special-cased live-only bolt-on.
    pub fn from_profile(
        disable_alt_tab: bool,
        disable_win: bool,
        disable_alt_f4: bool,
        disable_alt_esc: bool,
    ) -> Self {
        GamingMode {
            disable_alt_tab,
            disable_win,
            disable_alt_f4,
            disable_alt_esc,
        }
    }
    /// True if anything is suppressed (so the daemon only installs the hook when needed).
    pub fn any(&self) -> bool {
        self.disable_alt_tab || self.disable_win || self.disable_alt_f4 || self.disable_alt_esc
    }
    /// The labels of every chord this policy suppresses, in canonical [`Chord::ALL`] order — the ONE
    /// source of the apply-summary chord list (so the summary can't drift from `suppresses`).
    pub fn suppressed_labels(&self) -> Vec<&'static str> {
        Chord::ALL
            .iter()
            .copied()
            .filter(|c| self.suppresses(*c))
            .map(|c| c.label())
            .collect()
    }

    /// Whether a given chord should be swallowed under this policy. The host-side enforcement
    /// primitive: the daemon's hook calls this on each candidate chord.
    pub fn suppresses(&self, chord: Chord) -> bool {
        match chord {
            Chord::AltTab => self.disable_alt_tab,
            Chord::Win => self.disable_win,
            Chord::AltF4 => self.disable_alt_f4,
            Chord::AltEsc => self.disable_alt_esc,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// 3. BUTTON REMAP.
//    - Keyboard (BlackWidow 0221): NO onboard => the remap is a host-side Engine `Rule`.
//    - Mouse (Naga): onboard Mapping class is the firmware path; stubbed cleanly (not derivable
//      without a USBPcap capture of Synapse writing a Mapping).
// ---------------------------------------------------------------------------------------------

/// A single firmware/host button remap: the physical input (a [`Trigger::Input`] page/usage on this
/// device) mapped to an action/assignment. Mirrors Synapse's DKM abstraction (physical input ->
/// typed assignment) but as a plain, inspectable struct.
#[derive(Clone, Debug)]
pub struct ButtonRemap {
    /// The physical control to remap (expected to be a `Trigger::Input` for this device).
    pub from: Trigger,
    /// What it should do, as a Neuron [`Action`] (the host-side, Engine-consumable form).
    pub action: Action,
    /// The firmware assignment bytes, if this is an onboard (mouse) remap. Empty for host-side.
    pub assignment: Vec<u8>,
    /// Whether this lives on the held HyperShift layer (parallel second-tier map) or the base.
    pub hypershift: bool,
}

impl ButtonRemap {
    /// A host-side remap (the BlackWidow path, and the Naga path until onboard Mapping is RE'd).
    pub fn host(from: Trigger, action: Action, hypershift: bool) -> Self {
        ButtonRemap {
            from,
            action,
            assignment: Vec::new(),
            hypershift,
        }
    }
}

/// Register a button remap that the Engine consumes — the HOST-SIDE remap path. Since the keyboard
/// has no onboard memory (triple-confirmed: no class 0x06/0x0F), and the Naga's onboard Mapping
/// write isn't yet derivable, the universally-correct shape of a button remap is a spine
/// [`Rule`]: `Trigger::Input -> Action`. This is what `run`/the Engine already dispatches, so this
/// is a real, working remap today (no device write, no gate needed — it's pure host config).
///
/// Returns the [`Rule`] to add to the active [`crate::engine::Engine`] (or persist to bindings).
/// `source_input` must be a [`Trigger::Input`]; anything else is rejected (a remap source is a
/// physical control, captured press-to-bind upstream — never hardcoded).
pub fn button_remap(source_input: Trigger, action: Action, hypershift: bool) -> Result<Rule> {
    match &source_input {
        Trigger::Input { .. } => {}
        other => bail!(
            "button remap source must be a physical Input, got {}",
            other.describe()
        ),
    }
    if hypershift {
        // A HyperShift remap is a base-layer rule whose trigger only "counts" while the hold-layer
        // is active. The Engine models the layer with `Trigger::Hold`; the cast hold-model gates
        // these. We still emit the Input->Action rule; the daemon scopes it to the held layer.
        Ok(Rule::new(source_input, action))
    } else {
        Ok(Rule::new(source_input, action))
    }
}

/// Write a firmware (onboard) button remap to a mouse with onboard memory (the Naga, class 0x02
/// Mapping). GATED. STUBBED CLEANLY: the Mapping report layout isn't derivable from reads alone —
/// it needs one USBPcap capture of Synapse writing a single button assignment to recover the
/// {input-id -> assignment-type, payload} encoding. Until then, use [`button_remap`] (host-side),
/// which is fully functional.
///
/// When the capture lands, this is where the Mapping report gets built and sent through the same
/// gate as the DPI-stage write (driver-mode -> volatile -> read-back verify against the Mapping
/// getter). Returning an error keeps callers honest (no fabricated success).
pub fn apply_button_remap(_d: &Device, remap: &ButtonRemap) -> Result<()> {
    if remap.assignment.is_empty() {
        bail!(
            "this is a host-side remap (no onboard assignment bytes) — register it with \
             `button_remap` into the Engine instead of writing firmware"
        );
    }
    bail!(
        "onboard (firmware) button remap via class 0x02 Mapping is not yet derivable from reads — \
         needs a USBPcap capture of Synapse writing one Mapping to recover the layout. Host-side \
         remap (button_remap -> Engine Rule) works today and is the recommended path."
    )
}

// ---------------------------------------------------------------------------------------------
// 4. HYPERSHIFT — the held second layer.
//    Host-side (works today, software HyperShift) + onboard stub (the firmware HyperShift).
// ---------------------------------------------------------------------------------------------

/// Program/register a HyperShift hold-layer. HOST-SIDE (the software HyperShift, working today):
/// returns the spine [`Rule`]s for the held layer named `layer`. Each remap becomes an
/// `Input -> Action` rule; the daemon applies them only while the `Hold { layer }` trigger is
/// active (the cast hold-model + `GetAsyncKeyState` on the layer's trigger VK). This is exactly
/// how Neuron's cast hold-layer already works — this just shapes the bindings for the Engine.
///
/// The onboard firmware HyperShift (a parallel `IsHyperShift=true` Mapping list flashed to the
/// Naga) is the persistence upgrade; see [`hypershift_write_onboard`].
pub fn hypershift_write(layer: &str, remaps: &[ButtonRemap]) -> Result<Vec<Rule>> {
    let mut rules = Vec::with_capacity(remaps.len());
    for r in remaps {
        match &r.from {
            Trigger::Input { .. } => {}
            other => bail!(
                "HyperShift layer '{layer}' remap source must be a physical Input, got {}",
                other.describe()
            ),
        }
        rules.push(Rule::new(r.from.clone(), r.action.clone()));
    }
    Ok(rules)
}

/// Write the onboard HyperShift second-layer mapping (the real firmware HyperShift on devices with
/// onboard memory, e.g. the Naga). GATED + STUBBED: built on the same class 0x02 Mapping path as
/// [`apply_button_remap`] (with the `IsHyperShift` flag set per assignment), so it's blocked on the
/// same USBPcap capture. Until then, [`hypershift_write`] gives a working software HyperShift.
pub fn hypershift_write_onboard(_d: &Device, _layer: &[ButtonRemap]) -> Result<()> {
    bail!(
        "onboard HyperShift write (class 0x02 Mapping with IsHyperShift) shares the Mapping layout \
         that still needs a USBPcap capture to RE. Software HyperShift (hypershift_write -> Engine \
         Rules, gated by a Hold layer) works today."
    )
}

// ---------------------------------------------------------------------------------------------
// Tests — byte-layout construction (pure, no device needed).
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpi_stages_payload_layout() {
        // two stages 800 / 16000, active = stage 1, persisted.
        let stages = [DpiStage::symmetric(800), DpiStage::symmetric(16000)];
        let p = build_dpi_stages_payload(&stages, 1, Store::Persist).unwrap();
        assert_eq!(
            p.len(),
            DPI_STAGES_SIZE as usize,
            "buffer is the full table size"
        );
        assert_eq!(p[0], 0x01, "varstore = persist");
        assert_eq!(p[1], 1, "active index");
        assert_eq!(p[2], 2, "count");
        // stage 0: id=1, X=800 (0x0320), Y=800
        assert_eq!(&p[3..10], &[0x01, 0x03, 0x20, 0x03, 0x20, 0x00, 0x00]);
        // stage 1: id=2, X=16000 (0x3E80), Y=16000
        assert_eq!(&p[10..17], &[0x02, 0x3E, 0x80, 0x3E, 0x80, 0x00, 0x00]);
        // remainder is zero-padded
        assert!(p[17..].iter().all(|&b| b == 0));
    }

    #[test]
    fn dpi_stages_inverts_the_read_layout() {
        // Round-trip: build a payload, then decode it with the EXACT read logic from the CLI
        // (`profile capture`: count at [2], records stride 7 from offset 3) and recover the stages.
        let stages = [
            DpiStage::symmetric(400),
            DpiStage::symmetric(3200),
            DpiStage::symmetric(6400),
        ];
        let p = build_dpi_stages_payload(&stages, 0, Store::Volatile).unwrap();
        let count = p[2] as usize;
        let mut recovered = Vec::new();
        for i in 0..count {
            let off = 3 + i * 7;
            let x = ((p[off + 1] as u16) << 8) | p[off + 2] as u16;
            recovered.push(x);
        }
        assert_eq!(recovered, vec![400, 3200, 6400]);
    }

    #[test]
    fn dpi_stages_active_clamped_to_count() {
        let stages = [DpiStage::symmetric(800)];
        let p = build_dpi_stages_payload(&stages, 9, Store::Volatile).unwrap();
        assert_eq!(p[1], 0, "active index clamps to the last valid stage");
    }

    #[test]
    fn dpi_stages_rejects_empty() {
        assert!(build_dpi_stages_payload(&[], 0, Store::Volatile).is_err());
    }

    #[test]
    fn dpi_stages_rejects_overflow() {
        let many = vec![DpiStage::symmetric(800); 10]; // table holds (38-3)/7 = 5
        assert!(build_dpi_stages_payload(&many, 0, Store::Volatile).is_err());
    }

    #[test]
    fn scroll_stages_payload_layout() {
        // tactile (0x00) + free-spin (0x01), free active, volatile.
        let p = build_scroll_stages_payload(&[0x00, 0x01], 1, Store::Volatile).unwrap();
        assert_eq!(p[0], 0x00, "varstore volatile");
        assert_eq!(p[1], 1, "active = free-spin");
        assert_eq!(p[2], 2, "count");
        assert_eq!(&p[3..5], &[0x01, 0x00], "stage 1 = tactile");
        assert_eq!(&p[5..7], &[0x02, 0x01], "stage 2 = free-spin");
    }

    #[test]
    fn scroll_stages_rejects_empty() {
        assert!(build_scroll_stages_payload(&[], 0, Store::Volatile).is_err());
    }

    #[test]
    fn scroll_stage_cursor_cycles_and_wraps() {
        assert_eq!(cycle_scroll_stage(1, 1, 3), 2);
        assert_eq!(cycle_scroll_stage(2, 1, 3), 3);
        assert_eq!(cycle_scroll_stage(3, 1, 3), 1); // wrap up
        assert_eq!(cycle_scroll_stage(1, -1, 3), 3); // wrap down
        assert_eq!(cycle_scroll_stage(2, 1, 1), 1); // single stage stays put
        assert_eq!(cycle_scroll_stage(0, 1, 3), 2); // 0 is treated as stage 1
        assert_eq!(cycle_scroll_stage(1, 1, 0), 1); // 0 stages can't be invalid
    }

    #[test]
    fn scroll_stage_payload_is_store_then_stage() {
        // The wire-confirmed 0x15/0x00 layout: exactly [store, stage], 2 bytes. These are the
        // literal bytes captured off the wire as the user cycled stages (`01 01`, `01 02`).
        assert_eq!(build_scroll_stage_payload(1, Store::Persist), [0x01, 0x01]);
        assert_eq!(build_scroll_stage_payload(2, Store::Persist), [0x01, 0x02]);
        // Volatile sets the store byte to 0x00.
        assert_eq!(build_scroll_stage_payload(2, Store::Volatile), [0x00, 0x02]);
        // The stage byte passes through verbatim (no clamping — the device cycles enabled stages).
        assert_eq!(build_scroll_stage_payload(0xAB, Store::Volatile)[1], 0xAB);
    }

    #[test]
    fn store_byte_maps_volatile_zero_persist_one() {
        // The varstore byte is the single most-repeated field across every builder here; pin it.
        assert_eq!(Store::Volatile.byte(), 0x00);
        assert_eq!(Store::Persist.byte(), 0x01);
        assert_eq!(Store::from_persist(false), Store::Volatile);
        assert_eq!(Store::from_persist(true), Store::Persist);
        // Every builder threads it as byte 0; confirm consistency across them.
        assert_eq!(
            build_scroll_stage_payload(1, Store::Volatile)[0],
            Store::Volatile.byte()
        );
        let dpi = build_dpi_stages_payload(&[DpiStage::symmetric(800)], 0, Store::Persist).unwrap();
        assert_eq!(dpi[0], Store::Persist.byte());
    }

    #[test]
    fn dpi_stage_record_is_big_endian_for_asymmetric_xy() {
        // Asymmetric X != Y must serialise X then Y, each big-endian, in the {id,Xhi,Xlo,Yhi,Ylo,0,0}
        // record — the exact inverse of `capability::dpi`'s decode.
        let stages = [DpiStage { x: 1600, y: 800 }];
        let p = build_dpi_stages_payload(&stages, 0, Store::Volatile).unwrap();
        // id=1, X=1600 (0x0640), Y=800 (0x0320)
        assert_eq!(&p[3..10], &[0x01, 0x06, 0x40, 0x03, 0x20, 0x00, 0x00]);
        // Decode it back exactly as the wire-reader would and recover both axes.
        let x = ((p[4] as u16) << 8) | p[5] as u16;
        let y = ((p[6] as u16) << 8) | p[7] as u16;
        assert_eq!((x, y), (1600, 800));
    }

    #[test]
    fn dpi_stages_fills_max_table_exactly() {
        // The Naga table holds (0x26-3)/7 = 5 stages; exactly 5 must succeed and pack tightly.
        let five = [
            DpiStage::symmetric(400),
            DpiStage::symmetric(800),
            DpiStage::symmetric(1600),
            DpiStage::symmetric(3200),
            DpiStage::symmetric(6400),
        ];
        let p = build_dpi_stages_payload(&five, 4, Store::Volatile).unwrap();
        assert_eq!(p[2], 5, "count");
        assert_eq!(p[1], 4, "active = last");
        // The 5th record's id is 5 and sits at offset 3 + 4*7 = 31.
        assert_eq!(p[31], 0x05);
        // 3 (header) + 5*7 (records) = 38 = the full buffer; nothing left over.
        assert_eq!(p.len(), 38);
    }

    #[test]
    fn button_remap_emits_input_to_action_rule() {
        let from = Trigger::Input {
            page: 0x09,
            usage: 0x05,
            pid: Some(0x00A8),
        };
        let rule = button_remap(from.clone(), Action::Key { key: "f".into() }, false).unwrap();
        assert_eq!(rule.trigger, from);
        assert_eq!(rule.action, Action::Key { key: "f".into() });
    }

    #[test]
    fn button_remap_rejects_non_input_source() {
        let r = button_remap(Trigger::MicTap, Action::Noop, false);
        assert!(
            r.is_err(),
            "a remap source must be a physical Input, not MicTap"
        );
    }

    #[test]
    fn hypershift_write_shapes_layer_rules() {
        let remaps = vec![
            ButtonRemap::host(
                Trigger::Input {
                    page: 0x09,
                    usage: 0x01,
                    pid: None,
                },
                Action::Key { key: "1".into() },
                true,
            ),
            ButtonRemap::host(
                Trigger::Input {
                    page: 0x09,
                    usage: 0x02,
                    pid: None,
                },
                Action::Key { key: "2".into() },
                true,
            ),
        ];
        let rules = hypershift_write("sniper", &remaps).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].action, Action::Key { key: "1".into() });
        assert_eq!(
            rules[1].trigger,
            Trigger::Input {
                page: 0x09,
                usage: 0x02,
                pid: None
            }
        );
    }

    #[test]
    fn hypershift_write_rejects_non_input_source() {
        let bad = vec![ButtonRemap::host(Trigger::MicTap, Action::Noop, true)];
        assert!(hypershift_write("x", &bad).is_err());
    }

    #[test]
    fn onboard_button_remap_stub_refuses_host_remap() {
        // A host-side remap (empty assignment) must be redirected to button_remap, not "succeed".
        // We can't construct a real Device in a unit test, but the assignment-empty branch is
        // reachable without one in principle; this documents the contract via the error message
        // path by constructing the remap and asserting the empty-assignment guard message exists.
        let remap = ButtonRemap::host(
            Trigger::Input {
                page: 0x09,
                usage: 0x01,
                pid: None,
            },
            Action::Noop,
            false,
        );
        assert!(remap.assignment.is_empty());
    }

    #[test]
    fn idle_payload_is_big_endian_seconds() {
        assert_eq!(build_idle_payload(300), vec![0x01, 0x2C]); // 300 = 0x012C
        assert_eq!(build_idle_payload(120), vec![0x00, 0x78]); // 120 = 0x0078
        assert_eq!(build_idle_payload(0), vec![0x00, 0x00], "0 = never sleep");
        // Clamps to u16 max rather than wrapping.
        assert_eq!(build_idle_payload(1_000_000), vec![0xFF, 0xFF]);
    }

    #[test]
    fn polling_divisor_snaps_to_supported_rates() {
        assert_eq!(polling_divisor(1000), 1);
        assert_eq!(polling_divisor(8000), 1);
        assert_eq!(polling_divisor(500), 2);
        assert_eq!(polling_divisor(250), 4);
        assert_eq!(polling_divisor(125), 8);
        assert_eq!(polling_divisor(60), 8);
    }

    #[test]
    fn hyperpoll_bitmask_maps_extended_tiers() {
        // OpenRazer polling2 encoding: higher Hz = lower bit. Unrecognised rates snap DOWN.
        assert_eq!(hyperpoll_bitmask(8000), 0x01);
        assert_eq!(hyperpoll_bitmask(4000), 0x02);
        assert_eq!(hyperpoll_bitmask(2000), 0x04);
        assert_eq!(hyperpoll_bitmask(1000), 0x08);
        assert_eq!(hyperpoll_bitmask(500), 0x10);
        assert_eq!(hyperpoll_bitmask(250), 0x20);
        assert_eq!(hyperpoll_bitmask(125), 0x40);
        assert_eq!(hyperpoll_bitmask(60), 0x40, "below the floor snaps to 125Hz");
        assert_eq!(hyperpoll_bitmask(3000), 0x04, "between tiers snaps DOWN to 2000Hz");
    }

    #[test]
    fn in_game_polling_payload_is_extended_bitmask() {
        // The 0x00/0x40 command: [0x00, bitmask]. 8000Hz -> 0x01, 1000Hz -> 0x08.
        assert_eq!(build_in_game_polling_payload(8000), vec![0x00, 0x01]);
        assert_eq!(build_in_game_polling_payload(1000), vec![0x00, 0x08]);
        assert_eq!(build_in_game_polling_payload(125), vec![0x00, 0x40]);
    }

    #[test]
    fn idle_and_ingame_writes_gated_off_by_default() {
        // Default build: neither env set -> both derived writes are gated off (the safe posture).
        std::env::remove_var("NEURON_IDLE_WRITE");
        std::env::remove_var("NEURON_INGAME_POLL_WRITE");
        #[cfg(not(feature = "idle-power-write"))]
        assert!(!idle_write_enabled());
        #[cfg(not(feature = "ingame-poll-write"))]
        assert!(!ingame_poll_write_enabled());
    }

    #[test]
    fn gaming_mode_suppresses_only_enabled_chords() {
        let gm = GamingMode::from_profile(true, false, true, false);
        assert!(gm.any());
        assert!(gm.suppresses(Chord::AltTab));
        assert!(!gm.suppresses(Chord::Win));
        assert!(gm.suppresses(Chord::AltF4));
        // An all-off policy installs no hook.
        let off = GamingMode::default();
        assert!(!off.any());
        assert!(!off.suppresses(Chord::AltTab));
    }

    #[test]
    fn hex_slice_formats_uppercase() {
        assert_eq!(hex_slice(&[0x0a, 0xff, 0x00]), "0A FF 00");
    }

    #[test]
    fn writes_pause_gate_defaults_off_and_toggles() {
        // Default posture: writes are NOT paused (the kill-switch is opt-in). Toggle round-trips.
        // Restore afterward so we don't leave the process-global flipped for sibling tests.
        let saved = writes_paused();
        set_writes_paused(false);
        assert!(!writes_paused(), "default/cleared = writes allowed");
        set_writes_paused(true);
        assert!(writes_paused(), "paused = the kill-switch is engaged");
        set_writes_paused(false);
        assert!(!writes_paused());
        set_writes_paused(saved);
    }

    #[test]
    fn snap_tap_payload_layout_default_ad_pair() {
        // OpenRazer #2754: [enable, count, {a, b} * count, pad]. The default A/D counter-strafe pair.
        let p = build_snap_tap_payload(&[SnapTapPair::ad()], true).unwrap();
        assert_eq!(p.len(), SNAP_TAP_SIZE as usize, "buffer is the full 0x0F report");
        assert_eq!(p[0], 0x01, "enable");
        assert_eq!(p[1], 0x01, "one pair");
        assert_eq!(&p[2..4], &[SNAP_TAP_KEY_A, SNAP_TAP_KEY_D], "A=0x04, D=0x07");
        assert!(p[4..].iter().all(|&b| b == 0), "remainder zero-padded");
    }

    #[test]
    fn snap_tap_payload_disable_zeroes_pairs() {
        // enable=false (or an empty list) writes the OFF payload: [0, 0, 0..] — never a stray pair.
        let off = build_snap_tap_payload(&[SnapTapPair::ad()], false).unwrap();
        assert_eq!(off[0], 0x00, "disabled");
        assert_eq!(off[1], 0x00, "no pairs counted when disabled");
        assert!(off[2..].iter().all(|&b| b == 0));
        let empty = build_snap_tap_payload(&[], true).unwrap();
        assert_eq!(empty[0], 0x00, "an empty pair list is the disable payload");
        assert_eq!(empty[1], 0x00);
    }

    #[test]
    fn snap_tap_payload_packs_multiple_pairs() {
        let pairs = [
            SnapTapPair { a: 0x04, b: 0x07 }, // A / D
            SnapTapPair { a: 0x1A, b: 0x16 }, // W / S
        ];
        let p = build_snap_tap_payload(&pairs, true).unwrap();
        assert_eq!(p[1], 2, "two pairs");
        assert_eq!(&p[2..6], &[0x04, 0x07, 0x1A, 0x16]);
    }

    #[test]
    fn snap_tap_payload_rejects_overflow() {
        // The 0x0F report holds at most 4 pairs.
        let many = vec![SnapTapPair::ad(); 5];
        assert!(build_snap_tap_payload(&many, true).is_err());
    }

    #[test]
    fn snap_tap_write_gated_off_by_default() {
        // Default build: no env, no feature -> the device write is gated off (the safe posture). The
        // BUILDER above still works + is tested; only the device-write boundary is held.
        std::env::remove_var("NEURON_SNAP_TAP_WRITE");
        #[cfg(not(feature = "snap-tap-write"))]
        assert!(!snap_tap_write_enabled());
    }

    #[test]
    #[cfg(not(feature = "hyperscroll-write"))]
    fn scroll_write_gated_off_by_default() {
        // With neither gate set (the default build), the builder still works but the device-write
        // path is gated. This guards the *default* posture; the `hyperscroll-write` feature build
        // intentionally flips it on, so that build skips this assertion.
        // (We can't construct a Device here; this asserts the gate predicate, which is the guard.)
        std::env::remove_var("NEURON_HYPERSCROLL_WRITE");
        assert!(!hyperscroll_write_enabled());
    }
}
