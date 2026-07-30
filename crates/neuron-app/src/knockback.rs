// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! KNOCKBACK — the rhythm familiar, as an in-app session you enter.
//!
//! A lightweight, non-invasive AFK duet: you drum on the cast trigger (and fidget the mouse)
//! while you wait — in queue, on a respawn timer, idle — and a spectral twin knocks your
//! rhythm back with one small flourish. The engine (onset → motif → the Engram-backed
//! [`neuron::twin::Familiar`]) lives in `neuron-core`, tested headless; this module is the
//! session: a state machine whose every transition is VISIBLE on the spell overlay's stage.
//!
//! ## The stage (how it reads, with zero tutorial)
//! One hard-light staff, anchored where your cursor was when you entered:
//!   * **Your strikes appear the instant you press** — phosphor constructs building left→right
//!     in real time; spacing IS your rhythm (time → pixels, fixed scale). Wiggles are smaller
//!     ghost constructs riding just under the beam.
//!   * **A seal-arc drains around your newest strike** — when it empties, your phrase commits.
//!     The phrase boundary, learnable without a word.
//!   * **The twin answers in your own tempo**: it clears the staff and rebuilds your rhythm in
//!     violet exactly where your constructs stood (the mirror made visible), then extends it —
//!     the flourish, larger and warmer. The reply ends in an open BLUEPRINT pulsing exactly
//!     where the next beat would fall.
//!   * **Your answering strike materializes the blueprint** — wireframe flashes into built
//!     light — and your next phrase begins. Harmony washes the stage hush-green; counterpoint
//!     washes warm violet; a storm runs amber; a stillpoint dilates the visual clocks.
//!   * **The weave strip** along the bottom crystallizes one shard per exchange — the score,
//!     the save file, and the art, always in view. The FAMILIAR itself breathes at the staff's
//!     head and dims when unfed.
//!
//! Runs on its own thread (like the whiteboard); while live it OWNS the cast trigger (the
//! weave service stands down and a low-level guard keeps taps from leaking into the game).
//! ESC leaves; the weave stays in the brain.

use crate::ui::AppWindow;
use std::sync::atomic::{AtomicBool, Ordering};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
/// The drum key the live session armed (0 = none). The weave service stands down ONLY this
/// key while the duet plays — every other slot keeps weaving.
static OWNED_VK: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// The drum key the live session owns (0 = no session). See [`owned_vk`].
pub fn owned_vk() -> i32 {
    let v = OWNED_VK.load(Ordering::SeqCst);
    // self-heal: a session that died/stalled without clearing OWNED_VK would dark its drum key
    // forever; if its organ has gone silent past the cap, the claim is stale → report unowned.
    if v != 0 && crate::flight::organ_stalled(crate::flight::organ::KNOCKBACK) {
        0
    } else {
        v
    }
}

/// Where the familiar's brain is persisted between sessions.
fn brain_path() -> std::path::PathBuf {
    neuron::runroot::run_root().join("runtime").join("twin.knbk")
}

/// Enter or leave the session.
pub fn toggle(weak: &slint::Weak<AppWindow>) {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        // already running → ask it to stop.
        STOP.store(true, Ordering::SeqCst);
        return;
    }
    STOP.store(false, Ordering::SeqCst);
    #[cfg(windows)]
    {
        let weak = weak.clone();
        // PANIC-PROOF / spawn-refusal teardown: the trigger guard, the owned-key claim and the
        // ACTIVE flag must all release on every exit path — a leaked claim here is "all
        // spellcasting is dead until restart", and a refused spawn must not dark the drum key
        // waiting on a session that will never run.
        crate::worker::spawn_guarded(
            "neuron-knockback",
            || {
                crate::teleport::click_guard::disarm();
                crate::flight::pulse_clear(crate::flight::organ::KNOCKBACK);
                OWNED_VK.store(0, Ordering::SeqCst);
                ACTIVE.store(false, Ordering::SeqCst);
            },
            move || {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| imp::run(&weak)));
            },
        );
    }
    #[cfg(not(windows))]
    {
        let _ = weak;
        ACTIVE.store(false, Ordering::SeqCst);
    }
}

// the live-readout poster is shared with the weave service — one definition lives in `beacon`.
// re-exported here so the session's `super::post_status` call sites keep resolving unchanged.
// (gated to match its only consumer, the `#[cfg(windows)]` session `imp` module.)
#[cfg(windows)]
use crate::beacon::post_status;

#[cfg(windows)]
mod imp {
    use super::*;
    use crate::overlay::{SpellOverlay, TwinBeat, WeaveMode};
    use neuron::rhythm::{DetectorConfig, MotifBuilder, MotifConfig, OnsetDetector, OnsetKind};
    use neuron::twin::{Emergent, Familiar, Judgment, Knockback, TwinConfig};
    use std::time::Instant;
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;

    const VK_ESCAPE: i32 = 0x1B;

    // ── stage geometry + palette (mirrors the overlay's staff) ──────────────
    /// Stage x runs −STAFF..+STAFF around the anchor. While YOU drum, time maps to x at a
    /// fixed live scale (instant, stable — your strikes never shift once placed); when the
    /// twin answers, it re-lays the rhythm at a FITTED scale so every phrase spans the staff
    /// like a line of sheet music — relative spacing still IS the rhythm.
    const STAFF: f32 = 185.0;
    /// Where phrases begin (inset so the familiar at the staff's head stands apart).
    const X0: f32 = -STAFF + 26.0;
    /// The live drumming scale (px per ms).
    const LIVE_SCALE: f32 = 0.16;
    /// A phrase commits after this much silence (the seal-arc drains over exactly this).
    const PHRASE_GAP_MS: u64 = 700;
    /// How long the blueprint-materialize flash runs.
    const FLARE_MS: u64 = 320;

    const PHOSPHOR: (f32, f32, f32) = (0.29, 0.95, 0.69);
    const VIOLET: (f32, f32, f32) = (0.72, 0.65, 1.0);
    const FLOURISH: (f32, f32, f32) = (0.67, 0.76, 0.93); // violet leaning hush — "the new part"
    const HUSH: (f32, f32, f32) = (0.60, 0.90, 0.84);
    const WARM_VIOLET: (f32, f32, f32) = (0.85, 0.55, 0.95);
    const AMBER: (f32, f32, f32) = (0.95, 0.70, 0.29);
    const GHOST_GREY: (f32, f32, f32) = (0.55, 0.52, 0.72);

    #[inline]
    fn key_down(vk: i32) -> bool {
        unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
    }

    #[inline]
    fn cursor() -> (f32, f32) {
        let mut p = POINT { x: 0, y: 0 };
        unsafe {
            GetCursorPos(&mut p);
        }
        (p.x as f32, p.y as f32)
    }

    fn load_or_new() -> Familiar {
        std::fs::read(super::brain_path())
            .ok()
            .and_then(|b| Familiar::load(&b))
            .unwrap_or_else(|| Familiar::new(TwinConfig::default()))
    }

    /// One strike standing on the staff during YOUR phrase.
    struct LiveBeat {
        t_rel: u64,
        energy: f32,
        ghost: bool,
    }

    /// What the session is doing — every variant has a distinct look on the stage.
    enum Stage {
        /// Idle or drumming: your constructs build; the seal drains after your last strike.
        Listening,
        /// The twin plays its reply in real tempo (interruptible — your strike yields it).
        /// `scale` is the fitted px/ms its phrase (+ the blueprint slot) spans the staff with.
        Reply {
            kb: Knockback,
            started_t: u64,
            haunting: bool,
            scale: f32,
        },
    }

    /// The fitted layout for a reply: px/ms so phrase + one blueprint slot fill the staff.
    fn fit_scale(kb: &Knockback) -> f32 {
        let span = (kb.duration_ms() + median_ioi(kb)).max(900) as f32;
        ((STAFF - X0 - 26.0) / span).clamp(0.04, 0.30)
    }

    fn median_ioi(kb: &Knockback) -> u64 {
        let mut iois: Vec<u64> = kb
            .onsets
            .windows(2)
            .map(|w| w[1].t_ms - w[0].t_ms)
            .collect();
        if iois.is_empty() {
            return 350;
        }
        iois.sort_unstable();
        iois[iois.len() / 2].max(120)
    }

    /// A mood wash on the stage: colour, strength, birth, lifetime.
    struct Wash {
        rgb: (f32, f32, f32),
        strength: f32,
        set_t: u64,
        ttl_ms: u64,
    }

    impl Wash {
        fn current(&self, t: u64) -> (f32, f32, f32, f32) {
            let age = t.saturating_sub(self.set_t);
            if age >= self.ttl_ms || self.strength <= 0.0 {
                return (0.0, 0.0, 0.0, 0.0);
            }
            let s = self.strength * (1.0 - age as f32 / self.ttl_ms as f32);
            (self.rgb.0, self.rgb.1, self.rgb.2, s)
        }
    }

    pub fn run(weak: &slint::Weak<AppWindow>) {
        let overlay = SpellOverlay::spawn();
        let mut fam = load_or_new();
        let cast = neuron::cast::CastConfig::load();
        let trigger = cast.trigger;
        let trigger_name = neuron::capture::vk_name(trigger);
        let mut detector = OnsetDetector::new(DetectorConfig::default());
        let mut builder = MotifBuilder::new(MotifConfig {
            phrase_gap_ms: PHRASE_GAP_MS,
            ..MotifConfig::default()
        });

        // OWN the drum key: claim it (the weave service stands down on exactly this key) and —
        // for MOUSE buttons — swallow its clicks so a tap keeps the beat without leaking into
        // the game underneath. A swallowed event never updates GetAsyncKeyState, so the drum is
        // read from the guard's own edge tracker; keyboard triggers aren't swallowed (that
        // would eat typing) and read via GetAsyncKeyState as normal.
        super::OWNED_VK.store(trigger, Ordering::SeqCst);
        let guarded = matches!(trigger, 0x01 | 0x02 | 0x04 | 0x05 | 0x06);
        if guarded {
            crate::teleport::click_guard::arm_button(trigger);
        }

        super::post_status(
            weak,
            format!(
                "knockback \u{2014} the familiar wakes. drum {trigger_name} \u{00b7} esc leaves"
            ),
        );

        // ── session state ──
        let start = Instant::now();
        let mut stage = Stage::Listening;
        let mut live: Vec<LiveBeat> = Vec::new();
        let mut phrase_start: Option<u64> = None;
        let mut last_onset_t: u64 = 0;
        let mut blueprint_x: Option<f32> = None;
        let mut bp_flare: Option<(f32, u64)> = None;
        let mut wash = Wash {
            rgb: (0.0, 0.0, 0.0),
            strength: 0.0,
            set_t: 0,
            ttl_ms: 1,
        };
        let mut dilate = 0.0f32;
        let mut shards: Vec<(f32, f32, f32)> = Vec::new();
        let mut presence_floor = 1.0f32; // sinks while unfed; activity restores it
        let mut had_onset = false; // any strike this session yet?
        let mut answer_hint = false; // show "answer it" after the session's first reply
        let mut first_reply_done = false;
        let mut haunted_idle = false;
        let mut last_activity_t: u64 = 0;
        let mut session_exchanges: u32 = 0;
        let mut down = false;
        let mut esc_was = false;
        let mut last_sent = Instant::now() - std::time::Duration::from_secs(1);

        crate::flight::trace("knockback", "session enter", trigger as u64);
        loop {
            if super::STOP.load(Ordering::SeqCst) {
                break;
            }
            crate::flight::pulse(crate::flight::organ::KNOCKBACK);
            let t = start.elapsed().as_millis() as u64;

            // ESC (edge) leaves the session.
            let esc = key_down(VK_ESCAPE);
            if esc && !esc_was {
                break;
            }
            esc_was = esc;

            // ── input → onsets (always live, even while the twin plays) ──
            let mut struck: Vec<neuron::rhythm::Onset> = Vec::new();

            // motion → ghost notes (fidgeting is playing; lighter than a tap)
            let (cx, cy) = cursor();
            if let Some(mut g) = detector.sample(t, cx, cy) {
                g.energy = (g.energy * 0.6).max(0.1);
                struck.push(g);
            }

            // the trigger drum: the DOWN edge is the hit — instant, like a real drum. Energy
            // comes from the playing itself (spacing: you can't hit hard fast), so deliberate
            // knocks build heavy constructs and rolls build light ones. The guarded mouse
            // button reads from the hook's edge tracker (swallowed events are invisible to
            // GetAsyncKeyState); both reads compose safely.
            let pressed =
                key_down(trigger) || (guarded && crate::teleport::click_guard::swallowed_down());
            if pressed && !down {
                down = true;
                let energy = match phrase_start {
                    Some(_) if builder.pending_len() > 0 => {
                        let ioi = t.saturating_sub(last_onset_t);
                        (ioi as f32 / 350.0).clamp(0.3, 0.95)
                    }
                    _ => 0.7, // a firm opening knock
                };
                struck.push(detector.strike(t, energy));
            } else if !pressed && down {
                down = false;
            }

            for o in struck {
                had_onset = true;
                haunted_idle = false;
                last_activity_t = t;
                presence_floor = 1.0;

                // a strike during the twin's reply YIELDS it — the mirror never pushes. The
                // reply completes instantly (its blueprint lands) and your answer begins.
                if let Stage::Reply {
                    kb,
                    haunting,
                    scale,
                    ..
                } = &stage
                {
                    if !haunting && kb.open {
                        blueprint_x = Some(blueprint_pos(kb, *scale));
                        if !first_reply_done {
                            first_reply_done = true;
                            answer_hint = true;
                        }
                    }
                    stage = Stage::Listening;
                }

                // Feed the builder FIRST, then mirror its state on the stage — the builder is
                // the one source of phrase truth. (push can close the previous phrase if the
                // gap raced poll_close; do_exchange clears the stage, so all live-beat
                // bookkeeping must come after.)
                let was_pending = builder.pending_len();
                if let Some(m) = builder.push(o) {
                    do_exchange(
                        &mut fam,
                        &m,
                        t,
                        &mut stage,
                        &mut wash,
                        &mut dilate,
                        &mut shards,
                        &mut session_exchanges,
                        &mut live,
                        &mut phrase_start,
                        weak,
                    );
                }
                if phrase_start.is_none() || was_pending == 0 {
                    // this strike opened a new phrase. If a blueprint stood waiting, this is
                    // the ANSWER: it materializes — wireframe into built light. (When the
                    // strike instead raced a seal, the blueprint is consumed silently.)
                    if let Some(bx) = blueprint_x.take() {
                        if was_pending == 0 {
                            bp_flare = Some((bx, t));
                        }
                        answer_hint = false;
                    }
                    live.clear();
                    phrase_start = Some(o.t_ms);
                }
                last_onset_t = o.t_ms;
                live.push(LiveBeat {
                    t_rel: o.t_ms.saturating_sub(phrase_start.unwrap_or(o.t_ms)),
                    energy: o.energy,
                    ghost: o.kind == OnsetKind::Ghost,
                });
            }

            // ── the phrase seals after silence → the twin answers ──
            if matches!(stage, Stage::Listening) {
                if let Some(m) = builder.poll_close(t) {
                    do_exchange(
                        &mut fam,
                        &m,
                        t,
                        &mut stage,
                        &mut wash,
                        &mut dilate,
                        &mut shards,
                        &mut session_exchanges,
                        &mut live,
                        &mut phrase_start,
                        weak,
                    );
                }
            }

            // ── the reply finishes on its own beat ──
            if let Stage::Reply {
                kb,
                started_t,
                haunting,
                scale,
            } = &stage
            {
                let elapsed = t.saturating_sub(*started_t);
                if elapsed > kb.duration_ms() + 420 {
                    if !haunting && kb.open {
                        blueprint_x = Some(blueprint_pos(kb, *scale));
                        if !first_reply_done {
                            first_reply_done = true;
                            answer_hint = true;
                        }
                    }
                    stage = Stage::Listening;
                }
            }

            // ── idleness: the glow dims, asking gently; deep idle summons a haunting ──
            let idle_ms = t.saturating_sub(last_activity_t);
            if idle_ms > 20_000 {
                presence_floor = (presence_floor * 0.9995).max(0.35);
            }
            if matches!(stage, Stage::Listening)
                && builder.pending_len() == 0
                && idle_ms > 40_000
                && !haunted_idle
            {
                if let Some((kb, age)) = fam.haunting() {
                    haunted_idle = true;
                    wash = Wash {
                        rgb: GHOST_GREY,
                        strength: 0.30,
                        set_t: t,
                        ttl_ms: kb.duration_ms() + 2000,
                    };
                    super::post_status(
                        weak,
                        format!("\u{2039} a haunting \u{2014} you, {age} exchanges ago"),
                    );
                    let scale = fit_scale(&kb);
                    stage = Stage::Reply {
                        kb,
                        started_t: t,
                        haunting: true,
                        scale,
                    };
                }
            }

            dilate *= 0.9992; // a stillpoint releases slowly, never snaps

            // ── compose + send the stage (throttled when nothing moves) ──
            let animating = !matches!(stage, Stage::Listening)
                || builder.pending_len() > 0
                || bp_flare.is_some_and(|(_, ft)| t.saturating_sub(ft) < FLARE_MS + 80)
                || wash.current(t).3 > 0.01
                || dilate > 0.02
                || t < 1500;
            let interval = if animating { 16 } else { 200 };
            if last_sent.elapsed().as_millis() as u64 >= interval {
                last_sent = Instant::now();
                let scene = compose(
                    t,
                    &stage,
                    &live,
                    phrase_start,
                    &builder,
                    blueprint_x,
                    bp_flare,
                    &wash,
                    dilate,
                    &shards,
                    presence_floor,
                    had_onset,
                    answer_hint,
                    &trigger_name,
                    &fam,
                );
                overlay.begin(scene);
                // an expired flare graduates into a plain first-beat construct (kind 0 via live)
                if bp_flare.is_some_and(|(_, ft)| t.saturating_sub(ft) >= FLARE_MS + 80) {
                    bp_flare = None;
                }
            }

            std::thread::sleep(std::time::Duration::from_millis(7));
        }

        // leaving: stop guarding the trigger, finish any pending phrase, persist the brain.
        crate::flight::trace("knockback", "session exit", session_exchanges as u64);
        crate::flight::pulse_clear(crate::flight::organ::KNOCKBACK);
        crate::teleport::click_guard::disarm();
        if let Some(m) = builder.flush() {
            fam.receive(&m);
        }
        let path = super::brain_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = neuron::salvage::atomic_write(&path, &fam.save()) {
            eprintln!("neuron: failed to save familiar state ({e})");
        }
        overlay.end();
        let g = fam.signals();
        super::post_status(
            weak,
            format!(
                "knockback closed \u{2014} {} exchanges woven \u{00b7} sync {:.2} \u{00b7} depth {}",
                g.exchanges, g.sync, g.palette_depth
            ),
        );
    }

    /// Seal a player phrase into the familiar and stage the reply. Every consequence is
    /// VISIBLE: the judgment washes the stage, a storm runs amber, a stillpoint dilates,
    /// and a new shard crystallizes onto the weave strip.
    #[allow(clippy::too_many_arguments)] // the session's working set, threaded explicitly
    fn do_exchange(
        fam: &mut Familiar,
        m: &neuron::rhythm::Motif,
        t: u64,
        stage: &mut Stage,
        wash: &mut Wash,
        dilate: &mut f32,
        shards: &mut Vec<(f32, f32, f32)>,
        session_exchanges: &mut u32,
        live: &mut Vec<LiveBeat>,
        phrase_start: &mut Option<u64>,
        weak: &slint::Weak<AppWindow>,
    ) {
        *session_exchanges += 1;
        let turn = fam.receive(m);

        // the verdict on YOUR answer, as colour — instant, wordless.
        match turn.judged {
            Some(Judgment::Harmony) => {
                *wash = Wash {
                    rgb: HUSH,
                    strength: 0.45,
                    set_t: t,
                    ttl_ms: 1400,
                };
            }
            Some(Judgment::Counterpoint) => {
                *wash = Wash {
                    rgb: WARM_VIOLET,
                    strength: 0.40,
                    set_t: t,
                    ttl_ms: 1400,
                };
            }
            None => {}
        }
        // emergent moments override the wash — they're bigger weather.
        let mut shard_hue = match turn.judged {
            Some(Judgment::Harmony) => 160.0,
            Some(Judgment::Counterpoint) => 285.0,
            None => 220.0,
        };
        match &turn.event {
            Some(Emergent::Storm { .. }) => {
                *wash = Wash {
                    rgb: AMBER,
                    strength: 0.55,
                    set_t: t,
                    ttl_ms: turn.knockback.duration_ms() + 1600,
                };
                shard_hue = 40.0;
            }
            Some(Emergent::Stillpoint { depth }) => {
                *dilate = (0.5 + depth * 0.4).clamp(0.0, 0.9);
                *wash = Wash {
                    rgb: HUSH,
                    strength: 0.40,
                    set_t: t,
                    ttl_ms: 2600,
                };
            }
            _ => {}
        }

        // the weave grows one crystal — earned depth rotates the whole strip's hue.
        let depth_rot = (turn.signals.palette_depth.saturating_sub(1)) as f32 * 8.0;
        let amp = m.onsets.iter().map(|o| o.energy).fold(0.0f32, f32::max);
        shards.push((shard_hue + depth_rot, amp, 1.0));
        let n = shards.len();
        for (i, s) in shards.iter_mut().enumerate() {
            s.2 = 0.55 + 0.45 * (i as f32 + 1.0) / n as f32;
        }
        if shards.len() > 64 {
            let drop = shards.len() - 64;
            shards.drain(..drop);
        }

        // the twin takes the staff: your phrase is its material now.
        live.clear();
        *phrase_start = None;

        let event_note = match &turn.event {
            Some(Emergent::Storm { phrase_len }) => {
                format!(" \u{00b7} \u{26c8} storm ({phrase_len} beats)")
            }
            Some(Emergent::Stillpoint { depth }) => {
                format!(" \u{00b7} \u{25e6} stillpoint {depth:.2}")
            }
            Some(Emergent::Haunting { age }) => format!(" \u{00b7} \u{2039} haunting ({age})"),
            None => String::new(),
        };
        let judged_note = match turn.judged {
            Some(Judgment::Harmony) => " \u{00b7} harmony",
            Some(Judgment::Counterpoint) => " \u{00b7} counterpoint",
            None => "",
        };
        let g = turn.signals;
        super::post_status(
            weak,
            format!(
                "knock {} \u{2192} twin answers {} (+{} flourish){judged_note}{event_note} \u{00b7} sync {:.2}",
                m.len(),
                turn.knockback.len(),
                turn.knockback.len().saturating_sub(turn.knockback.flourish_from),
                g.sync,
            ),
        );

        let scale = fit_scale(&turn.knockback);
        *stage = Stage::Reply {
            kb: turn.knockback,
            started_t: t,
            haunting: false,
            scale,
        };
    }

    /// Where the blueprint stands: one median beat past the reply's last construct — the gap
    /// sits exactly where the next beat WOULD fall, so answering on time is spatially natural.
    fn blueprint_pos(kb: &Knockback, scale: f32) -> f32 {
        (X0 + (kb.duration_ms() + median_ioi(kb)) as f32 * scale).min(STAFF + 10.0)
    }

    /// Build the full stage payload from session state. Pure assembly — every visual rule
    /// lives either here (what stands where) or in the overlay arm (how it's lit).
    #[allow(clippy::too_many_arguments)] // the stage's full working set, passed by value
    fn compose(
        t: u64,
        stage: &Stage,
        live: &[LiveBeat],
        phrase_start: Option<u64>,
        builder: &MotifBuilder,
        blueprint_x: Option<f32>,
        bp_flare: Option<(f32, u64)>,
        wash: &Wash,
        dilate: f32,
        shards: &[(f32, f32, f32)],
        presence_floor: f32,
        had_onset: bool,
        answer_hint: bool,
        trigger_name: &str,
        fam: &Familiar,
    ) -> WeaveMode {
        let mut beats: Vec<TwinBeat> = Vec::new();
        let mut seal = -1.0f32;

        match stage {
            Stage::Listening => {
                // YOUR phrase, building live at the fixed live scale — a strike never shifts
                // once placed; the spacing is your rhythm, raw.
                if let Some(p0) = phrase_start {
                    for b in live {
                        let x = (X0 + b.t_rel as f32 * LIVE_SCALE).min(STAFF);
                        let age = t.saturating_sub(p0 + b.t_rel);
                        let phase = (age as f32 / 2600.0).clamp(0.0, 0.7);
                        beats.push(TwinBeat {
                            x,
                            y: if b.ghost { 13.0 } else { 0.0 },
                            r: if b.ghost {
                                5.0 + 5.0 * b.energy
                            } else {
                                6.0 + 8.0 * b.energy
                            },
                            rgb: PHOSPHOR,
                            weight: b.energy,
                            phase,
                            kind: 0,
                        });
                    }
                    if builder.pending_len() > 0 {
                        let since =
                            t.saturating_sub(p0 + live.last().map(|b| b.t_rel).unwrap_or(0));
                        seal = 1.0 - (since as f32 / PHRASE_GAP_MS as f32).clamp(0.0, 1.0);
                    }
                }
                // the open blueprint, waiting patiently.
                if let Some(bx) = blueprint_x {
                    beats.push(TwinBeat {
                        x: bx,
                        y: 0.0,
                        r: 12.0,
                        rgb: PHOSPHOR,
                        weight: 1.0,
                        phase: 0.0,
                        kind: 3,
                    });
                }
            }
            Stage::Reply {
                kb,
                started_t,
                haunting,
                scale,
            } => {
                // the twin plays in YOUR tempo, re-laying the rhythm at the fitted scale —
                // the phrase written out across the staff like a line of sheet music. The
                // shape is yours exactly; the flourish extends it.
                let elapsed = t.saturating_sub(*started_t);
                for (i, o) in kb.onsets.iter().enumerate() {
                    if o.t_ms > elapsed {
                        break;
                    }
                    let x = (X0 + o.t_ms as f32 * scale).min(STAFF);
                    let phase = ((elapsed - o.t_ms) as f32 / 1400.0).clamp(0.0, 0.6);
                    let flourish = i >= kb.flourish_from;
                    let (rgb, kind, rbase) = if *haunting {
                        (GHOST_GREY, 1u8, 5.0)
                    } else if flourish {
                        (FLOURISH, 2u8, 9.0)
                    } else {
                        (VIOLET, 1u8, 6.0)
                    };
                    beats.push(TwinBeat {
                        x,
                        y: 0.0,
                        r: rbase + 8.0 * o.energy,
                        rgb,
                        weight: if *haunting { o.energy * 0.6 } else { o.energy },
                        phase,
                        kind,
                    });
                }
            }
        }

        // the materialize flare rides on top of everything (the answered blueprint).
        if let Some((bx, ft)) = bp_flare {
            let age = t.saturating_sub(ft);
            if age < FLARE_MS {
                beats.push(TwinBeat {
                    x: bx,
                    y: 0.0,
                    r: 12.0,
                    rgb: PHOSPHOR,
                    weight: 1.0,
                    phase: age as f32 / FLARE_MS as f32,
                    kind: 4,
                });
            }
        }

        // presence: materialize over the first 600 ms, then breathe at the floor.
        let presence = ((t as f32 / 600.0).min(1.0)) * presence_floor;

        // the only words the game ever says, and only until they're not needed.
        let hint = if !had_onset {
            format!("drum {trigger_name} \u{00b7} wiggle plays too \u{00b7} esc leaves")
        } else if answer_hint && blueprint_x.is_some() {
            "answer it \u{2014} finish the line".to_string()
        } else {
            String::new()
        };

        let _ = fam; // (brain stats ride the status line; the stage stays wordless)
        WeaveMode::Twin {
            beats,
            seal,
            wash: wash.current(t),
            weave: shards.to_vec(),
            hint,
            presence,
            dilate,
        }
    }
}
