// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The STACK inspector's sense of the stack: the State surface over [`neuron::stacksense`].
//!
//! Each layer is recorded alone over a short window on a thread of its own (once per look: a blend,
//! opacity or ducking change re-composites the takes it already has), and the reading comes back to
//! the UI when it lands, if the stack is still the one it was read for. Every layer is judged, not
//! only the selected one, and what comes back is worn by the controls already there:
//!
//! - pointing at a stack row shows its layer on the board itself: keys its light doesn't reach dim,
//!   a key where it's lost wears a faint ring, and the keys where it keeps another layer's light out
//!   wear an amber one. A ring marks the exception: when a layer is lost, or does its harm, over most
//!   of what it lights, the dimming says the first and the board's frame takes the amber for the
//!   second. Pointing at a key lights the row of the layer that shows there.
//! - a trouble and what would fix it sit in a strip above the rows, apart from the layers themselves
//!   ("Wave buried under Wildlife" · "Wildlife → screen", "Wildlife → 35%", "Wave ↑ above Wildlife").
//!   A trouble has a tier, like a linter's: a note (it reads, but could read better), a warning
//!   (buried, partly showing) or an error (as good as invisible). The words, the line's colour and the
//!   board's marks all carry it.
//!   Resting on a fix plays the stack with it on the board, from the recordings, and changes nothing
//!   else: not the layers, not the controls, not the device. Leaving puts the board back; a click
//!   keeps it as an ordinary lighting edit the status bar can undo. A setting changed by hand is never
//!   second-guessed.
//! - the opacity rail carries the selected layer's temperature: unseen, reads well, or buries what's
//!   beneath; the blend picker previews the stack under each blend and marks the one its fixes favour.
//!
//! A newly added layer starts in the blend, opacity and ducking [`neuron::stacksense::Stack::place`]
//! picks.

use super::{
    flush_lighting_save, layer_label, refresh_layers, ui_grid_dims, update_value_model, AppWindow, ComponentHandle, Image,
    RefCell, Rgba8Pixel, SharedPixelBuffer, SharedRt, State,
};
use crate::ui::{LightingFix, LightingStackRow};
use neuron::effects::Blend;
use neuron::lighting::Rgb;
use neuron::pattern::{InputDependencies, LayerDef, LayerInstanceId};
use crate::light_render::TAKE_STEP;
use neuron::stacksense::{Arrangement, Fix, Look, Part, Stack, Take, Trouble, Zone, OFFERS};
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};
use std::time::{Duration, Instant};

/// A placement reads every this many recorded frames.
const PLACE_EVERY: usize = 4;
/// Opacity steps the slider's temperature is scored at.
const STEPS: usize = 50;
/// The blend picker's order.
const BLENDS: [Blend; 5] = [Blend::Normal, Blend::Screen, Blend::Add, Blend::Multiply, Blend::Cut];
/// A drag answers only for where it stops.
const SETTLE: Duration = Duration::from_millis(60);
/// Takes remembered across reads; a stack is a handful of layers, and a few recent looks besides.
const TAKES_KEPT: usize = 24;
/// How often a playing preview redraws the board, blending between recorded frames.
const PLAY_TICK: Duration = Duration::from_millis(33);
/// Under this share of its light reaching a key (on [`neuron::stacksense::Reading::visible`]'s scale),
/// the layer is lost there.
const SHOWS: f32 = 0.35;
/// The bits of a focus cell (LightingStackRow.focus).
const REACHES: i32 = 1;
const LOST: i32 = 2;
const TROUBLE: i32 = 4;
/// Past this share (of the keys a layer lights, or for its harm of the board), a ring would be on
/// nearly every key and say nothing.
const MOST: f32 = 0.6;
/// Under this visibility a buried layer is as good as invisible: an error, not a warning.
const HIDDEN: f32 = 0.12;
/// Past this share of its lit keys going to white, a wash-out is a warning, not a note.
const WHITED: f32 = 0.5;
/// The tiers of a trouble.
const NOTE: i32 = 0;
const WARNING: i32 = 1;
const ERROR: i32 = 2;

/// What the board shows of one layer: the per-cell bits, and whether its harm covers most of it.
type Focus = (Vec<i32>, bool);

/// A layer setting stack sense may suggest, and so must not suggest back once it's set by hand.
#[derive(Clone, Copy)]
pub(super) enum Setting {
    Blend = 0,
    Opacity = 1,
    Duck = 2,
}

/// A setting changed by hand on the board being edited: (layer, [`Setting`]).
type Hand = (u64, u8);
/// The stack a layer id belongs to: ids are only unique within one profile's stack on one board
/// (profile, pid, unit).
type Board = (String, u16, String);

/// A stack on a board: what a reading is for. It reads every layer, so pointing at another one
/// needs no new reading.
#[derive(Clone, Debug, PartialEq)]
struct Ask {
    dims: (u8, u8),
    defs: Vec<LayerDef>,
    hand: Vec<Hand>,
}

/// What kind of trouble a group of layers shares with one cause.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Buried under (or blacked out by) the cause above.
    Under,
    /// Lost in the cause beneath.
    In,
    /// Washing out to white with the cause beneath.
    Wash,
    /// Multiply with nothing under it.
    Nothing,
}

/// A trouble in the stack, said once: the layers it happens to (topmost first, as the rows read), the layer causing it,
/// what kind it is, how bad ([`NOTE`], [`WARNING`], [`ERROR`], the worst of its layers' and `hidden`
/// only when every one of them is as good as gone), and its fixes (indices into [`Answer::fixes`]),
/// best first. Layers buried by the same layer are one trouble: "Tidepool and Starlight buried under
/// Wildlife".
struct Told {
    victims: Vec<usize>,
    cause: Option<usize>,
    kind: Kind,
    tier: i32,
    hidden: bool,
    fixes: Vec<usize>,
}

impl Told {
    fn involves(&self, part: usize) -> bool {
        self.victims.contains(&part) || self.cause == Some(part)
    }
}

/// A reading of an [`Ask`].
struct Answer {
    ask: Ask,
    /// Every fix stack sense offers across the stack.
    fixes: Vec<Fix>,
    troubles: Vec<Told>,
    /// Per layer: what the board shows while its row is pointed at (see [`focus`]).
    focus: Vec<Focus>,
    /// Per cell: the layer that shows there, -1 where nothing is lit.
    owners: Vec<i32>,
    /// Per layer: the worst tier of a trouble it causes, -1 for none.
    causes: Vec<i32>,
    /// Per layer: the opacity rail's temperature and the blend picker's previews.
    strips: Vec<Vec<Zone>>,
    previews: Vec<Vec<Vec<Rgb>>>,
    number: u64,
}

#[derive(Default)]
struct Shown {
    /// The reading on show and the stack it was made for.
    answer: Option<Answer>,
    /// The reading and selection last put into the controls.
    projected: Option<(u64, usize)>,
}

/// A fix playing on the board: the stack and board it was measured on, what the board showed before,
/// and the timer drawing it.
struct Playing {
    base: (Vec<LayerDef>, (u8, u8), Board),
    before: Vec<slint::Color>,
    _timer: slint::Timer,
}

thread_local! {
    static SHOWN: RefCell<Shown> = RefCell::new(Shown::default());
    static PLAYING: RefCell<Option<Playing>> = const { RefCell::new(None) };
    /// The layers (by id, bottom first) the board's focus was last set against.
    static FOCUSED_ON: RefCell<Vec<Option<LayerInstanceId>>> = const { RefCell::new(Vec::new()) };
    static HOME: RefCell<Option<(slint::Weak<AppWindow>, SharedRt)>> = const { RefCell::new(None) };
    static HAND: RefCell<BTreeSet<(Board, Hand)>> = const { RefCell::new(BTreeSet::new()) };
    /// Under test, readings asked for but not yet landed, while [`defer_readings`] is on.
    #[cfg(test)]
    static UNLANDED: RefCell<Option<Vec<Ask>>> = const { RefCell::new(None) };
}

static DESK: (Mutex<Desk<Ask>>, Condvar) = (Mutex::new(Desk::new()), Condvar::new());
/// Each reading's number, so one already projected isn't projected again.
static READINGS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TAKES: Mutex<Vec<(LayerDef, (u8, u8), Arc<Take>)>> = Mutex::new(Vec::new());

/// What a layer draws, without how it's laid on the stack: the key a take is remembered by.
fn content(def: &LayerDef) -> LayerDef {
    LayerDef { id: None, blend: Blend::Normal, opacity: 1.0, enabled: true, duck: false, ..def.clone() }
}

fn take_for(def: &LayerDef, dims: (u8, u8)) -> Arc<Take> {
    let key = content(def);
    let found = TAKES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(k, d, _)| *d == dims && *k == key).map(|(_, _, t)| t.clone());
    if let Some(take) = found {
        return take;
    }
    let take = Arc::new(crate::light_render::record_layer(&key, dims));
    if take.frames.is_empty() {
        return take;
    }
    let mut takes = TAKES.lock().unwrap_or_else(PoisonError::into_inner);
    takes.push((key, dims, take.clone()));
    if takes.len() > TAKES_KEPT {
        takes.remove(0);
    }
    take
}

/// Whether a layer's recording stands for what it really shows: one lit by audio, the screen or a
/// live readout records the stand-in, not your music or your battery.
fn known(def: &LayerDef) -> bool {
    neuron::pattern::pattern_def(&def.pattern).is_some_and(|p| matches!(p.input_dependencies(), InputDependencies::None | InputDependencies::Keys))
}

fn stack_of(defs: &[LayerDef], dims: (u8, u8)) -> Stack {
    stack_every(defs, dims, 1)
}

/// [`stack_of`] from every `every`th recorded frame: coarser, and as many times quicker to read.
fn stack_every(defs: &[LayerDef], dims: (u8, u8), every: usize) -> Stack {
    let cells = usize::from(dims.0) * usize::from(dims.1);
    let takes: Vec<Arc<Take>> = defs
        .iter()
        .map(|d| take_for(d, dims))
        .map(|t| if every > 1 { Arc::new(Take { frames: t.frames.iter().step_by(every).cloned().collect() }) } else { t })
        .collect();
    let frames = takes.iter().map(|t| t.frames.len()).max().unwrap_or(0);
    let parts = defs
        .iter()
        .zip(takes)
        .map(|(d, take)| {
            // a layer that drew nothing for this board (a painted frame of another size) stands aside
            // unlit and unjudged, rather than leaving the whole stack with nothing to read
            let blank = take.frames.len() < frames;
            Part {
                take: if blank { Arc::new(Take { frames: vec![vec![Rgb::BLACK; cells]; frames] }) } else { take },
                region: d.region.clone(),
                look: Look { blend: d.blend, opacity: d.opacity, enabled: d.enabled, duck: d.duck },
                known: known(d) && !blank,
            }
        })
        .collect();
    Stack::new(parts, (usize::from(dims.0), usize::from(dims.1)))
}

/// The layer a trouble happens to, and the one causing it (none for Multiply over nothing).
fn pair(sel: usize, trouble: Trouble) -> (usize, Option<usize>) {
    match trouble {
        Trouble::ErasedBy(o) | Trouble::CoveredBy(o) => (sel, Some(o)),
        Trouble::Erases(b) | Trouble::Covers(b) => (b, Some(sel)),
        Trouble::WashesOut(b) | Trouble::LostIn(b) => (sel, Some(b)),
        Trouble::NothingUnder => (sel, None),
    }
}

/// A fix that would undo a setting the person chose by hand.
fn second_guesses(fix: Fix, defs: &[LayerDef], hand: &[Hand]) -> bool {
    let set = |part: usize, what: u8| defs.get(part).and_then(|d| d.id).is_some_and(|id| hand.contains(&(id.0, what)));
    match fix {
        Fix::Blend { part, .. } => set(part, Setting::Blend as u8),
        Fix::Opacity { part, .. } => set(part, Setting::Opacity as u8),
        Fix::Duck { part } => set(part, Setting::Duck as u8),
        Fix::Above { .. } | Fix::Below { .. } => false,
    }
}

/// What the board shows of layer `part` while its row is pointed at, per cell: [`REACHES`] where its
/// light gets to the board, [`LOST`] where it lights but it's lost (or hidden), and [`TROUBLE`] over
/// `harms`, the cells where it keeps another layer's light out. A layer the reading can't speak for
/// shows where it lights. The lost ring is dropped when it would cover most of what the layer lights,
/// and the harm ring when it would cover most of the board; for the harm, the second value says so
/// instead.
fn focus(stack: &Stack, arr: &Arrangement, part: usize, def: &LayerDef, harms: Option<&[bool]>) -> Focus {
    let reach = stack.reach(arr, part);
    let own = stack.glimpse(part);
    let mut marks: Vec<i32> = reach
        .iter()
        .zip(&own)
        .enumerate()
        .map(|(i, (r, &c))| {
            let lit = match r {
                Some(_) if !known(def) => REACHES,
                Some(r) if *r >= SHOWS => REACHES,
                Some(_) => LOST,
                None if !def.enabled && c != Rgb::BLACK => LOST,
                None => 0,
            };
            lit | if harms.is_some_and(|t| t.get(i).copied().unwrap_or(false)) { TROUBLE } else { 0 }
        })
        .collect();
    let count = |bit: i32| marks.iter().filter(|&&m| m & bit != 0).count() as f32;
    // what a layer harms lies mostly where it doesn't light (an eraser's dark keys), so that share is
    // of the board
    let wide = count(TROUBLE) > MOST * marks.len().max(1) as f32;
    let lost = count(LOST) > MOST * count(REACHES | LOST).max(1.0);
    let strip = if wide { TROUBLE } else { 0 } | if lost { LOST } else { 0 };
    for m in &mut marks {
        *m &= !strip;
    }
    (marks, wide)
}

/// [`focus`] for every layer, with the cells of each trouble (`pairs`: (troubled, culprit) and where
/// it happens) marked on the layer causing it.
fn focuses(stack: &Stack, arr: &Arrangement, defs: &[LayerDef], pairs: &[((usize, usize), Vec<bool>)]) -> Vec<Focus> {
    (0..defs.len())
        .map(|p| {
            let mut harms: Option<Vec<bool>> = None;
            for (_, cells) in pairs.iter().filter(|((_, culprit), _)| *culprit == p) {
                match harms.as_mut() {
                    Some(t) => t.iter_mut().zip(cells).for_each(|(t, c)| *t |= c),
                    None => harms = Some(cells.clone()),
                }
            }
            focus(stack, arr, p, &defs[p], harms.as_deref())
        })
        .collect()
}

fn answer(ask: Ask) -> Answer {
    let stack = stack_of(&ask.defs, ask.dims);
    let arr = stack.current();
    let n = ask.defs.len();
    let (mut fixes, mut troubles, mut pairs): (Vec<Fix>, Vec<Told>, Vec<(usize, usize)>) = (Vec::new(), Vec::new(), Vec::new());
    let mut said: Vec<(usize, Option<usize>)> = Vec::new();
    let now = stack.read(&arr);
    for p in 0..n {
        let sense = stack.sense_from(p, now.clone(), |fix| !second_guesses(fix, &ask.defs, &ask.hand));
        let Some(trouble) = sense.trouble else { continue };
        // the layer a trouble happens to and the one causing it each read it from their side; it's
        // said once, from the side read first, with the fixes both sides found
        let (troubled, culprit) = pair(p, trouble);
        let tier = tier_of(trouble, sense.now.visible[troubled].unwrap_or(0.0), sense.now.washed[p]);
        let kind = match trouble {
            Trouble::ErasedBy(_) | Trouble::CoveredBy(_) | Trouble::Erases(_) | Trouble::Covers(_) => Kind::Under,
            Trouble::LostIn(_) => Kind::In,
            Trouble::WashesOut(_) => Kind::Wash,
            Trouble::NothingUnder => Kind::Nothing,
        };
        let key = (troubled, culprit.unwrap_or(troubled));
        if !pairs.contains(&key) {
            pairs.push(key);
        }
        // one trouble per cause and kind, gathering every layer it happens to; a wash-out is its pair's
        let at = troubles.iter().position(|t| {
            t.cause == culprit && t.kind == kind && (kind != Kind::Wash || t.victims.contains(&troubled))
                && kind != Kind::Nothing
        });
        if at.is_none() && said.contains(&(troubled, culprit)) {
            continue;
        }
        let i = at.unwrap_or_else(|| {
            troubles.push(Told { victims: Vec::new(), cause: culprit, kind, tier, hidden: true, fixes: Vec::new() });
            troubles.len() - 1
        });
        let t = &mut troubles[i];
        if !said.contains(&(troubled, culprit)) {
            said.push((troubled, culprit));
            if !t.victims.contains(&troubled) {
                t.victims.push(troubled);
            }
            t.hidden &= tier == ERROR;
        }
        t.tier = t.tier.max(tier);
        for offer in &sense.offers {
            let at = fixes.iter().position(|f| *f == offer.fix).unwrap_or_else(|| {
                fixes.push(offer.fix);
                fixes.len() - 1
            });
            if !t.fixes.contains(&at) {
                t.fixes.push(at);
            }
        }
    }
    for t in &mut troubles {
        t.victims.sort_unstable_by(|a, b| b.cmp(a));
    }
    let mut causes = vec![-1; n];
    for t in &troubles {
        if let Some(culprit) = t.cause {
            causes[culprit] = causes[culprit].max(t.tier);
        }
    }
    let pairs: Vec<((usize, usize), Vec<bool>)> = pairs
        .into_iter()
        .filter(|(t, c)| t != c)
        .map(|(troubled, culprit)| ((troubled, culprit), stack.harm(&arr, culprit, troubled)))
        .collect();
    let strips = (0..n).map(|p| stack.strip(p, STEPS)).collect();
    let previews = (0..n)
        .map(|p| {
            let shown_at = stack.showcase(p);
            BLENDS
                .iter()
                .map(|&blend| {
                    let mut at = arr.clone();
                    at.looks[p] = Look { blend, enabled: true, ..at.looks[p] };
                    stack.frame(&at, shown_at)
                })
                .collect()
        })
        .collect();
    let focus = focuses(&stack, &arr, &ask.defs, &pairs);
    let owners = stack.owners(&arr).into_iter().map(|o| o.map_or(-1, |p| p as i32)).collect();
    let number = READINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Answer { ask, fixes, troubles, focus, owners, causes, strips, previews, number }
}

/// The stack-sense thread's queue: the one ask waiting (the latest wins) and the one being read.
#[derive(Debug)]
struct Desk<T> {
    queued: Option<T>,
    running: Option<T>,
}

impl<T: Clone + PartialEq> Desk<T> {
    const fn new() -> Self {
        Desk { queued: None, running: None }
    }

    /// Ask for a reading; true when the worker has something new to wake for. The same question
    /// already being read is answered when that reading lands, so anything queued since (a stack
    /// that went somewhere and came back) is moot.
    fn post(&mut self, ask: T) -> bool {
        if self.running.as_ref() == Some(&ask) {
            self.queued = None;
            return false;
        }
        self.queued = Some(ask);
        true
    }

    /// The worker takes the waiting ask to read.
    fn start(&mut self) -> Option<T> {
        let ask = self.queued.take();
        self.running.clone_from(&ask);
        ask
    }

    /// The worker is done reading; true when the reading still answers the latest ask.
    fn finish(&mut self) -> bool {
        self.running = None;
        self.queued.is_none()
    }
}

/// Read `ask` on the stack-sense thread; the answer arrives through [`deliver`].
// under test the reading is made in step with the window, and the worker below is never reached
#[cfg_attr(test, allow(unreachable_code))]
fn post(ask: Ask) {
    #[cfg(test)]
    {
        // a test reads in step with the window, unless it asks to see the gap the worker leaves
        if UNLANDED.with(|u| u.borrow_mut().as_mut().map(|pending| pending.push(ask.clone())).is_none()) {
            deliver(answer(ask));
        }
        return;
    }
    static WORKER: Once = Once::new();
    WORKER.call_once(|| {
        let work = || loop {
            let mut desk = DESK.0.lock().unwrap_or_else(PoisonError::into_inner);
            while desk.queued.is_none() {
                desk = DESK.1.wait(desk).unwrap_or_else(PoisonError::into_inner);
            }
            drop(desk);
            std::thread::sleep(SETTLE);
            let Some(ask) = DESK.0.lock().unwrap_or_else(PoisonError::into_inner).start() else { continue };
            let reading = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| answer(ask)));
            let fresh = DESK.0.lock().unwrap_or_else(PoisonError::into_inner).finish();
            match reading {
                Ok(reading) if fresh => {
                    let _ = slint::invoke_from_event_loop(move || deliver(reading));
                }
                Ok(_) => {}
                Err(_) => eprintln!("neuron: stack sense couldn't read the stack"),
            }
        };
        if !crate::worker::spawn_detached("neuron-stack-sense", work) {
            eprintln!("neuron: stack sense thread not started");
        }
    });
    if DESK.0.lock().unwrap_or_else(PoisonError::into_inner).post(ask) {
        DESK.1.notify_one();
    }
}

/// Under test, hold readings back until [`land_readings`], as the worker thread does for the time a
/// reading takes, so a test sees the window between a stack changing and its reading landing.
#[cfg(test)]
pub(crate) fn defer_readings() {
    UNLANDED.with(|u| *u.borrow_mut() = Some(Vec::new()));
}

/// Under test, land every reading held back since [`defer_readings`], in order.
#[cfg(test)]
pub(crate) fn land_readings() {
    let pending = UNLANDED.with(|u| u.borrow_mut().as_mut().map(std::mem::take).unwrap_or_default());
    for ask in pending {
        deliver(answer(ask));
    }
}

fn home() -> Option<(AppWindow, SharedRt)> {
    HOME.with(|home| home.borrow().as_ref().and_then(|(app, sh)| Some((app.upgrade()?, sh.clone()))))
}

fn ask_now(app: &AppWindow, sh: &SharedRt) -> Option<Ask> {
    let s = sh.borrow();
    // a hand-written opacity that isn't a number can't be read (and never equals itself)
    if s.light_layers.is_empty() || s.light_layers.iter().any(|d| !d.opacity.is_finite()) {
        return None;
    }
    let dims = ui_grid_dims(&app.global::<State>());
    let board = board_of(&s);
    let hand = HAND.with(|hand| hand.borrow().iter().filter(|(b, _)| *b == board).map(|(_, h)| *h).collect());
    (dims.0 > 0 && dims.1 > 0).then(|| Ask { dims, defs: s.light_layers.clone(), hand })
}

fn board_of(s: &super::Shared) -> Board {
    (s.rt.active_profile.clone(), s.rt.selected_pid, s.rt.selected_unit.clone())
}

/// The layer being edited, within the stack.
fn selected_of(s: &super::Shared) -> usize {
    s.selected_layer.min(s.light_layers.len().saturating_sub(1))
}

/// Layer `id`'s setting was changed by hand in the stack being edited: stack sense won't offer to
/// change it back for the rest of the session.
pub(super) fn hand_set(s: &super::Shared, id: Option<LayerInstanceId>, setting: Setting) {
    if let Some(id) = id {
        HAND.with(|hand| hand.borrow_mut().insert((board_of(s), (id.0, setting as u8))));
    }
}

fn blend_word(blend: Blend) -> &'static str {
    if blend == Blend::Cut { "cutout" } else { blend.as_str() }
}

/// A layer's name, told apart from a namesake: two Wildlife layers read "Wildlife above" and
/// "Wildlife below" from the other's point of view.
fn name_of(defs: &[LayerDef], part: usize, from: usize) -> String {
    let name = layer_label(&defs[part]);
    if part != from && layer_label(&defs[from]) == name {
        format!("{name} {}", if part > from { "above" } else { "below" })
    } else {
        name
    }
}

/// How bad a trouble is: a wash-out by the share of the layer's lit keys gone to white (a note, or a
/// warning for most of them), any other by the visibility left to the layer it happens to (buried
/// is a warning, as good as invisible an error).
fn tier_of(trouble: Trouble, visible: f32, washed: f32) -> i32 {
    match trouble {
        Trouble::NothingUnder => ERROR,
        Trouble::WashesOut(_) if washed > WHITED => WARNING,
        Trouble::WashesOut(_) => NOTE,
        _ if visible < HIDDEN => ERROR,
        _ => WARNING,
    }
}

/// Layer names as a list: "Tidepool", "Tidepool and Starlight", "Tidepool, Starlight and Aurora";
/// with `most` names at most, the rest counted ("Tidepool and 2 more").
fn listed(names: &[String], most: usize) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        _ if names.len() > most => format!("{} and {} more", names[..most].join(", "), names.len() - most),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// A trouble in words, in full and with the layers it happens to cut to one and a count: the strip
/// shows the full words when they fit. An error says it plainly.
fn told(defs: &[LayerDef], t: &Told) -> (String, String) {
    let from = t.victims.first().copied().unwrap_or(0);
    // namesakes are counted, not repeated: "2 Wave layers and Tidepool"
    let mut names: Vec<(String, usize)> = Vec::new();
    for &v in &t.victims {
        let name = name_of(defs, v, t.cause.unwrap_or(from));
        match names.iter_mut().find(|(n, _)| *n == name) {
            Some((_, count)) => *count += 1,
            None => names.push((name, 1)),
        }
    }
    let names: Vec<String> = names.into_iter().map(|(n, c)| if c > 1 { format!("{c} {n} layers") } else { n }).collect();
    let cause = t.cause.map(|c| name_of(defs, c, from)).unwrap_or_default();
    let say = |who: String| {
        match t.kind {
            Kind::Under if t.hidden => format!("{who} hidden under {cause}"),
            Kind::Under => format!("{who} buried under {cause}"),
            Kind::In if t.hidden => format!("{who} can't be seen in {cause}"),
            Kind::In => format!("{who} lost in {cause}"),
            Kind::Wash => format!("{who} and {cause} wash out to white"),
            Kind::Nothing => format!("{who} has nothing under it to darken"),
        }
    };
    let (full, short) = (say(listed(&names, usize::MAX)), say(listed(&names, 1)));
    // "Static and 1 more" can be longer than "Static and Wave": the short form is only ever shorter
    let short = if short.chars().count() < full.chars().count() { short } else { full.clone() };
    (full, short)
}

/// A fix in words: the layer it changes, and the setting it would take.
fn written(defs: &[LayerDef], fix: Fix, offer: usize) -> LightingFix {
    let name = |p: usize| layer_label(&defs[p]);
    let text = match fix {
        Fix::Blend { part, blend } => format!("{} → {}", name(part), blend_word(blend)),
        Fix::Opacity { part, opacity } => format!("{} → {}%", name(part), (opacity * 100.0).round() as u32),
        Fix::Above { part, other } => format!("{} ↑ above {}", name(part), name_of(defs, other, part)),
        Fix::Below { part, other } => format!("{} ↓ below {}", name(part), name_of(defs, other, part)),
        Fix::Duck { part } => format!("{} → ducks", name(part)),
    };
    LightingFix { text: text.into(), offer: offer as i32 }
}

/// The STACK rows, topmost first: each layer's settings, and what the board shows while its row is
/// pointed at, matched by layer identity.
pub(super) fn rows(defs: &[LayerDef]) -> Vec<LightingStackRow> {
    type Mark = (Option<LayerInstanceId>, Focus, i32);
    let (marks, cells): (Vec<Mark>, usize) = SHOWN.with(|shown| {
        let shown = shown.borrow();
        let Some(a) = shown.answer.as_ref() else { return (Vec::new(), 0) };
        let marks = a.ask.defs.iter().zip(&a.focus).zip(&a.causes).map(|((d, f), &c)| (d.id, f.clone(), c)).collect();
        (marks, usize::from(a.ask.dims.0) * usize::from(a.ask.dims.1))
    });
    defs.iter()
        .enumerate()
        .rev()
        .map(|(index, layer)| {
            let mark = marks.iter().find(|m| m.0.is_some() && m.0 == layer.id);
            let (focus, wide) = mark.map(|m| m.1.clone()).filter(|f| f.0.len() == cells).unwrap_or_default();
            LightingStackRow {
                index: index as i32,
                name: layer_label(layer).into(),
                blend: blend_word(layer.blend).into(),
                amount: format!("{}%", (layer.opacity * 100.0).round() as u32).into(),
                place: if layer.region.is_empty() { "".into() } else { "placed".into() },
                ducks: layer.duck,
                enabled: layer.enabled,
                focus: slint::ModelRc::new(slint::VecModel::from(focus)),
                wide,
                tier: mark.map_or(-1, |m| m.2),
            }
        })
        .collect()
}

/// Bring the reading up to date with the stack. Called whenever the inspector is projected.
pub(super) fn refresh(app: &AppWindow, sh: &SharedRt) {
    // a fix playing on the board was measured on the stack as it was; once that changes, it stops
    let at = playing_on(app, sh);
    let moved = PLAYING.with(|playing| playing.borrow().as_ref().is_some_and(|p| p.base != at));
    if moved {
        stop(app);
        let st = app.global::<State>();
        st.set_light_sense_epoch(st.get_light_sense_epoch().wrapping_add(1));
    }
    // the focus names a layer by its place in the stack; once the layers move, that place may hold
    // another layer, so the board lets go until a row is pointed at again
    let st = app.global::<State>();
    let ids: Vec<Option<LayerInstanceId>> = sh.borrow().light_layers.iter().map(|d| d.id).collect();
    if FOCUSED_ON.with(|f| *f.borrow() != ids) {
        st.set_light_focus_layer(-1);
        st.set_light_sense_epoch(st.get_light_sense_epoch().wrapping_add(1));
        FOCUSED_ON.with(|f| *f.borrow_mut() = ids);
    }
    let Some(ask) = ask_now(app, sh) else {
        SHOWN.with(|shown| *shown.borrow_mut() = Shown::default());
        project(app, None);
        return;
    };
    let current = SHOWN.with(|shown| shown.borrow().answer.as_ref().is_some_and(|a| a.ask == ask));
    project(app, Some(selected_of(&sh.borrow())));
    if !current {
        post(ask);
    }
}

/// A reading has landed: show it if it's for the stack as it is now.
fn deliver(reading: Answer) {
    let Some((app, sh)) = home() else { return };
    if ask_now(&app, &sh).as_ref() != Some(&reading.ask) {
        return;
    }
    let defs = reading.ask.defs.clone();
    let selected = selected_of(&sh.borrow());
    // the strip under the pointer is about to say something new: a fix playing from it stops
    stop(&app);
    SHOWN.with(|shown| shown.borrow_mut().answer = Some(reading));
    let st = app.global::<State>();
    st.set_light_sense_epoch(st.get_light_sense_epoch().wrapping_add(1));
    update_value_model(st.get_light_stack_rows(), rows(&defs), |model| st.set_light_stack_rows(model));
    project(&app, Some(selected));
}

/// A white-on-alpha strip the theme tints, one pixel per opacity step, a touch under full strength
/// so the rail's temperature stays quieter than its fill.
fn strip_image(strip: &[Zone], pick: impl Fn(&Zone) -> f32) -> Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(strip.len().max(1) as u32, 1);
    for (px, zone) in buffer.make_mut_slice().iter_mut().zip(strip) {
        let a = (pick(zone).clamp(0.0, 1.0) * 0.8 * 255.0).round() as u8;
        *px = Rgba8Pixel { r: a, g: a, b: a, a };
    }
    Image::from_rgba8_premultiplied(buffer)
}

/// Put the reading on show into the controls, for `selected` (the layer being edited now; `None` with
/// no stack).
fn project(app: &AppWindow, selected: Option<usize>) {
    let number = SHOWN.with(|shown| shown.borrow().answer.as_ref().map(|a| a.number));
    let key = number.zip(selected);
    if key.is_some() && SHOWN.with(|shown| shown.borrow().projected == key) {
        return;
    }
    SHOWN.with(|shown| shown.borrow_mut().projected = key);
    let st = app.global::<State>();
    let (blend, strip, previews, owners, (trouble, tier), offers) = SHOWN.with(|shown| {
        let shown = shown.borrow();
        let Some(a) = shown.answer.as_ref() else { return (-1, Vec::new(), Vec::new(), Vec::new(), ((String::new(), String::new()), NOTE), Vec::new()) };
        let sel = selected.unwrap_or(0).min(a.ask.defs.len().saturating_sub(1));
        // the strip says the worst trouble the selected layer is part of, or else the worst one
        let worst = |a: &&Told, b: &&Told| a.tier.cmp(&b.tier).then(b.victims.len().cmp(&a.victims.len()).reverse());
        let shown_trouble = a.troubles.iter().filter(|t| t.involves(sel)).max_by(worst).or_else(|| a.troubles.iter().max_by(worst));
        let trouble = shown_trouble.map_or(((String::new(), String::new()), NOTE), |t| (told(&a.ask.defs, t), t.tier));
        let offers: Vec<LightingFix> =
            shown_trouble.map(|t| t.fixes.iter().take(OFFERS).map(|&i| written(&a.ask.defs, a.fixes[i], i)).collect()).unwrap_or_default();
        // the blend the strip's own fixes favour for this layer first, then any reading's
        let ranked = shown_trouble.into_iter().flat_map(|t| t.fixes.iter().map(|&i| a.fixes[i])).chain(a.fixes.iter().copied());
        let blend = ranked.into_iter().find_map(|f| match f {
            Fix::Blend { part, blend } if part == sel => BLENDS.iter().position(|&b| b == blend).map(|i| i as i32),
            _ => None,
        });
        let previews = a.previews.get(sel).into_iter().flatten().map(|frame| Image::from_rgba8_premultiplied(crate::light_render::rasterize_overlay(frame, a.ask.dims))).collect();
        let strip = a.strips.get(sel).cloned().unwrap_or_default();
        (blend.unwrap_or(-1), strip, previews, a.owners.clone(), trouble, offers)
    });
    st.set_light_sense_trouble(trouble.0.into());
    st.set_light_sense_trouble_short(trouble.1.into());
    st.set_light_sense_tier(tier);
    update_value_model(st.get_light_sense_offers(), offers, |model| st.set_light_sense_offers(model));
    update_value_model(st.get_light_cell_owner(), owners, |model| st.set_light_cell_owner(model));
    st.set_light_sense_blend(blend);
    st.set_light_opacity_zoned(!strip.is_empty());
    if !strip.is_empty() {
        st.set_light_opacity_cold(strip_image(&strip, |z| z.cold));
        st.set_light_opacity_warm(strip_image(&strip, |z| z.warm));
        st.set_light_opacity_hot(strip_image(&strip, |z| z.hot));
    }
    st.set_light_blend_previews(slint::ModelRc::new(slint::VecModel::from(previews)));
}

/// `defs` with `fix` applied; the selection stays on the layer it was on.
fn applied(defs: &[LayerDef], sel: usize, fix: Fix) -> (Vec<LayerDef>, usize) {
    let mut next = defs.to_vec();
    let mut order: Vec<usize> = (0..defs.len()).collect();
    match fix {
        Fix::Blend { part, blend } => next[part].blend = blend,
        Fix::Opacity { part, opacity } => next[part].opacity = opacity,
        Fix::Duck { part } => next[part].duck = true,
        Fix::Above { part, other } | Fix::Below { part, other } => {
            order.retain(|&p| p != part);
            let at = order.iter().position(|&p| p == other).unwrap_or(0) + usize::from(matches!(fix, Fix::Above { .. }));
            order.insert(at.min(order.len()), part);
            next = order.iter().map(|&p| defs[p].clone()).collect();
        }
    }
    let sel = order.iter().position(|&p| p == sel).unwrap_or(sel);
    (next, sel)
}

/// The stack offer `index` would leave, from `defs` as they are: `None` when it no longer applies.
fn offered(index: i32, defs: &[LayerDef], sel: usize) -> Option<(Vec<LayerDef>, usize)> {
    SHOWN.with(|shown| {
        let shown = shown.borrow();
        let a = shown.answer.as_ref()?;
        let fix = *a.fixes.get(usize::try_from(index).ok()?)?;
        // a fix was measured on one stack; on any other (mid-drag, before the next reading lands) it
        // would be a guess, so it does nothing until the reading catches up
        (a.ask.defs == defs).then(|| applied(defs, sel, fix))
    })
}

fn set_stack(app: &AppWindow, sh: &SharedRt, defs: Vec<LayerDef>, sel: usize) {
    {
        let mut s = sh.borrow_mut();
        s.light_layers = defs;
        s.selected_layer = sel;
        s.layers_rev = s.layers_rev.wrapping_add(1);
    }
    refresh_layers(app, sh);
}

/// The stack and board a fix playing now would be measured on.
fn playing_on(app: &AppWindow, sh: &SharedRt) -> (Vec<LayerDef>, (u8, u8), Board) {
    let s = sh.borrow();
    (s.light_layers.clone(), ui_grid_dims(&app.global::<State>()), board_of(&s))
}

/// Whether a fix is playing on the board: nothing else may draw the board's frame meanwhile.
pub(crate) fn previewing() -> bool {
    PLAYING.with(|playing| playing.borrow().is_some())
}

/// Stop a fix playing on the board and put back what it showed; the next live frame draws over it.
fn stop(app: &AppWindow) {
    let Some(playing) = PLAYING.with(|playing| playing.borrow_mut().take()) else { return };
    super::update_grid_model(&app.global::<State>(), playing.before);
    super::LIGHT_PREVIEW.with(|p| {
        p.borrow_mut().last_rgb.remove(&crate::light_render::SurfaceId::Hero);
    });
}

/// The board's colours at `elapsed` into a loop of recorded `frames`, blending between neighbours.
fn played(frames: &[Vec<Rgb>], elapsed: Duration) -> Vec<slint::Color> {
    let at = elapsed.as_secs_f32() / TAKE_STEP.as_secs_f32();
    let n = frames.len().max(1);
    let f = (at.floor() as usize) % n;
    let (a, b) = (&frames[f], &frames[(f + 1) % n]);
    a.iter().zip(b).map(|(&x, &y)| {
        let c = Rgb::lerp(x, y, at.fract());
        super::grid_display(c.r, c.g, c.b)
    }).collect()
}

/// Play offer `index` on the board, from the recordings, changing nothing else; -1 stops.
fn preview(app: &AppWindow, sh: &SharedRt, index: i32) {
    stop(app);
    if index < 0 {
        return;
    }
    let base = playing_on(app, sh);
    let sel = selected_of(&sh.borrow());
    let Some((next, _)) = offered(index, &base.0, sel) else { return };
    let dims = base.1;
    let stack = stack_of(&next, dims);
    let arr = stack.current();
    let mut frames: Vec<Vec<Rgb>> = Vec::new();
    loop {
        let frame = stack.frame(&arr, frames.len());
        if frame.is_empty() {
            break;
        }
        frames.push(frame);
    }
    if frames.is_empty() {
        return;
    }
    let st = app.global::<State>();
    let before: Vec<slint::Color> = slint::Model::iter(&st.get_grid_px()).collect();
    super::update_grid_model(&st, played(&frames, Duration::ZERO));
    let timer = slint::Timer::default();
    let (w, start) = (app.as_weak(), Instant::now());
    timer.start(slint::TimerMode::Repeated, PLAY_TICK, move || {
        if let Some(app) = w.upgrade() {
            super::update_grid_model(&app.global::<State>(), played(&frames, start.elapsed()));
        }
    });
    PLAYING.with(|playing| *playing.borrow_mut() = Some(Playing { base, before, _timer: timer }));
}

/// Keep offer `index`: an ordinary lighting edit, journaled so the status bar's undo takes it back.
fn accept(app: &AppWindow, sh: &SharedRt, index: i32) {
    stop(app);
    let (was, sel) = {
        let s = sh.borrow();
        (s.light_layers.clone(), selected_of(&s))
    };
    let Some((next, next_sel)) = offered(index, &was, sel) else { return };
    let (pid, unit) = {
        let s = sh.borrow();
        (s.rt.selected_pid, s.rt.selected_unit.clone())
    };
    set_stack(app, sh, next.clone(), next_sel);
    flush_lighting_save();
    crate::undo::record_lighting(super::LightingCommit { pid, unit, before: was, after: next });
}

/// The look a layer just added at `part` of `defs` should start with, from its own default `preferred`.
/// It runs under the click that adds the layer, so it reads a coarser recording: choosing a look
/// needs the stack's character, not every frame of it.
pub(super) fn place(app: &AppWindow, defs: &[LayerDef], part: usize, preferred: Blend) -> Look {
    let dims = ui_grid_dims(&app.global::<State>());
    if defs.len() < 2 || dims.0 == 0 || dims.1 == 0 {
        return Look { blend: preferred, opacity: 1.0, enabled: true, duck: false };
    }
    stack_every(defs, dims, PLACE_EVERY).place(part, preferred)
}

pub(super) fn install(app: &AppWindow, sh: &SharedRt) {
    HOME.with(|home| *home.borrow_mut() = Some((app.as_weak(), sh.clone())));
    let st = app.global::<State>();
    let (w, s) = (app.as_weak(), sh.clone());
    st.on_light_sense_preview(move |index| {
        if let Some(app) = w.upgrade() {
            preview(&app, &s, index);
        }
    });
    let (w, s) = (app.as_weak(), sh.clone());
    st.on_light_sense_accept(move |index| {
        if let Some(app) = w.upgrade() {
            accept(&app, &s, index);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(pattern: &str) -> LayerDef {
        LayerDef { pattern: pattern.into(), ..LayerDef::default() }
    }

    #[test]
    fn moving_a_layer_keeps_the_selection_on_it() {
        let defs = vec![layer("uniform"), layer("axis"), layer("radial")];
        let (next, sel) = applied(&defs, 0, Fix::Above { part: 0, other: 2 });
        assert_eq!(next.iter().map(|d| d.pattern.as_str()).collect::<Vec<_>>(), ["axis", "radial", "uniform"]);
        assert_eq!(sel, 2);
        let (next, sel) = applied(&defs, 1, Fix::Below { part: 2, other: 0 });
        assert_eq!(next.iter().map(|d| d.pattern.as_str()).collect::<Vec<_>>(), ["radial", "uniform", "axis"]);
        assert_eq!(sel, 2, "the selected layer moved up one place when another went under it");
    }

    #[test]
    fn a_take_is_shared_by_every_look_of_a_layer() {
        let mut a = layer("uniform");
        let mut b = a.clone();
        a.blend = Blend::Screen;
        b.opacity = 0.4;
        b.enabled = false;
        assert_eq!(content(&a), content(&b));
    }

    #[test]
    fn a_fix_is_written_with_the_layer_it_changes() {
        let defs = vec![layer("uniform"), layer("axis")];
        let words = written(&defs, Fix::Blend { part: 1, blend: Blend::Cut }, 4);
        assert_eq!(words.offer, 4);
        assert!(words.text.ends_with("→ cutout"), "{}", words.text);
        assert!(written(&defs, Fix::Opacity { part: 0, opacity: 0.45 }, 0).text.ends_with("→ 45%"));
        assert!(written(&defs, Fix::Above { part: 0, other: 1 }, 1).text.contains(" ↑ above "));
        assert!(written(&defs, Fix::Duck { part: 0 }, 2).text.ends_with("→ ducks"));
        let under = |victims: Vec<usize>, hidden| Told { victims, cause: Some(2), kind: Kind::Under, tier: WARNING, hidden, fixes: Vec::new() };
        let three = vec![layer("uniform"), layer("axis"), layer("radial")];
        let (full, short) = told(&three, &under(vec![0], false));
        assert!(full.contains(" buried under ") && full == short, "{full}");
        assert!(told(&three, &under(vec![0], true)).0.contains(" hidden under "));
        let (full, short) = told(&three, &under(vec![0, 1], false));
        assert!(full.contains(" and ") && full.contains(" buried under "), "{full}");
        assert_eq!(short, full, "\"Static and 1 more\" is no shorter than \"Static and Wave\", so the full words stay");
        let many = vec![layer("uniform"), layer("axis"), layer("radial"), layer("uniform"), layer("axis")];
        let t = Told { victims: vec![3, 2, 1, 0], cause: Some(4), kind: Kind::Under, tier: WARNING, hidden: false, fixes: Vec::new() };
        let (full, short) = told(&many, &t);
        assert!(short.contains(" more buried under ") && short.len() < full.len(), "{short} / {full}");
    }

    #[test]
    fn layers_are_listed_and_counted_past_what_fits() {
        let names: Vec<String> = ["Tidepool", "Starlight", "Aurora"].iter().map(|s| (*s).to_string()).collect();
        assert_eq!(listed(&names[..1], 9), "Tidepool");
        assert_eq!(listed(&names[..2], 9), "Tidepool and Starlight");
        assert_eq!(listed(&names, 9), "Tidepool, Starlight and Aurora");
        assert_eq!(listed(&names, 1), "Tidepool and 2 more");
    }

    #[test]
    fn a_trouble_is_tiered_by_what_is_left_to_see() {
        assert_eq!(tier_of(Trouble::CoveredBy(1), 0.05, 0.0), ERROR, "as good as invisible");
        assert_eq!(tier_of(Trouble::CoveredBy(1), 0.2, 0.0), WARNING, "buried, partly showing");
        assert_eq!(tier_of(Trouble::LostIn(0), 0.29, 0.0), WARNING, "anything buried is at least a warning");
        assert_eq!(tier_of(Trouble::WashesOut(0), 0.9, 0.3), NOTE, "a little white");
        assert_eq!(tier_of(Trouble::WashesOut(0), 0.9, 0.7), WARNING, "mostly white");
        assert_eq!(tier_of(Trouble::NothingUnder, 0.0, 0.0), ERROR);
    }

    #[test]
    fn a_coarse_placement_picks_what_the_full_recording_would() {
        let _no_keys = neuron::capture::suppress_key_reads();
        for look in ["wildlife", "starlight", "fire", "static"] {
            let defs: Vec<LayerDef> = ["wave", "tidepool", look].iter().map(|s| neuron::pattern::preset_layer(s).expect("a catalog look")).collect();
            let preferred = defs[2].blend;
            let coarse = stack_every(&defs, (6, 22), PLACE_EVERY).place(2, preferred);
            assert_eq!(coarse, stack_of(&defs, (6, 22)).place(2, preferred), "{look}");
        }
    }

    #[test]
    fn an_erasers_harm_is_ringed_until_it_covers_most_of_the_board() {
        let _no_keys = neuron::capture::suppress_key_reads();
        let dims = (2, 10);
        let painted = |lit: &dyn Fn(usize) -> bool| LayerDef {
            pattern: "custom".into(),
            frame: (0..20).map(|i| if lit(i) { [0x20, 0xC9, 0xB0] } else { [0, 0, 0] }).collect(),
            ..LayerDef::default()
        };
        // two lit keys on top, blacking out three of the wash's
        let defs = vec![painted(&|_| true), painted(&|i| i < 2)];
        let stack = stack_of(&defs, dims);
        let arr = stack.current();
        let few: Vec<bool> = (0..20).map(|i| (2..5).contains(&i)).collect();
        let (marks, wide) = focus(&stack, &arr, 1, &defs[1], Some(&few));
        assert!(!wide, "three keys of twenty is not most of the board");
        assert_eq!(marks.iter().filter(|&&m| m & TROUBLE != 0).count(), 3, "so each is ringed");
        let most: Vec<bool> = (0..20).map(|i| i >= 2).collect();
        let (marks, wide) = focus(&stack, &arr, 1, &defs[1], Some(&most));
        assert!(wide && marks.iter().all(|&m| m & TROUBLE == 0), "eighteen of twenty is the board's frame, not a ring on each");
    }

    #[test]
    fn a_preview_plays_between_the_recorded_frames() {
        let frames = vec![vec![Rgb::new(0, 0, 0)], vec![Rgb::new(200, 100, 0)]];
        let half = played(&frames, TAKE_STEP / 2)[0];
        assert!(half.red() > 80 && half.red() < 120, "halfway between frames: {half:?}");
        assert_eq!(played(&frames, TAKE_STEP * 3), played(&frames, TAKE_STEP), "it loops");
    }

    #[test]
    fn a_setting_chosen_by_hand_is_not_second_guessed() {
        let mut defs = vec![layer("uniform"), layer("axis")];
        defs[1].id = Some(LayerInstanceId(7));
        let hand = [(7, 0)];
        assert!(second_guesses(Fix::Blend { part: 1, blend: Blend::Screen }, &defs, &hand));
        assert!(!second_guesses(Fix::Opacity { part: 1, opacity: 0.5 }, &defs, &hand), "only the setting chosen");
        assert!(!second_guesses(Fix::Below { part: 1, other: 0 }, &defs, &hand));
    }

    #[test]
    fn a_trouble_pairs_the_layer_it_happens_to_with_its_cause() {
        assert_eq!(pair(1, Trouble::Covers(0)), (0, Some(1)));
        assert_eq!(pair(0, Trouble::CoveredBy(1)), (0, Some(1)));
        assert_eq!(pair(0, Trouble::NothingUnder), (0, None));
    }


    /// Properties of a reading of any stack: real looks (light shows, key-driven, a live readout and a
    /// painted frame) on small boards, in random blends, opacities, ducking, regions and order.
    mod properties {
        use super::*;
        use proptest::prelude::*;

        const LOOKS: [&str; 8] = ["static", "wave", "starlight", "wildlife", "tidepool", "fire", "audiometer", "painted"];
        const BLENDS: [Blend; 5] = [Blend::Normal, Blend::Screen, Blend::Add, Blend::Multiply, Blend::Cut];

        fn look(slug: &str, cells: usize, seed: u8) -> LayerDef {
            if slug == "painted" {
                let frame = (0..cells).map(|i| if (i + usize::from(seed)) % 3 == 0 { [0, 0, 0] } else { [seed, 255 - seed, 90] }).collect();
                return LayerDef { pattern: "custom".into(), frame, ..LayerDef::default() };
            }
            neuron::pattern::preset_layer(slug).expect("a catalog look")
        }

        prop_compose! {
            fn stack()(rows in 1u8..=2, cols in 2u8..=5, n in 1usize..=4)
                (dims in Just((rows, cols)),
                 picks in prop::collection::vec((0..LOOKS.len(), 0..BLENDS.len(), prop_oneof![Just(1.0f32), Just(0.5f32), 0.0f32..=1.0],
                     any::<bool>(), prop::bool::weighted(0.9), any::<u8>(), prop::option::weighted(0.25, any::<prop::sample::Index>())), n),
                 selected in any::<prop::sample::Index>())
                -> ((u8, u8), Vec<LayerDef>, usize)
            {
                let cells = usize::from(dims.0) * usize::from(dims.1);
                let defs: Vec<LayerDef> = picks.iter().enumerate().map(|(i, &(l, b, opacity, duck, enabled, seed, region))| {
                    let mut d = look(LOOKS[l], cells, seed);
                    d.id = Some(LayerInstanceId(i as u64 + 1));
                    d.blend = BLENDS[b];
                    d.opacity = opacity;
                    d.duck = duck;
                    d.enabled = enabled;
                    if let Some(r) = region {
                        let first = r.index(cells) as u32;
                        d.region = (first..cells as u32).collect();
                    }
                    d
                }).collect();
                let selected = selected.index(defs.len());
                (dims, defs, selected)
            }
        }

        fn config() -> ProptestConfig {
            ProptestConfig { cases: 40, ..ProptestConfig::default() }
        }

        proptest! {
            #![proptest_config(config())]

            /// A reading of any stack is well formed: every trouble names layers that exist, its cause
            /// apart from the layers it happens to, every fix it offers exists, applies to the stack it
            /// was read from, and keeps every layer (and the selection on its layer); the words name
            /// the layers; the board's maps fit the board.
            #[test]
            fn a_reading_of_any_stack_is_well_formed((dims, defs, selected) in stack()) {
                let _no_keys = neuron::capture::suppress_key_reads();
                let n = defs.len();
                let cells = usize::from(dims.0) * usize::from(dims.1);
                let a = answer(Ask { dims, defs: defs.clone(), hand: Vec::new() });
                prop_assert_eq!(a.focus.len(), n);
                prop_assert!(a.focus.iter().all(|(f, _)| f.len() == cells));
                prop_assert_eq!(a.owners.len(), cells);
                prop_assert!(a.owners.iter().all(|&o| (-1..n as i32).contains(&o)));
                prop_assert!(a.causes.iter().all(|&c| (-1..=ERROR).contains(&c)));
                for t in &a.troubles {
                    prop_assert!(!t.victims.is_empty());
                    let mut v = t.victims.clone();
                    v.sort_unstable();
                    v.dedup();
                    prop_assert_eq!(v.len(), t.victims.len(), "a layer named twice");
                    prop_assert!(t.victims.iter().all(|&p| p < n));
                    if let Some(c) = t.cause {
                        prop_assert!(c < n && !t.victims.contains(&c), "a layer causing its own trouble");
                        prop_assert!(a.causes[c] >= t.tier);
                    }
                    prop_assert!((NOTE..=ERROR).contains(&t.tier));
                    prop_assert!(!t.hidden || t.tier == ERROR);
                    prop_assert!(t.fixes.iter().all(|&i| i < a.fixes.len()));
                    let (full, short) = told(&defs, t);
                    prop_assert!(!full.is_empty() && short.len() <= full.len(), "{} / {}", full, short);
                    prop_assert!(full.contains(&name_of(&defs, t.victims[0], t.cause.unwrap_or(t.victims[0]))), "{}", full);
                }
                for (i, &fix) in a.fixes.iter().enumerate() {
                    let part = match fix {
                        Fix::Blend { part, .. } | Fix::Opacity { part, .. } | Fix::Above { part, .. } | Fix::Below { part, .. } | Fix::Duck { part } => part,
                    };
                    prop_assert!(written(&defs, fix, i).text.starts_with(layer_label(&defs[part]).as_str()));
                    let (next, sel) = applied(&defs, selected, fix);
                    let mut ids: Vec<_> = next.iter().map(|d| d.id).collect();
                    ids.sort_unstable();
                    prop_assert_eq!(ids, (1..=n as u64).map(|i| Some(LayerInstanceId(i))).collect::<Vec<_>>());
                    prop_assert_eq!(next[sel].id, defs[selected].id, "the selection left its layer");
                }
                // one trouble per cause and kind: no layer is told twice about the same thing
                for (i, t) in a.troubles.iter().enumerate() {
                    for u in &a.troubles[i + 1..] {
                        prop_assert!(!(t.cause == u.cause && t.kind == u.kind && t.victims.iter().any(|v| u.victims.contains(v))),
                            "{:?} told twice about {:?}", t.victims, t.cause);
                    }
                }
                // the board marks harm only on a layer the strip blames, in the tier it's blamed at
                for (p, (marks, wide)) in a.focus.iter().enumerate() {
                    if *wide || marks.iter().any(|m| m & TROUBLE != 0) {
                        prop_assert!(a.causes[p] >= NOTE, "layer {} marked as harming but blamed for nothing", p);
                    }
                }
                prop_assert_eq!(a.strips.len(), n);
                prop_assert!(a.strips.iter().all(|z| z.is_empty() || z.len() == STEPS + 1));
                prop_assert!(a.previews.iter().all(|p| p.len() == BLENDS.len() && p.iter().all(|f| f.len() == cells)));
                let mut seen = vec![false; a.fixes.len()];
                for t in &a.troubles {
                    for &i in &t.fixes {
                        seen[i] = true;
                    }
                }
                prop_assert!(seen.iter().all(|&s| s), "a fix offered for no trouble");
            }

            /// A fix kept from the strip leaves exactly the stack its preview played: the layers it
            /// writes, recorded afresh, composite frame for frame like the arrangement stack sense
            /// measured.
            #[test]
            fn a_kept_fix_leaves_the_stack_it_previewed((dims, defs, selected) in stack()) {
                let _no_keys = neuron::capture::suppress_key_reads();
                let stack = stack_of(&defs, dims);
                let arr = stack.current();
                let a = answer(Ask { dims, defs: defs.clone(), hand: Vec::new() });
                for &fix in &a.fixes {
                    let (next, _) = applied(&defs, selected, fix);
                    let kept = stack_of(&next, dims);
                    let measured = stack.apply(&arr, fix);
                    for f in 0..8 {
                        prop_assert_eq!(kept.frame(&kept.current(), f), stack.frame(&measured, f), "{:?}, frame {}", fix, f);
                    }
                }
            }

            /// The reading queue, against any interleaving of asks (the stack changing, maybe back to
            /// where it was), the worker taking an ask, and the worker finishing: once things go
            /// quiet, what's on show is the reading for the stack as it is.
            #[test]
            fn the_reading_on_show_catches_up_with_the_stack(steps in prop::collection::vec((0u8..3, 0u8..4), 1..40)) {
                let mut desk: Desk<u8> = Desk::new();
                let (mut latest, mut shown, mut reading) = (None::<u8>, None::<u8>, None::<u8>);
                // the window's side: a stack change asks unless the reading on show already answers it
                let ask = |desk: &mut Desk<u8>, latest: &mut Option<u8>, shown: Option<u8>, v: u8| {
                    *latest = Some(v);
                    if shown != Some(v) {
                        desk.post(v);
                    }
                };
                // the worker's side: a reading that lands is shown only if it's for the stack as it is
                let land = |desk: &mut Desk<u8>, reading: &mut Option<u8>, shown: &mut Option<u8>, latest: Option<u8>| {
                    if let Some(v) = reading.take() {
                        if desk.finish() && Some(v) == latest {
                            *shown = Some(v);
                        }
                    }
                };
                for (op, v) in steps {
                    match op {
                        0 => ask(&mut desk, &mut latest, shown, v),
                        1 if reading.is_none() => reading = desk.start(),
                        _ => land(&mut desk, &mut reading, &mut shown, latest),
                    }
                }
                for _ in 0..4 {
                    land(&mut desk, &mut reading, &mut shown, latest);
                    reading = desk.start();
                }
                prop_assert!(desk.queued.is_none() && desk.running.is_none() && reading.is_none(), "the worker goes quiet");
                prop_assert_eq!(shown, latest, "what's on show answers the stack as it is");
            }

            /// A setting chosen by hand is never offered back, on any stack.
            #[test]
            fn nothing_set_by_hand_is_offered_back((dims, defs, _) in stack(), what in 0u8..3) {
                let _no_keys = neuron::capture::suppress_key_reads();
                let hand: Vec<Hand> = defs.iter().filter_map(|d| d.id).map(|id| (id.0, what)).collect();
                let a = answer(Ask { dims, defs: defs.clone(), hand: hand.clone() });
                for &fix in &a.fixes {
                    prop_assert!(!second_guesses(fix, &defs, &hand), "{:?} undoes a setting chosen by hand", fix);
                }
            }
        }
    }
}
