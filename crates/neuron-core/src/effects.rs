// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Shared lighting primitives — the small, model-agnostic pieces the Pattern × Spectrum engine is
//! built from. The open effect SYSTEM itself now lives in [`crate::pattern`] (the shapes) and
//! [`crate::spectrum`] (the colour programs); a layer is a [`pattern::LayerDef`](crate::pattern::LayerDef)
//! composited by [`pattern::Compositor`](crate::pattern::Compositor). This module keeps only what BOTH
//! halves (and the GUI inspector) share:
//!
//!   * the typed PARAM schema ([`Param`] / [`ParamKind`]) a pattern declares, which the inspector
//!     auto-renders into one control per knob,
//!   * the layer [`Blend`] modes + [`blend_px`] (how a layer composites over the ones beneath it), and
//!   * the colour-wheel helpers ([`rgb_hue`] / [`jitter_hue`]) the spectrum's hue motion reuses.
//!
//! There is no `FrameGen` here any more: every shape is a [`Pattern`](crate::pattern::Pattern) that
//! emits a field, coloured by a [`Spectrum`](crate::spectrum::Spectrum). Adding a look is a registry
//! entry + a `Pattern` impl (see [`crate::pattern`]), never a new effect struct here.

use crate::lighting::Rgb;
use serde::{Deserialize, Serialize};

// ── THE PARAM SCHEMA — a pattern declares its knobs AS DATA ───────────────────────────────
// The open-studio thesis applied to the UI: a pattern is a generator + a typed list of the knobs it
// honours. The GUI reads this table and auto-renders one control per param (a slider for a Range, a
// segment for an Enum, a switch for a Toggle). Adding/retuning a pattern's knobs is DATA, never new
// per-pattern UI. Patterns carry NO colour params — colour is the layer's Spectrum — but the `Color`
// kind is kept for the inspector's generic plumbing.

/// The TYPE of a declared param — drives which control the UI renders. `key` is the stable id the UI
/// passes back to the setter.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamKind {
    /// A colour value. Patterns never declare this (colour lives in the Spectrum); kept for the
    /// inspector's generic control plumbing.
    Color,
    /// A continuous slider. `(min, max, default)` in the param's own units.
    Range { min: f32, max: f32, default: f32 },
    /// A pick-one segment. `options` are the labels; the value is the chosen index.
    Enum { options: &'static [&'static str], default: u8 },
    /// An on/off switch.
    Toggle { default: bool },
}

/// One declared knob: a stable `key` (what the setter keys on), a human `label`, and its typed kind.
#[derive(Clone, Debug)]
pub struct Param {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    /// Conditional VISIBILITY: `Some((other_key, values))` shows this knob only while the layer's
    /// `other_key` enum currently holds one of `values`. Data-driven honesty — a knob that would
    /// no-op for the current configuration (e.g. the meter's audio `focus` while a CPU source is
    /// selected) simply isn't rendered, instead of sitting there dead. `None` = always shown.
    pub only_when: Option<(&'static str, &'static [u8])>,
}

// ── BLEND — how a layer's pixels combine with what's beneath them ─────────────────────────

/// How a layer's pixels combine with what's beneath them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Blend {
    Normal, // opaque over (within the region)
    Add,    // additive — stacked glows brighten
    Screen, // lighten — softer than add, never clips ugly
    /// Sprite/cutout: an unlit (pure-black) cell falls THROUGH to the layer beneath; a lit cell
    /// REPLACES it at true colour. The readout blend: a data layer (vitals gauge, on-air light)
    /// must vanish where it has nothing to say AND read its exact colour where it does — Normal
    /// would punch black holes when idle, Screen would wash the lit colour into whatever effect
    /// runs underneath (battery red over an aurora reads pink).
    Cut,
}

impl Blend {
    #[must_use]
    #[allow(clippy::should_implement_trait)] // Unknown names intentionally map to Normal.
    pub fn from_str(s: &str) -> Blend {
        match s.to_lowercase().as_str() {
            "add" => Blend::Add,
            "screen" => Blend::Screen,
            "cut" | "cutout" => Blend::Cut,
            _ => Blend::Normal,
        }
    }
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Blend::Add => "add",
            Blend::Screen => "screen",
            Blend::Cut => "cut",
            Blend::Normal => "normal",
        }
    }
}

/// Composite `over` onto `under` by `mode` — the per-cell core the [`Compositor`](crate::pattern::Compositor) runs.
#[must_use]
pub fn blend_px(under: Rgb, over: Rgb, mode: Blend) -> Rgb {
    match mode {
        Blend::Normal => over,
        Blend::Add => Rgb::new(
            under.r.saturating_add(over.r),
            under.g.saturating_add(over.g),
            under.b.saturating_add(over.b),
        ),
        Blend::Screen => {
            let s = |a: u8, b: u8| (255 - ((255 - u16::from(a)) * (255 - u16::from(b)) / 255)) as u8;
            Rgb::new(s(under.r, over.r), s(under.g, over.g), s(under.b, over.b))
        }
        // Cutout: black = "nothing to say" (fall through); anything lit replaces at true colour.
        Blend::Cut => {
            if over == Rgb::BLACK {
                under
            } else {
                over
            }
        }
    }
}

// ── breathe shape — the shared "living light" envelope ─────────────────────────────────────

/// The breathing envelope every alive effect exhales with: the same period as `sin`, but shaped
/// like a real resting breath — a quick inhale, a short dwell at the crest, then a long relax
/// (≈1:2 I:E with an ~7% top hold) — instead of a symmetric up-down wobble. `phase` is in RADIANS
/// (one cycle per TAU); output spans −1 at the trough to +1 at the crest, so a caller keeps its
/// existing `centre + amp * …` structure and only swaps the `sin` for a breathe.
#[must_use]
pub fn breathe_shape(phase: f32) -> f32 {
    let t = (phase / std::f32::consts::TAU).rem_euclid(1.0);
    // the inhale completes in the first third of the cycle and crests at sin's 90°; it then HOLDS,
    // and the relax runs the remaining ~60% — long enough that the follow-through visibly drifts
    // back down instead of snapping.
    const INHALE: f32 = 1.0 / 3.0;
    const CREST: f32 = 0.07;
    const REST: f32 = 1.0 - INHALE - CREST;
    let theta = if t < INHALE {
        let u = t / INHALE;
        let e = u * u * (3.0 - 2.0 * u); // smooth in, so the crest lands full, not peaked
        std::f32::consts::FRAC_PI_2 * e
    } else if t < INHALE + CREST {
        std::f32::consts::FRAC_PI_2 // the dwell
    } else {
        std::f32::consts::FRAC_PI_2 + std::f32::consts::FRAC_PI_2 * 3.0 * (t - INHALE - CREST) / REST
    };
    theta.sin()
}

// ── colour-wheel helpers — shared by the Spectrum's hue motion ────────────────────────────

/// Derive a hue (degrees, 0..360) from an Rgb. A greyscale/near-black colour has no hue, so it falls
/// back to a pleasant aurora green (≈150°) rather than collapsing to red.
#[must_use]
pub fn rgb_hue(c: Rgb) -> f32 {
    let r = f32::from(c.r) / 255.0;
    let g = f32::from(c.g) / 255.0;
    let b = f32::from(c.b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d < 0.02 {
        return 150.0; // no chroma → default aurora green
    }
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    h.rem_euclid(360.0)
}

/// Rotate a colour's HUE by `deg` degrees, preserving its saturation/value. A near-grey colour (no
/// chroma to turn) is returned unchanged. This is what [`Motion::Cycle`](crate::spectrum::Motion) uses.
#[must_use]
pub fn jitter_hue(base: Rgb, deg: f32) -> Rgb {
    if deg == 0.0 {
        return base;
    }
    let (rf, gf, bf) = (f32::from(base.r) / 255.0, f32::from(base.g) / 255.0, f32::from(base.b) / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    if max - min < 0.02 {
        return base; // no chroma → nothing to rotate
    }
    Rgb::from_hsv(rgb_hue(base) + deg, (max - min) / max, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_modes() {
        let a = Rgb::new(100, 100, 100);
        let b = Rgb::new(50, 0, 200);
        assert_eq!(blend_px(a, b, Blend::Normal), b, "normal is opaque-over");
        assert_eq!(
            blend_px(a, b, Blend::Add),
            Rgb::new(150, 100, 255),
            "add saturates"
        );
        // screen never goes below either input and never overflows
        let s = blend_px(a, b, Blend::Screen);
        assert!(s.r >= a.r && s.g >= a.g && s.b >= a.b);
    }

    #[test]
    fn blend_tag_round_trips() {
        for m in [Blend::Normal, Blend::Add, Blend::Screen] {
            assert_eq!(Blend::from_str(m.as_str()), m);
        }
        assert_eq!(Blend::from_str("nonsense"), Blend::Normal);
    }

    #[test]
    fn rgb_hue_reads_primaries_and_greys() {
        assert!((rgb_hue(Rgb::new(255, 0, 0)) - 0.0).abs() < 1.0);
        assert!((rgb_hue(Rgb::new(0, 255, 0)) - 120.0).abs() < 1.0);
        assert!((rgb_hue(Rgb::new(0, 0, 255)) - 240.0).abs() < 1.0);
        assert_eq!(rgb_hue(Rgb::new(20, 20, 20)), 150.0, "grey → default aurora green");
    }

    #[test]
    fn jitter_hue_rotates_chroma_and_passes_grey() {
        // red rotated +120° lands in the greens
        let g = jitter_hue(Rgb::new(255, 0, 0), 120.0);
        assert!(g.g > g.r && g.g > g.b);
        // a grey has no hue to turn — returned unchanged
        assert_eq!(jitter_hue(Rgb::new(40, 40, 40), 90.0), Rgb::new(40, 40, 40));
        // zero rotation is identity
        assert_eq!(jitter_hue(Rgb::new(1, 2, 3), 0.0), Rgb::new(1, 2, 3));
    }

    #[test]
    fn breathe_shape_is_an_asymmetric_1_2_breath() {
        let n = 240;
        let vals: Vec<f32> = (0..=n)
            .map(|k| breathe_shape(std::f32::consts::TAU * k as f32 / n as f32))
            .collect();
        let above = vals.iter().filter(|&&v| v > 0.0).count();
        let below = vals.iter().filter(|&&v| v < 0.0).count();
        // the positive (inhale + crest-hold) half owns the larger share of the cycle; the exhale
        // sprint down through zero is the shorter leg (1:2 I:E, not a 1:1 sine)
        assert!(above > n * 55 / 100, "the inhale-and-hold side should dominate the cycle (above={above})");
        assert!(below < n * 45 / 100, "the through-zero exhale should take the short leg (below={below})");
        // full −1..+1 range like sin, so `centre + amp * shape` behaves exactly like the old sine
        let peak = vals.iter().copied().fold(f32::MIN, f32::max);
        let trough = vals.iter().copied().fold(f32::MAX, f32::min);
        assert!(peak > 0.995 && trough < -0.995, "must swing the full range ({peak}, {trough})");
        // the crest actually HOLDS (a plateau, not a poke) — several samples sit at the top
        let near_top = vals.iter().filter(|&&v| v > 0.999).count();
        assert!(near_top >= 4, "the crest must dwell, not spike ({near_top})");
    }
}
