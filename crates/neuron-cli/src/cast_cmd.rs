// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron cast` and `neuron gesture`: the spellweaving surface. One cast trigger held down opens a
//! radial wheel (wedges), draws glyphs (gestures) and, with taps first, opens instruments
//! (rhythms). Everything here edits `cast.toml` and the gesture vault, with the same limits the GUI
//! enforces, and echoes the resulting config.

use crate::spec::read_spec;
use crate::{live, out};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use neuron::action::Action;
use neuron::authoring::{self, CastSet};
use neuron::cast::CastConfig;
use neuron::controls::ControlRef;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum CastCmd {
    /// Show the cast config: trigger, rhythm, wheel, glyph binds, activation slots
    Show,
    /// Write a starter cast.toml you can edit
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Run the cast engine on the terminal: hold the trigger, flick or draw, release (ESC stops)
    Run {
        /// override the trigger: a control spec (mouse:4, key:f13) or a legacy virtual-key number
        #[arg(long)]
        trigger: Option<String>,
    },
    /// Change the scalar settings; give any number of flags
    Set(SetArgs),
    /// The radial wheel's wedges
    Wedge {
        #[command(subcommand)]
        action: WedgeCmd,
    },
    /// The rhythms on the cast trigger: N taps then hold opens an instrument or fires an action
    Rhythm {
        #[command(subcommand)]
        action: RhythmCmd,
    },
    /// Glyph binds (same as `neuron gesture bind`)
    Glyph {
        #[command(subcommand)]
        action: GlyphCmd,
    },
}

#[derive(Args)]
pub struct SetArgs {
    /// the hold control: mouse:4, key:f13, macro:M1, input:0x09/0x04@pid (or JSON), or --capture
    #[arg(long)]
    trigger: Option<String>,
    /// press the control instead (Windows)
    #[arg(long, conflicts_with = "trigger")]
    capture: bool,
    /// activation rhythm: `hold`, `tap hold`, `tap tap hold`, `tap tap`
    #[arg(long)]
    activation: Option<String>,
    /// wedge count, 3 up to the deadzone's limit
    #[arg(long)]
    sectors: Option<usize>,
    /// minimum flick distance in mouse counts
    #[arg(long)]
    deadzone: Option<f64>,
    /// spell assist 0.0-0.6 (0 = strict)
    #[arg(long)]
    assist: Option<f64>,
    /// auto | radial | gesture
    #[arg(long)]
    mode: Option<String>,
    /// swap the wheel for its hypershift set while a layer is held: on | off
    #[arg(long)]
    hyper: Option<String>,
}

#[derive(Subcommand)]
pub enum WedgeCmd {
    /// List the wedges with their compass names
    List {
        #[arg(long)]
        hyper: bool,
    },
    /// Bind wedge INDEX (0 = north, clockwise) to an action
    Set {
        index: usize,
        #[arg(long)]
        action: String,
        /// edit the hypershift wheel
        #[arg(long)]
        hyper: bool,
        #[arg(long)]
        allow_missing_refs: bool,
    },
    /// Unbind wedge INDEX
    Clear {
        index: usize,
        #[arg(long)]
        hyper: bool,
    },
}

#[derive(Subcommand)]
pub enum RhythmCmd {
    List,
    /// Bind "TAPS taps then hold" to an action
    Set {
        taps: u8,
        #[arg(long)]
        action: String,
        #[arg(long)]
        allow_missing_refs: bool,
    },
    /// Unbind a rhythm
    Rm { taps: u8 },
}

#[derive(Subcommand)]
pub enum GlyphCmd {
    /// Bind a recorded glyph to an action
    Bind {
        name: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        allow_missing_refs: bool,
    },
    /// Remove a glyph's binding (the recorded shape stays)
    Unbind { name: String },
}

#[derive(Subcommand)]
pub enum GestureCmd {
    /// Validate the eigenmotion engine against synthetic shapes (no device needed)
    Selftest,
    /// Record a named glyph: hold the cast trigger, draw, release
    Record {
        name: String,
        /// the control to hold: a spec, or a legacy virtual-key number (default: the cast trigger)
        #[arg(long)]
        trigger: Option<String>,
    },
    /// Recognize a drawn glyph against the vault: hold the trigger, draw, release
    Match {
        #[arg(long)]
        trigger: Option<String>,
    },
    /// List the recorded glyphs with their binds, and the attunement
    List,
    /// Attune the glyph properties and match threshold (saved in the vault)
    Tune {
        #[arg(long)]
        damping: Option<f64>,
        #[arg(long)]
        curve: Option<f64>,
        #[arg(long)]
        resid: Option<f64>,
        #[arg(long)]
        threshold: Option<f64>,
        #[arg(long)]
        resample: Option<usize>,
        #[arg(long)]
        invariant: Option<f64>,
    },
    /// Rename a glyph; its bind follows
    Rename { old: String, new: String },
    /// Delete a glyph and its bind
    Delete { name: String },
    /// Delete every glyph and bind
    Clear {
        #[arg(long)]
        yes: bool,
    },
    /// Bind a glyph to an action
    Bind {
        name: String,
        #[arg(long)]
        action: String,
        #[arg(long)]
        allow_missing_refs: bool,
    },
    /// Remove a glyph's bind
    Unbind { name: String },
}

/// A control for the cast engine or glyph capture: a spec, a legacy VK number, or (when absent)
/// the configured cast trigger.
pub fn control_arg(s: Option<&str>) -> Result<ControlRef> {
    let Some(s) = s else { return Ok(CastConfig::load().trigger) };
    if let Ok(vk) = s.trim().parse::<i32>() {
        return Ok(ControlRef::from_vk(vk));
    }
    if let Some(hex) = s.trim().strip_prefix("0x").and_then(|h| i32::from_str_radix(h, 16).ok()) {
        return Ok(ControlRef::from_vk(hex));
    }
    match authoring::parse_trigger(&read_spec(s)?).map_err(anyhow::Error::msg)? {
        neuron::engine::Trigger::Input { page, usage, pid } => Ok(ControlRef { page, usage, pid }),
        other => bail!("the cast trigger must be a control (mouse:4, key:f13, …), not '{}'", other.describe()),
    }
}

fn wedges(cfg: &CastConfig, hyper: bool) -> Vec<Value> {
    let set = if hyper { &cfg.hyper_radial } else { &cfg.radial };
    (0..cfg.sectors)
        .map(|i| {
            let a = set.get(i).cloned().unwrap_or_default();
            json!({ "index": i, "compass": neuron::radial::compass(i, cfg.sectors), "action": a, "action_text": a.describe() })
        })
        .collect()
}

/// The cast config as an agent reads it.
pub fn cast_json() -> Value {
    let cfg = CastConfig::load();
    let (slots, complaints) = cfg.mode_slots();
    json!({
        "path": CastConfig::path().display().to_string(),
        "exists": CastConfig::path().exists(),
        "trigger": cfg.trigger,
        "trigger_text": cfg.trigger.label(),
        "activation": cfg.activation,
        "sectors": cfg.sectors,
        "deadzone": cfg.deadzone,
        "assist": cfg.assist,
        "mode": format!("{:?}", cfg.mode).to_lowercase(),
        "hyper_radial_on": cfg.hyper_radial_on,
        "wedges": wedges(&cfg, false),
        "hyper_wedges": wedges(&cfg, true),
        "glyphs": cfg.gestures.iter().map(|(n, a)| json!({ "name": n, "action": a, "action_text": a.describe() })).collect::<Vec<_>>(),
        "rhythms": cfg.rhythm_actions.iter().map(|r| json!({ "taps": r.taps, "phrase": neuron::cast::taps_phrase(r.taps), "action": r.action, "action_text": r.action.describe() })).collect::<Vec<_>>(),
        "slots": slots.iter().map(|s| json!({ "taps": s.taps, "weave": s.is_weave, "action_text": s.action.describe() })).collect::<Vec<_>>(),
        "complaints": complaints,
    })
}

fn print_cast(v: &Value) {
    println!(
        "cast ({}): trigger={}  activation={}  mode={}  sectors={}  deadzone={}  assist={}  hypershift-wheel={}",
        if v["exists"] == true { "cast.toml" } else { "defaults" },
        v["trigger_text"].as_str().unwrap_or(""),
        v["activation"].as_str().unwrap_or(""),
        v["mode"].as_str().unwrap_or(""),
        v["sectors"],
        v["deadzone"],
        v["assist"],
        if v["hyper_radial_on"] == true { "on" } else { "off" }
    );
    println!("  wheel:");
    for w in v["wedges"].as_array().into_iter().flatten() {
        println!("    [{}] {:>3}  ->  {}", w["index"], w["compass"].as_str().unwrap_or(""), w["action_text"].as_str().unwrap_or(""));
    }
    println!("  glyph binds:");
    for g in v["glyphs"].as_array().into_iter().flatten() {
        println!("    {:<14} ->  {}", g["name"].as_str().unwrap_or(""), g["action_text"].as_str().unwrap_or(""));
    }
    println!("  rhythms:");
    for r in v["rhythms"].as_array().into_iter().flatten() {
        println!("    {:<16} ->  {}", r["phrase"].as_str().unwrap_or(""), r["action_text"].as_str().unwrap_or(""));
    }
    for c in v["complaints"].as_array().into_iter().flatten() {
        println!("  ! {}", c.as_str().unwrap_or(""));
    }
}

fn action_arg(spec: &str, allow: bool) -> Result<Action> {
    let a = authoring::parse_action(&read_spec(spec)?).map_err(anyhow::Error::msg)?;
    for i in crate::spec::checked_action(&a, allow)? {
        out::note(format!("warning: {}", i.message));
    }
    Ok(a)
}

pub fn cast(cmd: CastCmd) -> Result<()> {
    match cmd {
        CastCmd::Show => {
            let v = cast_json();
            out::emit(&v, || print_cast(&v))
        }
        CastCmd::Init { force } => {
            if CastConfig::path().exists() && !force {
                bail!("cast.toml already exists (use --force to overwrite)");
            }
            neuron::salvage::atomic_write(&CastConfig::path(), neuron::cast::TEMPLATE_TOML.as_bytes())?;
            live::finish(cast_json(), format!("wrote {}", CastConfig::path().display()));
            Ok(())
        }
        CastCmd::Run { trigger } => {
            crate::cast_run(Some(control_arg(trigger.as_deref())?));
            Ok(())
        }
        CastCmd::Set(a) => {
            let mut cfg = CastConfig::load();
            let mut notes = Vec::new();
            if a.trigger.is_some() || a.capture {
                let t = crate::spec::trigger_from(a.trigger.as_deref(), a.capture)?;
                let neuron::engine::Trigger::Input { page, usage, pid } = t else { bail!("the cast trigger must be a control") };
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Trigger(ControlRef { page, usage, pid })).map_err(anyhow::Error::msg)?);
            }
            if let Some(p) = a.activation {
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Activation(p)).map_err(anyhow::Error::msg)?);
            }
            if let Some(d) = a.deadzone {
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Deadzone(d)).map_err(anyhow::Error::msg)?);
            }
            if let Some(n) = a.sectors {
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Sectors(n)).map_err(anyhow::Error::msg)?);
            }
            if let Some(x) = a.assist {
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Assist(x)).map_err(anyhow::Error::msg)?);
            }
            if let Some(m) = a.mode {
                let mode = match m.to_ascii_lowercase().as_str() {
                    "auto" => neuron::cast::Mode::Auto,
                    "radial" => neuron::cast::Mode::Radial,
                    "gesture" => neuron::cast::Mode::Gesture,
                    other => bail!("mode must be auto, radial or gesture, not '{other}'"),
                };
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::Mode(mode)).map_err(anyhow::Error::msg)?);
            }
            if let Some(h) = a.hyper {
                let on = match h.to_ascii_lowercase().as_str() {
                    "on" | "true" => true,
                    "off" | "false" => false,
                    other => bail!("--hyper takes on or off, not '{other}'"),
                };
                notes.push(authoring::apply_cast_set(&mut cfg, CastSet::HyperRadialOn(on)).map_err(anyhow::Error::msg)?);
            }
            if notes.is_empty() {
                bail!("nothing to change: pass --trigger, --activation, --sectors, --deadzone, --assist, --mode or --hyper");
            }
            authoring::save_cast(&cfg).map_err(anyhow::Error::msg)?;
            let mut v = cast_json();
            v["changed"] = json!(notes);
            live::finish(v, notes.join("; "));
            Ok(())
        }
        CastCmd::Wedge { action } => wedge(action),
        CastCmd::Rhythm { action } => rhythm(action),
        CastCmd::Glyph { action } => glyph_bind(action),
    }
}

fn wedge(cmd: WedgeCmd) -> Result<()> {
    match cmd {
        WedgeCmd::List { hyper } => {
            let cfg = CastConfig::load();
            let rows = wedges(&cfg, hyper);
            out::emit(&json!({ "hyper": hyper, "sectors": cfg.sectors, "wedges": rows }), || {
                for w in &rows {
                    println!("[{}] {:>3}  ->  {}", w["index"], w["compass"].as_str().unwrap_or(""), w["action_text"].as_str().unwrap_or(""));
                }
            })
        }
        WedgeCmd::Set { index, action, hyper, allow_missing_refs } => {
            let mut cfg = CastConfig::load();
            if index >= cfg.sectors {
                bail!("wedge {index} is beyond the wheel's {} sectors (`neuron cast set --sectors N` first)", cfg.sectors);
            }
            let a = action_arg(&action, allow_missing_refs)?;
            authoring::set_sector_action_on(&mut cfg, index, a.clone(), hyper).map_err(anyhow::Error::msg)?;
            let mut v = cast_json();
            v["wedge"] = json!({ "index": index, "hyper": hyper, "action": a });
            live::finish(v, format!("wedge {index}{} -> {}", if hyper { " (hypershift)" } else { "" }, a.describe()));
            Ok(())
        }
        WedgeCmd::Clear { index, hyper } => {
            let mut cfg = CastConfig::load();
            authoring::clear_sector_action(&mut cfg, index, hyper).map_err(anyhow::Error::msg)?;
            live::finish(cast_json(), format!("wedge {index} cleared"));
            Ok(())
        }
    }
}

fn rhythm(cmd: RhythmCmd) -> Result<()> {
    match cmd {
        RhythmCmd::List => {
            let v = cast_json();
            out::emit(&json!({ "rhythms": v["rhythms"], "complaints": v["complaints"] }), || {
                for r in v["rhythms"].as_array().into_iter().flatten() {
                    println!("{:<16} ->  {}", r["phrase"].as_str().unwrap_or(""), r["action_text"].as_str().unwrap_or(""));
                }
            })
        }
        RhythmCmd::Set { taps, action, allow_missing_refs } => {
            if taps == 0 {
                bail!("0 taps is the weave's own plain hold; bind a rhythm of 1 or more taps");
            }
            let a = action_arg(&action, allow_missing_refs)?;
            let mut cfg = CastConfig::load();
            authoring::set_rhythm_action(&mut cfg, taps, a.clone()).map_err(anyhow::Error::msg)?;
            live::finish(cast_json(), format!("{} -> {}", neuron::cast::taps_phrase(taps), a.describe()));
            Ok(())
        }
        RhythmCmd::Rm { taps } => {
            let mut cfg = CastConfig::load();
            if !cfg.rhythm_actions.iter().any(|r| r.taps == taps) {
                bail!("no rhythm with {taps} tap(s) is bound");
            }
            authoring::delete_rhythm_action(&mut cfg, taps).map_err(anyhow::Error::msg)?;
            live::finish(cast_json(), format!("{} unbound", neuron::cast::taps_phrase(taps)));
            Ok(())
        }
    }
}

fn glyph_bind(cmd: GlyphCmd) -> Result<()> {
    match cmd {
        GlyphCmd::Bind { name, action, allow_missing_refs } => bind_glyph(&name, &action, allow_missing_refs),
        GlyphCmd::Unbind { name } => unbind_glyph(&name),
    }
}

fn bind_glyph(name: &str, action: &str, allow: bool) -> Result<()> {
    let vault = neuron::gesture::Vault::load();
    if !vault.templates.iter().any(|t| t.name == name) {
        let known: Vec<&str> = vault.templates.iter().map(|t| t.name.as_str()).collect();
        bail!("no recorded glyph '{name}' (recorded: {}); record it with `neuron gesture record {name}`", if known.is_empty() { "none".into() } else { known.join(", ") });
    }
    let a = action_arg(action, allow)?;
    let mut cfg = CastConfig::load();
    authoring::set_gesture_action(&mut cfg, name, a.clone()).map_err(anyhow::Error::msg)?;
    live::finish(cast_json(), format!("glyph '{name}' -> {}", a.describe()));
    Ok(())
}

fn unbind_glyph(name: &str) -> Result<()> {
    let mut cfg = CastConfig::load();
    if !authoring::clear_gesture_action(&mut cfg, name).map_err(anyhow::Error::msg)? {
        bail!("glyph '{name}' has no bind");
    }
    live::finish(cast_json(), format!("glyph '{name}' unbound"));
    Ok(())
}

pub fn gesture(cmd: GestureCmd) -> Result<()> {
    match cmd {
        GestureCmd::Selftest => crate::gesture_selftest(),
        GestureCmd::Record { name, trigger } => crate::gesture_record(&name, control_arg(trigger.as_deref())?),
        GestureCmd::Match { trigger } => crate::gesture_match(control_arg(trigger.as_deref())?),
        GestureCmd::List => list(),
        GestureCmd::Tune { damping, curve, resid, threshold, resample, invariant } => {
            crate::gesture_tune(damping, curve, resid, threshold, resample, invariant)
        }
        GestureCmd::Rename { old, new } => {
            authoring::rename_gesture(&old, &new).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "renamed": { "from": old, "to": new } }), format!("renamed '{old}' to '{}'", new.trim()));
            Ok(())
        }
        GestureCmd::Delete { name } => {
            authoring::delete_gesture(&name).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "deleted": name }), format!("deleted '{name}'"));
            Ok(())
        }
        GestureCmd::Clear { yes } => {
            if !yes {
                bail!("refusing to delete every glyph without --yes");
            }
            let n = authoring::clear_gestures().map_err(anyhow::Error::msg)?;
            live::finish(json!({ "deleted_glyphs": n }), format!("deleted {n} glyph(s) and their binds"));
            Ok(())
        }
        GestureCmd::Bind { name, action, allow_missing_refs } => bind_glyph(&name, &action, allow_missing_refs),
        GestureCmd::Unbind { name } => unbind_glyph(&name),
    }
}

fn list() -> Result<()> {
    let vault = neuron::gesture::Vault::load();
    let cast = CastConfig::load();
    let c = &vault.config;
    let glyphs: Vec<Value> = vault
        .templates
        .iter()
        .map(|t| {
            let bound = cast.gestures.get(&t.name);
            json!({
                "name": t.name, "blocks": t.word.sigs.len(),
                "winding": t.word.inv.winding, "bending": t.word.inv.bending, "closure": t.word.inv.closure,
                "action": bound, "action_text": bound.map(Action::describe),
            })
        })
        .collect();
    let orphans: Vec<&String> = cast.gestures.keys().filter(|n| !vault.templates.iter().any(|t| &t.name == *n)).collect();
    let v = json!({
        "attunement": { "w_damping": c.w_damping, "w_curve": c.w_curve, "w_resid": c.w_resid, "w_invariant": c.w_invariant, "threshold": c.threshold, "resample": c.resample },
        "glyphs": glyphs,
        "binds_without_a_recorded_glyph": orphans,
    });
    out::emit(&v, || {
        println!("attunement: w_damping={} w_curve={} w_resid={} w_invariant={} threshold={} resample={}", c.w_damping, c.w_curve, c.w_resid, c.w_invariant, c.threshold, c.resample);
        if glyphs.is_empty() {
            println!("(no glyphs recorded)");
        }
        for g in &glyphs {
            println!(
                "  {:<16} {} blocks  winding {:+.2} closure {:.2}  {}",
                g["name"].as_str().unwrap_or(""),
                g["blocks"],
                g["winding"].as_f64().unwrap_or(0.0),
                g["closure"].as_f64().unwrap_or(0.0),
                g["action_text"].as_str().map_or("(unbound)".to_string(), |t| format!("-> {t}"))
            );
        }
        for n in &orphans {
            println!("  ! '{n}' is bound but has no recorded shape");
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_args_take_specs_and_legacy_virtual_keys() {
        assert_eq!(control_arg(Some("5")).unwrap(), ControlRef::from_vk(5));
        assert_eq!(control_arg(Some("0x05")).unwrap(), ControlRef::from_vk(5));
        let c = control_arg(Some("mouse:4")).unwrap();
        assert_eq!((c.page, c.usage), (9, 4));
        assert!(control_arg(Some("gesture:x")).is_err(), "only controls can hold the cast");
        assert!(control_arg(Some("nonsense")).is_err());
    }
}
