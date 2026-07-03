//! The engine-facing runtime — Neuron's logic state, owned by the GUI process.
//!
//! This is the thin layer between the Slint view and `neuron-core`. It holds the device registry
//! and the loaded config (bindings, cast, profiles, app-rules, gesture vault), and exposes typed
//! operations the glue layer calls on the UI thread. Device I/O is fast getter/setter round-trips
//! done on demand (a device is opened, used, dropped) — matching the CLI's pattern and keeping the
//! non-`Send` transport off worker threads. Long-running lighting animation gets its own thread
//! that re-enumerates its own device handle.
//!
//! Direct typed calls only — no serde-over-IPC. The GUI is a thin view over Profile / Bindings /
//! CastConfig / Engine.

use neuron::bindings::Bindings;
use neuron::capability::{self as cap, Store};
use neuron::cast::CastConfig;
use neuron::device::Device;
use neuron::engine::{Rule, Trigger};
use neuron::gesture::Vault;
use neuron::lighting::{Effect, Lights, Rgb};
use neuron::profile::{AppRule, AppRules, Profile};
use neuron::registry::{DeviceDef, Registry};
use neuron::transport;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// A snapshot of one enumerated device's live state, ready to map into a `DeviceRow`.
pub struct DeviceState {
    pub name: String,
    pub codename: String,
    pub pid: u16,
    /// The PHYSICAL unit this row is (`transport::path_instance`) — the identity that tells two
    /// identical devices apart. Every control-plane operation the row triggers (opens, setters,
    /// streams, host lighting) targets this unit, never "the first HID that shares my pid".
    pub instance: String,
    pub mode: String,
    pub connected: bool,
    pub firmware: String,
    pub dpi: String,
    pub polling: String,
    pub brightness: String,
    pub battery: String,
    pub charging: bool,
    pub storage: String,
    pub icon: &'static str,
    // numeric twins of the display strings (None = unread/asleep) — these seed the FEEL
    // controls so the sliders show hardware truth, not compile-time defaults.
    pub dpi_n: Option<u16>,
    pub polling_n: Option<u32>,
    pub brightness_n: Option<u8>,
    /// 0..1 battery level for the drawn cell gauge (None = no battery / unread).
    pub battery_frac: Option<f32>,
    // ── registry-driven CAPABILITY flags — what this device can actually do, from its descriptor
    // (`DeviceDef::supports`). The Device panel gates each control on these so a keyboard never shows
    // a DPI fader and a mouse without onboard storage never shows the persist toggle. Honest by data.
    pub cap_dpi: bool,  // SetDpi — DPI fader, DPI stages, sniper, lift-off/debounce
    pub cap_poll: bool, // SetPolling — polling contacts + in-game polling
    pub cap_light: bool, // Lighting — the brightness fader
    pub cap_bright: bool, // Brightness (the GETTER) — the LIGHT readout; a device that can set but
    // never report brightness (the legacy BlackWidow) must not show a readout that reads "—" forever
    pub cap_scroll: bool, // SetScrollStage — scroll-wheel stages
    pub cap_store: bool, // Storage — persist-to-onboard
    pub cap_idle: bool, // Battery (wireless proxy) — the idle-off timer
    pub cap_plate: bool, // has a [side_plates] map — surfaces the push-detected side-plate readout
}

/// One live lighting stream's controls, owned PER-DEVICE in [`AppRuntime::anim`]: the stop flag its
/// worker polls each frame, and the fps it reads live. Every stream is the layer compositor now (vitals
/// is just a `vitals` LAYER in the stack, not a bespoke paint-on-demand surface), so the fps slider
/// always applies to whatever board is streaming.
pub struct AnimStream {
    pub stop: Arc<AtomicBool>,
    pub fps: Arc<AtomicU32>,
}

/// The resident runtime state. UI-thread owned (held in an `Rc<RefCell<_>>` by the glue).
pub struct AppRuntime {
    pub registry: Registry,
    pub bindings: Bindings,
    pub cast: CastConfig,
    pub vault: Vault,
    pub app_rules: AppRules,
    pub profiles: Vec<Profile>,
    pub active_profile: String,
    pub persist: bool,
    /// Currently selected device pid (for per-device panels). 0 = none. The pid names the MODEL/
    /// link-mode (capability gates, per-model config); `selected_unit` names the physical unit.
    pub selected_pid: u16,
    /// The selected PHYSICAL unit (`transport::path_instance`) — what makes selection precise when
    /// two identical devices share a pid. Empty = no unit pinned (match by pid alone), which only
    /// happens before the first scan.
    pub selected_unit: String,
    /// Live lighting streams, keyed by physical UNIT (`path_instance`). Each board gets its OWN
    /// stop flag + fps, so starting, stopping, or re-pacing one board's lighting NEVER touches
    /// another's — including its identical twin on the same pid — and switching which board you're
    /// editing leaves the others streaming.
    pub anim: HashMap<String, AnimStream>,
    /// The fps the GUI slider shows for the SELECTED board (seeded on selection: legacy → 6, matrix →
    /// 30). On apply it SEEDS that board's stream fps; moving the slider re-paces the selected board's
    /// live stream. Each running stream owns its own fps copy (in `anim`), so re-pacing or selecting a
    /// different board can't change another board's speed. The data/vitals surface ignores fps.
    pub light_fps: Arc<AtomicU32>,
    /// The host-side gaming-mode suppression policy from the last-applied profile (Alt+Tab/Win/
    /// Alt+F4). A GUI-hosted daemon LL-keyboard hook consults this; carried so apply stays the
    /// canonical source. Empty by default (nothing suppressed).
    pub gaming_mode: neuron::writes::GamingMode,
}

impl AppRuntime {
    pub fn load() -> Self {
        let registry = Registry::load().unwrap_or(Registry {
            devices: Vec::new(),
        });
        let profiles = neuron::profile::list()
            .into_iter()
            .filter_map(|n| Profile::load(&n).ok())
            .collect();
        AppRuntime {
            registry,
            bindings: Bindings::load(),
            cast: CastConfig::load(),
            vault: Vault::load(),
            app_rules: AppRules::load(),
            profiles,
            active_profile: "—".into(),
            persist: false,
            selected_pid: 0,
            selected_unit: String::new(),
            anim: HashMap::new(),
            light_fps: Arc::new(AtomicU32::new(30)),
            gaming_mode: neuron::writes::GamingMode::default(),
        }
    }

    fn store(&self) -> Store {
        Store::from_persist(self.persist)
    }

    fn writes_paused(&self) -> bool {
        neuron::writes::writes_paused()
    }

    // ── device enumeration + live reads ──────────────────────────────────

    /// Enumerate every recognized device and read its live state (best-effort; unread fields
    /// show "—" so an asleep wireless mouse still lists). Read-only — always safe.
    ///
    /// One row per PHYSICAL UNIT, not per pid: a device's several HID collections collapse to one
    /// row via `path_instance`, but two identical devices (same pid, two units) get two rows —
    /// each read through its OWN control path, so the readouts are that unit's truth, never the
    /// truth of whichever twin enumerated first.
    pub fn scan_devices(&mut self) -> Vec<DeviceState> {
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        // Pass 1 — resolve units before any device I/O: dedupe collections to units, and learn
        // which pids have duplicate units so naming + vitals routing can be decided up front.
        struct Unit {
            def: DeviceDef,
            pid: u16,
            path: transport::DevicePath,
            instance: String,
        }
        let mut units: Vec<Unit> = Vec::new();
        for i in &infos {
            let Some(def) = self.registry.find_by_pid(i.vid, i.pid) else {
                continue;
            };
            if !def.matches_control(i.usage_page, i.usage, i.feature_len) {
                continue;
            }
            let instance = i.instance();
            if units.iter().any(|u| u.instance == instance) {
                continue; // another collection of the SAME physical unit
            }
            units.push(Unit {
                def: def.clone(),
                pid: i.pid,
                path: i.path.clone(),
                instance,
            });
        }
        let mut out = Vec::new();
        for u in units.iter() {
            // Duplicate group = other units sharing this (codename, pid). Numbering is by
            // instance ORDER, not enumeration order, so "· 1"/"· 2" stay glued to the same
            // physical unit across rescans (enumeration order is not stable; instances are).
            let twins: Vec<&str> = units
                .iter()
                .filter(|o| o.pid == u.pid && o.def.codename == u.def.codename)
                .map(|o| o.instance.as_str())
                .collect();
            // The battery edge-detector (`vitals::observe`) is pid-keyed core state; feeding it
            // from BOTH twins would interleave two batteries into one series and fabricate
            // charge/drop edges. Route it from the pid's lowest instance only — one stable unit.
            let feed_vitals =
                twins.iter().min().copied() == Some(u.instance.as_str());
            let mut st = read_device_state(&u.def, u.pid, &u.path, feed_vitals);
            st.instance = u.instance.clone();
            if twins.len() > 1 {
                let nth = {
                    let mut sorted = twins.clone();
                    sorted.sort_unstable();
                    sorted.iter().position(|s| *s == u.instance).unwrap_or(0) + 1
                };
                st.name = format!("{} · {}", st.name, nth);
            }
            out.push(st);
        }
        // Selection follows reality: if the selected unit is no longer enumerated (unplugged,
        // dongle gone) — or nothing was selected yet — adopt the first recognized unit so the
        // per-device panels never target a ghost. pid 0 / empty unit never match a row, so this
        // one branch covers both first-scan auto-pick and stale-selection healing.
        if !out
            .iter()
            .any(|d| d.pid == self.selected_pid && d.instance == self.selected_unit)
        {
            self.selected_pid = out.first().map(|d| d.pid).unwrap_or(0);
            self.selected_unit = out.first().map(|d| d.instance.clone()).unwrap_or_default();
        }
        out
    }

    /// Open the currently-selected device (or the first recognized one).
    ///
    /// Unit-precise: when a physical unit is pinned (`selected_unit`), only THAT unit's control
    /// interface matches — two identical devices sharing a pid can never swap under a setter or
    /// a read. If the pinned unit is gone (unplugged between scans, replugged into another
    /// port), fall back to pid matching — the same "selection follows reality" healing
    /// `scan_devices` does, just mid-cycle.
    ///
    /// Returns a [`GatedDevice`]: while the handle lives, the device's host
    /// lighting writer is PARKED (see `crate::host::io_gate`), because a
    /// streaming writer's `set_feature` clobbers the pending reply of any
    /// concurrent getter on the device's ONE feature-report channel — the
    /// race that read every getter as "—" and could fail a read-back verify.
    /// Derefs to [`Device`], so every getter/setter call site is unchanged.
    pub fn open_selected(&self) -> anyhow::Result<GatedDevice> {
        let infos = transport::enumerate()?;
        let unit_pass = [false, true]; // pass 0: exact unit; pass 1: pid-only healing
        for relaxed in unit_pass {
            for i in &infos {
                if let Some(def) = self.registry.find_by_pid(i.vid, i.pid) {
                    if def.matches_control(i.usage_page, i.usage, i.feature_len)
                        && (self.selected_pid == 0 || i.pid == self.selected_pid)
                        && (relaxed
                            || self.selected_unit.is_empty()
                            || i.instance() == self.selected_unit)
                    {
                        let gate = crate::host::io_gate(i.pid);
                        return Device::open_path(def.clone(), i.pid, &i.path)
                            .map(|dev| GatedDevice { _gate: gate, dev });
                    }
                }
            }
        }
        anyhow::bail!("no recognized Razer device connected")
    }

    /// The def of the selected device, if known/connected.
    pub fn selected_def(&self) -> Option<DeviceDef> {
        let infos = transport::enumerate().ok()?;
        for i in &infos {
            if let Some(def) = self.registry.find_by_pid(i.vid, i.pid) {
                if def.matches_control(i.usage_page, i.usage, i.feature_len)
                    && (self.selected_pid == 0 || i.pid == self.selected_pid)
                {
                    return Some(def.clone());
                }
            }
        }
        None
    }

    // ── performance setters (gated) ──────────────────────────────────────

    pub fn apply_dpi(&self, dpi: u16) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => {
                let _ = d.run("device_mode"); // wake / ensure reachable
                match cap::set_dpi(&d, dpi, dpi, self.store()) {
                    Ok(_) => {
                        // confirmation fires past the committed write — same as apply_polling /
                        // apply_brightness (was missing here, so GUI DPI changes earned no card).
                        // Absolute set → no prior read, so no old→new (matches Intent::DpiSet).
                        // Per-device de-dup keyed by the device we just opened + wrote.
                        neuron::confirm::dpi(d.pid, dpi as u32, None);
                        format!("DPI -> {dpi}")
                    }
                    Err(e) => format!("DPI failed: {e}"),
                }
            }
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply a polling rate. Returns the status line plus the SNAPPED rate the device actually
    /// took (so the UI control can settle onto the real detent, not the dragged value).
    pub fn apply_polling(&self, hz: u32) -> (String, Option<u32>) {
        if self.writes_paused() {
            return ("writes paused".into(), None);
        }
        match self.open_selected() {
            Ok(d) => match cap::set_polling_hz(&d, hz) {
                Ok(actual) => {
                    // confirmation fires past the committed write — and NOT on the profile-apply
                    // path (that goes through apply_with_session), so a profile switch stays one card.
                    neuron::confirm::polling(actual, None);
                    (format!("polling -> {actual} Hz"), Some(actual))
                }
                Err(e) => (format!("polling failed: {e}"), None),
            },
            Err(e) => (format!("no device: {e}"), None),
        }
    }

    pub fn apply_brightness(&self, pct: u8) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => match cap::set_brightness(&d, pct, self.store()) {
                Ok(_) => {
                    neuron::confirm::brightness(pct as u32, None);
                    format!("brightness -> {pct}%")
                }
                Err(e) => format!("brightness failed: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    // ── surfaced perf controls (Synapse buries these) ─────────────────────

    /// Read the device's current LED idle-off timeout (seconds) — the read side of `set_idle_secs`,
    /// proven live at 0x07/0x83. `None` if no device / asleep / unsupported.
    pub fn read_idle_secs(&self) -> Option<u16> {
        let d = self.open_selected().ok()?;
        cap::idle_timeout_secs(&d).ok()
    }

    /// Apply the full DPI STAGE LIST (the cycle) as one table — `writes::set_dpi_stages`, the
    /// verify-gated write. `list` is "/"-separated DPI values; `active` is the active stage index.
    pub fn apply_dpi_stages(&self, list: &str, active: u8) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        // Parse with ACCOUNTING: a precision instrument never guesses. Garbage or out-of-range
        // tokens refuse the whole apply with the offenders named, rather than silently writing
        // half the list the user typed.
        let mut stages: Vec<neuron::writes::DpiStage> = Vec::new();
        let mut bad: Vec<String> = Vec::new();
        for tok in list
            .split(['/', ',', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            match tok.parse::<u16>() {
                Ok(v) if (100..=30_000).contains(&v) => {
                    stages.push(neuron::writes::DpiStage::symmetric(v));
                }
                _ => bad.push(tok.to_string()),
            }
        }
        if !bad.is_empty() {
            return format!(
                "invalid stage(s) {} — use numbers 100–30000, e.g. 800/1600/3200",
                bad.join(", ")
            );
        }
        if stages.is_empty() {
            return "no DPI stages — e.g. 800/1600/3200".into();
        }
        // a stale UI index must never ship out-of-range to the device.
        let active = (active as usize).min(stages.len() - 1) as u8;
        match self.open_selected() {
            Ok(d) => {
                let _ = d.run("device_mode");
                match neuron::writes::set_dpi_stages(&d, &stages, active, self.store()) {
                    Ok(()) => format!("DPI stages [{}] active {}", fmt_stages(&stages), active + 1),
                    Err(e) => format!("DPI stages failed: {e}"),
                }
            }
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Read the device's current DPI stage table (the cycle) — best-effort, `[]` when absent.
    /// The read side of `apply_dpi_stages`, used to seed the editor with hardware truth.
    pub fn read_dpi_stages(&self) -> Vec<u16> {
        let mut out = Vec::new();
        if let Ok(d) = self.open_selected() {
            if let Ok(s) = d.run("dpi_stages") {
                if s.len() > 2 {
                    let count = s[2] as usize;
                    for i in 0..count {
                        let off = 3 + i * 7; // [id, Xhi, Xlo, Yhi, Ylo, 0, 0]
                        if off + 2 < s.len() {
                            let x = ((s[off + 1] as u16) << 8) | s[off + 2] as u16;
                            if x > 0 {
                                out.push(x);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Apply HyperScroll wheel stages (class 0x0B) — verify-gated + hardware-pending; surfaces the
    /// honest gated message when the env flag is unset. `list` is "/"-separated mode names/bytes.
    pub fn apply_scroll_stages(&self, list: &str) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        // map "tactile"/"free" friendly names to mode bytes (0 tactile / 1 free-spin).
        let mut modes = Vec::new();
        let mut bad = Vec::new();
        for tok in list
            .split(['/', ',', ' '])
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            let t = tok.to_lowercase();
            if t.starts_with("free") {
                modes.push(1u8);
            } else if t.starts_with("tact") {
                modes.push(0u8);
            } else if let Ok(v) = t.parse::<u8>() {
                modes.push(v);
            } else {
                bad.push(tok.to_string());
            }
        }
        if !bad.is_empty() {
            return format!(
                "invalid scroll mode(s) {} — use tactile/free",
                bad.join(", ")
            );
        }
        if modes.is_empty() {
            return "no scroll modes — e.g. tactile/free".into();
        }
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_scroll_stages(&d, &modes, 0, self.store()) {
                Ok(()) => format!("scroll stages ({} mode(s)) applied", modes.len()),
                Err(e) => format!("scroll stages [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply the LED idle-off timeout (seconds) — verify-gated (NEURON_IDLE_WRITE), honest [gated].
    pub fn apply_idle(&self, secs: u32) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_idle_secs(&d, secs) {
                Ok(()) => format!("idle-off -> {secs}s"),
                Err(e) => format!("idle-off [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply the in-game polling split (wired/dongle Hz) — verify-gated (NEURON_INGAME_POLL_WRITE).
    pub fn apply_ingame_polling(&self, wired: u32, dongle: u32) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_in_game_polling(&d, wired, dongle) {
                Ok(()) => format!("in-game polling {wired}/{dongle} Hz"),
                Err(e) => format!("in-game polling [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply the symmetric LIFT-OFF DISTANCE level (0 low / 1 medium / 2 high) — verify-gated
    /// (`writes::set_lift_off_distance` re-reads 0x0B/0x85) + env-gated (NEURON_LOD_WRITE).
    pub fn apply_lift_off_distance(&self, level: u8) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        let label = lod_label(level);
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_lift_off_distance(&d, level) {
                Ok(()) => format!("lift-off distance -> {label}"),
                Err(e) => format!("lift-off distance [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Read the device's current symmetric LIFT-OFF DISTANCE level (0..2) — the read side of
    /// `apply_lift_off_distance` (0x0B/0x85). `None` if no device / asleep / unsupported.
    pub fn lift_off_distance(&self) -> Option<u8> {
        let d = self.open_selected().ok()?;
        neuron::writes::lift_off_distance(&d).ok()
    }

    /// Apply the ASYMMETRIC LIFT-OFF distance — separate LIFT (2..=26) and LANDING (1..=25) levels —
    /// verify-gated (`writes::set_lift_off_asymmetric` re-reads the shared 0x0B/0x85 getter and
    /// confirms mode=async + the pair). The split / Focus-Pro-30K flex; mirrors `apply_lift_off_distance`.
    pub fn apply_lift_off_asymmetric(&self, lift: u8, landing: u8) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_lift_off_asymmetric(&d, lift, landing) {
                Ok(()) => format!("lift-off split -> lift {lift} / landing {landing}"),
                Err(e) => format!("lift-off split [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Read the device's current ASYMMETRIC lift-off pair `(lift, landing)` (1-based levels) — the
    /// read side of `apply_lift_off_asymmetric`. `Some` only when the device is in async (split) mode;
    /// `None` if symmetric / no device / asleep / unsupported.
    pub fn lift_off_async(&self) -> Option<(u8, u8)> {
        let d = self.open_selected().ok()?;
        neuron::writes::lift_off_async(&d)
    }

    /// Apply Snap Tap (SOCD) on/off — the MECHANICAL ADVANTAGES "edge" write. Verify-gated +
    /// env-gated (`writes::set_snap_tap` re-reads 0x02/0xA7; `NEURON_SNAP_TAP_WRITE` opens the
    /// boundary). Uses the default A/D counter-strafe pair. Honest `[gated]` on hardware that can't
    /// do it (the user's BlackWidow Chroma V2), exactly like the LOD / idle / in-game-polling writes.
    pub fn apply_snap_tap(&self, enable: bool) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        let pairs = [neuron::writes::SnapTapPair::ad()];
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_snap_tap(&d, &pairs, enable) {
                Ok(()) => format!("snap tap -> {}", if enable { "on" } else { "off" }),
                Err(e) => format!("snap tap [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    // ── lighting ─────────────────────────────────────────────────────────

    /// Effects available on the selected device (native first, then emulated).
    pub fn effects(&self) -> Vec<(String, bool, bool)> {
        match self.selected_def().and_then(|d| d.lighting.clone()) {
            Some(def) => def
                .available()
                .into_iter()
                .map(|e| (e.name().to_string(), def.supports_native(e), e.uses_color()))
                .collect(),
            None => Vec::new(),
        }
    }

    /// Grid dimensions of the selected device's lighting (rows, cols). `(0, 0)` when no lit
    /// device is selected — the editor renders its honest empty state instead of a fake matrix
    /// implying hardware that isn't there.
    pub fn grid_dims(&self) -> (u8, u8) {
        self.selected_def()
            .and_then(|d| d.lighting.clone())
            .map(|l| (l.rows, l.cols))
            .unwrap_or((0, 0))
    }

    /// The selected device's kind hint ("keyboard"/"mouse"/…) — drives the procedural chassis
    /// the lighting render draws around the LED lattice. Empty when nothing is selected.
    pub fn grid_kind(&self) -> &'static str {
        self.selected_def().map(|d| icon_for(&d)).unwrap_or("")
    }

    /// The DEFAULT streaming fps for the selected lit device + whether it's a LEGACY board (the
    /// GUI's protocol note). Both protocols now default to 30: the old legacy-6 seed encoded
    /// "frames drop above ~6" folklore that a live wire probe falsified — the BlackWidow sustains
    /// 30 fps cleanly under the production write discipline (see `max_fps_for` in the host bridge
    /// for the measurements). `None` when the selection has no lighting. Seeds the GUI fps control
    /// on selection — the user tunes from there.
    pub fn light_fps_default(&self) -> Option<(u32, bool)> {
        self.selected_def()
            .and_then(|d| d.lighting)
            .map(|l| match l.protocol {
                neuron::lighting::Protocol::Legacy => (30, true),
                neuron::lighting::Protocol::Matrix => (30, false),
            })
    }

    pub fn apply_effect(&self, name: &str, color: Rgb) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        let Some(eff) = Effect::from_name(name) else {
            return format!("unknown effect '{name}'");
        };
        match self.open_selected() {
            Ok(d) => {
                let Some(def) = d.def.lighting.clone() else {
                    return "device has no lighting".into();
                };
                let lights = Lights::new(&d, def);
                if let Err(e) = lights.ensure_control() {
                    return format!("control failed: {e}");
                }
                let col = if eff.uses_color() { Some(color) } else { None };
                match lights.set_effect(eff, col, self.persist) {
                    Ok(_) => format!("effect -> {name}"),
                    Err(e) => format!("effect failed: {e}"),
                }
            }
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Stream the LAYER COMPOSITOR live on a worker thread (re-opens its own device). The compositor is
    /// built from the whole layer stack (Pattern × Spectrum layers).
    /// The layered composite is inherently the custom-frame path (it streams blended frames).
    ///
    /// `unit` is the physical unit's `path_instance` — the stream targets exactly that board.
    /// Empty = "any board with this pid" (only legitimate for pre-scan callers; the GUI always
    /// has a unit in hand).
    pub fn start_layers(
        &mut self,
        defs: Vec<neuron::pattern::LayerDef>,
        pid: u16,
        unit: &str,
        on_done: impl FnOnce(Option<String>, Arc<AtomicBool>) + Send + 'static,
    ) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        if defs.is_empty() {
            return "no layers".into();
        }
        // Stop THIS board's prior LOCAL stream first — before EITHER pipe below — so a board that
        // was streaming app-side when the host came up (the runtime host toggle) can't end up with
        // both the old anim thread AND the host writer painting it: the exact double-writer race
        // the host exists to kill. Other boards keep streaming.
        if let Some(a) = self.anim.remove(unit) {
            a.stop.store(true, Ordering::SeqCst);
        }
        // HOST INTEGRATION: when the protocol host owns this device's writer,
        // the app must NOT stream on its own thread. Push the stack as the
        // host's animated BASE layer instead — games/tools paint above it and
        // it returns when they release. No app-side stream to reconcile, so
        // on_done is skipped; the "compositing" indicator reads host::has_lighting.
        if crate::host::active() {
            let fps = self.light_fps.load(Ordering::Relaxed);
            if crate::host::set_lighting(pid, unit, defs.clone(), fps) {
                return "compositing (host)".into();
            }
            // Device not bridged by the host — fall through to a local stream
            // (defs still owned; the clone above is only spent on the host path).
        }
        let stop = Arc::new(AtomicBool::new(false));
        // this stream's OWN fps copy, seeded from the GUI's current value — so re-pacing or selecting
        // another board can never change THIS stream's speed.
        let fps_src = Arc::new(AtomicU32::new(self.light_fps.load(Ordering::Relaxed)));
        self.anim.insert(
            unit.to_string(),
            AnimStream {
                stop: stop.clone(),
                fps: fps_src.clone(),
            },
        );
        let unit = unit.to_string();
        std::thread::spawn(move || {
            let outcome: Result<(), String> = (|| {
                let reg = Registry::load().map_err(|e| format!("registry: {e}"))?;
                let infos = transport::enumerate().map_err(|e| format!("enumerate: {e}"))?;
                for i in &infos {
                    if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
                        if def.matches_control(i.usage_page, i.usage, i.feature_len)
                            && (pid == 0 || i.pid == pid)
                            && (unit.is_empty() || i.instance() == unit)
                        {
                            let d = Device::open_path(def.clone(), i.pid, &i.path)
                                .map_err(|e| format!("open failed: {e}"))?;
                            let ldef = d
                                .def
                                .lighting
                                .clone()
                                .ok_or_else(|| "device has no lighting".to_string())?;
                            let mut comp = neuron::pattern::Compositor::from_defs(&defs);
                            let lights = Lights::new(&d, ldef);
                            lights
                                // LIVE fps: read the shared atomic each frame so the GUI's fps
                                // control re-paces this running composite without a restart.
                                .animate(&mut comp, || fps_src.load(Ordering::Relaxed), 86_400, || {
                                    stop.load(Ordering::SeqCst) || neuron::writes::writes_paused()
                                })
                                .map_err(|e| format!("animate: {e}"))?;
                            return Ok(());
                        }
                    }
                }
                Err("device not found".into())
            })();
            on_done(outcome.err(), stop);
        });
        "compositing".into()
    }

    /// Stop ONE board's lighting stream (the board the GUI is acting on). Others keep streaming —
    /// including an identical twin on the same pid (`unit` addresses the one physical board).
    pub fn stop_animation(&mut self, pid: u16, unit: &str) {
        // Host-owned base: drop it there (the board latches its last frame).
        if crate::host::active() {
            crate::host::clear_lighting(pid, unit);
        }
        if let Some(a) = self.anim.remove(unit) {
            a.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Stop EVERY live lighting stream (the writes-paused kill-switch / shutdown).
    pub fn stop_all_animation(&mut self) {
        for (_, a) in self.anim.drain() {
            a.stop.store(true, Ordering::SeqCst);
        }
        // Host-owned base layers are streams too — composited through the arbiter, not this map —
        // so the global stop must drop them as well, or a host-managed board would sail through
        // the kill-switch/Observe transition with its last frame latched while every local stream
        // was cut. Mirrors `stop_animation`'s per-board `clear_lighting`; no-op when host inactive.
        crate::host::clear_all_lighting();
    }

    /// Is this board's lighting stream currently live? (Host base counts — the app
    /// is compositing that board even though it doesn't own the writer.)
    pub fn animating(&self, pid: u16, unit: &str) -> bool {
        self.anim.contains_key(unit) || crate::host::has_lighting(pid, unit)
    }

    /// Is ANY board's lighting stream live? Local anim threads OR host-composited base layers —
    /// the same both-sources truth `animating(pid)` reports per board, so the writes-paused/arm
    /// gates that key off this don't skip their stop/reset when the only live lighting is host-owned.
    pub fn any_animating(&self) -> bool {
        !self.anim.is_empty() || crate::host::any_lighting()
    }

    /// Is `token` still this unit's CURRENT stop flag (not superseded by a newer start)? A worker's
    /// completion callback uses this to ignore a STALE end (its stream was already replaced/stopped).
    pub fn anim_is_current(&self, unit: &str, token: &Arc<AtomicBool>) -> bool {
        self.anim
            .get(unit)
            .is_some_and(|a| Arc::ptr_eq(&a.stop, token))
    }

    /// Drop this unit's stream entry IF `token` is still the current one — a worker that ended ON ITS
    /// OWN (error / time cap) cleaning up after itself, without clobbering a stream that replaced it.
    pub fn anim_clear(&mut self, unit: &str, token: &Arc<AtomicBool>) {
        if self
            .anim
            .get(unit)
            .is_some_and(|a| Arc::ptr_eq(&a.stop, token))
        {
            self.anim.remove(unit);
        }
    }

    /// Re-pace this board's live stream (the fps slider) without restarting it. No-op if it isn't
    /// streaming. The stream reads its own `fps` copy live each frame, so this just stores the new
    /// value — every stream is a paced layer compositor now (the old paint-on-demand vitals surface
    /// is gone).
    pub fn set_anim_fps(&self, pid: u16, unit: &str, fps: u32) {
        if crate::host::active() {
            crate::host::set_fps(pid, unit, fps);
        }
        if let Some(a) = self.anim.get(unit) {
            a.fps.store(fps, Ordering::Relaxed);
        }
    }

    /// Feed the core lighting VITALS provider so the `vitals` PATTERN (the GUI preview + the streamed
    /// layer) has fresh battery / charge / DPI-stage — the unified replacement for the old `start_vitals`
    /// paint loop (gone; vitals is now just a compositor layer streamed through `start_layers`). Call it
    /// on the ~1s heartbeat while a vitals surface is live. Cheap + gated: the device read runs
    /// OFF-THREAD and only when the battery-aware throttle (`neuron::vitals::due`) permits, so a sleeping
    /// mouse is woken no more than the battery cards already wake it (enumeration, which picks the source,
    /// wakes nothing). `forced` bypasses the throttle once, for a prompt first read on activation. Only a
    /// single read runs at a time — the 60ms heartbeat can't herd the mouse.
    pub fn pump_vitals(&self, forced: bool) {
        static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
        if IN_FLIGHT.swap(true, Ordering::AcqRel) {
            return; // a read is already running — don't stack another.
        }
        std::thread::spawn(move || {
            // contain a fault so a read error can never strand the in-flight latch.
            let _ = std::panic::catch_unwind(|| publish_source_vitals(forced));
            IN_FLIGHT.store(false, Ordering::Release);
        });
    }

    // ── spine: rules from the loaded config ──────────────────────────────

    /// Build the unified read-only rule view from the same core assembler the live worker uses.
    /// GUI-authored rules are excluded here so `glue::refresh_rules` can append them as removable
    /// rows with stable edit indexes.
    pub fn rules(&self) -> (Vec<RuleView>, Vec<RuleView>) {
        let mut base = Vec::new();
        let mut hyper = Vec::new();

        for r in self.spine_rules() {
            let view = rule_view(&r);
            if r.layer.is_some() {
                hyper.push(view);
            } else {
                base.push(view);
            }
        }

        (base, hyper)
    }

    /// Spine `Rule`s assembled from the same sources as live dispatch, excluding GUI-authored rows
    /// that the editor appends separately as removable UI entries.
    pub fn spine_rules(&self) -> Vec<Rule> {
        let sidecars = neuron::controls::load_rule_sidecars_except("gui.rules.toml");
        neuron::controls::build_runtime_from(&self.bindings, &self.cast, &self.app_rules, &sidecars)
            .engine
            .to_rules()
    }

    // ── profiles ─────────────────────────────────────────────────────────

    pub fn reload_profiles(&mut self) {
        self.profiles = neuron::profile::list()
            .into_iter()
            .filter_map(|n| Profile::load(&n).ok())
            .collect();
    }

    /// Save the FULL current device state into a named profile — a real read-back of every
    /// capability the hardware exposes, not just the three slider values. Reads (best-effort): the
    /// active DPI + the full DPI STAGE LIST (the cycle), polling, brightness, idle-off timeout, and
    /// the current lighting effect from a matrix device. Falls back to the passed slider values when
    /// a device is absent/asleep so a headless save still captures the UI's intent. Also folds in the
    /// host-side gaming-mode toggles (no device read needed).
    pub fn save_profile_from_devices(
        &mut self,
        name: &str,
        dpi: u16,
        hz: u32,
        brightness: u8,
        lighting: Vec<neuron::pattern::LayerDef>,
    ) -> String {
        if name.trim().is_empty() {
            return "name required".into();
        }
        // Full device read-back now lives in core (shared with the CLI): active DPI + the full stage
        // list, polling, brightness, idle-off, and the current matrix effect — plus the host-side
        // gaming-mode policy. An absent/asleep device simply leaves those fields None.
        // Park every bridged writer for the fleet-wide read (core walks devices itself, so the
        // per-open gate can't reach in — same reply-clobber race as the row sweep).
        let _gates = crate::host::io_gate_all();
        let mut p = neuron::profile::capture_from_devices(
            &self.registry,
            name,
            self.gaming_mode,
            self.persist,
            // respect the device the user picked in the UI — pid AND physical unit, so a rig
            // with two identical devices captures the exact board being edited, not its twin.
            self.selected_pid,
            &self.selected_unit,
        );

        // The UI slider values are fallbacks only — applied where the device did not answer.
        p.dpi = p.dpi.or(Some(dpi));
        p.polling_hz = p.polling_hz.or(Some(hz));
        p.brightness = p.brightness.or(Some(brightness));

        // capture_from_devices can't synthesize a LayerDef stack from raw effect registers, so the
        // caller hands us the LIVE compositor stack — that's what gets saved as this profile's lighting.
        p.lighting = lighting;

        match p.save() {
            Ok(_) => {
                self.reload_profiles();
                format!("captured profile '{name}': {}", p.summary())
            }
            Err(e) => format!("save failed: {e}"),
        }
    }

    pub fn delete_profile(&mut self, name: &str) -> String {
        let path = Profile::path(name);
        let r = std::fs::remove_file(path);
        // Lighting lives IN the profile TOML now (the layer stack) — there's no frame sidecar to
        // reap, so removing the one file is a full delete.
        self.reload_profiles();
        match r {
            Ok(_) => {
                // a deleted profile can't stay "active" — the header pill must drop to none.
                if self.active_profile == name {
                    self.active_profile = "—".into();
                    neuron::profile::set_active("");
                }
                // dangling app rules would fail forever at focus-switch time; say so now.
                let refs = self
                    .app_rules
                    .rules
                    .iter()
                    .filter(|r| r.profile == name)
                    .count();
                if refs > 0 {
                    format!("deleted '{name}' — {refs} app rule(s) still reference it")
                } else {
                    format!("deleted '{name}'")
                }
            }
            Err(e) => format!("delete failed: {e}"),
        }
    }

    pub fn add_app_rule(&mut self, app: &str, profile: &str) -> String {
        let app = app.trim();
        let profile = profile.trim();
        if app.is_empty() || profile.is_empty() {
            return "app + profile required".into();
        }
        // the rule is only as real as its target: a typo'd profile would fail silently forever.
        if !self.profiles.iter().any(|p| p.name == profile) {
            return format!("no profile '{profile}' — save it first");
        }
        // duplicates are dead weight (first match wins), so refuse them with the reason.
        if self
            .app_rules
            .rules
            .iter()
            .any(|r| r.app.eq_ignore_ascii_case(app) && r.profile == profile)
        {
            return format!("rule {app} -> {profile} already exists");
        }
        self.app_rules.rules.push(AppRule {
            app: app.into(),
            profile: profile.into(),
        });
        // the rule is live the moment it's pushed; but a failed disk write must SAY so, not report a
        // clean success the user would trust across a restart (where the unsaved rule is gone).
        match self.save_app_rules() {
            Ok(()) => format!("rule {app} -> {profile}"),
            Err(e) => format!("rule {app} -> {profile} — added live but not saved: {e}"),
        }
    }

    pub fn remove_app_rule(&mut self, idx: usize) {
        if idx < self.app_rules.rules.len() {
            self.app_rules.rules.remove(idx);
            // no status channel on the remove path (the caller shows nothing) — the rule is gone live;
            // a persist failure is inert here, so it stays swallowed rather than fabricating a report.
            let _ = self.save_app_rules();
        }
    }

    fn save_app_rules(&self) -> Result<(), String> {
        let s = toml::to_string_pretty(&self.app_rules).map_err(|e| e.to_string())?;
        std::fs::write(AppRules::path(), s).map_err(|e| e.to_string())
    }

    // ── backup ───────────────────────────────────────────────────────────

    /// Snapshot ONE physical unit's getter space, addressed by its `path_instance` (the row's
    /// unit id) — so backing up one of two identical devices snapshots the one you clicked.
    pub fn backup(&self, unit: &str) -> String {
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(e) => return format!("enumerate failed: {e}"),
        };
        for i in &infos {
            if i.instance() != unit {
                continue;
            }
            if let Some(def) = self.registry.find_by_pid(i.vid, i.pid).cloned() {
                if !def.matches_control(i.usage_page, i.usage, i.feature_len) {
                    continue; // wrong collection of the right unit — keep looking
                }
                // the backup sweep reads the ENTIRE getter space — park the
                // host writer or streaming frames clobber half the replies.
                let _gate = crate::host::io_gate(i.pid);
                return snapshot_device(&def, i.pid, &i.path);
            }
        }
        "device not found".into()
    }

    // ── diagnostics: the in-app test harness ("prove it works to me") ─────

    /// Fire each capability live and report a verdict per probe. Read-only / dry-run only — NOTHING
    /// here writes to a device or fires a real OS action, so it's always safe to run (true to the
    /// gate). This is the user's transparency surface AND the manual test harness.
    pub fn run_diagnostics(&mut self) -> Vec<DiagProbe> {
        let mut out = Vec::new();

        // 1) transport — can we even enumerate HID?
        match transport::enumerate() {
            Ok(infos) => out.push(DiagProbe::pass(
                "transport enumerate",
                format!("{} HID interface(s) visible", infos.len()),
            )),
            Err(e) => out.push(DiagProbe::fail("transport enumerate", e.to_string())),
        }

        // 2) registry — devices recognized.
        out.push(DiagProbe::pass(
            "device registry",
            format!("{} device profile(s) loaded", self.registry.devices.len()),
        ));

        // 3) device round-trip — open the selected device and read a getter back.
        match self.open_selected() {
            Ok(d) => match cap::firmware(&d) {
                Ok(fw) => out.push(DiagProbe::pass(
                    "device round-trip",
                    format!("read firmware v{fw} from {}", d.def.name),
                )),
                Err(_) => {
                    // firmware can be unread on an asleep wireless mouse — try DPI as a second read.
                    match cap::dpi(&d) {
                        Ok((x, _)) => out.push(DiagProbe::pass(
                            "device round-trip",
                            format!("read DPI {x} from {}", d.def.name),
                        )),
                        Err(e) => out.push(DiagProbe::skip(
                            "device round-trip",
                            format!("device present but asleep ({e})"),
                        )),
                    }
                }
            },
            Err(e) => out.push(DiagProbe::skip("device round-trip", e.to_string())),
        }

        // 4) lighting test pattern — build a real test frame WITHOUT writing it (proves the
        //    render/canvas path end-to-end; sending is gated/optional).
        match self.selected_def().and_then(|d| d.lighting.clone()) {
            Some(def) => {
                let mut canvas = neuron::lighting::Canvas::new(def.rows, def.cols);
                // a diagonal accent sweep — deterministic, easy to eyeball if pushed.
                for r in 0..def.rows as usize {
                    for c in 0..def.cols as usize {
                        if (r + c) % 3 == 0 {
                            canvas.px[r * def.cols as usize + c] = Rgb::new(0x4a, 0xf2, 0xb0);
                        }
                    }
                }
                let lit = canvas
                    .px
                    .iter()
                    .filter(|p| p.r != 0 || p.g != 0 || p.b != 0)
                    .count();
                out.push(DiagProbe::pass(
                    "lighting test pattern",
                    format!(
                        "built {}×{} frame, {lit} LEDs lit (not pushed)",
                        def.rows, def.cols
                    ),
                ));
            }
            None => out.push(DiagProbe::skip(
                "lighting test pattern",
                "selected device has no lighting".into(),
            )),
        }

        // 5) effect availability — can we resolve the device's effect superset?
        let effs = self.effects();
        if effs.is_empty() {
            out.push(DiagProbe::skip(
                "effect engine",
                "no lighting device selected".into(),
            ));
        } else {
            let native = effs.iter().filter(|e| e.1).count();
            out.push(DiagProbe::pass(
                "effect engine",
                format!(
                    "{} effects ({native} native, {} emulated)",
                    effs.len(),
                    effs.len() - native
                ),
            ));
        }

        // 6) spine assembly — the Trigger->Action engine builds from the on-disk config.
        let n = neuron::engine::Engine::new(self.spine_rules()).rules.len();
        let total = self.bindings.bindings.len() + n;
        out.push(DiagProbe::pass(
            "spine engine",
            format!(
                "{total} rule(s) assembled ({n} cast, {} binding)",
                self.bindings.bindings.len()
            ),
        ));

        // 7) macro runtime — the Python Macro Host's interpreter + host scripts resolve. Does NOT
        //    SPAWN the sidecar (that warms lazily on the first real macro); it DOES extract the
        //    interpreter on the very first call, but startup warms it on a background thread, so by the
        //    time diagnostics run it's resolved and cheap. "skip" honestly when no python is available.
        if neuron::macros::macro_host().available() {
            out.push(DiagProbe::pass(
                "macro runtime",
                "python runtime + host scripts resolved (sidecar warms on demand)".into(),
            ));
        } else {
            out.push(DiagProbe::skip(
                "macro runtime",
                "no python runtime resolved (bundle runtime/python or install python)".into(),
            ));
        }

        // 8) gesture vault — the eigenmotion store loads.
        out.push(DiagProbe::pass(
            "gesture vault",
            format!("{} glyph template(s) loaded", self.vault.templates.len()),
        ));

        // 9) audio (mic) — the Core-Audio capture endpoint resolves (read-only).
        match neuron::audio::resolve_capture(None) {
            Some(e) => out.push(DiagProbe::pass(
                "mic endpoint",
                format!("resolved: {}", e.name),
            )),
            None => out.push(DiagProbe::skip("mic endpoint", "no capture device".into())),
        }

        out
    }
}

/// One diagnostics probe result, mapped 1:1 to the Slint `DiagRow`.
pub struct DiagProbe {
    pub name: &'static str,
    pub detail: String,
    pub state: &'static str, // "pass" | "fail" | "skip"
}

impl DiagProbe {
    fn pass(name: &'static str, detail: String) -> Self {
        DiagProbe {
            name,
            detail,
            state: "pass",
        }
    }
    fn fail(name: &'static str, detail: String) -> Self {
        DiagProbe {
            name,
            detail,
            state: "fail",
        }
    }
    fn skip(name: &'static str, detail: String) -> Self {
        DiagProbe {
            name,
            detail,
            state: "skip",
        }
    }
    fn pending(name: &'static str) -> Self {
        DiagProbe {
            name,
            detail: "not yet run".into(),
            state: "pending",
        }
    }
}

/// The fixed roster of diagnostic STATIONS, in bench order (a stable 3×3 grid). [`AppRuntime::run_diagnostics`]
/// emits a verdict for each in this order; the Settings bench seeds them PENDING at startup so the
/// "prove it works" panel always shows its stations — lit live on a run — instead of a blank void.
pub const DIAGNOSTIC_STATIONS: [&str; 9] = [
    "transport enumerate",
    "device registry",
    "device round-trip",
    "lighting test pattern",
    "effect engine",
    "spine engine",
    "macro runtime",
    "gesture vault",
    "mic endpoint",
];

/// The bench's PENDING seed — every station awaiting its first run (state "pending", not a verdict).
pub fn pending_diagnostic_stations() -> Vec<DiagProbe> {
    DIAGNOSTIC_STATIONS
        .iter()
        .map(|n| DiagProbe::pending(n))
        .collect()
}

/// Find the source device (the first battery-capable mouse) and PUBLISH its live vitals to the core
/// lighting provider for the `vitals` pattern to visualise — the GUI's equivalent of the CLI's
/// `read_mouse_vitals` read, but feeding [`neuron::lighting::publish_vitals`] instead of painting. The
/// wake-costing OPEN+READ is gated by the battery-aware throttle (`neuron::vitals::due`) so a sleeping
/// mouse is woken no more than the battery cards do; enumeration (used to pick the source) wakes
/// nothing. On a good battery read it also feeds `vitals::observe`, so the battery CARDS ride the same
/// sample; a failed read marks the throttle stale (a quick retry) and leaves the last snapshot warm
/// (the provider never flickers to dark once fed). `forced` bypasses the throttle once, for a prompt
/// first paint on activation. Each sub-read is best-effort — an asleep mouse falls back to stage 0 /
/// last-known charge rather than aborting.
fn publish_source_vitals(forced: bool) {
    let Ok(reg) = Registry::load() else { return };
    let Ok(infos) = transport::enumerate() else { return };
    // Candidate control interfaces, then the LOWEST instance wins — enumeration order is not
    // stable, and with two identical battery mice an order-dependent pick would alternate which
    // unit feeds the (pid-keyed) vitals series between calls, fabricating battery edges.
    let source = infos
        .iter()
        .filter(|i| {
            reg.find_by_pid(i.vid, i.pid).is_some_and(|def| {
                def.commands.contains_key("battery_level")
                    && def.matches_control(i.usage_page, i.usage, i.feature_len)
            })
        })
        .min_by_key(|i| i.instance());
    {
        let Some(i) = source else { return };
        let Some(def) = reg.find_by_pid(i.vid, i.pid) else { return };
        // gate the wake-costing OPEN+READ behind the shared throttle; the enumeration above was free.
        if !neuron::vitals::due(i.pid, forced) {
            return;
        }
        // park the host writer for the read (same reply-clobber race as the row sweep)
        let _gate = crate::host::io_gate(i.pid);
        let Ok(d) = Device::open_path(def.clone(), i.pid, &i.path) else {
            neuron::vitals::mark_stale(i.pid); // couldn't open — retry soon, don't hold the window.
            return;
        };
        match cap::battery_percent(&d) {
            Ok(battery_pct) => {
                // charge falls back to the last-known state on a read blip (never a phantom unplugged).
                let charging = cap::charging(&d)
                    .ok()
                    .or_else(|| neuron::vitals::last_charging(i.pid))
                    .unwrap_or(false);
                // active DPI stage from the `dpi_stages_active` (or `dpi_stages`) reply: `s[1]` = active
                // index, `s[2]` = count; length-guarded so a short reply falls back to stage 0.
                let (active_stage, stage_count) = d
                    .run("dpi_stages_active")
                    .or_else(|_| d.run("dpi_stages"))
                    .ok()
                    .and_then(|s| Some((*s.get(1)?, *s.get(2)?)))
                    .unwrap_or((0, 0));
                neuron::lighting::publish_vitals(neuron::lighting::Vitals {
                    battery_pct,
                    charging,
                    active_stage,
                    stage_count,
                });
                // share the read: keep the battery CARDS fresh off the same sample (passive scan).
                neuron::vitals::observe(i.pid, battery_pct, charging, false);
            }
            Err(_) => neuron::vitals::mark_stale(i.pid), // asleep/blip — retry soon, keep last warm.
        }
    }
}

/// A flat rule row for the view (kept here so the glue maps it 1:1 to the Slint struct).
pub struct RuleView {
    pub trigger: String,
    pub action: String,
    pub layer: String,
    pub kind: &'static str,
}

fn rule_view(r: &Rule) -> RuleView {
    RuleView {
        trigger: r.trigger.describe(),
        action: r.action.describe(),
        layer: r.layer.clone().unwrap_or_else(|| "base".into()),
        kind: trigger_kind(&r.trigger),
    }
}

fn trigger_kind(t: &Trigger) -> &'static str {
    match t {
        Trigger::Input { .. } => "input",
        Trigger::Gesture { .. } => "gesture",
        Trigger::RadialSector { .. } => "radial",
        Trigger::AppFocus { .. } => "app",
        Trigger::MicTap => "mic",
        Trigger::Hold { .. } => "hold",
        Trigger::Cast { .. } => "cast",
    }
}

/// A transiently-opened device plus the host-writer gate that keeps its
/// feature-report channel exclusive while the handle lives (the writer parks;
/// frames resume when this drops). Derefs to [`Device`] so the whole
/// getter/setter surface uses it unchanged — the gate rides along invisibly.
pub struct GatedDevice {
    _gate: Option<crate::host::IoGate>,
    dev: Device,
}

impl std::ops::Deref for GatedDevice {
    type Target = Device;
    fn deref(&self) -> &Device {
        &self.dev
    }
}

/// Read one device's live state (best-effort). Each getter is independent so a partial/asleep
/// device still yields a row with what it could read. Opens the exact control `path` the caller
/// enumerated — never "some interface with this pid" — so each row of a duplicate pair reads its
/// own hardware. `feed_vitals` routes the shared battery sample into the pid-keyed edge-detector;
/// the caller enables it for ONE unit per pid (see `scan_devices`).
fn read_device_state(
    def: &DeviceDef,
    pid: u16,
    path: &transport::DevicePath,
    feed_vitals: bool,
) -> DeviceState {
    let mode = def
        .mode_for(pid)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "?".into());
    let icon = icon_for(def);
    let mut st = DeviceState {
        name: def.name.clone(),
        codename: def.codename.clone(),
        pid,
        instance: String::new(), // the caller stamps the unit id it resolved
        mode,
        connected: false,
        firmware: "—".into(),
        dpi: "—".into(),
        polling: "—".into(),
        brightness: "—".into(),
        battery: String::new(),
        charging: false,
        storage: String::new(),
        icon,
        dpi_n: None,
        polling_n: None,
        brightness_n: None,
        battery_frac: None,
        // capabilities are a pure read over the registry descriptor — no hardware probe.
        cap_dpi: def.supports(neuron::registry::Capability::SetDpi),
        cap_poll: def.supports(neuron::registry::Capability::SetPolling),
        cap_light: def.supports(neuron::registry::Capability::Lighting),
        cap_bright: def.supports(neuron::registry::Capability::Brightness),
        cap_scroll: def.supports(neuron::registry::Capability::SetScrollStage),
        cap_store: def.supports(neuron::registry::Capability::Storage),
        cap_idle: def.supports(neuron::registry::Capability::Battery),
        cap_plate: def.has_side_plates(),
    };
    // Park this device's host lighting writer for the whole getter sweep —
    // without the gate, streaming frames clobber every getter's pending reply
    // and the row reads all-"—" whenever lighting is live (seen on-desk).
    let _gate = crate::host::io_gate(pid);
    if let Ok(d) = Device::open_path(def.clone(), pid, path) {
        // any successful read marks the device reachable
        if let Ok(fw) = cap::firmware(&d) {
            st.firmware = format!("v{fw}");
            st.connected = true;
        }
        if let Ok((x, _y)) = cap::dpi(&d) {
            st.dpi = format!("{x}");
            st.dpi_n = Some(x);
            st.connected = true;
        }
        if let Ok(hz) = cap::polling_rate_hz(&d) {
            st.polling = format!("{hz} Hz");
            st.polling_n = Some(hz);
        }
        if let Ok(br) = cap::brightness_percent(&d) {
            st.brightness = format!("{br}%");
            st.brightness_n = Some(br);
        }
        if let Ok(b) = cap::battery_percent(&d) {
            // charge falls back to the LAST-KNOWN state on a read blip (never a phantom "unplugged"),
            // so a battery edge is never lost to a charge-read failure, yet no spurious charge card.
            let charging = cap::charging(&d)
                .ok()
                .or_else(|| neuron::vitals::last_charging(pid))
                .unwrap_or(false);
            st.charging = charging;
            st.battery = format!("{b}%");
            st.battery_frac = Some((b as f32 / 100.0).clamp(0.0, 1.0));
            // passive scan (from_event = false): feed the edge-detector off this read we already
            // did — but only from the one designated unit per pid (the detector is pid-keyed;
            // two twins feeding it would interleave two batteries and fabricate edges).
            if feed_vitals {
                neuron::vitals::observe(pid, b, charging, false);
            }
        }
        if let Ok(s) = cap::storage(&d) {
            st.storage = format!("{}% free", s.pct_remaining());
        }
    }
    st
}

/// Human label for a symmetric lift-off-distance level (0 low / 1 medium / 2 high).
fn lod_label(level: u8) -> &'static str {
    match level.min(2) {
        0 => "low",
        1 => "medium",
        _ => "high",
    }
}

/// Format a DPI stage list for a status line ("800/1600/3200").
fn fmt_stages(stages: &[neuron::writes::DpiStage]) -> String {
    stages
        .iter()
        .map(|s| s.x.to_string())
        .collect::<Vec<_>>()
        .join("/")
}

fn icon_for(def: &DeviceDef) -> &'static str {
    let n = def.name.to_lowercase();
    if n.contains("naga") || n.contains("mouse") || n.contains("deathadder") || n.contains("viper")
    {
        "mouse"
    } else if n.contains("blackwidow") || n.contains("keyboard") || n.contains("huntsman") {
        "keyboard"
    } else {
        "device"
    }
}

/// Snapshot a device's full getter space to a timestamped backup JSON (read-only safety move).
/// Opens the exact enumerated control path — the caller resolved the physical unit, so a
/// re-enumeration here could not swap in an identical twin.
fn snapshot_device(def: &DeviceDef, pid: u16, path: &transport::DevicePath) -> String {
    use neuron::backup::{GetterSnap, IfaceSnap, Snapshot};
    use neuron::discover;
    let Ok(d) = Device::open_path(def.clone(), pid, path) else {
        return "device unreachable".into();
    };
    let mut getters = Vec::new();
    // sweep the standard getter space (read-only; ids >= 0x80)
    for class in 0x00u8..=0x0F {
        for id in 0x80u8..=0x8F {
            if let Ok(a) = d.exec_dynamic(class, id, 0x20, &[]) {
                let kind = discover::classify(&a);
                if kind != "empty" {
                    let raw: String = a.iter().map(|b| format!("{b:02x}")).collect();
                    getters.push(GetterSnap {
                        class,
                        id,
                        kind: kind.into(),
                        raw,
                    });
                }
            }
        }
    }
    let ci = &def.control_interface;
    let snap = Snapshot {
        vid: def.vendor_id,
        pid,
        name: def.name.clone(),
        unix_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        interfaces: vec![IfaceSnap {
            usage_page: ci.usage_page,
            usage: ci.usage,
            getters,
        }],
    };
    let _ = std::fs::create_dir_all("backups");
    let path = std::path::Path::new("backups").join(snap.filename());
    match std::fs::write(&path, snap.to_json()) {
        Ok(_) => format!(
            "backed up {} getters -> {}",
            snap.getter_count(),
            path.display()
        ),
        Err(e) => format!("backup write failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime loads from disk (registry + config) without a device present.
    #[test]
    fn runtime_loads_headless() {
        let rt = AppRuntime::load();
        // a fresh runtime has no active profile; write authority lives in neuron-core::safety.
        assert_eq!(rt.active_profile, "—");
        // the registry should carry the builtin devices (Naga + BlackWidow at minimum).
        assert!(
            !rt.registry.devices.is_empty(),
            "registry should load builtin devices"
        );
    }

    /// The diagnostics harness ALWAYS returns a verdict per probe, never panics, and never claims a
    /// pass it can't back up. Device-bound probes degrade to "skip" when no hardware is present.
    #[test]
    fn diagnostics_yield_verdicts_without_hardware() {
        let mut rt = AppRuntime::load();
        let probes = rt.run_diagnostics();
        assert!(!probes.is_empty(), "diagnostics must report something");
        // every probe is one of the three known verdicts.
        for p in &probes {
            assert!(
                matches!(p.state, "pass" | "fail" | "skip"),
                "probe '{}' had unknown verdict '{}'",
                p.name,
                p.state
            );
            assert!(
                !p.detail.is_empty(),
                "probe '{}' must explain itself",
                p.name
            );
        }
        // the registry + spine + gesture-vault probes are hardware-independent and MUST pass.
        let must_pass = ["device registry", "spine engine", "gesture vault"];
        for name in must_pass {
            let probe = probes.iter().find(|p| p.name == name);
            assert!(probe.is_some(), "missing probe '{name}'");
            assert_eq!(
                probe.unwrap().state,
                "pass",
                "probe '{name}' should pass headless"
            );
        }
    }

    /// The macro-runtime probe yields an honest verdict (pass when python+scripts resolve, skip
    /// otherwise) and never spawns a sidecar just to report.
    #[test]
    fn macro_runtime_probe_yields_a_verdict() {
        let mut rt = AppRuntime::load();
        let probes = rt.run_diagnostics();
        let p = probes
            .iter()
            .find(|p| p.name == "macro runtime")
            .expect("macro runtime probe present");
        assert!(
            matches!(p.state, "pass" | "skip"),
            "unexpected state '{}'",
            p.state
        );
        assert!(!p.detail.is_empty());
    }

    /// The spine `RuleView` assembly maps every config source into a tagged row (base layer).
    #[test]
    fn rules_view_assembles_from_config() {
        let rt = AppRuntime::load();
        let (base, hyper) = rt.rules();
        // hyper layer is currently always empty (split not yet encoded) — that's the honest state.
        assert!(hyper.is_empty());
        // every base row carries a non-empty trigger + a known kind.
        for r in &base {
            assert!(!r.trigger.is_empty());
            assert_eq!(r.layer, "base");
            assert!(matches!(
                r.kind,
                "input" | "gesture" | "radial" | "app" | "mic" | "hold" | "cast"
            ));
        }
    }

    /// Profile save -> reload round-trips through the runtime (uses a temp-named profile, cleaned up).
    #[test]
    fn profile_save_and_delete_roundtrip() {
        // `Profile::path` is cwd-relative (`profiles/<name>.toml`), and the editor/prefs/apptest
        // tests swap the process-global cwd. Take the shared cwd lock so this save+reload is
        // isolated in its own temp dir and can't race a cwd swap out from under it.
        let _cwd = crate::testsupport::cwd_guard("runtime_profile");
        let mut rt = AppRuntime::load();
        let name = format!("__neuron_test_{}", std::process::id());
        let msg = rt.save_profile_from_devices(&name, 1234, 500, 60, vec![]);
        // the message reports the capture either way ("captured" on the full-state path, "saved"
        // historically).
        assert!(
            msg.contains("captured") || msg.contains("saved"),
            "unexpected: {msg}"
        );
        // It shows up in the reloaded list with every core field POPULATED. The values themselves
        // are deliberately not pinned: capture is a REAL device read-back first, slider fallback
        // second — on a dev machine with the mouse awake this captures the hardware's live DPI,
        // headless it captures the slider values. Pinning the fallback numbers made this test
        // flake with the hardware's sleep state (seen live: awake Naga answered DPI 800).
        let p = rt
            .profiles
            .iter()
            .find(|p| p.name == name)
            .expect("saved profile present");
        assert!(p.dpi.is_some(), "dpi captured (device read or fallback)");
        assert!(p.polling_hz.is_some(), "polling captured");
        assert!(p.brightness.is_some(), "brightness captured");
        // delete it and confirm it's gone.
        let del = rt.delete_profile(&name);
        assert!(del.contains("deleted"), "unexpected: {del}");
        assert!(!rt.profiles.iter().any(|p| p.name == name));
    }

    /// An empty profile name is rejected (no silent empty-named file).
    #[test]
    fn empty_profile_name_rejected() {
        let mut rt = AppRuntime::load();
        let msg = rt.save_profile_from_devices("  ", 800, 1000, 50, vec![]);
        assert!(msg.contains("name required"));
    }

    /// Grid dims are honest: with no lit device selected the sentinel is (0,0) — the editor shows
    /// its empty state instead of a fake matrix. With a device, both dims are >= 1.
    #[test]
    fn grid_dims_honest() {
        let rt = AppRuntime::load();
        let (rows, cols) = rt.grid_dims();
        // either a real matrix or the honest no-device sentinel; never a half-empty axis.
        assert!((rows >= 1 && cols >= 1) || (rows == 0 && cols == 0));
    }

    /// Writes-paused blocks every device setter without touching hardware.
    #[test]
    fn paused_writes_block_setters() {
        let rt = AppRuntime::load();
        let saved = neuron::writes::writes_paused();
        neuron::writes::set_writes_paused(true);
        assert_eq!(rt.apply_dpi(1600), "writes paused");
        assert_eq!(rt.apply_polling(1000).0, "writes paused");
        assert_eq!(rt.apply_brightness(50), "writes paused");
        assert_eq!(
            rt.apply_effect("static", Rgb::new(1, 2, 3)),
            "writes paused"
        );
        neuron::writes::set_writes_paused(saved);
    }

    /// DPI stage parsing refuses garbage with the offenders NAMED (never a silent partial apply),
    /// and the active index is clamped into range before any write.
    #[test]
    fn dpi_stage_parse_accounts_for_garbage() {
        let rt = AppRuntime::load(); // writes not paused, but the parse rejects before any device IO
        let msg = rt.apply_dpi_stages("800/abc/99999/1600", 0);
        assert!(msg.contains("invalid stage(s)"), "unexpected: {msg}");
        assert!(msg.contains("abc"), "must name the bad token: {msg}");
        assert!(
            msg.contains("99999"),
            "must name the out-of-range token: {msg}"
        );
        let empty = rt.apply_dpi_stages("  ", 0);
        assert!(empty.contains("no DPI stages"), "unexpected: {empty}");
    }

    /// HyperScroll mode parsing refuses unknown tokens before any gated device write, so the UI and
    /// backend share one no-silent-partial rule.
    #[test]
    fn scroll_stage_parse_accounts_for_garbage() {
        let rt = AppRuntime::load();
        let msg = rt.apply_scroll_stages("tactile/weird/free");
        assert!(msg.contains("invalid scroll mode"), "unexpected: {msg}");
        assert!(msg.contains("weird"), "must name the bad token: {msg}");
        let empty = rt.apply_scroll_stages("  ");
        assert!(empty.contains("no scroll modes"), "unexpected: {empty}");
    }

    /// Deleting the active profile clears the active marker (the header pill must drop to none)
    /// and reports app rules that still reference the deleted profile.
    #[test]
    fn delete_active_profile_clears_marker() {
        let _cwd = crate::testsupport::cwd_guard("runtime_delete_active");
        let mut rt = AppRuntime::load();
        let name = format!("__neuron_del_{}", std::process::id());
        rt.save_profile_from_devices(&name, 800, 1000, 50, vec![]);
        rt.active_profile = name.clone();
        rt.app_rules.rules.push(AppRule {
            app: "game".into(),
            profile: name.clone(),
        });
        let msg = rt.delete_profile(&name);
        assert!(msg.contains("deleted"), "unexpected: {msg}");
        assert!(
            msg.contains("1 app rule"),
            "must warn about dangling rules: {msg}"
        );
        assert_eq!(rt.active_profile, "—", "active marker must clear");
    }

    /// App rules validate their target profile exists and refuse duplicates.
    #[test]
    fn app_rule_validates_profile_and_dupes() {
        let _cwd = crate::testsupport::cwd_guard("runtime_app_rule");
        let mut rt = AppRuntime::load();
        let missing = rt.add_app_rule("game", "__no_such_profile__");
        assert!(missing.contains("no profile"), "unexpected: {missing}");
        assert!(rt.app_rules.rules.is_empty());
        let name = format!("__neuron_rule_{}", std::process::id());
        rt.save_profile_from_devices(&name, 800, 1000, 50, vec![]);
        let ok = rt.add_app_rule("game", &name);
        assert!(ok.contains("rule game ->"), "unexpected: {ok}");
        let dup = rt.add_app_rule("GAME", &name);
        assert!(
            dup.contains("already exists"),
            "case-insensitive dupe: {dup}"
        );
        assert_eq!(rt.app_rules.rules.len(), 1);
        rt.delete_profile(&name);
    }
}
