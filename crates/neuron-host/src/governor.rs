// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The restart governor — supervision as a damped second-order system.
//!
//! A naive retry loop is first-order: it either hammers (restart storm) or
//! over-backs-off (dead subsystem nobody restarts). We model restart *pressure*
//! with the same AR(2) recurrence the eigenmotion stack is built on
//! (`z[n] = K·z[n-1] − G·z[n-2]`, see glyph.rs / engram): each crash injects an
//! impulse into the state, and between crashes the free recurrence dissipates
//! pressure back toward zero. The restart delay is `base + pressure·scale`, so:
//!
//! - an isolated crash → small pressure → near-instant restart, then the
//!   pressure decays and the system forgets;
//! - a burst of crashes → impulses arrive faster than the decay → pressure
//!   compounds → delays stretch → past the escalation threshold the micro tier
//!   gives up and hands the failure up (macro tier / human).
//!
//! The stability claim is not vibes: for the characteristic polynomial
//! `x² − Kx + G`, both roots lie inside the unit circle iff (Jury criterion,
//! 2nd order) `|G| < 1` and `|K| < 1 + G`. `Config::validate` enforces it at
//! construction — a governor that could storm is unrepresentable. The default
//! is the critically-damped repeated root `λ = 0.8` (K = 2λ, G = λ²): fastest
//! decay with zero oscillation, i.e. no thrash between "too eager" and "too
//! shy".
//!
//! The ledger mirrors the codec's energy-capture accounting: how much failure
//! was absorbed at this tier vs escalated past it. A rising escalation ratio
//! means the system is failing in ways this tier doesn't model — the residual,
//! in codec terms — which is exactly the number a health readout should show.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// AR(2) coefficients of the free (between-crash) pressure recurrence.
    pub k: f64,
    pub g: f64,
    /// Wall-clock length of one recurrence step.
    pub step: Duration,
    /// Pressure injected per crash.
    pub impulse: f64,
    /// Restart delay = base + pressure × per_unit (clamped to max).
    pub base_delay: Duration,
    pub delay_per_unit: Duration,
    pub max_delay: Duration,
    /// Pressure at which this tier stops absorbing and escalates.
    pub escalate_at: f64,
}

impl Config {
    /// Critically-damped default: repeated root λ (0 < λ < 1) ⇒ K = 2λ, G = λ².
    /// Impulse response is (a + b·n)·λⁿ — a brief rise, then monotone decay,
    /// no oscillation.
    pub fn critically_damped(lambda: f64) -> Config {
        Config {
            k: 2.0 * lambda,
            g: lambda * lambda,
            step: Duration::from_secs(1),
            impulse: 1.0,
            base_delay: Duration::from_millis(250),
            delay_per_unit: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            escalate_at: 6.0,
        }
    }

    /// Jury stability criterion for `x² − Kx + G` (roots strictly inside the
    /// unit circle): |G| < 1 and |K| < 1 + G.
    pub fn is_stable(&self) -> bool {
        self.g.abs() < 1.0 && self.k.abs() < 1.0 + self.g
    }
}

/// Failure-energy accounting, the engram capture metric applied to faults:
/// `absorbed / crashes` is this tier's capture ratio; what escalates is the
/// residual the next tier (or the human) must absorb.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    pub crashes: u64,
    pub absorbed: u64,
    pub escalated: u64,
}

impl Ledger {
    pub fn absorption(&self) -> f64 {
        if self.crashes == 0 {
            1.0
        } else {
            self.absorbed as f64 / self.crashes as f64
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Contain: restart the failed task after this delay.
    RestartAfter(Duration),
    /// This tier is saturated — hand the failure up.
    Escalate,
}

pub struct Governor {
    cfg: Config,
    /// AR(2) state: z1 = pressure now, z2 = pressure one step ago.
    z1: f64,
    z2: f64,
    last: Option<Instant>,
    pub ledger: Ledger,
}

/// Once pressure decays below this it is snapped to zero — with spectral
/// radius < 1 the tail is exponentially small, and snapping keeps `settle`
/// O(short) regardless of how long a subsystem sat quiet.
const FLOOR: f64 = 1e-9;

impl Governor {
    /// Refuses unstable coefficients outright: a supervisor that could
    /// restart-storm is not a thing this type can express.
    pub fn new(cfg: Config) -> Result<Governor, &'static str> {
        if !cfg.is_stable() {
            return Err("unstable (K,G): roots of x^2 - Kx + G must lie inside the unit circle");
        }
        if cfg.step.is_zero() {
            return Err("step must be non-zero");
        }
        Ok(Governor { cfg, z1: 0.0, z2: 0.0, last: None, ledger: Ledger::default() })
    }

    /// Advance the free recurrence by however many whole steps elapsed since
    /// the last event. Stability guarantees this converges; the floor snap
    /// bounds the loop for arbitrarily long quiet periods.
    fn settle(&mut self, now: Instant) {
        let Some(last) = self.last else { return };
        let steps = (now.saturating_duration_since(last).as_nanos()
            / self.cfg.step.as_nanos().max(1)) as u64;
        for _ in 0..steps {
            let z = self.cfg.k * self.z1 - self.cfg.g * self.z2;
            self.z2 = self.z1;
            self.z1 = z;
            if self.z1.abs() < FLOOR && self.z2.abs() < FLOOR {
                self.z1 = 0.0;
                self.z2 = 0.0;
                break;
            }
        }
        if steps > 0 {
            self.last = Some(last + self.cfg.step * (steps as u32));
        }
    }

    /// A supervised task crashed. Returns the containment verdict.
    pub fn on_crash(&mut self, now: Instant) -> Verdict {
        self.settle(now);
        if self.last.is_none() {
            self.last = Some(now);
        }
        self.z1 += self.cfg.impulse;
        self.ledger.crashes += 1;
        if self.z1 >= self.cfg.escalate_at {
            self.ledger.escalated += 1;
            return Verdict::Escalate;
        }
        self.ledger.absorbed += 1;
        let extra = self.cfg.delay_per_unit.as_secs_f64() * self.z1;
        let delay = (self.cfg.base_delay + Duration::from_secs_f64(extra)).min(self.cfg.max_delay);
        Verdict::RestartAfter(delay)
    }

    /// Current pressure (settled to `now`) — the health readout number.
    pub fn pressure(&mut self, now: Instant) -> f64 {
        self.settle(now);
        self.z1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gov() -> Governor {
        Governor::new(Config::critically_damped(0.8)).unwrap()
    }

    #[test]
    fn unstable_coefficients_are_unrepresentable() {
        // λ = 1 ⇒ roots ON the unit circle: pressure never decays. Refused.
        assert!(Governor::new(Config::critically_damped(1.0)).is_err());
        // Blatantly divergent.
        let mut c = Config::critically_damped(0.8);
        c.k = 3.0;
        assert!(Governor::new(c).is_err());
        // The default is fine.
        assert!(Config::critically_damped(0.8).is_stable());
    }

    #[test]
    fn isolated_crash_recovers_quickly_and_is_forgotten() {
        let mut g = gov();
        let t0 = Instant::now();
        match g.on_crash(t0) {
            Verdict::RestartAfter(d) => {
                // One crash: base + 1.0×per_unit = 750ms. Cheap containment.
                assert_eq!(d, Duration::from_millis(750));
            }
            v => panic!("unexpected {v:?}"),
        }
        // After a long quiet period the pressure has fully dissipated — the
        // stability guarantee observed: the system FORGETS.
        let later = t0 + Duration::from_secs(300);
        assert!(g.pressure(later) < 1e-3, "pressure {} should have decayed", g.pressure(later));
        assert_eq!(g.ledger, Ledger { crashes: 1, absorbed: 1, escalated: 0 });
    }

    #[test]
    fn crash_burst_compounds_pressure_and_escalates() {
        let mut g = gov();
        let t0 = Instant::now();
        let mut delays = Vec::new();
        let mut escalated_at = None;
        for i in 0..20u64 {
            // Hammering: a crash every 100ms, far faster than dissipation.
            let t = t0 + Duration::from_millis(100 * i);
            match g.on_crash(t) {
                Verdict::RestartAfter(d) => delays.push(d),
                Verdict::Escalate => {
                    escalated_at = Some(i);
                    break;
                }
            }
        }
        // Delays must grow monotonically under a same-step burst (impulses sum,
        // nothing dissipates within one step) …
        for w in delays.windows(2) {
            assert!(w[1] >= w[0], "burst delays must not shrink: {delays:?}");
        }
        // … and the tier must give up rather than absorb forever.
        let at = escalated_at.expect("a hammering burst must escalate");
        assert!(at >= 2, "should absorb a few crashes before escalating (got {at})");
        assert_eq!(g.ledger.escalated, 1);
        assert!(g.ledger.absorption() < 1.0);
    }

    #[test]
    fn pressure_rises_then_decays_without_oscillation() {
        // The critically-damped impulse response: rises for a few steps, peaks,
        // then decays monotonically — never crossing zero (no thrash).
        let mut g = gov();
        let t0 = Instant::now();
        g.on_crash(t0);
        let mut prev = g.pressure(t0);
        let mut peaked = false;
        for s in 1..80u64 {
            let p = g.pressure(t0 + Duration::from_secs(s));
            assert!(p >= 0.0, "critically damped response must not oscillate below zero");
            if p < prev {
                peaked = true;
            } else {
                assert!(!peaked, "response must be single-peaked, rose again after decaying");
            }
            prev = p;
        }
        assert!(peaked, "response must eventually decay");
        assert!(prev < 1e-2, "tail must approach zero, got {prev}");
    }

    #[test]
    fn spaced_crashes_stay_contained_forever() {
        // Crashes far apart (5min) must NEVER escalate no matter how many —
        // each one finds the pressure fully dissipated. This is the property
        // that distinguishes "flaky adapter, contain it" from "dying adapter,
        // escalate it".
        let mut g = gov();
        let t0 = Instant::now();
        for i in 0..1000u64 {
            let t = t0 + Duration::from_secs(300 * i);
            match g.on_crash(t) {
                Verdict::RestartAfter(d) => assert_eq!(d, Duration::from_millis(750)),
                Verdict::Escalate => panic!("spaced crashes must never escalate (crash #{i})"),
            }
        }
        assert_eq!(g.ledger.absorption(), 1.0);
    }

    #[test]
    fn delay_is_clamped_to_max() {
        let mut c = Config::critically_damped(0.8);
        c.escalate_at = f64::INFINITY; // absorb everything, to probe the clamp
        c.max_delay = Duration::from_secs(2);
        let mut g = Governor::new(c).unwrap();
        let t0 = Instant::now();
        let mut last = Duration::ZERO;
        for i in 0..50u64 {
            if let Verdict::RestartAfter(d) = g.on_crash(t0 + Duration::from_millis(10 * i)) {
                last = d;
            }
        }
        assert_eq!(last, Duration::from_secs(2));
    }
}
