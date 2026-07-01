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
    pub cap_scroll: bool, // SetScrollStage — scroll-wheel stages
    pub cap_store: bool, // Storage — persist-to-onboard
    pub cap_idle: bool, // Battery (wireless proxy) — the idle-off timer
    pub cap_plate: bool, // has a [side_plates] map — surfaces the push-detected side-plate readout
}

/// One live lighting stream's controls, owned PER-DEVICE in [`AppRuntime::anim`]: the stop flag its
/// worker polls each frame, and the fps it reads (only the layer compositor uses fps; the vitals
/// surface paints on-demand and ignores it).
pub struct AnimStream {
    pub stop: Arc<AtomicBool>,
    pub fps: Arc<AtomicU32>,
    /// FPS-PACED (the layer compositor, reads `fps` live each frame) vs paint-on-demand (the vitals
    /// surface, which ignores `fps`). The fps slider only writes paced streams, so it can never silently
    /// land on a vitals stream's unused `fps` when vitals is the board's current entry in [`anim`].
    pub fps_paced: bool,
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
    /// Currently selected device pid (for per-device panels). 0 = none.
    pub selected_pid: u16,
    /// Live lighting streams, keyed by device pid. Each board gets its OWN stop flag + fps, so
    /// starting, stopping, or re-pacing one board's lighting NEVER touches another's — and switching
    /// which board you're editing leaves the others streaming. (This was a single global flag + bool,
    /// which made every apply/stop/device-switch tear down whatever one stream happened to be live.)
    pub anim: HashMap<u16, AnimStream>,
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
    pub fn scan_devices(&mut self) -> Vec<DeviceState> {
        let mut out = Vec::new();
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(_) => return out,
        };
        let mut seen = std::collections::BTreeSet::new();
        for i in &infos {
            let Some(def) = self.registry.find_by_pid(i.vid, i.pid) else {
                continue;
            };
            if !def.matches_control(i.usage_page, i.usage, i.feature_len) {
                continue;
            }
            if !seen.insert(i.pid) {
                continue;
            }
            let def = def.clone();
            out.push(read_device_state(&def, i.pid));
        }
        // Selection follows reality: if the selected pid is no longer enumerated (unplugged,
        // dongle gone) — or nothing was selected yet — adopt the first recognized device so the
        // per-device panels never target a ghost. pid 0 never matches a row, so this one branch
        // covers both first-scan auto-pick and stale-pid healing.
        if !out.iter().any(|d| d.pid == self.selected_pid) {
            self.selected_pid = out.first().map(|d| d.pid).unwrap_or(0);
        }
        out
    }

    /// Open the currently-selected device (or the first recognized one).
    pub fn open_selected(&self) -> anyhow::Result<Device> {
        let infos = transport::enumerate()?;
        for i in &infos {
            if let Some(def) = self.registry.find_by_pid(i.vid, i.pid) {
                if def.matches_control(i.usage_page, i.usage, i.feature_len)
                    && (self.selected_pid == 0 || i.pid == self.selected_pid)
                {
                    return Device::open_path(def.clone(), i.pid, &i.path);
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

    /// The DEFAULT streaming fps for the selected lit device + whether it's a slow LEGACY board:
    /// legacy boards default to 6 (they drop frames above ~6), matrix devices to 30. `None` when the
    /// selection has no lighting. Seeds the GUI fps control on selection — the user tunes from there.
    pub fn light_fps_default(&self) -> Option<(u32, bool)> {
        self.selected_def()
            .and_then(|d| d.lighting)
            .map(|l| match l.protocol {
                neuron::lighting::Protocol::Legacy => (6, true),
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

    /// Paint an explicit per-LED frame (row-major) to the device.
    pub fn push_frame(&self, frame: &[Rgb]) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => {
                let Some(def) = d.def.lighting.clone() else {
                    return "device has no lighting".into();
                };
                let (rows, cols) = (def.rows as usize, def.cols as usize);
                let mut canvas = neuron::lighting::Canvas::new(def.rows, def.cols);
                for (i, px) in frame.iter().enumerate().take(rows * cols) {
                    canvas.px[i] = *px;
                }
                let lights = Lights::new(&d, def);
                if let Err(e) = lights.ensure_control() {
                    return format!("control failed: {e}");
                }
                match lights.paint(&canvas) {
                    Ok(_) => "frame pushed".into(),
                    Err(e) => format!("paint failed: {e}"),
                }
            }
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Stream the LAYER COMPOSITOR live on a worker thread (re-opens its own device). The compositor is
    /// built from the whole layer stack (Pattern × Spectrum layers).
    /// The layered composite is inherently the custom-frame path (it streams blended frames).
    pub fn start_layers(
        &mut self,
        defs: Vec<neuron::pattern::LayerDef>,
        pid: u16,
        on_done: impl FnOnce(Option<String>, Arc<AtomicBool>) + Send + 'static,
    ) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        if defs.is_empty() {
            return "no layers".into();
        }
        // stop only THIS board's prior stream (if any) — other boards keep streaming.
        if let Some(a) = self.anim.get(&pid) {
            a.stop.store(true, Ordering::SeqCst);
        }
        let stop = Arc::new(AtomicBool::new(false));
        // this stream's OWN fps copy, seeded from the GUI's current value — so re-pacing or selecting
        // another board can never change THIS stream's speed.
        let fps_src = Arc::new(AtomicU32::new(self.light_fps.load(Ordering::Relaxed)));
        self.anim.insert(
            pid,
            AnimStream {
                stop: stop.clone(),
                fps: fps_src.clone(),
                fps_paced: true, // the layer compositor reads fps live each frame
            },
        );
        std::thread::spawn(move || {
            let outcome: Result<(), String> = (|| {
                let reg = Registry::load().map_err(|e| format!("registry: {e}"))?;
                let infos = transport::enumerate().map_err(|e| format!("enumerate: {e}"))?;
                for i in &infos {
                    if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
                        if def.matches_control(i.usage_page, i.usage, i.feature_len)
                            && (pid == 0 || i.pid == pid)
                        {
                            let d = Device::open(def.clone(), i.pid)
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

    /// Stop ONE board's lighting stream (the board the GUI is acting on). Others keep streaming.
    pub fn stop_animation(&mut self, pid: u16) {
        if let Some(a) = self.anim.remove(&pid) {
            a.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Stop EVERY live lighting stream (the writes-paused kill-switch / shutdown).
    pub fn stop_all_animation(&mut self) {
        for (_, a) in self.anim.drain() {
            a.stop.store(true, Ordering::SeqCst);
        }
    }

    /// Is `pid`'s lighting stream currently live?
    pub fn animating(&self, pid: u16) -> bool {
        self.anim.contains_key(&pid)
    }

    /// Is ANY board's lighting stream live?
    pub fn any_animating(&self) -> bool {
        !self.anim.is_empty()
    }

    /// Is `token` still `pid`'s CURRENT stop flag (not superseded by a newer start)? A worker's
    /// completion callback uses this to ignore a STALE end (its stream was already replaced/stopped).
    pub fn anim_is_current(&self, pid: u16, token: &Arc<AtomicBool>) -> bool {
        self.anim
            .get(&pid)
            .is_some_and(|a| Arc::ptr_eq(&a.stop, token))
    }

    /// Drop `pid`'s stream entry IF `token` is still the current one — a worker that ended ON ITS OWN
    /// (error / time cap) cleaning up after itself, without clobbering a stream that replaced it.
    pub fn anim_clear(&mut self, pid: u16, token: &Arc<AtomicBool>) {
        if self
            .anim
            .get(&pid)
            .is_some_and(|a| Arc::ptr_eq(&a.stop, token))
        {
            self.anim.remove(&pid);
        }
    }

    /// Re-pace `pid`'s live stream (the fps slider) without restarting it. No-op if it isn't streaming
    /// OR if the current stream is the paint-on-demand vitals surface (whose `fps` is unused) — so the
    /// slider can never silently write into a non-paced stream that happens to share the board's pid.
    pub fn set_anim_fps(&self, pid: u16, fps: u32) {
        if let Some(a) = self.anim.get(&pid) {
            if a.fps_paced {
                a.fps.store(fps, Ordering::Relaxed);
            }
        }
    }

    /// Start the cross-device VITALS surface on a worker thread — the GUI mirror of the CLI's
    /// `lighting mirror`. SINK = the selected lit, custom-frame-capable device (the keyboard); SOURCE
    /// = the first battery-capable mouse. Reads the mouse's live battery/charge/DPI-stage and paints
    /// `render_vitals` onto the keyboard ON-DEMAND (repaint only on change, or each tick while charging
    /// so the cyan crest animates). Same generation-token contract as `start_layers`. Returns a status.
    pub fn start_vitals(
        &mut self,
        sink_pid: u16,
        on_done: impl FnOnce(Option<String>, Arc<AtomicBool>) + Send + 'static,
    ) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        // keyed by the SINK board (the keyboard) so only ITS prior stream is replaced.
        if let Some(a) = self.anim.get(&sink_pid) {
            a.stop.store(true, Ordering::SeqCst);
        }
        let stop = Arc::new(AtomicBool::new(false));
        self.anim.insert(
            sink_pid,
            AnimStream {
                stop: stop.clone(),
                fps: Arc::new(AtomicU32::new(1)), // vitals paints on-demand; fps unused
                fps_paced: false,
            },
        );
        std::thread::spawn(move || {
            let outcome: Result<(), String> = (|| {
                let reg = Registry::load().map_err(|e| format!("registry: {e}"))?;
                let infos = transport::enumerate().map_err(|e| format!("enumerate: {e}"))?;
                // open the SINK (the selected custom-frame keyboard)
                let mut sink = None;
                let mut source = None;
                for i in &infos {
                    if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
                        let lit_frame = def
                            .lighting
                            .as_ref()
                            .map(|l| l.custom_frame.is_some())
                            .unwrap_or(false);
                        if sink.is_none()
                            && lit_frame
                            && def.matches_control(i.usage_page, i.usage, i.feature_len)
                            && (sink_pid == 0 || i.pid == sink_pid)
                        {
                            if let Ok(d) = Device::open(def.clone(), i.pid) {
                                sink = Some(d);
                            }
                        }
                        // SOURCE: a battery-capable device (the mouse). Don't reuse the sink.
                        if source.is_none()
                            && def.commands.contains_key("battery_level")
                            && def.matches_control(i.usage_page, i.usage, i.feature_len)
                        {
                            if let Ok(d) = Device::open(def.clone(), i.pid) {
                                source = Some(d);
                            }
                        }
                    }
                }
                let sink = sink.ok_or_else(|| {
                    "no custom-frame keyboard selected to paint vitals onto".to_string()
                })?;
                let source = source.ok_or_else(|| {
                    "no battery-capable mouse found to read vitals from (wake the Naga)".to_string()
                })?;
                let ldef = sink
                    .def
                    .lighting
                    .clone()
                    .ok_or_else(|| "sink has no lighting".to_string())?;
                let (rows, cols) = (ldef.rows, ldef.cols);
                let lights = Lights::new(&sink, ldef);
                lights
                    .ensure_control()
                    .map_err(|e| format!("control failed: {e}"))?;
                let mut last: Option<neuron::lighting::Vitals> = None;
                let mut phase: f32 = 0.0;
                while !stop.load(Ordering::SeqCst) && !neuron::writes::writes_paused() {
                    let v = read_vitals(&source, last);
                    let changed = last != Some(v);
                    if changed || last.is_none() || v.charging {
                        phase = (phase + 0.18) % 1.0;
                        let frame = neuron::lighting::render_vitals(v, rows, cols, phase);
                        let _ = lights.paint_frame(&frame);
                        last = Some(v);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
                Ok(())
            })();
            on_done(outcome.err(), stop);
        });
        "mirroring vitals".into()
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
    ) -> String {
        if name.trim().is_empty() {
            return "name required".into();
        }
        let mut p = Profile {
            name: name.to_string(),
            dpi: Some(dpi),
            polling_hz: Some(hz),
            brightness: Some(brightness),
            persist: self.persist,
            // capture the live gaming-mode policy (host-side, no device read).
            disable_alt_tab: self.gaming_mode.disable_alt_tab,
            disable_win: self.gaming_mode.disable_win,
            disable_alt_f4: self.gaming_mode.disable_alt_f4,
            ..Default::default()
        };

        // Real device read-back (best-effort; overrides the slider fallbacks when a device answers).
        if let Ok(d) = self.open_selected() {
            if let Ok((x, _)) = cap::dpi(&d) {
                p.dpi = Some(x);
            }
            // the FULL DPI stage list (what you cycle), not just the active one.
            p.dpi_stages = self.read_dpi_stages();
            if let Ok(hz) = cap::polling_rate_hz(&d) {
                p.polling_hz = Some(hz);
            }
            if let Ok(b) = cap::brightness_percent(&d) {
                p.brightness = Some(b);
            }
            if let Ok(secs) = cap::idle_timeout_secs(&d) {
                p.idle_secs = Some(secs as u32);
            }
        }

        // current lighting effect from a matrix device (decode the effect-id -> effect name).
        if let Some(def) = self.selected_def().filter(|d| d.lighting.is_some()) {
            if let Ok(d) = Device::open(def.clone(), self.selected_pid) {
                if let Some(l) = def.lighting.as_ref() {
                    if let Ok(st) = d.run("lighting_state") {
                        if st.len() > 2 {
                            if let Some((k, _)) = l.effects.iter().find(|(_, v)| **v == st[2]) {
                                p.lighting = Some(k.clone());
                            }
                        }
                    }
                }
            }
        }

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
        self.save_app_rules();
        format!("rule {app} -> {profile}")
    }

    pub fn remove_app_rule(&mut self, idx: usize) {
        if idx < self.app_rules.rules.len() {
            self.app_rules.rules.remove(idx);
            self.save_app_rules();
        }
    }

    fn save_app_rules(&self) {
        if let Ok(s) = toml::to_string_pretty(&self.app_rules) {
            let _ = std::fs::write(AppRules::path(), s);
        }
    }

    // ── backup ───────────────────────────────────────────────────────────

    pub fn backup(&self, pid: u16) -> String {
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(e) => return format!("enumerate failed: {e}"),
        };
        for i in &infos {
            if i.pid == pid {
                if let Some(def) = self.registry.find_by_pid(i.vid, i.pid).cloned() {
                    return snapshot_device(&def, pid);
                }
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

/// Read a source device's live vitals (battery %, charge, active DPI stage) for the cross-device
/// data surface — the GUI mirror of the CLI's `read_mouse_vitals`. Each sub-read is best-effort: an
/// asleep wireless mouse falls back to the last value rather than aborting the surface. The active
/// DPI stage comes from the `dpi_stages_active` (or `dpi_stages`) getter reply: `s[1]` = active index,
/// `s[2]` = stage count (live-confirmed in the CLI).
fn read_vitals(d: &Device, last: Option<neuron::lighting::Vitals>) -> neuron::lighting::Vitals {
    let prev = last.unwrap_or(neuron::lighting::Vitals {
        battery_pct: 0,
        charging: false,
        active_stage: 0,
        stage_count: 0,
    });
    let battery_pct = cap::battery_percent(d).unwrap_or(prev.battery_pct);
    let charging = cap::charging(d).unwrap_or(prev.charging);
    let (active_stage, stage_count) = d
        .run("dpi_stages_active")
        .or_else(|_| d.run("dpi_stages"))
        .ok()
        .map(|s| (s[1], s[2]))
        .unwrap_or((prev.active_stage, prev.stage_count));
    neuron::lighting::Vitals {
        battery_pct,
        charging,
        active_stage,
        stage_count,
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
        Trigger::Hotkey { .. } => "hotkey",
        Trigger::Gesture { .. } => "gesture",
        Trigger::RadialSector { .. } => "radial",
        Trigger::AppFocus { .. } => "app",
        Trigger::MicTap => "mic",
        Trigger::Hold { .. } => "hold",
        Trigger::Cast { .. } => "cast",
    }
}

/// Read one device's live state (best-effort). Each getter is independent so a partial/asleep
/// device still yields a row with what it could read.
fn read_device_state(def: &DeviceDef, pid: u16) -> DeviceState {
    let mode = def
        .mode_for(pid)
        .map(|m| m.name.clone())
        .unwrap_or_else(|| "?".into());
    let icon = icon_for(def);
    let mut st = DeviceState {
        name: def.name.clone(),
        codename: def.codename.clone(),
        pid,
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
        cap_scroll: def.supports(neuron::registry::Capability::SetScrollStage),
        cap_store: def.supports(neuron::registry::Capability::Storage),
        cap_idle: def.supports(neuron::registry::Capability::Battery),
        cap_plate: def.has_side_plates(),
    };
    if let Ok(d) = Device::open(def.clone(), pid) {
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
            // passive scan (from_event = false): feed the edge-detector off this read we already did.
            neuron::vitals::observe(pid, b, charging, false);
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
fn snapshot_device(def: &DeviceDef, pid: u16) -> String {
    use neuron::backup::{GetterSnap, IfaceSnap, Snapshot};
    use neuron::discover;
    let Ok(d) = Device::open(def.clone(), pid) else {
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
                "input" | "hotkey" | "gesture" | "radial" | "app" | "mic" | "hold" | "cast"
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
        let msg = rt.save_profile_from_devices(&name, 1234, 500, 60);
        // headless (no device), the slider values become the captured profile; the message reports
        // the capture either way ("captured" on the full-state path, "saved" historically).
        assert!(
            msg.contains("captured") || msg.contains("saved"),
            "unexpected: {msg}"
        );
        // it shows up in the reloaded list with the values we set.
        let p = rt
            .profiles
            .iter()
            .find(|p| p.name == name)
            .expect("saved profile present");
        assert_eq!(p.dpi, Some(1234));
        assert_eq!(p.polling_hz, Some(500));
        assert_eq!(p.brightness, Some(60));
        // delete it and confirm it's gone.
        let del = rt.delete_profile(&name);
        assert!(del.contains("deleted"), "unexpected: {del}");
        assert!(!rt.profiles.iter().any(|p| p.name == name));
    }

    /// An empty profile name is rejected (no silent empty-named file).
    #[test]
    fn empty_profile_name_rejected() {
        let mut rt = AppRuntime::load();
        let msg = rt.save_profile_from_devices("  ", 800, 1000, 50);
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
        rt.save_profile_from_devices(&name, 800, 1000, 50);
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
        rt.save_profile_from_devices(&name, 800, 1000, 50);
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
