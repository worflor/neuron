//! Shared test-only isolation helpers.
//!
//! The process current-directory AND the environment are single global resources:
//! `std::env::set_current_dir`/`set_var` mutate them for the WHOLE process, not the calling
//! thread. Several tests in this crate exercise run-root file IO (the GUI rules sidecar in
//! `editor`, `app.toml` in `prefs`, and the prefs round-trip driven through the live `State`
//! callback in `apptest`). Config resolves through `neuron::runroot::run_root()` (NEURON_RUN_DIR,
//! else the exe dir), so isolating a test means pointing NEURON_RUN_DIR at a private temp dir —
//! and the cwd comes along so any test-local relative IO lands in the same place. If any two such
//! tests run concurrently they corrupt each other's view.
//!
//! The fix is ONE process-wide lock that EVERY cwd/run-root-mutating test acquires first. Three
//! independent per-module mutexes don't serialize against each other — they must share this
//! single lock. `cwd_guard()` is the only sanctioned way to retarget cwd or NEURON_RUN_DIR in a
//! test: it takes the global lock, points both at a unique temp dir, and restores + cleans up on
//! `Drop`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// The single process-wide cwd/run-root lock. ALL mutating tests across the crate share this so
/// they run serially relative to one another regardless of which module they live in.
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Holds the global lock for the lifetime of the test body, points the process (cwd AND
/// NEURON_RUN_DIR) at a private temp dir, and restores both (and removes the temp dir) on `Drop`.
pub struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
    prev_run_dir: Option<OsString>,
    tmp: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
        match self.prev_run_dir.take() {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Acquire the global lock and enter a unique throwaway directory (cwd + NEURON_RUN_DIR both point
/// at it, so run-root config IO and test-local relative IO agree). The `tag` only flavors the
/// temp-dir name for debuggability; uniqueness comes from pid + a monotonic counter so two guards
/// (even with the same tag, even on the same thread) never collide.
pub fn cwd_guard(tag: &str) -> CwdGuard {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // Recover from a poisoned lock: a panicking test must not wedge every later cwd test.
    let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::current_dir().unwrap();
    let prev_run_dir = std::env::var_os("NEURON_RUN_DIR");
    let tmp = std::env::temp_dir().join(format!(
        "neuron_{}_{}_{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&tmp);
    std::env::set_current_dir(&tmp).unwrap();
    std::env::set_var("NEURON_RUN_DIR", &tmp);
    CwdGuard {
        _lock: lock,
        prev,
        prev_run_dir,
        tmp,
    }
}

// ── convention regression: no bare poison-prone lock unwraps in production code ────────────────
//
// The workspace's established pattern for recovering from a poisoned std-sync guard is
// `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` (see dispatch.rs, host.rs, and —
// after the 2026-07-09 sweep — every other production call site). A bare `.lock().unwrap()` (or
// `.read()`/`.write()` on an `RwLock`) means one panic while holding the guard poisons it FOREVER,
// and every later locker panics too — a single reader-thread fault can silently brick a whole
// subsystem (mute/battery decode, live paint, etc.) for the rest of the process's life. This test
// is the tripwire: it fails loudly if the pattern regresses, rather than relying on every future
// PR remembering the convention.
//
// Living in `testsupport.rs` (rather than a new file) because this module is already the crate's
// home for shared, cross-cutting test infrastructure that isn't about any one feature.
#[cfg(test)]
mod conventions {
    use std::path::{Path, PathBuf};

    /// The workspace root: two levels up from this crate's manifest (`crates/neuron-app/..`/`..`).
    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
    }

    /// Recursively collect every `.rs` file under `dir`.
    fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// This codebase's tests live in a trailing `#[cfg(test)]\nmod tests { ... }` block that runs to
    /// EOF. Truncate at the FIRST line whose trimmed content is exactly `#[cfg(test)]` — everything
    /// from there on is test code, where a bare `.lock().unwrap()` is fine (a poisoned lock in a test
    /// SHOULD panic loudly). Verified against the real repo layout: every file in the audited census
    /// has exactly one such marker, opening a `mod tests` that runs to the file's last line (including
    /// `macro_host.rs`, which has production code both well before AND for hundreds of lines up to its
    /// single trailing `#[cfg(test)]`) — so a naive truncation at the first marker is exactly right
    /// here. `dispatch.rs` has one too (at its own trailing tests module) and is already clean, so it
    /// must scan to zero — if it doesn't, this truncation heuristic is the thing that's wrong.
    fn strip_test_region(src: &str) -> &str {
        let mut offset = 0;
        for line in src.lines() {
            if line.trim() == "#[cfg(test)]" {
                return &src[..offset];
            }
            offset += line.len() + 1; // the '\n' this `lines()` iteration consumed
        }
        src
    }

    /// `file:line` for every bare `.lock()/.read()/.write()` + `.unwrap()` pair in `path`'s
    /// production region. Whitespace (including newlines, so a call chain wrapped across lines like
    /// `.lock()\n    .unwrap()` is still caught) is collapsed before matching, and `//` line comments
    /// are stripped first so a doc comment that merely mentions the pattern in prose can't
    /// false-positive. Reports the source line the match STARTS on.
    fn bare_poison_unwraps(path: &Path) -> Vec<(usize, &'static str)> {
        let Ok(content) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let production = strip_test_region(&content);

        // A whitespace-stripped haystack, with a parallel byte->line map, so a match can be blamed
        // on the line it started on regardless of how many lines its chain spans.
        let mut haystack = String::with_capacity(production.len());
        let mut line_at: Vec<usize> = Vec::with_capacity(production.len());
        for (i, line) in production.lines().enumerate() {
            let code = match line.find("//") {
                Some(idx) => &line[..idx],
                None => line,
            };
            for ch in code.chars().filter(|c| !c.is_whitespace()) {
                haystack.push(ch);
                line_at.push(i + 1); // 1-indexed, matching editor/compiler convention
            }
        }

        let mut hits = Vec::new();
        for needle in [".lock().unwrap()", ".read().unwrap()", ".write().unwrap()"] {
            let mut start = 0;
            while let Some(pos) = haystack[start..].find(needle) {
                let abs = start + pos;
                hits.push((line_at.get(abs).copied().unwrap_or(0), needle));
                start = abs + 1; // overlap-safe; duplicates collapse below
            }
        }
        hits.sort_unstable();
        hits.dedup();
        hits
    }

    #[test]
    fn no_bare_poison_unwraps_in_production_code() {
        let root = workspace_root();
        let crates_dir = root.join("crates");
        let mut files = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&crates_dir) {
            for entry in entries.flatten() {
                let src = entry.path().join("src");
                if src.is_dir() {
                    collect_rs_files(&src, &mut files);
                }
            }
        }
        assert!(
            files.len() > 50,
            "the workspace crate sweep under {} found only {} .rs file(s) — path resolution is \
             broken (expected the whole workspace, hundreds of files)",
            crates_dir.display(),
            files.len(),
        );

        let mut offenders = Vec::new();
        for file in &files {
            for (line, pat) in bare_poison_unwraps(file) {
                offenders.push(format!("{}:{line} ({pat})", file.display()));
            }
        }
        assert!(
            offenders.is_empty(),
            "use .lock().unwrap_or_else(std::sync::PoisonError::into_inner) — see docs/TDD.md \
             poison discipline\n{}",
            offenders.join("\n")
        );
    }
}

// ── persistence audit (TDD §8: "no GUI save path writes outside the executable/run directory") ──
//
// Two layers:
//   1. DERIVATION — every config path helper this crate + neuron-core expose resolves UNDER
//      `neuron::runroot::run_root()` (the shared anchor of the two binaries; NEURON_RUN_DIR when
//      set, else the exe's directory). Pins each helper's contract so a future change that swaps
//      in a different anchor (the CWD again, `dirs::config_dir()` / `%APPDATA%`) fails loudly
//      here instead of shipping the app/CLI config split all over again.
//   2. BEHAVIORAL SAMPLE — under `cwd_guard` (which pins NEURON_RUN_DIR to a private temp dir),
//      run one real save through two independent save paths (prefs + the editor's GUI-rules
//      sidecar) and confirm the files landed under the pinned run root.
//
// NOT COVERED here, on purpose (not invented):
//   * the bundled Python interpreter's on-disk extraction dir
//     (`neuron::macros::pyruntime`'s `runtime_base()`, under `dirs::data_local_dir()`) IS a
//     genuine absolute, outside-run-root path — but it materializes a shipped INTERPRETER BINARY,
//     never a GUI save, so it's outside this audit's scope by the brief's own wording ("no GUI
//     SAVE path"). Noted, not asserted against.
#[cfg(test)]
mod persistence_audit {
    use super::cwd_guard;
    use std::path::Path;

    /// A config path helper's result must live under the run root — the shared anchor both
    /// binaries resolve config against.
    fn assert_under_run_root(label: &str, p: &Path) {
        let root = neuron::runroot::run_root();
        assert!(
            p.starts_with(&root),
            "{label} resolved to {} which is OUTSIDE the run root {}",
            p.display(),
            root.display()
        );
    }

    /// DERIVATION layer: every path-deriving fn found across neuron-app + neuron-core (prefs,
    /// bindings, cast, feel, profile/apps/profiles-dir, gestures, macro scripts, auto device
    /// defs, backups via the run root, strokes, the crash log).
    #[test]
    fn every_known_config_path_helper_resolves_under_the_run_root() {
        // Pin the run root so the assertion is against a known anchor (and the exe-dir fallback
        // can't race another test's NEURON_RUN_DIR override — the guard holds the global lock).
        let _g = cwd_guard("persistence_audit_derivation");

        assert_under_run_root("prefs::Prefs::path (app.toml)", &crate::prefs::Prefs::path());
        assert_under_run_root(
            "editor::gui_rules_path (profiles/gui.rules.toml)",
            &crate::editor::gui_rules_path(),
        );
        assert_under_run_root(
            "flight::crash_log_path (neuron-crash.log)",
            &crate::flight::crash_log_path(),
        );
        assert_under_run_root(
            "strokelab::strokes_dir (strokes/)",
            &crate::strokelab::strokes_dir(),
        );
        assert_under_run_root(
            "neuron::bindings::Bindings::path (bindings.toml)",
            &neuron::bindings::Bindings::path(),
        );
        assert_under_run_root(
            "neuron::cast::CastConfig::path (cast.toml)",
            &neuron::cast::CastConfig::path(),
        );
        assert_under_run_root(
            "neuron::feel::FeelConfig::path (feel.toml)",
            &neuron::feel::FeelConfig::path(),
        );
        assert_under_run_root(
            "neuron::profile::AppRules::path (apps.toml)",
            &neuron::profile::AppRules::path(),
        );
        assert_under_run_root(
            "neuron::profile::profiles_dir (profiles/)",
            &neuron::profile::profiles_dir(),
        );
        assert_under_run_root(
            "neuron::profile::Profile::path (profiles/<name>.toml)",
            &neuron::profile::Profile::path("audit-probe"),
        );
        assert_under_run_root(
            "neuron::gesture::Vault::path (gestures.json)",
            &neuron::gesture::Vault::path(),
        );
        assert_under_run_root(
            "neuron::macros::macro_host::macros_dir (macros/scripts)",
            &neuron::macros::macro_host::macros_dir(),
        );
        assert_under_run_root(
            "neuron::synth::auto_def_path (devices/auto/<dialect>-<pid>.toml)",
            &neuron::synth::auto_def_path("razer", 0x00ab),
        );
    }

    /// BEHAVIORAL SAMPLE: one real save on two independent paths (prefs, and the editor's GUI
    /// rules sidecar), under a pinned temp run root — the files must appear THERE.
    #[test]
    fn a_real_save_lands_under_the_pinned_run_root() {
        let _g = cwd_guard("persistence_audit");

        let status = crate::prefs::set_start_minimized(true);
        assert!(
            !status.to_lowercase().contains("failed"),
            "prefs save reported failure: {status}"
        );
        assert!(
            crate::prefs::Prefs::path().exists(),
            "app.toml did not appear under the pinned run root after set_start_minimized"
        );

        crate::editor::save_gui_rules(&[])
            .expect("gui rules save must succeed under a writable temp run root");
        assert!(
            crate::editor::gui_rules_path().exists(),
            "profiles/gui.rules.toml did not appear under the pinned run root after save_gui_rules"
        );
    }
}
