// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! QMK VIA raw HID: keyboards running QMK with VIA enabled (Keychron and many others). Source-derived
//! from QMK's own handler; no VIA board has been on the wire.
//!
//! The channel is QMK's raw HID collection, usage page 0xFF60 / usage 0x61 with 32-byte reports
//! (`tmk_core/protocol/usb_descriptor_common.h` L45-L49 and `usb_descriptor.h` L304 @8252b630).
//! A request is `[command, data...]`; the board answers on the same collection with the request
//! echoed and the answer written over its data. A command it doesn't handle comes back with byte 0
//! set to 0xFF (`quantum/via.c` L479-L489), so asking is safe.
//!
//! Adoption asks the protocol version (0x01), then lighting brightness (custom get 0x08, value 1)
//! on each QMK lighting channel until one answers: RGB matrix 3, RGB light 2, LED matrix 5,
//! backlight 1 (`quantum/via.h` L88-L119). Brightness is 0..255 on the wire, which is the raw
//! level neuron's brightness readout takes at `args[2]`. Brightness is set on the same channel
//! with the unsaved custom set (0x07) and read back ([`set_brightness`]).
//!
//! Never sent: anything that writes EEPROM or the keymap (0x05, 0x06, 0x09, 0x0A, 0x0F, 0x10,
//! 0x13, 0x15), the bootloader jump (0x0B), and the switch-matrix read (keyboard value 0x03), which
//! reports live key state.

use crate::dialect::Dialect;
use crate::transport::{HidDeviceInfo, Transport};
use anyhow::{bail, Result};

pub const RAW_USAGE_PAGE: u16 = 0xFF60;
pub const RAW_USAGE: u16 = 0x61;

/// One raw HID report on the wire: 32 bytes of data, plus the report-id byte Windows counts in a
/// collection's report length. A collection with no report ids still reports 33.
const DATA_LEN: usize = 32;
const WIRE_LEN: usize = DATA_LEN + 1;

const CMD_GET_PROTOCOL_VERSION: u8 = 0x01;
const CMD_CUSTOM_SET_VALUE: u8 = 0x07;
const CMD_CUSTOM_GET_VALUE: u8 = 0x08;
const UNHANDLED: u8 = 0xFF;

/// QMK lighting channels, in the order adoption asks them.
const LIGHTING_CHANNELS: [u8; 4] = [0x03, 0x02, 0x05, 0x01];
const VALUE_BRIGHTNESS: u8 = 0x01;

/// Reshape tags in a command spec's `args[0]`, stripped before the wire.
pub const RESHAPE_NONE: u8 = 0x00;
pub const RESHAPE_BRIGHTNESS: u8 = 0x01;

const READ_WINDOW_MS: u32 = 100;
const MAX_SILENT_WINDOWS: u32 = 10;
/// Unrelated frames (another VIA client's replies) a wait skips before giving up.
const MAX_FOREIGN_FRAMES: u32 = 32;

fn request(command: u8, data: &[u8]) -> [u8; WIRE_LEN] {
    let mut r = [0u8; WIRE_LEN];
    r[1] = command;
    for (slot, b) in r[2..].iter_mut().zip(data) {
        *slot = *b;
    }
    r
}

/// The 32 data bytes of an input report, with or without the leading report-id byte.
fn data_of(buf: &[u8]) -> Option<&[u8]> {
    match buf.len() {
        WIRE_LEN if buf[0] == 0 => Some(&buf[1..]),
        DATA_LEN => Some(buf),
        _ => None,
    }
}

enum Answer {
    Reply([u8; DATA_LEN]),
    Unhandled,
    Silent,
}

/// Wait for the reply echoing `echo` (the request's leading data bytes). An unhandled request
/// comes back with byte 0 = 0xFF and the rest of the request intact.
fn await_reply(t: &dyn Transport, echo: &[u8]) -> Answer {
    let (mut silent, mut foreign) = (0, 0);
    while silent < MAX_SILENT_WINDOWS && foreign < MAX_FOREIGN_FRAMES {
        let mut buf = [0u8; WIRE_LEN];
        let n = match t.read_input(&mut buf, READ_WINDOW_MS) {
            Ok(n) if n <= WIRE_LEN => n,
            Ok(_) => continue,
            Err(_) => {
                silent += 1;
                continue;
            }
        };
        let Some(data) = data_of(&buf[..n]) else {
            foreign += 1;
            continue;
        };
        if data.starts_with(echo) {
            let mut reply = [0u8; DATA_LEN];
            reply.copy_from_slice(data);
            return Answer::Reply(reply);
        }
        if data[0] == UNHANDLED && data[1..].starts_with(&echo[1..]) {
            return Answer::Unhandled;
        }
        foreign += 1;
    }
    Answer::Silent
}

fn ask(t: &dyn Transport, command: u8, data: &[u8]) -> Answer {
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    if t.write_output(&request(command, data)).is_err() {
        return Answer::Silent;
    }
    let mut echo = vec![command];
    echo.extend_from_slice(data);
    await_reply(t, &echo)
}

/// The VIA protocol version a board reports, or `None` when it doesn't speak VIA.
pub fn protocol_version(t: &dyn Transport) -> Option<u16> {
    // The reply overwrites the request's data, so echo only the command byte.
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    t.write_output(&request(CMD_GET_PROTOCOL_VERSION, &[])).ok()?;
    match await_reply(t, &[CMD_GET_PROTOCOL_VERSION]) {
        Answer::Reply(r) => Some(u16::from_be_bytes([r[1], r[2]])).filter(|v| *v != 0),
        Answer::Unhandled | Answer::Silent => None,
    }
}

/// The first QMK lighting channel that answers a brightness read.
fn lighting_channel(t: &dyn Transport) -> Option<u8> {
    LIGHTING_CHANNELS
        .into_iter()
        .find(|&ch| matches!(ask(t, CMD_CUSTOM_GET_VALUE, &[ch, VALUE_BRIGHTNESS]), Answer::Reply(_)))
}

/// Is this command spec one VIA adoption emits (the brightness read or its unsaved set, on a QMK
/// lighting channel)? The run-root check holds a learned VIA file to exactly these.
#[must_use]
pub fn is_adopted_command(c: &crate::registry::CommandSpec) -> bool {
    let shape = match c.class {
        CMD_CUSTOM_GET_VALUE => [RESHAPE_BRIGHTNESS, VALUE_BRIGHTNESS],
        CMD_CUSTOM_SET_VALUE => [RESHAPE_NONE, VALUE_BRIGHTNESS],
        _ => return false,
    };
    LIGHTING_CHANNELS.contains(&c.id) && c.args == shape && c.transaction_id.is_none()
}

/// Set lighting brightness (0..=255) through VIA's custom set (0x07), which QMK applies without
/// saving (`*_noeeprom`, `quantum/via.c` L716-L723, L544-L551, L628-L634, L799-L805 @8252b630);
/// neuron never sends the save (0x09). Read back afterwards: the board stores brightness at its own
/// resolution (a backlight may have three steps), so the write holds when the board reached the
/// value, came closer to it, or is within a step's rounding; a board that stayed put is an error.
pub fn set_brightness(dev: &crate::device::Device, level: u8) -> Result<()> {
    let read = || dev.run("brightness").map(|a| a[2]);
    let before = read()?;
    dev.run_args("set_brightness", &[RESHAPE_NONE, VALUE_BRIGHTNESS, level])?;
    let after = read()?;
    let off = |v: u8| v.abs_diff(level);
    if after == level || off(after) < off(before) || off(after) <= 2 {
        Ok(())
    } else {
        bail!("VERIFY FAILED on brightness: asked {} for {level}, it stayed at {after}", dev.def.name)
    }
}

fn reshape(tag: u8, payload: &[u8]) -> [u8; 80] {
    let mut out = [0u8; 80];
    match tag {
        RESHAPE_BRIGHTNESS => out[2] = payload.first().copied().unwrap_or(0),
        _ => {
            let n = payload.len().min(80);
            out[..n].copy_from_slice(&payload[..n]);
        }
    }
    out
}

fn board_name(product: &str, pid: u16) -> String {
    let trimmed = product.trim();
    if trimmed.is_empty() {
        format!("QMK keyboard {pid:04x}")
    } else {
        trimmed.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
    }
}

pub struct ViaDialect;

impl Dialect for ViaDialect {
    fn id(&self) -> &'static str {
        "via"
    }

    /// The raw HID collection, sized for 32-byte reports in both directions. Any vendor: QMK
    /// boards carry their maker's VID.
    fn claims(&self, info: &HidDeviceInfo) -> bool {
        info.usage_page == RAW_USAGE_PAGE
            && info.usage == RAW_USAGE
            && usize::from(info.output_len) == WIRE_LEN
            && usize::from(info.input_len) == WIRE_LEN
    }

    fn matches_control(&self, def: &crate::registry::DeviceDef, info: &HidDeviceInfo) -> bool {
        self.claims(info)
            && info.vid == def.vendor_id
            && def.modes.iter().any(|m| m.product_id == info.pid)
    }

    fn interested(&self, info: &HidDeviceInfo) -> bool {
        info.usage_page == RAW_USAGE_PAGE
    }

    /// Read-only adoption: a board that answers the protocol version is a VIA board; brightness is
    /// adopted from the first lighting channel that answers. No setters.
    ///
    /// Only a keyboard is asked (its unit also enumerates a boot-keyboard collection). A raw HID
    /// collection belongs to whatever firmware handles it, and with VIA off a board's own code
    /// decides what byte 0x01 means; VIA's tools likewise talk only to boards they recognise
    /// (vial-gui `util.py` `find_vial_devices` @aef8222a).
    fn probe(&self, t: &dyn Transport, ctx: &crate::synth::SynthCtx) -> Option<crate::synth::Synthesis> {
        use crate::registry::{CommandSpec, ControlInterface, DefOrigin, DeviceDef, Mode};
        use crate::synth::{Heuristic, Synthesis};

        if !ctx.keyboard {
            return None;
        }

        let start = std::time::Instant::now();
        let version = protocol_version(t)?;
        let roundtrip_ms = start.elapsed().as_millis() as u64;

        let mut commands = std::collections::BTreeMap::new();
        let channel = lighting_channel(t);
        if let Some(channel) = channel {
            commands.insert(
                "brightness".to_string(),
                CommandSpec {
                    class: CMD_CUSTOM_GET_VALUE,
                    id: channel,
                    size: 0,
                    args: vec![RESHAPE_BRIGHTNESS, VALUE_BRIGHTNESS],
                    transaction_id: None,
                },
            );
        }
        // Only the read was asked; the set on the same channel is proven by read-back at each use.
        let answered: Vec<String> = commands.keys().cloned().collect();
        if let Some(channel) = channel {
            commands.insert(
                "set_brightness".to_string(),
                CommandSpec {
                    class: CMD_CUSTOM_SET_VALUE,
                    id: channel,
                    size: 0,
                    args: vec![RESHAPE_NONE, VALUE_BRIGHTNESS],
                    transaction_id: None,
                },
            );
        }
        let def = DeviceDef {
            name: board_name(&ctx.product, ctx.pid),
            codename: format!("auto-{:04x}", ctx.pid),
            dialect: "via".into(),
            origin: DefOrigin::Auto,
            vendor_id: ctx.vid,
            transaction_id: 0,
            stream_wait_us: 0,
            modes: vec![Mode { name: "default".into(), product_id: ctx.pid }],
            event_alias_pids: Vec::new(),
            control_interface: ControlInterface {
                usage_page: ctx.usage_page,
                usage: ctx.usage,
                feature_report_len: ctx.feature_len,
                interface: None,
            },
            commands,
            lighting: None,
            side_plates: None,
            side_plate_binds: std::collections::BTreeMap::new(),
            fn_binds: std::collections::BTreeMap::new(),
            events: None,
            buttons: Vec::new(),
            custody: None,
        };
        let mut s = Synthesis::from_probe(def, answered, Heuristic(0), None, Heuristic(0), String::new(), Some(roundtrip_ms));
        if let Some(channel) = channel {
            s.provenance.push(format!(
                "#   - brightness: QMK lighting channel {channel} answered (VIA protocol {version:#06x})."
            ));
        }
        Some(s)
    }

    /// `spec_class` = VIA command, `spec_id` = its first data byte (the channel), `args[0]` =
    /// reshape tag, `args[1..]` = the rest of the request data.
    fn exec(&self, t: &dyn Transport, _tx: u8, spec_class: u8, spec_id: u8, _size: u8, args: &[u8]) -> Result<[u8; 80]> {
        let tag = args.first().copied().unwrap_or(RESHAPE_NONE);
        let mut data = vec![spec_id];
        data.extend_from_slice(args.get(1..).unwrap_or(&[]));
        match ask(t, spec_class, &data) {
            Answer::Reply(r) => Ok(reshape(tag, &r[1 + data.len()..])),
            Answer::Unhandled => bail!("VIA command {spec_class:#04x}/{spec_id:#04x} unhandled by this board"),
            Answer::Silent => Err(crate::dialect::NoReply { class: spec_class, id: spec_id }.into()),
        }
    }

    /// No streamed writes in this dialect.
    fn exec_fast(&self, _t: &dyn Transport, _tx: u8, _class: u8, _id: u8, _size: u8, _args: &[u8], _wait: u64) -> crate::dialect::Sent {
        crate::dialect::Sent::Lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::DevicePath;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A scripted QMK board: answers each request from a handler, with the report-id byte Windows
    /// prepends.
    struct Board {
        writes: Mutex<Vec<Vec<u8>>>,
        inbox: Mutex<VecDeque<Vec<u8>>>,
        answer: fn(&[u8]) -> Vec<Vec<u8>>,
    }

    impl Board {
        fn new(answer: fn(&[u8]) -> Vec<Vec<u8>>) -> Self {
            Board { writes: Mutex::new(Vec::new()), inbox: Mutex::new(VecDeque::new()), answer }
        }
    }

    impl Transport for Board {
        fn set_feature(&self, _: &[u8]) -> Result<()> { bail!("no feature reports") }
        fn get_feature(&self, _: &mut [u8]) -> Result<usize> { bail!("no feature reports") }
        fn write_output(&self, buf: &[u8]) -> Result<()> {
            assert_eq!(buf.len(), WIRE_LEN);
            self.writes.lock().unwrap().push(buf.to_vec());
            self.inbox.lock().unwrap().extend((self.answer)(&buf[1..]));
            Ok(())
        }
        fn read_input(&self, buf: &mut [u8], _: u32) -> Result<usize> {
            let frame = self.inbox.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("timeout"))?;
            buf[..frame.len()].copy_from_slice(&frame);
            Ok(frame.len())
        }
    }

    fn wire(data: &[u8]) -> Vec<u8> {
        let mut v = vec![0u8; WIRE_LEN];
        v[1..1 + data.len()].copy_from_slice(data);
        v
    }

    /// QMK's VIA handler for a board with an RGB matrix at brightness 200.
    fn rgb_matrix_board(req: &[u8]) -> Vec<Vec<u8>> {
        let mut reply = req.to_vec();
        match req[0] {
            CMD_GET_PROTOCOL_VERSION => reply[1..3].copy_from_slice(&[0x00, 0x0C]),
            CMD_CUSTOM_GET_VALUE if req[1] == 0x03 && req[2] == VALUE_BRIGHTNESS => reply[3] = 200,
            _ => reply[0] = UNHANDLED,
        }
        vec![wire(&reply)]
    }

    fn ctx() -> crate::synth::SynthCtx {
        crate::synth::SynthCtx {
            vid: 0x3434,
            pid: 0x0281,
            usage_page: RAW_USAGE_PAGE,
            usage: RAW_USAGE,
            feature_len: 0,
            product: "Keychron Q1".into(),
            keyboard: true,
        }
    }

    #[test]
    fn claims_only_the_raw_hid_collection() {
        let info = |page, usage, out, inp| HidDeviceInfo {
            vid: 0x3434, pid: 1, usage_page: page, usage, feature_len: 0, input_len: inp, output_len: out,
            path: DevicePath::from_str_for_tests("x"), product: String::new(),
        };
        assert!(ViaDialect.claims(&info(0xFF60, 0x61, 33, 33)));
        assert!(!ViaDialect.claims(&info(0xFF60, 0x61, 65, 65)), "not a 32-byte raw HID pipe");
        assert!(!ViaDialect.claims(&info(0xFF00, 0x61, 33, 33)));
        assert!(!ViaDialect.claims(&info(0x0001, 0x06, 33, 33)));
        assert!(ViaDialect.interested(&info(0xFF60, 0x61, 65, 65)));
    }

    #[test]
    fn adopts_a_via_board_and_reads_its_brightness() {
        let board = Board::new(rgb_matrix_board);
        let s = ViaDialect.probe(&board, &ctx()).expect("a VIA board is adopted");
        assert_eq!(s.def.dialect, "via");
        assert_eq!(s.def.name, "Keychron Q1");
        assert!(s.provenance.iter().any(|l| l.contains("0x000c")), "{:?}", s.provenance);
        let b = s.def.command("brightness").expect("brightness adopted");
        assert_eq!((b.class, b.id), (CMD_CUSTOM_GET_VALUE, 0x03), "the RGB matrix channel answered");
        let set = s.def.command("set_brightness").expect("the unsaved set on the same channel");
        assert_eq!((set.class, set.id), (CMD_CUSTOM_SET_VALUE, 0x03));
        assert_eq!(s.proven_commands.get(), &vec!["brightness".to_string()], "only the read was asked");
        let out = ViaDialect.exec(&board, 0, b.class, b.id, b.size, &b.args).unwrap();
        assert_eq!(out[2], 200, "brightness lands where the readout takes it");
        let sent = board.writes.lock().unwrap();
        assert!(sent.iter().all(|w| matches!(w[1], CMD_GET_PROTOCOL_VERSION | CMD_CUSTOM_GET_VALUE)), "adoption only reads");
    }

    /// A board that keeps a brightness and stores it at its own resolution: `steps` levels across
    /// 0..=255, like a QMK backlight; `ignores` drops every set.
    struct Lit {
        level: Mutex<u8>,
        steps: u16,
        ignores: bool,
        sent: Mutex<Vec<Vec<u8>>>,
        inbox: Mutex<VecDeque<Vec<u8>>>,
    }

    impl Lit {
        fn new(level: u8, steps: u16, ignores: bool) -> Self {
            Lit { level: Mutex::new(level), steps, ignores, sent: Mutex::default(), inbox: Mutex::default() }
        }
    }

    impl Transport for Lit {
        fn set_feature(&self, _: &[u8]) -> Result<()> { bail!("no feature reports") }
        fn get_feature(&self, _: &mut [u8]) -> Result<usize> { bail!("no feature reports") }
        fn write_output(&self, buf: &[u8]) -> Result<()> {
            let req = &buf[1..];
            self.sent.lock().unwrap().push(req.to_vec());
            let mut reply = req.to_vec();
            match (req[0], req[2]) {
                (CMD_CUSTOM_SET_VALUE, VALUE_BRIGHTNESS) if !self.ignores => {
                    let step = u16::from(req[3]) * self.steps / 255;
                    *self.level.lock().unwrap() = (step * 255 / self.steps) as u8;
                }
                (CMD_CUSTOM_SET_VALUE, _) => {}
                (CMD_CUSTOM_GET_VALUE, VALUE_BRIGHTNESS) => reply[3] = *self.level.lock().unwrap(),
                _ => reply[0] = UNHANDLED,
            }
            self.inbox.lock().unwrap().push_back(wire(&reply));
            Ok(())
        }
        fn read_input(&self, buf: &mut [u8], _: u32) -> Result<usize> {
            let frame = self.inbox.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("timeout"))?;
            buf[..frame.len()].copy_from_slice(&frame);
            Ok(frame.len())
        }
    }

    fn lit_device(lit: Lit) -> (crate::device::Device, std::sync::Arc<Lit>) {
        let def = ViaDialect.probe(&Board::new(rgb_matrix_board), &ctx()).unwrap().def;
        let lit = std::sync::Arc::new(lit);
        struct Shared(std::sync::Arc<Lit>);
        impl Transport for Shared {
            fn set_feature(&self, b: &[u8]) -> Result<()> { self.0.set_feature(b) }
            fn get_feature(&self, b: &mut [u8]) -> Result<usize> { self.0.get_feature(b) }
            fn write_output(&self, b: &[u8]) -> Result<()> { self.0.write_output(b) }
            fn read_input(&self, b: &mut [u8], t: u32) -> Result<usize> { self.0.read_input(b, t) }
        }
        (crate::device::Device::with_transport(def, 0x0281, Box::new(Shared(lit.clone()))), lit)
    }

    #[test]
    fn brightness_is_set_unsaved_and_read_back() {
        let (dev, lit) = lit_device(Lit::new(40, 255, false));
        crate::capability::set_brightness(&dev, 80, crate::capability::Store::Persist).unwrap();
        assert_eq!(crate::capability::brightness_percent(&dev).unwrap(), 80);
        let sent = lit.sent.lock().unwrap();
        assert!(sent.iter().any(|r| r[0] == CMD_CUSTOM_SET_VALUE && r[1] == 0x03 && r[2] == VALUE_BRIGHTNESS));
        assert!(sent.iter().all(|r| r[0] != 0x09), "never the EEPROM save");
    }

    #[test]
    fn a_coarse_backlight_lands_on_its_nearest_step() {
        let (dev, _) = lit_device(Lit::new(0, 3, false));
        crate::capability::set_brightness(&dev, 70, crate::capability::Store::Volatile).unwrap();
        assert_eq!(crate::capability::brightness_percent(&dev).unwrap(), 67, "step 2 of 3");
    }

    #[test]
    fn a_board_that_ignores_the_write_is_an_error() {
        let (dev, _) = lit_device(Lit::new(40, 255, true));
        let err = crate::capability::set_brightness(&dev, 90, crate::capability::Store::Volatile).unwrap_err();
        assert!(format!("{err}").contains("stayed at"), "{err}");
    }

    #[test]
    fn the_learned_file_round_trips() {
        let s = ViaDialect.probe(&Board::new(rgb_matrix_board), &ctx()).unwrap();
        let text = crate::synth::emit_toml(&s);
        let mut back: crate::registry::DeviceDef = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}
{text}"));
        back.origin = s.def.origin.clone();
        assert_eq!(back, s.def);
        assert!(text.contains("channel 3 answered"), "{text}");
    }

    #[test]
    fn a_board_without_via_is_not_adopted() {
        let board = Board::new(|req| {
            let mut reply = req.to_vec();
            reply[0] = UNHANDLED;
            vec![wire(&reply)]
        });
        assert!(ViaDialect.probe(&board, &ctx()).is_none());
        let silent = Board::new(|_| Vec::new());
        assert!(ViaDialect.probe(&silent, &ctx()).is_none());
    }

    #[test]
    fn a_raw_hid_collection_that_is_not_on_a_keyboard_is_never_asked() {
        let board = Board::new(rgb_matrix_board);
        let not_a_keyboard = crate::synth::SynthCtx { keyboard: false, ..ctx() };
        assert!(ViaDialect.probe(&board, &not_a_keyboard).is_none());
        assert!(board.writes.lock().unwrap().is_empty(), "nothing was sent");
    }

    #[test]
    fn another_clients_traffic_is_skipped() {
        let board = Board::new(|req| {
            let mut stray = vec![0u8; DATA_LEN];
            stray[0] = 0x04; // someone else's keymap read
            let mut reply = req.to_vec();
            if req[0] == CMD_GET_PROTOCOL_VERSION {
                reply[1..3].copy_from_slice(&[0x00, 0x0C]);
            } else {
                reply[0] = UNHANDLED;
            }
            vec![wire(&stray), wire(&reply)]
        });
        assert_eq!(protocol_version(&board), Some(0x000C));
    }
}
