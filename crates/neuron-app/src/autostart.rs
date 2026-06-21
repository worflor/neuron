//! Start-with-Windows — a single HKCU\...\Run registry value pointing at this exe. No installer,
//! no service: the leanest possible autostart, set/cleared via `reg.exe` (no extra crate). Reading
//! state is a `reg query`. Reversible and inspectable, true to the motto.

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE: &str = "Neuron";

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Enable or disable autostart. Returns a status line.
#[cfg(windows)]
pub fn set(enabled: bool) -> String {
    if enabled {
        let exe = exe_path();
        let arg = format!("\"{exe}\" --tray");
        let out = std::process::Command::new("reg")
            .args([
                "add", RUN_KEY, "/v", VALUE, "/t", "REG_SZ", "/d", &arg, "/f",
            ])
            .output();
        match out {
            Ok(o) if o.status.success() => "start-with-Windows enabled".into(),
            Ok(_) => "failed to set autostart".into(),
            Err(e) => format!("autostart error: {e}"),
        }
    } else {
        let out = std::process::Command::new("reg")
            .args(["delete", RUN_KEY, "/v", VALUE, "/f"])
            .output();
        match out {
            Ok(_) => "start-with-Windows disabled".into(),
            Err(e) => format!("autostart error: {e}"),
        }
    }
}

/// Whether the autostart value is present.
#[cfg(windows)]
pub fn is_enabled() -> bool {
    std::process::Command::new("reg")
        .args(["query", RUN_KEY, "/v", VALUE])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
pub fn set(_enabled: bool) -> String {
    "autostart: Windows-only".into()
}

#[cfg(not(windows))]
pub fn is_enabled() -> bool {
    false
}
