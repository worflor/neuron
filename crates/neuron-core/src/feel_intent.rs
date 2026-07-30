// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Host-side FEEL INTENT — the durable record of what the user last APPLIED to a device
//! (active DPI + the DPI stage cycle), keyed by pid.
//!
//! Why this exists: the wake-reconcile used to treat the DEVICE'S persisted varstore plane as
//! "the user's intent". Live incident 2026-07-23 disproved that: the Naga's unmapped
//! onboard-PROFILE flash (class 0x05) restored the FACTORY stage table into BOTH varstore
//! planes, so the reconcile faithfully re-enforced 400/800/1600/3200/6400 against the user —
//! the device can corrupt its own "persisted truth", so the device can never be the sole
//! authority on what the user wanted. This file is the authority: it is written the moment a
//! feel apply round-trips (verify-gated, so only proven-landed state is recorded) and read by
//! every reassert path (wake, DPI-announce, startup). Devices WITHOUT onboard storage get the
//! same durability for free — the intent lives on the host and is re-asserted volatile.
//!
//! One file, `feel-intent.toml` in the run root, a table per pid (hex). Loads are
//! failure-tolerant (a corrupt file reads as empty — reasserts then simply do nothing until the
//! next apply re-records; never a crash, never a clobber of the user's device from bad data).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The recorded feel intent for one device (pid).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeelIntent {
    /// Active DPI as (x, y), if the user ever applied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<(u16, u16)>,
    /// The DPI stage cycle (X values, symmetric), empty = never applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<u16>,
    /// Active stage index, 0-based, meaningful only when `stages` is non-empty.
    #[serde(default)]
    pub active: u8,
}

impl FeelIntent {
    /// True when nothing has ever been recorded — a reassert must then fall back to
    /// device-derived truth (or do nothing), never invent values.
    pub fn is_empty(&self) -> bool {
        self.dpi.is_none() && self.stages.is_empty()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct FileShape {
    /// pid (lowercase hex, e.g. "00a8") -> intent.
    #[serde(default)]
    devices: BTreeMap<String, FeelIntent>,
}

fn path() -> PathBuf {
    crate::runroot::run_root().join("feel-intent.toml")
}

fn pid_key(pid: u16) -> String {
    format!("{pid:04x}")
}

fn load_file() -> FileShape {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_file(f: &FileShape) -> anyhow::Result<()> {
    let s = toml::to_string_pretty(f)?;
    crate::salvage::atomic_write(&path(), s.as_bytes())?;
    Ok(())
}

/// The recorded intent for `pid`, if any non-empty one exists.
pub fn get(pid: u16) -> Option<FeelIntent> {
    let f = load_file();
    f.devices.get(&pid_key(pid)).filter(|i| !i.is_empty()).cloned()
}

/// Record a proven-landed active-DPI apply. Merges into the pid's record (stage list is kept).
/// Best-effort persistence: a failed disk write is returned but callers treat it as a log-line,
/// never as a failed device apply (the device write already succeeded).
pub fn record_dpi(pid: u16, x: u16, y: u16) -> anyhow::Result<()> {
    let mut f = load_file();
    let e = f.devices.entry(pid_key(pid)).or_default();
    e.dpi = Some((x, y));
    // an applied absolute DPI that IS a stage makes that stage active — keep the cycle coherent
    if let Some(i) = e.stages.iter().position(|&s| s == x) {
        e.active = i as u8;
    }
    save_file(&f)
}

/// Record a proven-landed stage-table apply (the cycle + active index, 0-based).
pub fn record_stages(pid: u16, stages: &[u16], active: u8) -> anyhow::Result<()> {
    let mut f = load_file();
    let e = f.devices.entry(pid_key(pid)).or_default();
    e.stages = stages.to_vec();
    // Clamp in usize and cast LAST. Casting the length first would wrap modulo 256 for a table
    // longer than 256 entries (len 257 -> 0), silently pinning a valid active index to stage 0.
    e.active = usize::from(active).min(stages.len().saturating_sub(1)) as u8;
    // the active stage IS the active DPI once the table lands (firmware behavior) — mirror it
    if let Some(&x) = stages.get(e.active as usize) {
        e.dpi = Some((x, x));
    }
    save_file(&f)
}

/// Forget a device's record (e.g. the user explicitly resets to hardware defaults).
pub fn clear(pid: u16) -> anyhow::Result<()> {
    let mut f = load_file();
    f.devices.remove(&pid_key(pid));
    save_file(&f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intent_roundtrips_through_toml() {
        let mut f = FileShape::default();
        f.devices.insert(
            "00a8".into(),
            FeelIntent { dpi: Some((800, 800)), stages: vec![800, 30000], active: 0 },
        );
        let s = toml::to_string_pretty(&f).unwrap();
        let back: FileShape = toml::from_str(&s).unwrap();
        assert_eq!(back.devices["00a8"].stages, vec![800, 30000]);
        assert_eq!(back.devices["00a8"].dpi, Some((800, 800)));
    }

    #[test]
    fn corrupt_file_reads_as_empty_not_a_crash() {
        let f: FileShape = toml::from_str("devices = 3").unwrap_or_default();
        assert!(f.devices.is_empty());
    }

    #[test]
    fn empty_intent_is_reported_empty() {
        assert!(FeelIntent::default().is_empty());
        assert!(!FeelIntent { dpi: Some((800, 800)), ..Default::default() }.is_empty());
    }
}
