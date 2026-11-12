// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The second colour picker and its suggestion row: the State surface over [`neuron::assist`].
//!
//! The picker addresses a colour as lightness, vividness (a share of the most chroma the display
//! shows at that lightness and hue) and hue, so moving one leaves the other two where they were.
//! The row shows the engine's offers for the palette being edited. Hovering an offer shows it on
//! the strip and the board and leaving puts the palette back; only a click keeps it, and the row
//! then offers the way back.

use super::*;
use crate::ui::AssistOffer;
use neuron::assist::{self, Lch, Offer, Scope};
use neuron::spectrum::{Interp, Motion, Palette, Stop};

/// (layer, frame): which palette a remembered state belongs to.
type Home = (usize, usize);

#[derive(Default)]
struct Sense {
    /// The palette before the edit in progress.
    base: Option<(Home, Vec<Stop>)>,
    /// What the row shows, and the scene and slate it came from.
    rows: Vec<Offer>,
    slate: Option<(assist::Scene, assist::Slate)>,
    /// The palette to return to while an offer is shown on hover.
    stash: Option<(Vec<Stop>, Interp)>,
    /// After an offer is taken: the palette it replaced and the stops it left, until either changes.
    undo: Option<(Home, Vec<Stop>, Interp, Vec<Stop>)>,
    /// The picker's own coordinates and the colour they made. Kept while the stop still has that
    /// colour, so the cursor neither drifts through 8-bit rounding nor loses its hue at grey.
    pick: Option<(Lch, f32, Rgb)>,
    plane_hue: Option<u32>,
    rail_at: Option<(u32, u32)>,
}

thread_local! {
    static SENSE: RefCell<Sense> = RefCell::default();
}

fn active(sh: &SharedRt) -> Option<(Home, Palette)> {
    let s = sh.borrow();
    let layer = s.light_layers.get(s.selected_layer)?;
    let frame = s.active_frame.min(layer.spectrum.seq.len().saturating_sub(1));
    Some(((s.selected_layer, frame), layer.spectrum.seq.get(frame)?.palette.clone()))
}

fn same_shape(a: &[Stop], b: &[Stop]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.at == y.at)
}

/// Call before stop `idx` is recoloured. Recolouring the same stop again continues one edit;
/// anything else starts a new one from the palette as it is now.
pub(super) fn note_edit(sh: &SharedRt, idx: usize) {
    let Some((home, palette)) = active(sh) else { return };
    SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let continues = sense.base.as_ref().is_some_and(|(at, base)| {
            *at == home
                && same_shape(base, &palette.stops)
                && base.iter().zip(&palette.stops).enumerate().all(|(i, (was, now))| i == idx || was.col == now.col)
        });
        if !continues {
            sense.base = Some((home, palette.stops.clone()));
        }
    });
}

fn selected(app: &AppWindow, stops: usize) -> usize {
    (app.global::<State>().get_light_sel_stop().max(0) as usize).min(stops.saturating_sub(1))
}

/// Re-read the active palette into the row and the picker. Runs after every palette change.
pub(super) fn refresh(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    if !st.get_picker_v2() {
        return;
    }
    let Some((home, palette)) = active(sh) else {
        SENSE.with(|sense| *sense.borrow_mut() = Sense::default());
        show(app, &[], 0);
        return;
    };
    let chosen = selected(app, palette.stops.len());
    // while an offer is on show the row must not change under the pointer
    let showing = SENSE.with(|sense| sense.borrow().stash.is_some());
    if !showing {
        let rows = SENSE.with(|sense| {
            let mut sense = sense.borrow_mut();
            if sense.base.as_ref().is_some_and(|(at, base)| *at != home || !same_shape(base, &palette.stops)) {
                sense.base = None;
            }
            if sense.undo.as_ref().is_some_and(|(at, _, _, left)| *at != home || *left != palette.stops) {
                sense.undo = None;
            }
            let scene = assist::Scene {
                stops: palette.stops.clone(),
                interp: palette.interp,
                before: sense.base.as_ref().map(|(_, base)| base.clone()),
                selected: chosen,
                pins: Vec::new(),
            };
            let slate = assist::propose(&assist::Rules, &scene);
            let mut rows = slate.offers.clone();
            if let Some((_, stops, interp, _)) = &sense.undo {
                rows.push(Offer { id: "undo", title: "put it back", scope: Scope::Palette, stops: stops.clone(), interp: *interp });
            }
            sense.slate = Some((scene, slate));
            sense.rows = rows.clone();
            rows
        });
        show(app, &rows, chosen);
    }
    sync_picker(app, &palette, chosen);
}

fn show(app: &AppWindow, rows: &[Offer], chosen: usize) {
    let st = app.global::<State>();
    let rows: Vec<AssistOffer> = rows.iter().map(|offer| AssistOffer {
        title: offer.title.into(),
        solid: offer.scope == Scope::Stop,
        col: offer.stops.get(chosen).map(|s| rgb_to_color(s.col)).unwrap_or_default(),
        swatch: palette_strip_image(&Palette { stops: offer.stops.clone(), motion: Motion::Hold, interp: offer.interp }),
    }).collect();
    update_value_model(st.get_assist_offers(), rows, |model| st.set_assist_offers(model));
}

fn put(app: &AppWindow, sh: &SharedRt, stops: Vec<Stop>, interp: Interp) {
    edit_active_palette(app, sh, |palette| {
        palette.stops = stops;
        palette.interp = interp;
    });
}

/// Show row `index` on the live palette, or put the palette back for a negative index.
fn preview(app: &AppWindow, sh: &SharedRt, index: i32) {
    let Some((_, palette)) = active(sh) else { return };
    let step = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        match usize::try_from(index).ok().and_then(|i| sense.rows.get(i).cloned()) {
            Some(offer) => {
                sense.stash.get_or_insert((palette.stops.clone(), palette.interp));
                Some((offer.stops, offer.interp))
            }
            None => sense.stash.take(),
        }
    });
    if let Some((stops, interp)) = step {
        put(app, sh, stops, interp);
    }
}

/// Keep row `index`. The palette it replaces becomes the row's way back.
fn accept(app: &AppWindow, sh: &SharedRt, index: i32) {
    let Some((home, palette)) = active(sh) else { return };
    let taken = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let index = usize::try_from(index).ok()?;
        let offer = sense.rows.get(index).cloned()?;
        let (was_stops, was_interp) = sense.stash.take().unwrap_or((palette.stops.clone(), palette.interp));
        // an engine offer is only good for the scene it was made for; the way back needs no slate
        let current = offer.id == "undo"
            || sense.slate.as_ref().is_some_and(|(scene, slate)| {
                scene.stops == was_stops && scene.interp == was_interp && slate.take(scene, index).is_some()
            });
        if !current {
            return Some((was_stops, was_interp));
        }
        sense.undo = (offer.id != "undo").then(|| (home, was_stops, was_interp, offer.stops.clone()));
        sense.base = None;
        Some((offer.stops, offer.interp))
    });
    if let Some((stops, interp)) = taken {
        put(app, sh, stops, interp);
    }
}

const PLANE: (u32, u32) = (128, 64);
const RAIL: u32 = 180;

fn image(width: u32, height: u32, at: impl Fn(u32, u32) -> Rgb) -> Image {
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let c = at(i as u32 % width, i as u32 / width);
        *px = Rgba8Pixel { r: c.r, g: c.g, b: c.b, a: 255 };
    }
    Image::from_rgba8(buf)
}

/// Point the picker at the selected stop and redraw whichever of its two surfaces changed.
fn sync_picker(app: &AppWindow, palette: &Palette, chosen: usize) {
    let st = app.global::<State>();
    let Some(stop) = palette.stops.get(chosen) else { return };
    let (lch, vivid) = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        if let Some((lch, vivid, made)) = sense.pick {
            if made == stop.col {
                return (lch, vivid);
            }
        }
        let mut lch = Lch::of(stop.col);
        if lch.c < 0.01 {
            // a grey has no hue of its own: stay on the hue the picker was showing
            lch.h = sense.pick.map_or(lch.h, |(last, _, _)| last.h);
        }
        let room = Lch::max_chroma(lch.l, lch.h);
        let vivid = if room < 1e-3 { 0.0 } else { (lch.c / room).min(1.0) };
        sense.pick = Some((lch, vivid, stop.col));
        (lch, vivid)
    });
    st.set_picker_hue(lch.h);
    st.set_picker_vivid(vivid);
    st.set_picker_light(lch.l.clamp(0.0, 1.0));

    let hue_key = (lch.h * 1440.0).round() as u32;
    let rail_key = ((lch.l * 255.0).round() as u32, (vivid * 255.0).round() as u32);
    let (plane_stale, rail_stale) = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let stale = (sense.plane_hue != Some(hue_key), sense.rail_at != Some(rail_key));
        sense.plane_hue = Some(hue_key);
        sense.rail_at = Some(rail_key);
        stale
    });
    if plane_stale {
        let rooms: Vec<f32> = (0..PLANE.1)
            .map(|y| Lch::max_chroma(1.0 - y as f32 / (PLANE.1 - 1) as f32, lch.h))
            .collect();
        st.set_picker_plane(image(PLANE.0, PLANE.1, |x, y| {
            let l = 1.0 - y as f32 / (PLANE.1 - 1) as f32;
            Lch { l, c: rooms[y as usize] * x as f32 / (PLANE.0 - 1) as f32, h: lch.h }.rgb()
        }));
    }
    if rail_stale {
        st.set_picker_rail(image(RAIL, 1, |x, _| {
            let h = x as f32 / RAIL as f32;
            Lch { l: lch.l, c: Lch::max_chroma(lch.l, h) * vivid, h }.rgb()
        }));
    }
    // the other stops' hues, so the rail shows where the rest of the palette sits
    let marks: Vec<f32> = palette.stops.iter().enumerate()
        .filter(|(i, _)| *i != chosen)
        .map(|(_, s)| Lch::of(s.col))
        .filter(|other| other.c >= 0.03)
        .map(|other| other.h)
        .collect();
    update_value_model(st.get_picker_marks(), marks, |model| st.set_picker_marks(model));
}

/// Recolour the selected stop from picker coordinates.
fn pick(app: &AppWindow, sh: &SharedRt, lch_at: impl FnOnce(Lch, f32) -> (Lch, f32)) {
    let Some((_, palette)) = active(sh) else { return };
    let chosen = selected(app, palette.stops.len());
    let Some((from, vivid)) = SENSE.with(|sense| sense.borrow().pick.map(|(lch, vivid, _)| (lch, vivid))) else { return };
    let (mut lch, vivid) = lch_at(from, vivid);
    lch.c = Lch::max_chroma(lch.l, lch.h) * vivid;
    let made = lch.rgb();
    SENSE.with(|sense| sense.borrow_mut().pick = Some((lch, vivid, made)));
    note_edit(sh, chosen);
    edit_active_palette(app, sh, |palette| palette.set_stop_color(chosen, made));
}

pub(super) fn install(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    st.set_picker_v2(crate::prefs::picker_v2());
    let bind = |f: fn(&AppWindow, &SharedRt, f32, f32)| {
        let (w, sh) = (app.as_weak(), sh.clone());
        move |a: f32, b: f32| {
            if let Some(app) = w.upgrade() {
                f(&app, &sh, a, b);
            }
        }
    };
    st.on_picker_plane_pick(bind(|app, sh, vivid, light| {
        pick(app, sh, |from, _| (Lch { l: light.clamp(0.0, 1.0), ..from }, vivid.clamp(0.0, 1.0)));
    }));
    let hue = bind(|app, sh, hue, _| pick(app, sh, |from, vivid| (Lch { h: hue.clamp(0.0, 1.0) % 1.0, ..from }, vivid)));
    st.on_picker_hue_pick(move |h| hue(h, 0.0));
    let offer = |f: fn(&AppWindow, &SharedRt, i32)| {
        let (w, sh) = (app.as_weak(), sh.clone());
        move |index: i32| {
            if let Some(app) = w.upgrade() {
                f(&app, &sh, index);
            }
        }
    };
    st.on_assist_preview(offer(preview));
    st.on_assist_accept(offer(accept));
    let sync = offer(|app, sh, _| refresh(app, sh));
    st.on_picker_sync(move || sync(0));
    let (w, sh) = (app.as_weak(), sh.clone());
    st.on_set_picker_v2(move |on| {
        if let Some(app) = w.upgrade() {
            crate::prefs::set_picker_v2(on);
            // leave nothing half-shown behind when switching away
            preview(&app, &sh, -1);
            SENSE.with(|sense| *sense.borrow_mut() = Sense::default());
            show(&app, &[], 0);
            app.global::<State>().set_picker_v2(on);
            refresh(&app, &sh);
        }
    });
}
