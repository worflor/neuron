// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The macro Context API — the world a macro reacts to.
//!
//! A context-aware macro asks "where am I?" before it acts: which app is focused, what's the
//! window title, what folder is Explorer showing, what's on the clipboard, what text is
//! selected, and which window had focus *before* this macro fired (so it can restore it after a
//! sub-second in-game action). This struct is the `ctx` the `neuron-macro` prelude exposes.
//!
//! The fields are a SNAPSHOT (captured once when a trigger fires) so a macro sees a consistent
//! world. The `capture()` constructor reads the live OS state.
//!
//! Windows implementations are REAL here (the spine handed over stubs; the macro agent wired the
//! actual Win32 calls): `GetForegroundWindow`/`GetWindowTextW` for the title, the clipboard via
//! `OpenClipboard`+`CF_UNICODETEXT`, the foreground HWND captured as an `isize` and restored with
//! `SetForegroundWindow`. The Explorer-path and selection probes are best-effort and degrade to
//! `None` when not resolvable (Explorer-path needs the shell-windows COM walk; selection needs a
//! guarded clipboard probe) — never panicking. On non-Windows everything is `None`.

/// An opaque OS window handle, captured so focus can be restored later. Stored as a pointer-
//  sized integer to stay `Copy`/serde-free and platform-neutral at this layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowHandle(pub isize);

impl WindowHandle {
    /// Is this a real (non-null) handle?
    #[must_use]
    pub fn is_some(&self) -> bool {
        self.0 != 0
    }
}

/// A captured snapshot of the environment a macro runs against.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Foreground executable name, lowercased (e.g. "explorer.exe"). `None` if unknown.
    pub app: Option<String>,
    /// Foreground window title text. `None` if unknown.
    pub window_title: Option<String>,
    /// The working directory / Explorer path the foreground window represents (a shell folder
    /// for Explorer, the cwd for a terminal). `None` if not resolvable.
    pub cwd: Option<std::path::PathBuf>,
    /// Current clipboard text (Unicode). `None` if empty / non-text.
    pub clipboard: Option<String>,
    /// The currently-selected text, if obtainable (UI Automation / clipboard probe). `None`
    /// if no selection or not resolvable.
    pub selection: Option<String>,
    /// The window that had focus when the trigger fired — restore it after the macro
    /// (`restore_foreground`). Default = null handle.
    pub prev_window: WindowHandle,
}

impl Context {
    /// Capture the live OS environment into a snapshot. On Windows this queries
    /// `GetForegroundWindow` (handle + title), the clipboard (`CF_UNICODETEXT`), and the
    /// Explorer shell-folder path; every probe is read-only and falls back to `None` rather
    /// than failing, so `capture()` always yields a valid context.
    ///
    /// Each probe runs exactly ONCE and the results are threaded into whatever else needs them.
    /// That is not tidiness — it is measured: this runs on the dispatch thread, which services no
    /// other input while it does, and the naive version called `foreground_app` (an `OpenProcess` +
    /// `QueryFullProcessImageNameW` round trip) and `foreground_window_title` twice each, because
    /// `explorer_path` re-derived both for itself. `latency::CTX_CAPTURE` is what shows the
    /// difference; `latency::CTX_CLIPBOARD` is what proved the clipboard was NOT the expensive part
    /// (~78µs of ~734µs), which is what pointed here instead.
    #[must_use]
    pub fn capture() -> Self {
        let prev_window = foreground_window();
        let app = foreground_app();
        let window_title = foreground_window_title();
        Context {
            cwd: explorer_path(app.as_deref(), window_title.as_deref()),
            clipboard: clipboard_text(),
            selection: selection_text(),
            app,
            window_title,
            prev_window,
        }
    }

    /// Build a Context directly from explicit fields. Used by the dry-run interpreter and the
    /// verify-gate fuzzer to feed a macro adversarial / synthetic worlds without touching the OS.
    #[must_use]
    pub fn synthetic(
        app: Option<String>,
        window_title: Option<String>,
        cwd: Option<std::path::PathBuf>,
        clipboard: Option<String>,
        selection: Option<String>,
    ) -> Self {
        Context {
            app,
            window_title,
            cwd,
            clipboard,
            selection,
            prev_window: WindowHandle::default(),
        }
    }

    // --- prelude accessors (the `ctx.*()` surface macros call) ---------------------------

    /// Foreground app exe name (lowercased).
    #[must_use]
    pub fn app(&self) -> Option<&str> {
        self.app.as_deref()
    }
    /// Foreground window title.
    #[must_use]
    pub fn title(&self) -> Option<&str> {
        self.window_title.as_deref()
    }
    /// The Explorer/terminal path of the foreground window.
    #[must_use]
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }
    /// Clipboard text.
    #[must_use]
    pub fn clipboard(&self) -> Option<&str> {
        self.clipboard.as_deref()
    }
    /// Selected text.
    #[must_use]
    pub fn selection(&self) -> Option<&str> {
        self.selection.as_deref()
    }
    /// The window that had focus before the macro fired.
    #[must_use]
    pub fn prev_window(&self) -> WindowHandle {
        self.prev_window
    }

    /// Restore focus to the window that was foreground when the trigger fired — the last act of
    /// a quick in-game macro ("focus Discord, type, Alt-Tab back"). Returns `true` if the call
    /// succeeded. No-op (returns `false`) on a null handle or non-Windows.
    #[must_use]
    pub fn restore_foreground(&self) -> bool {
        restore_foreground(self.prev_window)
    }
}

// --- OS probes ----------------------------------------------------------------------------
//
// Each returns the typed `Option` the corresponding field needs. Read-only OS queries; every
// one degrades to `None`/`false` on failure so `capture()`/`restore_foreground()` never panic.

/// Foreground executable name — delegates to the existing, working implementation.
fn foreground_app() -> Option<String> {
    crate::app::foreground_app()
}

#[cfg(windows)]
fn foreground_window_title() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        // 512 wide chars is ample for any title bar; GetWindowTextW truncates+terminates.
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

#[cfg(windows)]
fn explorer_path(app: Option<&str>, title: Option<&str>) -> Option<std::path::PathBuf> {
    // Best-effort: an Explorer window's title is the folder *name* (or, with "show full path in
    // title bar" enabled, the full path). We only return a path when the title resolves to an
    // existing directory — anything ambiguous degrades to None (the COM IShellWindows walk is the
    // full solution; this guarded heuristic avoids a flaky COM dependency in the hot path).
    //
    // `app` and `title` are PASSED IN, not re-probed: the caller already has both, and re-deriving
    // them here meant every context capture paid for two extra `OpenProcess`/image-name round trips
    // (see `Context::capture`'s note).
    if app? != "explorer.exe" {
        return None;
    }
    let p = std::path::PathBuf::from(title?.trim());
    // `is_dir` is a filesystem stat, so it is reached only after the cheap checks have already
    // established this is an Explorer window with an absolute-looking title.
    if p.is_absolute() && p.is_dir() {
        Some(p)
    } else {
        None
    }
}

#[cfg(windows)]
fn clipboard_text() -> Option<String> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard,
    };
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    // CF_UNICODETEXT = 13.
    const CF_UNICODETEXT: u32 = 13;
    // Measured separately from the rest of `capture` (see `latency::CTX_CLIPBOARD`): this is the only
    // part that waits on a resource other processes contend for, so it is the only part whose cost is
    // not ours to simply make smaller.
    let _t = crate::latency::start(&crate::latency::CTX_CLIPBOARD);
    // Serialize this process's clipboard window against every other clipboard user — see
    // `crate::clipboard` for why there is exactly one process-wide lock.
    let _guard = crate::clipboard::clipboard_guard();
    unsafe {
        // OpenClipboard(NULL) associates with the current task; can fail if another process holds
        // it — degrade to None rather than block.
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return None;
        }
        let h: HANDLE = GetClipboardData(CF_UNICODETEXT);
        if h.is_null() {
            CloseClipboard();
            return None;
        }
        let ptr = GlobalLock(h) as *const u16;
        if ptr.is_null() {
            CloseClipboard();
            return None;
        }
        // The block is a NUL-terminated UTF-16 string; find its length (cap to stay bounded).
        let mut len = 0usize;
        while len < 1 << 20 && *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let s = String::from_utf16_lossy(slice);
        GlobalUnlock(h);
        CloseClipboard();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

#[cfg(windows)]
fn selection_text() -> Option<String> {
    // A reliable selection probe means synthesizing Ctrl+C and reading the clipboard back — but
    // that MUTATES the user's clipboard and only works in apps that honor copy, so it is unsafe to
    // do implicitly inside a read-only `capture()`. We leave selection as None here; a macro that
    // genuinely wants the selection calls the prelude's explicit (clipboard-clobbering) helper.
    None
}

#[cfg(windows)]
fn foreground_window() -> WindowHandle {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    unsafe { WindowHandle(GetForegroundWindow() as isize) }
}

#[cfg(windows)]
fn restore_foreground(h: WindowHandle) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    if !h.is_some() {
        return false;
    }
    unsafe { SetForegroundWindow(h.0 as _) != 0 }
}

// --- non-Windows stubs ---------------------------------------------------------------------

#[cfg(not(windows))]
fn foreground_window_title() -> Option<String> {
    None
}
#[cfg(not(windows))]
fn explorer_path(_app: Option<&str>, _title: Option<&str>) -> Option<std::path::PathBuf> {
    None
}
#[cfg(not(windows))]
fn clipboard_text() -> Option<String> {
    None
}
#[cfg(not(windows))]
fn selection_text() -> Option<String> {
    None
}
#[cfg(not(windows))]
fn foreground_window() -> WindowHandle {
    WindowHandle::default()
}
#[cfg(not(windows))]
fn restore_foreground(_h: WindowHandle) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_context_is_valid() {
        let c = Context::default();
        assert!(c.app().is_none());
        assert!(c.clipboard().is_none());
        assert!(!c.prev_window().is_some());
    }

    #[test]
    fn capture_does_not_panic() {
        // Every probe is a read-only OS query that degrades to None — capture must always succeed.
        let _ = Context::capture();
    }

    #[test]
    fn synthetic_round_trips_fields() {
        let c = Context::synthetic(
            Some("discord.exe".into()),
            Some("general — server".into()),
            Some(std::path::PathBuf::from("C:/tmp")),
            Some("clip".into()),
            Some("sel".into()),
        );
        assert_eq!(c.app(), Some("discord.exe"));
        assert_eq!(c.title(), Some("general — server"));
        assert_eq!(c.cwd(), Some(std::path::Path::new("C:/tmp")));
        assert_eq!(c.clipboard(), Some("clip"));
        assert_eq!(c.selection(), Some("sel"));
        // synthetic worlds carry a null prev_window (no real focus to restore).
        assert!(!c.prev_window().is_some());
    }

    #[test]
    fn restore_on_null_handle_is_false() {
        let c = Context::default();
        assert!(!c.restore_foreground());
    }
}
