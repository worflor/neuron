// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! What the model says when the person asks outright, and the colour it would give a stop with
//! none: the two ways a picker reaches past the model's own judgement that the palette is fine.

use super::*;
use crate::spectrum::Stop;

/// How many options one round of asking shows for each question.
const ROUND: usize = 3;

/// The model asked outright, for when it has nothing to volunteer. It answers the questions its
/// own offers answer (and, with no edit in progress, what colour suits the selected stop's place
/// and a tidy of the palette), listing options even where it would rather leave the palette as it
/// is; those carry [`Offer::reach`]. `round` pages through the options: each round shows the next
/// few, wrapping when they run out.
pub struct Asked<'a> {
    pub model: &'a Learned,
    pub round: usize,
}

impl Engine for Asked<'_> {
    fn offers(&self, scene: &Scene) -> Vec<Offer> {
        let model = self.model;
        let Some(frame) = model.frame(scene) else { return Vec::new() };
        let (active, ask) = (frame.active, frame.ask());
        let byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        let colour = |c: &Srgb| Rgb::new(byte(c[0]), byte(c[1]), byte(c[2]));
        let skip = self.round * ROUND;
        let mut out = Vec::new();
        let mut offer = |answer: Answer, id: &'static str, title: &'static str, scope: Scope| {
            let n = answer.options.len();
            for k in 0..ROUND.min(n) {
                let option = &answer.options[(skip + k) % n];
                let stops = scene.stops.iter().zip(&option.rgb).zip(&frame.rgb).map(|((stop, new), old)| {
                    let mut stop = *stop;
                    if scope == Scope::Stop || new != old {
                        stop.col = colour(new);
                    }
                    stop
                }).collect();
                out.push(Offer { id, title, scope, stops, interp: scene.interp, reach: answer.keep });
            }
        };
        let many = MODES;
        if !frame.pins[active] {
            let (id, title) = if scene.open || frame.was.is_none() { ("fits", "a colour for this place") } else { ("fits", "fits this palette") };
            let request = match &frame.was {
                Some(_) if !scene.open => Ask { count: many, ..frame.stop(false) },
                _ => Ask { count: many, ..frame.stop(true) },
            };
            offer(model.choose(&request), id, title, Scope::Stop);
        }
        if scene.open {
            return out;
        }
        let held: Vec<bool> = (0..frame.rgb.len()).map(|i| frame.pins[i] || (i == active && frame.was.is_some())).collect();
        match &frame.was {
            Some(was) => {
                offer(model.choose(&Ask { task: Task::Follow, before: Some(was), count: many, ..ask }), "follow", "carry the change through", Scope::Palette);
                offer(model.choose(&Ask { task: Task::Repair, before: Some(was), pins: &held, active: None, recent: &[active], count: many, ..ask }), "tidy", "tidy the rest", Scope::Palette);
            }
            None => offer(model.choose(&Ask { task: Task::Repair, pins: &held, active: None, count: many, ..ask }), "tidy", "tidy the palette", Scope::Palette),
        }
        out
    }
}

impl Learned {
    /// The colours the model would give stop `at` of `stops` if it had none, best first: a masked
    /// request with no edit budget, reading the other stops as they are. `library` sets the style,
    /// as [`Scene::library`] does. Empty for a palette of one, a stop out of range, or a position
    /// that is not a number or out of order; stops that share a spot are read a hair apart.
    #[must_use]
    pub fn fill(&self, stops: &[Stop], at: usize, library: &[Vec<Rgb>]) -> Vec<Rgb> {
        let n = stops.len();
        let Some(positions) = super::positions(stops).filter(|_| n >= 2 && at < n) else {
            return Vec::new();
        };
        let unit = |c: Rgb| [f64::from(c.r) / 255.0, f64::from(c.g) / 255.0, f64::from(c.b) / 255.0];
        let byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        let rgb: Vec<Srgb> = stops.iter().map(|s| unit(s.col)).collect();
        let support: Vec<Vec<Lab>> = library.iter().map(|palette| palette.iter().map(|c| oklab(unit(*c))).collect()).collect();
        let style = self.style_vector(&support);
        // as in a one-stop request: the palette's ends hold, unless one is the stop asked about
        let pins: Vec<bool> = (0..n).map(|i| i != at && (i == 0 || i == n - 1)).collect();
        let here: Vec<bool> = (0..n).map(|i| i == at).collect();
        let ask = Ask {
            task: Task::Masked, rgb: &rgb, before: None, positions: &positions, pins: &pins, scope: &here,
            active: Some(at), recent: &[], radius: None, count: MODES, intent: None, style: &style,
        };
        self.choose(&ask).options.iter().map(|option| {
            let c = option.rgb[at];
            Rgb::new(byte(c[0]), byte(c[1]), byte(c[2]))
        }).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assist::propose;
    use crate::spectrum::Interp;

    #[test]
    fn asked_about_a_palette_at_rest_it_answers_and_pages_through_its_options() {
        let model = Learned::parse(include_bytes!("../../../data/colour-model.golden.lumacore")).unwrap();
        let stops: Vec<Stop> = [(30, 40, 90), (60, 110, 180), (120, 170, 220), (200, 225, 245)].into_iter().enumerate()
            .map(|(i, (r, g, b))| Stop::new(Rgb::new(r, g, b), i as f32 / 3.0)).collect();
        let scene = Scene { stops: stops.clone(), interp: Interp::Oklab, before: None, selected: 1, pins: Vec::new(), open: false, library: Vec::new() };
        assert!(model.offers(&scene).iter().all(|offer| offer.id == "doubt"), "unasked, it only voices doubts");
        let first = propose(&Asked { model: &model, round: 0 }, &scene);
        assert!(!first.offers.is_empty(), "asked, it always has something");
        assert!(first.offers.iter().any(|offer| offer.scope == Scope::Stop), "including a colour for the selected stop");
        let pages: Vec<_> = (0..3).map(|round| propose(&Asked { model: &model, round }, &scene).offers).collect();
        assert!(pages.windows(2).any(|pair| pair[0] != pair[1]), "asking again shows other options");
        let held = Scene { pins: vec![false, true], ..scene };
        assert!(propose(&Asked { model: &model, round: 0 }, &held).offers.iter().all(|offer| offer.stops[1].col == stops[1].col));
        assert_eq!(model.fill(&stops, 2, &[]).len().min(1), 1, "a stop can always be filled");
    }
}
