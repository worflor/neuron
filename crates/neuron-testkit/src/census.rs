// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! In-process resource census — the unit-level dangling-thread/handle detector.
//!
//! [`budget`](crate::budget) censuses a child process (the real shipped binary) through a Job
//! Object, for whole-app resident-footprint budgets. This module censuses the current process —
//! cheap enough for an ordinary `cargo test` — to prove that one type's construct/use/drop
//! cycle leaves no threads or handles behind. Same FFI style as `budget.rs` (ToolHelp for
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
    /// Live threads owned by this process, per a ToolHelp thread snapshot filtered to our own
    /// pid (mirrors `budget.rs`'s `census` join, minus the job-membership step).
    pub threads: usize,
    /// Open kernel handles, per `GetProcessHandleCount` on our own pseudo-handle.
    pub handles: usize,
}

impl Census {
    /// Sample right now. Best-effort: a failed Win32 query reads as 0 for that field rather than
    /// panicking — a census is a diagnostic, and a transient query failure must not itself fail
    /// an unrelated test.
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
    if unsafe { GetProcessHandleCount(h, &mut handles) } == 0 {
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
    let mut ok = unsafe { Thread32First(snap, &mut entry) };
    while ok != 0 {
        if entry.th32OwnerProcessID == pid {
            count += 1;
        }
        ok = unsafe { Thread32Next(snap, &mut entry) };
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
    use std::sync::{Mutex, PoisonError};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject, INFINITE};

    // `cargo test` runs a binary's tests on multiple threads, but a Census reads whole-process
    // state, so racing tests would see each other's threads/handles as unexplained drift.
    // Serializes only the tests in this module; see `neuron-host/tests/churn.rs` for the same
    // pattern applied to a whole dedicated test binary.
    static LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn spawn_and_join_threads_converges() {
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let baseline = settle(Duration::from_secs(2));
        let handles: Vec<_> = (0..16)
            .map(|_| std::thread::spawn(|| std::thread::sleep(Duration::from_millis(5))))
            .collect();
        for h in handles {
            h.join().expect("join spawned thread");
        }
        let post = settle(Duration::from_secs(2));
        assert_converges(baseline, post, CensusTolerance::default(), "spawn_and_join_threads");
    }

    #[test]
    fn open_and_close_events_converges() {
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let baseline = settle(Duration::from_secs(2));
        for _ in 0..32 {
            let h: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            assert!(!h.is_null(), "CreateEventW failed");
            unsafe { CloseHandle(h) };
        }
        let post = settle(Duration::from_secs(2));
        assert_converges(baseline, post, CensusTolerance::default(), "open_and_close_events");
    }

    /// Proves the detector actually detects: leak a thread genuinely blocked (not merely
    /// forgotten-but-finished) on a never-signaled event, show `assert_converges` fails via
    /// `catch_unwind`, then signal the event so the thread exits before the test ends.
    #[test]
    fn assert_converges_catches_a_leaked_thread() {
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let baseline = settle(Duration::from_secs(2));

        let ev: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!ev.is_null(), "CreateEventW failed");
        // HANDLE (a raw pointer type) is not Send; smuggle it across the thread boundary as an
        // integer: it's an opaque OS handle, never dereferenced as memory, so this is sound.
        //
        // Leak three threads, not one: the default tolerance allows growth of 1, so a single
        // leaked thread sits exactly on the slack boundary and the detector stays silent. All
        // three park on one manual-reset event, so the single SetEvent below releases all of them.
        let ev_addr = ev as usize;
        for _ in 0..3 {
            let leaker = std::thread::spawn(move || {
                let ev = ev_addr as HANDLE;
                unsafe { WaitForSingleObject(ev, INFINITE) };
            });
            // No JoinHandle survives past this point — a genuine dangling thread, not a
            // detached-but-already-finished one.
            std::mem::forget(leaker);
        }

        // Give the leaked thread a moment to actually reach the wait before sampling it.
        std::thread::sleep(Duration::from_millis(50));
        let post = Census::now();

        let result = std::panic::catch_unwind(|| {
            assert_converges(baseline, post, CensusTolerance::default(), "leaked_thread");
        });
        assert!(result.is_err(), "assert_converges must fail on a genuinely leaked thread");

        // Release the leaked thread and wait for the census to reflect it (still holding LOCK)
        // before returning, so the next test's baseline isn't sampled mid-teardown.
        unsafe {
            SetEvent(ev);
            CloseHandle(ev);
        }
        let _ = settle(Duration::from_secs(2));
    }
}
