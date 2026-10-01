// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The radial wheel as a physical knob under a gamepad stick, rendered on the pad's motors.
//!
//! Wedge boundaries are detents: a raised-cosine ridge the stick climbs as it nears a boundary
//! (scaled by deflection and by sweep speed, so a still stick parked between wedges only hums),
//! and a click on crossing it that sharpens with speed and pans toward the trigger on the side
//! the stick points. Engaging the wheel, reaching a fan's rim, firing and letting go are short
//! synthesised envelopes over that field. One renderer thread mixes everything at 125 Hz while
//! anything sounds, writes only when the quantised output changes, and sleeps otherwise.
//!
//! The strengths, widths and decay times are taste, tuned by hand on a PowerA pad's four motors
//! (2026-09-30), not measurements; the tests pin their shape (what rises, what decays, what stays
//! in range), not their values.

use crate::haptics::Rumble;
use std::f64::consts::{PI, TAU};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENGAGE: f64 = crate::radial::StickAim::ENGAGE;
/// Deflection at which a fannable wedge's second tier opens, and (a little inside it) closes again.
const RIM_OUT: f64 = crate::radial::FAN_REACH;
const RIM_IN: f64 = RIM_OUT - 0.06;
/// How far past a boundary (in wedges) the stick must go before the crossing counts.
const HYSTERESIS: f64 = 0.06;
/// Half-width of a detent ridge, in wedges.
const RIDGE: f64 = 0.22;
/// Angular-speed smoothing time constant, seconds.
const OMEGA_TAU: f64 = 0.06;
/// Angular speed (rad/s) at which the speed gain reaches 1 − 1/e.
const OMEGA_SCALE: f64 = 4.0;
/// Faster than any thumb turns a stick (a full turn in 60 ms); a larger rate is a glitch.
const OMEGA_MAX: f64 = 100.0;
const FRAME: Duration = Duration::from_millis(8);
/// A stick not reported for this long has left the wheel.
const STALE: Duration = Duration::from_millis(60);
/// Output resolution: 64 levels per motor, and anything under 3 of them is below what an
/// eccentric-mass motor turns at, so it is sent as off.
const LEVELS: f32 = 64.0;
const FLOOR: u8 = 3;

/// A synthesised event over the detent field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cue {
    /// The stick picked up the wheel.
    Engage,
    /// A wedge boundary was crossed: `strength` 0..1, `pan` −1 (left) .. 1 (right).
    Click { strength: f32, pan: f32 },
    /// Pushed out through a fannable wedge's rim.
    Rim,
    /// Came back inside the rim.
    RimBack,
    /// The wheel fired.
    Fire,
    /// The wheel closed without firing.
    Cancel,
}

impl Cue {
    /// The motor strengths `t_ms` after the cue started, or `None` once it has finished.
    #[must_use]
    pub fn at(self, t_ms: f32) -> Option<Rumble> {
        let decay = |tau: f32| (-t_ms / tau).exp();
        match self {
            Cue::Engage => (t_ms < 60.0).then(|| {
                let e = decay(12.0);
                Rumble { low: 0.0, high: 0.28 * e, left_trigger: 0.16 * e, right_trigger: 0.16 * e }
            }),
            Cue::Click { strength, pan } => (t_ms < 60.0).then(|| click(t_ms, strength, pan)),
            Cue::Rim => (t_ms < 110.0).then(|| {
                let a = click(t_ms, 0.7, 0.0);
                let b = if t_ms >= 50.0 { click(t_ms - 50.0, 0.7, 0.0) } else { Rumble::OFF };
                sum(a, b)
            }),
            Cue::RimBack => (t_ms < 60.0).then(|| click(t_ms, 0.3, 0.0)),
            Cue::Fire => (t_ms < 320.0).then(|| {
                // A crisp strike on the fast motors over a heavy body that swells in and settles.
                let attack = (t_ms / 6.0).min(1.0);
                let settle = (-(t_ms - 6.0).max(0.0) / 70.0).exp();
                let t = 0.6 * decay(14.0);
                Rumble { low: 0.7 * attack * settle, high: 0.9 * decay(10.0), left_trigger: t, right_trigger: t }
            }),
            Cue::Cancel => (t_ms < 200.0).then(|| Rumble {
                low: 0.22 * (t_ms / 8.0).min(1.0) * decay(55.0),
                ..Rumble::OFF
            }),
        }
    }
}

/// A detent click: held flat for 10 ms so the motor spins up, then a fast decay.
fn click(t_ms: f32, strength: f32, pan: f32) -> Rumble {
    let e = strength * if t_ms < 10.0 { 1.0 } else { (-(t_ms - 10.0) / 9.0).exp() };
    let (l, r) = equal_power(pan);
    Rumble { low: 0.0, high: 0.55 * e, left_trigger: e * l, right_trigger: e * r }
}

/// Equal-power pan: −1 = all left, 0 = both at 1/√2, 1 = all right.
fn equal_power(pan: f32) -> (f32, f32) {
    let a = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (a.cos().max(0.0), a.sin().max(0.0))
}

fn sum(a: Rumble, b: Rumble) -> Rumble {
    Rumble {
        low: a.low + b.low,
        high: a.high + b.high,
        left_trigger: a.left_trigger + b.left_trigger,
        right_trigger: a.right_trigger + b.right_trigger,
    }
}

fn smoothstep(e0: f64, e1: f64, x: f64) -> f64 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The knob's state between stick samples.
#[derive(Clone, Copy, Debug, Default)]
pub struct Knob {
    /// The last engaged sample's bearing and time (seconds).
    prev: Option<(f64, f64)>,
    /// Smoothed signed angular speed, rad/s.
    omega: f64,
    /// The wedge the stick is in, once engaged.
    cell: Option<usize>,
    rim: bool,
}

impl Knob {
    /// Feed one stick sample (x, y in −1..1, y down) at `t` seconds on a wheel of `sectors`
    /// wedges; `fans(wedge)` says whether pushing out on that wedge opens a second tier. Returns
    /// the detent field's strengths and appends any cues.
    pub fn feel(&mut self, x: f64, y: f64, t: f64, sectors: usize, fans: impl Fn(usize) -> bool, cues: &mut Vec<Cue>) -> Rumble {
        let m = x.hypot(y).min(1.0);
        // A sample that isn't a number (a driver glitch) is a stick at rest, never a motor command.
        if !x.is_finite() || !y.is_finite() || m < ENGAGE || !t.is_finite() {
            *self = Knob::default();
            return Rumble::OFF;
        }
        let n = sectors.max(1);
        let theta = x.atan2(-y).rem_euclid(TAU);
        match self.prev {
            Some((p, pt)) if t - pt >= 0.001 && t - pt < 0.1 => {
                let dt = t - pt;
                let d = (theta - p + PI).rem_euclid(TAU) - PI;
                let rate = (d / dt).clamp(-OMEGA_MAX, OMEGA_MAX);
                self.omega += (1.0 - (-dt / OMEGA_TAU).exp()) * (rate - self.omega);
            }
            // Samples closer than a millisecond carry no usable rate; keep the smoothed one.
            Some((_, pt)) if (0.0..0.001).contains(&(t - pt)) => {}
            Some(_) => self.omega = 0.0,
            None => {}
        }
        self.prev = Some((theta, t));
        let speed = 1.0 - (-self.omega.abs() / OMEGA_SCALE).exp();

        // Bearing in wedges: centres at integers, boundaries at halves.
        let pos = theta / (TAU / n as f64);
        let near = (pos.round() as usize) % n;
        let to_boundary = 0.5 - (pos - pos.round()).abs();
        let pan = (x / x.hypot(y)) as f32;
        match self.cell {
            None => {
                self.cell = Some(near);
                cues.push(Cue::Engage);
            }
            Some(c) if c != near && to_boundary >= HYSTERESIS => {
                self.cell = Some(near);
                cues.push(Cue::Click { strength: (0.45 + 0.5 * speed) as f32, pan });
            }
            _ => {}
        }
        let fannable = self.cell.is_some_and(&fans);
        if !self.rim && fannable && m >= RIM_OUT {
            self.rim = true;
            cues.push(Cue::Rim);
        } else if self.rim && m < RIM_IN {
            self.rim = false;
            cues.push(Cue::RimBack);
        } else if self.rim && !fannable {
            self.rim = false;
        }

        if n < 2 {
            return Rumble::OFF;
        }
        let u = to_boundary / RIDGE;
        let ridge = if u < 1.0 { 0.5 * (1.0 + (PI * u).cos()) } else { 0.0 };
        let a = (ridge * smoothstep(ENGAGE, 0.9, m) * (0.3 + 0.7 * speed)) as f32;
        let (l, r) = equal_power(pan);
        Rumble { low: 0.0, high: 0.4 * a, left_trigger: 0.3 * a * l, right_trigger: 0.3 * a * r }
    }
}

/// Sum the field and every sounding cue (`ms` since each began), clamped to 0..1.
#[must_use]
pub fn mix(field: Rumble, cues: impl IntoIterator<Item = (Cue, f32)>) -> Rumble {
    let r = cues.into_iter().filter_map(|(c, ms)| c.at(ms)).fold(field, sum);
    Rumble {
        low: r.low.clamp(0.0, 1.0),
        high: r.high.clamp(0.0, 1.0),
        left_trigger: r.left_trigger.clamp(0.0, 1.0),
        right_trigger: r.right_trigger.clamp(0.0, 1.0),
    }
}

/// Every motor at `gain` times its strength, clamped to 0..1.
#[must_use]
pub fn scaled(r: Rumble, gain: f32) -> Rumble {
    let g = |x: f32| (x * gain).clamp(0.0, 1.0);
    Rumble { low: g(r.low), high: g(r.high), left_trigger: g(r.left_trigger), right_trigger: g(r.right_trigger) }
}

/// Motor strengths as the levels actually sent; below [`FLOOR`] is off.
#[must_use]
pub fn quantise(r: Rumble) -> [u8; 4] {
    let q = |x: f32| match (x.clamp(0.0, 1.0) * LEVELS).round() as u8 {
        v if v < FLOOR => 0,
        v => v,
    };
    [q(r.low), q(r.high), q(r.left_trigger), q(r.right_trigger)]
}

fn level(q: [u8; 4]) -> Rumble {
    let f = |v: u8| f32::from(v) / LEVELS;
    Rumble { low: f(q[0]), high: f(q[1]), left_trigger: f(q[2]), right_trigger: f(q[3]) }
}

struct Stage {
    device: Option<String>,
    epoch: Option<Instant>,
    knob: Knob,
    field: Rumble,
    fed: Option<Instant>,
    voices: Vec<(Cue, Instant)>,
    sent: [u8; 4],
    scratch: Vec<Cue>,
}

impl Stage {
    fn sounding(&self) -> bool {
        self.fed.is_some() || !self.voices.is_empty() || self.sent != [0; 4]
    }
}

static STAGE: Mutex<Stage> = Mutex::new(Stage {
    device: None,
    epoch: None,
    knob: Knob { prev: None, omega: 0.0, cell: None, rim: false },
    field: Rumble::OFF,
    fed: None,
    voices: Vec::new(),
    sent: [0; 4],
    scratch: Vec::new(),
});
static WAKE: Condvar = Condvar::new();
/// The user's strength for everything the knob plays, as `f32` bits; 1.0 is as tuned.
static INTENSITY: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x3F80_0000);
/// The most a user can turn the knob up to.
pub const MAX_INTENSITY: f64 = 1.5;

/// Set how strongly the knob plays: 0 is off (the motors are never woken), 1 as tuned, up to
/// [`MAX_INTENSITY`].
pub fn set_intensity(x: f64) {
    let x = if x.is_finite() { x.clamp(0.0, MAX_INTENSITY) } else { 1.0 };
    INTENSITY.store((x as f32).to_bits(), std::sync::atomic::Ordering::Relaxed);
}

#[must_use]
pub fn intensity() -> f32 {
    f32::from_bits(INTENSITY.load(std::sync::atomic::Ordering::Relaxed))
}

/// Play the knob's vocabulary on `device` once, so its feel can be judged by hand: the wheel
/// engaging, three detent clicks sweeping left to right, the rim, then a fire.
pub fn demo(device: &str) {
    if intensity() <= 0.0 {
        return;
    }
    let mut s = stage();
    let now = Instant::now();
    if s.device.as_deref() != Some(device) {
        s.device = Some(device.to_string());
        s.sent = [0; 4];
    }
    s.fed = None;
    s.field = Rumble::OFF;
    s.knob = Knob::default();
    let at = |ms: u64| now + Duration::from_millis(ms);
    s.voices = vec![
        (Cue::Engage, at(0)),
        (Cue::Click { strength: 0.55, pan: -0.8 }, at(160)),
        (Cue::Click { strength: 0.7, pan: 0.0 }, at(300)),
        (Cue::Click { strength: 0.9, pan: 0.8 }, at(420)),
        (Cue::Rim, at(620)),
        (Cue::Fire, at(900)),
    ];
    start(s);
}

fn stage() -> std::sync::MutexGuard<'static, Stage> {
    STAGE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The stick on `device` is at (x, y) over a wheel of `sectors` wedges; `fans[i] > 1` when wedge
/// `i` opens a second tier. Call on every tick the wheel is open; a stick not reported for
/// [`STALE`] is treated as gone.
pub fn aim(device: &str, x: f64, y: f64, sectors: usize, fans: &[usize]) {
    if intensity() <= 0.0 {
        return;
    }
    let mut s = stage();
    let now = Instant::now();
    let t = now.duration_since(*s.epoch.get_or_insert(now)).as_secs_f64();
    let previous = if s.device.as_deref() == Some(device) {
        None
    } else {
        s.knob = Knob::default();
        s.voices.clear();
        let old = s.device.replace(device.to_string()).filter(|_| s.sent != [0; 4]);
        // The new device starts at rest; what was sent belonged to the old one.
        s.sent = [0; 4];
        old
    };
    if s.fed.is_some_and(|f| now.duration_since(f) > STALE) {
        s.knob = Knob::default();
    }
    let mut cues = std::mem::take(&mut s.scratch);
    s.field = s.knob.feel(x, y, t, sectors, |w| fans.get(w).is_some_and(|&n| n > 1), &mut cues);
    s.voices.extend(cues.drain(..).map(|c| (c, now)));
    s.scratch = cues;
    // A resting stick keeps nothing awake; the renderer sleeps until it moves out.
    s.fed = s.knob.cell.is_some().then_some(now);
    start(s);
    if let Some(old) = previous {
        crate::haptics::set(&old, Rumble::OFF);
    }
}

/// The wheel fired from the stick: the fire envelope, and the detent field stops.
pub fn fire() {
    cue_and_stop(Cue::Fire, false);
}

/// The wheel closed without firing: a soft let-go, when a stick was on it.
pub fn cancel() {
    cue_and_stop(Cue::Cancel, true);
}

fn cue_and_stop(cue: Cue, only_if_aiming: bool) {
    if intensity() <= 0.0 {
        return;
    }
    let mut s = stage();
    let now = Instant::now();
    let aiming = s.fed.is_some_and(|f| now.duration_since(f) <= STALE) && s.knob.cell.is_some();
    s.fed = None;
    s.field = Rumble::OFF;
    s.knob = Knob::default();
    if s.device.is_none() || (only_if_aiming && !aiming) {
        return;
    }
    s.voices.push((cue, now));
    start(s);
}

/// Wake the renderer, spawning it on first use.
fn start(s: std::sync::MutexGuard<'static, Stage>) {
    static RENDERER: OnceLock<bool> = OnceLock::new();
    let sounding = s.sounding();
    drop(s);
    if !sounding {
        return;
    }
    RENDERER.get_or_init(|| crate::worker::spawn_detached("neuron-knob", render));
    WAKE.notify_one();
}

fn render() {
    let mut s = stage();
    loop {
        while !s.sounding() {
            s = WAKE.wait(s).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let now = Instant::now();
        if s.fed.is_some_and(|f| now.duration_since(f) > STALE) {
            s.fed = None;
            s.field = Rumble::OFF;
            s.knob = Knob::default();
        }
        // A cue scheduled for later (the demo's phrase) waits its turn; one that has played out goes.
        s.voices.retain(|(c, t0)| *t0 > now || c.at(now.duration_since(*t0).as_secs_f32() * 1000.0).is_some());
        let started = s.voices.iter().filter(|(_, t0)| *t0 <= now).map(|(c, t0)| (*c, now.duration_since(*t0).as_secs_f32() * 1000.0));
        let out = quantise(scaled(mix(s.field, started), intensity()));
        let send = (out != s.sent).then(|| s.device.clone()).flatten();
        s.sent = out;
        drop(s);
        if let Some(device) = send {
            crate::haptics::set(&device, level(out));
        }
        std::thread::sleep(FRAME);
        s = stage();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sweep the stick clockwise from `from` to `to` degrees at full deflection over `secs`,
    /// sampling every 4 ms; returns the cues and the peak field.
    fn sweep(knob: &mut Knob, from: f64, to: f64, secs: f64, t0: f64, cues: &mut Vec<Cue>) -> f32 {
        let steps = (secs / 0.004).ceil() as usize;
        let mut peak = 0f32;
        for i in 0..=steps {
            let deg = from + (to - from) * i as f64 / steps as f64;
            let (x, y) = (deg.to_radians().sin(), -deg.to_radians().cos());
            let r = knob.feel(x, y, t0 + i as f64 * 0.004, 8, |_| false, cues);
            peak = peak.max(r.high);
        }
        peak
    }

    fn clicks(cues: &[Cue]) -> Vec<f32> {
        cues.iter().filter_map(|c| if let Cue::Click { strength, .. } = c { Some(*strength) } else { None }).collect()
    }

    #[test]
    fn engaging_announces_itself_once_and_crossing_a_boundary_clicks_once() {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        sweep(&mut k, 0.0, 40.0, 0.5, 0.0, &mut cues);
        assert_eq!(cues.first(), Some(&Cue::Engage));
        assert_eq!(cues.iter().filter(|c| **c == Cue::Engage).count(), 1);
        assert_eq!(clicks(&cues).len(), 1, "8 wedges: 0°→40° crosses the 22.5° boundary once");
    }

    #[test]
    fn a_stick_trembling_on_a_boundary_does_not_chatter() {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        for i in 0..200 {
            let deg: f64 = 22.5 + if i % 2 == 0 { 1.5 } else { -1.5 };
            let (x, y) = (deg.to_radians().sin(), -deg.to_radians().cos());
            k.feel(x, y, f64::from(i) * 0.004, 8, |_| false, &mut cues);
        }
        assert!(clicks(&cues).len() <= 1, "jitter inside the hysteresis band clicked {:?}", clicks(&cues));
    }

    #[test]
    fn a_faster_sweep_clicks_harder_and_climbs_a_taller_ridge() {
        let (mut slow, mut fast) = (Knob::default(), Knob::default());
        let (mut cs, mut cf) = (Vec::new(), Vec::new());
        let ps = sweep(&mut slow, 0.0, 40.0, 2.0, 0.0, &mut cs);
        let pf = sweep(&mut fast, 0.0, 40.0, 0.08, 0.0, &mut cf);
        assert!(clicks(&cf)[0] > clicks(&cs)[0] + 0.1, "{:?} vs {:?}", clicks(&cf), clicks(&cs));
        assert!(pf > ps, "fast ridge {pf} vs slow {ps}");
    }

    #[test]
    fn the_ridge_lives_on_boundaries_not_wedge_centres() {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        let at = |k: &mut Knob, deg: f64, t: f64, cues: &mut Vec<Cue>| {
            k.feel(deg.to_radians().sin(), -deg.to_radians().cos(), t, 8, |_| false, cues).high
        };
        at(&mut k, 0.0, 0.0, &mut cues);
        assert_eq!(at(&mut k, 0.0, 0.2, &mut cues), 0.0, "a wedge centre is flat");
        let mut k = Knob::default();
        at(&mut k, 22.5, 0.0, &mut cues);
        assert!(at(&mut k, 22.5, 0.2, &mut cues) > 0.05, "a boundary hums even at rest");
    }

    #[test]
    fn a_stick_near_centre_is_silent_and_releases_the_wheel() {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        assert_eq!(k.feel(0.1, -0.1, 0.0, 8, |_| false, &mut cues), Rumble::OFF);
        assert!(cues.is_empty());
    }

    #[test]
    fn pushing_through_a_fannable_rim_taps_and_coming_back_releases() {
        let mut k = Knob::default();
        let mut cues = Vec::new();
        for (i, m) in [0.5, 0.7, 0.85, 0.9, 0.78, 0.7].into_iter().enumerate() {
            k.feel(0.0, -m, i as f64 * 0.01, 8, |w| w == 0, &mut cues);
        }
        assert_eq!(cues, vec![Cue::Engage, Cue::Rim, Cue::RimBack]);
        let mut k = Knob::default();
        let mut cues = Vec::new();
        k.feel(0.0, -0.95, 0.0, 8, |_| false, &mut cues);
        assert_eq!(cues, vec![Cue::Engage], "no rim where nothing fans");
    }

    #[test]
    fn a_right_pointing_stick_clicks_on_the_right_trigger() {
        let r = click(0.0, 1.0, 1.0);
        assert!(r.right_trigger > 0.99 && r.left_trigger < 0.01);
        let (l, r) = equal_power(0.0);
        assert!((l * l + r * r - 1.0).abs() < 1e-5, "centre keeps constant power");
    }

    #[test]
    fn every_cue_starts_audible_and_ends() {
        for cue in [Cue::Engage, Cue::Click { strength: 0.5, pan: 0.0 }, Cue::Rim, Cue::RimBack, Cue::Fire, Cue::Cancel] {
            let peak = (0..40).filter_map(|ms| cue.at(ms as f32)).map(|r| quantise(r).into_iter().max().unwrap()).max().unwrap();
            assert!(peak >= FLOOR, "{cue:?} never reaches a motor");
            assert!(cue.at(400.0).is_none(), "{cue:?} never ends");
        }
        let body_early = Cue::Fire.at(2.0).unwrap().low;
        let body_peak = Cue::Fire.at(6.0).unwrap().low;
        assert!(body_peak > body_early, "the fire's body swells in rather than snapping on");
    }

    #[test]
    fn the_users_strength_scales_every_motor_and_zero_is_silence() {
        let fire = mix(Rumble::OFF, [(Cue::Fire, 3.0)]);
        assert_eq!(quantise(scaled(fire, 0.0)), [0; 4], "off sends nothing");
        let loud = scaled(fire, MAX_INTENSITY as f32);
        assert!([loud.low, loud.high, loud.left_trigger, loud.right_trigger].iter().all(|v| (0.0..=1.0).contains(v)));
        assert!(scaled(fire, 0.5).high < fire.high && scaled(fire, 1.2).high > fire.high);
    }

    #[test]
    fn quantising_floors_what_a_motor_cannot_turn_at_and_mixing_clamps() {
        assert_eq!(quantise(Rumble { low: 0.02, high: 1.0, left_trigger: 0.5, right_trigger: 0.0 }), [0, 64, 32, 0]);
        let loud = mix(Rumble { high: 0.9, ..Rumble::OFF }, [(Cue::Fire, 0.0)]);
        assert!(loud.high <= 1.0);
    }
}
