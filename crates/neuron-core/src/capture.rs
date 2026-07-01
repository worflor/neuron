//! Press-to-bind capture — the core primitive behind the project's strongest UX rule:
//! **the user PRESSES the control they want; we never hardcode a button.** (See the memory note:
//! "DONT HARD CODE IT TO FUCKING THUMB 2".)
//!
//! Historically this logic lived in the CLI (`main.rs::capture_keypress` / `vk_name`, used by the
//! `sniper` bind flow). The GUI needs the exact same primitive for press-to-bind on every panel
//! (sniper, gesture/radial triggers, button remaps), so it is lifted here into the headless core
//! where both the CLI and the Slint app can call it.
//!
//! Two capture surfaces:
//! * [`capture_keypress`] — wait for any newly-pressed virtual-key (keyboard key OR mouse button,
//!   since `GetAsyncKeyState` covers `VK_LBUTTON`/`VK_RBUTTON`/`VK_MBUTTON`/`VK_XBUTTON1`/2). This is
//!   the baseline-then-detect loop: snapshot what is already held, then return the first key that
//!   transitions *down* afterwards. **ESC always cancels** (returns `None`).
//! * [`capture_mouse_button`] — same loop but restricted to the mouse VK set, for "bind a mouse
//!   button" UI that should ignore keyboard keys.
//!
//! Plus friendly-name helpers so the UI shows "Mouse 5 (thumb 2)" / "'1'" / "Phone Mute" — never a
//! raw hex code:
//! * [`vk_name`] — name a captured virtual-key.
//! * [`usage_name`] — name a HID `(page, usage)` device control (re-exported from [`controls`] so
//!   callers have one import for both capture surfaces).
//!
//! # Safety / threading
//! Capture only *reads* input state (`GetAsyncKeyState`) — it never synthesises input, so it is
//! **not** gated by `action::input_armed()` and is always safe to run (even in `--safe` mode). It
//! is a blocking poll loop; a GUI must run it on a worker thread (it polls ~8 ms and returns as soon
//! as a key is pressed or ESC cancels), or use [`capture_keypress_until`] to abort it from the UI
//! thread.

pub use crate::controls::usage_name;

/// Mouse virtual-keys (`GetAsyncKeyState` codes): L/R/Middle + the two X-buttons (thumb 1/2).
/// Used by [`capture_mouse_button`] to restrict capture to the mouse and by [`is_mouse_vk`].
pub const MOUSE_VKS: [i32; 5] = [
    0x01, // VK_LBUTTON
    0x02, // VK_RBUTTON
    0x04, // VK_MBUTTON
    0x05, // VK_XBUTTON1 (thumb 1 / "Mouse 4")
    0x06, // VK_XBUTTON2 (thumb 2 / "Mouse 5")
];

/// Windows virtual-key for ESC, the universal "cancel capture" key.
pub const VK_ESCAPE: i32 = 0x1B;

/// True if `vk` is one of the mouse buttons (vs a keyboard key).
pub fn is_mouse_vk(vk: i32) -> bool {
    MOUSE_VKS.contains(&vk)
}

/// Friendly, human-facing name for a captured virtual-key. Mouse buttons read as the gamer-familiar
/// "Mouse 4 (thumb 1)" / "Mouse 5 (thumb 2)"; digits and letters quote their character; anything
/// else falls back to a transparent `VK 0x..` (we never hide the real value — the design motto).
///
/// This is the name a press-to-bind UI shows the moment the user releases the control.
pub fn vk_name(vk: i32) -> String {
    match vk {
        0x01 => "Left Mouse".into(),
        0x02 => "Right Mouse".into(),
        0x03 => "Cancel".into(),
        0x04 => "Middle Mouse".into(),
        0x05 => "Mouse 4 (thumb 1)".into(),
        0x06 => "Mouse 5 (thumb 2)".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x10 => "Shift".into(),
        0x11 => "Ctrl".into(),
        0x12 => "Alt".into(),
        0x14 => "Caps Lock".into(),
        VK_ESCAPE => "Esc".into(),
        0x20 => "Space".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        v if (0x30..=0x39).contains(&v) => format!("'{}'", (v as u8) as char), // '0'..'9'
        v if (0x41..=0x5A).contains(&v) => format!("'{}'", (v as u8) as char), // 'A'..'Z'
        v if (0x70..=0x7B).contains(&v) => format!("F{}", v - 0x6F),           // F1..F12
        v => format!("VK 0x{v:02X}"),
    }
}

use std::cell::Cell;

thread_local! {
    /// While set on a thread, [`key_down`] reports every key as UP *without* the per-key
    /// `GetAsyncKeyState` syscall. Why: the lighting page's TILE GRID re-renders ~16 effect
    /// thumbnails every ~90ms, and three of them (reactive/ripple/comet) scan all 256 virtual-keys
    /// per frame — ~765 `GetAsyncKeyState` syscalls per tick (measured ~180µs *each effect*) purely
    /// to drive live keyboard reactivity in postage-stamp previews that don't need it. The SELECTED
    /// effect's big preview and the device stream still scan live on their own paths; only the
    /// thumbnail pass suppresses it. Thread-local, so worker-thread capture loops are unaffected.
    static SUPPRESS_KEY_READS: Cell<bool> = const { Cell::new(false) };
}

/// RAII guard returned by [`suppress_key_reads`]: live key reads on THIS thread report UP until it
/// drops (panic-safe). Scope it around a hot render that does not need real key state.
pub struct KeyReadGuard(());
impl Drop for KeyReadGuard {
    fn drop(&mut self) {
        SUPPRESS_KEY_READS.with(|s| s.set(false));
    }
}

/// Suppress live keyboard reads on the current thread for the lifetime of the returned guard — so a
/// hot, reactivity-irrelevant render (the lighting tile grid) skips the per-key `GetAsyncKeyState`
/// syscalls entirely. See [`SUPPRESS_KEY_READS`].
pub fn suppress_key_reads() -> KeyReadGuard {
    SUPPRESS_KEY_READS.with(|s| s.set(true));
    KeyReadGuard(())
}

/// Read the current pressed state of one virtual-key. Reads only (never injects), so it is safe
/// regardless of the input-arm gate. Returns `false` immediately (no syscall) while a
/// [`suppress_key_reads`] guard is active on this thread.
#[cfg(windows)]
pub fn key_down(vk: i32) -> bool {
    if SUPPRESS_KEY_READS.with(|s| s.get()) {
        return false;
    }
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: GetAsyncKeyState is a pure read of the async key state for a valid VK in 0..256.
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}
#[cfg(not(windows))]
pub fn key_down(_vk: i32) -> bool {
    if SUPPRESS_KEY_READS.with(|s| s.get()) {
        return false;
    }
    false
}

/// Held-state bitmask for the Razer macro keys — bit `i` = the i-th macro key (M(i+1)) currently held.
/// The macro keys arrive on Razer's Driver-Mode `0x04` HID report (decoded by `neuron-app::macrokeys`),
/// NOT as Windows virtual-keys, so `GetAsyncKeyState`/[`key_down`] never sees them. This mask is the
/// macro-key analogue of the OS async key state: SHARED, STATELESS held-state. Each lighting consumer
/// (the device animate loop AND the GUI hero preview run the same effect at once) keeps its OWN `prev[]`
/// and detects its own down-edges off this shared state, so neither drains the other — exactly how the
/// VK path already works (a consume-once queue would let one consumer steal the press from the other).
static MACRO_HELD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Store the live macro-key held mask — called by the macro-key reader on EVERY `0x04` report,
/// including an all-released report (mask `0`), so RELEASES propagate and the next press re-detects.
pub fn set_macro_held(mask: u8) {
    MACRO_HELD.store(mask, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the `i`-th macro key (M(i+1)) is currently held — the macro-key analogue of [`key_down`].
/// Returns `false` (no read) while a [`suppress_key_reads`] guard is active on this thread, the SAME
/// suppression `key_down` honours, so the lighting tile-grid thumbnails skip the macro scan too. `i ≥ 8`
/// is always `false` (the mask is 8 bits). Pure read of shared state — safe regardless of the input gate.
pub fn macro_key_down(i: usize) -> bool {
    if SUPPRESS_KEY_READS.with(|s| s.get()) {
        return false;
    }
    i < 8 && (MACRO_HELD.load(std::sync::atomic::Ordering::Relaxed) & (1 << i)) != 0
}

/// Snapshot which of all 256 virtual-keys are currently down — the *baseline* a capture starts
/// from, so a key already held when capture begins is ignored (we only detect a fresh press).
#[cfg(windows)]
fn baseline() -> [bool; 256] {
    let mut b = [false; 256];
    for (vk, slot) in b.iter_mut().enumerate() {
        *slot = key_down(vk as i32);
    }
    b
}

/// Wait for the user to press ANY key or mouse button and return its virtual-key. Keys already held
/// when capture starts are ignored (baseline-then-detect). **ESC cancels** (returns `None`).
///
/// This is the press-to-bind primitive: the UI says "press the control you want", calls this, and
/// shows [`vk_name`] of the result — no hex to type, nothing hardcoded. Blocking; run on a worker
/// thread in a GUI. For a cancellable-from-another-thread version, see [`capture_keypress_until`].
#[cfg(windows)]
pub fn capture_keypress() -> Option<i32> {
    capture_filtered(|_| true)
}
#[cfg(not(windows))]
pub fn capture_keypress() -> Option<i32> {
    None
}

/// Like [`capture_keypress`] but only accepts mouse buttons — for "bind a mouse button" UI that
/// should ignore keyboard keys. ESC still cancels.
#[cfg(windows)]
pub fn capture_mouse_button() -> Option<i32> {
    capture_filtered(is_mouse_vk)
}
#[cfg(not(windows))]
pub fn capture_mouse_button() -> Option<i32> {
    None
}

/// Cancellable [`capture_keypress`]: returns `None` if ESC is pressed OR `stop` is set from another
/// thread (a GUI flips it to abort the capture, e.g. the user closed the bind dialog). Otherwise
/// returns the first newly-pressed virtual-key. The GUI's press-to-bind owns an `Arc<AtomicBool>`
/// and clones it into the capture worker.
#[cfg(windows)]
pub fn capture_keypress_until(stop: &std::sync::atomic::AtomicBool) -> Option<i32> {
    capture_filtered_until(stop, |_| true)
}
#[cfg(not(windows))]
pub fn capture_keypress_until(_stop: &std::sync::atomic::AtomicBool) -> Option<i32> {
    None
}

/// Core capture loop: snapshot the baseline, then poll until a key passing `accept` transitions
/// down (returns its VK), or ESC is pressed (returns `None`). Shared by the public capture fns.
#[cfg(windows)]
fn capture_filtered(accept: impl Fn(i32) -> bool) -> Option<i32> {
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    capture_filtered_until(&NEVER, accept)
}

/// As [`capture_filtered`] but also abortable via `stop`.
#[cfg(windows)]
fn capture_filtered_until(
    stop: &std::sync::atomic::AtomicBool,
    accept: impl Fn(i32) -> bool,
) -> Option<i32> {
    use std::time::Duration;
    let base = baseline();
    loop {
        if key_down(VK_ESCAPE) || stop.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        // VK 0 is unassigned; scan 1..256 and return the first newly-pressed accepted key.
        for vk in 1..256 {
            if vk != VK_ESCAPE && accept(vk) && key_down(vk) && !base[vk as usize] {
                return Some(vk);
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_vks_classified() {
        assert!(is_mouse_vk(0x01) && is_mouse_vk(0x05) && is_mouse_vk(0x06));
        assert!(!is_mouse_vk(0x41)); // 'A' is not a mouse button
        assert!(!is_mouse_vk(VK_ESCAPE));
        assert_eq!(MOUSE_VKS.len(), 5);
    }

    #[test]
    fn vk_names_are_friendly_never_raw_for_common_controls() {
        assert_eq!(vk_name(0x05), "Mouse 4 (thumb 1)");
        assert_eq!(vk_name(0x06), "Mouse 5 (thumb 2)");
        assert_eq!(vk_name(0x01), "Left Mouse");
        assert_eq!(vk_name(0x04), "Middle Mouse");
        assert_eq!(vk_name(0x31), "'1'"); // digit
        assert_eq!(vk_name(0x41), "'A'"); // letter
        assert_eq!(vk_name(0x70), "F1");
        assert_eq!(vk_name(0x7B), "F12");
        assert_eq!(vk_name(VK_ESCAPE), "Esc");
        assert_eq!(vk_name(0x20), "Space");
    }

    #[test]
    fn unknown_vk_falls_back_to_transparent_hex() {
        // We never hide the real value — an unmapped VK shows its code, not "?".
        assert_eq!(vk_name(0xFE), "VK 0xFE");
    }

    #[test]
    fn usage_name_reexport_matches_controls() {
        // The HID-control name helper is the same one the listener uses.
        assert_eq!(usage_name(0x0C, 0xE9), "Volume Up");
        assert_eq!(usage_name(0x0B, 0x2F), "Phone Mute");
        assert_eq!(
            usage_name(crate::controls::MIC_TAP.0, crate::controls::MIC_TAP.1),
            "Mic Tap"
        );
        assert_eq!(usage_name(0x0C, 0x99), "?");
    }

    #[test]
    fn key_down_is_safe_to_call_off_windows() {
        // On non-Windows this is a stub; on Windows it is a pure read. Either way: no panic,
        // and it never synthesises input (so it is gate-independent).
        let _ = key_down(0x41);
    }
}
