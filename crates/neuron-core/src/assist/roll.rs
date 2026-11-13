// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A palette rolled at random: the colour model fills the stops around an anchor, so a roll comes
//! out coherent rather than as noise. The same seed gives the same palette.

use super::learned::Learned;
use super::{distance, lab, rgb, Lch};
use crate::lighting::Rgb;
use crate::spectrum::Stop;

/// What a roll starts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The palette as it is: held stops stay, the rest are rolled to suit them. With none held,
    /// the selected stop is the anchor.
    Around,
    /// A new palette around a random anchor, with a new number of stops.
    Fresh,
    /// As fresh, anchored on a colour from the person's other palettes and in their style.
    Mine,
    /// Every colour by chance, the model aside.
    Wildcard,
}

/// How far a roll strays from the model's first choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wild {
    Steady,
    Lively,
    Wild,
}

impl Mode {
    pub const ALL: [Mode; 4] = [Mode::Around, Mode::Fresh, Mode::Mine, Mode::Wildcard];

    /// The name a menu shows and a preference stores.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Around => "around",
            Mode::Fresh => "fresh",
            Mode::Mine => "mine",
            Mode::Wildcard => "wildcard",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Mode> {
        Mode::ALL.into_iter().find(|mode| mode.name() == name)
    }
}

impl Wild {
    pub const ALL: [Wild; 3] = [Wild::Steady, Wild::Lively, Wild::Wild];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Wild::Steady => "steady",
            Wild::Lively => "lively",
            Wild::Wild => "wild",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Wild> {
        Wild::ALL.into_iter().find(|wild| wild.name() == name)
    }

    /// How likely each of the model's options is to be taken, best first.
    fn weights(self) -> &'static [f32] {
        match self {
            Wild::Steady => &[0.7, 0.3],
            Wild::Lively => &[0.45, 0.3, 0.15, 0.1],
            Wild::Wild => &[1.0, 1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// What to roll.
pub struct Roll<'a> {
    /// The palette as it is.
    pub stops: &'a [Stop],
    /// Stops a roll around this palette keeps. Shorter than `stops` means the rest are free.
    pub held: &'a [bool],
    pub selected: usize,
    /// The person's other palettes: the anchor and style of [`Mode::Mine`], the style of
    /// [`Mode::Around`].
    pub library: &'a [Vec<Rgb>],
    pub mode: Mode,
    pub wild: Wild,
    pub seed: u64,
}

/// SplitMix64: small, seedable, and good enough for picking colours.
struct Dice(u64);

impl Dice {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `0..1`.
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }

    /// An index drawn with the given weights.
    fn pick(&mut self, weights: &[f32]) -> usize {
        let total: f32 = weights.iter().sum();
        let mut at = self.unit() * total;
        for (i, w) in weights.iter().enumerate() {
            if at < *w {
                return i;
            }
            at -= w;
        }
        weights.len().saturating_sub(1)
    }
}

/// How many stops a fresh palette gets: mostly three or four, sometimes five or six, now and then
/// seven or eight.
const FRESH_COUNTS: [(usize, f32); 6] = [(3, 0.30), (4, 0.30), (5, 0.20), (6, 0.12), (7, 0.05), (8, 0.03)];

/// A colour that reads as a colour: not too dark, light or grey for the wildness.
fn anchor(dice: &mut Dice, wild: Wild) -> Rgb {
    let (light, vivid) = match wild {
        Wild::Steady => ((0.5, 0.8), (0.55, 0.9)),
        Wild::Lively => ((0.4, 0.85), (0.45, 1.0)),
        Wild::Wild => ((0.25, 0.92), (0.3, 1.0)),
    };
    let (l, h) = (dice.range(light.0, light.1), dice.unit());
    Lch { l, c: Lch::max_chroma(l, h) * dice.range(vivid.0, vivid.1), h }.rgb()
}

/// A rough palette around the known stops, for the model to refine: unknown stops blend between
/// the known ones on either side in OKLab, and past the last known one drift in hue and lightness
/// so the palette has somewhere to go.
fn sketch(stops: &mut [Stop], known: &[bool], dice: &mut Dice, wild: Wild) {
    let n = stops.len();
    let spread = match wild {
        Wild::Steady => 0.08,
        Wild::Lively => 0.16,
        Wild::Wild => 0.3,
    };
    let fixed: Vec<usize> = (0..n).filter(|&i| known[i]).collect();
    let Some(&first) = fixed.first() else { return };
    for i in (0..n).filter(|&i| !known[i]) {
        let before = fixed.iter().rev().find(|&&k| k < i).copied();
        let after = fixed.iter().find(|&&k| k > i).copied();
        stops[i].col = match (before, after) {
            (Some(a), Some(b)) => {
                let t = (stops[i].at - stops[a].at) / (stops[b].at - stops[a].at).max(1e-6);
                let (x, y) = (lab(stops[a].col), lab(stops[b].col));
                rgb(std::array::from_fn(|d| x[d] + (y[d] - x[d]) * t))
            }
            (from, _) => {
                let from = from.unwrap_or(first);
                let steps = (i as f32 - from as f32).abs();
                let base = Lch::of(stops[from].col);
                let h = (base.h + dice.range(-spread, spread) * steps).rem_euclid(1.0);
                let l = (base.l + dice.range(-0.12, 0.12) * steps).clamp(0.22, 0.92);
                Lch { l, c: Lch::max_chroma(l, h) * (base.c / Lch::max_chroma(base.l, base.h).max(1e-3)).clamp(0.3, 1.0), h }.rgb()
            }
        };
    }
}

/// Roll a palette. Positions are kept for [`Mode::Around`]; the other modes space a new number of
/// stops evenly. Held stops come back exactly in [`Mode::Around`]. Without a model the sketch is
/// the roll.
#[must_use]
pub fn roll(model: Option<&Learned>, r: &Roll) -> Vec<Stop> {
    let mut dice = Dice(r.seed ^ 0xA076_1D64_78BD_642F);
    let (mut stops, mut known) = match r.mode {
        Mode::Around if !r.stops.is_empty() => {
            let held: Vec<bool> = (0..r.stops.len()).map(|i| r.held.get(i).copied().unwrap_or(false)).collect();
            let known = if held.contains(&true) { held } else { (0..r.stops.len()).map(|i| i == r.selected.min(r.stops.len() - 1)).collect() };
            (r.stops.to_vec(), known)
        }
        _ => {
            let n = FRESH_COUNTS[dice.pick(&FRESH_COUNTS.map(|(_, w)| w))].0;
            let stops: Vec<Stop> = (0..n).map(|i| Stop::new(Rgb::new(0, 0, 0), i as f32 / (n - 1) as f32)).collect();
            (stops, vec![false; n])
        }
    };
    let n = stops.len();
    if r.mode == Mode::Wildcard {
        for stop in &mut stops {
            let (l, h) = (dice.range(0.25, 0.9), dice.unit());
            stop.col = Lch { l, c: Lch::max_chroma(l, h) * dice.range(0.2, 1.0), h }.rgb();
        }
        return stops;
    }
    if !known.contains(&true) {
        let at = (dice.unit() * n as f32) as usize % n.max(1);
        let mine: Vec<Rgb> = r.library.iter().flatten().copied().collect();
        stops[at].col = match r.mode {
            Mode::Mine if !mine.is_empty() => mine[(dice.next() % mine.len() as u64) as usize],
            _ => anchor(&mut dice, r.wild),
        };
        known[at] = true;
    }
    sketch(&mut stops, &known, &mut dice, r.wild);
    let Some(model) = model.filter(|_| n >= 2) else { return stops };
    let library: &[Vec<Rgb>] = if r.mode == Mode::Fresh { &[] } else { r.library };
    // fill outward from what is known: each stop is asked about with its nearest known
    // neighbours already settled; a wild roll fills in any order
    let mut order: Vec<usize> = (0..n).filter(|&i| !known[i]).collect();
    let reach = |stops: &[Stop], i: usize, known: &[bool]| (0..n).filter(|&k| known[k]).map(|k| (stops[k].at - stops[i].at).abs()).fold(f32::MAX, f32::min);
    while !order.is_empty() {
        let next = if r.wild == Wild::Wild {
            (dice.next() % order.len() as u64) as usize
        } else {
            let mut best = 0;
            for (slot, &i) in order.iter().enumerate() {
                if reach(&stops, i, &known) < reach(&stops, order[best], &known) {
                    best = slot;
                }
            }
            best
        };
        let i = order.swap_remove(next);
        let options = model.fill(&stops, i, library);
        if !options.is_empty() {
            let weights = r.wild.weights();
            stops[i].col = options[dice.pick(&weights[..weights.len().min(options.len())])];
        }
        known[i] = true;
    }
    stops
}

/// Whether two palettes look the same: the same number of stops, none of them visibly apart.
#[must_use]
pub fn same(a: &[Stop], b: &[Stop]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| (x.at - y.at).abs() < 1e-4 && distance(lab(x.col), lab(y.col)) < 0.02)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn palette() -> Vec<Stop> {
        [(30, 40, 90), (60, 110, 180), (120, 170, 220), (200, 225, 245)].into_iter().enumerate()
            .map(|(i, (r, g, b))| Stop::new(Rgb::new(r, g, b), i as f32 / 3.0)).collect()
    }

    fn ask<'a>(stops: &'a [Stop], held: &'a [bool], mode: Mode, wild: Wild, seed: u64) -> Roll<'a> {
        Roll { stops, held, selected: 1, library: &[], mode, wild, seed }
    }

    #[test]
    fn a_seed_rolls_the_same_palette_and_another_seed_a_different_one() {
        let stops = palette();
        let model = Learned::parse(include_bytes!("../../data/colour-model.golden.lumacore")).unwrap();
        for mode in Mode::ALL {
            let once = roll(Some(&model), &ask(&stops, &[], mode, Wild::Lively, 7));
            assert_eq!(once, roll(Some(&model), &ask(&stops, &[], mode, Wild::Lively, 7)), "{mode:?}");
            assert!((1..20).any(|seed| !same(&once, &roll(Some(&model), &ask(&stops, &[], mode, Wild::Lively, 7 + seed)))), "{mode:?} never changes");
        }
    }

    #[test]
    fn around_keeps_held_stops_and_positions() {
        let stops = palette();
        let held = [true, false, false, true];
        let model = Learned::parse(include_bytes!("../../data/colour-model.golden.lumacore")).unwrap();
        for seed in 0..12 {
            for wild in Wild::ALL {
                let out = roll(Some(&model), &ask(&stops, &held, Mode::Around, wild, seed));
                assert_eq!(out.len(), stops.len());
                assert!(out.iter().zip(&stops).all(|(a, b)| a.at == b.at));
                assert!(out[0].col == stops[0].col && out[3].col == stops[3].col, "held stops move: {out:?}");
            }
        }
    }

    #[test]
    fn fresh_palettes_are_mostly_small_and_never_past_eight() {
        let stops = palette();
        let counts: Vec<usize> = (0..400).map(|seed| roll(None, &ask(&stops, &[], Mode::Fresh, Wild::Steady, seed)).len()).collect();
        assert!(counts.iter().all(|n| (3..=8).contains(n)));
        let small = counts.iter().filter(|&&n| n <= 4).count();
        assert!(small > 200 && small < 280, "three or four stops in {small} of 400");
        assert!(counts.iter().any(|&n| n >= 7), "now and then a long one");
        let fresh = roll(None, &ask(&stops, &[], Mode::Fresh, Wild::Steady, 3));
        assert!(fresh.windows(2).all(|pair| pair[0].at < pair[1].at) && fresh[0].at == 0.0 && fresh.last().unwrap().at == 1.0);
    }
}
