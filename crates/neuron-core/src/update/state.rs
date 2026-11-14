// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The updater's own record, `update/state.json` in the run root: the conditional-request tag,
//! the last release seen, the check schedule, a dismissal, and an install handed off but not yet
//! confirmed by the next launch.

use super::client::LatestRelease;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismissal {
    pub version: String,
    pub at: u64,
}

/// An install handed to Setup or the update script. The next launch compares its own version
/// with `to` to learn whether it took.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub from: String,
    pub to: String,
    pub started: u64,
    /// Where the handoff writes its exit code, when it can.
    #[serde(default)]
    pub outcome: String,
}

/// A freshly installed version that has not yet run long enough to count as healthy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probation {
    pub version: String,
    pub previous: String,
    /// The kept copy of `previous`'s program files.
    pub kept: String,
    /// Launches of `version` that have not exited cleanly or proved themselves.
    pub starts: u32,
}

/// A version that failed probation and was replaced by the one before it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolledBack {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub crash_log: String,
    #[serde(default)]
    pub kept: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub release: Option<LatestRelease>,
    #[serde(default)]
    pub last_check: Option<u64>,
    #[serde(default)]
    pub failures: u32,
    #[serde(default)]
    pub retry_at: Option<u64>,
    #[serde(default)]
    pub dismissed: Option<Dismissal>,
    /// The newest version already announced with a notification.
    #[serde(default)]
    pub announced: Option<String>,
    #[serde(default)]
    pub pending: Option<Pending>,
    /// A version whose bundled notes still wait to be offered after an update landed.
    #[serde(default)]
    pub fresh_install: Option<String>,
    #[serde(default)]
    pub probation: Option<Probation>,
    #[serde(default)]
    pub rolled_back: Option<RolledBack>,
    /// A version that rolled back; never offered again, only newer ones are.
    #[serde(default)]
    pub skip: Option<String>,
}

pub fn dir(run_root: &Path) -> PathBuf {
    run_root.join("update")
}

/// What the running app's updater shows right now, `update/live.json`: written by the app on every
/// change so `neuron update` can read it without asking.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    /// "" · "available" · "downloading" · "ready" · "installing" · "error" · "updated" · "rolled-back"
    pub phase: String,
    /// The incoming (or just-installed) version, without the "v".
    pub version: String,
    /// 0..1 while downloading.
    pub progress: f32,
    pub error: String,
    /// The SYSTEM line: "v0.1.8 available", "v0.1.7 · up to date", or why this copy does not update.
    pub status: String,
    pub checking: bool,
    /// This copy updates itself at all.
    pub enabled: bool,
    /// The state shown is a preview (`neuron update preview`), not a real release.
    pub preview: bool,
    /// When this was written, unix seconds.
    pub at: u64,
}

impl Live {
    pub fn path(run_root: &Path) -> PathBuf {
        dir(run_root).join("live.json")
    }

    pub fn load(run_root: &Path) -> Option<Self> {
        std::fs::read(Self::path(run_root)).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    pub fn save(&self, run_root: &Path) -> std::io::Result<()> {
        let path = Self::path(run_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::salvage::atomic_write(&path, &body)
    }
}

impl State {
    pub fn path(run_root: &Path) -> PathBuf {
        dir(run_root).join("state.json")
    }

    /// Never fails: a missing or unreadable record is a fresh one.
    pub fn load(run_root: &Path) -> Self {
        std::fs::read(Self::path(run_root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self, run_root: &Path) -> std::io::Result<()> {
        let path = Self::path(run_root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        crate::salvage::atomic_write(&path, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_tolerates_garbage() {
        let root = std::env::temp_dir().join(format!("neuron-update-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(State::load(&root), State::default());
        let s = State { etag: Some("\"abc\"".into()), failures: 2, pending: Some(Pending { from: "0.1.7".into(), to: "0.1.8".into(), started: 5, outcome: String::new() }), ..State::default() };
        s.save(&root).unwrap();
        assert_eq!(State::load(&root), s);
        std::fs::write(State::path(&root), b"{not json").unwrap();
        assert_eq!(State::load(&root), State::default());
        assert_eq!(Live::load(&root), None);
        let live = Live { phase: "downloading".into(), version: "0.1.8".into(), progress: 0.5, enabled: true, at: 9, ..Live::default() };
        live.save(&root).unwrap();
        assert_eq!(Live::load(&root), Some(live));
        let _ = std::fs::remove_dir_all(&root);
    }
}
