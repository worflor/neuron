// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

use crate::audio::Flow;
use crate::action::Action;
use crate::pattern::LayerDef;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

pub const LIMIT: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub enum AudioValue {
    Volume(f32),
    Mute(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    Audio { id: String, flow: Flow, before: AudioValue, applied: AudioValue },
    Profile { before: String, applied: String },
    Lighting { pid: u16, unit: String, before: Vec<LayerDef>, applied: Vec<LayerDef> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    pub id: u64,
    pub entry: Entry,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Audio(AudioValue),
    Profile(String),
    Lighting(Vec<LayerDef>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AudioActionResult {
    Changed { id: String, flow: Flow, name: String, before: AudioValue, applied: AudioValue },
    Unchanged { name: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioValueKind { Volume, Mute }

/// Build a reversible profile entry only when both sides identify a concrete active profile.
#[must_use]
pub fn profile_entry(before: String, applied: String) -> Option<Entry> {
    if before.is_empty() || before == applied { return None; }
    Some(Entry::Profile { before, applied })
}

/// Apply a mic/output gain or mute action through the host's trusted audio seam. `armed` is the
/// process safety state supplied by a host; tests pass a local value to a fake backend and never
/// arm the process-wide input gate.
pub fn apply_audio_action(
    action: &Action,
    armed: bool,
    mut resolve: impl FnMut(Flow, Option<&str>) -> Result<Option<(String, String)>, String>,
    mut read: impl FnMut(&str, Flow, AudioValueKind) -> Result<AudioValue, String>,
    mut write: impl FnMut(&str, Flow, &AudioValue) -> Result<(), String>,
) -> Result<AudioActionResult, String> {
    if !armed { return Err("audio action is disarmed".into()); }
    let (flow, device, request) = match action {
        Action::MicMute { device, mode } => (Flow::Capture, device.as_deref(), AudioRequest::Mute(mode)),
        Action::OutputMute { device, mode } => (Flow::Render, device.as_deref(), AudioRequest::Mute(mode)),
        Action::MicGain { device, delta_pct } => (Flow::Capture, device.as_deref(), AudioRequest::Nudge(*delta_pct)),
        Action::OutputGain { device, delta_pct } => (Flow::Render, device.as_deref(), AudioRequest::Nudge(*delta_pct)),
        Action::MicGainSet { device, pct } => (Flow::Capture, device.as_deref(), AudioRequest::Absolute(*pct)),
        _ => return Err("action is not an audio mutation".into()),
    };
    let kind = match request { AudioRequest::Mute(_) => AudioValueKind::Mute, AudioRequest::Nudge(_) | AudioRequest::Absolute(_) => AudioValueKind::Volume };
    let endpoint = resolve(flow, device)?.ok_or_else(|| if flow == Flow::Capture { "no mic" } else { "no output device" }.to_string())?;
    let before = read(&endpoint.0, flow, kind).map_err(|e| format!("endpoint read failed: {e}"))?;
    let target = match (request, &before) {
        (AudioRequest::Mute(mode), AudioValue::Mute(current)) => AudioValue::Mute(match mode {
            "toggle" => !current,
            "on" => true,
            "off" => false,
            _ => return Err("invalid mute mode".into()),
        }),
        (AudioRequest::Nudge(delta), AudioValue::Volume(current)) if delta.is_finite() && current.is_finite() => AudioValue::Volume((current + delta / 100.0).clamp(0.0, 1.0)),
        (AudioRequest::Absolute(pct), AudioValue::Volume(current)) if pct.is_finite() && (0.0..=100.0).contains(&pct) && current.is_finite() => AudioValue::Volume(pct / 100.0),
        (AudioRequest::Nudge(_) | AudioRequest::Absolute(_), AudioValue::Volume(_)) => return Err("invalid gain or endpoint value".into()),
        _ => return Err("audio endpoint returned the wrong value kind".into()),
    };
    if same_value(&before, &target) { return Ok(AudioActionResult::Unchanged { name: endpoint.1 }); }
    write(&endpoint.0, flow, &target).map_err(|e| format!("endpoint write failed: {e}"))?;
    let applied = read(&endpoint.0, flow, kind).map_err(|e| format!("endpoint read-back failed: {e}"))?;
    if !same_value(&applied, &target) { return Err("endpoint read-back mismatch".into()); }
    Ok(AudioActionResult::Changed { id: endpoint.0, flow, name: endpoint.1, before, applied })
}

#[derive(Clone, Copy)]
enum AudioRequest<'a> {
    Mute(&'a str),
    Nudge(f32),
    Absolute(f32),
}

impl Entry {
    fn states(&self) -> (State, State) {
        match self {
            Self::Audio { before, applied, .. } => (State::Audio(before.clone()), State::Audio(applied.clone())),
            Self::Profile { before, applied } => (State::Profile(before.clone()), State::Profile(applied.clone())),
            Self::Lighting { before, applied, .. } => (State::Lighting(before.clone()), State::Lighting(applied.clone())),
        }
    }
}

static JOURNAL: OnceLock<Mutex<Journal>> = OnceLock::new();

#[derive(Default)]
pub struct Journal {
    entries: VecDeque<Record>,
    next_id: u64,
}

impl Journal {
    pub fn push(&mut self, entry: Entry) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let id = self.next_id;
        self.entries.push_back(Record { id, entry });
        while self.entries.len() > LIMIT { self.entries.pop_front(); }
        id
    }

    pub fn peek(&self) -> Option<Record> { self.entries.back().cloned() }

    pub fn complete(&mut self, id: u64) {
        if let Some(index) = self.entries.iter().position(|entry| entry.id == id) { self.entries.remove(index); }
    }

    pub fn clear(&mut self) { self.entries.clear(); }

    pub fn len(&self) -> usize { self.entries.len() }
}

fn journal() -> &'static Mutex<Journal> {
    JOURNAL.get_or_init(|| Mutex::new(Journal { entries: VecDeque::with_capacity(LIMIT), next_id: 0 }))
}

pub fn push(entry: Entry) -> u64 {
    journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(entry)
}

pub fn peek() -> Option<Record> {
    journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner).peek()
}

pub fn complete(id: u64) {
    journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner).complete(id);
}

pub fn clear() {
    journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
}

pub fn len() -> usize { journal().lock().unwrap_or_else(std::sync::PoisonError::into_inner).len() }

#[must_use]
pub fn same_value(a: &AudioValue, b: &AudioValue) -> bool {
    match (a, b) {
        (AudioValue::Volume(a), AudioValue::Volume(b)) => a.is_finite() && b.is_finite() && (a - b).abs() <= 0.005,
        (AudioValue::Mute(a), AudioValue::Mute(b)) => a == b,
        _ => false,
    }
}

/// Restore one recorded mutation only while its resource still matches the committed value.
/// The host binds `read` and `write` to the concrete endpoint/profile/device in the entry.
pub fn restore_transaction(
    entry: &Entry,
    mut read: impl FnMut() -> Result<State, String>,
    mut write: impl FnMut(&State) -> Result<(), String>,
) -> Result<(), String> {
    let (before, applied) = entry.states();
    let current = read()?;
    if !same_state(&current, &applied) {
        return Err("resource changed since the recorded action".into());
    }
    write(&before)?;
    let restored = read()?;
    if !same_state(&restored, &before) {
        return Err("restored value did not verify".into());
    }
    Ok(())
}

fn same_state(a: &State, b: &State) -> bool {
    match (a, b) {
        (State::Audio(a), State::Audio(b)) => same_value(a, b),
        (State::Profile(a), State::Profile(b)) => a == b,
        (State::Lighting(a), State::Lighting(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    #[test]
    fn journal_is_bounded_and_keeps_only_the_newest_twenty() {
        let mut journal = Journal::default();
        for i in 0..25 {
            journal.push(Entry::Profile { before: format!("p{i}"), applied: format!("p{}", i + 1) });
        }
        assert_eq!(journal.len(), LIMIT);
        let last = journal.peek().unwrap();
        assert_eq!(last.entry, Entry::Profile { before: "p24".into(), applied: "p25".into() });
        journal.complete(last.id);
        assert_eq!(journal.peek().unwrap().entry, Entry::Profile { before: "p23".into(), applied: "p24".into() });
    }

    #[test]
    fn completing_an_older_successful_restore_preserves_a_newer_change() {
        let mut journal = Journal::default();
        let older = journal.push(Entry::Profile { before: "a".into(), applied: "b".into() });
        let newer = journal.push(Entry::Profile { before: "b".into(), applied: "c".into() });
        journal.complete(older);
        assert_eq!(journal.len(), 1);
        assert_eq!(journal.peek().unwrap().id, newer);
    }

    #[test]
    fn profile_entry_requires_a_concrete_prior_profile_and_a_real_change() {
        assert!(profile_entry(String::new(), "game".into()).is_none());
        assert!(profile_entry("desktop".into(), "desktop".into()).is_none());
        assert_eq!(profile_entry("desktop".into(), "game".into()), Some(Entry::Profile { before: "desktop".into(), applied: "game".into() }));
    }

    #[test]
    fn audio_values_compare_with_quantization_and_reject_stale_or_nonfinite_values() {
        assert!(same_value(&AudioValue::Volume(0.501), &AudioValue::Volume(0.504)));
        assert!(!same_value(&AudioValue::Volume(0.50), &AudioValue::Volume(0.52)));
        assert!(!same_value(&AudioValue::Volume(f32::NAN), &AudioValue::Volume(f32::NAN)));
        assert!(!same_value(&AudioValue::Mute(true), &AudioValue::Mute(false)));
    }

    #[test]
    fn restore_transaction_checks_stale_state_and_read_write_failures_before_journal_pop() {
        let entry = Entry::Audio {
            id: "endpoint-id".into(), flow: Flow::Capture,
            before: AudioValue::Volume(0.4), applied: AudioValue::Volume(0.7),
        };
        let current = std::cell::RefCell::new(State::Audio(AudioValue::Volume(0.9)));
        assert!(restore_transaction(&entry, || Ok(current.borrow().clone()), |_| panic!("stale state must not write")).is_err());
        *current.borrow_mut() = State::Audio(AudioValue::Volume(0.7));
        assert!(restore_transaction(&entry, || Err("endpoint missing".into()), |_| Ok(())).is_err());
        assert!(restore_transaction(&entry, || Ok(current.borrow().clone()), |_| Err("setter failed".into())).is_err());
        assert!(restore_transaction(&entry, || Ok(current.borrow().clone()), |_| Ok(())).is_err(), "a successful setter with stale read-back is not success");

        let mut journal = Journal::default();
        let record = journal.push(entry.clone());
        restore_transaction(&entry, || Ok(current.borrow().clone()), |state| { *current.borrow_mut() = state.clone(); Ok(()) }).unwrap();
        assert_eq!(journal.peek().unwrap().id, record, "the transaction does not mutate the journal itself");
        journal.complete(record);
        assert_eq!(journal.len(), 0);
    }

    #[test]
    fn audio_mutation_uses_trusted_reads_and_emits_only_verified_changed_receipts() {
        use std::cell::RefCell;
        let state = RefCell::new(AudioValue::Volume(0.4));
        let writes = RefCell::new(0usize);
        let mut resolve = |flow: Flow, name: Option<&str>| Ok(Some(("stable-endpoint-id".into(), format!("{} {}", flow.label(), name.unwrap_or("default")))));
        let mut read = |id: &str, _flow: Flow, _kind: AudioValueKind| {
            assert_eq!(id, "stable-endpoint-id");
            Ok(state.borrow().clone())
        };
        let mut write = |id: &str, _flow: Flow, value: &AudioValue| {
            assert_eq!(id, "stable-endpoint-id");
            *writes.borrow_mut() += 1;
            *state.borrow_mut() = value.clone();
            Ok(())
        };
        let action = Action::MicGainSet { device: None, pct: 70.0 };
        let result = apply_audio_action(&action, true, &mut resolve, &mut read, &mut write).unwrap();
        let AudioActionResult::Changed { id, before, applied, .. } = result else { panic!("expected a committed edit") };
        assert_eq!(id, "stable-endpoint-id");
        assert!(same_value(&before, &AudioValue::Volume(0.4)));
        assert!(same_value(&applied, &AudioValue::Volume(0.7)));
        assert_eq!(*writes.borrow(), 1);

        let no_op = apply_audio_action(
            &Action::MicGainSet { device: None, pct: 70.0 }, true,
            |_, _| Ok(Some(("stable-endpoint-id".into(), "mic".into()))),
            |_, _, _| Ok(state.borrow().clone()),
            |_, _, _| { *writes.borrow_mut() += 1; Ok(()) },
        ).unwrap();
        assert_eq!(no_op, AudioActionResult::Unchanged { name: "mic".into() });
        assert_eq!(*writes.borrow(), 1, "no-op does not call the setter or produce a receipt");
    }

    #[test]
    fn audio_mutation_refuses_disarmed_missing_unreadable_failed_and_unverified_backends() {
        use std::cell::RefCell;
        let action = Action::OutputGain { device: None, delta_pct: 10.0 };
        assert!(apply_audio_action(&action, false, |_, _| panic!("disarmed must short-circuit"), |_, _, _| panic!(), |_, _, _| panic!()).is_err());
        assert!(apply_audio_action(&action, true, |_, _| Ok(None), |_, _, _| panic!(), |_, _, _| panic!()).is_err());
        assert!(apply_audio_action(&action, true, |_, _| Ok(Some(("id".into(), "speaker".into()))), |_, _, _| Err("getter failed".into()), |_, _, _| panic!()).is_err());

        let state = RefCell::new(AudioValue::Volume(0.5));
        assert!(apply_audio_action(
            &action, true,
            |_: Flow, _: Option<&str>| Ok(Some(("id".into(), "speaker".into()))),
            |_: &str, _: Flow, _: AudioValueKind| Ok(state.borrow().clone()),
            |_: &str, _: Flow, _: &AudioValue| Err("setter failed".into()),
        ).is_err());
        assert!(apply_audio_action(
            &action, true,
            |_: Flow, _: Option<&str>| Ok(Some(("id".into(), "speaker".into()))),
            |_: &str, _: Flow, _: AudioValueKind| Ok(state.borrow().clone()),
            |_: &str, _: Flow, _: &AudioValue| Ok(()),
        ).is_err(), "setter success needs read-back verification");
    }

    #[test]
    fn mute_toggle_and_relative_gain_keep_their_distinct_contracts() {
        use std::cell::RefCell;
        let state = RefCell::new(AudioValue::Mute(false));
        let result = apply_audio_action(
            &Action::MicMute { device: None, mode: "toggle".into() }, true,
            |flow, _| Ok(Some(("mic-id".into(), flow.label().into()))),
            |_, _, _| Ok(state.borrow().clone()),
            |_, _, next| { *state.borrow_mut() = next.clone(); Ok(()) },
        ).unwrap();
        assert!(matches!(result, AudioActionResult::Changed { applied: AudioValue::Mute(true), .. }));

        let gain = RefCell::new(AudioValue::Volume(0.5));
        let result = apply_audio_action(
            &Action::MicGain { device: None, delta_pct: 5.0 }, true,
            |_, _| Ok(Some(("mic-id".into(), "mic".into()))),
            |_, _, _| Ok(gain.borrow().clone()),
            |_, _, next| { *gain.borrow_mut() = next.clone(); Ok(()) },
        ).unwrap();
        assert!(matches!(result, AudioActionResult::Changed { applied: AudioValue::Volume(v), .. } if (v - 0.55).abs() < 0.005));
    }
}
