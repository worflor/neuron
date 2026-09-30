// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Rumble any HID device whose descriptor declares Physical Interface Device magnitudes (page
//! 0x0F usage 0x70), filling the output report through the OS's own report builder: actuator
//! enable, magnitudes, duration, then all-zero to stop. Run: hid_rumble [ms] [percent]
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidD_FreePreparsedData, HidD_GetPreparsedData, HidP_GetCaps, HidP_InitializeReportForID, HidP_Output,
        HidP_SetUsageValue, HidP_SetUsageValueArray, HIDP_CAPS,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use std::os::windows::ffi::OsStrExt;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let ms: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(500);
    let pct: u8 = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(80);
    for i in neuron::transport::enumerate()? {
        if i.output_len == 0 {
            continue;
        }
        let wide: Vec<u16> = i.path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        // SAFETY: a zero-access open to read preparsed data; freed and closed below.
        let report = unsafe {
            let h = CreateFileW(wide.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, std::ptr::null_mut());
            if h == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut pp = 0isize;
            let ok = HidD_GetPreparsedData(h, &mut pp) != 0;
            CloseHandle(h);
            if !ok {
                continue;
            }
            let mut caps: HIDP_CAPS = std::mem::zeroed();
            HidP_GetCaps(pp, &mut caps);
            let len = usize::from(caps.OutputReportByteLength);
            let build = |mag: u8| -> Option<Vec<u8>> {
                let mut buf = vec![0u8; len];
                if HidP_InitializeReportForID(HidP_Output, 0, pp, buf.as_mut_ptr(), len as u32) != 0x0011_0000 {
                    return None;
                }
                let mags = [mag; 4];
                // Magnitudes are the defining field: without them this isn't a rumble report.
                if HidP_SetUsageValueArray(HidP_Output, 0x0F, 0, 0x70, mags.as_ptr(), 4, pp, buf.as_mut_ptr(), len as u32) != 0x0011_0000 {
                    return None;
                }
                HidP_SetUsageValue(HidP_Output, 0x0F, 0, 0x97, if mag > 0 { 0x0F } else { 0 }, pp, buf.as_mut_ptr(), len as u32);
                HidP_SetUsageValue(HidP_Output, 0x0F, 0, 0x50, 0xFF, pp, buf.as_mut_ptr(), len as u32);
                Some(buf)
            };
            let r = (build(pct), build(0));
            HidD_FreePreparsedData(pp);
            r
        };
        let (Some(on), Some(off)) = report else { continue };
        println!("{:04x}:{:04x} '{}': PID rumble report {:02x?}", i.vid, i.pid, i.product, on);
        let t = neuron::transport::open_path(&i.path)?;
        match t.write_output(&on) {
            Ok(()) => println!("  on"),
            Err(e) => println!("  write failed: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(ms));
        let _ = t.write_output(&off);
        println!("  off");
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
