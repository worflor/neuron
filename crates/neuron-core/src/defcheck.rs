// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Load-time bounds for device definitions read from the run root.
//!
//! A definition file records discovery; it does not grant authority. A named command may only use
//! an opcode synthesis or a built-in definition gives that name, any other command must be a read
//! the protocol ledger records, and lighting may only write opcodes synthesis emits. The bound is
//! derived from the same knowledge synthesis uses, so everything synthesized passes and the bound
//! grows with the catalog and ledger. Default binds (`side_plate_binds`, `fn_binds`) are actions,
//! not discovery: a file may only repeat the ones a built-in definition ships.

use crate::registry::DeviceDef;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

/// Longest per-command settle wait a file may ask for. The Naga's measured wireless pacing is
/// 31 ms; this leaves headroom while keeping a frame from holding the wire lock indefinitely.
const MAX_STREAM_WAIT_US: u64 = 250_000;

struct Knowledge {
    /// (dialect, command name) -> opcodes that name may use.
    named: BTreeMap<(String, String), BTreeSet<(u8, u8)>>,
    /// (dialect) -> reads the protocol ledger records.
    reads: BTreeMap<String, BTreeSet<(u8, u8)>>,
    /// Razer lighting write opcodes.
    lighting: BTreeSet<(u8, u8)>,
    /// (table, usage, action spec) default binds the built-in definitions ship.
    binds: BTreeSet<(String, String, String)>,
}

/// Every default bind a definition carries, as (table, usage, action spec).
fn binds_of(def: &DeviceDef) -> impl Iterator<Item = (String, String, String)> + '_ {
    let plates = def.side_plate_binds.iter().flat_map(|(label, binds)| {
        binds.iter().map(move |(u, a)| (format!("side_plate_binds.{label}"), u.to_ascii_lowercase(), a.clone()))
    });
    let fns = def.fn_binds.iter().map(|(u, a)| ("fn_binds".to_string(), u.to_ascii_lowercase(), a.clone()));
    plates.chain(fns)
}

fn knowledge() -> &'static Knowledge {
    static K: OnceLock<Knowledge> = OnceLock::new();
    K.get_or_init(|| {
        let mut named: BTreeMap<(String, String), BTreeSet<(u8, u8)>> = BTreeMap::new();
        let mut lighting: BTreeSet<(u8, u8)> = crate::synth::lighting_opcodes().into_iter().collect();
        let mut binds = BTreeSet::new();
        for (name, class, id) in crate::synth::known_commands() {
            named.entry(("razer".into(), name.into())).or_default().insert((class, id));
        }
        for src in crate::registry::BUILTINS {
            if let Ok(def) = toml::from_str::<DeviceDef>(src) {
                binds.extend(binds_of(&def));
                for (name, c) in &def.commands {
                    named.entry((def.dialect.clone(), name.clone())).or_default().insert((c.class, c.id));
                }
                if let Some(l) = &def.lighting {
                    lighting.insert((l.effect.class, l.effect.id));
                    lighting.extend(l.custom_frame.iter().map(|c| (c.class, c.id)));
                    lighting.extend(l.brightness.iter().map(|c| (c.class, c.id)));
                }
            }
        }
        let mut reads: BTreeMap<String, BTreeSet<(u8, u8)>> = BTreeMap::new();
        let ledger: toml::Table = include_str!("../protocol/ledger.toml").parse().unwrap_or_default();
        for fact in ledger.get("fact").and_then(toml::Value::as_array).into_iter().flatten() {
            let field = |k: &str| fact.get(k).and_then(toml::Value::as_str).unwrap_or("");
            let byte = |k: &str| fact.get(k).and_then(toml::Value::as_integer).and_then(|v| u8::try_from(v).ok());
            if field("kind") == "get" {
                if let (Some(class), Some(id)) = (byte("class"), byte("id")) {
                    reads.entry(field("dialect").to_string()).or_default().insert((class, id));
                }
            }
        }
        Knowledge { named, reads, lighting, binds }
    })
}

/// Refuse a run-root definition that would make Neuron send something its protocol knowledge
/// does not back. `Err` names the first problem.
pub fn check(def: &DeviceDef) -> Result<(), String> {
    let vendor = match def.dialect.as_str() {
        "razer" | "razer-audio" => crate::dialect::RAZER_VID,
        "hidpp" => crate::hidpp::HIDPP_VID,
        other => return Err(format!("unknown dialect `{other}`")),
    };
    if def.vendor_id != vendor {
        return Err(format!(
            "dialect `{}` speaks to vendor {vendor:04x}, but the file names vendor {:04x}",
            def.dialect, def.vendor_id
        ));
    }
    if def.stream_wait_us > MAX_STREAM_WAIT_US {
        return Err(format!("stream_wait_us {} is above {MAX_STREAM_WAIT_US}", def.stream_wait_us));
    }
    if let Some((table, usage, action)) = binds_of(def).find(|b| !knowledge().binds.contains(b)) {
        return Err(format!("{table} binds {usage} to `{action}`, which no built-in definition ships"));
    }
    // HID++ commands address feature indices the device reports at runtime, which no static
    // table can describe; that dialect is bounded by its vendor and its own read-only adoption.
    if def.dialect == "hidpp" {
        return Ok(());
    }
    let k = knowledge();
    let reads = k.reads.get(&def.dialect);
    for (name, c) in &def.commands {
        let op = (c.class, c.id);
        match k.named.get(&(def.dialect.clone(), name.clone())) {
            Some(known) if known.contains(&op) => {}
            Some(known) => {
                return Err(format!(
                    "command `{name}` uses {:02x}/{:02x}, but Neuron knows `{name}` only as {}",
                    c.class,
                    c.id,
                    known.iter().map(|(a, b)| format!("{a:02x}/{b:02x}")).collect::<Vec<_>>().join(", ")
                ));
            }
            None if c.id & 0x80 != 0 && reads.is_some_and(|r| r.contains(&op)) => {}
            None => {
                return Err(format!(
                    "command `{name}` ({:02x}/{:02x}) is not a command Neuron knows or a read the protocol ledger records",
                    c.class, c.id
                ));
            }
        }
    }
    if let Some(l) = &def.lighting {
        let writes = std::iter::once(&l.effect).chain(l.custom_frame.iter()).chain(l.brightness.iter());
        for c in writes {
            if !k.lighting.contains(&(c.class, c.id)) {
                return Err(format!("lighting writes {:02x}/{:02x}, which Neuron does not use for lighting", c.class, c.id));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin(i: usize) -> DeviceDef {
        toml::from_str(crate::registry::BUILTINS[i]).unwrap()
    }

    #[test]
    fn every_builtin_passes() {
        for i in 0..crate::registry::BUILTINS.len() {
            let d = builtin(i);
            assert_eq!(check(&d), Ok(()), "{}", d.name);
        }
    }

    #[test]
    fn a_getter_aimed_at_the_mode_switch_is_refused() {
        let mut d = builtin(0);
        let fw = d.commands.get_mut("firmware_version").unwrap();
        (fw.class, fw.id, fw.args) = (0x00, 0x04, vec![0x01, 0x00]);
        let why = check(&d).unwrap_err();
        assert!(why.contains("firmware_version") && why.contains("00/04"), "{why}");
    }

    #[test]
    fn a_setter_moved_to_another_opcode_is_refused() {
        let mut d = builtin(0);
        let dpi = d.commands.get_mut("set_dpi").unwrap();
        (dpi.class, dpi.id) = (0x00, 0x04);
        assert!(check(&d).unwrap_err().contains("set_dpi"));
    }

    #[test]
    fn an_unknown_write_is_refused_and_a_known_read_is_not() {
        let mut d = builtin(0);
        let mut extra = d.commands["firmware_version"].clone();
        (extra.class, extra.id) = (0x06, 0x0B);
        d.commands.insert("mystery".into(), extra.clone());
        assert!(check(&d).unwrap_err().contains("mystery"));
        // 00/bf is a ledger read (receiver paired devices) under a name no file uses.
        (extra.class, extra.id) = (0x00, 0xBF);
        d.commands.insert("mystery".into(), extra);
        assert_eq!(check(&d), Ok(()));
    }

    #[test]
    fn the_registry_refuses_a_crafted_file_and_keeps_a_honest_one() {
        let _root = crate::authoring::test_run_root();
        let auto = crate::runroot::run_root().join("devices").join("auto");
        std::fs::create_dir_all(&auto).unwrap();
        let honest = crate::registry::BUILTINS[0].replace("0x00A7", "0x7AA7").replace("0x00a7", "0x7aa7");
        std::fs::write(auto.join("razer-7aa7.toml"), &honest).unwrap();
        let crafted = honest
            .replace("0x7AA7", "0x7AA8")
            .replace("0x00A8", "0x7AA8")
            .replace("0x00A9", "0x7AA8")
            .replace("[commands.firmware_version]\nclass = 0x00\nid = 0x81", "[commands.firmware_version]\nclass = 0x00\nid = 0x04")
            .replace("[commands.firmware_version]\r\nclass = 0x00\r\nid = 0x81", "[commands.firmware_version]\r\nclass = 0x00\r\nid = 0x04");
        assert!(crafted.contains("id = 0x04"), "the crafted file aims firmware_version at 00/04");
        std::fs::write(auto.join("razer-7aa8.toml"), crafted).unwrap();

        let reg = crate::registry::Registry::load().unwrap();
        let vid = crate::dialect::RAZER_VID;
        assert!(reg.find_by_pid(vid, 0x7AA7).is_some(), "an honest file must still load");
        assert!(reg.find_by_pid(vid, 0x7AA8).is_none(), "a crafted file must not load");
    }

    #[test]
    fn a_file_may_only_repeat_the_default_binds_neuron_ships() {
        let mut d = builtin(0);
        d.fn_binds.insert("0x3b".into(), "run:calc.exe".into());
        assert!(check(&d).unwrap_err().contains("fn_binds"));

        let mut d = builtin(0);
        d.side_plate_binds.entry("2-button".into()).or_default().insert("0x2d".into(), "run:calc.exe".into());
        assert!(check(&d).unwrap_err().contains("side_plate_binds"));

        let mut d = builtin(0);
        d.fn_binds.insert("0x3B".into(), "media:vol-down".into());
        assert_eq!(check(&d), Ok(()), "a bind a built-in ships passes, whatever the usage's case");
    }

    #[test]
    fn lighting_vendor_and_pacing_are_bounded() {
        let mut d = builtin(1);
        let l = d.lighting.as_mut().unwrap();
        (l.effect.class, l.effect.id) = (0x05, 0x0B);
        assert!(check(&d).unwrap_err().contains("lighting"));

        let mut d = builtin(1);
        d.vendor_id = 0x046D;
        assert!(check(&d).unwrap_err().contains("vendor"));

        let mut d = builtin(0);
        d.stream_wait_us = 9_000_000_000_000_000_000;
        assert!(check(&d).unwrap_err().contains("stream_wait_us"));
    }
}
