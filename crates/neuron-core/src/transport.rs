//! Platform-agnostic transport: send/receive HID feature reports to the control pipe.
//!
//! Windows uses the native `windows-sys` path (open with access=0, feature IOCTLs are
//! FILE_ANY_ACCESS — which is how you talk to a protected HID mouse). Other platforms
//! can drop in a hidapi/hidraw impl behind the same trait later.

use anyhow::Result;
use std::ffi::{OsStr, OsString};

/// An opaque handle key identifying one enumerated HID interface.
///
/// The portable layer (this trait + `device.rs`) only stores, clones, and compares it — it never
/// inspects the contents. Each platform backend is the sole place that knows the encoding: on
/// Windows it's a UTF-16 device-interface path consumed by `CreateFileW`; a future hidraw/IOKit
/// backend keys on a `CString`/`&str`. `OsString` is the platform-native opaque string that lets
/// every backend round-trip its own native path losslessly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DevicePath(OsString);

impl DevicePath {
    /// The backend converts this to its native path type (e.g. wide chars on Windows).
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Build from a Windows wide string, stripping a trailing NUL if present so the stored key is
    /// the bare path. (The NUL is re-added on demand by [`to_wide_nul`].)
    #[cfg(windows)]
    pub fn from_wide(w: &[u16]) -> DevicePath {
        use std::os::windows::ffi::OsStringExt;
        let trimmed = match w.last() {
            Some(0) => &w[..w.len() - 1],
            _ => w,
        };
        DevicePath(OsString::from_wide(trimmed))
    }

    /// The NUL-terminated wide string `CreateFileW` consumes. The terminator is centralized here so
    /// no caller can forget it.
    #[cfg(windows)]
    pub fn to_wide_nul(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.0.encode_wide().chain(std::iter::once(0)).collect()
    }

    /// TEST SEAM — build a `DevicePath` from a plain string, cross-platform (the real
    /// constructors are Windows-only wide-string ceremony). Lets other crates' tests (e.g.
    /// neuron-host's `bridge::discover_from` tests) construct synthetic HID paths without
    /// depending on a platform backend. Not for production use — real paths come from
    /// `enumerate()`.
    #[doc(hidden)]
    pub fn from_str_for_tests(s: &str) -> DevicePath {
        DevicePath(OsString::from(s))
    }
}

/// One enumerated HID collection.
pub struct HidDeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub path: DevicePath, // platform-opaque handle key
    /// The device's own USB product string (e.g. "Razer Naga V2 Pro"), empty when the
    /// OS/device doesn't offer one. Used to give auto-synthesized device defs an honest
    /// name instead of a bare pid.
    pub product: String,
}

impl HidDeviceInfo {
    /// The identity of the PHYSICAL unit this collection belongs to — see [`path_instance`].
    pub fn instance(&self) -> String {
        path_instance(&self.path.0.to_string_lossy())
    }
}

/// Reduce a raw HID device-interface path to the identity of the PHYSICAL device it belongs to —
/// the thing that tells "two collections of one device" apart from "two identical devices".
/// This is the app-wide per-UNIT identity: everything that must address one specific physical
/// unit (the device panel's rows, the selected-device control plane, the host bridge's surface
/// keys) derives it from here, so they can never disagree.
///
/// Heuristic over the Windows HID path shape, e.g.
/// `\\?\hid#vid_1532&pid_0221&mi_01&col02#8&2f5ca30f&0&0001#{4d1e55b2-f16f-11cf-88cb-001111000030}`:
/// - `mi_XX` (multiple-interface index) and `colXX` (collection index) are interface-level, not
///   device-level, so they're stripped — a keyboard's several collections (main + consumer
///   control + vendor) must collapse to ONE instance.
/// - the trailing `#{guid}` is the device-interface-CLASS guid (identical for every unit of the
///   same kind of HID device) — stripped too, it carries no per-unit information.
/// - what survives — vid/pid plus the container id (`8&2f5ca30f&0&0001`) — is what actually
///   differs between two identical devices plugged into different USB ports.
///
/// Regex-free by design (no new dependency): lowercase + segment filtering only.
pub fn path_instance(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    // Drop the trailing "#{...}" interface-class guid, if present.
    let without_guid = match lower.rfind("#{") {
        Some(i) => &lower[..i],
        None => lower.as_str(),
    };
    without_guid
        .split('#')
        .map(|segment| {
            segment
                .split('&')
                .filter(|part| !(part.starts_with("mi_") || part.starts_with("col")))
                .collect::<Vec<_>>()
                .join("&")
        })
        .collect::<Vec<_>>()
        .join("#")
}

/// A feature-report channel to one device.
pub trait Transport {
    fn set_feature(&self, buf: &[u8]) -> Result<()>;
    fn get_feature(&self, buf: &mut [u8]) -> Result<()>;
}

/// A read channel for device-INITIATED input reports — the unsolicited reports a device pushes on
/// its own (e.g. a Razer mouse announcing "DPI is now X" when you press its onboard DPI button).
/// Feature reports are pull (request/response); these are push, so they need their own handle opened
/// with read access. `read` blocks until one report arrives (or the handle is closed). `Send` so a
/// listener thread can own it.
pub trait InputReader: Send {
    /// Block for the next input report; returns the number of bytes written into `buf`.
    fn read(&self, buf: &mut [u8]) -> Result<usize>;
}

#[cfg(windows)]
mod windows_hid;

#[cfg(windows)]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    windows_hid::enumerate()
}

#[cfg(windows)]
pub fn open_path(path: &DevicePath) -> Result<Box<dyn Transport>> {
    Ok(Box::new(windows_hid::WinHid::open(path)?))
}

/// Open a collection for READING its device-initiated input reports. Fails on OS-protected
/// collections (the mouse/keyboard top-level collections deny `GENERIC_READ`); succeeds on the
/// vendor collections where event reports actually ride.
#[cfg(windows)]
pub fn open_reader(path: &DevicePath) -> Result<Box<dyn InputReader>> {
    Ok(Box::new(windows_hid::WinHidReader::open(path)?))
}

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_path(_path: &DevicePath) -> Result<Box<dyn Transport>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_reader(_path: &DevicePath) -> Result<Box<dyn InputReader>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}
