// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Verbs that name or change a device itself: firmware button functions (`button`), the emblem and
//! name every surface shows for a device (`badge`), and the honest replacement for the retired
//! `remap` verb.

use crate::{live, out};
use anyhow::{bail, Result};
use clap::Subcommand;
use neuron::buttons::{self, ButtonPlan, Role};
use neuron::controls::{control_label, ControlRef};
use neuron::device::Device;
use neuron::registry::{Capability, CanonicalPid, DeviceDef, Registry};
use serde_json::{json, Value};

// ── button: firmware functions ───────────────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum ButtonCmd {
    /// What the binds imply per button: firmware-performed, host-performed, or stock (touches no hardware)
    Plan {
        /// only this device (hex pid)
        #[arg(long)]
        pid: Option<String>,
    },
    /// Read each button's current function from the device (read-only)
    Read {
        #[arg(long)]
        pid: Option<String>,
    },
    /// Write the firmware-performed part of the plan, every write verified by read-back. Volatile:
    /// the device forgets it on replug or power loss.
    Apply {
        #[arg(long)]
        pid: Option<String>,
        /// consent to the buttons emitting their keys on their own (the device performs the bind
        /// with no software running) until the device resets or `button restore`
        #[arg(long)]
        arm: bool,
    },
    /// Put every button back to its factory function, verified by read-back
    Restore {
        #[arg(long)]
        pid: Option<String>,
    },
}

fn label_usage(usage: u8) -> String {
    control_label(0x07, u16::from(usage))
}

fn hex_bytes(b: &[u8]) -> String {
    use std::fmt::Write as _;
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn role_json(p: ButtonPlan) -> Value {
    let (role, mods) = match p.role {
        Role::Stock => ("stock", 0),
        Role::Performed { mods, .. } => ("firmware", mods),
        Role::Private { .. } => ("host", 0),
    };
    json!({
        "id": p.id,
        "stock_usage": p.stock_usage,
        "stock": label_usage(p.stock_usage),
        "role": role,
        "emits_usage": p.emits(),
        "emits": label_usage(p.emits()),
        "modifiers": mods,
        "record": hex_bytes(&p.record().0),
    })
}

fn button_defs<'a>(reg: &'a Registry, pid: Option<&str>) -> Result<Vec<&'a DeviceDef>> {
    let want = pid.map(neuron::authoring::parse_pid).transpose().map_err(anyhow::Error::msg)?;
    Ok(reg
        .devices
        .iter()
        .filter(|d| !d.buttons.is_empty())
        .filter(|d| want.is_none_or(|w| d.modes.first().is_some_and(|m| CanonicalPid::of(m.product_id) == w)))
        .collect())
}

/// The plan for `def` from the live spine, with the cast trigger as the held control.
fn plan_for(def: &DeviceDef, rules: &[neuron::engine::Rule], held: ControlRef, taken: &mut std::collections::BTreeSet<u8>) -> Vec<ButtonPlan> {
    buttons::plan(def, rules, Some(held), taken)
}

pub fn button(cmd: ButtonCmd, reg: &Registry) -> Result<()> {
    match cmd {
        ButtonCmd::Plan { pid } => {
            let rt = neuron::controls::build_runtime();
            let rules = rt.engine.to_rules();
            let mut taken = std::collections::BTreeSet::new();
            let mut devices = Vec::new();
            for def in button_defs(reg, pid.as_deref())? {
                let plan = plan_for(def, &rules, rt.cast_trigger, &mut taken);
                devices.push(json!({
                    "device": def.name,
                    "pid": def.modes.first().map(|m| format!("{}", CanonicalPid::of(m.product_id))),
                    "buttons": plan.iter().map(|p| role_json(*p)).collect::<Vec<_>>(),
                }));
            }
            let v = json!({
                "devices": devices,
                "note": "roles: firmware = the device emits the target key itself; host = needs the resident app's interceptor (a private F13-F24 key); stock = untouched",
            });
            out::emit(&v, || {
                if devices.is_empty() {
                    println!("no known device has firmware-assignable buttons");
                }
                for d in &devices {
                    println!("{}", d["device"].as_str().unwrap_or("?"));
                    for b in d["buttons"].as_array().into_iter().flatten() {
                        println!("  {:>2}  {:<10} {:<9} -> {}", b["id"], b["stock"].as_str().unwrap_or(""), b["role"].as_str().unwrap_or(""), b["emits"].as_str().unwrap_or(""));
                    }
                }
            })
        }
        ButtonCmd::Read { pid } => {
            let mut devices = Vec::new();
            for d in open_button_devices(reg, pid.as_deref())? {
                let mut rows = Vec::new();
                for b in &d.def.buttons {
                    rows.push(match buttons::read(&d, b.id) {
                        Ok(rec) => {
                            let kb = rec.as_keyboard();
                            json!({
                                "id": b.id, "stock": label_usage(b.stock_usage),
                                "record": hex_bytes(&rec.0),
                                "emits": kb.map(|(_, u)| label_usage(u)),
                                "modifiers": kb.map(|(m, _)| m),
                            })
                        }
                        Err(e) => json!({ "id": b.id, "stock": label_usage(b.stock_usage), "error": e.to_string() }),
                    });
                }
                devices.push(json!({ "device": d.def.name, "pid": format!("{:04x}", d.pid), "buttons": rows }));
            }
            out::emit(&json!({ "devices": devices }), || {
                for d in &devices {
                    println!("{}", d["device"].as_str().unwrap_or("?"));
                    for b in d["buttons"].as_array().into_iter().flatten() {
                        match b["error"].as_str() {
                            Some(e) => println!("  {:>2}  {:<10} read failed: {e}", b["id"], b["stock"].as_str().unwrap_or("")),
                            None => println!("  {:>2}  {:<10} emits {}", b["id"], b["stock"].as_str().unwrap_or(""), b["emits"].as_str().unwrap_or("(not a keyboard function)")),
                        }
                    }
                }
            })
        }
        ButtonCmd::Apply { pid, arm } => {
            let rt = neuron::controls::build_runtime();
            let rules = rt.engine.to_rules();
            let mut taken = std::collections::BTreeSet::new();
            let devices = open_button_devices(reg, pid.as_deref())?;
            // Plan every device first so nothing is written when the request is refused.
            let mut jobs: Vec<(Device, Vec<ButtonPlan>, usize)> = Vec::new();
            for d in devices {
                let full = plan_for(&d.def, &rules, rt.cast_trigger, &mut taken);
                let host = full.iter().filter(|p| matches!(p.role, Role::Private { .. })).count();
                // A private key needs the app's interceptor to swallow it; without one the button
                // would type F13-F24 into the desktop. Those buttons stay stock here.
                let plan: Vec<ButtonPlan> = full
                    .into_iter()
                    .map(|p| if matches!(p.role, Role::Private { .. }) { ButtonPlan { role: Role::Stock, ..p } } else { p })
                    .collect();
                jobs.push((d, plan, host));
            }
            let firmware: usize = jobs.iter().map(|(_, p, _)| p.iter().filter(|b| b.role != Role::Stock).count()).sum();
            if firmware > 0 && !arm {
                bail!("{firmware} button(s) would be written; pass --arm to let the device emit those keys itself (see `neuron button plan`)");
            }
            let mut report = Vec::new();
            for (d, plan, host) in &jobs {
                neuron::writes::ensure_custody(d);
                let applied = buttons::apply(d, plan)?;
                // read back what the device now holds, independently of apply's own check
                let mut mismatches = Vec::new();
                for p in plan {
                    match buttons::read(d, p.id) {
                        Ok(rec) if rec == p.record() || (p.role == Role::Stock && rec.as_keyboard() == Some((0, p.stock_usage))) => {}
                        Ok(rec) => mismatches.push(format!("button {} holds {:02x?}", p.id, rec.0)),
                        Err(e) => mismatches.push(format!("button {} unreadable: {e}", p.id)),
                    }
                }
                if !mismatches.is_empty() {
                    bail!("read-back disagrees on {}: {}", d.def.name, mismatches.join("; "));
                }
                report.push(json!({
                    "device": d.def.name, "pid": format!("{:04x}", d.pid), "firmware_functions": applied,
                    "host_performed_left_stock": host, "buttons": plan.iter().map(|p| role_json(*p)).collect::<Vec<_>>(),
                }));
            }
            let v = json!({ "applied": report, "volatile": true });
            out::emit(&v, || {
                for r in &report {
                    println!("{}: {} firmware function(s) written and verified ({} host-performed bind(s) need the app)", r["device"].as_str().unwrap_or("?"), r["firmware_functions"], r["host_performed_left_stock"]);
                }
                println!("volatile: the device forgets these on replug; the resident app re-applies them itself");
            })
        }
        ButtonCmd::Restore { pid } => {
            let mut report = Vec::new();
            for d in open_button_devices(reg, pid.as_deref())? {
                neuron::writes::ensure_custody(&d);
                buttons::restore_stock(&d)?;
                for b in &d.def.buttons {
                    let rec = buttons::read(&d, b.id)?;
                    if rec.as_keyboard() != Some((0, b.stock_usage)) {
                        bail!("read-back disagrees: button {} holds {:02x?} after restore", b.id, rec.0);
                    }
                }
                report.push(json!({ "device": d.def.name, "pid": format!("{:04x}", d.pid), "buttons": d.def.buttons.len() }));
            }
            out::emit(&json!({ "restored": report }), || {
                for r in &report {
                    println!("{}: {} button(s) back to factory, verified", r["device"].as_str().unwrap_or("?"), r["buttons"]);
                }
            })
        }
    }
}

/// Every connected device that takes button functions, each opened through its live link.
fn open_button_devices(reg: &Registry, pid: Option<&str>) -> Result<Vec<Device>> {
    let want = pid.map(neuron::authoring::parse_pid).transpose().map_err(anyhow::Error::msg)?;
    let infos = neuron::transport::enumerate()?;
    let mut out_devices: Vec<Device> = Vec::new();
    for i in &infos {
        let Some(def) = reg.find_for_pipe(i) else { continue };
        if !def.supports(Capability::ButtonFunction)
            || reg.preferred_link_for(i, &infos).is_some()
            || out_devices.iter().any(|d| d.def.name == def.name && d.dpi_unit == i.instance())
            || want.is_some_and(|w| CanonicalPid::of(i.pid) != w)
        {
            continue;
        }
        if let Ok(d) = Device::open_path(def.clone(), i.pid, &i.path) {
            out_devices.push(d);
        }
    }
    if out_devices.is_empty() {
        bail!("no connected device with firmware-assignable buttons{}", pid.map_or(String::new(), |p| format!(" matching pid {p}")));
    }
    Ok(out_devices)
}

// ── remap: retired ───────────────────────────────────────────────────────────────────────────

/// The old `remap` wrote `15/02`, which reads back but does not change what a key emits. A
/// firmware rebind is a bind plus the button planner, so this verb now authors that bind: `--key`
/// names the stock key of a firmware button (or `--button` its id), `--to` the key it should emit.
pub fn remap(reg: &Registry, key: Option<&str>, button_id: Option<&str>, to: Option<&str>, reset: bool) -> Result<()> {
    if reset {
        bail!("`remap --reset` wrote a register that never changed the key; restore the firmware with `neuron button restore`, and drop binds with `neuron bind rm`");
    }
    let Some(to) = to else { bail!("--to <key> is required (e.g. --to g); `remap` is now shorthand for `neuron bind add`") };
    let stock_usage: u8 = match (key, button_id) {
        (Some(_), Some(_)) => bail!("pass --key or --button, not both"),
        (Some(k), None) => neuron::action::hid_usage_for_key(k).ok_or_else(|| anyhow::anyhow!("'{k}' is not a key I can resolve to a HID usage"))?,
        (None, Some(b)) => {
            let id = u8::from_str_radix(b.trim_start_matches("0x"), 16).map_err(|_| anyhow::anyhow!("--button must be a hex button id (see `neuron button plan`)"))?;
            reg.devices
                .iter()
                .flat_map(|d| d.buttons.iter())
                .find(|s| s.id == id)
                .map(|s| s.stock_usage)
                .ok_or_else(|| anyhow::anyhow!("no known firmware button has id {id:#04x}"))?
        }
        (None, None) => bail!("say which button: --key <stock key> or --button <hex id> (`neuron button plan` lists them)"),
    };
    let owners: Vec<&DeviceDef> = reg
        .devices
        .iter()
        .filter(|d| d.buttons.iter().any(|b| b.stock_usage == stock_usage))
        .collect();
    let [def] = owners.as_slice() else {
        bail!(
            "{} known device(s) have a firmware button whose stock key is that; use `neuron bind add --trigger 'input:0x07/0x{stock_usage:02x}@<pid>'` to name the device",
            owners.len()
        )
    };
    let Some(first) = def.modes.first() else { bail!("{} has no product id", def.name) };
    let pid = CanonicalPid::of(first.product_id);
    let trigger = neuron::engine::Trigger::Input { page: 0x07, usage: u16::from(stock_usage), pid: Some(pid) };
    let action = neuron::authoring::parse_action(&format!("key:{to}")).map_err(anyhow::Error::msg)?;
    let outcome = neuron::authoring::store_add_rule(&neuron::authoring::RuleStore::Gui, trigger.clone(), action.clone(), None).map_err(anyhow::Error::msg)?;
    live::finish(
        json!({ "trigger": trigger, "action": action, "outcome": format!("{outcome:?}"), "device": def.name }),
        format!(
            "bound {} on {}  ->  {}. The button planner makes the firmware emit it while the app runs with input armed; without the app, `neuron button apply --arm`.",
            control_label(0x07, u16::from(stock_usage)),
            def.name,
            action.describe()
        ),
    );
    Ok(())
}

// ── badge ────────────────────────────────────────────────────────────────────────────────────

#[derive(Subcommand)]
pub enum BadgeCmd {
    /// Every connected device's badge, plus any badge set for a device that is not connected
    List,
    /// Set a device's emblem and/or name (a value left out keeps its current setting)
    Set {
        /// hex pid (see `badge list`)
        pid: String,
        /// mouse | keyboard | keypad | pad | stick | headset | mic | dial | device
        #[arg(long)]
        emblem: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Return a device to its automatic emblem and name
    Clear { pid: String },
}

fn badge_json(pid: CanonicalPid, connected: bool) -> Value {
    let b = neuron::badge::of(pid);
    json!({
        "pid": format!("{pid}"), "emblem": b.emblem.key(), "name": b.name, "own_name": b.own_name,
        "custom_emblem": b.custom_emblem, "custom_name": b.custom_name, "connected": connected,
    })
}

pub fn badge(cmd: BadgeCmd) -> Result<()> {
    match cmd {
        BadgeCmd::List => {
            let infos = neuron::transport::enumerate().unwrap_or_default();
            neuron::badge::learn_from(&infos);
            let mut rows: std::collections::BTreeMap<u16, Value> = std::collections::BTreeMap::new();
            for i in &infos {
                let cp = CanonicalPid::of(i.pid);
                rows.entry(cp.get()).or_insert_with(|| badge_json(cp, true));
            }
            for pid in neuron::setup::badges_table().keys() {
                if let Ok(raw) = u16::from_str_radix(pid, 16) {
                    let cp = CanonicalPid::of(raw);
                    rows.entry(cp.get()).or_insert_with(|| badge_json(cp, false));
                }
            }
            let rows: Vec<Value> = rows.into_values().collect();
            out::emit(&json!({ "badges": rows }), || {
                for r in &rows {
                    println!("{}  {:<9} {}{}", r["pid"].as_str().unwrap_or(""), r["emblem"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""), if r["connected"] == true { "" } else { "  (not connected)" });
                }
            })
        }
        BadgeCmd::Set { pid, emblem, name } => {
            let cp = neuron::authoring::parse_pid(&pid).map_err(anyhow::Error::msg)?;
            if emblem.is_none() && name.is_none() {
                bail!("nothing to set: pass --emblem and/or --name");
            }
            let new_emblem = emblem
                .as_deref()
                .map(|e| neuron::badge::Emblem::parse(e).ok_or_else(|| anyhow::anyhow!("unknown emblem '{e}' (mouse, keyboard, keypad, pad, stick, headset, mic, dial, device)")))
                .transpose()?;
            let current = neuron::setup::badges_table().get(&format!("{cp}")).cloned().unwrap_or_default();
            let emblem = new_emblem.or_else(|| current.emblem.as_deref().and_then(neuron::badge::Emblem::parse));
            let name = name.or(current.name);
            neuron::badge::set(cp, emblem, name.as_deref())?;
            live::finish(badge_json(cp, false), format!("badge {cp}: {}", neuron::badge::name(cp)));
            Ok(())
        }
        BadgeCmd::Clear { pid } => {
            let cp = neuron::authoring::parse_pid(&pid).map_err(anyhow::Error::msg)?;
            neuron::badge::set(cp, None, None)?;
            live::finish(badge_json(cp, false), format!("badge {cp} back to automatic"));
            Ok(())
        }
    }
}

// ── idle ─────────────────────────────────────────────────────────────────────────────────────

/// `neuron idle [SECS]`: the device's LED idle-off / sleep timeout (0 = never), read or written
/// with the verify-gated `set_idle_secs` (it re-reads `0x07/0x83` and demands the echo).
pub fn idle(reg: &Registry, secs: Option<u32>) -> Result<()> {
    let d = crate::open_with_command(reg, "battery_level")?;
    if let Some(s) = secs {
        if neuron::writes::writes_paused() {
            bail!("device writes are paused");
        }
        neuron::writes::set_idle_secs(&d, s)?;
    }
    let got = neuron::capability::idle_timeout_secs(&d)?;
    let verified = secs.is_none_or(|s| u32::from(got) == s.min(u32::from(u16::MAX)));
    out::emit(&json!({ "idle_secs": got, "verified": secs.map(|_| verified) }), || match secs {
        Some(s) => println!("idle-off -> {got}s  [{}]", if u32::from(got) == s { "verified" } else { "MISMATCH" }),
        None => println!("idle-off: {got}s"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_rows_describe_each_role() {
        let stock = ButtonPlan { id: 0x4b, stock_usage: 0x2e, role: Role::Stock };
        assert_eq!(role_json(stock)["role"], "stock");
        let fw = ButtonPlan { id: 0x4b, stock_usage: 0x2e, role: Role::Performed { mods: 0, usage: 0x0a } };
        let j = role_json(fw);
        assert_eq!(j["role"], "firmware");
        assert_eq!(j["emits"], "G");
        let host = ButtonPlan { id: 0x4b, stock_usage: 0x2e, role: Role::Private { usage: 0x73 } };
        assert_eq!(role_json(host)["role"], "host");
    }

    #[test]
    fn remap_refuses_the_unsafe_and_ambiguous_forms() {
        let reg = Registry::load().unwrap();
        assert!(remap(&reg, Some("="), None, None, false).is_err(), "--to is required");
        assert!(remap(&reg, Some("="), None, Some("g"), true).is_err(), "--reset points at button restore");
        assert!(remap(&reg, Some("="), Some("4b"), Some("g"), false).is_err(), "--key and --button conflict");
        assert!(remap(&reg, None, None, Some("g"), false).is_err());
    }
}
