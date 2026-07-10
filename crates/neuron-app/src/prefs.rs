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

/// The persisted lighting state for ONE device — the applied effect/layer stack plus the chosen stream
/// fps. Saved per-device (keyed by pid) so a board resumes its own effect after a relaunch instead of
/// sitting frozen on the device's last held frame. Every field defaults, so an older/partial record
/// still loads (a pre-unification `data` mode migrates via [`DeviceLight::migrated`]).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DeviceLight {
    /// The chosen streaming fps for this board (the user's pick — restored verbatim on launch, so the
    /// per-device default only applies when nothing's saved). 0 = unset (fall back to the default).
    #[serde(default)]
    pub fps: u32,
    /// LEGACY compat shim (pre-unification): an old save's DATA-mode slug (e.g. "mouse-battery").
    /// Deserialize-ONLY — read from the old `data` key but NEVER re-serialized (`skip_serializing`), so
    /// it evaporates from disk on the next save. [`DeviceLight::migrated`] folds it into a `vitals`
    /// LAYER on load, so an existing save resumes as the vitals surface with zero user action. Remove
    /// once no pre-release save can still carry it.
    #[serde(default, rename = "data", skip_serializing)]
    pub legacy_data: Option<String>,
    /// The applied compositor stack — the SINGLE representation of a board's lighting (a `vitals` readout
    /// is now just a layer in here, not a sidecar mode). Empty when nothing's applied. Serialises as
    /// `[[lighting.<pid>.layers]]` array-of-tables; each layer is flat (a pattern key, optional params, a
    /// tiered spectrum, region, blend, enabled — see `pattern::LayerDef`).
    #[serde(default)]
    pub layers: Vec<neuron::pattern::LayerDef>,
}

impl DeviceLight {
    /// Fold a LEGACY data-mode save into the unified layer stack: an old `data = "mouse-battery"` record
    /// becomes a `vitals` LAYER appended to `layers` (unless one's already present), so a pre-unification
    /// save resumes as the vitals surface with no user action. One-way + idempotent; the `legacy_data`
    /// shim is consumed (and it never re-serializes), so the next save drops the old field for good.
    pub fn migrated(mut self) -> Self {
        // Only the KNOWN data-mode slug maps to a vitals layer. `.take()` always consumes the shim (so an
        // unknown/garbled value can't re-serialize), but only `Some("mouse-battery")` folds into a readout
        // — a stray or future `data` value is dropped, never grafted onto the board. Don't duplicate an
        // existing vitals layer.
        if self.legacy_data.take().as_deref() == Some("mouse-battery")
            && !self.layers.iter().any(|l| l.pattern == "vitals")
        {
            if let Some(v) = neuron::pattern::preset_layer("vitals") {
                self.layers.push(v);
            }
        }
        self
    }
}

/// On-disk GUI preferences. All fields default so a missing/partial file still loads.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
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
    /// The sniper hold's own gate — DEFAULT OFF, the one kind that isn't: sniper fires mid-game
    /// where a card is exactly the wrong garnish, and the slowed crosshair already confirms the
    /// hold. Opt in here if you want the on/off cards anyway. (Its dedup side-effect — absorbing
    /// the device's echo so no spurious plain-DPI card fires — happens regardless of this gate.)
    #[serde(default)]
    pub notif_sniper: bool,
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
    /// CONNECTIONS master switch — let other software drive the lighting THROUGH neuron's arbiter
    /// (games speak Razer Chroma, tools speak OpenRGB), composing ABOVE the configured base stack
    /// instead of fighting it for the device. Default OFF: turning it on opens two loopback ports
    /// and changes who may paint the boards — that stays a conscious opt-in (SYSTEM → CONNECTIONS).
    #[serde(default)]
    pub host_enabled: bool,
    /// Serve the Razer Chroma SDK protocol (localhost:54235) — what Chroma-enabled games speak.
    /// Gated by `host_enabled`; default ON so enabling connections is one switch, not three.
    #[serde(default = "default_true")]
    pub host_chroma: bool,
    /// Serve the OpenRGB SDK protocol (localhost:6742) — what RGB tools / Home Assistant speak.
    /// Gated by `host_enabled`; default ON, same one-switch reasoning as `host_chroma`.
    #[serde(default = "default_true")]
    pub host_openrgb: bool,
    /// Connect OUT to OBS Studio (obs-websocket, localhost:4455) so macros can drive scenes /
    /// stream / recording, and OBS events reach the signal bus. Gated by `host_enabled`; default
    /// OFF — it's an outbound connection to a specific app most users don't run, so it opts in
    /// separately (unlike the always-useful lighting servers).
    #[serde(default)]
    pub host_obs: bool,
    /// CHROMA (games) paint lane — HOW a game's Chroma frame combines with your base lighting,
    /// the emergent config no last-writer-wins tool (Synapse, OpenRGB) can offer because only an
    /// arbiter has the concept: "replace" (the game takes the keys it paints outright), "merge"
    /// (default — SCREENED over your base so its bright keys punch through while your lighting
    /// stays underneath), "boost" (added) or "tint" (multiplied). Applies live to REST + native
    /// Chroma.
    #[serde(default = "default_chroma_paint_mode")]
    pub host_chroma_paint_mode: String,
    /// Chroma paint opacity, 0..=100.
    #[serde(default = "default_paint_strength")]
    pub host_chroma_paint_strength: u8,
    /// Chroma crossfade time in milliseconds, 0..=2500.
    #[serde(default = "default_chroma_paint_fade_ms")]
    pub host_chroma_paint_fade_ms: u32,
    /// OPENRGB (tools) paint lane — same domain as the Chroma lane, its own settings. Defaults to
    /// "replace" so OpenRGB config tools keep their classic hard-takeover feel (a set colour
    /// appears as-sent) until the user opts into blending.
    #[serde(default = "default_openrgb_paint_mode")]
    pub host_openrgb_paint_mode: String,
    /// OpenRGB paint opacity, 0..=100.
    #[serde(default = "default_paint_strength")]
    pub host_openrgb_paint_strength: u8,
    /// OpenRGB crossfade time in milliseconds, 0..=2500. Default 0 (instant), matching the
    /// hard-takeover default mode.
    #[serde(default)]
    pub host_openrgb_paint_fade_ms: u32,
    /// UNIVERSAL hands-off list: physical device ids excluded from ALL external paint (Chroma AND
    /// OpenRGB). Empty means every attached lighting-capable device is included, including future
    /// hotplug.
    #[serde(default)]
    pub host_paint_disabled_devices: Vec<String>,
    /// When on, your own lighting outranks every visitor: the base claims the OVERRIDE band, so
    /// games/tools keep painting underneath but never show (they read as suppressed). Default off
    /// (visitors compose ABOVE your base). Applies live.
    #[serde(default)]
    pub host_base_always_wins: bool,
    /// The obs-websocket Server Password (OBS → Tools → WebSocket Server Settings). Empty = a
    /// passwordless OBS server (auth off). Stored in app.toml like the rest; the `NEURON_OBS_PASSWORD`
    /// env var, when set, overrides this (a dev/headless escape hatch).
    #[serde(default)]
    pub host_obs_password: String,
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

fn default_chroma_paint_mode() -> String {
    "merge".to_string()
}

fn default_openrgb_paint_mode() -> String {
    "replace".to_string()
}

fn default_paint_strength() -> u8 {
    100
}

fn default_chroma_paint_fade_ms() -> u32 {
    450
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
            notif_sniper: false, // the one default-off kind — see the field doc
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
            host_enabled: false,
            host_chroma: true,
            host_openrgb: true,
            host_obs: false,
            host_chroma_paint_mode: default_chroma_paint_mode(),
            host_chroma_paint_strength: default_paint_strength(),
            host_chroma_paint_fade_ms: default_chroma_paint_fade_ms(),
            host_openrgb_paint_mode: default_openrgb_paint_mode(),
            host_openrgb_paint_strength: default_paint_strength(),
            host_openrgb_paint_fade_ms: 0,
            host_paint_disabled_devices: Vec::new(),
            host_base_always_wins: false,
            host_obs_password: String::new(),
            lighting: BTreeMap::new(),
        }
    }
}

// Cached prefs for the hot read path (the notif engine reads these every animation tick). Invalidated
// by `save()`; see `Prefs::load_cached`.
static PREFS_CACHE: OnceLock<Mutex<Prefs>> = OnceLock::new();
static PREFS_DIRTY: AtomicBool = AtomicBool::new(true);

impl Prefs {
    /// The prefs file path (in the run root, like the rest of the config).
    pub fn path() -> PathBuf {
        neuron::runroot::run_root().join("app.toml")
    }

    /// Load the prefs (never errors). An absent file yields all-defaults silently (normal first run).
    /// A file that EXISTS but doesn't fully parse is SALVAGED field-by-field rather than discarded: one
    /// bad value (a typo'd number, a hand-edit, a type that shifted during pre-release shaping) defaults
    /// only ITSELF while every sibling pref — accents, safety/notification gates, the lighting stack —
    /// is kept. Each salvaged-to-default part warns by name, so a corrupt config is visible, not a
    /// silent reset. `load` NEVER writes: a partially-bad file keeps its good values and stays on disk
    /// untouched (only `save` writes).
    pub fn load() -> Self {
        let Ok(s) = std::fs::read_to_string(Self::path()) else {
            return Prefs::default(); // absent file → defaults (first run; nothing to warn about)
        };
        let table = toml::from_str::<toml::Table>(&s).ok();
        // Fast path: a clean whole-struct parse (the overwhelmingly common case).
        if let Ok(p) = toml::from_str::<Prefs>(&s) {
            return p;
        }
        // Something didn't fit the struct. Re-parse to a raw table and rebuild field-by-field so one
        // corrupt value can't nuke the rest. If it isn't even valid TOML, fall back to all-defaults —
        // still WITHOUT touching the file (only `save` writes).
        match table {
            Some(table) => Self::from_table_salvaging(&table),
            None => {
                eprintln!("neuron: app.toml is not valid TOML; using defaults (file left intact)");
                Prefs::default()
            }
        }
    }

    /// Rebuild [`Prefs`] from a parsed TOML table, salvaging field by field: each pref that fails to
    /// deserialize (wrong type, out-of-range, …) falls back to its OWN default while every sibling that
    /// parses is kept; a missing field defaults silently, a present-but-bad one warns by name. This is
    /// the resilient slow path [`load`](Self::load) drops to when a whole-struct parse fails.
    fn from_table_salvaging(table: &toml::Table) -> Self {
        let mut p = Prefs::default();
        // Each pref: if present, deserialize it on its own; on failure keep the default and warn. The
        // target type (so `try_into` knows what to build) is inferred from the assigned field.
        macro_rules! salvage {
            ($key:literal, $field:ident) => {
                if let Some(v) = table.get($key) {
                    match v.clone().try_into() {
                        Ok(parsed) => p.$field = parsed,
                        Err(e) => eprintln!(
                            "neuron: app.toml `{}` is malformed ({e}); keeping the default",
                            $key
                        ),
                    }
                }
            };
        }
        salvage!("start_minimized", start_minimized);
        salvage!("ui_accent", ui_accent);
        salvage!("weave_accent", weave_accent);
        salvage!("weave_material", weave_material);
        salvage!("phoenix", phoenix);
        salvage!("notif_enabled", notif_enabled);
        salvage!("notif_placement", notif_placement);
        salvage!("notif_x", notif_x);
        salvage!("notif_y", notif_y);
        salvage!("notif_audio", notif_audio);
        salvage!("notif_dpi", notif_dpi);
        salvage!("notif_sniper", notif_sniper);
        salvage!("notif_scroll", notif_scroll);
        salvage!("notif_polling", notif_polling);
        salvage!("notif_brightness", notif_brightness);
        salvage!("notif_profile", notif_profile);
        salvage!("notif_layer", notif_layer);
        salvage!("notif_macro", notif_macro);
        salvage!("notif_battery", notif_battery);
        salvage!("notif_side_plate", notif_side_plate);
        salvage!("notif_volume", notif_volume);
        salvage!("notif_sound", notif_sound);
        salvage!("notif_panel", notif_panel);
        salvage!("notif_stack", notif_stack);
        salvage!("host_enabled", host_enabled);
        salvage!("host_chroma", host_chroma);
        salvage!("host_openrgb", host_openrgb);
        salvage!("host_obs", host_obs);
        salvage!("host_chroma_paint_mode", host_chroma_paint_mode);
        salvage!("host_chroma_paint_strength", host_chroma_paint_strength);
        salvage!("host_chroma_paint_fade_ms", host_chroma_paint_fade_ms);
        salvage!("host_openrgb_paint_mode", host_openrgb_paint_mode);
        salvage!("host_openrgb_paint_strength", host_openrgb_paint_strength);
        salvage!("host_openrgb_paint_fade_ms", host_openrgb_paint_fade_ms);
        salvage!("host_paint_disabled_devices", host_paint_disabled_devices);
        salvage!("host_base_always_wins", host_base_always_wins);
        salvage!("host_obs_password", host_obs_password);
        // `lighting` is a per-device map — salvage it board-by-board so one corrupt record drops only
        // itself, not every other saved stack.
        if let Some(v) = table.get("lighting") {
            p.lighting = salvage_lighting(v);
        }
        p
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
            *cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = fresh.clone();
            fresh
        } else {
            cell.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
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
            Kind::Sniper => self.notif_sniper,
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

/// Salvage the per-device lighting map: parse it whole first, and only if that fails fall to a
/// board-by-board rebuild, so a single corrupt device record defaults only itself and every other
/// saved stack survives. A `lighting` value that isn't a table at all yields an empty map. Drops are
/// warned by device key, never silent.
fn salvage_lighting(v: &toml::Value) -> BTreeMap<String, DeviceLight> {
    // whole-map fast path
    let whole: Result<BTreeMap<String, DeviceLight>, _> = v.clone().try_into();
    if let Ok(map) = whole {
        return map;
    }
    let mut out = BTreeMap::new();
    match v.as_table() {
        Some(table) => {
            for (pid, light) in table {
                let parsed: Result<DeviceLight, _> = light.clone().try_into();
                match parsed {
                    Ok(d) => {
                        out.insert(pid.clone(), d);
                    }
                    Err(e) => eprintln!(
                        "neuron: app.toml `[lighting.{pid}]` is malformed ({e}); dropping that board's saved lighting"
                    ),
                }
            }
        }
        None => eprintln!("neuron: app.toml `lighting` is not a table; dropping all saved lighting"),
    }
    out
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

/// Persist one per-event gate by slug, returning a user-facing status line.
pub fn set_notif_event(slug: &str, v: bool) -> String {
    let mut p = Prefs::load();
    match slug {
        "dpi" => p.notif_dpi = v,
        "sniper" => p.notif_sniper = v,
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

/// Read the CONNECTIONS master switch (default OFF — a conscious opt-in).
pub fn host_enabled() -> bool {
    Prefs::load().host_enabled
}

/// Persist the CONNECTIONS master switch, returning a user-facing status line.
pub fn set_host_enabled(v: bool) -> String {
    let mut p = Prefs::load();
    p.host_enabled = v;
    match p.save() {
        Ok(()) => format!("connections {}", if v { "open" } else { "closed" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the Chroma (games) protocol gate (default ON; meaningful only while connections are open).
pub fn host_chroma() -> bool {
    Prefs::load().host_chroma
}

/// Persist the Chroma protocol gate, returning a user-facing status line.
pub fn set_host_chroma(v: bool) -> String {
    let mut p = Prefs::load();
    p.host_chroma = v;
    match p.save() {
        Ok(()) => format!("chroma (games) {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the OpenRGB (tools) protocol gate (default ON; meaningful only while connections are open).
pub fn host_openrgb() -> bool {
    Prefs::load().host_openrgb
}

/// Persist the OpenRGB protocol gate, returning a user-facing status line.
pub fn set_host_openrgb(v: bool) -> String {
    let mut p = Prefs::load();
    p.host_openrgb = v;
    match p.save() {
        Ok(()) => format!("openrgb (tools) {}", if v { "on" } else { "off" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the OBS connect gate (default OFF; meaningful only while connections are open).
pub fn host_obs() -> bool {
    Prefs::load().host_obs
}

/// Persist the OBS connect gate, returning a user-facing status line.
pub fn set_host_obs(v: bool) -> String {
    let mut p = Prefs::load();
    p.host_obs = v;
    match p.save() {
        Ok(()) => format!("obs {}", if v { "connecting" } else { "disconnected" }),
        Err(e) => format!("save failed: {e}"),
    }
}

/// Read the OBS server password FOR RUNTIME AUTH — env `NEURON_OBS_PASSWORD` overrides the saved
/// pref (the dev/headless escape hatch). Do NOT use this to seed the UI: an env-only secret must
/// stay ephemeral (see [`host_obs_password_saved`]).
pub fn host_obs_password() -> String {
    std::env::var("NEURON_OBS_PASSWORD").unwrap_or_else(|_| Prefs::load().host_obs_password)
}

/// The SAVED OBS password only (never the env override) — what the settings field shows and edits.
/// Keeping the env value out of the UI is what keeps an env-only secret from being surfaced and,
/// one save-click later, persisted into app.toml.
pub fn host_obs_password_saved() -> String {
    Prefs::load().host_obs_password
}

/// Persist the OBS server password, returning a user-facing status line (never echoes the secret).
pub fn set_host_obs_password(v: &str) -> String {
    let mut p = Prefs::load();
    p.host_obs_password = v.to_string();
    match p.save() {
        Ok(()) => {
            if v.is_empty() {
                "obs password cleared (assuming auth is off)".into()
            } else {
                "obs password saved".into()
            }
        }
        Err(e) => format!("save failed: {e}"),
    }
}

/// Normalize an external-paint blend mode string to a known value (unknown → "merge").
fn normalize_paint_mode(v: &str) -> &'static str {
    match v {
        "replace" => "replace",
        "boost" => "boost",
        "tint" => "tint",
        _ => "merge",
    }
}

/// The SplitToggle index for a paint mode (0 replace, 1 merge, 2 boost, 3 tint).
fn paint_mode_index(mode: &str) -> i32 {
    match mode {
        "replace" => 0,
        "boost" => 2,
        "tint" => 3,
        _ => 1,
    }
}

// ── CHROMA (games) paint lane ──────────────────────────────────────────────
pub fn host_chroma_paint_mode() -> String {
    normalize_paint_mode(&Prefs::load().host_chroma_paint_mode).to_string()
}

pub fn host_chroma_paint_mode_index() -> i32 {
    paint_mode_index(&host_chroma_paint_mode())
}

pub fn set_host_chroma_paint_mode(v: &str) -> String {
    let mut p = Prefs::load();
    let mode = normalize_paint_mode(v);
    p.host_chroma_paint_mode = mode.to_string();
    match p.save() {
        Ok(()) => format!("chroma games blend: {mode}"),
        Err(e) => format!("save failed: {e}"),
    }
}

pub fn host_chroma_paint_strength() -> u8 {
    Prefs::load().host_chroma_paint_strength.clamp(0, 100)
}

pub fn set_host_chroma_paint_strength(v: u8) -> String {
    let mut p = Prefs::load();
    p.host_chroma_paint_strength = v.clamp(0, 100);
    match p.save() {
        Ok(()) => format!("chroma games strength: {}%", p.host_chroma_paint_strength),
        Err(e) => format!("save failed: {e}"),
    }
}

pub fn host_chroma_paint_fade_ms() -> u32 {
    Prefs::load().host_chroma_paint_fade_ms.clamp(0, 2500)
}

pub fn set_host_chroma_paint_fade_ms(v: u32) -> String {
    let mut p = Prefs::load();
    p.host_chroma_paint_fade_ms = v.clamp(0, 2500);
    match p.save() {
        Ok(()) => format!("chroma games fade: {} ms", p.host_chroma_paint_fade_ms),
        Err(e) => format!("save failed: {e}"),
    }
}

// ── OPENRGB (tools) paint lane ─────────────────────────────────────────────
pub fn host_openrgb_paint_mode() -> String {
    normalize_paint_mode(&Prefs::load().host_openrgb_paint_mode).to_string()
}

pub fn host_openrgb_paint_mode_index() -> i32 {
    paint_mode_index(&host_openrgb_paint_mode())
}

pub fn set_host_openrgb_paint_mode(v: &str) -> String {
    let mut p = Prefs::load();
    let mode = normalize_paint_mode(v);
    p.host_openrgb_paint_mode = mode.to_string();
    match p.save() {
        Ok(()) => format!("openrgb tools blend: {mode}"),
        Err(e) => format!("save failed: {e}"),
    }
}

pub fn host_openrgb_paint_strength() -> u8 {
    Prefs::load().host_openrgb_paint_strength.clamp(0, 100)
}

pub fn set_host_openrgb_paint_strength(v: u8) -> String {
    let mut p = Prefs::load();
    p.host_openrgb_paint_strength = v.clamp(0, 100);
    match p.save() {
        Ok(()) => format!("openrgb tools strength: {}%", p.host_openrgb_paint_strength),
        Err(e) => format!("save failed: {e}"),
    }
}

pub fn host_openrgb_paint_fade_ms() -> u32 {
    Prefs::load().host_openrgb_paint_fade_ms.clamp(0, 2500)
}

pub fn set_host_openrgb_paint_fade_ms(v: u32) -> String {
    let mut p = Prefs::load();
    p.host_openrgb_paint_fade_ms = v.clamp(0, 2500);
    match p.save() {
        Ok(()) => format!("openrgb tools fade: {} ms", p.host_openrgb_paint_fade_ms),
        Err(e) => format!("save failed: {e}"),
    }
}

// ── UNIVERSAL hands-off device list (Chroma AND OpenRGB) ────────────────────
pub fn host_paint_disabled_devices() -> Vec<String> {
    Prefs::load().host_paint_disabled_devices
}

pub fn host_paint_device_enabled(id: &str) -> bool {
    !Prefs::load().host_paint_disabled_devices.iter().any(|x| x == id)
}

pub fn set_host_paint_device(id: &str, enabled: bool) -> String {
    let mut p = Prefs::load();
    if enabled {
        p.host_paint_disabled_devices.retain(|x| x != id);
    } else if !p.host_paint_disabled_devices.iter().any(|x| x == id) {
        p.host_paint_disabled_devices.push(id.to_string());
    }
    p.host_paint_disabled_devices.sort();
    p.host_paint_disabled_devices.dedup();
    match p.save() {
        Ok(()) => "hands-off devices saved".into(),
        Err(e) => format!("save failed: {e}"),
    }
}

// ── my-lighting-always-wins base priority ──────────────────────────────────
pub fn host_base_always_wins() -> bool {
    Prefs::load().host_base_always_wins
}

pub fn set_host_base_always_wins(v: bool) -> String {
    let mut p = Prefs::load();
    p.host_base_always_wins = v;
    match p.save() {
        Ok(()) => if v {
            "your lighting always wins — visitors paint underneath".into()
        } else {
            "visitors can paint over your lighting".into()
        },
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

    /// Every new paint-lane key persists AND reloads through the real save/load — and the mode
    /// getters normalize a junk on-disk value to a known mode rather than leaking it.
    #[test]
    fn host_paint_lanes_round_trip() {
        let _g = cwd_guard();
        // Chroma lane
        assert!(set_host_chroma_paint_mode("tint").contains("tint"));
        assert_eq!(host_chroma_paint_mode(), "tint");
        assert_eq!(host_chroma_paint_mode_index(), 3);
        set_host_chroma_paint_strength(40);
        assert_eq!(host_chroma_paint_strength(), 40);
        set_host_chroma_paint_fade_ms(1200);
        assert_eq!(host_chroma_paint_fade_ms(), 1200);
        // OpenRGB lane
        assert!(set_host_openrgb_paint_mode("boost").contains("boost"));
        assert_eq!(host_openrgb_paint_mode(), "boost");
        assert_eq!(host_openrgb_paint_mode_index(), 2);
        set_host_openrgb_paint_strength(75);
        assert_eq!(host_openrgb_paint_strength(), 75);
        set_host_openrgb_paint_fade_ms(300);
        assert_eq!(host_openrgb_paint_fade_ms(), 300);
        // clamps
        set_host_chroma_paint_strength(200);
        assert_eq!(host_chroma_paint_strength(), 100);
        set_host_openrgb_paint_fade_ms(9999);
        assert_eq!(host_openrgb_paint_fade_ms(), 2500);
    }

    /// A junk mode string on disk normalizes to "merge" through the getter (unreleased app: no
    /// migration, the getter is the one guard).
    #[test]
    fn junk_paint_mode_normalizes_to_merge() {
        let _g = cwd_guard();
        std::fs::write(Prefs::path(), "host_chroma_paint_mode = \"garbage\"\n").unwrap();
        assert_eq!(host_chroma_paint_mode(), "merge");
        assert_eq!(host_chroma_paint_mode_index(), 1);
    }

    /// The universal hands-off list adds/removes by id and dedups, and `enabled` is the inverse of
    /// membership.
    #[test]
    fn host_paint_devices_round_trip() {
        let _g = cwd_guard();
        assert!(host_paint_device_enabled("unit-a"), "empty list ⇒ every device enabled");
        set_host_paint_device("unit-a", false);
        set_host_paint_device("unit-a", false); // idempotent — no duplicate
        assert!(!host_paint_device_enabled("unit-a"));
        assert_eq!(host_paint_disabled_devices(), vec!["unit-a".to_string()]);
        set_host_paint_device("unit-a", true);
        assert!(host_paint_device_enabled("unit-a"));
        assert!(host_paint_disabled_devices().is_empty());
    }

    /// `host_base_always_wins` persists both ways.
    #[test]
    fn host_base_always_wins_round_trips() {
        let _g = cwd_guard();
        assert!(!host_base_always_wins(), "default off");
        set_host_base_always_wins(true);
        assert!(host_base_always_wins());
        set_host_base_always_wins(false);
        assert!(!host_base_always_wins());
    }

    /// Lighting state persists per-device and reloads losslessly (fps + a multi-layer stack).
    #[test]
    fn device_light_round_trips() {
        let _g = cwd_guard();
        let pid = 0x0226u16;
        let mut params = neuron::pattern::Params::default();
        params.set("speed", 2.0);
        let state = DeviceLight {
            fps: 12,
            legacy_data: None,
            layers: vec![
                neuron::pattern::LayerDef {
                    pattern: "heat".into(),
                    params,
                    spectrum: neuron::spectrum::Spectrum::gradient(vec![
                        neuron::lighting::Rgb::new(180, 0, 0),
                        neuron::lighting::Rgb::new(255, 255, 220),
                    ]),
                    region: vec![5, 2, 9],
                    blend: neuron::effects::Blend::Add,
                    enabled: true,
                    ..Default::default()
                },
                neuron::pattern::LayerDef {
                    pattern: "uniform".into(),
                    spectrum: neuron::spectrum::Spectrum::solid(neuron::lighting::Rgb::new(
                        0x4A, 0xF2, 0xB0,
                    )),
                    ..Default::default()
                },
            ],
        };
        set_device_light(pid, state.clone()).expect("save lighting");
        let back = device_light(pid).expect("lighting reloads");
        assert_eq!(back.fps, 12);
        assert!(back.legacy_data.is_none());
        assert_eq!(back.layers.len(), 2);
        assert_eq!(back.layers[0].pattern, "heat");
        assert_eq!(back.layers[0].params.f32("speed", 0.0), 2.0);
        assert_eq!(back.layers[0].region, vec![5, 2, 9]);
        assert_eq!(back.layers[0].blend, neuron::effects::Blend::Add);
        assert_eq!(
            back.layers[1].spectrum,
            neuron::spectrum::Spectrum::solid(neuron::lighting::Rgb::new(0x4A, 0xF2, 0xB0))
        );
        // a different (unsaved) device has no state — keying is real.
        assert!(device_light(0x00A8).is_none());
    }

    /// A MULTI-layer stack where higher layers carry TABLE-tier spectra (motion / positioned stops)
    /// round-trips through the real save/load. Regression guard for "stacked layers collapse to base on
    /// relaunch" — the collapse was a save-flush TIMING bug (now flushed on structural edits + on exit),
    /// not serde, but this pins the data path so the table-tier stack can never silently fail to persist.
    #[test]
    fn multi_layer_table_spectrum_stack_round_trips() {
        let _g = cwd_guard();
        use neuron::lighting::Rgb;
        use neuron::spectrum::{Motion, Palette, Spectrum, Stop};
        let pid = 0x0221u16;
        let thermal = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(6, 7, 18), 0.0),
                Stop::new(Rgb::new(190, 22, 0), 0.32),
                Stop::new(Rgb::new(255, 255, 255), 1.0),
            ],
            Motion::Hold,
        ));
        let aurora = Spectrum::from_palette(Palette::new(
            vec![
                Stop::new(Rgb::new(0, 255, 128), 0.0),
                Stop::new(Rgb::new(128, 0, 255), 1.0),
            ],
            Motion::Flow { speed: 1.0, chaos: 0.5 },
        ));
        let state = DeviceLight {
            fps: 30,
            legacy_data: None,
            layers: vec![
                neuron::pattern::LayerDef {
                    pattern: "thermal".into(),
                    spectrum: thermal,
                    blend: neuron::effects::Blend::Screen,
                    ..Default::default()
                },
                neuron::pattern::LayerDef {
                    pattern: "flow".into(),
                    spectrum: aurora,
                    blend: neuron::effects::Blend::Add,
                    ..Default::default()
                },
            ],
        };
        set_device_light(pid, state.clone()).expect("save multi-layer stack");
        let back = device_light(pid).expect("reload");
        assert_eq!(back.layers.len(), 2, "BOTH stacked layers must survive");
        assert_eq!(
            back.layers, state.layers,
            "each layer's distinct table-tier spectrum must round-trip (no collapse to base)"
        );
    }

    /// The field-by-field salvage list in `from_table_salvaging` must stay in LOCKSTEP with the struct:
    /// add a pref but forget its `salvage!` line and the resilient slow path would silently drop it on a
    /// corrupt config. This pins it both ways — the struct literal below forces every field to be named
    /// (a NEW field won't compile until it's added here), and at runtime we flip them all off-default,
    /// corrupt ONE so the fast whole-struct parse fails (forcing the salvage path), and assert every
    /// OTHER field survived. A field that reverts ⇒ its `salvage!` line is missing.
    #[test]
    fn salvage_preserves_every_field_when_one_is_corrupt() {
        let _g = cwd_guard();
        let d = Prefs::default();
        let want = Prefs {
            start_minimized: !d.start_minimized,
            ui_accent: "ff0000".into(),
            weave_accent: "00ff00".into(),
            weave_material: "test-mat".into(),
            phoenix: !d.phoenix,
            notif_enabled: !d.notif_enabled,
            notif_placement: "test-place".into(),
            notif_x: 0.25,
            notif_y: 0.5,
            notif_audio: !d.notif_audio,
            notif_dpi: !d.notif_dpi,
            notif_sniper: !d.notif_sniper,
            notif_scroll: !d.notif_scroll,
            notif_polling: !d.notif_polling,
            notif_brightness: !d.notif_brightness,
            notif_profile: !d.notif_profile,
            notif_layer: !d.notif_layer,
            notif_macro: !d.notif_macro,
            notif_battery: !d.notif_battery,
            notif_side_plate: !d.notif_side_plate,
            notif_volume: 0.125,
            notif_sound: "test-sound".into(),
            notif_panel: !d.notif_panel,
            notif_stack: "test-stack".into(),
            host_enabled: !d.host_enabled,
            host_chroma: !d.host_chroma,
            host_openrgb: !d.host_openrgb,
            host_obs: !d.host_obs,
            host_chroma_paint_mode: "boost".into(),
            host_chroma_paint_strength: 65,
            host_chroma_paint_fade_ms: 700,
            host_openrgb_paint_mode: "tint".into(),
            host_openrgb_paint_strength: 33,
            host_openrgb_paint_fade_ms: 250,
            host_paint_disabled_devices: vec!["unit-a".into(), "unit-b".into()],
            host_base_always_wins: !d.host_base_always_wins,
            host_obs_password: "test-pw".into(),
            lighting: d.lighting.clone(),
        };
        let body = toml::to_string_pretty(&want).unwrap();
        // corrupt one scalar's TYPE (string field → bool) so the whole-struct parse fails and the
        // field-by-field salvage runs. The anchor is an exact string literal (no float formatting risk).
        let corrupted = body.replacen("notif_sound = \"test-sound\"", "notif_sound = true", 1);
        assert_ne!(
            corrupted, body,
            "corruption anchor missing — the serialized form drifted"
        );
        std::fs::write(Prefs::path(), corrupted).unwrap();
        let got = Prefs::load();
        // only the corrupted field defaults; EVERY other field must have survived the salvage.
        let mut expect = want;
        expect.notif_sound = d.notif_sound.clone();
        assert_eq!(
            got, expect,
            "a field reverted to default → its `salvage!` line is missing from from_table_salvaging"
        );
    }

    /// A LEGACY data-mode save (`data = "mouse-battery"`) is a deserialize-only compat shim: it loads
    /// into `legacy_data`, `migrated()` folds it into a `vitals` LAYER, and it NEVER re-serializes — so
    /// the old field evaporates the next time the board's lighting is written. This is the zero-user-action
    /// migration path a pre-unification save rides on load.
    #[test]
    fn legacy_data_mode_migrates_to_a_vitals_layer() {
        let _g = cwd_guard();
        let pid = 0x0226u16;
        // an OLD save carries the data-mode slug under the device's `[lighting.<pid>]` table.
        std::fs::write(
            Prefs::path(),
            format!("[lighting.{}]\nfps = 6\ndata = \"mouse-battery\"\n", light_key(pid)),
        )
        .unwrap();
        let back = device_light(pid).expect("legacy record loads");
        assert_eq!(
            back.legacy_data.as_deref(),
            Some("mouse-battery"),
            "old `data` deserializes into the legacy_data shim"
        );
        assert!(back.layers.is_empty(), "the legacy save has no layers yet");
        // migration folds the data mode into a `vitals` layer (what load_lighting_into_state applies).
        let migrated = back.migrated();
        assert!(migrated.legacy_data.is_none(), "the shim is consumed by migration");
        assert_eq!(migrated.layers.len(), 1, "a vitals layer replaces the data mode");
        assert_eq!(migrated.layers[0].pattern, "vitals");
        // and a re-save drops the old field for good (legacy_data is skip_serializing).
        set_device_light(pid, migrated).expect("save migrated");
        assert!(
            !std::fs::read_to_string(Prefs::path()).unwrap().contains("data ="),
            "the legacy `data` key must not be re-serialized"
        );
    }

    /// `migrated()` is idempotent + guarded: a record that ALREADY carries a `vitals` layer doesn't grow a
    /// SECOND one even with a stray `legacy_data` shim set — the shim is still consumed (so it can't
    /// re-serialize), and the existing readout is left untouched. (No filesystem — pure in-memory fold.)
    #[test]
    fn migrated_does_not_duplicate_an_existing_vitals_layer() {
        let mut d = DeviceLight::default();
        d.layers.push(neuron::pattern::preset_layer("vitals").expect("vitals preset"));
        d.legacy_data = Some("mouse-battery".into());
        let m = d.migrated();
        assert!(m.legacy_data.is_none(), "the shim is consumed regardless");
        assert_eq!(m.layers.len(), 1, "an existing vitals layer is not duplicated");
        assert_eq!(m.layers[0].pattern, "vitals");
    }

    /// Only the known `mouse-battery` slug migrates; any OTHER/unknown legacy `data` value is DROPPED
    /// (consumed but never folded into a layer) — a forward-compat guard so a future or garbled slug can't
    /// silently graft a vitals readout onto a board.
    #[test]
    fn migrated_drops_an_unknown_legacy_data_value() {
        let mut d = DeviceLight::default();
        d.legacy_data = Some("something-else".into());
        let m = d.migrated();
        assert!(m.legacy_data.is_none(), "the unknown shim is still consumed");
        assert!(m.layers.is_empty(), "an unknown data value adds no layer");
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
                ..Default::default()
            },
        )
        .unwrap();
        let p = Prefs::load();
        assert!(!p.start_minimized, "sibling pref survives the lighting write");
        assert_eq!(p.ui_accent, "ff8800");
        assert_eq!(p.lighting.get(&light_key(0x0226)).map(|d| d.fps), Some(30));
    }

    /// RESILIENCE: ONE malformed field must not nuke the whole config. A file with several good values
    /// plus one bad one (a string where a float is wanted) loads everything else verbatim and defaults
    /// ONLY the bad field — the old all-or-nothing parse would have discarded the lot.
    #[test]
    fn one_bad_field_keeps_the_rest() {
        let _g = cwd_guard();
        std::fs::write(
            Prefs::path(),
            // notif_volume is malformed (a string, not an f32); everything else is valid.
            "start_minimized = false\n\
             ui_accent = \"ff8800\"\n\
             notif_enabled = false\n\
             notif_sound = \"warm\"\n\
             notif_volume = \"loud\"\n",
        )
        .unwrap();
        let p = Prefs::load();
        // the good siblings all survive
        assert!(!p.start_minimized, "good bool survives a bad sibling");
        assert_eq!(p.ui_accent, "ff8800", "good string survives");
        assert!(!p.notif_enabled, "good gate survives");
        assert_eq!(p.notif_sound, "warm", "good slug survives");
        // only the malformed field falls back to its own default
        assert_eq!(
            p.notif_volume,
            default_notif_volume(),
            "only the bad field defaults"
        );
    }

    /// RESILIENCE: a fully-valid (non-default) config round-trips through save→load unchanged, including
    /// the lighting map — the salvage path must never disturb a clean file.
    #[test]
    fn valid_config_round_trips_unchanged() {
        let _g = cwd_guard();
        let mut want = Prefs::default();
        want.start_minimized = false;
        want.ui_accent = "ff8800".into();
        want.weave_accent = "00aaff".into();
        want.notif_enabled = false;
        want.notif_placement = "bottom-left".into();
        want.notif_x = 0.0;
        want.notif_y = 1.0;
        want.notif_volume = 0.33;
        want.notif_sound = "glass".into();
        want.notif_stack = "digest".into();
        want.lighting.insert(
            light_key(0x0226),
            DeviceLight {
                fps: 24,
                layers: vec![neuron::pattern::preset_layer("vitals").unwrap()],
                ..Default::default()
            },
        );
        want.save().expect("save");
        let got = Prefs::load();
        assert!(!got.start_minimized);
        assert_eq!(got.ui_accent, "ff8800");
        assert_eq!(got.weave_accent, "00aaff");
        assert!(!got.notif_enabled);
        assert_eq!(got.notif_placement, "bottom-left");
        assert_eq!(got.notif_x, 0.0);
        assert_eq!(got.notif_y, 1.0);
        assert_eq!(got.notif_volume, 0.33);
        assert_eq!(got.notif_sound, "glass");
        assert_eq!(got.notif_stack, "digest");
        assert_eq!(
            got.lighting.get(&light_key(0x0226)).map(|d| d.fps),
            Some(24)
        );
        assert_eq!(
            got.lighting.get(&light_key(0x0226)).map(|d| d.layers.len()),
            Some(1),
            "the saved vitals layer round-trips (the data mode is now just a layer)"
        );
    }

    /// RESILIENCE: a file that sets only SOME fields defaults exactly the missing ones (the serde-default
    /// fast path) — present values read through, absent ones fall back, nothing else is touched.
    #[test]
    fn missing_fields_default_just_those() {
        let _g = cwd_guard();
        std::fs::write(Prefs::path(), "ui_accent = \"123456\"\n").unwrap();
        let p = Prefs::load();
        assert_eq!(p.ui_accent, "123456", "the present field reads through");
        assert!(p.start_minimized, "missing bool defaults true");
        assert_eq!(p.weave_accent, default_accent(), "missing string defaults");
        assert_eq!(p.notif_volume, default_notif_volume(), "missing f32 defaults");
        assert!(p.lighting.is_empty(), "missing map defaults empty");
    }

    /// RESILIENCE: a garbage (non-TOML) file yields all-defaults without panicking, and an empty file
    /// (valid TOML — an empty table) does too.
    #[test]
    fn garbage_or_empty_file_yields_defaults() {
        let _g = cwd_guard();
        std::fs::write(Prefs::path(), "this is not [valid toml = = @@@\n").unwrap();
        let p = Prefs::load();
        assert!(p.start_minimized, "garbage → default");
        assert_eq!(p.ui_accent, default_accent(), "garbage → default accent");

        std::fs::write(Prefs::path(), "").unwrap();
        let p = Prefs::load();
        assert!(p.start_minimized, "empty → default");
        assert_eq!(p.ui_accent, default_accent(), "empty → default accent");
    }

    /// RESILIENCE: within the lighting map, ONE corrupt device record drops only itself — the other
    /// boards' saved stacks AND the sibling top-level prefs all survive.
    #[test]
    fn one_bad_lighting_device_keeps_others() {
        let _g = cwd_guard();
        std::fs::write(
            Prefs::path(),
            "ui_accent = \"abcabc\"\n\
             [lighting.0226]\n\
             fps = 12\n\
             [lighting.00a8]\n\
             fps = \"fast\"\n", // malformed: fps must be an integer
        )
        .unwrap();
        let p = Prefs::load();
        assert_eq!(p.ui_accent, "abcabc", "top-level sibling survives bad lighting");
        assert_eq!(
            p.lighting.get("0226").map(|d| d.fps),
            Some(12),
            "the good board survives"
        );
        assert!(
            !p.lighting.contains_key("00a8"),
            "only the corrupt board is dropped"
        );
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
