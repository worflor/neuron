// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Spectrum — the COLOUR PROGRAM half of the Spectrum lighting model (a layer = PATTERN × SPECTRUM).
//!
//! A [`Pattern`](crate::pattern::Pattern) emits, per cell, a sample coordinate `u` (0..1) and an
//! `intensity` (0..1) — it knows nothing about colour. A **Spectrum** is the other half: it maps
//! `(t, u) -> Rgb`. The render pipeline is `spectrum.at(t, u).scale_f(intensity)` per cell.
//!
//! A Spectrum is a palette + motion + an optional sequenced timeline:
//!
//! ```text
//! Spectrum { seq: Vec<Frame>, play: Loop }          // len 1 = no sequence
//! Frame    { palette: Palette, hold: f32, fade: f32, ease: Ease }   // dwell secs + crossfade-in secs
//! Palette  { stops: Vec<Stop>, motion: Motion }     // gradient + intra-frame animation
//! Stop     { col: Rgb, at: f32 }                     // 0..1 position; 1 stop = solid
//! Motion   = Hold | Drift{speed} | Cycle{speed} | Breathe{speed,depth} | Flow{speed,chaos}
//! Loop     = Once | Loop | PingPong
//! Ease     = Linear | Smooth | Snap
//! ```
//!
//! [`Spectrum::at`] is fully N-generic (any number of stops, any number of frames; O(stops) per cell)
//! and resolves: the sequence position under `play` -> the active frame + its eased crossfade with the
//! previous frame -> the active palette's `Motion` at `t` -> an N-stop lerp at `u` -> blend the two
//! frames by the eased fade factor.
//!
//! ## Serde — tightest tiered format (fresh, no legacy)
//! A Spectrum serialises to the SMALLEST shape that captures it (so most layers stay TOML-flat):
//!   * **solid** (1 stop, no motion, 1 frame) -> a bare hex string `"RRGGBB"`,
//!   * **static gradient** (≥2 evenly-spaced stops, no motion, 1 frame) -> an array of hex,
//!   * **+motion / uneven stops** (1 frame) -> a palette table `{ stops, motion, speed, .. }`,
//!   * **sequence** (>1 frame) -> a keyframe table `{ play, seq = [ frame tables ] }`.
//!
//! Deserialisation accepts all four shapes. `Rgb` itself is a hex string (the project-wide convention).

use crate::lighting::Rgb;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::f32::consts::TAU;

// ───────────────────────────────────────── enums ─────────────────────────────────────────

/// Easing applied to a frame's crossfade-in factor (and reusable wherever a 0..1 ramp is shaped).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Ease {
    /// `x` — a straight linear crossfade.
    #[default]
    Linear,
    /// smoothstep `x²(3−2x)` — eased in and out.
    Smooth,
    /// a hard cut at the midpoint — no visible blend (an instant frame switch).
    Snap,
}

impl Ease {
    /// Shape a 0..1 factor by this ease. Always returns 0..1.
    #[must_use]
    pub fn apply(self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            Ease::Linear => x,
            Ease::Smooth => x * x * (3.0 - 2.0 * x),
            Ease::Snap => {
                if x >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Ease::Linear => "linear",
            Ease::Smooth => "smooth",
            Ease::Snap => "snap",
        }
    }

    /// Parse a tag, defaulting to [`Ease::Linear`] for anything unrecognised.
    #[must_use]
    #[allow(clippy::should_implement_trait)] // Unknown names preserve the default easing.
    pub fn from_str(s: &str) -> Ease {
        match s.to_ascii_lowercase().as_str() {
            "smooth" => Ease::Smooth,
            "snap" => Ease::Snap,
            _ => Ease::Linear,
        }
    }
}

/// How a sequence advances once `t` passes its total duration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Loop {
    /// Clamp at the last frame (play once, then hold the end).
    Once,
    /// Wrap back to the start (the common case).
    #[default]
    Loop,
    /// Reflect — forward then backward, forever.
    PingPong,
}

impl Loop {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Loop::Once => "once",
            Loop::Loop => "loop",
            Loop::PingPong => "pingpong",
        }
    }

    /// Parse a tag, defaulting to [`Loop::Loop`] for anything unrecognised.
    #[must_use]
    #[allow(clippy::should_implement_trait)] // Unknown names preserve the default loop mode.
    pub fn from_str(s: &str) -> Loop {
        match s.to_ascii_lowercase().as_str() {
            "once" => Loop::Once,
            "pingpong" | "ping-pong" => Loop::PingPong,
            _ => Loop::Loop,
        }
    }
}

/// How a [`Palette`] interpolates COLOUR between its stops. The default is raw RGB (predictable,
/// preserves every existing preset's look); [`Interp::Hsv`] is the opt-in perceptual path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Interp {
    /// Linear RGB lerp between stops — predictable and cheap, but a red→blue blend sags through a
    /// muddy, desaturated purple at the midpoint. The default so no preset's look changes.
    #[default]
    Rgb,
    /// HSV interpolation taking the SHORTEST hue path between stops (so red→blue rounds through vivid
    /// magenta, never grey) with saturation/value lerped linearly — vivid like the built-in rainbow.
    Hsv,
}

impl Interp {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Interp::Rgb => "rgb",
            Interp::Hsv => "hsv",
        }
    }

    /// Parse a tag, defaulting to [`Interp::Rgb`] for anything unrecognised.
    #[must_use]
    #[allow(clippy::should_implement_trait)] // Unknown names preserve the default interpolation.
    pub fn from_str(s: &str) -> Interp {
        match s.to_ascii_lowercase().as_str() {
            "hsv" => Interp::Hsv,
            _ => Interp::Rgb,
        }
    }
}

/// Intra-frame palette animation — applied at time `t` before the stops are sampled.
#[derive(Clone, Copy, Debug, PartialEq)]
#[derive(Default)]
pub enum Motion {
    /// Static — the palette is sampled as-is.
    #[default]
    Hold,
    /// Slide the stop positions along `u` over time (a scrolling gradient).
    Drift { speed: f32 },
    /// Rotate the sampled colour's HUE through the wheel over time.
    Cycle { speed: f32 },
    /// Modulate brightness up and down over time. `depth` 0..1 is the dip amount.
    Breathe { speed: f32, depth: f32 },
    /// Organic multi-octave drift of `u` (a living, non-repeating flow). `chaos` 0..1 adds octaves.
    Flow { speed: f32, chaos: f32 },
}


impl Motion {
    /// The stable tag (`"hold"`, `"drift"`, …).
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Motion::Hold => "hold",
            Motion::Drift { .. } => "drift",
            Motion::Cycle { .. } => "cycle",
            Motion::Breathe { .. } => "breathe",
            Motion::Flow { .. } => "flow",
        }
    }

    /// The motion rate (0 for [`Motion::Hold`]).
    #[must_use]
    pub fn speed(&self) -> f32 {
        match *self {
            Motion::Hold => 0.0,
            Motion::Drift { speed }
            | Motion::Cycle { speed }
            | Motion::Breathe { speed, .. }
            | Motion::Flow { speed, .. } => speed,
        }
    }

    /// Build a motion from its parts — the data-driven constructor the serde layer and the (phase-3)
    /// motion editor share. Unknown `kind` -> [`Motion::Hold`]; `depth`/`chaos` fall back to a sane
    /// default when the chosen motion needs one.
    #[must_use]
    pub fn from_parts(kind: &str, speed: f32, depth: Option<f32>, chaos: Option<f32>) -> Motion {
        match kind.to_ascii_lowercase().as_str() {
            "drift" => Motion::Drift { speed },
            "cycle" => Motion::Cycle { speed },
            "breathe" => Motion::Breathe {
                speed,
                depth: depth.unwrap_or(0.5),
            },
            "flow" => Motion::Flow {
                speed,
                chaos: chaos.unwrap_or(0.5),
            },
            _ => Motion::Hold,
        }
    }
}

// ─────────────────────────────────────── core structs ────────────────────────────────────

/// One colour stop in a palette gradient: a colour at a 0..1 position. A single stop = a solid colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stop {
    pub col: Rgb,
    pub at: f32,
}

impl Stop {
    #[must_use]
    pub fn new(col: Rgb, at: f32) -> Stop {
        Stop { col, at }
    }
}

/// A gradient (its stops) plus an intra-frame [`Motion`] and an [`Interp`] colour space. Stops are
/// kept sorted ascending by `at` (the constructors and the serde layer guarantee it); [`Palette::at`]
/// assumes that invariant. `interp` defaults to RGB; set it to [`Interp::Hsv`] for a vivid gradient.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub stops: Vec<Stop>,
    pub motion: Motion,
    pub interp: Interp,
}

impl Palette {
    /// A solid single-colour palette (one stop, no motion).
    #[must_use]
    pub fn solid(c: Rgb) -> Palette {
        Palette {
            stops: vec![Stop::new(c, 0.0)],
            motion: Motion::Hold,
            interp: Interp::Rgb,
        }
    }

    /// An evenly-spaced static gradient from a colour list (stop `i` sits at `i/(n-1)`).
    #[must_use]
    pub fn gradient(cols: Vec<Rgb>) -> Palette {
        let n = cols.len();
        let stops = cols
            .into_iter()
            .enumerate()
            .map(|(i, c)| Stop::new(c, even_pos(i, n)))
            .collect();
        Palette {
            stops,
            motion: Motion::Hold,
            interp: Interp::Rgb,
        }
    }

    /// An arbitrary palette — the stops are sorted on construction so the sampler's bracket scan holds.
    #[must_use]
    pub fn new(mut stops: Vec<Stop>, motion: Motion) -> Palette {
        stops.sort_by(|a, b| a.at.total_cmp(&b.at));
        Palette {
            stops,
            motion,
            interp: Interp::Rgb,
        }
    }

    /// Sample the stops at `u` (0..1) — an N-generic lerp between the bracketing stops, in the palette's
    /// [`Interp`] space. 0 stops -> black; 1 stop -> that colour; `u` below the first / above the last
    /// clamps to the end stop. Cheap: O(stops). RGB interp keeps gradients predictable; HSV interp takes
    /// the shortest hue path for vivid blends. (Hue MOTION over time still lives in [`Motion::Cycle`].)
    #[must_use]
    pub fn sample(&self, u: f32) -> Rgb {
        let stops = &self.stops;
        match stops.len() {
            0 => Rgb::BLACK,
            1 => stops[0].col,
            _ => {
                let u = u.clamp(0.0, 1.0);
                let last = stops.len() - 1;
                if u <= stops[0].at {
                    return stops[0].col;
                }
                if u >= stops[last].at {
                    return stops[last].col;
                }
                for w in stops.windows(2) {
                    let (a, b) = (w[0], w[1]);
                    if u >= a.at && u <= b.at {
                        let span = b.at - a.at;
                        let f = if span > 1e-6 { (u - a.at) / span } else { 0.0 };
                        return blend_stops(a.col, b.col, f, self.interp);
                    }
                }
                stops[last].col
            }
        }
    }

    /// Sample the palette at `(t, u)`, applying its [`Motion`]. This is the per-frame colour program:
    /// Drift/Flow shift the lookup `u`; Cycle rotates the sampled hue; Breathe modulates brightness.
    #[must_use]
    pub fn at(&self, t: f32, u: f32) -> Rgb {
        match self.motion {
            Motion::Hold => self.sample(u),
            Motion::Drift { speed } => self.sample((u + t * speed).rem_euclid(1.0)),
            Motion::Cycle { speed } => {
                let col = self.sample(u);
                // reuse the comet hue-rotate (no-ops on a near-grey colour, which has no hue to turn)
                crate::effects::jitter_hue(col, (t * speed * 360.0).rem_euclid(360.0))
            }
            Motion::Breathe { speed, depth } => {
                let col = self.sample(u);
                let d = depth.clamp(0.0, 1.0);
                // asymmetric breathe (quick inhale, crest hold, long relax) dip in [1-d, 1]: full
                // bright at the crest, dimmed by `depth` at the trough. Phase is shifted a third of a
                // cycle so the crest (not the zero-crossing) sits at t=0, as the old cos did — but the
                // shape's positive-shifted mean means this reads a touch brighter overall than cosine.
                let f = 1.0 - d * 0.5 * (1.0 - crate::effects::breathe_shape(t * TAU * speed + TAU / 3.0));
                col.scale_f(f)
            }
            Motion::Flow { speed, chaos } => self.sample(flow_u(u, t, speed, chaos)),
        }
    }

    // ── editing (the gradient-strip editor) — keep the stops sorted ascending by `at` ──────────

    /// Insert a stop at position `at` (0..1), its colour SAMPLED from the gradient there (so a new stop
    /// lands on the existing curve), and return its index. Keeps the stops sorted.
    pub fn add_stop(&mut self, at: f32) -> usize {
        let at = at.clamp(0.0, 1.0);
        let col = self.sample(at);
        let pos = self.stops.partition_point(|s| s.at <= at);
        self.stops.insert(pos, Stop::new(col, at));
        pos
    }

    /// Remove the stop at `idx` — a no-op that keeps a palette from ever dropping below ONE stop (a
    /// palette with no stops is meaningless; a single stop is a solid).
    pub fn remove_stop(&mut self, idx: usize) {
        if self.stops.len() > 1 && idx < self.stops.len() {
            self.stops.remove(idx);
        }
    }

    /// Recolour the stop at `idx` (no-op if out of range).
    pub fn set_stop_color(&mut self, idx: usize, col: Rgb) {
        if let Some(s) = self.stops.get_mut(idx) {
            s.col = col;
        }
    }

    /// Reposition the stop at `idx` to `at` (0..1), re-sorting, and return its NEW index (so the editor
    /// can keep the moved stop selected).
    pub fn move_stop(&mut self, idx: usize, at: f32) -> usize {
        if idx >= self.stops.len() {
            return idx;
        }
        let mut s = self.stops.remove(idx);
        s.at = at.clamp(0.0, 1.0);
        let pos = self.stops.partition_point(|x| x.at <= s.at);
        self.stops.insert(pos, s);
        pos
    }
}

/// A keyframe in a sequenced spectrum: a palette shown for `hold` seconds after crossfading IN from
/// the previous frame over `fade` seconds (eased by `ease`).
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub palette: Palette,
    pub hold: f32,
    pub fade: f32,
    pub ease: Ease,
}

impl Frame {
    /// A frame holding `palette` with no crossfade (the single-frame default).
    #[must_use]
    pub fn new(palette: Palette) -> Frame {
        Frame {
            palette,
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        }
    }

    /// A solid single-colour frame.
    #[must_use]
    pub fn solid(c: Rgb) -> Frame {
        Frame::new(Palette::solid(c))
    }
}

/// The colour program: a sequence of [`Frame`]s played under a [`Loop`] policy. `seq.len() == 1` is
/// the common "no sequence" case (just a palette). Build via the constructors and sample with [`at`].
///
/// [`at`]: Spectrum::at
#[derive(Clone, Debug, PartialEq)]
pub struct Spectrum {
    pub seq: Vec<Frame>,
    pub play: Loop,
}

impl Default for Spectrum {
    fn default() -> Self {
        Spectrum::solid(Rgb::new(0x4A, 0xF2, 0xB0))
    }
}

impl Spectrum {
    /// A solid single-colour spectrum.
    #[must_use]
    pub fn solid(c: Rgb) -> Spectrum {
        Spectrum {
            seq: vec![Frame::solid(c)],
            play: Loop::Loop,
        }
    }

    /// True when this is a single solid colour (one frame, one Hold stop) — the "colour-driven" case a
    /// single-colour override (CLI `--color`, a colour-knob layer) should repaint.
    #[must_use]
    pub fn is_solid(&self) -> bool {
        self.seq.len() == 1
            && self.seq[0].palette.motion == Motion::Hold
            && self.seq[0].palette.stops.len() == 1
    }

    /// Re-tint: every stop becomes `c`, but motion, stop positions, and the whole sequence are PRESERVED.
    /// So a `breathing` look (uniform pattern + a Breathe spectrum) recolours to a *breathing* `c`, not a
    /// flat static one. The Synapse importer uses this for single-colour effects so their animation
    /// survives the colour swap — replacing the whole spectrum with `solid(c)` silently dropped the only
    /// motion a breathing layer had.
    #[must_use]
    pub fn recolored(&self, c: Rgb) -> Spectrum {
        let mut s = self.clone();
        for f in &mut s.seq {
            for stop in &mut f.palette.stops {
                stop.col = c;
            }
        }
        s
    }

    /// A static evenly-spaced gradient.
    #[must_use]
    pub fn gradient(cols: Vec<Rgb>) -> Spectrum {
        Spectrum {
            seq: vec![Frame::new(Palette::gradient(cols))],
            play: Loop::Loop,
        }
    }

    /// A single-frame spectrum wrapping an arbitrary palette (stops + motion).
    #[must_use]
    pub fn from_palette(p: Palette) -> Spectrum {
        Spectrum {
            seq: vec![Frame::new(p)],
            play: Loop::Loop,
        }
    }

    /// A sequenced spectrum (a timeline of frames) under a play policy. An empty `frames` collapses
    /// to a black solid so the sampler always has something to show.
    #[must_use]
    pub fn sequence(frames: Vec<Frame>, play: Loop) -> Spectrum {
        if frames.is_empty() {
            return Spectrum::solid(Rgb::BLACK);
        }
        Spectrum { seq: frames, play }
    }

    /// Sample the colour at animation time `t` (seconds) for the spectrum coordinate `u` (0..1).
    ///
    /// Resolution: total duration `Σ(hold+fade)` -> a forward position under `play` (Once clamps, Loop
    /// wraps, `PingPong` reflects) -> the active frame + the eased crossfade with the previous frame
    /// during its `fade` window -> each frame's palette is sampled WITH its motion at `t` -> the two are
    /// blended by the eased fade factor. Fully N-generic; O(stops) per call.
    #[must_use]
    pub fn at(&self, t: f32, u: f32) -> Rgb {
        let n = self.seq.len();
        if n == 0 {
            return Rgb::BLACK;
        }
        if n == 1 {
            return self.seq[0].palette.at(t, u);
        }
        // total cycle duration (each frame occupies hold + fade seconds)
        let dur = |f: &Frame| f.hold.max(0.0) + f.fade.max(0.0);
        let total: f32 = self.seq.iter().map(dur).sum();
        if total <= 1e-6 {
            // degenerate timing (all-zero durations) — nothing to advance through; show the first frame.
            return self.seq[0].palette.at(t, u);
        }
        // map t -> a forward position p in [0, total]
        let p = match self.play {
            Loop::Once => t.clamp(0.0, total),
            Loop::Loop => t.rem_euclid(total),
            Loop::PingPong => {
                let q = t.rem_euclid(2.0 * total);
                if q <= total {
                    q
                } else {
                    2.0 * total - q
                }
            }
        };
        // find the active frame i and the local time within it. p == total (Once at the end) falls
        // through the loop to the last frame, fully held (local = its duration), so no crossfade.
        let mut acc = 0.0;
        let mut i = n - 1;
        let mut local = dur(&self.seq[n - 1]);
        for (idx, f) in self.seq.iter().enumerate() {
            let d = dur(f);
            if p < acc + d {
                i = idx;
                local = p - acc;
                break;
            }
            acc += d;
        }
        let frame = &self.seq[i];
        let this = frame.palette.at(t, u);
        let fade = frame.fade.max(0.0);
        if fade > 1e-6 && local < fade {
            // the previous frame to crossfade in FROM. Loop wraps (frame 0's prev is the last frame) so
            // the loop seam is seamless; Once/PingPong have no wrap at frame 0 (it simply appears).
            let prev_idx = match self.play {
                Loop::Loop => Some((i + n - 1) % n),
                Loop::Once | Loop::PingPong => i.checked_sub(1),
            };
            if let Some(pi) = prev_idx {
                let prev = self.seq[pi].palette.at(t, u);
                let f = frame.ease.apply(local / fade);
                return Rgb::lerp(prev, this, f);
            }
        }
        this
    }
}

/// A 7-stop full-circle rainbow — a convenient default for spatial patterns (Axis/Radial).
#[must_use]
pub fn rainbow() -> Spectrum {
    Spectrum::gradient(vec![
        Rgb::from_hsv(0.0, 1.0, 1.0),
        Rgb::from_hsv(60.0, 1.0, 1.0),
        Rgb::from_hsv(120.0, 1.0, 1.0),
        Rgb::from_hsv(180.0, 1.0, 1.0),
        Rgb::from_hsv(240.0, 1.0, 1.0),
        Rgb::from_hsv(300.0, 1.0, 1.0),
        Rgb::from_hsv(360.0, 1.0, 1.0),
    ])
}

// ─────────────────────────────────────── helpers ─────────────────────────────────────────

/// Blend two stop colours by factor `f` (0..1) in the chosen [`Interp`] space — the one place the
/// gradient interpolation lives (the sampler calls it per bracket).
fn blend_stops(a: Rgb, b: Rgb, f: f32, interp: Interp) -> Rgb {
    match interp {
        Interp::Rgb => Rgb::lerp(a, b, f),
        Interp::Hsv => hsv_lerp_short(a, b, f),
    }
}

/// Decompose an [`Rgb`] into (hue°, saturation 0..1, value 0..1). Hue reuses [`rgb_hue`](crate::effects::rgb_hue)
/// (which falls back to a stable aurora-green for an achromatic colour, so the short-path stays defined).
fn rgb_to_hsv(c: Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let s = if max <= 0.0 { 0.0 } else { (max - min) / max };
    (crate::effects::rgb_hue(c), s, max)
}

/// Interpolate two colours in HSV, taking the SHORTEST way round the hue wheel (so red↔blue rounds
/// through magenta, not through the muddy grey an RGB lerp produces). Saturation/value lerp linearly.
fn hsv_lerp_short(a: Rgb, b: Rgb, f: f32) -> Rgb {
    let f = f.clamp(0.0, 1.0);
    let (ha, sa, va) = rgb_to_hsv(a);
    let (hb, sb, vb) = rgb_to_hsv(b);
    // shortest signed hue delta in (-180, 180], so we cross the nearer arc of the wheel.
    let mut dh = hb - ha;
    if dh > 180.0 {
        dh -= 360.0;
    } else if dh < -180.0 {
        dh += 360.0;
    }
    let h = (ha + dh * f).rem_euclid(360.0);
    let s = sa + (sb - sa) * f;
    let v = va + (vb - va) * f;
    Rgb::from_hsv(h, s, v)
}

/// The even 0..1 position of stop `i` of `n` (stop 0 at 0.0, stop n-1 at 1.0; a lone stop at 0.0).
fn even_pos(i: usize, n: usize) -> f32 {
    if n <= 1 {
        0.0
    } else {
        i as f32 / (n - 1) as f32
    }
}

/// Organic multi-octave `u`-drift for [`Motion::Flow`]: one base octave (always on) plus two finer
/// octaves scaled by `chaos`, wrapped into 0..1. Bounded so it stays a gentle wander, not a jump.
fn flow_u(u: f32, t: f32, speed: f32, chaos: f32) -> f32 {
    let c = chaos.clamp(0.0, 1.0);
    let o1 = (t * speed * TAU * 0.50 + u * TAU).sin();
    let o2 = (t * speed * TAU * 1.13 + u * TAU * 2.0).sin();
    let o3 = (t * speed * TAU * 0.27 + u * TAU * 3.0).sin();
    let drift = (o1 * 0.5 + o2 * 0.3 * c + o3 * 0.2 * c) * 0.5; // ~±0.5
    (u + drift).rem_euclid(1.0)
}

/// True when the stops are (within epsilon) evenly spaced 0..1 — i.e. representable by the bare hex
/// ARRAY tier (positions implied), so the serde layer can drop explicit positions.
fn stops_are_even(stops: &[Stop]) -> bool {
    let n = stops.len();
    if n < 2 {
        return false; // 1 stop is the SOLID tier, not the gradient tier
    }
    stops
        .iter()
        .enumerate()
        .all(|(i, s)| (s.at - even_pos(i, n)).abs() < 1e-4)
}

// ─────────────────────────────────── tiered serde ───────────────────────────────────────
//
// A Spectrum serialises to the smallest shape that captures it and deserialises from any of the four.
// `Spectrum`'s own Serialize/Deserialize just delegate to these derive-able repr types.

/// One stop on the wire: a bare hex string (even-array context) OR a `{ col, at }` table (explicit
/// position). The palette/sequence tiers always EMIT the table form (lossless positions); both are
/// accepted on read.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StopRepr {
    Hex(String),
    Full { col: String, at: f32 },
}

/// A spectrum table — covers BOTH the single-frame palette tier (`stops`/`motion`/…) and the sequence
/// tier (`play`/`seq`). The presence of `seq` selects which.
#[derive(Serialize, Deserialize, Default)]
struct SpectrumTable {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stops: Vec<StopRepr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    motion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    speed: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depth: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chaos: Option<f32>,
    /// the gradient interpolation space — emitted only when non-default (`"hsv"`); `None`/absent = RGB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    play: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    seq: Option<Vec<FrameTable>>,
}

/// One frame on the wire (the sequence tier): a palette (stops + motion) plus its timing.
#[derive(Serialize, Deserialize)]
struct FrameTable {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    stops: Vec<StopRepr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    motion: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    speed: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    depth: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chaos: Option<f32>,
    /// the gradient interpolation space — emitted only when non-default (`"hsv"`); `None`/absent = RGB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interp: Option<String>,
    #[serde(default = "one")]
    hold: f32,
    #[serde(default)]
    fade: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ease: Option<String>,
}

fn one() -> f32 {
    1.0
}

/// The four on-the-wire shapes a Spectrum can take, dispatched untagged by JSON/TOML type
/// (string -> Solid, array -> Gradient, table -> Table). The single source for both directions.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SpectrumRepr {
    Solid(String),
    Gradient(Vec<String>),
    Table(SpectrumTable),
}

// --- conversions: Spectrum <-> repr ---

fn stop_to_repr(s: &Stop) -> StopRepr {
    StopRepr::Full {
        col: s.col.to_hex(),
        at: s.at,
    }
}

/// The wire tag for a palette's [`Interp`] — `None` for the RGB default (so it serialises nothing),
/// `Some("hsv")` only when set. Keeps a default-RGB palette from ever growing an `interp` field.
fn interp_tag(i: Interp) -> Option<String> {
    match i {
        Interp::Rgb => None,
        Interp::Hsv => Some(i.as_str().to_string()),
    }
}

/// (motion-tag, speed, depth, chaos) for the wire — `Hold` emits nothing.
fn motion_parts(m: &Motion) -> (Option<String>, Option<f32>, Option<f32>, Option<f32>) {
    match *m {
        Motion::Hold => (None, None, None, None),
        Motion::Drift { speed } => (Some("drift".into()), Some(speed), None, None),
        Motion::Cycle { speed } => (Some("cycle".into()), Some(speed), None, None),
        Motion::Breathe { speed, depth } => (Some("breathe".into()), Some(speed), Some(depth), None),
        Motion::Flow { speed, chaos } => (Some("flow".into()), Some(speed), None, Some(chaos)),
    }
}

/// Parse wire stops into sorted [`Stop`]s. All-hex (no positions) -> evenly distributed; any explicit
/// position -> use it (a missing one defaults to 0.0). Clamped to 0..1 and sorted ascending.
fn parse_stops(reprs: Vec<StopRepr>) -> Result<Vec<Stop>, String> {
    let mut tmp: Vec<(Option<f32>, Rgb)> = Vec::with_capacity(reprs.len());
    for r in reprs {
        match r {
            StopRepr::Hex(h) => {
                let c = Rgb::parse(&h).ok_or_else(|| format!("invalid stop hex '{h}'"))?;
                tmp.push((None, c));
            }
            StopRepr::Full { col, at } => {
                let c = Rgb::parse(&col).ok_or_else(|| format!("invalid stop hex '{col}'"))?;
                tmp.push((Some(at), c));
            }
        }
    }
    let any_pos = tmp.iter().any(|(a, _)| a.is_some());
    let n = tmp.len();
    let mut stops: Vec<Stop> = tmp
        .into_iter()
        .enumerate()
        .map(|(i, (a, c))| {
            let at = match a {
                Some(v) => v.clamp(0.0, 1.0),
                None if any_pos => 0.0,
                None => even_pos(i, n),
            };
            Stop::new(c, at)
        })
        .collect();
    stops.sort_by(|a, b| a.at.total_cmp(&b.at));
    Ok(stops)
}

fn table_to_palette(
    stops: Vec<StopRepr>,
    motion: Option<String>,
    speed: Option<f32>,
    depth: Option<f32>,
    chaos: Option<f32>,
    interp: Option<String>,
) -> Result<Palette, String> {
    let stops = parse_stops(stops)?;
    let motion = match motion {
        Some(k) => Motion::from_parts(&k, speed.unwrap_or(1.0), depth, chaos),
        None => Motion::Hold,
    };
    let interp = interp.as_deref().map_or(Interp::Rgb, Interp::from_str);
    Ok(Palette { stops, motion, interp })
}

impl FrameTable {
    fn into_frame(self) -> Result<Frame, String> {
        let palette = table_to_palette(
            self.stops,
            self.motion,
            self.speed,
            self.depth,
            self.chaos,
            self.interp,
        )?;
        let ease = self.ease.as_deref().map_or(Ease::Linear, Ease::from_str);
        Ok(Frame {
            palette,
            hold: self.hold,
            fade: self.fade,
            ease,
        })
    }
}

fn frame_to_table(f: &Frame) -> FrameTable {
    let (motion, speed, depth, chaos) = motion_parts(&f.palette.motion);
    FrameTable {
        stops: f.palette.stops.iter().map(stop_to_repr).collect(),
        motion,
        speed,
        depth,
        chaos,
        interp: interp_tag(f.palette.interp),
        hold: f.hold,
        fade: f.fade,
        ease: if f.ease == Ease::Linear {
            None
        } else {
            Some(f.ease.as_str().to_string())
        },
    }
}

impl Spectrum {
    fn to_repr(&self) -> SpectrumRepr {
        if self.seq.len() == 1 {
            let f = &self.seq[0];
            let pal = &f.palette;
            if pal.motion == Motion::Hold {
                // a lone stop is a solid (interp is moot — there's nothing to blend between).
                if pal.stops.len() == 1 {
                    return SpectrumRepr::Solid(pal.stops[0].col.to_hex());
                }
                // the bare-array gradient tier carries no interp field, so it's only valid for the RGB
                // default; an HSV gradient falls through to the table tier (which serialises `interp`).
                if pal.interp == Interp::Rgb && stops_are_even(&pal.stops) {
                    return SpectrumRepr::Gradient(
                        pal.stops.iter().map(|s| s.col.to_hex()).collect(),
                    );
                }
            }
            // palette tier: explicit stops (+ motion if any, + interp if non-default)
            let (motion, speed, depth, chaos) = motion_parts(&pal.motion);
            return SpectrumRepr::Table(SpectrumTable {
                stops: pal.stops.iter().map(stop_to_repr).collect(),
                motion,
                speed,
                depth,
                chaos,
                interp: interp_tag(pal.interp),
                play: None,
                seq: None,
            });
        }
        // sequence tier
        SpectrumRepr::Table(SpectrumTable {
            play: Some(self.play.as_str().to_string()),
            seq: Some(self.seq.iter().map(frame_to_table).collect()),
            ..SpectrumTable::default()
        })
    }
}

impl SpectrumRepr {
    fn into_spectrum(self) -> Result<Spectrum, String> {
        match self {
            SpectrumRepr::Solid(hex) => {
                let c = Rgb::parse(&hex).ok_or_else(|| format!("invalid hex '{hex}'"))?;
                Ok(Spectrum::solid(c))
            }
            SpectrumRepr::Gradient(hexes) => {
                let mut cols = Vec::with_capacity(hexes.len());
                for h in &hexes {
                    cols.push(Rgb::parse(h).ok_or_else(|| format!("invalid hex '{h}'"))?);
                }
                Ok(Spectrum::gradient(cols))
            }
            SpectrumRepr::Table(t) => {
                if let Some(seq) = t.seq {
                    let play = t.play.as_deref().map(Loop::from_str).unwrap_or_default();
                    let mut frames = Vec::with_capacity(seq.len());
                    for f in seq {
                        frames.push(f.into_frame()?);
                    }
                    Ok(Spectrum::sequence(frames, play))
                } else {
                    let pal =
                        table_to_palette(t.stops, t.motion, t.speed, t.depth, t.chaos, t.interp)?;
                    Ok(Spectrum::from_palette(pal))
                }
            }
        }
    }
}

impl Serialize for Spectrum {
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.to_repr().serialize(ser)
    }
}

impl<'de> Deserialize<'de> for Spectrum {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        SpectrumRepr::deserialize(de)?
            .into_spectrum()
            .map_err(serde::de::Error::custom)
    }
}

// ─────────────────────────────────────────── tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A holder so a Spectrum (which can serialise to a bare string/array) can sit at a TOML field.
    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Holder {
        spectrum: Spectrum,
    }

    fn approx(a: Rgb, b: Rgb, tol: i32) -> bool {
        (i32::from(a.r) - i32::from(b.r)).abs() <= tol
            && (i32::from(a.g) - i32::from(b.g)).abs() <= tol
            && (i32::from(a.b) - i32::from(b.b)).abs() <= tol
    }

    // REGRESSION (importer): recolouring a breathing spectrum must keep it BREATHING, not flatten it to a
    // static colour (the old `spectrum = Spectrum::solid(c)` dropped the motion → imported breathing layers
    // went static).
    #[test]
    fn recolored_retints_stops_but_keeps_motion() {
        let breathing = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(10, 200, 90), 0.0)],
            Motion::Breathe { speed: 1.0, depth: 0.5 },
        ));
        let red = breathing.recolored(Rgb::new(255, 0, 0));
        assert_eq!(red.seq[0].palette.stops[0].col, Rgb::new(255, 0, 0), "stop re-tinted");
        assert_eq!(
            red.seq[0].palette.motion,
            Motion::Breathe { speed: 1.0, depth: 0.5 },
            "the Breathe motion (a breathing layer's only animation) must survive the recolour"
        );
        assert!(!red.is_solid(), "a recoloured breathing spectrum must NOT collapse to a static solid");
    }

    // REPRO: a stacked layer with a TABLE-tier spectrum (motion / sequence) inside a TOML
    // array-of-tables, with scalar fields trailing the spectrum — the real LayerDef shape.
    #[test]
    fn table_spectrum_survives_in_an_array_of_tables() {
        #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
        struct Layer {
            pattern: String,
            spectrum: Spectrum,
            blend: String,
            enabled: bool,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
        struct Stack {
            layers: Vec<Layer>,
        }
        let flow = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(255, 0, 0), 0.0),
                Stop::new(Rgb::new(0, 0, 255), 1.0),
            ],
            Motion::Flow { speed: 1.2, chaos: 0.4 },
        ));
        let seq = Spectrum::sequence(
            vec![
                Frame::solid(Rgb::new(10, 20, 30)),
                Frame::new(Palette::gradient(vec![Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)])),
            ],
            Loop::PingPong,
        );
        let stack = Stack {
            layers: vec![
                Layer {
                    pattern: "uniform".into(),
                    spectrum: Spectrum::solid(Rgb::new(0, 255, 0)),
                    blend: "normal".into(),
                    enabled: true,
                },
                Layer {
                    pattern: "flow".into(),
                    spectrum: flow,
                    blend: "add".into(),
                    enabled: true,
                },
                Layer {
                    pattern: "seqd".into(),
                    spectrum: seq,
                    blend: "screen".into(),
                    enabled: false,
                },
            ],
        };
        let s = toml::to_string(&stack).expect("serialize stack to TOML");
        println!("=== TOML ===\n{s}\n=== END ===");
        let back: Stack = toml::from_str(&s).expect("deserialize stack from TOML");
        assert_eq!(
            back, stack,
            "every stacked layer (incl. table-tier spectra) must round-trip distinctly"
        );
    }

    // REPRO of the real app.toml path: `to_string_PRETTY` (what Prefs::save uses) + the exact
    // BTreeMap -> device -> layers[] -> spectrum -> stops[] nesting depth, with MULTIPLE layers whose
    // spectra are table-tier (positioned stops). This is where stacked layers were collapsing on reload.
    #[test]
    fn pretty_nested_multi_layer_stack_round_trips() {
        use std::collections::BTreeMap;
        #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
        struct Layer {
            pattern: String,
            spectrum: Spectrum,
            blend: String,
            enabled: bool,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
        struct Dev {
            fps: u32,
            layers: Vec<Layer>,
        }
        #[derive(Serialize, Deserialize, PartialEq, Debug, Default)]
        struct Root {
            lighting: BTreeMap<String, Dev>,
        }
        // thermal-like: positioned (non-even) stops -> the TABLE tier -> [[...spectrum.stops]]
        let thermal = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(6, 7, 18), 0.0),
                Stop::new(Rgb::new(190, 22, 0), 0.32),
                Stop::new(Rgb::new(255, 255, 255), 1.0),
            ],
            Motion::Hold,
        ));
        let aurora = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(0, 255, 128), 0.0),
                Stop::new(Rgb::new(128, 0, 255), 1.0),
            ],
            Motion::Flow { speed: 1.0, chaos: 0.5 },
        ));
        let mut lighting = BTreeMap::new();
        lighting.insert(
            "0221".to_string(),
            Dev {
                fps: 30,
                layers: vec![
                    Layer {
                        pattern: "thermal".into(),
                        spectrum: thermal,
                        blend: "screen".into(),
                        enabled: true,
                    },
                    Layer {
                        pattern: "flow".into(),
                        spectrum: aurora,
                        blend: "add".into(),
                        enabled: true,
                    },
                ],
            },
        );
        let root = Root { lighting };
        let s = toml::to_string_pretty(&root).expect("pretty serialize");
        println!("=== PRETTY TOML ===\n{s}\n=== END ===");
        let back: Root = toml::from_str(&s).expect("deserialize");
        assert_eq!(
            back, root,
            "both stacked table-tier layers must round-trip distinctly (no collapse to base)"
        );
    }

    // ── Spectrum::at — the sampler across every tier ────────────────────────────────────────

    #[test]
    fn solid_is_constant_in_t_and_u() {
        let s = Spectrum::solid(Rgb::new(10, 20, 30));
        for &t in &[0.0, 1.0, 7.3, 100.0] {
            for &u in &[0.0, 0.5, 1.0] {
                assert_eq!(s.at(t, u), Rgb::new(10, 20, 30));
            }
        }
    }

    #[test]
    fn gradient_lerps_n_generically_by_u() {
        // 3-stop red→green→blue at 0, .5, 1
        let s = Spectrum::gradient(vec![
            Rgb::new(255, 0, 0),
            Rgb::new(0, 255, 0),
            Rgb::new(0, 0, 255),
        ]);
        assert_eq!(s.at(0.0, 0.0), Rgb::new(255, 0, 0), "u=0 is the first stop");
        assert_eq!(s.at(0.0, 0.5), Rgb::new(0, 255, 0), "u=.5 is the middle stop");
        assert_eq!(s.at(0.0, 1.0), Rgb::new(0, 0, 255), "u=1 is the last stop");
        // a quarter of the way is half between red and green
        assert!(approx(s.at(0.0, 0.25), Rgb::new(128, 128, 0), 2));
        // out-of-range u clamps to the end stops
        assert_eq!(s.at(0.0, -1.0), Rgb::new(255, 0, 0));
        assert_eq!(s.at(0.0, 2.0), Rgb::new(0, 0, 255));
    }

    #[test]
    fn drift_motion_scrolls_u_over_time() {
        let s = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(255, 0, 0), 0.0), Stop::new(Rgb::new(0, 0, 255), 1.0)],
            Motion::Drift { speed: 1.0 },
        ));
        // at t=0, u=0 is the first stop; drifting by speed*t shifts the lookup, so the SAME u reads a
        // different colour a moment later (the gradient slides past).
        let a = s.at(0.0, 0.0);
        let b = s.at(0.25, 0.0);
        assert_eq!(a, Rgb::new(255, 0, 0));
        assert_ne!(a, b, "drift must move the gradient under a fixed u");
    }

    #[test]
    fn cycle_motion_rotates_hue_over_time() {
        let s = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(255, 0, 0), 0.0)],
            Motion::Cycle { speed: 1.0 },
        ));
        let a = s.at(0.0, 0.0);
        // a third of a full cycle rotates red (0°) by 120° -> green-ish; clearly a different hue.
        let b = s.at(1.0 / 3.0, 0.0);
        assert_eq!(a, Rgb::new(255, 0, 0));
        assert_ne!(a, b, "cycle must rotate the hue over time");
        assert!(b.g > b.r, "a +120° rotation of red lands in the greens");
    }

    #[test]
    fn breathe_motion_dims_and_recovers() {
        let s = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(200, 200, 200), 0.0)],
            Motion::Breathe { speed: 1.0, depth: 1.0 },
        ));
        // t=0: cos(0)=1 -> factor 1 -> full brightness. half a period -> the trough (fully dimmed).
        assert_eq!(s.at(0.0, 0.0), Rgb::new(200, 200, 200));
        let trough = s.at(0.5, 0.0);
        assert!(trough.r < 10, "depth=1 breathe must dim to ~black at the trough ({trough:?})");
    }

    #[test]
    fn flow_motion_moves_but_stays_in_gamut() {
        let s = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(255, 0, 0), 0.0), Stop::new(Rgb::new(0, 0, 255), 1.0)],
            Motion::Flow { speed: 1.0, chaos: 1.0 },
        ));
        // flow is non-static: at least one sampled instant differs from t=0 (it wanders u over time).
        let base = s.at(0.0, 0.5);
        let moved = (1..20).any(|k| s.at(k as f32 * 0.1, 0.5) != base);
        assert!(moved, "flow must animate the lookup over time");
    }

    #[test]
    fn palette_stop_editing_keeps_sorted_and_bounds() {
        // start with a 2-stop gradient red(0) → blue(1).
        let mut p = Palette::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        // add a stop at the middle — its colour is sampled from the curve (≈purple), inserted in order.
        let mid = p.add_stop(0.5);
        assert_eq!(mid, 1, "the new stop lands between the two ends");
        assert_eq!(p.stops.len(), 3);
        assert!((p.stops[1].at - 0.5).abs() < 1e-6);
        assert!(approx(p.stops[1].col, Rgb::new(128, 0, 128), 2), "sampled from the gradient");
        // recolour it.
        p.set_stop_color(1, Rgb::new(0, 255, 0));
        assert_eq!(p.stops[1].col, Rgb::new(0, 255, 0));
        // move it past the end stop → re-sorts to the last index.
        let moved = p.move_stop(1, 1.0);
        assert_eq!(moved, 2, "moved stop ends up last after the re-sort");
        assert_eq!(p.stops[2].col, Rgb::new(0, 255, 0));
        // remove down to one stop, then a further remove is a no-op (never below 1).
        p.remove_stop(2);
        p.remove_stop(1);
        assert_eq!(p.stops.len(), 1);
        p.remove_stop(0);
        assert_eq!(p.stops.len(), 1, "a palette never drops below one stop");
    }

    #[test]
    fn sequence_loop_holds_then_crossfades() {
        // two solid frames: red (hold 1, no fade) then blue (hold 1, fade 1) — looping.
        let red = Frame {
            palette: Palette::solid(Rgb::new(255, 0, 0)),
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        };
        let blue = Frame {
            palette: Palette::solid(Rgb::new(0, 0, 255)),
            hold: 1.0,
            fade: 1.0,
            ease: Ease::Linear,
        };
        let s = Spectrum::sequence(vec![red, blue], Loop::Loop);
        // timeline: [0,1) red held; [1,2) blue fading in from red; [2,3) blue held; then wraps.
        assert_eq!(s.at(0.5, 0.0), Rgb::new(255, 0, 0), "frame 0 held");
        // mid blue-fade (t=1.5 -> local 0.5 of a 1s fade): halfway red→blue.
        assert!(approx(s.at(1.5, 0.0), Rgb::new(128, 0, 128), 2), "blue crossfading in over red");
        assert_eq!(s.at(2.5, 0.0), Rgb::new(0, 0, 255), "frame 1 held");
        // LOOP: frame 0 has no fade, so right after the wrap it's solid red again (seamless).
        assert_eq!(s.at(3.5, 0.0), Rgb::new(255, 0, 0), "loops back to frame 0");
    }

    #[test]
    fn sequence_once_clamps_at_the_end() {
        let a = Frame {
            palette: Palette::solid(Rgb::new(255, 0, 0)),
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        };
        let b = Frame {
            palette: Palette::solid(Rgb::new(0, 0, 255)),
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        };
        let s = Spectrum::sequence(vec![a, b], Loop::Once);
        assert_eq!(s.at(0.5, 0.0), Rgb::new(255, 0, 0));
        assert_eq!(s.at(1.5, 0.0), Rgb::new(0, 0, 255));
        // past the end it CLAMPS to the last frame, forever.
        assert_eq!(s.at(99.0, 0.0), Rgb::new(0, 0, 255));
    }

    #[test]
    fn sequence_pingpong_reflects() {
        let a = Frame {
            palette: Palette::solid(Rgb::new(255, 0, 0)),
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        };
        let b = Frame {
            palette: Palette::solid(Rgb::new(0, 0, 255)),
            hold: 1.0,
            fade: 0.0,
            ease: Ease::Linear,
        };
        let s = Spectrum::sequence(vec![a, b], Loop::PingPong);
        // total = 2, period = 4. forward [0,2): red then blue; reflected [2,4): blue then red.
        assert_eq!(s.at(0.5, 0.0), Rgb::new(255, 0, 0));
        assert_eq!(s.at(1.5, 0.0), Rgb::new(0, 0, 255));
        assert_eq!(s.at(2.5, 0.0), Rgb::new(0, 0, 255), "reflected: still blue just past the turn");
        assert_eq!(s.at(3.5, 0.0), Rgb::new(255, 0, 0), "reflected back toward red");
    }

    #[test]
    fn ease_snap_is_a_hard_cut() {
        assert_eq!(Ease::Snap.apply(0.49), 0.0);
        assert_eq!(Ease::Snap.apply(0.5), 1.0);
        assert_eq!(Ease::Smooth.apply(0.5), 0.5);
        assert_eq!(Ease::Linear.apply(0.42), 0.42);
    }

    // ── tiered serde — every tier round-trips through JSON and TOML ──────────────────────────

    fn json_round_trip(s: &Spectrum) -> Spectrum {
        let j = serde_json::to_string(s).expect("ser json");
        serde_json::from_str(&j).expect("de json")
    }

    fn toml_round_trip(s: &Spectrum) -> Spectrum {
        let h = Holder { spectrum: s.clone() };
        let t = toml::to_string(&h).expect("ser toml");
        let back: Holder = toml::from_str(&t).expect("de toml");
        back.spectrum
    }

    #[test]
    fn solid_tier_serialises_as_bare_hex() {
        let s = Spectrum::solid(Rgb::new(0x4A, 0xF2, 0xB0));
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"4AF2B0\"", "solid -> bare hex string");
        assert_eq!(json_round_trip(&s), s);
        assert_eq!(toml_round_trip(&s), s);
    }

    #[test]
    fn gradient_tier_serialises_as_hex_array() {
        let s = Spectrum::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        assert_eq!(serde_json::to_string(&s).unwrap(), "[\"FF0000\",\"0000FF\"]", "gradient -> hex array");
        assert_eq!(json_round_trip(&s), s);
        assert_eq!(toml_round_trip(&s), s);
    }

    #[test]
    fn motion_tier_round_trips() {
        // a 2-stop palette with Flow motion -> the palette table tier (stops + motion fields).
        let s = Spectrum::from_palette(Palette::new(
            vec![Stop::new(Rgb::new(10, 200, 120), 0.0), Stop::new(Rgb::new(80, 0, 255), 1.0)],
            Motion::Flow { speed: 1.5, chaos: 0.75 },
        ));
        assert_eq!(json_round_trip(&s), s);
        assert_eq!(toml_round_trip(&s), s);
    }

    #[test]
    fn uneven_gradient_keeps_explicit_positions() {
        // unevenly-spaced stops can't use the bare-array tier; they round-trip via the table tier.
        let s = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(255, 0, 0), 0.0),
                Stop::new(Rgb::new(0, 255, 0), 0.2),
                Stop::new(Rgb::new(0, 0, 255), 1.0),
            ],
            Motion::Hold,
        ));
        // serialised as a table (not a bare array), preserving the 0.2 position.
        let back = json_round_trip(&s);
        assert_eq!(back, s);
        assert!((back.seq[0].palette.stops[1].at - 0.2).abs() < 1e-6);
        assert_eq!(toml_round_trip(&s), s);
    }

    #[test]
    fn sequence_tier_round_trips() {
        let s = Spectrum::sequence(
            vec![
                Frame {
                    palette: Palette::new(
                        vec![Stop::new(Rgb::new(255, 0, 0), 0.0), Stop::new(Rgb::new(255, 200, 0), 1.0)],
                        Motion::Breathe { speed: 0.5, depth: 0.4 },
                    ),
                    hold: 2.0,
                    fade: 0.5,
                    ease: Ease::Smooth,
                },
                Frame {
                    palette: Palette::solid(Rgb::new(0, 0, 255)),
                    hold: 1.0,
                    fade: 1.5,
                    ease: Ease::Snap,
                },
            ],
            Loop::PingPong,
        );
        assert_eq!(json_round_trip(&s), s);
        assert_eq!(toml_round_trip(&s), s);
    }

    #[test]
    fn deserialises_hand_written_tiers() {
        // the hand-editable forms a user could type — each parses to the right spectrum.
        let solid: Spectrum = serde_json::from_str("\"FF8800\"").unwrap();
        assert_eq!(solid, Spectrum::solid(Rgb::new(0xFF, 0x88, 0x00)));

        let grad: Spectrum = serde_json::from_str("[\"FF0000\",\"00FF00\",\"0000FF\"]").unwrap();
        assert_eq!(grad.seq[0].palette.stops.len(), 3);
        assert!((grad.seq[0].palette.stops[1].at - 0.5).abs() < 1e-6);

        // a palette table with hex-only stops + motion: positions implied even, motion parsed.
        let pal: Spectrum =
            serde_json::from_str(r#"{"stops":["FF0000","0000FF"],"motion":"drift","speed":2.0}"#)
                .unwrap();
        assert_eq!(pal.seq[0].palette.motion, Motion::Drift { speed: 2.0 });
        assert_eq!(pal.seq[0].palette.stops.len(), 2);
        assert!((pal.seq[0].palette.stops[1].at - 1.0).abs() < 1e-6);

        // a sequence table.
        let seq: Spectrum = serde_json::from_str(
            r#"{"play":"once","seq":[{"stops":["FF0000"],"hold":1.0},{"stops":["0000FF"],"hold":1.0,"fade":0.5}]}"#,
        )
        .unwrap();
        assert_eq!(seq.play, Loop::Once);
        assert_eq!(seq.seq.len(), 2);
        assert!((seq.seq[1].fade - 0.5).abs() < 1e-6);
    }

    // ── perceptual (HSV) interpolation — opt-in, shortest hue path ───────────────────────────

    #[test]
    fn rgb_interp_is_the_default_and_unchanged() {
        // a default red→blue gradient is plain RGB: its midpoint is the muddy half-bright purple.
        let p = Palette::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        assert_eq!(p.interp, Interp::Rgb, "RGB is the default interp");
        assert!(approx(p.sample(0.5), Rgb::new(128, 0, 128), 1), "RGB midpoint is the raw lerp");
        assert_eq!(p.sample(0.0), Rgb::new(255, 0, 0));
        assert_eq!(p.sample(1.0), Rgb::new(0, 0, 255));
    }

    #[test]
    fn hsv_interp_takes_the_short_hue_path_avoiding_mud() {
        // red(0°)→blue(240°): the SHORT way round the wheel goes backward through magenta(300°), not
        // forward through green — so the midpoint is a VIVID, fully-saturated magenta, never grey.
        let mut p = Palette::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        p.interp = Interp::Hsv;
        let mid = p.sample(0.5);
        assert!(
            mid.r > 220 && mid.b > 220 && mid.g < 30,
            "HSV midpoint of red→blue is vivid magenta, got {mid:?}"
        );
        // saturated, not grey: the channel spread is wide (a grey would have r≈g≈b).
        let spread = i32::from(mid.r.max(mid.b)) - i32::from(mid.g);
        assert!(spread > 200, "magenta is far from grey (spread {spread})");
        // and it's brighter/more vivid than the muddy RGB midpoint (128,0,128).
        let rgb_mid = Palette::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]).sample(0.5);
        assert!(
            i32::from(mid.r) + i32::from(mid.b) > i32::from(rgb_mid.r) + i32::from(rgb_mid.b),
            "HSV magenta {mid:?} is more vivid than RGB {rgb_mid:?}"
        );
        // endpoints are still exact in either space.
        assert_eq!(p.sample(0.0), Rgb::new(255, 0, 0));
        assert_eq!(p.sample(1.0), Rgb::new(0, 0, 255));
    }

    #[test]
    fn interp_serde_omits_default_and_round_trips_hsv() {
        // DEFAULT (RGB) gradient: serialises to the bare hex ARRAY — no `interp` field grows.
        let rgb = Spectrum::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        let j = serde_json::to_string(&rgb).unwrap();
        assert_eq!(j, "[\"FF0000\",\"0000FF\"]", "default RGB gradient stays a bare array");
        assert!(!j.contains("interp"), "a default palette never grows an interp field");
        assert_eq!(json_round_trip(&rgb), rgb);
        assert_eq!(toml_round_trip(&rgb), rgb);

        // HSV gradient: can't use the bare array (it carries no interp), so it falls to the table tier
        // and DOES serialise `interp:"hsv"`, and round-trips losslessly through JSON and TOML.
        let mut pal = Palette::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]);
        pal.interp = Interp::Hsv;
        let hsv = Spectrum::from_palette(pal);
        let jh = serde_json::to_string(&hsv).unwrap();
        assert!(jh.contains("\"interp\":\"hsv\""), "HSV gradient serialises its interp: {jh}");
        let back = json_round_trip(&hsv);
        assert_eq!(back, hsv);
        assert_eq!(back.seq[0].palette.interp, Interp::Hsv);
        assert_eq!(toml_round_trip(&hsv), hsv);
    }

    #[test]
    fn interp_parses_from_hand_written_table() {
        let p: Spectrum =
            serde_json::from_str(r#"{"stops":["FF0000","0000FF"],"interp":"hsv"}"#).unwrap();
        assert_eq!(p.seq[0].palette.interp, Interp::Hsv);
        // an absent/unknown interp tag defaults to RGB.
        let d: Spectrum = serde_json::from_str(r#"{"stops":["FF0000","0000FF"]}"#).unwrap();
        assert_eq!(d.seq[0].palette.interp, Interp::Rgb);
    }
}
