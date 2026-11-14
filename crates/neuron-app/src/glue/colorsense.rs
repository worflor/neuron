// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The second colour picker and its suggestion row: the State surface over [`neuron::assist`].
//!
//! The picker addresses a colour as lightness, vividness (a share of the most chroma the display
//! shows at that lightness and hue) and hue, so moving one leaves the other two where they were.
//! Offers for the palette being edited come from the built-in rules and, when the run root holds
//! a bundle under `models/`, a learned colour model. A whole-palette offer is a capsule in the row; a
//! colour for the selected stop is a dot in the picker, where that colour would be picked by hand.
//! Hovering either (or stepping to it with Tab) shows it on the strip and the board and leaving
//! puts the palette back; only a click or Enter keeps it, and the row then offers the way back.
//!
//! The rules answer at once. The model is asked on its own thread, once a drag has settled, and
//! its answer joins the row when it arrives if the palette is still the one it was asked about.
//! A stop just added is offered colours before anyone has picked one; a pinned stop is left alone
//! by every offer. The model also says where it has doubts: a stop it would move is marked on the
//! strip, and its colour for that stop is a dot once the stop is selected. Behind the plane it
//! draws its terrain for the selected stop like a relief map: rings around the colours it would
//! put at that place in the palette, and on the rail a skyline that stands tallest at the hues
//! where that ground is highest. A doubted stop shows the colour the model would rather see. The terrain reads the rest of the palette, not the stop's own
//! colour, so it holds still while the stop is dragged. With `picker_journal` set in the preferences, picks and what was offered are
//! appended to `models/picker-sessions.jsonl` in the run root ([`neuron::assist::journal`]).

use super::*;
use crate::ui::{AssistOffer, StopDoubt};
use neuron::assist::journal::{Control, Journal};
use neuron::assist::learned::{Asked, Learned, Terrain};
use neuron::assist::roll::{self, Mode, Wild};
use neuron::assist::{self, Engine, Lch, Offer, Scope};
use neuron::spectrum::{Interp, Motion, Palette, Stop};
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};

/// (layer, frame): which palette a remembered state belongs to.
type Home = (usize, usize);

/// One thing on offer: a capsule in the row, or a dot in the picker.
#[derive(Clone)]
struct Row {
    offer: Offer,
    /// Its place in the slate; `None` for the way back and for a roll, which need no slate.
    slate: Option<usize>,
    /// Where a single-colour offer sits on the picker plane: (vividness, lightness).
    dot: Option<(f32, f32)>,
    /// A palette the roll button made.
    roll: bool,
}

#[derive(Default)]
struct Sense {
    /// The palette before the edit in progress.
    base: Option<(Home, Vec<Stop>)>,
    /// What is on offer (capsules first, then dots), and the scene and slate it came from.
    rows: Vec<Row>,
    slate: Option<(assist::Scene, assist::Slate)>,
    /// The model's answer for a scene, held until nothing is on show.
    pending: Option<(assist::Scene, assist::Slate, Option<Terrain>)>,
    /// The model's terrain for the selected stop, what it was made from ([`ground_key`]), and the
    /// hue it was last drawn at.
    terrain: Option<Terrain>,
    terrain_of: u64,
    terrain_at: Option<u32>,
    /// The palette to return to while an offer is shown on hover.
    stash: Option<(Vec<Stop>, Interp)>,
    /// After an offer is taken: the palette it replaced and the stops it left, until either changes.
    undo: Option<(Home, Vec<Stop>, Interp, Vec<Stop>)>,
    /// A stop just added whose colour nobody has chosen, and the palette it was added to.
    open: Option<(Home, usize, Vec<Stop>)>,
    /// Stops no offer may change.
    pins: Option<(Home, Vec<bool>)>,
    /// The picker's own coordinates and the colour they made. Kept while the stop still has that
    /// colour, so the cursor neither drifts through 8-bit rounding nor loses its hue at grey.
    pick: Option<(Lch, f32, Rgb)>,
    plane_hue: Option<u32>,
    rail_at: Option<(u32, u32)>,
    journal: Option<(Home, Journal)>,
    /// What moved the colour last, the last colour the journal has, and whether the journal's
    /// last slate is still unanswered.
    control: Option<Control>,
    logged: Option<(usize, Rgb)>,
    offered: bool,
    /// When the journal's last slate went on screen.
    offered_at: Option<std::time::Instant>,
    /// The person asked the model outright about this palette: the palette, which round of
    /// options, and the slate that round made for the scene it was made for.
    asking: Option<(Home, Vec<Stop>, usize, Option<(assist::Scene, assist::Slate)>)>,
    /// Palettes rolled for a palette, newest first, and how many rolls there have been.
    rolls: Vec<(Home, Offer)>,
    rolled: u64,
    /// Options asked for that the person waved off, for the palette they were asked about.
    waved: Option<(Home, Vec<Stop>, Vec<Vec<Stop>>)>,
}

/// How long a slate must stay on screen before replacing it counts as passing it over. A slate
/// swapped out sooner (the rules' answer replaced by the model's a moment later) was never seen.
const SEEN: std::time::Duration = std::time::Duration::from_millis(800);

/// The most capsules the suggestion row shows: what fits the inspector column beside the ask
/// capsule and a caption.
const PILLS: usize = 5;

/// The most rolls the row keeps.
const ROLLS: usize = 3;

/// The most palettes, and colours of each, handed to an engine as the person's other work.
const LIBRARY: (usize, usize) = (8, 12);
/// How long a palette must hold still before the model is asked about it.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(90);

thread_local! {
    static SENSE: RefCell<Sense> = RefCell::default();
    /// The learned model in the run root, read on first use.
    static MODEL: RefCell<Option<Option<Arc<Learned>>>> = const { RefCell::new(None) };
    /// The window and state a model answer is delivered to.
    static HOME: RefCell<Option<(slint::Weak<AppWindow>, SharedRt)>> = const { RefCell::new(None) };
    static JOURNALING: Cell<bool> = const { Cell::new(false) };
    /// A decision went into the journal: the taste is refitted once the row is drawn.
    static LEARN: Cell<bool> = const { Cell::new(false) };
}

fn model() -> Option<Arc<Learned>> {
    MODEL.with(|model| {
        model.borrow_mut().get_or_insert_with(|| {
            // a test opts into a model with `use_model`; the shipped one is too slow unoptimised
            if cfg!(test) {
                return None;
            }
            // a bundle in the run root's models folder, else Luma-T as Neuron ships it
            let model = Learned::open(&neuron::runroot::run_root().join("models"));
            for note in model.notes() {
                eprintln!("neuron: colour model: {note}");
            }
            Some(Arc::new(model))
        }).clone()
    })
}

/// Use Luma-T as Neuron ships it on this thread (the README shots show the real model).
#[cfg(test)]
pub(crate) fn use_builtin_model() {
    MODEL.with(|slot| *slot.borrow_mut() = Some(Some(Arc::new(Learned::builtin()))));
}

/// Use `bundle` as the colour model on this thread, or none.
#[cfg(test)]
pub(crate) fn use_model(bundle: Option<&[u8]>) {
    let model = bundle.map(|bytes| Arc::new(Learned::parse(bytes).expect("a readable bundle")));
    MODEL.with(|slot| *slot.borrow_mut() = Some(model));
}

/// The newest scene waiting for the model. A scene still waiting when another arrives is never
/// worked on: during a drag only the palette it ends on is worth an answer.
static DESK: (Mutex<Option<(Arc<Learned>, assist::Scene)>>, Condvar) = (Mutex::new(None), Condvar::new());

/// Everything the model and the rules have to say about `scene`.
fn both(model: &Learned, scene: &assist::Scene) -> (assist::Slate, Option<Terrain>) {
    (assist::propose(&assist::Chain(&engines(model)), scene), model.terrain(scene))
}

/// The model, and the fixed rules unless the model's silence is a judgement of its own (a taste
/// head that chose to leave the palette alone) that the rules would only talk over.
fn engines(model: &Learned) -> Vec<&dyn Engine> {
    if model.answers_alone() { vec![model] } else { vec![model, &assist::Rules] }
}

/// Ask the model about `scene` on its own thread. The answer comes back through [`deliver`].
fn ask(model: Arc<Learned>, scene: assist::Scene) {
    static WORKER: Once = Once::new();
    WORKER.call_once(|| {
        let work = || loop {
            let mut next = DESK.0.lock().unwrap_or_else(PoisonError::into_inner);
            while next.is_none() {
                next = DESK.1.wait(next).unwrap_or_else(PoisonError::into_inner);
            }
            drop(next);
            std::thread::sleep(SETTLE);
            let Some((model, scene)) = DESK.0.lock().unwrap_or_else(PoisonError::into_inner).take() else { continue };
            let (slate, terrain) = both(&model, &scene);
            if DESK.0.lock().unwrap_or_else(PoisonError::into_inner).is_none() {
                let _ = slint::invoke_from_event_loop(move || deliver(scene, slate, terrain));
            }
        };
        if !crate::worker::spawn_detached("neuron-colour-model", work) {
            eprintln!("neuron: colour model thread not started");
        }
    });
    *DESK.0.lock().unwrap_or_else(PoisonError::into_inner) = Some((model, scene));
    DESK.1.notify_one();
}

/// The model's answer for `scene` has arrived. It is used if the palette is still that scene.
fn deliver(scene: assist::Scene, slate: assist::Slate, terrain: Option<Terrain>) {
    SENSE.with(|sense| sense.borrow_mut().pending = Some((scene, slate, terrain)));
    let home = HOME.with(|home| home.borrow().as_ref().and_then(|(app, sh)| Some((app.upgrade()?, sh.clone()))));
    if let Some((app, sh)) = home {
        refresh(&app, &sh);
    }
}

/// A value that changes when anything a terrain reads changes: everything in the scene except the
/// selected stop's own colour and the edit history.
fn ground_key(scene: &assist::Scene) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (i, stop) in scene.stops.iter().enumerate() {
        let col = if i == scene.selected { Rgb::default() } else { stop.col };
        (col.r, col.g, col.b, stop.at.to_bits()).hash(&mut h);
    }
    for c in scene.library.iter().flatten() {
        (c.r, c.g, c.b).hash(&mut h);
    }
    (scene.selected, &scene.pins, scene.library.len()).hash(&mut h);
    h.finish()
}

/// How much of the chroma the display shows at a colour's lightness and hue it uses.
fn vividness(lch: Lch) -> f32 {
    let room = Lch::max_chroma(lch.l, lch.h);
    if room < 1e-3 { 0.0 } else { (lch.c / room).min(1.0) }
}

fn active(sh: &SharedRt) -> Option<(Home, Palette)> {
    let s = sh.borrow();
    let layer = s.light_layers.get(s.selected_layer)?;
    let frame = s.active_frame.min(layer.spectrum.seq.len().saturating_sub(1));
    Some(((s.selected_layer, frame), layer.spectrum.seq.get(frame)?.palette.clone()))
}

/// The person's own palettes: the rest of the lighting stack first, then their saved effects.
fn library(sh: &SharedRt, home: Home) -> Vec<Vec<Rgb>> {
    let s = sh.borrow();
    let mut all: Vec<Vec<Rgb>> = Vec::new();
    let mut add = |colours: Vec<Rgb>| {
        if !colours.is_empty() && !all.contains(&colours) && all.len() < LIBRARY.0 {
            all.push(colours);
        }
    };
    for (l, layer) in s.light_layers.iter().enumerate() {
        for (f, frame) in layer.spectrum.seq.iter().enumerate() {
            if (l, f) != home {
                add(frame.palette.stops.iter().take(LIBRARY.1).map(|stop| stop.col).collect());
            }
        }
    }
    for colours in saved_palettes() {
        add(colours);
    }
    all
}

/// The palettes in the person's saved effects, read again only when the effects folder changes.
fn saved_palettes() -> Vec<Vec<Rgb>> {
    thread_local! {
        static SAVED: RefCell<Option<(Option<std::time::SystemTime>, Vec<Vec<Rgb>>)>> = const { RefCell::new(None) };
    }
    let dir = neuron::user_effects::effects_dir();
    let stamp = std::fs::metadata(&dir).and_then(|m| m.modified()).ok();
    SAVED.with(|saved| {
        let mut saved = saved.borrow_mut();
        if saved.as_ref().is_none_or(|(at, _)| *at != stamp) {
            let palettes = neuron::user_effects::list().unwrap_or_default().into_iter()
                .filter_map(|(slug, _, _)| neuron::user_effects::load(&slug).ok())
                .flat_map(|effect| effect.layers)
                .flat_map(|layer| layer.spectrum.seq.into_iter().map(|frame| frame.palette.stops.iter().take(LIBRARY.1).map(|stop| stop.col).collect::<Vec<_>>()))
                .collect();
            *saved = Some((stamp, palettes));
        }
        saved.as_ref().map(|(_, palettes)| palettes.clone()).unwrap_or_default()
    })
}

/// Fit the person's taste again from the journal, so what they just took or passed over shapes
/// the next offers instead of the next launch's.
fn learn_taste() {
    if let Some(model) = model() {
        if let Err(e) = model.relearn(&neuron::runroot::run_root().join("models")) {
            eprintln!("neuron: colour taste not refitted ({e})");
        }
    }
}

fn same_shape(a: &[Stop], b: &[Stop]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.at == y.at)
}

/// Put the palette back if an offer is on show: an edit is made to the palette as it is, never
/// to one that is only being looked at.
fn settle(app: &AppWindow, sh: &SharedRt) {
    if SENSE.with(|sense| sense.borrow().stash.is_some()) {
        preview(app, sh, -1);
    }
}

/// Call before stop `idx` is recoloured. Recolouring the same stop again continues one edit;
/// anything else starts a new one from the palette as it is now.
pub(super) fn note_edit(app: &AppWindow, sh: &SharedRt, idx: usize) {
    settle(app, sh);
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

/// Call after stop `idx` was added and selected: until someone colours it, what it shows is a
/// placeholder and colours for it may be offered.
pub(super) fn note_added(app: &AppWindow, sh: &SharedRt, idx: usize) {
    let Some((home, palette)) = active(sh) else { return };
    SENSE.with(|sense| sense.borrow_mut().open = Some((home, idx, palette.stops)));
    refresh(app, sh);
}

fn selected(app: &AppWindow, stops: usize) -> usize {
    (app.global::<State>().get_light_sel_stop().max(0) as usize).min(stops.saturating_sub(1))
}

/// Make sure a journal session for `home` is open, when the preferences ask for one. A journal
/// that cannot be written is given up on for the rest of the run.
fn journaling(sense: &mut Sense, home: Home, palette: &[Stop]) -> bool {
    if !JOURNALING.get() {
        return false;
    }
    if sense.journal.as_ref().is_none_or(|(at, _)| *at != home) {
        let folder = neuron::runroot::run_root().join("models");
        let begun = std::fs::create_dir_all(&folder).and_then(|()| Journal::start(&folder.join("picker-sessions.jsonl"), palette));
        (sense.logged, sense.offered) = (None, false);
        sense.journal = written(begun).map(|journal| (home, journal));
    }
    sense.journal.is_some()
}

fn written<T>(result: std::io::Result<T>) -> Option<T> {
    result.map_err(|e| {
        eprintln!("neuron: picker journal not written ({e})");
        JOURNALING.set(false);
    }).ok()
}

/// Re-read the active palette into the row and the picker. Runs after every palette change.
pub(super) fn refresh(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    if !st.get_picker_v2() {
        return;
    }
    let Some((home, palette)) = active(sh) else {
        SENSE.with(|sense| *sense.borrow_mut() = Sense::default());
        show(app, &[], 0, &[], &[]);
        return;
    };
    let chosen = selected(app, palette.stops.len());
    // while an offer is on show the row must not change under the pointer
    let showing = SENSE.with(|sense| sense.borrow().stash.is_some());
    if !showing {
        let others = library(sh, home);
        let (rows, pins, doubts) = SENSE.with(|sense| {
            let mut sense = sense.borrow_mut();
            let sense = &mut *sense;
            if sense.base.as_ref().is_some_and(|(at, base)| *at != home || !same_shape(base, &palette.stops)) {
                sense.base = None;
            }
            if sense.undo.as_ref().is_some_and(|(at, _, _, left)| *at != home || *left != palette.stops) {
                sense.undo = None;
            }
            if sense.open.as_ref().is_some_and(|(at, idx, then)| *at != home || *idx != chosen || *then != palette.stops) {
                sense.open = None;
            }
            if sense.pins.as_ref().is_some_and(|(at, pins)| *at != home || pins.len() != palette.stops.len()) {
                sense.pins = None;
            }
            let pins = sense.pins.as_ref().map(|(_, pins)| pins.clone()).unwrap_or_default();
            let open = sense.open.is_some();
            let scene = assist::Scene {
                stops: palette.stops.clone(),
                interp: palette.interp,
                before: sense.base.as_ref().filter(|_| !open).map(|(_, base)| base.clone()),
                selected: chosen,
                pins: pins.clone(),
                open,
                library: others,
            };
            // The engines are only asked again when something they read has changed. The rules
            // answer here; the model's answer for the same scene replaces theirs when it arrives.
            let arrived = sense.pending.take().filter(|(asked, _, _)| *asked == scene);
            let held = sense.slate.take().filter(|(asked, _)| *asked == scene);
            // `Some` replaces the terrain; a slate that was already here keeps the one it came with
            let (slate, terrain) = match (arrived, held) {
                (Some((_, slate, terrain)), _) => (slate, Some(terrain)),
                (None, Some((_, slate))) => (slate, None),
                (None, None) => match model() {
                    Some(model) if cfg!(test) => {
                        let (slate, terrain) = both(&model, &scene);
                        (slate, Some(terrain))
                    }
                    Some(model) => {
                        // until the model answers, the rules do, unless it answers alone
                        let interim = if model.answers_alone() { assist::propose(&assist::Chain(&[]), &scene) } else { assist::propose(&assist::Rules, &scene) };
                        ask(model, scene.clone());
                        (interim, Some(None))
                    }
                    None => (assist::propose(&assist::Rules, &scene), Some(None)),
                },
            };
            // an answer on its way does not blank a terrain made from the same surroundings
            let ground = ground_key(&scene);
            match terrain {
                Some(None) if ground == sense.terrain_of && model().is_some() => {}
                Some(terrain) => (sense.terrain, sense.terrain_at) = (terrain, None),
                None => {}
            }
            sense.terrain_of = ground;
            if sense.asking.as_ref().is_some_and(|(at, then, _, _)| *at != home || *then != palette.stops) {
                sense.asking = None;
            }
            // asked outright, the model lists what it would otherwise keep to itself
            let slate = match (sense.asking.as_mut(), model()) {
                (Some((_, _, round, made)), Some(model)) => {
                    if made.as_ref().is_none_or(|(asked, _)| *asked != scene) {
                        let asked = Asked { model: &model, round: *round };
                        let mut all: Vec<&dyn Engine> = vec![&asked];
                        all.extend(engines(&model));
                        *made = Some((scene.clone(), assist::propose(&assist::Chain(&all), &scene)));
                    }
                    made.as_ref().map_or(slate, |(_, asked)| asked.clone())
                }
                _ => slate,
            };
            // a doubt about another stop is a mark on that stop, not something to pick here
            let mut doubts: Vec<Option<Rgb>> = vec![None; palette.stops.len()];
            for offer in slate.offers.iter().filter(|offer| offer.scope == Scope::Aside) {
                if let Some(stop) = offer.stops.iter().zip(&palette.stops).position(|(new, old)| new.col != old.col) {
                    doubts[stop].get_or_insert(offer.stops[stop].col);
                }
            }
            let offered = |scope: Scope| slate.offers.iter().enumerate().filter(move |(_, offer)| offer.scope == scope);
            // rolls lead the row, newest first; then the engines' palettes, the way back, the dots
            // a roll that would move a stop held since it was made is no longer one to offer
            sense.rolls.retain(|(at, offer)| {
                *at == home
                    && (offer.stops.len() != palette.stops.len()
                        || offer.stops.iter().zip(&palette.stops).enumerate().all(|(i, (new, old))| !pins.get(i).copied().unwrap_or(false) || new.col == old.col))
            });
            // the row holds PILLS capsules beside the ask capsule; older rolls give way to the
            // engines' palettes and the way back, the newest roll is always shown
            let palettes = offered(Scope::Palette).count() + usize::from(sense.undo.is_some());
            let room = PILLS.saturating_sub(palettes).max(1);
            let mut rows: Vec<Row> = sense.rolls.iter().take(room).map(|(_, offer)| Row { offer: offer.clone(), slate: None, dot: None, roll: true }).collect();
            rows.extend(offered(Scope::Palette).map(|(i, offer)| Row { offer: offer.clone(), slate: Some(i), dot: None, roll: false }));
            if let Some((_, stops, interp, _)) = &sense.undo {
                let offer = Offer { id: "undo", title: "put it back", scope: Scope::Palette, stops: stops.clone(), interp: *interp, reach: false };
                rows.push(Row { offer, slate: None, dot: None, roll: false });
            }
            rows.extend(offered(Scope::Stop).filter_map(|(i, offer)| {
                let lch = Lch::of(offer.stops.get(chosen)?.col);
                Some(Row { offer: offer.clone(), slate: Some(i), dot: Some((vividness(lch), lch.l.clamp(0.0, 1.0))), roll: false })
            }));
            if sense.waved.as_ref().is_some_and(|(at, then, _)| *at != home || *then != palette.stops) {
                sense.waved = None;
            }
            if let Some((_, _, waved)) = &sense.waved {
                rows.retain(|row| !row.offer.reach || !waved.contains(&row.offer.stops));
            }
            st.set_assist_asking(sense.asking.is_some());
            record(sense, home, &palette, chosen, &rows);
            sense.slate = Some((scene, slate));
            sense.rows = rows.clone();
            (rows, pins, doubts)
        });
        st.set_assist_can_ask(model().is_some());
        show(app, &rows, chosen, &pins, &doubts);
        if LEARN.with(|learn| learn.replace(false)) {
            learn_taste();
        }
    }
    sync_picker(app, &palette, chosen);
}

/// Write what just happened to the journal, when one is kept: the colour the edit in progress
/// has reached, and the offers when they change.
fn record(sense: &mut Sense, home: Home, palette: &Palette, chosen: usize, rows: &[Row]) {
    if !journaling(sense, home, &palette.stops) {
        return;
    }
    let moved = sense.base.as_ref().is_some_and(|(_, base)| base.get(chosen).map(|s| s.col) != palette.stops.get(chosen).map(|s| s.col));
    let now = palette.stops.get(chosen).map(|stop| (chosen, stop.col));
    let control = sense.control.take().unwrap_or(Control::Other);
    let changed = rows.len() != sense.rows.len() || rows.iter().zip(&sense.rows).any(|(a, b)| a.offer != b.offer);
    let Some((_, journal)) = sense.journal.as_mut() else { return };
    let mut wrote = Ok(());
    if let Some((idx, col)) = now.filter(|now| moved && Some(*now) != sense.logged) {
        wrote = wrote.and(journal.drag(idx, col, control));
        sense.logged = now;
    }
    if changed {
        let slate: Vec<&[Stop]> = rows.iter().filter(|row| row.slate.is_some()).map(|row| row.offer.stops.as_slice()).collect();
        let passed = sense.offered && sense.offered_at.is_some_and(|at| at.elapsed() >= SEEN);
        if passed {
            wrote = wrote.and(journal.dismissed());
        }
        wrote = wrote.and(journal.shown(&slate));
        sense.offered = !slate.is_empty();
        sense.offered_at = sense.offered.then(std::time::Instant::now);
        if passed {
            // after this borrow of the sense ends
            LEARN.with(|learn| learn.set(true));
        }
    }
    written(wrote);
}

fn show(app: &AppWindow, rows: &[Row], chosen: usize, pins: &[bool], doubts: &[Option<Rgb>]) {
    let st = app.global::<State>();
    let marks: Vec<StopDoubt> = doubts.iter().map(|doubt| StopDoubt { on: doubt.is_some(), col: rgb_to_color(doubt.unwrap_or_default()) }).collect();
    update_value_model(st.get_light_doubts(), marks, |model| st.set_light_doubts(model));
    st.set_assist_ramps(rows.iter().filter(|row| row.dot.is_none()).count() as i32);
    st.set_assist_keyed(-1);
    // the row is only rebuilt with nothing on show
    st.set_assist_showing(false);
    st.set_light_sel_pinned(pins.get(chosen).copied().unwrap_or(false));
    update_value_model(st.get_light_pins(), pins.to_vec(), |model| st.set_light_pins(model));
    // the options are numbered by the engines' own ranking, the way back aside: option 1 is
    // what they would offer first, whichever part of the picker it sits in
    let ranked = ranking(rows);
    let rows: Vec<AssistOffer> = rows.iter().enumerate().map(|(i, Row { offer, dot, slate, roll })| {
        let num = ranked.iter().position(|&row| row == i).map_or(0, |rank| rank as i32 + 1);
        match *dot {
            Some((vivid, light)) => {
                let col = offer.stops.get(chosen).map(|s| s.col).unwrap_or_default();
                AssistOffer { title: offer.title.into(), solid: true, num, reach: offer.reach, col: rgb_to_color(col), vivid, light, hue: Lch::of(col).h, ..AssistOffer::default() }
            }
            None => AssistOffer {
                title: offer.title.into(),
                back: slate.is_none() && !roll,
                rolled: *roll,
                reach: offer.reach,
                num,
                swatch: palette_strip_image(&Palette { stops: offer.stops.clone(), motion: Motion::Hold, interp: offer.interp }),
                ..AssistOffer::default()
            },
        }
    }).collect();
    st.set_assist_prime(rows.iter().position(|row| row.num == 1).map_or(-1, |i| i as i32));
    update_value_model(st.get_assist_offers(), rows, |model| st.set_assist_offers(model));
}

/// The dot at plane point (`x`, `y`) of a plane `w` by `h` pixels: the nearest within reach.
fn dot_at(x: f32, y: f32, w: f32, h: f32) -> i32 {
    const REACH: f32 = 9.0;
    SENSE.with(|sense| {
        sense.borrow().rows.iter().enumerate()
            .filter_map(|(i, row)| {
                let (vivid, light) = row.dot?;
                // the dot is drawn held inside the plane, so it is found there too
                let (cx, cy) = ((vivid * w).clamp(5.0, (w - 5.0).max(5.0)), ((1.0 - light) * h).clamp(5.0, (h - 5.0).max(5.0)));
                let far = (cx - x).hypot(cy - y);
                (far <= REACH).then_some((i, far))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(-1, |(i, _)| i as i32)
    })
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
        match usize::try_from(index).ok().and_then(|i| sense.rows.get(i).map(|row| row.offer.clone())) {
            Some(offer) => {
                sense.stash.get_or_insert((palette.stops.clone(), palette.interp));
                Some((offer.stops, offer.interp))
            }
            None => sense.stash.take(),
        }
    });
    match step {
        Some((stops, interp)) => {
            put(app, sh, stops, interp);
            if let Some((stops, interp)) = SENSE.with(|sense| sense.borrow().stash.clone()) {
                let st = app.global::<State>();
                st.set_assist_before(palette_strip_image(&Palette { stops, motion: Motion::Hold, interp }));
                st.set_assist_showing(true);
            }
        }
        // nothing was on show: an answer that arrived meanwhile may now be
        None => refresh(app, sh),
    }
}

/// Take row `index` off the row: a roll is forgotten, and an option the model was asked for is
/// not offered again for this palette. Other rows are the engines' own and stay.
fn dismiss(app: &AppWindow, sh: &SharedRt, index: i32) {
    preview(app, sh, -1);
    let Some((home, palette)) = active(sh) else { return };
    let changed = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let Some(row) = usize::try_from(index).ok().and_then(|i| sense.rows.get(i).cloned()) else { return false };
        if row.roll {
            sense.rolls.retain(|(_, offer)| *offer != row.offer);
            true
        } else if row.offer.reach {
            let waved = sense.waved.get_or_insert_with(|| (home, palette.stops.clone(), Vec::new()));
            waved.2.push(row.offer.stops);
            true
        } else {
            false
        }
    });
    if changed {
        refresh(app, sh);
    }
}

/// Keep row `index`. The palette it replaces becomes the row's way back.
fn accept(app: &AppWindow, sh: &SharedRt, index: i32) {
    let Some((home, palette)) = active(sh) else { return };
    let taken = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let sense = &mut *sense;
        let index = usize::try_from(index).ok()?;
        let Row { offer, slate: place, roll: rolled, .. } = sense.rows.get(index).cloned()?;
        let (was_stops, was_interp) = sense.stash.take().unwrap_or((palette.stops.clone(), palette.interp));
        // an engine offer is only good for the scene it was made for; the way back needs no slate
        let current = match place {
            None => true,
            Some(place) => sense.slate.as_ref().is_some_and(|(scene, slate)| {
                scene.stops == was_stops && scene.interp == was_interp && slate.take(scene, place).is_some()
            }),
        };
        if !current {
            return Some((was_stops, was_interp));
        }
        let choice = sense.rows[..index].iter().filter(|row| row.slate.is_some()).count();
        if journaling(sense, home, &was_stops) {
            let offered = std::mem::take(&mut sense.offered);
            if let Some((_, journal)) = sense.journal.as_mut() {
                LEARN.with(|learn| learn.set(place.is_some() && offered));
                written(match place {
                    Some(_) if offered => journal.accepted(choice),
                    Some(_) => Ok(()),
                    None if rolled => Ok(()),
                    None => journal.undo(&offer.stops),
                });
            }
        }
        sense.undo = (place.is_some() || rolled).then(|| (home, was_stops, was_interp, offer.stops.clone()));
        sense.base = None;
        Some((offer.stops, offer.interp))
    });
    if let Some((stops, interp)) = taken {
        put(app, sh, stops, interp);
    }
    if LEARN.with(|learn| learn.replace(false)) {
        learn_taste();
    }
}

/// The rows that carry an offer, best first: in the order the engines put them on the slate,
/// which is their ranking across every kind of offer.
fn ranking(rows: &[Row]) -> Vec<usize> {
    let mut ranked: Vec<(usize, usize)> = rows.iter().enumerate().filter_map(|(i, row)| Some((row.slate?, i))).collect();
    ranked.sort_unstable();
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// The row of option `n` (1 is the first choice), of the way back for 0, or -1.
fn option_row(n: i32) -> i32 {
    SENSE.with(|sense| {
        let sense = sense.borrow();
        let found = if n == 0 {
            sense.rows.iter().position(|row| row.slate.is_none() && !row.roll)
        } else {
            usize::try_from(n - 1).ok().and_then(|at| ranking(&sense.rows).get(at).copied())
        };
        found.map_or(-1, |i| i as i32)
    })
}

/// Put the first choice on show, with the keyboard on it.
fn draft_first(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    let first = st.get_assist_prime();
    if first >= 0 {
        preview(app, sh, first);
        st.set_assist_keyed(first);
    }
}

/// Ask the model outright about the palette as it is, or, asked already, for its next few
/// options. The first answer goes on show.
fn ask_more(app: &AppWindow, sh: &SharedRt) {
    settle(app, sh);
    let Some((home, palette)) = active(sh) else { return };
    if model().is_none() {
        return;
    }
    SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let round = match &sense.asking {
            Some((at, then, round, _)) if *at == home && *then == palette.stops => round + 1,
            _ => 0,
        };
        sense.asking = Some((home, palette.stops.clone(), round, None));
    });
    refresh(app, sh);
    draft_first(app, sh);
}

fn roll_choice() -> (Mode, Wild) {
    let (mode, wild) = crate::prefs::roll_choice();
    (Mode::from_name(&mode).unwrap_or(Mode::Around), Wild::from_name(&wild).unwrap_or(Wild::Lively))
}

/// Roll a palette in the chosen mode and put it on show at the head of the row.
fn roll_now(app: &AppWindow, sh: &SharedRt) {
    settle(app, sh);
    let Some((home, palette)) = active(sh) else { return };
    let chosen = selected(app, palette.stops.len());
    let (mode, wild) = roll_choice();
    let others = library(sh, home);
    let model = model();
    let (held, count) = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        sense.rolled += 1;
        let held = sense.pins.as_ref().filter(|(at, _)| *at == home).map(|(_, pins)| pins.clone()).unwrap_or_default();
        (held, sense.rolled)
    });
    let clock = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    let seed = clock ^ count.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    // a roll that lands on the palette as it is would look like nothing happened
    let Some(stops) = (0..4u64)
        .map(|attempt| roll::roll(model.as_deref(), &roll::Roll { stops: &palette.stops, held: &held, selected: chosen, library: &others, mode, wild, seed: seed.wrapping_add(attempt) }))
        .find(|stops| !roll::same(stops, &palette.stops))
    else {
        return;
    };
    let title = match mode {
        Mode::Around => "rolled around this",
        Mode::Fresh => "a fresh roll",
        Mode::Mine => "rolled like your others",
        Mode::Wildcard => "a wildcard",
    };
    SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let offer = Offer { id: "roll", title, scope: Scope::Palette, stops, interp: palette.interp, reach: false };
        sense.rolls.insert(0, (home, offer));
        sense.rolls.truncate(ROLLS);
    });
    refresh(app, sh);
    let st = app.global::<State>();
    preview(app, sh, 0);
    st.set_assist_keyed(0);
}

/// Hold the selected stop against every offer, or let it go.
fn toggle_pin(app: &AppWindow, sh: &SharedRt) {
    let Some((home, palette)) = active(sh) else { return };
    let chosen = selected(app, palette.stops.len());
    SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let mut pins = sense.pins.take().filter(|(at, pins)| *at == home && pins.len() == palette.stops.len()).map_or_else(|| vec![false; palette.stops.len()], |(_, pins)| pins);
        if let Some(pin) = pins.get_mut(chosen) {
            *pin = !*pin;
        }
        sense.pins = Some((home, pins));
    });
    refresh(app, sh);
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
        let vivid = vividness(lch);
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
        // what a click on the rail gives: always a hue you can see
        let (l, vivid) = hue_shows(lch.l, vivid);
        st.set_picker_rail(image(RAIL, 1, |x, _| {
            let h = x as f32 / RAIL as f32;
            Lch { l, c: Lch::max_chroma(l, h) * vivid, h }.rgb()
        }));
    }
    // the plane's lines depend on the hue it shows; the rail's ridge only on the terrain
    let (plane, rail) = SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        let (fresh, turned) = (sense.terrain_at.is_none(), sense.terrain_at != Some(hue_key));
        sense.terrain_at = Some(hue_key);
        let terrain = sense.terrain.as_ref();
        (turned.then(|| terrain.and_then(|terrain| contours(terrain, lch.h))), fresh.then(|| terrain.and_then(ridge)))
    });
    if let Some(plane) = plane {
        st.set_picker_terrain_on(plane.is_some());
        if let Some(plane) = plane {
            st.set_picker_terrain(plane);
        }
    }
    if let Some(rail) = rail {
        st.set_picker_ridge_on(rail.is_some());
        if let Some(rail) = rail {
            st.set_picker_ridge(rail);
        }
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

/// The terrain over the picker plane at hue `h`, drawn like a relief map: three rings at set
/// heights of the whole terrain's peak, the inner ones brighter, each a light line in a dark halo
/// so it reads over any colour, and a faint wash on the summit. A terrain whose high ground lies
/// at another hue leaves this plane empty; the rail shows where to turn.
fn contours(terrain: &Terrain, h: f32) -> Option<Image> {
    // twice the plane's drawn size, so the lines come out smooth
    const GRID: (usize, usize) = (384, 192);
    /// Height of each ring as a share of the peak, and how bright its line is.
    const RINGS: [(f32, f32); 3] = [(0.08, 0.5), (0.32, 0.75), (0.66, 1.0)];
    const LINE: f32 = 1.5;
    const HALO: f32 = 3.5;
    let top = terrain.peak();
    if !(top > 0.0 && top.is_finite()) {
        return None;
    }
    let angle = h * std::f32::consts::TAU;
    let (cos, sin) = (angle.cos(), angle.sin());
    let light = |y: usize| 1.0 - y as f32 / (GRID.1 - 1) as f32;
    let rooms: Vec<f32> = (0..GRID.1).map(|y| Lch::max_chroma(light(y), h)).collect();
    let height: Vec<f32> = (0..GRID.0 * GRID.1).map(|i| {
        let (x, y) = (i % GRID.0, i / GRID.0);
        let c = rooms[y] * x as f32 / (GRID.0 - 1) as f32;
        terrain.density([light(y), c * cos, c * sin]) / top
    }).collect();
    if height.iter().all(|v| *v < RINGS[0].0) {
        return None;
    }
    let at = |x: usize, y: usize| height[y.min(GRID.1 - 1) * GRID.0 + x.min(GRID.0 - 1)];
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(GRID.0 as u32, GRID.1 as u32);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i % GRID.0, i / GRID.0);
        let here = height[i];
        // how far, in pixels, this point is from each ring: its height's distance over the slope
        let slope = ((at(x + 1, y) - at(x.saturating_sub(1), y)) / 2.0).hypot((at(x, y + 1) - at(x, y.saturating_sub(1))) / 2.0).max(1e-6);
        let (mut line, mut halo) = (0.0f32, 0.0f32);
        for (level, bright) in RINGS {
            let far = (here - level).abs() / slope;
            line = line.max((LINE - far).clamp(0.0, 1.0) * bright);
            halo = halo.max(((HALO - far) / HALO).clamp(0.0, 1.0) * 0.45 * bright);
        }
        let wash = ((here - RINGS[1].0) / (RINGS[2].0 - RINGS[1].0)).clamp(0.0, 1.0) * 0.08;
        let white = (line * 0.85).max(wash);
        let alpha = white + halo * (1.0 - white);
        if alpha > 0.004 {
            // premultiplied: white ink over a black halo
            let ink = (white * 255.0) as u8;
            *px = Rgba8Pixel { r: ink, g: ink, b: ink, a: (alpha * 255.0) as u8 };
        }
    }
    Some(Image::from_rgba8_premultiplied(buf))
}

/// The terrain along the hue rail, as a skyline standing on the rail's foot: at each hue, a hill
/// as tall as the highest ground anywhere at that hue.
fn ridge(terrain: &Terrain) -> Option<Image> {
    const SIZE: (usize, usize) = (360, 32);
    const SIDE: usize = 16;
    /// The tallest a hill stands, as a share of the rail's height.
    const RISE: f32 = 0.72;
    let top = terrain.peak();
    if !(top > 0.0 && top.is_finite()) {
        return None;
    }
    let hills: Vec<f32> = (0..SIZE.0).map(|x| {
        let h = x as f32 / SIZE.0 as f32;
        let angle = h * std::f32::consts::TAU;
        let (cos, sin) = (angle.cos(), angle.sin());
        let mut best = 0.0f32;
        for l in 1..SIDE {
            let l = l as f32 / SIDE as f32;
            let room = Lch::max_chroma(l, h);
            for c in 0..=SIDE {
                let c = room * c as f32 / SIDE as f32;
                best = best.max(terrain.density([l, c * cos, c * sin]));
            }
        }
        (best / top).min(1.0).sqrt() * RISE * SIZE.1 as f32
    }).collect();
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE.0 as u32, SIZE.1 as u32);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i % SIZE.0, i / SIZE.0);
        // how far below the hilltop this pixel is; ground too low to matter draws nothing
        let under = hills[x] - (SIZE.1 - 1 - y) as f32;
        if hills[x] > 1.5 && under > 0.0 {
            let edge = (2.0 - under).clamp(0.0, 1.0);
            let white = (0.44 + 0.5 * edge) * under.min(1.0);
            let ink = (white * 255.0) as u8;
            *px = Rgba8Pixel { r: ink, g: ink, b: ink, a: ink };
        }
    }
    Some(Image::from_rgba8_premultiplied(buf))
}

/// The mark under a stop the model doubts: a short wave, white so the theme can colour it.
fn wave() -> Image {
    const SIZE: (usize, usize) = (48, 16);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(SIZE.0 as u32, SIZE.1 as u32);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = ((i % SIZE.0) as f32, (i / SIZE.0) as f32);
        let turn = x / SIZE.0 as f32 * 3.0 * std::f32::consts::PI;
        let line = SIZE.1 as f32 / 2.0 + 4.6 * turn.sin();
        // distance to the curve, corrected for its slope so the stroke keeps one width
        let lean = (4.6 * 3.0 * std::f32::consts::PI / SIZE.0 as f32 * turn.cos()).hypot(1.0);
        let ink = ((3.0 - (y - line).abs() / lean).clamp(0.0, 1.0) * 255.0) as u8;
        *px = Rgba8Pixel { r: ink, g: ink, b: ink, a: ink };
    }
    Image::from_rgba8_premultiplied(buf)
}

/// The lightness and vividness a change of hue lands on. Hue shows nowhere near grey, black or
/// white, so turning one of those to a hue also brings out enough colour to see it; the rail draws
/// what this gives, so it always shows hues.
fn hue_shows(light: f32, vivid: f32) -> (f32, f32) {
    if vivid < 0.08 || !(0.12..=0.95).contains(&light) {
        (light.clamp(0.3, 0.85), vivid.max(0.65))
    } else {
        (light, vivid)
    }
}

/// Recolour the selected stop from picker coordinates.
fn pick(app: &AppWindow, sh: &SharedRt, control: Control, lch_at: impl FnOnce(Lch, f32) -> (Lch, f32)) {
    settle(app, sh);
    let Some((_, palette)) = active(sh) else { return };
    let chosen = selected(app, palette.stops.len());
    let Some((from, vivid)) = SENSE.with(|sense| sense.borrow().pick.map(|(lch, vivid, _)| (lch, vivid))) else { return };
    let (mut lch, vivid) = lch_at(from, vivid);
    lch.c = Lch::max_chroma(lch.l, lch.h) * vivid;
    let made = lch.rgb();
    SENSE.with(|sense| {
        let mut sense = sense.borrow_mut();
        sense.pick = Some((lch, vivid, made));
        sense.control = Some(control);
    });
    note_edit(app, sh, chosen);
    edit_active_palette(app, sh, |palette| palette.set_stop_color(chosen, made));
}

pub(super) fn install(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    st.set_picker_v2(crate::prefs::picker_v2());
    st.set_doubt_wave(wave());
    JOURNALING.set(crate::prefs::picker_journal());
    HOME.with(|home| *home.borrow_mut() = Some((app.as_weak(), sh.clone())));
    let bind = |f: fn(&AppWindow, &SharedRt, f32, f32, f32)| {
        let (w, sh) = (app.as_weak(), sh.clone());
        move |a: f32, b: f32, c: f32| {
            if let Some(app) = w.upgrade() {
                f(&app, &sh, a, b, c);
            }
        }
    };
    let plane = bind(|app, sh, vivid, light, _| {
        pick(app, sh, Control::Plane, |from, _| (Lch { l: light.clamp(0.0, 1.0), ..from }, vivid.clamp(0.0, 1.0)));
    });
    st.on_picker_plane_pick(move |vivid, light| plane(vivid, light, 0.0));
    let hue = bind(|app, sh, hue, _, _| {
        pick(app, sh, Control::Slider, |from, vivid| {
            let (l, vivid) = hue_shows(from.l, vivid);
            (Lch { l, h: hue.clamp(0.0, 1.0) % 1.0, ..from }, vivid)
        })
    });
    st.on_picker_hue_pick(move |h| hue(h, 0.0, 0.0));
    st.on_picker_nudge(bind(|app, sh, vivid, light, hue| {
        pick(app, sh, Control::Other, |from, was| {
            let (l, was) = if hue != 0.0 { hue_shows(from.l, was) } else { (from.l, was) };
            (Lch { l: (l + light).clamp(0.0, 1.0), h: (from.h + hue).rem_euclid(1.0), ..from }, (was + vivid).clamp(0.0, 1.0))
        });
    }));
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
    st.on_assist_dismiss(offer(dismiss));
    st.on_assist_at(dot_at);
    st.on_assist_option(option_row);
    let (w, sh2) = (app.as_weak(), sh.clone());
    st.on_assist_ask(move || {
        if let Some(app) = w.upgrade() {
            ask_more(&app, &sh2);
        }
    });
    let (w, sh2) = (app.as_weak(), sh.clone());
    st.on_roll(move || {
        if let Some(app) = w.upgrade() {
            roll_now(&app, &sh2);
        }
    });
    let names = |all: &[&'static str]| ModelRc::new(VecModel::from(all.iter().map(|n| SharedString::from(*n)).collect::<Vec<_>>()));
    st.set_roll_modes(names(&Mode::ALL.map(Mode::name)));
    st.set_roll_wilds(names(&Wild::ALL.map(Wild::name)));
    let shown = |app: &AppWindow| {
        let (mode, wild) = roll_choice();
        let st = app.global::<State>();
        st.set_roll_mode(Mode::ALL.iter().position(|m| *m == mode).unwrap_or(0) as i32);
        st.set_roll_wild(Wild::ALL.iter().position(|w| *w == wild).unwrap_or(1) as i32);
    };
    shown(app);
    let w = app.as_weak();
    st.on_roll_choose(move |mode, wild| {
        let mode = Mode::ALL.get(mode.max(0) as usize).copied().unwrap_or(Mode::Around);
        let wild = Wild::ALL.get(wild.max(0) as usize).copied().unwrap_or(Wild::Lively);
        crate::prefs::set_roll_choice(mode.name(), wild.name());
        if let Some(app) = w.upgrade() {
            shown(&app);
        }
    });
    let sync = offer(|app, sh, _| refresh(app, sh));
    st.on_picker_sync(move || sync(0));
    let pin = offer(|app, sh, _| toggle_pin(app, sh));
    st.on_toggle_pin(move || pin(0));
    let (w, sh) = (app.as_weak(), sh.clone());
    st.on_set_picker_v2(move |on| {
        if let Some(app) = w.upgrade() {
            crate::prefs::set_picker_v2(on);
            // leave nothing half-shown behind when switching away
            preview(&app, &sh, -1);
            SENSE.with(|sense| *sense.borrow_mut() = Sense::default());
            show(&app, &[], 0, &[], &[]);
            app.global::<State>().set_picker_v2(on);
            refresh(&app, &sh);
        }
    });
}
