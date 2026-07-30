// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The neuron-core bridge — real hardware becomes kernel surfaces.
//!
//! This is where the R&D stops being abstract: the registry's device TOMLs map
//! to [`SurfaceInfo`]s, and each device gets ONE [`Writer`] whose sink speaks
//! the proven HID write recipe (the exact inner step of `Lights::animate`:
//! changed rows → `row_report` → `send_lighting_fast` → latch). Everything
//! else — the GUI, protocol adapters, telemetry bindings — paints through the
//! arbiter. The bridge deliberately does NOT hand out device handles: the
//! writer remains the only component allowed to touch the device.
//!
//! Honest edges, stated up front:
//! - **Kind is a heuristic** (DPI-capable ⇒ mouse; tall LED matrix ⇒
//!   keyboard) until the device TOMLs grow an explicit `kind` field — the
//!   right fix rather than guessing from geometry.
//! - **Unclaimed LEDs paint black.** `None` cells mean "nothing claims this
//!   LED"; a row-addressed HID write must still send whole rows, and carrying
//!   dead sessions' pixels forward would be exactly the stuck-lighting bug
//!   this project exists to kill. (In practice the app's base stack claims
//!   everything anyway.)
//! - **Legacy boards stream at full rate.** The old "legacy drops writes above
//!   ~6fps" belief was FOLKLORE from the ACK'd path's 10ms poll sleep — a live
//!   wire probe (`neuron::device::tests::live_stream_strategy_probe`) measured
//!   the BlackWidow Chroma V2 sustaining 30fps clean. Legacy's real quirk is
//!   burst sensitivity, handled by the sink's 2ms per-row breathe (below), not
//!   by an fps cap.
//! - **No hotplug yet**: a device that vanishes goes dormant (decimated
//!   control-retry, fire-and-forget writes are inherently silent); re-attach
//!   on replug arrives with the app integration, alongside hidwatch's 20s
//!   monitor pattern.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crate::api::{Grid, HostApi, SurfaceInfo, SurfaceKind};
use crate::arbiter::{LiveContent, Rgb};
use crate::shell::HostHandle;
use crate::writer::{FrameSink, Writer, WriterPauser};

use neuron::device::Device;
use neuron::lighting::{changed_rows_into, LightingDef, Lights, Protocol, Rgb as CoreRgb};
use neuron::pattern::{quantized_t, render_elapsed, Compositor, LayerDef};
use neuron::registry::{Capability, DeviceDef, Registry};
use neuron::transport::{self, DevicePath, HidDeviceInfo};

/// The translation layer that makes the integration feel native: the app's
/// own `Vec<LayerDef>` lighting stack (what the GUI edits, what profiles
/// carry, what prefs persist) rendered as LIVE arbiter content — through the
/// SAME `Compositor`, the SAME process-global render epoch, and the SAME
/// quantized clock as the app's preview, so "the preview provably matches the
/// board" keeps holding when frames flow through the arbiter instead of a
/// per-pid anim thread. The fps atomic is shared with the device's writer:
/// one knob paces both the render quantization and the write cadence, exactly
/// like the app's streams.
pub struct CompositorContent {
    defs: Vec<LayerDef>,
    rows: u8,
    cols: u8,
    fps: Arc<AtomicU32>,
    comp: Compositor,
}

impl CompositorContent {
    pub fn new(defs: Vec<LayerDef>, rows: u8, cols: u8, fps: Arc<AtomicU32>) -> CompositorContent {
        let comp = Compositor::from_defs(&defs);
        CompositorContent {
            defs,
            rows,
            cols,
            fps,
            comp,
        }
    }
}

impl LiveContent for CompositorContent {
    fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
        let fps = self
            .fps
            .load(Ordering::Relaxed)
            .clamp(1, neuron::lighting::MAX_STREAM_FPS);
        let t = quantized_t(render_elapsed(), fps);
        self.comp
            .render(self.rows, self.cols, t)
            .into_iter()
            .map(|c| Some(Rgb(c.r, c.g, c.b)))
            .collect()
    }

    fn boxed_clone(&self) -> Box<dyn LiveContent> {
        Box::new(CompositorContent::new(
            self.defs.clone(),
            self.rows,
            self.cols,
            self.fps.clone(),
        ))
    }
}

/// Stable surface key for the common case: codename + the link-mode PID (wired
/// vs dongle count as the same *model* but distinct link personalities). Used
/// as-is when this (codename, pid) is unique among discovered devices; see
/// [`surface_key_dup`] for what two+ identical units get instead.
pub fn surface_key(def: &DeviceDef, pid: u16) -> String {
    format!("{}-{:04x}", def.codename, pid)
}

/// Disambiguated surface key for when two+ physical devices share (codename,
/// pid) — identical models plugged in together. `instance` is the device's
/// [`path_instance`]; folding it into the key means NEITHER unit keeps the
/// ambiguous bare key once a duplicate exists (both get suffixed, not just
/// the second one seen — enumeration order must not decide who looks "first").
fn surface_key_dup(def: &DeviceDef, pid: u16, instance: &str) -> String {
    format!(
        "{}-{:04x}-{:08x}",
        def.codename,
        pid,
        fnv1a(instance) as u32
    )
}

/// Inline FNV-1a (no new dependency, per the workspace's dependency freeze) — a short, stable
/// suffix to tell identical devices apart in a key. Not a security hash; collisions just mean a
/// rarer disambiguation failure, not an exploitable one.
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// The per-UNIT identity lives in core now (`neuron::transport::path_instance`) so the app's
/// device model and this bridge derive it from the SAME function and can never disagree about
/// which physical unit a path belongs to. Re-exported to keep this module the vocabulary home
/// for surface keying.
pub use neuron::transport::path_instance;

/// Heuristic until the TOMLs carry an explicit kind (see module docs).
pub fn surface_kind(def: &DeviceDef) -> SurfaceKind {
    if def.supports(Capability::Dpi) || def.supports(Capability::SetDpi) {
        SurfaceKind::Mouse
    } else if def.lighting.as_ref().is_some_and(|l| l.rows >= 4) {
        SurfaceKind::Keyboard
    } else {
        SurfaceKind::Generic
    }
}

/// A device def → the surface it exposes, under an already-computed key (see [`discover_from`]
/// for how the key is chosen). `None` for devices without a lighting block — they aren't
/// paintable surfaces (their tuning/battery capabilities join the host through the control plane
/// later, not here).
fn surface_info_with_key(def: &DeviceDef, key: String) -> Option<SurfaceInfo> {
    let l = def.lighting.as_ref()?;
    Some(SurfaceInfo {
        key,
        name: def.name.clone(),
        kind: surface_kind(def),
        leds: l.led_count(),
        grid: Some(Grid {
            rows: l.rows as usize,
            cols: l.cols as usize,
        }),
    })
}

/// A device def → the surface it exposes, keyed the common way (bare `codename-pid`, see
/// [`surface_key`]). `None` for devices without a lighting block.
pub fn surface_info(def: &DeviceDef, pid: u16) -> Option<SurfaceInfo> {
    surface_info_with_key(def, surface_key(def, pid))
}

/// One discovered, bridgeable device (control collection matched, lighting
/// present, not yet opened).
pub struct Discovered {
    pub info: SurfaceInfo,
    def: DeviceDef,
    pid: u16,
    path: DevicePath,
    /// The physical unit this surface belongs to ([`path_instance`]) — what
    /// lets the app address ONE unit of a duplicate pair precisely.
    instance: String,
}

/// Enumerate HID and match against the registry — the only I/O in discovery, split out so
/// [`discover_from`] stays pure and testable with synthetic HID lists.
pub fn discover(reg: &Registry) -> Vec<Discovered> {
    let Ok(hids) = transport::enumerate() else {
        return Vec::new();
    };
    discover_from(&hids, reg)
}

/// The pure core of discovery: match against the registry, collapse one physical device's several
/// HID collections to one entry, and assign every surface a key that's honest about duplicates —
/// two identical devices never share a bare key (see [`surface_key_dup`]). No I/O, so this is
/// exhaustively covered by the tests below with synthetic [`HidDeviceInfo`] lists.
fn discover_from(hids: &[HidDeviceInfo], reg: &Registry) -> Vec<Discovered> {
    struct Matched {
        def: DeviceDef,
        pid: u16,
        path: DevicePath,
        instance: String,
    }
    let mut matched: Vec<Matched> = Vec::new();
    for hid in hids {
        // find_for_pipe: resolve the def that DRIVES this exact collection as its control pipe
        // (family-aware). Discovery matches per-pipe then collapses collections to one physical
        // unit, so on a two-family pid each control pipe reaches the family that can paint it,
        // rather than find_by_pid's first-by-pid def which could be the other family entirely.
        let Some(def) = reg.find_for_pipe(hid) else {
            continue;
        };
        if def.lighting.is_none() {
            continue; // not a paintable surface
        }
        let instance = path_instance(&hid.path.as_os_str().to_string_lossy());
        if matched.iter().any(|m| m.instance == instance) {
            continue; // another collection of the SAME physical device already matched
        }
        matched.push(Matched {
            def: def.clone(),
            pid: hid.pid,
            path: hid.path.clone(),
            instance,
        });
    }

    // Group by (codename, pid) so duplicates — identical models plugged in more than once — are
    // known BEFORE keys are assigned; every member of a duplicate group gets the disambiguated
    // key, not just the second one seen.
    let mut counts: HashMap<(String, u16), usize> = HashMap::new();
    for m in &matched {
        *counts.entry((m.def.codename.clone(), m.pid)).or_insert(0) += 1;
    }

    matched
        .into_iter()
        .filter_map(|m| {
            let dup = counts[&(m.def.codename.clone(), m.pid)] > 1;
            let key = if dup {
                surface_key_dup(&m.def, m.pid, &m.instance)
            } else {
                surface_key(&m.def, m.pid)
            };
            let info = surface_info_with_key(&m.def, key)?;
            Some(Discovered {
                info,
                def: m.def,
                pid: m.pid,
                path: m.path,
                instance: m.instance,
            })
        })
        .collect()
}

/// The HID frame sink: the one place bytes reach this device. Mirrors the
/// proven `Lights::animate` write step; pacing and full-frame dedup live in
/// the [`Writer`], row-level dedup lives here (a one-row change costs one row
/// write, a static frame costs zero).
///
/// Constructed from a RECIPE (def + pid + path) and opened lazily on the
/// writer thread — the `Device` handle is born on, and never leaves, the one
/// thread that writes it. A failed open (device asleep/unplugged) degrades to
/// decimated retries, which is also the replug-recovery path.
pub struct HidSink {
    def: DeviceDef,
    pid: u16,
    path: DevicePath,
    light: LightingDef,
    dev: Option<Device>,
    controlled: bool,
    last: Option<Vec<CoreRgb>>,
    changed: Vec<usize>,
    /// Decimation counter while the device is unreachable: retry roughly
    /// every 32nd frame instead of hammering a dead handle — the vitals
    /// lesson (never herd a sleeping mouse).
    skips: u32,
    /// Round-robin cursor for the ACTIVE (while-flowing) heal: each such heal resends ONE
    /// row, advancing here, so the un-tear still covers the whole board over `rows` heals but
    /// WITHOUT a full-board repaint — which on a legacy board is a ~12ms row-by-row stall that
    /// reads as a periodic flicker under a live game.
    heal_row: usize,
    /// Set by `refresh`; the next `write` folds the round-robin heal row in.
    heal_pending: bool,
}

impl HidSink {
    pub fn new(def: DeviceDef, pid: u16, path: DevicePath) -> HidSink {
        let light = def
            .lighting
            .clone()
            .expect("bridge only sinks lighting devices");
        HidSink {
            def,
            pid,
            path,
            light,
            dev: None,
            controlled: false,
            last: None,
            changed: Vec::new(),
            skips: 0,
            heal_row: 0,
            heal_pending: false,
        }
    }
}

/// The board's honest maximum stream rate.
///
/// Legacy boards were long capped at 6 fps on "frames drop above ~6" folklore — but a live wire
/// probe (`device::tests::live_stream_strategy_probe`, BlackWidow Chroma V2) measured the REAL
/// cost: ~1ms per feature report under the production set+drain discipline, a full 7-report frame
/// in <10ms, 30 fps sustained for 90 frames with zero failures/overruns and the device healthy
/// after. The old ceiling came from the ACK'd write path (10ms first-poll sleep × 7 reports ≈
/// 6 fps), not the silicon — Synapse animates the same board fast, and now so do we. The sink's
/// per-row breathing (2ms) plus the writer's periodic self-heal repaint guard the burst-drop edge
/// the old cap was afraid of.
fn max_fps_for(def: &DeviceDef) -> u32 {
    let _ = def; // per-device ceilings gone; kept as the seam a future genuinely-slow link plugs into
    neuron::lighting::MAX_STREAM_FPS
}

impl FrameSink for HidSink {
    fn write(&mut self, frame: &[Option<Rgb>]) {
        // SAFE-mode parity: the app's process-wide writes-paused kill switch
        // gates this sink exactly like every other device writer.
        if neuron::writes::writes_paused() {
            return;
        }
        // Nothing claims this surface at all → don't touch the silicon. The
        // firmware's latched frame IS the correct display (onboard-first),
        // and this is exactly the app's own "stop" semantics: the stream
        // ends, the board keeps its last frame. A PARTIALLY unclaimed frame
        // still paints (holes go black, deterministically) — only the
        // fully-unclaimed case leaves the device alone.
        //
        // KEEP `self.last`: the board still shows the last written frame, so
        // whatever paints next must be a DELTA against it, NOT a full-board
        // resend. A single all-none tick (a one-frame arbiter gap — e.g. a
        // heartbeat-leased game layer momentarily dropping out) previously
        // nulled `last`, forcing the very next frame to repaint every row —
        // a ~12ms row-by-row wipe on a legacy board = a visible full-board
        // FLASH. Holding `last` makes that gap invisible.
        if frame.iter().all(|c| c.is_none()) {
            return;
        }
        if self.skips > 0 {
            self.skips -= 1;
            return;
        }
        // Open + take host control (driver-mode) lazily and idempotently;
        // while the device is unreachable, degrade to decimated retries.
        if self.dev.is_none() {
            match Device::open_path(self.def.clone(), self.pid, &self.path) {
                Ok(d) => {
                    self.dev = Some(d);
                    self.controlled = false;
                    self.last = None; // fresh handle: never assume board state
                }
                Err(_) => {
                    self.skips = 32;
                    return;
                }
            }
        }
        let dev = self.dev.as_ref().expect("opened above");
        if !self.controlled {
            match Lights::new(dev, self.light.clone()).ensure_control() {
                Ok(()) => self.controlled = true,
                Err(_) => {
                    // Unreachable mid-session (sleep/unplug): drop the handle
                    // so the next attempt reopens from scratch.
                    self.dev = None;
                    self.skips = 32;
                    return;
                }
            }
        }
        let px: Vec<CoreRgb> = frame
            .iter()
            .map(|c| match c {
                Some(Rgb(r, g, b)) => CoreRgb::new(*r, *g, *b),
                None => CoreRgb::BLACK,
            })
            .collect();
        changed_rows_into(
            self.last.as_deref(),
            &px,
            self.light.cols as usize,
            &mut self.changed,
        );
        // Heal (round-robin): resend ONE extra row on top of the naturally-changed ones,
        // advancing the cursor. Over `rows` heals the whole board is re-asserted, so a
        // silently-dropped row un-tears — but no single heal is ever a full-board repaint.
        let n_rows = (self.light.rows as usize).max(1);
        if std::mem::take(&mut self.heal_pending) {
            let r = self.heal_row % n_rows;
            self.heal_row = self.heal_row.wrapping_add(1);
            if !self.changed.contains(&r) {
                self.changed.push(r);
            }
        }
        let mut sent = false;
        for &row in &self.changed {
            if let Some(rep) = self.light.row_report(&px, row) {
                dev.send_lighting_fast(&rep);
                sent = true;
                // Legacy boards have slow HID: a tight burst of row writes can
                // overrun them and rows get silently DROPPED (observed live:
                // a half-red half-stale torn frame). Breathe between rows;
                // matrix boards keep the full-rate path.
                if matches!(self.light.protocol, Protocol::Legacy) {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        }
        if sent {
            dev.send_lighting_fast(&self.light.custom_display_report());
        }
        let buf = self.last.get_or_insert_with(Vec::new);
        buf.clear();
        buf.extend_from_slice(&px);
    }

    /// Writer-driven un-tear (see [`FrameSink::refresh`]). Arms a SINGLE round-robin row for the
    /// next write — never `last = None` (a full-board resend), which on a legacy per-row board is
    /// a ~12ms row-by-row wipe = a visible flash. The writer sweeps the whole board one row per
    /// tick after content stops, so a dropped static row still un-tears without ever flashing.
    fn refresh(&mut self) {
        self.heal_pending = true;
    }
}

/// The running bridge: declared surfaces, their writers, and the app-facing
/// maps (pid → surface key, key → shared fps pace). Dropping it stops and
/// joins every writer (each holding the only handle to its device).
pub struct Bridge {
    pub surfaces: Vec<SurfaceInfo>,
    /// The app speaks pids; protocols speak surface keys. This is the seam — and it's
    /// deliberately one-to-MANY: two identical devices share a pid while being two distinct
    /// surfaces. Pid-level callers (profile apply, anything without a unit in hand) iterate
    /// this; unit-precise callers resolve through [`key_for_unit`](Bridge::key_for_unit).
    by_pid: HashMap<u16, Vec<String>>,
    /// Physical unit ([`path_instance`]) → surface key: the PRECISE resolution the app's
    /// per-unit control plane uses, so an operation aimed at one unit of a duplicate pair
    /// lands on exactly that unit's surface.
    by_unit: HashMap<String, String>,
    /// Per-surface pace shared between the writer AND that surface's
    /// CompositorContent — the GUI fps slider writes here and both follow.
    paces: HashMap<String, Arc<AtomicU32>>,
    grids: HashMap<String, (u8, u8)>,
    /// Per-surface pause valves — how transient readers (getter sweeps,
    /// read-back-verified setters) borrow the device's one feature-report
    /// channel from its streaming writer (see [`WriterPauser`]).
    pausers: HashMap<String, WriterPauser>,
    _writers: Vec<Writer>,
}

impl Bridge {
    /// Every surface this pid maps to — usually one, but two identical devices legitimately
    /// share a pid (see the `by_pid` doc). Empty slice if the pid isn't bridged at all. For
    /// callers WITHOUT a unit in hand (pid-level config operations), applying to every surface
    /// is honest mirroring; anything addressing one physical unit resolves via
    /// [`key_for_unit`](Bridge::key_for_unit) instead.
    pub fn keys_for_pid(&self, pid: u16) -> &[String] {
        self.by_pid.get(&pid).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// The one surface key belonging to a physical unit ([`path_instance`]) — the precise
    /// resolution for per-unit operations. `None` when that unit isn't bridged (no lighting
    /// block, or it appeared after attach); callers must NOT fall back to a pid sibling — that
    /// would silently retarget a different physical device.
    pub fn key_for_unit(&self, unit: &str) -> Option<&String> {
        self.by_unit.get(unit)
    }

    pub fn unit_surfaces(&self) -> Vec<(String, String)> {
        self.by_unit.iter().map(|(unit, key)| (unit.clone(), key.clone())).collect()
    }

    /// The surface's writer pause valve (see [`WriterPauser`]) — `None` if
    /// the surface isn't bridged.
    pub fn pauser(&self, key: &str) -> Option<WriterPauser> {
        self.pausers.get(key).cloned()
    }

    pub fn pace(&self, key: &str) -> Option<Arc<AtomicU32>> {
        self.paces.get(key).cloned()
    }

    /// Live re-pace, same contract as the app's `set_anim_fps`. `false` if
    /// the surface isn't bridged.
    pub fn set_fps(&self, key: &str, fps: u32) -> bool {
        match self.paces.get(key) {
            Some(p) => {
                p.store(
                    fps.clamp(1, neuron::lighting::MAX_STREAM_FPS),
                    Ordering::Relaxed,
                );
                true
            }
            None => false,
        }
    }

    /// The device's LED matrix shape (rows, cols) — what CompositorContent
    /// needs to render the app's LayerDefs onto this surface.
    pub fn grid_of(&self, key: &str) -> Option<(u8, u8)> {
        self.grids.get(key).copied()
    }
}

/// Writer-thread QoS — called ON the writer thread (the sink factory runs
/// there). A 30fps stream needs ~33ms cycles from `thread::sleep`, but when a
/// fullscreen game has focus Windows coarsens background timers to ~15.6ms
/// and deprioritizes the thread — the writer then blows deadlines and drops
/// frames, which is exactly the "lighting goes laggy in-game, fine on the
/// desktop" failure. Two counters, both scoped to intent:
/// - `timeBeginPeriod(1)` — 1ms timer resolution for this process, so paced
///   sleeps wake on time. Never unwound: the writer lives for the process
///   (the OS releases it at exit).
/// - `SetThreadPriority(ABOVE_NORMAL)` — the frame writer outranks bulk
///   background work but never the input/dispatch spine (which runs at
///   normal priority on its own latency budget) nor anything time-critical.
///
/// Raw `#[link]` FFI, no new deps (the workspace manifest is frozen); a
/// no-op off Windows.
fn writer_thread_qos() {
    #[cfg(windows)]
    unsafe {
        #[link(name = "winmm")]
        extern "system" {
            fn timeBeginPeriod(u_period: u32) -> u32;
        }
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThread() -> isize;
            fn SetThreadPriority(h_thread: isize, n_priority: i32) -> i32;
        }
        const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;
        timeBeginPeriod(1);
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

/// Discover, declare, and start one writer per device. `fps` is the requested
/// rate; each board is clamped to its honest maximum. The sink RECIPE crosses
/// into the writer thread; the device opens there (lazily, with retry), so an
/// asleep or slow-to-wake device delays nothing and races nobody.
pub fn attach(reg: &Registry, host: &HostHandle, fps: u32) -> Bridge {
    let mut surfaces = Vec::new();
    let mut writers = Vec::new();
    let mut by_pid = HashMap::new();
    let mut paces = HashMap::new();
    let mut grids = HashMap::new();
    let mut pausers = HashMap::new();
    let mut by_unit = HashMap::new();
    for d in discover(reg) {
        let rate = fps.min(max_fps_for(&d.def));
        let pace = Arc::new(AtomicU32::new(rate));
        let mut h = host.clone();
        h.declare(d.info.clone());
        let key = d.info.key.clone();
        by_pid
            .entry(d.pid)
            .or_insert_with(Vec::new)
            .push(key.clone());
        by_unit.insert(d.instance.clone(), key.clone());
        paces.insert(key.clone(), pace.clone());
        if let Some(l) = d.def.lighting.as_ref() {
            grids.insert(key.clone(), (l.rows, l.cols));
        }
        surfaces.push(d.info);
        let (def, pid, path) = (d.def, d.pid, d.path);
        let writer = Writer::spawn_paced(host.clone(), key.clone(), pace, move || {
            writer_thread_qos();
            HidSink::new(def, pid, path)
        });
        pausers.insert(key, writer.pauser());
        writers.push(writer);
    }
    Bridge {
        surfaces,
        by_pid,
        by_unit,
        paces,
        grids,
        pausers,
        _writers: writers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel builds pure-std, so `writer.rs` MIRRORS neuron-core's stream ceiling and pure
    /// pacing math instead of importing them. This test — in the one module that sees BOTH crates
    /// — is what makes that mirroring safe: any drift in the constant or the pacing behaviour
    /// fails here instead of shipping as a silent cadence divergence.
    #[test]
    fn writer_mirrors_neuron_cores_ceiling_and_pacing() {
        assert_eq!(
            crate::writer::MAX_WRITER_FPS,
            neuron::lighting::MAX_STREAM_FPS,
            "the writer's fps clamp domain must equal the pipeline-wide MAX_STREAM_FPS"
        );
        let t0 = Instant::now();
        let dt = std::time::Duration::from_millis(33);
        for (deadline, now) in [
            (t0 + dt, t0),                                    // ahead of schedule → sleep
            (t0, t0 + std::time::Duration::from_millis(10)),  // small overrun → absorb
            (t0, t0 + std::time::Duration::from_millis(100)), // stall → lag clamps to one dt
            (t0, t0),                                         // exactly on time
        ] {
            assert_eq!(
                crate::writer::pace(deadline, now, dt),
                neuron::lighting::pace(deadline, now, dt),
                "writer::pace must behave identically to lighting::pace"
            );
        }
    }

    /// The embedded registry (the two real device TOMLs) must map to sane
    /// surfaces — this is the hardware-free proof that the TOML → SurfaceInfo
    /// seam holds for every device we actually ship definitions for.
    #[test]
    fn embedded_defs_map_to_coherent_surfaces() {
        let reg = Registry::load().expect("embedded registry parses");
        assert!(!reg.devices.is_empty());
        for def in &reg.devices {
            let Some(l) = def.lighting.as_ref() else {
                continue;
            };
            let pid = def.product_ids().next().expect("every def has a mode");
            let info = surface_info(def, pid).expect("lighting def ⇒ surface");
            assert_eq!(info.leds, l.led_count());
            let g = info.grid.expect("bridged surfaces are grids");
            assert_eq!(g.rows * g.cols, info.leds);
            assert!(
                info.key.starts_with(&def.codename),
                "key {} ~ {}",
                info.key,
                def.codename
            );
            assert!(!info.name.is_empty());
        }
    }

    #[test]
    fn kind_heuristic_separates_the_real_devices() {
        let reg = Registry::load().unwrap();
        let kinds: Vec<(String, SurfaceKind)> = reg
            .devices
            .iter()
            .map(|d| (d.codename.clone(), surface_kind(d)))
            .collect();
        // The Naga (DPI-capable) must classify as a mouse; the BlackWidow
        // (6-row matrix, no DPI) as a keyboard. If a future TOML breaks this,
        // it's the cue to add the explicit kind field instead of patching the
        // heuristic.
        for (codename, kind) in &kinds {
            if codename.contains("naga") {
                assert_eq!(*kind, SurfaceKind::Mouse, "{codename}");
            }
            if codename.contains("blackwidow") {
                assert_eq!(*kind, SurfaceKind::Keyboard, "{codename}");
            }
        }
    }

    #[test]
    fn wired_and_dongle_pids_are_distinct_surfaces_of_one_model() {
        let reg = Registry::load().unwrap();
        for def in &reg.devices {
            let keys: Vec<String> = def.product_ids().map(|pid| surface_key(def, pid)).collect();
            let mut dedup = keys.clone();
            dedup.sort();
            dedup.dedup();
            assert_eq!(
                keys.len(),
                dedup.len(),
                "link modes must not collide: {keys:?}"
            );
        }
    }

    #[test]
    fn path_instance_strips_collection_segments_but_keeps_container_id() {
        let a = r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        // A second collection of the SAME physical device: mi_/col differ, container id doesn't.
        let a2 = r"\\?\hid#vid_1532&pid_0221&mi_00&col01#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_eq!(
            path_instance(a),
            path_instance(a2),
            "same physical device must collapse"
        );

        // A second, physically distinct unit: same collection segment, different container id.
        let b = r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&3a9cd410&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}";
        assert_ne!(
            path_instance(a),
            path_instance(b),
            "different container id must stay distinct"
        );
    }

    /// Build a synthetic HID enumeration entry for `discover_from` tests — no real transport
    /// I/O, so discovery's matching/grouping/keying logic is provable without hardware.
    fn hid(
        vid: u16,
        pid: u16,
        usage_page: u16,
        usage: u16,
        feature_len: u16,
        path: &str,
    ) -> HidDeviceInfo {
        HidDeviceInfo {
            vid,
            pid,
            usage_page,
            usage,
            feature_len,
            // Wave-3 output/input surface (HID++): irrelevant to these razer_report bridge tests.
            input_len: 0,
            output_len: 0,
            path: neuron::transport::DevicePath::from_str_for_tests(path),
            product: String::new(),
        }
    }

    /// The BlackWidow Chroma V2's control collection shape, straight from its TOML, so synthetic
    /// entries actually pass `matches_control`.
    const BW_VID: u16 = 0x1532;
    const BW_PID: u16 = 0x0221;
    const BW_USAGE_PAGE: u16 = 0x0001;
    const BW_USAGE: u16 = 0x0002;
    const BW_FEATURE_LEN: u16 = 91;

    fn bw_hid(path: &str) -> HidDeviceInfo {
        hid(
            BW_VID,
            BW_PID,
            BW_USAGE_PAGE,
            BW_USAGE,
            BW_FEATURE_LEN,
            path,
        )
    }

    #[test]
    fn discover_from_collapses_one_devices_collections_to_one_surface() {
        let reg = Registry::load().unwrap();
        let hids = vec![
            // Main + vendor collections of ONE physical keyboard: only mi_/col differ.
            bw_hid(r"\\?\hid#vid_1532&pid_0221&mi_00&col01#8&2f5ca30f&0&0001#{guid}"),
            bw_hid(r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{guid}"),
        ];
        let out = discover_from(&hids, &reg);
        assert_eq!(out.len(), 1, "one physical device must yield one surface");
        assert_eq!(
            out[0].info.key,
            surface_key_of(&reg, "BlackWidow Chroma V2", BW_PID)
        );
    }

    #[test]
    fn discover_from_gives_two_identical_devices_distinct_hash_suffixed_keys() {
        let reg = Registry::load().unwrap();
        let hids = vec![
            bw_hid(r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{guid}"),
            bw_hid(r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&3a9cd410&0&0001#{guid}"),
        ];
        let out = discover_from(&hids, &reg);
        assert_eq!(
            out.len(),
            2,
            "two physically distinct units must both survive"
        );
        let bare = surface_key_of(&reg, "BlackWidow Chroma V2", BW_PID);
        for d in &out {
            assert_ne!(
                d.info.key, bare,
                "no surface may keep the ambiguous bare key"
            );
            assert!(d.info.key.starts_with(&bare), "key {} ~ {bare}", d.info.key);
        }
        assert_ne!(
            out[0].info.key, out[1].info.key,
            "the two units must get distinct keys"
        );
    }

    #[test]
    fn each_discovered_unit_carries_its_own_instance_for_precise_targeting() {
        // The app's per-unit control plane resolves `path_instance(row) → surface key` through
        // `Bridge::key_for_unit` (built from these Discovered entries). That only works if every
        // Discovered's `instance` is exactly the path_instance of the path it was matched from —
        // for duplicates AND for the unique case.
        let reg = Registry::load().unwrap();
        let paths = [
            r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{guid}",
            r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&3a9cd410&0&0001#{guid}",
        ];
        let hids: Vec<_> = paths.iter().map(|p| bw_hid(p)).collect();
        let out = discover_from(&hids, &reg);
        assert_eq!(out.len(), 2);
        for (d, p) in out.iter().zip(paths.iter()) {
            assert_eq!(
                d.instance,
                path_instance(p),
                "instance must be the unit identity of the path it matched"
            );
        }
        assert_ne!(out[0].instance, out[1].instance);
        assert_ne!(
            out[0].info.key, out[1].info.key,
            "distinct instances must resolve to distinct surface keys — the bijection key_for_unit serves"
        );
    }

    #[test]
    fn discover_from_keeps_bare_keys_for_two_different_models() {
        let reg = Registry::load().unwrap();
        let naga = reg
            .devices
            .iter()
            .find(|d| d.codename == "Aria")
            .expect("Naga def present");
        let naga_pid = naga.product_ids().next().unwrap();
        let hids = vec![
            bw_hid(r"\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{guid}"),
            hid(
                naga.vendor_id,
                naga_pid,
                naga.control_interface.usage_page,
                naga.control_interface.usage,
                naga.control_interface.feature_report_len,
                r"\\?\hid#vid_1532&pid_00a7&mi_02&col03#9&1a2b3c4d&0&0002#{guid}",
            ),
        ];
        let out = discover_from(&hids, &reg);
        assert_eq!(out.len(), 2);
        assert_eq!(
            out[0].info.key,
            surface_key_of(&reg, "BlackWidow Chroma V2", BW_PID)
        );
        assert_eq!(out[1].info.key, surface_key_of(&reg, "Aria", naga_pid));
    }

    /// Look up a def by codename and compute its (unique-case) key the same way production does
    /// — kept as a helper so the tests above assert against the real `surface_key`, not a
    /// hand-copied string.
    fn surface_key_of(reg: &Registry, codename: &str, pid: u16) -> String {
        let def = reg
            .devices
            .iter()
            .find(|d| d.codename == codename)
            .expect("def present");
        surface_key(def, pid)
    }
}
