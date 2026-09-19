// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The WEAVE/BEACON service — the ONE owner of the cast trigger, with two faces:
//!
//! **LIVE SPELLWEAVING** (the idle face): whenever no beacon is pending, this thread watches the
//! cast trigger; hold it and flick/draw, and the stroke resolves through the one cast resolver
//! (radial ⊆ glyph) and fires through the live dispatch Engine — spellweaving as a real keybind.
//!
//! **BEACON** (the prompt face) — the GUI side of two-part ("prime, then activate") macros:
//!
//! A running Python macro calls `neuron.ask("deploy?")` and blocks (its own sidecar worker only).
//! The ask travels host-ward as a `prompt` frame, the Macro Host reader surfaces it as a
//! [`BeaconEvent::Ask`], and THIS service presents it SIGNAL-FIRST: a quiet one-line strip at the
//! top of the cursor's monitor (the question + the grammar) — **the wheel never forces itself
//! open**. When the user engages (holds the cast trigger), the ask wheel materializes at the
//! cursor: flick west (your accent) = YES, east (raw material) = NO, vertical = PASS (the macro gets
//! its `default`). A bail — ESC mid-weave, a deadzone release — costs nothing; the strip returns
//! and the beacon keeps waiting. Nothing modal, no focus steal, no dialog box — the overlay is
//! click-through and only a deliberate committed flick can ever resolve a prompt.
//!
//! Asks queue: one shows at a time, the header pill counts the rest. A sidecar-side timeout (or a
//! sidecar respawn) RETIRES the prompt — the [`BeaconEvent::Retire`]/[`RetireAll`] routes set the
//! presented prompt's stop flag and the capture withdraws without committing anything.
//!
//! Threading: a ROUTER thread drains the Macro Host's event stream and only touches shared state
//! (so a retire can always land instantly, even mid-capture), and a PRESENTER thread works the
//! queue one prompt at a time (the blocking weave capture lives here). UI mirroring crosses to the
//! Slint thread via `invoke_from_event_loop`, same as every other worker in the app.
//!
//! SAFE-mode note: answering a beacon synthesizes NO input — ask/notify are deliberately not
//! arm-gated, so a macro can converse with the user even in SAFE mode. The capture path only
//! READS input state (the same promise as press-to-bind).

use crate::ui::{AppWindow, State};
use neuron::macros::{macro_host, BeaconEvent};
use slint::ComponentHandle;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// While the GUI editor owns the trigger (recording a glyph / testing the wheel), the beacon
/// presenter STANDS DOWN — one trigger press must never feed two captures (a preview flick that
/// silently answered a pending ask would be the worst kind of surprise).
static EDITOR_WEAVE: AtomicBool = AtomicBool::new(false);

/// A one-shot INSTRUMENT REQUEST (0 = none, 1 = teleport, 2 = whiteboard) — the n-style side
/// door: the GUI's try-buttons (and, later, bound actions) open an instrument without its
/// rhythm. Teleport primes onto the next PLAIN HOLD of the cast trigger; whiteboard opens its
/// session directly. The weave watcher consumes it at its next iteration (its idle capture
/// cancels the instant a request lands, so "next iteration" is now).
static INSTRUMENT_REQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Ask the weave service to open an instrument (1 = teleport primed on a hold, 2 = whiteboard,
/// 3 = the dial — its target rides in [`DIAL_TARGET`]).
pub fn request_instrument(id: u32) {
    INSTRUMENT_REQ.store(id, Ordering::SeqCst);
}

/// The primed dial's target (0 = output volume, 1 = mic volume), read when instrument 3 lands.
static DIAL_TARGET: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Prime the analog DIAL for `target` — the next hold of the cast trigger becomes the slide.
pub fn prime_dial(target: neuron::action::DialTarget) {
    use neuron::action::DialTarget;
    DIAL_TARGET.store(
        match target {
            DialTarget::OutputVolume => 0,
            DialTarget::MicVolume => 1,
        },
        Ordering::SeqCst,
    );
    INSTRUMENT_REQ.store(3, Ordering::SeqCst);
}

/// RAII guard the editor takes around its own weave captures. While any guard lives, a pending
/// beacon keeps waiting quietly (its strip stays up; its timeout still applies) and re-arms the
/// moment the editor lets go.
pub struct EditorWeave;

impl EditorWeave {
    pub fn engage() -> Self {
        EDITOR_WEAVE.store(true, Ordering::SeqCst);
        EditorWeave
    }
}

impl Drop for EditorWeave {
    fn drop(&mut self) {
        EDITOR_WEAVE.store(false, Ordering::SeqCst);
    }
}

/// Whether the editor currently owns the trigger — surfaces that share the trigger key (the
/// whiteboard session, now on its own thread) PAUSE while this is true so one press never
/// feeds two listeners.
pub fn editor_weave_active() -> bool {
    EDITOR_WEAVE.load(Ordering::SeqCst)
}

/// True while a beacon prompt is being PRESENTED on the trigger (the answer wheel / signal
/// strip owns holds of the cast key). Same pause contract as [`editor_weave_active`].
static BEACON_PRESENTING: AtomicBool = AtomicBool::new(false);

/// True from the instant the SYSTEM panel's "test" button mock-fires a beacon until that beacon
/// is answered/retired (queue + current drains to 0 — cleared in [`mirror_count`]). The per-macro
/// sidecar worker is SERIAL: without this gate, rapid test clicks QUEUE mock fires behind the one
/// blocked in `neuron.ask`, so answering one instantly raises the next ("the beacon never stops"),
/// and the presenter — pinned in `present()` with `current = Some` for the whole backlog — never
/// reaches the idle arm that runs the radial/spellweave, starving it. The gate makes test fires
/// strictly one-at-a-time, so the backlog (and the lag/starvation it causes) can't form.
pub static TEST_BEACON_INFLIGHT: AtomicBool = AtomicBool::new(false);

pub fn beacon_presenting() -> bool {
    BEACON_PRESENTING.load(Ordering::SeqCst)
}

/// THE OWNERSHIP PREDICATE for the cast trigger: may the live-weave capture path arm right now?
/// `whiteboard`'s own session pause spells this out inline as
/// `editor_weave_active() || beacon_presenting()`; this is the same rule, named once, so
/// `live_weave`'s entry gate and any sibling capture-owning surface read one fact instead of two
/// copies that could drift apart. (A *queued-but-not-yet-presenting* ask preempts live weave too,
/// but structurally — the presenter loop in [`start`] pops the queue before it would ever call
/// `live_weave`; [`prompt_pending`] is that check, pinned separately in tests below.)
fn weave_may_capture() -> bool {
    !EDITOR_WEAVE.load(Ordering::SeqCst) && !BEACON_PRESENTING.load(Ordering::SeqCst)
}

/// One queued ask, as the presenter sees it.
struct Prompt {
    pid: u64,
    macro_id: String,
    text: String,
    /// The answer wheel's options (the wedges) — `["yes","no"]` for a plain ask, N labels for a
    /// `choose`, one for a `confirm`. The whole prompt system is this list + the radial core.
    options: Vec<String>,
    detail: String,
}

#[derive(Default)]
struct Q {
    queue: VecDeque<Prompt>,
    /// The pid being presented right now + its stop flag (set = withdraw the capture).
    current: Option<(u64, Arc<AtomicBool>)>,
}

type Shared = Arc<(Mutex<Q>, Condvar)>;

/// Hard cap on the pending-ask backlog. The presenter (`start()`'s presenter loop) services
/// EXACTLY ONE prompt at a time — `queue.pop_front()` then `present()` blocks the whole thread
/// until that one ask is answered/retired — so the queue is purely a "how many more are behind
/// this one" backlog, never a worklist multiple threads drain in parallel. A macro calling
/// `neuron.ask` in a tight loop with a UI attached that never answers (the no-UI case is already
/// auto-dismissed at the Macro Host, see `macro_host.rs`'s `Some("prompt")` handler) would
/// otherwise grow `Q.queue` without bound. 32 is generous headroom for legitimate concurrent
/// multi-macro asks (the header pill already shows the backlog count to the user) while keeping
/// the worst case bounded to `MAX_PENDING_PROMPTS * size_of(Prompt)` instead of unbounded.
const MAX_PENDING_PROMPTS: usize = 32;

/// Enqueue an ask, or — once the backlog is already at [`MAX_PENDING_PROMPTS`] — refuse it
/// HONESTLY instead of growing the queue further: answer it `None`, the exact "passed/dismissed"
/// shape `present()` sends below for a real PASS (and the shape the Macro Host's own no-UI
/// auto-dismiss uses), so the blocked `neuron.ask` in the sidecar returns its `default` at once
/// instead of riding out its full timeout budget behind a backlog it will never see. Refusing the
/// NEWEST ask (rather than evicting an older queued one to make room) is the honest policy: an
/// older queued prompt may be the very next one shown, or one a human is already mid-flick
/// answering — silently voiding it would be a surprise no different from a lost message. A new ask
/// arriving at the cap is, structurally, the LEAST likely of the lot to ever be seen (it would sit
/// 32-deep in the backlog), so it is the one that costs least to refuse.
///
/// Returns whether the prompt was queued (`false` = refused at the cap).
fn try_enqueue(shared: &Shared, p: Prompt) -> bool {
    let (q, cv) = &**shared;
    let mut g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if g.queue.len() >= MAX_PENDING_PROMPTS {
        drop(g);
        macro_host().answer(p.pid, None);
        return false;
    }
    g.queue.push_back(p);
    drop(g);
    cv.notify_all();
    true
}

/// Start the beacon service (router + presenter threads). Call ONCE from the real `main` run path,
/// after the live runtime starts — never from tests (it talks to the real Macro Host + input state).
pub fn start(weak: slint::Weak<AppWindow>) {
    // the audio widget readings refresh OFF the weave thread (Core-Audio COM can hang on a bad
    // endpoint; the weave capture must never block on it — see `audio_cache`).
    #[cfg(windows)]
    audio_cache::ensure();
    let shared: Shared = Arc::new((Mutex::new(Q::default()), Condvar::new()));
    let rx = macro_host().beacon_events();

    // ── ROUTER: drain MacroHost beacon events; never blocks on UI or capture ──
    {
        let shared = shared.clone();
        let weak = weak.clone();
        crate::worker::spawn_detached("neuron-beacon-router", move || {
                while let Ok(ev) = rx.recv() {
                    neuron::prof::bump(&neuron::prof::ROUTER_EVENT);
                    match ev {
                        BeaconEvent::Ask {
                            pid,
                            macro_id,
                            text,
                            options,
                            detail,
                            ..
                        } => {
                            let queued = try_enqueue(
                                &shared,
                                Prompt {
                                    pid,
                                    macro_id,
                                    text,
                                    options,
                                    detail,
                                },
                            );
                            // a refusal doesn't change the shown count — only mirror on a real enqueue.
                            if queued {
                                mirror_count(&weak, &shared);
                            }
                        }
                        BeaconEvent::Retire { pid } => {
                            let (q, _) = &*shared;
                            let mut g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            g.queue.retain(|p| p.pid != pid);
                            if let Some((cur, stop)) = &g.current {
                                if *cur == pid {
                                    stop.store(true, Ordering::SeqCst);
                                }
                            }
                            drop(g);
                            mirror_count(&weak, &shared);
                        }
                        BeaconEvent::RetireAll => {
                            let (q, _) = &*shared;
                            let mut g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            g.queue.clear();
                            if let Some((_, stop)) = &g.current {
                                stop.store(true, Ordering::SeqCst);
                            }
                            drop(g);
                            mirror_count(&weak, &shared);
                        }
                        BeaconEvent::Notify { macro_id, text } => {
                            // fire-and-forget status: the over-game CARD (the same surface as
                            // confirmations — gated by the Kind::Macro notif pref) AND the in-app
                            // status line / macro-log readout.
                            crate::notifs::post_macro(&macro_id, &text);
                            let w = weak.clone();
                            let line = format!("{macro_id}: {text}");
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = w.upgrade() {
                                    let st = app.global::<State>();
                                    st.set_status_line(line.into());
                                    st.set_status_kind("info".into());
                                    st.set_status_stale(false);
                                }
                            });
                        }
                    }
                }
                // the Macro Host replaced this listener (a new beacon_events() call) — retire quietly.
            });
    }

    // ── PRESENTER: the ONE owner of the cast trigger. Alternates between two faces:
    //   * a beacon is pending  -> present it (the ask wheel answers the trigger);
    //   * nothing pending      -> LIVE WEAVE WATCH (the trigger weaves — radial flicks + glyphs
    //                             fire for real, injected into the live dispatch Engine).
    // One thread, one capture at a time = double-consume of a press is impossible by
    // construction; the editor's EditorWeave guard pre-empts both faces.
    crate::worker::spawn_detached("neuron-weave-presenter", move || {
            // ONE persistent overlay serves both faces (ask wheel + weave sigil) — no window or
            // render-thread churn per weave; begin()/end() show and fade it as needed.
            let overlay = crate::overlay::SpellOverlay::spawn();
            loop {
                // THE ENGINE MUST NOT DIE. Every other long-lived session (whiteboard, knockback)
                // is panic-walled; the weave watcher is the ONE thread whose death silently bricks
                // ALL spellcasting until restart ("it just stops responding, and stays dead"). Wall
                // each cycle: a Rust panic — or a mutex another thread poisoned as it unwound — logs
                // a flight breadcrumb, releases any half-claimed beacon, and the watch carries on.
                // (Hardware/SEH faults still fall through to the phoenix restart, as before.)
                neuron::prof::bump(&neuron::prof::PRESENTER_CYCLE);
                let cycle = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let next = {
                        let (q, _) = &*shared;
                        let mut g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        g.queue.pop_front().map(|p| {
                            let stop = Arc::new(AtomicBool::new(false));
                            g.current = Some((p.pid, stop.clone()));
                            (p, stop)
                        })
                    };
                    match next {
                        Some((p, stop)) => {
                            present(&weak, &shared, &overlay, &p, &stop);
                            {
                                let (q, _) = &*shared;
                                q.lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .current = None;
                            }
                            mirror_count(&weak, &shared);
                        }
                        None => {
                            #[cfg(windows)]
                            live_weave(&weak, &shared, &overlay);
                            #[cfg(not(windows))]
                            {
                                // no weave capture off-Windows yet — just wait for prompts.
                                let (q, cv) = &*shared;
                                let g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                let _ = cv.wait_timeout(g, std::time::Duration::from_millis(500));
                            }
                        }
                    }
                }));
                if cycle.is_err() {
                    // a single cycle unwound — keep the engine alive. Drop any beacon we half-claimed
                    // so the queue can't wedge on a dead `current`, then resume the watch.
                    crate::flight::trace("weave", "cycle panicked \u{2014} engine survived", 0);
                    let (q, _) = &*shared;
                    q.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .current = None;
                    // a panic mid-present would otherwise strand a test beacon's gate (the queued==0
                    // clear in mirror_count never runs), locking the test button — release it here.
                    TEST_BEACON_INFLIGHT.store(false, Ordering::SeqCst);
                    mirror_count(&weak, &shared);
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            }
        });
}

/// THE LIVE WEAVE WATCHER — what makes spellweaving a real keybind, not a demo button. Runs
/// whenever no beacon is pending: waits for the user's cast activation rhythm, streams the
/// stroke to the overlay, resolves it through the ONE cast resolver (radial ⊆ glyph, spell
/// assist included), and INJECTS the resolved trigger into the live dispatch worker — so a
/// flick or a glyph composes with `HyperShift` layers, intents, turbo, SAFE mode and the live
/// readout exactly like a hardware button. Stands down instantly when a beacon arrives, when
/// the editor takes the trigger, or when the config generation moves (a re-bound trigger or
/// edited rhythm applies on the very next weave).
#[cfg(windows)]
fn live_weave(
    weak: &slint::Weak<AppWindow>,
    shared: &Shared,
    overlay: &crate::overlay::SpellOverlay,
) {
    neuron::prof::bump(&neuron::prof::LIVE_WEAVE);
    // The EDITOR owns the trigger: don't even ARM a capture. Raw-input registration is
    // per-process and LAST-WINS — a capture we start here (even one the cancel kills in 3ms)
    // STEALS the editor capture's motion stream and orphans the registration on a window we
    // immediately destroy. This was the "record glyph doesn't work at all" bug: the recorder's
    // rhythm still activated (key polling), but its drain never saw another count of motion.
    // flight heartbeat: the weave presenter proves it's alive each arming pass (and again
    // inside the cancel predicate below, which ticks every few ms even while a capture blocks
    // — so a wedged capture reads as alive, a dead thread reads as a stall).
    crate::flight::pulse(crate::flight::organ::WEAVE);
    if !weave_may_capture() {
        std::thread::sleep(std::time::Duration::from_millis(50));
        return;
    }
    let gen = crate::dispatch::reload_generation();
    let cast = neuron::cast::CastConfig::load();
    let vault = neuron::gesture::Vault::load();
    let feel = neuron::feel::FeelConfig::load();

    // an INSTRUMENT REQUEST (try-button / bound action) jumps the rhythm queue: whiteboard opens
    // its session directly; teleport narrows the slot set to ONE plain-hold slot — the exact same
    // capture/ghost/scry/commit flow below, just with the easiest possible entry.
    let request = INSTRUMENT_REQ.swap(0, Ordering::SeqCst);

    // ── KNOCKBACK coexistence ── the session owns ONLY its drum key, never the whole weave
    // service. Requests must keep flowing here even while it plays — recasting knockback IS the
    // toggle that leaves it (a starved request queue was the "everything's dead" bug: the toggle
    // never landed, and the stale request silently re-entered the mode right after ESC).
    if request == 5 {
        crate::knockback::toggle(weak);
        return;
    }
    // the control the live session actually armed (None = no session). Slots on this control
    // stand down below; instruments that would ride it get a friendly refusal instead of a fight.
    let kb_ctl = crate::knockback::owned_ctl();
    if (request == 1 || request == 3 || request == 6) && kb_ctl == Some(cast.trigger) {
        post_status(
            weak,
            "the familiar holds that trigger \u{2014} esc (or recast knockback) to leave the duet first".into(),
        );
        return;
    }

    // the validated slot set — every instrument's own binding, same-key sharing by rhythm.
    let (slots, complaints) = cast.mode_slots();
    // a config conflict is reported ONCE per config generation, then we stay quiet about it.
    static COMPLAINED_GEN: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(u64::MAX);
    if !complaints.is_empty() && COMPLAINED_GEN.swap(gen, Ordering::SeqCst) != gen {
        post_status(weak, complaints.join(" \u{00b7} "));
    }
    let cap_slots: Vec<neuron::glyph::CaptureSlot> = match request {
        // primed teleport: the next plain hold of the cast trigger IS the drag.
        1 => vec![neuron::glyph::CaptureSlot {
            id: 1,
            ctl: cast.trigger,
            taps: 0,
        }],
        // primed dial: the next plain hold of the cast trigger IS the slide.
        3 => vec![neuron::glyph::CaptureSlot {
            id: 3,
            ctl: cast.trigger,
            taps: 0,
        }],
        // primed control center: the next plain hold of the cast trigger opens the glance.
        6 => vec![neuron::glyph::CaptureSlot {
            id: 6,
            ctl: cast.trigger,
            taps: 0,
        }],
        _ => slots
            .iter()
            .map(|s| neuron::glyph::CaptureSlot {
                // the slot's own contract id: 0 weave, 1 teleport, 2 whiteboard, 100+taps =
                // "fire via Trigger::Cast{taps}" (any other bound action — routed via the spine).
                id: s.capture_id(),
                ctl: s.ctl,
                taps: s.taps,
            })
            .collect(),
    };

    // gesture-only mode draws the rune sigil; radial/auto show the wedge wheel (a flick is the
    // common case and the wheel is the aim aid — a drawn glyph still gets the comet trail).
    // The LIVE wheel carries its wedge labels — and wedges bound to live state are WIDGETS:
    // a volume wedge says the volume, a mic wedge says whether you're hot, the active profile
    // wears its dot. Read once per weave (host-side, instant), refreshed by the dial.
    let weave_mode = match cast.mode {
        neuron::cast::Mode::Gesture => crate::overlay::WeaveMode::Glyph { hint: None },
        _ => crate::overlay::WeaveMode::Radial {
            sectors: cast.sectors.max(1) as u8,
            widgets: radial_widgets(&cast),
            fans: radial_fans(&cast),
        },
    };
    let active = std::cell::Cell::new(u32::MAX);
    // a "fire via the spine" rhythm landed (capture id >= 100): carries the tap count so, after
    // the capture ends WITHOUT a drawing session, we inject `Trigger::Cast { taps }` and the live
    // Engine resolves it to the bound action — the proof that a rhythm is a first-class trigger.
    let fire_taps = std::cell::Cell::new(None::<u8>);
    let snap: std::cell::RefCell<Option<crate::teleport::Snapshot>> = std::cell::RefCell::new(None);
    // TELEPORT AIM state: the DEPTH DIAL's offset into the desk stack (scroll while dragging =
    // descend through overlapped/fullscreen windows; resets when the ghost moves to a new
    // column), the current pick + when it was picked (every window EARNS its bloom by dwell —
    // the zoom-in feel), its warp point, whether it's a realm (other-desktop) pick, and what's
    // currently bloomed in the portal.
    let ghost_d = std::cell::Cell::new((0.0f64, 0.0f64));
    let hover = std::cell::Cell::new(0isize);
    let depth = std::cell::Cell::new(0i32);
    let aimed = std::cell::Cell::new(0isize);
    let aimed_rect = std::cell::Cell::new((0i32, 0i32, 0i32, 0i32));
    // the aimed blob's minimap CELL in screen px — the scry bloom hugs THIS (its own cell), so the
    // peek opens right beside the blob it mirrors instead of a fixed reach from the cursor.
    let aimed_cell = std::cell::Cell::new((0i32, 0i32, 0i32, 0i32));
    let aimed_warp = std::cell::Cell::new((0i32, 0i32));
    let aimed_realm = std::cell::Cell::new(false);
    let sel_since = std::cell::Cell::new(std::time::Instant::now());
    let bloomed = std::cell::Cell::new(0isize);
    // SPECTRAL VERB state: a right-click GRABS the aimed window (it rides the ghost until the
    // button releases onto a drop target); a left-click SUMMONS it to the weave origin. A fired
    // verb ends the weave — the verb IS the commit. Flick velocity is an EMA of the ghost's
    // motion (counts/ms) so an up-flick at release reads as intent, not position.
    let carrying = std::cell::Cell::new(0isize);
    let carry_rect = std::cell::Cell::new((0i32, 0i32, 0i32, 0i32));
    let carry_realm = std::cell::Cell::new(false);
    let carry_paint = std::cell::Cell::new(std::time::Instant::now());
    let flick = std::cell::Cell::new((0.0f64, 0.0f64));
    let flick_at = std::cell::Cell::new(std::time::Instant::now());
    let flick_ghost = std::cell::Cell::new((0.0f64, 0.0f64));
    let verb_done: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
    // the WEAVE DIAL's aim: the stroke's current net displacement (which wedge the wheel turns)
    let weave_aim = std::cell::Cell::new((0.0f64, 0.0f64));
    // LIVE-PREDICT THROTTLE (gesture mode): re-running `analyze` + `vault.predict` (a fresh DTW
    // matrix per template over the WHOLE growing stroke) on EVERY drain tick (~60-125 Hz) is
    // mostly redundant — the becoming-glyph HINT barely changes tick-to-tick. Gate the recompute
    // to at most ~60 ms, OR sooner if the tip moves a min straight-line distance from the last (so a fast
    // flick still firms up promptly). Between recomputes the last-sent hint stays live in the
    // render thread (a Push never clears it), so the displayed forecast is identical to what an
    // unthrottled recompute would have shown — only the wasted DTW passes are skipped.
    let last_predict_at = std::cell::Cell::new(None::<std::time::Instant>);
    let last_predict_tip = std::cell::Cell::new((0.0f64, 0.0f64));
    // the ANALOG DIAL (instrument 3): the live value 0..1, the last stroke point (for per-frame
    // velocity), and a smoothed turn-speed (drives the gauge's pulse + coarse/fine label).
    let dial = std::cell::RefCell::new(crate::dialweave::Dial::default());
    // THE CONTROL CENTER (instrument 6): the system-state snapshot taken once at activation (it
    // reads live Win32 — net/wifi/bluetooth — so it's taken on the press, not re-read per frame).
    let control: std::cell::RefCell<Option<crate::control::Glance>> = std::cell::RefCell::new(None);
    // whiteboard by request TOGGLES its session — which lives on its OWN thread, so the weave
    // service (teleport, glyphs, beacons) keeps working while the board is open. The weave
    // thread being captive inside the session was the "whiteboard bricks the spell system" bug.
    if request == 2 {
        // the board would fight the familiar for a shared draw key — refuse with directions.
        let board_key = slots
            .iter()
            .find(|s| s.action == neuron::action::Action::Whiteboard)
            .map_or(cast.trigger, |s| s.ctl);
        if kb_ctl == Some(board_key) {
            post_status(
                weak,
                "the familiar holds that trigger \u{2014} esc (or recast knockback) to leave the duet first".into(),
            );
            return;
        }
        crate::whiteboard::toggle(weak);
        return;
    }
    // ── while a whiteboard session is live, the board OWNS its use key ──
    // The session and the weave service are separate threads now; without this, a draw-hold
    // double-fires as a cast ("i can't draw, it just activates the spellweaving"). Slots on
    // OTHER keys keep working — only the board's key stands down.
    let board_ctl = crate::whiteboard::active().then(|| {
        slots
            .iter()
            .find(|s| s.action == neuron::action::Action::Whiteboard)
            .map_or(cast.trigger, |s| s.ctl)
    });
    if request == 1 {
        if board_ctl == Some(cast.trigger) {
            post_status(
                weak,
                "the whiteboard holds that trigger \u{2014} close the board (esc) to teleport"
                    .into(),
            );
            return;
        }
        post_status(
            weak,
            "teleport primed \u{2014} hold your trigger and drag the ghost (esc backs out)".into(),
        );
    }
    if request == 6 {
        if board_ctl == Some(cast.trigger) {
            post_status(
                weak,
                "the whiteboard holds that trigger \u{2014} close the board (esc) to glance".into(),
            );
            return;
        }
        post_status(
            weak,
            "control center primed \u{2014} hold your trigger to glance (esc backs out)".into(),
        );
    }
    // apply the board's key ownership: drop every slot on the board's use key. The knockback
    // session's drum key stands down the same way — slots on OTHER keys keep weaving while the
    // duet plays, so radial/teleport/whiteboard on their own triggers never go dark.
    let cap_slots: Vec<neuron::glyph::CaptureSlot> = cap_slots
        .into_iter()
        .filter(|s| board_ctl != Some(s.ctl) && kb_ctl != Some(s.ctl))
        .collect();
    if cap_slots.is_empty() {
        // every slot shares an owned key — nothing to listen for until that session closes.
        std::thread::sleep(std::time::Duration::from_millis(60));
        return;
    }

    let cancel = || {
        crate::flight::pulse(crate::flight::organ::WEAVE);
        // HEARTBEAT the click-guard's deadman from the SAME pulse: if this predicate stops being
        // called (a stalled capture — exactly when WEAVE goes silent), the guard self-heals and
        // the mouse comes back. A live weave renews it every few ms, so it stays solid in use.
        crate::teleport::click_guard::renew();
        // the aim clock rides the cancel predicate — it ticks every few ms inside the capture
        // loops whether or not the mouse moves, which is exactly what hold-still gestures need.
        let mut verb_fired = false;
        if active.get() == 1 {
            if let Some(s) = snap.borrow().as_ref() {
                let d = ghost_d.get();
                let o = s.project(s.cursor.0, s.cursor.1);
                let (gx, gy) = (
                    o.0 + d.0 as f32 * crate::teleport::GHOST_GAIN,
                    o.1 + d.1 as f32 * crate::teleport::GHOST_GAIN,
                );
                // flick velocity (counts/ms), EMA'd so release reads recent intent
                {
                    let dt = flick_at.get().elapsed().as_millis().max(1) as f64;
                    if dt >= 8.0 {
                        let lg = flick_ghost.get();
                        let v = ((d.0 - lg.0) / dt, (d.1 - lg.1) / dt);
                        let f = flick.get();
                        flick.set((f.0 * 0.65 + v.0 * 0.35, f.1 * 0.65 + v.1 * 0.35));
                        flick_ghost.set(d);
                        flick_at.set(std::time::Instant::now());
                    }
                }
                let (l_down, r_down, r_up) = neuron::glyph::take_click_edges();
                if carrying.get() != 0 {
                    // ── CARRYING: the grabbed window rides the ghost; release drops it ──
                    if carry_paint.get().elapsed().as_millis() >= 45 {
                        overlay.begin(s.map_mode_carrying(
                            -1,
                            (-1, -1),
                            Some((carry_rect.get(), (gx, gy))),
                        ));
                        overlay.push(vec![(gx, gy)]);
                        carry_paint.set(std::time::Instant::now());
                    }
                    if r_up > 0 {
                        let hwnd = carrying.replace(0);
                        let v = flick.get();
                        let desk_bottom = s.project(s.vx, s.vy + s.vh).1;
                        let msg = if v.1 < -0.9 && v.1.abs() > 1.5 * v.0.abs() {
                            // UP-FLICK: auto-sort — it decides, and says why
                            crate::teleport::auto_sort(s, hwnd)
                        } else if let Some((ri, _)) = s.realm_hit(gx, gy) {
                            crate::teleport::banish(hwnd, s.realms[ri].guid)
                        } else if gy > desk_bottom + 8.0 {
                            // below the desk, on no card: the VOID — forge a new realm
                            crate::teleport::banish_new(hwnd)
                        } else {
                            crate::teleport::place(hwnd, s.target_of(d.0, d.1), carry_realm.get())
                        };
                        *verb_done.borrow_mut() = Some(msg);
                        verb_fired = true;
                    }
                } else {
                    verb_fired = aim_tick(
                        s,
                        d,
                        gx,
                        gy,
                        l_down,
                        r_down,
                        &hover,
                        &depth,
                        &aimed,
                        &aimed_rect,
                        &aimed_cell,
                        &aimed_warp,
                        &aimed_realm,
                        &sel_since,
                        &bloomed,
                        &carrying,
                        &carry_rect,
                        &carry_realm,
                        &verb_done,
                        overlay,
                        weak,
                    );
                }
            }
        }
        // ── THE DIAL: while the wheel is up, scrolling over a sound wedge TURNS it — the
        // widget you can twist. Each tick = ±2%; the wedge's label updates in place (pinned
        // re-begin keeps the wheel exactly where it is). ──
        if active.get() == 0 && matches!(weave_mode, crate::overlay::WeaveMode::Radial { .. }) {
            let ticks = neuron::glyph::take_wheel_ticks();
            if ticks != 0 {
                let aim = weave_aim.get();
                if (aim.0 * aim.0 + aim.1 * aim.1).sqrt() >= 12.0 {
                    let s = neuron::radial::sector_for(aim.0, aim.1, cast.sectors);
                    if let Some(a) = cast
                        .active_radial(crate::dispatch::hypershift_held())
                        .get(s)
                    {
                        use neuron::action::{Action, MediaKind};
                        let delta = ticks as f32 * 0.02;
                        let turned = match a {
                            Action::OutputGain { device, .. }
                            | Action::OutputMute { device, .. } => {
                                out_ctl(device.as_deref()).map(|c| c.nudge(delta)).is_some()
                            }
                            Action::MicGain { device, .. } | Action::MicMute { device, .. } => {
                                mic_ctl(device.as_deref()).map(|c| c.nudge(delta)).is_some()
                            }
                            // any media-transport wedge doubles as the master volume knob
                            Action::Media {
                                key:
                                    MediaKind::PlayPause
                                    | MediaKind::Stop
                                    | MediaKind::Next
                                    | MediaKind::Prev
                                    | MediaKind::VolumeUp
                                    | MediaKind::VolumeDown
                                    | MediaKind::VolumeMute,
                            } => out_ctl(None).map(|c| c.nudge(delta)).is_some(),
                            _ => false,
                        };
                        if turned {
                            overlay.begin(crate::overlay::WeaveMode::Radial {
                                sectors: cast.sectors.max(1) as u8,
                                widgets: radial_widgets(&cast),
                                fans: radial_fans(&cast),
                            });
                        }
                    }
                }
            }
        }
        let c_prompt = prompt_pending(shared);
        let c_editor = EDITOR_WEAVE.load(Ordering::SeqCst);
        let c_gen = crate::dispatch::reload_generation() != gen;
        let c_instr = INSTRUMENT_REQ.load(Ordering::SeqCst) != 0; // a try-button yanks the wait
                                                                  // knockback claimed or released its drum key mid-wait → re-arm with the right slots.
        let c_kb = crate::knockback::owned_ctl() != kb_ctl;
        // a "fire via the spine" rhythm just activated: end the capture AT ONCE (no drawing
        // session) so we inject Trigger::Cast right after — the rhythm IS the whole gesture.
        let c_fire = fire_taps.get().is_some();
        let fired = verb_fired || c_prompt || c_editor || c_gen || c_instr || c_kb || c_fire;
        if fired && std::env::var_os("NEURON_PROFILE").is_some() {
            eprintln!(
                "[CANCEL] verb={verb_fired} prompt={c_prompt} editor={c_editor} gen={c_gen} instr={c_instr} kb={c_kb} (gen now={} loaded={gen})",
                crate::dispatch::reload_generation()
            );
        }
        fired
    };
    // trace the capture lifecycle so a stall localizes from the flight log: if the last weave
    // trace is "arm" → stuck in the activation wait; "activated N" with no "done" → stuck in the
    // hold/drain (the cursor-lock freeze); "done N" → stuck after, in resolve/inject.
    crate::flight::trace("weave", "arm capture", cap_slots.len() as u64);
    let result = neuron::glyph::capture_slots_until(
        &cap_slots,
        &feel,
        600,
        &cancel,
        |id| {
            crate::flight::trace("weave", "activated", u64::from(id));
            // the instrument materializes the moment its rhythm lands
            active.set(id);
            match id {
                1 => {
                    // the wheel/click accumulators are global (they gather during ANY capture,
                    // teleport or not) — discard whatever piled up since the last aim so the
                    // depth dial starts at the surface and no phantom verb fires on entry.
                    let _ = neuron::glyph::take_wheel_ticks();
                    let _ = neuron::glyph::take_click_edges();
                    // the CLICK GUARD: L/R clicks become spectral verbs, so the apps under the
                    // pinned cursor must never receive them (the held trigger stays exempt).
                    let exempt = cap_slots
                        .iter()
                        .find(|s| s.id == 1)
                        .and_then(|s| s.ctl.vk_hint())
                        .unwrap_or(0);
                    crate::teleport::click_guard::arm(exempt);
                    let mut s = crate::teleport::snapshot();
                    // stamp the warpstones so the map can SHOW where your tethers are while you
                    // aim (geometry stays decoupled — the snapshot doesn't know the wm verbs).
                    s.tethers = crate::wm::tether_hwnds();
                    overlay.begin(s.map_mode());
                    snap.replace(Some(s));
                }
                2 => {} // whiteboard enters its session AFTER the activation release
                3 => {
                    // drop any wheel ticks the idle wait gathered so a stray scroll doesn't jump the
                    // output device on entry (scroll-to-switch-device is live in the step below).
                    let _ = neuron::glyph::take_wheel_ticks();
                    // THE DIAL: open the target at its current value, show the gauge with its
                    // target icon, device name, and mute state — you see WHAT you're turning.
                    let target =
                        crate::dialweave::target_from_code(DIAL_TARGET.load(Ordering::SeqCst));
                    dial.borrow_mut().begin(target);
                    let d = dial.borrow();
                    let (value, fill, glow) = d.reading();
                    overlay.begin(crate::overlay::WeaveMode::Dial {
                        value,
                        device: d.device.clone(),
                        fill,
                        glow,
                        mic: d.is_mic(),
                        muted: d.muted(),
                    });
                }
                6 => {
                    // THE CONTROL CENTER: glance the live system once (net/wifi/bluetooth, Win32 —
                    // taken here so the per-frame render never touches a blocking API) + the audio
                    // cache for the output readout, then paint the glance card.
                    let g = crate::control::glance();
                    overlay.begin(control_mode(&g));
                    control.replace(Some(g));
                }
                fire if fire >= 100 => {
                    // A "fire via the spine" rhythm (capture id = 100 + taps): record the tap count
                    // and begin NO overlay — the cancel predicate now reads `fire_taps` and ends the
                    // capture at once, so this rhythm draws nothing. The bound action fires from the
                    // injected `Trigger::Cast` after the capture returns.
                    fire_taps.set(Some((fire - 100) as u8));
                }
                _ => {
                    // the dial reads fresh ticks only — drop whatever the idle wait gathered
                    let _ = neuron::glyph::take_wheel_ticks();
                    overlay.begin(weave_mode.clone());
                }
            }
        },
        |pts| match active.get() {
            1 => {
                // the ghost: cursor's map spot + drag·gain (same math commit() warps with)
                if let Some(s) = snap.borrow().as_ref() {
                    let d = pts.last().map_or((0.0, 0.0), |c| (c.re, c.im));
                    ghost_d.set(d); // the scry dwell clock reads this
                    let o = s.project(s.cursor.0, s.cursor.1);
                    overlay.push(vec![(
                        o.0 + d.0 as f32 * crate::teleport::GHOST_GAIN,
                        o.1 + d.1 as f32 * crate::teleport::GHOST_GAIN,
                    )]);
                }
            }
            2 => {}
            3 => {
                // SCROLL while the OUTPUT dial is open = pick a DIFFERENT output device — the "output
                // device side" that was impossible to figure out is now right here on the dial. The
                // wheel cycles the system default render endpoint; the slide keeps controlling whatever
                // device is current. (The mic dial has no device cycle — one capture endpoint.)
                let ticks = neuron::glyph::take_wheel_ticks();
                if ticks != 0 && !dial.borrow().is_mic() {
                    let target =
                        crate::dialweave::target_from_code(DIAL_TARGET.load(Ordering::SeqCst));
                    for _ in 0..ticks.unsigned_abs().min(8) {
                        let _ = neuron::audio::flip_output(&[]); // cycle to the next connected output
                    }
                    let mut d = dial.borrow_mut();
                    d.begin(target); // re-resolve to the new default endpoint + its live level
                    let (value, fill, glow) = d.reading();
                    let (device, mic, muted) = (d.device.clone(), d.is_mic(), d.muted());
                    drop(d);
                    overlay.begin(crate::overlay::WeaveMode::Dial {
                        value,
                        device,
                        fill,
                        glow,
                        mic,
                        muted,
                    });
                }
                // THE DIAL: integrate the latest stroke point into the value, live, and refresh
                // the gauge (pinned re-begin keeps it in place).
                if let Some(c) = pts.last() {
                    let mut d = dial.borrow_mut();
                    let (value, fill, glow) = d.step((c.re, c.im));
                    let (device, mic, muted) = (d.device.clone(), d.is_mic(), d.muted());
                    drop(d);
                    overlay.begin(crate::overlay::WeaveMode::Dial {
                        value,
                        device,
                        fill,
                        glow,
                        mic,
                        muted,
                    });
                }
            }
            6 => {
                // THE CONTROL CENTER: just stream the aim so the overlay can light the quadrant the
                // flick is reaching for (west = output flip, east = bluetooth). No state changes
                // until release — a glance must never act mid-look.
                let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
                overlay.push(rel);
            }
            _ => {
                let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
                if let Some(c) = pts.last() {
                    weave_aim.set((c.re, c.im)); // the dial aims where the stroke is
                }
                overlay.push(rel);
                // ── LIVE NEXT-GLYPH PREDICTION (gesture mode): name + icon + shape-ghost of the
                // intent the stroke is becoming, firming up as it commits (phone-autocomplete) ──
                // THROTTLED: recompute only on the first eligible tick, then at most every ~60 ms
                // OR once the tip has moved a minimum straight-line distance from the last predict.
                // Between recomputes the prior hint stays live (a Push never clears it), so the
                // forecast shown is byte-identical to an unthrottled recompute at the moments it
                // DOES recompute — only redundant DTW passes are skipped.
                if matches!(cast.mode, neuron::cast::Mode::Gesture) && pts.len() >= 8 {
                    let tip = pts.last().map_or((0.0, 0.0), |c| (c.re, c.im));
                    let due = match last_predict_at.get() {
                        None => true, // the first eligible tick always predicts, exactly as before
                        Some(at) => {
                            let lt = last_predict_tip.get();
                            let moved = ((tip.0 - lt.0).powi(2) + (tip.1 - lt.1).powi(2)).sqrt();
                            at.elapsed().as_millis() >= 60 || moved >= 24.0
                        }
                    };
                    if due {
                        last_predict_at.set(Some(std::time::Instant::now()));
                        last_predict_tip.set(tip);
                        let word = neuron::glyph::analyze(pts, &vault.config);
                        if let Some((name, score, _ru)) = vault.predict(&word) {
                            let thr = vault.config.threshold.max(1e-6);
                            let conf = (1.0 - score / (thr * 1.6)).clamp(0.0, 1.0) as f32;
                            let locked = score <= thr;
                            let action = cast.gestures.get(&name).cloned().unwrap_or_default();
                            let mut view = wedge_view(&action);
                            // the gesture NAME is what it's becoming; keep the action's icon (the intent)
                            if matches!(view.glyph, crate::overlay::WedgeGlyph::Blank) {
                                view = crate::overlay::WedgeView {
                                    glyph: crate::overlay::WedgeGlyph::Mark,
                                    title: name.clone(),
                                    value: None,
                                    tone: crate::overlay::Tone::Plain,
                                    meter: None,
                                };
                            } else {
                                view.title = name.clone();
                            }
                            let ghost = vault.exemplar(&name).to_vec();
                            overlay.hint(Some(crate::overlay::GlyphHint {
                                view,
                                confidence: conf,
                                locked,
                                ghost,
                            }));
                        }
                    }
                }
            }
        },
    );

    // any path out of the capture retires an open scry portal — it lives only while aiming —
    // and the click guard (clicks must only ever be swallowed while a weave aims).
    if bloomed.get() != 0 {
        crate::teleport::scry_send(crate::teleport::ScryCmd::Hide);
    }
    crate::teleport::click_guard::disarm();
    crate::flight::trace(
        "weave",
        "capture done",
        result.as_ref().map_or(99, |(id, _)| u64::from(*id)),
    );

    // ── FIRE-VIA-THE-SPINE rhythm ── a rhythm bound to an arbitrary action landed: the capture
    // ended at once (no draw), so close any (none) overlay and INJECT `Trigger::Cast { taps }`.
    // The live Engine resolves it to the bound action and fires it through the one dispatch path
    // (layers / intents / SAFE / readout all apply) — never run the action here, or it would fork.
    if let Some(taps) = fire_taps.get() {
        overlay.end();
        crate::dispatch::inject_trigger(neuron::engine::Trigger::Cast { taps });
        return;
    }

    let Some((id, path)) = result else {
        // a SPECTRAL VERB commits by ending the capture — its story posts here, with the
        // arrival flare a teleport commit gets (the verb IS a commit, not a cancel).
        if let Some(msg) = verb_done.borrow_mut().take() {
            overlay.end();
            post_status(weak, msg);
            overlay.begin(crate::overlay::WeaveMode::Glyph { hint: None });
            overlay.recognized(true);
            overlay.end();
            return;
        }
        // cancelled (beacon/editor/reload) or ESC — never spin hot.
        if active.get() != u32::MAX {
            overlay.end();
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
        return;
    };

    match id {
        // ── TELEPORT: release = warp (+focus on a blob); a sub-deadzone release bails ──
        1 => {
            overlay.end();
            let (dx, dy) = neuron::radial::net_displacement(&path);
            let s = snap.into_inner().unwrap_or_default();
            if (dx * dx + dy * dy).sqrt() < crate::teleport::COMMIT_DEADZONE {
                return; // a peek at the map, not a journey — free
            }
            // the aim rides the commit: a realm pick steps through its card (focus switches the
            // desktop natively, cursor lands at the mapped point); a desk pick warps + focuses
            // whatever the dial selected.
            let msg = if aimed_realm.get() && aimed.get() != 0 {
                let (wx, wy) = aimed_warp.get();
                crate::teleport::commit_at(wx, wy, aimed.get())
            } else {
                let chosen = (aimed.get() != 0).then(|| aimed.get());
                crate::teleport::commit(&s, dx, dy, chosen)
            };
            post_status(weak, msg);
            // arrival flare: the overlay re-anchors at the WARPED cursor and flares there.
            overlay.begin(crate::overlay::WeaveMode::Glyph { hint: None });
            overlay.recognized(true);
            overlay.end();
        }
        // ── WHITEBOARD: the rhythm TOGGLES the session (open ⇄ close), which runs on its own
        // thread — the weave service goes right back to watching for everything else. ──
        2 => {
            overlay.end();
            crate::whiteboard::toggle(weak);
        }
        // ── DIAL: the value was applied live the whole slide; release just settles it. The status
        // NAMES the endpoint you turned (the output side was "hard to figure out") and, for the
        // output dial, points at how to SWITCH it — the device is no longer a mystery. ──
        3 => {
            let d = dial.borrow();
            let (label, ..) = d.reading();
            let dev = d.device.clone();
            let is_mic = d.is_mic();
            drop(d);
            overlay.end();
            let line = match (dev.is_empty(), is_mic) {
                (false, false) => format!("\u{2713} {dev} {label} \u{00b7} flip output: control center (\u{2190}) or an output-flip wedge"),
                (false, true) => format!("\u{2713} {dev} {label}"),
                _ => format!("\u{2713} {label}"),
            };
            post_status(weak, line);
        }
        // ── CONTROL CENTER: a flick commits the quick action of the card it reaches for —
        // north = the network settings seam, west = flip the output, east = the bluetooth seam.
        // Anything sub-deadzone is a pure glance and just closes. The quadrant rule matches the
        // overlay's card highlight, so what lit is what commits. ──
        6 => {
            overlay.end();
            let (dx, dy) = neuron::radial::net_displacement(&path);
            if (dx * dx + dy * dy).sqrt() < cast.deadzone {
                return; // a glance, not a verb — free
            }
            let msg = if dx.abs() > dy.abs() {
                if dx < 0.0 {
                    // WEST: flip to the next output device (empty = cycle every connected one).
                    neuron::audio::flip_output(&[])
                } else {
                    // EAST: toggle the bluetooth radio in place (item 18 — "toggle it real quick"),
                    // falling back to the settings seam if WinRT access is denied or there's no radio.
                    crate::control::bluetooth_toggle()
                }
            } else if dy < 0.0 {
                // NORTH: the network seam — "am i on the right thing? let me go change it".
                crate::control::open_network()
            } else {
                return; // a downward flick lands on no card — treat as a glance, close.
            };
            post_status(weak, msg);
        }
        // ── WEAVE: resolve + inject through the one Engine (unchanged) ──
        _ => {
            if path.is_empty() {
                overlay.end();
                std::thread::sleep(std::time::Duration::from_millis(40));
                return;
            }
            // ── THE SECOND TIER: if the stroke pushed OUT past the rim of a fannable wedge, it
            // picked a sub-option (a device), not the wedge's plain action. Same fan_pick rule
            // the overlay highlighted with, so what lit IS what commits. ──
            if let Some(msg) = resolve_fan(&cast, &path) {
                overlay.recognized(true);
                overlay.end();
                post_status(weak, msg);
                return;
            }
            if let Some(r) = cast.resolve(&path, &vault) {
                overlay.recognized(true);
                overlay.end();
                // Emit the STRUCTURED trigger and let the live Engine do the firing — never
                // run the action here, or layers/intents/SAFE/readout would fork.
                let trigger = match r.sector {
                    Some(s) => neuron::engine::Trigger::RadialSector {
                        menu: neuron::controls::CAST_MENU.into(),
                        sector: s as u8,
                    },
                    None => neuron::engine::Trigger::Gesture {
                        name: r.label.clone(),
                    },
                };
                crate::dispatch::inject_trigger(trigger);
                if r.assisted {
                    post_status(weak, format!("weave \u{2248} {} (spell assist)", r.label));
                }
            } else {
                let (dx, dy) = neuron::radial::net_displacement(&path);
                let net = (dx * dx + dy * dy).sqrt();
                if net < cast.deadzone {
                    // a release inside the centre dead-zone is the CLEAN CANCEL — no action,
                    // no nag, no fizzle flash. The wheel just closes (same cost as a peek).
                    overlay.end();
                } else {
                    // an unrecognized stroke that DID travel out fizzles softly — a miss costs
                    // nothing, but the readout says WHY so a real attempt is never a mystery.
                    overlay.recognized(false);
                    overlay.end();
                    post_status(
                        weak,
                        "weave fizzled \u{2014} not a clean flick and no glyph matched".into(),
                    );
                }
            }
        }
    }
}

/// Is any ask waiting to be presented? (The live weave stands down the moment one is.)
fn prompt_pending(shared: &Shared) -> bool {
    let (q, _) = &**shared;
    !q.lock().unwrap_or_else(std::sync::PoisonError::into_inner).queue.is_empty()
}

/// Format a system [`crate::control::Glance`] (+ the cached output reading) into the overlay's
/// CONTROL-CENTER card. Net reads as "ethernet"/"wifi"/"offline" with the interface name; the
/// output rides the audio cache (same source the wheel's widgets use — no inline COM here); the
/// bluetooth wedge says present/absent. All pre-formatted so the render never touches a live API.
#[cfg(windows)]
fn control_mode(g: &crate::control::Glance) -> crate::overlay::WeaveMode {
    use crate::control::Link;
    let link = match g.link {
        Link::Ethernet => 1u8,
        Link::WiFi => 2,
        Link::None => 0,
    };
    // the network value: the medium, or the interface name when we have a tidier one to show.
    let net = if g.link == Link::None {
        "offline".to_string()
    } else if !g.iface.is_empty() {
        g.iface.clone()
    } else {
        g.link.label().to_string()
    };
    // the output device + its level, from the off-thread audio cache (instant, never COM here).
    let ac = audio_cache::snap();
    let out = ac.out_name.clone().unwrap_or_else(|| "—".into());
    let out_fill = ac.out.map_or(0.0, |(lvl, _)| lvl);
    let bt = if g.bt_present {
        "on \u{00b7} flick"
    } else {
        "none"
    };
    crate::overlay::WeaveMode::Control {
        link,
        net,
        ssid: g.ssid.clone(),
        out,
        out_fill,
        bt: bt.to_string(),
        bt_on: g.bt_present,
    }
}

/// The wheel's wedge WIDGETS — each wedge is a live instrument readout (icon + value + tone +
/// meter), built data-driven from the bound action + live host state (Core Audio, the profile
/// cursor, live window counts, tether stones). Host-side reads only — instant, no device wakes.
#[cfg(windows)]
pub(crate) fn radial_widgets(cast: &neuron::cast::CastConfig) -> Vec<crate::overlay::WedgeView> {
    // show the HyperShift wedge set while a HyperShift layer is held (matches what the engine fires).
    let radial = cast.active_radial(crate::dispatch::hypershift_held());
    (0..cast.sectors)
        .map(|i| match radial.get(i) {
            Some(neuron::action::Action::Noop) | None => crate::overlay::WedgeView::blank(),
            Some(a) => wedge_view(a),
        })
        .collect()
}

/// THE SECOND-TIER COMMIT: if the stroke reached OUT past the rim of a fannable wedge, it picked
/// a sub-option — resolve which and execute it. Two fan today: an OUTPUT wedge fans its devices
/// (pick → set default); a SUMMON wedge with several windows open fans them (pick → summon that
/// exact one). The `fan_pick` rule + rim + candidate ORDER match the overlay's and `radial_fans`'
/// exactly, so the lit option IS the committed one. `None` = no fan pick (fall through to the
/// normal wedge resolve, e.g. output-flip's plain cycle / summon's launch-or-raise).
#[cfg(windows)]
fn resolve_fan(cast: &neuron::cast::CastConfig, path: &[neuron::glyph::C]) -> Option<String> {
    let (dx, dy) = neuron::radial::net_displacement(path);
    if (dx * dx + dy * dy).sqrt() < cast.deadzone {
        return None;
    }
    let (ix, iy) = neuron::radial::intent_vector(path);
    let wedge = neuron::radial::sector_for(ix, iy, cast.sectors);
    let tip = path.last().map(|c| (c.re as f32, c.im as f32))?;
    // a small closure so each fannable action resolves its pick identically — rim matches the
    // overlay's `(CX - 26).min(150)` for the standard canvas.
    let pick = |n: usize| {
        let p = crate::overlay::fan_pick(tip, wedge as i32, cast.sectors as u8, n, 150.0);
        (p >= 0 && (p as usize) < n).then_some(p as usize)
    };
    match cast
        .active_radial(crate::dispatch::hypershift_held())
        .get(wedge)
    {
        Some(neuron::action::Action::OutputFlip { devices }) => {
            let cands = neuron::audio::flip_candidates(devices);
            let e = &cands[pick(cands.len())?];
            Some(if neuron::audio::set_default(&e.id) {
                format!("output \u{2192} {}", e.name)
            } else {
                format!("output flip failed ({})", e.name)
            })
        }
        Some(neuron::action::Action::Summon { window, mode }) => {
            // only a real choice fans — one (or zero) windows falls through to the plain summon
            // (raise-the-one / launch). Same frontmost-first list `radial_fans` lit.
            let wins = crate::glance::matches(window);
            if wins.len() < 2 {
                return None;
            }
            let (hwnd, _) = wins[pick(wins.len())?];
            Some(crate::wm::summon_hwnd(hwnd, *mode))
        }
        _ => None,
    }
}

/// The SECOND-TIER fan options per wedge (index = sector): a wedge whose action FANS (today,
/// `OutputFlip`) lists its sub-options; everything else is empty. Generic — any future fannable
/// action plugs in here.
#[cfg(windows)]
fn radial_fans(cast: &neuron::cast::CastConfig) -> Vec<Vec<crate::overlay::FanView>> {
    // pure-Rust filtering over the CACHED render list (no Core-Audio COM on the weave thread —
    // `flip_candidates`/`default_render_id` here used to hang a cast permanently when an endpoint
    // stalled). Mirrors `flip_candidates`: empty devices = all connected; else substring match.
    let ac = audio_cache::snap();
    let radial = cast.active_radial(crate::dispatch::hypershift_held());
    (0..cast.sectors)
        .map(|i| match radial.get(i) {
            Some(neuron::action::Action::OutputFlip { devices }) => {
                let cands: Vec<&(String, String)> = if devices.is_empty() {
                    ac.renders.iter().collect()
                } else {
                    devices
                        .iter()
                        .filter_map(|n| {
                            let nl = n.to_lowercase();
                            ac.renders
                                .iter()
                                .find(|(name, _)| name.to_lowercase().contains(&nl))
                        })
                        .collect()
                };
                cands
                    .into_iter()
                    .map(|(name, id)| crate::overlay::FanView {
                        active: ac.default_id.as_deref() == Some(id.as_str()),
                        label: name.clone(),
                    })
                    .collect()
            }
            // SUMMON fans out when MORE THAN ONE window of the app is open — each option summons
            // that exact window. A single match (or none) doesn't fan: the plain wedge action
            // raises the one or LAUNCHES the app, so the second tier only appears when there's a
            // real choice to make. Frontmost-first (matching `resolve_fan`); the live one is marked.
            Some(neuron::action::Action::Summon { window, .. }) => {
                let wins = crate::glance::matches(window);
                if wins.len() < 2 {
                    Vec::new()
                } else {
                    let fg = foreground_hwnd();
                    wins.into_iter()
                        .map(|(h, title)| crate::overlay::FanView {
                            active: h == fg,
                            label: title,
                        })
                        .collect()
                }
            }
            _ => Vec::new(),
        })
        .collect()
}

/// The current foreground top-level window (its `GA_ROOT`), for marking the live summon-fan option.
#[cfg(windows)]
fn foreground_hwnd() -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};
    unsafe {
        let h = GetForegroundWindow();
        if h.is_null() {
            0
        } else {
            let r = GetAncestor(h, GA_ROOT);
            (if r.is_null() { h } else { r }) as isize
        }
    }
}

/// THE AUDIO CACHE — all Core-Audio COM for the wheel's widget readouts lives on its OWN thread,
/// NEVER the weave thread. A hung audio endpoint (a sleeping dongle, an endpoint mid-change) blocks
/// COM INDEFINITELY; the weave thread used to open a `VolumeCtl` inline on every cast to read
/// volume/mute for the widgets, so that hang would freeze spellweaving permanently (the bug the
/// flight recorder caught: weave silent forever, dispatch fine). Now the weave thread only reads
/// these cached values (instant, never COM); if COM stalls, only this refresher thread waits and
/// the cache serves last-known — the cast always proceeds.
pub(crate) mod audio_cache {
    use std::sync::Mutex;

    /// Trim an audio endpoint name to the identifying part: the hardware in parentheses if
    /// present ("Headset Earphone (Razer `BlackShark` V2)" → "Razer `BlackShark` V2"), else the
    /// name, capped.
    pub(crate) fn short_device(name: &str) -> String {
        let core = match (name.find('('), name.rfind(')')) {
            (Some(a), Some(b)) if b > a + 1 => name[a + 1..b].trim(),
            _ => name.trim(),
        };
        let core = if core.is_empty() { name.trim() } else { core };
        if core.chars().count() > 22 {
            core.chars().take(21).collect::<String>() + "\u{2026}"
        } else {
            core.to_string()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::short_device;

        #[test]
        fn keeps_the_hardware_in_parentheses() {
            assert_eq!(short_device("Headset Earphone (Razer BlackShark V2)"), "Razer BlackShark V2");
        }

        #[test]
        fn falls_back_to_the_whole_name_when_parentheses_are_empty_or_absent() {
            assert_eq!(short_device("Speakers ()"), "Speakers ()");
            assert_eq!(short_device("  Speakers  "), "Speakers");
        }

        #[test]
        fn caps_long_names_at_21_characters_plus_an_ellipsis() {
            let out = short_device("An Extremely Long Endpoint Name Indeed");
            assert_eq!(out.chars().count(), 22);
            assert!(out.ends_with('\u{2026}'));
        }
    }

    #[derive(Clone, Default)]
    pub struct Snap {
        /// (volume 0..1, muted) of the default RENDER endpoint, or None if unavailable.
        pub out: Option<(f32, bool)>,
        /// (volume 0..1, muted) of the default CAPTURE endpoint.
        pub mic: Option<(f32, bool)>,
        /// short friendly name of the default render device.
        pub out_name: Option<String>,
        /// every connected RENDER endpoint (friendly name, id) — the output-flip fan's source,
        /// enumerated off-thread (this is the COM call that used to hang the weave per-cast).
        pub renders: Vec<(String, String)>,
        /// the current default render endpoint id (marks the fan's "active" device).
        pub default_id: Option<String>,
    }

    static CACHE: Mutex<Snap> = Mutex::new(Snap {
        out: None,
        mic: None,
        out_name: None,
        renders: Vec::new(),
        default_id: None,
    });
    static START: std::sync::Once = std::sync::Once::new();

    pub fn snap() -> Snap {
        CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Spawn the refresher once (from `beacon::start`). Idempotent.
    pub fn ensure() {
        START.call_once(|| {
            crate::worker::spawn_detached("neuron-audio-cache", || loop {
                let s = read();
                *CACHE
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = s;
                std::thread::sleep(std::time::Duration::from_millis(400));
            });
        });
    }

    fn read() -> Snap {
        let default_id = neuron::audio::default_render_id();
        let renders: Vec<(String, String)> = neuron::audio::endpoints(neuron::audio::Flow::Render)
            .into_iter()
            .map(|e| (e.name, e.id))
            .collect();
        // the default render's reading + short name (resolve via the cached default id)
        let out_ep = neuron::audio::resolve_render(None);
        let out_name = out_ep
            .as_ref()
            .map(|e| short_device(&e.name));
        let out = out_ep
            .and_then(|e| neuron::audio::VolumeCtl::open(&e.id))
            .map(|c| (c.get_volume(), c.get_mute()));
        let mic = neuron::audio::resolve_capture(None)
            .and_then(|e| neuron::audio::VolumeCtl::open(&e.id))
            .map(|c| (c.get_volume(), c.get_mute()));
        Snap {
            out,
            mic,
            out_name,
            renders,
            default_id,
        }
    }
}

/// THE WIDGET MAPPER — one bound action → a live wedge readout. The icon archetype is inherent to
/// the action; the tone, value, and meter come from the CACHED audio snapshot (never inline COM on
/// the weave thread — see [`audio_cache`]) plus instant host reads (profile, window count, tether).
#[cfg(windows)]
fn wedge_view(a: &neuron::action::Action) -> crate::overlay::WedgeView {
    use crate::overlay::{Tone, WedgeGlyph, WedgeView};
    use neuron::action::{Action, DialTarget};
    let mk = |glyph, title: &str, value: Option<String>, tone, meter| WedgeView {
        glyph,
        title: title.to_string(),
        value,
        tone,
        meter,
    };
    // a cached volume reading → level% + mute colour + a fill meter (shared by output/mic/dial)
    let audio_view = |reading: Option<(f32, bool)>, glyph, title: &str| match reading {
        Some((lvl, muted)) => mk(
            glyph,
            title,
            Some(if muted {
                "muted".into()
            } else {
                format!("{}%", (lvl * 100.0).round() as i32)
            }),
            if muted { Tone::Off } else { Tone::Live },
            Some(lvl),
        ),
        None => mk(glyph, title, None, Tone::Inert, None),
    };
    let ac = audio_cache::snap();
    match a {
        Action::OutputGain { .. } | Action::OutputMute { .. } => {
            audio_view(ac.out, WedgeGlyph::Speaker, "output")
        }
        Action::MicGain { .. } | Action::MicGainSet { .. } | Action::MicMute { .. } => {
            audio_view(ac.mic, WedgeGlyph::Mic, "mic")
        }
        Action::Dial { target } => {
            let mic = matches!(target, DialTarget::MicVolume);
            let g = if mic {
                WedgeGlyph::Mic
            } else {
                WedgeGlyph::Speaker
            };
            audio_view(if mic { ac.mic } else { ac.out }, g, "dial")
        }
        Action::MomentaryMic { .. } => mk(
            WedgeGlyph::Mic,
            "push-talk",
            Some("hold".into()),
            Tone::Plain,
            None,
        ),
        Action::Sniper { dpi } => mk(
            WedgeGlyph::Target,
            "sniper",
            Some(format!("hold \u{2192} {dpi}")),
            Tone::Plain,
            None,
        ),
        // a VERB, not a readout: "flip output" + the device you're on NOW, so it reads as "press to
        // flip away from <current>" instead of mislabeling the current device as the action itself.
        Action::OutputFlip { .. } => mk(
            WedgeGlyph::Flip,
            "flip output",
            ac.out_name,
            Tone::Active,
            None,
        ),
        Action::Media { .. } => mk(WedgeGlyph::Media, &a.describe(), None, Tone::Plain, None),
        Action::ProfileSwitch { name } => {
            let active = neuron::profile::active() == *name;
            mk(
                WedgeGlyph::ProfileDot,
                name,
                None,
                if active { Tone::Active } else { Tone::Plain },
                None,
            )
        }
        Action::ProfileCycle { .. } => {
            mk(WedgeGlyph::ProfileDot, "profile", None, Tone::Plain, None)
        }
        Action::Glance { target } => {
            let n = crate::glance::count(target);
            mk(
                WedgeGlyph::WindowStack,
                target,
                Some(format!("\u{00d7}{n}")),
                if n > 0 { Tone::Live } else { Tone::Inert },
                None,
            )
        }
        // SUMMON reads how many of the app are open: 0 → it'll LAUNCH (dim "open"); 1 → raise the
        // one; ≥2 → the count says it'll FAN out (push past the rim to pick a specific window).
        Action::Summon { window, .. } => {
            let n = crate::glance::count(window);
            let value = match n {
                0 => Some("open".into()),
                _ => Some(format!("\u{00d7}{n}")),
            };
            mk(
                WedgeGlyph::Summon,
                window,
                value,
                if n > 0 { Tone::Live } else { Tone::Inert },
                None,
            )
        }
        Action::Banish { .. } => mk(WedgeGlyph::Banish, "banish", None, Tone::Plain, None),
        // PIN is a TOGGLE — the wedge says "unpin" (lit) on an already-pinned window and "pin"
        // otherwise, so it never claims "pin" while a press would unpin (the bug the tether had).
        Action::Pin { pick } => match crate::wm::pin_preview(*pick) {
            Some((title, true)) => mk(WedgeGlyph::Pin, "unpin", Some(title), Tone::Active, None),
            Some((title, false)) => mk(WedgeGlyph::Pin, "pin", Some(title), Tone::Plain, None),
            None => mk(WedgeGlyph::Pin, "pin", None, Tone::Inert, None),
        },
        Action::Kill { .. } => mk(WedgeGlyph::Banish, "kill", None, Tone::Off, None),
        // the echo wedge PREDICTS what it will replay — "echo → press [f]", never a black box.
        Action::Echo => mk(
            WedgeGlyph::Mark,
            "echo",
            Some(crate::dispatch::last_action_desc().unwrap_or_else(|| "nothing yet".into())),
            Tone::Plain,
            None,
        ),
        // the tether wedge PREDICTS its next press (mirrors wm::tether's decision) instead of just
        // naming the current stone — so it stops lying about set/unset. The slot reads on the title
        // so multiple tethers are legible; the RELEASE branch glows warn-red (it's destructive).
        Action::Tether { slot, mode } => {
            let tag = if slot.trim().is_empty() {
                String::new()
            } else {
                format!(" [{}]", slot.trim())
            };
            match mode {
                neuron::action::TetherMode::Mark => {
                    use crate::wm::TetherPreview;
                    match crate::wm::tether_preview(slot) {
                        TetherPreview::Set => mk(
                            WedgeGlyph::Anchor,
                            &format!("set here{tag}"),
                            None,
                            Tone::Inert,
                            None,
                        ),
                        TetherPreview::Warp { title } => mk(
                            WedgeGlyph::Anchor,
                            &format!("warp{tag}"),
                            Some(title),
                            Tone::Active,
                            None,
                        ),
                        TetherPreview::Release { title } => mk(
                            WedgeGlyph::Anchor,
                            &format!("release{tag}"),
                            Some(title),
                            Tone::Off,
                            None,
                        ),
                    }
                }
                // the WORMHOLE wedge predicts the swap target (anchor A ⇄ B). An incomplete portal
                // shows the endpoint a press will CAPTURE (the rebind), so it reads as "set B" until
                // both ends exist, then "swap → <the other side>".
                neuron::action::TetherMode::Wormhole => {
                    use crate::wm::WormholePreview;
                    match crate::wm::wormhole_preview(slot) {
                        WormholePreview::Set { side } => mk(
                            WedgeGlyph::Teleport,
                            &format!("set {side}{tag}"),
                            None,
                            Tone::Inert,
                            None,
                        ),
                        WormholePreview::Swap { title } => mk(
                            WedgeGlyph::Teleport,
                            &format!("swap{tag}"),
                            Some(title),
                            Tone::Active,
                            None,
                        ),
                    }
                }
            }
        }
        Action::GhostPaste { .. } => mk(WedgeGlyph::Ghost, "paste", None, Tone::Plain, None),
        Action::Pocket { slot, .. } => {
            let v = neuron::pocket::view_of(slot);
            if v.is_empty() {
                mk(WedgeGlyph::Ghost, "pocket", None, Tone::Inert, None)
            } else {
                mk(
                    WedgeGlyph::Ghost,
                    "pocket",
                    Some(v.summary),
                    Tone::Active,
                    None,
                )
            }
        }
        Action::Teleport => mk(WedgeGlyph::Teleport, "teleport", None, Tone::Plain, None),
        Action::Whiteboard => mk(WedgeGlyph::Whiteboard, "board", None, Tone::Plain, None),
        Action::Control => mk(WedgeGlyph::Network, "control", None, Tone::Plain, None),
        Action::Knockback => mk(WedgeGlyph::Knockback, "knock", None, Tone::Plain, None),
        Action::Curtain => mk(WedgeGlyph::Curtain, "curtain", None, Tone::Plain, None),
        Action::Lock => mk(WedgeGlyph::Screen, "lock", None, Tone::Off, None),
        Action::Sleep => mk(WedgeGlyph::Screen, "sleep", None, Tone::Off, None),
        Action::Run { .. } => mk(WedgeGlyph::Terminal, "run", None, Tone::Plain, None),
        Action::Script { .. } => mk(WedgeGlyph::Python, "macro", None, Tone::Plain, None),
        Action::DpiSet { .. } | Action::DpiCycle { .. } => {
            mk(WedgeGlyph::Target, "dpi", None, Tone::Plain, None)
        }
        Action::ScrollStageCycle { .. } => {
            mk(WedgeGlyph::Scroll, "scroll", None, Tone::Plain, None)
        }
        Action::Sequence { .. } => mk(WedgeGlyph::Key, "sequence", None, Tone::Plain, None),
        Action::Turbo { .. } => mk(WedgeGlyph::Key, "turbo", None, Tone::Plain, None),
        Action::Key { .. } | Action::MouseButton { .. } => {
            mk(WedgeGlyph::Key, &a.describe(), None, Tone::Plain, None)
        }
        // OBS — the wedge reads the LIVE broadcast truth from the host's mirror: Active while
        // the thing it toggles is running, Inert when OBS isn't connected (the wedge never
        // promises what the host can't deliver right now).
        Action::Obs { op, arg } => {
            use neuron::action::ObsOp;
            let s = crate::host::obs_snapshot();
            let (title, running) = match op {
                ObsOp::Stream => ("stream", s.streaming),
                ObsOp::Record => ("record", s.recording),
                ObsOp::RecordPause => ("rec pause", s.recording),
                ObsOp::Replay => ("clip", false),
                ObsOp::Scene => ("scene", false),
                ObsOp::Mute => ("obs mute", false),
            };
            let detail = if s.connected {
                match op {
                    ObsOp::Scene => Some(format!("\u{2192} {arg}")),
                    ObsOp::Stream if s.streaming => Some("LIVE".to_string()),
                    ObsOp::Record | ObsOp::RecordPause if s.recording => {
                        Some("recording".to_string())
                    }
                    ObsOp::Mute if !arg.is_empty() => Some(arg.clone()),
                    _ => None,
                }
            } else {
                Some("OBS not connected".to_string())
            };
            let tone = if !s.connected {
                Tone::Inert
            } else if running {
                Tone::Active
            } else {
                Tone::Plain
            };
            mk(WedgeGlyph::Media, title, detail, tone, None)
        }
        Action::Noop => WedgeView::blank(),
    }
}

#[cfg(windows)]
fn out_ctl(device: Option<&str>) -> Option<neuron::audio::VolumeCtl> {
    neuron::audio::resolve_render(device).and_then(|e| neuron::audio::VolumeCtl::open(&e.id))
}

#[cfg(windows)]
fn mic_ctl(device: Option<&str>) -> Option<neuron::audio::VolumeCtl> {
    neuron::audio::resolve_capture(device).and_then(|e| neuron::audio::VolumeCtl::open(&e.id))
}

/// One TELEPORT AIM tick (ghost not carrying): resolve what the ghost touches (realm blob first,
/// else the desk with the depth dial), keep the hot highlight + earned scry bloom + landing
/// marker honest — and read the SPECTRAL VERBS: right-click grabs the aimed window onto the
/// ghost; left-click summons it to the weave origin. Returns true when a verb committed the
/// weave (the capture should end).
#[allow(clippy::too_many_arguments)] // the live aim's working set, threaded explicitly
fn aim_tick(
    s: &crate::teleport::Snapshot,
    d: (f64, f64),
    gx: f32,
    gy: f32,
    l_down: i32,
    r_down: i32,
    hover: &std::cell::Cell<isize>,
    depth: &std::cell::Cell<i32>,
    aimed: &std::cell::Cell<isize>,
    aimed_rect: &std::cell::Cell<(i32, i32, i32, i32)>,
    aimed_cell: &std::cell::Cell<(i32, i32, i32, i32)>,
    aimed_warp: &std::cell::Cell<(i32, i32)>,
    aimed_realm: &std::cell::Cell<bool>,
    sel_since: &std::cell::Cell<std::time::Instant>,
    bloomed: &std::cell::Cell<isize>,
    carrying: &std::cell::Cell<isize>,
    carry_rect: &std::cell::Cell<(i32, i32, i32, i32)>,
    carry_realm: &std::cell::Cell<bool>,
    verb_done: &std::cell::RefCell<Option<String>>,
    overlay: &crate::overlay::SpellOverlay,
    weak: &slint::Weak<AppWindow>,
) -> bool {
    // what is the ghost touching? a REALM card window (another desktop's little world) takes
    // precedence; otherwise the desk, with the DEPTH DIAL descending the window stack under
    // the point (a fullscreen app is just position 0). the pick: (hwnd, screen rect, warp pt)
    type Pick = (isize, (i32, i32, i32, i32), (i32, i32));
    let mut hot = -1i32;
    let mut hot_realm = (-1i32, -1i32);
    // the depth-dial affordance the overlay draws: (index, count) for the column under the ghost.
    // stays (0,0) — hidden — unless the desk pick sits in a stack of overlapping windows.
    let mut depth_pips = (0i32, 0i32);
    let mut sel: Option<Pick> = None;
    if let Some((ri, wi_opt)) = s.realm_hit(gx, gy) {
        let wi = wi_opt.or_else(|| {
            // a card's empty area aims its frontmost window
            (0..s.realms[ri].windows.len()).min_by_key(|&i| s.realms[ri].windows[i].z)
        });
        if let Some(wi) = wi {
            let w = &s.realms[ri].windows[wi];
            let b = s.realm_blob(ri, wi);
            let fx = ((gx - b[0]) / (b[2] - b[0]).max(1e-3)).clamp(0.0, 1.0);
            let fy = ((gy - b[1]) / (b[3] - b[1]).max(1e-3)).clamp(0.0, 1.0);
            let warp = (
                w.rect.0 + (fx * (w.rect.2 - w.rect.0) as f32) as i32,
                w.rect.1 + (fy * (w.rect.3 - w.rect.1) as f32) as i32,
            );
            hot_realm = (ri as i32, wi as i32);
            aimed_realm.set(true);
            // a realm blob's cell is already canvas-relative — lift it onto the glass directly.
            let half = crate::teleport::CANVAS_HALF;
            let (ox, oy) = s.overlay_origin();
            aimed_cell.set((
                ox + (half + b[0]) as i32,
                oy + (half + b[1]) as i32,
                ox + (half + b[2]) as i32,
                oy + (half + b[3]) as i32,
            ));
            sel = Some((w.hwnd, w.rect, warp));
        }
    } else {
        let (tx, ty) = s.target_of(d.0, d.1);
        let stack = s.stack_at(tx, ty); // the overlapping column under the ghost, front→back
        let ticks = neuron::glyph::take_wheel_ticks();
        if ticks != 0 {
            depth.set((depth.get() - ticks).max(0));
        }
        // DON'T snap back to the surface just because the frontmost window under the point
        // changed — while descended into a lower layer, looking AROUND inside that same window
        // must stay on it. Only a genuine exit (the held window no longer covers the point)
        // resets the dial; otherwise re-seat the depth onto the held window's new stack index so
        // the same window stays picked even as neighbours slide over the cursor. A scroll this
        // same tick is the user deliberately descending PAST the held window — honour it (no
        // re-seat) so the dial still moves while the cursor sits inside one window.
        let held = aimed.get();
        let held_at = stack.iter().position(|w| w.hwnd == held);
        match held_at {
            Some(i) if depth.get() > 0 && held != 0 && ticks == 0 => {
                depth.set(i as i32); // re-seat onto the held window's new index, stay on it
                hover.set(stack.first().map_or(0, |w| w.hwnd));
            }
            _ => {
                let front = stack.first().map_or(0, |w| w.hwnd);
                // staying on the same held window (a scroll tick) must NOT count as a new column,
                // or the very scroll that descends would reset the dial it just turned.
                if front != hover.get() && !(held_at.is_some() && depth.get() > 0) {
                    hover.set(front);
                    depth.set(0); // a new column under the ghost resets the dial
                }
            }
        }
        if !stack.is_empty() {
            let i = (depth.get().max(0) as usize).min(stack.len() - 1);
            let w = stack[i];
            depth_pips = (i as i32, stack.len() as i32); // teach the descent when windows overlap
            hot = s.index_of(w.hwnd).map_or(-1, |i| i as i32);
            aimed_realm.set(false);
            aimed_cell.set(s.cell_screen(w.rect));
            sel = Some((w.hwnd, w.rect, (tx, ty)));
        }
    }
    let sel_h = sel.map_or(0, |(hw, ..)| hw);
    if let Some((_, rect, warp)) = sel {
        aimed_rect.set(rect);
        aimed_warp.set(warp);
    }
    // ── the SPECTRAL VERBS read the fresh aim ──
    if r_down > 0 && sel_h != 0 {
        // GRAB: the window becomes carried matter; the portal retires (the phantom suffices)
        carrying.set(sel_h);
        carry_rect.set(aimed_rect.get());
        carry_realm.set(aimed_realm.get());
        if bloomed.get() != 0 {
            crate::teleport::scry_send(crate::teleport::ScryCmd::Hide);
            bloomed.set(0);
        }
        post_status(
            weak,
            "spectral grab \u{00b7} drop: monitor / realm card / the void below (new realm) \u{00b7} flick \u{2191} = auto-sort".into(),
        );
        return false;
    }
    if l_down > 0 && sel_h != 0 {
        // SUMMON: the window comes to your hand — the verb commits the weave
        *verb_done.borrow_mut() = Some(crate::teleport::summon(sel_h, s.cursor, aimed_realm.get()));
        return true;
    }
    if sel_h != aimed.get() {
        aimed.set(sel_h);
        sel_since.set(std::time::Instant::now());
        // every window EARNS its bloom — switching aim (drag OR dial) retires the
        // old peek; the new one appears after its own dwell: the zoom-in feel.
        if bloomed.get() != 0 {
            crate::teleport::scry_send(crate::teleport::ScryCmd::Hide);
            bloomed.set(0);
        }
        // the hot blob follows; the PINNED re-begin keeps the map in place, and
        // re-pushing the ghost keeps the drag visually continuous. the depth pips ride along so
        // the "scroll descends here" affordance appears the moment overlapping windows are aimed.
        overlay.begin(s.map_mode_aim(hot, hot_realm, depth_pips));
        overlay.push(vec![(gx, gy)]);
    }
    if sel_h != 0
        && bloomed.get() != sel_h
        && sel_since.get().elapsed().as_millis() as u64 >= crate::teleport::SCRY_DWELL_MS
    {
        crate::teleport::scry_send(crate::teleport::ScryCmd::Show {
            src: sel_h,
            src_rect: aimed_rect.get(),
            near: aimed_cell.get(), // hug the minimap cell this peek mirrors
        });
        bloomed.set(sel_h);
    }
    if sel_h != 0 && bloomed.get() == sel_h {
        // the LANDING MARKER: the warp point, projected into the peek.
        let rect = aimed_rect.get();
        let (w, hgt) = (
            (rect.2 - rect.0).max(1) as f32,
            (rect.3 - rect.1).max(1) as f32,
        );
        let warp = aimed_warp.get();
        crate::teleport::scry_send(crate::teleport::ScryCmd::Aim {
            fx: (warp.0 - rect.0) as f32 / w,
            fy: (warp.1 - rect.1) as f32 / hgt,
        });
    }
    if sel_h == 0 && bloomed.get() != 0 {
        crate::teleport::scry_send(crate::teleport::ScryCmd::Hide);
        bloomed.set(0);
    }
    false
}

/// The screen-direction arrow (one of 8) pointing along `bearing` (`atan2(dy, dx)`, dy DOWN) — so the
/// answer hint shows which way to flick each option. Generic over N: yes/no reads "← yes · → no", a
/// 4-way reads "← a · ↓ b · → c · ↑ d", no special-casing.
fn dir_arrow(bearing: f64) -> &'static str {
    use std::f64::consts::PI;
    let sect = ((bearing.rem_euclid(2.0 * PI) / (PI / 4.0)).round() as usize) % 8;
    [
        "\u{2192}", "\u{2198}", "\u{2193}", "\u{2199}", "\u{2190}", "\u{2196}", "\u{2191}", "\u{2197}",
    ][sect]
}

/// The answer-hint line for the prompt CARD + the readout: each option with its flick arrow, then
/// pass. ONE source for both the announcement-card grammar and the UI readout hint, derived from the
/// same `wedge_bearing` the wheel and the verdict use — so the hint can never disagree with the geometry.
fn answer_hint(options: &[String]) -> String {
    let n = options.len().max(1);
    let mut parts: Vec<String> = options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("{} {}", dir_arrow(neuron::radial::wedge_bearing(i, n)), o))
        .collect();
    parts.push("pass".to_string());
    parts.join(" \u{00b7} ")
}

/// Present ONE prompt, SIGNAL-FIRST — a beacon never forces a wheel open:
///   1. the quiet stage: a one-line strip at the top of the cursor's monitor (the question + the
///      grammar), while the capture waits in the background for the user to engage;
///   2. the user holds the cast trigger → the ask wheel materializes AT the cursor (clamped
///      on-glass) and the flick commits: west = yes, east = no, vertical = pass;
///   3. a bail (release inside the deadzone, or ESC mid-weave) costs nothing — the strip returns
///      and the beacon keeps waiting. Only a committed flick, the macro's own timeout, or a
///      sidecar respawn resolves it. ESC never dismisses a beacon (games lean on ESC constantly).
fn present(
    weak: &slint::Weak<AppWindow>,
    shared: &Shared,
    overlay: &crate::overlay::SpellOverlay,
    p: &Prompt,
    stop: &Arc<AtomicBool>,
) {
    neuron::prof::bump(&neuron::prof::PRESENT);
    // the trigger belongs to the answer wheel for the duration — sibling listeners (the
    // whiteboard session) pause on this flag. Cleared on every way out via the guard below.
    BEACON_PRESENTING.store(true, Ordering::SeqCst);
    struct Presenting;
    impl Drop for Presenting {
        fn drop(&mut self) {
            BEACON_PRESENTING.store(false, Ordering::SeqCst);
        }
    }
    let _presenting = Presenting;
    mirror_active(weak, shared, Some(p));

    // Beacons are CLICK-THROUGH and additive — answering mid-game without alt-tabbing is the whole
    // point — so they ALWAYS present, even over an exclusive-fullscreen game. No defer, no toggle.
    if stop.load(Ordering::SeqCst) {
        mirror_active(weak, shared, None);
        return;
    }

    let cast = neuron::cast::CastConfig::load();
    let feel = neuron::feel::FeelConfig::load();
    let phrase = neuron::feel::Phrase::hold(); // answering is always the plain hold — predictable
    let deadzone = cast.deadzone;
    let trigger_name = cast.trigger.label();
    // built as its two REAL parts — how to engage, then the answer set — separated by a line break, so
    // the wrapper keeps the options together on their own row instead of splitting the list mid-way.
    // (Not a hardcoded row: it's the grammar's actual structure; the wrapper just honours the '\n'.)
    let grammar = format!("hold {trigger_name}\n{}", answer_hint(&p.options));

    // THE ONE PIPELINE — the ask ANNOUNCEMENT is now a PERSISTENT slot in the notifs stack (the same
    // surface as confirmations + macro notifies), NOT a separate Signal overlay. It lives until a
    // ClearAsk removes it. A drop guard clears it on EVERY exit (answered / passed / timeout / retire
    // / stop / superseded) so a card can never strand. Only the interactive WHEEL (drawn on engage,
    // below) stays on the beacon's own overlay — the one thing that stays separate.
    crate::notifs::post_ask(p.pid, &p.macro_id, &p.text, &grammar);
    struct ClearAsk(u64);
    impl Drop for ClearAsk {
        fn drop(&mut self) {
            crate::notifs::clear_ask(self.0);
        }
    }
    let _ask_card = ClearAsk(p.pid);

    let verdict: Option<usize> = loop {
        // stand down while the editor owns the trigger (recording a glyph / testing the wheel):
        // the ask keeps waiting — its strip stays up, its timeout still retires it via `stop`.
        while EDITOR_WEAVE.load(Ordering::SeqCst) && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
        // The capture's ACTIVATION TICK (one empty-progress call on the press) morphs the strip
        // into the wheel at the cursor — engage-to-open, never auto-open. The cancel PREDICATE
        // composes retire with editor stand-down, so an editor capture starting mid-wait yanks
        // this one instantly (and the bail path below re-arms it afterwards).
        let engaged = std::cell::Cell::new(false);
        let ov = &overlay;
        let label = p.text.clone();
        let detail = p.detail.clone();
        let options = p.options.clone();
        let cancel = || {
            // BEAT the weave heartbeat + renew the click-guard deadman while the ask blocks — the
            // same as live_weave's predicate. The presenter is busy HERE during an ask (live_weave
            // isn't running, so its per-pass beat is paused); without this, an ask held open past
            // SESSION_STALL_MS (1.5s) falsely flags "weave service SILENT — wedged or dead", and the
            // deadman could strand the mouse. capture_phrase_until ticks this every few ms whether
            // or not the cursor moves, so a still, un-answered ask now correctly reads as ALIVE.
            crate::flight::pulse(crate::flight::organ::WEAVE);
            crate::teleport::click_guard::renew();
            stop.load(Ordering::SeqCst) || EDITOR_WEAVE.load(Ordering::SeqCst)
        };
        let path = neuron::glyph::capture_phrase_until(
            cast.trigger,
            &phrase,
            &feel,
            600,
            &cancel,
            |pts| {
                if !engaged.replace(true) {
                    ov.begin(crate::overlay::WeaveMode::Ask {
                        label: label.clone(),
                        detail: detail.clone(),
                        options: options.clone(),
                    });
                }
                let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
                ov.push(rel);
            },
        );
        if stop.load(Ordering::SeqCst) {
            break None; // retired (timeout / respawn / superseded) — no answer is sent
        }
        let (dx, dy) = neuron::radial::net_displacement(&path);
        let committed = (dx * dx + dy * dy).sqrt() >= deadzone;
        match neuron::radial::pick_wedge(dx, dy, deadzone, p.options.len()) {
            Some(idx) => break Some(idx), // a committed flick into an option wedge
            None if committed => break None, // committed into a gap = a deliberate PASS → default
            None => {
                // a bail: ESC, a motionless release, an under-deadzone flick (held, thought, let go),
                // or an editor stand-down. The persistent ask CARD stays up — only fade the WHEEL if
                // it opened. (The sleep keeps a held ESC from spinning the capture loop hot.)
                if engaged.get() {
                    overlay.end();
                }
                std::thread::sleep(std::time::Duration::from_millis(120));
                continue;
            }
        }
    };

    match verdict {
        Some(idx) => {
            macro_host().answer(p.pid, Some(idx));
            overlay.recognized(true);
            let picked = p.options.get(idx).map_or("?", String::as_str);
            post_status(weak, format!("beacon \u{2192} {picked} \u{00b7} {}", p.text));
        }
        None => {
            // passed, or retired without a flick (timeout / supersede / link loss). Hand the macro
            // its default NOW so an abandoned prompt never pins the worker until the full sidecar-side
            // timeout — harmless if the sidecar already timed out or died (an expired pid is ignored).
            macro_host().answer(p.pid, None);
            overlay.recognized(false);
            post_status(weak, format!("beacon passed \u{00b7} {}", p.text));
        }
    }
    // the flare/fizzle fades on its own render thread — the overlay is persistent (the
    // presenter's), so nothing here waits on an animation before the next prompt or weave.
    overlay.end();
    mirror_active(weak, shared, None);
}

// The verdict rule now lives in ONE place for every prompt: `neuron::radial::pick_wedge` (the radial
// core, specialized for answering). The beacon verdict AND the overlay highlight both read it, so they
// can never disagree; N=2 reproduces the legacy WEST=yes / EAST=no / vertical=pass exactly (proven by
// `radial::tests::prompt_wheel_is_exactly_yes_no_pass_at_n2`).

/// Mirror the presented prompt (or its absence) into the UI: the header pill + the readout.
fn mirror_active(weak: &slint::Weak<AppWindow>, shared: &Shared, p: Option<&Prompt>) {
    let (active, text, hint, who) = match p {
        Some(p) => (
            true,
            format!("{} \u{00b7} {}", p.macro_id, p.text),
            "hold the cast trigger\n\u{2192} yes \u{00b7} \u{2190} no \u{00b7} \u{2195} pass".to_string(),
            p.macro_id.clone(),
        ),
        None => (false, String::new(), String::new(), String::new()),
    };
    let queued = {
        let (q, _) = &**shared;
        let g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.queue.len() + usize::from(g.current.is_some())
    };
    let w = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = w.upgrade() {
            let st = app.global::<State>();
            st.set_beacon_active(active);
            st.set_beacon_text(text.into());
            st.set_beacon_hint(hint.into());
            st.set_beacon_macro(who.into());
            st.set_beacon_count(queued as i32);
            if active {
                st.set_status_line(
                    "a macro is asking \u{2014} hold the cast trigger and flick".into(),
                );
                st.set_status_kind("info".into());
                st.set_status_stale(false);
            }
        }
    });
}

/// Mirror just the queue depth (router-side changes while a prompt is being presented).
fn mirror_count(weak: &slint::Weak<AppWindow>, shared: &Shared) {
    let queued = {
        let (q, _) = &**shared;
        let g = q.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        g.queue.len() + usize::from(g.current.is_some())
    };
    // a "test" beacon's gate lifts the moment nothing is queued/presenting — the mock fire it
    // raised has been answered or retired, so the SYSTEM panel can mock-fire again (see
    // [`TEST_BEACON_INFLIGHT`]). Notify events don't touch the queue, so this can't lift early.
    if queued == 0 {
        TEST_BEACON_INFLIGHT.store(false, Ordering::SeqCst);
    }
    let w = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = w.upgrade() {
            let st = app.global::<State>();
            st.set_beacon_count(queued as i32);
            if queued == 0 {
                st.set_beacon_active(false);
                st.set_beacon_text("".into());
                st.set_beacon_hint("".into());
                st.set_beacon_macro("".into());
            }
        }
    });
}

/// Post a one-line status to the live readout (shared by the weave service and sibling sessions
/// like knockback — one definition, one behaviour).
pub(crate) fn post_status(weak: &slint::Weak<AppWindow>, line: String) {
    let w = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = w.upgrade() {
            let st = app.global::<State>();
            st.set_status_line(line.into());
            st.set_status_kind("info".into());
            st.set_status_stale(false);
        }
    });
}

#[cfg(test)]
mod tests {
    use neuron::glyph::C;

    fn path(dx: f64, dy: f64) -> Vec<C> {
        vec![C::new(0.0, 0.0), C::new(dx / 2.0, dy / 2.0), C::new(dx, dy)]
    }

    /// The yes/no prompt (N=2) is the radial core's `pick_wedge`: WEST = yes(0), east = no(1),
    /// vertical/gap = pass (None), exactly as the overlay lights and the legacy rule did. YES on the
    /// LEFT. (The pure geometry is proven in `neuron::radial::tests`; here we cover the beacon's own
    /// path → net_displacement → pick_wedge usage.)
    fn pick2(dx: f64, dy: f64) -> Option<usize> {
        let (x, y) = neuron::radial::net_displacement(&path(dx, dy));
        neuron::radial::pick_wedge(x, y, 40.0, 2)
    }

    #[test]
    fn yes_no_prompt_maps_the_quadrants() {
        assert_eq!(pick2(-80.0, 0.0), Some(0), "west flick = yes");
        assert_eq!(pick2(80.0, 0.0), Some(1), "east flick = no");
        assert_eq!(pick2(-70.0, -50.0), Some(0), "WNW = yes (west-dominant)");
        assert_eq!(pick2(70.0, 50.0), Some(1), "ESE = no (east-dominant)");
        assert_eq!(pick2(0.0, -120.0), None, "up = pass");
        assert_eq!(pick2(10.0, 110.0), None, "down = pass");
        assert_eq!(pick2(50.0, -60.0), None, "vertical-dominant diagonal = pass");
    }

    /// Under the deadzone (or no motion) never picks a wedge. The beacon separates this BAIL (re-arm,
    /// the prompt stays up) from a committed gap-flick (a deliberate PASS) by the flick magnitude.
    #[test]
    fn under_deadzone_picks_nothing() {
        assert_eq!(pick2(10.0, 5.0), None, "tiny flick");
        assert_eq!(neuron::radial::pick_wedge(0.0, 0.0, 40.0, 2), None, "no motion");
    }

    // ── TRIGGER-OWNERSHIP (editor weave / pending prompt / whiteboard / knockback must
    // never double-consume the same trigger) ──────────────────────────────────────────────────
    //
    // `EDITOR_WEAVE` and `BEACON_PRESENTING` are process-globals also touched by the real
    // weave-presenter/whiteboard threads on the live app path (neither runs under `cargo test`,
    // but a future test could add one, and `cargo test` runs this file's tests concurrently by
    // default). `OWNERSHIP_LOCK` serializes every test below against every other; `OwnershipGuard`
    // snapshots both flags on entry and restores them on drop (even on panic), so no test can leak
    // a flipped flag into a sibling — the same discipline `testsupport::cwd_guard` uses for the cwd.

    static OWNERSHIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct OwnershipGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        editor: bool,
        presenting: bool,
    }

    impl OwnershipGuard {
        fn take() -> Self {
            let lock = OWNERSHIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let editor = super::EDITOR_WEAVE.load(std::sync::atomic::Ordering::SeqCst);
            let presenting = super::BEACON_PRESENTING.load(std::sync::atomic::Ordering::SeqCst);
            Self {
                _lock: lock,
                editor,
                presenting,
            }
        }
    }

    impl Drop for OwnershipGuard {
        fn drop(&mut self) {
            super::EDITOR_WEAVE.store(self.editor, std::sync::atomic::Ordering::SeqCst);
            super::BEACON_PRESENTING.store(self.presenting, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// With neither owner claimed, the live-weave path may capture.
    #[test]
    fn weave_may_capture_with_no_owner_claimed() {
        let _g = OwnershipGuard::take();
        super::EDITOR_WEAVE.store(false, std::sync::atomic::Ordering::SeqCst);
        super::BEACON_PRESENTING.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            super::weave_may_capture(),
            "no owner claimed the trigger — live weave should be free to arm"
        );
    }

    /// The GUI editor recording/testing a glyph stands the live-weave capture path down, and
    /// releasing its `EditorWeave` guard resumes it — the exact RAII contract `live_weave`'s entry
    /// gate (now `weave_may_capture`) relies on.
    #[test]
    fn editor_weave_stands_down_live_capture_then_resumes() {
        let _g = OwnershipGuard::take();
        super::BEACON_PRESENTING.store(false, std::sync::atomic::Ordering::SeqCst);
        super::EDITOR_WEAVE.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(super::weave_may_capture(), "baseline: nothing claimed yet");

        let guard = super::EditorWeave::engage();
        assert!(super::editor_weave_active(), "engage() must publish ownership");
        assert!(
            !super::weave_may_capture(),
            "the editor holding the trigger must preempt live weave"
        );

        drop(guard);
        assert!(
            !super::editor_weave_active(),
            "dropping the guard must release ownership"
        );
        assert!(
            super::weave_may_capture(),
            "ownership released — live weave capture must resume"
        );
    }

    /// A presenting beacon (the answer wheel/signal strip) stands the live-weave path down for the
    /// duration, matching `whiteboard`'s own pause check (`editor_weave_active() ||
    /// beacon_presenting()`) — and release resumes capture, same as the editor case.
    #[test]
    fn beacon_presenting_stands_down_live_capture_then_resumes() {
        let _g = OwnershipGuard::take();
        super::EDITOR_WEAVE.store(false, std::sync::atomic::Ordering::SeqCst);
        super::BEACON_PRESENTING.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(
            !super::weave_may_capture(),
            "a presenting beacon must preempt live weave"
        );

        super::BEACON_PRESENTING.store(false, std::sync::atomic::Ordering::SeqCst);
        assert!(
            super::weave_may_capture(),
            "the beacon finishing (present() returned, its RAII guard cleared) resumes capture"
        );
    }

    /// A prompt merely sitting in the queue (not yet presenting) is the OTHER half of "pending
    /// prompt preempts live weave" — enforced structurally in `start()`'s presenter loop (it pops
    /// the queue before ever calling `live_weave`), gated on exactly this predicate.
    #[test]
    fn pending_queued_prompt_is_detected_before_presenting() {
        let shared: super::Shared = std::sync::Arc::new((
            std::sync::Mutex::new(super::Q::default()),
            std::sync::Condvar::new(),
        ));
        assert!(
            !super::prompt_pending(&shared),
            "an empty queue never preempts the live weave slot"
        );
        {
            let (q, _) = &*shared;
            q.lock().unwrap().queue.push_back(super::Prompt {
                pid: 1,
                macro_id: "test-macro".into(),
                text: "deploy?".into(),
                options: vec!["yes".into(), "no".into()],
                detail: String::new(),
            });
        }
        assert!(
            super::prompt_pending(&shared),
            "a queued (not-yet-presented) ask must preempt the presenter loop's live_weave branch"
        );
    }

    /// A macro (or a bug) hammering `neuron.ask` in a loop with a UI attached that never answers
    /// must never grow the queue past [`super::MAX_PENDING_PROMPTS`] — the root fix for the
    /// unbounded `VecDeque`. Drives the real enqueue path (`try_enqueue`, the same function the
    /// router thread calls per `BeaconEvent::Ask`) directly, without spinning up the presenter or
    /// a real Macro Host session, so it stays deterministic and doesn't arm input or wait on
    /// wall-clock. Every overflowed ask must be refused promptly (never silently dropped — the
    /// caller learns immediately via `try_enqueue`'s `false` return, mirroring the real "answer it
    /// `None` right now" behaviour), and the queue must keep servicing at the cap: draining one
    /// frees exactly one slot for the next arrival.
    #[test]
    fn flood_of_asks_is_capped_and_overflow_is_refused_not_dropped() {
        let shared: super::Shared = std::sync::Arc::new((
            std::sync::Mutex::new(super::Q::default()),
            std::sync::Condvar::new(),
        ));
        let mk = |pid: u64| super::Prompt {
            pid,
            macro_id: "flood-macro".into(),
            text: format!("ask {pid}"),
            options: vec!["yes".into(), "no".into()],
            detail: String::new(),
        };

        let mut queued_count = 0usize;
        let mut refused_count = 0usize;
        for pid in 0..500u64 {
            if super::try_enqueue(&shared, mk(pid)) {
                queued_count += 1;
            } else {
                refused_count += 1;
            }
            // never exceeds the cap at ANY point during the flood, not just at the end.
            let len = shared.0.lock().unwrap().queue.len();
            assert!(
                len <= super::MAX_PENDING_PROMPTS,
                "queue grew past the cap at ask #{pid}: len={len}"
            );
        }
        assert_eq!(
            queued_count,
            super::MAX_PENDING_PROMPTS,
            "exactly a cap's worth got queued out of the flood"
        );
        assert_eq!(
            refused_count,
            500 - super::MAX_PENDING_PROMPTS,
            "every ask past the cap was refused promptly, not silently dropped"
        );

        // the queue still SERVICES at the cap: draining one (as the presenter does when it takes
        // the next prompt to show) frees exactly one slot for the next arrival.
        {
            let (q, _) = &*shared;
            q.lock().unwrap().queue.pop_front();
        }
        assert!(
            super::try_enqueue(&shared, mk(9999)),
            "a freed slot must admit the next ask"
        );
        let len = shared.0.lock().unwrap().queue.len();
        assert_eq!(
            len,
            super::MAX_PENDING_PROMPTS,
            "back at the cap after one drain + one enqueue"
        );
    }
}
