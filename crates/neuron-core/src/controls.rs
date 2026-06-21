//! Control-event listener + decoder — the foundation for remapping. Captures Raw Input
//! (INPUTSINK) from the headset's Consumer (knob/media) and Telephony (mute) collections and
//! decodes each report into semantic `(usage_page, usage)` pairs via HidP — so bindings match
//! on *meaning* ("Volume Up", "Phone Mute"), not fragile raw bytes. `watch` prints them;
//! `run` (see `bindings`) dispatches them into actions like "set the real mic's gain".

/// One decoded control report from a Razer device.
#[derive(Clone, Debug, Default)]
pub struct ControlEvent {
    pub pid: String,           // source device, e.g. "0529"
    pub hits: Vec<(u16, u16)>, // active (usage_page, usage) pairs ("buttons currently down")
    pub raw: Vec<u8>,          // raw HID report bytes (full transparency)
}

impl ControlEvent {
    /// True if this report has a given (page, usage) active.
    pub fn has(&self, page: u16, usage: u16) -> bool {
        self.hits.contains(&(page, usage))
    }
    /// True on a "press" (any usage active) vs the matching release report (none active).
    pub fn is_press(&self) -> bool {
        !self.hits.is_empty()
    }
}

/// Synthetic trigger for the Seiren tap-to-mute, detected host-side via the capture
/// endpoint's mute toggling (Core Audio) — no vendor HID needed. Uses the HID private-use
/// page so it can never collide with a real usage.
pub const MIC_TAP: (u16, u16) = (0xF000, 0x01);

/// Friendly name for the common control usages we expect (printing only).
pub fn usage_name(page: u16, usage: u16) -> &'static str {
    match (page, usage) {
        MIC_TAP => "Mic Tap",
        (0x0C, 0xE9) => "Volume Up",
        (0x0C, 0xEA) => "Volume Down",
        (0x0C, 0xE2) => "Mute",
        (0x0C, 0xCD) => "Play/Pause",
        (0x0C, 0xB5) => "Next Track",
        (0x0C, 0xB6) => "Prev Track",
        (0x0C, 0xB7) => "Stop",
        (0x0B, 0x2F) => "Phone Mute",
        (0x0B, 0x20) => "Hook Switch",
        _ => "?",
    }
}

/// Usage pages we decode from each report.
pub const PROBE_PAGES: [u16; 2] = [0x0C, 0x0B]; // Consumer, Telephony

#[cfg(windows)]
pub fn watch(seconds: u64) {
    let mut count = 0u32;
    println!("Watching headset knob / mute / consumer controls for {seconds}s (ESC to stop).");
    println!("Turn the knob, press the mute toggle, tap the mic - events print below:\n");
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    win::listen(
        Some(seconds),
        &NEVER,
        true, // interactive terminal tool — ESC stops the watch, as printed above
        |ev| {
            if !ev.is_press() {
                return; // skip release reports in the printout
            }
            let decoded: Vec<String> = ev
                .hits
                .iter()
                .map(|&(p, u)| format!("{} (0x{p:02X}/0x{u:02X})", usage_name(p, u)))
                .collect();
            let hex: String = ev
                .raw
                .iter()
                .take(16)
                .map(|b| format!("{b:02X} "))
                .collect();
            if decoded.is_empty() {
                println!("  [PID {}] {hex}", ev.pid);
            } else {
                println!("  [PID {}] {}   raw: {hex}", ev.pid, decoded.join(", "));
            }
            count += 1;
        },
        || {},
    );
    println!("\ncaptured {count} Razer control event(s).");
}

#[cfg(not(windows))]
pub fn watch(_seconds: u64) {
    println!("control watching is Windows-only for now");
}

/// Listen for control events, dispatching each to `on_event`, until `seconds` elapse (or
/// forever if `None`) or ESC is pressed. `on_tick` runs once per loop iteration (~5 ms) — the
/// daemon uses it to poll the mic's mute state for tap detection. Reusable by `watch`/`run`.
///
/// This is the CLI daemon's entry point and its behaviour is unchanged — it delegates to
/// [`listen_until`] with a stop flag that is never set. A GUI worker thread should prefer
/// [`listen_until`] so it can stop the loop cleanly from another thread.
#[cfg(windows)]
pub fn listen(seconds: Option<u64>, on_event: impl FnMut(&ControlEvent), on_tick: impl FnMut()) {
    // A stop flag that is never set: identical behaviour to the historical `listen` (run until
    // `seconds` elapse or ESC). Keeps the CLI daemon path byte-for-byte the same.
    static NEVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    win::listen(seconds, &NEVER, true, on_event, on_tick);
}

#[cfg(not(windows))]
pub fn listen(_seconds: Option<u64>, _on_event: impl FnMut(&ControlEvent), _on_tick: impl FnMut()) {
}

/// Stop-signalled variant of [`listen`] — the primitive a GUI worker thread drives. Identical to
/// [`listen`] (registers Raw Input on a hidden top-level window, decodes each report and calls
/// `on_event`, runs `on_tick` each iteration) but the loop also exits as soon as `stop` becomes
/// `true`, and `esc_stops` chooses whether a physical ESC press ends the loop: `true` for an
/// interactive capture ("press a control — ESC cancels"), **`false` for a RESIDENT worker** — a
/// resident engine that died on ESC would silently kill every cast/remap the first time the user
/// closed a game menu (the exact bug this flag exists to prevent).
/// The GUI builds an [`Engine`] (see [`build_runtime`]), spawns a thread, and inside the
/// `on_event` closure turns each [`ControlEvent`] into a [`Trigger`] (via [`event_trigger`]) and
/// calls [`Engine::fire`] — exactly as the CLI daemon does in `run_listen` — then flips `stop`
/// from the UI thread to tear the worker down on profile change / app exit.
///
/// `stop` is borrowed (`'static` is NOT required); the caller owns it — typically an
/// `Arc<AtomicBool>` cloned into the worker thread and held by the UI side. The loop polls it once
/// per ~5 ms iteration, so teardown latency is sub-frame.
///
/// # GUI usage sketch
/// ```no_run
/// # #[cfg(windows)] {
/// use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
/// use std::cell::RefCell;
/// use neuron::controls::{self, event_trigger};
/// use neuron::macros::context::Context;
///
/// // 1. The UI side owns the stop flag and a way to signal the worker.
/// let stop = Arc::new(AtomicBool::new(false));
///
/// // 2. The worker thread builds the unified Engine from the same on-disk configs the CLI uses.
/// let worker_stop = stop.clone();
/// let handle = std::thread::spawn(move || {
///     // ARM input on the live path (the GUI gates this behind its safe-mode toggle):
///     neuron::action::arm_input(true);
///     let rt = RefCell::new(controls::build_runtime());
///     controls::listen_until(
///         None,                 // run until stopped
///         &worker_stop,
///         false,                // RESIDENT worker: a stray ESC press must never kill it
///         |ev| {
///             let Some(trigger) = event_trigger(ev) else {
///                 rt.borrow_mut().engine.release_all();   // release HyperShift edges
///                 return;
///             };
///             // hold any layer this input toggles, then dispatch through the one Engine:
///             let layers: Vec<String> = rt.borrow().engine.resolve(&trigger)
///                 .iter().filter_map(|r| r.layer.clone()).collect();
///             for l in layers { rt.borrow_mut().engine.hold(l); }
///             let ctx = Context::capture();
///             for rule in rt.borrow().engine.resolve(&trigger) {
///                 let _ = rule.action.run_ctx(&ctx);      // run each matched action
///             }
///         },
///         || {},                // on_tick: poll mic-tap / app-switch here if desired
///     );
/// });
///
/// // 3. To stop the worker cleanly from the UI thread:
/// stop.store(true, Ordering::Relaxed);
/// handle.join().ok();
/// # }
/// ```
#[cfg(windows)]
pub fn listen_until(
    seconds: Option<u64>,
    stop: &std::sync::atomic::AtomicBool,
    esc_stops: bool,
    on_event: impl FnMut(&ControlEvent),
    on_tick: impl FnMut(),
) {
    win::listen(seconds, stop, esc_stops, on_event, on_tick);
}

/// Inert non-Windows twin of [`listen_until`] (IDENTICAL signature). There is no Raw-Input source
/// off-Windows yet, so `on_event` is NEVER called (no hardware edges to diff). But the resident
/// worker's tick drives reload / inject / profile-apply commands, so this still pumps `on_tick`
/// every ~50 ms (matching the Windows path's `tick % 10` throttle cadence) until `stop` is set or
/// the optional `seconds` budget elapses. `esc_stops` is meaningless without a key source and is
/// ignored. This keeps the GUI's config plumbing live cross-platform while input itself is dormant.
///
/// NOTE (seam tradeoff): `listen_until` is ONE function with a single stable signature, so a
/// `#[cfg]` pair (Win32 body + inert body) is the right tool here — unlike the multi-primitive
/// window-manager seam, which needed a trait to abstract 22 separate Win32 calls. Both arms keep
/// byte-identical signatures so the caller is platform-agnostic.
#[cfg(not(windows))]
pub fn listen_until(
    seconds: Option<u64>,
    stop: &std::sync::atomic::AtomicBool,
    _esc_stops: bool,
    _on_event: impl FnMut(&ControlEvent),
    mut on_tick: impl FnMut(),
) {
    use std::time::Instant;
    let start = Instant::now();
    while seconds.map_or(true, |s| start.elapsed().as_secs() < s) {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            break;
        }
        on_tick();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

// ─────────────────────────── the live spine: configs -> one Engine ───────────────────────────
//
// Historically the run-daemon dispatched control events through `Bindings::dispatch` (this
// module's sibling, `bindings.rs`) while gestures/radial went through `cast.rs` and app-switching
// lived inline in the CLI. That is three code paths for the one primitive the project's keystone
// insight collapses: *something happened (a [`Trigger`]) so do this (an [`Action`])*.
//
// [`build_engine`] is the bridge: it reads every config source the daemon already loads — the
// `bindings.toml`, the cast `cast.toml` (radial wedges + glyph spells), every imported
// `profiles/*.rules.toml` spine sidecar, and the app-switch `apps.toml` — and folds them into ONE
// [`Engine`] via [`Engine::from_rules`]. The run-daemon then translates each device event into a
// [`Trigger`] and calls [`Engine::fire`], so the SAME dispatcher serves buttons, gestures, the
// radial wheel, app focus, the mic tap and HyperShift layers. Backward compatible: the on-disk
// formats are unchanged — this only changes how they are *executed* at runtime.

use crate::action::Action;
use crate::bindings::{Binding, Bindings};
use crate::cast::CastConfig;
use crate::engine::{Engine, Rule, Trigger};
use crate::profile::AppRules;

/// Map ONE decoded control event to the [`Trigger`] the engine matches on. Uses the FIRST active
/// `(page, usage)` hit and carries the source `pid` so a rule can restrict by device. Returns
/// `None` for a release report (no hits).
///
/// NOTE: a real Razer HID report carries the *set* of buttons currently down (an edge-state), so
/// this single-trigger view drops every usage after the first when more than one control is active
/// (the multi-button / HyperShift-hold-plus-press case). The live daemon should prefer
/// [`HoldEdges::edges`], which diffs successive reports into per-button down/up edges. This helper
/// is retained for the watch/printout path and back-compat.
pub fn event_trigger(ev: &ControlEvent) -> Option<Trigger> {
    let &(page, usage) = ev.hits.first()?;
    let pid = u16::from_str_radix(&ev.pid, 16).ok();
    Some(Trigger::Input { page, usage, pid })
}

/// Map a single decoded `(page, usage)` hit + source pid to its [`Trigger::Input`].
fn hit_trigger(page: u16, usage: u16, pid: Option<u16>) -> Trigger {
    Trigger::Input { page, usage, pid }
}

/// One per-button transition derived from diffing two successive HID reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputEdge {
    /// A control that was NOT down in the previous report is now down — dispatch this trigger.
    Down(Trigger),
    /// A control that WAS down is now released — release any HyperShift layer it activated.
    Up(Trigger),
}

/// Edge-detector for the live dispatch loop. A real Razer control report is the *set of buttons
/// currently down*, reported as an edge — so the daemon must DIFF successive reports to recover
/// each button's individual press/release, rather than reacting to one usage or to an empty report.
///
/// Why this exists (the two bugs it fixes):
/// * **multi-button drop** — [`event_trigger`] kept only `hits.first()`, so a second control held
///   at the same time (a HyperShift hold key + another button) never dispatched. [`edges`] yields a
///   [`InputEdge::Down`] for EVERY newly-pressed control.
/// * **stuck / over-eager HyperShift release** — the old loop called `release_all()` on any
///   empty-hits report, dropping every held layer the instant ANY button went up. [`edges`] yields
///   a precise [`InputEdge::Up`] only for the control that actually transitioned to released, so a
///   caller can release exactly that input's layer (see [`HoldEdges` usage] in the daemons).
///
/// Per source device (`pid`): the previous down-set is tracked separately, because two devices
/// report independently and a release on one must not look like a release on the other.
#[derive(Default)]
pub struct HoldEdges {
    /// Previously-down `(page, usage)` controls, keyed by source pid (`None` = unknown pid bucket).
    /// Kept sorted/deduped: input reports are tiny, so a compact `Vec` beats a tree here.
    down: std::collections::HashMap<Option<u16>, Vec<(u16, u16)>>,
}

impl HoldEdges {
    pub fn new() -> Self {
        Self::default()
    }

    /// Diff this report against the previous one *from the same device* and return the per-button
    /// transitions: a [`InputEdge::Down`] for each newly-pressed control and a [`InputEdge::Up`]
    /// for each newly-released control. Down edges are returned before up edges so a press is
    /// dispatched before a sibling release is processed.
    ///
    /// An empty report (`hits == []`) is the all-released edge for that device: it yields an `Up`
    /// for every control that was down — no more `release_all()`-on-any-empty over-reach.
    pub fn edges(&mut self, ev: &ControlEvent) -> Vec<InputEdge> {
        let pid = u16::from_str_radix(&ev.pid, 16).ok();
        let mut now = ev.hits.clone();
        normalize_hits(&mut now);
        let prev = self.down.entry(pid).or_default();
        let mut out = Vec::with_capacity(now.len().saturating_add(prev.len()));
        // down edges: in `now`, not in `prev`.
        for &(p, u) in &now {
            if prev.binary_search(&(p, u)).is_err() {
                out.push(InputEdge::Down(hit_trigger(p, u, pid)));
            }
        }
        // up edges: in `prev`, not in `now`.
        for &(p, u) in prev.iter() {
            if now.binary_search(&(p, u)).is_err() {
                out.push(InputEdge::Up(hit_trigger(p, u, pid)));
            }
        }
        *prev = now;
        out
    }
}

fn normalize_hits(hits: &mut Vec<(u16, u16)>) {
    if hits.len() > 1 {
        hits.sort_unstable();
        hits.dedup();
    }
}

/// Translate a [`Binding`] (control-event -> action, the `bindings.toml` row) into a spine
/// [`Rule`]. The trigger becomes a [`Trigger::Input`] (page/usage, optional pid filter); the
/// stringly-typed action becomes a typed [`Action`]:
///
/// * `mic-gain` -> [`Action::MicGain`] (delta percentage points)
/// * `mic-mute` -> [`Action::MicMute`] (mode on/off/toggle)
/// * `mic-gain-set` -> [`Action::MicGainSet`] (absolute percentage)
/// * `run`          -> [`Action::Run`] (shell command)
pub fn binding_rule(b: &Binding) -> Option<Rule> {
    let pid = b.pid.as_ref().and_then(|p| u16::from_str_radix(p, 16).ok());
    let trigger = Trigger::Input {
        page: b.page,
        usage: b.usage,
        pid,
    };
    let action = match b.action.as_str() {
        "mic-gain" => Action::MicGain {
            device: b.device.clone(),
            delta_pct: b.delta_pct.unwrap_or(0.0),
        },
        "mic-gain-set" => Action::MicGainSet {
            device: b.device.clone(),
            pct: b.pct.unwrap_or(0.0),
        },
        "mic-mute" => Action::MicMute {
            device: b.device.clone(),
            mode: b.mode.clone().unwrap_or_else(|| "toggle".into()),
        },
        "run" => Action::Run {
            cmd: b.cmd.clone().unwrap_or_default(),
        },
        _ => return None,
    };
    Some(Rule::new(trigger, action))
}

/// The assembled live runtime: the one [`Engine`] every input source dispatches through, plus the
/// bits the daemon needs that aren't expressible as plain `Rule`s.
pub struct Runtime {
    /// The unified dispatcher (base rules + named HyperShift layers).
    pub engine: Engine,
    /// The cast hold-trigger VK (from `cast.toml`) — the button to watch to capture a gesture /
    /// radial flick and emit a [`Trigger::Gesture`] / [`Trigger::RadialSector`].
    ///
    /// LIVE in the GUI app: its weave watcher (neuron-app `beacon.rs`, the one owner of the cast
    /// trigger) captures the held stroke on a dedicated thread, resolves it through
    /// [`crate::cast::CastConfig::resolve`], and injects the resolved trigger into the live
    /// dispatch Engine. The CLI `run` daemon does NOT capture weaves (its blocking capture would
    /// freeze the Raw-Input pump) — CLI weaving stays on the one-shot `cast run` subcommand.
    pub cast_trigger: i32,
    /// The cast radial sector count (so a dispatcher resolves a flick to the right wedge index).
    pub cast_sectors: usize,
    /// The radial menu name the cast wedges are registered under (matches the rules built here).
    pub cast_menu: String,
    /// Per-input HyperShift bookkeeping: which layer(s) each currently-held input activated, keyed
    /// by the activating [`Trigger`]. Lets a release of a SPECIFIC input drop ONLY that input's
    /// layer(s) — instead of the old `release_all()`-on-any-release that dropped every layer the
    /// moment any unrelated button went up.
    held_by: std::collections::HashMap<Trigger, Vec<String>>,
    /// The layer STANCE (see [`crate::feel::LayerMode`]) — hold / latch / smart / one-shot.
    /// Razer ships hold-only; the stance is Neuron's fix. Set from `feel.toml` by [`build_runtime`].
    layer_mode: crate::feel::LayerMode,
    /// The tap-vs-hold discriminator for the `Smart` stance (from `feel.toml`).
    hold_ms: u64,
    /// Layers currently LATCHED on (by the latch/smart/one-shot stances) — they survive the
    /// trigger's release and drop on the next toggle (or, for one-shots, the next fired trigger).
    latched: std::collections::HashSet<String>,
    /// One-shot-armed layers: released automatically after the next non-layer trigger fires.
    oneshot: std::collections::HashSet<String>,
    /// Down timestamps for the smart stance's tap-vs-momentary decision.
    down_at: std::collections::HashMap<Trigger, std::time::Instant>,
}

impl Runtime {
    /// The number of rules across base + all layers (for the daemon's startup banner).
    pub fn rule_count(&self) -> usize {
        self.engine.rules.len() + self.engine.layers.values().map(Vec::len).sum::<usize>()
    }

    /// Set the layer stance + timing from feel config (see [`crate::feel`]).
    pub fn set_feel(&mut self, feel: &crate::feel::FeelConfig) {
        self.layer_mode = feel.hypershift;
        self.hold_ms = feel.hold_ms;
    }

    /// If this trigger fires a [`crate::action::Action::MomentaryMic`], its `(device, mode)` — so
    /// the daemon's edge loop can do the press on DOWN and the restore on UP (a held action the
    /// stateless dispatch can't express on its own). `None` if no momentary-mic binds this input.
    pub fn momentary_mic_for(
        &self,
        trigger: &Trigger,
    ) -> Option<(Option<String>, crate::action::MomentaryMode)> {
        for rule in self.engine.resolve(trigger) {
            if let crate::action::Action::MomentaryMic { device, mode } = &rule.action {
                return Some((device.clone(), *mode));
            }
        }
        None
    }

    /// Handle an input DOWN edge for HyperShift, in the configured STANCE:
    ///   * `Hold` — the layer lives while the input is held (Razer's behaviour).
    ///   * `Latch` — this press toggles the layer on/off; the up edge is ignored.
    ///   * `Smart` — the layer holds immediately (usable right now); the UP edge decides:
    ///     a tap latches it, a long hold was momentary. One trigger, both muscle memories.
    ///   * `OneShot` — this press arms the layer for exactly the next fired trigger; pressing
    ///     again before firing disarms (activation just to deactivate is free).
    ///
    /// Returns nothing; call [`Engine::resolve`]/`fire` separately to dispatch the input's action.
    pub fn hold_for_input(&mut self, trigger: &Trigger) {
        use crate::feel::LayerMode;
        // Scan ALL layers (held or not) for one whose rule matches this input — `resolve` would
        // only see a layer's rules once it's ALREADY held, so it can't discover the activating
        // input. `layers_activated_by` answers "pressing this input holds which layers?".
        let layers = self.engine.layers_activated_by(trigger);
        if layers.is_empty() {
            return;
        }
        match self.layer_mode {
            LayerMode::Hold => {
                for l in &layers {
                    self.engine.hold(l.clone());
                }
                self.held_by.insert(trigger.clone(), layers);
            }
            LayerMode::Latch => {
                for l in &layers {
                    if self.latched.remove(l) {
                        self.oneshot.remove(l);
                        if !self.kept_by_inputs(l) {
                            self.engine.release(l);
                        }
                    } else {
                        self.latched.insert(l.clone());
                        self.engine.hold(l.clone());
                    }
                }
            }
            LayerMode::Smart => {
                // hold NOW (zero latency while pressed); the release classifies tap vs momentary.
                for l in &layers {
                    self.engine.hold(l.clone());
                }
                self.held_by.insert(trigger.clone(), layers);
                self.down_at
                    .insert(trigger.clone(), std::time::Instant::now());
            }
            LayerMode::OneShot => {
                for l in &layers {
                    if self.latched.remove(l) {
                        self.oneshot.remove(l);
                        if !self.kept_by_inputs(l) {
                            self.engine.release(l);
                        }
                    } else {
                        self.latched.insert(l.clone());
                        self.oneshot.insert(l.clone());
                        self.engine.hold(l.clone());
                    }
                }
            }
        }
    }

    /// Handle an input UP edge for HyperShift. In the `Hold` stance this releases exactly the
    /// layer(s) this input activated; in `Smart` it classifies the press (tap = latch, long =
    /// momentary end); the latch/one-shot stances ignore up edges entirely.
    pub fn release_for_input(&mut self, trigger: &Trigger) {
        use crate::feel::LayerMode;
        match self.layer_mode {
            LayerMode::Hold => {
                if let Some(layers) = self.held_by.remove(trigger) {
                    for l in &layers {
                        // Only release a layer once no other still-held input keeps it active.
                        if !self.kept_by_inputs(l) && !self.latched.contains(l) {
                            self.engine.release(l);
                        }
                    }
                }
            }
            LayerMode::Latch | LayerMode::OneShot => { /* the down edge did all the work */ }
            LayerMode::Smart => {
                let dur_ms = self
                    .down_at
                    .remove(trigger)
                    .map(|t| t.elapsed().as_millis() as u64)
                    .unwrap_or(u64::MAX);
                if let Some(layers) = self.held_by.remove(trigger) {
                    for l in &layers {
                        if dur_ms < self.hold_ms {
                            // a TAP: toggle the latch. Already latched -> this tap unlatches.
                            if self.latched.remove(l) {
                                if !self.kept_by_inputs(l) {
                                    self.engine.release(l);
                                }
                            } else {
                                self.latched.insert(l.clone()); // stays held — latched on
                            }
                        } else {
                            // a long HOLD: momentary — ends with the button (unless latched).
                            if !self.latched.contains(l) && !self.kept_by_inputs(l) {
                                self.engine.release(l);
                            }
                        }
                    }
                }
            }
        }
    }

    /// A non-layer trigger fired — one-shot layers are now SPENT and drop. The arming press
    /// itself (a layer trigger) never consumes the shot.
    pub fn note_fired(&mut self, trigger: &Trigger) {
        if self.oneshot.is_empty() {
            return;
        }
        if !self.engine.layers_activated_by(trigger).is_empty() {
            return;
        }
        let spent: Vec<String> = self.oneshot.drain().collect();
        for l in spent {
            self.latched.remove(&l);
            if !self.kept_by_inputs(&l) {
                self.engine.release(&l);
            }
        }
    }

    /// Is `layer` still activated by some physically-held input (the held_by map)?
    fn kept_by_inputs(&self, layer: &str) -> bool {
        self.held_by.values().any(|v| v.iter().any(|x| x == layer))
    }

    /// Reconcile a SOFTWARE latch on a named layer (the tray/hotkey "HyperShift ON" toggle, as
    /// opposed to a physically-held input). `on` asserts the layer held; `!on` releases it — but
    /// ONLY if neither a physically-held input nor a stance latch still owns it, so dropping the
    /// tray latch can't yank a layer out from under a finger (or a latched stance). Engine layers
    /// are a SET (re-holding is a no-op), so calling this every tick is cheap and self-healing.
    pub fn latch_layer(&mut self, layer: &str, on: bool) {
        if on {
            self.engine.hold(layer.to_string());
        } else if !self.kept_by_inputs(layer) && !self.latched.contains(layer) {
            self.engine.release(layer);
        }
    }

    /// Release EVERY held layer and clear the per-input + stance bookkeeping — the focus-loss /
    /// pause safety net (not the normal release path, which is [`release_for_input`]). Use when the
    /// daemon can no longer trust its down-set (window focus lost, loop paused) so a layer can't
    /// get stuck.
    pub fn release_all_layers(&mut self) {
        self.engine.release_all();
        self.held_by.clear();
        self.latched.clear();
        self.oneshot.clear();
        self.down_at.clear();
    }
}

/// The radial menu name the cast wedges register under in the spine. One name so the daemon's
/// fired [`Trigger::RadialSector`] matches the rules [`build_engine`] created.
pub const CAST_MENU: &str = "comms";

/// Build the unified [`Engine`] from every on-disk config source the run-daemon honours. This is
/// the heart of live-wiring the spine: instead of three dispatchers, one.
///
/// Sources folded in (all backward compatible — formats unchanged):
/// 1. **`bindings.toml`** device-control remaps -> [`Trigger::Input`] rules.
/// 2. **`cast.toml`** radial wedges -> [`Trigger::RadialSector`] rules (one per bound sector) and
///    glyph spells -> [`Trigger::Gesture`] rules.
/// 3. **`profiles/*.rules.toml`** sidecars (the migration importer's spine output, and any future
///    GUI-authored rules) -> their `Rule`s verbatim, *including* HyperShift `layer` tags, so an
///    imported held-layer bind dispatches as a real layer.
/// 4. **`apps.toml`** app-switch rules -> [`Trigger::AppFocus`] -> [`Action::ProfileSwitch`] rules,
///    so focus-driven profile switching is the same spine primitive (a daemon intent).
///
/// Returns a [`Runtime`] (the engine + the leftovers the daemon needs). Pure aside from reading the
/// config files; safe to call at daemon startup.
pub fn build_runtime() -> Runtime {
    let bindings = Bindings::load();
    let cast = CastConfig::load();
    let app_rules = AppRules::load();
    let mut rt = build_runtime_from(&bindings, &cast, &app_rules, &load_rule_sidecars());
    // the layer stance + timing windows come from feel.toml (defaults when absent).
    rt.set_feel(&crate::feel::FeelConfig::load());
    rt
}

/// Testable core of [`build_runtime`]: assemble the [`Runtime`] from already-loaded configs. Keeps
/// the file IO ([`build_runtime`]) separate from the pure rule-folding so the mapping is unit-
/// testable without touching disk.
pub fn build_runtime_from(
    bindings: &Bindings,
    cast: &CastConfig,
    app_rules: &AppRules,
    sidecar_rules: &[Rule],
) -> Runtime {
    let mut rules: Vec<Rule> = Vec::new();
    // 1. device-control bindings -> Input rules.
    for b in &bindings.bindings {
        if let Some(r) = binding_rule(b) {
            rules.push(r);
        }
    }

    // 2. cast radial wedges -> RadialSector rules; glyph spells -> Gesture rules. A `Noop` wedge is
    //    skipped (an unbound wedge contributes no rule), and wedges past the configured sector
    //    count are unreachable (a flick can only resolve 0..sectors) so they contribute none either
    //    — shrinking the wheel keeps the surplus actions on disk but out of the live engine.
    for (i, a) in cast.radial.iter().take(cast.sectors).enumerate() {
        if *a != Action::Noop {
            rules.push(Rule::new(
                Trigger::RadialSector {
                    menu: CAST_MENU.into(),
                    sector: i as u8,
                },
                a.clone(),
            ));
        }
    }
    // 2b. HYPERSHIFT radial wedges (when enabled) -> the SAME RadialSector triggers, but tagged on the
    //     "hypershift" layer, so the EXISTING layer resolution fires them instead of the base wedge
    //     while a HyperShift layer is held. Pure reuse of the layer engine — no new dispatch path, no
    //     new backend; the overlay just renders `active_radial` to match what the engine will fire.
    if cast.hyper_radial_on {
        for (i, a) in cast.hyper_radial.iter().take(cast.sectors).enumerate() {
            if *a != Action::Noop {
                rules.push(Rule::on_layer(
                    "hypershift",
                    Trigger::RadialSector {
                        menu: CAST_MENU.into(),
                        sector: i as u8,
                    },
                    a.clone(),
                ));
            }
        }
    }
    for (name, a) in &cast.gestures {
        if *a != Action::Noop {
            rules.push(Rule::new(
                Trigger::Gesture { name: name.clone() },
                a.clone(),
            ));
        }
    }
    // 2c. cast RHYTHM map -> Cast{taps} rules. Each "N taps then hold" on the cast trigger is a
    //     first-class trigger the engine resolves to its bound action: this is what makes a
    //     rhythm remappable AND lets `inject_trigger(Trigger::Cast{taps})` (from the capture
    //     state machine) fire through the ONE spine instead of a hard-wired instrument route.
    //     taps=0 (the weave's own plain hold) and Noop are skipped — the weave resolves itself.
    for rb in &cast.rhythm_actions {
        if rb.action != Action::Noop && rb.taps != 0 {
            rules.push(Rule::new(Trigger::Cast { taps: rb.taps }, rb.action.clone()));
        }
    }

    // 3. imported / GUI-authored spine sidecars verbatim (HyperShift layer tags preserved).
    rules.extend(sidecar_rules.iter().cloned());

    // 4. app-aware profile switching -> AppFocus -> ProfileSwitch intent rules.
    for r in &app_rules.rules {
        rules.push(Rule::new(
            Trigger::AppFocus { app: r.app.clone() },
            Action::ProfileSwitch {
                name: r.profile.clone(),
            },
        ));
    }

    Runtime {
        engine: Engine::from_rules(rules),
        cast_trigger: cast.trigger,
        cast_sectors: cast.sectors,
        cast_menu: CAST_MENU.into(),
        held_by: std::collections::HashMap::new(),
        layer_mode: crate::feel::LayerMode::default(),
        hold_ms: crate::feel::FeelConfig::default().hold_ms,
        latched: std::collections::HashSet::new(),
        oneshot: std::collections::HashSet::new(),
        down_at: std::collections::HashMap::new(),
    }
}

/// Load every `profiles/*.rules.toml` spine sidecar into a flat `Vec<Rule>`. These are the
/// migration importer's output (and any GUI-authored rules). Missing dir / parse errors degrade to
/// an empty list rather than failing the daemon.
pub fn load_rule_sidecars() -> Vec<Rule> {
    load_rule_sidecars_with(|_| true)
}

pub fn load_rule_sidecars_except(excluded_file_name: &str) -> Vec<Rule> {
    load_rule_sidecars_with(|name| name != excluded_file_name)
}

fn load_rule_sidecars_with(include: impl Fn(&str) -> bool) -> Vec<Rule> {
    use crate::engine::RuleDoc;
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("profiles") else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !file_name.ends_with(".rules.toml") || !include(file_name) {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(&path) {
            match toml::from_str::<RuleDoc>(&s) {
                Ok(doc) => out.extend(doc.rules),
                Err(e) => eprintln!("  (skipping {}: {e})", path.display()),
            }
        }
    }
    out
}

#[cfg(test)]
mod spine_tests {
    use super::*;
    use crate::action::{Action, Direction};
    use crate::bindings::Binding;
    use crate::cast::CastConfig;
    use crate::engine::Trigger;
    use crate::profile::{AppRule, AppRules};

    fn bind(page: u16, usage: u16, action: &str) -> Binding {
        Binding {
            desc: String::new(),
            page,
            usage,
            pid: None,
            action: action.into(),
            device: None,
            delta_pct: None,
            pct: None,
            mode: None,
            cmd: None,
        }
    }

    #[test]
    fn event_trigger_uses_first_hit_and_pid() {
        let ev = ControlEvent {
            pid: "0529".into(),
            hits: vec![(0x0C, 0xE9)],
            raw: vec![],
        };
        let t = event_trigger(&ev).unwrap();
        assert_eq!(
            t,
            Trigger::Input {
                page: 0x0C,
                usage: 0xE9,
                pid: Some(0x0529)
            }
        );
        // a release report (no hits) yields no trigger (it drives hold edges, not dispatch).
        let rel = ControlEvent {
            pid: "0529".into(),
            hits: vec![],
            raw: vec![],
        };
        assert!(event_trigger(&rel).is_none());
    }

    #[test]
    fn hold_edges_diffs_down_and_up() {
        let mut he = HoldEdges::new();
        let ev = |hits: Vec<(u16, u16)>| ControlEvent {
            pid: "00a8".into(),
            hits,
            raw: vec![],
        };
        // press button A -> one Down edge.
        let e = he.edges(&ev(vec![(0x09, 0x01)]));
        assert_eq!(
            e,
            vec![InputEdge::Down(Trigger::Input {
                page: 0x09,
                usage: 0x01,
                pid: Some(0x00a8)
            })]
        );
        // now press B while A stays down -> only B is a new Down (A is NOT re-emitted).
        let e = he.edges(&ev(vec![(0x09, 0x01), (0x09, 0x02)]));
        assert_eq!(
            e,
            vec![InputEdge::Down(Trigger::Input {
                page: 0x09,
                usage: 0x02,
                pid: Some(0x00a8)
            })],
            "a second button held with the first dispatches; the first is not dropped or re-fired"
        );
        // release A only (B still down) -> ONE Up edge for A, nothing for B.
        let e = he.edges(&ev(vec![(0x09, 0x02)]));
        assert_eq!(
            e,
            vec![InputEdge::Up(Trigger::Input {
                page: 0x09,
                usage: 0x01,
                pid: Some(0x00a8)
            })],
            "releasing one button yields an Up for ONLY that button"
        );
        // empty report -> B's Up edge (all-released), not a blanket release of unrelated state.
        let e = he.edges(&ev(vec![]));
        assert_eq!(
            e,
            vec![InputEdge::Up(Trigger::Input {
                page: 0x09,
                usage: 0x02,
                pid: Some(0x00a8)
            })]
        );
    }

    #[test]
    fn hold_edges_are_per_device() {
        let mut he = HoldEdges::new();
        let a = ControlEvent {
            pid: "00a8".into(),
            hits: vec![(0x09, 0x01)],
            raw: vec![],
        };
        let b_empty = ControlEvent {
            pid: "0221".into(),
            hits: vec![],
            raw: vec![],
        };
        assert_eq!(he.edges(&a).len(), 1, "device A press");
        // an empty report from a DIFFERENT device must not release device A's button.
        assert!(
            he.edges(&b_empty).is_empty(),
            "device B empty report does not touch device A"
        );
    }

    #[test]
    fn runtime_releases_only_the_inputs_own_layer() {
        // sniper layer activated by input X; comms layer activated by input Y. Releasing Y must NOT
        // drop sniper (the old release_all dropped both on any release).
        let x = Trigger::Input {
            page: 0x09,
            usage: 0x01,
            pid: None,
        };
        let y = Trigger::Input {
            page: 0x09,
            usage: 0x02,
            pid: None,
        };
        let sidecar = vec![
            Rule::on_layer("sniper", x.clone(), Action::DpiSet { dpi: 400 }),
            Rule::on_layer("comms", y.clone(), Action::Noop),
        ];
        let mut rt = build_runtime_from(
            &Bindings::default(),
            &CastConfig::default(),
            &AppRules::default(),
            &sidecar,
        );
        rt.hold_for_input(&x);
        rt.hold_for_input(&y);
        assert!(rt.engine.is_held("sniper") && rt.engine.is_held("comms"));
        // release Y -> only comms drops; sniper stays held.
        rt.release_for_input(&y);
        assert!(
            rt.engine.is_held("sniper"),
            "unrelated layer must stay held"
        );
        assert!(
            !rt.engine.is_held("comms"),
            "the released input's layer drops"
        );
        // release X -> sniper drops.
        rt.release_for_input(&x);
        assert!(!rt.engine.is_held("sniper"));
    }

    /// A small runtime with ONE hypershift-style layer activated by input X (the stance tests).
    fn stance_rt(mode: crate::feel::LayerMode) -> (Runtime, Trigger) {
        let x = Trigger::Input {
            page: 0x09,
            usage: 0x01,
            pid: None,
        };
        let sidecar = vec![Rule::on_layer("hypershift", x.clone(), Action::Noop)];
        let mut rt = build_runtime_from(
            &Bindings::default(),
            &CastConfig::default(),
            &AppRules::default(),
            &sidecar,
        );
        rt.set_feel(&crate::feel::FeelConfig {
            hypershift: mode,
            ..Default::default()
        });
        (rt, x)
    }

    #[test]
    fn latch_stance_taps_on_and_off() {
        let (mut rt, x) = stance_rt(crate::feel::LayerMode::Latch);
        // tap 1: down toggles ON; the up edge is ignored.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        assert!(
            rt.engine.is_held("hypershift"),
            "latched on past the release"
        );
        // tap 2: down toggles OFF — activation just to deactivate, free.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        assert!(!rt.engine.is_held("hypershift"), "second tap unlatches");
    }

    #[test]
    fn smart_stance_tap_latches_hold_is_momentary() {
        let (mut rt, x) = stance_rt(crate::feel::LayerMode::Smart);
        // a TAP (down+up inside hold_ms): latches.
        rt.hold_for_input(&x);
        assert!(
            rt.engine.is_held("hypershift"),
            "held immediately on down (zero latency)"
        );
        rt.release_for_input(&x); // released instantly = a tap
        assert!(rt.engine.is_held("hypershift"), "a tap LATCHES the stance");
        // a second tap unlatches.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        assert!(
            !rt.engine.is_held("hypershift"),
            "tap again to drop the latch"
        );
        // a LONG hold is momentary: simulate by zeroing hold_ms so any press reads "long".
        rt.set_feel(&crate::feel::FeelConfig {
            hypershift: crate::feel::LayerMode::Smart,
            hold_ms: 0,
            ..Default::default()
        });
        rt.hold_for_input(&x);
        assert!(rt.engine.is_held("hypershift"));
        rt.release_for_input(&x);
        assert!(
            !rt.engine.is_held("hypershift"),
            "a long hold releases with the button"
        );
    }

    #[test]
    fn one_shot_stance_arms_for_exactly_one_fire() {
        let (mut rt, x) = stance_rt(crate::feel::LayerMode::OneShot);
        let other = Trigger::Input {
            page: 0x09,
            usage: 0x07,
            pid: None,
        };
        // arm: tap the layer trigger.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        assert!(rt.engine.is_held("hypershift"), "armed");
        // the arming press itself must not consume the shot.
        rt.note_fired(&x);
        assert!(
            rt.engine.is_held("hypershift"),
            "the arming trigger doesn't spend the shot"
        );
        // the next real trigger spends it.
        rt.note_fired(&other);
        assert!(!rt.engine.is_held("hypershift"), "spent after one fire");
        // arming then tapping again DISARMS (free deactivate), without firing anything.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        assert!(
            !rt.engine.is_held("hypershift"),
            "tap-tap = arm then disarm"
        );
    }

    #[test]
    fn tray_latch_respects_stance_latches() {
        let (mut rt, x) = stance_rt(crate::feel::LayerMode::Latch);
        rt.hold_for_input(&x); // stance-latched ON
        rt.release_for_input(&x);
        // the tray reconcile asserting OFF must NOT yank a stance-latched layer.
        rt.latch_layer("hypershift", false);
        assert!(
            rt.engine.is_held("hypershift"),
            "stance latch owns the layer"
        );
        // dropping the stance latch then lets the tray reconcile release it.
        rt.hold_for_input(&x);
        rt.release_for_input(&x);
        rt.latch_layer("hypershift", false);
        assert!(!rt.engine.is_held("hypershift"));
    }

    #[test]
    fn binding_rule_maps_typed_actions() {
        let mut g = bind(0x0C, 0xE9, "mic-gain");
        g.delta_pct = Some(4.0);
        g.device = Some("seiren".into());
        let r = binding_rule(&g).unwrap();
        assert_eq!(
            r.trigger,
            Trigger::Input {
                page: 0x0C,
                usage: 0xE9,
                pid: None
            }
        );
        assert_eq!(
            r.action,
            Action::MicGain {
                device: Some("seiren".into()),
                delta_pct: 4.0
            }
        );

        let mut m = bind(0x0B, 0x2F, "mic-mute");
        m.mode = Some("off".into());
        assert_eq!(
            binding_rule(&m).unwrap().action,
            Action::MicMute {
                device: None,
                mode: "off".into()
            }
        );

        let mut run = bind(0xF000, 0x01, "run");
        run.cmd = Some("echo hi".into());
        assert_eq!(
            binding_rule(&run).unwrap().action,
            Action::Run {
                cmd: "echo hi".into()
            }
        );

        let mut set = bind(0x0C, 0xE9, "mic-gain-set");
        set.pct = Some(42.0);
        assert_eq!(
            binding_rule(&set).unwrap().action,
            Action::MicGainSet {
                device: None,
                pct: 42.0
            }
        );
    }

    #[test]
    fn build_runtime_folds_every_source_into_one_engine() {
        let mut gain = bind(0x0C, 0xE9, "mic-gain");
        gain.delta_pct = Some(4.0);
        let mut set = bind(0x0C, 0xEA, "mic-gain-set");
        set.pct = Some(55.0);
        let bindings = Bindings {
            bindings: vec![gain, set],
        };

        // cast: a radial wedge + a glyph spell (+ a Noop wedge that must be skipped)
        let mut cast = CastConfig {
            radial: vec![
                Action::Key { key: "1".into() },
                Action::Noop, // skipped
            ],
            trigger: 0x06,
            ..Default::default()
        };
        cast.gestures
            .insert("bolt".into(), Action::Key { key: "f".into() });

        // app rule -> AppFocus -> ProfileSwitch
        let apps = AppRules {
            rules: vec![AppRule {
                app: "valorant".into(),
                profile: "fps".into(),
            }],
        };

        // a HyperShift sidecar rule (imported)
        let sidecar = vec![Rule::on_layer(
            "sniper",
            Trigger::Input {
                page: 0x09,
                usage: 0x05,
                pid: None,
            },
            Action::DpiSet { dpi: 400 },
        )];

        let rt = build_runtime_from(&bindings, &cast, &apps, &sidecar);

        assert_eq!(rt.cast_trigger, 0x06);

        // base rules: mic-gain + mic-gain-set + radial wedge 0 + gesture + the default cast
        // rhythm (teleport on tap-then-hold) + app-focus = 6
        assert_eq!(rt.engine.rules.len(), 6, "six base rules folded in");
        // one HyperShift layer from the sidecar
        assert_eq!(rt.engine.layers.len(), 1);
        assert_eq!(rt.engine.layers["sniper"].len(), 1);

        // the folded engine actually dispatches each source's trigger:
        let ctx = crate::macros::context::Context::default();
        // device input (mic-gain) -> 1 match
        assert_eq!(
            rt.engine
                .dispatch(
                    &Trigger::Input {
                        page: 0x0C,
                        usage: 0xE9,
                        pid: None
                    },
                    &ctx
                )
                .len(),
            1
        );
        // radial wedge 0 -> 1 match; wedge 1 (Noop) -> 0
        assert_eq!(
            rt.engine
                .dispatch(
                    &Trigger::RadialSector {
                        menu: CAST_MENU.into(),
                        sector: 0
                    },
                    &ctx
                )
                .len(),
            1
        );
        assert_eq!(
            rt.engine
                .dispatch(
                    &Trigger::RadialSector {
                        menu: CAST_MENU.into(),
                        sector: 1
                    },
                    &ctx
                )
                .len(),
            0,
            "an unbound (Noop) wedge contributes no rule"
        );
        // the default cast RHYTHM is folded as a first-class Cast trigger -> its bound action.
        assert_eq!(
            rt.engine.dispatch(&Trigger::Cast { taps: 1 }, &ctx).len(),
            1,
            "the tap-then-hold rhythm dispatches through the one engine"
        );
        // gesture -> 1
        assert_eq!(
            rt.engine
                .dispatch(
                    &Trigger::Gesture {
                        name: "bolt".into()
                    },
                    &ctx
                )
                .len(),
            1
        );
        // app focus (substring) -> 1, and the action is the ProfileSwitch intent
        let log = rt.engine.dispatch(
            &Trigger::AppFocus {
                app: "valorant.exe".into(),
            },
            &ctx,
        );
        assert_eq!(log.len(), 1);
        let r = rt
            .engine
            .resolve_top(&Trigger::AppFocus {
                app: "valorant.exe".into(),
            })
            .unwrap();
        assert_eq!(
            r.action.intent(),
            Some(crate::action::Intent::ProfileSwitch("fps".into()))
        );
    }

    #[test]
    fn hypershift_sidecar_only_fires_while_held() {
        let sidecar = vec![Rule::on_layer(
            "sniper",
            Trigger::Input {
                page: 0x09,
                usage: 0x05,
                pid: None,
            },
            Action::DpiSet { dpi: 400 },
        )];
        let mut rt = build_runtime_from(
            &Bindings::default(),
            &CastConfig::default(),
            &AppRules::default(),
            &sidecar,
        );
        let input = Trigger::Input {
            page: 0x09,
            usage: 0x05,
            pid: None,
        };
        assert_eq!(
            rt.engine.resolve(&input).len(),
            0,
            "layer rule dormant until held"
        );
        rt.engine.hold("sniper");
        assert_eq!(
            rt.engine.resolve(&input).len(),
            1,
            "held -> the layer rule dispatches"
        );
    }

    #[test]
    fn app_focus_rule_carries_profile_switch_intent() {
        let apps = AppRules {
            rules: vec![AppRule {
                app: "code".into(),
                profile: "work".into(),
            }],
        };
        let rt = build_runtime_from(&Bindings::default(), &CastConfig::default(), &apps, &[]);
        let fired = Trigger::AppFocus {
            app: "code.exe".into(),
        };
        let rule = rt
            .engine
            .resolve_top(&fired)
            .expect("app rule matches by substring");
        assert_eq!(
            rule.action,
            Action::ProfileSwitch {
                name: "work".into()
            }
        );
    }

    // keep `Direction` import used (it documents the intent surface the daemon drives)
    #[test]
    fn dpi_cycle_action_exposes_intent() {
        let a = Action::DpiCycle { dir: Direction::Up };
        assert!(a.intent().is_some());
    }
}

#[cfg(windows)]
mod win {
    use super::{ControlEvent, PROBE_PAGES};
    use std::ffi::c_void;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Devices::HumanInterfaceDevice::{
        HidP_GetUsages, HidP_Input, HidP_MaxUsageListLength, HIDP_STATUS_SUCCESS,
    };
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
        RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_DEVICENAME, RIDI_PREPARSEDDATA,
        RID_INPUT, RIM_TYPEHID,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE, WM_INPUT,
    };

    unsafe fn device_path(hdev: isize) -> String {
        let mut size: u32 = 0;
        GetRawInputDeviceInfoW(hdev as _, RIDI_DEVICENAME, std::ptr::null_mut(), &mut size);
        if size == 0 || size > 1024 {
            return String::new();
        }
        let mut buf = vec![0u16; size as usize];
        let got = GetRawInputDeviceInfoW(
            hdev as _,
            RIDI_DEVICENAME,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        );
        if got == u32::MAX {
            return String::new();
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    unsafe fn preparsed_data(hdev: isize) -> Vec<u8> {
        let mut size: u32 = 0;
        GetRawInputDeviceInfoW(
            hdev as _,
            RIDI_PREPARSEDDATA,
            std::ptr::null_mut(),
            &mut size,
        );
        if size == 0 || size > 1 << 20 {
            return Vec::new();
        }
        let mut buf = vec![0u8; size as usize];
        let got = GetRawInputDeviceInfoW(
            hdev as _,
            RIDI_PREPARSEDDATA,
            buf.as_mut_ptr() as *mut c_void,
            &mut size,
        );
        if got == u32::MAX {
            return Vec::new();
        }
        buf
    }

    /// Decode the active usages on one page from a report, given its preparsed data.
    unsafe fn decode(preparsed: &[u8], report: &mut [u8], page: u16) -> Vec<u16> {
        if preparsed.is_empty() {
            return Vec::new();
        }
        let pp = preparsed.as_ptr() as isize; // PHIDP_PREPARSED_DATA
        let max = HidP_MaxUsageListLength(HidP_Input, page, pp);
        if max == 0 {
            return Vec::new();
        }
        let mut list = vec![0u16; max as usize];
        let mut len = max;
        let st = HidP_GetUsages(
            HidP_Input,
            page,
            0,
            list.as_mut_ptr(),
            &mut len,
            pp,
            report.as_mut_ptr(),
            report.len() as u32,
        );
        if st == HIDP_STATUS_SUCCESS {
            list.truncate(len as usize);
            list
        } else {
            Vec::new()
        }
    }

    pub fn listen(
        seconds: Option<u64>,
        stop: &std::sync::atomic::AtomicBool,
        esc_stops: bool,
        mut on_event: impl FnMut(&ControlEvent),
        mut on_tick: impl FnMut(),
    ) {
        unsafe {
            let cls: Vec<u16> = "Static\0".encode_utf16().collect();
            // A normal top-level (but never-shown) window. NOT a message-only window
            // (HWND_MESSAGE): those do not reliably receive WM_INPUT with RIDEV_INPUTSINK.
            let hwnd = CreateWindowExW(
                0,
                cls.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(), // top-level (no parent)
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if hwnd.is_null() {
                println!("failed to create listener window");
                return;
            }

            // Register the control usage pages: Consumer (knob/media) + Telephony (mute).
            let rids = [
                RAWINPUTDEVICE {
                    usUsagePage: 0x0C,
                    usUsage: 0x01,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 0x0B,
                    usUsage: 0x05,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
                RAWINPUTDEVICE {
                    usUsagePage: 0x0B,
                    usUsage: 0x01,
                    dwFlags: RIDEV_INPUTSINK,
                    hwndTarget: hwnd,
                },
            ];
            if RegisterRawInputDevices(
                rids.as_ptr(),
                rids.len() as u32,
                std::mem::size_of::<RAWINPUTDEVICE>() as u32,
            ) == 0
            {
                println!("RegisterRawInputDevices failed");
                DestroyWindow(hwnd);
                return;
            }

            let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
            let dbg = std::env::var("NEURON_DEBUG").is_ok();
            let mut n_input = 0u32;
            let mut n_hid = 0u32;
            let start = Instant::now();
            while seconds.map_or(true, |s| start.elapsed().as_secs() < s) {
                // A GUI worker thread (or any caller of `listen_until`) flips this to tear the
                // loop down cleanly from another thread; the CLI passes a flag that is never set.
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                // ESC ends INTERACTIVE listens only (watch / press-to-bind). A resident engine
                // (the GUI's live dispatch) must survive ESC — it's the weave-cancel key AND the
                // close-the-game-menu key; dying on it silently killed every cast until restart.
                if esc_stops && (GetAsyncKeyState(0x1B) as u16 & 0x8000) != 0 {
                    break;
                }
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                    if msg.message == WM_INPUT {
                        n_input += 1;
                        let mut size: u32 = 0;
                        GetRawInputData(
                            msg.lParam as HRAWINPUT,
                            RID_INPUT,
                            std::ptr::null_mut(),
                            &mut size,
                            header,
                        );
                        if size > 0 && size < 4096 {
                            let mut buf = vec![0u8; size as usize];
                            let got = GetRawInputData(
                                msg.lParam as HRAWINPUT,
                                RID_INPUT,
                                buf.as_mut_ptr() as *mut c_void,
                                &mut size,
                                header,
                            );
                            if got != u32::MAX && got > 0 {
                                let ri = &*(buf.as_ptr() as *const RAWINPUT);
                                if ri.header.dwType == RIM_TYPEHID {
                                    n_hid += 1;
                                    let hdev = ri.header.hDevice as isize;
                                    let path = device_path(hdev);
                                    if dbg {
                                        let n =
                                            (ri.data.hid.dwSizeHid * ri.data.hid.dwCount) as usize;
                                        let bytes = std::slice::from_raw_parts(
                                            ri.data.hid.bRawData.as_ptr(),
                                            n.min(16),
                                        );
                                        let hex: String =
                                            bytes.iter().map(|b| format!("{b:02X} ")).collect();
                                        eprintln!("    [dbg] HID ev {n}B  {hex}  dev={path}");
                                    }
                                    if path.to_lowercase().contains("vid_1532") {
                                        let pid = path
                                            .to_lowercase()
                                            .split("pid_")
                                            .nth(1)
                                            .map(|s| s.chars().take(4).collect::<String>())
                                            .unwrap_or_default();
                                        let n =
                                            (ri.data.hid.dwSizeHid * ri.data.hid.dwCount) as usize;
                                        let mut report = std::slice::from_raw_parts(
                                            ri.data.hid.bRawData.as_ptr(),
                                            n,
                                        )
                                        .to_vec();
                                        let pp = preparsed_data(hdev);
                                        let mut hits = Vec::new();
                                        for &page in &PROBE_PAGES {
                                            for u in decode(&pp, &mut report, page) {
                                                hits.push((page, u));
                                            }
                                        }
                                        let ev = ControlEvent {
                                            pid,
                                            hits,
                                            raw: report,
                                        };
                                        on_event(&ev);
                                    }
                                }
                            }
                        }
                    }
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                on_tick();
                std::thread::sleep(Duration::from_millis(5));
            }
            if dbg {
                eprintln!("    [dbg] WM_INPUT msgs={n_input}  HID events={n_hid}");
            }
            DestroyWindow(hwnd);
        }
    }
}
