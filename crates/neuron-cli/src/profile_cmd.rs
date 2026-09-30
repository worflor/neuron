// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron profile`: a profile is one object: device settings, a lighting stack, and its own
//! binds. Create, edit, apply, rename, delete, export and import them, and route focused apps to
//! them (`profile route`).

use crate::{live, out};
use anyhow::{bail, Result};
use clap::Subcommand;
use neuron::manage::{self, ProfileBundle};
use neuron::profile::{AppRules, Profile};
use neuron::registry::Registry;
use serde_json::{json, Value};

#[derive(Subcommand)]
pub enum ProfileCmd {
    /// List saved profiles
    List,
    /// Show a profile: settings, lighting stack and binds
    Show { name: String },
    /// The profile the app or daemon last applied
    Active,
    /// Create an empty profile (then `profile set`, `light stack add --profile`, `bind add --profile`)
    New { name: String },
    /// Save a profile from explicit values (only the flags you pass are stored); replaces settings
    /// of an existing profile but keeps its lighting and binds
    Save {
        name: String,
        #[arg(long)]
        dpi: Option<u16>,
        #[arg(long)]
        polling: Option<u32>,
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        disable_alt_tab: bool,
        #[arg(long)]
        disable_win: bool,
        #[arg(long)]
        disable_alt_f4: bool,
        #[arg(long)]
        disable_alt_esc: bool,
        #[arg(long)]
        idle_secs: Option<u32>,
        #[arg(long)]
        in_game_wired: Option<u32>,
        #[arg(long)]
        in_game_dongle: Option<u32>,
        /// flash to onboard memory on apply (survives with no software running)
        #[arg(long)]
        persist: bool,
    },
    /// Edit settings of an existing profile: `profile set NAME dpi=1600 polling=500 disable-win=true`
    /// (`unset` clears an optional one; fields: `neuron catalog`)
    Set {
        name: String,
        /// KEY=VALUE pairs
        #[arg(required = true)]
        values: Vec<String>,
    },
    /// Apply a profile to the connected devices (from this process), or hand it to the running app
    Apply {
        name: String,
        /// have the running app apply it through its own device session (what the profile sheet does)
        #[arg(long)]
        live: bool,
    },
    /// Delete a profile and its binds
    Delete {
        name: String,
        #[arg(long)]
        yes: bool,
    },
    /// Rename a profile; its binds and every route that named it follow
    Rename { from: String, to: String },
    /// Capture the CURRENT live device settings into a profile (reads the hardware; also the
    /// clean way to take what Synapse left on it)
    Capture { name: String },
    /// Write a profile with its lighting and binds as one document (TOML, or JSON with --json)
    Export {
        name: String,
        /// write to this file instead of stdout
        #[arg(long)]
        out: Option<String>,
    },
    /// Read an exported profile (file, or `-` for stdin)
    Import {
        file: String,
        /// overwrite a profile of the same name
        #[arg(long)]
        replace: bool,
    },
    /// Which profile a focused app switches to
    Route {
        #[command(subcommand)]
        action: RouteCmd,
    },
    /// Legacy spelling of `route`: no args lists, `APP PROFILE` adds
    Autoswitch {
        app: Option<String>,
        profile: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RouteCmd {
    /// The routes in priority order, the fallback, and the focused app
    List,
    /// Route apps whose exe contains APP to PROFILE
    Add { app: String, profile: String },
    /// Remove the route at INDEX
    Rm { index: usize },
    /// Move a route (first match wins, so order is priority)
    Mv { from: usize, to: usize },
    /// The profile to return to when the focused app matches no route
    Default {
        profile: Option<String>,
        /// clear it: stay on the active profile
        #[arg(long, conflicts_with = "profile")]
        clear: bool,
    },
}

fn bundle_json(b: &ProfileBundle) -> Value {
    json!({
        "name": b.profile.name,
        "summary": b.profile.summary(),
        "profile": b.profile,
        "binds": b.rules.iter().enumerate().map(|(i, r)| neuron::authoring::rule_json(i, r)).collect::<Vec<_>>(),
    })
}

fn routes_json() -> Value {
    let rules = AppRules::load();
    let saved = neuron::profile::list();
    let dangling = rules.dangling(&saved);
    let focused = neuron::app::foreground_app();
    json!({
        "default": rules.default,
        "focused_app": focused,
        "routes": rules.rules.iter().enumerate().map(|(i, r)| json!({
            "index": i, "app": r.app, "profile": r.profile, "dangling": dangling.contains(&i),
        })).collect::<Vec<_>>(),
    })
}

pub fn run(cmd: ProfileCmd, reg: &Registry) -> Result<()> {
    match cmd {
        ProfileCmd::List => {
            let active = neuron::profile::restore_active_once();
            let mut rows = Vec::new();
            for n in neuron::profile::try_list()? {
                rows.push(match manage::export_profile(&n) {
                    Ok(b) => json!({
                        "name": b.profile.name, "file": n, "summary": b.profile.summary(), "binds": b.rules.len(),
                        "lighting_layers": b.profile.lighting.len(),
                        "active": Profile::file_key(&n) == Profile::file_key(&active),
                    }),
                    Err(e) => json!({ "name": n, "file": n, "error": e }),
                });
            }
            out::emit(&json!({ "active": active, "profiles": rows }), || {
                if rows.is_empty() {
                    println!("no profiles (create one: neuron profile new NAME)");
                }
                for r in &rows {
                    match r["error"].as_str() {
                        Some(e) => println!("  {:<16} (unreadable: {e})", r["name"].as_str().unwrap_or("")),
                        None => println!("{} {:<16} {}", if r["active"] == true { "*" } else { " " }, r["name"].as_str().unwrap_or(""), r["summary"].as_str().unwrap_or("")),
                    }
                }
            })
        }
        ProfileCmd::Show { name } => {
            let b = manage::export_profile(&name).map_err(anyhow::Error::msg)?;
            out::emit(&bundle_json(&b), || {
                println!("{}: {}", b.profile.name, b.profile.summary());
                for (i, r) in b.rules.iter().enumerate() {
                    println!("  bind [{i}] {}", r.summary());
                }
            })
        }
        ProfileCmd::Active => {
            let a = neuron::profile::restore_active_once();
            out::emit(&json!({ "active": a }), || println!("{}", if a.is_empty() { "(none)" } else { &a }))
        }
        ProfileCmd::New { name } => {
            let p = manage::create_profile(&name).map_err(anyhow::Error::msg)?;
            let b = manage::export_profile(&p.name).map_err(anyhow::Error::msg)?;
            live::finish(bundle_json(&b), format!("created profile '{}'", p.name));
            Ok(())
        }
        ProfileCmd::Save {
            name,
            dpi,
            polling,
            brightness,
            disable_alt_tab,
            disable_win,
            disable_alt_f4,
            disable_alt_esc,
            idle_secs,
            in_game_wired,
            in_game_dongle,
            persist,
        } => {
            if let Some(why) = neuron::profile::name_conflict(&name) {
                bail!("{why}");
            }
            let mut p = Profile {
                name: name.clone(),
                dpi,
                polling_hz: polling,
                brightness,
                disable_alt_tab,
                disable_win,
                disable_alt_f4,
                disable_alt_esc,
                idle_secs,
                in_game_polling: match (in_game_wired, in_game_dongle) {
                    (Some(w), Some(d)) => Some((w, d)),
                    _ => None,
                },
                persist,
                ..Default::default()
            };
            if p.is_empty() {
                bail!("nothing to save: pass at least one setting flag (see 'neuron profile save --help'), or use `profile new` and `profile set`");
            }
            if let Ok(existing) = Profile::load(&name) {
                out::note(format!("overwriting '{name}' settings ({})", existing.summary()));
                p.lighting = existing.lighting;
                p.dpi_stages = existing.dpi_stages;
            }
            p.save().map_err(anyhow::Error::msg)?;
            let b = manage::export_profile(&name).map_err(anyhow::Error::msg)?;
            live::finish(bundle_json(&b), format!("saved profile '{name}': {}", p.summary()));
            Ok(())
        }
        ProfileCmd::Set { name, values } => {
            let mut p = Profile::load(&name).map_err(|_| anyhow::anyhow!("no profile '{name}' (neuron profile new {name})"))?;
            for v in &values {
                let (k, val) = v.split_once('=').ok_or_else(|| anyhow::anyhow!("'{v}' should be KEY=VALUE"))?;
                manage::set_profile_field(&mut p, k.trim(), val).map_err(anyhow::Error::msg)?;
            }
            p.save().map_err(anyhow::Error::msg)?;
            let b = manage::export_profile(&name).map_err(anyhow::Error::msg)?;
            live::finish(bundle_json(&b), format!("'{name}': {}", b.profile.summary()));
            Ok(())
        }
        ProfileCmd::Apply { name, live: to_app } => {
            if to_app {
                Profile::load(&name).map_err(|_| anyhow::anyhow!("no profile '{name}'"))?;
                return match live::notify(&neuron::livesync::Command::ApplyProfile { name: name.clone() }) {
                    live::Live::Reloaded => out::emit(&json!({ "applied_by": "app", "profile": name }), || println!("the running app is applying '{name}'")),
                    other => bail!("no running app took it ({}); apply from this process with `neuron profile apply {name}`", other.word()),
                };
            }
            crate::profile_apply(reg, &name)
        }
        ProfileCmd::Delete { name, yes } => {
            let b = manage::export_profile(&name).map_err(|_| anyhow::anyhow!("no profile '{name}' (neuron profile list)"))?;
            if !yes {
                out::note(format!("'{name}': {} and {} bind(s)", b.profile.summary(), b.rules.len()));
                bail!("refusing to delete without --yes");
            }
            let rep = manage::delete_profile(&name).map_err(anyhow::Error::msg)?;
            if !rep.dangling_routes.is_empty() {
                out::note(format!("{} route(s) still point at '{name}': {}; re-point or remove them (`profile route`)", rep.dangling_routes.len(), rep.dangling_routes.join(", ")));
            }
            match (&rep.was_fallback, &rep.fallback_save_error) {
                (true, None) => out::note("it was the fallback profile; the fallback is now \"stay put\""),
                (true, Some(e)) => out::note(format!("it was the fallback profile, and clearing that failed ({e}): `profile route default --clear`")),
                _ => {}
            }
            live::finish(json!({ "deleted": name, "report": rep }), format!("deleted '{name}' and its {} bind(s)", rep.binds_removed));
            Ok(())
        }
        ProfileCmd::Rename { from, to } => {
            let rep = manage::rename_profile(&from, &to).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "renamed": { "from": from, "to": rep.landed }, "routes_followed": rep.routes_followed }), format!("renamed '{from}' to '{}' ({} route(s) followed)", rep.landed, rep.routes_followed));
            Ok(())
        }
        ProfileCmd::Capture { name } => crate::profile_capture(reg, &name),
        ProfileCmd::Export { name, out: dest } => {
            let b = manage::export_profile(&name).map_err(anyhow::Error::msg)?;
            let text = if out::json() { serde_json::to_string_pretty(&b)? } else { toml::to_string_pretty(&b)? };
            match dest {
                Some(path) => {
                    neuron::salvage::atomic_write(std::path::Path::new(&path), text.as_bytes())?;
                    out::note(format!("wrote {path}"));
                }
                None => println!("{text}"),
            }
            Ok(())
        }
        ProfileCmd::Import { file, replace } => {
            let spec = if file == "-" { "-".to_string() } else { format!("@{file}") };
            let text = crate::spec::read_spec(&spec)?;
            let b: ProfileBundle = if text.trim_start().starts_with('{') {
                serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("profile JSON: {e}"))?
            } else {
                toml::from_str(&text).map_err(|e| anyhow::anyhow!("profile TOML: {e}"))?
            };
            for (i, r) in b.rules.iter().enumerate() {
                let issues = neuron::authoring::check_rule(r, &neuron::authoring::Refs::default());
                if neuron::authoring::has_errors(&issues) {
                    bail!("bind {i}: {}", issues.iter().map(|x| x.message.as_str()).collect::<Vec<_>>().join("; "));
                }
            }
            let layer_issues = neuron::layers::check_stack(&b.profile.lighting);
            if neuron::authoring::has_errors(&layer_issues) {
                bail!("{}", layer_issues.iter().map(|x| x.message.as_str()).collect::<Vec<_>>().join("; "));
            }
            let landed = manage::import_bundle(&b, replace).map_err(anyhow::Error::msg)?;
            let back = manage::export_profile(&landed).map_err(anyhow::Error::msg)?;
            live::finish(bundle_json(&back), format!("imported '{landed}'"));
            Ok(())
        }
        ProfileCmd::Route { action } => route(action),
        ProfileCmd::Autoswitch { app, profile } => match (app, profile) {
            (Some(a), Some(p)) => route(RouteCmd::Add { app: a, profile: p }),
            (None, None) => route(RouteCmd::List),
            _ => bail!("give both an app and a profile, or neither to list"),
        },
    }
}

fn route(cmd: RouteCmd) -> Result<()> {
    match cmd {
        RouteCmd::List => {
            let v = routes_json();
            out::emit(&v, || {
                println!("focused app: {}", v["focused_app"].as_str().unwrap_or("(unknown)"));
                println!("fallback:    {}", v["default"].as_str().unwrap_or("(stay on the active profile)"));
                for r in v["routes"].as_array().into_iter().flatten() {
                    println!("  [{}] '{}' -> {}{}", r["index"], r["app"].as_str().unwrap_or(""), r["profile"].as_str().unwrap_or(""), if r["dangling"] == true { "   (profile missing)" } else { "" });
                }
            })
        }
        RouteCmd::Add { app, profile } => {
            let i = manage::route_add(&app, &profile).map_err(anyhow::Error::msg)?;
            live::finish(routes_json(), format!("route [{i}]: focus '{app}' -> profile '{profile}'"));
            Ok(())
        }
        RouteCmd::Rm { index } => {
            let gone = manage::route_remove(index).map_err(anyhow::Error::msg)?;
            live::finish(routes_json(), format!("removed route '{}' -> {}", gone.app, gone.profile));
            Ok(())
        }
        RouteCmd::Mv { from, to } => {
            manage::route_move(from, to).map_err(anyhow::Error::msg)?;
            live::finish(routes_json(), format!("moved route {from} to {to}"));
            Ok(())
        }
        RouteCmd::Default { profile, clear } => {
            if profile.is_none() && !clear {
                bail!("name the fallback profile, or --clear");
            }
            manage::route_default(profile.as_deref()).map_err(anyhow::Error::msg)?;
            live::finish(routes_json(), profile.map_or("fallback cleared".to_string(), |p| format!("fallback profile: {p}")));
            Ok(())
        }
    }
}
