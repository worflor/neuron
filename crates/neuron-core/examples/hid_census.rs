// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only: every HID collection on the machine, and for each non-keyboard/mouse input
//! collection what its report descriptor declares — button ranges and value fields (usage page,
//! usage, logical range, bit size, report id), as the OS parses them. Run: hid_census [vid]
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_GetButtonCaps, HidP_GetCaps, HidP_GetValueCaps, HidP_Input,
        HIDP_BUTTON_CAPS, HIDP_CAPS, HIDP_VALUE_CAPS,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};

    let only_vid = std::env::args().nth(1).and_then(|v| u16::from_str_radix(&v, 16).ok());
    for i in neuron::transport::enumerate()? {
        if only_vid.is_some_and(|v| v != i.vid) {
            continue;
        }
        println!(
            "{:04x}:{:04x} up={:04x} u={:04x} in={} out={} feat={} '{}'",
            i.vid, i.pid, i.usage_page, i.usage, i.input_len, i.output_len, i.feature_len, i.product
        );
        let kbd_or_mouse = i.usage_page == 0x01 && matches!(i.usage, 0x02 | 0x06);
        if kbd_or_mouse || i.input_len == 0 {
            continue;
        }
        let wide: Vec<u16> = i.path.as_os_str().encode_wide_null();
        // SAFETY: a zero-access open of an enumerated interface path, closed below; the preparsed
        // data is freed before the handle.
        unsafe {
            let h = CreateFileW(wide.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            if h == INVALID_HANDLE_VALUE {
                println!("    (not openable)");
                continue;
            }
            let mut pp = 0isize;
            if HidD_GetPreparsedData(h, &mut pp) == 0 {
                CloseHandle(h);
                continue;
            }
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            HidP_GetCaps(pp, &mut caps);
            let mut nb = caps.NumberInputButtonCaps;
            let mut bcaps: Vec<HIDP_BUTTON_CAPS> = vec![std::mem::zeroed(); nb as usize];
            HidP_GetButtonCaps(HidP_Input, bcaps.as_mut_ptr(), &mut nb, pp);
            for b in &bcaps[..nb as usize] {
                let (lo, hi) = if b.IsRange != 0 {
                    (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax)
                } else {
                    (b.Anonymous.NotRange.Usage, b.Anonymous.NotRange.Usage)
                };
                println!("    buttons  rid={:02x} page={:04x} usage {:04x}..{:04x}", b.ReportID, b.UsagePage, lo, hi);
            }
            let mut nv = caps.NumberInputValueCaps;
            let mut vcaps: Vec<HIDP_VALUE_CAPS> = vec![std::mem::zeroed(); nv as usize];
            HidP_GetValueCaps(HidP_Input, vcaps.as_mut_ptr(), &mut nv, pp);
            for v in &vcaps[..nv as usize] {
                let (lo, hi) = if v.IsRange != 0 {
                    (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
                } else {
                    (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage)
                };
                println!(
                    "    value    rid={:02x} page={:04x} usage {:04x}..{:04x} logical {}..{} bits={} count={} null={}",
                    v.ReportID, v.UsagePage, lo, hi, v.LogicalMin, v.LogicalMax, v.BitSize, v.ReportCount, v.HasNull
                );
            }
            let mut no = caps.NumberOutputValueCaps;
            let mut ocaps: Vec<HIDP_VALUE_CAPS> = vec![std::mem::zeroed(); no as usize];
            HidP_GetValueCaps(windows_sys::Win32::Devices::HumanInterfaceDevice::HidP_Output, ocaps.as_mut_ptr(), &mut no, pp);
            for v in &ocaps[..no as usize] {
                let (lo, hi) = if v.IsRange != 0 {
                    (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
                } else {
                    (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage)
                };
                println!(
                    "    OUTPUT   rid={:02x} page={:04x} usage {:04x}..{:04x} logical {}..{} bits={} count={}",
                    v.ReportID, v.UsagePage, lo, hi, v.LogicalMin, v.LogicalMax, v.BitSize, v.ReportCount
                );
            }
            HidD_FreePreparsedData(pp);
            CloseHandle(h);
        }
    }
    Ok(())
}

#[cfg(windows)]
trait WideNull {
    fn encode_wide_null(&self) -> Vec<u16>;
}
#[cfg(windows)]
impl WideNull for std::ffi::OsStr {
    fn encode_wide_null(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(std::iter::once(0)).collect()
    }
}

#[cfg(not(windows))]
fn main() {}
