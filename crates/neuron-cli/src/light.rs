// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! `neuron light`: the lighting look as data. A look is a bottom-up stack of layers (a pattern, its
//! knobs, a colour spectrum, a region, a blend); this edits the stack a profile holds
//! (`--profile`) or the look a device resumes with (`--pid`). Painting a stack onto hardware is
//! `neuron profile apply`.

use crate::{live, out};
use anyhow::{bail, Result};
use clap::{Args, Subcommand};
use neuron::layers::{self, LayerMods, LightTarget};
use neuron::pattern::LayerDef;
use serde_json::{json, Value};

#[derive(Args, Clone)]
pub struct TargetArgs {
    /// a profile's lighting stack
    #[arg(long)]
    pub profile: Option<String>,
    /// a device's saved look, by hex pid (the app resumes it at launch)
    #[arg(long, conflicts_with = "profile")]
    pub pid: Option<String>,
}

impl TargetArgs {
    fn target(&self) -> Result<LightTarget> {
        match (&self.profile, &self.pid) {
            (Some(p), None) => Ok(LightTarget::Profile(p.clone())),
            (None, Some(pid)) => {
                let raw = crate::parse_pid(pid)?;
                Ok(LightTarget::Device(neuron::registry::CanonicalPid::of(raw).get()))
            }
            _ => bail!("say whose stack: --profile NAME or --pid HEX"),
        }
    }
}

/// The shared layer flags.
#[derive(Args, Clone, Default)]
pub struct LayerArgs {
    /// start from a named look (`neuron light catalog`): fire, aurora, static, …
    #[arg(long)]
    preset: Option<String>,
    /// start from a pattern key: uniform, axis, radial, heat, rain, …
    #[arg(long)]
    pattern: Option<String>,
    /// start from a whole layer as JSON/TOML
    #[arg(long)]
    spec: Option<String>,
    /// re-tint to one colour, RRGGBB (keeps the spectrum's motion)
    #[arg(long)]
    color: Option<String>,
    /// an evenly spaced gradient: RRGGBB,RRGGBB,…
    #[arg(long, value_delimiter = ',')]
    gradient: Vec<String>,
    /// motion for the spectrum: hold | drift[:speed] | cycle[:speed] | breathe[:speed] | flow[:speed]
    #[arg(long)]
    motion: Option<String>,
    /// a whole spectrum as JSON/TOML
    #[arg(long)]
    spectrum: Option<String>,
    /// normal | add | screen | cut
    #[arg(long)]
    blend: Option<String>,
    /// explicit LED cells, row-major indexes: 0,1,2
    #[arg(long, value_delimiter = ',')]
    region: Option<Vec<u32>>,
    /// a rectangle of the board: r0,c0,r1,c1 (needs --board)
    #[arg(long, requires = "board", value_delimiter = ',')]
    rect: Option<Vec<i32>>,
    /// the board size the rectangle lives on: ROWSxCOLS (e.g. 6x22 for a full keyboard)
    #[arg(long)]
    board: Option<String>,
    /// a knob: key=value (enum knobs take an option name or index); repeatable
    #[arg(long = "param", value_name = "KEY=VALUE")]
    params: Vec<String>,
    /// turn the layer off
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
    /// turn the layer on
    #[arg(long)]
    enable: bool,
}

impl LayerArgs {
    fn mods(&self) -> Result<LayerMods> {
        let rect = match (&self.rect, &self.board) {
            (Some(r), Some(b)) => {
                let (rows, cols) = b
                    .split_once(['x', 'X'])
                    .and_then(|(r, c)| Some((r.parse::<u8>().ok()?, c.parse::<u8>().ok()?)))
                    .ok_or_else(|| anyhow::anyhow!("--board is ROWSxCOLS, e.g. 6x22"))?;
                let [r0, c0, r1, c1] = r.as_slice() else { bail!("--rect is r0,c0,r1,c1") };
                Some(((*r0, *c0, *r1, *c1), (rows, cols)))
            }
            _ => None,
        };
        let params = self
            .params
            .iter()
            .map(|p| {
                p.split_once('=')
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .ok_or_else(|| anyhow::anyhow!("--param takes key=value, not '{p}'"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(LayerMods {
            preset: self.preset.clone(),
            pattern: self.pattern.clone(),
            spec: self.spec.as_deref().map(crate::spec::read_spec).transpose()?,
            color: self.color.clone(),
            gradient: self.gradient.clone(),
            motion: self.motion.clone(),
            spectrum: self.spectrum.as_deref().map(crate::spec::read_spec).transpose()?,
            blend: self.blend.clone(),
            region: self.region.clone(),
            rect,
            params,
            enabled: if self.disable {
                Some(false)
            } else if self.enable {
                Some(true)
            } else {
                None
            },
        })
    }
}

#[derive(Subcommand)]
pub enum StackCmd {
    /// The layers, bottom first
    List(TargetArgs),
    /// Add a layer (on top unless --at)
    Add {
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        layer: LayerArgs,
        /// insert at this index (0 = bottom)
        #[arg(long)]
        at: Option<usize>,
    },
    /// Edit the layer at INDEX (colour, gradient, motion, blend, region, knobs, on/off)
    Set {
        index: usize,
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        layer: LayerArgs,
    },
    /// Remove the layer at INDEX
    Rm {
        index: usize,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Move a layer to another position
    Mv {
        from: usize,
        to: usize,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Remove every layer
    Clear {
        #[command(flatten)]
        target: TargetArgs,
        #[arg(long)]
        yes: bool,
    },
    /// Replace the whole stack from a JSON/TOML array of layers (`@file` or `-`)
    Replace {
        #[command(flatten)]
        target: TargetArgs,
        layers: String,
    },
}

#[derive(Subcommand)]
pub enum EffectCmd {
    /// Save the current stack as a named user effect
    Save {
        /// a name for the effect
        name: String,
        /// comma-separated tags
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Export a user effect as a portable TOML file
    Export {
        /// the effect's slug or name
        slug: String,
        /// output file path
        #[arg(long)]
        out: Option<String>,
    },
    /// Import a user effect from a TOML file
    Import {
        /// path to the effect file
        path: String,
    },
    /// Delete a user effect
    Delete {
        /// the effect's slug or name
        slug: String,
        #[arg(long)]
        yes: bool,
    },
    /// List all user effects
    List,
}

fn stack_json(t: &LightTarget, stack: &[LayerDef]) -> Value {
    json!({
        "target": t.label(),
        "layers": stack,
        "issues": layers::check_stack(stack),
        "paint": match t {
            LightTarget::Profile(n) => format!("neuron profile apply {n}"),
            LightTarget::Device(_) => "the app resumes this look at launch".to_string(),
        },
    })
}

fn describe(i: usize, l: &LayerDef) -> String {
    format!(
        "[{i}] {}{}  blend={}{}{}",
        l.pattern,
        neuron::pattern::slug_for_layer(l).map_or(String::new(), |s| format!(" ({s})")),
        l.blend.as_str(),
        if l.region.is_empty() { String::new() } else { format!("  region={} cell(s)", l.region.len()) },
        if l.enabled { "" } else { "  [off]" },
    )
}

fn save_and_echo(t: &LightTarget, stack: &[LayerDef], line: String) -> Result<()> {
    let issues = layers::check_stack(stack);
    if neuron::authoring::has_errors(&issues) {
        bail!("{}", issues.iter().map(|i| i.message.as_str()).collect::<Vec<_>>().join("; "));
    }
    t.save(stack).map_err(anyhow::Error::msg)?;
    let back = t.load().map_err(anyhow::Error::msg)?;
    if back != stack {
        bail!("read-back of the saved stack does not match what was written");
    }
    live::finish(stack_json(t, &back), line);
    Ok(())
}

/// `neuron lighting fps --pid HEX [N]`: the stream frame rate a device's saved look uses.
pub fn fps(pid: &str, value: Option<u32>) -> Result<()> {
    let raw = crate::parse_pid(pid)?;
    let raw = neuron::registry::CanonicalPid::of(raw).get();
    match value {
        None => {
            let n = layers::device_fps(raw).map_err(anyhow::Error::msg)?;
            out::emit(&json!({ "pid": format!("{raw:04x}"), "fps": n }), || println!("{}", if n == 0 { "default".to_string() } else { format!("{n} fps") }))
        }
        Some(n) => {
            layers::set_device_fps(raw, n).map_err(anyhow::Error::msg)?;
            let back = layers::device_fps(raw).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "pid": format!("{raw:04x}"), "fps": back }), format!("{raw:04x}: {}", if back == 0 { "default fps".to_string() } else { format!("{back} fps") }));
            Ok(())
        }
    }
}

pub fn catalog() -> Result<()> {
    let c = layers::catalog_json();
    out::emit(&c, || {
        println!("presets (--preset SLUG):");
        for p in c["presets"].as_array().into_iter().flatten() {
            println!("  {:<12} {:<7} {}", p["slug"].as_str().unwrap_or(""), p["group"].as_str().unwrap_or(""), p["blurb"].as_str().unwrap_or(""));
        }
        println!("\npatterns (--pattern KEY) and their knobs (--param KEY=VALUE):");
        for p in c["patterns"].as_array().into_iter().flatten() {
            println!("  {}", p["key"].as_str().unwrap_or(""));
            for k in p["params"].as_array().into_iter().flatten() {
                println!("      {:<14} {} {}", k["key"].as_str().unwrap_or(""), k["kind"].as_str().unwrap_or(""), k.get("options").map_or(String::new(), ToString::to_string));
            }
        }
    })
}

pub fn stack(cmd: StackCmd) -> Result<()> {
    match cmd {
        StackCmd::List(t) => {
            let t = t.target()?;
            let s = t.load().map_err(anyhow::Error::msg)?;
            out::emit(&stack_json(&t, &s), || {
                if s.is_empty() {
                    println!("{}: no layers", t.label());
                }
                for (i, l) in s.iter().enumerate() {
                    println!("{}", describe(i, l));
                }
            })
        }
        StackCmd::Add { target, layer, at } => {
            let t = target.target()?;
            let mut s = t.load().map_err(anyhow::Error::msg)?;
            let l = layers::build_layer(&layer.mods()?).map_err(anyhow::Error::msg)?;
            let i = layers::stack_add(&mut s, l.clone(), at).map_err(anyhow::Error::msg)?;
            save_and_echo(&t, &s, format!("added {}", describe(i, &l)))
        }
        StackCmd::Set { index, target, layer } => {
            let t = target.target()?;
            let mut s = t.load().map_err(anyhow::Error::msg)?;
            let mods = layer.mods()?;
            if mods.preset.is_some() || mods.pattern.is_some() || mods.spec.is_some() {
                bail!("`set` edits a layer in place; to change its pattern, `rm` it and `add` a new one");
            }
            let n = s.len();
            let Some(l) = s.get_mut(index) else { bail!("no layer {index} ({n} layers)") };
            layers::modify_layer(l, &mods).map_err(anyhow::Error::msg)?;
            let line = format!("edited {}", describe(index, l));
            save_and_echo(&t, &s, line)
        }
        StackCmd::Rm { index, target } => {
            let t = target.target()?;
            let mut s = t.load().map_err(anyhow::Error::msg)?;
            let gone = layers::stack_remove(&mut s, index).map_err(anyhow::Error::msg)?;
            save_and_echo(&t, &s, format!("removed {}", describe(index, &gone)))
        }
        StackCmd::Mv { from, to, target } => {
            let t = target.target()?;
            let mut s = t.load().map_err(anyhow::Error::msg)?;
            layers::stack_move(&mut s, from, to).map_err(anyhow::Error::msg)?;
            save_and_echo(&t, &s, format!("moved layer {from} to {to}"))
        }
        StackCmd::Clear { target, yes } => {
            let t = target.target()?;
            if !yes {
                bail!("refusing to clear the stack without --yes");
            }
            save_and_echo(&t, &[], "cleared".into())
        }
        StackCmd::Replace { target, layers: text } => {
            let t = target.target()?;
            let text = crate::spec::read_spec(&text)?;
            // a JSON/TOML array of layers; TOML has no top-level array, so it rides under `layers`
            #[derive(serde::Deserialize)]
            struct Doc {
                layers: Vec<LayerDef>,
            }
            let s: Vec<LayerDef> = match serde_json::from_str::<Vec<LayerDef>>(text.trim()) {
                Ok(v) => v,
                Err(je) => toml::from_str::<Doc>(&text).map(|d| d.layers).map_err(|te| anyhow::anyhow!("not a JSON array of layers ({je}) nor TOML with `[[layers]]` ({te})"))?,
            };
            let n = s.len();
            save_and_echo(&t, &s, format!("replaced with {n} layer(s)"))
        }
    }
}

pub fn effect(cmd: EffectCmd) -> Result<()> {
    match cmd {
        EffectCmd::Save { name, tags, target } => {
            let t = target.target()?;
            let stack = t.load().map_err(anyhow::Error::msg)?;
            if stack.is_empty() {
                bail!("{} has no layers — nothing to save", t.label());
            }
            let saved = neuron::user_effects::save(&name, &tags, &stack).map_err(anyhow::Error::msg)?;
            let slug = saved.slug.clone();
            live::finish(
                json!({ "slug": saved.slug, "name": name, "tags": tags, "layers": stack.len(), "replaced": saved.replaced }),
                if saved.replaced {
                    format!("replaced '{slug}' with the current stack")
                } else {
                    format!("saved '{name}' as {slug}")
                },
            );
            Ok(())
        }
        EffectCmd::Export { slug, out } => {
            let text = neuron::user_effects::export_bundle(&slug).map_err(anyhow::Error::msg)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, text.as_bytes()).map_err(|e| anyhow::anyhow!("{path}: {e}"))?;
                    live::finish(json!({ "slug": slug, "path": path }), format!("exported {slug} to {path}"));
                }
                None => {
                    out::emit(&json!({ "slug": slug, "toml": text }), || print!("{text}"))?;
                }
            }
            Ok(())
        }
        EffectCmd::Import { path } => {
            let saved = neuron::user_effects::import_from_file(std::path::Path::new(&path)).map_err(anyhow::Error::msg)?;
            live::finish(
                json!({ "slug": saved.slug, "path": path, "replaced": saved.replaced }),
                if saved.replaced {
                    format!("imported {slug} from {path} (replaced the one already there)", slug = saved.slug)
                } else {
                    format!("imported {slug} from {path}", slug = saved.slug)
                },
            );
            Ok(())
        }
        EffectCmd::Delete { slug, yes } => {
            if !yes {
                bail!("refusing to delete without --yes");
            }
            neuron::user_effects::delete(&slug).map_err(anyhow::Error::msg)?;
            live::finish(json!({ "slug": slug }), format!("deleted {slug}"));
            Ok(())
        }
        EffectCmd::List => {
            let effects = neuron::user_effects::list().map_err(anyhow::Error::msg)?;
            let json = json!({
                "effects": effects
                    .iter()
                    .map(|(slug, name, tags)| json!({ "slug": slug, "name": name, "tags": tags }))
                    .collect::<Vec<_>>()
            });
            out::emit(&json, || {
                if effects.is_empty() {
                    println!("no saved effects yet — build a look, then `neuron light saved save \"NAME\"`");
                }
                for (slug, name, tags) in &effects {
                    println!("  {:<20} {:<24} {}", slug, name, tags.join(", "));
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_is_required_and_unambiguous() {
        assert!(TargetArgs { profile: None, pid: None }.target().is_err());
        assert_eq!(TargetArgs { profile: Some("p".into()), pid: None }.target().unwrap(), LightTarget::Profile("p".into()));
        assert_eq!(TargetArgs { profile: None, pid: Some("0221".into()) }.target().unwrap(), LightTarget::Device(0x0221));
        assert!(TargetArgs { profile: None, pid: Some("zz".into()) }.target().is_err());
    }

    #[test]
    fn layer_flags_become_layer_mods() {
        let a = LayerArgs {
            preset: Some("fire".into()),
            params: vec!["speed=2".into()],
            rect: Some(vec![0, 0, 1, 2]),
            board: Some("6x22".into()),
            disable: true,
            ..LayerArgs::default()
        };
        let m = a.mods().unwrap();
        assert_eq!(m.params, vec![("speed".to_string(), "2".to_string())]);
        assert_eq!(m.rect, Some(((0, 0, 1, 2), (6, 22))));
        assert_eq!(m.enabled, Some(false));
        assert!(LayerArgs { params: vec!["nokey".into()], ..LayerArgs::default() }.mods().is_err());
        assert!(LayerArgs { rect: Some(vec![0, 0, 1, 1]), board: Some("big".into()), ..LayerArgs::default() }.mods().is_err());
    }
}
