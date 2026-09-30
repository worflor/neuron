// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The standard gamepad: the one layout every platform already agrees on (the W3C Gamepad
//! "standard" mapping; Windows GameInput's gamepad reading, macOS `GCExtendedGamepad` and Linux
//! evdev's `BTN_SOUTH`… all describe it). A platform backend only fills in a [`StandardPad`];
//! everything above it — controls, labels, sticks, the radial — is shared, so a bind on "A" means
//! the same button on any pad and any OS.
//!
//! Buttons become controls on [`PAD_PAGE`] numbered by the W3C standard index. The sticks and
//! triggers become value fields on the synthetic [`PAD_FIELD_PAGE`] and go through
//! [`crate::analog`] like any HID value, so they learn their rest, yield directions and pair into
//! sticks the same way; the d-pad becomes the analog hat.

use crate::analog::Field;

/// Controls for standard-pad buttons; usage = the W3C standard button index.
pub const PAD_PAGE: u16 = 0xFE20;
/// Synthetic field page for standard-pad sticks and triggers (usages mirror Generic Desktop's:
/// X/Y left stick, Rx/Ry right stick, Z/Rz left/right trigger).
pub const PAD_FIELD_PAGE: u16 = 0x00FE;

/// W3C standard-mapping button indices (triggers and the d-pad travel as analog fields/hat).
pub mod button {
    pub const SOUTH: u16 = 0; // A (Xbox), B (Nintendo), Cross
    pub const EAST: u16 = 1; // B, A, Circle
    pub const WEST: u16 = 2; // X, Y, Square
    pub const NORTH: u16 = 3; // Y, X, Triangle
    pub const LEFT_SHOULDER: u16 = 4;
    pub const RIGHT_SHOULDER: u16 = 5;
    pub const BACK: u16 = 8; // View, Select, Minus
    pub const START: u16 = 9; // Menu, Plus
    pub const LEFT_STICK: u16 = 10;
    pub const RIGHT_STICK: u16 = 11;
    pub const DPAD_UP: u16 = 12;
    pub const DPAD_DOWN: u16 = 13;
    pub const DPAD_LEFT: u16 = 14;
    pub const DPAD_RIGHT: u16 = 15;
    pub const HOME: u16 = 16; // Guide, Home, PS
    pub const CAPTURE: u16 = 17; // Share, Capture
}

/// One gamepad's state. Sticks are -1..1 (`y` positive = down), triggers 0..1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StandardPad {
    /// Bit `i` = W3C button `i` held (d-pad bits included).
    pub buttons: u32,
    pub left_trigger: f32,
    pub right_trigger: f32,
    pub left_x: f32,
    pub left_y: f32,
    pub right_x: f32,
    pub right_y: f32,
}

/// The value range a standard-pad field is expressed in (the analog model learns rest from it).
const AXIS_SPAN: i32 = 32767;

const fn field(usage: u16, min: i32) -> Field {
    Field { page: PAD_FIELD_PAGE, usage, logical_min: min, logical_max: AXIS_SPAN }
}
const LX: Field = field(0x30, -AXIS_SPAN);
const LY: Field = field(0x31, -AXIS_SPAN);
const LT: Field = field(0x32, 0);
const RX: Field = field(0x33, -AXIS_SPAN);
const RY: Field = field(0x34, -AXIS_SPAN);
const RT: Field = field(0x35, 0);
/// The d-pad as a Generic Desktop hat (logical 1..8, 0 = centred), so it reads like any HID hat.
const HAT: Field = Field { page: 0x01, usage: 0x39, logical_min: 1, logical_max: 8 };

impl StandardPad {
    fn held(&self, b: u16) -> bool {
        self.buttons & (1 << b) != 0
    }

    /// The button controls currently down (d-pad excluded: it travels as the hat).
    #[must_use]
    pub fn button_hits(&self) -> Vec<(u16, u16)> {
        (0..32u16)
            .filter(|&b| self.held(b) && !(button::DPAD_UP..=button::DPAD_RIGHT).contains(&b))
            .map(|b| (PAD_PAGE, b))
            .collect()
    }

    /// The analog fields, for [`crate::analog::Device::observe`].
    #[must_use]
    pub fn values(&self) -> Vec<(Field, i32)> {
        let v = |x: f32| (x.clamp(-1.0, 1.0) * AXIS_SPAN as f32).round() as i32;
        vec![
            (LX, v(self.left_x)),
            (LY, v(self.left_y)),
            (LT, v(self.left_trigger)),
            (RX, v(self.right_x)),
            (RY, v(self.right_y)),
            (RT, v(self.right_trigger)),
            (HAT, self.hat()),
        ]
    }

    /// The d-pad as a hat value: 1 = up, clockwise to 8 = up-left, 0 = centred.
    fn hat(&self) -> i32 {
        let (u, d, l, r) = (
            self.held(button::DPAD_UP),
            self.held(button::DPAD_DOWN),
            self.held(button::DPAD_LEFT),
            self.held(button::DPAD_RIGHT),
        );
        match (u && !d, d && !u, l && !r, r && !l) {
            (true, _, false, false) => 1,
            (true, _, false, true) => 2,
            (_, _, false, true) if !u && !d => 3,
            (_, true, false, true) => 4,
            (_, true, false, false) => 5,
            (_, true, true, false) => 6,
            (_, _, true, false) if !u && !d => 7,
            (true, _, true, false) => 8,
            _ => 0,
        }
    }
}

/// Start this platform's standard-pad sources (idempotent). Windows: GameInput, for pads whose
/// HID reaches no background process. A new platform adds its native gamepad API here (Linux
/// evdev, macOS GameController) and everything above the [`StandardPad`] is already shared.
pub fn start_platform_sources() {
    static STARTED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        #[cfg(windows)]
        {
            crate::hid_haptics::register();
            crate::gameinput::start();
        }
    });
}

/// A human name for a standard-pad control.
#[must_use]
pub fn control_label(page: u16, usage: u16) -> Option<String> {
    use button::*;
    if page != PAD_PAGE {
        return None;
    }
    Some(
        match usage {
            SOUTH => "Pad south",
            EAST => "Pad east",
            WEST => "Pad west",
            NORTH => "Pad north",
            LEFT_SHOULDER => "Pad L shoulder",
            RIGHT_SHOULDER => "Pad R shoulder",
            BACK => "Pad back",
            START => "Pad start",
            LEFT_STICK => "Pad L stick press",
            RIGHT_STICK => "Pad R stick press",
            HOME => "Pad home",
            CAPTURE => "Pad capture",
            _ => return Some(format!("Pad button {usage}")),
        }
        .to_string(),
    )
}

/// Names for the standard-pad value fields, used by [`crate::analog::control_label`]. Positional,
/// like the buttons: the same name on every family's pad.
#[must_use]
pub fn field_name(usage: u16) -> Option<&'static str> {
    Some(match usage {
        0x30 | 0x31 => "L stick",
        0x33 | 0x34 => "R stick",
        0x32 => "L trigger",
        0x35 => "R trigger",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analog::{Device, AXIS_NEG_PAGE, AXIS_POS_PAGE, HAT_PAGE};

    fn run(d: &mut Device, pad: &StandardPad, from: u64, to: u64) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            out = d.observe(&pad.values(), t);
            t += 8;
        }
        out
    }

    #[test]
    fn a_resting_pad_is_two_sticks_and_two_triggers_with_nothing_pressed() {
        let mut d = Device::default();
        assert!(run(&mut d, &StandardPad::default(), 0, 1_000).is_empty());
        assert_eq!(d.sticks().len(), 2, "left and right sticks pair up");
    }

    #[test]
    fn sticks_triggers_and_the_dpad_become_controls() {
        let mut d = Device::default();
        run(&mut d, &StandardPad::default(), 0, 1_000);
        let pad = StandardPad {
            buttons: (1 << button::SOUTH) | (1 << button::DPAD_UP) | (1 << button::DPAD_RIGHT),
            right_trigger: 0.9,
            left_x: -1.0,
            ..StandardPad::default()
        };
        let mut hits = d.observe(&pad.values(), 1_008);
        hits.extend(pad.button_hits());
        hits.sort_unstable();
        let mut want = vec![
            (AXIS_NEG_PAGE, 0xFE30), // left stick pushed left
            (AXIS_POS_PAGE, 0xFE35), // right trigger pulled
            (HAT_PAGE, 0),           // d-pad up …
            (HAT_PAGE, 1),           // … and right
            (PAD_PAGE, button::SOUTH),
        ];
        want.sort_unstable();
        assert_eq!(hits, want);
    }

    #[test]
    fn labels_name_the_pad() {
        assert_eq!(control_label(PAD_PAGE, button::SOUTH).as_deref(), Some("Pad south"));
        assert_eq!(crate::analog::control_label(AXIS_NEG_PAGE, 0xFE30).as_deref(), Some("L stick ←"));
        assert_eq!(crate::analog::control_label(AXIS_POS_PAGE, 0xFE35).as_deref(), Some("R trigger"));
        assert_eq!(crate::analog::control_label(AXIS_NEG_PAGE, 0xFE34).as_deref(), Some("R stick ↑"));
    }
}
