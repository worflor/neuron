// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Keeps connected devices' firmware button functions in step with the live binds (see
//! `neuron::buttons` for the model). One serial worker owns every write so requests can't
//! interleave on a device's control pipe; the live loop only posts the rules it wants.

use neuron::controls::ControlRef;
use neuron::engine::Rule;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// What the firmware should hold: the latest rule set, the held cast trigger, and whether a
/// press-to-bind capture wants the factory functions back while it listens.
struct Want {
    rules: Vec<Rule>,
    held: Option<ControlRef>,
    capturing: bool,
}

static WANT: Mutex<Option<Want>> = Mutex::new(None);

fn want() -> std::sync::MutexGuard<'static, Option<Want>> {
    WANT.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Post the live rule set (on dispatch start and every reload) and bring devices to it.
pub fn set_rules(rules: Vec<Rule>, held: Option<ControlRef>) {
    let capturing = want().as_ref().is_some_and(|w| w.capturing);
    *want() = Some(Want { rules, held, capturing });
    kick();
}

/// Re-apply the current plan: a device connected, woke, or dropped its volatile functions.
pub fn reapply() {
    if want().is_some() {
        kick();
    }
}

/// Capture is listening: put every button back to its factory key so the pressed button is
/// identified by its stock usage, not whatever it is currently bound to emit.
pub fn capture_started() {
    if let Some(w) = want().as_mut() {
        w.capturing = true;
    }
    kick();
}

/// Capture finished: bring the binds back.
pub fn capture_ended() {
    if let Some(w) = want().as_mut() {
        w.capturing = false;
    }
    kick();
}

/// Restore every device's factory button functions, synchronously (app teardown): a volatile
/// function outliving neuron would leave a button emitting a private F13..F24 key.
pub fn restore_all() {
    *want() = None;
    for d in devices() {
        if let Err(e) = neuron::buttons::restore_stock(&d) {
            eprintln!("[buttons] pid={:04x}: restore failed: {e}", d.pid);
        }
    }
}

/// A pass is queued and hasn't started: further requests ride on it, since every pass reads the
/// latest want.
static QUEUED: AtomicBool = AtomicBool::new(false);

fn kick() {
    static TX: crate::worker::Service<()> = crate::worker::Service::new();
    if QUEUED.swap(true, Ordering::AcqRel) {
        return;
    }
    let tx = crate::worker::service_sender(&TX, "neuron-buttons", |rx| {
        crate::worker::drain(rx, "neuron-buttons", |()| {
            QUEUED.store(false, Ordering::Release);
            sync();
        });
    });
    match tx {
        Some(tx) if tx.send(()).is_ok() => {}
        _ => QUEUED.store(false, Ordering::Release),
    }
}

/// Live dispatch stopped: binds are off, so every button goes back to its factory function.
pub fn stop() {
    *want() = None;
    STOPPED.store(true, Ordering::Release);
    kick();
}

/// Set by [`stop`] so the next pass restores factory functions even with no want posted.
static STOPPED: AtomicBool = AtomicBool::new(false);
/// The arm state the last pass planned under.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Called every live tick: a change of the input arm gate re-plans, because a firmware-performed
/// bind is still neuron making a key happen and must stop while input is disarmed.
pub fn tick() {
    let armed = neuron::action::input_armed();
    if ARMED.swap(armed, Ordering::AcqRel) != armed {
        kick();
    }
}

/// One pass: every connected, button-capable device to the plan the latest want implies. Binds
/// are only planned while input is armed and no capture is listening; otherwise factory.
fn sync() {
    let stopped = STOPPED.swap(false, Ordering::AcqRel);
    let target = want().as_ref().map(|w| (w.rules.clone(), w.held, w.capturing));
    let Some((rules, held, capturing)) = target.or_else(|| stopped.then(|| (Vec::new(), None, false))) else {
        return;
    };
    let live = !capturing && neuron::action::input_armed();
    let mut taken = std::collections::BTreeSet::new();
    for d in devices() {
        // A firmware-custody device only runs its button functions in normal mode.
        neuron::writes::ensure_custody(&d);
        let plan = if live {
            neuron::buttons::plan(&d.def, &rules, held, &mut taken)
        } else {
            neuron::buttons::plan(&d.def, &[], None, &mut taken)
        };
        match neuron::buttons::apply(&d, &plan) {
            Ok(n) => crate::flight::trace("buttons", "firmware functions applied", n as u64),
            Err(e) => {
                crate::flight::trace("buttons", "firmware functions not applied", u64::from(d.pid));
                eprintln!("[buttons] pid={:04x}: {e}", d.pid);
            }
        }
    }
}

/// Every connected device that can take button functions, each through its live link.
fn devices() -> Vec<neuron::device::Device> {
    let Some(reg) = crate::hidwatch::registry() else { return Vec::new() };
    let Ok(infos) = neuron::transport::enumerate() else { return Vec::new() };
    let mut out: Vec<neuron::device::Device> = Vec::new();
    for i in &infos {
        let Some(def) = reg.find_for_pipe(i) else { continue };
        if !def.supports(neuron::registry::Capability::ButtonFunction)
            || reg.preferred_link_for(i, &infos).is_some()
            || out.iter().any(|d| d.def.name == def.name && d.dpi_unit == i.instance())
        {
            continue;
        }
        if let Ok(d) = neuron::device::Device::open_path(def.clone(), i.pid, &i.path) {
            out.push(d);
        }
    }
    out
}
