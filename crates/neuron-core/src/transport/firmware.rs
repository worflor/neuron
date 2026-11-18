// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Phantom Razer keyboard firmware: a stateful model of how a board answers `razer_report`
//! traffic, so synthesis and the lighting path can be exercised against families neuron has no
//! hardware for.
//!
//! [`crate::transport::mock`] scripts single answers; this models a board. Writes change its
//! state, getters read that state back, and [`Board::shown`] says what its LEDs would display, so
//! a test asserts "the keys turned red", not "a packet went out".
//!
//! What real firmware does at the edges is mostly unknown: a request at a transaction id outside
//! its cohort, an effect aimed at an LED it lacks, a `data_size` shorter than openrazer's, a
//! collection that answers identity getters but ignores lighting. Each of those is a [`Behaviour`]
//! knob a test must state. A conformance suite runs every plausible combination, so neuron's
//! behaviour is pinned under each assumption instead of under the one a mock happened to encode.
//! Agreeing with this model is not hardware evidence.

use super::{Backend, DevicePath, HidDeviceInfo, InputReader, Transport, WireLock};
use crate::lighting::Rgb;
use crate::protocol::{Report, BUF_LEN};
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// The command family a board's lighting answers on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    /// Class 0x0F (openrazer "extended matrix").
    Extended,
    /// Class 0x03 (openrazer "standard matrix").
    Standard,
}

/// What a board does with a request it does not honour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No reply ever echoes the request.
    Silent,
    /// A reply carrying this status byte (0x03 fail, 0x05 unsupported).
    Status(u8),
    /// SUCCESS, and nothing changes: the Chroma V2's wrong-tx lighting writes.
    AckIgnore,
    /// Treated like an honoured request.
    Honour,
}

/// Which requests the transaction-id cohort applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Every request: even the firmware getter is refused at a foreign tx.
    All,
    /// Lighting requests only; identity and policy getters answer at any tx.
    Lighting,
    /// Effect and custom-frame writes only: the BlackWidow Chroma V2's split brain, where
    /// brightness and every getter ride the default tx (live, ledger `razer/tx/bw-lighting`).
    Effects,
    /// Every lighting write; every getter answers at any tx.
    Writes,
}

/// One board's firmware behaviour. Every field is an assumption a test states.
#[derive(Clone, Debug)]
pub struct Behaviour {
    pub family: Family,
    /// Transaction ids the board honours.
    pub cohort: Vec<u8>,
    /// Transaction ids its getters honour, when they differ from its writes' (openrazer reads the
    /// BlackWidow V4 X at 0x1F and writes its effects at 0x3F).
    pub read_cohort: Option<Vec<u8>>,
    pub scope: Scope,
    /// A getter at a transaction id outside the cohort.
    pub foreign_getter: Refusal,
    /// A write at a transaction id outside the cohort.
    pub foreign_write: Refusal,
    /// LEDs effects and brightness may address (extended family).
    pub leds: Vec<u8>,
    /// LED 0x00 addresses every LED in `leds`.
    pub zero_led_is_all: bool,
    /// An effect, brightness write or brightness read aimed at an LED the board lacks.
    pub foreign_led: Refusal,
    /// A brightness read at an LED the board lacks answers as if it were the first LED.
    pub lenient_led_reads: bool,
    /// Effects whose `data_size` is below openrazer's fixed size are acknowledged and ignored.
    pub strict_sizes: bool,
    /// The custom-frame display must carry LED 0x00 (`[00, 00, 08]`).
    pub strict_display: bool,
    pub rows: u8,
    pub cols: u8,
    /// Answers the game-mode getter (0x03/0x80).
    pub game_mode: bool,
    /// Replies carry this transaction id instead of echoing the request's.
    pub reply_tx: Option<u8>,
    /// Replies carry a wrong CRC byte.
    pub bad_crc: bool,
    /// Lighting writes are acknowledged and ignored unless the device mode is driver (0x03).
    pub needs_driver_mode: bool,
}

impl Behaviour {
    /// A matrix keyboard that honours exactly `cohort` for lighting, answers foreign lighting
    /// getters UNSUPPORTED, acknowledges and ignores foreign lighting writes, addresses its
    /// backlight as LED 0x05 and holds effects to openrazer's sizes. The strictest reading of the
    /// sources that still matches a V3 being adopted at tx 0x1F.
    #[must_use]
    pub fn extended(cohort: &[u8], rows: u8, cols: u8) -> Self {
        Behaviour {
            family: Family::Extended,
            cohort: cohort.to_vec(),
            read_cohort: None,
            scope: Scope::Lighting,
            foreign_getter: Refusal::Status(0x05),
            foreign_write: Refusal::AckIgnore,
            leds: vec![0x05],
            zero_led_is_all: false,
            foreign_led: Refusal::Status(0x05),
            lenient_led_reads: false,
            strict_sizes: true,
            strict_display: true,
            rows,
            cols,
            game_mode: true,
            reply_tx: None,
            bad_crc: false,
            needs_driver_mode: false,
        }
    }

    /// A legacy keyboard shaped like the BlackWidow Chroma V2: getters honour any tx, lighting
    /// writes land only at `write_tx`.
    #[must_use]
    pub fn standard(write_tx: u8, rows: u8, cols: u8) -> Self {
        Behaviour {
            family: Family::Standard,
            cohort: vec![write_tx],
            read_cohort: None,
            scope: Scope::Effects,
            foreign_getter: Refusal::Honour,
            foreign_write: Refusal::AckIgnore,
            leds: vec![0x05],
            zero_led_is_all: false,
            foreign_led: Refusal::Status(0x05),
            lenient_led_reads: false,
            strict_sizes: true,
            strict_display: false,
            rows,
            cols,
            game_mode: true,
            reply_tx: None,
            bad_crc: false,
            needs_driver_mode: false,
        }
    }
}

/// What the board's LEDs display.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    Dark,
    /// A native effect on one LED (`led` 0x00 with `zero_led_is_all` covers the board).
    Effect { led: u8, effect: u8, color: Option<Rgb> },
    /// The custom frame buffer, see [`Board::frame`].
    Frame,
}

/// One request as the board saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub tx: u8,
    pub class: u8,
    pub id: u8,
    pub size: u8,
    pub args: Vec<u8>,
}

#[derive(Default)]
struct State {
    shown: Option<Shown>,
    frame: Vec<Rgb>,
    brightness: BTreeMap<u8, u8>,
    game_mode: u8,
    device_mode: u8,
    log: Vec<Seen>,
    /// Applied writes, in order.
    applied: Vec<Seen>,
    /// Writes sent to an identity-only decoy collection.
    decoy_writes: Vec<Seen>,
}

/// A phantom board: the state behind every collection of one unit.
pub struct Board {
    pub behaviour: Behaviour,
    pub pid: u16,
    state: Mutex<State>,
}

impl Board {
    #[must_use]
    pub fn new(pid: u16, behaviour: Behaviour) -> Self {
        let state = State {
            frame: vec![Rgb::new(0, 0, 0); behaviour.rows as usize * behaviour.cols as usize],
            brightness: behaviour.leds.iter().map(|&led| (led, 0xFF)).collect(),
            ..State::default()
        };
        Board { behaviour, pid, state: Mutex::new(state) }
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// What the LEDs display now.
    #[must_use]
    pub fn shown(&self) -> Shown {
        self.st().shown.clone().unwrap_or(Shown::Dark)
    }

    /// The custom frame buffer, row-major.
    #[must_use]
    pub fn frame(&self) -> Vec<Rgb> {
        self.st().frame.clone()
    }

    #[must_use]
    pub fn brightness(&self, led: u8) -> Option<u8> {
        self.st().brightness.get(&led).copied()
    }

    /// Every request the control collection received.
    #[must_use]
    pub fn log(&self) -> Vec<Seen> {
        self.st().log.clone()
    }

    /// Every write the board applied.
    #[must_use]
    pub fn applied(&self) -> Vec<Seen> {
        self.st().applied.clone()
    }

    /// Every write sent to one of the unit's identity-only decoy collections.
    #[must_use]
    pub fn decoy_writes(&self) -> Vec<Seen> {
        self.st().decoy_writes.clone()
    }

    /// The device-mode byte the board holds.
    #[must_use]
    pub fn device_mode(&self) -> u8 {
        self.st().device_mode
    }

    /// The reply owed for one request, or `None` for silence.
    fn answer(&self, buf: &[u8]) -> Option<[u8; BUF_LEN]> {
        if buf.len() < BUF_LEN {
            return None;
        }
        let seen = Seen {
            tx: buf[2],
            class: buf[7],
            id: buf[8],
            size: buf[6],
            args: buf[9..89].to_vec(),
        };
        self.st().log.push(seen.clone());
        let b = &self.behaviour;
        let getter = seen.id & 0x80 != 0;
        let lighting = seen.class == 0x0F
            || (seen.class == 0x03 && matches!(seen.id, 0x03 | 0x0A | 0x0B | 0x83 | 0x88 | 0x89));
        let effect = matches!((seen.class, seen.id), (0x0F, 0x02 | 0x03) | (0x03, 0x0A | 0x0B));
        let in_scope = match b.scope {
            Scope::All => true,
            Scope::Lighting => lighting,
            Scope::Effects => effect,
            Scope::Writes => lighting && !getter,
        };
        let mut apply = true;
        let cohort = if getter { b.read_cohort.as_ref().unwrap_or(&b.cohort) } else { &b.cohort };
        if in_scope && !cohort.contains(&seen.tx) {
            match if getter { b.foreign_getter } else { b.foreign_write } {
                Refusal::Silent => return None,
                Refusal::Status(s) => return Some(self.reply(&seen, s, &[])),
                Refusal::AckIgnore => apply = false,
                Refusal::Honour => {}
            }
        }
        if !apply {
            return Some(self.reply(&seen, 0x02, &seen.args));
        }
        match self.handle(&seen) {
            Outcome::Answer(body) => Some(self.reply(&seen, 0x02, &body)),
            Outcome::Applied => {
                self.st().applied.push(seen.clone());
                Some(self.reply(&seen, 0x02, &seen.args))
            }
            Outcome::Refused(Refusal::Silent) => None,
            Outcome::Refused(Refusal::Status(s)) => Some(self.reply(&seen, s, &[])),
            Outcome::Refused(_) => Some(self.reply(&seen, 0x02, &seen.args)),
        }
    }

    fn reply(&self, seen: &Seen, status: u8, body: &[u8]) -> [u8; BUF_LEN] {
        let tx = self.behaviour.reply_tx.unwrap_or(seen.tx);
        let mut r = Report::command(tx, seen.class, seen.id, seen.size);
        r.status = status;
        let n = body.len().min(80);
        r.args[..n].copy_from_slice(&body[..n]);
        let mut out = r.to_buf();
        if self.behaviour.bad_crc {
            out[89] ^= 0xFF;
        }
        out
    }

    fn led_ok(&self, led: u8) -> bool {
        let b = &self.behaviour;
        b.leds.contains(&led) || (led == 0x00 && b.zero_led_is_all) || b.foreign_led == Refusal::Honour
    }

    fn handle(&self, s: &Seen) -> Outcome {
        let b = &self.behaviour;
        let a = &s.args;
        match (s.class, s.id) {
            (0x00, 0x81) => Outcome::Answer(vec![0x01, 0x02]),
            (0x00, 0x82) => Outcome::Answer(format!("PH{:04X}PHANTOM", self.pid).into_bytes()),
            (0x00, 0x84) => Outcome::Answer(vec![self.st().device_mode, 0x00]),
            (0x00, 0x04) if matches!(a[0], 0x00 | 0x03) => {
                self.st().device_mode = a[0];
                Outcome::Applied
            }
            (0x00, 0x85) => Outcome::Answer(vec![0x01]),
            (0x03, 0x80) if b.game_mode => Outcome::Answer(vec![a[0], a[1], self.st().game_mode]),
            (0x03, 0x00) if b.game_mode => {
                self.st().game_mode = a[2];
                Outcome::Applied
            }
            _ => {
                let lighting_write = s.id & 0x80 == 0 && matches!((s.class, s.id), (0x0F, _) | (0x03, 0x03 | 0x0A | 0x0B));
                if lighting_write && b.needs_driver_mode && self.st().device_mode != 0x03 {
                    return Outcome::Refused(Refusal::AckIgnore);
                }
                match b.family {
                    Family::Extended => self.extended(s),
                    Family::Standard => self.standard(s),
                }
            }
        }
    }

    fn extended(&self, s: &Seen) -> Outcome {
        let b = &self.behaviour;
        let a = &s.args;
        match (s.class, s.id) {
            (0x0F, 0x84) => {
                if !self.led_ok(a[1]) && !b.lenient_led_reads {
                    return Outcome::Refused(b.foreign_led);
                }
                let led = if a[1] == 0x00 || !b.leds.contains(&a[1]) { b.leds[0] } else { a[1] };
                let level = self.st().brightness.get(&led).copied().unwrap_or(0);
                Outcome::Answer(vec![a[0], a[1], level])
            }
            (0x0F, 0x82) => {
                let effect = match self.st().shown.clone() {
                    Some(Shown::Effect { effect, .. }) => effect,
                    Some(Shown::Frame) => 0x08,
                    _ => 0x00,
                };
                Outcome::Answer(vec![0x00, b.leds[0], effect])
            }
            (0x0F, 0x04) => {
                if !self.led_ok(a[1]) {
                    return Outcome::Refused(b.foreign_led);
                }
                let mut st = self.st();
                if a[1] == 0x00 {
                    for led in &b.leds {
                        st.brightness.insert(*led, a[2]);
                    }
                } else {
                    st.brightness.insert(a[1], a[2]);
                }
                Outcome::Applied
            }
            (0x0F, 0x02) => {
                let (led, effect) = (a[1], a[2]);
                if effect == 0x08 {
                    let display_ok = led == 0x00 || (!b.strict_display && self.led_ok(led));
                    if !display_ok {
                        return Outcome::Refused(b.foreign_led);
                    }
                    if b.strict_sizes && s.size < 0x0C {
                        return Outcome::Refused(Refusal::AckIgnore);
                    }
                    self.st().shown = Some(Shown::Frame);
                    return Outcome::Applied;
                }
                if !self.led_ok(led) {
                    return Outcome::Refused(b.foreign_led);
                }
                let need = match effect {
                    0x00 | 0x03 | 0x04 => 0x06,
                    0x01 | 0x05 => 0x09,
                    0x02 | 0x07 => 0x06,
                    _ => return Outcome::Refused(Refusal::Status(0x05)),
                };
                if b.strict_sizes && s.size < need {
                    return Outcome::Refused(Refusal::AckIgnore);
                }
                let color = matches!(effect, 0x01 | 0x05)
                    .then(|| Rgb::new(a[6], a[7], a[8]))
                    .or_else(|| (matches!(effect, 0x02 | 0x07) && s.size >= 0x09).then(|| Rgb::new(a[6], a[7], a[8])));
                self.st().shown = Some(if effect == 0x00 {
                    Shown::Dark
                } else {
                    Shown::Effect { led, effect, color }
                });
                Outcome::Applied
            }
            (0x0F, 0x03) => {
                let (row, start, stop) = (a[2] as usize, a[3] as usize, a[4] as usize);
                let (rows, cols) = (b.rows as usize, b.cols as usize);
                if row >= rows || start > stop || stop >= cols || 5 + (stop - start + 1) * 3 > 80 {
                    return Outcome::Refused(Refusal::Status(0x03));
                }
                let mut st = self.st();
                for (i, col) in (start..=stop).enumerate() {
                    let p = 5 + i * 3;
                    st.frame[row * cols + col] = Rgb::new(a[p], a[p + 1], a[p + 2]);
                }
                Outcome::Applied
            }
            _ => Outcome::Refused(Refusal::Status(0x05)),
        }
    }

    fn standard(&self, s: &Seen) -> Outcome {
        let b = &self.behaviour;
        let a = &s.args;
        match (s.class, s.id) {
            (0x03, 0x83) => {
                if a[1] != 0x05 {
                    return Outcome::Refused(b.foreign_led);
                }
                let level = self.st().brightness.get(&0x05).copied().unwrap_or(0);
                Outcome::Answer(vec![a[0], a[1], level])
            }
            (0x03, 0x88) => {
                let lit = !matches!(self.st().shown, None | Some(Shown::Dark));
                Outcome::Answer(if lit { vec![0x01, 0x05, 0x01, 0x00, 0x00, 0x01] } else { vec![0; 6] })
            }
            (0x03, 0x03) => {
                if a[1] != 0x05 {
                    return Outcome::Refused(b.foreign_led);
                }
                self.st().brightness.insert(0x05, a[2]);
                Outcome::Applied
            }
            (0x03, 0x0A) => {
                let effect = a[0];
                let need = match effect {
                    0x00 | 0x04 => 0x01,
                    0x01 => 0x02,
                    0x02 => 0x05,
                    0x03 => 0x08,
                    0x05 => 0x02,
                    0x06 => 0x04,
                    _ => return Outcome::Refused(Refusal::Status(0x05)),
                };
                if b.strict_sizes && s.size != need {
                    return Outcome::Refused(Refusal::AckIgnore);
                }
                let shown = match effect {
                    0x00 => Shown::Dark,
                    0x05 => Shown::Frame,
                    0x06 => Shown::Effect { led: 0x05, effect, color: Some(Rgb::new(a[1], a[2], a[3])) },
                    _ => Shown::Effect { led: 0x05, effect, color: None },
                };
                self.st().shown = Some(shown);
                Outcome::Applied
            }
            (0x03, 0x0B) => {
                let (row, start, stop) = (a[1] as usize, a[2] as usize, a[3] as usize);
                let (rows, cols) = (b.rows as usize, b.cols as usize);
                if a[0] != 0xFF || row >= rows || start > stop || stop >= cols || 4 + (stop - start + 1) * 3 > 80 {
                    return Outcome::Refused(Refusal::Status(0x03));
                }
                let mut st = self.st();
                for (i, col) in (start..=stop).enumerate() {
                    let p = 4 + i * 3;
                    st.frame[row * cols + col] = Rgb::new(a[p], a[p + 1], a[p + 2]);
                }
                Outcome::Applied
            }
            _ => Outcome::Refused(Refusal::Status(0x05)),
        }
    }
}

enum Outcome {
    Answer(Vec<u8>),
    Applied,
    Refused(Refusal),
}

/// How one collection of a unit behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The board's control collection: [`Board`] answers everything.
    Control,
    /// Answers the identity getters (firmware, serial, mode) and refuses everything else as
    /// UNSUPPORTED: a collection that qualifies as a talking pipe but carries no lighting.
    IdentityOnly,
    /// Accepts requests and never replies.
    Mute,
}

struct Collection {
    info: HidDeviceInfo,
    role: Role,
    wire: Arc<WireLock>,
    pending: Mutex<Option<[u8; BUF_LEN]>>,
}

/// One physical unit: a board plus the HID collections it enumerates as.
pub struct Unit {
    pub board: Arc<Board>,
    product: String,
    tag: String,
    collections: Vec<Arc<Collection>>,
}

impl Unit {
    /// A unit with no collections yet. `tag` keeps two units of one PID apart.
    #[must_use]
    pub fn new(pid: u16, product: &str, tag: &str, behaviour: Behaviour) -> Self {
        Unit {
            board: Arc::new(Board::new(pid, behaviour)),
            product: product.to_string(),
            tag: tag.to_string(),
            collections: Vec::new(),
        }
    }

    /// Add a collection on USB `interface` with the given usage and feature length.
    #[must_use]
    pub fn collection(mut self, interface: u8, usage_page: u16, usage: u16, feature_len: u16, role: Role) -> Self {
        // Windows' shape: the instance segment differs per interface (its own hash) and per
        // collection (its own index), so collections of one unit share only the Container ID that
        // `research_metadata` reports (see `transport::unit_key`).
        let n = self.collections.len() + 1;
        let path = format!(
            "\\\\?\\hid#vid_1532&pid_{:04x}&mi_{interface:02x}&col{n:02}#{}&{}{interface:x}&0&{:04}#{{4d1e55b2-f16f-11cf-88cb-001111000030}}",
            self.board.pid,
            7 + u32::from(interface),
            self.tag,
            n - 1
        );
        self.collections.push(Arc::new(Collection {
            info: HidDeviceInfo {
                vid: 0x1532,
                pid: self.board.pid,
                usage_page,
                usage,
                feature_len,
                input_len: 0,
                output_len: 0,
                path: DevicePath::from_str_for_tests(&path),
                product: self.product.clone(),
            },
            role,
            wire: Arc::new(WireLock::new_local()),
            pending: Mutex::new(None),
        }));
        self
    }

    /// Enumeration records for every collection.
    #[must_use]
    pub fn infos(&self) -> Vec<HidDeviceInfo> {
        self.collections.iter().map(|c| copy_info(&c.info)).collect()
    }
}

fn copy_info(i: &HidDeviceInfo) -> HidDeviceInfo {
    HidDeviceInfo {
        vid: i.vid,
        pid: i.pid,
        usage_page: i.usage_page,
        usage: i.usage,
        feature_len: i.feature_len,
        input_len: i.input_len,
        output_len: i.output_len,
        path: i.path.clone(),
        product: i.product.clone(),
    }
}

struct Handle {
    board: Arc<Board>,
    collection: Arc<Collection>,
}

/// A transport onto `board`'s control collection, with no enumeration around it.
#[must_use]
pub fn control(board: &Arc<Board>) -> Box<dyn Transport> {
    let unit = Unit {
        board: Arc::clone(board),
        product: String::new(),
        tag: "direct".into(),
        collections: Vec::new(),
    }
    .collection(0, 0xFF00, 0x0002, BUF_LEN as u16, Role::Control);
    Box::new(Handle { board: Arc::clone(board), collection: Arc::clone(&unit.collections[0]) })
}

impl Transport for Handle {
    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        let reply = match self.collection.role {
            Role::Control => self.board.answer(buf),
            Role::Mute => None,
            Role::IdentityOnly => (buf.len() >= BUF_LEN).then(|| {
                let (class, id) = (buf[7], buf[8]);
                if id & 0x80 == 0 {
                    self.board.st().decoy_writes.push(Seen { tx: buf[2], class, id, size: buf[6], args: buf[9..89].to_vec() });
                }
                let mut r = Report::command(buf[2], class, id, buf[6]);
                match (class, id) {
                    (0x00, 0x81) => {
                        r.status = 0x02;
                        r.args[..2].copy_from_slice(&[0x01, 0x00]);
                    }
                    (0x00, 0x82 | 0x84) => r.status = 0x02,
                    _ => r.status = 0x05,
                }
                r.to_buf()
            }),
        };
        // A request that gets no reply leaves the previous one in the feature buffer, as on real
        // hardware: readers must tell a stale reply from a fresh one.
        if let Some(reply) = reply {
            *self.collection.pending.lock().unwrap_or_else(PoisonError::into_inner) = Some(reply);
        }
        Ok(())
    }

    fn get_feature(&self, buf: &mut [u8]) -> Result<usize> {
        let pending = *self.collection.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(reply) = pending else {
            return Ok(0);
        };
        let n = buf.len().min(BUF_LEN);
        buf[..n].copy_from_slice(&reply[..n]);
        Ok(n)
    }

    fn wire_lock(&self) -> Option<Arc<WireLock>> {
        Some(Arc::clone(&self.collection.wire))
    }
}

/// A [`Backend`] serving phantom units.
#[derive(Default)]
pub struct FirmwareBackend {
    units: Vec<Unit>,
}

impl FirmwareBackend {
    #[must_use]
    pub fn new() -> Self {
        FirmwareBackend::default()
    }

    /// Add a unit and hand back its board for assertions.
    pub fn with(&mut self, unit: Unit) -> Arc<Board> {
        let board = Arc::clone(&unit.board);
        self.units.push(unit);
        board
    }

    fn find(&self, path: &DevicePath) -> Option<(Arc<Board>, Arc<Collection>)> {
        self.units.iter().find_map(|u| {
            u.collections
                .iter()
                .find(|c| c.info.path == *path)
                .map(|c| (Arc::clone(&u.board), Arc::clone(c)))
        })
    }
}

impl Backend for FirmwareBackend {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>> {
        Ok(self.units.iter().flat_map(Unit::infos).collect())
    }

    fn open_path(&self, path: &DevicePath) -> Result<Box<dyn Transport>> {
        match self.find(path) {
            Some((board, collection)) => Ok(Box::new(Handle { board, collection })),
            None => bail!("firmware phantom: nothing at {path:?}"),
        }
    }

    /// Every collection of a unit reports the unit's tag as its Windows Container ID.
    fn research_metadata(&self, path: &DevicePath) -> Result<super::research::Metadata> {
        use super::research::{DescriptorSource, Grouping, Metadata};
        let unit = self
            .units
            .iter()
            .find(|u| u.collections.iter().any(|c| c.info.path == *path))
            .ok_or_else(|| anyhow::anyhow!("firmware phantom: nothing at {path:?}"))?;
        Ok(Metadata {
            source: DescriptorSource::WindowsHidParser,
            grouping: Grouping::WindowsContainer,
            group_key: Some(unit.tag.clone()),
            complete: true,
            ..Metadata::unavailable()
        })
    }

    fn open_reader(&self, path: &DevicePath) -> Result<Box<dyn InputReader>> {
        if self.find(path).is_none() {
            bail!("firmware phantom: nothing at {path:?}");
        }
        Ok(Box::new(super::mock::MockReader::new(Vec::new())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn send(t: &dyn Transport, tx: u8, class: u8, id: u8, size: u8, args: &[u8]) -> Option<[u8; BUF_LEN]> {
        let mut r = Report::command(tx, class, id, size);
        r.args[..args.len()].copy_from_slice(args);
        t.set_feature(&r.to_buf()).unwrap();
        let mut b = [0u8; BUF_LEN];
        (t.get_feature(&mut b).unwrap() == BUF_LEN).then_some(b)
    }

    fn v3_unit() -> (FirmwareBackend, Arc<Board>, Vec<HidDeviceInfo>) {
        let mut be = FirmwareBackend::new();
        let unit = Unit::new(0x024E, "Phantom V3", "a", Behaviour::extended(&[0x3F], 6, 22))
            .collection(0, 0x0001, 0x0006, 0, Role::Mute)
            .collection(2, 0x0001, 0x0002, 91, Role::IdentityOnly)
            .collection(3, 0x000C, 0x0001, 91, Role::Control);
        let infos = unit.infos();
        let board = be.with(unit);
        (be, board, infos)
    }

    #[test]
    fn a_strict_board_honours_only_its_cohort() {
        let (be, board, infos) = v3_unit();
        let t = be.open_path(&infos[2].path).unwrap();
        let foreign = send(&*t, 0x1F, 0x0F, 0x84, 0x03, &[0x00, 0x05]).expect("a status reply");
        assert_eq!(foreign[1], 0x05, "a foreign-tx getter is refused");
        let own = send(&*t, 0x3F, 0x0F, 0x84, 0x03, &[0x00, 0x05]).expect("answered");
        assert_eq!((own[1], own[2], own[11]), (0x02, 0x3F, 0xFF), "SUCCESS, tx echoed, full brightness");

        let ack = send(&*t, 0x1F, 0x0F, 0x02, 0x09, &[0x00, 0x05, 0x01, 0, 0, 1, 0xFF, 0, 0]).unwrap();
        assert_eq!(ack[1], 0x02, "a foreign-tx write is acknowledged");
        assert_eq!(board.shown(), Shown::Dark, "and ignored");
        send(&*t, 0x3F, 0x0F, 0x02, 0x09, &[0x00, 0x05, 0x01, 0, 0, 1, 0xFF, 0, 0]).unwrap();
        assert_eq!(board.shown(), Shown::Effect { led: 0x05, effect: 0x01, color: Some(Rgb::new(0xFF, 0, 0)) });
    }

    #[test]
    fn leds_sizes_and_the_display_prefix_are_enforced() {
        let (be, board, infos) = v3_unit();
        let t = be.open_path(&infos[2].path).unwrap();
        let wrong_led = send(&*t, 0x3F, 0x0F, 0x02, 0x09, &[0x00, 0x00, 0x01, 0, 0, 1, 0, 0xFF, 0]).unwrap();
        assert_eq!(wrong_led[1], 0x05, "LED 0x00 is not this board's backlight");
        send(&*t, 0x3F, 0x0F, 0x02, 0x03, &[0x00, 0x05, 0x03]).unwrap();
        assert_eq!(board.shown(), Shown::Dark, "a short spectrum is ignored");
        send(&*t, 0x3F, 0x0F, 0x03, 0x0B, &[0x00, 0x00, 0x01, 0x00, 0x01, 1, 2, 3, 4, 5, 6]).unwrap();
        send(&*t, 0x3F, 0x0F, 0x02, 0x0C, &[0x00, 0x05, 0x08]).unwrap();
        assert_eq!(board.shown(), Shown::Dark, "the display must carry LED 0x00");
        send(&*t, 0x3F, 0x0F, 0x02, 0x0C, &[0x00, 0x00, 0x08]).unwrap();
        assert_eq!(board.shown(), Shown::Frame);
        assert_eq!(&board.frame()[22..24], &[Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)], "row 1 painted");
        let state = send(&*t, 0x3F, 0x0F, 0x82, 0x20, &[]).unwrap();
        assert_eq!(state[11], 0x08, "lighting_state reports the custom frame");
    }

    #[test]
    fn decoys_answer_identity_or_nothing() {
        let (be, _board, infos) = v3_unit();
        let identity = be.open_path(&infos[1].path).unwrap();
        assert_eq!(send(&*identity, 0x1F, 0x00, 0x81, 0x02, &[]).unwrap()[1], 0x02);
        assert_eq!(send(&*identity, 0x3F, 0x0F, 0x84, 0x03, &[0x00, 0x05]).unwrap()[1], 0x05);
        let mute = be.open_path(&infos[0].path).unwrap();
        assert!(send(&*mute, 0x1F, 0x00, 0x81, 0x02, &[]).is_none());
        assert_eq!(infos[2].interface(), Some(3));
        assert_ne!(infos[1].instance(), infos[2].instance(), "Windows gives each interface its own instance");
        let _serial = super::super::mock::test_lock().lock().unwrap_or_else(PoisonError::into_inner);
        let _bus = super::super::install_backend(Arc::new(be));
        assert_eq!(super::super::unit_key(&infos[1]), super::super::unit_key(&infos[2]), "one unit by container");
    }

    #[test]
    fn a_standard_board_needs_its_lighting_tx_but_not_for_getters() {
        let mut be = FirmwareBackend::new();
        let unit = Unit::new(0x0221, "Phantom Chroma V2", "b", Behaviour::standard(0x3F, 6, 22))
            .collection(2, 0x0001, 0x0002, 91, Role::Control);
        let infos = unit.infos();
        let board = be.with(unit);
        let t = be.open_path(&infos[0].path).unwrap();
        assert_eq!(send(&*t, 0xFF, 0x03, 0x83, 0x03, &[0x01, 0x05]).unwrap()[1], 0x02);
        send(&*t, 0xFF, 0x03, 0x0A, 0x04, &[0x06, 0xFF, 0, 0]).unwrap();
        assert_eq!(board.shown(), Shown::Dark, "the split-brain no-op");
        send(&*t, 0x3F, 0x03, 0x0A, 0x04, &[0x06, 0xFF, 0, 0]).unwrap();
        assert_eq!(board.shown(), Shown::Effect { led: 0x05, effect: 0x06, color: Some(Rgb::new(0xFF, 0, 0)) });
    }
}
