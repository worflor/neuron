// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! In-process resource census — the unit-level dangling-thread/handle detector.
//!
//! [`budget`](crate::budget) censuses a child process (the real shipped binary) through a Job
//! Object, for whole-app resident-footprint budgets. This module censuses the current process —
//! cheap enough for an ordinary `cargo test` — to prove that one type's construct/use/drop
//! cycle leaves no threads or handles behind. Same FFI style as `budget.rs` (`ToolHelp` for
//! thread enumeration), reusing the same `windows-sys` features this crate already enables.

#![cfg(windows)]

use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount,
};

/// A snapshot of the CURRENT process's thread and handle counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Census {
    /// Live threads owned by this process, per a `ToolHelp` thread snapshot filtered to our own
    /// pid (mirrors `budget.rs`'s `census` join, minus the job-membership step).
    pub threads: usize,
    /// Open kernel handles, per `GetProcessHandleCount` on our own pseudo-handle.
    pub handles: usize,
}

impl Census {
    /// Sample right now. Best-effort: a failed Win32 query reads as 0 for that field rather than
    /// panicking — a census is a diagnostic, and a transient query failure must not itself fail
    /// an unrelated test.
    #[must_use]
    pub fn now() -> Census {
        Census {
            threads: current_process_thread_count(),
            handles: current_process_handle_count(),
        }
    }
}

fn current_process_handle_count() -> usize {
    // GetCurrentProcess returns a constant pseudo-handle (-1) that is never closed.
    let h = unsafe { GetCurrentProcess() };
    let mut handles: u32 = 0;
    if unsafe { GetProcessHandleCount(h, &raw mut handles) } == 0 {
        0
    } else {
        handles as usize
    }
}

fn current_process_thread_count() -> usize {
    let pid = unsafe { GetCurrentProcessId() };
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return 0;
    }
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
    let mut count = 0usize;
    let mut ok = unsafe { Thread32First(snap, &raw mut entry) };
    while ok != 0 {
        if entry.th32OwnerProcessID == pid {
            count += 1;
        }
        ok = unsafe { Thread32Next(snap, &raw mut entry) };
    }
    unsafe { CloseHandle(snap) };
    count
}

/// How far a post-churn [`Census`] may drift from its baseline and still count as "converged".
#[derive(Clone, Copy, Debug)]
pub struct CensusTolerance {
    /// Max allowed GROWTH in threads (`post - baseline`). Shrinkage is never a leak — see
    /// [`assert_converges`].
    pub threads_slack: usize,
    /// Max allowed GROWTH in handles (`post - baseline`). Shrinkage is never a leak.
    pub handles_slack: usize,
}

impl Default for CensusTolerance {
    fn default() -> Self {
        // Threads: slack of 1 absorbs a thread still mid-teardown despite `settle`'s stability
        // poll, without masking a genuine per-instance leak.
        // Handles: Windows runtime caches (CRT lazy-inits, loader bookkeeping, COM) grow the
        // count a few at a time independent of anything under test; +/-8 absorbs that noise
        // while still catching a real per-cycle leak.
        CensusTolerance { threads_slack: 1, handles_slack: 8 }
    }
}

/// Assert that `post` did not grow beyond `baseline` by more than `tolerance`. Panics rather
/// than returning a `Result` so callers proving the detector itself works can wrap this in
/// `catch_unwind`.
///
/// The check is directional: only growth (`post > baseline`) fails. A `post` below baseline is
/// never a leak - some unrelated thread transiently alive at baseline may have since exited,
/// leaving the process cleaner. An absolute `|post - baseline|` check would false-fail on that
/// shrinkage, which a whole-process census in a shared test binary routinely sees.
pub fn assert_converges(baseline: Census, post: Census, tolerance: CensusTolerance, label: &str) {
    let thread_growth = post.threads.saturating_sub(baseline.threads);
    let handle_growth = post.handles.saturating_sub(baseline.handles);
    assert!(
        thread_growth <= tolerance.threads_slack,
        "{label}: threads leaked — baseline {}, post {} (growth {} > slack {})",
        baseline.threads,
        post.threads,
        thread_growth,
        tolerance.threads_slack
    );
    assert!(
        handle_growth <= tolerance.handles_slack,
        "{label}: handles leaked — baseline {}, post {} (growth {} > slack {})",
        baseline.handles,
        post.handles,
        handle_growth,
        tolerance.handles_slack
    );
}

/// Poll [`Census::now`] every 20ms until two consecutive samples come back identical, or
/// `deadline` elapses. Thread teardown is asynchronous (a joined `JoinHandle` proves the
/// closure returned, not that the kernel has finished reclaiming the thread), so sampling
/// immediately after a join/close is exactly the timing-flake this avoids. Returns the last
/// census either way; residual drift shows up honestly in `assert_converges`'s failure message.
#[must_use]
pub fn settle(deadline: Duration) -> Census {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    let start = Instant::now();
    let mut last = Census::now();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let next = Census::now();
        if next == last {
            return next;
        }
        last = next;
        if start.elapsed() >= deadline {
            return last;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The comparison itself, on hand-built samples: growth past slack fails, growth within it
    /// passes, and shrinkage is never a leak. Deterministic — it samples no process state, so it
    /// cannot be confused by whatever else the test binary is doing.
    ///
    /// The live counterpart, which leaks a real thread and watches this fire, is
    /// `tests/census_live.rs`: it needs a test binary to itself.
    #[test]
    fn growth_past_slack_fails_and_shrinkage_never_does() {
        let tol = CensusTolerance { threads_slack: 1, handles_slack: 8 };
        let census = |threads, handles| Census { threads, handles };
        let fails = |a: Census, b: Census| {
            std::panic::catch_unwind(move || assert_converges(a, b, tol, "unit")).is_err()
        };

        assert!(!fails(census(10, 100), census(11, 108)), "growth exactly at slack is allowed");
        assert!(fails(census(10, 100), census(12, 100)), "one thread past slack is a leak");
        assert!(fails(census(10, 100), census(10, 109)), "one handle past slack is a leak");
        assert!(!fails(census(10, 100), census(2, 40)), "shrinkage is not a leak");
    }
}
