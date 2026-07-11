//! The named-worker primitives live in neuron-core (`neuron::worker`) so every crate shares one
//! implementation and the `conventions` test can allowlist a single file. This re-export keeps
//! `crate::worker::spawn_*` spellable from the app.

pub use neuron::worker::{
    contain, contain_frame, drain, service_sender, spawn_detached, spawn_guarded, spawn_named,
    spawn_notify, Service,
};
