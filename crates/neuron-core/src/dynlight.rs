// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Windows Dynamic Lighting: whether Windows is set to drive HID LampArray devices itself.
//!
//! A keyboard that exposes a LampArray collection can be held by Windows (Settings ›
//! Personalization › Dynamic Lighting) while Neuron writes its vendor protocol; vendor apps refuse
//! to drive lighting in that state. Windows keeps the switch in `HKCU\Software\Microsoft\Lighting`
//! as `AmbientLightingEnabled` (and `ControlledByForegroundApp` for "compatible apps in the
//! foreground always control lighting"). The value names are undocumented; this reads them and
//! never writes.

use serde::Serialize;

/// The Dynamic Lighting switches, as Windows stores them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DynamicLighting {
    /// "Use Dynamic Lighting on my devices".
    pub enabled: bool,
    /// "Compatible apps in the foreground always control lighting".
    pub foreground_apps_control: bool,
}

/// The current switches; `None` off Windows, before Windows 11's Lighting settings exist, or when
/// the values cannot be read.
#[must_use]
pub fn state() -> Option<DynamicLighting> {
    #[cfg(windows)]
    {
        let enabled = read_dword("AmbientLightingEnabled")?;
        Some(DynamicLighting {
            enabled: enabled != 0,
            foreground_apps_control: read_dword("ControlledByForegroundApp").is_some_and(|v| v != 0),
        })
    }
    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn read_dword(name: &str) -> Option<u32> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let key: Vec<u16> = "Software\\Microsoft\\Lighting".encode_utf16().chain([0]).collect();
    let value: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated UTF-16 that outlive the call; `data` is a u32 and
    // `size` says so; RRF_RT_REG_DWORD makes Windows refuse any other value type.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&raw mut data).cast(),
            &raw mut size,
        )
    };
    (status == 0).then_some(data)
}

/// A line for reports and the lighting probe.
#[must_use]
pub fn describe(state: Option<DynamicLighting>) -> String {
    match state {
        Some(DynamicLighting { enabled: true, foreground_apps_control }) => format!(
            "Windows Dynamic Lighting is on{}: Windows can hold the lights of a keyboard that also exposes a LampArray",
            if foreground_apps_control { ", foreground apps take control" } else { "" }
        ),
        Some(DynamicLighting { enabled: false, .. }) => "Windows Dynamic Lighting is off".into(),
        None => "Windows Dynamic Lighting: unknown".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_the_switch_never_fails_and_describes_every_state() {
        let _ = state();
        assert!(describe(Some(DynamicLighting { enabled: true, foreground_apps_control: true })).contains("is on"));
        assert_eq!(describe(Some(DynamicLighting { enabled: false, foreground_apps_control: true })), "Windows Dynamic Lighting is off");
        assert!(describe(None).contains("unknown"));
    }
}
