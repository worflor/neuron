// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The lighting probe: ask every Razer control collection of a unit the lighting getters, at every
//! transaction-id cohort, and record exactly what came back.
//!
//! This is the evidence a "detected but dark" report needs. It separates the failure modes that
//! look identical from outside: a collection that answers identity getters but not lighting, a
//! board that answers lighting only at one cohort, an LED the board lacks, a reply whose tx or
//! CRC the validator rejects. Read-only by construction: every request is a getter (id >= 0x80,
//! enforced by a test), sent under the pipe's wire lock, with no serial read and no path in the
//! report. Compatibility Lab recipe `razer-lighting-getters-v1`.

use crate::protocol::{crc, Report, BUF_LEN};
use crate::transport::{self, HidDeviceInfo, Transport};
use anyhow::Result;
use serde::Serialize;
use std::time::Duration;

/// Recipe id, for reports and issue text.
pub const RECIPE: &str = "razer-lighting-getters-v1";

/// The transaction ids asked, in order.
pub const COHORTS: [u8; 4] = crate::dialect::RAZER_COHORTS;

/// One read-only question.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Getter {
    pub name: &'static str,
    pub class: u8,
    pub id: u8,
    pub size: u8,
    pub args: &'static [u8],
}

/// The questions: identity, the matrix brightness at each LED a keyboard may use, the matrix
/// state, and the legacy family's brightness, state and game-mode reads.
pub const GETTERS: &[Getter] = &[
    Getter { name: "firmware", class: 0x00, id: 0x81, size: 0x02, args: &[] },
    Getter { name: "device_mode", class: 0x00, id: 0x84, size: 0x02, args: &[] },
    Getter { name: "matrix_brightness_backlight", class: 0x0F, id: 0x84, size: 0x03, args: &[0x00, 0x05] },
    Getter { name: "matrix_brightness_logo", class: 0x0F, id: 0x84, size: 0x03, args: &[0x00, 0x04] },
    Getter { name: "matrix_brightness_zero", class: 0x0F, id: 0x84, size: 0x03, args: &[0x00, 0x00] },
    Getter { name: "matrix_brightness_backlight_varstore", class: 0x0F, id: 0x84, size: 0x03, args: &[0x01, 0x05] },
    Getter { name: "matrix_state", class: 0x0F, id: 0x82, size: 0x20, args: &[] },
    Getter { name: "legacy_brightness", class: 0x03, id: 0x83, size: 0x03, args: &[0x01, 0x05] },
    Getter { name: "legacy_state", class: 0x03, id: 0x88, size: 0x06, args: &[] },
    Getter { name: "game_mode", class: 0x03, id: 0x80, size: 0x03, args: &[0x00, 0x08] },
];

/// What one question got back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// SUCCESS; the first bytes of the body.
    Answered { body: Vec<u8> },
    /// A non-SUCCESS status byte (0x03 fail, 0x05 unsupported, …).
    Status { status: u8 },
    /// A fresh reply echoed the request but carried another tx. Neuron's validator drops these.
    TxMismatch { reply_tx: u8 },
    /// A fresh reply echoed the request with a wrong CRC. Neuron's validator drops these.
    BadCrc,
    /// No fresh reply echoed the request within the window. A buffer still holding the previous
    /// reply is silence, not an answer.
    Silent,
    /// No reply echoed the request, but a fresh reply to another command arrived: another program
    /// was talking to the device during the probe, so this cell says nothing about the board.
    CrossTalk { class: u8, id: u8 },
    /// The transport failed (unplugged, access denied).
    Error { message: String },
}

#[derive(Debug, Serialize)]
pub struct Cell {
    pub getter: &'static str,
    pub tx: u8,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// One control collection's answers.
#[derive(Debug, Serialize)]
pub struct Pipe {
    pub interface: Option<u8>,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    /// The keyboard atlas names this collection as the control collection.
    pub atlas_collection: bool,
    pub cells: Vec<Cell>,
}

/// What the keyboard atlas says about the PID, for reading the answers against.
#[derive(Debug, Serialize)]
pub struct AtlasSummary {
    pub name: String,
    pub class: Option<String>,
    pub lighting_tx: Vec<u8>,
    pub tx_grade: String,
    pub interface: Option<u8>,
    pub collection: Option<(u16, u16)>,
    pub led: Vec<u8>,
    pub dims: Option<(u8, u8)>,
}

/// The report IDs a HID LampArray collection uses for each standard report (HID Usage Tables,
/// Lighting and Illumination page 0x59), read from the OS descriptor parser with no device I/O.
/// A missing report means the collection is not the standard shape.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct LampArrayShape {
    pub attributes: Option<u8>,
    pub lamp_request: Option<u8>,
    pub lamp_response: Option<u8>,
    pub multi_update: Option<u8>,
    pub range_update: Option<u8>,
    pub control: Option<u8>,
    /// LampMultiUpdate's lamp slots per report (its LampId field's count), when declared.
    pub multi_update_slots: Option<u16>,
}

impl LampArrayShape {
    /// Every report a host needs to drive the array is declared.
    #[must_use]
    pub fn standard(&self) -> bool {
        self.attributes.is_some()
            && self.lamp_request.is_some()
            && self.lamp_response.is_some()
            && (self.multi_update.is_some() || self.range_update.is_some())
            && self.control.is_some()
    }
}

/// Logical-collection usages of the LampArray reports (HUTRR84).
const LAMP_REPORTS: [u16; 6] = [0x02, 0x20, 0x22, 0x50, 0x60, 0x70];

/// Shape a LampArray collection's feature fields. `None` when the metadata has no page-0x59 feature
/// fields at all.
#[must_use]
pub fn lamparray_shape(meta: &crate::transport::research::Metadata) -> Option<LampArrayShape> {
    use crate::transport::research::ReportKind;
    let mut shape = LampArrayShape::default();
    let mut any = false;
    for f in meta.fields.iter().filter(|f| f.kind == ReportKind::Feature && f.usage_page == 0x0059) {
        any = true;
        let Some(link) = meta.links.get(usize::from(f.link_collection)) else { continue };
        if link.usage_page != 0x0059 || !LAMP_REPORTS.contains(&link.usage) {
            continue;
        }
        let slot = match link.usage {
            0x02 => &mut shape.attributes,
            0x20 => &mut shape.lamp_request,
            0x22 => &mut shape.lamp_response,
            0x50 => &mut shape.multi_update,
            0x60 => &mut shape.range_update,
            _ => &mut shape.control,
        };
        slot.get_or_insert(f.report_id);
        // LampId (0x21) inside LampMultiUpdate: its report count is the slots per report.
        if link.usage == 0x50 && (f.usage_min..=f.usage_max).contains(&0x21) {
            shape.multi_update_slots = Some(f.report_count);
        }
    }
    any.then_some(shape)
}

/// One physical unit.
#[derive(Debug, Serialize)]
pub struct UnitReport {
    pub recipe: &'static str,
    pub pid: String,
    pub atlas: Option<AtlasSummary>,
    /// The unit also exposes a HID LampArray (Windows Dynamic Lighting can hold its lights).
    pub lamparray: bool,
    /// Windows' Dynamic Lighting switches when the probe ran.
    pub dynamic_lighting: Option<crate::dynlight::DynamicLighting>,
    /// The LampArray collection's report shape, when the OS parser can read it.
    pub lamparray_shape: Option<LampArrayShape>,
    pub pipes: Vec<Pipe>,
}

/// Ask one question at one tx, classifying the reply without the production validator so a
/// rejected reply is visible as such. The feature buffer is read before sending: a reply equal to
/// it is the previous command's, left behind by a board that did not answer this one.
pub fn ask(t: &dyn Transport, g: &Getter, tx: u8, window: Duration) -> Outcome {
    debug_assert!(g.id & 0x80 != 0, "the lighting probe only sends getters");
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    let mut stale = [0u8; BUF_LEN];
    let stale_len = t.get_feature(&mut stale).unwrap_or(0);
    let mut req = Report::command(tx, g.class, g.id, g.size);
    req.args[..g.args.len()].copy_from_slice(g.args);
    let request = req.to_buf();
    if let Err(e) = t.set_feature(&request) {
        return Outcome::Error { message: e.to_string() };
    }
    let polls = (window.as_millis() / 2).max(1);
    let mut foreign = None;
    for i in 0..polls {
        std::thread::sleep(Duration::from_millis(2));
        // A board still busy with another program's write drops a request; re-send, as synthesis's
        // probe does.
        if foreign.is_some() && i % 8 == 7 {
            let _ = t.set_feature(&request);
        }
        let mut b = [0u8; BUF_LEN];
        match t.get_feature(&mut b) {
            Ok(n) if n == BUF_LEN => {}
            Ok(_) => continue,
            Err(e) => return Outcome::Error { message: e.to_string() },
        }
        if stale_len == BUF_LEN && b == stale {
            continue;
        }
        if b[7] != g.class || b[8] != g.id {
            if b[1] != 0x00 && (b[7], b[8]) != (0, 0) {
                foreign = Some((b[7], b[8]));
            }
            continue;
        }
        if b[1] == 0x00 || b[1] == 0x01 {
            continue;
        }
        if b[2] != tx {
            return Outcome::TxMismatch { reply_tx: b[2] };
        }
        if b[89] != crc(&b) {
            return Outcome::BadCrc;
        }
        return match b[1] {
            0x02 => Outcome::Answered { body: b[9..9 + 4].to_vec() },
            s => Outcome::Status { status: s },
        };
    }
    match foreign {
        Some((class, id)) => Outcome::CrossTalk { class, id },
        None => Outcome::Silent,
    }
}

/// Ask every question at every cohort on one collection.
pub fn probe_pipe(t: &dyn Transport, window: Duration) -> Vec<Cell> {
    let mut cells = Vec::new();
    for g in GETTERS {
        for tx in COHORTS {
            cells.push(Cell { getter: g.name, tx, outcome: ask(t, g, tx, window) });
        }
    }
    cells
}

fn atlas_summary(pid: u16) -> Option<AtlasSummary> {
    let k = crate::atlas::lookup(crate::dialect::RAZER_VID, pid)?;
    Some(AtlasSummary {
        name: k.name.clone(),
        class: k.class().preferred().map(|c| format!("{c:?}").to_lowercase()),
        lighting_tx: k.tx_candidates(),
        tx_grade: k.lighting_tx().grade(),
        interface: k.interface().preferred(),
        collection: k.collections().first().map(|c| (c.usage_page, c.usage)),
        led: k.led().candidates(),
        dims: k.dims().preferred(),
    })
}

fn razer_claimed(i: &HidDeviceInfo) -> bool {
    crate::dialect::claimed_by(i).is_some_and(|d| d.id() == "razer")
}

/// Probe every connected Razer unit, or only `pid`. Each claimed collection is asked every
/// question at every cohort; `window` bounds the wait per question.
pub fn sweep(pid: Option<u16>, window: Duration) -> Result<Vec<UnitReport>> {
    let infos = transport::enumerate()?;
    let keys: Vec<String> = infos.iter().map(transport::unit_key).collect();
    let mut units: Vec<(u16, String)> = Vec::new();
    for (i, key) in infos.iter().zip(&keys) {
        if razer_claimed(i) && pid.is_none_or(|p| p == i.pid) && !units.contains(&(i.pid, key.clone())) {
            units.push((i.pid, key.clone()));
        }
    }
    let mut out = Vec::new();
    for (unit_pid, key) in units {
        let atlas = atlas_summary(unit_pid);
        let mine: Vec<&HidDeviceInfo> =
            infos.iter().zip(&keys).filter(|(i, k)| i.pid == unit_pid && **k == key).map(|(i, _)| i).collect();
        let lamparray = mine.iter().any(|i| i.usage_page == 0x0059);
        let lamparray_shape = mine
            .iter()
            .filter(|i| i.usage_page == 0x0059)
            .find_map(|i| transport::research_metadata(&i.path).ok().and_then(|m| lamparray_shape(&m)));
        let mut pipes = Vec::new();
        for i in mine.iter().filter(|i| razer_claimed(i)) {
            let cells = match transport::open_path(&i.path) {
                Ok(t) => probe_pipe(&*t, window),
                Err(e) => vec![Cell { getter: "open", tx: 0, outcome: Outcome::Error { message: e.to_string() } }],
            };
            pipes.push(Pipe {
                interface: i.interface(),
                usage_page: i.usage_page,
                usage: i.usage,
                feature_len: i.feature_len,
                atlas_collection: atlas.as_ref().is_some_and(|a| a.collection == Some((i.usage_page, i.usage))),
                cells,
            });
        }
        out.push(UnitReport {
            recipe: RECIPE,
            pid: format!("{unit_pid:04x}"),
            atlas,
            lamparray,
            dynamic_lighting: crate::dynlight::state(),
            lamparray_shape,
            pipes,
        });
    }
    Ok(out)
}

fn shape_line(shape: Option<&LampArrayShape>) -> String {
    let Some(sh) = shape else {
        return "shape not readable".into();
    };
    let id = |r: Option<u8>| r.map_or("-".into(), |r| r.to_string());
    format!(
        "{} · reports attr {} req {} resp {} multi {} range {} ctrl {}{}",
        if sh.standard() { "standard" } else { "non-standard" },
        id(sh.attributes),
        id(sh.lamp_request),
        id(sh.lamp_response),
        id(sh.multi_update),
        id(sh.range_update),
        id(sh.control),
        sh.multi_update_slots.map_or(String::new(), |n| format!(" · {n} lamps per update")),
    )
}

/// Short names for [`GETTERS`], in order, for [`summary`].
const SHORT: [&str; 10] = ["fw", "mode", "bl", "logo", "zero", "bl-vs", "state", "lbr", "lstate", "game"];

/// A few lines that fit in an issue form field: per collection, the transaction ids each read
/// answered at (`-` none, `busy` another program's traffic, `rejected` a reply the validator drops).
#[must_use]
pub fn summary(units: &[UnitReport]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    for u in units {
        let _ = writeln!(s, "{} · razer {}", RECIPE, u.pid);
        if let Some(a) = &u.atlas {
            let txs: Vec<String> = a.lighting_tx.iter().map(|t| format!("{t:02X}")).collect();
            let _ = writeln!(s, "atlas: {} · tx {} · {}", a.name, txs.join("/"), a.collection.map_or("?".into(), |(p, u)| format!("{p:04X}/{u:04X}")));
        }
        if u.lamparray {
            let _ = writeln!(s, "LampArray ({}) · {}", shape_line(u.lamparray_shape.as_ref()), crate::dynlight::describe(u.dynamic_lighting));
        }
        for p in &u.pipes {
            let mut line = format!("if{} {:04X}/{:04X}:", p.interface.map_or("?".into(), |i| i.to_string()), p.usage_page, p.usage);
            for (g, short) in GETTERS.iter().zip(SHORT) {
                let cells: Vec<&Cell> = p.cells.iter().filter(|c| c.getter == g.name).collect();
                let ok: String = cells
                    .iter()
                    .filter(|c| matches!(c.outcome, Outcome::Answered { .. }))
                    .map(|c| format!("{:02X}", c.tx))
                    .collect();
                let mark = if !ok.is_empty() {
                    ok
                } else if cells.iter().any(|c| matches!(c.outcome, Outcome::CrossTalk { .. })) {
                    "busy".into()
                } else if cells.iter().any(|c| matches!(c.outcome, Outcome::TxMismatch { .. } | Outcome::BadCrc)) {
                    "rejected".into()
                } else {
                    "-".into()
                };
                let _ = write!(line, " {short}:{mark}");
            }
            s.push_str(&line);
            s.push('\n');
        }
    }
    s
}

/// Where a unit's last automatic probe is kept, beside its generated definition.
#[must_use]
pub fn evidence_path(dialect_id: &str, pid: u16) -> std::path::PathBuf {
    crate::synth::auto_def_path(dialect_id, pid).with_extension("lighting.txt")
}

/// Probe `pid` and keep the [`summary`] beside its generated definition, for the device report.
/// Read-only on the device.
pub fn record(pid: u16) -> Result<()> {
    let units = sweep(Some(pid), Duration::from_millis(60))?;
    if units.is_empty() {
        return Ok(());
    }
    let path = evidence_path("razer", pid);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, summary(&units))?;
    Ok(())
}

fn short(o: &Outcome) -> String {
    match o {
        Outcome::Answered { body } => format!("ok {:02X}{:02X}{:02X}", body[0], body[1], body[2]),
        Outcome::Status { status: 0x05 } => "unsupp".into(),
        Outcome::Status { status: 0x03 } => "fail".into(),
        Outcome::Status { status } => format!("st {status:02X}"),
        Outcome::TxMismatch { reply_tx } => format!("tx!{reply_tx:02X}"),
        Outcome::BadCrc => "crc!".into(),
        Outcome::Silent => "-".into(),
        Outcome::CrossTalk { class, id } => format!("busy {class:02X}{id:02X}"),
        Outcome::Error { .. } => "err".into(),
    }
}

/// The plain-text table for a terminal or an issue.
#[must_use]
pub fn render(units: &[UnitReport]) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    if units.is_empty() {
        s.push_str("No Razer control collection is connected.\n");
    }
    for u in units {
        let _ = writeln!(s, "Razer {} · recipe {}", u.pid, u.recipe);
        match &u.atlas {
            Some(a) => {
                let txs: Vec<String> = a.lighting_tx.iter().map(|t| format!("0x{t:02X}")).collect();
                let _ = writeln!(
                    s,
                    "  atlas: {} · class {} · lighting tx {} ({}) · interface {} · collection {} · {}",
                    a.name,
                    a.class.as_deref().unwrap_or("?"),
                    txs.join("/"),
                    a.tx_grade,
                    a.interface.map_or("?".into(), |i| i.to_string()),
                    a.collection.map_or("?".into(), |(p, u)| format!("{p:04X}/{u:04X}")),
                    a.dims.map_or("?".into(), |(r, c)| format!("{r}x{c}")),
                );
            }
            None => s.push_str("  atlas: no entry for this PID\n"),
        }
        if u.lamparray {
            let _ = writeln!(s, "  also exposes a HID LampArray ({}) · {}", shape_line(u.lamparray_shape.as_ref()), crate::dynlight::describe(u.dynamic_lighting));
        }
        for p in &u.pipes {
            let _ = writeln!(
                s,
                "  interface {} · {:04X}/{:04X} · {} bytes{}",
                p.interface.map_or("?".into(), |i| i.to_string()),
                p.usage_page,
                p.usage,
                p.feature_len,
                if p.atlas_collection { " · the atlas's control collection" } else { "" }
            );
            let _ = writeln!(s, "    {:<38} {:>10} {:>10} {:>10} {:>10}", "getter", "tx 1F", "tx 3F", "tx 9F", "tx FF");
            for g in GETTERS {
                let row: Vec<String> = COHORTS
                    .iter()
                    .map(|&tx| {
                        p.cells
                            .iter()
                            .find(|c| c.getter == g.name && c.tx == tx)
                            .map_or("?".into(), |c| short(&c.outcome))
                    })
                    .collect();
                let _ = writeln!(s, "    {:<38} {:>10} {:>10} {:>10} {:>10}", g.name, row[0], row[1], row[2], row[3]);
            }
            if let Some(Cell { outcome: Outcome::Error { message }, .. }) = p.cells.first() {
                let _ = writeln!(s, "    could not open: {message}");
            }
        }
    }
    let busy = units.iter().flat_map(|u| &u.pipes).flat_map(|p| &p.cells).any(|c| matches!(c.outcome, Outcome::CrossTalk { .. }));
    if busy {
        s.push_str("Another program wrote to a device during the probe (busy cells). Quit Neuron, Synapse and any RGB app, then run it again.\n");
    }
    s.push_str("ok = answered (first body bytes) · unsupp/fail = refused · tx!/crc! = a reply Neuron's validator rejects · busy = another program's traffic · - = no reply\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::firmware::{Behaviour, FirmwareBackend, Refusal, Role, Unit};
    use std::sync::{Arc, PoisonError};

    /// A LampArray laid out as HUTRR84 declares it: one logical collection per report under the
    /// application collection, page 0x59 throughout. Report IDs are the descriptor's, not fixed.
    #[test]
    fn a_standard_lamparray_shape_is_recognised_from_os_metadata() {
        use crate::transport::research::{Field, LinkCollection, Metadata, ReportKind};
        let link = |usage| LinkCollection {
            usage_page: 0x59, usage, parent: 0, children: 0, first_child: 0, next_sibling: 0, collection_type: 2, alias: false,
        };
        let field = |report_id, link_collection, usage, report_count| Field {
            kind: ReportKind::Feature, button: false, alias: false, is_range: false, report_id,
            usage_page: 0x59, usage_min: usage, usage_max: usage, link_collection,
            data_index_min: 0, data_index_max: 0, bit_field: 2, absolute: true, bit_size: Some(8),
            report_count, logical_min: None, logical_max: None, physical_min: None, physical_max: None,
            units: None, units_exponent: None, has_null: false,
        };
        let mut meta = Metadata::unavailable();
        meta.links = vec![link(0x01), link(0x02), link(0x20), link(0x22), link(0x50), link(0x60), link(0x70)];
        meta.fields = vec![
            field(1, 1, 0x03, 1),
            field(2, 2, 0x21, 1),
            field(3, 3, 0x21, 1),
            field(4, 4, 0x03, 1),
            field(4, 4, 0x21, 8),
            field(5, 5, 0x61, 1),
            field(6, 6, 0x71, 1),
        ];
        let shape = lamparray_shape(&meta).expect("page-0x59 features");
        assert!(shape.standard());
        assert_eq!((shape.attributes, shape.multi_update, shape.control, shape.multi_update_slots), (Some(1), Some(4), Some(6), Some(8)));
        meta.fields.retain(|f| f.report_id != 6);
        assert!(!lamparray_shape(&meta).unwrap().standard(), "no control report, no host control");
        meta.fields.clear();
        assert!(lamparray_shape(&meta).is_none());
    }

    #[test]
    fn every_question_is_a_getter_and_none_reads_the_serial() {
        for g in GETTERS {
            assert!(g.id & 0x80 != 0, "{} is not a getter", g.name);
            assert_ne!((g.class, g.id), (0x00, 0x82), "the serial stays off the report");
            assert!(g.args.len() <= 2);
        }
        let known: Vec<(u8, u8)> = crate::synth::known_commands().iter().map(|&(_, c, i)| (c, i)).collect();
        for g in GETTERS {
            assert!(known.contains(&(g.class, g.id)), "{} asks an opcode synthesis does not know", g.name);
        }
    }

    #[test]
    fn the_sweep_separates_decoys_cohorts_and_rejected_replies() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let mut be = FirmwareBackend::new();
        let mut v3 = Behaviour::extended(&[0x3F], 6, 22);
        v3.foreign_getter = Refusal::Status(0x05);
        let board = be.with(
            Unit::new(0x024E, "Phantom V3", "sweep", v3)
                .collection(1, 0x0001, 0x0002, 91, Role::IdentityOnly)
                .collection(3, 0x000C, 0x0001, 91, Role::Control)
                .collection(4, 0x0059, 0x0001, 0, Role::Mute),
        );
        let mut odd = Behaviour::extended(&[0x1F], 6, 22);
        odd.reply_tx = Some(0x00);
        be.with(Unit::new(0x7E04, "Phantom odd", "odd", odd).collection(2, 0x0001, 0x0002, 91, Role::Control));
        let _bus = crate::transport::install_backend(Arc::new(be));

        let units = sweep(None, Duration::from_millis(20)).unwrap();
        assert_eq!(units.len(), 2, "one report per physical unit, not per collection");
        let v3 = units.iter().find(|u| u.pid == "024e").unwrap();
        assert!(v3.lamparray);
        assert_eq!(v3.pipes.len(), 2, "both claimed collections of the unit are probed");
        assert_eq!(v3.atlas.as_ref().unwrap().interface, Some(3));
        let cell = |p: &Pipe, g: &str, tx: u8| p.cells.iter().find(|c| c.getter == g && c.tx == tx).unwrap().outcome.clone();
        let decoy = v3.pipes.iter().find(|p| p.interface == Some(1)).unwrap();
        assert!(matches!(cell(decoy, "firmware", 0x1F), Outcome::Answered { .. }));
        assert_eq!(cell(decoy, "matrix_brightness_backlight", 0x3F), Outcome::Status { status: 0x05 });
        let control = v3.pipes.iter().find(|p| p.interface == Some(3)).unwrap();
        assert!(control.atlas_collection);
        assert_eq!(cell(control, "matrix_brightness_backlight", 0x1F), Outcome::Status { status: 0x05 });
        assert_eq!(cell(control, "matrix_brightness_backlight", 0x3F), Outcome::Answered { body: vec![0x00, 0x05, 0xFF, 0x00] });
        assert_eq!(cell(control, "matrix_brightness_logo", 0x3F), Outcome::Status { status: 0x05 });
        let odd = units.iter().find(|u| u.pid == "7e04").unwrap();
        assert_eq!(cell(&odd.pipes[0], "firmware", 0x1F), Outcome::TxMismatch { reply_tx: 0x00 });
        assert!(board.applied().is_empty() && board.decoy_writes().is_empty(), "the sweep wrote nothing");

        let sum = summary(&units);
        assert!(sum.contains("if3 000C/0001: fw:1F3F9FFF mode:1F3F9FFF bl:3F logo:-"), "{sum}");
        assert!(sum.contains("if1 0001/0002: fw:1F3F9FFF") && sum.contains("fw:rejected"), "{sum}");
        assert!(sum.lines().all(|l| l.len() < 200), "short enough for an issue form");

        let text = render(&units);
        assert!(text.contains("the atlas's control collection") && text.contains("LampArray") && text.contains("tx!00"), "{text}");
        let json = serde_json::to_value(&units).unwrap();
        assert_eq!(json[0]["recipe"], RECIPE);
        let json = json.to_string();
        assert!(!json.contains("hid#") && !json.contains("Phantom"), "no paths or product strings in the report");
    }

    /// A board that silently drops a request leaves the previous reply in its feature buffer. That
    /// is silence: not another program's traffic, and not a reply the validator rejects.
    #[test]
    fn a_stale_buffer_reads_as_silence() {
        let _serial = crate::transport::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let mut be = FirmwareBackend::new();
        let mut b = Behaviour::extended(&[0x1F], 6, 22);
        b.foreign_getter = Refusal::Silent;
        be.with(Unit::new(0x7E05, "Phantom quiet", "quiet", b).collection(2, 0x0001, 0x0002, 91, Role::Control));
        let _bus = crate::transport::install_backend(Arc::new(be));
        let units = sweep(None, Duration::from_millis(10)).unwrap();
        let p = &units[0].pipes[0];
        let cell = |g: &str, tx: u8| p.cells.iter().find(|c| c.getter == g && c.tx == tx).unwrap().outcome.clone();
        assert!(matches!(cell("matrix_brightness_backlight", 0x1F), Outcome::Answered { .. }));
        assert_eq!(cell("matrix_brightness_backlight", 0x3F), Outcome::Silent, "not a tx mismatch");
        assert_eq!(cell("matrix_state", 0x9F), Outcome::Silent, "not cross-talk");
        assert!(!render(&units).contains("Another program"));
    }
}
