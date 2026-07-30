// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A zero-dependency failpoint registry for deterministic race reproduction.
//!
//! ## Doctrine
//! Concurrency bugs (a waiter that races a cleanup, a respawn that races a request) are normally
//! reproduced by hand: sprinkle `sleep`s at the suspected window and hope the scheduler cooperates,
//! or worse, leave the bug undiscovered because nobody could reliably hit the window. A failpoint
//! makes "the sidecar dies EXACTLY here" a one-line test setup: [`arm`] a named point with an
//! [`Action`], run the code, and the armed thread pauses (or panics, or fails) AT THAT LINE while a
//! second thread advances into the window. The grid of (points × invariants) this enables — kill
//! the reader here, insert here, respawn here, assert the invariant still holds — replaces
//! hand-crafted, non-deterministic race repros with deterministic ones.
//!
//! ## Cost model
//! [`failpoint!`] must cost NOTHING in a release build: it expands to a literal no-op — not a
//! branch, not a load, not a symbol — because its body is wrapped in
//! `#[cfg(any(test, feature = "failpoints"))]`, and that whole block (including every path it
//! references) is deleted by the compiler outside test/failpoints builds. This module is written
//! so the OUTER `pub mod failpoint;` line in `lib.rs` can stay unconditional (needed so the macro
//! name itself always resolves, even at call sites that are never cfg-gated by hand) while every
//! byte of actual STATE — the registry, the hit counters, `arm`/`disarm` — lives behind that same
//! cfg and simply does not exist in a release binary.
//!
//! In test/failpoints builds, the cost of an UNARMED call site is one relaxed atomic load: see
//! [`GENERATION`]. Nothing has ever been armed in the overwhelming majority of the test suite, so
//! the overwhelming majority of failpoint call sites never touch the registry's mutex at all.
//!
//! Note on gating: the two macros below are defined UNCONDITIONALLY (so `failpoint!(...)` always
//! resolves as a macro name, even at call sites nobody hand-wraps in `#[cfg(...)]`), but every
//! byte of actual STATE they touch — this whole registry — is behind
//! `#[cfg(any(test, feature = "failpoints"))]` and simply does not exist in a release binary; the
//! macros' own bodies carry the identical cfg, so the two stay consistent by construction.

#[cfg(any(test, feature = "failpoints"))]
use std::collections::HashMap;
#[cfg(any(test, feature = "failpoints"))]
use std::sync::atomic::{AtomicUsize, Ordering};
#[cfg(any(test, feature = "failpoints"))]
use std::sync::{Mutex, OnceLock};
#[cfg(any(test, feature = "failpoints"))]
use std::time::Duration;

/// What a registered failpoint does when its call site is reached.
#[cfg(any(test, feature = "failpoints"))]
#[derive(Clone, Debug)]
pub enum Action {
    /// Panic immediately — simulates a thread dying (or a hard bug) exactly at this point.
    Panic,
    /// Sleep for `Duration`, then continue normally. The tool for FREEZING one thread at a race
    /// window edge while a second thread (not sleeping) advances past it.
    Sleep(Duration),
    /// Fail this operation. Only meaningful at a point reachable through [`failpoint_result!`]
    /// (a point that can early-return an error); a bare [`failpoint!`] treats it as a no-op,
    /// since there is nothing for a `()`-returning call site to fail *into*.
    Fail,
}

/// Bumped by every [`arm`]/[`disarm`]/[`disarm_all`] call, NEVER reset back down (arming is a
/// one-way latch on "has this process ever used a failpoint"). [`failpoint!`]'s fast path is a
/// single relaxed load of this counter: 0 means nothing has EVER been armed anywhere in the
/// process, and the call site returns immediately without touching the registry's mutex — the
/// steady-state cost for the whole ordinary test suite (which never arms anything) is one atomic
/// load per call site, no lock, no hashmap.
#[cfg(any(test, feature = "failpoints"))]
static GENERATION: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(test, feature = "failpoints"))]
struct Registry {
    /// name -> the action to take when that name's call site is next reached.
    armed: Mutex<HashMap<&'static str, Action>>,
    /// name -> how many times that name's call site has been reached (while GENERATION > 0, i.e.
    /// on the slow path — see [`hits`]). The anti-vacuity guard: a chaos test that arms a point
    /// its own exercise never actually reaches is a vacuous test, and `hits(name) == 0` catches it.
    hits: Mutex<HashMap<&'static str, usize>>,
}

#[cfg(any(test, feature = "failpoints"))]
static REGISTRY: OnceLock<Registry> = OnceLock::new();

#[cfg(any(test, feature = "failpoints"))]
fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry {
        armed: Mutex::new(HashMap::new()),
        hits: Mutex::new(HashMap::new()),
    })
}

/// Arm `name`: the next (and every subsequent, until [`disarm`]) time its call site runs, it takes
/// `action`. Resets `name`'s hit counter to 0, so a fresh arm starts a fresh "was it reached?"
/// window — a residual count from an earlier test (or an earlier arm/disarm cycle of the same
/// point) can never leak into this one's assertions.
#[cfg(any(test, feature = "failpoints"))]
pub fn arm(name: &'static str, action: Action) {
    let reg = registry();
    reg.armed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name, action);
    reg.hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name, 0);
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Disarm `name`: its call site goes back to being a no-op. Its hit counter is left intact (a test
/// can disarm — e.g. via [`Armed`]'s `Drop` — and still assert on `hits(name)` afterward).
#[cfg(any(test, feature = "failpoints"))]
pub fn disarm(name: &'static str) {
    registry()
        .armed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(name);
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// Disarm every point AND clear every hit counter — full reset, for test teardown/isolation.
#[cfg(any(test, feature = "failpoints"))]
pub fn disarm_all() {
    let reg = registry();
    reg.armed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    reg.hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    GENERATION.fetch_add(1, Ordering::SeqCst);
}

/// How many times `name`'s call site has been reached since it was last [`arm`]ed (0 if it has
/// never been armed, or was armed but never reached — the vacuous-test case this exists to catch).
#[cfg(any(test, feature = "failpoints"))]
pub fn hits(name: &'static str) -> usize {
    *registry()
        .hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .unwrap_or(&0)
}

/// RAII handle for an armed failpoint: [`disarm`]s `name` on drop, including on an unwinding panic
/// (a failed assertion in the middle of a chaos test must never leak an armed point into whatever
/// test the process runs next). Construct with [`Armed::new`]; the returned guard's only job is to
/// live at least as long as the window under test.
#[cfg(any(test, feature = "failpoints"))]
#[must_use = "the failpoint disarms as soon as this guard drops"]
pub struct Armed(&'static str);

#[cfg(any(test, feature = "failpoints"))]
impl Armed {
    /// Arm `name` with `action` and return a guard that disarms it on drop.
    pub fn new(name: &'static str, action: Action) -> Self {
        arm(name, action);
        Armed(name)
    }
}

#[cfg(any(test, feature = "failpoints"))]
impl Drop for Armed {
    fn drop(&mut self) {
        disarm(self.0);
    }
}

/// Internal: consulted only by [`failpoint!`]/[`failpoint_result!`] after their fast-path check has
/// already established `GENERATION > 0`. Counts the hit unconditionally (whether or not `name`
/// itself is currently armed — a point can be "reached" without an action being attached to it,
/// which is still useful signal), then returns the armed [`Action`] if any.
#[cfg(any(test, feature = "failpoints"))]
#[doc(hidden)]
pub fn __check(name: &'static str) -> Option<Action> {
    let reg = registry();
    *reg.hits
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(name)
        .or_insert(0) += 1;
    reg.armed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

/// Internal: the fast-path gate shared by both macros — a single relaxed load, no lock, in the
/// common case where nothing has ever been armed.
#[cfg(any(test, feature = "failpoints"))]
#[doc(hidden)]
pub fn __gen_is_zero() -> bool {
    GENERATION.load(Ordering::Relaxed) == 0
}

/// Mark a point in the code as a failpoint. In a release build (no `test` cfg, no `failpoints`
/// feature) this expands to nothing — not even a branch. In a test/failpoints build, it consults
/// the global registry: unarmed (the overwhelming common case) it is a single relaxed atomic load
/// and nothing else; armed, it runs the registered [`Action`] ([`Action::Fail`] is a no-op here —
/// use [`failpoint_result!`] at a point that can actually return an error).
#[macro_export]
macro_rules! failpoint {
    ($name:expr) => {{
        #[cfg(any(test, feature = "failpoints"))]
        {
            if !$crate::failpoint::__gen_is_zero() {
                if let Some(action) = $crate::failpoint::__check($name) {
                    match action {
                        $crate::failpoint::Action::Panic => {
                            panic!("failpoint '{}' fired: Panic", $name)
                        }
                        $crate::failpoint::Action::Sleep(d) => std::thread::sleep(d),
                        $crate::failpoint::Action::Fail => {}
                    }
                }
            }
        }
    }};
}

/// Like [`failpoint!`], for a point that can early-return an error. `$err` is evaluated (and
/// returned from the ENCLOSING function via a bare `return`) only when the point is armed with
/// [`Action::Fail`]; [`Action::Panic`]/[`Action::Sleep`] behave exactly as in [`failpoint!`].
/// Zero cost in release for the same reason as `failpoint!` — the whole body is cfg-gated out.
#[macro_export]
macro_rules! failpoint_result {
    ($name:expr, $err:expr) => {{
        #[cfg(any(test, feature = "failpoints"))]
        {
            if !$crate::failpoint::__gen_is_zero() {
                if let Some(action) = $crate::failpoint::__check($name) {
                    match action {
                        $crate::failpoint::Action::Panic => {
                            panic!("failpoint '{}' fired: Panic", $name)
                        }
                        $crate::failpoint::Action::Sleep(d) => std::thread::sleep(d),
                        $crate::failpoint::Action::Fail => return $err,
                    }
                }
            }
        }
    }};
}

/// Serializes every test that touches the PROCESS-GLOBAL failpoint registry. The armed map, the
/// hit counters, and the generation are one shared table; `disarm_all()` (exercised by the
/// concurrency test) wipes it wholesale, so two failpoint-using tests running on cargo's default
/// parallel harness would corrupt each other's arm/hit expectations — and a failpoint-USING
/// integration test (the macro-host death-race tests) would see its armed sleep vanish mid-race.
/// Every test that arms/disarms/reaches a failpoint holds this first (the `fail` crate does the
/// same). Poison-tolerant. Death-race tests take `SIDECAR_TEST_LOCK` before this one; nothing takes
/// them in the opposite order, so the fixed acquisition order is deadlock-free.
#[cfg(test)]
pub(crate) static FAILPOINT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// Acquire the global failpoint-registry test lock (poison-tolerant). Held for a test's whole
    /// body so no concurrent test mutates the shared armed/hit table underneath it.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        FAILPOINT_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Silence the default panic hook for a call known to panic intentionally, run it inside
    /// `catch_unwind`, then restore the hook — the shared pattern used elsewhere in this crate
    /// (see `worker.rs`'s tests) for keeping deliberate-panic tests' output clean.
    fn catches<F: FnOnce() + std::panic::UnwindSafe>(f: F) -> bool {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::panic::catch_unwind(f);
        std::panic::set_hook(prev);
        result.is_err()
    }

    #[test]
    fn armed_panic_fires_and_counts_a_hit() {
        let _g = guard();
        let name = "failpoint.tests.armed_panic_fires";
        let _g = Armed::new(name, Action::Panic);
        let before = hits(name);
        let panicked = catches(|| failpoint!(name));
        assert!(panicked, "an Action::Panic failpoint must panic when reached");
        assert_eq!(hits(name), before + 1, "a reached failpoint must count exactly one hit");
    }

    #[test]
    fn raii_disarm_restores_the_no_op_behavior() {
        let _g = guard();
        let name = "failpoint.tests.raii_disarm_restores";
        {
            let _g = Armed::new(name, Action::Panic);
            assert!(catches(|| failpoint!(name)), "armed: must fire");
        } // guard drops here -> disarm(name)
        // disarmed: must be silent now, even though GENERATION stays > 0 process-wide.
        let before = hits(name);
        failpoint!(name); // must NOT panic
        assert_eq!(
            hits(name),
            before + 1,
            "a disarmed point is still REACHED (counts a hit) — it just takes no action"
        );
    }

    #[test]
    fn hits_counts_reaches_not_arms() {
        let _g = guard();
        let name = "failpoint.tests.hits_counts_reaches";
        let _g = Armed::new(name, Action::Sleep(Duration::from_millis(0)));
        assert_eq!(hits(name), 0, "arm() resets the counter — nothing reached yet");
        failpoint!(name);
        failpoint!(name);
        failpoint!(name);
        assert_eq!(hits(name), 3, "three reaches while armed must count as three hits");
    }

    #[test]
    fn unarmed_point_is_always_silent() {
        // Never armed (a name unique to this test): whichever path serves it — the true
        // GENERATION==0 fast path if this happens to be the first failpoint touched in the
        // process, or the slow path if some other test already armed something first — the
        // functional contract is identical: a call site nobody armed does nothing observable.
        let _g = guard();
        let name = "failpoint.tests.unarmed_point_is_always_silent";
        let panicked = catches(|| failpoint!(name));
        assert!(!panicked, "an unarmed failpoint must never panic");
    }

    #[test]
    fn failpoint_result_returns_the_caller_supplied_error_on_fail() {
        let _g = guard();
        let name = "failpoint.tests.failpoint_result_fails";
        fn probe(name: &'static str) -> Result<u32, String> {
            failpoint_result!(name, Err("injected".to_string()));
            Ok(7)
        }
        assert_eq!(probe(name), Ok(7), "unarmed: the real return value must pass through");
        let _g = Armed::new(name, Action::Fail);
        assert_eq!(
            probe(name),
            Err("injected".to_string()),
            "Action::Fail must early-return the caller-supplied error expression"
        );
    }

    #[test]
    fn concurrent_arm_disarm_does_not_deadlock() {
        // Hammer arm/disarm/hits/disarm_all from many threads on a handful of shared names —
        // the property under test is liveness (every thread finishes, joins cleanly), not any
        // particular interleaving's outcome.
        let _g = guard();
        let names: &[&'static str] = &[
            "failpoint.tests.concurrent.a",
            "failpoint.tests.concurrent.b",
            "failpoint.tests.concurrent.c",
        ];
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    let name = names[i % names.len()];
                    for _ in 0..200 {
                        arm(name, Action::Sleep(Duration::from_millis(0)));
                        let _ = hits(name);
                        failpoint!(name);
                        disarm(name);
                    }
                    disarm_all();
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread must not panic");
        }
    }
}
