//! Device-event listener — the channel Synapse reads for its instant OSD. A Razer wireless mouse
//! PUSHES a HID input report when its ONBOARD buttons change state (DPI button → "DPI is now X",
//! scroll-stage button → "stage N", plug/unplug → a `05 0c` power poke). Our transport only ever did
//! feature get/set (pull), so nothing in Neuron heard these — this listens and turns them into
//! confirmations, event-driven, no polling.
//!
//! Report format (captured live from a Naga V2 Pro on its readable `up=0x0001 u=0x0000` collection):
//!   `05 02 <dpiX:u16be> <dpiY:u16be> …` → DPI changed
//!   `05 3a <stage:u8> …`                → scroll/sensitivity stage changed
//!   `05 0c 00 …`                        → power/charge state changed (STATELESS poke; same bytes for
//!                                          plug and unplug — "read me"). On it we SETTLE the charge
//!                                          state (poll until it latches) then feed `vitals`.
//!   `05 0e <plate_id:u8> …`             → a swappable SIDE PLATE was attached/detached. `plate_id`
//!                                          is a hardware strap-code (0=detached); resolved to a label
//!                                          via the registry `[side_plates]` map, de-dup'd on change.
//!   `05 11 <state:u8> …`                → an AUDIO device's capacitive TAP-MUTE toggled (Razer Seiren
//!                                          V3 Mini family, pid 0x056a; captured live 2026-07-08).
//!                                          `state` 0=live/green, 1=muted/red. This vocabulary is now
//!                                          FAMILY knowledge on the `razer-audio` dialect
//!                                          (`Dialect::default_event_for`), not a hardcoded arm here —
//!                                          a def's own `[events]` table (`DeviceDef::event_for`)
//!                                          still OVERRIDES it per device when one is present. The
//!                                          firmware owns the mute+LED self-contained; we bridge it to
//!                                          the OS capture mute so the tap drives the UI pill + apps
//!                                          (the role a Synapse install used to play). See
//!                                          `bridge_mic_mute`.
//! De-dup for the DPI/scroll echoes lives in `confirm`; the battery/charge edge logic + throttle live
//! in `neuron::vitals`. A hotplug MONITOR re-arms collections after a dongle replug. The 04/05 MOUSE
//! arms above (DPI/scroll/power/plate) remain a hardcoded vocabulary pinned to the Naga V2 Pro family
//! — only the `05 11` mute event has moved to the registry `[events]` map so far; the rest move the
//! same way as their defs grow `[events]` entries.
//!
//! DRIVER-MODE BUTTON EVENTS — a SECOND report family, the `04` lead byte, captured live off the Naga
//! V2 Pro (pid 0x00A8) 2026-07-07. In driver mode (device_mode 0x03) the firmware STOPS acting on its
//! own onboard DPI/scroll/profile buttons; it DEFERS them to the resident software as bare "the button
//! happened" events (Synapse silently implements the semantics). While neuron holds the driver lease
//! for its lighting stream that duty is OURS or those buttons go dead:
//!   `04 52 …` → DPI-stage button      `04 57 …` → scroll-sensitivity button
//!   `04 50 …` → profile button        `04 00 …` → ANY button's release (buf[1]=0x00 — ignored)
//! The event carries NO stage index and NO direction — software owns the cycle. `decode` maps the code
//! to an [`Intent`] and hands it to a single SERIAL worker that — gated on a FRESH device_mode==0x03
//! read — fulfills it through `neuron::intent::run_shared_intent`, the same cycle policy the CLI/GUI
//! use. This is neuron honouring the driver-mode custody contract: hold the lease, own the buttons.
//!
//! `NEURON_HIDWATCH=1` additionally logs every raw report as hex (for decoding new devices) and the
//! (otherwise-silent) battery-read failures.

use neuron::registry::EventKind;
use neuron::transport::DevicePath;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Settle window for the device-push BATCHER. Every pushed settings-report (dpi / scroll / side plate)
/// joins a batch and waits this much quiet before the batch is decided. Two jobs in one window:
///   • absorbs a side plate's seating BOUNCE (the strap flickers `00`↔`01` before it settles) — the
///     last value per kind wins, so `confirm` only ever sees the settled code; and
///   • collects the wake/reconnect burst (the device re-announces dpi+scroll+plate together) so the
///     burst can be recognized and silenced as a STATE SYNC rather than carded as user actions.
/// ~220ms is comfortably longer than any observed bounce / wake burst yet short enough that a real
/// change's card still feels immediate.
const BATCH_SETTLE: Duration = Duration::from_millis(220);
/// The "a human physically couldn't" threshold. A person cannot change two DISTINCT states within this
/// span — DPI and scroll are separate buttons, and a plate swap is a multi-second physical act — but
/// the firmware emits its whole wake-announce within a few ms. So a batch holding ≥2 DISTINCT kinds
/// whose FIRST reports land inside this span is unambiguously a device sync, never user input → learn
/// it silently. Kept well under the ~250ms+ a genuine two-button sequence takes, so it can NEVER
/// false-trigger on real actions; a lone change (one kind) always cards.
const BURST_SPAN: Duration = Duration::from_millis(150);
/// Scroll-stage track length — the canonical source is `intent::SCROLL_STAGE_COUNT` (no magic dupe).
const SCROLL_STAGE_MAX: u32 = neuron::intent::SCROLL_STAGE_COUNT as u32;
/// How often the hotplug monitor re-enumerates to catch a dongle replug / hub glitch / sleep-wake.
const HOTPLUG_POLL: Duration = Duration::from_secs(20);

/// Verbose discovery logging (`NEURON_HIDWATCH=1`), read once and cached.
fn verbose() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var("NEURON_HIDWATCH").ok().as_deref() == Some("1"))
}

/// The device registry, loaded ONCE and cached. Battery reads used to re-parse the device TOMLs from
/// disk on every poke + throttled sample; the registry is immutable at runtime, so this kills all that
/// hot-path I/O.
/// The app-layer registry — the ONE shared, RELOADABLE cache both `hidwatch` (arming, battery) and
/// `glue` (endpoint capability gating) read, so both track the runtime's normal reload path instead
/// of freezing a startup snapshot until restart (the reload-mismatch a review caught). It returns
/// `&'static` on purpose: `event_def` is threaded into background reader threads and held for the
/// reader's life, so it must outlive any borrow. To stay `&'static` AND reloadable, [`reload_registry`]
/// LEAKS a fresh `Registry` and swaps the pointer — old readers keep their still-valid leaked snapshot,
/// new arms see the new one. The leak is bounded by adoption events (rare — a few per session at most),
/// not a per-frame cost, which is the deliberate trade for keeping the zero-alloc `&'static` reader path.
pub fn registry() -> Option<&'static neuron::registry::Registry> {
    let mut cell = reg_cell().lock().unwrap_or_else(|p| p.into_inner());
    if cell.is_none() {
        *cell = neuron::registry::Registry::load()
            .ok()
            .map(|r| &*Box::leak(Box::new(r)));
    }
    *cell
}

/// Re-read the registry from disk and swap it in — called from the runtime's `synth_dirty` reload
/// path (an adoption wrote a new `devices/auto/*.toml`) so hidwatch arming + glue capability gating
/// see the newly-adopted/edited defs on the next re-arm/select, not only after a restart.
pub fn reload_registry() {
    if let Ok(r) = neuron::registry::Registry::load() {
        *reg_cell().lock().unwrap_or_else(|p| p.into_inner()) = Some(&*Box::leak(Box::new(r)));
    }
}

fn reg_cell() -> &'static Mutex<Option<&'static neuron::registry::Registry>> {
    static R: OnceLock<Mutex<Option<&'static neuron::registry::Registry>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(None))
}

/// Arm the listener: a reader thread per readable, event-carrying collection of each connected Razer
/// DPI-mouse, plus a hotplug monitor that re-arms after replug. Never fails the app.
pub fn start() {
    debug_assert!(
        crate::glue::ui_installed(),
        "startup-order contract: glue::install_ui must run before hidwatch::start — early mute events would silently drop (main.rs wiring)"
    );
    registry(); // pre-warm (surface a load error early; cache for the hot path)
    let mouse_pids: HashSet<u16> = match registry() {
        Some(r) => r
            .devices
            .iter()
            .filter(|d| d.supports(neuron::registry::Capability::Dpi))
            .flat_map(|d| d.product_ids())
            .collect(),
        None => {
            if verbose() {
                eprintln!("[hidwatch] registry load failed; not listening");
            }
            return;
        }
    };

    // Armed collection paths — shared so the monitor and the reader threads (which remove their own
    // path on exit) agree on what's live, so a REPLUG re-arms instead of staying deaf.
    let armed: Arc<Mutex<HashSet<DevicePath>>> = Arc::new(Mutex::new(HashSet::new()));
    arm_new(&mouse_pids, &armed);

    let mon = armed.clone();
    crate::worker::spawn_detached("neuron-hidwatch-mon", move || loop {
        thread::sleep(HOTPLUG_POLL);
        arm_new(&mouse_pids, &mon);
    });
}

/// Enumerate and spawn a reader for any event-carrying mouse collection not already armed.
fn arm_new(mouse_pids: &HashSet<u16>, armed: &Arc<Mutex<HashSet<DevicePath>>>) {
    let infos = match neuron::transport::enumerate() {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut spawned = 0usize;
    for info in infos {
        // Three ways a readable collection earns a reader — each carries its OWN vendor/shape gate,
        // so there is no blanket VID filter above them (an earlier `vid != RAZER_VID continue` made
        // the two family-general paths a lie: a non-razer family's event pipe would be dropped before
        // it was ever checked). Now the emergence in the doc matches the code — a pipe arms purely
        // from what claims it:
        //   • a DPI-mouse's sibling generic-desktop collection (undefined usage `u=0x0000`) or a
        //     vendor page — the 04/05 button/settings reports; gated to the Razer DPI-mouse pid set
        //     (`mouse_pids`), which is itself Razer-only, so this path stays Razer by construction;
        //   • ANY registry def's declared event-pipe (`DeviceDef::event_pipe_matches`) — the PER-
        //     DEVICE override from `[events]` DATA; vendor-matched (`vendor_id == info.vid`) so a pid
        //     collision across vendors can't mis-arm;
        //   • ANY dialect's FAMILY-wide push vocabulary (`neuron::dialect::event_dialect_for`) — the
        //     dialect's own `claims` carries its vendor/shape signature (razer-audio pins Razer VID +
        //     000c/0001/64B), so a future non-Razer family with an events pipe arms here too, from
        //     SHAPE alone, with no def anywhere.
        // Skip the OS-protected mouse collection + plain media keys (0-length feature report).
        let is_mouse_event = mouse_pids.contains(&info.pid)
            && ((info.usage_page == 0x0001 && info.usage == 0x0000) || info.usage_page >= 0xFF00);
        let event_def = registry().and_then(|r| {
            r.devices.iter().find(|d| {
                d.vendor_id == info.vid
                    && d.product_ids().any(|p| p == info.pid)
                    && d.event_pipe_matches(info.usage_page, info.usage, info.feature_len)
            })
        });
        let event_dialect = neuron::dialect::event_dialect_for(&info);
        if !(is_mouse_event || event_def.is_some() || event_dialect.is_some()) {
            continue;
        }
        // claim this collection (HashSet::insert is false if already armed → skip)
        if !armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(info.path.clone()) {
            continue;
        }
        spawn_reader(info.pid, info.path.clone(), armed.clone(), event_def, event_dialect, info.product.clone());
        spawned += 1;
    }
    if verbose() && spawned > 0 {
        eprintln!("[hidwatch] armed {spawned} collection(s)");
    }
}

fn spawn_reader(
    pid: u16,
    path: DevicePath,
    armed: Arc<Mutex<HashSet<DevicePath>>>,
    def: Option<&'static neuron::registry::DeviceDef>,
    dialect: Option<&'static dyn neuron::dialect::Dialect>,
    product: String,
) {
    let tag = format!("pid={pid:04x}");
    // Record this collection's product string in the emergent capability surface (see
    // `hardware_mute_products`'s doc) THE MOMENT a dialect-claimed events collection arms — before we
    // know it will ever push anything — so glue's capability check can resolve it as soon as it's
    // live. The insertion RULE (empty never lands; the write facet only on a proven setter) lives in
    // `note_audio_capability`, where tests pin it.
    if let Some(d) = dialect {
        note_audio_capability(d, &product);
    }
    // `armed` claimed this collection's path in `arm_new` before this spawn — the release below is
    // the ONE place that un-claims it (on open failure, on read-loop exit, on a spawn refusal/panic),
    // so the monitor can always retry a stranded claim instead of a collection going deaf forever.
    let release_armed = armed.clone();
    let release_path = path.clone();
    crate::worker::spawn_guarded(
        "neuron-hidwatch",
        move || {
            release_armed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&release_path);
        },
        move || {
            let reader = match neuron::transport::open_reader(&path) {
                Ok(r) => r,
                Err(e) => {
                    if verbose() {
                        eprintln!("[hidwatch] {tag}: not readable ({e})");
                    }
                    return; // release un-claims `path`; the monitor retries later
                }
            };
            if verbose() {
                eprintln!("[hidwatch] LISTENING {tag}");
            }
            // AUTHORITATIVE SEED: a best-effort one-shot read of the current mute state, ONCE per arm,
            // so the UI is correct from launch — not only after the first tap. Opens its own FEATURE
            // transport on the same collection (the reader handle above is INPUT-only); any failure
            // (device asleep, not this family) is silently skipped — this is a seed, not a requirement.
            // Runs on this reader's own thread, before the read loop starts, so it costs no extra thread.
            if let Some(d) = dialect {
                if !product.is_empty() {
                    if let Ok(t) = neuron::transport::open_path(&path) {
                        if let Some(muted) = d.read_audio_mute(t.as_ref()) {
                            bridge_mic_mute(&product, muted);
                        }
                    }
                }
            }
            let mut buf = [0u8; 64];
            loop {
                match neuron::transport::classify_read(reader.read(&mut buf)) {
                    neuron::transport::ReadStep::Data(n) if n > 0 => {
                        if verbose() {
                            eprint!("[hidwatch] {tag} n={n} ");
                            for b in &buf[..n] {
                                eprint!("{b:02x} ");
                            }
                            eprintln!();
                        }
                        decode(&buf[..n], pid, def, dialect, &product);
                        // lazy battery freshness — piggyback on activity (the device is awake; it's
                        // sending reports), throttled, off-thread so a slow open never stalls reads.
                        if neuron::vitals::due(pid, false) {
                            // `due` already advanced the throttle (claimed the read slot). If the
                            // read never runs — thread refused OR the worker panicked — the slot
                            // must be released to the SHORT stale-retry, not left claimed for the
                            // full interval: `done(None)` marks it stale. A completed read reports
                            // its own success/failure inside the worker.
                            crate::worker::spawn_notify(
                                "neuron-hidwatch-batt",
                                move || match read_battery(pid) {
                                    Some((b, c)) => {
                                        neuron::vitals::observe(pid, b, c, false);
                                        true
                                    }
                                    None => {
                                        neuron::vitals::mark_stale(pid);
                                        if verbose() {
                                            eprintln!("[hidwatch] pid={pid:04x}: battery read failed");
                                        }
                                        false
                                    }
                                },
                                move |ran| {
                                    if ran.is_none() {
                                        neuron::vitals::mark_stale(pid);
                                    }
                                },
                            );
                        }
                    }
                    neuron::transport::ReadStep::Data(_) => {} // zero-length read — keep listening
                    neuron::transport::ReadStep::Idle => {} // read timed out — keep listening
                    neuron::transport::ReadStep::Gone => {
                        // unplugged / device gone — release un-claims `path` so the monitor re-arms.
                        if verbose() {
                            eprintln!("[hidwatch] {tag}: closed");
                        }
                        return;
                    }
                }
            }
        },
    );
}

/// Record an arming, dialect-claimed collection's product string on the emergent capability
/// surface(s): the READ facet always, the WRITE facet only when the arming dialect PROVED a setter
/// (`Dialect::audio_mute_writable` — today never: razer-audio is read-only, hidpp/razer have no audio
/// concept; the plumbing is ready for a future writable family with zero call-site changes).
///
/// The insertion INVARIANT, pinned by tests below: an EMPTY (or blank) product is NEVER inserted.
/// These stores hold needles for `neuron::audio::endpoint_matches_product`, whose contract is that an
/// empty needle identifies nothing — the stores must only ever hold strings that can actually resolve
/// an endpoint, so a nameless collection simply doesn't surface a capability.
fn note_audio_capability(dialect: &dyn neuron::dialect::Dialect, product: &str) {
    let product = product.trim();
    if product.is_empty() {
        return;
    }
    mute_products_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(product.to_string());
    if dialect.audio_mute_writable() {
        mute_writable_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(product.to_string());
    }
}

/// The emergent capability surface glue reads: audio-product names discovered from LIVE claiming (a
/// dialect-claimed events collection actually arming), never from any def. Insert-only — a replug
/// re-inserts the same string, harmlessly; nothing is ever removed, so a momentary read race can never
/// see a product vanish mid-session. Lazily created (a `HashSet` can't init a `const` static).
fn mute_products_store() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Snapshot of every USB product string a hardware-mute-pushing collection has armed under, this
/// session. `glue::endpoint_has_hardware_mute` reads this to decide whether an audio endpoint's mute
/// is hardware-owned — discovered from what the dialect layer actually CLAIMED and armed, not from a
/// registry def (a def with its own `[events]` MuteState entry is a SEPARATE, additional check there).
pub fn hardware_mute_products() -> Vec<String> {
    mute_products_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect()
}

/// The WRITE facet's own store, lazily created — mirrors [`mute_products_store`].
fn mute_writable_store() -> &'static Mutex<HashSet<String>> {
    static S: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashSet::new()))
}

/// The emergent WRITE-facet surface: a product lands here ONLY when its arming dialect's
/// `audio_mute_writable()` is true (today: never — see `spawn_reader`; the plumbing is ready for a
/// writable-mute family).
pub fn mute_writable_products() -> Vec<String> {
    mute_writable_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).iter().cloned().collect()
}

/// Bridge an audio device's firmware tap-mute (its pushed `05 11 <state>`-shaped report) to the
/// OS/Core-Audio capture mute, the shared mic-state provider, and the UI — the MuteState handler for
/// [`decode`]. Setting the OS mute lights up the whole chain the resident dispatch loop already
/// watches (Discord/games follow the OS mute the same way Synapse used to drive it), with no extra UI
/// plumbing beyond the two explicit nudges below.
///
/// Off-thread: endpoint resolution + `VolumeCtl::open` do COM enumeration that must never stall this
/// reader loop. Idempotent (setting the mute it already has is harmless), so a duplicate report is a
/// no-op. Cross-platform via the `audio` seam — off Windows endpoint resolution returns `None`, so
/// this compiles and no-ops with zero cfg here.
///
/// Endpoint resolution is BY DEVICE IDENTITY, not a global guess: `find_capture(product)` resolves
/// via the ONE shared identity predicate (`neuron::audio::endpoint_matches_product` — the endpoint's
/// Core-Audio name contains the USB product string, e.g. "Microphone (2- Razer Seiren V3 Mini)"
/// contains "Razer Seiren V3 Mini") — so a second capture device never receives THIS device's mute.
/// `product` is the arming collection's OWN USB product string (not a def's `name`, which may not
/// exist at all for a dialect-only arm). There is deliberately NO fallback to a generic
/// `resolve_capture(None)` guess: an unresolvable event is not bridged (see below).
///
/// ORDER MATTERS: (1) the OS mute FIRST, so any poller that races this reads the already-converged
/// state; (2) `mic_state::publish_hardware` so the shared provider (and anything reading it, like the
/// `miclight` pattern) flips instantly instead of waiting for its own next sample; (3) the UI nudge
/// last, once the truth it will read is already settled.
fn bridge_mic_mute(product: &str, muted: bool) {
    if product.trim().is_empty() {
        // No USB product string → we cannot identify WHICH capture endpoint pushed this, and a
        // generic guess (resolve_capture(None)) would set the mute on whatever default/first mic that
        // returns — a real WRONG-DEVICE write, not a stale readout. With nothing to match on, the tap
        // is simply not bridged — nothing downstream either: publishing to the shared provider or
        // nudging the UI for a device we can't name would be the same wrong-device lie in softer
        // form. (`find_capture("")` is ALSO a no-match by the predicate's own contract now — this
        // early return is the honest log line + skipping the pointless thread, not the safety.)
        // (A dialect-armed pipe on real Razer hardware always reports a product string.)
        if verbose() {
            eprintln!("[hidwatch] mute event with no product string — not bridged (endpoint unidentifiable)");
        }
        return;
    }
    let product = product.to_string();
    crate::worker::spawn_detached("neuron-hidwatch-mute", move || {
        // DEVICE-PRECISE: only the capture endpoint whose Core-Audio name CONTAINS this device's USB
        // product string. NO resolve_capture(None) fallback — on a name miss (localized/stripped
        // endpoint string) we skip the OS write entirely: an honest no-op beats muting someone else's
        // mic. The UI row still learns of the tap via `notify_hardware_mute`, matched by the SAME
        // product, so a miss here degrades to "OS doesn't follow" for that one device, never a
        // cross-device write.
        if let Some(ep) = neuron::audio::find_capture(&product) {
            if let Some(ctl) = neuron::audio::VolumeCtl::open(&ep.id) {
                ctl.set_mute(muted);
            }
        }
        neuron::mic_state::publish_hardware(muted);
        crate::glue::notify_hardware_mute(&product, muted);
    });
}

/// Translate one device-pushed report into the right action. De-dup / edge logic live downstream.
/// `def` is the registry def whose event pipe armed this collection (`None` for a plain mouse
/// collection, or a dialect-only arm with no def loaded); `dialect` is the family that PUSHES events
/// on this collection's shape (`None` when neither a def nor a dialect claims it — unreachable in
/// practice, since `arm_new` only spawns a reader when at least one of the three arming conditions
/// holds). Event resolution order: `def.event_for` FIRST (the per-device OVERRIDE) then
/// `dialect.default_event_for` (the family fallback) — so a device with its own `[events]` table
/// always wins over the family's generic vocabulary, and a device with NO def (or an empty auto one)
/// still gets the family's events. `product` is the arming collection's own USB product string, used
/// (not `def.name`, which may not exist) to resolve the OS capture endpoint in `bridge_mic_mute`.
fn decode(
    buf: &[u8],
    pid: u16,
    def: Option<&'static neuron::registry::DeviceDef>,
    dialect: Option<&'static dyn neuron::dialect::Dialect>,
    product: &str,
) {
    if buf.len() < 6 {
        return;
    }
    // REGISTRY-DRIVEN EVENTS FIRST (a def's own `[events]` table, e.g. a curated Seiren-class def),
    // else the FAMILY vocabulary (`Dialect::default_event_for`, e.g. razer-audio's `05 11` tap-mute) —
    // ahead of the 04/05 hardcoded families below so either always wins. The resolution chain +
    // payload read are pure (`mute_event_state`) so tests pin them without touching the OS mute.
    if let Some(muted) = mute_event_state(buf, def, dialect) {
        bridge_mic_mute(product, muted);
        return;
    }
    // 04-FAMILY: the DRIVER-MODE deferred-button vocabulary (see module header). The firmware, having
    // handed us its onboard DPI/scroll/profile buttons, emits a bare event per press; we ARE the
    // implementer. Map the code to a cycle intent and hand it to the serial worker — the mode gate and
    // the actual device write live THERE, off this reader thread. buf[1]==0x00 is a release and any
    // other code is an unseen deferred button: both map to None (release silently, unknown under
    // verbose). Sits BEFORE the 05-family gate; the 05 path below is unchanged.
    if buf[0] == 0x04 {
        match button_intent(buf[1]) {
            Some(intent) => {
                if let Some(tx) = button_worker() {
                    let _ = tx.send((pid, intent));
                }
            }
            None => {
                if buf[1] != 0x00 && verbose() {
                    eprintln!(
                        "[hidwatch] pid={pid:04x}: unknown 04-family button code {:#04x}",
                        buf[1]
                    );
                }
            }
        }
        return;
    }
    if buf[0] != 0x05 {
        return;
    }
    match buf[1] {
        // DPI changed: X then Y as big-endian u16 (device sets both axes together).
        0x02 => {
            let dpi = u16::from_be_bytes([buf[2], buf[3]]) as u32;
            if (100..=30_000).contains(&dpi) {
                batch_push(pid, Push::Dpi(dpi));
                // WAKE-RECONCILE, SECOND TRIGGER. The `05 0c` power poke is NOT emitted on every wake:
                // dpi_trap.log 08:39 (resident app) caught a wake that restored DPI 16000 and announced
                // it (`05 02 3e 80`) with NO `05 0c` — so the power-event trigger never fired and the
                // stale plane sat uncorrected. The DPI-announce is the RELIABLE signal: it fires on
                // every device-side DPI change, including the stale restore itself (the device confesses
                // its own corruption). But unlike the `05 0c` reassert (unconditional), this trigger is
                // MEMBERSHIP-GATED — the announce ALSO fires when the user's onboard DPI button walks
                // the cycle, and snapping that back would fight the user's thumb — so the worker heals
                // only a value FOREIGN to the persisted cycle (`maybe_reconcile_announced`). The decision
                // to admit this wake is made SYNCHRONOUSLY via the SAME 5s per-pid `reassert_due`
                // debounce the `05 0c` hook uses: whichever trigger sees a given wake FIRST stamps the
                // window and the other bows out, so one wake never double-fires a reconcile. The check
                // itself (persisted-cycle read + reconcile = control-pipe round-trips that must never
                // block this reader) rides its own off-thread worker.
                if reassert_due(pid) {
                    let announced = dpi as u16;
                    crate::worker::spawn_detached("neuron-hidwatch-dpi", move || {
                        maybe_reconcile_announced(pid, announced)
                    });
                }
            }
        }
        // Scroll / sensitivity stage changed: stage index in byte[2], bounded by the real stage count.
        0x3a => {
            let stage = buf[2] as u32;
            if (1..=SCROLL_STAGE_MAX).contains(&stage) {
                batch_push(pid, Push::Scroll(stage));
            }
        }
        // Power/charge poke — STATELESS. Settle the charge state (poll until it latches) then observe
        // as a real EVENT (fires a card even on a freshly-plugged device's first sample). Off-thread.
        //
        // The `05 0c` family ALSO fires on WAKE-from-idle — and that is when the wake-restore trap
        // bites: a device wakes with a STALE factory volatile plane (the Naga self-announced DPI 16000
        // with its stage table reverted while the persisted store stayed clean, 2026-07-07). Trap-proven
        // in BOTH device modes — the restoring source is an unmapped onboard-profile flash, not driver
        // mode — so the heal below is mode-independent. Real Synapse re-asserts config on every wake; we
        // take that duty here. The debounce decision is made SYNCHRONOUSLY on this listener thread (the
        // wake burst emits several 05-events; only the first arms a reconcile), but the reconcile itself
        // — control-pipe round-trips that must never block this reader — rides the SAME off-thread charge
        // worker, sequenced AFTER the settle so two control paths don't contend on the device's one
        // feature channel.
        //
        // KEPT as the BELT even though the `05 02` announce is the reliable wake signal: some wakes DO
        // emit `05 0c`, and one may not announce a DPI change at all (if the restored DPI happens to
        // equal what the volatile plane already held, the device emits no `05 02`). Both triggers funnel
        // into ONE debounced reconcile via the shared `reassert_due` stamps — whichever fires first for
        // a given wake wins the window — so this belt never double-fires against the announce path. The
        // `05 0c` reassert stays UNCONDITIONAL (no membership gate): a power poke is never a user's
        // onboard DPI button, so there is no legitimate cycle-step to protect here.
        0x0c => {
            let reassert = reassert_due(pid);
            crate::worker::spawn_detached("neuron-hidwatch-charge", move || {
                match settle_charge(pid) {
                    Some((b, c)) => neuron::vitals::observe(pid, b, c, true),
                    None => {
                        neuron::vitals::mark_stale(pid);
                        if verbose() {
                            eprintln!("[hidwatch] pid={pid:04x}: charge settle read failed");
                        }
                    }
                }
                if reassert {
                    maybe_reassert(pid);
                }
            });
        }
        // SIDE PLATE attached/detached: the swappable plate's hardware strap-code rides in byte[2].
        // Push-only — there is NO getter (a full getter sweep + swap-diff confirmed zero change), so
        // THIS report is the detection. Resolve the strap-code to a human label via the device's
        // registry `[side_plates]` map (id 0 = detached, handled in `plate_label`), then hand it to the
        // batcher — which absorbs the mid-seat bounce AND folds it into the wake-burst sync detection.
        0x0e => {
            let id = buf[2];
            let label = plate_label(pid, id);
            // RAW log stays on every event (discovery still sees the whole bounce under NEURON_HIDWATCH=1).
            if verbose() {
                eprintln!("[hidwatch] pid={pid:04x}: side plate -> {label} (id={id:#04x}) [raw]");
            }
            // Only the SETTLED value reaches `confirm`, and only if the batch isn't a wake-sync burst —
            // the bounce + the burst are both absorbed in the batcher, keeping `confirm` pure.
            batch_push(pid, Push::Plate(id, label));
            // SEAM (next phase): a plate change could also drive a per-plate PROFILE auto-switch.
            // Wire it here, off this same de-dup'd edge, so a swap both cards AND switches in one place.
        }
        _ => {}
    }
}

/// The PURE half of the hardware-mute event path: resolve one pushed report against the two-layer
/// event vocabulary and, when it names a MuteState, read the state bit the payload carries
/// (`report[2]`: 0=live, 1=muted). Resolution order is the contract [`decode`] promises — a def's own
/// `[events]` table FIRST (the per-device OVERRIDE), the family vocabulary
/// (`Dialect::default_event_for`) second — so a device with a curated def always wins over the
/// family's generic map, and a device with NO def still speaks via its family. `None` = this report
/// carries no mute event (or is too short to carry the state bit) — the caller falls through to the
/// other report families. No I/O, no globals: the side-effectful bridging stays in
/// [`bridge_mic_mute`], which is why tests can pin this chain without touching the OS mute.
fn mute_event_state(
    buf: &[u8],
    def: Option<&neuron::registry::DeviceDef>,
    dialect: Option<&dyn neuron::dialect::Dialect>,
) -> Option<bool> {
    let event = def
        .and_then(|d| d.event_for(buf))
        .or_else(|| dialect.and_then(|d| d.default_event_for(buf)));
    match event? {
        EventKind::MuteState => (buf.len() >= 3).then(|| buf[2] != 0),
    }
}

/// Map a 04-family deferred-button code to the cycle [`Intent`] it REQUESTS. Pure + table-testable so
/// the vocabulary is pinned without hardware. All three cycle UP: a deferred button carries no
/// direction (the firmware forwards only "pressed"), and a single physical button walks its cycle
/// forward — the same one-way step Synapse's onboard buttons do. buf[1]==0x00 (release) and any
/// unrecognized code fall through to `None` — every real press is a DELIBERATE user act, so there is
/// nothing to debounce; the serial worker's in-order execution is the whole ordering contract (3
/// presses = 3 steps).
///
/// INTERPLAY (DpiCycle): the volatile DPI write `run_shared_intent` makes here itself provokes a
/// device `05 02` DPI-announce. That announce feeds `maybe_reconcile_announced`, whose membership gate
/// recognizes an IN-CYCLE value and stays out of the way (it heals only values foreign to the persisted
/// cycle) — so our own cycle-step is never fought. That quiet depends on the active profile's
/// `dpi_stages` matching the device's persisted cycle; profile apply writes BOTH, so keep them synced.
fn button_intent(code: u8) -> Option<neuron::action::Intent> {
    use neuron::action::{Direction, Intent};
    match code {
        0x52 => Some(Intent::DpiCycle(Direction::Up)),
        0x57 => Some(Intent::ScrollStageCycle(Direction::Up)),
        0x50 => Some(Intent::ProfileCycle(Direction::Up)),
        _ => None,
    }
}

/// The SINGLE deferred-button implementer. Lazily spawned on first press; every mapped press is sent
/// down this one channel so presses execute STRICTLY in order and never race each other on the device's
/// one control pipe (two cycle-writes interleaving would corrupt the step). Returns the send-end; the
/// receive-end lives in the worker loop forever.
// `service_sender` caches the send-end ONLY once the worker actually spawned — so a refused spawn
// (resource exhaustion) can't leave presses flowing into a dead channel forever; the next press
// retries. `None` = the worker can't start right now, so the press is dropped honestly.
fn button_worker() -> Option<std::sync::mpsc::Sender<(u16, neuron::action::Intent)>> {
    static TX: crate::worker::Service<(u16, neuron::action::Intent)> =
        crate::worker::Service::new();
    crate::worker::service_sender(&TX, "neuron-hidwatch-button", |rx| {
        // drain CONTAINS a panic per press (a malformed HID reply slice-indexing, say) so one bad
        // press can't kill the serial button worker for the rest of the run.
        crate::worker::drain(rx, "neuron-hidwatch-button", |(pid, intent)| {
            fulfill_button(pid, intent);
        })
    })
}

/// Fulfill one deferred-button request on the serial worker (never the reader). The DRIVER-MODE GATE:
/// read device_mode FRESH per press and act only when it reads 0x03. WHY act only in driver mode — in
/// NORMAL mode (0x00) the firmware acts on the button ITSELF and merely announces the result via the
/// 05-family, so cycling here too would DOUBLE-APPLY; in driver mode the firmware defers and the 04
/// event is a REQUEST we must satisfy. WHY fresh, not cached — the lighting stream's driver lease comes
/// and goes, so a cached mode would either drop presses (stale "normal") or double-apply (stale
/// "driver"); the getter round-trip is cheap next to the write it guards. Past the gate, the request
/// rides the SAME shared cycle policy the CLI/GUI dispatch use (`run_shared_intent`: stage lookup,
/// volatile writes, resident scroll cursor, confirmation cards) so there is one implementation.
fn fulfill_button(pid: u16, intent: neuron::action::Intent) {
    let Some(d) = open_device(pid) else {
        return;
    };
    if neuron::writes::device_mode(&d) != Some(0x03) {
        // normal mode (firmware owns the button) or an unanswered getter (asleep link) — not ours.
        if verbose() {
            eprintln!("[hidwatch] pid={pid:04x}: 04-family button ignored (not in driver mode)");
        }
        return;
    }
    drop(d); // the gate handle is done; run_shared_intent opens its own writable via the session.
    let Some(reg) = registry() else {
        return;
    };
    let mut devices = neuron::device::DeviceSession::new(reg);
    let mut cursor = neuron::intent::ProcessProfileCursor;
    if let Some(msg) = neuron::intent::run_shared_intent(&mut devices, &mut cursor, &intent) {
        if verbose() {
            eprintln!("[hidwatch] pid={pid:04x}: 04-family button -> {msg}");
        }
    }
}

/// Resolve a side-plate hardware strap-code to its human label via the device's registry
/// `[side_plates]` table — the id→label map is DATA (the device TOML), never hardcoded here.
/// Strap-code `0` means no plate ("detached"); an UNKNOWN code degrades to a transparent "plate N"
/// rather than lying, so a firmware that adds a plate still reads honestly before the TOML catches up.
fn plate_label(pid: u16, id: u8) -> String {
    if id == 0 {
        return "detached".into();
    }
    registry()
        .and_then(|r| r.devices.iter().find(|d| d.product_ids().any(|p| p == pid)))
        .and_then(|d| d.side_plate_label(id))
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("plate {id}"))
}

/// One device-pushed settings change, on its way into the batch.
enum Push {
    Dpi(u32),
    Scroll(u32),
    Plate(u8, String), // raw strap-code + its resolved label
}

/// The pending batch of device-pushed settings reports inside one device's current settle window. One
/// slot per kind (so a same-kind bounce / rapid re-press collapses to the LATEST value), each
/// remembering the FIRST instant that kind appeared — that first-seen time is what the burst test
/// measures. One of these lives PER DEVICE (keyed by pid) so two armed mice never share a batch.
struct Batch {
    dpi: Option<(Instant, u32)>,
    scroll: Option<(Instant, u32)>,
    plate: Option<(Instant, u8, String)>,
}
impl Batch {
    const EMPTY: Batch = Batch { dpi: None, scroll: None, plate: None };
}

/// One device's batch plus its settle generation. Every push for THAT device bumps the generation; a
/// flush acts only if it still holds the latest (a newer push extends the window). Per-pid, so device
/// A's burst can't cancel device B's pending flush, and a burst is judged within ONE device's window.
struct BatchState {
    batch: Batch,
    generation: u64,
}

/// The per-pid pending batches, lazily created (a `HashMap` can't initialize a `const` static).
fn batches() -> &'static Mutex<HashMap<u16, BatchState>> {
    static B: OnceLock<Mutex<HashMap<u16, BatchState>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Add a pushed report to `pid`'s batch and (re)arm its settle window. Same-kind repeats overwrite the
/// VALUE but keep the FIRST-seen instant — so a seating bounce or a rapid DPI re-press stays one kind
/// at one moment, never a fake "burst". Every push bumps THAT device's generation so only its final
/// flush acts; a sibling device's batch and generation are untouched.
fn batch_push(pid: u16, ev: Push) {
    let my_gen = {
        let mut map = batches().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let st = map
            .entry(pid)
            .or_insert_with(|| BatchState { batch: Batch::EMPTY, generation: 0 });
        let now = Instant::now();
        match ev {
            Push::Dpi(v) => {
                let t = st.batch.dpi.map(|(t, _)| t).unwrap_or(now);
                st.batch.dpi = Some((t, v));
            }
            Push::Scroll(v) => {
                let t = st.batch.scroll.map(|(t, _)| t).unwrap_or(now);
                st.batch.scroll = Some((t, v));
            }
            Push::Plate(id, label) => {
                let t = st.batch.plate.as_ref().map(|(t, _, _)| *t).unwrap_or(now);
                st.batch.plate = Some((t, id, label));
            }
        }
        st.generation += 1;
        st.generation
    };
    crate::worker::spawn_detached("neuron-hidwatch-batch", move || {
        thread::sleep(BATCH_SETTLE);
        // Take + decide under the lock so a report landing in the gap can't be lost: if a newer
        // push for THIS pid bumped its generation, a later flush owns the batch — this one bows out.
        let batch = {
            let mut map = batches().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(st) = map.get_mut(&pid) else {
                return;
            };
            if st.generation != my_gen {
                return;
            }
            std::mem::replace(&mut st.batch, Batch::EMPTY)
        };
        flush_batch(pid, batch);
    });
}

/// Decide a settled batch. The SYNC TEST: ≥2 distinct card-worthy kinds whose first reports landed
/// within [`BURST_SPAN`] is a device state-announce (a human can't touch two distinct settings that
/// fast) → learn every value SILENTLY. Otherwise each present kind is a real user action → `observe_*`
/// it (which still de-dups and cards on a genuine change). A DETACHED plate (id 0) is the "no plate"
/// beat of a swap, never a state worth announcing, so it never counts toward the burst.
fn flush_batch(pid: u16, b: Batch) {
    let mut firsts: Vec<Instant> = Vec::new();
    if let Some((t, _)) = b.dpi {
        firsts.push(t);
    }
    if let Some((t, _)) = b.scroll {
        firsts.push(t);
    }
    if let Some((t, id, _)) = &b.plate {
        if *id != 0 {
            firsts.push(*t);
        }
    }
    let is_sync = firsts.len() >= 2 && {
        let lo = *firsts.iter().min().unwrap();
        let hi = *firsts.iter().max().unwrap();
        hi.duration_since(lo) <= BURST_SPAN
    };

    if is_sync {
        if let Some((_, v)) = b.dpi {
            neuron::confirm::prime_dpi(pid, v);
        }
        if let Some((_, v)) = b.scroll {
            neuron::confirm::prime_scroll(pid, v);
        }
        if let Some((_, id, label)) = b.plate {
            neuron::confirm::prime_side_plate(pid, id as u32, &label);
        }
    } else {
        if let Some((_, v)) = b.dpi {
            neuron::confirm::observe_dpi(pid, v);
        }
        if let Some((_, v)) = b.scroll {
            neuron::confirm::observe_scroll(pid, v, SCROLL_STAGE_MAX);
        }
        if let Some((_, id, label)) = b.plate {
            neuron::confirm::observe_side_plate(pid, id as u32, &label);
        }
    }
}

/// Open `pid`'s control interface (registry cached) and read battery % + charging. On a charge-read
/// error, fall back to the LAST-KNOWN charging state — never a phantom "unplugged" false — so a blip
/// can't fire a spurious charge card, yet the known-good battery % still feeds the edge logic.
fn read_battery(pid: u16) -> Option<(u8, bool)> {
    let d = open_device(pid)?;
    let b = neuron::capability::battery_percent(&d).ok()?;
    let c = neuron::capability::charging(&d)
        .ok()
        .or_else(|| neuron::vitals::last_charging(pid))
        .unwrap_or(false);
    Some((b, c))
}

/// After a `05 0c` poke, poll the charge state until it's STABLE (two identical reads) or a cap
/// elapses, reusing ONE open handle — so the card lands as soon as the device latches (snappy) without
/// trusting a fixed magic delay (reliable on slow-latching firmware). Cap: `NEURON_CHARGE_SETTLE_MS`
/// (default 150, min 50).
fn settle_charge(pid: u16) -> Option<(u8, bool)> {
    let d = open_device(pid)?;
    let cap_ms = std::env::var("NEURON_CHARGE_SETTLE_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(150)
        .max(50);
    let deadline = Instant::now() + Duration::from_millis(cap_ms);
    let mut last: Option<bool> = None;
    loop {
        if let Ok(c) = neuron::capability::charging(&d) {
            if last == Some(c) {
                let b = neuron::capability::battery_percent(&d).ok()?;
                return Some((b, c));
            }
            last = Some(c);
        }
        if Instant::now() >= deadline {
            // timeout — best effort: the last read (or the prior known state), plus a battery read.
            let c = last.or_else(|| neuron::vitals::last_charging(pid))?;
            let b = neuron::capability::battery_percent(&d).ok()?;
            return Some((b, c));
        }
        thread::sleep(Duration::from_millis(8));
    }
}

fn open_device(pid: u16) -> Option<neuron::device::Device> {
    let reg = registry()?;
    let def = reg.devices.iter().find(|d| d.product_ids().any(|p| p == pid))?;
    neuron::device::Device::open(def.clone(), pid).ok()
}

/// WAKE-RECONCILE debounce, SHARED across BOTH wake triggers. The `05 0c` power/wake family fires
/// SEVERAL events per wake burst, and the `05 02` DPI-announce fires on the same wake too; without a
/// shared window each would queue its own reconcile (redundant control-pipe traffic against a
/// just-woken device — and the announce and a power event for the SAME wake would double-fire). One
/// stamp map, so at most one reconcile per pid per window regardless of which trigger saw the wake
/// first.
const REASSERT_DEBOUNCE: Duration = Duration::from_secs(5);

/// Per-pid last-reassert stamps (lazily created — a `HashMap` can't init a `const` static).
fn reassert_stamps() -> &'static Mutex<HashMap<u16, Instant>> {
    static S: OnceLock<Mutex<HashMap<u16, Instant>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Should `pid` reassert now? True (and stamps the moment) only when the debounce window has elapsed
/// since the last reassert — so a wake BURST arms exactly one. Decided synchronously on the listener
/// thread so the burst is collapsed before any worker spawns.
fn reassert_due(pid: u16) -> bool {
    let mut map = reassert_stamps().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    match map.get(&pid) {
        Some(&last) if now.duration_since(last) < REASSERT_DEBOUNCE => false,
        _ => {
            map.insert(pid, now);
            true
        }
    }
}

/// Reconcile `pid`'s volatile DPI plane with its persisted truth after a wake — the Synapse duty
/// neuron owes for a device whose onboard-profile flash reloads factory tables on wake (see
/// `writes::reconcile_volatile_with_persisted` for the trap). NO device-mode gate: the reconcile was
/// once gated to driver mode on the assumption a normal-mode wake loads persisted config, but the
/// 2026-07-07 trap DISPROVED that — a NORMAL-mode wake restored the factory volatile table from the
/// unmapped profile store all the same. So it runs on every razer wake; the reconcile's OWN
/// disagreement gate (it writes nothing when the two planes already agree) provides the do-no-harm
/// property the mode gate was only approximating. Runs on the charge worker thread (off the reader).
/// Logs the outcome (a rare, debounced device-integrity event earns a line even without
/// NEURON_HIDWATCH; the read/verify inside the write is the safety net so a failure is honest, never a
/// silent corruption).
fn maybe_reassert(pid: u16) {
    let Some(d) = open_device(pid) else {
        return;
    };
    // FAMILY GATE (still correct): the wake-reconcile duty is part of RAZER's custody contract — the
    // varstore getters it reads are razer-framed (DPI class 0x04). `open_device` resolves ANY family's
    // def by pid, so on a non-razer def those getters would emit a validly-framed HID++ message with
    // garbage meaning at that hardware. Bail before any read on anything that isn't razer. (The old
    // driver-mode gate that sat here is GONE — the trap proved a normal-mode wake corrupts volatile
    // state too, so the reconcile runs mode-independently and leans on its own disagreement gate.)
    if d.def.dialect != "razer" {
        return;
    }
    reconcile_now(pid, &d);
}

/// The DPI-ANNOUNCE (`05 02`) wake worker — the MEMBERSHIP-GATED sibling of [`maybe_reassert`]. Runs
/// off the reader after the shared [`reassert_due`] debounce admitted this wake. The announce fires on
/// EVERY device-side DPI change, so before healing we must tell a legitimate onboard-button cycle-step
/// apart from the trap-proven stale wake-restore — `writes::announced_dpi_is_foreign` reads the
/// device's PERSISTED cycle and tests membership:
///   • `Some(false)` — `announced` is a cycle member → the user's onboard button chose it → do NOTHING
///     (a reconcile would snap the cursor back against the user's thumb).
///   • `Some(true)`  — the value is in no configured stage → nobody legitimate chose it (the 08:39
///     wake-restore's factory 16000, which arrived with no `05 0c` power event) → run the reconcile.
///   • `None`        — the persisted cycle is unreadable → do NOTHING (never heal on missing evidence).
/// Same `dialect != "razer"` family gate as [`maybe_reassert`]: the varstore getters are razer-framed,
/// so bail before any read on a non-razer def that `open_device` resolved by pid.
fn maybe_reconcile_announced(pid: u16, announced: u16) {
    let Some(d) = open_device(pid) else {
        return;
    };
    if d.def.dialect != "razer" {
        return;
    }
    match neuron::writes::announced_dpi_is_foreign(&d, announced) {
        Some(true) => reconcile_now(pid, &d),
        _ => {
            // Some(false) = legitimate onboard cycle-step; None = unreadable/empty cycle. Either way no
            // heal — the anti-fight-the-user gate and the never-reconcile-on-missing-evidence rule.
            if verbose() {
                eprintln!(
                    "[hidwatch] pid={pid:04x}: DPI announce {announced} not foreign; no reconcile"
                );
            }
        }
    }
}

/// Run the disagreement-gated reconcile against an already-opened, already-family-checked device and
/// log the outcome. SHARED by both wake triggers — the `05 0c` power poke ([`maybe_reassert`], which
/// reaches here unconditionally) and the `05 02` DPI-announce ([`maybe_reconcile_announced`], which
/// reaches here only past the foreign-membership gate) — so the "a rare debounced device-integrity
/// event earns a line even without NEURON_HIDWATCH; the read/verify inside the write is the safety
/// net" logging is identical on both paths.
fn reconcile_now(pid: u16, d: &neuron::device::Device) {
    match neuron::writes::reconcile_volatile_with_persisted(d) {
        Ok(items) if !items.is_empty() => {
            eprintln!("[hidwatch] pid={pid:04x}: wake-reconcile -> {}", items.join(", "));
        }
        Ok(_) => {
            if verbose() {
                eprintln!("[hidwatch] pid={pid:04x}: wake-reconcile (stores already agree)");
            }
        }
        Err(e) => {
            if verbose() {
                eprintln!("[hidwatch] pid={pid:04x}: wake-reconcile failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Naga V2 Pro's dongle PID — the device that ships the `[side_plates]` map.
    const NAGA_PID: u16 = 0x00A8;

    // The per-pid batches and `confirm`'s per-pid baselines for NAGA_PID, plus the global sink, are
    // shared across these tests; serialize the tests that drive `decode` (and read the sink) so their
    // settle threads can't interleave across tests.
    static BATCH_TEST_LOCK: Mutex<()> = Mutex::new(());

    // Drive one device-pushed report through the real decode path. `b1` is the report kind byte.
    fn feed(b1: u8, b2: u8, b3: u8) {
        let mut buf = [0u8; 16];
        buf[0] = 0x05;
        buf[1] = b1;
        buf[2] = b2;
        buf[3] = b3;
        decode(&buf, NAGA_PID, None, None, "");
    }
    fn dpi_report(dpi: u16) {
        let [hi, lo] = dpi.to_be_bytes();
        feed(0x02, hi, lo);
    }
    fn scroll_report(stage: u8) {
        feed(0x3a, stage, 0);
    }
    fn plate_report(id: u8) {
        feed(0x0e, id, 0);
    }
    // Long enough for the settle thread to fire and finish before we assert.
    fn settle() {
        thread::sleep(BATCH_SETTLE + Duration::from_millis(150));
    }

    #[test]
    fn registry_cache_swaps_on_reload_instead_of_freezing_startup() {
        // The reload-mismatch pin: this cache is the ONE app-layer registry both hidwatch (arming)
        // and glue (capability gating) read, and the runtime's `synth_dirty` reload path calls
        // `reload_registry()` on it. A regression back to a frozen OnceLock snapshot would make an
        // adopted def appear in the device list while event arming + mute gating kept the startup
        // snapshot until restart — so pin that a reload actually SWAPS the snapshot. (Old readers
        // keeping their previous leaked snapshot is by design; new reads must see the new one.)
        let before = registry().expect("registry loads on a dev checkout")
            as *const neuron::registry::Registry;
        reload_registry();
        let after = registry().expect("registry reloads") as *const neuron::registry::Registry;
        assert!(
            !std::ptr::eq(before, after),
            "reload_registry must swap in a freshly loaded snapshot, not keep the startup one"
        );
    }

    #[test]
    fn plate_label_resolves_strap_codes_via_registry() {
        // the id→label map is DATA (the device TOML) — these are the live-verified strap-codes.
        assert_eq!(plate_label(NAGA_PID, 0x01), "2-button");
        assert_eq!(plate_label(NAGA_PID, 0x03), "12-button");
        assert_eq!(plate_label(NAGA_PID, 0x04), "6-button");
        // 0 = no plate; an unknown code degrades to a transparent "plate N", never a wrong label.
        assert_eq!(plate_label(NAGA_PID, 0x00), "detached");
        assert_eq!(plate_label(NAGA_PID, 0x09), "plate 9");
    }

    #[test]
    fn lone_plate_report_settles_and_reaches_confirm() {
        // a single 05 0e <strap_id> is ONE kind alone — never a sync burst — so after the settle
        // window it reaches `confirm` and updates the readout.
        let _g = BATCH_TEST_LOCK.lock().unwrap();
        plate_report(0x03); // 12-button
        settle();
        assert_eq!(neuron::confirm::last_plate(NAGA_PID).as_deref(), Some("12-button"));
    }

    #[test]
    fn settle_window_keeps_only_the_last_plate_value() {
        // a seating bounce: the strap flickers detached↔seated before it settles. Same-kind reports
        // overwrite the pending value (keeping first-seen), so only the FINAL stable code reaches
        // `confirm` — the flicker never does, and one kind alone is never mistaken for a sync.
        let _g = BATCH_TEST_LOCK.lock().unwrap();
        plate_report(0); // detached
        plate_report(1); // 2-button
        plate_report(0); // detached
        plate_report(1); // settles here
        settle();
        assert_eq!(neuron::confirm::last_plate(NAGA_PID).as_deref(), Some("2-button"));
    }

    #[test]
    fn wake_burst_is_primed_silently_never_carded() {
        // THE FIX: a wake/reconnect re-announces dpi + scroll + plate in one tight burst (these decode
        // calls land within microseconds — far inside BURST_SPAN). That's a STATE SYNC: zero cards,
        // but the state is still LEARNED (the readout reflects the synced plate).
        let _g = BATCH_TEST_LOCK.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        neuron::confirm::set_sink(Some(tx));
        // Drain to QUIESCENCE before the actual test. The confirm sink is process-global, and a
        // prior test's async settle worker — delayed past its own fixed-sleep `settle` under heavy
        // full-workspace load — can still be in flight and would otherwise land in this sink as a
        // phantom card (observed as spurious "got N cards" failures only under load). We hold
        // BATCH_TEST_LOCK, so no other batch test runs concurrently, and any worker already spawned
        // fires within one `BATCH_SETTLE`; two consecutive empty spans therefore prove nothing
        // spawned before this point is still pending, so the only cards the assertion below can see
        // are the ones THIS test's reports produce.
        let mut quiet = 0;
        while quiet < 2 {
            settle();
            if rx.try_iter().count() == 0 {
                quiet += 1;
            } else {
                quiet = 0;
            }
        }
        dpi_report(1600);
        scroll_report(3);
        plate_report(4); // 6-button
        settle();
        let cards: Vec<_> = rx.try_iter().collect();
        neuron::confirm::set_sink(None);
        assert!(cards.is_empty(), "a wake-burst must prime silently, got {} card(s)", cards.len());
        assert_eq!(neuron::confirm::last_plate(NAGA_PID).as_deref(), Some("6-button"));
    }

    #[test]
    fn a_lone_change_outside_any_burst_still_cards() {
        // the other half of foolproof: a single change is a USER action and must still card. Establish
        // a known baseline first (plate 1), then a DIFFERENT lone plate must produce a card — proving
        // the burst guard never over-suppresses genuine input.
        let _g = BATCH_TEST_LOCK.lock().unwrap();
        plate_report(1); // 2-button — set the baseline (no sink yet)
        settle();
        let (tx, rx) = std::sync::mpsc::channel();
        neuron::confirm::set_sink(Some(tx));
        plate_report(4); // 6-button, ALONE → a real swap → must card
        settle();
        neuron::confirm::set_sink(None);
        let cards: Vec<_> = rx.try_iter().collect();
        assert!(
            cards.iter().any(|c| c.kind == neuron::confirm::Kind::SidePlate),
            "a lone plate swap must still card; got {cards:?}"
        );
    }

    #[test]
    fn decode_ignores_short_or_foreign_reports() {
        // the existing guards must still hold — a non-0x05 lead byte or a too-short buffer is a no-op
        // (no panic), so the new arm can't destabilize the DPI/scroll/charge decoding.
        decode(&[0x05, 0x0e], NAGA_PID, None, None, ""); // too short (< 6) — guarded
        decode(&[0x02, 0x0e, 0x03, 0, 0, 0], NAGA_PID, None, None, ""); // neither 04 nor 05 lead byte — guarded
    }

    #[test]
    fn deferred_button_codes_map_to_their_cycle_intents() {
        use neuron::action::{Direction, Intent};
        // the live-captured 04-family vocabulary, pinned. Each maps to its cycle, stepping UP.
        assert!(matches!(button_intent(0x52), Some(Intent::DpiCycle(Direction::Up))));
        assert!(matches!(button_intent(0x57), Some(Intent::ScrollStageCycle(Direction::Up))));
        assert!(matches!(button_intent(0x50), Some(Intent::ProfileCycle(Direction::Up))));
    }

    // ── the hardware-mute event path, pinned WITHOUT hardware or the OS mute ────────────────────
    // `mute_event_state` is the pure resolution chain `decode` promises; these mint a def from TOML
    // (the only public constructor, same as registry.rs's own tests) and resolve the real
    // razer-audio dialect by its live pipe signature — no stubs, the actual production layers.

    /// The razer-audio family, resolved exactly the way `arm_new` resolves it: by claiming the
    /// Seiren-class pipe shape (Razer VID + usage 000c/0001 + 64-byte feature report).
    fn razer_audio() -> &'static dyn neuron::dialect::Dialect {
        let info = neuron::transport::HidDeviceInfo {
            vid: 0x1532,
            pid: 0x056A,
            usage_page: 0x000C,
            usage: 0x0001,
            feature_len: 64,
            input_len: 0,
            output_len: 0,
            path: neuron::transport::DevicePath::from_str_for_tests("test-pipe"),
            product: "Razer Seiren V3 Mini".into(),
        };
        neuron::dialect::event_dialect_for(&info).expect("razer-audio claims its own signature")
    }

    /// A minimal def whose `[events]` table names a NON-family lead pair (`05 02`) as MuteState —
    /// distinguishable from razer-audio's family `05 11`, so the tests can see WHICH layer answered.
    fn def_with_events() -> neuron::registry::DeviceDef {
        toml::from_str(
            "name = \"Test Seiren\"\ncodename = \"test-seiren\"\nvendor_id = 5426\ntransaction_id = 0x1F\n\
             [[modes]]\nname = \"wired\"\nproduct_id = 1386\n\
             [control_interface]\nusage_page = 1\nusage = 2\nfeature_report_len = 91\n\
             [commands]\n\
             [events]\nusage_page = 12\nusage = 1\nfeature_len = 64\n\
             [events.reports]\n\"0502\" = \"mute_state\"\n",
        )
        .expect("test def parses")
    }

    #[test]
    fn mute_event_resolves_def_override_first_then_family_fallback() {
        let def = def_with_events();
        let fam = razer_audio();
        // the def's OWN vocabulary answers for a lead pair the family doesn't know: the override layer.
        assert_eq!(
            mute_event_state(&[0x05, 0x02, 0x01, 0, 0, 0], Some(&def), Some(fam)),
            Some(true),
            "a def's [events] entry must resolve even when the family vocabulary is silent"
        );
        // the family vocabulary still answers THROUGH a present-but-silent def: the fallback layer.
        assert_eq!(
            mute_event_state(&[0x05, 0x11, 0x00, 0, 0, 0], Some(&def), Some(fam)),
            Some(false),
            "a family push must survive a def that doesn't name it"
        );
        // a def-less (auto/empty) arm still speaks via its family — the emergence the dialect layer buys.
        assert_eq!(mute_event_state(&[0x05, 0x11, 0x01, 0, 0, 0], None, Some(fam)), Some(true));
        // and a def-only arm needs no dialect.
        assert_eq!(mute_event_state(&[0x05, 0x02, 0x00, 0, 0, 0], Some(&def), None), Some(false));
    }

    #[test]
    fn mute_event_ignores_foreign_short_and_unclaimed_reports() {
        let def = def_with_events();
        let fam = razer_audio();
        // a lead pair NEITHER layer names is not a mute event — decode falls through to 04/05 arms.
        assert_eq!(mute_event_state(&[0x05, 0x3a, 0x01, 0, 0, 0], Some(&def), Some(fam)), None);
        // no def and no dialect (a plain mouse collection) can never produce one.
        assert_eq!(mute_event_state(&[0x05, 0x11, 0x01, 0, 0, 0], None, None), None);
        // too short to carry the state bit → not an event, never a guessed state.
        assert_eq!(mute_event_state(&[0x05, 0x11], None, Some(fam)), None);
    }

    #[test]
    fn capability_surface_never_learns_an_empty_product() {
        // the stores hold needles for `endpoint_matches_product`, whose contract is "empty
        // identifies nothing" — so an arm with a stripped/blank product must surface NO capability.
        let fam = razer_audio();
        note_audio_capability(fam, "");
        note_audio_capability(fam, "   ");
        assert!(
            hardware_mute_products().iter().all(|p| !p.trim().is_empty()),
            "a blank product must never land on the capability surface"
        );
    }

    #[test]
    fn capability_surface_learns_read_facet_but_not_unproven_write() {
        let fam = razer_audio();
        // a unique name so this test owns its entry in the process-shared, insert-only store.
        let product = "Test Capability Mic 7f3a";
        note_audio_capability(fam, product);
        assert!(
            hardware_mute_products().iter().any(|p| p == product),
            "an arming dialect-claimed collection must surface the READ facet"
        );
        // razer-audio is read-only (`audio_mute_writable` = false): the WRITE facet must stay
        // empty of it — the UI renders an indicator, never a toggle that would fight the firmware LED.
        assert!(
            !mute_writable_products().iter().any(|p| p == product),
            "a read-only family must never surface the WRITE facet"
        );
    }

    #[test]
    fn releases_and_unknown_button_codes_map_to_nothing() {
        // buf[1]==0x00 is EVERY button's release — never an action. Unknown codes are unseen deferred
        // buttons — ignored (logged under verbose only), never guessed into a wrong cycle.
        assert!(button_intent(0x00).is_none(), "release must map to nothing");
        for code in [0x01u8, 0x51, 0x53, 0x56, 0x99, 0xff] {
            assert!(button_intent(code).is_none(), "unknown code {code:#04x} must map to nothing");
        }
    }

    // ── HOTPLUG BURST STRESS: the map-mutating entry points under concurrent rapid
    // plug/unplug/re-plug, same pid AND distinct pids at once ──────────────────────────────────
    //
    // `mute_products_store`/`mute_writable_store` (via `note_audio_capability`), `batches` (via
    // `batch_push`, the same function `decode`'s 0x02/0x3a/0x0e arms call), and `reassert_stamps`
    // (via `reassert_due`, the same function `decode`'s 0x02/0x0c arms call) are the four
    // process-global `OnceLock<Mutex<..>>` maps keyed by pid/product. Driven DIRECTLY (not through
    // `decode`) so the burst can never touch a real device: `decode`'s own dpi/charge arms spawn a
    // worker that opens a device BY PID (`maybe_reconcile_announced`/`maybe_reassert` ->
    // `open_device`), which for a REGISTERED pid (e.g. `NAGA_PID`, used by this file's other tests)
    // would attempt a real HID open on whatever hardware happens to be plugged into this machine —
    // exactly the "never touch real hardware" line a test must not cross. Calling `batch_push` and
    // `reassert_due` straight — the same functions `decode` calls into — exercises the identical
    // map code with zero device I/O, registered pid or not. `button_worker`/`fulfill_button`/
    // `open_device`/`read_battery`/`settle_charge` are NOT exercised here: every one of them needs a
    // real (or at least registered) device behind `open_device` to do anything but bail early, so
    // they are out of headless scope — honestly excluded rather than faked.
    #[test]
    fn hotplug_burst_stresses_global_maps_without_corruption() {
        let _g = BATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        const THREADS: u16 = 16;
        const ITERS: usize = 50;
        // even-indexed threads hammer ONE shared pid — rapid plug/unplug/re-plug of the SAME
        // device; odd-indexed threads each own a distinct pid — several different devices at once.
        // Neither pid range is registered in the real device TOMLs, so `open_device` (were it ever
        // reached from here, which it isn't) would bail before any real HID call.
        const SHARED_PID: u16 = 0xE0FF;
        let fam = razer_audio();

        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                std::thread::spawn(move || {
                    let pid = if t % 2 == 0 { SHARED_PID } else { 0xE100 + t };
                    let product = format!("burst-mic-{t}");
                    for i in 0..ITERS {
                        // "plug": the arm-time capability learn + a device-pushed settings report,
                        // alternating kind so DPI/scroll/plate each get real traffic across the run.
                        note_audio_capability(fam, &product);
                        reassert_due(pid);
                        match i % 3 {
                            0 => batch_push(pid, Push::Dpi(800 + (i as u32 % 100))),
                            1 => batch_push(pid, Push::Scroll(1 + (i as u32 % SCROLL_STAGE_MAX))),
                            _ => batch_push(pid, Push::Plate((i % 5) as u8, format!("plate-{i}"))),
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("a burst thread must never panic");
        }

        // quiesce: let every settle worker the burst spawned finish flushing.
        settle();

        // no poisoned mutex: a raw `.lock()` on each of the four maps must still succeed.
        assert!(mute_products_store().lock().is_ok(), "mute_products_store poisoned");
        assert!(mute_writable_store().lock().is_ok(), "mute_writable_store poisoned");
        assert!(batches().lock().is_ok(), "batches poisoned");
        assert!(reassert_stamps().lock().is_ok(), "reassert_stamps poisoned");

        // every pid the burst touched settled to a CLEAN (fully-flushed) batch — no half-applied
        // state from the storm survives quiescence, whether the pid was shared or exclusive.
        {
            let map = batches().lock().unwrap_or_else(|e| e.into_inner());
            for t in 0..THREADS {
                let pid = if t % 2 == 0 { SHARED_PID } else { 0xE100 + t };
                let st = map
                    .get(&pid)
                    .unwrap_or_else(|| panic!("pid {pid:#06x} lost its batch entry"));
                assert!(
                    st.batch.dpi.is_none() && st.batch.scroll.is_none() && st.batch.plate.is_none(),
                    "pid {pid:#06x} left a half-flushed batch after quiescence: dpi={:?} scroll={:?} plate={:?}",
                    st.batch.dpi,
                    st.batch.scroll,
                    st.batch.plate.as_ref().map(|(_, id, _)| id),
                );
            }
        }

        // reassert_stamps learned every pid the burst touched — the shared pid plus each distinct
        // one — never a phantom key (a torn write landing under the wrong pid) and never a dropped
        // one (contention on the shared pid losing a write outright).
        {
            let map = reassert_stamps().lock().unwrap_or_else(|e| e.into_inner());
            assert!(map.contains_key(&SHARED_PID), "the shared pid must have a reassert stamp");
            for t in (1..THREADS).step_by(2) {
                let pid = 0xE100 + t;
                assert!(map.contains_key(&pid), "distinct pid {pid:#06x} must have a reassert stamp");
            }
        }

        // the capability surface learned every thread's product (READ facet), and none of it
        // leaked into the WRITE facet — razer-audio is read-only, the same invariant the
        // single-threaded `capability_surface_learns_read_facet_but_not_unproven_write` test pins.
        let read = hardware_mute_products();
        let write = mute_writable_products();
        for t in 0..THREADS {
            let product = format!("burst-mic-{t}");
            assert!(
                read.iter().any(|p| *p == product),
                "product {product} missing from the read facet"
            );
            assert!(
                !write.iter().any(|p| *p == product),
                "razer-audio must never surface the write facet"
            );
        }

        // ── FRESH-PID PASS: a normal single-threaded run, on a pid/product the burst never
        // touched, must behave EXACTLY like it would on a clean process — no state the storm left
        // behind corrupts or wedges a later, unrelated arm. Mirrors the existing single-threaded
        // `lone_plate_report_settles_and_reaches_confirm` expectation, computed the same way.
        const FRESH_PID: u16 = 0xE999;
        assert!(
            reassert_due(FRESH_PID),
            "a pid never seen before (burst or not) must reassert on its first wake"
        );
        batch_push(FRESH_PID, Push::Plate(2, "clean-plate".into()));
        settle();
        assert_eq!(
            neuron::confirm::last_plate(FRESH_PID).as_deref(),
            Some("clean-plate"),
            "a lone plate report on a fresh pid must still settle and reach confirm after the burst"
        );
        let fresh_product = "burst-clean-product";
        note_audio_capability(fam, fresh_product);
        assert!(
            hardware_mute_products().iter().any(|p| p == fresh_product),
            "a fresh product must still land on the capability surface after the burst"
        );

        // Leave the process-global surface as clean as a fresh start. This test drives ~800 async
        // settle/batch workers and populates four shared maps with ~17 pids; `BATCH_TEST_LOCK`
        // serializes test EXECUTION but does not RESET this state, so a straggler card (a worker
        // still flushing after `settle`'s fixed sleep, which a loaded machine can outrun) or a
        // residual map entry would leak into whatever test runs next (e.g. the wake-burst test,
        // which asserts a SILENT prime and would see the stragglers as phantom cards). Drain the
        // confirm pipeline to quiescence, then clear the maps we filled.
        {
            let (tx, rx) = std::sync::mpsc::channel();
            neuron::confirm::set_sink(Some(tx));
            // Two full settle spans with no new input: any worker still in flight from the storm
            // has flushed by the end of the second quiet span. Loop until a span yields nothing.
            for _ in 0..5 {
                settle();
                if rx.try_iter().count() == 0 {
                    break;
                }
            }
            neuron::confirm::set_sink(None);
        }
        batches().lock().unwrap_or_else(|e| e.into_inner()).clear();
        reassert_stamps().lock().unwrap_or_else(|e| e.into_inner()).clear();
        mute_products_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
        mute_writable_store().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}
