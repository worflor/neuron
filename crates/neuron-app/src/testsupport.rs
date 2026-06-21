//! Shared test-only isolation helpers.
//!
//! The process current-directory is a single global resource: `std::env::set_current_dir` mutates
//! it for the WHOLE process, not the calling thread. Several tests in this crate exercise
//! run-directory-relative file IO (the GUI rules sidecar in `editor`, `app.toml` in `prefs`, and the
//! prefs round-trip driven through the live `State` callback in `apptest`). If any two of those run
//! concurrently they corrupt each other's cwd and the relative-path reads land in the wrong dir.
//!
//! The fix is ONE process-wide lock that EVERY cwd-mutating test acquires before touching the cwd.
//! Three independent per-module mutexes don't serialize against each other — they must share this
//! single lock. `cwd_guard()` is the only sanctioned way to change the cwd in a test: it takes the
//! global lock, swaps into a unique temp dir, and restores + cleans up on `Drop`.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// The single process-wide cwd lock. ALL cwd-mutating tests across the crate share this so they run
/// serially relative to one another regardless of which module they live in.
static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Holds the global cwd lock for the lifetime of the test body, points the process at a private
/// temp dir, and restores the original cwd (and removes the temp dir) on `Drop`.
pub struct CwdGuard {
    _lock: MutexGuard<'static, ()>,
    prev: PathBuf,
    tmp: PathBuf,
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.prev);
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

/// Acquire the global cwd lock and enter a unique throwaway directory. The `tag` only flavors the
/// temp-dir name for debuggability; uniqueness comes from pid + a monotonic counter so two guards
/// (even with the same tag, even on the same thread) never collide.
pub fn cwd_guard(tag: &str) -> CwdGuard {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    // Recover from a poisoned lock: a panicking test must not wedge every later cwd test.
    let lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::current_dir().unwrap();
    let tmp = std::env::temp_dir().join(format!(
        "neuron_{}_{}_{}",
        tag,
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(&tmp);
    std::env::set_current_dir(&tmp).unwrap();
    CwdGuard {
        _lock: lock,
        prev,
        tmp,
    }
}
