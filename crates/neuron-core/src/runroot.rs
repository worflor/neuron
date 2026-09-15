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

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Every config entry (file or directory) the run root owns — the ONE list, so the migration in
/// [`adopt_legacy_run_root`] can never drift out of sync with what the path helpers actually
/// write. Ordered roughly by how much a user would miss it.
///
/// Log files are deliberately ABSENT: they are per-run diagnostics, not user data, and copying a
/// stale crash log into a fresh home would misreport the new home's history.
pub const CONFIG_ENTRIES: &[&str] = &[
    "profiles",
    "macros",
    "gestures.json",
    "pockets",
    "strokes",
    "scripts",
    "devices",
    "options",
    "runtime",
    "backups",
    "app.toml",
    "apps.toml",
    "bindings.toml",
    "board.toml",
    "cast.toml",
    "feel.toml",
    "feel-intent.toml",
    "glance.toml",
    "twin.knbk",
];

/// Entries whose presence at a destination means a REAL config universe already lives there, so a
/// migration must stand down rather than merge two universes.
///
/// This is [`CONFIG_ENTRIES`] MINUS `runtime/`, and that exclusion is load-bearing. `runtime/` is
/// shared: the run root keeps `twin.knbk` and `pockets/` under it, but the bundled-CPython
/// extraction ([`crate::macros::pyruntime`]) independently materializes the interpreter into
/// `data_local_dir()/neuron/runtime` — which is the SAME directory the run root now resolves to.
/// So a machine that had ever warmed the Python sidecar already had a `runtime/` there, with no
/// config in it at all. Treating that as "already live" aborted the migration silently and started
/// the app on default settings while the user's real config sat untouched in the build tree.
///
/// The rule this encodes: a liveness marker must be something ONLY the config system creates.
const LIVE_MARKERS: &[&str] = &[
    "profiles",
    "macros",
    "gestures.json",
    "pockets",
    "strokes",
    "scripts",
    "devices",
    "options",
    "backups",
    "app.toml",
    "apps.toml",
    "bindings.toml",
    "board.toml",
    "cast.toml",
    "feel.toml",
    "feel-intent.toml",
    "glance.toml",
    "twin.knbk",
];

/// The directory every runtime-config path is resolved against. User-supplied CLI path arguments
/// (imports/exports) deliberately do NOT go through here — those stay relative to the user's CWD.
///
/// Resolution order:
///   1. `NEURON_RUN_DIR` when set (read per call, so a test can retarget it).
///   2. The exe's own directory, when that is a legitimate portable home — see
///      [`portable_home_ok`]. This is the shipped-install case and keeps the tray app and the CLI
///      (which sit side by side) in ONE config universe.
///   3. The per-user data dir (`%LOCALAPPDATA%\neuron`), when the exe dir is not a home we may
///      keep data in — a `Program Files` install we cannot write to, or a cargo build tree.
///
/// The CWD is never an anchor. It is the user's directory, not ours: the tray app can be
/// autostarted from System32 and the CLI runs from whatever shell the user stands in, so
/// resolving against it splits the two binaries into different (often empty) config universes —
/// the exact bug this module exists to prevent. Even the last-resort arm prefers the per-user
/// dir, which both binaries at least agree on.
pub fn run_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("NEURON_RUN_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(resolve_root).clone()
}

/// The exe's directory, or `None` if the platform won't tell us.
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()?
        .parent()
        .map(std::path::Path::to_path_buf)
}

fn resolve_root() -> PathBuf {
    let Some(dir) = exe_dir() else {
        return user_data_root().unwrap_or_else(|| PathBuf::from("."));
    };
    if portable_home_ok(&dir) {
        return dir;
    }
    // No per-user dir either (a stripped environment with no LOCALAPPDATA): the exe dir is the
    // honest last resort — possibly unwritable, but at least it is the SAME answer in both
    // binaries, which the CWD would not be.
    user_data_root().unwrap_or(dir)
}

/// Where a build-tree binary sits, when the exe dir is inside a cargo `target/` directory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BuildTreeRole {
    /// `target/release`, `target/debug`, `target/<triple>/release`, … — a real run of the app
    /// straight out of a build tree. Config must NOT live here: it is one `cargo clean` from
    /// gone, and `target/` is gitignored, so nothing else would notice the loss.
    Deployed,
    /// `target/*/deps`, `target/*/examples`, … — a test harness or example binary. These are
    /// throwaway executables whose config must stay SANDBOXED next to them; sending them to the
    /// real per-user config dir would let `cargo test` scribble on the user's actual profiles.
    Sandbox,
}

/// Is `dir` a directory we may keep user config in?
fn portable_home_ok(dir: &Path) -> bool {
    match build_tree_role(dir) {
        Some(BuildTreeRole::Deployed) => false,
        Some(BuildTreeRole::Sandbox) => true,
        // A normal install. Portable only if we can actually write here — a `Program Files`
        // install cannot, and silently failing every save is worse than relocating.
        None => is_writable(dir),
    }
}

/// Classify `dir` against the cargo build layout. `None` when it is not inside a cargo `target/`
/// directory at all.
///
/// A `target` component only counts when its parent holds a `Cargo.toml`, which is the shape
/// cargo builds into — so a user directory that merely happens to be named "target" is not
/// matched and keeps its portable home.
fn build_tree_role(dir: &Path) -> Option<BuildTreeRole> {
    let in_build_tree = dir.ancestors().any(|a| {
        a.file_name().is_some_and(|n| n == "target")
            && a.parent()
                .is_some_and(|p| p.join("Cargo.toml").is_file())
    });
    if !in_build_tree {
        return None;
    }
    // Cargo puts throwaway binaries in a named subdirectory of the profile dir; the profile dir
    // itself holds the real ones. Keyed on the leaf name so cross-compiled layouts
    // (`target/<triple>/release`) classify the same as native ones.
    let leaf_is_scratch = dir
        .file_name()
        .is_some_and(|n| matches!(n.to_str(), Some("deps" | "examples" | "build" | "incremental")));
    Some(if leaf_is_scratch {
        BuildTreeRole::Sandbox
    } else {
        BuildTreeRole::Deployed
    })
}

/// Can we create files in `dir`? Probed for real rather than inferred from metadata: on Windows a
/// directory's read-only bit says nothing about the ACL that actually decides, and `Program Files`
/// additionally virtualizes writes for some processes.
fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".neuron-write-probe-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(f) => {
            drop(f);
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `%LOCALAPPDATA%\neuron` (and the platform equivalents), created if absent.
fn user_data_root() -> Option<PathBuf> {
    let root = dirs::data_local_dir()?.join("neuron");
    std::fs::create_dir_all(&root).ok()?;
    Some(root)
}

/// ONE-TIME MIGRATION off a build-tree run root.
///
/// Before the run root learned to reject cargo build directories, a self-built install kept every
/// config entry in `target/release` — where a routine `cargo clean` silently destroys profiles,
/// macros, gestures, and the `backups/` directory that was supposed to be the safety net. This
/// carries that config forward to the resolved run root the first time the new build starts.
///
/// COPIES rather than moves, deliberately: the old tree stays intact as a de-facto backup, and
/// `cargo clean` reclaims it later at no cost. Runs only when the destination has NO config of its
/// own, so it can never overwrite live data, and is a no-op once it has run (the destination now
/// has config). Call once at startup, BEFORE anything reads config.
///
/// Returns `Some((from, to))` when entries were actually carried over.
pub fn adopt_legacy_run_root() -> Option<(PathBuf, PathBuf)> {
    if std::env::var_os("NEURON_RUN_DIR").is_some_and(|v| !v.is_empty()) {
        return None; // an explicit anchor is the user's choice — never second-guess it
    }
    let root = run_root();
    let exe = exe_dir()?;
    if exe == root {
        return None; // still a portable home — nothing moved
    }
    if !matches!(build_tree_role(&exe), Some(BuildTreeRole::Deployed)) {
        return None; // only the build-tree case is a known-lossy home
    }
    if LIVE_MARKERS.iter().any(|e| root.join(e).exists()) {
        return None; // the destination is already live — never clobber it
    }
    let legacy = richest_legacy_home(&exe)?;
    let mut carried = 0usize;
    for entry in CONFIG_ENTRIES {
        let src = legacy.join(entry);
        if src.exists() && copy_entry(&src, &root.join(entry)).is_ok() {
            carried += 1;
        }
    }
    (carried > 0).then_some((legacy, root))
}

/// Which build-tree directory holds the config universe worth carrying forward.
///
/// A repo has TWO of them — `target/release` (the daily-driver install) and `target/debug` (a
/// `cargo run`) — and either binary can be the first to start after an upgrade. Migrating from
/// whichever one happened to run first would be a coin flip that can strand the real config: the
/// destination is only migrated ONCE, so a debug tree getting there first permanently shadows the
/// release tree's profiles.
///
/// So: score every candidate by how much config it actually holds and take the richest, with the
/// running exe's own directory winning ties (it is the one the user just launched).
fn richest_legacy_home(exe: &Path) -> Option<PathBuf> {
    let score = |d: &Path| CONFIG_ENTRIES.iter().filter(|e| d.join(e).exists()).count();
    let mut best = (score(exe), exe.to_path_buf());
    if let Some(target) = exe.parent() {
        for sibling in ["release", "debug"] {
            let dir = target.join(sibling);
            if dir == exe || !dir.is_dir() {
                continue;
            }
            let s = score(&dir);
            if s > best.0 {
                best = (s, dir);
            }
        }
    }
    (best.0 > 0).then_some(best.1)
}

/// Recursive copy of one config entry (file or directory tree), NEVER overwriting.
///
/// The never-overwrite rule matters for the one shared entry, `runtime/`: the destination may
/// already hold the bundled-CPython extraction (see [`LIVE_MARKERS`]), and merging the legacy
/// tree's `runtime/` into it must add the config-owned children (`twin.knbk`, `pockets/`) without
/// stomping an interpreter that is already correct — or anything else that got there first.
fn copy_entry(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_entry(&e.path(), &dst.join(e.file_name()))?;
        }
        Ok(())
    } else {
        if dst.exists() {
            return Ok(()); // something is already there; the migration never clobbers
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
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
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
            "a TEST binary lives in target/*/deps — a sandbox role, so its run root stays beside \
             it and `cargo test` can never reach the user's real config dir"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
    }

    /// A scratch cargo tree: `<root>/Cargo.toml` + `<root>/target/...`, so `build_tree_role`'s
    /// manifest check has something real to find.
    fn cargo_tree(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "neuron_runroot_tree_{}_{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        root
    }

    /// The classification that decides whether config may live beside the exe. The profile dir is
    /// a DEPLOYED run (config must relocate — `cargo clean` would eat it); its `deps`/`examples`
    /// subdirs are throwaway test binaries (config stays sandboxed beside them).
    #[test]
    fn build_tree_role_separates_deployed_runs_from_test_sandboxes() {
        let root = cargo_tree("role");
        let target = root.join("target");

        for profile in ["release", "debug"] {
            assert_eq!(
                build_tree_role(&target.join(profile)),
                Some(BuildTreeRole::Deployed),
                "target/{profile} is a real run of the app — config must not stay there"
            );
        }
        // Cross-compiled layouts classify by the LEAF name, so they read the same as native ones.
        assert_eq!(
            build_tree_role(&target.join("x86_64-pc-windows-msvc").join("release")),
            Some(BuildTreeRole::Deployed),
            "a cross-compiled profile dir is still a deployed run"
        );
        for scratch in ["deps", "examples", "build", "incremental"] {
            assert_eq!(
                build_tree_role(&target.join("debug").join(scratch)),
                Some(BuildTreeRole::Sandbox),
                "target/debug/{scratch} holds throwaway binaries — they keep a local sandbox"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A directory merely NAMED `target` is not a cargo build tree: with no `Cargo.toml` above it
    /// there is nothing to `cargo clean`, so it keeps its portable home.
    #[test]
    fn a_target_named_dir_without_a_manifest_is_not_a_build_tree() {
        let root = std::env::temp_dir().join(format!("neuron_runroot_named_{}", std::process::id()));
        let dir = root.join("target").join("release");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(
            build_tree_role(&dir),
            None,
            "no manifest above `target` → not cargo's build output"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unwritable directory is not a portable home — a `Program Files` install must relocate
    /// rather than silently fail every save.
    #[test]
    fn portable_home_needs_a_writable_dir() {
        let dir = std::env::temp_dir().join(format!("neuron_runroot_w_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(portable_home_ok(&dir), "a plain writable dir is a valid home");
        assert!(
            !portable_home_ok(&dir.join("does-not-exist")),
            "a directory we cannot create files in is never a home"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The migration carries a nested config tree across, and is a NO-OP once the destination has
    /// config of its own — it must never overwrite live data.
    #[test]
    fn legacy_adoption_copies_once_and_never_clobbers() {
        let legacy = cargo_tree("adopt").join("target").join("release");
        std::fs::create_dir_all(legacy.join("profiles")).unwrap();
        std::fs::write(legacy.join("app.toml"), "old = true\n").unwrap();
        std::fs::write(legacy.join("profiles").join("gui.rules.toml"), "rules = []\n").unwrap();

        let dest = std::env::temp_dir().join(format!("neuron_runroot_dest_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::create_dir_all(&dest).unwrap();

        // The pure half of the migration, driven directly so the test needs no exe relocation.
        let carried = CONFIG_ENTRIES
            .iter()
            .filter(|e| legacy.join(e).exists())
            .filter(|e| copy_entry(&legacy.join(e), &dest.join(e)).is_ok())
            .count();
        assert_eq!(carried, 2, "app.toml + the profiles/ tree");
        assert_eq!(
            std::fs::read_to_string(dest.join("profiles").join("gui.rules.toml")).unwrap(),
            "rules = []\n",
            "a nested config file must survive the copy"
        );

        // The guard the real entry point applies: a destination that already has config is live.
        assert!(
            CONFIG_ENTRIES.iter().any(|e| dest.join(e).exists()),
            "the populated destination must now read as live, so adoption stands down"
        );

        let _ = std::fs::remove_dir_all(&dest);
    }

    /// THE BUG THIS SHIPPED WITH, ONCE. `runtime/` is shared between the run root (which keeps
    /// `twin.knbk` and `pockets/` there) and the bundled-CPython extraction, which materializes
    /// the interpreter into `data_local_dir()/neuron/runtime` — the SAME directory the run root
    /// resolves to on a source build. Any machine that had ever warmed the Python sidecar already
    /// had a `runtime/` at the destination, containing no config whatsoever. With `runtime` acting
    /// as a liveness marker, that aborted the migration silently and the app came up on DEFAULT
    /// settings while the real config sat untouched in the build tree.
    #[test]
    fn a_pre_existing_python_runtime_does_not_read_as_live_config() {
        let dest = std::env::temp_dir().join(format!("neuron_runroot_pyrt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        // Exactly what a warmed sidecar leaves behind: runtime/, and nothing else.
        std::fs::create_dir_all(dest.join("runtime").join("py-3.12.13-x86_64-pc-windows-msvc"))
            .unwrap();

        assert!(
            !LIVE_MARKERS.iter().any(|e| dest.join(e).exists()),
            "an extracted interpreter is not a config universe — migration must still run"
        );
        assert!(
            CONFIG_ENTRIES.contains(&"runtime"),
            "runtime still MIGRATES (it holds twin.knbk + pockets); it just cannot mark liveness"
        );
        assert!(
            !LIVE_MARKERS.contains(&"runtime"),
            "runtime must never be a liveness marker — it has a second, unrelated owner"
        );

        // And once real config IS there, the guard does trip.
        std::fs::write(dest.join("app.toml"), "").unwrap();
        assert!(LIVE_MARKERS.iter().any(|e| dest.join(e).exists()));

        let _ = std::fs::remove_dir_all(&dest);
    }

    /// The migration must never stomp what is already at the destination — the shared `runtime/`
    /// makes this reachable in practice, since a correct interpreter can already be sitting there.
    #[test]
    fn copying_never_overwrites_an_existing_file() {
        let base = std::env::temp_dir().join(format!("neuron_runroot_noclob_{}", std::process::id()));
        let (src, dst) = (base.join("src"), base.join("dst"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(src.join("f.toml"), "from the old tree").unwrap();
        std::fs::write(dst.join("f.toml"), "already here").unwrap();

        copy_entry(&src, &dst).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("f.toml")).unwrap(),
            "already here",
            "the destination's own file wins; a migration adds, it does not replace"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A repo holds both `target/release` and `target/debug`, and either binary can start first
    /// after an upgrade. The migration runs ONCE, so picking the wrong tree strands the real
    /// config behind a destination that now reads as live — pick the one with the most config.
    #[test]
    fn legacy_source_is_the_richest_tree_not_whichever_ran_first() {
        let target = cargo_tree("richest").join("target");
        let release = target.join("release");
        let debug = target.join("debug");
        std::fs::create_dir_all(release.join("profiles")).unwrap();
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::write(release.join("app.toml"), "").unwrap();
        std::fs::write(release.join("cast.toml"), "").unwrap();
        std::fs::write(debug.join("app.toml"), "").unwrap();

        // A debug-tree binary starting first must still carry the RELEASE tree's config forward.
        assert_eq!(
            richest_legacy_home(&debug).unwrap(),
            release,
            "the richer sibling wins, whichever binary happened to start"
        );
        // And the running exe's own tree wins when it is at least as rich.
        assert_eq!(
            richest_legacy_home(&release).unwrap(),
            release,
            "ties go to the directory the user actually launched from"
        );

        // Nothing to carry is not a migration.
        let empty = target.join("x86_64-unknown-none").join("release");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(
            richest_legacy_home(&empty).is_none(),
            "a build tree with no config at all yields no source"
        );

        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }
}
