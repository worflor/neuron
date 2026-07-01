//! Pattern — the SHAPE & MOTION half of the Spectrum lighting model (a layer = PATTERN × SPECTRUM).
//!
//! A **Pattern** owns the dynamics (heat sim, scroll, keypress ignite, ripple, flow field, meter, …)
//! and per tick emits a [`Field`]: for each cell a sample coordinate `u` (0..1, what to look up in the
//! layer's [`Spectrum`](crate::spectrum::Spectrum)) and an `intensity` (0..1, brightness/mask). A
//! pattern knows NOTHING about colour. It is stateful (`Box<dyn Pattern>`, one instance per layer).
//!
//! The render pipeline for a layer is: `field = pattern.field(rows, cols, t)`, then per cell
//! `cell = spectrum.at(t, u).scale_f(intensity)` ([`Field::render`]) — then (phase 3) the region mask
//! + blend over the layers below.
//!
//! **The Screen exception.** A screen-mirror (Ambient) pattern is inherently full-colour, so it emits
//! per-cell [`Rgb`] DIRECTLY via [`Field::Color`] — a passthrough that bypasses the 1-D spectrum.
//!
//! ## The registry — the single source of truth
//! Every pattern is defined ONCE in [`REGISTRY`] as a [`PatternDef`] carrying ALL its metadata
//! (`key`, `label`, the `make` factory, the typed param schema, a `default_spectrum`, tile meta). The
//! factory ([`make_pattern`]), the inspector param schema ([`pattern_params`]) and the tile catalog
//! ([`registry`]) ALL derive from this one table. **Adding a pattern = one [`PatternDef`] entry + the
//! [`Pattern`] impl — nothing else.** A registry-completeness test guarantees every entry is whole and
//! its default spectrum round-trips, so a half-registration fails the build.
//!
//! Patterns carry NO colour params (colour lives in the Spectrum); their schema uses only the existing
//! [`ParamKind::Range`] / [`Enum`](ParamKind::Enum) / [`Toggle`](ParamKind::Toggle).

use crate::effects::{Blend, Param, ParamKind};
use crate::lighting::Rgb;
use crate::spectrum::{self, Motion, Palette, Spectrum, Stop};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::f32::consts::{PI, TAU};

// ─────────────────────────────────────── Field & Pattern ─────────────────────────────────

/// One cell's sample from a [`Pattern`]: a spectrum lookup coordinate `u` (0..1) and a brightness
/// `intensity` (0..1). Colour is resolved later from the layer's spectrum.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Cell {
    pub u: f32,
    pub intensity: f32,
}

impl Cell {
    pub fn new(u: f32, intensity: f32) -> Cell {
        Cell { u, intensity }
    }
}

/// One tick of a pattern's output over the whole matrix (row-major, `rows*cols` entries).
///
/// `Scalar` is the normal case (per-cell `u`+`intensity`, coloured by the layer's spectrum). `Color`
/// is the Screen/Ambient exception — a full-colour pattern that emits `Rgb` directly, bypassing the
/// spectrum.
pub enum Field {
    Scalar(Vec<Cell>),
    Color(Vec<Rgb>),
}

impl Field {
    pub fn len(&self) -> usize {
        match self {
            Field::Scalar(v) => v.len(),
            Field::Color(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Resolve this field to concrete colours via `spectrum` at time `t`. A `Scalar` cell becomes
    /// `spectrum.at(t, u).scale_f(intensity)`; a `Color` cell passes through unchanged (the spectrum is
    /// ignored). This is the per-layer core of the render pipeline (region mask + blend come on top).
    pub fn render(&self, spectrum: &Spectrum, t: f32) -> Vec<Rgb> {
        match self {
            Field::Scalar(cells) => cells
                .iter()
                .map(|c| spectrum.at(t, c.u).scale_f(c.intensity))
                .collect(),
            Field::Color(px) => px.clone(),
        }
    }
}

/// A stateful shape-and-motion generator. One instance per layer; `field` is called once per tick.
pub trait Pattern {
    /// Apply the layer's param values. Called once when the layer is built and again when a knob
    /// changes. The default ignores params (for patterns that declare none).
    fn configure(&mut self, _params: &Params) {}

    /// Emit this tick's field for a `rows`×`cols` matrix at elapsed time `t` (seconds).
    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field;
}

// ──────────────────────────────────────────── Params ─────────────────────────────────────

/// A layer's pattern-param values, keyed by the schema's stable param `key`. Every value is stored as
/// an `f32` (an [`Enum`](ParamKind::Enum) index as a whole number, a [`Toggle`](ParamKind::Toggle) as
/// 0.0/1.0), so the bag stays flat and TOML-clean. Sparse by design: only overrides are stored, and a
/// pattern reads each knob with its own default, so an empty bag yields correct defaults.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Params(pub BTreeMap<String, f32>);

impl Params {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Read a continuous knob (or `default` if unset).
    pub fn f32(&self, key: &str, default: f32) -> f32 {
        self.0.get(key).copied().unwrap_or(default)
    }

    /// Read an enum-index knob (or `default` if unset), rounded to the nearest whole value.
    pub fn u8(&self, key: &str, default: u8) -> u8 {
        self.0
            .get(key)
            .map(|v| v.round().clamp(0.0, 255.0) as u8)
            .unwrap_or(default)
    }

    /// Read a toggle knob (or `default` if unset) — true at/above 0.5.
    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.0.get(key).map(|v| *v >= 0.5).unwrap_or(default)
    }

    /// Set a knob value.
    pub fn set(&mut self, key: impl Into<String>, v: f32) {
        self.0.insert(key.into(), v);
    }

    /// A param bag pre-filled with every default from a pattern's schema — the starting values the
    /// inspector shows for a freshly-applied layer. Unknown patterns yield an empty bag.
    pub fn defaults_for(pattern: &str) -> Params {
        let mut p = Params::default();
        for prm in pattern_params(pattern) {
            match prm.kind {
                ParamKind::Range { default, .. } => p.set(prm.key, default),
                ParamKind::Enum { default, .. } => p.set(prm.key, default as f32),
                ParamKind::Toggle { default } => p.set(prm.key, if default { 1.0 } else { 0.0 }),
                ParamKind::Color => {} // patterns carry no colour params (colour is the spectrum)
            }
        }
        p
    }
}

// ─────────────────────────────────────────── LayerDef ────────────────────────────────────

/// A serialisable layer: a pattern (by key) + its params + the colour [`Spectrum`] + region mask +
/// blend. The new shape of a lighting layer — the GUI sends it and a profile persists it; the live
/// pattern (with its stateful generator) is built from this on demand (phase 2/3).
///
/// Serialises FLAT for the common case: `pattern`/`blend` are strings, `region` an int array,
/// `enabled` a bool, `params` is omitted when empty, and `spectrum` is a bare hex string for a solid
/// (the tiered Spectrum serde). Only a layer with param overrides or a motion/sequence spectrum grows
/// a sub-table. `#[serde(default)]` makes every field optional so a hand-written record stays valid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerDef {
    pub pattern: String,
    #[serde(skip_serializing_if = "Params::is_empty")]
    pub params: Params,
    pub spectrum: Spectrum,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub region: Vec<u32>,
    pub blend: Blend,
    pub enabled: bool,
}

impl Default for LayerDef {
    fn default() -> Self {
        LayerDef {
            pattern: "uniform".into(),
            params: Params::default(),
            spectrum: Spectrum::solid(Rgb::new(0x4A, 0xF2, 0xB0)),
            region: Vec::new(),
            blend: Blend::Normal,
            enabled: true,
        }
    }
}

impl LayerDef {
    /// Build the live, configured [`Pattern`] for this layer (or `None` if the pattern key is unknown).
    pub fn make_pattern(&self) -> Option<Box<dyn Pattern>> {
        let mut p = make_pattern(&self.pattern)?;
        p.configure(&self.params);
        Some(p)
    }
}

// ───────────────────────────────────────── the registry ──────────────────────────────────

/// Tile / catalog metadata for a pattern — what the (phase-3) preset grid shows about it.
#[derive(Clone, Copy, Debug)]
pub struct TileMeta {
    /// A one-line human blurb for the tile.
    pub blurb: &'static str,
    /// Does the pattern need LIVE input (keys / audio / screen / telemetry) to show anything? Headless
    /// previews and the no-dead-knob sweeps skip these (they render dark without a live source).
    pub live_input: bool,
}

/// The single definition of one pattern: its stable `key`, display `label`, the `make` factory, the
/// typed param schema, a `default_spectrum`, and tile meta. The factory, the inspector schema and the
/// tile catalog ALL derive from this — adding a pattern is ONE entry here plus the [`Pattern`] impl.
///
/// All fields are `'static`/fn-pointer so the whole [`REGISTRY`] is a `const` table (the single source
/// of truth). `params` and `default_spectrum` are fns because their values allocate (a `Vec` / a
/// `Spectrum` with `Vec` stops) and so can't be `const` data.
pub struct PatternDef {
    pub key: &'static str,
    pub label: &'static str,
    pub make: fn() -> Box<dyn Pattern>,
    pub params: fn() -> Vec<Param>,
    pub default_spectrum: fn() -> Spectrum,
    pub tile: TileMeta,
}

/// The registry — the SINGLE source of truth for every pattern. The factory ([`make_pattern`]), the
/// inspector schema ([`pattern_params`]), the default spectra and the tile catalog ALL derive from
/// this one table. Adding a pattern = ONE entry here + the [`Pattern`] impl — nothing else.
///
/// The twelve shapes the whole effect set collapses to (the colour of each lives in its
/// [`Spectrum`], chosen per preset — see [`presets`]): `uniform` (Static/Breathing/Cycle), `axis`
/// (Wave), `radial` (Color Wheel), `heat` (Fire), `streak` (Cascade rain + Comet), `sparkle`
/// (Starlight), `ignite` (Reactive), `ring` (Ripple), `flow` (Aurora), `thermal` (Typing Heat),
/// `meter` (Audio Meter + Pulse) and `screen` (Ambient — the full-colour exception).
static REGISTRY: &[PatternDef] = &[
    PatternDef {
        key: "uniform",
        label: "Uniform",
        make: || Box::new(Uniform),
        params: Vec::new,
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "one colour across the whole board",
            live_input: false,
        },
    },
    PatternDef {
        key: "axis",
        label: "Axis",
        make: || Box::new(Axis::default()),
        params: || vec![direction_param(), speed_param()],
        default_spectrum: spectrum::rainbow,
        tile: TileMeta {
            blurb: "a gradient scrolling along an axis (the wave shape)",
            live_input: false,
        },
    },
    PatternDef {
        key: "radial",
        label: "Radial",
        make: || Box::new(Radial::default()),
        params: || vec![direction_param(), speed_param()],
        default_spectrum: spectrum::rainbow,
        tile: TileMeta {
            blurb: "a hue wheel turning around the centre",
            live_input: false,
        },
    },
    PatternDef {
        key: "heat",
        label: "Heat",
        make: || Box::new(Heat::default()),
        params: || vec![speed_param(), density_param()],
        default_spectrum: fire_spectrum,
        tile: TileMeta {
            blurb: "an upward fire — heat rises, flickers and cools",
            live_input: false,
        },
    },
    PatternDef {
        key: "streak",
        label: "Streak",
        make: || Box::new(Streak::default()),
        params: || vec![mode_param(), speed_param(), density_param()],
        default_spectrum: streak_spectrum,
        tile: TileMeta {
            blurb: "falling rain or streaking comets, tail to head",
            live_input: false,
        },
    },
    PatternDef {
        key: "sparkle",
        label: "Sparkle",
        make: || Box::new(Sparkle::default()),
        params: || vec![speed_param(), density_param(), fade_param()],
        default_spectrum: || Spectrum::solid(Rgb::new(0xFF, 0xFF, 0xE0)),
        tile: TileMeta {
            blurb: "random twinkles igniting and fading like stars",
            live_input: false,
        },
    },
    PatternDef {
        key: "ignite",
        label: "Ignite",
        make: || Box::new(Ignite::default()),
        params: || vec![fade_param(), glow_param()],
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "lights the key you press, then fades",
            live_input: true,
        },
    },
    PatternDef {
        key: "ring",
        label: "Ring",
        make: || Box::new(Ring::default()),
        params: || vec![speed_param(), fade_param()],
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "a keypress sends a ring rippling outward",
            live_input: true,
        },
    },
    PatternDef {
        key: "flow",
        label: "Flow",
        make: || Box::new(Flow::default()),
        params: || vec![speed_param()],
        default_spectrum: aurora_spectrum,
        tile: TileMeta {
            blurb: "a slow aurora flow drifting over the board",
            live_input: false,
        },
    },
    PatternDef {
        key: "thermal",
        label: "Thermal",
        make: || Box::new(Thermal::default()),
        params: || vec![sensitivity_param(), fade_param()],
        default_spectrum: thermal_spectrum,
        tile: TileMeta {
            blurb: "your typing rendered as a living heat map",
            live_input: true,
        },
    },
    PatternDef {
        key: "meter",
        label: "Meter",
        make: || Box::new(Meter::default()),
        params: || vec![source_param(), speed_param()],
        default_spectrum: meter_spectrum,
        tile: TileMeta {
            blurb: "a live meter — audio level or system load",
            live_input: true,
        },
    },
    PatternDef {
        key: "screen",
        label: "Screen",
        make: || Box::new(Screen::default()),
        params: || vec![speed_param(), saturation_param()],
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "the board mirrors the colours on your screen",
            live_input: true,
        },
    },
];

/// The house default accent (the weave teal) — the neutral colour solid-spectrum patterns start in.
const ACCENT: Rgb = Rgb::new(0x4A, 0xF2, 0xB0);

/// The full registry slice — the tile catalog reads this.
pub fn registry() -> &'static [PatternDef] {
    REGISTRY
}

/// Look up a pattern definition by key (case-insensitive).
pub fn pattern_def(key: &str) -> Option<&'static PatternDef> {
    REGISTRY.iter().find(|d| d.key.eq_ignore_ascii_case(key))
}

/// Build a fresh, UNCONFIGURED pattern instance by key (the factory). `None` for an unknown key.
/// (Use [`LayerDef::make_pattern`] to build one already configured with a layer's params.)
pub fn make_pattern(key: &str) -> Option<Box<dyn Pattern>> {
    pattern_def(key).map(|d| (d.make)())
}

/// The typed param schema the inspector auto-renders for a pattern (empty for an unknown key).
pub fn pattern_params(key: &str) -> Vec<Param> {
    pattern_def(key).map(|d| (d.params)()).unwrap_or_default()
}

/// The built-in default spectrum for a pattern (`None` for an unknown key).
pub fn default_spectrum(key: &str) -> Option<Spectrum> {
    pattern_def(key).map(|d| (d.default_spectrum)())
}

/// Every registered pattern key, in registry order.
pub fn pattern_keys() -> Vec<&'static str> {
    REGISTRY.iter().map(|d| d.key).collect()
}

// shared param-schema constructors (reused across pattern defs so the ranges read consistently)

/// A `speed` rate knob (the design default is 1.0).
fn speed_param() -> Param {
    Param {
        key: "speed",
        label: "speed",
        kind: ParamKind::Range {
            min: 0.25,
            max: 4.0,
            default: 1.0,
        },
    }
}

/// A 4-way `direction` knob (→ ← ↑ ↓).
fn direction_param() -> Param {
    Param {
        key: "direction",
        label: "direction",
        kind: ParamKind::Enum {
            options: &["→", "←", "↑", "↓"],
            default: 0,
        },
    }
}

/// A `density` population/intensity knob (fire height, rain busy-ness, comet count) — default 1.0.
fn density_param() -> Param {
    Param {
        key: "density",
        label: "density",
        kind: ParamKind::Range {
            min: 0.25,
            max: 3.0,
            default: 1.0,
        },
    }
}

/// A `fade` decay-rate knob for ignite-then-fade shapes (Sparkle/Ignite/Ring) — default 1.0.
fn fade_param() -> Param {
    Param {
        key: "fade",
        label: "fade",
        kind: ParamKind::Range {
            min: 0.25,
            max: 3.0,
            default: 1.0,
        },
    }
}

/// Thermal's `sensitivity` — how hot each keypress lands (the deposit-strength gain). Default 1.0.
fn sensitivity_param() -> Param {
    Param {
        key: "sensitivity",
        label: "sensitivity",
        kind: ParamKind::Range {
            min: 0.25,
            max: 3.0,
            default: 1.0,
        },
    }
}

/// Screen's `saturation` pop knob — 1.0 = faithful screen colour, higher = more saturated.
fn saturation_param() -> Param {
    Param {
        key: "saturation",
        label: "saturation",
        kind: ParamKind::Range {
            min: 1.0,
            max: 3.0,
            default: 1.0,
        },
    }
}

/// Streak's `mode` — falling rain vs streaking comets (the two streak shapes).
fn mode_param() -> Param {
    Param {
        key: "mode",
        label: "mode",
        kind: ParamKind::Enum {
            options: &["rain", "comet"],
            default: 0,
        },
    }
}

/// The reactive neighbour-`glow` toggle (Ignite) — off lights only the pressed key's cell.
fn glow_param() -> Param {
    Param {
        key: "glow",
        label: "neighbour glow",
        kind: ParamKind::Toggle { default: false },
    }
}

/// Meter's `source` — the live signal driving the bars: speaker output, mic, CPU, RAM, or the
/// combined CPU/RAM "load" view (the old Pulse). The label IS the value (data-driven, no bespoke UI).
fn source_param() -> Param {
    Param {
        key: "source",
        label: "source",
        kind: ParamKind::Enum {
            options: &["speakers", "mic", "cpu", "ram", "load"],
            default: 0,
        },
    }
}

// ──────────────────────────────────── sample pattern impls ───────────────────────────────
//
// Three complete patterns that seed the registry and exercise the foundation end-to-end. Phase 2 ports
// the rest (Heat, Streak, Sparkle, Ignite, Ring, Flow, Thermal, Meter, Screen, …) by adding a
// PatternDef entry + the impl — no other code changes.

/// Uniform: every cell samples the spectrum at the SAME coordinate (`u = 0`) at full intensity. With a
/// solid spectrum that's a static colour; with a Cycle-motion spectrum the whole board cycles together;
/// with a Breathe-motion spectrum it breathes — the "Static / Breathing / Cycle" family, all from one
/// shape × different spectra.
pub struct Uniform;

impl Pattern for Uniform {
    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let n = rows as usize * cols as usize;
        Field::Scalar(vec![Cell::new(0.0, 1.0); n])
    }
}

/// Axis: a gradient laid along one axis that SCROLLS over time — the "wave" shape. Each cell's `u` is
/// its position along the chosen axis, shifted by `t * speed`, so a static gradient spectrum slides
/// across the board. Intensity is full; depth comes from the spectrum. The colour is entirely the
/// spectrum's job (rainbow by default).
#[derive(Default)]
pub struct Axis {
    direction: u8,
    speed: f32,
}

impl Pattern for Axis {
    fn configure(&mut self, p: &Params) {
        self.direction = p.u8("direction", 0);
        self.speed = p.f32("speed", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let mut cells = vec![Cell::default(); r * c];
        // SCROLL_RATE: spectrum-spans per second at speed 1.0 (a calm travelling gradient).
        const SCROLL_RATE: f32 = 0.2;
        let shift = t * self.speed * SCROLL_RATE;
        // Half-open [0,1) positions (x/c, not x/(c-1)) so the gradient TILES seamlessly under the
        // scroll wrap — no endpoint collides at exactly 1.0 (which `rem_euclid` would fold to 0.0).
        let cf = c.max(1) as f32;
        let rf = r.max(1) as f32;
        for y in 0..r {
            for x in 0..c {
                let pos = match self.direction {
                    1 => (c - 1 - x) as f32 / cf,  // ← (decreasing left→right)
                    2 => (r - 1 - y) as f32 / rf,  // ↑ (decreasing top→bottom)
                    3 => y as f32 / rf,            // ↓
                    _ => x as f32 / cf,            // → (increasing left→right)
                };
                cells[y * c + x] = Cell::new((pos + shift).rem_euclid(1.0), 1.0);
            }
        }
        Field::Scalar(cells)
    }
}

/// Radial: a hue wheel anchored on the matrix centre — each cell's `u` is its ANGLE around the centre,
/// the whole wheel spinning over time (the "Color Wheel" shape). Intensity is a soft dome (brightest at
/// the hub, easing to the rim) so the wheel reads with rounded depth. With a rainbow spectrum this is a
/// true radial rainbow.
#[derive(Default)]
pub struct Radial {
    direction: u8,
    speed: f32,
}

impl Pattern for Radial {
    fn configure(&mut self, p: &Params) {
        self.direction = p.u8("direction", 0);
        self.speed = p.f32("speed", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let mut cells = vec![Cell::default(); r * c];
        let cx = (c as f32 - 1.0) / 2.0;
        let cy = (r as f32 - 1.0) / 2.0;
        let max_rad = (cx * cx + cy * cy).sqrt().max(1.0);
        // SPIN_RATE: turns per second at speed 1.0. DIRECTION sets the spin SIGN — →/↓ (0/3) turn one
        // way, ←/↑ (1/2) the other (a radial wheel has no L/R/U/D axis, so direction reads as cw vs ccw).
        const SPIN_RATE: f32 = 0.15;
        let dir = if matches!(self.direction, 1 | 2) { -1.0 } else { 1.0 };
        let spin = t * self.speed * SPIN_RATE * dir;
        for y in 0..r {
            for x in 0..c {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                // angle 0..1 around the centre, advanced by the spin
                let ang = dy.atan2(dx) / (2.0 * PI) + 0.5; // 0..1
                let u = (ang + spin).rem_euclid(1.0);
                // soft dome: 1.0 at the hub easing to 0.7 at the rim — rounded depth, never dark.
                let rad = (dx * dx + dy * dy).sqrt() / max_rad;
                let intensity = 1.0 - 0.30 * rad;
                cells[y * c + x] = Cell::new(u, intensity.clamp(0.0, 1.0));
            }
        }
        Field::Scalar(cells)
    }
}

// ─────────────────────────── shared pattern primitives (PRNG + live keys) ─────────────────────

/// A lean deterministic xorshift32 step → 0.0..1.0 (no `rand` dependency — the project stays lean).
/// The stateful patterns (Heat/Streak/Sparkle) carry their seed as a `u32` (so they `#[derive(Default)]`
/// freely); the zero-guard means an unseeded `0` still produces a stable stream on first use.
fn xorshift(state: &mut u32) -> f32 {
    let mut x = if *state == 0 { 0x9E37_79B9 } else { *state };
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

/// The number of down-state slots [`scan_key_presses`] tracks: the 256 Windows virtual-keys plus the
/// 6 Razer macro keys (M1..M6), which don't ride VKs. Every pattern that owns a `prev` for the scan must
/// size it to this so the macro slots (`256..256+6`) exist; a shorter slice still can't panic (the macro
/// loop is length-guarded), it just won't react to macro keys.
pub const KEY_SCAN_SLOTS: usize = 256 + 6;

/// Scan the LIVE keyboard for fresh key-DOWN edges (the safe `capture::key_down` read — never injects).
/// Updates `prev` (last-frame down-state for VK 1..256) and calls `on_press(ry, cx)` for each fresh
/// press that resolves to a real board cell within `r`×`c` via the standard Razer keymap
/// ([`crate::lighting::vk_to_key_cell`]). A key the map doesn't carry (mouse buttons, generic modifiers,
/// media keys) resolves to None and fires nothing — an accurate reactive surface never lights a key you
/// didn't press. Returns the TOTAL fresh-press count (incl. unmapped keys), which Thermal uses as its
/// typing-rate signal. Off Windows the read is a no-op, so nothing ever fires.
///
/// Beyond the VKs it also scans the 6 Razer macro keys (M1..M6), which arrive on Razer's Driver-Mode
/// `0x04` report — NOT as VKs — via [`crate::capture::macro_key_down`] (suppression-aware, like
/// `key_down`). Their down-state lives in `prev` slots `256..256+6` ([`KEY_SCAN_SLOTS`]) and uses the
/// SAME down-edge logic, so a pressed macro key lights its cell ([`crate::lighting::razer_key_cell`] of
/// [`crate::lighting::MACRO_KEY_NAMES`]) — fixing the macro column going dark on the live-input effects.
fn scan_key_presses(
    prev: &mut [bool],
    r: usize,
    c: usize,
    mut on_press: impl FnMut(usize, usize),
) -> u32 {
    let mut presses = 0u32;
    for vk in 1..256usize {
        let down = crate::capture::key_down(vk as i32);
        if down && !prev[vk] {
            presses += 1;
            if let Some((ry, cx)) = crate::lighting::vk_to_key_cell(vk as i32) {
                let (ry, cx) = (ry as usize, cx as usize);
                if ry < r && cx < c {
                    on_press(ry, cx);
                }
            }
        }
        prev[vk] = down;
    }
    // The macro keys (M1..M6) don't ride Windows VKs — bridge their shared held-state with the SAME
    // down-edge logic so the same press lights the same cell. Length-guarded so a `prev` shorter than
    // KEY_SCAN_SLOTS can never panic (it just won't react). A name with no cell on this board fires nothing.
    for i in 0..crate::lighting::MACRO_KEY_NAMES.len() {
        let slot = 256 + i;
        if slot >= prev.len() {
            break;
        }
        let down = crate::capture::macro_key_down(i);
        if down && !prev[slot] {
            presses += 1;
            if let Some((ry, cx)) = crate::lighting::razer_key_cell(crate::lighting::MACRO_KEY_NAMES[i]) {
                let (ry, cx) = (ry as usize, cx as usize);
                if ry < r && cx < c {
                    on_press(ry, cx);
                }
            }
        }
        prev[slot] = down;
    }
    presses
}

// ───────────────────────────────────── Heat (the Fire shape) ──────────────────────────────────

/// Heat: an upward-propagating fire SIM. The bottom row is seeded white-hot with flicker; heat
/// diffuses up and cools, so the field is a black→hot gradient that licks and gutters. The colour ramp
/// (ember→white) lives entirely in the spectrum: each cell emits `u = heat` (the ramp coordinate) and
/// `intensity = per-column flicker`, so `spectrum.at(t, heat)` paints the flame and the flicker makes it
/// dance. `speed` scales the sim rate (more heat-steps/sec → a churning fire); `density` scales how
/// tall/full the flame climbs. Deterministic PRNG (no `rand`). The hard-won fire physics, re-expressed.
#[derive(Default)]
pub struct Heat {
    heat: Vec<f32>,
    dims: (u8, u8),
    rng: u32,
    speed: f32,
    density: f32,
    last_t: f32,
    step_acc: f32,
}

impl Heat {
    /// Advance the heat field one simulation step: re-seed the flickering bottom row, then propagate
    /// upward with density-scaled cooling. Re-rolling the flicker each step makes the step COUNT the
    /// flicker/propagation rate — which is how `speed` is honoured.
    fn step(&mut self, r: usize, c: usize) {
        let bottom = (r - 1) * c;
        for x in 0..c {
            self.heat[bottom + x] = 0.90 + xorshift(&mut self.rng) * 0.10;
        }
        // DENSITY scales how much heat survives the climb: higher → less cooling → a taller, fuller flame.
        let dens = self.density.clamp(0.25, 3.0);
        let cool_scale = (1.0 / dens).clamp(0.4, 2.0);
        for y in 0..r - 1 {
            for x in 0..c {
                let below = (y + 1) * c + x;
                let bl = (y + 1) * c + (x + c - 1) % c;
                let br = (y + 1) * c + (x + 1) % c;
                let avg = (self.heat[below] * 2.0 + self.heat[bl] + self.heat[br]) / 4.0;
                let cool = (0.14 + xorshift(&mut self.rng) * 0.10) * cool_scale;
                self.heat[y * c + x] = (avg - cool).max(0.0);
            }
        }
    }
}

impl Pattern for Heat {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
        self.density = p.f32("density", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.heat = vec![0.0; n];
            self.rng = 0x9E37_79B9;
            self.dims = (rows, cols);
            self.last_t = t;
            self.step_acc = 0.0;
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        // SPEED drives the sim from elapsed time: ~18 steps/sec at speed 1.0, accumulating the fraction.
        // A static `t` still steps once (never freezes); a time reset (dt<0) steps once; the burst is
        // capped so a long stall can't run thousands of steps in one frame.
        const BASE_STEPS_PER_SEC: f32 = 18.0;
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.speed.clamp(0.1, 6.0);
        self.step_acc += dt * BASE_STEPS_PER_SEC * spd;
        let mut steps = self.step_acc.floor().max(1.0) as u32;
        self.step_acc -= self.step_acc.floor();
        steps = steps.min(8);
        for _ in 0..steps {
            self.step(r, c);
        }
        // emit (u = heat, intensity = per-column flicker). The flicker DEPTH ramps with heat (steady
        // embers, dancing tips), so the white-hot licks flare and gutter while the base stays a calm bed.
        let cells = self
            .heat
            .iter()
            .enumerate()
            .map(|(i, &h)| Cell::new(h.clamp(0.0, 1.0), fire_flicker(i % c, t, spd, h)))
            .collect();
        Field::Scalar(cells)
    }
}

/// Per-column luminance flicker in 0..1 — multiplies a cell's heat-colour so the flame varies in
/// BRIGHTNESS, not only along the ramp. Two incommensurate sines per column make an organic wobble; the
/// DEPTH ramps with heat (hot tips flicker hard, cool embers barely waver). ≥0.5 even at the tips so a
/// lick dims but never blinks fully out. A pure fn of column + time (the sim's seed row carries the rand).
fn fire_flicker(x: usize, t: f32, speed: f32, heat: f32) -> f32 {
    let xf = x as f32;
    let a = (t * 9.0 * speed + xf * 1.7).sin();
    let b = (t * 13.0 * speed + xf * 0.6 + 2.0).sin();
    let mix = 0.5 + 0.5 * (0.6 * a + 0.4 * b);
    let depth = 0.06 + 0.34 * heat.clamp(0.0, 1.0);
    (1.0 - depth + depth * mix).clamp(0.0, 1.0)
}

// ──────────────────────────── Streak (the Cascade rain + Comet shapes) ─────────────────────────

/// One comet in the parade — a continuous float head `(x, y)` on a unit velocity `(vx, vy)`, plus a
/// fresh set of per-comet traits so no two are alike. `respawn` is the lifecycle clock: `0.0` = ALIVE
/// and streaking; `> 0.0` = DEAD, counting sim-steps down to a fresh respawn (no edge wrap, no loop).
#[derive(Clone, Copy, Debug, PartialEq)]
struct CometBody {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    speed_mul: f32,
    trail: f32,
    bright: f32,
    respawn: f32,
}

/// Streak: bright shapes streaking across the board, tail → head, coloured by the spectrum (default a
/// tail-colour → white-head gradient). Two modes:
///   * **rain** (Cascade): each column runs an independent vertical drop — a white-hot head falling with
///     a fading coloured tail; drops spawn staggered and respawn after a random gap, a living downpour.
///   * **comet** (Comet): an endless PARADE of varied, cardinal-biased streaks on free velocity vectors
///     that DON'T wrap — a comet runs off the board, dies, and a fresh DIFFERENT one enters shortly
///     after; a keypress on a live head BREAKS it (a white-hot burst) and respawns it different.
///
/// `speed` is the fall/travel rate, `density` the population (rain busy-ness / comet count: 1 by default,
/// up to ~7). Both modes emit per cell `(u, intensity)`: `intensity` is the streak brightness; `u` rises
/// toward the head (so the head reads as the spectrum's hot/white end, the tail as its cool end). Driven
/// by a fixed-rate step accumulator so the look is identical at the legacy 6fps and at 30/60fps.
#[derive(Default)]
pub struct Streak {
    mode: u8,
    speed: f32,
    density: f32,
    dims: (u8, u8),
    rng: u32,
    last_t: f32,
    step_acc: f32,
    // rain state
    level: Vec<f32>,
    head: Vec<f32>,
    active: Vec<bool>,
    wait: Vec<f32>,
    // comet state
    comets: Vec<CometBody>,
    burst: Vec<f32>,
    prev: Vec<bool>,
}

impl Streak {
    fn rand(&mut self) -> f32 {
        xorshift(&mut self.rng)
    }

    // ── rain (Cascade) ──────────────────────────────────────────────────────────────────────────

    /// Average respawn gap in sim-steps — shorter at higher density (busier downpour), longer at low.
    fn rain_gap(&self) -> f32 {
        const BASE_GAP_STEPS: f32 = 40.0;
        (BASE_GAP_STEPS / self.density.clamp(0.25, 3.0)).max(2.0)
    }

    fn rain_spawn(&mut self, col: usize) {
        self.active[col] = true;
        self.head[col] = -(self.rand() * 3.0);
    }

    /// Seed the per-column rain so the board is already raining on the first frame; active probability
    /// scales with `density` (a dense rain begins nearly full, a sparse one mostly empty).
    fn rain_init(&mut self, r: usize, c: usize) {
        let dens = self.density.clamp(0.25, 3.0);
        let p_active = (0.30 + 0.23 * dens).clamp(0.0, 0.95);
        let g = self.rain_gap();
        for x in 0..c {
            if self.rand() < p_active {
                self.active[x] = true;
                self.head[x] = self.rand() * r as f32;
            } else {
                self.active[x] = false;
                self.wait[x] = self.rand() * g;
            }
        }
    }

    /// Advance the rain one step: fade every trail a notch (the exponential tail), then drop each active
    /// head one notch (painting white-hot where it lands) or count down a waiting column's respawn gap.
    fn rain_step(&mut self, r: usize, c: usize) {
        const DECAY: f32 = 0.80;
        const ADVANCE: f32 = 0.22;
        for v in self.level.iter_mut() {
            *v *= DECAY;
            if *v < 0.02 {
                *v = 0.0;
            }
        }
        for x in 0..c {
            if self.active[x] {
                let h = self.head[x];
                if h >= 0.0 && (h as usize) < r {
                    self.level[h as usize * c + x] = 1.0;
                }
                let nh = h + ADVANCE;
                self.head[x] = nh;
                if nh >= r as f32 + 4.0 {
                    self.active[x] = false;
                    self.wait[x] = self.rain_gap() * (0.5 + self.rand());
                }
            } else {
                self.wait[x] -= 1.0;
                if self.wait[x] <= 0.0 {
                    self.rain_spawn(x);
                }
            }
        }
    }

    // ── comet ───────────────────────────────────────────────────────────────────────────────────

    /// How many comets stream at once: density 1.0 → exactly ONE calm streak; up the knob for a swarm
    /// (up to ~7 at 3.0); never fewer than one. Monotonic — this is how `density` is honoured for comets.
    fn comet_count(&self) -> usize {
        let d = self.density.clamp(0.25, 3.0);
        (1.0 + (d - 1.0).max(0.0) * 3.0).round().clamp(1.0, 8.0) as usize
    }

    /// Roll a brand-NEW comet — everything fresh from the PRNG so no two are alike: a cardinal-biased,
    /// axis-COUPLED direction (clean H/V common, a gentle lean frequent, a true ~45° rake rare) with the
    /// entry edge coupled to it, plus its own pace, trail length and head brightness. Born ALIVE.
    fn spawn_body(&mut self, r: usize, c: usize) -> CometBody {
        let (rf, cf) = (r.max(1) as f32, c.max(1) as f32);
        // PRIMARY AXIS — the board's long axis gently favoured, clamped so both axes stay populated.
        let p_horizontal = (cf / (cf + rf)).clamp(0.4, 0.6);
        let horizontal = self.rand() < p_horizontal;
        let positive = self.rand() < 0.5;
        // PERPENDICULAR DRIFT cubed to pile mass near zero (most near-cardinal; the thin tail reaches 45°).
        let drift_sign = if self.rand() < 0.5 { -1.0 } else { 1.0 };
        let u = self.rand();
        let drift = drift_sign * u * u * u;
        let inv = 1.0 / (1.0 + drift * drift).sqrt();
        let (x, y, vx, vy) = if horizontal {
            let dir = if positive { 1.0 } else { -1.0 };
            let x0 = if positive { -1.0 } else { cf };
            let y0 = self.rand() * rf;
            (x0, y0, dir * inv, drift * inv)
        } else {
            let dir = if positive { 1.0 } else { -1.0 };
            let y0 = if positive { -1.0 } else { rf };
            let x0 = self.rand() * cf;
            (x0, y0, drift * inv, dir * inv)
        };
        let speed_mul = 0.6 + self.rand() * 1.0;
        let span = (rf * rf + cf * cf).sqrt();
        let trail = (span * (0.18 + self.rand() * 0.45)).max(2.5);
        let bright = 0.82 + self.rand() * 0.18;
        CometBody { x, y, vx, vy, speed_mul, trail, bright, respawn: 0.0 }
    }

    /// Paint a bright radial BURST into the break-flash field at `(x, y)` — the comet-break shatter. The
    /// core is pushed ABOVE 1.0 so the render drives it to the spectrum's hot/white end (the brightest
    /// moment); it falls off to a glow at the rim. Out-of-board cells are clipped (no wrap).
    fn paint_burst(&mut self, x: f32, y: f32, r: usize, c: usize) {
        const BURST_RADIUS: f32 = 2.2;
        let bx = x.round() as isize;
        let by = y.round() as isize;
        let reach = BURST_RADIUS.ceil() as isize;
        for dy in -reach..=reach {
            for dx in -reach..=reach {
                let d = ((dx * dx + dy * dy) as f32).sqrt();
                if d > BURST_RADIUS {
                    continue;
                }
                let gx = bx + dx;
                let gy = by + dy;
                if gx < 0 || gy < 0 || gx >= c as isize || gy >= r as isize {
                    continue;
                }
                let inten = 1.0 - d / BURST_RADIUS;
                let val = 0.6 + 0.9 * inten;
                let i = gy as usize * c + gx as usize;
                self.burst[i] = self.burst[i].max(val);
            }
        }
    }

    /// A press at cell `(pr, pc)`: BREAK every live comet whose head sits within the hit radius — a burst
    /// at the impact + a fresh respawn (so a broken comet comes back DIFFERENT). Dead comets are skipped.
    /// Returns whether anything broke. Pure of I/O (the live key read happens in `field`), so it's testable.
    fn break_at(&mut self, pr: f32, pc: f32, r: usize, c: usize) -> bool {
        const HIT_RADIUS: f32 = 1.5;
        let mut broke = false;
        for i in 0..self.comets.len() {
            if self.comets[i].respawn > 0.0 {
                continue;
            }
            let dx = self.comets[i].x - pc;
            let dy = self.comets[i].y - pr;
            if (dx * dx + dy * dy).sqrt() <= HIT_RADIUS {
                let (bx, by) = (self.comets[i].x, self.comets[i].y);
                self.paint_burst(bx, by, r, c);
                self.comets[i] = self.spawn_body(r, c);
                broke = true;
            }
        }
        broke
    }

    fn comet_fully_off(&self, i: usize, r: usize, c: usize) -> bool {
        let b = &self.comets[i];
        let m = b.trail + 2.0;
        b.x < -m || b.x > (c as f32 - 1.0) + m || b.y < -m || b.y > (r as f32 - 1.0) + m
    }

    /// Advance the comet parade one step: fade the break-flash field; then each comet either counts down
    /// its respawn gap (and re-rolls when it elapses) or — if alive — moves along its velocity (no wrap)
    /// and dies once it has fully left the board. Step count IS the travel rate — how `speed` is honoured.
    fn comet_step(&mut self, r: usize, c: usize) {
        const BURST_DECAY: f32 = 0.80;
        const ADVANCE: f32 = 0.45;
        const RESPAWN_MIN: f32 = 1.0;
        const RESPAWN_SPAN: f32 = 6.0;
        for v in self.burst.iter_mut() {
            *v *= BURST_DECAY;
            if *v < 0.02 {
                *v = 0.0;
            }
        }
        for i in 0..self.comets.len() {
            if self.comets[i].respawn > 0.0 {
                self.comets[i].respawn -= 1.0;
                if self.comets[i].respawn <= 0.0 {
                    self.comets[i] = self.spawn_body(r, c);
                }
                continue;
            }
            let dist = ADVANCE * self.comets[i].speed_mul;
            self.comets[i].x += self.comets[i].vx * dist;
            self.comets[i].y += self.comets[i].vy * dist;
            if self.comet_fully_off(i, r, c) {
                self.comets[i].respawn = RESPAWN_MIN + self.rand() * RESPAWN_SPAN;
            }
        }
    }

    /// Steps the sim by elapsed-time accumulation (~`base`/sec at speed 1.0), shared by both modes so the
    /// look is fps-independent. Returns how many whole steps to run this frame (≥1, capped at 8).
    fn accrue_steps(&mut self, t: f32, base_per_sec: f32) -> u32 {
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.speed.clamp(0.1, 6.0);
        self.step_acc += dt * base_per_sec * spd;
        let steps = self.step_acc.floor().max(1.0) as u32;
        self.step_acc -= self.step_acc.floor();
        steps.min(8)
    }
}

impl Pattern for Streak {
    fn configure(&mut self, p: &Params) {
        self.mode = p.u8("mode", 0);
        self.speed = p.f32("speed", 1.0);
        self.density = p.f32("density", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.rng = 0x2545_F491;
            self.dims = (rows, cols);
            self.last_t = t;
            self.step_acc = 0.0;
            self.level = vec![0.0; n];
            self.head = vec![0.0; c];
            self.active = vec![false; c];
            self.wait = vec![0.0; c];
            self.burst = vec![0.0; n];
            self.comets.clear();
            self.prev = vec![false; KEY_SCAN_SLOTS];
            if n > 0 && self.mode == 0 {
                self.rain_init(r, c);
            }
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }

        if self.mode == 1 {
            // COMET. Reconcile the parade to the density count, scattering fresh comets along their path
            // so the board is alive immediately (a comet pushed past the far edge simply dies + re-enters).
            let want = self.comet_count();
            let span = ((r as f32).powi(2) + (c as f32).powi(2)).sqrt();
            while self.comets.len() < want {
                let mut b = self.spawn_body(r, c);
                let lead = self.rand() * span;
                b.x += b.vx * lead;
                b.y += b.vy * lead;
                self.comets.push(b);
            }
            if self.comets.len() > want {
                self.comets.truncate(want);
            }
            let steps = self.accrue_steps(t, 24.0);
            for _ in 0..steps {
                self.comet_step(r, c);
            }
            // BREAK on fresh key-downs that hit a live head (the same safe down-edge scan reactive uses).
            let mut hits: Vec<(usize, usize)> = Vec::new();
            scan_key_presses(&mut self.prev, r, c, |ry, cx| hits.push((ry, cx)));
            for (ry, cx) in hits {
                self.break_at(ry as f32, cx as f32, r, c);
            }
            // RENDER: each live comet draws its own gradient streak (tail → white-hot head) into the
            // per-cell field, kept by MAX intensity so overlapping streaks read "lighten"; the break
            // burst overlays on top (its core drives toward the spectrum's hot/white end).
            let mut inten = vec![0.0f32; n];
            let mut ucoord = vec![0.0f32; n];
            let comets = std::mem::take(&mut self.comets);
            for b in &comets {
                if b.respawn <= 0.0 {
                    draw_comet(&mut inten, &mut ucoord, b, r, c);
                }
            }
            self.comets = comets;
            for i in 0..n {
                let v = self.burst[i];
                if v > 0.0 {
                    let bi = v.min(1.0);
                    if bi > inten[i] {
                        inten[i] = bi;
                        ucoord[i] = v.min(1.0); // a hot burst reads as the spectrum's white/hot end
                    }
                }
            }
            let cells = (0..n).map(|i| Cell::new(ucoord[i], inten[i])).collect();
            return Field::Scalar(cells);
        }

        // RAIN (Cascade).
        let steps = self.accrue_steps(t, 20.0);
        for _ in 0..steps {
            self.rain_step(r, c);
        }
        // emit (u, intensity): the tail (v < HEAD_THRESH) sits at u≈0 (the spectrum's tail colour) with
        // intensity = v; the head ramps u → 1 (the spectrum's white head) at full intensity.
        const HEAD_THRESH: f32 = 0.9;
        let cells = self
            .level
            .iter()
            .map(|&v| {
                if v <= 0.0 {
                    Cell::new(0.0, 0.0)
                } else if v >= HEAD_THRESH {
                    let f = ((v - HEAD_THRESH) / (1.0 - HEAD_THRESH)).clamp(0.0, 1.0);
                    Cell::new(f, 1.0)
                } else {
                    Cell::new(0.0, v)
                }
            })
            .collect();
        Field::Scalar(cells)
    }
}

/// Draw one live comet as a gradient STREAK into the `(intensity, u)` field: from the white-hot head
/// (`d = 0`, u → 1) back along the reversed velocity to the tail end (`d = trail`, u = 0, fading out).
/// Per-comet `trail` length and `bright` head intensity give every comet its own look. Sampled in
/// sub-cell steps with a small anti-aliased footprint so the streak stays continuous at any slant; each
/// cell keeps the MAX-intensity contribution (and its u) — the "lighten" of overlapping streaks.
fn draw_comet(inten: &mut [f32], ucoord: &mut [f32], b: &CometBody, r: usize, c: usize) {
    const SAMPLE_STEP: f32 = 0.4;
    const REACH: f32 = 1.1;
    let trail = b.trail.max(2.0);
    let mut d = 0.0;
    while d <= trail {
        let f = 1.0 - d / trail; // 1 at the head → 0 at the tail end
        let whiteness = ((f - 0.7) / 0.3).clamp(0.0, 1.0); // only the front of the streak whitens
        let v = b.bright * f;
        if v > 0.0 {
            let (px, py) = (b.x - b.vx * d, b.y - b.vy * d);
            let bx = px.floor() as isize;
            let by = py.floor() as isize;
            for dy in -1..=2 {
                for dx in -1..=2 {
                    let gx = bx + dx;
                    let gy = by + dy;
                    if gx < 0 || gy < 0 || gx >= c as isize || gy >= r as isize {
                        continue;
                    }
                    let dist = ((px - gx as f32).powi(2) + (py - gy as f32).powi(2)).sqrt();
                    let w = (1.0 - dist / REACH).clamp(0.0, 1.0);
                    if w <= 0.0 {
                        continue;
                    }
                    let sample = v * w;
                    let i = gy as usize * c + gx as usize;
                    if sample > inten[i] {
                        inten[i] = sample;
                        ucoord[i] = whiteness;
                    }
                }
            }
        }
        d += SAMPLE_STEP;
    }
}

// ───────────────────────────────── Sparkle (the Starlight shape) ───────────────────────────────

/// Sparkle: cells randomly ignite to full brightness and fade out, a slow shimmer of stars. The colour
/// is the spectrum (default a soft white); each cell emits `u = 0` (one solid colour) and `intensity =
/// its twinkle level`. `density` populates the sky, `speed` how briskly stars cycle, `fade` the twinkle
/// length. Deterministic PRNG (no `rand`). The hard-won Starlight, re-expressed.
#[derive(Default)]
pub struct Sparkle {
    level: Vec<f32>,
    dims: (u8, u8),
    rng: u32,
    speed: f32,
    density: f32,
    fade: f32,
    acc: f32,
}

impl Pattern for Sparkle {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
        self.density = p.f32("density", 1.0);
        self.fade = p.f32("fade", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.level = vec![0.0; n];
            self.rng = 0x1357_2468;
            self.dims = (rows, cols);
            self.acc = 0.0;
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        // spawn ~ (3% of cells) × speed × density new stars per frame, accumulating the fraction.
        self.acc += n as f32 * 0.03 * self.speed.max(0.1) * self.density.clamp(0.1, 4.0);
        while self.acc >= 1.0 {
            self.acc -= 1.0;
            let i = (xorshift(&mut self.rng) * n as f32) as usize % n;
            self.level[i] = 0.85 + xorshift(&mut self.rng) * 0.15;
        }
        // fade every cell toward dark — FADE is the twinkle length (higher → faster decay → crisper sparks).
        let decay = 0.04 * self.speed.max(0.1) * self.fade.clamp(0.1, 4.0);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        let cells = self.level.iter().map(|&l| Cell::new(0.0, l)).collect();
        Field::Scalar(cells)
    }
}

// ───────────────────────────────── Ignite (the Reactive shape) ─────────────────────────────────

/// Ignite: the board lights where you type, then fades — driven by the LIVE keyboard. Each fresh
/// key-down ignites that key's TRUE cell (the standard Razer keymap, one LED per key); the cell then
/// fades. With `glow` on, the four neighbours catch a softer glow too. Emits `u = 0` (one solid colour
/// from the spectrum) and `intensity = the cell's decaying level`. `fade` is the trail length. Off
/// Windows the key read is a no-op (the board idles dark). The hard-won Reactive keymap, re-expressed.
#[derive(Default)]
pub struct Ignite {
    level: Vec<f32>,
    prev: Vec<bool>,
    dims: (u8, u8),
    fade: f32,
    glow: bool,
}

impl Pattern for Ignite {
    fn configure(&mut self, p: &Params) {
        self.fade = p.f32("fade", 1.0);
        self.glow = p.bool("glow", false);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.level = vec![0.0; n];
            self.prev = vec![false; KEY_SCAN_SLOTS];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        let glow = self.glow;
        let level = &mut self.level;
        scan_key_presses(&mut self.prev, r, c, |ry, cx| {
            let cell = ry * c + cx;
            level[cell] = 1.0;
            if glow {
                for (dy, dx) in [(0isize, 1isize), (0, -1), (1, 0), (-1, 0)] {
                    let ny = ry as isize + dy;
                    let nx = cx as isize + dx;
                    if ny >= 0 && ny < r as isize && nx >= 0 && nx < c as isize {
                        let ni = ny as usize * c + nx as usize;
                        level[ni] = level[ni].max(0.55);
                    }
                }
            }
        });
        // fade — FADE is the trail length (higher → faster decay → a snappier glow).
        let decay = 0.06 * self.fade.clamp(0.1, 4.0);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        let cells = self.level.iter().map(|&l| Cell::new(0.0, l)).collect();
        Field::Scalar(cells)
    }
}

// ─────────────────────────────────── Ring (the Ripple shape) ───────────────────────────────────

/// One live ripple: its origin cell (fractional) and the elapsed time it was born at — so its radius and
/// fade are a pure function of the current `t`.
#[derive(Clone, Copy)]
struct RippleWave {
    or: f32,
    oc: f32,
    t0: f32,
}

/// Ring: pressing a key sends a ring of light radiating OUTWARD across the board from that key's cell.
/// It watches the LIVE keyboard for fresh key-downs (the same safe down-edge scan Ignite uses) and spawns
/// a ripple at each pressed key's TRUE cell; a small fixed POOL overlaps a flurry without unbounded
/// growth. Each cell takes the MAX over the active ripples — brightness peaks at the moving ring
/// (a Gaussian band) and fades with age. Emits `u = 0` (the spectrum's colour) and `intensity = the ring
/// envelope`. `speed` is the outward velocity, `fade` the ring lifetime. The hard-won Ripple, re-expressed.
#[derive(Default)]
pub struct Ring {
    waves: Vec<RippleWave>,
    prev: Vec<bool>,
    dims: (u8, u8),
    speed: f32,
    fade: f32,
}

impl Ring {
    fn spawn(&mut self, or: f32, oc: f32, t: f32) {
        const MAX_WAVES: usize = 8;
        if self.waves.len() >= MAX_WAVES {
            if let Some(idx) = self
                .waves
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.t0.total_cmp(&b.1.t0))
                .map(|(i, _)| i)
            {
                self.waves.remove(idx);
            }
        }
        self.waves.push(RippleWave { or, oc, t0: t });
    }
}

impl Pattern for Ring {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
        self.fade = p.f32("fade", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.waves.clear();
            self.prev = vec![false; KEY_SCAN_SLOTS];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        let mut spawns: Vec<(usize, usize)> = Vec::new();
        scan_key_presses(&mut self.prev, r, c, |ry, cx| spawns.push((ry, cx)));
        for (ry, cx) in spawns {
            self.spawn(ry as f32, cx as f32, t);
        }
        // expansion + fade tunables. `speed` is the ring's outward velocity (cells/sec); `fade` the
        // lifetime (higher → shorter rings). RING_WIDTH is the Gaussian band half-width (thickness).
        const BASE_SPEED: f32 = 7.0;
        const BASE_LIFETIME: f32 = 2.2;
        const RING_WIDTH: f32 = 1.15;
        let speed = self.speed.clamp(0.1, 6.0);
        let life = BASE_LIFETIME / self.fade.clamp(0.25, 4.0);
        self.waves.retain(|w| t >= w.t0 && (t - w.t0) <= life);
        let mut cells = vec![Cell::new(0.0, 0.0); n];
        if self.waves.is_empty() {
            return Field::Scalar(cells);
        }
        for y in 0..r {
            for x in 0..c {
                let mut env = 0.0f32;
                for w in &self.waves {
                    let age = t - w.t0;
                    let radius = age * BASE_SPEED * speed;
                    let dr = y as f32 - w.or;
                    let dc = x as f32 - w.oc;
                    let d = (dr * dr + dc * dc).sqrt();
                    let band_arg = (d - radius) / RING_WIDTH;
                    let band = (-(band_arg * band_arg)).exp();
                    let envelope = (1.0 - age / life).clamp(0.0, 1.0);
                    env = env.max(band * envelope);
                }
                cells[y * c + x] = Cell::new(0.0, env.clamp(0.0, 1.0));
            }
        }
        Field::Scalar(cells)
    }
}

// ───────────────────────────────────── Flow (the Aurora shape) ─────────────────────────────────

/// Flow: a slow, flowing northern-lights field. Several incommensurate sine flows over (x, y, t) drift a
/// per-cell sample coordinate `u` so the spectrum's colours wander organically across the board, while a
/// SEPARATE, slower set of sines undulates the brightness so bands glow and dim — depth in BOTH the
/// colour walk and luminance, never a flat wash. The spectrum supplies the palette (default an
/// aurora gradient of greens/teals/violets). `speed` is the flow rate. Pure time function, self-animating.
/// The hard-won Aurora, re-expressed (its hue swing is now the spectrum's gradient, sampled by the flow).
#[derive(Default)]
pub struct Flow {
    speed: f32,
}

impl Pattern for Flow {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        let mut cells = vec![Cell::new(0.0, 0.0); n];
        if n == 0 {
            return Field::Scalar(cells);
        }
        let spd = self.speed.clamp(0.1, 6.0);
        let t1 = t * 0.10 * spd;
        let t2 = t * 0.067 * spd;
        let t3 = t * 0.041 * spd;
        for y in 0..r {
            let ny = if r > 1 { y as f32 / (r as f32 - 1.0) } else { 0.5 };
            for x in 0..c {
                let nx = if c > 1 { x as f32 / (c as f32 - 1.0) } else { 0.5 };
                // u: three incommensurate sine flows over space+time → an organic walk across the palette.
                let h1 = (nx * 1.5 + ny * 0.6 + t1 * TAU).sin();
                let h2 = (nx * 0.7 - ny * 1.3 + t2 * TAU).sin();
                let h3 = (ny * 2.1 + t3 * TAU).sin();
                let drift = h1 * 0.5 + h2 * 0.3 + h3 * 0.2; // -1..1
                let u = (0.5 + 0.5 * drift).clamp(0.0, 1.0);
                // intensity: a SEPARATE, slower luminance undulation so bands stand out in depth.
                let b1 = (nx * 1.1 - t2 * TAU * 0.8).sin();
                let b2 = (ny * 1.7 + t3 * TAU * 1.3).sin();
                let lum = 0.5 + 0.5 * (b1 * 0.6 + b2 * 0.4);
                let v = (0.30 + 0.70 * lum).clamp(0.0, 1.0);
                cells[y * c + x] = Cell::new(u, v);
            }
        }
        Field::Scalar(cells)
    }
}

// ──────────────────────────────── Thermal (the Typing Heat shape) ──────────────────────────────

/// Thermal: the board is a LIVING, position-aware heat MAP of your typing. Three forces act on a per-cell
/// thermal field, exactly like heat on a plate: fresh key-downs DEPOSIT a radial splat at the pressed
/// key's cell; the field DIFFUSES (heat-conserving 4-neighbour conduction + a gentle upward buoyancy);
/// and it COOLS continuously by a temperature-dependent radiative+Newtonian curve (a hot cell flashes
/// down fast, embers linger). Your typing RATE sets how HOT each deposit lands (speed → intensity, not a
/// flood). Emits `u = temperature` (the spectrum's cold→white ramp coordinate) and `intensity = a gentle
/// breath × per-cell heat-haze shimmer`. The hard-won Typing-Heat physics, re-expressed exactly.
#[derive(Default)]
pub struct Thermal {
    heat: Vec<f32>,
    scratch: Vec<f32>,
    prev: Vec<bool>,
    rate: f32,
    dims: (u8, u8),
    sensitivity: f32,
    fade: f32,
    last_t: f32,
}

impl Pattern for Thermal {
    fn configure(&mut self, p: &Params) {
        self.sensitivity = p.f32("sensitivity", 1.0);
        self.fade = p.f32("fade", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.heat = vec![0.0; n];
            self.scratch = vec![0.0; n];
            self.prev = vec![false; KEY_SCAN_SLOTS];
            self.rate = 0.0;
            self.dims = (rows, cols);
            self.last_t = t;
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let fade = self.fade.clamp(0.1, 4.0);
        let sens = self.sensitivity.clamp(0.25, 3.0);
        // (1) COOL then (2) DIFFUSE the existing field before this frame's fresh presses land on top. Both
        // dt-scaled (fps-independent); a static t freezes.
        cool_field(&mut self.heat, fade, dt);
        diffuse_field(&mut self.heat, &mut self.scratch, r, c, dt);
        // detect fresh key-downs: count ALL of them (the typing RATE) and record each pressed key's cell.
        let mut pending: Vec<(usize, usize)> = Vec::new();
        let presses = scan_key_presses(&mut self.prev, r, c, |ry, cx| pending.push((ry, cx)));
        // update the typing-RATE EMA, then DEPOSIT — the rate sets the deposit PEAK (fast typing →
        // white-hot, slow → dim embers — speed → INTENSITY, not coverage).
        self.rate = step_rate(self.rate, presses, dt, fade);
        let peak = deposit_peak(self.rate, sens);
        for (ry, cx) in pending {
            deposit_heat(&mut self.heat, ry, cx, r, c, peak);
        }
        // emit (u = temperature clamped to the ramp, intensity = breath × heat-haze shimmer). A fresh
        // flare (temp > 1) clamps u to the spectrum's hot/white end.
        let breath = 0.94 + 0.06 * (t * TAU / 6.0).sin();
        let cells = (0..n)
            .map(|i| {
                let temp = self.heat[i].max(0.0);
                let (x, y) = (i % c, i / c);
                let shimmer = heat_shimmer(x, y, t, temp);
                Cell::new(temp.min(1.0), (breath * shimmer).clamp(0.0, 1.0))
            })
            .collect();
        Field::Scalar(cells)
    }
}

/// Deposit heat for one fresh key-down as a soft RADIAL splat — a hot core with a smooth falloff, so a
/// press reads as a glowing blob, not a hard cross. `peak` is the core heat (set by the typing rate ×
/// sensitivity). Accumulates, clamped so a mashed key saturates white-hot instead of running away.
fn deposit_heat(heat: &mut [f32], ry: usize, cx: usize, r: usize, c: usize, peak: f32) {
    const RADIUS: f32 = 1.8;
    const MAX: f32 = 1.6;
    let (ry, cx) = (ry as isize, cx as isize);
    let reach = RADIUS.ceil() as isize;
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d > RADIUS {
                continue;
            }
            let (ny, nx) = (ry + dy, cx + dx);
            if ny < 0 || ny >= r as isize || nx < 0 || nx >= c as isize {
                continue;
            }
            let falloff = 1.0 - d / RADIUS;
            let add = peak * falloff * falloff;
            let i = ny as usize * c + nx as usize;
            heat[i] = (heat[i] + add).min(MAX);
        }
    }
}

/// Cool the whole heat field one frame — a temperature-DEPENDENT, dt-scaled radiative+convective leak
/// (`dT/dt = -(RAD·T³ + LIN·T)·fade`, integrated by freezing the per-cell rate and stepping
/// exponentially: stable, never negative). A hot cell sheds heat FAST (the radiative T³ term), a cool one
/// SLOWLY (the Newtonian baseline) — so a white-hot key flashes down through the ramp in ~½–1s while the
/// embers LINGER for several seconds. `fade` scales the whole rate. fps-independent (dt 0 ⇒ frozen).
fn cool_field(field: &mut [f32], fade: f32, dt: f32) {
    const RAD: f32 = 3.0;
    const LIN: f32 = 0.22;
    let f = fade.clamp(0.1, 4.0);
    let dt = dt.max(0.0);
    if dt == 0.0 {
        return;
    }
    for h in field.iter_mut() {
        let t = *h;
        if t <= 0.0 {
            continue;
        }
        let rate = (RAD * t * t + LIN) * f;
        *h = t * (-rate * dt).exp();
    }
}

/// Diffuse the heat field one frame — a heat-CONSERVING 4-neighbour conduction blur with a gentle UPWARD
/// buoyancy (heat rises). Redistributes heat as pairwise EDGE FLUXES (what leaves a cell enters its
/// neighbour) so the field SUM is preserved exactly — the only thing that removes heat is [`cool_field`].
/// Accumulated as per-cell deltas into the reused `scratch` (no per-frame alloc). dt-scaled (dt 0 ⇒ frozen).
fn diffuse_field(field: &mut [f32], scratch: &mut [f32], r: usize, c: usize, dt: f32) {
    const DIFFUSE_RATE: f32 = 6.0;
    const RISE_RATE: f32 = 1.0;
    let dt = dt.max(0.0);
    if dt == 0.0 || scratch.len() != field.len() || field.is_empty() {
        return;
    }
    let k = (DIFFUSE_RATE * dt).min(0.45) / 4.0;
    let rise = (RISE_RATE * dt).min(0.08);
    for s in scratch.iter_mut() {
        *s = 0.0;
    }
    for y in 0..r {
        for x in 0..c {
            let i = y * c + x;
            let here = field[i];
            if x + 1 < c {
                let j = i + 1;
                let flux = k * (here - field[j]);
                scratch[i] -= flux;
                scratch[j] += flux;
            }
            if y + 1 < r {
                let below = i + c;
                let flux = k * (here - field[below]);
                scratch[i] -= flux;
                scratch[below] += flux;
                let buoy = rise * field[below];
                scratch[below] -= buoy;
                scratch[i] += buoy;
            }
        }
    }
    for (h, d) in field.iter_mut().zip(scratch.iter()) {
        *h += *d;
    }
}

/// The pure typing-RATE integrator — a leaky integrator of fresh key-downs (deterministic, testable).
/// Each press adds a ballistic impulse; between frames it leaks toward 0 with an exponential release
/// scaled by `fade`, dt-scaled. Returns the new rate 0..1 (an EMA of keys/sec); it only sets how hot the
/// next deposits land (see [`deposit_peak`]) — it never floods the board directly.
fn step_rate(rate: f32, presses: u32, dt: f32, fade: f32) -> f32 {
    const GAIN: f32 = 0.09;
    const RELEASE_BASE: f32 = 0.5;
    let fade = fade.clamp(0.1, 4.0);
    let release = (-RELEASE_BASE * fade * dt.max(0.0)).exp();
    let cooled = rate.clamp(0.0, 1.0) * release;
    (cooled + presses as f32 * GAIN).clamp(0.0, 1.0)
}

/// How HOT a fresh deposit lands — the SPEED→INTENSITY mapping. The typing `rate` lifts the peak from a
/// dim ember (slow/lone press) toward white-hot (a fast flurry); `sensitivity` scales the whole thing.
fn deposit_peak(rate: f32, sensitivity: f32) -> f32 {
    const EMBER: f32 = 0.32;
    const SPAN: f32 = 0.95;
    let r = rate.clamp(0.0, 1.0);
    (EMBER + SPAN * r) * sensitivity.clamp(0.25, 3.0)
}

/// Per-cell heat-haze SHIMMER in 0..1 — multiplies a cell's brightness so a hot board breathes. Two
/// incommensurate sines (phase-seeded by position) make an organic wobble; the DEPTH scales with the
/// cell's temperature (cold cells steady, hot cells waver like rising heat). ≤ 1.0 (it only ever dims).
fn heat_shimmer(x: usize, y: usize, t: f32, temp: f32) -> f32 {
    let (xf, yf) = (x as f32, y as f32);
    let a = (t * 6.5 + xf * 1.7 + yf * 0.9).sin();
    let b = (t * 9.3 + xf * 0.6 - yf * 1.3 + 2.0).sin();
    let mix = 0.5 + 0.5 * (0.6 * a + 0.4 * b);
    let depth = 0.03 + 0.15 * temp.clamp(0.0, 1.0);
    (1.0 - depth + depth * mix).clamp(0.0, 1.0)
}

// ──────────────────────── Meter (the Audio Meter + Pulse shapes) ───────────────────────────────

/// Meter: the board is a live METER, its bars coloured by the spectrum. The `source` chooses the signal:
/// `speakers`/`mic` drive a bottom-up VU bar (per-column shimmer; `u` rises up the bar so the spectrum
/// reads low → high); `cpu`/`ram` drive a horizontal load bar (`u = load`, so the spectrum's calm→urgent
/// ramp colours the whole bar by how hard the machine is working); `load` is the combined Pulse view
/// (CPU on top, RAM on the bottom). The whole board breathes, faster under CPU load. Every signal comes
/// from a SHARED background provider (audio_level / sys_stats), decoupled from the frame rate. `speed`
/// scales the audio shimmer + the breath rate. The hard-won Audio-Meter + Pulse, re-expressed.
#[derive(Default)]
pub struct Meter {
    source: u8,
    speed: f32,
}

impl Pattern for Meter {
    fn configure(&mut self, p: &Params) {
        self.source = p.u8("source", 0);
        self.speed = p.f32("speed", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        let spd = self.speed.clamp(0.1, 6.0);
        match self.source {
            0 | 1 => {
                let src = if self.source == 1 { "mic" } else { "speakers" };
                crate::audio_level::ensure(src);
                let level = crate::audio_level::level().clamp(0.0, 1.0);
                Field::Scalar(render_audio_meter(level, t, spd, r, c))
            }
            _ => {
                crate::sys_stats::ensure();
                let cpu = crate::sys_stats::cpu();
                let ram = crate::sys_stats::ram();
                Field::Scalar(render_load_meter(self.source, cpu, ram, t, spd, r, c))
            }
        }
    }
}

/// The pure audio-VU renderer — a bottom-up bar whose height is the smoothed `level` with a per-column
/// shimmer. Each lit cell emits `u = its height fraction` (0 at the bottom → 1 at the crest, so the
/// spectrum reads low → high) and full intensity; unlit cells are dark. Deterministic + testable.
fn render_audio_meter(level: f32, t: f32, speed: f32, r: usize, c: usize) -> Vec<Cell> {
    let mut cells = vec![Cell::new(0.0, 0.0); r * c];
    for x in 0..c {
        let phase = x as f32 * 0.7;
        let shimmer = 0.7 + 0.3 * (t * 5.0 * speed + phase).sin();
        let bar = (level * shimmer).clamp(0.0, 1.0) * r as f32; // lit rows in this column
        for y in 0..r {
            let from_bottom = (r - 1 - y) as f32; // 0 at the bottom row
            if from_bottom < bar {
                let frac = if r > 1 { from_bottom / (r as f32 - 1.0) } else { 0.0 };
                cells[y * c + x] = Cell::new(frac.clamp(0.0, 1.0), 1.0);
            }
        }
    }
    cells
}

/// The pure load-meter renderer — horizontal bars filling left→right in proportion to load, the WHOLE
/// board breathing (the rate rising with CPU). `source` selects `cpu`/`ram` (one full-board bar) or
/// `load` (CPU on the top half, RAM on the bottom). Each lit cell emits `u = the zone's load` (so the
/// spectrum's calm→urgent ramp colours the bar by load) and `intensity = the breath` (the fractional
/// leading edge dims smoothly). Deterministic + testable.
fn render_load_meter(source: u8, cpu: f32, ram: f32, t: f32, speed: f32, r: usize, c: usize) -> Vec<Cell> {
    let mut cells = vec![Cell::new(0.0, 0.0); r * c];
    let cpu = cpu.clamp(0.0, 1.0);
    let ram = ram.clamp(0.0, 1.0);
    let rate = (0.4 + 2.0 * cpu) * speed;
    let breath = (0.85 + 0.15 * (t * TAU * rate).sin()).clamp(0.0, 1.0);
    let mut paint = |y0: usize, y1: usize, load: f32| {
        let filled = load * c as f32;
        let full = filled.floor() as usize;
        let frac = filled - filled.floor();
        for y in y0..y1 {
            for x in 0..c {
                if x < full {
                    cells[y * c + x] = Cell::new(load, breath);
                } else if x == full && frac > 0.0 {
                    cells[y * c + x] = Cell::new(load, breath * frac);
                }
            }
        }
    };
    if source == 4 {
        let cpu_rows = (r + 1) / 2;
        paint(0, cpu_rows, cpu); // CPU — top
        paint(cpu_rows, r, ram); // RAM — bottom
    } else if source == 3 {
        paint(0, r, ram); // RAM — whole board
    } else {
        paint(0, r, cpu); // CPU — whole board
    }
    cells
}

// ───────────────────────────────── Screen (the Ambient exception) ──────────────────────────────

/// Screen: the board becomes an AMBILIGHT — it samples the SCREEN and paints each key the colour of the
/// matching screen region (left → left, top → top). Inherently full-colour, so it's the [`Field::Color`]
/// EXCEPTION: it emits per-cell `Rgb` DIRECTLY, bypassing the 1-D spectrum. Reads the SHARED
/// `screen_ambient` provider (downscaled zone grid), decoupled from the frame rate. `speed` is the ease
/// rate (how fast the board chases the screen); `saturation` makes the colours pop. Off-Windows / before
/// the first capture the grid is black and the board idles dark. The hard-won Ambient, re-expressed.
#[derive(Default)]
pub struct Screen {
    prev: Vec<Rgb>,
    dims: (u8, u8),
    speed: f32,
    saturation: f32,
}

impl Pattern for Screen {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
        self.saturation = p.f32("saturation", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let n = rows as usize * cols as usize;
        if self.dims != (rows, cols) {
            self.prev = vec![Rgb::BLACK; n];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Field::Color(Vec::new());
        }
        crate::screen_ambient::ensure();
        let (gc, gr, grid) = crate::screen_ambient::grid();
        let ease = (0.25 * self.speed).clamp(0.04, 1.0);
        let boost = (self.saturation - 1.0).max(0.0);
        self.prev = render_ambient(&grid, gc, gr, &self.prev, rows, cols, ease, boost);
        Field::Color(self.prev.clone())
    }
}

/// The pure Ambient renderer (no capture I/O) — maps each board cell to its matching SCREEN ZONE and
/// eases the previous frame toward it (gentle temporal smoothing). `ease` is the per-frame lerp factor,
/// `boost` the saturation amount. Deterministic + testable; an all-black grid → a dark board (honest).
fn render_ambient(
    grid: &[Rgb],
    gcols: usize,
    grows: usize,
    prev: &[Rgb],
    rows: u8,
    cols: u8,
    ease: f32,
    boost: f32,
) -> Vec<Rgb> {
    let (r, c) = (rows as usize, cols as usize);
    let n = r * c;
    let mut out = vec![Rgb::BLACK; n];
    if n == 0 {
        return out;
    }
    let ease = ease.clamp(0.0, 1.0);
    for y in 0..r {
        let ny = if r > 1 { y as f32 / (r as f32 - 1.0) } else { 0.5 };
        for x in 0..c {
            let nx = if c > 1 { x as f32 / (c as f32 - 1.0) } else { 0.5 };
            let zone = crate::screen_ambient::sample_grid(grid, gcols, grows, nx, ny);
            let target = crate::screen_ambient::boost_saturation(zone, boost);
            let prev_cell = prev.get(y * c + x).copied().unwrap_or(Rgb::BLACK);
            out[y * c + x] = Rgb::lerp(prev_cell, target, ease);
        }
    }
    out
}

// ─────────────────────────── default-spectrum constructors (per pattern) ───────────────────────
//
// A pattern's built-in default spectrum (what a freshly-applied layer starts in before a preset/edit).
// Each is a fn so the registry table stays `const` (Spectrum allocates). Presets ([`presets`]) override
// these with their own spectra.

/// Fire's ember→white incandescent ramp (the colour `Heat`'s `u = heat` samples).
fn fire_spectrum() -> Spectrum {
    Spectrum::from_palette(Palette::new(
        vec![
            Stop::new(Rgb::BLACK, 0.0),
            Stop::new(Rgb::new(180, 0, 0), 0.30),
            Stop::new(Rgb::new(255, 90, 0), 0.60),
            Stop::new(Rgb::new(255, 210, 40), 0.85),
            Stop::new(Rgb::new(255, 255, 220), 1.0),
        ],
        Motion::Hold,
    ))
}

/// Typing-Heat's incandescent (blackbody) ramp: a faint cool-dark glow → ember → red → orange → amber →
/// pure white, so the board reads HOT at every level (and white-hot flares clamp to pure white).
fn thermal_spectrum() -> Spectrum {
    Spectrum::from_palette(Palette::new(
        vec![
            Stop::new(Rgb::new(6, 7, 18), 0.0),
            Stop::new(Rgb::new(72, 6, 2), 0.12),
            Stop::new(Rgb::new(190, 22, 0), 0.32),
            Stop::new(Rgb::new(255, 96, 0), 0.55),
            Stop::new(Rgb::new(255, 200, 46), 0.80),
            Stop::new(Rgb::new(255, 255, 255), 1.0),
        ],
        Motion::Hold,
    ))
}

/// The aurora palette — greens ↔ teals ↔ blues ↔ violets (the `Flow` pattern walks `u` across it).
fn aurora_spectrum() -> Spectrum {
    Spectrum::gradient(vec![
        Rgb::new(0, 200, 120),
        Rgb::new(0, 180, 200),
        Rgb::new(40, 80, 255),
        Rgb::new(150, 60, 220),
    ])
}

/// The streak tail → head gradient (the layer's tail colour rising to a white head).
fn streak_spectrum() -> Spectrum {
    Spectrum::gradient(vec![ACCENT, Rgb::new(255, 255, 255)])
}

/// The default meter gradient — the bar colour low → white at the crest (audio). Pulse overrides with a
/// green → amber → red urgency ramp via its preset.
fn meter_spectrum() -> Spectrum {
    Spectrum::gradient(vec![ACCENT, Rgb::new(255, 255, 255)])
}

// ───────────────────────────────────────── PRESETS (pure data) ─────────────────────────────────
//
// A preset = { label, pattern key, pattern param values, spectrum }. The tile grid IS this list.
// Adding a "look" is ONE entry here — zero code. Every effect from the old menu maps to a (pattern +
// default spectrum) preset; the full collapse of the effect set onto the twelve shapes.

/// One named look: a pattern + its param overrides + a colour [`Spectrum`]. Pure data the (phase-3) tile
/// grid renders and the editor seeds a fresh layer from. `params`/`spectrum` are fns so the table stays
/// `const`-friendly (both allocate).
pub struct Preset {
    /// A stable id (the lighting tile's slug + the import target). Distinct from the pattern key
    /// because several presets share one pattern (Static/Breathing/Cycle are all `uniform`).
    pub slug: &'static str,
    pub label: &'static str,
    pub pattern: &'static str,
    pub params: fn() -> Params,
    pub spectrum: fn() -> Spectrum,
}

impl Preset {
    /// Build the [`LayerDef`] this preset describes (a fresh layer ready to composite/persist).
    pub fn to_layer(&self) -> LayerDef {
        LayerDef {
            pattern: self.pattern.into(),
            params: (self.params)(),
            spectrum: (self.spectrum)(),
            region: Vec::new(),
            blend: Blend::Normal,
            enabled: true,
        }
    }
}

/// The PRESET catalog — the full effect set collapsed onto the twelve shapes, in grid order. The single
/// source for the (phase-3) tile grid. Each is pure data: a pattern key, param overrides, and a spectrum.
pub fn presets() -> Vec<Preset> {
    vec![
        Preset { slug: "static", label: "Static", pattern: "uniform", params: pp_none, spectrum: sp_static },
        Preset { slug: "breathing", label: "Breathing", pattern: "uniform", params: pp_none, spectrum: sp_breathing },
        Preset { slug: "cycle", label: "Cycle", pattern: "uniform", params: pp_none, spectrum: sp_cycle },
        Preset { slug: "wave", label: "Wave", pattern: "axis", params: pp_none, spectrum: spectrum::rainbow },
        Preset { slug: "colorwheel", label: "Color Wheel", pattern: "radial", params: pp_none, spectrum: spectrum::rainbow },
        Preset { slug: "fire", label: "Fire", pattern: "heat", params: pp_none, spectrum: fire_spectrum },
        Preset { slug: "typingheat", label: "Typing Heat", pattern: "thermal", params: pp_none, spectrum: thermal_spectrum },
        Preset { slug: "cascade", label: "Cascade", pattern: "streak", params: pp_rain, spectrum: sp_cascade },
        Preset { slug: "comet", label: "Comet", pattern: "streak", params: pp_comet, spectrum: streak_spectrum },
        Preset { slug: "starlight", label: "Starlight", pattern: "sparkle", params: pp_none, spectrum: sp_starlight },
        Preset { slug: "reactive", label: "Reactive", pattern: "ignite", params: pp_none, spectrum: sp_solid_accent },
        Preset { slug: "ripple", label: "Ripple", pattern: "ring", params: pp_none, spectrum: sp_solid_accent },
        Preset { slug: "aurora", label: "Aurora", pattern: "flow", params: pp_none, spectrum: aurora_spectrum },
        Preset { slug: "audiometer", label: "Audio Meter", pattern: "meter", params: pp_audio, spectrum: meter_spectrum },
        Preset { slug: "pulse", label: "Pulse", pattern: "meter", params: pp_load, spectrum: sp_pulse },
        Preset { slug: "ambient", label: "Ambient", pattern: "screen", params: pp_none, spectrum: sp_solid_accent },
    ]
}

/// Look up a preset by its stable slug (case-insensitive). The single resolver the GUI tile picker
/// and the Synapse importer both use.
pub fn preset_by_slug(slug: &str) -> Option<Preset> {
    presets().into_iter().find(|p| p.slug.eq_ignore_ascii_case(slug))
}

/// Build the [`LayerDef`] a preset slug describes (a fresh layer), or `None` for an unknown slug.
pub fn preset_layer(slug: &str) -> Option<LayerDef> {
    preset_by_slug(slug).map(|p| p.to_layer())
}

/// The slug of the preset a layer EXACTLY matches (pattern + params + spectrum), or `None` once the
/// user has customised it past any preset. The lighting page uses this to highlight the active tile —
/// a freshly-picked preset highlights its tile; editing a stop/knob clears the highlight (honest: the
/// look is now bespoke, not a named preset).
pub fn slug_for_layer(def: &LayerDef) -> Option<&'static str> {
    presets().into_iter().find_map(|p| {
        let l = p.to_layer();
        (l.pattern == def.pattern && l.params == def.params && l.spectrum == def.spectrum)
            .then_some(p.slug)
    })
}

// preset param-builders (non-capturing fns so they're fn-pointers in the table)
fn pp_none() -> Params {
    Params::default()
}
fn pp_rain() -> Params {
    let mut p = Params::default();
    p.set("mode", 0.0);
    // CASCADE falls at 1/4 the design speed by default — full-speed rain reads as choppy/laggy on a
    // legacy ~6fps board (big per-frame jumps), and even on the GUI preview it was too fast to track.
    // 0.25 is the slowest the speed knob allows; bump it from there if you want a downpour.
    p.set("speed", 0.25);
    p
}
fn pp_comet() -> Params {
    let mut p = Params::default();
    p.set("mode", 1.0);
    p
}
fn pp_audio() -> Params {
    let mut p = Params::default();
    p.set("source", 0.0); // speakers
    p
}
fn pp_load() -> Params {
    let mut p = Params::default();
    p.set("source", 4.0); // the combined CPU/RAM Pulse view
    p
}

// preset spectrum-builders
fn sp_static() -> Spectrum {
    Spectrum::solid(ACCENT)
}
fn sp_solid_accent() -> Spectrum {
    Spectrum::solid(ACCENT)
}
fn sp_breathing() -> Spectrum {
    Spectrum::from_palette(Palette::new(
        vec![Stop::new(ACCENT, 0.0)],
        Motion::Breathe { speed: 1.0, depth: 0.7 },
    ))
}
fn sp_cycle() -> Spectrum {
    // a uniform board (u = 0) sampling a single stop whose HUE rotates over time → the whole-board cycle.
    Spectrum::from_palette(Palette::new(
        vec![Stop::new(Rgb::new(255, 0, 0), 0.0)],
        Motion::Cycle { speed: 0.15 },
    ))
}
fn sp_cascade() -> Spectrum {
    // the cinema look: a green tail rising to a white head.
    Spectrum::gradient(vec![Rgb::new(0, 230, 70), Rgb::new(255, 255, 255)])
}
fn sp_starlight() -> Spectrum {
    Spectrum::solid(Rgb::new(0xFF, 0xFF, 0xE0))
}
fn sp_pulse() -> Spectrum {
    // calm → urgent: green (idle) → amber (mid) → red (maxed), sampled by the load.
    Spectrum::gradient(vec![Rgb::new(0, 230, 60), Rgb::new(255, 170, 0), Rgb::new(255, 0, 0)])
}

// ─────────────────────────── THE COMPOSITOR — a stack of Pattern × Spectrum layers ─────────────
//
// Each layer renders its pattern's field, resolves it through the layer's spectrum (× intensity), then
// masks to its region and blends over the layers beneath — the full render pipeline. [`Compositor::render`]
// produces one `rows*cols` frame at time `t`; the device animate / stream / preview paths (lighting.rs)
// drive it directly. Colour lives entirely in each layer's spectrum (there is no base colour).

/// A live layer: a configured stateful pattern bound to its colour spectrum, region mask and blend.
pub struct Layer {
    pub pattern: Box<dyn Pattern>,
    pub spectrum: Spectrum,
    /// Sorted, de-duplicated row-major cell indices the layer paints; empty = the whole board.
    pub region: Vec<u32>,
    pub blend: Blend,
    pub enabled: bool,
}

impl Layer {
    fn covers(&self, i: usize) -> bool {
        self.region.is_empty() || self.region.binary_search(&(i as u32)).is_ok()
    }
}

/// The layer stack. `layers[0]` is the bottom; later layers composite on top.
pub struct Compositor {
    pub layers: Vec<Layer>,
}

impl Compositor {
    /// Build the live stack from serialisable [`LayerDef`]s — resolves + configures each pattern (an
    /// unknown key falls back to a benign `uniform`) and sorts each region mask for the binary-search test.
    pub fn from_defs(defs: &[LayerDef]) -> Compositor {
        let layers = defs
            .iter()
            .map(|d| {
                let pattern = d.make_pattern().unwrap_or_else(|| Box::new(Uniform));
                let mut region = d.region.clone();
                region.sort_unstable();
                region.dedup();
                Layer {
                    pattern,
                    spectrum: d.spectrum.clone(),
                    region,
                    blend: d.blend,
                    enabled: d.enabled,
                }
            })
            .collect();
        Compositor { layers }
    }

    /// Render one composited frame (`rows*cols`, row-major) at elapsed time `t`. Per layer: the pattern's
    /// field → resolved through the spectrum (× intensity) → region mask → blend over the layers below.
    pub fn render(&mut self, rows: u8, cols: u8, t: f32) -> Vec<Rgb> {
        let n = rows as usize * cols as usize;
        let mut out = vec![Rgb::BLACK; n];
        for layer in self.layers.iter_mut() {
            if !layer.enabled {
                continue;
            }
            let field = layer.pattern.field(rows, cols, t);
            if field.len() != n {
                continue; // a misbehaving pattern can't corrupt the stack
            }
            let px = field.render(&layer.spectrum, t);
            for i in 0..n {
                if layer.covers(i) {
                    out[i] = crate::effects::blend_px(out[i], px[i], layer.blend);
                }
            }
        }
        out
    }
}

// ─────────────────────────────────────────── tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrum::{Motion, Palette, Stop};
    use std::collections::HashSet;

    /// `MACRO_HELD` (in `capture`) is process-global, so the macro-key scan test must not run
    /// concurrently with another test that also pokes it. Serialize them here (poison-tolerant), the
    /// same way confirm.rs's tests do for their global sink.
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    // ── the registry-completeness gate — half-registration fails the build forever ──────────

    #[test]
    fn registry_is_complete_and_round_trips() {
        assert!(!registry().is_empty(), "the registry must not be empty");
        let mut seen_keys = HashSet::new();
        for d in registry() {
            // identity: a unique, non-empty key + a non-empty label
            assert!(!d.key.is_empty(), "a pattern has an empty key");
            assert!(!d.label.is_empty(), "{}: empty label", d.key);
            assert!(seen_keys.insert(d.key), "duplicate pattern key {}", d.key);
            assert!(!d.tile.blurb.is_empty(), "{}: empty tile blurb", d.key);

            // make: produces a working pattern that fills the matrix
            let mut p = (d.make)();
            let f = p.field(6, 22, 0.0);
            assert_eq!(f.len(), 6 * 22, "{}: field must fill rows*cols", d.key);

            // params: well-formed (unique non-empty keys; NO colour params — colour is the spectrum)
            let params = (d.params)();
            let mut pk = HashSet::new();
            for prm in &params {
                assert!(!prm.key.is_empty(), "{}: empty param key", d.key);
                assert!(pk.insert(prm.key), "{}: duplicate param {}", d.key, prm.key);
                assert!(
                    !matches!(prm.kind, ParamKind::Color),
                    "{}: patterns carry no colour params (colour lives in the spectrum)",
                    d.key
                );
            }

            // default_spectrum: valid AND round-trips through serde (json + toml)
            let sp = (d.default_spectrum)();
            assert!(!sp.seq.is_empty(), "{}: default spectrum has no frames", d.key);
            let j = serde_json::to_string(&sp).expect("ser default spectrum");
            let back: Spectrum = serde_json::from_str(&j).expect("de default spectrum");
            assert_eq!(sp, back, "{}: default spectrum must round-trip", d.key);

            // the public lookups all derive from this same entry
            assert!(make_pattern(d.key).is_some(), "{}: factory lookup", d.key);
            assert_eq!(pattern_params(d.key).len(), params.len(), "{}: schema lookup", d.key);
            assert!(default_spectrum(d.key).is_some(), "{}: default-spectrum lookup", d.key);
        }
        assert_eq!(pattern_keys().len(), registry().len());
    }

    #[test]
    fn unknown_pattern_key_is_graceful() {
        assert!(make_pattern("nope").is_none());
        assert!(pattern_params("nope").is_empty());
        assert!(default_spectrum("nope").is_none());
        assert!(pattern_def("nope").is_none());
    }

    // ── Field rendering — the per-layer pipeline core ───────────────────────────────────────

    #[test]
    fn scalar_field_renders_through_the_spectrum() {
        // uniform field (u=0, intensity=1) over a solid spectrum -> that solid colour everywhere.
        let mut u = Uniform;
        let f = u.field(2, 3, 0.0);
        let sp = Spectrum::solid(Rgb::new(40, 80, 120));
        let px = f.render(&sp, 0.0);
        assert_eq!(px.len(), 6);
        assert!(px.iter().all(|&c| c == Rgb::new(40, 80, 120)));
    }

    #[test]
    fn intensity_scales_the_spectrum_colour() {
        // a half-intensity cell over a solid white spectrum renders to ~half brightness.
        let cells = vec![Cell::new(0.0, 0.5)];
        let f = Field::Scalar(cells);
        let px = f.render(&Spectrum::solid(Rgb::new(200, 100, 50)), 0.0);
        assert_eq!(px[0], Rgb::new(100, 50, 25));
    }

    #[test]
    fn color_field_passes_through_ignoring_the_spectrum() {
        // the Screen exception: a Color field renders verbatim, spectrum ignored.
        let f = Field::Color(vec![Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)]);
        let px = f.render(&Spectrum::solid(Rgb::new(255, 255, 255)), 0.0);
        assert_eq!(px, vec![Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)]);
    }

    // ── sample patterns animate correctly ───────────────────────────────────────────────────

    #[test]
    fn axis_lays_a_gradient_and_scrolls() {
        let mut a = Axis::default();
        a.configure(&Params::defaults_for("axis"));
        // a spatial gradient: along the → axis, u rises left→right at t=0.
        let f = a.field(1, 4, 0.0);
        if let Field::Scalar(cells) = f {
            assert!(cells[0].u < cells[3].u, "u increases along the axis");
        } else {
            panic!("axis emits a scalar field");
        }
        // it SCROLLS: the same cell reads a different u a moment later.
        let u0 = match a.field(1, 4, 0.0) {
            Field::Scalar(c) => c[0].u,
            _ => unreachable!(),
        };
        let u1 = match a.field(1, 4, 3.0) {
            Field::Scalar(c) => c[0].u,
            _ => unreachable!(),
        };
        assert_ne!(u0, u1, "axis must scroll over time");
    }

    #[test]
    fn axis_direction_changes_the_gradient_orientation() {
        let mut right = Axis::default();
        right.configure(&Params(BTreeMap::from([("direction".into(), 0.0)])));
        let mut left = Axis::default();
        left.configure(&Params(BTreeMap::from([("direction".into(), 1.0)])));
        let fr = match right.field(1, 4, 0.0) {
            Field::Scalar(c) => c,
            _ => unreachable!(),
        };
        let fl = match left.field(1, 4, 0.0) {
            Field::Scalar(c) => c,
            _ => unreachable!(),
        };
        // → rises left→right; ← falls left→right (mirror).
        assert!(fr[0].u < fr[3].u);
        assert!(fl[0].u > fl[3].u);
    }

    #[test]
    fn radial_domes_intensity_from_hub_to_rim() {
        let mut rad = Radial::default();
        rad.configure(&Params::defaults_for("radial"));
        let f = rad.field(5, 5, 0.0);
        if let Field::Scalar(cells) = f {
            let centre = cells[2 * 5 + 2].intensity; // the hub cell
            let corner = cells[0].intensity; // a rim cell
            assert!(centre > corner, "the hub is brighter than the rim ({centre} vs {corner})");
            assert!(centre <= 1.0 && corner >= 0.0);
        } else {
            panic!("radial emits a scalar field");
        }
    }

    // ── Params bag ──────────────────────────────────────────────────────────────────────────

    #[test]
    fn params_defaults_come_from_the_schema() {
        let p = Params::defaults_for("axis");
        // axis declares direction (enum default 0) + speed (range default 1.0)
        assert_eq!(p.f32("speed", -1.0), 1.0);
        assert_eq!(p.u8("direction", 9), 0);
        // an unset knob falls back to the caller's default
        assert_eq!(p.f32("missing", 2.5), 2.5);
        assert!(p.bool("missing", true));
    }

    #[test]
    fn configured_pattern_honours_params() {
        // build the live pattern through a LayerDef so configure() runs with the layer's params.
        let mut def = LayerDef {
            pattern: "axis".into(),
            ..Default::default()
        };
        def.params.set("direction", 1.0); // ← reverses orientation
        let mut p = def.make_pattern().expect("axis builds");
        match p.field(1, 4, 0.0) {
            Field::Scalar(c) => assert!(c[0].u > c[3].u, "the ← direction param took effect"),
            _ => unreachable!(),
        }
    }

    // ── LayerDef serde — the new layer shape ────────────────────────────────────────────────

    #[test]
    fn layer_def_json_round_trips_full() {
        let def = LayerDef {
            pattern: "radial".into(),
            params: Params(BTreeMap::from([("speed".into(), 2.5)])),
            spectrum: Spectrum::from_palette(Palette::new(
                vec![Stop::new(Rgb::new(255, 0, 0), 0.0), Stop::new(Rgb::new(0, 0, 255), 1.0)],
                Motion::Drift { speed: 1.5 },
            )),
            region: vec![3, 1, 2],
            blend: Blend::Add,
            enabled: false,
        };
        let j = serde_json::to_string(&def).unwrap();
        let back: LayerDef = serde_json::from_str(&j).unwrap();
        assert_eq!(def, back);
    }

    #[test]
    fn solid_layer_is_flat_in_toml() {
        // the COMMON case: a uniform/solid layer with no param overrides serialises FLAT (spectrum as a
        // bare hex string, params omitted) so a layer stack round-trips as a TOML array of tables.
        #[derive(Serialize, Deserialize)]
        struct Stack {
            layers: Vec<LayerDef>,
        }
        let stack = Stack {
            layers: vec![
                LayerDef::default(),
                LayerDef {
                    pattern: "axis".into(),
                    spectrum: Spectrum::gradient(vec![Rgb::new(255, 0, 0), Rgb::new(0, 0, 255)]),
                    blend: Blend::Screen,
                    ..Default::default()
                },
            ],
        };
        let t = toml::to_string(&stack).unwrap();
        // the solid layer's spectrum is a flat hex string, and params don't appear at all.
        assert!(t.contains("spectrum = \"4AF2B0\""), "solid spectrum is flat hex:\n{t}");
        assert!(!t.contains("[layers.params]"), "empty params omitted:\n{t}");
        let back: Stack = toml::from_str(&t).unwrap();
        assert_eq!(back.layers.len(), 2);
        assert_eq!(back.layers[0], stack.layers[0]);
        assert_eq!(back.layers[1], stack.layers[1]);
    }

    #[test]
    fn layer_def_tolerates_missing_fields() {
        // a hand-written record with only the pattern key loads with every other field defaulted.
        let d: LayerDef = toml::from_str("pattern = \"uniform\"").unwrap();
        let def = LayerDef::default();
        assert_eq!(d.pattern, "uniform");
        assert_eq!(d.spectrum, def.spectrum);
        assert_eq!(d.blend, def.blend);
        assert_eq!(d.enabled, def.enabled);
        assert!(d.params.is_empty());
    }

    // ── the full registry — all twelve shapes present ───────────────────────────────────────────

    #[test]
    fn registry_has_the_full_twelve_shapes() {
        let keys = pattern_keys();
        for k in [
            "uniform", "axis", "radial", "heat", "streak", "sparkle", "ignite", "ring", "flow",
            "thermal", "meter", "screen",
        ] {
            assert!(keys.contains(&k), "registry is missing the '{k}' pattern");
        }
        assert_eq!(keys.len(), 12, "exactly the twelve shapes are registered");
    }

    // ── presets are pure, valid data (the tile grid) ────────────────────────────────────────────

    #[test]
    fn every_preset_is_valid_and_round_trips() {
        let ps = presets();
        assert!(!ps.is_empty(), "the preset catalog must not be empty");
        let mut labels = HashSet::new();
        for p in &ps {
            assert!(!p.label.is_empty(), "a preset has an empty label");
            assert!(labels.insert(p.label), "duplicate preset label {}", p.label);
            // the preset's pattern is a real registered shape
            let def = pattern_def(p.pattern)
                .unwrap_or_else(|| panic!("{}: unknown pattern '{}'", p.label, p.pattern));
            // every param key the preset sets is a real knob in that pattern's schema
            let schema_keys: HashSet<&str> = (def.params)().iter().map(|q| q.key).collect();
            for key in (p.params)().0.keys() {
                assert!(
                    schema_keys.contains(key.as_str()),
                    "{}: param '{key}' is not in {}'s schema",
                    p.label,
                    p.pattern
                );
            }
            // the spectrum round-trips through serde
            let sp = (p.spectrum)();
            let j = serde_json::to_string(&sp).expect("ser preset spectrum");
            let back: Spectrum = serde_json::from_str(&j).expect("de preset spectrum");
            assert_eq!(sp, back, "{}: spectrum must round-trip", p.label);
            // and it builds a working layer that fills the matrix
            let mut layer = p.to_layer().make_pattern().expect("preset builds a pattern");
            assert_eq!(layer.field(6, 22, 0.0).len(), 6 * 22, "{}: fills rows*cols", p.label);
        }
    }

    #[test]
    fn solid_presets_are_recolourable_via_is_solid() {
        // The CLI `lighting run <look> --color HEX` recolours a look ONLY when its spectrum
        // `is_solid()` (one Hold stop) — repainting that lone stop — and leaves a palette/motion look
        // alone (it owns its colour). This pins that contract so the `--color` path can't silently rot.
        use crate::spectrum::Spectrum;
        let solid = ["static", "reactive", "ripple", "ambient", "starlight"];
        let owns_palette = ["wave", "colorwheel", "fire", "aurora", "cascade", "breathing", "cycle"];
        for slug in solid {
            let layer = preset_layer(slug).unwrap_or_else(|| panic!("preset '{slug}' exists"));
            assert!(layer.spectrum.is_solid(), "'{slug}' must be a solid (recolourable by --color)");
            // the override the CLI applies: replace the lone stop with the chosen colour.
            let repainted = Spectrum::solid(Rgb::new(0x12, 0x34, 0x56));
            assert_eq!(repainted.seq[0].palette.stops.len(), 1, "override stays a single stop");
            assert_eq!(repainted.at(0.0, 0.5), Rgb::new(0x12, 0x34, 0x56));
        }
        for slug in owns_palette {
            let layer = preset_layer(slug).unwrap_or_else(|| panic!("preset '{slug}' exists"));
            assert!(!layer.spectrum.is_solid(), "'{slug}' owns its palette — --color must skip it");
        }
    }

    // ── Heat (Fire) — the preserved heat sim ────────────────────────────────────────────────────

    /// Total heat (the `u` coordinate) in the TOP half — how far the flame climbs.
    fn heat_top(cells: &[Cell], rows: usize, cols: usize) -> f32 {
        cells[0..(rows / 2) * cols].iter().map(|c| c.u).sum()
    }

    #[test]
    fn heat_density_changes_flame_height() {
        let run = |dens: f32| {
            let mut h = Heat::default();
            let mut p = Params::default();
            p.set("density", dens);
            h.configure(&p);
            let mut f = Field::Scalar(Vec::new());
            for _ in 0..40 {
                f = h.field(8, 10, 0.0);
            }
            match f {
                Field::Scalar(c) => heat_top(&c, 8, 10),
                _ => unreachable!(),
            }
        };
        assert!(run(2.5) > run(0.4), "a denser fire climbs higher than a sparse one");
    }

    #[test]
    fn heat_speed_changes_propagation_over_time() {
        let top_after = |spd: f32, frames: usize| -> f32 {
            let mut h = Heat::default();
            let mut p = Params::default();
            p.set("speed", spd);
            h.configure(&p);
            let mut f = Field::Scalar(Vec::new());
            for i in 0..frames {
                f = h.field(8, 10, i as f32 * 0.05); // ~20fps
            }
            match f {
                Field::Scalar(c) => heat_top(&c, 8, 10),
                _ => unreachable!(),
            }
        };
        assert!(
            top_after(3.0, 5) > top_after(0.4, 5),
            "a faster fire climbs higher in the same wall-time window"
        );
    }

    // ── Streak (Cascade rain + Comet) — fills, animates, lifecycle, break ────────────────────────

    fn scalar(f: Field) -> Vec<Cell> {
        match f {
            Field::Scalar(c) => c,
            _ => panic!("expected a scalar field"),
        }
    }

    #[test]
    fn streak_rain_fills_and_animates() {
        let mut s = Streak::default();
        s.configure(&Params::defaults_for("streak")); // mode default 0 = rain
        let a = scalar(s.field(6, 22, 0.0));
        assert_eq!(a.len(), 6 * 22);
        assert!(a.iter().any(|c| c.intensity > 0.0), "rain lights some cells");
        let b = scalar(s.field(6, 22, 1.0));
        assert!(a != b, "the rain animates over time");
        assert!(a.iter().all(|c| (0.0..=1.0).contains(&c.u) && (0.0..=1.0).contains(&c.intensity)));
    }

    #[test]
    fn streak_comet_breaks_and_respawns() {
        let mut s = Streak::default();
        let mut p = Params::default();
        p.set("mode", 1.0); // comet
        s.configure(&p);
        let _ = s.field(6, 22, 0.0); // populate the parade (1 comet by default)
        assert!(!s.comets.is_empty(), "the comet parade is populated");
        // place a live comet at a known head, then BREAK it with a press on that cell.
        s.comets[0] = CometBody {
            x: 5.0,
            y: 3.0,
            vx: 1.0,
            vy: 0.0,
            speed_mul: 1.0,
            trail: 5.0,
            bright: 1.0,
            respawn: 0.0,
        };
        assert!(s.break_at(3.0, 5.0, 6, 22), "a press on the head breaks the comet");
        assert!(s.burst[3 * 22 + 5] > 0.0, "the break paints a burst at the impact");
        // a comet that runs fully off the board DIES (queues a respawn) — no eternal wrap.
        s.comets[0] = CometBody {
            x: 100.0,
            y: 3.0,
            vx: 1.0,
            vy: 0.0,
            speed_mul: 1.0,
            trail: 3.0,
            bright: 1.0,
            respawn: 0.0,
        };
        assert!(s.comet_fully_off(0, 6, 22));
        s.comet_step(6, 22);
        assert!(s.comets[0].respawn > 0.0, "a comet off the board dies and queues a respawn");
    }

    #[test]
    fn streak_comet_count_scales_with_density() {
        let count = |d: f32| {
            let mut s = Streak::default();
            let mut p = Params::default();
            p.set("mode", 1.0);
            p.set("density", d);
            s.configure(&p);
            let _ = s.field(6, 22, 0.0);
            s.comets.len()
        };
        assert_eq!(count(1.0), 1, "density 1.0 → exactly one calm streak");
        assert!(count(3.0) > 1, "turning density up grows the parade");
    }

    // ── Sparkle (Starlight) ─────────────────────────────────────────────────────────────────────

    #[test]
    fn sparkle_twinkles_over_time() {
        let mut s = Sparkle::default();
        s.configure(&Params::defaults_for("sparkle"));
        let mut any = false;
        for i in 0..30 {
            let c = scalar(s.field(6, 22, i as f32 * 0.05));
            assert_eq!(c.len(), 6 * 22);
            if c.iter().any(|x| x.intensity > 0.0) {
                any = true;
            }
            assert!(c.iter().all(|x| x.u == 0.0), "sparkle samples one solid colour (u=0)");
        }
        assert!(any, "stars must ignite over time");
    }

    // ── live-input patterns idle well-formed without keys (robust to whatever keys are held) ──────

    #[test]
    fn live_input_patterns_are_well_formed() {
        for key in ["ignite", "ring", "thermal", "meter", "screen"] {
            let mut p = make_pattern(key).unwrap();
            let f = p.field(6, 22, 0.0);
            assert_eq!(f.len(), 6 * 22, "{key} fills rows*cols");
            if let Field::Scalar(cells) = f {
                assert!(
                    cells.iter().all(|c| (0.0..=1.0).contains(&c.u) && (0.0..=1.0).contains(&c.intensity)),
                    "{key} emits u/intensity in range"
                );
            }
        }
    }

    #[test]
    fn macro_key_press_lights_its_cell_with_held_edge_detection() {
        // The macro column (M1..M6) goes dark on the live-input effects because the keys aren't VKs —
        // they ride Razer's `0x04` report into `capture::macro_key_down`. This verifies the bridge:
        // a fresh M1 down-edge lights M1's cell exactly once, a still-held M1 does NOT re-fire, and a
        // release→re-press fires again — the same shared-held-state down-edge model the VK path uses.
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());

        let m1 = crate::lighting::razer_key_cell("M1").expect("M1 has a standard cell");
        assert_eq!(m1, (1, 0), "M1 lives at (row 1, col 0)");
        let (m1r, m1c) = (m1.0 as usize, m1.1 as usize);
        let (rows, cols) = (6usize, 22usize);
        let mut prev = vec![false; KEY_SCAN_SLOTS];

        // Count ONLY fresh presses landing on M1's cell. No VK resolves to (1,0), so live keyboard
        // state can't pollute this count — only the macro bridge can light it. (Suppression is a
        // thread-local that defaults off, so reads on this fresh test thread are NOT suppressed.)
        let mut count_m1 = |prev: &mut Vec<bool>| {
            let mut hits = 0u32;
            scan_key_presses(prev, rows, cols, |ry, cx| {
                if (ry, cx) == (m1r, m1c) {
                    hits += 1;
                }
            });
            hits
        };

        // Settle the baseline with M1 released, so the next press is a true down-edge.
        crate::capture::set_macro_held(0);
        let _ = count_m1(&mut prev);

        // Press M1 → exactly one fire at its cell.
        crate::capture::set_macro_held(1 << 0);
        assert_eq!(count_m1(&mut prev), 1, "fresh M1 press lights its cell once");

        // Still held → no re-fire (held, not re-pressed).
        assert_eq!(count_m1(&mut prev), 0, "a held M1 does not re-fire");

        // Release (observe the up-edge), then press again → fires again.
        crate::capture::set_macro_held(0);
        let _ = count_m1(&mut prev);
        crate::capture::set_macro_held(1 << 0);
        assert_eq!(count_m1(&mut prev), 1, "release then re-press fires again");

        // Leave the process-global mask clean for any other test.
        crate::capture::set_macro_held(0);
    }

    #[test]
    fn macro_code_index_maps_the_report_codes() {
        // The Razer Driver-Mode `0x04` report numbers the macro keys sequentially from 0x20.
        assert_eq!(crate::lighting::macro_code_index(0x20), Some(0)); // M1
        assert_eq!(crate::lighting::macro_code_index(0x25), Some(5)); // M6
        assert_eq!(crate::lighting::macro_code_index(0x01), None); // FN is not a macro key
        assert_eq!(crate::lighting::macro_code_index(0x00), None); // released
        assert_eq!(crate::lighting::macro_code_index(0x26), None); // past M6
        // Every macro name resolves to a cell (so a press is never silently swallowed by a bad name).
        for name in crate::lighting::MACRO_KEY_NAMES {
            assert!(crate::lighting::razer_key_cell(name).is_some(), "{name} has a cell");
        }
    }

    // ── Flow (Aurora) ───────────────────────────────────────────────────────────────────────────

    #[test]
    fn flow_animates_and_stays_in_range() {
        let mut fl = Flow::default();
        fl.configure(&Params::defaults_for("flow"));
        let a = scalar(fl.field(6, 22, 0.0));
        let b = scalar(fl.field(6, 22, 3.0));
        assert!(a != b, "the flow drifts over time");
        assert!(a.iter().all(|c| (0.0..=1.0).contains(&c.u) && (0.0..=1.0).contains(&c.intensity)));
    }

    // ── Thermal (Typing Heat) — the preserved radiative cooling + conserving diffusion + rate ─────

    #[test]
    fn thermal_cooling_is_temperature_dependent() {
        // a HOT cell sheds a bigger FRACTION of its heat than a COOL one over the same dt.
        let mut field = vec![1.0_f32, 0.1];
        cool_field(&mut field, 1.0, 0.1);
        let hot_frac = (1.0 - field[0]) / 1.0;
        let cool_frac = (0.1 - field[1]) / 0.1;
        assert!(hot_frac > cool_frac, "the hot cell flashes down faster than the ember lingers");
        assert!(field[0] >= 0.0 && field[1] >= 0.0, "cooling never goes negative");
        // a paused clock (dt 0) freezes the field.
        let mut frozen = vec![0.5_f32];
        cool_field(&mut frozen, 1.0, 0.0);
        assert_eq!(frozen[0], 0.5);
    }

    #[test]
    fn thermal_diffusion_conserves_total() {
        // one hot cell in a 3×3 plate spreads to its neighbours WITHOUT changing the total (only cooling
        // removes heat; the blur must not eat or create it).
        let mut field = vec![0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0];
        let mut scratch = vec![0.0; 9];
        let before: f32 = field.iter().sum();
        diffuse_field(&mut field, &mut scratch, 3, 3, 0.1);
        let after: f32 = field.iter().sum();
        assert!((before - after).abs() < 1e-4, "diffusion conserves total heat ({before} vs {after})");
        assert!(field[4] < 1.0, "the hot centre gave heat to its neighbours");
        assert!(field.iter().enumerate().any(|(i, &v)| i != 4 && v > 0.0), "neighbours warmed");
    }

    #[test]
    fn thermal_rate_and_peak_track_typing() {
        // presses lift the typing rate; a higher rate (and higher sensitivity) lands a hotter deposit.
        let idle = step_rate(0.0, 0, 0.1, 1.0);
        let typed = step_rate(0.0, 3, 0.1, 1.0);
        assert!(typed > idle, "fresh presses raise the typing rate");
        assert!(deposit_peak(1.0, 1.0) > deposit_peak(0.0, 1.0), "a faster rate lands hotter");
        assert!(deposit_peak(0.5, 2.0) > deposit_peak(0.5, 1.0), "sensitivity scales the deposit");
    }

    // ── Meter (Audio Meter + Pulse) — the pure renderers ────────────────────────────────────────

    #[test]
    fn meter_audio_fills_from_the_bottom() {
        // a loud level lights the bottom rows; u rises up the bar (low → high). Silence → dark.
        let loud = render_audio_meter(1.0, 0.0, 1.0, 6, 22);
        assert!(loud[5 * 22].intensity > 0.0, "the bottom row lights when loud");
        assert_eq!(loud[5 * 22].u, 0.0, "the bottom of the bar samples the spectrum's low end");
        assert_eq!(loud[0].intensity, 0.0, "the very top stays dark at this level");
        let silent = render_audio_meter(0.0, 0.0, 1.0, 6, 22);
        assert!(silent.iter().all(|c| c.intensity == 0.0), "silence → a dark meter");
    }

    #[test]
    fn meter_load_colours_the_bar_by_load() {
        // a CPU bar at 50% fills the left half; every lit cell samples the spectrum at u = the load (so the
        // calm→urgent ramp colours the whole bar by how hard the machine is working).
        let cells = render_load_meter(2, 0.5, 0.0, 0.0, 1.0, 6, 22);
        assert!(cells[0].intensity > 0.0, "the bar's left is lit");
        assert!((cells[0].u - 0.5).abs() < 1e-6, "u carries the load level");
        assert_eq!(cells[21].intensity, 0.0, "the unfilled right stays dark");
        // the combined "load" view splits CPU (top) over RAM (bottom).
        let split = render_load_meter(4, 1.0, 0.0, 0.0, 1.0, 6, 22);
        assert!(split[0].intensity > 0.0, "a maxed CPU lights the top");
        assert_eq!(split[5 * 22].intensity, 0.0, "an idle RAM leaves the bottom dark");
    }

    // ── Screen (Ambient) — the pure renderer + the Color exception ───────────────────────────────

    #[test]
    fn screen_mirrors_the_grid_directly() {
        // a 1×1 red screen, eased fully in one step → a red board; the Screen pattern emits a Color field.
        let out = render_ambient(&[Rgb::new(255, 0, 0)], 1, 1, &[Rgb::BLACK], 1, 1, 1.0, 0.0);
        assert_eq!(out[0], Rgb::new(255, 0, 0));
        // an all-black grid → a dark board (honest, never faked).
        let dark = render_ambient(&[Rgb::BLACK], 1, 1, &[Rgb::new(9, 9, 9)], 1, 1, 1.0, 0.0);
        assert_eq!(dark[0], Rgb::BLACK);
    }

    // ── the COMPOSITOR — pattern × spectrum, region, blend ──────────────────────────────────────

    #[test]
    fn compositor_renders_pattern_through_spectrum() {
        // a uniform pattern over a solid spectrum → that one colour everywhere.
        let defs = vec![LayerDef {
            pattern: "uniform".into(),
            spectrum: Spectrum::solid(Rgb::new(10, 20, 30)),
            ..Default::default()
        }];
        let mut comp = Compositor::from_defs(&defs);
        let f = comp.render(2, 3, 0.0);
        assert_eq!(f.len(), 6);
        assert!(f.iter().all(|&c| c == Rgb::new(10, 20, 30)));
        // an axis pattern over a rainbow → a spatial gradient (neighbouring cells differ).
        let mut wave = Compositor::from_defs(&[LayerDef {
            pattern: "axis".into(),
            spectrum: spectrum::rainbow(),
            ..Default::default()
        }]);
        let wf = wave.render(1, 8, 0.0);
        assert!(wf.windows(2).any(|s| s[0] != s[1]), "the wave is a spatial gradient");
    }

    #[test]
    fn compositor_blends_layers_within_regions() {
        let defs = vec![
            LayerDef {
                pattern: "uniform".into(),
                spectrum: Spectrum::solid(Rgb::new(255, 0, 0)),
                ..Default::default()
            },
            LayerDef {
                pattern: "uniform".into(),
                spectrum: Spectrum::solid(Rgb::new(0, 255, 0)),
                region: vec![0, 1],
                ..Default::default()
            },
        ];
        let mut comp = Compositor::from_defs(&defs);
        let f = comp.render(2, 3, 0.0);
        assert_eq!(f[0], Rgb::new(0, 255, 0), "cell 0 is in the top layer's region");
        assert_eq!(f[1], Rgb::new(0, 255, 0), "cell 1 too");
        assert_eq!(f[2], Rgb::new(255, 0, 0), "cell 2 falls through to the bottom layer");
    }

    #[test]
    fn compositor_add_blend_brightens() {
        let defs = vec![
            LayerDef {
                pattern: "uniform".into(),
                spectrum: Spectrum::solid(Rgb::new(100, 0, 0)),
                ..Default::default()
            },
            LayerDef {
                pattern: "uniform".into(),
                spectrum: Spectrum::solid(Rgb::new(0, 0, 100)),
                blend: Blend::Add,
                ..Default::default()
            },
        ];
        let mut comp = Compositor::from_defs(&defs);
        let f = comp.render(1, 1, 0.0);
        assert_eq!(f[0], Rgb::new(100, 0, 100), "add stacks the channels");
    }

    #[test]
    fn compositor_renders_a_full_device_frame() {
        // the device animate/stream/preview paths call render() directly — it fills rows*cols.
        let mut comp = Compositor::from_defs(&[LayerDef::default()]);
        let f = comp.render(6, 22, 0.0);
        assert_eq!(f.len(), 6 * 22);
        // a disabled layer contributes nothing (the board stays black underneath).
        let mut off = Compositor::from_defs(&[LayerDef {
            spectrum: Spectrum::solid(Rgb::new(255, 255, 255)),
            enabled: false,
            ..Default::default()
        }]);
        assert!(off.render(2, 2, 0.0).iter().all(|&c| c == Rgb::BLACK));
    }
}
