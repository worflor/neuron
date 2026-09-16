// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Live census tests — the ones that sample whole-process thread and handle counts.
//!
//! They live in their own test binary because a [`Census`] reads process-wide state while
//! `cargo test` runs a binary's tests on several threads at once. A mutex serializes the tests
//! written here against each other, but it cannot serialize them against unrelated tests sharing
//! the process: a neighbour spawning or joining threads between a baseline and a post sample looks
//! like drift, and — worse for the detector test below — a neighbour's threads EXITING can cancel
//! out a deliberate leak and make the detector look silent when it is working. One binary, one
//! kind of test, no neighbours. Same reasoning as `neuron-host/tests/churn.rs`.
//!
//! The pure comparison logic is unit-tested next to `assert_converges` itself.

#![cfg(windows)]

use neuron_testkit::census::{assert_converges, settle, Census, CensusTolerance};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject, INFINITE};

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

/// Proves the detector actually detects: leak threads genuinely blocked (not merely
/// forgotten-but-finished) on a never-signaled event, show `assert_converges` fails via
/// `catch_unwind`, then signal the event so the threads exit before the test ends.
#[test]
fn assert_converges_catches_a_leaked_thread() {
    let _guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let baseline = settle(Duration::from_secs(2));

    let ev: HANDLE = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    assert!(!ev.is_null(), "CreateEventW failed");
    // HANDLE (a raw pointer type) is not Send; smuggle it across the thread boundary as an
    // integer: it's an opaque OS handle, never dereferenced as memory, so this is sound.
    //
    // Leak three threads, not one: the default tolerance allows growth of 1, so a single leaked
    // thread sits exactly on the slack boundary and the detector stays silent. All three park on
    // one manual-reset event, so the single SetEvent below releases all of them.
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

    // Give the leaked threads a moment to actually reach the wait before sampling.
    std::thread::sleep(Duration::from_millis(50));
    let post = Census::now();

    let result = std::panic::catch_unwind(|| {
        assert_converges(baseline, post, CensusTolerance::default(), "leaked_thread");
    });
    assert!(result.is_err(), "assert_converges must fail on a genuinely leaked thread");

    // Release the leaked threads and wait for the census to reflect it (still holding LOCK)
    // before returning, so the next test's baseline isn't sampled mid-teardown.
    unsafe {
        SetEvent(ev);
        CloseHandle(ev);
    }
    let _ = settle(Duration::from_secs(2));
}
