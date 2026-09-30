// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Telling a running neuron app about an edit. Every verb that writes config calls [`reload`] once
//! it has written; the app re-reads its files on the next UI tick (see `neuron::livesync`). With no
//! app running nothing waits: the edit is on disk and the next launch reads it.

use neuron::livesync::{self, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static SKIP: AtomicBool = AtomicBool::new(false);

/// `--no-live`: never signal the app (batch scripts that signal once at the end).
pub fn set_skip(on: bool) {
    SKIP.store(on, Ordering::Relaxed);
}

/// Is a neuron app running in this session? The app owns the graceful-shutdown event; opening it
/// (without signalling) is a cheap presence test. Where that test does not exist the answer is
/// "maybe", and the caller falls back to waiting for an acknowledgement.
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // one signature with the non-Windows stub, which answers None
pub fn app_running() -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenEventW, EVENT_MODIFY_STATE};
    let name: Vec<u16> = std::ffi::OsStr::new(r"Local\WofloLabs.Neuron.Shutdown")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `name` is a live NUL-terminated UTF-16 string; a non-null handle is closed at once.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        return Some(false);
    }
    // SAFETY: `handle` was just returned by OpenEventW and is closed exactly once.
    unsafe { CloseHandle(handle) };
    Some(true)
}

#[cfg(not(windows))]
pub fn app_running() -> Option<bool> {
    None
}

/// Where a running app stands relative to this process's run root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    No,
    /// An app that published this run root is alive.
    Here,
    /// An app runs, but this root is not its own (or, with `NEURON_RUN_DIR` set, it cannot be
    /// shown to be).
    Elsewhere,
    Unknown,
}

pub fn presence() -> Presence {
    let running = app_running();
    if running == Some(false) {
        return Presence::No;
    }
    if neuron::livesync::app_on_this_root().is_some() {
        return Presence::Here;
    }
    let pinned = std::env::var_os("NEURON_RUN_DIR").is_some_and(|d| !d.is_empty());
    match (running, pinned) {
        (Some(true), true) => Presence::Elsewhere,
        (Some(true), false) => Presence::Here, // an app build that predates the published root
        (_, true) => Presence::No,
        _ => Presence::Unknown,
    }
}

/// What happened to an app notification, as the word a script can branch on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    Skipped,
    Reloaded,
    NoApp,
    /// An app is running, but on a different run root: nothing was sent.
    OtherRoot,
    Failed,
}

impl Live {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Live::Skipped => "skipped",
            Live::Reloaded => "reloaded",
            Live::NoApp => "no-app",
            Live::OtherRoot => "other-root",
            Live::Failed => "failed",
        }
    }

    /// Text appended to a human status line.
    #[must_use]
    pub fn suffix(self) -> &'static str {
        match self {
            Live::Skipped => "",
            Live::Reloaded => "  (live: the running app reloaded it)",
            Live::NoApp => "  (no app picked it up: it takes effect when the app starts, or `neuron reload`)",
            Live::OtherRoot => "  (an app is running on another run root; it was not signalled)",
            Live::Failed => "  (could not signal the app; it will read this on its next start)",
        }
    }
}

/// Send `cmd` to a running app and wait briefly for it to be claimed.
pub fn notify(cmd: &Command) -> Live {
    if SKIP.load(Ordering::Relaxed) {
        return Live::Skipped;
    }
    match presence() {
        Presence::No => return Live::NoApp,
        Presence::Elsewhere => return Live::OtherRoot,
        Presence::Here | Presence::Unknown => {}
    }
    match livesync::notify_app(cmd, Duration::from_millis(2500)) {
        Ok(true) => Live::Reloaded,
        Ok(false) => Live::NoApp,
        Err(_) => Live::Failed,
    }
}

/// Signal a config reload.
pub fn reload() -> Live {
    notify(&Command::Reload)
}

/// Finish a config-writing verb: signal the app, then print `value` (with a `live` field) as JSON
/// or `line` (with the live status) as text.
pub fn finish(mut value: serde_json::Value, line: impl AsRef<str>) {
    let live = reload();
    if let Some(o) = value.as_object_mut() {
        o.insert("live".into(), live.word().into());
    }
    crate::out::done(value, format!("{}{}", line.as_ref(), live.suffix()));
}
