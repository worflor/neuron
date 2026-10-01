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
/// Serializes synchronous stance restores with the background planner. The quarantine flag and
/// firmware table describe one process-wide transaction, so two passes must never clear or write
/// through each other.
static OPERATIONS: Mutex<()> = Mutex::new(());

/// The seated side plate's layer (`plate:12-button`), as the mouse announced it.
static PLATE: Mutex<Option<String>> = Mutex::new(None);

/// A plate was seated or removed: a button two plates bind differently needs re-planning.
pub fn set_plate(layer: Option<String>) {
    let mut p = PLATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if *p != layer {
        *p = layer;
        drop(p);
        reapply();
    }
}

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
    neuron::buttons::begin_private_pool_quarantine();
    neuron::action::arm_input(false);
    *want() = None;
    if let Err(e) = restore_stock_now() { eprintln!("[buttons] restore failed: {e}"); }
}

/// Move the app's input and write gates with firmware button custody ordered around them.
pub fn set_stance(armed: bool, paused: bool) -> Result<(), String> {
    neuron::buttons::begin_private_pool_quarantine();
    neuron::action::arm_input(false);
    neuron::writes::set_writes_paused(paused);
    restore_stock_now()?;
    if armed {
        neuron::action::arm_input(true);
        reapply();
    }
    Ok(())
}

fn restore_stock_now() -> Result<(), String> {
    let _operation = OPERATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    neuron::buttons::begin_private_pool_quarantine();
    let mut failures = Vec::new();
    let devices = devices()?;
    for d in devices {
        if let Err(e) = neuron::buttons::restore_stock_safety(&d) {
            failures.push(format!("pid={:04x} unit={}: {e}", d.pid, d.dpi_unit));
        }
    }
    if failures.is_empty() {
        neuron::buttons::clear_private_pool_quarantine();
        Ok(())
    } else {
        Err(failures.join("; "))
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
    if let Err(e) = set_stance(false, neuron::writes::writes_paused()) {
        eprintln!("[buttons] stop restore failed: {e}");
    }
    *want() = None;
}
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
    let _operation = OPERATIONS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let target = want().as_ref().map(|w| (w.rules.clone(), w.held, w.capturing));
    let Some((rules, held, capturing)) = target else {
        return;
    };
    let plate = PLATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let mut taken = std::collections::BTreeSet::new();
    let mut groups: std::collections::BTreeMap<u16, Vec<(neuron::device::Device, Vec<neuron::buttons::ButtonPlan>)>> = std::collections::BTreeMap::new();
    let devices = match devices() {
        Ok(devices) => devices,
        Err(e) => {
            let reason = format!("device discovery failed: {e}");
            fail_closed(&reason);
            return;
        }
    };
    let live = !capturing && neuron::action::input_armed() && !neuron::writes::writes_paused();
    for d in devices {
        let plan = if live {
            neuron::buttons::plan_for(&d.def, &rules, held, neuron::buttons::Plate::Seated(plate.as_deref()), &mut taken)
        } else {
            neuron::buttons::plan(&d.def, &[], None, &mut taken)
        };
        let pid = d.def.modes.first().map_or(d.pid, |m| m.product_id);
        groups.entry(neuron::registry::CanonicalPid::of(pid).get()).or_default().push((d, plan));
    }
    let mut whole_stock_verified = !live;
    for group in groups.values() {
        let mut inconsistent = false;
        for (d, plan) in group {
            // A firmware-custody device only runs its button functions in normal mode.
            neuron::writes::ensure_custody(d);
            let expected = plan.iter().filter(|p| p.role != neuron::buttons::Role::Stock).count();
            let stock = plan.iter().all(|p| p.role == neuron::buttons::Role::Stock);
            let applied = if stock {
                neuron::buttons::restore_stock_safety(d).map(|()| 0)
            } else {
                neuron::buttons::apply_live(d, plan)
            };
            match applied {
                Ok(n) if n == expected => crate::flight::trace("buttons", "firmware functions applied", n as u64),
                Ok(n) => {
                    inconsistent = true;
                    crate::flight::trace("buttons", "firmware functions partially applied", n as u64);
                    eprintln!("[buttons] pid={:04x}: applied {n} of {expected} planned functions", d.pid);
                }
                Err(e) => {
                    inconsistent = true;
                    crate::flight::trace("buttons", "firmware functions not applied", u64::from(d.pid));
                    eprintln!("[buttons] pid={:04x}: {e}", d.pid);
                }
            }
            if live && !neuron::action::input_armed() {
                inconsistent = true;
                break;
            }
        }
        if inconsistent {
            // `performs` is pid-scoped, so a same-model unit cannot retain a firmware-performed
            // record when another unit of that model failed to accept its plan.
            if !neuron::action::input_armed() {
                neuron::buttons::begin_private_pool_quarantine();
            }
            let mut restore_failures = Vec::new();
            for (d, _) in group {
                if let Err(e) = neuron::buttons::restore_stock_safety(d) {
                    restore_failures.push(format!("pid={:04x} unit={}: {e}", d.pid, d.dpi_unit));
                }
            }
            if !restore_failures.is_empty() {
                whole_stock_verified = false;
                fail_closed(&format!("same-PID stock restore failed: {}", restore_failures.join("; ")));
            } else if !neuron::action::input_armed() {
                // The next serialized whole-inventory stock pass owns clearing quarantine after
                // every physical unit verifies stock.
                neuron::buttons::begin_private_pool_quarantine();
            }
        }
        if live && !neuron::action::input_armed() { break; }
    }
    if whole_stock_verified {
        neuron::buttons::clear_private_pool_quarantine();
    }
}

fn fail_closed(reason: &str) {
    neuron::buttons::begin_private_pool_quarantine();
    neuron::action::arm_input(false);
    eprintln!("[buttons] input disarmed and private keys quarantined: {reason}");
}

/// Every connected device that can take button functions, each through its live link.
fn devices() -> Result<Vec<neuron::device::Device>, String> {
    let reg = crate::hidwatch::registry().ok_or_else(|| "HID registry is not initialized".to_string())?;
    let infos = neuron::transport::enumerate().map_err(|e| format!("HID enumeration failed: {e}"))?;
    let mut out: Vec<neuron::device::Device> = Vec::new();
    for i in &infos {
        let Some(def) = reg.find_for_pipe(i) else { continue };
        if !def.supports(neuron::registry::Capability::ButtonFunction)
            || reg.preferred_link_for(i, &infos).is_some()
            || out.iter().any(|d| d.def.name == def.name && d.dpi_unit == i.instance())
        {
            continue;
        }
        let d = neuron::device::Device::open_path(def.clone(), i.pid, &i.path)
            .map_err(|e| format!("cannot open button device pid={:04x} unit={}: {e}", i.pid, i.instance()))?;
        out.push(d);
    }
    Ok(out)
}
