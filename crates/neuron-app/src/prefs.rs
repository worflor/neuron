//! App preferences — the small set of GUI-only settings that aren't device config. Lives as plain,
//! hand-editable TOML (`app.toml`) in the run directory, same as every other Neuron config. Today
//! it holds `start_minimized`: whether a `--tray`/autostart launch (or a bare launch) should bring
//! up the window or stay resident in the tray. Persisted for real — `main` reads it at startup to
//! decide whether to show the window, and the Settings toggle writes it back.
//!
//! `start-with-Windows` stays in `autostart.rs` (it's a registry Run value, not a file pref).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// The persisted lighting state for ONE device — the applied effect/layer stack (or data mode) plus
/// the chosen stream fps. Saved per-device (keyed by pid) so a board resumes its own effect after a
/// relaunch instead of sitting frozen on the device's last held frame. Every field defaults, so an
/// older/partial record still loads.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct DeviceLight {
    /// The chosen streaming fps for this board (the user's pick — restored verbatim on launch, so the
    /// per-device default only applies when nothing's saved). 0 = unset (fall back to the default).
    #[serde(default)]
    pub fps: u32,
    /// The active DATA-mode slug (e.g. "mouse-battery"), mutually exclusive with `layers`. `None` =
    /// no data mode (an effect stack, or nothing, owns the board).
    #[serde(default)]
    pub data: Option<String>,
    /// The applied compositor stack — empty when a data mode owns the board, or nothing's applied.
    /// Serialises as `[[lighting.<pid>.layers]]` array-of-tables; each layer is flat (see `LayerDef`).
    #[serde(default)]
    pub layers: Vec<neuron::effects::LayerDef>,
}

/// On-disk GUI preferences. All fields default so a missing/partial file still loads.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Prefs {
    /// Start resident in the tray with NO window shown. Defaults to true (lean, tray-first).
    #[serde(default = "default_true")]
    pub start_minimized: bool,
    /// The ONE interface accent (hex RRGGBB, no '#') — the phosphor the whole instrument tints with.
    /// The whole UI derives from it (glow/line/dim/ink), so this single value reskins everything.
    /// Default is the stock phosphor `4af2b0`.
    #[serde(default = "default_accent")]
    pub ui_accent: String,
    /// The weave accent (hex RRGGBB, no '#') — the colour the spellweaving cast filament, glyph ink
    /// and radial sigils paint with. Kept separate from the UI accent so the cast can read as its own
    /// instrument. Default matches the UI phosphor (`4af2b0`).
    #[serde(default = "default_accent")]
    pub weave_accent: String,
    /// The spellweaving material the cast is poured from — one of the engine's [`crate::material`]
    /// surfaces ("directed-intent", "fluid-thought", "materialized-desire", "gentle-breeze", …).
    /// Default is the house look, Directed Intent.
    #[serde(default = "default_material")]
    pub weave_material: String,
    /// PHOENIX — let the OS relaunch Neuron after a crash/hang (RegisterApplicationRestart). Default
    /// ON: the always-on flight recorder + an automatic respawn is the whole reliability story. Takes
    /// effect at the next launch (the OS registration happens once, at startup).
    #[serde(default = "default_true")]
    pub phoenix: bool,
    /// NOTIFICATIONS — the state-change confirmation cards. Master switch (default ON).
    #[serde(default = "default_true")]
    pub notif_enabled: bool,
    /// Where the card appears: "off" · "top-left" · "top-right" · "bottom-left" · "bottom-right" ·
    /// "inline" (top-centre) · "custom" (a free, hand-dragged spot — see `notif_x`/`notif_y`).
    /// Default "top-right".
    #[serde(default = "default_notif_placement")]
    pub notif_placement: String,
    /// The card's hand-placed anchor as a fraction of the monitor work-area (0..1, x then y), used
    /// when `notif_placement` is "custom". The named presets resolve to canonical corners/edges
    /// regardless of these, so an older file (or one set to a preset) still lands correctly.
    /// Default top-right (1, 0).
    #[serde(default = "default_notif_x")]
    pub notif_x: f32,
    #[serde(default = "default_notif_y")]
    pub notif_y: f32,
    /// Play the audio cue alongside (or, with placement "off", instead of) the card. Default ON. The
    /// cross-platform synth lands next; the switch is live now so the preference persists.
    #[serde(default = "default_true")]
    pub notif_audio: bool,
    /// Per-event gates — confirm DPI / sensitivity / polling / brightness / profile / layer changes.
    /// All default ON (macro-fire is opt-in per binding, never a global gate here).
    #[serde(default = "default_true")]
    pub notif_dpi: bool,
    #[serde(default = "default_true")]
    pub notif_scroll: bool,
    #[serde(default = "default_true")]
    pub notif_polling: bool,
    #[serde(default = "default_true")]
    pub notif_brightness: bool,
    #[serde(default = "default_true")]
    pub notif_profile: bool,
    #[serde(default = "default_true")]
    pub notif_layer: bool,
    /// Macro/BEACON notify card — the fire-and-forget `neuron.notify()` card a macro posts. Default
    /// ON. (This gates only the NOTIFY card; a macro's ASK prompt is never gated — you must see it to
    /// answer.)
    #[serde(default = "default_true")]
    pub notif_macro: bool,
    /// Device battery / charge cards — low/critical thresholds, charging engage·disengage, fully
    /// charged. Default ON; turn off if you don't want power notifications.
    #[serde(default = "default_true")]
    pub notif_battery: bool,
    /// Swappable SIDE-PLATE attach/detach cards (the device pushes its plate strap-code; no getter).
    /// Default ON — a rare, deliberate hardware action you generally want confirmed.
    #[serde(default = "default_true")]
    pub notif_side_plate: bool,
    /// Audio-cue master volume, 0..1. Default 0.7.
    #[serde(default = "default_notif_volume")]
    pub notif_volume: f32,
    /// Audio voice palette slug: "pulse" (soft default) · "warm" · "glass".
    #[serde(default = "default_notif_sound")]
    pub notif_sound: String,
    /// Draw the grounded squircle panel (app-card chrome) behind the card, vs the floating spell
    /// look. Default ON.
    #[serde(default = "default_true")]
    pub notif_panel: bool,
    /// How MULTIPLE live notifications present: "stack" (a reflowing column of up to 4 cards growing
    /// away from the corner, newest nearest, a `+N` tail past 4), "latest" (exactly one card, a new
    /// note crossfade-SWAPS it), or "digest" (one SUMMARY card — a count + a row of the distinct
    /// source glyphs + the latest line — when >1 is live, a plain single card otherwise). Default
    /// "stack". (The settings UI to pick this is a later pass; the engine reads it now.)
    #[serde(default = "default_stack_mode")]
    pub notif_stack: String,
    /// LIGHTING — the persisted applied effect/layer stack + chosen fps, keyed by device pid (4-digit
    /// hex). Lets each lit board resume its effect across a relaunch. Empty by default; ONLY the
    /// lighting page writes it. Declared LAST: it serialises as `[lighting.<pid>]` sub-tables, and TOML
    /// forbids a scalar after a table, so every flat pref above must come first.
    #[serde(default)]
    pub lighting: BTreeMap<String, DeviceLight>,
}

/// The default multi-notification presentation — the reflowing column (newest nearest the corner).
fn default_stack_mode() -> String {
    "stack".to_string()
}

/// How multiple live notifications present (see [`Prefs::notif_stack`]). Parsed from the pref string;
/// anything unknown falls back to [`StackMode::Stack`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackMode {
    /// A reflowing column of up to 4 cards (newest nearest the corner), a `+N` tail past that.
    Stack,
    /// Exactly one card; a new note crossfade-swaps the outgoing for the incoming.
    Latest,
    /// One summary card (count + distinct-source glyph row + latest line) when >1 is live.
    Digest,
}

fn default_true() -> bool {
    true
}

/// Default audio-cue volume — comfortable, not loud.
fn default_notif_volume() -> f32 {
    0.7
}

/// Default audio voice — the soft pulse.
fn default_notif_sound() -> String {
    "pulse".to_string()
}

/// The voice palette slugs offered in the picker (must match `neuron::tone::Timbre::PALETTES`).
pub const NOTIF_VOICES: [&str; 3] = ["pulse", "warm", "glass"];

/// The stock phosphor accent — the default for both the UI and the weave tint.
fn default_accent() -> String {
    "4af2b0".to_string()
}

/// The house spellweaving material — Directed Intent.
fn default_material() -> String {
    "directed-intent".to_string()
}

/// The default notification placement — the top-right corner, out of the way (Razer-familiar).
fn default_notif_placement() -> String {
    "top-right".to_string()
}

/// Default free-placement anchor — top-right (matches the default preset).
fn default_notif_x() -> f32 {
    1.0
}
fn default_notif_y() -> f32 {
    0.0
}

/// The valid placement slugs ("custom" = a free hand-placed spot; "off" = no card).
pub const NOTIF_PLACEMENTS: [&str; 7] = [
    "off",
    "top-left",
    "top-right",
    "bottom-left",
    "bottom-right",
    "inline",
    "custom",
];

/// The canonical anchor (0..1) for a NAMED preset slug, or `None` for "custom" / "off" / junk.
pub fn notif_canonical(slug: &str) -> Option<(f32, f32)> {
    match slug {
        "top-left" => Some((0.0, 0.0)),
        "top-right" => Some((1.0, 0.0)),
        "bottom-left" => Some((0.0, 1.0)),
        "bottom-right" => Some((1.0, 1.0)),
        "inline" => Some((0.5, 0.0)),
        _ => None,
    }
}

/// The preset slug an EXACT anchor lands on (so a drag that snaps to a corner keeps its name), or
/// `None` for a genuinely free spot (which is stored as "custom").
fn notif_preset_slug(x: f32, y: f32) -> Option<&'static str> {
    let near = |a: f32, b: f32| (a - b).abs() < 0.001;
    if near(x, 0.0) && near(y, 0.0) {
        Some("top-left")
    } else if near(x, 1.0) && near(y, 0.0) {
        Some("top-right")
    } else if near(x, 0.0) && near(y, 1.0) {
        Some("bottom-left")
    } else if near(x, 1.0) && near(y, 1.0) {
        Some("bottom-right")
    } else if near(x, 0.5) && near(y, 0.0) {
        Some("inline")
    } else {
        None
    }
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            start_minimized: true,
            ui_accent: default_accent(),
            weave_accent: default_accent(),
            weave_material: default_material(),
            phoenix: true,
            notif_enabled: true,
            notif_placement: default_notif_placement(),
            notif_x: default_notif_x(),
            notif_y: default_notif_y(),
            notif_audio: true,
            notif_dpi: true,
            notif_scroll: true,
            notif_polling: true,
            notif_brightness: true,
            notif_profile: true,
            notif_layer: true,
            notif_macro: true,
            notif_battery: true,
            notif_side_plate: true,
            notif_volume: default_notif_volume(),
            notif_sound: default_notif_sound(),
            notif_panel: true,
            notif_stack: default_stack_mode(),
            lighting: BTreeMap::new(),
        }
    }
}

// Cached prefs for the hot read path (the notif engine reads these every animation tick). Invalidated
// by `save()`; see `Prefs::load_cached`.
static PREFS_CACHE: OnceLock<Mutex<Prefs>> = OnceLock::new();
static PREFS_DIRTY: AtomicBool = AtomicBool::new(true);

impl Prefs {
    /// The prefs file path (run-directory-relative, like the rest of the config).
    pub fn path() -> PathBuf {
        PathBuf::from("app.toml")
    }

    /// Load the prefs (defaults if the file is absent or unparseable — never errors).
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist the prefs back to `app.toml`. Returns a status line.
    pub fn save(&self) -> Result<(), String> {
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(Self::path(), body).map_err(|e| e.to_string())?;
        PREFS_DIRTY.store(true, Ordering::Release); // invalidate the load_cached() copy
        Ok(())
    }

    /// Like [`load`](Self::load), but CACHED — reloads from disk only after a `save` flips the dirty
    /// flag. The notification engine reads prefs every animation tick; this stops it re-parsing
    /// app.toml ~95×/card while a card animates (idle cost stays zero — the engine blocks when empty).
    pub fn load_cached() -> Prefs {
        let cell = PREFS_CACHE.get_or_init(|| Mutex::new(Prefs::load()));
        if PREFS_DIRTY.swap(false, Ordering::AcqRel) {
            let fresh = Prefs::load();
            *cell.lock().unwrap() = fresh.clone();
            fresh
        } else {
            cell.lock().unwrap().clone()
        }
    }

    /// The card's anchor as a fraction of the monitor work-area (0..1). A named preset resolves to
    /// its canonical corner/edge (so an older file — or one hand-edited to a preset — always lands
    /// right); "custom" reads the free `notif_x`/`notif_y`; "off" still yields a position (its last
    /// spot) for the UI puck — `notif_place_xy` is what gates whether a card actually shows.
    pub fn notif_xy(&self) -> (f32, f32) {
        notif_canonical(&self.notif_placement)
            .unwrap_or((self.notif_x.clamp(0.0, 1.0), self.notif_y.clamp(0.0, 1.0)))
    }

    /// The card anchor (0..1) when a card SHOULD show, or `None` for "off" (audio-only / nothing).
    pub fn notif_place_xy(&self) -> Option<(f32, f32)> {
        if self.notif_placement == "off" {
            None
        } else {
            Some(self.notif_xy())
        }
    }

    /// How multiple live notifications present, parsed to the [`StackMode`] enum (unknown → Stack).
    pub fn notif_stack_mode(&self) -> StackMode {
        match self.notif_stack.as_str() {
            "latest" => StackMode::Latest,
            "digest" => StackMode::Digest,
            _ => StackMode::Stack,
        }
    }

    /// Is this confirmation kind gated ON? (A macro's NOTIFY card is opt-in at the binding AND honours
    /// the `notif_macro` gate here; a macro's ASK prompt is never routed through this gate.)
    #[inline]
    pub fn notif_kind_on(&self, kind: neuron::confirm::Kind) -> bool {
        use neuron::confirm::Kind;
        match kind {
            Kind::Dpi => self.notif_dpi,
            Kind::Scroll => self.notif_scroll,
            Kind::Polling => self.notif_polling,
            Kind::Brightness => self.notif_brightness,
            Kind::Profile => self.notif_profile,
            Kind::Layer => self.notif_layer,
            Kind::Macro => self.notif_macro,
            Kind::Battery => self.notif_battery,
            Kind::SidePlate => self.notif_side_plate,
        }
    }
}

/// Convenience: read just the start-minimized flag (the one `main` consults at launch).
pub fn start_minimized() -> bool {
    Prefs::load().start_minimized
}

/// Convenience: persist the start-minimized flag, returning a user-facing status line.
pub fn set_start_minimized(v: bool) -> String {
    let mut p = Prefs::load();
    p.start_minimized = v;
    match p.save() {
        Ok(()) => format!(
            "start-minimized {} (saved to app.toml)",
            if v { "enabled" } else { "disabled" }
        ),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Normalize a hex colour string to a bare 6-digit RRGGBB (drops a leading '#', upper/lowercases as
/// given, validates it's 6 hex digits). Returns the stock accent for anything unparseable, so a junk
/// pref never leaves the UI un-tinted.
pub fn normalize_hex(s: &str) -> String {
    let t = s.trim().trim_start_matches('#');
    if t.len() == 6 && t.chars().all(|c| c.is_ascii_hexdigit()) {
        t.to_ascii_lowercase()
    } else {
        default_accent()
    }
}

/// Read the saved interface accent (bare RRGGBB).
pub fn ui_accent() -> String {
    normalize_hex(&Prefs::load().ui_accent)
}

/// Persist the interface accent (bare RRGGBB), returning a user-facing status line.
pub fn set_ui_accent(hex: &str) -> String {
    let v = normalize_hex(hex);
    let mut p = Prefs::load();
    p.ui_accent = v.clone();
    match p.save() {
        Ok(()) => format!("interface accent → #{v} (saved to app.toml)"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the saved weave (spellweaving cast) accent (bare RRGGBB).
pub fn weave_accent() -> String {
    normalize_hex(&Prefs::load().weave_accent)
}

/// Persist the weave accent (bare RRGGBB), returning a user-facing status line.
pub fn set_weave_accent(hex: &str) -> String {
    let v = normalize_hex(hex);
    let mut p = Prefs::load();
    p.weave_accent = v.clone();
    match p.save() {
        Ok(()) => format!("weave accent → #{v} (saved to app.toml)"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the saved spellweaving material slug (e.g. "fluid-thought").
pub fn weave_material() -> String {
    Prefs::load().weave_material
}

/// Persist the spellweaving material slug, returning a user-facing status line.
pub fn set_weave_material(slug: &str) -> String {
    let mut p = Prefs::load();
    p.weave_material = slug.to_string();
    match p.save() {
        Ok(()) => format!("spellweaving material → {slug} (saved to app.toml)"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the PHOENIX auto-restart preference (default ON).
pub fn phoenix() -> bool {
    Prefs::load().phoenix
}

/// Persist the PHOENIX preference, returning a user-facing status line.
pub fn set_phoenix(v: bool) -> String {
    let mut p = Prefs::load();
    p.phoenix = v;
    match p.save() {
        Ok(()) => format!(
            "crash auto-restart {} (applies next launch)",
            if v { "armed" } else { "disarmed" }
        ),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the notifications master switch (default ON).
pub fn notif_enabled() -> bool {
    Prefs::load().notif_enabled
}

/// Persist the notifications master switch, returning a user-facing status line.
pub fn set_notif_enabled(v: bool) -> String {
    let mut p = Prefs::load();
    p.notif_enabled = v;
    match p.save() {
        Ok(()) => format!("notifications {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the notification placement slug (validated; falls back to the default for junk).
pub fn notif_placement() -> String {
    let p = Prefs::load().notif_placement;
    if NOTIF_PLACEMENTS.contains(&p.as_str()) {
        p
    } else {
        default_notif_placement()
    }
}

/// Persist the notification placement slug, returning a user-facing status line.
pub fn set_notif_placement(slug: &str) -> String {
    if !NOTIF_PLACEMENTS.contains(&slug) {
        return format!("invalid placement '{slug}'");
    }
    let mut p = Prefs::load();
    p.notif_placement = slug.to_string();
    // a named preset also pins the free anchor to its canonical spot, so "off" remembers a real
    // place and the UI puck can never disagree with the slug.
    if let Some((x, y)) = notif_canonical(slug) {
        p.notif_x = x;
        p.notif_y = y;
    }
    match p.save() {
        Ok(()) => format!("notification placement → {slug}"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Persist a HAND-PLACED card anchor (0..1, fractions of the work-area). An exact preset spot keeps
/// its named slug (so the UI lights that preset and the readout reads "top-right"); anything else is
/// a free "custom" position. This is the single write path the draggable placer commits through.
pub fn set_notif_pos(x: f32, y: f32) -> String {
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    let mut p = Prefs::load();
    p.notif_placement = notif_preset_slug(x, y).unwrap_or("custom").to_string();
    p.notif_x = x;
    p.notif_y = y;
    let slug = p.notif_placement.clone();
    match p.save() {
        Ok(()) => format!(
            "notification placement → {slug} ({:.0}%, {:.0}%)",
            x * 100.0,
            y * 100.0
        ),
        Err(e) => format!("save failed: {e}"),
    }
}

/// The resolved card anchor (0..1) for the UI puck.
pub fn notif_pos() -> (f32, f32) {
    Prefs::load().notif_xy()
}

/// Read the notification audio-cue flag (default ON).
pub fn notif_audio() -> bool {
    Prefs::load().notif_audio
}

/// Persist the notification audio-cue flag, returning a user-facing status line.
pub fn set_notif_audio(v: bool) -> String {
    let mut p = Prefs::load();
    p.notif_audio = v;
    match p.save() {
        Ok(()) => format!("notification audio {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read one per-event gate by slug (dpi/scroll/polling/brightness/profile/layer/macro).
pub fn notif_event(slug: &str) -> bool {
    let p = Prefs::load();
    match slug {
        "dpi" => p.notif_dpi,
        "scroll" => p.notif_scroll,
        "polling" => p.notif_polling,
        "brightness" => p.notif_brightness,
        "profile" => p.notif_profile,
        "layer" => p.notif_layer,
        "macro" => p.notif_macro,
        "battery" => p.notif_battery,
        "side_plate" => p.notif_side_plate,
        _ => false,
    }
}

/// Persist one per-event gate by slug, returning a user-facing status line.
pub fn set_notif_event(slug: &str, v: bool) -> String {
    let mut p = Prefs::load();
    match slug {
        "dpi" => p.notif_dpi = v,
        "scroll" => p.notif_scroll = v,
        "polling" => p.notif_polling = v,
        "brightness" => p.notif_brightness = v,
        "profile" => p.notif_profile = v,
        "layer" => p.notif_layer = v,
        "macro" => p.notif_macro = v,
        "battery" => p.notif_battery = v,
        "side_plate" => p.notif_side_plate = v,
        _ => return format!("unknown notify event '{slug}'"),
    }
    match p.save() {
        Ok(()) => format!("notify {slug} {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the WHEN-SEVERAL-LAND presentation mode ("stack" | "latest" | "digest").
pub fn notif_stack() -> String {
    Prefs::load().notif_stack
}

/// Persist the WHEN-SEVERAL-LAND presentation mode, returning a user-facing status line.
pub fn set_notif_stack(mode: &str) -> String {
    if !matches!(mode, "stack" | "latest" | "digest") {
        return format!("unknown stack mode '{mode}'");
    }
    let mut p = Prefs::load();
    p.notif_stack = mode.to_string();
    match p.save() {
        Ok(()) => format!("when several land: {mode}"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the audio-cue volume (0..1, default 0.7).
pub fn notif_volume() -> f32 {
    Prefs::load().notif_volume.clamp(0.0, 1.0)
}

/// Persist the audio-cue volume, returning a user-facing status line.
pub fn set_notif_volume(v: f32) -> String {
    let v = v.clamp(0.0, 1.0);
    let mut p = Prefs::load();
    p.notif_volume = v;
    match p.save() {
        Ok(()) => format!("notification volume → {}%", (v * 100.0).round() as i32),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the audio voice slug (validated against the palette; junk falls back to the default).
pub fn notif_sound() -> String {
    let s = Prefs::load().notif_sound;
    if NOTIF_VOICES.contains(&s.as_str()) {
        s
    } else {
        default_notif_sound()
    }
}

/// Persist the audio voice slug, returning a user-facing status line.
pub fn set_notif_sound(slug: &str) -> String {
    if !NOTIF_VOICES.contains(&slug) {
        return format!("unknown voice '{slug}'");
    }
    let mut p = Prefs::load();
    p.notif_sound = slug.to_string();
    match p.save() {
        Ok(()) => format!("notification voice → {slug}"),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the card-background flag (default ON).
pub fn notif_panel() -> bool {
    Prefs::load().notif_panel
}

/// Persist the card-background flag, returning a user-facing status line.
pub fn set_notif_panel(v: bool) -> String {
    let mut p = Prefs::load();
    p.notif_panel = v;
    match p.save() {
        Ok(()) => format!("notification background {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// The map key for a device's lighting state — its pid as 4-digit lowercase hex (the same handle the
/// runtime keys the selected device by). `0` (no device) has no key.
pub fn light_key(pid: u16) -> String {
    format!("{pid:04x}")
}

/// Read the saved lighting state for a device pid, or `None` if nothing's persisted for it.
pub fn device_light(pid: u16) -> Option<DeviceLight> {
    if pid == 0 {
        return None;
    }
    Prefs::load().lighting.get(&light_key(pid)).cloned()
}

/// Persist a device's lighting state (upsert by pid), preserving every sibling pref. Pid `0` is a
/// no-op (nothing selected). Cheap by design — call it on user lighting changes, NEVER per animation
/// frame.
pub fn set_device_light(pid: u16, state: DeviceLight) -> Result<(), String> {
    if pid == 0 {
        return Ok(());
    }
    let mut p = Prefs::load();
    p.lighting.insert(light_key(pid), state);
    p.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    // cwd isolation: the prefs file is run-directory-relative. cwd is a process-global, so ALL
    // cwd-mutating tests across the crate share ONE lock (see `testsupport`) and serialize.
    fn cwd_guard() -> crate::testsupport::CwdGuard {
        crate::testsupport::cwd_guard("prefs_test")
    }

    /// The default is tray-first (start minimized = true) when no file exists.
    #[test]
    fn default_is_start_minimized() {
        let _g = cwd_guard();
        assert!(
            Prefs::load().start_minimized,
            "default should be start-minimized"
        );
        assert!(start_minimized());
    }

    /// Toggling start-minimized persists to disk and reloads losslessly.
    #[test]
    fn start_minimized_round_trips() {
        let _g = cwd_guard();
        let msg = set_start_minimized(false);
        assert!(msg.contains("disabled"), "unexpected: {msg}");
        assert!(!start_minimized(), "false must persist + reload");
        let msg = set_start_minimized(true);
        assert!(msg.contains("enabled"), "unexpected: {msg}");
        assert!(start_minimized(), "true must persist + reload");
    }

    /// Lighting state persists per-device and reloads losslessly (fps + a multi-layer stack).
    #[test]
    fn device_light_round_trips() {
        let _g = cwd_guard();
        let pid = 0x0226u16;
        let state = DeviceLight {
            fps: 12,
            data: None,
            layers: vec![
                neuron::effects::LayerDef {
                    effect: "fire".into(),
                    color: neuron::lighting::Rgb::new(255, 90, 0),
                    speed: 2.0,
                    region: vec![5, 2, 9],
                    blend: neuron::effects::Blend::Add,
                    ..Default::default()
                },
                neuron::effects::LayerDef {
                    effect: "static".into(),
                    color: neuron::lighting::Rgb::new(0x4A, 0xF2, 0xB0),
                    ..Default::default()
                },
            ],
        };
        set_device_light(pid, state.clone()).expect("save lighting");
        let back = device_light(pid).expect("lighting reloads");
        assert_eq!(back.fps, 12);
        assert!(back.data.is_none());
        assert_eq!(back.layers.len(), 2);
        assert_eq!(back.layers[0].effect, "fire");
        assert_eq!(back.layers[0].region, vec![5, 2, 9]);
        assert_eq!(back.layers[0].blend, neuron::effects::Blend::Add);
        assert_eq!(back.layers[1].color, neuron::lighting::Rgb::new(0x4A, 0xF2, 0xB0));
        // a different (unsaved) device has no state — keying is real.
        assert!(device_light(0x00A8).is_none());
    }

    /// A data-mode lighting state (no layers) persists too.
    #[test]
    fn device_light_data_mode_round_trips() {
        let _g = cwd_guard();
        let pid = 0x0226u16;
        set_device_light(
            pid,
            DeviceLight {
                fps: 6,
                data: Some("mouse-battery".into()),
                layers: vec![],
            },
        )
        .expect("save");
        let back = device_light(pid).expect("reload");
        assert_eq!(back.data.as_deref(), Some("mouse-battery"));
        assert!(back.layers.is_empty());
    }

    /// An OLD app.toml with NO `[lighting]` section still loads (back-compat) — the map defaults empty,
    /// every other pref reads through, and a lighting write doesn't disturb the siblings.
    #[test]
    fn old_config_without_lighting_loads() {
        let _g = cwd_guard();
        std::fs::write(
            Prefs::path(),
            "start_minimized = false\nui_accent = \"ff8800\"\n",
        )
        .unwrap();
        let p = Prefs::load();
        assert!(!p.start_minimized);
        assert_eq!(p.ui_accent, "ff8800");
        assert!(p.lighting.is_empty(), "missing section defaults to empty");
        // writing lighting must preserve the pre-existing siblings.
        set_device_light(
            0x0226,
            DeviceLight {
                fps: 30,
                data: None,
                layers: vec![],
            },
        )
        .unwrap();
        let p = Prefs::load();
        assert!(!p.start_minimized, "sibling pref survives the lighting write");
        assert_eq!(p.ui_accent, "ff8800");
        assert_eq!(p.lighting.get(&light_key(0x0226)).map(|d| d.fps), Some(30));
    }

    /// The accent write must not clobber sibling prefs in app.toml (load-modify-save discipline).
    #[test]
    fn accent_write_preserves_siblings() {
        let _g = cwd_guard();
        set_start_minimized(false);
        set_ui_accent("ff8800");
        assert_eq!(ui_accent(), "ff8800", "accent must persist + reload");
        assert!(
            !start_minimized(),
            "sibling pref must survive the accent write"
        );
    }
}
