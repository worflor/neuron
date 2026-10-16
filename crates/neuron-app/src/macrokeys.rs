// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Razer deferred macro-key input, routed through `controls::inject_event`.
//!
//! The `0x04` report carries held vendor-key codes (`0x20`=M1, `0x01`=FN, zero=released),
//! captured on the BlackWidow Chroma V2. Eligibility follows the shared reloadable registry:
//! Razer non-DPI definitions with device-mode support and driver custody; mouse aliases are excluded.
//! Collection-held sets are combined per canonical device, and the lighting mask spans all readers, so a
//! disconnect or ownership change releases only that source. Other keyboards remain unverified.
//! `NEURON_HIDWATCH=1` logs discovery and decoded reports.

use neuron::transport::DevicePath;
use neuron::registry::CanonicalPid;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    crate::hidwatch::registry()
}

#[derive(Default)]
struct MacroInput {
    allowed: BTreeMap<u16, CanonicalPid>,
    held: HashMap<DevicePath, (u16, CanonicalPid, Vec<u8>)>,
}

struct MacroUpdate {
    pid: CanonicalPid,
    codes: Vec<u8>,
    mask: u8,
    raw: Vec<u8>,
}

impl MacroInput {
    fn set_allowed(&mut self, allowed: BTreeMap<u16, CanonicalPid>) -> Vec<MacroUpdate> {
        self.allowed = allowed;
        let mut revoked = BTreeSet::new();
        self.held.retain(|_, (raw, pid, _)| {
            if self.allowed.get(raw) == Some(pid) { true } else { revoked.insert(*pid); false }
        });
        revoked.into_iter().map(|pid| self.update(pid, Vec::new())).collect()
    }

    fn accept(&mut self, path: &DevicePath, pid: u16, buf: &[u8]) -> Option<MacroUpdate> {
        let canonical = *self.allowed.get(&pid)?;
        if buf.first() != Some(&0x04) { return None }
        let codes: Vec<u8> = buf[1..].iter().copied().filter(|&c| c != 0).collect();
        if codes.is_empty() { self.held.remove(path); }
        else { self.held.insert(path.clone(), (pid, canonical, codes)); }
        Some(self.update(canonical, buf.to_vec()))
    }

    fn release(&mut self, path: &DevicePath) -> Option<MacroUpdate> {
        let (_, pid, _) = self.held.remove(path)?;
        Some(self.update(pid, Vec::new()))
    }

    fn update(&self, pid: CanonicalPid, raw: Vec<u8>) -> MacroUpdate {
        let mut codes: Vec<u8> = self.held.values().filter(|(_, source, _)| *source == pid)
            .flat_map(|(_, _, codes)| codes.iter().copied()).collect();
        codes.sort_unstable();
        codes.dedup();
        let mask = self.held.values().flat_map(|(_, _, codes)| codes.iter())
            .filter_map(|&code| neuron::lighting::macro_code_index(code))
            .fold(0, |mask, i| mask | (1 << i));
        MacroUpdate { pid, codes, mask, raw }
    }
}

/// Arm the macro-key reader: put every connected Razer keyboard into Driver Mode, then read its
/// macro report and inject the keys. A hotplug monitor re-arms + re-asserts Driver Mode after a
/// replug. Never fails the app.
pub fn start() {
    let armed: Arc<Mutex<HashSet<DevicePath>>> = Arc::new(Mutex::new(HashSet::new()));
    let input = Arc::new(Mutex::new(MacroInput::default()));
    refresh(&input, &armed);

    crate::worker::spawn_detached("neuron-macrokeys-mon", move || loop {
        thread::sleep(HOTPLUG_POLL);
        refresh(&input, &armed);
    });
}

fn refresh(input: &Arc<Mutex<MacroInput>>, armed: &Arc<Mutex<HashSet<DevicePath>>>) {
    let Some(reg) = registry() else { return };
    let pids = reg.razer_macro_key_pids();
    let identities = pids.iter().map(|&pid| (pid, CanonicalPid::of(pid))).collect();
    {
        let mut state = input.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Publish revocations under the same lock as reports, so no old reader can re-hold a key.
        for update in state.set_allowed(identities) { publish(update); }
    }
    ensure_driver_mode(reg, &pids);
    arm_new(&pids, armed, input);
}

/// Put every connected Razer keyboard (speaks `device_mode`, not a mouse) into Driver Mode so its
/// macro keys emit the `0x04` report. Idempotent — already-on is a harmless no-op. Boards lacking
/// the command (or not connected) are skipped; a failure never aborts.
fn ensure_driver_mode(reg: &neuron::registry::Registry, pids: &BTreeSet<u16>) {
    if neuron::writes::writes_paused() { return }
    for def in &reg.devices {
        if def.vendor_id != neuron::synth::RAZER_VID || def.dialect != "razer"
            || !def.has_command("device_mode") || def.supports(neuron::registry::Capability::Dpi)
            || def.custody != neuron::registry::Custody::Driver { continue }
        for pid in def.product_ids().filter(|pid| pids.contains(pid)) {
            let Ok(dev) = neuron::device::Device::open(def.clone(), pid) else {
                continue; // not connected
            };
            if neuron::writes::device_mode(&dev) == Some(DRIVER_MODE) { continue }
            if neuron::writes::writes_paused() { return }
            let result = neuron::writes::set_device_mode(&dev, DRIVER_MODE).and_then(|_| {
                anyhow::ensure!(neuron::writes::device_mode(&dev) == Some(DRIVER_MODE), "driver-mode read-back mismatch");
                Ok(())
            });
            match result {
                Ok(()) => {
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
fn arm_new(pids: &BTreeSet<u16>, armed: &Arc<Mutex<HashSet<DevicePath>>>, input: &Arc<Mutex<MacroInput>>) {
    let infos = match neuron::transport::enumerate() {
        Ok(v) => v,
        Err(_) => return,
    };
    for info in infos {
        if info.vid != neuron::synth::RAZER_VID || !pids.contains(&info.pid) {
            continue;
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
        spawn_reader(info.pid, info.path.clone(), armed.clone(), input.clone());
    }
}

fn spawn_reader(pid: u16, path: DevicePath, armed: Arc<Mutex<HashSet<DevicePath>>>, input: Arc<Mutex<MacroInput>>) {
    let tag = format!("pid={pid:04x}");
    // `armed` claimed this collection's path in `arm_new` before this spawn — the release below is
    // the ONE place that un-claims it (open failure, read-loop exit, spawn refusal/panic), so the
    // monitor can always retry a stranded claim instead of a collection going deaf forever.
    let release_armed = armed;
    let release_path = path.clone();
    let release_input = input.clone();
    crate::worker::spawn_guarded(
        "neuron-macrokeys",
        move || release_reader(&release_path, &release_armed, &release_input, publish),
        move || {
            // Input posture: this thread is the FIRST place a macro-key press becomes visible to us,
            // and it spends its life blocked in a HID read — so being scheduled promptly on wake is
            // the whole job, and being scheduled late puts a delay in front of everything downstream.
            neuron::timing::boost_input_thread();
            let reader = match neuron::transport::open_reader(&path) {
                Ok(r) => r,
                Err(e) => {
                    if verbose() {
                        eprintln!("[macrokeys] {tag}: not readable ({e})");
                    }
                    return; // release un-claims `path`; the monitor retries later
                }
            };
            if verbose() {
                eprintln!("[macrokeys] LISTENING {tag}");
            }
            read_reports(&*reader, pid, &path, &input, publish);
        },
    );
}

fn release_reader(path: &DevicePath, armed: &Arc<Mutex<HashSet<DevicePath>>>,
    input: &Arc<Mutex<MacroInput>>, mut emit: impl FnMut(MacroUpdate)) {
    let mut state = input.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(update) = state.release(path) { emit(update); }
    drop(state);
    armed.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(path);
}

fn read_reports(reader: &dyn neuron::transport::InputReader, pid: u16, path: &DevicePath,
    input: &Arc<Mutex<MacroInput>>, mut emit: impl FnMut(MacroUpdate)) {
    let mut buf = [0u8; 64];
    loop {
        if !input.lock().unwrap_or_else(std::sync::PoisonError::into_inner).allowed.contains_key(&pid) { return }
        match neuron::transport::classify_read(reader.read(&mut buf)) {
            neuron::transport::ReadStep::Data(n) if n > 0 => {
                let _t = neuron::latency::start(&neuron::latency::HID_DECODE);
                let mut state = input.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(update) = state.accept(path, pid, &buf[..n]) { emit(update); }
            }
            neuron::transport::ReadStep::Data(_) | neuron::transport::ReadStep::Idle => {}
            neuron::transport::ReadStep::Gone => return,
        }
    }
}

fn publish(update: MacroUpdate) {
    let hits: Vec<(u16, u16)> = update.codes.into_iter()
        .map(|c| (neuron::controls::RAZER_MACRO_PAGE, u16::from(c)))
        .collect();
    if verbose() {
        eprintln!("[macrokeys] pid={} macro hits={hits:02x?}", update.pid);
    }
    neuron::capture::set_macro_held(update.mask);
    neuron::controls::inject_event(neuron::controls::ControlEvent {
        pid: Some(update.pid),
        stream: neuron::controls::Stream::Deferred,
        hits,
        raw: update.raw,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identities(pids: &[u16]) -> BTreeMap<u16, CanonicalPid> {
        pids.iter().map(|&pid| (pid, CanonicalPid::of(pid))).collect()
    }

    #[test]
    fn revocation_during_a_read_releases_held_keys_and_drops_the_late_report() {
        struct RevokingReader {
            reads: std::sync::atomic::AtomicUsize,
            input: Arc<Mutex<MacroInput>>,
            updates: Arc<Mutex<Vec<MacroUpdate>>>,
        }
        impl neuron::transport::InputReader for RevokingReader {
            fn read(&self, buf: &mut [u8]) -> anyhow::Result<Option<usize>> {
                let read = self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if read == 1 {
                    let revoked = self.input.lock().unwrap().set_allowed(BTreeMap::new());
                    self.updates.lock().unwrap().extend(revoked);
                }
                assert!(read < 2, "a revoked reader must exit before its next read");
                buf[..2].copy_from_slice(&[0x04, 0x20 + read as u8]);
                Ok(Some(2))
            }
        }
        let input = Arc::new(Mutex::new(MacroInput::default()));
        input.lock().unwrap().set_allowed(identities(&[0x4567]));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let reader = RevokingReader { reads: std::sync::atomic::AtomicUsize::new(0), input: input.clone(), updates: updates.clone() };
        let path = DevicePath::from_str_for_tests("revoked-macro-reader");
        read_reports(&reader, 0x4567, &path, &input, |update| updates.lock().unwrap().push(update));
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].codes, [0x20]);
        assert_eq!(updates[0].mask, 1);
        assert!(updates[1].codes.is_empty());
        assert_eq!(updates[1].mask, 0);
        assert!(input.lock().unwrap().release(&path).is_none());
    }

    #[test]
    fn reader_exit_releases_its_keys_and_path_through_the_guarded_worker() {
        struct DisconnectingReader(std::sync::atomic::AtomicBool);
        impl neuron::transport::InputReader for DisconnectingReader {
            fn read(&self, buf: &mut [u8]) -> anyhow::Result<Option<usize>> {
                if self.0.swap(true, std::sync::atomic::Ordering::SeqCst) { anyhow::bail!("unplugged") }
                buf[..2].copy_from_slice(&[0x04, 0x20]);
                Ok(Some(2))
            }
        }
        let path = DevicePath::from_str_for_tests("unplugged-macro-reader");
        let armed = Arc::new(Mutex::new(HashSet::from([path.clone()])));
        let input = Arc::new(Mutex::new(MacroInput::default()));
        input.lock().unwrap().set_allowed(identities(&[0x4567]));
        let updates = Arc::new(Mutex::new(Vec::new()));
        let (done, completion) = std::sync::mpsc::channel();
        let release_path = path.clone();
        let release_armed = armed.clone();
        let release_input = input.clone();
        let release_updates = updates.clone();
        let work_input = input.clone();
        let work_updates = updates.clone();
        let work_path = path.clone();
        assert!(crate::worker::spawn_guarded("test-macro-release", move || {
            release_reader(&release_path, &release_armed, &release_input, |update| release_updates.lock().unwrap().push(update));
            let _ = done.send(());
        }, move || {
            read_reports(&DisconnectingReader(std::sync::atomic::AtomicBool::new(false)), 0x4567,
                &work_path, &work_input, |update| work_updates.lock().unwrap().push(update));
        }));
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(armed.lock().unwrap().is_empty());
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].codes, [0x20]);
        assert!(updates[1].codes.is_empty());
        assert_eq!(updates[1].mask, 0);
        assert!(input.lock().unwrap().release(&path).is_none());
    }

    #[test]
    fn releasing_one_collection_preserves_other_collections_and_keyboards() {
        let mut input = MacroInput::default();
        input.set_allowed(identities(&[0x4567, 0x4568]));
        let first = DevicePath::from_str_for_tests("macro-first");
        let sibling = DevicePath::from_str_for_tests("macro-sibling");
        let other = DevicePath::from_str_for_tests("macro-other-keyboard");
        assert!(input.accept(&first, 0x9999, &[0x04, 0x20]).is_none());
        input.accept(&first, 0x4567, &[0x04, 0x20]).unwrap();
        input.accept(&sibling, 0x4567, &[0x04, 0x21]).unwrap();
        input.accept(&other, 0x4568, &[0x04, 0x22]).unwrap();
        let release = input.release(&first).unwrap();
        assert_eq!(release.codes, [0x21]);
        assert_eq!(release.mask, 0b110);
        let release = input.release(&sibling).unwrap();
        assert!(release.codes.is_empty());
        assert_eq!(release.mask, 0b100);
        let release = input.release(&other).unwrap();
        assert!(release.codes.is_empty());
        assert_eq!(release.mask, 0);
    }

    #[test]
    fn aliases_share_held_keys_and_identity_changes_release_the_old_bucket() {
        let mut input = MacroInput::default();
        let canonical = CanonicalPid::of(0x4570);
        input.set_allowed(BTreeMap::from([(0x4570, canonical), (0x4571, canonical)]));
        let first = DevicePath::from_str_for_tests("macro-link");
        let alias = DevicePath::from_str_for_tests("macro-alias");
        input.accept(&first, 0x4570, &[0x04, 0x20]).unwrap();
        let update = input.accept(&alias, 0x4571, &[0x04, 0]).unwrap();
        assert_eq!(update.pid, canonical);
        assert_eq!(update.codes, [0x20]);
        input.accept(&alias, 0x4571, &[0x04, 0x21]).unwrap();
        let revoked = input.set_allowed(BTreeMap::from([(0x4571, canonical)]));
        assert_eq!(revoked.len(), 1);
        assert_eq!(revoked[0].codes, [0x21]);
        assert_eq!(revoked[0].mask, 0b10);
        let changed = input.set_allowed(identities(&[0x4571]));
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].pid, canonical);
        assert!(changed[0].codes.is_empty());
        assert_eq!(changed[0].mask, 0);
    }

    #[test]
    fn adopted_keyboard_reaches_the_macro_registry_without_restart() {
        // Adoption extends process-lifetime identities; keep fixture PIDs out of other tests.
        const CHILD: &str = "NEURON_TEST_MACRO_ADOPTION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "macrokeys::tests::adopted_keyboard_reaches_the_macro_registry_without_restart", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return;
        }
        let _guard = crate::testsupport::cwd_guard("macro-adoption");
        crate::hidwatch::reload_registry(neuron::registry::Registry::load().unwrap());
        let adopted_pid = 0x4567;
        assert!(registry().unwrap().find_by_pid(neuron::synth::RAZER_VID, adopted_pid).is_none());

        let definition = include_str!("../../neuron-core/devices/razer-blackwidow-chroma-v2.toml")
            .replace("product_id = 0x0221", "product_id = 0x4567")
            .replace("[[modes]]", "event_alias_pids = [0x4569]\n\n[[modes]]");
        let auto_dir = neuron::runroot::run_root().join("devices/auto");
        std::fs::create_dir_all(&auto_dir).unwrap();
        std::fs::write(auto_dir.join("razer-4567.toml"), definition).unwrap();
        crate::hidwatch::reload_registry(neuron::registry::Registry::load().unwrap());

        assert!(registry().unwrap().find_by_pid(neuron::synth::RAZER_VID, adopted_pid).is_some(),
            "macro-key discovery must see a completed background adoption");
        assert!(registry().unwrap().razer_macro_key_pids().contains(&adopted_pid));
        assert_eq!(CanonicalPid::of(0x4569), CanonicalPid::of(adopted_pid));
    }
}
