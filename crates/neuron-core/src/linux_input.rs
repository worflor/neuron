// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Linux evdev/uinput boundary. The live listener owns physical devices; actions only write to
//! this module's virtual device after the process-wide input arm gate has been opened.

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode};
use std::sync::{mpsc, Mutex, OnceLock};
use std::sync::atomic::{AtomicI32, Ordering};
use crate::registry::CanonicalPid;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// HID keyboard-page usages and Linux input-event codes. The table is deliberately bidirectional:
// a key captured on Linux gets the same portable HID trigger a Windows Raw-Input capture gets.
const KEYS: &[(u16, KeyCode)] = &[
    (0x04, KeyCode::KEY_A), (0x05, KeyCode::KEY_B), (0x06, KeyCode::KEY_C),
    (0x07, KeyCode::KEY_D), (0x08, KeyCode::KEY_E), (0x09, KeyCode::KEY_F),
    (0x0A, KeyCode::KEY_G), (0x0B, KeyCode::KEY_H), (0x0C, KeyCode::KEY_I),
    (0x0D, KeyCode::KEY_J), (0x0E, KeyCode::KEY_K), (0x0F, KeyCode::KEY_L),
    (0x10, KeyCode::KEY_M), (0x11, KeyCode::KEY_N), (0x12, KeyCode::KEY_O),
    (0x13, KeyCode::KEY_P), (0x14, KeyCode::KEY_Q), (0x15, KeyCode::KEY_R),
    (0x16, KeyCode::KEY_S), (0x17, KeyCode::KEY_T), (0x18, KeyCode::KEY_U),
    (0x19, KeyCode::KEY_V), (0x1A, KeyCode::KEY_W), (0x1B, KeyCode::KEY_X),
    (0x1C, KeyCode::KEY_Y), (0x1D, KeyCode::KEY_Z),
    (0x1E, KeyCode::KEY_1), (0x1F, KeyCode::KEY_2), (0x20, KeyCode::KEY_3),
    (0x21, KeyCode::KEY_4), (0x22, KeyCode::KEY_5), (0x23, KeyCode::KEY_6),
    (0x24, KeyCode::KEY_7), (0x25, KeyCode::KEY_8), (0x26, KeyCode::KEY_9),
    (0x27, KeyCode::KEY_0), (0x28, KeyCode::KEY_ENTER), (0x29, KeyCode::KEY_ESC),
    (0x2A, KeyCode::KEY_BACKSPACE), (0x2B, KeyCode::KEY_TAB),
    (0x2C, KeyCode::KEY_SPACE), (0x2D, KeyCode::KEY_MINUS),
    (0x2E, KeyCode::KEY_EQUAL), (0x2F, KeyCode::KEY_LEFTBRACE),
    (0x30, KeyCode::KEY_RIGHTBRACE), (0x31, KeyCode::KEY_BACKSLASH),
    (0x32, KeyCode::KEY_102ND), (0x33, KeyCode::KEY_SEMICOLON),
    (0x34, KeyCode::KEY_APOSTROPHE), (0x35, KeyCode::KEY_GRAVE),
    (0x36, KeyCode::KEY_COMMA), (0x37, KeyCode::KEY_DOT),
    (0x38, KeyCode::KEY_SLASH), (0x39, KeyCode::KEY_CAPSLOCK),
    (0x3A, KeyCode::KEY_F1), (0x3B, KeyCode::KEY_F2), (0x3C, KeyCode::KEY_F3),
    (0x3D, KeyCode::KEY_F4), (0x3E, KeyCode::KEY_F5), (0x3F, KeyCode::KEY_F6),
    (0x40, KeyCode::KEY_F7), (0x41, KeyCode::KEY_F8), (0x42, KeyCode::KEY_F9),
    (0x43, KeyCode::KEY_F10), (0x44, KeyCode::KEY_F11), (0x45, KeyCode::KEY_F12),
    (0x46, KeyCode::KEY_SYSRQ), (0x47, KeyCode::KEY_SCROLLLOCK),
    (0x48, KeyCode::KEY_PAUSE), (0x49, KeyCode::KEY_INSERT),
    (0x4A, KeyCode::KEY_HOME), (0x4B, KeyCode::KEY_PAGEUP),
    (0x4C, KeyCode::KEY_DELETE), (0x4D, KeyCode::KEY_END),
    (0x4E, KeyCode::KEY_PAGEDOWN), (0x4F, KeyCode::KEY_RIGHT),
    (0x50, KeyCode::KEY_LEFT), (0x51, KeyCode::KEY_DOWN),
    (0x52, KeyCode::KEY_UP), (0x53, KeyCode::KEY_NUMLOCK),
    (0x54, KeyCode::KEY_KPSLASH), (0x55, KeyCode::KEY_KPASTERISK),
    (0x56, KeyCode::KEY_KPMINUS), (0x57, KeyCode::KEY_KPPLUS),
    (0x58, KeyCode::KEY_KPENTER), (0x59, KeyCode::KEY_KP1),
    (0x5A, KeyCode::KEY_KP2), (0x5B, KeyCode::KEY_KP3),
    (0x5C, KeyCode::KEY_KP4), (0x5D, KeyCode::KEY_KP5),
    (0x5E, KeyCode::KEY_KP6), (0x5F, KeyCode::KEY_KP7),
    (0x60, KeyCode::KEY_KP8), (0x61, KeyCode::KEY_KP9),
    (0x62, KeyCode::KEY_KP0), (0x63, KeyCode::KEY_KPDOT),
    (0x65, KeyCode::KEY_COMPOSE),
    (0x68, KeyCode::KEY_F13), (0x69, KeyCode::KEY_F14),
    (0x6A, KeyCode::KEY_F15), (0x6B, KeyCode::KEY_F16),
    (0x6C, KeyCode::KEY_F17), (0x6D, KeyCode::KEY_F18),
    (0x6E, KeyCode::KEY_F19), (0x6F, KeyCode::KEY_F20),
    (0x70, KeyCode::KEY_F21), (0x71, KeyCode::KEY_F22),
    (0x72, KeyCode::KEY_F23), (0x73, KeyCode::KEY_F24),
    (0xE0, KeyCode::KEY_LEFTCTRL), (0xE1, KeyCode::KEY_LEFTSHIFT),
    (0xE2, KeyCode::KEY_LEFTALT), (0xE3, KeyCode::KEY_LEFTMETA),
    (0xE4, KeyCode::KEY_RIGHTCTRL), (0xE5, KeyCode::KEY_RIGHTSHIFT),
    (0xE6, KeyCode::KEY_RIGHTALT), (0xE7, KeyCode::KEY_RIGHTMETA),
];

pub fn keycode_for_usage(usage: u16) -> Option<u16> {
    KEYS.iter().find(|(u, _)| *u == usage).map(|(_, k)| k.0)
}

pub fn keycode_for_name(name: &str) -> Option<u16> {
    if let Some(usage) = crate::action::hid_usage_for_key(name) {
        return keycode_for_usage(u16::from(usage));
    }
    let name = name.trim().to_ascii_lowercase();
    if let Some(digit) = name.strip_prefix("num").and_then(|n| n.parse::<u16>().ok()) {
        if digit <= 9 {
            return keycode_for_usage(if digit == 0 { 0x62 } else { 0x58 + digit });
        }
    }
    Some(match name.as_str() {
        "num*" => KeyCode::KEY_KPASTERISK.0,
        "num+" => KeyCode::KEY_KPPLUS.0,
        "num-" => KeyCode::KEY_KPMINUS.0,
        "num." => KeyCode::KEY_KPDOT.0,
        "num/" => KeyCode::KEY_KPSLASH.0,
        "media-play-pause" | "media-play" | "play-pause" => KeyCode::KEY_PLAYPAUSE.0,
        "media-stop" => KeyCode::KEY_STOPCD.0,
        "media-next" => KeyCode::KEY_NEXTSONG.0,
        "media-prev" | "media-previous" => KeyCode::KEY_PREVIOUSSONG.0,
        "volume-up" => KeyCode::KEY_VOLUMEUP.0,
        "volume-down" => KeyCode::KEY_VOLUMEDOWN.0,
        "volume-mute" => KeyCode::KEY_MUTE.0,
        "browser-back" => KeyCode::KEY_BACK.0,
        "browser-forward" => KeyCode::KEY_FORWARD.0,
        "browser-refresh" => KeyCode::KEY_REFRESH.0,
        _ => return None,
    })
}

pub fn keycode_for_vk(vk: u16) -> Option<u16> {
    keycode_for_name(&crate::action::key_param_for_vk(vk))
}

pub fn hit_for_keycode(code: u16) -> (u16, u16) {
    if let Some((usage, _)) = KEYS.iter().find(|(_, k)| k.0 == code) {
        return (0x07, *usage);
    }
    match code {
        0x110..=0x114 => (0x09, code - 0x10F),
        c if c == KeyCode::KEY_VOLUMEUP.0 => (0x0C, 0xE9),
        c if c == KeyCode::KEY_VOLUMEDOWN.0 => (0x0C, 0xEA),
        c if c == KeyCode::KEY_MUTE.0 => (0x0C, 0xE2),
        c if c == KeyCode::KEY_PLAYPAUSE.0 => (0x0C, 0xCD),
        c if c == KeyCode::KEY_NEXTSONG.0 => (0x0C, 0xB5),
        c if c == KeyCode::KEY_PREVIOUSSONG.0 => (0x0C, 0xB6),
        c if c == KeyCode::KEY_STOPCD.0 => (0x0C, 0xB7),
        _ => (0xFF07, code),
    }
}

fn output_device() -> std::io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for &(_, code) in KEYS {
        keys.insert(code);
    }
    for code in [
        KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE,
        KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA, KeyCode::KEY_VOLUMEUP,
        KeyCode::KEY_VOLUMEDOWN, KeyCode::KEY_MUTE,
        KeyCode::KEY_PLAYPAUSE, KeyCode::KEY_NEXTSONG,
        KeyCode::KEY_PREVIOUSSONG, KeyCode::KEY_STOPCD,
        KeyCode::KEY_BACK, KeyCode::KEY_FORWARD, KeyCode::KEY_REFRESH,
    ] {
        keys.insert(code);
    }
    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    rel.insert(RelativeAxisCode::REL_X);
    rel.insert(RelativeAxisCode::REL_Y);
    rel.insert(RelativeAxisCode::REL_WHEEL);
    rel.insert(RelativeAxisCode::REL_HWHEEL);
    VirtualDevice::builder()?
        .name("neuron virtual input")
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()
}

static OUTPUT: OnceLock<Mutex<Option<VirtualDevice>>> = OnceLock::new();
static GRABBED: OnceLock<Mutex<HashMap<CanonicalPid, usize>>> = OnceLock::new();
static DOWN: OnceLock<Mutex<HashMap<PathBuf, HashSet<u16>>>> = OnceLock::new();
static MOTION: OnceLock<Mutex<Vec<mpsc::Sender<Motion>>>> = OnceLock::new();
static WHEEL: AtomicI32 = AtomicI32::new(0);
static LEFT_DOWN: AtomicI32 = AtomicI32::new(0);
static RIGHT_DOWN: AtomicI32 = AtomicI32::new(0);
static RIGHT_UP: AtomicI32 = AtomicI32::new(0);

#[derive(Clone, Copy)]
pub struct Motion { pub dx: i32, pub dy: i32, pub at_ms: u32 }

fn motion_stamp() -> u32 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u32
}

pub fn observe_motion() -> mpsc::Receiver<Motion> {
    let (tx, rx) = mpsc::channel();
    MOTION.get_or_init(|| Mutex::new(Vec::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(tx);
    rx
}

pub fn note_relative(code: u16, value: i32) {
    let at_ms = motion_stamp();
    let motion = match code {
        c if c == RelativeAxisCode::REL_X.0 => Some(Motion { dx: value, dy: 0, at_ms }),
        c if c == RelativeAxisCode::REL_Y.0 => Some(Motion { dx: 0, dy: value, at_ms }),
        c if c == RelativeAxisCode::REL_WHEEL.0 => {
            WHEEL.fetch_add(value, Ordering::Relaxed);
            None
        }
        _ => None,
    };
    if let Some(motion) = motion {
        MOTION.get_or_init(|| Mutex::new(Vec::new()))
            .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|tx| tx.send(motion).is_ok());
    }
}

pub fn note_mouse_button(code: u16, down: bool) {
    match (code, down) {
        (0x110, true) => { LEFT_DOWN.fetch_add(1, Ordering::Relaxed); }
        (0x111, true) => { RIGHT_DOWN.fetch_add(1, Ordering::Relaxed); }
        (0x111, false) => { RIGHT_UP.fetch_add(1, Ordering::Relaxed); }
        _ => {}
    }
}

pub fn take_wheel_ticks() -> i32 { WHEEL.swap(0, Ordering::Relaxed) }
pub fn take_click_edges() -> (i32, i32, i32) {
    (LEFT_DOWN.swap(0, Ordering::Relaxed), RIGHT_DOWN.swap(0, Ordering::Relaxed), RIGHT_UP.swap(0, Ordering::Relaxed))
}

pub fn note_physical_key(path: &Path, code: u16, down: bool) {
    let mut all = DOWN.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let keys = all.entry(path.to_path_buf()).or_default();
    if down { keys.insert(code); } else { keys.remove(&code); }
}

pub fn forget_physical_device(path: &Path) {
    DOWN.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(path);
}

pub fn physical_key_down(vk: i32) -> bool {
    let code = match vk {
        0x01 => Some(KeyCode::BTN_LEFT.0),
        0x02 => Some(KeyCode::BTN_RIGHT.0),
        0x04 => Some(KeyCode::BTN_MIDDLE.0),
        0x05 => Some(KeyCode::BTN_SIDE.0),
        0x06 => Some(KeyCode::BTN_EXTRA.0),
        vk if (0..256).contains(&vk) => keycode_for_vk(vk as u16),
        _ => None,
    };
    code.is_some_and(|code| DOWN.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .values().any(|keys| keys.contains(&code)))
}

pub fn note_grab(pid: CanonicalPid, grabbed: bool) {
    let mut all = GRABBED.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if grabbed {
        *all.entry(pid).or_default() += 1;
    } else if let Some(count) = all.get_mut(&pid) {
        *count = count.saturating_sub(1);
        if *count == 0 { all.remove(&pid); }
    }
}

pub fn grabbed(pid: CanonicalPid) -> bool {
    GRABBED.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(&pid)
}

pub fn output_ready() -> bool {
    let output = OUTPUT.get_or_init(|| Mutex::new(None));
    let mut guard = output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if guard.is_none() {
        *guard = output_device().ok();
    }
    guard.is_some()
}

pub fn emit(event: InputEvent) -> bool {
    if !crate::action::input_armed() || !output_ready() {
        return false;
    }
    let output = OUTPUT.get().expect("output initialized");
    let mut guard = output.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(device) = guard.as_mut() else { return false };
    if device.emit(&[event]).is_err() {
        *guard = None;
        return false;
    }
    true
}

pub fn emit_key(code: u16, down: bool) -> bool {
    emit(InputEvent::new(EventType::KEY.0, code, i32::from(down)))
}

pub fn emit_relative(code: RelativeAxisCode, value: i32) -> bool {
    emit(InputEvent::new(EventType::RELATIVE.0, code.0, value))
}

pub fn virtual_for_device(device: &Device) -> std::io::Result<VirtualDevice> {
    let mut builder = VirtualDevice::builder()?.name("neuron replay input");
    if let Some(original) = device.supported_keys() {
        let mut keys = AttributeSet::<KeyCode>::new();
        for key in original.iter() { keys.insert(key); }
        for &(_, key) in KEYS { keys.insert(key); }
        builder = builder.with_keys(&keys)?;
    }
    if let Some(axes) = device.supported_relative_axes() {
        builder = builder.with_relative_axes(axes)?;
    }
    if let Ok(axes) = device.get_absinfo() {
        for (code, info) in axes {
            builder = builder.with_absolute_axis(&evdev::UinputAbsSetup::new(code, info))?;
        }
    }
    if let Some(switches) = device.supported_switches() {
        builder = builder.with_switches(switches)?;
    }
    if let Some(misc) = device.misc_properties() {
        builder = builder.with_msc(misc)?;
    }
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_map_round_trips_without_arming_input() {
        for &(usage, code) in KEYS {
            assert_eq!(keycode_for_usage(usage), Some(code.0));
            assert_eq!(hit_for_keycode(code.0), (0x07, usage));
        }
    }

    #[test]
    fn mouse_and_consumer_pages_are_distinct() {
        assert_eq!(hit_for_keycode(KeyCode::BTN_SIDE.0), (0x09, 4));
        assert_eq!(hit_for_keycode(KeyCode::KEY_VOLUMEUP.0), (0x0C, 0xE9));
    }
}
