// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! In-process resource census — the unit-level dangling-thread/handle detector.
//!
//! [`budget`](crate::budget) censuses a CHILD process (the real shipped binary) through a Job
//! Object, for whole-app resident-footprint budgets. This module censuses the CURRENT process —
//! cheap enough to run inside an ordinary `cargo test`, for proving that one TYPE's construct/use/
//! drop cycle leaves no threads or handles behind. Same FFI style as `budget.rs` (ToolHelp for
//! thread enumeration), reusing the SAME `windows-sys` features this crate already enables
//! (`Win32_Foundation`, `Win32_System_Threading`, `Win32_System_Diagnostics_ToolHelp`) — no new
//! feature flags needed.

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
        // Threads: the exact failure mode this module hunts is a detached leaker, so the default
        // slack is tiny — 1 — enough to absorb a single thread still mid-teardown despite
        // `settle`'s stability poll, without masking a genuine leak (which shows up as +1 PER
        // churned instance, not a one-time wobble).
        //
        // Handles: Windows runtime caches (CRT lazy-inits, loader bookkeeping, COM) can grow the
        // count a few at a time independent of anything under test — exact equality is documented
        // as flaky. ±8 is generous enough to absorb that noise while still catching a real
        // per-cycle leak: 50 churn cycles leaking even 1 handle each overshoots 8 by ~6x.
        CensusTolerance { threads_slack: 1, handles_slack: 8 }
    }
}

/// Assert that `post` did not GROW beyond `baseline` by more than `tolerance`. Panics (does not
/// return a `Result`) so callers proving the detector itself works can wrap this in `catch_unwind`.
///
/// The check is DIRECTIONAL: a leak is GROWTH (`post > baseline`), and only growth fails. A `post`
/// BELOW `baseline` is never a leak — it means the baseline was captured while some unrelated thread
/// (a prior test's still-winding-down worker that `settle`'s stability poll caught mid-teardown, or
/// runtime lazy-init) was transiently alive and has since exited, leaving the process CLEANER than
/// at baseline. An absolute `|post - baseline|` check would false-fail on exactly that shrinkage,
/// which is what a whole-process census running in a shared test binary routinely sees.
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

/// Poll [`Census::now`] every 20ms until two CONSECUTIVE samples come back identical, or
/// `deadline` elapses — whichever is first. Thread teardown is asynchronous (a joined
/// `JoinHandle` proves the closure returned, not that the kernel has finished reclaiming the
/// thread), so sampling immediately after a join/close is exactly the timing-flake this exists
/// to avoid. Returns the last-observed census either way; a caller that still sees drift at the
/// deadline gets that drift reflected honestly in `assert_converges`'s failure message rather
/// than this function hiding it behind a bool.
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

    // `cargo test` runs a binary's tests on multiple threads by default, but a Census reads
    // whole-PROCESS state — two of these tests racing would each see the other's threads/handles
    // as unexplained drift. Serialize just the tests IN THIS MODULE against each other; this
    // does not (and cannot, from in here) protect against unrelated tests elsewhere in the same
    // `neuron-testkit` unit-test binary spawning threads/handles concurrently — see the module's
    // callers (`neuron-host/tests/churn.rs`) for the same lock pattern applied to a whole,
    // dedicated test binary, which does not share this exposure.
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

    /// Proves the detector actually DETECTS: leak a thread genuinely blocked (not merely
    /// forgotten-but-finished) on a never-signaled event, show `assert_converges` fails via
    /// `catch_unwind`, then signal the event so the thread actually exits before the test ends.
    #[test]
    fn assert_converges_catches_a_leaked_thread() {
        let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        let baseline = settle(Duration::from_secs(2));

        let ev: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        assert!(!ev.is_null(), "CreateEventW failed");
        // HANDLE (a raw pointer type) is not Send; smuggle it across the thread boundary as an
        // integer — it's an opaque OS handle, never dereferenced as memory, so this is sound.
        //
        // Leak THREE threads, not one: the default tolerance deliberately allows a growth of 1
        // (one thread mid-teardown is normal wobble, not a leak), so a single leaked thread sits
        // exactly ON the slack boundary and the detector — correctly — stays silent. The self-test
        // must leak unambiguously PAST the slack to prove detection fires. All three park on the
        // one manual-reset event, so the single SetEvent below still releases every one of them.
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

        // Clean up: release the leaked thread so it actually exits, then wait for the census to
        // reflect that (still holding `LOCK`) before returning — otherwise the next test in this
        // module could sample ITS baseline while this thread is still mid-teardown and inherit a
        // spurious +1 that then vanishes from under it.
        unsafe {
            SetEvent(ev);
            CloseHandle(ev);
        }
        let _ = settle(Duration::from_secs(2));
    }
}
