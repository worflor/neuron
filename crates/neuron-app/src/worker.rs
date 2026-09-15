// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The named-worker primitives live in neuron-core (`neuron::worker`) so every crate shares one
//! implementation and the `conventions` test can allowlist a single file. This re-export keeps
//! `crate::worker::spawn_*` spellable from the app.

#[cfg(windows)]
pub use neuron::worker::{contain, contain_frame};
pub use neuron::worker::{
    drain, service_sender, spawn_detached, spawn_guarded, spawn_named,
    spawn_notify, Service,
};
