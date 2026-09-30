// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Rumble for any HID device whose descriptor declares Physical Interface Device magnitudes
//! (page 0x0F usage 0x70). The output report is filled through the OS's own report builder from
//! the declared usages — actuator enable (0x97), magnitudes (0x70), duration (0x50) — so no
//! device gets bytes of its own. Verified 2026-09-30 on a PowerA pad's XInput-HID shadow, where
//! GameInput's own rumble call moved nothing.
//!
//! An Xbox-protocol pad's controls arrive through GameInput under `gameinput#vid:pid#…`, but its
//! motors sit on its XInput-HID shadow, a separate device. The two are linked through the device
//! tree: the shadow's parent carries the pad's USB serial, and so does the pad's own USB device,
//! whose vid:pid is the GameInput device's. Two identical pads can't be told apart this way; the
//! first match wins.
//!
//! Only a Game Pad collection (Generic Desktop 0x01/0x05) is a target: a joystick or wheel's
//! Physical Interface Device outputs drive force feedback, not rumble, and are never written.
//! The XInput-HID shadow class (`&IG_` paths, one descriptor Windows gives every Xbox-protocol pad)
//! is verified; any other pad's magnitudes have no declared motor roles, so writing them stays
//! behind `NEURON_HAPTICS_WRITE` until a pad of that kind is confirmed.
//!
//! Streaming callers (the radial's knob renderer) write ~125 times a second, so each thread keeps
//! its open [`Motors`] — write handle, parsed descriptor, report layout — and a write is only a
//! report fill. A device found to have no motors is not looked for again for a few seconds.

use crate::haptics::{Rumble, Sink};
use std::cell::RefCell;
use std::collections::HashMap;
use std::os::windows::ffi::OsStrExt;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW, CM_Get_Parent, CM_Locate_DevNodeW,
    CM_GETIDLIST_FILTER_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_GetCaps, HidP_GetValueCaps, HidP_InitializeReportForID, HidP_Output,
    HidP_SetUsageValue, HidP_SetUsageValueArray, HIDP_CAPS, HIDP_STATUS_SUCCESS, HIDP_VALUE_CAPS,
};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};

const PID: u16 = 0x0F;
const ENABLE: u16 = 0x97;
const MAGNITUDE: u16 = 0x70;
const DURATION: u16 = 0x50;

/// Register the backend with [`crate::haptics`].
pub fn register() {
    crate::haptics::register(Box::new(PidRumble));
}

/// An open rumble target: its write handle, its parsed descriptor and where its magnitudes sit.
struct Motors {
    transport: Box<dyn crate::transport::Transport>,
    pp: isize,
    len: usize,
    report_id: u8,
    count: usize,
    max: i32,
}

impl Drop for Motors {
    fn drop(&mut self) {
        // SAFETY: `pp` came from HidD_GetPreparsedData in `Motors::open` and is freed only here.
        unsafe {
            HidD_FreePreparsedData(self.pp);
        }
    }
}

impl Motors {
    /// Open `path` as a rumble target, or `None` when it declares no PID magnitudes.
    fn open(path: &crate::transport::DevicePath) -> Option<Motors> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: a zero-access handle for the parsed descriptor, closed at once; the descriptor is
        // then owned by the returned `Motors`, or freed here when the device declares no magnitudes.
        unsafe {
            let h = CreateFileW(wide.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            if h == INVALID_HANDLE_VALUE {
                return None;
            }
            let mut pp = 0isize;
            let got = HidD_GetPreparsedData(h, &raw mut pp) != 0;
            CloseHandle(h);
            if !got {
                return None;
            }
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            HidP_GetCaps(pp, &raw mut caps);
            let mut n = caps.NumberOutputValueCaps;
            let mut vcaps: Vec<HIDP_VALUE_CAPS> = vec![std::mem::zeroed(); usize::from(n)];
            HidP_GetValueCaps(HidP_Output, vcaps.as_mut_ptr(), &raw mut n, pp);
            let mag = vcaps[..usize::from(n)]
                .iter()
                .find(|v| v.UsagePage == PID && v.IsRange == 0 && v.Anonymous.NotRange.Usage == MAGNITUDE)
                .map(|v| (usize::from(v.ReportCount), v.LogicalMax.max(1), v.ReportID));
            let transport = if mag.is_some() { crate::transport::open_path(path).ok() } else { None };
            match (mag, transport) {
                (Some((count, max, report_id)), Some(transport)) => {
                    Some(Motors { transport, pp, len: usize::from(caps.OutputReportByteLength), report_id, count, max })
                }
                _ => {
                    HidD_FreePreparsedData(pp);
                    None
                }
            }
        }
    }

    /// Fill and send the report for `r`. Magnitudes go in the order the XInput-HID shadow uses
    /// (SDL_hidapi_xboxone.c, zlib): left trigger, right trigger, low, high; a device with fewer
    /// takes the body motors first.
    fn send(&self, r: Rumble) -> bool {
        let scale = |x: f32| (x.clamp(0.0, 1.0) * self.max as f32).round() as u8;
        let values: Vec<u8> = match self.count {
            4 => vec![scale(r.left_trigger), scale(r.right_trigger), scale(r.low), scale(r.high)],
            2 => vec![scale(r.low), scale(r.high)],
            n => vec![scale(r.low.max(r.high)); n.max(1)],
        };
        let mut buf = vec![0u8; self.len];
        let len = self.len as u32;
        // SAFETY: HID report builder calls on the descriptor this `Motors` owns, into an owned buffer.
        unsafe {
            if HidP_InitializeReportForID(HidP_Output, self.report_id, self.pp, buf.as_mut_ptr(), len) != HIDP_STATUS_SUCCESS
                || HidP_SetUsageValueArray(HidP_Output, PID, 0, MAGNITUDE, values.as_ptr(), values.len() as u16, self.pp, buf.as_mut_ptr(), len)
                    != HIDP_STATUS_SUCCESS
            {
                return false;
            }
            let on = values.iter().any(|v| *v > 0);
            // Optional fields: absent on some devices, so their status is not an error.
            HidP_SetUsageValue(HidP_Output, PID, 0, ENABLE, if on { (1 << self.count.min(4)) - 1 } else { 0 }, self.pp, buf.as_mut_ptr(), len);
            HidP_SetUsageValue(HidP_Output, PID, 0, DURATION, 0xFF, self.pp, buf.as_mut_ptr(), len);
        }
        self.transport.write_output(&buf).is_ok()
    }
}

/// Which HID device drives each named device's motors (`None`: none, as of when it was looked
/// for), so a write doesn't re-walk the device tree.
static TARGETS: std::sync::Mutex<Option<HashMap<String, (Option<crate::transport::DevicePath>, std::time::Instant)>>> =
    std::sync::Mutex::new(None);
/// How long "this device has no motors" is believed before looking again (it may be replugged).
const MISS_TTL: std::time::Duration = std::time::Duration::from_secs(5);

thread_local! {
    /// This thread's open targets, keyed by the name callers use.
    static OPEN: RefCell<HashMap<String, Motors>> = RefCell::new(HashMap::new());
}

/// Writes to a pad outside the verified XInput-HID shadow class are enabled.
fn unverified_writes() -> bool {
    std::env::var_os("NEURON_HAPTICS_WRITE").is_some()
}

fn target_for(device: &str) -> Option<crate::transport::DevicePath> {
    if let Some((p, at)) = TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(|m| m.get(device)) {
        if p.is_some() || at.elapsed() < MISS_TTL {
            return p.clone();
        }
    }
    let found = crate::transport::enumerate().ok().and_then(|infos| {
        infos.into_iter().filter(|i| i.output_len > 0 && (i.usage_page, i.usage) == (0x01, 0x05)).find(|i| {
            let path = i.path.as_os_str().to_string_lossy();
            if !path.to_ascii_lowercase().contains("&ig_") && !unverified_writes() {
                return false;
            }
            path.eq_ignore_ascii_case(device)
                || (device.starts_with("gameinput#")
                    && gip_link(&path).is_some_and(|(vid, pid)| device.starts_with(&format!("gameinput#{vid:04x}:{pid:04x}#"))))
        })
    });
    let path = found.map(|i| i.path);
    TARGETS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(Default::default)
        .insert(device.to_string(), (path.clone(), std::time::Instant::now()));
    path
}

struct PidRumble;

impl Sink for PidRumble {
    fn owns(&self, device: &str) -> bool {
        target_for(device).is_some()
    }
    fn set(&self, device: &str, r: Rumble) -> bool {
        OPEN.with(|open| {
            let mut open = open.borrow_mut();
            if !open.contains_key(device) {
                let Some(path) = target_for(device) else { return false };
                let Some(motors) = Motors::open(&path) else {
                    forget_target(device);
                    return false;
                };
                open.insert(device.to_string(), motors);
            }
            let sent = open.get(device).is_some_and(|m| m.send(r));
            if !sent {
                // Unplugged or re-enumerated: found afresh on the next write.
                open.remove(device);
                forget_target(device);
            }
            sent
        })
    }
}

/// The target failed to open or write: remember that it has none for now.
fn forget_target(device: &str) {
    if let Some(m) = TARGETS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
        m.insert(device.to_string(), (None, std::time::Instant::now()));
    }
}

/// For an XInput-HID shadow ("&IG_" in its path), the vid:pid of the pad it shadows: the USB
/// device whose instance id ends with the same serial as the shadow's parent.
fn gip_link(path: &str) -> Option<(u16, u16)> {
    if !path.to_ascii_lowercase().contains("&ig_") {
        return None;
    }
    let parent = parent_instance(&instance_id(path)?)?;
    let serial = parent.rsplit('&').next()?.to_ascii_uppercase();
    present_ids("USB").into_iter().find_map(|id| {
        let up = id.to_ascii_uppercase();
        if up.contains("&IG_") || !up.ends_with(&format!("\\{serial}")) {
            return None;
        }
        let hex = |key: &str| up.split(key).nth(1).and_then(|s| u16::from_str_radix(s.get(..4)?, 16).ok());
        Some((hex("VID_")?, hex("PID_")?))
    })
}

/// The name a present USB device reports on the bus ("PowerA Xbox Series X Controller"), for a
/// device whose own interfaces carry no product string.
#[must_use]
pub fn usb_name(vid: u16, pid: u16) -> Option<String> {
    use windows_sys::Win32::Devices::DeviceAndDriverInstallation::CM_Get_DevNode_PropertyW;
    use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_BusReportedDeviceDesc, DEVPROP_TYPE_STRING};
    let key = format!("\\VID_{vid:04X}&PID_{pid:04X}\\");
    let id = present_ids("USB").into_iter().find(|id| id.to_ascii_uppercase().contains(&key))?;
    let w: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: configuration-manager queries into owned buffers of the sizes passed.
    unsafe {
        let mut node = 0u32;
        if CM_Locate_DevNodeW(&raw mut node, w.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            return None;
        }
        let mut kind = 0u32;
        let mut buf = vec![0u16; 256];
        let mut size = (buf.len() * 2) as u32;
        if CM_Get_DevNode_PropertyW(node, &DEVPKEY_Device_BusReportedDeviceDesc, &raw mut kind, buf.as_mut_ptr().cast(), &raw mut size, 0)
            != CR_SUCCESS
            || kind != DEVPROP_TYPE_STRING
        {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]).trim().to_string()).filter(|s| !s.is_empty())
    }
}

/// A device interface path (`\\?\HID#VID_…#inst#{guid}`) → its device instance id
/// (`HID\VID_…\inst`).
fn instance_id(path: &str) -> Option<String> {
    let body = path.strip_prefix(r"\\?\")?;
    let mut parts: Vec<&str> = body.split('#').collect();
    if parts.len() < 3 {
        return None;
    }
    parts.pop(); // the interface class guid
    Some(parts.join("\\"))
}

fn parent_instance(instance: &str) -> Option<String> {
    let w: Vec<u16> = instance.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: plain configuration-manager queries into owned buffers.
    unsafe {
        let mut node = 0u32;
        if CM_Locate_DevNodeW(&raw mut node, w.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) != CR_SUCCESS {
            return None;
        }
        let mut parent = 0u32;
        if CM_Get_Parent(&raw mut parent, node, 0) != CR_SUCCESS {
            return None;
        }
        let mut buf = vec![0u16; 512];
        if CM_Get_Device_IDW(parent, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..end]))
    }
}

/// Every present device instance id under an enumerator ("USB").
fn present_ids(enumerator: &str) -> Vec<String> {
    let filter: Vec<u16> = enumerator.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: size query then fill of an owned buffer of that size.
    unsafe {
        let flags = CM_GETIDLIST_FILTER_PRESENT | 0x0000_0001; // | CM_GETIDLIST_FILTER_ENUMERATOR
        let mut len = 0u32;
        if CM_Get_Device_ID_List_SizeW(&raw mut len, filter.as_ptr(), flags) != CR_SUCCESS || len == 0 {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        if CM_Get_Device_ID_ListW(filter.as_ptr(), buf.as_mut_ptr(), len, flags) != CR_SUCCESS {
            return Vec::new();
        }
        buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_paths_become_instance_ids() {
        assert_eq!(
            instance_id(r"\\?\HID#VID_045E&PID_02FF&IG_00#7&278e89ed&0&0000#{4d1e55b2-f16f-11cf-88cb-001111000030}").as_deref(),
            Some(r"HID\VID_045E&PID_02FF&IG_00\7&278e89ed&0&0000")
        );
        assert_eq!(instance_id("gameinput#24c6:543a#x"), None);
    }
}
