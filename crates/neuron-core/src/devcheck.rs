// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The in-app device check: the read-only questions `neuron compat lighting` asks, put to one
//! device, with a compact result for its row and the full text to copy or attach to a report.
//! Nothing here writes to a device.

use std::fmt::Write as _;
use std::time::Duration;

/// How one transaction id fared on one control collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxState {
    /// Nothing echoed the request.
    Silent,
    /// A lighting question was answered.
    Answered,
    /// The board refused the questions (a status reply).
    Refused,
    /// Another app's traffic, a rejected reply or an I/O error: says nothing about the board.
    Unclear,
}

/// One control collection's row in the check.
#[derive(Clone, Debug)]
pub struct CheckLine {
    /// `if3 0C/0001`, or the collection's usage alone when it names no interface.
    pub label: String,
    /// The collection the device's definition uses.
    pub in_use: bool,
    pub cells: Vec<(u8, TxState)>,
}

/// What a check found.
#[derive(Clone, Debug, Default)]
pub struct DeviceCheck {
    pub lines: Vec<CheckLine>,
    /// Facts the lamps can't carry, one short line each.
    pub notes: Vec<String>,
    /// Everything, as text to copy.
    pub text: String,
    /// The short form a device report carries.
    pub summary: String,
}

/// The questions' wait per reply; long enough for a receiver.
const WINDOW: Duration = Duration::from_millis(120);

/// Check one device by the family that speaks to it, its product id and, when known, its unit
/// (`HidDeviceInfo::instance`, the row's id), so a same-pid device of another vendor or a twin
/// isn't asked in its place. Read-only.
#[must_use]
pub fn check(dialect: &str, pid: u16, unit: &str, name: &str) -> DeviceCheck {
    let mut out = match dialect {
        "razer" => razer(pid),
        "hidpp" => hidpp(pid, unit),
        "via" => via(pid, unit),
        _ => DeviceCheck::default(),
    };
    if out.lines.is_empty() && out.notes.is_empty() {
        out.notes.push("nothing neuron can safely ask this device yet".into());
    }
    let mut text = format!("neuron {} · device check · {dialect} {pid:04x} · {name}\n", env!("CARGO_PKG_VERSION"));
    for n in &out.notes {
        let _ = writeln!(text, "note: {n}");
    }
    text.push('\n');
    text.push_str(&out.text);
    let evidence = crate::report::DeviceEvidence::gather(dialect, pid);
    if let Some(file) = &evidence.device_file {
        let _ = write!(text, "\ndefinition:\n{file}\n");
    }
    if let Some(on) = evidence.dynamic_lighting {
        let _ = writeln!(text, "\nwindows dynamic lighting: {}", if on { "on" } else { "off" });
    }
    if out.summary.is_empty() {
        out.summary = out.notes.join("
");
    }
    let private = crate::report::Private::current();
    out.text = crate::report::scrub(&text, &private);
    out.summary = crate::report::scrub(&out.summary, &private);
    out
}

impl DeviceCheck {
    /// The device report's evidence, with this check's findings in place of the kept probe.
    #[must_use]
    pub fn evidence(&self, dialect: &str, pid: u16) -> crate::report::DeviceEvidence {
        let mut e = crate::report::DeviceEvidence::gather(dialect, pid);
        if !self.summary.trim().is_empty() {
            e.probe = Some(self.summary.clone());
        }
        e
    }
}

/// A receiver's status for a device behind it that didn't answer: asleep or out of range, not a refusal.
const UNREACHABLE: u8 = 0x04;

fn tx_state(cells: &[&crate::lightprobe::Cell]) -> TxState {
    use crate::lightprobe::Outcome;
    let lit = |c: &&&crate::lightprobe::Cell| c.getter != "firmware" && c.getter != "device_mode";
    if cells.iter().filter(lit).any(|c| matches!(c.outcome, Outcome::Answered { .. })) {
        TxState::Answered
    } else if cells.iter().filter(lit).any(|c| matches!(c.outcome, Outcome::Status { status } if status != UNREACHABLE)) {
        TxState::Refused
    } else if cells.iter().filter(lit).any(|c| !matches!(c.outcome, Outcome::Silent | Outcome::Status { .. })) {
        TxState::Unclear
    } else {
        TxState::Silent
    }
}

fn razer(pid: u16) -> DeviceCheck {
    let mut out = DeviceCheck::default();
    let units = match crate::lightprobe::sweep(Some(pid), WINDOW) {
        Ok(u) => u,
        Err(e) => {
            out.notes.push(format!("couldn't list devices: {e}"));
            return out;
        }
    };
    let def = crate::registry::shared().find_by_pid(crate::dialect::RAZER_VID, pid).cloned();
    let in_use = def.as_ref().map(|d| (d.control_interface.usage_page, d.control_interface.usage, d.control_interface.interface));
    for u in &units {
        for p in &u.pipes {
            let mut txs: Vec<u8> = p.cells.iter().map(|c| c.tx).collect();
            txs.dedup();
            txs.sort_unstable();
            txs.dedup();
            let cells = txs
                .iter()
                .map(|&tx| (tx, tx_state(&p.cells.iter().filter(|c| c.tx == tx).collect::<Vec<_>>())))
                .collect();
            let label = match p.interface {
                Some(i) => format!("if{i} {:02X}/{:04X}", p.usage_page & 0xFF, p.usage),
                None => format!("{:04X}/{:04X}", p.usage_page, p.usage),
            };
            let used = in_use.is_some_and(|(page, usage, iface)| {
                (page, usage) == (p.usage_page, p.usage) && iface.is_none_or(|i| p.interface == Some(i))
            });
            out.lines.push(CheckLine { label, in_use: used, cells });
            if p.cells.iter().any(|c| matches!(c.outcome, crate::lightprobe::Outcome::CrossTalk { .. })) {
                push_once(&mut out.notes, "another app was talking to it during the check");
            }
            if !p.cells.is_empty()
                && p.cells.iter().all(|c| matches!(c.outcome, crate::lightprobe::Outcome::Status { status: UNREACHABLE }))
            {
                push_once(&mut out.notes, "its receiver answers, but the device behind it is asleep or out of range");
            }
            if p.cells.iter().any(|c| matches!(c.outcome, crate::lightprobe::Outcome::Error { .. })) {
                push_once(&mut out.notes, "some questions couldn't be sent; another app may hold it");
            }
        }
        if u.lamparray && u.dynamic_lighting.as_ref().is_some_and(|d| d.enabled) {
            push_once(&mut out.notes, "Windows Dynamic Lighting can take its lights");
        }
    }
    if units.is_empty() {
        out.notes.push("not connected".into());
    } else {
        out.summary = crate::lightprobe::summary(&units);
        // A learned device's later reports carry its latest clean probe; one taken while another
        // app talked to it, or with nothing answering, doesn't replace a better one.
        let clean = out.notes.is_empty() && out.lines.iter().any(|l| l.cells.iter().any(|c| c.1 == TxState::Answered));
        if clean && crate::synth::auto_def_path("razer", pid).exists() {
            let _ = std::fs::write(crate::lightprobe::evidence_path("razer", pid), &out.summary);
        }
    }
    out.text = crate::lightprobe::render(&units);
    out
}

fn push_once(notes: &mut Vec<String>, n: &str) {
    if !notes.iter().any(|x| x == n) {
        notes.push(n.into());
    }
}

/// The device's claimed control pipe for `dialect`, opened: on `unit` when one is named.
fn open_claimed(dialect: &str, pid: u16, unit: &str) -> Option<Box<dyn crate::transport::Transport>> {
    let infos = crate::transport::enumerate().ok()?;
    let claimed = |i: &&crate::transport::HidDeviceInfo| {
        i.pid == pid && crate::dialect::claimed_by(i).is_some_and(|d| d.id() == dialect)
    };
    // The row's id names the pipe it was built from; a row built from a sibling collection falls
    // back to the pid's first claimed pipe.
    let i = infos
        .iter()
        .filter(claimed)
        .find(|i| i.instance() == unit)
        .or_else(|| infos.iter().find(claimed))?;
    crate::transport::open_path(&i.path).ok()
}

fn hidpp(pid: u16, unit: &str) -> DeviceCheck {
    let mut out = DeviceCheck::default();
    let Some(t) = open_claimed("hidpp", pid, unit) else {
        out.notes.push("not connected".into());
        return out;
    };
    match crate::hidpp::probe_report(&*t) {
        Some(p) => {
            let mut has = Vec::new();
            if p.battery_feature.is_some() {
                has.push("battery");
            }
            if p.dpi_feature.is_some() {
                has.push("dpi");
            }
            out.notes.push(format!("HID++ {}.{} · {}", p.protocol_major, p.protocol_minor, if has.is_empty() { "no battery or dpi feature".into() } else { has.join(" · ") }));
            out.text = format!("{p:#?}\n");
        }
        None => out.notes.push("no HID++ 2.0 answer; a device behind a receiver may be asleep".into()),
    }
    out
}

fn via(pid: u16, unit: &str) -> DeviceCheck {
    let mut out = DeviceCheck::default();
    let Some(t) = open_claimed("via", pid, unit) else {
        out.notes.push("not connected".into());
        return out;
    };
    match crate::via::protocol_version(&*t) {
        Some(v) => {
            let channel = crate::via::lighting_channel(&*t);
            out.notes.push(match channel {
                Some(c) => format!("VIA {v:#06x} · brightness on lighting channel {c}"),
                None => format!("VIA {v:#06x} · no lighting channel answered"),
            });
            out.text = format!("via protocol {v:#06x}\nlighting channel: {channel:?}\n");
        }
        None => out.notes.push("no VIA answer; VIA may be turned off in its firmware".into()),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lightprobe::{Cell, Outcome};

    fn cell(getter: &'static str, outcome: Outcome) -> Cell {
        Cell { getter, tx: 0x1F, outcome }
    }

    #[test]
    fn a_lighting_answer_outranks_a_refusal_and_firmware_alone_is_not_lighting() {
        let firmware = cell("firmware", Outcome::Answered { body: vec![1, 2, 3, 4] });
        let refused = cell("brightness", Outcome::Status { status: 0x05 });
        let lit = cell("effect", Outcome::Answered { body: vec![0; 4] });
        assert_eq!(tx_state(&[&firmware, &refused]), TxState::Refused);
        assert_eq!(tx_state(&[&firmware, &refused, &lit]), TxState::Answered);
        assert_eq!(tx_state(&[&firmware]), TxState::Silent);
    }

    #[test]
    fn traffic_that_says_nothing_about_the_board_is_unclear() {
        let silent = cell("brightness", Outcome::Silent);
        let other = cell("effect", Outcome::CrossTalk { class: 0x00, id: 0x86 });
        assert_eq!(tx_state(&[&silent]), TxState::Silent);
        assert_eq!(tx_state(&[&silent, &other]), TxState::Unclear);
    }

    #[test]
    fn a_receiver_whose_device_is_asleep_is_silent_not_refusing() {
        let asleep = cell("brightness", Outcome::Status { status: UNREACHABLE });
        assert_eq!(tx_state(&[&asleep, &asleep]), TxState::Silent);
    }

    #[test]
    fn a_check_with_no_device_says_so_and_names_what_it_asked() {
        let c = check("razer", 0x02a5, "", "a board");
        assert!(!c.notes.is_empty(), "a check always says something");
        assert!(c.text.contains("device check · razer 02a5 · a board"), "{}", c.text);
        let unknown = check("lumen", 0x0001, "", "a lamp");
        assert_eq!(unknown.notes, ["nothing neuron can safely ask this device yet"]);
    }
}
