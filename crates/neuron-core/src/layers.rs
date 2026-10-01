// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Lighting authoring: the effect catalog and the layer stack a profile (or a device's saved look)
//! holds. A layer is a pattern, its knobs, a colour spectrum, an optional region and a blend; the
//! stack composites bottom-up. Everything the GUI's lighting page edits is expressible here, and
//! every knob is checked against the pattern's own schema before it is written.

use crate::authoring::Issue;
use crate::effects::{Blend, ParamKind};
use crate::lighting::Rgb;
use crate::pattern::{self, LayerDef};
use crate::spectrum::{Motion, Palette, Spectrum};

/// Where a layer stack lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LightTarget {
    /// A profile's `lighting` stack; `profile apply` paints it.
    Profile(String),
    /// The look a device resumes with when the app starts: `lighting.<pid>.layers` in `app.toml`.
    Device(u16),
}

impl LightTarget {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            LightTarget::Profile(n) => format!("profile:{n}"),
            LightTarget::Device(p) => format!("device:{p:04x}"),
        }
    }

    /// The stack as saved (empty when nothing is).
    pub fn load(&self) -> Result<Vec<LayerDef>, String> {
        match self {
            LightTarget::Profile(name) => Ok(crate::profile::Profile::load(name).map_err(|e| e.to_string())?.lighting),
            LightTarget::Device(pid) => {
                let t = crate::manage::app_table()?;
                let Some(layers) = t
                    .get("lighting")
                    .and_then(|l| l.get(format!("{pid:04x}")))
                    .and_then(|d| d.get("layers"))
                else {
                    return Ok(Vec::new());
                };
                layers.clone().try_into().map_err(|e: toml::de::Error| format!("saved lighting for {pid:04x}: {e}"))
            }
        }
    }

    /// Save the stack, preserving every other setting in the same file.
    pub fn save(&self, stack: &[LayerDef]) -> Result<(), String> {
        match self {
            LightTarget::Profile(name) => {
                let mut p = crate::profile::Profile::load(name).map_err(|e| e.to_string())?;
                p.lighting = stack.to_vec();
                p.save()
            }
            LightTarget::Device(pid) => {
                let mut t = crate::manage::app_table()?;
                let layers = toml::Value::try_from(stack.to_vec()).map_err(|e| e.to_string())?;
                let lighting = t
                    .entry("lighting")
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .ok_or("app.toml `lighting` is not a table")?;
                let dev = lighting
                    .entry(format!("{pid:04x}"))
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .ok_or("app.toml lighting entry is not a table")?;
                dev.insert("layers".into(), layers);
                crate::manage::save_app_table(&t)
            }
        }
    }
}

/// The frame rate a device's saved look streams at (`0` = unset, the app's per-device default).
pub fn device_fps(pid: u16) -> Result<u32, String> {
    let t = crate::manage::app_table()?;
    Ok(t.get("lighting")
        .and_then(|l| l.get(format!("{pid:04x}")))
        .and_then(|d| d.get("fps"))
        .and_then(toml::Value::as_integer)
        .map_or(0, |n| u32::try_from(n).unwrap_or(0)))
}

/// Set a device's stream frame rate (1-30), or clear it with `0`.
pub fn set_device_fps(pid: u16, fps: u32) -> Result<(), String> {
    if fps > 30 {
        return Err("fps must be 1-30 (0 clears it back to the default)".into());
    }
    let mut t = crate::manage::app_table()?;
    let lighting = t
        .entry("lighting")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or("app.toml `lighting` is not a table")?;
    let dev = lighting
        .entry(format!("{pid:04x}"))
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .ok_or("app.toml lighting entry is not a table")?;
    if fps == 0 {
        dev.remove("fps");
    } else {
        dev.insert("fps".into(), toml::Value::Integer(i64::from(fps)));
    }
    crate::manage::save_app_table(&t)
}

/// The effect catalog: every preset (a named look) and every pattern with its typed knobs.
#[must_use]
pub fn catalog_json() -> serde_json::Value {
    let presets: Vec<_> = pattern::presets()
        .iter()
        .map(|p| {
            serde_json::json!({
                "slug": p.slug, "label": p.label, "pattern": p.pattern, "group": p.group(),
                "blurb": p.blurb, "source": p.source,
            })
        })
        .collect();
    let patterns: Vec<_> = pattern::registry()
        .iter()
        .map(|d| {
            let params: Vec<_> = (d.params)()
                .iter()
                .map(|p| match &p.kind {
                    ParamKind::Range { min, max, default } => {
                        serde_json::json!({"key": p.key, "label": p.label, "kind": "range", "min": min, "max": max, "default": default})
                    }
                    ParamKind::Enum { options, default } => {
                        serde_json::json!({"key": p.key, "label": p.label, "kind": "enum", "options": options, "default": default})
                    }
                    ParamKind::Toggle { default } => {
                        serde_json::json!({"key": p.key, "label": p.label, "kind": "toggle", "default": default})
                    }
                    ParamKind::Color => serde_json::json!({"key": p.key, "label": p.label, "kind": "color"}),
                })
                .collect();
            serde_json::json!({
                "key": d.key, "label": d.label, "has_spectrum": d.has_spectrum,
                "readout": d.readout, "live_input": d.tile.live_input, "params": params,
            })
        })
        .collect();
    let user_effects: Vec<_> = crate::user_effects::list()
        .unwrap_or_default()
        .into_iter()
        .map(|(slug, name, tags)| serde_json::json!({"slug": slug, "name": name, "tags": tags}))
        .collect();
    serde_json::json!({
        "presets": presets,
        "patterns": patterns,
        "user_effects": user_effects,
        "blends": ["normal", "add", "screen", "cut"],
        "motions": ["hold", "drift", "cycle", "breathe", "flow"],
    })
}

/// A board rectangle: `(r0, c0, r1, c1)` corners on a `(rows, cols)` board.
pub type BoardRect = ((i32, i32, i32, i32), (u8, u8));

/// The pieces of a layer an agent can specify. `build_layer` makes a fresh layer from them and
/// `modify_layer` edits an existing one; an unset field leaves the layer's own value alone.
#[derive(Clone, Debug, Default)]
pub struct LayerMods {
    /// a named look (`fire`, `aurora`, …) to start from
    pub preset: Option<String>,
    /// a pattern key to start from (`uniform`, `axis`, …)
    pub pattern: Option<String>,
    /// a whole layer as JSON/TOML; other fields then modify it
    pub spec: Option<String>,
    /// re-tint to one colour (keeps the spectrum's motion)
    pub color: Option<String>,
    /// an evenly spaced gradient of these colours
    pub gradient: Vec<String>,
    /// `kind[:speed]` motion for a gradient (`drift:0.5`, `cycle`, `breathe:1`, `flow:0.3`, `hold`)
    pub motion: Option<String>,
    /// a whole spectrum as JSON/TOML
    pub spectrum: Option<String>,
    pub blend: Option<String>,
    /// explicit LED cells (row-major index), empty = leave alone
    pub region: Option<Vec<u32>>,
    /// a rectangle `(r0, c0, r1, c1)` on a `(rows, cols)` board
    pub rect: Option<BoardRect>,
    /// knob overrides by param key; an enum knob takes its option label or index
    pub params: Vec<(String, String)>,
    pub enabled: Option<bool>,
}

fn parse_color(s: &str) -> Result<Rgb, String> {
    Rgb::parse(s).ok_or_else(|| format!("'{s}' is not a colour (RRGGBB hex)"))
}

fn parse_blend(s: &str) -> Result<Blend, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "normal" => Ok(Blend::Normal),
        "add" => Ok(Blend::Add),
        "screen" => Ok(Blend::Screen),
        "cut" => Ok(Blend::Cut),
        other => Err(format!("blend '{other}' must be normal, add, screen or cut")),
    }
}

fn parse_motion(s: &str) -> Result<Motion, String> {
    let (kind, speed) = match s.split_once(':') {
        Some((k, v)) => (k, v.trim().parse::<f32>().map_err(|_| format!("motion speed '{v}' is not a number"))?),
        None => (s, 0.5),
    };
    let kind = kind.trim().to_ascii_lowercase();
    if !["hold", "drift", "cycle", "breathe", "flow"].contains(&kind.as_str()) {
        return Err(format!("motion '{kind}' must be hold, drift, cycle, breathe or flow"));
    }
    Ok(Motion::from_parts(&kind, speed, None, None))
}

/// Set one knob on a layer by param key, checked against the pattern's schema.
pub fn set_param(layer: &mut LayerDef, key: &str, raw: &str) -> Result<(), String> {
    let schema = pattern::pattern_params(&layer.pattern);
    let Some(p) = schema.iter().find(|p| p.key == key) else {
        let keys: Vec<&str> = schema.iter().map(|p| p.key).collect();
        return Err(format!(
            "pattern '{}' has no knob '{key}' (knobs: {})",
            layer.pattern,
            if keys.is_empty() { "none".to_string() } else { keys.join(", ") }
        ));
    };
    let raw = raw.trim();
    let v = match &p.kind {
        ParamKind::Range { min, max, .. } => {
            let v = raw.parse::<f32>().map_err(|_| format!("'{key}' needs a number"))?;
            if !(*min..=*max).contains(&v) {
                return Err(format!("'{key}' must be {min}..{max}, not {v}"));
            }
            v
        }
        ParamKind::Enum { options, .. } => {
            if let Some(i) = options.iter().position(|o| o.eq_ignore_ascii_case(raw)) {
                i as f32
            } else {
                match raw.parse::<usize>() {
                    Ok(i) if i < options.len() => i as f32,
                    _ => return Err(format!("'{key}' must be one of: {}", options.join(", "))),
                }
            }
        }
        ParamKind::Toggle { .. } => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "1" => 1.0,
            "false" | "off" | "0" => 0.0,
            _ => return Err(format!("'{key}' is a switch: on | off")),
        },
        ParamKind::Color => return Err(format!("'{key}' is a colour knob; set the layer's spectrum instead")),
    };
    layer.params.set(key, v);
    Ok(())
}

/// Edit `layer` with `mods` (everything except `preset`/`pattern`/`spec`, which pick a starting point).
pub fn modify_layer(layer: &mut LayerDef, mods: &LayerMods) -> Result<(), String> {
    if let Some(json) = &mods.spectrum {
        layer.spectrum = crate::authoring::parse_structured::<Spectrum>(json).or_else(|e| {
            // a bare string like "#ff0000" is also a spectrum; accept it without braces
            serde_json::from_str::<Spectrum>(&format!("\"{}\"", json.trim())).map_err(|_| e)
        })?;
    }
    if !mods.gradient.is_empty() {
        let cols = mods.gradient.iter().map(|c| parse_color(c)).collect::<Result<Vec<_>, _>>()?;
        let mut pal = Palette::gradient(cols);
        if let Some(m) = &mods.motion {
            pal.motion = parse_motion(m)?;
        }
        layer.spectrum = Spectrum::from_palette(pal);
    } else if let Some(m) = &mods.motion {
        let motion = parse_motion(m)?;
        let mut s = layer.spectrum.clone();
        for f in &mut s.seq {
            f.palette.motion = motion;
        }
        layer.spectrum = s;
    }
    if let Some(c) = &mods.color {
        layer.spectrum = layer.spectrum.recolored(parse_color(c)?);
    }
    if let Some(b) = &mods.blend {
        layer.blend = parse_blend(b)?;
    }
    if let Some(cells) = &mods.region {
        layer.region.clone_from(cells);
    }
    if let Some(((r0, c0, r1, c1), (rows, cols))) = mods.rect {
        layer.region = pattern::region_from_rect(r0, c0, r1, c1, rows, cols);
    }
    for (k, v) in &mods.params {
        set_param(layer, k, v)?;
    }
    if let Some(e) = mods.enabled {
        layer.enabled = e;
    }
    Ok(())
}

/// A fresh layer from a preset, a pattern, or a spec, then edited by the rest of `mods`.
pub fn build_layer(mods: &LayerMods) -> Result<LayerDef, String> {
    let mut layer = if let Some(spec) = &mods.spec {
        crate::authoring::parse_structured::<LayerDef>(spec)?
    } else if let Some(slug) = &mods.preset {
        pattern::preset_layer(slug).ok_or_else(|| {
            let slugs: Vec<&str> = pattern::presets().iter().map(|p| p.slug).collect();
            format!("no preset '{slug}' (presets: {})", slugs.join(", "))
        })?
    } else if let Some(key) = &mods.pattern {
        let def = pattern::pattern_def(key).ok_or_else(|| format!("no pattern '{key}' (patterns: {})", pattern::pattern_keys().join(", ")))?;
        LayerDef {
            pattern: def.key.to_string(),
            params: pattern::Params::defaults_for(def.key),
            spectrum: (def.default_spectrum)(),
            ..LayerDef::default()
        }
    } else {
        return Err("name what to add: --preset SLUG, --pattern KEY or --spec JSON (`neuron light catalog`)".into());
    };
    modify_layer(&mut layer, mods)?;
    Ok(layer)
}

/// Every problem with a layer: an unknown pattern, an unknown or out-of-range knob, a cell that is
/// not a valid index, a `custom` layer with no frame.
#[must_use]
pub fn check_layer(l: &LayerDef) -> Vec<Issue> {
    let mut out = Vec::new();
    let err = Issue::error;
    let Some(def) = pattern::pattern_def(&l.pattern) else {
        out.push(err(format!("unknown pattern '{}' (patterns: {})", l.pattern, pattern::pattern_keys().join(", "))));
        return out;
    };
    let schema = (def.params)();
    for (k, v) in &l.params.0 {
        match schema.iter().find(|p| p.key == k) {
            None => out.push(err(format!("pattern '{}' has no knob '{k}'", l.pattern))),
            Some(p) => {
                if let ParamKind::Range { min, max, .. } = p.kind {
                    if !(min..=max).contains(v) {
                        out.push(err(format!("knob '{k}' is {v}, outside {min}..{max}")));
                    }
                }
            }
        }
    }
    if l.pattern == "custom" && l.frame.is_empty() {
        out.push(err("a custom layer needs a frame (one [r,g,b] per LED)".into()));
    }
    out
}

/// Every problem with a whole stack, layer by layer.
#[must_use]
pub fn check_stack(stack: &[LayerDef]) -> Vec<Issue> {
    stack
        .iter()
        .enumerate()
        .flat_map(|(i, l)| {
            check_layer(l).into_iter().map(move |mut is| {
                is.message = format!("layer {i}: {}", is.message);
                is
            })
        })
        .collect()
}

/// Apply one semantic action edit to a cloned stack and return the validated replacement.
/// The caller persists and streams this returned value as one commit.
pub fn edit_stack(stack: &[LayerDef], edit: &crate::action::LightingLayerOp) -> Result<Vec<LayerDef>, String> {
    use crate::action::LightingLayerOp as Edit;
    let mut next = stack.to_vec();
    let at = |i: usize, n: usize| {
        if i < n { Ok(i) } else { Err(format!("no layer {i} ({n} layers)")) }
    };
    match edit {
        Edit::Toggle { index } => {
            let i = at(*index, next.len())?;
            next[i].enabled = !next[i].enabled;
        }
        Edit::Enable { index } | Edit::Disable { index } => {
            let i = at(*index, next.len())?;
            next[i].enabled = matches!(edit, Edit::Enable { .. });
        }
        Edit::Push { preset, layer } => {
            next.push(layer_source(preset.as_deref(), layer.as_ref())?);
        }
        Edit::Pop => {
            next.pop().ok_or("cannot pop an empty lighting stack")?;
        }
        Edit::Replace { index, preset, layer } => {
            let i = at(*index, next.len())?;
            next[i] = layer_source(preset.as_deref(), layer.as_ref())?;
        }
        Edit::Move { from, to } => {
            stack_move(&mut next, *from, *to)?;
        }
        Edit::Spectrum { index, spectrum } => {
            let i = at(*index, next.len())?;
            next[i].spectrum = spectrum.clone();
        }
        Edit::Params { index, params } => {
            let i = at(*index, next.len())?;
            for (key, value) in params {
                if !value.is_finite() {
                    return Err(format!("knob '{key}' must be finite"));
                }
                set_param(&mut next[i], key, &value.to_string())?;
            }
        }
        Edit::Region { index, cells } => {
            let i = at(*index, next.len())?;
            next[i].region = cells.clone();
        }
    }
    let issues = check_stack(&next);
    if let Some(issue) = issues.iter().find(|issue| issue.severity == crate::authoring::Severity::Error) {
        return Err(issue.message.clone());
    }
    Ok(next)
}

fn layer_source(preset: Option<&str>, layer: Option<&LayerDef>) -> Result<LayerDef, String> {
    match (preset, layer) {
        (Some(name), None) if !name.trim().is_empty() => {
            pattern::preset_layer(name).ok_or_else(|| format!("unknown lighting preset '{name}'"))
        }
        (None, Some(layer)) => Ok(layer.clone()),
        (Some(_), Some(_)) => Err("choose a preset or a layer definition, not both".into()),
        _ => Err("lighting push/replace needs a preset or layer definition".into()),
    }
}

/// Insert `layer` at `at` (default: the top of the stack). Returns its index.
pub fn stack_add(stack: &mut Vec<LayerDef>, layer: LayerDef, at: Option<usize>) -> Result<usize, String> {
    let i = at.unwrap_or(stack.len());
    if i > stack.len() {
        return Err(format!("index {i} is past the top of the {}-layer stack", stack.len()));
    }
    stack.insert(i, layer);
    Ok(i)
}

/// Remove the layer at `i`.
pub fn stack_remove(stack: &mut Vec<LayerDef>, i: usize) -> Result<LayerDef, String> {
    if i >= stack.len() {
        return Err(format!("no layer {i} ({} layers)", stack.len()));
    }
    Ok(stack.remove(i))
}

/// Move the layer at `from` to index `to` (0 is the bottom of the stack).
pub fn stack_move(stack: &mut Vec<LayerDef>, from: usize, to: usize) -> Result<(), String> {
    let n = stack.len();
    if from >= n || to >= n {
        return Err(format!("index out of range ({n} layers)"));
    }
    let l = stack.remove(from);
    stack.insert(to, l);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_and_pattern_build_valid_layers_with_edits() {
        let mut m = LayerMods { preset: Some("fire".into()), blend: Some("add".into()), ..LayerMods::default() };
        let fire = build_layer(&m).unwrap();
        assert_eq!(fire.pattern, "heat");
        assert_eq!(fire.blend, Blend::Add);
        assert!(check_layer(&fire).is_empty());

        m = LayerMods { pattern: Some("uniform".into()), color: Some("ff0000".into()), ..LayerMods::default() };
        let red = build_layer(&m).unwrap();
        assert_eq!(red.spectrum, Spectrum::solid(Rgb::new(255, 0, 0)));

        assert!(build_layer(&LayerMods::default()).is_err(), "nothing named = a clear error");
        assert!(build_layer(&LayerMods { preset: Some("nope".into()), ..LayerMods::default() }).is_err());
        assert!(build_layer(&LayerMods { pattern: Some("nope".into()), ..LayerMods::default() }).is_err());
    }

    #[test]
    fn knobs_are_checked_against_the_pattern_schema() {
        let mut l = build_layer(&LayerMods { preset: Some("cascade".into()), ..LayerMods::default() }).unwrap();
        assert!(set_param(&mut l, "no_such_knob", "1").is_err());
        let schema = pattern::pattern_params("rain");
        let (key, min, max) = schema
            .iter()
            .find_map(|p| match p.kind {
                ParamKind::Range { min, max, .. } => Some((p.key, min, max)),
                _ => None,
            })
            .expect("rain declares a range knob");
        set_param(&mut l, key, &format!("{max}")).unwrap();
        assert_eq!(l.params.f32(key, -1.0), max);
        assert!(set_param(&mut l, key, &format!("{}", max + 1000.0)).is_err());
        assert!(set_param(&mut l, key, "abc").is_err());
        let _ = min;
        l.params.set("bogus", 1.0);
        assert!(!check_layer(&l).is_empty(), "a hand-written bogus knob is reported");
    }

    #[test]
    fn gradient_motion_region_and_stack_ops() {
        let mut l = build_layer(&LayerMods { pattern: Some("axis".into()), ..LayerMods::default() }).unwrap();
        modify_layer(
            &mut l,
            &LayerMods {
                gradient: vec!["ff0000".into(), "0000ff".into()],
                motion: Some("drift:0.4".into()),
                rect: Some(((0, 0, 1, 2), (6, 22))),
                enabled: Some(false),
                ..LayerMods::default()
            },
        )
        .unwrap();
        assert_eq!(l.spectrum.seq[0].palette.stops.len(), 2);
        assert_eq!(l.spectrum.seq[0].palette.motion, Motion::Drift { speed: 0.4 });
        assert_eq!(l.region, vec![0, 1, 2, 22, 23, 24]);
        assert!(!l.enabled);
        assert!(parse_blend("multiply").is_err());
        assert!(parse_motion("spin").is_err());

        let mut stack = Vec::new();
        let a = build_layer(&LayerMods { preset: Some("static".into()), ..LayerMods::default() }).unwrap();
        let b = build_layer(&LayerMods { preset: Some("fire".into()), ..LayerMods::default() }).unwrap();
        stack_add(&mut stack, a.clone(), None).unwrap();
        stack_add(&mut stack, b.clone(), Some(0)).unwrap();
        assert_eq!(stack[0], b);
        stack_move(&mut stack, 0, 1).unwrap();
        assert_eq!(stack[1], b);
        assert!(stack_move(&mut stack, 0, 5).is_err());
        assert_eq!(stack_remove(&mut stack, 0).unwrap(), a);
        assert!(stack_remove(&mut stack, 3).is_err());
    }

    #[test]
    fn lighting_action_edits_clone_validate_and_keep_final_indexes() {
        use crate::action::LightingLayerOp as Edit;
        let base = vec![pattern::preset_layer("fire").unwrap(), pattern::preset_layer("aurora").unwrap()];
        let pushed = edit_stack(&base, &Edit::Push { preset: Some("fire".into()), layer: None }).unwrap();
        assert_eq!(pushed.len(), 3);
        assert_eq!(base.len(), 2, "the source stack is immutable");
        let moved = edit_stack(&pushed, &Edit::Move { from: 2, to: 0 }).unwrap();
        assert_eq!(moved[0], pushed[2], "destination is the final index after removal");
        let cleared = edit_stack(&moved, &Edit::Region { index: 0, cells: Vec::new() }).unwrap();
        assert!(cleared[0].region.is_empty());
        assert!(edit_stack(&base, &Edit::Push { preset: Some("missing".into()), layer: None }).is_err());
        assert!(edit_stack(&base, &Edit::Pop).is_ok());
        assert!(edit_stack(&[], &Edit::Pop).is_err());
        assert!(edit_stack(&base, &Edit::Move { from: 1, to: 2 }).is_err());
        assert!(edit_stack(&base, &Edit::Params { index: 0, params: [ ("bad".into(), 2.0) ].into() }).is_err());
    }

    #[test]
    fn catalog_lists_every_preset_and_pattern() {
        let c = catalog_json();
        assert_eq!(c["presets"].as_array().unwrap().len(), pattern::presets().len());
        assert_eq!(c["patterns"].as_array().unwrap().len(), pattern::registry().len());
    }

    /// The catalog's `user_effects` is the ONE place the GUI's YOUR-EFFECTS shelf and
    /// `neuron light saved list` both read, so a saved look must show up there the moment it lands —
    /// and a corrupt entry must degrade to "no user effects", never take the whole catalog (and with
    /// it every preset tile) down.
    #[test]
    fn catalog_carries_saved_effects_and_survives_a_bad_one() {
        let _r = crate::authoring::test_run_root();
        let layer = build_layer(&LayerMods { preset: Some("aurora".into()), ..LayerMods::default() }).unwrap();
        crate::user_effects::save("Shelf Test", &["cool".into()], std::slice::from_ref(&layer)).unwrap();
        let c = catalog_json();
        let found = c["user_effects"].as_array().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["slug"], "shelf-test");
        assert_eq!(found[0]["name"], "Shelf Test");
        assert_eq!(found[0]["tags"][0], "cool");

        std::fs::write(crate::user_effects::effect_path("shelf-test"), "this is not toml =").unwrap();
        let after = catalog_json();
        assert!(
            after["presets"].as_array().unwrap().len() == pattern::presets().len(),
            "a corrupt saved effect must not take the preset catalog down with it"
        );
        assert!(after["user_effects"].as_array().unwrap().is_empty());
    }

    #[test]
    fn stacks_persist_on_a_profile_and_a_device() {
        let _r = crate::authoring::test_run_root();
        crate::manage::create_profile("lit").unwrap();
        let layer = build_layer(&LayerMods { preset: Some("aurora".into()), ..LayerMods::default() }).unwrap();
        let profile = LightTarget::Profile("lit".into());
        profile.save(std::slice::from_ref(&layer)).unwrap();
        assert_eq!(profile.load().unwrap(), vec![layer.clone()]);

        let dev = LightTarget::Device(0x0221);
        assert!(dev.load().unwrap().is_empty());
        crate::manage::set_app_pref("phoenix", "false").unwrap();
        dev.save(std::slice::from_ref(&layer)).unwrap();
        assert_eq!(dev.load().unwrap(), vec![layer]);
        assert_eq!(crate::manage::app_table().unwrap()["phoenix"].as_bool(), Some(false), "sibling prefs survive");
        assert_eq!(device_fps(0x0221).unwrap(), 0);
        set_device_fps(0x0221, 24).unwrap();
        assert_eq!(device_fps(0x0221).unwrap(), 24);
        assert_eq!(dev.load().unwrap().len(), 1, "setting the rate keeps the layers");
        assert!(set_device_fps(0x0221, 31).is_err());
        set_device_fps(0x0221, 0).unwrap();
        assert_eq!(device_fps(0x0221).unwrap(), 0);
    }
}
