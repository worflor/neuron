// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The rhythm familiar — KNOCKBACK's twin. A spectral entity with no rhythm of its own:
//! it learns to move by watching you and knocks back what it learns, with one tiny
//! flourish. This module is **pure logic** (no platform, no wall-clock, no I/O); the
//! session in `neuron-app` drives it and paints what it returns.
//!
//! ## The twin *is* the codec
//! - **Memory / generation** — every exchange is an [`engram`] trajectory. The knockback is
//!   the player's motif re-voiced, plus a continuation produced by fitting a damped
//!   oscillator over the motif and spinning it **one or two beats past** where the player
//!   stopped. That continuation is the model's own extrapolation of you — familiar physics,
//!   a phrase you never actually played. The "tiny flourish" is not a bolted-on generator;
//!   it is `predict` running off the end of `fit`.
//! - **Attention** — player and twin byte-streams run through [`crate::logos`]. Player
//!   self-surprise is *novelty*; mutual cross-predictability is *sync* (flow). Heat is a
//!   running density×energy envelope.
//! - **The mirror** — the twin's output is clamped to the player's demonstrated peak
//!   ([`crate::rhythm::MirrorStats`]). There is no difficulty variable anywhere. The only
//!   way to make the game hard is to *be* hard; slow down and the twin settles to you.
//!
//! Everything emergent (Storm / Stillpoint / Haunting / Sigil) is a rule over these
//! signals, not a subsystem. Determinism is load-bearing: all randomness is a SplitMix64
//! seeded from the exchange index, so a given script of motifs always yields an identical
//! brain — which is exactly what the simulated-player tests assert.

use serde::{Deserialize, Serialize};

use crate::glyph::{self, C};
use crate::logos::LogosStream;
use crate::rhythm::{
    embed_motif, motif_bytes, Ceiling, MirrorStats, Motif, Onset, OnsetKind, Voice,
};
use engram::brain::Brain;
use engram::brain_io;

// ── configuration (attunement; persisted in twin.toml) ───────────────────────

/// Tunable thresholds and presence. Everything here is *attunement* — calibrated from real
/// play — never live state (which lives in the brain and the rings).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TwinConfig {
    /// Engram projection temperature (alpha) for the brain.
    pub alpha: f32,
    /// Max extra beats the twin may add past the player's phrase (the flourish length).
    pub max_flourish: usize,
    /// Surprise (bits/byte) below which the player counts as "locked into a groove".
    pub groove_novelty: f32,
    /// Heat above which, sustained, a Storm can break.
    pub storm_heat: f32,
    /// Consecutive hot+locked turns required to summon a Storm.
    pub storm_turns: u32,
    /// Sync above which, sustained, a Stillpoint opens.
    pub stillpoint_sync: f32,
    /// Consecutive in-sync turns required to enter a Stillpoint.
    pub stillpoint_turns: u32,
    /// Eigen-distance below which the player's answer reads as harmony (else counterpoint).
    pub harmony_distance: f32,
    /// How many past motifs to keep for Storms / Hauntings / replay.
    pub memory_ring: usize,
}

impl Default for TwinConfig {
    fn default() -> Self {
        TwinConfig {
            alpha: 0.005,
            max_flourish: 2,
            groove_novelty: 2.2,
            storm_heat: 0.62,
            storm_turns: 3,
            stillpoint_sync: 0.72,
            stillpoint_turns: 3,
            harmony_distance: 0.5,
            memory_ring: 256,
        }
    }
}

// ── what a turn produces ──────────────────────────────────────────────────────

/// The twin's reply: a phrase it plays back, left to right.
#[derive(Clone, Debug)]
pub struct Knockback {
    /// The beats the twin plays. Times are relative to the start of the knockback (ms).
    pub onsets: Vec<Onset>,
    /// Index into `onsets` where the flourish (the continuation past your phrase) begins.
    pub flourish_from: usize,
    /// True if the phrase ends on an open beat — an empty ring inviting your answer.
    pub open: bool,
    /// The cold reflection voice the twin wears this turn.
    pub voice: Voice,
}

impl Knockback {
    pub fn len(&self) -> usize {
        self.onsets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.onsets.is_empty()
    }
    pub fn duration_ms(&self) -> u64 {
        self.onsets.last().map(|o| o.t_ms).unwrap_or(0)
    }
    /// Onsets/sec of the played phrase (for ceiling assertions and the readout).
    pub fn density(&self) -> f32 {
        if self.onsets.len() < 2 {
            return 0.0;
        }
        let d = self.duration_ms().max(1);
        (self.onsets.len() as f32 - 1.0) * 1000.0 / d as f32
    }
}

/// How the twin read the player's last answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Judgment {
    /// The answer matched where the phrase was heading.
    Harmony,
    /// The answer diverged, but self-consistently — a valid twist.
    Counterpoint,
}

/// An emergent moment that broke this turn. Each is a consequence, never a scheduled mode.
#[derive(Clone, Debug, PartialEq)]
pub enum Emergent {
    /// Earned heat: the twin braids your hottest motifs into one chained phrase to answer.
    Storm { phrase_len: usize },
    /// Flow rewarded with calm: time dilates. `depth` in `[0,1]` is how deep the sync ran.
    Stillpoint { depth: f32 },
    /// History surfaces: an old motif replays in faded light. `age` = exchanges ago.
    Haunting { age: usize },
}

/// A snapshot of the live signals, surfaced to the overlay and the panel readout.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Signals {
    /// Player self-surprise, bits/byte — are you doing something new?
    pub novelty: f32,
    /// Mutual predictability in `[0,1]` — are you and the twin finishing each other's lines?
    pub sync: f32,
    /// Running density×energy envelope in `[0,1]` — how hot you're running.
    pub heat: f32,
    /// Eigen-space drift of the last motif from the brain (0..100) — a second novelty view.
    pub drift: f32,
    /// Storm-won palette depth: the weave's colour richness, earned over time.
    pub palette_depth: u32,
    /// Total exchanges this brain has ever absorbed.
    pub exchanges: u64,
}

/// The full result of feeding the twin one player motif.
#[derive(Clone, Debug)]
pub struct TwinTurn {
    pub knockback: Knockback,
    /// How your answer to the *previous* knockback read (None on the opening knock).
    pub judged: Option<Judgment>,
    pub event: Option<Emergent>,
    pub signals: Signals,
}

// ── deterministic PRNG (no thread_rng — reproducible brains) ─────────────────

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct SplitMix64 {
    state: u64,
}
impl SplitMix64 {
    fn seeded(seed: u64) -> Self {
        SplitMix64 {
            state: seed.wrapping_add(0x9E37_79B9_7F4A_7C15),
        }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// A small signed jitter in [-mag, mag].
    fn jitter(&mut self, mag: f32) -> f32 {
        let u = (self.next_u64() >> 11) as f32 / (1u64 << 53) as f32; // 0..1
        (u * 2.0 - 1.0) * mag
    }
}

// ── the familiar ──────────────────────────────────────────────────────────────

/// The rhythm familiar. Hold one per session; feed it the player's motifs.
pub struct Familiar {
    cfg: TwinConfig,
    brain: Brain,
    mirror: MirrorStats,

    // four attention streams: player-self, twin-self, player↻twin, twin↻player.
    s_player: LogosStream,
    s_twin: LogosStream,
    s_pt: LogosStream, // player history predicting the twin
    s_tp: LogosStream, // twin history predicting the player

    // running signals
    novelty: f32,
    sync: f32,
    heat: f32,
    drift: f32,
    palette_depth: u32,

    // emergent state machines
    hot_run: u32,
    sync_run: u32,
    turns: u64,

    // memory for storms / hauntings / replay (kept verbatim, cheap)
    ring: Vec<Motif>,

    // the twin's last continuation shape, to judge the player's next answer
    last_continuation: Option<Vec<f32>>,

    rng: SplitMix64,
}

impl Familiar {
    /// Birth a fresh familiar with no rhythm of its own.
    pub fn new(cfg: TwinConfig) -> Self {
        let dim = crate::rhythm::EMBED_DIM;
        Familiar {
            brain: Brain::new(dim, cfg.alpha),
            mirror: MirrorStats::default(),
            s_player: LogosStream::new(),
            s_twin: LogosStream::new(),
            s_pt: LogosStream::new(),
            s_tp: LogosStream::new(),
            novelty: 0.0,
            sync: 0.0,
            heat: 0.0,
            drift: 0.0,
            palette_depth: 1,
            hot_run: 0,
            sync_run: 0,
            turns: 0,
            ring: Vec::new(),
            last_continuation: None,
            cfg,
            rng: SplitMix64::seeded(SEED),
        }
    }

    pub fn config(&self) -> &TwinConfig {
        &self.cfg
    }

    pub fn signals(&self) -> Signals {
        Signals {
            novelty: self.novelty,
            sync: self.sync,
            heat: self.heat,
            drift: self.drift,
            palette_depth: self.palette_depth,
            exchanges: self.brain.total_absorbed as u64,
        }
    }

    pub fn ceiling(&self) -> Ceiling {
        self.mirror.ceiling()
    }

    /// The whole loop for one player phrase: judge the prior answer, update every signal,
    /// learn the motif, generate the knockback (clamped to your peak), and surface any
    /// emergent moment. This is the only entry point the session needs.
    pub fn receive(&mut self, motif: &Motif) -> TwinTurn {
        // ── 1. judge the answer to our previous knockback ──────────────────
        let judged = self.judge_answer(motif);

        // ── 2. update attention signals ────────────────────────────────────
        let pbytes = motif_bytes(motif);
        self.novelty = self.s_player.surprise_of(&pbytes);
        // how well the twin's accumulated view predicts this player phrase
        let tp_surprise = self.s_tp.surprise_of(&pbytes);

        // ── 3. mirror + heat ───────────────────────────────────────────────
        self.mirror.observe(motif);
        let peak_e = motif.onsets.iter().map(|o| o.energy).fold(0.0, f32::max);
        let instant_heat = (motif.density() * 0.25 + peak_e).min(2.0) / 2.0;
        self.heat = self.heat * 0.7 + instant_heat * 0.3;

        // ── 4. learn the motif (brain + ring) ──────────────────────────────
        let (traj, rows, _dim) = embed_motif(motif);
        self.drift = self.brain.measure(&traj, rows).drift as f32;
        self.brain.fast_absorb(&traj, rows, None);
        self.ring.push(motif.clone());
        if self.ring.len() > self.cfg.memory_ring {
            let excess = self.ring.len() - self.cfg.memory_ring;
            self.ring.drain(..excess);
        }
        self.turns += 1;

        // ── 5. decide the reply: ordinary knockback, or an emergent moment ──
        let mut event = None;

        // Storm: hot and locked into a groove for long enough.
        let locked = self.novelty <= self.cfg.groove_novelty;
        if self.heat >= self.cfg.storm_heat && locked {
            self.hot_run += 1;
        } else {
            self.hot_run = 0;
        }

        let knockback = if self.hot_run >= self.cfg.storm_turns && self.ring.len() >= 3 {
            self.hot_run = 0;
            self.palette_depth += 1;
            let storm = self.braid_storm();
            event = Some(Emergent::Storm {
                phrase_len: storm.len(),
            });
            storm
        } else {
            self.generate_knockback(motif)
        };

        // feed the twin's reply into the attention streams (self + cross)
        let tb = knockback_bytes(&knockback);
        self.s_twin.observe_all(&tb);
        let pt_surprise = self.s_pt.surprise_of(&tb);

        // ── 6. sync from mutual cross-surprise ─────────────────────────────
        // both directions low (each predicts the other) → high sync.
        const SYNC_SCALE: f32 = 6.0; // bits/byte that maps to "totally unpredictable"
        let cross = ((pt_surprise + tp_surprise) * 0.5 / SYNC_SCALE).clamp(0.0, 1.0);
        let instant_sync = 1.0 - cross;
        self.sync = self.sync * 0.6 + instant_sync * 0.4;

        if self.sync >= self.cfg.stillpoint_sync {
            self.sync_run += 1;
        } else {
            self.sync_run = 0;
        }
        // Stillpoint: don't override a Storm in the same turn.
        if event.is_none() && self.sync_run >= self.cfg.stillpoint_turns {
            event = Some(Emergent::Stillpoint { depth: self.sync });
        }

        // remember the flourish shape so we can judge the next answer
        self.last_continuation = Some(continuation_shape(&knockback));

        TwinTurn {
            knockback,
            judged,
            event,
            signals: self.signals(),
        }
    }

    /// Compare a player phrase to where our last flourish was heading.
    fn judge_answer(&self, motif: &Motif) -> Option<Judgment> {
        let cont = self.last_continuation.as_ref()?;
        let shape = motif.normalized();
        if shape.is_empty() || cont.is_empty() {
            return Some(Judgment::Counterpoint);
        }
        let dist = shape_distance(cont, &shape);
        Some(if dist <= self.cfg.harmony_distance {
            Judgment::Harmony
        } else {
            Judgment::Counterpoint
        })
    }

    /// The ordinary knockback: replay the player's rhythm in the twin's cold voice, then
    /// extend it by `predict`-ing the fitted oscillator a beat or two past the end. The
    /// whole phrase is clamped so the twin never out-runs the player's demonstrated peak.
    fn generate_knockback(&mut self, motif: &Motif) -> Knockback {
        let ceil = self.mirror.ceiling();
        let twin_voice = self.twin_voice(motif);

        // replay: the player's onsets, re-timed from t=0, re-voiced.
        let t0 = motif.start_ms();
        let mut onsets: Vec<Onset> = motif
            .onsets
            .iter()
            .map(|o| Onset {
                t_ms: o.t_ms.saturating_sub(t0),
                energy: o.energy,
                kind: o.kind,
                voice: twin_voice,
            })
            .collect();
        let flourish_from = onsets.len();

        // the flourish: continue the rhythm with the fitted oscillator.
        let flourish = self.flourish(motif, &ceil, twin_voice);
        let mut last_t = onsets.last().map(|o| o.t_ms).unwrap_or(0);
        for (ioi, energy) in flourish {
            let step = (ioi.max(ceil.min_ioi_ms as u64)).max(40);
            last_t += step;
            onsets.push(Onset {
                t_ms: last_t,
                energy,
                kind: OnsetKind::Tap,
                voice: twin_voice,
            });
        }

        // clamp overall density to the ceiling: if the twin is denser than you've shown,
        // stretch the whole phrase in time until it fits. (Never escalates past you.)
        clamp_density(&mut onsets, ceil.density);

        Knockback {
            onsets,
            flourish_from,
            open: true,
            voice: twin_voice,
        }
    }

    /// Predict the next `1..=max_flourish` beats as (ioi_ms, energy) pairs by fitting a
    /// damped oscillator over the motif's (ioi-ratio, energy) feature series and spinning it
    /// forward. Falls back to a gentle echo when the phrase is too short to fit.
    fn flourish(&mut self, motif: &Motif, ceil: &Ceiling, _voice: Voice) -> Vec<(u64, f32)> {
        let n = motif.onsets.len();
        let med = motif.median_ioi().max(ceil.min_ioi_ms);
        // how many beats to add, never beyond the configured flourish or the length ceiling.
        let room = (ceil.length.round() as i64 - n as i64).max(0) as usize;
        let count = self.cfg.max_flourish.min(room.max(1)).max(1);

        // feature series: z[k] = ioi_ratio[k] + i·energy[k]
        if n >= 3 {
            let iois = motif.iois();
            let mut z: Vec<C> = Vec::with_capacity(n);
            // align energies to interval index (use the later onset's energy per interval)
            for (k, &ioi) in iois.iter().enumerate() {
                let ratio = ioi as f64 / med as f64;
                let energy = motif.onsets[k + 1].energy as f64;
                z.push(C::new(ratio, energy));
            }
            if z.len() >= 3 {
                if let Some(fit) = glyph::fit(&z) {
                    let mut out = Vec::with_capacity(count);
                    let mut z1 = z[z.len() - 1];
                    let mut z2 = z[z.len() - 2];
                    for _ in 0..count {
                        // z[next] = K·z1 − G·z2
                        let next = fit.k.mul(z1).sub(fit.g.mul(z2));
                        z2 = z1;
                        z1 = next;
                        let ratio = next.re.clamp(0.25, 4.0);
                        let energy = (next.im as f32).clamp(0.1, 1.0);
                        // a featherweight deterministic flourish: nudge the timing slightly
                        let jit = 1.0 + self.rng.jitter(0.06) as f64;
                        let ioi = (med as f64 * ratio * jit).round() as u64;
                        out.push((ioi.max(ceil.min_ioi_ms as u64), energy));
                    }
                    return out;
                }
            }
        }

        // echo fallback: repeat the last interval, slightly softened.
        let last_ioi = motif.iois().last().copied().unwrap_or(med as u64);
        let last_e = motif.onsets.last().map(|o| o.energy).unwrap_or(0.5);
        (0..count)
            .map(|_| {
                (
                    last_ioi.max(ceil.min_ioi_ms as u64),
                    (last_e * 0.85).max(0.15),
                )
            })
            .collect()
    }

    /// The twin's voice: the player's dominant voice, reflected — a small deterministic
    /// perturbation so it reads as "almost you". The cold *colour* is applied in the overlay;
    /// here we shape ring decay/spin/shimmer.
    fn twin_voice(&mut self, motif: &Motif) -> Voice {
        let mut v = motif.dominant_voice();
        v.spin = (v.spin + self.rng.jitter(0.12)).clamp(-1.0, 1.0);
        v.decay = (v.decay + self.rng.jitter(0.08)).clamp(0.0, 1.0);
        v.shimmer = (v.shimmer + self.rng.jitter(0.05)).clamp(0.0, 1.0);
        v
    }

    /// Storm: braid the player's three highest-energy remembered motifs into one chained
    /// phrase and dare them to answer it whole.
    fn braid_storm(&mut self) -> Knockback {
        let mut ranked: Vec<&Motif> = self.ring.iter().collect();
        ranked.sort_by(|a, b| b.total_energy().partial_cmp(&a.total_energy()).unwrap());
        let picks: Vec<Motif> = ranked.into_iter().take(3).cloned().collect();
        let ceil = self.mirror.ceiling();
        let voice = picks
            .first()
            .map(|m| m.dominant_voice())
            .unwrap_or_else(Voice::neutral);

        let mut onsets = Vec::new();
        let mut t = 0u64;
        for (i, m) in picks.iter().enumerate() {
            let t0 = m.start_ms();
            for o in &m.onsets {
                let rel = o.t_ms.saturating_sub(t0);
                onsets.push(Onset {
                    t_ms: t + rel,
                    energy: o.energy,
                    kind: o.kind,
                    voice,
                });
            }
            // a breath between braided strands
            t = onsets.last().map(|o| o.t_ms).unwrap_or(t) + (ceil.min_ioi_ms as u64) * 2;
            let _ = i;
        }
        clamp_density(&mut onsets, ceil.density);
        let flourish_from = onsets.len();
        Knockback {
            onsets,
            flourish_from,
            open: true,
            voice,
        }
    }

    /// Summon a haunting: an old motif (early memories preferred) replayed faded. Returns
    /// the phrase and its age in exchanges, or None if there's no deep history yet.
    pub fn haunting(&self) -> Option<(Knockback, usize)> {
        if self.ring.len() < 8 {
            return None;
        }
        // prefer the early third of memory — the clumsy first fidgets.
        let idx = self.ring.len() / 6;
        let m = &self.ring[idx];
        let t0 = m.start_ms();
        let voice = m.dominant_voice();
        let onsets = m
            .onsets
            .iter()
            .map(|o| Onset {
                t_ms: o.t_ms.saturating_sub(t0),
                energy: o.energy * 0.5,
                kind: o.kind,
                voice,
            })
            .collect();
        let age = self.ring.len() - idx;
        Some((
            Knockback {
                onsets: {
                    let v: Vec<Onset> = onsets;
                    v
                },
                flourish_from: 0,
                open: false,
                voice,
            },
            age,
        ))
    }

    /// The personal sigil: a drawing no other human could generate, grown from the running
    /// distribution of the player's eigen-vocabulary (the brain's dream buffer of per-motif
    /// K). Returns a normalized 2D path in `[-1,1]²`, ready to render or export as SVG.
    pub fn sigil_path(&self, samples: usize) -> Vec<(f32, f32)> {
        // Build a small oscillator bank from the spread of remembered K eigenvalues and run
        // it forward — the same predict-from-fit that generates knockbacks, turned to ink.
        let dream = &self.brain.dream;
        if dream.is_empty() {
            return Vec::new();
        }
        let p = self.brain.pairs;
        // average a handful of dream K's into a few drawing oscillators
        let banks = 5.min(p).max(1);
        let mut path = Vec::with_capacity(samples);
        // accumulate complex position by summing oscillators z_b[n] = K_b·z_b[n-1]
        let mut state: Vec<(f64, f64)> = Vec::with_capacity(banks);
        let mut ks: Vec<(f64, f64)> = Vec::with_capacity(banks);
        for b in 0..banks {
            // mean K of pair b across the dream buffer
            let (mut kr, mut ki) = (0.0f64, 0.0f64);
            for e in dream {
                kr += e.k[b].re;
                ki += e.k[b].im;
            }
            let n = dream.len() as f64;
            kr /= n;
            ki /= n;
            // pull just inside the unit circle so the drawing is a bounded rosette
            let mag = (kr * kr + ki * ki).sqrt().max(1e-6);
            let target = 0.92 + 0.06 * (b as f64 / banks as f64);
            let s = target / mag.max(target);
            ks.push((kr * s, ki * s));
            state.push((1.0, 0.0));
        }
        let mut maxr = 1e-6f64;
        let mut raw = Vec::with_capacity(samples);
        for _ in 0..samples {
            let (mut x, mut y) = (0.0f64, 0.0f64);
            for b in 0..banks {
                // z = K·z
                let (zr, zi) = state[b];
                let (kr, ki) = ks[b];
                let nr = kr * zr - ki * zi;
                let ni = kr * zi + ki * zr;
                state[b] = (nr, ni);
                x += nr / (b + 1) as f64;
                y += ni / (b + 1) as f64;
            }
            maxr = maxr.max(x.hypot(y));
            raw.push((x, y));
        }
        for (x, y) in raw {
            path.push(((x / maxr) as f32, (y / maxr) as f32));
        }
        path
    }

    pub fn memory_len(&self) -> usize {
        self.ring.len()
    }

    // ── persistence ────────────────────────────────────────────────────────

    /// Serialize the whole familiar: the Engram brain plus the game-side rings and signals.
    pub fn save(&self) -> Vec<u8> {
        let brain_bytes = brain_io::save(&self.brain);
        let side = SideSave {
            cfg: self.cfg,
            mirror: self.mirror,
            novelty: self.novelty,
            sync: self.sync,
            heat: self.heat,
            drift: self.drift,
            palette_depth: self.palette_depth,
            turns: self.turns,
            ring: self.ring.clone(),
            rng: self.rng,
        };
        let side_json = serde_json::to_vec(&side).unwrap_or_default();
        let mut out = Vec::with_capacity(8 + brain_bytes.len() + side_json.len());
        out.extend_from_slice(b"KNBK");
        out.push(1); // version
        out.extend_from_slice(&(brain_bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(&brain_bytes);
        out.extend_from_slice(&side_json);
        out
    }

    /// Restore a familiar from [`Familiar::save`] bytes.
    pub fn load(data: &[u8]) -> Option<Familiar> {
        if data.len() < 9 || &data[..4] != b"KNBK" || data[4] != 1 {
            return None;
        }
        let blen = u32::from_le_bytes([data[5], data[6], data[7], data[8]]) as usize;
        let brain_end = 9 + blen;
        if data.len() < brain_end {
            return None;
        }
        let brain = brain_io::load(&data[9..brain_end])?;
        let side: SideSave = serde_json::from_slice(&data[brain_end..]).ok()?;
        Some(Familiar {
            cfg: side.cfg,
            brain,
            mirror: side.mirror,
            s_player: LogosStream::new(),
            s_twin: LogosStream::new(),
            s_pt: LogosStream::new(),
            s_tp: LogosStream::new(),
            novelty: side.novelty,
            sync: side.sync,
            heat: side.heat,
            drift: side.drift,
            palette_depth: side.palette_depth,
            hot_run: 0,
            sync_run: 0,
            turns: side.turns,
            ring: side.ring,
            last_continuation: None,
            rng: side.rng,
        })
    }

    /// The raw brain, for diagnostics (`neuron twin`) and tests.
    pub fn brain(&self) -> &Brain {
        &self.brain
    }
}

#[derive(Serialize, Deserialize)]
struct SideSave {
    cfg: TwinConfig,
    mirror: MirrorStats,
    novelty: f32,
    sync: f32,
    heat: f32,
    drift: f32,
    palette_depth: u32,
    turns: u64,
    ring: Vec<Motif>,
    rng: SplitMix64,
}

// ── free helpers ──────────────────────────────────────────────────────────────

/// Quantize a knockback into the Logos byte alphabet (same packing as player motifs).
fn knockback_bytes(k: &Knockback) -> Vec<u8> {
    if k.onsets.is_empty() {
        return Vec::new();
    }
    // build a Motif-like view to reuse the shared alphabet
    let m = Motif {
        onsets: k.onsets.clone(),
    };
    motif_bytes(&m)
}

/// The flourish's normalized rhythm shape (IOIs as ratios of their median) — what the
/// player is invited to complete.
fn continuation_shape(k: &Knockback) -> Vec<f32> {
    if k.flourish_from >= k.onsets.len() || k.onsets.len() < 2 {
        return Vec::new();
    }
    let tail = &k.onsets[k.flourish_from.saturating_sub(1)..];
    let iois: Vec<u64> = tail
        .windows(2)
        .map(|w| w[1].t_ms.saturating_sub(w[0].t_ms))
        .collect();
    if iois.is_empty() {
        return Vec::new();
    }
    let mut sorted = iois.clone();
    sorted.sort_unstable();
    let med = sorted[sorted.len() / 2].max(1) as f32;
    iois.iter().map(|&i| i as f32 / med).collect()
}

/// Distance between two normalized rhythm shapes, length-tolerant (compares the overlap).
fn shape_distance(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 1.0;
    }
    let mut s = 0.0;
    for i in 0..n {
        s += (a[i] - b[i]).abs();
    }
    s / n as f32
}

/// Stretch a phrase in time (in place) until its density no longer exceeds `max_density`.
/// Only ever *slows* the twin — it can never escalate past the player's demonstrated peak.
fn clamp_density(onsets: &mut [Onset], max_density: f32) {
    if onsets.len() < 2 || max_density <= 0.0 {
        return;
    }
    let dur = onsets
        .last()
        .unwrap()
        .t_ms
        .saturating_sub(onsets[0].t_ms)
        .max(1);
    let density = (onsets.len() as f32 - 1.0) * 1000.0 / dur as f32;
    if density <= max_density {
        return;
    }
    let scale = density / max_density; // > 1
    let t0 = onsets[0].t_ms;
    for o in onsets.iter_mut() {
        let rel = o.t_ms.saturating_sub(t0) as f32;
        o.t_ms = t0 + (rel * scale) as u64;
    }
}

// LogosStream helper: observe a slice without reading surprise.
impl LogosStream {
    fn observe_all(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.observe(b);
        }
    }
}

/// A fixed deterministic seed ("KNOCKBK" in ascii) — the familiar's randomness is fully
/// reproducible so a given script of motifs always grows the same brain.
const SEED: u64 = 0x004B_4E4F_434B_424B;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rhythm::Voice;

    // ── scripted simulated players (the test centerpiece) ──────────────────

    fn onset(t: u64, e: f32, v: Voice) -> Onset {
        Onset {
            t_ms: t,
            energy: e,
            kind: OnsetKind::Tap,
            voice: v,
        }
    }

    /// Build a motif from (offset, ioi, count) at a fixed energy and voice.
    fn groove(start: u64, ioi: u64, count: usize, energy: f32, v: Voice) -> Motif {
        let onsets = (0..count)
            .map(|i| onset(start + i as u64 * ioi, energy, v))
            .collect();
        Motif { onsets }
    }

    #[test]
    fn opening_knock_yields_a_knockback() {
        let mut fam = Familiar::new(TwinConfig::default());
        let m = groove(0, 250, 4, 0.6, Voice::neutral());
        let turn = fam.receive(&m);
        assert!(
            turn.judged.is_none(),
            "no prior answer to judge on the opening knock"
        );
        assert!(!turn.knockback.is_empty(), "the twin must answer");
        assert!(turn.knockback.flourish_from <= turn.knockback.len());
        assert!(
            turn.knockback.open,
            "the phrase ends open, inviting an answer"
        );
    }

    #[test]
    fn knockback_carries_the_flourish() {
        let mut fam = Familiar::new(TwinConfig::default());
        let m = groove(0, 250, 5, 0.6, Voice::neutral());
        let turn = fam.receive(&m);
        // the twin replays your 5 and adds at least one continuation beat
        assert!(
            turn.knockback.len() > m.len(),
            "knockback ({}) should extend the motif ({})",
            turn.knockback.len(),
            m.len()
        );
    }

    #[test]
    fn mirror_never_exceeds_demonstrated_peak() {
        // The core promise: the twin's reply is never denser than what the player has shown.
        let mut fam = Familiar::new(TwinConfig::default());
        for _ in 0..12 {
            let chill = groove(0, 700, 3, 0.4, Voice::neutral());
            let turn = fam.receive(&chill);
            let ceil = fam.ceiling();
            assert!(
                turn.knockback.density() <= ceil.density + 0.05,
                "knockback density {} exceeded ceiling {}",
                turn.knockback.density(),
                ceil.density
            );
        }
    }

    #[test]
    fn fader_ceiling_decays_after_slowing_down() {
        // Tryhard, then chill: the twin must breathe back down within a few exchanges.
        let mut fam = Familiar::new(TwinConfig::default());
        for _ in 0..5 {
            fam.receive(&groove(0, 120, 8, 0.9, Voice::neutral()));
        }
        let hot = fam.ceiling().density;
        for _ in 0..8 {
            fam.receive(&groove(0, 800, 2, 0.3, Voice::neutral()));
        }
        let cool = fam.ceiling().density;
        assert!(
            cool < hot,
            "ceiling must ease down: hot {hot} → cool {cool}"
        );
    }

    #[test]
    fn tryhard_summons_a_storm_chill_never_does() {
        // Chill player: no storm across a long session.
        let mut chill = Familiar::new(TwinConfig::default());
        let mut chill_storms = 0;
        for _ in 0..40 {
            if matches!(
                chill
                    .receive(&groove(0, 650, 3, 0.35, Voice::neutral()))
                    .event,
                Some(Emergent::Storm { .. })
            ) {
                chill_storms += 1;
            }
        }
        assert_eq!(
            chill_storms, 0,
            "a calm player should never trigger a storm"
        );

        // Tryhard locked into one hot groove: a storm must eventually break.
        let mut hard = Familiar::new(TwinConfig::default());
        let hot = groove(0, 110, 9, 0.95, Voice::neutral());
        let mut storms = 0;
        for _ in 0..40 {
            if matches!(hard.receive(&hot).event, Some(Emergent::Storm { .. })) {
                storms += 1;
            }
        }
        assert!(storms >= 1, "a hot, locked-in player should earn a storm");
    }

    #[test]
    fn metronome_reaches_stillpoint_and_density_holds() {
        // A perfectly steady player co-adapts with the twin → sync rises → stillpoint, and
        // the twin's density must NOT escalate out of flow.
        let mut fam = Familiar::new(TwinConfig::default());
        let beat = groove(0, 300, 4, 0.55, Voice::neutral());
        let mut hit_stillpoint = false;
        let mut max_density: f32 = 0.0;
        for _ in 0..30 {
            let turn = fam.receive(&beat);
            max_density = max_density.max(turn.knockback.density());
            if matches!(turn.event, Some(Emergent::Stillpoint { .. })) {
                hit_stillpoint = true;
            }
        }
        assert!(hit_stillpoint, "a metronome should find a stillpoint");
        // density stays bounded by the steady player's own pace (~3.3/s at 300ms), not racing.
        assert!(
            max_density < 6.0,
            "stillpoint must dilate, not escalate: {max_density}"
        );
    }

    #[test]
    fn harmony_vs_counterpoint_is_judged() {
        let mut fam = Familiar::new(TwinConfig::default());
        let m = groove(0, 250, 5, 0.6, Voice::neutral());
        fam.receive(&m); // opening knock; sets last_continuation
                         // answer with the same steady pace → should read as harmony
        let same = groove(0, 250, 3, 0.6, Voice::neutral());
        let turn = fam.receive(&same);
        assert!(turn.judged.is_some());
        // answer with a wildly different jagged pace → counterpoint
        let jagged = Motif {
            onsets: vec![
                onset(0, 0.9, Voice::neutral()),
                onset(900, 0.2, Voice::neutral()),
                onset(950, 0.9, Voice::neutral()),
            ],
        };
        let turn2 = fam.receive(&jagged);
        assert!(turn2.judged.is_some());
    }

    #[test]
    fn determinism_same_script_same_brain() {
        let script: Vec<Motif> = (0..20usize)
            .map(|i| {
                groove(
                    0,
                    200 + (i as u64 % 5) * 40,
                    3 + (i % 3),
                    0.5 + (i % 3) as f32 * 0.1,
                    Voice::neutral(),
                )
            })
            .collect();
        let run = || {
            let mut fam = Familiar::new(TwinConfig::default());
            for m in &script {
                fam.receive(m);
            }
            fam.save()
        };
        assert_eq!(
            run(),
            run(),
            "same script + seed must yield byte-identical save"
        );
    }

    #[test]
    fn save_load_roundtrips() {
        let mut fam = Familiar::new(TwinConfig::default());
        for i in 0..15 {
            fam.receive(&groove(0, 220 + i * 10, 4, 0.6, Voice::neutral()));
        }
        let bytes = fam.save();
        let loaded = Familiar::load(&bytes).expect("load");
        assert_eq!(loaded.memory_len(), fam.memory_len());
        assert_eq!(loaded.signals().palette_depth, fam.signals().palette_depth);
        assert_eq!(loaded.save(), bytes, "re-save must match");
    }

    #[test]
    fn sigil_emerges_from_history() {
        let mut fam = Familiar::new(TwinConfig::default());
        for i in 0..30 {
            fam.receive(&groove(0, 180 + (i % 7) * 30, 4, 0.5, Voice::neutral()));
        }
        let path = fam.sigil_path(256);
        assert!(!path.is_empty(), "a played-in familiar should have a sigil");
        // bounded in the normalized square
        for (x, y) in &path {
            assert!(
                x.abs() <= 1.001 && y.abs() <= 1.001,
                "sigil point out of unit square"
            );
        }
    }

    #[test]
    fn haunting_needs_history() {
        let mut fam = Familiar::new(TwinConfig::default());
        assert!(fam.haunting().is_none(), "no hauntings without deep memory");
        for i in 0..20 {
            fam.receive(&groove(0, 250 + i * 5, 3, 0.5, Voice::neutral()));
        }
        let h = fam.haunting();
        assert!(h.is_some(), "a played-in familiar can be haunted");
        let (kb, age) = h.unwrap();
        assert!(!kb.is_empty() && age > 0);
    }
}
