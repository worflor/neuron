// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron control`: the controls a bind can name. Lists a connected device's controls with their
//! human names, lists every named control, translates a `(page, usage)` to its label, and captures
//! a control by pressing it (the GUI's press-to-bind).

use crate::out;
use anyhow::{bail, Result};
use clap::Subcommand;
use neuron::controls::{control_label, ControlRef, RAZER_MACRO_PAGE};
use neuron::registry::{CanonicalPid, Registry};
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum ControlCmd {
    /// Connected devices and the controls each exposes, with their human names and trigger specs
    List {
        /// only this device (hex pid, e.g. 00a8)
        #[arg(long)]
        pid: Option<String>,
    },
    /// Every control the engine can name: keyboard keys, mouse buttons, consumer keys, macro keys
    Catalog {
        /// keyboard | mouse | consumer | macro | all
        #[arg(long, default_value = "all")]
        page: String,
    },
    /// The human name of a control from its HID page and usage (decimal or 0x hex)
    Name { page: String, usage: String },
    /// Press a control on any device to learn its identity (Windows). Prints the trigger to bind.
    Capture {
        /// give up after this many seconds
        #[arg(long, default_value_t = 30)]
        seconds: u64,
        /// only accept a control from this device (hex pid)
        #[arg(long)]
        pid: Option<String>,
    },
}

fn num(s: &str) -> Result<u16> {
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u16::from_str_radix(h, 16),
        None => t.parse(),
    }
    .map_err(|_| anyhow::anyhow!("'{s}' is not a number (decimal or 0x hex)"))
}

/// One control as data: identity, both names, and the spec that binds it.
pub fn control_json(c: ControlRef) -> Value {
    json!({
        "page": c.page,
        "usage": c.usage,
        "pid": c.pid.map(|p| format!("{p}")),
        "name": c.name(),
        "label": c.label(),
        "trigger": c.to_trigger(),
        "spec": match c.pid {
            Some(p) => format!("input:0x{:02x}/0x{:02x}@{p}", c.page, c.usage),
            None => format!("input:0x{:02x}/0x{:02x}", c.page, c.usage),
        },
    })
}

pub fn run(cmd: ControlCmd, reg: &Registry) -> Result<()> {
    match cmd {
        ControlCmd::List { pid } => list(reg, pid.as_deref()),
        ControlCmd::Catalog { page } => catalog(&page),
        ControlCmd::Name { page, usage } => {
            let c = ControlRef { page: num(&page)?, usage: num(&usage)?, pid: None };
            let v = control_json(c);
            out::emit(&v, || println!("{}", c.name()))
        }
        ControlCmd::Capture { seconds, pid } => {
            let want = pid.as_deref().map(neuron::authoring::parse_pid).transpose().map_err(anyhow::Error::msg)?;
            let Some(c) = capture(seconds, want)? else {
                bail!("no control was pressed within {seconds}s");
            };
            let v = control_json(c);
            out::emit(&v, || {
                println!("{}", c.label());
                println!("  bind it:  --trigger {}", v["spec"].as_str().unwrap_or(""));
            })
        }
    }
}

fn list(reg: &Registry, only: Option<&str>) -> Result<()> {
    let want = only.map(neuron::authoring::parse_pid).transpose().map_err(anyhow::Error::msg)?;
    let infos = neuron::transport::enumerate()?;
    neuron::badge::learn_from(&infos);
    // one entry per physical device: its several HID collections share a canonical pid, and only
    // some of them are the control pipe a registry def frames
    let mut defs: std::collections::BTreeMap<u16, Option<&neuron::registry::DeviceDef>> = std::collections::BTreeMap::new();
    for i in &infos {
        let cp = CanonicalPid::of(i.pid);
        if want.is_some_and(|w| w != cp) {
            continue;
        }
        let found = reg.find_for_pipe(i);
        let slot = defs.entry(cp.get()).or_insert(None);
        if slot.is_none() {
            *slot = found;
        }
    }
    let mut seen: std::collections::BTreeMap<u16, Value> = std::collections::BTreeMap::new();
    for (raw, def) in defs {
        let cp = CanonicalPid::of(raw);
        let badge = neuron::badge::of(cp);
        let mut controls: Vec<Value> = Vec::new();
        if let Some(def) = def {
            for b in &def.buttons {
                let c = ControlRef { page: 0x07, usage: u16::from(b.stock_usage), pid: Some(cp) };
                let mut j = control_json(c);
                j["source"] = "firmware-button".into();
                j["button_id"] = b.id.into();
                controls.push(j);
            }
        }
        // a keyboard also exposes a pointer collection, so the mouse buttons are listed only for a
        // device that is a mouse by its own definition (or has none)
        let is_mouse = badge.emblem == neuron::badge::Emblem::Mouse && def.is_none_or(|d| d.supports(neuron::registry::Capability::Dpi));
        if is_mouse {
            for u in 1..=5u16 {
                let mut j = control_json(ControlRef { page: 0x09, usage: u, pid: Some(cp) });
                j["source"] = "standard-mouse".into();
                controls.push(j);
            }
        }
        seen.insert(
            cp.get(),
            json!({
                "pid": format!("{cp}"),
                "name": badge.name,
                "emblem": badge.emblem.key(),
                "def": def.map(|d| d.name.clone()),
                "controls": controls,
                "note": "keys, macro keys and other buttons: `neuron control catalog`, or press one with `neuron control capture --pid <pid>`",
            }),
        );
    }
    let devices: Vec<Value> = seen.into_values().collect();
    out::emit(&json!({ "devices": devices }), || {
        if devices.is_empty() {
            println!("no devices found");
        }
        for d in &devices {
            println!("{}  [{}]  pid={}", d["name"].as_str().unwrap_or("?"), d["emblem"].as_str().unwrap_or("?"), d["pid"].as_str().unwrap_or("?"));
            for c in d["controls"].as_array().into_iter().flatten() {
                println!("    {:<22} {}", c["name"].as_str().unwrap_or("?"), c["spec"].as_str().unwrap_or(""));
            }
        }
    })
}

fn catalog(page: &str) -> Result<()> {
    let mut rows: Vec<Value> = Vec::new();
    let mut push = |group: &str, p: u16, u: u16| {
        let label = control_label(p, u);
        // an unnamed usage falls back to a hex id; the catalog lists names only
        if label.starts_with("Key 0x") || label.starts_with("0x") || label.starts_with("Macro 0x") || label.starts_with("Scancode") {
            return;
        }
        let mut j = control_json(ControlRef { page: p, usage: u, pid: None });
        j["group"] = group.into();
        rows.push(j);
    };
    let want = |g: &str| page == "all" || page == g;
    if !["all", "keyboard", "mouse", "consumer", "macro"].contains(&page) {
        bail!("page must be keyboard, mouse, consumer, macro or all");
    }
    if want("keyboard") {
        for u in 0x04..=0xE7u16 {
            push("keyboard", 0x07, u);
        }
    }
    if want("mouse") {
        for u in 1..=5u16 {
            push("mouse", 0x09, u);
        }
    }
    if want("consumer") {
        for p in [0x0Cu16, 0x0B, 0x01] {
            for u in 0..=0x300u16 {
                push("consumer", p, u);
            }
        }
    }
    if want("macro") {
        push("macro", RAZER_MACRO_PAGE, 0x01);
        for u in 0x20..=0x4Fu16 {
            push("macro", RAZER_MACRO_PAGE, u);
        }
    }
    out::emit(&json!({ "controls": rows }), || {
        for r in &rows {
            println!("{:<10} {:<22} {}", r["group"].as_str().unwrap_or(""), r["name"].as_str().unwrap_or(""), r["spec"].as_str().unwrap_or(""));
        }
    })
}

/// Press-to-bind: wait for the first control event and return its identity. Left mouse is skipped
/// so the click that started the command can't bind itself. A Razer macro press that is followed
/// within 80 ms by its keyboard twin resolves to the twin, as the GUI does.
#[cfg(windows)]
#[allow(clippy::unnecessary_wraps)] // one signature with the non-Windows stub, which fails
pub fn capture(seconds: u64, only: Option<CanonicalPid>) -> Result<Option<ControlRef>> {
    use std::cell::Cell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    const TWIN_SETTLE: Duration = Duration::from_millis(80);
    let stop = AtomicBool::new(false);
    let found: Cell<Option<ControlRef>> = Cell::new(None);
    let parked: Cell<Option<(ControlRef, Instant)>> = Cell::new(None);
    out::note("press the control to capture (ESC cancels)...");
    neuron::controls::listen_until(
        Some(seconds),
        &stop,
        true,
        |ev| {
            let Some(&(page, usage)) = ev.hits.first() else { return };
            if (page, usage) == (0x09, 1) || only.is_some_and(|w| ev.pid != Some(w)) {
                return;
            }
            let c = ControlRef { page, usage, pid: ev.pid };
            match parked.get() {
                None if page == RAZER_MACRO_PAGE => parked.set(Some((c, Instant::now()))),
                Some((p, at)) => {
                    if at.elapsed() >= TWIN_SETTLE {
                        found.set(Some(p));
                    } else if page != RAZER_MACRO_PAGE && c.pid == p.pid {
                        found.set(Some(c));
                    }
                    if found.get().is_some() {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                None => {
                    found.set(Some(c));
                    stop.store(true, Ordering::Relaxed);
                }
            }
        },
        || {
            if let Some((p, at)) = parked.get() {
                if at.elapsed() >= TWIN_SETTLE {
                    found.set(Some(p));
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Duration::from_millis(5)
        },
    );
    Ok(found.get())
}

#[cfg(not(windows))]
pub fn capture(_seconds: u64, _only: Option<CanonicalPid>) -> Result<Option<ControlRef>> {
    bail!("press-to-capture needs the Windows control listener; name the control instead (`neuron control catalog`)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_take_decimal_and_hex() {
        assert_eq!(num("9").unwrap(), 9);
        assert_eq!(num("0x0C").unwrap(), 12);
        assert!(num("nine").is_err());
    }

    #[test]
    fn control_json_carries_a_spec_the_trigger_parser_accepts() {
        let c = ControlRef { page: 0x09, usage: 4, pid: Some(CanonicalPid::of(0x00a8)) };
        let v = control_json(c);
        let spec = v["spec"].as_str().unwrap();
        assert_eq!(neuron::authoring::parse_trigger(spec).unwrap(), c.to_trigger());
        assert_eq!(control_json(ControlRef { page: 9, usage: 4, pid: None })["name"], "Mouse 4 (thumb 1)");
    }

    #[test]
    fn the_catalog_lists_named_keys_only() {
        crate::out::set_json(false);
        assert!(catalog("keyboard").is_ok());
        assert!(catalog("nonsense").is_err());
    }
}
