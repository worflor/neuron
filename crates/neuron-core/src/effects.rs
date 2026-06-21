//! The open effect system — effects are pluggable **frame generators**, not a fixed firmware
//! menu. A device exposes a few primitives; everything else (fire, starlight, audio-reactive,
//! physics) is a function that produces frames, streamed to the custom-frame channel. Adding an
//! effect = adding a `FrameGen`. This is the open Chroma Studio: no software lock, no firmware
//! ceiling. The animation backend (`lighting::Lights::animate`) drives any generator.

use crate::lighting::Rgb;

/// A frame generator: given the matrix size, elapsed time `t` (seconds), and a base colour,
/// produce one frame of `rows*cols` colours (row-major). May carry state across frames
/// (fire's heat map, an audio buffer, a physics field, ...).
pub trait FrameGen {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, base: Rgb) -> Vec<Rgb>;
}

/// Per-layer parameters. Generators read these instead of baking in constants, so an effect's
/// SPEED and DIRECTION are user-tunable — a real instrument, not a fixed preset. Colour stays the
/// `base` arg of `frame()` (each compositor layer feeds its own colour as base).
#[derive(Clone, Copy, Debug)]
pub struct EffectParams {
    pub speed: f32,    // animation-rate multiplier (1.0 = the design default)
    pub direction: u8, // 0 → · 1 ← · 2 ↑ · 3 ↓  (directional effects only)
}
impl Default for EffectParams {
    fn default() -> Self {
        EffectParams {
            speed: 1.0,
            direction: 0,
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
        "fire" => Some(Box::new(Fire::default())),
        "colorwheel" | "wheel" => Some(Box::new(ColorWheel { p })),
        "starlight" | "stars" => Some(Box::new(Starlight::new(p))),
        "reactive" => Some(Box::new(Reactive::new(p))),
        "audiometer" | "audio" | "vu" => Some(Box::new(AudioMeter::new(p))),
        "rows" => Some(Box::new(Rows)),
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

/// Names of the built-in generators (for help / a future GUI palette). The four "live" effects
/// (`reactive`/`audiometer`) and the pure-time `colorwheel`/`starlight` are resolvable too; only the
/// self-contained pure ones are listed here (the registry test iterates this without touching the
/// keyboard/audio meter).
pub const BUILTINS: &[&str] = &[
    "static",
    "spectrum",
    "wave",
    "breathing",
    "fire",
    "colorwheel",
    "starlight",
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
        // whole device cycles hue together; speed scales the rate (1.0 = ~1 cycle / 3s)
        let c = Rgb::from_hsv(t * 120.0 * self.p.speed, 1.0, 1.0);
        vec![c; rows as usize * cols as usize]
    }
}

pub struct Wave {
    p: EffectParams,
}
impl FrameGen for Wave {
    fn frame(&mut self, rows: u8, cols: u8, t: f32, _base: Rgb) -> Vec<Rgb> {
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
                let h = (t * 0.33 * self.p.speed + u) * 360.0;
                f[y * c + x] = Rgb::from_hsv(h, 1.0, 1.0);
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
        let b = 0.5 - 0.5 * (t * TAU / 3.0 * self.p.speed).cos(); // smooth cosine; speed scales the breath
        vec![base.scale_f(b); rows as usize * cols as usize]
    }
}

// ── fire: a real heat simulation — the kind of effect Synapse software-locks ─────────────

/// Upward-propagating fire. Bottom row is seeded hot with flicker; heat diffuses up and cools;
/// the heat field is mapped to a black→red→orange→yellow→white ramp. Deterministic PRNG so it
/// needs no `rand` dependency (the project stays lean).
pub struct Fire {
    heat: Vec<f32>,
    dims: (u8, u8),
    rng: u32,
}

impl Default for Fire {
    fn default() -> Self {
        Fire {
            heat: Vec::new(),
            dims: (0, 0),
            rng: 0x9E37_79B9,
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
}

impl FrameGen for Fire {
    fn frame(&mut self, rows: u8, cols: u8, _t: f32, _base: Rgb) -> Vec<Rgb> {
        let (r, c) = (rows as usize, cols as usize);
        if self.dims != (rows, cols) {
            self.heat = vec![0.0; r * c];
            self.dims = (rows, cols);
        }
        if r == 0 || c == 0 {
            return Vec::new();
        }
        // seed the bottom row white-hot and fairly steady (the fire's base)
        let bottom = (r - 1) * c;
        for x in 0..c {
            self.heat[bottom + x] = 0.90 + self.rand() * 0.10;
        }
        // propagate upward with STRONG cooling so the fire stays low (bottom 2-3 rows) with only
        // sparse licks reaching the top — a readable flame shape rather than full-board noise.
        for y in 0..r - 1 {
            for x in 0..c {
                let below = (y + 1) * c + x;
                let bl = (y + 1) * c + (x + c - 1) % c;
                let br = (y + 1) * c + (x + 1) % c;
                let avg = (self.heat[below] * 2.0 + self.heat[bl] + self.heat[br]) / 4.0;
                let cool = 0.14 + self.rand() * 0.10;
                self.heat[y * c + x] = (avg - cool).max(0.0);
            }
        }
        self.heat.iter().map(|&h| fire_color(h)).collect()
    }
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
        let spin = t * 60.0 * self.p.speed; // degrees/sec at speed 1
        for y in 0..r {
            for x in 0..c {
                let ang = (y as f32 - cy).atan2(x as f32 - cx); // -PI..PI
                let hue = (ang / PI * 180.0 + spin).rem_euclid(360.0);
                f[y * c + x] = Rgb::from_hsv(hue, 1.0, 1.0);
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
        // spawn ~ (3% of cells) * speed new stars per frame, accumulating the fraction
        self.acc += n as f32 * 0.03 * self.p.speed;
        while self.acc >= 1.0 {
            self.acc -= 1.0;
            let i = (self.rand() * n as f32) as usize % n;
            self.level[i] = 0.85 + self.rand() * 0.15;
        }
        // fade every cell toward dark
        let decay = 0.04 * self.p.speed.max(0.1);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        self.level.iter().map(|&l| base.scale_f(l)).collect()
    }
}

// ── reactive: the board lights where you type, then fades — driven by the LIVE keyboard ──────

/// Reactive: polls the live keyboard (`GetAsyncKeyState`, a safe read — never injects) and ignites a
/// cell each time a key transitions down, which then fades. The device exposes no key→cell map, so a
/// pressed key hashes to a stable cell (same key always lights the same spot) with its immediate
/// neighbours catching a softer glow — a real "type and it answers" effect rather than a fixed
/// preset. Reads the layer colour. Off Windows the key read is a no-op (the board stays dark).
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
        // detect fresh key-downs and ignite their cells (+ a softer ring of neighbours)
        for vk in 1..256usize {
            let down = crate::capture::key_down(vk as i32);
            if down && !self.prev[vk] {
                // hash the vk to a stable cell (golden-ratio scramble for good spread)
                let cell = (vk.wrapping_mul(2654435761) >> 8) % n;
                self.level[cell] = 1.0;
                let (cy, cx) = (cell / c, cell % c);
                for (dy, dx) in [(0isize, 1isize), (0, -1), (1, 0), (-1, 0)] {
                    let ny = cy as isize + dy;
                    let nx = cx as isize + dx;
                    if ny >= 0 && ny < r as isize && nx >= 0 && nx < c as isize {
                        let ni = ny as usize * c + nx as usize;
                        self.level[ni] = self.level[ni].max(0.55);
                    }
                }
            }
            self.prev[vk] = down;
        }
        // fade — faster decay at higher speed
        let decay = 0.06 * self.p.speed.max(0.1);
        for l in self.level.iter_mut() {
            *l = (*l - decay).max(0.0);
        }
        self.level.iter().map(|&l| base.scale_f(l)).collect()
    }
}

// ── audiometer: the board is a VU meter driven by the LIVE audio peak from the speakers ──────

/// Audiometer: a level meter driven by the OS peak-sample value of the current output device. The
/// smoothed level fills the board from the bottom rows up; each column shimmers on a stable phase so
/// the bar dances like a spectrum, and the lit cells gradient from the layer colour (bottom) toward
/// white-hot (the crest). Opens the meter lazily on the first frame (so it lives on the thread that
/// streams it). With no signal / off Windows it idles near-dark — an honest silent meter, not a fake.
pub struct AudioMeter {
    meter: Option<crate::audio::MeterCtl>,
    opened: bool,
    smooth: f32,
    p: EffectParams,
}
impl AudioMeter {
    fn new(p: EffectParams) -> Self {
        AudioMeter {
            meter: None,
            opened: false,
            smooth: 0.0,
            p,
        }
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
        if !self.opened {
            self.meter = crate::audio::MeterCtl::open_default_render();
            self.opened = true;
        }
        let raw = self.meter.as_ref().map(|m| m.peak()).unwrap_or(0.0);
        // a little headroom boost so normal listening fills most of the board, then clamp
        let raw = (raw * 1.6).clamp(0.0, 1.0);
        // fast attack, slow release — the classic VU envelope
        let k = if raw > self.smooth { 0.6 } else { 0.12 };
        self.smooth += (raw - self.smooth) * k;
        let level = self.smooth.clamp(0.0, 1.0);
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

// ── THE COMPOSITOR — a stack of effect layers blended into one frame ─────────────────────
// This is the open Chroma Studio made real: each layer is its own generator + colour + region +
// blend, composited bottom-to-top into a single Vec<Rgb>. The compositor IS a FrameGen, so the
// existing animate/stream/mirror paths drive it with ZERO changes — they already take any FrameGen.

/// How a layer's pixels combine with what's beneath them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
#[derive(Clone, Debug)]
pub struct LayerDef {
    pub effect: String,
    pub color: Rgb,
    pub speed: f32,
    pub direction: u8,
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
            region: Vec::new(),
            blend: Blend::Normal,
            enabled: true,
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
                let gen = make_with(
                    &d.effect,
                    EffectParams {
                        speed: d.speed,
                        direction: d.direction,
                    },
                )
                .unwrap_or_else(|| Box::new(Solid));
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
}
