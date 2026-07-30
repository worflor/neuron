// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Focused-window detection — the input for app-aware profile switching. Knowing which
//! executable is in the foreground lets the daemon auto-apply a profile (game profile when a
//! game has focus, a calm one for the browser, ...). Read-only OS query, no hooks.

/// Last resolved `(hwnd, pid) -> exe name`, memoized.
///
/// `foreground_app` is called on the LIVE DISPATCH THREAD (via `Context::capture`, for every macro
/// press that needs context), and its expensive half is the `OpenProcess` +
/// `QueryFullProcessImageNameW` round trip — measured as the bulk of a ~380µs context capture. The
/// cheap half (`GetForegroundWindow` + `GetWindowThreadProcessId`) is enough to tell whether the
/// answer can be reused, so repeat presses in the same window skip the round trip entirely.
///
/// ONE slot, deliberately: the foreground window is singular by definition, so a slot per window
/// would be storage for a question nobody asks. A miss simply costs what the whole call used to.
///
/// Keyed on the `(hwnd, pid)` PAIR, not either alone. Window handles and process ids are both
/// recycled after their owner dies, but for a stale entry to be wrongly reused BOTH would have to be
/// reissued together to a different executable. Cheap insurance against the one failure this memo
/// could otherwise introduce — an app-scoped rule matching on a stale name.
#[cfg(windows)]
static FOREGROUND_MEMO: std::sync::Mutex<Option<(isize, u32, String)>> =
    std::sync::Mutex::new(None);

/// The executable name of the currently-focused window, lowercased (e.g. "valorant.exe").
/// None if there's no foreground window or it can't be queried.
///
/// Memoized on the focused window — see [`FOREGROUND_MEMO`]. Callers get the same answer they always
/// did; they just usually get it without a process-query syscall.
#[cfg(windows)]
pub fn foreground_app() -> Option<String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid == 0 {
            return None;
        }
        let key = (hwnd as isize, pid);
        // A leaf lock — nothing else is acquired while it is held, so it cannot deadlock against the
        // dispatch or macro-host locks the callers may already hold.
        if let Some((h, p, name)) = FOREGROUND_MEMO
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if (*h, *p) == key {
                return Some(name.clone());
            }
        }
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        let name = path.rsplit(['\\', '/']).next().map(|s| s.to_lowercase())?;
        // Memoize only a SUCCESSFUL resolve. Caching a failure would mean a window we momentarily
        // could not query stayed unknown for as long as it kept focus.
        *FOREGROUND_MEMO.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((key.0, key.1, name.clone()));
        Some(name)
    }
}

/// Forget the memoized foreground app, so the next [`foreground_app`] re-queries the OS.
///
/// Exists for tests, which must not inherit whatever window happened to be focused during an earlier
/// test in the same process — and as the escape hatch if a caller ever needs a guaranteed-fresh read.
pub fn forget_foreground_memo() {
    #[cfg(windows)]
    {
        *FOREGROUND_MEMO.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

#[cfg(not(windows))]
pub fn foreground_app() -> Option<String> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    /// The memo must not change the ANSWER, only the cost. Two back-to-back calls have to agree —
    /// and the second is the one served from the memo, so this is what pins that the fast path
    /// returns the same value the slow path computed rather than something stale or empty.
    #[test]
    fn a_memoized_read_agrees_with_the_fresh_one() {
        super::forget_foreground_memo();
        let fresh = super::foreground_app();
        let memoized = super::foreground_app();
        assert_eq!(fresh, memoized, "the memo returned a different app than the live query");
    }

    /// Clearing the memo must leave the function working (not stuck on `None`), since a cleared memo
    /// is the state every process starts in.
    #[test]
    fn clearing_the_memo_forces_a_working_re_query() {
        let first = super::foreground_app();
        super::forget_foreground_memo();
        let after = super::foreground_app();
        assert_eq!(
            first.is_some(),
            after.is_some(),
            "a cleared memo changed whether the foreground app could be resolved at all"
        );
    }
}
