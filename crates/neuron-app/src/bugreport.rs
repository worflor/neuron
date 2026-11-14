// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! SYSTEM's "report a bug": builds a report with [`neuron::report`] off the UI thread, then shows
//! its ZIP in the file browser and either opens the pre-filled issue or hands the page a prompt
//! for the person's coding agent to copy. Nothing is sent: the person attaches the ZIP themselves.

use crate::ui::{AppWindow, State};
use slint::ComponentHandle;

/// Build a report of `what` and act on it: the issue page, or the agent prompt when `for_agent`.
pub(crate) fn start(app: &AppWindow, what: String, for_agent: bool) {
    let weak = app.as_weak();
    app.global::<State>().set_status_line("writing a bug report…".into());
    let spawned = crate::worker::spawn_detached("neuron-bug-report", move || {
        let mut flight = Vec::new();
        crate::flight::dump(&mut flight);
        let devices = neuron::registry::Registry::load().map(|reg| neuron::report::connected_devices(&reg)).unwrap_or_default();
        let input = neuron::report::Input {
            what,
            version: env!("CARGO_PKG_VERSION").into(),
            devices,
            crash_log: std::fs::read_to_string(crate::flight::crash_log_path()).ok(),
            flight: Some(String::from_utf8_lossy(&flight).into_owned()),
        };
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let built = neuron::report::build(&input, &neuron::runroot::run_root(), &neuron::report::Private::current(), now, &neuron::report::skill_location());
        let _ = slint::invoke_from_event_loop(move || {
            let Some(app) = weak.upgrade() else { return };
            let st = app.global::<State>();
            match built {
                Ok(report) => {
                    reveal(&report.zip);
                    if for_agent {
                        let copied = !cfg!(test) && neuron::pocket::copy_text(&report.agent_prompt);
                        st.set_status_line(if copied {
                            "copied a prompt for your coding agent · the report's zip is open beside it".into()
                        } else {
                            format!("report written to {} · the clipboard was busy, so point your agent at that folder", report.dir.display()).into()
                        });
                    } else {
                        open(&report.issue_url);
                        st.set_status_line("report written · drag its zip into the issue that just opened".into());
                    }
                }
                Err(e) => st.set_status_line(format!("bug report not written: {e}").into()),
            }
        });
    });
    if !spawned {
        app.global::<State>().set_status_line("bug report not written: no worker thread".into());
    }
}

/// Open a web page in the default browser. Tests never open anything.
fn open(url: &str) {
    if cfg!(test) {
        return;
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
        let (verb, file) = (wide("open"), wide(url));
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call; a null window and
        // null parameters/directory are permitted by ShellExecuteW.
        unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Show a file in the file browser, selected. Tests never open anything.
fn reveal(path: &std::path::Path) {
    if cfg!(test) {
        return;
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = path.parent() {
            let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
        }
    }
}
