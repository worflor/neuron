// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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
use std::sync::OnceLock;
use std::time::Instant;

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

// ─────────────────────────────────── Bounds (the sprite substrate) ───────────────────────

/// The sub-rectangle (origin + size) a layer occupies on the board — the placement substrate a
/// bounds-aware pattern renders relative to. `(row0, col0)` is the top-left cell; `rows`×`cols` the
/// extent. Derived from a layer's region mask ([`Bounds::from_region`]); a region-less (whole-board)
/// layer gets [`Bounds::board`]. Most patterns ignore it and just fill `rows`×`cols`; a placement-aware
/// one (the `vitals` readout) scales its drawing to fit whatever rect it was handed, so it reads right
/// at any size from a two-cell strip to the full board.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub row0: u8,
    pub col0: u8,
    pub rows: u8,
    pub cols: u8,
}

impl Bounds {
    /// The whole board — origin `(0, 0)`, full extent. The placement a region-less layer occupies.
    pub fn board(rows: u8, cols: u8) -> Bounds {
        Bounds { row0: 0, col0: 0, rows, cols }
    }

    /// The bounding box of a region's cells on a `rows`×`cols` board (each index → `(r, c)` row-major).
    /// An empty region covers the whole board, so it maps to [`Bounds::board`]. Indices at/past the end
    /// of the board are skipped (defensive); if none are in range the result also falls back to the full
    /// board. Scattered cells collapse to the single ENCLOSING rectangle (the bbox), never the literal
    /// cells — the placement is a rectangle, the region mask still carves the exact shape on top.
    pub fn from_region(region: &[u32], rows: u8, cols: u8) -> Bounds {
        if region.is_empty() || rows == 0 || cols == 0 {
            return Bounds::board(rows, cols);
        }
        let c = cols as u32;
        let n = rows as u32 * c;
        let (mut r0, mut c0, mut r1, mut c1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut any = false;
        for &i in region {
            if i >= n {
                continue; // an out-of-board index can't define the placement — ignore it.
            }
            let (r, col) = (i / c, i % c);
            r0 = r0.min(r);
            c0 = c0.min(col);
            r1 = r1.max(r);
            c1 = c1.max(col);
            any = true;
        }
        if !any {
            return Bounds::board(rows, cols);
        }
        Bounds {
            row0: r0 as u8,
            col0: c0 as u8,
            rows: (r1 - r0 + 1) as u8,
            cols: (c1 - c0 + 1) as u8,
        }
    }
}

// ─────────────────── pure stack / placement helpers (extracted from the app glue) ───────────────────
//
// Small, side-effect-free index math the GUI's lighting glue leans on. Kept in core (not trapped in a
// Slint closure) so the fiddly cases — a remove BELOW the selection, a whole-board place rect — are
// unit-tested once here rather than re-derived (and mis-derived) at the call site.

/// The selected-layer index after removing layer `removed` from a stack, given the prior `selected`
/// index and the NEW length `new_len` (AFTER the removal). Removing a layer BELOW the selection shifts
/// everything above it down one, so the selection must DECREMENT to stay on the same layer; removing the
/// selection itself (or anything above it) leaves the index put, only clamped to the new last layer. An
/// emptied stack (`new_len == 0`) has no selection → `0`. Pure, so the glue can't get the below-the-
/// selection case wrong (the old single-clamp left `selected` pointing one layer too high).
pub fn selection_after_remove(removed: usize, selected: usize, new_len: usize) -> usize {
    if new_len == 0 {
        return 0;
    }
    let sel = if removed < selected { selected.saturating_sub(1) } else { selected };
    sel.min(new_len - 1)
}

/// The row-major cell indices a PLACE rectangle covers on a `rows`×`cols` board — the pure geometry the
/// app's place-gesture commit runs. The two corners `(r0,c0)`–`(r1,c1)` may arrive in ANY order and off
/// the board; they're clamped to `[0,rows-1]`×`[0,cols-1]` and ordered (lo,hi). A rect covering the WHOLE
/// board returns an EMPTY vec — the canonical "region-less / full board" the [`Compositor`] treats as no
/// mask (so a full-board drag and a Reset converge honestly). Otherwise the enclosed cells, row-major. A
/// degenerate board (`rows`/`cols` == 0) yields empty. Pure, so the placement math is tested away from Slint.
pub fn region_from_rect(r0: i32, c0: i32, r1: i32, c1: i32, rows: u8, cols: u8) -> Vec<u32> {
    if rows == 0 || cols == 0 {
        return Vec::new();
    }
    let (rmax, cmax) = (rows as i32 - 1, cols as i32 - 1);
    let rlo = r0.min(r1).clamp(0, rmax);
    let rhi = r0.max(r1).clamp(0, rmax);
    let clo = c0.min(c1).clamp(0, cmax);
    let chi = c0.max(c1).clamp(0, cmax);
    // a whole-board rect stores as EMPTY (region-less) — the canonical full board the compositor masks by.
    if rlo == 0 && clo == 0 && rhi == rmax && chi == cmax {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(((rhi - rlo + 1) * (chi - clo + 1)) as usize);
    for r in rlo..=rhi {
        for c in clo..=chi {
            v.push((r as u32) * cols as u32 + c as u32);
        }
    }
    v
}

/// A stateful shape-and-motion generator. One instance per layer; `field` is called once per tick.
/// `Send` because a built [`Compositor`] may live on whichever thread drives a device (the app's
/// per-board anim threads today, the protocol host's writer/kernel threads too) — patterns are plain
/// data plus atomic reads, so this states a fact rather than adding a burden.
pub trait Pattern: Send {
    /// Apply the layer's param values. Called once when the layer is built and again when a knob
    /// changes. The default ignores params (for patterns that declare none).
    fn configure(&mut self, _params: &Params) {}

    /// Receive the layer's raw per-LED cells. Custom / static-frame layers paint these directly;
    /// procedural patterns ignore them (defaulted, mirroring how [`configure`](Pattern::configure) is).
    fn set_frame(&mut self, _cells: &[[u8; 3]]) {}

    /// Receive the placement rect this layer occupies; placement-aware patterns render relative to it,
    /// the rest ignore it, defaulted like [`set_frame`](Pattern::set_frame). The compositor computes it
    /// from the layer's region ([`Bounds::from_region`]) and calls this once per tick before [`field`]
    /// (Pattern::field), so a bounds-aware pattern always sees its current rect.
    fn set_bounds(&mut self, _b: Bounds) {}

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
    /// Raw per-LED cells for a `custom` (hand-painted / imported) static-frame layer — one `[R, G, B]`
    /// per LED, row-major. Empty (and omitted from TOML) for every procedural pattern; the `custom`
    /// pattern reads it via [`Pattern::set_frame`]. This is how an imported/painted frame becomes a
    /// first-class layer in the stack rather than a per-profile sidecar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frame: Vec<[u8; 3]>,
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
            frame: Vec::new(),
        }
    }
}

impl LayerDef {
    /// Build the live, configured [`Pattern`] for this layer (or `None` if the pattern key is unknown).
    pub fn make_pattern(&self) -> Option<Box<dyn Pattern>> {
        let mut p = make_pattern(&self.pattern)?;
        p.configure(&self.params);
        p.set_frame(&self.frame);
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
    /// Does this pattern colour THROUGH the 1-D [`Spectrum`] (i.e. it emits a [`Field::Scalar`])? `true`
    /// for every scalar shape; `false` for the full-colour patterns that emit [`Field::Color`] directly
    /// and ignore the spectrum (`screen`, `custom`, `vitals`). The app reads this to hide the spectrum
    /// editor for full-colour layers instead of branching on the pattern key by hand.
    pub has_spectrum: bool,
    /// Is this a device-telemetry READOUT (data rendered as a gauge) rather than a decorative effect?
    /// `true` only for `vitals`. Lets the app surface / group it distinctly without string-matching keys.
    pub readout: bool,
}

/// The registry — the SINGLE source of truth for every pattern. The factory ([`make_pattern`]), the
/// inspector schema ([`pattern_params`]), the default spectra and the tile catalog ALL derive from
/// this one table. Adding a pattern = ONE entry here + the [`Pattern`] impl — nothing else.
///
/// The thirteen shapes the whole effect set collapses to (the colour of each lives in its
/// [`Spectrum`], chosen per preset — see [`presets`]): `uniform` (Static/Breathing/Cycle), `axis`
/// (Wave), `radial` (Color Wheel), `heat` (Fire), `rain` (Cascade — rain/matrix), `comet` (Comet), `sparkle`
/// (Starlight), `ignite` (Reactive), `ring` (Ripple), `flow` (Aurora), `thermal` (Typing Heat),
/// `meter` (Audio Meter + Pulse) and `screen` (Ambient — the full-colour exception). Plus two
/// full-colour non-effect layers: `custom` (a painted/imported frame) and `vitals` (the device's live
/// battery/charge readout as a compositable, resolution-independent gauge).
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
    },
    PatternDef {
        key: "rain",
        label: "Rain",
        make: || Box::new(Rain::default()),
        params: || vec![mode_param(), speed_param(), density_param()],
        default_spectrum: sp_cascade,
        tile: TileMeta {
            blurb: "falling rain — or matrix code streams, white-hot heads",
            live_input: false,
        },
        has_spectrum: true,
        readout: false,
    },
    PatternDef {
        key: "comet",
        label: "Comet",
        make: || Box::new(Comet::default()),
        params: || vec![speed_param(), density_param()],
        default_spectrum: streak_spectrum,
        tile: TileMeta {
            blurb: "streaking comets — break one with a keypress",
            live_input: true,
        },
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
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
        has_spectrum: true,
        readout: false,
    },
    PatternDef {
        key: "meter",
        label: "Meter",
        make: || Box::new(Meter::default()),
        params: || vec![source_param(), focus_param(), speed_param()],
        default_spectrum: meter_spectrum,
        tile: TileMeta {
            blurb: "a live meter — audio loudness or system load",
            live_input: true,
        },
        has_spectrum: true,
        readout: false,
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
        has_spectrum: false,
        readout: false,
    },
    PatternDef {
        key: "custom",
        label: "Custom",
        make: || Box::new(StaticFrame::default()),
        params: Vec::new,
        default_spectrum: || Spectrum::solid(Rgb::new(0, 0, 0)),
        tile: TileMeta {
            blurb: "a hand-painted or imported per-key frame",
            live_input: false,
        },
        has_spectrum: false,
        readout: false,
    },
    PatternDef {
        key: "vitals",
        label: "Vitals",
        make: || Box::new(Vitals::default()),
        params: Vec::new,
        // A Color (full-colour) pattern — it paints its own gauge colours and IGNORES the spectrum; the
        // registry still requires a non-empty default, so a solid accent stands in (never sampled).
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "the device's live battery & charge as a gauge",
            live_input: true,
        },
        has_spectrum: false,
        readout: true,
    },
    PatternDef {
        key: "onair",
        label: "On Air",
        make: || Box::new(OnAir::default()),
        params: || vec![signal_param(), standby_param()],
        default_spectrum: onair_spectrum,
        tile: TileMeta {
            blurb: "lights where you paint it while your stream is live",
            live_input: true,
        },
        // A SCALAR readout — unlike vitals it colours THROUGH the spectrum, so the user paints the
        // on-air look with the full engine (any colour, gradient, breathe/cycle motion).
        has_spectrum: true,
        readout: true,
    },
    PatternDef {
        key: "miclight",
        label: "Mic Light",
        make: || Box::new(MicLight::default()),
        params: || vec![show_param()],
        default_spectrum: onair_spectrum, // the same warning red as On Air (repaintable, as ever)
        tile: TileMeta {
            blurb: "lights where you paint it while your mic is muted (or hot)",
            live_input: true,
        },
        has_spectrum: true,
        readout: true,
    },
    PatternDef {
        key: "modeheld",
        label: "Mode Held",
        make: || Box::new(ModeHeld::default()),
        params: || vec![held_param()],
        default_spectrum: || Spectrum::solid(ACCENT),
        tile: TileMeta {
            blurb: "lights while a hold layer or sniper is engaged",
            live_input: true,
        },
        has_spectrum: true,
        readout: true,
    },
    PatternDef {
        key: "signal",
        label: "Signal",
        make: || Box::new(SignalLight::default()),
        params: || vec![channel_param(), style_param()],
        default_spectrum: sp_pulse, // the green→amber→red urgency ramp `level` reads along
        tile: TileMeta {
            blurb: "a light your macros drive: neuron.signal(channel, value)",
            live_input: true,
        },
        has_spectrum: true,
        readout: true,
    },
];

/// The house default accent (the weave teal) — the neutral colour solid-spectrum patterns start in.
const ACCENT: Rgb = Rgb::new(0x4A, 0xF2, 0xB0);

/// The full registry slice — the tile catalog reads this.
pub fn registry() -> &'static [PatternDef] {
    REGISTRY
}

/// Look up a pattern definition by key (case-insensitive). The retired `streak` key (the pre-split
/// rain+comet pattern whose `mode` knob cross-linked the two tiles) aliases to `rain` — a saved
/// streak layer keeps rendering (mode 0 = rain unchanged; the rare mode-1 comet layer lands on
/// rain's `matrix` submode and re-picks its own Comet tile in one click).
pub fn pattern_def(key: &str) -> Option<&'static PatternDef> {
    let key = if key.eq_ignore_ascii_case("streak") { "rain" } else { key };
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

/// Does this pattern colour through the 1-D [`Spectrum`] (a [`Field::Scalar`] pattern), so the spectrum
/// editor is meaningful for it? Registry-driven. An unknown key assumes `true` — the safe default
/// (show the editor rather than silently hide it). `false` for the full-colour patterns (`screen`,
/// `custom`, `vitals`). Replaces app-side string-matching on the pattern key.
pub fn pattern_has_spectrum(key: &str) -> bool {
    pattern_def(key).map(|d| d.has_spectrum).unwrap_or(true)
}

/// Is this pattern a device-telemetry READOUT (a gauge, not a decorative effect)? Registry-driven;
/// `true` only for `vitals`, unknown → `false`. Replaces app-side string-matching on the pattern key.
pub fn pattern_is_readout(key: &str) -> bool {
    pattern_def(key).map(|d| d.readout).unwrap_or(false)
}

// ─────────────────────────────────── the render clock (one epoch, one formula) ─────────────

/// The process-global RENDER CLOCK epoch — one shared `Instant` every animated surface quantises
/// against, so the GUI preview and the device stream advance in the SAME discrete frames (the preview
/// provably mirrors the board). Both read the elapsed via [`render_elapsed`] (the wrapped, precision-safe
/// f32) and quantise it with [`quantized_t`]; a device stream keeps its own start only for the run-duration
/// bound + pacing, never for the phase. Shared and unchanging, so a stream restart can't jump the phase.
pub fn render_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Quantise an elapsed time (seconds) to whole `1/fps` steps — floor `elapsed·fps` back to `/fps`. The
/// ONE render-clock formula the GUI preview and the device `animate` loop both run, so their phases step
/// identically (chunky at 6 fps, smooth at 30). `fps` is floored to ≥1 (a 0 would divide by zero).
pub fn quantized_t(elapsed_secs: f32, fps: u32) -> f32 {
    let fps = fps.max(1) as f32;
    (elapsed_secs * fps).floor() / fps
}

/// The render clock's elapsed seconds, WRAPPED at 4096s so the f32 keeps frame-level precision at any
/// uptime — the ONE choke point every surface reads instead of `render_epoch().elapsed().as_secs_f32()`.
/// The wrap MUST happen in the integer/duration domain: `elapsed().as_secs_f32() % 4096.0` is already
/// broken, because after ~a day the f32 has shed sub-frame bits BEFORE the modulo (patterns stutter, then
/// freeze). We fold in MILLIS (u128, exact) and only then scale to seconds, so the result carries full
/// ms precision forever. Sibling of weave.rs `seconds()`. The noise/absolute-`t` fields are periodic, so
/// the one reseed per ~68min is imperceptible; speed-scaled motion now rides dt accumulators (Axis/Radial/
/// Flow/Meter) and never sees the reseed at all.
pub fn render_elapsed() -> f32 {
    (render_epoch().elapsed().as_millis() % 4_096_000) as f32 * 1e-3
}

// shared param-schema constructors (reused across pattern defs so the ranges read consistently)

/// A `speed` rate knob (the design default is 1.0).
fn speed_param() -> Param {
    Param {
        key: "speed",
        label: "speed",
        only_when: None,
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
        only_when: None,
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
        only_when: None,
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
        only_when: None,
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
        only_when: None,
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
        only_when: None,
        kind: ParamKind::Range {
            min: 1.0,
            max: 3.0,
            default: 1.0,
        },
    }
}

/// Rain's `mode` — the fall's CHARACTER: `rain` (staggered drops, breathing gaps) or `matrix`
/// (continuous per-column code streams: varied column speeds, long luminous tails, glyph-shimmer).
/// The old third occupant of this knob — comet — is its own pattern now: the cross-link that let
/// the Cascade tile toggle into Comet (and vice versa) hid one effect inside the other.
fn mode_param() -> Param {
    Param {
        key: "mode",
        label: "mode",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["rain", "matrix"],
            default: 0,
        },
    }
}

/// The reactive neighbour-`glow` toggle (Ignite) — off lights only the pressed key's cell.
fn glow_param() -> Param {
    Param {
        key: "glow",
        label: "neighbour glow",
        only_when: None,
        kind: ParamKind::Toggle { default: false },
    }
}

/// Meter's `source` — the live signal driving the bars: speaker output, mic, CPU, RAM, or the
/// combined CPU/RAM "load" view (the old Pulse). The label IS the value (data-driven, no bespoke UI).
fn source_param() -> Param {
    Param {
        key: "source",
        label: "source",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["speakers", "mic", "cpu", "ram", "load"],
            default: 0,
        },
    }
}

/// Meter's `focus` — WHERE the audio meter listens (Synapse's tunable sensitivity, done honestly):
/// `auto` weighs the whole mix and self-frames; `bass`/`mids`/`highs` drive the brightness from that
/// register alone — `bass` is the classic "pumps with the kick" feel. Gated to the audio sources
/// (`only_when` source = speakers/mic): the load meters have no registers, so the knob simply
/// doesn't render there instead of sitting dead.
fn focus_param() -> Param {
    Param {
        key: "focus",
        label: "focus",
        only_when: Some(("source", &[0, 1])),
        kind: ParamKind::Enum {
            options: &["auto", "bass", "mids", "highs"],
            default: 0,
        },
    }
}

/// On-Air's `signal` — which OBS-announced truth lights the layer: the live stream, a running
/// recording, or either. Everything behind this knob was announced by OBS itself (and resynced at
/// connect), never assumed.
fn signal_param() -> Param {
    Param {
        key: "signal",
        label: "signal",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["stream", "record", "stream or record"],
            default: 0,
        },
    }
}

/// On-Air's `standby` — a faint trace while OBS is CONNECTED but the signal is off, so the painted
/// placement stays visible (and provably armed) without ever reading as "live".
fn standby_param() -> Param {
    Param {
        key: "standby",
        label: "standby glow",
        only_when: None,
        kind: ParamKind::Toggle { default: false },
    }
}

/// Mic Light's `show` — which real mute state lights the layer: `muted` (the red-slash
/// convention — dark means you're live) or `hot mic` (lit = the world can hear you).
fn show_param() -> Param {
    Param {
        key: "show",
        label: "lights when",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["muted", "hot mic"],
            default: 0,
        },
    }
}

/// Mode Held's `signal` — which held input mode lights the layer.
fn held_param() -> Param {
    Param {
        key: "signal",
        label: "signal",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["hold layer", "sniper", "either"],
            default: 0,
        },
    }
}

/// Signal's `channel` — which macro-drivable channel this layer renders (1-indexed to match
/// `neuron.signal(channel, value)`).
fn channel_param() -> Param {
    Param {
        key: "channel",
        label: "channel",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["1", "2", "3", "4"],
            default: 0,
        },
    }
}

/// Signal's `style` — `level` (the value picks the colour along the spectrum, full brightness)
/// or `glow` (the value is the brightness of the spectrum across the placement).
fn style_param() -> Param {
    Param {
        key: "style",
        label: "style",
        only_when: None,
        kind: ParamKind::Enum {
            options: &["level", "glow"],
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
    // integrated SCROLL phase (Sparkle idiom). The old `t * speed` slid the gradient by the never-reset
    // epoch × the LIVE speed — so editing speed rescaled the whole accrued shift and TELEPORTED the wave
    // (a jump proportional to uptime). We integrate speed·dt instead; the value wraps naturally in [0,1).
    shift_phase: f32,
    last_t: f32,
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
        // integrate the shift so a live speed edit can't rescale the accrued phase (a `t * speed` jump).
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.shift_phase = (self.shift_phase + dt * self.speed * SCROLL_RATE).rem_euclid(1.0);
        let shift = self.shift_phase;
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
    // integrated SPIN phase (Sparkle idiom). `t * speed` spun by the never-reset epoch × the LIVE speed,
    // so a speed edit rescaled the accrued angle and JUMPED the wheel (proportional to uptime). Integrate
    // speed·dir·dt; wraps naturally in [0,1). A live direction flip just reverses the increment (no jump).
    spin_phase: f32,
    last_t: f32,
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
        // integrate the spin so a live speed edit can't rescale the accrued angle (a `t * speed` jump).
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.spin_phase = (self.spin_phase + dt * self.speed * SPIN_RATE * dir).rem_euclid(1.0);
        let spin = self.spin_phase;
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
        // An advancing clock runs exactly what wall-time bought — zero steps on most frames when the
        // rate sits below the render fps (the old ≥1-per-frame floor tied the flicker to the frame rate
        // and deadened the knob's low end). A static `t` / time reset (dt ≤ 0) still steps once (never
        // freezes); the burst is capped so a long stall can't run thousands of steps in one frame.
        const BASE_STEPS_PER_SEC: f32 = 18.0;
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.speed.clamp(0.1, 6.0);
        self.step_acc += dt * BASE_STEPS_PER_SEC * spd;
        let mut steps = if dt > 0.0 { self.step_acc.floor() as u32 } else { 1 };
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

// ──────────────────────────── the streak family: Rain (Cascade) + Comet ─────────────────────────
//
// One family, TWO patterns. They used to be a single `streak` pattern whose `mode` knob toggled
// rain ↔ comet — which meant BOTH tiles carried a knob that morphed one effect into the other
// (Comet hidden inside Cascade's inspector and vice versa). Split so each owns its identity;
// `mode` now belongs to Rain alone and picks the fall's CHARACTER (rain / matrix).

/// The shared sim-step accumulator the streak family paces with: steps the sim by elapsed-time
/// accumulation (~`base`/sec at speed 1.0) so the look is fps-independent. An advancing clock runs
/// EXACTLY the steps wall-time bought — zero on most frames when the rate is below the render fps
/// (a ≥1-per-frame floor would tie the sim to the frame rate and make slow speeds a lie). A
/// static/reset clock (dt == 0 — the first frame after init, or a frozen `t`) still steps once so
/// the sim never freezes. Capped at 8 so a long stall can't run thousands of steps in one frame.
#[derive(Default)]
struct StepClock {
    last_t: f32,
    acc: f32,
}

impl StepClock {
    fn reset(&mut self, t: f32) {
        self.last_t = t;
        self.acc = 0.0;
    }

    fn accrue(&mut self, t: f32, base_per_sec: f32, speed: f32) -> u32 {
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.acc += dt * base_per_sec * speed.clamp(0.1, 6.0);
        let steps = if dt > 0.0 { self.acc.floor() as u32 } else { 1 };
        self.acc -= self.acc.floor();
        steps.min(8)
    }
}

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

/// Rain (the Cascade tile): per-column vertical falls, tail → white-hot head, coloured by the
/// spectrum. Two characters via `mode`:
///   * **rain**: each column runs an independent staggered drop — a bright head with a fading tail,
///     respawning after a breathing gap. A living downpour.
///   * **matrix**: continuous CODE STREAMS — every column its own pace (rolled per spawn), longer
///     luminous tails, near-instant re-entry so the board reads as always-flowing glyph columns,
///     and a per-step glyph SHIMMER (random trail cells re-roll their brightness, the "characters
///     changing" read). The terminal-rain look.
///
/// `speed` scales the fall rate, `density` the column population (drizzle → storm). Emits per cell
/// `(u, intensity)`: the head rides `u → 1` (the spectrum's hot/white end), the tail sits at `u ≈ 0`
/// fading out. Paced by [`StepClock`] so the look is identical at any frame rate.
#[derive(Default)]
pub struct Rain {
    mode: u8,
    speed: f32,
    density: f32,
    dims: (u8, u8),
    rng: u32,
    clock: StepClock,
    level: Vec<f32>,
    head: Vec<f32>,
    active: Vec<bool>,
    wait: Vec<f32>,
    /// Per-column stream pace (0.6..1.6), re-rolled at every spawn — read by MATRIX mode so each
    /// code column falls at its own speed (plain rain keeps the uniform fall).
    col_speed: Vec<f32>,
}

impl Rain {
    fn rand(&mut self) -> f32 {
        xorshift(&mut self.rng)
    }

    /// Average respawn gap in sim-steps — shorter at higher density (busier downpour), longer at low.
    fn gap(&self) -> f32 {
        const BASE_GAP_STEPS: f32 = 40.0;
        (BASE_GAP_STEPS / self.density.clamp(0.25, 3.0)).max(2.0)
    }

    fn spawn(&mut self, col: usize) {
        self.active[col] = true;
        self.head[col] = -(self.rand() * 3.0);
        self.col_speed[col] = 0.6 + self.rand(); // matrix reads it; plain rain ignores it
    }

    /// Seed the per-column fall so the board is alive on the first frame; active probability scales
    /// with `density`, and MATRIX starts fuller (code walls read wrong half-empty).
    fn init(&mut self, r: usize, c: usize) {
        let dens = self.density.clamp(0.25, 3.0);
        let base_p = if self.mode == 1 { 0.50 } else { 0.30 };
        let p_active = (base_p + 0.23 * dens).clamp(0.0, 0.95);
        let g = self.gap();
        for x in 0..c {
            self.col_speed[x] = 0.6 + self.rand();
            if self.rand() < p_active {
                self.active[x] = true;
                self.head[x] = self.rand() * r as f32;
            } else {
                self.active[x] = false;
                self.wait[x] = self.rand() * g;
            }
        }
    }

    /// Advance one step: fade every trail a notch (the exponential tail), shimmer matrix glyphs,
    /// then drop each active head (painting white-hot where it lands) or count down a waiting
    /// column's respawn gap. Mode picks the character constants: matrix falls slightly faster per
    /// step, keeps longer tails, and re-enters almost immediately (continuous streams) where rain
    /// breathes between drops.
    fn step(&mut self, r: usize, c: usize) {
        let matrix = self.mode == 1;
        let decay = if matrix { 0.90 } else { 0.80 };
        let advance = if matrix { 0.30 } else { 0.22 };
        for v in self.level.iter_mut() {
            *v *= decay;
            if *v < 0.02 {
                *v = 0.0;
            }
        }
        if matrix {
            // GLYPH SHIMMER: a few random trail cells re-roll their brightness each step — the
            // "characters changing" flicker that makes code rain read as code, not just streaks.
            // Only TRAIL cells (below the head band) shimmer, so heads stay clean white.
            let n = r * c;
            for _ in 0..(c / 3).max(1) {
                let i = ((self.rand() * n as f32) as usize).min(n.saturating_sub(1));
                let v = self.level[i];
                if v > 0.06 && v < 0.85 {
                    self.level[i] = (v * (0.55 + self.rand() * 0.8)).clamp(0.0, 0.85);
                }
            }
        }
        for x in 0..c {
            if self.active[x] {
                let h = self.head[x];
                if h >= 0.0 && (h as usize) < r {
                    self.level[h as usize * c + x] = 1.0;
                }
                let mul = if matrix { self.col_speed[x] } else { 1.0 };
                let nh = h + advance * mul;
                self.head[x] = nh;
                if nh >= r as f32 + 4.0 {
                    self.active[x] = false;
                    let g = self.gap();
                    // matrix streams re-enter almost at once (a code wall never sits empty);
                    // rain takes a proper breath between drops.
                    self.wait[x] =
                        if matrix { g * (0.1 + 0.3 * self.rand()) } else { g * (0.5 + self.rand()) };
                }
            } else {
                self.wait[x] -= 1.0;
                if self.wait[x] <= 0.0 {
                    self.spawn(x);
                }
            }
        }
    }
}

impl Pattern for Rain {
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
            self.clock.reset(t);
            self.level = vec![0.0; n];
            self.head = vec![0.0; c];
            self.active = vec![false; c];
            self.wait = vec![0.0; c];
            self.col_speed = vec![1.0; c];
            if n > 0 {
                self.init(r, c);
            }
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }

        // The base rate ANCHORS speed 1.0 at lively rain — 8 steps/sec × 0.22 rows/step ≈ 1.8
        // rows/sec, a drop crossing a keyboard in ~3.4s. (The previous anchor of 5/sec ≈ 1.1
        // rows/sec read as molasses at the design default; before THAT, 20/sec was a downpour
        // nobody ran, shipped pre-slowed to 0.25 on the knob — the knob now spans drizzle→storm
        // around a default that's actually right.)
        let steps = self.clock.accrue(t, 8.0, self.speed);
        for _ in 0..steps {
            self.step(r, c);
        }
        // emit (u, intensity): the tail (v < HEAD_THRESH) sits at u≈0 (the spectrum's tail colour)
        // with intensity = v; the head ramps u → 1 (the spectrum's white head) at full intensity.
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

/// Comet (its own tile — no longer a mode hidden inside Cascade): an endless PARADE of varied,
/// cardinal-biased streaks on free velocity vectors that DON'T wrap — a comet runs off the board,
/// dies, and a fresh DIFFERENT one enters shortly after; a keypress on a live head BREAKS it (a
/// white-hot burst) and respawns it different. `speed` is the travel rate, `density` the parade
/// size (1 calm streak by default, up to ~7). Emits `(u, intensity)` with `u` rising toward the
/// head, so the head reads as the spectrum's hot/white end. Paced by [`StepClock`].
#[derive(Default)]
pub struct Comet {
    speed: f32,
    density: f32,
    dims: (u8, u8),
    rng: u32,
    clock: StepClock,
    comets: Vec<CometBody>,
    burst: Vec<f32>,
    prev: Vec<bool>,
}

impl Comet {
    fn rand(&mut self) -> f32 {
        xorshift(&mut self.rng)
    }

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

}

impl Pattern for Comet {
    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0);
        self.density = p.f32("density", 1.0);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.rng = 0x2545_F491;
            self.dims = (rows, cols);
            self.clock.reset(t);
            self.burst = vec![0.0; n];
            self.comets.clear();
            self.prev = vec![false; KEY_SCAN_SLOTS];
        }
        if n == 0 {
            return Field::Scalar(Vec::new());
        }

        // Reconcile the parade to the density count, scattering fresh comets along their path
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
        let steps = self.clock.accrue(t, 24.0, self.speed);
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
    // ONE integrated flow phase (Sparkle idiom). Every drift term is linear in ∫speed·dt, so a single
    // accumulator feeds all three: the old `t * 0.10 * speed` etc. multiplied the never-reset epoch by the
    // LIVE speed, so editing speed rescaled every accrued flow and JOLTED the whole field. We wrap `flow_t`
    // at 1000.0 — where the primary drift terms (×0.10/0.067/0.041) all land on integer sine-cycles, a
    // seamless reseed — which also keeps `flow_t` from the f32 precision death an unbounded integral hits.
    flow_t: f32,
    last_t: f32,
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
        // integrate one base phase so a live speed edit can't rescale the accrued flows (a `t * spd` jolt).
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        self.flow_t = (self.flow_t + dt * spd).rem_euclid(1000.0);
        let t1 = self.flow_t * 0.10;
        let t2 = self.flow_t * 0.067;
        let t3 = self.flow_t * 0.041;
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

/// Meter: the board is a live METER, coloured by the spectrum.
///
/// `speakers`/`mic` make the ENTIRE board ONE uniform surface (Synapse's Audio Meter shape — no
/// spatial fill, no moving edge, nothing to flicker) driven by TWO channels from the shared
/// [`crate::audio_spectrum`] analysis:
///   * **colour** (`u`) = the TONE — the log-frequency centroid of the mix, so the gradient is a
///     register axis: a kick drum slams the board into the gradient's deep end, vocals and synths
///     live in its middle, cymbals lift it toward the top;
///   * **brightness** (intensity) = the LOUDNESS — VU-integrated and auto-gained with crest
///     headroom, so the board pumps with the beat at any listening volume and true silence is an
///     honestly dark board.
/// Where Synapse's meter slides one colour by raw amplitude, this hears WHAT is playing, not just
/// how loud — the EQ depth a single level number can't carry. If no PCM stream can open it
/// degrades to the OS peak (brightness only, mid-gradient colour).
///
/// `cpu`/`ram` drive a horizontal load bar (`u = load`, so a calm→urgent ramp colours the bar by
/// how hard the machine is working); `load` is the combined Pulse view (CPU on top, RAM on the
/// bottom), the whole board breathing faster under CPU load.
///
/// `focus` is Synapse's tunable sensitivity, done honestly: `auto` listens to the whole mix
/// (self-framing), `bass`/`mids`/`highs` drive the brightness from that register alone — each with
/// its own auto-gain, so "bass" pumps with the kick no matter how bright the cymbals are. `speed`
/// scales the ballistics (brightness attack/release, breath rate) — the responsiveness knob,
/// Synapse's requested-and-never-shipped "decay". dt-scaled, so a legacy ~6fps board and the 60fps
/// preview trace the same envelope through the SAME shared providers.
#[derive(Default)]
pub struct Meter {
    source: u8,
    speed: f32,
    focus: u8,
    // audio-tint state (per instance — each surface keeps its own ballistics but reads the same
    // shared provider, so the device and the preview agree)
    bar: f32,
    last_t: f32,
    // load-meter breathe PHASE (integrated per-instance). The breathe rate rises with CPU, so the old
    // `t * rate` form rescaled the WHOLE accrued phase every time load moved — a strobe proportional to
    // uptime. We integrate rate·dt into this accumulator instead (Sparkle/Heat idiom), wrapping it.
    breath_phase: f32,
}

impl Pattern for Meter {
    fn configure(&mut self, p: &Params) {
        self.source = p.u8("source", 0);
        self.speed = p.f32("speed", 1.0);
        self.focus = p.u8("focus", 0);
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
                crate::audio_spectrum::ensure(src);
                let region = (self.focus as usize).min(crate::audio_spectrum::REGIONS - 1);
                let (level, tone) = crate::audio_spectrum::signal()
                    .map(|s| (s.levels[region], s.tone))
                    .unwrap_or_else(|| {
                        // no PCM stream (off-platform / exotic endpoint) — the honest OS peak
                        // instead, with the colour parked mid-gradient (no tone data to hear).
                        crate::audio_level::ensure(src);
                        (crate::audio_level::level(), 0.5)
                    });
                // dt from the shared render clock (0.25 cap absorbs pauses; covers a 6fps board's 167ms)
                let dt = (t - self.last_t).clamp(0.0, 0.25);
                self.last_t = t;
                self.bar = meter_step_bar(self.bar, level.clamp(0.0, 1.0), dt, spd);
                Field::Scalar(paint_audio_tint(tone, self.bar, n))
            }
            _ => {
                crate::sys_stats::ensure();
                let cpu = crate::sys_stats::cpu();
                let ram = crate::sys_stats::ram();
                // breathe: the rate rises with CPU, so we INTEGRATE rate·dt into a wrapped phase (never
                // `t * rate`, which would slew the whole accrued phase on every load change). dt from the
                // shared render clock (0.25 cap absorbs pauses / a 6fps board's 167ms frame).
                let dt = (t - self.last_t).clamp(0.0, 0.25);
                self.last_t = t;
                let rate = (0.4 + 2.0 * cpu.clamp(0.0, 1.0)) * spd;
                self.breath_phase = (self.breath_phase + rate * dt).rem_euclid(1.0);
                let breath = (0.85 + 0.15 * (self.breath_phase * TAU).sin()).clamp(0.0, 1.0);
                Field::Scalar(render_load_meter(self.source, cpu, ram, breath, r, c))
            }
        }
    }
}

// audio-meter ballistics — the taste constants. The provider already VU-integrates the loudness
// (~150ms), so these only shape the last glide: quick enough to pop on a beat, slow enough that
// the brightness never strobes. Per-second rates, dt-scaled (frame-rate-independent), scaled by `speed`.
const BAR_ATTACK: f32 = 15.0; // 1/s — rise rate toward a louder level
const BAR_RELEASE: f32 = 5.0; // 1/s — sink rate toward a quieter level

/// One ballistic step for the meter level: fast attack toward a louder target, slow release toward
/// a quieter one, dt- and speed-scaled via an exponential approach (frame-rate-independent).
fn meter_step_bar(h: f32, target: f32, dt: f32, speed: f32) -> f32 {
    let rate = if target > h { BAR_ATTACK } else { BAR_RELEASE };
    let a = 1.0 - (-rate * speed * dt.max(0.0)).exp();
    (h + (target - h) * a).clamp(0.0, 1.0)
}

/// The pure audio-meter painter — the two-channel tint: every cell identical, `u = the tone` (the
/// gradient is a bass→treble register axis) and `intensity = the loudness` (the whole board pumps
/// with the beat; silence is dark). Deterministic + trivial — which is the point: no spatial edge
/// exists, so nothing can flicker.
fn paint_audio_tint(tone: f32, level: f32, n: usize) -> Vec<Cell> {
    vec![Cell::new(tone.clamp(0.0, 1.0), level.clamp(0.0, 1.0)); n]
}

/// The pure load-meter renderer — horizontal bars filling left→right in proportion to load, the WHOLE
/// board breathing (the rate rising with CPU). `source` selects `cpu`/`ram` (one full-board bar) or
/// `load` (CPU on the top half, RAM on the bottom). Each lit cell emits `u = the zone's load` (so the
/// spectrum's calm→urgent ramp colours the bar by load) and `intensity = the breath` (the fractional
/// leading edge dims smoothly). `breath` is the caller's integrated breathe envelope (a wrapped phase's
/// sin, in 0..1) — passed in, never re-derived from an absolute clock. Deterministic + testable.
fn render_load_meter(source: u8, cpu: f32, ram: f32, breath: f32, r: usize, c: usize) -> Vec<Cell> {
    let mut cells = vec![Cell::new(0.0, 0.0); r * c];
    let cpu = cpu.clamp(0.0, 1.0);
    let ram = ram.clamp(0.0, 1.0);
    let breath = breath.clamp(0.0, 1.0);
    let mut paint = |y0: usize, y1: usize, load: f32| {
        let filled = load * c as f32;
        for y in y0..y1 {
            for x in 0..c {
                let rank = x as f32;
                if rank + 1.0 <= filled {
                    cells[y * c + x] = Cell::new(load, breath);
                } else if rank < filled {
                    cells[y * c + x] = Cell::new(load, breath * (filled - rank));
                }
            }
        }
    };
    if source == 4 {
        let cpu_rows = r.div_ceil(2);
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

// ───────────────────────────────── Custom (the static-frame layer) ──────────────────────────────

/// Custom: a hand-painted or imported per-key frame promoted to a first-class layer. Like [`Screen`]
/// it's inherently full-colour, so it's the other [`Field::Color`] exception — it emits its raw cells
/// DIRECTLY, bypassing the 1-D spectrum. The cells arrive once via [`Pattern::set_frame`] (from the
/// layer's `frame`); a matrix bigger than the frame pads with black, a smaller one truncates.
#[derive(Default)]
struct StaticFrame {
    cells: Vec<Rgb>,
}

impl Pattern for StaticFrame {
    fn set_frame(&mut self, cells: &[[u8; 3]]) {
        self.cells = cells.iter().map(|c| Rgb::new(c[0], c[1], c[2])).collect();
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let n = rows as usize * cols as usize;
        let mut px = self.cells.clone();
        px.resize(n, Rgb::new(0, 0, 0));
        Field::Color(px)
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

// ─────────────────────────────── Vitals (the device-telemetry readout) ──────────────────────────

/// Vitals: the device's live battery / charge state as a first-class, COMPOSITABLE layer — the
/// cross-device data surface ([`crate::lighting::render_vitals`]) rebuilt as a resolution-independent
/// [`Pattern`]. Where that surface anchors onto MEANINGFUL physical keys (the number row, the F-keys)
/// and so only reads right on a full keyboard, this pattern fills whatever rect the layer occupies (its
/// [`Bounds`], from the region) with a PROPORTIONAL battery gauge — a `battery_color` fill scaled to the
/// placement, a charging crest sweeping the lit run — so it reads correctly at any size from a two-cell
/// strip to the whole board. Inherently full-colour, so it's a [`Field::Color`] pattern (spectrum-free,
/// like [`Screen`]/`Custom`). The snapshot comes from a SHARED feed ([`crate::lighting::publish_vitals`])
/// the app pushes; before the first publish the board idles pure BLACK (a live-input pattern with no
/// source) — and its preset overlays with [`Blend::Cut`], for which black means "nothing to say", so an
/// idle or empty-gauge cell falls through to the effect beneath instead of punching an opaque black hole
/// (and a lit gauge cell lands at TRUE colour, where the old Screen blend washed it into the effect).
///
/// Private (built only via the registry factory, like `Custom`/[`StaticFrame`]) so its name can't
/// collide with the [`crate::lighting::Vitals`] SNAPSHOT it renders in a downstream glob import.
#[derive(Default)]
struct Vitals {
    /// The rect this layer occupies, delivered by [`Pattern::set_bounds`] each tick. `None` until the
    /// compositor sets it (e.g. a bare `field` call in a test) — then the whole board is assumed.
    bounds: Option<Bounds>,
}

impl Pattern for Vitals {
    fn set_bounds(&mut self, b: Bounds) {
        self.bounds = Some(b);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let n = rows as usize * cols as usize;
        // No published snapshot yet → idle dark: a live readout with no source is honestly BLACK. (Unlike
        // the audio meter, which idles at its spectrum's LOW colour — that's a Scalar field; vitals is a
        // Color field of black.) When this layer is OVERLAID its preset defaults to `Blend::Cut`, for
        // which black falls THROUGH to the effect beneath (and lit cells replace at true colour).
        let v = match crate::lighting::latest_vitals() {
            Some(v) => v,
            None => return Field::Color(vec![Rgb::BLACK; n]),
        };
        let bounds = self.bounds.unwrap_or_else(|| Bounds::board(rows, cols));
        // CREST_RATE: charging-crest sweeps per second — a calm travelling highlight along the lit run.
        const CREST_RATE: f32 = 0.6;
        let phase = (t * CREST_RATE).rem_euclid(1.0);
        Field::Color(render_vitals_bounds(v, rows, cols, bounds, phase))
    }
}

/// The resolution-independent VITALS readout — the [`Vitals`] pattern's renderer, and the reason the
/// data surface becomes a real layer. Draws a PROPORTIONAL battery gauge into the `bounds` sub-rect of
/// an otherwise-black `rows*cols` board: the leftmost `round(pct% × bounds.cols)` columns of the rect
/// fill with [`battery_color`](crate::lighting::battery_color) (RED low → AMBER mid → GREEN full) across
/// the rect's full height, a non-zero battery always lighting ≥1 column (1% ≠ empty); while charging, a
/// bright cyan crest ([`VITALS_CYAN`](crate::lighting::VITALS_CYAN)) sweeps the lit run (`phase` 0..1).
/// It reuses the cross-device surface's MATH (the fill colour, the ≥1-lit rule, the crest lerp) but
/// places it PROPORTIONALLY, so it fills a 1×2 strip or the full board equally well — where the
/// key-anchored [`render_vitals`](crate::lighting::render_vitals) needs a real keyboard to read right.
/// Pure — no I/O, fully unit-testable. (DPI-stage pips stay in the key-anchored surface; a pip strip
/// doesn't generalise below the F-key count, so the proportional layer leads with the battery gauge.)
///
/// `pub` because the app's lighting-page renders the vitals TILE thumbnail through this SAME renderer
/// (over `Bounds::board`) instead of the key-anchored [`render_vitals`](crate::lighting::render_vitals),
/// so the swatch matches the APPLIED layer at any grid size rather than reading ~all-black off-keyboard.
pub fn render_vitals_bounds(v: crate::lighting::Vitals, rows: u8, cols: u8, b: Bounds, phase: f32) -> Vec<Rgb> {
    let (br, bc) = (rows as usize, cols as usize);
    let mut f = vec![Rgb::BLACK; br * bc];
    let (w, h) = (b.cols as usize, b.rows as usize);
    if br == 0 || bc == 0 || w == 0 || h == 0 {
        return f;
    }
    // BATTERY GAUGE — round(pct% × width) columns lit; any non-zero battery lights ≥1 (1% ≠ empty).
    let pct = v.battery_pct.min(100) as f32;
    let lit = if v.battery_pct == 0 {
        0
    } else {
        ((pct / 100.0 * w as f32).round() as usize).clamp(1, w)
    };
    let fill = crate::lighting::battery_color(v.battery_pct);
    // The charging crest travels across the lit run; ~2-column-wide bright cyan peak.
    let crest = phase * lit.max(1) as f32;
    for col in 0..lit {
        let color = if v.charging {
            let d = (col as f32 - crest).abs();
            let glow = (1.0 - d / 2.0).clamp(0.0, 1.0);
            Rgb::lerp(fill, crate::lighting::VITALS_CYAN, 0.30 + 0.70 * glow)
        } else {
            fill
        };
        // Paint the full height of the rect for this column, offset to the placement origin + clipped.
        for row in 0..h {
            let rr = b.row0 as usize + row;
            let cc = b.col0 as usize + col;
            if rr < br && cc < bc {
                f[rr * bc + cc] = color;
            }
        }
    }
    f
}

/// ON AIR — the broadcast truth as a paintable layer, NOT a forced colour. The user places it
/// (region) and dresses it (spectrum: any colour, gradient, breathe/cycle motion); this pattern
/// only decides WHEN it shows, from the OBS-announced state the app mirrors into
/// [`crate::lighting::publish_broadcast`]. Off-signal it renders zero-intensity (black), which its
/// readout preset composites with [`Blend::Cut`] — so the user's own lighting shows through until
/// the moment they're actually live. Honest by construction: no feed, or a torn-down OBS
/// connection, reads as off-air; a tally that might be wrong is worse than none.
#[derive(Default)]
struct OnAir {
    /// Which announced truth lights the layer: 0 = stream, 1 = record, 2 = either.
    signal: u8,
    /// Faint placement trace while connected but off-signal (opt-in).
    standby: bool,
    bounds: Option<Bounds>,
}

impl Pattern for OnAir {
    fn configure(&mut self, params: &Params) {
        self.signal = params.u8("signal", 0);
        self.standby = params.bool("standby", false);
    }

    fn set_bounds(&mut self, b: Bounds) {
        self.bounds = Some(b);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let b = crate::lighting::latest_broadcast();
        let connected = b.is_some_and(|b| b.connected);
        let live = b.is_some_and(|b| {
            b.connected
                && match self.signal {
                    1 => b.recording,
                    2 => b.streaming || b.recording,
                    _ => b.streaming,
                }
        });
        // Full brightness on-signal; a faint opt-in trace while merely connected; dark otherwise.
        const STANDBY: f32 = 0.10;
        let intensity = if live {
            1.0
        } else if self.standby && connected {
            STANDBY
        } else {
            0.0
        };
        placement_field(self.bounds, rows, cols, intensity)
    }
}

/// The shared body of the boolean readout layers (on-air / mic light / mode held): a scalar
/// field at one `intensity` whose `u` spans the PLACEMENT horizontally — so a gradient spectrum
/// paints across the painted region (a solid spectrum ignores `u`; Motion spectra breathe/cycle
/// on top). The spectrum does ALL the colour work; these patterns only gate. Zero intensity is
/// the all-dark field their Cut-blended presets treat as transparent.
fn placement_field(bounds: Option<Bounds>, rows: u8, cols: u8, intensity: f32) -> Field {
    let n = rows as usize * cols as usize;
    if intensity <= 0.0 {
        return Field::Scalar(vec![Cell::new(0.0, 0.0); n]);
    }
    let bounds = bounds.unwrap_or_else(|| Bounds::board(rows, cols));
    let span = bounds.cols.max(1) as f32 - 1.0;
    let mut cells = Vec::with_capacity(n);
    for _r in 0..rows {
        for c in 0..cols {
            let u = if span > 0.0 {
                ((c.saturating_sub(bounds.col0)) as f32 / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            cells.push(Cell::new(u, intensity));
        }
    }
    Field::Scalar(cells)
}

/// MIC LIGHT — "am I muted?" as a paintable layer. The truth comes from the shared
/// [`crate::mic_state`] provider (the system capture endpoint's real mute state, sampled off the
/// render path, idle-auto-stopping); the `show` knob picks which state lights it — `muted` (the
/// red-slash convention) or `hot mic` (lit = the world can hear you). UNKNOWN (no mic resolved)
/// renders dark, never a guess: a mute indicator that can be wrong is worse than none.
#[derive(Default)]
struct MicLight {
    /// false = light when MUTED (default); true = light when HOT.
    show_hot: bool,
    bounds: Option<Bounds>,
}

impl Pattern for MicLight {
    fn configure(&mut self, params: &Params) {
        self.show_hot = params.u8("show", 0) == 1;
    }

    fn set_bounds(&mut self, b: Bounds) {
        self.bounds = Some(b);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        crate::mic_state::ensure();
        let lit = match crate::mic_state::muted() {
            Some(muted) => {
                if self.show_hot {
                    !muted
                } else {
                    muted
                }
            }
            None => false,
        };
        placement_field(self.bounds, rows, cols, if lit { 1.0 } else { 0.0 })
    }
}

/// MODE HELD — the live input mode as a paintable layer: lights while a hold layer (HyperShift)
/// or a sniper hold is engaged, per the `signal` knob. Fed edge-accurately by the dispatch loop
/// ([`crate::lighting::publish_hold`]); before the live loop has ever published (or after it
/// stops and pushes the default) there is no mode to show and the layer is dark. Paint it over
/// the keys your hold layer rebinds and the board itself tells you which mode you're in.
#[derive(Default)]
struct ModeHeld {
    /// 0 = hold layer, 1 = sniper, 2 = either.
    signal: u8,
    bounds: Option<Bounds>,
}

impl Pattern for ModeHeld {
    fn configure(&mut self, params: &Params) {
        self.signal = params.u8("signal", 0);
    }

    fn set_bounds(&mut self, b: Bounds) {
        self.bounds = Some(b);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let lit = crate::lighting::latest_hold().is_some_and(|h| match self.signal {
            1 => h.sniper,
            2 => h.layer || h.sniper,
            _ => h.layer,
        });
        placement_field(self.bounds, rows, cols, if lit { 1.0 } else { 0.0 })
    }
}

/// SIGNAL — a light your macros drive: renders one of the numbered
/// [`crate::lighting::signal`] channels (`neuron.signal(2, 0.8)` sets channel 2 to 0.8). The
/// emergence seam of the data tiles: neuron doesn't know what the light MEANS (CI status, a
/// pomodoro, a boss timer, "someone joined voice") — the user's script decides, and this layer
/// renders it wherever they painted it. Two styles: `level` samples the spectrum AT the value
/// (an urgency ramp: 0.2 reads green, 1.0 reads red on the default gradient) at full
/// brightness; `glow` spans the spectrum across the placement with the value as brightness.
/// Zero — every channel's untouched default — is dark either way, so an unused channel costs
/// nothing and an unconfigured layer never lies.
#[derive(Default)]
struct SignalLight {
    channel: usize,
    /// false = level (value picks the colour); true = glow (value is the brightness).
    glow: bool,
    bounds: Option<Bounds>,
}

impl Pattern for SignalLight {
    fn configure(&mut self, params: &Params) {
        self.channel = params.u8("channel", 0) as usize;
        self.glow = params.u8("style", 0) == 1;
    }

    fn set_bounds(&mut self, b: Bounds) {
        self.bounds = Some(b);
    }

    fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
        let v = crate::lighting::signal(self.channel);
        if v <= 0.0 {
            return placement_field(self.bounds, rows, cols, 0.0);
        }
        if self.glow {
            return placement_field(self.bounds, rows, cols, v);
        }
        // level: every cell samples the spectrum AT the value, full brightness — the value IS
        // the colour coordinate, so a green→amber→red gradient reads as urgency.
        let n = rows as usize * cols as usize;
        Field::Scalar(vec![Cell::new(v, 1.0); n])
    }
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

/// The default meter gradient — a bass→treble REGISTER axis for the audio tint (`u = tone`): deep
/// violet where the kick lives, the house accent through the mids, white at the cymbals' end.
/// Brightness (loudness) is carried by intensity, so no black foot is needed — silence dims the
/// board to dark whatever the colours. Pulse overrides with a green → amber → red urgency ramp.
fn meter_spectrum() -> Spectrum {
    Spectrum::gradient(vec![Rgb::new(0x7B, 0x2F, 0xF2), ACCENT, Rgb::new(255, 255, 255)])
}

/// On-Air's default: the classic tally red — a STARTING point, never a mandate (the whole point of
/// the layer is that the user repaints it with any spectrum the engine can hold).
fn onair_spectrum() -> Spectrum {
    Spectrum::solid(Rgb::new(255, 0, 0))
}

// ───────────────────────────────────────── PRESETS (pure data) ─────────────────────────────────
//
// A preset = { label, pattern key, pattern param values, spectrum }. The tile grid IS this list.
// Adding a "look" is ONE entry here — zero code. Every effect from the old menu maps to a (pattern +
// default spectrum) preset; the full collapse of the effect set onto the thirteen shapes.

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
    /// One plain-words line: what this look IS (and, for a fed tile, what feeds it). Preset-specific
    /// because several presets share one pattern (Audio Meter and Pulse are both `meter` but read
    /// different worlds).
    pub blurb: &'static str,
    /// The real-world feed this tile reads, named for the catalog chip ("keys", "audio", "battery",
    /// "OBS", "mic", …). Empty for a pure light show (clock-driven, reads nothing).
    pub source: &'static str,
}

impl Preset {
    /// Which catalog SHELF this look belongs on — the taxonomy the lighting page groups its tiles by:
    /// `"effect"` (a light show on a clock — reads nothing), `"input"` (driven by your live typing),
    /// `"data"` (reads a real feed — audio, screen, battery, stream state, a macro signal). Derived
    /// from the registry truth (`readout` / `live_input`) plus the two fed-but-not-readout patterns
    /// (`meter` reads audio/load, `screen` reads your desktop), so a new preset lands on the right
    /// shelf for free.
    pub fn group(&self) -> &'static str {
        if pattern_is_readout(self.pattern) || matches!(self.pattern, "meter" | "screen") {
            "data"
        } else if pattern_def(self.pattern).is_some_and(|d| d.tile.live_input) {
            "input"
        } else {
            "effect"
        }
    }

    /// Build the [`LayerDef`] this preset describes (a fresh layer ready to composite/persist).
    pub fn to_layer(&self) -> LayerDef {
        LayerDef {
            pattern: self.pattern.into(),
            params: (self.params)(),
            spectrum: (self.spectrum)(),
            region: Vec::new(),
            // A READOUT overlay (the vitals gauge, the on-air light) defaults to Cut, not Normal:
            // black is Cut's "nothing to say", so idle/empty cells fall THROUGH to the effect
            // beneath, while lit cells land at TRUE colour. (Screen used to carry this job — its
            // black-identity gave the fall-through, but it WASHED the lit colour into whatever ran
            // underneath: battery red over an aurora read pink. Cut keeps both halves honest.)
            // Registry-driven — a future readout preset inherits the right blend for free.
            blend: if pattern_is_readout(self.pattern) { Blend::Cut } else { Blend::Normal },
            enabled: true,
            frame: Vec::new(),
        }
    }
}

/// The PRESET catalog — the full effect set collapsed onto the thirteen shapes, in grid order. The single
/// source for the (phase-3) tile grid. Each is pure data: a pattern key, param overrides, and a spectrum.
pub fn presets() -> Vec<Preset> {
    vec![
        Preset { slug: "static", label: "Static", pattern: "uniform", params: pp_none, spectrum: sp_static,
            blurb: "one colour across the whole board", source: "" },
        Preset { slug: "breathing", label: "Breathing", pattern: "uniform", params: pp_none, spectrum: sp_breathing,
            blurb: "the colour breathes — a slow rise and fall", source: "" },
        Preset { slug: "cycle", label: "Cycle", pattern: "uniform", params: pp_none, spectrum: sp_cycle,
            blurb: "the whole board cycles through the spectrum", source: "" },
        Preset { slug: "wave", label: "Wave", pattern: "axis", params: pp_none, spectrum: spectrum::rainbow,
            blurb: "a gradient scrolling along an axis", source: "" },
        Preset { slug: "colorwheel", label: "Color Wheel", pattern: "radial", params: pp_none, spectrum: spectrum::rainbow,
            blurb: "a hue wheel turning around the centre", source: "" },
        Preset { slug: "fire", label: "Fire", pattern: "heat", params: pp_none, spectrum: fire_spectrum,
            blurb: "an upward fire — heat rises, flickers, cools", source: "" },
        Preset { slug: "typingheat", label: "Typing Heat", pattern: "thermal", params: pp_none, spectrum: thermal_spectrum,
            blurb: "your typing rendered as a living heat map", source: "keys" },
        Preset { slug: "cascade", label: "Cascade", pattern: "rain", params: pp_rain, spectrum: sp_cascade,
            blurb: "falling rain — matrix streams, white-hot heads", source: "" },
        Preset { slug: "comet", label: "Comet", pattern: "comet", params: pp_none, spectrum: streak_spectrum,
            blurb: "streaking comets — break one with a keypress", source: "keys" },
        Preset { slug: "starlight", label: "Starlight", pattern: "sparkle", params: pp_none, spectrum: sp_starlight,
            blurb: "random twinkles igniting and fading like stars", source: "" },
        Preset { slug: "reactive", label: "Reactive", pattern: "ignite", params: pp_none, spectrum: sp_solid_accent,
            blurb: "lights the key you press, then fades", source: "keys" },
        Preset { slug: "ripple", label: "Ripple", pattern: "ring", params: pp_none, spectrum: sp_solid_accent,
            blurb: "a keypress sends a ring rippling outward", source: "keys" },
        Preset { slug: "aurora", label: "Aurora", pattern: "flow", params: pp_none, spectrum: aurora_spectrum,
            blurb: "a slow aurora flow drifting over the board", source: "" },
        Preset { slug: "audiometer", label: "Audio Meter", pattern: "meter", params: pp_audio, spectrum: meter_spectrum,
            blurb: "brightness follows loudness, colour follows tone", source: "audio" },
        Preset { slug: "pulse", label: "Pulse", pattern: "meter", params: pp_load, spectrum: sp_pulse,
            blurb: "a gauge filling with your CPU + RAM load", source: "system" },
        Preset { slug: "ambient", label: "Ambient", pattern: "screen", params: pp_none, spectrum: sp_solid_accent,
            blurb: "the board mirrors the colours on your screen", source: "screen" },
        Preset { slug: "vitals", label: "Vitals", pattern: "vitals", params: pp_none, spectrum: sp_solid_accent,
            blurb: "the device's live battery & charge as a gauge", source: "battery" },
        Preset { slug: "onair", label: "On Air", pattern: "onair", params: pp_none, spectrum: onair_spectrum,
            blurb: "lights where you paint it while your stream is live", source: "OBS" },
        Preset { slug: "miclight", label: "Mic Light", pattern: "miclight", params: pp_none, spectrum: onair_spectrum,
            blurb: "lights where you paint it while your mic is muted (or hot)", source: "mic" },
        Preset { slug: "modeheld", label: "Mode Held", pattern: "modeheld", params: pp_none, spectrum: sp_solid_accent,
            blurb: "lights while a hold layer or sniper is engaged", source: "modes" },
        Preset { slug: "signal", label: "Signal", pattern: "signal", params: pp_none, spectrum: sp_pulse,
            blurb: "a light your macros drive: neuron.signal(n, v)", source: "macros" },
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
    // speed stays at the design default 1.0 — the rain's base rate itself is anchored so 1.0 IS the
    // lively rain (see the step-rate note in `Rain::field`); no pre-slowed override needed.
    let mut p = Params::default();
    p.set("mode", 0.0);
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
            // Hand the pattern its placement rect (the region's bbox, or the whole board when
            // region-less) BEFORE it renders, so a bounds-aware pattern fills exactly its sub-rect. The
            // region mask below still gives the exact-shape transparency; this only sizes the drawing.
            let bbox = Bounds::from_region(&layer.region, rows, cols);
            layer.pattern.set_bounds(bbox);
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

            // capability honesty: a full-colour pattern (Field::Color) bypasses the spectrum, so it
            // MUST declare has_spectrum = false — the flag and the real field kind can never drift.
            if matches!(f, Field::Color(_)) {
                assert!(!d.has_spectrum, "{}: a Color pattern must not claim a spectrum", d.key);
            }

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

    // ── the ON AIR readout — the broadcast truth as a paintable layer ────────────────────────

    /// The broadcast slot is process-global (like the vitals feed), so the onair tests serialize
    /// on the same lock the other global-poking tests use.
    #[test]
    fn onair_lights_only_when_the_broadcast_says_live_and_never_goes_stale() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::lighting::{clear_broadcast, publish_broadcast, Broadcast};
        clear_broadcast();
        let def = preset_layer("onair").expect("onair preset");
        assert_eq!(def.blend, Blend::Cut, "a readout preset composites as a cutout");
        let mut comp = Compositor::from_defs(&[def]);
        // no feed at all → dark (a readout with no source never guesses)
        assert!(comp.render(2, 4, 0.0).iter().all(|p| *p == Rgb::BLACK));
        // connected but off-air → still dark (standby is opt-in)
        publish_broadcast(Broadcast { connected: true, streaming: false, recording: false });
        assert!(comp.render(2, 4, 0.1).iter().all(|p| *p == Rgb::BLACK));
        // live → the layer's OWN spectrum at full brightness (preset default: tally red)
        publish_broadcast(Broadcast { connected: true, streaming: true, recording: false });
        assert!(comp.render(2, 4, 0.2).iter().all(|p| *p == Rgb::new(255, 0, 0)));
        // the teardown publish (disconnected) kills it — a torn-down OBS can NEVER leave a
        // stale "live" on the board, even though `streaming` was last announced true.
        publish_broadcast(Broadcast { connected: false, streaming: true, recording: false });
        assert!(comp.render(2, 4, 0.3).iter().all(|p| *p == Rgb::BLACK));
        clear_broadcast();
    }

    #[test]
    fn onair_signal_knob_picks_which_truth_and_standby_traces_placement() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::lighting::{clear_broadcast, publish_broadcast, Broadcast};
        // signal = record: a running RECORDING lights it, a live stream alone does not.
        let mut def = preset_layer("onair").expect("onair preset");
        def.params.set("signal", 1.0);
        let mut comp = Compositor::from_defs(&[def]);
        publish_broadcast(Broadcast { connected: true, streaming: true, recording: false });
        assert!(comp.render(1, 4, 0.0).iter().all(|p| *p == Rgb::BLACK));
        publish_broadcast(Broadcast { connected: true, streaming: false, recording: true });
        assert!(comp.render(1, 4, 0.1).iter().all(|p| *p == Rgb::new(255, 0, 0)));
        // standby: connected + off-signal shows a FAINT trace (placement visible, never "live"),
        // and disconnection extinguishes even that.
        let mut def = preset_layer("onair").expect("onair preset");
        def.params.set("standby", 1.0);
        let mut comp = Compositor::from_defs(&[def]);
        publish_broadcast(Broadcast { connected: true, streaming: false, recording: false });
        let px = comp.render(1, 4, 0.2);
        assert!(px.iter().all(|p| p.r > 0 && p.r < 80 && p.g == 0 && p.b == 0), "faint red trace, got {px:?}");
        publish_broadcast(Broadcast::default());
        assert!(comp.render(1, 4, 0.3).iter().all(|p| *p == Rgb::BLACK));
        clear_broadcast();
    }

    // ── MIC LIGHT / MODE HELD / SIGNAL — the rest of the data-tile family ────────────────────

    #[test]
    fn miclight_renders_the_shown_state_and_never_guesses() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let def = preset_layer("miclight").expect("miclight preset");
        assert_eq!(def.blend, Blend::Cut, "a readout preset composites as a cutout");
        let mut comp = Compositor::from_defs(&[def]);
        // UNKNOWN (no mic resolved) → dark, whichever state the knob shows.
        crate::mic_state::test_set(0);
        assert!(comp.render(1, 4, 0.0).iter().all(|p| *p == Rgb::BLACK));
        // muted → the default shows it (red); live → dark.
        crate::mic_state::test_set(2);
        assert!(comp.render(1, 4, 0.1).iter().all(|p| *p == Rgb::new(255, 0, 0)));
        crate::mic_state::test_set(1);
        assert!(comp.render(1, 4, 0.2).iter().all(|p| *p == Rgb::BLACK));
        // show = hot mic inverts the gate.
        let mut def = preset_layer("miclight").expect("miclight preset");
        def.params.set("show", 1.0);
        let mut comp = Compositor::from_defs(&[def]);
        assert!(comp.render(1, 4, 0.3).iter().all(|p| *p == Rgb::new(255, 0, 0)), "hot mic lights when live");
        crate::mic_state::test_set(2);
        assert!(comp.render(1, 4, 0.4).iter().all(|p| *p == Rgb::BLACK), "hot mic goes dark when muted");
        crate::mic_state::test_set(0);
    }

    #[test]
    fn modeheld_follows_the_dispatch_hold_state() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::lighting::{clear_hold, publish_hold, HoldState};
        clear_hold();
        let accent = Rgb::new(0x4A, 0xF2, 0xB0);
        let def = preset_layer("modeheld").expect("modeheld preset");
        let mut comp = Compositor::from_defs(&[def]);
        // no live loop has ever published → dark (there is no mode to show)
        assert!(comp.render(1, 4, 0.0).iter().all(|p| *p == Rgb::BLACK));
        // a held layer lights the default; releasing it darkens; sniper alone does NOT
        // light the layer-signal default.
        publish_hold(HoldState { layer: true, sniper: false });
        assert!(comp.render(1, 4, 0.1).iter().all(|p| *p == accent));
        publish_hold(HoldState { layer: false, sniper: true });
        assert!(comp.render(1, 4, 0.2).iter().all(|p| *p == Rgb::BLACK));
        // signal = either lights on sniper too; the teardown default darkens everything.
        let mut def = preset_layer("modeheld").expect("modeheld preset");
        def.params.set("signal", 2.0);
        let mut comp = Compositor::from_defs(&[def]);
        assert!(comp.render(1, 4, 0.3).iter().all(|p| *p == accent));
        publish_hold(HoldState::default());
        assert!(comp.render(1, 4, 0.4).iter().all(|p| *p == Rgb::BLACK));
        clear_hold();
    }

    #[test]
    fn signal_layer_renders_its_channel_as_level_or_glow() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::lighting::set_signal;
        set_signal(0, 0.0);
        set_signal(1, 0.0);
        // level (default): the value picks the colour along the layer's own spectrum — use a
        // black→white ramp so the expected colour is exact greyscale.
        let mut def = preset_layer("signal").expect("signal preset");
        def.spectrum = Spectrum::gradient(vec![Rgb::BLACK, Rgb::new(255, 255, 255)]);
        let mut comp = Compositor::from_defs(&[def.clone()]);
        // untouched channel (0.0) → dark: an unused channel costs nothing and never lies.
        assert!(comp.render(1, 4, 0.0).iter().all(|p| *p == Rgb::BLACK));
        set_signal(0, 1.0);
        assert!(comp.render(1, 4, 0.1).iter().all(|p| *p == Rgb::new(255, 255, 255)));
        // channel isolation: this layer reads channel 1 (knob "2"), which is still zero.
        def.params.set("channel", 1.0);
        let mut comp = Compositor::from_defs(&[def.clone()]);
        assert!(comp.render(1, 4, 0.2).iter().all(|p| *p == Rgb::BLACK));
        // glow: the value is the BRIGHTNESS of the spectrum (solid white × 0.5 ≈ mid grey).
        set_signal(1, 0.5);
        def.spectrum = Spectrum::solid(Rgb::new(255, 255, 255));
        def.params.set("style", 1.0);
        let mut comp = Compositor::from_defs(&[def]);
        let px = comp.render(1, 4, 0.3);
        assert!(
            px.iter().all(|p| p.r > 100 && p.r < 155 && p.r == p.g && p.g == p.b),
            "glow at 0.5 reads ~half-bright, got {px:?}"
        );
        // out-of-range values clamp on write, so a wild macro can't overdrive the layer.
        set_signal(0, 9.0);
        assert_eq!(crate::lighting::signal(0), 1.0);
        set_signal(0, 0.0);
        set_signal(1, 0.0);
    }

    #[test]
    fn onair_composites_as_a_cutout_over_the_users_own_lighting() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use crate::lighting::{clear_broadcast, publish_broadcast, Broadcast};
        clear_broadcast();
        // a teal base with an onair layer painted on cells {1, 2} of a 1×4 strip
        let base = LayerDef { spectrum: Spectrum::solid(Rgb::new(0, 100, 100)), ..Default::default() };
        let mut tally = preset_layer("onair").expect("onair preset");
        tally.region = vec![1, 2];
        let mut comp = Compositor::from_defs(&[base, tally]);
        // OFF-AIR: the user's own lighting shows EVERYWHERE — the painted region punches no hole.
        publish_broadcast(Broadcast { connected: true, streaming: false, recording: false });
        assert!(comp.render(1, 4, 0.0).iter().all(|p| *p == Rgb::new(0, 100, 100)));
        // LIVE: exactly the painted cells read TRUE red (no screen-wash), the rest stay the base.
        publish_broadcast(Broadcast { connected: true, streaming: true, recording: false });
        let px = comp.render(1, 4, 0.1);
        assert_eq!(px[0], Rgb::new(0, 100, 100));
        assert_eq!(px[1], Rgb::new(255, 0, 0));
        assert_eq!(px[2], Rgb::new(255, 0, 0));
        assert_eq!(px[3], Rgb::new(0, 100, 100));
        clear_broadcast();
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
            frame: Vec::new(),
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

    // ── the full registry — all thirteen shapes present ─────────────────────────────────────────

    #[test]
    fn registry_has_the_full_thirteen_shapes() {
        let keys = pattern_keys();
        for k in [
            "uniform", "axis", "radial", "heat", "rain", "comet", "sparkle", "ignite", "ring", "flow",
            "thermal", "meter", "screen",
        ] {
            assert!(keys.contains(&k), "registry is missing the '{k}' pattern");
        }
        // the thirteen procedural shapes + the `custom` static-frame layer + the DATA readouts
        // (`vitals`, `onair`, `miclight`, `modeheld`, `signal`).
        assert!(keys.contains(&"custom"), "registry is missing the 'custom' layer type");
        for k in ["vitals", "onair", "miclight", "modeheld", "signal"] {
            assert!(keys.contains(&k), "registry is missing the '{k}' readout");
        }
        assert_eq!(keys.len(), 19, "the thirteen shapes + the custom frame layer + the five readouts");
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
    fn rain_fills_and_animates() {
        let mut s = Rain::default();
        s.configure(&Params::defaults_for("rain")); // mode default 0 = rain
        let a = scalar(s.field(6, 22, 0.0));
        assert_eq!(a.len(), 6 * 22);
        assert!(a.iter().any(|c| c.intensity > 0.0), "rain lights some cells");
        let b = scalar(s.field(6, 22, 1.0));
        assert!(a != b, "the rain animates over time");
        assert!(a.iter().all(|c| (0.0..=1.0).contains(&c.u) && (0.0..=1.0).contains(&c.intensity)));
    }

    #[test]
    fn rain_matrix_mode_streams_continuously() {
        let mut s = Rain::default();
        let mut p = Params::default();
        p.set("mode", 1.0); // matrix — continuous code streams
        s.configure(&p);
        let a = scalar(s.field(6, 22, 0.0));
        assert!(a.iter().any(|c| c.intensity > 0.0), "the code wall is alive from frame one");
        let b = scalar(s.field(6, 22, 1.0));
        assert!(a != b, "the streams animate over time");
        assert!(a.iter().all(|c| (0.0..=1.0).contains(&c.u) && (0.0..=1.0).contains(&c.intensity)));
        // per-column pace: matrix rolls a distinct speed per stream — the signature variance.
        let distinct = s.col_speed.windows(2).any(|w| (w[0] - w[1]).abs() > 1e-3);
        assert!(distinct, "matrix columns fall at their own speeds");
        // over a long run every stream keeps re-entering (near-continuous columns, no dead board).
        for k in 2..40 {
            let _ = s.field(6, 22, k as f32);
        }
        let end = scalar(s.field(6, 22, 40.0));
        assert!(end.iter().any(|c| c.intensity > 0.0), "the code wall never goes dark");
    }

    #[test]
    fn legacy_streak_key_aliases_to_rain() {
        // saved layers from before the split (pattern = "streak") must keep resolving: the alias
        // lands on rain (mode 0 = identical rain; mode 1 now reads as rain's matrix submode).
        assert!(make_pattern("streak").is_some(), "the retired key still builds a pattern");
        assert_eq!(pattern_def("streak").unwrap().key, "rain");
        assert!(!pattern_params("streak").is_empty(), "the alias serves the schema too");
    }

    #[test]
    fn comet_breaks_and_respawns() {
        let mut s = Comet::default();
        s.configure(&Params::default());
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
    fn comet_count_scales_with_density() {
        let count = |d: f32| {
            let mut s = Comet::default();
            let mut p = Params::default();
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
        let count_m1 = |prev: &mut Vec<bool>| {
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
    fn meter_bar_ballistics_attack_fast_release_slow() {
        // one 60fps step toward a transient climbs much further than one step releasing from it.
        let rise = meter_step_bar(0.0, 1.0, 1.0 / 60.0, 1.0);
        let fall = 1.0 - meter_step_bar(1.0, 0.0, 1.0 / 60.0, 1.0);
        assert!(rise > fall * 2.0, "attack ({rise}) far outpaces release ({fall})");
        // dt-scaling: a 6fps board's single big step lands close to ten 60fps steps (same envelope).
        let one_big = meter_step_bar(0.0, 1.0, 10.0 / 60.0, 1.0);
        let mut ten_small = 0.0;
        for _ in 0..10 {
            ten_small = meter_step_bar(ten_small, 1.0, 1.0 / 60.0, 1.0);
        }
        assert!(
            (one_big - ten_small).abs() < 0.02,
            "the exponential approach is frame-rate-independent ({one_big} vs {ten_small})"
        );
    }

    #[test]
    fn meter_focus_knob_is_gated_to_the_audio_sources() {
        // the `focus` knob (Synapse's tunable sensitivity) is schema-gated: visible only while the
        // meter's source is an AUDIO one (speakers/mic) — a load meter has no registers, so the
        // knob must not render there as a dead control.
        let params = pattern_params("meter");
        let focus = params.iter().find(|p| p.key == "focus").expect("meter declares focus");
        assert_eq!(
            focus.only_when,
            Some(("source", &[0u8, 1][..])),
            "focus is visible only for the speakers/mic sources"
        );
        // every other meter knob is unconditional.
        for p in params.iter().filter(|p| p.key != "focus") {
            assert!(p.only_when.is_none(), "{} is an always-on knob", p.key);
        }
    }

    #[test]
    fn meter_audio_tint_is_one_uniform_surface_tone_by_colour_loudness_by_brightness() {
        // EVERY cell identical (the uniform Synapse shape — no spatial edge, nothing to flicker):
        // u = the tone (the gradient's bass→treble register axis), intensity = the loudness.
        let cells = paint_audio_tint(0.2, 0.8, 6 * 22);
        assert_eq!(cells.len(), 6 * 22);
        assert!(cells.iter().all(|&c| c == cells[0]), "the whole board is ONE surface");
        assert_eq!(cells[0], Cell::new(0.2, 0.8), "colour carries the tone, brightness the loudness");
        // silence → intensity 0 → an honestly dark board, whatever the tone reads.
        let dark = paint_audio_tint(0.7, 0.0, 4);
        assert_eq!(dark[0].intensity, 0.0, "no loudness → no light");
        // out-of-range inputs clamp.
        let clamped = paint_audio_tint(7.0, -3.0, 1)[0];
        assert_eq!((clamped.u, clamped.intensity), (1.0, 0.0));
        // the default meter gradient is a register axis: bass ≠ mids ≠ treble, treble ends white.
        let g = meter_spectrum();
        let (bass, mid, treble) = (g.at(0.0, 0.0), g.at(0.0, 0.5), g.at(0.0, 1.0));
        assert_ne!(bass, mid, "the kick's register has its own colour");
        assert_eq!(treble, Rgb::new(255, 255, 255), "the cymbals' end crests white");
    }

    #[test]
    fn meter_load_colours_the_bar_by_load() {
        // a CPU bar at 50% fills the left half; every lit cell samples the spectrum at u = the load (so the
        // calm→urgent ramp colours the whole bar by how hard the machine is working).
        let cells = render_load_meter(2, 0.5, 0.0, 0.85, 6, 22);
        assert!(cells[0].intensity > 0.0, "the bar's left is lit");
        assert!((cells[0].u - 0.5).abs() < 1e-6, "u carries the load level");
        assert_eq!(cells[21].intensity, 0.0, "the unfilled right stays dark");
        // the combined "load" view splits CPU (top) over RAM (bottom).
        let split = render_load_meter(4, 1.0, 0.0, 0.85, 6, 22);
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

    // ── Bounds (the sprite substrate) + set_bounds wiring ─────────────────────────────────────────

    #[test]
    fn bounds_from_region_is_the_enclosing_bbox() {
        // empty region → the whole board.
        assert_eq!(Bounds::from_region(&[], 6, 22), Bounds::board(6, 22));
        assert_eq!(Bounds::board(6, 22), Bounds { row0: 0, col0: 0, rows: 6, cols: 22 });
        // a contiguous 2×2 block at (1,1) on a 4×4 board (cells 5,6,9,10) → exactly that rect.
        assert_eq!(
            Bounds::from_region(&[5, 6, 9, 10], 4, 4),
            Bounds { row0: 1, col0: 1, rows: 2, cols: 2 }
        );
        // scattered cells collapse to the single ENCLOSING bbox: (0,1) and (3,2) on a 4×4 board.
        assert_eq!(
            Bounds::from_region(&[1, 14], 4, 4),
            Bounds { row0: 0, col0: 1, rows: 4, cols: 2 }
        );
        // a single cell → a 1×1 rect at that cell.
        assert_eq!(Bounds::from_region(&[10], 4, 4), Bounds { row0: 2, col0: 2, rows: 1, cols: 1 });
        // out-of-board indices are ignored; all-out-of-range falls back to the full board.
        assert_eq!(Bounds::from_region(&[999], 4, 4), Bounds::board(4, 4));
    }

    #[test]
    fn compositor_hands_each_layer_its_region_bbox() {
        use std::sync::{Arc, Mutex as StdMutex};

        // A probe pattern that records the last Bounds the compositor handed it. (Arc+Mutex, not
        // Rc+Cell: `Pattern: Send` — patterns cross onto anim/writer threads.)
        struct Probe(Arc<StdMutex<Option<Bounds>>>);
        impl Pattern for Probe {
            fn set_bounds(&mut self, b: Bounds) {
                *self.0.lock().unwrap() = Some(b);
            }
            fn field(&mut self, rows: u8, cols: u8, _t: f32) -> Field {
                Field::Scalar(vec![Cell::new(0.0, 0.0); rows as usize * cols as usize])
            }
        }

        // a region carving a 2×2 block at (1,1) on a 4×4 board → the layer's bbox is that rect.
        let seen = Arc::new(StdMutex::new(None));
        let mut comp = Compositor {
            layers: vec![Layer {
                pattern: Box::new(Probe(seen.clone())),
                spectrum: Spectrum::solid(Rgb::new(1, 2, 3)),
                region: vec![5, 6, 9, 10],
                blend: Blend::Normal,
                enabled: true,
            }],
        };
        let _ = comp.render(4, 4, 0.0);
        assert_eq!(*seen.lock().unwrap(), Some(Bounds { row0: 1, col0: 1, rows: 2, cols: 2 }));

        // a region-less layer is handed the whole board.
        let seen2 = Arc::new(StdMutex::new(None));
        let mut comp2 = Compositor {
            layers: vec![Layer {
                pattern: Box::new(Probe(seen2.clone())),
                spectrum: Spectrum::solid(Rgb::new(1, 2, 3)),
                region: Vec::new(),
                blend: Blend::Normal,
                enabled: true,
            }],
        };
        let _ = comp2.render(4, 4, 0.0);
        assert_eq!(*seen2.lock().unwrap(), Some(Bounds::board(4, 4)));
    }

    // ── capability flags (registry-driven; no app-side key string-matching) ───────────────────────

    #[test]
    fn capability_flags_are_registry_driven() {
        // scalar shapes colour through the spectrum; the full-colour patterns don't.
        assert!(pattern_has_spectrum("uniform"));
        assert!(pattern_has_spectrum("meter"));
        assert!(!pattern_has_spectrum("screen"));
        assert!(!pattern_has_spectrum("custom"));
        assert!(!pattern_has_spectrum("vitals"));
        // only vitals is a data readout.
        assert!(pattern_is_readout("vitals"));
        assert!(!pattern_is_readout("meter"));
        assert!(!pattern_is_readout("screen"));
        // unknown keys take the safe defaults (show the editor; not a readout).
        assert!(pattern_has_spectrum("nope"));
        assert!(!pattern_is_readout("nope"));
    }

    // ── Vitals — the resolution-independent readout + its live feed ───────────────────────────────

    #[test]
    fn vitals_readout_fills_bounds_proportionally() {
        use crate::lighting::{battery_color, Vitals as VSnap};
        // FULL board, 100% → every column of the board lights the green fill (charging off).
        let full = render_vitals_bounds(
            VSnap { battery_pct: 100, charging: false, active_stage: 0, stage_count: 1 },
            2, 10, Bounds::board(2, 10), 0.0,
        );
        assert_eq!(full.len(), 20);
        assert!(full.iter().all(|&c| c == battery_color(100)), "100% fills the whole rect");
        // 50% lights the left half (round(0.5×10)=5 columns) across BOTH rows; the rest dark.
        let half = render_vitals_bounds(
            VSnap { battery_pct: 50, charging: false, active_stage: 0, stage_count: 1 },
            2, 10, Bounds::board(2, 10), 0.0,
        );
        for row in 0..2 {
            for col in 0..5 {
                assert_eq!(half[row * 10 + col], battery_color(50), "col {col} lit");
            }
            for col in 5..10 {
                assert_eq!(half[row * 10 + col], Rgb::BLACK, "col {col} dark");
            }
        }
        // 0% → nothing lit anywhere.
        let empty = render_vitals_bounds(
            VSnap { battery_pct: 0, charging: false, active_stage: 0, stage_count: 1 },
            2, 10, Bounds::board(2, 10), 0.0,
        );
        assert!(empty.iter().all(|&c| c == Rgb::BLACK), "0% lights nothing");
    }

    #[test]
    fn vitals_readout_is_resolution_independent_and_offset() {
        use crate::lighting::Vitals as VSnap;
        // A TINY 1×2 strip still reads: 50% lights exactly one of its two cells (proportional, not a
        // fixed sprite) — the "looks correct from 1×2 up" guarantee.
        let tiny = render_vitals_bounds(
            VSnap { battery_pct: 50, charging: false, active_stage: 0, stage_count: 1 },
            1, 2, Bounds::board(1, 2), 0.0,
        );
        assert_eq!(tiny.len(), 2);
        assert_ne!(tiny[0], Rgb::BLACK, "the one lit cell");
        assert_eq!(tiny[1], Rgb::BLACK, "the other stays dark at 50% of two cells");
        // ANY non-zero battery lights ≥1 cell even on a tiny strip (1% ≠ empty).
        let one = render_vitals_bounds(
            VSnap { battery_pct: 1, charging: false, active_stage: 0, stage_count: 1 },
            1, 2, Bounds::board(1, 2), 0.0,
        );
        assert_ne!(one[0], Rgb::BLACK, "1% still lights one cell");
        // A sub-rect placement paints ONLY inside its bounds, at the right origin: (1,1)+2×2 on a 4×4
        // board at 100% lights (1,1),(1,2),(2,1),(2,2) and nothing outside.
        let sub = render_vitals_bounds(
            VSnap { battery_pct: 100, charging: false, active_stage: 0, stage_count: 1 },
            4, 4, Bounds { row0: 1, col0: 1, rows: 2, cols: 2 }, 0.0,
        );
        for i in 0..16usize {
            let (r, c) = (i / 4, i % 4);
            let inside = (1..=2).contains(&r) && (1..=2).contains(&c);
            if inside {
                assert_ne!(sub[i], Rgb::BLACK, "cell {i} inside the rect lights");
            } else {
                assert_eq!(sub[i], Rgb::BLACK, "cell {i} outside the rect stays dark");
            }
        }
    }

    #[test]
    fn vitals_readout_charging_alters_the_lit_run() {
        use crate::lighting::Vitals as VSnap;
        let off = render_vitals_bounds(
            VSnap { battery_pct: 60, charging: false, active_stage: 0, stage_count: 1 },
            2, 10, Bounds::board(2, 10), 0.0,
        );
        let on = render_vitals_bounds(
            VSnap { battery_pct: 60, charging: true, active_stage: 0, stage_count: 1 },
            2, 10, Bounds::board(2, 10), 0.0,
        );
        assert!(off != on, "charging shifts the lit run toward the cyan crest");
    }

    #[test]
    fn vitals_pattern_gates_on_a_published_source() {
        // Serialised: the vitals feed is a process-global, so don't race a test that also publishes.
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // No source published → the board idles dark (a Color field of black), like the quiet meter.
        crate::lighting::clear_vitals();
        let mut p = make_pattern("vitals").expect("vitals pattern builds");
        p.set_bounds(Bounds::board(2, 6));
        match p.field(2, 6, 0.0) {
            Field::Color(px) => {
                assert_eq!(px.len(), 12);
                assert!(px.iter().all(|&c| c == Rgb::BLACK), "no source → a dark board");
            }
            _ => panic!("vitals emits a Color field"),
        }
        // Publish a full battery → the readout now lights the board.
        crate::lighting::publish_vitals(crate::lighting::Vitals {
            battery_pct: 100,
            charging: false,
            active_stage: 0,
            stage_count: 1,
        });
        match p.field(2, 6, 0.0) {
            Field::Color(px) => {
                assert!(px.iter().any(|&c| c != Rgb::BLACK), "a published source lights the readout");
            }
            _ => panic!("vitals emits a Color field"),
        }
        // Leave the global clean for any other test.
        crate::lighting::clear_vitals();
    }

    #[test]
    fn vitals_overlay_cuts_out_over_the_base() {
        // Serialised: the vitals feed is a process-global, so don't race a publisher.
        let _g = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let (rows, cols) = (2u8, 8u8);
        let base_col = Rgb::new(30, 90, 180);
        // a solid-lit base beneath a Cut-blended vitals overlay (the readout preset's default blend).
        let base = LayerDef {
            pattern: "uniform".into(),
            spectrum: Spectrum::solid(base_col),
            ..Default::default()
        };
        let vitals = preset_by_slug("vitals").expect("vitals preset").to_layer();
        assert_eq!(vitals.blend, Blend::Cut, "a readout preset overlays as a cutout");

        // NO published data → the whole vitals layer is black; black is Cut's "nothing to say", so
        // the base shows THROUGH everywhere — not a black hole punched over the effect.
        crate::lighting::clear_vitals();
        let mut comp = Compositor::from_defs(&[base.clone(), vitals.clone()]);
        let out = comp.render(rows, cols, 0.0);
        assert!(out.iter().all(|&c| c == base_col), "no-data vitals overlay leaves the base intact");

        // WITH data → the LIT gauge cells land at TRUE colour (Cut replaces — battery red must
        // read red, never a screen-wash of red and base); the empty track passes the base through.
        crate::lighting::publish_vitals(crate::lighting::Vitals {
            battery_pct: 50, charging: false, active_stage: 0, stage_count: 1,
        });
        let mut comp = Compositor::from_defs(&[base.clone(), vitals.clone()]);
        let out = comp.render(rows, cols, 0.0);
        // 50% over 8 cols → the left 4 columns light at the gauge colour; the right 4 pass through.
        let lit = crate::lighting::battery_color(50);
        assert_ne!(lit, base_col, "a lit gauge cell must differ from the bare base");
        for row in 0..rows as usize {
            for col in 0..4usize {
                assert_eq!(out[row * cols as usize + col], lit, "lit gauge cell reads its true colour");
            }
            for col in 4..cols as usize {
                assert_eq!(out[row * cols as usize + col], base_col, "empty-track cell passes the base through");
            }
        }
        crate::lighting::clear_vitals();
    }

    #[test]
    fn vitals_readout_single_lit_column_and_crest() {
        use crate::lighting::{battery_color, Vitals as VSnap};
        // A rect exactly ONE column wide at 100% lights exactly one column (the lit == 1 path). Not
        // charging → the flat fill across the column's full height.
        let flat = render_vitals_bounds(
            VSnap { battery_pct: 100, charging: false, active_stage: 0, stage_count: 1 },
            3, 1, Bounds::board(3, 1), 0.0,
        );
        assert_eq!(flat.len(), 3);
        assert!(flat.iter().all(|&c| c == battery_color(100)), "the one column lights the full-height fill");
        // charging drives the crest lerp over that single lit column (the `lit.max(1)` crest path) → a
        // different, cyan-shifted colour, and crucially no panic when only one column is lit.
        let charging = render_vitals_bounds(
            VSnap { battery_pct: 100, charging: true, active_stage: 0, stage_count: 1 },
            3, 1, Bounds::board(3, 1), 0.0,
        );
        assert!(charging.iter().all(|&c| c != Rgb::BLACK), "the lit column stays lit while charging");
        assert!(charging != flat, "charging crest-shifts the single lit column");
    }

    #[test]
    fn vitals_readout_bounds_exceeding_the_board_are_clipped() {
        use crate::lighting::Vitals as VSnap;
        // A placement rect larger than / hanging off the board must CLIP, never index out of bounds.
        let (rows, cols) = (2u8, 3u8);
        let px = render_vitals_bounds(
            VSnap { battery_pct: 100, charging: true, active_stage: 0, stage_count: 1 },
            rows, cols,
            Bounds { row0: 1, col0: 2, rows: 9, cols: 9 }, // extends far past the 2×3 board
            0.4,
        );
        assert_eq!(px.len(), rows as usize * cols as usize, "output is always board-sized");
        // Only the single in-board cell of the oversized rect (row 1, col 2) can light; the rest of the
        // rect is clipped away. (100% over a 9-wide rect lights all 9 cols, but only col 2 exists here.)
        for i in 0..px.len() {
            let (r, c) = (i / cols as usize, i % cols as usize);
            if r == 1 && c == 2 {
                assert_ne!(px[i], Rgb::BLACK, "the one in-board cell of the oversized rect lights");
            } else {
                assert_eq!(px[i], Rgb::BLACK, "clipped / out-of-rect cells stay dark");
            }
        }
    }

    // ── pure stack / placement helpers ────────────────────────────────────────────────────────────

    #[test]
    fn selection_after_remove_tracks_the_selection() {
        // remove BELOW the selection → it decrements to stay on the same layer (len 3 → 2, sel 2 → 1).
        assert_eq!(selection_after_remove(0, 2, 2), 1, "remove below → decrement");
        // remove AT the selection (not the last) → index stays, now a different layer (len 3 → 2, sel 1).
        assert_eq!(selection_after_remove(1, 1, 2), 1, "remove at → clamp only");
        // remove the LAST, which was selected → clamp down to the new last (len 3 → 2, sel 2 → 1).
        assert_eq!(selection_after_remove(2, 2, 2), 1, "remove selected last → clamp to new last");
        // remove ABOVE the selection → nothing shifts (len 3 → 2, sel 0 stays 0).
        assert_eq!(selection_after_remove(2, 0, 2), 0, "remove above → unchanged");
        // remove-to-empty → no selection, 0.
        assert_eq!(selection_after_remove(0, 0, 0), 0, "emptied stack → 0");
    }

    #[test]
    fn region_from_rect_math() {
        // whole-board rect → EMPTY (the canonical region-less full board).
        assert!(region_from_rect(0, 0, 3, 5, 4, 6).is_empty(), "full board stores empty");
        // a single cell → exactly that row-major index.
        assert_eq!(region_from_rect(1, 2, 1, 2, 4, 6), vec![6 + 2]);
        // a rect (rows 1..=2, cols 2..=3) on a 4×6 board → the four enclosed cells, row-major.
        assert_eq!(region_from_rect(1, 2, 2, 3, 4, 6), vec![6 + 2, 6 + 3, 2 * 6 + 2, 2 * 6 + 3]);
        // out-of-order corners describe the SAME rect (the corners get ordered).
        assert_eq!(region_from_rect(2, 3, 1, 2, 4, 6), region_from_rect(1, 2, 2, 3, 4, 6));
        // corners off the board are CLAMPED in — a fully-overhanging rect clamps to the whole board → empty.
        assert!(region_from_rect(-5, -5, 99, 99, 4, 6).is_empty(), "clamps to the whole board → empty");
        // a partially-overhanging rect clamps to a sub-rect (rows 2..=3, cols 4..=5), not the full board.
        assert_eq!(region_from_rect(2, 4, 99, 99, 4, 6), vec![2 * 6 + 4, 2 * 6 + 5, 3 * 6 + 4, 3 * 6 + 5]);
        // a degenerate board → empty, no panic.
        assert!(region_from_rect(0, 0, 0, 0, 0, 0).is_empty());
    }
}
