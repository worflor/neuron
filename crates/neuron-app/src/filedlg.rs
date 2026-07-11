//! Native file-open dialog for the Import wizard — replaces the type-the-path stub. We drive the
//! real Windows file-open dialog via PowerShell's `System.Windows.Forms.OpenFileDialog` (a genuine
//! native Win32 common dialog), which needs NO extra crate / no new windows-sys feature. The dialog
//! is modal in its own STA process, so we run it on a worker thread and post the chosen path back to
//! the UI thread (the handler is stashed UI-thread-local so the cross-thread `invoke_from_event_loop`
//! closure stays `Send` — it carries only the plain path string).
//!
//! The alternative (raw `GetOpenFileNameW`) would need the `Win32_UI_Controls_Dialogs` windows-sys
//! feature; shelling the system dialog keeps the crate's dependency surface unchanged while still
//! presenting the standard OS file picker the user expects.

use crate::ui::AppWindow;
use slint::ComponentHandle;

type PickHandler = Box<dyn Fn(&AppWindow, String)>;

thread_local! {
    static PICK_HANDLER: std::cell::RefCell<Option<PickHandler>> = const { std::cell::RefCell::new(None) };
    static PICK_WINDOW: std::cell::RefCell<Option<slint::Weak<AppWindow>>> = const { std::cell::RefCell::new(None) };
}

/// Open a native file-open dialog filtered to Synapse exports, on a worker thread. Invokes `on_pick`
/// with the chosen path on the UI thread (empty string = cancelled / dialog unavailable).
pub fn pick_synapse_export(app: &AppWindow, on_pick: impl Fn(&AppWindow, String) + 'static) {
    PICK_HANDLER.with(|h| *h.borrow_mut() = Some(Box::new(on_pick)));
    PICK_WINDOW.with(|c| *c.borrow_mut() = Some(app.as_weak()));
    crate::worker::spawn_detached("neuron-file-dialog", move || {
        let path = open_dialog();
        let _ = slint::invoke_from_event_loop(move || finish(path));
    });
}

/// Run the stashed pick handler on the UI thread.
fn finish(path: String) {
    let weak = PICK_WINDOW.with(|c| c.borrow().clone());
    let Some(app) = weak.and_then(|w| w.upgrade()) else {
        return;
    };
    let handler = PICK_HANDLER.with(|h| h.borrow_mut().take());
    if let Some(h) = handler {
        h(&app, path);
    }
}

/// Run the native OpenFileDialog (PowerShell/WinForms) and return the selected path, or "" if the
/// user cancelled or the dialog could not be shown.
#[cfg(windows)]
fn open_dialog() -> String {
    const SCRIPT: &str = r#"
Add-Type -AssemblyName System.Windows.Forms | Out-Null
$d = New-Object System.Windows.Forms.OpenFileDialog
$d.Title = 'Select a Synapse export'
$d.Filter = 'Synapse exports (*.synapse3;*.ChromaEffects)|*.synapse3;*.ChromaEffects|All files (*.*)|*.*'
$d.CheckFileExists = $true
if ($d.ShowDialog() -eq [System.Windows.Forms.DialogResult]::OK) { [Console]::Out.Write($d.FileName) }
"#;
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
