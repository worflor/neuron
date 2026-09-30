// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron bind`, `neuron action`, `neuron trigger`: the Trigger -> Action spine, authored from
//! text. Every trigger and action is the same value the config files hold, written as inline
//! JSON, an inline TOML table, or a short `kind:value` form (see `neuron action list` and
//! `neuron trigger list`). Every write is validated first and echoed back from disk.

use crate::{live, out};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use neuron::action::Action;
use neuron::authoring::{self, AddOutcome, Issue, Refs, RuleStore, Severity};
use neuron::engine::{Rule, Trigger};
use serde_json::{json, Value};

/// Where a list of binds lives: the always-live `gui.rules.toml`, or one profile's own binds.
#[derive(Args, Clone, Default)]
pub struct StoreArgs {
    /// use this profile's own binds (live while that profile is active) instead of the always-live set
    #[arg(long)]
    pub profile: Option<String>,
}

impl StoreArgs {
    pub fn store(&self) -> RuleStore {
        match &self.profile {
            Some(p) => RuleStore::Profile(p.clone()),
            None => RuleStore::Gui,
        }
    }
}

#[derive(Args, Clone)]
pub struct AddArgs {
    /// what fires it: key:F13 · mouse:4 · macro:M1 · input:0x09/0x04@pid · gesture:NAME · radial:3 ·
    /// app:EXE · mic-tap · hold:LAYER · cast:1 · game-light:APP/EFFECT · or JSON/TOML
    #[arg(long)]
    pub trigger: Option<String>,
    /// press a control instead of naming it (Windows)
    #[arg(long, conflicts_with = "trigger")]
    pub capture: bool,
    /// what it does: key:ctrl+s · dpi:1600 · macro:NAME · run:CMD · … or JSON/TOML (`neuron action list`)
    #[arg(long)]
    pub action: String,
    /// put the bind on a layer (hypershift, or any name a hold:LAYER trigger activates)
    #[arg(long)]
    pub layer: Option<String>,
    /// shorthand for --layer hypershift
    #[arg(long, conflicts_with = "layer")]
    pub hypershift: bool,
    /// accept a macro or profile name that does not exist yet (reported as a warning)
    #[arg(long)]
    pub allow_missing_refs: bool,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Args, Clone)]
pub struct SetArgs {
    /// the bind's index, counting from 0 (see `bind list`)
    pub index: usize,
    #[arg(long)]
    pub trigger: Option<String>,
    #[arg(long)]
    pub action: Option<String>,
    /// move it to this layer
    #[arg(long)]
    pub layer: Option<String>,
    /// move it to the base layer
    #[arg(long, conflicts_with = "layer")]
    pub base: bool,
    #[arg(long)]
    pub allow_missing_refs: bool,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Args, Clone)]
pub struct RmArgs {
    /// the bind's index, counting from 0 (see `bind list`)
    pub index: Option<usize>,
    /// or name it by trigger (with --layer for a layered bind)
    #[arg(long, conflicts_with = "index")]
    pub trigger: Option<String>,
    #[arg(long, requires = "trigger")]
    pub layer: Option<String>,
    #[command(flatten)]
    pub store: StoreArgs,
}

#[derive(Subcommand)]
pub enum HoldCmd {
    /// Show the HyperShift hold key
    Show,
    /// Set it: the control you hold to reach the second layer
    Set {
        #[arg(long)]
        trigger: Option<String>,
        #[arg(long, conflicts_with = "trigger")]
        capture: bool,
    },
    /// Remove it
    Clear,
}

#[derive(Subcommand)]
pub enum BindCmd {
    /// List the authored binds with their indexes
    List {
        /// only this layer (hypershift, plate:12-button, …)
        #[arg(long)]
        layer: Option<String>,
        /// only base-layer binds
        #[arg(long, conflicts_with = "layer")]
        base: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// One bind by index
    Show {
        /// the bind's index, counting from 0 (see `bind list`)
        index: usize,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Add a bind; the same trigger on the same layer is replaced, never duplicated
    Add(AddArgs),
    /// Edit the bind at INDEX: its trigger, action or layer
    Set(SetArgs),
    /// Remove a bind by index or by trigger
    Rm(RmArgs),
    /// Move a bind to another position in the list
    Mv {
        /// current index, counting from 0
        from: usize,
        /// new index, counting from 0
        to: usize,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// Remove every bind on a layer (or the base layer)
    Clear {
        #[arg(long)]
        layer: Option<String>,
        #[arg(long, conflicts_with = "layer")]
        base: bool,
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        store: StoreArgs,
    },
    /// The HyperShift hold key (the control you hold to reach the second layer)
    Hold {
        #[command(subcommand)]
        action: HoldCmd,
    },
    /// Validate a trigger and action without writing anything
    Check {
        #[arg(long)]
        trigger: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        allow_missing_refs: bool,
    },
    /// The live spine as the engine assembles it: every source folded together, read-only
    Live,
    /// The legacy bindings.toml (four string actions); new binds belong in `bind add`
    #[command(hide = true)]
    Legacy,
    /// Write the commented starter bindings.toml
    #[command(hide = true)]
    Init {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub enum ActionCmd {
    /// Every action: the shorthand ids with their parameter grammar, and a JSON example of every variant
    List,
    /// Resolve and validate an action spec; print it normalized as JSON and TOML
    Check {
        spec: String,
        #[arg(long)]
        allow_missing_refs: bool,
    },
}

#[derive(Subcommand)]
pub enum TriggerCmd {
    /// Every trigger kind with its shorthand and a JSON example
    List,
    /// Resolve a trigger spec; print it normalized as JSON and TOML
    Check { spec: String },
}

/// Read a spec argument: `@path` reads a file, `-` reads stdin, anything else is the text itself.
pub fn read_spec(s: &str) -> Result<String> {
    use std::io::Read as _;
    if s == "-" {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        return Ok(buf);
    }
    match s.strip_prefix('@') {
        Some(path) => Ok(std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?),
        None => Ok(s.to_string()),
    }
}

/// Check an action; errors bail with every message, warnings come back for the caller to report.
pub fn checked_action(a: &Action, allow_missing_refs: bool) -> Result<Vec<Issue>> {
    settle(authoring::check_action(a, &Refs::default()), allow_missing_refs)
}

fn settle(issues: Vec<Issue>, allow_missing_refs: bool) -> Result<Vec<Issue>> {
    let issues: Vec<Issue> = issues
        .into_iter()
        .map(|i| if allow_missing_refs { i.allowing_missing_refs() } else { i })
        .collect();
    if authoring::has_errors(&issues) {
        let msgs: Vec<&str> = issues.iter().filter(|i| i.severity == Severity::Error).map(|i| i.message.as_str()).collect();
        bail!("{}", msgs.join("; "));
    }
    Ok(issues)
}

pub fn trigger_from(spec: Option<&str>, capture: bool) -> Result<Trigger> {
    if capture {
        let Some(c) = crate::control::capture(30, None)? else { bail!("no control was pressed") };
        return Ok(c.to_trigger());
    }
    let Some(spec) = spec else { bail!("name the trigger: --trigger SPEC (or --capture to press it)") };
    authoring::parse_trigger(&read_spec(spec)?).map_err(anyhow::Error::msg)
}

fn warn_lines(issues: &[Issue]) {
    for i in issues.iter().filter(|i| i.severity == Severity::Warning) {
        out::note(format!("warning: {}", i.message));
    }
}

/// A turbo shorthand with no rate runs at 10 cps; the confirmation says so.
fn defaults_note(action_spec: &str) -> &'static str {
    let t = action_spec.trim();
    if t.get(..6).is_some_and(|h| h.eq_ignore_ascii_case("turbo:")) && !t.contains('\u{00b7}') {
        "  (10 cps is the default; `turbo:f \u{00b7} 12` sets the rate)"
    } else {
        ""
    }
}

fn layer_arg(layer: Option<&String>, hypershift: bool) -> Option<String> {
    if hypershift {
        Some("hypershift".into())
    } else {
        layer.cloned()
    }
}

fn state(store: &RuleStore) -> Result<Value> {
    let rules = store.load().map_err(anyhow::Error::msg)?;
    Ok(json!({
        "store": store.label(),
        "path": store.path().display().to_string(),
        "rules": rules.iter().enumerate().map(|(i, r)| authoring::rule_json(i, r)).collect::<Vec<_>>(),
    }))
}

fn line(i: usize, r: &Rule) -> String {
    match &r.layer {
        Some(l) => format!("[{i}] ({l}) {}  ->  {}", r.trigger.describe(), r.action.describe()),
        None => format!("[{i}] {}  ->  {}", r.trigger.describe(), r.action.describe()),
    }
}

pub fn bind(cmd: BindCmd) -> Result<()> {
    match cmd {
        BindCmd::List { layer, base, store } => {
            let store = store.store();
            let mut v = state(&store)?;
            let keep = |r: &Value| -> bool {
                if base {
                    r["layer"].is_null()
                } else {
                    layer.as_deref().is_none_or(|l| r["layer"].as_str() == Some(l))
                }
            };
            if let Some(rows) = v["rules"].as_array_mut() {
                rows.retain(keep);
            }
            out::emit(&v, || {
                let rows = v["rules"].as_array().map(Vec::as_slice).unwrap_or_default();
                if rows.is_empty() {
                    println!("no binds in {} ({})", store.label(), store.path().display());
                }
                for r in rows {
                    let i = r["index"].as_u64().unwrap_or(0);
                    let layer = r["layer"].as_str().map_or(String::new(), |l| format!(" ({l})"));
                    println!("[{i}]{layer} {}  ->  {}", r["trigger_text"].as_str().unwrap_or(""), r["action_text"].as_str().unwrap_or(""));
                }
            })
        }
        BindCmd::Show { index, store } => {
            let store = store.store();
            let rules = store.load().map_err(anyhow::Error::msg)?;
            let Some(r) = rules.get(index) else { bail!("no bind at index {index} ({} binds)", rules.len()) };
            let v = authoring::rule_json(index, r);
            out::emit(&v, || {
                println!("{}", line(index, r));
                println!("  trigger: {}", serde_json::to_string(&r.trigger).unwrap_or_default());
                println!("  action:  {}", serde_json::to_string(&r.action).unwrap_or_default());
            })
        }
        BindCmd::Add(a) => {
            let store = a.store.store();
            let trigger = trigger_from(a.trigger.as_deref(), a.capture)?;
            let action = authoring::parse_action(&read_spec(&a.action)?).map_err(anyhow::Error::msg)?;
            let layer = layer_arg(a.layer.as_ref(), a.hypershift);
            let rule = Rule { trigger: trigger.clone(), action: action.clone(), layer: layer.clone() };
            let issues = settle(authoring::check_rule(&rule, &Refs::default()), a.allow_missing_refs)?;
            warn_lines(&issues);
            let outcome = authoring::store_add_rule(&store, trigger, action, layer).map_err(anyhow::Error::msg)?;
            let (word, idx) = match outcome {
                AddOutcome::Added(i) => ("added", i),
                AddOutcome::Replaced(i) => ("replaced", i),
            };
            let mut v = state(&store)?;
            let saved = v["rules"][idx].clone();
            v["outcome"] = word.into();
            v["index"] = idx.into();
            v["rule"] = saved;
            v["warnings"] = json!(issues.iter().filter(|i| i.severity == Severity::Warning).map(|i| &i.message).collect::<Vec<_>>());
            live::finish(v, format!("{word}: {}{}", line(idx, &rule), defaults_note(&a.action)));
            Ok(())
        }
        BindCmd::Set(a) => {
            let store = a.store.store();
            let trigger = a.trigger.as_deref().map(|t| trigger_from(Some(t), false)).transpose()?;
            let action = a
                .action
                .as_deref()
                .map(|s| authoring::parse_action(&read_spec(s)?).map_err(anyhow::Error::msg))
                .transpose()?;
            let layer = if a.base { Some(None) } else { a.layer.clone().map(Some) };
            if trigger.is_none() && action.is_none() && layer.is_none() {
                bail!("nothing to change: pass --trigger, --action, --layer or --base");
            }
            // validate what the rule will BE, not just the pieces
            let rules = store.load().map_err(anyhow::Error::msg)?;
            let Some(cur) = rules.get(a.index) else { bail!("no bind at index {} ({} binds)", a.index, rules.len()) };
            let next = Rule {
                trigger: trigger.clone().unwrap_or_else(|| cur.trigger.clone()),
                action: action.clone().unwrap_or_else(|| cur.action.clone()),
                layer: layer.clone().unwrap_or_else(|| cur.layer.clone()),
            };
            let issues = settle(authoring::check_rule(&next, &Refs::default()), a.allow_missing_refs)?;
            warn_lines(&issues);
            let edited = authoring::store_edit_rule(&store, a.index, trigger, action, layer).map_err(anyhow::Error::msg)?;
            let mut v = state(&store)?;
            v["index"] = a.index.into();
            v["rule"] = authoring::rule_json(a.index, &edited);
            live::finish(v, format!("edited: {}{}", line(a.index, &edited), a.action.as_deref().map_or("", defaults_note)));
            Ok(())
        }
        BindCmd::Rm(a) => {
            let store = a.store.store();
            let index = match (a.index, &a.trigger) {
                (Some(i), _) => i,
                (None, Some(t)) => {
                    let trigger = trigger_from(Some(t), false)?;
                    let rules = store.load().map_err(anyhow::Error::msg)?;
                    authoring::find_rule(&rules, &trigger, a.layer.as_deref())
                        .ok_or_else(|| anyhow::anyhow!("no bind for that trigger{}", a.layer.as_deref().map_or(String::new(), |l| format!(" on layer '{l}'"))))?
                }
                (None, None) => bail!("say which bind: an index, or --trigger SPEC"),
            };
            let gone = authoring::store_remove_rule(&store, index).map_err(anyhow::Error::msg)?;
            let mut v = state(&store)?;
            v["removed"] = authoring::rule_json(index, &gone);
            live::finish(v, format!("removed: {}", line(index, &gone)));
            Ok(())
        }
        BindCmd::Mv { from, to, store } => {
            let store = store.store();
            authoring::store_move_rule(&store, from, to).map_err(anyhow::Error::msg)?;
            live::finish(state(&store)?, format!("moved bind {from} to position {to}"));
            Ok(())
        }
        BindCmd::Clear { layer, base, yes, store } => {
            let store = store.store();
            if !base && layer.is_none() {
                bail!("say which layer to clear: --base or --layer NAME");
            }
            if !yes {
                bail!("refusing to clear binds without --yes");
            }
            let n = authoring::store_clear_layer(&store, layer.as_deref()).map_err(anyhow::Error::msg)?;
            let mut v = state(&store)?;
            v["removed_count"] = n.into();
            live::finish(v, format!("cleared {n} bind(s)"));
            Ok(())
        }
        BindCmd::Hold { action } => hold(action),
        BindCmd::Check { trigger, action, allow_missing_refs } => {
            let t = authoring::parse_trigger(&read_spec(&trigger)?).map_err(anyhow::Error::msg)?;
            let a = authoring::parse_action(&read_spec(&action)?).map_err(anyhow::Error::msg)?;
            let issues = settle(authoring::check_rule(&Rule::new(t.clone(), a.clone()), &Refs::default()), allow_missing_refs)?;
            let v = json!({ "ok": true, "trigger": t, "trigger_text": t.describe(), "action": a, "action_text": a.describe(), "issues": issues });
            out::emit(&v, || {
                println!("ok: {}  ->  {}", t.describe(), a.describe());
                warn_lines(&issues);
            })
        }
        BindCmd::Live => {
            let rt = neuron::controls::build_runtime();
            let rules = rt.engine.to_rules();
            let faults = neuron::controls::take_sidecar_faults();
            let v = json!({
                "count": rules.len(),
                "layers": rt.engine.layers.keys().collect::<Vec<_>>(),
                "rules": rules.iter().enumerate().map(|(i, r)| authoring::rule_json(i, r)).collect::<Vec<_>>(),
                "sidecar_faults": faults.iter().map(|(f, why)| json!({"file": f, "why": why})).collect::<Vec<_>>(),
            });
            out::emit(&v, || {
                println!("{} live rule(s):", rules.len());
                for (i, r) in rules.iter().enumerate() {
                    println!("  {}", line(i, r));
                }
                for (f, why) in &faults {
                    println!("  ! {f}: {why}");
                }
            })
        }
        BindCmd::Legacy => {
            let b = neuron::bindings::Bindings::load();
            let v = serde_json::to_value(&b)?;
            out::emit(&v, || {
                println!("bindings ({}):", if neuron::bindings::Bindings::path().exists() { "from bindings.toml" } else { "built-in defaults" });
                if b.bindings.is_empty() {
                    println!("  (none)");
                }
                for x in &b.bindings {
                    println!("  {}", x.summary());
                }
            })
        }
        BindCmd::Init { force } => {
            let p = neuron::bindings::Bindings::path();
            if p.exists() && !force {
                bail!("bindings.toml already exists (use --force to overwrite)");
            }
            neuron::salvage::atomic_write(&p, neuron::bindings::TEMPLATE_TOML.as_bytes())?;
            live::finish(json!({ "wrote": p.display().to_string() }), format!("wrote {} — edit it to customize.", p.display()));
            Ok(())
        }
    }
}

fn hold(cmd: HoldCmd) -> Result<()> {
    match cmd {
        HoldCmd::Show => {
            let h = authoring::hypershift_hold();
            out::emit(&json!({ "hold": h, "text": h.as_ref().map(Trigger::describe) }), || match &h {
                Some(t) => println!("hypershift hold key: {}", t.describe()),
                None => println!("no hypershift hold key set"),
            })
        }
        HoldCmd::Set { trigger, capture } => {
            let t = trigger_from(trigger.as_deref(), capture)?;
            authoring::set_hypershift_hold(t.clone()).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "hold": t, "text": t.describe() }), format!("hypershift hold key: {}", t.describe()));
            Ok(())
        }
        HoldCmd::Clear => {
            authoring::clear_hypershift_hold().map_err(anyhow::Error::msg)?;
            live::finish(json!({ "hold": Value::Null }), "hypershift hold key cleared");
            Ok(())
        }
    }
}

pub fn action(cmd: ActionCmd) -> Result<()> {
    match cmd {
        ActionCmd::List => {
            let shorthand: Vec<Value> = authoring::ACTION_PALETTE
                .iter()
                .map(|(id, label, hint, group, required, tier)| json!({ "id": id, "label": label, "does": authoring::action_blurb(id), "param": hint, "group": group, "required": required, "tier": tier }))
                .collect();
            let types: Vec<Value> = authoring::action_examples()
                .into_iter()
                .map(|(a, note)| json!({ "type": authoring::variant_name(&a), "note": note, "example": a }))
                .collect();
            let v = json!({ "shorthand": shorthand, "types": types });
            out::emit(&v, || {
                println!("shorthand  --action id:param   (e.g. key:ctrl+s  dpi:1600  macro:NAME  mute:mic)");
                for s in &shorthand {
                    println!("  {:<14} {:<36} {}", s["id"].as_str().unwrap_or(""), s["does"].as_str().unwrap_or(""), s["param"].as_str().unwrap_or(""));
                }
                println!("\nJSON  --action '{{\"type\":\"key\",\"key\":\"f5\"}}'   (every variant; a TOML inline table works too)");
                for t in &types {
                    println!("  {:<20} {}", t["type"].as_str().unwrap_or(""), t["example"]);
                }
            })
        }
        ActionCmd::Check { spec, allow_missing_refs } => {
            let a = authoring::parse_action(&read_spec(&spec)?).map_err(anyhow::Error::msg)?;
            let issues = checked_action(&a, allow_missing_refs)?;
            let toml = toml::to_string_pretty(&a).unwrap_or_default();
            let v = json!({ "ok": true, "action": a, "action_text": a.describe(), "toml": toml, "issues": issues });
            out::emit(&v, || {
                println!("{}", a.describe());
                println!("{}", serde_json::to_string(&a).unwrap_or_default());
                warn_lines(&issues);
            })
        }
    }
}

pub fn trigger(cmd: TriggerCmd) -> Result<()> {
    match cmd {
        TriggerCmd::List => {
            let rows: Vec<Value> = authoring::trigger_examples()
                .into_iter()
                .map(|(t, note, forms)| json!({ "note": note, "shorthand": forms, "example": t }))
                .collect();
            out::emit(&json!({ "triggers": rows }), || {
                for r in &rows {
                    println!("{:<52} {}", r["shorthand"].as_str().unwrap_or(""), r["note"].as_str().unwrap_or(""));
                    println!("    {}", r["example"]);
                }
            })
        }
        TriggerCmd::Check { spec } => {
            let t = authoring::parse_trigger(&read_spec(&spec)?).map_err(anyhow::Error::msg)?;
            let toml = toml::to_string_pretty(&t).unwrap_or_default();
            let v = json!({ "ok": true, "trigger": t, "trigger_text": t.describe(), "toml": toml });
            out::emit(&v, || {
                println!("{}", t.describe());
                println!("{}", serde_json::to_string(&t).unwrap_or_default());
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settle_downgrades_only_unresolved_references() {
        let missing = Issue::error("no macro 'x'");
        let refs = authoring::check_action(
            &Action::Script { script: neuron::action::ScriptRef { id: "definitely_not_a_macro_9f2".into(), kind: neuron::action::ScriptKind::Python } },
            &Refs { macros: Some(std::collections::BTreeSet::default()), profiles: Some(std::collections::BTreeSet::default()) },
        );
        assert!(settle(refs.clone(), false).is_err(), "a missing macro is an error by default");
        assert!(settle(refs, true).is_ok(), "and a warning with --allow-missing-refs");
        assert!(settle(vec![missing], true).is_err(), "a plain error is never downgraded");
    }

    #[test]
    fn read_spec_passes_text_and_reads_at_files() {
        assert_eq!(read_spec("key:f5").unwrap(), "key:f5");
        let p = std::env::temp_dir().join(format!("neuron_spec_{}.json", std::process::id()));
        std::fs::write(&p, r#"{"type":"echo"}"#).unwrap();
        assert_eq!(read_spec(&format!("@{}", p.display())).unwrap(), r#"{"type":"echo"}"#);
        let _ = std::fs::remove_file(p);
        assert!(read_spec("@/no/such/file").is_err());
    }
}
