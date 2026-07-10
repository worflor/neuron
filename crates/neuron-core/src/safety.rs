//! Process-wide runtime safety state.
//!
//! This is the single in-process source of truth for Neuron's live authority:
//! input/process side-effects and device writes. Platform helpers, the GUI, the CLI daemon, and the
//! Macro Host mirror should route through this module instead of each owning a separate flag.

use std::sync::atomic::{AtomicBool, Ordering};

static INPUT_ARMED: AtomicBool = AtomicBool::new(false);
static WRITES_PAUSED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SafetyState {
    pub input_armed: bool,
    pub writes_paused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeMode {
    Observe,
    Device,
    Input,
    Live,
}

impl RuntimeMode {
    pub fn from_state(s: SafetyState) -> Self {
        match (s.writes_paused, s.input_armed) {
            (true, false) => RuntimeMode::Observe,
            (false, false) => RuntimeMode::Device,
            (true, true) => RuntimeMode::Input,
            (false, true) => RuntimeMode::Live,
        }
    }

    pub fn state(self) -> SafetyState {
        match self {
            RuntimeMode::Observe => SafetyState {
                input_armed: false,
                writes_paused: true,
            },
            RuntimeMode::Device => SafetyState {
                input_armed: false,
                writes_paused: false,
            },
            RuntimeMode::Input => SafetyState {
                input_armed: true,
                writes_paused: true,
            },
            RuntimeMode::Live => SafetyState {
                input_armed: true,
                writes_paused: false,
            },
        }
    }
}

pub fn set_input_armed(on: bool) {
    INPUT_ARMED.store(on, Ordering::SeqCst);
}

pub fn input_armed() -> bool {
    INPUT_ARMED.load(Ordering::SeqCst)
}

pub fn set_writes_paused(paused: bool) {
    WRITES_PAUSED.store(paused, Ordering::SeqCst);
}

pub fn writes_paused() -> bool {
    WRITES_PAUSED.load(Ordering::SeqCst)
}

pub fn set_state(state: SafetyState) {
    set_input_armed(state.input_armed);
    set_writes_paused(state.writes_paused);
}

pub fn state() -> SafetyState {
    SafetyState {
        input_armed: input_armed(),
        writes_paused: writes_paused(),
    }
}

pub fn mode() -> RuntimeMode {
    RuntimeMode::from_state(state())
}

pub fn set_mode(mode: RuntimeMode) {
    set_state(mode.state());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `INPUT_ARMED`/`WRITES_PAUSED` are process-global — EVERY test in this binary that touches
    /// safety state (this module, plus `writes.rs`/`intent.rs`/`action.rs`'s own save/restore-style
    /// tests) shares them. `TEST_LOCK` serializes the tests IN THIS MODULE against each other so two
    /// don't interleave their sets; it can't serialize against tests in other files (out of scope for
    /// this change — those files own their own tests), so this module additionally never assumes a
    /// clean starting state: every test snapshots on entry and restores on exit via [`Restore`].
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// RAII restore: snapshot `state()` when acquired, replay the snapshot in `Drop` — so a panicking
    /// `assert!` mid-test still restores the prior globals instead of leaking an armed/paused state
    /// into whatever runs next in this binary. Per the safety invariant that governs this whole test
    /// module (never leave real input armed), `Drop` additionally forces input OFF regardless of what
    /// was snapshotted: writes are restored to their PRIOR value (this module doesn't own that gate's
    /// "resting" state), but input always ends disarmed.
    struct Restore {
        _lock: std::sync::MutexGuard<'static, ()>,
        prior_writes_paused: bool,
    }

    impl Restore {
        fn acquire() -> Self {
            let lock = TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            Restore {
                _lock: lock,
                prior_writes_paused: writes_paused(),
            }
        }
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            set_input_armed(false);
            set_writes_paused(self.prior_writes_paused);
        }
    }

    /// TDD §4.6's state machine, pinned directly against [`RuntimeMode::state`]: each stance's
    /// (writes_paused, input_armed) pair, AND that [`RuntimeMode::from_state`] is its exact inverse —
    /// so a caller that reads back a state via `from_state` always recovers the mode that produced it
    /// (no two modes may share a state, no state may resolve to the wrong mode).
    #[test]
    fn runtime_mode_state_matches_the_tdd_state_machine_and_round_trips() {
        let cases = [
            (RuntimeMode::Observe, SafetyState { input_armed: false, writes_paused: true }),
            (RuntimeMode::Device, SafetyState { input_armed: false, writes_paused: false }),
            (RuntimeMode::Input, SafetyState { input_armed: true, writes_paused: true }),
            (RuntimeMode::Live, SafetyState { input_armed: true, writes_paused: false }),
        ];
        for (mode, expected) in cases {
            assert_eq!(mode.state(), expected, "{mode:?}'s (writes_paused, input_armed) pair");
            assert_eq!(
                RuntimeMode::from_state(expected),
                mode,
                "from_state must invert state() exactly for {mode:?}"
            );
        }
    }

    /// The wiring guarantee: `set_mode` must synchronize BOTH process-global gates atomically enough
    /// that the global getters (`input_armed`/`writes_paused`) and `mode()` all agree with the stance
    /// just set — for every one of the four stances, not just the common ones. No real input is armed
    /// by this: nothing in a unit test binary reads `INPUT_ARMED` to synthesize an actual keystroke, and
    /// the flag is forced back off before this test returns (see [`Restore`]).
    #[test]
    fn set_mode_synchronizes_both_gates_for_every_stance() {
        let _r = Restore::acquire();
        for stance in [RuntimeMode::Observe, RuntimeMode::Device, RuntimeMode::Input, RuntimeMode::Live] {
            set_mode(stance);
            let expected = stance.state();
            assert_eq!(input_armed(), expected.input_armed, "{stance:?} input_armed getter");
            assert_eq!(writes_paused(), expected.writes_paused, "{stance:?} writes_paused getter");
            assert_eq!(state(), expected, "{stance:?} state() getter");
            assert_eq!(mode(), stance, "{stance:?} round-trips through mode()");
        }
    }

    /// Setting the SAME stance twice must be a no-op the second time — no toggling, no flicker. Covers
    /// both a "gates off" stance (Device) and a "gates on" stance (Live) so idempotency isn't accidental
    /// symmetry around one flag value.
    #[test]
    fn set_mode_is_idempotent() {
        let _r = Restore::acquire();
        for mode in [RuntimeMode::Device, RuntimeMode::Live] {
            set_mode(mode);
            let first = state();
            set_mode(mode);
            let second = state();
            assert_eq!(first, second, "re-applying {mode:?} must not change the gates");
            assert_eq!(second, mode.state(), "{mode:?} still holds its defined state after re-apply");
        }
    }

    /// The two gates are INDEPENDENT primitives — `set_input_armed` must never touch `writes_paused`
    /// and vice versa. `set_mode` is the only thing that moves both together; the raw setters below are
    /// exactly what the GUI's separate "pause writes" toggle and "arm input" control call, so if they
    /// leaked into each other those two independent UI controls would secretly couple.
    #[test]
    fn input_and_write_gates_are_independent() {
        let _r = Restore::acquire();
        set_mode(RuntimeMode::Device); // known baseline: writes_paused=false, input_armed=false

        set_writes_paused(true);
        assert!(writes_paused(), "writes_paused reflects the setter");
        assert!(!input_armed(), "flipping writes_paused must not arm input");

        set_input_armed(true);
        assert!(input_armed(), "input_armed reflects the setter");
        assert!(writes_paused(), "arming input must not un-pause writes — the two gates are independent");

        set_input_armed(false);
        assert!(!input_armed());
        assert!(writes_paused(), "disarming input must not touch writes_paused");
    }
}
