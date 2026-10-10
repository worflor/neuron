// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Pairing — which gradient suits which effect, as a score rather than a list.
//!
//! A gradient is reduced to a [`Look`]: five numbers measured in OKLab, so they track what the eye
//! sees rather than the raw RGB. An effect reads its palette coordinate `u` in one of three ways
//! ([`Reading`]); each reading weights the look's numbers into a fit in 0..1. Hand-picked pins from
//! `data/pairings.toml` rank first; the best-scoring remainder fills the other slots.
//!
//! Every number is a closed form over the sampled ramp, so the laws are testable: an evenly spaced hue
//! wheel has spread 1, a linear grey ramp has monotonicity 1, a ramp whose ends meet has closure 1.

use crate::lighting::Rgb;
use crate::pattern::{pattern_def, preset_by_slug, preset_layer, LayerDef, PaletteAddressing};
use crate::spectrum::{gradient_index, gradient_preset, Palette, Spectrum, Stop, GRADIENT_PRESETS};
use serde::Deserialize;

const SAMPLES: usize = 32;
/// Most pairings an effect shows (pins first, then the best-scoring remainder).
const MAX_PAIRS: usize = 4;
/// A derived pairing must fit at least this well to be offered.
const MIN_FIT: f32 = 0.62;
/// Two looks closer than this (summed over the six numbers) read as the same gradient, so a derived
/// pairing is skipped when it resembles the effect's own colours or one already offered.
const TOO_ALIKE: f32 = 0.25;

/// A gradient's measured character; every field is 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    /// How steadily it brightens along `u`: Pearson r of `u` against lightness, clipped at 0.
    pub mono: f32,
    /// Hue variety: 1 − |Σ c·e^{ih}| / Σ c over the samples (0 = one hue, 1 = the wheel is covered).
    pub span: f32,
    /// Mean chroma against a vivid reference.
    pub vivid: f32,
    /// Lightness range, max − min.
    pub range: f32,
    /// How well the ends meet: 1 when the first and last colours match, so a scrolling ramp tiles.
    pub closure: f32,
    /// Lightest-dark: the minimum lightness along the ramp. A dark foot vanishes under a mask.
    pub floor: f32,
}

/// OKLab (L, a, b) of an sRGB colour.
fn oklab(c: Rgb) -> (f32, f32, f32) {
    crate::spectrum::rgb_to_oklab(c)
}

/// Measure a palette.
#[must_use]
pub fn look(p: &Palette) -> Look {
    let n = SAMPLES;
    let nf = n as f32;
    let labs: Vec<(f32, f32, f32)> = (0..n).map(|i| oklab(p.sample(i as f32 / (nf - 1.0)))).collect();
    let (mu, ml) = (0.5, labs.iter().map(|c| c.0).sum::<f32>() / nf);
    let (mut cov, mut vu, mut vl) = (0.0, 0.0, 0.0);
    for (i, c) in labs.iter().enumerate() {
        let du = i as f32 / (nf - 1.0) - mu;
        let dl = c.0 - ml;
        cov += du * dl;
        vu += du * du;
        vl += dl * dl;
    }
    let mono = if vl < 1e-9 { 0.0 } else { (cov / (vu * vl).sqrt()).max(0.0) };
    let (mut x, mut y, mut csum) = (0.0f32, 0.0f32, 0.0f32);
    for &(_, a, b) in &labs {
        x += a;
        y += b;
        csum += a.hypot(b);
    }
    // hue is meaningless on a near-grey ramp, so spread fades in with mean chroma (full by 0.05)
    let span = if csum < 1e-6 { 0.0 } else { (1.0 - x.hypot(y) / csum) * (csum / nf / 0.05).min(1.0) };
    let vivid = (csum / nf / 0.25).min(1.0);
    let (lo, hi) = labs.iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| (lo.min(c.0), hi.max(c.0)));
    let floor = lo.clamp(0.0, 1.0);
    let (f, t) = (labs[0], labs[n - 1]);
    let gap = ((f.0 - t.0).powi(2) + (f.1 - t.1).powi(2) + (f.2 - t.2).powi(2)).sqrt();
    Look { mono, span, vivid, range: (hi - lo).clamp(0.0, 1.0), closure: 1.0 - (gap / 0.6).min(1.0), floor }
}

/// How an effect spends its palette coordinate `u`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reading {
    /// `u` is energy (flames, typing heat): low is dim, high is hot — wants a ramp that brightens.
    Energy,
    /// `u` is position or phase (waves, wheels, flows): wants variety, vividness, and ends that meet.
    Position,
    /// `u` runs along a streak or an age (comets, rain, ripples, sparks): wants contrast and colour.
    Streak,
}

/// The reading of a preset, or `None` when its colour isn't a free choice (fed, full-colour or
/// single-colour looks never get pairings).
#[must_use]
pub fn reading(slug: &str) -> Option<Reading> {
    pattern_reading(preset_by_slug(slug)?.pattern)
}

/// The reading of a pattern key (see [`reading`]).
#[must_use]
pub fn pattern_reading(pattern: &str) -> Option<Reading> {
    match pattern {
        "heat" | "thermal" => Some(Reading::Energy),
        "flow" => Some(Reading::Position),
        "rain" | "comet" | "sparkle" | "ring" | "ignite" => Some(Reading::Streak),
        _ => None,
    }
}

/// Fit of a look to a reading, 0..1: a convex weighting of the look's numbers. When the effect
/// addresses the palette by board position (`placement`), a gradient with a dark foot would blank
/// whole stretches of the board under the effect's own brightness mask, so the fit scales by how light
/// the ramp's darkest point is (full at 0.35 lightness and up).
#[must_use]
pub fn fit(r: Reading, l: &Look, placement: bool) -> f32 {
    let (wm, ws, wv, wr, wc) = match r {
        Reading::Energy => (0.40, 0.15, 0.15, 0.30, 0.0),
        Reading::Position => (0.0, 0.40, 0.30, 0.0, 0.30),
        Reading::Streak => (0.15, 0.20, 0.30, 0.35, 0.0),
    };
    let base = wm * l.mono + ws * l.span + wv * l.vivid + wr * l.range + wc * l.closure;
    if placement { base * (l.floor / 0.35).min(1.0) } else { base }
}

fn distance(a: &Look, b: &Look) -> f32 {
    (a.mono - b.mono).abs() + (a.span - b.span).abs() + (a.vivid - b.vivid).abs()
        + (a.range - b.range).abs() + (a.closure - b.closure).abs() + (a.floor - b.floor).abs()
}

/// An energy ramp needs a cold end: a gradient whose colours are all bright and close together
/// (ice, bubble) leaves a heat map flat, because cold and hot cells read almost the same. Give such
/// a ramp a dark foot of its own first colour (8% of it) and squeeze the original into the top 82%, so
/// heat reads as dim → bright in the gradient's own hue. Ramps that already start dark pass through.
#[must_use]
pub fn energy_foot(p: &Palette) -> Palette {
    const FOOT_AT: f32 = 0.18;
    let Some(first) = p.stops.first() else { return p.clone() };
    if look(p).floor < 0.2 {
        return p.clone();
    }
    let c = first.col;
    let dim = |v: u8| (f32::from(v) * 0.08) as u8;
    let mut stops = vec![Stop::new(Rgb::new(dim(c.r), dim(c.g), dim(c.b)), 0.0)];
    stops.extend(p.stops.iter().map(|s| Stop::new(s.col, FOOT_AT + (1.0 - FOOT_AT) * s.at)));
    Palette { stops, motion: p.motion, interp: p.interp }
}

/// The palette a gradient preset becomes on an effect with this reading.
fn dressed(r: Reading, p: Palette) -> Palette {
    if r == Reading::Energy { energy_foot(&p) } else { p }
}

/// The palette `p` should wear on a pattern: an energy pattern gets [`energy_foot`], others as-is.
#[must_use]
pub fn palette_for_pattern(pattern: &str, p: Palette) -> Palette {
    match pattern_reading(pattern) {
        Some(r) => dressed(r, p),
        None => p,
    }
}

/// Where a pairing came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Pinned,
    Derived,
}

/// One suggestion: apply gradient preset `gradient` (an index into [`GRADIENT_PRESETS`]) to an effect,
/// optionally in one of its modes (`params` override the preset's knobs; `on` names the mode).
#[derive(Clone, Debug, PartialEq)]
pub struct Pairing {
    pub gradient: usize,
    pub source: Source,
    pub fit: f32,
    pub params: Vec<(String, f32)>,
    pub on: Option<String>,
}

impl Pairing {
    /// The chip's name: the gradient, plus the mode it rides on ("bubble on matrix").
    #[must_use]
    pub fn label(&self) -> String {
        let g = GRADIENT_PRESETS[self.gradient].0;
        match &self.on {
            Some(m) => format!("{g} on {m}"),
            None => g.to_owned(),
        }
    }
}

/// The gradient an effect already wears by default; it is never offered back to the same effect.
const OWN: &[(&str, &str)] = &[("fire", "fire"), ("typingheat", "thermal"), ("aurora", "aurora")];

fn is_own(slug: &str, gradient: usize) -> bool {
    OWN.iter().any(|(e, g)| e.eq_ignore_ascii_case(slug) && gradient_index(g) == Some(gradient))
}

#[derive(Deserialize)]
struct Pin {
    effect: String,
    with: String,
    /// Knob overrides, e.g. `{ mode = 1 }`.
    #[serde(default)]
    params: std::collections::BTreeMap<String, f32>,
    /// The mode's name for the chip ("matrix"), when `params` selects one.
    #[serde(default)]
    on: Option<String>,
}

#[derive(Deserialize)]
struct PinFile {
    #[serde(default)]
    closed: Vec<String>,
    pin: Vec<Pin>,
    #[serde(default)]
    skip: Vec<Pin>,
}

fn pins() -> Vec<(String, usize, Pin)> {
    let f: PinFile = toml::from_str(include_str!("../data/pairings.toml")).expect("pairings.toml parses");
    f.pin
        .into_iter()
        .filter_map(|p| gradient_index(&p.with).map(|i| (p.effect.clone(), i, p)))
        .collect()
}

fn skipped(slug: &str, gradient: usize) -> bool {
    let f: PinFile = toml::from_str(include_str!("../data/pairings.toml")).expect("pairings.toml parses");
    f.skip
        .iter()
        .any(|s| s.effect.eq_ignore_ascii_case(slug) && gradient_index(&s.with) == Some(gradient))
}

/// The pairings for an effect slug: its pins in file order, then the best-scoring gradients (≥
/// [`MIN_FIT`]) that don't resemble the effect's own colours or one already offered, up to
/// [`MAX_PAIRS`]. Empty for an effect with no [`Reading`].
#[must_use]
pub fn pairings_for(slug: &str) -> Vec<Pairing> {
    let (Some(r), Some(preset)) = (reading(slug), preset_by_slug(slug)) else { return Vec::new() };
    let placement = pattern_def(preset.pattern)
        .is_some_and(|d| d.palette_addressing == PaletteAddressing::Placement);
    let looks: Vec<Look> = (0..GRADIENT_PRESETS.len())
        .map(|i| look(&dressed(r, gradient_preset(i).expect("in range"))))
        .collect();
    let fits: Vec<f32> = looks.iter().map(|l| fit(r, l, placement)).collect();
    let own = (preset.spectrum)().seq.first().map(|f| look(&f.palette));
    let mut out: Vec<Pairing> = pins()
        .into_iter()
        .filter(|(e, g, _)| e.eq_ignore_ascii_case(slug) && !is_own(slug, *g))
        .map(|(_, g, p)| Pairing {
            gradient: g,
            source: Source::Pinned,
            fit: fits[g],
            params: p.params.into_iter().collect(),
            on: p.on,
        })
        .collect();
    let closed = {
        let f: PinFile = toml::from_str(include_str!("../data/pairings.toml")).expect("pairings.toml parses");
        f.closed.iter().any(|e| e.eq_ignore_ascii_case(slug))
    };
    if closed {
        return out;
    }
    let mut rest: Vec<usize> = (0..fits.len())
        .filter(|g| !is_own(slug, *g) && !skipped(slug, *g) && !out.iter().any(|p| p.gradient == *g && p.params.is_empty()))
        .collect();
    rest.sort_by(|a, b| fits[*b].total_cmp(&fits[*a]));
    for g in rest {
        if out.len() >= MAX_PAIRS || fits[g] < MIN_FIT {
            break;
        }
        let alike = own.as_ref().is_some_and(|o| distance(o, &looks[g]) < TOO_ALIKE)
            || out.iter().any(|p| distance(&looks[p.gradient], &looks[g]) < TOO_ALIKE);
        if !alike {
            out.push(Pairing { gradient: g, source: Source::Derived, fit: fits[g], params: Vec::new(), on: None });
        }
    }
    out
}

/// The layer for a pairing on `slug`: the preset with its spectrum swapped and any mode knobs set.
#[must_use]
pub fn pairing_layer(slug: &str, pairing: &Pairing) -> Option<LayerDef> {
    let mut layer = preset_layer(slug)?;
    layer.spectrum = Spectrum::from_palette(palette_for_pattern(&layer.pattern, gradient_preset(pairing.gradient)?));
    for (k, v) in &pairing.params {
        layer.params.set(k, *v);
    }
    Some(layer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grey_ramp() -> Palette {
        Palette::gradient(vec![Rgb::new(0, 0, 0), Rgb::new(255, 255, 255)])
    }

    #[test]
    fn grey_ramp_is_monotone_colourless_and_open() {
        let l = look(&grey_ramp());
        assert!(l.mono > 0.97, "{l:?}");
        assert!(l.span < 0.01 && l.vivid < 0.01 && l.closure < 0.01, "{l:?}");
        assert!(l.range > 0.99);
    }

    #[test]
    fn evenly_spaced_wheel_covers_every_hue_and_closes() {
        let hues: Vec<Rgb> = (0..=6).map(|k| Rgb::from_hsv(k as f32 * 60.0, 1.0, 1.0)).collect();
        let l = look(&Palette::gradient(hues));
        assert!(l.span > 0.9, "{l:?}");
        assert!(l.closure > 0.99, "{l:?}");
    }

    #[test]
    fn solid_has_no_structure() {
        let l = look(&Palette::solid(Rgb::new(200, 40, 40)));
        assert!(l.mono == 0.0 && l.range < 1e-4 && l.closure > 0.99);
    }

    #[test]
    fn pins_resolve_and_lead() {
        let f: PinFile = toml::from_str(include_str!("../data/pairings.toml")).unwrap();
        assert_eq!(f.pin.len(), pins().len(), "a pin names an unknown gradient");
        for p in f.pin.iter().filter(|p| !p.params.is_empty()) {
            assert!(p.on.is_some(), "pin {} with {} sets knobs but names no mode", p.effect, p.with);
        }
        for sk in &f.skip {
            assert!(gradient_index(&sk.with).is_some(), "skip names an unknown gradient {}", sk.with);
        }
        for p in &f.pin {
            assert!(reading(&p.effect).is_some(), "pin on '{}' has no reading", p.effect);
        }
        let comet = pairings_for("comet");
        assert_eq!(comet[0].source, Source::Pinned);
        assert_eq!(GRADIENT_PRESETS[comet[0].gradient].0, "rainbow");
        let names = |slug: &str| -> Vec<String> { pairings_for(slug).iter().map(Pairing::label).collect() };
        assert_eq!(&names("comet")[..3], ["rainbow", "bubble", "sunset on light painting"]);
        assert_eq!(names("cascade")[0], "bubble on matrix");
        assert_eq!(&names("ripple")[..2], ["bubble", "sunset"]);
        assert_eq!(names("typingheat")[0], "rainbow");
        assert_eq!(names("fire"), ["aurora", "sunset"], "fire is a closed list");
        assert_eq!(names("comet")[2], "sunset on light painting");
        assert_eq!(names("cascade")[1], "aurora");
        assert_eq!(names("starlight")[..2], ["rainbow", "bubble"]);
        assert_eq!(names("aurora")[0], "fire");
    }

    #[test]
    fn pairing_layer_swaps_only_the_spectrum() {
        let base = preset_layer("comet").unwrap();
        let pair = Pairing { gradient: 0, source: Source::Pinned, fit: 0.0, params: Vec::new(), on: None };
        let l = pairing_layer("comet", &pair).unwrap();
        assert_eq!((&l.pattern, &l.params), (&base.pattern, &base.params));
        assert_ne!(l.spectrum, base.spectrum);
        let matrix = Pairing { params: vec![("mode".into(), 1.0)], on: Some("matrix".into()), ..pair.clone() };
        assert_eq!(pairing_layer("cascade", &matrix).unwrap().params.f32("mode", 0.0), 1.0);
        assert_eq!(matrix.label(), "rainbow on matrix");
        assert!(pairing_layer("comet", &Pairing { gradient: 99, ..pair }).is_none());
    }

    #[test]
    fn dark_footed_ramps_are_penalised_when_placement_addressed() {
        let dark = look(&Palette::gradient(vec![Rgb::new(0, 0, 0), Rgb::new(255, 120, 0), Rgb::new(255, 255, 255)]));
        assert!(fit(Reading::Streak, &dark, true) < 0.2 * fit(Reading::Streak, &dark, false) + 1e-6);
    }

    #[test]
    fn derived_pairings_are_distinct_from_each_other_and_the_effect() {
        for s in ["fire", "typingheat", "comet", "reactive", "ripple", "cascade", "starlight", "aurora"] {
            let ps = pairings_for(s);
            assert!(ps.len() <= MAX_PAIRS, "{s}");
            for (i, a) in ps.iter().enumerate().filter(|(_, p)| p.source == Source::Derived) {
                for b in &ps[..i] {
                    let d = distance(&look(&gradient_preset(a.gradient).unwrap()), &look(&gradient_preset(b.gradient).unwrap()));
                    assert!(d >= TOO_ALIKE, "{s}: {} ~ {}", GRADIENT_PRESETS[a.gradient].0, GRADIENT_PRESETS[b.gradient].0);
                }
            }
        }
    }

    #[test]
    fn energy_foot_gives_bright_ramps_a_cold_end_and_leaves_dark_ones_alone() {
        let ocean = gradient_preset(gradient_index("ice").unwrap()).unwrap();
        assert!(look(&ocean).floor > 0.2);
        let dressed = energy_foot(&ocean);
        assert!(look(&dressed).floor < 0.15 && look(&dressed).range > look(&ocean).range);
        assert_eq!(dressed.stops.len(), ocean.stops.len() + 1);
        let dark = Palette::gradient(vec![Rgb::new(0, 0, 0), Rgb::new(255, 120, 0), Rgb::new(255, 255, 255)]);
        assert_eq!(energy_foot(&dark), dark);
    }

    #[test]
    fn an_effect_is_never_offered_its_own_gradient() {
        for (slug, own) in OWN {
            let g = gradient_index(own).expect("OWN names a gradient");
            assert!(pairings_for(slug).iter().all(|p| p.gradient != g), "{slug} offered {own}");
        }
    }

    #[test]
    fn fed_and_flat_effects_get_none() {
        for s in ["static", "wave", "colorwheel", "audiometer", "vitals", "ambient"] {
            assert!(pairings_for(s).is_empty(), "{s}");
        }
    }

    #[test]
    fn position_effects_prefer_the_closed_wheel() {
        let best = (0..GRADIENT_PRESETS.len())
            .max_by(|&a, &b| {
                let f = |i| fit(Reading::Position, &look(&gradient_preset(i).unwrap()), false);
                f(a).total_cmp(&f(b))
            })
            .unwrap();
        assert_eq!(GRADIENT_PRESETS[best].0, "rainbow");
    }
}
