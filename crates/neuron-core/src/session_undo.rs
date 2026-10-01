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

/// Verified audio edits collected across one continuous dial gesture.
#[derive(Clone, Debug, PartialEq)]
pub struct VolumeReceipt {
    id: String,
    flow: Flow,
    before: f32,
    applied: f32,
}

impl VolumeReceipt {
    /// Start only from a trusted finite endpoint read.
    #[must_use]
    pub fn new(id: String, flow: Flow, before: f32) -> Option<Self> {
        (before.is_finite() && (0.0..=1.0).contains(&before)).then_some(Self { id, flow, before, applied: before })
    }

    /// Advance the receipt only after a setter and its trusted read-back succeed.
    pub fn verified_applied(&mut self, applied: f32) -> bool {
        if !applied.is_finite() || !(0.0..=1.0).contains(&applied) { return false; }
        self.applied = applied;
        true
    }

    /// Finish the gesture only if the endpoint still has its last verified value.
    #[must_use]
    pub fn finish(self, current: Option<f32>) -> Option<Entry> {
        let current = AudioValue::Volume(current?);
        let applied = AudioValue::Volume(self.applied);
        let before = AudioValue::Volume(self.before);
        if !same_value(&current, &applied) || !actual_change(&before, &applied) { return None; }
        Some(Entry::Audio { id: self.id, flow: self.flow, before, applied })
    }
}

/// Run one dial frame through an arm check, setter and trusted read-back before advancing its receipt.
pub fn apply_verified_volume_step(
    receipt: &mut VolumeReceipt,
    desired: f32,
    mut armed: impl FnMut() -> bool,
    set: impl FnOnce(f32) -> bool,
    read: impl FnOnce() -> Option<f32>,
) -> Option<f32> {
    if !desired.is_finite() || !(0.0..=1.0).contains(&desired) || !armed() || !set(desired) { return None; }
    let applied = read()?;
    if !same_value(&AudioValue::Volume(applied), &AudioValue::Volume(desired)) { return None; }
    receipt.verified_applied(applied).then_some(applied)
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

/// Build a profile Undo entry only after the apply report confirms no fields were skipped or gated.
#[must_use]
pub fn verified_profile_entry(before: String, applied: String, skipped: &[String], gated: &[String]) -> Option<Entry> {
    if !skipped.is_empty() || !gated.is_empty() { return None; }
    profile_entry(before, applied)
}

/// A profile restore is complete only at the prior cursor with no unresolved fields.
#[must_use]
pub fn profile_restore_complete(current: &str, before: &str, unresolved: &[String]) -> bool {
    current == before && unresolved.is_empty()
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
    if !actual_change(&before, &target) { return Ok(AudioActionResult::Unchanged { name: endpoint.1 }); }
    write(&endpoint.0, flow, &target).map_err(|e| format!("endpoint write failed: {e}"))?;
    let applied = read(&endpoint.0, flow, kind).map_err(|e| format!("endpoint read-back failed: {e}"))?;
    if !same_value(&applied, &target) { return Err("endpoint read-back mismatch".into()); }
    if !actual_change(&before, &applied) { return Ok(AudioActionResult::Unchanged { name: endpoint.1 }); }
    Ok(AudioActionResult::Changed { id: endpoint.0, flow, name: endpoint.1, before, applied })
}

/// Apply a native audio Action through trusted endpoint reads and the process arm gate, then
/// record its verified result for session Undo. This is the shared fallback used by direct and
/// sequenced Actions, so audio mutations never bypass the receipt path.
#[cfg(windows)]
pub fn apply_native_audio_action(action: &Action) -> String {
    use crate::audio::{self, VolumeCtl};
    let result = apply_audio_action(
        action,
        crate::safety::input_armed(),
        |flow, name| {
            let endpoint = match flow { Flow::Capture => audio::resolve_capture(name), Flow::Render => audio::resolve_render(name) };
            Ok(endpoint.map(|endpoint| (endpoint.id, endpoint.name)))
        },
        |id, _, kind| {
            let ctl = VolumeCtl::open(id).ok_or_else(|| "endpoint unavailable".to_owned())?;
            match kind {
                AudioValueKind::Volume => ctl.try_get_volume().map(AudioValue::Volume).ok_or_else(|| "volume read failed".into()),
                AudioValueKind::Mute => ctl.try_get_mute().map(AudioValue::Mute).ok_or_else(|| "mute read failed".into()),
            }
        },
        |id, _, value| {
            let ctl = VolumeCtl::open(id).ok_or_else(|| "endpoint unavailable".to_owned())?;
            if !crate::safety::input_armed() { return Err("audio action is disarmed".into()); }
            let ok = match value { AudioValue::Volume(value) => ctl.set_volume(*value), AudioValue::Mute(value) => ctl.set_mute(*value) };
            if ok { Ok(()) } else { Err("endpoint write failed".into()) }
        },
    );
    match result {
        Ok(AudioActionResult::Unchanged { name }) => format!("{name} unchanged"),
        Ok(AudioActionResult::Changed { id, flow, name, before, applied }) => {
            if flow == Flow::Capture
                && matches!((&before, &applied), (AudioValue::Mute(a), AudioValue::Mute(b)) if a != b)
                && audio::is_default_capture_id(&id)
            {
                crate::mic_state::note_self_mute_write();
            }
            push(Entry::Audio { id, flow, before, applied: applied.clone() });
            match applied {
                AudioValue::Volume(value) => format!("{name} {} -> {}%", if flow == Flow::Capture { "gain" } else { "vol" }, (value * 100.0).round() as i32),
                AudioValue::Mute(value) => format!("{name} mute -> {}", if value { "ON" } else { "off" }),
            }
        }
        Err(error) => format!("{}: {error}", action.describe()),
    }
}

#[cfg(not(windows))]
pub fn apply_native_audio_action(action: &Action) -> String {
    match action {
        Action::MicGain { .. } | Action::MicGainSet { .. } => "mic audio controls: windows-only".into(),
        Action::MicMute { .. } => "mic mute: windows-only".into(),
        Action::OutputGain { .. } => "output audio controls: windows-only".into(),
        Action::OutputMute { .. } => "output mute: windows-only".into(),
        _ => "action is not an audio mutation".into(),
    }
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

    #[must_use]
    pub fn is_empty(&self) -> bool { self.entries.is_empty() }
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

fn actual_change(a: &AudioValue, b: &AudioValue) -> bool {
    match (a, b) {
        (AudioValue::Volume(a), AudioValue::Volume(b)) => a.is_finite() && b.is_finite() && (a - b).abs() > f32::EPSILON,
        (AudioValue::Mute(a), AudioValue::Mute(b)) => a != b,
        _ => true,
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
        assert!(verified_profile_entry("desktop".into(), "game".into(), &["mouse DPI".into()], &[]).is_none());
        assert!(verified_profile_entry("desktop".into(), "game".into(), &[], &["idle timeout".into()]).is_none());
        assert!(verified_profile_entry("".into(), "game".into(), &[], &[]).is_none());
        assert_eq!(verified_profile_entry("desktop".into(), "game".into(), &[], &[]), Some(Entry::Profile { before: "desktop".into(), applied: "game".into() }));
        assert!(profile_restore_complete("desktop", "desktop", &[]));
        assert!(!profile_restore_complete("desktop", "desktop", &["DPI".into()]));
        assert!(!profile_restore_complete("game", "desktop", &[]));
    }

    #[test]
    fn dial_volume_receipt_requires_verified_change_and_unchanged_final_resource() {
        use crate::audio::Flow;
        let mut receipt = VolumeReceipt::new("endpoint-id".into(), Flow::Render, 0.4).unwrap();
        assert!(receipt.verified_applied(0.4));
        assert!(receipt.clone().finish(Some(0.4)).is_none(), "no-op dial movement is not journaled");
        assert!(receipt.clone().finish(Some(0.8)).is_none(), "external mixer changes make the receipt stale");
        assert!(receipt.verified_applied(0.7));
        assert!(apply_verified_volume_step(&mut receipt, 0.9, || true, |_| true, || Some(0.7)).is_none(),
            "setter success with mismatched read-back cannot advance the receipt");
        assert_eq!(receipt.clone().finish(None), None, "unreadable final state cannot be claimed");
        assert_eq!(
            receipt.finish(Some(0.7)),
            Some(Entry::Audio { id: "endpoint-id".into(), flow: Flow::Render, before: AudioValue::Volume(0.4), applied: AudioValue::Volume(0.7) }),
        );
        let mut tiny = VolumeReceipt::new("endpoint-id".into(), Flow::Render, 0.500).unwrap();
        assert!(tiny.verified_applied(0.501));
        assert!(tiny.finish(Some(0.501)).is_some(), "a sub-half-percent change is still a real edit");

        let failed_set = VolumeReceipt::new("endpoint-id".into(), Flow::Capture, 0.25).unwrap();
        assert_eq!(failed_set.finish(Some(0.25)), None, "a failed setter never advances the verified receipt");
        assert!(VolumeReceipt::new("endpoint-id".into(), Flow::Capture, f32::NAN).is_none());
    }

    #[test]
    fn dial_volume_step_checks_arm_and_requires_setter_and_readback_success() {
        let mut receipt = VolumeReceipt::new("endpoint-id".into(), Flow::Render, 0.4).unwrap();
        let mut writes = 0;
        assert_eq!(apply_verified_volume_step(&mut receipt, 0.7, || false, |_| { writes += 1; true }, || Some(0.7)), None);
        assert_eq!(writes, 0, "disarmed skips the setter");
        assert_eq!(apply_verified_volume_step(&mut receipt, 0.7, || true, |_| false, || panic!("failed setter skips read-back")), None);
        assert_eq!(apply_verified_volume_step(&mut receipt, 0.7, || true, |_| true, || None), None);
        assert_eq!(apply_verified_volume_step(&mut receipt, 0.7, || true, |_| true, || Some(0.7)), Some(0.7));
        assert_eq!(receipt.finish(Some(0.7)).unwrap().states().1, State::Audio(AudioValue::Volume(0.7)));
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

        let tiny = RefCell::new(AudioValue::Volume(0.500));
        let result = apply_audio_action(
            &Action::MicGain { device: None, delta_pct: 0.1 }, true,
            |_, _| Ok(Some(("mic-id".into(), "mic".into()))),
            |_, _, _| Ok(tiny.borrow().clone()),
            |_, _, next| { *tiny.borrow_mut() = next.clone(); Ok(()) },
        ).unwrap();
        assert!(matches!(result, AudioActionResult::Changed { applied: AudioValue::Volume(v), .. } if (v - 0.501).abs() < f32::EPSILON));

        let unchanged = RefCell::new(AudioValue::Volume(0.500));
        let result = apply_audio_action(
            &Action::MicGain { device: None, delta_pct: 0.1 }, true,
            |_, _| Ok(Some(("mic-id".into(), "mic".into()))),
            |_, _, _| Ok(unchanged.borrow().clone()),
            |_, _, _| Ok(()),
        ).unwrap();
        assert_eq!(result, AudioActionResult::Unchanged { name: "mic".into() });
    }
}
