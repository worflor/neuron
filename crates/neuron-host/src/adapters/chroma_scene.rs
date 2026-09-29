// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Generic scene understanding for a stream of game lighting frames.
//!
//! A Chroma game paints a stable picture (an ambient colour plus keys that hold their own
//! colour) and plays rendered effects over it (waves, pulses, sweeps). This module splits the
//! stream into those two layers without knowing anything about the game:
//!
//! - **Rest layer.** Once the board has been still for [`SceneConfig::settle_ms`], the modal
//!   colour is the ambient, every other still cell is a *role* (grouped by exact colour), and
//!   cells that keep changing while the board is otherwise still are *animated*.
//! - **Looks.** Each distinct rest layer the game settles into (a menu, a hero's in-match
//!   picture) is a *look* with a stable id, so the game's phases can be named and reacted to.
//! - **Effect layer.** Board-wide motion opens an effect measured against the frame before it;
//!   it closes on a return to that frame, a quiet gap, or a length cap. Its signature (a
//!   deviation-weighted hue histogram, duration, and spatial uniformity) is matched against
//!   recurring *templates*, so the same effect gets the same id every time it plays.
//!
//! Labels ("this template is an ult") are deliberately not this module's job; it only promises
//! that the same thing gets the same id.
//!
//! Pure state machine, per the adapter contract: `(t_ms, frame)` in, [`SceneEvent`]s out. No
//! clock, no I/O. Calibrated on a 600 s Overwatch capture (see the fixture test); the time
//! constants are defaults, not game knowledge.
#![forbid(unsafe_code)]

use super::chroma_analyze::Rgb;
use serde::{Deserialize, Serialize};

/// Number of chromatic hue bins in a signature.
pub const HUE_BINS: usize = 12;
/// Signature histogram length: [`HUE_BINS`] chromatic bins + achromatic + darkening.
pub const SIG_BINS: usize = HUE_BINS + 2;
/// Histogram bin for low-saturation (white/grey) deviation.
pub const BIN_ACHROMATIC: usize = HUE_BINS;
/// Histogram bin for cells the effect made darker than the pre-effect frame.
pub const BIN_DARKEN: usize = HUE_BINS + 1;

/// Tuning for a [`Scene`]. The defaults are the values measured against real captures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneConfig {
    /// An open effect closes after this long without board motion.
    pub gap_ms: u64,
    /// An effect is cut (and marked truncated) at this length.
    pub max_effect_ms: u64,
    /// Board stillness needed before the rest layer is re-read.
    pub settle_ms: u64,
    /// Per-channel deviation from the pre-effect frame that counts a cell as affected.
    pub affect: u8,
    /// Max deviation still counted as "back to the pre-effect frame".
    pub baseline_eps: u8,
    /// Signature distance below which a closed effect joins an existing template.
    pub match_theta: f32,
    /// How far into an effect the early guess is made.
    pub prefix_ms: u64,
    /// Distance threshold for the early-guess (prefix) matcher.
    pub prefix_theta: f32,
    /// Templates kept per scene; the least recently seen is evicted past this.
    pub max_templates: usize,
    /// Grids smaller than this get the rest layer only: with a handful of LEDs every flicker
    /// reads as board-wide motion and templates degenerate into noise.
    pub min_cells_for_effects: usize,
    /// The modal colour must cover at least this share of the grid to count as ambient.
    pub ambient_min_share: f32,
}

impl Default for SceneConfig {
    fn default() -> Self {
        SceneConfig {
            gap_ms: 300,
            max_effect_ms: 6000,
            settle_ms: 1000,
            affect: 24,
            baseline_eps: 2,
            match_theta: 0.10,
            prefix_ms: 250,
            prefix_theta: 0.20,
            max_templates: 64,
            min_cells_for_effects: 16,
            ambient_min_share: 0.25,
        }
    }
}

/// What a closed (or partially seen) effect looks like, independent of where it played.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    /// Deviation-weighted histogram over [`SIG_BINS`], normalised to sum 1 (all zero if nothing
    /// crossed the affect threshold).
    pub hue: [f32; SIG_BINS],
    /// Effect length in ms (at least 1).
    pub dur_ms: f32,
    /// Mean share of cells lit to at least half the frame's strongest deviation: ~1 for a
    /// board-wide flash, small for a travelling wave front.
    pub uniformity: f32,
}

impl Signature {
    fn dur_term(&self, other: &Signature) -> f32 {
        ((self.dur_ms / other.dur_ms).ln().abs() / 4f32.ln()).min(1.0)
    }
    fn hue_term(&self, other: &Signature) -> f32 {
        0.5 * self.hue.iter().zip(other.hue.iter()).map(|(a, b)| (a - b).abs()).sum::<f32>()
    }

    /// Mean of the hue, duration, and uniformity distances, each in `[0, 1]`.
    #[must_use]
    pub fn distance(&self, other: &Signature) -> f32 {
        (self.hue_term(other) + self.dur_term(other) + (self.uniformity - other.uniformity).abs()) / 3.0
    }

    /// Worst of the three distances. The prefix matcher uses this: early in an effect one
    /// strongly mismatched part is more telling than an average.
    #[must_use]
    pub fn max_distance(&self, other: &Signature) -> f32 {
        self.hue_term(other).max(self.dur_term(other)).max((self.uniformity - other.uniformity).abs())
    }

    /// The histogram bin carrying the most weight, if any weight at all.
    #[must_use]
    pub fn dominant_bin(&self) -> Option<usize> {
        let (i, w) = self
            .hue
            .iter()
            .copied()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(&b.1))?;
        (w > 0.0).then_some(i)
    }

    fn blend(&mut self, s: &Signature, a: f32) {
        for (h, x) in self.hue.iter_mut().zip(s.hue.iter()) {
            *h = (1.0 - a) * *h + a * x;
        }
        self.uniformity = (1.0 - a) * self.uniformity + a * s.uniformity;
        self.dur_ms = ((1.0 - a) * self.dur_ms.ln() + a * s.dur_ms.ln()).exp();
    }
}

/// A human word for a histogram bin (`"cyan"`, `"white"`, `"darken"`).
#[must_use]
pub fn bin_name(bin: usize) -> &'static str {
    const NAMES: [&str; SIG_BINS] = [
        "red", "orange", "yellow", "lime", "green", "teal", "cyan", "azure", "blue", "violet",
        "magenta", "rose", "white", "darken",
    ];
    NAMES.get(bin).copied().unwrap_or("?")
}

/// The chromatic bin for a colour, or [`BIN_ACHROMATIC`] below 25 % saturation.
#[must_use]
pub fn hue_bin(c: Rgb) -> usize {
    let (r, g, b) = (f32::from(c.0), f32::from(c.1), f32::from(c.2));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    if max <= 0.0 || (max - min) / max < 0.25 {
        return BIN_ACHROMATIC;
    }
    let d = max - min;
    let h = if r >= g && r >= b {
        ((g - b) / d).rem_euclid(6.0)
    } else if g >= b {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    ((h * HUE_BINS as f32).round() as usize) % HUE_BINS
}

/// A colour that a set of cells holds while the board is at rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub rgb: Rgb,
    /// Cell indices, ascending.
    pub cells: Vec<u16>,
}

/// The rest layer as of the last settled read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rest {
    /// The modal colour, when it covers enough of the grid to be called one.
    pub ambient: Option<Rgb>,
    /// Still cells that differ from the ambient, grouped by exact colour, ordered by first cell.
    pub roles: Vec<Role>,
    /// Cells that kept changing while the board was otherwise still.
    pub animated: Vec<u16>,
}

/// Something the scene noticed. Times are the caller's clock, in ms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SceneEvent {
    /// An effect has been playing for [`SceneConfig::prefix_ms`]; `likely` is the template it
    /// resolved to most often the last times it started like this.
    Burst { started_ms: u64, likely: Option<u32> },
    /// An effect finished and was matched to template `id`.
    Effect {
        id: u32,
        started_ms: u64,
        dur_ms: u32,
        /// Signature distance to the template (before the template absorbed this instance).
        distance: f32,
        /// This instance created the template.
        new: bool,
        /// Cut at [`SceneConfig::max_effect_ms`] rather than ending on its own.
        truncated: bool,
    },
    /// The ambient colour changed (or appeared/disappeared).
    Ambient { from: Option<Rgb>, to: Option<Rgb> },
    /// The role set changed; read [`Scene::rest`] for the new one.
    Roles { count: usize },
    /// The board settled into look `id` (having been in `from`).
    Look { id: u32, from: Option<u32>, new: bool },
}

/// A resting picture the game returns to: its ambient colour and the colours its keys hold.
/// Serde round-trippable, like [`Template`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Look {
    pub id: u32,
    pub ambient: Rgb,
    /// The held key colours, sorted.
    pub colours: Vec<Rgb>,
    /// Times the game settled into it.
    pub count: u32,
    pub last_ms: u64,
}

impl Look {
    /// Per-channel slack when matching: a game's resting colours are exact, but a look read
    /// mid-fade can be a step off.
    const SLACK: u8 = 6;

    fn matches(&self, ambient: Rgb, colours: &[Rgb]) -> bool {
        let near = |a: Rgb, b: Rgb| a.0.abs_diff(b.0) <= Self::SLACK && a.1.abs_diff(b.1) <= Self::SLACK && a.2.abs_diff(b.2) <= Self::SLACK;
        near(self.ambient, ambient)
            && self.colours.len() == colours.len()
            && self.colours.iter().zip(colours).all(|(a, b)| near(*a, *b))
    }
}

/// A recurring effect as the scene currently understands it. Serde round-trippable so a caller
/// can keep a game's templates across sessions (see [`Scene::with_templates`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: u32,
    pub signature: Signature,
    /// Times it has played.
    pub count: u32,
    pub last_ms: u64,
    /// The brightest non-darkening colour of the instance that created it.
    pub swatch: Rgb,
}

/// Early-guess cluster: effects that START alike, with a vote over what they became.
struct PrefixTemplate {
    signature: Signature,
    count: u32,
    outcomes: Vec<(u32, u32)>,
}

impl PrefixTemplate {
    fn likely(&self) -> Option<u32> {
        self.outcomes.iter().max_by_key(|(_, n)| *n).map(|(id, _)| *id)
    }
    fn vote(&mut self, id: u32) {
        match self.outcomes.iter_mut().find(|(t, _)| *t == id) {
            Some((_, n)) => *n += 1,
            None => self.outcomes.push((id, 1)),
        }
    }
}

/// An effect being accumulated.
struct Open {
    t0: u64,
    t_last_motion: u64,
    base: Vec<Rgb>,
    mask: Vec<bool>,
    peak: Vec<u8>,
    peak_rgb: Vec<Rgb>,
    dark: Vec<bool>,
    uni_sum: f32,
    uni_frames: u32,
    frames: u32,
    /// Index into the prefix templates, once the early guess was made.
    prefix: Option<usize>,
    prefix_sig: Option<Signature>,
}

impl Open {
    fn signature(&self, affect: u8, dur_ms: u64) -> Signature {
        let mut hue = [0f32; SIG_BINS];
        for (i, &p) in self.peak.iter().enumerate() {
            if p > affect {
                let bin = if self.dark[i] { BIN_DARKEN } else { hue_bin(self.peak_rgb[i]) };
                hue[bin] += f32::from(p);
            }
        }
        let total: f32 = hue.iter().sum();
        if total > 0.0 {
            for h in &mut hue {
                *h /= total;
            }
        }
        let uniformity = if self.uni_frames > 0 { self.uni_sum / self.uni_frames as f32 } else { 1.0 };
        Signature { hue, dur_ms: dur_ms.max(1) as f32, uniformity }
    }

    fn swatch(&self) -> Rgb {
        self.peak
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.dark[*i])
            .max_by_key(|(_, p)| **p)
            .map_or((0, 0, 0), |(i, _)| self.peak_rgb[i])
    }
}

fn dev(a: Rgb, b: Rgb) -> u8 {
    a.0.abs_diff(b.0).max(a.1.abs_diff(b.1)).max(a.2.abs_diff(b.2))
}

fn sum3(c: Rgb) -> u16 {
    u16::from(c.0) + u16::from(c.1) + u16::from(c.2)
}

/// One device grid's scene. Feed every new frame with [`push`](Scene::push) and call
/// [`tick`](Scene::tick) when no frame arrived (games only write on change, so time has to
/// reach the scene some other way for effects to close and the rest layer to settle).
pub struct Scene {
    cfg: SceneConfig,
    cells: usize,
    /// Changed cells (outside the animated set) that count as board motion.
    motion_cells: usize,
    prev: Option<Vec<Rgb>>,
    last_change: Vec<Option<u64>>,
    last_motion: Option<u64>,
    first_frame_ms: Option<u64>,
    last_rest_read: Option<u64>,
    animated: Vec<bool>,
    open: Option<Open>,
    rest: Rest,
    looks: Vec<Look>,
    current_look: Option<u32>,
    next_look: u32,
    templates: Vec<Template>,
    prefixes: Vec<PrefixTemplate>,
    next_id: u32,
    frames: u64,
}

impl Scene {
    /// A scene over a grid of `cells` LEDs.
    #[must_use]
    pub fn new(cells: usize, cfg: SceneConfig) -> Self {
        Scene {
            cfg,
            cells,
            motion_cells: cells.div_ceil(32).max(1),
            prev: None,
            last_change: vec![None; cells],
            last_motion: None,
            first_frame_ms: None,
            last_rest_read: None,
            animated: vec![false; cells],
            open: None,
            rest: Rest::default(),
            templates: Vec::new(),
            prefixes: Vec::new(),
            next_id: 0,
            frames: 0,
            looks: Vec::new(),
            current_look: None,
            next_look: 0,
        }
    }

    /// A scene that already knows `templates` (e.g. loaded from a previous session): recurring
    /// effects keep their ids, and new ones are numbered after the highest known id.
    #[must_use]
    pub fn with_templates(cells: usize, cfg: SceneConfig, templates: Vec<Template>) -> Self {
        let mut scene = Scene::new(cells, cfg);
        scene.next_id = templates.iter().map(|t| t.id.saturating_add(1)).max().unwrap_or(0);
        scene.templates = templates;
        scene.templates.truncate(cfg.max_templates);
        scene
    }

    /// Seed known looks (e.g. from a previous session); new ones number after the highest id.
    #[must_use]
    pub fn with_looks(mut self, looks: Vec<Look>) -> Self {
        self.next_look = looks.iter().map(|l| l.id.saturating_add(1)).max().unwrap_or(0);
        self.looks = looks;
        self.looks.truncate(self.cfg.max_templates);
        self
    }

    /// Known looks, oldest id first.
    #[must_use]
    pub fn looks(&self) -> &[Look] {
        &self.looks
    }

    /// The look the board last settled into, if it has one.
    #[must_use]
    pub fn current_look(&self) -> Option<u32> {
        self.current_look
    }

    /// The picture from just before the effect under way started.
    #[must_use]
    pub fn baseline(&self) -> Option<&[Rgb]> {
        self.open.as_ref().map(|o| o.base.as_slice())
    }

    /// The brightest non-darkening colour the effect under way has reached so far.
    #[must_use]
    pub fn open_swatch(&self) -> Option<Rgb> {
        self.open.as_ref().map(Open::swatch)
    }

    /// Grid size this scene was built for.
    #[must_use]
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// The newest frame seen.
    #[must_use]
    pub fn frame(&self) -> Option<&[Rgb]> {
        self.prev.as_deref()
    }

    /// Frames fed so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The rest layer as of the last settled read.
    #[must_use]
    pub fn rest(&self) -> &Rest {
        &self.rest
    }

    /// Known templates, oldest id first.
    #[must_use]
    pub fn templates(&self) -> &[Template] {
        &self.templates
    }

    /// When the effect under way (if any) started.
    #[must_use]
    pub fn open_since(&self) -> Option<u64> {
        self.open.as_ref().map(|o| o.t0)
    }

    fn effects_enabled(&self) -> bool {
        self.cells >= self.cfg.min_cells_for_effects
    }

    /// Feed a new frame observed at `t_ms`. A frame of the wrong length is ignored.
    pub fn push(&mut self, t_ms: u64, frame: &[Rgb], out: &mut Vec<SceneEvent>) {
        if frame.len() != self.cells {
            return;
        }
        self.frames += 1;
        self.first_frame_ms.get_or_insert(t_ms);
        let Some(prev) = self.prev.take() else {
            self.prev = Some(frame.to_vec());
            self.tick(t_ms, out);
            return;
        };
        let mut changed = 0usize;
        for (i, (a, b)) in frame.iter().zip(prev.iter()).enumerate() {
            if a != b {
                self.last_change[i] = Some(t_ms);
                if !self.animated[i] {
                    changed += 1;
                }
            }
        }
        if changed > 0 && self.effects_enabled() {
            if changed >= self.motion_cells {
                self.last_motion = Some(t_ms);
                let open = self.open.get_or_insert_with(|| Open {
                    t0: t_ms,
                    t_last_motion: t_ms,
                    base: prev.clone(),
                    mask: self.animated.clone(),
                    peak: vec![0; frame.len()],
                    peak_rgb: vec![(0, 0, 0); frame.len()],
                    dark: vec![false; frame.len()],
                    uni_sum: 0.0,
                    uni_frames: 0,
                    frames: 0,
                    prefix: None,
                    prefix_sig: None,
                });
                open.t_last_motion = t_ms;
            }
            if let Some(open) = self.open.as_mut() {
                let back = accumulate(open, frame, self.cfg.affect, self.cfg.baseline_eps);
                if back && open.frames > 2 {
                    self.prev = Some(frame.to_vec());
                    self.close(t_ms, out);
                    self.tick(t_ms, out);
                    return;
                }
            }
        }
        self.prev = Some(frame.to_vec());
        self.tick(t_ms, out);
    }

    /// Advance time with no new frame: closes a stale effect, makes the early guess, and
    /// re-reads the rest layer once the board has settled.
    pub fn tick(&mut self, t_ms: u64, out: &mut Vec<SceneEvent>) {
        if let Some(open) = self.open.as_ref() {
            let quiet = t_ms.saturating_sub(open.t_last_motion) > self.cfg.gap_ms;
            let long = t_ms.saturating_sub(open.t0) >= self.cfg.max_effect_ms;
            if quiet || long {
                self.close(t_ms, out);
            } else if open.prefix_sig.is_none() && t_ms.saturating_sub(open.t0) >= self.cfg.prefix_ms {
                self.early_guess(t_ms, out);
            }
        }
        if self.open.is_none() {
            let since = self.last_motion.or(self.first_frame_ms);
            let settled = since.is_some_and(|m| t_ms.saturating_sub(m) >= self.cfg.settle_ms);
            // A 4 Hz ceiling: the read is cheap, but nothing in it can change faster than that
            // in a way anyone would want reported.
            let due = self.last_rest_read.is_none_or(|r| t_ms.saturating_sub(r) >= 250);
            if settled && due && self.prev.is_some() {
                self.read_rest(t_ms, out);
            }
        }
    }

    fn early_guess(&mut self, t_ms: u64, out: &mut Vec<SceneEvent>) {
        let Some(open) = self.open.as_mut() else { return };
        let sig = open.signature(self.cfg.affect, t_ms.saturating_sub(open.t0).min(self.cfg.prefix_ms));
        let nearest = self
            .prefixes
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.signature.max_distance(&sig)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let likely = match nearest {
            Some((i, d)) if d < self.cfg.prefix_theta => {
                open.prefix = Some(i);
                self.prefixes[i].likely()
            }
            _ => None,
        };
        open.prefix_sig = Some(sig);
        out.push(SceneEvent::Burst { started_ms: open.t0, likely });
    }

    fn close(&mut self, t_ms: u64, out: &mut Vec<SceneEvent>) {
        let Some(open) = self.open.take() else { return };
        // One changed frame and then stillness is a cut to a new resting picture, not an effect:
        // the rest layer reads it once the board settles.
        if open.frames < 2 || open.t_last_motion == open.t0 {
            return;
        }
        let truncated = t_ms.saturating_sub(open.t0) >= self.cfg.max_effect_ms;
        let dur = open.t_last_motion.saturating_sub(open.t0);
        let sig = open.signature(self.cfg.affect, dur);

        let nearest = self
            .templates
            .iter()
            .enumerate()
            .map(|(i, t)| (i, t.signature.distance(&sig)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        let (id, distance, new) = match nearest {
            Some((i, d)) if d < self.cfg.match_theta => {
                let t = &mut self.templates[i];
                t.count += 1;
                t.last_ms = t_ms;
                t.signature.blend(&sig, 1.0 / t.count.min(16) as f32);
                (t.id, d, false)
            }
            other => {
                if self.templates.len() >= self.cfg.max_templates {
                    if let Some(oldest) =
                        self.templates.iter().enumerate().min_by_key(|(_, t)| t.last_ms).map(|(i, _)| i)
                    {
                        // An early guess must never name an effect that no longer exists.
                        let gone = self.templates.remove(oldest).id;
                        for p in &mut self.prefixes {
                            p.outcomes.retain(|(id, _)| *id != gone);
                        }
                    }
                }
                let id = self.next_id;
                self.next_id = self.next_id.saturating_add(1);
                self.templates.push(Template {
                    id,
                    signature: sig,
                    count: 1,
                    last_ms: t_ms,
                    swatch: open.swatch(),
                });
                (id, other.map_or(f32::INFINITY, |(_, d)| d), true)
            }
        };

        // Train the early-guess matcher on how this effect began. An effect shorter than the
        // prefix window began as all of itself.
        let prefix_sig = open.prefix_sig.unwrap_or_else(|| {
            open.signature(self.cfg.affect, dur.min(self.cfg.prefix_ms))
        });
        let slot = open.prefix.or_else(|| {
            self.prefixes
                .iter()
                .enumerate()
                .map(|(i, p)| (i, p.signature.max_distance(&prefix_sig)))
                .filter(|(_, d)| *d < self.cfg.prefix_theta)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        });
        match slot {
            Some(i) => {
                let p = &mut self.prefixes[i];
                p.count += 1;
                p.signature.blend(&prefix_sig, 1.0 / p.count.min(16) as f32);
                p.vote(id);
            }
            None if self.prefixes.len() < self.cfg.max_templates => {
                self.prefixes.push(PrefixTemplate { signature: prefix_sig, count: 1, outcomes: vec![(id, 1)] });
            }
            None => {}
        }

        out.push(SceneEvent::Effect {
            id,
            started_ms: open.t0,
            dur_ms: u32::try_from(dur).unwrap_or(u32::MAX),
            distance,
            new,
            truncated,
        });
    }

    fn read_rest(&mut self, t_ms: u64, out: &mut Vec<SceneEvent>) {
        self.last_rest_read = Some(t_ms);
        let Some(frame) = self.prev.as_ref() else { return };
        for (i, a) in self.animated.iter_mut().enumerate() {
            *a = self.last_change[i].is_some_and(|c| t_ms.saturating_sub(c) < self.cfg.settle_ms);
        }

        let mut sorted: Vec<Rgb> = frame.clone();
        sorted.sort_unstable();
        let mut best = (sorted[0], 0usize);
        let mut run = (sorted[0], 0usize);
        for &c in &sorted {
            if c == run.0 {
                run.1 += 1;
            } else {
                run = (c, 1);
            }
            if run.1 > best.1 {
                best = run;
            }
        }
        let ambient = (best.1 as f32 >= self.cfg.ambient_min_share * self.cells as f32).then_some(best.0);

        let mut roles: Vec<Role> = Vec::new();
        if let Some(amb) = ambient {
            for (i, &c) in frame.iter().enumerate() {
                if self.animated[i] || c == amb {
                    continue;
                }
                let cell = u16::try_from(i).unwrap_or(u16::MAX);
                match roles.iter_mut().find(|r| r.rgb == c) {
                    Some(r) => r.cells.push(cell),
                    None => roles.push(Role { rgb: c, cells: vec![cell] }),
                }
            }
        }
        let animated: Vec<u16> = self
            .animated
            .iter()
            .enumerate()
            .filter(|(_, a)| **a)
            .map(|(i, _)| u16::try_from(i).unwrap_or(u16::MAX))
            .collect();

        if ambient != self.rest.ambient {
            out.push(SceneEvent::Ambient { from: self.rest.ambient, to: ambient });
        }
        if roles != self.rest.roles {
            out.push(SceneEvent::Roles { count: roles.len() });
        }
        if let Some(amb) = ambient {
            self.settle_look(t_ms, amb, &roles, out);
        }
        self.rest = Rest { ambient, roles, animated };
    }
}

impl Scene {
    fn settle_look(&mut self, t_ms: u64, ambient: Rgb, roles: &[Role], out: &mut Vec<SceneEvent>) {
        let mut colours: Vec<Rgb> = roles.iter().map(|r| r.rgb).collect();
        colours.sort_unstable();
        let (id, new) = if let Some(l) = self.looks.iter_mut().find(|l| l.matches(ambient, &colours)) {
            if self.current_look != Some(l.id) {
                l.count += 1;
            }
            l.last_ms = t_ms;
            (l.id, false)
        } else {
            if self.looks.len() >= self.cfg.max_templates {
                if let Some(oldest) = self.looks.iter().enumerate().min_by_key(|(_, l)| l.last_ms).map(|(i, _)| i) {
                    self.looks.remove(oldest);
                }
            }
            let id = self.next_look;
            self.next_look = self.next_look.saturating_add(1);
            self.looks.push(Look { id, ambient, colours, count: 1, last_ms: t_ms });
            (id, true)
        };
        if self.current_look != Some(id) {
            out.push(SceneEvent::Look { id, from: self.current_look, new });
            self.current_look = Some(id);
        }
    }
}

/// Fold one frame into an open effect. Returns true when the frame is back at the pre-effect
/// picture.
fn accumulate(open: &mut Open, frame: &[Rgb], affect: u8, eps: u8) -> bool {
    open.frames += 1;
    let mut frame_max = 0u8;
    for (i, &c) in frame.iter().enumerate() {
        if open.mask[i] {
            continue;
        }
        let d = dev(c, open.base[i]);
        frame_max = frame_max.max(d);
        if d > open.peak[i] {
            open.peak[i] = d;
            open.peak_rgb[i] = c;
            open.dark[i] = sum3(c) < sum3(open.base[i]);
        }
    }
    if frame_max > affect {
        let half = frame_max / 2 + frame_max % 2;
        let lit = frame
            .iter()
            .enumerate()
            .filter(|(i, c)| !open.mask[*i] && dev(**c, open.base[*i]) >= half)
            .count();
        open.uni_sum += lit as f32 / frame.len() as f32;
        open.uni_frames += 1;
    }
    frame_max <= eps
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 22;
    const H: usize = 6;
    const N: usize = W * H;
    const BG: Rgb = (55, 30, 0);
    const WASD: Rgb = (222, 153, 0);

    fn board() -> Vec<Rgb> {
        let mut f = vec![BG; N];
        for c in [2 * W + 3, 3 * W + 2, 3 * W + 3, 3 * W + 4] {
            f[c] = WASD;
        }
        f
    }

    struct Feed {
        scene: Scene,
        t: u64,
        events: Vec<SceneEvent>,
    }

    impl Feed {
        fn new() -> Self {
            Feed { scene: Scene::new(N, SceneConfig::default()), t: 0, events: Vec::new() }
        }
        fn frame(&mut self, f: &[Rgb]) {
            self.t += 16;
            self.scene.push(self.t, f, &mut self.events);
        }
        fn idle(&mut self, ms: u64) {
            let end = self.t + ms;
            while self.t < end {
                self.t += 16;
                self.scene.tick(self.t, &mut self.events);
            }
        }
        fn effects(&self) -> Vec<(u32, bool)> {
            self.events
                .iter()
                .filter_map(|e| match e {
                    SceneEvent::Effect { id, new, .. } => Some((*id, *new)),
                    _ => None,
                })
                .collect()
        }
    }

    /// A ring expanding from `origin` at `speed` cells per frame, in `colour`.
    fn ring(feed: &mut Feed, origin: (f32, f32), colour: Rgb, frames: usize) {
        for k in 0..frames {
            let radius = k as f32 * 0.6;
            let mut f = board();
            for (i, c) in f.iter_mut().enumerate() {
                let d = ((i / W) as f32 - origin.0).hypot((i % W) as f32 - origin.1);
                if (d - radius).abs() < 1.0 {
                    *c = colour;
                }
            }
            feed.frame(&f);
        }
        feed.frame(&board());
    }

    fn flash(feed: &mut Feed, colour: Rgb, frames: usize) {
        for k in 0..frames {
            let s = 1.0 - k as f32 / frames as f32;
            let c = ((f32::from(colour.0) * s) as u8, (f32::from(colour.1) * s) as u8, (f32::from(colour.2) * s) as u8);
            feed.frame(&vec![c; N]);
        }
        feed.frame(&board());
    }

    #[test]
    fn hue_bins_cover_the_wheel() {
        assert_eq!(hue_bin((255, 0, 0)), 0);
        assert_eq!(hue_bin((0, 255, 0)), 4);
        assert_eq!(hue_bin((0, 255, 255)), 6);
        assert_eq!(hue_bin((0, 0, 255)), 8);
        assert_eq!(hue_bin((200, 200, 200)), BIN_ACHROMATIC);
        assert_eq!(hue_bin((0, 0, 0)), BIN_ACHROMATIC);
    }

    #[test]
    fn a_still_board_settles_into_ambient_and_roles() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1200);
        let rest = feed.scene.rest();
        assert_eq!(rest.ambient, Some(BG));
        assert_eq!(rest.roles.len(), 1);
        assert_eq!(rest.roles[0].rgb, WASD);
        assert_eq!(rest.roles[0].cells, vec![47, 68, 69, 70]);
        assert!(feed.events.iter().any(|e| matches!(e, SceneEvent::Ambient { to: Some(BG), .. })));
    }

    #[test]
    fn a_blinking_key_is_animated_not_a_role_or_an_effect() {
        let mut feed = Feed::new();
        for k in 0..200 {
            let mut f = board();
            if k % 30 < 15 {
                f[2 * W + 2] = (35, 216, 237);
            }
            feed.frame(&f);
        }
        let rest = feed.scene.rest();
        assert_eq!(rest.animated, vec![46]);
        assert!(rest.roles.iter().all(|r| !r.cells.contains(&46)));
        assert!(feed.effects().is_empty(), "one blinking key is not board motion");
    }

    #[test]
    fn the_same_effect_keeps_its_id_and_a_different_one_gets_a_new_id() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        for _ in 0..3 {
            ring(&mut feed, (2.5, 2.0), (35, 216, 237), 40);
            feed.idle(1500);
        }
        for _ in 0..3 {
            flash(&mut feed, (41, 218, 7), 25);
            feed.idle(900);
        }
        let fx = feed.effects();
        assert_eq!(fx.len(), 6, "{fx:?}");
        assert!(fx[0].1 && !fx[1].1 && !fx[2].1);
        assert_eq!(fx[0].0, fx[1].0);
        assert_eq!(fx[0].0, fx[2].0);
        assert_ne!(fx[3].0, fx[0].0);
        assert_eq!(fx[3].0, fx[4].0);
        assert_eq!(fx[3].0, fx[5].0);
        let ring_t = &feed.scene.templates()[0];
        let flash_t = &feed.scene.templates()[1];
        assert!(ring_t.signature.uniformity < flash_t.signature.uniformity);
        assert_eq!(bin_name(ring_t.signature.dominant_bin().unwrap_or(99)), "cyan");
    }

    #[test]
    fn the_early_guess_learns_what_an_effect_becomes() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        for _ in 0..3 {
            ring(&mut feed, (2.5, 2.0), (35, 216, 237), 40);
            feed.idle(1500);
        }
        let bursts: Vec<Option<u32>> = feed
            .events
            .iter()
            .filter_map(|e| match e {
                SceneEvent::Burst { likely, .. } => Some(*likely),
                _ => None,
            })
            .collect();
        assert_eq!(bursts.len(), 3);
        assert_eq!(bursts[0], None, "nothing to guess from yet");
        assert_eq!(bursts[2], Some(feed.effects()[0].0));
    }

    #[test]
    fn a_never_ending_effect_is_cut_and_marked() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        for k in 0..600u32 {
            let v = (k % 200) as u8;
            feed.frame(&vec![(v, 255 - v, 0); N]);
        }
        assert!(feed.events.iter().any(|e| matches!(e, SceneEvent::Effect { truncated: true, .. })));
    }

    #[test]
    fn a_tiny_grid_reads_rest_but_never_effects() {
        let mut scene = Scene::new(3, SceneConfig::default());
        let mut out = Vec::new();
        for k in 0..300u64 {
            let c = if k % 20 < 10 { (0, 61, 111) } else { (255, 255, 42) };
            scene.push(k * 16, &[c, c, c], &mut out);
        }
        assert!(out.iter().all(|e| !matches!(e, SceneEvent::Effect { .. } | SceneEvent::Burst { .. })));
    }

    #[test]
    fn a_rainbow_idle_has_no_ambient() {
        let mut feed = Feed::new();
        let f: Vec<Rgb> = (0..N).map(|i| ((i * 2) as u8, 255 - (i * 2) as u8, (i % 7 * 30) as u8)).collect();
        feed.frame(&f);
        feed.idle(1200);
        assert_eq!(feed.scene.rest().ambient, None);
        assert!(feed.scene.rest().roles.is_empty());
    }

    #[test]
    fn the_template_table_is_bounded() {
        let cfg = SceneConfig { max_templates: 4, ..SceneConfig::default() };
        let mut feed = Feed { scene: Scene::new(N, cfg), t: 0, events: Vec::new() };
        feed.frame(&board());
        feed.idle(1100);
        for h in 0..8u8 {
            let colour = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0), (255, 0, 255), (0, 255, 255), (255, 255, 255), (128, 0, 255)][usize::from(h)];
            flash(&mut feed, colour, 10 + usize::from(h) * 12);
            feed.idle(900);
        }
        assert!(feed.scene.templates().len() <= 4);
        let ids: Vec<u32> = feed.scene.templates().iter().map(|t| t.id).collect();
        assert!(ids.iter().all(|&id| id >= 4), "the oldest were evicted: {ids:?}");
    }

    const OW_SCENE: &[u8] = include_bytes!("chroma_shm_data/overwatch-keyboard-scene.bin");

    /// Decode the delta-stream fixture into `(t_ms, frame)` pairs.
    fn ow_frames() -> Vec<(u64, Vec<Rgb>)> {
        assert_eq!(&OW_SCENE[..4], b"NCS1");
        let cells = usize::from(u16::from_le_bytes([OW_SCENE[4], OW_SCENE[5]]));
        let mut frame = vec![(0, 0, 0); cells];
        let mut out = Vec::new();
        let mut p = 6;
        while p < OW_SCENE.len() {
            let t = u32::from_le_bytes([OW_SCENE[p], OW_SCENE[p + 1], OW_SCENE[p + 2], OW_SCENE[p + 3]]);
            let n = usize::from(u16::from_le_bytes([OW_SCENE[p + 4], OW_SCENE[p + 5]]));
            p += 6;
            for q in OW_SCENE[p..p + 4 * n].chunks_exact(4) {
                frame[usize::from(q[0])] = (q[1], q[2], q[3]);
            }
            p += 4 * n;
            out.push((u64::from(t), frame.clone()));
        }
        out
    }

    /// Replays real Overwatch play: the ult wave (radial from Q) must keep one id across hero
    /// select and three plays in a later match, green pulses must get their own id, and both
    /// match ambients and the match-2 key roles must be read.
    #[test]
    fn real_overwatch_play_groups_into_stable_templates() {
        let mut scene = Scene::new(N, SceneConfig::default());
        let mut events = Vec::new();
        let mut roles_seen: Vec<Rest> = Vec::new();
        let mut last = 0u64;
        for (t, f) in ow_frames() {
            let mut now = last + 16;
            while now < t && now < last + 3000 {
                scene.tick(now, &mut events);
                now += 16;
            }
            let before = events.len();
            scene.push(t, &f, &mut events);
            if events[before..].iter().any(|e| matches!(e, SceneEvent::Roles { .. })) {
                roles_seen.push(scene.rest().clone());
            }
            last = t;
        }
        scene.tick(last + 3000, &mut events);

        let effect_at = |at: u64| -> Option<u32> {
            events.iter().find_map(|e| match *e {
                SceneEvent::Effect { id, started_ms, .. } if started_ms.abs_diff(at) < 200 => Some(id),
                _ => None,
            })
        };
        let ult: Vec<Option<u32>> = [20_300, 389_900, 409_600, 434_900].map(effect_at).to_vec();
        let pulse: Vec<Option<u32>> = [50_840, 51_400, 52_130, 52_720].map(effect_at).to_vec();
        assert!(ult.iter().all(|id| id.is_some() && *id == ult[0]), "ult waves: {ult:?} in {events:?}");
        assert!(pulse.iter().all(|id| id.is_some() && *id == pulse[0]), "green pulses: {pulse:?}");
        assert_ne!(ult[0], pulse[0]);

        let ambients: Vec<Rgb> = events
            .iter()
            .filter_map(|e| match e {
                SceneEvent::Ambient { to: Some(c), .. } => Some(*c),
                _ => None,
            })
            .collect();
        assert!(ambients.contains(&(49, 14, 10)), "match 1 ambient: {ambients:?}");
        assert!(ambients.contains(&(55, 30, 0)), "match 2 ambient: {ambients:?}");

        let has_role = |rgb: Rgb, cells: usize| {
            roles_seen.iter().any(|r| r.roles.iter().any(|role| role.rgb == rgb && role.cells.len() == cells))
        };
        assert!(has_role((222, 153, 0), 4), "WASD role: {roles_seen:?}");
        assert!(has_role((4, 141, 144), 2), "E + Shift role: {roles_seen:?}");

        // Each match-2 window settles back into the same look.
        let a = scene.looks().iter().find(|l| l.ambient == (55, 30, 0)).expect("the match-2 look");
        assert!(a.count >= 2, "match 2's look was settled into again after each ult: {:?}", scene.looks());
    }

    #[test]
    fn templates_survive_a_new_session_with_their_ids() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        ring(&mut feed, (2.5, 2.0), (35, 216, 237), 40);
        feed.idle(1500);
        let saved = feed.scene.templates().to_vec();
        let text = serde_json::to_string(&saved).expect("templates serialize");
        let loaded: Vec<Template> = serde_json::from_str(&text).expect("templates deserialize");

        let mut next = Feed { scene: Scene::with_templates(N, SceneConfig::default(), loaded), t: 0, events: Vec::new() };
        next.frame(&board());
        next.idle(1100);
        ring(&mut next, (2.5, 2.0), (35, 216, 237), 40);
        next.idle(1500);
        flash(&mut next, (41, 218, 7), 25);
        next.idle(1500);
        let fx = next.effects();
        assert_eq!(fx[0], (saved[0].id, false), "the known effect keeps its id: {fx:?}");
        assert!(fx[1].1 && fx[1].0 > saved[0].id, "a new effect gets a fresh id");
    }

    #[test]
    fn the_baseline_is_the_picture_before_the_effect() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        let mut f = board();
        for c in f.iter_mut().take(40) {
            *c = (0, 255, 255);
        }
        feed.frame(&f);
        assert_eq!(feed.scene.baseline(), Some(board().as_slice()));
        assert_eq!(feed.scene.open_swatch(), Some((0, 255, 255)));
    }

    #[test]
    fn a_cut_to_a_new_resting_picture_is_not_an_effect() {
        let mut feed = Feed::new();
        feed.frame(&board());
        feed.idle(1100);
        let mut next = board();
        for c in next.iter_mut().filter(|c| **c == BG) {
            *c = (98, 28, 21);
        }
        feed.frame(&next);
        // the game keeps writing a blinking key over the new picture
        for k in 0..60 {
            let mut f = next.clone();
            if k % 20 < 10 {
                f[46] = (35, 216, 237);
            }
            feed.frame(&f);
        }
        feed.idle(1500);
        assert!(feed.effects().is_empty(), "{:?}", feed.events);
        assert_eq!(feed.scene.rest().ambient, Some((98, 28, 21)));
    }

    #[test]
    fn resting_pictures_become_looks_that_keep_their_ids() {
        let looks = |feed: &Feed| -> Vec<(u32, Option<u32>, bool)> {
            feed.events
                .iter()
                .filter_map(|e| match e {
                    SceneEvent::Look { id, from, new } => Some((*id, *from, *new)),
                    _ => None,
                })
                .collect()
        };
        let menu: Vec<Rgb> = vec![(50, 51, 52); N];
        let mut feed = Feed::new();
        feed.frame(&menu);
        feed.idle(1200);
        feed.frame(&board());
        feed.idle(1200);
        feed.frame(&menu);
        feed.idle(1200);
        assert_eq!(looks(&feed), vec![(0, None, true), (1, Some(0), true), (0, Some(1), false)]);
        assert_eq!(feed.scene.looks()[0].count, 2, "the menu was entered twice");
        assert_eq!(feed.scene.looks()[1].colours, vec![WASD]);

        let known = feed.scene.looks().to_vec();
        let text = serde_json::to_string(&known).expect("looks serialize");
        let mut next = Feed { scene: Scene::new(N, SceneConfig::default()).with_looks(serde_json::from_str(&text).expect("looks parse")), t: 0, events: Vec::new() };
        next.frame(&board());
        next.idle(1200);
        assert_eq!(looks(&next), vec![(1, None, false)], "a known look keeps its id next session");
    }

    #[test]
    fn a_rainbow_idle_is_not_a_look() {
        let mut feed = Feed::new();
        let f: Vec<Rgb> = (0..N).map(|i| ((i * 2) as u8, 255 - (i * 2) as u8, (i % 7 * 30) as u8)).collect();
        feed.frame(&f);
        feed.idle(1200);
        assert!(feed.scene.looks().is_empty());
    }

    #[test]
    fn a_wrong_sized_frame_is_ignored() {
        let mut scene = Scene::new(N, SceneConfig::default());
        let mut out = Vec::new();
        scene.push(0, &[(1, 2, 3); 5], &mut out);
        assert_eq!(scene.frames(), 0);
        assert!(scene.frame().is_none());
    }
}
