// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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
//! `CastConfig` / Engine.

use neuron::bindings::Bindings;
use neuron::capability::{self as cap, Store};
use neuron::cast::CastConfig;
use neuron::device::Device;
use neuron::engine::{Rule, Trigger};
use neuron::gesture::Vault;
use neuron::lighting::{Effect, Lights, Rgb};
use neuron::profile::{AppRule, AppRules, Profile};
use neuron::registry::{DeviceDef, Registry};
use neuron::synth::{adopt_key, AdoptKey};
use neuron::transport;
use std::collections::HashMap;
use std::fmt::Write as _;
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
    /// The control PLANE's family (`DeviceDef::dialect`) — selection identity is (pid, unit,
    /// dialect) because one physical unit may carry several protocol families' control pipes, each
    /// its own plane. Rows are PER-PLANE: today every desk unit is N=1 (one plane per unit) so the
    /// list is byte-identical, but a future multi-family unit (`razer_report` + an audio dialect on
    /// one pid) lists one channel row per plane, and every routing decision keys off this rather
    /// than first-family-wins.
    pub dialect: String,
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
    pub cap_bright_set: bool, // SetBrightness (the WRITE, either dialect) — the BRIGHTNESS fader + its slot in the one-gesture apply; distinct from cap_light (a lighting BLOCK ≠ a brightness write: synthesized defs only carry lighting.brightness when the probe proved it)
    pub cap_scroll: bool, // SetScrollStage — scroll-wheel stages
    pub cap_store: bool, // Storage — persist-to-onboard
    pub cap_idle: bool, // Battery (wireless proxy) — the idle-off timer
    pub cap_plate: bool, // has a [side_plates] map — surfaces the push-detected side-plate readout
    pub cap_game_mode: bool, // SetGameMode — the keyboard's FIRMWARE FN+F10 Win-key kill (the
    // device-physical sibling of the host KEY GUARD chord swallows). Keyboard-only.
    /// PLACEHOLDER row — one WITHOUT a registry def behind it, so it is never auto-picked and is
    /// selectable-but-inert (every capability gate off, the FEEL/perf panels stay blank). Two
    /// kinds ride this one flag, so the glue selection policy needs ZERO cases for them (the point):
    ///   * LEARNING — an unknown pipe whose adoption probe is running now ("learning device…");
    ///     replaced by the real registry-backed row (same unit instance, so a selection carries
    ///     over) when the probe lands.
    ///   * UNRESPONSIVE — a razer-claimed pipe that answered nothing after N adoption retries
    ///     (the strike ledger), surfaced so a broken/asleep device isn't silently invisible.
    /// Both are non-operable (connected = false); only LEARNING later becomes a real row. (The
    /// NO-PROTOCOL unclaimed pipes were a THIRD kind here until 2026-07-07 — they double-listed
    /// hardware whose functional face already sits in the device list, so they moved out of the row
    /// model entirely and became one dim FOOTNOTE line: see `AppRuntime::unclaimed`.)
    pub adopting: bool,
}

/// One live lighting stream's controls, owned PER-DEVICE in [`AppRuntime::anim`]: the stop flag its
/// worker polls each frame, and the fps it reads live. Every stream is the layer compositor now (vitals
/// is just a `vitals` LAYER in the stack, not a bespoke paint-on-demand surface), so the fps slider
/// always applies to whatever board is streaming.
pub struct AnimStream {
    pub stop: Arc<AtomicBool>,
    pub fps: Arc<AtomicU32>,
}

/// Re-probe cadence for a pid that stayed unknown (e.g. a mouse deep-asleep at first probe — it
/// only wakes on user input, so poll slowly until it answers). This is BOTH the retry interval
/// `adopt_unknown_in_background` gates each re-probe on AND the window `adoption_pending` (the
/// watch-timer gate) uses to decide a retry is coming due — so a silent unknown device keeps the
/// timer alive just long enough to rescan once per window and adopt within ~a minute of waking.
const SYNTH_RETRY: std::time::Duration = std::time::Duration::from_mins(1);

/// The resident runtime state. UI-thread owned (held in an `Rc<RefCell<_>>` by the glue).
pub struct AppRuntime {
    pub registry: Registry,
    pub bindings: Bindings,
    pub cast: CastConfig,
    pub vault: Vault,
    pub app_rules: AppRules,
    pub profiles: Vec<Profile>,
    /// Profiles whose file wouldn't parse, as `(name, why)` — surfaced in the sheet so a broken
    /// profile reads as broken instead of silently missing.
    pub broken_profiles: Vec<(String, String)>,
    pub active_profile: String,
    pub persist: bool,
    /// Currently selected device pid (for per-device panels). 0 = none. The pid names the MODEL/
    /// link-mode (capability gates, per-model config); `selected_unit` names the physical unit.
    pub selected_pid: u16,
    /// The selected PHYSICAL unit (`transport::path_instance`) — what makes selection precise when
    /// two identical devices share a pid. Empty = no unit pinned (match by pid alone), which only
    /// happens before the first scan.
    pub selected_unit: String,
    /// The selected control plane's FAMILY (`DeviceDef::dialect`) — the third leg of the (pid,
    /// unit, dialect) selection identity. Empty = no plane pinned (match by pid/unit alone), the
    /// pre-first-scan / stateless state. On a unit exposing several families' pipes this is what
    /// keeps opens, snapshot seeding, and profile capture on the plane the user picked instead of
    /// whichever family enumerated first (the review's three findings share this root).
    pub selected_dialect: String,
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
    /// When each unknown [`AdoptKey`] ((dialect, pid)) was last probed by a background auto-adoption
    /// (`neuron::synth`) AND how many times it has been tried (the STRIKE count). Keyed on the full
    /// identity, never bare pid: two families can share a pid, so a bare-pid ledger would conflate
    /// their retry state (and let one family's strike silence the other). Not every scan tick — but
    /// not once-per-run either: a wireless device DEEP-asleep at first probe only wakes on user
    /// input, so unanswered keys retry on a slow cadence (`SYNTH_RETRY`) and adopt within a minute of
    /// waking. Each retry increments the strike; a key that reaches [`UNRESPONSIVE_STRIKES`]
    /// surfaces as a dim "answered nothing" placeholder row instead of staying invisible. A key
    /// that leaves enumeration is forgotten immediately (unplug → replug = the natural instant
    /// retry), which also resets its strikes.
    synth_attempted: HashMap<AdoptKey, (std::time::Instant, u32)>,
    /// [`AdoptKey`]s whose adoption probe is RUNNING right now (inserted before the thread spawns,
    /// cleared by the thread when it finishes). Drives the transient "learning device…" row so
    /// a new device is visible the instant it's plugged in, not after the multi-second probe.
    synth_inflight: Arc<std::sync::Mutex<std::collections::HashSet<AdoptKey>>>,
    /// [`AdoptKey`]s with a LIVE probe thread RIGHT NOW — the spawn guard that keeps the single-probe
    /// invariant even when a probe outlives the `SYNTH_RETRY` cadence (a stuck/slow wireless probe
    /// can exceed 60s, and re-pushing the key then would spawn a SECOND overlapping probe for it).
    /// Distinct from `synth_inflight`, which only drives the transient "learning…" row for a key's
    /// FIRST attempt: this set spans every attempt (first and retry) and gates spawning, not UI.
    synth_running: Arc<std::sync::Mutex<std::collections::HashSet<AdoptKey>>>,
    /// Set by a finished adoption thread; the next scan tick reloads the registry so the newly
    /// synthesized device appears without a restart.
    synth_dirty: Arc<AtomicBool>,
    /// Monotonic count of registry swaps — the def-identity half of the panel-seeding key
    /// (`seeded_key`). A reload means ANY def behind an unchanged unit id may have changed (the
    /// learning→real adoption flip, a user-edited devices/auto file), so a same-unit selection
    /// after a bump must re-seed. Bumped at every site that reassigns `self.registry`.
    pub registry_gen: u64,
    /// The (unit, dialect, `registry_gen`) the inspector panel was last seeded for — the select
    /// path's reseed decision. The seed identity is the control PLANE (unit + family), not the bare
    /// unit: a selection reseeds iff this differs from the newly selected plane's key, which catches
    /// a different unit, a same unit whose def changed under a reload, AND (on a future multi-family
    /// unit) a switch between two family channels sharing one unit id. None = never seeded.
    pub seeded_key: Option<(String, String, u64)>,
    /// Physical units first-light-checked this run — the once-per-unit-per-run guard for the tx
    /// self-heal. The heal is a PROBE cost (a handful of ACK'd lighting writes + read-backs to walk
    /// the tx cohort), NOT a per-apply cost, so it must fire at most once per unit per run: after a
    /// unit is verified (healed or already-right) its id lands here and no later apply re-probes it.
    /// Cleared only by relaunch — a run is the natural scope (the def on disk doesn't change under us
    /// except by our own heal, which flips the registry generation the normal way).
    pub healed_units: std::collections::HashSet<String>,
    /// The interested-but-unclaimed vendor pipes from the last scan — our hardware whose framing no
    /// dialect speaks (the audio sidecars: the 41-byte sound card, the 64-byte Seiren). This is
    /// INVENTORY, not channels: their functional faces (the Core-Audio mic/output rows) already list
    /// as real device rows, so surfacing each pipe as its own row double-listed real hardware (live
    /// complaint 2026-07-07). The device page renders the whole ledger as ONE dim footnote line under
    /// the deck, per DIALECT-RND's revised failed-adoption ruling. One row per pid (fattest pipe).
    pub unclaimed: Vec<neuron::synth::UnclaimedPipe>,
}

/// The single background-scan slot: at most one hardware scan (`scan_hardware`) in flight at a
/// time. A free-standing static (not an `AppRuntime` field) because the worker that clears it
/// runs on a spawned thread that never touches `AppRuntime` at all — see `scan_hardware`'s doc for
/// why. Mirrors `pump_vitals`'s local `IN_FLIGHT` and `synth_running`'s spawn guard.
static SCAN_BG_INFLIGHT: AtomicBool = AtomicBool::new(false);

/// Claim the background-scan slot; `false` if one is already running. The `ADOPT_WATCH_TIMER` tick
/// (glue.rs) calls this before spawning, so a slow scan (a sleepy wireless device's open can take
/// hundreds of ms) can never stack a second overlapping one under the 1s cadence.
pub fn scan_bg_try_start() -> bool {
    !SCAN_BG_INFLIGHT.swap(true, Ordering::AcqRel)
}

/// Release the background-scan slot — called once the worker's result (or its absence, on an
/// enumerate failure or a caught panic) has been handled.
pub fn scan_bg_finish() {
    SCAN_BG_INFLIGHT.store(false, Ordering::Release);
}

/// Adoption strikes at which a still-unrecognized, still-enumerated pid stops being merely retried
/// and starts SURFACING as a dim "unresponsive · answered nothing" placeholder row. Three tries
/// (~3 `SYNTH_RETRY` windows) is enough to distinguish "asleep, will wake" from "claimed but never
/// answers" without flashing a scary row at every device that's briefly slow to first-probe.
const UNRESPONSIVE_STRIKES: u32 = 3;

impl AppRuntime {
    pub fn load() -> Self {
        let registry = Registry::load().unwrap_or(Registry {
            devices: Vec::new(),
        });
        let mut profiles = Vec::new();
        let mut broken_profiles = Vec::new();
        for e in neuron::profile::load_all() {
            match e {
                neuron::profile::ProfileEntry::Ok(p) => profiles.push(*p),
                neuron::profile::ProfileEntry::Broken { name, why } => {
                    broken_profiles.push((name, why));
                }
            }
        }
        // Restore the profile CURSOR before anything reads it. Nothing is written to the device
        // here — it already holds what this profile applied last session. Restoring the name is
        // what makes three things true at boot: the header names the profile you're on, the
        // gaming-hook reconcile unit can resolve a real policy (its own doc says it is otherwise
        // dead after every reboot, and it was, because `active()` started empty), and the profile's
        // binds sidecar is in scope. It lives in core, not prefs, so `neuron run` restores the same
        // cursor instead of starting blind.
        // `restore_active` already refuses a cursor whose profile won't load. What it returns is
        // the string that was STORED, though, and the sheet's row highlight compares against each
        // profile's own `name` field — so adopt the canonical spelling here. A cursor saved as
        // "Valorant" against a file whose name field reads "valorant" would otherwise light the
        // header while no row in the list matched it.
        // `_once`, not the unconditional restore: this is a CONSTRUCTOR, and the unconditional one
        // is authoritative in both directions — it assigns EMPTY when the run root holds no cursor
        // file. Calling it from here wiped the process-wide cursor every time a runtime was built
        // against a directory without one, which in the test binary (many run roots, one process)
        // silently cleared a cursor another test had just set. The `_once` form keeps a cursor the
        // process has already chosen and only reads disk when there is nothing to keep.
        let remembered = neuron::profile::restore_active_once();
        let active_profile = profiles
            .iter()
            .find(|p| {
                !remembered.is_empty()
                    && Profile::file_key(&p.name) == Profile::file_key(&remembered)
            }).map_or_else(|| "—".to_string(), |p| p.name.clone());
        // …and seed the runtime's gaming-mode copy from that same profile. The startup reconcile
        // unit installs the HOOK from the cursor, but THIS copy is what a capture reads: left at
        // default, re-capturing the profile you are already on would silently drop its key guards.
        let gaming_mode = profiles
            .iter()
            .find(|p| p.name == active_profile)
            .map(neuron::profile::Profile::gaming_mode)
            .unwrap_or_default();
        AppRuntime {
            registry,
            bindings: Bindings::load(),
            cast: CastConfig::load(),
            vault: Vault::load(),
            app_rules: AppRules::load(),
            profiles,
            broken_profiles,
            active_profile,
            persist: false,
            selected_pid: 0,
            selected_unit: String::new(),
            selected_dialect: String::new(),
            anim: HashMap::new(),
            light_fps: Arc::new(AtomicU32::new(30)),
            gaming_mode,
            synth_attempted: HashMap::new(),
            synth_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            synth_running: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
            synth_dirty: Arc::new(AtomicBool::new(false)),
            registry_gen: 0,
            seeded_key: None,
            healed_units: std::collections::HashSet::new(),
            unclaimed: Vec::new(),
        }
    }

    /// A clone of the shared `synth_dirty` flag — the SAME `Arc<AtomicBool>` the adoption worker
    /// sets. The first-light heal worker (in glue) sets it after rewriting an auto file, so the
    /// next `scan_devices` reloads the registry, bumps `registry_gen`, and the seeded-key machinery
    /// re-seeds every downstream panel — the exact reload chain a successful adoption already rides.
    pub fn synth_dirty_handle(&self) -> Arc<AtomicBool> {
        self.synth_dirty.clone()
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
        self.reload_registry_if_dirty();
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let out = scan_units(&self.registry, &infos);
        self.finish_scan(infos, out)
    }

    /// A finished background adoption wrote a new devices/auto/*.toml — reload the registry so
    /// the freshly synthesized device becomes a row this very tick. A disk read (small TOML
    /// files), never device I/O, so this always runs on the owning thread — in `scan_devices`
    /// directly, and in `finish_background_scan` once a backgrounded scan's result lands.
    /// Returns whether a reload actually happened, so the BACKGROUND completion path can tell
    /// that rows computed before this point were resolved against a now-stale registry.
    fn reload_registry_if_dirty(&mut self) -> bool {
        if self.synth_dirty.swap(false, Ordering::SeqCst) {
            if let Ok(reg) = Registry::load() {
                self.registry = reg;
                // A def behind an existing unit id may have changed (learning→real, edited auto
                // file) — bump the generation so a same-unit selection re-seeds the panel.
                self.registry_gen = self.registry_gen.wrapping_add(1);
            }
            // Keep the app-layer shared cache (hidwatch arming + glue capability gating) in step with
            // this reload, so a freshly-adopted device's event pipe arms + gates without a restart.
            crate::hidwatch::reload_registry();
            return true;
        }
        false
    }

    /// Fold a finished BACKGROUND scan (see [`scan_hardware`]) into `self` — the exact same
    /// in-memory tail `scan_devices` runs synchronously (registry-dirty reload, adoption
    /// bookkeeping, the transient learning/placeholder rows, the unclaimed ledger, selection
    /// healing). No device I/O happens here — `infos`/`out` are the worker's plain-data result —
    /// so this is cheap and safe to call straight from the UI thread once
    /// `slint::invoke_from_event_loop` hops the result back (see the `ADOPT_WATCH_TIMER` install in
    /// glue.rs, the fix for the UI-thread stall this split exists for).
    ///
    /// The `bool` in the return is the STALE flag: `true` means an adoption finished between the
    /// worker loading ITS registry snapshot (`scan_hardware` can't borrow `self` off-thread, so it
    /// loads its own) and this completion running — the rows in hand were resolved against the
    /// PRE-adoption registry and may omit the freshly adopted device. The caller MUST answer
    /// `true` by kicking one more background scan immediately: it cannot rely on the adopt-watch
    /// timer to self-correct, because the successful adoption is exactly what clears
    /// `adoption_pending()` and stops that timer — the stale list would otherwise stand until a
    /// manual refresh. (The synchronous `scan_devices` has no such window: it reloads BEFORE
    /// resolving rows.) Convergent by construction: each re-kick is one guarded scan, and only a
    /// NEW adoption landing mid-flight can set the flag again.
    pub fn finish_background_scan(
        &mut self,
        infos: Vec<transport::HidDeviceInfo>,
        out: Vec<DeviceState>,
    ) -> (Vec<DeviceState>, bool) {
        let stale = self.reload_registry_if_dirty();
        (self.finish_scan(infos, out), stale)
    }

    /// The cheap, in-memory tail of a scan — everything AFTER the hardware I/O (`scan_units`):
    /// adoption bookkeeping, the transient "learning…"/"unresponsive" rows, the unclaimed ledger,
    /// and selection healing. Shared by the synchronous `scan_devices` and the backgrounded
    /// `finish_background_scan` so the two paths can never drift apart.
    fn finish_scan(
        &mut self,
        infos: Vec<transport::HidDeviceInfo>,
        mut out: Vec<DeviceState>,
    ) -> Vec<DeviceState> {
        self.adopt_unknown_in_background(&infos);
        // A device being adopted RIGHT NOW is visible immediately as a transient "learning"
        // row (product string as its name, every control gated off) instead of appearing out
        // of thin air seconds later. ONE row per razer_report pipe — the same signature the
        // probe targets — never one per HID collection (a mouse exposes a dozen collections
        // under its pid; without this filter the page flooded with duplicate learning rows).
        // Same unit-instance id as the real row that replaces it, so a selection made during
        // the probe carries straight over.
        if let Ok(inflight) = self.synth_inflight.lock() {
            for i in &infos {
                // ONE learning row per CLAIMED control pipe — the same pipes the probe targets,
                // now family-agnostic (was the hardcoded razer VID + 91-byte test). A new dialect
                // makes its in-flight adoptions render here with zero change to this app code.
                // Derive the adoption key (the same claiming test, now yielding identity) and match
                // inflight on the FULL key so a twin-family pid can't borrow the other's learning row.
                let Some(key) = adopt_key(i) else {
                    continue;
                };
                let family = key.0;
                if !inflight.contains(&key) {
                    continue;
                }
                let instance = i.instance();
                if out.iter().any(|d| d.instance == instance) {
                    continue; // another claimed control pipe of the same learning unit
                }
                out.push(learning_row(i, family, instance));
            }
        }
        // FAILED-ADOPTION SURFACE (DIALECT-RND, REVISED 2026-07-07): two truths, two presentations.
        // A CLAIMED pipe that stays unresponsive is a DEVICE STATE → a dim, non-selectable
        // placeholder row (below) so "couldn't reach this" is honest data on screen, appended AFTER
        // the real rows, `adopting = true` (never auto-picked, inert) and `connected = false`. An
        // UNCLAIMED-but-interested pipe is INVENTORY, not a channel → the footnote line, not a row
        // (see the `self.unclaimed` stash further down). Only the strike source builds rows here:
        //   (a) STRIKE rows — pids some dialect CLAIMED but whose probe never answered after
        //       UNRESPONSIVE_STRIKES retries. The attempted-ledger retains ONLY live+unrecognized
        //       pids, so an entry here is guaranteed still on the bus — no separate liveness check.
        for (&(family, pid), &(_, strikes)) in &self.synth_attempted {
            if strikes < UNRESPONSIVE_STRIKES {
                continue;
            }
            // The key already CARRIES the claiming family (`key.0`), so the label no longer
            // re-derives it via `claimed_by` — the ledger key IS the identity, disambiguated across
            // two same-pid families. The product-string lookup matches the same key (not bare pid)
            // so a twin family's pipe can't lend its product name here. The attempted-ledger retains
            // only live+unrecognized keys, so a match is guaranteed still on the bus.
            let info = infos.iter().find(|i| adopt_key(i) == Some((family, pid)));
            let name = info
                .map(|i| i.product.trim().to_string())
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| format!("{family} device {pid:04x}"));
            out.push(placeholder_row(
                name,
                pid,
                format!("unresponsive-{pid:04x}"),
                format!("unresponsive · claimed by {family}, never answered"),
                // The plane's family is the claiming dialect (`key.0`) — a placeholder row carries
                // it like a real row so its selection identity is complete even while inert.
                family.to_string(),
            ));
        }
        // The UNCLAIMED LEDGER (vendor pipes NO dialect can frame — the audio sidecars) is NOT a
        // second source of placeholder rows: as of 2026-07-07 it is INVENTORY, not channels. Each
        // such pipe's functional face already lists above as a real device row (the Seiren's mic,
        // the sound card's output — Core-Audio endpoints), so a row per pipe double-listed real
        // hardware. Stash the ledger on the runtime instead; the device page renders it as ONE dim
        // footnote line under the deck (glue formats `unclaimed_note`). Computed over the enumeration
        // we ALREADY hold (`unclaimed_from`, not `unclaimed_pipes`, so scan never enumerates twice);
        // `unclaimed_from` already dedupes to one row per pid (fattest feature_len).
        self.unclaimed = neuron::synth::unclaimed_from(&self.registry, &infos);
        // Selection follows reality: if the selected unit is no longer enumerated (unplugged,
        // dongle gone) — or nothing was selected yet — adopt the first recognized unit so the
        // per-device panels never target a ghost. pid 0 / empty unit never match a row, so this
        // one branch covers both first-scan auto-pick and stale-selection healing. Learning
        // rows are never auto-picked (their def doesn't exist yet, so panels would ghost).
        if !out.iter().any(|d| {
            d.pid == self.selected_pid
                && d.instance == self.selected_unit
                && d.dialect == self.selected_dialect
        }) {
            // Selection identity is the full PLANE (pid, unit, dialect): heal by matching all three,
            // and seed all three from the picked row. pid 0 / empty unit / empty dialect never match
            // a real row, so this one branch still covers both first-scan auto-pick and stale-plane
            // healing (the empty dialect is just as un-matchable as the empty unit already was).
            let first = out.iter().find(|d| !d.adopting);
            self.selected_pid = first.map_or(0, |d| d.pid);
            self.selected_unit = first.map(|d| d.instance.clone()).unwrap_or_default();
            self.selected_dialect = first.map(|d| d.dialect.clone()).unwrap_or_default();
        }
        out
    }

    /// Spawn one background auto-adoption (`neuron::synth`) per UNKNOWN Razer pid seen this
    /// run: any `razer_report` pipe the registry can't resolve gets probed + synthesized into
    /// devices/auto/<pid>.toml off-thread, then `synth_dirty` makes the next scan tick reload
    /// the registry. Zero cost when everything is recognized (the common case — a pid-set
    /// diff over the enumeration the scan already did). The probe is safe to run while the
    /// app works: it only touches the unknown device, which nothing else opens until the
    /// registry knows it.
    fn adopt_unknown_in_background(&mut self, infos: &[transport::HidDeviceInfo]) {
        // The ledger tracks UNKNOWN pids only: drop entries whose pid left the bus (unplug →
        // replug = the natural instant retry) OR became registry-recognized (a successful adoption
        // must retire its own retry state — a stale entry would arm `adoption_pending` forever and
        // turn the watch timer into a permanent 1 Hz rescan loop). scan_devices reloads the registry
        // BEFORE calling here, so an adopted pid already resolves and gets swept on this same pass.
        let reg = &self.registry;
        // Retain iff some enumerated pipe STILL yields this exact key AND the registry can't resolve
        // its pid — key-qualified so a twin family's pipe on the same pid can't keep a stale entry
        // alive (both parts preserved from the bare-pid retain: same-key liveness + still-unknown).
        self.synth_attempted.retain(|key, _| {
            // Still-unknown is a FAMILY question (key.0 = the claiming dialect): a stale retry entry
            // survives only while a pipe still maps to this exact key AND that family is unadopted.
            // knows_family (not find_by_pid) so a sibling family's def on the same pid can't retire
            // another family's retry state — the same family-scoping the loop below uses.
            infos.iter().any(|i| {
                adopt_key(i) == Some(*key) && !reg.knows_family(i.vid, i.pid, key.0)
            })
        });
        let now = std::time::Instant::now();
        let mut unknown: Vec<AdoptKey> = Vec::new(); // keys to probe this pass
        let mut first_try: Vec<AdoptKey> = Vec::new(); // subset never probed before → "learning" row
        // Single-probe guard, locked ONCE for the whole build: a pid whose probe thread is still
        // running (a slow/stuck wireless probe can outlast SYNTH_RETRY) must not be re-pushed here,
        // or we'd spawn a SECOND overlapping probe for it. The same guard is written below with the
        // pids we actually spawn on, so the "already running → skip" read and the "now running"
        // mark are one critical section. A poisoned lock (a probe thread panicked) recovers rather
        // than wedging adoption forever.
        let mut running = match self.synth_running.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        for i in infos {
            // Derive the adoption identity ONCE per pipe. This REPLACES the old separate
            // `claimed_by(i).is_none()` guard — same claiming test (None = no family claims it,
            // skip) — and yields the (dialect, pid) key that every dedupe/retry below hangs on, so
            // a probe's scope can't spill across two families that share a pid. The app's adoption
            // machinery stays family-agnostic: the claiming decision lives entirely in core.
            let Some(key) = adopt_key(i) else {
                continue;
            };
            // Already-known is family-scoped (key.0 = the claiming dialect): a razer def on this pid
            // must NOT suppress adopting the same unit's second-family pipe. knows_family, not
            // find_by_pid, is what keeps the second family adoptable on a shared pid.
            if self.registry.knows_family(i.vid, i.pid, key.0)
                || unknown.contains(&key)
                || running.contains(&key)
            {
                continue;
            }
            match self.synth_attempted.get(&key) {
                None => {
                    unknown.push(key);
                    first_try.push(key);
                }
                // A key that stayed silent retries on the slow cadence — WITHOUT the
                // "learning…" row, so a never-answering pipe doesn't flash UI every minute.
                Some((at, _strikes)) if now.duration_since(*at) >= SYNTH_RETRY => unknown.push(key),
                Some(_) => {}
            }
        }
        if unknown.is_empty() {
            return;
        }
        // Stamp the attempt time AND bump the strike count: a first try starts at 1, each retry
        // increments, and the count is what drives the "unresponsive" placeholder row once it
        // reaches UNRESPONSIVE_STRIKES. (A key that leaves the bus is retained out entirely above,
        // so its strikes reset on replug — the natural fresh start.)
        for &key in &unknown {
            let strikes = self
                .synth_attempted
                .get(&key)
                .map_or(0, |(_, n)| *n);
            self.synth_attempted.insert(key, (now, strikes + 1));
        }
        if let Ok(mut inflight) = self.synth_inflight.lock() {
            inflight.extend(first_try.iter().copied());
        }
        // Mark exactly the pids we're about to spawn on as running, then release the guard before
        // spawning. The RELEASE runs whether the worker finishes, panics, or the OS refuses the
        // thread — so a spawn failure can never leave these keys latched "running"/"learning" and
        // block every future adoption attempt for the rest of the run.
        running.extend(unknown.iter().copied());
        drop(running);
        let dirty = self.synth_dirty.clone();
        let inflight = self.synth_inflight.clone();
        let running = self.synth_running.clone();
        let release_keys = unknown.clone(); // the worker consumes `unknown`; the guard needs its own
        let release = move || {
            // Retire the "learning…" rows for FIRST attempts, and drop the single-probe guard for
            // every key this thread owned so a still-unknown key can be re-probed next tick.
            // `into_inner` on poison: unconditional release is the whole point — a poisoned lock
            // must not strand the latch any more than a spawn failure may.
            inflight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|k| !first_try.contains(k));
            running
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|k| !release_keys.contains(k));
        };
        crate::worker::spawn_guarded("neuron-runtime-adopt", release, move || {
            // Fresh registry (not a clone of the UI's): adoption must judge "unknown" against
            // what's on DISK, so a def another process adopted meanwhile isn't re-probed. Probe
            // ONLY this worker's keys (`adopt_keys`, not `adopt_unknown`): a second worker spawned
            // for a DIFFERENT key must not also probe — and double-write the auto file of — a key
            // this worker already owns. Keying on (dialect, pid) is what makes that scope precise
            // when two families share a pid: this worker's key never selects the other's pipe.
            if let Ok(reg) = Registry::load() {
                if let Ok(a) = neuron::synth::adopt_keys(&reg, &unknown) {
                    if !a.adopted.is_empty() {
                        dirty.store(true, Ordering::SeqCst);
                    }
                }
            }
        });
    }

    /// Is an auto-adoption in flight, or its result not yet folded into the device list? The
    /// device page polls this on a light timer and re-scans while true, so a "learning…" row
    /// resolves into the real device the moment its probe lands — no manual re-scan. A cheap
    /// flag check when nothing is being adopted (the permanent case).
    pub fn adoption_active(&self) -> bool {
        self.synth_dirty.load(Ordering::SeqCst)
            || self
                .synth_inflight
                .lock()
                .is_ok_and(|s| !s.is_empty())
    }

    /// Does the adoption machinery need the watch timer to keep ticking — active work NOW, or a
    /// RETRY coming due? `adoption_active` alone goes false after a FAILED first probe (inflight
    /// cleared, nothing dirty), which used to put the timer to sleep and orphan the `SYNTH_RETRY`
    /// cadence entirely (the "adopts within a minute of waking" promise had no driver). The retry
    /// half is a pure in-memory check over `synth_attempted` — no enumeration, no device I/O — so
    /// an attached-but-silent unknown device costs one real rescan per `SYNTH_RETRY` window and a
    /// flag check per tick, nothing more. Self-cleaning: unplugging the device lets the next scan
    /// retain the key out of `synth_attempted`, and the gate goes permanently quiet.
    pub fn adoption_pending(&self) -> bool {
        self.adoption_active()
            || self
                .synth_attempted
                .values()
                .any(|(at, _strikes)| at.elapsed() >= SYNTH_RETRY)
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
        match resolve_plane(
            &self.registry,
            &infos,
            self.selected_pid,
            &self.selected_unit,
            &self.selected_dialect,
        ) {
            Some((def, i)) => {
                let gate = crate::host::io_gate(i.pid);
                Device::open_path(def.clone(), i.pid, &i.path)
                    .map(|dev| GatedDevice { _gate: gate, dev })
            }
            None => anyhow::bail!("no recognized Razer device connected"),
        }
    }

    /// The def of the selected device, if known/connected.
    pub fn selected_def(&self) -> Option<DeviceDef> {
        let infos = transport::enumerate().ok()?;
        // The one plane resolver: the selected (pid, unit, dialect) plane, unit-precise then
        // pid-healed. Sharpens the old pid-only match with the same unit + family precision every
        // other selection consumer uses, so a two-family unit returns the picked family's def, not
        // find_by_pid's first-by-pid one.
        resolve_plane(
            &self.registry,
            &infos,
            self.selected_pid,
            &self.selected_unit,
            &self.selected_dialect,
        )
        .map(|(def, _)| def.clone())
    }

    // ── performance setters (gated) ──────────────────────────────────────

    pub fn apply_dpi(&self, dpi: u16) -> String {
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => {
                let _ = d.run("device_mode"); // wake / ensure reachable
                // DUAL-PLANE by design (2026-07-23): volatile first so the mouse acts right now,
                // then onboard-persist so hardware truth survives power-cycles and zero-software
                // operation. A device without a working persist plane keeps the volatile success
                // (its durability is the host's feel-intent reassert-on-wake instead).
                match cap::set_dpi(&d, dpi, dpi, cap::Store::Volatile, neuron::dpi_origin::Cause::UserApplied) {
                    Ok(()) => {
                        let onboard = cap::set_dpi(&d, dpi, dpi, cap::Store::Persist, neuron::dpi_origin::Cause::UserApplied);
                        // confirmation fires past the committed write — same as apply_polling /
                        // apply_brightness. Absolute set → no prior read, so no old→new.
                        neuron::confirm::dpi_unit(d.pid, &d.dpi_unit, u32::from(dpi), None);
                        match onboard {
                            Ok(()) => format!("DPI -> {dpi} (saved to mouse)"),
                            Err(e) => format!("DPI -> {dpi} (onboard save unavailable: {e})"),
                        }
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
                Ok(()) => {
                    neuron::confirm::brightness(u32::from(pct), None);
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
        // kill-switch AFTER parse (parse is read-only and its accounting is useful even while
        // paused; also keeps the parse honest under the process-global pause other code may flip).
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => {
                let _ = d.run("device_mode");
                // DUAL-PLANE by design (2026-07-23): volatile (acts now) then onboard-persist
                // (survives power-cycle / zero-software) — see apply_dpi. The writer records the
                // host feel intent itself, from the declared cause.
                match neuron::writes::set_dpi_stages(&d, &stages, active, cap::Store::Volatile, neuron::dpi_origin::Cause::UserApplied) {
                    Ok(()) => {
                        let onboard =
                            neuron::writes::set_dpi_stages(&d, &stages, active, cap::Store::Persist, neuron::dpi_origin::Cause::UserApplied);
                        match onboard {
                            Ok(()) => format!(
                                "DPI stages [{}] active {} (saved to mouse)",
                                fmt_stages(&stages),
                                active + 1
                            ),
                            Err(e) => format!(
                                "DPI stages [{}] active {} (onboard save unavailable: {e})",
                                fmt_stages(&stages),
                                active + 1
                            ),
                        }
                    }
                    Err(e) => format!("DPI stages failed: {e}"),
                }
            }
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply `HyperScroll` wheel stages (class 0x0B) — verify-gated + hardware-pending; surfaces the
    /// honest gated message when the env flag is unset. `list` is "/"-separated mode names/bytes.
    pub fn apply_scroll_stages(&self, list: &str) -> String {
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
        // kill-switch AFTER parse — see apply_dpi_stages.
        if self.writes_paused() {
            return "writes paused".into();
        }
        match self.open_selected() {
            Ok(d) => match neuron::writes::set_scroll_stages(&d, &modes, 0, self.store()) {
                Ok(()) => format!("scroll stages ({} mode(s)) applied", modes.len()),
                Err(e) => format!("scroll stages [gated]: {e}"),
            },
            Err(e) => format!("no device: {e}"),
        }
    }

    /// Apply the LED idle-off timeout (seconds) — verify-gated (`NEURON_IDLE_WRITE`), honest [gated].
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

    /// Apply the in-game polling split (wired/dongle Hz) — verify-gated (`NEURON_INGAME_POLL_WRITE`).
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
    /// (`writes::set_lift_off_distance` re-reads 0x0B/0x85) + env-gated (`NEURON_LOD_WRITE`).
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
    /// do it (the user's `BlackWidow` Chroma V2), exactly like the LOD / idle / in-game-polling writes.
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

    /// TOGGLE the keyboard FIRMWARE game mode (the FN+F10 Win-key kill) — a read-modify-write against
    /// the DEVICE, not the UI. The checkbox is seeded ASYNCHRONOUSLY (see glue's `seed_perf_async`) and
    /// is cleared to false on every device switch, so it can be stale for the first ~500ms after a
    /// switch — deriving the write from it sent INVERTED commands (a click in that window on an already
    /// game-mode board wrote ON again instead of off; review-caught). The device is the ONLY truthful
    /// source of "current", so we read it (`cap::game_mode`) in the SAME open we write through: read
    /// truth → write `!current` (which `cap::set_game_mode` read-back verifies, bailing on MISMATCH) →
    /// return `(Some(new_state), status)`. Any step failing returns `(None, honest error)` so the caller
    /// posts the error and leaves the display cache untouched. Honours writes-paused first.
    pub fn toggle_game_mode(&self) -> (Option<bool>, String) {
        if self.writes_paused() {
            return (None, "writes paused".into());
        }
        let d = match self.open_selected() {
            Ok(d) => d,
            Err(e) => return (None, format!("no device: {e}")),
        };
        // READ device truth — the toggle's pivot. Never the UI cache (stale post-switch).
        let current = match neuron::capability::game_mode(&d) {
            Ok(c) => c,
            Err(e) => return (None, format!("game mode [unreadable]: {e}")),
        };
        let want = !current;
        match neuron::capability::set_game_mode(&d, want) {
            Ok(()) if want => (
                Some(true),
                "keyboard game mode -> ON \u{00b7} Win key dead in firmware".to_string(),
            ),
            Ok(()) => (Some(false), "keyboard game mode -> off".to_string()),
            Err(e) => (None, format!("game mode [failed]: {e}")),
        }
    }

    // ── lighting ─────────────────────────────────────────────────────────

    /// Effects available on the selected device (native first, then emulated).
    pub fn effects(&self) -> Vec<(String, bool, bool)> {
        match self.selected_def().and_then(|d| d.lighting) {
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
            .and_then(|d| d.lighting)
            .map_or((0, 0), |l| (l.rows, l.cols))
    }

    /// The selected device's kind hint ("keyboard"/"mouse"/…) — drives the procedural chassis
    /// the lighting render draws around the LED lattice. Empty when nothing is selected.
    pub fn grid_kind(&self) -> &'static str {
        self.selected_def().map_or("", |d| icon_for(&d))
    }

    /// The DEFAULT streaming fps for the selected lit device + whether it's a LEGACY board (the
    /// GUI's protocol note). Both protocols now default to 30: the old legacy-6 seed encoded
    /// "frames drop above ~6" folklore that a live wire probe falsified — the `BlackWidow` sustains
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
                    Ok(()) => format!("effect -> {name}"),
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
        // `on_done` (glue) is the SOLE cleanup authority: it removes this board's `anim` entry and
        // clears the "compositing" indicator. The worker returns its outcome and `done` calls
        // `on_done` exactly once — with the real error, a synthesized error on spawn refusal, or
        // `None` on success — so a refused/panicked thread can't leave a PHANTOM compositor (entry
        // present + indicator lit + no thread). `on_done` defers its work via invoke_from_event_loop,
        // so the synchronous spawn-fail path only POSTS the cleanup — no re-borrow of `self` here.
        let stop_for_done = stop.clone();
        let spawned = crate::worker::spawn_notify(
            "neuron-runtime-anim",
            move || {
            let outcome: Result<(), String> = (|| {
                let reg = Registry::load().map_err(|e| format!("registry: {e}"))?;
                let infos = transport::enumerate().map_err(|e| format!("enumerate: {e}"))?;
                for i in &infos {
                    // find_for_pipe: the family-aware control-pipe def, so a two-family unit streams
                    // to the family that can paint this pipe.
                    if let Some(def) = reg.find_for_pipe(i) {
                        if (pid == 0 || i.pid == pid)
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
                            let streamed = lights
                                // LIVE fps: read the shared atomic each frame so the GUI's fps
                                // control re-paces this running composite without a restart.
                                .animate(&mut comp, || fps_src.load(Ordering::Relaxed), 86_400, || {
                                    stop.load(Ordering::SeqCst) || neuron::writes::writes_paused()
                                })
                                .map_err(|e| format!("animate: {e}"));
                            // CUSTODY RELEASE (the DPI-16000 trap): streaming lighting holds this board
                            // in driver mode (every write flips it via ensure_driver), which defers its
                            // onboard buttons/FN to software AND orphans the wake-reassert duty. Driver
                            // mode is a LEASE for the stream's duration, not a permanent state — now that
                            // THIS board's (only) stream is ending and no host writer holds the device,
                            // hand ownership back to the firmware. Routed through `release_custody` (the
                            // stream's def IS razer today — lighting blocks only exist there — but the
                            // dialect hook means a future STREAMING family releases ITS own custody
                            // instead of receiving a razer-framed mode packet). Best-effort on the
                            // stream's OWN handle (the cleanest teardown point: the Device is still open
                            // here); the next write re-flips driver mode idempotently. Skipped when the
                            // host owns the writer — it manages the board's mode itself and may still be
                            // painting it. Also skipped under the writes-paused kill-switch — the
                            // stream ALSO exits on pause, and a paused state must freeze every device
                            // write (exit-restore + the next wake-reassert still cover the lease).
                            if !crate::host::active() && !neuron::writes::writes_paused() {
                                let _ = d.release_custody();
                            }
                            return streamed;
                        }
                    }
                }
                Err("device not found".into())
            })();
            outcome.err()
            },
            move |res| {
                // res: Some(err_opt) = worker ran; None = thread refused or panicked.
                let err = res.unwrap_or_else(|| Some("lighting thread could not start".into()));
                on_done(err, stop_for_done);
            },
        );
        if !spawned {
            // `done` already ran (posted the cleanup); report the failure, not "compositing".
            return "lighting thread could not start".into();
        }
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
        // The latch is cleared by the release guard, which runs whether the worker completes,
        // panics, OR the OS refuses the thread — so a spawn failure under resource exhaustion
        // can never strand `IN_FLIGHT` true and silence vitals for the rest of the run.
        crate::worker::spawn_guarded(
            "neuron-runtime-vitals",
            || IN_FLIGHT.store(false, Ordering::Release),
            move || publish_source_vitals(forced),
        );
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
        // Scoped to the active profile by the loader, so this list shows the binds that are
        // actually live right now — not every profile's binds at once.
        let sidecars =
            neuron::controls::load_rule_sidecars_except(neuron::controls::GUI_RULES_FILE);
        neuron::controls::build_runtime_from(&self.bindings, &self.cast, &self.app_rules, &sidecars)
            .engine
            .to_rules()
    }

    // ── profiles ─────────────────────────────────────────────────────────

    /// Reload the saved profiles, keeping the unreadable ones as named faults rather than dropping
    /// them. The old `filter_map(...ok())` made a profile with one bad line disappear from the sheet
    /// while its file sat on disk — no row, no message, and the name still occupied.
    pub fn reload_profiles(&mut self) {
        self.profiles.clear();
        self.broken_profiles.clear();
        for e in neuron::profile::load_all() {
            match e {
                neuron::profile::ProfileEntry::Ok(p) => self.profiles.push(*p),
                neuron::profile::ProfileEntry::Broken { name, why } => {
                    self.broken_profiles.push((name, why));
                }
            }
        }
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
        // one reserved-name check for every naming door — "gui" would aim this profile's binds
        // sidecar at gui.rules.toml (the binds you authored in the app), and a Windows device name
        // fails the write with an opaque OS error instead of a sentence.
        if let Some(why) = neuron::profile::name_conflict(name) {
            return why;
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
            // respect the device the user picked in the UI — pid, physical unit, AND dialect, so a
            // rig with two identical devices captures the exact board being edited, not its twin,
            // and a future multi-family unit captures the exact control PLANE (not first-family).
            self.selected_pid,
            &self.selected_unit,
            &self.selected_dialect,
        );

        // The UI slider values are fallbacks only — applied where the device did not answer.
        p.dpi = p.dpi.or(Some(dpi));
        p.polling_hz = p.polling_hz.or(Some(hz));
        p.brightness = p.brightness.or(Some(brightness));

        // capture_from_devices can't synthesize a LayerDef stack from raw effect registers, so the
        // caller hands us the LIVE compositor stack — that's what gets saved as this profile's lighting.
        p.lighting = lighting;

        match p.save() {
            Ok(()) => {
                self.reload_profiles();
                format!("captured profile '{name}': {}", p.summary())
            }
            Err(e) => format!("save failed: {e}"),
        }
    }

    pub fn delete_profile(&mut self, name: &str) -> String {
        // A profile is BOTH files: `<name>.toml` and the `<name>.rules.toml` binds paired with it.
        // Deleting only the first left the binds live forever (every sidecar folds into the spine)
        // with no UI able to remove them, and the orphan reserved the filename so re-importing the
        // same profile landed at " (2)". `Profile::delete` owns both.
        let r = Profile::delete(name);
        self.reload_profiles();
        match r {
            Ok(()) => {
                // a deleted profile can't stay "active" — the header pill must drop to none.
                // Compare against the PROCESS-WIDE cursor too, not just our display copy: an
                // async apply updates the cell on its worker before this copy catches up, and
                // a delete landing in that window used to leave the cell dangling.
                if self.active_profile == name || neuron::profile::active() == name {
                    self.active_profile = "—".into();
                    neuron::profile::set_active("");
                    // The profile is gone, so its host-side key suppression must go with it.
                    // Without this the Key Guard stayed lit and Alt+Tab stayed swallowed with no
                    // profile left to explain it — while the panel's own caption said it lifts when
                    // the profile changes. Live-reproduced: delete a gaming profile, Alt+Tab dies.
                    self.gaming_mode = neuron::writes::GamingMode::default();
                    crate::dispatch::set_gaming_policy(self.gaming_mode);
                }
                // The FALLBACK is different from a rule: a rule that goes dangling still shows in
                // the list where you can see and remove it, but a dangling fallback would fire on
                // every unmatched focus change and fail, while the picker (which resolves by name)
                // quietly displayed "stay put". Clear it rather than keep a hidden broken setting.
                // Same rule as everywhere else here: a write that didn't happen is not a success.
                // A dangling fallback left on disk resumes switching to a deleted profile.
                let cleared_fallback = if self.app_rules.default.as_deref() == Some(name) {
                    self.app_rules.default = None;
                    match self.save_app_rules() {
                        Ok(()) => Some(String::new()),
                        // memory back in step with disk: the fallback is still there, still
                        // pointing at the profile just deleted, and the panel must say so rather
                        // than show a clean state the next launch will contradict.
                        Err(e) => {
                            self.app_rules = AppRules::load();
                            Some(format!(" (but apps.toml did not save: {e})"))
                        }
                    }
                } else {
                    None
                };
                // dangling app rules would fail forever at focus-switch time; say so now.
                let refs = self
                    .app_rules
                    .rules
                    .iter()
                    .filter(|r| r.profile == name)
                    .count();
                let mut msg = format!("deleted '{name}'");
                if refs > 0 {
                    use std::fmt::Write as _;
                    let _ = write!(msg, " · {refs} app rule(s) still point at it");
                }
                if let Some(note) = cleared_fallback {
                    use std::fmt::Write as _;
                    let _ = write!(msg, " · it was the fallback, now stay put{note}");
                }
                msg
            }
            Err(e) => format!("delete failed: {e}"),
        }
    }

    /// Rename a profile, carrying its binds sidecar and every app rule that pointed at the old
    /// name. Without the rule retarget a rename would silently break auto-switch — the exact
    /// failure that made "capture under a new name, delete the old one" the wrong workaround.
    pub fn rename_profile(&mut self, from: &str, to: &str) -> String {
        let to = to.trim();
        if to.is_empty() {
            return "name required".into();
        }
        if from == to {
            return format!("'{from}' already has that name");
        }
        // Decided BEFORE the rename, because `Profile::rename` moves the process-wide cursor itself
        // — asking afterwards whether the cursor still says `from` always answers no, so the app's
        // display copy would never be updated (and a stale copy would never be healed).
        let was_active = Profile::file_key(&self.active_profile) == Profile::file_key(from)
            || Profile::file_key(&neuron::profile::active()) == Profile::file_key(from);
        match Profile::rename(from, to) {
            Ok(landed) => {
                // Synced FIRST, before anything that can fail, so the early-return path below can't
                // leave the header naming a profile that no longer exists under that name. (Core
                // already moved the process cursor; this is the app's copy of it.)
                if was_active {
                    self.active_profile.clone_from(&landed);
                    neuron::profile::set_active(&landed);
                }
                let mut retargeted = 0;
                for r in self.app_rules.rules.iter_mut().filter(|r| r.profile == from) {
                    r.profile.clone_from(&landed);
                    retargeted += 1;
                }
                if self.app_rules.default.as_deref() == Some(from) {
                    self.app_rules.default = Some(landed.clone());
                    retargeted += 1;
                }
                // ONE write for both edits, and its failure is REPORTED. Discarding it reported a
                // clean rename while apps.toml on disk still named the old profile — correct-looking
                // until the next launch, when auto-switch silently stopped working. Nothing here can
                // roll the rename back safely, so the honest outcome is to say what didn't persist.
                if retargeted > 0 {
                    if let Err(e) = self.save_app_rules() {
                        // Put the in-memory rules BACK. The panel refreshes from this copy, so
                        // leaving the retarget applied would show routes pointing at the new name
                        // while the disk — and therefore the live dispatcher — still held the old:
                        // the UI quietly disagreeing with what actually routes.
                        self.app_rules = AppRules::load();
                        self.reload_profiles();
                        crate::dispatch::request_reload();
                        return format!(
                            "renamed '{from}' to '{landed}', but apps.toml did not save ({e}) \
                             · its auto-switch routes still name '{from}' and now dangle"
                        );
                    }
                }
                self.reload_profiles();
                // the binds sidecar moved with it, so the live spine must re-read from the new stem.
                crate::dispatch::request_reload();
                match retargeted {
                    0 => format!("renamed '{from}' to '{landed}'"),
                    n => format!("renamed '{from}' to '{landed}' · {n} app rule(s) followed"),
                }
            }
            Err(e) => format!("rename failed: {e}"),
        }
    }

    /// Set (or clear, with an empty name) the profile auto-switch falls back to when the focused
    /// app matches no rule.
    pub fn set_default_profile(&mut self, name: &str) -> String {
        let name = name.trim();
        if name.is_empty() {
            self.app_rules.default = None;
            // Report the write, and on failure put memory BACK so the panel shows what actually
            // routes. Discarding the result cleared the fallback in memory, said so, and left the
            // old one on disk to come back at the next launch — a setting that un-sets itself,
            // with the UI insisting otherwise in the meantime.
            let msg = match self.save_app_rules() {
                Ok(()) => "no fallback · the active profile stays put".to_string(),
                Err(e) => {
                    self.app_rules = AppRules::load();
                    format!("could not clear the fallback ({e}) · it is unchanged")
                }
            };
            crate::dispatch::request_reload();
            return msg;
        }
        if !self.profiles.iter().any(|p| p.name == name) {
            return format!("no profile '{name}'");
        }
        let previous = self.app_rules.default.clone();
        self.app_rules.default = Some(name.to_string());
        let msg = match self.save_app_rules() {
            Ok(()) => format!("fallback profile · {name}"),
            // Same rule as the clear branch: memory goes back to what disk holds, so the picker
            // can't show a selection that a reload or the next launch would silently revert.
            Err(e) => {
                self.app_rules.default = previous;
                format!("could not set the fallback ({e}) · it is unchanged")
            }
        };
        crate::dispatch::request_reload();
        msg
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
        self.app_rules.save()
    }

    // ── backup ───────────────────────────────────────────────────────────

    /// Snapshot ONE physical unit's getter space, addressed by its `path_instance` (the row's
    /// unit id) — so backing up one of two identical devices snapshots the one you clicked.
    ///
    /// PLANE decision (DIALECT-RND): backup resolves the unit's FIRST resolvable plane (the first
    /// pipe `find_for_pipe` claims), NOT a selection-precise (unit, dialect) plane — deliberately.
    /// Backup targets the ROW's unit, not the current SELECTION, and today every unit is N=1 (one
    /// plane) so "first plane" IS the only plane and the snapshot is exact. A future multi-family
    /// unit snapshotting its first plane is acceptable-and-documented: a plane-specific backup can
    /// arrive with real multi-plane hardware (it would need the dialect threaded through the State
    /// callback + panels/device.slint callsite — out of this migration's edit scope). The sweep
    /// reads the raw getter space regardless of family, so it never MIS-reads the wrong plane.
    pub fn backup(&self, unit: &str) -> String {
        let infos = match transport::enumerate() {
            Ok(v) => v,
            Err(e) => return format!("enumerate failed: {e}"),
        };
        for i in &infos {
            if i.instance() != unit {
                continue;
            }
            // find_for_pipe: only the def that DRIVES this collection as its control pipe (family-
            // aware) is the right one to sweep — a non-control sibling of the right unit resolves to
            // None here and we keep looking, same as the old matches_control skip.
            if let Some(def) = self.registry.find_for_pipe(i).cloned() {
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
        match self.selected_def().and_then(|d| d.lighting) {
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
        //    interpreter on the very first call. "skip" honestly when no python is available.
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
            // find_for_pipe: only a battery-bearing def that DRIVES this pipe (family-aware control
            // rule) is a vitals source — a two-family unit routes battery from the framing family.
            reg.find_for_pipe(i)
                .is_some_and(|def| def.commands.contains_key("battery_level"))
        })
        .min_by_key(|i| i.instance());
    {
        let Some(i) = source else { return };
        let Some(def) = reg.find_for_pipe(i) else { return };
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

/// The advanced-FEEL live readouts a device switch needs: idle timeout, DPI stage table, LOD.
/// Read as ONE batched sweep over a single opened handle (see [`read_perf_snapshot`]).
#[derive(Default)]
pub struct PerfSnapshot {
    pub idle_secs: Option<u16>,
    pub dpi_stages: Vec<u16>,
    /// The device's ACTIVE stage index (0-based) from the same stage-table reply — so the editor
    /// can seed the picked stage from hardware truth, not a hardcoded "stage 1".
    pub dpi_active: Option<u8>,
    pub lod_async: Option<(u8, u8)>,
    pub lod_level: Option<u8>,
    /// The keyboard's FIRMWARE game mode (the FN+F10 Win-key kill), best-effort. `None` on a device
    /// with no `game_mode` getter (every mouse) or an unreadable/asleep board — the KEY GUARD card's
    /// firmware sibling row seeds its toggle from this.
    pub game_mode: Option<bool>,
}

/// Batched advanced-FEEL read sweep for one physical unit — a FREE function so a worker thread
/// can run it (the UI's `AppRuntime` is `Rc`-held and UI-thread-only; threads open their own
/// handles, the same pattern the lighting streams use). ONE enumeration + ONE open serve every
/// getter — the old per-getter `open_selected()` paid enumerate+open FOUR times, which is what
/// made clicking a capable device visibly hitch. The host writer is parked for the sweep
/// (`io_gate`), same as every other getter path.
pub fn read_perf_snapshot(pid: u16, unit: &str, dialect: &str) -> PerfSnapshot {
    let mut snap = PerfSnapshot::default();
    let Ok(reg) = Registry::load() else {
        return snap;
    };
    let Ok(infos) = transport::enumerate() else {
        return snap;
    };
    let _gate = crate::host::io_gate(pid);
    // The one plane resolver — the selected (pid, unit, dialect) plane. With the plane now
    // selection-PRECISE, the best-effort field reads below are CORRECT as-is: the resolved plane
    // either exposes each getter or it honestly doesn't. Deliberately NO cross-plane fallback sweep
    // — the review's "the sweep stops at the first pipe" dissolves because the pipe is no longer
    // ARBITRARY (it's the plane the user picked), so reading only that plane's getters is right.
    let Some((def, i)) = resolve_plane(&reg, &infos, pid, unit, dialect) else {
        return snap;
    };
    let Ok(d) = Device::open_path(def.clone(), i.pid, &i.path) else {
        return snap;
    };
    snap.idle_secs = cap::idle_timeout_secs(&d).ok();
    // The ACTIVE getter (0x04/0x86) is the user's REAL cycle; the slot-table fallback
    // (0x04/0x83) covers boards without 0x86 — seeding the editor from the slot table is how
    // a stray apply CORRUPTED the user's onboard cycle with factory stages (live incident
    // 2026-07-07). Bind the reply ONCE — both the stage list and the active index decode from
    // the same buffer (identical layout for either getter).
    if let Ok(s) = d.run("dpi_stages_active").or_else(|_| d.run("dpi_stages")) {
        snap.dpi_stages = neuron::writes::decode_dpi_stages(&s);
        snap.dpi_active = neuron::writes::decode_dpi_active(&s);
    }
    // Asymmetric LOD first (device reports split mode); else symmetric level.
    snap.lod_async = neuron::writes::lift_off_async(&d);
    if snap.lod_async.is_none() {
        snap.lod_level = neuron::writes::lift_off_distance(&d).ok().map(|l| l.min(2));
    }
    // FIRMWARE GAME MODE (the Win-key kill) — same one-open sweep, best-effort. A device with
    // no game_mode command (every mouse) errors before any I/O, so `.ok()` = None for free.
    snap.game_mode = neuron::capability::game_mode(&d).ok();
    snap
}

/// Resolve the SELECTED control plane among enumerated pipes: the pipe of `unit` (pid-healed
/// when the unit left) whose resolving def speaks `dialect`. Empty dialect = first resolvable
/// plane (pre-selection / stateless callers). The one resolution rule for `open_selected`,
/// `selected_def`, and the perf snapshot — first-family-wins on a multi-plane unit was the
/// review-caught identity gap. Two passes like `open_selected`'s old shape: pass 0 exact unit,
/// pass 1 pid-only healing; within a pass, `find_for_pipe` gives the family-aware control def and
/// we accept iff the dialect gate passes. Today N=1 (one plane per unit) so pass 0 finds the exact
/// pipe and the list is byte-identical; the dialect gate only ever excludes a SECOND family's pipe
/// on a future multi-family unit.
fn resolve_plane<'a>(
    reg: &'a Registry,
    infos: &'a [transport::HidDeviceInfo],
    pid: u16,
    unit: &str,
    dialect: &str,
) -> Option<(&'a DeviceDef, &'a transport::HidDeviceInfo)> {
    for relaxed in [false, true] {
        for i in infos {
            // find_for_pipe: the def that DRIVES this control pipe (family-aware), so a two-family
            // unit resolves each pipe under the family that can frame it.
            let Some(def) = reg.find_for_pipe(i) else {
                continue;
            };
            if (pid != 0 && i.pid != pid)
                || !(relaxed || unit.is_empty() || i.instance() == unit)
            {
                continue;
            }
            // The dialect leg of the plane identity: empty = first resolvable plane (stateless /
            // pre-selection); otherwise only the pipe whose def speaks the picked family qualifies.
            if !(dialect.is_empty() || def.dialect == dialect) {
                continue;
            }
            return Some((def, i));
        }
    }
    None
}

/// The transient row for a device whose adoption probe is in flight: honest name (the USB
/// product string when the device offers one), "learning device…" as its mode, every control
/// gated off. No device I/O — the probe thread owns the pipe.
fn learning_row(i: &transport::HidDeviceInfo, family: &str, instance: String) -> DeviceState {
    // Name fallback follows the CLAIMING dialect (Finding 3): the call site already resolved which
    // family claims this pipe, so it threads the id in rather than this row re-deriving "razer".
    let name = if i.product.trim().is_empty() {
        format!("{family} device {:04x}", i.pid)
    } else {
        i.product.trim().to_string()
    };
    DeviceState {
        name,
        codename: "learning".into(),
        pid: i.pid,
        instance,
        // A learning row's plane family = the CLAIMING dialect: when the probe lands and the real
        // registry-backed row flips in (same unit id AND same dialect — the def is tagged with this
        // very family), the selection carries straight over because the plane identity is unchanged.
        dialect: family.to_string(),
        mode: "learning device…".into(),
        connected: true,
        firmware: "—".into(),
        dpi: "—".into(),
        polling: "—".into(),
        brightness: "—".into(),
        battery: String::new(),
        charging: false,
        storage: String::new(),
        icon: "device",
        dpi_n: None,
        polling_n: None,
        brightness_n: None,
        battery_frac: None,
        cap_dpi: false,
        cap_poll: false,
        cap_light: false,
        cap_bright: false,
        cap_bright_set: false,
        cap_scroll: false,
        cap_store: false,
        cap_idle: false,
        cap_plate: false,
        cap_game_mode: false,
        adopting: true,
    }
}

/// A non-operable PLACEHOLDER row for the failed-adoption surface — the UNRESPONSIVE (strike
/// ledger) and NO-PROTOCOL (unclaimed ledger) rows. No device I/O and no registry def: every
/// capability gate off, `connected = false` (it can't be driven), `adopting = true` (so the glue
/// selection policy treats it exactly like a learning row — never auto-picked, selectable-but-inert
/// — with zero new cases). `mode` carries the honest one-line reason; `instance` is a synthetic id
/// so it can't collide with a real unit's `path_instance`. It keeps the pipe's REAL pid (like a
/// learning row): a pid with no registry def makes `open_selected` bail honestly if the row is ever
/// manually selected — a pid of 0 would instead trip the "any device" branch and open a real one.
fn placeholder_row(
    name: String,
    pid: u16,
    instance: String,
    mode: String,
    dialect: String,
) -> DeviceState {
    DeviceState {
        name,
        codename: "unrecognized".into(),
        pid,
        instance,
        // The claiming family, carried even on an inert row so its selection identity is complete.
        dialect,
        mode,
        connected: false,
        firmware: "—".into(),
        dpi: "—".into(),
        polling: "—".into(),
        brightness: "—".into(),
        battery: String::new(),
        charging: false,
        storage: String::new(),
        icon: "device",
        dpi_n: None,
        polling_n: None,
        brightness_n: None,
        battery_frac: None,
        cap_dpi: false,
        cap_poll: false,
        cap_light: false,
        cap_bright: false,
        cap_bright_set: false,
        cap_scroll: false,
        cap_store: false,
        cap_idle: false,
        cap_plate: false,
        cap_game_mode: false,
        adopting: true,
    }
}

/// The registry-driven unit-resolution + per-unit read loop — the SLOW half of a scan (opens
/// every resolved unit via `read_device_state`, which can block on a sleepy wireless device).
/// Takes `registry` by reference rather than `&AppRuntime` so it has no `AppRuntime` borrow at
/// all: shared by the synchronous `scan_devices` (passing `&self.registry`) and the free-standing
/// `scan_hardware` (which loads its own, since a spawned thread can't borrow `AppRuntime` — see
/// its doc for why).
fn scan_units(registry: &Registry, infos: &[transport::HidDeviceInfo]) -> Vec<DeviceState> {
    // Pass 1 — resolve units before any device I/O: dedupe collections to units, and learn
    // which pids have duplicate units so naming + vitals routing can be decided up front.
    struct Unit {
        def: DeviceDef,
        pid: u16,
        path: transport::DevicePath,
        instance: String,
    }
    let mut units: Vec<Unit> = Vec::new();
    for i in infos {
        // find_for_pipe: the def that DRIVES this pipe (family-aware control-pipe rule), so a
        // two-family unit resolves each control pipe to the family that can frame it.
        let Some(def) = registry.find_for_pipe(i) else {
            continue;
        };
        // DATA-ONLY defs (an `[events]` vocabulary with no commands/lighting — the Seiren) never
        // grow a row: nothing here is operable, and the device's user-facing face is its
        // Core-Audio endpoint row. A second knob-less HID row would double-list the hardware.
        if !def.is_operable() {
            continue;
        }
        let instance = i.instance();
        // One row per PLANE (unit × family), not per unit: the dedupe key is (instance, dialect)
        // because a multi-family unit carries one control pipe per family and each is its own
        // channel. Today every unit is N=1 (one plane), so the (instance, dialect) key collapses
        // to exactly the old per-instance list — identical rows — and only a future two-family
        // unit splits into two rows here.
        if units
            .iter()
            .any(|u| u.instance == instance && u.def.dialect == def.dialect)
        {
            continue; // another collection of the SAME physical plane (unit + family)
        }
        units.push(Unit {
            def: def.clone(),
            pid: i.pid,
            path: i.path.clone(),
            instance,
        });
    }
    let mut out = Vec::new();
    for u in &units {
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
        let feed_vitals = twins.iter().min().copied() == Some(u.instance.as_str());
        let mut st = read_device_state(&u.def, u.pid, &u.path, feed_vitals);
        st.instance.clone_from(&u.instance);
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
    out
}

/// The BACKGROUND half of a device scan — free-standing (not a method): the UI's `AppRuntime` is
/// `!Send` (it holds a raw registry + non-thread-safe caches), so a worker thread can never borrow
/// `self`. Mirrors `read_perf_snapshot`'s worker (glue.rs's `seed_perf_async`) — it loads its OWN
/// `Registry` fresh from disk instead of touching the UI's cached copy, does the real HID I/O
/// (`transport::enumerate()` + opening every resolved unit — the part that can block for hundreds
/// of ms on a sleepy wireless device), and hands back plain data. `None` on an enumerate failure,
/// mirroring `scan_devices`'s own early return (no partial/stale bookkeeping on a failed scan).
///
/// Running this off the UI thread is the whole fix for the `ADOPT_WATCH_TIMER` stall (glue.rs):
/// that 1s timer used to call `scan_devices` — this exact I/O — synchronously on the UI thread
/// once a second for as long as anything was unadopted, freezing the window in a way the
/// organ-stall watchdog can't see (the UI tick itself is the stall). The caller spawns a thread
/// that calls this, then folds the result in via `AppRuntime::finish_background_scan` once it
/// hops back to the UI thread (`slint::invoke_from_event_loop`).
pub fn scan_hardware() -> Option<(Vec<transport::HidDeviceInfo>, Vec<DeviceState>)> {
    let registry = Registry::load().unwrap_or(Registry {
        devices: Vec::new(),
    });
    let infos = transport::enumerate().ok()?;
    let out = scan_units(&registry, &infos);
    Some((infos, out))
}

fn read_device_state(
    def: &DeviceDef,
    pid: u16,
    path: &transport::DevicePath,
    feed_vitals: bool,
) -> DeviceState {
    let mode = def
        .mode_for(pid).map_or_else(|| "?".into(), |m| m.name.clone());
    let icon = icon_for(def);
    let mut st = DeviceState {
        name: def.name.clone(),
        codename: def.codename.clone(),
        pid,
        instance: String::new(), // the caller stamps the unit id it resolved
        // The plane's family, straight off the resolving def — this row IS (unit, def.dialect),
        // one channel of a possibly multi-family unit.
        dialect: def.dialect.clone(),
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
        cap_bright_set: def.supports(neuron::registry::Capability::SetBrightness),
        cap_scroll: def.supports(neuron::registry::Capability::SetScrollStage),
        cap_store: def.supports(neuron::registry::Capability::Storage),
        cap_idle: def.supports(neuron::registry::Capability::Battery),
        cap_plate: def.has_side_plates(),
        cap_game_mode: def.supports(neuron::registry::Capability::SetGameMode),
        adopting: false,
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
            st.battery_frac = Some((f32::from(b) / 100.0).clamp(0.0, 1.0));
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
                    let mut raw = String::new();
                    for b in &a {
                        let _ = write!(raw, "{b:02x}");
                    }
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
            .map_or(0, |d| d.as_secs()),
        interfaces: vec![IfaceSnap {
            usage_page: ci.usage_page,
            usage: ci.usage,
            getters,
        }],
    };
    let dir = neuron::runroot::run_root().join("backups");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(snap.filename());
    match std::fs::write(&path, snap.to_json()) {
        Ok(()) => format!(
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

    /// The stale-rows race (review finding, 2026-07-09): an adoption finishing while a background
    /// scan is in flight sets `synth_dirty` AFTER the worker resolved its rows against the old
    /// registry. `finish_background_scan` must SAY SO (stale = true) so its caller re-kicks one
    /// more scan — the adopt-watch timer can't be relied on, because the successful adoption is
    /// exactly what clears `adoption_pending()` and stops it. Pins: dirty-at-completion → stale
    /// true; clean completion → stale false (no infinite re-kick loop).
    #[test]
    fn background_scan_reports_stale_rows_when_an_adoption_landed_mid_flight() {
        let mut rt = AppRuntime::load();
        // adoption lands mid-flight: the worker has computed rows, then this flag flips.
        rt.synth_dirty.store(true, Ordering::SeqCst);
        let (_, stale) = rt.finish_background_scan(Vec::new(), Vec::new());
        assert!(
            stale,
            "a dirty registry at completion means the rows predate the adoption — caller must rescan"
        );
        // …and the reload consumed the dirty flag: the re-kicked scan completes clean.
        let (_, stale) = rt.finish_background_scan(Vec::new(), Vec::new());
        assert!(!stale, "a clean completion must not re-kick (convergence)");
    }

    /// The runtime loads from disk (registry + config) without a device present.
    #[test]
    fn runtime_loads_headless() {
        let _cwd = crate::testsupport::cwd_guard("runtime_loads_headless");
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
        // `Profile::path` resolves via the run root (`NEURON_RUN_DIR`), and the editor/prefs/apptest
        // tests swap that process-global override. Take the shared guard so this save+reload is
        // isolated in its own temp dir and can't race an override swap out from under it.
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
        assert!(msg.contains("needs a name"), "got: {msg}");
        // the same door refuses the names that would collide with the app's own binds file or
        // that the filesystem would reject — one check, not a per-call-site guard.
        assert!(
            rt.save_profile_from_devices("gui", 800, 1000, 50, vec![])
                .contains("the binds you author"),
        );
        assert!(rt
            .save_profile_from_devices("NUL", 800, 1000, 50, vec![])
            .contains("windows won't let"));
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
        // The pause gate is PROCESS-GLOBAL, so flipping it here is visible to every other test
        // running at the same time — and anything behind the gate (a profile switch, say) silently
        // does nothing while this test holds it. Take the binary's serialization lock so the window
        // can't overlap someone else's. Found the hard way: this raced the auto-switch dispatch
        // test into an intermittent failure that looked like a routing bug.
        let _cwd = crate::testsupport::cwd_guard("runtime_paused_writes");
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

    /// A rename is one operation across FOUR persisted things: the profile TOML, its binds
    /// sidecar, every auto-switch route naming it, and the active cursor. Each used to be a
    /// separate place someone could forget — the old workaround for "rename" was capture-new +
    /// delete-old, which silently orphaned the routes.
    #[test]
    fn rename_carries_binds_routes_the_fallback_and_the_cursor() {
        let _cwd = crate::testsupport::cwd_guard("runtime_rename_carries");
        let mut rt = AppRuntime::load();
        let from = format!("__neuron_ren_{}", std::process::id());
        let to = format!("{from}_new");
        rt.save_profile_from_devices(&from, 800, 1000, 50, vec![]);
        std::fs::write(Profile::rules_path(&from), b"rules = []\n").unwrap();
        rt.app_rules.rules.push(AppRule {
            app: "game".into(),
            profile: from.clone(),
        });
        rt.app_rules.default = Some(from.clone());
        rt.app_rules.save().unwrap();
        rt.active_profile = from.clone();
        neuron::profile::set_active(&from);

        let msg = rt.rename_profile(&from, &to);
        assert!(msg.contains("renamed"), "unexpected: {msg}");
        assert!(Profile::path(&to).exists(), "the profile moved");
        assert!(Profile::rules_path(&to).exists(), "its binds moved with it");
        assert!(!Profile::rules_path(&from).exists(), "and left nothing behind");
        assert_eq!(rt.app_rules.rules[0].profile, to, "the route followed");
        assert_eq!(rt.app_rules.default.as_deref(), Some(to.as_str()), "so did the fallback");
        assert_eq!(rt.active_profile, to, "and the header");
        assert_eq!(neuron::profile::active(), to, "and the process cursor");
        // …and it all survives a restart, because the write actually happened.
        assert_eq!(
            AppRules::load().rules[0].profile,
            to,
            "apps.toml on disk carries the new name"
        );

        neuron::profile::set_active("");
        let _ = Profile::delete(&to);
    }

    /// Deleting the active profile must take its HOST-side key suppression with it. Live-verified
    /// as a bug first: delete a gaming profile and the Key Guard stayed lit, Alt+Tab stayed
    /// swallowed, and the panel's own caption ("it lifts when the profile changes") was contradicted
    /// by the header two inches above it reading NO PROFILE.
    #[test]
    fn deleting_the_active_gaming_profile_lifts_its_key_guard() {
        let _cwd = crate::testsupport::cwd_guard("runtime_delete_gaming");
        let mut rt = AppRuntime::load();
        let name = format!("__neuron_gam_{}", std::process::id());
        rt.gaming_mode = neuron::writes::GamingMode::from_profile(true, true, false, false);
        rt.save_profile_from_devices(&name, 800, 1000, 50, vec![]);
        rt.active_profile = name.clone();
        neuron::profile::set_active(&name);
        assert!(Profile::load(&name).unwrap().has_gaming(), "the profile captured its guards");

        rt.delete_profile(&name);
        assert_eq!(rt.active_profile, "—");
        assert!(
            !rt.gaming_mode.any(),
            "no profile is active, so nothing should still be suppressing chords"
        );
        assert!(!neuron::hook::policy().any(), "and the shared policy carrier agrees");
        neuron::profile::set_active("");
    }

    /// The fallback is a setting that must not un-set itself: clearing it when the profile it names
    /// is deleted has to reach DISK, or unmatched focus resumes switching to a gone profile after
    /// a restart.
    #[test]
    fn deleting_the_fallback_profile_clears_it_on_disk() {
        let _cwd = crate::testsupport::cwd_guard("runtime_delete_fallback");
        let mut rt = AppRuntime::load();
        let name = format!("__neuron_fb_{}", std::process::id());
        rt.save_profile_from_devices(&name, 800, 1000, 50, vec![]);
        assert!(rt.set_default_profile(&name).contains("fallback"));
        assert_eq!(AppRules::load().default.as_deref(), Some(name.as_str()));

        let msg = rt.delete_profile(&name);
        assert!(msg.contains("fallback"), "the message says what changed: {msg}");
        assert_eq!(rt.app_rules.default, None, "cleared live");
        assert_eq!(AppRules::load().default, None, "and on disk");
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
