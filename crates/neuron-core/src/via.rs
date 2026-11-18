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

/// Where a board answers lighting brightness: a QMK channel, or a flat id on older and Vial boards.
pub(crate) fn lighting_channel(t: &dyn Transport) -> Option<u8> {
    let version = protocol_version(t)?;
    lighting_of(t, version).brightness.map(|(id, _)| id)
}

/// Is this command spec one VIA adoption emits (the brightness read or its unsaved set, on a QMK
/// lighting channel)? The run-root check holds a learned VIA file to exactly these.
#[must_use]
pub fn is_adopted_command(c: &crate::registry::CommandSpec) -> bool {
    let tag = match c.class {
        CMD_CUSTOM_GET_VALUE => RESHAPE_BRIGHTNESS,
        CMD_CUSTOM_SET_VALUE => RESHAPE_NONE,
        _ => return false,
    };
    let shaped = if LIGHTING_CHANNELS.contains(&c.id) {
        c.args == [tag, VALUE_BRIGHTNESS]
    } else {
        matches!(c.id, FLAT_RGB_BRIGHTNESS | FLAT_BACKLIGHT_BRIGHTNESS) && c.args == [tag]
    };
    shaped && c.transaction_id.is_none()
}

/// Set lighting brightness (0..=255) through VIA's custom set (0x07), which QMK applies without
/// saving (`*_noeeprom`, `quantum/via.c` L716-L723, L544-L551, L628-L634, L799-L805 @8252b630);
/// neuron never sends the save (0x09). Read back afterwards: the board stores brightness at its own
/// resolution (a backlight may have three steps, and a capped RGB matrix reads back up to four low),
/// so the write holds when the board reached the value, came closer to it, or is within that
/// rounding; a board that stayed put is an error.
pub fn set_brightness(dev: &crate::device::Device, level: u8) -> Result<()> {
    let read = || dev.run("brightness").map(|a| a[2]);
    let before = read()?;
    let channelled = dev.def.command("set_brightness").is_some_and(|c| LIGHTING_CHANNELS.contains(&c.id));
    if channelled {
        dev.run_args("set_brightness", &[RESHAPE_NONE, VALUE_BRIGHTNESS, level])?;
    } else {
        dev.run_args("set_brightness", &[RESHAPE_NONE, level])?;
    }
    let after = read()?;
    let off = |v: u8| v.abs_diff(level);
    if after == level || off(after) < off(before) || off(after) <= 4 {
        Ok(())
    } else {
        bail!("VERIFY FAILED on brightness: asked {} for {level}, it stayed at {after}", dev.def.name)
    }
}

// ── lighting framing ─────────────────────────────────────────────────────────────────────────────
// Facts from qmk_firmware `quantum/via.h`/`via.c` @8252b630 (channelled) and @a69ab05d (flat),
// vial-qmk `quantum/via.c`, `vialrgb.c`, `vial.c` @dd43959a, and `quantum/color.c` @8252b630.

/// VIA protocol from which 0x07/0x08 carry a channel byte (`via.h` L42 @8252b630; 0x000C at
/// `qmk_firmware` @7f805cc7). 0x000B shipped both ways, so it is asked.
const CHANNELLED_SINCE: u16 = 0x000C;
const AMBIGUOUS_VERSION: u16 = 0x000B;
/// Channelled value ids: 2 effect, 4 colour as `[h, s]` (`via.h` L88-L125).
const VALUE_EFFECT: u8 = 0x02;
const VALUE_COLOUR: u8 = 0x04;
const CHANNEL_RGBLIGHT: u8 = 0x02;
const CHANNEL_RGB_MATRIX: u8 = 0x03;
/// Flat ids (protocol ≤ 0x000A, every Vial build): RGB light and RGB matrix share them.
const FLAT_RGB_BRIGHTNESS: u8 = 0x80;
const FLAT_RGB_EFFECT: u8 = 0x81;
const FLAT_RGB_COLOUR: u8 = 0x83;
const FLAT_BACKLIGHT_BRIGHTNESS: u8 = 0x09;
/// The `channel` a flat board's colour block names: its RGB ids, which carry no channel byte.
pub const FLAT_RGB: u8 = 0x80;
/// Solid colour is effect 1 on every build: rgb_matrix SOLID_COLOR, rgblight STATIC_LIGHT
/// (`solid_color_anim.h` L1 and `rgblight_modes.h` L1-L2 are unguarded).
const SOLID_EFFECT: u8 = 0x01;
/// Fillers for a lighting read: a value id the firmware doesn't handle echoes the request
/// unchanged with no 0xFF (`via.c` L523-L542), so an answer is one that overwrote the filler.
const FILLERS: [u8; 2] = [0xA5, 0x5A];

/// Vial: keyboard id (not an echo: protocol u32 LE at 0..3, VialRGB flag at byte 12 bit 0,
/// `vial.c` L94-L107), and VialRGB's subcommands on the flat lighting get/set (`vialrgb.h` L7-L22).
const CMD_VIAL: u8 = 0xFE;
const VIAL_GET_KEYBOARD_ID: u8 = 0x00;
const VIAL_MIN_PROTOCOL: u32 = 4;
const VIALRGB_GET_INFO: u8 = 0x40;
const VIALRGB_MODE: u8 = 0x41;
const VIALRGB_GET_NUMBER_LEDS: u8 = 0x43;
const VIALRGB_GET_LED_INFO: u8 = 0x44;
const VIALRGB_DIRECT_FASTSET: u8 = 0x42;
const VIALRGB_PROTOCOL: u16 = 1;
const VIALRGB_DIRECT: u16 = 1;
/// LEDs a fastset carries (`vialrgb.c` L90-L110). Only LEDs 0-255 are addressable (L146).
const FASTSET_LEDS: usize = 9;
const NOT_A_KEY: u8 = 0xFF;
/// QMK's LED coordinate space (`rgb_matrix_types.h`): x 0..=224, y 0..=64.
const LED_X_SPAN: u16 = 225;
const LED_Y_SPAN: u16 = 65;

/// Is `ch` a lighting channel a colour or brightness block may name?
#[must_use]
pub fn is_lighting_channel(ch: u8) -> bool {
    LIGHTING_CHANNELS.contains(&ch) || ch == FLAT_RGB
}

/// Like [`ask`], matching the reply on the first `echo_len` data bytes only, for a request whose
/// tail the answer overwrites.
fn ask_echo(t: &dyn Transport, command: u8, data: &[u8], echo_len: usize) -> Answer {
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    if t.write_output(&request(command, data)).is_err() {
        return Answer::Silent;
    }
    let mut echo = vec![command];
    echo.extend_from_slice(&data[..echo_len.min(data.len())]);
    await_reply(t, &echo)
}

/// Read `n` bytes of a lighting value addressed by `prefix` (channel and value id, or a flat id).
/// `None` when the board doesn't handle it, including the silent echo of an id it doesn't know.
fn read_value(t: &dyn Transport, prefix: &[u8], n: usize) -> Option<Vec<u8>> {
    let mut answers = Vec::with_capacity(2);
    for filler in FILLERS {
        let mut data = prefix.to_vec();
        data.extend(std::iter::repeat_n(filler, n));
        match ask_echo(t, CMD_CUSTOM_GET_VALUE, &data, prefix.len()) {
            Answer::Reply(r) => answers.push(r[1 + prefix.len()..1 + prefix.len() + n].to_vec()),
            Answer::Unhandled | Answer::Silent => return None,
        }
    }
    let echoed = |v: &[u8], f: u8| v.iter().all(|&b| b == f);
    if echoed(&answers[0], FILLERS[0]) && echoed(&answers[1], FILLERS[1]) {
        return None;
    }
    answers.pop()
}

/// Write a lighting value (RAM only: every value neuron sets goes through QMK's `_noeeprom` path).
fn write_value(t: &dyn Transport, prefix: &[u8], data: &[u8]) -> Result<()> {
    let mut req = prefix.to_vec();
    req.extend_from_slice(data);
    match ask(t, CMD_CUSTOM_SET_VALUE, &req) {
        Answer::Reply(_) => Ok(()),
        Answer::Unhandled => bail!("the board doesn't handle that lighting value"),
        Answer::Silent => bail!("the board didn't answer"),
    }
}

/// Is a board on channelled lighting ids? `version` is its VIA protocol.
fn channelled(t: &dyn Transport, version: u16) -> bool {
    match version {
        v if v >= CHANNELLED_SINCE => true,
        AMBIGUOUS_VERSION => matches!(ask(t, CMD_CUSTOM_GET_VALUE, &[FLAT_RGB_BRIGHTNESS]), Answer::Unhandled),
        _ => false,
    }
}

/// Where a board's whole-board RGB values live: effect, colour and brightness prefixes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb3 {
    channel: u8,
}

impl Rgb3 {
    fn effect(self) -> Vec<u8> {
        if self.channel == FLAT_RGB { vec![FLAT_RGB_EFFECT] } else { vec![self.channel, VALUE_EFFECT] }
    }
    fn colour(self) -> Vec<u8> {
        if self.channel == FLAT_RGB { vec![FLAT_RGB_COLOUR] } else { vec![self.channel, VALUE_COLOUR] }
    }
    fn brightness(self) -> Vec<u8> {
        if self.channel == FLAT_RGB { vec![FLAT_RGB_BRIGHTNESS] } else { vec![self.channel, VALUE_BRIGHTNESS] }
    }
    /// The brightness byte that shows value `v`. RGB matrix stores `scale8(x, 255)` = `x*255 >> 8`
    /// (`via.c` L721-L723), one short of `x`; rgblight and the flat ids take it as is.
    fn brightness_wire(self, v: u8) -> u8 {
        if self.channel == CHANNEL_RGB_MATRIX {
            ((u16::from(v) * 256).div_ceil(255)).min(255) as u8
        } else {
            v
        }
    }
}

// ── QMK colour maths ─────────────────────────────────────────────────────────────────────────────

/// QMK's `hsv_to_rgb` (`quantum/color.c` L22-L110 @8252b630), without the optional CIE curve: the
/// colour a board shows for `[h, s, v]`. The hue wheel's period is 255.
#[must_use]
pub fn qmk_hsv_to_rgb([h, s, v]: [u8; 3]) -> [u8; 3] {
    if s == 0 {
        return [v, v, v];
    }
    let (h, s, v) = (u16::from(h), u16::from(s), u16::from(v));
    let region = h * 6 / 255;
    let rem = ((h * 2 - region * 85) * 3) & 0xFF;
    let p = (v * (255 - s)) >> 8;
    let q = (v * (255 - ((s * rem) >> 8))) >> 8;
    let t = (v * (255 - ((s * (255 - rem)) >> 8))) >> 8;
    let (r, g, b) = match region {
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        5 => (v, p, q),
        _ => (v, t, p),
    };
    [r as u8, g as u8, b as u8]
}

/// The `[h, s, v]` whose QMK rendering is closest to `c`: a first guess from the usual hexcone,
/// refined by searching around it with [`qmk_hsv_to_rgb`] itself, so the integer maths' own losses
/// are accounted for (within 2 of any colour, per channel).
#[must_use]
pub fn rgb_to_qmk_hsv(c: crate::lighting::Rgb) -> [u8; 3] {
    let (r, g, b) = (i32::from(c.r), i32::from(c.g), i32::from(c.b));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max == 0 {
        return [0, 0, 0];
    }
    let s0 = ((max - min) * 255 + max / 2) / max;
    let deg = if max == min {
        0.0
    } else if max == r {
        60.0 * ((g - b) as f64 / f64::from(max - min)).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) as f64 / f64::from(max - min) + 2.0)
    } else {
        60.0 * ((r - g) as f64 / f64::from(max - min) + 4.0)
    };
    let h0 = (deg * 255.0 / 360.0).round() as i32;
    let err = |hsv: [u8; 3]| {
        let o = qmk_hsv_to_rgb(hsv);
        let d = [(i32::from(o[0]) - r).abs(), (i32::from(o[1]) - g).abs(), (i32::from(o[2]) - b).abs()];
        (d[0].max(d[1]).max(d[2]), d[0] + d[1] + d[2])
    };
    let mut best = [h0.rem_euclid(255) as u8, s0.clamp(0, 255) as u8, max as u8];
    let mut best_err = err(best);
    for dv in 0..=3 {
        let v = (max + dv).min(255) as u8;
        for ds in -3..=3 {
            let s = (s0 + ds).clamp(0, 255) as u8;
            for dh in -3..=3 {
                let h = (h0 + dh).rem_euclid(255) as u8;
                let e = err([h, s, v]);
                if e < best_err {
                    (best, best_err) = ([h, s, v], e);
                }
            }
        }
    }
    best
}

// ── adoption ─────────────────────────────────────────────────────────────────────────────────────

/// What a board's VIA lighting can do: the brightness value it answers, a whole-board colour, and
/// VialRGB's per-LED colour.
#[derive(Debug, Default)]
struct Lighting {
    /// `(spec id, value args)` of the brightness read: a channel with value 1, or a flat id.
    brightness: Option<(u8, Vec<u8>)>,
    colour: Option<u8>,
    vialrgb: Option<crate::lighting::LightingDef>,
}

fn lighting_of(t: &dyn Transport, version: u16) -> Lighting {
    let mut out = Lighting::default();
    if channelled(t, version) {
        let ch = LIGHTING_CHANNELS.into_iter().find(|&ch| read_value(t, &[ch, VALUE_BRIGHTNESS], 1).is_some());
        if let Some(ch) = ch {
            out.brightness = Some((ch, vec![VALUE_BRIGHTNESS]));
            let colourful = matches!(ch, CHANNEL_RGB_MATRIX | CHANNEL_RGBLIGHT);
            if colourful && read_value(t, &[ch, VALUE_COLOUR], 2).is_some() {
                out.colour = Some(ch);
            }
        }
        return out;
    }
    out.vialrgb = vialrgb(t, version);
    if read_value(t, &[FLAT_RGB_BRIGHTNESS], 1).is_some() {
        out.brightness = Some((FLAT_RGB_BRIGHTNESS, Vec::new()));
        if out.vialrgb.is_none() && read_value(t, &[FLAT_RGB_COLOUR], 2).is_some() {
            out.colour = Some(FLAT_RGB);
        }
    } else if read_value(t, &[FLAT_BACKLIGHT_BRIGHTNESS], 1).is_some() {
        out.brightness = Some((FLAT_BACKLIGHT_BRIGHTNESS, Vec::new()));
    }
    out
}

/// A Vial board's keyboard id: `(vial protocol, VialRGB flag)`. The answer is not an echo, so the
/// first frame back is taken.
fn vial_keyboard_id(t: &dyn Transport) -> Option<(u32, bool)> {
    let wire = t.wire_lock();
    let _wire = wire.as_ref().map(|w| w.acquire());
    t.write_output(&request(CMD_VIAL, &[VIAL_GET_KEYBOARD_ID])).ok()?;
    let mut buf = [0u8; WIRE_LEN];
    for _ in 0..MAX_SILENT_WINDOWS {
        let Ok(n) = t.read_input(&mut buf, READ_WINDOW_MS) else { continue };
        let data = data_of(&buf[..n.min(WIRE_LEN)])?;
        if data[0] == UNHANDLED || data[..2] == [CMD_VIAL, VIAL_GET_KEYBOARD_ID] {
            return None;
        }
        return Some((u32::from_le_bytes([data[0], data[1], data[2], data[3]]), data[12] & 1 != 0));
    }
    None
}

/// A VialRGB get: subcommand `sub` with `args`, answered over the request.
fn vialrgb_get(t: &dyn Transport, sub: u8, args: &[u8]) -> Option<[u8; DATA_LEN]> {
    let mut data = vec![sub];
    data.extend_from_slice(args);
    match ask_echo(t, CMD_CUSTOM_GET_VALUE, &data, 1) {
        Answer::Reply(r) => Some(r),
        Answer::Unhandled | Answer::Silent => None,
    }
}

/// VialRGB per-LED lighting: every LED on the frame its key matrix spans (an LED that is not a key,
/// like underglow, at its place by position). `None` without VialRGB.
fn vialrgb(t: &dyn Transport, version: u16) -> Option<crate::lighting::LightingDef> {
    if version < 9 {
        return None;
    }
    let (vial, flagged) = vial_keyboard_id(t)?;
    if vial < VIAL_MIN_PROTOCOL || !flagged {
        return None;
    }
    let info = vialrgb_get(t, VIALRGB_GET_INFO, &[])?;
    if u16::from_le_bytes([info[2], info[3]]) != VIALRGB_PROTOCOL {
        return None;
    }
    let count = vialrgb_get(t, VIALRGB_GET_NUMBER_LEDS, &[]).map(|r| u16::from_le_bytes([r[2], r[3]]))?;
    if count == 0 || count > 256 {
        return None;
    }
    let mut leds = Vec::with_capacity(usize::from(count));
    for i in 0..count {
        let r = vialrgb_get(t, VIALRGB_GET_LED_INFO, &[i as u8, 0])?;
        leds.push((i, r[2], r[3], r[5], r[6]));
    }
    let keyed = leds.iter().filter(|l| l.3 != NOT_A_KEY && l.4 != NOT_A_KEY);
    let rows = keyed.clone().map(|l| l.3).max().map_or(1, |r| r + 1);
    let cols = keyed.map(|l| l.4).max().map_or(1, |c| c + 1);
    let leds = leds
        .into_iter()
        .map(|(i, x, y, row, col)| {
            let (row, col) = if row == NOT_A_KEY || col == NOT_A_KEY {
                (
                    (u16::from(y) * u16::from(rows) / LED_Y_SPAN).min(u16::from(rows) - 1),
                    (u16::from(x) * u16::from(cols) / LED_X_SPAN).min(u16::from(cols) - 1),
                )
            } else {
                (u16::from(row), u16::from(col))
            };
            [i, row, col]
        })
        .collect();
    Some(family_lighting(crate::lighting::Protocol::Vialrgb, rows, cols, None, leds))
}

fn family_lighting(
    protocol: crate::lighting::Protocol,
    rows: u8,
    cols: u8,
    channel: Option<u8>,
    leds: Vec<[u16; 3]>,
) -> crate::lighting::LightingDef {
    crate::lighting::LightingDef {
        protocol,
        rows,
        cols,
        varstore: 0,
        led_id: 0,
        effects: std::collections::BTreeMap::new(),
        custom_id: 0,
        effect: crate::registry::CommandSpec::default(),
        custom_frame: None,
        brightness: None,
        framing: crate::lighting::Framing::default(),
        display_args: None,
        channel,
        leds,
    }
}

// ── painting ─────────────────────────────────────────────────────────────────────────────────────

/// What neuron found on the board and puts back at release.
enum Held {
    /// One colour for the whole board: the effect, `[h, s]` and brightness read at take.
    Board { at: Rgb3, effect: Option<u8>, hs: Option<[u8; 2]>, brightness: Option<u8>, sent: Option<[u8; 3]> },
    /// VialRGB direct mode: the mode read at take (`mode u16 LE, speed, h, s, v`), the board's
    /// brightness cap, and each LED's last `[h, s, v]`.
    Vial { mode: [u8; 6], max: u8, sent: Vec<Option<[u8; 3]>> },
}

/// Neuron's hold on a VIA board's lighting.
pub(crate) struct ColourLink {
    held: Held,
    verified: bool,
}

/// Take a board's lighting: remember what it shows, then switch it to what neuron paints through
/// (solid colour, or VialRGB direct). RAM only; nothing is saved.
pub(crate) fn take_lighting(t: &dyn Transport, def: &crate::lighting::LightingDef) -> Result<ColourLink> {
    use crate::lighting::Protocol;
    let held = match def.protocol {
        Protocol::Vialrgb => {
            let mode = vialrgb_get(t, VIALRGB_MODE, &[]).ok_or_else(|| anyhow::anyhow!("VialRGB mode unanswered"))?;
            let info = vialrgb_get(t, VIALRGB_GET_INFO, &[]).ok_or_else(|| anyhow::anyhow!("VialRGB info unanswered"))?;
            let saved = [mode[2], mode[3], mode[4], mode[5], mode[6], mode[7]];
            let direct = VIALRGB_DIRECT.to_le_bytes();
            write_value(t, &[VIALRGB_MODE], &[direct[0], direct[1], saved[2], saved[3], saved[4], saved[5]])?;
            let now = vialrgb_get(t, VIALRGB_MODE, &[]).map(|r| u16::from_le_bytes([r[2], r[3]]));
            anyhow::ensure!(now == Some(VIALRGB_DIRECT), "the board didn't take VialRGB direct mode");
            Held::Vial { mode: saved, max: info[4].max(1), sent: vec![None; def.leds.len()] }
        }
        _ => {
            let at = Rgb3 { channel: def.channel.ok_or_else(|| anyhow::anyhow!("no lighting channel"))? };
            let effect = read_value(t, &at.effect(), 1).map(|v| v[0]);
            let hs = read_value(t, &at.colour(), 2).map(|v| [v[0], v[1]]);
            let brightness = read_value(t, &at.brightness(), 1).map(|v| v[0]);
            write_value(t, &at.effect(), &[SOLID_EFFECT])?;
            Held::Board { at, effect, hs, brightness, sent: None }
        }
    };
    Ok(ColourLink { held, verified: false })
}

impl ColourLink {
    /// Paint a frame: the whole board takes its mean colour; a VialRGB board takes each LED's own,
    /// sending only the LEDs that changed. The first colour is read back.
    pub(crate) fn paint(&mut self, t: &dyn Transport, def: &crate::lighting::LightingDef, px: &[crate::lighting::Rgb]) -> crate::dialect::Sent {
        use crate::dialect::Sent;
        let verified = self.verified;
        let result = match &mut self.held {
            Held::Board { at, sent, .. } => {
                let [h, s, v] = rgb_to_qmk_hsv(crate::family::mean_of(px));
                let before = *sent;
                let mut r = Ok(());
                if before.map(|p| [p[0], p[1]]) != Some([h, s]) {
                    r = r.and_then(|()| write_value(t, &at.colour(), &[h, s]));
                }
                if before.map(|p| p[2]) != Some(v) {
                    r = r.and_then(|()| write_value(t, &at.brightness(), &[at.brightness_wire(v)]));
                }
                let r = r.and_then(|()| {
                    if verified {
                        return Ok(());
                    }
                    let back = read_value(t, &at.colour(), 2);
                    anyhow::ensure!(back.as_deref() == Some(&[h, s][..]), "the board didn't take the colour");
                    Ok(())
                });
                *sent = r.is_ok().then_some([h, s, v]);
                r
            }
            Held::Vial { max, sent, .. } => {
                let colours = crate::family::led_colours(def, px);
                let want: Vec<[u8; 3]> = colours
                    .iter()
                    .map(|&(_, c)| {
                        let [h, s, v] = rgb_to_qmk_hsv(c);
                        [h, s, (u16::from(v) * u16::from(*max) / 255) as u8]
                    })
                    .collect();
                let mut r = Ok(());
                let mut i = 0;
                while i < want.len() && r.is_ok() {
                    if sent[i] == Some(want[i]) {
                        i += 1;
                        continue;
                    }
                    let end = (i + FASTSET_LEDS).min(want.len());
                    let first = (colours[i].0).to_le_bytes();
                    let mut data = vec![first[0], first[1], (end - i) as u8];
                    for hsv in &want[i..end] {
                        data.extend_from_slice(hsv);
                    }
                    r = write_value(t, &[VIALRGB_DIRECT_FASTSET], &data);
                    if r.is_ok() {
                        for (slot, hsv) in sent[i..end].iter_mut().zip(&want[i..end]) {
                            *slot = Some(*hsv);
                        }
                    }
                    i = end;
                }
                if r.is_err() {
                    sent.iter_mut().for_each(|s| *s = None);
                }
                r
            }
        };
        match result {
            Ok(()) => {
                self.verified = true;
                Sent::Taken
            }
            Err(_) => Sent::Lost,
        }
    }

    /// Put back what the board showed at take: VialRGB's mode exactly; a whole-board colour's
    /// colour, brightness (nudged up when QMK's rounding read it back low) and effect.
    pub(crate) fn release(&self, t: &dyn Transport) {
        match &self.held {
            Held::Vial { mode, .. } => {
                let _ = write_value(t, &[VIALRGB_MODE], mode);
            }
            Held::Board { at, effect, hs, brightness, .. } => {
                if let Some(hs) = hs {
                    let _ = write_value(t, &at.colour(), hs);
                }
                if let Some(b) = *brightness {
                    let mut wire = b;
                    for _ in 0..3 {
                        if write_value(t, &at.brightness(), &[wire]).is_err() {
                            break;
                        }
                        match read_value(t, &at.brightness(), 1).map(|v| v[0]) {
                            Some(now) if now < b && wire < 255 => wire = wire.saturating_add(b - now),
                            _ => break,
                        }
                    }
                }
                if let Some(e) = *effect {
                    let _ = write_value(t, &at.effect(), &[e]);
                }
            }
        }
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
        let found = lighting_of(t, version);
        let brightness_at = |tag: u8, value: &[u8]| {
            let mut args = vec![tag];
            args.extend_from_slice(value);
            args
        };
        if let Some((id, value)) = &found.brightness {
            commands.insert(
                "brightness".to_string(),
                CommandSpec { class: CMD_CUSTOM_GET_VALUE, id: *id, size: 0, args: brightness_at(RESHAPE_BRIGHTNESS, value), transaction_id: None },
            );
        }
        // Only the read was asked; the set on the same id is proven by read-back at each use.
        let answered: Vec<String> = commands.keys().cloned().collect();
        if let Some((id, value)) = &found.brightness {
            commands.insert(
                "set_brightness".to_string(),
                CommandSpec { class: CMD_CUSTOM_SET_VALUE, id: *id, size: 0, args: brightness_at(RESHAPE_NONE, value), transaction_id: None },
            );
        }
        let lighting = found.vialrgb.clone().or_else(|| {
            found.colour.map(|ch| family_lighting(crate::lighting::Protocol::Via, 1, 1, Some(ch), vec![[0, 0, 0]]))
        });
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
            lighting,
            side_plates: None,
            side_plate_binds: std::collections::BTreeMap::new(),
            fn_binds: std::collections::BTreeMap::new(),
            events: None,
            buttons: Vec::new(),
            custody: None,
        };
        let mut s = Synthesis::from_probe(def, answered, Heuristic(0), None, Heuristic(0), String::new(), Some(roundtrip_ms));
        if let Some((id, value)) = &found.brightness {
            let at = if value.is_empty() { format!("flat id {id:#04x}") } else { format!("QMK lighting channel {id}") };
            s.provenance.push(format!("#   - brightness: {at} answered (VIA protocol {version:#06x})."));
        }
        if let Some(l) = &s.def.lighting {
            let what = match l.protocol {
                crate::lighting::Protocol::Vialrgb => format!("VialRGB, {} LEDs on a {}x{} frame", l.leds.len(), l.rows, l.cols),
                _ => "one colour for the whole board, read back on its colour value".into(),
            };
            s.provenance.push(format!("#   - lighting: {what}."));
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

#[cfg(test)]
#[path = "via_lighting_tests.rs"]
mod lighting_tests;
