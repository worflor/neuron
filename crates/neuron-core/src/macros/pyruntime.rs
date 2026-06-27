//! The BUNDLED Python runtime — app-OWNED, app-MATERIALIZED.
//!
//! `neuron` embeds a private CPython (a `python-build-standalone` tarball, chosen for the build
//! target by `build.rs`) and the two host scripts (`neuron_host.py` + `neuron.py`) directly in the
//! binary. On first use [`ensure_runtime`] unpacks the interpreter into the user's data dir and
//! drops the host scripts beside each other, then hands back the three paths the sidecar spawn
//! needs. The macro sidecar therefore runs from an interpreter the app SHIPS — never the user's
//! system Python, never a PATH probe, never an env-var hack.
//!
//! ## Why materialize to disk (not run from memory)
//! CPython is a real interpreter that wants a real filesystem: its stdlib, `DLLs`/`lib`, and the
//! host scripts must exist as files for `python neuron_host.py` to import them. We extract ONCE
//! (idempotent: a present interpreter binary short-circuits) into a versioned dir, so an app
//! update that bumps the pinned CPython lands in a NEW dir and the old one is simply unused.
//!
//! ## Crash-safety + concurrency
//! Extraction is ATOMIC: we unpack into a sibling temp dir and `rename` it into place, so a build
//! killed mid-extract never leaves a half-runtime that *looks* complete (the check is "does the
//! interpreter binary exist at the final path"). A process-wide lock serializes concurrent callers
//! (the host may spawn sessions from several threads), and the work is memoized so the common
//! warm-path is a single map lookup.
//!
//! ## Host co-location (no PYTHONPATH)
//! `neuron_host.py` and `neuron.py` are written into the SAME `host/` dir, and the sidecar is
//! spawned as `python <…>/host/neuron_host.py` — so `sys.path[0]` is that dir and `import neuron`
//! resolves the sibling. python-build-standalone honours normal `sys.path` (unlike the Windows
//! *embeddable* zip, which needs a `._pth`), so NO PYTHONPATH / `_pth` surgery is required.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The embedded interpreter tarball (gzip'd tar) chosen by build.rs for this build's target.
static PY_TARBALL: &[u8] = include_bytes!(env!("NEURON_PY_TARBALL"));
/// The bundled CPython version (e.g. "3.12.13"), for the versioned runtime dir name.
const PY_VER: &str = env!("NEURON_PY_VER");
/// The PBS triple the embedded interpreter was built for, for the versioned runtime dir name.
const PY_TRIPLE: &str = env!("NEURON_PY_TRIPLE");

/// The host scripts, embedded verbatim from the repo (their CONTENTS are unchanged — only WHERE
/// they come from moved into the binary). Relative to this file
/// (`crates/neuron-core/src/macros/`), the repo's `runtime/host/` is four levels up.
const HOST_PY: &str = include_str!("../../../../runtime/host/neuron_host.py");
const NEURON_PY: &str = include_str!("../../../../runtime/host/neuron.py");

/// The resolved bundled runtime: the interpreter to spawn, the host entry script, and the dir that
/// holds both host scripts (the spawn's `current_dir`, so `import neuron` resolves the sibling).
#[derive(Clone, Debug)]
pub struct Runtime {
    /// The bundled interpreter (Windows `python/python.exe`, Unix `python/bin/python3`).
    pub python: PathBuf,
    /// The host entry script (`host/neuron_host.py`).
    pub host_script: PathBuf,
    /// The dir holding `neuron_host.py` + `neuron.py` (spawn `current_dir`).
    pub host_dir: PathBuf,
}

/// The embedded `neuron.py` host-module source — the macro author's reference (`neuron macro
/// prelude` prints it). Returned straight from the binary, so it's always the version this build
/// ships, with no dependency on a materialized runtime.
pub fn host_module_source() -> &'static str {
    NEURON_PY
}

/// Memoized success + the serialization lock. `OnceLock<Result>` would refuse to retry a transient
/// failure, so we keep a `Mutex<Option<Runtime>>`: once materialized we return the cached paths;
/// a prior failure leaves `None` and the next caller retries.
static RUNTIME: OnceLock<Mutex<Option<Runtime>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<Runtime>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

/// Materialize (idempotently) the bundled CPython + host scripts and return their paths.
///
/// Cheap on the warm path (already-materialized → a clone of the cached struct). On a cold call it
/// extracts the embedded interpreter to `<data>/neuron/runtime/py-<ver>-<triple>/` (atomic
/// rename) if absent and (re)writes the host scripts if missing or stale. Returns a clear
/// `Err(String)` on any IO failure — the GUI surfaces it; nothing here panics.
pub fn ensure_runtime() -> Result<Runtime, String> {
    let lock = slot();
    let mut guard = lock
        .lock()
        .map_err(|_| "python runtime lock poisoned".to_string())?;
    if let Some(rt) = guard.as_ref() {
        return Ok(rt.clone());
    }

    let base = runtime_base()?; // <data>/neuron/runtime
    let py_dir = base.join(format!("py-{PY_VER}-{PY_TRIPLE}"));
    let python = py_dir.join(python_rel());

    // 1. Interpreter: extract once. "Done" == the interpreter binary exists at the final path.
    if !python.exists() {
        extract_interpreter(&py_dir)?;
    }
    if !python.exists() {
        return Err(format!(
            "bundled python missing after extraction: {}",
            python.display()
        ));
    }

    // 2. Host scripts: co-located in host/, written if missing OR changed (so an app update that
    //    ships new host scripts refreshes them — the bytes are embedded, so "changed" is exact).
    let host_dir = base.join("host");
    std::fs::create_dir_all(&host_dir)
        .map_err(|e| format!("create host dir {}: {e}", host_dir.display()))?;
    let host_script = host_dir.join("neuron_host.py");
    write_if_changed(&host_script, HOST_PY.as_bytes())?;
    write_if_changed(&host_dir.join("neuron.py"), NEURON_PY.as_bytes())?;

    let rt = Runtime {
        python,
        host_script,
        host_dir,
    };
    *guard = Some(rt.clone());
    Ok(rt)
}

/// `<data-local>/neuron/runtime`, falling back to a stable temp-dir path if the platform has no
/// data-local dir (headless/CI). Same path each run so the extract stays idempotent.
fn runtime_base() -> Result<PathBuf, String> {
    let root = dirs::data_local_dir().unwrap_or_else(|| std::env::temp_dir().join("neuron-data"));
    Ok(root.join("neuron").join("runtime"))
}

/// The interpreter path INSIDE the extracted `python/` tree, per OS. python-build-standalone puts
/// `python.exe` at the top of `python/` on Windows and `python3` under `python/bin/` on Unix.
fn python_rel() -> PathBuf {
    #[cfg(windows)]
    {
        Path::new("python").join("python.exe")
    }
    #[cfg(not(windows))]
    {
        Path::new("python").join("bin").join("python3")
    }
}

/// Extract the embedded tarball into `py_dir` ATOMICALLY: unpack into a sibling temp dir, then
/// rename it into place. The archive's top-level entry is `python/`, so the extracted tree is
/// `<py_dir>/python/…` — matching [`python_rel`].
fn extract_interpreter(py_dir: &Path) -> Result<(), String> {
    let parent = py_dir
        .parent()
        .ok_or_else(|| format!("runtime dir has no parent: {}", py_dir.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create runtime base {}: {e}", parent.display()))?;

    // A unique sibling temp dir (pid + nanos) so concurrent *processes* don't collide on the temp
    // name; the in-process Mutex already serializes threads.
    let stamp = format!(
        "{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let tmp = py_dir.with_file_name(format!(
        "{}.{stamp}",
        py_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "py".into())
    ));
    // Clean any stale temp from a previously-killed extract.
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .map_err(|e| format!("create extract temp {}: {e}", tmp.display()))?;

    // Unpack the gzip'd tar (pure-Rust → identical on every OS; preserves unix permission bits so
    // `python/bin/python3` stays executable).
    let unpack = || -> std::io::Result<()> {
        let gz = flate2::read::GzDecoder::new(PY_TARBALL);
        let mut ar = tar::Archive::new(gz);
        ar.set_preserve_permissions(true);
        ar.unpack(&tmp)
    };
    if let Err(e) = unpack() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(format!("unpack bundled python: {e}"));
    }

    // Commit. If another caller (another process) won the race and the final dir now exists, our
    // work is redundant — discard the temp and accept theirs (rename onto a non-empty dir fails on
    // most platforms, so treat an existing target as success).
    if py_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(());
    }
    match std::fs::rename(&tmp, py_dir) {
        Ok(()) => Ok(()),
        Err(e) => {
            // A loser of a cross-process race: target appeared between the check and the rename.
            if py_dir.exists() {
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            } else {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(format!("commit python dir {}: {e}", py_dir.display()))
            }
        }
    }
}

/// Write `bytes` to `path` only if the file is absent or its contents differ — so the common warm
/// path does no IO, and an app update with new host-script bytes refreshes them.
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Ok(mut f) = std::fs::File::open(path) {
        let mut cur = Vec::new();
        if f.read_to_end(&mut cur).is_ok() && cur == bytes {
            return Ok(());
        }
    }
    std::fs::write(path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tarball_is_nonempty() {
        // include_bytes! of the build.rs-SLIMMED tarball must be substantial (a real CPython, just
        // with the .pdb/pip/test/__pycache__ weight stripped — ~15-20 MB compressed vs the ~44 MB
        // upstream). Floor guards a corrupt/empty embed; the ceiling proves slimming actually ran
        // (an un-slimmed bundle is ~44 MB).
        let n = PY_TARBALL.len();
        assert!(n > 10_000_000, "embedded python tarball looks too small: {n} bytes");
        assert!(
            n < 35_000_000,
            "embedded python tarball looks un-slimmed ({n} bytes; upstream is ~44 MB)"
        );
    }

    /// Regression guard for the build.rs slim step: the embedded tarball must KEEP everything a
    /// macro could touch (interpreter + encodings + tkinter) and DROP the prune list (.pdb,
    /// ensurepip, the test suite, pip). Reads the embedded bytes directly — no materialized runtime.
    #[test]
    fn slim_tarball_keeps_essentials_drops_prune_list() {
        let gz = flate2::read::GzDecoder::new(PY_TARBALL);
        let mut ar = tar::Archive::new(gz);
        let mut paths = Vec::new();
        for e in ar.entries().expect("slim tarball entries") {
            let e = e.expect("slim tarball entry");
            let p = e
                .path()
                .expect("entry path")
                .to_string_lossy()
                .replace('\\', "/");
            paths.push(p);
        }
        assert!(!paths.is_empty(), "slim tarball has no entries");

        // KEEP (zero feature loss): the interpreter, the codecs, and tkinter must all survive.
        assert!(
            paths
                .iter()
                .any(|p| p.ends_with("python.exe") || p.ends_with("bin/python3")),
            "slim tarball is missing the interpreter (python.exe / bin/python3)"
        );
        assert!(
            paths.iter().any(|p| p.contains("encodings/")),
            "slim tarball is missing Lib/encodings"
        );
        assert!(
            paths.iter().any(|p| p.contains("tkinter")),
            "slim tarball is missing tkinter (a macro could use it)"
        );

        // PRUNE: none of these may remain.
        for p in &paths {
            let lp = p.to_ascii_lowercase();
            assert!(!lp.ends_with(".pdb"), "slim tarball still has debug symbols: {p}");
            assert!(!lp.contains("ensurepip"), "slim tarball still has ensurepip: {p}");
            assert!(!lp.contains("/lib/test/"), "slim tarball still has the test suite: {p}");
            assert!(
                !lp.contains("/site-packages/pip"),
                "slim tarball still has pip: {p}"
            );
        }
    }

    #[test]
    fn python_rel_is_os_correct() {
        let rel = python_rel();
        #[cfg(windows)]
        assert!(rel.ends_with("python.exe"));
        #[cfg(not(windows))]
        assert!(rel.ends_with("python3"));
    }
}
