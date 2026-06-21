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

/// Refresh the mic panel from the live endpoint.
pub fn refresh(app: &AppWindow) {
    let st = app.global::<State>();
    match endpoint() {
        Some((id, name)) => {
            if let Some(ctl) = VolumeCtl::open(&id) {
                st.set_mic_name(name.into());
                st.set_mic_gain((ctl.get_volume() * 100.0).round());
                st.set_mic_muted(ctl.get_mute());
                return;
            }
            st.set_mic_name(name.into());
        }
        None => st.set_mic_name("(no capture device)".into()),
    }
}

/// Set absolute gain (0..100 %). `None` = no capture endpoint / open failed — the gain did NOT
/// change, and the caller must say so instead of asserting success.
pub fn set_gain(pct: f32) -> Option<f32> {
    let (id, _) = endpoint()?;
    let ctl = VolumeCtl::open(&id)?;
    ctl.set_volume((pct / 100.0).clamp(0.0, 1.0));
    Some(pct)
}

/// Toggle mute, returning the new muted state. `None` = no endpoint (nothing flipped).
pub fn toggle_mute() -> Option<bool> {
    let (id, _) = endpoint()?;
    let ctl = VolumeCtl::open(&id)?;
    Some(ctl.toggle_mute())
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

/// Set absolute output volume (0..100 %). `None` = no render endpoint (volume unchanged).
pub fn set_output_gain(pct: f32) -> Option<f32> {
    let (id, _) = render_endpoint()?;
    let ctl = VolumeCtl::open(&id)?;
    ctl.set_volume((pct / 100.0).clamp(0.0, 1.0));
    Some(pct)
}

/// Toggle output mute, returning the new muted state. `None` = no endpoint (nothing flipped).
pub fn toggle_output_mute() -> Option<bool> {
    let (id, _) = render_endpoint()?;
    let ctl = VolumeCtl::open(&id)?;
    Some(ctl.toggle_mute())
}
