// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Vibration, on any device a platform backend can drive. Backends register a [`Sink`]; callers
//! name a device (the same path its controls and sticks are published under) and never see which
//! API moves the motors.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Motor strengths 0..1: the two body motors (heavy low-frequency, light high-frequency) and the
/// trigger motors pads that have them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rumble {
    pub low: f32,
    pub high: f32,
    pub left_trigger: f32,
    pub right_trigger: f32,
}

impl Rumble {
    pub const OFF: Rumble = Rumble { low: 0.0, high: 0.0, left_trigger: 0.0, right_trigger: 0.0 };
}

/// A platform backend that can move some devices' motors.
pub trait Sink: Send + Sync {
    /// Whether this sink can move `device`'s motors, found without writing to it.
    fn owns(&self, device: &str) -> bool;
    /// Set `device`'s motors; `false` when this sink doesn't own the device.
    fn set(&self, device: &str, rumble: Rumble) -> bool;
}

static SINKS: Mutex<Vec<Box<dyn Sink>>> = Mutex::new(Vec::new());
/// Per device, bumped per pulse so an older pulse's stop never cuts a newer one short on the same
/// device (and never spares another device's motors).
static GENERATION: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

fn bump(device: &str) -> u64 {
    let mut g = GENERATION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let n = g.get_or_insert_with(HashMap::new).entry(device.to_string()).or_default();
    *n += 1;
    *n
}

fn current(device: &str, generation: u64) -> bool {
    GENERATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .and_then(|m| m.get(device))
        == Some(&generation)
}

/// Register a backend.
pub fn register(sink: Box<dyn Sink>) {
    SINKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(sink);
}

/// Set a device's motors until changed, through every backend that owns it (an API can accept a
/// device and still not reach its motors, so none is trusted to be the only one). `false` when no
/// backend owns the device.
pub fn set(device: &str, rumble: Rumble) -> bool {
    SINKS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .fold(false, |owned, s| s.set(device, rumble) | owned)
}

/// Vibrate `device` for `ms` milliseconds, then stop. A newer pulse supersedes an older one.
pub fn pulse(device: &str, rumble: Rumble, ms: u64) -> bool {
    let generation = bump(device);
    if !set(device, rumble) {
        return false;
    }
    let device = device.to_string();
    crate::worker::spawn_detached("neuron-haptics", move || {
        std::thread::sleep(Duration::from_millis(ms));
        if current(&device, generation) {
            set(&device, Rumble::OFF);
        }
    });
    true
}

/// Play a pattern on `device`: each step holds its strengths for its duration, then the motors
/// stop. Runs on its own thread; a newer pulse or pattern supersedes it mid-way.
pub fn play(device: &str, steps: Vec<(Rumble, u64)>) -> bool {
    let generation = bump(device);
    let Some((first, _)) = steps.first() else { return false };
    if !set(device, *first) {
        return false;
    }
    let device = device.to_string();
    crate::worker::spawn_detached("neuron-haptics", move || {
        for (i, (r, ms)) in steps.iter().enumerate() {
            if !current(&device, generation) {
                return;
            }
            if i > 0 {
                set(&device, *r);
            }
            std::thread::sleep(Duration::from_millis(*ms));
        }
        if current(&device, generation) {
            set(&device, Rumble::OFF);
        }
    });
    true
}

/// Stop every motor neuron can reach (app exit: a GameInput pad keeps whatever it was last told).
pub fn stop_all() {
    for d in devices() {
        bump(&d);
        set(&d, Rumble::OFF);
    }
}

/// Every device with sticks that a backend can move the motors of, for callers that just want
/// "the pad". Asks the backends; writes nothing.
#[must_use]
pub fn devices() -> Vec<String> {
    let mut out: Vec<String> = crate::analog::sticks().into_iter().map(|(p, _)| p).collect();
    out.sort();
    out.dedup();
    let sinks = SINKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    out.retain(|d| sinks.iter().any(|s| s.owns(d)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Recorder(Arc<Mutex<Vec<(String, Rumble)>>>);
    impl Sink for Recorder {
        fn owns(&self, device: &str) -> bool {
            device.starts_with("haptics-test#")
        }
        fn set(&self, device: &str, rumble: Rumble) -> bool {
            if device.starts_with("haptics-test#") {
                self.0.lock().unwrap().push((device.to_string(), rumble));
                return true;
            }
            false
        }
    }

    const TICK: Rumble = Rumble { low: 0.0, high: 0.35, left_trigger: 0.0, right_trigger: 0.0 };

    #[test]
    fn a_pulse_reaches_the_owning_sink_and_stops() {
        let log = Arc::new(Mutex::new(Vec::new()));
        register(Box::new(Recorder(log.clone())));
        assert!(!set("nobody#owns-this", TICK), "unowned devices report false");
        assert!(pulse("haptics-test#pad", TICK, 10));
        std::thread::sleep(Duration::from_millis(200));
        let got = log.lock().unwrap().clone();
        assert_eq!(got.first().map(|g| g.1), Some(TICK));
        assert_eq!(got.last().map(|g| g.1), Some(Rumble::OFF));
    }
}
