// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use neuron::audio::VolumeCtl;
pub use neuron::session_undo::{AudioValue, Entry};

pub fn record_profile(before: String, applied: String, skipped: &[String], gated: &[String]) {
    if let Some(entry) = neuron::session_undo::verified_profile_entry(before, applied, skipped, gated) {
        neuron::session_undo::push(entry);
    }
}

pub fn record_lighting(commit: crate::glue::LightingCommit) {
    neuron::session_undo::push(Entry::Lighting { pid: commit.pid, unit: commit.unit, before: commit.before, applied: commit.after });
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
