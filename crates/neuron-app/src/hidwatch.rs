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
//! De-dup for the DPI/scroll echoes lives in `confirm`; the battery/charge edge logic + throttle live
//! in `neuron::vitals`. A hotplug MONITOR re-arms collections after a dongle replug. Verified on the
//! Naga V2 Pro family; when more devices are captured the report map should move to the registry.
//!
//! `NEURON_HIDWATCH=1` additionally logs every raw report as hex (for decoding new devices) and the
//! (otherwise-silent) battery-read failures.

use neuron::transport::DevicePath;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const RAZER_VID: u16 = 0x1532;
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
fn registry() -> Option<&'static neuron::registry::Registry> {
    static R: OnceLock<Option<neuron::registry::Registry>> = OnceLock::new();
    R.get_or_init(|| neuron::registry::Registry::load().ok()).as_ref()
}

/// Arm the listener: a reader thread per readable, event-carrying collection of each connected Razer
/// DPI-mouse, plus a hotplug monitor that re-arms after replug. Never fails the app.
pub fn start() {
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
    thread::Builder::new()
        .name("neuron-hidwatch-mon".into())
        .spawn(move || loop {
            thread::sleep(HOTPLUG_POLL);
            arm_new(&mouse_pids, &mon);
        })
        .ok();
}

/// Enumerate and spawn a reader for any event-carrying mouse collection not already armed.
fn arm_new(mouse_pids: &HashSet<u16>, armed: &Arc<Mutex<HashSet<DevicePath>>>) {
    let infos = match neuron::transport::enumerate() {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut spawned = 0usize;
    for info in infos {
        if info.vid != RAZER_VID || !mouse_pids.contains(&info.pid) {
            continue;
        }
        // Where Razer's event reports ride: the sibling generic-desktop collection with the undefined
        // usage (`u=0x0000`) or a vendor page. Skip the OS-protected mouse collection + media keys.
        let event_carrying =
            (info.usage_page == 0x0001 && info.usage == 0x0000) || info.usage_page >= 0xFF00;
        if !event_carrying {
            continue;
        }
        // claim this collection (HashSet::insert is false if already armed → skip)
        if !armed.lock().unwrap().insert(info.path.clone()) {
            continue;
        }
        spawn_reader(info.pid, info.path.clone(), armed.clone());
        spawned += 1;
    }
    if verbose() && spawned > 0 {
        eprintln!("[hidwatch] armed {spawned} collection(s)");
    }
}

fn spawn_reader(pid: u16, path: DevicePath, armed: Arc<Mutex<HashSet<DevicePath>>>) {
    let tag = format!("pid={pid:04x}");
    thread::Builder::new()
        .name("neuron-hidwatch".into())
        .spawn(move || {
            let reader = match neuron::transport::open_reader(&path) {
                Ok(r) => r,
                Err(e) => {
                    if verbose() {
                        eprintln!("[hidwatch] {tag}: not readable ({e})");
                    }
                    armed.lock().unwrap().remove(&path); // let the monitor retry later
                    return;
                }
            };
            if verbose() {
                eprintln!("[hidwatch] LISTENING {tag}");
            }
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) if n > 0 => {
                        if verbose() {
                            eprint!("[hidwatch] {tag} n={n} ");
                            for b in &buf[..n] {
                                eprint!("{b:02x} ");
                            }
                            eprintln!();
                        }
                        decode(&buf[..n], pid);
                        // lazy battery freshness — piggyback on activity (the device is awake; it's
                        // sending reports), throttled, off-thread so a slow open never stalls reads.
                        if neuron::vitals::due(pid, false) {
                            thread::spawn(move || match read_battery(pid) {
                                Some((b, c)) => neuron::vitals::observe(pid, b, c, false),
                                None => {
                                    neuron::vitals::mark_stale(pid); // don't strand the throttle
                                    if verbose() {
                                        eprintln!("[hidwatch] pid={pid:04x}: battery read failed");
                                    }
                                }
                            });
                        }
                    }
                    Ok(_) => {} // zero-length read — keep listening
                    Err(_) => {
                        // unplugged / device gone — drop our claim so the monitor re-arms on replug.
                        armed.lock().unwrap().remove(&path);
                        if verbose() {
                            eprintln!("[hidwatch] {tag}: closed");
                        }
                        return;
                    }
                }
            }
        })
        .ok();
}

/// Translate one device-pushed report into the right action. De-dup / edge logic live downstream.
fn decode(buf: &[u8], pid: u16) {
    if buf.len() < 6 || buf[0] != 0x05 {
        return;
    }
    match buf[1] {
        // DPI changed: X then Y as big-endian u16 (device sets both axes together).
        0x02 => {
            let dpi = u16::from_be_bytes([buf[2], buf[3]]) as u32;
            if (100..=30_000).contains(&dpi) {
                batch_push(pid, Push::Dpi(dpi));
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
        0x0c => {
            thread::spawn(move || match settle_charge(pid) {
                Some((b, c)) => neuron::vitals::observe(pid, b, c, true),
                None => {
                    neuron::vitals::mark_stale(pid);
                    if verbose() {
                        eprintln!("[hidwatch] pid={pid:04x}: charge settle read failed");
                    }
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
        let mut map = batches().lock().unwrap();
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
    thread::Builder::new()
        .name("neuron-hidwatch-batch".into())
        .spawn(move || {
            thread::sleep(BATCH_SETTLE);
            // Take + decide under the lock so a report landing in the gap can't be lost: if a newer
            // push for THIS pid bumped its generation, a later flush owns the batch — this one bows out.
            let batch = {
                let mut map = batches().lock().unwrap();
                let Some(st) = map.get_mut(&pid) else {
                    return;
                };
                if st.generation != my_gen {
                    return;
                }
                std::mem::replace(&mut st.batch, Batch::EMPTY)
            };
            flush_batch(pid, batch);
        })
        .ok();
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
        decode(&buf, NAGA_PID);
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
        dpi_report(1600);
        scroll_report(3);
        plate_report(4); // 6-button
        settle();
        neuron::confirm::set_sink(None);
        let cards: Vec<_> = rx.try_iter().collect();
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
        decode(&[0x05, 0x0e], NAGA_PID); // too short (< 6) — guarded
        decode(&[0x01, 0x0e, 0x03, 0, 0, 0], NAGA_PID); // wrong lead byte — guarded
    }
}
