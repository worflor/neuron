//! Spellweaving — the held-stroke → action engine, and Neuron's novel-input playground.
//!
//! You hold a trigger and *weave* a stroke; on release it resolves to an [`crate::action::Action`].
//! Weaves form a single complexity continuum, and the engine recognizes them at the right
//! resolution — they are NOT separate systems:
//!
//! - **Radial** ([`radial`]) is the DEGENERATE weave: a single committed flick where only the net
//!   *direction* matters, bucketed into N sectors. Zero-training, instant, muscle-memory. It is a
//!   strict **subset** of the gesture space — a one-segment glyph you only read directionally.
//! - **Glyphs** ([`glyph`] + the [`gesture`] Vault) are the RICH end: arbitrary drawn shapes
//!   recognized by eigenmotion (a damped complex-oscillator fit per stroke), size/speed-invariant,
//!   CW ≠ CCW.
//!
//! The [`cast`] resolver is the one entry point. A held stroke resolves to a radial sector (simple)
//! or a named glyph (rich), auto-branching on stroke geometry: a straight, committed flick reads as
//! radial; a confidently-recognized shape reads as a glyph. **Same trigger, same capture path**
//! ([`glyph::capture_held`]), one resolution — so radial isn't a sibling of spellweaving, it's the
//! simplest point *on* the spellweaving continuum.
//!
//! This module is the umbrella: the facets each live in their own file and are re-exported here as
//! the canonical `spellweaving::*` surface. Reach for `crate::spellweaving` when you mean "the whole
//! weave system"; the individual facet modules remain for fine-grained use.

#[doc(inline)]
pub use crate::cast::{self, CastConfig, Mode};
#[doc(inline)]
pub use crate::gesture::{self, Vault};
#[doc(inline)]
pub use crate::glyph;
#[doc(inline)]
pub use crate::radial::{self, RadialItem, RadialMenu};

/// Where a weave sits on the recognition continuum — the conceptual label for "how much shape the
/// engine had to read." Radial is the floor (direction only); a named glyph is the ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weave {
    /// A directional flick resolved to one of N sectors — the degenerate weave (radial subset).
    Radial,
    /// A drawn shape resolved to a named glyph in the [`Vault`] — the rich weave.
    Glyph,
}

impl Weave {
    pub fn label(self) -> &'static str {
        match self {
            Weave::Radial => "radial (simple)",
            Weave::Glyph => "glyph (rich)",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn umbrella_reexports_resolve() {
        // The whole weave system is reachable through the one umbrella, radial included.
        let _radial: Option<RadialItem> = None;
        let _vault_ty: Option<Vault> = None;
        assert_eq!(Weave::Radial.label(), "radial (simple)");
        assert_eq!(Weave::Glyph.label(), "glyph (rich)");
        // The resolver's default mode is Auto (branches radial vs glyph on geometry).
        assert_eq!(Mode::default(), Mode::Auto);
    }
}
