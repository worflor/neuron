// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Stack sense — reading a layer stack the way the eye does, and saying what would help.
//!
//! A layer's blend, opacity and place in the stack don't change what its own pattern draws; they
//! only change how that light lands on what's beneath. So each layer is recorded once, alone, over a
//! short window (a [`Take`]), and every arrangement after that is pure re-compositing of recorded
//! frames: the same per-cell [`blend_px_opacity`] the live compositor runs, at a few hundred cells and
//! a dozen frames. That makes it cheap to ask "what if" a few hundred times.
//!
//! Two numbers come out of an arrangement for every layer, both in OKLab so they track what a person
//! sees:
//!
//! - **presence**: how much light the layer has on its own, over black.
//! - **visibility**: how much of that presence still changes the final frame once the stack is on
//!   top of and under it (the frame with the layer against the frame without it, over its presence).
//!
//! Every conflict is a layer whose visibility collapses, or colour going to white where two bright
//! layers add up. [`Stack::sense`] names the trouble for one layer and searches nearby arrangements
//! for the few that fix it; [`Stack::strip`] scores every opacity for the opacity slider; and
//! [`Stack::place`] picks a newly added layer's blend and opacity.

use crate::effects::{blend_px_opacity, duck_px, duck_weights, Blend};
use crate::lighting::Rgb;
use crate::spectrum::rgb_to_oklab;
use std::sync::Arc;

/// Visibility at which a layer reads clearly; more than this earns nothing extra in the score.
const CLEAR: f32 = 0.6;
/// Visibility under which a layer is in trouble.
pub const LOST: f32 = 0.3;
/// How many fixes a [`Sense`] offers at most.
pub const OFFERS: usize = 3;
/// How much a counterfactual must lift a visibility before it counts as the cause.
const CAUSE: f32 = 0.2;
/// Less presence than this (in OKLab, averaged over the board) is a layer with nothing lit to judge.
const DARK: f32 = 0.004;
/// A cell this far from black (OKLab) is lit.
const LIT: f32 = 0.05;
/// A Normal layer lit in fewer than this share of its cells has gaps that black out what's under.
const SPARSE: f32 = 0.7;
/// Share of washed cells at which a layer counts as washing out.
const WASHED: f32 = 0.2;

/// One layer's own light over a short window: `frames[f][cell]`, row-major, black outside its region.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Take {
    pub frames: Vec<Vec<Rgb>>,
}

/// The presentation of a layer: everything neuron may suggest changing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Look {
    pub blend: Blend,
    pub opacity: f32,
    pub enabled: bool,
    /// Dims what's beneath its lit cells, as the live compositor does.
    pub duck: bool,
}

/// One layer of a stack to read.
#[derive(Clone, Debug)]
pub struct Part {
    pub take: Arc<Take>,
    /// Row-major cells the layer paints; empty is the whole board.
    pub region: Vec<u32>,
    pub look: Look,
    /// False when the recording can't stand for the real thing (a layer lit by audio or the screen
    /// recorded with nothing playing). Such a layer still composites, but nothing is said about it.
    pub known: bool,
}

/// A way to lay the stack out: bottom-first order of part indices, and each part's look.
#[derive(Clone, Debug, PartialEq)]
pub struct Arrangement {
    pub order: Vec<usize>,
    pub looks: Vec<Look>,
}

/// What an arrangement does to every layer.
#[derive(Clone, Debug, PartialEq)]
pub struct Reading {
    /// Per part: how much of its own presence still shows, 0..1. `None` for a layer with nothing to
    /// judge (hidden, unknown, or unlit).
    pub visible: Vec<Option<f32>>,
    /// Per part: the share of its lit cells that the stack pushes to white.
    pub washed: Vec<f32>,
    /// One number for the whole arrangement: higher is a stack where everything reads.
    pub score: f32,
}

/// What's wrong with a layer, in terms a person would use. Indices are parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trouble {
    /// A Normal layer above, mostly dark, blacks this one out through its gaps.
    ErasedBy(usize),
    /// A layer above is drawn over this one.
    CoveredBy(usize),
    /// This layer, mostly dark and Normal, blacks out one beneath it.
    Erases(usize),
    /// This layer is drawn over one beneath it.
    Covers(usize),
    /// This layer and the one beneath add up to white.
    WashesOut(usize),
    /// This layer's light is lost in the one beneath it.
    LostIn(usize),
    /// Multiply with nothing lit beneath it to darken.
    NothingUnder,
}

/// A suggested change, applied to the current arrangement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Fix {
    Blend { part: usize, blend: Blend },
    Opacity { part: usize, opacity: f32 },
    /// Move `part` to sit directly above `other`.
    Above { part: usize, other: usize },
    /// Move `part` to sit directly below `other`.
    Below { part: usize, other: usize },
    /// Have `part` duck what's beneath it.
    Duck { part: usize },
}

/// A fix with what it buys.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    pub fix: Fix,
    /// The reading the stack would have with the fix applied.
    pub after: Reading,
}

/// What [`Stack::sense`] has to say about one layer.
#[derive(Clone, Debug, PartialEq)]
pub struct Sense {
    pub now: Reading,
    pub trouble: Option<Trouble>,
    pub offers: Vec<Offer>,
}

/// One step of the opacity strip: how cold (unseen), warm (reads well) and hot (buries what's beneath
/// or washes out) that opacity is, each 0..1.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Zone {
    pub cold: f32,
    pub warm: f32,
    pub hot: f32,
}

/// A recorded stack, ready to be re-composited.
pub struct Stack {
    parts: Vec<Part>,
    cells: usize,
    frames: usize,
    /// Per part: whether it paints each cell.
    masks: Vec<Vec<bool>>,
    /// Per part, per frame, per cell: how hard it pushes down what's beneath when it ducks.
    ducks: Vec<Vec<Vec<f32>>>,
    /// Per part, per frame, per cell: its own light in OKLab.
    labs: Vec<Vec<Vec<[f32; 3]>>>,
    /// Per part: presence (mean OKLab distance from black over the board and the window).
    presence: Vec<f32>,
    /// Per part: share of its cells and frames that are lit.
    density: Vec<f32>,
}

fn lab(c: Rgb) -> [f32; 3] {
    let (l, a, b) = rgb_to_oklab(c);
    [l, a, b]
}

fn distance(x: [f32; 3], y: [f32; 3]) -> f32 {
    ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
}

const BLACK: [f32; 3] = [0.0, 0.0, 0.0];
const WHITE: [f32; 3] = [1.0, 0.0, 0.0];

impl Stack {
    /// Read `parts` (bottom first) on a `rows`×`cols` board.
    #[must_use]
    pub fn new(parts: Vec<Part>, (rows, cols): (usize, usize)) -> Stack {
        let cells = rows * cols;
        let frames = parts.iter().map(|p| p.take.frames.len()).min().unwrap_or(0);
        let masks: Vec<Vec<bool>> = parts
            .iter()
            .map(|p| {
                if p.region.is_empty() {
                    vec![true; cells]
                } else {
                    let mut mask = vec![false; cells];
                    for &cell in &p.region {
                        if let Some(m) = mask.get_mut(cell as usize) {
                            *m = true;
                        }
                    }
                    mask
                }
            })
            .collect();
        let labs: Vec<Vec<Vec<[f32; 3]>>> = parts
            .iter()
            .map(|p| {
                p.take.frames[..frames]
                    .iter()
                    .map(|frame| (0..cells).map(|i| frame.get(i).copied().map_or(BLACK, lab)).collect())
                    .collect()
            })
            .collect();
        let samples = (cells * frames).max(1) as f32;
        let presence = labs
            .iter()
            .zip(&masks)
            .map(|(take, mask)| {
                take.iter().flat_map(|f| f.iter().zip(mask).filter(|(_, m)| **m).map(|(c, _)| distance(*c, BLACK))).sum::<f32>()
                    / samples
            })
            .collect();
        let density = labs
            .iter()
            .zip(&masks)
            .map(|(take, mask)| {
                let covered = mask.iter().filter(|m| **m).count() * frames;
                let lit = take
                    .iter()
                    .flat_map(|f| f.iter().zip(mask).filter(|(c, m)| **m && distance(**c, BLACK) > LIT))
                    .count();
                if covered == 0 { 0.0 } else { lit as f32 / covered as f32 }
            })
            .collect();
        let ducks = parts
            .iter()
            .zip(&masks)
            .map(|(p, mask)| {
                p.take.frames[..frames]
                    .iter()
                    .map(|frame| {
                        let mut weights = Vec::new();
                        duck_weights(frame, |i| mask.get(i).copied().unwrap_or(false), rows, cols, &mut weights);
                        weights
                    })
                    .collect()
            })
            .collect();
        Stack { parts, cells, frames, masks, ducks, labs, presence, density }
    }

    /// The arrangement the parts are in now.
    #[must_use]
    pub fn current(&self) -> Arrangement {
        Arrangement { order: (0..self.parts.len()).collect(), looks: self.parts.iter().map(|p| p.look).collect() }
    }

    /// The composited frame `f` of `arr`, leaving out `skip`.
    fn compose(&self, arr: &Arrangement, skip: Option<usize>, f: usize, out: &mut Vec<Rgb>) {
        out.clear();
        out.resize(self.cells, Rgb::BLACK);
        for &p in &arr.order {
            let look = arr.looks[p];
            if Some(p) == skip || !look.enabled {
                continue;
            }
            let take = &self.parts[p].take.frames[f];
            for (i, cell) in out.iter_mut().enumerate() {
                if self.masks[p][i] {
                    let under = if look.duck { duck_px(*cell, self.ducks[p][f][i], look.opacity) } else { *cell };
                    *cell = blend_px_opacity(under, take.get(i).copied().unwrap_or(Rgb::BLACK), look.blend, look.opacity);
                }
            }
        }
    }

    /// The composited frame `f` of `arr`, for drawing.
    #[must_use]
    pub fn frame(&self, arr: &Arrangement, f: usize) -> Vec<Rgb> {
        let mut out = Vec::new();
        if f < self.frames {
            self.compose(arr, None, f, &mut out);
        }
        out
    }

    /// The frame in the window where `part` has the most light: the one to show it by.
    #[must_use]
    pub fn showcase(&self, part: usize) -> usize {
        (0..self.frames)
            .max_by(|&a, &b| {
                let sum = |f: usize| self.labs[part][f].iter().map(|c| c[0]).sum::<f32>();
                sum(a).total_cmp(&sum(b))
            })
            .unwrap_or(0)
    }

    /// Whether a part is one the reading can speak for under `arr`.
    /// A layer switched off, or at no opacity, is hidden by choice: nothing is said about it.
    fn judged(&self, arr: &Arrangement, p: usize) -> bool {
        arr.looks[p].enabled && arr.looks[p].opacity > 0.0 && self.parts[p].known && self.presence[p] > DARK
    }

    /// What `arr` does to every layer.
    #[must_use]
    pub fn read(&self, arr: &Arrangement) -> Reading {
        let n = self.parts.len();
        let mut full = vec![Vec::new(); self.frames];
        let mut scratch = Vec::new();
        for (f, lab_frame) in full.iter_mut().enumerate() {
            self.compose(arr, None, f, &mut scratch);
            *lab_frame = scratch.iter().map(|c| lab(*c)).collect::<Vec<_>>();
        }
        let mut visible = vec![None; n];
        let mut washed = vec![0.0; n];
        for p in 0..n {
            if !self.judged(arr, p) {
                continue;
            }
            // Only the cells the layer lights count toward its own visibility. What its dark cells do
            // to the layers beneath is their loss, and shows up in their reading, not as credit here.
            let adds = matches!(arr.looks[p].blend, Blend::Add | Blend::Screen);
            let (mut shown, mut has) = (0.0, 0.0);
            let (mut lit, mut white) = (0usize, 0usize);
            for (f, full_frame) in full.iter().enumerate() {
                self.compose(arr, Some(p), f, &mut scratch);
                for i in (0..self.cells).filter(|&i| self.masks[p][i]) {
                    let own_lab = self.labs[p][f][i];
                    let own = distance(own_lab, BLACK);
                    if own <= LIT {
                        continue;
                    }
                    let without = lab(scratch[i]);
                    shown += distance(full_frame[i], without);
                    has += own;
                    // white the layer didn't bring itself, and that wasn't there without it: two
                    // bright layers adding up
                    if adds {
                        lit += 1;
                        if distance(full_frame[i], WHITE) < 0.12 && distance(own_lab, WHITE) > 0.3 && distance(without, WHITE) >= 0.12 {
                            white += 1;
                        }
                    }
                }
            }
            // the eye's response to a change in light is compressive: a share of 0.16 of a layer's
            // light reads as present, not as a sixth of it
            visible[p] = Some(if has > 0.0 { (shown / has).clamp(0.0, 1.0).sqrt() } else { 0.0 });
            washed[p] = if lit == 0 { 0.0 } else { white as f32 / lit as f32 };
        }
        // reading clearly is most of it; past that, more still counts a little, so an arrangement
        // that shows everything fully beats one that only just gets everything across
        let seen: Vec<f32> = visible.iter().flatten().map(|v| 0.8 * (v / CLEAR).min(1.0) + 0.2 * v).collect();
        let score = if seen.is_empty() {
            0.0
        } else {
            seen.iter().sum::<f32>() / seen.len() as f32 + 0.5 * seen.iter().copied().fold(1.0, f32::min)
                - 0.8 * washed.iter().copied().fold(0.0, f32::max)
        };
        Reading { visible, washed, score }
    }

    /// `arr` with `fix` applied.
    #[must_use]
    pub fn apply(&self, arr: &Arrangement, fix: Fix) -> Arrangement {
        let mut next = arr.clone();
        match fix {
            Fix::Blend { part, blend } => next.looks[part].blend = blend,
            Fix::Opacity { part, opacity } => next.looks[part].opacity = opacity,
            Fix::Duck { part } => next.looks[part].duck = true,
            Fix::Above { part, other } | Fix::Below { part, other } => {
                next.order.retain(|&p| p != part);
                let at = next.order.iter().position(|&p| p == other).unwrap_or(0);
                let at = if matches!(fix, Fix::Above { .. }) { at + 1 } else { at };
                next.order.insert(at.min(next.order.len()), part);
            }
        }
        next
    }

    /// Parts below and above `part` in `arr`, nearest first.
    fn neighbours(arr: &Arrangement, part: usize) -> (Vec<usize>, Vec<usize>) {
        let at = arr.order.iter().position(|&p| p == part).unwrap_or(0);
        let below = arr.order[..at].iter().rev().copied().collect();
        let above = arr.order[at + 1..].to_vec();
        (below, above)
    }

    /// `arr` with `part` switched off.
    fn without(arr: &Arrangement, part: usize) -> Arrangement {
        let mut next = arr.clone();
        next.looks[part].enabled = false;
        next
    }

    /// Name the trouble `part` is in under the current arrangement, if any.
    fn trouble(&self, now: &Reading, part: usize) -> Option<Trouble> {
        let arr = self.current();
        // a layer with no light of its own to judge can still black out what's beneath it
        if !arr.looks[part].enabled || !self.parts[part].known {
            return None;
        }
        let lift = |victim: usize, cause: usize| {
            let freed = self.read(&Self::without(&arr, cause));
            freed.visible[victim].unwrap_or(0.0) - now.visible[victim].unwrap_or(0.0)
        };
        let (below, above) = Self::neighbours(&arr, part);
        let mine = now.visible[part].unwrap_or(1.0);
        let gaps = |p: usize| arr.looks[p].blend == Blend::Normal && self.density[p] < SPARSE;
        if mine < LOST {
            let culprit = above
                .iter()
                .filter(|&&a| arr.looks[a].enabled)
                .map(|&a| (a, lift(part, a)))
                .max_by(|x, y| x.1.total_cmp(&y.1))
                .filter(|(_, gain)| *gain >= CAUSE);
            if let Some((a, _)) = culprit {
                return Some(if gaps(a) { Trouble::ErasedBy(a) } else { Trouble::CoveredBy(a) });
            }
            // buried twice over: taking away either layer alone frees nothing, so the cause is the
            // topmost layer that buries it on its own, once the whole cover is shown to be the cause
            let bare = above.iter().fold(arr.clone(), |at, &a| Self::without(&at, a));
            let freed = self.read(&bare).visible[part].unwrap_or(0.0) - mine >= CAUSE;
            let alone = |a: usize| {
                let only = above.iter().filter(|&&o| o != a).fold(arr.clone(), |at, &o| Self::without(&at, o));
                self.read(&only).visible[part].unwrap_or(1.0) < LOST
            };
            if let Some(&a) = above.iter().rev().filter(|&&a| arr.looks[a].enabled).find(|&&a| freed && alone(a)) {
                return Some(if gaps(a) { Trouble::ErasedBy(a) } else { Trouble::CoveredBy(a) });
            }
        }
        for &b in &below {
            if now.visible[b].is_some_and(|v| v < LOST) && lift(b, part) >= CAUSE {
                return Some(if gaps(part) { Trouble::Erases(b) } else { Trouble::Covers(b) });
            }
        }
        if now.washed[part] > WASHED {
            // the layer beneath whose light, taken away, takes the most of the white with it
            let cause = below
                .iter()
                .filter(|&&b| arr.looks[b].enabled)
                .map(|&b| (b, now.washed[part] - self.read(&Self::without(&arr, b)).washed[part]))
                .max_by(|x, y| x.1.total_cmp(&y.1));
            if let Some((b, _)) = cause {
                return Some(Trouble::WashesOut(b));
            }
        }
        if mine < LOST {
            if arr.looks[part].blend == Blend::Multiply {
                return Some(Trouble::NothingUnder);
            }
            let culprit = below
                .iter()
                .filter(|&&b| arr.looks[b].enabled)
                .map(|&b| (b, lift(part, b)))
                .max_by(|x, y| x.1.total_cmp(&y.1))
                .filter(|(_, gain)| *gain >= CAUSE);
            if let Some((b, _)) = culprit {
                return Some(Trouble::LostIn(b));
            }
        }
        None
    }

    /// Whether `after` is better than `now` at the thing `trouble` is about.
    fn eases(trouble: Trouble, part: usize, now: &Reading, after: &Reading) -> bool {
        let gain = |p: usize| after.visible[p].unwrap_or(0.0) - now.visible[p].unwrap_or(0.0);
        match trouble {
            Trouble::ErasedBy(_) | Trouble::CoveredBy(_) | Trouble::LostIn(_) | Trouble::NothingUnder => gain(part) >= 0.15,
            Trouble::Erases(b) | Trouble::Covers(b) => gain(b) >= 0.15,
            Trouble::WashesOut(_) => after.washed[part] <= now.washed[part] - 0.1,
        }
    }

    /// What a fix costs in surprise: changing someone else's layer or the order costs more than
    /// adjusting the layer being edited.
    fn cost(fix: Fix, part: usize) -> f32 {
        match fix {
            Fix::Opacity { part: p, .. } | Fix::Blend { part: p, .. } if p == part => 0.02,
            Fix::Duck { part: p } if p == part => 0.03,
            Fix::Opacity { .. } | Fix::Blend { .. } | Fix::Duck { .. } => 0.05,
            Fix::Above { .. } | Fix::Below { .. } => 0.06,
        }
    }

    /// Read the stack for `part` (the layer being edited): its trouble, and up to [`OFFERS`] fixes.
    #[must_use]
    pub fn sense(&self, part: usize) -> Sense {
        self.sense_from(part, self.read(&self.current()), |_| true)
    }

    /// [`Stack::sense`] with the current arrangement's reading already in hand, so sensing every
    /// layer of a stack reads it once, offering only the fixes `allowed` lets through (ranked among
    /// themselves, so a refused fix leaves room for the next best).
    #[must_use]
    pub fn sense_from(&self, part: usize, now: Reading, allowed: impl Fn(Fix) -> bool) -> Sense {
        let arr = self.current();
        let trouble = if part < self.parts.len() { self.trouble(&now, part) } else { None };
        let Some(trouble) = trouble else {
            return Sense { now, trouble: None, offers: Vec::new() };
        };
        let other = match trouble {
            Trouble::ErasedBy(o) | Trouble::CoveredBy(o) | Trouble::Erases(o) | Trouble::Covers(o)
            | Trouble::WashesOut(o) | Trouble::LostIn(o) => Some(o),
            Trouble::NothingUnder => None,
        };
        let mut fixes = Vec::new();
        for p in std::iter::once(part).chain(other) {
            let look = arr.looks[p];
            for blend in [Blend::Normal, Blend::Screen, Blend::Add, Blend::Cut] {
                if blend != look.blend {
                    fixes.push(Fix::Blend { part: p, blend });
                }
            }
            for opacity in [0.35, 0.5, 0.7, 1.0] {
                if (opacity - look.opacity).abs() > 0.1 {
                    fixes.push(Fix::Opacity { part: p, opacity });
                }
            }
        }
        // a layer lost in or washing out what's beneath can push that down where it lights instead
        if matches!(trouble, Trouble::LostIn(_) | Trouble::WashesOut(_)) && !arr.looks[part].duck {
            fixes.push(Fix::Duck { part });
        }
        if let Some(o) = other {
            let (_, above) = Self::neighbours(&arr, part);
            fixes.push(if above.contains(&o) { Fix::Above { part, other: o } } else { Fix::Below { part, other: o } });
        }
        let mut offers: Vec<(f32, Offer)> = fixes
            .into_iter()
            .filter(|&fix| allowed(fix))
            .filter_map(|fix| {
                let fixed = self.apply(&arr, fix);
                // a fix only counts if it changes what's on the board: dimming a layer nobody can see
                // reads as a gain (the layers above differ more from what's under them) but shows nothing
                if (0..self.frames).all(|f| self.frame(&fixed, f) == self.frame(&arr, f)) {
                    return None;
                }
                let after = self.read(&fixed);
                let gain = after.score - now.score - Self::cost(fix, part);
                (gain >= 0.05 && Self::eases(trouble, part, &now, &after)).then_some((gain, Offer { fix, after }))
            })
            .collect();
        offers.sort_by(|a, b| b.0.total_cmp(&a.0));
        // one of each kind per layer: the best blend for a layer, its best opacity, the best move
        let mut kept: Vec<Offer> = Vec::new();
        for (_, offer) in offers {
            let kind = |fix: Fix| match fix {
                Fix::Blend { part, .. } => (0, part),
                Fix::Opacity { part, .. } => (1, part),
                Fix::Above { part, .. } | Fix::Below { part, .. } => (2, part),
                Fix::Duck { part } => (3, part),
            };
            if kept.len() < OFFERS && !kept.iter().any(|k| kind(k.fix) == kind(offer.fix)) {
                kept.push(offer);
            }
        }
        Sense { now, trouble: Some(trouble), offers: kept }
    }

    /// Per cell, how much of `part`'s own light reaches the board under `arr`, 0..1 on the same
    /// compressive scale as [`Reading::visible`], averaged over the frames where the layer lights that
    /// cell. `None` for a cell it never lights.
    #[must_use]
    pub fn reach(&self, arr: &Arrangement, part: usize) -> Vec<Option<f32>> {
        let mut sum = vec![0.0_f32; self.cells];
        let mut lit = vec![0_u32; self.cells];
        if part >= self.parts.len() || !arr.looks[part].enabled {
            return vec![None; self.cells];
        }
        let (mut full, mut without) = (Vec::new(), Vec::new());
        for f in 0..self.frames {
            self.compose(arr, None, f, &mut full);
            self.compose(arr, Some(part), f, &mut without);
            for i in (0..self.cells).filter(|&i| self.masks[part][i]) {
                let own = distance(self.labs[part][f][i], BLACK);
                if own > LIT {
                    sum[i] += (distance(lab(full[i]), lab(without[i])) / own).clamp(0.0, 1.0).sqrt();
                    lit[i] += 1;
                }
            }
        }
        sum.iter().zip(&lit).map(|(s, &n)| (n > 0).then(|| s / n as f32)).collect()
    }

    /// The cells where `culprit` keeps `victim`'s light from the board: taking the culprit away gives
    /// the victim back at least a tenth of its light there, in at least a third of the frames the
    /// victim lights it.
    #[must_use]
    pub fn harm(&self, arr: &Arrangement, culprit: usize, victim: usize) -> Vec<bool> {
        let n = self.parts.len();
        if culprit >= n || victim >= n || culprit == victim {
            return vec![false; self.cells];
        }
        let freed = Self::without(arr, culprit);
        let (mut hits, mut lit) = (vec![0_u32; self.cells], vec![0_u32; self.cells]);
        let (mut a, mut b, mut c, mut d) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for f in 0..self.frames {
            self.compose(arr, None, f, &mut a);
            self.compose(arr, Some(victim), f, &mut b);
            self.compose(&freed, None, f, &mut c);
            self.compose(&freed, Some(victim), f, &mut d);
            for i in (0..self.cells).filter(|&i| self.masks[victim][i]) {
                if distance(self.labs[victim][f][i], BLACK) <= LIT {
                    continue;
                }
                lit[i] += 1;
                let now = distance(lab(a[i]), lab(b[i]));
                let then = distance(lab(c[i]), lab(d[i]));
                if then - now > 0.1 * distance(self.labs[victim][f][i], BLACK) {
                    hits[i] += 1;
                }
            }
        }
        hits.iter().zip(&lit).map(|(&h, &l)| l > 0 && h * 3 >= l).collect()
    }

    /// Per cell, the part whose light most changes what the board shows there, over the window: the
    /// layer a person pointing at that key is looking at. `None` where nothing is lit.
    #[must_use]
    pub fn owners(&self, arr: &Arrangement) -> Vec<Option<usize>> {
        let mut best = vec![(0.0_f32, None); self.cells];
        let (mut full, mut without) = (Vec::new(), Vec::new());
        for p in (0..self.parts.len()).filter(|&p| arr.looks[p].enabled) {
            let mut sum = vec![0.0_f32; self.cells];
            for f in 0..self.frames {
                self.compose(arr, None, f, &mut full);
                self.compose(arr, Some(p), f, &mut without);
                for i in (0..self.cells).filter(|&i| self.masks[p][i]) {
                    sum[i] += distance(lab(full[i]), lab(without[i]));
                }
            }
            for (b, s) in best.iter_mut().zip(sum) {
                if s > b.0 && s > LIT * self.frames as f32 * 0.5 {
                    *b = (s, Some(p));
                }
            }
        }
        best.into_iter().map(|(_, p)| p).collect()
    }

    /// `part`'s own light in the frame that shows it best.
    #[must_use]
    pub fn glimpse(&self, part: usize) -> Vec<Rgb> {
        let f = self.showcase(part);
        let mut out = vec![Rgb::BLACK; self.cells];
        if let Some(frame) = self.parts.get(part).and_then(|p| p.take.frames.get(f)) {
            for (i, cell) in out.iter_mut().enumerate() {
                if self.masks[part][i] {
                    *cell = frame.get(i).copied().unwrap_or(Rgb::BLACK);
                }
            }
        }
        out
    }

    /// Score every opacity of `part` from 0 to 1 in `steps` steps, for the opacity slider.
    #[must_use]
    pub fn strip(&self, part: usize, steps: usize) -> Vec<Zone> {
        let arr = self.current();
        if part >= self.parts.len() || !self.parts[part].known || self.presence[part] <= DARK {
            return Vec::new();
        }
        let (below, _) = Self::neighbours(&arr, part);
        let mut base = arr.clone();
        base.looks[part].enabled = true;
        let alone = self.read(&Self::without(&base, part));
        (0..=steps)
            .map(|k| {
                let mut at = base.clone();
                at.looks[part].opacity = k as f32 / steps.max(1) as f32;
                let reading = self.read(&at);
                let mine = reading.visible[part].unwrap_or(0.0);
                // how much of what's beneath still shows, against how much showed without this layer
                let kept = below
                    .iter()
                    .filter_map(|&b| Some(reading.visible[b]? / alone.visible[b]?.max(0.05)))
                    .fold(1.0_f32, f32::min)
                    .min(1.0);
                let cold = (1.0 - mine / 0.25).clamp(0.0, 1.0);
                let hot = ((0.6 - kept) / 0.6).clamp(0.0, 1.0).max(((reading.washed[part] - 0.1) / 0.3).clamp(0.0, 1.0));
                let warm = ((mine / CLEAR).min(1.0) * (1.0 - hot) * (1.0 - cold)).clamp(0.0, 1.0);
                Zone { cold, warm, hot }
            })
            .collect()
    }

    /// The look a newly added `part` should start with: the blend, opacity and ducking that let it and
    /// everything already there read best. `preferred` (the effect's own default) wins a near tie, and
    /// so does not ducking.
    #[must_use]
    pub fn place(&self, part: usize, preferred: Blend) -> Look {
        let arr = self.current();
        let Some(&start) = arr.looks.get(part) else { return Look { blend: preferred, opacity: 1.0, enabled: true, duck: false } };
        // a layer whose recording can't stand for it keeps its own blend; a dark one is still placed
        // so its dark cells don't black out what's beneath
        if self.parts.len() < 2 || !self.parts[part].known {
            return Look { blend: preferred, ..start };
        }
        let mut best = (f32::MIN, start);
        // the look's own blend is always in the running, whatever it is
        let mut blends = vec![Blend::Normal, Blend::Screen, Blend::Add, Blend::Cut];
        if !blends.contains(&preferred) {
            blends.push(preferred);
        }
        for blend in blends {
            for opacity in [1.0, 0.8, 0.6] {
                for duck in [false, true] {
                    let mut at = arr.clone();
                    at.looks[part] = Look { blend, opacity, enabled: true, duck };
                    let bias = if blend == preferred { 0.03 } else { 0.0 } + 0.02 * opacity - if duck { 0.05 } else { 0.0 };
                    let score = self.read(&at).score + bias;
                    if score > best.0 {
                        best = (score, at.looks[part]);
                    }
                }
            }
        }
        best.1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELLS: usize = 16;
    const BOARD: (usize, usize) = (4, 4);
    const FRAMES: usize = 4;

    fn take(at: impl Fn(usize, usize) -> Rgb) -> Arc<Take> {
        Arc::new(Take { frames: (0..FRAMES).map(|f| (0..CELLS).map(|i| at(f, i)).collect()).collect() })
    }

    fn part(take: Arc<Take>, blend: Blend) -> Part {
        Part { take, region: Vec::new(), look: Look { blend, opacity: 1.0, enabled: true, duck: false }, known: true }
    }

    /// A full board of one colour.
    fn wash(c: Rgb) -> Arc<Take> {
        take(move |_, _| c)
    }

    /// One lit cell in four, moving each frame: a sparse effect.
    fn sparks(c: Rgb) -> Arc<Take> {
        take(move |f, i| if (i + f) % 4 == 0 { c } else { Rgb::BLACK })
    }

    const TEAL: Rgb = Rgb::new(0x20, 0xC9, 0xB0);
    const AMBER: Rgb = Rgb::new(0xFF, 0xA0, 0x10);

    #[test]
    fn a_layer_alone_shows_in_full() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal)], BOARD);
        let reading = stack.read(&stack.current());
        assert!((reading.visible[0].unwrap() - 1.0).abs() < 1e-3);
    }

    #[test]
    fn sparse_normal_on_top_erases_and_screen_is_offered() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(sparks(AMBER), Blend::Normal)], BOARD);
        let sense = stack.sense(0);
        assert_eq!(sense.trouble, Some(Trouble::ErasedBy(1)));
        assert!(sense.now.visible[0].unwrap() < LOST);
        let first = sense.offers.first().expect("a fix");
        assert!(
            matches!(first.fix, Fix::Blend { part: 1, blend: Blend::Screen | Blend::Cut | Blend::Add }),
            "the fix lets the dark cells through: {:?}",
            first.fix
        );
        assert!(first.after.visible[0].unwrap() > sense.now.visible[0].unwrap() + 0.15);
    }

    #[test]
    fn the_layer_doing_the_erasing_hears_about_it_too() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(sparks(AMBER), Blend::Normal)], BOARD);
        assert_eq!(stack.sense(1).trouble, Some(Trouble::Erases(0)));
    }

    #[test]
    fn a_dense_layer_on_top_covers() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(wash(AMBER), Blend::Normal)], BOARD);
        assert_eq!(stack.sense(0).trouble, Some(Trouble::CoveredBy(1)));
    }

    #[test]
    fn layers_that_get_along_are_left_alone() {
        let deep = Rgb::new(0x10, 0x30, 0xC0);
        let stack = Stack::new(vec![part(wash(deep), Blend::Normal), part(sparks(AMBER), Blend::Screen)], BOARD);
        let sense = stack.sense(0);
        assert_eq!(sense.trouble, None, "{:?}", sense.now);
        assert!(sense.offers.is_empty());
        assert_eq!(stack.sense(1).trouble, None);
    }

    #[test]
    fn two_bright_layers_added_wash_out() {
        let orange = Rgb::new(0xE0, 0x90, 0x20);
        let stack = Stack::new(vec![part(wash(Rgb::new(0x40, 0x80, 0xE0)), Blend::Normal), part(wash(orange), Blend::Add)], BOARD);
        let sense = stack.sense(1);
        assert!(sense.now.washed[1] > WASHED, "{:?}", sense.now.washed);
        assert_eq!(sense.trouble, Some(Trouble::WashesOut(0)));
        assert!(!sense.offers.is_empty());
    }

    #[test]
    fn multiply_over_nothing_is_dark() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Multiply)], BOARD);
        assert_eq!(stack.sense(0).trouble, Some(Trouble::NothingUnder));
    }

    #[test]
    fn a_region_outside_the_cover_is_untouched() {
        let mut top = part(wash(AMBER), Blend::Normal);
        top.region = (0..8).collect();
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), top], BOARD);
        let v = stack.read(&stack.current()).visible[0].unwrap();
        // half its light still lands; visibility is on the eye's compressive scale
        assert!((v * v - 0.5).abs() < 0.05, "half the board still shows the bottom layer: {v}");
    }

    #[test]
    fn unknown_and_hidden_layers_are_not_judged() {
        let mut quiet = part(wash(TEAL), Blend::Normal);
        quiet.known = false;
        let mut hidden = part(wash(AMBER), Blend::Normal);
        hidden.look.enabled = false;
        let stack = Stack::new(vec![quiet, hidden], BOARD);
        let reading = stack.read(&stack.current());
        assert_eq!(reading.visible, vec![None, None]);
        assert_eq!(stack.sense(0).trouble, None);
        assert_eq!(stack.sense(1).trouble, None);
    }

    #[test]
    fn the_strip_runs_cold_to_hot_over_a_dense_layer() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(wash(AMBER), Blend::Normal)], BOARD);
        let strip = stack.strip(1, 10);
        assert_eq!(strip.len(), 11);
        assert!(strip[0].cold > 0.9, "at 0% the top layer is unseen: {:?}", strip[0]);
        assert!(strip[10].hot > 0.9, "at 100% it buries the layer beneath: {:?}", strip[10]);
        let middle = strip[4];
        assert!(middle.warm > middle.cold && middle.warm > middle.hot, "somewhere between, both read: {middle:?}");
    }

    #[test]
    fn a_new_sparse_layer_is_placed_so_the_one_beneath_still_shows() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(sparks(AMBER), Blend::Normal)], BOARD);
        let look = stack.place(1, Blend::Normal);
        assert_ne!(look.blend, Blend::Normal, "Normal would black out the layer beneath: {look:?}");
        assert!(look.enabled);
    }

    #[test]
    fn a_dark_layer_is_still_placed_so_it_hides_nothing() {
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(wash(Rgb::BLACK), Blend::Normal)], BOARD);
        assert_eq!(stack.sense(1).trouble, Some(Trouble::Erases(0)), "selected, the dark layer hears what it does");
        assert_ne!(stack.place(1, Blend::Normal).blend, Blend::Normal);
    }

    #[test]
    fn a_new_layer_on_its_own_keeps_its_own_blend() {
        let stack = Stack::new(vec![part(sparks(AMBER), Blend::Normal)], BOARD);
        assert_eq!(stack.place(0, Blend::Cut).blend, Blend::Cut);
    }

    #[test]
    fn reach_is_dark_where_a_layer_is_buried_and_full_where_it_shows() {
        let mut top = part(wash(AMBER), Blend::Normal);
        top.region = (0..8).collect();
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), top], BOARD);
        let reach = stack.reach(&stack.current(), 0);
        assert!(reach[..8].iter().all(|r| r.is_some_and(|r| r < 0.05)), "under the cover: {:?}", &reach[..8]);
        assert!(reach[8..].iter().all(|r| r.is_some_and(|r| r > 0.95)), "in the open: {:?}", &reach[8..]);
        let sparse = Stack::new(vec![part(sparks(AMBER), Blend::Normal)], BOARD);
        assert!(sparse.reach(&sparse.current(), 0).iter().all(Option::is_some), "every cell lights in some frame");
    }

    #[test]
    fn harm_marks_the_cells_where_the_culprit_does_it() {
        let mut top = part(wash(AMBER), Blend::Normal);
        top.region = (0..8).collect();
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), top], BOARD);
        let harm = stack.harm(&stack.current(), 1, 0);
        assert!(harm[..8].iter().all(|&h| h) && harm[8..].iter().all(|&h| !h), "{harm:?}");
        let along = Stack::new(vec![part(wash(TEAL), Blend::Normal), part(sparks(AMBER), Blend::Screen)], BOARD);
        assert!(along.harm(&along.current(), 1, 0).iter().all(|&h| !h), "a screen over a wash harms nothing");
    }

    #[test]
    fn moving_puts_a_layer_where_asked() {
        let stack = Stack::new(
            vec![part(wash(TEAL), Blend::Normal), part(wash(AMBER), Blend::Normal), part(sparks(TEAL), Blend::Screen)],
            BOARD,
        );
        let arr = stack.current();
        assert_eq!(stack.apply(&arr, Fix::Above { part: 0, other: 1 }).order, vec![1, 0, 2]);
        assert_eq!(stack.apply(&arr, Fix::Below { part: 2, other: 0 }).order, vec![2, 0, 1]);
    }

    #[test]
    fn a_ducking_layer_dims_what_is_beneath_where_it_lights() {
        let spark = take(|_, i| if i == 5 { AMBER } else { Rgb::BLACK });
        let mut top = part(spark, Blend::Add);
        top.look.duck = true;
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), top], BOARD);
        let frame = stack.frame(&stack.current(), 0);
        // under the spark the wash keeps a fifth, and the spark adds over it
        assert_eq!(frame[5], Rgb::new(0xFF, 0xC8, 0x33), "the key itself");
        // its four neighbours keep three fifths
        for i in [1, 4, 6, 9] {
            assert_eq!(frame[i], Rgb::new(0x13, 0x79, 0x6A), "neighbour {i}");
        }
        // further off, nothing changes
        for i in [0, 2, 7, 10, 15] {
            assert_eq!(frame[i], TEAL, "far cell {i}");
        }
    }

    #[test]
    fn ducking_lets_a_dim_layer_read_over_a_bright_one() {
        let bright = Rgb::new(0xD0, 0xD0, 0xC0);
        let dim = Rgb::new(0x50, 0x30, 0x08);
        let stack = Stack::new(vec![part(wash(bright), Blend::Normal), part(sparks(dim), Blend::Add)], BOARD);
        let arr = stack.current();
        let before = stack.read(&arr).visible[1].unwrap();
        let after = stack.read(&stack.apply(&arr, Fix::Duck { part: 1 })).visible[1].unwrap();
        assert!(after > before + 0.15, "ducking frees it: {before} -> {after}");
        let sense = stack.sense(1);
        assert!(matches!(sense.trouble, Some(Trouble::LostIn(0) | Trouble::WashesOut(0))), "{:?}", sense.trouble);
        assert!(sense.offers.iter().any(|o| o.fix == Fix::Duck { part: 1 }), "ducking is offered: {:?}", sense.offers);
        // and once it ducks, it isn't offered again
        let mut top = part(sparks(dim), Blend::Add);
        top.look.duck = true;
        let ducked = Stack::new(vec![part(wash(bright), Blend::Normal), top], BOARD);
        assert!(ducked.sense(1).offers.iter().all(|o| o.fix != Fix::Duck { part: 1 }));
    }

    #[test]
    fn a_wash_out_names_the_layer_whose_light_does_it() {
        // a faint layer between, left unjudged so only the blame for the white is in question
        let mut faint = part(wash(Rgb::new(0x10, 0x10, 0x18)), Blend::Add);
        faint.known = false;
        let stack = Stack::new(
            vec![part(wash(Rgb::new(0xA0, 0xC0, 0xF0)), Blend::Normal), faint, part(wash(Rgb::new(0xC0, 0x40, 0x20)), Blend::Add)],
            BOARD,
        );
        assert_eq!(stack.sense(2).trouble, Some(Trouble::WashesOut(0)), "the bright wash two down, not the dim one between");
    }

    #[test]
    fn a_layer_buried_twice_over_is_buried_by_the_one_on_top() {
        let stack = Stack::new(
            vec![part(wash(TEAL), Blend::Normal), part(wash(AMBER), Blend::Normal), part(wash(Rgb::new(0xC0, 0x20, 0x40)), Blend::Normal)],
            BOARD,
        );
        assert_eq!(stack.sense(0).trouble, Some(Trouble::CoveredBy(2)), "neither cover alone frees it; the top one is what's seen");
        assert_eq!(stack.sense(1).trouble, Some(Trouble::CoveredBy(2)));
    }

    #[test]
    fn each_key_is_owned_by_the_layer_that_shows_there() {
        let mut top = part(wash(AMBER), Blend::Normal);
        top.region = (0..8).collect();
        let stack = Stack::new(vec![part(wash(TEAL), Blend::Normal), top], BOARD);
        let owners = stack.owners(&stack.current());
        assert!(owners[..8].iter().all(|o| *o == Some(1)) && owners[8..].iter().all(|o| *o == Some(0)), "{owners:?}");
        let dark = Stack::new(vec![part(wash(Rgb::BLACK), Blend::Normal)], BOARD);
        assert!(dark.owners(&dark.current()).iter().all(Option::is_none));
    }

    /// Properties every stack has, whatever its layers: random boards, colours over several frames,
    /// blends, opacities, ducking and regions.
    mod properties {
        use super::*;
        use crate::pattern::{Compositor, LayerDef, LayerInstanceId, LightingSession, ProcessInputHub, SceneLayerSpec};
        use proptest::prelude::*;

        const BLENDS: [Blend; 5] = [Blend::Normal, Blend::Screen, Blend::Add, Blend::Multiply, Blend::Cut];

        /// One layer as the test sees it: its frames, look and region (empty: the whole board).
        #[derive(Clone, Debug)]
        struct Layer {
            frames: Vec<Vec<Rgb>>,
            look: Look,
            region: Vec<u32>,
            known: bool,
        }

        #[derive(Clone, Debug)]
        struct Case {
            dims: (usize, usize),
            layers: Vec<Layer>,
        }

        impl Case {
            fn stack(&self) -> Stack {
                let parts = self
                    .layers
                    .iter()
                    .map(|l| Part { take: Arc::new(Take { frames: l.frames.clone() }), region: l.region.clone(), look: l.look, known: l.known })
                    .collect();
                Stack::new(parts, self.dims)
            }

            /// The stack as `arr` arranges it, built that way from the start: the layers in its
            /// order, each in its look.
            fn arranged(&self, arr: &Arrangement) -> Case {
                let layers = arr.order.iter().map(|&p| Layer { look: arr.looks[p], ..self.layers[p].clone() }).collect();
                Case { dims: self.dims, layers }
            }

            /// The same stack as the live compositor builds it, from each layer's frame `f` as a
            /// painted layer.
            fn defs(&self, f: usize) -> Vec<LayerDef> {
                self.layers
                    .iter()
                    .enumerate()
                    .map(|(i, l)| LayerDef {
                        id: Some(LayerInstanceId(i as u64 + 1)),
                        pattern: "custom".into(),
                        frame: l.frames[f].iter().map(|c| [c.r, c.g, c.b]).collect(),
                        region: l.region.clone(),
                        blend: l.look.blend,
                        opacity: l.look.opacity,
                        duck: l.look.duck,
                        enabled: l.look.enabled,
                        ..LayerDef::default()
                    })
                    .collect()
            }
        }

        fn colour() -> impl Strategy<Value = Rgb> {
            // black and white often, so dark gaps, full light and clipping all come up
            prop_oneof![
                2 => Just(Rgb::BLACK),
                1 => Just(Rgb::new(255, 255, 255)),
                6 => (any::<u8>(), any::<u8>(), any::<u8>()).prop_map(|(r, g, b)| Rgb::new(r, g, b)),
            ]
        }

        fn layer(cells: usize, frames: usize) -> impl Strategy<Value = Layer> {
            (
                prop::collection::vec(prop::collection::vec(colour(), cells), frames),
                0..BLENDS.len(),
                prop_oneof![Just(0.0f32), Just(1.0f32), 0.0f32..=1.0],
                any::<bool>(),
                prop::bool::weighted(0.9),
                prop::option::weighted(0.3, prop::collection::btree_set(0..cells as u32, 1..=cells)),
                prop::bool::weighted(0.9),
            )
                .prop_map(|(frames, b, opacity, duck, enabled, region, known)| Layer {
                    frames,
                    look: Look { blend: BLENDS[b], opacity, enabled, duck },
                    region: region.map(|r| r.into_iter().collect()).unwrap_or_default(),
                    known,
                })
        }

        fn case() -> impl Strategy<Value = Case> {
            (1usize..=3, 1usize..=5, 1usize..=3).prop_flat_map(|(rows, cols, frames)| {
                prop::collection::vec(layer(rows * cols, frames), 1..=4).prop_map(move |layers| Case { dims: (rows, cols), layers })
            })
        }

        fn config() -> ProptestConfig {
            ProptestConfig { cases: 96, ..ProptestConfig::default() }
        }

        proptest! {
            #![proptest_config(config())]

            /// What stack sense reads is what the board shows: its compositing matches the live
            /// compositor's (and the lighting session's) for every blend, opacity, duck and region.
            #[test]
            fn stack_sense_composites_exactly_like_the_live_board(case in case()) {
                let _no_keys = crate::capture::suppress_key_reads();
                let stack = case.stack();
                let (rows, cols) = (case.dims.0 as u8, case.dims.1 as u8);
                for f in 0..case.layers[0].frames.len() {
                    let defs = case.defs(f);
                    let live = Compositor::from_defs(&defs).render_at_time(rows, cols, std::time::Duration::ZERO);
                    prop_assert_eq!(&stack.frame(&stack.current(), f), &live, "frame {}", f);
                    let specs: Vec<SceneLayerSpec> = defs.iter().map(|d| SceneLayerSpec { id: d.id.unwrap(), def: d.clone() }).collect();
                    let mut session = LightingSession::from_specs((rows, cols), &specs).unwrap();
                    session.advance_tick(&ProcessInputHub::new(), std::time::Instant::now());
                    let resolved = session.resolve_presentation(std::time::Duration::ZERO);
                    prop_assert_eq!(resolved.pixels.as_ref(), live.as_slice());
                }
            }

            /// A reading is always in range, and says nothing about a layer it can't judge.
            #[test]
            fn a_reading_is_in_range_and_silent_on_what_it_cannot_judge(case in case()) {
                let stack = case.stack();
                let reading = stack.read(&stack.current());
                prop_assert!(reading.score.is_finite());
                for (p, l) in case.layers.iter().enumerate() {
                    if let Some(v) = reading.visible[p] {
                        prop_assert!((0.0..=1.0).contains(&v), "visible {}", v);
                        prop_assert!(l.look.enabled && l.known, "judged a layer it can't speak for");
                    }
                    prop_assert!((0.0..=1.0).contains(&reading.washed[p]));
                }
            }

            /// A layer at zero opacity, wherever it sits, changes nothing on the board and nothing in
            /// how any other layer reads.
            #[test]
            fn a_layer_at_zero_opacity_changes_nothing(case in case(), at in any::<prop::sample::Index>(), ghost in layer(15, 3)) {
                let cells = case.dims.0 * case.dims.1;
                let frames = case.layers[0].frames.len();
                let mut ghost = ghost;
                ghost.frames = ghost.frames.iter().cycle().take(frames).map(|f| f.iter().copied().cycle().take(cells).collect()).collect();
                ghost.region.retain(|&c| (c as usize) < cells);
                ghost.look.opacity = 0.0;
                let mut with = case.clone();
                let at = at.index(case.layers.len() + 1);
                with.layers.insert(at, ghost);
                let (a, b) = (case.stack(), with.stack());
                for f in 0..frames {
                    prop_assert_eq!(a.frame(&a.current(), f), b.frame(&b.current(), f));
                }
                let (ra, rb) = (a.read(&a.current()), b.read(&b.current()));
                for p in 0..case.layers.len() {
                    let q = if p >= at { p + 1 } else { p };
                    prop_assert_eq!(ra.visible[p], rb.visible[q], "layer {}", p);
                }
            }

            /// Every offer is what it says: its reading is the stack's with the fix applied, it changes
            /// what the board shows, and it eases the trouble it was offered for. Every trouble names
            /// real, other, switched-on layers.
            #[test]
            fn every_offer_is_honest_and_every_trouble_well_formed(case in case()) {
                let stack = case.stack();
                let arr = stack.current();
                let n = case.layers.len();
                let now = stack.read(&arr);
                for p in 0..n {
                    let sense = stack.sense_from(p, now.clone(), |_| true);
                    prop_assert_eq!(&sense, &stack.sense(p));
                    let Some(trouble) = sense.trouble else {
                        prop_assert!(sense.offers.is_empty());
                        continue;
                    };
                    match trouble {
                        Trouble::ErasedBy(o) | Trouble::CoveredBy(o) | Trouble::Erases(o) | Trouble::Covers(o)
                        | Trouble::WashesOut(o) | Trouble::LostIn(o) => {
                            prop_assert!(o < n && o != p && arr.looks[o].enabled, "{:?} from {}", trouble, p);
                        }
                        Trouble::NothingUnder => {}
                    }
                    prop_assert!(sense.offers.len() <= OFFERS);
                    for offer in &sense.offers {
                        let fixed = stack.apply(&arr, offer.fix);
                        prop_assert_eq!(&offer.after, &stack.read(&fixed));
                        prop_assert!(Stack::eases(trouble, p, &now, &offer.after), "{:?} doesn't ease {:?}", offer.fix, trouble);
                        let frames = case.layers[0].frames.len();
                        prop_assert!((0..frames).any(|f| stack.frame(&fixed, f) != stack.frame(&arr, f)), "{:?} changes nothing", offer.fix);
                    }
                }
            }

            /// Refusing a kind of fix takes only that kind away: the trouble stays the same, nothing
            /// refused is offered, and every offer that wasn't refused is still offered.
            #[test]
            fn a_refused_fix_leaves_room_for_the_next_best(case in case(), refused in 0u8..4) {
                let stack = case.stack();
                let now = stack.read(&stack.current());
                let refuses = |fix: Fix| match fix {
                    Fix::Blend { .. } => refused == 0,
                    Fix::Opacity { .. } => refused == 1,
                    Fix::Above { .. } | Fix::Below { .. } => refused == 2,
                    Fix::Duck { .. } => refused == 3,
                };
                for p in 0..case.layers.len() {
                    let all = stack.sense_from(p, now.clone(), |_| true);
                    let some = stack.sense_from(p, now.clone(), |f| !refuses(f));
                    prop_assert_eq!(all.trouble, some.trouble);
                    prop_assert!(some.offers.iter().all(|o| !refuses(o.fix)));
                    let kept: Vec<Fix> = all.offers.iter().map(|o| o.fix).filter(|&f| !refuses(f)).collect();
                    prop_assert!(kept.iter().all(|f| some.offers.iter().any(|o| o.fix == *f)), "{:?} lost from {:?}", kept, some.offers);
                }
            }

            /// A fix applied to a stack reads exactly like the stack built with it applied: the same
            /// frames, the same score, and each layer the same visibility in its new place. What the
            /// strip previews is what keeping the fix leaves.
            #[test]
            fn a_fixed_arrangement_reads_like_the_stack_it_leaves(case in case()) {
                let stack = case.stack();
                let arr = stack.current();
                let frames = case.layers[0].frames.len();
                for p in 0..case.layers.len() {
                    for offer in stack.sense(p).offers {
                        let fixed = stack.apply(&arr, offer.fix);
                        let rebuilt = case.arranged(&fixed).stack();
                        for f in 0..frames {
                            prop_assert_eq!(rebuilt.frame(&rebuilt.current(), f), stack.frame(&fixed, f));
                        }
                        let (was, now) = (stack.read(&fixed), rebuilt.read(&rebuilt.current()));
                        prop_assert!((was.score - now.score).abs() < 1e-4, "{} vs {}", was.score, now.score);
                        for (at, &q) in fixed.order.iter().enumerate() {
                            prop_assert_eq!(was.visible[q], now.visible[at]);
                        }
                    }
                }
            }

            /// Keeping the best fix over and over settles: every kept fix leaves a stack that reads
            /// better than the one before, so the strip never talks someone round in a circle, and it
            /// runs out of fixes within a handful of keeps.
            #[test]
            fn keeping_fixes_settles(case in case()) {
                let mut case = case;
                let mut scores = Vec::new();
                for _ in 0..24 {
                    let stack = case.stack();
                    let arr = stack.current();
                    let now = stack.read(&arr);
                    scores.push(now.score);
                    let best = (0..case.layers.len())
                        .flat_map(|p| stack.sense_from(p, now.clone(), |_| true).offers)
                        .max_by(|a, b| a.after.score.total_cmp(&b.after.score));
                    let Some(best) = best else { return Ok(()) };
                    prop_assert!(best.after.score > now.score, "{:?} keeps {} -> {}", best.fix, now.score, best.after.score);
                    case = case.arranged(&stack.apply(&arr, best.fix));
                }
                prop_assert!(false, "still offering after 24 keeps: {:?}", scores);
            }

            /// A move keeps every layer, once.
            #[test]
            fn a_move_keeps_every_layer(case in case(), a in any::<prop::sample::Index>(), b in any::<prop::sample::Index>(), up in any::<bool>()) {
                let stack = case.stack();
                let n = case.layers.len();
                if n < 2 {
                    return Ok(());
                }
                let part = a.index(n);
                let other = (part + 1 + b.index(n - 1)) % n;
                let fix = if up { Fix::Above { part, other } } else { Fix::Below { part, other } };
                let mut order = stack.apply(&stack.current(), fix).order;
                let at = |o: &[usize], x: usize| o.iter().position(|&p| p == x).unwrap();
                let (pa, oa) = (at(&order, part), at(&order, other));
                prop_assert_eq!(pa, if up { oa + 1 } else { oa - 1 });
                order.sort_unstable();
                prop_assert_eq!(order, (0..n).collect::<Vec<_>>());
            }

            /// The layer a key is said to show is one that lights it, and taking it away changes that
            /// key.
            #[test]
            fn a_key_belongs_to_a_layer_that_changes_it(case in case()) {
                let stack = case.stack();
                let arr = stack.current();
                let frames = case.layers[0].frames.len();
                for (i, owner) in stack.owners(&arr).into_iter().enumerate() {
                    let Some(p) = owner else { continue };
                    prop_assert!(arr.looks[p].enabled && stack.masks[p][i]);
                    let gone = Stack::without(&arr, p);
                    prop_assert!((0..frames).any(|f| stack.frame(&arr, f)[i] != stack.frame(&gone, f)[i]), "key {} said to show {}", i, p);
                }
            }

            /// The opacity rail has one step per opacity, every reading in range.
            #[test]
            fn the_rail_is_one_reading_per_step(case in case(), at in any::<prop::sample::Index>()) {
                let stack = case.stack();
                let part = at.index(case.layers.len());
                let strip = stack.strip(part, 10);
                prop_assert!(strip.is_empty() || strip.len() == 11);
                for z in strip {
                    prop_assert!([z.cold, z.warm, z.hot].iter().all(|v| (0.0..=1.0).contains(v)), "{:?}", z);
                }
            }

            /// A new layer lands in a look at least as readable as the one it would have started in.
            #[test]
            fn a_placed_layer_reads_no_worse_than_its_default(case in case(), at in any::<prop::sample::Index>(), b in 0..BLENDS.len()) {
                let stack = case.stack();
                let part = at.index(case.layers.len());
                let preferred = BLENDS[b];
                let look = stack.place(part, preferred);
                prop_assert!((0.0..=1.0).contains(&look.opacity));
                let mut placed = stack.current();
                placed.looks[part] = look;
                let mut start = stack.current();
                start.looks[part] = Look { blend: preferred, opacity: 1.0, enabled: true, duck: false };
                if case.layers.len() >= 2 && case.layers[part].known {
                    prop_assert!(stack.read(&placed).score + 0.08 >= stack.read(&start).score, "{:?}", look);
                }
            }

            /// Ducking only ever takes light away, by at most its depth, and only inside the layer's
            /// region; at zero opacity it does nothing.
            #[test]
            fn ducking_only_dims_and_only_where_the_layer_is(
                px in prop::collection::vec(colour(), 15),
                under in colour(),
                region in prop::collection::btree_set(0usize..15, 0..15),
                opacity in 0.0f32..=1.0,
            ) {
                let mut w = Vec::new();
                let covers = |i: usize| region.is_empty() || region.contains(&i);
                crate::effects::duck_weights(&px, covers, 3, 5, &mut w);
                for (i, &wi) in w.iter().enumerate() {
                    prop_assert!((0.0..=1.0).contains(&wi));
                    if !covers(i) {
                        prop_assert_eq!(wi, 0.0);
                    }
                    let d = crate::effects::duck_px(under, wi, opacity);
                    prop_assert!(d.r <= under.r && d.g <= under.g && d.b <= under.b);
                    let floor = |c: u8| (f32::from(c) * (1.0 - crate::effects::DUCK_DEPTH)).floor() as u8;
                    prop_assert!(d.r >= floor(under.r) && d.g >= floor(under.g) && d.b >= floor(under.b));
                    prop_assert_eq!(crate::effects::duck_px(under, wi, 0.0), under);
                }
            }

            /// Layers that only add light give the same board in any order.
            #[test]
            fn added_light_does_not_care_about_order(case in case(), seed in any::<u64>()) {
                let mut case = case;
                for l in &mut case.layers {
                    l.look = Look { blend: Blend::Add, opacity: 1.0, enabled: true, duck: false };
                }
                let mut shuffled = case.clone();
                let n = shuffled.layers.len();
                shuffled.layers.rotate_left((seed as usize) % n);
                let (a, b) = (case.stack(), shuffled.stack());
                for f in 0..case.layers[0].frames.len() {
                    prop_assert_eq!(a.frame(&a.current(), f), b.frame(&b.current(), f));
                }
            }
        }
    }
}
