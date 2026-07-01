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
}

// ── BLEND — how a layer's pixels combine with what's beneath them ─────────────────────────

/// How a layer's pixels combine with what's beneath them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Blend {
    Normal, // opaque over (within the region)
    Add,    // additive — stacked glows brighten
    Screen, // lighten — softer than add, never clips ugly
}

impl Blend {
    pub fn from_str(s: &str) -> Blend {
        match s.to_lowercase().as_str() {
            "add" => Blend::Add,
            "screen" => Blend::Screen,
            _ => Blend::Normal,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Blend::Add => "add",
            Blend::Screen => "screen",
            Blend::Normal => "normal",
        }
    }
}

/// Composite `over` onto `under` by `mode` — the per-cell core the [`Compositor`](crate::pattern::Compositor) runs.
pub fn blend_px(under: Rgb, over: Rgb, mode: Blend) -> Rgb {
    match mode {
        Blend::Normal => over,
        Blend::Add => Rgb::new(
            under.r.saturating_add(over.r),
            under.g.saturating_add(over.g),
            under.b.saturating_add(over.b),
        ),
        Blend::Screen => {
            let s = |a: u8, b: u8| (255 - ((255 - a as u16) * (255 - b as u16) / 255)) as u8;
            Rgb::new(s(under.r, over.r), s(under.g, over.g), s(under.b, over.b))
        }
    }
}

// ── colour-wheel helpers — shared by the Spectrum's hue motion ────────────────────────────

/// Derive a hue (degrees, 0..360) from an Rgb. A greyscale/near-black colour has no hue, so it falls
/// back to a pleasant aurora green (≈150°) rather than collapsing to red.
pub fn rgb_hue(c: Rgb) -> f32 {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
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
pub fn jitter_hue(base: Rgb, deg: f32) -> Rgb {
    if deg == 0.0 {
        return base;
    }
    let (rf, gf, bf) = (base.r as f32 / 255.0, base.g as f32 / 255.0, base.b as f32 / 255.0);
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
}
