// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Windows GameInput as a standard-pad source.
//!
//! Xbox-protocol pads (GIP, e.g. PowerA 24c6:543a) are owned by Windows' `xboxgip` driver and
//! reach a background process through neither Raw Input nor XInput reliably (observed
//! 2026-09-30); GameInput's gamepad reading delivers them in the background under its default
//! focus policy. Each connected gamepad is polled, turned into a [`StandardPad`], and fed through
//! the same analog model and control stream as every other device.
//!
//! The vtable slots and struct offsets below are from the Windows SDK 10.0.26100 `GameInput.h`.
//! `GameInput.dll` is loaded on demand, so a system without it simply has no GameInput pads.

use crate::pad::{button, StandardPad};
use std::ffi::c_void;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const KIND_GAMEPAD: u32 = 0x0004_0000;
const KIND_MOTION: u32 = 0x0000_1000;
const STATUS_CONNECTED: u32 = 0x01;
const ASYNC_ENUMERATION: u32 = 1;

// IGameInput slots (IUnknown takes 0..=2).
const GI_GET_CURRENT_READING: usize = 4;
const GI_REGISTER_READING_CALLBACK: usize = 8;
const GI_REGISTER_DEVICE_CALLBACK: usize = 9;
// IGameInputReading / IGameInputDevice slots.
const RELEASE: usize = 2;
const ADD_REF: usize = 1;
const READING_GET_MOTION_STATE: usize = 19;
const READING_GET_GAMEPAD_STATE: usize = 22;
const DEVICE_GET_INFO: usize = 3;
const DEVICE_GET_BATTERY_STATE: usize = 5;
const DEVICE_SET_RUMBLE_STATE: usize = 10;
const BATTERY_CHARGING: i32 = 3;

const POLL_ACTIVE: Duration = Duration::from_millis(4);
/// Idle cadence when GameInput's reading callback can't be trusted to wake the poller.
const POLL_IDLE: Duration = Duration::from_millis(8);
/// How long every pad must be unchanged before the poller goes idle.
const IDLE_AFTER: Duration = Duration::from_secs(2);
/// While idle on the reading callback, the poller still looks this often (battery, a missed wake).
const IDLE_CHECK: Duration = Duration::from_millis(100);
/// Analog change below which GameInput raises no reading callback (stick noise at rest).
const WAKE_THRESHOLD: f32 = 0.01;
/// Motion is published for awareness, not control, so 20 Hz is plenty.
const MOTION_EVERY: Duration = Duration::from_millis(50);

/// GameInputGamepadButtons → W3C standard-pad button index.
const BUTTONS: [(u32, u16); 14] = [
    (0x0001, button::START),
    (0x0002, button::BACK),
    (0x0004, button::SOUTH),
    (0x0008, button::EAST),
    (0x0010, button::WEST),
    (0x0020, button::NORTH),
    (0x0040, button::DPAD_UP),
    (0x0080, button::DPAD_DOWN),
    (0x0100, button::DPAD_LEFT),
    (0x0200, button::DPAD_RIGHT),
    (0x0400, button::LEFT_SHOULDER),
    (0x0800, button::RIGHT_SHOULDER),
    (0x1000, button::LEFT_STICK),
    (0x2000, button::RIGHT_STICK),
];

/// A COM object pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Obj(usize);

impl Obj {
    /// # Safety
    /// `self` must be a live COM object whose vtable has at least `i + 1` slots.
    unsafe fn slot(self, i: usize) -> usize {
        unsafe { *(*(self.0 as *const *const usize)).add(i) }
    }

    /// # Safety
    /// `self` must be a live COM object.
    unsafe fn add_ref(self) {
        unsafe {
            let f: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(self.slot(ADD_REF));
            f(self.0 as *mut c_void);
        }
    }

    /// # Safety
    /// `self` must be a live COM object on which the caller holds a reference.
    unsafe fn release(self) {
        unsafe {
            let f: extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(self.slot(RELEASE));
            f(self.0 as *mut c_void);
        }
    }
}

/// A connected gamepad and what never changes about it, read once at connect.
#[derive(Clone, PartialEq, Eq)]
struct Pad {
    obj: Obj,
    vid: u16,
    pid: u16,
    path: String,
    motion: bool,
}

/// A reference on a pad, taken under the [`DEVICES`] lock and released on drop, so a device the
/// callback removes stays alive for whoever is still using it.
struct Held(Pad);

impl Drop for Held {
    fn drop(&mut self) {
        // SAFETY: the reference `snapshot` took.
        unsafe { self.0.obj.release() };
    }
}

/// Connected gamepads, maintained by GameInput's device callback; each entry owns one reference.
static DEVICES: Mutex<Vec<Pad>> = Mutex::new(Vec::new());
/// Pads that disconnected, for the poll thread to release their held controls.
static GONE: Mutex<Vec<Pad>> = Mutex::new(Vec::new());
static RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set by GameInput's reading callback: a pad produced a new reading.
static FRESH: Mutex<bool> = Mutex::new(false);
static WAKE: std::sync::Condvar = std::sync::Condvar::new();

extern "system" fn on_reading(_token: u64, _ctx: *mut c_void, _reading: *mut c_void, _overrun: bool) {
    // The reading stays GameInput's; the poll thread reads the current state itself.
    *FRESH.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
    WAKE.notify_one();
}

/// Block until a reading callback fires or `timeout` passes. `true` when woken by a reading.
fn wait_for_reading(timeout: Duration) -> bool {
    let fresh = FRESH.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (mut fresh, _) = WAKE.wait_timeout_while(fresh, timeout, |f| !*f).unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::replace(&mut *fresh, false)
}

fn devices() -> std::sync::MutexGuard<'static, Vec<Pad>> {
    DEVICES.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Every connected pad, each with a reference of its own.
fn snapshot() -> Vec<Held> {
    devices()
        .iter()
        .map(|p| {
            // SAFETY: listed devices hold the list's reference while the lock is held.
            unsafe { p.obj.add_ref() };
            Held(p.clone())
        })
        .collect()
}

/// GameInput is delivering pads, so the XInput-HID shadows Raw Input also sees are duplicates.
#[must_use]
pub fn running() -> bool {
    RUNNING.load(std::sync::atomic::Ordering::Acquire)
}

/// A pad with this pid is delivered through GameInput; Raw Input's view of it is a duplicate.
#[must_use]
pub fn owns(pid: u16) -> bool {
    devices().iter().any(|p| p.pid == pid)
}

/// Connected GameInput units with this VID:PID; the HID shadow can only be joined when this is one.
pub(crate) fn unit_count(vid: u16, pid: u16) -> usize {
    devices().iter().filter(|p| (p.vid, p.pid) == (vid, pid)).count()
}

extern "system" fn on_device(_token: u64, _ctx: *mut c_void, device: *mut c_void, _ts: u64, current: u32, _previous: u32) {
    let obj = Obj(device as usize);
    let mut list = devices();
    let known = list.iter().position(|p| p.obj == obj);
    match known {
        None if current & STATUS_CONNECTED != 0 => {
            // SAFETY: GameInput hands us a live device; the reference keeps it past the callback.
            let pad = unsafe {
                obj.add_ref();
                let (vid, pid, id) = identity(obj);
                Pad { obj, vid, pid, path: format!("gameinput#{vid:04x}:{pid:04x}#{id}"), motion: supported_input(obj) & KIND_MOTION != 0 }
            };
            let (vid, pid, path) = (pad.vid, pad.pid, pad.path.clone());
            list.push(pad);
            drop(list);
            crate::hid_haptics::gameinput_devices_changed();
            // The device-tree name lookup runs after the list is released: the poll thread waits on it.
            let name = crate::hid_haptics::usb_name(vid, pid);
            crate::badge::learn(crate::registry::CanonicalPid::of(pid), Some(crate::badge::Emblem::Pad), name.as_deref().unwrap_or(""));
            if std::env::var_os("NEURON_DEBUG").is_some() {
                eprintln!("[gameinput] pad connected {path}");
            }
        }
        Some(i) if current & STATUS_CONNECTED == 0 => {
            let pad = list.remove(i);
            // SAFETY: the list's reference, taken on connect; a poll snapshot holds its own.
            unsafe { pad.obj.release() };
            GONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(pad);
            drop(list);
            crate::hid_haptics::gameinput_devices_changed();
        }
        _ => {}
    }
}

/// `supportedInput` from the device info (offset 104 in `GameInputDeviceInfo`).
///
/// # Safety
/// `device` must be a live `IGameInputDevice`.
unsafe fn supported_input(device: Obj) -> u32 {
    unsafe {
        let f: extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(device.slot(DEVICE_GET_INFO));
        let p = f(device.0 as *mut c_void);
        u32::from_le_bytes([*p.add(104), *p.add(105), *p.add(106), *p.add(107)])
    }
}

/// Moves the motors of GameInput pads.
struct RumbleSink;

impl crate::haptics::Sink for RumbleSink {
    fn owns(&self, device: &str) -> bool {
        devices().iter().any(|p| p.path == device)
    }
    fn set(&self, device: &str, r: crate::haptics::Rumble) -> bool {
        let Some(pad) = snapshot().into_iter().find(|p| p.0.path == device) else { return false };
        let params = [r.low, r.high, r.left_trigger, r.right_trigger];
        // SAFETY: `pad` holds its own reference until dropped at the end of this call.
        unsafe {
            let f: extern "system" fn(*mut c_void, *const [f32; 4]) = std::mem::transmute(pad.0.obj.slot(DEVICE_SET_RUMBLE_STATE));
            f(pad.0.obj.0 as *mut c_void, &params);
        }
        true
    }
}

/// `(vid, pid, device id prefix)` from the device's `GameInputDeviceInfo`.
///
/// # Safety
/// `device` must be a live `IGameInputDevice`.
unsafe fn identity(device: Obj) -> (u16, u16, String) {
    unsafe {
        let f: extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(device.slot(DEVICE_GET_INFO));
        let p = f(device.0 as *mut c_void);
        let vid = u16::from_le_bytes([*p.add(4), *p.add(5)]);
        let pid = u16::from_le_bytes([*p.add(6), *p.add(7)]);
        // deviceId: 32 bytes at offset 32 (APP_LOCAL_DEVICE_ID) — stable per physical unit.
        let id: String = (0..8).map(|i| format!("{:02x}", *p.add(32 + i))).collect();
        (vid, pid, id)
    }
}

/// Map a `GameInputGamepadState` (buttons, LT, RT, LX, LY, RX, RY) to the standard pad.
fn standard(buttons: u32, axes: [f32; 6]) -> StandardPad {
    let [lt, rt, lx, ly, rx, ry] = axes;
    StandardPad {
        buttons: BUTTONS.iter().filter(|(bit, _)| buttons & bit != 0).fold(0, |acc, (_, b)| acc | 1 << b),
        left_trigger: lt,
        right_trigger: rt,
        left_x: lx,
        left_y: -ly, // GameInput's Y grows upward; the standard pad's grows downward
        right_x: rx,
        right_y: -ry,
    }
}

/// Start the GameInput pad source once. Silently absent when GameInput isn't available.
pub fn start() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        crate::worker::spawn_detached("neuron-gameinput", run);
    });
}

fn run() {
    // SAFETY (whole body): COM calls through vtable slots matching the SDK header; every object
    // pointer is GameInput's own, each pad is used only through a `Held` reference, and each
    // reading is released after use.
    unsafe {
        let dll: Vec<u16> = "GameInput.dll\0".encode_utf16().collect();
        let lib = LoadLibraryW(dll.as_ptr());
        if lib.is_null() {
            return;
        }
        let Some(create) = GetProcAddress(lib, c"GameInputCreate".as_ptr().cast()) else { return };
        let create: extern "system" fn(*mut *mut c_void) -> i32 = std::mem::transmute(create);
        let mut raw: *mut c_void = std::ptr::null_mut();
        if create(&mut raw) < 0 || raw.is_null() {
            return;
        }
        let gi = Obj(raw as usize);
        type RegisterDevice = extern "system" fn(
            *mut c_void,
            *mut c_void,
            u32,
            u32,
            u32,
            *mut c_void,
            extern "system" fn(u64, *mut c_void, *mut c_void, u64, u32, u32),
            *mut u64,
        ) -> i32;
        let register: RegisterDevice = std::mem::transmute(gi.slot(GI_REGISTER_DEVICE_CALLBACK));
        let mut token = 0u64;
        if register(gi.0 as *mut c_void, std::ptr::null_mut(), KIND_GAMEPAD, STATUS_CONNECTED, ASYNC_ENUMERATION, std::ptr::null_mut(), on_device, &mut token) < 0 {
            return;
        }
        RUNNING.store(true, std::sync::atomic::Ordering::Release);
        // Readings wake an idle poller; without the callback, idle polling carries on at 8 ms.
        type RegisterReading = extern "system" fn(
            *mut c_void,
            *mut c_void,
            u32,
            f32,
            *mut c_void,
            extern "system" fn(u64, *mut c_void, *mut c_void, bool),
            *mut u64,
        ) -> i32;
        let register_reading: RegisterReading = std::mem::transmute(gi.slot(GI_REGISTER_READING_CALLBACK));
        let mut reading_token = 0u64;
        let mut woken_by_readings = register_reading(
            gi.0 as *mut c_void,
            std::ptr::null_mut(),
            KIND_GAMEPAD,
            WAKE_THRESHOLD,
            std::ptr::null_mut(),
            on_reading,
            &mut reading_token,
        ) >= 0;
        let mut waited_idle = false;
        type GetReading = extern "system" fn(*mut c_void, u32, *mut c_void, *mut *mut c_void) -> i32;
        let get_reading: GetReading = std::mem::transmute(gi.slot(GI_GET_CURRENT_READING));
        crate::haptics::register(Box::new(RumbleSink));
        let mut battery_at = crate::timing::ago(Duration::from_secs(10));
        let mut motion_at = crate::timing::ago(Duration::from_secs(1));
        let mut changed_at = Instant::now();

        let epoch = Instant::now();
        let mut analog = crate::analog::Devices::default();
        let mut last_hits: std::collections::HashMap<String, Vec<(u16, u16)>> = std::collections::HashMap::new();
        loop {
            // A pad that left mid-press releases everything it held, after its last reading.
            for pad in std::mem::take(&mut *GONE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)) {
                crate::sensors::forget(&pad.path);
                crate::analog::publish_sticks(&pad.path, Vec::new());
                analog.forget(&pad.path);
                if last_hits.remove(&pad.path).is_some_and(|h| !h.is_empty()) {
                    crate::controls::inject_event(crate::controls::ControlEvent {
                        pid: Some(crate::registry::CanonicalPid::of(pad.pid)),
                        stream: crate::controls::Stream::Pad,
                        hits: Vec::new(),
                        raw: Vec::new(),
                    });
                }
            }
            let pads = snapshot();
            if pads.is_empty() {
                std::thread::sleep(Duration::from_millis(250));
                continue;
            }
            let sample_motion = motion_at.elapsed() >= MOTION_EVERY;
            if sample_motion {
                motion_at = Instant::now();
            }
            for held in &pads {
                let pad = &held.0;
                let mut r: *mut c_void = std::ptr::null_mut();
                if get_reading(gi.0 as *mut c_void, KIND_GAMEPAD, pad.obj.0 as *mut c_void, &mut r) < 0 || r.is_null() {
                    continue;
                }
                let reading = Obj(r as usize);
                let state: extern "system" fn(*mut c_void, *mut [u32; 7]) -> bool = std::mem::transmute(reading.slot(READING_GET_GAMEPAD_STATE));
                let mut st = [0u32; 7];
                let ok = state(r, &mut st);
                reading.release();
                if !ok {
                    continue;
                }
                let axes = [1, 2, 3, 4, 5, 6].map(|i| f32::from_bits(st[i]));
                let standard = standard(st[0], axes);
                if pad.motion && sample_motion {
                    let mut m: *mut c_void = std::ptr::null_mut();
                    if get_reading(gi.0 as *mut c_void, KIND_MOTION, pad.obj.0 as *mut c_void, &mut m) >= 0 && !m.is_null() {
                        let motion = Obj(m as usize);
                        let f: extern "system" fn(*mut c_void, *mut [f32; 20]) -> bool = std::mem::transmute(motion.slot(READING_GET_MOTION_STATE));
                        let mut ms = [0f32; 20];
                        if f(m, &mut ms) {
                            crate::sensors::publish(&pad.path, "motion", crate::sensors::Reading::Motion {
                                accel: [ms[0], ms[1], ms[2]],
                                gyro: [ms[3], ms[4], ms[5]],
                            });
                        }
                        motion.release();
                    }
                }
                let dev = analog.device(&pad.path);
                let mut hits = standard.button_hits();
                hits.extend(dev.observe(&standard.values(), epoch.elapsed().as_millis() as u64));
                crate::analog::publish_sticks(&pad.path, dev.sticks());
                if last_hits.get(&pad.path) != Some(&hits) {
                    if waited_idle {
                        // The pad changed while the poller slept on the callback, and no callback
                        // said so: it can't be trusted to wake us, so poll instead from now on.
                        woken_by_readings = false;
                        eprintln!("[gameinput] the reading callback missed a change; polling instead");
                    }
                    changed_at = Instant::now();
                    last_hits.insert(pad.path.clone(), hits.clone());
                    crate::controls::inject_event(crate::controls::ControlEvent {
                        pid: Some(crate::registry::CanonicalPid::of(pad.pid)),
                        stream: crate::controls::Stream::Pad,
                        hits,
                        raw: Vec::new(),
                    });
                }
            }
            if battery_at.elapsed() >= Duration::from_secs(2) {
                battery_at = Instant::now();
                for held in &pads {
                    // GameInputBatteryState: chargeRate, maxChargeRate, remainingCapacity,
                    // fullChargeCapacity (f32), then status (i32).
                    let mut b = [0u32; 5];
                    let f: extern "system" fn(*mut c_void, *mut [u32; 5]) = std::mem::transmute(held.0.obj.slot(DEVICE_GET_BATTERY_STATE));
                    f(held.0.obj.0 as *mut c_void, &mut b);
                    let (remaining, full, status) = (f32::from_bits(b[2]), f32::from_bits(b[3]), b[4] as i32);
                    if status > 0 && full > 0.0 {
                        crate::sensors::publish(&held.0.path, "battery", crate::sensors::Reading::Battery {
                            level: (remaining / full).clamp(0.0, 1.0),
                            charging: Some(status == BATTERY_CHARGING),
                        });
                    }
                }
            }
            drop(pads);
            // Full rate while the pads are in use; half rate once they have been still a while.
            let idle = changed_at.elapsed() >= IDLE_AFTER;
            waited_idle = false;
            if !idle {
                std::thread::sleep(POLL_ACTIVE);
            } else if woken_by_readings {
                // Asleep until a pad produces a reading; a timeout check that finds a change
                // anyway means the callback missed it.
                waited_idle = !wait_for_reading(IDLE_CHECK);
            } else {
                std::thread::sleep(POLL_IDLE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_state_maps_to_the_standard_pad() {
        // A + View + d-pad left, left stick pushed up (GameInput +1), right trigger half pulled.
        let pad = standard(0x0004 | 0x0002 | 0x0100, [0.0, 0.5, 0.0, 1.0, 0.0, 0.0]);
        assert_eq!(pad.buttons, (1 << button::SOUTH) | (1 << button::BACK) | (1 << button::DPAD_LEFT));
        assert_eq!(pad.left_y, -1.0, "up is negative on the standard pad");
        assert_eq!(pad.right_trigger, 0.5);
    }
}
