// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Report layouts as DATA, for devices whose HID descriptor doesn't describe what they send.
//!
//! A layout is the same thing a descriptor is — which bits of which report are which control — so
//! one generic bit-field decoder serves every layout file, and no device gets code of its own. A
//! layout decodes into a [`StandardPad`], so a device described this way binds exactly like any
//! other pad. Layouts ship embedded (`layouts/*.toml`) and can be added in the run root's
//! `layouts/` directory.

use crate::pad::{button, StandardPad};
use serde::Deserialize;

/// One device family's report layout.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub name: String,
    pub vendor_id: u16,
    pub product_ids: Vec<u16>,
    /// The input report this layout describes (byte 0 of the report). Other reports are ignored.
    pub report_id: u8,
    #[serde(default)]
    pub buttons: Vec<ButtonBit>,
    #[serde(default)]
    pub axes: Vec<AxisBits>,
    /// Output reports that start the device streaming this layout, for a device that is silent
    /// until told (a Switch Pro over USB with no other app driving it). Each is zero-padded to the
    /// device's output report length.
    #[serde(default)]
    pub init: Vec<InitStep>,
}

/// One start-up output report, and the input report that must answer it (by prefix), if any.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitStep {
    pub out: Vec<u8>,
    #[serde(default)]
    pub reply: Option<Vec<u8>>,
    /// Settle time after a step that expects no reply, in ms (default 30).
    #[serde(default)]
    pub wait_ms: Option<u64>,
}

/// A button: `report[byte] & mask`, and the standard-pad control it is.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ButtonBit {
    pub byte: usize,
    pub mask: u8,
    pub pad: String,
}

/// An axis: an unsigned little-endian bit field of the report, its raw range, and which
/// standard-pad axis it is. `invert` flips it (a raw axis that grows upward, say).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisBits {
    pub pad: String,
    pub bit: usize,
    pub bits: u8,
    #[serde(default)]
    pub min: u32,
    pub max: u32,
    #[serde(default)]
    pub invert: bool,
}

/// Every `layouts/*.toml` in the crate, embedded by the build script.
const EMBEDDED: &[&str] = include!(concat!(env!("OUT_DIR"), "/layouts.rs"));

/// Read an unsigned little-endian bit field.
#[must_use]
pub fn bits(report: &[u8], bit: usize, len: u8) -> Option<u32> {
    let mut v = 0u32;
    for i in 0..usize::from(len) {
        let b = bit.checked_add(i)?;
        let byte = *report.get(b / 8)?;
        v |= u32::from((byte >> (b % 8)) & 1) << i;
    }
    Some(v)
}

impl Layout {
    /// Does this layout claim device `vid:pid`?
    #[must_use]
    pub fn claims(&self, vid: u16, pid: u16) -> bool {
        self.vendor_id == vid && self.product_ids.contains(&pid)
    }

    /// Decode one report. `None` for a report this layout doesn't describe.
    #[must_use]
    pub fn decode(&self, report: &[u8]) -> Option<StandardPad> {
        if report.first() != Some(&self.report_id) {
            return None;
        }
        let mut pad = StandardPad::default();
        for b in &self.buttons {
            if report.get(b.byte)? & b.mask == 0 {
                continue;
            }
            match b.pad.as_str() {
                "left_trigger" => pad.left_trigger = 1.0,
                "right_trigger" => pad.right_trigger = 1.0,
                name => pad.buttons |= 1 << button_index(name)?,
            }
        }
        for a in &self.axes {
            let raw = bits(report, a.bit, a.bits)?.clamp(a.min, a.max);
            let span = (a.max - a.min) as f32;
            let mut v = ((raw - a.min) as f32 / span) * 2.0 - 1.0;
            if a.invert {
                v = -v;
            }
            match a.pad.as_str() {
                "left_x" => pad.left_x = v,
                "left_y" => pad.left_y = v,
                "right_x" => pad.right_x = v,
                "right_y" => pad.right_y = v,
                "left_trigger" => pad.left_trigger = (v + 1.0) / 2.0,
                "right_trigger" => pad.right_trigger = (v + 1.0) / 2.0,
                _ => return None,
            }
        }
        Some(pad)
    }

    /// Every name this layout uses must mean something; a typo refuses the layout at load.
    fn validate(&self) -> Result<(), String> {
        for b in &self.buttons {
            if !matches!(b.pad.as_str(), "left_trigger" | "right_trigger") && button_index(&b.pad).is_none() {
                return Err(format!("{}: unknown pad button '{}'", self.name, b.pad));
            }
        }
        for a in &self.axes {
            if !matches!(a.pad.as_str(), "left_x" | "left_y" | "right_x" | "right_y" | "left_trigger" | "right_trigger") {
                return Err(format!("{}: unknown pad axis '{}'", self.name, a.pad));
            }
            if a.bits == 0 || a.bits > 32 || a.max <= a.min {
                return Err(format!("{}: axis '{}' has an empty range", self.name, a.pad));
            }
        }
        Ok(())
    }
}

fn button_index(name: &str) -> Option<u16> {
    use button::*;
    Some(match name {
        "south" => SOUTH,
        "east" => EAST,
        "west" => WEST,
        "north" => NORTH,
        "left_shoulder" => LEFT_SHOULDER,
        "right_shoulder" => RIGHT_SHOULDER,
        "back" => BACK,
        "start" => START,
        "left_stick" => LEFT_STICK,
        "right_stick" => RIGHT_STICK,
        "dpad_up" => DPAD_UP,
        "dpad_down" => DPAD_DOWN,
        "dpad_left" => DPAD_LEFT,
        "dpad_right" => DPAD_RIGHT,
        "home" => HOME,
        "capture" => CAPTURE,
        _ => return None,
    })
}

/// Every known layout: embedded ones, then the run root's `layouts/*.toml`. A file that doesn't
/// parse or validate is skipped with a log line, never half-used.
#[must_use]
pub fn all() -> &'static [Layout] {
    static ALL: std::sync::OnceLock<Vec<Layout>> = std::sync::OnceLock::new();
    ALL.get_or_init(|| {
        let mut out = Vec::new();
        let mut take = |src: &str, from: &str| match toml::from_str::<Layout>(src) {
            Ok(l) => match l.validate() {
                Ok(()) => out.push(l),
                Err(e) => eprintln!("[layout] {from}: {e}"),
            },
            Err(e) => eprintln!("[layout] {from}: {e}"),
        };
        for src in EMBEDDED {
            take(src, "embedded");
        }
        if let Ok(dir) = std::fs::read_dir(crate::runroot::run_root().join("layouts")) {
            for e in dir.flatten() {
                if e.path().extension().is_some_and(|x| x == "toml") {
                    if let Ok(src) = std::fs::read_to_string(e.path()) {
                        take(&src, &e.path().display().to_string());
                    }
                }
            }
        }
        out
    })
}

/// Send a layout's start-up sequence. Each step is written as an output report padded to
/// `report_len`. A step with a `reply` is re-sent up to five times, each waiting 100 ms for an input
/// report starting with the reply, and fails the sequence if none ever comes (the retry rule SDL's
/// Switch driver uses: a pad switching baud or waking needs a moment). A step without one gets a
/// short settle before the next.
pub fn run_init(t: &dyn crate::transport::Transport, layout: &Layout, report_len: usize) -> anyhow::Result<()> {
    for (i, step) in layout.init.iter().enumerate() {
        let mut buf = step.out.clone();
        if buf.len() < report_len {
            buf.resize(report_len, 0);
        }
        let Some(want) = &step.reply else {
            t.write_output(&buf)?;
            std::thread::sleep(std::time::Duration::from_millis(step.wait_ms.unwrap_or(30)));
            continue;
        };
        let mut got = false;
        for _ in 0..5 {
            t.write_output(&buf)?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                let mut rep = vec![0u8; report_len.max(64)];
                if let Ok(n) = t.read_input(&mut rep, 20) {
                    if rep[..n].starts_with(want) {
                        got = true;
                        break;
                    }
                }
            }
            if got {
                break;
            }
        }
        anyhow::ensure!(got, "{}: start-up step {i} got no {want:02x?} reply after 5 tries", layout.name);
    }
    Ok(())
}

/// Whether layout start-up sequences may be sent (`NEURON_PAD_INIT`; see [`wake_silent_pads`]).
#[must_use]
pub fn pad_init_enabled() -> bool {
    std::env::var_os("NEURON_PAD_INIT").is_some()
}

/// When each layout-described device last sent a report it describes, by `(vid, pid)`.
static LAST_REPORT: std::sync::Mutex<Option<std::collections::HashMap<(u16, u16), std::time::Instant>>> =
    std::sync::Mutex::new(None);

/// The input path saw a described report from `vid:pid`.
pub fn note_report(vid: u16, pid: u16) {
    let mut g = LAST_REPORT.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.get_or_insert_with(Default::default).insert((vid, pid), std::time::Instant::now());
}

/// Start every connected layout device that has a start-up sequence and hasn't streamed for two
/// seconds. A device another app keeps streaming is left alone. Gated behind `NEURON_PAD_INIT`
/// until the sequence is verified on hardware through neuron (it is SDL's, per the layout file).
pub fn wake_silent_pads() {
    if !pad_init_enabled() {
        return;
    }
    let Ok(infos) = crate::transport::enumerate() else { return };
    for i in infos {
        let Some(layout) = find(i.vid, i.pid) else { continue };
        if layout.init.is_empty() || i.output_len == 0 || !(i.usage_page == 0x01 && matches!(i.usage, 0x04 | 0x05)) {
            continue;
        }
        let silent = LAST_REPORT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|m| m.get(&(i.vid, i.pid)).map(|t| t.elapsed()))
            .is_none_or(|e| e > std::time::Duration::from_secs(2));
        if !silent {
            continue;
        }
        match crate::transport::open_path(&i.path).and_then(|t| run_init(t.as_ref(), layout, usize::from(i.output_len))) {
            Ok(()) => eprintln!("[layout] started {} ({:04x}:{:04x})", layout.name, i.vid, i.pid),
            Err(e) => eprintln!("[layout] could not start {}: {e}", layout.name),
        }
    }
}

/// The layout for device `vid:pid`, if one is known.
#[must_use]
pub fn find(vid: u16, pid: u16) -> Option<&'static Layout> {
    all().iter().find(|l| l.claims(vid, pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_pro() -> Layout {
        EMBEDDED.iter().map(|src| toml::from_str::<Layout>(src).unwrap()).find(|l| l.claims(0x057E, 0x2009)).unwrap()
    }

    #[test]
    fn every_shipped_layout_parses_and_validates() {
        assert!(!EMBEDDED.is_empty());
        for src in EMBEDDED {
            let l: Layout = toml::from_str(src).unwrap();
            l.validate().unwrap();
        }
    }

    /// A live report from a resting Switch Pro (2026-09-30), first 16 bytes, zero-padded.
    fn resting() -> Vec<u8> {
        let mut r = vec![0x30, 0xA8, 0x91, 0x00, 0x80, 0x00, 0x2E, 0x78, 0x7F, 0xB9, 0xF7, 0x7B, 0x09, 0x10, 0xFD, 0x64];
        r.resize(49, 0);
        r
    }

    #[test]
    fn embedded_layouts_parse_and_validate() {
        let l = switch_pro();
        l.validate().unwrap();
        assert!(l.claims(0x057E, 0x2009));
    }

    #[test]
    fn the_bit_reader_matches_the_packed_stick_formula() {
        let r = resting();
        // SDL's formula for the left stick: x = b6 | (b7 & 0xF) << 8, y = (b7 >> 4) | b8 << 4.
        assert_eq!(bits(&r, 48, 12), Some(0x2E | ((0x78 & 0xF) << 8)));
        assert_eq!(bits(&r, 60, 12), Some((0x78 >> 4) | (0x7F << 4)));
        assert_eq!(bits(&r, 72, 12), Some(0xB9 | ((0xF7 & 0xF) << 8)));
        assert_eq!(bits(&r, 84, 12), Some((0xF7 >> 4) | (0x7B << 4)));
    }

    #[test]
    fn a_resting_switch_pro_decodes_to_a_centred_pad() {
        let pad = switch_pro().decode(&resting()).unwrap();
        // byte 4 bit 0x80 is the charging-grip flag, not a button: nothing is held.
        assert_eq!(pad.buttons, 0);
        for v in [pad.left_x, pad.left_y, pad.right_x, pad.right_y] {
            assert!(v.abs() < 0.05, "resting stick off centre: {v}");
        }
    }

    #[test]
    fn buttons_map_by_position() {
        let mut r = resting();
        r[3] = 0x08 | 0x80; // A + ZR
        r[5] = 0x02; // d-pad up
        let pad = switch_pro().decode(&r).unwrap();
        assert_eq!(pad.buttons, (1 << button::EAST) | (1 << button::DPAD_UP));
        assert_eq!(pad.right_trigger, 1.0);
    }

    #[test]
    fn other_reports_are_not_decoded() {
        let mut r = resting();
        r[0] = 0x21;
        assert!(switch_pro().decode(&r).is_none());
    }

    #[test]
    fn the_switch_pro_start_up_is_the_sdl_sequence() {
        let l = switch_pro();
        let outs: Vec<Vec<u8>> = l.init.iter().map(|s| s.out.clone()).collect();
        assert_eq!(outs[0], vec![0x80, 0x02], "handshake first");
        assert_eq!(l.init[0].reply.as_deref(), Some(&[0x81u8, 0x02][..]));
        assert_eq!(outs[outs.len() - 2][10..12], [0x03, 0x30], "selects full report mode");
        assert_eq!(outs.last().unwrap(), &vec![0x80, 0x04], "and force-USB starts the stream");
    }

    #[test]
    fn a_layout_with_an_unknown_name_is_refused() {
        let mut l = switch_pro();
        l.buttons[0].pad = "souht".into();
        assert!(l.validate().is_err());
    }
}
