//! Process-wide clipboard serialization.
//!
//! The Win32 clipboard is a single PROCESS-WIDE resource with no internal per-thread locking: the
//! pointer `GetClipboardData` hands back is only valid until *any* thread calls `CloseClipboard`.
//! Two of our threads touching it at once (a macro's context probe racing a pocket
//! copy/paste/restore, or two probes in parallel) would race — one thread's `CloseClipboard`
//! frees/moves the block another is still scanning through `GlobalLock`, a use-after-free that
//! surfaces as an access violation or heap corruption. This is exactly the shape that once bit us
//! (see the process-wide lock added to fix a real UAF) — so there is now exactly ONE lock, and
//! EVERY `OpenClipboard`...`CloseClipboard` window in the workspace holds it for the full span.
//! (Cross-process contention is a separate, already-handled concern: a foreign holder just makes
//! `OpenClipboard` fail, which callers treat as "busy" and may retry.)
//!
//! `clipboard_guard()` is the ONLY sanctioned way to enter a clipboard critical section. New code
//! that calls `OpenClipboard` without holding this guard reintroduces the original bug and is
//! caught by `neuron-app`'s `every_clipboard_open_is_serialized` convention sweep.

#[cfg(windows)]
static CLIPBOARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the process-wide clipboard lock. A poisoned lock still yields the guard so a prior
/// panic in an unrelated clipboard user never wedges every later clipboard access (the critical
/// sections this guards are unsafe FFI that itself never panics, so there is no corrupt state to
/// protect against).
#[cfg(windows)]
pub fn clipboard_guard() -> std::sync::MutexGuard<'static, ()> {
    CLIPBOARD_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
