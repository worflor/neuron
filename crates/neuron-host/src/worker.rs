//! The named-thread primitive for neuron-host's lifecycle threads (protocol servers, per-socket
//! connection handlers, the OBS bridge, the paced writer, the SHM arbiter). Every one of these is
//! OWNED by a struct that `.join()`s its handle on Drop/stop, so they need the JoinHandle back —
//! the fire-and-forget helpers in neuron-core can't express that. This is a local, pure-`std`
//! mirror of `neuron::worker::spawn_named`: the host kernel keeps `neuron` an OPTIONAL dependency
//! (see this crate's Cargo.toml) and must build without it, so it cannot reach across for the
//! primitive. Routing every raw thread creation through this one file lets the `conventions` test
//! stay a strict allowlist (this file + neuron-core's `worker.rs`) with no per-site markers.

/// Spawn a NAMED worker and return its `JoinHandle`. The name is mandatory (per-thread CPU
/// attribution); the caller owns the `JoinHandle` and joins it on teardown. An `Err` is a spawn
/// refusal the owner must tolerate (the feature is simply unavailable — no partial state to
/// unwind, since these threads hold their own state and publish nothing before running).
pub fn spawn_named<T, W>(name: &str, work: W) -> std::io::Result<std::thread::JoinHandle<T>>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(work)
}

/// The Drop-time backstop for every owned thread in this crate: join `handle` if it finishes
/// within `deadline`, otherwise LOG LOUDLY and drop the handle instead of blocking the dropping
/// thread forever.
///
/// `JoinHandle::join` cannot be time-bounded on stable std, so this polls `is_finished()` on a
/// short, cheap cadence instead. Every owned-thread loop in this crate is DESIGNED (see the
/// `*_DROP_DEADLINE` constants next to each `Drop` impl) so `deadline` comfortably covers its
/// guaranteed stop-check cadence — hitting this backstop means that design bound was violated
/// (a socket read/write outlived its timeout, a sink call never returned), not routine slowness.
/// A leaked thread at shutdown is strictly better than a hung process: the caller trades a
/// possible thread leak for a Drop that always returns.
pub fn join_bounded<T>(
    handle: std::thread::JoinHandle<T>,
    deadline: std::time::Duration,
    name: &str,
) -> Option<T> {
    let start = std::time::Instant::now();
    let step = std::time::Duration::from_millis(5);
    while !handle.is_finished() {
        if start.elapsed() >= deadline {
            eprintln!(
                "[worker] {name}: did not stop within {deadline:?} at drop — leaking the \
                 thread rather than hanging the process (a Layer-1 stop-check bound was \
                 violated; this is a bug to investigate, not a deadline to raise)"
            );
            return None;
        }
        std::thread::sleep(step);
    }
    handle.join().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn join_bounded_joins_a_thread_that_finishes_in_time() {
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        let handle = spawn_named("t-join-fast", move || {
            flag.store(true, Ordering::SeqCst);
            7u32
        })
        .expect("spawn");
        let v = join_bounded(handle, Duration::from_secs(2), "t-join-fast");
        assert_eq!(v, Some(7));
        assert!(ran.load(Ordering::SeqCst));
    }

    #[test]
    fn join_bounded_gives_up_and_returns_none_on_a_wedged_thread() {
        // A thread that never returns (parked on a channel recv with no sender) — the exact
        // shape of a wedged owned thread. join_bounded must not hang the CALLER.
        let (_tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = spawn_named("t-join-wedged", move || {
            let _ = rx.recv(); // blocks forever: sender is never dropped or sent to
        })
        .expect("spawn");
        let start = std::time::Instant::now();
        let deadline = Duration::from_millis(100);
        let v = join_bounded(handle, deadline, "t-join-wedged");
        assert!(v.is_none(), "a wedged thread must report None, not join");
        assert!(
            start.elapsed() < deadline * 3,
            "join_bounded must return promptly after its deadline, took {:?}",
            start.elapsed()
        );
        // The wedged thread is intentionally leaked here; the test process exits without it.
    }
}
