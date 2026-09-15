// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! THE MACRO RUNNER — a small pool of warm threads that macro bodies are handed to.
//!
//! ## Why this exists (measured, not assumed)
//!
//! A macro sequence can sleep — held keys, inter-step delays — so it must not run on the dispatch
//! thread, which services every other device edge. The original fix was to spawn a fresh thread per
//! fire. Correct, and much more expensive than it looks: the `latency_probe` example measured
//! `macro_spawn` at **mean 342µs, p90 639µs, p99 2.6ms, worst 3.4ms** — paid ON the dispatch thread,
//! before the macro's first keystroke can go out. It was the single largest controllable cost in the
//! whole press→output path, larger than resolve (~3µs) and dispatch (~2µs) by two orders of magnitude.
//!
//! Threads that already exist cost a channel send instead: a few microseconds, and no allocation of
//! stack or TEB on the input path.
//!
//! ## Why a pool of four, and not one
//!
//! One worker would serialize macros: press a macro with 500ms of delays, then press another, and the
//! second waits half a second. That is a worse product than the thread-per-fire version it replaces.
//! Four lets ordinary overlapping use run concurrently — the same as before — while putting a real
//! ceiling on what a stuck-repeating trigger can create.
//!
//! ## Why bounded, and why a full queue DROPS
//!
//! The unbounded version had a real failure mode: a held or mashed macro trigger created threads
//! faster than they finished, and the pile-up was felt as general lag with no obvious cause. A bounded
//! queue makes the limit explicit. When it is full the job is REFUSED and says so, rather than:
//!
//! * blocking the dispatch thread until space frees (that stalls every other binding — the exact
//!   thing this module exists to prevent), or
//! * running inline (same stall, for the macro's entire duration), or
//! * silently discarding it (the user sees a macro not fire and has no way to know why).
//!
//! A refusal is the honest outcome: something is already asking for more macro work than the machine
//! can run, and the caller reports that.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};

/// How many macro bodies can run at once. See the module doc — enough for real overlapping use,
/// small enough to bound what a stuck trigger can occupy.
const WORKERS: usize = 4;

/// How many macros may be WAITING beyond those running. A handful covers a burst of deliberate
/// presses; past it, the machine is being asked for more than it can do and saying so beats hiding it.
const QUEUE_CAP: usize = 64;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// How many jobs have been refused because the queue was full — surfaced so backpressure is
/// diagnosable after the fact instead of only being felt.
static REFUSED: AtomicU64 = AtomicU64::new(0);

struct Pool {
    tx: SyncSender<Job>,
}

fn pool() -> Option<&'static Pool> {
    static POOL: OnceLock<Option<Pool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = sync_channel::<Job>(QUEUE_CAP);
        // The receiver is shared: each worker locks it only long enough to take a job, then RELEASES
        // before running it. Holding the lock across the job would serialize the pool down to one
        // worker — the bug that would quietly undo the whole point of having four.
        let rx = Arc::new(Mutex::new(rx));
        let mut started = 0usize;
        for i in 0..WORKERS {
            let rx = rx.clone();
            if crate::worker::spawn_detached(&format!("neuron-macro-{i}"), move || {
                worker_loop(&rx)
            }) {
                started += 1;
            }
        }
        if started == 0 {
            // Not a single worker: the caller falls back to spawning per fire, which is slower but
            // still works. Never leave the pool half-initialized-and-claimed.
            return None;
        }
        Some(Pool { tx })
    })
    .as_ref()
}

// Pool workers deliberately run at NORMAL priority — see `worker_loop`.
fn worker_loop(rx: &Mutex<Receiver<Job>>) {
    // NOT boosted, and that is a measured decision rather than an oversight.
    //
    // These threads were briefly raised to above-normal on the theory that a worker woken by a
    // keypress should be scheduled promptly, since its wake latency lands in `PRESS_TO_OUTPUT`. The
    // measurement says otherwise: with 14 above-normal threads saturating the CPU, a 3-step macro's
    // `press_to_output` was 18.5ms mean / 895µs p50 boosted, versus 19.0ms mean / 895µs p50 at normal
    // priority — indistinguishable. The term that dominates a macro's time-to-first-output is
    // `SendInput` itself, not how quickly this thread is scheduled.
    //
    // And unlike the pump and the HID readers — short, bounded, ours — a pool worker runs ARBITRARY
    // USER MACRO BODIES: sequences that can hold keys and sleep for up to a minute per step. Boosting
    // them would hand elevated scheduling to unbounded user code for its whole duration, competing
    // with the foreground application the user actually cares about. Paying that to gain nothing
    // measurable is exactly the trade `crate::timing` argues against.
    loop {
        let job = {
            let guard = rx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.recv()
        };
        match job {
            Ok(job) => {
                // Contain a panicking macro body: this thread is shared and permanent, so letting it
                // die would silently shrink the pool with every bad macro until nothing ran at all.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                // Clear any input-edge stamp the job left behind. Workers are REUSED, so a job that
                // adopted an origin and then produced no output would otherwise leave it set for the
                // next job on this thread — which would report that unrelated macro's first keystroke
                // as having taken however long the two were apart. See `crate::latency`.
                crate::latency::adopt(None);
            }
            // Every sender is gone (only at process teardown) — nothing more can arrive.
            Err(_) => return,
        }
    }
}

/// The outcome of handing a macro body to the runner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// A warm worker took it (the fast, normal path).
    Queued,
    /// The pool could not be created at all; the caller should fall back to its own thread.
    NoPool,
    /// The queue is full — too many macros already running or waiting. Deliberately NOT run: see
    /// the module doc on why refusing beats blocking, stalling, or discarding silently.
    Refused,
}

/// Hand a macro body to a warm worker. Returns at once — the work happens off the calling thread.
///
/// This is the hot path from a keypress, so it must stay cheap: a `try_send` into a bounded channel
/// and nothing else. No allocation beyond boxing the job, no lock held across any work, no syscall.
pub fn submit(job: impl FnOnce() + Send + 'static) -> Submitted {
    let Some(pool) = pool() else {
        return Submitted::NoPool;
    };
    match pool.tx.try_send(Box::new(job)) {
        Ok(()) => Submitted::Queued,
        Err(TrySendError::Full(_)) => {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            Submitted::Refused
        }
        // Disconnected means every worker is gone, which only happens if all four died. Report it as
        // "no pool" so the caller falls back to its own thread rather than losing the macro.
        Err(TrySendError::Disconnected(_)) => Submitted::NoPool,
    }
}

/// How many macro submissions have been refused for a full queue since start — the backpressure
/// counter the status readout can show, so "my macros stopped firing" has an answer.
pub fn refused_count() -> u64 {
    REFUSED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// The pool is PROCESS-GLOBAL with a fixed worker count, and cargo runs these tests in parallel —
    /// so without serialization one test's queued jobs occupy the workers another test is asserting
    /// about. The concurrency test needs two free workers to prove overlap, and the cost comparison
    /// needs an idle pool to time against; either would fail intermittently, in a way that reads as a
    /// real bug. Poison-tolerant so one panicking test cannot wedge the rest.
    static POOL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Take the pool lock and wait for the pool to go idle, so each test starts from the same state
    /// regardless of what ran before it.
    fn exclusive_pool() -> std::sync::MutexGuard<'static, ()> {
        let guard = POOL_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Drain: push one job per worker and wait for all of them. They can only all complete once
        // every worker has finished whatever it was carrying, which is a definitive idle signal — far
        // more reliable than sleeping and hoping.
        let (tx, rx) = mpsc::channel();
        let mut sent = 0;
        for _ in 0..WORKERS {
            let tx = tx.clone();
            if submit(move || tx.send(()).unwrap_or(())) == Submitted::Queued {
                sent += 1;
            }
        }
        for _ in 0..sent {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
        guard
    }

    #[test]
    fn a_submitted_job_runs_on_a_worker() {
        let _pool = exclusive_pool();
        let (tx, rx) = mpsc::channel();
        assert_eq!(submit(move || tx.send(7).unwrap_or(())), Submitted::Queued);
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(7),
            "the pool ran the job"
        );
    }

    #[test]
    fn jobs_run_off_the_calling_thread() {
        let _pool = exclusive_pool();
        // The entire point: `submit` must return before the work finishes, or it would be blocking the
        // dispatch thread exactly like the synchronous path it replaces.
        let (done_tx, done_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        submit(move || {
            let _ = go_rx.recv_timeout(Duration::from_secs(5));
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.try_recv().is_err(),
            "submit returned while the job was still blocked — so it did not run inline"
        );
        let _ = go_tx.send(());
        assert!(done_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "and it then completed");
    }

    #[test]
    fn several_jobs_run_concurrently_rather_than_one_at_a_time() {
        let _pool = exclusive_pool();
        // Pins the reason the pool has more than one worker: a slow macro must not delay the next
        // one. Two jobs that each wait for the OTHER to start can only both finish if they overlap.
        let (a_started, a_started_rx) = mpsc::channel();
        let (b_started, b_started_rx) = mpsc::channel();
        let (fin, fin_rx) = mpsc::channel();
        let fin2 = fin.clone();
        submit(move || {
            let _ = a_started.send(());
            let _ = b_started_rx.recv_timeout(Duration::from_secs(5));
            let _ = fin.send('a');
        });
        submit(move || {
            let _ = b_started.send(());
            let _ = a_started_rx.recv_timeout(Duration::from_secs(5));
            let _ = fin2.send('b');
        });
        let first = fin_rx.recv_timeout(Duration::from_secs(5));
        let second = fin_rx.recv_timeout(Duration::from_secs(5));
        assert!(
            first.is_ok() && second.is_ok(),
            "both jobs finished, so they ran at the same time rather than serially"
        );
    }

    #[test]
    fn a_panicking_macro_does_not_kill_its_worker() {
        let _pool = exclusive_pool();
        // A shared, permanent worker must survive a bad macro body — otherwise every panic would
        // shrink the pool by one until macros stopped running entirely, with no visible cause.
        submit(|| panic!("a macro body blew up"));
        // The pool must still serve work afterwards.
        let (tx, rx) = mpsc::channel();
        submit(move || tx.send(1).unwrap_or(()));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)),
            Ok(1),
            "the pool still runs jobs after a panicking one"
        );
    }

    #[test]
    #[ignore = "perf micro-bench; run explicitly with --ignored --nocapture (see the note below)"]
    fn submitting_is_far_cheaper_than_spawning_a_thread() {
        let _pool = exclusive_pool();
        // The measurement that justifies this module, as a test: handing work to a warm worker must be
        // dramatically cheaper than creating a thread, since that difference is paid on the dispatch
        // thread on every macro press. Compared as a RATIO against this machine's own thread-spawn
        // cost rather than an absolute microsecond budget, so it holds on slow and fast hardware alike.
        //
        // WHY #[ignore]: the ratio survives slow hardware, but not CONTENDED hardware. On a shared
        // 2-core CI runner this measured 620.8us to submit against 545.3us to spawn - the pool came
        // out SLOWER - because a preempted submit and an unusually cheap spawn are both artefacts of
        // someone else's job on the same box, not of this code. On a real desk the gap is roughly an
        // order of magnitude (~37us vs ~342us). So it keeps its teeth where the number means
        // something and stops failing CI at random, which is the same call already made for
        // `lighting_bench`. Run it with `--ignored` locally whenever the pool is touched.
        // Stay well under QUEUE_CAP so nothing is refused: a refusal is CHEAPER than a real submit,
        // so counting refusals as submits would flatter the pool and make this test dishonest.
        let n = 32;
        assert!(n < QUEUE_CAP, "the comparison must not run into backpressure");
        let spawn_start = std::time::Instant::now();
        for _ in 0..n {
            crate::worker::spawn_detached("neuron-macro-spawn-baseline", || {});
        }
        let spawn_cost = spawn_start.elapsed();

        let (tx, rx) = mpsc::channel();
        let submit_start = std::time::Instant::now();
        let mut queued = 0usize;
        for _ in 0..n {
            let tx = tx.clone();
            if submit(move || tx.send(()).unwrap_or(())) == Submitted::Queued {
                queued += 1;
            }
        }
        let submit_cost = submit_start.elapsed();
        assert_eq!(queued, n, "every job was accepted, so the timings compare like with like");
        // Drain exactly what was queued, so the pool is idle for the other tests.
        for _ in 0..queued {
            assert!(
                rx.recv_timeout(Duration::from_secs(10)).is_ok(),
                "every queued job ran"
            );
        }
        assert!(
            submit_cost * 3 < spawn_cost,
            "submitting {n} jobs took {submit_cost:?} vs {spawn_cost:?} to spawn {n} threads — the \
             warm pool is supposed to be several times cheaper, which is its entire justification"
        );
    }
}
