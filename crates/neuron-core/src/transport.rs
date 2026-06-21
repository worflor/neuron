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
}

/// One enumerated HID collection.
pub struct HidDeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub path: DevicePath, // platform-opaque handle key
}

/// A feature-report channel to one device.
pub trait Transport {
    fn set_feature(&self, buf: &[u8]) -> Result<()>;
    fn get_feature(&self, buf: &mut [u8]) -> Result<()>;
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

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_path(_path: &DevicePath) -> Result<Box<dyn Transport>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}
