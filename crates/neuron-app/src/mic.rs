// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Audio panel glue — thin wrapper over neuron-core's Core-Audio `VolumeCtl`. Resolves both sides of
//! the chain: the default CAPTURE device (mic, preferring a Razer/Seiren) and the default RENDER
//! device (headset / sound card / speakers), reading/writing gain + mute on each. No vendor HID —
//! pure Core Audio, generic over whatever endpoints the OS exposes (no hardcoded PID).

use crate::ui::{AppWindow, State};
use neuron::audio::{self, VolumeCtl};
use slint::ComponentHandle;

/// The resolved capture endpoint id + name (None if no mic).
fn endpoint() -> Option<(String, String)> {
    audio::resolve_capture(None).map(|e| (e.id, e.name))
}

/// The resolved RENDER endpoint id + name (headphones / sound card; None if no output device).
fn render_endpoint() -> Option<(String, String)> {
    audio::resolve_render(None).map(|e| (e.id, e.name))
}

/// Refresh the mic panel from the live endpoint — NAME + GAIN only. `mic-muted` is NOT set here:
/// the launch `mic_mute` reconcile unit owns the initial seed (this early call used to race
/// hidwatch's hardware-mute bridge and read a stale pre-bridge value — the Chunk-B launch bug), and
/// `glue::publish_mic_state` is the one authoritative writer of `mic-muted` everywhere else. The
/// request below re-runs that same unit so the manual "refresh mic" button still refreshes the
/// mute reading — through the one writer, not a second one here.
pub fn refresh(app: &AppWindow) {
    let st = app.global::<State>();
    match endpoint() {
        Some((id, name)) => {
            if let Some(ctl) = VolumeCtl::open(&id) {
                st.set_mic_name(name.into());
                st.set_mic_gain((ctl.get_volume() * 100.0).round());
            } else {
                st.set_mic_name(name.into());
            }
        }
        None => st.set_mic_name("(no capture device)".into()),
    }
    crate::reconcile::request(crate::reconcile::Scope::Unit("mic_mute"));
}

// ── OUTPUT (render) side: headphones / sound card / speakers ────────────────

/// Refresh the output panel from the live render endpoint (the headset / sound-card volume + mute).
pub fn refresh_output(app: &AppWindow) {
    let st = app.global::<State>();
    match render_endpoint() {
        Some((id, name)) => {
            if let Some(ctl) = VolumeCtl::open(&id) {
                st.set_out_name(name.into());
                st.set_out_gain((ctl.get_volume() * 100.0).round());
                st.set_out_muted(ctl.get_mute());
                return;
            }
            st.set_out_name(name.into());
        }
        None => st.set_out_name("(no output device)".into()),
    }
}
