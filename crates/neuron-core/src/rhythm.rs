// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Rhythm engine for KNOCKBACK — the desk-drum half of the rhythm familiar.
//!
//! This module is **pure**: no platform code, no wall-clock, no device I/O. It turns a
//! stream of timestamped mouse samples and click edges into a stream of musical
//! [`Onset`]s, gathers them into [`Motif`]s, embeds a motif as an Engram trajectory (so
//! the [`crate::twin`] can fit a damped oscillator over it and spin it forward into a
//! knockback), quantizes onsets to a byte alphabet (for the Logos attention organ in
//! [`crate::logos`]), and keeps the **mirror statistics** — the trailing reflection of the
//! player's demonstrated peak that the twin can never exceed.
//!
//! Time is always passed in as `t_ms` by the caller. The session supplies real
//! timestamps; tests supply virtual ones. Nothing here reads the clock itself, so every
//! function is deterministic and exhaustively testable.
//!
//! ## The physics, honestly
//! A *voice* is carved from a drawn stroke's eigenmotion fit ([`crate::glyph::GlyphFit`]):
//! the dominant eigenvalue's magnitude is the ring's decay, its argument is the spin /
//! handedness, and the fit residual is the shimmer. Beats wear the voice that was active
//! when they were struck — which is why a drawn shape literally becomes *how your rhythm
//! looks*. That is the EIGEN-RHYTHM verb.

use crate::glyph::GlyphFit;
use serde::{Deserialize, Serialize};

// ── voice (a drawn shape becomes an instrument timbre) ───────────────────────

/// How many distinct voice classes the byte alphabet distinguishes (3 bits).
pub const VOICE_CLASSES: u8 = 8;

/// A carved instrument voice. Its fields come straight from a stroke's eigenmotion fit;
/// they drive both the sound-of-the-thing (conceptually) and the look of every beat ring
/// struck while this voice is active.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Voice {
    /// Signed rotation in `[-1, 1]` from `arg(λ)` — handedness + tightness of the curl.
    pub spin: f32,
    /// Ring decay length in `[0, 1]` from `|λ|` — a tight loop rings long, an open flick
    /// dies fast.
    pub decay: f32,
    /// Grain / shimmer in `[0, 1]` from the fit residual over mean step — how irregular
    /// the hand was.
    pub shimmer: f32,
    /// Hue offset in `[0, 1)` derived from the spin — the voice's colour signature.
    pub hue: f32,
    /// Quantized identity `0..VOICE_CLASSES` used by the byte alphabet.
    pub class: u8,
}

impl Voice {
    /// The plain voice every session starts with before any shape is carved: a clean,
    /// upright, medium-decay ring.
    #[must_use]
    pub fn neutral() -> Voice {
        Voice {
            spin: 0.0,
            decay: 0.5,
            shimmer: 0.1,
            hue: 0.0,
            class: 0,
        }
    }

    /// Carve a voice from a stroke's eigenmotion fit. Uses the dominant eigenvalue (the
    /// one further from the origin — the mode that actually shapes the motion).
    #[must_use]
    pub fn from_fit(f: &GlyphFit) -> Voice {
        let (dom, _sub) = if f.lambda1.abs() >= f.lambda2.abs() {
            (f.lambda1, f.lambda2)
        } else {
            (f.lambda2, f.lambda1)
        };
        let mag = dom.abs();
        let arg = dom.arg(); // -π..π

        // decay: |λ| in [0,1] maps to ring persistence; clamp the (rare) >1 unstable fits.
        let decay = (mag.clamp(0.0, 1.0)) as f32;
        // spin: normalized signed angle.
        let spin = (arg / std::f64::consts::PI).clamp(-1.0, 1.0) as f32;
        // shimmer: residual relative to the mean step is scale-free irregularity.
        let shimmer = if f.mean_step > 1e-9 {
            (f.residual / f.mean_step).min(1.0) as f32
        } else {
            0.0
        };
        // hue: fold the full -1..1 spin onto the colour wheel.
        let hue = ((spin * 0.5) + 0.5).rem_euclid(1.0);
        // class: quantize the spin into VOICE_CLASSES bins (handedness is identity).
        let class = (((spin * 0.5 + 0.5) * f32::from(VOICE_CLASSES)) as i32)
            .clamp(0, i32::from(VOICE_CLASSES) - 1) as u8;

        Voice {
            spin,
            decay,
            shimmer,
            hue,
            class,
        }
    }
}

// ── onsets (a hit is a beat) ─────────────────────────────────────────────────

/// What kind of strike produced an onset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnsetKind {
    /// A full drum hit — the trigger button clicked.
    Tap,
    /// A ghost note — a motion impulse with no click. Fidgeting *is* playing.
    Ghost,
}

/// One musical event: a beat struck at `t_ms`, with a strength and the voice it wore.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Onset {
    pub t_ms: u64,
    /// Strike strength in `[0, 1]`.
    pub energy: f32,
    pub kind: OnsetKind,
    pub voice: Voice,
}

// ── onset detection (continuous motion + click edges → beats) ────────────────

/// Tuning for the onset detector. Defaults are attuned to a 1000 Hz mouse but are
/// resolution-free (they key off velocity in screen units per millisecond).
#[derive(Clone, Copy, Debug)]
pub struct DetectorConfig {
    /// Velocity (units/ms) above which a motion impulse fires a ghost note.
    pub ghost_threshold: f32,
    /// The detector re-arms once velocity falls below this fraction of the threshold —
    /// hysteresis so one wiggle is one onset, not five.
    pub rearm_fraction: f32,
    /// Minimum gap between two ghost notes.
    pub ghost_refractory_ms: u64,
    /// Velocity that maps to full energy 1.0.
    pub energy_ref: f32,
    /// Tap hold duration (ms) that maps to full energy 1.0 (a firm press hits harder).
    pub tap_energy_ms: u64,
}

impl Default for DetectorConfig {
    fn default() -> Self {
        DetectorConfig {
            ghost_threshold: 1.6,
            rearm_fraction: 0.45,
            ghost_refractory_ms: 90,
            energy_ref: 6.0,
            tap_energy_ms: 140,
        }
    }
}

/// Turns a live stream of `(t_ms, x, y)` samples and trigger click edges into onsets.
#[derive(Clone, Debug)]
pub struct OnsetDetector {
    cfg: DetectorConfig,
    last_pt: Option<(u64, f32, f32)>,
    armed: bool,
    last_ghost_ms: Option<u64>,
    voice: Voice,
}

impl OnsetDetector {
    #[must_use]
    pub fn new(cfg: DetectorConfig) -> Self {
        OnsetDetector {
            cfg,
            last_pt: None,
            armed: true,
            last_ghost_ms: None,
            voice: Voice::neutral(),
        }
    }

    /// Swap the active voice (called when the player carves a new one).
    pub fn set_voice(&mut self, v: Voice) {
        self.voice = v;
    }

    #[must_use]
    pub fn voice(&self) -> Voice {
        self.voice
    }

    /// Feed one motion sample. Returns a ghost onset if a fresh motion impulse peaked.
    pub fn sample(&mut self, t_ms: u64, x: f32, y: f32) -> Option<Onset> {
        let out = if let Some((pt, px, py)) = self.last_pt {
            let dt = (t_ms.saturating_sub(pt)).max(1) as f32;
            let vel = ((x - px).hypot(y - py)) / dt;
            let thr = self.cfg.ghost_threshold;
            let refractory_ok = match self.last_ghost_ms {
                Some(last) => t_ms.saturating_sub(last) >= self.cfg.ghost_refractory_ms,
                None => true,
            };
            if self.armed && vel >= thr && refractory_ok {
                self.armed = false;
                self.last_ghost_ms = Some(t_ms);
                Some(Onset {
                    t_ms,
                    energy: (vel / self.cfg.energy_ref).clamp(0.05, 1.0),
                    kind: OnsetKind::Ghost,
                    voice: self.voice,
                })
            } else {
                if vel < thr * self.cfg.rearm_fraction {
                    self.armed = true;
                }
                None
            }
        } else {
            None
        };
        self.last_pt = Some((t_ms, x, y));
        out
    }

    /// Register a trigger tap. `hold_ms` is how long the button was held (press strength).
    pub fn tap(&mut self, t_ms: u64, hold_ms: u64) -> Onset {
        let energy = (hold_ms as f32 / self.cfg.tap_energy_ms as f32).clamp(0.2, 1.0);
        self.strike(t_ms, energy)
    }

    /// Register a strike at the PRESS edge with an explicit energy — the drum verb. A drum
    /// answers the moment the stick lands, not when it lifts; sessions that need instant
    /// visual feedback fire this on the down edge (energy derived from the playing itself,
    /// e.g. spacing — you can't hit hard fast).
    pub fn strike(&mut self, t_ms: u64, energy: f32) -> Onset {
        // Reset the motion gate so the press itself doesn't also fire a ghost.
        self.armed = false;
        self.last_ghost_ms = Some(t_ms);
        Onset {
            t_ms,
            energy: energy.clamp(0.05, 1.0),
            kind: OnsetKind::Tap,
            voice: self.voice,
        }
    }
}

// ── motifs (a phrase of beats) ───────────────────────────────────────────────

/// Tuning for gathering onsets into motifs.
#[derive(Clone, Copy, Debug)]
pub struct MotifConfig {
    /// A silence longer than this closes the current motif.
    pub phrase_gap_ms: u64,
    /// Hard cap on onsets in a single motif (runaway guard).
    pub max_onsets: usize,
}

impl Default for MotifConfig {
    fn default() -> Self {
        MotifConfig {
            phrase_gap_ms: 700,
            max_onsets: 64,
        }
    }
}

/// A closed phrase: a run of onsets with no internal gap longer than `phrase_gap_ms`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Motif {
    pub onsets: Vec<Onset>,
}

impl Motif {
    #[must_use]
    pub fn len(&self) -> usize {
        self.onsets.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.onsets.is_empty()
    }
    #[must_use]
    pub fn start_ms(&self) -> u64 {
        self.onsets.first().map_or(0, |o| o.t_ms)
    }
    #[must_use]
    pub fn duration_ms(&self) -> u64 {
        match (self.onsets.first(), self.onsets.last()) {
            (Some(a), Some(b)) => b.t_ms.saturating_sub(a.t_ms),
            _ => 0,
        }
    }
    /// Inter-onset intervals in milliseconds (length = len-1).
    #[must_use]
    pub fn iois(&self) -> Vec<u64> {
        self.onsets
            .windows(2)
            .map(|w| w[1].t_ms.saturating_sub(w[0].t_ms))
            .collect()
    }
    /// Median inter-onset interval — the motif's own tempo. Falls back to a moderate
    /// 300 ms for a single hit so downstream maths never divides by zero.
    #[must_use]
    pub fn median_ioi(&self) -> f32 {
        let mut iois = self.iois();
        if iois.is_empty() {
            return 300.0;
        }
        iois.sort_unstable();
        let mid = iois.len() / 2;
        if iois.len().is_multiple_of(2) {
            (iois[mid - 1] + iois[mid]) as f32 / 2.0
        } else {
            iois[mid] as f32
        }
    }
    #[must_use]
    pub fn total_energy(&self) -> f32 {
        self.onsets.iter().map(|o| o.energy).sum()
    }
    /// Onsets per second over the motif's span (its density).
    #[must_use]
    pub fn density(&self) -> f32 {
        let d = self.duration_ms();
        if d == 0 {
            return 0.0;
        }
        (self.onsets.len() as f32 - 1.0) * 1000.0 / d as f32
    }
    /// IOIs as ratios of the median — the rhythm's *shape*, independent of absolute speed.
    /// This is what the twin learns, so it mirrors your pattern, not your wrist's tempo.
    #[must_use]
    pub fn normalized(&self) -> Vec<f32> {
        let med = self.median_ioi().max(1.0);
        self.iois().iter().map(|&i| i as f32 / med).collect()
    }
    /// The dominant voice of the motif (most common voice class among its onsets).
    pub fn dominant_voice(&self) -> Voice {
        if self.onsets.is_empty() {
            return Voice::neutral();
        }
        let mut counts = [0u32; VOICE_CLASSES as usize];
        for o in &self.onsets {
            counts[o.voice.class as usize] += 1;
        }
        let best = counts
            .iter()
            .enumerate()
            .max_by_key(|(_, &c)| c)
            .map_or(0, |(i, _)| i);
        // return a representative voice of that class
        self.onsets
            .iter()
            .find(|o| o.voice.class as usize == best).map_or_else(Voice::neutral, |o| o.voice)
    }
}

/// Accumulates onsets and emits a [`Motif`] each time a phrase closes.
#[derive(Clone, Debug)]
pub struct MotifBuilder {
    cfg: MotifConfig,
    pending: Vec<Onset>,
}

impl MotifBuilder {
    #[must_use]
    pub fn new(cfg: MotifConfig) -> Self {
        MotifBuilder {
            cfg,
            pending: Vec::new(),
        }
    }

    /// Push the next onset. If it arrives after a gap longer than `phrase_gap_ms`, the
    /// *previous* run is closed and returned, and this onset begins a new phrase. Also
    /// closes if `max_onsets` is reached.
    pub fn push(&mut self, o: Onset) -> Option<Motif> {
        let closed = if let Some(last) = self.pending.last() {
            if o.t_ms.saturating_sub(last.t_ms) > self.cfg.phrase_gap_ms {
                Some(Motif {
                    onsets: std::mem::take(&mut self.pending),
                })
            } else {
                None
            }
        } else {
            None
        };
        self.pending.push(o);
        if self.pending.len() >= self.cfg.max_onsets {
            return Some(Motif {
                onsets: std::mem::take(&mut self.pending),
            });
        }
        closed
    }

    /// Has the pending phrase been silent past the gap as of `now_ms`? If so, close it.
    /// The session calls this on idle ticks so the last phrase doesn't hang forever.
    pub fn poll_close(&mut self, now_ms: u64) -> Option<Motif> {
        if let Some(last) = self.pending.last() {
            if now_ms.saturating_sub(last.t_ms) > self.cfg.phrase_gap_ms {
                return Some(Motif {
                    onsets: std::mem::take(&mut self.pending),
                });
            }
        }
        None
    }

    /// Force-close whatever is pending (e.g. on session exit).
    pub fn flush(&mut self) -> Option<Motif> {
        if self.pending.is_empty() {
            None
        } else {
            Some(Motif {
                onsets: std::mem::take(&mut self.pending),
            })
        }
    }

    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

// ── embedding (a motif becomes an Engram trajectory) ─────────────────────────

/// Embedding dimension fed to the Engram codec: 8 complex oscillator pairs. The codec
/// takes any even D; 16 is ample for a rhythm envelope and keeps the brain compact.
pub const EMBED_DIM: usize = 16;
/// Number of trajectory rows (time samples) per motif embedding.
pub const EMBED_ROWS: usize = 48;

/// Build a continuous loudness envelope of the motif over `rows` samples spanning its
/// duration. Each onset deposits a Gaussian pulse scaled by its energy and shaped by its
/// voice decay (a long-ringing voice leaves a wider bump). Returns `rows + EMBED_DIM`
/// samples so the delay-embedding below always has full history.
fn envelope(m: &Motif, rows: usize) -> Vec<f32> {
    let pre = EMBED_DIM;
    let n = rows + pre;
    let mut env = vec![0.0f32; n];
    if m.onsets.len() < 2 {
        // a single hit: one pulse near the start of the live region
        if let Some(o) = m.onsets.first() {
            deposit(&mut env, pre as f32 + 1.0, o.energy, o.voice.decay);
        }
        return env;
    }
    let span = m.duration_ms().max(1) as f32;
    let t0 = m.start_ms() as f32;
    for o in &m.onsets {
        // map onset time → sample index in the live region [pre, pre+rows)
        let frac = ((o.t_ms as f32) - t0) / span; // 0..1
        let center = pre as f32 + frac * (rows as f32 - 1.0);
        deposit(&mut env, center, o.energy, o.voice.decay);
    }
    env
}

/// Add one Gaussian pulse to the envelope. Width grows with the voice's decay so the
/// rhythm's *texture* (staccato vs. ringing) is carried into the trajectory.
fn deposit(env: &mut [f32], center: f32, energy: f32, decay: f32) {
    let sigma = 0.9 + 2.4 * decay; // staccato ≈0.9, ringing ≈3.3 samples
    let inv2s2 = 1.0 / (2.0 * sigma * sigma);
    let lo = ((center - 4.0 * sigma).floor().max(0.0)) as usize;
    let hi = ((center + 4.0 * sigma).ceil() as usize).min(env.len().saturating_sub(1));
    for (offset, slot) in env[lo..=hi].iter_mut().enumerate() {
        let d = (lo + offset) as f32 - center;
        *slot += energy * (-d * d * inv2s2).exp();
    }
}

/// Embed a motif as a `[EMBED_ROWS × EMBED_DIM]` row-major f32 trajectory via **delay
/// embedding** (Takens reconstruction) of its loudness envelope: row τ is the window of
/// the `EMBED_DIM` most recent envelope samples. This is the standard phase-space lift —
/// the AR(2)-per-pair oscillator that Engram fits over it captures the rhythm's spectral
/// content, and spinning that oscillator forward predicts the next beats: the flourish.
#[must_use]
pub fn embed_motif(m: &Motif) -> (Vec<f32>, usize, usize) {
    let env = envelope(m, EMBED_ROWS);
    let mut traj = vec![0.0f32; EMBED_ROWS * EMBED_DIM];
    for r in 0..EMBED_ROWS {
        let base = r + EMBED_DIM; // env index of the newest sample for this row
        for d in 0..EMBED_DIM {
            traj[r * EMBED_DIM + d] = env[base - d];
        }
    }
    (traj, EMBED_ROWS, EMBED_DIM)
}

// ── mirror statistics (the trailing reflection of demonstrated peak) ─────────

/// A peak tracker with fast attack and slow decay: it jumps to meet a new high and eases
/// back down when you let off. This is *why* slowing down lowers the twin's ceiling within
/// a few exchanges, and why there is no difficulty variable anywhere — the twin is a
/// portrait of your recent peak, not a target.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct PeakTracker {
    value: f32,
    attack: f32,
    decay: f32,
}

impl PeakTracker {
    #[must_use]
    pub fn new(initial: f32, attack: f32, decay: f32) -> Self {
        PeakTracker {
            value: initial,
            attack,
            decay,
        }
    }
    pub fn update(&mut self, x: f32) {
        let rate = if x > self.value {
            self.attack
        } else {
            self.decay
        };
        self.value += rate * (x - self.value);
    }
    #[must_use]
    pub fn value(&self) -> f32 {
        self.value
    }
}

/// The ceiling the twin's knockback is clamped to. Read-only snapshot of the mirror.
#[derive(Clone, Copy, Debug)]
pub struct Ceiling {
    /// Fastest sustainable tempo the player has shown, as a *minimum* IOI in ms.
    pub min_ioi_ms: f32,
    /// Densest phrase (onsets/sec) shown.
    pub density: f32,
    /// Longest phrase (onset count) shown.
    pub length: f32,
    /// Hardest hit (energy) shown.
    pub energy: f32,
}

/// Trailing reflection of the player's demonstrated peak across motifs.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct MirrorStats {
    // tempo tracked as "speed" = 1000/min_ioi so attack means *getting faster*
    tempo_speed: PeakTracker,
    density: PeakTracker,
    length: PeakTracker,
    energy: PeakTracker,
}

impl Default for MirrorStats {
    fn default() -> Self {
        MirrorStats {
            // start gentle: ~600 ms IOI, sparse, short, soft
            tempo_speed: PeakTracker::new(1000.0 / 600.0, 0.5, 0.06),
            density: PeakTracker::new(1.0, 0.5, 0.06),
            length: PeakTracker::new(2.0, 0.4, 0.05),
            energy: PeakTracker::new(0.4, 0.5, 0.08),
        }
    }
}

impl MirrorStats {
    /// Fold a freshly closed motif into the mirror.
    pub fn observe(&mut self, m: &Motif) {
        if m.onsets.is_empty() {
            return;
        }
        let med = m.median_ioi().max(40.0);
        self.tempo_speed.update(1000.0 / med);
        self.density.update(m.density());
        self.length.update(m.onsets.len() as f32);
        // peak (not mean) energy of the phrase
        let peak_e = m.onsets.iter().map(|o| o.energy).fold(0.0, f32::max);
        self.energy.update(peak_e);
    }

    #[must_use]
    pub fn ceiling(&self) -> Ceiling {
        Ceiling {
            min_ioi_ms: (1000.0 / self.tempo_speed.value().max(0.1)).clamp(60.0, 2000.0),
            density: self.density.value().max(0.2),
            length: self.length.value().max(1.0),
            energy: self.energy.value().clamp(0.1, 1.0),
        }
    }
}

// ── byte alphabet (onsets → bytes for the Logos attention organ) ─────────────

/// Quantize an onset into one byte: `[3 bits log-IOI | 2 bits energy | 3 bits voice]`.
/// The IOI is the gap *from the previous onset*; pass the first onset's IOI as its own
/// median or any sentinel — the high bucket simply reads as "a long wait".
///
/// This is the alphabet the Logos predictor consumes: a player who repeats a groove emits
/// a low-surprise byte stream; a player breaking new ground emits a high-surprise one. The
/// twin's stream and the player's stream cross-predicting each other *is* sync.
#[must_use]
pub fn onset_byte(ioi_ms: u64, energy: f32, voice_class: u8) -> u8 {
    // log-IOI bucket: map ~16 ms..~2 s across 8 buckets (log2 of ioi/16, clamped).
    let ioi = ioi_ms.max(1) as f32;
    let bucket = ((ioi / 16.0).log2().floor()).clamp(0.0, 7.0) as u8;
    let e = ((energy.clamp(0.0, 0.999) * 4.0) as u8) & 0b11;
    let v = voice_class & 0b111;
    (bucket << 5) | (e << 3) | v
}

/// Turn a closed motif into its byte string for the attention organ. The first onset uses
/// the motif's median IOI as a stand-in gap.
#[must_use]
pub fn motif_bytes(m: &Motif) -> Vec<u8> {
    if m.onsets.is_empty() {
        return Vec::new();
    }
    let med = m.median_ioi() as u64;
    let mut out = Vec::with_capacity(m.onsets.len());
    let mut prev: Option<u64> = None;
    for o in &m.onsets {
        let ioi = match prev {
            Some(p) => o.t_ms.saturating_sub(p),
            None => med,
        };
        out.push(onset_byte(ioi, o.energy, o.voice.class));
        prev = Some(o.t_ms);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{fit, C};

    fn onset(t: u64, e: f32) -> Onset {
        Onset {
            t_ms: t,
            energy: e,
            kind: OnsetKind::Tap,
            voice: Voice::neutral(),
        }
    }

    fn motif(times: &[u64]) -> Motif {
        Motif {
            onsets: times.iter().map(|&t| onset(t, 0.6)).collect(),
        }
    }

    // ── voice ────────────────────────────────────────────────────────────────

    #[test]
    fn voice_neutral_is_upright() {
        let v = Voice::neutral();
        assert_eq!(v.spin, 0.0);
        assert_eq!(v.class, 0);
    }

    #[test]
    fn voice_from_circle_fit_has_spin_and_long_decay() {
        // a clean CCW circle: strong rotation, |λ|≈1 (sustained loop) → long decay.
        let n = 64;
        let z: Vec<C> = (0..n)
            .map(|i| {
                let th = 2.0 * std::f64::consts::PI * f64::from(i) / 16.0;
                C::new(th.cos(), th.sin())
            })
            .collect();
        let f = fit(&z).expect("fit");
        let v = Voice::from_fit(&f);
        assert!(v.decay > 0.6, "circle should ring: decay={}", v.decay);
        assert!(v.spin.abs() > 0.05, "circle should have spin: {}", v.spin);
        assert!((0.0..1.0).contains(&v.hue));
    }

    #[test]
    fn voice_handedness_separates_classes() {
        let n = 64;
        let ccw: Vec<C> = (0..n)
            .map(|i| {
                let th = 2.0 * std::f64::consts::PI * f64::from(i) / 16.0;
                C::new(th.cos(), th.sin())
            })
            .collect();
        let cw: Vec<C> = (0..n)
            .map(|i| {
                let th = 2.0 * std::f64::consts::PI * f64::from(i) / 16.0;
                C::new(th.cos(), -th.sin())
            })
            .collect();
        let vccw = Voice::from_fit(&fit(&ccw).unwrap());
        let vcw = Voice::from_fit(&fit(&cw).unwrap());
        assert_ne!(
            vccw.class, vcw.class,
            "CW and CCW should be different voices"
        );
    }

    // ── onset detection ───────────────────────────────────────────────────────

    #[test]
    fn tap_makes_an_onset_with_press_energy() {
        let mut d = OnsetDetector::new(DetectorConfig::default());
        let soft = d.tap(0, 40);
        let firm = d.tap(500, 200);
        assert_eq!(soft.kind, OnsetKind::Tap);
        assert!(firm.energy > soft.energy, "firmer press hits harder");
    }

    #[test]
    fn strike_is_instant_and_suppresses_its_own_ghost() {
        let mut d = OnsetDetector::new(DetectorConfig::default());
        let o = d.strike(100, 0.8);
        assert_eq!(o.kind, OnsetKind::Tap);
        assert!((o.energy - 0.8).abs() < 1e-6);
        // the press motion right after a strike must not double-fire as a ghost
        d.sample(101, 0.0, 0.0);
        assert!(
            d.sample(105, 40.0, 0.0).is_none(),
            "strike gates the immediate ghost"
        );
    }

    #[test]
    fn one_wiggle_is_one_ghost_not_many() {
        let mut d = OnsetDetector::new(DetectorConfig::default());
        // a single fast swipe across several samples, then settle
        let mut ghosts = 0;
        // ramp up fast (above threshold for several samples)
        for i in 0..6 {
            let t = i * 4;
            if d.sample(t, i as f32 * 20.0, 0.0).is_some() {
                ghosts += 1;
            }
        }
        // settle (velocity ~0 so it re-arms)
        for i in 0..10 {
            let t = 100 + i * 16;
            d.sample(t, 120.0, 0.0);
        }
        assert_eq!(
            ghosts, 1,
            "hysteresis should collapse one swipe to one onset"
        );
    }

    #[test]
    fn two_separated_wiggles_are_two_ghosts() {
        let mut d = OnsetDetector::new(DetectorConfig::default());
        let mut ghosts = 0;
        // swipe 1
        for i in 0..4 {
            if d.sample(i * 4, i as f32 * 20.0, 0.0).is_some() {
                ghosts += 1;
            }
        }
        // rest (re-arm)
        for i in 0..12 {
            d.sample(200 + i * 16, 80.0, 0.0);
        }
        // swipe 2
        for i in 0..4 {
            if d.sample(400 + i * 4, 80.0 + i as f32 * 20.0, 0.0).is_some() {
                ghosts += 1;
            }
        }
        assert_eq!(ghosts, 2);
    }

    // ── motifs ────────────────────────────────────────────────────────────────

    #[test]
    fn builder_closes_on_gap() {
        let mut b = MotifBuilder::new(MotifConfig::default());
        assert!(b.push(onset(0, 0.5)).is_none());
        assert!(b.push(onset(200, 0.5)).is_none());
        assert!(b.push(onset(400, 0.5)).is_none());
        // big gap → previous phrase closes, this onset starts a new one
        let closed = b.push(onset(2000, 0.5)).expect("phrase should close");
        assert_eq!(closed.len(), 3);
        assert_eq!(b.pending_len(), 1);
    }

    #[test]
    fn shave_and_a_haircut_has_the_right_shape() {
        // da-da-da-DA-da : five beats, the classic. normalized shape is speed-free.
        let m = motif(&[0, 250, 500, 650, 900]);
        let norm = m.normalized();
        assert_eq!(norm.len(), 4);
        // played twice as fast, the normalized shape must be identical
        let fast = motif(&[0, 125, 250, 325, 450]);
        let fnorm = fast.normalized();
        for (a, b) in norm.iter().zip(fnorm.iter()) {
            assert!((a - b).abs() < 0.05, "{a} vs {b}");
        }
    }

    #[test]
    fn poll_close_fires_after_silence() {
        let mut b = MotifBuilder::new(MotifConfig::default());
        b.push(onset(0, 0.5));
        b.push(onset(200, 0.5));
        assert!(b.poll_close(500).is_none(), "still within the phrase");
        let closed = b.poll_close(1200).expect("silence closes it");
        assert_eq!(closed.len(), 2);
    }

    // ── embedding ─────────────────────────────────────────────────────────────

    #[test]
    fn embedding_has_expected_shape() {
        let m = motif(&[0, 300, 600, 900]);
        let (traj, rows, dim) = embed_motif(&m);
        assert_eq!(rows, EMBED_ROWS);
        assert_eq!(dim, EMBED_DIM);
        assert_eq!(traj.len(), EMBED_ROWS * EMBED_DIM);
        // the envelope is non-trivial: there is real energy in the trajectory
        let total: f32 = traj.iter().map(|x| x.abs()).sum();
        assert!(total > 0.5, "embedding should carry energy, got {total}");
    }

    #[test]
    fn embedding_oscillator_predicts_forward() {
        // The whole point: fit an oscillator on the embedded rhythm and spinning it
        // forward should stay bounded and non-trivial (a real continuation, the flourish).
        use engram::fit::fit_all;
        use engram::predict::{from_complex, predict_all, to_complex};
        let m = motif(&[0, 300, 600, 900, 1200, 1500]);
        let (traj, rows, dim) = embed_motif(&m);
        let p = dim / 2;
        let z = to_complex(&traj, rows, dim);
        let fit = fit_all(&z, rows, p);
        let seed2: Vec<_> = z[(rows - 2) * p..(rows - 1) * p].to_vec();
        let seed1: Vec<_> = z[(rows - 1) * p..rows * p].to_vec();
        let cont = predict_all(&seed1, &seed2, &fit.k, &fit.g, 16);
        let fcont = from_complex(&cont, 16, p);
        assert_eq!(fcont.len(), 16 * dim);
        // continuation must be finite and bounded (stable oscillator)
        let maxv = fcont.iter().copied().fold(0.0f32, |a, b| a.max(b.abs()));
        assert!(
            maxv.is_finite() && maxv < 50.0,
            "continuation exploded: {maxv}"
        );
    }

    // ── mirror ────────────────────────────────────────────────────────────────

    #[test]
    fn mirror_rises_to_peak_then_eases_down() {
        let mut mir = MirrorStats::default();
        // play a dense fast burst
        let hot = motif(&[0, 100, 200, 300, 400, 500, 600]);
        for _ in 0..3 {
            mir.observe(&hot);
        }
        let hot_ceil = mir.ceiling();
        // then play slow and sparse for a while
        let chill = motif(&[0, 800]);
        for _ in 0..8 {
            mir.observe(&chill);
        }
        let cool_ceil = mir.ceiling();
        assert!(
            hot_ceil.density > cool_ceil.density,
            "mirror must ease down when you slow"
        );
        assert!(
            cool_ceil.min_ioi_ms > hot_ceil.min_ioi_ms,
            "tempo ceiling relaxes"
        );
    }

    #[test]
    fn mirror_never_below_floor() {
        let mir = MirrorStats::default();
        let c = mir.ceiling();
        assert!(c.min_ioi_ms <= 2000.0 && c.min_ioi_ms >= 60.0);
        assert!(c.density >= 0.2);
    }

    // ── byte alphabet ─────────────────────────────────────────────────────────

    #[test]
    fn onset_byte_packs_fields() {
        let b = onset_byte(16, 0.0, 0); // smallest IOI bucket, lowest energy, voice 0
        assert_eq!(b >> 5, 0);
        let b2 = onset_byte(16 * 128, 0.99, 7); // ~2 s, top energy, voice 7
        assert_eq!(b2 >> 5, 7);
        assert_eq!(b2 & 0b111, 7);
        assert_eq!((b2 >> 3) & 0b11, 3);
    }

    #[test]
    fn repeated_groove_is_low_entropy_bytes() {
        // a steady groove should emit a near-constant byte (predictable → low surprise)
        let m = motif(&[0, 250, 500, 750, 1000, 1250]);
        let bytes = motif_bytes(&m);
        assert_eq!(bytes.len(), 6);
        // after the first, every IOI is identical → identical bytes
        assert!(
            bytes[1..].iter().all(|&b| b == bytes[1]),
            "steady groove → constant bytes"
        );
    }
}
