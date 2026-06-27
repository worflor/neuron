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
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const RAZER_VID: u16 = 0x1532;
/// Trailing debounce for a seating side plate. When a plate seats, its strap contact BOUNCES (the
/// strap-code flickers, esp. `00`↔`01` for the 2-button) before it settles — so we wait this window
/// of quiet after the LAST `05 0e` report before committing, so only the SETTLED value cards. ~220ms
/// is comfortably longer than an observed seat bounce yet short enough to feel instant on a clean swap.
const PLATE_DEBOUNCE: Duration = Duration::from_millis(220);
/// Generation counter for the plate debounce: every `05 0e` bumps it; a debounce thread commits only
/// if it is still the latest (no newer report arrived during its wait). A bounce keeps bumping it, so
/// every superseded value is discarded and only the final, stable one ever reaches `confirm`.
static PLATE_GEN: AtomicU64 = AtomicU64::new(0);
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
                neuron::confirm::observe_dpi(dpi);
            }
        }
        // Scroll / sensitivity stage changed: stage index in byte[2], bounded by the real stage count.
        0x3a => {
            let stage = buf[2] as u32;
            if (1..=SCROLL_STAGE_MAX).contains(&stage) {
                neuron::confirm::observe_scroll(stage, SCROLL_STAGE_MAX);
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
        // de-dup'd core observe — which absorbs the brief mid-seat transient (fire only on real change).
        0x0e => {
            let id = buf[2];
            let label = plate_label(pid, id);
            // RAW log stays on every event (discovery still sees the whole bounce under NEURON_HIDWATCH=1).
            if verbose() {
                eprintln!("[hidwatch] pid={pid:04x}: side plate -> {label} (id={id:#04x}) [raw]");
            }
            // Only the DEBOUNCED (settled) value reaches `confirm` — the seat bounce is absorbed here,
            // keeping `confirm` pure (no timing there). See `commit_plate_debounced`.
            commit_plate_debounced(id, label);
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

/// Trailing time-debounce for a seating side plate. Each `05 0e` report bumps the generation and
/// spawns a short-lived thread that sleeps [`PLATE_DEBOUNCE`] and then commits to `confirm` ONLY if it
/// is still the latest generation — i.e. no newer report arrived during its wait. A seating bounce
/// (`00→01→00→01`) fires this several times in quick succession; each spawn supersedes the previous,
/// so all but the FINAL value find a higher generation and exit without committing. The result: a swap
/// settles to exactly one `observe_side_plate` call carrying the stable value (`confirm` stays pure —
/// it only ever sees the settled code, never the flicker). Cheap: a handful of short threads per swap.
fn commit_plate_debounced(id: u8, label: String) {
    let my_gen = PLATE_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    thread::Builder::new()
        .name("neuron-hidwatch-plate".into())
        .spawn(move || {
            thread::sleep(PLATE_DEBOUNCE);
            if PLATE_GEN.load(Ordering::Relaxed) == my_gen {
                // settled — no newer 05 0e arrived during the window; this is the value to commit.
                neuron::confirm::observe_side_plate(id as u32, &label);
            }
        })
        .ok();
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

    // The plate debounce generation + `confirm`'s last-plate are process-global; serialize the tests
    // that drive a real commit so their generations/threads can't interleave and supersede each other.
    static PLATE_TEST_LOCK: Mutex<()> = Mutex::new(());

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
    fn decode_routes_05_0e_report_to_plate_observe() {
        // a 16-byte side-plate report drives the push-only plate detection: 05 0e <strap_id>. The
        // commit is TRAILING-DEBOUNCED (a seating plate bounces), so it lands AFTER the quiet window,
        // not synchronously — wait past it, then assert the settled label reached `confirm`.
        let _g = PLATE_TEST_LOCK.lock().unwrap();
        let mut buf = [0u8; 16];
        buf[0] = 0x05;
        buf[1] = 0x0e;
        buf[2] = 0x03;
        decode(&buf, NAGA_PID);
        thread::sleep(PLATE_DEBOUNCE + Duration::from_millis(120));
        assert_eq!(neuron::confirm::last_plate().as_deref(), Some("12-button"));
    }

    #[test]
    fn plate_debounce_commits_only_the_settled_value() {
        // simulate a seating bounce: the 2-button strap flickers detached↔seated before it settles on
        // seated. Each event supersedes the prior generation, so only the FINAL stable value commits —
        // the intermediate flicker never reaches `confirm`.
        let _g = PLATE_TEST_LOCK.lock().unwrap();
        commit_plate_debounced(0, "detached".into());
        commit_plate_debounced(1, "2-button".into());
        commit_plate_debounced(0, "detached".into());
        commit_plate_debounced(1, "2-button".into()); // settles here
        thread::sleep(PLATE_DEBOUNCE + Duration::from_millis(120));
        assert_eq!(neuron::confirm::last_plate().as_deref(), Some("2-button"));
    }

    #[test]
    fn decode_ignores_short_or_foreign_reports() {
        // the existing guards must still hold — a non-0x05 lead byte or a too-short buffer is a no-op
        // (no panic), so the new arm can't destabilize the DPI/scroll/charge decoding.
        decode(&[0x05, 0x0e], NAGA_PID); // too short (< 6) — guarded
        decode(&[0x01, 0x0e, 0x03, 0, 0, 0], NAGA_PID); // wrong lead byte — guarded
    }
}
