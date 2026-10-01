// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use neuron::audio::{Flow, VolumeCtl};
pub use neuron::session_undo::{AudioValue, Entry};

pub fn record_profile(before: String, applied: String) {
    if let Some(entry) = neuron::session_undo::profile_entry(before, applied) {
        neuron::session_undo::push(entry);
    }
}

pub fn record_lighting(commit: crate::glue::LightingCommit) {
    neuron::session_undo::push(Entry::Lighting { pid: commit.pid, unit: commit.unit, before: commit.before, applied: commit.after });
}

pub fn run_host_action(action: &neuron::action::Action) -> Option<String> {
    use neuron::action::Action;
    match action {
        Action::MicGain { .. } | Action::MicGainSet { .. } | Action::OutputGain { .. }
        | Action::MicMute { .. } | Action::OutputMute { .. } => Some(apply_audio(action)),
        _ => None,
    }
}

fn apply_audio(action: &neuron::action::Action) -> String {
    let result = neuron::session_undo::apply_audio_action(
        action,
        neuron::safety::input_armed(),
        |flow, name| {
            let endpoint = match flow { Flow::Capture => neuron::audio::resolve_capture(name), Flow::Render => neuron::audio::resolve_render(name) };
            Ok(endpoint.map(|endpoint| (endpoint.id, endpoint.name)))
        },
        |id, _flow, kind| {
            let ctl = VolumeCtl::open(id).ok_or_else(|| "endpoint unavailable".to_string())?;
            let value = match kind {
                neuron::session_undo::AudioValueKind::Volume => ctl.try_get_volume().map(AudioValue::Volume),
                neuron::session_undo::AudioValueKind::Mute => ctl.try_get_mute().map(AudioValue::Mute),
            };
            value.ok_or_else(|| "endpoint read failed".into())
        },
        |id, _flow, value| {
            if !neuron::safety::input_armed() { return Err("audio action is disarmed".into()); }
            let ctl = VolumeCtl::open(id).ok_or_else(|| "endpoint unavailable".to_string())?;
            let ok = match value { AudioValue::Volume(value) => ctl.set_volume(*value), AudioValue::Mute(value) => ctl.set_mute(*value) };
            if ok { Ok(()) } else { Err("endpoint setter failed".into()) }
        },
    );
    match result {
        Ok(neuron::session_undo::AudioActionResult::Unchanged { name }) => format!("{name} unchanged"),
        Ok(neuron::session_undo::AudioActionResult::Changed { id, flow, name, before, applied }) => {
            neuron::session_undo::push(Entry::Audio { id, flow, before, applied: applied.clone() });
            match applied {
                AudioValue::Volume(value) => format!("{name} {} -> {}%", if flow == Flow::Capture { "gain" } else { "vol" }, (value * 100.0).round() as i32),
                AudioValue::Mute(value) => format!("{name} mute -> {}", if value { "ON" } else { "off" }),
            }
        }
        Err(error) => error,
    }
}

pub fn undo_audio(entry: &Entry) -> Result<(), String> {
    let Entry::Audio { id, flow: _, before: _, applied } = entry else { return Err("undo entry is not audio".into()); };
    if !neuron::safety::input_armed() { return Err("undo is disarmed".into()); }
    let ctl = VolumeCtl::open(id).ok_or_else(|| format!("audio endpoint '{id}' is unavailable"))?;
    let read = || match applied {
        AudioValue::Volume(_) => ctl.try_get_volume().map(AudioValue::Volume).map(neuron::session_undo::State::Audio),
        AudioValue::Mute(_) => ctl.try_get_mute().map(AudioValue::Mute).map(neuron::session_undo::State::Audio),
    }.ok_or_else(|| format!("audio endpoint '{id}' could not be read"));
    neuron::session_undo::restore_transaction(entry, read, |state| {
        if !neuron::safety::input_armed() { return Err("undo is disarmed".into()); }
        let neuron::session_undo::State::Audio(value) = state else { return Err("undo entry is not audio".into()); };
        let ok = match value { AudioValue::Volume(value) => ctl.set_volume(*value), AudioValue::Mute(value) => ctl.set_mute(*value) };
        if ok { Ok(()) } else { Err(format!("audio endpoint '{id}' restore failed")) }
    }).map_err(|e| format!("audio endpoint '{id}': {e}"))
}
