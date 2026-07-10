//! Razer macro-key input — native protocol talk, capability-driven, EMERGENT.
//!
//! A Razer keyboard in "Driver Mode" stops handling its dedicated macro keys onboard and instead
//! PUSHES a vendor HID input report on a readable sibling collection (the generic-desktop `u=0x0000`
//! collection — the same place a Razer mouse's event reports ride): report id `0x04`, 16 bytes,
//! carrying the ARRAY of currently-held macro-key codes (`0x20`=M1, `0x21`=M2 … `0x01`=FN,
//! `0x00`=released). Captured live from a BlackWidow Chroma V2 and matching OpenRazer's
//! `razer_raw_event` — the `0x04` report is a Razer PROTOCOL constant, not a per-board fact.
//!
//! We read that report and inject each held code as a bindable `(RAZER_MACRO_PAGE, code)` control via
//! `controls::inject_event`, so the macro keys bind + dispatch exactly like any other control. The
//! design is CAPABILITY-DRIVEN and EMERGENT — nothing hardcoded to one keyboard:
//!   * device scope = Razer devices that speak `device_mode` and are NOT mice (mice are hidwatch's),
//!     so any Razer keyboard qualifies with no PID list;
//!   * Driver Mode is enabled through the registry `device_mode` capability;
//!   * the bindable key set is *whatever codes the board reports* — 3 macro keys yield 3 controls,
//!     10 yield 10, with no table to edit.
//!
//! `NEURON_HIDWATCH=1` logs discovery + every decoded macro report (shared with hidwatch's verbosity).

use neuron::registry::Capability;
use neuron::transport::DevicePath;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

/// How often to re-enumerate (catch a keyboard replug) and re-assert Driver Mode (a replug reverts).
const HOTPLUG_POLL: Duration = Duration::from_secs(20);
/// Driver Mode — the firmware hands its macro keys to the host (they emit the `0x04` report).
/// The switch itself routes through `neuron::writes::set_device_mode` (the one home of the opcode).
const DRIVER_MODE: u8 = 0x03;

fn verbose() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| std::env::var("NEURON_HIDWATCH").ok().as_deref() == Some("1"))
}

fn registry() -> Option<&'static neuron::registry::Registry> {
    static R: OnceLock<Option<neuron::registry::Registry>> = OnceLock::new();
    R.get_or_init(|| neuron::registry::Registry::load().ok()).as_ref()
}

/// Arm the macro-key reader: put every connected Razer keyboard into Driver Mode, then read its
/// macro report and inject the keys. A hotplug monitor re-arms + re-asserts Driver Mode after a
/// replug. Never fails the app.
pub fn start() {
    let Some(reg) = registry() else {
        if verbose() {
            eprintln!("[macrokeys] registry load failed; not listening");
        }
        return;
    };
    // Mice belong to hidwatch (it reads the SAME vendor collection for DPI/scroll). Macro keys ride
    // keyboards: Razer devices that speak `device_mode` but aren't DPI mice — capability-driven scope.
    let mouse_pids: HashSet<u16> = reg
        .devices
        .iter()
        .filter(|d| d.supports(Capability::Dpi))
        .flat_map(|d| d.product_ids())
        .collect();

    ensure_driver_mode(reg, &mouse_pids);

    let armed: Arc<Mutex<HashSet<DevicePath>>> = Arc::new(Mutex::new(HashSet::new()));
    arm_new(&mouse_pids, &armed);

    let mon = armed.clone();
    let mice = mouse_pids.clone();
    thread::Builder::new()
        .name("neuron-macrokeys-mon".into())
        .spawn(move || loop {
            thread::sleep(HOTPLUG_POLL);
            if let Some(reg) = registry() {
                ensure_driver_mode(reg, &mice); // a replug reverts to onboard — re-assert
            }
            arm_new(&mice, &mon);
        })
        .ok();
}

/// Put every connected Razer keyboard (speaks `device_mode`, not a mouse) into Driver Mode so its
/// macro keys emit the `0x04` report. Idempotent — already-on is a harmless no-op. Boards lacking
/// the command (or not connected) are skipped; a failure never aborts.
fn ensure_driver_mode(reg: &neuron::registry::Registry, mouse_pids: &HashSet<u16>) {
    for def in &reg.devices {
        if !def.has_command("device_mode") || def.supports(Capability::Dpi) {
            continue; // not a vendor-mode keyboard, or it's a mouse (hidwatch's domain)
        }
        for pid in def.product_ids() {
            if mouse_pids.contains(&pid) {
                continue;
            }
            let Ok(dev) = neuron::device::Device::open(def.clone(), pid) else {
                continue; // not connected
            };
            match neuron::writes::set_device_mode(&dev, DRIVER_MODE) {
                Ok(_) => {
                    if verbose() {
                        eprintln!("[macrokeys] {pid:04x}: Driver Mode enabled");
                    }
                }
                Err(e) => {
                    if verbose() {
                        eprintln!("[macrokeys] {pid:04x}: driver-mode set failed ({e})");
                    }
                }
            }
        }
    }
}

/// Enumerate and spawn a reader for any readable, event-carrying collection of each connected Razer
/// KEYBOARD not already armed — the same vendor-collection signature hidwatch uses for mice.
fn arm_new(mouse_pids: &HashSet<u16>, armed: &Arc<Mutex<HashSet<DevicePath>>>) {
    let infos = match neuron::transport::enumerate() {
        Ok(v) => v,
        Err(_) => return,
    };
    for info in infos {
        if info.vid != neuron::synth::RAZER_VID || mouse_pids.contains(&info.pid) {
            continue; // non-Razer, or a mouse (hidwatch owns those collections)
        }
        // Where Razer's vendor input rides: the sibling generic-desktop collection with the undefined
        // usage (`u=0x0000`), or a vendor page. The OS-protected keyboard/mouse + the media collection
        // are not event-carrying for us (they fail to open or carry standard cooked input).
        let event_carrying =
            (info.usage_page == 0x0001 && info.usage == 0x0000) || info.usage_page >= 0xFF00;
        if !event_carrying {
            continue;
        }
        if !armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(info.path.clone()) {
            continue; // already armed
        }
        spawn_reader(info.pid, info.path.clone(), armed.clone());
    }
}

fn spawn_reader(pid: u16, path: DevicePath, armed: Arc<Mutex<HashSet<DevicePath>>>) {
    let tag = format!("pid={pid:04x}");
    thread::Builder::new()
        .name("neuron-macrokeys".into())
        .spawn(move || {
            let reader = match neuron::transport::open_reader(&path) {
                Ok(r) => r,
                Err(e) => {
                    if verbose() {
                        eprintln!("[macrokeys] {tag}: not readable ({e})");
                    }
                    armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&path); // let the monitor retry later
                    return;
                }
            };
            if verbose() {
                eprintln!("[macrokeys] LISTENING {tag}");
            }
            let mut buf = [0u8; 64];
            loop {
                match reader.read(&mut buf) {
                    Ok(n) if n > 0 => decode(&buf[..n], pid),
                    Ok(_) => {} // zero-length read — keep listening
                    Err(_) => {
                        armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&path); // unplugged — monitor re-arms on replug
                        if verbose() {
                            eprintln!("[macrokeys] {tag}: closed");
                        }
                        return;
                    }
                }
            }
        })
        .ok();
}

/// Decode a Razer macro report and inject the held keys. The report is `04 <code>* 00*` — id `0x04`
/// then the ARRAY of currently-held macro-key codes. Every non-zero code is a held control; an
/// all-released report (`04 00 …`) yields an empty set, so the edge detector raises the Up edges.
fn decode(buf: &[u8], pid: u16) {
    if buf.first() != Some(&0x04) {
        return; // not a macro report (a different vendor report may share this collection)
    }
    let hits: Vec<(u16, u16)> = buf[1..]
        .iter()
        .copied()
        .filter(|&c| c != 0)
        .map(|c| (neuron::controls::RAZER_MACRO_PAGE, c as u16))
        .collect();
    if verbose() {
        eprintln!("[macrokeys] pid={pid:04x} macro hits={hits:02x?}");
    }
    // Bridge the macro keys into the lighting live-input scan (reactive/heat/ripple/comet). They are NOT
    // Windows VKs, so `GetAsyncKeyState` can't see them — we publish their held-state mask (bit i = M(i+1)
    // held) for `capture::macro_key_down`, mirroring how `key_down` exposes VK held-state. Set
    // UNCONDITIONALLY: an all-released report yields mask 0, propagating the RELEASE so the next press
    // re-detects (shared held-state, not a consume-once edge that one consumer would drain from another).
    let mut macro_mask = 0u8;
    for &code in &buf[1..] {
        if let Some(i) = neuron::lighting::macro_code_index(code) {
            macro_mask |= 1 << i;
        }
    }
    neuron::capture::set_macro_held(macro_mask);
    // Synthetic edge-bucket pid (high range, never a real Razer PID < 0x1000) so the macro stream
    // gets its OWN HoldEdges bucket — it must NOT be diffed against the same keyboard's STANDARD
    // keys, which arrive via Raw Input under the real PID. Macro binds are device-ANY (capture.rs
    // drops this pid), so it's runtime-only — never persisted — and only needs to be locally distinct.
    debug_assert!(pid < 0x1000, "Razer pid {pid:#06x} would alias the 0xF000 macro-bucket prefix");
    neuron::controls::inject_event(neuron::controls::ControlEvent {
        pid: format!("{:04x}", 0xF000u16 | (pid & 0x0FFF)),
        hits,
        raw: buf.to_vec(),
    });
}
