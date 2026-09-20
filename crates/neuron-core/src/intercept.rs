// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Device-scoped input claims — the user-mode "input shim", keyboard AND mouse.
//!
//! Some devices (the Razer Naga thumb grid) are hardware keyboards with FIXED key usages; they
//! can't be remapped in firmware (proven live — see the `naga-side-plate-binding` notes). Razer
//! itself remaps them host-side with a kernel HID filter (`RzDev_*.sys`). This module is the SAME
//! mechanism in user mode, no driver: swallow the device's emission and (for plain key remaps)
//! inject a replacement, device-scoped so the SAME physical control on another device is
//! untouched.
//!
//! THE OWNERSHIP PRINCIPLE, device-agnostic: a pid-scoped BOUND control owns its input. A
//! keyboard key bound to a plain `Key` is REPLACED (swallow + inject the target); a keyboard key
//! bound to anything else, and any bound mouse middle/X button, is SWALLOWED — the original
//! emission never reaches the desktop, and the live dispatcher fires the bound action off Raw
//! Input (which a hook swallow does not block). Mouse buttons ride the same correlation core in
//! their own physkey namespace (`0x8000 | button`) behind a separately-installed `WH_MOUSE_LL`
//! hook; left/right are never claimable.
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
    /// Canonical source device — the same identity the rule spine and the held registry speak, so
    /// the shim cannot disagree with them about which device a claim belongs to.
    pub pid: crate::registry::CanonicalPid,
    pub from: u16,
    pub to: KeyOut,
}

/// The MOUSE-BUTTON physkey namespace: mouse buttons live in the same correlation core as
/// keyboard scancodes, encoded as `0x8000 | button` (buttons 3=middle, 4=X1, 5=X2 — the only
/// swallowable ones; left/right are never claimed). Scancodes are ≤ 0x1FF, so the namespaces
/// can't collide, and one `Interceptor` serves both hooks.
pub const MOUSE_PHYSKEY_BASE: u16 = 0x8000;

/// Encode a Button-page usage (3..=5) as its mouse physkey.
#[must_use]
pub fn mouse_physkey(button: u16) -> u16 {
    MOUSE_PHYSKEY_BASE | button
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
    #[must_use]
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
    #[must_use]
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
    pub fn on_rawinput(
        &mut self,
        scancode: u16,
        down: bool,
        pid: crate::registry::CanonicalPid,
    ) -> Option<Inject> {
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
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// True if no remaps are configured (the whole shim is a no-op).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_scancode.is_empty()
    }

    /// Is there a remap for this (physkey, device pid)? Lets the live dispatcher SKIP its own
    /// host-side handling of a trigger the shim already owns (avoids the double-send).
    #[must_use]
    pub fn has_remap(&self, physkey: u16, pid: crate::registry::CanonicalPid) -> bool {
        self.by_scancode
            .get(&physkey)
            .is_some_and(|v| v.iter().any(|r| r.pid == pid))
    }

    /// Like [`has_remap`], but ONLY for replacement remaps ([`KeyOut::Scancode`]). Swallow claims
    /// don't count — the dispatcher must still fire their actions (see the module-level `owns`).
    #[must_use]
    pub fn has_key_remap(&self, physkey: u16, pid: crate::registry::CanonicalPid) -> bool {
        self.by_scancode.get(&physkey).is_some_and(|v| {
            v.iter()
                .any(|r| r.pid == pid && matches!(r.to, KeyOut::Scancode(_)))
        })
    }

    /// GRAB-MODEL resolve (e.g. Linux evdev `EVIOCGRAB`): the device is known at read time, so
    /// there is no separate swallow/hook and no `pending` to match — look the remap up directly.
    /// Returns the target key if this (physkey, pid) is remapped, else the original (pass-through).
    /// Unlike [`on_rawinput`], this never touches `pending` (the Windows correlation path). The
    /// Windows/macOS backends use `on_hook`+`on_rawinput`; a grab backend uses this instead.
    /// `None` = the key is claimed with [`KeyOut::Swallow`] — emit nothing.
    #[must_use]
    pub fn resolve_direct(
        &self,
        physkey: u16,
        down: bool,
        pid: crate::registry::CanonicalPid,
    ) -> Option<Inject> {
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
#[cfg(windows)]
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

/// Arm the shim with the active device claims (from the engine's pid-scoped bindings). Installs
/// each hook only when a claim in ITS namespace exists — a config with only keyboard claims never
/// pays for a global mouse hook, and vice versa. A later empty config tears everything down.
/// Idempotent.
pub fn configure(remaps: Vec<Remap>) {
    let any = !remaps.is_empty();
    let want_mouse = remaps.iter().any(|r| r.from & MOUSE_PHYSKEY_BASE != 0);
    let want_kbd = remaps.iter().any(|r| r.from & MOUSE_PHYSKEY_BASE == 0);
    {
        let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.get_or_insert_with(Interceptor::new).set_remaps(remaps);
    }
    if any {
        ACTIVE.store(true, Ordering::SeqCst);
        if want_kbd {
            sys::install();
        } else {
            sys::uninstall();
        }
        if want_mouse {
            sys::install_mouse();
        } else {
            sys::uninstall_mouse();
        }
    } else {
        deactivate();
    }
}

/// Build the shim's CLAIM on one engine rule, if it can express one: a base-layer, pid-scoped
/// `Input` trigger on a keyboard page or a mouse middle/X button.
///
///   * keyboard key + plain `Key` action → a REPLACEMENT remap ([`KeyOut::Scancode`]); the shim
///     emits the target and the dispatcher SKIPS the rule ([`owns`]) — no double-send;
///   * anything else the shim can hook (keyboard key with a non-Key action, any claimable mouse
///     button) → a SWALLOW claim: the original emission is suppressed, and the dispatcher still
///     fires the action off Raw Input (which a hook swallow never blocks).
///
/// Hold-layer rules and device-any binds are never claimed (a device-any swallow would eat the
/// control on EVERY device). `None` for anything else.
///
/// Cross-platform: the ONLY OS-specific step is [`sys::physkey_for_usage`] (HID usage → platform
/// key-id — scancodes on Windows, evdev keycodes on Linux, …). A new platform gets this for free
/// once its `sys` fills that one function.
#[must_use]
pub fn claim_for_rule(rule: &crate::engine::Rule) -> Option<Remap> {
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
        } => (
            *page,
            *usage,
            // Already canonical on both sides by type — the rule carries a CanonicalPid and so
            // does the raw-input attribution that resolves it.
            *pid,
        ),
        _ => return None,
    };
    let from = match page {
        0x07 => sys::physkey_for_usage(usage)?,
        0xFF07 => usage, // unmapped key: the usage IS already a raw platform key-id (make | E0 flag)
        // Mouse middle/X buttons (3..=5): the mouse-hook namespace. Left/right are NEVER claimed
        // — swallowing the primary buttons on a bad config would unusable the desktop.
        0x09 if (3..=5).contains(&usage) => mouse_physkey(usage),
        _ => return None, // consumer/macro/etc. have no OS emission to suppress — engine-only
    };
    // THE OWNERSHIP PRINCIPLE, device-agnostic: a pid-scoped BOUND control owns its input.
    //   * a plain `Key` remap on a keyboard key REPLACES the keystroke (Scancode target);
    //   * every other action — and every mouse-button bind — SWALLOWS the original emission and
    //     lets host dispatch fire the action off Raw Input (which a hook swallow never blocks).
    // Without the swallow arm, a side-plate key bound to a macro typed its digit AND ran the
    // macro, and a bound mouse side button still clicked into the app underneath — the exact
    // "additive, not a rebind" feel this shim exists to kill.
    let to = match &rule.action {
        Action::Key { key } if page != 0x09 => {
            KeyOut::Scancode(sys::physkey_for_usage(u16::from(crate::action::hid_usage_for_key(key)?))?)
        }
        _ => KeyOut::Swallow,
    };
    Some(Remap { pid, from, to })
}

/// Arm the shim from a live [`Engine`]: extract every rule the shim can claim ([`claim_for_rule`])
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
        .filter_map(claim_for_rule)
        .filter(|r| claim.is_none_or(|c| (r.pid, r.from) != (c.pid, c.from)))
        .collect();
    remaps.extend(claim);
    remaps
}

/// The swallow-only [`Remap`] for a held-bind control, when the shim can express it: pid-scoped
/// (a device-any bind would eat the key on EVERY keyboard — never) and on a keyboard page the
/// platform can hook. `None` otherwise.
#[must_use]
pub fn swallow_for_control(ctl: crate::controls::ControlRef) -> Option<Remap> {
    let pid = ctl.pid?;
    let from = match ctl.page {
        0x07 => sys::physkey_for_usage(ctl.usage)?,
        0xFF07 => ctl.usage,
        // mouse middle/X buttons ride the mouse-hook namespace; left/right are never claimed.
        0x09 if (3..=5).contains(&ctl.usage) => mouse_physkey(ctl.usage),
        _ => return None, // consumer/macro controls have no OS emission to suppress
    };
    Some(Remap {
        pid,
        from,
        to: KeyOut::Swallow,
    })
}

/// Does the shim FULLY own this trigger `(page, usage, pid)` — i.e. hold a REPLACEMENT remap
/// ([`KeyOut::Scancode`]) for it? The live dispatcher calls this to skip its host-side dispatch
/// for keys the shim already re-emits at the input layer. SWALLOW claims deliberately return
/// `false`: they only suppress the original emission, and the dispatcher must still fire the
/// bound action (skipping it would make every swallowed bind dead). `false` when disarmed or on
/// an unclaimable page.
pub fn owns(page: u16, usage: u16, pid: u16) -> bool {
    let pid = crate::registry::CanonicalPid::of(pid);
    // While paused the shim owns nothing, so the live dispatcher must NOT skip its own dispatch on
    // our behalf — otherwise the edge falls through the gap between the two of us.
    if standing_down() {
        return false;
    }
    #[cfg(target_os = "linux")]
    if !crate::linux_input::grabbed(pid) {
        return false;
    }
    let physkey = match page {
        0x07 => match sys::physkey_for_usage(usage) {
            Some(p) => p,
            None => return false,
        },
        0xFF07 => usage,
        _ => return false, // mouse claims are always Swallow — never owned
    };
    let g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    g.as_ref().is_some_and(|c| c.has_key_remap(physkey, pid))
}

/// Disarm the shim and uninstall both hooks (the desktop returns to normal instantly).
pub fn deactivate() {
    ACTIVE.store(false, Ordering::SeqCst);
    if let Ok(mut g) = CORE.lock() {
        if let Some(c) = g.as_mut() {
            c.set_remaps([]);
        }
    }
    sys::uninstall();
    sys::uninstall_mouse();
}

/// Whether this physical Linux device has a claim that requires exclusive replay. A listener
/// creates its uinput mirror before taking the grab, and releases the grab on any replay fault.
#[cfg(target_os = "linux")]
pub fn linux_wants_grab(pid: crate::registry::CanonicalPid) -> bool {
    if standing_down() || !crate::action::input_armed() {
        return false;
    }
    CORE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .is_some_and(|core| core.by_scancode.values().flatten().any(|claim| claim.pid == pid))
}

/// Resolve one key from an already-grabbed, device-identified evdev stream. The original event
/// passes through whenever the shim is paused or disarmed; only an active device claim can eat it.
#[cfg(target_os = "linux")]
pub fn linux_resolve(physkey: u16, down: bool, pid: crate::registry::CanonicalPid) -> Option<Inject> {
    if standing_down() || !crate::action::input_armed() {
        return Some(Inject { scancode: physkey, down });
    }
    let core = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match core.as_ref() {
        Some(core) => core.resolve_direct(physkey, down, pid),
        None => Some(Inject { scancode: physkey, down }),
    }
}

/// The HOOK's per-edge decision: should this keystroke be swallowed? Called from the LL hook proc
/// with the physkey (`scancode | 0x100 if extended`). Records it as pending when swallowed. Gated
/// on the input-arm kill-switch — in safe mode we swallow nothing (pure pass-through, no remap).
#[cfg_attr(not(windows), allow(dead_code))]
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
/// to its device and inject the resolved key. No-op unless armed. `pid` is the RAW pid straight off
/// the wire — this is one of the two doors where it becomes a canonical identity, so callers never
/// have to know the rule.
pub fn on_raw_keyboard(physkey: u16, down: bool, pid: u16) {
    let pid = crate::registry::CanonicalPid::of(pid);
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

/// The RAW-INPUT reader's per-edge call for MOUSE buttons (3=middle, 4=X1, 5=X2): the mouse-hook
/// twin of [`on_raw_keyboard`], sharing the same correlation core via the mouse physkey
/// namespace. Attributes a hook-swallowed click and replays it when it came from an unclaimed
/// device.
pub fn on_raw_mouse(button: u16, down: bool, pid: u16) {
    on_raw_keyboard(mouse_physkey(button), down, pid);
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
    if i.scancode & MOUSE_PHYSKEY_BASE != 0 {
        sys::inject_mouse(i.scancode & !MOUSE_PHYSKEY_BASE, i.down);
    } else {
        sys::inject(i.scancode, i.down);
    }
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
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_MIDDLEDOWN,
        MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW,
        UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE,
        WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_QUIT,
        WM_SYSKEYDOWN, WM_SYSKEYUP, WM_USER, WM_XBUTTONDOWN, WM_XBUTTONUP,
    };

    const LLKHF_EXTENDED: u32 = 0x01;
    const XBUTTON1: u16 = 0x0001;
    const XBUTTON2: u16 = 0x0002;

    static INSTALLED: AtomicBool = AtomicBool::new(false);
    static HANDLE: Mutex<isize> = Mutex::new(0);
    static PUMP: Mutex<Option<Pump>> = Mutex::new(None);
    static PUMP_TID: AtomicU32 = AtomicU32::new(0);

    // The MOUSE hook's own pump slot — same lifecycle as the keyboard one, separately installed
    // so a keyboard-only config never pays for a global mouse hook (and vice versa).
    static M_INSTALLED: AtomicBool = AtomicBool::new(false);
    static M_HANDLE: Mutex<isize> = Mutex::new(0);
    static M_PUMP: Mutex<Option<Pump>> = Mutex::new(None);
    static M_PUMP_TID: AtomicU32 = AtomicU32::new(0);

    struct Pump {
        join: JoinHandle<()>,
        tid: u32,
    }

    /// HID Keyboard/Keypad usage → Windows physkey (`scancode | 0x100 if extended`). The single
    /// OS-specific mapping the shared arming code needs; delegates to the one forward table.
    pub fn physkey_for_usage(usage: u16) -> Option<u16> {
        crate::controls::win::usage_to_physkey(usage)
    }

    /// `SendInput` one keyboard event BY SCANCODE, stamped with our signature so the hook passes it.
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
        unsafe { SendInput(1, &raw const input, std::mem::size_of::<INPUT>() as i32) };
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

    /// The mouse twin of `proc`: middle/X button edges enter the SAME correlation core via the
    /// mouse physkey namespace. Left/right buttons and motion pass through untouched — this proc
    /// never even inspects them beyond the message id, so the pointer path stays cold.
    unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            let ms = &*(lparam as *const MSLLHOOKSTRUCT);
            // Never touch our OWN injected replays — pass them straight through (no loop).
            if ms.dwExtraInfo != INJECT_SIG {
                let button_edge = match wparam as u32 {
                    WM_MBUTTONDOWN => Some((3u16, true)),
                    WM_MBUTTONUP => Some((3u16, false)),
                    WM_XBUTTONDOWN | WM_XBUTTONUP => {
                        let x = (ms.mouseData >> 16) as u16;
                        let n = if x == XBUTTON2 { 5u16 } else { 4u16 };
                        Some((n, wparam as u32 == WM_XBUTTONDOWN))
                    }
                    _ => None,
                };
                if let Some((button, down)) = button_edge {
                    if hook_edge(super::mouse_physkey(button), down) {
                        return 1; // swallow: the click never reaches the app underneath.
                    }
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// One hook pump's shared body — both hooks (keyboard, mouse) run this with their own slot
    /// statics and proc. This callback sits in the OS input path, so being scheduled late delays
    /// every event on the machine — and here it would also widen the swallow→replay window this
    /// shim's whole design is built around.
    #[allow(clippy::type_complexity)]
    fn pump_main_for(
        hook_id: i32,
        hook_proc: unsafe extern "system" fn(i32, WPARAM, LPARAM) -> LRESULT,
        installed: &AtomicBool,
        handle: &Mutex<isize>,
        pump_tid: &AtomicU32,
    ) {
        crate::timing::boost_input_thread();
        let h: HHOOK =
            unsafe { SetWindowsHookExW(hook_id, Some(hook_proc), std::ptr::null_mut(), 0) };
        if !h.is_null() {
            *handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = h as isize;
            installed.store(true, Ordering::SeqCst);
        }
        let mut msg: MSG = unsafe { std::mem::zeroed() };
        unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), WM_USER, WM_USER, PM_NOREMOVE) };
        pump_tid.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        if h.is_null() {
            return;
        }
        loop {
            let r = unsafe { GetMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0) };
            if r <= 0 {
                break;
            }
        }
        let hh = *handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if hh != 0 {
            unsafe { UnhookWindowsHookEx(hh as HHOOK) };
            *handle.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = 0;
        }
        installed.store(false, Ordering::SeqCst);
    }

    fn install_for(
        name: &'static str,
        pump: &Mutex<Option<Pump>>,
        pump_tid: &'static AtomicU32,
        installed: &'static AtomicBool,
        body: fn(),
    ) {
        let mut pump = pump.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if pump.is_some() {
            return; // already pumping
        }
        pump_tid.store(0, Ordering::SeqCst);
        let Ok(join) = crate::worker::spawn_named(name, body) else {
            return;
        };
        let mut tid = 0u32;
        for _ in 0..1_000_000 {
            tid = pump_tid.load(Ordering::SeqCst);
            if tid != 0 {
                break;
            }
            std::thread::yield_now();
        }
        if tid == 0 || !installed.load(Ordering::SeqCst) {
            let _ = join.join();
            pump_tid.store(0, Ordering::SeqCst);
            return;
        }
        *pump = Some(Pump { join, tid });
    }

    fn uninstall_for(pump: &Mutex<Option<Pump>>, pump_tid: &AtomicU32) {
        let handle = pump.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
        let Some(Pump { join, tid }) = handle else {
            return;
        };
        unsafe { PostThreadMessageW(tid, WM_QUIT, 0, 0) };
        let _ = join.join();
        pump_tid.store(0, Ordering::SeqCst);
    }

    pub fn install() {
        install_for("neuron-remap-hook", &PUMP, &PUMP_TID, &INSTALLED, || {
            pump_main_for(WH_KEYBOARD_LL, proc, &INSTALLED, &HANDLE, &PUMP_TID);
        });
    }

    pub fn uninstall() {
        uninstall_for(&PUMP, &PUMP_TID);
    }

    pub fn install_mouse() {
        install_for("neuron-remap-mhook", &M_PUMP, &M_PUMP_TID, &M_INSTALLED, || {
            pump_main_for(WH_MOUSE_LL, mouse_proc, &M_INSTALLED, &M_HANDLE, &M_PUMP_TID);
        });
    }

    pub fn uninstall_mouse() {
        uninstall_for(&M_PUMP, &M_PUMP_TID);
    }

    /// `SendInput` one mouse-button event (3=middle, 4=X1, 5=X2), stamped with our signature so the
    /// mouse hook passes it — the replay path for a swallowed click that turned out to belong to
    /// an unclaimed device. Gated on the arm kill-switch like every injection.
    pub fn inject_mouse(button: u16, down: bool) {
        if !crate::action::input_armed() {
            return;
        }
        let (flags, data) = match (button, down) {
            (3, true) => (MOUSEEVENTF_MIDDLEDOWN, 0u32),
            (3, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (4, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON1)),
            (4, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON1)),
            (5, true) => (MOUSEEVENTF_XDOWN, u32::from(XBUTTON2)),
            (5, false) => (MOUSEEVENTF_XUP, u32::from(XBUTTON2)),
            _ => return,
        };
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: INJECT_SIG,
                },
            },
        };
        unsafe { SendInput(1, &raw const input, std::mem::size_of::<INPUT>() as i32) };
    }
}

#[cfg(target_os = "linux")]
mod sys {
    pub fn physkey_for_usage(usage: u16) -> Option<u16> {
        crate::linux_input::keycode_for_usage(usage)
    }
    pub fn inject(physkey: u16, down: bool) {
        let _ = crate::linux_input::emit_key(physkey, down);
    }
    pub fn inject_mouse(button: u16, down: bool) {
        let code = match button {
            1..=5 => 0x10F + button,
            _ => return,
        };
        let _ = crate::linux_input::emit_key(code, down);
    }
    pub fn install() {}
    pub fn uninstall() {}
    pub fn install_mouse() {}
    pub fn uninstall_mouse() {}
}

#[cfg(not(any(windows, target_os = "linux")))]
mod sys {
    pub fn physkey_for_usage(_usage: u16) -> Option<u16> { None }
    pub fn inject(_physkey: u16, _down: bool) {}
    pub fn inject_mouse(_button: u16, _down: bool) {}
    pub fn install() {}
    pub fn uninstall() {}
    pub fn install_mouse() {}
    pub fn uninstall_mouse() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAGA_RAW: u16 = 0x00a8;
    const KBD_RAW: u16 = 0x0221;
    /// Test devices as the spine sees them. `CanonicalPid::of` is not `const`, so these are
    /// functions — which is also the honest shape: canonicalization is a lookup against the loaded
    /// registry, not a compile-time constant.
    fn naga() -> crate::registry::CanonicalPid {
        crate::registry::CanonicalPid::of(NAGA_RAW)
    }
    fn kbd() -> crate::registry::CanonicalPid {
        crate::registry::CanonicalPid::of(KBD_RAW)
    }
    // '=' scancode 0x0D remapped to 'g' scancode 0x22, on the Naga only.
    fn eq_to_g() -> Remap {
        Remap {
            pid: naga(),
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
        // Raw-Input arrives ~0.5ms later attributing it to the naga() -> emit 'g'.
        assert_eq!(
            i.on_rawinput(0x0D, true, naga()),
            Some(Inject { scancode: 0x22, down: true })
        );
        // The release edge, same path.
        assert!(i.on_hook(0x0D, false, 1400));
        assert_eq!(
            i.on_rawinput(0x0D, false, naga()),
            Some(Inject { scancode: 0x22, down: false })
        );
        assert_eq!(i.pending_len(), 0, "both edges resolved");

        // The SAME scancode from the real keyboard is swallowed too (device unknown at hook)...
        assert!(i.on_hook(0x0D, true, 2000));
        // ...but Raw-Input attributes it to the KEYBOARD -> replay the ORIGINAL '=' unchanged.
        assert_eq!(
            i.on_rawinput(0x0D, true, kbd()),
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
        assert!(!owns(0x07, 0x2E, NAGA_RAW), "a paused shim owns no trigger");

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
        assert_eq!(i.on_rawinput(0x0D, true, kbd()), None);
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
            i.on_rawinput(0x0D, true, naga()),
            Some(Inject { scancode: 0x22, down: true })
        );
        assert_eq!(i.pending_len(), 1);
        // Second resolves the remaining one.
        assert_eq!(
            i.on_rawinput(0x0D, true, naga()),
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
            i.resolve_direct(0x0D, true, naga()),
            Some(Inject { scancode: 0x22, down: true })
        );
        // The same key from another device -> passed through unchanged.
        assert_eq!(
            i.resolve_direct(0x0D, true, kbd()),
            Some(Inject { scancode: 0x0D, down: true })
        );
        // An unremapped key -> unchanged, and no pending state was touched.
        assert_eq!(
            i.resolve_direct(0x1E, false, naga()),
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
            pid: Some(naga()),
        };
        // the same key carries a plain Key remap rule AND is the held cast trigger.
        let engine = Engine::from_rules(vec![
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1E, pid: Some(naga()) },
                Action::Key { key: "g".into() },
            ),
            // an unrelated remap on another key must survive untouched.
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1F, pid: Some(naga()) },
                Action::Key { key: "h".into() },
            ),
        ]);
        let remaps = compose_remaps(&engine, Some(ctl));
        // stored pids canonicalize through the registry: the dongle pid (00a8, the test's naga())
        // composes into claims under the Naga's canonical identity (00a7, its first mode).
        let canon = naga();
        let from = sys::physkey_for_usage(0x1E).expect("'1' has a scancode");
        let on_trigger: Vec<_> = remaps
            .iter()
            .filter(|r| r.pid == canon && r.from == from)
            .collect();
        assert_eq!(on_trigger.len(), 1, "exactly one rule may own the trigger key");
        assert_eq!(on_trigger[0].to, KeyOut::Swallow, "and it is the swallow claim");
        let other = sys::physkey_for_usage(0x1F).expect("'2' has a scancode");
        assert!(
            remaps.iter().any(|r| r.from == other && matches!(r.to, KeyOut::Scancode(_))),
            "the non-colliding remap survives"
        );
    }

    /// THE OWNERSHIP PRINCIPLE, per device class: a pid-scoped bound control claims its input —
    /// keyboard Key rules become replacement remaps, every other claimable bind (keyboard key
    /// with a non-Key action, mouse middle/X button with anything) becomes a swallow. Left/right
    /// mouse buttons and device-any rules are never claimed.
    #[cfg(windows)]
    #[test]
    fn claims_cover_every_pid_scoped_bind_not_just_key_remaps() {
        use crate::action::Action;
        use crate::engine::{Engine, Rule, Trigger};
        let engine = Engine::from_rules(vec![
            // keyboard key → Key: a replacement remap
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1E, pid: Some(crate::registry::CanonicalPid::of(0x0221)) },
                Action::Key { key: "g".into() },
            ),
            // keyboard key → a macro: swallow (the action is the meaning now)
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x1F, pid: Some(crate::registry::CanonicalPid::of(0x0221)) },
                Action::Run { cmd: "echo m".into() },
            ),
            // mouse X1 → anything: swallow in the mouse namespace
            Rule::new(
                Trigger::Input { page: 0x09, usage: 4, pid: Some(crate::registry::CanonicalPid::of(0x0221)) },
                Action::Key { key: "q".into() },
            ),
            // LEFT mouse button: never claimed, no matter the bind
            Rule::new(
                Trigger::Input { page: 0x09, usage: 1, pid: Some(crate::registry::CanonicalPid::of(0x0221)) },
                Action::Run { cmd: "echo m".into() },
            ),
            // device-any: never claimed (a global swallow would eat every device's key)
            Rule::new(
                Trigger::Input { page: 0x07, usage: 0x20, pid: None },
                Action::Key { key: "h".into() },
            ),
        ]);
        let remaps = compose_remaps(&engine, None);
        assert_eq!(remaps.len(), 3, "key remap + keyboard swallow + mouse swallow");
        let sc_1e = sys::physkey_for_usage(0x1E).unwrap();
        let sc_1f = sys::physkey_for_usage(0x1F).unwrap();
        assert!(remaps
            .iter()
            .any(|r| r.from == sc_1e && matches!(r.to, KeyOut::Scancode(_))));
        assert!(remaps.iter().any(|r| r.from == sc_1f && r.to == KeyOut::Swallow));
        assert!(remaps
            .iter()
            .any(|r| r.from == mouse_physkey(4) && r.to == KeyOut::Swallow));
    }

    /// `owns` must claim ONLY replacement remaps — a swallow claim suppresses the emission but
    /// the dispatcher still fires the action; skipping it would kill every swallowed bind.
    #[test]
    fn owns_counts_key_remaps_but_never_swallow_claims() {
        let mut i = Interceptor::new();
        i.set_remaps([
            Remap { pid: naga(), from: 0x0D, to: KeyOut::Scancode(0x22) },
            Remap { pid: naga(), from: 0x02, to: KeyOut::Swallow },
        ]);
        assert!(i.has_key_remap(0x0D, naga()), "a replacement remap is owned");
        assert!(!i.has_key_remap(0x02, naga()), "a swallow claim is NOT owned");
        assert!(i.has_remap(0x02, naga()), "…but it IS a claim (the hook swallows it)");
    }

    /// A held-bind claim (the cast trigger on a specific device) swallows that device's
    /// keystroke entirely — and ONLY that device's: the same key elsewhere replays unchanged.
    #[test]
    fn swallow_claim_eats_the_devices_key_and_only_that_devices() {
        let mut i = Interceptor::new();
        // '1' scancode 0x02 claimed as a held bind on the Naga.
        i.set_remaps([Remap {
            pid: naga(),
            from: 0x02,
            to: KeyOut::Swallow,
        }]);
        assert!(i.on_hook(0x02, true, 1000), "claimed scancode is swallowed at hook time");
        // Naga attribution -> nothing injected: the '1' never reaches the desktop.
        assert_eq!(i.on_rawinput(0x02, true, naga()), None);
        // The real keyboard's '1' is swallowed then replayed unchanged.
        assert!(i.on_hook(0x02, true, 2000));
        assert_eq!(
            i.on_rawinput(0x02, true, kbd()),
            Some(Inject { scancode: 0x02, down: true })
        );
        // Grab-model path agrees.
        assert_eq!(i.resolve_direct(0x02, true, naga()), None);
        assert_eq!(
            i.resolve_direct(0x02, true, kbd()),
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
