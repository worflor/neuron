//! Gaming-mode key suppression — a host `WH_KEYBOARD_LL` low-level keyboard hook that swallows
//! Alt+Tab / the Windows key / Alt+F4 while a gaming-mode profile is active. This is Synapse's
//! "Gaming Mode" done host-side (no firmware write): determinism + reversibility (the policy lifts
//! the instant the profile changes or the hook uninstalls).
//!
//! Split into two halves so the *decision* is pure and unit-testable while the *install* is the
//! thin Win32 shell:
//! * [`decide`] — pure: given the current [`GamingMode`] policy, the key event ([`KeyEvent`]) and
//!   the live modifier state ([`Mods`]), return whether to **swallow** the key. No Win32, no global
//!   state — fully tested below.
//! * [`install`] / [`Hook`] — the live Win32 hook: sets the global policy, installs a
//!   `WH_KEYBOARD_LL` hook whose callback calls [`decide`] and returns `1` (swallow) or chains to
//!   the next hook. Both the CLI daemon and the GUI runtime drive this identical API.
//!
//! # Live-path only (never in tests)
//! Installing a global low-level keyboard hook affects the whole desktop, so — exactly like the
//! input-arm gate — it must only run on a real live run. [`install`] is the only thing that touches
//! Win32; **no test calls it**. The decision logic ([`decide`]) is pure and is what the tests
//! exercise. The hook is also a no-op installer unless the policy actually suppresses something
//! (`GamingMode::any()`), so an all-false profile installs nothing.

use crate::writes::{Chord, GamingMode};

/// A keyboard event as the low-level hook sees it: the virtual-key and whether it is a key-*down*
/// (vs key-up). Modeled plainly so [`decide`] needs no Win32 types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// The Windows virtual-key code (`KBDLLHOOKSTRUCT.vkCode`).
    pub vk: u32,
    /// True on press (`WM_KEYDOWN`/`WM_SYSKEYDOWN`), false on release.
    pub down: bool,
}

/// Live modifier state the decision needs (Alt held, for Alt+Tab / Alt+F4 detection). The hook
/// tracks this itself from the event stream (it sees every Alt up/down) rather than calling
/// `GetAsyncKeyState`, so the decision is a pure function of its inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    /// Either Alt (menu) key is currently down.
    pub alt: bool,
}

// Virtual-keys the policy recognises.
const VK_TAB: u32 = 0x09;
const VK_F4: u32 = 0x73;
const VK_ESCAPE: u32 = 0x1B;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_LMENU: u32 = 0xA4; // left Alt
const VK_RMENU: u32 = 0xA5; // right Alt
const VK_MENU: u32 = 0x12; // generic Alt (some injectors report this)

/// Is this vk one of the Alt keys?
fn is_alt(vk: u32) -> bool {
    vk == VK_LMENU || vk == VK_RMENU || vk == VK_MENU
}

/// Is this vk one of the Windows keys?
fn is_win(vk: u32) -> bool {
    vk == VK_LWIN || vk == VK_RWIN
}

/// Update the running modifier state from one event — the hook calls this BEFORE [`decide`] so the
/// Alt key's own up/down is tracked. Returns the new state. Kept separate (and pure) so the tests
/// can drive a sequence of events deterministically.
pub fn track(mut mods: Mods, ev: KeyEvent) -> Mods {
    if is_alt(ev.vk) {
        mods.alt = ev.down;
    }
    mods
}

/// The pure policy decision: should this key event be **swallowed** (suppressed) given the active
/// gaming-mode `policy` and the current `mods` state?
///
/// Rules (only on the key-*down* edge — we never swallow a release, which would strand a modifier):
/// * **Win** key down -> swallow if `policy` disables Win.
/// * **Tab** down while **Alt** held -> Alt+Tab -> swallow if `policy` disables Alt+Tab.
/// * **F4** down while **Alt** held -> Alt+F4 -> swallow if `policy` disables Alt+F4.
///
/// Everything else passes through. This is exactly what the hook callback returns `1` (eat) on.
pub fn decide(policy: &GamingMode, ev: KeyEvent, mods: Mods) -> bool {
    if !ev.down {
        return false; // never swallow a key-up (avoid stuck modifiers)
    }
    if is_win(ev.vk) {
        return policy.suppresses(Chord::Win);
    }
    if mods.alt && ev.vk == VK_TAB {
        return policy.suppresses(Chord::AltTab);
    }
    if mods.alt && ev.vk == VK_F4 {
        return policy.suppresses(Chord::AltF4);
    }
    if mods.alt && ev.vk == VK_ESCAPE {
        return policy.suppresses(Chord::AltEsc);
    }
    false
}

// ─────────────────────────────── live Win32 hook (install/uninstall) ───────────────────────────
//
// Only this section touches Win32. It is `#[cfg(windows)]`, and `install` is the sole entry that
// registers a global hook — kept out of every test path. A `Hook` handle uninstalls on drop, so a
// caller (CLI daemon / GUI runtime) just holds it for the lifetime of the gaming-mode session.

#[cfg(windows)]
mod sys {
    use super::{decide, track, GamingMode, KeyEvent, Mods};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
        WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    // Global state the C-callback reads (a hook proc has a fixed signature — no user pointer).
    // Only one gaming-mode hook is meaningful at a time, so a process-global is the right model.
    static POLICY: Mutex<GamingMode> = Mutex::new(GamingMode {
        disable_alt_tab: false,
        disable_win: false,
        disable_alt_f4: false,
        disable_alt_esc: false,
    });
    static MODS: Mutex<Mods> = Mutex::new(Mods { alt: false });
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    // The installed hook handle, stored so `uninstall` (and Drop) can remove it.
    static HANDLE: Mutex<isize> = Mutex::new(0);

    /// The `WH_KEYBOARD_LL` callback. Tracks Alt state, asks [`decide`], and returns `1` to swallow
    /// or chains to the next hook. Must be `extern "system"` with the exact LL-hook signature.
    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // code < 0 (HC_ACTION is 0): we must not process, just chain.
        if code >= 0 {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            let down = wparam as u32 == WM_KEYDOWN || wparam as u32 == WM_SYSKEYDOWN;
            let ev = KeyEvent {
                vk: kb.vkCode,
                down,
            };
            // Update tracked modifier state first, then decide.
            let new_mods = {
                let mut m = MODS.lock().unwrap();
                *m = track(*m, ev);
                *m
            };
            let policy = *POLICY.lock().unwrap();
            if decide(&policy, ev, new_mods) {
                return 1; // swallow: do NOT call the next hook -> the chord never reaches the OS.
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// Install (or update the policy of) the gaming-mode hook. If `policy` suppresses nothing, this
    /// uninstalls any existing hook and installs none (zero cost when not gaming). Idempotent: a
    /// second call just updates the live policy. Returns `true` if a hook is now active.
    pub fn install(policy: GamingMode) -> bool {
        *POLICY.lock().unwrap() = policy;
        if !policy.any() {
            uninstall();
            return false;
        }
        if INSTALLED.load(Ordering::Relaxed) {
            return true; // already hooked; policy updated above.
        }
        // SAFETY: standard LL keyboard-hook install with a valid extern "system" proc.
        let h: HHOOK =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), std::ptr::null_mut(), 0) };
        if h.is_null() {
            return false;
        }
        *HANDLE.lock().unwrap() = h as isize;
        INSTALLED.store(true, Ordering::Relaxed);
        true
    }

    /// Remove the hook if installed (idempotent). The desktop returns to normal Alt+Tab/Win/Alt+F4
    /// behaviour immediately — the reversibility guarantee.
    pub fn uninstall() {
        if !INSTALLED.swap(false, Ordering::Relaxed) {
            return;
        }
        let h = *HANDLE.lock().unwrap();
        if h != 0 {
            // SAFETY: `h` is a handle we installed and have not yet removed.
            unsafe { UnhookWindowsHookEx(h as HHOOK) };
            *HANDLE.lock().unwrap() = 0;
        }
        *MODS.lock().unwrap() = Mods::default();
    }

    /// True if a gaming-mode hook is currently installed.
    pub fn is_installed() -> bool {
        INSTALLED.load(Ordering::Relaxed)
    }
}

/// RAII handle for an installed gaming-mode hook. Drop uninstalls — so a caller can scope the hook
/// to a gaming-mode session and have it lift automatically. Construct via [`install`].
///
/// NOTE: a `WH_KEYBOARD_LL` hook only receives events while the installing thread runs a message
/// loop (`GetMessage`/`PeekMessage`). The CLI daemon and the GUI already pump messages on their
/// listener thread, so the hook lives on that thread. If you install from a thread with no message
/// pump, the callback will not fire — install it on the same thread as the Raw-Input listener.
#[derive(Debug)]
pub struct Hook {
    active: bool,
}

impl Hook {
    /// True if this handle installed a live hook (false when the policy suppressed nothing).
    pub fn active(&self) -> bool {
        self.active
    }
    /// Uninstall now (also happens on drop). Idempotent.
    pub fn uninstall(&mut self) {
        #[cfg(windows)]
        sys::uninstall();
        self.active = false;
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        #[cfg(windows)]
        sys::uninstall();
    }
}

/// Install (or update) the gaming-mode keyboard hook for `policy`. Usable identically by the CLI
/// daemon and the GUI runtime: call it on the thread that pumps the Raw-Input message loop when a
/// gaming-mode profile becomes active, and drop (or [`Hook::uninstall`]) the returned handle when it
/// deactivates. If `policy` suppresses nothing, no hook is installed (`Hook::active()` is `false`).
///
/// **Live-path only.** This touches the global desktop; never call it from a test. (The pure
/// [`decide`] policy is what tests verify.)
#[cfg(windows)]
pub fn install(policy: GamingMode) -> Hook {
    let active = sys::install(policy);
    Hook { active }
}

/// Non-Windows stub: no global hook concept; returns an inert handle.
#[cfg(not(windows))]
pub fn install(_policy: GamingMode) -> Hook {
    Hook { active: false }
}

/// True if a gaming-mode hook is currently installed (Windows; always `false` elsewhere).
#[cfg(windows)]
pub fn is_installed() -> bool {
    sys::is_installed()
}
#[cfg(not(windows))]
pub fn is_installed() -> bool {
    false
}

// ─────────────────────────── shared policy carrier (ONE source, no drift) ───────────────────────
//
// Both the CLI daemon and the GUI live runtime drive the gaming-mode hook off the SAME desired
// policy. Rather than each client keeping its own copy (which drifted before), the desired policy
// lives here as a process-global cell. A profile apply pushes the new policy via [`set_policy`];
// the live listener thread periodically calls [`reconcile`] to make the installed hook match. This
// is the single source of truth — the CLI and GUI both go through these two functions.

use std::sync::Mutex as StdMutex;

/// The desired gaming-mode policy the live thread should enforce. Set by whichever client applies a
/// profile (CLI `profile apply` path / GUI on_apply_profile); read by [`reconcile`] on the listener
/// thread. Defaults to "suppress nothing", so until a gaming-mode profile is applied nothing hooks.
static DESIRED_POLICY: StdMutex<GamingMode> = StdMutex::new(GamingMode {
    disable_alt_tab: false,
    disable_win: false,
    disable_alt_f4: false,
    disable_alt_esc: false,
});

/// Push the desired gaming-mode policy (e.g. the active profile's `ApplyReport::gaming_mode`). The
/// next [`reconcile`] on the listener thread installs / updates / drops the hook to match. Safe to
/// call from any thread (e.g. the UI thread on profile apply); does NOT itself touch Win32.
pub fn set_policy(policy: GamingMode) {
    *DESIRED_POLICY.lock().unwrap() = policy;
}

/// The currently desired policy (the one [`reconcile`] will enforce). Mostly for diagnostics/tests.
pub fn policy() -> GamingMode {
    *DESIRED_POLICY.lock().unwrap()
}

/// Make the installed hook match [`policy`]. Call this on the listener thread (the only thread that
/// pumps the Raw-Input message loop the LL hook needs). Idempotent and cheap:
/// * policy suppresses something + not yet installed -> install (sets `*slot`).
/// * policy suppresses something + already installed  -> update the live policy in place (no
///   uninstall/reinstall thrash — `install` only re-registers when not already hooked).
/// * policy suppresses nothing + installed            -> drop the handle (uninstall).
///
/// `slot` is the caller-owned RAII handle (held for the session so Drop uninstalls on shutdown).
#[cfg(windows)]
pub fn reconcile(slot: &mut Option<Hook>) {
    let desired = *DESIRED_POLICY.lock().unwrap();
    if desired.any() {
        // Already hooked: update the live policy WITHOUT dropping the existing handle (dropping
        // first would briefly uninstall — the thrash bug). `sys::install` updates in place.
        let active = sys::install(desired);
        if slot.is_none() {
            // Adopt a handle so Drop still uninstalls on shutdown; the hook is already live.
            *slot = Some(Hook { active });
        }
    } else if slot.is_some() {
        *slot = None; // drop -> uninstall (reversibility: desktop returns to normal immediately)
    }
}

/// Non-Windows: no hook concept; keep the desired policy carrier coherent but install nothing.
#[cfg(not(windows))]
pub fn reconcile(slot: &mut Option<Hook>) {
    let desired = *DESIRED_POLICY.lock().unwrap();
    if desired.any() {
        if slot.is_none() {
            *slot = Some(install(desired));
        }
    } else {
        *slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::writes::GamingMode;

    fn down(vk: u32) -> KeyEvent {
        KeyEvent { vk, down: true }
    }
    fn up(vk: u32) -> KeyEvent {
        KeyEvent { vk, down: false }
    }

    #[test]
    fn alt_tab_swallowed_only_when_alt_held_and_policy_on() {
        let policy = GamingMode::from_profile(true, false, false, false); // disable Alt+Tab only
                                                                   // Tab alone (no Alt) is never an app-switch -> pass through.
        assert!(!decide(&policy, down(VK_TAB), Mods { alt: false }));
        // Alt held + Tab down -> swallow.
        assert!(decide(&policy, down(VK_TAB), Mods { alt: true }));
        // With the policy off, Alt+Tab passes through.
        let off = GamingMode::default();
        assert!(!decide(&off, down(VK_TAB), Mods { alt: true }));
    }

    #[test]
    fn win_key_swallowed_per_policy() {
        let policy = GamingMode::from_profile(false, true, false, false); // disable Win only
        assert!(decide(&policy, down(VK_LWIN), Mods::default()));
        assert!(decide(&policy, down(VK_RWIN), Mods::default()));
        // Win-up is never swallowed (no stuck key).
        assert!(!decide(&policy, up(VK_LWIN), Mods::default()));
        // Off policy lets the Win key through.
        assert!(!decide(
            &GamingMode::default(),
            down(VK_LWIN),
            Mods::default()
        ));
    }

    #[test]
    fn alt_f4_swallowed_only_when_alt_held_and_policy_on() {
        let policy = GamingMode::from_profile(false, false, true, false); // disable Alt+F4 only
        assert!(!decide(&policy, down(VK_F4), Mods { alt: false })); // bare F4 passes
        assert!(decide(&policy, down(VK_F4), Mods { alt: true })); // Alt+F4 swallowed
                                                                   // Alt+Tab is NOT swallowed by an Alt+F4-only policy.
        assert!(!decide(&policy, down(VK_TAB), Mods { alt: true }));
    }

    #[test]
    fn alt_esc_swallowed_only_when_alt_held_and_policy_on() {
        let policy = GamingMode::from_profile(false, false, false, true); // disable Alt+Esc only
        assert!(!decide(&policy, down(VK_ESCAPE), Mods { alt: false })); // bare Esc passes
        assert!(decide(&policy, down(VK_ESCAPE), Mods { alt: true })); // Alt+Esc swallowed
                                                                       // chord-specific: an Alt+Esc-only policy leaves Alt+Tab alone.
        assert!(!decide(&policy, down(VK_TAB), Mods { alt: true }));
        // policy off -> Alt+Esc passes through.
        assert!(!decide(&GamingMode::default(), down(VK_ESCAPE), Mods { alt: true }));
    }

    #[test]
    fn key_up_is_never_swallowed() {
        let all = GamingMode::from_profile(true, true, true, false);
        assert!(!decide(&all, up(VK_TAB), Mods { alt: true }));
        assert!(!decide(&all, up(VK_F4), Mods { alt: true }));
        assert!(!decide(&all, up(VK_LWIN), Mods::default()));
    }

    #[test]
    fn ordinary_keys_pass_through_even_in_gaming_mode() {
        let all = GamingMode::from_profile(true, true, true, false);
        // 'A' (0x41), with and without Alt, is never suppressed.
        assert!(!decide(&all, down(0x41), Mods { alt: false }));
        assert!(!decide(&all, down(0x41), Mods { alt: true }));
        // Tab WITHOUT Alt is not Alt+Tab.
        assert!(!decide(&all, down(VK_TAB), Mods { alt: false }));
    }

    #[test]
    fn track_follows_alt_edges() {
        let mut m = Mods::default();
        assert!(!m.alt);
        m = track(m, down(VK_LMENU));
        assert!(m.alt, "left Alt down sets the flag");
        m = track(m, down(VK_TAB)); // unrelated key doesn't change Alt
        assert!(m.alt);
        m = track(m, up(VK_LMENU));
        assert!(!m.alt, "Alt up clears the flag");
        // generic VK_MENU and right Alt also work
        m = track(m, down(VK_RMENU));
        assert!(m.alt);
        m = track(m, up(VK_RMENU));
        assert!(!m.alt);
    }

    #[test]
    fn end_to_end_alt_tab_via_tracked_state() {
        // Simulate the event stream the hook sees: Alt down, then Tab down.
        let policy = GamingMode::from_profile(true, false, false, false);
        let mut m = Mods::default();
        let e_alt = down(VK_LMENU);
        m = track(m, e_alt);
        assert!(
            !decide(&policy, e_alt, m),
            "the Alt key itself is not swallowed"
        );
        let e_tab = down(VK_TAB);
        m = track(m, e_tab);
        assert!(decide(&policy, e_tab, m), "Tab after Alt -> swallowed");
    }

    // The shared desired-policy carrier the CLI and GUI both drive. Pure (no hook install): exercises
    // only the cell that `reconcile` reads — never touches Win32, never installs a global hook.
    #[test]
    fn desired_policy_carrier_roundtrips() {
        // Restore whatever was set so this test stays order-independent for the process-global cell.
        let saved = super::policy();
        super::set_policy(GamingMode::from_profile(true, false, true, false));
        let p = super::policy();
        assert!(p.disable_alt_tab && !p.disable_win && p.disable_alt_f4);
        assert!(p.any());
        super::set_policy(GamingMode::default());
        assert!(
            !super::policy().any(),
            "an all-false policy suppresses nothing"
        );
        super::set_policy(saved);
    }
}
