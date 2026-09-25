// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Native file-open dialog for the Import wizard — replaces the type-the-path stub. We drive the
//! real Windows file-open dialog via PowerShell's `System.Windows.Forms.OpenFileDialog` (a genuine
//! native Win32 common dialog), which needs NO extra crate / no new windows-sys feature. The dialog
//! is modal in its own STA process, so we run it on a worker thread and post the chosen path back to
//! the UI thread (handlers are stashed by request id in UI-thread-local storage, so the cross-thread
//! `invoke_from_event_loop` closure stays `Send` — it carries only the id and plain path string).
//!
//! The alternative (raw `GetOpenFileNameW`) would need the `Win32_UI_Controls_Dialogs` windows-sys
//! feature; shelling the system dialog keeps the crate's dependency surface unchanged while still
//! presenting the standard OS file picker the user expects.

use crate::ui::AppWindow;
use slint::ComponentHandle;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

type PickHandler = Box<dyn FnOnce(String)>;

static NEXT_PICK_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static PICK_HANDLERS: std::cell::RefCell<HashMap<u64, PickHandler>> = std::cell::RefCell::new(HashMap::new());
}

/// Open a native file-open dialog filtered to Synapse exports, on a worker thread. Invokes `on_pick`
/// with the chosen path on the UI thread (empty string = cancelled / dialog unavailable).
pub fn pick_synapse_export(app: &AppWindow, on_pick: impl Fn(&AppWindow, String) + 'static) {
    let id = NEXT_PICK_ID.fetch_add(1, Ordering::Relaxed);
    let weak = app.as_weak();
    PICK_HANDLERS.with(|handlers| {
        handlers.borrow_mut().insert(
            id,
            Box::new(move |path| {
                if let Some(app) = weak.upgrade() {
                    on_pick(&app, path);
                }
            }),
        );
    });
    if !crate::worker::spawn_detached("neuron-file-dialog", move || {
        let path = open_dialog();
        let _ = slint::invoke_from_event_loop(move || finish(id, path));
    }) {
        finish(id, String::new());
    }
}

/// Complete only the request whose dialog returned; callers run this on the UI thread.
fn finish(id: u64, path: String) {
    let handler = PICK_HANDLERS.with(|handlers| {
        let mut pending = handlers.borrow_mut();
        take_request(&mut pending, id)
    });
    if let Some(handler) = handler {
        handler(path);
    }
}

fn take_request<T>(pending: &mut HashMap<u64, T>, id: u64) -> Option<T> {
    pending.remove(&id)
}

/// Run the native `OpenFileDialog` (PowerShell/WinForms) and return the selected path, or "" if the
/// user cancelled or the dialog could not be shown.
#[cfg(windows)]
fn open_dialog() -> String {
    const SCRIPT: &str = r"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Title = 'Select a Synapse export'
$d.Filter = 'Synapse exports (*.synapse3;*.ChromaEffects)|*.synapse3;*.ChromaEffects|All files (*.*)|*.*'
$d.CheckFileExists = $true
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Out.Write($d.FileName) }
";
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-STA", "-Command", SCRIPT])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        Err(_) => String::new(),
    }
}

#[cfg(not(windows))]
fn open_dialog() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn overlapping_dialogs_deliver_paths_to_their_own_handlers() {
        let mut pending: HashMap<u64, PickHandler> = HashMap::new();
        let results = Arc::new(Mutex::new(Vec::new()));
        for id in [11, 22] {
            let results = results.clone();
            pending.insert(
                id,
                Box::new(move |path| results.lock().unwrap().push((id, path))),
            );
        }

        let second = take_request(&mut pending, 22).unwrap();
        second("second.synapse3".into());
        let first = take_request(&mut pending, 11).unwrap();
        first("first.synapse3".into());
        assert!(take_request(&mut pending, 11).is_none());
        assert_eq!(
            *results.lock().unwrap(),
            vec![
                (22, "second.synapse3".to_string()),
                (11, "first.synapse3".to_string()),
            ]
        );
        assert!(pending.is_empty());
    }
}
