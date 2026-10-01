// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Analog controls from any HID device's own report descriptor: sticks, triggers, hats, sliders.
//!
//! No device table. A device declares its value fields (usage page, usage, logical range); what
//! each field IS is learned from how it behaves. An axis that settles mid-range is a stick axis
//! (centred, two directions); one that settles at an end of its range is a trigger (one
//! direction). Settling is measured, not assumed: every stretch the value holds still votes for
//! the rest point it held at, weighted by how long it held, so a stick pushed to the edge for a
//! second can't outvote the minutes it spends centred. A Hat Switch (Generic Desktop 0x39) is a
//! direction index with a null "centred" state.
//!
//! Every field then yields DIGITAL controls — each direction of each axis, each cardinal of each
//! hat — with hysteresis, so they bind exactly like keys. Centred axes pair into STICKS (X/Y,
//! Rx/Ry, Z/Rz) whose 2-D deflection drives analog consumers like the radial.

use std::collections::HashMap;

/// Synthetic control pages (the vendor-defined range), so analog-derived controls are ordinary
/// `(page, usage)` pairs. The usage names the source field: `field_page << 8 | field_usage`.
/// An axis pushed toward its logical maximum, or a trigger pulled.
pub const AXIS_POS_PAGE: u16 = 0xFE10;
/// An axis pushed toward its logical minimum.
pub const AXIS_NEG_PAGE: u16 = 0xFE11;
/// A hat direction: usage = `hat_index * 4 + {0 up, 1 right, 2 down, 3 left}`.
pub const HAT_PAGE: u16 = 0xFE39;

/// Deflection at which a stick direction presses, and below which it releases.
const PRESS: f32 = 0.55;
const RELEASE: f32 = 0.35;
/// The same for a trigger, which a light pull should still press.
const TRIGGER_PRESS: f32 = 0.30;
const TRIGGER_RELEASE: f32 = 0.18;
/// Settled time a shape needs before it counts as established.
const ESTABLISHED_MS: f64 = 5_000.0;
/// Once established, a shape other than centred only takes over with this much more settled time,
/// and at least [`OVERTURN_MS`] of it: a trigger held pulled through a long race is still a
/// trigger that rests low.
const OVERTURN_RATIO: f64 = 10.0;
const OVERTURN_MS: f64 = 300_000.0;
/// A value that moves less than this fraction of its range is holding still.
const STILL: f32 = 0.02;
/// How long a value must hold still before the stretch counts as a rest.
const SETTLE_MS: u64 = 120;
/// Rest votes decay once they sum past this, so a device's behaviour can change over a session.
const VOTE_CAP_MS: f64 = 120_000.0;

/// One value field a report descriptor declares, as the OS parsed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub page: u16,
    pub usage: u16,
    pub logical_min: i32,
    pub logical_max: i32,
}

impl Field {
    /// Can this field carry a control? Absolute Generic Desktop / Simulation / Game fields only:
    /// a battery level (Generic Device Controls) or a vendor blob is not something you press.
    #[must_use]
    pub fn is_control(&self) -> bool {
        matches!(self.page, 0x01 | 0x02 | 0x05 | crate::pad::PAD_FIELD_PAGE) && self.logical_max > self.logical_min
    }

    fn is_hat(&self) -> bool {
        self.page == 0x01 && self.usage == 0x39
    }

    /// The synthetic usage naming this field.
    #[must_use]
    pub fn control_usage(&self) -> u16 {
        ((self.page & 0xFF) << 8) | (self.usage & 0xFF)
    }

    fn range(&self) -> f32 {
        (i64::from(self.logical_max) - i64::from(self.logical_min)) as f32
    }

    fn mid(&self) -> f32 {
        (self.logical_min as f32 + self.logical_max as f32) / 2.0
    }
}

/// What a field turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// Rests mid-range: deflects both ways.
    Centred,
    /// Rests at its logical minimum: deflects up toward max.
    RestsLow,
    /// Rests at its logical maximum: deflects down toward min.
    RestsHigh,
    Hat,
    /// Not yet seen at rest (a slider parked partway, a stick held since connect): presses nothing
    /// and aims nothing until it settles somewhere.
    Unknown,
}

#[derive(Debug)]
struct FieldState {
    field: Field,
    /// Settled milliseconds voted for each rest: mid, low end, high end. Decays, so the argmax
    /// follows the recent session.
    votes: [f64; 3],
    /// The same, never decayed: how much evidence each rest has in total.
    total: [f64; 3],
    /// The rest the field is taken to have (an index into `votes`), once it has been seen at one.
    class: Option<usize>,
    last: i32,
    still_since: u64,
    counted_to: u64,
    deflection: f32,
    pos: bool,
    neg: bool,
    hat: [bool; 4],
}

impl FieldState {
    fn new(field: Field, first: i32, now: u64) -> Self {
        let mut s = FieldState {
            field,
            votes: [0.0; 3],
            total: [0.0; 3],
            class: None,
            last: first,
            still_since: now,
            counted_to: now,
            deflection: 0.0,
            pos: false,
            neg: false,
            hat: [false; 4],
        };
        // The first sample is a weak prior: most devices are untouched when they connect.
        if let Some(i) = s.rest_class(first) {
            s.votes[i] = 1.0;
            s.class = Some(i);
        }
        s
    }

    /// Move to the best-supported rest. Settling to centred is always allowed (a trigger never
    /// rests mid-range); leaving an established shape for an end needs overwhelming evidence.
    fn reconsider(&mut self) {
        let best = (0..3).max_by(|&a, &b| self.votes[a].total_cmp(&self.votes[b])).unwrap_or(0);
        if self.votes[best] <= 0.0 {
            return;
        }
        let Some(class) = self.class else {
            self.class = Some(best);
            return;
        };
        if best == class {
            return;
        }
        let established = self.total[class] >= ESTABLISHED_MS;
        if best == 0
            || !established
            || (self.total[best] >= OVERTURN_MS && self.total[best] >= OVERTURN_RATIO * self.total[class])
        {
            self.class = Some(best);
        }
    }

    /// Which rest a raw value could be: mid (0), low end (1), high end (2), or none (a held
    /// deflection partway through the range is never a rest).
    fn rest_class(&self, v: i32) -> Option<usize> {
        let f = &self.field;
        let r = f.range();
        let x = v as f32;
        if (x - f.mid()).abs() <= 0.2 * r {
            Some(0)
        } else if x - f.logical_min as f32 <= 0.1 * r {
            Some(1)
        } else if f.logical_max as f32 - x <= 0.1 * r {
            Some(2)
        } else {
            None
        }
    }

    fn shape(&self) -> Shape {
        if self.field.is_hat() {
            return Shape::Hat;
        }
        match self.class {
            None => Shape::Unknown,
            Some(1) => Shape::RestsLow,
            Some(2) => Shape::RestsHigh,
            Some(_) => Shape::Centred,
        }
    }

    fn observe(&mut self, v: i32, now: u64) {
        let f = self.field;
        if f.is_hat() {
            let n = i64::from(f.logical_max) - i64::from(f.logical_min) + 1;
            let idx = i64::from(v) - i64::from(f.logical_min);
            self.hat = [false; 4];
            if (0..n).contains(&idx) && (n == 4 || n == 8) {
                // Direction index → compass angle (0 = up, clockwise), then the cardinals it lies on.
                let deg = idx as f32 * 360.0 / n as f32;
                for (c, centre) in [0.0f32, 90.0, 180.0, 270.0].into_iter().enumerate() {
                    let d = (deg - centre).rem_euclid(360.0);
                    self.hat[c] = d.min(360.0 - d) < 67.5;
                }
            }
            self.last = v;
            return;
        }
        // Rest voting: time spent still at a rest-like value counts toward that rest.
        if ((i64::from(v) - i64::from(self.last)) as f32).abs() > STILL * f.range() {
            self.still_since = now;
            self.counted_to = now;
        } else if now.saturating_sub(self.still_since) >= SETTLE_MS {
            if let Some(i) = self.rest_class(v) {
                let settled = now.saturating_sub(self.counted_to.max(self.still_since.saturating_add(SETTLE_MS))) as f64;
                self.votes[i] += settled;
                self.total[i] += settled;
            }
            self.counted_to = now;
            let total: f64 = self.votes.iter().sum();
            if total > VOTE_CAP_MS {
                self.votes.iter_mut().for_each(|w| *w *= 0.5);
            }
            self.reconsider();
        }
        self.last = v;
        let x = v as f32;
        self.deflection = match self.shape() {
            Shape::Centred => ((x - f.mid()) / (f.range() / 2.0)).clamp(-1.0, 1.0),
            Shape::RestsLow => ((x - f.logical_min as f32) / f.range()).clamp(0.0, 1.0),
            Shape::RestsHigh => ((f.logical_max as f32 - x) / f.range()).clamp(0.0, 1.0),
            Shape::Hat | Shape::Unknown => 0.0,
        };
        let d = self.deflection;
        let (press, release) = if self.class == Some(0) { (PRESS, RELEASE) } else { (TRIGGER_PRESS, TRIGGER_RELEASE) };
        self.pos = if self.pos { d > release } else { d > press };
        self.neg = if self.neg { d < -release } else { d < -press };
    }
}

/// One stick: a pair of centred axes and its current 2-D deflection (`y` positive = down, the HID
/// convention).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stick {
    /// The pair's control usages, `(x, y)`.
    pub axes: (u16, u16),
    pub x: f32,
    pub y: f32,
}

/// Stable owner of one stick axis pair during a held radial cast.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StickSource {
    pub path: String,
    pub axes: (u16, u16),
}

/// The axis pairs that form sticks when both halves are centred.
const STICK_PAIRS: [(u16, u16); 3] = [(0x30, 0x31), (0x33, 0x34), (0x32, 0x35)];

/// One device's analog state.
#[derive(Debug, Default)]
pub struct Device {
    fields: Vec<FieldState>,
}

impl Device {
    /// Feed one report's values (`(field, raw value)` for every control field the report carried)
    /// at `now` milliseconds. Returns the digital controls currently down.
    pub fn observe(&mut self, values: &[(Field, i32)], now: u64) -> Vec<(u16, u16)> {
        for &(field, v) in values.iter().filter(|(f, _)| f.is_control()) {
            match self.fields.iter_mut().find(|s| s.field == field) {
                Some(s) => s.observe(v, now),
                None => {
                    let mut s = FieldState::new(field, v, now);
                    s.observe(v, now);
                    self.fields.push(s);
                }
            }
        }
        self.down()
    }

    /// The digital controls currently down.
    #[must_use]
    pub fn down(&self) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        let mut hat_index = 0u16;
        for s in &self.fields {
            let u = s.field.control_usage();
            if s.field.is_hat() {
                for (c, on) in s.hat.iter().enumerate() {
                    if *on {
                        out.push((HAT_PAGE, hat_index * 4 + c as u16));
                    }
                }
                hat_index += 1;
                continue;
            }
            if s.pos {
                out.push((AXIS_POS_PAGE, u));
            }
            if s.neg {
                out.push((AXIS_NEG_PAGE, u));
            }
        }
        out
    }

    /// What each field has turned out to be, `(control usage, shape)`.
    #[must_use]
    pub fn shapes(&self) -> Vec<(u16, Shape)> {
        self.fields.iter().map(|s| (s.field.control_usage(), s.shape())).collect()
    }

    /// The device's sticks: Generic Desktop axis pairs whose halves both rest centred.
    #[must_use]
    pub fn sticks(&self) -> Vec<Stick> {
        let centred = |page: u16, usage: u16| {
            self.fields
                .iter()
                .find(|s| s.field.page == page && s.field.usage == usage && s.shape() == Shape::Centred)
        };
        [0x01, crate::pad::PAD_FIELD_PAGE]
            .into_iter()
            .flat_map(|page| STICK_PAIRS.iter().map(move |&pair| (page, pair)))
            .filter_map(|(page, (ux, uy))| {
                let (x, y) = (centred(page, ux)?, centred(page, uy)?);
                Some(Stick { axes: (x.field.control_usage(), y.field.control_usage()), x: x.deflection, y: y.deflection })
            })
            .collect()
    }
}

/// Every device's analog state, keyed by the device's path (one physical unit).
#[derive(Debug, Default)]
pub struct Devices(HashMap<String, Device>);

impl Devices {
    pub fn device(&mut self, path: &str) -> &mut Device {
        self.0.entry(path.to_string()).or_default()
    }

    /// Drop a device that left, so a replug learns afresh.
    pub fn forget(&mut self, path: &str) {
        self.0.remove(path);
    }
}

/// The latest sticks of every device, keyed by device path, for analog consumers (the radial).
static STICKS: std::sync::Mutex<Option<HashMap<String, Vec<Stick>>>> = std::sync::Mutex::new(None);

/// Record a device's current sticks (the input pump calls this per report).
pub fn publish_sticks(path: &str, sticks: Vec<Stick>) {
    let mut g = STICKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = g.get_or_insert_with(HashMap::new);
    if sticks.is_empty() {
        map.remove(path);
    } else {
        map.insert(path.to_string(), sticks);
    }
}

/// Every stick currently known, with the path of the device it belongs to.
#[must_use]
pub fn sticks() -> Vec<(String, Stick)> {
    let g = STICKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.iter()
        .flat_map(|m| m.iter().flat_map(|(p, v)| v.iter().map(move |s| (p.clone(), *s))))
        .collect()
}

/// The most deflected finite stick right now. When `pid` is set, only sticks from that canonical
/// product can aim; a missing source never falls through to an unrelated pad.
#[must_use]
pub fn strongest_stick_on(pid: Option<crate::registry::CanonicalPid>) -> Option<(StickSource, f64, f64)> {
    strongest(sticks(), pid)
}

/// Current sample from the physical path already chosen for one active cast.
#[must_use]
pub fn stick_on_source(pid: Option<crate::registry::CanonicalPid>, source: &StickSource) -> Option<(f64, f64)> {
    stick_on_source_from(sticks(), pid, source)
}

fn stick_on_source_from(items: Vec<(String, Stick)>, pid: Option<crate::registry::CanonicalPid>, source: &StickSource) -> Option<(f64, f64)> {
    items.into_iter().find(|(path, stick)| path == &source.path && stick.axes == source.axes && owns_pid(path, pid))
        .and_then(|(_, stick)| (stick.x.is_finite() && stick.y.is_finite()).then_some((f64::from(stick.x), f64::from(stick.y))))
}

fn owns_pid(path: &str, pid: Option<crate::registry::CanonicalPid>) -> bool {
    pid.is_none_or(|expected| {
        u16::from_str_radix(&crate::controls::pid_from_path(path), 16)
            .ok().map(crate::registry::CanonicalPid::of) == Some(expected)
    })
}

fn strongest(items: Vec<(String, Stick)>, pid: Option<crate::registry::CanonicalPid>) -> Option<(StickSource, f64, f64)> {
    items.into_iter()
        .filter(|(path, s)| owns_pid(path, pid) && s.x.is_finite() && s.y.is_finite())
        .map(|(path, stick)| (StickSource { path, axes: stick.axes }, f64::from(stick.x), f64::from(stick.y)))
        .max_by(|a, b| a.1.hypot(a.2).total_cmp(&b.1.hypot(b.2))
            .then_with(|| b.0.path.cmp(&a.0.path))
            .then_with(|| (b.0.axes.0, b.0.axes.1).cmp(&(a.0.axes.0, a.0.axes.1))))
}

/// A human name for an analog-derived control: the descriptor's own axis name with its direction.
#[must_use]
pub fn control_label(page: u16, usage: u16) -> Option<String> {
    let axis = |u: u16| -> String {
        let (fpage, fusage) = (u >> 8, u & 0xFF);
        if fpage == crate::pad::PAD_FIELD_PAGE {
            if let Some(name) = crate::pad::field_name(fusage) {
                return name.into();
            }
        }
        match (fpage, fusage) {
            // X/Y and Rx/Ry are the two sticks on every pad descriptor surveyed; Z/Rz are a
            // trigger pair on some and a right stick on others, so they keep their axis names.
            (0x01, 0x30 | 0x31) => "L stick".into(),
            (0x01, 0x32) => "Z".into(),
            (0x01, 0x33 | 0x34) => "R stick".into(),
            (0x01, 0x35) => "Rz".into(),
            (0x01, 0x36) => "Slider".into(),
            (0x01, 0x37) => "Dial".into(),
            (0x01, 0x38) => "Wheel".into(),
            (0x02, 0xBB) => "Throttle".into(),
            (0x02, 0xBA) => "Rudder".into(),
            (0x02, 0xC4) => "Accelerator".into(),
            (0x02, 0xC5) => "Brake".into(),
            _ => format!("Axis {fpage:02X}/{fusage:02X}"),
        }
    };
    let vertical = |u: u16| matches!(u & 0xFF, 0x31 | 0x34);
    let horizontal_stick = |u: u16| matches!(u >> 8, 0x01 | crate::pad::PAD_FIELD_PAGE) && matches!(u & 0xFF, 0x30 | 0x33);
    let trigger = |u: u16| u >> 8 == crate::pad::PAD_FIELD_PAGE && matches!(u & 0xFF, 0x32 | 0x35);
    Some(match page {
        AXIS_POS_PAGE if vertical(usage) => format!("{} ↓", axis(usage)),
        AXIS_NEG_PAGE if vertical(usage) => format!("{} ↑", axis(usage)),
        AXIS_POS_PAGE if horizontal_stick(usage) => format!("{} →", axis(usage)),
        AXIS_NEG_PAGE if horizontal_stick(usage) => format!("{} ←", axis(usage)),
        AXIS_POS_PAGE if trigger(usage) => axis(usage),
        AXIS_POS_PAGE => format!("{} +", axis(usage)),
        AXIS_NEG_PAGE => format!("{} −", axis(usage)),
        HAT_PAGE => {
            let dir = ["↑", "→", "↓", "←"][usize::from(usage % 4)];
            match usage / 4 {
                0 => format!("Hat {dir}"),
                n => format!("Hat {} {dir}", n + 1),
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The Xbox-compatible pad census, 2026-09-30: X/Y/Rx/Ry 0..65535, Z/Rz 0..1023, hat 1..8.
    const X: Field = Field { page: 0x01, usage: 0x30, logical_min: 0, logical_max: 65535 };
    const Y: Field = Field { page: 0x01, usage: 0x31, logical_min: 0, logical_max: 65535 };
    const Z: Field = Field { page: 0x01, usage: 0x32, logical_min: 0, logical_max: 1023 };
    const HAT: Field = Field { page: 0x01, usage: 0x39, logical_min: 1, logical_max: 8 };
    const BATTERY: Field = Field { page: 0x06, usage: 0x20, logical_min: 0, logical_max: 255 };

    #[test]
    fn stick_selection_filters_nonfinite_and_never_falls_back_from_a_pad_pid() {
        let pid = crate::registry::CanonicalPid::of(0x1234);
        let stick = |x, y| Stick { axes: (0x0130, 0x0131), x, y };
        let samples = vec![
            ("HID#VID_045E&PID_1234#nan".into(), stick(f32::NAN, 1.0)),
            ("HID#VID_045E&PID_1234#own".into(), stick(0.3, 0.4)),
            ("HID#VID_045E&PID_9876#other".into(), stick(0.0, 0.9)),
        ];
        let picked = strongest(samples.clone(), Some(pid)).unwrap();
        assert_eq!(picked.0.path, "HID#VID_045E&PID_1234#own");
        assert_eq!(strongest(samples, None).unwrap().0.path, "HID#VID_045E&PID_9876#other");
        assert_eq!(strongest(vec![("HID#VID_045E&PID_9876#other".into(), stick(0.0, 1.0))], Some(pid)), None);
    }

    #[test]
    fn cast_source_latches_the_chosen_axis_pair_and_does_not_drift_to_another_stick() {
        let pid = crate::registry::CanonicalPid::of(0x1234);
        let path = "HID#VID_045E&PID_1234#pad".to_string();
        let left = Stick { axes: (0x0130, 0x0131), x: 0.0, y: 0.0 };
        let right = Stick { axes: (0x0133, 0x0134), x: 0.8, y: 0.0 };
        let source = strongest(vec![(path.clone(), left), (path.clone(), right)], Some(pid)).unwrap().0;
        assert_eq!(source.axes, (0x0133, 0x0134));
        let next = vec![
            (path.clone(), Stick { x: 0.9, y: 0.0, ..left }),
            (path.clone(), Stick { x: 0.7, y: 0.1, ..right }),
        ];
        let (x, y) = stick_on_source_from(next, Some(pid), &source).unwrap();
        assert!((x - 0.7).abs() < 1e-6 && (y - 0.1).abs() < 1e-6);
        assert_eq!(stick_on_source_from(vec![(path, left)], Some(pid), &source), None, "missing chosen axis pair is a source loss");
    }

    fn run(d: &mut Device, values: &[(Field, i32)], from: u64, to: u64) -> Vec<(u16, u16)> {
        let mut last = Vec::new();
        let mut t = from;
        while t <= to {
            last = d.observe(values, t);
            t += 8; // a 125 Hz report stream
        }
        last
    }

    #[test]
    fn axes_learn_what_they_are_from_where_they_rest() {
        let mut d = Device::default();
        run(&mut d, &[(X, 32768), (Y, 32767), (Z, 0)], 0, 1_000);
        let shapes = d.shapes();
        assert!(shapes.contains(&(0x0130, Shape::Centred)));
        assert!(shapes.contains(&(0x0132, Shape::RestsLow)), "a trigger rests at an end");
        assert_eq!(d.sticks().len(), 1, "X/Y pair into a stick");
        assert!(d.down().is_empty(), "nothing is pressed at rest");
    }

    #[test]
    fn a_stick_held_at_the_edge_does_not_become_a_trigger() {
        let mut d = Device::default();
        run(&mut d, &[(X, 32768), (Y, 32768)], 0, 60_000); // a minute centred
        let held = run(&mut d, &[(X, 65535), (Y, 32768)], 60_008, 63_000); // three seconds right
        assert!(d.shapes().contains(&(0x0130, Shape::Centred)));
        assert_eq!(held, vec![(AXIS_POS_PAGE, 0x0130)]);
        let s = d.sticks()[0];
        assert!(s.x > 0.99 && s.y.abs() < 0.01);
    }

    #[test]
    fn a_device_first_seen_mid_press_relearns_its_rest() {
        let mut d = Device::default();
        // Connected with the stick held right: the weak first-sample prior says "rests high".
        run(&mut d, &[(X, 65535)], 0, 200);
        // Then it spends seconds centred: the settled time outvotes the prior.
        run(&mut d, &[(X, 32768)], 208, 5_000);
        assert!(d.shapes().contains(&(0x0130, Shape::Centred)));
    }

    #[test]
    fn a_trigger_held_through_a_long_race_is_still_a_trigger() {
        let mut d = Device::default();
        run(&mut d, &[(Z, 0)], 0, 20_000); // twenty seconds at rest
        let held = run(&mut d, &[(Z, 1023)], 20_008, 140_000); // two minutes pulled
        assert!(d.shapes().contains(&(0x0132, Shape::RestsLow)), "{:?}", d.shapes());
        assert_eq!(held, vec![(AXIS_POS_PAGE, 0x0132)], "pulled reads as pressed the whole time");
        assert!(d.observe(&[(Z, 0)], 140_008).is_empty(), "and releases when let go");
    }

    #[test]
    fn a_light_trigger_pull_presses() {
        let mut d = Device::default();
        run(&mut d, &[(Z, 0)], 0, 1_000);
        assert_eq!(d.observe(&[(Z, 360)], 1_008), vec![(AXIS_POS_PAGE, 0x0132)], "a third of the travel");
        assert!(d.observe(&[(Z, 150)], 1_016).is_empty());
    }

    #[test]
    fn a_slider_parked_partway_presses_nothing_until_it_rests() {
        let slider = Field { page: 0x01, usage: 0x36, logical_min: 0, logical_max: 1000 };
        let mut d = Device::default();
        assert!(run(&mut d, &[(slider, 800)], 0, 3_000).is_empty(), "never seen at rest");
        assert!(d.sticks().is_empty());
        assert!(d.shapes().contains(&(0x0136, Shape::Unknown)));
        run(&mut d, &[(slider, 0)], 3_008, 4_000);
        assert!(d.shapes().contains(&(0x0136, Shape::RestsLow)));
        assert_eq!(d.observe(&[(slider, 800)], 4_008), vec![(AXIS_POS_PAGE, 0x0136)]);
    }

    #[test]
    fn a_full_signed_32_bit_range_does_not_overflow() {
        let wide = Field { page: 0x01, usage: 0x30, logical_min: i32::MIN, logical_max: i32::MAX };
        let mut d = Device::default();
        d.observe(&[(wide, i32::MIN)], 0);
        d.observe(&[(wide, i32::MAX)], 8);
    }

    #[test]
    fn directions_press_with_hysteresis() {
        let mut d = Device::default();
        run(&mut d, &[(X, 32768)], 0, 500);
        let at = |f: f32| 32768 + (f * 32767.0) as i32;
        assert_eq!(d.observe(&[(X, at(0.5))], 508), vec![], "below the press threshold");
        assert_eq!(d.observe(&[(X, at(0.6))], 516), vec![(AXIS_POS_PAGE, 0x0130)]);
        assert_eq!(d.observe(&[(X, at(0.4))], 524), vec![(AXIS_POS_PAGE, 0x0130)], "held through the gap");
        assert_eq!(d.observe(&[(X, at(0.3))], 532), vec![], "released below the release threshold");
        assert_eq!(d.observe(&[(X, 32768 - (0.7 * 32768.0) as i32)], 540), vec![(AXIS_NEG_PAGE, 0x0130)]);
    }

    #[test]
    fn the_hat_reports_cardinals_and_diagonals_press_two() {
        let mut d = Device::default();
        assert_eq!(d.observe(&[(HAT, 1)], 0), vec![(HAT_PAGE, 0)], "1 = up");
        assert_eq!(d.observe(&[(HAT, 2)], 8), vec![(HAT_PAGE, 0), (HAT_PAGE, 1)], "up-right");
        assert_eq!(d.observe(&[(HAT, 5)], 16), vec![(HAT_PAGE, 2)], "down");
        assert_eq!(d.observe(&[(HAT, 0)], 24), vec![], "the null value is centred");
    }

    #[test]
    fn non_control_fields_are_ignored() {
        let mut d = Device::default();
        assert!(d.observe(&[(BATTERY, 200)], 0).is_empty());
        assert!(d.shapes().is_empty());
    }

    #[test]
    fn labels_come_from_the_descriptor_names() {
        assert_eq!(control_label(AXIS_POS_PAGE, 0x0130).as_deref(), Some("L stick →"));
        assert_eq!(control_label(AXIS_NEG_PAGE, 0x0134).as_deref(), Some("R stick ↑"));
        assert_eq!(control_label(AXIS_POS_PAGE, 0x0132).as_deref(), Some("Z +"));
        assert_eq!(control_label(HAT_PAGE, 1).as_deref(), Some("Hat →"));
        assert_eq!(control_label(0x09, 1), None);
    }
}
