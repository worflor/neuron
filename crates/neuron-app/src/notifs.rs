//! The NOTIFICATION ENGINE — the consumer end of [`neuron::confirm`].
//!
//! A dedicated thread drains the confirmation channel and, for each one, does TWO independent things
//! (the two-axis model): it plays an audio cue (if the audio axis is on) and shows a card (if the
//! placement axis isn't "off"). Either, both, or neither — "sound-only" is just audio-on with
//! placement off. Rapid repeats COALESCE the card in place AND walk the tone up the pentatonic, so
//! cycling DPI is one card that updates and a rising run of tones — never a stack of five, never a
//! jackhammer.
//!
//! The card draws in its own value-hierarchy card grammar (`draw_notify_card`) on its own overlay; the sound is the
//! `neuron::tone` synth driven through [`crate::sound`]. Both are gated by the live prefs on every
//! confirmation, so toggles take effect immediately.

use crate::overlay::{DigestView, NotifySlot, WedgeGlyph};
use crate::prefs::{Prefs, StackMode};
use crate::sound::SoundEngine;
use neuron::confirm::{Confirmation, Kind, Shape};
use neuron::tone::{pentatonic_hz, Timbre};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// THE ONE NOTIFICATION INPUT — everything informational rides this single channel into the engine,
/// so confirmations, macro notifies, AND the beacon's ask ANNOUNCEMENT are all SLOTS in the one
/// stack. Only the interactive answer WHEEL (the radial you flick) stays a separate surface.
///
///   * `Confirm` — a state-change confirmation (or a macro notify, which is just a `Kind::Macro`
///     `Confirmation`). `confirm::set_sink` stays pure: a tiny forwarder thread (see [`run`]'s setup
///     in `main`) maps each `Confirmation` it drains into a `Note::Confirm` here, so the core never
///     learns the engine's enum.
///   * `Ask` — the beacon raises a macro's `neuron.ask(...)` as a PERSISTENT card (no auto-expire);
///     it lives until a matching `ClearAsk` removes it. Identified by `ask:{pid}` so a re-post
///     coalesces and `clear_ask` can find it.
///   * `ClearAsk` — the beacon answered / passed / timed out / retired the ask: drop its card.
pub enum Note {
    Confirm(Confirmation),
    Ask {
        pid: u64,
        macro_id: String,
        question: String,
        grammar: String,
    },
    ClearAsk {
        pid: u64,
    },
}

/// The notification engine's own sender, stored so a macro's `neuron.notify()` ([`post_macro`]) and
/// the beacon's ask ([`post_ask`]/[`clear_ask`]) can post onto the SAME surface confirmations use.
/// Set once at startup (in `main`, beside `confirm::set_sink` + the forwarder).
static NOTE_SINK: OnceLock<Sender<Note>> = OnceLock::new();

/// Hand the engine's `Note` sender to the macro-notify + ask paths. Called once at startup.
pub fn set_note_sink(tx: Sender<Note>) {
    let _ = NOTE_SINK.set(tx);
}

/// Post the beacon's ask ANNOUNCEMENT as a PERSISTENT card in the one notification stack — the same
/// pipeline as confirmations + macro notifies (everything informational is one system; only the
/// answer wheel stays separate). The card never auto-expires; it lives until [`clear_ask`] with the
/// same `pid` removes it, and a re-post coalesces in place (ident `ask:{pid}`). The beacon calls this
/// once when an ask is presented; the WHEEL (the thing you flick) is still drawn on the beacon's own
/// overlay — only the waiting ANNOUNCEMENT moved here.
pub fn post_ask(pid: u64, macro_id: &str, question: &str, grammar: &str) {
    let Some(tx) = NOTE_SINK.get() else {
        return;
    };
    let _ = tx.send(Note::Ask {
        pid,
        macro_id: macro_id.to_string(),
        question: question.to_string(),
        grammar: grammar.to_string(),
    });
}

/// Remove the persistent ask card for `pid` from the stack — the beacon calls this on EVERY exit
/// (answered / passed / timeout / retire / stop / superseded), so an ask card never strands.
pub fn clear_ask(pid: u64) {
    if let Some(tx) = NOTE_SINK.get() {
        let _ = tx.send(Note::ClearAsk { pid });
    }
}

/// A monotonic sequence so each macro notify gets a UNIQUE ident — see [`post_macro`].
static MACRO_SEQ: AtomicU64 = AtomicU64::new(0);

/// Show a macro's `neuron.notify()` line as a card on the notification surface — the same crafted
/// over-game card confirmations use, not only the in-app status line. Built HERE (the app layer), NOT
/// via a `confirm::` constructor, so the core's "a notice needs a real change — no post-arbitrary-text"
/// model stays pure: the macro layer (the user's own code) is the one legitimate free-text source.
///
/// Each call gets a UNIQUE ident (`macro:{id}:{seq}`), so an LLM macro that emits several distinct
/// `neuron.notify()` lines becomes several SEPARATE stacked cards — none collapsed into one (the user
/// must never miss a line). The macro API itself is unchanged: no new argument, no new host protocol —
/// the uniqueness is invented entirely on this side. It rides the same engine, so the live Kind::Macro
/// notif toggle gates it exactly like every other card.
pub fn post_macro(id: &str, text: &str) {
    let Some(tx) = NOTE_SINK.get() else {
        return;
    };
    let title = if id.is_empty() || id == "?" {
        "Macro".to_string()
    } else {
        id.to_string()
    };
    let seq = MACRO_SEQ.fetch_add(1, Ordering::Relaxed);
    let _ = tx.send(Note::Confirm(Confirmation {
        kind: Kind::Macro,
        shape: Shape::Discrete {
            label: text.to_string(),
        },
        title,
        ident: format!("macro:{id}:{seq}"),
        prev: None,
    }));
}

// ── THE STACK ENGINE — timings + animation params (all hand-rolled, ticked off a ~60fps loop) ──
// SNAPPY is the bar: fast, decisive, smoothly interpolated. Entry slides+fades in (~170ms ease-out);
// exit fades + collapses the gap (~150ms); a coalesce BUMP pulses the card (~120ms); the column
// REFLOW eases each card to its target slot (~150ms crisp spring, overshoot-free). The HOLD (the
// at-rest dwell before a card leaves) is the legacy 1200ms, reset on every coalesce.

/// How long a card holds at full strength before it begins leaving (reset by a coalesce).
const HOLD: Duration = Duration::from_millis(1200);
/// Entry animation length (slide-in + fade-up).
const ENTER_MS: f32 = 170.0;
/// Exit animation length (fade + height-collapse so the column closes the gap).
const LEAVE_MS: f32 = 150.0;
/// Coalesce-bump length (the scale pulse + value flash on the card that updated).
const BUMP_MS: f32 = 120.0;
/// Reflow spring constant — fraction of the remaining gap closed per ~16ms tick (≈150ms settle).
/// Crisp, not bouncy: a simple exponential approach, no overshoot.
const REFLOW_K: f32 = 0.32;
/// The engine's tick period while any note is alive (≈60fps).
const TICK: Duration = Duration::from_millis(16);
/// Stack mode keeps at most this many LIVE cards; the rest become the "+N more" tail.
const STACK_CAP: usize = 4;

/// The musical key: A4 + 3 semitones = C — a calm mid root for the pentatonic cues.
const ROOT: i32 = 3;
/// Cues within this window of the previous one form one rising phrase (the burst continuity); a
/// longer gap resets to each event's own leitmotif anchor.
const CUE_WINDOW: Duration = Duration::from_millis(1600);

/// A card's life stage in the stack. `Enter` and `Leave` run an animation; `Hold` is the at-rest
/// dwell that ends at the slot's `deadline` (reset on every coalesce).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Enter,
    Hold,
    Leave,
}

/// One live notification the engine is holding. Identity-coalesced by `ident`: a confirmation kind
/// ("dpi"/"profile"/…) coalesces in place (update + reset deadline + bump); a macro notify never
/// coalesces (its ident is unique per call), so an LLM's distinct lines all survive as separate
/// cards. Carries the card CONTENT plus the per-slot animation the engine eases each tick.
struct Slot {
    ident: String,
    kind: Kind,
    glyph: WedgeGlyph,
    // the card content (the value-hierarchy parts the overlay rasterizes).
    title: String,
    value: String,
    prev: String,
    dir: i8,
    fill: f32,
    prev_fill: f32,
    panel: bool,
    /// THE ASK card: a beacon's `neuron.ask(...)` announcement. PERSISTENT — it has no `deadline`
    /// countdown (never auto-leaves), lives until a `ClearAsk{pid}` pushes it to Leave, and the
    /// overlay draws it in the ask layout (the `?`/Ask glyph + the prominent question + the grammar
    /// row) instead of the value-hierarchy. Every other slot is `false`.
    is_ask: bool,
    /// bumped whenever the content above changes, so the overlay re-rasterizes only on a real change.
    rev: u64,
    phase: Phase,
    phase_since: Instant,
    /// when this slot leaves Hold for Leave (reset on each coalesce). IGNORED for an ask slot — an
    /// ask is persistent and only leaves on an explicit `ClearAsk`.
    deadline: Instant,
    /// the eased CURRENT column offset from the corner edge (animates toward `target_y`).
    cur_y: f32,
    /// a fresh-spawn flag so the first tick seeds `cur_y` from the entry slide rather than 0.
    seeded: bool,
    /// the coalesce bump's start (a brief scale pulse + value flash); far past = no bump.
    bump_since: Instant,
}

/// Drain confirmations forever, holding an identity-coalesced STACK of live notes and presenting them
/// in the user's chosen mode (Stack column / Latest swap / Digest summary). Returns only when the
/// channel's sender is dropped (app shutdown). The engine owns ALL timing: it ticks a ~60fps loop
/// while any note is alive, advancing phases + easing each slot's animation, and pushes the whole
/// slot list to the overlay each tick (which renders it). The AUDIO cue fires per note exactly as
/// before (a coalesce still plays its tone).
pub fn run(rx: Receiver<Note>) {
    // The card overlay is created lazily; the audio engine opens its output stream once up front
    // (silent until struck). Either may be absent — no audio device, or notifications off — and both
    // paths then degrade to no-ops.
    let mut overlay: Option<crate::overlay::SpellOverlay> = None;
    let mut sound = SoundEngine::new(crate::prefs::notif_volume());
    let mut music = Music::new();
    let mut slots: Vec<Slot> = Vec::new();
    let mut pushed_empty = false; // whether we've already told the overlay the stack went empty

    loop {
        // No live cards → BLOCK until the next note (zero idle cost). With cards alive, wait only a
        // tick so the animation advances even between notes.
        let recv = if slots.is_empty() {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        } else {
            rx.recv_timeout(TICK)
        };
        match recv {
            Ok(n) => handle_note(&mut slots, &mut sound, &mut music, n),
            Err(RecvTimeoutError::Timeout) => {} // just tick
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // drain any other notes already queued (a burst) before we tick/animate, so a flood
        // coalesces/pushes in one pass rather than one card per tick.
        while let Ok(n) = rx.try_recv() {
            handle_note(&mut slots, &mut sound, &mut music, n);
        }

        // advance phases + animation, then present.
        let p = Prefs::load_cached();
        let mode = p.notif_stack_mode();
        tick_phases(&mut slots, mode);
        if slots.is_empty() {
            if !pushed_empty {
                if let Some(ov) = overlay.as_ref() {
                    ov.end(); // the stack emptied — fade the overlay out.
                }
                pushed_empty = true;
            }
            continue;
        }
        pushed_empty = false;
        // THE REAL ANCHOR — pass the user's ACTUAL (nx, ny) fractions to the overlay (NOT a snapped
        // corner). The overlay centres the column block on `nx` (clamped on-screen) and grows it from
        // `ny` (top half → down, bottom half → up), so a centre placement renders centred and a corner
        // placement hugs that corner. An ask-only stack still needs a place even when placement is
        // "off" for confirmations (the ask is never gated), so default to top-centre then.
        let (nx, ny) = p.notif_place_xy().unwrap_or((0.5, 0.0));
        let ov = overlay.get_or_insert_with(crate::overlay::SpellOverlay::spawn);
        reflow(&mut slots, mode); // ease the column toward its targets, then snapshot to the overlay
        let (slot_views, digest, tail) = present(&slots, mode);
        ov.stack(slot_views, mode_code(mode), (nx, ny), digest, tail);
    }
}

/// Apply one [`Note`] to the live set: gate + audio + ingest a confirmation, post/coalesce a
/// persistent ask card, or push an ask to Leave on a clear. (The audio cue fires per confirmation
/// exactly as before; asks are silent here — the beacon owns the ask's own feedback.)
fn handle_note(slots: &mut Vec<Slot>, sound: &mut Option<SoundEngine>, music: &mut Music, n: Note) {
    match n {
        Note::Confirm(c) => {
            if let Some(act) = decide(&c) {
                if act.audio {
                    music.play(&c, sound, act.vol, act.tid);
                }
                if let Some(card) = act.card {
                    ingest(slots, card);
                }
            }
        }
        Note::Ask {
            pid,
            macro_id,
            question,
            grammar,
        } => ingest_ask(slots, pid, &macro_id, &question, &grammar),
        Note::ClearAsk { pid } => clear_ask_slot(slots, pid),
    }
}

/// Coalesce or push a new card into the live set. Identity by `ident`: a HIT updates the card data in
/// place (bump `rev` when content actually changed), resets the deadline, and marks a coalesce bump;
/// a MISS pushes a fresh `Enter` slot. (Confirmations coalesce by their kind ident; macro notifies
/// have unique idents so they never coalesce — each is its own card.)
fn ingest(slots: &mut Vec<Slot>, card: CardData) {
    let now = Instant::now();
    if let Some(s) = slots.iter_mut().find(|s| s.ident == card.ident) {
        // COALESCE — update content; bump the rev only if something visible changed.
        let changed = s.title != card.title
            || s.value != card.value
            || s.prev != card.prev
            || s.dir != card.dir
            || (s.fill - card.fill).abs() > 1e-4
            || (s.prev_fill - card.prev_fill).abs() > 1e-4
            || s.glyph != card.glyph;
        if changed {
            s.rev = s.rev.wrapping_add(1);
        }
        s.title = card.title;
        s.value = card.value;
        s.prev = card.prev;
        s.dir = card.dir;
        s.fill = card.fill;
        s.prev_fill = card.prev_fill;
        s.glyph = card.glyph;
        s.kind = card.kind;
        s.panel = card.panel;
        s.deadline = now + HOLD;
        s.bump_since = now; // a fresh changed-pulse
        if s.phase == Phase::Leave {
            // it was leaving but got refreshed — bring it back to Hold.
            s.phase = Phase::Hold;
            s.phase_since = now;
        }
        return;
    }
    // MISS — a new card, entering. Insert at the FRONT (newest nearest the corner).
    slots.insert(
        0,
        Slot {
            ident: card.ident,
            kind: card.kind,
            glyph: card.glyph,
            title: card.title,
            value: card.value,
            prev: card.prev,
            dir: card.dir,
            fill: card.fill,
            prev_fill: card.prev_fill,
            panel: card.panel,
            is_ask: false,
            rev: 0,
            phase: Phase::Enter,
            phase_since: now,
            deadline: now + HOLD,
            cur_y: 0.0,
            seeded: false,
            bump_since: now - Duration::from_secs(10),
        },
    );
}

/// Post (or coalesce) the beacon's PERSISTENT ask card. Identity `ask:{pid}`: a re-post of the same
/// pid updates the question/grammar in place; otherwise a fresh ask slot enters at the FRONT. The
/// ask never gets a live `deadline` — it stays until [`clear_ask_slot`] removes it. The grammar line
/// rides `prev` (the overlay's ask layout draws it as the grammar row); the question is the `value`.
/// Always shows: the ask is never gated by the confirmation prefs (a macro may converse even with
/// cards "off"), and it stands regardless of placement (placement only chooses WHERE).
fn ingest_ask(slots: &mut Vec<Slot>, pid: u64, _macro_id: &str, question: &str, grammar: &str) {
    let now = Instant::now();
    let ident = format!("ask:{pid}");
    if let Some(s) = slots.iter_mut().find(|s| s.ident == ident) {
        // COALESCE — refresh the prompt text; bump only on a real change.
        let changed = s.value != question || s.prev != grammar;
        if changed {
            s.rev = s.rev.wrapping_add(1);
            s.bump_since = now;
        }
        s.value = question.to_string();
        s.prev = grammar.to_string();
        if s.phase == Phase::Leave {
            // it was leaving (a stale ClearAsk) but got re-posted — bring it back.
            s.phase = Phase::Hold;
            s.phase_since = now;
        }
        return;
    }
    slots.insert(
        0,
        Slot {
            ident,
            kind: Kind::Macro, // the ask is a macro-origin note (drives its leitmotif/grouping)
            glyph: WedgeGlyph::Ask,
            title: "Macro asks".to_string(),
            value: question.to_string(),
            prev: grammar.to_string(),
            dir: 0,
            fill: -1.0,
            prev_fill: -1.0,
            panel: Prefs::load_cached().notif_panel,
            is_ask: true,
            rev: 0,
            phase: Phase::Enter,
            phase_since: now,
            // far-future: an ask is PERSISTENT — `tick_phases` never expires it (it ignores the
            // deadline for ask slots), but a sane value keeps the field honest.
            deadline: now + Duration::from_secs(86_400),
            cur_y: 0.0,
            seeded: false,
            bump_since: now - Duration::from_secs(10),
        },
    );
}

/// Push the persistent ask card for `pid` to Leave so it fades + the column closes its gap. Called on
/// EVERY beacon exit (answered / passed / timeout / retire / stop / superseded) — idempotent: a clear
/// for a pid with no card (already gone) is a silent no-op.
fn clear_ask_slot(slots: &mut [Slot], pid: u64) {
    let ident = format!("ask:{pid}");
    if let Some(s) = slots.iter_mut().find(|s| s.ident == ident && s.phase != Phase::Leave) {
        s.phase = Phase::Leave;
        s.phase_since = Instant::now();
    }
}

/// Advance every slot's phase off the clock + drop fully-left slots. Enter→Hold after the entry
/// anim; Hold→Leave at the deadline; a finished Leave is removed. In LATEST mode a new card SWAPS the
/// old: every non-newest live card is pushed to Leave at once, so only the newest stays (the others
/// crossfade out) — that's what makes Latest a swap, not a column. Pure timing — the per-frame eased
/// values (alpha/scale/y) are computed in [`present`].
fn tick_phases(slots: &mut Vec<Slot>, mode: StackMode) {
    let now = Instant::now();
    // LATEST: keep only the newest live card; demote the rest to Leave (the crossfade-out half of a
    // swap). `slots[0]` is the newest (we insert at the front).
    if mode == StackMode::Latest {
        let mut seen_live = false;
        for s in slots.iter_mut() {
            // the PERSISTENT ask is never swapped away by an incoming confirmation — it's awaiting
            // an answer, so it stands in Latest mode too (only its OWN ClearAsk retires it).
            if s.phase == Phase::Leave || s.is_ask {
                continue;
            }
            if !seen_live {
                seen_live = true; // the newest survives
            } else if s.phase != Phase::Leave {
                s.phase = Phase::Leave;
                s.phase_since = now;
            }
        }
    }
    for s in slots.iter_mut() {
        match s.phase {
            Phase::Enter => {
                if now.duration_since(s.phase_since).as_millis() as f32 >= ENTER_MS {
                    s.phase = Phase::Hold;
                    s.phase_since = now;
                }
            }
            // an ask is PERSISTENT — it dwells in Hold forever (no deadline countdown); only an
            // explicit ClearAsk moves it to Leave. Every other slot leaves at its deadline.
            Phase::Hold => {
                if !s.is_ask && now >= s.deadline {
                    s.phase = Phase::Leave;
                    s.phase_since = now;
                }
            }
            Phase::Leave => {}
        }
    }
    // drop cards whose Leave anim has finished.
    slots.retain(|s| {
        !(s.phase == Phase::Leave
            && now.duration_since(s.phase_since).as_millis() as f32 >= LEAVE_MS)
    });
}

/// Cubic ease-out (decisive landing, no overshoot).
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t).powi(3)
}

/// REFLOW — set each slot's eased column offset `cur_y` toward its target slot (a crisp exponential
/// spring, ≈150ms settle, no overshoot), so when a card enters/leaves its neighbours GLIDE into the
/// new gap instead of jumping. Mode-aware: Latest/Digest stack everything at y=0 (one spot); Stack
/// lays a column. A fresh slot is SEEDED at its target (+ an entry slide handled by the overlay's
/// alpha) so it doesn't sweep up from y=0 on its first frame. Must run before [`present`].
fn reflow(slots: &mut [Slot], mode: StackMode) {
    let now = Instant::now();
    // compute targets (independent of cur_y), then ease each toward its target.
    let mut targets: Vec<f32> = Vec::with_capacity(slots.len());
    // an ASK in flight forces a COLUMN layout in every mode (the persistent prompt must read clearly
    // — a Latest swap or a Digest collapse would hide the question the user has to answer). With no
    // ask present, Latest/Digest keep their exact prior single-spot layout (zero regression).
    let column = mode == StackMode::Stack || slots.iter().any(|s| s.is_ask);
    if column {
        let mut y = 0.0f32;
        let mut shown_live = 0usize;
        for s in slots.iter() {
            let leaving = s.phase == Phase::Leave;
            let capped = !leaving && shown_live >= STACK_CAP;
            if !leaving && !capped {
                shown_live += 1;
            }
            targets.push(y);
            if !capped {
                let collapse = if leaving {
                    ease_out(now.duration_since(s.phase_since).as_millis() as f32 / LEAVE_MS)
                } else {
                    0.0
                };
                y += card_height(s) * (1.0 - collapse) + STACK_CARD_GAP_PX;
            }
        }
    } else {
        for _ in slots.iter() {
            targets.push(0.0);
        }
    }
    for (s, &t) in slots.iter_mut().zip(targets.iter()) {
        if !s.seeded {
            // SEED a fresh card a touch BEFORE its slot (nearer the corner edge) so the spring pulls
            // it inward — a crisp slide-IN that rides alongside the overlay's fade-up. The offset is
            // toward the corner (smaller y), so the column gently nudges as the newcomer arrives.
            s.cur_y = (t - ENTRY_SLIDE_PX).max(0.0 - ENTRY_SLIDE_PX);
            s.seeded = true;
        } else {
            s.cur_y += (t - s.cur_y) * REFLOW_K; // crisp exponential approach
        }
    }
}

/// How far (px) a freshly-spawned card slides in from (toward the corner) as it springs to its slot.
const ENTRY_SLIDE_PX: f32 = 22.0;

/// Build the overlay payload from the live slots for the chosen presentation mode. Eases each slot's
/// animation (entry slide+fade, exit fade+collapse, coalesce bump) into a `NotifySlot` at the
/// already-reflowed `cur_y`. Call [`reflow`] FIRST. Returns `(slots, digest, tail)`:
///   * Stack: up to `STACK_CAP` cards in a reflowing column, the rest reported as `tail`.
///   * Latest: the newest live card + any still-leaving card (so a swap crossfades over one spot).
///   * Digest: a summary `DigestView` (count + distinct glyph row + latest line) when >1 is live,
///     else the lone card as a plain slot.
fn present(slots: &[Slot], mode: StackMode) -> (Vec<NotifySlot>, DigestView, u32) {
    let now = Instant::now();

    // helper: the coalesce bump (scale pulse + value flash) for a slot.
    let bump_of = |s: &Slot| -> (f32, f32) {
        // a quick 1.0→1.06→1.0 pulse + a value flash that fades over BUMP_MS.
        let age = now.duration_since(s.bump_since).as_millis() as f32;
        if age >= BUMP_MS {
            (1.0, 0.0)
        } else {
            let t = age / BUMP_MS;
            let pulse = (t * std::f32::consts::PI).sin(); // 0→1→0
            (1.0 + 0.06 * pulse, (1.0 - t) * pulse.max(0.0))
        }
    };
    let alpha_of = |s: &Slot| -> f32 {
        match s.phase {
            Phase::Enter => {
                let t = now.duration_since(s.phase_since).as_millis() as f32 / ENTER_MS;
                ease_out(t)
            }
            Phase::Hold => 1.0,
            Phase::Leave => {
                let t = now.duration_since(s.phase_since).as_millis() as f32 / LEAVE_MS;
                1.0 - ease_out(t)
            }
        }
    };
    let collapse_of = |s: &Slot| -> f32 {
        if s.phase == Phase::Leave {
            let t = now.duration_since(s.phase_since).as_millis() as f32 / LEAVE_MS;
            ease_out(t)
        } else {
            0.0
        }
    };

    // a column closure: the Stack-mode layout (used directly for Stack, AND for Latest/Digest while
    // a persistent ask is in flight — the prompt must read as a clear card, never swapped/collapsed).
    let column = |slots: &[Slot]| -> (Vec<NotifySlot>, DigestView, u32) {
        let mut out = Vec::new();
        let mut shown_live = 0usize;
        let mut overflow = 0u32;
        for s in slots {
            let leaving = s.phase == Phase::Leave;
            if !leaving {
                if shown_live >= STACK_CAP {
                    overflow += 1;
                    continue; // beyond the cap → counted into the tail, not drawn
                }
                shown_live += 1;
            }
            out.push(view(s, alpha_of(s), bump_of(s), collapse_of(s)));
        }
        (out, DigestView::none(), overflow)
    };

    // an ask in flight overrides Latest/Digest with the column (see `reflow`'s matching rule).
    if mode != StackMode::Stack && slots.iter().any(|s| s.is_ask) {
        return column(slots);
    }

    match mode {
        StackMode::Latest => {
            // ONE card: the newest live (non-leaving) card, PLUS any still-leaving card so the swap
            // crossfades (incoming fades in over the same slot the outgoing fades out of).
            let mut out = Vec::new();
            // the incoming (front-most non-leaving) card sits at y=0 and fades in.
            if let Some(s) = slots.iter().find(|s| s.phase != Phase::Leave) {
                out.push(view(s, alpha_of(s), bump_of(s), collapse_of(s)));
            }
            // the outgoing (a leaving card) crossfades out over the same spot.
            for s in slots.iter().filter(|s| s.phase == Phase::Leave) {
                out.push(view(s, alpha_of(s), bump_of(s), collapse_of(s)));
            }
            (out, DigestView::none(), 0)
        }
        StackMode::Digest => {
            let live: Vec<&Slot> = slots.iter().filter(|s| s.phase != Phase::Leave).collect();
            if live.len() <= 1 {
                // a single live note (plus any still-leaving) → a plain single card.
                let mut out = Vec::new();
                for s in slots {
                    out.push(view(s, alpha_of(s), bump_of(s), collapse_of(s)));
                }
                (out, DigestView::none(), 0)
            } else {
                // a SUMMARY: count + the distinct source glyphs (deduped by kind, first-seen order)
                // + the latest note's line. The newest is `live[0]` (we insert at the front).
                let mut glyphs: Vec<WedgeGlyph> = Vec::new();
                let mut seen: Vec<Kind> = Vec::new();
                for s in &live {
                    if !seen.contains(&s.kind) {
                        seen.push(s.kind);
                        glyphs.push(s.glyph);
                    }
                }
                let latest = live[0];
                // the summary's own reveal eases over ENTER_MS from the newest card's spawn.
                let reveal = ease_out(
                    now.duration_since(latest.phase_since).as_millis() as f32 / ENTER_MS,
                );
                let digest = DigestView {
                    count: live.len() as u32,
                    glyphs,
                    title: latest.title.clone(),
                    value: latest.value.clone(),
                    alpha: 1.0,
                    reveal: reveal.max(0.4),
                };
                (Vec::new(), digest, 0)
            }
        }
        StackMode::Stack => {
            // a reflowing COLUMN: newest nearest the corner (front of the vec). Visible up to
            // STACK_CAP live cards; the rest become the +N tail. A leaving card still occupies its
            // (collapsing) slot so neighbours glide into the gap (its `cur_y` was eased by reflow).
            column(slots)
        }
    }
}

/// Vertical gap (px) between stacked cards.
pub const STACK_CARD_GAP_PX: f32 = 8.0;

// ── ONE LAYOUT SPEC — the single source of truth for card geometry ───────────────────────────────
// The box height and EVERY row's y come from the SAME ordered list of rows here, so the box is the
// SUM of its rows (it can never under-fit) and the renderer positions each row at the exact y this
// produces. No `STACK_BASE_H`, no per-element magic offsets that can drift from the height formula —
// that drift was the bug (the box allotted 58px but the title+value+track+prev stack drew ~60px, so
// the trailing line spilled under the border). Row HEIGHTS are the font line-boxes (fixed sizes →
// constant), so this is exact without needing the rasters, which lets the engine size the column
// without touching GDI. The renderer calls the SAME function for its draw y's.
const PAD_TOP: f32 = 5.0;
const PAD_BOT: f32 = 5.0;
const TITLE_H: f32 = 14.0; // 13px noun row
const GAP_TITLE_BODY: f32 = 1.0;
const BODY_H: f32 = 17.0; // 15px value/body line (also the per-extra-line growth)
const GAP_BODY_TRACK: f32 = 5.0;
const TRACK_H: f32 = 3.0; // the ranged track rail
const GAP_TO_PREV: f32 = 4.0;
const PREV_H: f32 = 11.0; // 11px "was X" trailing line
const ASK_Q_H: f32 = 18.0; // 16px question row
const GAP_Q_GRAMMAR: f32 = 3.0;
const ASK_GRAMMAR_H: f32 = 15.0; // 13px grammar row

// ── the ICON CHIP — the square badge in the left column. Its geometry lives HERE, with the row spec,
// so the box is SIZED to fit it and the chip is POSITIONED relative to the headline — never a fixed
// badge dropped into a box that knows nothing about it (the old bug: a 38px chip in a 42px box → 2px
// of clearance). Because these feed the SAME layout pass as the text rows, changing a font size or
// adding a row makes the chip's clearance and alignment follow automatically; there is no second,
// hand-tuned copy of these numbers to drift out of sync. ──
pub const CHIP_HALF: f32 = 19.0; // chip half-size → a 38px rounded-square badge
pub const CHIP_GLYPH_R: f32 = 13.0; // the glyph's scale inside the chip
const CHIP_MARGIN: f32 = 9.0; // the MINIMUM clearance kept between the chip and the card's top/bottom
const CONTENT_INSET: f32 = 11.0; // left inset to the icon column (clears the accent tab + a breath)
const ICON_TEXT_GAP: f32 = 17.0; // gap from the chip's right edge to the text column
/// The chip CENTRE's x, as an offset from the card's left inner edge (constant — same for every card).
pub const CHIP_DX: f32 = CONTENT_INSET + CHIP_HALF;
/// The text column's LEFT x, as an offset from the card's left inner edge.
pub const TEXT_DX: f32 = CONTENT_INSET + 2.0 * CHIP_HALF + ICON_TEXT_GAP;

/// What a card contains — drives the layout. `lines` = body rows (confirmation: wrapped value;
/// ask: grammar rows).
#[derive(Clone, Copy, Debug)]
pub struct CardShape {
    pub is_ask: bool,
    pub lines: usize,
    pub has_track: bool,
    pub has_prev: bool,
}

/// The resolved geometry — the box `height`, the icon chip's centre, and each row's y-centre, all
/// measured DOWN FROM THE CARD'S TOP INNER EDGE (`cy - height/2`). The renderer draws the chip at
/// `top + chip_cy` and title at `top + title_cy`, etc. — it owns NO geometry of its own, so it can
/// never disagree with the box this sizes. Unused rows for a shape are harmless (their `has_*`/`lines`
/// gate the draw).
#[derive(Clone, Copy, Default)]
pub struct CardLayout {
    pub height: f32,
    pub chip_cy: f32, // the icon chip's CENTRE y — anchored to the headline cluster, box-fitted
    pub title_cy: f32,
    pub body0_cy: f32, // first body line (confirmation) OR the question (ask)
    pub body_step: f32,
    pub track_cy: f32,
    pub prev_cy: f32,
    pub grammar0_cy: f32, // ask: first grammar row
}

/// THE one place card geometry is computed — a running cursor down the rows, so `height` is exactly
/// the content extent and every row y is consistent with it. Called by both the engine (for column
/// reflow) and the renderer (for the draw positions).
pub fn card_layout(shape: CardShape) -> CardLayout {
    let mut l = CardLayout {
        body_step: if shape.is_ask { ASK_GRAMMAR_H } else { BODY_H },
        ..Default::default()
    };
    // ── 1. lay the TEXT column out from a provisional top (PAD_TOP below y=0), and remember the
    // HEADLINE CLUSTER extent — the title+value (confirmation) or the question (ask). The chip anchors
    // to THAT cluster, so whatever secondary rows (track / prev / grammar) hang below never drag the
    // icon off the headline it belongs to. ──
    let mut y = PAD_TOP;
    let cluster_top = y;
    let cluster_bot;
    if shape.is_ask {
        l.body0_cy = y + ASK_Q_H / 2.0; // the question — the cluster
        y += ASK_Q_H;
        cluster_bot = y;
        y += GAP_Q_GRAMMAR;
        l.grammar0_cy = y + ASK_GRAMMAR_H / 2.0;
        y += ASK_GRAMMAR_H * shape.lines.max(1) as f32;
    } else {
        l.title_cy = y + TITLE_H / 2.0;
        y += TITLE_H + GAP_TITLE_BODY;
        l.body0_cy = y + BODY_H / 2.0;
        y += BODY_H * shape.lines.max(1) as f32;
        cluster_bot = y; // title + value lines = the cluster
        if shape.has_track {
            y += GAP_BODY_TRACK;
            l.track_cy = y + TRACK_H / 2.0;
            y += TRACK_H;
        }
        if shape.has_prev {
            y += GAP_TO_PREV;
            l.prev_cy = y + PREV_H / 2.0;
            y += PREV_H;
        }
    }
    let text_bot = y + PAD_BOT; // the text column's full extent (its top is 0)

    // ── 2. anchor the chip to the cluster's centre, then size the box as the UNION of the text column
    // and the chip+margin. Whichever is taller sets each edge: a SHORT card can never squish the chip
    // (the chip floor wins → a guaranteed CHIP_MARGIN of air); a TALL card keeps that same margin (the
    // text wins, the chip rides the headline). This single rule replaces every per-variant tuning. ──
    let chip_cy = (cluster_top + cluster_bot) / 2.0;
    let top = 0.0_f32.min(chip_cy - CHIP_HALF - CHIP_MARGIN);
    let bot = text_bot.max(chip_cy + CHIP_HALF + CHIP_MARGIN);

    // ── 3. normalise so the box's top inner edge is 0 — shift every row + the chip down by -top. ──
    let shift = -top;
    l.height = bot - top;
    l.chip_cy = chip_cy + shift;
    l.title_cy += shift;
    l.body0_cy += shift;
    l.track_cy += shift;
    l.prev_cy += shift;
    l.grammar0_cy += shift;
    l
}

/// A slot's card shape, from its content. Confirmation: title (always) + value wrapped to ≤3 lines +
/// a track (ranged) + a prev ("was X"). Ask: a fixed question + ≤2 grammar rows.
fn card_shape(s: &Slot) -> CardShape {
    if s.is_ask {
        return CardShape {
            is_ask: true,
            lines: wrap_count(&s.prev, 30, 3).max(1),
            has_track: false,
            has_prev: false,
        };
    }
    CardShape {
        is_ask: false,
        lines: wrap_count(&s.value, 28, 3).max(1),
        has_track: s.fill >= 0.0,
        has_prev: !s.prev.is_empty(),
    }
}

/// A slot's full card height (the engine's column-reflow target) — straight from the one layout spec.
fn card_height(s: &Slot) -> f32 {
    card_layout(card_shape(s)).height
}

/// How many wrapped rows `s` occupies (≤`lines`), honouring EXPLICIT '\n' breaks — the engine's mirror
/// of the renderer's `wrap_lines`, so the measured column height matches the drawn rows.
fn wrap_count(s: &str, cols: usize, lines: usize) -> usize {
    let lines = lines.max(1);
    let mut rows = 0usize;
    for seg in s.split('\n') {
        if rows >= lines {
            break;
        }
        rows += wrap_one_count(seg, cols, lines - rows);
    }
    rows.max(1).min(lines)
}

/// Row count for ONE line (no '\n').
fn wrap_one_count(s: &str, cols: usize, lines: usize) -> usize {
    let cols = cols.max(1);
    if s.trim().is_empty() {
        return 1;
    }
    let mut rows = 1usize;
    let mut col = 0usize;
    for word in s.split_whitespace() {
        let wlen = word.chars().count();
        if col == 0 {
            col = wlen.min(cols);
            // a giant word spills onto extra rows
            if wlen > cols {
                rows += (wlen - 1) / cols;
                col = wlen % cols;
            }
        } else if col + 1 + wlen <= cols {
            col += 1 + wlen;
        } else {
            rows += 1;
            col = wlen.min(cols);
            if wlen > cols {
                rows += (wlen - 1) / cols;
                col = wlen % cols;
            }
        }
        if rows >= lines {
            return lines;
        }
    }
    rows.min(lines)
}

/// One slot → a `NotifySlot` for the overlay, with the eased animation baked in. The column offset
/// is the slot's already-reflowed `cur_y` (eased toward its target by [`reflow`], run just before).
fn view(s: &Slot, alpha: f32, bump: (f32, f32), collapse: f32) -> NotifySlot {
    NotifySlot {
        ident: s.ident.clone(),
        rev: s.rev,
        title: s.title.clone(),
        value: s.value.clone(),
        prev: s.prev.clone(),
        glyph: s.glyph,
        dir: s.dir,
        fill: s.fill,
        prev_fill: s.prev_fill,
        panel: s.panel,
        is_ask: s.is_ask,
        y: s.cur_y,
        alpha,
        scale: bump.0,
        collapse,
        bump: bump.1,
    }
}

/// The presentation mode as the overlay's `u8` code (0 stack / 1 latest / 2 digest).
fn mode_code(m: StackMode) -> u8 {
    match m {
        StackMode::Stack => 0,
        StackMode::Latest => 1,
        StackMode::Digest => 2,
    }
}

/// What a confirmation should do, after the live config: play audio? show a card? + the audio params.
struct Act {
    audio: bool,
    card: Option<CardData>,
    vol: f32,
    tid: u8,
}

/// The card CONTENT a confirmation produces, before it joins the stack (placement + animation are the
/// engine's job). One per confirmation/coalesce.
struct CardData {
    ident: String,
    kind: Kind,
    glyph: WedgeGlyph,
    title: String,
    value: String,
    prev: String,
    dir: i8,
    fill: f32,
    prev_fill: f32,
    panel: bool,
}

/// Apply the live prefs to a confirmation. `None` = master off or this kind muted (do nothing).
fn decide(c: &Confirmation) -> Option<Act> {
    let p = Prefs::load_cached();
    if !p.notif_enabled || !p.notif_kind_on(c.kind) {
        return None;
    }
    let card = p.notif_place_xy().map(|_| {
        let (title, value, prev, dir) = format_card(c);
        let (fill, prev_fill) = range_fill(c);
        CardData {
            ident: c.ident.clone(),
            kind: c.kind,
            glyph: kind_glyph(c.kind),
            title,
            value,
            prev,
            dir,
            fill,
            prev_fill,
            panel: p.notif_panel,
        }
    });
    Some(Act {
        audio: p.notif_audio,
        card,
        vol: p.notif_volume,
        tid: Timbre::id_of(&p.notif_sound),
    })
}

/// The running musical state — turns a stream of confirmations into a never-clashing, emergent line.
struct Music {
    deg: i32,
    at: Instant,
}

impl Music {
    fn new() -> Music {
        Music {
            deg: 0,
            at: Instant::now() - Duration::from_secs(60),
        }
    }

    /// Strike the cue for `c` through `sound`, advancing the melodic state. `tid` is the user's chosen
    /// voice — but a battery ALARM (see [`battery_alarm`]) forces the GLASS timbre regardless, so the
    /// "something's wrong" gesture is sonically distinct even when the routine voice is the soft pulse.
    fn play(&mut self, c: &Confirmation, sound: &mut Option<SoundEngine>, vol: f32, tid: u8) {
        let Some(eng) = sound.as_mut() else { return };
        eng.set_volume(vol);
        let now = Instant::now();
        let (notes, voice) = self.cue(c, now);
        let tid = voice.unwrap_or(tid);
        for (deg, vel, delay) in notes {
            eng.strike(pentatonic_hz(deg, ROOT), vel, tid, delay);
        }
    }

    /// Build the note gesture, plus an OPTIONAL forced timbre id (`Some` overrides the user's voice —
    /// only the battery alarm uses it). The START degree rises within a burst (continuity) or resets
    /// to the event's leitmotif anchor; the SHAPE picks the gesture — ranged values rise/fall by
    /// direction, a profile switch arpeggiates, a layer "locks" with a two-note leap. Every NORMAL
    /// degree is pentatonic, so nothing the routine path can produce ever clashes.
    ///
    /// EXCEPTION — the battery ALARM. A `Kind::Battery` confirmation whose title reads low/critical
    /// (derived HERE from the engine's already-emitted text, NOT from any new field in the pure
    /// `confirm` core) bypasses the pretty pentatonic line for a deliberately-less-consonant urgency
    /// figure on the GLASS timbre. It does NOT advance the melodic state — an alarm is its own gesture,
    /// never a note in the emergent burst-melody.
    fn cue(&mut self, c: &Confirmation, now: Instant) -> (Vec<(i32, f32, u16)>, Option<u8>) {
        if c.kind == Kind::Battery && battery_alarm(&c.title) {
            return (alarm_gesture(&c.title), Some(Timbre::id_of("glass")));
        }
        let recent = now.duration_since(self.at) < CUE_WINDOW;
        let base = if recent {
            let d = self.deg + 1;
            if d > 8 { 0 } else { d } // rise a while, then fall home so a long burst stays in range
        } else {
            kind_anchor(c.kind)
        };
        let notes = match &c.shape {
            Shape::Ranged { value, .. } => match direction(c.prev.as_deref(), *value) {
                Dir::Up => vec![(base, 0.78, 0), (base + 1, 0.85, 95)],
                Dir::Down => vec![(base + 1, 0.85, 0), (base, 0.78, 95)],
                Dir::Flat => vec![(base, 0.85, 0)],
            },
            Shape::Discrete { .. } => match c.kind {
                Kind::Profile => vec![(base, 0.76, 0), (base + 1, 0.82, 90), (base + 2, 0.86, 180)],
                Kind::Layer => vec![(base, 0.85, 0), (base + 2, 0.8, 80)],
                _ => vec![(base, 0.85, 0)],
            },
        };
        self.deg = notes.last().map(|n| n.0).unwrap_or(base);
        self.at = now;
        (notes, None)
    }
}

/// Is this battery confirmation an ALARM (low / critical)? Derived from the title text the engine
/// already emits ("Battery low", "Battery critical") — the severity lives HERE in the app, never in
/// the pure `confirm` core (which only knows a battery changed, not how to dramatize it).
fn battery_alarm(title: &str) -> bool {
    let t = title.to_ascii_lowercase();
    t.contains("low") || t.contains("critical")
}

/// The battery ALARM tone gesture — "something's wrong" with eyes closed. A FALLING low-HIGH-low
/// figure that breaks the consonant pentatonic line: it leans on the leitmotif's neighbour degrees so
/// the interval grates a touch against the routine cues, lands with a harder velocity floor than any
/// pleasant cue, and — for CRITICAL — adds a third, lower, even-harder repeat so it reads as more
/// urgent than merely-low. Paired with the forced GLASS timbre (a brighter, more cutting voice than
/// the house pulse) it is unmistakably an alarm, not a routine layer/DPI toggle. NOT pentatonic-safe
/// by design (urgency over prettiness), and never advances the burst melody.
fn alarm_gesture(title: &str) -> Vec<(i32, f32, u16)> {
    // anchor a sixth below the routine battery degree (`kind_anchor` = 4) so the alarm sits in a
    // distinctly LOW, ominous register that no routine cue visits. The +1 reach is a non-pentatonic
    // semitone-ish lean (the GLASS √2 ratio adds its own inharmonic bite on top).
    const LOW: i32 = -2;
    let critical = title.to_ascii_lowercase().contains("critical");
    if critical {
        // low → high → LOWER, harder and three-beat: the most insistent figure.
        vec![(LOW, 0.92, 0), (LOW + 4, 0.97, 110), (LOW - 1, 1.0, 230)]
    } else {
        // low → high → low: a clear falling alarm, harder than any consonant cue.
        vec![(LOW, 0.88, 0), (LOW + 4, 0.95, 120), (LOW, 0.9, 250)]
    }
}

enum Dir {
    Up,
    Down,
    Flat,
}

/// Direction of a ranged change from its `prev` string to the new `value` (drives the rise/fall cue).
fn direction(prev: Option<&str>, value: f64) -> Dir {
    match prev.and_then(|p| p.parse::<f64>().ok()) {
        Some(p) if value > p => Dir::Up,
        Some(p) if value < p => Dir::Down,
        _ => Dir::Flat,
    }
}

/// Each event kind's home pentatonic degree — a recognizable pitch identity (leitmotif).
fn kind_anchor(k: Kind) -> i32 {
    match k {
        Kind::Profile => 0,
        Kind::Brightness | Kind::Scroll => 1,
        Kind::Dpi | Kind::Macro => 2,
        Kind::Polling => 3,
        Kind::Layer => 4,
        Kind::Battery => 4,
        // a side-plate swap sits a step ABOVE the routine cluster (and well clear of the battery
        // alarm's deliberately-low register) so a hardware-piece change reads as its own bright event.
        Kind::SidePlate => 5,
    }
}

/// Turn a confirmation into the card's STRUCTURED parts for the value-hierarchy layout:
/// `(title, value, prev, dir)`. `title` is the noun (small, dim, top); `value` is the headline
/// (large, bright); `prev` is the old reading already prefixed for its grammar ("was 800" for a
/// ranged change, "\u{2190} chill" for a discrete one), or "" when there's no prior; `dir` is the
/// ranged change's direction for the \u{2191}/\u{2193} cue (-1 down / 0 none / +1 up). The card
/// owns the typography (sizes, hue, the arrow) — this just supplies the words.
fn format_card(c: &Confirmation) -> (String, String, String, i8) {
    match &c.shape {
        Shape::Ranged { value, unit, .. } => {
            let now = format_num(*value, unit);
            let dir = match direction(c.prev.as_deref(), *value) {
                Dir::Up => 1,
                Dir::Down => -1,
                Dir::Flat => 0,
            };
            let prev = match &c.prev {
                Some(p) if !p.is_empty() => format!("was {p}"),
                _ => String::new(),
            };
            (c.title.clone(), now, prev, dir)
        }
        Shape::Discrete { label } => {
            let prev = match &c.prev {
                Some(p) if !p.is_empty() => format!("\u{2190} {p}"),
                _ => String::new(),
            };
            (c.title.clone(), label.clone(), prev, 0)
        }
    }
}

/// The card's TRACK position: where the new value (and the old, when known) sits on its 0..1 range —
/// the data the signature track-bar visualizes. A ranged change returns `(fill, prev_fill)` clamped
/// to 0..1, with `prev_fill = -1` when there's no prior reading to mark. A discrete change has no
/// track, so both are `-1` (the card draws no bar). This is the geometric twin of `format_card`:
/// that gives the words, this gives the position along the rail.
fn range_fill(c: &Confirmation) -> (f32, f32) {
    match &c.shape {
        Shape::Ranged {
            value, min, max, ..
        } => {
            let span = (max - min).abs().max(f64::EPSILON);
            let fill = (((value - min) / span) as f32).clamp(0.0, 1.0);
            let prev_fill = c
                .prev
                .as_deref()
                .and_then(|p| p.parse::<f64>().ok())
                .map(|p| (((p - min) / span) as f32).clamp(0.0, 1.0))
                .unwrap_or(-1.0);
            (fill, prev_fill)
        }
        Shape::Discrete { .. } => (-1.0, -1.0),
    }
}

/// Each kind's procedural GLYPH — the pre-attentive identity that replaces the generic pip, so a
/// card reads as "DPI" or "brightness" before a word is parsed. This is the card's ENTIRE type
/// identity: colour is the live material accent (the user's theme), never a hardcoded per-kind hue.
/// The app owns this mapping; core stays free of the overlay's icon set.
fn kind_glyph(k: Kind) -> crate::overlay::WedgeGlyph {
    use crate::overlay::WedgeGlyph as G;
    match k {
        Kind::Dpi => G::Target,
        Kind::Scroll => G::Scroll,
        Kind::Polling => G::Pulse,
        Kind::Brightness => G::Sun,
        Kind::Profile => G::ProfileDot,
        Kind::Layer => G::WindowStack,
        Kind::Macro => G::Terminal,
        Kind::Battery => G::Battery,
        Kind::SidePlate => G::SidePlate,
    }
}

/// Format a ranged value with only its symbol unit — the title already carries the noun (so a DPI
/// card reads "1600", not "1600 DPI").
fn format_num(v: f64, unit: &str) -> String {
    let n = v.round() as i64;
    match unit {
        "%" => format!("{n}%"),
        "Hz" => format!("{n} Hz"),
        _ => format!("{n}"),
    }
}

/// Fire ONE test notification, drawn at random from the pool of kinds the user has actually opted
/// into — derived from [`Kind::ALL`] filtered by their gates, never a hardcoded subset. Goes through
/// the exact same emission path a real change uses (so it genuinely exercises audio + card).
pub fn fire_test() -> String {
    let p = Prefs::load_cached();
    if !p.notif_enabled {
        return "notifications are off — enable them first".into();
    }
    if p.notif_place_xy().is_none() && !p.notif_audio {
        return "placement and audio are both off — turn one on to test".into();
    }
    let allowed: Vec<Kind> = Kind::ALL
        .into_iter()
        .filter(|k| p.notif_kind_on(*k))
        .collect();
    if allowed.is_empty() {
        return "every event is muted — enable one to test".into();
    }
    let pick = allowed[pseudo_rand() % allowed.len()];
    emit_sample(pick);
    format!("test notification fired — {}", pick.slug())
}

/// Emit a representative confirmation for `kind` via the real typed constructors.
fn emit_sample(kind: Kind) {
    use neuron::confirm;
    // A reserved pseudo-pid for the self-test, chosen OUTSIDE both the real-Razer-PID space (all low —
    // e.g. 0x0226) AND the audio-endpoint fallback pid 0 (glue's `…unwrap_or(0)`), so its per-device
    // de-dup baseline collides with nothing and a test fire can never suppress a genuine settings card.
    const TEST_PID: u16 = 0xFFFF;
    match kind {
        Kind::Dpi => confirm::dpi(TEST_PID, 1600, Some(800)),
        Kind::Scroll => confirm::scroll(TEST_PID, 3, 5, Some(2)),
        Kind::Polling => confirm::polling(1000, Some(500)),
        Kind::Brightness => confirm::brightness(70, Some(40)),
        Kind::Profile => confirm::profile("gaming", Some("chill")),
        Kind::Layer => confirm::layer("sniper", true),
        Kind::Macro => confirm::macro_fired("test macro"),
        Kind::Battery => confirm::battery(20, "Battery low", Some(45)),
        Kind::SidePlate => confirm::side_plate("12-button"),
    }
}

/// A throwaway index source for the test's random pick — no `rand` dependency, just the clock's
/// sub-second jitter. Good enough to vary which sample fires.
fn pseudo_rand() -> usize {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuron::confirm::{Confirmation, Kind, Shape};

    fn battery_confirm(title: &str, pct: f64) -> Confirmation {
        Confirmation {
            kind: Kind::Battery,
            shape: Shape::Ranged {
                value: pct,
                min: 0.0,
                max: 100.0,
                unit: "%",
            },
            title: title.into(),
            ident: "battery".into(),
            prev: Some("80".into()),
        }
    }

    /// Every card shape the engine can render — confirmations (±track, ±prev, 1..=3 value lines) and
    /// asks (1..=3 grammar rows). The layout's whole promise is that it stays consistent across ALL of
    /// these with no per-variant tuning, so the audit drives the audit: assert the invariants on every
    /// one. (Heights aside, this is the contract that kept the chip from ever squishing again.)
    fn all_shapes() -> Vec<CardShape> {
        let mut v = Vec::new();
        for lines in 1..=3usize {
            for has_track in [false, true] {
                for has_prev in [false, true] {
                    v.push(CardShape { is_ask: false, lines, has_track, has_prev });
                }
            }
            v.push(CardShape { is_ask: true, lines, has_track: false, has_prev: false });
        }
        v
    }

    #[test]
    fn card_layout_never_squishes_the_chip() {
        // the squish bug was a fixed 38px chip in a box sized from text alone (down to 42px → 2px of
        // clearance). The self-sizing box must now keep AT LEAST the design margin on BOTH edges of the
        // chip, for every shape — that's the floor that can never be crossed again.
        let eps = 0.01_f32;
        for s in all_shapes() {
            let l = card_layout(s);
            let top_clear = l.chip_cy - CHIP_HALF;
            let bot_clear = l.height - (l.chip_cy + CHIP_HALF);
            assert!(top_clear >= CHIP_MARGIN - eps, "{s:?}: chip top clearance {top_clear:.1} < {CHIP_MARGIN}");
            assert!(bot_clear >= CHIP_MARGIN - eps, "{s:?}: chip bottom clearance {bot_clear:.1} < {CHIP_MARGIN}");
        }
    }

    #[test]
    fn chip_stays_anchored_to_the_headline() {
        // the icon must badge the HEADLINE (title+value, or the question) the SAME way on every shape,
        // so a track bar or a second grammar row can never drag it off — the drift the audit found.
        let eps = 0.01_f32;
        for s in all_shapes() {
            let l = card_layout(s);
            let headline_cy = if s.is_ask {
                l.body0_cy // the question
            } else {
                let title_top = l.title_cy - TITLE_H / 2.0;
                let value_bot = l.body0_cy - BODY_H / 2.0 + BODY_H * s.lines.max(1) as f32;
                (title_top + value_bot) / 2.0
            };
            assert!(
                (l.chip_cy - headline_cy).abs() < eps,
                "{s:?}: chip at {:.1} drifted off headline centre {:.1}",
                l.chip_cy,
                headline_cy
            );
        }
    }

    #[test]
    fn box_contains_every_row_with_its_bottom_pad() {
        // the banner's promise: the box is the SUM of its content, so the last row never spills the
        // border. Assert at least PAD_BOT below the lowest drawn row, for every shape.
        let eps = 0.01_f32;
        for s in all_shapes() {
            let l = card_layout(s);
            let last_bot = if s.is_ask {
                l.grammar0_cy - ASK_GRAMMAR_H / 2.0 + ASK_GRAMMAR_H * s.lines.max(1) as f32
            } else if s.has_prev {
                l.prev_cy + PREV_H / 2.0
            } else if s.has_track {
                l.track_cy + TRACK_H / 2.0
            } else {
                l.body0_cy - BODY_H / 2.0 + BODY_H * s.lines.max(1) as f32
            };
            assert!(
                l.height - last_bot >= PAD_BOT - eps,
                "{s:?}: only {:.1}px below the last row (< PAD_BOT {PAD_BOT})",
                l.height - last_bot
            );
        }
    }

    // The severity is derived from the title text the engine already emits — NOT from any new field in
    // the pure confirm core. These are the exact titles confirm::battery is called with.
    #[test]
    fn battery_alarm_detects_low_and_critical_only() {
        assert!(battery_alarm("Battery low"));
        assert!(battery_alarm("Battery critical"));
        assert!(battery_alarm("LOW")); // case-insensitive
        // routine power events are NOT alarms — they keep the pleasant pentatonic cue
        assert!(!battery_alarm("Charging"));
        assert!(!battery_alarm("On battery"));
        assert!(!battery_alarm("Fully charged"));
    }

    // A dying battery must NOT sound like a routine layer toggle. The alarm cue is a DISTINCT,
    // urgent, deliberately-less-pretty gesture: forced GLASS timbre, a low anchor no routine cue
    // visits, a higher velocity floor, and (for low/critical) a multi-note falling figure.
    #[test]
    fn battery_alarm_cue_is_distinct_and_urgent() {
        let mut m = Music::new();
        let now = Instant::now();

        // routine battery (e.g. "Charging") uses the normal voice + the consonant kind anchor.
        let (routine, routine_voice) = m.cue(&battery_confirm("Charging", 90.0), now);
        assert!(routine_voice.is_none(), "a routine battery cue keeps the user's voice");

        // a LOW battery: forced GLASS, a falling multi-note gesture, harder than any routine cue.
        let mut m2 = Music::new();
        let (low, low_voice) = m2.cue(&battery_confirm("Battery low", 18.0), now);
        assert_eq!(low_voice, Some(Timbre::id_of("glass")), "alarm forces the GLASS timbre");
        assert!(low.len() >= 2, "the alarm is a multi-note figure, not a single ping");

        // urgency-by-velocity: the alarm's QUIETEST note is louder than the routine cue's LOUDEST.
        let alarm_floor = low.iter().map(|n| n.1).fold(f32::INFINITY, f32::min);
        let routine_ceil = routine.iter().map(|n| n.1).fold(0.0f32, f32::max);
        assert!(
            alarm_floor > routine_ceil,
            "alarm velocity floor {alarm_floor} must exceed routine ceiling {routine_ceil}"
        );

        // distinct REGISTER: the alarm lives below the routine battery leitmotif (kind_anchor=4),
        // so it never collides in pitch identity with a pleasant cue.
        let alarm_top = low.iter().map(|n| n.0).max().unwrap();
        assert!(
            alarm_top < kind_anchor(Kind::Battery),
            "the whole alarm sits below the routine battery anchor"
        );

        // an alarm is its own gesture — it must NOT advance the emergent burst melody. A fresh Music
        // starts at deg 0 with `at` far in the past; the alarm path returns before touching either.
        assert_eq!(m2.deg, 0, "the alarm left the melodic degree untouched");
        assert!(m2.at < now, "the alarm did not stamp the cue clock (no burst continuity)");
    }

    // CRITICAL is even more insistent than merely-low: more notes and a higher velocity floor.
    #[test]
    fn battery_critical_outweighs_low() {
        let mut m = Music::new();
        let (low, _) = m.cue(&battery_confirm("Battery low", 12.0), Instant::now());
        let mut m2 = Music::new();
        let (crit, _) = m2.cue(&battery_confirm("Battery critical", 4.0), Instant::now());
        let low_floor = low.iter().map(|n| n.1).fold(f32::INFINITY, f32::min);
        let crit_floor = crit.iter().map(|n| n.1).fold(f32::INFINITY, f32::min);
        assert!(crit_floor >= low_floor, "critical is at least as hard as low");
        assert!(crit.len() >= low.len(), "critical is at least as insistent as low");
    }
}
