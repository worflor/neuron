//! The neuron-core bridge — real hardware becomes kernel surfaces.
//!
//! This is where the R&D stops being abstract: the registry's device TOMLs map
//! to [`SurfaceInfo`]s, and each device gets ONE [`Writer`] whose sink speaks
//! the proven HID write recipe (the exact inner step of `Lights::animate`:
//! changed rows → `row_report` → `send_lighting_fast` → latch). Everything
//! else — the GUI, protocol adapters, telemetry bindings — paints through the
//! arbiter. The bridge deliberately does NOT hand out device handles: the
//! whole point (§9.3) is that the writer is the only writer.
//!
//! Honest edges, stated up front:
//! - **Kind is a heuristic** (DPI-capable ⇒ mouse; tall LED matrix ⇒
//!   keyboard) until the device TOMLs grow an explicit `kind` field — the
//!   right fix, noted in the R&D doc.
//! - **Unclaimed LEDs paint black.** `None` cells mean "nothing claims this
//!   LED"; a row-addressed HID write must still send whole rows, and carrying
//!   dead sessions' pixels forward would be exactly the stuck-lighting bug
//!   this project exists to kill. (In practice the app's base stack claims
//!   everything anyway.)
//! - **Legacy boards are paced at 6fps** regardless of the requested rate —
//!   they physically drop writes above that (hardware-confirmed on the
//!   BlackWidow Chroma V2). Asking for more would be flicker, not honesty.
//! - **No hotplug yet**: a device that vanishes goes dormant (decimated
//!   control-retry, fire-and-forget writes are inherently silent); re-attach
//!   on replug arrives with the app integration, alongside hidwatch's 20s
//!   monitor pattern.

use crate::api::{Grid, HostApi, SurfaceInfo, SurfaceKind};
use crate::arbiter::Rgb;
use crate::shell::HostHandle;
use crate::writer::{FrameSink, Writer};

use neuron::device::Device;
use neuron::lighting::{changed_rows_into, LightingDef, Lights, Protocol, Rgb as CoreRgb};
use neuron::registry::{Capability, DeviceDef, Registry};
use neuron::transport::{self, DevicePath};

/// Stable surface key: codename + the link-mode PID (wired vs dongle count as
/// the same *model* but distinct link personalities; two identical devices on
/// one machine are a future refinement, noted honestly).
pub fn surface_key(def: &DeviceDef, pid: u16) -> String {
    format!("{}-{:04x}", def.codename, pid)
}

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

/// A device def → the surface it exposes. `None` for devices without a
/// lighting block — they aren't paintable surfaces (their tuning/battery
/// capabilities join the host through the control plane later, not here).
pub fn surface_info(def: &DeviceDef, pid: u16) -> Option<SurfaceInfo> {
    let l = def.lighting.as_ref()?;
    Some(SurfaceInfo {
        key: surface_key(def, pid),
        name: def.name.clone(),
        kind: surface_kind(def),
        leds: l.led_count(),
        grid: Some(Grid { rows: l.rows as usize, cols: l.cols as usize }),
    })
}

/// One discovered, bridgeable device (control collection matched, lighting
/// present, not yet opened).
pub struct Discovered {
    pub info: SurfaceInfo,
    def: DeviceDef,
    pid: u16,
    path: DevicePath,
}

/// Enumerate HID, match against the registry, keep exactly one control
/// collection per physical device.
pub fn discover(reg: &Registry) -> Vec<Discovered> {
    let mut out: Vec<Discovered> = Vec::new();
    let Ok(devices) = transport::enumerate() else {
        return out;
    };
    for hid in devices {
        let Some(def) = reg.find_by_pid(hid.vid, hid.pid) else { continue };
        if !def.matches_control(hid.usage_page, hid.usage, hid.feature_len) {
            continue;
        }
        let Some(info) = surface_info(def, hid.pid) else { continue };
        if out.iter().any(|d| d.info.key == info.key) {
            continue; // composite devices expose several collections; one wins
        }
        out.push(Discovered { info, def: def.clone(), pid: hid.pid, path: hid.path });
    }
    out
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
}

impl HidSink {
    pub fn new(def: DeviceDef, pid: u16, path: DevicePath) -> HidSink {
        let light = def.lighting.clone().expect("bridge only sinks lighting devices");
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
        }
    }
}

/// The board's honest maximum stream rate (see module docs).
fn max_fps_for(def: &DeviceDef) -> u32 {
    match def.lighting.as_ref().map(|l| l.protocol) {
        Some(Protocol::Legacy) => 6,
        _ => 30,
    }
}

impl FrameSink for HidSink {
    fn write(&mut self, frame: &[Option<Rgb>]) {
        // SAFE-mode parity: the app's process-wide writes-paused kill switch
        // gates this sink exactly like every other device writer.
        if neuron::writes::writes_paused() {
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
        changed_rows_into(self.last.as_deref(), &px, self.light.cols as usize, &mut self.changed);
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

    /// Writer-driven periodic repaint (see [`FrameSink::refresh`]): forget
    /// what we believe is on the board so the next write resends every row —
    /// the self-healing pass that un-tears a board that dropped writes.
    fn refresh(&mut self) {
        self.last = None;
    }
}

/// The running bridge: declared surfaces + their writers. Dropping it stops
/// and joins every writer (each holding the only handle to its device).
pub struct Bridge {
    pub surfaces: Vec<SurfaceInfo>,
    _writers: Vec<Writer>,
}

/// Discover, declare, and start one writer per device. `fps` is the requested
/// rate; each board is clamped to its honest maximum. The sink RECIPE crosses
/// into the writer thread; the device opens there (lazily, with retry), so an
/// asleep or slow-to-wake device delays nothing and races nobody.
pub fn attach(reg: &Registry, host: &HostHandle, fps: u32) -> Bridge {
    let mut surfaces = Vec::new();
    let mut writers = Vec::new();
    for d in discover(reg) {
        let rate = fps.min(max_fps_for(&d.def));
        let mut h = host.clone();
        h.declare(d.info.clone());
        let key = d.info.key.clone();
        surfaces.push(d.info);
        let (def, pid, path) = (d.def, d.pid, d.path);
        writers.push(Writer::spawn(host.clone(), key, rate, move || {
            HidSink::new(def, pid, path)
        }));
    }
    Bridge { surfaces, _writers: writers }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded registry (the two real device TOMLs) must map to sane
    /// surfaces — this is the hardware-free proof that the TOML → SurfaceInfo
    /// seam holds for every device we actually ship definitions for.
    #[test]
    fn embedded_defs_map_to_coherent_surfaces() {
        let reg = Registry::load().expect("embedded registry parses");
        assert!(!reg.devices.is_empty());
        for def in &reg.devices {
            let Some(l) = def.lighting.as_ref() else { continue };
            let pid = def.product_ids().next().expect("every def has a mode");
            let info = surface_info(def, pid).expect("lighting def ⇒ surface");
            assert_eq!(info.leds, l.led_count());
            let g = info.grid.expect("bridged surfaces are grids");
            assert_eq!(g.rows * g.cols, info.leds);
            assert!(info.key.starts_with(&def.codename), "key {} ~ {}", info.key, def.codename);
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
            let keys: Vec<String> =
                def.product_ids().map(|pid| surface_key(def, pid)).collect();
            let mut dedup = keys.clone();
            dedup.sort();
            dedup.dedup();
            assert_eq!(keys.len(), dedup.len(), "link modes must not collide: {keys:?}");
        }
    }
}
