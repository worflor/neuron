// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Everything a device can tell us that isn't a control: battery, motion, any other value its
//! descriptor declares. Platform backends publish; macros, the GUI and future sensor-driven
//! triggers read one registry, whatever the device or OS.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

/// One sensor reading.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Reading {
    /// Charge 0..1, and whether it is charging when the device says.
    Battery { level: f32, charging: Option<bool> },
    /// Acceleration in g and angular velocity in rad/s, device axes.
    Motion { accel: [f32; 3], gyro: [f32; 3] },
    /// Any other declared value, normalized 0..1 over its logical range.
    Value { page: u16, usage: u16, value: f32 },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub device: String,
    pub key: String,
    pub reading: Reading,
    pub at: Instant,
}

static REGISTRY: Mutex<BTreeMap<(String, String), Entry>> = Mutex::new(BTreeMap::new());

/// Record `device`'s latest `key` reading (e.g. "battery", "motion").
pub fn publish(device: &str, key: &str, reading: Reading) {
    let mut g = REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.insert(
        (device.to_string(), key.to_string()),
        Entry { device: device.to_string(), key: key.to_string(), reading, at: Instant::now() },
    );
}

/// A declared non-control value field as a reading: Generic Device Controls' Battery Strength
/// (0x06/0x20) is a battery level, anything else a named value normalized over its logical range.
#[must_use]
pub fn from_field(field: &crate::analog::Field, raw: i32) -> (String, Reading) {
    let span = (i64::from(field.logical_max) - i64::from(field.logical_min)).max(1) as f32;
    let level = ((i64::from(raw) - i64::from(field.logical_min)) as f32 / span).clamp(0.0, 1.0);
    match (field.page, field.usage) {
        (0x06, 0x20) => ("battery".into(), Reading::Battery { level, charging: None }),
        (page, usage) => (format!("{page:02X}/{usage:02X}"), Reading::Value { page, usage, value: level }),
    }
}

/// Forget a device (it disconnected).
pub fn forget(device: &str) {
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).retain(|(d, _), _| d != device);
}

/// Every reading currently known.
#[must_use]
pub fn snapshot() -> Vec<Entry> {
    REGISTRY.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().cloned().collect()
}

/// One-line human form of a reading.
#[must_use]
pub fn describe(r: &Reading) -> String {
    match r {
        Reading::Battery { level, charging } => format!(
            "battery {:.0}%{}",
            level * 100.0,
            match charging {
                Some(true) => " (charging)",
                Some(false) => "",
                None => " (?)",
            }
        ),
        Reading::Motion { accel, gyro } => format!(
            "accel ({:.2}, {:.2}, {:.2}) g · gyro ({:.2}, {:.2}, {:.2}) rad/s",
            accel[0], accel[1], accel[2], gyro[0], gyro[1], gyro[2]
        ),
        Reading::Value { page, usage, value } => format!("{page:02X}/{usage:02X} = {value:.2}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_fields_become_readings() {
        let battery = crate::analog::Field { page: 0x06, usage: 0x20, logical_min: 0, logical_max: 255 };
        let (key, r) = from_field(&battery, 255);
        assert_eq!(key, "battery");
        assert!(matches!(r, Reading::Battery { level, charging: None } if (level - 1.0).abs() < 1e-6));
        let odd = crate::analog::Field { page: 0xFF00, usage: 0x01, logical_min: i32::MIN, logical_max: i32::MAX };
        let (key, r) = from_field(&odd, 0);
        assert_eq!(key, "FF00/01");
        assert!(matches!(r, Reading::Value { value, .. } if (0.0..=1.0).contains(&value)));
    }

    #[test]
    fn readings_are_kept_per_device_and_key_and_forgotten_with_the_device() {
        publish("test-dev-a", "battery", Reading::Battery { level: 0.5, charging: Some(true) });
        publish("test-dev-a", "battery", Reading::Battery { level: 0.6, charging: Some(true) });
        publish("test-dev-b", "battery", Reading::Battery { level: 0.1, charging: None });
        let mine: Vec<Entry> = snapshot().into_iter().filter(|e| e.device.starts_with("test-dev-")).collect();
        assert_eq!(mine.len(), 2, "one reading per (device, key)");
        assert!(mine.iter().any(|e| e.reading == Reading::Battery { level: 0.6, charging: Some(true) }));
        forget("test-dev-a");
        forget("test-dev-b");
        assert!(snapshot().iter().all(|e| !e.device.starts_with("test-dev-")));
        assert_eq!(describe(&Reading::Battery { level: 0.5, charging: Some(true) }), "battery 50% (charging)");
    }
}
