// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Settling a learned keyboard's lighting: find, by writing and reading back, where its lighting
//! writes land, rewrite its generated file to match, and record the outcome.
//!
//! Synthesis can only prove reads, and a board can answer a read where it ignores a write (the
//! Chroma V2 acknowledges and drops lighting writes at a foreign tx). Two write-side checks close
//! that gap:
//!
//! 1. **Calibration**: at each candidate collection, tx and LED, nudge the brightness one step,
//!    read it back, and put it back. A board that ignored the write reads back unchanged; the
//!    first candidate that reads back the nudged level is where brightness writes land. One step
//!    of 255 on a volatile (NOSTORE) write; a board at level 0 is not nudged.
//! 2. **The first-light heal** ([`crate::writes::first_light_heal`]): `off` then the custom-frame
//!    display at each tx, landing only when the lighting state moves to the custom frame.
//!
//! Each candidate tx first takes custody (the device-mode switch every lighting write takes) at
//! that tx. Nothing else is written. The three paths keep separate transaction ids: getters ride
//! the device default (where reads answered), brightness and effects their own overrides, so
//! proving one never moves another. The outcome is kept in a sidecar record
//! (`devices/auto/<dialect>-<pid>.settle.toml`) so a unit is settled once per firmware and
//! definition, and a failure stays visible. Only generated (`Auto`) Razer keyboard definitions are
//! settled, never while writes are paused.

use crate::registry::{DefOrigin, DeviceDef};
use crate::transport::{self, HidDeviceInfo};
use anyhow::Result;
use serde::{Deserialize, Serialize};

const COHORTS: [u8; 4] = crate::dialect::RAZER_COHORTS;

/// Where a keyboard's brightness writes were seen to land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub usage_page: u16,
    pub usage: u16,
    pub interface: Option<u8>,
    pub tx: u8,
    pub led: u8,
    /// Candidates tried before this one landed (0 = the definition was already right).
    pub tried: usize,
}

/// How a settle ended. Stored in the sidecar record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The definition's lighting writes were seen to land as they are.
    Verified,
    /// They landed elsewhere, and the definition now points there.
    Moved,
    /// They landed elsewhere, but the file was edited by hand, so it was left alone.
    Proposed,
    /// The board answered, but no candidate write was seen to land.
    Unverified,
}

/// The sidecar record of the last settle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub verdict: Verdict,
    /// Firmware version bytes (00/81) when settled; a firmware change settles again.
    pub firmware: Option<[u8; 2]>,
    /// The generated file's digest when settled; a changed file settles again.
    pub file_digest: Option<String>,
    pub synth_version: u32,
    /// Seconds since the Unix epoch.
    pub when: u64,
    /// What was checked and found, one line each.
    pub notes: Vec<String>,
}

/// The outcome of [`settle`].
#[derive(Clone, Debug)]
pub struct Settled {
    /// The definition the device should now run on (equal to the input unless `Moved`).
    pub def: DeviceDef,
    pub verdict: Verdict,
    /// One line per verified change.
    pub changes: Vec<String>,
    /// Whether lighting writes went out (the caller re-applies the user's lighting after).
    pub wrote: bool,
}

fn dedup(v: impl IntoIterator<Item = u8>) -> Vec<u8> {
    let mut out = Vec::new();
    for x in v {
        if !out.contains(&x) {
            out.push(x);
        }
    }
    out
}

/// Where the settle record for a generated definition lives.
#[must_use]
pub fn record_path(dialect_id: &str, pid: u16) -> std::path::PathBuf {
    crate::synth::auto_def_path(dialect_id, pid).with_extension("settle.toml")
}

/// The last settle record for a generated definition, if any.
#[must_use]
pub fn record_for(dialect_id: &str, pid: u16) -> Option<Record> {
    toml::from_str(&std::fs::read_to_string(record_path(dialect_id, pid)).ok()?).ok()
}

/// How sure Neuron is of a definition's lighting, for a badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightingTrust {
    /// A curated or built-in definition: its bytes were checked on hardware by hand.
    Curated,
    /// Learned, and its writes were seen to land on this unit.
    Checked,
    /// Learned from probing and the keyboard atlas; not yet settled on this unit.
    Learned,
    /// Learned, and settling found its writes land elsewhere, but the file was edited by hand.
    FixWaiting,
    /// Learned, and no lighting write was seen to land.
    Unconfirmed,
}

impl LightingTrust {
    /// The lamp a device row draws: 0 none (curated, or not yet settled), 1 checked, 2 a fix
    /// waits, 3 unconfirmed.
    #[must_use]
    pub fn lamp(self) -> i32 {
        match self {
            LightingTrust::Curated | LightingTrust::Learned => 0,
            LightingTrust::Checked => 1,
            LightingTrust::FixWaiting => 2,
            LightingTrust::Unconfirmed => 3,
        }
    }
}

/// How sure Neuron is of `def`'s lighting, from its origin and its settle record. `None` without
/// lighting.
#[must_use]
pub fn lighting_trust(def: &DeviceDef) -> Option<LightingTrust> {
    def.lighting.as_ref()?;
    if def.origin != DefOrigin::Auto {
        return Some(LightingTrust::Curated);
    }
    let pid = def.modes.first()?.product_id;
    Some(match record_for(&def.dialect, pid).map(|r| r.verdict) {
        Some(Verdict::Verified | Verdict::Moved) => LightingTrust::Checked,
        Some(Verdict::Proposed) => LightingTrust::FixWaiting,
        Some(Verdict::Unverified) => LightingTrust::Unconfirmed,
        None => LightingTrust::Learned,
    })
}

fn file_digest(dialect_id: &str, pid: u16) -> Option<String> {
    let text = std::fs::read_to_string(crate::synth::auto_def_path(dialect_id, pid)).ok()?;
    text.lines().find_map(|l| l.strip_prefix("# synth-digest: ")).map(|d| d.trim().to_string())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The result of one nudge.
enum Nudge {
    Landed,
    Ignored,
    /// The level read 0: a one-step nudge would light a dark board, so it is not attempted.
    Dark,
}

/// Read brightness at `led`: at `tx`, then at the device default, where reads answered during
/// synthesis (a board can read at one tx and take writes at another).
fn read_level(d: &crate::device::Device, tx: u8, led: u8) -> Option<u8> {
    d.exec_dynamic_tx(tx, 0x0F, 0x84, 0x03, &[0x00, led])
        .or_else(|_| d.exec_dynamic_tx(d.def.transaction_id, 0x0F, 0x84, 0x03, &[0x00, led]))
        .ok()
        .filter(|a| a[1] == led)
        .map(|a| a[2])
}

/// Nudge brightness at `(tx, led)` and read it back. Once a nudge write has gone out, the original
/// level is always written back, whatever the reply.
fn nudge(d: &crate::device::Device, tx: u8, led: u8) -> Nudge {
    let Some(level) = read_level(d, tx, led) else {
        return Nudge::Ignored;
    };
    if level == 0 {
        return Nudge::Dark;
    }
    let probe = level - 1;
    let sent = d.exec_dynamic_tx(tx, 0x0F, 0x04, 0x03, &[0x00, led, probe]);
    let landed = sent.is_ok() && read_level(d, tx, led) == Some(probe);
    let _ = d.exec_dynamic_tx(tx, 0x0F, 0x04, 0x03, &[0x00, led, level]);
    if landed { Nudge::Landed } else { Nudge::Ignored }
}

/// The unit's claimed collections of this dialect, the definition's own first. `unit` is one of
/// the unit's [`HidDeviceInfo::instance`] strings (the app's identity); collections are grouped by
/// [`transport::unit_key`], since Windows gives every collection its own instance. An identity no
/// enumerated collection carries falls back to the PID alone.
fn unit_pipes<'a>(def: &DeviceDef, infos: &'a [HidDeviceInfo], pid: u16, unit: &str) -> Vec<&'a HidDeviceInfo> {
    let claimed = |i: &&HidDeviceInfo| {
        i.pid == pid && crate::dialect::claimed_by(i).is_some_and(|d| d.id() == def.dialect)
    };
    let key = infos.iter().find(|i| i.pid == pid && i.instance() == unit).map(transport::unit_key);
    let mut pipes: Vec<&HidDeviceInfo> = match &key {
        Some(k) => infos.iter().filter(claimed).filter(|i| transport::unit_key(i) == *k).collect(),
        None => infos.iter().filter(claimed).collect(),
    };
    pipes.sort_by_key(|i| !def.matches_control(i));
    pipes
}

/// Is this definition's lighting the keyboard block calibration knows how to nudge?
fn calibratable(def: &DeviceDef) -> bool {
    let Some(l) = def.lighting.as_ref() else { return false };
    l.protocol == crate::lighting::Protocol::Matrix
        && l.framing == crate::lighting::Framing::Fixed
        && l.brightness.as_ref().is_some_and(|b| b.args.len() == 2)
        && !def.commands.contains_key("dpi")
}

/// Calibrate a keyboard's brightness path. `None` when the definition is not calibratable, writes
/// are paused, or no candidate landed. The second value says whether any nudge write went out and
/// whether the board read dark.
#[must_use]
pub fn calibrate(def: &DeviceDef, pipes: &[&HidDeviceInfo], pid: u16) -> (Option<Calibration>, CalibrationTrace) {
    let mut trace = CalibrationTrace::default();
    if !calibratable(def) {
        return (None, trace);
    }
    let l = def.lighting.as_ref().expect("calibratable");
    let b = l.brightness.as_ref().expect("calibratable");
    let current_tx = b.transaction_id.unwrap_or(def.transaction_id);
    let atlas_leds = crate::atlas::lookup(def.vendor_id, pid).map(|k| k.led().candidates()).unwrap_or_default();
    let leds = dedup([b.args[1]].into_iter().chain(atlas_leds).chain([0x05]));
    let txs = dedup([current_tx, def.transaction_id].into_iter().chain(COHORTS));
    for pipe in pipes {
        let Ok(d) = crate::device::Device::open_path(def.clone(), pid, &pipe.path) else {
            continue;
        };
        // A collection that answers no lighting read at any candidate is not where lighting lives;
        // it is sent nothing, not even a mode switch.
        if !txs.iter().any(|&tx| leds.iter().any(|&led| read_level(&d, tx, led).is_some())) {
            continue;
        }
        for &tx in &txs {
            if crate::writes::writes_paused() {
                return (None, trace);
            }
            crate::writes::ensure_custody_at(&d, tx);
            for &led in &leds {
                match nudge(&d, tx, led) {
                    Nudge::Landed => {
                        trace.wrote = true;
                        return (
                            Some(Calibration {
                                usage_page: pipe.usage_page,
                                usage: pipe.usage,
                                interface: pipe.interface(),
                                tx,
                                led,
                                tried: trace.tried,
                            }),
                            trace,
                        );
                    }
                    Nudge::Dark => trace.dark = true,
                    Nudge::Ignored => {}
                }
                trace.tried += 1;
            }
        }
    }
    (None, trace)
}

/// What a calibration attempt did, for the record.
#[derive(Clone, Copy, Debug, Default)]
pub struct CalibrationTrace {
    pub tried: usize,
    pub dark: bool,
    pub wrote: bool,
}

/// `def` with brightness aimed where `cal` proved writes land: the LED in every lighting command,
/// the brightness setter's own tx override (none when it is the device default), and the
/// collection. An interface is pinned only when the collection shape repeats on another interface
/// of the unit.
#[must_use]
pub fn with_calibration(def: &DeviceDef, cal: &Calibration, twin_shapes: bool) -> DeviceDef {
    let mut d = def.clone();
    d.control_interface.usage_page = cal.usage_page;
    d.control_interface.usage = cal.usage;
    d.control_interface.interface = if twin_shapes { cal.interface } else { None };
    let brightness_tx = (cal.tx != d.transaction_id).then_some(cal.tx);
    if let Some(l) = d.lighting.as_mut() {
        l.led_id = cal.led;
        if l.effect.args.len() == 2 {
            l.effect.args[1] = cal.led;
        }
        if let Some(b) = l.brightness.as_mut() {
            b.args = vec![0x00, cal.led];
            b.transaction_id = brightness_tx;
        }
    }
    // The getter keeps the device default: reads answer where they answered during synthesis.
    if let Some(c) = d.commands.get_mut("brightness") {
        if c.class == 0x0F {
            c.args = vec![0x00, cal.led];
        }
    }
    d
}

/// Calibrate, then heal, a learned keyboard's lighting; rewrite its generated file once if either
/// moved it, and keep the outcome in the settle record. `unit` is the physical unit's
/// [`HidDeviceInfo::instance`] (empty = any unit of `pid`). `Ok(None)` when there is nothing to
/// settle (not a generated Razer definition, no lighting, writes paused, the board unreachable),
/// or when the record already covers this firmware and file.
pub fn settle(def: &DeviceDef, pid: u16, unit: &str) -> Result<Option<Settled>> {
    if crate::writes::writes_paused() || def.origin != DefOrigin::Auto || def.lighting.is_none() || def.dialect != "razer" {
        return Ok(None);
    }
    let infos = transport::enumerate()?;
    let pipes = unit_pipes(def, &infos, pid, unit);
    let Some(first) = pipes.first() else {
        return Ok(None);
    };
    let firmware = crate::device::Device::open_path(def.clone(), pid, &first.path)
        .ok()
        .and_then(|d| d.exec_dynamic(0x00, 0x81, 0x02, &[]).ok())
        .map(|a| [a[0], a[1]]);
    let Some(firmware) = firmware else {
        return Ok(None);
    };
    let digest = file_digest(&def.dialect, pid);
    if let Some(r) = record_for(&def.dialect, pid) {
        if r.firmware == Some(firmware) && r.file_digest == digest && r.synth_version == crate::synth::SYNTH_VERSION {
            return Ok(None);
        }
    }

    let mut current = def.clone();
    let mut changes = Vec::new();
    let mut notes = Vec::new();
    let (cal, trace) = calibrate(&current, &pipes, pid);
    let mut wrote = trace.wrote;
    match &cal {
        Some(cal) => {
            let twins = pipes.iter().filter(|p| (p.usage_page, p.usage) == (cal.usage_page, cal.usage)).count() > 1;
            let next = with_calibration(&current, cal, twins);
            let line = format!(
                "#   - brightness: HARDWARE-VERIFIED by calibration (a one-step write read back at tx 0x{:02X}, LED 0x{:02X}, collection {:04X}/{:04X}{}).",
                cal.tx,
                cal.led,
                cal.usage_page,
                cal.usage,
                cal.interface.map_or(String::new(), |i| format!(" on interface {i}"))
            );
            notes.push(line.clone());
            if next != current {
                changes.push(line);
                current = next;
            }
        }
        None if calibratable(&current) => notes.push(format!(
            "brightness: no one-step write read back after {} candidates{}",
            trace.tried,
            if trace.dark { " (the board read brightness 0, so it was not nudged)" } else { "" }
        )),
        None => {}
    }

    let control = pipes.iter().find(|p| current.matches_control(p)).copied();
    let mut effects_landed = false;
    if let Some(pipe) = control {
        if let Ok(d) = crate::device::Device::open_path(current.clone(), pid, &pipe.path) {
            let lighting_tx = current.lighting_tx().unwrap_or(current.transaction_id);
            wrote |= current.command("lighting_state").is_some();
            match crate::writes::first_light_heal(&d, &current) {
                Some(tx) => {
                    effects_landed = true;
                    let line = format!(
                        "#   - lighting effects: HARDWARE-VERIFIED by first-light heal (state moved to the custom frame after off + display at tx 0x{tx:02X})."
                    );
                    notes.push(line.clone());
                    if tx != lighting_tx {
                        changes.push(line);
                        current = crate::synth::with_lighting_tx(&current, tx);
                    }
                }
                None => notes.push("effects: the custom-frame display was not seen to land at any tx".into()),
            }
        }
    }

    let mut verdict = if cal.is_some() || effects_landed {
        if changes.is_empty() { Verdict::Verified } else { Verdict::Moved }
    } else {
        Verdict::Unverified
    };
    if !changes.is_empty() {
        let resolves = infos.iter().any(|i| i.pid == pid && current.matches_control(i));
        if !resolves {
            notes.push("the corrected definition matched no enumerated collection; the file was left alone".into());
            verdict = Verdict::Unverified;
            current = def.clone();
        } else if crate::synth::rewrite_auto(&def.dialect, pid, &current, &changes).is_err() {
            verdict = Verdict::Proposed;
            notes.extend(changes.iter().map(|c| format!("proposed, not applied: {}", c.trim_start_matches("#   - "))));
            current = def.clone();
        }
    }
    let record = Record {
        verdict: verdict.clone(),
        firmware: Some(firmware),
        file_digest: file_digest(&def.dialect, pid),
        synth_version: crate::synth::SYNTH_VERSION,
        when: now_secs(),
        notes,
    };
    if let Ok(text) = toml::to_string(&record) {
        let _ = std::fs::write(record_path(&def.dialect, pid), text);
    }
    Ok(Some(Settled { def: current, verdict, changes, wrote }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::firmware::{Behaviour, Board, FirmwareBackend, Refusal, Role, Scope, Unit};
    use std::sync::{Arc, PoisonError};

    fn learned_v3(backend: &mut FirmwareBackend, b: Behaviour) -> Arc<Board> {
        backend.with(
            Unit::new(0x024E, "Phantom V3", "cal", b)
                .collection(0, 0x0001, 0x0006, 0, Role::Mute)
                .collection(1, 0x0001, 0x0002, 91, Role::IdentityOnly)
                .collection(3, 0x000C, 0x0001, 91, Role::Control),
        )
    }

    fn adopted(pid: u16) -> DeviceDef {
        crate::synth::adopt_unknown(&crate::registry::Registry::load().unwrap()).unwrap();
        crate::registry::Registry::load().unwrap().find_by_pid(0x1532, pid).unwrap().clone()
    }

    /// Reads answer at any LED, writes land only on LED 0x00: synthesis takes the first LED that
    /// reads (0x05), and calibration moves the board to the LED its writes land on.
    #[test]
    fn calibration_finds_the_led_writes_land_on() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let mut be = FirmwareBackend::new();
        let mut b = Behaviour::extended(&[0x3F], 6, 22);
        b.leds = vec![0x00];
        b.lenient_led_reads = true;
        let board = learned_v3(&mut be, b);
        let _bus = transport::install_backend(Arc::new(be));
        let mut def = adopted(0x024E);
        assert_eq!(def.lighting.as_ref().unwrap().led_id, 0x05, "reads answered at the first LED asked");
        // The atlas names only 0x05 for the V3; this board is the case where it is wrong.
        def.lighting.as_mut().unwrap().brightness.as_mut().unwrap().args = vec![0x00, 0x00];

        let settled = settle(&def, 0x024E, "").unwrap().expect("settled");
        assert_eq!(settled.verdict, Verdict::Moved);
        let l = settled.def.lighting.as_ref().unwrap();
        assert_eq!((l.led_id, &l.effect.args[..]), (0x00, &[0x00, 0x00][..]));
        assert_eq!(board.brightness(0x00), Some(0xFF), "the nudge was put back");
        assert!(board.decoy_writes().is_empty(), "nothing written to the decoy collection");
        let reloaded = crate::registry::Registry::load().unwrap().find_by_pid(0x1532, 0x024E).unwrap().clone();
        assert_eq!(reloaded.lighting, settled.def.lighting, "the file was rewritten");
        let text = std::fs::read_to_string(crate::synth::auto_def_path("razer", 0x024E)).unwrap();
        assert!(text.contains("by calibration") && text.contains("How the lighting bytes were chosen"), "{text}");
        assert!(text.contains("lighting tx 0x3F"), "synthesis's provenance survives the rewrite: {text}");
        assert_eq!(record_for("razer", 0x024E).unwrap().verdict, Verdict::Moved);
    }

    /// Reads answer at every tx, writes land only at 0x3F: calibration and the heal find it, the
    /// device default (where reads answered) stays, and brightness and effects get overrides.
    #[test]
    fn writes_held_to_a_tx_move_only_the_write_paths() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let mut be = FirmwareBackend::new();
        let mut b = Behaviour::extended(&[0x3F], 6, 22);
        b.scope = Scope::Writes;
        b.foreign_write = Refusal::AckIgnore;
        learned_v3(&mut be, b);
        let _bus = transport::install_backend(Arc::new(be));
        let mut def = adopted(0x024E);
        def.transaction_id = 0x1F;
        let settled = settle(&def, 0x024E, "").unwrap().expect("settled");
        assert_eq!(settled.verdict, Verdict::Moved);
        assert_eq!(settled.def.transaction_id, 0x1F, "getters stay where they answered");
        let l = settled.def.lighting.as_ref().unwrap();
        assert_eq!(l.effect.transaction_id, Some(0x3F));
        assert_eq!(l.custom_frame.as_ref().unwrap().transaction_id, Some(0x3F));
        assert_eq!(l.brightness.as_ref().unwrap().transaction_id, Some(0x3F));
        assert_eq!(settled.def.command("brightness").unwrap().transaction_id, None, "the getter reads at the default");
    }

    #[test]
    fn a_right_definition_is_verified_recorded_and_not_settled_again() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let mut be = FirmwareBackend::new();
        let board = learned_v3(&mut be, Behaviour::extended(&[0x3F], 6, 22));
        let _bus = transport::install_backend(Arc::new(be));
        let def = adopted(0x024E);
        let before = std::fs::read_to_string(crate::synth::auto_def_path("razer", 0x024E)).unwrap();
        let settled = settle(&def, 0x024E, "").unwrap().expect("settled");
        assert_eq!(settled.verdict, Verdict::Verified);
        assert!(settled.changes.is_empty() && settled.wrote);
        assert_eq!(std::fs::read_to_string(crate::synth::auto_def_path("razer", 0x024E)).unwrap(), before);
        let writes = board.applied().len();
        assert!(settle(&def, 0x024E, "").unwrap().is_none(), "the record covers this firmware and file");
        assert_eq!(board.applied().len(), writes, "and nothing was written the second time");
    }

    /// Nothing lands anywhere: the verdict says so, the file is untouched, and brightness is back.
    #[test]
    fn a_board_that_takes_no_write_is_recorded_unverified() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let mut be = FirmwareBackend::new();
        let mut b = Behaviour::extended(&[0x3F], 6, 22);
        b.scope = Scope::Writes;
        b.cohort = vec![0x42];
        let board = learned_v3(&mut be, b);
        let _bus = transport::install_backend(Arc::new(be));
        let def = adopted(0x024E);
        let before = std::fs::read_to_string(crate::synth::auto_def_path("razer", 0x024E)).unwrap();
        let settled = settle(&def, 0x024E, "").unwrap().expect("settled");
        assert_eq!(settled.verdict, Verdict::Unverified);
        assert_eq!(std::fs::read_to_string(crate::synth::auto_def_path("razer", 0x024E)).unwrap(), before);
        assert_eq!(board.brightness(0x05), Some(0xFF));
        let record = record_for("razer", 0x024E).unwrap();
        assert!(record.notes.iter().any(|n| n.contains("no one-step write")), "{:?}", record.notes);
    }

    /// A file edited by hand is never rewritten; the correction is recorded as a proposal.
    #[test]
    fn a_hand_edited_file_gets_a_proposal_not_a_rewrite() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let mut be = FirmwareBackend::new();
        let mut b = Behaviour::extended(&[0x3F], 6, 22);
        b.scope = Scope::Writes;
        b.foreign_write = Refusal::AckIgnore;
        learned_v3(&mut be, b);
        let _bus = transport::install_backend(Arc::new(be));
        let path = crate::synth::auto_def_path("razer", 0x024E);
        let mut def = adopted(0x024E);
        let edited = std::fs::read_to_string(&path).unwrap().replace("transaction_id = 0x3F\n", "transaction_id = 0x1F\n");
        std::fs::write(&path, &edited).unwrap();
        crate::registry::invalidate();
        def.transaction_id = 0x1F;
        let settled = settle(&def, 0x024E, "").unwrap().expect("settled");
        assert_eq!(settled.verdict, Verdict::Proposed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited, "the person's edit stands");
        assert!(record_for("razer", 0x024E).unwrap().notes.iter().any(|n| n.starts_with("proposed")));
        assert_eq!(lighting_trust(&def), Some(LightingTrust::FixWaiting));
    }

    #[test]
    fn trust_follows_origin_and_the_settle_record() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _root = crate::authoring::test_run_root();
        let curated: DeviceDef = toml::from_str(crate::registry::BUILTINS[0]).unwrap();
        assert_eq!(lighting_trust(&curated), Some(LightingTrust::Curated));
        assert_eq!(LightingTrust::Curated.lamp(), 0);
        let mut be = FirmwareBackend::new();
        learned_v3(&mut be, Behaviour::extended(&[0x3F], 6, 22));
        let _bus = transport::install_backend(Arc::new(be));
        let def = adopted(0x024E);
        assert_eq!(lighting_trust(&def), Some(LightingTrust::Learned), "not settled yet");
        settle(&def, 0x024E, "").unwrap();
        assert_eq!(lighting_trust(&def), Some(LightingTrust::Checked));
        assert_eq!(LightingTrust::Checked.lamp(), 1);
    }

    /// A pin from synthesis that the calibrated collection does not share is dropped, so the
    /// definition can never point at a collection that does not exist.
    #[test]
    fn calibration_never_keeps_a_pin_for_another_collection() {
        let def: DeviceDef = toml::from_str(crate::registry::BUILTINS[0]).unwrap();
        let mut def = def;
        def.control_interface.interface = Some(1);
        let cal = Calibration { usage_page: 0x000C, usage: 0x0001, interface: Some(3), tx: def.transaction_id, led: 0x05, tried: 0 };
        let moved = with_calibration(&def, &cal, false);
        assert_eq!(moved.control_interface.interface, None);
        assert_eq!((moved.control_interface.usage_page, moved.control_interface.usage), (0x000C, 0x0001));
        assert_eq!(with_calibration(&def, &cal, true).control_interface.interface, Some(3));
    }
}
