// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Lighting-as-telemetry — infer game events from the decoded Chroma stream.
//!
//! Every Chroma game continuously projects a lossy view of its internal state onto
//! the keys: ability cooldowns as a key going dark then lighting when ready, ult
//! charge as a brightness fill, alerts as a periodic pulse, damage as a red flash.
//! Once the SDK stream is decoded (see [`super::chroma_shm`]) that projection is a
//! clean, ms-timestamped signal `L(led, t)` — so recovering the events is an
//! estimation problem, solved here with no game API and nothing in the game process.
//!
//! Pure state machine, per the adapter contract: `(timestamp_ms, frame)` in, a list
//! of [`LightEvent`] out. No I/O, no clock — time arrives with each frame, which is
//! what makes it replay-testable against synthetic signals (see the tests).
//!
//! The math is deliberately light and robust (no allocation per frame beyond the
//! bounded history): perceptual luma, hysteresis onset/offset, a least-squares slope
//! for ramp + ETA, and mean-upcrossing counting for pulse frequency.
//!
//! Pure, safe Rust — enforced: this module handles no OS resources, so it forbids
//! `unsafe` outright.
//!
//! R&D icebox: fully built and unit-tested, but consumed by nothing in the app
//! today — no caller feeds it decoded Chroma frames. Kept because the codec
//! fixtures and the estimators (hysteresis, ramp/ETA, pulse counting) are the
//! hard part of lighting-as-telemetry; wiring a real consumer (game-event
//! inference from the light stream) is future work, not this module's job.
#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// An 8-bit-per-channel colour, the decoder's output unit.
pub type Rgb = (u8, u8, u8);

/// Perceptual luminance in `[0, 1]`: sRGB channels gamma-linearised (≈2.2) then
/// weighted by the Rec.709 coefficients. Brightness ramps read monotonically in this
/// space, which is what the ramp/pulse detectors assume.
#[must_use]
pub fn luma(c: Rgb) -> f32 {
    let lin = |v: u8| (f32::from(v) / 255.0).powf(2.2);
    0.2126 * lin(c.0) + 0.7152 * lin(c.1) + 0.0722 * lin(c.2)
}

/// A semantic event inferred from one LED's light trajectory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightEvent {
    /// Dark → lit. Often "ability came up" (step-style) or a key activating.
    Onset { led: usize, ts: u32, rgb: Rgb },
    /// Lit → dark. Often "ability used" — the start of a cooldown.
    Offset { led: usize, ts: u32 },
    /// A completed use→ready cycle on a key, with the cooldown duration MEASURED
    /// from the light (offset → next onset). The headline telemetry: the game never
    /// told us the cooldown; we timed it.
    Cooldown { led: usize, ts: u32, duration_ms: u32 },
    /// Brightness rising steadily (a fill / charge). `rate_per_s` is luma/second and
    /// `eta_ms` predicts when it reaches the key's recent max (charge complete).
    Ramp { led: usize, ts: u32, rate_per_s: f32, eta_ms: u32 },
    /// Periodic flashing (an alert / ready-pulse), at the estimated frequency.
    Pulse { led: usize, ts: u32, hz: f32 },
}

#[derive(Clone, Copy)]
struct Sample {
    ts: u32,
    luma: f32,
}

/// Per-LED detector state + bounded history.
struct Track {
    hist: VecDeque<Sample>,
    lit: bool,
    off_since: Option<u32>, // ts of the last offset, for cooldown timing
    ramping: bool,
    pulsing: bool,
    last_rgb: Rgb,
}

impl Track {
    fn new() -> Track {
        Track {
            hist: VecDeque::new(),
            lit: false,
            off_since: None,
            ramping: false,
            pulsing: false,
            last_rgb: (0, 0, 0),
        }
    }
}

/// Detector thresholds. Defaults are tuned for keyboard cooldown/ult/alert patterns
/// at typical game update rates; exposed so callers can retune per device or game.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// History window analysed for ramp/pulse (ms).
    pub window_ms: u32,
    /// Hysteresis: luma above this = lit, below `off_th` = dark. The gap rejects
    /// flicker at the on/off boundary.
    pub on_th: f32,
    pub off_th: f32,
    /// Minimum luma/second slope to call a rise a ramp.
    pub ramp_min_rate: f32,
    /// A ramp is "done" (→ implicit ready) once luma is within this of its recent max.
    pub ramp_done_frac: f32,
    /// Minimum peak-to-peak luma amplitude for a pulse to count.
    pub pulse_min_amp: f32,
    /// Minimum full cycles seen in the window to call it a pulse.
    pub pulse_min_cycles: u32,
    /// Ignore a "cooldown" longer than this (ms) — it's a scene change, not a CD.
    pub max_cooldown_ms: u32,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            window_ms: 2500,
            on_th: 0.12,
            off_th: 0.04,
            ramp_min_rate: 0.15,
            ramp_done_frac: 0.9,
            pulse_min_amp: 0.10,
            pulse_min_cycles: 2,
            max_cooldown_ms: 120_000,
        }
    }
}

/// Infers [`LightEvent`]s from a decoded Chroma frame stream. One per device.
pub struct ChromaAnalyzer {
    tracks: Vec<Track>,
    cfg: Config,
}

impl ChromaAnalyzer {
    #[must_use]
    pub fn new(leds: usize) -> ChromaAnalyzer {
        ChromaAnalyzer::with_config(leds, Config::default())
    }

    #[must_use]
    pub fn with_config(leds: usize, cfg: Config) -> ChromaAnalyzer {
        ChromaAnalyzer {
            tracks: (0..leds).map(|_| Track::new()).collect(),
            cfg,
        }
    }

    /// Ingest one frame at time `ts` (ms), returning any events detected this step.
    /// Frames with a non-advancing `ts` (the game hasn't committed anything new) are
    /// ignored, so polling faster than the game updates costs nothing and never
    /// double-counts.
    pub fn ingest(&mut self, ts: u32, frame: &[Rgb]) -> Vec<LightEvent> {
        let mut events = Vec::new();
        let cfg = self.cfg;
        for (led, &rgb) in frame.iter().enumerate() {
            if led >= self.tracks.len() {
                break;
            }
            let t = &mut self.tracks[led];
            // Dedup: only advance a track when this LED's frame is actually new.
            if t.hist.back().is_some_and(|s| s.ts == ts) {
                continue;
            }
            t.last_rgb = rgb;
            let l = luma(rgb);
            t.hist.push_back(Sample { ts, luma: l });
            while t
                .hist
                .front()
                .is_some_and(|s| ts.wrapping_sub(s.ts) > cfg.window_ms)
            {
                t.hist.pop_front();
            }

            // ── onset / offset (hysteresis) ──
            if !t.lit && l >= cfg.on_th {
                t.lit = true;
                events.push(LightEvent::Onset { led, ts, rgb });
                if let Some(off) = t.off_since.take() {
                    let dur = ts.wrapping_sub(off);
                    if dur <= cfg.max_cooldown_ms {
                        events.push(LightEvent::Cooldown { led, ts, duration_ms: dur });
                    }
                }
            } else if t.lit && l <= cfg.off_th {
                t.lit = false;
                t.ramping = false;
                t.pulsing = false;
                t.off_since = Some(ts);
                events.push(LightEvent::Offset { led, ts });
            }

            // ── ramp (rising fill) + pulse (periodic) need a populated window ──
            if t.hist.len() >= 5 {
                let Some((slope, span_s, lo, hi, mean, upcross, mono_frac)) = window_stats(&t.hist) else {
                    continue;
                };

                // Ramp: a sustained monotonic rise, not a one-sample step (a flash has
                // low mono_frac — one jump then flat — so it's rejected here). ETA
                // predicts arrival at full brightness (charge complete). We can't gate
                // on the window max: during a live rise the newest sample IS the max.
                let rising = slope >= cfg.ramp_min_rate && mono_frac >= 0.6 && l < cfg.ramp_done_frac;
                if t.lit && rising && !t.ramping {
                    t.ramping = true;
                    let eta_ms = if slope > 1e-4 {
                        (((1.0 - l) / slope) * 1000.0).clamp(0.0, cfg.max_cooldown_ms as f32) as u32
                    } else {
                        0
                    };
                    events.push(LightEvent::Ramp { led, ts, rate_per_s: slope, eta_ms });
                } else if t.ramping && slope < cfg.ramp_min_rate * 0.3 {
                    t.ramping = false; // plateaued — charge complete (implicit ready)
                }

                // Pulse: enough amplitude AND enough mean-upcrossings over the span.
                let amp = hi - lo;
                let cycles = upcross; // one upcrossing per period
                let _ = mean;
                if amp >= cfg.pulse_min_amp && cycles >= cfg.pulse_min_cycles && span_s > 0.2 {
                    let hz = cycles as f32 / span_s;
                    if !t.pulsing {
                        t.pulsing = true;
                        events.push(LightEvent::Pulse { led, ts, hz });
                    }
                } else {
                    t.pulsing = false;
                }
            }
        }
        events
    }
}

/// Window statistics for one LED's history, one pass, no allocation: least-squares
/// luma slope (per second), time span (s), min/max/mean luma, count of mean-
/// upcrossings (≈ pulse cycles), and the fraction of adjacent samples that rose
/// (monotonicity — separates a sustained ramp from a single step/flash).
fn window_stats(hist: &VecDeque<Sample>) -> Option<(f32, f32, f32, f32, f32, u32, f32)> {
    let first = hist.front()?;
    let last = hist.back()?;
    let n = hist.len() as f32;
    let t0 = first.ts;
    let (mut sx, mut sy, mut sxx, mut sxy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    let (mut lo, mut hi, mut sum) = (f32::MAX, f32::MIN, 0.0f32);
    for s in hist {
        let x = s.ts.wrapping_sub(t0) as f32 / 1000.0; // seconds
        let y = s.luma;
        sx += x;
        sy += y;
        sxx += x * x;
        sxy += x * y;
        lo = lo.min(y);
        hi = hi.max(y);
        sum += y;
    }
    let denom = n * sxx - sx * sx;
    let slope = if denom.abs() > 1e-6 {
        (n * sxy - sx * sy) / denom
    } else {
        0.0
    };
    let mean = sum / n;
    let span_s = last.ts.wrapping_sub(t0) as f32 / 1000.0;
    let mut upcross = 0u32;
    let mut rose = 0u32;
    let mut prev = first.luma;
    for s in hist.iter().skip(1) {
        if prev < mean && s.luma >= mean {
            upcross += 1;
        }
        if s.luma > prev {
            rose += 1;
        }
        prev = s.luma;
    }
    let mono_frac = if hist.len() > 1 {
        rose as f32 / (hist.len() - 1) as f32
    } else {
        0.0
    };
    Some((slope, span_s, lo, hi, mean, upcross, mono_frac))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(a: &mut ChromaAnalyzer, series: &[(u32, Rgb)]) -> Vec<LightEvent> {
        let mut all = Vec::new();
        for &(ts, rgb) in series {
            all.extend(a.ingest(ts, &[rgb]));
        }
        all
    }

    #[test]
    fn onset_and_offset_with_hysteresis() {
        let mut a = ChromaAnalyzer::new(1);
        let ev = feed(
            &mut a,
            &[
                (0, (0, 0, 0)),
                (16, (255, 255, 255)), // clearly lit
                (32, (255, 255, 255)),
                (48, (0, 0, 0)), // dark
            ],
        );
        assert!(matches!(ev[0], LightEvent::Onset { led: 0, .. }));
        assert!(ev.iter().any(|e| matches!(e, LightEvent::Offset { led: 0, .. })));
    }

    #[test]
    fn measures_a_cooldown_from_light() {
        // Lit, then used (dark) at t=1000, then ready (lit) at t=9000 → CD ≈ 8000 ms.
        let mut a = ChromaAnalyzer::new(1);
        let mut series = vec![(0u32, (255, 255, 255))];
        series.push((1000, (0, 0, 0))); // offset
        // stay dark through the cooldown
        for t in (1200..9000).step_by(400) {
            series.push((t, (0, 0, 0)));
        }
        series.push((9000, (255, 255, 255))); // ready
        let ev = feed(&mut a, &series);
        let cd = ev
            .iter()
            .find_map(|e| match e {
                LightEvent::Cooldown { duration_ms, .. } => Some(*duration_ms),
                _ => None,
            })
            .expect("a cooldown was measured");
        assert!(
            (7900..=8100).contains(&cd),
            "measured cooldown {cd}ms ≈ 8000ms"
        );
    }

    #[test]
    fn detects_a_rising_fill_ramp() {
        // Brightness climbs 0→full over 2s → a Ramp with a sane ETA.
        let mut a = ChromaAnalyzer::new(1);
        let mut series = Vec::new();
        for t in (0..2000).step_by(100) {
            let v = (255.0 * (t as f32 / 2000.0)) as u8;
            series.push((t, (v, v, v)));
        }
        let ev = feed(&mut a, &series);
        assert!(
            ev.iter().any(|e| matches!(e, LightEvent::Ramp { .. })),
            "expected a ramp, got {ev:?}"
        );
    }

    #[test]
    fn detects_a_pulse_frequency() {
        // ~5 Hz square wave for 2s → a Pulse near 5 Hz.
        let mut a = ChromaAnalyzer::new(1);
        let mut series = Vec::new();
        let mut t = 0u32;
        while t < 2000 {
            // 100ms on, 100ms off = one 200ms cycle = 5 Hz
            let on = (t / 100).is_multiple_of(2);
            let c = if on { (255, 255, 255) } else { (0, 0, 0) };
            series.push((t, c));
            t += 20;
        }
        let ev = feed(&mut a, &series);
        let hz = ev.iter().find_map(|e| match e {
            LightEvent::Pulse { hz, .. } => Some(*hz),
            _ => None,
        });
        assert!(hz.is_some(), "expected a pulse, got {ev:?}");
        let hz = hz.unwrap();
        assert!((3.0..=7.0).contains(&hz), "pulse {hz}Hz ≈ 5Hz");
    }
}
