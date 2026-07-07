//! Windows HID transport via the Win32 API (windows-sys). Mirrors the proven approach:
//! open the control collection with dwDesiredAccess = 0 (Windows blocks GENERIC_R/W on a
//! mouse, but HidD_Get/SetFeature use FILE_ANY_ACCESS IOCTLs, so access=0 works).

use super::{DevicePath, HidDeviceInfo, Transport};
use anyhow::{bail, Result};
use std::ffi::c_void;
use std::ptr;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetFeature, HidD_GetHidGuid,
    HidD_GetPreparsedData, HidD_GetProductString, HidD_SetFeature, HidP_GetCaps,
    HIDD_ATTRIBUTES, HIDP_CAPS,
};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

const HIDP_OK: i32 = 0x0011_0000; // HIDP_STATUS_SUCCESS
const GENERIC_READ_FLAG: u32 = 0x8000_0000; // GENERIC_READ — declared locally to dodge windows-sys path churn

// `ReadFile` isn't exported under this windows-sys feature set; declare it directly. It lives in
// kernel32, which this crate already links (CreateFileW et al.), so the symbol resolves.
#[link(name = "kernel32")]
extern "system" {
    fn ReadFile(
        handle: HANDLE,
        buf: *mut c_void,
        len: u32,
        read: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
}

unsafe fn wide_from_ptr(p: *const u16) -> Vec<u16> {
    let mut v = Vec::new();
    let mut i = 0isize;
    loop {
        let c = *p.offset(i);
        v.push(c);
        if c == 0 {
            break;
        }
        i += 1;
    }
    v
}

unsafe fn query(path: &[u16]) -> Option<HidDeviceInfo> {
    let h = CreateFileW(
        path.as_ptr(),
        0, // query only — works on protected mouse/keyboard collections
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        ptr::null(),
        OPEN_EXISTING,
        0,
        ptr::null_mut(),
    );
    if h == INVALID_HANDLE_VALUE {
        return None;
    }
    let mut info = None;
    let mut attr: HIDD_ATTRIBUTES = std::mem::zeroed();
    attr.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if HidD_GetAttributes(h, &mut attr) != 0 {
        let mut pp: isize = 0; // PHIDP_PREPARSED_DATA is an opaque isize in windows-sys
        if HidD_GetPreparsedData(h, &mut pp) != 0 {
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            if HidP_GetCaps(pp, &mut caps) == HIDP_OK {
                // Product string (IOCTL_HID_GET_PRODUCT_STRING is FILE_ANY_ACCESS, so it works
                // on this access-0 handle like Get/SetFeature). Best-effort: empty on failure.
                let mut prod = [0u16; 127];
                let product = if HidD_GetProductString(
                    h,
                    prod.as_mut_ptr() as *mut c_void,
                    (prod.len() * 2) as u32,
                ) != 0
                {
                    let end = prod.iter().position(|&c| c == 0).unwrap_or(prod.len());
                    String::from_utf16_lossy(&prod[..end]).trim().to_string()
                } else {
                    String::new()
                };
                info = Some(HidDeviceInfo {
                    vid: attr.VendorID,
                    pid: attr.ProductID,
                    usage_page: caps.UsagePage,
                    usage: caps.Usage,
                    feature_len: caps.FeatureReportByteLength,
                    // `path` is the NUL-terminated wide buffer from `wide_from_ptr`; store it as the
                    // opaque key (NUL stripped) — `WinHid::open` re-adds it via `to_wide_nul`.
                    path: DevicePath::from_wide(path),
                    product,
                });
            }
            HidD_FreePreparsedData(pp);
        }
    }
    CloseHandle(h);
    info
}

pub fn enumerate() -> Result<Vec<HidDeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        let mut guid: GUID = std::mem::zeroed();
        HidD_GetHidGuid(&mut guid);
        let set = SetupDiGetClassDevsW(
            &guid,
            ptr::null(),
            ptr::null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        );
        if set == -1 {
            // HDEVINFO is an isize here; failure is INVALID_HANDLE_VALUE (-1).
            bail!("SetupDiGetClassDevs failed");
        }
        let mut idx = 0u32;
        loop {
            let mut ifa: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
            ifa.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
            if SetupDiEnumDeviceInterfaces(set, ptr::null_mut(), &guid, idx, &mut ifa) == 0 {
                break;
            }
            idx += 1;

            let mut req = 0u32;
            SetupDiGetDeviceInterfaceDetailW(
                set,
                &ifa,
                ptr::null_mut(),
                0,
                &mut req,
                ptr::null_mut(),
            );
            if req == 0 {
                continue;
            }
            let mut buf = vec![0u8; req as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            // cbSize is the size of the fixed header: 8 on 64-bit, 6 on 32-bit.
            (*detail).cbSize = if cfg!(target_pointer_width = "64") {
                8
            } else {
                6
            };
            if SetupDiGetDeviceInterfaceDetailW(set, &ifa, detail, req, &mut req, ptr::null_mut())
                == 0
            {
                continue;
            }
            let path_ptr = ptr::addr_of!((*detail).DevicePath) as *const u16;
            let path = wide_from_ptr(path_ptr);
            if let Some(info) = query(&path) {
                out.push(info);
            }
        }
        SetupDiDestroyDeviceInfoList(set);
    }
    Ok(out)
}

pub struct WinHid {
    handle: HANDLE,
}

impl WinHid {
    pub fn open(path: &DevicePath) -> Result<Self> {
        // Re-add the NUL terminator `CreateFileW` requires (centralized in `to_wide_nul`). This
        // reproduces the exact wide buffer the enumeration path passed to `CreateFileW`.
        let wide = path.to_wide_nul();
        unsafe {
            let h = CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                bail!("CreateFile on control interface failed");
            }
            Ok(WinHid { handle: h })
        }
    }
}

impl Drop for WinHid {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

/// A read handle for one collection's device-initiated input reports. Opened with `GENERIC_READ`
/// (which `ReadFile` needs) — so it FAILS on the OS-protected mouse/keyboard collections and only
/// succeeds on the vendor collections where Razer's event reports (DPI/stage changes) ride.
pub struct WinHidReader {
    handle: HANDLE,
}

// The handle is a raw OS pointer; we own it solely here and close it on drop, so it's safe to move
// to the listener thread that owns this reader.
unsafe impl Send for WinHidReader {}

impl WinHidReader {
    pub fn open(path: &DevicePath) -> Result<Self> {
        let wide = path.to_wide_nul();
        unsafe {
            let h = CreateFileW(
                wide.as_ptr(),
                GENERIC_READ_FLAG,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                0,
                ptr::null_mut(),
            );
            if h == INVALID_HANDLE_VALUE {
                bail!("CreateFile (read) failed — collection is OS-protected or busy");
            }
            Ok(WinHidReader { handle: h })
        }
    }
}

impl super::InputReader for WinHidReader {
    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        unsafe {
            let mut got: u32 = 0;
            // synchronous (handle opened without FILE_FLAG_OVERLAPPED) — blocks until a report lands.
            if ReadFile(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut got,
                ptr::null_mut(),
            ) == 0
            {
                bail!("ReadFile failed");
            }
            Ok(got as usize)
        }
    }
}

impl Drop for WinHidReader {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

impl Transport for WinHid {
    fn set_feature(&self, buf: &[u8]) -> Result<()> {
        unsafe {
            if HidD_SetFeature(self.handle, buf.as_ptr() as *const c_void, buf.len() as u32) == 0 {
                bail!("HidD_SetFeature failed");
            }
        }
        Ok(())
    }
    fn get_feature(&self, buf: &mut [u8]) -> Result<()> {
        unsafe {
            if HidD_GetFeature(
                self.handle,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
            ) == 0
            {
                bail!("HidD_GetFeature failed");
            }
        }
        Ok(())
    }
}
