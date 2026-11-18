// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Mic-mute refresh: re-runs the `mic_mute` reconcile unit, whose one authoritative writer is
//! `glue::publish_mic_state`.

/// Re-read the default capture endpoint's OS mute through the `mic_mute` reconcile unit (the
/// launch seed calls it).
pub fn refresh() {
    crate::reconcile::request(crate::reconcile::Scope::Unit("mic_mute"));
}
