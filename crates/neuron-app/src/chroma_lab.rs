// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The Chroma lab: a live read of what a native Chroma game is painting, split into the generic
//! scene layers of [`neuron_host::adapters::chroma_scene`] (ambient, key roles, animated keys,
//! recurring effects, the scenes the game rests in), shown under VISITORS on the LIGHTING page.
//!
//! The tap runs beside the native Chroma server and dies with it. It reads the game's own frames
//! (never neuron's composite) and only while something uses the result: the lab on screen, a
//! bound game-light rule, a Game Light layer on a board, a hidden effect, or a scene alert.
//! Otherwise it checks for a game at 2 Hz and reads nothing.
//!
//! What it feeds back, all feedback-only:
//! - `Trigger::GameLight` into the one dispatch engine (which refuses non-feedback actions): an
//!   effect as `#id` / its name, a scene starting as `@id` / its name;
//! - the lighting engine's game feed, which the Game Light layer paints on any device;
//! - a hold frame on the SHM server, so an effect the user hid shows the at-rest picture;
//! - a game notification when a scene the user flagged ends while the game isn't focused.
//!
//! Effects, scenes, and the user's names and flags for them persist per game exe.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use neuron_host::adapters::chroma_analyze::Rgb;
use neuron_host::adapters::chroma_scene::{bin_name, Look, Rest, Template, BIN_ACHROMATIC, BIN_DARKEN, SIG_BINS};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::ui::{
    AppWindow, ChromaLabBar, ChromaLabCell, ChromaLabMark, ChromaLabScene, ChromaLabStrip, ChromaLabSwatch,
    ChromaLabTemplate, State,
};

/// Keyboard grid width the Chroma SDK writes, independent of the physical board.
const KEYBOARD_COLS: usize = 22;
/// How long a lab view keeps the tap reading after the last UI tick.
const VIEW_GRACE: Duration = Duration::from_secs(2);

/// One device class as the lab currently sees it.
#[derive(Clone, Debug, Default)]
pub struct LabDevice {
    pub device_type: u8,
    pub name: &'static str,
    pub cols: usize,
    /// The effect the game last wrote ("custom", "static", "breathing", …).
    pub effect: &'static str,
    pub frame: Vec<Rgb>,
    pub rest: Rest,
    pub templates: Vec<Template>,
    /// When the effect under way started (lab clock, ms).
    pub open_since: Option<u64>,
    /// The template the early guess named for the effect under way.
    pub guess: Option<u32>,
    /// Frames per second the game wrote over the last second.
    pub fps: f32,
    /// When the game last wrote this device (lab clock, ms).
    pub last_frame_ms: Option<u64>,
}

/// A lab log entry.
#[derive(Clone, Debug)]
pub struct LabLine {
    pub at_ms: u64,
    pub text: String,
    pub swatch: Option<Rgb>,
    /// What this line marks on the lab's timeline, if anything.
    pub mark: Option<Mark>,
}

/// A timeline mark: an effect that played (ending at the line's time), or the board settling
/// into a scene.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Effect { id: u32, dur_ms: u32 },
    Scene { id: u32 },
}

/// The user's notes on one recurring effect.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectNote {
    pub id: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// The user's notes on one scene.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneNote {
    pub id: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Notify when the game leaves this scene while it isn't focused.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alert: bool,
}

/// Everything the lab UI renders, published by the tap.
#[derive(Clone, Debug, Default)]
pub struct LabSnapshot {
    /// A tap is running (the native Chroma server is up).
    pub serving: bool,
    /// The connected game's process name, while one is connected.
    pub game: Option<String>,
    pub now_ms: u64,
    pub devices: Vec<LabDevice>,
    pub notes: Vec<EffectNote>,
    pub looks: Vec<Look>,
    pub current_look: Option<u32>,
    pub scene_notes: Vec<SceneNote>,
    /// Seconds of keyboard frames held for "save the last minute".
    pub capture_secs: f32,
    pub log: VecDeque<LabLine>,
}

impl LabSnapshot {
    fn note(&self, id: u32) -> Option<&EffectNote> {
        self.notes.iter().find(|n| n.id == id)
    }
    fn scene_note(&self, id: u32) -> Option<&SceneNote> {
        self.scene_notes.iter().find(|n| n.id == id)
    }
    fn scene_label(&self, id: u32) -> String {
        self.scene_note(id).filter(|n| !n.name.is_empty()).map_or_else(|| format!("@{id}"), |n| n.name.clone())
    }
}

static LAB: Mutex<LabSnapshot> = Mutex::new(LabSnapshot {
    serving: false,
    game: None,
    now_ms: 0,
    devices: Vec::new(),
    notes: Vec::new(),
    looks: Vec::new(),
    current_look: None,
    scene_notes: Vec::new(),
    capture_secs: 0.0,
    log: VecDeque::new(),
});

/// The lab's latest published state.
pub fn snapshot() -> LabSnapshot {
    LAB.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The frames of an effect's most recent play, for replaying it on the lab grid.
type Instance = std::sync::Arc<Vec<(u64, Vec<Rgb>)>>;

static INSTANCES: Mutex<Vec<(u32, Instance)>> = Mutex::new(Vec::new());

fn instance(id: u32) -> Option<Instance> {
    INSTANCES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(i, _)| *i == id).map(|(_, f)| f.clone())
}

/// Requests from the UI, drained by the tap.
#[derive(Clone, Debug)]
enum LabCmd {
    Rename { id: u32, name: String },
    Hide { id: u32, hidden: bool },
    RenameScene { id: u32, name: String },
    AlertScene { id: u32, alert: bool },
    SaveCapture,
    Forget,
}

static COMMANDS: Mutex<VecDeque<LabCmd>> = Mutex::new(VecDeque::new());
static TAP_THREAD: Mutex<Option<std::thread::Thread>> = Mutex::new(None);
static LAST_VIEW: Mutex<Option<Instant>> = Mutex::new(None);
static RULES_LISTEN: AtomicBool = AtomicBool::new(false);

fn send(cmd: LabCmd) {
    COMMANDS.lock().unwrap_or_else(PoisonError::into_inner).push_back(cmd);
    wake();
}

fn wake() {
    if let Some(t) = TAP_THREAD.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        t.unpark();
    }
}

/// The lab is on screen: keep reading frames for a moment.
pub fn note_viewed() {
    let was_idle = LAST_VIEW
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .replace(Instant::now())
        .is_none_or(|t| t.elapsed() >= VIEW_GRACE);
    if was_idle {
        wake();
    }
}

fn viewed_recently() -> bool {
    LAST_VIEW.lock().unwrap_or_else(PoisonError::into_inner).is_some_and(|t| t.elapsed() < VIEW_GRACE)
}

/// Whether any rule binds a game-light trigger. Set by the dispatch worker whenever it rebuilds
/// the engine.
pub fn set_rules_listen(on: bool) {
    RULES_LISTEN.store(on, Ordering::Relaxed);
    if on {
        wake();
    }
}

/// Where a game's effects and scenes are kept: `<run root>/chroma/<exe>.toml`.
fn book_path(game: &str) -> PathBuf {
    let stem: String = game
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    neuron::runroot::run_root().join("chroma").join(format!("{stem}.toml"))
}

#[cfg(windows)]
fn captures_dir() -> PathBuf {
    neuron::runroot::run_root().join("chroma").join("captures")
}

/// One game's saved understanding: templates and looks (so ids survive restarts) and the
/// user's notes on both.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    templates: Vec<Template>,
    #[serde(default)]
    notes: Vec<EffectNote>,
    #[serde(default)]
    looks: Vec<Look>,
    #[serde(default)]
    scene_notes: Vec<SceneNote>,
}

/// Drop templates for single-frame cuts to a new resting picture (saved before the scene
/// stopped templating them), unless the user named or hid one.
fn prune_cuts(book: &mut Book) {
    let noted: Vec<u32> = book.notes.iter().filter(|n| !n.name.is_empty() || n.hidden).map(|n| n.id).collect();
    book.templates.retain(|t| t.signature.dur_ms > 1.0 || noted.contains(&t.id));
}

/// Load a game's book. `Err` carries why an existing file couldn't be read; the caller then
/// refuses to save over it.
#[cfg(windows)]
fn load_book(game: &str) -> Result<Book, String> {
    let path = book_path(game);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Book::default()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

#[cfg(windows)]
fn save_book(game: &str, book: &Book) -> Result<(), String> {
    let path = book_path(game);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = toml::to_string_pretty(book).map_err(|e| e.to_string())?;
    neuron::salvage::atomic_write(&path, text.as_bytes()).map_err(|e| e.to_string())
}

/// Encode keyboard frames as the `NCS1` delta stream the scene fixture test replays: `NCS1`,
/// `u16` cell count, then per frame `u32` ms, `u16` changed cells, and `(cell, r, g, b)` quads.
fn encode_ncs1(frames: &VecDeque<(u64, Vec<Rgb>)>) -> Vec<u8> {
    let cells = frames.front().map_or(0, |f| f.1.len());
    let t0 = frames.front().map_or(0, |f| f.0);
    let mut out = b"NCS1".to_vec();
    out.extend_from_slice(&u16::try_from(cells).unwrap_or(u16::MAX).to_le_bytes());
    let mut prev: Option<&Vec<Rgb>> = None;
    for (t, f) in frames {
        let changed: Vec<usize> = (0..f.len()).filter(|&i| prev.is_none_or(|p| p.get(i) != Some(&f[i]))).collect();
        if changed.is_empty() {
            continue;
        }
        out.extend_from_slice(&u32::try_from(t - t0).unwrap_or(u32::MAX).to_le_bytes());
        out.extend_from_slice(&u16::try_from(changed.len()).unwrap_or(u16::MAX).to_le_bytes());
        for i in changed {
            let c = f[i];
            out.extend_from_slice(&[u8::try_from(i).unwrap_or(u8::MAX), c.0, c.1, c.2]);
        }
        prev = Some(f);
    }
    out
}

/// Encode keyboard frames as a Razer `.chroma` animation (version 1, 2D, device 0 = the 6x22
/// keyboard) in the layout the Chroma SDK's animation files use; not yet opened in Razer's own
/// tools. Each frame lasts until the next; the last lasts one SDK frame (33 ms).
fn encode_chroma(frames: &VecDeque<(u64, Vec<Rgb>)>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&1i32.to_le_bytes());
    out.push(1); // 2D
    out.push(0); // keyboard
    out.extend_from_slice(&i32::try_from(frames.len()).unwrap_or(i32::MAX).to_le_bytes());
    for (k, (t, f)) in frames.iter().enumerate() {
        let next = frames.get(k + 1).map_or(t + 33, |n| n.0);
        let secs = next.saturating_sub(*t) as f32 / 1000.0;
        out.extend_from_slice(&secs.to_le_bytes());
        for i in 0..132 {
            let (r, g, b) = f.get(i).copied().unwrap_or((0, 0, 0));
            out.extend_from_slice(&u32::from_le_bytes([r, g, b, 0]).to_le_bytes());
        }
    }
    out
}

/// The key at a cell of the Chroma SDK's 6x22 keyboard grid (its `RZKEY` row/column layout).
/// Empty where the grid has no key.
fn key_name(cell: usize) -> &'static str {
    const ROWS: [[&str; KEYBOARD_COLS]; 6] = [
        ["", "Esc", "", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "PrtSc", "ScrLk", "Pause", "", "", "logo", ""],
        ["M1", "`", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=", "Backspace", "Insert", "Home", "PgUp", "NumLock", "Num /", "Num *", "Num -"],
        ["M2", "Tab", "Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P", "[", "]", "\\", "Delete", "End", "PgDn", "Num 7", "Num 8", "Num 9", "Num +"],
        ["M3", "Caps", "A", "S", "D", "F", "G", "H", "J", "K", "L", ";", "'", "#", "Enter", "", "", "", "Num 4", "Num 5", "Num 6", ""],
        ["M4", "LShift", "\\", "Z", "X", "C", "V", "B", "N", "M", ",", ".", "/", "", "RShift", "", "Up", "", "Num 1", "Num 2", "Num 3", "Num Enter"],
        ["M5", "LCtrl", "LWin", "LAlt", "", "", "", "Space", "", "", "", "RAlt", "Fn", "Menu", "RCtrl", "Left", "Down", "Right", "", "Num 0", "Num .", ""],
    ];
    ROWS.get(cell / KEYBOARD_COLS).and_then(|r| r.get(cell % KEYBOARD_COLS)).copied().unwrap_or("")
}

#[cfg(windows)]
pub use tap::{start, Tap};

#[cfg(windows)]
mod tap {
    use super::{
        captures_dir, encode_chroma, encode_ncs1, load_book, prune_cuts, save_book, viewed_recently, Book,
        EffectNote, LabCmd, LabDevice, LabLine, LabSnapshot, Mark, SceneNote, COMMANDS, INSTANCES, LAB, RULES_LISTEN,
        TAP_THREAD,
    };
    use neuron::engine::Trigger;
    use neuron::lighting::GameFeed;
    use neuron_host::adapters::chroma_analyze::Rgb;
    use neuron_host::adapters::chroma_scene::{Scene, SceneConfig, SceneEvent};
    use neuron_host::adapters::chroma_shm::server::ShmServer;
    use neuron_host::adapters::chroma_shm::{DeviceClass, Effect};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, PoisonError};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    /// Log lines kept: enough to draw a busy minute on the timeline.
    const LOG_CAP: usize = 160;
    /// Effect recordings kept for replay.
    const INSTANCE_CAP: usize = 64;
    /// Poll period while reading: under the SDK's ~15.6 ms frame quantum.
    const LIVE_POLL: Duration = Duration::from_millis(8);
    /// Poll period while nothing uses the lab: presence checks only.
    const IDLE_POLL: Duration = Duration::from_millis(500);
    const PRESENCE_EVERY_MS: u64 = 500;
    const PUBLISH_EVERY_MS: u64 = 50;
    const SAVE_EVERY_MS: u64 = 5000;
    const CAPTURE_MS: u64 = 60_000;
    /// How long an ended effect lingers in the game feed.
    const FEED_DECAY_MS: u64 = 600;
    /// A hold never outlives this, whatever the scene reports.
    const HOLD_MAX_MS: u64 = 7000;

    struct Watch {
        class: DeviceClass,
        scene: Scene,
        head: Option<u32>,
        effect: &'static str,
        guess: Option<u32>,
        last_frame_ms: Option<u64>,
        fps_window: (u64, u32),
        fps: f32,
    }

    impl Watch {
        fn new(class: DeviceClass) -> Self {
            let (_, rows, cols) = class.layout().grid;
            Watch::with_scene(class, Scene::new(usize::from(rows) * usize::from(cols), SceneConfig::default()))
        }
        fn with_scene(class: DeviceClass, scene: Scene) -> Self {
            Watch { class, scene, head: None, effect: "", guess: None, last_frame_ms: None, fps_window: (0, 0), fps: 0.0 }
        }
    }

    /// The connected game and what the tap keeps about it.
    struct Game {
        pid: u32,
        name: String,
        notes: Vec<EffectNote>,
        scene_notes: Vec<SceneNote>,
        /// Why the saved book couldn't be read, if it couldn't; nothing is saved over it.
        book_error: Option<String>,
        dirty: bool,
        saved_at: u64,
        /// The flagged scene an alert already fired for, until the game settles back into it.
        alerted: Option<u32>,
    }

    impl Game {
        fn hidden(&self, id: u32) -> bool {
            self.notes.iter().any(|n| n.id == id && n.hidden)
        }
        fn label(&self, id: u32) -> Option<&str> {
            self.notes.iter().find(|n| n.id == id && !n.name.is_empty()).map(|n| n.name.as_str())
        }
        fn scene_label(&self, id: u32) -> Option<&str> {
            self.scene_notes.iter().find(|n| n.id == id && !n.name.is_empty()).map(|n| n.name.as_str())
        }
        fn alerts(&self, id: u32) -> bool {
            self.scene_notes.iter().any(|n| n.id == id && n.alert)
        }
        fn note_mut(&mut self, id: u32) -> &mut EffectNote {
            let i = if let Some(i) = self.notes.iter().position(|n| n.id == id) {
                i
            } else {
                self.notes.push(EffectNote { id, ..EffectNote::default() });
                self.notes.len() - 1
            };
            &mut self.notes[i]
        }
        fn scene_note_mut(&mut self, id: u32) -> &mut SceneNote {
            let i = if let Some(i) = self.scene_notes.iter().position(|n| n.id == id) {
                i
            } else {
                self.scene_notes.push(SceneNote { id, ..SceneNote::default() });
                self.scene_notes.len() - 1
            };
            &mut self.scene_notes[i]
        }
        /// Tell the user the game left scene `id`, once per departure, and only while they're
        /// looking at something else (in the game, they can see it themselves). A scene ends when
        /// another settles or the game closes, never on silence: Overwatch stops writing on a
        /// still menu as well as on a loading screen.
        fn alert_departure(&mut self, id: u32, what: &str) {
            if !self.alerts(id) || self.alerted == Some(id) {
                return;
            }
            self.alerted = Some(id);
            let focused = neuron::app::foreground_app().unwrap_or_default().to_lowercase();
            if focused.contains(&self.name.to_lowercase()) {
                return;
            }
            let scene = self.scene_label(id).map_or_else(|| format!("@{id}"), str::to_string);
            neuron::confirm::game_scene(&self.name, &format!("left {scene} · {what}"));
        }
    }

    /// Keyboard effect state that spans frames: a hold in place, whether the trigger already
    /// fired for the effect under way, and the colour fading out of the game feed.
    #[derive(Default)]
    struct Effects {
        holding_since: Option<u64>,
        /// The effect id the trigger already fired for, from the early guess.
        fired: Option<u32>,
        fading: Option<(Rgb, u64)>,
    }

    /// The running tap. Dropping it stops and joins the thread and clears the published lab.
    pub struct Tap {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for Tap {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.thread().unpark();
                let _ = t.join();
            }
            *TAP_THREAD.lock().unwrap_or_else(PoisonError::into_inner) = None;
            *LAB.lock().unwrap_or_else(PoisonError::into_inner) = LabSnapshot::default();
            neuron::lighting::publish_game_feed(None);
        }
    }

    /// Start the tap over `server`. It watches every device class the SDK defines: the lab shows
    /// what the game paints, whatever hardware is attached.
    pub fn start(server: Arc<ShmServer>) -> Option<Tap> {
        let watches: Vec<Watch> = DeviceClass::ALL.into_iter().map(Watch::new).collect();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread =
            neuron::worker::spawn_named("neuron-chroma-lab", move || run(&server, watches, &flag)).ok()?;
        *TAP_THREAD.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread.thread().clone());
        Some(Tap { stop, thread: Some(thread) })
    }

    fn push_mark(log: &mut VecDeque<LabLine>, at_ms: u64, text: String, swatch: Option<Rgb>, mark: Option<Mark>) {
        log.push_front(LabLine { at_ms, text, swatch, mark });
        log.truncate(LOG_CAP);
    }

    fn push_line(log: &mut VecDeque<LabLine>, at_ms: u64, text: String) {
        push_mark(log, at_ms, text, None, None);
    }

    /// Keep the keyboard frames of effect `id`'s play (from just before it started) for replay.
    fn record_instance(capture: &VecDeque<(u64, Vec<Rgb>)>, id: u32, started_ms: u64, now: u64) {
        let frames: Vec<(u64, Vec<Rgb>)> = capture
            .iter()
            .skip_while(|f| f.0 + 20 < started_ms)
            .take_while(|f| f.0 <= now)
            .map(|(t, f)| (t.saturating_sub(started_ms), f.clone()))
            .collect();
        if frames.len() < 2 {
            return;
        }
        let mut all = INSTANCES.lock().unwrap_or_else(PoisonError::into_inner);
        all.retain(|(i, _)| *i != id);
        all.push((id, Arc::new(frames)));
        if all.len() > INSTANCE_CAP {
            all.remove(0);
        }
    }

    fn flush(game: &mut Game, watches: &[Watch], log: &mut VecDeque<LabLine>, now: u64) {
        if !game.dirty || game.book_error.is_some() {
            return;
        }
        game.notes.retain(|n| !n.name.is_empty() || n.hidden);
        game.scene_notes.retain(|n| !n.name.is_empty() || n.alert);
        let kb = &watches[0].scene;
        let book = Book {
            templates: kb.templates().to_vec(),
            notes: game.notes.clone(),
            looks: kb.looks().to_vec(),
            scene_notes: game.scene_notes.clone(),
        };
        match save_book(&game.name, &book) {
            Ok(()) => {
                game.dirty = false;
                game.saved_at = now;
            }
            Err(e) => {
                push_line(log, now, format!("couldn't save {}'s effects: {e}", game.name));
                game.book_error = Some(e);
            }
        }
    }

    /// Fire `Trigger::GameLight` for a thing the game did, by id and (if named) by name.
    fn fire(game: &Game, id_form: String, name: Option<&str>) {
        if !RULES_LISTEN.load(Ordering::Relaxed) {
            return;
        }
        crate::dispatch::inject_trigger(Trigger::GameLight { app: game.name.clone(), effect: id_form });
        if let Some(name) = name {
            crate::dispatch::inject_trigger(Trigger::GameLight { app: game.name.clone(), effect: name.to_string() });
        }
    }

    fn save_capture(g: &Game, capture: &VecDeque<(u64, Vec<Rgb>)>) -> String {
        if capture.len() < 2 {
            return "nothing to save yet: the game hasn't painted while the lab was reading".to_string();
        }
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let stem = format!("{}-{secs}", g.name.to_lowercase());
        let dir = captures_dir();
        let write = |ext: &str, bytes: Vec<u8>| {
            std::fs::create_dir_all(&dir).and_then(|()| neuron::salvage::atomic_write(&dir.join(format!("{stem}.{ext}")), &bytes))
        };
        match write("ncs1", encode_ncs1(capture)).and_then(|()| write("chroma", encode_chroma(capture))) {
            Ok(()) => format!("saved {} frames to {} (.ncs1 and .chroma)", capture.len(), dir.join(&stem).display()),
            Err(e) => format!("capture not saved: {e}"),
        }
    }

    /// Releases the keyboard hold however the tap exits, a panic included; otherwise the board
    /// would keep the held picture until the process ends.
    struct ReleaseHold<'a>(&'a ShmServer);

    impl Drop for ReleaseHold<'_> {
        fn drop(&mut self) {
            self.0.set_hold(DeviceClass::Keyboard.bit(), None);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn run(server: &ShmServer, mut watches: Vec<Watch>, stop: &AtomicBool) {
        let _release = ReleaseHold(server);
        let epoch = Instant::now();
        let clock = || u64::try_from(epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut game: Option<Game> = None;
        let mut presence_at: Option<u64> = None;
        let mut published_at: Option<u64> = None;
        let mut log: VecDeque<LabLine> = VecDeque::new();
        let mut capture: VecDeque<(u64, Vec<Rgb>)> = VecDeque::new();
        let mut fx = Effects::default();
        let mut events = Vec::new();

        while !stop.load(Ordering::Relaxed) {
            let now = clock();

            if presence_at.is_none_or(|t| now.saturating_sub(t) >= PRESENCE_EVERY_MS) {
                presence_at = Some(now);
                let pid = server
                    .any_client_connected()
                    .then(|| server.registered_apps().first().map(|a| a.id))
                    .flatten();
                if pid != game.as_ref().map(|g| g.pid) {
                    if let Some(mut old) = game.take() {
                        if let Some(look) = watches[0].scene.current_look() {
                            old.alert_departure(look, "the game closed");
                        }
                        old.dirty = true;
                        flush(&mut old, &watches, &mut log, now);
                        push_line(&mut log, now, format!("{} left", old.name));
                    }
                    server.set_hold(DeviceClass::Keyboard.bit(), None);
                    fx = Effects::default();
                    capture.clear();
                    neuron::lighting::publish_game_feed(None);
                    INSTANCES.lock().unwrap_or_else(PoisonError::into_inner).clear();
                    for w in &mut watches {
                        *w = Watch::new(w.class);
                    }
                    if let Some(pid) = pid {
                        let name = crate::purge::process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
                        let (mut book, book_error) = match load_book(&name) {
                            Ok(b) => (b, None),
                            Err(e) => {
                                push_line(&mut log, now, format!("couldn't read saved effects, not saving over them: {e}"));
                                (Book::default(), Some(e))
                            }
                        };
                        prune_cuts(&mut book);
                        // Stamps are this tap's clock; anything loaded was last seen in an
                        // earlier session, which 0 marks.
                        book.templates.iter_mut().for_each(|t| t.last_ms = 0);
                        book.looks.iter_mut().for_each(|l| l.last_ms = 0);
                        let (known, scenes) = (book.templates.len(), book.looks.len());
                        let (_, rows, cols) = DeviceClass::Keyboard.layout().grid;
                        let scene = Scene::with_templates(usize::from(rows) * usize::from(cols), SceneConfig::default(), book.templates)
                            .with_looks(book.looks);
                        watches[0] = Watch::with_scene(DeviceClass::Keyboard, scene);
                        let text = if known + scenes > 0 {
                            format!("{name} connected · {known} known effects, {scenes} scenes")
                        } else {
                            format!("{name} connected")
                        };
                        push_line(&mut log, now, text);
                        game = Some(Game {
                            pid,
                            name,
                            notes: book.notes,
                            scene_notes: book.scene_notes,
                            book_error,
                            dirty: false,
                            saved_at: now,
                            alerted: None,
                        });
                    }
                }
            }

            let commands: Vec<LabCmd> = COMMANDS.lock().unwrap_or_else(PoisonError::into_inner).drain(..).collect();
            for cmd in commands {
                let Some(g) = game.as_mut() else { continue };
                g.dirty = true;
                match cmd {
                    LabCmd::Rename { id, name } => g.note_mut(id).name = name.trim().to_string(),
                    LabCmd::Hide { id, hidden } => {
                        g.note_mut(id).hidden = hidden;
                        let what = g.label(id).map_or_else(|| format!("#{id}"), str::to_string);
                        push_line(&mut log, now, format!("{what} {}", if hidden { "hidden" } else { "shown again" }));
                    }
                    LabCmd::RenameScene { id, name } => g.scene_note_mut(id).name = name.trim().to_string(),
                    LabCmd::AlertScene { id, alert } => {
                        g.scene_note_mut(id).alert = alert;
                        g.alerted = None;
                    }
                    LabCmd::Forget => {
                        watches[0] = Watch::new(DeviceClass::Keyboard);
                        g.notes.clear();
                        g.scene_notes.clear();
                        server.set_hold(DeviceClass::Keyboard.bit(), None);
                        fx = Effects::default();
                        INSTANCES.lock().unwrap_or_else(PoisonError::into_inner).clear();
                        push_line(&mut log, now, format!("forgot {}'s effects and scenes", g.name));
                    }
                    LabCmd::SaveCapture => {
                        let text = save_capture(g, &capture);
                        push_line(&mut log, now, text);
                    }
                }
            }

            let listening = game.as_ref().is_some_and(|g| {
                viewed_recently()
                    || RULES_LISTEN.load(Ordering::Relaxed)
                    || neuron::lighting::game_feed_wanted()
                    || g.notes.iter().any(|n| n.hidden)
                    || g.scene_notes.iter().any(|n| n.alert)
            });

            for (wi, w) in watches.iter_mut().enumerate() {
                events.clear();
                if listening {
                    if let Some(head) = server.frame_head(w.class.bit()) {
                        if w.head != Some(head) {
                            w.head = Some(head);
                            if let Some(frame) = server.device_frame(w.class) {
                                w.effect = frame.effect.name();
                                if frame.effect != Effect::None {
                                    let cells = frame.cells_at(now);
                                    w.scene.push(now, &cells, &mut events);
                                    w.last_frame_ms = Some(now);
                                    w.fps_window.1 += 1;
                                    if wi == 0 {
                                        capture.push_back((now, cells));
                                        while capture.front().is_some_and(|f| now.saturating_sub(f.0) > CAPTURE_MS) {
                                            capture.pop_front();
                                        }
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // Re-read the current frame when listening resumes.
                    w.head = None;
                }
                w.scene.tick(now, &mut events);
                if now.saturating_sub(w.fps_window.0) >= 1000 {
                    w.fps = w.fps_window.1 as f32 * 1000.0 / now.saturating_sub(w.fps_window.0).max(1) as f32;
                    w.fps_window = (now, 0);
                }
                let Some(g) = game.as_mut() else { continue };
                for e in &events {
                    handle(server, g, w, wi == 0, &mut fx, &mut log, &capture, now, e);
                }
            }

            if fx.holding_since.is_some_and(|t| now.saturating_sub(t) > HOLD_MAX_MS) {
                server.set_hold(DeviceClass::Keyboard.bit(), None);
                fx.holding_since = None;
            }

            if let Some(g) = game.as_ref() {
                neuron::lighting::publish_game_feed(Some(feed(g, &watches, &fx, now)));
            }
            if let Some(g) = game.as_mut() {
                if g.dirty && now.saturating_sub(g.saved_at) >= SAVE_EVERY_MS {
                    flush(g, &watches, &mut log, now);
                }
            }

            if published_at.is_none_or(|t| now.saturating_sub(t) >= PUBLISH_EVERY_MS) {
                published_at = Some(now);
                let devices = watches
                    .iter()
                    .filter(|w| w.last_frame_ms.is_some())
                    .map(|w| {
                        let (_, _, cols) = w.class.layout().grid;
                        LabDevice {
                            device_type: w.class.bit(),
                            name: w.class.name(),
                            cols: usize::from(cols),
                            effect: w.effect,
                            frame: w.scene.frame().map(<[Rgb]>::to_vec).unwrap_or_default(),
                            rest: w.scene.rest().clone(),
                            templates: w.scene.templates().to_vec(),
                            open_since: w.scene.open_since(),
                            guess: w.guess,
                            fps: w.fps,
                            last_frame_ms: w.last_frame_ms,
                        }
                    })
                    .collect();
                let capture_secs = match (capture.front(), capture.back()) {
                    (Some(a), Some(b)) => (b.0 - a.0) as f32 / 1000.0,
                    _ => 0.0,
                };
                let kb = &watches[0].scene;
                *LAB.lock().unwrap_or_else(PoisonError::into_inner) = LabSnapshot {
                    serving: true,
                    game: game.as_ref().map(|g| g.name.clone()),
                    now_ms: now,
                    devices,
                    notes: game.as_ref().map(|g| g.notes.clone()).unwrap_or_default(),
                    looks: kb.looks().to_vec(),
                    current_look: kb.current_look(),
                    scene_notes: game.as_ref().map(|g| g.scene_notes.clone()).unwrap_or_default(),
                    capture_secs,
                    log: log.clone(),
                };
            }

            std::thread::park_timeout(if listening { LIVE_POLL } else { IDLE_POLL });
        }

        if let Some(mut g) = game.take() {
            g.dirty = true;
            flush(&mut g, &watches, &mut log, clock());
        }
    }

    /// The game feed for the Game Light layer: the keyboard's resting ambient (or the first
    /// class that has one), and the colour of a visible effect while it plays and fades.
    fn feed(g: &Game, watches: &[Watch], fx: &Effects, now: u64) -> GameFeed {
        let to = |c: Rgb| neuron::lighting::Rgb::new(c.0, c.1, c.2);
        let ambient = watches.iter().find_map(|w| w.scene.rest().ambient).map(to);
        let kb = &watches[0];
        let playing = kb.scene.open_since().is_some() && !kb.guess.is_some_and(|id| g.hidden(id));
        let effect = if playing {
            let colour = kb
                .guess
                .and_then(|id| kb.scene.templates().iter().find(|t| t.id == id).map(|t| t.swatch))
                .or_else(|| kb.scene.open_swatch());
            colour.map(|c| (to(c), 1.0))
        } else {
            fx.fading.and_then(|(c, at)| {
                let k = 1.0 - now.saturating_sub(at) as f32 / FEED_DECAY_MS as f32;
                (k > 0.0).then_some((to(c), k))
            })
        };
        GameFeed { ambient, effect }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle(
        server: &ShmServer,
        g: &mut Game,
        w: &mut Watch,
        keyboard: bool,
        fx: &mut Effects,
        log: &mut VecDeque<LabLine>,
        capture: &VecDeque<(u64, Vec<Rgb>)>,
        now: u64,
        e: &SceneEvent,
    ) {
        let dev = w.class.name();
        match *e {
            SceneEvent::Burst { likely, .. } => {
                w.guess = likely;
                if !keyboard {
                    return;
                }
                if let Some(id) = likely {
                    if g.hidden(id) {
                        if let Some(base) = w.scene.baseline() {
                            server.set_hold(DeviceClass::Keyboard.bit(), Some(base.to_vec()));
                            fx.holding_since = Some(now);
                        }
                    }
                    fire(g, format!("#{id}"), g.label(id));
                    fx.fired = Some(id);
                }
            }
            SceneEvent::Effect { id, started_ms, dur_ms, new, truncated, .. } => {
                w.guess = None;
                let swatch = w.scene.templates().iter().find(|t| t.id == id).map(|t| t.swatch);
                if keyboard {
                    if fx.holding_since.take().is_some() {
                        server.set_hold(DeviceClass::Keyboard.bit(), None);
                    }
                    // The early guess can name the wrong effect; the one that actually played
                    // fires once it's known.
                    if fx.fired.take() != Some(id) {
                        fire(g, format!("#{id}"), g.label(id));
                    }
                    fx.fading = swatch.filter(|_| !g.hidden(id)).map(|c| (c, now));
                    g.dirty = true;
                    record_instance(capture, id, started_ms, now);
                }
                let what = g.label(id).map_or_else(|| format!("#{id}"), |n| format!("{n} (#{id})"));
                let secs = f64::from(dur_ms) / 1000.0;
                let cut = if truncated { " (cut)" } else { "" };
                let text = if new {
                    format!("{dev}: new effect {what} · {secs:.1}s{cut}")
                } else {
                    format!("{dev}: {what} played · {secs:.1}s{cut}")
                };
                push_mark(log, now, text, swatch, keyboard.then_some(Mark::Effect { id, dur_ms }));
            }
            SceneEvent::Look { id, from, new } => {
                if !keyboard {
                    return;
                }
                g.dirty = true;
                if let Some(prev) = from {
                    g.alert_departure(prev, "now in another scene");
                }
                if g.alerted == Some(id) {
                    g.alerted = None;
                }
                fire(g, format!("@{id}"), g.scene_label(id));
                let label = g.scene_label(id).map_or_else(|| format!("@{id}"), str::to_string);
                let ambient = w.scene.looks().iter().find(|l| l.id == id).map(|l| l.ambient);
                let text = if new { format!("new scene {label}") } else { format!("scene {label}") };
                push_mark(log, now, text, ambient, Some(Mark::Scene { id }));
            }
            SceneEvent::Ambient { .. } | SceneEvent::Roles { .. } => {}
        }
    }
}

// ── UI ────────────────────────────────────────────────────────────────────────
//
// The lab is one live board plus the things that explain it, all pointing back at the board:
// hovering an at-rest chip spotlights its keys, hovering an effect (a row or a timeline mark)
// replays its last play on the board, and the lens previews live as it's dragged. Words appear
// only where they carry a fact (the hovered key, what is replaying).

fn color(c: Rgb) -> slint::Color {
    slint::Color::from_rgb_u8(c.0, c.1, c.2)
}

fn bin_color(bin: usize) -> slint::Color {
    match bin {
        BIN_ACHROMATIC => slint::Color::from_rgb_u8(0xd8, 0xd8, 0xd8),
        BIN_DARKEN => slint::Color::from_rgb_u8(0x40, 0x40, 0x48),
        _ => {
            let h = bin as f32 / 12.0 * 6.0;
            let x = 1.0 - (h % 2.0 - 1.0).abs();
            let (r, g, b) = match h as u32 {
                0 => (1.0, x, 0.0),
                1 => (x, 1.0, 0.0),
                2 => (0.0, 1.0, x),
                3 => (0.0, x, 1.0),
                4 => (x, 0.0, 1.0),
                _ => (1.0, 0.0, x),
            };
            let to = |v: f32| (40.0 + v * 200.0) as u8;
            slint::Color::from_rgb_u8(to(r), to(g), to(b))
        }
    }
}

fn ago(now: u64, at: u64) -> String {
    let s = now.saturating_sub(at) / 1000;
    match s {
        0 => "now".into(),
        s if s < 60 => format!("{s}s ago"),
        s => format!("{}m ago", s / 60),
    }
}

/// Spotlight groups a board cell belongs to: 0 ambient, `1 + role index` for a held key colour,
/// [`GROUP_ANIMATED`] for an animated key, [`GROUP_NONE`] for anything else (an effect over it).
const GROUP_ANIMATED: i32 = 100;
const GROUP_NONE: i32 = -1;

fn cell_group(i: usize, c: Rgb, rest: &Rest) -> i32 {
    let cell = u16::try_from(i).unwrap_or(u16::MAX);
    if rest.animated.contains(&cell) {
        return GROUP_ANIMATED;
    }
    if let Some(k) = rest.roles.iter().position(|r| r.rgb == c && r.cells.contains(&cell)) {
        return i32::try_from(k).map_or(GROUP_NONE, |k| k + 1);
    }
    if rest.ambient == Some(c) {
        0
    } else {
        GROUP_NONE
    }
}

fn group_word(g: i32) -> &'static str {
    match g {
        0 => "ambient",
        GROUP_ANIMATED => "animated",
        GROUP_NONE => "effect",
        _ => "held colour",
    }
}

/// Plain words for how an effect covers the board and how long it runs.
fn shape_words(dur_ms: f32, uniformity: f32) -> String {
    let spread = if uniformity >= 0.7 {
        "flash"
    } else if uniformity >= 0.35 {
        "sweep"
    } else {
        "wave"
    };
    format!("{spread} {:.1}s", dur_ms / 1000.0)
}

/// Key names for a set of cells, or a count when there are none or too many to read.
fn key_list(cells: &[u16]) -> String {
    let names: Vec<&str> = cells.iter().map(|&c| key_name(usize::from(c))).filter(|k| !k.is_empty()).collect();
    if names.is_empty() || names.len() > 6 {
        format!("{} keys", cells.len())
    } else {
        names.join(" ")
    }
}

fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

thread_local! {
    static CELLS: Rc<VecModel<ChromaLabCell>> = Rc::new(VecModel::default());
    static TEMPLATES: Rc<VecModel<ChromaLabTemplate>> = Rc::new(VecModel::default());
    static SCENES: Rc<VecModel<ChromaLabScene>> = Rc::new(VecModel::default());
    static CHIPS: Rc<VecModel<ChromaLabSwatch>> = Rc::new(VecModel::default());
    static MARKS: Rc<VecModel<ChromaLabMark>> = Rc::new(VecModel::default());
    static STRIPS: Rc<VecModel<ChromaLabStrip>> = Rc::new(VecModel::default());
    /// Per-id models reused while their content holds still: a fresh model every tick would
    /// make every row compare unequal and be re-set 15 times a second.
    static BARS: std::cell::RefCell<Vec<(u32, [f32; SIG_BINS], ModelRc<ChromaLabBar>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static SCENE_KEYS: std::cell::RefCell<Vec<(u32, Vec<Rgb>, ModelRc<slint::Color>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static STRIP_CELLS: std::cell::RefCell<Vec<(u8, Vec<Rgb>, ModelRc<slint::Color>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static REPLAY: std::cell::Cell<Option<(u32, Instant)>> = const { std::cell::Cell::new(None) };
    static NOTED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Update `model` to `rows` in place. Replacing the model would rebuild every row's component
/// on each refresh, wiping hover state and half-typed text.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    let n = model.row_count();
    for (i, r) in rows.iter().enumerate() {
        if i < n {
            if model.row_data(i).as_ref() != Some(r) {
                model.set_row_data(i, r.clone());
            }
        } else {
            model.push(r.clone());
        }
    }
    for _ in rows.len()..n {
        model.remove(rows.len());
    }
}

/// A cached colour-list model keyed by `key`, rebuilt only when `colours` changed.
fn colours_for<K: PartialEq + Copy>(
    cache: &std::cell::RefCell<Vec<(K, Vec<Rgb>, ModelRc<slint::Color>)>>,
    key: K,
    colours: &[Rgb],
) -> ModelRc<slint::Color> {
    let mut cache = cache.borrow_mut();
    if let Some((_, c, m)) = cache.iter().find(|(k, _, _)| *k == key) {
        if c.as_slice() == colours {
            return m.clone();
        }
    }
    let m = ModelRc::new(VecModel::from(colours.iter().map(|&c| color(c)).collect::<Vec<_>>()));
    cache.retain(|(k, _, _)| *k != key);
    cache.push((key, colours.to_vec(), m.clone()));
    m
}

/// The histogram model for template `id`, rebuilt only when its hue signature moved.
fn bars_for(id: u32, hue: &[f32; SIG_BINS]) -> ModelRc<ChromaLabBar> {
    BARS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some((_, h, m)) = cache.iter().find(|(i, _, _)| *i == id) {
            // bit equality: this is a cache key, not a numeric comparison
            if h.iter().zip(hue).all(|(a, b)| a.to_bits() == b.to_bits()) {
                return m.clone();
            }
        }
        let m = ModelRc::new(VecModel::from(
            (0..SIG_BINS).map(|b| ChromaLabBar { h: hue[b], c: bin_color(b) }).collect::<Vec<_>>(),
        ));
        cache.retain(|(i, _, _)| *i != id);
        cache.push((id, *hue, m.clone()));
        m
    })
}

/// The frame of `inst` to show `elapsed` into a looping replay, holding the last frame briefly
/// so the loop reads as "played, then again".
fn replay_frame(inst: &[(u64, Vec<Rgb>)], elapsed: Duration) -> Option<&Vec<Rgb>> {
    let len = inst.last()?.0 + 500;
    let t = u64::try_from(elapsed.as_millis()).unwrap_or(0) % len.max(1);
    inst.iter().take_while(|(ft, _)| *ft <= t).last().or(inst.first()).map(|(_, f)| f)
}

/// The feedback action a rule runs, as the lab's `(kind, param)` pair (see [`lab_action`]).
fn bound_kind(a: &neuron::action::Action) -> (i32, String) {
    use neuron::action::{Action, ObsOp};
    match a {
        Action::Obs { op: ObsOp::Replay, .. } => (1, String::new()),
        Action::Obs { op: ObsOp::Scene, arg } => (2, arg.clone()),
        Action::ProfileSwitch { name } => (3, name.clone()),
        Action::OutputMute { .. } => (4, String::new()),
        _ => (0, String::new()),
    }
}

/// The last minute as timeline marks: scene bands under effect bars, newest on the right.
fn marks(lab: &LabSnapshot) -> Vec<ChromaLabMark> {
    const WINDOW: u64 = 60_000;
    let now = lab.now_ms;
    let t0 = now.saturating_sub(WINDOW);
    let x = |t: u64| (t.saturating_sub(t0) as f32 / WINDOW as f32).clamp(0.0, 1.0);
    let mut out = Vec::new();

    let mut bounds: Vec<&LabLine> =
        lab.log.iter().filter(|l| matches!(l.mark, Some(Mark::Scene { .. }))).collect();
    bounds.reverse();
    let first = bounds.iter().rposition(|l| l.at_ms <= t0).unwrap_or(0);
    for (k, l) in bounds.iter().enumerate().skip(first) {
        let Some(Mark::Scene { id }) = l.mark else { continue };
        let end = bounds.get(k + 1).map_or(now, |n| n.at_ms);
        let (Some(c), true) = (l.swatch, end > t0) else { continue };
        out.push(ChromaLabMark {
            x: x(l.at_ms),
            w: (x(end) - x(l.at_ms)).max(0.0),
            color: color(c),
            band: true,
            id: -1,
            text: format!("{} · from {}", lab.scene_label(id), ago(now, l.at_ms)).into(),
        });
    }
    for l in &lab.log {
        let Some(Mark::Effect { id, dur_ms }) = l.mark else { continue };
        if l.at_ms < t0 {
            break;
        }
        let start = l.at_ms.saturating_sub(u64::from(dur_ms));
        let name = lab
            .note(id)
            .filter(|n| !n.name.is_empty())
            .map_or_else(|| format!("#{id}"), |n| format!("{} #{id}", n.name));
        out.push(ChromaLabMark {
            x: x(start),
            w: (x(l.at_ms) - x(start)).max(0.006),
            color: color(l.swatch.unwrap_or((180, 180, 180))),
            band: false,
            id: i32::try_from(id).unwrap_or(-1),
            text: format!("{name} · {:.1}s · {}", f64::from(dur_ms) / 1000.0, ago(now, l.at_ms)).into(),
        });
    }
    out
}

/// Push the lab's latest state into the UI models. Cheap enough for the lab's 15 fps timer.
#[allow(clippy::too_many_lines)]
pub fn refresh(app: &AppWindow) {
    note_viewed();
    let st = app.global::<State>();
    let lab = snapshot();
    let now = lab.now_ms;
    let kb = lab.devices.iter().find(|d| d.device_type == 0x01);
    let label = |id: u32| lab.note(id).filter(|n| !n.name.is_empty()).map_or_else(|| format!("#{id}"), |n| n.name.clone());

    // ── status: facts while a game paints, otherwise why there's nothing ──
    let focused = st.get_focused_app().to_string();
    let exe = focused.trim().trim_end_matches(".exe").to_string();
    let status = match (&lab.game, lab.serving) {
        (_, false) if !crate::host::active() => "connections are closed · SYSTEM → connections".to_string(),
        (_, false) if !crate::prefs::host_chroma() => "chroma games are off · SYSTEM → connections".to_string(),
        (_, false) => "the native chroma server isn't up · SYSTEM → connections says why".to_string(),
        (None, true) if !exe.is_empty() && exe != "—" && !exe.to_lowercase().starts_with("neuron") => {
            format!("waiting for a chroma game · {exe} isn't painting chroma")
        }
        (None, true) => "waiting for a chroma game".to_string(),
        (Some(_), true) => String::new(),
    };
    st.set_chroma_lab_status(status.into());
    st.set_chroma_lab_game(lab.game.as_deref().map(str::to_lowercase).unwrap_or_default().into());
    let fresh = kb
        .or(lab.devices.first())
        .and_then(|d| d.last_frame_ms)
        .is_some_and(|t| now.saturating_sub(t) < 1000);
    st.set_chroma_lab_fresh(fresh);
    st.set_chroma_lab_fps(kb.map_or(0, |d| d.fps.round() as i32));
    let scene_now = lab.current_look.map(|id| lab.scene_label(id)).unwrap_or_default();
    st.set_chroma_lab_scene(scene_now.into());
    let playing = kb.and_then(|d| d.open_since.map(|_| d.guess));
    st.set_chroma_lab_now(
        match playing {
            Some(Some(id)) => format!("▶ {}", label(id)),
            Some(None) => "▶ effect".to_string(),
            None => String::new(),
        }
        .into(),
    );
    st.set_chroma_lab_capture(if lab.capture_secs >= 1.0 {
        format!("save {:.0}s", lab.capture_secs.min(60.0)).into()
    } else {
        "".into()
    });

    // ── the board: live, replaying an effect, or through the lens ──
    let replay_id = u32::try_from(st.get_chroma_lab_replay())
        .ok()
        .or_else(|| u32::try_from(st.get_chroma_lab_selected()).ok());
    let replay = replay_id.and_then(|id| instance(id).map(|inst| (id, inst)));
    let elapsed = REPLAY.with(|r| match (r.get(), &replay) {
        (Some((id, t)), Some((rid, _))) if id == *rid => t.elapsed(),
        (_, Some((rid, _))) => {
            r.set(Some((*rid, Instant::now())));
            Duration::ZERO
        }
        (_, None) => {
            r.set(None);
            Duration::ZERO
        }
    });
    let lens = neuron_host::paint::Lens {
        hue_shift: st.get_chroma_lens_hue().round().clamp(-180.0, 180.0) as i16,
        saturation: st.get_chroma_lens_saturation().round().clamp(0.0, 200.0) as u8,
        brightness: st.get_chroma_lens_brightness().round().clamp(0.0, 200.0) as u8,
    };
    let compare = st.get_chroma_lab_compare();
    let through_lens = !lens.is_identity() && !compare;
    let lensed = |c: Rgb| {
        if through_lens {
            let o = lens.apply(neuron_host::arbiter::Rgb(c.0, c.1, c.2));
            (o.0, o.1, o.2)
        } else {
            c
        }
    };
    let shown: Option<&Vec<Rgb>> = match &replay {
        Some((_, inst)) => replay_frame(inst, elapsed),
        None => kb.map(|d| &d.frame),
    };
    let cells: Vec<ChromaLabCell> = match (shown, kb) {
        (Some(frame), Some(d)) => frame
            .iter()
            .enumerate()
            .map(|(i, &c)| ChromaLabCell { color: color(lensed(c)), group: cell_group(i, c, &d.rest) })
            .collect(),
        _ => Vec::new(),
    };
    st.set_chroma_lab_cols(i32::try_from(kb.map_or(KEYBOARD_COLS, |d| d.cols)).unwrap_or(1));
    st.set_chroma_lab_kb_effect(kb.map_or("", |d| d.effect).into());
    CELLS.with(|m| sync(m, cells));

    let hover = usize::try_from(st.get_chroma_lab_hover()).ok();
    let readout = match (hover, shown, kb) {
        (Some(i), Some(frame), Some(d)) if i < frame.len() => {
            let c = frame[i];
            let key = match key_name(i) {
                "" => "no key",
                k => k,
            };
            format!("{key} · {} · {}", hex(c), group_word(cell_group(i, c, &d.rest)))
        }
        _ => match &replay {
            Some((id, inst)) => {
                let secs = inst.last().map_or(0.0, |f| f.0 as f64 / 1000.0);
                format!("▶ {} · its last play, {secs:.1}s", label(*id))
            }
            None if !lens.is_identity() && compare => "the game's own colours".to_string(),
            None if !lens.is_identity() => "through your lens · hold the board to compare".to_string(),
            None => String::new(),
        },
    };
    st.set_chroma_lab_readout(readout.into());

    // ── at rest: ambient, each held colour, the animated keys ──
    let chips: Vec<ChromaLabSwatch> = kb
        .map(|d| {
            let mut v = Vec::new();
            if let Some(a) = d.rest.ambient {
                v.push(ChromaLabSwatch { color: color(a), text: "ambient".into(), group: 0 });
            }
            for (k, r) in d.rest.roles.iter().enumerate() {
                v.push(ChromaLabSwatch {
                    color: color(r.rgb),
                    text: key_list(&r.cells).into(),
                    group: i32::try_from(k).map_or(GROUP_NONE, |k| k + 1),
                });
            }
            if !d.rest.animated.is_empty() {
                v.push(ChromaLabSwatch {
                    color: slint::Color::from_argb_u8(0, 0, 0, 0),
                    text: key_list(&d.rest.animated).into(),
                    group: GROUP_ANIMATED,
                });
            }
            v
        })
        .unwrap_or_default();
    CHIPS.with(|m| sync(m, chips));

    // ── every other class the game paints, drawn as its own grid ──
    let strips: Vec<ChromaLabStrip> = lab
        .devices
        .iter()
        .filter(|d| d.device_type != 0x01)
        .map(|d| {
            let cells: Vec<Rgb> = d.frame.iter().map(|&c| lensed(c)).collect();
            ChromaLabStrip {
                name: d.name.into(),
                effect: d.effect.into(),
                cols: i32::try_from(d.cols).unwrap_or(1),
                cells: STRIP_CELLS.with(|c| colours_for(c, d.device_type, &cells)),
            }
        })
        .collect();
    STRIPS.with(|m| sync(m, strips));

    // ── scenes: where the game rests, named once, kept per game ──
    let rules = gui_rules_cached();
    let rule_for = |form: String| {
        lab.game.as_deref().and_then(|g| {
            let trig = neuron::engine::Trigger::GameLight { app: g.to_lowercase(), effect: form };
            rules.iter().find(|r| r.trigger == trig)
        })
    };
    let scene_sel = st.get_chroma_lab_scene_selected();
    let scenes: Vec<ChromaLabScene> = lab
        .looks
        .iter()
        .map(|l| {
            let note = lab.scene_note(l.id);
            let rule = rule_for(format!("@{}", l.id));
            let (bound_k, bound_p) = rule.map_or((0, String::new()), |r| bound_kind(&r.action));
            let num = i32::try_from(l.id).unwrap_or(-1);
            let seen = if l.last_ms == 0 { "earlier".to_string() } else { ago(now, l.last_ms) };
            ChromaLabScene {
                id: format!("@{}", l.id).into(),
                num,
                name: note.map(|n| n.name.clone()).unwrap_or_default().into(),
                ambient: color(l.ambient),
                keys: SCENE_KEYS.with(|c| colours_for(c, l.id, &l.colours)),
                count: i32::try_from(l.count).unwrap_or(i32::MAX),
                current: lab.current_look == Some(l.id),
                alert: note.is_some_and(|n| n.alert),
                selected: num == scene_sel,
                detail: format!("{} · {seen}", if l.count == 1 { "once".to_string() } else { format!("{}×", l.count) }).into(),
                bound: rule.map(|r| r.action.describe()).unwrap_or_default().into(),
                bound_kind: bound_k,
                bound_param: bound_p.into(),
            }
        })
        .collect();
    SCENES.with(|m| sync(m, scenes));

    // ── recurring effects, in id order so an open row stays put ──
    let selected = st.get_chroma_lab_selected();
    let mut templates: Vec<&Template> = kb.map(|d| d.templates.iter().collect()).unwrap_or_default();
    templates.sort_by_key(|t| t.id);
    let rows: Vec<ChromaLabTemplate> = templates
        .iter()
        .take(32)
        .map(|t| {
            let sig = &t.signature;
            let family = sig.dominant_bin().map_or("dim", bin_name);
            let note = lab.note(t.id);
            let guessed = kb.is_some_and(|d| d.guess == Some(t.id));
            let heat = if guessed {
                0.6
            } else if t.last_ms == 0 {
                0.0
            } else {
                (1.0 - now.saturating_sub(t.last_ms) as f32 / 1500.0).clamp(0.0, 1.0)
            };
            let seen = if t.last_ms == 0 { "earlier".to_string() } else { ago(now, t.last_ms) };
            let rule = rule_for(format!("#{}", t.id));
            let (bound_k, bound_p) = rule.map_or((0, String::new()), |r| bound_kind(&r.action));
            let num = i32::try_from(t.id).unwrap_or(-1);
            ChromaLabTemplate {
                id: format!("#{}", t.id).into(),
                num,
                name: note.map(|n| n.name.clone()).unwrap_or_default().into(),
                hidden: note.is_some_and(|n| n.hidden),
                selected: num == selected,
                replayable: instance(t.id).is_some(),
                swatch: color(t.swatch),
                count: i32::try_from(t.count).unwrap_or(i32::MAX),
                detail: format!("{family} {} · {seen}", shape_words(sig.dur_ms, sig.uniformity)).into(),
                bound: rule.map(|r| r.action.describe()).unwrap_or_default().into(),
                bound_kind: bound_k,
                bound_param: bound_p.into(),
                heat,
                bars: bars_for(t.id, &sig.hue),
            }
        })
        .collect();
    TEMPLATES.with(|m| sync(m, rows));

    MARKS.with(|m| sync(m, marks(&lab)));

    // Notices (a game connecting, a capture saved, a save failing) go to the app's status line
    // once each, as they happen; opening the lab doesn't replay old ones.
    let newest = lab.log.iter().find(|l| l.mark.is_none()).map(|l| (l.at_ms, l.text.clone()));
    NOTED.with(|n| {
        if let Some((at, text)) = newest {
            match n.get() {
                Some(seen) if at > seen => {
                    st.set_status_line(text.into());
                    n.set(Some(at));
                }
                None => n.set(Some(at)),
                _ => {}
            }
        }
    });
}

thread_local! {
    static RULES: std::cell::RefCell<Option<(Instant, Vec<neuron::engine::Rule>)>> =
        const { std::cell::RefCell::new(None) };
}

/// The GUI rules, re-read from disk at most once a second (the lab refreshes at 15 Hz).
fn gui_rules_cached() -> Vec<neuron::engine::Rule> {
    RULES.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().is_none_or(|(at, _)| at.elapsed() >= Duration::from_secs(1)) {
            *c = Some((Instant::now(), crate::editor::load_gui_rules()));
        }
        c.as_ref().map(|(_, r)| r.clone()).unwrap_or_default()
    })
}

/// The feedback actions the lab offers, as `(palette id, param)`. `None` unbinds.
fn lab_action(kind: i32, param: &str) -> Option<(&'static str, String)> {
    match kind {
        1 => Some(("obs", "replay".into())),
        2 => Some(("obs", format!("scene {}", param.trim()))),
        3 => Some(("profile", param.trim().to_string())),
        4 => Some(("mute", String::new())),
        _ => None,
    }
}

/// Bind (or with `kind` 0, unbind) what happens when the game does `form` (`#id` for an effect
/// playing, `@id` for a scene starting). Rules key on ids, which persist; names don't.
fn bind(form: String, kind: i32, param: &str) -> String {
    let Some(game) = snapshot().game else { return "no chroma game connected".into() };
    if matches!(kind, 2 | 3) && param.trim().is_empty() {
        return "name the scene or profile first".into();
    }
    let trigger = neuron::engine::Trigger::GameLight { app: game.to_lowercase(), effect: form.clone() };
    let mut rules = match crate::editor::try_load_gui_rules() {
        Ok(r) => r,
        Err(e) => return format!("your bindings file didn't read, so nothing was changed: {e}"),
    };
    rules.retain(|r| r.trigger != trigger);
    let msg = match lab_action(kind, param) {
        None => format!("{form} no longer does anything"),
        Some((palette, p)) => {
            let action = crate::editor::build_action(palette, &p);
            let text = format!("when {form} happens: {}", action.describe());
            rules.push(neuron::engine::Rule::new(trigger, action));
            text
        }
    };
    match crate::editor::save_gui_rules(&rules) {
        Ok(()) => {
            RULES.with(|c| *c.borrow_mut() = None);
            crate::dispatch::request_reload();
            msg
        }
        Err(e) => format!("couldn't save the binding: {e}"),
    }
}

/// Wire the lab's UI callbacks.
pub fn install(app: &AppWindow) {
    let st = app.global::<State>();
    CELLS.with(|m| st.set_chroma_lab_cells(ModelRc::from(Rc::clone(m))));
    TEMPLATES.with(|m| st.set_chroma_lab_templates(ModelRc::from(Rc::clone(m))));
    SCENES.with(|m| st.set_chroma_lab_scenes(ModelRc::from(Rc::clone(m))));
    CHIPS.with(|m| st.set_chroma_lab_roles(ModelRc::from(Rc::clone(m))));
    MARKS.with(|m| st.set_chroma_lab_marks(ModelRc::from(Rc::clone(m))));
    STRIPS.with(|m| st.set_chroma_lab_strips(ModelRc::from(Rc::clone(m))));
    let w = app.as_weak();
    st.on_chroma_lab_tick(move || {
        if let Some(app) = w.upgrade() {
            refresh(&app);
        }
    });
    st.on_chroma_lab_rename(|id, name| {
        if let Ok(id) = u32::try_from(id) {
            send(LabCmd::Rename { id, name: name.to_string() });
        }
    });
    st.on_chroma_lab_scene_rename(|id, name| {
        if let Ok(id) = u32::try_from(id) {
            send(LabCmd::RenameScene { id, name: name.to_string() });
        }
    });
    let w = app.as_weak();
    st.on_chroma_lab_scene_alert(move |id, alert| {
        if let (Ok(id), Some(app)) = (u32::try_from(id), w.upgrade()) {
            send(LabCmd::AlertScene { id, alert });
            let msg = if alert {
                format!("@{id}: you'll get a game notification when it ends while you're in another window")
            } else {
                format!("@{id}: no alert")
            };
            app.global::<State>().set_status_line(msg.into());
        }
    });
    let w = app.as_weak();
    st.on_chroma_lab_hide(move |id, hidden| {
        if let Ok(id) = u32::try_from(id) {
            send(LabCmd::Hide { id, hidden });
            if let Some(app) = w.upgrade() {
                let msg = if hidden {
                    format!("#{id} hidden: your keyboard keeps the at-rest picture about a quarter second in")
                } else {
                    format!("#{id} shown again")
                };
                app.global::<State>().set_status_line(msg.into());
            }
        }
    });
    st.on_chroma_lab_save_capture(|| send(LabCmd::SaveCapture));
    st.on_chroma_lab_forget(|| send(LabCmd::Forget));
    let w = app.as_weak();
    st.on_chroma_lab_bind(move |id, kind, param| {
        if let (Ok(id), Some(app)) = (u32::try_from(id), w.upgrade()) {
            let msg = bind(format!("#{id}"), kind, &param);
            app.global::<State>().set_status_line(msg.into());
        }
    });
    let w = app.as_weak();
    st.on_chroma_lab_scene_bind(move |id, kind, param| {
        if let (Ok(id), Some(app)) = (u32::try_from(id), w.upgrade()) {
            let msg = bind(format!("@{id}"), kind, &param);
            app.global::<State>().set_status_line(msg.into());
        }
    });
    let w = app.as_weak();
    st.on_chroma_lab_set_lens(move |hue, saturation, brightness| {
        let msg = crate::prefs::set_host_chroma_lens(
            hue.round().clamp(-180.0, 180.0) as i16,
            saturation.round().clamp(0.0, 200.0) as u8,
            brightness.round().clamp(0.0, 200.0) as u8,
        );
        crate::host::apply_protocol_prefs();
        if let Some(app) = w.upgrade() {
            let st = app.global::<State>();
            let (h, s, b) = crate::prefs::host_chroma_lens();
            st.set_chroma_lens_hue(f32::from(h));
            st.set_chroma_lens_saturation(f32::from(s));
            st.set_chroma_lens_brightness(f32::from(b));
            st.set_status_line(msg.into());
        }
    });
    let (h, s, b) = crate::prefs::host_chroma_lens();
    st.set_chroma_lens_hue(f32::from(h));
    st.set_chroma_lens_saturation(f32::from(s));
    st.set_chroma_lens_brightness(f32::from(b));
}

#[cfg(test)]
mod tests {
    use super::*;
    use neuron_host::adapters::chroma_scene::Role;

    #[test]
    fn cells_are_grouped_for_the_spotlight() {
        let rest = Rest {
            ambient: Some((55, 30, 0)),
            roles: vec![Role { rgb: (4, 141, 144), cells: vec![48] }, Role { rgb: (222, 153, 0), cells: vec![47] }],
            animated: vec![46],
        };
        assert_eq!(cell_group(0, (55, 30, 0), &rest), 0);
        assert_eq!(cell_group(48, (4, 141, 144), &rest), 1);
        assert_eq!(cell_group(47, (222, 153, 0), &rest), 2);
        assert_eq!(cell_group(46, (35, 216, 237), &rest), GROUP_ANIMATED);
        assert_eq!(cell_group(47, (255, 255, 255), &rest), GROUP_NONE, "a held key under an effect");
    }

    #[test]
    fn a_replay_loops_through_its_frames_then_holds() {
        let inst = vec![(0, vec![(1, 1, 1)]), (100, vec![(2, 2, 2)]), (200, vec![(3, 3, 3)])];
        let at = |ms: u64| replay_frame(&inst, Duration::from_millis(ms)).map(|f| f[0]);
        assert_eq!(at(0), Some((1, 1, 1)));
        assert_eq!(at(150), Some((2, 2, 2)));
        assert_eq!(at(650), Some((3, 3, 3)), "held on the last frame");
        assert_eq!(at(700), Some((1, 1, 1)), "then from the top");
    }

    #[test]
    fn every_bin_has_a_distinct_colour() {
        let colours: Vec<slint::Color> = (0..SIG_BINS).map(bin_color).collect();
        for (i, a) in colours.iter().enumerate() {
            for b in &colours[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    /// The key table must agree with what real Overwatch frames showed: WASD at r2c3/r3c2-4,
    /// E at r2c4, Q (the ult key) at r2c2, left shift at r4c1, the logo LED at r0c20.
    #[test]
    fn the_key_table_matches_the_captured_overwatch_roles() {
        let at = |r: usize, c: usize| key_name(r * KEYBOARD_COLS + c);
        assert_eq!([at(2, 3), at(3, 2), at(3, 3), at(3, 4)], ["W", "A", "S", "D"]);
        assert_eq!(at(2, 4), "E");
        assert_eq!(at(2, 2), "Q");
        assert_eq!(at(4, 1), "LShift");
        assert_eq!(at(0, 20), "logo");
        assert_eq!(key_name(999), "");
    }

    #[test]
    fn a_capture_encodes_as_the_fixture_format() {
        let mut frames = VecDeque::new();
        frames.push_back((1000, vec![(1, 2, 3), (4, 5, 6)]));
        frames.push_back((1016, vec![(1, 2, 3), (9, 9, 9)]));
        frames.push_back((1032, vec![(1, 2, 3), (9, 9, 9)]));
        let b = encode_ncs1(&frames);
        assert_eq!(&b[..4], b"NCS1");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), 2);
        assert_eq!(&b[6..12], &[0, 0, 0, 0, 2, 0], "first frame at t=0 lists both cells");
        assert_eq!(&b[12..20], &[0, 1, 2, 3, 1, 4, 5, 6]);
        assert_eq!(&b[20..26], &[16, 0, 0, 0, 1, 0], "second frame lists only the changed cell");
        assert_eq!(&b[26..30], &[1, 9, 9, 9]);
        assert_eq!(b.len(), 30, "an unchanged frame is skipped");
    }

    #[test]
    fn a_capture_encodes_as_a_razer_chroma_animation() {
        let mut frames = VecDeque::new();
        frames.push_back((1000, vec![(255, 0, 0); 132]));
        frames.push_back((1250, vec![(0, 0, 255); 132]));
        let b = encode_chroma(&frames);
        assert_eq!(i32::from_le_bytes([b[0], b[1], b[2], b[3]]), 1, "version");
        assert_eq!((b[4], b[5]), (1, 0), "2D, keyboard");
        assert_eq!(i32::from_le_bytes([b[6], b[7], b[8], b[9]]), 2, "frames");
        let frame = 4 + 132 * 4;
        assert_eq!(b.len(), 10 + 2 * frame);
        assert_eq!(f32::from_le_bytes([b[10], b[11], b[12], b[13]]), 0.25, "lasts until the next");
        assert_eq!(&b[14..18], &[255, 0, 0, 0], "0x00BBGGRR: red in the low byte");
        let second = 10 + frame;
        assert!((f32::from_le_bytes([b[second], b[second + 1], b[second + 2], b[second + 3]]) - 0.033).abs() < 1e-6);
        assert_eq!(&b[second + 4..second + 8], &[0, 0, 255, 0]);
    }

    #[test]
    fn the_lab_offers_only_feedback_actions() {
        for kind in 1..=4 {
            let (palette, param) = lab_action(kind, "x").expect("an action");
            assert!(crate::editor::build_action(palette, &param).is_feedback(), "kind {kind}");
        }
        assert!(lab_action(0, "").is_none());
    }

    #[test]
    fn a_book_round_trips_through_toml() {
        let book = Book {
            templates: Vec::new(),
            notes: vec![
                EffectNote { id: 3, name: "ult wave".into(), hidden: false },
                EffectNote { id: 5, name: String::new(), hidden: true },
            ],
            looks: vec![Look { id: 0, ambient: (50, 51, 52), colours: vec![(222, 153, 0)], count: 2, last_ms: 0 }],
            scene_notes: vec![SceneNote { id: 0, name: "menu".into(), alert: true }],
        };
        let text = toml::to_string_pretty(&book).expect("serialize");
        let back: Book = toml::from_str(&text).expect("parse");
        assert_eq!(back.notes, book.notes);
        assert_eq!(back.looks, book.looks);
        assert_eq!(back.scene_notes, book.scene_notes);
        let old: Book = toml::from_str("notes = []\n").expect("a book from before scenes existed");
        assert!(old.looks.is_empty() && old.scene_notes.is_empty());
    }

    #[test]
    fn cut_templates_are_pruned_unless_the_user_kept_them() {
        use neuron_host::adapters::chroma_scene::Signature;
        let t = |id: u32, dur_ms: f32| Template {
            id,
            signature: Signature { hue: [0.0; SIG_BINS], dur_ms, uniformity: 0.5 },
            count: 1,
            last_ms: 0,
            swatch: (0, 0, 0),
        };
        let mut book = Book {
            templates: vec![t(0, 1.0), t(1, 4523.0), t(2, 1.0)],
            notes: vec![EffectNote { id: 2, name: "kept".into(), hidden: false }],
            ..Book::default()
        };
        prune_cuts(&mut book);
        let ids: Vec<u32> = book.templates.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn a_book_path_is_one_safe_file_name() {
        let p = book_path("Over/watch?.exe");
        assert_eq!(p.file_name().and_then(|n| n.to_str()), Some("over_watch__exe.toml"));
        assert_eq!(p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()), Some("chroma"));
    }

    #[test]
    fn scene_labels_fall_back_to_their_id() {
        let lab = LabSnapshot {
            scene_notes: vec![SceneNote { id: 1, name: "menu".into(), alert: false }],
            ..LabSnapshot::default()
        };
        assert_eq!(lab.scene_label(1), "menu");
        assert_eq!(lab.scene_label(2), "@2");
    }
}
