// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Device-scoped keyboard remap — the user-mode "input shim".
//!
//! Some devices (the Razer Naga thumb grid) are hardware keyboards with FIXED key usages; they
//! can't be remapped in firmware (proven live — see the `naga-side-plate-binding` notes). Razer
//! itself remaps them host-side with a kernel HID filter (`RzDev_*.sys`). This module is the SAME
//! mechanism in user mode, no driver: swallow the device's keystroke and inject a replacement,
//! device-scoped so the SAME physical key on a real keyboard is untouched.
//!
//! ## Why "swallow-and-replay" (measured, not assumed)
//! The two signals we have are the low-level keyboard hook (which can SWALLOW a keystroke but is
//! device-BLIND) and Raw-Input (which knows the device PID but is observe-only). A live spike
//! (`hookorder`) measured the ordering across 60+ keystrokes: **the LL hook fires ~0.3–2ms BEFORE
//! Raw-Input, always.** So at hook time the device is unknown. The robust design is therefore:
//!   1. **Hook**: swallow EVERY keystroke whose scancode has a device remap (device unknown yet),
//!      buffer it as `pending` ([`on_hook`]).
//!   2. **Raw-Input** (~0.5ms later, carries the PID): resolve the pending entry — if it came from
//!      the remapped device, inject the target key; if from any OTHER device, replay the original
//!      ([`on_rawinput`]).
//!   3. **Fail-open**: a pending entry that never gets a Raw-Input match within a short window is
//!      replayed as-is ([`expire`]) — a keystroke is never lost.
//!
//! This module is the PURE decision core (no Win32) so it is fully unit-tested; the live hook +
//! Raw-Input wiring drives it one event at a time. Injected replays are filtered OUT by the live
//! layer (via a `dwExtraInfo` signature) before they ever reach [`on_hook`], so the shim never
//! re-processes its own output.

use std::collections::{HashMap, VecDeque};

/// What a matched keystroke should turn into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOut {
    /// Emit this physical scancode instead (a plain key remap).
    Scancode(u16),
    /// Emit NOTHING — the key is claimed by a host feature (the cast/weave trigger) and its
    /// keystroke must not reach the desktop at all. The same key on any OTHER device still
    /// replays unchanged, exactly like a remap.
    Swallow,
}

/// One device-scoped remap: pressing physical `from` scancode on device `pid` emits `to`. The same
/// scancode on any other device is replayed unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Remap {
    pub pid: u16,
    pub from: u16,
    pub to: KeyOut,
}

/// A keystroke to synthesize (the live layer turns this into a `SendInput`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inject {
    pub scancode: u16,
    /// True on the make (press) edge, false on the break (release).
    pub down: bool,
}

/// One swallowed keystroke awaiting device attribution from Raw-Input.
#[derive(Clone, Copy, Debug)]
struct Pending {
    scancode: u16,
    down: bool,
    at_us: u64,
}

/// The pure correlation core. Not `Send`-shared directly — the live layer wraps it in a mutex and
/// drives `on_hook` (hook thread) + `on_rawinput`/`expire` (reader thread).
#[derive(Default)]
pub struct Interceptor {
    /// scancode -> remaps that involve it (O(1) lookup for the hot hook path).
    by_scancode: HashMap<u16, Vec<Remap>>,
    /// swallowed-but-not-yet-attributed keystrokes, oldest first.
    pending: VecDeque<Pending>,
}

impl Interceptor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the remap table (called when the engine's bindings change). Clears pending too —
    /// a config swap must not resolve a stale keystroke against a new rule.
    pub fn set_remaps(&mut self, remaps: impl IntoIterator<Item = Remap>) {
        self.by_scancode.clear();
        self.pending.clear();
        for r in remaps {
            self.by_scancode.entry(r.from).or_default().push(r);
        }
    }

    /// True if ANY device remap involves this scancode — the hook swallows it (device unknown yet).
    pub fn is_remapped_scancode(&self, scancode: u16) -> bool {
        self.by_scancode.contains_key(&scancode)
    }

    /// HOOK edge: record a swallowed keystroke as pending. Returns whether the hook should swallow
    /// (true iff the scancode is remapped for some device). Only call for NON-injected events.
    pub fn on_hook(&mut self, scancode: u16, down: bool, now_us: u64) -> bool {
        if !self.by_scancode.contains_key(&scancode) {
            return false;
        }
        self.pending.push_back(Pending {
            scancode,
            down,
            at_us: now_us,
        });
        true
    }

    /// RAW-INPUT edge (device known): resolve the matching pending keystroke. Returns what to
    /// inject — the target key if `pid` matches a remap for this scancode, else the original
    /// (another device sent it). `None` if there's no pending match (e.g. an injected replay's own
    /// Raw-Input echo, or a scancode we don't remap).
    pub fn on_rawinput(&mut self, scancode: u16, down: bool, pid: u16) -> Option<Inject> {
        let pos = self
            .pending
            .iter()
            .position(|p| p.scancode == scancode && p.down == down)?;
        self.pending.remove(pos);
        let remaps = self.by_scancode.get(&scancode)?;
        match remaps.iter().find(|r| r.pid == pid) {
            Some(r) => match r.to {
                KeyOut::Scancode(sc) => Some(Inject { scancode: sc, down }),
                // Claimed by a host feature: the swallow IS the resolution — inject nothing.
                KeyOut::Swallow => None,
            },
            // Some OTHER device sent this scancode — replay it unchanged so the real key still works.
            None => Some(Inject { scancode, down }),
        }
    }

    /// FAIL-OPEN sweep: any pending keystroke with no Raw-Input attribution within `window_us` is
    /// replayed as-is (a keystroke is never swallowed forever). Call periodically (the dispatch
    /// tick). Returns the replays to inject, oldest first.
    pub fn expire(&mut self, now_us: u64, window_us: u64) -> Vec<Inject> {
        let mut out = Vec::new();
        while let Some(front) = self.pending.front() {
            if now_us.saturating_sub(front.at_us) < window_us {
                break;
            }
            let p = self.pending.pop_front().expect("front just peeked");
            out.push(Inject {
                scancode: p.scancode,
                down: p.down,
            });
        }
        out
    }

    /// Pending count — for diagnostics/tests.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// True if no remaps are configured (the whole shim is a no-op).
    pub fn is_empty(&self) -> bool {
        self.by_scancode.is_empty()
    }

    /// Is there a remap for this (physkey, device pid)? Lets the live dispatcher SKIP its own
    /// host-side handling of a trigger the shim already owns (avoids the double-send).
    pub fn has_remap(&self, physkey: u16, pid: u16) -> bool {
        self.by_scancode
            .get(&physkey)
            .is_some_and(|v| v.iter().any(|r| r.pid == pid))
    }

    /// GRAB-MODEL resolve (e.g. Linux evdev `EVIOCGRAB`): the device is known at read time, so
    /// there is no separate swallow/hook and no `pending` to match — look the remap up directly.
    /// Returns the target key if this (physkey, pid) is remapped, else the original (pass-through).
    /// Unlike [`on_rawinput`], this never touches `pending` (the Windows correlation path). The
    /// Windows/macOS backends use `on_hook`+`on_rawinput`; a grab backend uses this instead.
    /// `None` = the key is claimed with [`KeyOut::Swallow`] — emit nothing.
    pub fn resolve_direct(&self, physkey: u16, down: bool, pid: u16) -> Option<Inject> {
        match self
            .by_scancode
            .get(&physkey)
            .and_then(|v| v.iter().find(|r| r.pid == pid))
        {
            Some(r) => match r.to {
                KeyOut::Scancode(sc) => Some(Inject { scancode: sc, down }),
                KeyOut::Swallow => None,
            },
            None => Some(Inject {
                scancode: physkey,
                down,
            }),
        }
    }
}

// ─────────────────────────────── LIVE LAYER (process-global + Win32) ───────────────────────────
//
// The pure [`Interceptor`] above is driven by two threads: the LL keyboard hook (writes `pending`
// via [`hook_edge`]) and the Raw-Input reader in `controls::listen` (resolves via [`on_raw_keyboard`]
// and injects). They share ONE process-global behind a mutex — the same single-owner model as
// `hook.rs`'s gaming hook. Everything is a zero-cost no-op until [`configure`] arms it.
//
// KEY WIN32 FACTS this relies on (measured / documented):
// * A WH_KEYBOARD_LL hook returning 1 suppresses the LEGACY message path (WM_KEYDOWN/WM_CHAR) but
//   NOT Raw-Input (WM_INPUT) — so we still SEE the swallowed key (to attribute + resolve it), and
//   press-to-bind capture (which reads Raw-Input) keeps working.
// * Our own `SendInput` replays carry a `dwExtraInfo` signature so the hook passes them through
//   instead of re-swallowing (no loop); their Raw-Input echo has no `pending` match so the reader
//   ignores it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Signature stamped into `dwExtraInfo` on every keystroke WE inject — the hook recognizes its own
/// replays and passes them through. "nrn\0" — arbitrary but distinctive.
const INJECT_SIG: usize = 0x006e_726e;
/// Fail-open window: a swallowed keystroke with no Raw-Input attribution within this long is
/// replayed as-is. The measured hook→Raw-Input gap is ~0.3–2ms, so 8ms is a comfortable ceiling.
const EXPIRE_US: u64 = 8_000;

static CORE: Mutex<Option<Interceptor>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Temporarily suspend the shim WITHOUT tearing down the hook — set while a press-to-bind capture
/// is in flight, so pressing a control to BIND it isn't swallowed/remapped (the user is binding it,
/// not using it). Mirrors how the live dispatcher skips firing during capture.
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Suspend/resume the shim for the duration of a press-to-bind capture. While paused, keys pass
/// through untouched (no swallow, no inject) so a captured control is read cleanly. Cheap; call
/// `true` when a capture starts and `false` when it ends.
pub fn set_paused(on: bool) {
    PAUSED.store(on, Ordering::SeqCst);
}

/// Is the shim currently paused for a press-to-bind capture? Read-only witness for the capture
/// teardown paths' tests (a leaked pause = keyboard remaps and the cast-trigger swallow silently
/// dead process-wide until the next capture) and for diagnostics.
pub fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// Is the shim standing down for this edge — either not armed at all, or [`set_paused`] for a
/// press-to-bind capture? The three per-edge entry points (`hook_edge`, `on_raw_keyboard`, `owns`)
/// all route through this so "paused" cannot come to mean different things in different places.
///
/// [`expire_tick`] deliberately does NOT consult it: a keystroke swallowed in the instant BEFORE the
/// pause must still fail open and replay, or pausing would eat the key it was meant to protect.
fn standing_down() -> bool {
    !ACTIVE.load(Ordering::Relaxed) || PAUSED.load(Ordering::Relaxed)
}

fn epoch() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}
fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Arm the shim with the active device remaps (from the engine's pid-scoped keyboard bindings).
/// Installs the hook on first non-empty config; a later empty config tears it down. Idempotent.
pub fn configure(remaps: Vec<Remap>) {
    let any = !remaps.is_empty();
    {
        let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.get_or_insert_with(Interceptor::new).set_remaps(remaps);
    }
    if any {
        ACTIVE.store(true, Ordering::SeqCst);
        install();
    } else {
        deactivate();
    }
}

/// Build a device-side [`Remap`] from an engine rule, IF it's one the shim can own: a base-layer,
/// pid-scoped, keyboard-page `Input` trigger whose action is a plain `Key`. The live loop feeds
/// these to [`configure`] and EXCLUDES them from the engine (so the engine never additively
/// dispatches them — that would re-introduce the double-send). `None` for anything else. Windows
/// only (the usage↔scancode map is a Win32 concern).
/// Build a device-side [`Remap`] from an engine rule, IF it's one the shim can own: a base-layer,
/// pid-scoped, keyboard-page `Input` trigger whose action is a plain `Key`. The live loop feeds
/// these to [`configure`] and EXCLUDES them from the engine (so the engine never additively
/// dispatches them — that would re-introduce the double-send). `None` for anything else.
///
/// Cross-platform: the ONLY OS-specific step is [`sys::physkey_for_usage`] (HID usage → platform
/// key-id — scancodes on Windows, evdev keycodes on Linux, …). A new platform gets this for free
/// once its `sys` fills that one function.
pub fn remap_for_rule(rule: &crate::engine::Rule) -> Option<Remap> {
    use crate::action::Action;
    use crate::engine::Trigger;
    if rule.layer.is_some() {
        return None; // hold-layer bindings keep their host-side semantics
    }
    let (page, usage, pid) = match &rule.trigger {
        Trigger::Input {
            page,
            usage,
            pid: Some(pid),
        } => (*page, *usage, *pid),
        _ => return None,
    };
    let from = match page {
        0x07 => sys::physkey_for_usage(usage)?,
        0xFF07 => usage, // unmapped key: the usage IS already a raw platform key-id (make | E0 flag)
        _ => return None, // consumer/button/etc. aren't keyboard keys — leave to the engine
    };
    let key = match &rule.action {
        Action::Key { key } => key,
        _ => return None,
    };
    let to = sys::physkey_for_usage(crate::action::hid_usage_for_key(key)? as u16)?;
    Some(Remap {
        pid,
        from,
        to: KeyOut::Scancode(to),
    })
}

/// Arm the shim from a live [`Engine`]: extract every rule the shim can own ([`remap_for_rule`])
/// and [`configure`] with them. The dispatcher separately SKIPS these triggers ([`owns`]) so the
/// engine never additively fires them. The ONE call the live loop makes on start + every reload.
pub fn configure_from_engine(engine: &crate::engine::Engine) {
    configure_from_engine_with(engine, None);
}

/// [`configure_from_engine`] plus a HELD-BIND claim: when a host feature owns a control as a held
/// trigger (the cast/weave trigger), pass it here and — if it is a pid-scoped keyboard key the
/// shim can express — its keystroke is swallowed device-scoped, so holding the Naga side-plate
/// key to weave stops typing `1111…` into the focused app. The feature still sees the hold via
/// the Raw-Input held registry (a hook swallow never blocks Raw Input). Device-any or
/// mouse-button binds contribute nothing (mouse swallowing stays with the click-guard).
pub fn configure_from_engine_with(
    engine: &crate::engine::Engine,
    held_bind: Option<crate::controls::ControlRef>,
) {
    configure(compose_remaps(engine, held_bind));
}

/// The pure remap-set composer behind [`configure_from_engine_with`] (split out so precedence is
/// unit-testable without touching the process-global shim). The held-bind claim WINS over an
/// ordinary engine remap on the same `(pid, key)`: resolution picks the FIRST matching rule, so a
/// user who both remapped a key and made it the cast trigger would otherwise have the remap
/// shadow the swallow — holding the trigger would type the remapped key into the focused app.
/// The trigger claim is the more specific intent (every other surface already stands down to it),
/// so the colliding remap is dropped, not merely out-ordered.
fn compose_remaps(
    engine: &crate::engine::Engine,
    held_bind: Option<crate::controls::ControlRef>,
) -> Vec<Remap> {
    let claim = held_bind.and_then(swallow_for_control);
    let mut remaps: Vec<Remap> = engine
        .rules
        .iter()
        .filter_map(remap_for_rule)
        .filter(|r| claim.is_none_or(|c| (r.pid, r.from) != (c.pid, c.from)))
        .collect();
    remaps.extend(claim);
    remaps
}

/// The swallow-only [`Remap`] for a held-bind control, when the shim can express it: pid-scoped
/// (a device-any bind would eat the key on EVERY keyboard — never) and on a keyboard page the
/// platform can hook. `None` otherwise.
pub fn swallow_for_control(ctl: crate::controls::ControlRef) -> Option<Remap> {
    let pid = ctl.pid?;
    let from = match ctl.page {
        0x07 => sys::physkey_for_usage(ctl.usage)?,
        0xFF07 => ctl.usage,
        _ => return None, // mouse/consumer/macro controls aren't LL-keyboard-hook territory
    };
    Some(Remap {
        pid,
        from,
        to: KeyOut::Swallow,
    })
}

/// Does the shim own this trigger `(page, usage, pid)`? The live dispatcher calls this to skip its
/// host-side dispatch for keys the shim already remaps at the input layer. `false` when disarmed or
/// on a non-keyboard page.
pub fn owns(page: u16, usage: u16, pid: u16) -> bool {
    // While paused the shim owns nothing, so the live dispatcher must NOT skip its own dispatch on
    // our behalf — otherwise the edge falls through the gap between the two of us.
    if standing_down() {
        return false;
    }
    let physkey = match page {
        0x07 => match sys::physkey_for_usage(usage) {
            Some(p) => p,
            None => return false,
        },
        0xFF07 => usage,
        _ => return false,
    };
    let g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.as_ref().is_some_and(|c| c.has_remap(physkey, pid))
}

/// Disarm the shim and uninstall the hook (the desktop returns to normal instantly).
pub fn deactivate() {
    ACTIVE.store(false, Ordering::SeqCst);
    if let Ok(mut g) = CORE.lock() {
        if let Some(c) = g.as_mut() {
            c.set_remaps([]);
        }
    }
    uninstall();
}

/// The HOOK's per-edge decision: should this keystroke be swallowed? Called from the LL hook proc
/// with the physkey (`scancode | 0x100 if extended`). Records it as pending when swallowed. Gated
/// on the input-arm kill-switch — in safe mode we swallow nothing (pure pass-through, no remap).
fn hook_edge(physkey: u16, down: bool) -> bool {
    // `standing_down` covers PAUSED too: swallowing a key mid-capture is exactly the bug the pause
    // exists to prevent (the user is BINDING this control, not using it).
    if standing_down() || !crate::action::input_armed() {
        return false;
    }
    let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match g.as_mut() {
        Some(c) => c.on_hook(physkey, down, now_us()),
        None => false,
    }
}

/// The RAW-INPUT reader's per-edge call (from `controls::listen`): attribute a swallowed keystroke
/// to its device and inject the resolved key. No-op unless armed. `pid` is the source device pid.
pub fn on_raw_keyboard(physkey: u16, down: bool, pid: u16) {
    // Paused means transparent in BOTH directions: no swallow above, no inject here.
    if standing_down() {
        return;
    }
    let inject = {
        let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.as_mut().and_then(|c| c.on_rawinput(physkey, down, pid))
    };
    if let Some(i) = inject {
        emit(i);
    }
}

/// Fail-open sweep — call on the dispatch tick. Replays any keystroke that was swallowed but never
/// attributed (so a key is never lost if Raw-Input is delayed/dropped).
pub fn expire_tick() {
    if !ACTIVE.load(Ordering::Relaxed) {
        return;
    }
    let replays = {
        let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.as_mut()
            .map(|c| c.expire(now_us(), EXPIRE_US))
            .unwrap_or_default()
    };
    for i in replays {
        emit(i);
    }
}

// ── the platform boundary: everything OS-specific lives in ONE place, `mod sys` ────────────────
// Both `sys` impls expose the SAME four verbs, so the shared code above/below is fully portable:
//   install()  / uninstall()        — begin / end intercepting
//   inject(physkey, down)           — synthesize a keystroke (our own, signature-stamped)
//   physkey_for_usage(usage) -> ..  — HID usage → platform key-id (scancode / evdev keycode / …)
// A new platform = fill in a `sys` with these four. The correlation brain ([`Interceptor`]) and all
// arming/engine wiring are shared and need no changes.

fn emit(i: Inject) {
    sys::inject(i.scancode, i.down);
}
fn install() {
    sys::install();
}
fn uninstall() {
    sys::uninstall();
}

#[cfg(windows)]
mod sys {
    use super::{hook_edge, INJECT_SIG};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::thread::JoinHandle;
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
        KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN,
        WM_KEYUP, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER,
    };

    const LLKHF_EXTENDED: u32 = 0x01;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static HANDLE: Mutex<isize> = Mutex::new(0);
    static PUMP: Mutex<Option<Pump>> = Mutex::new(None);
    static PUMP_TID: AtomicU32 = AtomicU32::new(0);

    struct Pump {
        join: JoinHandle<()>,
        tid: u32,
    }

    /// HID Keyboard/Keypad usage → Windows physkey (`scancode | 0x100 if extended`). The single
    /// OS-specific mapping the shared arming code needs; delegates to the one forward table.
    pub fn physkey_for_usage(usage: u16) -> Option<u16> {
        crate::controls::win::usage_to_physkey(usage)
    }

    /// SendInput one keyboard event BY SCANCODE, stamped with our signature so the hook passes it.
    /// Gated on the arm kill-switch (like `action::win_key`). `physkey = scancode | 0x100 if E0`.
    pub fn inject(physkey: u16, down: bool) {
        if !crate::action::input_armed() {
            return;
        }
        let extended = (physkey & 0x100) != 0;
        let mut flags = KEYEVENTF_SCANCODE;
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: (physkey & 0xFF),
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECT_SIG,
                },
            },
        };
        unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    }

    unsafe extern "system" fn proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let kb = &*(lparam as *const KBDLLHOOKSTRUCT);
            // Never touch our OWN injected replays — pass them straight through (no loop).
            if kb.dwExtraInfo != INJECT_SIG {
                let w = wparam as u32;
                let down = w == WM_KEYDOWN || w == WM_SYSKEYDOWN;
                let up = w == WM_KEYUP || w == WM_SYSKEYUP;
                if down || up {
                    let extended = (kb.flags & LLKHF_EXTENDED) != 0;
                    let physkey = (kb.scanCode as u16 & 0xFF) | if extended { 0x100 } else { 0 };
                    if hook_edge(physkey, down) {
                        return 1; // swallow: the legacy WM_KEYDOWN/CHAR path is suppressed.
                    }
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    fn pump_main() {
        // Same posture as the gaming-mode hook (see `hook::pump_main`): this callback sits in the OS
        // keyboard path, so being scheduled late delays every keystroke on the machine — and here it
        // would also widen the swallow→replay window this shim's whole design is built around.
        crate::timing::boost_input_thread();
        let h: HHOOK =
            unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(proc), std::ptr::null_mut(), 0) };
        if !h.is_null() {
            *HANDLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = h as isize;
            INSTALLED.store(true, Ordering::SeqCst);
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE) };
        PUMP_TID.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        if h.is_null() {
            return;
        }
        loop {
            let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break;
            }
        }
        let hh = *HANDLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if hh != 0 {
            unsafe { UnhookWindowsHookEx(hh as HHOOK) };
            *HANDLE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = 0;
        }
        INSTALLED.store(false, Ordering::SeqCst);
    }

    pub fn install() {
        let mut pump = PUMP.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if pump.is_some() {
            return; // already pumping
        }
        PUMP_TID.store(0, Ordering::SeqCst);
        let Ok(join) = crate::worker::spawn_named("neuron-remap-hook", pump_main) else {
            return;
        };
        let mut tid = 0u32;
        for _ in 0..1_000_000 {
            tid = PUMP_TID.load(Ordering::SeqCst);
            if tid != 0 {
                break;
            }
            std::thread::yield_now();
        }
        if tid == 0 || !INSTALLED.load(Ordering::SeqCst) {
            let _ = join.join();
            PUMP_TID.store(0, Ordering::SeqCst);
            return;
        }
        *pump = Some(Pump { join, tid });
    }

    pub fn uninstall() {
        let handle = PUMP.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let Some(Pump { join, tid }) = handle else {
            return;
        };
        unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
        let _ = join.join();
        PUMP_TID.store(0, Ordering::SeqCst);
    }
}

#[cfg(not(windows))]
mod sys {
    //! Non-Windows placeholder — the ONE module a Linux/macOS port fills. The shared code (the
    //! correlation core, arming, engine wiring) is complete and portable; only these verbs are
    //! OS-specific. Today they no-op, so the shim compiles everywhere but only *acts* on Windows.
    //!
    //! ## Filling this in
    //! **Linux (evdev + uinput) — the simplest of the three.** `EVIOCGRAB` a specific `/dev/input`
    //! node: that yields device-identified events AND suppresses them from the rest of the system
    //! in ONE step, so the swallow-and-replay correlation is NOT needed. `install` spawns a reader
    //! thread that, per grabbed event, calls [`super::Interceptor::resolve_direct`] (device known,
    //! no `pending`) and injects the result via `uinput`. `physkey_for_usage`: HID usage → evdev
    //! keycode.
    //!
    //! **macOS (CGEventTap + IOHIDManager).** A keyboard `CGEventTapCreate` tap can suppress and
    //! reinject in place; device attribution is the hard part (taps don't carry the device), so
    //! correlate against `IOHIDManager` the way Windows correlates against Raw-Input — REUSE the
    //! swallow/attribute core ([`super::Interceptor::on_hook`] + `on_rawinput`). `physkey_for_usage`:
    //! HID usage → the platform virtual keycode.

    /// No key-id map yet → [`super::remap_for_rule`] builds nothing, so the shim stays inert here.
    pub fn physkey_for_usage(_usage: u16) -> Option<u16> {
        None
    }
    pub fn inject(_physkey: u16, _down: bool) {}
    pub fn install() {}
    pub fn uninstall() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAGA: u16 = 0x00a8;
    const KBD: u16 = 0x0221;
    // '=' scancode 0x0D remapped to 'g' scancode 0x22, on the Naga only.
    fn eq_to_g() -> Remap {
        Remap {
            pid: NAGA,
            from: 0x0D,
            to: KeyOut::Scancode(0x22),
        }
    }

    #[test]
    fn naga_key_is_remapped_other_device_is_replayed() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        // The '=' scancode is remapped, so the hook swallows it (device unknown at hook time).
        assert!(i.on_hook(0x0D, true, 1000));
        // Raw-Input arrives ~0.5ms later attributing it to the NAGA -> emit 'g'.
        assert_eq!(
            i.on_rawinput(0x0D, true, NAGA),
            Some(Inject { scancode: 0x22, down: true })
        );
        // The release edge, same path.
        assert!(i.on_hook(0x0D, false, 1400));
        assert_eq!(
            i.on_rawinput(0x0D, false, NAGA),
            Some(Inject { scancode: 0x22, down: false })
        );
        assert_eq!(i.pending_len(), 0, "both edges resolved");

        // The SAME scancode from the real keyboard is swallowed too (device unknown at hook)...
        assert!(i.on_hook(0x0D, true, 2000));
        // ...but Raw-Input attributes it to the KEYBOARD -> replay the ORIGINAL '=' unchanged.
        assert_eq!(
            i.on_rawinput(0x0D, true, KBD),
            Some(Inject { scancode: 0x0D, down: true })
        );
    }

    #[test]
    fn unremapped_scancode_is_never_swallowed() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        assert!(!i.is_remapped_scancode(0x1E)); // 'a', not remapped
        assert!(!i.on_hook(0x1E, true, 1000), "hook must pass unremapped keys through");
        assert_eq!(i.pending_len(), 0);
    }

    /// `set_paused` must actually stand the shim down.
    ///
    /// Every test above drives a local [`Interceptor`] directly, which bypasses the module-level
    /// gates entirely — so `PAUSED` was stored by [`set_paused`] and then read by NOTHING, and a
    /// press-to-bind capture was still swallowed and remapped by the shim it was supposed to
    /// suspend. This exercises the gate itself so the flag can never go dead again.
    #[test]
    fn pausing_stands_the_shim_down() {
        // Process-global statics: restore both before returning so no later test inherits them.
        let was_active = ACTIVE.load(Ordering::SeqCst);

        ACTIVE.store(true, Ordering::SeqCst);
        set_paused(false);
        assert!(!standing_down(), "armed and unpaused, the shim is live");

        set_paused(true);
        assert!(
            standing_down(),
            "set_paused(true) must be OBSERVED by the per-edge gate, not just stored"
        );
        assert!(
            !hook_edge(0x0D, true),
            "a paused shim must never swallow the key being bound"
        );

        // A paused shim is transparent even while armed, so it must claim ownership of nothing.
        assert!(!owns(0x07, 0x2E, NAGA), "a paused shim owns no trigger");

        set_paused(false);
        ACTIVE.store(was_active, Ordering::SeqCst);
    }

    #[test]
    fn fail_open_replays_unattributed_keystrokes() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        i.on_hook(0x0D, true, 1000);
        // No Raw-Input arrived. Before the window, nothing expires.
        assert!(i.expire(1003, 5000).is_empty());
        // After the window, the swallowed key is replayed as-is (never lost).
        assert_eq!(
            i.expire(7000, 5000),
            vec![Inject { scancode: 0x0D, down: true }]
        );
        assert_eq!(i.pending_len(), 0);
    }

    #[test]
    fn injected_echo_without_pending_is_ignored() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        // A Raw-Input event with no matching pending entry (e.g. our own replay's echo) -> None.
        assert_eq!(i.on_rawinput(0x0D, true, KBD), None);
    }

    #[test]
    fn rapid_repeats_resolve_in_order() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        // Two quick down edges buffered before either Raw-Input arrives.
        i.on_hook(0x0D, true, 1000);
        i.on_hook(0x0D, true, 1100);
        assert_eq!(i.pending_len(), 2);
        // First Raw-Input (Naga) resolves the OLDEST -> 'g'.
        assert_eq!(
            i.on_rawinput(0x0D, true, NAGA),
            Some(Inject { scancode: 0x22, down: true })
        );
        assert_eq!(i.pending_len(), 1);
        // Second resolves the remaining one.
        assert_eq!(
            i.on_rawinput(0x0D, true, NAGA),
            Some(Inject { scancode: 0x22, down: true })
        );
        assert_eq!(i.pending_len(), 0);
    }

    #[test]
    fn resolve_direct_is_the_grab_model_path() {
        // The Linux/evdev backend knows the device at read time — no swallow, no pending.
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        // Naga '=' -> 'g' directly.
        assert_eq!(
            i.resolve_direct(0x0D, true, NAGA),
            Some(Inject { scancode: 0x22, down: true })
        );
        // The same key from another device -> passed through unchanged.
        assert_eq!(
            i.resolve_direct(0x0D, true, KBD),
            Some(Inject { scancode: 0x0D, down: true })
        );
        // An unremapped key -> unchanged, and no pending state was touched.
        assert_eq!(
            i.resolve_direct(0x1E, false, NAGA),
            Some(Inject { scancode: 0x1E, down: false })
        );
        assert_eq!(i.pending_len(), 0);
    }

    /// The held-bind claim must WIN over an ordinary engine remap on the same device key —
    /// resolution picks the first `(pid, from)` match, so without the compose-time drop the
    /// remap would shadow the swallow and holding the cast trigger would TYPE the remapped key.
    /// (Windows-only: composing needs the usage→scancode map.)
    #[cfg(windows)]
    #[test]
    fn held_bind_claim_beats_a_colliding_engine_remap() {
        use crate::action::Action;
        use crate::engine::{Engine, Rule, Trigger};
        let ctl = crate::controls::ControlRef {
            page: 0x07,
            usage: 0x1E, // the side-plate '1'
            pid: Some(NAGA),
        };
        // the same key carries a plain Key remap rule AND is the held cast trigger.
        let engine = Engine::from_rules(vec![
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1E, pid: Some(NAGA) },
                Action::Key { key: "g".into() },
            ),
            // an unrelated remap on another key must survive untouched.
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1F, pid: Some(NAGA) },
                Action::Key { key: "h".into() },
            ),
        ]);
        let remaps = compose_remaps(&engine, Some(ctl));
        let from = sys::physkey_for_usage(0x1E).expect("'1' has a scancode");
        let on_trigger: Vec<_> = remaps
            .iter()
            .filter(|r| r.pid == NAGA && r.from == from)
            .collect();
        assert_eq!(on_trigger.len(), 1, "exactly one rule may own the trigger key");
        assert_eq!(on_trigger[0].to, KeyOut::Swallow, "and it is the swallow claim");
        let other = sys::physkey_for_usage(0x1F).expect("'2' has a scancode");
        assert!(
            remaps.iter().any(|r| r.from == other && matches!(r.to, KeyOut::Scancode(_))),
            "the non-colliding remap survives"
        );
    }

    /// A held-bind claim (the cast trigger on a specific device) swallows that device's
    /// keystroke entirely — and ONLY that device's: the same key elsewhere replays unchanged.
    #[test]
    fn swallow_claim_eats_the_devices_key_and_only_that_devices() {
        let mut i = Interceptor::new();
        // '1' scancode 0x02 claimed as a held bind on the Naga.
        i.set_remaps([Remap {
            pid: NAGA,
            from: 0x02,
            to: KeyOut::Swallow,
        }]);
        assert!(i.on_hook(0x02, true, 1000), "claimed scancode is swallowed at hook time");
        // Naga attribution -> nothing injected: the '1' never reaches the desktop.
        assert_eq!(i.on_rawinput(0x02, true, NAGA), None);
        // The real keyboard's '1' is swallowed then replayed unchanged.
        assert!(i.on_hook(0x02, true, 2000));
        assert_eq!(
            i.on_rawinput(0x02, true, KBD),
            Some(Inject { scancode: 0x02, down: true })
        );
        // Grab-model path agrees.
        assert_eq!(i.resolve_direct(0x02, true, NAGA), None);
        assert_eq!(
            i.resolve_direct(0x02, true, KBD),
            Some(Inject { scancode: 0x02, down: true })
        );
    }

    #[test]
    fn set_remaps_clears_stale_pending() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        i.on_hook(0x0D, true, 1000);
        assert_eq!(i.pending_len(), 1);
        // Reloading the config drops in-flight keystrokes (they'd resolve against stale rules).
        i.set_remaps([eq_to_g()]);
        assert_eq!(i.pending_len(), 0);
    }
}
