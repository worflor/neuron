// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! RECONCILER — the coordination engine behind "the UI must never show a fake state".
//!
//! The bug class this exists to close: a subsystem publishes device/OS state to the UI before the
//! thing that makes a READ truthful has actually run (hidwatch not armed yet, the mic bridge not
//! wired, devices not enumerated), gated on the wrong condition (a timer tick, a window-shown
//! event) instead of on the readiness that actually matters — so the UI shows a stale/default value
//! until the user happens to interact and re-triggers a real read. This module separates the three
//! concerns that got tangled together:
//!   * RESOLVE — read the current truth (a later chunk's job, inside a `ReconcileUnit::run`).
//!   * RECORD  — publish it to whatever shared state the UI reads (also inside `run`).
//!   * RENDER  — the UI's own concern; it only ever reads what RECORD wrote.
//! This chunk builds the scheduler that decides WHEN `run` fires: not before its declared
//! `Readiness` dependencies are met, but never later than a bounded timeout either — so a slow or
//! permanently-absent dependency can't wedge the unit forever. Everything here is pure coordination:
//! no Slint, no device I/O, no real `Readiness` signals wired anywhere yet. Later chunks plug real
//! units (and call `signal_ready` from the real subsystems) into this engine unchanged.
//!
//! Vocabulary: [`Truth<V>`] tags every published value with WHERE it came from (`Read` off real
//! hardware/OS state, `Asserted` because we ourselves wrote it and therefore know it's true, or
//! `Unknown` when there is nothing honest to show) — so a unit's `run` can never smuggle a fake
//! default past the UI; an unresolved value is `Unknown`, not a zero or an empty string.
//!
//! The scheduler: [`register`] adds a [`ReconcileUnit`] to the process-global registry;
//! [`request`]/[`request_with_timeout`] enqueue a [`Scope`] (all units, or one unit) onto
//! the single worker thread [`start`] spawns; the worker gates each targeted unit on
//! [`deps_ready`] OR a per-request timeout, whichever comes first, waking EVENT-DRIVEN off a
//! `Condvar` that [`signal_ready`] notifies — never a sleep-poll loop.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── Truth<V> — provenance-tagged value ──────────────────────────────────────────────────────────

/// A published value tagged with WHERE it came from. The whole point: a caller can never confuse a
/// real read with a fabricated default, and a write-only capability's "we wrote it, so it's true"
/// confidence is distinguishable from an actual hardware read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Truth<V> {
    /// Read from real hardware/OS truth.
    Read(V),
    /// Write-only capability: we wrote this value, so — absent a getter to contradict it — it is
    /// true.
    ///
    /// Currently UNCONSTRUCTED, deliberately: the only write-only capability wired to a unit is the
    /// scroll stage, and nothing in the app persists a scroll-stage value to re-assert at launch —
    /// so `scroll_stage` honestly resolves `Unknown` (a dash) instead. The moment a value IS
    /// persisted, that unit re-applies it and publishes it as `Asserted`; keeping the variant is
    /// what makes "we made this true" expressible rather than tempting a fake `Read`.
    #[allow(dead_code)]
    Asserted(V),
    /// Asleep, no getter, or nothing persisted yet: the UI must show a dash, never a fake default.
    Unknown,
}

// NOTE — `Truth` deliberately carries no combinators (`value`/`map`/`as_ref` etc). Units match on it
// directly and publish; helpers written "in case" were dead weight and are gone. Add one when a unit
// actually needs it.

// ── Readiness — a barrier expressed as data ─────────────────────────────────────────────────────

/// A subsystem whose start-up makes some read truthful. Extensible: add a variant here and nothing
/// else needs to change (the bitset is keyed by discriminant, well under the 32-bit budget) — so
/// variants are added WHEN A UNIT WAITS ON ONE, not speculatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Readiness {
    /// The mic/audio bridge is wired — capture-endpoint mute state is now resolvable. Signalled by
    /// `hidwatch` once its initial hardware-mute seeds have run (or it knows none will).
    MicBridge,
    /// The initial device enumeration/scan has completed — the device list is now real, so a
    /// per-device read reflects hardware rather than an empty registry.
    DevicesScanned,
}

impl Readiness {
    fn bit(self) -> u32 {
        1 << (self as u32)
    }
}

/// The process-global signaled set, as a bitset (cheaper than a `Mutex<HashSet<_>>` and lock-free
/// to read on every `deps_ready` check).
static READY_BITS: AtomicU32 = AtomicU32::new(0);

/// The mutex+condvar pair `signal_ready` notifies and the worker waits on. The mutex guards nothing
/// but the wait/notify handshake itself — the real state lives in `READY_BITS` — so every check of
/// `deps_ready`/`should_stop` that matters for correctness happens WHILE HOLDING this lock, closing
/// the lost-wakeup window between "check the bits" and "start waiting".
fn ready_gate() -> &'static (Mutex<()>, Condvar) {
    static GATE: OnceLock<(Mutex<()>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (Mutex::new(()), Condvar::new()))
}

/// Mark `r` ready and wake every worker waiting on readiness. Idempotent (signaling an
/// already-ready dep is harmless).
pub fn signal_ready(r: Readiness) {
    READY_BITS.fetch_or(r.bit(), Ordering::SeqCst);
    let (lock, cv) = ready_gate();
    let _guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    cv.notify_all();
}

/// Whether `r` has been signaled.
pub fn is_ready(r: Readiness) -> bool {
    READY_BITS.load(Ordering::SeqCst) & r.bit() != 0
}

/// Clear every signaled readiness bit. Test-only: production code never un-signals a readiness that
/// has already become true.
#[cfg(test)]
pub fn reset_readiness() {
    READY_BITS.store(0, Ordering::SeqCst);
}

/// Pure: every dep in `deps` has been signaled (an empty slice is trivially ready).
fn deps_ready(deps: &[Readiness]) -> bool {
    deps.iter().all(|d| is_ready(*d))
}

/// Block until `should_stop()` is true (checked WHILE HOLDING the gate's lock, so a `signal_ready`
/// racing this call either lands before the check — seen immediately — or after this starts
/// waiting — delivered by `notify_all`; there is no window where it is lost) or `deadline` passes,
/// whichever comes first. A single call may wake early on an unrelated signal; the caller is
/// expected to re-check its own condition and call again if still waiting on something else.
fn wait_for_progress_or_deadline(deadline: Instant, should_stop: impl Fn() -> bool) {
    let (lock, cv) = ready_gate();
    let mut guard = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    loop {
        if should_stop() {
            return;
        }
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let (g, _timed_out) = cv
            .wait_timeout(guard, deadline - now)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard = g;
    }
}

// ── ReconcileUnit — a registered unit of state to resolve+publish ──────────────────────────────

/// One registered unit of work: resolve current truth and publish it, unconditionally, the moment
/// its `deps` are satisfied (or a timeout forces it). The scheduler is type-agnostic — each unit
/// owns its own value type `V` entirely inside its closure; the engine only ever sees `Fn()`.
pub struct ReconcileUnit {
    pub id: &'static str,
    pub deps: &'static [Readiness],
    /// Resolve current truth and publish it. Owns its own value type internally; called with no
    /// arguments and expected to reach whatever shared state the UI reads on its own.
    pub run: Box<dyn Fn() + Send + Sync>,
}

impl ReconcileUnit {
    pub fn new(
        id: &'static str,
        deps: &'static [Readiness],
        run: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        ReconcileUnit { id, deps, run: Box::new(run) }
    }
}

/// The default per-unit wait: how long a unit waits for its deps before running anyway. Tests
/// inject a much shorter timeout via [`request_with_timeout`] so they never sleep for real seconds.
pub const RECONCILE_DEP_TIMEOUT: Duration = Duration::from_secs(2);

// ── Scope — what a request targets ───────────────────────────────────────────────────────────────

/// What a [`request`] targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every registered unit. What `main` fires once at the end of startup.
    All,
    /// The one registered unit with this id (a no-op if none matches). E.g. the manual
    /// "refresh mic" button re-running just `mic_mute`.
    Unit(&'static str),
}

// ── the registry + worker singleton ─────────────────────────────────────────────────────────────

/// The process-global registry. Units are held behind `Arc` so the worker can snapshot the ones a
/// request targets, drop the registry lock, and run them WITHOUT holding it — a slow or panicking
/// `run` can never block a concurrent `register`.
fn registry() -> &'static Mutex<Vec<Arc<ReconcileUnit>>> {
    static R: OnceLock<Mutex<Vec<Arc<ReconcileUnit>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Vec::new()))
}

/// Register a unit. Safe to call before or after [`start`] — the worker always reads the live
/// registry when a request arrives, never a snapshot taken at spawn time.
pub fn register(unit: ReconcileUnit) {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(Arc::new(unit));
}

/// Clear every registered unit. Test-only — production never needs to un-register (units live for
/// the process).
#[cfg(test)]
pub fn reset_registry() {
    registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
}

fn units_for_scope(scope: Scope) -> Vec<Arc<ReconcileUnit>> {
    let reg = registry().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match scope {
        Scope::All => reg.iter().cloned().collect(),
        Scope::Unit(id) => reg.iter().filter(|u| u.id == id).cloned().collect(),
    }
}

/// The live worker's send-end, present only once [`start`] has actually spawned the thread —
/// mirrors this codebase's `OnceLock<Mutex<Sender<_>>>` singleton idiom (see e.g. `hidwatch`'s
/// registry cell). `OnceLock::set` makes the spawn itself race-free: if two callers both observe
/// `start` as not-yet-run, only one wins the `set` and only the winner spawns the thread.
static WORKER_TX: OnceLock<Mutex<Sender<(Scope, Duration)>>> = OnceLock::new();

/// Spawn the single reconcile worker thread. Idempotent — a second (or concurrent) call is a no-op.
pub fn start() {
    if WORKER_TX.get().is_some() {
        return;
    }
    let (tx, rx) = mpsc::channel::<(Scope, Duration)>();
    if WORKER_TX.set(Mutex::new(tx)).is_ok() {
        // `spawn_named` (not fire-and-forget): this is a lifecycle thread with no owner to join it
        // yet in this chunk (nothing wires teardown), but it is the project's one thread-creation
        // primitive that returns a real handle — the ONLY thing this chunk needs, so no latch is
        // left stranded by a refused spawn (there is none to strand).
        let _ = crate::worker::spawn_named("neuron-reconcile", move || worker_loop(rx));
    }
    // else: lost the race to set WORKER_TX — another thread's `start()` is spawning; `tx`/`rx` here
    // are simply dropped.
}

/// Enqueue a reconcile request for `scope`, using the default [`RECONCILE_DEP_TIMEOUT`]. A no-op if
/// [`start`] has never been called (nothing is listening).
pub fn request(scope: Scope) {
    request_with_timeout(scope, RECONCILE_DEP_TIMEOUT);
}

/// [`request`] with an injectable per-unit timeout — the seam tests use to keep runs fast.
pub fn request_with_timeout(scope: Scope, timeout: Duration) {
    if let Some(tx) = WORKER_TX.get() {
        let tx = tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = tx.send((scope, timeout));
    }
}

fn worker_loop(rx: Receiver<(Scope, Duration)>) {
    for (scope, timeout) in rx {
        run_scope(scope, timeout);
    }
}

/// Run every unit `scope` targets, each gated on its own deps-ready-or-timeout: a slow dep never
/// serializes the whole batch behind it — a unit whose deps are already met runs promptly, and each
/// iteration waits only for the SHORTEST outstanding deadline among what's left, so a unit that
/// becomes eligible early runs early even while a sibling is still pending.
fn run_scope(scope: Scope, timeout: Duration) {
    let now = Instant::now();
    let mut pending: Vec<(Arc<ReconcileUnit>, Instant)> =
        units_for_scope(scope).into_iter().map(|u| (u, now + timeout)).collect();

    while !pending.is_empty() {
        let mut i = 0;
        while i < pending.len() {
            let (ready, timed_out) = {
                let (unit, deadline) = &pending[i];
                (deps_ready(unit.deps), Instant::now() >= *deadline)
            };
            if ready || timed_out {
                let (unit, _) = pending.remove(i);
                run_unit(&unit);
            } else {
                i += 1;
            }
        }
        if pending.is_empty() {
            break;
        }
        // Safe: `pending` is non-empty here (the loop above would have `break`d otherwise).
        let earliest = pending.iter().map(|(_, d)| *d).min().unwrap();
        wait_for_progress_or_deadline(earliest, || {
            pending.iter().any(|(u, _)| deps_ready(u.deps))
        });
    }
}

/// Call `unit.run()` exactly once, with its panic CONTAINED — one bad unit can never kill the
/// worker or block its siblings.
fn run_unit(unit: &ReconcileUnit) {
    let f = &unit.run;
    if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        crate::flight::trace("reconcile-panic", unit.id, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex as StdMutex;

    /// Serializes every test below: they all share the process-global readiness bitset and unit
    /// registry (`reset_readiness`/`reset_registry` wipe process state a concurrently-running test
    /// would also be relying on). Poison-tolerant — one panicking test must not wedge the rest.
    /// Mirrors `neuron-core::controls`'s `INJECT_TEST_LOCK`.
    static RECONCILE_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// Common setup: hold the serialization lock, start with a clean readiness/registry slate, and
    /// make sure the worker is running (idempotent — later tests' calls are no-ops).
    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let guard = RECONCILE_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_readiness();
        reset_registry();
        start();
        guard
    }

    // `Truth` has no combinators left to test — it is a plain provenance tag that units match on
    // directly (the `value`/`is_known`/`as_ref`/`map` helpers, and these tests with them, went when
    // it turned out nothing but the tests ever called them).

    // ── deps_ready ───────────────────────────────────────────────────────────────────────────────

    #[test]
    fn deps_ready_logic() {
        let _guard = setup();
        assert!(deps_ready(&[]), "no deps is trivially ready");
        assert!(!deps_ready(&[Readiness::DevicesScanned]), "an unsignaled dep is not ready");
        signal_ready(Readiness::DevicesScanned);
        assert!(deps_ready(&[Readiness::DevicesScanned]));
        assert!(
            !deps_ready(&[Readiness::DevicesScanned, Readiness::MicBridge]),
            "one missing dep among several keeps the whole set not-ready"
        );
        signal_ready(Readiness::MicBridge);
        assert!(deps_ready(&[Readiness::DevicesScanned, Readiness::MicBridge]));
    }

    // ── the scheduler ────────────────────────────────────────────────────────────────────────────

    #[test]
    fn unit_with_already_satisfied_deps_runs_promptly() {
        let _guard = setup();
        signal_ready(Readiness::DevicesScanned);
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        register(ReconcileUnit::new("t-prompt", &[Readiness::DevicesScanned], move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        let start = Instant::now();
        request_with_timeout(Scope::Unit("t-prompt"), Duration::from_secs(2));
        wait_for(|| ran.load(Ordering::SeqCst) == 1, Duration::from_millis(500));
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert!(
            start.elapsed() < Duration::from_millis(400),
            "an already-ready unit must not wait out any timeout: took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn unit_with_unmet_dep_waits_then_runs_exactly_once_after_signal() {
        let _guard = setup();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        register(ReconcileUnit::new("t-wait", &[Readiness::DevicesScanned], move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        // A generous timeout — if the signal below didn't wake it event-driven, the test would have
        // to wait out the whole thing; asserting the ran-before-signal state below proves it didn't.
        request_with_timeout(Scope::Unit("t-wait"), Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "must not run before its dep is signaled");
        let sig_ran = ran.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            signal_ready(Readiness::DevicesScanned);
            let _ = sig_ran; // keep the clone alive for clarity; signal_ready needs no arg from it
        });
        wait_for(|| ran.load(Ordering::SeqCst) >= 1, Duration::from_secs(1));
        // give any (incorrect) double-run a moment to show up before asserting exactly-once
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "run exactly once, not twice");
    }

    #[test]
    fn unit_whose_deps_never_satisfy_runs_anyway_after_the_injected_timeout() {
        let _guard = setup();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        register(ReconcileUnit::new("t-timeout", &[Readiness::MicBridge], move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        let timeout = Duration::from_millis(60);
        let start = Instant::now();
        request_with_timeout(Scope::Unit("t-timeout"), timeout);
        // must NOT have run before the timeout elapses (MicBridge is never signaled in this test)
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(ran.load(Ordering::SeqCst), 0, "must not fire early on an unmet dep");
        wait_for(|| ran.load(Ordering::SeqCst) == 1, Duration::from_secs(1));
        assert!(
            start.elapsed() >= timeout,
            "must not run before its timeout elapsed: ran after {:?}, timeout was {:?}",
            start.elapsed(),
            timeout
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn run_is_called_exactly_once_per_request_no_double_run() {
        let _guard = setup();
        signal_ready(Readiness::DevicesScanned);
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        register(ReconcileUnit::new("t-once", &[Readiness::DevicesScanned], move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        request_with_timeout(Scope::Unit("t-once"), Duration::from_secs(2));
        wait_for(|| ran.load(Ordering::SeqCst) >= 1, Duration::from_millis(500));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(ran.load(Ordering::SeqCst), 1, "one request runs a unit exactly once");
    }

    #[test]
    fn a_panicking_unit_is_contained_and_does_not_stop_its_siblings() {
        let _guard = setup();
        signal_ready(Readiness::DevicesScanned);
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the intentional panic below
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        register(ReconcileUnit::new("t-panic", &[Readiness::DevicesScanned], || {
            panic!("boom — this unit is deliberately broken");
        }));
        register(ReconcileUnit::new("t-panic-sibling", &[Readiness::DevicesScanned], move || {
            r.fetch_add(1, Ordering::SeqCst);
        }));
        request_with_timeout(Scope::All, Duration::from_secs(2));
        wait_for(|| ran.load(Ordering::SeqCst) == 1, Duration::from_millis(500));
        std::panic::set_hook(prev);
        assert_eq!(ran.load(Ordering::SeqCst), 1, "the sibling still ran despite the panic");
    }

    #[test]
    fn a_fast_dep_unit_runs_promptly_without_waiting_on_a_slow_sibling() {
        // The "All at launch" shape: two units in ONE request, one ready immediately, one gated on a
        // dep that only arrives later. The fast one must not be serialized behind the slow one's wait
        // — that is exactly the launch stall this scheduler exists to avoid.
        let _guard = setup();
        signal_ready(Readiness::DevicesScanned);
        let fast_ran = Arc::new(AtomicUsize::new(0));
        let slow_ran = Arc::new(AtomicUsize::new(0));
        let fr = fast_ran.clone();
        let sr = slow_ran.clone();
        register(ReconcileUnit::new("t-fast", &[Readiness::DevicesScanned], move || {
            fr.fetch_add(1, Ordering::SeqCst);
        }));
        register(ReconcileUnit::new("t-slow", &[Readiness::MicBridge], move || {
            sr.fetch_add(1, Ordering::SeqCst);
        }));

        request_with_timeout(Scope::All, Duration::from_secs(2));
        wait_for(|| fast_ran.load(Ordering::SeqCst) == 1, Duration::from_millis(300));
        // the fast unit ran well before the slow one's 2s timeout could possibly have elapsed
        assert_eq!(fast_ran.load(Ordering::SeqCst), 1);
        assert_eq!(slow_ran.load(Ordering::SeqCst), 0, "the slow unit is still waiting on its dep");
        signal_ready(Readiness::MicBridge);
        wait_for(|| slow_ran.load(Ordering::SeqCst) == 1, Duration::from_millis(500));
        assert_eq!(slow_ran.load(Ordering::SeqCst), 1);
    }

    /// Poll-free-ish test helper: bounded busy-wait on a condition, for asserting the ASYNC worker's
    /// effect from the test thread. (The engine itself is event-driven; only the test's own
    /// observation of its result needs to poll, same as this codebase's other worker tests — see
    /// `neuron-core::worker`'s tests.)
    fn wait_for(mut cond: impl FnMut() -> bool, cap: Duration) {
        let start = Instant::now();
        while !cond() {
            if start.elapsed() >= cap {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
