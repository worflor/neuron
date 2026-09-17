// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! TIMING POSTURE — precise waits, and asking the scheduler to take input seriously.
//!
//! Two primitives, both "make the OS treat us correctly" rather than "do less work", and both with a
//! real Windows implementation behind a signature that compiles and behaves sensibly everywhere.
//!
//! ## 1. [`sleep_precise`] — the macro-step pause, and why it needs no Win32 of its own
//!
//! Windows' scheduler tick is ~15.6ms by default, so the classic `Sleep(2)` waits until the next
//! tick rather than 2ms — which would mean every macro inter-step delay under ~16ms silently became
//! ~16ms, and a macro authored with 2ms spacing ran many times slower than written.
//!
//! **This was measured, and it is not what happens.** Rust 1.75+ implements `std::thread::sleep` on
//! Windows with a high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`, Windows 10
//! 1803+) and falls back to `Sleep` only on older systems. A hand-rolled timer here was written,
//! measured against `std::thread::sleep` side by side on the same machine (see the `latency_probe`
//! example, CASE 5), found to be **indistinguishable** — 1ms request: 2.05ms vs 2.18ms mean — and
//! deleted. Duplicating the standard library's `unsafe` FFI to gain nothing is a liability, not
//! a fix.
//!
//! So this function is a thin, INSTRUMENTED wrapper: it delegates, and records the overshoot in
//! [`crate::latency::SLEEP_ERROR`] so macro timing fidelity is a number we watch rather than a
//! property we assume. If a future toolchain, or an older Windows, ever regresses these pauses to
//! tick granularity, that histogram is where it shows up.
//!
//! What this deliberately does NOT do is call `timeBeginPeriod(1)`. That raises the timer resolution
//! for the WHOLE SYSTEM, costing every other process power and interrupt overhead — precisely the
//! invasive, machine-wide behaviour this project exists as the alternative to. A per-process gain is
//! never worth a system-wide tax.
//!
//! ## 2. [`boost_input_thread`] — because a fullscreen game outranks us
//!
//! A thread at default priority competes with every other normal thread on the box. When a game has
//! the foreground (its own threads boosted, all cores busy), the input pump can wait tens of
//! milliseconds merely to be *scheduled* — which the user experiences as "my macro fired late",
//! intermittently, exactly when it matters most. Microsoft's own guidance is to run a process's input
//! thread at `ABOVE_NORMAL` or `HIGHEST` for responsiveness.
//!
//! We use `ABOVE_NORMAL`, deliberately, and only on the small number of threads that service input:
//!
//! * It is enough to win against normal-priority work, which is the actual problem.
//! * It stays well below the real-time band (16-31), which would let a bug starve the system.
//! * `HIGHEST`/`TIME_CRITICAL` would put us above most of the foreground app's own threads — a
//!   utility that makes the user's game stutter to shave a millisecond off its own latency has made
//!   the wrong trade. These threads block in a wait almost all of the time; they need to be scheduled
//!   *promptly*, not *first*.
//!
//! The same reasoning bounds WHICH threads get it. Boosted: the dispatch pump, both low-level hook
//! pumps, and the HID readers — all short, bounded, and ours. NOT boosted: the macro runner pool,
//! which executes arbitrary user macro bodies that can hold keys and sleep for up to a minute per
//! step. Measurement backed that up (a macro's time-to-first-output was identical either way, because
//! `SendInput` dominates it), so boosting them would have handed elevated scheduling to unbounded
//! user code in exchange for nothing. See `crate::macros::runner::worker_loop`.
//!
//! Off Windows this is a no-op ON PURPOSE, not an unfinished port: raising scheduling priority on
//! Linux/macOS means `nice` (needs privilege to lower the number) or a real-time policy
//! (`SCHED_RR`, needs `CAP_SYS_NICE`). A background utility that demands either at startup — or
//! fails noisily when refused — is invasive. See [`boost_input_thread`]'s doc for what to reach for
//! if that ever changes.

use std::time::{Duration, Instant};

/// Pause for `d` as part of a macro's timeline, and record how far it overshot.
///
/// The wait itself is `std::thread::sleep`, which is already the right primitive on every platform we
/// target — a high-resolution waitable timer on Windows 10 1803+, `nanosleep` elsewhere (see the
/// module doc for the measurement that settled this). The value this adds is the
/// [`crate::latency::SLEEP_ERROR`] sample: macro step timing is the one place the app deliberately
/// waits on the user's behalf, so its accuracy is a product property, and product properties should
/// be measured continuously rather than trusted.
///
/// Every macro inter-step delay and every timed hold goes through here, so the histogram covers the
/// real distribution on the user's real machine — including the tail, where an overshoot actually
/// gets noticed.
pub fn sleep_precise(d: Duration) {
    if d.is_zero() {
        return;
    }
    let start = std::time::Instant::now();
    std::thread::sleep(d);
    // Only the OVERSHOOT is interesting — a sleep cannot legitimately return early, so a signed
    // error would need a second histogram to say nothing extra.
    crate::latency::SLEEP_ERROR.record(start.elapsed().saturating_sub(d));
}

/// Ask the scheduler to run the CURRENT thread promptly because it services input.
///
/// Call once, from the thread itself, right after it starts. Returns whether the priority actually
/// changed, so a caller can record the truth rather than assume it (the flight log does).
///
/// Windows: `THREAD_PRIORITY_ABOVE_NORMAL` — enough to beat ordinary work, deliberately not the
/// real-time band. See the module doc for why not higher. Elsewhere: a no-op returning `false`; the
/// honest port needs `libc::setpriority` (privileged) or `sched_setscheduler` with `SCHED_RR`
/// (`CAP_SYS_NICE`), neither of which a background utility should take without being asked.
///
/// `NEURON_INPUT_PRIORITY=default` opts out, leaving every input thread at normal priority. That is
/// both a field escape hatch (so a suspected scheduling interaction can be ruled out live, without a
/// rebuild — the same shape as `NEURON_PUMP=poll`) and what makes the change TESTABLE: the
/// `pump_latency` example flips it to measure `inject_hop` boosted vs unboosted under load, which is
/// the only way to show the boost does anything rather than assert that it should.
pub fn boost_input_thread() -> bool {
    if !priority_enabled() {
        return false;
    }
    let ok = boost_impl();
    if ok {
        // Count each THREAD once, not each call. Several call sites can legitimately run more than
        // once — the HID readers are respawned on every device replug, and a pump can be reopened —
        // and counting calls would drift upward over a long session until the posture line claimed
        // more boosted threads than the process has. A thread-local latch makes the number mean what
        // its name says.
        BOOSTED_HERE.with(|first| {
            if !first.replace(true) {
                BOOSTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    ok
}

thread_local! {
    /// Has THIS thread already been counted in [`BOOSTED`]? See [`boost_input_thread`].
    static BOOSTED_HERE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How many distinct threads have successfully raised their priority.
///
/// Reported next to the latency table, because the posture CHANGES the numbers: `inject_hop` measured
/// 40µs boosted and 7.3ms unboosted under above-normal contention. A latency report that does not say
/// which of those configurations produced it is missing the single most important piece of its own
/// context. It is also how the boost can be verified from outside without elevation — reading another
/// (elevated) process's thread priorities is access-denied, so the process reports on itself instead.
///
/// Counted once per thread, never decremented: a reader thread that exits on unplug stays in the
/// total. That is a deliberate limit rather than an oversight — subtracting would need every call
/// site to hold an RAII guard for its thread's whole life, which is a lot of machinery for a
/// diagnostic whose actual job is answering "is the boost on at all?". See [`boosted_thread_count`]
/// for the wording that keeps it honest.
static BOOSTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many distinct input threads have raised their priority since start.
///
/// NOT a live count: a thread that has since exited (a HID reader whose device was unplugged) is
/// still included. Deliberately stated that way rather than implying "currently" — see [`BOOSTED`].
pub fn boosted_thread_count() -> u64 {
    BOOSTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// One line describing this process's input-scheduling posture, for diagnostics.
pub fn posture() -> String {
    if !priority_enabled() {
        return "input priority: DEFAULT (NEURON_INPUT_PRIORITY opt-out set)".into();
    }
    format!(
        "input priority: above-normal on {} input thread(s) raised since start",
        boosted_thread_count()
    )
}

/// Is the input-priority boost enabled? Read once — the environment does not change mid-run.
fn priority_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("NEURON_INPUT_PRIORITY")
            .map(|v| v != "default" && v != "0")
            .unwrap_or(true)
    })
}

#[cfg(windows)]
fn boost_impl() -> bool {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: FFI. `GetCurrentThread` returns a pseudo-handle that needs no closing and is always
    // valid for the calling thread; `SetThreadPriority` only reads it.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) != 0 }
}

#[cfg(not(windows))]
fn boost_impl() -> bool {
    false
}

/// An `Instant` `d` in the past — clamped to the clock's origin instead of panicking when the
/// process is younger than `d`.
///
/// `Instant::now() - d` is the obvious spelling and it panics. On Windows an `Instant` is a
/// `QueryPerformanceCounter` reading whose zero is system boot, so subtracting a minute from "now"
/// overflows for the first minute of every uptime — and a logon-launched process starts exactly
/// then.
///
/// Callers use this to seed a "last seen" stamp far enough back that the first comparison reads as
/// stale. When the clamp bites the stamp is the clock origin instead, so that first comparison
/// reads as recent — a cosmetic miss in the opening seconds of a boot, which is the whole reason
/// this is a clamp and not a panic.
pub fn ago(d: Duration) -> Instant {
    let now = Instant::now();
    now.checked_sub(d).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `SLEEP_ERROR` is a process-global histogram and cargo runs this module's tests in PARALLEL, so
    /// every test that sleeps would otherwise be recording into the same counter another test is
    /// asserting on (which is exactly how this started: a `count == 1` assertion saw 2). Serialize the
    /// ones that read it. Poison-tolerant — one panicking test must not wedge the rest.
    static SLEEP_HIST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_zero_sleep_returns_immediately_and_records_nothing() {
        let _guard = SLEEP_HIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::latency::SLEEP_ERROR.reset();
        let t = Instant::now();
        sleep_precise(Duration::ZERO);
        assert!(t.elapsed() < Duration::from_millis(2), "a zero sleep must not wait");
        assert_eq!(
            crate::latency::SLEEP_ERROR.count(),
            0,
            "a sleep that never happened is not a timing sample"
        );
    }

    #[test]
    fn sleep_never_returns_early() {
        let _guard = SLEEP_HIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The one hard contract: a step's pause may overshoot, but returning EARLY would reorder a
        // macro's keystrokes relative to the app receiving them, which is a correctness bug.
        for ms in [1u64, 3, 7] {
            let want = Duration::from_millis(ms);
            let t = Instant::now();
            sleep_precise(want);
            let took = t.elapsed();
            assert!(took >= want, "asked {want:?}, returned after only {took:?}");
        }
    }

    /// Can this OS create a high-resolution waitable timer (Windows 10 1803+)?
    ///
    /// That is the capability `std::thread::sleep` uses internally; without it, std documents a
    /// fallback to plain `Sleep`, whose granularity IS the ~15.6ms scheduler tick. The strict
    /// assertion below is about OUR code not regressing, so it must not fail merely because the
    /// platform genuinely cannot do better — that would be a test asserting the OS version.
    #[cfg(windows)]
    fn hi_res_timer_available() -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            CreateWaitableTimerExW, CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
        };
        // SAFETY: FFI with the documented null/null optional arguments; the handle is closed here.
        unsafe {
            let h = CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            );
            if h.is_null() {
                false
            } else {
                CloseHandle(h);
                true
            }
        }
    }

    #[cfg(not(windows))]
    fn hi_res_timer_available() -> bool {
        true // nanosleep is sub-millisecond everywhere we target off Windows
    }

    /// Macro step fidelity, pinned. A pause quantised to the ~15.6ms scheduler tick would make every
    /// short inter-step delay wrong by ~8x, so this fails if that ever becomes the behaviour — whether
    /// because a toolchain regressed, or because someone replaced the wait with a raw `Sleep`.
    ///
    /// The tight bound is asserted only where the platform can actually honour it. On a system with no
    /// high-resolution timer the module's own docs promise the `Sleep` fallback, and demanding
    /// sub-tick timing there would fail code that is behaving exactly as documented. A looser bound
    /// still applies everywhere, so the test keeps teeth on every machine rather than vanishing.
    #[test]
    fn short_sleeps_are_not_rounded_up_to_the_scheduler_tick() {
        let _guard = SLEEP_HIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let want = Duration::from_millis(2);
        // Take the BEST of many attempts: the claim is about the mechanism's CAPABILITY, so only one
        // clean sample is needed to prove it, while any single sample can be stolen by an unrelated
        // scheduling hiccup. The count is generous (~80ms total) precisely so a loaded CI box, a VM,
        // or a moment of contention cannot fail a correct implementation — raising the sample count
        // reduces flakiness without weakening what is asserted, which loosening the bound would.
        let best = (0..40)
            .map(|_| {
                let t = Instant::now();
                sleep_precise(want);
                t.elapsed()
            })
            .min()
            .expect("at least one sample");
        let bound = if hi_res_timer_available() {
            Duration::from_millis(8) // sub-tick timing is available: hold it to that
        } else {
            // The documented fallback. One tick plus headroom still catches a pause that has become
            // wildly wrong, without asserting a capability this OS does not have.
            Duration::from_millis(40)
        };
        assert!(
            best < bound,
            "a 2ms sleep took {best:?} at best (bound {bound:?}, high-resolution timer available: \
             {}) — macro step timing has regressed toward the scheduler-tick quantum",
            hi_res_timer_available()
        );
    }

    #[test]
    fn sleep_error_is_recorded_as_overshoot() {
        let _guard = SLEEP_HIST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        crate::latency::SLEEP_ERROR.reset();
        sleep_precise(Duration::from_millis(2));
        assert_eq!(crate::latency::SLEEP_ERROR.count(), 1, "the step's timing error was recorded");
        crate::latency::SLEEP_ERROR.reset();
    }

    #[test]
    fn boosting_the_current_thread_is_safe_to_call_and_honest_about_the_result() {
        // Windows: must succeed on an ordinary thread (unless the env opt-out is set, which the test
        // runner may well have inherited). Elsewhere: must report `false` rather than pretend. Either
        // way it must never panic — it runs at worker startup, before anything is set up to contain a
        // fault.
        let boosted = boost_input_thread();
        #[cfg(windows)]
        assert_eq!(
            boosted,
            priority_enabled(),
            "on Windows the boost must succeed exactly when it is enabled — ABOVE_NORMAL is always \
             grantable to a thread in its own process, so any other outcome is a real failure"
        );
        #[cfg(not(windows))]
        assert!(!boosted, "the non-Windows arm must not claim a boost it did not perform");
    }

    /// The posture line is the only way the boost can be confirmed on a deployed elevated build (an
    /// outside process gets access-denied reading its thread priorities), so it has to be accurate in
    /// both configurations rather than a fixed string.
    #[test]
    fn the_posture_line_reports_the_real_configuration() {
        let before = boosted_thread_count();
        let boosted = boost_input_thread();
        let after = boosted_thread_count();
        let line = posture();
        if priority_enabled() {
            assert!(
                line.contains("above-normal"),
                "enabled posture must name the level it applied: {line}"
            );
            #[cfg(windows)]
            {
                assert!(boosted, "on Windows an enabled boost must succeed");
                assert!(after > before, "a successful boost must be counted: {before} -> {after}");
            }
        } else {
            assert!(
                line.contains("DEFAULT"),
                "the opt-out posture must say so plainly rather than imply a boost: {line}"
            );
            assert!(!boosted);
            assert_eq!(after, before, "a refused boost must not be counted as one");
        }
    }

    /// One thread must count ONCE however many times it boosts. The HID readers re-run their boost on
    /// every device replug, so counting calls made the posture line drift upward over a session until
    /// it claimed more boosted threads than the process had — a diagnostic quietly overstating itself.
    #[test]
    fn a_thread_is_counted_once_no_matter_how_often_it_boosts() {
        // Run on a FRESH thread so this test owns its own thread-local latch and cannot be perturbed
        // by whatever the parallel test runner has already boosted.
        let handle = std::thread::spawn(|| {
            let before = boosted_thread_count();
            let first = boost_input_thread();
            let after_first = boosted_thread_count();
            for _ in 0..5 {
                boost_input_thread();
            }
            let after_many = boosted_thread_count();
            (first, before, after_first, after_many)
        });
        let (first, before, after_first, after_many) = handle.join().expect("probe thread finished");
        if !first {
            return; // boosting is disabled or unsupported here; nothing to count
        }
        assert_eq!(
            after_first,
            before + 1,
            "the first boost on a thread counts exactly one thread"
        );
        assert_eq!(
            after_many, after_first,
            "five more boosts on the SAME thread added {} phantom threads to the count",
            after_many - after_first
        );
    }

    #[test]
    fn ago_clamps_instead_of_overflowing_the_clock_origin() {
        let now = Instant::now();
        let ordinary = ago(Duration::from_secs(10));
        assert!(ordinary <= now, "an ordinary lookback must not land in the future");

        // The boot-time case, forced: no process is ever this old, so the subtraction cannot be
        // satisfied and the panic-free path is the ONLY one this can take. `Instant::now() - d`
        // here would abort the test.
        let absurd = ago(Duration::from_secs(60 * 60 * 24 * 365 * 1000));
        assert!(absurd <= Instant::now(), "the clamped stamp must still be in the past");
    }

    #[test]
    fn the_priority_opt_out_is_honoured_and_reports_no_boost() {
        // The escape hatch has to actually disable the thing, and say so — a `true` return with no
        // priority change would make the flight log claim a boost that never happened.
        // `priority_enabled` memoizes, so assert against whatever this process resolved rather than
        // mutating the environment underneath a `OnceLock` (which would prove nothing).
        if priority_enabled() {
            assert!(
                boost_input_thread() || !cfg!(windows),
                "enabled means the boost is applied"
            );
        } else {
            assert!(
                !boost_input_thread(),
                "with NEURON_INPUT_PRIORITY=default the boost must be refused, not silently applied"
            );
        }
    }
}
