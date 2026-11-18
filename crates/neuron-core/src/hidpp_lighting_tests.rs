// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! HID++ colour lighting against a modelled Logitech device.

#![cfg(test)]

use super::*;
use crate::lighting::{Lights, Rgb};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const IDX_ONBOARD: u8 = 0x0B;
const IDX_ZONES: u8 = 0x0C;
const IDX_RGB: u8 = 0x0D;
const IDX_KEYS: u8 = 0x0E;

/// A modelled Logitech device: it answers by feature and function, keeps an onboard mode and each
/// zone's colour, and records every request.
#[derive(Default)]
struct Logi {
    features: Vec<(u16, u8)>,
    mode: Mutex<u8>,
    keys: Vec<u16>,
    zone_effects: Vec<Vec<u16>>,
    monochrome: bool,
    zone_rgb: Mutex<Vec<[u8; 3]>>,
    busy_frame_ends: Mutex<u32>,
    refuse_claim: bool,
    zone_effect: Mutex<Vec<[u8; 11]>>,
    writes: Mutex<Vec<Vec<u8>>>,
    inbox: Mutex<VecDeque<Vec<u8>>>,
}

impl Logi {
    fn requests(&self) -> Vec<Vec<u8>> {
        self.writes.lock().unwrap().clone()
    }

    fn to(&self, feature: u8, fn_id: u8) -> Vec<Vec<u8>> {
        self.requests().into_iter().filter(|r| r[2] == feature && r[3] >> 4 == fn_id).collect()
    }

    fn answer(&self, req: &[u8]) -> Vec<u8> {
        let (feature, fn_id, p) = (req[2], req[3] >> 4, &req[4..]);
        let mut r = req[..4].to_vec();
        r.resize(LONG_LEN, 0);
        match (feature, fn_id) {
            (IROOT_FEATURE, IROOT_FN_GET_FEATURE) => {
                let id = be16(p[0], p[1]);
                r[4] = self.features.iter().find(|f| f.0 == id).map_or(0, |f| f.1);
            }
            (IDX_ONBOARD, FN_8100_GET_MODE) => r[4] = *self.mode.lock().unwrap(),
            (IDX_ONBOARD, FN_8100_SET_MODE) => *self.mode.lock().unwrap() = p[0],
            (IDX_KEYS, FN_8081_GET_INFO) => {
                let page = u16::from(p[1]);
                for &id in &self.keys {
                    if id / 112 == page {
                        let bit = usize::from(id % 112);
                        r[6 + bit / 8] |= 1 << (bit % 8);
                    }
                }
            }
            (IDX_RGB, FN_8071_SW_CONTROL) if self.refuse_claim && p[1] == 0x03 => {
                let mut e = vec![REPORT_ID_LONG, req[1], ERR_HIDPP20, feature, req[3], 0x09];
                e.resize(LONG_LEN, 0);
                return e;
            }
            (IDX_KEYS, FN_8081_FRAME_END) => {
                let mut busy = self.busy_frame_ends.lock().unwrap();
                if *busy > 0 {
                    *busy -= 1;
                    let mut e = vec![REPORT_ID_LONG, req[1], ERR_HIDPP20, feature, req[3], BUSY];
                    e.resize(LONG_LEN, 0);
                    return e;
                }
            }
            (IDX_ZONES, FN_8070_GET_INFO) => {
                r[4] = self.zone_effects.len() as u8;
                r[8] = if self.monochrome { EXT_MONOCHROME_ONLY as u8 } else { 0 } | EXT_HAS_ZONE_EFFECT as u8;
            }
            (IDX_ZONES, FN_8070_GET_ZONE_INFO) => r[7] = self.zone_effects[usize::from(p[0])].len() as u8,
            (IDX_ZONES, FN_8070_GET_EFFECT_INFO) => {
                let id = self.zone_effects[usize::from(p[0])][usize::from(p[1])];
                r[6..8].copy_from_slice(&id.to_be_bytes());
            }
            (IDX_ZONES, FN_8070_SET_ZONE_EFFECT) => {
                let zone = usize::from(p[0]);
                self.zone_rgb.lock().unwrap()[zone] = [p[2], p[3], p[4]];
                self.zone_effect.lock().unwrap()[zone].copy_from_slice(&p[1..12]);
            }
            (IDX_ZONES, FN_8070_GET_ZONE_EFFECT) => {
                let zone = usize::from(p[0]);
                r[4] = p[0];
                r[5..16].copy_from_slice(&self.zone_effect.lock().unwrap()[zone]);
            }
            _ => {}
        }
        r
    }
}

struct Wire(Arc<Logi>);

impl Transport for Wire {
    fn set_feature(&self, _: &[u8]) -> Result<()> {
        bail!("no feature reports")
    }

    fn get_feature(&self, _: &mut [u8]) -> Result<usize> {
        bail!("no feature reports")
    }

    fn write_output(&self, buf: &[u8]) -> Result<()> {
        self.0.writes.lock().unwrap().push(buf.to_vec());
        let reply = self.0.answer(buf);
        self.0.inbox.lock().unwrap().push_back(reply);
        Ok(())
    }

    fn read_input(&self, buf: &mut [u8], _: u32) -> Result<usize> {
        let frame = self.0.inbox.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("silent"))?;
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }
}

fn keyboard() -> Arc<Logi> {
    Arc::new(Logi {
        features: vec![
            (FEATURE_ONBOARD_PROFILES, IDX_ONBOARD),
            (FEATURE_RGB_EFFECTS, IDX_RGB),
            (FEATURE_PER_KEY_LIGHTING_V2, IDX_KEYS),
        ],
        mode: Mutex::new(ONBOARD_MODE),
        // A (usage 0x04), Esc (0x29), G1, the logo, a key the grid has no cell for, Left Shift
        keys: vec![1, 0x29 - 3, KEY_ID_G1, KEY_ID_LOGO, 150, 105],
        ..Logi::default()
    })
}

fn mouse_zones(monochrome: bool) -> Arc<Logi> {
    Arc::new(Logi {
        features: vec![(FEATURE_COLOR_LED_EFFECTS, IDX_ZONES)],
        zone_effects: vec![vec![0x0000, EFFECT_FIXED, 0x0002, 0x0003], vec![0x0000, 0x0003, EFFECT_FIXED]],
        monochrome,
        zone_rgb: Mutex::new(vec![[0; 3]; 2]),
        // zone 0 cycles: index 3, id 3, so the byte means the same either way; zone 1 cycles at
        // index 1, id 3, which a read-back cannot name for sure
        zone_effect: Mutex::new(vec![[3, 0, 0, 0, 0, 0, 0x0B, 0xB8, 64, 0, 0], [1, 0, 0, 0, 0, 0, 0x0B, 0xB8, 64, 0, 0]]),
        ..Logi::default()
    })
}

fn def(lighting: Option<crate::lighting::LightingDef>) -> crate::registry::DeviceDef {
    let mut d: crate::registry::DeviceDef = toml::from_str(
        "name = \"Logi\"\ncodename = \"l\"\ndialect = \"hidpp\"\nvendor_id = 1133\ntransaction_id = 0\n\
         [[modes]]\nname = \"default\"\nproduct_id = 50000\n\
         [control_interface]\nusage_page = 65280\nusage = 2\nfeature_report_len = 0\n\
         [commands]\n",
    )
    .unwrap();
    d.lighting = lighting;
    d
}

fn device(logi: &Arc<Logi>, unit: &str) -> crate::device::Device {
    let lighting = probe_lighting(&Wire(logi.clone()), DEVICE_INDEX);
    let mut d = crate::device::Device::with_transport(def(lighting), 50000, Box::new(Wire(logi.clone())));
    d.dpi_unit = unit.into();
    d
}

#[test]
fn keys_land_on_their_keyboard_cells_and_an_unknown_key_shows_the_mean() {
    let l = probe_lighting(&Wire(keyboard()), DEVICE_INDEX).expect("per-key lighting");
    assert_eq!((l.protocol, l.rows, l.cols), (crate::lighting::Protocol::Hidpp, 6, 22));
    let cell = |name| crate::lighting::razer_key_cell(name).map(|(r, c)| [u16::from(r), u16::from(c)]).unwrap();
    let at = |id: u16| l.leds.iter().find(|x| x[0] == id).map(|x| [x[1], x[2]]).unwrap();
    assert_eq!(at(1), cell("A"));
    assert_eq!(at(0x29 - 3), cell("ESC"));
    assert_eq!(at(KEY_ID_G1), cell("M1"));
    assert_eq!(at(KEY_ID_LOGO), cell("LOGO"));
    assert_eq!(at(150)[0], NO_CELL);
    assert_eq!(at(105), cell("LSHIFT"), "modifiers have their own id range");
    assert_eq!(crate::defcheck::check(&def(Some(l))), Ok(()), "what adoption learns loads");
}

#[test]
fn a_frame_goes_out_once_and_only_its_changed_keys_follow() {
    let logi = keyboard();
    let d = device(&logi, "unit-keys");
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    let mut frame = vec![Rgb::new(10, 20, 30); 6 * 22];
    lights.paint_frame(&frame).unwrap();
    assert_eq!(*logi.mode.lock().unwrap(), HOST_MODE, "onboard mode gives way to the host");
    assert_eq!(logi.to(IDX_RGB, FN_8071_SW_CONTROL)[0][4..7], SW_CONTROL_TAKE);
    let sets = logi.to(IDX_KEYS, FN_8081_SET_INDIVIDUAL);
    assert_eq!(sets.len(), 4, "six keys, four to a report, and the first frame sent twice");
    assert_eq!(&sets[0][4..8], &[1, 10, 20, 30]);
    assert_eq!(&sets[1][12..16], &sets[1][8..12], "a short report repeats its last key, never key 0");
    assert_eq!(logi.to(IDX_KEYS, FN_8081_FRAME_END).len(), 2);

    lights.paint_frame(&frame).unwrap();
    assert_eq!(logi.to(IDX_KEYS, FN_8081_SET_INDIVIDUAL).len(), 4, "an unchanged frame sends nothing");

    let (r, c) = crate::lighting::razer_key_cell("A").unwrap();
    frame[usize::from(r) * 22 + usize::from(c)] = Rgb::new(255, 0, 0);
    lights.paint_frame(&frame).unwrap();
    let sets = logi.to(IDX_KEYS, FN_8081_SET_INDIVIDUAL);
    assert_eq!(sets.len(), 5, "A and the unknown key, whose mean moved with it, share one report");
    assert_eq!(&sets[4][4..8], &[1, 255, 0, 0]);

    d.release_custody().ok();
    assert_eq!(logi.to(IDX_RGB, FN_8071_SW_CONTROL).last().unwrap()[4..7], SW_CONTROL_GIVE);
    assert_eq!(*logi.mode.lock().unwrap(), ONBOARD_MODE, "the mode read at take is put back");
    let allowed = [(IDX_RGB, FN_8071_SW_CONTROL), (IDX_ONBOARD, FN_8100_GET_MODE), (IDX_ONBOARD, FN_8100_SET_MODE)];
    for r in logi.requests() {
        let (f, fun) = (r[2], r[3] >> 4);
        assert!(f == IROOT_FEATURE || f == IDX_KEYS || allowed.contains(&(f, fun)), "unexpected request {r:02x?}");
    }
}

#[test]
fn a_busy_frame_end_is_waited_out() {
    let logi = keyboard();
    *logi.busy_frame_ends.lock().unwrap() = 3;
    let d = device(&logi, "unit-busy");
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    lights.paint_frame(&vec![Rgb::new(1, 2, 3); 6 * 22]).unwrap();
    assert_eq!(logi.to(IDX_KEYS, FN_8081_FRAME_END).len(), 5, "three busy, the frame, its replay");
    d.release_custody().ok();
}

#[test]
fn a_refused_claim_puts_the_onboard_mode_back() {
    let logi = Arc::new(Logi { refuse_claim: true, ..Arc::try_unwrap(keyboard()).ok().unwrap() });
    let d = device(&logi, "unit-refused");
    let lights = Lights::new(&d, d.def.lighting.clone().unwrap());
    assert!(lights.paint_frame(&vec![Rgb::new(1, 2, 3); 6 * 22]).is_err());
    assert_eq!(*logi.mode.lock().unwrap(), ONBOARD_MODE, "nothing is left changed");
    assert!(logi.to(IDX_KEYS, FN_8081_SET_INDIVIDUAL).is_empty());
}

#[test]
fn zones_take_their_fixed_effect_index_in_ram_and_read_back() {
    let logi = mouse_zones(false);
    let d = device(&logi, "unit-zones");
    let l = d.def.lighting.clone().expect("zone lighting");
    assert_eq!((l.rows, l.cols), (1, 2));
    let lights = Lights::new(&d, l);
    lights.paint_frame(&[Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]).unwrap();
    let sets = logi.to(IDX_ZONES, FN_8070_SET_ZONE_EFFECT);
    assert_eq!(sets.len(), 2);
    assert_eq!(&sets[0][4..10], &[0, 1, 255, 0, 0, FIXED_NO_RAMP], "zone 0's fixed effect is index 1");
    assert_eq!(&sets[1][4..10], &[1, 2, 0, 0, 255, FIXED_NO_RAMP], "zone 1's is index 2");
    assert!(sets.iter().all(|s| s[16] == 0), "the persist byte keeps it out of flash");
    assert_eq!(logi.to(IDX_ZONES, FN_8070_GET_ZONE_EFFECT).len(), 3, "each zone read at take, then the first colour read back");
    assert_eq!(logi.to(IDX_ZONES, FN_8070_SET_SW_CONTROL)[0][4..6], [1, 0]);
    d.release_custody().ok();
    assert_eq!(logi.to(IDX_ZONES, FN_8070_SET_SW_CONTROL).last().unwrap()[4], 0);
    assert_eq!(logi.zone_effect.lock().unwrap()[0], [3, 0, 0, 0, 0, 0, 0x0B, 0xB8, 64, 0, 0], "zone 0 cycles again");
    assert_eq!(logi.zone_effect.lock().unwrap()[1][0], 2, "zone 1 is not guessed at");
}

#[test]
fn a_monochrome_device_offers_no_colour() {
    assert!(probe_lighting(&Wire(mouse_zones(true)), DEVICE_INDEX).is_none());
}
