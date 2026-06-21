//! Platform-agnostic transport: send/receive HID feature reports to the control pipe.
//!
//! Windows uses the native `windows-sys` path (open with access=0, feature IOCTLs are
//! FILE_ANY_ACCESS — which is how you talk to a protected HID mouse). Other platforms
//! can drop in a hidapi/hidraw impl behind the same trait later.

use anyhow::Result;

/// One enumerated HID collection.
pub struct HidDeviceInfo {
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub path: Vec<u16>, // null-terminated wide path (platform-opaque handle key)
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
pub fn open_path(path: &[u16]) -> Result<Box<dyn Transport>> {
    Ok(Box::new(windows_hid::WinHid::open(path)?))
}

#[cfg(not(windows))]
pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}

#[cfg(not(windows))]
pub fn open_path(_path: &[u16]) -> Result<Box<dyn Transport>> {
    anyhow::bail!("transport not implemented on this platform yet (hidapi backend pending)")
}
