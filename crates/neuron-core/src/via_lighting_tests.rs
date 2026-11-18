// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! VIA and VialRGB colour lighting against a modelled QMK board.

#![cfg(test)]

use super::*;
use crate::lighting::{Lights, Protocol, Rgb};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// A board's lighting as QMK keeps it in RAM.
#[derive(Clone, Copy, Default)]
struct State {
    enabled: bool,
    mode: u8,
    h: u8,
    s: u8,
    v: u8,
}

/// One VialRGB LED: position, flags, and its matrix row and column (0xFF when it isn't a key).
type Led = (u8, u8, u8, u8);

/// A modelled QMK board: VIA protocol `version`, an RGB matrix on channel 3 (or the flat ids),
/// and optionally VialRGB. Unknown value ids echo the request unchanged, as QMK's handler does.
#[derive(Default)]
struct Qmk {
    version: u16,
    rgb: bool,
    state: Mutex<State>,
    vial: Option<Vec<Led>>,
    vial_mode: Mutex<[u8; 6]>,
    direct: Mutex<Vec<[u8; 3]>>,
    sent: Mutex<Vec<Vec<u8>>>,
    inbox: Mutex<VecDeque<Vec<u8>>>,
}

impl Qmk {
    fn requests(&self) -> Vec<Vec<u8>> {
        self.sent.lock().unwrap().clone()
    }

    fn answer(&self, req: &[u8]) -> Vec<u8> {
        let mut r = req.to_vec();
        let mut st = self.state.lock().unwrap();
        let channelled = self.version >= CHANNELLED_SINCE;
        match req[0] {
            CMD_GET_PROTOCOL_VERSION => r[1..3].copy_from_slice(&self.version.to_be_bytes()),
            CMD_VIAL if self.vial.is_some() => {
                r = vec![0; DATA_LEN];
                r[0..4].copy_from_slice(&6u32.to_le_bytes());
                r[12] = 1;
            }
            CMD_CUSTOM_GET_VALUE | CMD_CUSTOM_SET_VALUE if channelled => {
                let set = req[0] == CMD_CUSTOM_SET_VALUE;
                match (req[1], req[2]) {
                    (CHANNEL_RGB_MATRIX, v) if self.rgb => match (v, set) {
                        (VALUE_BRIGHTNESS, false) => r[3] = st.v,
                        (VALUE_BRIGHTNESS, true) if st.enabled => st.v = ((u16::from(req[3]) * 255) >> 8) as u8,
                        (VALUE_EFFECT, false) => r[3] = if st.enabled { st.mode } else { 0 },
                        (VALUE_EFFECT, true) => {
                            st.enabled = req[3] != 0;
                            if st.enabled {
                                st.mode = req[3];
                            }
                        }
                        (VALUE_COLOUR, false) => r[3..5].copy_from_slice(&[st.h, st.s]),
                        (VALUE_COLOUR, true) if st.enabled => (st.h, st.s) = (req[3], req[4]),
                        _ => {}
                    },
                    (ch, _) if ch > 5 => r[0] = UNHANDLED,
                    (_, _) if !self.rgb => r[0] = UNHANDLED,
                    _ => {}
                }
            }
            CMD_CUSTOM_GET_VALUE | CMD_CUSTOM_SET_VALUE => {
                let set = req[0] == CMD_CUSTOM_SET_VALUE;
                match (req[1], set) {
                    (FLAT_RGB_BRIGHTNESS, false) if self.rgb => r[2] = st.v,
                    (FLAT_RGB_BRIGHTNESS, true) if self.rgb && st.enabled => st.v = req[2],
                    (FLAT_RGB_EFFECT, false) if self.rgb => r[2] = if st.enabled { st.mode } else { 0 },
                    (FLAT_RGB_EFFECT, true) if self.rgb => {
                        st.enabled = req[2] != 0;
                        if st.enabled {
                            st.mode = req[2];
                        }
                    }
                    (FLAT_RGB_COLOUR, false) if self.rgb => r[2..4].copy_from_slice(&[st.h, st.s]),
                    (FLAT_RGB_COLOUR, true) if self.rgb && st.enabled => (st.h, st.s) = (req[2], req[3]),
                    (VIALRGB_GET_INFO, false) if self.vial.is_some() => r[2..5].copy_from_slice(&[1, 0, 200]),
                    (VIALRGB_MODE, false) if self.vial.is_some() => r[2..8].copy_from_slice(&*self.vial_mode.lock().unwrap()),
                    (VIALRGB_MODE, true) if self.vial.is_some() => self.vial_mode.lock().unwrap().copy_from_slice(&req[2..8]),
                    (VIALRGB_GET_NUMBER_LEDS, false) => {
                        if let Some(leds) = &self.vial {
                            r[2..4].copy_from_slice(&(leds.len() as u16).to_le_bytes());
                        }
                    }
                    (VIALRGB_GET_LED_INFO, false) => {
                        if let Some(&(x, y, row, col)) = self.vial.as_ref().and_then(|l| l.get(usize::from(req[2]))) {
                            r[2..7].copy_from_slice(&[x, y, 0x04, row, col]);
                        }
                    }
                    (VIALRGB_DIRECT_FASTSET, true) if self.vial.is_some() => {
                        let first = usize::from(u16::from_le_bytes([req[2], req[3]]));
                        let mut direct = self.direct.lock().unwrap();
                        for i in 0..usize::from(req[4]) {
                            let at = 5 + 3 * i;
                            if let Some(slot) = direct.get_mut(first + i) {
                                *slot = [req[at], req[at + 1], req[at + 2].min(200)];
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => r[0] = UNHANDLED,
        }
        let mut w = vec![0u8; WIRE_LEN];
        w[1..1 + r.len().min(DATA_LEN)].copy_from_slice(&r[..r.len().min(DATA_LEN)]);
        w
    }
}

struct Wire(Arc<Qmk>);

impl Transport for Wire {
    fn set_feature(&self, _: &[u8]) -> Result<()> {
        bail!("no feature reports")
    }

    fn get_feature(&self, _: &mut [u8]) -> Result<usize> {
        bail!("no feature reports")
    }

    fn write_output(&self, buf: &[u8]) -> Result<()> {
        self.0.sent.lock().unwrap().push(buf[1..].to_vec());
        let reply = self.0.answer(&buf[1..]);
        self.0.inbox.lock().unwrap().push_back(reply);
        Ok(())
    }

    fn read_input(&self, buf: &mut [u8], _: u32) -> Result<usize> {
        let frame = self.0.inbox.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("timeout"))?;
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }
}

fn ctx() -> crate::synth::SynthCtx {
    crate::synth::SynthCtx {
        vid: 0xFEED,
        pid: 0x0001,
        usage_page: RAW_USAGE_PAGE,
        usage: RAW_USAGE,
        feature_len: 0,
        product: "Board".into(),
        keyboard: true,
    }
}

fn adopt(q: &Arc<Qmk>) -> crate::registry::DeviceDef {
    ViaDialect.probe(&Wire(q.clone()), &ctx()).expect("a VIA board is adopted").def
}

fn device(q: &Arc<Qmk>, unit: &str) -> crate::device::Device {
    let mut d = crate::device::Device::with_transport(adopt(q), 0x0001, Box::new(Wire(q.clone())));
    d.dpi_unit = unit.into();
    d
}

fn matrix_board(state: State) -> Arc<Qmk> {
    Arc::new(Qmk { version: 0x000D, rgb: true, state: Mutex::new(state), ..Qmk::default() })
}

#[test]
fn a_board_that_only_echoes_is_not_given_lighting_it_never_answered() {
    // Flat firmware with no RGB: every lighting id echoes the request unchanged.
    let q = Arc::new(Qmk { version: 0x0009, ..Qmk::default() });
    let def = adopt(&q);
    assert!(def.command("brightness").is_none(), "an echo is not an answer");
    assert!(def.lighting.is_none());
    // and on channelled firmware, a board whose lighting isn't compiled answers 0xFF
    let q = Arc::new(Qmk { version: 0x000D, ..Qmk::default() });
    assert!(adopt(&q).lighting.is_none());
}

#[test]
fn a_matrix_board_gets_one_colour_and_puts_everything_back() {
    let q = matrix_board(State { enabled: true, mode: 7, h: 30, s: 40, v: 200 });
    let d = device(&q, "unit-matrix");
    let l = d.def.lighting.clone().expect("whole-board colour");
    assert_eq!((l.protocol, l.channel, l.rows, l.cols), (Protocol::Via, Some(CHANNEL_RGB_MATRIX), 1, 1));
    assert_eq!(crate::defcheck::check(&d.def), Ok(()), "what adoption learns loads");

    let lights = Lights::new(&d, l);
    lights.paint_frame(&[Rgb::new(0, 0, 255)]).unwrap();
    let st = *q.state.lock().unwrap();
    assert_eq!((st.mode, st.h, st.s), (SOLID_EFFECT, 170, 255), "solid blue: QMK's hue 170");
    assert_eq!(qmk_hsv_to_rgb([st.h, st.s, st.v]), [0, 0, 200], "full blue at the board's own brightness");

    let before = q.requests().len();
    lights.paint_frame(&[Rgb::new(0, 0, 255)]).unwrap();
    assert_eq!(q.requests().len(), before, "an unchanged colour sends nothing");

    d.release_custody().ok();
    let st = *q.state.lock().unwrap();
    assert_eq!((st.enabled, st.mode, st.h, st.s, st.v), (true, 7, 30, 40, 200), "effect, colour and brightness put back");
    assert!(q.requests().iter().all(|r| r[0] != 0x09), "never the EEPROM save");
}

#[test]
fn the_brightness_slider_scales_the_colour_neuron_paints() {
    let q = matrix_board(State { enabled: true, mode: 1, h: 0, s: 0, v: 255 });
    let d = device(&q, "unit-slider");
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    lights.paint_frame(&[Rgb::new(255, 0, 0)]).unwrap();
    crate::capability::set_brightness(&d, 50, crate::capability::Store::Volatile).unwrap();
    let half = q.state.lock().unwrap().v;
    assert!((120..=130).contains(&half), "half of full red, at once: {half}");
    assert_eq!(crate::capability::brightness_percent(&d).unwrap(), 50, "the readout shows the slider, not the frame");
    lights.paint_frame(&[Rgb::new(255, 0, 0)]).unwrap();
    assert_eq!(q.state.lock().unwrap().v, half, "the next frame keeps the slider's level");
    lights.paint_frame(&[Rgb::new(128, 0, 0)]).unwrap();
    let quarter = q.state.lock().unwrap().v;
    assert!((60..=66).contains(&quarter), "a dimmer frame, scaled the same: {quarter}");
    d.release_custody().ok();
    // an RGB matrix stores VIA brightness as `x*255 >> 8`, so 254 is as close to 255 as VIA gets
    assert_eq!(q.state.lock().unwrap().v, 254, "the board's own brightness put back");
}

#[test]
fn a_disabled_board_is_lit_for_neuron_and_turned_off_again() {
    let q = matrix_board(State { enabled: false, mode: 4, h: 0, s: 0, v: 90 });
    let d = device(&q, "unit-dark");
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    lights.paint_frame(&[Rgb::new(255, 0, 0)]).unwrap();
    assert!(q.state.lock().unwrap().enabled);
    d.release_custody().ok();
    assert!(!q.state.lock().unwrap().enabled, "it was off, and it is off again");
}

#[test]
fn a_flat_board_takes_colour_on_its_flat_ids() {
    let q = Arc::new(Qmk { version: 0x000A, rgb: true, state: Mutex::new(State { enabled: true, mode: 1, h: 0, s: 0, v: 100 }), ..Qmk::default() });
    let d = device(&q, "unit-flat");
    let b = d.def.command("brightness").expect("brightness on the flat id");
    assert_eq!((b.id, b.args.as_slice()), (FLAT_RGB_BRIGHTNESS, &[RESHAPE_BRIGHTNESS][..]));
    assert_eq!(d.def.lighting.as_ref().and_then(|l| l.channel), Some(FLAT_RGB));
    crate::capability::set_brightness(&d, 60, crate::capability::Store::Volatile).unwrap();
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    lights.paint_frame(&[Rgb::new(0, 255, 0)]).unwrap();
    let st = *q.state.lock().unwrap();
    assert_eq!(qmk_hsv_to_rgb([st.h, st.s, st.v]), [0, 153, 0], "green at the 60% set before");
    d.release_custody().ok();
}

#[test]
fn a_vialrgb_board_paints_each_led_in_direct_mode_and_gets_its_mode_back() {
    // a 2x3 key matrix plus one underglow LED at the right edge
    let leds: Vec<Led> = vec![(0, 0, 0, 0), (112, 0, 0, 1), (224, 0, 0, 2), (0, 64, 1, 0), (112, 64, 1, 1), (224, 64, 1, 2), (224, 64, 0xFF, 0xFF)];
    let q = Arc::new(Qmk {
        version: 0x0009,
        vial: Some(leds.clone()),
        vial_mode: Mutex::new([5, 0, 128, 10, 20, 150]),
        direct: Mutex::new(vec![[0; 3]; leds.len()]),
        ..Qmk::default()
    });
    let d = device(&q, "unit-vial");
    let l = d.def.lighting.clone().expect("VialRGB lighting");
    assert_eq!((l.protocol, l.rows, l.cols), (Protocol::Vialrgb, 2, 3));
    assert_eq!(l.leds[6], [6, 1, 2], "the underglow LED at the bottom right sits there");
    assert_eq!(crate::defcheck::check(&d.def), Ok(()));

    let lights = Lights::new(&d, l);
    let frame: Vec<Rgb> = (0..6).map(|i| if i % 2 == 0 { Rgb::new(255, 0, 0) } else { Rgb::new(0, 0, 255) }).collect();
    lights.paint_frame(&frame).unwrap();
    let mode = *q.vial_mode.lock().unwrap();
    assert_eq!(u16::from_le_bytes([mode[0], mode[1]]), VIALRGB_DIRECT);
    let direct = q.direct.lock().unwrap().clone();
    assert_eq!(qmk_hsv_to_rgb([direct[0][0], direct[0][1], 255]), [255, 0, 0]);
    assert_eq!(direct[1][0], 170, "LED 1 is blue");
    assert!(direct.iter().all(|c| c[2] <= 200), "value kept under the board's cap");

    let fastsets = |q: &Qmk| q.requests().iter().filter(|r| r[0] == CMD_CUSTOM_SET_VALUE && r[1] == VIALRGB_DIRECT_FASTSET).count();
    let sent = fastsets(&q);
    lights.paint_frame(&frame).unwrap();
    assert_eq!(fastsets(&q), sent, "an unchanged frame sends nothing");

    d.release_custody().ok();
    assert_eq!(*q.vial_mode.lock().unwrap(), [5, 0, 128, 10, 20, 150], "the mode read at take, exactly");
}

#[test]
fn every_colour_lands_within_two_of_what_was_asked() {
    for r in (0..=255).step_by(17) {
        for g in (0..=255).step_by(51) {
            for b in (0..=255).step_by(85) {
                let c = Rgb::new(r as u8, g as u8, b as u8);
                let shown = qmk_hsv_to_rgb(rgb_to_qmk_hsv(c));
                let off = shown.iter().zip([c.r, c.g, c.b]).map(|(a, b)| a.abs_diff(b)).max().unwrap();
                assert!(off <= 4, "{c:?} shows as {shown:?}");
            }
        }
    }
    assert_eq!(qmk_hsv_to_rgb([0, 255, 255]), [255, 0, 0]);
    assert_eq!(qmk_hsv_to_rgb([255, 255, 255]), [255, 0, 0], "the wheel's period is 255");
}
