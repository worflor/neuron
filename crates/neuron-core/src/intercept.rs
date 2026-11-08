// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Device-scoped input claims — the user-mode "input shim", keyboard AND mouse.
//!
//! A device that can reassign its buttons in firmware (see [`crate::buttons`]) performs its binds
//! itself; this module then only swallows the private keys it hands to the host, which name their
//! device by themselves. For everything else it is the host-side fallback: swallow the device's
//! emission and (for plain key remaps) inject a replacement, device-scoped so the SAME physical
//! control on another device is untouched.
//!
//! THE OWNERSHIP PRINCIPLE, device-agnostic: a pid-scoped BOUND control owns its input. A
//! keyboard key bound to a plain `Key` is REPLACED (swallow + inject the target); a keyboard key
//! bound to anything else, and any bound mouse middle/X button, is SWALLOWED. Mouse buttons ride
//! the same core in their own physkey namespace (`0x8000 | button`) behind a separately-installed
//! `WH_MOUSE_LL` hook; left/right are never claimable.
//!
//! ## The hook decides, because its swallow hides the key from Raw Input
//! The low-level hook can swallow but does not know the device; Raw Input knows the device but
//! arrives after the hook, and NOT AT ALL for a key the hook swallowed (measured with `hookorder`
//! and live 2026-09-29; re-observed 2026-11-08). So the hook decides alone
//! ([`Interceptor::on_edge`]):
//!   * a conditional claim ([`KeyOut::SwallowWhileHeld`]) whose hold key is down takes the key as
//!     its own device's;
//!   * otherwise the key is attributed: among the devices Raw Input has seen in its namespace that
//!     can send it (a definition's `[[buttons]]` limits a device to those keys), the one active
//!     most recently. A key only one device can send is exact; a key two keyboards share follows
//!     the one typed on last, so the first press after switching keyboards can be misattributed;
//!   * a key no known device claims passes untouched.
//!
//! A taken key is delivered as its device's edge so its bind fires, or replaced by its target.
//! Firmware-assigned private keys skip attribution: they name their device themselves.
//!
//! [`Interceptor`] is the PURE decision core (no Win32), fully unit-tested; the live hook and the
//! Raw-Input feed drive it one event at a time. Our own output carries a `dwExtraInfo` signature
//! the hook passes through, so the shim never re-processes it.

use std::collections::{HashMap, HashSet};

/// What a matched keystroke should turn into.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyOut {
    /// Emit this physical scancode instead (a plain key remap).
    Scancode(u16),
    /// Emit NOTHING — the key is claimed by a host feature (the cast/weave trigger) and its
    /// keystroke must not reach the desktop at all. The same key on any OTHER device still
    /// passes unchanged, exactly like a remap.
    Swallow,
    /// Taken while this control (a layer's hold key) is down, and otherwise left alone. Taken as
    /// the claim's device without attribution: holding that device's key is the evidence.
    SwallowWhileHeld(crate::controls::ControlRef),
}

/// One device-scoped remap: pressing physical `from` scancode on device `pid` emits `to`. The same
/// scancode on any other device passes unchanged.
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

/// A key the hook took: the device it was attributed to and what it becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    pub pid: crate::registry::CanonicalPid,
    /// [`KeyOut::Scancode`] (inject the target) or [`KeyOut::Swallow`] (deliver it as `pid`'s).
    pub to: KeyOut,
    /// `false` for an auto-repeat of a press already acted on.
    pub first: bool,
}

/// The pure decision core. Not `Send`-shared directly — the live layer wraps it in a mutex and
/// drives [`Interceptor::on_edge`] from the hooks and [`Interceptor::observe`] from Raw Input.
#[derive(Default)]
pub struct Interceptor {
    /// scancode -> remaps that involve it (O(1) lookup for the hot hook path).
    by_scancode: HashMap<u16, Vec<Remap>>,
    /// Answers whether a [`KeyOut::SwallowWhileHeld`] control is down. `None` = never held.
    held: Option<fn(crate::controls::ControlRef) -> bool>,
    /// Keys taken at the hook and not yet released: the release follows the press's decision,
    /// whatever changed in between.
    taken: HashMap<u16, (crate::registry::CanonicalPid, KeyOut)>,
    /// Last Raw-Input press per (mouse namespace?, device), in microseconds.
    activity: HashMap<(bool, crate::registry::CanonicalPid), u64>,
    /// Devices that can only send some keys (a definition's `[[buttons]]`), as physkeys. A
    /// device not listed can send any key.
    keysets: HashMap<crate::registry::CanonicalPid, HashSet<u16>>,
}

impl Interceptor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The core the live layer drives: conditional claims ask the controls held registry.
    fn live() -> Self {
        Self {
            held: Some(|c| crate::controls::control_held(c.page, c.usage, c.pid) == Some(true)),
            ..Self::default()
        }
    }

    /// Set how [`KeyOut::SwallowWhileHeld`] learns whether its control is down.
    pub fn set_held_query(&mut self, held: fn(crate::controls::ControlRef) -> bool) {
        self.held = Some(held);
    }

    /// Rebuild the active remap table. Keys already taken keep the decision they were taken with.
    pub fn set_remaps(&mut self, remaps: impl IntoIterator<Item = Remap>) {
        let mut by_scancode: HashMap<u16, Vec<Remap>> = HashMap::new();
        for r in remaps {
            by_scancode.entry(r.from).or_default().push(r);
        }
        self.by_scancode = by_scancode;
    }

    /// Declare which keys each limited device can send.
    pub fn set_keysets(&mut self, keysets: HashMap<crate::registry::CanonicalPid, HashSet<u16>>) {
        self.keysets = keysets;
    }

    /// A Raw-Input press from `pid`: the key reached the desktop, so `pid` is active.
    pub fn observe(&mut self, physkey: u16, pid: crate::registry::CanonicalPid, now_us: u64) {
        self.activity.insert((physkey & MOUSE_PHYSKEY_BASE != 0, pid), now_us);
    }

    /// The device most likely to have sent `physkey`: among the devices seen in its namespace
    /// that can send it, the one active most recently.
    fn attribute(&self, physkey: u16) -> Option<crate::registry::CanonicalPid> {
        let mouse = physkey & MOUSE_PHYSKEY_BASE != 0;
        self.activity
            .iter()
            .filter(|((ns, pid), _)| {
                *ns == mouse && (mouse || self.keysets.get(pid).is_none_or(|ks| ks.contains(&physkey)))
            })
            .max_by_key(|(_, at)| **at)
            .map(|((_, pid), _)| *pid)
    }

    /// The claim `pid` makes on a key among `remaps`: a conditional swallow whose control is held
    /// wins, else the first unconditional claim.
    fn resolve_claim(&self, remaps: &[Remap], pid: crate::registry::CanonicalPid) -> Option<KeyOut> {
        let mut fallback = None;
        for r in remaps.iter().filter(|r| r.pid == pid) {
            match r.to {
                KeyOut::SwallowWhileHeld(c) => {
                    if self.held.is_some_and(|held| held(c)) {
                        return Some(KeyOut::Swallow);
                    }
                }
                to => {
                    fallback.get_or_insert(to);
                }
            }
        }
        fallback
    }

    /// HOOK edge (device unknown): take it or leave it. A conditional claim whose hold key is
    /// down takes the key as its own device's. Otherwise the key is attributed to a device
    /// ([`Interceptor::observe`] feeds that) and taken when that device claims it. `None`: the key
    /// is not ours and passes untouched.
    pub fn on_edge(&mut self, physkey: u16, down: bool) -> Option<Taken> {
        if !down {
            return self.taken.remove(&physkey).map(|(pid, to)| Taken { pid, to, first: true });
        }
        if let Some(&(pid, to)) = self.taken.get(&physkey) {
            return Some(Taken { pid, to, first: false });
        }
        let remaps = self.by_scancode.get(&physkey)?;
        let shifted = self.held.and_then(|held| {
            remaps.iter().find_map(|r| match r.to {
                KeyOut::SwallowWhileHeld(c) if held(c) => Some(r.pid),
                _ => None,
            })
        });
        let (pid, to) = match shifted {
            Some(pid) => (pid, KeyOut::Swallow),
            None => {
                let pid = self.attribute(physkey)?;
                let to = remaps
                    .iter()
                    .find(|r| r.pid == pid && !matches!(r.to, KeyOut::SwallowWhileHeld(_)))?
                    .to;
                (pid, to)
            }
        };
        self.taken.insert(physkey, (pid, to));
        Some(Taken { pid, to, first: true })
    }

    /// Stand down: release every taken key, as `(physkey, taken)`.
    fn drain_transition(&mut self) -> Vec<(u16, Taken)> {
        self.taken
            .drain()
            .map(|(physkey, (pid, to))| (physkey, Taken { pid, to, first: true }))
            .collect()
    }

    /// True if no remaps are configured (the whole shim is a no-op).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_scancode.is_empty()
    }

    /// Is there a replacement remap ([`KeyOut::Scancode`]) for this (physkey, pid)? Swallow claims
    /// don't count — the dispatcher must still fire their actions (see the module-level `owns`).
    #[must_use]
    pub fn has_key_remap(&self, physkey: u16, pid: crate::registry::CanonicalPid) -> bool {
        self.by_scancode.get(&physkey).is_some_and(|v| {
            v.iter()
                .any(|r| r.pid == pid && matches!(r.to, KeyOut::Scancode(_)))
        })
    }

    /// GRAB-MODEL resolve (e.g. Linux evdev `EVIOCGRAB`): the device is known at read time, so
    /// there is no hook decision to make — look the claim up directly. Returns the target key if
    /// this (physkey, pid) is remapped, else the original (pass-through). `None` = the key is
    /// swallowed — emit nothing.
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
            .and_then(|v| self.resolve_claim(v, pid))
        {
            Some(to) => match to {
                KeyOut::Scancode(sc) => Some(Inject { scancode: sc, down }),
                KeyOut::Swallow | KeyOut::SwallowWhileHeld(_) => None,
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
// The pure [`Interceptor`] above is driven by the LL hooks (decide via [`hook_edge`]) and the
// Raw-Input reader in `controls::listen` (feeds device activity via [`on_raw_keyboard`]). They share
// ONE process-global behind a mutex — the same single-owner model as `hook.rs`'s gaming hook.
// Everything is a zero-cost no-op until [`configure`] arms it.
//
// Our own `SendInput` output carries a `dwExtraInfo` signature so the hook passes it through
// instead of taking it again, and its Raw-Input echo carries no device, so it is not activity.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

/// Signature stamped into `dwExtraInfo` on every keystroke WE inject — the hook recognizes its own
/// replays and passes them through. "nrn\0" — arbitrary but distinctive.
#[cfg(windows)]
const INJECT_SIG: usize = 0x006e_726e;

static CORE: Mutex<Option<Interceptor>> = Mutex::new(None);
struct EdgeState {
    disarming: bool,
    in_flight: usize,
}
/// The hook only holds this while recording an edge. OS injection runs after it is released, so
/// SendInput can call back through the hook without waiting on a lock held by its own caller.
static EDGE_TRANSITION: Mutex<EdgeState> = Mutex::new(EdgeState { disarming: false, in_flight: 0 });
static EDGE_IDLE: Condvar = Condvar::new();
/// State changes may wait for prior emissions, but the hook never takes this mutex.
static STANDDOWN_TRANSITION: Mutex<()> = Mutex::new(());

struct Emission;
impl Drop for Emission {
    fn drop(&mut self) {
        let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.in_flight -= 1;
        EDGE_IDLE.notify_all();
    }
}

fn reserve_emission(state: &mut EdgeState) -> Emission {
    state.in_flight += 1;
    Emission
}
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Temporarily suspend the shim WITHOUT tearing down the hook — set while a press-to-bind capture
/// is in flight, so pressing a control to BIND it isn't swallowed/remapped (the user is binding it,
/// not using it). Mirrors how the live dispatcher skips firing during capture.
static PAUSED: AtomicBool = AtomicBool::new(false);

/// Suspend/resume the shim for the duration of a press-to-bind capture. While paused, keys pass
/// through untouched so a captured control is read cleanly. Keys already taken are released
/// before the pause takes effect. Call `true` at capture start and `false` at its end.
pub fn set_paused(on: bool) {
    let _standdown = STANDDOWN_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pending, _emission) = {
        let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        PAUSED.store(on, Ordering::SeqCst);
        let pending = if on && !state.disarming { take_transition_edges() } else { Vec::new() };
        let emission = (!pending.is_empty()).then(|| reserve_emission(&mut state));
        if emission.is_some() {
            while state.in_flight > 1 {
                state = EDGE_IDLE.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        (pending, emission)
    };
    for (physkey, taken) in pending { release(physkey, taken); }
}

fn take_transition_edges() -> Vec<(u16, Taken)> {
    CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .map(Interceptor::drain_transition)
        .unwrap_or_default()
}

/// Release taken keys while the input gate is still armed, then close it before a new hook edge
/// can enter. Called only by the process-wide safety setter.
pub(crate) fn disarm_input_with(close_gate: impl FnOnce()) {
    let _standdown = STANDDOWN_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let pending = {
        let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        state.disarming = true;
        let pending = take_transition_edges();
        while state.in_flight != 0 {
            state = EDGE_IDLE.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        pending
    };
    for (physkey, taken) in pending { release(physkey, taken); }
    let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    close_gate();
    state.disarming = false;
}

/// Is the shim currently paused for a press-to-bind capture? Read-only witness for the capture
/// teardown paths' tests (a leaked pause = keyboard remaps and the cast-trigger swallow silently
/// dead process-wide until the next capture) and for diagnostics.
pub fn paused() -> bool {
    PAUSED.load(Ordering::SeqCst)
}

/// Is the shim standing down for this edge — either not armed at all, or [`set_paused`] for a
/// press-to-bind capture? The per-edge entry points (`hook_edge`, `owns`)
/// all route through this so "paused" cannot come to mean different things in different places.
fn standing_down() -> bool {
    standing_down_for(ACTIVE.load(Ordering::Relaxed), PAUSED.load(Ordering::Relaxed))
}

/// Is the hook swallowing firmware-assigned private keys right now? When it isn't (a capture is
/// binding, or input is disarmed), a private key reaches Raw Input and is read as its button.
#[must_use]
pub fn takes_private_keys() -> bool {
    !standing_down() && crate::action::input_armed()
}

fn standing_down_for(active: bool, paused: bool) -> bool {
    !active || paused
}

fn requested_hooks_installed(
    want_kbd: bool,
    want_mouse: bool,
    kbd_installed: bool,
    mouse_installed: bool,
) -> bool {
    (!want_kbd || kbd_installed) && (!want_mouse || mouse_installed)
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
    let _standdown = STANDDOWN_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let any = !remaps.is_empty();
    let want_mouse = remaps.iter().any(|r| r.from & MOUSE_PHYSKEY_BASE != 0);
    let want_kbd = remaps.iter().any(|r| r.from & MOUSE_PHYSKEY_BASE == 0);
    let keysets = device_keysets(&crate::registry::shared());
    // A config swap stands down the old remaps and releases their held outputs before the new
    // rules can own an edge. No key already withheld from the desktop is discarded.
    let (pending, _emission) = {
        let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ACTIVE.store(false, Ordering::SeqCst);
        let pending = if !state.disarming { take_transition_edges() } else { Vec::new() };
        let emission = (!pending.is_empty()).then(|| reserve_emission(&mut state));
        if emission.is_some() {
            while state.in_flight > 1 {
                state = EDGE_IDLE.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        let mut g = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let core = g.get_or_insert_with(Interceptor::live);
        core.set_remaps(remaps);
        core.set_keysets(keysets);
        (pending, emission)
    };
    for (physkey, taken) in pending { release(physkey, taken); }
    drop(_emission);
    if any {
        let kbd_installed = if want_kbd {
            sys::install()
        } else {
            sys::uninstall();
            true
        };
        let mouse_installed = if want_mouse {
            sys::install_mouse()
        } else {
            sys::uninstall_mouse();
            true
        };
        ACTIVE.store(
            requested_hooks_installed(want_kbd, want_mouse, kbd_installed, mouse_installed),
            Ordering::SeqCst,
        );
    } else {
        sys::uninstall();
        sys::uninstall_mouse();
    }
}

/// Build the shim's CLAIM on one engine rule, if it can express one: a base-layer, pid-scoped
/// `Input` trigger on a keyboard page or a mouse middle/X button.
///
///   * keyboard key + plain `Key` action → a REPLACEMENT remap ([`KeyOut::Scancode`]); the shim
///     emits the target and the dispatcher SKIPS the rule ([`owns`]) — no double-send;
///   * anything else the shim can hook (keyboard key with a non-Key action, any claimable mouse
///     button) → a SWALLOW claim: the original emission is suppressed, and the hook delivers the
///     edge as the device's so the dispatcher still fires the action.
///
/// Hold-layer rules and device-any binds are never claimed here (shifted binds get conditional
/// claims from `layer_claims`) (a device-any swallow would eat the
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
    //     delivers it as the device's edge, so host dispatch fires the action.
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
/// key to weave stops typing `1111…` into the focused app. The feature reads the hold from the
/// held registry, which the hook's delivery feeds. Device-any or
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
        // A button the firmware plan manages no longer emits its stock key, so a claim on that key
        // would only swallow (and delay) the same key typed on every other keyboard.
        .filter(|r| !firmware_managed(r))
        .filter_map(claim_for_rule)
        .filter(|r| claim.is_none_or(|c| (r.pid, r.from) != (c.pid, c.from)))
        .collect();
    remaps.extend(claim);
    remaps.extend(layer_claims(engine).filter(|r| claim.is_none_or(|c| (r.pid, r.from) != (c.pid, c.from))));
    // A host-performed button emits a private key only its device can send: swallowing it is exact.
    remaps.extend(crate::buttons::private_keys().into_iter().filter_map(|(pid, usage)| {
        Some(Remap { pid, from: sys::physkey_for_usage(u16::from(usage))?, to: KeyOut::Swallow })
    }));
    remaps
}

/// Claims for the binds a hold key shifts: each swallows its key on its device only while one of
/// its layer's hold keys is down, so the unshifted key still types. Layers without a hold key
/// (every rule activates the layer itself) and plate layers claim nothing.
fn layer_claims(engine: &crate::engine::Engine) -> impl Iterator<Item = Remap> + '_ {
    use crate::engine::Trigger;
    engine
        .layers
        .iter()
        .filter(|(name, _)| !crate::engine::is_plate_layer(name))
        .flat_map(|(_, rules)| {
            let holds: Vec<crate::controls::ControlRef> = rules
                .iter()
                .filter(|r| crate::engine::is_hold_key(r))
                .filter_map(|r| match r.trigger {
                    Trigger::Input { page, usage, pid } => Some(crate::controls::ControlRef { page, usage, pid }),
                    _ => None,
                })
                .collect();
            rules
                .iter()
                .filter(|r| !crate::engine::is_hold_key(r) && !firmware_managed(r))
                .filter_map(|r| claim_for_rule(&crate::engine::Rule::new(r.trigger.clone(), crate::action::Action::Noop)))
                .flat_map(move |base| {
                    holds.clone().into_iter().map(move |c| Remap { to: KeyOut::SwallowWhileHeld(c), ..base })
                })
                .collect::<Vec<_>>()
        })
}

/// The keys each device with a declared button table can send, as physkeys: its buttons' stock
/// keys. A device without one may send any key.
fn device_keysets(reg: &crate::registry::Registry) -> HashMap<crate::registry::CanonicalPid, HashSet<u16>> {
    let mut out: HashMap<crate::registry::CanonicalPid, HashSet<u16>> = HashMap::new();
    for def in reg.devices.iter().filter(|d| !d.buttons.is_empty()) {
        let keys: HashSet<u16> = def.buttons.iter().filter_map(|b| sys::physkey_for_usage(u16::from(b.stock_usage))).collect();
        for pid in def.owned_event_pids() {
            out.entry(crate::registry::CanonicalPid::of(pid)).or_default().extend(keys.iter().copied());
        }
    }
    out
}

fn firmware_managed(rule: &crate::engine::Rule) -> bool {
    matches!(
        rule.trigger,
        crate::engine::Trigger::Input { page: 0x07, usage, pid: Some(pid) } if crate::buttons::covers(pid, usage)
    )
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
    if standing_down() || !crate::action::input_armed() {
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
    let _standdown = STANDDOWN_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (pending, _emission) = {
        let mut state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        ACTIVE.store(false, Ordering::SeqCst);
        let pending = if !state.disarming { take_transition_edges() } else { Vec::new() };
        let emission = (!pending.is_empty()).then(|| reserve_emission(&mut state));
        if emission.is_some() {
            while state.in_flight > 1 {
                state = EDGE_IDLE.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        }
        if let Ok(mut g) = CORE.lock() {
            if let Some(c) = g.as_mut() {
                c.set_remaps([]);
            }
        }
        (pending, emission)
    };
    for (physkey, taken) in pending { release(physkey, taken); }
    drop(_emission);
    sys::uninstall();
    sys::uninstall_mouse();
}

/// The HOOK's per-edge decision: should this keystroke be swallowed? Called from the LL hook proc
/// with the physkey (`scancode | 0x100 if extended`). A taken key is delivered or replaced here.
/// Gated on the input-arm kill-switch — in safe mode we swallow nothing (pure pass-through).
#[cfg_attr(not(windows), allow(dead_code))]
fn hook_edge(physkey: u16, down: bool, injected: bool) -> bool {
    let state = EDGE_TRANSITION.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // A failed firmware-button restore can leave one of neuron's private F13..F24 functions
    // active on the device. Suppress that small pool until every button verifies stock, even
    // while ordinary input synthesis is disarmed; otherwise the artifact reaches the desktop.
    if !injected
        && sys::usage_for_physkey(physkey).is_some_and(crate::buttons::private_pool_quarantined)
    {
        return true;
    }
    // `standing_down` covers PAUSED too: swallowing a key mid-capture is exactly the bug the pause
    // exists to prevent (the user is BINDING this control, not using it).
    if state.disarming || standing_down() || !crate::action::input_armed() {
        return false;
    }
    // A firmware-assigned private key names its device by itself: resolved here, no Raw-Input
    // correlation (a swallowed keystroke never reaches Raw Input anyway). A software-injected one
    // (a hotkey tool sending the same F-key) is never ours.
    if !injected && sys::usage_for_physkey(physkey).is_some_and(|u| crate::buttons::deliver_private(u, down)) {
        return true;
    }
    // Another program's synthetic key has no device to attribute; it is never ours to take.
    if injected {
        return false;
    }
    let mut state = state;
    let taken = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut().and_then(|c| c.on_edge(physkey, down));
    let Some(taken) = taken else { return false };
    let emission = reserve_emission(&mut state);
    drop(state);
    match taken.to {
        KeyOut::Scancode(scancode) => emit_later(Inject { scancode, down }, emission),
        KeyOut::Swallow | KeyOut::SwallowWhileHeld(_) => {
            if taken.first {
                deliver_taken(taken.pid, physkey, down);
            }
        }
    }
    true
}

/// Release a key taken at the hook when the shim stands down.
fn release(physkey: u16, taken: Taken) {
    match taken.to {
        KeyOut::Scancode(scancode) => emit(Inject { scancode, down: false }),
        KeyOut::Swallow | KeyOut::SwallowWhileHeld(_) => deliver_taken(taken.pid, physkey, false),
    }
}

/// Injects from the hook go out on their own thread: `SendInput` runs the hooks again, and the hook
/// thread must not wait on itself.
fn emit_later(inject: Inject, emission: Emission) {
    static TX: OnceLock<Mutex<std::sync::mpsc::Sender<(Inject, Emission)>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(Inject, Emission)>();
        crate::worker::spawn_detached("neuron-remap-emit", move || {
            for (inject, emission) in rx {
                emit(inject);
                drop(emission);
            }
        });
        Mutex::new(tx)
    });
    // No emitter (it failed to start or died): emit here rather than lose the key.
    if let Err(std::sync::mpsc::SendError((inject, emission))) =
        tx.lock().unwrap_or_else(std::sync::PoisonError::into_inner).send((inject, emission))
    {
        emit(inject);
        drop(emission);
    }
}

/// One device's keys the hook took, as `(page, usage)`.
type TakenSet = (crate::registry::CanonicalPid, Vec<(u16, u16)>);
/// Keys the hook took, per device: the held set each delivery reports.
static TAKEN_HELD: Mutex<Vec<TakenSet>> = Mutex::new(Vec::new());

/// Deliver a key the hook took as an edge of `pid`, so its bind fires: the hook's swallow hid it
/// from Raw Input.
fn deliver_taken(pid: crate::registry::CanonicalPid, physkey: u16, down: bool) {
    let key = if physkey & MOUSE_PHYSKEY_BASE != 0 {
        (0x09, physkey & !MOUSE_PHYSKEY_BASE)
    } else if let Some(usage) = sys::usage_for_physkey(physkey) {
        (0x07, usage)
    } else {
        (0xFF07, physkey)
    };
    let hits = {
        let mut held = TAKEN_HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = held.iter().position(|(p, _)| *p == pid).unwrap_or_else(|| {
            held.push((pid, Vec::new()));
            held.len() - 1
        });
        let set = &mut held[at].1;
        if down {
            if !set.contains(&key) {
                set.push(key);
            }
        } else {
            set.retain(|k| *k != key);
        }
        set.clone()
    };
    crate::controls::inject_event(crate::controls::ControlEvent {
        pid: Some(pid),
        stream: crate::controls::Stream::Hooked,
        hits,
        raw: Vec::new(),
    });
}

/// The RAW-INPUT reader's per-edge call (from `controls::listen`): a key that reached the desktop
/// marks its device active, which is what [`hook_edge`] attributes the next claimed key by. `pid`
/// is the RAW pid off the wire; 0 is a synthetic event with no device.
pub fn on_raw_keyboard(physkey: u16, down: bool, pid: u16) {
    if !down || pid == 0 {
        return;
    }
    let pid = crate::registry::CanonicalPid::of(pid);
    if let Some(c) = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_mut() {
        c.observe(physkey, pid, now_us());
    }
}

/// The RAW-INPUT reader's per-edge call for MOUSE buttons (3=middle, 4=X1, 5=X2): the mouse-hook
/// twin of [`on_raw_keyboard`], in the mouse physkey namespace.
pub fn on_raw_mouse(button: u16, down: bool, pid: u16) {
    on_raw_keyboard(mouse_physkey(button), down, pid);
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
        INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
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
    const LLKHF_INJECTED: u32 = 0x10;
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

    /// Platform key-id (`scancode | 0x100 if E0`) → HID keyboard usage.
    pub fn usage_for_physkey(physkey: u16) -> Option<u16> {
        crate::controls::win::scancode_to_usage(physkey & 0xFF, physkey & 0x100 != 0)
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
        crate::action::send_win_input(std::slice::from_ref(&input));
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
                    if hook_edge(physkey, down, kb.flags & LLKHF_INJECTED != 0) {
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
                    if hook_edge(super::mouse_physkey(button), down, false) {
                        return 1; // swallow: the click never reaches the app underneath.
                    }
                }
            }
        }
        CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
    }

    /// One hook pump's shared body — both hooks (keyboard, mouse) run this with their own slot
    /// statics and proc. This callback sits in the OS input path, so being scheduled late delays
    /// every event on the machine.
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
    ) -> bool {
        let mut pump = pump.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = pump.take() {
            if installed.load(Ordering::SeqCst) {
                *pump = Some(existing);
                return true; // already pumping
            }
            let _ = existing.join.join();
            pump_tid.store(0, Ordering::SeqCst);
        }
        pump_tid.store(0, Ordering::SeqCst);
        let Ok(join) = crate::worker::spawn_named(name, body) else {
            return false;
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
            return false;
        }
        *pump = Some(Pump { join, tid });
        true
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

    pub fn install() -> bool {
        install_for("neuron-remap-hook", &PUMP, &PUMP_TID, &INSTALLED, || {
            pump_main_for(WH_KEYBOARD_LL, proc, &INSTALLED, &HANDLE, &PUMP_TID);
        })
    }

    pub fn uninstall() {
        uninstall_for(&PUMP, &PUMP_TID);
    }

    pub fn install_mouse() -> bool {
        install_for("neuron-remap-mhook", &M_PUMP, &M_PUMP_TID, &M_INSTALLED, || {
            pump_main_for(WH_MOUSE_LL, mouse_proc, &M_INSTALLED, &M_HANDLE, &M_PUMP_TID);
        })
    }

    pub fn uninstall_mouse() {
        uninstall_for(&M_PUMP, &M_PUMP_TID);
    }

    /// `SendInput` one mouse-button event (3=middle, 4=X1, 5=X2), stamped with our signature so the
    /// mouse hook passes it. Gated on the arm kill-switch like every injection.
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
        crate::action::send_win_input(std::slice::from_ref(&input));
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
    //! in ONE step, so no attribution is needed. `install` spawns a reader thread that, per grabbed
    //! event, calls [`super::Interceptor::resolve_direct`] (device known) and injects the result
    //! via `uinput`. `physkey_for_usage`: HID usage → evdev keycode.
    //!
    //! **macOS (CGEventTap + IOHIDManager).** A keyboard `CGEventTapCreate` tap can suppress and
    //! reinject in place; taps don't carry the device, so feed `IOHIDManager` activity to
    //! [`super::Interceptor::observe`] and decide in the tap with [`super::Interceptor::on_edge`],
    //! as Windows does with Raw Input. `physkey_for_usage`: HID usage → the platform virtual keycode.

    /// No key-id map yet → [`super::claim_for_rule`] builds nothing, so the shim stays inert here.
    pub fn physkey_for_usage(_usage: u16) -> Option<u16> {
        None
    }
    pub fn usage_for_physkey(_physkey: u16) -> Option<u16> {
        None
    }
    pub fn inject(_physkey: u16, _down: bool) {}
    pub fn inject_mouse(_button: u16, _down: bool) {}
    pub fn install() -> bool { false }
    pub fn uninstall() {}
    pub fn install_mouse() -> bool { false }
    pub fn uninstall_mouse() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    static LIVE_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

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

    thread_local! {
        static FN_DOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    fn fn_ref() -> crate::controls::ControlRef {
        crate::controls::ControlRef { page: crate::controls::RAZER_MACRO_PAGE, usage: 0x01, pid: Some(kbd()) }
    }
    fn fn_core(extra: &[Remap]) -> Interceptor {
        let mut i = Interceptor::new();
        i.set_held_query(|c| c == fn_ref() && FN_DOWN.with(std::cell::Cell::get));
        let shifted = Remap { pid: kbd(), from: 0x3C, to: KeyOut::SwallowWhileHeld(fn_ref()) };
        i.set_remaps(std::iter::once(shifted).chain(extra.iter().copied()));
        i
    }

    fn naga_keys() -> HashMap<crate::registry::CanonicalPid, HashSet<u16>> {
        HashMap::from([(naga(), HashSet::from([0x02, 0x0D]))])
    }

    #[test]
    fn a_shifted_key_is_taken_only_while_its_hold_key_is_down() {
        FN_DOWN.with(|f| f.set(false));
        let mut i = fn_core(&[]);
        assert_eq!(i.on_edge(0x3C, true), None, "plain F2 passes untouched");
        assert_eq!(i.on_edge(0x3C, false), None);

        FN_DOWN.with(|f| f.set(true));
        assert_eq!(i.on_edge(0x3C, true), Some(Taken { pid: kbd(), to: KeyOut::Swallow, first: true }));
        assert_eq!(i.on_edge(0x3C, true), Some(Taken { pid: kbd(), to: KeyOut::Swallow, first: false }), "auto-repeat");
        // Fn lets go first: the taken press still owns its release.
        FN_DOWN.with(|f| f.set(false));
        assert_eq!(i.on_edge(0x3C, false), Some(Taken { pid: kbd(), to: KeyOut::Swallow, first: true }));
        assert_eq!(i.on_edge(0x3C, false), None, "one release per press");
    }

    #[test]
    fn a_core_with_no_held_query_never_shifts() {
        let mut i = Interceptor::new();
        i.set_remaps([Remap { pid: kbd(), from: 0x3C, to: KeyOut::SwallowWhileHeld(fn_ref()) }]);
        i.observe(0x1E, kbd(), 0);
        assert_eq!(i.on_edge(0x3C, true), None);
    }

    #[test]
    fn a_key_only_one_device_can_send_is_that_devices() {
        let mut i = Interceptor::new();
        i.set_keysets(naga_keys());
        i.set_remaps([Remap { pid: kbd(), from: 0x3F, to: KeyOut::Swallow }]);
        i.observe(0x1E, kbd(), 10);
        i.observe(0x02, naga(), 20);
        // F5 last came... nowhere, but the Naga can only send its grid keys: the keyboard sent it.
        assert_eq!(i.on_edge(0x3F, true), Some(Taken { pid: kbd(), to: KeyOut::Swallow, first: true }));
        assert_eq!(i.on_edge(0x3F, false), Some(Taken { pid: kbd(), to: KeyOut::Swallow, first: true }));
    }

    #[test]
    fn a_shared_key_goes_to_the_device_active_last() {
        let mut i = Interceptor::new();
        i.set_keysets(naga_keys());
        i.set_remaps([eq_to_g()]);
        i.observe(0x1E, kbd(), 10);
        i.observe(0x02, naga(), 20);
        assert_eq!(i.on_edge(0x0D, true), Some(Taken { pid: naga(), to: KeyOut::Scancode(0x22), first: true }));
        assert_eq!(i.on_edge(0x0D, false), Some(Taken { pid: naga(), to: KeyOut::Scancode(0x22), first: true }));

        i.observe(0x1E, kbd(), 30);
        assert_eq!(i.on_edge(0x0D, true), None, "the keyboard's '=' is not claimed and passes untouched");
        assert_eq!(i.on_edge(0x0D, false), None);
    }

    #[test]
    fn nothing_is_taken_before_any_device_is_known() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        assert_eq!(i.on_edge(0x0D, true), None);
        assert_eq!(i.on_edge(0x1E, true), None, "an unclaimed key is never taken");
    }

    #[test]
    fn mouse_buttons_attribute_among_mice() {
        let mut i = Interceptor::new();
        i.set_remaps([Remap { pid: naga(), from: mouse_physkey(4), to: KeyOut::Swallow }]);
        i.observe(0x1E, kbd(), 50);
        i.observe(mouse_physkey(1), naga(), 10);
        assert_eq!(
            i.on_edge(mouse_physkey(4), true),
            Some(Taken { pid: naga(), to: KeyOut::Swallow, first: true }),
            "keyboard activity says nothing about which mouse clicked"
        );
    }

    #[test]
    fn a_reconfigured_key_releases_with_the_decision_it_was_pressed_with() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g()]);
        i.observe(0x02, naga(), 0);
        assert_eq!(i.on_edge(0x0D, true), Some(Taken { pid: naga(), to: KeyOut::Scancode(0x22), first: true }));
        i.set_remaps([Remap { pid: naga(), from: 0x0D, to: KeyOut::Scancode(0x30) }]);
        assert_eq!(i.on_edge(0x0D, false), Some(Taken { pid: naga(), to: KeyOut::Scancode(0x22), first: true }));
        assert_eq!(i.on_edge(0x0D, true), Some(Taken { pid: naga(), to: KeyOut::Scancode(0x30), first: true }));
    }

    #[test]
    fn standing_down_releases_every_taken_key_once() {
        let mut i = Interceptor::new();
        i.set_remaps([eq_to_g(), Remap { pid: naga(), from: 0x02, to: KeyOut::Swallow }]);
        i.observe(0x02, naga(), 0);
        i.on_edge(0x0D, true);
        i.on_edge(0x02, true);
        let mut released = i.drain_transition();
        released.sort_by_key(|(physkey, _)| *physkey);
        assert_eq!(released, vec![
            (0x02, Taken { pid: naga(), to: KeyOut::Swallow, first: true }),
            (0x0D, Taken { pid: naga(), to: KeyOut::Scancode(0x22), first: true }),
        ]);
        assert!(i.drain_transition().is_empty());
        assert_eq!(i.on_edge(0x0D, false), None, "the physical release after a stand-down passes");
    }

    #[test]
    fn resolve_direct_resolves_the_shift_itself() {
        FN_DOWN.with(|f| f.set(false));
        let base = Remap { pid: kbd(), from: 0x3C, to: KeyOut::Scancode(0x22) };
        let i = fn_core(&[base]);
        assert_eq!(i.resolve_direct(0x3C, true, kbd()), Some(Inject { scancode: 0x22, down: true }));
        FN_DOWN.with(|f| f.set(true));
        assert_eq!(i.resolve_direct(0x3C, true, kbd()), None);
        assert_eq!(i.resolve_direct(0x3C, true, naga()), Some(Inject { scancode: 0x3C, down: true }));
    }

    /// `set_paused` must actually stand the shim down.
    ///
    /// Every test above drives a local [`Interceptor`] directly, which bypasses the module-level
    /// gates entirely — so `PAUSED` was stored by [`set_paused`] and then read by NOTHING, and a
    /// press-to-bind capture was still swallowed and remapped by the shim it was supposed to
    /// suspend. This exercises the gate itself so the flag can never go dead again.
    #[test]
    fn pausing_stands_the_shim_down() {
        let _serial = LIVE_STATE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
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
            !hook_edge(0x0D, true, false),
            "a paused shim must never swallow the key being bound"
        );

        // A paused shim is transparent even while armed, so it must claim ownership of nothing.
        assert!(!owns(0x07, 0x2E, NAGA_RAW), "a paused shim owns no trigger");

        set_paused(false);
        ACTIVE.store(was_active, Ordering::SeqCst);
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
        // An unremapped key -> unchanged.
        assert_eq!(
            i.resolve_direct(0x1E, false, naga()),
            Some(Inject { scancode: 0x1E, down: false })
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_layer_with_a_hold_key_claims_its_shifted_keys_conditionally() {
        use crate::action::{Action, MediaKind};
        use crate::engine::{Engine, Rule, Trigger};
        let fn_key = Trigger::Input { page: crate::controls::RAZER_MACRO_PAGE, usage: 0x01, pid: Some(kbd()) };
        let f2 = Trigger::Input { page: 0x07, usage: 0x3B, pid: Some(kbd()) };
        let f3 = Trigger::Input { page: 0x07, usage: 0x3C, pid: Some(kbd()) };
        let engine = Engine::from_rules(vec![
            Rule::on_layer("fn", fn_key, Action::Noop),
            Rule::on_layer("fn", f2, Action::Media { key: MediaKind::VolumeDown }),
            // a layer with no hold key: its rule holds the layer itself, so nothing is shifted
            Rule::on_layer("sniper", f3, Action::DpiSet { dpi: 400 }),
        ]);
        let remaps = compose_remaps(&engine, None);
        let f2_key = sys::physkey_for_usage(0x3B).unwrap();
        assert_eq!(
            remaps,
            vec![Remap { pid: kbd(), from: f2_key, to: KeyOut::SwallowWhileHeld(fn_ref()) }],
        );
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
    }

    #[test]
    fn failed_requested_hook_install_never_activates_dispatch_ownership() {
        // Keyboard install failed, even though an unrequested mouse hook is trivially satisfied.
        let active = requested_hooks_installed(true, false, false, true);
        assert!(!active);
        assert!(standing_down_for(active, false));
        // If only the mouse install fails, the shared dispatcher also stands down: a partial hook
        // set must never make `owns` suppress a host action for a key the OS hook cannot replace.
        let active = requested_hooks_installed(true, true, true, false);
        assert!(!active);
        assert!(standing_down_for(active, false));
        assert!(requested_hooks_installed(true, false, true, false));
        assert!(requested_hooks_installed(false, true, false, true));
    }

    #[test]
    fn owns_stands_down_when_process_input_is_disarmed() {
        let _serial = LIVE_STATE_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let was_active = ACTIVE.load(Ordering::SeqCst);
        let was_paused = PAUSED.load(Ordering::SeqCst);
        let previous = {
            let mut core = CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let previous = core.take();
            let mut test_core = Interceptor::new();
            test_core.set_remaps([Remap { pid: naga(), from: 0x2E, to: KeyOut::Scancode(0x22) }]);
            *core = Some(test_core);
            previous
        };
        set_paused(false);

        let active = requested_hooks_installed(true, false, true, true);
        ACTIVE.store(active, Ordering::SeqCst);
        assert!(CORE.lock().unwrap().as_ref().unwrap().has_key_remap(0x2E, naga()));
        assert!(!owns(0xFF07, 0x2E, NAGA_RAW), "the process gate overrides a live hook claim");

        let active = requested_hooks_installed(true, false, false, true);
        ACTIVE.store(active, Ordering::SeqCst);
        assert!(!owns(0xFF07, 0x2E, NAGA_RAW));

        *CORE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = previous;
        ACTIVE.store(was_active, Ordering::SeqCst);
        PAUSED.store(was_paused, Ordering::SeqCst);
    }
}
