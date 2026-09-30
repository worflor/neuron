// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The whole-setup verbs: `dump` and `apply` (the entire authored setup as one document),
//! `catalog` (everything an agent can name), `status`, `reload`, `config` (paths and app
//! preferences), and `feel` (timing windows, the hypershift stance, sniper, and the device feel
//! verbs under one noun).

use crate::{live, out};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use neuron::registry::Registry;
use neuron::setup::{self, ApplyOpts, Section};
use std::fmt::Write as _;
use serde_json::{json, Value};

// ── dump / apply ─────────────────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct DumpArgs {
    /// only these sections: rules, cast, feel, apps, bindings, profiles, badges, macros, app, gestures
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// also include the recorded glyph shapes (bulky)
    #[arg(long)]
    vault: bool,
    /// write to this file instead of stdout
    #[arg(long)]
    out: Option<String>,
}

#[derive(Args)]
pub struct ApplyArgs {
    /// a setup document written by `dump` (TOML or JSON), or `-` for stdin
    file: String,
    /// validate and show what would change, write nothing
    #[arg(long)]
    dry_run: bool,
    /// also delete what the document leaves out, inside the sections it contains
    #[arg(long)]
    prune: bool,
    /// syntax-check every macro with the Python runtime before writing
    #[arg(long)]
    check_macros: bool,
    /// a macro, profile or route target that does not exist is an error (default: a warning)
    #[arg(long)]
    strict: bool,
}

pub fn dump(a: DumpArgs) -> Result<()> {
    let mut sections: Vec<Section> = if a.only.is_empty() {
        Section::DEFAULT.to_vec()
    } else {
        a.only.iter().map(|s| Section::parse(s)).collect::<Result<_, _>>().map_err(anyhow::Error::msg)?
    };
    if a.vault && !sections.contains(&Section::Gestures) {
        sections.push(Section::Gestures);
    }
    let doc = setup::dump(&sections).map_err(anyhow::Error::msg)?;
    let text = if out::json() { serde_json::to_string_pretty(&doc)? } else { setup::to_toml(&doc).map_err(anyhow::Error::msg)? };
    match a.out {
        Some(path) => {
            neuron::salvage::atomic_write(std::path::Path::new(&path), text.as_bytes())?;
            out::note(format!("wrote {path} ({} section(s))", sections.len()));
        }
        None => println!("{text}"),
    }
    Ok(())
}

pub fn apply(a: ApplyArgs) -> Result<()> {
    let spec = if a.file == "-" { "-".to_string() } else { format!("@{}", a.file) };
    let text = crate::spec::read_spec(&spec)?;
    let doc = setup::parse(&text).map_err(anyhow::Error::msg)?;
    let report = setup::apply(&doc, ApplyOpts { dry_run: a.dry_run, prune: a.prune, check_macros: a.check_macros, strict_refs: a.strict }).map_err(anyhow::Error::msg)?;
    if neuron::authoring::has_errors(&report.issues) {
        if out::json() {
            out::print_json(&serde_json::to_value(&report)?);
        }
        for i in &report.issues {
            out::note(format!("{}: {}", if i.severity == neuron::authoring::Severity::Error { "error" } else { "warning" }, i.message));
        }
        bail!("the document has problems; nothing was written");
    }
    for i in &report.issues {
        out::note(format!("warning: {}", i.message));
    }
    let changed = report.sections.iter().filter(|s| s.status != "unchanged").count();
    let line = |verb: &str| {
        let mut s = format!("{verb} {changed} of {} section(s)", report.sections.len());
        for r in &report.sections {
            let _ = write!(s, "\n  {:<9} {:<13} {}", r.section, r.status, r.detail);
        }
        s
    };
    let v = serde_json::to_value(&report)?;
    if a.dry_run {
        out::emit(&v, || println!("{}", line("would update")))
    } else if changed == 0 {
        out::emit(&v, || println!("{}", line("nothing to change:")))
    } else {
        live::finish(v, line("applied"));
        Ok(())
    }
}

// ── reload / status / catalog / config ───────────────────────────────────────────────────────

pub fn reload() -> Result<()> {
    let l = live::reload();
    let running = live::app_running();
    out::emit(&json!({ "live": l.word(), "app_running": running }), || match l {
        live::Live::OtherRoot => println!("an app is running on another run root; it was not signalled"),
        live::Live::Reloaded => println!("the running app re-read its config"),
        live::Live::NoApp => println!("no running app; config is read when it starts"),
        live::Live::Skipped => println!("skipped (--no-live)"),
        live::Live::Failed => println!("could not signal the app"),
    })
}

pub fn status() -> Result<()> {
    let root = neuron::runroot::run_root();
    let rules = neuron::authoring::RuleStore::Gui.load().map_or(0, |r| r.len());
    let presence = live::presence();
    let v = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "run_root": root.display().to_string(),
        "app_running": live::app_running(),
        "app_on_this_root": matches!(presence, live::Presence::Here),
        "active_profile": neuron::profile::restore_active_once(),
        "counts": {
            "binds": rules,
            "profiles": neuron::profile::list().len(),
            "macros": neuron::macros::macro_host::list_macros().len(),
            "glyphs": neuron::gesture::Vault::load().templates.len(),
            "routes": neuron::profile::AppRules::load().rules.len(),
        },
    });
    out::emit(&v, || {
        println!("neuron {}   run root {}", v["version"].as_str().unwrap_or(""), root.display());
        println!("app running: {}", match presence {
            live::Presence::Here => "yes",
            live::Presence::No => "no",
            live::Presence::Elsewhere => "yes, on another run root (edits here do not reach it)",
            live::Presence::Unknown => "unknown",
        });
        println!("active profile: {}", v["active_profile"].as_str().filter(|s| !s.is_empty()).unwrap_or("(none)"));
        println!("binds {}  profiles {}  macros {}  glyphs {}  routes {}", v["counts"]["binds"], v["counts"]["profiles"], v["counts"]["macros"], v["counts"]["glyphs"], v["counts"]["routes"]);
    })
}

/// Everything an agent can name, in one document.
pub fn catalog_json() -> Value {
    let actions: Vec<Value> = neuron::authoring::action_examples()
        .into_iter()
        .map(|(a, note)| json!({ "type": neuron::authoring::variant_name(&a), "note": note, "example": a }))
        .collect();
    json!({
        "actions": { "shorthand": neuron::authoring::ACTION_PALETTE.iter().map(|(id, label, hint, group, required, tier)| json!({"id": id, "label": label, "does": neuron::authoring::action_blurb(id), "param": hint, "group": group, "required": required, "tier": tier})).collect::<Vec<_>>(), "types": actions },
        "triggers": neuron::authoring::trigger_examples().into_iter().map(|(t, note, forms)| json!({"note": note, "shorthand": forms, "example": t})).collect::<Vec<_>>(),
        "lighting": neuron::layers::catalog_json(),
        "emblems": neuron::badge::Emblem::ALL.iter().map(|e| e.key()).collect::<Vec<_>>(),
        "profile_fields": neuron::manage::PROFILE_FIELDS.iter().map(|(k, h)| json!({"key": k, "value": h})).collect::<Vec<_>>(),
        "app_preferences": neuron::manage::APP_PREF_KINDS.iter().map(|(k, t)| json!({"key": k, "type": t, "secret": neuron::manage::SECRET_PREF_KEYS.contains(k)})).collect::<Vec<_>>(),
        "setup_sections": Section::DEFAULT.iter().map(|s| s.name()).chain(["gestures"]).collect::<Vec<_>>(),
        "hypershift_stances": ["hold", "latch", "smart", "one-shot"],
        "cast_modes": ["auto", "radial", "gesture"],
        "activation_phrases": ["hold", "tap hold", "tap tap hold", "tap tap"],
    })
}

pub fn catalog() -> Result<()> {
    let v = catalog_json();
    out::emit(&v, || {
        println!("neuron catalog: pass --json for the full machine-readable form.");
        println!("  actions    {} types, {} shorthand ids    (neuron action list)", v["actions"]["types"].as_array().map_or(0, Vec::len), v["actions"]["shorthand"].as_array().map_or(0, Vec::len));
        println!("  triggers   {} kinds                       (neuron trigger list)", v["triggers"].as_array().map_or(0, Vec::len));
        println!("  lighting   {} presets, {} patterns         (neuron light catalog)", v["lighting"]["presets"].as_array().map_or(0, Vec::len), v["lighting"]["patterns"].as_array().map_or(0, Vec::len));
        println!("  emblems    {}", v["emblems"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default());
        println!("  sections   {}", v["setup_sections"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default());
    })
}

#[derive(Subcommand)]
pub enum ConfigCmd {
    /// The run root and every config file with whether it exists
    Path,
    /// The GUI's `app.toml` preferences (notifications, accents, connections, …)
    App {
        #[command(subcommand)]
        action: AppPrefCmd,
    },
}

#[derive(Subcommand)]
pub enum AppPrefCmd {
    /// Every preference key with its type, and its value when set
    List,
    /// One preference's value (absent = the app's default)
    Get { key: String },
    /// Set a preference (validated); takes effect when the app next starts
    Set { key: String, value: String },
    /// Remove a preference so the app uses its default
    Unset { key: String },
}

pub fn config(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Path => {
            let root = neuron::runroot::run_root();
            let files: Vec<(&str, std::path::PathBuf)> = vec![
                ("app preferences", neuron::manage::app_toml_path()),
                ("cast", neuron::cast::CastConfig::path()),
                ("feel", neuron::feel::FeelConfig::path()),
                ("auto-switch routes", neuron::profile::AppRules::path()),
                ("legacy bindings", neuron::bindings::Bindings::path()),
                ("always-live binds", neuron::authoring::gui_rules_path()),
                ("profiles", neuron::profile::profiles_dir()),
                ("macros", neuron::macros::macro_host::macros_dir()),
                ("glyph vault", neuron::gesture::Vault::path()),
                ("badges", root.join("badges.toml")),
                ("auto-adopted devices", root.join("devices").join("auto")),
            ];
            let rows: Vec<Value> = files.iter().map(|(n, p)| json!({ "what": n, "path": p.display().to_string(), "exists": p.exists() })).collect();
            out::emit(&json!({ "run_root": root.display().to_string(), "files": rows }), || {
                println!("run root: {}", root.display());
                for r in &rows {
                    println!("  {:<20} {} {}", r["what"].as_str().unwrap_or(""), if r["exists"] == true { "+" } else { "-" }, r["path"].as_str().unwrap_or(""));
                }
            })
        }
        ConfigCmd::App { action } => match action {
            AppPrefCmd::List => {
                let table = neuron::manage::app_table().map_err(anyhow::Error::msg)?;
                let rows: Vec<Value> = neuron::manage::APP_PREF_KINDS
                    .iter()
                    .map(|(k, t)| {
                        let secret = neuron::manage::SECRET_PREF_KEYS.contains(k);
                        json!({ "key": k, "type": t, "secret": secret, "set": table.contains_key(*k), "value": if secret || *k == "lighting" { Value::Null } else { table.get(*k).map_or(Value::Null, |v| serde_json::to_value(v).unwrap_or(Value::Null)) } })
                    })
                    .collect();
                out::emit(&json!({ "preferences": rows }), || {
                    for r in &rows {
                        let val = if r["secret"] == true { "<secret>".to_string() } else if r["set"] == true { r["value"].to_string() } else { "(default)".to_string() };
                        println!("  {:<30} {:<7} {val}", r["key"].as_str().unwrap_or(""), r["type"].as_str().unwrap_or(""));
                    }
                })
            }
            AppPrefCmd::Get { key } => {
                if neuron::manage::SECRET_PREF_KEYS.contains(&key.as_str()) {
                    bail!("'{key}' is a secret and is never printed");
                }
                let table = neuron::manage::app_table().map_err(anyhow::Error::msg)?;
                if !neuron::manage::app_pref_keys().contains(&key.as_str()) {
                    bail!("unknown preference '{key}' (`neuron config app list`)");
                }
                let v = table.get(&key).map(serde_json::to_value).transpose()?;
                out::emit(&json!({ "key": key, "set": v.is_some(), "value": v }), || match &v {
                    Some(v) => println!("{v}"),
                    None => println!("(default)"),
                })
            }
            AppPrefCmd::Set { key, value } => {
                let v = neuron::manage::set_app_pref(&key, &value).map_err(anyhow::Error::msg)?;
                live::finish(json!({ "key": key, "value": serde_json::to_value(&v)? }), format!("{key} = {v}  (takes effect when the app starts)"));
                Ok(())
            }
            AppPrefCmd::Unset { key } => {
                let had = neuron::manage::unset_app_pref(&key).map_err(anyhow::Error::msg)?;
                live::finish(json!({ "key": key, "was_set": had }), if had { format!("{key} back to its default") } else { format!("{key} was not set") });
                Ok(())
            }
        },
    }
}

// ── feel ─────────────────────────────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct SniperArgs {
    /// the control to hold: a spec (mouse:5, key:f13, ...); bare `--trigger` presses one instead
    #[arg(long, alias = "bind", num_args = 0..=1, default_missing_value = "capture")]
    trigger: Option<String>,
    /// precision DPI while held (100-30000)
    #[arg(long)]
    dpi: Option<u16>,
    /// remove the sniper bind
    #[arg(long, conflicts_with_all = ["trigger", "dpi"])]
    unbind: bool,
}

#[derive(Subcommand)]
pub enum FeelCmd {
    /// The timing windows, the hypershift stance and the sniper bind
    Show,
    /// Set the rhythm/tap timing windows and the hypershift stance
    Timing {
        /// a press released within this is a tap (1-5000 ms)
        #[arg(long)]
        hold_ms: Option<u64>,
        /// longest pause inside a rhythm (1-5000 ms)
        #[arg(long)]
        gap_ms: Option<u64>,
        /// motion after release that still belongs to the stroke (1-5000 ms)
        #[arg(long)]
        coyote_ms: Option<u64>,
        /// hold | latch | smart | one-shot
        #[arg(long)]
        hypershift: Option<String>,
    },
    /// Hold-to-precision DPI
    Sniper(SniperArgs),
    /// Mouse DPI: no value reads, a value sets (verified)
    Dpi { value: Option<u16> },
    /// Polling rate in Hz: read or set
    Polling { hz: Option<u32> },
    /// The DPI stage list
    Stages {
        stages: Vec<u16>,
        /// which stage is active, counting from 1 (bind indexes count from 0)
        #[arg(long, default_value_t = 1)]
        active: u8,
        #[arg(long)]
        persist: bool,
    },
    /// HyperScroll wheel stage
    Scroll {
        /// the stage to make active, counting from 1
        stage: Option<u8>,
        #[arg(long)]
        volatile: bool,
    },
    /// Sensor lift-off distance
    Lod {
        #[arg(long)]
        lift: Option<u8>,
        #[arg(long)]
        landing: Option<u8>,
        #[arg(long)]
        sym: Option<u8>,
    },
    /// Lighting brightness 0-100
    Brightness { pct: Option<u8> },
    /// Keyboard firmware game mode: on | off
    GameMode { state: Option<String> },
    /// LED idle-off / sleep timeout in seconds (0 = never)
    Idle { secs: Option<u32> },
}

fn feel_json() -> Value {
    let cfg = neuron::feel::FeelConfig::load();
    let sniper = neuron::authoring::sniper_binding();
    json!({
        "hold_ms": cfg.hold_ms, "gap_ms": cfg.gap_ms, "coyote_ms": cfg.coyote_ms,
        "hypershift": cfg.hypershift.describe(),
        "sniper": sniper.as_ref().map(|(t, dpi)| json!({ "trigger": t, "trigger_text": t.describe(), "dpi": dpi })),
    })
}

fn parse_stance(s: &str) -> Result<neuron::feel::LayerMode> {
    match s.trim().to_ascii_lowercase().as_str() {
        "hold" => Ok(neuron::feel::LayerMode::Hold),
        "latch" => Ok(neuron::feel::LayerMode::Latch),
        "smart" => Ok(neuron::feel::LayerMode::Smart),
        "one-shot" | "oneshot" => Ok(neuron::feel::LayerMode::OneShot),
        other => bail!("hypershift stance must be hold, latch, smart or one-shot, not '{other}'"),
    }
}

pub fn sniper(a: SniperArgs) -> Result<()> {
    use neuron::authoring as au;
    if a.unbind {
        au::unbind_sniper().map_err(anyhow::Error::msg)?;
        live::finish(feel_json(), "sniper unbound");
        return Ok(());
    }
    let existing = au::sniper_binding();
    let dpi = a.dpi.or(existing.as_ref().map(|(_, d)| *d)).filter(|d| *d != 0).unwrap_or(400);
    if !(100..=30_000).contains(&dpi) {
        bail!("sniper dpi must be 100-30000, not {dpi}");
    }
    match (&a.trigger, a.dpi) {
        (Some(spec), _) => {
            let t = crate::spec::trigger_from(if spec == "capture" { None } else { Some(spec) }, spec == "capture")?;
            au::set_sniper_button(t.clone(), dpi).map_err(anyhow::Error::msg)?;
            live::finish(feel_json(), format!("sniper: hold {} -> {dpi} DPI", t.describe()));
        }
        (None, Some(_)) => {
            if !au::set_sniper_dpi(dpi).map_err(anyhow::Error::msg)? {
                bail!("no sniper button is bound yet; add --trigger <control>");
            }
            live::finish(feel_json(), format!("sniper precision DPI -> {dpi}"));
        }
        (None, None) => {
            let v = feel_json();
            out::emit(&v["sniper"].clone(), || match &existing {
                Some((t, d)) => println!("sniper: hold {} -> {d} DPI", t.describe()),
                None => println!("no sniper bind (add one: neuron feel sniper --trigger mouse:5 --dpi 400)"),
            })?;
        }
    }
    Ok(())
}

pub fn feel(cmd: FeelCmd, reg: &Registry) -> Result<()> {
    match cmd {
        FeelCmd::Show => {
            let v = feel_json();
            out::emit(&v, || {
                println!("timing: hold {}ms  gap {}ms  coyote {}ms   hypershift stance: {}", v["hold_ms"], v["gap_ms"], v["coyote_ms"], v["hypershift"].as_str().unwrap_or(""));
                match v["sniper"].as_object() {
                    Some(s) => println!("sniper: hold {} -> {} DPI", s["trigger_text"].as_str().unwrap_or(""), s["dpi"]),
                    None => println!("sniper: not bound"),
                }
            })
        }
        FeelCmd::Timing { hold_ms, gap_ms, coyote_ms, hypershift } => {
            use neuron::manage::{apply_feel_set, FeelSet};
            let mut cfg = neuron::feel::FeelConfig::load();
            let mut notes = Vec::new();
            if let Some(v) = hold_ms {
                notes.push(apply_feel_set(&mut cfg, FeelSet::HoldMs(v)).map_err(anyhow::Error::msg)?);
            }
            if let Some(v) = gap_ms {
                notes.push(apply_feel_set(&mut cfg, FeelSet::GapMs(v)).map_err(anyhow::Error::msg)?);
            }
            if let Some(v) = coyote_ms {
                notes.push(apply_feel_set(&mut cfg, FeelSet::CoyoteMs(v)).map_err(anyhow::Error::msg)?);
            }
            if let Some(s) = hypershift {
                notes.push(apply_feel_set(&mut cfg, FeelSet::Hypershift(parse_stance(&s)?)).map_err(anyhow::Error::msg)?);
            }
            if notes.is_empty() {
                bail!("nothing to change: pass --hold-ms, --gap-ms, --coyote-ms or --hypershift");
            }
            cfg.save().map_err(anyhow::Error::msg)?;
            live::finish(feel_json(), notes.join("; "));
            Ok(())
        }
        FeelCmd::Sniper(a) => sniper(a),
        FeelCmd::Dpi { value } => crate::dpi_cmd(reg, value),
        FeelCmd::Polling { hz } => crate::polling_cmd(reg, hz),
        FeelCmd::Stages { stages, active, persist } => crate::dpi_stages_cmd(reg, &stages, active, persist),
        FeelCmd::Scroll { stage, volatile } => crate::scroll_cmd(reg, stage, volatile),
        FeelCmd::Lod { lift, landing, sym } => crate::lod_cmd(reg, lift, landing, sym),
        FeelCmd::Brightness { pct } => crate::brightness_cmd(reg, pct),
        FeelCmd::GameMode { state } => crate::gamemode_cmd(reg, state.as_deref()),
        FeelCmd::Idle { secs } => crate::device_cmds::idle(reg, secs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stances_are_parsed_strictly() {
        assert!(parse_stance("smart").is_ok());
        assert!(parse_stance("one-shot").is_ok());
        assert!(parse_stance("toggle").is_err(), "an unknown stance must not silently become hold");
    }

    #[test]
    fn the_catalog_names_every_action_type_and_section() {
        let c = catalog_json();
        assert_eq!(c["actions"]["types"].as_array().map(Vec::len), Some(neuron::authoring::action_examples().len()));
        assert!(c["setup_sections"].as_array().is_some_and(|s| s.iter().any(|x| x == "profiles")));
        assert!(c["app_preferences"].as_array().is_some_and(|p| p.iter().any(|x| x["key"] == "host_obs_password" && x["secret"] == true)));
    }
}
