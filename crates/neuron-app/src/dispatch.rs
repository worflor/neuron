// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! LIVE DISPATCH — the headline. A device-event runtime on a dedicated worker thread, so GUI-bound
//! remaps fire LIVE without the CLI daemon.
//!
//! This is the GUI's port of the CLI daemon's `run_listen`/`fire_trigger` spine (see
//! `neuron-cli/src/main.rs`). On a worker thread it:
//!   * ARMS input synthesis on the live path (gated behind the GUI's safe-mode toggle), so bound
//!     keys/clicks/macros actually fire — remaps work out of the box;
//!   * builds the ONE unified [`Engine`] from every on-disk config (bindings/cast/hypershift/
//!     app-rules) via [`neuron::controls::build_runtime`];
//!   * activates the GamingMode `WH_KEYBOARD_LL` suppression hook (which self-hosts a dedicated
//!     message-pump thread in `neuron::hook`, so it is immune to this loop's blocking device I/O —
//!     an LL hook Windows silently bypasses if its installing thread misses the ~300 ms timeout);
//!   * on each Raw-Input event translates to a [`Trigger`] and dispatches through the Engine —
//!     HyperShift hold edges via held-layer state, daemon [`Intent`]s routed (DPI / scroll /
//!     profile), Turbo repeated while held, the mic-tap + app-focus polled on the tick;
//!   * posts live status (last trigger fired, active layer, focused app) back to the UI thread via
//!     [`slint::invoke_from_event_loop`].
//!
//! # Input-safety invariant (non-negotiable)
//! The live loop is started ONLY from the real `main` run path (see [`LiveRuntime::start`]). It is
//! NEVER constructed by the UI tests (which build `State` + glue but never call `start`), and it is
//! the GUI's safe-mode toggle — not this module — that decides whether [`neuron::action::arm_input`]
//! is flipped on. When the GUI is in safe-mode (disarmed via `--safe` or Settings), the loop still runs and
//! resolves every trigger, but SendInput / process-spawn stay no-ops (the core arm gate). So tests
//! never arm and never start this runtime.

use crate::ui::{AppWindow, State};
use neuron::controls::{self, ControlEvent, HoldEdges, InputEdge, MIC_TAP};
use neuron::engine::Trigger;
use neuron::executor::{DispatchExecutor, DispatchOutcome, IntentRunner, TurboRuntime};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

enum LiveCommand {
    Reload,
    Inject(Trigger),
    ToggleHyperShift(Sender<bool>),
    ReconcileGamingHook,
    ApplyProfile {
        name: String,
        persist: bool,
        reply: Sender<Result<ProfileApplyResult, String>>,
    },
}

static NEXT_LIVE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static LIVE_TX: Mutex<Option<(u64, Sender<LiveCommand>)>> = Mutex::new(None);

fn send_live(cmd: LiveCommand) -> bool {
    let tx = LIVE_TX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|(_, tx)| tx.clone());
    let sent = tx.is_some_and(|tx| tx.send(cmd).is_ok());
    if sent {
        // Every LiveCommand (Reload/Inject/ToggleHyperShift/ApplyProfile/ReconcileGamingHook)
        // funnels through this one function, so signaling the pump wake event here (see
        // `neuron::controls::wake_pump`) covers all of them in one place: with the blocking-wait
        // pump active, this wakes it immediately so the command is serviced on the next tick instead
        // of waiting out the cadence. (Under `NEURON_PUMP=poll` the fixed sleep ignores it, harmless.)
        neuron::controls::wake_pump();
    }
    sent
}

/// Monotonic config generation, bumped with every [`request_reload`]. Long-lived watchers that
/// CAN'T rebuild mid-wait (the weave watcher blocks inside a capture) snapshot this before
/// waiting and abort their wait when it moves — so a re-bound cast trigger or edited rhythm
/// takes effect on the very next weave, not after one stale capture.
static RELOAD_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Ask the live worker to rebuild its Engine from on-disk config on its next tick (~5 ms).
/// Callable from any thread; a config write landing mid-rebuild simply re-sets the flag.
pub fn request_reload() {
    RELOAD_GEN.fetch_add(1, Ordering::SeqCst);
    let _ = send_live(LiveCommand::Reload);
}

/// The current config generation (see [`request_reload`]).
pub fn reload_generation() -> u64 {
    RELOAD_GEN.load(Ordering::SeqCst)
}

/// Triggers INJECTED by other live subsystems — today the weave watcher (a resolved radial flick
/// or glyph from the beacon/weave thread). Drained by the worker's tick (~5 ms), so an injected
/// trigger dispatches through the ONE Engine exactly like a hardware event: HyperShift layers,
/// intents, turbo, the SAFE-mode gate and the live readout all compose identically.
/// Queue a trigger for the live worker's next tick. Callable from any thread; a no-op burden if
/// the worker isn't running (the queue is drained only by it, and bounded by real user gestures).
pub fn inject_trigger(t: Trigger) {
    crate::flight::trace("cast", "trigger injected", 0);
    if !send_live(LiveCommand::Inject(t)) {
        crate::flight::trace("cast", "trigger dropped (live worker absent)", 0);
    }
}

/// What [`Action::Echo`] would replay RIGHT NOW, as a short description — so the echo wedge can
/// say "echo → press [f]" instead of being a black box. `None` until something echoable has fired.
static LAST_ACTION_DESC: Mutex<Option<String>> = Mutex::new(None);

pub fn last_action_desc() -> Option<String> {
    LAST_ACTION_DESC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Live mirror of "is the HyperShift layer currently held?" — updated every status tick from the
/// engine's real held-layer set (source of truth). Read by the cast overlay (`beacon`) to swap the
/// radial to its HyperShift set; an atomic so it crosses to the overlay loop without a lock.
static HYPERSHIFT_HELD: AtomicBool = AtomicBool::new(false);

/// Live mirrors of the two hold edges the lighting engine's `modeheld` DATA layer renders: is ANY
/// hold layer engaged, is a sniper hold live. Written where each edge actually happens (the status
/// tick / the sniper press+release), then pushed together via [`push_hold_state`] — and pushed as
/// the default when the live loop stops, so a painted mode light can never outlive the mode.
static LAYER_HELD: AtomicBool = AtomicBool::new(false);
static SNIPER_HELD: AtomicBool = AtomicBool::new(false);

/// Push the current hold state into the lighting engine's feed (one lock + copy; cheap enough for
/// the status tick).
fn push_hold_state() {
    neuron::lighting::publish_hold(neuron::lighting::HoldState {
        layer: LAYER_HELD.load(Ordering::Relaxed),
        sniper: SNIPER_HELD.load(Ordering::Relaxed),
    });
}

/// Flip the software HyperShift latch, returning the NEW state. The SHIFT pill lights from the
/// engine's real held-layers via the status post — one source of truth, no UI-side write.
pub fn toggle_hypershift_latch() -> bool {
    let (tx, rx) = channel();
    if !send_live(LiveCommand::ToggleHyperShift(tx)) {
        return false;
    }
    rx.recv_timeout(std::time::Duration::from_millis(80))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub struct ProfileApplyResult {
    pub name: String,
    pub summary: String,
    pub policy: neuron::writes::GamingMode,
}

/// Apply a profile through the live worker's device session and return its report.
///
/// This is the GUI/manual profile path. It keeps hardware writes on the same runtime that owns
/// live profile-switch/profile-cycle intents instead of doing profile writes from the Slint thread.
pub fn apply_profile(name: String, persist: bool) -> Result<ProfileApplyResult, String> {
    let (reply, rx) = channel();
    if !send_live(LiveCommand::ApplyProfile {
        name: name.clone(),
        persist,
        reply,
    }) {
        return Err("live profile apply unavailable".into());
    }
    // A profile apply writes several device settings, each a verify-gated HID round-trip. On a WIRELESS
    // board (dongle latency + a possibly-asleep device) those add up well past a few seconds — an 8s
    // deadline spuriously "timed out" a save that was merely slow, and the GUI then reverted lighting to
    // the pre-apply look. 20s lets a slow apply actually land (it still returns the instant it finishes).
    rx.recv_timeout(Duration::from_secs(20))
        .map_err(|_| format!("profile '{name}' apply timed out"))?
}

/// Whether a HyperShift layer is held right now (the radial swaps to its HyperShift set on this).
pub fn hypershift_held() -> bool {
    HYPERSHIFT_HELD.load(Ordering::Relaxed)
}

/// A snapshot of live-dispatch status pushed to the UI thread on each interesting event.
#[derive(Clone, Default)]
pub struct LiveStatus {
    /// Human description of the last trigger that fired (e.g. "Mouse 5 (thumb 2)" / "sector 2").
    pub last_trigger: String,
    /// The result line of the last dispatched action.
    pub last_action: String,
    /// Currently-held HyperShift layer names, joined (empty = base layer only).
    pub held_layers: String,
    /// The current foreground app exe (app-aware switch input).
    pub focused_app: String,
    /// Total triggers dispatched since the loop started (a live activity counter).
    pub fired: u64,
    /// The process-wide active-profile cursor at the last dispatch (live ProfileSwitch/Cycle
    /// intents update it; the UI mirrors it so a bound-button switch shows in the header).
    pub active_profile: String,
}

/// The resident live-dispatch runtime. Owns the worker thread + its stop flag. Dropping it (or
/// calling [`stop`](LiveRuntime::stop)) tears the worker down cleanly and disarms input.
pub struct LiveRuntime {
    id: u64,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Whether this runtime armed real input (so stop can disarm symmetrically).
    armed: bool,
}

impl LiveRuntime {
    /// Start the live device-event loop on a worker thread. `armed` arms real input synthesis
    /// (the GUI passes its safe-mode toggle here); when `false` the loop runs in observe/dry-run
    /// mode (the core arm gate keeps SendInput a no-op). `weak` lets the worker post status back.
    ///
    /// LIVE-PATH ONLY: call this from `main`, never from a test or from glue::install.
    pub fn start(weak: slint::Weak<AppWindow>, armed: bool) -> Self {
        let id = NEXT_LIVE_ID.fetch_add(1, Ordering::SeqCst);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (tx, rx) = channel();
        *LIVE_TX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((id, tx));
        neuron::action::arm_input(armed);
        // LiveRuntime owns this handle and joins it in stop(), so it routes through the
        // handle-returning primitive.
        let handle =
            crate::worker::spawn_named("neuron-live-dispatch", move || run_worker(weak, worker_stop, rx))
                .ok();
        if handle.is_none() {
            *LIVE_TX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
        LiveRuntime {
            id,
            stop,
            handle,
            armed,
        }
    }

    /// Is the worker thread still alive?
    pub fn running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Signal the worker to stop and join it. Disarms input if this runtime armed it. Idempotent.
    pub fn stop(&mut self) {
        {
            let mut live = LIVE_TX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if live.as_ref().is_some_and(|(id, _)| *id == self.id) {
                *live = None;
            }
        }
        self.stop.store(true, Ordering::SeqCst);
        // The pump may now be blocked in the wait (up to ~1s idle) instead of busy-polling —
        // wake it so it re-checks `stop` at the top of its loop and exits immediately, keeping
        // this join fast instead of stalling out the rest of a stale interval.
        neuron::controls::wake_pump();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if self.armed {
            neuron::action::arm_input(false);
        }
    }
}

impl Drop for LiveRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Every borrow the live worker's two `listen_until` closures need, gathered into one struct so the
/// closure BODIES can live in named functions ([`live_edge`], [`live_tick`]) a test can call
/// directly — one edge / one tick at a time — instead of only ever running inside [`run_worker`]'s
/// immortal loop.
///
/// `run_worker` wraps the whole struct in ONE `RefCell` and hands both closures a shared reference
/// to it; each closure opens its own `borrow_mut()` for the span of its own call. `listen_until`
/// invokes `on_event` and `on_tick` strictly sequentially (never re-entrantly — see its own doc), so
/// the two `borrow_mut()`s never overlap at runtime; this is the same trick the pre-refactor code
/// already used with a `RefCell` per field (`rt`/`exec`/`devices`/`edges`/`turbos`/…), just gathered
/// under one lock instead of many. Fields that a free function ELSEWHERE in this module already
/// takes as `&RefCell<...>` keep that inner `RefCell` (so those signatures don't change): `rt`,
/// `exec`, `devices`, `edges`, `momentary`, `held_keys`, `sniper`, `turbos`. Everything else is a
/// plain field, mutated directly through the `&mut LiveCtx` that `live_edge`/`live_tick` get from
/// their own `RefCell<LiveCtx>::borrow_mut()`.
struct LiveCtx<'a> {
    rt: RefCell<controls::Runtime>,
    exec: RefCell<DispatchExecutor>,
    devices: RefCell<neuron::device::DeviceSession<'a>>,
    /// Status shared with the UI: only post deltas (the loop runs hot, the UI updates on events).
    status: Arc<Mutex<LiveStatus>>,
    weak: slint::Weak<AppWindow>,
    /// Edge-detector: a Razer report is the SET of buttons currently down, so we DIFF successive
    /// reports into per-button down/up edges. This fixes multi-button chords (every newly-pressed
    /// control dispatches, not just the first hit) and precise HyperShift release (only the input
    /// that actually went up releases ITS layer — no blanket release_all on any empty report).
    edges: RefCell<HoldEdges>,
    /// MOMENTARY MIC held state: trigger -> (mic device, mute-state to restore on release). A held
    /// momentary action the stateless dispatch can't express — the edge loop owns its press/release.
    momentary: MomentaryMap,
    /// INPUT→KEY REMAP held state: trigger -> the output VKs currently held down. A key remap holds
    /// its output key while the control is held (so a macro key / remapped button acts like the real
    /// key — hold = hold, the OS auto-repeats), which the stateless tap-only action can't express.
    held_keys: KeyHoldMap,
    /// SNIPER held state: trigger -> the DPI to RESTORE on release (snapshotted at press, so it
    /// respects whatever stage the mouse was on). A held device write the stateless dispatch can't
    /// express — the edge loop owns the drop on DOWN and the restore on UP, like the momentary mic.
    sniper: SniperMap,
    turbos: RefCell<TurboRuntime>,
    live_rx: Receiver<LiveCommand>,
    /// Dispatch's OWN previous mic-mute cache sample — the stream it edge-detects on to fire MicTap
    /// on a real external toggle. SEPARATE from `glue::mic_tap_baseline` (the pill's shown value): a
    /// fresher source (the launch reconcile unit) can seed the baseline while dispatch's 400ms cache
    /// still lags, and edge-detecting against that cross-source value would misread the lag as a
    /// phantom tap. `None` until dispatch's first sample.
    last_mute: Option<bool>,
    switcher: neuron::app_focus::AppFocusSwitch,
    tick: u32,
    /// When the ~50ms mic-tap / app-focus polls last ran. These poll at their OWN wall-clock cadence
    /// (`POLL_INTERVAL`), decoupled from the PUMP's cadence: the pump now waits a variable interval
    /// (turbo ~8ms, idle ~1000ms), so a fixed `tick % N` gate would over-poll during turbo and
    /// under-poll (10×!) at idle. `None` until the first poll.
    last_poll: Option<Instant>,
    reload_pending: bool,
    injected: Vec<Trigger>,
    hypershift_latch: bool,
    applied_hypershift_latch: bool,
    gaming_policy_dirty: bool,
    /// GamingMode suppression hook handle. `None` in every `#[cfg(test)]`-built `LiveCtx` (see
    /// [`LiveCtx::for_tests`]) — a test must never touch the real Win32 LL hook.
    hook: Option<neuron::hook::Hook>,
}

#[cfg(test)]
impl<'a> LiveCtx<'a> {
    /// Build a `LiveCtx` for a unit test. Reads config from the CURRENT process cwd exactly like
    /// [`run_worker`]'s real construction does (via [`controls::build_runtime`]) — a test isolates
    /// that by wrapping itself in [`crate::testsupport::cwd_guard`] before calling this. Takes an
    /// already-built empty [`neuron::registry::Registry`] by reference (the caller owns it, since
    /// [`neuron::device::DeviceSession`] borrows it — the same local-then-borrow shape `run_worker`
    /// itself uses for `reg`/`devices`). Uses a DEAD `slint::Weak` (`Default`): there is no running
    /// event loop in a test, and `post_status`'s `slint::invoke_from_event_loop` call already
    /// tolerates that — with no platform registered it just returns
    /// `Err(EventLoopError::NoEventLoopProvider)` without ever touching the weak handle, exactly the
    /// same as every `let _ = slint::invoke_from_event_loop(...)` call site already assumes.
    /// NEVER installs the gaming hook (unlike `run_worker`'s real startup right after construction)
    /// — a test must never install the real LL hook or race another test on the process-global
    /// desired-policy cell; `hook` starts (and, in every test, stays) `None`.
    fn for_tests(reg: &'a neuron::registry::Registry, live_rx: Receiver<LiveCommand>) -> Self {
        LiveCtx {
            rt: RefCell::new(controls::build_runtime()),
            exec: RefCell::new(DispatchExecutor::new()),
            devices: RefCell::new(neuron::device::DeviceSession::new(reg)),
            status: Arc::new(Mutex::new(LiveStatus::default())),
            weak: slint::Weak::default(),
            edges: RefCell::new(HoldEdges::new()),
            momentary: RefCell::new(std::collections::HashMap::new()),
            held_keys: RefCell::new(std::collections::HashMap::new()),
            sniper: RefCell::new(std::collections::HashMap::new()),
            turbos: RefCell::new(TurboRuntime::new()),
            live_rx,
            last_mute: None,
            switcher: neuron::app_focus::AppFocusSwitch::new(),
            tick: 0,
            last_poll: None,
            reload_pending: false,
            injected: Vec::new(),
            hypershift_latch: false,
            applied_hypershift_latch: false,
            gaming_policy_dirty: false,
            hook: None,
        }
    }
}

/// A storm of faults inside this window trips the breaker. Mirrors the macro host's `Breaker`
/// (`crates/neuron-core/src/macros/macro_host.rs`) — same "more than N in a window" shape, same
/// N and window (4 in 30s), since both are guarding the identical failure mode: a deterministically
/// panicking body pinned behind a respawn-forever loop.
const DISPATCH_BREAKER_WINDOW: Duration = Duration::from_secs(30);
const DISPATCH_BREAKER_MAX: u32 = 4;
/// Resume-sleep base and ceiling. The base matches the pre-breaker fixed sleep (250ms) so a lone,
/// isolated fault behaves exactly as before; each further fault inside the window DOUBLES it, so a
/// building storm backs off before the breaker gives up outright (250ms, 500ms, 1s, 2s, 4s, 8s…).
const DISPATCH_BREAKER_BASE_SLEEP: Duration = Duration::from_millis(250);
const DISPATCH_BREAKER_MAX_SLEEP: Duration = Duration::from_secs(8);

/// Crash bookkeeping for the dispatch respawn loop — the live-dispatch twin of the macro host's
/// `Breaker`, deliberately NOT copy-pasted: unlike `Breaker`, which auto-resumes after a fixed
/// cooldown, a live-dispatch halt is user-facing (dead remaps/casts, not a background macro), so
/// there is no silent auto-resume — once tripped it STAYS tripped until an explicit
/// `LiveCommand::Reload` resets it (see `service_while_halted`). And unlike `Breaker`, every method
/// here takes `now: Instant` as a parameter instead of calling `Instant::now()` internally, so the
/// trip/backoff/reset arithmetic is pure and a test can drive it with synthetic timestamps instead
/// of real sleeps.
#[derive(Default)]
struct DispatchBreaker {
    /// Timestamps of faults still inside the window (oldest first).
    faults: std::collections::VecDeque<Instant>,
    /// Set once a storm crosses `DISPATCH_BREAKER_MAX`; only `reset` clears it.
    tripped: bool,
}

impl DispatchBreaker {
    fn new() -> Self {
        Self::default()
    }

    /// Record one respawn-triggering fault at `now`; drop faults that have aged out of the window,
    /// trip the breaker if more than `DISPATCH_BREAKER_MAX` remain inside it, and return the resume
    /// sleep to use before the caller's next reopen attempt (ignored once tripped — the caller stops
    /// reopening entirely and calls `service_while_halted` instead).
    fn record_fault(&mut self, now: Instant) -> Duration {
        self.faults.push_back(now);
        while self
            .faults
            .front()
            .is_some_and(|t| now.duration_since(*t) > DISPATCH_BREAKER_WINDOW)
        {
            self.faults.pop_front();
        }
        let n = self.faults.len() as u32;
        if n > DISPATCH_BREAKER_MAX {
            self.tripped = true;
        }
        DISPATCH_BREAKER_BASE_SLEEP
            .saturating_mul(1u32 << n.saturating_sub(1).min(5))
            .min(DISPATCH_BREAKER_MAX_SLEEP)
    }

    /// Is the breaker currently refusing to respawn?
    fn tripped(&self) -> bool {
        self.tripped
    }

    /// User-initiated revival (a `LiveCommand::Reload` arriving while halted): forget every past
    /// fault and un-trip, so the caller gets a completely fresh storm budget on the next attempt.
    fn reset(&mut self) {
        self.faults.clear();
        self.tripped = false;
    }
}

/// Service the live-command channel while the breaker is tripped — the respawn loop calls this
/// INSTEAD of reopening `listen_until`, so a deterministically-panicking cycle stops busy-faulting
/// at ~4Hz forever. It must keep draining `rx` (never stop reading it): `LiveCommand::Reload` is the
/// ONLY way out of a halt, and it arrives on this same channel — a tripped breaker that also stopped
/// servicing commands could never be revived.
///
/// Every command other than `Reload` is silently dropped while halted. That's safe: both reply-
/// bearing commands already bound their wait with `recv_timeout` on the CALLING side
/// (`toggle_hypershift_latch` / `apply_profile`), so to them a halted organ reads as an honest
/// timeout, never a hang — exactly the "no signal" failure this breaker exists to fix, just moved
/// from "silence forever" to "a bounded, explained wait."
///
/// Returns `true` once a `Reload` resets the breaker (the caller should resume respawning), `false`
/// if `stop` was asked for first, or the channel disconnected (the sender side is gone — treat that
/// the same as a stop rather than spin).
fn service_while_halted(
    rx: &Receiver<LiveCommand>,
    stop: &Arc<AtomicBool>,
    breaker: &mut DispatchBreaker,
) -> bool {
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(LiveCommand::Reload) => {
                breaker.reset();
                return true;
            }
            Ok(_other) => {} // dropped while halted — see doc above
            Err(RecvTimeoutError::Timeout) => {} // loop back and recheck `stop`
            Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

/// The worker body: arm input, install the gaming hook, build the engine, run the listen loop.
///
/// PLATFORM-NEUTRAL: every callee here is portable or cfg-seamed at its source —
/// [`neuron::controls::listen_until`] (Win32 Raw-Input pump on Windows, an inert tick-only loop
/// off-Windows), [`neuron::hook`] (LL gaming hook on Windows, inert stubs off-Windows), and
/// [`neuron::app_focus::AppFocusSwitch`] (foreground polling on Windows, `None` off-Windows). On a
/// non-Windows host the loop still arms/builds the engine and processes reload/inject/profile
/// commands on the tick — only hardware input edges are dormant (no source yet).
fn run_worker(weak: slint::Weak<AppWindow>, stop: Arc<AtomicBool>, live_rx: Receiver<LiveCommand>) {
    // Input posture: this is THE thread that turns a device edge into an action, so if the scheduler
    // leaves it waiting behind a fullscreen game's threads, every binding fires late — intermittently,
    // and worst exactly when a game is running. `latency::INJECT_HOP` is what measures whether this
    // is working. See `neuron::timing::boost_input_thread` for why ABOVE_NORMAL and not higher.
    let boosted = neuron::timing::boost_input_thread();
    crate::flight::trace(
        "life",
        if boosted {
            "live dispatch pump: input priority raised"
        } else {
            "live dispatch pump: running at default priority"
        },
        0,
    );
    // Build the ONE unified spine (bindings.toml + cast.toml + profiles/*.rules.toml + apps.toml).
    let rt = controls::build_runtime();
    let exec = DispatchExecutor::new();
    let reg = neuron::registry::Registry::load().unwrap_or(neuron::registry::Registry {
        devices: Vec::new(),
    });
    let devices = neuron::device::DeviceSession::new(&reg);

    // Status shared with the UI: only post deltas (the loop runs hot, the UI updates on events).
    let status = Arc::new(Mutex::new(LiveStatus::default()));
    {
        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.held_layers = String::new();
    }
    post_status(&weak, &status);

    // Every borrow the two `listen_until` closures below need, gathered into ONE `LiveCtx` (see its
    // doc) built ONCE here — outside the immortal loop — so its state SURVIVES a listener reopen
    // exactly like the individual RefCells it replaces used to.
    let mut ctx = RefCell::new(LiveCtx {
        rt: RefCell::new(rt),
        exec: RefCell::new(exec),
        devices: RefCell::new(devices),
        status,
        weak,
        edges: RefCell::new(HoldEdges::new()),
        momentary: RefCell::new(std::collections::HashMap::new()),
        held_keys: RefCell::new(std::collections::HashMap::new()),
        sniper: RefCell::new(std::collections::HashMap::new()),
        turbos: RefCell::new(TurboRuntime::new()),
        live_rx,
        last_mute: None,
        switcher: neuron::app_focus::AppFocusSwitch::new(),
        tick: 0,
        last_poll: None,
        reload_pending: false,
        injected: Vec::new(),
        hypershift_latch: false,
        applied_hypershift_latch: false,
        gaming_policy_dirty: false,
        hook: None,
    });

    // GamingMode suppression hook (Alt+Tab / Win / Alt+F4). The hook SELF-HOSTS a dedicated
    // message-pump thread (see neuron::hook / sys::pump_main), so it is immune to THIS thread's
    // blocking device I/O. That immunity is the whole fix: an LL keyboard hook is silently bypassed
    // by Windows if its installing thread misses the LowLevelHooksTimeout (~300 ms), and this
    // listener does tens-of-ms sniper writes / mic reads / sleep throttles inside its callback —
    // when the hook lived here, the KEY GUARD chords only suppressed while the thread was idle, i.e.
    // never reliably. Now this handle is just an ownership token pushing desired policy at a hook
    // that pumps itself. The policy comes from the active profile's ApplyReport; we read it from the
    // shared cell the glue updates on profile apply.
    install_gaming_hook(&mut ctx.get_mut().hook);

    // DEVICE-SIDE REMAP SHIM: arm the user-mode interceptor from the engine's pid-scoped keyboard
    // `Key` bindings (the Naga thumb grid etc. — hardware keyboards Razer only remaps via a kernel
    // filter; this is the driver-free user-mode equivalent). No-op when there are none.
    {
        let c = ctx.borrow();
        neuron::intercept::configure_from_engine(&c.rt.borrow().engine);
    }

    // ── THE IMMORTAL LISTENER ── this worker is the organ that fires every cast and remap; if it
    // dies, spellweaving "visually works but nothing happens" — the worst reliability lie the app
    // can tell. So: ESC must NOT stop it (esc_stops=false — ESC is the weave-cancel key and the
    // close-the-game-menu key; the old shared listener died on the first ESC of the session), and
    // any exit that wasn't an explicit stop (a contained fault, a dead listener window) is logged
    // to the flight ring and the listener is simply REOPENED. Casts must never silently die.
    //
    // "Immortal" has a limit, though: a DETERMINISTICALLY panicking cycle (a config that panics
    // every tick) would otherwise respawn at ~4Hz forever, burning CPU and spamming the flight log
    // with zero signal to the user that live dispatch is effectively dead. `breaker` is that limit —
    // see its doc. While tripped this loop stops reopening the listener but keeps the command
    // channel alive via `service_while_halted`, so a user-initiated Reload is still the one way back.
    let mut breaker = DispatchBreaker::new();
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            controls::listen_until(
                None,
                &stop,
                false,
                |ev| live_edge(&mut ctx.borrow_mut(), ev),
                || live_tick(&mut ctx.borrow_mut()),
            );
        }));
        if stop.load(Ordering::SeqCst) {
            break; // an asked-for stop — the only legitimate way out
        }
        let breadcrumb = if outcome.is_err() {
            "dispatch listener fault contained — reopening"
        } else {
            "dispatch listener exited unasked — reopening"
        };
        crate::flight::trace("life", breadcrumb, 0);
        {
            let c = ctx.borrow();
            momentary_release_all(&c.momentary); // never strand a held mic across a respawn
            key_remap_release_all(&c.held_keys); // nor a held remapped key
            sniper_release_all(&c.devices, &c.sniper); // nor a held sniper (restore the DPI)
        }
        let resume_sleep = breaker.record_fault(Instant::now());
        if breaker.tripped() {
            crate::flight::trace("life", "dispatch breaker tripped — halting respawns until Reload", 0);
            {
                let c = ctx.borrow();
                let mut s = c.status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                s.last_trigger = "dispatch halted".into();
                s.last_action =
                    format!("live dispatch halted after repeated faults: {breadcrumb} — reload to retry");
                post_status(&c.weak, &c.status);
            }
            let revived = {
                let c = ctx.borrow();
                service_while_halted(&c.live_rx, &stop, &mut breaker)
            };
            if !revived {
                break; // stop was asked for (or the channel died) while halted
            }
            crate::flight::trace("life", "dispatch breaker reset by Reload — resuming", 0);
            continue; // breaker.tripped() is now false; go straight back to reopening, no sleep
        }
        std::thread::sleep(resume_sleep);
    }

    // teardown: restore any mic a momentary action was holding (the loop ended mid-hold) + release
    // any remapped key still held + restore any sniper DPI still dropped, the hook drops here
    // (uninstalls), input disarms in LiveRuntime::stop.
    {
        let c = ctx.borrow();
        momentary_release_all(&c.momentary);
        key_remap_release_all(&c.held_keys);
        sniper_release_all(&c.devices, &c.sniper);
    }
    // the hold feed goes default with the loop — a painted mode light never outlives the mode.
    LAYER_HELD.store(false, Ordering::Relaxed);
    SNIPER_HELD.store(false, Ordering::Relaxed);
    push_hold_state();
    neuron::intercept::deactivate(); // uninstall the remap-shim hook; the desktop returns to normal
    drop(ctx); // the hook (among everything else) drops here (uninstalls)
}

/// The live worker's per-event edge handler — the exact body of `run_worker`'s old `on_event`
/// closure, verbatim (capture-active check, edge loop, down/up arms, publish_held), now callable
/// one edge at a time so a test can drive it directly instead of only through the immortal loop.
/// Does the device-side remap shim own this trigger? Only pid-scoped keyboard-page `Input`
/// triggers can be shim-owned; everything else dispatches through the engine as before.
fn interceptor_owns(t: &Trigger) -> bool {
    match t {
        Trigger::Input {
            page,
            usage,
            pid: Some(pid),
        } => neuron::intercept::owns(*page, *usage, *pid),
        _ => false,
    }
}

fn live_edge(ctx: &mut LiveCtx, ev: &ControlEvent) {
    // While a press-to-bind capture is in flight, the user is pressing a control to BIND it,
    // not to use it — track edges but fire NOTHING, so the captured key doesn't also run
    // whatever it's currently bound to. (During a CONTROL capture we mostly see nothing at
    // all: the capture's own transient listener steals the process's Raw-Input registration
    // until it ends — the resident pump re-arms itself right after; see controls REARM.)
    let capturing = crate::capture::CAPTURE_ACTIVE.load(Ordering::Relaxed);
    let edges = {
        // The held-set diff that turns one report into Down/Up edges. `edges` returns an owned Vec,
        // so the timer closes on the diff itself rather than spanning every action the edges go on
        // to fire — which is what would make this reading meaningless.
        let _t = neuron::latency::start(&neuron::latency::EDGE_DIFF);
        ctx.edges.borrow_mut().edges(ev)
    };
    for edge in edges {
        if capturing {
            continue;
        }
        // The remap SHIM owns this trigger at the input layer (it swallows the original + injects
        // the target key). Do NOT also dispatch it host-side — that would double-send.
        let edge_trigger = match &edge {
            InputEdge::Down(t) | InputEdge::Up(t) => t,
        };
        if interceptor_owns(edge_trigger) {
            continue;
        }
        match edge {
            InputEdge::Down(trigger) => {
                // Hold any HyperShift layer THIS input activates (tracked per-input so its
                // release drops only its own layer), then dispatch the input's action.
                ctx.rt.borrow_mut().hold_for_input(&trigger);
                // An input→key REMAP holds the output key while held (edge-driven, like
                // the mic) so it behaves like the real key; every OTHER action fires once.
                // Skip fire_trigger when we held a key — firing would ALSO tap it.
                if !key_remap_press(&ctx.rt, &ctx.held_keys, &trigger, &ctx.status, &ctx.weak) {
                    if let Some(outcome) = fire_trigger(
                        &mut ctx.devices.borrow_mut(),
                        &mut ctx.rt.borrow_mut(),
                        &mut ctx.exec.borrow_mut(),
                        &trigger,
                        &ctx.status,
                        &ctx.weak,
                    ) {
                        ctx.turbos.borrow_mut().start(outcome.turbo);
                    }
                }
                // momentary mic: capture the rest state + flip while held.
                momentary_press(&ctx.rt, &ctx.momentary, &trigger);
                // sniper: snapshot the live DPI + drop to precision while held.
                sniper_press(&ctx.devices, &ctx.rt, &ctx.sniper, &trigger);
            }
            InputEdge::Up(trigger) => {
                ctx.rt.borrow_mut().release_for_input(&trigger);
                ctx.turbos.borrow_mut().release(&trigger);
                key_remap_release(&ctx.held_keys, &trigger); // release the held output key
                momentary_release(&ctx.momentary, &trigger); // restore the mic on release
                sniper_release(&ctx.devices, &ctx.sniper, &trigger); // restore the DPI on release
            }
        }
    }
    publish_held(&ctx.rt, &ctx.status, &ctx.weak);
}

/// Fallback pump cadence while a turbo is held but [`TurboRuntime::min_interval`] somehow can't
/// name one (never happens today — `min_interval` is always `Some` when `is_active()` is true;
/// kept as a documented floor rather than unwrapping).
const TURBO_FALLBACK_CADENCE: Duration = Duration::from_millis(8);
/// Matches today's `tick % 10` throttle: the mic-tap / app-focus polls already run at ~50ms
/// (5ms pump iteration * 10). Returned as the cadence hint while the engine binds either.
const POLL_CADENCE: Duration = Duration::from_millis(50);
/// Nothing in the active engine needs periodic polling — a generous idle cadence. Once the
/// blocking-wait rewrite lands, the wake event (not this hint) delivers real work instantly; this
/// value only bounds how long the pump would otherwise sit blocked with nothing to do.
const IDLE_CADENCE: Duration = Duration::from_millis(1000);

/// Wall-clock cadence for the mic-tap / app-focus polls, independent of the pump's own (variable)
/// cadence. Matches the ~50ms the old fixed-5ms pump delivered via its `tick % 10` gate.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The pump-cadence HINT this tick wants (see [`controls::listen_until`]'s `on_tick` doc): the max
/// time the pump should wait before calling `live_tick` again. Cheap on every call — turbo
/// activity is an O(1) check ([`TurboRuntime::is_active`]) and the engine's poll-need is a cached
/// bool ([`controls::Runtime::needs_periodic_poll`]), never a rule rescan. The blocking-wait pump
/// waits at most this long before the next tick (turbo ~8ms / mic-or-appfocus-bound 50ms / idle 1s);
/// under `NEURON_PUMP=poll` the fixed 5ms sleep ignores it.
fn live_cadence(ctx: &LiveCtx) -> Duration {
    let turbos = ctx.turbos.borrow();
    if turbos.is_active() {
        return turbos.min_interval().unwrap_or(TURBO_FALLBACK_CADENCE);
    }
    drop(turbos);
    if ctx.rt.borrow().needs_periodic_poll() {
        return POLL_CADENCE;
    }
    IDLE_CADENCE
}

/// The live worker's per-tick handler — the exact body of `run_worker`'s old `on_tick` closure
/// (tick increment, command drain, flight pulse, reload rebuild, injected drain, latch reconcile,
/// gaming hook reconcile, turbo tick, and the timestamp-throttled mic-tap + app-focus polls below
/// it), now callable one tick at a time so a test can drive it directly. Returns the pump-cadence
/// hint (see [`live_cadence`]) at EVERY exit path — the blocking-wait pump waits that long before
/// the next tick.
fn live_tick(ctx: &mut LiveCtx) -> Duration {
    ctx.tick = ctx.tick.wrapping_add(1);
    // Remap shim fail-open: replay any keystroke swallowed but never attributed by Raw-Input.
    neuron::intercept::expire_tick();
    for cmd in ctx.live_rx.try_iter() {
        match cmd {
            LiveCommand::Reload => ctx.reload_pending = true,
            LiveCommand::Inject(trigger) => ctx.injected.push(trigger),
            LiveCommand::ToggleHyperShift(reply) => {
                ctx.hypershift_latch = !ctx.hypershift_latch;
                let _ = reply.send(ctx.hypershift_latch);
            }
            LiveCommand::ReconcileGamingHook => ctx.gaming_policy_dirty = true,
            LiveCommand::ApplyProfile {
                name,
                persist,
                reply,
            } => {
                let result = apply_profile_live(&mut ctx.devices.borrow_mut(), &name, persist);
                if let Ok(applied) = &result {
                    neuron::hook::set_policy(applied.policy);
                    ctx.gaming_policy_dirty = true;
                    {
                        let mut s = ctx
                            .status
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        s.last_trigger = format!("profile {name}");
                        s.last_action = applied.summary.clone();
                        s.active_profile = name.clone();
                    }
                    post_status(&ctx.weak, &ctx.status);
                }
                let _ = reply.send(result);
            }
        }
    }
    // flight heartbeat: the live loop proves it's alive every tick — a silent organ is
    // surfaced by the UI watch (and recorded in every crash dump).
    crate::flight::pulse(crate::flight::organ::DISPATCH);
    // Engine hot-reload: a GUI editor rewrote the config — rebuild NOW, before any
    // throttle (an atomic swap per tick is free; the RefCell has no outstanding borrow
    // here because the event and tick closures run sequentially on this thread).
    if ctx.reload_pending {
        ctx.reload_pending = false;
        momentary_release_all(&ctx.momentary); // a held mic can't survive a config swap
        key_remap_release_all(&ctx.held_keys); // nor a held remapped key
        sniper_release_all(&ctx.devices, &ctx.sniper); // nor a held sniper (restore the DPI)
        *ctx.rt.borrow_mut() = controls::build_runtime();
        // Re-arm the remap shim from the rebuilt engine (a rebind/added binding takes effect here).
        neuron::intercept::configure_from_engine(&ctx.rt.borrow().engine);
        ctx.exec.borrow_mut().clear();
        ctx.devices.borrow_mut().clear();
        ctx.turbos.borrow_mut().clear();
        ctx.applied_hypershift_latch = !ctx.hypershift_latch;
        *LAST_ACTION_DESC
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        publish_held(&ctx.rt, &ctx.status, &ctx.weak);
    }
    // Injected triggers (the weave watcher's resolved flicks/glyphs) — same Engine,
    // same fire path, so a cast composes with layers/intents/SAFE exactly like hardware.
    {
        let drained = std::mem::take(&mut ctx.injected);
        for t in drained {
            if fire_trigger(
                &mut ctx.devices.borrow_mut(),
                &mut ctx.rt.borrow_mut(),
                &mut ctx.exec.borrow_mut(),
                &t,
                &ctx.status,
                &ctx.weak,
            )
            .is_some()
            {
                crate::flight::trace("cast", "injected trigger fired", 0);
            } else {
                // a cast the user SAW resolve (the wheel lit, the glyph flared) that
                // matched nothing in the engine — that must never be a silent fizzle.
                crate::flight::trace("cast", "injected trigger matched NOTHING", 0);
                {
                    let mut s = ctx
                        .status
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    s.last_trigger = t.describe();
                    s.last_action =
                        "cast hit nothing \u{2014} not bound in the engine (reload/config mismatch?)".into();
                }
                post_status(&ctx.weak, &ctx.status);
            }
        }
    }
    if ctx.applied_hypershift_latch != ctx.hypershift_latch {
        ctx.rt
            .borrow_mut()
            .latch_layer("hypershift", ctx.hypershift_latch);
        ctx.applied_hypershift_latch = ctx.hypershift_latch;
        publish_held(&ctx.rt, &ctx.status, &ctx.weak);
    }
    // Gaming policy is pushed by glue/profile apply; reconcile only when that shared
    // carrier changed instead of polling every dispatch tick.
    if ctx.gaming_policy_dirty {
        ctx.gaming_policy_dirty = false;
        install_gaming_hook(&mut ctx.hook);
    }
    {
        let mut intents = AppIntentRunner {
            devices: &mut ctx.devices.borrow_mut(),
        };
        ctx.turbos
            .borrow_mut()
            .tick(&mut ctx.exec.borrow_mut(), &mut intents);
    }
    // Throttle the periodic polls (mic-tap / app-focus) to their OWN ~50ms wall-clock cadence,
    // independent of how fast the pump is ticking. The old `tick % 10` gate assumed a fixed 5ms
    // pump iteration (10 × 5ms ≈ 50ms); the pump now waits a VARIABLE cadence, so counting ticks
    // would poll every ~80ms under turbo and only every ~10s at idle (a 10× regression the reviewer
    // caught). A timestamp gate gives the real ~50ms in every pump mode.
    let now = Instant::now();
    if ctx.last_poll.is_some_and(|t| now.duration_since(t) < POLL_INTERVAL) {
        return live_cadence(ctx);
    }
    ctx.last_poll = Some(now);
    // mic tap (cached Core-Audio mute toggle) -> a MicTap trigger AND its raw Input usage.
    // Reads the CACHED mic mute (refreshed off-thread by `beacon::audio_cache`) — polling
    // `VolumeCtl::get_mute()` inline here used to hang the whole dispatch loop when an audio
    // endpoint stalled (Core-Audio COM blocks indefinitely), the dispatch-stall the flight log
    // caught. STATE PUBLISH and EFFECTS FIRING are two separate concerns here (the Chunk-B fix):
    // publishing to the UI pill is UNCONDITIONAL, every sample, through the one authoritative
    // writer (`glue::publish_mic_state`) — the old code only published on a detected `fire` edge,
    // so a wrong value seeded before this loop's first sample (or by anything else) could sit on
    // screen, silently adopted as the new baseline, until the NEXT real toggle. Firing the
    // EFFECTS (`Trigger::MicTap` + its synthetic `Input`) stays edge-gated, via the pure
    // `mic_tap_decision`, and additionally consults the echo latch (`neuron::mic_state::
    // take_self_mute_write`) so a change neuron caused itself (hidwatch's hardware-mute bridge,
    // the panel's mic toggle, a momentary hold, `Action::MicMute`) never re-fires as if it were a
    // fresh external tap.
    if let Some(now) = crate::beacon::audio_cache::snap().mic.map(|(_, m)| m) {
        // Edge-detect on dispatch's OWN cache stream (`ctx.last_mute`), NOT the shared pill baseline:
        // a fresher source can seed the baseline while this ~400ms cache still lags, and firing off
        // that cross-source disagreement would be a phantom tap. First sample (`None`) is never an
        // edge.
        let own_edge = ctx.last_mute.is_some_and(|prev| prev != now);
        // Decide MicTap's EFFECTS. Only an EDGE can be a tap; and an edge that is OUR OWN write
        // finally surfacing must not fire. `consume_self_write_on_edge` answers the latter AND closes
        // the self-write window on that edge — so the suppression is bounded by "our write surfaced",
        // not by a wall clock, and therefore survives an arbitrarily-delayed cache observation
        // (endpoint stall / pump starvation) that a fixed-duration window could not cover. A non-edge
        // sample can't be our write surfacing, so it never consumes the window.
        let fire = own_edge && !neuron::mic_state::consume_self_write_on_edge();
        // Publish the pill on an own-stream edge, OR to seed it when nothing has published yet (the
        // launch reconcile unit may have read Unknown or not run). Never publish this (possibly
        // stale) reading OVER a value a fresher source already seeded — that flips the pill backward.
        if own_edge || crate::glue::mic_tap_baseline().is_none() {
            crate::glue::publish_mic_state(now);
        }
        ctx.last_mute = Some(now);
        if fire {
            // A mic tap is a real input edge, so it is stamped like one — otherwise this whole class of
            // trigger would be invisible to `press_to_output` / `edge_to_done`. It is NOT an injected
            // `ControlEvent` (it is detected here, by polling Core Audio), so nothing upstream has
            // wrapped it; a live capture proved the gap — the toggles produced `resolve` samples with
            // no end-to-end reading at all. ONE `with_edge` spans both triggers because one physical
            // tap fires both, and the instrument measures the tap, not each rule it matches.
            neuron::latency::with_edge(Instant::now(), || {
                fire_trigger(
                    &mut ctx.devices.borrow_mut(),
                    &mut ctx.rt.borrow_mut(),
                    &mut ctx.exec.borrow_mut(),
                    &Trigger::MicTap,
                    &ctx.status,
                    &ctx.weak,
                );
                let (p, u) = MIC_TAP;
                fire_trigger(
                    &mut ctx.devices.borrow_mut(),
                    &mut ctx.rt.borrow_mut(),
                    &mut ctx.exec.borrow_mut(),
                    &Trigger::Input {
                        page: p,
                        usage: u,
                        pid: Some(0x056a),
                    },
                    &ctx.status,
                    &ctx.weak,
                );
            });
        }
    }
    // app-aware switch: a focus change fires an AppFocus trigger; the Engine's matching
    // rule (-> ProfileSwitch intent) does the switch (or nothing if unbound).
    if let Some(app) = ctx.switcher.poll() {
        {
            let mut s = ctx
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            s.focused_app = app.clone();
        }
        post_status(&ctx.weak, &ctx.status);
        // Stamped for the same reason as the mic tap: a focus change is an edge the user causes, and a
        // profile switch that feels slow should be measurable rather than anecdotal.
        neuron::latency::with_edge(Instant::now(), || {
            fire_trigger(
                &mut ctx.devices.borrow_mut(),
                &mut ctx.rt.borrow_mut(),
                &mut ctx.exec.borrow_mut(),
                &Trigger::AppFocus { app },
                &ctx.status,
                &ctx.weak,
            );
        });
    }
    live_cadence(ctx)
}

// ── MOMENTARY MIC: the held push-to-talk / push-to-mute edge handling ─────────────────────────
type MomentaryMap =
    std::cell::RefCell<std::collections::HashMap<neuron::engine::Trigger, (Option<String>, bool)>>;

/// Open the mic VolumeCtl for a momentary action's (optional) device, WITH the resolved endpoint id.
/// Callers need the id to ask `neuron::audio::is_default_capture_id` before arming the echo latch: a
/// momentary bound to a NAMED secondary mic must not arm a latch the dispatch detector (which only
/// ever samples the DEFAULT endpoint) would then consume against an unrelated real tap.
/// Cross-platform via the `audio` seam: off-Windows `VolumeCtl::open` returns `None` (no audio
/// backend), so the whole momentary path falls through to a no-op without any cfg gating here.
fn open_mic(device: &Option<String>) -> Option<(String, neuron::audio::VolumeCtl)> {
    neuron::audio::resolve_capture(device.as_deref())
        .and_then(|e| neuron::audio::VolumeCtl::open(&e.id).map(|c| (e.id, c)))
}

/// Open the mic-tap self-write window for a mute write we just made — but ONLY if it (a) actually
/// CHANGED the state (`changed`: a no-op write is no transition, so the poll sees no edge and a
/// window would only shadow a real tap) and (b) landed on the DEFAULT capture endpoint (the one
/// stream the detector samples). See `neuron::mic_state`'s doc. `VolumeCtl::set_mute` returns
/// whether it changed anything, so the window now tracks real OS transitions, not write attempts.
fn note_mic_write_if_default(id: &str, changed: bool) {
    if changed && neuron::audio::is_default_capture_id(id) {
        neuron::mic_state::note_self_mute_write();
    }
}

/// A trigger's DOWN edge: if it binds a momentary mic, capture the resting mute-state, flip the mic
/// for the hold, and remember what to restore. Idempotent (a repeat down without an up is ignored).
/// Off-Windows `open_mic` finds no handle, so this is a no-op — one definition, every target.
fn momentary_press(
    rt: &std::cell::RefCell<neuron::controls::Runtime>,
    held: &MomentaryMap,
    trigger: &neuron::engine::Trigger,
) {
    if held.borrow().contains_key(trigger) {
        return;
    }
    let Some((device, mode)) = rt.borrow().momentary_mic_for(trigger) else {
        return;
    };
    if let Some((id, ctl)) = open_mic(&device) {
        let (while_held, on_release) = mode.states(ctl.get_mute());
        // Open the self-write window only if the write CHANGES the OS state (`set_mute` returns that).
        // A no-op write is no transition, so the dispatch poll sees no edge and a window would only
        // shadow a genuine tap. A real transition's ~1s window opens well inside the ~400ms cache lag
        // before the poll could sample it, so there's no race despite arming after the write.
        let changed = ctl.set_mute(while_held);
        note_mic_write_if_default(&id, changed);
        held.borrow_mut()
            .insert(trigger.clone(), (device, on_release));
    }
}

/// A trigger's UP edge: restore the mic to its resting state.
fn momentary_release(held: &MomentaryMap, trigger: &neuron::engine::Trigger) {
    if let Some((device, restore)) = held.borrow_mut().remove(trigger) {
        if let Some((id, ctl)) = open_mic(&device) {
            let changed = ctl.set_mute(restore);
            note_mic_write_if_default(&id, changed); // only on a real transition — see momentary_press
        }
    }
}

/// Restore EVERY held mic and clear the map — the safety net for a config swap / daemon stop, so a
/// momentary can never strand the mic flipped.
fn momentary_release_all(held: &MomentaryMap) {
    for (_, (device, restore)) in held.borrow_mut().drain() {
        if let Some((id, ctl)) = open_mic(&device) {
            let changed = ctl.set_mute(restore);
            note_mic_write_if_default(&id, changed); // only on a real transition — see momentary_press
        }
    }
}

// ── SNIPER: the held hold-to-precision-DPI edge handling (mirrors the momentary mic) ───────────
/// trigger → (the DPI to RESTORE on release, the device pid it was written to — the pid keys the
/// confirmation + its echo-absorbing baseline, see `neuron::confirm::sniper`).
type SniperMap = std::cell::RefCell<std::collections::HashMap<Trigger, (u16, u16)>>;

/// A trigger's DOWN edge: if it binds a [`neuron::action::Action::Sniper`], snapshot the LIVE DPI,
/// drop to the precision DPI (VOLATILE — never flashed onboard, so it reverts on its own), and
/// remember the base to restore. Idempotent (a repeat down without an up is ignored). The snapshot +
/// drop are ONE `with_writable` op so a stale handle recovers once. No device / unsupported → a
/// silent no-op (the mouse is untouched, and nothing is recorded so the up edge is a no-op too).
fn sniper_press(
    devices: &std::cell::RefCell<neuron::device::DeviceSession<'_>>,
    rt: &std::cell::RefCell<neuron::controls::Runtime>,
    held: &SniperMap,
    trigger: &Trigger,
) {
    if held.borrow().contains_key(trigger) {
        return;
    }
    let Some(dpi) = rt.borrow().sniper_dpi_for(trigger) else {
        return;
    };
    // Park the host lighting writers before touching the wire: the DPI
    // snapshot is an ACK'd READ, and a streaming lighting frame clobbers its
    // pending reply — the race that made a bound sniper silently no-op from
    // the day the host started streaming the base layer. Worst case is one
    // parked frame (~35ms) added to the press edge; a sniper that fires
    // late-but-always beats one that never does.
    let _gates = crate::host::io_gate_all();
    let base = devices.borrow_mut().with_writable("set_dpi", |d| {
        let (base_x, _) = neuron::capability::dpi(d)?;
        neuron::capability::set_dpi(d, dpi, dpi, neuron::capability::Store::Volatile)?;
        Ok((base_x, d.pid))
    });
    if let Ok((base_x, pid)) = base {
        held.borrow_mut().insert(trigger.clone(), (base_x, pid));
        // its OWN confirmation kind (gated separately from plain DPI, default off) — and the
        // constructor updates the pid's DPI baseline either way, so the mouse's echo of this
        // write is absorbed instead of carding as a spurious "DPI changed" mid-game.
        neuron::confirm::sniper(pid, dpi as u32, Some(base_x as u32), true);
        // the mode-light edge: a sniper hold is now live.
        SNIPER_HELD.store(true, Ordering::Relaxed);
        push_hold_state();
    }
}

/// A trigger's UP edge: restore the snapshotted base DPI (volatile) and forget the hold, so a
/// re-press re-snapshots. A no-op if this trigger wasn't holding a sniper.
fn sniper_release(
    devices: &std::cell::RefCell<neuron::device::DeviceSession<'_>>,
    held: &SniperMap,
    trigger: &Trigger,
) {
    // remove OUTSIDE the if-let so the RefMut temporary is dropped before the emptiness re-read
    // below (an if-let scrutinee's temporary lives for the whole block).
    let removed = held.borrow_mut().remove(trigger);
    if let Some((base, pid)) = removed {
        // same wire discipline as the press: the restore is read-back verified
        let _gates = crate::host::io_gate_all();
        let ok = devices.borrow_mut().with_writable("set_dpi", |d| {
            neuron::capability::set_dpi(d, base, base, neuron::capability::Store::Volatile)
        });
        if ok.is_ok() {
            neuron::confirm::sniper(pid, base as u32, None, false);
        }
        // the mode-light edge: only dark when NO sniper hold remains (two thumbs, one truth).
        SNIPER_HELD.store(!held.borrow().is_empty(), Ordering::Relaxed);
        push_hold_state();
    }
}

/// Restore EVERY held sniper and clear the map — the safety net for a config swap / daemon stop, so
/// a held sniper can never STRAND the mouse at the precision DPI. Drains first (releasing the map
/// borrow) so each device write can re-borrow the session.
fn sniper_release_all(
    devices: &std::cell::RefCell<neuron::device::DeviceSession<'_>>,
    held: &SniperMap,
) {
    let bases: Vec<(u16, u16)> = held.borrow_mut().drain().map(|(_, held)| held).collect();
    // the map is drained either way — the mode light must read dark from here on.
    SNIPER_HELD.store(false, Ordering::Relaxed);
    push_hold_state();
    if bases.is_empty() {
        return;
    }
    let _gates = crate::host::io_gate_all(); // park the lighting writers for the restore batch
    for (base, pid) in bases {
        let ok = devices.borrow_mut().with_writable("set_dpi", |d| {
            neuron::capability::set_dpi(d, base, base, neuron::capability::Store::Volatile)
        });
        if ok.is_ok() {
            neuron::confirm::sniper(pid, base as u32, None, false);
        }
    }
}

// ── INPUT→KEY REMAP: hold the output key while the control is held (edge-driven, like the mic) ──
type KeyHoldMap = std::cell::RefCell<std::collections::HashMap<Trigger, Vec<u16>>>;

/// A trigger's DOWN edge: if it binds a plain key output, press-and-HOLD that key and remember the
/// VKs (so the UP edge releases them). Returns `true` if it held a key — the caller then SKIPS the
/// one-shot dispatch (firing would tap the same key). Idempotent on a repeat down. Mirrors
/// [`momentary_press`]; the held output key behaves like the real key (the OS supplies auto-repeat).
fn key_remap_press(
    rt: &std::cell::RefCell<neuron::controls::Runtime>,
    held: &KeyHoldMap,
    trigger: &Trigger,
    status: &Arc<Mutex<LiveStatus>>,
    weak: &slint::Weak<AppWindow>,
) -> bool {
    if held.borrow().contains_key(trigger) {
        return true; // already holding (a repeat down) — still a key remap, don't fire
    }
    let Some(key) = rt.borrow().key_remap_for(trigger) else {
        return false; // not a key remap — let the caller dispatch normally
    };
    let vks = neuron::action::press_and_hold(&key);
    if vks.is_empty() {
        return false; // unknown key — fall back to normal dispatch (it reports the error)
    }
    held.borrow_mut().insert(trigger.clone(), vks);
    rt.borrow_mut().note_fired(trigger); // a real trigger fired: spend one-shot layers, etc.
    {
        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_trigger = trigger.describe();
        s.last_action = format!("hold [{key}]");
        s.fired += 1;
        s.active_profile = neuron::profile::active();
    }
    post_status(weak, status);
    true
}

/// A trigger's UP edge: release the output key it was holding.
fn key_remap_release(held: &KeyHoldMap, trigger: &Trigger) {
    if let Some(vks) = held.borrow_mut().remove(trigger) {
        neuron::action::release_keys(&vks);
    }
}

/// Release EVERY held output key and clear the map — the safety net for a config swap / daemon stop,
/// so a remap can never strand a key down.
fn key_remap_release_all(held: &KeyHoldMap) {
    for (_, vks) in held.borrow_mut().drain() {
        neuron::action::release_keys(&vks);
    }
}

/// Fire one [`Trigger`] through the unified Engine and carry out the result — the GUI's port of the
/// CLI daemon's `fire_trigger`. Runs every matching rule's host action, routes any daemon [`Intent`]
/// (DPI / scroll / profile) to real device/profile state, single-presses a Turbo, and records the
/// outcome into the shared status for the UI readout.
fn fire_trigger(
    devices: &mut neuron::device::DeviceSession<'_>,
    rt: &mut neuron::controls::Runtime,
    exec: &mut DispatchExecutor,
    trigger: &Trigger,
    status: &Arc<Mutex<LiveStatus>>,
    weak: &slint::Weak<AppWindow>,
) -> Option<DispatchOutcome> {
    let mut intents = AppIntentRunner { devices };
    let outcome = exec.fire(&rt.engine, trigger, &mut intents)?;
    *LAST_ACTION_DESC
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = exec.last_action_desc();
    // a real trigger fired: one-shot layers are now spent (the arming press never counts).
    rt.note_fired(trigger);
    {
        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.last_trigger = outcome.trigger.clone();
        s.last_action = outcome.action.clone();
        s.fired += 1;
        // carry the cursor so a live ProfileSwitch/Cycle shows in the header within this post.
        s.active_profile = neuron::profile::active();
    }
    post_status(weak, status);
    Some(outcome)
}

struct AppIntentRunner<'a, 'reg> {
    devices: &'a mut neuron::device::DeviceSession<'reg>,
}

impl IntentRunner for AppIntentRunner<'_, '_> {
    fn run_intent(&mut self, intent: &neuron::action::Intent) -> String {
        run_intent(self.devices, intent)
    }
}

/// Carry out a daemon [`Intent`] against live device/profile state — the GUI port of the CLI's
/// `run_intent`. The stateless Action layer has no device handle, so DPI/scroll/profile work lands
/// here. Returns a short result line for the status readout.
fn run_intent(
    devices: &mut neuron::device::DeviceSession<'_>,
    intent: &neuron::action::Intent,
) -> String {
    use neuron::action::Intent;
    // INSTRUMENT intents are APP-level (no device write) — they route to the weave service and
    // deliberately bypass the write kill-switch below (pausing device writes must not take your
    // whiteboard away). Any trigger in the spine can now open an instrument: wedge, glyph,
    // button, app rule — the n-style promise.
    match intent {
        Intent::Teleport => {
            crate::beacon::request_instrument(1);
            return "teleport primed \u{2014} hold the trigger and drag the ghost".into();
        }
        Intent::Whiteboard => {
            crate::beacon::request_instrument(2);
            return "whiteboard opening".into();
        }
        Intent::Knockback => {
            crate::beacon::request_instrument(5);
            return "knockback \u{2014} the familiar wakes; drum on the trigger".into();
        }
        Intent::Glance(target) => {
            // GLANCE: toggle the live peek constellation — pure view, no device, no focus change.
            return crate::glance::toggle(target);
        }
        // the window QUICK-ACTIONS: host-side window management, no device write.
        Intent::Summon(window, mode) => return crate::wm::summon(window, *mode),
        Intent::Banish(pick) => return crate::wm::banish(*pick),
        Intent::Pin(pick) => return crate::wm::pin(*pick),
        Intent::Kill(pick) => return crate::wm::kill(*pick),
        Intent::Tether(slot, mode) => {
            return match mode {
                neuron::action::TetherMode::Mark => crate::wm::tether(slot),
                neuron::action::TetherMode::Wormhole => crate::wm::wormhole(slot),
            }
        }
        Intent::Echo => return "echo is handled by the dispatch executor".into(),
        // DIAL: prime the analog knob — the next hold slides it (app weave service).
        Intent::Dial(target) => {
            crate::beacon::prime_dial(*target);
            return "dial primed \u{2014} hold the trigger and slide (up = more)".into();
        }
        // CONTROL CENTER: prime the system-state glance — the next hold opens it.
        Intent::Control => {
            crate::beacon::request_instrument(6);
            return "control center primed \u{2014} hold the trigger to glance".into();
        }
        _ => {}
    }
    let mut cursor = neuron::intent::ProcessProfileCursor;
    neuron::intent::run_shared_intent(devices, &mut cursor, intent)
        .unwrap_or_else(|| "instrument routed".into())
}

fn apply_profile_live(
    devices: &mut neuron::device::DeviceSession<'_>,
    name: &str,
    persist: bool,
) -> Result<ProfileApplyResult, String> {
    if neuron::writes::writes_paused() {
        return Err("writes paused".into());
    }
    let mut profile =
        neuron::profile::Profile::load(name).map_err(|e| format!("profile '{name}': {e}"))?;
    profile.persist = persist;
    // paint_lighting = false: the GUI streams the profile's lighting stack via its live compositor
    // (on_apply_profile), so painting it here too would fight that stream for the device and stall.
    // Park every bridged host writer for the write batch — each setter is read-back verified, and
    // a streaming lighting frame can clobber a verify reply (the same race that blanked readouts).
    let _gates = crate::host::io_gate_all();
    let report = profile.apply_with_session(devices, false);
    neuron::profile::set_active(name);
    Ok(ProfileApplyResult {
        name: name.to_string(),
        summary: format!("applied '{name}': {}", report.summary()),
        policy: report.gaming_mode,
    })
}

/// Publish the current held-layer set into the status (so the header SHIFT pill reflects live
/// HyperShift). Only posts when it changed (cheap on the hot path).
fn publish_held(
    rt: &std::cell::RefCell<neuron::controls::Runtime>,
    status: &Arc<Mutex<LiveStatus>>,
    weak: &slint::Weak<AppWindow>,
) {
    let held: String = {
        let r = rt.borrow();
        let mut v: Vec<&str> = r.engine.held_layers().collect();
        v.sort_unstable();
        v.join("+")
    };
    // mirror "hypershift held?" to the overlay-readable atomic every tick (cheap, lock-free).
    HYPERSHIFT_HELD.store(
        held.split('+').any(|l| l == "hypershift"),
        Ordering::Relaxed,
    );
    // and "ANY layer held?" into the lighting engine's hold feed (the `modeheld` layer's truth).
    LAYER_HELD.store(!held.is_empty(), Ordering::Relaxed);
    push_hold_state();
    let changed = {
        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if s.held_layers != held {
            s.held_layers = held;
            true
        } else {
            false
        }
    };
    if changed {
        post_status(weak, status);
    }
}

/// The glue calls this after a profile apply so the live thread picks up the new suppression policy.
/// Delegates to the ONE shared carrier in `neuron::hook` (the same one the CLI daemon drives) — no
/// GUI-local copy that could drift from the CLI's view of the active profile's `ApplyReport`.
pub fn set_gaming_policy(policy: neuron::writes::GamingMode) {
    neuron::hook::set_policy(policy);
    let _ = send_live(LiveCommand::ReconcileGamingHook);
}

/// (Re)install or uninstall the gaming-mode hook to match the current shared policy. Thin wrapper
/// over `neuron::hook::reconcile`, which is idempotent and won't thrash an already-installed hook.
/// Portable: `reconcile` is itself cfg-seamed (installs the LL hook on Windows, keeps the desired-
/// policy carrier coherent + installs nothing off-Windows), so this helper compiles on all targets.
fn install_gaming_hook(slot: &mut Option<neuron::hook::Hook>) {
    neuron::hook::reconcile(slot);
}

/// Post the current status snapshot to the UI thread. The closure must be `Send`, so it carries a
/// plain `LiveStatus` (cloned out of the mutex) and reaches the view through the weak handle.
fn post_status(weak: &slint::Weak<AppWindow>, status: &Arc<Mutex<LiveStatus>>) {
    let snap = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            let st = app.global::<State>();
            st.set_runtime_active(true);
            st.set_hypershift_on(!snap.held_layers.is_empty());
            if !snap.focused_app.is_empty() {
                st.set_focused_app(snap.focused_app.clone().into());
                crate::glue::note_focused_app(&app, &snap.focused_app);
            }
            if !snap.last_trigger.is_empty() {
                st.set_live_last_trigger(snap.last_trigger.clone().into());
                st.set_live_last_action(snap.last_action.clone().into());
                st.set_live_fired(snap.fired as i32);
                // NO status-line write: live telemetry has its own dedicated strip slot, and
                // stomping the status line erased deliberate feedback ("binding added") with
                // background scroll-wheel noise before the user could read it.
            }
            // a live ProfileSwitch/Cycle moved the cursor — mirror it into the header pill,
            // the GUI runtime, and the Profiles panel highlight. Read the process-wide cell
            // NOW, not the snapshot: `snap.active_profile` is a cache refreshed only when a
            // trigger fires, so after a delete/apply cleared the cell, a later status post
            // (tick, focus change) would re-assert the deleted profile from the stale copy.
            // The cell is the ONE cursor; display time reads display truth.
            let live_cursor = neuron::profile::active();
            if !live_cursor.is_empty() {
                crate::glue::note_live_profile(&app, &live_cursor);
            }
            st.set_live_held(snap.held_layers.clone().into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── the mic-tap detector, END TO END ────────────────────────────────────────────────────────
    //
    // These drive the REAL detector logic from `live_tick` — `own_edge && !consume_self_write_on_edge()`
    // over the REAL process-global self-write window — through the REAL timing sequences, which is
    // where every actual bug in this path has lived: the detector samples a ~400ms-refreshed cache
    // every ~50ms, so neuron's own writes surface LATE, out of step, and sometimes not at all. Four
    // successive designs each passed narrower unit tests and still fired phantom taps (or swallowed
    // real ones) against that timing. A phantom tap runs arbitrary user-bound actions, so these are
    // the tests that matter. (There is deliberately NO isolated pure-decision test: the decision is a
    // two-liner inlined in the loop, and a separate test of a hand-fed version was, twice now, a
    // fiction that misled review — so the contract is pinned only where it actually runs.)

    /// Serializes these tests: the self-write window is process-global.
    static MIC_SEQ_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// One cache sample through the EXACT detector logic `live_tick` runs: an edge is a change in our
    /// own cache stream; an edge fires MicTap's effects unless it's our own write surfacing (which
    /// also CLOSES the window). Advances the own-stream baseline. Returns whether the effects fire.
    fn detect(last: &mut Option<bool>, now: bool) -> bool {
        let own_edge = last.is_some_and(|prev| prev != now);
        let fire = own_edge && !neuron::mic_state::consume_self_write_on_edge();
        *last = Some(now);
        fire
    }

    #[test]
    fn seq_quick_momentary_through_a_lagging_cache_never_fires_a_phantom() {
        // A push-to-talk TAP: press writes, release writes, both inside ONE ~400ms cache window. The
        // cache can then surface the INTERMEDIATE muted state late — an edge that looks exactly like
        // a physical tap but is entirely neuron's own doing. Neither our-write edge may fire.
        let _g = MIC_SEQ_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        neuron::mic_state::reset_self_mute_write();
        let mut last = Some(false); // resting: unmuted, and the detector has seen that
        neuron::mic_state::note_self_mute_write(); // press (window opens)
        neuron::mic_state::note_self_mute_write(); // release — window re-armed to now
        assert!(!detect(&mut last, false), "stale pre-write sample: no edge, no fire");
        assert!(
            !detect(&mut last, true),
            "the INTERMEDIATE hold surfacing late is OUR edge — consumed, no phantom"
        );
        // The window closed on that edge; the cache settling back to `false` is a SECOND edge. With
        // only one un-consumed write left in a real momentary (press+release = two writes), a fresh
        // note stands in for the release's own late edge.
        neuron::mic_state::note_self_mute_write();
        assert!(!detect(&mut last, false), "the release's edge is ours too");
    }

    #[test]
    fn seq_a_delayed_edge_beyond_a_fixed_window_is_still_ours() {
        // THE finding this rewrite fixes. Suppression closes on the EDGE, not a wall clock — so even
        // if the cache is stalled far past any fixed duration, our write's edge (whenever it finally
        // surfaces) is still attributed to us. A fixed-timer window would have fired a phantom here.
        let _g = MIC_SEQ_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        neuron::mic_state::reset_self_mute_write();
        let mut last = Some(false);
        neuron::mic_state::note_self_mute_write(); // our write
        for _ in 0..50 {
            assert!(!detect(&mut last, false), "long stall — cache hasn't moved, no edge");
        }
        assert!(!detect(&mut last, true), "the write's edge, however delayed, is consumed as ours");
        assert!(detect(&mut last, false), "and the NEXT edge is external again — fires");
    }

    #[test]
    fn seq_an_external_change_with_no_window_fires_once() {
        // The case the detector still exists for: ANOTHER APP changed our mute. (A physical Seiren
        // tap does NOT come through here — hidwatch fires it from the HID edge directly.)
        let _g = MIC_SEQ_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        neuron::mic_state::reset_self_mute_write();
        let mut last = Some(false);
        assert!(detect(&mut last, true), "another app muted us — fire MicTap");
        assert!(!detect(&mut last, true), "…and only once — no change is not an edge");
        assert!(detect(&mut last, false), "it unmuting us fires again");
    }

    #[test]
    fn seq_a_non_edge_sample_never_consumes_the_window() {
        // The un-changed polls between our write and its cache edge must leave the window ARMED — a
        // design that consumed on every sample lost it before the edge arrived and fired a phantom.
        let _g = MIC_SEQ_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        neuron::mic_state::reset_self_mute_write();
        let mut last = Some(false);
        neuron::mic_state::note_self_mute_write(); // e.g. hidwatch bridging a hardware tap
        for _ in 0..8 {
            assert!(!detect(&mut last, false), "still lagging — no edge, window untouched");
        }
        assert!(!detect(&mut last, true), "our write's edge finally surfaces — suppressed");
    }

    /// LiveStatus defaults are inert (no fired triggers, no held layers) — the loop hasn't run.
    #[test]
    fn live_status_default_is_inert() {
        let s = LiveStatus::default();
        assert_eq!(s.fired, 0);
        assert!(s.last_trigger.is_empty());
        assert!(s.held_layers.is_empty());
    }

    /// Setting + reading the gaming policy round-trips through the process-global cell. This never
    /// installs a hook (no `hook::install`) — it only exercises the policy carrier the glue uses.
    #[test]
    fn gaming_policy_roundtrips() {
        let saved = neuron::hook::policy();
        set_gaming_policy(neuron::writes::GamingMode::from_profile(true, false, false, false));
        assert!(neuron::hook::policy().disable_alt_tab);
        // reset so we don't leak state into other tests.
        set_gaming_policy(neuron::writes::GamingMode::default());
        assert!(!neuron::hook::policy().any());
        neuron::hook::set_policy(saved);
    }

    /// `ProfileCycle` must step from the CURRENT profile, not a flat index. The GUI dispatch now
    /// delegates to the ONE shared `neuron::profile::cycle_index` (no GUI-local copy that could
    /// drift from the CLI daemon); this guards that the shared contract holds from the GUI's view.
    #[test]
    fn cycle_index_steps_from_current_with_wraparound() {
        use neuron::profile::cycle_index;
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // forward from each position wraps c -> a.
        assert_eq!(cycle_index(&names, "a", 1), 1, "a +1 -> b");
        assert_eq!(cycle_index(&names, "b", 1), 2, "b +1 -> c");
        assert_eq!(cycle_index(&names, "c", 1), 0, "c +1 wraps -> a");
        // backward from each position wraps a -> c.
        assert_eq!(cycle_index(&names, "a", -1), 2, "a -1 wraps -> c");
        assert_eq!(cycle_index(&names, "c", -1), 1, "c -1 -> b");
        // unknown/empty cursor (fresh start) begins at index 0, NOT a flat offset.
        assert_eq!(cycle_index(&names, "", 1), 0, "empty cursor starts at 0");
        assert_eq!(
            cycle_index(&names, "nonexistent", -1),
            0,
            "unknown cursor starts at 0"
        );
    }

    // ── LiveCtx wiring tests ──────────────────────────────────────────────────────────────────
    // These drive `live_edge`/`live_tick` directly against a `LiveCtx::for_tests` — never through
    // `run_worker` (which is LIVE-PATH ONLY: it arms input and would need a real listener). Every
    // test that builds a `LiveCtx` wraps itself in `cwd_guard` because `LiveCtx::for_tests` calls
    // `controls::build_runtime()`, which reads several run-root config files — without the
    // guard a test could race another test's NEURON_RUN_DIR swap and read garbage (see testsupport.rs).

    /// A minimal `profiles/*.rules.toml` sidecar binding one [`Trigger::AppFocus`] to
    /// [`neuron::action::Action::Echo`] — kept as one helper so every wiring test below writes the
    /// identical, known-good shape (tag = "kind"/"type", the [[rules]] array-of-tables nesting).
    fn marker_rule_toml(marker: &str) -> String {
        format!(
            "[[rules]]\n\n[rules.trigger]\nkind = \"app-focus\"\napp = \"{marker}\"\n\n[rules.action]\ntype = \"echo\"\n"
        )
    }

    fn marker_trigger(marker: &str) -> Trigger {
        Trigger::AppFocus {
            app: marker.to_string(),
        }
    }

    /// THE missing wiring guarantee: a config file edit followed by `request_reload` must actually
    /// change what the live engine resolves. Builds a `LiveCtx` against an on-disk config with NO
    /// rule for our marker trigger, confirms the freshly-built engine doesn't resolve it, THEN
    /// writes a sidecar that binds it, sends `LiveCommand::Reload`, drives one `live_tick`, and
    /// confirms the rebuilt engine now resolves it — proving `live_tick`'s reload block actually
    /// re-reads disk instead of e.g. only clearing held state.
    #[test]
    fn reload_consumes_a_config_edit() {
        let _g = crate::testsupport::cwd_guard("dispatch_reload_consumes");
        let trigger = marker_trigger("zzz-dispatch-reload-marker.exe");
        let reg = neuron::registry::Registry { devices: Vec::new() };
        let (tx, rx) = channel();
        let mut ctx = LiveCtx::for_tests(&reg, rx);
        assert!(
            ctx.rt.borrow().engine.resolve(&trigger).is_empty(),
            "before any sidecar exists, the marker trigger must resolve to nothing"
        );

        std::fs::create_dir_all("profiles").unwrap();
        std::fs::write(
            "profiles/test.rules.toml",
            marker_rule_toml("zzz-dispatch-reload-marker.exe"),
        )
        .unwrap();
        tx.send(LiveCommand::Reload).unwrap();
        live_tick(&mut ctx);

        assert_eq!(
            ctx.rt.borrow().engine.resolve(&trigger).len(),
            1,
            "a live_tick after Reload must rebuild the engine from the just-edited config"
        );
    }

    /// A held momentary mic / held key remap / held sniper DPI / held turbo must not survive a
    /// config reload — the reload block's whole point is that a config swap can't strand a physical
    /// hold. Seeds one entry into each held-state map (turbo via `TurboRuntime::start`, timed to
    /// already be "due"), sends `Reload`, drives one `live_tick`, and confirms every map is empty —
    /// for turbos (whose held set has no public inspector) by proving a MANUAL tick that WOULD fire
    /// a still-held turbo instead fires nothing.
    #[test]
    fn reload_clears_held_state() {
        let _g = crate::testsupport::cwd_guard("dispatch_reload_clears");
        let reg = neuron::registry::Registry { devices: Vec::new() };
        let (tx, rx) = channel();
        let mut ctx = LiveCtx::for_tests(&reg, rx);
        let trigger = marker_trigger("zzz-dispatch-seed-marker.exe");

        ctx.momentary.borrow_mut().insert(
            trigger.clone(),
            (Some("neuron-test-nonexistent-mic".to_string()), false),
        );
        ctx.held_keys
            .borrow_mut()
            .insert(trigger.clone(), vec![0x41]);
        ctx.sniper.borrow_mut().insert(trigger.clone(), (800, 0));
        ctx.turbos.borrow_mut().start(vec![neuron::executor::TurboStart {
            trigger: trigger.clone(),
            action: neuron::action::Action::Echo,
            cps: 100, // clamped max cps -> ~10ms interval
        }]);
        // let the seeded turbo actually come due before we reload, so the post-reload check below
        // (a manual tick firing nothing) is proof of CLEARING, not just of not-yet-due.
        std::thread::sleep(std::time::Duration::from_millis(30));

        tx.send(LiveCommand::Reload).unwrap();
        live_tick(&mut ctx);

        assert!(
            ctx.momentary.borrow().is_empty(),
            "reload must clear momentary mic holds"
        );
        assert!(
            ctx.held_keys.borrow().is_empty(),
            "reload must clear held key remaps"
        );
        assert!(
            ctx.sniper.borrow().is_empty(),
            "reload must clear sniper holds"
        );

        struct CountIntents(u32);
        impl neuron::executor::IntentRunner for CountIntents {
            fn run_intent(&mut self, _intent: &neuron::action::Intent) -> String {
                self.0 += 1;
                String::new()
            }
        }
        let mut counter = CountIntents(0);
        ctx.turbos
            .borrow_mut()
            .tick(&mut ctx.exec.borrow_mut(), &mut counter);
        assert_eq!(
            counter.0, 0,
            "reload must clear the held turbo (a due-but-still-held turbo would have fired here)"
        );
    }

    /// `inject_trigger`'s queue (drained here as `LiveCommand::Inject`) dispatches through the SAME
    /// engine + executor as a hardware edge — not a separate hard-wired path. Binds our marker
    /// trigger to `Action::Echo` (whose fresh, empty-history report — "nothing to echo yet" — is a
    /// precise, zero-side-effect string to assert on), injects it, drives one `live_tick`, and
    /// confirms the shared `LiveStatus` recorded exactly that fire.
    #[test]
    fn inject_fires_through_the_same_engine() {
        let _g = crate::testsupport::cwd_guard("dispatch_inject_fires");
        let trigger = marker_trigger("zzz-dispatch-inject-marker.exe");
        std::fs::create_dir_all("profiles").unwrap();
        std::fs::write(
            "profiles/test.rules.toml",
            marker_rule_toml("zzz-dispatch-inject-marker.exe"),
        )
        .unwrap();
        let reg = neuron::registry::Registry { devices: Vec::new() };
        let (tx, rx) = channel();
        let mut ctx = LiveCtx::for_tests(&reg, rx);

        tx.send(LiveCommand::Inject(trigger)).unwrap();
        live_tick(&mut ctx);

        let s = ctx
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            s.last_action, "nothing to echo yet",
            "the injected trigger must fire through the real executor (fresh history -> this exact report)"
        );
        assert_eq!(s.fired, 1, "exactly one dispatch must be recorded");
    }

    /// `LiveCommand::ToggleHyperShift` must flip the SOFTWARE latch, have `live_tick` reconcile it
    /// into the engine's real held-layer set (the one true source `publish_held` reads from), and
    /// reply on the given channel with the new state — all in one `live_tick` call.
    #[test]
    fn hypershift_latch_reconciles() {
        let _g = crate::testsupport::cwd_guard("dispatch_hypershift_latch");
        let reg = neuron::registry::Registry { devices: Vec::new() };
        let (tx, rx) = channel();
        let mut ctx = LiveCtx::for_tests(&reg, rx);
        let (reply_tx, reply_rx) = channel();

        tx.send(LiveCommand::ToggleHyperShift(reply_tx)).unwrap();
        live_tick(&mut ctx);

        let replied = reply_rx
            .try_recv()
            .expect("ToggleHyperShift must reply on its channel within the same tick");
        assert!(replied, "the first toggle latches the layer ON");
        assert!(
            ctx.rt.borrow().engine.is_held("hypershift"),
            "live_tick must reconcile the software latch into the engine's real held-layer set"
        );
    }

    /// A `LiveCommand` sender being dropped (the GUI thread tearing down `LiveRuntime`) must not
    /// panic `live_tick` — `Receiver::try_iter` on a disconnected channel simply ends, same as an
    /// empty one.
    #[test]
    fn tick_survives_command_channel_disconnect() {
        let _g = crate::testsupport::cwd_guard("dispatch_disconnect");
        let reg = neuron::registry::Registry { devices: Vec::new() };
        let (tx, rx) = channel();
        let mut ctx = LiveCtx::for_tests(&reg, rx);
        drop(tx);

        live_tick(&mut ctx); // must not panic

        assert_eq!(ctx.tick, 1, "the tick counter still advances");
    }

    // ── DispatchBreaker: pure logic, synthetic clocks, no real sleeps ────────────────────────────

    /// Faults spaced further apart than the window never accumulate — a rare, isolated respawn
    /// (e.g. a one-off device hiccup) must never trip the breaker, no matter how many happen over
    /// the life of the process.
    #[test]
    fn spaced_faults_never_trip() {
        let mut b = DispatchBreaker::new();
        let t0 = Instant::now();
        for i in 0..20 {
            b.record_fault(t0 + Duration::from_secs(i * (DISPATCH_BREAKER_WINDOW.as_secs() + 1)));
            assert!(!b.tripped(), "fault #{i} spaced past the window must not trip");
        }
    }

    /// A burst of faults inside the window trips the breaker exactly once `DISPATCH_BREAKER_MAX`
    /// is exceeded (the 5th fault within 30s, given the current MAX=4).
    #[test]
    fn burst_trips_the_breaker() {
        let mut b = DispatchBreaker::new();
        let t0 = Instant::now();
        for i in 0..DISPATCH_BREAKER_MAX {
            b.record_fault(t0 + Duration::from_millis(i as u64 * 10));
            assert!(!b.tripped(), "the {}th fault must not trip yet (MAX={})", i + 1, DISPATCH_BREAKER_MAX);
        }
        b.record_fault(t0 + Duration::from_millis(DISPATCH_BREAKER_MAX as u64 * 10));
        assert!(b.tripped(), "exceeding MAX faults inside the window must trip the breaker");
    }

    /// The resume sleep doubles with each fault still inside the window, capped at
    /// `DISPATCH_BREAKER_MAX_SLEEP` — a building storm backs off before it gives up outright.
    #[test]
    fn backoff_doubles_and_caps() {
        let mut b = DispatchBreaker::new();
        let t0 = Instant::now();
        let s1 = b.record_fault(t0);
        let s2 = b.record_fault(t0 + Duration::from_millis(1));
        let s3 = b.record_fault(t0 + Duration::from_millis(2));
        assert_eq!(s1, DISPATCH_BREAKER_BASE_SLEEP);
        assert_eq!(s2, DISPATCH_BREAKER_BASE_SLEEP * 2);
        assert_eq!(s3, DISPATCH_BREAKER_BASE_SLEEP * 4);
        // hammer far more faults into the same instant than it takes to trip — even ignoring the
        // trip, the sleep this returns must never exceed the ceiling.
        let mut last = Duration::ZERO;
        for _ in 0..30 {
            last = b.record_fault(t0);
        }
        assert_eq!(last, DISPATCH_BREAKER_MAX_SLEEP, "backoff must cap, never grow unbounded");
    }

    /// `reset` (the user-initiated-Reload path) forgets every past fault and un-trips — a fresh
    /// storm budget, not a partially-primed one.
    #[test]
    fn reset_forgets_history_and_untrips() {
        let mut b = DispatchBreaker::new();
        let t0 = Instant::now();
        for i in 0..=DISPATCH_BREAKER_MAX {
            b.record_fault(t0 + Duration::from_millis(i as u64));
        }
        assert!(b.tripped());
        b.reset();
        assert!(!b.tripped(), "reset must un-trip immediately");
        // and the fault history is really gone, not just the flag: it takes a full fresh burst to
        // trip again, not one more fault riding on the old count.
        let t1 = t0 + Duration::from_secs(60);
        for i in 0..DISPATCH_BREAKER_MAX {
            b.record_fault(t1 + Duration::from_millis(i as u64));
            assert!(!b.tripped(), "post-reset fault #{i} alone must not re-trip");
        }
    }

    /// The revival seam: `service_while_halted` is what the respawn loop calls instead of
    /// reopening the listener once tripped. This proves the loop-structure guarantee the task
    /// hinges on — a tripped breaker MUST keep servicing `LiveCommand`s (never just block deaf on
    /// the listener), because `Reload` is the only way out and it arrives on this same channel. A
    /// non-Reload command is dropped (its sender already bounds its own wait via `recv_timeout`),
    /// but the loop keeps draining and a subsequent `Reload` still revives it.
    #[test]
    fn halted_breaker_services_commands_and_revives_on_reload() {
        let mut b = DispatchBreaker::new();
        let t0 = Instant::now();
        for i in 0..=DISPATCH_BREAKER_MAX {
            b.record_fault(t0 + Duration::from_millis(i as u64));
        }
        assert!(b.tripped(), "setup: the breaker must be tripped before this test proves anything");

        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));

        // a command that ISN'T Reload must be drained (not left clogging the channel) and must
        // NOT revive the breaker on its own.
        tx.send(LiveCommand::ReconcileGamingHook).unwrap();
        tx.send(LiveCommand::Reload).unwrap();

        let revived = service_while_halted(&rx, &stop, &mut b);

        assert!(revived, "a Reload arriving while halted must revive service_while_halted");
        assert!(!b.tripped(), "revival must reset the breaker so the next fault gets a fresh budget");
    }

    /// `stop` being set while halted must return promptly (the app is shutting down, not asking
    /// for a revival) instead of blocking forever waiting for a `Reload` that will never come.
    #[test]
    fn halted_breaker_exits_promptly_on_stop() {
        let mut b = DispatchBreaker::new();
        b.record_fault(Instant::now());
        b.tripped = true; // force-trip without needing a full burst — this test is about `stop`, not the storm math

        let (_tx, rx) = channel::<LiveCommand>();
        let stop = Arc::new(AtomicBool::new(true)); // already asked to stop

        let revived = service_while_halted(&rx, &stop, &mut b);

        assert!(!revived, "a stop request while halted must exit without waiting for Reload");
    }

    // ── model-based random walker over the live dispatch state machine ──────────────────────────
    //
    // Everything above drives `live_edge`/`live_tick` with ONE hand-picked scenario per test. This
    // section instead throws a proptest-shrinkable SEQUENCE of arbitrary operations at the same seam
    // — raw multi-device edge chords (including down-without-up, duplicated-down, up-without-down),
    // ticks, reloads (the on-disk config-edit trick `reload_consumes_a_config_edit` established),
    // injects, the HyperShift latch toggle, and a hardware-free "ghost hold" seed (the direct-insert
    // trick `reload_clears_held_state` established, generalized to fire at ANY point in the walk, not
    // just once before a single reload) — and checks the same standing invariants after every step.
    mod walker {
        use super::*;
        use proptest::prelude::*;
        use proptest::strategy::{BoxedStrategy, Just, Union};
        use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

        /// One operation the walker can apply to a live `LiveCtx`. A plain data enum (not a closure)
        /// so a failing `Vec<Op>` SHRINKS — proptest can drop/simplify ops and still replay the exact
        /// same seam calls.
        #[derive(Clone, Debug)]
        enum Op {
            /// Toggle one (device, button) in/out of that device's currently-held set and feed the
            /// resulting full report through `live_edge` — `HoldEdges` does the down/up diffing, so
            /// this alphabet alone covers down-without-up (never toggle off), up-without-down (toggle
            /// off a button never pressed — a no-op edge), duplicated downs (toggle on twice in a row
            /// — idempotent, no re-fire), and interleaved devices (two independent per-device sets).
            RawEdge { device: u8, button: u8, pressed: bool },
            Tick,
            /// A short burst of extra ticks back-to-back (turbo/throttle/mic-tap-cadence stress).
            Burst(u8),
            Reload,
            /// Index into a small fixed pool of triggers (a bound one, an unbound one, the AppFocus
            /// marker) — see `inject_pool` in `run_episode`.
            Inject(u8),
            ToggleHyperShift,
            /// The hardware-free "ghost hold" seed: directly plants one entry into EACH of the three
            /// held-state maps (mirroring `reload_clears_held_state`'s seeding technique) without
            /// needing a real mic/device — so the "no stranded held key" law gets exercised even when
            /// no real hardware answers `sniper_press`/`momentary_press` in this environment.
            SeedGhostHold,
            NoOp,
        }

        // ApplyProfile was DROPPED from the alphabet: exercising it for real needs an on-disk
        // `Profile` TOML whose full field shape (device targets, lighting stack, etc.) this task
        // didn't have budget to verify byte-for-byte against `profile.rs` — a wrong shape would fail
        // to compile or degrade to a silent `Err` every time, adding a no-op op for real risk. The
        // `LiveCommand::ApplyProfile` plumbing itself (reply channel, status post) is still reachable
        // through the existing `apply_profile`/`ApplyProfile` unit coverage elsewhere in this file.

        /// A tiny deterministic PRNG (splitmix64) — used ONLY by the plain-`#[test]` curiosity sweep
        /// below, which drives its own bandit sampling outside proptest's generator. No wall clock, no
        /// external RNG crate, fully reproducible from one fixed seed.
        struct Lcg(u64);
        impl Lcg {
            fn next_u64(&mut self) -> u64 {
                self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
                let mut z = self.0;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
                z ^ (z >> 31)
            }
            fn next_range(&mut self, n: u64) -> u64 {
                if n == 0 { 0 } else { self.next_u64() % n }
            }
            fn next_bool(&mut self) -> bool {
                self.next_u64() & 1 == 1
            }
        }

        /// Map one op "category" index (0..=6, matching the mask bits below) to a concrete `Op` using
        /// the LCG — the curiosity sweep's hand-rolled twin of `op_category_strategies`' proptest
        /// generators, since that sweep runs outside proptest's own generator.
        fn op_from_category(cat: usize, rng: &mut Lcg) -> Op {
            match cat {
                0 => Op::RawEdge {
                    device: rng.next_range(2) as u8,
                    button: rng.next_range(5) as u8,
                    pressed: rng.next_bool(),
                },
                1 => Op::Tick,
                2 => Op::Reload,
                3 => Op::Inject(rng.next_range(3) as u8),
                4 => Op::ToggleHyperShift,
                5 => Op::SeedGhostHold,
                6 => Op::Burst(1 + rng.next_range(4) as u8),
                _ => Op::NoOp,
            }
        }

        /// The op alphabet as (mask bit, proptest strategy) pairs — SWARM TESTING: a walk episode
        /// picks a random SUBSET of these bits (see `walker_strategy`) so some episodes never Reload,
        /// some hammer only raw edges, etc. Feature-absence combinations a uniform mix rarely samples.
        fn op_category_strategies() -> Vec<(u8, BoxedStrategy<Op>)> {
            vec![
                (
                    1u8,
                    (0u8..2, 0u8..5, any::<bool>())
                        .prop_map(|(device, button, pressed)| Op::RawEdge { device, button, pressed })
                        .boxed(),
                ),
                (2u8, Just(Op::Tick).boxed()),
                (4u8, Just(Op::Reload).boxed()),
                (8u8, (0u8..3u8).prop_map(Op::Inject).boxed()),
                (16u8, Just(Op::ToggleHyperShift).boxed()),
                (32u8, Just(Op::SeedGhostHold).boxed()),
                (64u8, (1u8..=4u8).prop_map(Op::Burst).boxed()),
            ]
        }

        /// One op strategy that only ever produces ops whose category bit is set in `mask` — a NoOp
        /// fallback keeps `Union` non-empty for an all-zero mask (still a legal, if boring, episode).
        fn ops_for_mask(mask: u8) -> BoxedStrategy<Op> {
            let mut branches: Vec<(u32, BoxedStrategy<Op>)> = op_category_strategies()
                .into_iter()
                .filter(|(bit, _)| mask & bit != 0)
                .map(|(_, s)| (1u32, s))
                .collect();
            if branches.is_empty() {
                branches.push((1, Just(Op::NoOp).boxed()));
            }
            Union::new_weighted(branches).boxed()
        }

        /// The proptest strategy driving the main walker: pick an arbitrary swarm mask, then a bounded
        /// op sequence drawn only from that mask's categories. `Vec<Op>` shrinks on its own (proptest
        /// drops/simplifies elements), so a failure minimizes to the smallest reproducing sequence.
        fn walker_strategy() -> impl Strategy<Value = (u8, Vec<Op>)> {
            any::<u8>().prop_flat_map(|mask| {
                proptest::collection::vec(ops_for_mask(mask), 0usize..120)
                    .prop_map(move |ops| (mask, ops))
            })
        }

        /// A minimal but REAL rule set written to the pinned run-dir's `profiles/` sidecar so raw
        /// edges actually resolve to actions through the one engine, exactly like a hardware daemon:
        ///   * usage 1 -> `Action::Sniper` (a held-style rule; DPI writes silently no-op against the
        ///     empty test registry — no real device, no real write, but the press/release edge path,
        ///     including `sniper_dpi_for` resolution, is exercised for real).
        ///   * usage 3 -> `Action::Key` (a held-style rule; `press_and_hold`/`release_keys` run for
        ///     real but are gated no-ops because `arm_input` is never flipped on in these tests — see
        ///     the module's input-safety invariant doc at the top of this file).
        ///   * usage 4 -> `Action::Echo` (a plain one-shot action, so a base-layer dispatch fires too).
        ///   * the AppFocus marker -> `Action::Echo` (the `Inject` op's bound-trigger case).
        ///
        /// Deliberately NO on-disk `Action::MomentaryMic` rule: `device: None` resolves to the REAL
        /// default capture endpoint, and firing it through a real edge would flip the ACTUAL system
        /// mic mute on the machine running this test — the momentary held-map is instead exercised via
        /// `SeedGhostHold` (see its doc), which never touches audio hardware.
        fn write_walker_rules() {
            let rules = vec![
                neuron::engine::Rule::new(
                    Trigger::Input { page: 0x09, usage: 1, pid: None },
                    neuron::action::Action::Sniper { dpi: 400 },
                ),
                neuron::engine::Rule::new(
                    Trigger::Input { page: 0x09, usage: 3, pid: None },
                    neuron::action::Action::Key { key: "f".into() },
                ),
                neuron::engine::Rule::new(
                    Trigger::Input { page: 0x09, usage: 4, pid: None },
                    neuron::action::Action::Echo,
                ),
                neuron::engine::Rule::new(
                    marker_trigger("zzz-dispatch-walker-marker.exe"),
                    neuron::action::Action::Echo,
                ),
            ];
            std::fs::create_dir_all("profiles").unwrap();
            let doc = neuron::engine::RuleDoc { rules };
            std::fs::write("profiles/walker.rules.toml", toml::to_string(&doc).unwrap()).unwrap();
        }

        /// A cheap, cheaply-observable digest of "what state is the seam in right now" — the three
        /// held-map sizes, whether the HyperShift latch is armed, the held-layer count, and the fired
        /// counter (mod 256) — fed to a `LogosStream` as the curiosity signal, and used directly by
        /// `curiosity_sweep_finds_diverse_states` to count how many distinct states a sweep visited.
        fn state_digest(ctx: &LiveCtx) -> [u8; 6] {
            let momentary = ctx.momentary.borrow().len().min(255) as u8;
            let held_keys = ctx.held_keys.borrow().len().min(255) as u8;
            let sniper = ctx.sniper.borrow().len().min(255) as u8;
            let layers = ctx.rt.borrow().engine.held_layers().count().min(255) as u8;
            let latch = ctx.hypershift_latch as u8;
            let fired = (ctx
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .fired
                % 256) as u8;
            [momentary, held_keys, sniper, layers, latch, fired]
        }

        /// Standing invariants checked after EVERY op (see the task doc above for the full list):
        /// no panic (proven by returning here at all), input never armed, held-map sizes never run
        /// away, and — generalizing `reload_clears_held_state` to an ARBITRARY preceding op history —
        /// immediately after any `Reload` every held-state map (momentary/key-hold/sniper) is empty
        /// and no stranded turbo is due. `context` is folded into every assertion message so a
        /// shrunk-failure report carries the recent op trail + cumulative curiosity surprise.
        fn check_invariants(ctx: &LiveCtx, context: &str, after_reload: bool) {
            assert!(
                !neuron::action::input_armed(),
                "input must never arm during the walk ({context})"
            );
            assert!(ctx.momentary.borrow().len() < 1000, "runaway momentary growth ({context})");
            assert!(ctx.held_keys.borrow().len() < 1000, "runaway held-key growth ({context})");
            assert!(ctx.sniper.borrow().len() < 1000, "runaway sniper growth ({context})");
            if after_reload {
                assert!(
                    ctx.momentary.borrow().is_empty(),
                    "a reload must clear momentary holds ({context})"
                );
                assert!(
                    ctx.held_keys.borrow().is_empty(),
                    "a reload must clear held key remaps ({context})"
                );
                assert!(
                    ctx.sniper.borrow().is_empty(),
                    "a reload must clear sniper holds ({context})"
                );
                struct CountIntents(u32);
                impl neuron::executor::IntentRunner for CountIntents {
                    fn run_intent(&mut self, _intent: &neuron::action::Intent) -> String {
                        self.0 += 1;
                        String::new()
                    }
                }
                let mut counter = CountIntents(0);
                ctx.turbos.borrow_mut().tick(&mut ctx.exec.borrow_mut(), &mut counter);
                assert_eq!(
                    counter.0, 0,
                    "no stranded turbo may fire right after a reload ({context})"
                );
            }
        }

        /// Apply one `Op` to `ctx`, mutating `active` (the walker's own per-device down-set bookkeeping
        /// — the ONE piece of state the walker itself owns, everything else is real seam state).
        fn apply_op(
            ctx: &mut LiveCtx,
            op: &Op,
            active: &mut HashMap<u8, BTreeSet<(u16, u16)>>,
            tx: &Sender<LiveCommand>,
            inject_pool: &[Trigger],
            ghost_marker: &Trigger,
        ) {
            match op {
                Op::NoOp => {}
                Op::Tick => {
                    live_tick(ctx);
                }
                Op::Burst(n) => {
                    for _ in 0..*n {
                        live_tick(ctx);
                    }
                }
                Op::RawEdge { device, button, pressed } => {
                    let device: u8 = *device % 2;
                    let usage = 1u16 + (*button as u16 % 5);
                    let set = active.entry(device).or_default();
                    if *pressed {
                        set.insert((0x09, usage));
                    } else {
                        set.remove(&(0x09, usage));
                    }
                    let pid = if device == 0 { "00a8" } else { "0221" };
                    let ev = ControlEvent {
                        pid: pid.into(),
                        hits: set.iter().cloned().collect(),
                        raw: Vec::new(),
                    };
                    live_edge(ctx, &ev);
                }
                Op::Reload => {
                    let _ = tx.send(LiveCommand::Reload);
                    live_tick(ctx);
                }
                Op::Inject(idx) => {
                    let t = inject_pool[*idx as usize % inject_pool.len()].clone();
                    let _ = tx.send(LiveCommand::Inject(t));
                    live_tick(ctx);
                }
                Op::ToggleHyperShift => {
                    let (reply_tx, _reply_rx) = channel();
                    let _ = tx.send(LiveCommand::ToggleHyperShift(reply_tx));
                    live_tick(ctx);
                }
                Op::SeedGhostHold => {
                    ctx.momentary.borrow_mut().insert(
                        ghost_marker.clone(),
                        (Some("neuron-test-nonexistent-mic".to_string()), false),
                    );
                    ctx.held_keys.borrow_mut().insert(ghost_marker.clone(), vec![0x41]);
                    ctx.sniper.borrow_mut().insert(ghost_marker.clone(), (800, 0));
                }
            }
        }

        /// Run one full walk episode: build a fresh `LiveCtx` in a pinned temp run-dir, seed the real
        /// rule set, apply every op (checking invariants after each and feeding a `LogosStream` a state
        /// digest), then send a final `Reload` + tick and assert the "no stranded held key" law — a
        /// dropped up-edge must never survive a reload, no matter how the episode got there. Returns
        /// the episode's total curiosity surprise and the digest trail (the curiosity sweep uses both).
        fn run_episode(mask: u8, ops: &[Op]) -> (f32, Vec<[u8; 6]>) {
            let _g = crate::testsupport::cwd_guard("dispatch_walker");
            write_walker_rules();
            let reg = neuron::registry::Registry { devices: Vec::new() };
            let (tx, rx) = channel();
            let mut ctx = LiveCtx::for_tests(&reg, rx);

            let ghost_marker = marker_trigger("zzz-dispatch-walker-ghost.exe");
            let inject_pool = [
                marker_trigger("zzz-dispatch-walker-marker.exe"),
                Trigger::Input { page: 0x09, usage: 1, pid: None },
                Trigger::Input { page: 0x09, usage: 99, pid: None }, // deliberately unbound
            ];
            let mut active: HashMap<u8, BTreeSet<(u16, u16)>> = HashMap::new();
            let mut logos = neuron::logos::LogosStream::new();
            let mut total_surprise = 0.0f32;
            let mut digests = Vec::with_capacity(ops.len() + 1);
            let mut recent: VecDeque<String> = VecDeque::new();

            for (step, op) in ops.iter().enumerate() {
                apply_op(&mut ctx, op, &mut active, &tx, &inject_pool, &ghost_marker);
                recent.push_back(format!("{step}:{op:?}"));
                if recent.len() > 12 {
                    recent.pop_front();
                }
                let digest = state_digest(&ctx);
                total_surprise += logos.surprise_of(&digest);
                digests.push(digest);
                let context = format!(
                    "mask={mask:#04x}, recent ops: {recent:?}, cumulative surprise: {total_surprise:.2}"
                );
                check_invariants(&ctx, &context, matches!(op, Op::Reload));
            }

            // episode end: a final Reload + tick, then the "no stranded held key" law — whatever the
            // walk did, a reload afterward must leave every held-state map empty.
            let _ = tx.send(LiveCommand::Reload);
            live_tick(&mut ctx);
            let final_digest = state_digest(&ctx);
            total_surprise += logos.surprise_of(&final_digest);
            digests.push(final_digest);
            check_invariants(
                &ctx,
                &format!("mask={mask:#04x}, episode end, cumulative surprise: {total_surprise:.2}"),
                true,
            );

            (total_surprise, digests)
        }

        proptest! {
            // one `LiveCtx` (+ a pinned temp run-dir) per case, up to 120 ops each — keep it modest.
            #![proptest_config(ProptestConfig::with_cases(48))]

            /// THE flagship harness: an arbitrary, swarm-masked sequence of edges/ticks/reloads/
            /// injects/latch-toggles/ghost-holds must never panic, never arm input, and must never let
            /// a held state survive a reload — across every op-history proptest can construct, shrunk
            /// to the minimal failing sequence when it finds one.
            #[test]
            fn dispatch_state_machine_walker(case in walker_strategy()) {
                let (mask, ops) = case;
                run_episode(mask, &ops);
            }
        }

        /// A second, non-shrinking exploration pass: a simple deterministic bandit samples op
        /// CATEGORIES weighted by the curiosity (Logos surprise) each category mix earned in earlier
        /// episodes, biasing later episodes toward whatever kept producing novel states. This is a
        /// smoke floor around the real value (the invariant checker running under a self-steering
        /// sweep, not a fixed uniform mix) — it asserts the sweep visited a reasonably diverse set of
        /// held-map-size digests, not that the bandit converged to anything in particular.
        #[test]
        fn curiosity_sweep_finds_diverse_states() {
            const CATEGORIES: usize = 7; // RawEdge, Tick, Reload, Inject, ToggleHyperShift, SeedGhostHold, Burst
            const EPISODES: u32 = 64;
            const MIN_DISTINCT_STATES: usize = 12;

            let mut rng = Lcg(0xD1B54A32D192ED03);
            let mut weights = [1.0f64; CATEGORIES];
            let mut seen: HashSet<[u8; 6]> = HashSet::new();

            for _ in 0..EPISODES {
                let total_w: f64 = weights.iter().sum();
                let mut included: Vec<usize> = Vec::new();
                for (i, &w) in weights.iter().enumerate() {
                    let threshold = ((w / total_w) * CATEGORIES as f64).min(0.95);
                    let r = (rng.next_range(1_000_000) as f64) / 1_000_000.0;
                    if r < threshold {
                        included.push(i);
                    }
                }
                if included.is_empty() {
                    included.push(1); // Tick — always a safe, harmless fallback category
                }
                let len = rng.next_range(80) as usize; // keep the sweep light (64 episodes)
                let ops: Vec<Op> = (0..len)
                    .map(|_| {
                        let pick = included[rng.next_range(included.len() as u64) as usize];
                        op_from_category(pick, &mut rng)
                    })
                    .collect();
                let mask = included.iter().fold(0u8, |acc, &i| acc | (1u8 << i));

                let (surprise, digests) = run_episode(mask, &ops);
                seen.extend(digests);

                // reward every category this episode drew from, proportional to the surprise it found.
                let reward = (surprise as f64).max(0.01);
                for &i in &included {
                    weights[i] += reward / included.len() as f64;
                }
            }

            assert!(
                seen.len() >= MIN_DISTINCT_STATES,
                "the curiosity sweep should visit a diverse set of held-map-size states; saw only {} \
                 distinct digests out of {} — the bandit may have collapsed onto one op mix",
                seen.len(),
                EPISODES
            );
        }
    }
}
