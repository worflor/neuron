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
