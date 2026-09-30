// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The protocol ledger (`protocol/ledger.toml`) is the evidence record for every wire fact neuron
//! relies on. These tests keep it honest: every opcode the device defs declare or the core sends
//! has an entry, every grade is backed by the evidence it claims, and every write that isn't proven
//! on hardware or on the wire is gated (or its missing gate is declared as debt).

use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

const LEDGER: &str = include_str!("../protocol/ledger.toml");
const DEVICE_DEFS: &[(&str, &str)] = &[
    ("razer-naga-v2-pro.toml", include_str!("../devices/razer-naga-v2-pro.toml")),
    ("razer-blackwidow-chroma-v2.toml", include_str!("../devices/razer-blackwidow-chroma-v2.toml")),
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    sources: BTreeMap<String, Source>,
    fact: Vec<Fact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source {
    url: String,
    license: String,
    /// Pinned commit (or wiki/PR head) the citations refer to.
    sha: String,
    /// Another source this one copies; copies don't count as independent agreement.
    derived_from: Option<String>,
    #[allow(dead_code)]
    note: Option<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Get,
    Set,
    Push,
    Frame,
    Tx,
    Mode,
    Enum,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Grade {
    /// Observed on the owner's hardware by neuron (dated).
    Live,
    /// Seen on the wire in a capture of Razer's own software.
    Capture,
    /// Two or more independent sources agree.
    Agreed,
    /// One source says so.
    Single,
    /// Structural reconstruction or guess.
    Derived,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum EvidenceKind {
    Live,
    Capture,
    Source,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    kind: EvidenceKind,
    /// `[sources]` key, for `source` evidence.
    src: Option<String>,
    /// `path#Lnn` in the pinned sha, or a URL.
    at: Option<String>,
    /// ISO date, required for `live` evidence.
    date: Option<String>,
    what: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    key: String,
    dialect: String,
    kind: Kind,
    class: Option<u8>,
    id: Option<u8>,
    #[allow(dead_code)]
    layout: Option<String>,
    meaning: String,
    #[serde(default)]
    #[allow(dead_code)]
    devices: Vec<String>,
    grade: Grade,
    /// The `NEURON_*_WRITE` env var or cargo feature that keeps an unproven write off.
    gate: Option<String>,
    /// Why an unproven write ships ungated anyway. Visible debt, never a silent pass.
    ungated_debt: Option<String>,
    evidence: Vec<Evidence>,
    #[serde(default)]
    #[allow(dead_code)]
    conflicts: Vec<String>,
    /// Known upstream but not sent by neuron: a candidate, not a dependency.
    #[serde(default)]
    unused: bool,
}

fn ledger() -> Ledger {
    toml::from_str(LEDGER).unwrap_or_else(|e| panic!("protocol/ledger.toml does not parse: {e}"))
}

fn command_prefix(f: &Fact) -> Option<String> {
    Some(format!("{}/{:02x}/{:02x}", f.dialect, f.class?, f.id?))
}

/// The independent root of a source: follows `derived_from` to the original.
fn root<'a>(l: &'a Ledger, mut name: &'a str) -> &'a str {
    for _ in 0..8 {
        match l.sources.get(name).and_then(|s| s.derived_from.as_deref()) {
            Some(parent) => name = parent,
            None => break,
        }
    }
    name
}

#[test]
fn keys_are_unique_and_sources_are_pinned() {
    let l = ledger();
    let mut seen = BTreeSet::new();
    for f in &l.fact {
        assert!(seen.insert(f.key.as_str()), "duplicate ledger key {}", f.key);
        assert!(!f.meaning.trim().is_empty(), "{}: empty meaning", f.key);
        assert!(!f.evidence.is_empty(), "{}: a fact with no evidence is not a fact", f.key);
    }
    for (name, s) in &l.sources {
        assert!(s.url.starts_with("http"), "source {name}: url");
        assert!(!s.license.trim().is_empty(), "source {name}: license");
        assert!(s.sha.len() >= 7, "source {name}: pin a commit sha");
        if let Some(p) = &s.derived_from {
            assert!(l.sources.contains_key(p), "source {name}: derived_from unknown source {p}");
        }
    }
}

#[test]
fn command_facts_are_keyed_by_their_opcode() {
    for f in &ledger().fact {
        match f.kind {
            Kind::Get | Kind::Set => {
                let prefix = command_prefix(f).unwrap_or_else(|| panic!("{}: get/set needs class + id", f.key));
                assert!(
                    f.key == prefix || f.key.starts_with(&format!("{prefix}#")),
                    "{}: key must be {prefix} or {prefix}#variant",
                    f.key
                );
                let id = f.id.unwrap_or(0);
                match f.kind {
                    Kind::Get => assert!(id >= 0x80, "{}: getter ids are >= 0x80", f.key),
                    _ => assert!(id < 0x80, "{}: setter ids are < 0x80", f.key),
                }
            }
            _ => assert!(
                f.key.starts_with(&format!("{}/", f.dialect)),
                "{}: key must start with its dialect",
                f.key
            ),
        }
    }
}

#[test]
fn grades_are_backed_by_the_evidence_they_claim() {
    let l = ledger();
    for f in &l.fact {
        for e in &f.evidence {
            assert!(!e.what.trim().is_empty(), "{}: evidence without a description", f.key);
            match e.kind {
                EvidenceKind::Source => {
                    let src = e.src.as_deref().unwrap_or_else(|| panic!("{}: source evidence needs src", f.key));
                    assert!(l.sources.contains_key(src), "{}: unknown source {src}", f.key);
                    assert!(
                        e.at.as_deref().is_some_and(|a| !a.trim().is_empty()),
                        "{}: source evidence needs a citation (`at`)",
                        f.key
                    );
                }
                EvidenceKind::Live => assert!(
                    e.date.as_deref().is_some_and(|d| d.len() == 10 && d.starts_with("20")),
                    "{}: live evidence needs an ISO date",
                    f.key
                ),
                EvidenceKind::Capture => {}
            }
        }
        let has = |k: EvidenceKind| f.evidence.iter().any(|e| e.kind == k);
        // neuron citing its own code is a pointer, not a corroborating source.
        let roots: BTreeSet<&str> = f
            .evidence
            .iter()
            .filter_map(|e| e.src.as_deref())
            .map(|s| root(&l, s))
            .filter(|r| *r != "neuron")
            .collect();
        match f.grade {
            Grade::Live => assert!(has(EvidenceKind::Live), "{}: graded live without live evidence", f.key),
            Grade::Capture => assert!(has(EvidenceKind::Capture), "{}: graded capture without a capture", f.key),
            Grade::Agreed => assert!(
                roots.len() >= 2,
                "{}: graded agreed but only {roots:?} (copies of one source are one source)",
                f.key
            ),
            Grade::Single => assert!(!roots.is_empty(), "{}: graded single without a source", f.key),
            Grade::Derived => {}
        }
    }
}

#[test]
fn unproven_writes_are_gated_or_declared_debt() {
    let src = core_sources();
    let manifest = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml")).unwrap();
    for f in ledger().fact.iter().filter(|f| f.kind == Kind::Set && !f.unused) {
        if matches!(f.grade, Grade::Live | Grade::Capture) {
            continue;
        }
        match (&f.gate, &f.ungated_debt) {
            (Some(g), _) => assert!(
                src.values().any(|s| s.contains(g.as_str())) || manifest.contains(g.as_str()),
                "{}: gate {g} is not referenced anywhere in neuron-core",
                f.key
            ),
            (None, Some(why)) => assert!(!why.trim().is_empty(), "{}: empty ungated_debt", f.key),
            (None, None) => panic!(
                "{}: a {:?}-graded write must name its gate, or declare `ungated_debt`",
                f.key, f.grade
            ),
        }
    }
}

/// Every command a builtin device def declares has a ledger entry of the matching direction.
#[test]
fn every_device_def_command_has_a_fact() {
    let l = ledger();
    let known: BTreeSet<(u8, u8)> = l
        .fact
        .iter()
        .filter(|f| f.dialect == "razer")
        .filter_map(|f| Some((f.class?, f.id?)))
        .collect();
    let mut missing = Vec::new();
    for (file, text) in DEVICE_DEFS {
        let v: toml::Value = toml::from_str(text).unwrap();
        let mut specs: Vec<(String, &toml::Value)> = Vec::new();
        if let Some(cmds) = v.get("commands").and_then(|c| c.as_table()) {
            specs.extend(cmds.iter().map(|(k, s)| (format!("commands.{k}"), s)));
        }
        if let Some(light) = v.get("lighting").and_then(|c| c.as_table()) {
            for k in ["effect", "custom_frame", "brightness"] {
                if let Some(s) = light.get(k) {
                    specs.push((format!("lighting.{k}"), s));
                }
            }
        }
        for (name, spec) in specs {
            let byte = |k: &str| spec.get(k).and_then(toml::Value::as_integer).map(|n| n as u8);
            let (Some(class), Some(id)) = (byte("class"), byte("id")) else { continue };
            if !known.contains(&(class, id)) {
                missing.push(format!("{file} [{name}] razer/{class:02x}/{id:02x}"));
            }
        }
    }
    assert!(missing.is_empty(), "device-def commands with no ledger fact:\n  {}", missing.join("\n  "));
}

/// Every razer opcode the core sends with a constant class/id has a ledger entry. Calls whose
/// class or id is a runtime value (a TOML spec, a probe loop) are covered by the def test above or
/// by the probe catalog's own entries.
#[test]
fn every_opcode_the_core_sends_has_a_fact() {
    let l = ledger();
    let known: BTreeSet<(u8, u8)> = l
        .fact
        .iter()
        .filter(|f| f.dialect == "razer")
        .filter_map(|f| Some((f.class?, f.id?)))
        .collect();
    let sources = core_sources();
    let mut globals: HashMap<String, Option<u8>> = HashMap::new();
    for text in sources.values() {
        for (k, v) in u8_consts(text) {
            globals
                .entry(k)
                .and_modify(|old| if *old != Some(v) { *old = None })
                .or_insert(Some(v));
        }
    }
    let mut missing = BTreeSet::new();
    for (file, text) in &sources {
        if file.contains("hidpp") {
            continue; // HID++ frames its own feature-indexed requests; not razer_report.
        }
        let locals = u8_consts(text);
        let resolve = |tok: &str| -> Option<u8> {
            let tok = tok.trim();
            if let Some(h) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
                return u8::from_str_radix(h, 16).ok();
            }
            locals.get(tok).copied().or_else(|| globals.get(tok).copied().flatten())
        };
        for (call, skip) in [("exec_dynamic_tx(", 1usize), ("exec_dynamic(", 0), ("verify_getter(", 1)] {
            for args in call_args(text, call) {
                if args.len() < skip + 2 {
                    continue;
                }
                if let (Some(c), Some(i)) = (resolve(&args[skip]), resolve(&args[skip + 1])) {
                    if !known.contains(&(c, i)) {
                        missing.insert(format!("razer/{c:02x}/{i:02x} ({file})"));
                    }
                }
            }
        }
    }
    assert!(missing.is_empty(), "opcodes sent by neuron-core with no ledger fact:\n  {}", missing.into_iter().collect::<Vec<_>>().join("\n  "));
}

fn core_sources() -> BTreeMap<String, String> {
    fn walk(dir: &Path, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                let text = std::fs::read_to_string(&p).unwrap();
                out.insert(p.display().to_string(), without_test_modules(&text));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut out);
    out
}

/// `text` with every `#[cfg(test)] mod … { … }` removed: mock opcodes in unit tests never reach
/// hardware, so they need no ledger entry.
fn without_test_modules(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("#[cfg(test)]") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "#[cfg(test)]".len()..];
        let is_mod = after.trim_start().starts_with("mod ");
        let Some(open) = after.find('{').filter(|_| is_mod) else {
            out.push_str("#[cfg(test)]");
            rest = after;
            continue;
        };
        let mut depth = 0i32;
        let mut end = after.len();
        for (i, ch) in after[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// `const NAME: u8 = 0x..;` declarations in one file.
fn u8_consts(text: &str) -> HashMap<String, u8> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let l = line.trim().trim_start_matches("pub ").trim_start_matches("pub(crate) ");
        let Some(rest) = l.strip_prefix("const ") else { continue };
        let Some((name, rest)) = rest.split_once(':') else { continue };
        let Some((ty, val)) = rest.split_once('=') else { continue };
        if ty.trim() != "u8" {
            continue;
        }
        let val = val.trim().trim_end_matches(';').trim();
        if let Some(h) = val.strip_prefix("0x").or_else(|| val.strip_prefix("0X")) {
            if let Ok(v) = u8::from_str_radix(h, 16) {
                out.insert(name.trim().to_string(), v);
            }
        }
    }
    out
}

/// The top-level comma-separated arguments of every `name(` call in `text` (skips the fn's own
/// definition, whose first argument is `&self`).
fn call_args(text: &str, name: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(pos) = rest.find(name) {
        let before = &rest[..pos];
        let after = &rest[pos + name.len()..];
        rest = after;
        if before.ends_with("fn ") || before.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        let (mut depth, mut cur, mut args) = (0i32, String::new(), Vec::new());
        for ch in after.chars() {
            match ch {
                '(' | '[' | '{' => {
                    depth += 1;
                    cur.push(ch);
                }
                ')' | ']' | '}' if depth == 0 => {
                    args.push(std::mem::take(&mut cur));
                    break;
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    cur.push(ch);
                }
                ',' if depth == 0 => args.push(std::mem::take(&mut cur)),
                _ => cur.push(ch),
            }
        }
        if args.first().is_some_and(|a| a.trim().starts_with("&self")) {
            continue;
        }
        out.push(args.into_iter().map(|a| a.trim().to_string()).collect());
    }
    out
}

#[test]
fn call_scanner_reads_multiline_and_tx_calls() {
    let text = "d.exec_dynamic(\n    CLASS_X,\n    ID_Y, 0x02, &[a, b])\n; d.exec_dynamic_tx(0x1f, 0x04, 0x85, 0x07, &[]);";
    let calls = call_args(text, "exec_dynamic(");
    assert_eq!(calls, vec![vec!["CLASS_X", "ID_Y", "0x02", "&[a, b]"]]);
    let tx = call_args(text, "exec_dynamic_tx(");
    assert_eq!(tx[0][1..3], ["0x04".to_string(), "0x85".to_string()]);
    let consts = u8_consts("pub const CLASS_X: u8 = 0x15;\nconst ID_Y: u8 = 0x80;\nconst N: u16 = 0x10;");
    assert_eq!(consts.get("CLASS_X"), Some(&0x15));
    assert_eq!(consts.get("ID_Y"), Some(&0x80));
    assert!(!consts.contains_key("N"));
    let stripped = without_test_modules("a();\n#[cfg(test)]\nmod tests { fn t() { d.exec_dynamic(0x0c, 0x02) } }\nb();\n#[cfg(test)]\nfn keep() {}");
    assert!(!stripped.contains("0x0c") && stripped.contains("b();") && stripped.contains("fn keep"));
}
