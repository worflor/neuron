// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Pure tone synthesis — the mathematical heart of Neuron's audio. Notification cues now, the
//! knockback rhythm voice later. No I/O, no platform, no allocation in the per-sample hot path:
//! everything is f32, deterministic, and unit-testable. The real-time output layer (cpal) and the
//! offline WAV renderer drive the SAME [`Voice`], so what you audition is exactly what plays live.
//!
//! Design, from first principles (and the research brief):
//!  - **2-operator FM** (Chowning): one carrier + one modulator. A modulation INDEX that decays
//!    FASTER than the amplitude is what makes a tone read as *struck* — a bright transient that
//!    collapses to a pure ring. That single envelope is the soul of a pleasant mallet/bell.
//!  - **Raised-cosine attack** (zero slope at both ends → no click) then **exponential decay**
//!    (a real resonator loses energy in proportion to what it stores). ~5 ms attack, ~200-260 ms
//!    T60 → short, soft, never fatiguing.
//!  - **One-pole low-pass** rolls off the piercing 2-5 kHz band the ear hates.
//!  - **Major pentatonic** {0,2,4,7,9}: no minor-2nd, no tritone, so ANY combination of its notes
//!    is consonant. This is the guarantee that consecutive cues can never clash — the basis for the
//!    emergent, never-annoying melody a burst of notifications forms.
//!  - **Inverse A-weighting** (gentled) keeps notes even in perceived loudness across pitch.
//!  - **Cubic soft-clip** is the safety net so overlapping voices never clip harshly.

const TAU: f32 = std::f32::consts::TAU;
const PI: f32 = std::f32::consts::PI;
const LN_1000: f32 = 6.907_755; // ln(1000): exp decay constant for a -60 dB (T60) fall.

/// Major-pentatonic scale degrees, in semitones. Contains only {M2, m3, M3, P4, P5} intervals —
/// no minor-2nd and no tritone — so every pair drawn from it is consonant.
pub const PENTATONIC: [i32; 5] = [0, 2, 4, 7, 9];

/// Semitones from A4 (440 Hz) → frequency in Hz, 12-tone equal temperament.
#[must_use]
pub fn hz_from_a4(semitones: f32) -> f32 {
    440.0 * (semitones / 12.0).exp2()
}

/// A pentatonic position → semitones from A4. `degree` indexes the 5-note scale and wraps into
/// octaves (degree 5 is the root one octave up; negative descends). `root` transposes the whole
/// scale (semitones from A4); 0 = A.
#[must_use]
pub fn pentatonic_semitones(degree: i32, root: i32) -> f32 {
    let n = PENTATONIC.len() as i32;
    let octave = degree.div_euclid(n);
    let idx = degree.rem_euclid(n) as usize;
    (root + PENTATONIC[idx] + 12 * octave) as f32
}

/// Frequency (Hz) of a pentatonic degree over a root (semitones from A4).
#[must_use]
pub fn pentatonic_hz(degree: i32, root: i32) -> f32 {
    hz_from_a4(pentatonic_semitones(degree, root))
}

/// A timbre recipe: 2-op FM + envelope. A "palette" is just a named `Timbre`. Fully data-driven and
/// tunable — adding a voice is a new const, not new code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timbre {
    /// Modulator frequency = `ratio` × carrier. Low integer = harmonic/woody; irrational ≈√2 = bell.
    pub ratio: f32,
    /// Peak modulation index — how bright the strike is (more index = more sidebands).
    pub index: f32,
    /// Modulation-index decay time constant, seconds. Smaller = snappier, more percussive strike.
    pub index_tau: f32,
    /// Amplitude attack, seconds (raised-cosine, click-free).
    pub attack: f32,
    /// Amplitude T60: seconds to fall 60 dB (to ~0.1%). The note's length/ring.
    pub t60: f32,
    /// One-pole low-pass cutoff, Hz — tames the piercing highs so it's calm when repeated.
    pub lowpass: f32,
}

impl Timbre {
    /// SOFT PULSE — near-sine, the calmest voice and the house default. A tiny FM index gives it a
    /// touch of body over a pure tone; short, round, never sharp. The committed Neuron sound.
    pub const PULSE: Timbre = Timbre {
        ratio: 1.0,
        index: 0.6,
        index_tau: 0.09,
        attack: 0.008,
        t60: 0.22,
        lowpass: 3400.0,
    };
    /// WARM — a fuller cousin of pulse: a little more index for harmonic body, a darker low-pass and
    /// a longer ring. Cosy and low-fatigue.
    pub const WARM: Timbre = Timbre {
        ratio: 1.0,
        index: 1.3,
        index_tau: 0.11,
        attack: 0.009,
        t60: 0.34,
        lowpass: 2900.0,
    };
    /// GLASS — a SOFT struck-glass shimmer: an irrational ratio (≈√2) for a hint of metallic colour,
    /// but a low index + gentle low-pass keep it from ever clanging. Clear, bright-but-kind.
    pub const GLASS: Timbre = Timbre {
        ratio: 1.414,
        index: 1.7,
        index_tau: 0.14,
        attack: 0.005,
        t60: 0.5,
        lowpass: 5000.0,
    };

    /// The user-facing voice palette, in picker order (default first). One soft family — committed.
    pub const PALETTES: [(&'static str, Timbre); 3] = [
        ("pulse", Self::PULSE),
        ("warm", Self::WARM),
        ("glass", Self::GLASS),
    ];

    /// Resolve a palette slug (config/UI) to a timbre; unknown falls back to the default pulse.
    #[must_use]
    pub fn from_slug(slug: &str) -> Timbre {
        Self::PALETTES
            .iter()
            .find(|(s, _)| *s == slug)
            .map_or(Self::PULSE, |(_, t)| *t)
    }

    /// Compact palette id (0 = pulse) for packing into a real-time strike event.
    #[must_use]
    pub fn id_of(slug: &str) -> u8 {
        Self::PALETTES
            .iter()
            .position(|(s, _)| *s == slug)
            .unwrap_or(0) as u8
    }

    /// Timbre for a packed palette id (out-of-range → default pulse).
    #[must_use]
    pub fn by_id(id: u8) -> Timbre {
        Self::PALETTES.get(id as usize).map_or(Self::PULSE, |(_, t)| *t)
    }
}

/// Inverse A-weighting gain (IEC 61672), gentled to 60% and clamped, so notes read evenly in
/// perceived loudness across pitch instead of bass sounding weak and the mids jumping out.
#[must_use]
pub fn loudness_gain(f: f32) -> f32 {
    let f2 = f * f;
    let num = 12194.0_f32.powi(2) * f2 * f2;
    let den = (f2 + 20.6_f32.powi(2))
        * (f2 + 12194.0_f32.powi(2))
        * ((f2 + 107.7_f32.powi(2)) * (f2 + 737.9_f32.powi(2))).sqrt();
    let a_db = 20.0 * (num / den).log10() + 2.0; // A-weighting (≤ ~+1 dB across the audible band)
    let gain_db = (-0.6 * a_db).clamp(-6.0, 12.0); // compensate 60%; cap the bass boost
    10.0_f32.powf(gain_db / 20.0)
}

/// A sounding note: FM phase accumulators + an envelope clock. `next()` yields one mono sample and
/// advances by one sample. Allocation-free; safe to run inside a real-time audio callback.
#[derive(Clone)]
pub struct Voice {
    fc: f32,        // carrier Hz
    fm: f32,        // modulator Hz (= fc × ratio)
    timbre: Timbre,
    amp: f32,       // peak amplitude (velocity × loudness compensation)
    inv_sr: f32,
    car: f32,       // carrier phase
    md: f32,        // modulator phase
    t: f32,         // seconds since strike (after any delay)
    delay: u32,     // samples to wait before the note sounds (for scheduled gesture notes)
    lp: f32,        // one-pole low-pass state
    lp_k: f32,      // low-pass coefficient
    done: bool,
}

impl Voice {
    /// Strike a note NOW: carrier `freq` Hz, `velocity` 0..1, through `timbre`, at sample rate `sr`.
    #[must_use]
    pub fn strike(freq: f32, velocity: f32, timbre: Timbre, sr: f32) -> Voice {
        let lp_k = 1.0 - (-TAU * timbre.lowpass / sr).exp();
        Voice {
            fc: freq,
            fm: freq * timbre.ratio,
            timbre,
            amp: velocity.clamp(0.0, 1.0) * loudness_gain(freq),
            inv_sr: 1.0 / sr,
            car: 0.0,
            md: 0.0,
            t: 0.0,
            delay: 0,
            lp: 0.0,
            lp_k,
            done: false,
        }
    }

    /// Strike a note that sounds after `delay_samples` of silence — lets a whole multi-note cue
    /// gesture be fired into the engine at once, the voices self-scheduling their own onsets.
    #[must_use]
    pub fn strike_after(freq: f32, velocity: f32, timbre: Timbre, sr: f32, delay_samples: u32) -> Voice {
        let mut v = Self::strike(freq, velocity, timbre, sr);
        v.delay = delay_samples;
        v
    }

    /// Has the note's tail fallen below audibility? (The voice can be reclaimed.)
    #[must_use]
    pub fn done(&self) -> bool {
        self.done
    }

    /// Generate one sample and advance. Returns 0.0 once `done` (or while still delayed).
    #[inline]
    pub fn next(&mut self) -> f32 {
        if self.done {
            return 0.0;
        }
        if self.delay > 0 {
            self.delay -= 1;
            return 0.0;
        }
        // amplitude: raised-cosine attack (no click) → exponential decay (natural ring).
        let a = if self.t < self.timbre.attack {
            0.5 - 0.5 * (PI * self.t / self.timbre.attack).cos()
        } else {
            (-LN_1000 * (self.t - self.timbre.attack) / self.timbre.t60).exp()
        };
        // index decays faster than amplitude → the bright strike collapsing to a pure ring.
        let index = self.timbre.index * (-self.t / self.timbre.index_tau).exp();
        let raw = (self.car + index * self.md.sin()).sin();
        // advance phases (wrapped to keep f32 precision over long rings)
        self.car = (self.car + TAU * self.fc * self.inv_sr).rem_euclid(TAU);
        self.md = (self.md + TAU * self.fm * self.inv_sr).rem_euclid(TAU);
        self.t += self.inv_sr;
        // one-pole low-pass to keep the highs gentle
        let s = self.amp * a * raw;
        self.lp += self.lp_k * (s - self.lp);
        if self.t > self.timbre.attack && a < 0.0003 {
            self.done = true;
        }
        self.lp
    }
}

/// Cubic soft-clip (C¹-continuous, ×1.5 makeup so a full-scale input maps to ±1). The safety net
/// that keeps overlapping voices from clipping harshly — odd-harmonic warmth, not a hard edge.
#[inline]
#[must_use]
pub fn soft_clip(x: f32) -> f32 {
    // A NaN sample must never reach cpal — `x.clamp` alone passes NaN straight through
    // (`f32::clamp` returns `self` when neither comparison against `min`/`max` is true,
    // which NaN always satisfies), so a single poisoned upstream sample would otherwise
    // ride this "safety net" all the way to the audio device as real garbage audio. This
    // is the last stage before the mix leaves `render`/the live callback, so flush here.
    if x.is_nan() {
        return 0.0;
    }
    let x = x.clamp(-1.0, 1.0);
    1.5 * x - 0.5 * x * x * x
}

/// A scheduled strike for offline rendering: sample offset, frequency, velocity, timbre.
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub at: usize,
    pub freq: f32,
    pub vel: f32,
    pub timbre: Timbre,
}

/// Render `hits` into a mono buffer of `len` samples at `sr`, summing overlapping voices, then a
/// `master` gain and the cubic soft-clip safety net. Pure (same `Voice` the live engine runs), so
/// an audition WAV sounds identical to playback.
#[must_use]
pub fn render(hits: &[Hit], len: usize, sr: f32, master: f32) -> Vec<f32> {
    let mut buf = vec![0.0f32; len];
    for h in hits {
        let mut v = Voice::strike(h.freq, h.vel, h.timbre, sr);
        let mut i = h.at;
        while i < len && !v.done() {
            buf[i] += v.next();
            i += 1;
        }
    }
    for s in &mut buf {
        *s = soft_clip(*s * master);
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pentatonic_has_no_dissonant_intervals() {
        // every pair of degrees across two octaves must avoid the minor-2nd (1) and tritone (6).
        for a in 0..10 {
            for b in 0..10 {
                let ia = pentatonic_semitones(a, 0) as i32;
                let ib = pentatonic_semitones(b, 0) as i32;
                let d = (ia - ib).rem_euclid(12);
                assert_ne!(d, 1, "minor 2nd between degrees {a},{b}");
                assert_ne!(d, 6, "tritone between degrees {a},{b}");
                assert_ne!(d, 11, "major 7th between degrees {a},{b}");
            }
        }
    }

    #[test]
    fn a4_is_440() {
        assert!((hz_from_a4(0.0) - 440.0).abs() < 0.001);
        assert!((hz_from_a4(12.0) - 880.0).abs() < 0.001); // octave up
    }

    #[test]
    fn onset_is_clickfree_and_voice_decays() {
        // A click is an ONSET/envelope discontinuity, not the signal's (legit) high-frequency
        // content. The raised-cosine attack means sample 0 starts at ~0 and ramps; every voice must
        // also stay finite and decay to silence. (Bright FM voices legitimately slew fast — that's
        // sidebands, tested for smoothness separately on the near-sine PULSE.)
        let sr = 48_000.0;
        for t in [Timbre::PULSE, Timbre::WARM, Timbre::GLASS] {
            let mut v = Voice::strike(659.25, 0.9, t, sr);
            let first = v.next();
            assert!(first.abs() < 0.02, "{t:?} onset stepped (click) instead of ramping: {first}");
            let mut max = 0.0f32;
            let mut n = 1;
            while !v.done() && n < sr as usize {
                let s = v.next();
                assert!(s.is_finite(), "{t:?} produced a non-finite sample");
                max = max.max(s.abs());
                n += 1;
            }
            assert!(max > 0.05, "{t:?} produced no audible output");
            assert!(max < 2.0, "{t:?} ran away (pre-mix peak {max})");
            assert!(v.done(), "{t:?} never decayed to silence");
        }
    }

    #[test]
    fn pulse_is_smooth() {
        // PULSE is near-sine, so any glitch would show as a large per-sample jump.
        let sr = 48_000.0;
        let mut v = Voice::strike(523.25, 0.9, Timbre::PULSE, sr);
        let mut prev = v.next();
        let mut n = 1;
        while !v.done() && n < sr as usize {
            let s = v.next();
            assert!((s - prev).abs() < 0.15, "discontinuity at {n}: {prev}->{s}");
            prev = s;
            n += 1;
        }
    }

    #[test]
    fn render_bus_never_clips() {
        // six loud overlapping voices: the cubic soft-clip must keep the bus inside [-1, 1].
        let sr = 48_000.0;
        let hits: Vec<Hit> = (0..6)
            .map(|i| Hit {
                at: (i * 12) as usize,
                freq: pentatonic_hz(i, 0),
                vel: 1.0,
                timbre: Timbre::PULSE,
            })
            .collect();
        let buf = render(&hits, sr as usize, sr, 0.8);
        let peak = buf.iter().fold(0.0f32, |m, &s| m.max(s.abs()));
        assert!(peak <= 1.0, "render bus clipped at {peak}");
        assert!(buf.iter().all(|s| s.is_finite()), "render produced non-finite samples");
    }

    #[test]
    fn soft_clip_is_bounded_and_monotonic() {
        assert!((soft_clip(0.0)).abs() < 1e-6);
        assert!(soft_clip(10.0) <= 1.0 && soft_clip(10.0) > 0.9);
        assert!(soft_clip(-10.0) >= -1.0 && soft_clip(-10.0) < -0.9);
        assert!(soft_clip(0.5) > soft_clip(0.4)); // monotonic in the linear-ish region
    }

    // ── Property tests ──────────────────────────────────────────────────────────────
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn cfg() -> ProptestConfig {
            ProptestConfig { cases: 256, ..ProptestConfig::default() }
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (2a) `soft_clip` over the WHOLE f32 space, including NaN/±Inf/subnormals
            /// (`proptest::num::f32::ANY` samples every IEEE-754 category, not just the
            /// "normal" default). A finite/infinite input must land in `[-1,1]`; a NaN input
            /// is the one documented exception — it's flushed to `0.0` (see the comment on
            /// `soft_clip`) rather than riding a NaN through the mix to the audio device,
            /// which is the fix this property pins.
            #[test]
            fn soft_clip_is_bounded_for_all_floats(x in proptest::num::f32::ANY) {
                let y = soft_clip(x);
                if x.is_nan() {
                    prop_assert_eq!(y, 0.0, "NaN in must never reach the audio device as NaN");
                } else {
                    prop_assert!(y.is_finite(), "soft_clip({x}) = {y} is not finite");
                    prop_assert!((-1.0..=1.0).contains(&y), "soft_clip({x}) = {y} out of [-1,1]");
                }
            }
        }

        fn any_timbre() -> impl Strategy<Value = Timbre> {
            prop_oneof![Just(Timbre::PULSE), Just(Timbre::WARM), Just(Timbre::GLASS)]
        }

        /// A `Hit` with realistic (bounded, audible-range) parameters — the domain
        /// `render` is actually driven with in the live engine, not adversarial input
        /// (hostile-PCM-style fuzzing belongs to the DSP kernels in `audio_spectrum.rs`).
        fn any_hit(max_at: usize) -> impl Strategy<Value = Hit> {
            (0..max_at, 20.0f32..20_000.0, 0.0f32..=1.0, any_timbre()).prop_map(
                move |(at, freq, vel, timbre)| Hit { at, freq, vel, timbre },
            )
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (2b) Arbitrary small cue gestures (bounded count/params, matching how the
            /// engine actually schedules cues) always render to a fully finite, bounded
            /// buffer — `render`'s whole job is to be the safe, always-playable offline twin
            /// of the live callback.
            #[test]
            fn render_produces_finite_bounded_samples(
                hits in prop::collection::vec(any_hit(4_800), 0..8),
                master in 0.0f32..=2.0,
            ) {
                let sr = 48_000.0;
                let buf = render(&hits, 4_800, sr, master);
                prop_assert_eq!(buf.len(), 4_800);
                for &s in &buf {
                    prop_assert!(s.is_finite(), "render produced a non-finite sample: {s}");
                    prop_assert!((-1.0..=1.0).contains(&s), "render produced an out-of-range sample: {s}");
                }
            }
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (2c) `loudness_gain` is NOT monotone in frequency — it's the INVERSE of the
            /// A-weighting curve (module docs: "keeps notes even in perceived loudness across
            /// pitch"), and A-weighting itself is U-shaped (attenuates bass and highs relative
            /// to the ~2-4kHz presence band), so the inverse/compensation gain is U-shaped too:
            /// highest near the frequency extremes, lowest around ~3kHz. Numerically verified
            /// (not just by inspection): `loudness_gain(20.0) ≈ 3.98` (saturated at the +12dB
            /// clamp) while `loudness_gain(3000.0) ≈ 0.92` — asserting global monotonicity
            /// would be a FALSE law, so this property instead pins the one thing that IS true
            /// by construction: `gain_db` is unconditionally `.clamp(-6.0, 12.0)` before the
            /// `10^(gain_db/20)` conversion, so for any FINITE frequency the gain is bounded to
            /// `[10^(-6/20), 10^(12/20)]` regardless of how the A-weighting math resolves.
            #[test]
            fn loudness_gain_is_bounded_for_finite_freqs(f in 0.0f32..48_000.0) {
                let g = loudness_gain(f);
                prop_assert!(g.is_finite(), "loudness_gain({f}) = {g} is not finite");
                let lo = 10.0f32.powf(-6.0 / 20.0);
                let hi = 10.0f32.powf(12.0 / 20.0);
                prop_assert!(g >= lo - 1e-4 && g <= hi + 1e-4, "loudness_gain({f}) = {g} outside the clamp-implied [{lo},{hi}]");
            }
        }
    }
}
