//! LIVE DISPATCH — the headline. A device-event runtime on a dedicated worker thread, so GUI-bound
//! remaps fire LIVE without the CLI daemon.
//!
//! This is the GUI's port of the CLI daemon's `run_listen`/`fire_trigger` spine (see
//! `neuron-cli/src/main.rs`). On a worker thread it:
//!   * ARMS input synthesis on the live path (gated behind the GUI's safe-mode toggle), so bound
//!     keys/clicks/macros actually fire — remaps work out of the box;
//!   * builds the ONE unified [`Engine`] from every on-disk config (bindings/cast/hypershift/
//!     app-rules) via [`neuron::controls::build_runtime`];
//!   * installs the GamingMode `WH_KEYBOARD_LL` suppression hook on the same thread that pumps the
//!     Raw-Input message loop (an LL hook only fires while that thread pumps messages);
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
use neuron::engine::Trigger;
use neuron::executor::{DispatchExecutor, DispatchOutcome, IntentRunner, TurboRuntime};
use slint::ComponentHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    tx.is_some_and(|tx| tx.send(cmd).is_ok())
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
    rx.recv_timeout(Duration::from_secs(8))
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
        let handle = std::thread::Builder::new()
            .name("neuron-live-dispatch".into())
            .spawn(move || run_worker(weak, worker_stop, rx))
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

/// The worker body: arm input, install the gaming hook, build the engine, run the listen loop.
///
/// PLATFORM-NEUTRAL: every callee here is portable or cfg-seamed at its source —
/// [`neuron::controls::listen_until`] (Win32 Raw-Input pump on Windows, an inert tick-only loop
/// off-Windows), [`neuron::hook`] (LL gaming hook on Windows, inert stubs off-Windows), and
/// [`neuron::app_focus::AppFocusSwitch`] (foreground polling on Windows, `None` off-Windows). On a
/// non-Windows host the loop still arms/builds the engine and processes reload/inject/profile
/// commands on the tick — only hardware input edges are dormant (no source yet).
fn run_worker(weak: slint::Weak<AppWindow>, stop: Arc<AtomicBool>, live_rx: Receiver<LiveCommand>) {
    use neuron::controls::{self, HoldEdges, InputEdge, MIC_TAP};
    use std::cell::RefCell;

    // Build the ONE unified spine (bindings.toml + cast.toml + profiles/*.rules.toml + apps.toml).
    let rt = RefCell::new(controls::build_runtime());
    let exec = RefCell::new(DispatchExecutor::new());
    let reg = neuron::registry::Registry::load().unwrap_or(neuron::registry::Registry {
        devices: Vec::new(),
    });
    let devices = RefCell::new(neuron::device::DeviceSession::new(&reg));

    // Status shared with the UI: only post deltas (the loop runs hot, the UI updates on events).
    let status = Arc::new(Mutex::new(LiveStatus::default()));
    {
        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        s.held_layers = String::new();
    }
    post_status(&weak, &status);

    // Mic-tap detection reads the CACHED mic mute (refreshed off-thread by `beacon::audio_cache`).
    // Polling `VolumeCtl::get_mute()` inline here used to hang the whole dispatch loop when an audio
    // endpoint stalled (Core-Audio COM blocks indefinitely) — the dispatch-stall the flight log
    // caught. The cache means NO COM on this hot path; tap latency is the cache's ~400ms, fine for
    // a mute toggle. `None` until the cache warms / if no mic.
    let mut last_mute: Option<bool> = None;
    let mut switcher = neuron::app_focus::AppFocusSwitch::new();
    let mut tick = 0u32;
    let mut reload_pending = false;
    let mut injected = Vec::new();
    let mut hypershift_latch = false;
    let mut applied_hypershift_latch = false;
    let mut gaming_policy_dirty = false;

    // GamingMode suppression hook (Alt+Tab / Win / Alt+F4). Installed on THIS thread because an LL
    // keyboard hook only fires while its installing thread pumps messages — and `listen_until`'s
    // Raw-Input window pumps the message loop on this very thread. The policy comes from the active
    // profile's ApplyReport; we read it from the shared cell the glue updates on profile apply.
    let mut hook: Option<neuron::hook::Hook> = None;
    install_gaming_hook(&mut hook);

    // Edge-detector: a Razer report is the SET of buttons currently down, so we DIFF successive
    // reports into per-button down/up edges. This fixes multi-button chords (every newly-pressed
    // control dispatches, not just the first hit) and precise HyperShift release (only the input
    // that actually went up releases ITS layer — no blanket release_all on any empty report).
    let edges = RefCell::new(HoldEdges::new());
    // MOMENTARY MIC held state: trigger -> (mic device, mute-state to restore on release). A held
    // momentary action the stateless dispatch can't express — the edge loop owns its press/release.
    let momentary: RefCell<std::collections::HashMap<Trigger, (Option<String>, bool)>> =
        RefCell::new(std::collections::HashMap::new());
    // INPUT→KEY REMAP held state: trigger -> the output VKs currently held down. A key remap holds
    // its output key while the control is held (so a macro key / remapped button acts like the real
    // key — hold = hold, the OS auto-repeats), which the stateless tap-only action can't express.
    let held_keys: KeyHoldMap = RefCell::new(std::collections::HashMap::new());
    let turbos = RefCell::new(TurboRuntime::new());

    // ── THE IMMORTAL LISTENER ── this worker is the organ that fires every cast and remap; if it
    // dies, spellweaving "visually works but nothing happens" — the worst reliability lie the app
    // can tell. So: ESC must NOT stop it (esc_stops=false — ESC is the weave-cancel key and the
    // close-the-game-menu key; the old shared listener died on the first ESC of the session), and
    // any exit that wasn't an explicit stop (a contained fault, a dead listener window) is logged
    // to the flight ring and the listener is simply REOPENED. Casts must never silently die.
    loop {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            controls::listen_until(
                None,
                &stop,
                false,
                |ev| {
                    // While a press-to-bind capture is in flight, the user is pressing a control to BIND it,
                    // not to use it — keep the edge tracker in sync but fire NOTHING, so the captured key
                    // doesn't also run whatever it's currently bound to.
                    let capturing = crate::capture::CAPTURE_ACTIVE.load(Ordering::Relaxed);
                    for edge in edges.borrow_mut().edges(ev) {
                        if capturing {
                            continue;
                        }
                        match edge {
                            InputEdge::Down(trigger) => {
                                // Hold any HyperShift layer THIS input activates (tracked per-input so its
                                // release drops only its own layer), then dispatch the input's action.
                                rt.borrow_mut().hold_for_input(&trigger);
                                // An input→key REMAP holds the output key while held (edge-driven, like
                                // the mic) so it behaves like the real key; every OTHER action fires once.
                                // Skip fire_trigger when we held a key — firing would ALSO tap it.
                                if !key_remap_press(&rt, &held_keys, &trigger, &status, &weak) {
                                    if let Some(outcome) = fire_trigger(
                                        &mut devices.borrow_mut(),
                                        &mut rt.borrow_mut(),
                                        &mut exec.borrow_mut(),
                                        &trigger,
                                        &status,
                                        &weak,
                                    ) {
                                        turbos.borrow_mut().start(outcome.turbo);
                                    }
                                }
                                // momentary mic: capture the rest state + flip while held.
                                momentary_press(&rt, &momentary, &trigger);
                            }
                            InputEdge::Up(trigger) => {
                                rt.borrow_mut().release_for_input(&trigger);
                                turbos.borrow_mut().release(&trigger);
                                key_remap_release(&held_keys, &trigger); // release the held output key
                                momentary_release(&momentary, &trigger); // restore the mic on release
                            }
                        }
                    }
                    publish_held(&rt, &status, &weak);
                },
                || {
                    tick = tick.wrapping_add(1);
                    for cmd in live_rx.try_iter() {
                        match cmd {
                            LiveCommand::Reload => reload_pending = true,
                            LiveCommand::Inject(trigger) => injected.push(trigger),
                            LiveCommand::ToggleHyperShift(reply) => {
                                hypershift_latch = !hypershift_latch;
                                let _ = reply.send(hypershift_latch);
                            }
                            LiveCommand::ReconcileGamingHook => gaming_policy_dirty = true,
                            LiveCommand::ApplyProfile {
                                name,
                                persist,
                                reply,
                            } => {
                                let result =
                                    apply_profile_live(&mut devices.borrow_mut(), &name, persist);
                                if let Ok(applied) = &result {
                                    neuron::hook::set_policy(applied.policy);
                                    gaming_policy_dirty = true;
                                    {
                                        let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                        s.last_trigger = format!("profile {name}");
                                        s.last_action = applied.summary.clone();
                                        s.active_profile = name.clone();
                                    }
                                    post_status(&weak, &status);
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
                    if reload_pending {
                        reload_pending = false;
                        momentary_release_all(&momentary); // a held mic can't survive a config swap
                        key_remap_release_all(&held_keys); // nor a held remapped key
                        *rt.borrow_mut() = controls::build_runtime();
                        exec.borrow_mut().clear();
                        devices.borrow_mut().clear();
                        turbos.borrow_mut().clear();
                        applied_hypershift_latch = !hypershift_latch;
                        *LAST_ACTION_DESC
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        publish_held(&rt, &status, &weak);
                    }
                    // Injected triggers (the weave watcher's resolved flicks/glyphs) — same Engine,
                    // same fire path, so a cast composes with layers/intents/SAFE exactly like hardware.
                    {
                        let drained = std::mem::take(&mut injected);
                        for t in drained {
                            if fire_trigger(
                                &mut devices.borrow_mut(),
                                &mut rt.borrow_mut(),
                                &mut exec.borrow_mut(),
                                &t,
                                &status,
                                &weak,
                            )
                            .is_some()
                            {
                                crate::flight::trace("cast", "injected trigger fired", 0);
                            } else {
                                // a cast the user SAW resolve (the wheel lit, the glyph flared) that
                                // matched nothing in the engine — that must never be a silent fizzle.
                                crate::flight::trace("cast", "injected trigger matched NOTHING", 0);
                                {
                                    let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                    s.last_trigger = t.describe();
                                    s.last_action =
                                "cast hit nothing \u{2014} not bound in the engine (reload/config mismatch?)".into();
                                }
                                post_status(&weak, &status);
                            }
                        }
                    }
                    if applied_hypershift_latch != hypershift_latch {
                        rt.borrow_mut().latch_layer("hypershift", hypershift_latch);
                        applied_hypershift_latch = hypershift_latch;
                        publish_held(&rt, &status, &weak);
                    }
                    // Gaming policy is pushed by glue/profile apply; reconcile only when that shared
                    // carrier changed instead of polling every dispatch tick.
                    if gaming_policy_dirty {
                        gaming_policy_dirty = false;
                        install_gaming_hook(&mut hook);
                    }
                    {
                        let mut intents = AppIntentRunner {
                            devices: &mut devices.borrow_mut(),
                        };
                        turbos
                            .borrow_mut()
                            .tick(&mut exec.borrow_mut(), &mut intents);
                    }
                    if !tick.is_multiple_of(10) {
                        return; // throttle the periodic polls to ~50 ms
                    }
                    // mic tap (cached Core-Audio mute toggle) -> a MicTap trigger AND its raw Input usage.
                    if let Some(now) = crate::beacon::audio_cache::snap().mic.map(|(_, m)| m) {
                        let fire = last_mute == Some(!now); // a real toggle (not the first warm read)
                        last_mute = Some(now);
                        if fire {
                            // mirror the detected flip into the UI's mic pill — the panel otherwise only
                            // updates on its own toggle or a manual refresh.
                            let ui = weak.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = ui.upgrade() {
                                    app.global::<State>().set_mic_muted(now);
                                }
                            });
                            fire_trigger(
                                &mut devices.borrow_mut(),
                                &mut rt.borrow_mut(),
                                &mut exec.borrow_mut(),
                                &Trigger::MicTap,
                                &status,
                                &weak,
                            );
                            let (p, u) = MIC_TAP;
                            fire_trigger(
                                &mut devices.borrow_mut(),
                                &mut rt.borrow_mut(),
                                &mut exec.borrow_mut(),
                                &Trigger::Input {
                                    page: p,
                                    usage: u,
                                    pid: Some(0x056a),
                                },
                                &status,
                                &weak,
                            );
                        }
                    }
                    // app-aware switch: a focus change fires an AppFocus trigger; the Engine's matching
                    // rule (-> ProfileSwitch intent) does the switch (or nothing if unbound).
                    if let Some(app) = switcher.poll() {
                        {
                            let mut s = status.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            s.focused_app = app.clone();
                        }
                        post_status(&weak, &status);
                        fire_trigger(
                            &mut devices.borrow_mut(),
                            &mut rt.borrow_mut(),
                            &mut exec.borrow_mut(),
                            &Trigger::AppFocus { app },
                            &status,
                            &weak,
                        );
                    }
                },
            );
        }));
        if stop.load(Ordering::SeqCst) {
            break; // an asked-for stop — the only legitimate way out
        }
        crate::flight::trace(
            "life",
            if outcome.is_err() {
                "dispatch listener fault contained — reopening"
            } else {
                "dispatch listener exited unasked — reopening"
            },
            0,
        );
        momentary_release_all(&momentary); // never strand a held mic across a respawn
        key_remap_release_all(&held_keys); // nor a held remapped key
        std::thread::sleep(std::time::Duration::from_millis(250));
    }

    // teardown: restore any mic a momentary action was holding (the loop ended mid-hold) + release
    // any remapped key still held, the hook drops here (uninstalls), input disarms in LiveRuntime::stop.
    momentary_release_all(&momentary);
    key_remap_release_all(&held_keys);
    drop(hook);
}

// ── MOMENTARY MIC: the held push-to-talk / push-to-mute edge handling ─────────────────────────
type MomentaryMap =
    std::cell::RefCell<std::collections::HashMap<neuron::engine::Trigger, (Option<String>, bool)>>;

/// Open the mic VolumeCtl for a momentary action's (optional) device. Cross-platform via the
/// `audio` seam: off-Windows `VolumeCtl::open` returns `None` (no audio backend), so the whole
/// momentary path falls through to a no-op without any cfg gating here.
fn open_mic(device: &Option<String>) -> Option<neuron::audio::VolumeCtl> {
    neuron::audio::resolve_capture(device.as_deref())
        .and_then(|e| neuron::audio::VolumeCtl::open(&e.id))
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
    if let Some(ctl) = open_mic(&device) {
        let (while_held, on_release) = mode.states(ctl.get_mute());
        ctl.set_mute(while_held);
        held.borrow_mut()
            .insert(trigger.clone(), (device, on_release));
    }
}

/// A trigger's UP edge: restore the mic to its resting state.
fn momentary_release(held: &MomentaryMap, trigger: &neuron::engine::Trigger) {
    if let Some((device, restore)) = held.borrow_mut().remove(trigger) {
        if let Some(ctl) = open_mic(&device) {
            ctl.set_mute(restore);
        }
    }
}

/// Restore EVERY held mic and clear the map — the safety net for a config swap / daemon stop, so a
/// momentary can never strand the mic flipped.
fn momentary_release_all(held: &MomentaryMap) {
    for (_, (device, restore)) in held.borrow_mut().drain() {
        if let Some(ctl) = open_mic(&device) {
            ctl.set_mute(restore);
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
    let report = profile.apply_with_session(devices);
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
            if !snap.active_profile.is_empty() {
                // a live ProfileSwitch/Cycle moved the cursor — mirror it into the header pill,
                // the GUI runtime, and the Profiles panel highlight.
                crate::glue::note_live_profile(&app, &snap.active_profile);
            }
            st.set_live_held(snap.held_layers.clone().into());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
        set_gaming_policy(neuron::writes::GamingMode::from_profile(true, false, false));
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
}
