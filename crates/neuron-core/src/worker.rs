// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Named background workers whose completion callback CANNOT be stranded.
//!
//! The trap: a caller sets up state (a latch, a claimed throttle slot, a pending protocol
//! request) and the cleanup / result-reporting lives inside the spawned worker. If the worker
//! never runs — the OS refuses the thread under resource exhaustion — or panics partway, that
//! cleanup never happens and the state sticks: a latch stays "busy" for the process lifetime, a
//! macro blocks forever on a result frame that never comes, a compositor reports "live" with no
//! thread behind it.
//!
//! [`spawn_notify`] is the one primitive: `done` fires EXACTLY ONCE on every path — the worker's
//! value on success, `None` on a worker panic, `None` synchronously on a spawn refusal — because
//! the guard that calls it is MOVED INTO the worker closure, and `std::thread::Builder::spawn`
//! drops the closure it was handed (guard included) when it returns `Err`. [`spawn_guarded`] (a
//! symmetric release that ignores the outcome) and [`spawn_detached`] (genuine fire-and-forget)
//! are thin wrappers. Every production thread in the workspace goes through one of these — the
//! `conventions` test bans raw `thread::spawn`/`thread::Builder` everywhere else.
//!
//! Two constraints on a `done`/`release` closure, from where it may run:
//!   * on the worker-panic path it runs DURING UNWIND, so it must not itself panic (a panic while
//!     unwinding aborts the process);
//!   * on the spawn-refusal path it runs SYNCHRONOUSLY on the calling thread before the spawn
//!     function returns, so it must not require a lock the caller is already holding.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Runs `f` on drop unless already taken. Private: its only sound use is captured inside a
/// worker closure by the helpers below, where the drop-on-spawn-failure property holds.
struct Defer<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Spawn a NAMED worker and deliver its completion to `done` exactly once, on every path:
///   * worker returned `v`  → `done(Some(v))`
///   * worker panicked      → `done(None)` (during unwind)
///   * OS refused the thread → `done(None)`, synchronously, before this returns `false`.
///
/// Returns whether the thread was actually created. The bool is advisory — cleanup has already
/// run by the time you observe `false` — but a caller that returned a status BEFORE spawning
/// (e.g. "compositing") can use it to correct that status.
pub fn spawn_notify<T, W, D>(name: &str, work: W, done: D) -> bool
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
    D: FnOnce(Option<T>) + Send + 'static,
{
    let cell: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
    let guard = {
        let cell = cell.clone();
        Defer(Some(move || {
            let v = cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            done(v);
        }))
    };
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // `_guard` is bound FIRST so it drops LAST — after `work`'s value is stored, so the
            // release reads the real outcome. On a panic in `work` the store never happens and
            // the guard still runs with `None`.
            let _guard = guard;
            let v = work();
            *cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(v);
        })
        .is_ok()
}

/// Spawn a NAMED worker with a `release` that runs on every exit path (completion, panic, spawn
/// refusal) — for the "set a latch before spawn, clear it after" shape. The worker must NOT
/// clear the latch itself.
pub fn spawn_guarded<R, W>(name: &str, release: R, work: W) -> bool
where
    R: FnOnce() + Send + 'static,
    W: FnOnce() + Send + 'static,
{
    spawn_notify(name, work, move |_| release())
}

/// Spawn a NAMED fire-and-forget worker: no latch, no cleanup, nothing to report. The name is
/// still mandatory (per-thread CPU attribution). Spawn refusal is silently ignored — correct
/// only when there is genuinely nothing to undo.
pub fn spawn_detached<W>(name: &str, work: W) -> bool
where
    W: FnOnce() + Send + 'static,
{
    spawn_notify(name, work, |_| {})
}

/// Spawn a NAMED worker and return its `JoinHandle` — for LIFECYCLE threads an owner keeps and
/// `.join()`s on teardown (protocol servers, socket listeners, the paced writer, the LL-hook
/// pump, the macro-host reader/logger). These cannot use the fire-and-forget helpers — losing
/// the handle would break the clean join their Drop/stop depends on — but routing them through
/// here keeps every production thread in one module (so the `conventions` test is a strict
/// single-file allowlist with no per-site escape hatch) and keeps the name mandatory. The caller
/// owns failure handling: a returned `Err` is a spawn refusal, which the owner must tolerate
/// (typically: the feature is simply unavailable, no partial state to unwind).
pub fn spawn_named<T, W>(name: &str, work: W) -> std::io::Result<std::thread::JoinHandle<T>>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(work)
}

/// The resettable slot behind [`service_sender`]: the currently-live worker's sender paired with an
/// "is that worker still alive" flag. A plain `OnceLock<Sender>` cannot express this — it is a
/// one-way latch, so once a worker exits its cached sender feeds a dead receiver FOREVER. Declare
/// one as a `static SLOT: Service<T> = Service::new();`.
pub struct Service<T> {
    inner: Mutex<Option<(Sender<T>, Arc<AtomicBool>)>>,
}

impl<T> Service<T> {
    #[must_use]
    pub const fn new() -> Self {
        Service {
            inner: Mutex::new(None),
        }
    }
}

impl<T> Default for Service<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// A lazily-started, SELF-HEALING singleton SERVICE worker reachable by a channel. The first call
/// starts a NAMED worker draining the receiver and caches the sender; callers get `Some(sender)`.
/// It self-heals from EVERY worker-death path, not just a refused spawn:
///   * OS refuses the thread → nothing cached, the next call retries;
///   * the worker EXITS later (a setup-failure `return`, or a panic that escaped the loop's own
///     [`contain`]/[`drain`]/[`contain_frame`] containment) → its liveness flag drops, and the
///     next lookup re-spawns a fresh worker on a fresh channel.
///
/// This makes the module thesis TOTAL: there is never a cached sender feeding a dead receiver for
/// longer than one lookup (a bare `OnceLock` could strand it for the whole process lifetime — every
/// later `send` vanishing into a dead channel). `None` means the worker can't start right now; the
/// caller should skip the send (and may report it).
///
/// The returned `Sender` is a SNAPSHOT of the currently-live worker — grab-and-send, do NOT cache
/// it across calls (a stored clone can outlive its worker; a fresh lookup always re-checks
/// liveness). Because a re-spawn re-runs the worker's SETUP, that setup must be RE-ENTRANT: it may
/// register a window class (idempotent — a repeated `RegisterClassW` is ignored and the window
/// still creates), but it must not create a single-instance named kernel object it does not also
/// release, and it must NEVER call `service_sender` for its OWN slot (it would deadlock on the lock
/// this holds across the spawn).
pub fn service_sender<T, F>(slot: &'static Service<T>, name: &str, worker: F) -> Option<Sender<T>>
where
    T: Send + 'static,
    F: FnOnce(Receiver<T>) + Send + 'static,
{
    let mut cur = slot
        .inner
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((tx, alive)) = cur.as_ref() {
        if alive.load(Ordering::Acquire) {
            return Some(tx.clone());
        }
        // the worker exited: its `_dead` guard lowered `alive` on return/unwind, AFTER `rx` was
        // dropped — so a dead flag means the receiver is already gone. Fall through to re-spawn.
    }
    let (tx, rx) = mpsc::channel();
    let alive = Arc::new(AtomicBool::new(true));
    let flag = alive.clone();
    // spawn UNDER the lock so the check-and-start is atomic (no double-spawn race). `_dead` is bound
    // FIRST so it drops LAST — after `worker(rx)` fully returns/unwinds and `rx` is gone — hence a
    // caller that later observes `alive == false` sees a worker whose receiver is already dead. The
    // guard runs during unwind too, so it must not panic (an `AtomicBool` store cannot).
    let spawned = spawn_detached(name, move || {
        let _dead = Defer(Some(move || flag.store(false, Ordering::Release)));
        worker(rx);
    });
    if !spawned {
        *cur = None; // spawn refused — cache nothing so the next call retries
        return None;
    }
    *cur = Some((tx.clone(), alive));
    Some(tx)
}

/// Drain `rx` forever, handling each item with `handler`, CONTAINING a panic in any single item
/// so it can never kill the service. The five lazily-started singleton services ([`service_sender`])
/// are infinite loops that only exit on a panic — and their death would be SILENT and PERMANENT
/// (the cached sender feeds a dropped receiver; every later `send` vanishes into a dead channel).
/// A contained panic keeps the loop alive for the next command. The process panic hook still writes
/// the payload + backtrace to the crash log, so a contained panic is diagnosed, not forgiven.
///
/// State held across items in `handler` may be left half-updated by a panicked item
/// (`AssertUnwindSafe`) — for these overlay/device services that is cosmetic (the next command or a
/// redraw corrects it) and a lock must never be held ACROSS `handler` calls (it would poison).
pub fn drain<T, H>(rx: Receiver<T>, name: &str, mut handler: H)
where
    H: FnMut(T),
{
    let mut consecutive = 0u32;
    for item in rx {
        if let Ok(()) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(item))) { consecutive = 0 } else {
            // The panic hook already logged the payload+backtrace; escalate here only to
            // surface a worker STUCK panicking on every item (a deterministic bug), without
            // spamming a line per command.
            consecutive += 1;
            if consecutive == 1 || consecutive.is_multiple_of(16) {
                eprintln!("[worker] {name}: item panicked (contained), {consecutive} in a row");
            }
        }
    }
}

/// Run one unit of work with a panic CONTAINED — for a service whose loop is a Win32 message
/// pump (`while let Ok(cmd) = rx.try_recv() { … }` interleaved with `DispatchMessageW`), where the
/// blocking [`drain`] can't be used. Wrap each command's handling so one bad command (odd geometry,
/// a stale HWND) can't unwind out of the pump loop and kill the overlay service for the run. Same
/// contract as [`drain`]: the panic hook logs the payload; cross-command state may be half-updated
/// by a panicked command (cosmetic for these overlays — the next command/redraw corrects it).
pub fn contain<F: FnOnce()>(name: &str, f: F) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        eprintln!("[worker] {name}: command panicked (contained)");
    }
}

/// [`contain`] for a HOT per-frame lane (the ~60fps render/present a message-pump service runs
/// every iteration — pixel/geometry work, the likeliest panic surface). A deterministic frame
/// panic would fire at frame rate, so the log is THROTTLED (first, then on power-of-two counts);
/// `panics` is a per-render-site counter the caller holds across iterations. A panicked frame is a
/// DROPPED frame — the next tick repaints, which is why containing the render is sound here.
///
/// The message PUMP itself (`DispatchMessageW` → `extern "system"` window procs) is deliberately
/// NOT wrapped: a Rust panic across that ABI boundary ABORTS the process, so `catch_unwind` around
/// it is dead code that would imply a protection it cannot deliver.
pub fn contain_frame<F: FnOnce()>(name: &str, panics: &mut u32, f: F) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).is_err() {
        *panics += 1;
        if *panics == 1 || panics.is_power_of_two() {
            eprintln!("[worker] {name}: frame panicked (contained), {panics}x");
        }
    } else {
        *panics = 0; // a good frame resets the streak
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[test]
    fn done_receives_the_workers_value_on_success() {
        let (tx, rx) = mpsc::channel();
        let ok = spawn_notify("t-ok", || 42u32, move |v| tx.send(v).unwrap());
        assert!(ok);
        assert_eq!(rx.recv().unwrap(), Some(42));
    }

    #[test]
    fn done_receives_none_when_the_worker_panics() {
        let (tx, rx) = mpsc::channel();
        spawn_notify("t-panic", || panic!("boom"), move |v: Option<u32>| tx.send(v).unwrap());
        assert_eq!(rx.recv().unwrap(), None, "a panicking worker must still notify (None)");
    }

    // Handshake, not a sleep. The old shape was `spawn(...); sleep(50ms); assert_eq!(n, 1)`, which
    // under a loaded machine (the full suite runs tests in parallel) lost the race and reported
    // `left: 0, right: 1` — a false failure about the scheduler, not about the code. It was also
    // WEAKER than its own name: a single load 50ms in proves "ran at least once by then", never
    // "exactly once". Blocking on the release's own signal makes the first half deterministic, and
    // a second, expected-to-time-out receive makes the "exactly once" half real.
    #[test]
    fn guarded_release_runs_exactly_once() {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        let (tx, rx) = mpsc::channel();
        spawn_guarded(
            "t-once",
            move || {
                c.fetch_add(1, Ordering::SeqCst);
                let _ = tx.send(());
            },
            || {},
        );
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the guard's release must run on the worker's exit path");
        assert_eq!(n.load(Ordering::SeqCst), 1, "release ran once");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200)).is_err(),
            "release must run EXACTLY once — a second signal means the guard fired twice"
        );
    }

    #[test]
    fn service_sender_starts_once_and_reuses_the_channel() {
        static SLOT: Service<u32> = Service::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        // first call starts the worker draining rx into `seen`
        let tx1 = service_sender(&SLOT, "t-service", move |rx: Receiver<u32>| {
            for v in rx {
                sink.lock().unwrap().push(v);
            }
        })
        .expect("worker starts");
        tx1.send(1).unwrap();
        // second call reuses the SAME live worker (no second spawn)
        let tx2 = service_sender(&SLOT, "t-service", |_rx| unreachable!("must not spawn twice"))
            .expect("cached");
        tx2.send(2).unwrap();
        for _ in 0..2000 {
            if seen.lock().unwrap().len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut got = seen.lock().unwrap().clone();
        got.sort_unstable();
        assert_eq!(got, vec![1, 2]);
    }

    #[test]
    fn service_sender_respawns_after_the_worker_exits() {
        // THE THESIS AS A TEST: a worker that dies (a setup-failure `return`, or a panic in setup
        // outside any contained loop) must NOT strand the service — the next lookup re-spawns.
        static SLOT: Service<u32> = Service::new();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // silence the intentional setup panic below
        let spawns = Arc::new(AtomicUsize::new(0));

        // 1st worker: PANICS in setup (before it could ever drain) — the classic "cached-but-dead".
        let s = spawns.clone();
        let tx_dead = service_sender(&SLOT, "t-heal", move |_rx: Receiver<u32>| {
            s.fetch_add(1, Ordering::SeqCst);
            panic!("setup blew up"); // worker exits immediately; `_dead` lowers the alive flag
        })
        .expect("first spawn returns a sender");
        // let the worker run + die so its liveness flag is lowered before the next lookup.
        for _ in 0..2000 {
            if spawns.load(Ordering::SeqCst) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let _ = tx_dead.send(99); // vanishes into the dead receiver — no panic, just lost
        std::thread::sleep(std::time::Duration::from_millis(20)); // let the flag settle post-unwind
        std::panic::set_hook(prev);

        // 2nd lookup MUST detect the dead worker and re-spawn a fresh, healthy one.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let s2 = spawns.clone();
        let tx_live = service_sender(&SLOT, "t-heal", move |rx: Receiver<u32>| {
            s2.fetch_add(1, Ordering::SeqCst);
            for v in rx {
                sink.lock().unwrap().push(v);
            }
        })
        .expect("re-spawn after death");
        tx_live.send(7).unwrap();
        for _ in 0..2000 {
            if seen.lock().unwrap().as_slice() == [7] {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(*seen.lock().unwrap(), vec![7], "the re-spawned worker delivers");
        assert_eq!(spawns.load(Ordering::SeqCst), 2, "exactly two workers ran (died, then healed)");
    }

    #[test]
    fn drain_survives_a_panicking_item_and_keeps_going() {
        // Silence the default panic hook for the intentional panic below (keeps test output clean).
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let (tx, rx) = mpsc::channel::<u32>();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        spawn_detached("t-drain", move || {
            drain(rx, "t-drain", move |v| {
                // must NOT kill the loop
                assert!(v != 2, "bad item 2");
                sink.lock().unwrap().push(v);
            });
        });
        for v in [1u32, 2, 3] {
            tx.send(v).unwrap();
        }
        drop(tx); // let the loop end so the test is deterministic
        for _ in 0..2000 {
            if seen.lock().unwrap().len() == 2 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::panic::set_hook(prev);
        // 1 and 3 handled; 2 panicked but the loop survived to process 3.
        assert_eq!(*seen.lock().unwrap(), vec![1, 3]);
    }

    #[test]
    fn contain_frame_survives_a_panicking_frame_and_resets_the_streak() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut panics = 0u32;
        // a good frame keeps the streak at zero
        contain_frame("t-frame", &mut panics, || {});
        assert_eq!(panics, 0);
        // a bad frame is contained (does not unwind out) and bumps the streak
        contain_frame("t-frame", &mut panics, || panic!("bad frame"));
        assert_eq!(panics, 1);
        contain_frame("t-frame", &mut panics, || panic!("bad frame again"));
        assert_eq!(panics, 2);
        // a subsequent good frame resets the streak — the throttle re-arms
        contain_frame("t-frame", &mut panics, || {});
        assert_eq!(panics, 0);
        std::panic::set_hook(prev);
    }

    #[test]
    fn detached_runs_the_work() {
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        spawn_detached("t-detach", move || r.store(true, Ordering::SeqCst));
        for _ in 0..2000 {
            if ran.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(ran.load(Ordering::SeqCst));
    }
}
