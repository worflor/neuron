// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The RUN ROOT — the one directory Neuron's runtime config lives in.
//!
//! Two binaries (neuron-app, neuron-cli) share one run folder and one config universe
//! (bindings.toml, cast.toml, gestures.json, profiles/, devices/auto/, …). The process CWD is
//! the USER'S directory, not ours: the tray app can be autostarted from System32, and the CLI
//! runs from whatever shell the user stands in — resolving config against the CWD split the two
//! binaries into different (often stale or empty) config universes. The stable shared anchor is
//! the directory containing the current executable, since both exes ship side by side.
//!
//! `NEURON_RUN_DIR` overrides the anchor explicitly (tests isolate into a temp dir; power users
//! can pin a config home), consistent with the app's other `NEURON_*` env switches.

use std::path::PathBuf;
use std::sync::OnceLock;

/// The directory every runtime-config path is resolved against. `NEURON_RUN_DIR` if set
/// (read per call, so a test can retarget it), else the current exe's directory (computed
/// once), else `"."` as a last resort. User-supplied CLI path arguments (imports/exports)
/// deliberately do NOT go through here — those stay relative to the user's CWD.
pub fn run_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("NEURON_RUN_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    static EXE_DIR: OnceLock<PathBuf> = OnceLock::new();
    EXE_DIR
        .get_or_init(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        })
        .clone()
}

/// RAII pin for `NEURON_RUN_DIR`: sets it to `dir`, restores the PREVIOUS value (or absence)
/// on drop — so a caller-provided run root survives any test that isolates into a temp dir,
/// and early exits/panics restore it too. Env is process-global: unit tests inside this crate
/// must hold `ENV_LOCK` around the pin; single-scenario integration binaries are naturally
/// serialized.
pub struct RunDirPin {
    prev: Option<std::ffi::OsString>,
}

impl RunDirPin {
    pub fn to(dir: &std::path::Path) -> Self {
        let prev = std::env::var_os("NEURON_RUN_DIR");
        std::env::set_var("NEURON_RUN_DIR", dir);
        RunDirPin { prev }
    }
}

impl Drop for RunDirPin {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
    }
}

/// Env vars are process-global: every test in this crate that mutates `NEURON_RUN_DIR` must hold
/// this lock so parallel tests can't observe each other's override.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// The run-root contract: with `NEURON_RUN_DIR` set, every path resolves under it; without,
    /// under the directory containing the current executable.
    #[test]
    fn run_root_honors_env_override_and_falls_back_to_exe_dir() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");

        let tmp = std::env::temp_dir().join(format!("neuron_runroot_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);
        assert_eq!(run_root(), tmp, "NEURON_RUN_DIR must win when set");
        assert!(
            crate::cast::CastConfig::path().starts_with(&tmp),
            "a config path helper must resolve under the override"
        );

        std::env::remove_var("NEURON_RUN_DIR");
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
        assert_eq!(
            run_root(),
            exe_dir,
            "without the override, the run root is the exe's directory"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
    }
}
