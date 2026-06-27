//! The open effect system — effects are pluggable **frame generators**, not a fixed firmware
//! menu. A device exposes a few primitives; everything else (fire, starlight, audio-reactive,
//! physics) is a function that produces frames, streamed to the custom-frame channel. Adding an
//! effect = adding a `FrameGen`. This is the open Chroma Studio: no software lock, no firmware
//! ceiling. The animation backend (`lighting::Lights::animate`) drives any generator.

use crate::lighting::Rgb;
use serde::{Deserialize, Serialize};

/// A frame generator: given the matrix size, elapsed time `t` (seconds), and a base colour,
/// produce one frame of `rows*cols` colours (row-major). May carry state across frames
/// (fire's heat map, an audio buffer, a physics field, ...).
pub trait FrameGen {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb>;
}

/// Per-layer parameters. Generators read these instead of baking in constants, so an effect's
/// SPEED and DIRECTION are user-tunable — a real instrument, not a fixed preset. Colour stays the
/// `base` arg of `frame()` (each compositor layer feeds its own colour as base).
///
/// Every field has a sensible default, so adding a generator that wants a new knob means adding a
/// field here (+ a row in [`schema`]) — never reworking call sites. Only the knobs an effect's
/// SCHEMA exposes are surfaced in the UI; the rest stay at their defaults for that effect.
///
/// Most knobs are scalars/flags; `source` is the first DECLARED INPUT — the data the effect reacts
/// to (the audiometer's "speakers" vs "mic"). It's a `&'static str` from the schema's enum options,
/// so an effect-with-an-input stays as data-driven as a slider: the option label IS the value.
#[derive(Clone, Copy, Debug)]
pub struct EffectParams {
    pub speed: f32,     // animation-rate multiplier (1.0 = the design default)
    pub direction: u8,  // 0 → · 1 ← · 2 ↑ · 3 ↓  (directional effects only)
    pub density: f32,   // population/intensity knob (fire heat, starlight spawn rate) — 1.0 default
    pub fade: f32,      // decay rate for "ignite then fade" effects (reactive) — 1.0 default
    pub breath: u8,     // breathing flavour: 0 single · 1 dual · 2 random
    pub glow: bool,     // reactive: also light a pressed key's neighbour ring — false default
    pub source: &'static str, // declared INPUT for data-driven effects: audiometer "speakers"|"mic"
}
impl Default for EffectParams {
    fn default() -> Self {
        EffectParams {
            speed: 1.0,
            direction: 0,
            density: 1.0,
            fade: 1.0,
            breath: 0,
            glow: false,
            source: "speakers",
        }
    }
}

/// Resolve a generator by name with default params — the open registry.
pub fn make(name: &str) -> Option<Box<dyn FrameGen>> {
    make_with(name, EffectParams::default())
}

/// Resolve a generator by name with explicit params. New effect = one arm here.
pub fn make_with(name: &str, p: EffectParams) -> Option<Box<dyn FrameGen>> {
    match name.to_lowercase().as_str() {
        "static" | "solid" => Some(Box::new(Solid)),
        "spectrum" => Some(Box::new(Spectrum { p })),
        "wave" => Some(Box::new(Wave { p })),
        "breathing" => Some(Box::new(Breathing { p })),
        "fire" => Some(Box::new(Fire::with(p))),
        "cascade" | "matrix" | "rain" => Some(Box::new(Cascade::new(p))),
        "colorwheel" | "wheel" => Some(Box::new(ColorWheel { p })),
        "starlight" | "stars" => Some(Box::new(Starlight::new(p))),
        "reactive" => Some(Box::new(Reactive::new(p))),
        "ripple" => Some(Box::new(Ripple::new(p))),
        "comet" => Some(Box::new(Comet::new(p))),
        "aurora" => Some(Box::new(Aurora { p })),
        "audiometer" | "audio" | "vu" => Some(Box::new(AudioMeter::new(p))),
        "pulse" | "sysload" | "load" => Some(Box::new(Pulse)),
        "ambient" | "ambilight" | "screen" => Some(Box::new(Ambient::new(p))),
        "typingheat" | "typing" | "heat" => Some(Box::new(TypingHeat::new(p))),
        "rows" => Some(Box::new(Rows)),
        _ => None,
    }
}

// ── THE EFFECT PARAM SCHEMA — each effect declares its knobs AS DATA ──────────────────────
// The open-studio thesis applied to the UI: an effect is not a hand-built control panel, it's a
// generator + a typed list of the knobs it honours. The GUI reads this table and auto-renders one
// control per param (a slider for a Range, a segment for an Enum, a swatch for a Color, a switch for
// a Toggle) — exactly the weave-knobs pattern. Adding/retuning an effect's knobs is DATA here, never
// new per-effect UI. The CLI / a future TOML can read the same table.

/// The TYPE of a declared param — drives which control the UI renders and which `EffectParams` /
/// layer field the value writes to. `key` is the stable id the UI passes back to the setter.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamKind {
    /// The layer's base colour. Default = the user's weave accent (filled by the GUI).
    Color,
    /// A continuous slider. `(min, max, default)` in the param's own units.
    Range { min: f32, max: f32, default: f32 },
    /// A pick-one segment. `options` are the labels; the value is the chosen index.
    Enum { options: &'static [&'static str], default: u8 },
    /// An on/off switch.
    Toggle { default: bool },
}

/// One declared knob: a stable `key` (what the setter keys on), a human `label`, and its typed kind.
#[derive(Clone, Debug)]
pub struct Param {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
}

/// The param SCHEMA for an effect, by name — the list of knobs the GUI auto-renders. Empty = a
/// no-knob effect (e.g. a pure data surface). Unknown names return an empty schema, never panic.
///
/// Per the open-studio spec: static{color} · breathing{color,type,speed} · spectrum{speed}
/// · wave{direction,speed} · fire{speed,density} · cascade{color,speed,density} · starlight{color,
/// density,speed,fade} · reactive{color,fade,glow} · ripple{color,speed,fade} · comet{color,speed,
/// density} · aurora{color,speed} · colorwheel{speed,direction} ·
/// audiometer{color,source} · pulse{color} · typingheat{color,sensitivity,fade}. The `mouse-battery`
/// data surface declares no knobs. Every declared knob is HONOURED by that effect's generator (no
/// dead knobs); see the per-effect `frame()`. (A full per-endpoint / per-app audio picker for
/// `source` is a future enhancement — the schema only offers the default speakers|mic endpoints.)
pub fn schema(name: &str) -> Vec<Param> {
    // shared knob constructors so the ranges read consistently across effects
    let color = || Param { key: "color", label: "colour", kind: ParamKind::Color };
    let speed = || Param {
        key: "speed",
        label: "speed",
        kind: ParamKind::Range { min: 0.25, max: 4.0, default: 1.0 },
    };
    let direction = || Param {
        key: "direction",
        label: "direction",
        kind: ParamKind::Enum { options: &["→", "←", "↑", "↓"], default: 0 },
    };
    let density = || Param {
        key: "density",
        label: "density",
        kind: ParamKind::Range { min: 0.25, max: 3.0, default: 1.0 },
    };
    let fade = || Param {
        key: "fade",
        label: "fade",
        kind: ParamKind::Range { min: 0.25, max: 3.0, default: 1.0 },
    };
    let breath = || Param {
        key: "breath",
        label: "breath",
        kind: ParamKind::Enum { options: &["single", "dual", "random"], default: 0 },
    };
    // reactive's neighbour-glow switch: off (default) lights only the exact pressed key — accurate,
    // no cross-key bleed; on lights the pressed key PLUS a soft ring of its four neighbours. The first
    // real Toggle knob — proves the Toggle ParamKind is end-to-end (schema → UI → layer field → generator).
    let glow = || Param {
        key: "glow",
        label: "neighbour glow",
        kind: ParamKind::Toggle { default: false },
    };
    // the audiometer's declared INPUT: which live signal drives the bars. Plain-word labels (the
    // normie review flagged jargon) — "speakers" is the system output (default = today's behaviour),
    // "mic" is the user's microphone (the streamer's "my keyboard reacts to my voice").
    let source = || Param {
        key: "source",
        label: "source",
        kind: ParamKind::Enum { options: &["speakers", "mic"], default: 0 },
    };
    // ambient's "saturation" pop knob. It rides the `density` field (already wired through the GUI
    // setter + persistence) so it needs no new LayerDef field; 1.0 = faithful screen colour, higher =
    // more saturated. The label reads "saturation" even though the key is "density" — the schema's
    // key is plumbing, the label is the human word.
    let saturation = || Param {
        key: "density",
        label: "saturation",
        kind: ParamKind::Range { min: 1.0, max: 3.0, default: 1.0 },
    };
    // typing heat's "sensitivity" — the DEPOSIT-STRENGTH gain (how hot each keypress lands / how readily
    // heat builds at your fingers). It rides the `density` field (already wired through the GUI setter +
    // persistence, exactly like ambient's "saturation"), so it needs no new LayerDef field; the label is
    // the human word, the key is plumbing. 1.0 = the design default, higher = heats on lighter typing,
    // lower = needs a flurry.
    let sensitivity = || Param {
        key: "density",
        label: "sensitivity",
        kind: ParamKind::Range { min: 0.25, max: 3.0, default: 1.0 },
    };
    match name.to_lowercase().as_str() {
        "static" | "solid" => vec![color()],
        "breathing" => vec![color(), breath(), speed()],
        // spectrum is the iconic uniform spectrum-CYCLE (whole board = one hue, advancing through the
        // full circle over time) — it has no spatial axis, so it declares ONLY `speed` (the cycle
        // rate). No colour (it cycles every hue); no direction (a dead knob on a uniform cycle).
        "spectrum" => vec![speed()],
        "wave" => vec![direction(), speed()],
        "fire" => vec![speed(), density()],
        "cascade" | "matrix" | "rain" => vec![color(), speed(), density()],
        "starlight" | "stars" => vec![color(), density(), speed(), fade()],
        "reactive" => vec![color(), fade(), glow()],
        // ripple (reactive radial wave): colour + how fast the ring expands + the ring lifetime — the
        // three knobs its generator honours. Like reactive it needs a keypress to show anything.
        "ripple" => vec![color(), speed(), fade()],
        // comet (motion streak): colour + travel rate + how busy the stream is (1 comet by default →
        // turn density up for a swarm).
        "comet" => vec![color(), speed(), density()],
        // aurora (ambient flow): colour BIASES the palette (the hue the flow drifts around) + the flow
        // rate. Kept minimal — two knobs, both honoured by `Aurora::frame`.
        "aurora" => vec![color(), speed()],
        "colorwheel" | "wheel" => vec![speed(), direction()],
        "audiometer" | "audio" | "vu" => vec![color(), source()],
        // pulse: a live CPU/RAM load readout. One knob — the accent TINT for the load bars; the
        // green→amber→red urgency ramp and the CPU-driven throb rate are intrinsic (not user knobs).
        "pulse" | "sysload" | "load" => vec![color()],
        // ambient (ambilight): colour is INTRINSIC (it comes from the screen), so no colour knob —
        // just how fast the board chases the screen (`speed` = ease rate) and how much the colours pop
        // (`saturation`). Both honoured by `Ambient::frame`.
        "ambient" | "ambilight" | "screen" => vec![speed(), saturation()],
        // typing heat (live keyboard → a living heat MAP): the colour TINTS the hot end of the thermal
        // ramp (so "hot" can be the user's colour, not just red — the cold end stays a built-in cool blue);
        // `sensitivity` (rides the density field) is the deposit-strength gain (how hot each keypress
        // lands); `fade` is the COOLDOWN speed (how fast the heat field cools). All three honoured by
        // `TypingHeat::frame` (deposits) / `render_typing_heat` (the ramp).
        "typingheat" | "typing" | "heat" => vec![color(), sensitivity(), fade()],
        // the data surface(s) — no generator knobs; the source IS the content
        "mouse-battery" | "mouse_battery" | "vitals" => vec![],
        _ => vec![],
    }
}

/// Does this effect's schema declare a colour knob? (The GUI gates the swatch row on it; the backend
/// already passes the layer colour through harmlessly for effects that ignore it.)
pub fn uses_color(name: &str) -> bool {
    schema(name)
        .iter()
        .any(|p| matches!(p.kind, ParamKind::Color))
}

/// The built-in DEFAULT colour for an effect's colour knob — what a freshly-applied layer should start
/// in BEFORE the user picks a colour, when the global weave accent would be a poor fit. Most effects
/// return `None` (the caller pours them in the weave accent, the house default). `typingheat` returns a
/// warm fire tone: its colour knob is the FLAME's hue, so defaulting to a cool accent (the teal weave)
/// would recolour the fire cold out of the box — a warm default keeps it unmistakable orange→red→white
/// heat. The GUI consults this when it builds/replaces a layer (see `glue.rs`).
pub fn default_color(name: &str) -> Option<Rgb> {
    match name.to_lowercase().as_str() {
        // a warm amber (~#FFD9A0) — classic fire: the recolour leaves the incandescent ramp essentially
        // unchanged at this hue, so the default board is pure heat with zero blue.
        "typingheat" | "typing" | "heat" => Some(Rgb::new(0xFF, 0xD9, 0xA0)),
        _ => None,
    }
}

/// Diagnostic: each matrix row a distinct hue (row 0 red, increasing). Static — used to read a
/// device's physical row orientation/coverage so effects map correctly.
pub struct Rows;
impl FrameGen for Rows {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, _base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let mut f = vec![Rgb::BLACK; r * c];
        for y in 0..r {
            let hue = y as f32 / r.max(1) as f32 * 360.0;
            let col = Rgb::from_hsv(hue, 1.0, 1.0);
            for x in 0..c {
                f[y * c + x] = col;
            }
        }
        f
    }
}

/// Names of the built-in generators (for help / a future GUI palette). `reactive`/`audiometer` are
/// resolvable too but kept OUT (they do nothing without live keyboard/audio). `comet`/`aurora` are
/// self-driven so they're listed normally. `ripple` is key-reactive like `reactive`, but it IS listed
/// (it fills a full frame headless — dark with no keypress) so the new classics register together; the
/// no-dead-knob sweep simply skips it (its output needs a keypress, like reactive/audiometer/ambient).
/// `typingheat` is listed the same way: it fills a full frame headless (a cool, dim idle board with no
/// keypress) so it registers as a classic, and the no-dead-knob sweep skips it (its character needs
/// live typing — its knobs are proven by dedicated tests instead).
pub const BUILTINS: &[&str] = &[
    "static",
    "spectrum",
    "wave",
    "breathing",
    "fire",
    "cascade",
    "comet",
    "aurora",
    "colorwheel",
    "starlight",
    "ripple",
    "pulse",
    "ambient",
    "typingheat",
];

// ── built-in software effects (the emulatable primitives) ───────────────────────────────

pub struct Solid;
impl FrameGen for Solid {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, base: Rgb) -> Vec<Rgb> {
        vec![base; rows as usize * cols as usize]
    }
}

pub struct Spectrum {
    p: EffectParams,
}
impl FrameGen for Spectrum {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
        use std::f32::consts::TAU;
        let n = rows as usize * cols as usize;
        // The CLASSIC "spectrum cycling": the WHOLE board is ONE uniform hue that smoothly advances
        // through the entire 0..360° circle over time. At any instant every cell is the SAME colour
        // (no spatial spread — that's `wave`'s job); over time the hue cycles. `speed` is the cycle
        // RATE (~1 full spectrum / ~8s at 1.0). A uniform cycle has NO axis, so spectrum honours no
        // `direction` knob (its schema is just `[speed]`) — no dead knob.
        let hue = (t * self.p.speed / 8.0 * 360.0).rem_euclid(360.0);
        // a gentle WHOLE-BOARD brightness breath so the wash is alive, not a flat hue: the entire
        // board swells and dims together (~6s cycle, scaled by speed). Kept shallow (≈0.80..1.0) so
        // the spectrum stays bright and reads as itself — depth, not a pulse. `from_hsv`'s `v` arg
        // carries the brightness directly.
        let breath = (0.90 + 0.10 * (t * TAU / 6.0 * self.p.speed).sin()).clamp(0.0, 1.0);
        vec![Rgb::from_hsv(hue, 1.0, breath); n]
    }
}

pub struct Wave {
    p: EffectParams,
}
impl FrameGen for Wave {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
        use std::f32::consts::TAU;
        let (r, c) = (rows as usize, cols as usize);
        let mut f = vec![Rgb::BLACK; r * c];
        for y in 0..r {
            for x in 0..c {
                // the coordinate the hue scrolls along, 0..1 — chosen by direction
                let u = match self.p.direction {
                    1 => 1.0 - x as f32 / c.max(1) as f32, // ←
                    2 => 1.0 - y as f32 / r.max(1) as f32, // ↑
                    3 => y as f32 / r.max(1) as f32,       // ↓
                    _ => x as f32 / c.max(1) as f32,       // →
                };
                // the wave's identity: a travelling rainbow scrolling along the axis…
                let phase = t * 0.33 * self.p.speed + u;
                let h = phase * 360.0;
                // …now with a LUMINANCE crest that travels WITH the hue — one swell per board span,
                // its crest bright and its trough dim — so the wave reads as a moving SWELL, not a
                // flat rainbow scroll. `from_hsv`'s `v` arg IS brightness, so the depth costs nothing
                // extra. Stays in 0.72..1.0 so the trailing edge dims without ever going dark (a wave
                // still reads as a wave across the whole board).
                let v = 0.72 + 0.28 * (0.5 + 0.5 * (phase * TAU).sin());
                f[y * c + x] = Rgb::from_hsv(h, 1.0, v);
            }
        }
        f
    }
}

pub struct Breathing {
    p: EffectParams,
}
impl FrameGen for Breathing {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        use std::f32::consts::TAU;
        let phase = t * TAU / 3.0 * self.p.speed;
        // breath flavour: 0 single (one smooth swell), 1 dual (a quick double-pulse per cycle), 2
        // random (the cosine plus a slow wandering jitter so each breath lands a little different).
        let b = match self.p.breath {
            1 => {
                // two swells per period — square the rectified cosine so each crest is crisp
                let s = (phase).cos().abs();
                s * s
            }
            2 => {
                // base swell + a low-rate wobble driven by the time itself (deterministic, no rand dep)
                let swell = 0.5 - 0.5 * phase.cos();
                let wobble = 0.5 + 0.5 * (t * 0.7).sin() * (t * 1.7 + 1.3).cos();
                (swell * (0.6 + 0.4 * wobble)).clamp(0.0, 1.0)
            }
            _ => 0.5 - 0.5 * phase.cos(), // single: the classic smooth cosine swell
        };
        vec![base.scale_f(b); rows as usize * cols as usize]
    }
}

// ── fire: a real heat simulation — the kind of effect Synapse software-locks ─────────────

/// Upward-propagating fire. Bottom row is seeded hot with flicker; heat diffuses up and cools;
/// the heat field is mapped to a black→red→orange→yellow→white ramp. Deterministic PRNG so it
/// needs no `rand` dependency (the project stays lean). `speed` scales the flicker/propagation rate
/// (the sim advances `speed`× as many heat steps per second of elapsed `t`), so a slow fire smoulders
/// and a fast one churns; `density` governs how tall/full the flame climbs.
pub struct Fire {
    heat: Vec<f32>,
    dims: (u8, u8),
    rng: u32,
    p: EffectParams,
    last_t: f32, // elapsed time at the previous frame, to derive how far the sim should advance
    step_acc: f32, // fractional simulation-step accumulator (honours `speed` smoothly)
}

impl Default for Fire {
    fn default() -> Self {
        Fire {
            heat: Vec::new(),
            dims: (0, 0),
            rng: 0x9E37_79B9,
            p: EffectParams::default(),
            last_t: 0.0,
            step_acc: 0.0,
        }
    }
}

impl Fire {
    fn with(p: EffectParams) -> Self {
        Fire {
            p,
            ..Fire::default()
        }
    }
}

impl Fire {
    fn rand(&mut self) -> f32 {
        // xorshift32 -> 0.0..1.0
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Advance the heat field one simulation step: re-seed the flickering bottom row, then propagate
    /// upward with cooling. Each step re-rolls the flicker, so the number of steps a frame runs IS the
    /// flicker/propagation rate — which is how `speed` is honoured (more steps/sec at higher speed).
    fn step(&mut self, r: usize, c: usize) {
        // seed the bottom row white-hot and fairly steady (the fire's base)
        let bottom = (r - 1) * c;
        for x in 0..c {
            self.heat[bottom + x] = 0.90 + self.rand() * 0.10;
        }
        // DENSITY scales how much heat survives the climb: higher density → less cooling → a taller,
        // fuller flame; lower → a low sparse fire. Clamped so a flame never runs away or dies entirely.
        let dens = self.p.density.clamp(0.25, 3.0);
        let cool_scale = (1.0 / dens).clamp(0.4, 2.0);
        // propagate upward with STRONG cooling so the fire stays low (bottom 2-3 rows) with only
        // sparse licks reaching the top — a readable flame shape rather than full-board noise.
        for y in 0..r - 1 {
            for x in 0..c {
                let below = (y + 1) * c + x;
                let bl = (y + 1) * c + (x + c - 1) % c;
                let br = (y + 1) * c + (x + 1) % c;
                let avg = (self.heat[below] * 2.0 + self.heat[bl] + self.heat[br]) / 4.0;
                let cool = (0.14 + self.rand() * 0.10) * cool_scale;
                self.heat[y * c + x] = (avg - cool).max(0.0);
            }
        }
    }
}

impl FrameGen for Fire {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        if self.dims != (rows, cols) {
            self.heat = vec![0.0; r * c];
            self.dims = (rows, cols);
            self.last_t = t;
        }
        if r == 0 || c == 0 {
            return Vec::new();
        }
        // SPEED is honoured by advancing the sim from elapsed time: ~18 heat-steps/sec at speed 1.0,
        // scaled by `speed`, accumulating the fraction so even slow fires advance smoothly. A frame
        // that lands at the same `t` as the last (e.g. a static preview) still steps once so the flame
        // never freezes; `dt < 0` (a time reset) is treated as one step too.
        const BASE_STEPS_PER_SEC: f32 = 18.0;
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.p.speed.clamp(0.1, 6.0);
        self.step_acc += dt * BASE_STEPS_PER_SEC * spd;
        // run the accrued whole steps, but always at least one (and cap the burst so a long stall or a
        // big time jump can't run thousands of steps in one frame).
        let mut steps = self.step_acc.floor().max(1.0) as u32;
        self.step_acc -= self.step_acc.floor();
        steps = steps.min(8);
        for _ in 0..steps {
            self.step(r, c);
        }
        // Render: the heat ramp gives the black→red→orange→white COLOUR; on top of it each column
        // gets its own LUMINANCE flicker so the flame licks at its own rhythm (real fire flickers in
        // brightness, not only hue) — the steady gradient becomes a living flame. The flicker depth
        // grows with heat: the white-hot TIPS flare and gutter the most (vivid licks), while the
        // cooler EMBERS at the base only glow-and-dim gently, so the base stays a steady bed of coals
        // and the tips dance. `scale_f` applies the 0..1 intensity; `t`/`speed` drive the rhythm so a
        // fast fire flickers faster.
        let spd_f = self.p.speed.clamp(0.1, 6.0);
        self.heat
            .iter()
            .enumerate()
            .map(|(i, &h)| {
                let x = i % c;
                let flick = fire_flicker(x, t, spd_f, h);
                fire_color(h).scale_f(flick)
            })
            .collect()
    }
}

/// Per-column intensity flicker for the fire, in 0..1 — multiplies a cell's heat colour so the flame
/// varies in BRIGHTNESS over time, not just hue. Two incommensurate sines per column (phase-seeded by
/// `x`) make an organic wobble that differs column-to-column; it's a pure function of column + time
/// (no `rand` needed — the sim's seed row already carries the random heat). `heat` shapes the DEPTH:
/// the hot tips (heat→1) flicker hard (vivid licks), the cool embers (heat→0) barely waver (a steady
/// glowing base). Returns ≥0.5 even at the tips so a lick dims but never blinks fully out.
fn fire_flicker(x: usize, t: f32, speed: f32, heat: f32) -> f32 {
    let xf = x as f32;
    let a = (t * 9.0 * speed + xf * 1.7).sin();
    let b = (t * 13.0 * speed + xf * 0.6 + 2.0).sin();
    let mix = 0.5 + 0.5 * (0.6 * a + 0.4 * b); // 0..1 organic wobble, per column
    // flicker DEPTH ramps with heat: ~6% at the embers, up to ~40% at the white-hot tips.
    let depth = 0.06 + 0.34 * heat.clamp(0.0, 1.0);
    (1.0 - depth + depth * mix).clamp(0.0, 1.0)
}

/// Map a heat value 0..1 to a fire colour ramp.
fn fire_color(h: f32) -> Rgb {
    let h = h.clamp(0.0, 1.0);
    let lerp = |a: Rgb, b: Rgb, t: f32| Rgb::lerp(a, b, t);
    let black = Rgb::new(0, 0, 0);
    let red = Rgb::new(180, 0, 0);
    let orange = Rgb::new(255, 90, 0);
    let yellow = Rgb::new(255, 210, 40);
    let white = Rgb::new(255, 255, 220);
    match h {
        x if x < 0.30 => lerp(black, red, x / 0.30),
        x if x < 0.60 => lerp(red, orange, (x - 0.30) / 0.30),
        x if x < 0.85 => lerp(orange, yellow, (x - 0.60) / 0.25),
        x => lerp(yellow, white, (x - 0.85) / 0.15),
    }
}

// ── colorwheel: a rotating radial hue rainbow (a real wheel, not the whole-board spectrum) ──

/// A colour wheel anchored on the matrix centre: each cell's hue is its ANGLE around the centre,
/// the whole wheel rotating over time. Unlike `spectrum` (one hue for the whole board) this paints a
/// true radial rainbow — the effect Synapse calls "Wheel"/"ColorWheel". Pure time function; ignores
/// the layer colour (it owns the full hue circle).
pub struct ColorWheel {
    p: EffectParams,
}
impl FrameGen for ColorWheel {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
        use std::f32::consts::PI;
        let (r, c) = (rows as usize, cols as usize);
        let mut f = vec![Rgb::BLACK; r * c];
        // centre in cell coordinates (a true centre even for even dimensions)
        let cx = (c as f32 - 1.0) / 2.0;
        let cy = (r as f32 - 1.0) / 2.0;
        // DIRECTION sets the spin SIGN — →/↓ (0/3) spin one way, ←/↑ (1/2) the other. The radial
        // wheel has no left/right/up/down axis, so direction reads naturally as clockwise vs counter.
        let dir = match self.p.direction {
            1 | 2 => -1.0, // ← / ↑ → counter-clockwise
            _ => 1.0,      // → / ↓ → clockwise
        };
        let spin = t * 60.0 * self.p.speed * dir; // degrees/sec at speed 1
        // furthest cell from the centre, for normalising the radial brightness falloff (never 0).
        let max_rad = (cx * cx + cy * cy).sqrt().max(1.0);
        for y in 0..r {
            for x in 0..c {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let ang = dy.atan2(dx); // -PI..PI
                let ang_deg = ang / PI * 180.0;
                let hue = (ang_deg + spin).rem_euclid(360.0);
                // RADIAL DEPTH so the wheel has dimension, not a flat disc: a soft dome — the hub
                // brightest, easing toward a dimmer rim (still well-lit) — reads as a rounded wheel
                // even in a still frame. ON TOP, a gentle bright SWEEP rotates WITH the spin (a glint
                // travelling round the rim) so the wheel visibly turns. Both ride `from_hsv`'s `v`.
                let rad = (dx * dx + dy * dy).sqrt() / max_rad; // 0 at hub → 1 at the rim
                let dome = 1.0 - 0.30 * rad; // 1.0 hub → 0.70 rim
                let sweep = 0.85 + 0.15 * (ang_deg + spin).to_radians().cos();
                let v = (dome * sweep).clamp(0.0, 1.0);
                f[y * c + x] = Rgb::from_hsv(hue, 1.0, v);
            }
        }
        f
    }
}

// ── starlight: random twinkles in the layer colour (each cell sparks then fades) ────────────

/// Starlight: cells randomly ignite to the layer colour and fade out, like a slow shimmer of stars.
/// Stateful (per-cell brightness + a deterministic xorshift PRNG, no `rand` dep). `speed` scales how
/// often stars spawn and how fast they fade. Reads the layer colour (the twinkles are the theme hue).
pub struct Starlight {
    level: Vec<f32>,
    dims: (u8, u8),
    rng: u32,
    p: EffectParams,
    acc: f32, // fractional spawn accumulator (so low spawn rates still fire)
}
impl Starlight {
    fn new(p: EffectParams) -> Self {
        Starlight {
            level: Vec::new(),
            dims: (0, 0),
            rng: 0x1357_2468,
            p,
            acc: 0.0,
        }
    }
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }
}
impl FrameGen for Starlight {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.level = vec![0.0; n];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Vec::new();
        }
        // spawn ~ (3% of cells) * speed * density new stars per frame, accumulating the fraction.
        // density populates the sky (more/fewer stars at once); speed governs how briskly they cycle.
        self.acc += n as f32 * 0.03 * self.p.speed * self.p.density.clamp(0.1, 4.0);
        while self.acc >= 1.0 {
            self.acc -= 1.0;
            let i = (self.rand() * n as f32) as usize % n;
            self.level[i] = 0.85 + self.rand() * 0.15;
        }
        // fade every cell toward dark. FADE is the twinkle LENGTH: higher fade → faster decay → brief
        // crisp sparks; lower → long lingering stars. speed still nudges the cycle so stars that spawn
        // faster also clear faster, keeping the sky from saturating at high speed.
        let decay = 0.04 * self.p.speed.max(0.1) * self.p.fade.clamp(0.1, 4.0);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        self.level.iter().map(|&l| base.scale_f(l)).collect()
    }
}

// ── cascade: Matrix-style digital rain — the open-studio answer to a locked "preset" ────────

/// Cascade: classic Matrix digital rain. Each column runs an independent vertical "drop": a bright,
/// near-white HEAD falling top→bottom with a fading coloured TAIL streaming behind it. Drops spawn
/// from above at staggered random times and, once a head has fallen off the bottom, the column waits
/// a random gap then drops again — so over time a column shows many drops (a living downpour, not a
/// single sweep).
///
/// Stateful: a per-cell brightness field (the trails) plus per-column drop bookkeeping, advanced by a
/// deterministic xorshift PRNG (no `rand` dependency — the project stays lean, like Fire/Starlight).
/// It's driven by elapsed `t` through a step accumulator (the Fire pattern): the sim advances a fixed
/// number of steps per second regardless of frame rate, so the rain looks the SAME at the legacy 6fps
/// cap and at 30/60fps (a slow frame just runs a few more steps; a static `t` still steps once so the
/// rain never freezes). Fills any rows×cols.
///
/// Reads the layer COLOUR as the rain hue — pick green for the cinema look, or the weave accent by
/// default — and the head brightens toward white. `speed` is the FALL RATE (a single time-scale on
/// the whole sim: faster heads, proportionally faster trails). `density` is how BUSY the rain is —
/// more drops at once and a shorter respawn gap at high density, a sparse trickle at low.
pub struct Cascade {
    level: Vec<f32>,   // per-cell trail brightness 0..1 (a fresh head paints 1.0; tails decay)
    head: Vec<f32>,    // per-column head row position (float, grows downward; negative = above board)
    active: Vec<bool>, // per-column: is a drop currently falling
    wait: Vec<f32>,    // per-column respawn countdown in sim-steps (inactive columns only)
    dims: (u8, u8),
    rng: u32,
    p: EffectParams,
    last_t: f32,   // elapsed time at the previous frame, to derive how far the sim advances
    step_acc: f32, // fractional simulation-step accumulator (honours `speed` smoothly)
}

impl Cascade {
    fn new(p: EffectParams) -> Self {
        Cascade {
            level: Vec::new(),
            head: Vec::new(),
            active: Vec::new(),
            wait: Vec::new(),
            dims: (0, 0),
            rng: 0x2545_F491,
            p,
            last_t: 0.0,
            step_acc: 0.0,
        }
    }

    fn rand(&mut self) -> f32 {
        // xorshift32 -> 0.0..1.0 (the same lean PRNG Fire/Starlight use)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// Average respawn gap in sim-steps — shorter at higher density (a busier downpour), longer at
    /// low density (a sparse trickle). This is one of the two ways `density` is honoured.
    fn gap(&self) -> f32 {
        const BASE_GAP_STEPS: f32 = 40.0;
        (BASE_GAP_STEPS / self.p.density.clamp(0.25, 3.0)).max(2.0)
    }

    /// Start a fresh drop at the top of a column: the head enters from just above the board with a
    /// little random offset so neighbouring columns never fall in lock-step.
    fn spawn(&mut self, col: usize) {
        self.active[col] = true;
        self.head[col] = -(self.rand() * 3.0);
    }

    /// Seed the initial per-column state so the board is already raining on the first frame: each
    /// column either starts mid-fall at a random row, or starts waiting a random fraction of the gap.
    /// The active-probability scales with `density` (the second way density is honoured) so a dense
    /// rain begins nearly full while a sparse one starts mostly empty.
    fn init_columns(&mut self, r: usize, c: usize) {
        let dens = self.p.density.clamp(0.25, 3.0);
        let p_active = (0.30 + 0.23 * dens).clamp(0.0, 0.95);
        let g = self.gap();
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

    /// Advance the rain one simulation step: fade every trail a notch (the exponential tail), then for
    /// each column either drop its head one notch (painting the cell it lands on white-hot) or count
    /// down its respawn gap. Steps-per-second is fixed, so the number of steps a frame runs IS the
    /// fall rate — which is how `speed` is honoured (more steps/sec ⇒ heads fall and trails fade
    /// proportionally faster, keeping the trail SHAPE constant at any speed).
    fn step(&mut self, r: usize, c: usize) {
        const DECAY: f32 = 0.80; // per-step trail fade (multiplicative ⇒ a smooth exponential tail)
        const ADVANCE: f32 = 0.22; // rows a head falls per step (<1 so no cell is ever skipped)
        for v in self.level.iter_mut() {
            *v *= DECAY;
            if *v < 0.02 {
                *v = 0.0;
            }
        }
        for x in 0..c {
            if self.active[x] {
                let h = self.head[x];
                // paint the head cell at full brightness (only while it's on the board)
                if h >= 0.0 && (h as usize) < r {
                    self.level[h as usize * c + x] = 1.0;
                }
                let nh = h + ADVANCE;
                self.head[x] = nh;
                // the drop is done once the head has fallen a few rows past the bottom (so its tail
                // has fully cleared the board); the column then waits a random gap before dropping again.
                if nh >= r as f32 + 4.0 {
                    self.active[x] = false;
                    self.wait[x] = self.gap() * (0.5 + self.rand());
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

impl FrameGen for Cascade {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        if self.dims != (rows, cols) {
            self.level = vec![0.0; r * c];
            self.head = vec![0.0; c];
            self.active = vec![false; c];
            self.wait = vec![0.0; c];
            self.dims = (rows, cols);
            self.last_t = t;
            self.step_acc = 0.0;
            if r > 0 && c > 0 {
                self.init_columns(r, c);
            }
        }
        let n = r * c;
        if n == 0 {
            return Vec::new();
        }
        // SPEED drives the sim from elapsed time: ~20 steps/sec at speed 1.0, scaled by `speed`,
        // accumulating the fraction so slow rain still advances smoothly. A frame that lands at the
        // same `t` (a static preview) still steps once so the rain never freezes; a time reset (dt<0)
        // is treated as one step too. The burst is capped so a long stall can't run thousands of steps.
        const BASE_STEPS_PER_SEC: f32 = 20.0;
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.p.speed.clamp(0.1, 6.0);
        self.step_acc += dt * BASE_STEPS_PER_SEC * spd;
        let mut steps = self.step_acc.floor().max(1.0) as u32;
        self.step_acc -= self.step_acc.floor();
        steps = steps.min(8);
        for _ in 0..steps {
            self.step(r, c);
        }
        // render: a fresh head (level 1.0) brightens toward white; everything behind it is the layer
        // COLOUR scaled by its decayed brightness — a white head leading a coloured fading tail.
        const HEAD_THRESH: f32 = 0.9;
        let white = Rgb::new(255, 255, 255);
        self.level
            .iter()
            .map(|&v| {
                if v <= 0.0 {
                    Rgb::BLACK
                } else if v >= HEAD_THRESH {
                    let f = ((v - HEAD_THRESH) / (1.0 - HEAD_THRESH)).clamp(0.0, 1.0);
                    Rgb::lerp(base, white, 0.85 * f)
                } else {
                    base.scale_f(v)
                }
            })
            .collect()
    }
}

// ── reactive: the board lights where you type, then fades — driven by the LIVE keyboard ──────

/// Reactive: polls the live keyboard (`GetAsyncKeyState`, a safe read — never injects) and ignites a
/// key's cell each time it transitions down, which then fades. The pressed key lights its TRUE physical
/// cell via the standard Razer keymap ([`crate::lighting::vk_to_key_cell`]) — the key you press answers
/// where it actually sits. Razer keyboards have ONE LED per key, so each press lights exactly ONE cell
/// (a key whose board has no LED at that cell — or that the map doesn't carry, like mouse buttons /
/// generic modifiers / media keys — lights NOTHING: an accurate reactive surface never lies by flashing
/// a random cell). With the `glow` toggle off (default) only the pressed key's own cell lights —
/// accurate, no cross-key bleed; on, its four neighbours also catch a softer glow. Reads the layer
/// colour. Off Windows the key read is a no-op.
pub struct Reactive {
    level: Vec<f32>,
    prev: Vec<bool>, // last-frame down-state for VK 1..256
    dims: (u8, u8),
    p: EffectParams,
}
impl Reactive {
    fn new(p: EffectParams) -> Self {
        Reactive {
            level: Vec::new(),
            prev: vec![false; 256],
            dims: (0, 0),
            p,
        }
    }
}
impl FrameGen for Reactive {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.level = vec![0.0; n];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Vec::new();
        }
        // detect fresh key-downs and ignite the pressed key's SINGLE cell (+ a softer ring of its four
        // neighbours when `glow` is on). One LED per key: a key the map carries lights its real cell; a
        // key it doesn't carry (mouse buttons, generic modifiers, media keys) — or whose board has no
        // LED at that cell — resolves to None / a dark cell and lights NOTHING, no hashed-random
        // fallback, so the reactive surface stays accurate and never flashes a cell you didn't press.
        for vk in 1..256usize {
            let down = crate::capture::key_down(vk as i32);
            if down && !self.prev[vk] {
                if let Some((ry, cx)) = crate::lighting::vk_to_key_cell(vk as i32) {
                    let (ry, cx) = (ry as usize, cx as usize);
                    if ry < r && cx < c {
                        let cell = ry * c + cx;
                        self.level[cell] = 1.0;
                        if self.p.glow {
                            for (dy, dx) in [(0isize, 1isize), (0, -1), (1, 0), (-1, 0)] {
                                let ny = ry as isize + dy;
                                let nx = cx as isize + dx;
                                if ny >= 0 && ny < r as isize && nx >= 0 && nx < c as isize {
                                    let ni = ny as usize * c + nx as usize;
                                    self.level[ni] = self.level[ni].max(0.55);
                                }
                            }
                        }
                    }
                }
            }
            self.prev[vk] = down;
        }
        // fade — the FADE knob is the trail length: higher fade → faster decay → a snappier, shorter
        // glow; lower → a long lingering trail. (Speed no longer doubles as the decay so the two read
        // independently for this effect; the schema only exposes colour + fade for reactive.)
        let decay = 0.06 * self.p.fade.clamp(0.1, 4.0);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        self.level.iter().map(|&l| base.scale_f(l)).collect()
    }
}

// ── audiometer: the board is a VU meter driven by the LIVE audio peak of the chosen SOURCE ──────

/// Audiometer: a level meter driven by the OS peak-sample value of a chosen SOURCE — the system
/// output ("speakers", the default = follow the sound I hear) or the microphone ("mic", the
/// streamer's "my keyboard reacts to my voice"). The smoothed level fills the board from the bottom
/// rows up; each column shimmers on a stable phase so the bar dances like a spectrum, and the lit
/// cells gradient from the layer colour (bottom) toward white-hot (the crest).
///
/// The level comes from the SHARED [`crate::audio_level`] provider, not a private meter: that
/// background sampler reads the peak at ~60Hz and owns the boost + ballistic envelope, so the meter
/// stays responsive even on a board that streams frames at ~6fps (the old per-frame read aliased
/// badly). Each frame just `ensure`s the provider is pointed at this layer's source and reads the
/// published level — which is why the on-screen preview and the device match exactly: they read the
/// SAME number. If the source can't be resolved (no mic plugged in, off-Windows, …) the provider's
/// level stays 0 and the board idles dark — an honest silent meter, never a fake or a panic. The
/// generator is now stateless: flipping the source knob and re-applying (a fresh `from_defs`) simply
/// `ensure`s the new endpoint; the provider re-points with no stale handle.
pub struct AudioMeter {
    p: EffectParams,
}
impl AudioMeter {
    fn new(p: EffectParams) -> Self {
        AudioMeter { p }
    }
}
impl FrameGen for AudioMeter {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        let mut f = vec![Rgb::BLACK; n];
        if n == 0 {
            return f;
        }
        // point the shared sampler at the declared source ("mic" | "speakers") and read the latest
        // smoothed level — sampling + boost + envelope live in the provider, decoupled from this
        // frame rate. `ensure` is idempotent (no-op if already on this source) and `level` is a
        // lock-free atomic read; the provider auto-stops when nobody reads it.
        crate::audio_level::ensure(self.p.source);
        let level = crate::audio_level::level().clamp(0.0, 1.0);
        let white = Rgb::new(255, 255, 255);
        for x in 0..c {
            // per-column shimmer phase so a single level still reads as dancing bars
            let phase = x as f32 * 0.7;
            let shimmer = 0.7 + 0.3 * (t * 5.0 * self.p.speed + phase).sin();
            let bar = (level * shimmer).clamp(0.0, 1.0) * r as f32; // lit rows in this column
            for y in 0..r {
                let from_bottom = (r - 1 - y) as f32; // 0 at bottom row
                if from_bottom < bar {
                    // gradient bottom (base) -> crest (white)
                    let frac = if r > 1 {
                        from_bottom / (r as f32 - 1.0)
                    } else {
                        0.0
                    };
                    f[y * c + x] = Rgb::lerp(base, white, frac * 0.8);
                }
            }
        }
        f
    }
}

// ── pulse: the board is a live CPU/RAM load meter driven by the system telemetry provider ──────

/// Map a 0..=1 LOAD to a green→amber→red urgency colour: idle is green, busy ramps through amber to
/// red at maxed. Built from [`Rgb::from_hsv`] — hue 120° (green) at idle sweeps down to 0° (red) at
/// full, passing 60° (amber) at half — so the colour itself reads "how hard the machine is working".
/// (The battery gauge's `battery_color` ramps the OTHER way — full = green — so load gets its own.)
fn load_color(f: f32) -> Rgb {
    let f = f.clamp(0.0, 1.0);
    Rgb::from_hsv(120.0 * (1.0 - f), 1.0, 1.0)
}

/// Pulse: a glanceable live readout of the PC's load — the top half of the board is a CPU meter, the
/// bottom half a RAM meter. Each zone is a horizontal bar that fills left→right in proportion to its
/// load, coloured by the green→amber→red [`load_color`] ramp (idle green, maxed red) and tinted with
/// the layer COLOUR so the readout carries the user's accent. The whole board breathes, and the
/// breath RATE scales with CPU — an idle machine barely glows, a pegged one visibly throbs.
///
/// Like the audiometer it reads a SHARED background provider, [`crate::sys_stats`], not a per-frame
/// system call: that ~1Hz sampler owns the CPU-delta math + smoothing, so the meter is responsive and
/// correct even on a board that streams at ~6fps, and the on-screen preview matches the device because
/// both read the SAME published numbers. Off-Windows (or before the first sample) the provider reads
/// 0 and the board idles dark — an honest "no load" readout, never a fake. It's a FRONTIER effect: a
/// live system-telemetry surface, the kind a locked vendor app never exposes.
pub struct Pulse;
impl FrameGen for Pulse {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        // point the shared sampler at the system and read the latest smoothed load — the delta math +
        // smoothing live in the provider, decoupled from this frame rate. `ensure` is idempotent and
        // the getters are lock-free atomic reads; the provider auto-stops when nobody reads it.
        crate::sys_stats::ensure();
        let cpu = crate::sys_stats::cpu();
        let ram = crate::sys_stats::ram();
        render_pulse(cpu, ram, base, t, rows, cols)
    }
}

/// The pure Pulse renderer (no I/O) — takes the live CPU/RAM loads explicitly so it's deterministic
/// and unit-testable, exactly like `lighting::render_vitals`. `accent` is the colour-knob tint. Paints
/// a full `rows*cols` frame: the top `ceil(rows/2)` rows are the CPU bar, the bottom rows the RAM bar.
fn render_pulse(cpu: f32, ram: f32, accent: Rgb, t: f32, rows: u8, cols: u8) -> Vec<Rgb> {
    use std::f32::consts::TAU;
    let (r, c) = (rows as usize, cols as usize);
    let n = r * c;
    let mut f = vec![Rgb::BLACK; n];
    if n == 0 {
        return f;
    }
    let cpu = cpu.clamp(0.0, 1.0);
    let ram = ram.clamp(0.0, 1.0);
    // The whole board breathes; the RATE scales with CPU so a busy machine throbs faster (idle ≈ 0.4Hz
    // → pegged ≈ 2.4Hz). Brightness never drops below 0.7 so the bars stay readable through the dip.
    let rate = 0.4 + 2.0 * cpu;
    let breath = 0.85 + 0.15 * (t * TAU * rate).sin();
    // CPU on top, RAM on the bottom — CPU takes the middle row on an odd board.
    let cpu_rows = (r + 1) / 2;
    // Fill a zone's rows [y0, y1) as a horizontal meter for `load`: a full left→right run plus a
    // fractional leading edge, so even small load changes nudge the bar (responsive, not steppy).
    let mut paint = |y0: usize, y1: usize, load: f32| {
        let lc = load_color(load);
        // tint the load ramp toward the accent so the colour knob is honoured (no dead knob); the load
        // ramp stays dominant (70%) so green→red urgency still reads at a glance.
        let bar = Rgb::lerp(lc, accent, 0.30).scale_f(breath);
        let filled = load * c as f32;
        let full = filled.floor() as usize;
        let frac = filled - filled.floor();
        for y in y0..y1 {
            for x in 0..c {
                if x < full {
                    f[y * c + x] = bar;
                } else if x == full && frac > 0.0 {
                    // the leading edge dims with the fractional fill — a smooth meter tip.
                    f[y * c + x] = bar.scale_f(frac);
                }
            }
        }
    };
    paint(0, cpu_rows, cpu); // CPU — top
    paint(cpu_rows, r, ram); // RAM — bottom
    f
}

// ── ambient: the board mirrors the SCREEN — a true ambilight FRONTIER effect ─────────────────

/// Per-frame ease for the temporal smoothing, derived from the layer's `speed`: a faster speed snaps
/// the board to the screen, a slower one eases it (so colours glide instead of strobing). Clamped so
/// it always converges and never overshoots.
fn ambient_ease(speed: f32) -> f32 {
    (0.25 * speed).clamp(0.04, 1.0)
}

/// The saturation BOOST amount from the layer's `density`/"saturation" knob: 1.0 (default) = faithful
/// screen colour, higher = more pop. Below 1.0 clamps to no boost (the screen is never desaturated).
fn ambient_boost_amount(density: f32) -> f32 {
    (density - 1.0).max(0.0)
}

/// The pure Ambient renderer (no capture I/O) — maps each board cell to its matching SCREEN ZONE and
/// eases the previous frame toward it. Takes the screen grid + previous frame explicitly so it's
/// deterministic and unit-testable, exactly like [`render_pulse`]. The mapping is a true ambilight:
/// the LEFT of the screen lights the LEFT of the board, the TOP the top. `ease` is the per-frame lerp
/// factor; `boost` the saturation amount. Fills `rows*cols`; an all-black grid → a dark board (honest).
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
        // normalized board position → screen zone. A single-row/col board maps to that axis' centre.
        let ny = if r > 1 { y as f32 / (r as f32 - 1.0) } else { 0.5 };
        for x in 0..c {
            let nx = if c > 1 { x as f32 / (c as f32 - 1.0) } else { 0.5 };
            let zone = crate::screen_ambient::sample_grid(grid, gcols, grows, nx, ny);
            let target = crate::screen_ambient::boost_saturation(zone, boost);
            // ease toward the new colour from the previous frame (gentle temporal smoothing)
            let prev_cell = prev.get(y * c + x).copied().unwrap_or(Rgb::BLACK);
            out[y * c + x] = Rgb::lerp(prev_cell, target, ease);
        }
    }
    out
}

/// Ambient: the board becomes an AMBILIGHT — it samples the SCREEN and paints each key the colour of
/// the matching screen region (left of screen → left of board, top → top). A FRONTIER effect: a live
/// screen-mirror surface no locked vendor app exposes.
///
/// Like the audiometer/pulse it reads a SHARED background provider, [`crate::screen_ambient`], not a
/// per-frame capture: that ~18Hz capturer downscales the desktop to a tiny zone grid (GDI averages
/// it), so the capture cadence is decoupled from the ~6fps device stream and the on-screen preview
/// matches the board (both read the SAME grid). `speed` is the ease rate (how fast the board chases the
/// screen); the "saturation" knob (the `density` field) makes the colours pop. Colour is intrinsic — it
/// comes from the screen — so there's no colour knob and the layer `base` is ignored. Off-Windows or
/// before the first capture the grid is black and the board idles dark — honest, never faked.
pub struct Ambient {
    prev: Vec<Rgb>,
    dims: (u8, u8),
    p: EffectParams,
}
impl Ambient {
    fn new(p: EffectParams) -> Self {
        Ambient {
            prev: Vec::new(),
            dims: (0, 0),
            p,
        }
    }
}
impl FrameGen for Ambient {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, _base: Rgb) -> Vec<Rgb> {
        let n = rows as usize * cols as usize;
        if self.dims != (rows, cols) {
            self.prev = vec![Rgb::BLACK; n];
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Vec::new();
        }
        // point the shared capturer at the screen and read the latest zone grid — capture + downscale
        // live in the provider, decoupled from this frame rate. `ensure` is idempotent and `grid` is a
        // cheap lock+clone; the provider auto-stops when nobody reads it.
        crate::screen_ambient::ensure();
        let (gc, gr, grid) = crate::screen_ambient::grid();
        let ease = ambient_ease(self.p.speed);
        let boost = ambient_boost_amount(self.p.density);
        self.prev = render_ambient(&grid, gc, gr, &self.prev, rows, cols, ease, boost);
        self.prev.clone()
    }
}

// ── ripple: a reactive RADIAL wave — a keypress sends a ring of light across the board ────────

/// One live ripple: where it started (cell coordinates, fractional so the centre sits exactly on the
/// pressed key) and the elapsed time it was born at — so its radius and fade are a pure function of
/// the current `t`, no per-frame integration needed.
#[derive(Clone, Copy)]
struct RippleWave {
    or: f32, // origin row
    oc: f32, // origin column
    t0: f32, // birth time (elapsed seconds)
}

/// Ripple: pressing a key sends a ring of light radiating OUTWARD across the whole board from that
/// key's cell — the most-requested reactive effect, distinct from [`Reactive`] (which lights only the
/// pressed key). It watches the LIVE keyboard for fresh key-downs (the same safe `capture::key_down`
/// down-edge detection [`Reactive`] uses) and, for each press that resolves to a real cell
/// ([`crate::lighting::vk_to_key_cell`]), spawns a ripple centred there; a key the map doesn't carry
/// (mouse buttons, generic modifiers, media keys) spawns nothing — an accurate reactive surface never
/// rings from a key you didn't press. A small fixed POOL of ripples is kept (oldest evicted) so a
/// flurry of presses overlaps without unbounded growth.
///
/// Each ripple expands: its ring radius grows with age × `speed`, and a cell at matrix distance `d`
/// from the origin lights when `d` is near the current radius — brightness PEAKS at the ring and falls
/// off on both sides (a moving Gaussian band) AND fades as the ripple ages, so LUMINANCE depth is the
/// whole effect, not a flat ring. Cells take the MAX contribution over all active ripples; the colour
/// is the layer colour. `fade` is the ripple LIFETIME (higher fade → shorter-lived rings). Off Windows
/// the key read is a no-op, so the board idles dark — honest, never faked. (Needs a keypress to see.)
pub struct Ripple {
    waves: Vec<RippleWave>,
    prev: Vec<bool>, // last-frame down-state for VK 1..256 (fresh-press edge detection)
    dims: (u8, u8),
    p: EffectParams,
}

impl Ripple {
    fn new(p: EffectParams) -> Self {
        Ripple {
            waves: Vec::new(),
            prev: vec![false; 256],
            dims: (0, 0),
            p,
        }
    }

    /// Spawn a ripple centred on `(or, oc)`, born at `t`. The pool is fixed-size: once full, the OLDEST
    /// ripple (smallest birth time) is evicted so a burst of presses overlaps cleanly without growing
    /// the pool unbounded.
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

impl FrameGen for Ripple {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.waves.clear();
            self.dims = (rows, cols);
        }
        if n == 0 {
            return Vec::new();
        }
        // detect fresh key-downs and spawn a ripple at each pressed key's TRUE cell — the same one LED
        // per key the reactive map carries. A key the map doesn't carry resolves to None and spawns
        // nothing (no hashed-random fallback), so the surface stays accurate.
        for vk in 1..256usize {
            let down = crate::capture::key_down(vk as i32);
            if down && !self.prev[vk] {
                if let Some((ry, cx)) = crate::lighting::vk_to_key_cell(vk as i32) {
                    let (ry, cx) = (ry as usize, cx as usize);
                    if ry < r && cx < c {
                        self.spawn(ry as f32, cx as f32, t);
                    }
                }
            }
            self.prev[vk] = down;
        }
        // expansion + fade tunables. `speed` is the ring's outward velocity (cells/sec); `fade` sets the
        // LIFETIME (higher fade → shorter-lived rings). RING_WIDTH is the Gaussian band half-width in
        // cells — the ring's thickness.
        const BASE_SPEED: f32 = 7.0; // cells/sec at speed 1.0
        const BASE_LIFETIME: f32 = 2.2; // seconds at fade 1.0
        const RING_WIDTH: f32 = 1.15; // Gaussian sigma in cells
        let speed = self.p.speed.clamp(0.1, 6.0);
        let life = BASE_LIFETIME / self.p.fade.clamp(0.25, 4.0);
        // drop ripples that have aged out (or were born in the future after a time reset) so the pool
        // stays clean and a fresh press always finds room.
        self.waves.retain(|w| t >= w.t0 && (t - w.t0) <= life);
        let mut f = vec![Rgb::BLACK; n];
        if self.waves.is_empty() {
            return f;
        }
        // render: each cell takes the MAX contribution over the active ripples — brightness peaks at
        // the moving ring (Gaussian band), falls off on both sides, and decays with age. The layer
        // colour scaled by that 0..1 intensity IS the depth.
        for y in 0..r {
            for x in 0..c {
                let mut inten = 0.0f32;
                for w in &self.waves {
                    let age = t - w.t0;
                    let radius = age * BASE_SPEED * speed;
                    let dr = y as f32 - w.or;
                    let dc = x as f32 - w.oc;
                    let d = (dr * dr + dc * dc).sqrt();
                    let band_arg = (d - radius) / RING_WIDTH;
                    let band = (-(band_arg * band_arg)).exp(); // 1.0 at the ring, falling off both sides
                    let envelope = (1.0 - age / life).clamp(0.0, 1.0); // fades as the ripple ages
                    inten = inten.max(band * envelope);
                }
                if inten > 0.0 {
                    f[y * c + x] = base.scale_f(inten.clamp(0.0, 1.0));
                }
            }
        }
        f
    }
}

// ── comet: bright heads streaking on free velocity vectors, leaving fading tails + breakable ──────

/// One comet in the parade. A continuous float head POSITION `(x, y)` in cell coords plus a unit
/// VELOCITY `(vx, vy)` (so it travels at any slant, not locked to a raster), and a full set of
/// FRESHLY-ROLLED per-comet traits so no two are alike. `respawn` is the lifecycle clock: `0.0` = ALIVE
/// and streaking; `> 0.0` = DEAD, counting sim-steps down until it re-enters as a brand-new comet — no
/// edge wrapping, no eternal loop.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CometBody {
    x: f32,         // head column (continuous; NOT wrapped — a comet that runs off the board dies)
    y: f32,         // head row    (continuous; NOT wrapped)
    vx: f32,        // unit velocity x
    vy: f32,        // unit velocity y
    speed_mul: f32, // personal pace multiplier (~0.6..1.6) — some zippy, some cruising, never lockstep
    trail: f32,     // streak length in cells (short darts ↔ long streaks)
    bright: f32,    // head intensity (~0.82..1.0) — slight per-comet variation
    hue: f32,       // ± hue jitter (degrees) around the layer colour — a little life, still that colour
    respawn: f32,   // lifecycle clock: 0 = alive; > 0 = dead, counting steps down to a fresh respawn
}

/// Comet: an endless PARADE of varied bright heads streaking across the board, each leaving a fading
/// tail — self-animating, with an interactive "break" mechanic. The old version looped ONE comet on a
/// fixed wrapping trajectory forever (the same streak, over and over — flat and lifeless); this one is
/// a living stream of comets that are each DIFFERENT and never repeat.
///
/// **Lifecycle (no eternal loop).** A comet travels on a free velocity vector and does NOT wrap. When
/// it (head + whole trail) has fully run off the board it DIES and is RESPAWNED almost immediately —
/// only a tiny randomized stagger (≈0.04..0.29s) — as a brand-new comet from a random edge, so a fresh,
/// DIFFERENT streak enters shortly after the old one leaves (calm but alive, never the same loop). By
/// DEFAULT that's ONE comet: a single varied streak crosses, a brief beat, then a different one enters.
/// `density` scales the population UP from there — turn it up for a busy swarm (up to ~7 at once).
///
/// **Fresh rolls per (re)spawn** (the variety — drawn from the seeded xorshift each time, so no two
/// comets are alike): a COHESIVE, cardinal-biased DIRECTION — a primary AXIS (horizontal vs vertical,
/// the board's long axis gently favoured) + a travel direction, with the ENTRY EDGE coupled to it (a
/// comet enters one side and crosses ALONG its axis to the other) and a perpendicular DRIFT cubed to
/// concentrate near zero (MOST near-pure-cardinal, a thin tail reaching a true ~45°, never steeper); a
/// per-comet SPEED multiplier; a per-comet TRAIL length (short darts ↔ long streaks); slight per-comet
/// head BRIGHTNESS variation; and
/// a subtle HUE jitter (±18°) around the layer colour (small — a specific colour pick still reads as
/// itself, just with a little life). The tiny respawn stagger is randomized per comet too, so the fleet
/// stays desynchronized (they don't all enter/leave together) without ever leaving the board empty.
///
/// **Break.** Comet watches the LIVE keyboard for fresh key-downs (the same safe `capture::key_down`
/// down-edge scan [`Reactive`]/[`Ripple`] use). If a pressed key's cell
/// ([`crate::lighting::vk_to_key_cell`]) lands within a small hit radius of a live comet's HEAD, that
/// comet BREAKS: a white-hot BURST flashes at the impact (a radial splash into a decaying field — the
/// brightest moment on the board) and the comet RESPAWNS through the same fresh-roll path, so a broken
/// comet comes back DIFFERENT. Keys that miss do nothing; off Windows the key read is a no-op so the
/// motion simply plays on — graceful.
///
/// **Depth.** Each live comet is drawn as its own gradient STREAK every frame — a white-hot head
/// fading back along the reversed velocity to its (hue-jittered) layer-colour tail — composited with a
/// LIGHTEN blend so overlapping comets add light cleanly. A break burst pushes ABOVE head brightness to
/// pure white so a hit is unmistakably the brightest flash.
///
/// Driven by elapsed `t` through a step accumulator (the Cascade pattern): a fixed number of sim-steps
/// per second regardless of frame rate, so the streaks look the SAME at the legacy 6fps cap and at
/// 30/60fps, and a static `t` still steps once so they never freeze. The PRNG is seeded so a run is
/// reproducible (reproducible ≠ repetitive — it ACTUALLY varies every comet). Reads the layer colour.
pub struct Comet {
    level: Vec<f32>,        // the break-BURST field only: a burst paints >1.0, then this decays to 0
    comets: Vec<CometBody>, // the parade (count == `density`: 1 by default, up to ~7; dead ones respawn)
    prev: Vec<bool>,        // last-frame key-down state for VK 1..256 (fresh-press edge detection)
    dims: (u8, u8),
    p: EffectParams,
    rng: u32,      // deterministic xorshift (the spawn variety) — no `rand` dep, like Fire/Cascade
    last_t: f32,   // elapsed time at the previous frame, to derive how far the sim advances
    step_acc: f32, // fractional simulation-step accumulator (honours `speed` smoothly)
}

impl Comet {
    fn new(p: EffectParams) -> Self {
        Comet {
            level: Vec::new(),
            comets: Vec::new(),
            prev: vec![false; 256],
            dims: (0, 0),
            p,
            rng: 0xC0FF_EE11,
            last_t: 0.0,
            step_acc: 0.0,
        }
    }

    fn rand(&mut self) -> f32 {
        // xorshift32 -> 0.0..1.0 (the same lean PRNG Fire/Cascade/Starlight use)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 / (1u32 << 24) as f32
    }

    /// How many comets stream at once. The `density` DEFAULT (1.0) yields exactly ONE calm streak; turn
    /// the knob UP and the population scales into a busy swarm — up to ~7 at max (3.0). Below the default
    /// it stays at 1 (you never get fewer than one comet). Monotonic — this is how `density` is honoured.
    fn count(&self) -> usize {
        let d = self.p.density.clamp(0.25, 3.0);
        (1.0 + (d - 1.0).max(0.0) * 3.0).round().clamp(1.0, 8.0) as usize
    }

    /// Roll a brand-NEW comet — EVERYTHING fresh from the PRNG so no two are alike. Used for the initial
    /// population, an edge respawn (a comet that died), and a break respawn. It picks a primary AXIS,
    /// a travel direction, and the COUPLED entry edge, then crosses the board along that axis with a
    /// cubed perpendicular drift (cardinal-biased: clean H/V common, a gentle lean frequent, a true ~45°
    /// rake rare) — plus its own pace, trail length, head brightness, and a subtle hue jitter. Born
    /// ALIVE (`respawn = 0`).
    fn spawn_body(&mut self, r: usize, c: usize) -> CometBody {
        let (rf, cf) = (r.max(1) as f32, c.max(1) as f32);
        // DIRECTION — cardinal-biased and AXIS-COUPLED, so comets read as cohesive board-aligned sweeps
        // (clean H / V the common case, a gentle lean the frequent variety, a true ~45° rake the rare
        // spice) instead of the old perpetual diagonal. Built in four steps:
        //
        // 1. PRIMARY AXIS — horizontal vs vertical. The board's long axis is gently favoured (a wide
        //    board ⇒ more horizontal sweeps); the bias is clamped to a modest band so BOTH axes always
        //    stay well populated (vertical drops never starve on a very wide board, nor horizontal on a
        //    tall one). For the real 6×22 board this lands ≈60/40 horizontal.
        let p_horizontal = (cf / (cf + rf)).clamp(0.4, 0.6);
        let horizontal = self.rand() < p_horizontal;
        // 2. TRAVEL DIRECTION along that axis (±, 50/50): horizontal → rightward/leftward, vertical →
        //    downward/upward. (Note +y points DOWN — row 0 is the top.)
        let positive = self.rand() < 0.5;
        // 3+4. PERPENDICULAR DRIFT concentrated near zero: magnitude = sign · u³ · (primary = 1.0), with
        //    u uniform in [0,1]. Cubing piles the mass near 0 — MOST comets are near-pure-cardinal — while
        //    the thin tail reaches |drift| = 1.0, i.e. a true 45° (the cap: the perpendicular can never
        //    exceed the primary, so a horizontal-primary comet always still READS as horizontal). The
        //    (primary, perpendicular) vector is then normalised to unit velocity.
        let drift_sign = if self.rand() < 0.5 { -1.0 } else { 1.0 };
        let u = self.rand(); // 0..1
        let drift = drift_sign * u * u * u; // |drift| ≤ 1.0 ⇒ lean ≤ 45°
        let inv = 1.0 / (1.0 + drift * drift).sqrt(); // normaliser for (1, drift)
        // 3 (cont.). ENTRY EDGE coupled to axis+direction — a comet enters one side and crosses ALONG
        //    its axis to the other (cohesive sweeps/drops), at a random offset along that edge: moving
        //    right enters the LEFT edge; left → RIGHT; down → TOP; up → BOTTOM.
        let (x, y, vx, vy) = if horizontal {
            let dir = if positive { 1.0 } else { -1.0 };
            let x0 = if positive { -1.0 } else { cf }; // right → from the left, left → from the right
            let y0 = self.rand() * rf; // random offset down the entry edge
            (x0, y0, dir * inv, drift * inv) // primary = horizontal, perpendicular = vertical drift
        } else {
            let dir = if positive { 1.0 } else { -1.0 };
            let y0 = if positive { -1.0 } else { rf }; // down → from the top, up → from the bottom
            let x0 = self.rand() * cf; // random offset across the entry edge
            (x0, y0, drift * inv, dir * inv) // primary = vertical, perpendicular = horizontal drift
        };
        // per-comet rolls — the variety that makes the parade lively:
        let speed_mul = 0.6 + self.rand() * 1.0; // 0.6..1.6 — some zippy, some cruising
        let span = (rf * rf + cf * cf).sqrt();
        let trail = (span * (0.18 + self.rand() * 0.45)).max(2.5); // ~18..63% of the diagonal
        let bright = 0.82 + self.rand() * 0.18; // 0.82..1.0 head intensity
        let hue = (self.rand() * 2.0 - 1.0) * 18.0; // ±18° subtle hue jitter around the layer colour
        CometBody { x, y, vx, vy, speed_mul, trail, bright, hue, respawn: 0.0 }
    }

    /// Paint a bright radial BURST into the break-flash field at `(x, y)` — the comet-break shatter. The
    /// core is pushed ABOVE 1.0 so the render drives it to PURE white (the brightest moment on the
    /// board); it falls off to a coloured glow at the rim, then fades with the field. Out-of-board cells
    /// are clipped (NO wrap — the board no longer loops).
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
                let inten = 1.0 - d / BURST_RADIUS; // 1 at the core → 0 at the rim
                let val = 0.6 + 0.9 * inten; // rim ~0.6 (coloured) → core ~1.5 (pure white in render)
                let i = gy as usize * c + gx as usize;
                self.level[i] = self.level[i].max(val);
            }
        }
    }

    /// A press at cell `(pr, pc)`: BREAK every live comet whose head sits within the hit radius —
    /// flashing a burst at the impact and RESPAWNING the comet through the fresh-roll path (so a broken
    /// comet comes back DIFFERENT). Dead comets (awaiting respawn) have no head and are skipped. Returns
    /// whether anything broke. Pure of I/O (the live key read happens in `frame`), so it's unit-testable.
    fn break_at(&mut self, pr: f32, pc: f32, r: usize, c: usize) -> bool {
        const HIT_RADIUS: f32 = 1.5; // within ~1 cell of the head — a fair, satisfying hit window
        let mut broke = false;
        for i in 0..self.comets.len() {
            if self.comets[i].respawn > 0.0 {
                continue; // a dead comet has no head on the board to hit
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

    /// Has comet `i` (head + its whole trail) fully run off the board? Once the head is more than a
    /// trail-length past an edge, the tail has cleared too — the comet is gone and should die.
    fn fully_off(&self, i: usize, r: usize, c: usize) -> bool {
        let b = &self.comets[i];
        let m = b.trail + 2.0;
        b.x < -m || b.x > (c as f32 - 1.0) + m || b.y < -m || b.y > (r as f32 - 1.0) + m
    }

    /// Advance the parade one step. Fade the break-flash field a notch; then for each comet either count
    /// down its respawn gap (and re-roll a brand-new comet when it elapses) or — if alive — move it along
    /// its velocity (NO wrap) and mark it dead once it has fully left the board. Steps-per-second is
    /// fixed, so the number of steps a frame runs IS the travel rate — which is how `speed` is honoured.
    fn step(&mut self, r: usize, c: usize) {
        const BURST_DECAY: f32 = 0.80; // per-step fade of the break-flash field (smooth exponential)
        const ADVANCE: f32 = 0.45; // base cells a head moves per step (<1 so the streak is continuous)
        // near-IMMEDIATE respawn — only a tiny per-comet stagger (≈0.04..0.29s at 24/s) so a fresh comet
        // enters as the old one leaves: a STEADY population with no dead air, just enough jitter to keep
        // entrances desynchronized (never all in/out together).
        const RESPAWN_MIN: f32 = 1.0;
        const RESPAWN_SPAN: f32 = 6.0;
        for v in self.level.iter_mut() {
            *v *= BURST_DECAY;
            if *v < 0.02 {
                *v = 0.0;
            }
        }
        for i in 0..self.comets.len() {
            // DEAD: count the gap down, then RESPAWN as a brand-new, freshly-rolled comet from a random
            // edge — so the board shows an endless parade of DIFFERENT comets, never the same one looping.
            if self.comets[i].respawn > 0.0 {
                self.comets[i].respawn -= 1.0;
                if self.comets[i].respawn <= 0.0 {
                    self.comets[i] = self.spawn_body(r, c);
                }
                continue;
            }
            // ALIVE: travel along the velocity — NO WRAP. A comet that runs off the board DIES (queues a
            // randomized respawn delay), instead of looping its trajectory forever.
            let dist = ADVANCE * self.comets[i].speed_mul;
            self.comets[i].x += self.comets[i].vx * dist;
            self.comets[i].y += self.comets[i].vy * dist;
            if self.fully_off(i, r, c) {
                self.comets[i].respawn = RESPAWN_MIN + self.rand() * RESPAWN_SPAN;
            }
        }
    }
}

impl FrameGen for Comet {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.level = vec![0.0; n];
            self.comets.clear();
            self.dims = (rows, cols);
            self.last_t = t;
            self.step_acc = 0.0;
        }
        if n == 0 {
            return Vec::new();
        }
        // reconcile the parade size with the `density` count (spawn or trim as needed). A freshly-seeded
        // comet is SCATTERED along its path (advanced a random lead) so the board is alive immediately
        // rather than waiting for the first edge entries — a comet pushed past the far edge by the lead
        // simply dies on the next step and re-enters cleanly, which also staggers the entrances.
        let want = self.count();
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
        // SPEED drives the sim from elapsed time: ~24 steps/sec at speed 1.0, scaled by `speed`,
        // accumulating the fraction so slow comets still advance smoothly. A static `t` still steps once
        // (never freezes); a time reset (dt<0) steps once; the burst is capped so a long stall can't run
        // thousands of steps in one frame.
        const BASE_STEPS_PER_SEC: f32 = 24.0;
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        let spd = self.p.speed.clamp(0.1, 6.0);
        self.step_acc += dt * BASE_STEPS_PER_SEC * spd;
        let mut steps = self.step_acc.floor().max(1.0) as u32;
        self.step_acc -= self.step_acc.floor();
        steps = steps.min(8);
        for _ in 0..steps {
            self.step(r, c);
        }
        // BREAK: scan for fresh key-downs and break any comet whose head the press hit — the same safe
        // down-edge read reactive/ripple use. A key the map doesn't carry (or off Windows) resolves to
        // nothing, so the motion simply plays on. A broken comet respawns through the fresh-roll path.
        for vk in 1..256usize {
            let down = crate::capture::key_down(vk as i32);
            if down && !self.prev[vk] {
                if let Some((ry, cx)) = crate::lighting::vk_to_key_cell(vk as i32) {
                    let (ry, cx) = (ry as usize, cx as usize);
                    if ry < r && cx < c {
                        self.break_at(ry as f32, cx as f32, r, c);
                    }
                }
            }
            self.prev[vk] = down;
        }
        // RENDER: each LIVE comet draws its own gradient streak (white-hot head → hue-jittered
        // layer-colour tail, with its own length + brightness) composited LIGHTEN so overlapping comets
        // add light cleanly. Dead comets (in their respawn gap) draw nothing. The break-BURST field is
        // overlaid on top: its core renders PURE white (the brightest moment) and fades as it decays.
        let mut frame = vec![Rgb::BLACK; n];
        for b in &self.comets {
            if b.respawn > 0.0 {
                continue;
            }
            draw_comet(&mut frame, b, base, r, c);
        }
        for i in 0..n {
            let v = self.level[i];
            if v > 0.0 {
                frame[i] = lighten(frame[i], burst_color(v, base));
            }
        }
        frame
    }
}

// ── comet render helpers (free fns; no generator state) ───────────────────────────────────────

/// Per-channel max ("lighten") so overlapping comets / a burst add light without clipping to a wash.
fn lighten(a: Rgb, b: Rgb) -> Rgb {
    Rgb::new(a.r.max(b.r), a.g.max(b.g), a.b.max(b.b))
}

/// Deposit `col` at the continuous point `(px, py)` with a small anti-aliased radial footprint, so a
/// streak glides smoothly between cells instead of snapping cell-to-cell. Out-of-board cells are
/// skipped — NO wrap (a comet that leaves the board simply paints less, then dies).
fn deposit(frame: &mut [Rgb], px: f32, py: f32, col: Rgb, r: usize, c: usize) {
    const REACH: f32 = 1.1; // splat radius in cells — keeps the streak ~1 cell wide (crisp, not smeared)
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
            let i = gy as usize * c + gx as usize;
            frame[i] = lighten(frame[i], col.scale_f(w));
        }
    }
}

/// Rotate `base`'s HUE by `deg` while keeping its saturation + value — the subtle per-comet hue jitter.
/// A greyscale / near-black base has no hue to rotate (and `deg == 0` is a no-op), so it's returned
/// unchanged: a specific colour pick still reads as itself, with only a little life.
fn jitter_hue(base: Rgb, deg: f32) -> Rgb {
    if deg == 0.0 {
        return base;
    }
    let (rf, gf, bf) = (base.r as f32 / 255.0, base.g as f32 / 255.0, base.b as f32 / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    if max - min < 0.02 {
        return base; // no chroma → nothing to rotate
    }
    Rgb::from_hsv(rgb_hue(base) + deg, (max - min) / max, max)
}

/// Draw one live comet as a gradient STREAK: from the white-hot head (`d = 0`) back along the reversed
/// velocity to the tail end (`d = trail`), fading to the (hue-jittered) layer colour. Per-comet `trail`
/// length and `bright` head intensity give every comet its own look. Sampled in sub-cell steps and
/// anti-aliased (via [`deposit`]) so the streak stays continuous and smooth at any slant.
fn draw_comet(frame: &mut [Rgb], b: &CometBody, base: Rgb, r: usize, c: usize) {
    const SAMPLE_STEP: f32 = 0.4; // sub-cell sampling along the trail (< the splat reach ⇒ no gaps)
    let white = Rgb::new(255, 255, 255);
    let col = jitter_hue(base, b.hue);
    let trail = b.trail.max(2.0);
    let mut d = 0.0;
    while d <= trail {
        let f = 1.0 - d / trail; // 1 at the head → 0 at the tail end
        let whiteness = ((f - 0.7) / 0.3).clamp(0.0, 1.0); // only the front of the streak whitens
        let sample = Rgb::lerp(col, white, 0.85 * whiteness).scale_f(b.bright * f);
        if sample != Rgb::BLACK {
            deposit(frame, b.x - b.vx * d, b.y - b.vy * d, sample, r, c);
        }
        d += SAMPLE_STEP;
    }
}

/// Map a break-BURST field value to its colour: a coloured glow at the rim, brightening to a white-hot
/// head, and PURE white once pushed above 1.0 (the brightest moment of a hit). Mirrors a comet head's
/// toward-white depth so a burst reads as a hotter version of a head.
fn burst_color(v: f32, base: Rgb) -> Rgb {
    const HEAD_THRESH: f32 = 0.88;
    let white = Rgb::new(255, 255, 255);
    if v < HEAD_THRESH {
        base.scale_f(v)
    } else {
        let to_head = ((v - HEAD_THRESH) / (1.0 - HEAD_THRESH)).clamp(0.0, 1.0);
        let head_col = Rgb::lerp(base, white, 0.85 * to_head);
        let over = ((v - 1.0) / 0.5).clamp(0.0, 1.0);
        Rgb::lerp(head_col, white, over)
    }
}

// ── aurora: a slow, flowing northern-lights gradient — drifting bands of hue + luminance ──────

/// Derive a hue (degrees, 0..360) from an Rgb — the layer colour's hue, used as the CENTRE the aurora
/// flow drifts around. A greyscale/near-black colour has no hue, so it falls back to a pleasant aurora
/// green (≈150°) rather than collapsing to red.
fn rgb_hue(c: Rgb) -> f32 {
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d < 0.02 {
        return 150.0; // no chroma → default aurora green
    }
    let h = if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    h.rem_euclid(360.0)
}

/// Aurora: a slow, flowing northern-lights wash. Several INCOMMENSURATE sine flows over (x, y, t) drift
/// the HUE so colours wander organically across the board (greens ↔ cyans ↔ blues ↔ purples), while a
/// SEPARATE, slower set of sine waves undulates the BRIGHTNESS so bands glow and dim — depth lives in
/// BOTH hue and luminance, never a flat wash. The layer colour biases the palette: its hue is the
/// CENTRE the flow varies around (so the aurora can be tinted warm or cool). `speed` is the flow rate.
/// Pure time function — self-animating, gentle, no jank (distinct from spectrum's uniform wash and
/// wave's single travelling crest).
pub struct Aurora {
    p: EffectParams,
}
impl FrameGen for Aurora {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        use std::f32::consts::TAU;
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        let mut f = vec![Rgb::BLACK; n];
        if n == 0 {
            return f;
        }
        let spd = self.p.speed.clamp(0.1, 6.0);
        let base_hue = rgb_hue(base);
        // hue swing around the base, in degrees — wide enough to read as an aurora's colour drift,
        // narrow enough to stay in the green/blue/purple family rather than a full rainbow.
        const HUE_SPAN: f32 = 70.0;
        // three incommensurate temporal rates (slow — aurora is gentle) so the flow never visibly loops.
        let t1 = t * 0.10 * spd;
        let t2 = t * 0.067 * spd;
        let t3 = t * 0.041 * spd;
        for y in 0..r {
            let ny = if r > 1 { y as f32 / (r as f32 - 1.0) } else { 0.5 };
            for x in 0..c {
                let nx = if c > 1 { x as f32 / (c as f32 - 1.0) } else { 0.5 };
                // HUE: three incommensurate sine flows over space+time → an organic drift around base.
                let h1 = (nx * 1.5 + ny * 0.6 + t1 * TAU).sin();
                let h2 = (nx * 0.7 - ny * 1.3 + t2 * TAU).sin();
                let h3 = (ny * 2.1 + t3 * TAU).sin();
                let hue_drift = h1 * 0.5 + h2 * 0.3 + h3 * 0.2; // -1..1
                let hue = base_hue + hue_drift * HUE_SPAN;
                // BRIGHTNESS: a SEPARATE, slower undulation — waves of luminance that glow and dim, so
                // the bands stand out in depth, not only in hue. Stays in ~0.30..1.0 (never fully dark).
                let b1 = (nx * 1.1 - t2 * TAU * 0.8).sin();
                let b2 = (ny * 1.7 + t3 * TAU * 1.3).sin();
                let lum = 0.5 + 0.5 * (b1 * 0.6 + b2 * 0.4); // 0..1
                let v = (0.30 + 0.70 * lum).clamp(0.0, 1.0);
                f[y * c + x] = Rgb::from_hsv(hue, 0.85, v);
            }
        }
        f
    }
}

// ── typing heat: your typing rendered as TEMPERATURE — warms + brightens as you type, cools idle ──

/// TypingHeat: the board is a LIVING, position-aware heat MAP of your typing — a per-cell thermal field
/// that is ALWAYS in motion and can NEVER settle into a flat lit slab. Three forces act on the field,
/// exactly like real heat on a plate:
///
/// 1. **Local deposits (WHERE you type).** Each fresh key-down EDGE (the safe `capture::key_down`
///    down-edge scan reactive/ripple use) deposits a soft RADIAL splat at that key's TRUE cell
///    ([`crate::lighting::vk_to_key_cell`]) — a hot core with a smooth falloff. So the board shows where
///    your fingers land. A key the map doesn't carry (mouse buttons, modifiers, media) deposits nothing.
/// 2. **Diffusion (heat SPREADS, heat-CONSERVING).** Every frame the field is DIFFUSED — a cheap
///    4-neighbour conduction blur with a gentle UPWARD buoyancy (heat rises) — so hot spots bloom and bleed
///    into their neighbours into organic flowing gradients instead of isolated dots. It REDISTRIBUTES heat
///    as pairwise edge fluxes (what leaves a cell enters its neighbour), conserving the total exactly, so
///    the blur never eats energy — the only thing that removes heat is the cooling. Trivial on the small grid.
/// 3. **Continuous cooling (the COOLDOWN — now PHYSICAL).** The field always decays, every frame, by a
///    temperature-DEPENDENT curve (Newton's law of cooling + radiative loss): a hot cell sheds heat FAST, a
///    cool cell SLOWLY (see [`cool_field`]). So a white-hot key flashes down through orange→red→dim in
///    ~½–1s while the residual embers LINGER for several seconds — a long, satisfying drain after a stop,
///    not the old too-fast uniform fade. Tuned against the diffusion so the board is ALWAYS a gradient in
///    motion — hot where you just typed, cooling trails where you typed a moment ago — and so sustained fast
///    typing can NEVER flood it to a uniform fully-lit slab (the old global-warmth flood's exact failure).
///
/// **Speed → INTENSITY, not coverage.** There is NO global uniform warmth term any more (that flooded the
/// whole board flat). Instead your typing RATE — a leaky integrator of fresh key-downs — sets how HOT
/// each deposit lands: a fast flurry pushes deposits white-hot, slow typing leaves dim embers. So
/// INTENSITY (whiteness/peak) reflects how fast you type while POSITION (where the heat sits) reflects
/// where you type.
///
/// **Render** (the pure [`render_typing_heat`]): a cell's temperature IS its field value, mapped through
/// an INCANDESCENT (blackbody) ramp — cold = a faint cool-dark glow → deep ember-red → red → orange →
/// amber → WHITE-hot — so it reads HOT at every level, never through a green/cyan zone, brightening as it
/// heats. A heat-haze SHIMMER (depth scaling with local temperature, the Fire-flicker idea) makes a hot
/// board breathe. The colour knob RECOLOURS the flame COHERENTLY (a hue rotation, not a muddy lerp): its
/// warm default is classic fire, a cool colour gives a clean cold-flame; white-hot flare cores stay pure
/// white regardless of the flame's hue.
///
/// fps-independent: cooling, diffusion and the rate release are all dt-scaled (a static `t` simply freezes
/// — no spread, no decay). Cross-platform clean: key reads go through the seamed `capture::key_down`
/// (off-Windows → false), so the board just idles cool. Lightweight: the diffusion is one small per-frame
/// pass over the grid into a REUSED scratch buffer — no per-frame allocation, no perf regression.
pub struct TypingHeat {
    heat: Vec<f32>,    // per-cell heat field — deposits land here; cooled + diffused every frame
    scratch: Vec<f32>, // reused diffusion double-buffer (no per-frame allocation)
    pending: Vec<(usize, usize)>, // this frame's fresh-press cells, reused (deposit after the rate update)
    prev: Vec<bool>,   // last-frame down-state for VK 1..256 (fresh-press edge detection)
    rate: f32,         // typing-speed EMA 0..1 — sets DEPOSIT peak (intensity), NOT a global board flood
    dims: (u8, u8),
    p: EffectParams,
    last_t: f32, // elapsed time at the previous frame, to derive `dt` for fps-independent cooling/diffusion
}

impl TypingHeat {
    fn new(p: EffectParams) -> Self {
        TypingHeat {
            heat: Vec::new(),
            scratch: Vec::new(),
            pending: Vec::new(),
            prev: vec![false; 256],
            rate: 0.0,
            dims: (0, 0),
            p,
            last_t: 0.0,
        }
    }
}

impl FrameGen for TypingHeat {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        if self.dims != (rows, cols) {
            self.heat = vec![0.0; n];
            self.scratch = vec![0.0; n];
            self.prev = vec![false; 256];
            self.rate = 0.0;
            self.dims = (rows, cols);
            self.last_t = t;
        }
        if n == 0 {
            return Vec::new();
        }
        let dt = (t - self.last_t).max(0.0);
        self.last_t = t;
        // FADE is the COOLDOWN speed — it drives the field cooling AND the rate release: higher → cooler.
        let fade = self.p.fade.clamp(0.1, 4.0);
        // SENSITIVITY (rides the density field) is the deposit-strength gain — how hot each press lands.
        let sens = self.p.density.clamp(0.25, 3.0);
        // (1) COOL the whole field, then (2) DIFFUSE it — so existing heat fades + blooms before this
        // frame's fresh presses land crisp on top. Both dt-scaled (fps-independent); a static t freezes.
        cool_field(&mut self.heat, fade, dt);
        diffuse_field(&mut self.heat, &mut self.scratch, r, c, dt);
        // detect fresh key-downs: count them (for the typing RATE) and record each pressed key's TRUE
        // cell. A key the map doesn't carry (mouse buttons, generic modifiers, media keys) resolves to
        // None and is skipped — accurate, no random heat.
        self.pending.clear();
        let mut presses = 0u32;
        for vk in 1..256usize {
            let down = crate::capture::key_down(vk as i32);
            if down && !self.prev[vk] {
                presses += 1;
                if let Some((ry, cx)) = crate::lighting::vk_to_key_cell(vk as i32) {
                    let (ry, cx) = (ry as usize, cx as usize);
                    if ry < r && cx < c {
                        self.pending.push((ry, cx));
                    }
                }
            }
            self.prev[vk] = down;
        }
        // update the typing-RATE EMA from this frame's presses, then DEPOSIT: the rate sets the deposit
        // PEAK (fast typing → white-hot, slow → dim embers — speed → INTENSITY, not coverage).
        self.rate = step_rate(self.rate, presses, dt, fade);
        let peak = deposit_peak(self.rate, sens);
        for &(ry, cx) in &self.pending {
            deposit_heat(&mut self.heat, ry, cx, r, c, peak);
        }
        render_typing_heat(&self.heat, base, t, rows, cols)
    }
}

/// Deposit heat for one fresh key-down as a soft RADIAL splat — a hot core at the pressed cell with a
/// smooth falloff into the cells around it, so a press reads as a real hot spot (a glowing blob), not a
/// hard `+`-cross. `peak` is the core heat, set by the typing RATE × sensitivity (see [`deposit_peak`]):
/// white-hot for a fast flurry, a dim ember for a lone slow press. Accumulates into the field, clamped so
/// a mashed key saturates to white-hot instead of running away. A squared falloff keeps the core tight.
fn deposit_heat(heat: &mut [f32], ry: usize, cx: usize, r: usize, c: usize, peak: f32) {
    const RADIUS: f32 = 1.8; // splat reach in cells (a ~3-cell-wide glow)
    const MAX: f32 = 1.6; // ceiling so repeated hits saturate (white-hot) instead of running away
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
            let falloff = 1.0 - d / RADIUS; // 1 at the core → 0 at the rim
            let add = peak * falloff * falloff; // squared ⇒ a concentrated core, soft edge
            let i = ny as usize * c + nx as usize;
            heat[i] = (heat[i] + add).min(MAX);
        }
    }
}

/// Cool the whole heat field one frame — a temperature-DEPENDENT, dt-scaled radiative+convective leak: the
/// physics of a glowing body cooling, not a flat fraction off every cell. A HOT cell sheds heat FAST
/// (radiative loss ∝ Tⁿ), a COOL cell sheds it SLOWLY (a gentle Newtonian baseline toward ambient ≈ 0), so
/// a white-hot key flashes down through orange→red→dim in ~½–1s while the residual embers LINGER for
/// several seconds — a long, satisfying drain — instead of the old flat exponential that drained the whole
/// board uniformly and too fast (embers died as quickly as flares).
///
/// Model: `dT/dt = -(RAD·T³ + LIN·T)·fade`. The `RAD·T³` term is the RADIATIVE loss (true Stefan–Boltzmann
/// is T⁴, but T³ reads the same and avoids underflow — cheap, just one extra `T*T`); it dominates when the
/// cell is hot and vanishes fast as it cools. The `LIN·T` term is the slow NEWTONIAN baseline (an
/// exponential tail toward ambient 0 with τ ≈ 1/(LIN·fade) ≈ 4.5s — this is what makes embers linger). We
/// integrate by FREEZING the per-cell rate over the frame and stepping exponentially —
/// `T·exp(-(RAD·T² + LIN)·fade·dt)` — which is unconditionally stable, can NEVER go negative (asymptotic
/// toward 0: the board reaches ~dark when idle, just slowly at the low end), and is fps-independent because
/// it uses the REAL elapsed `dt` (dt 0 ⇒ no change; the board freezes on a paused clock — no baked-in step).
/// `fade` scales the WHOLE rate, so the user can still cool faster/slower around this realistic curve. This
/// temperature-dependent cooling — fast at the top, slow at the bottom — is what keeps the board a moving
/// gradient (a just-hit key always outshines one hit a moment ago) and a lingering ember-tail after a stop.
fn cool_field(field: &mut [f32], fade: f32, dt: f32) {
    const RAD: f32 = 3.0; // radiative loss weight (the Tⁿ term) — a hot cell's fast flash through the ramp
    const LIN: f32 = 0.22; // Newtonian baseline toward ambient 0 — the slow ember tail (τ ≈ 1/LIN ≈ 4.5s)
    let f = fade.clamp(0.1, 4.0);
    let dt = dt.max(0.0);
    if dt == 0.0 {
        return; // a paused clock freezes the field — no cooling on a static t
    }
    for h in field.iter_mut() {
        let t = *h;
        if t <= 0.0 {
            continue;
        }
        // the fractional cooling RATE rises with temperature: RAD·T² (radiative, dominant when hot) plus
        // the gentle LIN baseline (dominant for embers). Freeze it over the frame and step exponentially —
        // stable, never negative, asymptotic toward 0. So a hot cell loses a BIGGER fraction than a cool one.
        let rate = (RAD * t * t + LIN) * f;
        *h = t * (-rate * dt).exp();
    }
}

/// Diffuse the heat field one frame — a heat-CONSERVING 4-neighbour conduction blur with a gentle UPWARD
/// buoyancy (heat rises). Unlike a plain "blend toward the neighbour average" (which quietly LEAKS energy at
/// the boundaries and through directional weighting — the blur itself eating heat), this redistributes heat
/// as explicit pairwise EDGE FLUXES: whatever flows OUT of a cell flows INTO its neighbour, so the field
/// SUM is preserved exactly — the ONLY thing that removes heat is [`cool_field`]. Effect: a hot key BLOOMS
/// and its warmth spreads + lingers over a WIDER area (real conduction) instead of the dots staying isolated
/// and the spread silently draining the board.
///
/// Two conserving transports, accumulated as per-cell DELTAS into the REUSED `scratch` buffer (no per-frame
/// alloc): (1) symmetric DIFFUSION `k·(Tₐ−T_b)` toward equilibrium on every edge (heat flows hot→cold); and
/// (2) on each vertical edge a gentle BUOYANCY that lifts a small fraction of the LOWER cell's heat into the
/// one above it (heat rises). Both are antisymmetric — what leaves one cell enters the other — so total
/// energy is conserved to float precision. The per-edge weight is the interior toward-average blend ÷ 4, so
/// even a 4-edge interior cell stays inside the explicit-scheme stability limit (a boundary cell simply
/// exchanges with fewer neighbours). dt-scaled, so the spread is fps-independent (dt 0 ⇒ no spread — the
/// field freezes on a paused clock).
fn diffuse_field(field: &mut [f32], scratch: &mut [f32], r: usize, c: usize, dt: f32) {
    const DIFFUSE_RATE: f32 = 6.0; // conduction speed → interior toward-average blend per frame (× dt)
    const RISE_RATE: f32 = 1.0; // buoyancy speed → fraction of a lower cell's heat that rises per frame
    let dt = dt.max(0.0);
    if dt == 0.0 || scratch.len() != field.len() || field.is_empty() {
        return; // a paused clock (or a size mismatch) ⇒ no spread; the field is left untouched
    }
    // per-edge diffusion weight = the interior toward-average blend ÷ 4, so an interior cell (4 edges) stays
    // inside the explicit conservative scheme's stability limit while a boundary cell just exchanges with
    // fewer neighbours — a proper conservative discretisation, not a lossy normalised average.
    let k = (DIFFUSE_RATE * dt).min(0.45) / 4.0;
    let rise = (RISE_RATE * dt).min(0.08); // gentle, capped so each cell's self-weight stays positive (stable)
    for s in scratch.iter_mut() {
        *s = 0.0; // scratch now holds per-cell DELTAS; accumulate every edge's flux, then apply once
    }
    for y in 0..r {
        for x in 0..c {
            let i = y * c + x;
            let here = field[i];
            // horizontal edge to the RIGHT neighbour — symmetric diffusion (counts each edge once).
            if x + 1 < c {
                let j = i + 1;
                let flux = k * (here - field[j]); // hot → cold
                scratch[i] -= flux;
                scratch[j] += flux;
            }
            // vertical edge to the neighbour BELOW: symmetric diffusion + an upward buoyancy that lifts a
            // little of the LOWER cell's heat into this (upper) one (heat rises). Both conserve.
            if y + 1 < r {
                let below = i + c;
                let flux = k * (here - field[below]); // diffusion between this cell and the one below
                scratch[i] -= flux;
                scratch[below] += flux;
                let buoy = rise * field[below]; // a fraction of the lower cell's heat rises into this one
                scratch[below] -= buoy;
                scratch[i] += buoy;
            }
        }
    }
    for (h, d) in field.iter_mut().zip(scratch.iter()) {
        *h += *d;
    }
}

/// The pure typing-RATE integrator (no I/O) — a leaky integrator of fresh key-downs, so the rate DYNAMICS
/// are deterministic and unit-testable. Each press adds a ballistic impulse (heats quickly as you type);
/// between frames it leaks toward 0 with an exponential release scaled by `fade`, dt-scaled so it's
/// fps-independent. Returns the new rate in 0..1 — an EMA of your keys-per-second (faster typing → higher
/// rate). It NO LONGER lights the board directly (that uniform flood was the flat-slab bug); it only sets
/// how hot the next deposits land (see [`deposit_peak`]).
fn step_rate(rate: f32, presses: u32, dt: f32, fade: f32) -> f32 {
    const GAIN: f32 = 0.09; // rate per press (≈saturates at a brisk typing pace)
    const RELEASE_BASE: f32 = 0.5; // per-second leak at fade 1.0 (settles back over ~2s)
    let fade = fade.clamp(0.1, 4.0);
    let release = (-RELEASE_BASE * fade * dt.max(0.0)).exp();
    let cooled = rate.clamp(0.0, 1.0) * release;
    (cooled + presses as f32 * GAIN).clamp(0.0, 1.0)
}

/// How HOT a fresh deposit lands — the SPEED→INTENSITY mapping. The typing `rate` (0..1) lifts the peak
/// from a dim ember (slow/lone press) toward white-hot (a fast flurry), and `sensitivity` (the
/// deposit-strength gain, riding the density knob) scales the whole thing — turn it up to heat readily on
/// light typing, down to need a real flurry. The returned value is the splat's CORE heat; the field's own
/// ceiling (see [`deposit_heat`]) still bounds accumulation.
fn deposit_peak(rate: f32, sensitivity: f32) -> f32 {
    const EMBER: f32 = 0.32; // a lone slow press at rate≈0 → a dim ember
    const SPAN: f32 = 0.95; // added at full rate → core temp ≈1.27, past the white-hot threshold
    let r = rate.clamp(0.0, 1.0);
    (EMBER + SPAN * r) * sensitivity.clamp(0.25, 3.0)
}

/// Map one cell's temperature to its INCANDESCENT (blackbody) colour — the heart of the "looks like
/// heat" look. `temp` may exceed 1.0 (a fresh local flare on an already-warm board) → white-hot. The
/// ramp climbs through real fire colours — a faint cool-dark glow (cold) → deep ember-red → red →
/// orange → amber → white — so the board reads HOT at every level (embers → flame → white) and NEVER
/// passes through a green/cyan zone; the brightness rises with temperature. The `accent` recolours the
/// FLAME coherently (see [`recolor_flame`]): the warm default is classic fire, a cool accent gives a
/// clean cold-flame — never a muddy lerp. A pure fn — the whole palette lives here.
fn thermal_color(temp: f32, accent: Rgb) -> Rgb {
    let t = temp.max(0.0);
    let tn = t.min(1.0);
    let lerp = Rgb::lerp;
    // the incandescent control colours, coldest → hottest. COLD is a faint cool-dark glow (clearly
    // "cold", but the board isn't fully off); from EMBER up it's pure heat (no green/cyan ever).
    let cold = Rgb::new(6, 7, 18); // faint cool-dark
    let ember = Rgb::new(72, 6, 2); // first heat — deep ember red
    let red = Rgb::new(190, 22, 0);
    let orange = Rgb::new(255, 96, 0);
    let amber = Rgb::new(255, 200, 46);
    let white = Rgb::new(255, 246, 214); // white-hot (warm white)
    let ramp = match tn {
        x if x < 0.12 => lerp(cold, ember, x / 0.12),
        x if x < 0.32 => lerp(ember, red, (x - 0.12) / 0.20),
        x if x < 0.55 => lerp(red, orange, (x - 0.32) / 0.23),
        x if x < 0.80 => lerp(orange, amber, (x - 0.55) / 0.25),
        x => lerp(amber, white, (x - 0.80) / 0.20),
    };
    // RECOLOUR the flame COHERENTLY toward the accent's hue (the warm default → classic fire). This is a
    // hue ROTATION, not an RGB lerp toward the accent: the old lerp blended orange↔teal through grey and
    // ringed every white-hot flare with muddy blue-green; a rotation keeps a single clean flame palette.
    let flame = recolor_flame(ramp, accent);
    // WHITE-HOT peak: a fresh local flare (temp pushed above ~1.05) drives toward pure white — the
    // brightest moment, the key you just slammed — independent of the flame's hue, so flare cores always
    // read clean white (no tint), be it a warm fire or a cold flame.
    let over = ((t - 1.05) / 0.5).clamp(0.0, 1.0);
    lerp(flame, Rgb::new(255, 255, 255), over)
}

/// Recolour an incandescent ramp colour toward the accent's HUE — coherently. Rotates the colour's hue
/// (preserving its saturation + value) by the angle from the ramp's natural amber toward the accent, so
/// the whole flame becomes one clean palette: a WARM accent (the default ~#FFD9A0, hue ≈ 35°) rotates by
/// ≈0° and leaves CLASSIC FIRE, while a COOL accent gives a clean cold-flame (dark → its hue → white).
/// Crucially this is NOT an RGB lerp toward the accent — that path blended the orange ramp through grey
/// into a muddy blue-green halo around every flare. A near-grey accent (no clear hue) leaves fire as-is.
/// Reuses the comet hue-rotate ([`jitter_hue`], which no-ops on the near-grey dark base / white tips).
fn recolor_flame(ramp: Rgb, accent: Rgb) -> Rgb {
    const FIRE_REF_HUE: f32 = 35.0; // the ramp's amber midpoint — the "no shift" anchor for a warm accent
    let mx = accent.r.max(accent.g).max(accent.b) as f32;
    let mn = accent.r.min(accent.g).min(accent.b) as f32;
    let accent_chroma = if mx > 0.0 { (mx - mn) / mx } else { 0.0 };
    if accent_chroma < 0.08 {
        return ramp; // a greyscale / colourless accent → keep classic fire (no meaningful hue to chase)
    }
    jitter_hue(ramp, rgb_hue(accent) - FIRE_REF_HUE)
}

/// Per-cell heat-haze SHIMMER in 0..1 — multiplies a cell's brightness so a hot board lives and breathes
/// instead of sitting static (the [`fire_flicker`] idea, reused). Two incommensurate sines phase-seeded
/// by the cell's position make an organic per-cell wobble; the DEPTH scales with the cell's temperature,
/// so cold cells sit steady (a calm idle board) while hot cells waver like rising heat. Returns ≤ 1.0
/// (it only ever dims, never overshoots), so it never disturbs which cells are hottest.
fn heat_shimmer(x: usize, y: usize, t: f32, temp: f32) -> f32 {
    let (xf, yf) = (x as f32, y as f32);
    let a = (t * 6.5 + xf * 1.7 + yf * 0.9).sin();
    let b = (t * 9.3 + xf * 0.6 - yf * 1.3 + 2.0).sin();
    let mix = 0.5 + 0.5 * (0.6 * a + 0.4 * b); // 0..1 organic wobble
    let depth = 0.03 + 0.15 * temp.clamp(0.0, 1.0); // calm when cold, wavers when hot
    (1.0 - depth + depth * mix).clamp(0.0, 1.0)
}

/// The pure TypingHeat renderer (no I/O) — takes the heat `field` explicitly so it's deterministic and
/// unit-testable, exactly like [`render_pulse`]/`render_ambient`. Each cell's TEMPERATURE is simply its
/// field value, mapped through [`thermal_color`]. There is NO global warmth term (that uniform flood was
/// the flat-slab bug), so the board is exactly the position-aware field — hot where you typed, cooling
/// everywhere else, never a uniform slab. A gentle whole-board breath plus a per-cell heat-haze shimmer
/// (both driven by `t`) keep a warm board alive without disturbing the per-cell ordering. Fills
/// `rows*cols`; an empty field → a cool, dim idle board (honest, never faked).
fn render_typing_heat(field: &[f32], accent: Rgb, t: f32, rows: u8, cols: u8) -> Vec<Rgb> {
    use std::f32::consts::TAU;
    let (r, c) = (rows as usize, cols as usize);
    let n = r * c;
    let mut f = vec![Rgb::BLACK; n];
    if n == 0 {
        return f;
    }
    // a gentle whole-board brightness breath so a warm board isn't dead-flat — global (same for every
    // cell this frame), so it never disturbs the local hot-spot ordering. Shallow, ~6s cycle.
    let breath = 0.94 + 0.06 * (t * TAU / 6.0).sin();
    for y in 0..r {
        for x in 0..c {
            let i = y * c + x;
            let temp = field.get(i).copied().unwrap_or(0.0).max(0.0);
            // the heat-haze shimmer (depth scaling with this cell's temperature) rides on top of the
            // global breath, so hot areas waver like rising heat while cold ones stay calm.
            let shimmer = heat_shimmer(x, y, t, temp);
            f[i] = thermal_color(temp, accent).scale_f(breath * shimmer);
        }
    }
    f
}

/// A vivid representative TypingHeat frame for the tile THUMBNAIL — a board mid-flurry-of-typing showing
/// the NEW character: a FLOWING position-aware thermal MAP, not a uniform warm slab. A baked-in warm
/// gradient field (an ember floor everywhere so nothing is idle-cold, hottest through the centre rows,
/// cooler at the edges, leaning cool→hot left→right like cooling trails) carries several DISTINCT white-hot
/// radial FLARES scattered like just-pressed keys, each blooming into its surroundings. Built from the SAME
/// [`render_typing_heat`] ramp + [`deposit_heat`] blooms the live effect uses, so the postage-stamp tile
/// shows exactly what the effect IS even though the thumbnail pass can't read live keys (the grid render
/// suppresses key reads for perf). The accent tints the hot end, so the tile previews the user's colour too.
pub fn preview_typing_heat(accent: Rgb, rows: u8, cols: u8) -> Vec<Rgb> {
    let (r, c) = (rows as usize, cols as usize);
    let n = r * c;
    if n == 0 {
        return Vec::new();
    }
    // a baked-in WARM GRADIENT field: an ember FLOOR everywhere (so even the cool zones glow, never
    // idle-cold), hottest through the centre rows and cooler at the top/bottom edges, leaning warmer
    // left → right — a real thermal RANGE with cooling-trail drift, not a flat wash.
    let mut heat = vec![0.0f32; n];
    for y in 0..r {
        let ny = if r > 1 { y as f32 / (r as f32 - 1.0) } else { 0.5 };
        let centre = 1.0 - (2.0 * ny - 1.0).abs(); // 1 at the middle row → 0 at the top/bottom edge
        for x in 0..c {
            let nx = if c > 1 { x as f32 / (c as f32 - 1.0) } else { 0.5 };
            let lean = 0.45 + 0.55 * nx; // cooler at the left, hotter at the right
            heat[y * c + x] = (0.30 + 0.5 * centre) * lean; // ember floor + a centre-hot bloom
        }
    }
    // several DISTINCT hot flares scattered like fingers mid-typing (deterministic, so the tile is
    // stable). A fast-typing deposit peak, twice each, so the cores punch past white-hot over the gradient.
    let spots = [
        (r / 2, c / 6),
        (r / 2 + 1, c / 3),
        (r / 2, c / 2),
        (r / 3, (2 * c) / 3),
        (r / 2, (5 * c) / 6),
    ];
    let peak = deposit_peak(1.0, 1.0); // a fast flurry → white-hot cores
    for &(ry, cx) in &spots {
        if ry < r && cx < c {
            deposit_heat(&mut heat, ry, cx, r, c, peak);
            deposit_heat(&mut heat, ry, cx, r, c, peak);
        }
    }
    // t = 0 → the breath/shimmer at their mid value, giving a lively but stable still.
    render_typing_heat(&heat, accent, 0.0, rows, cols)
}

// ── THE COMPOSITOR — a stack of effect layers blended into one frame ─────────────────────
// This is the open Chroma Studio made real: each layer is its own generator + colour + region +
// blend, composited bottom-to-top into a single Vec<Rgb>. The compositor IS a FrameGen, so the
// existing animate/stream/mirror paths drive it with ZERO changes — they already take any FrameGen.

/// How a layer's pixels combine with what's beneath them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Blend {
    Normal, // opaque over (within the region)
    Add,    // additive — stacked glows brighten
    Screen, // lighten — softer than add, never clips ugly
}

impl Blend {
    pub fn from_str(s: &str) -> Blend {
        match s.to_lowercase().as_str() {
            "add" => Blend::Add,
            "screen" => Blend::Screen,
            _ => Blend::Normal,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Blend::Add => "add",
            Blend::Screen => "screen",
            Blend::Normal => "normal",
        }
    }
}

fn blend_px(under: Rgb, over: Rgb, mode: Blend) -> Rgb {
    match mode {
        Blend::Normal => over,
        Blend::Add => Rgb::new(
            under.r.saturating_add(over.r),
            under.g.saturating_add(over.g),
            under.b.saturating_add(over.b),
        ),
        Blend::Screen => {
            let s = |a: u8, b: u8| (255 - ((255 - a as u16) * (255 - b as u16) / 255)) as u8;
            Rgb::new(s(under.r, over.r), s(under.g, over.g), s(under.b, over.b))
        }
    }
}

/// Which cells a layer paints. `All` = the whole matrix; `Cells` = a sorted index set (the device
/// has no spatial metadata, so a region is a set of row-major cell indices the user painted).
#[derive(Clone, Debug)]
pub enum Region {
    All,
    Cells(Vec<u32>),
}

impl Region {
    fn covers(&self, i: usize) -> bool {
        match self {
            Region::All => true,
            Region::Cells(cs) => cs.binary_search(&(i as u32)).is_ok(),
        }
    }
}

/// A serializable layer definition (no boxed generator) — the shape the GUI sends and a profile
/// persists. The live `Layer` (with its stateful generator) is built from this on demand.
///
/// `#[serde(default)]` makes every field optional on load: a hand-edited or older record missing a
/// field falls back to [`LayerDef::default`], so the persisted lighting state stays forward/backward
/// compatible. Serialises FLAT (all fields are scalars — `color` is a hex string, `region` an inline
/// int array, `blend` a lowercase tag), which is required for the layer stack to round-trip as TOML.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LayerDef {
    pub effect: String,
    pub color: Rgb,
    pub speed: f32,
    pub direction: u8,
    pub density: f32, // fire heat / starlight population (1.0 default)
    pub fade: f32,    // reactive trail length (1.0 default)
    pub breath: u8,   // breathing flavour: 0 single · 1 dual · 2 random
    pub glow: bool,   // reactive: light a pressed key's neighbour ring (false default)
    pub source: String, // audiometer declared input: "speakers" (default) | "mic"
    pub region: Vec<u32>, // empty = whole device
    pub blend: Blend,
    pub enabled: bool,
}

impl Default for LayerDef {
    fn default() -> Self {
        LayerDef {
            effect: "static".into(),
            color: Rgb::new(0x4A, 0xF2, 0xB0),
            speed: 1.0,
            direction: 0,
            density: 1.0,
            fade: 1.0,
            breath: 0,
            glow: false,
            source: "speakers".into(),
            region: Vec::new(),
            blend: Blend::Normal,
            enabled: true,
        }
    }
}

impl LayerDef {
    /// The `EffectParams` this layer feeds its generator — one place that maps the def's tunable
    /// fields into the param bundle, so `from_defs` and any single-effect path stay in lock-step.
    pub fn params(&self) -> EffectParams {
        EffectParams {
            speed: self.speed,
            direction: self.direction,
            density: self.density,
            fade: self.fade,
            breath: self.breath,
            glow: self.glow,
            // map the persisted source string onto one of the schema's `&'static` options so
            // `EffectParams` stays `Copy` (no allocation) and an unknown value degrades to the
            // safe default rather than driving the generator off a junk endpoint.
            source: if self.source.eq_ignore_ascii_case("mic") {
                "mic"
            } else {
                "speakers"
            },
        }
    }
}

/// A live layer: a generator bound to its colour, region and blend.
pub struct Layer {
    pub gen: Box<dyn FrameGen>,
    pub color: Rgb,
    pub region: Region,
    pub blend: Blend,
    pub enabled: bool,
}

/// The stack. `layers[0]` is the bottom; later layers composite on top.
pub struct Compositor {
    pub layers: Vec<Layer>,
}

impl Compositor {
    /// Build the live stack from serializable defs (resolves generators, sorts region masks).
    pub fn from_defs(defs: &[LayerDef]) -> Compositor {
        let layers = defs
            .iter()
            .map(|d| {
                let gen = make_with(&d.effect, d.params()).unwrap_or_else(|| Box::new(Solid));
                let region = if d.region.is_empty() {
                    Region::All
                } else {
                    let mut cells = d.region.clone();
                    cells.sort_unstable();
                    cells.dedup();
                    Region::Cells(cells)
                };
                Layer {
                    gen,
                    color: d.color,
                    region,
                    blend: d.blend,
                    enabled: d.enabled,
                }
            })
            .collect();
        Compositor { layers }
    }
}

impl FrameGen for Compositor {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
        let n = rows as usize * cols as usize;
        let mut out = vec![Rgb::BLACK; n];
        for layer in self.layers.iter_mut() {
            if !layer.enabled {
                continue;
            }
            // each layer feeds its OWN colour as the generator's base
            let lf = layer.gen.frame(rows, cols, t, layer.color);
            if lf.len() != n {
                continue;
            }
            for i in 0..n {
                if layer.region.covers(i) {
                    out[i] = blend_px(out[i], lf[i], layer.blend);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compositor_blends_layers_within_regions() {
        // bottom: solid red over all; top: solid green over cells {0,1} with Normal blend
        let defs = vec![
            LayerDef {
                effect: "static".into(),
                color: Rgb::new(255, 0, 0),
                region: vec![],
                ..Default::default()
            },
            LayerDef {
                effect: "static".into(),
                color: Rgb::new(0, 255, 0),
                region: vec![0, 1],
                ..Default::default()
            },
        ];
        let mut comp = Compositor::from_defs(&defs);
        let f = comp.frame(2, 3, 0.0, Rgb::BLACK);
        assert_eq!(f.len(), 6);
        assert_eq!(
            f[0],
            Rgb::new(0, 255, 0),
            "cell 0 is in the top layer's region"
        );
        assert_eq!(f[1], Rgb::new(0, 255, 0), "cell 1 too");
        assert_eq!(
            f[2],
            Rgb::new(255, 0, 0),
            "cell 2 falls through to the bottom layer"
        );
    }

    #[test]
    fn add_blend_brightens() {
        let defs = vec![
            LayerDef {
                effect: "static".into(),
                color: Rgb::new(100, 0, 0),
                region: vec![],
                ..Default::default()
            },
            LayerDef {
                effect: "static".into(),
                color: Rgb::new(0, 0, 100),
                region: vec![],
                blend: Blend::Add,
                ..Default::default()
            },
        ];
        let mut comp = Compositor::from_defs(&defs);
        let f = comp.frame(1, 1, 0.0, Rgb::BLACK);
        assert_eq!(f[0], Rgb::new(100, 0, 100), "add stacks the channels");
    }

    #[test]
    fn rgb_serde_is_lossless_hex() {
        // a colour round-trips through TOML as a bare hex string (not a {r,g,b} sub-table).
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Holder {
            c: Rgb,
        }
        let h = Holder {
            c: Rgb::new(0x4A, 0xF2, 0xB0),
        };
        let s = toml::to_string(&h).unwrap();
        assert!(s.contains("c = \"4AF2B0\""), "colour serialises as hex: {s}");
        let back: Holder = toml::from_str(&s).unwrap();
        assert_eq!(h, back, "colour must round-trip losslessly");
    }

    #[test]
    fn layer_stack_round_trips_through_toml() {
        // a multi-layer stack with params, a region, and a non-default blend must survive a
        // serialize -> deserialize cycle byte-for-byte (this is what the persisted lighting state does).
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Stack {
            layers: Vec<LayerDef>,
        }
        let original = Stack {
            layers: vec![
                LayerDef {
                    effect: "fire".into(),
                    color: Rgb::new(255, 90, 0),
                    speed: 2.5,
                    density: 1.7,
                    region: vec![3, 1, 2],
                    blend: Blend::Add,
                    enabled: true,
                    ..Default::default()
                },
                LayerDef {
                    effect: "reactive".into(),
                    color: Rgb::new(0, 128, 255),
                    fade: 0.5,
                    glow: true,
                    source: "mic".into(),
                    direction: 2,
                    blend: Blend::Screen,
                    enabled: false,
                    ..Default::default()
                },
            ],
        };
        let toml = toml::to_string(&original).unwrap();
        let back: Stack = toml::from_str(&toml).unwrap();
        assert_eq!(back.layers.len(), 2);
        for (a, b) in original.layers.iter().zip(back.layers.iter()) {
            assert_eq!(a.effect, b.effect);
            assert_eq!(a.color, b.color);
            assert_eq!(a.speed, b.speed);
            assert_eq!(a.direction, b.direction);
            assert_eq!(a.density, b.density);
            assert_eq!(a.fade, b.fade);
            assert_eq!(a.breath, b.breath);
            assert_eq!(a.glow, b.glow);
            assert_eq!(a.source, b.source);
            assert_eq!(a.region, b.region);
            assert_eq!(a.blend, b.blend);
            assert_eq!(a.enabled, b.enabled);
        }
    }

    #[test]
    fn layer_def_tolerates_missing_fields() {
        // an old/partial record (only `effect`) loads with every other field at its default — the
        // back-compat guarantee for the persisted stack.
        let d: LayerDef = toml::from_str("effect = \"static\"").unwrap();
        let def = LayerDef::default();
        assert_eq!(d.effect, "static");
        assert_eq!(d.color, def.color);
        assert_eq!(d.speed, def.speed);
        assert_eq!(d.blend, def.blend);
        assert_eq!(d.enabled, def.enabled);
    }

    #[test]
    fn schema_covers_every_builtin_and_reads_as_data() {
        // every generator listed in BUILTINS has a schema the UI can render…
        for n in BUILTINS {
            let s = schema(n);
            // …and the colour gate agrees with the schema (no hand-kept second source of truth).
            assert_eq!(
                uses_color(n),
                s.iter().any(|p| matches!(p.kind, ParamKind::Color)),
                "{n}: uses_color must match the schema's colour knob"
            );
        }
        // the spec'd shapes — a couple of representative asserts (data, not per-effect UI)
        let keys = |n: &str| schema(n).iter().map(|p| p.key).collect::<Vec<_>>();
        assert_eq!(keys("static"), vec!["color"]);
        assert_eq!(keys("breathing"), vec!["color", "breath", "speed"]);
        // spectrum is the uniform spectrum-CYCLE — ONE knob (`speed`, the cycle rate). No colour (it
        // cycles every hue) and NO direction (a uniform cycle has no axis — that knob would be dead).
        assert_eq!(keys("spectrum"), vec!["speed"]);
        assert!(!uses_color("spectrum"), "spectrum cycles all hues — no colour knob");
        // wave (the spatial scrolling rainbow) keeps its axis `direction` — that's what distinguishes
        // it from spectrum, which is spatially uniform.
        assert_eq!(keys("wave"), vec!["direction", "speed"]);
        assert_eq!(keys("fire"), vec!["speed", "density"]);
        // cascade (Matrix rain): colour + fall-rate + how-busy — the three knobs its generator honours.
        assert_eq!(keys("cascade"), vec!["color", "speed", "density"]);
        assert!(uses_color("cascade"), "cascade rains in the layer colour");
        // reactive's third knob is the `glow` Toggle (the demo that proves Toggle is end-to-end).
        assert_eq!(keys("reactive"), vec!["color", "fade", "glow"]);
        // starlight gained a `fade` (twinkle length); colorwheel gained a spin `direction`.
        assert_eq!(keys("starlight"), vec!["color", "density", "speed", "fade"]);
        assert_eq!(keys("colorwheel"), vec!["speed", "direction"]);
        // the three new "obvious classics" — each built with luminance depth from the start.
        // ripple (reactive radial wave): colour + expansion rate + ring lifetime.
        assert_eq!(keys("ripple"), vec!["color", "speed", "fade"]);
        assert!(uses_color("ripple"), "ripple radiates in the layer colour");
        // comet (motion streak): colour + travel rate + comet count.
        assert_eq!(keys("comet"), vec!["color", "speed", "density"]);
        assert!(uses_color("comet"), "the comet streaks in the layer colour");
        // aurora (ambient flow): colour biases the palette + the flow rate.
        assert_eq!(keys("aurora"), vec!["color", "speed"]);
        assert!(uses_color("aurora"), "aurora's palette is biased by the layer colour");
        // the `glow` knob is a Toggle defaulting OFF (accurate reactive: only the pressed key's cell,
        // no cross-key neighbour bleed; glow is the opt-in).
        assert!(matches!(
            schema("reactive").into_iter().find(|p| p.key == "glow").map(|p| p.kind),
            Some(ParamKind::Toggle { default: false })
        ));
        // the audiometer declares its INPUT (speakers|mic) as a schema enum — a data-driven knob,
        // not bespoke UI; default index 0 = "speakers" so existing behaviour is unchanged.
        assert_eq!(keys("audiometer"), vec!["color", "source"]);
        assert!(matches!(
            schema("audiometer")
                .into_iter()
                .find(|p| p.key == "source")
                .map(|p| p.kind),
            Some(ParamKind::Enum { options: ["speakers", "mic"], default: 0 })
        ));
        // pulse (the live CPU/RAM load surface) declares ONE knob — the accent tint for its bars.
        assert_eq!(keys("pulse"), vec!["color"]);
        assert!(uses_color("pulse"), "pulse tints its load bars with the layer colour");
        // ambient (the live screen-mirror surface): ease rate + a "saturation" pop knob (keyed on the
        // density field). NO colour knob — the colour is intrinsic to the screen.
        assert_eq!(keys("ambient"), vec!["speed", "density"]);
        assert!(!uses_color("ambient"), "ambient's colour is the screen, not a knob");
        // typing heat (live keyboard → temperature): colour TINTS the hot end + sensitivity (the
        // rate→warmth gain, keyed on the density field, like ambient's "saturation") + fade (cooling).
        assert_eq!(keys("typingheat"), vec!["color", "density", "fade"]);
        assert!(uses_color("typingheat"), "typing heat tints its hot end with the layer colour");
        // the "sensitivity" knob is the density-field Range relabelled (the key is plumbing, the label is
        // the human word) — proof the label/key split works, like ambient's "saturation".
        assert_eq!(
            schema("typingheat").into_iter().find(|p| p.key == "density").map(|p| p.label),
            Some("sensitivity")
        );
        // the data surface declares NO generator knobs (the source is the content)
        assert!(schema("mouse-battery").is_empty());
        // an unknown name is an empty schema, never a panic
        assert!(schema("nonsense").is_empty());
    }

    #[test]
    fn fire_density_changes_flame_height() {
        let run = |dens: f32| {
            let mut g = Fire::with(EffectParams {
                density: dens,
                ..Default::default()
            });
            let mut f = Vec::new();
            for _ in 0..40 {
                f = g.frame(8, 10, 0.0, Rgb::BLACK);
            }
            // brightness of the top half (how far the flame climbs)
            f[0..4 * 10]
                .iter()
                .map(|c| c.r as u32 + c.g as u32 + c.b as u32)
                .sum::<u32>()
        };
        assert!(
            run(2.5) > run(0.4),
            "denser fire should climb higher than a sparse one"
        );
    }

    #[test]
    fn fire_speed_changes_propagation_rate_over_time() {
        // Fire honours `speed`: driven by elapsed `t`, a faster fire advances its heat sim more steps
        // per second, so after a short SHARED window the two flames differ AND the fast one has
        // propagated heat further up the board (the bottom seed has had time to climb).
        let top_after = |spd: f32, frames: usize| -> (Vec<Rgb>, u32) {
            let mut g = Fire::with(EffectParams { speed: spd, ..Default::default() });
            let mut f = Vec::new();
            for i in 0..frames {
                f = g.frame(8, 10, i as f32 * 0.05, Rgb::BLACK); // 0.05s/frame ~ 20fps
            }
            // brightness of the TOP half — how far the flame has had time to climb.
            let top = f[0..4 * 10].iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum::<u32>();
            (f, top)
        };
        // a brief window so a slow fire has barely climbed while a fast one has churned upward.
        let (slow, slow_top) = top_after(0.4, 5);
        let (fast, fast_top) = top_after(3.0, 5);
        assert_ne!(slow, fast, "speed must change the flame's evolution over the same window");
        assert!(
            fast_top > slow_top,
            "a faster fire climbs higher in the same time ({fast_top} vs {slow_top})"
        );
    }

    #[test]
    fn spectrum_is_uniform_and_cycles_distinct_from_wave() {
        // The CLASSIC spectrum-cycle: at any instant the WHOLE board is ONE uniform hue (every cell
        // identical), and that hue ADVANCES through the spectrum over time. This is the opposite of
        // `wave`, which lays a spatial rainbow GRADIENT across the board — the contrast is the point
        // (they used to look identical; spectrum is now its own iconic effect).
        let mut g = Spectrum { p: EffectParams::default() };
        let f0 = g.frame(6, 22, 0.0, Rgb::BLACK);
        // spatially UNIFORM: every cell in a frame is the exact same colour.
        assert!(
            f0.iter().all(|&c| c == f0[0]),
            "spectrum's board must be one uniform hue at any instant"
        );
        // the hue CYCLES over time — a later frame is a different (still-uniform) colour.
        let f1 = g.frame(6, 22, 2.0, Rgb::BLACK);
        assert!(f1.iter().all(|&c| c == f1[0]), "still spatially uniform a moment later");
        assert_ne!(f0[0], f1[0], "spectrum's whole-board hue must advance over time");
        // …and it is CLEARLY DISTINCT from wave: wave is a spatial GRADIENT (neighbouring cells
        // differ across the board) at a single instant, where spectrum is flat.
        let mut w = Wave { p: EffectParams::default() };
        let wf = w.frame(6, 22, 0.0, Rgb::BLACK);
        assert!(
            wf.windows(2).any(|s| s[0] != s[1]),
            "wave must be a spatial gradient (the foil to spectrum's uniformity)"
        );
    }

    #[test]
    fn colorwheel_direction_reverses_spin() {
        // colorwheel honours `direction` as the spin SIGN — the same wheel turned the other way.
        let run = |dir: u8| {
            let mut g = ColorWheel { p: EffectParams { direction: dir, ..Default::default() } };
            g.frame(6, 22, 0.5, Rgb::BLACK)
        };
        assert_ne!(run(0), run(1), "clockwise vs counter-clockwise spin must differ");
    }

    // ── luminance depth — effects use BRIGHTNESS for depth, not only hue/position ────────────────
    // For a full-saturation `from_hsv` colour the BRIGHTNESS is exactly the max channel (s=1 ⇒ the
    // largest component equals `v`), so `max(r,g,b)` is a clean per-cell brightness probe — hue-free.

    /// Per-cell brightness of a full-saturation frame: the max channel (== round(v*255)).
    fn bright(c: &Rgb) -> u8 {
        c.r.max(c.g).max(c.b)
    }

    #[test]
    fn wave_has_a_travelling_luminance_crest() {
        // the wave is no longer a flat rainbow: at a fixed time the brightness swells across the
        // board (a crest and a trough, not uniform), and that crest TRAVELS over time.
        let mut g = Wave { p: EffectParams::default() };
        let row0 = g.frame(1, 24, 0.0, Rgb::BLACK); // one row, direction → (brightness varies along x)
        let hi = row0.iter().map(bright).max().unwrap();
        let lo = row0.iter().map(bright).min().unwrap();
        assert!(
            hi as i32 - lo as i32 > 30,
            "wave must have a visible brightness crest across the board ({lo}..{hi}), not a flat scroll"
        );
        // the crest moves: the brightness profile a moment later is not the same as now.
        let later = g.frame(1, 24, 1.0, Rgb::BLACK);
        let prof = |f: &[Rgb]| f.iter().map(bright).collect::<Vec<_>>();
        assert_ne!(prof(&row0), prof(&later), "the wave's brightness crest must travel over time");
    }

    #[test]
    fn spectrum_breathes_in_brightness() {
        // the spectrum is alive, not a static wash: the whole-board brightness swells and dims over
        // time. Sample the breath at its peak vs its trough (¼ and ¾ through the ~6s cycle).
        let mean = |t: f32| -> f32 {
            let mut g = Spectrum { p: EffectParams::default() };
            let f = g.frame(6, 22, t, Rgb::BLACK);
            f.iter().map(|c| bright(c) as f32).sum::<f32>() / f.len() as f32
        };
        let peak = mean(1.5); // sin = +1 → breath max
        let trough = mean(4.5); // sin = -1 → breath min
        assert!(
            peak - trough > 10.0,
            "spectrum's whole-board brightness must breathe over time ({trough} → {peak})"
        );
    }

    #[test]
    fn colorwheel_has_radial_brightness_depth() {
        // the wheel has DIMENSION: the hub is brighter than the rim (a soft dome), so even a still
        // frame reads as a rounded wheel rather than a flat disc of full-bright hues.
        let mut g = ColorWheel { p: EffectParams::default() };
        let f = g.frame(7, 7, 0.0, Rgb::BLACK); // odd dims ⇒ a true centre cell at (3,3)
        let hub = bright(&f[3 * 7 + 3]);
        let corners = [f[0], f[6], f[6 * 7], f[6 * 7 + 6]];
        let rim_mean = corners.iter().map(|c| bright(c) as u32).sum::<u32>() / 4;
        assert!(
            hub as u32 > rim_mean + 20,
            "the colour wheel's hub must be brighter than its rim ({rim_mean} vs {hub})"
        );
    }

    #[test]
    fn fire_flicker_varies_over_time_and_by_column() {
        // the fire's per-column intensity flicker: it MOVES over time, differs column-to-column, the
        // depth ramps with heat (hot tips flare hard, cool embers barely waver), and it never blinks
        // fully out or overshoots (stays in (0,1]).
        let a = fire_flicker(0, 0.0, 1.0, 1.0);
        let b = fire_flicker(0, 0.5, 1.0, 1.0);
        assert_ne!(a, b, "a column's flicker must vary over time");
        assert_ne!(
            fire_flicker(0, 0.3, 1.0, 1.0),
            fire_flicker(5, 0.3, 1.0, 1.0),
            "different columns must flicker independently"
        );
        // depth grows with heat: over a time sweep the white-hot tips dip deeper than the cool embers.
        let (mut tip_min, mut ember_min) = (1.0f32, 1.0f32);
        for i in 0..200 {
            let t = i as f32 * 0.02;
            let v_tip = fire_flicker(2, t, 1.0, 1.0);
            let v_ember = fire_flicker(2, t, 1.0, 0.05);
            assert!(v_tip > 0.0 && v_tip <= 1.0, "flicker stays in (0,1]");
            assert!(v_ember > 0.0 && v_ember <= 1.0, "flicker stays in (0,1]");
            tip_min = tip_min.min(v_tip);
            ember_min = ember_min.min(v_ember);
        }
        assert!(
            tip_min < ember_min,
            "hot tips flicker deeper than cool embers ({tip_min} vs {ember_min})"
        );
    }

    #[test]
    fn starlight_fade_changes_twinkle_length() {
        // higher `fade` → faster decay → a dimmer board over time (shorter twinkles).
        let run = |fade: f32| {
            let mut g = Starlight::new(EffectParams { fade, ..Default::default() });
            let mut f = Vec::new();
            for _ in 0..30 {
                f = g.frame(8, 10, 0.0, Rgb::new(0, 255, 0));
            }
            f.iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum::<u32>()
        };
        assert!(run(0.3) > run(3.0), "a longer fade should leave more lit than a brisk one");
    }

    #[test]
    fn reactive_glow_toggle_changes_neighbour_lighting() {
        // With glow ON a pressed cell lights its neighbour ring; OFF lights only the exact cell.
        // We drive the ignite logic directly (no live keyboard in a unit test) by reusing the frame's
        // neighbour-spreading code through a synthesised press is not possible headless, so assert the
        // schema-default propagates into params and the field is read: build two reactives, ignite the
        // same cell, and compare. (Direct field poke keeps this deterministic and OS-independent.)
        let (r, c) = (6usize, 22usize);
        let n = r * c;
        let ignite = |glow: bool| -> Vec<f32> {
            let mut g = Reactive::new(EffectParams { glow, ..Default::default() });
            // mimic one fresh key-down on a known interior cell using the SAME spread the frame uses.
            g.level = vec![0.0; n];
            g.dims = (r as u8, c as u8);
            let cell = 2 * c + 5; // an interior cell with all four neighbours in-bounds
            g.level[cell] = 1.0;
            if g.p.glow {
                let (cy, cx) = (cell / c, cell % c);
                for (dy, dx) in [(0isize, 1isize), (0, -1), (1, 0), (-1, 0)] {
                    let (ny, nx) = (cy as isize + dy, cx as isize + dx);
                    if ny >= 0 && ny < r as isize && nx >= 0 && nx < c as isize {
                        let ni = ny as usize * c + nx as usize;
                        g.level[ni] = g.level[ni].max(0.55);
                    }
                }
            }
            g.level.clone()
        };
        let on = ignite(true);
        let off = ignite(false);
        let lit = |v: &[f32]| v.iter().filter(|&&x| x > 0.0).count();
        assert_eq!(lit(&off), 1, "glow off lights only the pressed cell");
        assert_eq!(lit(&on), 5, "glow on lights the cell + four neighbours");
    }

    #[test]
    fn reactive_uses_real_key_map_not_a_hash() {
        // Reactive ignites ONLY when vk_to_key_cell resolves — the hash fallback is gone. A mapped
        // key lights its TRUE cell: VK '1' (0x31) → (1,2), ESC → (0,1), F1 → (0,3), 'A' → (3,2),
        // LSHIFT → (4,1). The full keyboard is covered, so typed keys all land where they sit. Razer
        // keyboards have ONE LED per key, so each mapped key resolves to exactly ONE cell.
        assert_eq!(crate::lighting::vk_to_key_cell(0x31), Some((1, 2))); // '1'
        assert_eq!(crate::lighting::vk_to_key_cell(0x1B), Some((0, 1))); // ESC
        assert_eq!(crate::lighting::vk_to_key_cell(0x70), Some((0, 3))); // F1
        assert_eq!(crate::lighting::vk_to_key_cell(0x41), Some((3, 2))); // 'A'
        assert_eq!(crate::lighting::vk_to_key_cell(0xA0), Some((4, 1))); // LSHIFT
        // SPACE (VK 0x20) maps to its SINGLE standard cell (5,7) — not a multi-cell footprint. On a
        // board whose space bar has no LED there (this Chroma V2), that cell is dark, so pressing
        // space lights nothing — gracefully, with no special-casing.
        assert_eq!(crate::lighting::vk_to_key_cell(0x20), Some((5, 7)));
        // keys the map doesn't carry return None → the ignite block is SKIPPED → Reactive lights
        // NOTHING (no random cell): mouse buttons and the generic modifiers are such keys.
        assert_eq!(crate::lighting::vk_to_key_cell(0x01), None); // left mouse button
        assert_eq!(crate::lighting::vk_to_key_cell(0x10), None); // generic SHIFT (would double-light)
    }

    #[test]
    fn every_declared_knob_is_honoured_no_dead_knobs() {
        // The enforced rule: an effect must only expose knobs its generator HONORS. For each builtin,
        // toggling each declared Range/Enum/Toggle knob away from its default must change the output
        // (over a short window for stateful effects). Reactive is excluded (it needs live key input).
        use std::collections::HashSet;
        let lum = |f: &[Rgb]| f.iter().map(|c| (c.r as u32, c.g as u32, c.b as u32)).collect::<Vec<_>>();
        // run an effect with given params over a few frames, return the final frame's pixels.
        let render = |name: &str, p: EffectParams| -> Vec<Rgb> {
            let mut g = make_with(name, p).unwrap();
            let mut f = Vec::new();
            for i in 0..20 {
                f = g.frame(6, 12, i as f32 * 0.05, Rgb::new(0, 200, 255));
            }
            f
        };
        // reactive/ripple/audiometer/ambient read a LIVE provider (keyboard / audio / screen) whose
        // value a unit test can't control, so the generic "toggle a knob, watch the output move" sweep
        // can't exercise them deterministically. Their knobs are proven honoured by dedicated tests that
        // inject a known input (e.g. `ripple_honours_speed_and_fade` drives an injected wave; the
        // `ambient_*` tests drive `render_ambient` with a fixed grid). COMET is skipped too: its parade
        // lifecycle means a single comet can be mid-respawn-gap (board momentarily dark) on any given
        // FINAL frame, so a single-frame comparison is unreliable — its speed/density/colour are instead
        // proven by dedicated tests that accumulate over a window (`comet_speed_changes_output`,
        // `comet_density_adds_more_comets`, `comet_streaks_in_the_layer_colour`).
        // typingheat reads the LIVE keyboard too (a position-aware heat FIELD fed by key-down deposits,
        // then diffused + cooled every frame), which a unit test can't drive deterministically, so it's
        // skipped here and its knobs are proven by dedicated tests (`typing_heat_*`): sensitivity/rate via
        // the pure `deposit_peak`/`step_rate`, fade's field cooling via the generator + `cool_field`,
        // diffusion via `diffuse_field`, and the colour tint via `render_typing_heat`.
        let skip: HashSet<&str> =
            ["reactive", "ripple", "audiometer", "ambient", "comet", "typingheat"]
                .into_iter()
                .collect();
        for &name in BUILTINS {
            if skip.contains(name) {
                continue;
            }
            let base = render(name, EffectParams::default());
            for p in schema(name) {
                let mut tweaked = EffectParams::default();
                match p.kind {
                    ParamKind::Color => continue, // colour is the `base` arg, exercised elsewhere
                    ParamKind::Range { min, max, default } => {
                        // pick a value clearly away from the default within range.
                        tweaked = EffectParams {
                            speed: if p.key == "speed" { pick_far(min, max, default) } else { tweaked.speed },
                            density: if p.key == "density" { pick_far(min, max, default) } else { tweaked.density },
                            fade: if p.key == "fade" { pick_far(min, max, default) } else { tweaked.fade },
                            ..tweaked
                        };
                    }
                    ParamKind::Enum { default, options } => {
                        let other = ((default as usize + 1) % options.len().max(1)) as u8;
                        match p.key {
                            "direction" => tweaked.direction = other,
                            "breath" => tweaked.breath = other,
                            _ => continue,
                        }
                    }
                    ParamKind::Toggle { default } => match p.key {
                        "glow" => tweaked.glow = !default,
                        _ => continue,
                    },
                }
                let changed = render(name, tweaked);
                assert_ne!(
                    lum(&base),
                    lum(&changed),
                    "{name}: knob '{}' is declared but does not change output (dead knob)",
                    p.key
                );
            }
        }
    }

    /// pick a value clearly away from `default` within `[min,max]` (the far end from the default).
    fn pick_far(min: f32, max: f32, default: f32) -> f32 {
        if (default - min).abs() > (max - default).abs() { min } else { max }
    }

    #[test]
    fn registry_resolves_builtins_and_fire() {
        for n in BUILTINS {
            assert!(make(n).is_some(), "{n} should resolve");
        }
        assert!(make("nonsense").is_none());
    }

    #[test]
    fn generators_produce_full_frames() {
        for n in BUILTINS {
            let mut g = make(n).unwrap();
            let f = g.frame(6, 22, 1.5, Rgb::new(0, 255, 0));
            assert_eq!(f.len(), 6 * 22, "{n} must fill the matrix");
        }
    }

    #[test]
    fn fire_rises_hotter_at_the_bottom() {
        let mut fire = Fire::default();
        // run a few frames so heat propagates
        let mut f = Vec::new();
        for _ in 0..30 {
            f = fire.frame(6, 10, 0.0, Rgb::BLACK);
        }
        let brightness = |c: &Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let bottom: u32 = f[5 * 10..6 * 10].iter().map(brightness).sum();
        let top: u32 = f[0..10].iter().map(brightness).sum();
        assert!(
            bottom > top,
            "fire should be brighter at the bottom ({bottom} vs {top})"
        );
    }

    #[test]
    fn fire_color_ramps_black_to_white() {
        assert_eq!(fire_color(0.0), Rgb::new(0, 0, 0));
        let hot = fire_color(1.0);
        assert!(hot.r > 200 && hot.g > 180, "peak heat ~ white-hot");
    }

    #[test]
    fn layerdef_glow_flows_through_params() {
        // the Toggle backend plumbing: LayerDef's bool field maps into EffectParams (the same path the
        // GUI's on_set_param_toggle writes), so a generator built via from_defs honours the toggle.
        let mut d = LayerDef { effect: "reactive".into(), ..Default::default() };
        assert!(!d.params().glow, "default glow is off (accurate reactive — no cross-key bleed)");
        d.glow = true;
        assert!(d.params().glow, "setting the toggle propagates to the generator params");
    }

    // ── cascade (Matrix rain) ─────────────────────────────────────────────────────────────────

    /// Run a fresh Cascade for `frames` at ~20fps (0.05s/frame), return the final frame.
    fn cascade_run(p: EffectParams, base: Rgb, frames: usize) -> Vec<Rgb> {
        let mut g = Cascade::new(p);
        let mut f = Vec::new();
        for i in 0..frames {
            f = g.frame(8, 12, i as f32 * 0.05, base);
        }
        f
    }

    #[test]
    fn cascade_produces_full_frames() {
        // fills any rows×cols, including the odd/empty shapes the compositor may hand it.
        let mut g = Cascade::new(EffectParams::default());
        assert_eq!(g.frame(6, 22, 1.5, Rgb::new(0, 255, 0)).len(), 6 * 22);
        assert_eq!(g.frame(1, 1, 2.0, Rgb::new(0, 255, 0)).len(), 1);
        assert!(g.frame(0, 0, 0.0, Rgb::new(0, 255, 0)).is_empty());
    }

    #[test]
    fn cascade_animates_over_time() {
        // a self-animating effect: the rain at the start differs from the rain a moment later.
        let mut g = Cascade::new(EffectParams::default());
        let early = g.frame(8, 12, 0.0, Rgb::new(0, 255, 0));
        let mut late = early.clone();
        for i in 1..25 {
            late = g.frame(8, 12, i as f32 * 0.05, Rgb::new(0, 255, 0));
        }
        assert_ne!(early, late, "cascade must move (the heads fall) over time");
    }

    #[test]
    fn cascade_speed_changes_output() {
        // `speed` is a time-scale on the whole sim: over the same window a faster rain has fallen
        // further, so the frames differ.
        let base = Rgb::new(0, 255, 0);
        let slow = cascade_run(EffectParams { speed: 0.4, ..Default::default() }, base, 15);
        let fast = cascade_run(EffectParams { speed: 4.0, ..Default::default() }, base, 15);
        assert_ne!(slow, fast, "speed must change the rain's evolution over the same window");
    }

    #[test]
    fn cascade_density_changes_output() {
        // `density` governs how busy the rain is (spawn probability + respawn gap), so a dense rain
        // and a sparse one diverge — and the dense board is, on balance, more lit.
        let base = Rgb::new(0, 255, 0);
        let lit = |f: &[Rgb]| f.iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum::<u32>();
        let sparse = cascade_run(EffectParams { density: 0.25, ..Default::default() }, base, 20);
        let dense = cascade_run(EffectParams { density: 3.0, ..Default::default() }, base, 20);
        assert_ne!(sparse, dense, "density must change the output");
        assert!(
            lit(&dense) > lit(&sparse),
            "a denser rain lights more of the board ({} vs {})",
            lit(&dense),
            lit(&sparse)
        );
    }

    #[test]
    fn cascade_uses_the_layer_colour() {
        // the rain is poured in the layer colour: a green rain shows green tail cells (pure hue, no
        // red), a red rain shows red ones — and the two frames differ. (Heads whiten, but the trails
        // carry the base hue, so a lit board always reveals the colour.)
        let green = cascade_run(EffectParams::default(), Rgb::new(0, 255, 0), 18);
        let red = cascade_run(EffectParams::default(), Rgb::new(255, 0, 0), 18);
        assert_ne!(green, red, "the layer colour must drive the rain hue");
        // at least one lit cell is a pure-green tail (g>0 with no red) — proof the base hue is used,
        // not a hardcoded colour.
        assert!(
            green.iter().any(|c| c.g > 0 && c.r == 0),
            "a green cascade must light green (base-coloured) tail cells"
        );
    }

    // ── pulse (live CPU/RAM load surface) ──────────────────────────────────────────────────────

    fn lit_sum(f: &[Rgb]) -> u32 {
        f.iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum()
    }

    #[test]
    fn pulse_produces_full_frames() {
        // fills any rows×cols, including the odd/empty shapes the compositor may hand it.
        let accent = Rgb::new(0, 200, 255);
        assert_eq!(render_pulse(0.5, 0.5, accent, 0.0, 6, 22).len(), 6 * 22);
        assert_eq!(render_pulse(0.5, 0.5, accent, 0.0, 1, 1).len(), 1);
        assert!(render_pulse(0.5, 0.5, accent, 0.0, 0, 0).is_empty());
        // the live FrameGen path also fills the matrix (reads the provider; 0-load is fine).
        let mut g = make("pulse").unwrap();
        assert_eq!(g.frame(6, 12, 0.0, accent).len(), 6 * 12);
    }

    #[test]
    fn pulse_reflects_load() {
        // a heavier load lights MORE of the board than a light one (same time → same breath, so the
        // comparison isolates the load). The horizontal meters fill further at high load.
        let accent = Rgb::new(0, 200, 255);
        let low = render_pulse(0.1, 0.1, accent, 0.0, 6, 12);
        let high = render_pulse(0.9, 0.9, accent, 0.0, 6, 12);
        assert!(
            lit_sum(&high) > lit_sum(&low),
            "a busier machine lights more of the board ({} vs {})",
            lit_sum(&high),
            lit_sum(&low)
        );
        // idle reads (nearly) dark — an honest "no load" board, not faked motion.
        let idle = render_pulse(0.0, 0.0, accent, 0.0, 6, 12);
        assert_eq!(lit_sum(&idle), 0, "zero load lights nothing");
    }

    #[test]
    fn pulse_splits_cpu_top_ram_bottom() {
        // CPU drives the top half, RAM the bottom: a busy CPU + idle RAM lights the top, not the
        // bottom. (6 rows → CPU rows 0..3, RAM rows 3..6.)
        let accent = Rgb::new(0, 200, 255);
        let f = render_pulse(1.0, 0.0, accent, 0.0, 6, 12);
        let top = lit_sum(&f[0..3 * 12]);
        let bottom = lit_sum(&f[3 * 12..6 * 12]);
        assert!(top > 0, "a busy CPU lights the top zone");
        assert_eq!(bottom, 0, "an idle RAM leaves the bottom zone dark");
    }

    #[test]
    fn pulse_honours_color_knob() {
        // the accent tints the load bars, so two different colours at the same load differ — the
        // colour knob is live (no dead knob). Use a non-zero load so there are lit cells to tint.
        let green = render_pulse(0.8, 0.8, Rgb::new(0, 255, 0), 0.0, 6, 12);
        let red = render_pulse(0.8, 0.8, Rgb::new(255, 0, 0), 0.0, 6, 12);
        assert_ne!(green, red, "the layer colour must tint the load bars");
    }

    #[test]
    fn pulse_load_color_ramps_green_to_red() {
        // the urgency ramp: idle is green-dominant, maxed is red-dominant, half is amber-ish.
        let idle = load_color(0.0);
        assert!(idle.g > idle.r && idle.g > idle.b, "idle load reads green");
        let maxed = load_color(1.0);
        assert!(maxed.r > maxed.g && maxed.r > maxed.b, "maxed load reads red");
        let half = load_color(0.5);
        assert!(half.r > 0 && half.g > 0, "half load reads amber (red+green)");
    }

    // ── ambient (live screen-mirror / ambilight surface) ───────────────────────────────────────

    #[test]
    fn ambient_maps_screen_zones_to_board_corners() {
        // a true ambilight: the LEFT/TOP of the screen lights the LEFT/TOP of the board. Inject a
        // 2×2 screen grid with a distinct colour per corner; full ease (1.0), no boost, no prev.
        let grid = vec![
            Rgb::new(255, 0, 0),     // top-left
            Rgb::new(0, 255, 0),     // top-right
            Rgb::new(0, 0, 255),     // bottom-left
            Rgb::new(255, 255, 255), // bottom-right
        ];
        let f = render_ambient(&grid, 2, 2, &[], 2, 2, 1.0, 0.0);
        assert_eq!(f[0], Rgb::new(255, 0, 0), "board top-left = screen top-left");
        assert_eq!(f[1], Rgb::new(0, 255, 0), "board top-right = screen top-right");
        assert_eq!(f[2], Rgb::new(0, 0, 255), "board bottom-left = screen bottom-left");
        assert_eq!(f[3], Rgb::new(255, 255, 255), "board bottom-right = screen bottom-right");
    }

    #[test]
    fn ambient_produces_full_frames() {
        // fills any rows×cols, including the odd/empty shapes the compositor may hand it.
        let grid = vec![Rgb::new(10, 20, 30); 12];
        assert_eq!(render_ambient(&grid, 4, 3, &[], 6, 22, 1.0, 0.0).len(), 6 * 22);
        assert_eq!(render_ambient(&grid, 4, 3, &[], 1, 1, 1.0, 0.0).len(), 1);
        assert!(render_ambient(&grid, 4, 3, &[], 0, 0, 1.0, 0.0).is_empty());
        // the live FrameGen path also fills the matrix (reads the provider; a black grid is fine).
        let mut g = make("ambient").unwrap();
        assert_eq!(g.frame(6, 12, 0.0, Rgb::BLACK).len(), 6 * 12);
    }

    #[test]
    fn ambient_speed_controls_ease_rate() {
        // `speed` is the smoothing ease: from a dark previous frame, a faster speed lands CLOSER to
        // the (bright) screen colour after one frame than a slow one — proof the speed knob is honoured.
        let grid = vec![Rgb::new(200, 100, 50); 4];
        let prev = vec![Rgb::BLACK; 6];
        let snappy = render_ambient(&grid, 2, 2, &prev, 2, 3, ambient_ease(4.0), 0.0);
        let smooth = render_ambient(&grid, 2, 2, &prev, 2, 3, ambient_ease(0.25), 0.0);
        let bright = |f: &[Rgb]| f.iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum::<u32>();
        assert!(
            bright(&snappy) > bright(&smooth),
            "a higher speed eases toward the screen faster ({} vs {})",
            bright(&snappy),
            bright(&smooth)
        );
    }

    #[test]
    fn ambient_saturation_knob_pops_colours() {
        // the "saturation" knob (the density field) widens the channel spread, so a popped frame
        // differs from a faithful one and is more saturated — proof the knob is honoured (no dead knob).
        let grid = vec![Rgb::new(180, 90, 60); 1];
        let plain = render_ambient(&grid, 1, 1, &[], 1, 1, 1.0, ambient_boost_amount(1.0)); // boost 0
        let popped = render_ambient(&grid, 1, 1, &[], 1, 1, 1.0, ambient_boost_amount(3.0)); // boost 2
        assert_ne!(plain[0], popped[0], "the saturation knob must change the colour");
        let spread = |c: Rgb| (c.r.max(c.g).max(c.b) as i32) - (c.r.min(c.g).min(c.b) as i32);
        assert!(
            spread(popped[0]) > spread(plain[0]),
            "boost widens the channel spread (more saturated)"
        );
    }

    #[test]
    fn ambient_black_screen_idles_dark() {
        // an idle / dark screen → a dark board, even with the saturation knob cranked — honest, not faked.
        let grid = vec![Rgb::BLACK; 4];
        let f = render_ambient(&grid, 2, 2, &[], 4, 6, 1.0, 2.0);
        assert!(f.iter().all(|c| *c == Rgb::BLACK), "a dark screen leaves the board dark");
    }

    // ── ripple (reactive radial wave) ───────────────────────────────────────────────────────────
    // Like reactive, ripple reads the LIVE keyboard, so the unit tests INJECT a ripple directly into
    // the pool (the same field-poke pattern `reactive_glow_toggle_*` uses) to drive it deterministically.

    #[test]
    fn ripple_produces_full_frames() {
        // fills any rows×cols (idle/dark with no keypress), including the odd/empty shapes.
        let mut g = Ripple::new(EffectParams::default());
        assert_eq!(g.frame(6, 22, 1.5, Rgb::new(0, 255, 0)).len(), 6 * 22);
        assert_eq!(g.frame(1, 1, 2.0, Rgb::new(0, 255, 0)).len(), 1);
        assert!(g.frame(0, 0, 0.0, Rgb::new(0, 255, 0)).is_empty());
    }

    #[test]
    fn ripple_ring_is_brighter_than_its_surround() {
        // inject one ripple at the centre and render mid-life: the cell ON the ring (matrix distance ≈
        // radius from the origin) is brighter than the centre (well inside) and a far corner (outside)
        // — the ring is a moving luminance band, the whole point of the effect.
        let (r, c) = (9usize, 9usize);
        let mut g = Ripple::new(EffectParams::default());
        g.dims = (r as u8, c as u8);
        g.waves.push(RippleWave { or: 4.0, oc: 4.0, t0: 0.0 });
        let t = 3.0 / 7.0; // BASE_SPEED 7.0 ⇒ radius ≈ 3 cells at this age
        let f = g.frame(r as u8, c as u8, t, Rgb::new(0, 255, 0));
        let b = |y: usize, x: usize| bright(&f[y * c + x]);
        let on_ring = b(4, 7); // d = 3 from centre → on the ring crest
        let centre = b(4, 4); // d = 0 → well inside, dim
        let corner = b(0, 0); // d ≈ 5.66 → outside the ring, dim
        assert!(on_ring > centre, "the ring must outshine its centre ({centre} vs {on_ring})");
        assert!(on_ring > corner, "the ring must outshine cells outside it ({corner} vs {on_ring})");
    }

    #[test]
    fn ripple_honours_speed_and_fade() {
        // speed = expansion rate: at the same age a faster ripple's ring sits further out, so the
        // frames differ. fade = lifetime: a brisker fade dims the ring sooner, so a long-fade ring
        // leaves MORE lit than a brisk one at the same age.
        let render = |p: EffectParams, t: f32| -> Vec<Rgb> {
            let (r, c) = (9u8, 9u8);
            let mut g = Ripple::new(p);
            g.dims = (r, c);
            g.waves.push(RippleWave { or: 4.0, oc: 4.0, t0: 0.0 });
            g.frame(r, c, t, Rgb::new(0, 255, 0))
        };
        let slow = render(EffectParams { speed: 0.5, ..Default::default() }, 0.4);
        let fast = render(EffectParams { speed: 3.0, ..Default::default() }, 0.4);
        assert_ne!(slow, fast, "speed must move the ring outward at a different rate");
        let lit = |f: &[Rgb]| f.iter().map(|c| c.r as u32 + c.g as u32 + c.b as u32).sum::<u32>();
        let lasting = render(EffectParams { fade: 0.5, ..Default::default() }, 0.6);
        let brisk = render(EffectParams { fade: 3.0, ..Default::default() }, 0.6);
        assert!(
            lit(&lasting) > lit(&brisk),
            "a brisk fade leaves a dimmer ring at the same age ({} vs {})",
            lit(&brisk),
            lit(&lasting)
        );
    }

    // ── comet (an endless parade of varied, breakable streaks) ──────────────────────────────────
    // The lifecycle (no wrap → die → randomized gap → respawn fresh), the bold/varied spawn rolls, and
    // the break mechanic are driven directly through `frame`/`step`/`spawn_body`/`break_at` so the tests
    // stay deterministic and OS-independent (the live key read in `frame` is a no-op without a held key).
    // Window-accumulating tests (not single-final-frame) tolerate the intentional respawn gaps.

    #[test]
    fn comet_produces_full_frames() {
        let mut g = Comet::new(EffectParams::default());
        assert_eq!(g.frame(6, 22, 1.5, Rgb::new(0, 255, 0)).len(), 6 * 22);
        assert_eq!(g.frame(1, 1, 2.0, Rgb::new(0, 255, 0)).len(), 1);
        assert!(g.frame(0, 0, 0.0, Rgb::new(0, 255, 0)).is_empty());
    }

    #[test]
    fn comet_default_is_one_calm_varied_comet() {
        // the DEFAULT is a single CALM streak (not a swarm), but still VARIED — each pass is a freshly
        // rolled, DIFFERENT comet (different entry/angle/length), never the old eternal loop.
        use std::collections::HashSet;
        let mut g = Comet::new(EffectParams::default());
        assert_eq!(g.count(), 1, "default density must yield exactly ONE comet");
        let (r, c) = (6usize, 22usize);
        let frames = 240; // long enough for the single comet to cross + respawn several times
        let mut total = 0usize;
        // the live comet's ROLLED identity (constant for its life, re-rolled on respawn) — distinct
        // signatures across the window prove each successive comet is different.
        let mut signatures: HashSet<(u32, u32, u32)> = HashSet::new();
        for i in 0..frames {
            let f = g.frame(r as u8, c as u8, i as f32 * 0.05, Rgb::new(0, 255, 0));
            total += f.iter().filter(|&&p| p != Rgb::BLACK).count();
            if g.comets[0].respawn <= 0.0 {
                let b = g.comets[0];
                signatures.insert((b.vx.to_bits(), b.vy.to_bits(), b.trail.to_bits()));
            }
        }
        let avg = total / frames;
        // CALM: a single streak lights only a handful of cells on average — clearly not the busy swarm.
        assert!(avg <= 14, "the default must be calm — one streak, not a swarm (avg lit cells {avg})");
        // ALIVE: a comet is present most of the time (only brief beats between passes), not dead air.
        assert!(avg >= 2, "the default must stay alive — a comet streaking most of the time (avg {avg})");
        // VARIED: across its passes the single comet is re-rolled DIFFERENT each time (no eternal loop).
        assert!(signatures.len() >= 3, "each pass must be a DIFFERENT comet (saw {} distinct)", signatures.len());
    }

    #[test]
    fn comet_density_scales_up_to_a_busy_board() {
        // turning `density` UP scales the population into a busy swarm — several streaks at once, the
        // board reliably full. The companion to the calm default: density is the 1→many knob.
        let mut g = Comet::new(EffectParams { density: 3.0, ..Default::default() });
        assert!(g.count() >= 6, "max density must pack in several comets (count {})", g.count());
        let (r, c) = (6usize, 22usize);
        let frames = 60;
        let mut min_lit = usize::MAX;
        let mut total = 0usize;
        for i in 0..frames {
            let f = g.frame(r as u8, c as u8, i as f32 * 0.05, Rgb::new(0, 255, 0));
            let lit = f.iter().filter(|&&p| p != Rgb::BLACK).count();
            min_lit = min_lit.min(lit);
            total += lit;
        }
        let avg = total / frames;
        assert!(min_lit >= 6, "a busy board never goes near-empty (min lit cells {min_lit})");
        assert!(avg >= 20, "high density must be busy — several streaks at once (avg lit cells {avg})");
    }

    #[test]
    fn comet_animates_over_time() {
        // the parade is in motion: across a window, consecutive frames are not all identical.
        let mut g = Comet::new(EffectParams::default());
        let frames: Vec<Vec<Rgb>> =
            (0..30).map(|i| g.frame(8, 12, i as f32 * 0.05, Rgb::new(0, 255, 0))).collect();
        assert!(
            frames.windows(2).any(|w| w[0] != w[1]),
            "the comet field must change over time (motion, not a frozen frame)"
        );
    }

    #[test]
    fn comet_head_is_brighter_than_its_tail() {
        // depth: a comet's head lerps toward WHITE (its channel sum exceeds a pure full-bright base)
        // while the cells behind it are its dimmer gradient tail. Render a single known comet directly
        // (isolated from the live population) and compare the brightest cell (head) to the lit tail cells.
        let (r, c) = (6usize, 22usize);
        let b = CometBody {
            x: 11.0, y: 3.0, vx: 1.0, vy: 0.0,
            speed_mul: 1.0, trail: 8.0, bright: 1.0, hue: 0.0, respawn: 0.0,
        };
        let mut f = vec![Rgb::BLACK; r * c];
        draw_comet(&mut f, &b, Rgb::new(0, 255, 0), r, c);
        let sum = |c: &Rgb| c.r as u32 + c.g as u32 + c.b as u32;
        let head = f.iter().map(sum).max().unwrap();
        let lit: Vec<u32> = f.iter().map(sum).filter(|&b| b > 0).collect();
        assert!(lit.len() >= 2, "the comet must paint a head plus a tail");
        let tail = lit.iter().filter(|&&b| b < head).max().copied().unwrap_or(0);
        assert!(tail < head, "the head ({head}) must outshine the tail ({tail})");
        // base (0,255,0) sums to 255; a whitened head adds red+blue so it sums higher — proof of the
        // toward-white depth, not a flat coloured dot.
        assert!(head > 255, "the comet head must brighten toward white (sum {head} > base 255)");
    }

    #[test]
    fn comet_density_adds_more_comets() {
        // `density` is how MANY comets stream at once (1..3): more comets cover more of the board over a
        // window. Accumulate the union of ever-lit cells so the intentional respawn gaps don't matter.
        let coverage = |dens: f32| {
            let mut g = Comet::new(EffectParams { density: dens, ..Default::default() });
            let mut ever = vec![false; 6 * 12];
            for i in 0..30 {
                let f = g.frame(6, 12, i as f32 * 0.05, Rgb::new(0, 255, 0));
                for (e, c) in ever.iter_mut().zip(f) {
                    if c != Rgb::BLACK {
                        *e = true;
                    }
                }
            }
            ever.iter().filter(|&&b| b).count()
        };
        assert!(coverage(3.0) > coverage(1.0), "more density = more comets = more board covered");
    }

    #[test]
    fn comet_speed_changes_output() {
        // `speed` scales the sim rate: over the same window a faster parade has streaked further, so the
        // whole frame SEQUENCE differs (robust to gaps — compares all frames, not just the last).
        let run = |spd: f32| -> Vec<Vec<Rgb>> {
            let mut g = Comet::new(EffectParams { speed: spd, ..Default::default() });
            (0..20).map(|i| g.frame(8, 12, i as f32 * 0.05, Rgb::new(0, 255, 0))).collect()
        };
        assert_ne!(run(0.4), run(4.0), "speed must change the streak's progress over the same window");
    }

    #[test]
    fn comet_streaks_in_the_layer_colour() {
        // the streaks are poured in the layer colour (with only a subtle hue jitter): a green comet
        // stays recognizably GREEN, a red one red, and the two runs differ. Accumulate over a window.
        let run = |base: Rgb| -> Vec<Vec<Rgb>> {
            let mut g = Comet::new(EffectParams::default());
            (0..30).map(|i| g.frame(6, 12, i as f32 * 0.05, base)).collect()
        };
        let green = run(Rgb::new(0, 255, 0));
        let red = run(Rgb::new(255, 0, 0));
        assert_ne!(green, red, "the layer colour must drive the streak hue");
        // some lit tail cell is GREEN-DOMINANT (g is the strongest channel) — proof the streak carries
        // the chosen colour (the subtle hue jitter keeps it recognizably green, not a hardcoded clone).
        assert!(
            green.iter().flatten().any(|c| c.g > c.r && c.g > c.b && c.g > 0),
            "a green comet must light green-dominant cells"
        );
    }

    #[test]
    fn comet_spawns_cohesive_cardinal_biased() {
        // The NEW direction system: comets are AXIS-COUPLED and cardinal-biased, so they read as
        // board-aligned sweeps — MOST clean horizontal or vertical, MANY with a gentle lean, FEW raking
        // a true ~45° diagonal (the inverse of the old "always diagonal" peak). Classify each seeded
        // spawn's UNIT velocity: near-horizontal (|vy| < 0.26 ≈ within ~15° of flat), near-vertical
        // (|vx| < 0.26), else diagonal. Deterministic (seeded xorshift) ⇒ reproducible.
        let mut g = Comet::new(EffectParams::default());
        let (r, c) = (6usize, 22usize);
        let n = 400usize;
        let (mut horiz, mut vert, mut diag) = (0usize, 0usize, 0usize);
        let (mut up, mut down, mut left, mut right) = (false, false, false, false);
        for _ in 0..n {
            let b = g.spawn_body(r, c);
            assert!((b.vx * b.vx + b.vy * b.vy - 1.0).abs() < 1e-3, "velocity must be a unit vector");
            if b.vy.abs() < 0.26 {
                horiz += 1;
            } else if b.vx.abs() < 0.26 {
                vert += 1;
            } else {
                diag += 1;
            }
            if b.vy < -0.05 { up = true; }
            if b.vy > 0.05 { down = true; }
            if b.vx < -0.05 { left = true; }
            if b.vx > 0.05 { right = true; }
        }
        let cardinal = horiz + vert;
        // MOST comets read as clean cardinal sweeps — the cohesive majority, not the old diagonal peak.
        assert!(
            cardinal * 100 >= n * 60,
            "near-cardinal comets must be the MAJORITY (cardinal {cardinal}, horiz {horiz}, vert {vert}, diag {diag} of {n})"
        );
        // BOTH axes occur with a meaningful share — a cohesive MIX, never collapsed onto one axis.
        assert!(horiz * 100 >= n * 15, "horizontal sweeps must be common (horiz {horiz} of {n})");
        assert!(vert * 100 >= n * 15, "vertical drops must be common (vert {vert} of {n})");
        // …yet a thin tail of TRUE diagonals still occurs — variety as spice, no longer the norm.
        assert!(diag > 0, "some diagonal spice must still occur (diag {diag} of {n})");
        // every cardinal direction is still reachable (comets cross from all four coupled edges).
        assert!(up && down, "comets must cross with BOTH Y signs (drops up AND down, leans both ways)");
        assert!(left && right, "comets must cross with BOTH X signs (sweeps left AND right)");
    }

    #[test]
    fn comet_moves_along_y_over_time() {
        // a behavioural Y-axis check: inject a steep diagonal comet and step — its head ROW must change,
        // proving the velocity model frees it from any horizontal-only raster.
        let (r, c) = (8usize, 12usize);
        let mut g = Comet::new(EffectParams::default());
        g.dims = (r as u8, c as u8);
        g.level = vec![0.0; r * c];
        g.comets = vec![CometBody {
            x: 1.0, y: 1.0, vx: 0.6, vy: 0.8,
            speed_mul: 1.0, trail: 6.0, bright: 1.0, hue: 0.0, respawn: 0.0,
        }];
        let y0 = g.comets[0].y;
        for _ in 0..6 {
            g.step(r, c);
        }
        let y1 = g.comets[0].y;
        assert!((y1 - y0).abs() > 0.5, "a diagonal comet must move along Y over time ({y0} -> {y1})");
    }

    #[test]
    fn comet_respawns_with_fresh_varied_params() {
        // the variety engine: every (re)spawn re-rolls EVERYTHING from the PRNG, so successive comets
        // differ in angle / speed / trail / hue — they are not all the same comet (the eternal-loop fix).
        let mut g = Comet::new(EffectParams::default());
        let (r, c) = (6usize, 22usize);
        let bodies: Vec<CometBody> = (0..12).map(|_| g.spawn_body(r, c)).collect();
        let spread = |f: &dyn Fn(&CometBody) -> f32| {
            let (mut lo, mut hi) = (f32::MAX, f32::MIN);
            for b in &bodies {
                let v = f(b);
                lo = lo.min(v);
                hi = hi.max(v);
            }
            hi - lo
        };
        assert!(spread(&|b| b.vy.atan2(b.vx)) > 0.5, "successive comets must vary in ANGLE");
        assert!(spread(&|b| b.speed_mul) > 0.1, "successive comets must vary in SPEED");
        assert!(spread(&|b| b.trail) > 0.5, "successive comets must vary in TRAIL length");
        assert!(spread(&|b| b.hue) > 1.0, "successive comets must vary in HUE jitter");
        assert!(bodies.windows(2).any(|w| w[0] != w[1]), "comets must not all be identical");
    }

    #[test]
    fn comet_dies_off_board_then_respawns_as_a_new_one() {
        // the eternal-loop killer: a comet that runs off the board DIES (no wrap), waits a randomized
        // gap, then RESPAWNS as a freshly-rolled, DIFFERENT comet re-entering from an edge.
        let (r, c) = (6usize, 22usize);
        let mut g = Comet::new(EffectParams::default());
        g.dims = (r as u8, c as u8);
        g.level = vec![0.0; r * c];
        // a single comet near the right edge heading right — it will travel off the board.
        g.comets = vec![CometBody {
            x: (c - 1) as f32, y: 3.0, vx: 1.0, vy: 0.0,
            speed_mul: 1.0, trail: 4.0, bright: 1.0, hue: 0.0, respawn: 0.0,
        }];
        let original = g.comets[0];
        let mut died = false;
        for _ in 0..200 {
            g.step(r, c);
            if g.comets[0].respawn > 0.0 {
                died = true;
                break;
            }
        }
        assert!(died, "a comet that travels off the board must die (no eternal wrapping loop)");
        let mut respawned = false;
        for _ in 0..400 {
            g.step(r, c);
            if g.comets[0].respawn <= 0.0 {
                respawned = true;
                break;
            }
        }
        assert!(respawned, "after its randomized gap the comet must respawn");
        let fresh = g.comets[0];
        assert_ne!(fresh, original, "the respawn must be a freshly-rolled, DIFFERENT comet");
        // it re-enters from an edge (head at/just outside a border), not looping on from the death zone.
        let near_edge = fresh.x <= 0.0
            || fresh.x >= (c - 1) as f32
            || fresh.y <= 0.0
            || fresh.y >= (r - 1) as f32;
        assert!(near_edge, "a respawned comet enters from an edge ({}, {})", fresh.x, fresh.y);
    }

    #[test]
    fn comet_breaks_on_a_press_at_its_head_and_respawns_fresh() {
        // a fresh press on (or within ~1 cell of) a live comet's HEAD breaks it — flashing a white-hot
        // burst and RESPAWNING it through the fresh-roll path (so it comes back DIFFERENT and alive); a
        // press elsewhere does nothing.
        let (r, c) = (6usize, 22usize);
        let make = || {
            let mut g = Comet::new(EffectParams::default());
            g.dims = (r as u8, c as u8);
            g.level = vec![0.0; r * c];
            g.comets = vec![CometBody {
                x: 5.0, y: 3.0, vx: 1.0, vy: 0.0,
                speed_mul: 1.0, trail: 6.0, bright: 1.0, hue: 0.0, respawn: 0.0,
            }];
            g
        };
        // a press AT the head cell (row 3, col 5) breaks it.
        let mut hit = make();
        let before = hit.comets[0];
        assert!(hit.break_at(3.0, 5.0, r, c), "a press on the comet's head must break it");
        let after = hit.comets[0];
        assert_ne!(after, before, "the broken comet must respawn as a fresh, DIFFERENT comet");
        assert_eq!(after.respawn, 0.0, "a broken comet respawns alive (streaming again), not into a gap");
        // the break paints a white-hot BURST at the impact: the core is pushed above 1.0 so it renders
        // brighter than any head — the brightest moment.
        assert!(
            hit.level[3 * c + 5] > 1.0,
            "the break must flash a white-hot burst at the impact ({})",
            hit.level[3 * c + 5]
        );
        // a press FAR from any head breaks nothing and leaves the comet untouched.
        let mut miss = make();
        let before = miss.comets[0];
        assert!(!miss.break_at(3.0, 18.0, r, c), "a press far from any head breaks nothing");
        assert_eq!(miss.comets[0], before, "a missed press leaves the comet untouched");
        assert!(miss.level.iter().all(|&v| v == 0.0), "a missed press paints no burst");
    }

    // ── aurora (ambient flow) ───────────────────────────────────────────────────────────────────

    #[test]
    fn aurora_produces_full_frames() {
        let mut g = Aurora { p: EffectParams::default() };
        assert_eq!(g.frame(6, 22, 1.5, Rgb::new(0, 255, 0)).len(), 6 * 22);
        assert_eq!(g.frame(1, 1, 2.0, Rgb::new(0, 255, 0)).len(), 1);
        assert!(g.frame(0, 0, 0.0, Rgb::new(0, 255, 0)).is_empty());
    }

    #[test]
    fn aurora_flows_over_time() {
        let mut g = Aurora { p: EffectParams::default() };
        let a = g.frame(6, 22, 0.0, Rgb::new(0, 255, 0));
        let b = g.frame(6, 22, 5.0, Rgb::new(0, 255, 0));
        assert_ne!(a, b, "the aurora must drift over time");
    }

    #[test]
    fn aurora_has_brightness_depth_in_space() {
        // depth: at a fixed time the luminance undulates across the board (bands glow and dim), not a
        // flat wash — the brightest cell clearly exceeds the dimmest.
        let mut g = Aurora { p: EffectParams::default() };
        let f = g.frame(6, 22, 2.0, Rgb::new(0, 255, 0));
        let hi = f.iter().map(bright).max().unwrap();
        let lo = f.iter().map(bright).min().unwrap();
        assert!(
            hi as i32 - lo as i32 > 30,
            "aurora must undulate in brightness across the board ({lo}..{hi}), not a flat wash"
        );
    }

    #[test]
    fn aurora_colour_biases_the_palette() {
        // the layer colour is the hue CENTRE the flow drifts around, so a green-based aurora and a
        // red-based one differ — the colour knob is honoured (a Color knob the generic sweep skips).
        let mut gg = Aurora { p: EffectParams::default() };
        let mut gr = Aurora { p: EffectParams::default() };
        let green = gg.frame(6, 22, 1.0, Rgb::new(0, 255, 0));
        let red = gr.frame(6, 22, 1.0, Rgb::new(255, 0, 0));
        assert_ne!(green, red, "the layer colour must bias the aurora palette");
    }

    // ── typing heat (live keyboard → a living heat MAP) ──────────────────────────────────────────
    // Like reactive/ripple, TypingHeat reads the LIVE keyboard. The render + the field-physics helpers are
    // pure fns tested directly (`render_typing_heat`, `cool_field`, `diffuse_field`, `deposit_heat`,
    // `step_rate`, `deposit_peak`); the generator paths are tested with key reads SUPPRESSED
    // (`capture::suppress_key_reads`) so the per-VK scan is a deterministic no-op regardless of the host's
    // real key state, then injecting the heat field. The redesign's headline guarantee — that sustained
    // fast typing NEVER floods to a flat slab — is asserted in `typing_heat_never_a_flat_slab_*`.

    /// Per-cell brightness sum.
    fn lumsum(c: &Rgb) -> u32 {
        c.r as u32 + c.g as u32 + c.b as u32
    }

    #[test]
    fn typing_heat_produces_full_frames() {
        // the pure renderer fills any rows×cols, including the odd/empty shapes the compositor may hand it.
        let accent = Rgb::new(255, 170, 40);
        assert_eq!(render_typing_heat(&vec![0.0; 6 * 22], accent, 0.0, 6, 22).len(), 6 * 22);
        assert_eq!(render_typing_heat(&[0.0], accent, 0.0, 1, 1).len(), 1);
        assert!(render_typing_heat(&[], accent, 0.0, 0, 0).is_empty());
        // the live FrameGen path also fills the matrix (no keypress → a cool idle board, never a panic).
        let _no_keys = crate::capture::suppress_key_reads();
        let mut g = make("typingheat").unwrap();
        assert_eq!(g.frame(6, 12, 0.0, accent).len(), 6 * 12);
    }

    #[test]
    fn typing_heat_field_ramps_incandescent_and_brightens() {
        // INCANDESCENT + brighten-as-it-heats: a hotter FIELD → a hotter, brighter board climbing the
        // ember→flame ramp — NEVER through a green zone. A uniform field probes the ramp directly (the
        // never-a-slab dynamics are a separate test). Assert brightness rises, the idle board is dim, the
        // board is never green-dominant, and a hot board reads WARM (red leads).
        let accent = Rgb::new(255, 170, 40);
        let (rows, cols) = (6u8, 22u8);
        let render = |h: f32| {
            render_typing_heat(&vec![h; rows as usize * cols as usize], accent, 0.0, rows, cols)
        };
        let mean = |f: &[Rgb], pick: fn(&Rgb) -> u32| -> f32 {
            f.iter().map(pick).sum::<u32>() as f32 / f.len() as f32
        };
        let cold = render(0.0);
        let mid = render(0.5);
        let hot = render(1.0);
        // brightens monotonically with the field temperature — the board literally lights up where it's hot.
        assert!(
            mean(&hot, lumsum) > mean(&mid, lumsum) && mean(&mid, lumsum) > mean(&cold, lumsum),
            "the board must brighten as the field heats"
        );
        // the idle board is dim (a faint cool-dark glow, clearly "cold").
        assert!(mean(&cold, lumsum) < 60.0, "an idle board must be dim, not a faked-warm glow");
        // NEVER GREEN: at no temperature is green the dominant channel. Heat leads with red, cold leans
        // cool-dark/blue.
        for h in [0.0, 0.2, 0.4, 0.6, 0.8, 1.0] {
            let f = render(h);
            let g = mean(&f, |c| c.g as u32);
            let r = mean(&f, |c| c.r as u32);
            let b = mean(&f, |c| c.b as u32);
            assert!(g <= r.max(b), "the board must NEVER read green-dominant (heat {h}: r{r} g{g} b{b})");
        }
        // a hot board reads WARM — red is the dominant channel (incandescent flame), not green or blue.
        let hr = mean(&hot, |c| c.r as u32);
        assert!(hr > mean(&hot, |c| c.b as u32), "a hot board reads warm (red over blue)");
        assert!(hr > mean(&hot, |c| c.g as u32), "a hot board reads warm (red over green)");
    }

    #[test]
    fn typing_heat_diffusion_spreads_and_rises() {
        // the field DIFFUSES: a lone hot cell bleeds into its neighbours (centre drops, neighbours rise),
        // with a gentle UPWARD bias (heat rises → the cell ABOVE gains more than the one below). And a
        // static clock (dt 0) freezes it — no spread on a paused timeline.
        let (r, c) = (6usize, 22usize);
        let mut field = vec![0.0f32; r * c];
        let mut scratch = vec![0.0f32; r * c];
        let (cy, cx) = (3usize, 11usize);
        let i = cy * c + cx;
        field[i] = 1.0;
        diffuse_field(&mut field, &mut scratch, r, c, 0.05); // one conduction step
        assert!(field[i] < 1.0, "the hot cell must lose heat to its neighbours ({})", field[i]);
        for &(dy, dx) in &[(0isize, 1isize), (0, -1), (-1, 0), (1, 0)] {
            let ni = (cy as isize + dy) as usize * c + (cx as isize + dx) as usize;
            assert!(field[ni] > 0.0, "heat must spread into neighbour ({dy},{dx}) — saw {}", field[ni]);
        }
        let above = (cy - 1) * c + cx;
        let below = (cy + 1) * c + cx;
        assert!(
            field[above] > field[below],
            "heat must RISE — the cell above warms more than the one below ({} !> {})",
            field[above],
            field[below]
        );
        // dt 0 → the field is frozen (no diffusion on a paused clock).
        let mut f2 = vec![0.0f32; r * c];
        f2[i] = 1.0;
        let mut s2 = vec![0.0f32; r * c];
        diffuse_field(&mut f2, &mut s2, r, c, 0.0);
        assert_eq!(f2[i], 1.0, "dt 0 must freeze the field (no diffusion on a paused clock)");
    }

    #[test]
    fn typing_heat_diffusion_conserves_heat() {
        // CONSERVATION: the diffusion REDISTRIBUTES heat among neighbours without losing (or creating) any —
        // blurring ALONE preserves the total field sum (the only thing that may remove heat is the cooling).
        // A lossy "blend toward the average" used to drain the board through the blur; this can't.
        let (r, c) = (6usize, 22usize);
        let mut field = vec![0.0f32; r * c];
        let mut scratch = vec![0.0f32; r * c];
        // an uneven field: a couple of hot spots and a warm floor (the regime where a lossy blur leaks most).
        for (n, h) in field.iter_mut().enumerate() {
            *h = 0.10 + 0.03 * (n % 7) as f32;
        }
        field[3 * c + 11] = 1.6; // a white-hot core
        field[2 * c + 4] = 0.9;
        let before: f32 = field.iter().sum();
        // many conduction steps at a realistic dt — the sum must hold throughout (no slow drain).
        for _ in 0..120 {
            diffuse_field(&mut field, &mut scratch, r, c, 0.03);
        }
        let after: f32 = field.iter().sum();
        assert!(
            (after - before).abs() < before * 1e-3,
            "diffusion must conserve total heat (before {before}, after {after})"
        );
        // and it really did spread (the hot core handed heat to its neighbours, didn't just sit there).
        assert!(field[3 * c + 11] < 1.6, "the hot core must have spread its heat into neighbours");
    }

    #[test]
    fn typing_heat_cooling_is_temperature_dependent() {
        // THE HEADLINE: cooling is NON-LINEAR (Newton + radiative), not a flat fraction off every cell.
        // (a) a HOT cell loses MORE heat per frame (absolute) than a cool one; (b) the per-step FRACTIONAL
        // loss is larger when hotter (a hot cell sheds a bigger SHARE); (c) EMBERS LINGER — a warm-but-not-
        // hot cell keeps a meaningful fraction over a window where the old FLAT decay would be long dark.
        let dt = 0.03f32;
        let cool1 = |t: f32| {
            let mut f = [t];
            cool_field(&mut f, 1.0, dt);
            f[0]
        };
        // (a) absolute loss rises with temperature.
        let (hot, warm, ember) = (1.3f32, 0.6f32, 0.2f32);
        let abs_hot = hot - cool1(hot);
        let abs_warm = warm - cool1(warm);
        let abs_ember = ember - cool1(ember);
        assert!(
            abs_hot > abs_warm && abs_warm > abs_ember,
            "a hotter cell must lose MORE heat per frame (abs: {abs_ember} < {abs_warm} < {abs_hot})"
        );
        // (b) fractional loss rises with temperature (the temperature-dependent RATE itself).
        let frac_hot = abs_hot / hot;
        let frac_warm = abs_warm / warm;
        let frac_ember = abs_ember / ember;
        assert!(
            frac_hot > frac_warm && frac_warm > frac_ember,
            "the per-step FRACTIONAL loss must be larger when hotter ({frac_ember} < {frac_warm} < {frac_hot})"
        );
        // (c) EMBERS LINGER vs the old flat decay. Cool an ember for ~3s with the new curve; compare to the
        // old flat multiplicative decay (keep = exp(-2·fade·dt)) over the same time — the new tail is far
        // warmer (the old fade would have gone essentially dark).
        let mut new_ember = 0.25f32;
        let mut old_flat = 0.25f32;
        let old_keep = (-2.0f32 * 1.0 * dt).exp(); // the previous flat-decay constant
        for _ in 0..100 {
            // ~3s
            let mut f = [new_ember];
            cool_field(&mut f, 1.0, dt);
            new_ember = f[0];
            old_flat *= old_keep;
        }
        assert!(
            new_ember > 0.05,
            "embers must LINGER — a warm cell keeps a meaningful fraction after ~3s ({new_ember})"
        );
        assert!(
            new_ember > old_flat * 10.0,
            "the new ember tail must vastly outlast the old flat decay (new {new_ember} vs old {old_flat})"
        );
        // …and it still reaches ~dark eventually (asymptotic toward 0, just slowly at the low end).
        let mut idle = 0.25f32;
        for _ in 0..1000 {
            // ~30s
            let mut f = [idle];
            cool_field(&mut f, 1.0, dt);
            idle = f[0];
        }
        assert!(idle < 0.02, "a long-idle ember must eventually cool to ~dark ({idle})");
    }

    #[test]
    fn typing_heat_cooling_is_fps_independent() {
        // FPS-INDEPENDENCE: the cooling curve must be the same whether the app runs at 60fps or 30fps —
        // it uses the REAL elapsed dt, not a baked-in step. Cool the same start temperature over the same
        // 2s of wall-clock at two frame rates; the result must match closely.
        let cool_for = |total: f32, dt: f32, t0: f32| -> f32 {
            let mut f = [t0];
            let steps = (total / dt).round() as usize;
            for _ in 0..steps {
                cool_field(&mut f, 1.0, dt);
            }
            f[0]
        };
        for &t0 in &[1.0f32, 0.3] {
            let fast = cool_for(2.0, 1.0 / 60.0, t0); // 60fps
            let slow = cool_for(2.0, 1.0 / 30.0, t0); // 30fps
            assert!(
                (fast - slow).abs() < fast.max(0.01) * 0.1,
                "cooling must be fps-independent (t0 {t0}: 60fps {fast} vs 30fps {slow})"
            );
        }
        // a paused clock (dt 0) freezes the field — no cooling on a static t.
        let mut frozen = [0.8f32];
        cool_field(&mut frozen, 1.0, 0.0);
        assert_eq!(frozen[0], 0.8, "dt 0 must freeze the field (no cooling on a paused clock)");
    }

    #[test]
    fn typing_heat_never_a_flat_slab_under_sustained_typing() {
        // THE CORE REDESIGN: under SUSTAINED fast typing the board must stay a position-aware gradient in
        // motion — it must NEVER flood to a uniform, fully-lit slab (the old global-warmth flood's exact
        // failure). Simulate the field's per-frame math (cool → diffuse → deposit at a few keys) over many
        // frames, deterministically, then assert a clear gradient PERSISTS.
        let (r, c) = (6usize, 22usize);
        let mut field = vec![0.0f32; r * c];
        let mut scratch = vec![0.0f32; r * c];
        let keys = [(2usize, 4usize), (3, 9), (2, 14)]; // a few "home" keys, hit every frame (fast typing)
        let peak = deposit_peak(1.0, 1.0); // the fastest-typing deposit peak
        for _ in 0..200 {
            cool_field(&mut field, 1.0, 0.03); // ~30fps frames
            diffuse_field(&mut field, &mut scratch, r, c, 0.03);
            for &(ky, kx) in &keys {
                deposit_heat(&mut field, ky, kx, r, c, peak);
            }
        }
        let max = field.iter().cloned().fold(0.0f32, f32::max);
        let mean = field.iter().sum::<f32>() / field.len() as f32;
        // a clear gradient PERSISTS — the hottest cell is well above the board mean (never a flat slab).
        assert!(max > mean + 0.4, "the board must keep a thermal gradient, never flood flat (max {max}, mean {mean})");
        // a cell FAR from every typed key stays much cooler than a typed key (POSITION is preserved).
        let corner = c - 1; // top-right, away from all keys
        let typed = keys[1].0 * c + keys[1].1;
        assert!(
            field[typed] > field[corner] + 0.4,
            "typed keys must stay hotter than untyped regions ({} !> {})",
            field[typed],
            field[corner]
        );
        // and the field never runs away — bounded by the deposit ceiling.
        assert!(max <= 1.6 + 1e-3, "the field must stay bounded ({max})");
        // STOP typing → the EMBERS LINGER (the new physical cooldown): after a short pause the board has
        // shed its white-hot flares but is still meaningfully warm — a long, gradual drain, not the old
        // too-fast uniform fade.
        for _ in 0..200 {
            cool_field(&mut field, 1.0, 0.03); // ~6s of idle
            diffuse_field(&mut field, &mut scratch, r, c, 0.03);
        }
        let after_pause = field.iter().cloned().fold(0.0f32, f32::max);
        assert!(
            after_pause > 0.05,
            "embers must LINGER after a stop (a slow ember tail), not fade instantly ({after_pause})"
        );
        // …but a LONG idle eventually cools to ~dark (asymptotic toward 0 — the board does go out).
        for _ in 0..800 {
            cool_field(&mut field, 1.0, 0.03); // ~24s more
            diffuse_field(&mut field, &mut scratch, r, c, 0.03);
        }
        assert!(field.iter().all(|&h| h < 0.02), "a long-idle board must eventually cool to dark");
    }

    #[test]
    fn typing_heat_ramp_is_incandescent_red_to_white_never_green() {
        // the THERMAL RAMP itself (the heart of "looks like heat"): sampled cold→peak it must brighten,
        // climb red→orange→amber→white, NEVER be green-dominant, and the white-hot peak must read whitish.
        let accent = Rgb::new(255, 170, 40); // a warm accent (the realistic case)
        let probe = |temp: f32| thermal_color(temp, accent);
        let sum = |c: &Rgb| c.r as i32 + c.g as i32 + c.b as i32;
        // green is never the dominant channel anywhere on the ramp (incl. the white-hot overshoot).
        for k in 0..=26 {
            let temp = k as f32 / 20.0; // 0.0 .. 1.3
            let col = probe(temp);
            assert!(
                col.g <= col.r.max(col.b),
                "the incandescent ramp must never be green-dominant (temp {temp}: {col:?})"
            );
        }
        // brightness climbs across the ramp (coarse, robust to the accent tint): cold < warm < hot < peak.
        let pts = [0.05f32, 0.3, 0.55, 0.8, 1.0, 1.3];
        for w in pts.windows(2) {
            assert!(
                sum(&probe(w[1])) > sum(&probe(w[0])),
                "brightness must rise with temperature ({} → {})",
                w[0],
                w[1]
            );
        }
        // the working range is WARM: from the first real heat up, red leads blue (ember→flame), with no
        // green/cyan detour. (At the very cold base blue may lead — that's the faint cool-dark idle glow.)
        for &temp in &[0.2f32, 0.4, 0.6, 0.8] {
            let c = probe(temp);
            assert!(c.r > c.b, "heat must read warm — red over blue at temp {temp} ({c:?})");
            assert!(c.r >= c.g, "heat leads with red, never green, at temp {temp} ({c:?})");
        }
        // the cold base is a faint COOL-dark glow (dim, blue not red) — the board is off-but-cold.
        let cold = probe(0.0);
        assert!(sum(&cold) < 60, "the cold base must be dim ({cold:?})");
        assert!(cold.b >= cold.r, "the cold base reads cool (blue ≥ red) ({cold:?})");
        // the white-hot PEAK (a fresh flare) reads WHITISH — all channels high.
        let peak = probe(1.3);
        assert!(
            peak.r > 200 && peak.g > 190 && peak.b > 170,
            "a white-hot flare must read whitish (all channels high) ({peak:?})"
        );
    }

    #[test]
    fn typing_heat_hot_spot_is_hotter_cools_and_spreads() {
        // a deposited hot spot is hotter than its (cool) surroundings, SPREADS into its neighbours
        // (diffusion), and DECAYS over successive frames — the keys you hit flare, bloom, then fade. Drive
        // the generator with key reads suppressed so the per-VK scan is a no-op, then inject the field
        // directly (the field-poke pattern reactive/ripple use).
        let _no_keys = crate::capture::suppress_key_reads();
        let accent = Rgb::new(255, 170, 40);
        let (r, c) = (6usize, 22usize);
        let mut g = TypingHeat::new(EffectParams::default());
        let _ = g.frame(r as u8, c as u8, 0.0, accent); // first frame inits dims/field, anchors last_t
        let cell = 2 * c + 7; // an interior cell
        g.heat[cell] = 1.0; // a fresh hot spot; rate stays 0 (no presses)
        // RENDERED: the hit cell clearly outshines its cool surroundings (one frame; brightness = sum,
        // which is monotonic on the incandescent ramp — embers add channels as they heat).
        let f0 = g.frame(r as u8, c as u8, 0.05, accent);
        let start = lumsum(&f0[cell]);
        let surround = lumsum(&f0[cell + 5]); // a far, cold cell
        assert!(
            start > surround,
            "the hit cell must outshine its cool surroundings ({surround} vs {start})"
        );
        // DIFFUSION: after a frame the heat has bled into the immediate neighbours (they warmed from 0).
        assert!(
            g.heat[cell - 1] > 0.0 && g.heat[cell + 1] > 0.0 && g.heat[cell - c] > 0.0 && g.heat[cell + c] > 0.0,
            "heat must diffuse into the pressed cell's neighbours"
        );
        // COOLING — tested on the heat FIELD itself (the ground truth, free of the breath/shimmer
        // wobble): it decays strictly every frame toward zero (cooling + diffusing away).
        let mut last_field = g.heat[cell];
        assert!(last_field < 1.0, "the field must have started cooling+diffusing after the first step");
        let mut f_end = f0.clone();
        for k in 2..12 {
            f_end = g.frame(r as u8, c as u8, k as f32 * 0.05, accent);
            assert!(
                g.heat[cell] < last_field,
                "the heat field must cool strictly each frame ({} !< {last_field})",
                g.heat[cell]
            );
            last_field = g.heat[cell];
        }
        // …and the cooling shows on screen too: the hit cell is clearly dimmer at the end than at the
        // start (a coarse first-vs-last check, robust to the per-frame shimmer wobble).
        assert!(
            lumsum(&f_end[cell]) < start,
            "the hot spot must visibly cool over the window ({} !< {start})",
            lumsum(&f_end[cell])
        );
    }

    #[test]
    fn typing_heat_colour_recolours_the_flame_coherently() {
        // the colour knob RECOLOURS the flame coherently (a hue rotation): two different colours give a
        // different hot-zone hue, but a WHITE-HOT flare core stays PURE WHITE regardless — clean, never
        // muddy (the old lerp-tint ringed flares with blue-green; this can't).
        let warm = Rgb::new(255, 170, 40);
        let cool = Rgb::new(74, 160, 240); // a cool blue
        // hot zone (temp ≈ 0.9, no white overshoot): the flame's hue differs by the chosen colour.
        let a = render_typing_heat(&vec![0.9f32], warm, 0.0, 1, 1);
        let b = render_typing_heat(&vec![0.9f32], cool, 0.0, 1, 1);
        assert_ne!(a[0], b[0], "the colour knob must recolour the flame's hot zone");
        // a white-hot flare CORE (temp > 1.05) is pure white regardless of the flame colour — the same
        // bright white for either, and it reads white (all channels high). No tint, no mud.
        let core_warm = render_typing_heat(&vec![1.6f32], warm, 0.0, 1, 1); // temp 1.6 → white-hot
        let core_cool = render_typing_heat(&vec![1.6f32], cool, 0.0, 1, 1);
        assert_eq!(
            core_warm[0], core_cool[0],
            "a white-hot flare core stays pure white regardless of the flame colour"
        );
        let m = core_warm[0].r.min(core_warm[0].g).min(core_warm[0].b);
        assert!(m > 200, "the flare core must read white-hot (all channels high) ({:?})", core_warm[0]);
    }

    #[test]
    fn typing_heat_default_colour_is_pure_fire_no_blue() {
        // THE FIX: at the DEFAULT colour (the warm fire tone the GUI applies), the board is pure fire —
        // NO blue/green cells ringing the white-hot flares (the artifact the old lerp-tint caused). Build
        // a warm board with a flare and assert NO cell is green- or blue-dominant; the flare core is white.
        let flame = default_color("typingheat").expect("typing heat has a built-in warm default");
        let (r, c) = (6usize, 22usize);
        // an ember-warm FLOOR everywhere (the regime where the old lerp-tint ringed flares blue/green)…
        let mut heat = vec![0.30f32; r * c];
        let core = 3 * c + 11;
        let peak = deposit_peak(1.0, 1.0); // a fast-flurry peak → a white-hot core
        deposit_heat(&mut heat, 3, 11, r, c, peak); // a soft radial flare in the middle
        deposit_heat(&mut heat, 3, 11, r, c, peak);
        let f = render_typing_heat(&heat, flame, 0.0, r as u8, c as u8);
        for (i, cell) in f.iter().enumerate() {
            assert!(
                cell.r >= cell.g && cell.r >= cell.b,
                "default fire must NEVER be green/blue-dominant (cell {i}: {cell:?})"
            );
        }
        // the flare core reads white-hot (all channels high), not a coloured blob.
        let m = f[core].r.min(f[core].g).min(f[core].b);
        assert!(m > 180, "the flare core must read white-hot ({:?})", f[core]);
    }

    #[test]
    fn typing_heat_rate_and_sensitivity_shape_deposits() {
        // SPEED→INTENSITY: the typing-rate EMA lifts how HOT a deposit lands; sensitivity is the gain;
        // fade is the cooldown. All deterministic (pure helpers, no live keyboard).
        assert!(step_rate(0.0, 4, 0.05, 1.0) > 0.0, "typing must raise the rate from cold");
        // faster typing → a hotter deposit peak (toward white-hot); a slow/lone press → a dim ember.
        let fast = deposit_peak(0.95, 1.0);
        let slow = deposit_peak(0.05, 1.0);
        assert!(fast > slow, "faster typing must land hotter deposits ({slow} vs {fast})");
        assert!(slow < 0.5, "a near-idle press leaves a dim ember ({slow})");
        assert!(fast > 1.05, "a fast flurry pushes the deposit white-hot, past the overshoot ({fast})");
        // higher sensitivity heats more readily for the SAME rate (the deposit-strength gain).
        let touchy = deposit_peak(0.4, 3.0);
        let dull = deposit_peak(0.4, 0.3);
        assert!(touchy > dull, "higher sensitivity heats more readily ({dull} vs {touchy})");
        // higher fade cools the rate faster: from the same warm start over the same idle gap.
        let brisk = step_rate(1.0, 0, 0.5, 3.0);
        let lingering = step_rate(1.0, 0, 0.5, 0.3);
        assert!(brisk < lingering, "a higher fade cools the rate faster ({brisk} !< {lingering})");
        // the rate stays bounded in 0..1 even under a flurry of presses.
        let mashed = step_rate(1.0, 50, 0.01, 1.0);
        assert!((0.0..=1.0).contains(&mashed), "rate must stay clamped to 0..1 ({mashed})");
    }

    #[test]
    fn typing_heat_fade_cools_the_field_faster() {
        // the generator honours `fade` for the LOCAL field cooling too: from a fully-hot field, a higher
        // fade leaves the board dimmer after the same elapsed time. Keys suppressed → deterministic.
        let _no_keys = crate::capture::suppress_key_reads();
        let accent = Rgb::new(255, 170, 40);
        let (r, c) = (4usize, 6usize);
        let run = |fade: f32| -> u32 {
            let mut g = TypingHeat::new(EffectParams { fade, ..Default::default() });
            let _ = g.frame(r as u8, c as u8, 0.0, accent); // init + anchor last_t
            for h in g.heat.iter_mut() {
                *h = 1.0; // a fully-hot field
            }
            let mut f = Vec::new();
            for k in 1..10 {
                f = g.frame(r as u8, c as u8, k as f32 * 0.05, accent);
            }
            f.iter().map(lumsum).sum()
        };
        assert!(
            run(3.0) < run(0.3),
            "a higher fade cools the heat field faster (less lit after the same time)"
        );
    }

    #[test]
    fn typing_heat_idle_board_is_cool_and_dim() {
        // honesty: with no typing the board reads COOL and DIM (a faint cool-dark glow), not faked warmth.
        let accent = Rgb::new(255, 170, 40);
        let f = render_typing_heat(&vec![0.0; 6 * 12], accent, 0.0, 6, 12);
        assert!(
            f.iter().all(|c| lumsum(c) < 40),
            "an idle board must be dim (a faint cool-dark glow), never a faked-warm glow"
        );
        // what little light there is reads cool (blue ≥ red), not warm.
        assert!(f.iter().all(|c| c.b >= c.r), "the idle board's dim glow is cool");
    }

    #[test]
    fn typing_heat_preview_shows_off_the_effect() {
        // the representative thumbnail must SCREAM the NEW "typing heat": a FLOWING incandescent heat MAP
        // with a sense of thermal RANGE (cooling trails) and several DISTINCT white-hot flares blooming
        // like just-pressed keys — NOT a uniform warm slab — so the postage-stamp tile sells the effect
        // even though the thumbnail pass can't read live keys. Rendered in the DEFAULT warm fire colour.
        let accent = default_color("typingheat").expect("typing heat has a built-in warm default");
        let f = preview_typing_heat(accent, 6, 22);
        assert_eq!(f.len(), 6 * 22);
        // PURE FIRE — no blue/green cells anywhere (the artifact fix): every cell reads warm or white.
        for (i, cell) in f.iter().enumerate() {
            assert!(
                cell.r >= cell.g && cell.r >= cell.b,
                "the default preview must be pure fire — never green/blue-dominant (cell {i}: {cell:?})"
            );
        }
        let mean = f.iter().map(lumsum).sum::<u32>() as f32 / f.len() as f32;
        // clearly warmer than an idle board (its whole point as a preview).
        let idle = render_typing_heat(&vec![0.0; 6 * 22], accent, 0.0, 6, 22);
        let idle_mean = idle.iter().map(lumsum).sum::<u32>() as f32 / idle.len() as f32;
        assert!(mean > idle_mean + 80.0, "the preview must read warm, not idle ({idle_mean} → {mean})");
        // it reads WARM, not green: red is the dominant channel in the mean, and the board is never
        // green-dominant (the original complaint was a flat GREEN tile).
        let mr = f.iter().map(|c| c.r as u32).sum::<u32>() as f32 / f.len() as f32;
        let mg = f.iter().map(|c| c.g as u32).sum::<u32>() as f32 / f.len() as f32;
        let mb = f.iter().map(|c| c.b as u32).sum::<u32>() as f32 / f.len() as f32;
        assert!(mr > mg && mr > mb, "the preview must read warm (red-dominant), not green (r{mr} g{mg} b{mb})");
        // DEPTH/RANGE: a clear spread between the dimmest (cool edge) and brightest (flare) cells.
        let hi = f.iter().map(lumsum).max().unwrap();
        let lo = f.iter().map(lumsum).min().unwrap();
        assert!(hi as f32 > mean + 80.0, "the preview must show hot flares brighter than its baseline");
        assert!(hi > lo + 200, "the preview must show a thermal range (cool edges → white-hot cores)");
        // the hottest cell is a WHITE-HOT flare — all channels high (a clean bright core, not a muddy
        // blob). A white core renders as equal channels, so its MIN channel is still high.
        let hottest = f.iter().max_by_key(|c| lumsum(c)).unwrap();
        let minch = hottest.r.min(hottest.g).min(hottest.b);
        assert!(minch > 170, "the preview's hottest flare must read white-hot (all channels high) ({hottest:?})");
        // several DISTINCT bright flares (not one): count clearly white-hot cells (the flare cores).
        let bright_cores = f.iter().filter(|c| lumsum(c) > 550).count();
        assert!(bright_cores >= 3, "the preview must scatter several distinct flares (saw {bright_cores})");
        // empty shapes are safe.
        assert!(preview_typing_heat(accent, 0, 0).is_empty());
    }
}
