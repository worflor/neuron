// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Colour suggestions for a palette someone is editing.
//!
//! An [`Engine`] reads a [`Scene`] (the palette as it stands, what it looked like before the edit
//! in progress, the stop being worked on, and the stops that must not move) and proposes
//! [`Offer`]s: complete alternative palettes. [`propose`] turns those into a [`Slate`] after
//! checking every offer against the scene, so no engine can move a pinned stop, change the stop
//! count or positions, or offer what is already there. An empty slate means "keep what you have".
//!
//! Nothing here applies anything. The caller previews an offer, and takes one only when the person
//! picks it, through [`Slate::take`], which refuses a slate made for a different scene.
//!
//! [`Rules`] is the built-in engine: closed-form colour reasoning in OKLab. A learned engine plugs
//! in behind the same trait and inherits the same checks.

use crate::lighting::Rgb;
use crate::spectrum::{oklab_in_gamut, oklab_to_rgb, rgb_to_oklab, Interp, Stop};

/// The most offers a slate carries. A choice among a few is a suggestion; a long list is a menu.
pub const MAX_OFFERS: usize = 4;
/// OKLab distance below which two colours read as the same. Offers that move nothing further than
/// this, or that differ from an earlier offer by less, are dropped.
const SAME: f32 = 0.02;

/// What the engine is asked about.
#[derive(Clone, Debug, PartialEq)]
pub struct Scene {
    pub stops: Vec<Stop>,
    pub interp: Interp,
    /// The stops as they were before the edit in progress, when there is one. Same length and
    /// positions as `stops`, or it is ignored.
    pub before: Option<Vec<Stop>>,
    /// The stop being worked on.
    pub selected: usize,
    /// Stops no offer may change. Shorter than `stops` means the rest are free.
    pub pins: Vec<bool>,
}

impl Scene {
    fn pinned(&self, i: usize) -> bool {
        self.pins.get(i).copied().unwrap_or(false)
    }

    /// Whether the selected stop is one the edit in progress has recoloured. One-stop offers are
    /// about the colour being chosen, never about a stop nobody touched.
    fn selected_is_edited(&self) -> bool {
        let i = self.selected;
        self.before.as_ref().is_some_and(|b| b.len() == self.stops.len() && i < b.len() && b[i].col != self.stops[i].col)
    }

    /// A value that changes whenever anything an offer depends on changes.
    fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for group in std::iter::once(&self.stops).chain(self.before.iter()) {
            group.len().hash(&mut h);
            for s in group {
                (s.col.r, s.col.g, s.col.b, s.at.to_bits()).hash(&mut h);
            }
        }
        (self.interp.as_str(), self.selected, &self.pins).hash(&mut h);
        h.finish()
    }
}

/// How much of the palette an offer touches. The caller draws a one-stop offer as a colour and a
/// palette offer as a ramp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Stop,
    Palette,
}

/// One alternative: the whole palette as it would be.
#[derive(Clone, Debug, PartialEq)]
pub struct Offer {
    /// Stable within an engine, for telemetry-free bookkeeping and tests.
    pub id: &'static str,
    /// A few plain words for what taking it does.
    pub title: &'static str,
    pub scope: Scope,
    pub stops: Vec<Stop>,
    pub interp: Interp,
}

/// Something that proposes offers. It may return anything; [`propose`] keeps what is valid.
pub trait Engine {
    fn offers(&self, scene: &Scene) -> Vec<Offer>;
}

/// The checked offers for one scene.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Slate {
    scene: u64,
    pub offers: Vec<Offer>,
}

impl Slate {
    /// The offer at `index`, if this slate was made for `scene` as it is now.
    #[must_use]
    pub fn take(&self, scene: &Scene, index: usize) -> Option<&Offer> {
        (self.scene == scene.fingerprint()).then(|| self.offers.get(index)).flatten()
    }
}

/// Ask `engine` about `scene` and keep the offers that respect it.
#[must_use]
pub fn propose(engine: &dyn Engine, scene: &Scene) -> Slate {
    let mut offers: Vec<Offer> = Vec::new();
    for offer in engine.offers(scene) {
        let shape_kept = offer.stops.len() == scene.stops.len()
            && offer.stops.iter().zip(&scene.stops).enumerate().all(|(i, (new, old))| {
                new.at == old.at && (new.col == old.col || !scene.pinned(i))
            });
        let moved = offer.interp != scene.interp || apart(&offer.stops, &scene.stops) >= SAME;
        let fresh = offers.iter().all(|kept| kept.interp != offer.interp || apart(&kept.stops, &offer.stops) >= SAME);
        if shape_kept && moved && fresh {
            offers.push(offer);
        }
        if offers.len() == MAX_OFFERS {
            break;
        }
    }
    Slate { scene: scene.fingerprint(), offers }
}

/// The largest OKLab distance between corresponding stops.
fn apart(a: &[Stop], b: &[Stop]) -> f32 {
    a.iter().zip(b).map(|(x, y)| distance(lab(x.col), lab(y.col))).fold(0.0, f32::max)
}

type Lab = [f32; 3];

fn lab(c: Rgb) -> Lab {
    let (l, a, b) = rgb_to_oklab(c);
    [l, a, b]
}

fn rgb(c: Lab) -> Rgb {
    oklab_to_rgb(c[0], c[1], c[2])
}

fn distance(a: Lab, b: Lab) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn chroma(c: Lab) -> f32 {
    c[1].hypot(c[2])
}

/// A colour as OKLCh: lightness, chroma, and hue in turns (`0..1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lch {
    pub l: f32,
    pub c: f32,
    pub h: f32,
}

impl Lch {
    #[must_use]
    pub fn of(c: Rgb) -> Lch {
        let v = lab(c);
        Lch { l: v[0], c: chroma(v), h: (v[2].atan2(v[1]) / std::f32::consts::TAU).rem_euclid(1.0) }
    }

    /// The nearest displayable colour: out-of-gamut chroma is reduced, lightness and hue kept.
    #[must_use]
    pub fn rgb(self) -> Rgb {
        let angle = self.h * std::f32::consts::TAU;
        oklab_to_rgb(self.l, self.c * angle.cos(), self.c * angle.sin())
    }

    /// The most chroma a display can show at this lightness and hue.
    #[must_use]
    pub fn max_chroma(l: f32, h: f32) -> f32 {
        let angle = h * std::f32::consts::TAU;
        let (mut lo, mut hi) = (0.0f32, 0.4f32);
        for _ in 0..12 {
            let mid = (lo + hi) / 2.0;
            if oklab_in_gamut(l, mid * angle.cos(), mid * angle.sin()) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        lo
    }
}

/// The built-in engine. Each rule answers one question a person adjusting a gradient tends to hit
/// next, and stays silent when the palette gives it no reason to speak. It only speaks during an
/// edit (a scene with a `before`): a palette nobody is touching is left alone.
pub struct Rules;

impl Engine for Rules {
    fn offers(&self, scene: &Scene) -> Vec<Offer> {
        if scene.before.is_none() {
            return Vec::new();
        }
        [follow(scene), between(scene), pace(scene), mix(scene), vivid(scene)].into_iter().flatten().collect()
    }
}

/// You moved one colour: carry the same shift through its neighbours, fading with distance and
/// stopping at a pinned stop, so the ramp keeps its shape around the change.
fn follow(scene: &Scene) -> Option<Offer> {
    let before = scene.before.as_ref().filter(|b| {
        b.len() == scene.stops.len() && b.iter().zip(&scene.stops).all(|(x, y)| x.at == y.at)
    })?;
    let mut changed = (0..before.len()).filter(|&i| before[i].col != scene.stops[i].col);
    let (edited, None) = (changed.next()?, changed.next()) else { return None };
    let (was, now) = (lab(before[edited].col), lab(scene.stops[edited].col));
    let shift = [now[0] - was[0], now[1] - was[1], now[2] - was[2]];
    if distance(was, now) < SAME * 2.0 {
        return None;
    }
    let at = |i: usize| scene.stops[i].at;
    // how far the shift reaches on each side: to the nearest pinned stop, else one stop's spacing
    // past the end, so an end stop beside the edit still takes a fair share
    let reach = |pin: Option<usize>, end: f32| (pin.map_or(end, at) - at(edited)).abs().max(1e-3);
    let last = scene.stops.len() - 1;
    let spacing = (at(last) - at(0)) / last.max(1) as f32;
    let left = reach((0..edited).rev().find(|&i| scene.pinned(i)), at(0) - spacing);
    let right = reach((edited + 1..=last).find(|&i| scene.pinned(i)), at(last) + spacing);
    let stops = scene.stops.iter().enumerate().map(|(i, s)| {
        if i == edited || scene.pinned(i) {
            return *s;
        }
        let far = (s.at - at(edited)).abs() / if s.at < at(edited) { left } else { right };
        let w = (1.0 - far).clamp(0.0, 1.0);
        let w = w * w * (3.0 - 2.0 * w);
        let c = lab(s.col);
        Stop::new(rgb([c[0] + shift[0] * w, c[1] + shift[1] * w, c[2] + shift[2] * w]), s.at)
    }).collect();
    Some(Offer { id: "follow", title: "carry the change through", scope: Scope::Palette, stops, interp: scene.interp })
}

/// The selected stop sits between two others: the colour that lies on the way from one to the
/// other, at this stop's position, through hue instead of through grey.
fn between(scene: &Scene) -> Option<Offer> {
    let i = scene.selected;
    if i == 0 || i + 1 >= scene.stops.len() || scene.pinned(i) || !scene.selected_is_edited() {
        return None;
    }
    let (a, b, s) = (scene.stops[i - 1], scene.stops[i + 1], scene.stops[i]);
    let t = ((s.at - a.at) / (b.at - a.at).max(1e-6)).clamp(0.0, 1.0);
    let (la, lb) = (Lch::of(a.col), Lch::of(b.col));
    // a grey end has no hue of its own; it borrows the other end's
    let (ha, hb) = match (la.c < 0.02, lb.c < 0.02) {
        (true, false) => (lb.h, lb.h),
        (false, true) => (la.h, la.h),
        _ => (la.h, lb.h),
    };
    let turn = (hb - ha + 0.5).rem_euclid(1.0) - 0.5;
    let mid = Lch { l: la.l + (lb.l - la.l) * t, c: la.c + (lb.c - la.c) * t, h: (ha + turn * t).rem_euclid(1.0) };
    let mut stops = scene.stops.clone();
    stops[i].col = mid.rgb();
    Some(Offer { id: "between", title: "sit between its neighbours", scope: Scope::Stop, stops, interp: scene.interp })
}

/// A ramp that runs dark to light (or back) with a stop that turns the other way: put the inner
/// stops' lightness on the line between the ends, keeping each one's hue and chroma. A ramp that
/// only bends, without reversing, is a choice and is left alone.
fn pace(scene: &Scene) -> Option<Offer> {
    let n = scene.stops.len();
    if n < 3 {
        return None;
    }
    let (first, last) = (scene.stops[0], scene.stops[n - 1]);
    let (l0, l1) = (lab(first.col)[0], lab(last.col)[0]);
    if (l1 - l0).abs() < 0.2 {
        return None;
    }
    let target = |s: &Stop| l0 + (l1 - l0) * ((s.at - first.at) / (last.at - first.at).max(1e-6)).clamp(0.0, 1.0);
    let rising = l1 > l0;
    let reverses = scene.stops.windows(2).any(|pair| {
        let step = lab(pair[1].col)[0] - lab(pair[0].col)[0];
        if rising { step < -0.03 } else { step > 0.03 }
    });
    if !reverses {
        return None;
    }
    let stops = scene.stops.iter().enumerate().map(|(i, s)| {
        if i == 0 || i == n - 1 || scene.pinned(i) {
            return *s;
        }
        let c = lab(s.col);
        Stop::new(rgb([target(s), c[1], c[2]]), s.at)
    }).collect();
    Some(Offer { id: "pace", title: "even out the light", scope: Scope::Palette, stops, interp: scene.interp })
}

/// Mixed directly, two vivid colours far apart in hue pass through a dull middle. Offer the mix
/// that goes around the hue wheel instead; the stops stay as they are.
fn mix(scene: &Scene) -> Option<Offer> {
    if scene.interp != Interp::Rgb {
        return None;
    }
    let muddy = scene.stops.windows(2).any(|pair| {
        let (a, b) = (lab(pair[0].col), lab(pair[1].col));
        let middle = lab(Rgb::lerp(pair[0].col, pair[1].col, 0.5));
        chroma(a).min(chroma(b)) > 0.08 && chroma(middle) < 0.6 * chroma(a).min(chroma(b))
    });
    muddy.then(|| Offer {
        id: "mix",
        title: "mix through hue, not grey",
        scope: Scope::Palette,
        stops: scene.stops.clone(),
        interp: Interp::Hsv,
    })
}

/// The selected stop is much duller or much louder than the rest: bring its chroma to theirs.
fn vivid(scene: &Scene) -> Option<Offer> {
    let i = scene.selected;
    if scene.stops.len() < 3 || i >= scene.stops.len() || scene.pinned(i) || !scene.selected_is_edited() {
        return None;
    }
    let own = Lch::of(scene.stops[i].col);
    let others: Vec<f32> = (0..scene.stops.len()).filter(|&j| j != i).map(|j| Lch::of(scene.stops[j].col).c).collect();
    let usual = others.iter().sum::<f32>() / others.len() as f32;
    // a grey stop among greys, or one already close, has nothing to match
    if own.c < 0.02 || (own.c - usual).abs() < 0.07 {
        return None;
    }
    let mut stops = scene.stops.clone();
    stops[i].col = Lch { c: usual.min(Lch::max_chroma(own.l, own.h)), ..own }.rgb();
    Some(Offer { id: "vivid", title: "match the others' vividness", scope: Scope::Stop, stops, interp: scene.interp })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(colors: &[(u8, u8, u8)]) -> Scene {
        let n = colors.len().max(2) - 1;
        Scene {
            stops: colors.iter().enumerate().map(|(i, c)| Stop::new(Rgb::new(c.0, c.1, c.2), i as f32 / n as f32)).collect(),
            interp: Interp::Oklab,
            before: None,
            selected: 0,
            pins: Vec::new(),
        }
    }

    /// The same scene mid-edit, with nothing changed yet.
    fn editing(mut s: Scene) -> Scene {
        s.before = Some(s.stops.clone());
        s
    }

    fn ids(slate: &Slate) -> Vec<&'static str> {
        slate.offers.iter().map(|o| o.id).collect()
    }

    const MEADOW: &[(u8, u8, u8)] = &[(12, 45, 34), (67, 190, 116), (206, 229, 128)];

    #[test]
    fn a_settled_palette_gets_no_offers() {
        for selected in 0..3 {
            let mut meadow = editing(scene(MEADOW));
            meadow.selected = selected;
            assert_eq!(propose(&Rules, &meadow).offers, [], "stop {selected}");
        }
        assert_eq!(propose(&Rules, &editing(scene(&[(255, 0, 0)]))).offers, []);
        assert_eq!(propose(&Rules, &editing(scene(&[]))).offers, []);
        // a palette with every fault at once, while nobody is editing it
        let mut resting = scene(&[(255, 0, 0), (10, 10, 12), (0, 200, 255)]);
        resting.interp = Interp::Rgb;
        resting.selected = 1;
        assert_eq!(propose(&Rules, &resting).offers, []);
    }

    #[test]
    fn follow_carries_one_edit_through_free_stops_and_stops_at_a_pin() {
        let mut s = scene(&[(20, 40, 90), (60, 110, 180), (120, 170, 220), (200, 225, 245)]);
        s.before = Some(s.stops.clone());
        s.stops[1].col = Rgb::new(150, 80, 170);
        s.selected = 1;
        let slate = propose(&Rules, &s);
        let follow = slate.offers.iter().find(|o| o.id == "follow").expect("one edited stop draws a follow offer");
        assert_eq!(follow.stops[1], s.stops[1], "the edited stop is kept exactly");
        let moved = |i: usize| distance(lab(follow.stops[i].col), lab(s.stops[i].col));
        assert!(moved(0) > 0.03 && moved(2) > 0.03, "its neighbours take part of the shift");
        assert!(moved(2) > moved(3), "the shift fades with distance");

        s.pins = vec![false, false, true, false];
        let pinned = propose(&Rules, &s);
        let follow = pinned.offers.iter().find(|o| o.id == "follow").unwrap();
        assert_eq!(follow.stops[2], s.stops[2]);
        assert_eq!(follow.stops[3], s.stops[3], "nothing past a pin moves");

        s.stops[3].col = Rgb::new(1, 2, 3);
        assert!(!ids(&propose(&Rules, &s)).contains(&"follow"), "two edits are not one gesture");
    }

    #[test]
    fn between_and_pace_and_vivid_answer_a_stop_out_of_line() {
        // a near-black middle in a dark-to-light ramp
        let mut s = editing(scene(&[(12, 45, 34), (60, 150, 90), (20, 30, 24), (206, 229, 128)]));
        s.selected = 2;
        s.before.as_mut().unwrap()[2].col = Rgb::new(120, 200, 110);
        let slate = propose(&Rules, &s);
        assert!(ids(&slate).contains(&"between") && ids(&slate).contains(&"pace"), "{:?}", ids(&slate));
        let between = &slate.offers.iter().find(|o| o.id == "between").unwrap().stops[2];
        let (lo, hi) = (lab(s.stops[1].col)[0], lab(s.stops[3].col)[0]);
        assert!((lo..hi).contains(&lab(between.col)[0]));
        assert_eq!(slate.offers.iter().find(|o| o.id == "between").unwrap().scope, Scope::Stop);

        let mut dull = editing(scene(&[(255, 40, 40), (150, 110, 95), (40, 90, 255)]));
        dull.selected = 1;
        dull.before.as_mut().unwrap()[1].col = Rgb::new(200, 60, 200);
        let slate = propose(&Rules, &dull);
        let vivid = slate.offers.iter().find(|o| o.id == "vivid").expect("a dull stop among vivid ones");
        assert!(Lch::of(vivid.stops[1].col).c > Lch::of(dull.stops[1].col).c + 0.05);
    }

    #[test]
    fn mix_speaks_only_for_a_muddy_direct_blend() {
        let mut s = editing(scene(&[(255, 0, 0), (0, 200, 255)]));
        s.interp = Interp::Rgb;
        let slate = propose(&Rules, &s);
        let mix = slate.offers.iter().find(|o| o.id == "mix").expect("red to cyan sags through grey");
        assert_eq!((mix.interp, &mix.stops), (Interp::Hsv, &s.stops), "only the mix changes");
        s.interp = Interp::Hsv;
        assert!(!ids(&propose(&Rules, &s)).contains(&"mix"));
        let mut near = editing(scene(&[(255, 0, 0), (255, 120, 0)]));
        near.interp = Interp::Rgb;
        assert!(!ids(&propose(&Rules, &near)).contains(&"mix"));
    }

    struct Rogue;
    impl Engine for Rogue {
        fn offers(&self, scene: &Scene) -> Vec<Offer> {
            let offer = |id, edit: &dyn Fn(&mut Vec<Stop>)| {
                let mut stops = scene.stops.clone();
                edit(&mut stops);
                Offer { id, title: "", scope: Scope::Palette, stops, interp: scene.interp }
            };
            let red = Rgb::new(255, 0, 0);
            let mut all = vec![
                offer("moves-a-pin", &|s| s[0].col = red),
                offer("drops-a-stop", &|s| { s.pop(); }),
                offer("moves-a-position", &|s| s[1].at = 0.9),
                offer("changes-nothing", &|_| {}),
                offer("fine", &|s| s[1].col = red),
                offer("same-as-fine", &|s| s[1].col = Rgb::new(254, 0, 0)),
            ];
            all.extend((0..10u8).map(|i| offer("more", &|s| s[2].col = Rgb::new(0, i * 25, 255 - i * 25))));
            all
        }
    }

    #[test]
    fn the_slate_refuses_what_an_engine_may_not_do() {
        let mut s = scene(MEADOW);
        s.pins = vec![true];
        let slate = propose(&Rogue, &s);
        assert_eq!(ids(&slate)[0], "fine");
        assert!(ids(&slate)[1..].iter().all(|id| *id == "more"), "{:?}", ids(&slate));
        assert_eq!(slate.offers.len(), MAX_OFFERS);
    }

    #[test]
    fn a_slate_is_only_good_for_the_scene_it_was_made_for() {
        let mut s = editing(scene(&[(255, 0, 0), (0, 200, 255)]));
        s.interp = Interp::Rgb;
        let slate = propose(&Rules, &s);
        assert!(slate.take(&s, 0).is_some());
        assert!(slate.take(&s, 9).is_none());
        let mut later = s.clone();
        later.stops[0].col = Rgb::new(250, 0, 0);
        assert!(slate.take(&later, 0).is_none(), "the palette changed under the slate");
    }

    #[test]
    fn lch_round_trips_and_knows_the_gamut_edge() {
        for c in [Rgb::new(12, 45, 34), Rgb::new(255, 0, 0), Rgb::new(40, 90, 255), Rgb::new(128, 128, 128)] {
            let back = Lch::of(c).rgb();
            assert!(distance(lab(c), lab(back)) < 0.01, "{c:?} -> {back:?}");
        }
        let red = Lch::of(Rgb::new(255, 0, 0));
        let edge = Lch::max_chroma(red.l, red.h);
        assert!((edge - red.c).abs() < 0.02, "pure red sits on the edge: {edge} vs {}", red.c);
        let grey = Lch::max_chroma(0.6, 0.4);
        assert!((0.05..0.35).contains(&grey), "{grey}");
        assert!(Lch::max_chroma(1.0, 0.3) < 0.02, "white has no room for chroma");
    }
}
