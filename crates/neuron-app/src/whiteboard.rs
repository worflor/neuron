// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! WHITEBOARD — vector ink over the whole desk, on the spellweaving engine.
//!
//! A virtual-screen, click-through, topmost canvas that is HIDDEN (and costs nothing) until ink
//! exists. Entering the session (the whiteboard slot's rhythm, or — later — any bound action)
//! arms the grammar on the slot's own key:
//!   * HOLD + move = ink at the cursor (pen / highlighter / eraser, per the current tool);
//!   * TAP = undo the last stroke;
//!   * TAP-TAP = the TOOL WHEEL (the radial, your muscle memory: pen · highlight · erase ·
//!     colour · undo · clear · hide/show · done; colour opens a swatch wheel);
//!   * ESC = leave the session — the ink STAYS on screen (that's the point: marks for the
//!     meeting/screenshare survive until you clear or hide them).
//!
//! Strokes are vectors (point lists in virtual-screen space) — the `.gwyph` persistence rides
//! these directly once the codec port lands (the in-memory model already matches the format's
//! pen-stroke shape).
//!
//! Honesty rules: the canvas never takes input (WS_EX_TRANSPARENT), drawing reads only the
//! cursor, and nothing here consults the input-arm gate because nothing synthesizes input.

use crate::ui::{AppWindow, State};
use slint::ComponentHandle;
use std::sync::mpsc::{channel, Sender};

/// The 8 ink colours — the same literal palette the lighting panel offers (one app, one set).
pub const PALETTE: [u32; 8] = [
    0x4AF2B0, 0xFFFFFF, 0xFF3B30, 0xFF9500, 0xFFD60A, 0x30D158, 0x0A84FF, 0xBF5AF2,
];

/// The BRUSHES — real media, procedurally (deterministic hash noise, stable per stroke):
///   MARKER  the true whiteboard marker: crisp core, soft edge, faint dry-erase mottle
///   CRAYON  waxy grain — the tooth of the paper takes some pixels and refuses others
///   WATER   translucent pooling with darkened rims (the watercolour edge), never self-darkening
///   CHALK   dusty speckle, ragged at the edges
/// There is no separate highlighter — wide WATER at low alpha IS one (the old highlight
/// adapted into media that earn it); the old pen is the true marker.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Brush {
    Marker,
    Crayon,
    Water,
    Chalk,
    /// ICHOR — the app's material as ink: white-intent glass. The swatch becomes the ACCENT;
    /// the stroke runs a cool→white body that breaks into spectral fire along its edges.
    DirectedIntent,
}

impl Brush {
    fn name(self) -> &'static str {
        match self {
            Brush::Marker => "marker",
            Brush::Crayon => "crayon",
            Brush::Water => "water",
            Brush::Chalk => "chalk",
            Brush::DirectedIntent => "directed intent",
        }
    }
    fn parse(s: &str) -> Brush {
        match s {
            "crayon" => Brush::Crayon,
            "water" => Brush::Water,
            "chalk" => Brush::Chalk,
            "directed intent" => Brush::DirectedIntent,
            _ => Brush::Marker,
        }
    }
}

const BRUSHES: [Brush; 5] = [
    Brush::Marker,
    Brush::Crayon,
    Brush::Water,
    Brush::Chalk,
    Brush::DirectedIntent,
];

/// The PEN in your hand: brush + colour + thickness. What the board REMEMBERS (board.toml),
/// what a preset slot stores — your tool, parked your way.
#[derive(Clone, Copy, PartialEq)]
struct Pen {
    brush: Brush,
    color: u32,
    width: f32,
}

/// The thickness stops the size row offers (the old highlighter's 14 lives at the fat end).
const SIZES: [f32; 4] = [2.6, 3.6, 6.5, 14.0];

fn default_presets() -> Vec<Pen> {
    vec![
        Pen {
            brush: Brush::Marker,
            color: PALETTE[0],
            width: 3.6,
        },
        Pen {
            brush: Brush::Crayon,
            color: PALETTE[3],
            width: 6.5,
        },
        Pen {
            brush: Brush::Water,
            color: PALETTE[6],
            width: 14.0,
        },
        Pen {
            brush: Brush::Chalk,
            color: PALETTE[1],
            width: 3.6,
        },
    ]
}

// ── remembrance: board.toml — the pen, presets, and the palette's pin/parked spot all
// survive sessions AND restarts ──
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct BoardDoc {
    #[serde(default)]
    brush: String,
    #[serde(default)]
    color: u32,
    #[serde(default)]
    width: f32,
    #[serde(default)]
    presets: Vec<PresetDoc>,
    #[serde(default)]
    pinned: bool,
    #[serde(default = "d_park")]
    px: i32,
    #[serde(default = "d_park")]
    py: i32,
}

fn d_park() -> i32 {
    i32::MIN // sentinel: never parked
}

/// The palette's pin + parked position (shared between the doc and the palette thread).
static PAL_PIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static PAL_POS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

fn pack_pos(x: i32, y: i32) -> u64 {
    ((x as u32 as u64) << 32) | (y as u32 as u64)
}

fn unpack_pos(v: u64) -> Option<(i32, i32)> {
    (v != u64::MAX).then_some(((v >> 32) as u32 as i32, v as u32 as i32))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PresetDoc {
    brush: String,
    color: u32,
    width: f32,
}

fn board_path() -> std::path::PathBuf {
    neuron::runroot::run_root().join("board.toml")
}

fn board_load() -> (Pen, Vec<Pen>) {
    use std::sync::atomic::Ordering::SeqCst;
    let doc: BoardDoc = std::fs::read_to_string(board_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default();
    PAL_PIN.store(doc.pinned, SeqCst);
    if doc.px != i32::MIN && doc.py != i32::MIN {
        PAL_POS.store(pack_pos(doc.px, doc.py), SeqCst);
    }
    let pen = if doc.width > 0.0 {
        Pen {
            brush: Brush::parse(&doc.brush),
            color: doc.color,
            width: doc.width.clamp(1.0, 24.0),
        }
    } else {
        Pen {
            brush: Brush::Marker,
            color: PALETTE[0],
            width: 3.6,
        }
    };
    let mut presets: Vec<Pen> = doc
        .presets
        .iter()
        .map(|p| Pen {
            brush: Brush::parse(&p.brush),
            color: p.color,
            width: p.width.clamp(1.0, 24.0),
        })
        .collect();
    let defaults = default_presets();
    while presets.len() < defaults.len() {
        presets.push(defaults[presets.len()]);
    }
    presets.truncate(4);
    (pen, presets)
}

fn board_save(pen: &Pen, presets: &[Pen]) {
    use std::sync::atomic::Ordering::SeqCst;
    let park = unpack_pos(PAL_POS.load(SeqCst)).unwrap_or((i32::MIN, i32::MIN));
    let doc = BoardDoc {
        brush: pen.brush.name().into(),
        color: pen.color,
        width: pen.width,
        presets: presets
            .iter()
            .map(|p| PresetDoc {
                brush: p.brush.name().into(),
                color: p.color,
                width: p.width,
            })
            .collect(),
        pinned: PAL_PIN.load(SeqCst),
        px: park.0,
        py: park.1,
    };
    if let Ok(body) = toml::to_string_pretty(&doc) {
        if let Err(e) = neuron::salvage::atomic_write(&board_path(), body.as_bytes()) {
            eprintln!("neuron: failed to save whiteboard ({e})");
        }
    }
}

enum Cmd {
    Begin {
        color: u32,
        width: f32,
        brush: Brush,
        ghost: bool,
    },
    Pt(i32, i32),
    End,
    /// Commit the live stroke AS this polyline instead of its raw points (screen coords) —
    /// the shape-snap replacement. Full redraw (the freehand ink it replaces must vanish).
    EndAs(Vec<(i32, i32)>),
    /// Discard the live stroke entirely (a command-stroke's ghost trail must vanish unkept).
    DropLive,
    /// LIVE command feedback: underglow what the in-flight command stroke would act on —
    /// danger red for a delete (scribble/slash), accent for a tidy, white for a lasso take.
    /// `Command::None` clears the marks.
    Mark(Command, Vec<(i32, i32)>),
    /// COMMAND: delete every stroke the given path crosses (the scribble/slash, screen coords).
    /// Crossing NOTHING while a selection exists DESELECTS instead (the user's "a line over
    /// nothing unselects" model). The reply tells the caller what actually happened so the
    /// status never lies.
    DeleteCrossed(Vec<(i32, i32)>, std::sync::mpsc::Sender<Cull>),
    /// COMMAND: TIDY every stroke the path crosses — shape-snap it if the codec reads one,
    /// else smooth its jitter in place. The opt-in, retroactive shape-set.
    TidyCrossed(Vec<(i32, i32)>, std::sync::mpsc::Sender<usize>),
    /// COMMAND: select every stroke mostly inside the polygon (the lasso, screen coords).
    Select(Vec<(i32, i32)>),
    /// Translate the current selection (screen-space delta).
    NudgeSelection(i32, i32),
    RecolorSelection(u32),
    /// Re-brush / re-size the whole selection — the palette edits selected ink in place (so a
    /// pen change while strokes are selected applies to all of them, seamlessly).
    RebrushSelection(Brush),
    ResizeSelection(f32),
    Deselect,
    /// Drop the selection if one exists; reply whether it did (a blank command-stroke over
    /// nothing unselects, and the status must be truthful about it).
    DeselectAsk(std::sync::mpsc::Sender<bool>),
    /// Is this screen point inside the selection's reach? (reply on the one-shot channel)
    HitSelection(i32, i32, std::sync::mpsc::Sender<bool>),
    Undo,
    /// Put back the last undone stroke (the redo stack mirrors undo; any fresh edit clears it).
    Redo,
    Clear,
    Visible(bool),
    /// LASER (presentation) mode — none of these touch the kept ink; they live on a time-fading
    /// overlay so a presenter can point and react without leaving marks behind.
    ///   LaserBegin start a new trail RUN carrying the CURRENT pen — the trail renders in the same
    ///             material/size you're holding (no hardcoded laser look) and never bridges a lift
    ///   LaserPt   extend the live laser trail to this screen point (a comet that lingers ~2.5s)
    ///   LaserLift the hold ended — stop extending; the tail fades on its own
    ///   PingWheel  show/move the reactionary-ping radial at the cursor, sector under the aim lit
    ///   Ping       drop a comms ping of this kind at a screen point (blooms, then fades)
    LaserBegin {
        color: u32,
        width: f32,
        brush: Brush,
    },
    LaserPt(i32, i32),
    LaserLift,
    PingWheel(Option<(i32, i32, i32, u32)>),
    Ping(i32, i32, PingKind, u32),
}

/// A reactionary PING — the presenter's direct-comms vocabulary on the laser wheel. Each carries
/// its own intent colour (a ping COMMUNICATES, so per-kind colour is exactly the sanctioned use of
/// intent tint) and a one-glyph mark. The wheel lays them out clockwise from North.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PingKind {
    Here,  // ◎ reticle — the plain "look here" pointer
    Ask,   // ? — "is this clear? / what about this?"
    Bang,  // ! — "this matters — the key point"
    Yes,   // ✓ — "correct / agreed / done"
    No,    // ✗ — "wrong / avoid / not this"
    Arrow, // → — "go here / next / this leads to"
}

impl PingKind {
    /// Clockwise from North — the radial order (and count) the wheel + resolver share. The KIND is
    /// told apart by its SYMBOL now (the colour is the pen's, one palette everywhere), so the set is
    /// six abstract comms concepts a presenter / video-maker reaches for: attention, emphasis,
    /// direction, affirmation, negation, inquiry. No per-kind hue — `draw_ping_glyph` draws the mark.
    const WHEEL: [PingKind; 6] = [
        PingKind::Here,
        PingKind::Bang,
        PingKind::Arrow,
        PingKind::Yes,
        PingKind::No,
        PingKind::Ask,
    ];
    fn label(self) -> &'static str {
        match self {
            PingKind::Here => "here",
            PingKind::Ask => "?",
            PingKind::Bang => "!",
            PingKind::Yes => "yes",
            PingKind::No => "no",
            PingKind::Arrow => "go",
        }
    }
}

/// What a cull (scribble/slash) actually did — drives a truthful command status.
enum Cull {
    Deleted(usize),
    Deselected,
    Nothing,
}

/// One committed stroke — virtual-screen points + look. (The `.gwyph` pen-stroke shape.)
/// `seed` keys the brush texture so a stroke's grain is STABLE across redraws (a crayon line
/// must not shimmer when the canvas repaints); `ghost` = a command trail (flat translucent,
/// never textured, never kept).
struct Stroke {
    pts: Vec<(i32, i32)>,
    color: u32,
    width: f32,
    brush: Brush,
    ghost: bool,
    seed: u32,
}

// `service_sender` caches the send-end only once the worker spawned, so a refused spawn retries
// next call instead of stranding draw commands in a dead channel. `None` = can't start now.
fn canvas() -> Option<Sender<Cmd>> {
    static TX: crate::worker::Service<Cmd> = crate::worker::Service::new();
    crate::worker::service_sender(&TX, "neuron-whiteboard", imp::canvas_thread)
}

/// Send a canvas command, starting the worker on demand; silently no-ops if it can't start.
fn canvas_send(cmd: Cmd) {
    if let Some(tx) = canvas() {
        let _ = tx.send(cmd);
    }
}

/// Shared board state: the SESSION draws with it, the PALETTE edits it live — that's what lets
/// a pinned palette stay up while you draw (two threads, one tiny lock, copy-out per stroke).
struct BoardState {
    pen: Pen,
    presets: Vec<Pen>,
    veiled: bool,
    /// LASER (presentation) mode: a hold paints a fading laser trail instead of permanent ink,
    /// and the undo/redo gestures become comms PINGS — a presenter's pointer, not an editor.
    laser: bool,
}

/// A session is live (drives the toggle and keeps doubles impossible).
static WB_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set by anything that wants the session to END (toggle again, palette QUIT).
static WB_CLOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Is a whiteboard session live right now? (The weave service consults this: while the board
/// is open it OWNS its use key — slots sharing that key stand down so a draw-hold inks instead
/// of casting.)
pub fn active() -> bool {
    // ...and only while the session is genuinely beating. A board thread that died or wedged
    // without clearing WB_ACTIVE would dark its use-key forever (the "whiteboard end breaks the
    // weave service" freeze); organ_stalled heals it — the key returns to the weave on its own.
    WB_ACTIVE.load(std::sync::atomic::Ordering::SeqCst)
        && !crate::flight::organ_stalled(crate::flight::organ::WHITEBOARD)
}

/// Ask the live session to end (the palette's QUIT; the ink stays).
pub(crate) fn request_close() {
    WB_CLOSE.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// TOGGLE the whiteboard session — open it on its OWN thread, or close the one that's live.
/// The weave service calls this and returns immediately: teleport, glyphs, beacons and the
/// rhythm captures all keep working while the board is open (the session being modal on the
/// weave thread was the "whiteboard bricks the spell system" bug).
#[cfg(windows)]
pub fn toggle(weak: &slint::Weak<AppWindow>) {
    use std::sync::atomic::Ordering::SeqCst;
    if WB_ACTIVE.swap(true, SeqCst) {
        WB_CLOSE.store(true, SeqCst); // already open — this press closes it (ink stays)
        return;
    }
    WB_CLOSE.store(false, SeqCst);
    let weak = weak.clone();
    // RELEASE covers completion, panic, AND spawn refusal alike — WB_ACTIVE must never stay
    // claimed with nothing running behind it (that's "the board's key is dead until restart").
    crate::worker::spawn_guarded(
        "neuron-board-session",
        || {
            crate::flight::pulse_clear(crate::flight::organ::WHITEBOARD);
            WB_CLOSE.store(false, SeqCst);
            WB_ACTIVE.store(false, SeqCst);
            crate::flight::trace("wb", "session ended (key released to weave)", 0);
        },
        move || session_loop(&weak),
    );
}

#[cfg(not(windows))]
pub fn toggle(_weak: &slint::Weak<AppWindow>) {}

/// The whiteboard session body — its own thread, polling only (keys + cursor; no raw-input
/// registration, so it can never steal the weave engine's motion stream). Ends on ESC, the
/// toggle, the palette's QUIT, or a config reload; PAUSES (doesn't die) while the editor or a
/// presenting beacon owns the trigger.
#[cfg(windows)]
fn session_loop(weak: &slint::Weak<AppWindow>) {
    let cast = neuron::cast::CastConfig::load();
    let (slots, _) = cast.mode_slots();
    let vk = slots
        .iter()
        .find(|s| s.action == neuron::action::Action::Whiteboard)
        .map(|s| s.vk)
        .unwrap_or(cast.trigger);
    let feel = neuron::feel::FeelConfig::load();
    let gen = crate::dispatch::reload_generation();
    let done = || {
        // HEARTBEAT the session organ from here: done()/cancel() is polled by EVERY inner hold loop
        // (ink, command, drag, laser, ping-wheel), so a long legitimate stroke keeps the session
        // "alive" and the weave's key-standdown deadman (organ_stalled) never false-heals mid-draw —
        // which was popping the radial up partway through a long stroke. The outer loop pulses too.
        crate::flight::pulse(crate::flight::organ::WHITEBOARD);
        WB_CLOSE.load(std::sync::atomic::Ordering::SeqCst)
            || crate::dispatch::reload_generation() != gen
    };
    // No render worker (a refused spawn) → no session to run; the next open retries.
    let Some(tx) = canvas() else {
        return;
    };
    // hold the sender for this whole session and pass it to the gesture helpers by reference. If the
    // worker died mid-session sends here silently drop (as before) — the self-heal is the NEXT
    // `canvas()` (next open), which is why we must not stash this handle beyond the session.
    let tx = &tx;
    // the board REMEMBERS — and a fresh session always opens holding PRESET 1 (your default
    // pen: right-click slot 1 in the palette to change what "default" means).
    let (_, presets) = board_load();
    let state = std::sync::Arc::new(std::sync::Mutex::new(BoardState {
        pen: presets.first().copied().unwrap_or(Pen {
            brush: Brush::Marker,
            color: PALETTE[0],
            width: 3.6,
        }),
        presets,
        veiled: false,
        laser: false,
    }));
    let _ = tx.send(Cmd::Visible(true));
    let status = |weak: &slint::Weak<AppWindow>, pen: &Pen| {
        post_status(
            weak,
            format!(
                "whiteboard \u{00b7} {} #{:06X} \u{00b7} w{:.1} \u{00b7} hold = ink \u{00b7} 2\u{00d7}click+hold = command \u{00b7} 3\u{00d7}click = palette \u{00b7} esc = done",
                pen.brush.name(),
                pen.color,
                pen.width,
            ),
        )
    };
    {
        let pen = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pen;
        status(weak, &pen);
    }

    // ── the USE-KEY grammar: one key, counted clicks, holds branch ──
    //   hold            = ink (end-dwell = shape-set)        [on a selection: hold = drag it]
    //   2×click + hold  = COMMAND stroke (scribble/slash = delete · loop = select · ~ = tidy)
    //   3×click         = the canvas PALETTE at the cursor
    //   1 click         = undo — INSIDE the selection while one exists, global otherwise
    //   2×click         = unselect (matures after the multi-click window)
    // LASER (presentation) mode re-skins this same grammar without changing a single beat: the
    //   hold paints a fading laser trail (not ink); the tap = a PING here (not undo); the
    //   tap-and-hold-in-place = the reactionary-ping WHEEL (not redo). Commands/palette unchanged.
    let mut taps: u8 = 0;
    let mut last_release = std::time::Instant::now();
    let mut last_tap = (0i32, 0i32); // where the maturing tap landed (a laser-mode ping needs it)
                                     // the multi-click window gets a generous FLOOR here: the weave engine's gap_ms is tuned for
                                     // crisp cast rhythms, but a board command is "click, click-and-draw" mid-thought — a window
                                     // that tight made command mode feel like it kept deactivating (the second press landed a few
                                     // ms late, the rhythm decayed, and the hold turned into ink WITH a stray undo fired).
    let click_window = feel.gap_ms.max(420);
    'session: loop {
        crate::flight::pulse(crate::flight::organ::WHITEBOARD);
        if done() || neuron::glyph::key_down(0x1B) {
            break;
        }
        // the trigger is on loan: while the editor records or a beacon presents, the session
        // PAUSES (it doesn't die — your board is exactly where you left it when they finish).
        if crate::beacon::editor_weave_active() || crate::beacon::beacon_presenting() {
            std::thread::sleep(std::time::Duration::from_millis(30));
            continue;
        }
        // matured taps resolve once the multi-click window closes
        if taps > 0 && last_release.elapsed().as_millis() as u64 > click_window {
            let laser = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).laser;
            match taps {
                // a single tap: PING here in laser mode (the presenter's "look"), else UNDO
                1 if laser => {
                    let pc = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pen.color;
                    let _ = tx.send(Cmd::Ping(last_tap.0, last_tap.1, PingKind::Here, pc));
                }
                1 => {
                    let _ = tx.send(Cmd::Undo);
                }
                2 => {
                    let _ = tx.send(Cmd::Deselect);
                }
                _ => {}
            }
            taps = 0;
        }
        if neuron::glyph::key_down(vk) {
            let pressed = std::time::Instant::now();
            let start = cursor_pos();
            loop {
                if done() {
                    break 'session;
                }
                let now = cursor_pos();
                let moved = (now.0 - start.0).pow(2) + (now.1 - start.1).pow(2) >= 9;
                let held = pressed.elapsed().as_millis() as u64 >= feel.hold_ms;
                if !neuron::glyph::key_down(vk) {
                    // TAP-AND-HOLD IN PLACE = REDO (the mirror of tap = undo): a single press
                    // held past the hold beat, never moved, then released. Consumed here so it
                    // never matures into an undo tap. (Silent, like the undo tap.) In LASER mode
                    // this same gesture would have raised the ping wheel — but the wheel resolves
                    // interactively below (on the held branch), so a *released* hold-in-place here
                    // is the wheel being dismissed with no pick; nothing to do.
                    if taps == 0 && held && !moved {
                        if !state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).laser {
                            let _ = tx.send(Cmd::Redo);
                        }
                        break;
                    }
                    taps = taps.saturating_add(1);
                    last_release = std::time::Instant::now();
                    last_tap = start; // a laser-mode ping fires here when this tap matures
                    if taps >= 3 {
                        taps = 0;
                        palette::show(weak.clone(), state.clone());
                    }
                    break;
                }
                // LASER mode: a hold-in-place (no move yet) at taps==0 raises the reactionary-ping
                // WHEEL — the redo gesture re-cast as direct comms (resolves by release direction).
                if held && !moved && taps == 0 && state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).laser {
                    taps = 0;
                    ping_wheel(weak, tx, vk, &done, start, state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pen.color);
                    break;
                }
                // a held-in-place FIRST press is a redo candidate (resolves on release above),
                // so for t==0 only MOVEMENT commits to ink/drag; t>=1 still branches on the
                // hold (command-stroke / palette read their own motion).
                if moved || (held && taps != 0) {
                    let t = taps;
                    taps = 0;
                    state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).veiled = false; // a hold-action brings the ink back
                    match t {
                        // ── plain hold: laser trail (presentation), else drag a grabbed
                        //    selection, else ink ──
                        0 => {
                            if state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).laser {
                                let pen = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pen;
                                laser_trail(tx, vk, &done, start, &pen);
                            } else if selection_hit(tx, start) {
                                drag_selection(tx, vk, &done, start);
                            } else {
                                let _ = tx.send(Cmd::Deselect);
                                let pen = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pen;
                                draw_stroke(weak, tx, vk, &done, start, &pen);
                            }
                        }
                        // ── double-click + hold: a COMMAND stroke ──
                        //    (`taps` counts completed releases, so t==1 here means
                        //     one full click *then* this held press — the second click
                        //     of "2×click+hold" is the one being held)
                        1 => command_stroke(weak, tx, vk, &done, start),
                        // ── over-counted clicks ending in a hold: treat as palette intent ──
                        _ => palette::show(weak.clone(), state.clone()),
                    }
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }
    palette::session_ended();
    post_status(
        weak,
        "whiteboard closed \u{2014} ink stays until you clear it".into(),
    );
}

/// Ink one stroke (the plain hold): live points to the canvas, end-dwell = shape-set.
#[cfg(windows)]
fn draw_stroke(
    weak: &slint::Weak<AppWindow>,
    tx: &Sender<Cmd>,
    vk: i32,
    cancel: &(impl Fn() -> bool + ?Sized),
    start: (i32, i32),
    pen: &Pen,
) {
    let _ = tx.send(Cmd::Begin {
        color: pen.color,
        width: pen.width,
        brush: pen.brush,
        ghost: false,
    });
    let _ = tx.send(Cmd::Pt(start.0, start.1));
    let mut pts: Vec<(i32, i32)> = vec![start];
    let mut last_move = std::time::Instant::now();
    // INK STABILIZATION — speed-adaptive EMA on the cursor. Slow, deliberate strokes get the
    // most settling (where hand tremor lives); fast strokes ride nearly raw so the line never
    // lags a flick. Tuned to read like a gentle ~4% tablet smoothing — felt, not seen.
    let mut sx = start.0 as f64;
    let mut sy = start.1 as f64;
    while neuron::glyph::key_down(vk) && !cancel() {
        let p = cursor_pos();
        let dx = p.0 as f64 - sx;
        let dy = p.1 as f64 - sy;
        let speed = (dx * dx + dy * dy).sqrt(); // px per 6ms tick
        let alpha = (0.30 + speed * 0.035).clamp(0.30, 0.95);
        sx += dx * alpha;
        sy += dy * alpha;
        let q = (sx.round() as i32, sy.round() as i32);
        let lp = *pts.last().unwrap();
        if (q.0 - lp.0).pow(2) + (q.1 - lp.1).pow(2) > 4 {
            let _ = tx.send(Cmd::Pt(q.0, q.1));
            pts.push(q);
            last_move = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(6));
    }
    // SHAPE INTENT — the haptic dwell: held STILL at the stroke's end for a beat = "set this".
    // A clear template match replaces the ink; ambiguity (or any normal release) keeps every
    // pixel as drawn. Shapes are asked for with the hand, never forced.
    let setting = last_move.elapsed().as_millis() >= 300 && pts.len() >= 8;
    let snapped = setting
        .then(|| {
            let body: Vec<neuron::glyph::C> = pts
                .iter()
                .map(|p| neuron::glyph::C::new(p.0 as f64, p.1 as f64))
                .collect();
            neuron::shapes::snap(&body)
        })
        .flatten();
    match snapped {
        Some(shape) => {
            let poly: Vec<(i32, i32)> = neuron::shapes::polyline(&shape)
                .iter()
                .map(|(x, y)| (x.round() as i32, y.round() as i32))
                .collect();
            let _ = tx.send(Cmd::EndAs(poly));
            post_status(
                weak,
                format!("\u{2728} set: {}", neuron::shapes::identity(&shape)),
            );
        }
        None => {
            let _ = tx.send(Cmd::End);
        }
    }
}

/// LASER mode's hold: feed a fading pointer trail (never kept ink). Streams the cursor to the
/// canvas as `LaserPt`; on release the trail lifts and fades itself over LASER_MS. No shape-snap,
/// no smoothing settle — a laser is raw and immediate.
#[cfg(windows)]
fn laser_trail(
    tx: &Sender<Cmd>,
    vk: i32,
    cancel: &(impl Fn() -> bool + ?Sized),
    start: (i32, i32),
    pen: &Pen,
) {
    // a NEW run carrying the CURRENT pen — the trail renders in whatever material/size you hold (no
    // hardcoded laser look), and a run never bridges to the previous one across a lift.
    let _ = tx.send(Cmd::LaserBegin {
        color: pen.color,
        width: pen.width,
        brush: pen.brush,
    });
    let _ = tx.send(Cmd::LaserPt(start.0, start.1));
    let mut last = start;
    // the SAME speed-adaptive EMA the pen uses (draw_stroke): the laser followed the RAW cursor and
    // so beaded at every jitter while the pen blends — smoothing the path makes the trail one clean
    // ribbon, not a string of dots. Slow moves settle most (where tremor lives); flicks ride near-raw.
    let mut sx = start.0 as f64;
    let mut sy = start.1 as f64;
    while neuron::glyph::key_down(vk) && !cancel() {
        let p = cursor_pos();
        let dx = p.0 as f64 - sx;
        let dy = p.1 as f64 - sy;
        let speed = (dx * dx + dy * dy).sqrt();
        let alpha = (0.30 + speed * 0.035).clamp(0.30, 0.95);
        sx += dx * alpha;
        sy += dy * alpha;
        let q = (sx.round() as i32, sy.round() as i32);
        if (q.0 - last.0).pow(2) + (q.1 - last.1).pow(2) > 4 {
            let _ = tx.send(Cmd::LaserPt(q.0, q.1));
            last = q;
        }
        std::thread::sleep(std::time::Duration::from_millis(6));
    }
    let _ = tx.send(Cmd::LaserLift); // stop extending; the tail melts away on its own
}

/// LASER mode's tap-and-hold-in-place: the REACTIONARY-PING WHEEL — the direct-comms radial. The
/// six pings sit clockwise from North; the aim under the cursor lights live (the canvas draws it),
/// and the release drops that ping where the wheel was raised. A flick back to centre cancels.
#[cfg(windows)]
fn ping_wheel(
    weak: &slint::Weak<AppWindow>,
    tx: &Sender<Cmd>,
    vk: i32,
    cancel: &(impl Fn() -> bool + ?Sized),
    origin: (i32, i32),
    color: u32,
) {
    use std::f32::consts::TAU;
    let n = PingKind::WHEEL.len() as i32;
    // which option a cursor offset is aiming at (None inside the dead-zone = no pick / cancel)
    let aim = |p: (i32, i32)| -> Option<i32> {
        let (dx, dy) = ((p.0 - origin.0) as f32, (p.1 - origin.1) as f32);
        if dx * dx + dy * dy < 22.0 * 22.0 {
            return None; // dead-zone: not committed to any spoke
        }
        // bearing 0 = North, clockwise (atan2 with screen-y down): match the canvas layout
        let mut rel = dx.atan2(-dy); // 0 = up
        if rel < 0.0 {
            rel += TAU;
        }
        Some(((rel / TAU * n as f32).round() as i32).rem_euclid(n))
    };
    let mut sect = -1;
    while neuron::glyph::key_down(vk) && !cancel() {
        let pick = aim(cursor_pos());
        let s = pick.unwrap_or(-1);
        if s != sect {
            sect = s;
            let _ = tx.send(Cmd::PingWheel(Some((origin.0, origin.1, sect, color))));
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
    let _ = tx.send(Cmd::PingWheel(None)); // the wheel closes
    if let Some(k) = aim(cursor_pos()).and_then(|s| PingKind::WHEEL.get(s as usize).copied()) {
        let _ = tx.send(Cmd::Ping(origin.0, origin.1, k, color));
        post_status(weak, format!("ping \u{00b7} {}", k.label()));
    }
}

/// A COMMAND stroke (double-click + hold): drawn as a faint ghost (never kept), read LIVE as a
/// gesture — what it would act on lights up while you draw (danger red for a delete, accent for
/// a tidy, white for a lasso take), then the release commits:
///   SCRIBBLE / SLASH  delete what they cross (crossing a selection takes ALL of it)
///   LOOP              selects what it encircles
///   WAVE (~)          tidies what it crosses — snap the shapes, smooth the jitter
#[cfg(windows)]
fn command_stroke(
    weak: &slint::Weak<AppWindow>,
    tx: &Sender<Cmd>,
    vk: i32,
    cancel: &(impl Fn() -> bool + ?Sized),
    start: (i32, i32),
) {
    // the ghost trail: thin, translucent, white — visibly a command, not ink.
    let _ = tx.send(Cmd::Begin {
        color: 0xFFFFFF,
        width: 2.0,
        brush: Brush::Marker,
        ghost: true,
    });
    let _ = tx.send(Cmd::Pt(start.0, start.1));
    let mut pts: Vec<(i32, i32)> = vec![start];
    let mut marked = Command::None;
    let mut last_mark = std::time::Instant::now();
    while neuron::glyph::key_down(vk) && !cancel() {
        let p = cursor_pos();
        let lp = *pts.last().unwrap();
        if (p.0 - lp.0).pow(2) + (p.1 - lp.1).pow(2) > 4 {
            let _ = tx.send(Cmd::Pt(p.0, p.1));
            pts.push(p);
        }
        // LIVE intent: re-read the gesture as it grows; the canvas underglows the would-be
        // targets the moment a command's threshold lands (the "it sees me" feedback).
        if last_mark.elapsed().as_millis() >= 90 {
            let now = classify_command(&pts);
            if now != marked || now != Command::None {
                let _ = tx.send(Cmd::Mark(now, pts.clone()));
                marked = now;
            }
            last_mark = std::time::Instant::now();
        }
        std::thread::sleep(std::time::Duration::from_millis(6));
    }
    let _ = tx.send(Cmd::DropLive); // the ghost never survives — only its meaning does
    let _ = tx.send(Cmd::Mark(Command::None, Vec::new())); // marks resolve into the action
    match classify_command(&pts) {
        Command::Scribble => {
            let (rtx, rrx) = channel();
            let _ = tx.send(Cmd::DeleteCrossed(pts, rtx));
            post_status(weak, cull_status(&rrx, "scribbled out"));
        }
        Command::Slash => {
            let (rtx, rrx) = channel();
            let _ = tx.send(Cmd::DeleteCrossed(pts, rtx));
            post_status(weak, cull_status(&rrx, "crossed out"));
        }
        Command::Tidy => {
            let (rtx, rrx) = channel();
            let _ = tx.send(Cmd::TidyCrossed(pts, rtx));
            let n = rrx
                .recv_timeout(std::time::Duration::from_millis(200))
                .unwrap_or(0);
            post_status(
                weak,
                if n == 0 {
                    "\u{301c} nothing under the wave".into()
                } else {
                    format!(
                        "\u{301c} tidied {n} stroke{}",
                        if n == 1 { "" } else { "s" }
                    )
                },
            );
        }
        Command::Lasso => {
            let _ = tx.send(Cmd::Select(pts));
            post_status(
                weak,
                "selected \u{00b7} hold it to drag \u{00b7} scribble it to delete \u{00b7} tap = undo here \u{00b7} 2\u{00d7}click = unselect"
                    .into(),
            );
        }
        Command::None => {
            // a blank command-stroke (a line in command mode that read no glyph) UNSELECTS when
            // a selection exists — that's the user's mental model. With nothing selected it stays
            // a plain misread, so the status only claims an unselect when one truly happened.
            let (rtx, rrx) = channel();
            let _ = tx.send(Cmd::DeselectAsk(rtx));
            let dropped = rrx
                .recv_timeout(std::time::Duration::from_millis(60))
                .unwrap_or(false);
            post_status(
                weak,
                if dropped {
                    "unselected".into()
                } else {
                    "command not read \u{2014} scribble/slash delete \u{00b7} loop selects \u{00b7} \u{301c} tidies".into()
                },
            );
        }
    }
}

/// Turn a cull reply into a truthful status line (the verb for a real delete, else the honest
/// "unselected" / "nothing there").
#[cfg(windows)]
fn cull_status(rrx: &std::sync::mpsc::Receiver<Cull>, verb: &str) -> String {
    match rrx.recv_timeout(std::time::Duration::from_millis(60)) {
        Ok(Cull::Deleted(n)) => format!(
            "{verb} \u{00b7} {n} stroke{}",
            if n == 1 { "" } else { "s" }
        ),
        Ok(Cull::Deselected) => "unselected".into(),
        _ => "nothing there".into(),
    }
}

#[derive(PartialEq, Debug, Clone, Copy)]
enum Command {
    /// zig-zag over ink = delete it
    Scribble,
    /// one straight stroke crossing ink = delete just what it crosses (the lighter scratch-out)
    Slash,
    /// a closed loop = select what it encircles
    Lasso,
    /// a wave (~) over ink = tidy it: shape-snap what the codec reads, smooth the rest
    Tidy,
    None,
}

/// Read a command stroke's intent from its geometry. Pure + testable. The four commands occupy
/// four distinct geometric corners, so none can shadow another:
///   SCRIBBLE  reverses along its DOMINANT axis, long-for-its-net   (zig-zag)
///   LASSO     closes on itself around a real area                  (loop)
///   TIDY      reverses along the PERPENDICULAR axis while traveling (wave ~)
///   SLASH     stays on its chord                                   (straight)
fn classify_command(pts: &[(i32, i32)]) -> Command {
    if pts.len() < 6 {
        return Command::None;
    }
    let (sx, sy) = pts[0];
    let (ex, ey) = *pts.last().unwrap();
    let net = (((ex - sx).pow(2) + (ey - sy).pow(2)) as f64).sqrt();
    let arc: f64 = pts
        .windows(2)
        .map(|w| (((w[1].0 - w[0].0).pow(2) + (w[1].1 - w[0].1).pow(2)) as f64).sqrt())
        .sum();
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in pts {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    let diag = ((((x1 - x0).pow(2)) + ((y1 - y0).pow(2))) as f64)
        .sqrt()
        .max(1.0);
    // direction reversals along the dominant axis (zig-zag) AND the perpendicular axis (wave),
    // with a small per-leg distance so pixel jitter can't fake a reversal.
    let dom_is_x = (x1 - x0) >= (y1 - y0);
    let mut rev_dom = 0;
    let mut rev_perp = 0;
    let (mut sign_d, mut sign_p) = (0i32, 0i32);
    let (mut leg_d, mut leg_p) = (0i32, 0i32);
    for w in pts.windows(2) {
        let dx = w[1].0 - w[0].0;
        let dy = w[1].1 - w[0].1;
        let (d, p) = if dom_is_x { (dx, dy) } else { (dy, dx) };
        leg_d += d;
        leg_p += p;
        if leg_d.abs() >= 6 {
            let s = leg_d.signum();
            if sign_d != 0 && s != sign_d {
                rev_dom += 1;
            }
            sign_d = s;
            leg_d = 0;
        }
        if leg_p.abs() >= 6 {
            let s = leg_p.signum();
            if sign_p != 0 && s != sign_p {
                rev_perp += 1;
            }
            sign_p = s;
            leg_p = 0;
        }
    }
    if rev_dom >= 3 && arc / net.max(8.0) >= 2.2 {
        return Command::Scribble;
    }
    if net / diag <= 0.30 && diag >= 40.0 && arc >= 1.5 * diag {
        return Command::Lasso;
    }
    // wave: travels along its dominant axis while oscillating across it
    if rev_perp >= 3 && rev_dom <= 1 && net >= 40.0 && arc / net <= 2.6 {
        return Command::Tidy;
    }
    // straight: max perpendicular deviation from the chord stays small
    if net >= 30.0 {
        let chord = net.max(1e-9);
        let max_dev = pts
            .iter()
            .map(|p| {
                (((ex - sx) as f64) * ((sy - p.1) as f64)
                    - ((sx - p.0) as f64) * ((ey - sy) as f64))
                    .abs()
                    / chord
            })
            .fold(0.0f64, f64::max);
        if max_dev / chord <= 0.10 {
            return Command::Slash;
        }
    }
    Command::None
}

/// Is this screen point on the current selection? (sync round-trip to the canvas thread)
#[cfg(windows)]
fn selection_hit(tx: &Sender<Cmd>, p: (i32, i32)) -> bool {
    let (rtx, rrx) = channel();
    let _ = tx.send(Cmd::HitSelection(p.0, p.1, rtx));
    rrx.recv_timeout(std::time::Duration::from_millis(60))
        .unwrap_or(false)
}

/// Drag the selection while the use-key is held — the GRABBER is the selection itself.
#[cfg(windows)]
fn drag_selection(
    tx: &Sender<Cmd>,
    vk: i32,
    cancel: &(impl Fn() -> bool + ?Sized),
    start: (i32, i32),
) {
    let mut last = start;
    while neuron::glyph::key_down(vk) && !cancel() {
        let p = cursor_pos();
        if p != last {
            let _ = tx.send(Cmd::NudgeSelection(p.0 - last.0, p.1 - last.1));
            last = p;
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

fn cursor_pos() -> (i32, i32) {
    #[cfg(windows)]
    unsafe {
        let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
        (p.x, p.y)
    }
    #[cfg(not(windows))]
    (0, 0)
}

fn post_status(weak: &slint::Weak<AppWindow>, line: String) {
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

// ── the canvas: a virtual-screen layered window owning the strokes ──────────────────────────────
#[cfg(windows)]
mod imp {
    use super::{Brush, Cmd, Command, Cull, PingKind, Stroke};
    use crate::raster::{FieldBuf, FieldView, PixelBuf, PixelView};
    use std::sync::mpsc::Receiver;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{POINT, SIZE};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetSystemMetrics, PeekMessageW, SetWindowPos, ShowWindow,
        TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
        SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE,
        WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    };

    /// How close (px) the eraser must pass to a stroke's polyline to take it.
    const ERASE_REACH: f64 = 12.0;

    /// Laser-trail lifetime (ms): a head point lingers this long, then it's gone.
    const LASER_MS: u128 = 2500;
    /// Ping-bloom lifetime (ms): the ring expands and fades over this window.
    const PING_MS: u128 = 1800;

    pub fn canvas_thread(rx: Receiver<Cmd>) {
        unsafe {
            let (vx, vy) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
            );
            let (w, h) = (
                GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
                GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            );
            // the canvas: one virtual-screen-sized click-through layered window + its top-down BGRA
            // DIB, presented per frame at the virtual-screen origin (vx, vy).
            let surf = match crate::surface::LayeredSurface::new(&crate::surface::SurfaceSpec::new(
                "NeuronWhiteboard",
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                w,
                h,
            )) {
                Some(s) => s,
                None => return,
            };
            let hwnd = surf.hwnd();
            let px = surf.bits();
            let count = (w * h) as usize;
            std::ptr::write_bytes(px, 0, count);
            // The raster primitives below all take a bounds-checked PixelBuf/FieldBuf instead of
            // a loose (pointer, w, h) triple — dimensions travel WITH the pointer, so a
            // mismatched w/h can never reach them. `w`/`h` here are the SAME authoritative values
            // the DIB/fields were sized from (this closure never resizes them), so wrapping a
            // fresh view at each call site is just plumbing, not a borrow the loop needs to hold.
            // (already inside canvas_thread's outer `unsafe` block — no nested `unsafe {}` needed)
            macro_rules! pixbuf {
                ($p:expr) => {
                    &mut PixelBuf::from_raw_parts($p, w, h)
                };
            }
            macro_rules! pixview {
                ($p:expr) => {
                    PixelView::from_raw_parts($p, w, h)
                };
            }
            macro_rules! fieldbuf {
                ($p:expr) => {
                    &mut FieldBuf::from_raw_parts($p, w, h)
                };
            }
            macro_rules! fieldview {
                ($p:expr) => {
                    FieldView::from_raw_parts($p, w, h)
                };
            }

            let mut strokes: Vec<Stroke> = Vec::new();
            let mut live: Option<Stroke> = None;
            let mut seed_counter: u32 = 0x517C_C1B7; // per-stroke texture seeds (stable grain)
                                                     // ── the RIBBON FIELD: per-pixel min distance to the live stroke's SPINE ──
                                                     // The element the pipeline was missing: media alpha must come from distance to the
                                                     // stroke's polyline, not to each stamped disc (per-disc distance gave every brush a
                                                     // chain-of-dots look and put watercolour rims around every node). `dmin` is carved
                                                     // segment-by-segment (so it IS the polyline distance field where carved), `base` is
                                                     // the canvas as it was when the stroke began (joints re-resolve against it — a rim
                                                     // that becomes interior must RECEDE, which max-blend alone can't do), `live_bbox`
                                                     // tracks what to clean afterwards. Field invariant: f32::MAX everywhere outside a
                                                     // stroke render.
            let mut dmin: Vec<f32> = vec![f32::MAX; count];
            // the stroke-local LONGITUDE: arc length at the nearest spine point (valid wherever
            // dmin is carved) — the s of the media shaders' (s, d) UV.
            let mut smin: Vec<f32> = vec![0.0f32; count];
            let mut base: Vec<u32> = vec![0u32; count];
            let mut live_bbox: Option<Region> = None;
            let mut live_arc: f32 = 0.0; // cumulative arc length of the live stroke
            let mut selected: Vec<usize> = Vec::new();
            // the REDO stack: strokes lifted by undo, with the index to put each back at (so a
            // local selection-undo redoes in place). Any FRESH edit (new ink, delete, tidy,
            // nudge, recolor, clear) invalidates it — standard undo/redo.
            let mut redo: Vec<(usize, Stroke)> = Vec::new();
            // the LIVE command marks: what the in-flight gesture would act on, and in what color
            let mut marked: Vec<usize> = Vec::new();
            let mut mark_color: u32 = 0;
            let mut visible = false;
            let mut shown = false;
            // ── the time-fading OVERLAY: selection waveform + laser presentation layer ──
            // These animate, so the canvas can't stay purely event-driven while one is alive: we
            // tick a frame clock and re-stamp the overlay from `clean` (the strokes as last
            // composited) each frame, so nothing accumulates. `frame` is the animation phase;
            // `clean` is refreshed on every full repaint so the overlay always sits on current ink.
            let mut frame: u32 = 0;
            let mut clean: Vec<u32> = vec![0u32; count];
            let mut clean_valid = false;
            // the LASER trail: screen-space points, each born at an Instant so the tail fades.
            let mut laser: Vec<LaserRun> = Vec::new();
            let mut laser_live = false; // a hold is currently feeding the trail
                                        // comms PINGS dropped on the board (kind + birth) and the live reactionary-ping wheel.
            let mut pings: Vec<(i32, i32, PingKind, u32, Instant)> = Vec::new();
            let mut ping_wheel: Option<(i32, i32, i32, u32)> = None;
            let mut overlay_was = false; // an overlay drew last frame — gives it one clean exit
            let mut frame_panics = 0u32; // per-frame render panic streak (contain_frame throttle)

            let present = |dirty: bool| {
                if !dirty {
                    return;
                }
                let pos = POINT { x: vx, y: vy };
                let size = SIZE { cx: w, cy: h };
                surf.present(Some(pos), size, 255);
            };

            // event-driven: BLOCK on the channel (zero idle cost), batch what's queued, redraw once.
            'run: loop {
                // while an overlay animates (a selection, the laser trail, pings, or the live
                // ping-wheel) we wake on a ~30fps timeout to advance it; otherwise we BLOCK on the
                // channel (the documented zero-idle-cost rest). A timeout = an empty batch (pure
                // frame advance); a delivered command = the usual batch + redraw.
                let overlay_alive = !selected.is_empty()
                    || laser_live
                    || !laser.is_empty()
                    || !pings.is_empty()
                    || ping_wheel.is_some();
                let mut batch: Vec<Cmd> = Vec::new();
                if overlay_alive {
                    match rx.recv_timeout(std::time::Duration::from_millis(33)) {
                        Ok(c) => batch.push(c),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break 'run,
                    }
                } else {
                    match rx.recv() {
                        Ok(c) => batch.push(c),
                        Err(_) => break 'run,
                    }
                }
                let mut dirty = false;
                let mut full = false;
                while let Ok(c) = rx.try_recv() {
                    batch.push(c);
                }
                for cmd in batch {
                    // contain a per-command panic (pixel-pointer math on a bad size) so one bad
                    // command can't kill the whiteboard render loop for the run.
                    crate::worker::contain("neuron-whiteboard", || match cmd {
                        Cmd::Begin {
                            color,
                            width,
                            brush,
                            ghost,
                        } => {
                            seed_counter = seed_counter.wrapping_add(0x9E3779B9);
                            // snapshot the canvas: the stroke composites over THIS substrate so
                            // every joint re-resolves correctly as the field deepens.
                            std::ptr::copy_nonoverlapping(
                                px as *const u32,
                                base.as_mut_ptr(),
                                count,
                            );
                            live = Some(Stroke {
                                pts: Vec::new(),
                                color,
                                width,
                                brush,
                                ghost,
                                seed: seed_counter,
                            });
                            live_arc = 0.0;
                            visible = true;
                        }
                        Cmd::Pt(x, y) => {
                            if let Some(s) = live.as_mut() {
                                let p = (x - vx, y - vy);
                                if s.pts.last() != Some(&p) {
                                    // incremental: carve the fresh segment into the field, then
                                    // re-composite just its neighbourhood from TRUE distance.
                                    let from = s.pts.last().copied().unwrap_or(p);
                                    let reach = (s.width / 2.0).max(1.0) + 2.0;
                                    let seg_len = (((p.0 - from.0).pow(2) + (p.1 - from.1).pow(2))
                                        as f32)
                                        .sqrt();
                                    let r = carve(
                                        fieldbuf!(dmin.as_mut_ptr()),
                                        fieldbuf!(smin.as_mut_ptr()),
                                        from,
                                        p,
                                        reach,
                                        live_arc,
                                        seg_len,
                                    );
                                    live_arc += seg_len;
                                    composite(
                                        pixbuf!(px),
                                        Some(pixview!(base.as_ptr())),
                                        fieldview!(dmin.as_ptr()),
                                        fieldview!(smin.as_ptr()),
                                        r,
                                        s,
                                    );
                                    live_bbox = Some(union(live_bbox, r));
                                    s.pts.push(p);
                                    dirty = true;
                                }
                            }
                        }
                        Cmd::End => {
                            if let Some(s) = live.take() {
                                if !s.pts.is_empty() {
                                    strokes.push(s);
                                    redo.clear(); // a fresh stroke ends the redo line
                                    clean_valid = false; // the overlay substrate must re-snapshot
                                }
                            }
                            if let Some(r) = live_bbox.take() {
                                reset_region(fieldbuf!(dmin.as_mut_ptr()), r);
                            }
                        }
                        Cmd::EndAs(poly) => {
                            // the snap replacement: restore the canvas under the freehand, then
                            // render the perfect polyline fresh — no full-canvas repaint needed.
                            if let Some(mut s) = live.take() {
                                if let Some(r) = live_bbox.take() {
                                    restore_region(pixbuf!(px), pixview!(base.as_ptr()), r);
                                    reset_region(fieldbuf!(dmin.as_mut_ptr()), r);
                                }
                                s.pts = poly.iter().map(|(x, y)| (x - vx, y - vy)).collect();
                                if !s.pts.is_empty() {
                                    render_stroke(
                                        pixbuf!(px),
                                        fieldbuf!(dmin.as_mut_ptr()),
                                        fieldbuf!(smin.as_mut_ptr()),
                                        &s,
                                    );
                                    strokes.push(s);
                                    redo.clear(); // a fresh stroke ends the redo line
                                }
                                clean_valid = false; // strokes changed without a full repaint
                                dirty = true;
                            }
                        }
                        Cmd::DropLive => {
                            // the ghost vanishes EXACTLY: its region restores from the snapshot.
                            if live.take().is_some() {
                                if let Some(r) = live_bbox.take() {
                                    restore_region(pixbuf!(px), pixview!(base.as_ptr()), r);
                                    reset_region(fieldbuf!(dmin.as_mut_ptr()), r);
                                    clean_valid = false; // px region was rewritten; re-snapshot
                                    dirty = true;
                                }
                            }
                        }
                        Cmd::Mark(kind, path) => {
                            // LIVE feedback: light up what the in-flight command would act on.
                            let local: Vec<(f64, f64)> = path
                                .iter()
                                .map(|(x, y)| ((x - vx) as f64, (y - vy) as f64))
                                .collect();
                            let (set, color) = match kind {
                                Command::Scribble | Command::Slash => {
                                    (crossed_set(&strokes, &local, &selected), 0xFF453A)
                                }
                                Command::Tidy => {
                                    let mut t = crossed_set(&strokes, &local, &[]);
                                    t.retain(|i| !selected.contains(i)); // tidy ignores selection extension
                                    (t, 0x4AF2B0)
                                }
                                Command::Lasso => (lasso_set(&strokes, &local), 0xFFFFFF),
                                Command::None => (Vec::new(), 0),
                            };
                            if set != marked || color != mark_color {
                                marked = set;
                                mark_color = color;
                                full = true;
                            }
                        }
                        Cmd::DeleteCrossed(path, reply) => {
                            // the SCRIBBLE/SLASH: every stroke the command path crosses dies —
                            // and crossing a SELECTION takes all of it (one gesture, one intent).
                            let local: Vec<(f64, f64)> = path
                                .iter()
                                .map(|(x, y)| ((x - vx) as f64, (y - vy) as f64))
                                .collect();
                            let doomed = crossed_set(&strokes, &local, &selected);
                            if !doomed.is_empty() {
                                let n = doomed.len();
                                let mut idx = 0usize;
                                strokes.retain(|_| {
                                    let dead = doomed.contains(&idx);
                                    idx += 1;
                                    !dead
                                });
                                selected.clear();
                                marked.clear();
                                redo.clear(); // a delete ends the redo line
                                full = true;
                                let _ = reply.send(Cull::Deleted(n));
                            } else if !selected.is_empty() {
                                // a slash/scribble that crossed NOTHING, while a selection
                                // exists, UNSELECTS (the "a line over nothing unselects" model).
                                selected.clear();
                                full = true;
                                let _ = reply.send(Cull::Deselected);
                            } else {
                                let _ = reply.send(Cull::Nothing);
                            }
                        }
                        Cmd::TidyCrossed(path, reply) => {
                            // the WAVE: shape-snap what the codec reads, smooth the jitter on the
                            // rest — the retroactive, opt-in shape-set.
                            let local: Vec<(f64, f64)> = path
                                .iter()
                                .map(|(x, y)| ((x - vx) as f64, (y - vy) as f64))
                                .collect();
                            let targets = crossed_set(&strokes, &local, &[]);
                            for &i in &targets {
                                if let Some(s) = strokes.get_mut(i) {
                                    let body: Vec<neuron::glyph::C> = s
                                        .pts
                                        .iter()
                                        .map(|p| neuron::glyph::C::new(p.0 as f64, p.1 as f64))
                                        .collect();
                                    match neuron::shapes::snap(&body) {
                                        Some(shape) => {
                                            s.pts = neuron::shapes::polyline(&shape)
                                                .iter()
                                                .map(|(x, y)| (x.round() as i32, y.round() as i32))
                                                .collect();
                                        }
                                        None => smooth_stroke(s),
                                    }
                                }
                            }
                            let _ = reply.send(targets.len());
                            if !targets.is_empty() {
                                marked.clear();
                                redo.clear(); // a tidy ends the redo line
                                full = true;
                            }
                        }
                        Cmd::Select(poly) => {
                            // the LASSO: a stroke is taken when most of it lives inside the loop.
                            let local: Vec<(f64, f64)> = poly
                                .iter()
                                .map(|(x, y)| ((x - vx) as f64, (y - vy) as f64))
                                .collect();
                            selected = lasso_set(&strokes, &local);
                            marked.clear();
                            full = true;
                        }
                        Cmd::NudgeSelection(dx, dy) => {
                            if !selected.is_empty() && (dx != 0 || dy != 0) {
                                for &i in &selected {
                                    if let Some(s) = strokes.get_mut(i) {
                                        for p in s.pts.iter_mut() {
                                            p.0 += dx;
                                            p.1 += dy;
                                        }
                                    }
                                }
                                redo.clear(); // moving the selection is a fresh edit
                                full = true;
                            }
                        }
                        Cmd::RecolorSelection(c) => {
                            for &i in &selected {
                                if let Some(s) = strokes.get_mut(i) {
                                    s.color = c;
                                }
                            }
                            if !selected.is_empty() {
                                redo.clear();
                                full = true;
                            }
                        }
                        Cmd::RebrushSelection(brush) => {
                            // the palette changed the pen's BRUSH while ink is selected — every
                            // selected stroke takes the new medium in place, seamlessly.
                            for &i in &selected {
                                if let Some(s) = strokes.get_mut(i) {
                                    s.brush = brush;
                                }
                            }
                            if !selected.is_empty() {
                                redo.clear();
                                full = true;
                            }
                        }
                        Cmd::ResizeSelection(width) => {
                            for &i in &selected {
                                if let Some(s) = strokes.get_mut(i) {
                                    s.width = width;
                                }
                            }
                            if !selected.is_empty() {
                                redo.clear();
                                full = true;
                            }
                        }
                        Cmd::Deselect => {
                            if !selected.is_empty() {
                                selected.clear();
                                full = true;
                            }
                        }
                        Cmd::DeselectAsk(reply) => {
                            let had = !selected.is_empty();
                            if had {
                                selected.clear();
                                full = true;
                            }
                            let _ = reply.send(had);
                        }
                        Cmd::HitSelection(x, y, reply) => {
                            let p = ((x - vx) as f64, (y - vy) as f64);
                            let hit = selected.iter().any(|&i| {
                                strokes
                                    .get(i)
                                    .map(|s| stroke_hit_wide(s, p, 28.0))
                                    .unwrap_or(false)
                            });
                            let _ = reply.send(hit);
                        }
                        Cmd::Undo => {
                            // LOCAL undo while a selection exists: the most recent stroke IN the
                            // locale goes, the selection (minus it) stays. Global undo otherwise.
                            // Either way the lifted stroke + its index ride the redo stack.
                            if let Some(&last) = selected.iter().max() {
                                let s = strokes.remove(last);
                                redo.push((last, s));
                                selected.retain(|&i| i != last);
                                // indices above the removed slot shift down by one
                                for i in selected.iter_mut() {
                                    if *i > last {
                                        *i -= 1;
                                    }
                                }
                                full = true;
                            } else if let Some(s) = strokes.pop() {
                                redo.push((strokes.len(), s));
                                full = true;
                            }
                        }
                        Cmd::Redo => {
                            // put the last undone stroke back where it was; selection indices at or
                            // past it shift up so a held selection stays consistent.
                            if let Some((at, s)) = redo.pop() {
                                let at = at.min(strokes.len());
                                strokes.insert(at, s);
                                for i in selected.iter_mut() {
                                    if *i >= at {
                                        *i += 1;
                                    }
                                }
                                full = true;
                            }
                        }
                        Cmd::Clear => {
                            // a clear also sweeps the laser layer (the presenter's "reset the
                            // stage") — pings + trail + wheel go with the ink.
                            let had_overlay = !laser.is_empty() || !pings.is_empty();
                            laser.clear();
                            pings.clear();
                            ping_wheel = None;
                            laser_live = false;
                            if !strokes.is_empty() || live.is_some() || had_overlay {
                                strokes.clear();
                                selected.clear();
                                marked.clear();
                                mark_color = 0;
                                redo.clear();
                                live = None;
                                // ITEM 7 — reset the RIBBON FIELD too. The fresh stroke after a
                                // clear composites from `dmin`/`live_bbox`; leaving stale carved
                                // distances (or a dangling live region) made the old ink FLASH on
                                // first touch. Restore the field invariant (f32::MAX everywhere)
                                // so the next stroke reads only its own distances.
                                if let Some(r) = live_bbox.take() {
                                    reset_region(fieldbuf!(dmin.as_mut_ptr()), r);
                                }
                                dmin.fill(f32::MAX);
                                full = true;
                            }
                        }
                        Cmd::Visible(v) => {
                            visible = v;
                            dirty = true;
                        }
                        Cmd::LaserBegin {
                            color,
                            width,
                            brush,
                        } => {
                            // a new RUN carrying its pen — the trail uses the CURRENT material/size,
                            // and runs never connect across a lift (each renders on its own).
                            laser.push(LaserRun {
                                pen: super::Pen {
                                    brush,
                                    color,
                                    width,
                                },
                                pts: Vec::new(),
                            });
                            // a backstop on live runs (faded ones are pruned each frame anyway)
                            if laser.len() > 64 {
                                let drop = laser.len() - 64;
                                laser.drain(0..drop);
                            }
                            laser_live = true;
                            visible = true;
                            dirty = true;
                        }
                        Cmd::LaserPt(x, y) => {
                            // extend the CURRENT run; the new head is born NOW so it shines brightest
                            // and ages back along the tail. Cap one run so a long sweep can't grow
                            // unbounded (old points have faded to nothing well before this anyway).
                            if let Some(run) = laser.last_mut() {
                                run.pts.push((x - vx, y - vy, Instant::now()));
                                if run.pts.len() > 512 {
                                    let drop = run.pts.len() - 512;
                                    run.pts.drain(0..drop);
                                }
                            }
                            visible = true;
                            dirty = true;
                        }
                        Cmd::LaserLift => {
                            laser_live = false; // stop extending; the tail fades on its own
                        }
                        Cmd::PingWheel(w) => {
                            ping_wheel = w;
                            if w.is_some() {
                                visible = true;
                            }
                            dirty = true;
                        }
                        Cmd::Ping(x, y, kind, color) => {
                            pings.push((x - vx, y - vy, kind, color, Instant::now()));
                            visible = true;
                            dirty = true;
                        }
                    });
                }
                // the per-frame render — full repaint + the animated overlay pass + present — is
                // pixel/geometry work run every tick (the likeliest panic surface), contained on its
                // own hot lane so a panicked frame is a dropped frame (repainted next tick) instead
                // of a permanently dead whiteboard worker.
                crate::worker::contain_frame("neuron-whiteboard", &mut frame_panics, || {
                if full {
                    std::ptr::write_bytes(px, 0, count);
                    // the live stroke's carve must NOT pollute the committed strokes' shared distance
                    // field at crossings (the phantom-ink-at-the-intersection bug: a target's own ink
                    // leaks along the command path) — clear its region so the committed strokes carve
                    // into a clean field; the live stroke is re-carved fresh just below.
                    if let Some(r) = live_bbox {
                        reset_region(fieldbuf!(dmin.as_mut_ptr()), r);
                    }
                    for (i, s) in strokes.iter().enumerate() {
                        // selected ink wears the PRISM — white diffusing into a spectral fringe,
                        // mis-registered like chromatic aberration: two hue passes offset to
                        // opposite sides of the stroke, a white heart on top. Subtle, not loud —
                        // the ink itself is untouched.
                        if selected.contains(&i) {
                            let n = s.pts.len().max(2);
                            for (side, phase) in [(1.0f32, 0.0f32), (-1.0, 150.0)] {
                                let mut prev: Option<(i32, i32)> = None;
                                for (j, &p) in s.pts.iter().enumerate() {
                                    let from = prev.unwrap_or(p);
                                    // perpendicular offset = the aberration's mis-registration
                                    let (dx, dy) = ((p.0 - from.0) as f32, (p.1 - from.1) as f32);
                                    let len = (dx * dx + dy * dy).sqrt().max(1.0);
                                    let (ox, oy) = (
                                        (-dy / len * 1.6 * side).round() as i32,
                                        (dx / len * 1.6 * side).round() as i32,
                                    );
                                    let hue =
                                        (j as f32 / n as f32) * 300.0 + phase + (i as f32 * 47.0);
                                    let c = hsv(hue, 0.72, 1.0);
                                    stamp_segment(
                                        pixbuf!(px),
                                        (from.0 + ox, from.1 + oy),
                                        (p.0 + ox, p.1 + oy),
                                        c,
                                        s.width + 3.5,
                                        A_GLOW,
                                    );
                                    prev = Some(p);
                                }
                            }
                            // the white heart the spectrum diffuses out of
                            let mut prev: Option<(i32, i32)> = None;
                            for &p in &s.pts {
                                stamp_segment(
                                    pixbuf!(px),
                                    prev.unwrap_or(p),
                                    p,
                                    0xFFFFFF,
                                    s.width + 1.5,
                                    A_GLOW + 1,
                                );
                                prev = Some(p);
                            }
                        }
                        // marked ink (a live command's would-be targets) glows its verdict color
                        if marked.contains(&i) {
                            let mut prev: Option<(i32, i32)> = None;
                            for &p in &s.pts {
                                stamp_segment(
                                    pixbuf!(px),
                                    prev.unwrap_or(p),
                                    p,
                                    mark_color,
                                    s.width + 5.0,
                                    A_GLOW + 2,
                                );
                                prev = Some(p);
                            }
                        }
                        render_stroke(
                            pixbuf!(px),
                            fieldbuf!(dmin.as_mut_ptr()),
                            fieldbuf!(smin.as_mut_ptr()),
                            s,
                        );
                    }
                    // a live stroke survives the repaint: re-base on the fresh canvas, RE-CARVE its
                    // whole path into the now-clean field (cleared above, so it never polluted the
                    // committed strokes at a crossing), then composite it over the fresh base.
                    if let Some(s) = live.as_ref() {
                        std::ptr::copy_nonoverlapping(px as *const u32, base.as_mut_ptr(), count);
                        if !s.pts.is_empty() {
                            let reach = (s.width / 2.0).max(1.0) + 2.0;
                            let mut bb: Option<Region> = None;
                            let mut arc = 0.0f32;
                            if s.pts.len() == 1 {
                                bb = Some(carve(
                                    fieldbuf!(dmin.as_mut_ptr()),
                                    fieldbuf!(smin.as_mut_ptr()),
                                    s.pts[0],
                                    s.pts[0],
                                    reach,
                                    0.0,
                                    0.0,
                                ));
                            }
                            for sw in s.pts.windows(2) {
                                let seg_len = (((sw[1].0 - sw[0].0).pow(2)
                                    + (sw[1].1 - sw[0].1).pow(2))
                                    as f32)
                                    .sqrt();
                                bb = Some(union(
                                    bb,
                                    carve(
                                        fieldbuf!(dmin.as_mut_ptr()),
                                        fieldbuf!(smin.as_mut_ptr()),
                                        sw[0],
                                        sw[1],
                                        reach,
                                        arc,
                                        seg_len,
                                    ),
                                ));
                                arc += seg_len;
                            }
                            if let Some(r) = bb {
                                composite(
                                    pixbuf!(px),
                                    Some(pixview!(base.as_ptr())),
                                    fieldview!(dmin.as_ptr()),
                                    fieldview!(smin.as_ptr()),
                                    r,
                                    s,
                                );
                                live_bbox = Some(r);
                            }
                        }
                    }
                    // snapshot the freshly-composited canvas: the time-fading OVERLAY re-stamps
                    // onto THIS each frame (so nothing accumulates and the strokes never repaint
                    // just because a ping pulsed). A live stroke makes the snapshot provisional —
                    // the overlay layer doesn't run while inking anyway.
                    std::ptr::copy_nonoverlapping(px as *const u32, clean.as_mut_ptr(), count);
                    clean_valid = true;
                    dirty = true;
                }
                // ── the OVERLAY pass: selection waveform + laser trail + pings + the live ping
                // wheel. It restores the canvas from `clean` and stamps the animated layer on top,
                // every present — so it animates without ever repainting the kept ink. Pruned by
                // age (the laser tail and the ping blooms fade to nothing, then release the clock).
                {
                    let now = Instant::now();
                    laser.retain(|run| {
                        run.pts.last().is_some_and(|&(_, _, born)| {
                            now.duration_since(born).as_millis() < LASER_MS
                        })
                    });
                    pings.retain(|&(_, _, _, _, born)| {
                        now.duration_since(born).as_millis() < PING_MS
                    });
                    let overlay_now = !selected.is_empty()
                        || laser_live
                        || !laser.is_empty()
                        || !pings.is_empty()
                        || ping_wheel.is_some();
                    // NEVER restore-from-clean while a stroke is live (clean lacks the in-progress
                    // ink — stamping over it would erase what's being drawn). The overlay resumes
                    // the moment the stroke commits (a full repaint refreshes clean first).
                    if (overlay_now || overlay_was) && live.is_none() {
                        // need a clean substrate to stamp onto; a full pass above made one, else
                        // build it now (overlay appeared on a tick with no stroke change). The
                        // selection PRISM is only ever absent here when there is no selection —
                        // every selection-state change funnels through a `full` repaint, which
                        // draws the prism INTO clean — so the laser/ping rebuild needs bare strokes.
                        if !clean_valid {
                            std::ptr::write_bytes(px, 0, count);
                            for s in strokes.iter() {
                                render_stroke(
                                    pixbuf!(px),
                                    fieldbuf!(dmin.as_mut_ptr()),
                                    fieldbuf!(smin.as_mut_ptr()),
                                    s,
                                );
                            }
                            std::ptr::copy_nonoverlapping(
                                px as *const u32,
                                clean.as_mut_ptr(),
                                count,
                            );
                            clean_valid = true;
                        } else {
                            std::ptr::copy_nonoverlapping(clean.as_ptr(), px, count);
                        }
                        frame = frame.wrapping_add(1);
                        if !selected.is_empty() {
                            selection_wave(pixbuf!(px), &strokes, &selected, frame);
                        }
                        draw_laser(pixbuf!(px), &laser, now);
                        for &(x, y, kind, color, born) in &pings {
                            draw_ping(
                                pixbuf!(px),
                                x,
                                y,
                                kind,
                                color,
                                now.duration_since(born).as_millis() as f32,
                            );
                        }
                        if let Some((cx, cy, sect, color)) = ping_wheel {
                            draw_ping_wheel(pixbuf!(px), cx - vx, cy - vy, sect, color, frame);
                        }
                        dirty = true;
                    }
                    overlay_was = overlay_now;
                }
                // visibility: the window exists only while it has something to say
                let want = visible
                    && (!strokes.is_empty()
                        || live.is_some()
                        || !laser.is_empty()
                        || !pings.is_empty()
                        || ping_wheel.is_some());
                if want && !shown {
                    SetWindowPos(hwnd, HWND_TOPMOST, vx, vy, w, h, SWP_NOACTIVATE);
                    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                    shown = true;
                } else if !want && shown {
                    ShowWindow(hwnd, SW_HIDE);
                    shown = false;
                }
                if shown {
                    present(dirty || full);
                }
                });
                // the message pump is NOT contained: a panic across the `extern "system"`
                // window-proc ABI aborts the process, so catch_unwind here would be dead code.
                // pump so the window stays healthy
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                if false {
                    break 'run; // the channel closing ends the loop; the label documents intent
                }
            }
            // surf's Drop restores the bitmap, deletes the DIB + mem DC, releases the screen DC,
            // and destroys the window — exactly the old inline teardown order.
            drop(surf);
        }
    }

    /// Indices of strokes the command path CROSSES — segment-vs-segment (a fast scribble's
    /// sparse samples must not slip between a stroke's points; the old point test missed ~20%).
    /// Crossing any stroke of `selected` extends the set to the WHOLE selection: one gesture,
    /// one intent — you scribble a selection, all of it answers.
    fn crossed_set(strokes: &[Stroke], path: &[(f64, f64)], selected: &[usize]) -> Vec<usize> {
        let mut set: Vec<usize> = strokes
            .iter()
            .enumerate()
            .filter(|(_, s)| path_crosses(s, path))
            .map(|(i, _)| i)
            .collect();
        if !selected.is_empty() && set.iter().any(|i| selected.contains(i)) {
            for &i in selected {
                if !set.contains(&i) {
                    set.push(i);
                }
            }
            set.sort_unstable();
        }
        set
    }

    /// Indices of strokes the lasso takes — majority of points inside the loop.
    fn lasso_set(strokes: &[Stroke], poly: &[(f64, f64)]) -> Vec<usize> {
        strokes
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let inside = s
                    .pts
                    .iter()
                    .filter(|p| in_poly((p.0 as f64, p.1 as f64), poly))
                    .count();
                inside * 2 > s.pts.len() // majority rule
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Does the command path pass within erase-reach of this stroke? Segment-vs-segment with a
    /// cheap inflated-bbox rejection first (live marking runs this every ~90ms).
    fn path_crosses(s: &Stroke, path: &[(f64, f64)]) -> bool {
        if path.is_empty() || s.pts.is_empty() {
            return false;
        }
        let reach = ERASE_REACH + s.width as f64 / 2.0;
        // inflated bbox rejection
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in &s.pts {
            x0 = x0.min(p.0 as f64);
            y0 = y0.min(p.1 as f64);
            x1 = x1.max(p.0 as f64);
            y1 = y1.max(p.1 as f64);
        }
        if !path.iter().any(|p| {
            p.0 >= x0 - reach && p.0 <= x1 + reach && p.1 >= y0 - reach && p.1 <= y1 + reach
        }) {
            return false;
        }
        if path.len() == 1 || s.pts.len() == 1 {
            let pt_hit = |q: (f64, f64)| {
                if s.pts.len() == 1 {
                    let d = ((s.pts[0].0 as f64 - q.0).powi(2) + (s.pts[0].1 as f64 - q.1).powi(2))
                        .sqrt();
                    d <= reach
                } else {
                    stroke_hit(s, q)
                }
            };
            return path.iter().any(|q| pt_hit(*q));
        }
        path.windows(2).any(|pw| {
            s.pts.windows(2).any(|sw| {
                seg_seg_dist(
                    pw[0],
                    pw[1],
                    (sw[0].0 as f64, sw[0].1 as f64),
                    (sw[1].0 as f64, sw[1].1 as f64),
                ) <= reach
            })
        })
    }

    /// Minimum distance between two segments (0 when they intersect).
    fn seg_seg_dist(a1: (f64, f64), a2: (f64, f64), b1: (f64, f64), b2: (f64, f64)) -> f64 {
        // orientation-based intersection test
        let orient = |p: (f64, f64), q: (f64, f64), r: (f64, f64)| {
            (q.0 - p.0) * (r.1 - p.1) - (q.1 - p.1) * (r.0 - p.0)
        };
        let (d1, d2) = (orient(b1, b2, a1), orient(b1, b2, a2));
        let (d3, d4) = (orient(a1, a2, b1), orient(a1, a2, b2));
        if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0))
            && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
        {
            return 0.0;
        }
        seg_dist(a1, b1, b2)
            .min(seg_dist(a2, b1, b2))
            .min(seg_dist(b1, a1, a2))
            .min(seg_dist(b2, a1, a2))
    }

    /// Light in-place jitter smoothing for TIDY's non-shape strokes (window-5 moving average,
    /// endpoints pinned so the stroke never shrinks away from what it touched).
    fn smooth_stroke(s: &mut Stroke) {
        if s.pts.len() < 5 {
            return;
        }
        let src = s.pts.clone();
        for i in 2..src.len() - 2 {
            let (mut ax, mut ay) = (0i64, 0i64);
            for p in &src[i - 2..=i + 2] {
                ax += p.0 as i64;
                ay += p.1 as i64;
            }
            s.pts[i] = ((ax / 5) as i32, (ay / 5) as i32);
        }
    }

    /// HSV → 0xRRGGBB (the prism's little rainbow; s,v in 0..=1, h in degrees).
    fn hsv(h: f32, s: f32, v: f32) -> u32 {
        let h = h.rem_euclid(360.0) / 60.0;
        let c = v * s;
        let x = c * (1.0 - (h % 2.0 - 1.0).abs());
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        let q = |f: f32| (((f + m) * 255.0).round() as u32).min(255);
        (q(r) << 16) | (q(g) << 8) | q(b)
    }

    /// Point-in-polygon (ray cast) — the lasso's containment test.
    pub(super) fn in_poly(p: (f64, f64), poly: &[(f64, f64)]) -> bool {
        let mut inside = false;
        let n = poly.len();
        if n < 3 {
            return false;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, yi) = poly[i];
            let (xj, yj) = poly[j];
            if ((yi > p.1) != (yj > p.1))
                && (p.0 < (xj - xi) * (p.1 - yi) / (yj - yi + f64::EPSILON) + xi)
            {
                inside = !inside;
            }
            j = i;
        }
        inside
    }

    /// `stroke_hit` with a custom reach (the selection's grab halo is friendlier than an eraser).
    fn stroke_hit_wide(s: &Stroke, p: (f64, f64), reach: f64) -> bool {
        let reach = reach + s.width as f64 / 2.0;
        if s.pts.len() == 1 {
            let d = ((s.pts[0].0 as f64 - p.0).powi(2) + (s.pts[0].1 as f64 - p.1).powi(2)).sqrt();
            return d <= reach;
        }
        s.pts.windows(2).any(|w| {
            let (a, b) = (w[0], w[1]);
            seg_dist(p, (a.0 as f64, a.1 as f64), (b.0 as f64, b.1 as f64)) <= reach
        })
    }

    /// Does the eraser point come within reach of this stroke's polyline?
    fn stroke_hit(s: &Stroke, p: (f64, f64)) -> bool {
        let reach = ERASE_REACH + s.width as f64 / 2.0;
        if s.pts.len() == 1 {
            let d = ((s.pts[0].0 as f64 - p.0).powi(2) + (s.pts[0].1 as f64 - p.1).powi(2)).sqrt();
            return d <= reach;
        }
        s.pts.windows(2).any(|w| {
            let (a, b) = (w[0], w[1]);
            seg_dist(p, (a.0 as f64, a.1 as f64), (b.0 as f64, b.1 as f64)) <= reach
        })
    }

    /// Distance from point to segment.
    fn seg_dist(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
        let (vx, vy) = (b.0 - a.0, b.1 - a.1);
        let len2 = vx * vx + vy * vy;
        let t = if len2 <= 1e-9 {
            0.0
        } else {
            (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / len2).clamp(0.0, 1.0)
        };
        let (cx, cy) = (a.0 + vx * t, a.1 + vy * t);
        ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
    }

    /// Alpha class for the glow underlays ([`stamp_segment`]): translucent, max-blended so
    /// overlapping never darkens. (Ink alphas live in the brushes — [`brush_alpha`].)
    const A_GLOW: u32 = 120;

    /// A pixel region (x0, y0, x1, y1), inclusive.
    pub(super) type Region = (i32, i32, i32, i32);

    fn union(a: Option<Region>, b: Region) -> Region {
        match a {
            Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
            None => b,
        }
    }

    /// Distance from a pixel to a segment plus the projection parameter t (0..1 along it) —
    /// the field's metric AND the stroke-local longitude.
    fn seg_proj(p: (f32, f32), a: (i32, i32), b: (i32, i32)) -> (f32, f32) {
        let (ax, ay) = (a.0 as f32, a.1 as f32);
        let (vx, vy) = (b.0 as f32 - ax, b.1 as f32 - ay);
        let len2 = vx * vx + vy * vy;
        let t = if len2 <= 1e-9 {
            0.0
        } else {
            (((p.0 - ax) * vx + (p.1 - ay) * vy) / len2).clamp(0.0, 1.0)
        };
        let (cx, cy) = (ax + vx * t, ay + vy * t);
        (
            ((p.0 - cx) * (p.0 - cx) + (p.1 - cy) * (p.1 - cy)).sqrt(),
            t,
        )
    }

    fn seg_dist_f(p: (f32, f32), a: (i32, i32), b: (i32, i32)) -> f32 {
        seg_proj(p, a, b).0
    }

    /// CARVE one segment into the field: `dmin = min(dmin, dist)` over the capsule — and where
    /// this segment wins, `smin` takes the ARC LENGTH at the projected spine point. (dmin, smin)
    /// together are the stroke-local UV every medium shades in: `d` across the stroke, `s` along
    /// it — the coordinate grain rides, edges waver by, ink loads dry over. Returns the region.
    /// A raster primitive's natural arity: the buffers carry their own w/h now (no loose `w`/`h`
    /// params to go stale against them), so the count here is just the real varyings.
    #[allow(clippy::too_many_arguments)]
    fn carve(
        dmin: &mut FieldBuf,
        smin: &mut FieldBuf,
        a: (i32, i32),
        b: (i32, i32),
        reach: f32,
        s0: f32,
        seg_len: f32,
    ) -> Region {
        let (w, h) = (dmin.w(), dmin.h());
        if w <= 0 || h <= 0 {
            return (0, 0, -1, -1); // an empty region: every `r.1..=r.3`/`r.0..=r.2` iterates zero times
        }
        let ri = reach.ceil() as i32 + 1;
        // `saturating_*`: a pathological/off-screen coordinate (a stroke point near i32::MIN/MAX)
        // must clip at the field edge, not wrap or panic on the subtract/add overflowing.
        let x0 = (a.0.min(b.0).saturating_sub(ri)).clamp(0, w - 1);
        let x1 = (a.0.max(b.0).saturating_add(ri)).clamp(0, w - 1);
        let y0 = (a.1.min(b.1).saturating_sub(ri)).clamp(0, h - 1);
        let y1 = (a.1.max(b.1).saturating_add(ri)).clamp(0, h - 1);
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                let (d, t) = seg_proj((xx as f32, yy as f32), a, b);
                // SAFETY: (xx, yy) ranges over x0..=x1, y0..=y1, which were just clamped against
                // this SAME buffer's dmin.w()/dmin.h() above — not a separately-carried w/h.
                unsafe {
                    if d < dmin.get_unchecked(xx, yy) {
                        dmin.put_unchecked(xx, yy, d);
                        smin.put_unchecked(xx, yy, s0 + t * seg_len);
                    }
                }
            }
        }
        (x0, y0, x1, y1)
    }

    /// Return a carved region to the field's resting state (f32::MAX).
    fn reset_region(dmin: &mut FieldBuf, r: Region) {
        for yy in r.1..=r.3 {
            if let Some(row) = dmin.row_range_mut(yy, r.0, r.2) {
                row.fill(f32::MAX);
            }
        }
    }

    /// Restore a region of the canvas from the pre-stroke snapshot (ghost erase, shape-set swap).
    fn restore_region(px: &mut PixelBuf, base: PixelView, r: Region) {
        for yy in r.1..=r.3 {
            for xx in r.0..=r.2 {
                // `base` may legitimately be smaller/differently-shaped in a future refactor; go
                // through the checked `get` here (restore isn't the hot per-frame path stamp/
                // composite are) so a mismatch degrades to "skip that pixel", not corruption.
                if let Some(v) = base.get(xx, yy) {
                    px.put(xx, yy, v);
                }
            }
        }
    }

    /// COMPOSITE a region of ONE stroke from the distance field onto the canvas. With `base`
    /// (live strokes) every pixel re-resolves over the pre-stroke substrate — so when a new
    /// segment turns yesterday's watercolour RIM into today's interior, the rim actually
    /// recedes (pure max-blend can only ever get heavier; this is the joint-splotch fix).
    fn composite(
        px: &mut PixelBuf,
        base: Option<PixelView>,
        dmin: FieldView,
        smin: FieldView,
        r: Region,
        s: &Stroke,
    ) {
        {
            let rad = (s.width / 2.0).max(1.0);
            let mt = crate::weave::seconds();
            // The spellweaving pen pours the LIVE material — a per-stroke clone of whatever surface/knobs
            // the user has chosen in settings, with its ACCENT overridden to the chosen swatch, fed by
            // THIS stroke's signed-distance field (the same kind of field the overlay's glow buffer is;
            // here it's analytic). Density, gradient, dispersion taps and facet all come from the SDF, so
            // the whiteboard ink IS the spellweaving material, not a hardcoded imitation of it.
            let material_mat = if s.brush == Brush::DirectedIntent && !s.ghost {
                let mut m = crate::weave::live_material();
                let c = s.color;
                m.accent = (
                    ((c >> 16) & 0xFF) as f32 / 255.0,
                    ((c >> 8) & 0xFF) as f32 / 255.0,
                    (c & 0xFF) as f32 / 255.0,
                );
                m.accent_hue = crate::weave::hue_u32(c);
                Some(m)
            } else {
                None
            };
            for yy in r.1..=r.3 {
                for xx in r.0..=r.2 {
                    // SAFETY: (xx, yy) ranges over r, which every caller derives from carve()'s
                    // return — itself clamped against dmin's own w()/h(). `px`/`base` share those
                    // same dimensions by construction (one DIB, one snapshot of it), so the same
                    // range is in-bounds for all three buffers.
                    let (under, d, sa) = unsafe {
                        let under = match base {
                            Some(b) => b.get_unchecked(xx, yy),
                            None => px.get_unchecked(xx, yy),
                        };
                        (under, dmin.get_unchecked(xx, yy), smin.get_unchecked(xx, yy))
                    };
                    let (col, a) = match &material_mat {
                        Some(m) if d <= rad + 2.0 => {
                            // surface normal = the gradient of the distance field (clamped neighbours)
                            // — only its DIRECTION matters (it picks the facet); |∇d|≈1 everywhere.
                            let (dl, dr2, du, dd2) = unsafe {
                                (
                                    if xx > r.0 { dmin.get_unchecked(xx - 1, yy) } else { d },
                                    if xx < r.2 { dmin.get_unchecked(xx + 1, yy) } else { d },
                                    if yy > r.1 { dmin.get_unchecked(xx, yy - 1) } else { d },
                                    if yy < r.3 { dmin.get_unchecked(xx, yy + 1) } else { d },
                                )
                            };
                            material_core(
                                d,
                                rad,
                                (dr2 - dl) * 0.5,
                                (dd2 - du) * 0.5,
                                xx as f32,
                                yy as f32,
                                mt,
                                m,
                            )
                        }
                        _ => {
                            let a = if d <= rad + 2.0 {
                                brush_alpha(s.brush, s.ghost, s.seed, d, rad, sa, xx, yy)
                            } else {
                                0
                            };
                            (s.color, a)
                        }
                    };
                    // a command GHOST is a transient indicator, never ink: it draws ONLY on empty
                    // canvas, never over a kept stroke — so crossing a (grainy) stroke can't leave a
                    // white speck at the intersection that reads as a real mark (max-blend would let the
                    // ghost's 88 alpha beat a textured stroke's sub-88 grain pixels).
                    let out = if a == 0 || (s.ghost && (under >> 24) != 0) {
                        under
                    } else {
                        blend_max(under, col, a)
                    };
                    unsafe { px.put_unchecked(xx, yy, out) };
                }
            }
        }
    }

    /// The shared ICHOR ink core (used by the canvas SDF path AND the dock preview). Pours
    /// `ichor::shade` the SAME WAY the overlay's cast path does, so the ink IS the comet material:
    /// the field is the stroke's DENSITY (1 at the spine → 0 at the rim), the dispersion samples
    /// are REAL field taps along the FACET direction (a step of `dispersion` along `(ux,uy)`
    /// changes the SDF distance by `(ux,uy)·n̂`, so `dens` of that is the honest tap — not a flat
    /// radius falloff), and `grad` is the TRUE edge strength read straight off the tap disagreement
    /// (≈0 in the flat core, climbing at the rim where the density ramp clamps — so the accent rim
    /// self-concentrates at the edge instead of being faked from proximity). Colour is toned the
    /// cast way (channels clamped, NOT divided by luminance — the divide bleached the fire/rim into
    /// white), alpha-premultiplied so max-blend reproduces the additive glow + prismatic fire.
    fn material_core(
        d: f32,
        rad: f32,
        gx: f32,
        gy: f32,
        x: f32,
        y: f32,
        t: f32,
        m: &crate::weave::Material,
    ) -> (u32, u32) {
        let dens = |dd: f32| (1.0 - dd / rad).clamp(0.0, 1.0);
        let dn = (d / rad).clamp(0.0, 1.0);
        let here = dens(d);
        // taps ALONG the facet, like the overlay samples the glow: project the dispersion step
        // onto the surface normal (|∇d|≈1) so the tap walks the real field, then read its density.
        let (ux, uy, facet_u) = crate::weave::facet(gx, gy, m.facets);
        let gl = (gx * gx + gy * gy).sqrt().max(1e-4);
        let proj = (ux * gx + uy * gy) / gl * m.dispersion; // signed distance the tap travels
        let densb = dens(d - proj); // toward the spine (denser)
        let densr = dens(d + proj); // toward the rim (thinner)
                                    // the white-hot core: a single soft falloff — shade SQUARES heat internally, so feeding it
                                    // pre-squared (the old h*h) pinched the star to a needle. one power here, one there.
        let heat = ((0.55 - dn) / 0.55).max(0.0);
        // TRUE edge strength: how much the field disagrees across the taps (the same quantity the
        // fire rides) — flat interior ⇒ ~0 ⇒ no rim; the rim flank ⇒ rises ⇒ the accent edge lands.
        let grad = (here - densr).abs() + (densb - here).abs();
        // PASSTHROUGH: shade through the LIVE spellweaving material (whatever surface/knobs the user
        // chose) — the whiteboard ink IS the cast's material, not a hardcoded glass imitation.
        let px = crate::weave::Px {
            d: here,
            dr: densr,
            db: densb,
            gx,
            gy,
            grad,
            facet_u,
            heat,
            x,
            y,
            t,
        };
        let (cr, cg, cb, lum) = crate::weave::shade_surface(&px, m);
        let cov = (rad + 0.5 - d).clamp(0.0, 1.0); // 1px AA at the true rim
        // sRGB-encode (sqrt ≈ gamma 2.0) like EVERY other consumer of the material — the overlay
        // cast, the gallery swatch, the curtain static. Un-encoded linear crushed the dim end
        // (accent halos, breeze trails) so ink read as white cores on black while the gallery
        // showed the same material in full colour. Cores still clip to white; text stays crisp.
        let p = |v: f32| (v.min(1.0).sqrt() * 255.0) as u32;
        // toned the cast way: keep the colour, premultiply by coverage — no luminance divide.
        let col = (p(cr) << 16) | (p(cg) << 8) | p(cb);
        let a = ((lum * cov * 255.0).min(255.0)) as u32;
        (col, a)
    }

    /// The max-blend write rule, as a pure function: the heavier ink wins; equal-weight
    /// translucents recolour (last wins) so overlapping media never self-darken.
    fn blend_max(under: u32, color: u32, a: u32) -> u32 {
        let r = ((color >> 16) & 0xFF) * a / 255;
        let g = ((color >> 8) & 0xFF) * a / 255;
        let b = (color & 0xFF) * a / 255;
        let argb = (a << 24) | (r << 16) | (g << 8) | b;
        let ua = under >> 24;
        if ua < a || (under != argb && ua == a && a < 200) {
            argb
        } else {
            under
        }
    }

    /// Smooth 2-D value noise in [0, 1) — bilinear smoothstep over the integer hash lattice.
    /// This is the media engine's texture unit: sampled in STROKE SPACE (s along, d across) a
    /// grain rides the stroke like Procreate's "moving" grain; sampled in SCREEN SPACE it's
    /// paper-locked like their "texturized" grain. A few hashes per call, no tables.
    fn vnoise(x: f32, y: f32, seed: u32) -> f32 {
        let xi = x.floor();
        let yi = y.floor();
        let fx = x - xi;
        let fy = y - yi;
        let (x0, y0) = (xi as i32, yi as i32);
        let u = fx * fx * (3.0 - 2.0 * fx);
        let v = fy * fy * (3.0 - 2.0 * fy);
        let a = bnoise(x0, y0, seed);
        let b = bnoise(x0 + 1, y0, seed);
        let c = bnoise(x0, y0 + 1, seed);
        let e = bnoise(x0 + 1, y0 + 1, seed);
        let top = a + (b - a) * u;
        let bot = c + (e - c) * u;
        top + (bot - top) * v
    }

    /// The MEDIA — a tiny fragment shader per medium, evaluated in the stroke's own UV:
    /// `d` = distance across (from the spine), `s` = arc length along, plus screen (x, y) for
    /// paper-locked grain. Everything is closed-form + hash noise: no textures, no allocation,
    /// stable per stroke seed (grain never shimmers across repaints). Shared with the palette's
    /// live previews, so the dock shows the real ink.
    #[allow(clippy::too_many_arguments)] // a fragment shader's natural arity (its varyings)
    pub(super) fn brush_alpha(
        brush: Brush,
        ghost: bool,
        seed: u32,
        d: f32,
        rad: f32,
        s: f32,
        x: i32,
        y: i32,
    ) -> u32 {
        let cov = (rad + 0.5 - d).clamp(0.0, 1.0);
        if ghost {
            if cov <= 0.0 {
                return 0;
            }
            return (88.0 * cov) as u32;
        }
        let dn = (d / rad).min(1.0);
        match brush {
            // TRUE WHITEBOARD MARKER: a solid, FULLY OPAQUE pen — crisp body, a short soft edge
            // for clean anti-aliasing only. The pen lays flat ink (255 at the body), so a filled
            // region reads as one solid block and never shows the desktop through it; the GRAIN
            // lives in the textured media (crayon/water/chalk), not the pen.
            Brush::Marker => {
                if cov <= 0.0 {
                    return 0;
                }
                let edge = if dn < 0.82 {
                    1.0
                } else {
                    ((1.0 - dn) / 0.18).clamp(0.2, 1.0)
                };
                (255.0 * edge * cov) as u32
            }
            // CRAYON: wax is DIRECTIONAL — grains stretch ~7:1 along the pull (the defining
            // signature chalk must not share), gated by a fine isotropic paper tooth, heavier
            // deposit at the core, ragged at the true edge.
            Brush::Crayon => {
                if cov <= 0.0 {
                    return 0;
                }
                let grain = vnoise(s * 0.32, d * 2.3, seed);
                let tooth = bnoise(x, y, seed ^ 0x9D2C) * 0.5 + 0.5 * bnoise(x >> 1, y >> 1, seed);
                let g = grain * 0.7 + tooth * 0.3;
                if g < 0.30 + 0.40 * dn * dn {
                    return 0;
                }
                let wax = 1.0 - 0.22 * dn;
                (((128.0 + 112.0 * g) * wax).min(232.0) * cov) as u32
            }
            // WATERCOLOUR: the edge WAVERS (the distance itself is domain-warped by a slow
            // noise in s — washes don't have ruler edges), pigment POOLS in soft patches along
            // the body, walks to the wobbled rim as it dries, and the paper mottles it.
            Brush::Water => {
                let wob = (vnoise(s * 0.045, 3.7, seed) - 0.5) * rad * 0.42;
                let dw = d + wob;
                let covw = (rad + 0.5 - dw).clamp(0.0, 1.0);
                if covw <= 0.0 {
                    return 0;
                }
                let dnw = (dw.max(0.0) / rad).min(1.0);
                let pool = vnoise(s * 0.02, dw * 0.16, seed ^ 0x33) * 0.65
                    + vnoise(s * 0.07, dw * 0.45, seed ^ 0x77) * 0.35;
                let body = 42.0 + 46.0 * pool;
                let rim = 58.0 * dnw * dnw * dnw;
                let paper = 0.90 + 0.10 * bnoise(x >> 1, y >> 1, seed);
                (((body + rim) * paper).min(150.0) * covw) as u32
            }
            // CHALK: dust — ISOTROPIC speckle (no direction: the opposite of crayon), deposits
            // most at the core and breathes off toward the rim, and the BOARD's own texture
            // (screen-locked patches) decides where it refuses to land.
            Brush::Chalk => {
                if cov <= 0.0 {
                    return 0;
                }
                let dust = bnoise(x, y, seed.wrapping_add(13));
                let board = vnoise(x as f32 * 0.09, y as f32 * 0.09, 0xB0A2D); // paper-locked
                let density = (1.0 - dn).sqrt();
                if dust < 0.16 + 0.55 * (1.0 - density) || board < 0.16 {
                    return 0;
                }
                ((92.0 + 118.0 * dust * density + 28.0 * board).min(210.0) * cov) as u32
            }
            // ICHOR: hard-light glass — a crisp luminous body with a bright rim where the fire
            // lives (the colour is computed per-pixel in `ichor_ink`; this is the alpha profile).
            // Cleaner than the media brushes (it's light, not pigment): a solid core that lifts
            // toward the edge so the spectral rim reads, then a 1px AA falloff.
            Brush::DirectedIntent => {
                if cov <= 0.0 {
                    return 0;
                }
                let rim = ((dn - 0.55) / 0.45).clamp(0.0, 1.0);
                let body = 0.80 + 0.20 * rim; // brighter toward the rim — the glass edge glows
                let shimmer = 0.94 + 0.06 * vnoise(s * 0.06, d * 0.5, seed ^ 0x1C);
                (235.0 * body * shimmer * cov) as u32
            }
        }
    }

    /// A preview pixel for the dock cell: the REAL ichor (via `material_core`) with an ANALYTIC
    /// surface normal — the cell draws a sine spine `cy + 3·sin(0.22x)`, so the distance-field
    /// gradient is `(-spine', sgn)` (perpendicular to the spine, pointing off whichever side the
    /// pixel sits). Same material as the canvas; only the field source differs.
    pub(super) fn material_preview_px(
        d: f32,
        rad: f32,
        xx: i32,
        side: f32,
        t: f32,
        m: &crate::weave::Material,
    ) -> (u32, u32) {
        let slope = 0.66 * (xx as f32 * 0.22).cos(); // d/dx of the preview spine
                                                     // x = the cell longitude, y = the signed offset off the spine — so the LIVE material's
                                                     // animated fields (the same surface the canvas pours) breathe in the dock swatch too.
        material_core(d, rad, -slope * side, side, xx as f32, side * d, t, m)
    }

    /// Render a COMMITTED stroke (full repaints, shape-set replacements): carve every segment
    /// (accumulating arc length so the grain rides exactly as it did live), composite once from
    /// true distance, return the field to rest.
    pub(super) fn render_stroke(px: &mut PixelBuf, dmin: &mut FieldBuf, smin: &mut FieldBuf, s: &Stroke) {
        if s.pts.is_empty() {
            return;
        }
        let reach = (s.width / 2.0).max(1.0) + 2.0;
        let mut bb: Option<Region> = None;
        let mut arc = 0.0f32;
        if s.pts.len() == 1 {
            bb = Some(carve(dmin, smin, s.pts[0], s.pts[0], reach, 0.0, 0.0));
        }
        for sw in s.pts.windows(2) {
            // cast to f32 BEFORE subtracting/squaring — a pathological point pair (near
            // i32::MIN/MAX) would overflow the i32 `.pow(2)` this used to do; the field carve
            // itself is already float-space and handles any magnitude here without a panic.
            let (dx, dy) = (sw[1].0 as f32 - sw[0].0 as f32, sw[1].1 as f32 - sw[0].1 as f32);
            let seg_len = (dx * dx + dy * dy).sqrt();
            bb = Some(union(bb, carve(dmin, smin, sw[0], sw[1], reach, arc, seg_len)));
            arc += seg_len;
        }
        if let Some(r) = bb {
            composite(px, None, dmin.as_view(), smin.as_view(), r, s);
            reset_region(dmin, r);
        }
    }

    /// Deterministic 2D hash noise in [0, 1) — the brushes' grain. Pure integer mixing: no
    /// tables, no allocation, stable for a (x, y, seed) triple forever.
    fn bnoise(x: i32, y: i32, seed: u32) -> f32 {
        let mut n = (x as u32).wrapping_mul(0x9E37_79B1)
            ^ (y as u32).wrapping_mul(0x85EB_CA6B)
            ^ seed.wrapping_mul(0xC2B2_AE35);
        n ^= n >> 15;
        n = n.wrapping_mul(0x2C1B_3C6D);
        n ^= n >> 12;
        (n & 0xFFFF) as f32 / 65536.0
    }

    /// Stamp one flat-alpha segment as a CAPSULE (distance-to-segment, anti-aliased edge).
    /// For a flat alpha, the max-blend union of capsules IS the polyline's distance field — so
    /// glows and ghosts read as smooth ribbons, never chains of discs.
    #[allow(clippy::too_many_arguments)] // a raster primitive's natural arity, not an API smell
    fn stamp_segment(px: &mut PixelBuf, from: (i32, i32), to: (i32, i32), color: u32, width: f32, a: u32) {
        let (w, h) = (px.w(), px.h());
        if w <= 0 || h <= 0 {
            return;
        }
        let rad = (width / 2.0).max(1.0);
        let ri = rad.ceil() as i32 + 1;
        // saturating: a pathological endpoint near i32::MIN/MAX clips at the buffer edge instead
        // of overflowing the subtract/add.
        let x0 = (from.0.min(to.0).saturating_sub(ri)).clamp(0, w - 1);
        let x1 = (from.0.max(to.0).saturating_add(ri)).clamp(0, w - 1);
        let y0 = (from.1.min(to.1).saturating_sub(ri)).clamp(0, h - 1);
        let y1 = (from.1.max(to.1).saturating_add(ri)).clamp(0, h - 1);
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                let d = seg_dist_f((xx as f32, yy as f32), from, to);
                let cov = (rad + 0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let aa = (a as f32 * cov) as u32;
                if aa == 0 {
                    continue;
                }
                // SAFETY: (xx, yy) ranges over x0..=x1, y0..=y1, clamped above against this same
                // buffer's px.w()/px.h() — not a separately-carried w/h.
                unsafe {
                    let under = px.get_unchecked(xx, yy);
                    px.put_unchecked(xx, yy, blend_max(under, color, aa));
                }
            }
        }
    }

    /// THE SELECTION WAVEFORM — selection is the "intent" side of directed intent, so a crest of
    /// light TRAVELS along the selected ink. The hard prism edge (baked in `clean`) is untouched;
    /// this adds a moving wave: a bright spectral crest sweeping along each stroke by arc-fraction,
    /// broken by per-point noise so it shimmers like the material rather than marching as a dot.
    fn selection_wave(px: &mut PixelBuf, strokes: &[Stroke], selected: &[usize], frame: u32) {
        {
            let facets = crate::weave::live_material().facets.max(1) as f32;
            let phase = frame as f32 * 0.16;
            for (rank, &i) in selected.iter().enumerate() {
                let Some(s) = strokes.get(i) else { continue };
                if s.pts.len() < 2 {
                    continue;
                }
                // arc-length parametrise so the crest's SPEED is even regardless of point density
                let mut cum = 0.0f32;
                let mut acc: Vec<f32> = Vec::with_capacity(s.pts.len());
                acc.push(0.0);
                for win in s.pts.windows(2) {
                    // cast to f32 before subtracting (see render_stroke's identical fix): a
                    // pathological point pair overflows the i32 `.pow(2)` this used to do.
                    let (dx, dy) = (
                        win[1].0 as f32 - win[0].0 as f32,
                        win[1].1 as f32 - win[0].1 as f32,
                    );
                    cum += (dx * dx + dy * dy).sqrt();
                    acc.push(cum);
                }
                let total = cum.max(1.0);
                for (j, &p) in s.pts.iter().enumerate() {
                    let u = acc[j] / total; // 0..1 along the stroke
                                            // a crest sweeping along, offset per-stroke so multi-selection shimmers as a set
                    let wave = 0.5
                        + 0.5 * (u * std::f32::consts::TAU * 1.6 - phase - rank as f32 * 1.1).sin();
                    let tw = vnoise(acc[j] * 0.12, rank as f32 * 3.7, s.seed ^ 0x5E1E);
                    let spark = (wave * wave) * (0.45 + 0.55 * tw);
                    if spark <= 0.10 {
                        continue;
                    }
                    // cut-glass prism crest (facet-quantised hue) + a white heart it diffuses from —
                    // the comet material's own grammar, riding the selection's edge.
                    let hue = (((j as f32 / s.pts.len() as f32) * 300.0 + frame as f32 * 2.2)
                        / 360.0
                        * facets)
                        .round()
                        / facets
                        * 360.0;
                    let c = hsv(hue, 0.85, 1.0);
                    stamp_segment(px, p, p, c, s.width + 4.5, (150.0 * spark) as u32);
                    stamp_segment(px, p, p, 0xFFFFFF, s.width + 1.0, (200.0 * spark) as u32);
                }
            }
        }
    }

    /// A laser RUN — one lift-to-lift sweep, carrying the PEN it was drawn with so the trail renders
    /// in the user's CURRENT material/size (never a hardcoded one) and never connects across a lift.
    struct LaserRun {
        pen: super::Pen,
        pts: Vec<(i32, i32, Instant)>,
    }

    /// THE LASER TRAIL — a presenter's pointer that IS the current pen, with one added MECHANIC: it
    /// fades to nothing over `LASER_MS` instead of being kept. No hardcoded material, size or colour
    /// — each run renders through its own pen's brush (or the real Ichor material if that's the held
    /// pen), at the pen's width, so the laser is the same media you draw with, just transient.
    fn draw_laser(px: &mut PixelBuf, runs: &[LaserRun], now: Instant) {
        {
            let life_of = |born: Instant| {
                (1.0 - now.duration_since(born).as_millis() as f32 / LASER_MS as f32)
                    .clamp(0.0, 1.0)
            };
            for run in runs {
                let pen = &run.pen;
                let rad = (pen.width / 2.0).max(1.0); // the CURRENT pen's size — never hardcoded
                                                      // when the held pen IS directed-intent, pour the real material (accent = the swatch);
                                                      // otherwise the run renders through the pen's own brush below. Computed once per run.
                let material_mat = if pen.brush == super::Brush::DirectedIntent {
                    let mut m = crate::weave::live_material();
                    let c = pen.color;
                    m.accent = (
                        ((c >> 16) & 0xFF) as f32 / 255.0,
                        ((c >> 8) & 0xFF) as f32 / 255.0,
                        (c & 0xFF) as f32 / 255.0,
                    );
                    m.accent_hue = crate::weave::hue_u32(c);
                    Some(m)
                } else {
                    None
                };
                let trail = &run.pts;
                if trail.len() < 2 {
                    // a lone point still glints so a tap-hover pointer is never invisible — but it FADES
                    // with its age (the last node no longer stays frozen at full opacity).
                    if let Some(&(x, y, born)) = trail.first() {
                        let life = life_of(born);
                        if life > 0.0 {
                            splat_soft(px, x, y, rad.max(3.0), pen.color, (235.0 * life * life) as u32);
                        }
                    }
                    continue;
                }
                let mut prev: Option<(i32, i32, f32)> = None;
                for &(x, y, born) in trail {
                    let life = life_of(born);
                    if let Some((px0, py0, pl)) = prev {
                        // the segment's fade tracks its two endpoints' lives (head bright → tail gone)
                        let amp = life.max(pl).powi(2);
                        laser_segment(px, (px0, py0), (x, y), rad, amp, pen, material_mat.as_ref());
                    }
                    prev = Some((x, y, life));
                }
                // the head bead, in the PEN's colour, FADING with the head's life (no frozen-bright node)
                if let Some(&(x, y, born)) = trail.last() {
                    let life = life_of(born);
                    if life > 0.0 {
                        splat_soft(px, x, y, (rad * 0.9).max(3.0), pen.color, (235.0 * life * life) as u32);
                    }
                }
            }
        }
    }

    /// One laser segment, rendered through the run's PEN — the real Ichor material when that's the
    /// held pen (gradient = the segment normal), else the pen's own brush — pre-multiplied and
    /// max-blended like the canvas ink so overlapping never darkens, scaled by the fade `amp`.
    #[allow(clippy::too_many_arguments)] // a fragment shader's natural arity (its varyings)
    fn laser_segment(
        px: &mut PixelBuf,
        a: (i32, i32),
        b: (i32, i32),
        rad: f32,
        amp: f32,
        pen: &super::Pen,
        material_mat: Option<&crate::weave::Material>,
    ) {
        let (w, h) = (px.w(), px.h());
        if w <= 0 || h <= 0 {
            return;
        }
        let r = (rad + 2.0).ceil() as i32 + 1;
        // saturating: a pathological endpoint near i32::MIN/MAX clips at the buffer edge instead
        // of overflowing the subtract/add.
        let x0 = (a.0.min(b.0).saturating_sub(r)).clamp(0, w - 1);
        let x1 = (a.0.max(b.0).saturating_add(r)).clamp(0, w - 1);
        let y0 = (a.1.min(b.1).saturating_sub(r)).clamp(0, h - 1);
        let y1 = (a.1.max(b.1).saturating_add(r)).clamp(0, h - 1);
        let mt = crate::weave::seconds();
        for yy in y0..=y1 {
            for xx in x0..=x1 {
                let (d, t) = seg_proj((xx as f32, yy as f32), a, b);
                if d > rad + 2.0 {
                    continue;
                }
                let (col, al) = match material_mat {
                    Some(m) => {
                        // analytic field gradient = the segment normal (so the rim/facets resolve)
                        let (ax, ay) = (a.0 as f32, a.1 as f32);
                        let (bx, by) = (b.0 as f32, b.1 as f32);
                        let (cx, cy) = (ax + (bx - ax) * t, ay + (by - ay) * t);
                        let (mut gx, mut gy) = (xx as f32 - cx, yy as f32 - cy);
                        let gl = (gx * gx + gy * gy).sqrt().max(1e-3);
                        gx /= gl;
                        gy /= gl;
                        material_core(d, rad, gx, gy, xx as f32, yy as f32, mt, m)
                    }
                    None => {
                        // the pen's OWN brush — the laser is the same media, just fading. (arc = 0:
                        // the along-stroke streak doesn't vary on a transient pointer; the across/
                        // tooth grain via d,x,y still reads, so crayon/chalk look like themselves.)
                        let al = brush_alpha(pen.brush, false, 0x1A5E, d, rad, 0.0, xx, yy);
                        (pen.color, al)
                    }
                };
                let al = (al as f32 * amp) as u32;
                if al == 0 {
                    continue;
                }
                // SAFETY: (xx, yy) ranges over x0..=x1, y0..=y1, clamped above against this same
                // buffer's px.w()/px.h().
                unsafe {
                    let under = px.get_unchecked(xx, yy);
                    px.put_unchecked(xx, yy, blend_max(under, col, al));
                }
            }
        }
    }

    /// A soft round bead (a degenerate point capsule) — the ping/head dots. A point→point
    /// `stamp_segment` IS a disc (distance-to-segment with a==b), AA'd + max-blended, no new code.
    fn splat_soft(px: &mut PixelBuf, cx: i32, cy: i32, rad: f32, color: u32, peak: u32) {
        stamp_segment(px, (cx, cy), (cx, cy), color, rad * 2.0, peak);
    }

    // ── ping animation easings (the "svg magic": smooth/overshoot/draw-on curves) ──
    fn ease_out(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        1.0 - (1.0 - t) * (1.0 - t)
    }
    fn smooth(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
    /// back-ease-out: overshoots past 1, then settles — the emphatic "pop".
    fn ease_back(t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0) - 1.0;
        let s = 1.70158;
        1.0 + t * t * ((s + 1.0) * t + s)
    }

    /// One RIPPLE ring poured through the directed-intent material (accent = the pen colour) — the
    /// shared base every ping's animation builds on. `rr` = radius, `band` = half-width, `amp` =
    /// brightness; `d` runs across the band, the gradient is radial, so the ring IS the ink's glass.
    #[allow(clippy::too_many_arguments)] // a raster primitive's varyings, not an API smell
    fn ping_ring(px: &mut PixelBuf, cx: i32, cy: i32, rr: f32, band: f32, amp: f32, m: &crate::weave::Material) {
        if amp <= 0.01 || rr <= 0.0 {
            return;
        }
        let (w, h) = (px.w(), px.h());
        let yl = ((cy as f32 - rr - band - 1.0) as i32).max(0);
        let yh = ((cy as f32 + rr + band + 1.0) as i32).min(h - 1);
        let xl = ((cx as f32 - rr - band - 1.0) as i32).max(0);
        let xh = ((cx as f32 + rr + band + 1.0) as i32).min(w - 1);
        let mt = crate::weave::seconds();
        for yy in yl..=yh {
            for xx in xl..=xh {
                let dxp = xx as f32 - cx as f32;
                let dyp = yy as f32 - cy as f32;
                let dist = (dxp * dxp + dyp * dyp).sqrt();
                let d = (dist - rr).abs();
                if d > band {
                    continue;
                }
                let inv = 1.0 / dist.max(1e-3);
                let (col, al) = material_core(d, band, dxp * inv, dyp * inv, xx as f32, yy as f32, mt, m);
                let al = (al as f32 * amp.clamp(0.0, 1.0)) as u32;
                if al == 0 {
                    continue;
                }
                // SAFETY: (xx, yy) ranges over xl..=xh, yl..=yh, clamped above against this same
                // buffer's px.w()/px.h().
                unsafe {
                    let under = px.get_unchecked(xx, yy);
                    px.put_unchecked(xx, yy, blend_max(under, col, al));
                }
            }
        }
    }

    /// Draw a ping's SYMBOL as procedural line-art at (cx,cy), scale `sz`, colour `color`, `alpha`,
    /// DRAWN-ON up to `reveal` (0..1) of its total path length — so a ✓ strokes in, a ✗ slashes in.
    /// crosshair / ? / ! / ✓ / ✗ / → — the abstract comms idea, read at a glance.
    #[allow(clippy::too_many_arguments)] // a raster primitive's varyings, not an API smell
    fn draw_ping_glyph(
        px: &mut PixelBuf,
        cx: i32,
        cy: i32,
        sz: f32,
        kind: PingKind,
        color: u32,
        alpha: u32,
        reveal: f32,
    ) {
        {
            if alpha == 0 || sz < 1.0 {
                return;
            }
            let sr = (sz * 0.16).max(1.6);
            // each symbol = an ORDERED segment list (glyph-local −1..1) + an optional trailing dot, so
            // `reveal` can draw it on progressively; the dot lands once the strokes are ~complete.
            let (segs, dot): (Vec<((f32, f32), (f32, f32))>, Option<(f32, f32)>) = match kind {
                PingKind::Here => (
                    vec![((-0.6, 0.0), (0.6, 0.0)), ((0.0, -0.6), (0.0, 0.6))],
                    None,
                ),
                PingKind::Ask => (
                    vec![
                        ((-0.42, -0.28), (-0.18, -0.55)),
                        ((-0.18, -0.55), (0.2, -0.5)),
                        ((0.2, -0.5), (0.4, -0.18)),
                        ((0.4, -0.18), (0.12, 0.08)),
                        ((0.12, 0.08), (0.0, 0.32)),
                    ],
                    Some((0.0, 0.74)),
                ),
                PingKind::Bang => (vec![((0.0, -0.66), (0.0, 0.26))], Some((0.0, 0.7))),
                PingKind::Yes => (
                    vec![
                        ((-0.62, 0.05), (-0.16, 0.52)),
                        ((-0.16, 0.52), (0.66, -0.56)),
                    ],
                    None,
                ),
                PingKind::No => (
                    vec![
                        ((-0.56, -0.56), (0.56, 0.56)),
                        ((-0.56, 0.56), (0.56, -0.56)),
                    ],
                    None,
                ),
                PingKind::Arrow => (
                    vec![
                        ((-0.72, 0.0), (0.72, 0.0)),
                        ((0.72, 0.0), (0.26, -0.44)),
                        ((0.72, 0.0), (0.26, 0.44)),
                    ],
                    None,
                ),
            };
            let pt = |fx: f32, fy: f32| {
                (
                    (cx as f32 + fx * sz).round() as i32,
                    (cy as f32 + fy * sz).round() as i32,
                )
            };
            let lens: Vec<f32> = segs
                .iter()
                .map(|(a, b)| (b.0 - a.0).hypot(b.1 - a.1) * sz)
                .collect();
            let total: f32 = lens.iter().sum::<f32>().max(1e-3);
            let target = reveal.clamp(0.0, 1.0) * total;
            let mut acc = 0.0f32;
            for (i, (a, b)) in segs.iter().enumerate() {
                if acc >= target {
                    break;
                }
                let f = ((target - acc) / lens[i].max(1e-3)).clamp(0.0, 1.0); // partial draw of this seg
                let bp = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
                stamp_segment(px, pt(a.0, a.1), pt(bp.0, bp.1), color, sr * 2.0, alpha);
                acc += lens[i];
            }
            if reveal >= 0.9 {
                if let Some((dx, dy)) = dot {
                    let (px2, py2) = pt(dx, dy);
                    splat_soft(px, px2, py2, sr, color, alpha);
                }
            }
        }
    }

    /// A PING — a comms mark a presenter drops, with a CUSTOM animation per kind, all inheriting one
    /// base (a material ping_ring in the pen colour + the kind's SYMBOL): Here focuses rings inward;
    /// Bang shockwaves + pops + shakes; Ask bobs in curiously; Yes/No draw their mark on; Arrow
    /// thrusts forward with a motion-trail. Everything fades over PING_MS.
    #[allow(clippy::too_many_arguments)] // a raster primitive's varyings, not an API smell
    fn draw_ping(px: &mut PixelBuf, cx: i32, cy: i32, kind: PingKind, color: u32, age_ms: f32) {
        {
            let life = (1.0 - age_ms / PING_MS as f32).clamp(0.0, 1.0);
            if life <= 0.0 {
                return;
            }
            // the LIVE material, accent = the pen colour: every ripple is the real spellweaving ink
            let mut m = crate::weave::live_material();
            m.accent = (
                ((color >> 16) & 0xFF) as f32 / 255.0,
                ((color >> 8) & 0xFF) as f32 / 255.0,
                (color & 0xFF) as f32 / 255.0,
            );
            m.accent_hue = crate::weave::hue_u32(color);
            let ent = (age_ms / 180.0).clamp(0.0, 1.0); // the entrance window
            let big = 18.0; // the symbol is the hero now (it was lost in the bloom before)
            let amask = (235.0 * life) as u32; // the symbol's alpha as the ping ages out
            match kind {
                // FOCUS: three rings converge INWARD to lock on, then a gentle breath out; crosshair in.
                PingKind::Here => {
                    for k in 0..3 {
                        let rt = (ent * 1.25 - k as f32 * 0.16).clamp(0.0, 1.0);
                        if rt > 0.0 && rt < 1.0 {
                            ping_ring(
                                px,
                                cx,
                                cy,
                                6.0 + 34.0 * (1.0 - ease_out(rt)),
                                2.6,
                                (1.0 - rt) * life,
                                &m,
                            );
                        }
                    }
                    ping_ring(
                        px,
                        cx,
                        cy,
                        6.0 + (1.0 - life) * 18.0,
                        2.4,
                        life * life * 0.7,
                        &m,
                    );
                    draw_ping_glyph(px, cx, cy, big * smooth(ent), kind, color, amask, 1.0);
                }
                // EMPHASIS: a fast shockwave, the ! pops with overshoot + a quick vertical shake.
                PingKind::Bang => {
                    let sw = ease_out((age_ms / 240.0).clamp(0.0, 1.0));
                    ping_ring(
                        px,
                        cx,
                        cy,
                        6.0 + sw * 42.0,
                        3.2,
                        (1.0 - sw) * life,
                        &m,
                    );
                    // a decaying vertical jolt on NORMALISED time (reads as a shake at 30fps, not aliased
                    // buzz): 2.5 cycles over 300ms, envelope fading to nothing, starts dead-centre.
                    let sk = (age_ms / 300.0).clamp(0.0, 1.0);
                    let shake = (sk * std::f32::consts::TAU * 2.5).sin() * 3.0 * (1.0 - sk);
                    draw_ping_glyph(
                        px,
                        cx,
                        (cy as f32 + shake) as i32,
                        big * ease_back(ent),
                        kind,
                        color,
                        amask,
                        1.0,
                    );
                }
                // CURIOSITY: a soft slow pulse; the ? scales + bobs gently the whole time.
                PingKind::Ask => {
                    ping_ring(
                        px,
                        cx,
                        cy,
                        7.0 + (1.0 - life) * 26.0,
                        3.0,
                        (life * 0.8).min(1.0),
                        &m,
                    );
                    let bob = (age_ms * 0.006).sin() * 2.2 * life;
                    draw_ping_glyph(
                        px,
                        cx,
                        (cy as f32 + bob) as i32,
                        big * smooth(ent),
                        kind,
                        color,
                        amask,
                        smooth(ent),
                    );
                }
                // AFFIRM: the ✓ DRAWS ON over the entrance, on a soft expanding ripple.
                PingKind::Yes => {
                    ping_ring(
                        px,
                        cx,
                        cy,
                        7.0 + (1.0 - life) * 28.0,
                        3.0,
                        (life * 0.85).min(1.0),
                        &m,
                    );
                    draw_ping_glyph(px, cx, cy, big, kind, color, amask, ease_out(ent));
                }
                // NEGATE: the ✗ slashes in (both diagonals draw on) + a quick horizontal shake.
                PingKind::No => {
                    ping_ring(
                        px,
                        cx,
                        cy,
                        7.0 + (1.0 - life) * 28.0,
                        3.0,
                        (life * 0.85).min(1.0),
                        &m,
                    );
                    let sk = (age_ms / 300.0).clamp(0.0, 1.0);
                    let shake = (sk * std::f32::consts::TAU * 2.5).sin() * 2.8 * (1.0 - sk);
                    draw_ping_glyph(
                        px,
                        (cx as f32 + shake) as i32,
                        cy,
                        big,
                        kind,
                        color,
                        amask,
                        ease_out(ent),
                    );
                }
                // DIRECTION: the → thrusts forward (slides in from behind) trailing two fading ghosts.
                PingKind::Arrow => {
                    ping_ring(
                        px,
                        cx,
                        cy,
                        7.0 + (1.0 - life) * 26.0,
                        3.0,
                        (life * 0.8).min(1.0),
                        &m,
                    );
                    let push = (1.0 - ease_out(ent)) * big * 1.4; // slides in from behind to land at cx
                    draw_ping_glyph(
                        px,
                        (cx as f32 - push * 2.8) as i32,
                        cy,
                        big,
                        kind,
                        color,
                        (amask as f32 * 0.16) as u32,
                        1.0,
                    );
                    draw_ping_glyph(
                        px,
                        (cx as f32 - push * 1.8) as i32,
                        cy,
                        big,
                        kind,
                        color,
                        (amask as f32 * 0.36) as u32,
                        1.0,
                    );
                    draw_ping_glyph(
                        px,
                        (cx as f32 - push) as i32,
                        cy,
                        big,
                        kind,
                        color,
                        amask,
                        1.0,
                    );
                }
            }
            // a small white-hot heart anchors the exact spot (steady, a gentle beat)
            let beat = 0.7 + 0.3 * (age_ms * 0.02).sin();
            splat_soft(
                px,
                cx,
                cy,
                2.2 * life.max(0.3),
                0xFFFFFF,
                (190.0 * life * beat) as u32,
            );
        }
    }

    /// THE REACTIONARY-PING WHEEL — the direct-comms radial the laser-mode tap-and-hold raises:
    /// the six pings laid clockwise from North, the one under the aim lit. Drawn on the canvas at
    /// the cursor; the session resolves the choice by release direction (the radial weave grammar).
    #[allow(clippy::too_many_arguments)]
    fn draw_ping_wheel(px: &mut PixelBuf, cx: i32, cy: i32, sect: i32, color: u32, frame: u32) {
        let n = PingKind::WHEEL.len() as i32;
        let rr = 64.0;
        let breathe = 0.82 + 0.18 * ((frame as f32) * 0.12).sin();
        // the MENU wears the PEN's colour (it IS the stroke you're about to drop) — the six kinds
        // are told apart by their SYMBOL, not a hue; the aimed one goes white-hot to lead the eye.
        let phos = color;
        splat_soft(px, cx, cy, 6.0, phos, (90.0 * breathe) as u32); // faint hub = "here"
        for (k, kind) in PingKind::WHEEL.iter().enumerate() {
            let a = k as f32 / n as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2; // N, clockwise
            let (dx, dy) = (a.cos(), a.sin());
            let lit = k as i32 == sect;
            // a faint spoke out to the option, brightening when aimed
            for t in 1..=10 {
                let f = t as f32 / 10.0;
                let (x, y) = (cx as f32 + dx * rr * f, cy as f32 + dy * rr * f);
                stamp_segment(
                    px,
                    (x as i32, y as i32),
                    (x as i32, y as i32),
                    phos,
                    if lit { 2.6 } else { 1.8 },
                    (((if lit { 130.0 } else { 55.0 }) * f) as u32).max(1),
                );
            }
            // the option's SYMBOL at the rim — the aimed one bigger + white-hot over a soft halo
            let (ox, oy) = ((cx as f32 + dx * rr) as i32, (cy as f32 + dy * rr) as i32);
            if lit {
                splat_soft(px, ox, oy, 13.0, phos, (60.0 * breathe) as u32);
            }
            draw_ping_glyph(
                px,
                ox,
                oy,
                if lit { 13.0 } else { 9.0 },
                *kind,
                if lit { 0xFFFFFF } else { phos },
                if lit { 235 } else { 120 },
                1.0,
            );
        }
    }

    /// Adversarial coverage for the raster primitives above: every one of them is secretly pure
    /// (no window, no DIB, no OS call) once its buffers are `PixelBuf`/`FieldBuf` instead of a
    /// caller-trusted `(pointer, w, h)` triple. These tests drive them directly with synthetic
    /// buffers at edge-case geometry — the exact stale/mismatched-dimensions shape the PixelBuf
    /// refactor exists to make unrepresentable — and assert no panic and no out-of-region write.
    #[cfg(test)]
    mod tests {
        use super::*;

        /// A `w*h`-logical-word buffer with a sentinel guard band after it. Any raster primitive
        /// that walks past the logical region (the exact heap-corruption shape a stale/mismatched
        /// `w`/`h` used to allow) overwrites a sentinel — `assert_guard_intact` catches it.
        struct Guarded {
            buf: Vec<u32>,
            w: i32,
            h: i32,
        }
        const GUARD: usize = 64;
        const SENTINEL: u32 = 0xDEAD_BEEF;
        impl Guarded {
            fn new(w: i32, h: i32) -> Self {
                let n = if w <= 0 || h <= 0 { 0 } else { (w * h) as usize };
                let mut buf = vec![0u32; n + GUARD];
                for s in &mut buf[n..] {
                    *s = SENTINEL;
                }
                Guarded { buf, w, h }
            }
            fn pixel_buf(&mut self) -> PixelBuf<'_> {
                let n = if self.w <= 0 || self.h <= 0 {
                    0
                } else {
                    (self.w * self.h) as usize
                };
                PixelBuf::new(&mut self.buf[..n], self.w, self.h)
            }
            fn assert_guard_intact(&self) {
                let n = self.buf.len() - GUARD;
                assert!(
                    self.buf[n..].iter().all(|&v| v == SENTINEL),
                    "guard band corrupted — an OOB write escaped the logical {}x{} region",
                    self.w,
                    self.h
                );
            }
        }

        fn stroke(pts: Vec<(i32, i32)>, brush: Brush, width: f32) -> Stroke {
            Stroke {
                pts,
                color: 0xFF8040,
                width,
                brush,
                ghost: false,
                seed: 0x1234,
            }
        }

        const EXTREME: [i32; 9] = [
            i32::MIN,
            i32::MIN + 1,
            -1_000_000_000,
            -1,
            0,
            1,
            1_000_000_000,
            i32::MAX - 1,
            i32::MAX,
        ];

        #[test]
        fn stamp_segment_zero_size_buffer_is_noop_not_panic() {
            let mut g = Guarded::new(0, 0);
            let mut pb = g.pixel_buf();
            stamp_segment(&mut pb, (0, 0), (5, 5), 0xFFFFFF, 4.0, 200);
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn stamp_segment_extreme_coordinates_never_panic_or_corrupt() {
            let mut g = Guarded::new(24, 24);
            let mut pb = g.pixel_buf();
            for &x in &EXTREME {
                for &y in &EXTREME {
                    stamp_segment(&mut pb, (x, y), (x.wrapping_add(3), y), 0xFF00FF, 6.0, 255);
                }
            }
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn stamp_segment_radius_larger_than_buffer_stays_in_bounds() {
            let mut g = Guarded::new(8, 8);
            let mut pb = g.pixel_buf();
            // a capsule whose radius dwarfs the whole buffer, crossing all four edges
            stamp_segment(&mut pb, (-500, -500), (500, 500), 0xFFFFFF, 100_000.0, 255);
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn splat_soft_extreme_and_1x1_buffer() {
            let mut g = Guarded::new(1, 1);
            let mut pb = g.pixel_buf();
            for &(x, y) in &[(0, 0), (i32::MIN, i32::MAX), (i32::MAX, i32::MIN), (-1, -1)] {
                splat_soft(&mut pb, x, y, 9999.0, 0xABCDEF, 255);
            }
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn carve_and_reset_region_zero_size_field_is_noop() {
            let mut dmin_data: Vec<f32> = Vec::new();
            let mut smin_data: Vec<f32> = Vec::new();
            let mut dmin = FieldBuf::new(&mut dmin_data, 0, 0);
            let mut smin = FieldBuf::new(&mut smin_data, 0, 0);
            let r = carve(&mut dmin, &mut smin, (i32::MIN, i32::MIN), (i32::MAX, i32::MAX), 50.0, 0.0, 10.0);
            reset_region(&mut dmin, r); // must not panic on the degenerate region carve() returns
        }

        #[test]
        fn carve_extreme_segment_stays_in_bounds() {
            let (w, h) = (12, 12);
            let mut dmin_data = vec![f32::MAX; (w * h) as usize];
            let mut smin_data = vec![0.0f32; (w * h) as usize];
            let mut dmin = FieldBuf::new(&mut dmin_data, w, h);
            let mut smin = FieldBuf::new(&mut smin_data, w, h);
            let r = carve(&mut dmin, &mut smin, (i32::MIN, i32::MIN), (i32::MAX, i32::MAX), 25.0, 0.0, 1e9);
            // the returned region must itself be within the field's own bounds
            assert!(r.0 >= 0 && r.2 < w || r.0 > r.2);
            assert!(r.1 >= 0 && r.3 < h || r.1 > r.3);
            reset_region(&mut dmin, r);
        }

        #[test]
        fn composite_and_render_stroke_zero_size_canvas_is_noop() {
            let mut px_data: Vec<u32> = Vec::new();
            let mut dmin_data: Vec<f32> = Vec::new();
            let mut smin_data: Vec<f32> = Vec::new();
            let mut px = PixelBuf::new(&mut px_data, 0, 0);
            let mut dmin = FieldBuf::new(&mut dmin_data, 0, 0);
            let mut smin = FieldBuf::new(&mut smin_data, 0, 0);
            let s = stroke(vec![(i32::MIN, 0), (0, i32::MAX), (5, 5)], Brush::Marker, 6.0);
            render_stroke(&mut px, &mut dmin, &mut smin, &s); // must not panic
        }

        #[test]
        fn render_stroke_extreme_points_stay_in_guard_band() {
            let (w, h) = (16, 16);
            let mut g = Guarded::new(w, h);
            let mut dmin_data = vec![f32::MAX; (w * h) as usize];
            let mut smin_data = vec![0.0f32; (w * h) as usize];
            let mut dmin = FieldBuf::new(&mut dmin_data, w, h);
            let mut smin = FieldBuf::new(&mut smin_data, w, h);
            for &brush in &[Brush::Marker, Brush::Crayon, Brush::Water, Brush::Chalk, Brush::DirectedIntent] {
                let s = stroke(
                    vec![
                        (i32::MIN, i32::MIN),
                        (i32::MAX, i32::MAX),
                        (i32::MIN, i32::MAX),
                        (7, 7),
                    ],
                    brush,
                    12.0,
                );
                let mut pb = g.pixel_buf();
                render_stroke(&mut pb, &mut dmin, &mut smin, &s);
                drop(pb);
            }
            g.assert_guard_intact();
        }

        #[test]
        fn restore_region_mismatched_and_degenerate_regions_are_safe() {
            let mut g = Guarded::new(6, 6);
            let base_data = vec![0x11223344u32; 36];
            let base = PixelView::new(&base_data, 6, 6);
            let mut pb = g.pixel_buf();
            // a region that runs off every edge, plus the degenerate empty-region convention
            restore_region(&mut pb, base, (-100, -100, 100, 100));
            restore_region(&mut pb, base, (0, 0, -1, -1));
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn selection_wave_and_draw_laser_with_extreme_geometry() {
            let mut g = Guarded::new(20, 20);
            let strokes = vec![stroke(
                vec![(i32::MIN, 0), (0, 0), (i32::MAX, i32::MAX)],
                Brush::Water,
                8.0,
            )];
            let selected = vec![0usize, 99usize]; // 99 is out of range — must be skipped, not panic
            let mut pb = g.pixel_buf();
            selection_wave(&mut pb, &strokes, &selected, 42);
            let runs = vec![LaserRun {
                pen: super::super::Pen {
                    brush: Brush::Marker,
                    color: 0xFFFFFF,
                    width: 5.0,
                },
                pts: vec![
                    (i32::MIN, i32::MIN, Instant::now()),
                    (i32::MAX, i32::MAX, Instant::now()),
                    (10, 10, Instant::now()),
                ],
            }];
            draw_laser(&mut pb, &runs, Instant::now());
            drop(pb);
            g.assert_guard_intact();
        }

        #[test]
        fn ping_family_extreme_geometry_stays_in_guard_band() {
            let mut g = Guarded::new(20, 20);
            for &(cx, cy) in &[(i32::MIN, i32::MIN), (i32::MAX, i32::MAX), (10, 10), (-500, 500)] {
                let mut pb = g.pixel_buf();
                for kind in PingKind::WHEEL {
                    draw_ping(&mut pb, cx, cy, kind, 0xFF00AA, 50.0);
                    draw_ping_wheel(&mut pb, cx, cy, 2, 0x00FFAA, 7);
                }
                drop(pb);
            }
            g.assert_guard_intact();
        }

        #[test]
        fn ping_family_zero_size_buffer_is_noop() {
            let mut g = Guarded::new(0, 0);
            let mut pb = g.pixel_buf();
            draw_ping(&mut pb, 0, 0, PingKind::Bang, 0xFFFFFF, 10.0);
            draw_ping_wheel(&mut pb, 0, 0, 0, 0xFFFFFF, 1);
            drop(pb);
            g.assert_guard_intact();
        }

        /// The refactor's headline guarantee, demonstrated directly: a `PixelBuf` can only be
        /// built from a slice whose length actually matches the `w`/`h` it's paired with — a
        /// caller holding stale dimensions relative to its buffer cannot construct one at all,
        /// so there is no `(pointer, w, h)` triple left to go stale against each other.
        #[test]
        #[should_panic(expected = "slice len")]
        fn stale_dimensions_cannot_construct_a_pixel_buf() {
            let mut canvas = vec![0u32; 64 * 64]; // the REAL buffer size
            let stale_w = 128; // a caller's cached width from before a resize
            let stale_h = 128;
            let _ = PixelBuf::new(&mut canvas, stale_w, stale_h);
        }
    }
}

// ── the CANVAS PALETTE — a BASE panel with a floating tool DOCK beside it, on its own thread
// (so a PINNED palette stays up and live while you draw). One layered window holds both panels;
// the gap between them is fully transparent — and transparent pixels of a layered window are
// CLICK-THROUGH, so the gap is honest air. Dragging the base's window bar moves everything
// together; pin disables the fade and the click-outside dismiss. ──
#[cfg(windows)]
mod palette {
    use super::{
        board_save, unpack_pos, BoardState, Cmd, BRUSHES, PALETTE, PAL_PIN, PAL_POS, SIZES,
    };
    use crate::ui::AppWindow;
    use std::sync::atomic::Ordering::SeqCst;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};
    use windows_sys::Win32::Foundation::RECT;

    /// The dock (vertical tools), the air gap, the base (everything else).
    const DOCK_W: i32 = 46;
    const GAPX: i32 = 8;
    const BASE_W: i32 = 220;
    /// Base panel x-origin.
    const BX: i32 = DOCK_W + GAPX;
    const W: i32 = BX + BASE_W;
    const TITLE_H: i32 = 26;
    // taller now: the tri-button, the veil/laser toggles, end-session, then the footer hints.
    const BASE_H: i32 = 232;
    const DOCK_H: i32 = 278;
    const H: i32 = DOCK_H;
    /// idle (no hover, no click) before an UNPINNED palette fades itself away.
    const FADE_AFTER_MS: u128 = 6000;

    pub enum PCmd {
        /// Show (or re-anchor) the palette for this session's shared state.
        Show {
            weak: slint::Weak<AppWindow>,
            state: Arc<Mutex<BoardState>>,
        },
        /// The session ended — the palette leaves with it.
        SessionEnded,
    }

    /// Ask the palette to appear (3×click). Safe from any thread.
    pub fn show(weak: slint::Weak<AppWindow>, state: Arc<Mutex<BoardState>>) {
        send_cmd(PCmd::Show { weak, state });
    }

    /// The session is over — hide the palette (pinned or not).
    pub fn session_ended() {
        send_cmd(PCmd::SessionEnded);
    }

    fn tx() -> Option<Sender<PCmd>> {
        static TX: crate::worker::Service<PCmd> = crate::worker::Service::new();
        crate::worker::service_sender(&TX, "neuron-board-palette", palette_thread)
    }

    /// Send a palette command, starting the worker on demand; silently no-ops if it can't start.
    fn send_cmd(cmd: PCmd) {
        if let Some(t) = tx() {
            let _ = t.send(cmd);
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Item {
        /// one of the four media (marker / crayon / water / chalk) — the dock
        BrushK(usize),
        /// a thickness stop (the dock's dot column)
        Size(usize),
        Swatch(usize),
        /// a PRESET pen slot: click = pick it up, right-click = park your current pen in it
        Preset(usize),
        // the TRI-BUTTON: undo · redo · clear fused into one control (three adjacent zones under a
        // single frame). One place for "take it back / put it back / wipe it" — no scattered buttons.
        Undo,
        Redo,
        Clear,
        /// hide the INK (it stays; the next stroke shows it) — named so nobody mistakes it
        /// for closing this panel. Closing the panel is [`Item::Close`] (the ×).
        Veil,
        /// LASER (presentation) mode toggle — a core function, not a preference: point with a
        /// fading trail, ping instead of undo. Lives beside Veil as a state toggle.
        Laser,
        Quit,
        /// keep the palette up while drawing (no fade, no click-outside dismiss)
        Pin,
        Close,
    }

    fn items() -> Vec<(RECT, Item)> {
        let r = |l, t, rr, b| RECT {
            left: l,
            top: t,
            right: rr,
            bottom: b,
        };
        let mut v = vec![
            (r(W - 46, 5, W - 28, 21), Item::Pin),
            (r(W - 24, 5, W - 8, 21), Item::Close),
            // the TRI-BUTTON: undo · redo · clear, three adjacent zones (no air between) under one
            // shared frame so they read as a single control — the painter draws the frame once.
            (r(BX + 4, 106, BX + 74, 132), Item::Undo),
            (r(BX + 74, 106, BX + 144, 132), Item::Redo),
            (r(BX + 144, 106, BX + 216, 132), Item::Clear),
            // beneath the tri-button: the two state toggles, then end-session full-width below.
            (r(BX + 4, 140, BX + 106, 164), Item::Veil),
            (r(BX + 112, 140, BX + 216, 164), Item::Laser),
            (r(BX + 4, 170, BX + 216, 194), Item::Quit),
        ];
        for i in 0..PALETTE.len() {
            let x = BX + 4 + i as i32 * 27;
            v.push((r(x, 34, x + 24, 58), Item::Swatch(i)));
        }
        for i in 0..4usize {
            let x = BX + 4 + i as i32 * 53;
            v.push((r(x, 66, x + 50, 98), Item::Preset(i)));
        }
        // the DOCK: media cells (5 now — marker/crayon/water/chalk/ichor), tighter pitch so the
        // fifth still clears the hairline before the size dots, then the size dots beneath it.
        for i in 0..BRUSHES.len() {
            let y = 4 + i as i32 * 33;
            v.push((r(3, y, DOCK_W - 3, y + 29), Item::BrushK(i)));
        }
        for i in 0..SIZES.len() {
            let y = 180 + i as i32 * 24;
            v.push((r(8, y, DOCK_W - 8, y + 22), Item::Size(i)));
        }
        v
    }

    /// The base panel's window-bar strip is the drag handle (HTCAPTION) — except over the pin
    /// and × cells, which must stay clickable. Everything else is plain client area for the
    /// geometry-polled buttons. The class cursor is a real arrow (a null class cursor is why
    /// the old palette showed the busy spinner forever).
    unsafe extern "system" fn pal_proc(
        hwnd: windows_sys::Win32::Foundation::HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        use windows_sys::Win32::Foundation::RECT as WRECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, GetWindowRect};
        const WM_NCHITTEST: u32 = 0x0084;
        const HTCAPTION: isize = 2;
        const HTCLIENT: isize = 1;
        if msg == WM_NCHITTEST {
            unsafe {
                let mut r: WRECT = std::mem::zeroed();
                if GetWindowRect(hwnd, &mut r) != 0 {
                    let x = (lparam & 0xFFFF) as i16 as i32 - r.left;
                    let y = ((lparam >> 16) & 0xFFFF) as i16 as i32 - r.top;
                    let in_controls = (5..=21).contains(&y) && x >= W - 46;
                    if x >= BX && y < TITLE_H && !in_controls {
                        return HTCAPTION;
                    }
                }
            }
            return HTCLIENT;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// The palette engine: one persistent layered window on one thread, blocking on the channel
    /// while hidden (zero idle cost), polling hover/clicks while shown. PINNED = no fade, no
    /// click-outside dismiss, survives across strokes; dragging the window bar moves base+dock
    /// together and the parked spot is remembered (board.toml).
    fn palette_thread(rx: Receiver<PCmd>) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetCursorPos, PeekMessageW, SetWindowPos, ShowWindow,
            TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SW_HIDE,
            SW_SHOWNOACTIVATE,
        };
        unsafe {
            // the palette window + its DIB live in one LayeredSurface (the drag hit-test proc +
            // arrow cursor + ex_style captured by Surface::new).
            let surface = match Surface::new() {
                Some(s) => s,
                None => return,
            };
            let hwnd = surface.hwnd();
            let its = items();
            let mut ctx: Option<(slint::Weak<AppWindow>, Arc<Mutex<BoardState>>)> = None;
            let mut visible = false;
            let mut hover: Option<Item> = None;
            let mut last_use = std::time::Instant::now();
            let mut lmb_was = false;
            let mut rmb_was = false;
            let mut sca: u32 = 255; // fade ramp (SourceConstantAlpha)
            let mut expected: (i32, i32) = (0, 0);
            // the POP: a subtle fade-pop on appear (pop_in) and a quick fade-pop on intentional
            // dismiss (closing) — alive, but no bounce, so the static instrument feel survives.
            let mut pop_in = false;
            let mut closing = false;
            let mut frame_panics = 0u32; // per-frame repaint panic streak (contain_frame throttle)

            loop {
                // hidden = block (zero idle cost); shown = poll
                let cmd = if visible {
                    rx.try_recv().ok()
                } else {
                    match rx.recv() {
                        Ok(c) => Some(c),
                        Err(_) => return,
                    }
                };
                match cmd {
                    Some(PCmd::Show { weak, state }) => {
                        ctx = Some((weak, state));
                        // pinned: come back exactly where you parked it; unpinned: at the hand
                        let at = match unpack_pos(PAL_POS.load(SeqCst)) {
                            Some(p) if PAL_PIN.load(SeqCst) => p,
                            _ => {
                                let mut c = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
                                GetCursorPos(&mut c);
                                (c.x + 14, c.y + 14)
                            }
                        };
                        let (wl, wt, wr, wb) = crate::teleport::work_area_of((at.0, at.1));
                        let x = at.0.clamp(wl, (wr - W).max(wl));
                        let y = at.1.clamp(wt, (wb - H).max(wt));
                        SetWindowPos(hwnd, HWND_TOPMOST, x, y, W, H, SWP_NOACTIVATE);
                        expected = (x, y);
                        visible = true;
                        // pop IN: start faint and ramp up over a few frames (a re-show mid-close
                        // cancels the close). Subtle — a quick fade, not a slide.
                        sca = 70;
                        pop_in = true;
                        closing = false;
                        last_use = std::time::Instant::now();
                        lmb_was = neuron::glyph::key_down(0x01);
                        rmb_was = neuron::glyph::key_down(0x02);
                        if let Some((_, state)) = ctx.as_ref() {
                            let st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            surface.paint(&its, hover, &st, sca);
                        }
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        continue;
                    }
                    Some(PCmd::SessionEnded) => {
                        ShowWindow(hwnd, SW_HIDE);
                        visible = false;
                        ctx = None;
                        continue;
                    }
                    None => {}
                }
                if !visible {
                    continue;
                }
                let Some((weak, state)) = ctx.as_ref() else {
                    visible = false;
                    continue;
                };

                // pump (HTCAPTION drags run their modal loop in here)
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                // a finished drag parks the palette (remembered, pinned or not)
                let mut wr: windows_sys::Win32::Foundation::RECT = std::mem::zeroed();
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut wr);
                if (wr.left, wr.top) != expected && !neuron::glyph::key_down(0x01) {
                    expected = (wr.left, wr.top);
                    PAL_POS.store(super::pack_pos(wr.left, wr.top), SeqCst);
                    let st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    board_save(&st.pen, &st.presets);
                    last_use = std::time::Instant::now();
                }

                // hover + clicks by geometry, against the LIVE window position
                let mut p = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                let (lx, ly) = (p.x - wr.left, p.y - wr.top);
                let over = (0..W).contains(&lx) && (0..H).contains(&ly);
                let now_hover = if over {
                    its.iter()
                        .find(|(r, _)| lx >= r.left && lx < r.right && ly >= r.top && ly < r.bottom)
                        .map(|(_, i)| *i)
                } else {
                    None
                };
                if over && !closing {
                    last_use = std::time::Instant::now();
                    if sca < 255 && !pop_in {
                        sca = 255; // a returning hand un-fades instantly (but lets a pop-in finish)
                    }
                }
                let mut repaint = now_hover != hover;
                hover = now_hover;

                let lmb = neuron::glyph::key_down(0x01);
                let rmb = neuron::glyph::key_down(0x02);
                if rmb && !rmb_was {
                    if let Some(Item::Preset(i)) = now_hover {
                        // RIGHT-CLICK a preset = park your current pen in it. Slot 1 is also
                        // the pen every fresh session opens with — parking there sets your
                        // default.
                        last_use = std::time::Instant::now();
                        let mut st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        let pen = st.pen;
                        if let Some(slot) = st.presets.get_mut(i) {
                            *slot = pen;
                        }
                        board_save(&st.pen, &st.presets);
                        drop(st);
                        super::post_status(
                            weak,
                            if i == 0 {
                                "pen parked in preset 1 \u{2014} your new default".into()
                            } else {
                                format!("pen parked in preset {}", i + 1)
                            },
                        );
                        repaint = true;
                    }
                }
                if lmb && !lmb_was && !closing {
                    if let Some(it) = now_hover {
                        last_use = std::time::Instant::now();
                        let mut st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                        match it {
                            Item::BrushK(i) => {
                                st.pen.brush = BRUSHES[i % BRUSHES.len()];
                                // a pen change also re-skins whatever is selected — the palette
                                // is the selection's action set too (a no-op with none selected).
                                super::canvas_send(Cmd::RebrushSelection(st.pen.brush));
                                board_save(&st.pen, &st.presets);
                            }
                            Item::Size(i) => {
                                st.pen.width = SIZES[i % SIZES.len()];
                                super::canvas_send(Cmd::ResizeSelection(st.pen.width));
                                board_save(&st.pen, &st.presets);
                            }
                            Item::Swatch(i) => {
                                st.pen.color = PALETTE[i % PALETTE.len()];
                                // a swatch click also recolours whatever is selected — the
                                // palette doubles as the selection action set.
                                super::canvas_send(Cmd::RecolorSelection(st.pen.color));
                                board_save(&st.pen, &st.presets);
                            }
                            Item::Preset(i) => {
                                if let Some(slot) = st.presets.get(i).copied() {
                                    st.pen = slot;
                                    // a preset is the whole pen — selected ink takes all of it.
                                    super::canvas_send(Cmd::RebrushSelection(slot.brush));
                                    super::canvas_send(Cmd::ResizeSelection(slot.width));
                                    super::canvas_send(Cmd::RecolorSelection(slot.color));
                                    board_save(&st.pen, &st.presets);
                                    let line = format!(
                                        "preset {} \u{00b7} {} #{:06X} w{:.1}",
                                        i + 1,
                                        st.pen.brush.name(),
                                        st.pen.color,
                                        st.pen.width
                                    );
                                    drop(st);
                                    super::post_status(weak, line);
                                    st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                                }
                            }
                            Item::Undo => {
                                super::canvas_send(Cmd::Undo);
                            }
                            Item::Redo => {
                                super::canvas_send(Cmd::Redo);
                            }
                            Item::Clear => {
                                super::canvas_send(Cmd::Clear);
                            }
                            Item::Veil => {
                                st.veiled = !st.veiled;
                                let veiled = st.veiled;
                                super::canvas_send(Cmd::Visible(!veiled));
                                drop(st);
                                super::post_status(
                                    weak,
                                    if veiled {
                                        "ink veiled \u{2014} still there; unveil (or any stroke) brings it back".into()
                                    } else {
                                        "ink unveiled".into()
                                    },
                                );
                                st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            }
                            Item::Laser => {
                                st.laser = !st.laser;
                                let on = st.laser;
                                if !on {
                                    // leaving presentation mode: make sure no wheel is left up
                                    super::canvas_send(Cmd::PingWheel(None));
                                    super::canvas_send(Cmd::LaserLift);
                                }
                                drop(st);
                                super::post_status(
                                    weak,
                                    if on {
                                        "laser ON \u{2014} hold to point, tap to ping, hold-in-place for the ping wheel".into()
                                    } else {
                                        "laser off \u{2014} back to ink".into()
                                    },
                                );
                                st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            }
                            Item::Pin => {
                                let pinned = !PAL_PIN.load(SeqCst);
                                PAL_PIN.store(pinned, SeqCst);
                                // pinning HERE parks it here
                                PAL_POS.store(super::pack_pos(wr.left, wr.top), SeqCst);
                                board_save(&st.pen, &st.presets);
                                drop(st);
                                super::post_status(
                                    weak,
                                    if pinned {
                                        "palette pinned \u{2014} it stays while you draw".into()
                                    } else {
                                        "palette unpinned".into()
                                    },
                                );
                                st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            }
                            Item::Quit => {
                                drop(st);
                                super::request_close(); // the session sees it and winds down
                                closing = true; // pop the panel out as the session ends
                                continue;
                            }
                            Item::Close => {
                                drop(st);
                                closing = true; // pop THIS panel out, nothing else
                                continue;
                            }
                        }
                        drop(st);
                        repaint = true;
                    } else if !over && !PAL_PIN.load(SeqCst) {
                        // a real click elsewhere = done with the palette (unless pinned) — pop out
                        closing = true;
                    }
                }
                lmb_was = lmb;
                rmb_was = rmb;

                // POP-IN: ramp the alpha up to full over a few frames (the subtle "into existence").
                if pop_in {
                    sca = (sca + 48).min(255);
                    repaint = true;
                    if sca >= 255 {
                        pop_in = false;
                    }
                }
                // POP-OUT: an intentional dismiss fades quickly, then hides for real (the "out of
                // existence"). Distinct from the slow idle excuse-fade below.
                if closing {
                    sca = sca.saturating_sub(48);
                    repaint = true;
                    if sca == 0 {
                        ShowWindow(hwnd, SW_HIDE);
                        visible = false;
                        closing = false;
                        sca = 255;
                        continue;
                    }
                } else if !PAL_PIN.load(SeqCst) && last_use.elapsed().as_millis() > FADE_AFTER_MS {
                    // the fade: an UNPINNED palette excuses itself after a quiet while
                    sca = sca.saturating_sub(22);
                    repaint = true;
                    if sca == 0 {
                        ShowWindow(hwnd, SW_HIDE);
                        visible = false;
                        sca = 255;
                        continue;
                    }
                }
                if repaint {
                    let st = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    // the per-frame repaint (raw GDI/pixel work — the real panic surface here; the
                    // command match above uses `continue`, so it can't cross a closure and its state
                    // bookkeeping is low-risk anyway) on its own hot lane: a panicked frame is a
                    // dropped frame, throttled so a deterministic paint panic can't storm the log.
                    crate::worker::contain_frame("neuron-board-palette", &mut frame_panics, || {
                        surface.paint(&its, hover, &st, sca)
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }

    /// The palette's drawing surface: a 32-bit DIB presented via UpdateLayeredWindow, so the
    /// gap between base and dock is REAL transparency (and click-through). GDI draws the
    /// furniture; a per-panel alpha pass makes the panels opaque; the brush previews are
    /// rendered with the actual media engine (`imp::brush_alpha`) — the dock shows the ink
    /// you'll actually get, in your current colour.
    struct Surface {
        surf: crate::surface::LayeredSurface,
        mem: windows_sys::Win32::Graphics::Gdi::HDC,
        bits: *mut u32,
    }

    impl Surface {
        /// Build the palette's layered window (a draggable, non-click-through topmost popup with the
        /// drag hit-test proc + arrow cursor) and its W×H BGRA DIB. `None` only if the window can't
        /// be created (the thread then bails).
        fn new() -> Option<Self> {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                LoadCursorW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            };
            unsafe {
                let mut spec = crate::surface::SurfaceSpec::new(
                    "NeuronPalette",
                    WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                    W,
                    H,
                );
                spec.wndproc = Some(pal_proc);
                spec.cursor = LoadCursorW(std::ptr::null_mut(), 32512 as _); // IDC_ARROW
                let surf = crate::surface::LayeredSurface::new(&spec)?;
                let mem = surf.mem();
                let bits = surf.bits();
                Some(Surface { surf, mem, bits })
            }
        }

        #[inline]
        fn hwnd(&self) -> windows_sys::Win32::Foundation::HWND {
            self.surf.hwnd()
        }

        fn paint(&self, its: &[(RECT, Item)], hover: Option<Item>, st: &BoardState, sca: u32) {
            use windows_sys::Win32::Foundation::SIZE;
            use windows_sys::Win32::Graphics::Gdi::{
                CreateFontW, CreateSolidBrush, DeleteObject, Ellipse, FillRect, FrameRect,
                SelectObject, SetBkMode, SetTextColor, TextOutW, CLEARTYPE_QUALITY, DEFAULT_CHARSET,
                FW_BOLD, TRANSPARENT,
            };
            unsafe {
                let dc = self.mem;
                std::ptr::write_bytes(self.bits, 0, (W * H) as usize);
                let pinned = PAL_PIN.load(SeqCst);
                let bg = CreateSolidBrush(rgb(0x0c0d10));
                let line = CreateSolidBrush(rgb(0x1a1e25));
                let hov = CreateSolidBrush(rgb(0x14171c));
                let accent = CreateSolidBrush(rgb(0x4af2b0));
                let white = CreateSolidBrush(rgb(0xffffff));
                let danger = CreateSolidBrush(rgb(0xf25a6b));
                let base_rect = RECT {
                    left: BX,
                    top: 0,
                    right: W,
                    bottom: BASE_H,
                };
                let dock_rect = RECT {
                    left: 0,
                    top: 0,
                    right: DOCK_W,
                    bottom: DOCK_H,
                };
                FillRect(dc, &base_rect, bg);
                FrameRect(dc, &base_rect, line);
                FillRect(dc, &dock_rect, bg);
                FrameRect(dc, &dock_rect, line);
                // base window-bar: a hairline under the title says "this strip is the handle"
                let bar = RECT {
                    left: BX,
                    top: TITLE_H - 1,
                    right: W,
                    bottom: TITLE_H,
                };
                FillRect(dc, &bar, line);
                // dock separator between media and sizes
                let sep = RECT {
                    left: 6,
                    top: 172,
                    right: DOCK_W - 6,
                    bottom: 173,
                };
                FillRect(dc, &sep, line);

                let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
                let font = CreateFontW(
                    -12,
                    0,
                    0,
                    0,
                    FW_BOLD as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    face.as_ptr(),
                );
                let small = CreateFontW(
                    -10,
                    0,
                    0,
                    0,
                    400,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET as u32,
                    0,
                    0,
                    CLEARTYPE_QUALITY as u32,
                    0,
                    face.as_ptr(),
                );
                let old_font = SelectObject(dc, font as _);
                SetBkMode(dc, TRANSPARENT as i32);
                // nameplate
                let tick = RECT {
                    left: BX + 8,
                    top: 8,
                    right: BX + 11,
                    bottom: 19,
                };
                FillRect(dc, &tick, accent);
                SetTextColor(dc, rgb(0xd7dde6));
                let title: Vec<u16> = "BOARD".encode_utf16().collect();
                TextOutW(dc, BX + 17, 6, title.as_ptr(), title.len() as i32);
                // footer hints — in LASER mode the gestures re-skin, so the hint follows suit.
                SelectObject(dc, small as _);
                SetTextColor(dc, rgb(0x5b6470));
                let hints: [&str; 2] = if st.laser {
                    [
                        "LASER \u{00b7} hold = point \u{00b7} tap = ping \u{00b7} hold-in-place = ping wheel",
                        "no ink kept \u{2014} toggle laser off to draw again",
                    ]
                } else {
                    [
                        "hold ink \u{00b7} 2\u{00d7}hold command \u{00b7} tap undo \u{00b7} hold redo",
                        "right-click a preset = park your pen there",
                    ]
                };
                for (row, hint) in hints.iter().enumerate() {
                    let wide: Vec<u16> = hint.encode_utf16().collect();
                    TextOutW(
                        dc,
                        BX + 8,
                        200 + row as i32 * 14,
                        wide.as_ptr(),
                        wide.len() as i32,
                    );
                }

                for (r, it) in its {
                    let active = match it {
                        Item::BrushK(i) => BRUSHES[*i % BRUSHES.len()] == st.pen.brush,
                        Item::Size(i) => (SIZES[*i % SIZES.len()] - st.pen.width).abs() < 0.05,
                        Item::Swatch(i) => PALETTE[*i % PALETTE.len()] == st.pen.color,
                        Item::Preset(i) => {
                            st.presets.get(*i).map(|p| *p == st.pen).unwrap_or(false)
                        }
                        Item::Pin => pinned,
                        _ => false,
                    };
                    if hover == Some(*it) {
                        FillRect(dc, r, hov);
                    }
                    match it {
                        Item::BrushK(_) => {
                            // chrome only — the media preview is painted after the alpha pass
                            FrameRect(dc, r, if active { accent } else { line });
                        }
                        Item::Size(i) => {
                            FrameRect(dc, r, if active { accent } else { line });
                            let rad = [2, 3, 5, 8][*i % 4];
                            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
                            let dot =
                                CreateSolidBrush(rgb(if active { st.pen.color } else { 0x9aa1ac }));
                            let old = SelectObject(dc, dot as _);
                            Ellipse(dc, cx - rad, cy - rad, cx + rad, cy + rad);
                            SelectObject(dc, old);
                            DeleteObject(dot as _);
                        }
                        Item::Swatch(i) => {
                            let sw = CreateSolidBrush(rgb(PALETTE[*i % PALETTE.len()]));
                            let inner = RECT {
                                left: r.left + 2,
                                top: r.top + 2,
                                right: r.right - 2,
                                bottom: r.bottom - 2,
                            };
                            FillRect(dc, &inner, sw);
                            DeleteObject(sw as _);
                            FrameRect(dc, r, if active { white } else { line });
                        }
                        Item::Preset(i) => {
                            FrameRect(dc, r, if active { accent } else { line });
                            if let Some(p) = st.presets.get(*i) {
                                let rad = ((p.width / 2.0).clamp(2.5, 9.0)) as i32;
                                let (cx, cy) = (r.left + 16, (r.top + r.bottom) / 2);
                                let dot = CreateSolidBrush(rgb(p.color));
                                let old = SelectObject(dc, dot as _);
                                Ellipse(dc, cx - rad, cy - rad, cx + rad, cy + rad);
                                SelectObject(dc, old);
                                DeleteObject(dot as _);
                                SelectObject(dc, small as _);
                                SetTextColor(dc, rgb(0x9aa1ac));
                                let init = &p.brush.name()[..1];
                                let wide: Vec<u16> = init.encode_utf16().collect();
                                TextOutW(
                                    dc,
                                    r.left + 32,
                                    r.top + 11,
                                    wide.as_ptr(),
                                    wide.len() as i32,
                                );
                            }
                        }
                        Item::Pin => {
                            SelectObject(dc, small as _);
                            SetTextColor(
                                dc,
                                if pinned {
                                    rgb(0x4af2b0)
                                } else if hover == Some(*it) {
                                    rgb(0xd7dde6)
                                } else {
                                    rgb(0x5b6470)
                                },
                            );
                            let t: Vec<u16> = "pin".encode_utf16().collect();
                            TextOutW(dc, r.left + 1, r.top + 2, t.as_ptr(), t.len() as i32);
                        }
                        Item::Close => {
                            SelectObject(dc, font as _);
                            SetTextColor(
                                dc,
                                if hover == Some(*it) {
                                    rgb(0xd7dde6)
                                } else {
                                    rgb(0x5b6470)
                                },
                            );
                            let x: Vec<u16> = "\u{00d7}".encode_utf16().collect();
                            TextOutW(dc, r.left + 4, r.top + 1, x.as_ptr(), x.len() as i32);
                        }
                        // ── the TRI-BUTTON: undo · redo · clear, one frame, two dividers, centered
                        // labels. The frame + both dividers are re-asserted on every zone
                        // (idempotent) so a hover fill in one zone never eats a neighbour's border. ──
                        Item::Undo | Item::Redo | Item::Clear => {
                            let outer = RECT {
                                left: BX + 4,
                                top: 106,
                                right: BX + 216,
                                bottom: 132,
                            };
                            FrameRect(dc, &outer, line);
                            for dx in [BX + 74, BX + 144] {
                                let div = RECT {
                                    left: dx - 1,
                                    top: 110,
                                    right: dx,
                                    bottom: 128,
                                };
                                FillRect(dc, &div, line);
                            }
                            let label = match it {
                                Item::Undo => "undo",
                                Item::Redo => "redo",
                                _ => "clear",
                            };
                            SelectObject(dc, small as _);
                            SetTextColor(
                                dc,
                                if hover == Some(*it) {
                                    rgb(0xd7dde6)
                                } else {
                                    rgb(0x9aa1ac)
                                },
                            );
                            let wide: Vec<u16> = label.encode_utf16().collect();
                            // centre the label in the zone (small font ≈ 6px/char)
                            let tw = wide.len() as i32 * 6;
                            TextOutW(
                                dc,
                                r.left + ((r.right - r.left - tw) / 2).max(2),
                                r.top + 6,
                                wide.as_ptr(),
                                wide.len() as i32,
                            );
                        }
                        // LASER toggle — a state cell, phosphor when armed (like Pin), so its
                        // on/off reads at a glance without a settings panel.
                        Item::Laser => {
                            let on = st.laser;
                            FrameRect(dc, r, if on { accent } else { line });
                            SelectObject(dc, font as _);
                            SetTextColor(dc, if on { rgb(0x4af2b0) } else { rgb(0x9aa1ac) });
                            let wide: Vec<u16> = "laser".encode_utf16().collect();
                            TextOutW(
                                dc,
                                r.left + 10,
                                r.top + (r.bottom - r.top - 14) / 2,
                                wide.as_ptr(),
                                wide.len() as i32,
                            );
                        }
                        _ => {
                            FrameRect(dc, r, if *it == Item::Quit { danger } else { line });
                            let label = match it {
                                Item::Veil => {
                                    if st.veiled {
                                        "unveil ink"
                                    } else {
                                        "veil ink"
                                    }
                                }
                                Item::Quit => "end session",
                                _ => unreachable!(),
                            };
                            SelectObject(dc, font as _);
                            SetTextColor(
                                dc,
                                if *it == Item::Quit {
                                    rgb(0xf25a6b)
                                } else {
                                    rgb(0x9aa1ac)
                                },
                            );
                            let wide: Vec<u16> = label.encode_utf16().collect();
                            TextOutW(
                                dc,
                                r.left + 10,
                                r.top + (r.bottom - r.top - 14) / 2,
                                wide.as_ptr(),
                                wide.len() as i32,
                            );
                        }
                    }
                }
                SelectObject(dc, old_font);
                DeleteObject(font as _);
                DeleteObject(small as _);
                for b in [bg, line, hov, accent, white, danger] {
                    DeleteObject(b as _);
                }

                // ── alpha pass: panels become opaque; the gap stays REAL transparent air
                // (zero-alpha pixels of a layered window are click-through) ──
                for (rect, hgt) in [(&base_rect, BASE_H), (&dock_rect, DOCK_H)] {
                    for yy in 0..hgt {
                        for xx in rect.left..rect.right {
                            let i = (yy * W + xx) as usize;
                            *self.bits.add(i) |= 0xFF00_0000;
                        }
                    }
                }

                // ── the dock's media previews: REAL ink — each cell is a short curved stroke
                // rendered by the same media shaders the canvas uses, in your current colour
                // (the curve exists so each medium's EDGE character shows, not just its body) ──
                let mt = crate::weave::seconds();
                for (r, it) in its {
                    if let Item::BrushK(i) = it {
                        let brush = BRUSHES[*i % BRUSHES.len()];
                        let cy = (r.top + r.bottom) / 2;
                        let rad = 4.5f32;
                        for xx in (r.left + 4)..(r.right - 4) {
                            let s = (xx - r.left - 4) as f32 * 3.0; // stretched longitude
                            let spine = cy as f32 + 3.0 * ((xx as f32) * 0.22).sin();
                            let y0 = (spine - 9.0) as i32;
                            let y1 = (spine + 9.0) as i32;
                            // ICHOR preview uses the REAL material (swatch = accent); analytic
                            // surface normal from the preview spine. Built once per cell.
                            let material_mat = if brush == super::Brush::DirectedIntent {
                                let mut m = crate::weave::live_material();
                                let pc = st.pen.color;
                                m.accent = (
                                    ((pc >> 16) & 0xFF) as f32 / 255.0,
                                    ((pc >> 8) & 0xFF) as f32 / 255.0,
                                    (pc & 0xFF) as f32 / 255.0,
                                );
                                m.accent_hue = crate::weave::hue_u32(pc);
                                Some(m)
                            } else {
                                None
                            };
                            for yy in y0.max(0)..=y1.min(H - 1) {
                                let d = (yy as f32 - spine).abs();
                                let (c, a) = match &material_mat {
                                    Some(m) if d <= rad + 2.0 => {
                                        let side = if yy as f32 >= spine { 1.0 } else { -1.0 };
                                        super::imp::material_preview_px(d, rad, xx, side, mt, m)
                                    }
                                    _ => (
                                        st.pen.color,
                                        super::imp::brush_alpha(
                                            brush,
                                            false,
                                            0x5EED ^ (*i as u32),
                                            d,
                                            rad,
                                            s,
                                            xx,
                                            yy,
                                        ),
                                    ),
                                };
                                if a == 0 {
                                    continue;
                                }
                                let sr = ((c >> 16) & 0xFF) * a / 255;
                                let sg = ((c >> 8) & 0xFF) * a / 255;
                                let sb = (c & 0xFF) * a / 255;
                                let idx = (yy * W + xx) as usize;
                                let d0 = *self.bits.add(idx);
                                // premultiplied OVER onto the opaque panel
                                let inv = 255 - a;
                                let nr = sr + ((d0 >> 16) & 0xFF) * inv / 255;
                                let ng = sg + ((d0 >> 8) & 0xFF) * inv / 255;
                                let nb = sb + (d0 & 0xFF) * inv / 255;
                                *self.bits.add(idx) = 0xFF00_0000 | (nr << 16) | (ng << 8) | nb;
                            }
                        }
                    }
                }

                // present (position unchanged: None dst point keeps the window where it is); the
                // window-wide constant alpha is the fade level `sca`.
                let size = SIZE { cx: W, cy: H };
                self.surf.present(None, size, sca.min(255) as u8);
            }
        }
    }

    // Teardown is the wrapped `LayeredSurface`'s `Drop` (DIB + mem DC + screen DC + window) — the
    // palette thread loops forever, so this only runs if the thread itself ends.

    /// 0xRRGGBB -> COLORREF (GDI wants 0x00BBGGRR).
    fn rgb(c: u32) -> u32 {
        ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
    }
}
