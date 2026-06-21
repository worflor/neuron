//! THE SPELLWEAVING MATERIAL ENGINE — the substance every cast / glyph / radial sigil is poured from.
//!
//! **White intent.** The core idea: effects are fields, and the material is how light behaves at
//! the field's surface — but the light itself is the user's WILL: pure white at the core,
//! diffusing into cool pale light, and breaking into SPECTRAL FIRE only where it crosses a
//! geometric edge. Colour is never pigment here; it is what happens to white light at a facet.
//!
//!   * **the glob** — summed falloff fields shaded through a soft iso-shoulder ARE metaballs:
//!     two blobs near each other neck together like droplets. Surface tension as math.
//!   * **the body** — a heat ramp from cool diffused glass-light to white-hot. No pigment,
//!     no honey: dim regions read as moonlit haze, dense cores burn pure white.
//!   * **the facets** — the field gradient (the surface normal) is QUANTIZED into N planes,
//!     like cut glass. Refraction happens along the facet, not the smooth normal — light bands
//!     break into geometric segments. This is the "hard light" feel, honestly derived.
//!   * **the fire** — real dispersion: each colour channel taps the field at a slightly
//!     different point along the facet (thin-prism on our own geometry), and where the taps
//!     disagree — edges, exactly — a spectral flash ignites, hued around the user's ACCENT.
//!     Gemology calls this *fire*: the rainbow a diamond throws. Still it is calm; moved, it
//!     shimmers — the material only flashes when your intent moves it.
//!   * **the rim** — a quiet accent-tinted edge concentration (the user's tint is an edge
//!     identity, never a body).
//!
//! Everything tunable lives in [`Material`]; themes are data (`material.toml`), so a reskin is a
//! file, not a fork. The overlay composites through [`shade`]; future surfaces (glance frames,
//! whiteboard, UI) draw from the same well.

/// WHICH MATERIAL a cast is poured from — each a distinct physical model, not a palette swap. The
/// engine shades the same density field through whichever surface is chosen, so a glyph cast in
/// `FluidThought` genuinely refracts and throws caustics where the same stroke in `MaterializedDesire`
/// burns and sparks. The names are states of mind made visible (the magic IS the user's intent).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Surface {
    DirectedIntent,     // the house pour — white intent, cut-glass prism fire (the origin)
    FluidThought,       // water — refraction, flowing caustics, a bright Fresnel edge
    MaterializedDesire, // fire — blackbody embers, rising turbulence, buoyant sparks
    GentleBreeze, // air — curl-flow wisps + drifting motes the breeze carries (the surprise: it's light)
    SuddenInsight, // (engine pick) electric — branching plasma filaments that crackle
    MoltenResolve, // (engine pick) lava — a dark cooled crust split by white-hot glowing cracks
}

impl Surface {
    /// Every surface, in gallery order (Directed Intent first — the origin/default).
    pub const ALL: [Surface; 6] = [
        Surface::DirectedIntent,
        Surface::FluidThought,
        Surface::MaterializedDesire,
        Surface::GentleBreeze,
        Surface::SuddenInsight,
        Surface::MoltenResolve,
    ];
    /// The stable kebab slug persisted in `app.toml` / passed from the GUI.
    pub fn slug(self) -> &'static str {
        match self {
            Surface::DirectedIntent => "directed-intent",
            Surface::FluidThought => "fluid-thought",
            Surface::MaterializedDesire => "materialized-desire",
            Surface::GentleBreeze => "gentle-breeze",
            Surface::SuddenInsight => "sudden-insight",
            Surface::MoltenResolve => "molten-resolve",
        }
    }
    /// The display name shown in the gallery.
    pub fn name(self) -> &'static str {
        match self {
            Surface::DirectedIntent => "Directed Intent",
            Surface::FluidThought => "Fluid Thought",
            Surface::MaterializedDesire => "Materialized Desire",
            Surface::GentleBreeze => "Gentle Breeze",
            Surface::SuddenInsight => "Sudden Insight",
            Surface::MoltenResolve => "Molten Resolve",
        }
    }
    /// A one-line vibe caption.
    pub fn blurb(self) -> &'static str {
        match self {
            Surface::DirectedIntent => "white intent · cut-glass prism fire",
            Surface::FluidThought => "water · refraction + flowing caustics",
            Surface::MaterializedDesire => "fire · blackbody embers + rising sparks",
            Surface::GentleBreeze => "air · curl-flow wisps carrying motes of light",
            Surface::SuddenInsight => "electric · branching plasma, crackling",
            Surface::MoltenResolve => "lava · dark crust split by white-hot cracks",
        }
    }
    /// Resolve a slug back to a surface (unknown → the house Directed Intent).
    pub fn from_slug(s: &str) -> Surface {
        Surface::ALL
            .into_iter()
            .find(|k| k.slug() == s)
            .unwrap_or(Surface::DirectedIntent)
    }
}

/// Per-pixel inputs to a material shader: the raw field here, its gradient, the dispersion taps
/// (glass), where the pixel is, and WHEN (animation). The overlay/preview fills this once per pixel;
/// each surface uses the parts its physics needs.
#[derive(Clone, Copy)]
pub struct Px {
    pub d: f32,  // field density here
    pub dr: f32, // field tapped ALONG the facet (glass dispersion)
    pub db: f32, // field tapped AGAINST the facet
    pub gx: f32, // gradient vector (the surface normal direction × strength)
    pub gy: f32,
    pub grad: f32,    // |gradient| — edge strength
    pub facet_u: f32, // the facet's hue position 0..1 (glass)
    pub heat: f32,    // the white-hot / core channel
    pub x: f32,       // pixel coords (procedural fields read these)
    pub y: f32,
    pub t: f32, // seconds — the material's animation clock
}

// ── THE FIELD-STACK — a generic, data-driven material engine ─────────────────────────────────
// A material is a RECIPE: an ordered stack of LAYERS, each a physics PRIMITIVE (the love is in the
// math) shaded through a colour RAMP and composited. Adding a material is DATA (a recipe); adding a
// new physics primitive (rare) is one `Field` arm. Every recipe surfaces its meaningful KNOBS so the
// user can reshape the substance live. Lean by design — 2-3 layers, modest octaves — so the cast
// stays lightweight while each look EMERGES from composition, never bespoke per-effect code.

pub const MAX_LAYERS: usize = 4;
pub const MAX_KNOBS: usize = 5;

/// A physics PRIMITIVE — a field over (density, gradient, position, time). Each is real math; a
/// material composes several. Most return a scalar intensity (→ a colour ramp); `Prism` is chromatic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Field {
    Body,     // the substance's own presence (the metaball shoulder + diffusion halo)
    Turb,     // turbulent fbm advected along −y (rising heat / churn), gated by the field
    Caustic,  // interference of two drifting flows → thin focused-light filaments
    Cracks,   // Worley F2−F1 → a connected fracture network
    Wisp,     // curl-flow–smeared cloud (divergence-free, soft)
    Sparks,   // a rising grid of brief twinkling motes (embers / carried light)
    Filament, // BRANCHING dielectric breakdown — real lightning (sudden strikes, forking channels)
    Prism,    // chromatic dispersion at edges (cut glass) — emits its own rainbow
    Fresnel,  // grazing-edge rim (the surface catching light)
}

/// A colour RAMP a scalar 0..1 maps through. `Accent*` ramps fold in the material's accent, so the
/// user's weave colour flows into the substance's light.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ramp {
    #[allow(dead_code)] // vocabulary: a cool-white body ramp, available to recipes that want it
    CoolWhite, // glass body: cool pale → white-hot
    Blackbody, // fire/lava: black → red → orange → yellow → white
    Water,     // deep teal → pale aqua
    Plasma,    // electric: indigo → cyan → white
    Air,       // pale cool grey-white
    AccentHot, // dark → accent → (the focused light / corona / motes wear the user's hue)
    White,
}

/// How a layer composites onto the stack beneath it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mix {
    Add,
    Screen,
}

/// One LAYER of a recipe — a primitive + its sampling params + colour + blend. All Copy/const.
#[derive(Clone, Copy, Debug)]
pub struct Layer {
    pub field: Field,
    pub scale: f32, // spatial frequency (or, for Prism, the spectrum spread; for Fresnel, the gain)
    pub speed: f32, // temporal rate
    pub detail: f32, // fbm octaves / sharpness (1..5)
    pub warp: f32,  // domain warp — branching (Filament) / turbulence
    pub gain: f32,  // intensity weight
    pub ramp: Ramp,
    pub mix: Mix,
}
impl Layer {
    pub const ZERO: Layer = Layer {
        field: Field::Body,
        scale: 0.04,
        speed: 0.0,
        detail: 3.0,
        warp: 0.0,
        gain: 1.0,
        ramp: Ramp::White,
        mix: Mix::Add,
    };
}

/// Which scalar of a [`Layer`] a UI knob drives (so the knob system is generic — a knob is data).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KnobKind {
    Scale,
    Speed,
    Detail,
    Warp,
    Gain,
}

/// One tunable KNOB a material surfaces to the UI: a label, which layer/param it edits, its range.
#[derive(Clone, Copy, Debug)]
pub struct Knob {
    pub label: &'static str,
    pub layer: u8,
    pub kind: KnobKind,
    pub min: f32,
    pub max: f32,
}
impl Knob {
    pub const ZERO: Knob = Knob {
        label: "",
        layer: 0,
        kind: KnobKind::Gain,
        min: 0.0,
        max: 1.0,
    };
}

/// The recipe one theme pours. All colour components are linear 0..1.
#[derive(Clone, Copy, Debug)]
pub struct Material {
    /// Which physical surface this cast is poured from (the shader dispatch).
    pub surface: Surface,
    /// The ACCENT — the user's tint: rims + the centre of the fire's spectrum. Default phosphor.
    pub accent: (f32, f32, f32),
    /// The accent's hue (0..1) — precomputed so the hot path never converts.
    pub accent_hue: f32,
    /// The diffused body tint (the dim end of the ramp): cool pale glass-light, NOT a pigment.
    pub body: (f32, f32, f32),
    /// The hot end of the ramp: pure white (or a whisper off it).
    pub white: (f32, f32, f32),
    /// Iso shoulder for the metaball surface — smaller = tighter/harder, larger = globbier.
    pub shoulder: f32,
    /// Rim gain (the quiet accent edge).
    pub rim: f32,
    /// Dispersion sample offset in pixels (fringe width at edges).
    pub dispersion: f32,
    /// Spectral-flash gain — how loudly edges burn into colour. 0 = colourless glass.
    pub fire: f32,
    /// Hue spread of the fire around the accent hue (0 = monochrome accent, 1 = full rainbow).
    pub spectrum: f32,
    /// How many planes the surface normal is cut into (the geometric refraction).
    pub facets: u32,
    /// INTENT — an opt-in communicated hue (0..1) the fire+rim bend toward when the magic is
    /// trying to TELL the user something (warm red/orange = "about to unset/destroy"). Per-draw,
    /// never themed: a caller clones the material and dials it in via [`Material::with_intent`].
    pub intent_hue: f32,
    /// How hard the intent pulls (0 = OFF, the normal white→aberration look; 1 = fully the
    /// communicated colour). Default 0 — the plain material is untouched.
    pub intent: f32,
    /// THE RECIPE — the ordered layer stack the field-stack engine composites. `n_layers == 0` means
    /// "use the tuned glass `shade`" (the whiteboard ink path); a cast always has a recipe.
    pub layers: [Layer; MAX_LAYERS],
    pub n_layers: u8,
    /// The knobs this material surfaces to the UI (a generic, data-driven control list).
    pub knobs: [Knob; MAX_KNOBS],
    pub n_knobs: u8,
}

impl Material {
    /// The house pour: white intent — cool diffused body, white-hot cores, phosphor-centred fire.
    /// `n_layers == 0`: the legacy glass `shade` path (whiteboard ink). Cast materials are built by
    /// [`Material::preset`].
    pub const fn neuron() -> Material {
        Material {
            surface: Surface::DirectedIntent,
            accent: (74.0 / 255.0, 242.0 / 255.0, 176.0 / 255.0),
            accent_hue: 0.435,        // phosphor #4af2b0
            body: (0.74, 0.82, 0.96), // soft cool WHITE — diffused light, not grey paint
            white: (1.0, 1.0, 1.0),
            shoulder: 0.10,
            rim: 1.0,
            dispersion: 2.2,
            fire: 1.05,
            spectrum: 0.8, // wide prismatic refraction at the edges
            facets: 12,
            intent_hue: 0.0,
            intent: 0.0, // OFF: the default material communicates nothing, just the white look
            layers: [Layer::ZERO; MAX_LAYERS],
            n_layers: 0,
            knobs: [Knob::ZERO; MAX_KNOBS],
            n_knobs: 0,
        }
    }

    /// Read the live value a knob drives.
    pub fn knob_value(&self, i: usize) -> f32 {
        if i >= self.n_knobs as usize {
            return 0.0;
        }
        let k = self.knobs[i];
        let l = &self.layers[k.layer as usize];
        match k.kind {
            KnobKind::Scale => l.scale,
            KnobKind::Speed => l.speed,
            KnobKind::Detail => l.detail,
            KnobKind::Warp => l.warp,
            KnobKind::Gain => l.gain,
        }
    }

    /// Drive a knob — write back into the layer param it targets (clamped to its range).
    pub fn set_knob(&mut self, i: usize, v: f32) {
        if i >= self.n_knobs as usize {
            return;
        }
        let k = self.knobs[i];
        let v = v.clamp(k.min, k.max);
        let l = &mut self.layers[k.layer as usize];
        match k.kind {
            KnobKind::Scale => l.scale = v,
            KnobKind::Speed => l.speed = v,
            KnobKind::Detail => l.detail = v,
            KnobKind::Warp => l.warp = v,
            KnobKind::Gain => l.gain = v,
        }
    }

    /// Apply an accent (linear rgb + its hue) — folds the user's weave colour into every accent ramp.
    pub fn with_accent(mut self, c: (f32, f32, f32)) -> Material {
        self.accent = c;
        self.accent_hue = hue_of(c);
        self
    }

    /// Bend this material toward a COMMUNICATED hue — the only time the magic wears colour for
    /// the user (warm `~0.02` red / `~0.08` orange = "this is about to UNSET/destroy"). Returns
    /// a copy; the caller draws ONE pass with it, then drops it. `amount` 0..1 is how loudly:
    /// the fire+rim shift to the hue and the spectrum tightens so it reads as one warning colour,
    /// not a rainbow. `amount == 0` leaves the material identical — tinting is strictly opt-in.
    #[inline]
    pub fn with_intent(mut self, hue: f32, amount: f32) -> Material {
        self.intent_hue = hue.rem_euclid(1.0);
        self.intent = amount.clamp(0.0, 1.0);
        self
    }
}

/// The live material: `material.toml` beside the exe if present (a theme is a FILE), else the house
/// pour. Read once per process — a theme change is a relaunch, not a hot path.
pub fn material() -> &'static Material {
    static M: std::sync::OnceLock<Material> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let mut m = Material::neuron();
        if let Ok(s) = std::fs::read_to_string("material.toml") {
            let get = |k: &str| -> Option<String> {
                s.lines()
                    .map(str::trim)
                    .find(|l| l.starts_with(k) && l[k.len()..].trim_start().starts_with('='))
                    .and_then(|l| l.split('=').nth(1))
                    .map(|v| v.trim().trim_matches('"').to_string())
            };
            let hex = |v: &str| -> Option<(f32, f32, f32)> {
                let h = v.trim_start_matches('#');
                if h.len() != 6 {
                    return None;
                }
                let p = |i: usize| {
                    u8::from_str_radix(&h[i..i + 2], 16)
                        .ok()
                        .map(|b| b as f32 / 255.0)
                };
                Some((p(0)?, p(2)?, p(4)?))
            };
            if let Some(c) = get("accent").as_deref().and_then(hex) {
                m.accent = c;
                m.accent_hue = hue_of(c);
            }
            if let Some(c) = get("body").as_deref().and_then(hex) {
                m.body = c;
            }
            if let Some(c) = get("white").as_deref().and_then(hex) {
                m.white = c;
            }
            if let Some(v) = get("shoulder").and_then(|v| v.parse().ok()) {
                m.shoulder = v;
            }
            if let Some(v) = get("rim").and_then(|v| v.parse().ok()) {
                m.rim = v;
            }
            if let Some(v) = get("dispersion").and_then(|v| v.parse().ok()) {
                m.dispersion = v;
            }
            if let Some(v) = get("fire").and_then(|v| v.parse().ok()) {
                m.fire = v;
            }
            if let Some(v) = get("spectrum").and_then(|v| v.parse().ok()) {
                m.spectrum = v;
            }
            if let Some(v) = get("facets").and_then(|v| v.parse().ok()) {
                m.facets = v;
            }
        }
        m
    })
}

// ── the LIVE weave accent — the one user-facing colour of the material ───────────────────────
//
// In this material colour is never pigment: it is what white light BREAKS INTO at an edge. So the
// user's "weave colour" can only honestly enter in one place — the ACCENT: the hue the spectral
// fire centres on (`accent_hue`) plus the quiet rim tint (`accent`). The themed [`material`] stays
// read-once from `material.toml` (the advanced theme is a FILE); THIS is the live override the
// SYSTEM → APPEARANCE picker drives, so retinting the cast is instant, not a relaunch. It feeds the
// fire centre + rim only — body, white-hot cores, facets, dispersion and spectrum width are
// untouched, so the substance still reads as white intent, just throwing the user's hue at its edges.

use std::sync::Mutex;

/// THE LIVE CAST MATERIAL — the resolved recipe the overlay pours every frame: the chosen surface,
/// the user's accent, and any live knob edits, ALL in one place. The SYSTEM → APPEARANCE gallery
/// drives it; the overlay clones it once per frame (a brief lock, never per-pixel). Starts as the
/// glass fallback; glue resolves it to the saved material at startup.
static LIVE: Mutex<Material> = Mutex::new(Material::neuron());

/// Pick the live weave SURFACE — rebuilds the recipe, preserving the current accent.
pub fn set_weave_surface(s: Surface) {
    let mut live = LIVE.lock().unwrap();
    let accent = live.accent;
    *live = preset(s).with_accent(accent);
}

/// The live weave surface.
pub fn weave_surface() -> Surface {
    LIVE.lock().unwrap().surface
}

/// Set the live weave accent (packed `0xRRGGBB`) — folds into every accent ramp + the prism hue.
pub fn set_weave_accent(rgb: u32) {
    let c = (
        ((rgb >> 16) & 0xFF) as f32 / 255.0,
        ((rgb >> 8) & 0xFF) as f32 / 255.0,
        (rgb & 0xFF) as f32 / 255.0,
    );
    let mut live = LIVE.lock().unwrap();
    let m = (*live).with_accent(c);
    *live = m;
}

/// Reset the weave accent to the stock phosphor.
pub fn clear_weave_accent() {
    set_weave_accent(0x4A_F2B0);
}

/// Drive one of the live material's KNOBS (the UI sliders reshape the substance live).
pub fn set_weave_knob(i: usize, v: f32) {
    LIVE.lock().unwrap().set_knob(i, v);
}

/// Reset the current material's knobs to its recipe defaults (keeps the chosen surface + accent).
pub fn reset_weave_knobs() {
    let s = weave_surface();
    set_weave_surface(s); // rebuilds preset(s).with_accent(current) → knobs back to defaults
}

/// The live material's surfaced knobs, for the UI: `(label, value, min, max)` each.
pub fn weave_knobs() -> Vec<(&'static str, f32, f32, f32)> {
    let live = LIVE.lock().unwrap();
    (0..live.n_knobs as usize)
        .map(|i| {
            let k = live.knobs[i];
            (k.label, live.knob_value(i), k.min, k.max)
        })
        .collect()
}

/// The live cast material — a by-value copy for the per-FRAME overlay setup (never per-pixel).
pub fn live_material() -> Material {
    *LIVE.lock().unwrap()
}

/// The accent INSTRUMENT OVERLAYS wear — teleport's scry frame, glance's frame, the landing pip, …:
/// the user's live WEAVE colour (linear rgb 0..1), so a recolour in settings flows through the whole
/// instrument family instead of a hardcoded phosphor green. ONE source of truth for that choice —
/// point it at the interface accent (`prefs::ui_accent`) instead if overlays should ever follow the
/// INTERFACE colour rather than the weave colour.
pub fn overlay_accent() -> (f32, f32, f32) {
    live_material().accent
}

// ── SHIMMER — the material's TEMPORAL life ───────────────────────────────────────────────────
//
// The doc's promise — "Still it is calm; moved, it shimmers" — made shared. Every Directed-Intent
// surface that ANIMATES (the scry frame's travelling glints + prism drift, an idle overlay glow, a
// future breathing rim) draws its tempo from HERE, in real-world Hz off ONE material epoch — never
// a per-frame counter. Frame rate is a render detail: a 60fps portal and a 30fps canvas must
// shimmer at the SAME perceived speed, so motion lives in wall-clock SECONDS, not frames. And the
// tempos are deliberately SLOW — this substance is calm; it breathes and drifts, it never strobes.

/// The shared material epoch — when the substance woke. One clock for every surface, so they drift
/// in loose sympathy instead of each off its own start.
fn shimmer_epoch() -> std::time::Instant {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(std::time::Instant::now)
}

/// The material's characteristic TEMPOS, in Hz (cycles — or for an orbit, revolutions — per
/// SECOND). Named and shared so the whole substance's sense of motion is tuned in ONE place; slow
/// on purpose. Reach for these over a bare literal: a new effect inherits the material's calm.
pub mod tempo {
    /// Prism-hue drift along a rim — the white→aberration wander. ~once per 11s: barely moving.
    pub const DRIFT: f32 = 0.09;
    /// Glints orbiting a perimeter, in REVOLUTIONS/sec. ~once per 7s — a slow sliding gleam, not a
    /// chase light. (This replaced the scry strobe: a per-frame 0.035 that ran ≈2.2 rev/s.)
    pub const ORBIT: f32 = 0.15;
    /// A gentle breath — an idle alpha/glow swell. ~0.3Hz: a slow inhale, felt more than seen.
    pub const BREATH: f32 = 0.3;
}

/// The PHASE (radians, wrapped to one turn) of a motion at `hz` cycles/sec on the shared material
/// clock — feed it to `sin`/`cos`. Frame-rate INDEPENDENT: it advances the same per real second at
/// any repaint rate. Wrapped via the fractional turn, so it stays precise for any uptime and the
/// wrap is seamless (`sin` is periodic). Add a spatial term before `sin` for a travelling wave.
#[inline]
pub fn phase(hz: f32) -> f32 {
    let secs = shimmer_epoch().elapsed().as_secs_f64();
    let turn = (secs * hz as f64).rem_euclid(1.0); // 0..1 of a revolution — bounded, precise
    (turn * std::f64::consts::TAU) as f32
}

/// Wall-clock SECONDS since the material woke — the animation clock the per-pixel surfaces read
/// (fire flicker, water ripple, breeze drift). Wrapped to a long period so `sin`/noise stay precise
/// over any uptime. Compute it ONCE per frame and pass it into every `Px`, never per-pixel.
#[inline]
pub fn seconds() -> f32 {
    // wrap at 4096s so the f32 keeps fractional precision indefinitely (the motions are periodic).
    (shimmer_epoch().elapsed().as_secs_f64() % 4096.0) as f32
}

/// A 0..1 sine SWELL at `hz` on the shared clock — the common "breathe between two levels" need
/// (alpha, scale, glow). Phase 0 sits at the midpoint, rising.
#[inline]
pub fn swell(hz: f32, lo: f32, hi: f32) -> f32 {
    lo + (hi - lo) * (0.5 + 0.5 * phase(hz).sin())
}

/// The metaball iso-shoulder: a branch-free rational sigmoid. Summed gaussian falloffs pushed
/// through this become droplets that NECK when they approach — the glob, as a consequence.
#[inline]
pub fn shoulder(d: f32, k: f32) -> f32 {
    let d2 = d * d;
    d2 / (d2 + k)
}

/// Quantize a gradient into one of `n` facet planes (cut glass): returns the facet's unit
/// direction + the facet's position 0..1 around the circle (the fire's hue picks from this).
#[inline]
pub fn facet(gx: f32, gy: f32, n: u32) -> (f32, f32, f32) {
    let a = gy.atan2(gx);
    let step = std::f32::consts::TAU / n.max(1) as f32;
    let q = (a / step).round() * step;
    (q.cos(), q.sin(), (q / std::f32::consts::TAU + 0.5).fract())
}

/// A hue (0..1, wrapping) at full value — the fire's palette. Cheap hexcone, no powf.
#[inline]
pub fn hue_rgb(h: f32) -> (f32, f32, f32) {
    let h = (h.fract() + 1.0).fract() * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    }
}

/// The hue (0..1) of a packed `0xRRGGBB` colour — the whiteboard's material brush turns the chosen
/// swatch into the material's ACCENT (so the user's pen colour drives the spectral fire).
pub fn hue_u32(c: u32) -> f32 {
    hue_of((
        ((c >> 16) & 0xFF) as f32 / 255.0,
        ((c >> 8) & 0xFF) as f32 / 255.0,
        (c & 0xFF) as f32 / 255.0,
    ))
}

/// The hue (0..1) of an rgb colour — used once at theme load, never in the hot path.
fn hue_of((r, g, b): (f32, f32, f32)) -> f32 {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    if d <= 1e-6 {
        return 0.0;
    }
    let h = if max == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h / 6.0
}

/// Shade ONE pixel of the directed-intent surface — white intent.
///
/// * `d` — the field here (summed splat density);
/// * `dr`, `db` — the field tapped a touch ALONG / AGAINST the FACET direction (pass `d` for
///   both when the gradient is negligible);
/// * `heat` — the white-hot channel (cores, text) — ignites the ramp AND adds directly so a
///   pure-white mask (a label) never vanishes;
/// * `grad` — gradient magnitude (edge strength); `facet_u` — the facet's hue position 0..1.
///
/// Returns linear (r, g, b, lum); the caller owns alpha/premultiply/dusk.
#[inline]
pub fn shade(
    d: f32,
    dr: f32,
    db: f32,
    heat: f32,
    grad: f32,
    facet_u: f32,
    m: &Material,
) -> (f32, f32, f32, f32) {
    // the surface, per prism tap — taps disagree exactly at edges
    let sg = shoulder(d, m.shoulder);
    let sr = shoulder(dr, m.shoulder);
    let sb = shoulder(db, m.shoulder);
    // the DIFFUSION HALO: a soft glow bleeding past the hard surface so the substance GLOWS like
    // light, not paint (no dead cut edge). Achromatic — it rides the body colour, injects no hue.
    let halo = (d * 0.55).min(0.85);
    // the body is the DIFFUSED LIGHT: a soft cool WHITE that ignites to pure white-hot as it
    // densifies. The colour is NOT in the body — it lives entirely in the refraction (the fire).
    let t = (d * 0.7 + heat * 1.4).min(1.5);
    let u = (t / 1.5).clamp(0.0, 1.0);
    let u = u * u * (3.0 - 2.0 * u); // smoothstep — no banding at the joint
    let mut br = m.body.0 + (m.white.0 - m.body.0) * u;
    let mut bg = m.body.1 + (m.white.1 - m.body.1) * u;
    let mut bb = m.body.2 + (m.white.2 - m.body.2) * u;
    // INTENT warms the BODY too (the one sanctioned exception to "tint = edge, never body"): when
    // the magic is COMMUNICATING, the whole substance reads in the spoken colour — not just its
    // edges — so an "about to unset/destroy" mark is unmistakably warm. Brightness-preserving
    // (blends toward the hue at the body's own luma) and strictly opt-in: intent == 0 skips this
    // entirely, so the normal white→aberration look is byte-identical.
    if m.intent > 0.0 {
        let (ir, ig, ib) = hue_rgb(m.intent_hue);
        let bl = (br + bg + bb) / 3.0;
        let k = m.intent * 0.65;
        br += (ir * bl - br) * k;
        bg += (ig * bl - bg) * k;
        bb += (ib * bl - bb) * k;
    }
    // body emission per prism tap + the halo (the dispersion edging lives in the tap difference;
    // the halo is the same across taps, so it never tints — only the surface refracts).
    let mut r = br * (sr + halo * 0.5);
    let mut g = bg * (sg + halo * 0.5);
    let mut b = bb * (sb + halo * 0.5);
    // THE FIRE — the magic: white light breaking into PRISMATIC refraction wherever the taps
    // disagree (every edge, banded geometrically by the facet crossed). Blooms bright + saturated.
    // When INTENT is dialled in, the spectrum's centre slides to the communicated hue and its
    // spread narrows toward it — the rainbow collapses into one spoken colour (intent == 0: the
    // accent hue and full spread, identical to before).
    let fringe = (sr - sg).abs() + (sg - sb).abs() + (sr - sb).abs();
    let hue_c = m.accent_hue + (m.intent_hue - m.accent_hue) * m.intent;
    let spread = m.spectrum * (1.0 - 0.85 * m.intent);
    let (fr, fg, fb) = hue_rgb(hue_c + (facet_u - 0.5) * spread);
    let fire = (fringe * m.fire * 1.7).min(1.2);
    r += fr * fire;
    g += fg * fire;
    b += fb * fire;
    // the hard-light RIM: accent-tinted edge concentration (the surface catching the light) —
    // bent toward the intent hue when communicating (intent == 0: the bare accent, as before).
    let (mut ar, mut ag, mut ab) = m.accent;
    if m.intent > 0.0 {
        let (ir, ig, ib) = hue_rgb(m.intent_hue);
        ar += (ir - ar) * m.intent;
        ag += (ig - ag) * m.intent;
        ab += (ib - ab) * m.intent;
    }
    let rim = (grad * m.rim * (0.3 + sg)).min(0.85);
    r += ar * rim;
    g += ag * rim;
    b += ab * rim;
    // WHITE-HOT CORE: only the DENSEST core burns flat white (heat², a tight star), so the
    // diffusion + fire around it stay coloured instead of being washed out by a white blob.
    let c = (heat * heat * 1.1).min(1.15);
    r += c;
    g += c;
    b += c;
    let lum = (sg.max(sr).max(sb) + halo * 0.45 + c + rim * 0.6 + fire * 0.7).min(1.0);
    (r, g, b, lum)
}

// ── procedural noise toolkit — raw math, no deps, deterministic ───────────────────────────────
// The substance of every non-glass material: value noise + its fractal sum (fbm), Worley cells
// (lava cracks), a curl flow (air), a blackbody emission ramp (fire/lava). Cheap enough for the
// per-pixel overlay; deterministic so a cast looks the same every frame at the same time.

#[inline]
fn hashi(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^= x >> 16;
    x
}
#[inline]
fn hash2(ix: i32, iy: i32) -> f32 {
    let h = hashi((ix as u32).wrapping_mul(0x9E37_79B1) ^ (iy as u32).wrapping_mul(0x85EB_CA77));
    (h >> 8) as f32 / (1u32 << 24) as f32
}
#[inline]
fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
/// 2-D value noise (smooth-interpolated hashed lattice), 0..1.
fn vnoise(x: f32, y: f32) -> f32 {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let (fx, fy) = (x - xi as f32, y - yi as f32);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let a = hash2(xi, yi);
    let b = hash2(xi + 1, yi);
    let c = hash2(xi, yi + 1);
    let d = hash2(xi + 1, yi + 1);
    let ab = a + (b - a) * ux;
    let cd = c + (d - c) * ux;
    ab + (cd - ab) * uy
}
/// Fractal Brownian motion — `oct` octaves of value noise, normalised to 0..1.
fn fbm(mut x: f32, mut y: f32, oct: u32) -> f32 {
    let mut sum = 0.0;
    let mut amp = 0.5;
    let mut norm = 0.0;
    for _ in 0..oct {
        sum += amp * vnoise(x, y);
        norm += amp;
        amp *= 0.5;
        x *= 2.02;
        y *= 2.02;
    }
    sum / norm.max(1e-6)
}
/// Worley F1 + F2 (the two nearest jittered feature points) + the owning cell's hash. Lava cracks
/// live on the cell BOUNDARIES, where F2−F1 → 0 (a point equidistant from two cells) — a connected
/// fracture network, not isolated dots.
fn cellular(x: f32, y: f32) -> (f32, f32, f32) {
    let (xi, yi) = (x.floor() as i32, y.floor() as i32);
    let mut f1 = 9.0;
    let mut f2 = 9.0;
    let mut cell = 0.0;
    for dy in -1..=1 {
        for dx in -1..=1 {
            let (cx, cy) = (xi + dx, yi + dy);
            let px = cx as f32 + hash2(cx, cy);
            let py = cy as f32 + hash2(cx ^ 0x1234, cy ^ 0x5678);
            let dd = (px - x) * (px - x) + (py - y) * (py - y);
            if dd < f1 {
                f2 = f1;
                f1 = dd;
                cell = hash2(cx + 99, cy - 77);
            } else if dd < f2 {
                f2 = dd;
            }
        }
    }
    (f1.sqrt(), f2.sqrt(), cell)
}
/// Curl of an fbm potential → a divergence-free flow direction (air streamlines, no sources/sinks).
fn curl(x: f32, y: f32, t: f32) -> (f32, f32) {
    let e = 0.6;
    let p = |a: f32, b: f32| fbm(a + t, b - t * 0.5, 3);
    let dpdy = (p(x, y + e) - p(x, y - e)) / (2.0 * e);
    let dpdx = (p(x + e, y) - p(x - e, y)) / (2.0 * e);
    (dpdy, -dpdx)
}
/// A hot-body emission ramp: temperature 0..~1.2 → black → deep-red → orange → yellow → white.
/// A Planckian-locus emission ramp: temperature 0..~1.3 → black → deep-red → orange → straw → white.
/// Smoothstep shoulders (no Mach banding); green rises before red saturates; the white-hot tip
/// desaturates toward white instead of clamping to a flat plateau.
#[inline]
fn blackbody(t: f32) -> (f32, f32, f32) {
    let t = t.max(0.0);
    let r = smoothstep(0.0, 0.45, t);
    let g = smoothstep(0.30, 1.05, t).powf(1.4);
    let b = smoothstep(0.78, 1.30, t);
    let w = smoothstep(0.90, 1.25, t) * 0.30; // desaturate the hottest tip toward white
    (r + w * (1.0 - r), g, b + w * (1.0 - b))
}

// ── the surfaces — each its own physics, all over the same field ─────────────────────────────

/// Map a scalar 0..1 through a colour [`Ramp`] — the accent ramps fold in the material's hue, so the
/// user's weave colour lives in the substance's focused light / corona / motes.
#[inline]
fn ramp(id: Ramp, t: f32, m: &Material) -> (f32, f32, f32) {
    let t = t.clamp(0.0, 1.0);
    let l3 = |a: (f32, f32, f32), b: (f32, f32, f32), k: f32| {
        (
            a.0 + (b.0 - a.0) * k,
            a.1 + (b.1 - a.1) * k,
            a.2 + (b.2 - a.2) * k,
        )
    };
    let (ar, ag, ab) = m.accent;
    match id {
        Ramp::CoolWhite => l3((0.30, 0.40, 0.55), (1.0, 1.0, 1.0), smoothstep(0.0, 1.0, t)),
        Ramp::Blackbody => blackbody(t * 1.25),
        Ramp::Water => l3((0.02, 0.12, 0.20), (0.45, 0.90, 1.0), t),
        Ramp::Plasma => l3((0.18, 0.22, 0.85), (0.80, 0.95, 1.0), t),
        Ramp::Air => l3((0.55, 0.62, 0.74), (0.88, 0.93, 1.0), t),
        Ramp::AccentHot => l3((ar * 0.10, ag * 0.10, ab * 0.10), (ar, ag, ab), t),
        Ramp::White => (1.0, 1.0, 1.0),
    }
}

/// Embers / carried motes — a grid of points that rise, drift, and WINK (a sharp specular flash, not
/// a slow pulse), each leaving a faint vertical motion-blur trail. `rise` advects them up, `drift`
/// slides them sideways; `density` is the fraction of cells that ever light.
fn motes(x: f32, y: f32, t: f32, cell: f32, rise: f32, drift: f32, density: f32, twk: f32) -> f32 {
    let (dx, dy) = (x + t * drift, y - t * rise);
    let (cx, cy) = ((dx / cell).floor() as i32, (dy / cell).floor() as i32);
    if hash2(cx, cy) < 1.0 - density {
        return 0.0;
    }
    // a sharp WINK (dust catching a beam glints on/off; it doesn't slowly pulse)
    let life = smoothstep(0.55, 1.0, (t * twk + hash2(cx + 7, cy - 3) * 6.283).sin());
    let mx = cx as f32 * cell + cell * 0.5 + (hash2(cx + 1, cy) * cell * 0.6 - cell * 0.3);
    let my = cy as f32 * cell + cell * 0.5 + (hash2(cx + 2, cy) * cell * 0.4 - cell * 0.2);
    let ox = dx - mx;
    let oy = (dy - my) / 2.2; // stretch along travel → a comet trail for ~free
    let dd = (ox * ox + oy * oy).sqrt();
    (1.0 - dd / (cell * 0.30)).max(0.0).powi(2) * life
}

/// REAL LIGHTNING — dielectric breakdown, not glowing noise. The bolt is a 1-D CHANNEL (a noise-
/// displaced centreline threaded along the field gradient) measured by perpendicular DISTANCE, lit by
/// a Lorentzian `1/(1+d²k)` so it has a sub-pixel white-hot core and a soft corona tail. It FORKS (a
/// recursive domain-warp splits a dimmer side-channel; `max` of the two = a main bolt + a branch). It
/// STRIKES (jumps to a fresh, per-strike-jittered path every ~0.13s + a decaying flash), and the
/// previous strike lingers as a fading afterglow — a storm, not a strobe. Returns (corona, white-hot core).
fn lightning(p: &Px, l: &Layer, _m: &Material) -> (f32, f32) {
    // a SOFT presence gate so the bolt + bloom escape the body into the void (lightning's drama)
    let gate = smoothstep(-0.25, 0.5, p.d);
    if gate <= 0.001 {
        return (0.0, 0.0);
    }
    let t = p.t * l.speed.max(0.05);
    // a jittered strike clock — dark lulls broken by sudden strikes (a storm at night, not a strobe).
    // ~0.24s base so a small gallery tile actually CATCHES a live bolt most frames; the fire-time inside
    // each slot is jittered per-strike so the rhythm is uneven and the next flash STARTLES.
    let phase = t / 0.24;
    let k0 = phase.floor();
    let frac0 = phase - k0;
    let fire_at = hash2(k0 as i32, 41) * 0.45; // 0..0.45 into the slot the bolt actually fires
    let frac = ((frac0 - fire_at) / (1.0 - fire_at).max(0.2)).clamp(0.0, 1.0);
    // the bolt travels ALONG the field gradient (threads through the glyph)
    let gl = (p.gx * p.gx + p.gy * p.gy).sqrt().max(1e-3);
    let (ax, ay) = (p.gx / gl, p.gy / gl);
    let core_k = 11.0;

    // one strike's channel HIERARCHY — a main bolt + a daughter + a granddaughter fork, each thrown off
    // the PREVIOUS warped centreline (a self-similar tree, not parallel scratches) and each gated by an
    // along-channel mask so branches START and STOP like real forks. Returns (core intensity, wide bloom).
    let strike_chan = |seed: f32, amp: f32, tilt: f32, oct: u32| -> (f32, f32) {
        let (rx, ry) = (
            ax * tilt.cos() - ay * tilt.sin(),
            ax * tilt.sin() + ay * tilt.cos(),
        );
        let s = p.x * rx + p.y * ry; // along the bolt
        let n = -p.x * ry + p.y * rx; // perpendicular offset
                                      // a WIGGLIER centreline so the bolt visibly snakes across a single blob (the old freq barely
                                      // bent over a tile's width, so forks read as parallel smudges).
        let w1 = fbm(s * l.scale * 1.5, seed, 2) - 0.5;
        let s2 = s + w1 * l.warp * 30.0;
        let w2 = fbm(s2 * l.scale * 1.5 + seed, seed * 1.3, oct) - 0.5;
        let center = amp * (w1 * 0.7 + w2 * 0.5);
        let jit = (fbm(s * l.scale * 8.0 + t * 30.0, seed, 1) - 0.5) * 0.12;
        let lor = |dn: f32| 1.0 / (1.0 + dn * dn * core_k);
        // gen 0 — the main channel
        let g0 = lor(n - center - jit);
        // gen 1 — a daughter that LEAVES the trunk at a clear angle (doubled throw) and reaches, only
        // where its mask opens (lowered threshold ⇒ branches actually appear).
        let f1 = center + amp * (fbm(s2 * l.scale * 1.6 + seed * 2.0, seed, 2) - 0.5) * 2.0;
        let m1 = smoothstep(0.30, 0.55, fbm(s * l.scale * 0.7 + 3.1, seed * 1.7, 2));
        let g1 = lor(n - f1 - jit) * m1;
        // gen 2 — a granddaughter off the daughter, finer + dimmer + rarer
        let f2c = f1 + amp * (fbm(s2 * l.scale * 3.0 + seed * 4.0, seed * 0.7, 2) - 0.5) * 1.6;
        let m2 = smoothstep(0.40, 0.65, fbm(s * l.scale * 1.3 + 7.7, seed * 2.3, 2));
        let g2 = lor(n - f2c - jit) * m2;
        let core = g0.max(g1 * 0.55).max(g2 * 0.30);
        // a NARROW corona (≈2× the core, not 5×) gated to the strike instant — so the glow is a tight
        // sheath on the bolt, not a round blob smeared across the whole tile.
        let bloom = 1.0 / (1.0 + (n - center) * (n - center) * core_k * 0.22);
        (core, bloom)
    };

    // current strike (per-strike jittered amplitude + axis tilt)
    let (r1, r2) = (hash2(k0 as i32, 11), hash2(k0 as i32, 23));
    let (core_now, bloom_now) = strike_chan(
        k0 * 17.31,
        0.6 + 0.8 * r1,
        (r2 - 0.5) * 0.5,
        l.detail as u32,
    );
    // STRIKE envelope — a fast attack to a blinding peak, then a CUBIC decay to a near-zero floor that
    // holds for the rest of the slot (the lull). The eye gets dark-then-FLASH instead of a steady strobe.
    let attack = smoothstep(0.0, 0.04, frac);
    let decay = 1.0 - smoothstep(0.04, 0.30, frac);
    let flash_now = attack * decay * decay;
    // the PREVIOUS strike's white-hot channel burns DOWN across the lull — the magnesium afterimage that
    // makes a strike feel like it scorched the retina (analytic exponential, no state buffer).
    let (rp1, rp2) = (hash2((k0 - 1.0) as i32, 11), hash2((k0 - 1.0) as i32, 23));
    let (core_prev, _) = strike_chan((k0 - 1.0) * 17.31, 0.6 + 0.8 * rp1, (rp2 - 0.5) * 0.5, 2);
    let after = (-frac * 4.0).exp() * 0.55;
    // crackle rides only the LIT channel (fast, low-amplitude buzz) — dark and silent between strikes.
    let crackle = 0.9 + 0.1 * (t * 60.0).sin();
    // an ALWAYS-ON dim channel floor so a cast glyph stays a faint readable crackling line (legible ink),
    // while the corona/flash still lull to dark for the drama.
    let ink_floor = core_now * 0.13;

    let corona = ((core_now * 0.5 + bloom_now * 0.22) * flash_now * crackle
        + core_prev * 0.30 * after
        + ink_floor)
        * gate;
    let hot = (core_now.powi(3) * flash_now * crackle * 2.0
        + core_prev.powi(3) * after * 0.5
        + ink_floor * 0.4)
        * gate; // the blinding sub-pixel core
    (corona.min(1.6), hot.min(1.4))
}

/// DIRECTED INTENT — WHITE magic: a pure white body + a STABLE chromatic aberration at the edges. The
/// old version drove the hue from the facet-quantised taps, which SNAP as a stroke curves — that was the
/// "random hard colour changes". This version is colour-stable by construction: the body is white, and
/// the only colour is a thin CA fringe whose hue depends on the SMOOTH surface normal (gx), never on the
/// jumpy facet. An edge facing +x gets a RED fringe; facing −x, a CYAN fringe (a lens splitting R/B
/// horizontally). `l.scale` = aberration strength; `l.gain` = body brightness.
fn prism(p: &Px, l: &Layer, m: &Material) -> (f32, f32, f32, f32) {
    let sg = shoulder(p.d, m.shoulder); // the WHITE luminance (1 at the spine → 0 at the rim)
                                        // edge strength — where the density is changing (the silhouette). `p.grad` is a SMOOTH edge measure
                                        // (no facet quantisation), so nothing here can jump as the stroke turns.
    let edge = smoothstep(0.05, 0.40, p.grad);
    // CHROMATIC ABERRATION: a fixed-direction (horizontal) channel split — the hallmark lens/screen CA.
    // ca_dir = the x-component of the SMOOTH normal (−1..1). +x-facing edge → +red/−blue (red fringe);
    // −x-facing → −red/+blue (cyan fringe). Smooth ⇒ STABLE: it sweeps, it never snaps.
    let gl = (p.gx * p.gx + p.gy * p.gy).sqrt().max(1e-4);
    let ca_dir = p.gx / gl;
    let ca = edge * l.scale; // strength
                             // refractive shimmer — the white body breathes like living glass (subtle, slow, stable).
    let shimmer = 0.93 + 0.07 * vnoise(p.x * 0.05 + phase(tempo::DRIFT) * 1.4, p.y * 0.05);
    let white = sg * l.gain * shimmer; // the WHITE body
                                       // the white-hot CORE at the dense spine (the magic's heart). `sg` already makes the whole body white;
                                       // this just blazes the centre brighter.
    let core = (p.heat * p.heat * 1.2).min(1.2);
    // body WHITE everywhere + the signed CA fringe at the edges (red one flank, cyan the other).
    let mut r = white + core + ca_dir * ca;
    let mut g = white + core;
    let mut b = white + core - ca_dir * ca;
    // a SUBTLE, STABLE weave-accent rim so changing the weave colour tints the silhouette — confined to
    // the edge, off the white body, and not hue-jumpy. On a communicated intent (refusal) it swings warm.
    let (ar, ag, ab) = m.accent;
    let (tr, tg, tb) = if m.intent > 0.0 {
        hue_rgb(m.intent_hue)
    } else {
        (ar, ag, ab)
    };
    let rim = edge * (0.14 + 0.4 * m.intent);
    r += (tr - r) * rim;
    g += (tg - g) * rim;
    b += (tb - b) * rim;
    (r.max(0.0), g.max(0.0), b.max(0.0), (white + core).min(1.0))
}

/// The directional wave set the water surface is built from (kx, ky, frequency) — incommensurate so the
/// sum never loops. Shared by the caustic FOCUS and its analytic gradient (refraction).
const WATER_DIRS: [(f32, f32, f32); 4] = [
    (1.0, 0.0, 1.0),
    (-0.5, 0.866, 1.37),
    (-0.42, -0.908, 1.71),
    (0.7, 0.71, 2.1),
];

/// Caustics by wave FOCUSING (not mere crest-summing): real caustics are brightest where the surface is
/// CONCAVE and acts as a converging lens — i.e. where the Laplacian ∇²h > 0. For a sum of plane waves the
/// Laplacian is analytic: ∇²sin(k·x − ωt) = −|k|²·sin(...), the SAME `sin` reused. So this costs no
/// octaves, tracks the moving surface coherently, and gives thin nervous light filaments that brighten
/// and dim as the waves sweep through focus — the alive pool-floor net, sharper than the old crest⁶.
fn water_caustic(x: f32, y: f32, t: f32) -> f32 {
    let mut lap = 0.0;
    for (kx, ky, f) in WATER_DIRS {
        let k2 = f * f * (kx * kx + ky * ky);
        lap += -k2 * (f * (kx * x + ky * y) - t * f * 1.1).sin();
    }
    let focus = (lap / WATER_DIRS.len() as f32).max(0.0); // only CONVERGING patches gather light
    (focus * focus * 2.4).min(1.0) // square ⇒ thin bright veins, not round blobs
}

/// The water surface SLOPE (∇h) — the analytic derivative of the same wave sum (cos of the reused arg).
/// Refracts the caustic sample (the floor's light shifts where the surface above tilts) so the light-net
/// SWIMS under a moving skin instead of sitting in a fixed lattice.
fn water_grad(x: f32, y: f32, t: f32) -> (f32, f32) {
    let (mut gx, mut gy) = (0.0, 0.0);
    for (kx, ky, f) in WATER_DIRS {
        let c = (f * (kx * x + ky * y) - t * f * 1.1).cos();
        gx += kx * f * c;
        gy += ky * f * c;
    }
    let n = WATER_DIRS.len() as f32;
    (gx / n, gy / n)
}

/// Evaluate ONE layer at this pixel → its (rgb, intensity-for-luminance). The generic per-primitive
/// physics; a material is just a stack of these.
fn eval_layer(l: &Layer, p: &Px, m: &Material) -> (f32, f32, f32, f32) {
    let pres = shoulder(p.d, m.shoulder);
    let (t, x, y) = (p.t, p.x, p.y);
    match l.field {
        Field::Body => {
            let halo = (p.d * 0.55).min(0.85);
            let s = (pres + halo * 0.5) * l.gain;
            let (cr, cg, cb) = ramp(l.ramp, smoothstep(0.0, 1.3, p.d + p.heat * 1.4), m);
            let core = (p.heat * p.heat).min(1.0);
            (
                cr * s + core,
                cg * s + core,
                cb * s + core,
                (s * 0.7 + core).min(1.0),
            )
        }
        Field::Turb => {
            // A CONTINUOUS glowing flame that FLICKERS smoothly and tapers as it rises — not a flat line
            // (too solid) and not hard-threshold tongues (patchy/ugly). The brightness is modulated softly
            // by a rising noise field (the flicker) and dimmed toward the rising tips (the lick), so it
            // reads as a living flame with no dead gaps.
            let up = (p.gy / (p.grad + 1e-3)).clamp(0.0, 1.0); // 0 at the fuel base → 1 at the cool tips
            let fx = x * l.scale * 0.9;
            let fy = y * l.scale * 0.42 - t * l.speed * 1.7; // gentle upward rise (was too fast/animated)
                                                             // SLOW flow evolution ⇒ kills the fleshy membrane UNDULATION (the swirling sheet read as
                                                             // moving flesh, not fire); the flame rises through a near-stable turbulence instead.
            let (cx, cy) = curl(fx, fy, t * l.speed * 0.12);
            let warp = l.warp.max(8.0);
            let n = fbm(fx + cx * warp * 0.05, fy + cy * warp * 0.05, 3); // rising flame field, less warp
                                                                          // a FAST, SUBTLE brightness flicker (real fire trembles fast) layered on the slow rise — so it
                                                                          // reads as flickering FLAME, not a slow undulating membrane.
            let flicker = 0.86 + 0.14 * fbm(fx * 1.6, fy * 1.6 + t * l.speed * 6.0, 2);
            let flick = (0.5 + 0.5 * n) * flicker;
            let taper = (1.0 - up * 0.55).max(0.15);
            let dens = pres.max(0.08) * flick * taper * l.gain * 1.5;
            // TEMPERATURE = orange flame BODY, WHITE only at the fed core (high heat), deep-RED at the
            // cool rising tips.
            let temp = (0.34 + 0.26 * n + p.heat * 0.7 - up * 0.42).clamp(0.0, 1.15);
            let (mut cr, mut cg, mut cb) = ramp(l.ramp, temp, m);
            // the accent bends the flame's "want" across the orange body (off the white core + dark tips).
            let want = smoothstep(0.18, 0.55, temp) * (1.0 - smoothstep(0.70, 0.98, temp)) * 0.5;
            let (ar, ag, ab) = m.accent;
            cr += (ar - cr) * want;
            cg += (ag - cg) * want;
            cb += (ab - cb) * want;
            (cr * dens, cg * dens, cb * dens, (dens * 0.95).min(1.0))
        }
        Field::Caustic => {
            // focused-light net (analytic ∇²h, §water_caustic) + a touch of fbm break-up so it isn't too
            // regular. REFRACTION: sample displaced by the wave SLOPE (×`warp` knob) so the light-net
            // swims under the moving skin; a whisper of low-gain CURL adds water-correct eddies (no
            // sources/sinks). The accent lives in the focused light.
            let (sx, sy) = (x * l.scale * 13.0, y * l.scale * 13.0); // finer net ⇒ veins cross a thin stroke
            let ct = t * l.speed * 4.0;
            let (hgx, hgy) = water_grad(sx, sy, ct);
            let refr = l.warp; // the "refraction" knob — how hard the skin bends the light beneath it
            let (ex, ey) = curl(x * 0.3, y * 0.3, t * 0.1);
            let caustic = water_caustic(sx + hgx * refr + ex * 0.2, sy + hgy * refr + ey * 0.2, ct);
            let breakup = 0.7 + 0.3 * fbm(x * l.scale * 2.0, y * l.scale * 2.0 - t * l.speed, 2);
            // gate the focused light INSIDE the body so it glows from within — caustics haloing past the
            // rim would blur a cast letter (legibility); inside, the ink glows like lit water. Boosted +
            // sharpened (powf<1 lifts the dim net, the ×1.7 makes the bright veins POP) so the water reads
            // as light DANCING through it, not a flat teal tube.
            let c = ((caustic.powf(0.75) * breakup) * 1.7).min(1.3)
                * pres
                * smoothstep(0.10, 0.50, p.d);
            let i = c * l.gain;
            // the bright vein cores carry the WEAVE COLOUR (focused light wearing the user's hue) so a
            // red weave gives red-lit water, a violet weave violet-lit — visible tint response — while
            // the dim net stays watery teal.
            let (mut cr, mut cg, mut cb) = ramp(l.ramp, c.min(1.0), m);
            let (ar, ag, ab) = m.accent;
            let pop = smoothstep(0.5, 1.05, c);
            cr += (ar - cr) * pop * 0.7;
            cg += (ag - cg) * pop * 0.7;
            cb += (ab - cb) * pop * 0.7;
            (cr * i, cg * i, cb * i, i.min(1.0))
        }
        Field::Cracks => {
            // slow CHURN: warp the Worley lookup so the fissure network wanders/breathes, not a rigid sheet
            let wt = t * l.speed;
            let wx = fbm(x * l.scale * 0.5, y * l.scale * 0.5 + wt * 0.3, 2) - 0.5;
            let wy = fbm(x * l.scale * 0.5 + 5.2, y * l.scale * 0.5 - wt * 0.3, 2) - 0.5;
            let (f1, f2, cell) = cellular(
                x * l.scale + wx * 1.4 + wt,
                y * l.scale + wy * 1.4 - wt * 0.8,
            );
            let edge = f2 - f1; // 0 at the crack, grows into the crust
                                // a COOLING GRADIENT across the fissure: a THIN white-hot seam (sharpened), a FAT orange lip
                                // (radiant heat falls off slowly), a far blood-red bloom — the temperature of cooling lava.
            let mut core = (1.0 - edge * 9.0).clamp(0.0, 1.0).powf(1.5); // knife-line seam
            let lip = (1.0 - edge * 4.0).clamp(0.0, 1.0).powf(1.1); // wider orange shoulder
            let bloom = (1.0 - edge * 1.8).clamp(0.0, 1.0).powf(2.2); // reaches further into the dark
                                                                      // ink: a thin glyph stroke can fall BETWEEN cells; spine heat guarantees a molten thread so a
                                                                      // drawn line always reads as a glowing crack, never dead basalt.
            core = core.max(p.heat * 0.4);
            // per-cell TEMPERATURE (some seams run hotter/whiter — breaks the "every crack identical"
            // tell) + per-cell throb RATE (seams breathe out of phase) + a slow DEEP breath on the far
            // bloom only (the furnace below swelling), never the steady white seams.
            let cell_temp = 0.85 + 0.30 * cell;
            let throb = 0.85 + 0.15 * (t * (0.4 + 0.5 * cell) + cell * 6.283).sin();
            let deep = 0.8 + 0.2 * (t * 0.11).sin();
            let temp =
                ((core * 1.15 + lip * 0.55 + bloom * 0.18 * deep) * throb * cell_temp * l.gain)
                    .min(1.3);
            let (mut cr, mut cg, mut cb) = ramp(l.ramp, temp.min(1.0), m);
            let (ar, ag, ab) = m.accent; // accent only in the deepest molten core
            let kk = (core * core) * 0.35;
            cr += (ar - cr) * kk;
            cg += (ag - cg) * kk;
            cb += (ab - cb) * kk;
            // crust RELIEF — bump-light a TWO-scale fbm height (coarse slabs + fine grain) so the cooled
            // rock reads as rough basalt; warm-fill it near seams (lit from below) so it's basalt-beside-
            // lava, not grey gravel.
            let hh = |ax: f32, ay: f32| {
                0.6 * fbm(ax * l.scale * 3.0, ay * l.scale * 3.0, 2)
                    + 0.4 * fbm(ax * l.scale * 1.2, ay * l.scale * 1.2, 2)
            };
            let h = hh(x, y);
            let hx = hh(x + 1.5, y) - h;
            let hy = hh(x, y + 1.5) - h;
            let light = (hx * 0.6 - hy * 0.6 + 0.45).clamp(0.0, 1.0);
            let crust = (0.03 + 0.20 * light) * (1.0 - core) * pres;
            let warm = 1.0 + lip * 0.6; // rock beside a fissure catches a warm rim-light from the heat
                                        // SHADOW COLLAR — a thin darkening just outside the seam reads as the crust lip OVER-hanging
                                        // the throat, so the white core sits BELOW the surface (the cracks feel deep, heat from below).
            let collar = ((1.0 - edge * 7.0).clamp(0.0, 1.0) - core).max(0.0) * 0.5;
            let cd = (1.0 - collar).max(0.0);
            let i = temp * pres;
            (
                cr * i + crust * warm * cd,
                cg * i + crust * 0.92 * cd,
                cb * i + crust * 0.85 * cd,
                (i + crust * 0.4).min(1.0),
            )
        }
        Field::Wisp => {
            // GENTLE BREEZE = soft DRIFTING PARTICLES (motes of light/dust carried on the air), over a
            // near-invisible body — the VFX way to depict gentle wind. Every TEXTURE-in-the-body approach
            // (fbm fill, sin stripes, LIC streaks) read as a mosaic/zebra; gentle wind is PARTICLES that
            // drift along a current + curl turbulence, low velocity, fading softly in and out (never pop).
            let gust = 0.6 + 0.4 * (t * 0.10).sin() * (t * 0.063 + 1.3).sin();
            // a faint smooth body — a translucent HINT of the stroke (no pattern), so a glyph still reads.
            let body = smoothstep(0.0, 0.6, pres);
            // the drift field: a gentle prevailing breeze + curl turbulence translates the particle field.
            let (cfx, cfy) = curl(
                x * l.scale * 0.5,
                y * l.scale * 0.5,
                t * l.speed * gust * 0.3,
            );
            let drift = t * l.speed * gust * 24.0; // FASTER than water — air moves quick
            let (vdx, vdy) = (0.5 + cfx, -0.2 + cfy); // local drift velocity
            let vl = (vdx * vdx + vdy * vdy).sqrt().max(1e-3);
            let (ux, uy) = (vdx / vl, vdy / vl); // drift axis — particles trail along it
            let dx = x + vdx * drift;
            let dy = y + vdy * drift;
            // glowing FAIRY-DUST motes on a sparse jittered grid: varied size, a soft glow + a bright
            // twinkling CORE, stretched into a soft trail along the drift — magic carried on the wind.
            // FEW, BIG, SOFT glowing motes — not a dense field of tiny hard dots (that reads as TV static).
            let cell = 34.0; // SPARSE grid — few, distinct motes (dense packing read as TV static)
            let gx = (dx / cell).floor();
            let gy = (dy / cell).floor();
            let mut glow_sum = 0.0f32; // soft accent-coloured halos
            let mut core_sum = 0.0f32; // gentle glowing centres
            for oy in -1..=1 {
                for ox in -1..=1 {
                    let (cx, cy) = (gx + ox as f32, gy + oy as f32);
                    let h = hash2(cx as i32, cy as i32);
                    if h < 0.55 {
                        continue; // ~45% of cells ⇒ FEW, distinct motes (more = TV static)
                    }
                    let h2 = hash2(cx as i32 + 31, cy as i32 - 17);
                    let h3 = hash2(cx as i32 - 9, cy as i32 + 5);
                    let rad = cell * (0.44 + 0.48 * h3); // BIG soft motes — bold orbs, not fine speckle
                    let pcx = (cx + 0.15 + 0.7 * h) * cell;
                    let pcy = (cy + 0.15 + 0.7 * h2) * cell;
                    let (ddx, ddy) = (dx - pcx, dy - pcy);
                    // strongly stretch the falloff ALONG the drift ⇒ a long STREAK (dust FLYING on the
                    // wind, motion-blurred) — light and linear, NOT a round wet droplet (that read watery).
                    let al = ddx * ux + ddy * uy;
                    let ac = -ddx * uy + ddy * ux;
                    let dd = ((al * al * 0.06 + ac * ac).sqrt() / rad).min(1.0); // longer streak along the wind
                    let s = 1.0 - dd;
                    let life = 0.5 + 0.5 * (t * l.speed * 1.3 + h * 6.283).sin(); // soft fade in/out (no pop)
                    let twinkle = 0.7 + 0.3 * (t * l.speed * 4.0 + h2 * 9.0).sin(); // gentle twinkle
                    glow_sum += s * s * life;
                    core_sum += s.powi(3) * life * twinkle; // a SOFT glowing centre (not a hard speckle)
                }
            }
            let gate = body.max(0.30); // gently confined to the stroke region
            let g_soft = (glow_sum * gate).min(1.4);
            let g_core = (core_sum * gate).min(1.1);
            let (ar, ag, ab) = m.accent;
            // a SMOOTH whisper of body sheen — a translucent hint of lit air so a thin stroke still reads,
            // with NO high-frequency noise (the curl-noise "current" I tried read as a TV-STATIC stream). The
            // flowing streaky MOTES carry ALL the character (distorted particles drifting on the wind); the
            // body is just a quiet, smooth glow — never a noisy stream, never a solid ribbon.
            let sheen = body * 0.09 * l.gain;
            // the few motes GLOW: a saturated accent halo + a near-white twinkling core, each stretched into a
            // long streak along the drift (a particle FLYING on the breeze) — bold and distinct, not speckle.
            let r = sheen * (0.5 + 0.5 * ar)
                + (0.35 + 0.65 * ar) * g_soft * l.gain * 1.7
                + (0.9 + 0.1 * ar) * g_core * 2.5;
            let g = sheen * (0.5 + 0.5 * ag)
                + (0.35 + 0.65 * ag) * g_soft * l.gain * 1.7
                + (0.9 + 0.1 * ag) * g_core * 2.5;
            let b = sheen * (0.55 + 0.45 * ab)
                + (0.35 + 0.65 * ab) * g_soft * l.gain * 1.7
                + (0.9 + 0.1 * ab) * g_core * 2.5;
            let alpha = (sheen + g_soft * 0.75 + g_core * 0.95).min(0.9);
            (r, g, b, alpha)
        }
        Field::Sparks => {
            let s = motes(
                x,
                y,
                t,
                9.0,
                l.speed * 22.0,
                l.warp * 6.0,
                l.scale.clamp(0.02, 0.6),
                1.7,
            ) * pres.max(0.2)
                * l.gain
                * 1.4;
            // embers COOL as they rise (gold→orange→red for fire's Blackbody ramp; bright→dim for air's
            // accent) instead of being flat white dust — keyed to height above the source.
            let up = (p.gy / (p.grad + 1e-3)).clamp(0.0, 1.0);
            let spark_t = (1.05 - up * 0.7).clamp(0.25, 1.05);
            let (cr, cg, cb) = ramp(l.ramp, spark_t, m);
            (cr * s, cg * s, cb * s, s)
        }
        Field::Filament => {
            let (corona, hot) = lightning(p, l, m);
            let gi = corona * l.gain;
            // a JAGGED crackling electric SPINE keyed to heat so a THIN stroke reads as a LIVE wire — the
            // wandering bolt misses a thin stroke otherwise. Brightness jitters fast ALONG the spine
            // (spatial crackle) with sparse bright spark NODES winking, so it reads as lightning, not a
            // smooth tube. Near-zero in the gallery blob (heat lives only at its centre), so the branching
            // bolt stays the hero there.
            let along = (p.x + p.y) * 0.7; // a coordinate running along the stroke
                                           // SHARP intermittent crackle (powf darkens the gaps) + bright spark NODES that flash and move
                                           // — bright arc-points with dark gaps read as electric at any width; a smooth bright line never
                                           // does. A dim floor keeps the channel continuous (legible).
            let crackle = fbm(along * 0.9 + p.t * 26.0, p.t * 4.0, 2).powf(2.3); // sharper, 2-octave
            let node = smoothstep(0.66, 0.93, fbm(along * 0.28 + p.t * 9.0, 11.3, 1)); // brighter spark nodes
            let buzz = (0.05 + 0.7 * crackle + 1.7 * node).min(1.9); // darker gaps + brighter sparks ⇒ jagged arc
            let spine = smoothstep(0.20, 0.64, p.heat) * buzz; // engages a touch earlier so thin ink crackles
            let chot = hot + spine * 0.95;
            // hue split: the CORONA wears the ramp (the accent), the CORE is a cold blue-white — the
            // high-energy arc signature (N2+ emission). The core leans BLUE (B>R) so it reads electric.
            let (cr, cg, cb) = ramp(l.ramp, corona.min(1.0), m);
            // lean HARD blue (low red) so even a BRIGHT spine reads electric blue-white instead of washing to
            // plain white — that wash was why a thin insight stroke looked like a dull white line, not a wire.
            (
                cr * gi + chot * 0.40,
                cg * gi + chot * 0.74,
                cb * gi + chot * 1.75,
                (gi + chot).min(1.0),
            )
        }
        Field::Prism => prism(p, l, m),
        Field::Fresnel => {
            // Schlick: near-zero face-on, whips to ~1 at the grazing edge (the wet, glassy snap)
            let edge = (p.grad * l.scale).min(1.0);
            let f = (0.04 + 0.96 * edge.powi(5)) * (0.4 + 0.6 * pres) * l.gain;
            let (cr, cg, cb) = ramp(l.ramp, 1.0, m);
            (cr * f, cg * f, cb * f, f * 0.8)
        }
    }
}

/// Composite a material's whole layer stack at one pixel — the field-stack engine. `n_layers == 0`
/// falls back to the tuned glass `shade` (the whiteboard ink path).
#[inline]
pub fn shade_surface(p: &Px, m: &Material) -> (f32, f32, f32, f32) {
    if m.n_layers == 0 {
        return shade(p.d, p.dr, p.db, p.heat, p.grad, p.facet_u, m);
    }
    let (mut r, mut g, mut b, mut lum) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for i in 0..m.n_layers as usize {
        let l = &m.layers[i];
        let (lr, lg, lb, li) = eval_layer(l, p, m);
        match l.mix {
            Mix::Add => {
                r += lr;
                g += lg;
                b += lb;
            }
            Mix::Screen => {
                let s = |u: f32, v: f32| 1.0 - (1.0 - u.min(1.0)) * (1.0 - v.min(1.0));
                r = s(r, lr);
                g = s(g, lg);
                b = s(b, lb);
            }
        }
        lum += li;
    }
    (r, g, b, lum.min(1.0))
}

// ── the recipes — each material is DATA: a stack of primitives + the knobs it surfaces ───────────

/// Build a material RECIPE for `surface` (the layer stack + its tunable knobs). Adding a material is
/// editing THIS function — data, not a new shader. Knobs point at layer params, so the UI renders
/// them generically and the user reshapes the substance live.
pub fn preset(surface: Surface) -> Material {
    let mut m = Material::neuron();
    m.surface = surface;
    let mut layers = [Layer::ZERO; MAX_LAYERS];
    let mut knobs = [Knob::ZERO; MAX_KNOBS];
    let (nl, nk): (usize, usize) = match surface {
        Surface::DirectedIntent => {
            // the CA needs the R/B taps to sample genuinely different points — dispersion IS that offset.
            // A middle value: visible red/cyan edge fringe on a glyph's body, without turning a thin
            // radial label fully cyan (a fixed-pixel CA offset is naturally a bigger fraction of thin ink
            // — that's physically honest, so thin labels carry a little more tint, by design).
            m.dispersion = 2.2;
            // Prism IS the body: a WHITE field + EMERGENT RGB chromatic aberration (a red fringe on one
            // flank, cyan on the other, only at edges) + a subtle weave wash + the white-hot core. NOT a
            // rainbow. A quiet accent rim sits beneath it.
            layers[0] = Layer {
                field: Field::Prism,
                scale: 0.5,
                gain: 1.0,
                ..Layer::ZERO
            };
            // a WHISPER of accent rim — kept low so the mint rim doesn't drown the red/cyan CA edge.
            layers[1] = Layer {
                field: Field::Fresnel,
                scale: 2.4,
                gain: 0.28,
                ramp: Ramp::AccentHot,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "aberration",
                layer: 0,
                kind: KnobKind::Scale,
                min: 0.0,
                max: 1.0,
            };
            knobs[1] = Knob {
                label: "glow",
                layer: 0,
                kind: KnobKind::Gain,
                min: 0.5,
                max: 1.6,
            };
            knobs[2] = Knob {
                label: "rim",
                layer: 1,
                kind: KnobKind::Gain,
                min: 0.0,
                max: 1.4,
            };
            (2, 3)
        }
        Surface::FluidThought => {
            layers[0] = Layer {
                field: Field::Body,
                ramp: Ramp::Water,
                gain: 0.9,
                ..Layer::ZERO
            };
            // the caustic SCREENS over the body — focused light passing THROUGH water onto a floor stays
            // in-gamut over the bright body instead of additively blowing the accent to white.
            layers[1] = Layer {
                field: Field::Caustic,
                scale: 0.04,
                speed: 0.20,
                detail: 3.0,
                warp: 2.5,
                gain: 1.4,
                ramp: Ramp::AccentHot,
                mix: Mix::Screen,
                ..Layer::ZERO
            };
            layers[2] = Layer {
                field: Field::Fresnel,
                scale: 2.4,
                gain: 1.0,
                ramp: Ramp::Water,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "caustic scale",
                layer: 1,
                kind: KnobKind::Scale,
                min: 0.01,
                max: 0.12,
            };
            knobs[1] = Knob {
                label: "flow speed",
                layer: 1,
                kind: KnobKind::Speed,
                min: 0.0,
                max: 0.8,
            };
            knobs[2] = Knob {
                label: "edge light",
                layer: 2,
                kind: KnobKind::Scale,
                min: 0.5,
                max: 5.0,
            };
            knobs[3] = Knob {
                label: "refraction",
                layer: 1,
                kind: KnobKind::Warp,
                min: 0.0,
                max: 6.0,
            };
            (3, 4)
        }
        Surface::MaterializedDesire => {
            layers[0] = Layer {
                field: Field::Turb,
                scale: 0.045,
                speed: 1.9,
                detail: 3.0,
                warp: 12.0,
                gain: 1.0,
                ramp: Ramp::Blackbody,
                ..Layer::ZERO
            };
            // distinct bright EMBERS flying around the flame — sparser (scale 0.09 ⇒ each one stands
            // alone) + much brighter (gain 3.2), drifting (warp), rising fast.
            layers[1] = Layer {
                field: Field::Sparks,
                scale: 0.09,
                speed: 1.5,
                warp: 0.6,
                gain: 3.2,
                ramp: Ramp::Blackbody,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "turbulence",
                layer: 0,
                kind: KnobKind::Scale,
                min: 0.01,
                max: 0.12,
            };
            knobs[1] = Knob {
                label: "rise speed",
                layer: 0,
                kind: KnobKind::Speed,
                min: 0.2,
                max: 4.0,
            };
            knobs[2] = Knob {
                label: "lick",
                layer: 0,
                kind: KnobKind::Warp,
                min: 0.0,
                max: 25.0,
            };
            // octaves of the flame's fbm — low = smooth gas tongues, high = intricate filigree edges.
            knobs[3] = Knob {
                label: "detail",
                layer: 0,
                kind: KnobKind::Detail,
                min: 1.0,
                max: 5.0,
            };
            (2, 4)
        }
        Surface::GentleBreeze => {
            // ONE layer: the Wisp now renders the drifting glowing fairy-dust motes ITSELF (a second
            // Sparks layer doubled the particles into TV static). drift = speed, swirl = warp.
            layers[0] = Layer {
                field: Field::Wisp,
                scale: 0.02,
                speed: 0.25,
                detail: 3.0,
                warp: 9.0,
                gain: 1.05,
                ramp: Ramp::Air,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "drift",
                layer: 0,
                kind: KnobKind::Speed,
                min: 0.05,
                max: 0.6,
            };
            knobs[1] = Knob {
                label: "swirl",
                layer: 0,
                kind: KnobKind::Warp,
                min: 0.0,
                max: 20.0,
            };
            // presence — how strongly the glowing motes read (sheer ambient ↔ legible).
            knobs[2] = Knob {
                label: "presence",
                layer: 0,
                kind: KnobKind::Gain,
                min: 0.3,
                max: 1.6,
            };
            (1, 3)
        }
        Surface::SuddenInsight => {
            // the body is a FAINT violet void the strikes illuminate — NOT a glow that out-areas the
            // bolt (gain 0.30 painted the whole metaball indigo and drowned the channel). 0.12 = a near-
            // black field. The Filament SCREENS over it (light through the dark) and runs hot (gain 1.7)
            // so the strike's white-hot core out-brightens the ambient it can't out-area.
            layers[0] = Layer {
                field: Field::Body,
                gain: 0.05,
                ramp: Ramp::Plasma,
                ..Layer::ZERO
            };
            // Filament wears AccentHot so the WEAVE TINT rides the corona + branch tips, while the bolt's
            // CORE stays cold blue-white (the arc signature, added in the Filament arm). Changing the
            // accent now recolours the lightning's halo without killing its electric blue heart.
            layers[1] = Layer {
                field: Field::Filament,
                scale: 0.05,
                speed: 1.0,
                detail: 3.0,
                warp: 0.6,
                gain: 1.7,
                ramp: Ramp::AccentHot,
                mix: Mix::Screen,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "strike rate",
                layer: 1,
                kind: KnobKind::Speed,
                min: 0.3,
                max: 3.0,
            };
            knobs[1] = Knob {
                label: "branching",
                layer: 1,
                kind: KnobKind::Warp,
                min: 0.0,
                max: 1.5,
            };
            knobs[2] = Knob {
                label: "arc glow",
                layer: 1,
                kind: KnobKind::Gain,
                min: 0.8,
                max: 2.6,
            };
            (2, 3)
        }
        Surface::MoltenResolve => {
            layers[0] = Layer {
                field: Field::Cracks,
                scale: 0.05,
                speed: 0.05,
                gain: 1.0,
                ramp: Ramp::Blackbody,
                ..Layer::ZERO
            };
            knobs[0] = Knob {
                label: "cell size",
                layer: 0,
                kind: KnobKind::Scale,
                min: 0.02,
                max: 0.12,
            };
            knobs[1] = Knob {
                label: "churn",
                layer: 0,
                kind: KnobKind::Speed,
                min: 0.0,
                max: 0.3,
            };
            knobs[2] = Knob {
                label: "glow",
                layer: 0,
                kind: KnobKind::Gain,
                min: 0.3,
                max: 2.0,
            };
            (1, 3)
        }
    };
    m.layers = layers;
    m.n_layers = nl as u8;
    m.knobs = knobs;
    m.n_knobs = nk as u8;
    m
}

// ── the gallery preview — a living swatch so each material can be SEEN (and picked) ───────────

/// Render an animated `w×h` RGBA swatch of a resolved material `m` over a near-black tile (opaque —
/// a contained preview, not a transparency). A field of three drifting metaballs gives every
/// material the SAME shape to shade, so the gallery is an honest side-by-side. The selected tile can
/// pass the LIVE material so its swatch reflects the user's knob edits in real time.
pub fn material_preview_rgba(m: &Material, w: usize, h: usize, t: f32) -> Vec<u8> {
    let (wf, hf, rf) = (w as f32, h as f32, w.min(h) as f32);
    let blobs = [
        (
            wf * 0.5 + (t * 0.7).cos() * wf * 0.16,
            hf * 0.5 + (t * 0.6).sin() * hf * 0.16,
            rf * 0.30,
        ),
        (
            wf * 0.34 + (t * 0.5 + 2.0).sin() * wf * 0.13,
            hf * 0.42 + (t * 0.8).cos() * hf * 0.13,
            rf * 0.20,
        ),
        (
            wf * 0.66 + (t * 0.9 + 1.0).cos() * wf * 0.12,
            hf * 0.60 + (t * 0.4).sin() * hf * 0.12,
            rf * 0.17,
        ),
    ];
    let field = |x: f32, y: f32| -> f32 {
        let mut s = 0.0;
        for (bx, by, br) in blobs {
            let dd = ((x - bx) * (x - bx) + (y - by) * (y - by)) / (br * br);
            s += (-dd * 1.6).exp();
        }
        s
    };
    let bg = 6.0 / 255.0; // the tile's near-void ground
    let mut out = vec![0u8; w * h * 4];
    for yy in 0..h {
        for xx in 0..w {
            let (x, y) = (xx as f32, yy as f32);
            let d = field(x, y).min(1.6);
            let gx = (field(x + 1.0, y) - field(x - 1.0, y)) * 0.5;
            let gy = (field(x, y + 1.0) - field(x, y - 1.0)) * 0.5;
            let grad = (gx * gx + gy * gy).sqrt();
            let heat = (d - 1.0).max(0.0) * 1.4;
            let (ux, uy, fu) = facet(gx, gy, m.facets);
            let off = m.dispersion;
            let (dr, db) = if grad > 0.004 {
                (
                    field(x + ux * off, y + uy * off).min(1.6),
                    field(x - ux * off, y - uy * off).min(1.6),
                )
            } else {
                (d, d)
            };
            let px = Px {
                d,
                dr,
                db,
                gx,
                gy,
                grad,
                facet_u: fu,
                heat,
                x,
                y,
                t,
            };
            let (mut r, mut g, mut b, lum) = shade_surface(&px, m);
            r = r.min(1.0).sqrt();
            g = g.min(1.0).sqrt();
            b = b.min(1.0).sqrt();
            let a = lum.clamp(0.0, 1.0);
            // composite over the near-black tile → an opaque swatch
            let i = (yy * w + xx) * 4;
            out[i] = ((r * a + bg * (1.0 - a)) * 255.0) as u8;
            out[i + 1] = ((g * a + bg * (1.0 - a)) * 255.0) as u8;
            out[i + 2] = ((b * a + bg * (1.0 - a)) * 255.0) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

// ── the curtain static — the user's material as a procedural "signal cut" ────────────────────────

/// THE CURTAIN STATIC — procedural "signal" grain shaded by the material `m`, for the privacy
/// curtain's cut-in / burst-out (see `neuron::curtain`). Returns a `w×h` **BGRA**, top-down, opaque
/// buffer (the curtain blits it chunky-scaled). Every lit cell is fed through the SAME [`shade_surface`]
/// a cast uses, so the static wears the user's LIVE material — accent, fire, spectrum, facets — and
/// ANY surface, now or future, just works with no special-casing.
///
/// `intensity` 0..1 is the signal strength: `1.0` = a full energised field; as it falls the grain
/// thins (a rising floor kills cells) and darkens until `0.0` = pure black. The curtain ramps it 1→0
/// to cut to black on raise, and 0→1 to burst back on reveal.
pub fn material_static_bgra(m: &Material, w: usize, h: usize, t: f32, intensity: f32) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    // opaque alpha up front (BI_RGB ignores it, but keep the buffer honest)
    for px in out.chunks_mut(4) {
        px[3] = 255;
    }
    let intensity = intensity.clamp(0.0, 1.0);
    if intensity <= 0.003 || w == 0 || h == 0 {
        return out; // pure black
    }

    // The grain re-seeds ~40×/s off WALL-CLOCK t, so it flickers like real static at any frame rate.
    let frame = (t * 40.0) as i32;
    let hash = |x: i32, y: i32, z: i32| -> f32 {
        let mut n = x
            .wrapping_mul(374_761_393)
            .wrapping_add(y.wrapping_mul(668_265_263))
            .wrapping_add(z.wrapping_mul(1_274_126_177));
        n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
        ((n ^ (n >> 16)) as u32) as f32 / u32::MAX as f32
    };
    // a cell's animated density: mostly per-pixel flicker, plus a slow drifting band so it reads as an
    // energised SIGNAL rather than flat snow.
    let dens = |x: i32, y: i32| -> f32 {
        let grain = hash(x, y, frame);
        let band = 0.5 + 0.5 * (y as f32 * 0.18 + t * 2.3).sin();
        (grain * 0.9 + band * 0.3) * 1.3
    };
    // as the signal fades, the floor a fresh cell must clear rises → the field dissolves sparse → black.
    let cut = (1.0 - intensity) * 1.15;

    for yy in 0..h as i32 {
        for xx in 0..w as i32 {
            let d0 = dens(xx, yy);
            if d0 <= cut {
                continue; // dead cell → stays black
            }
            let d = d0.min(1.6);
            // cheap gradient off neighbour grain → drives the facets / heat that give the material its
            // chromatic flecks and white-hot specks.
            let gx = (dens(xx + 1, yy) - dens(xx - 1, yy)) * 0.5;
            let gy = (dens(xx, yy + 1) - dens(xx, yy - 1)) * 0.5;
            let grad = (gx * gx + gy * gy).sqrt();
            let heat = (d - 1.0).max(0.0) * 1.4;
            let (_ux, _uy, fu) = facet(gx, gy, m.facets);
            let px = Px {
                d,
                dr: d,
                db: d,
                gx,
                gy,
                grad,
                facet_u: fu,
                heat,
                x: xx as f32,
                y: yy as f32,
                t,
            };
            let (r, g, b, lum) = shade_surface(&px, m);
            // gamma, composite the ink over black (alpha = lum), and fade by how strongly the cell
            // cleared the cut × the overall intensity → a clean dissolve.
            let k = (d0 - cut).min(1.0) * intensity * lum.clamp(0.0, 1.0);
            let i = (yy as usize * w + xx as usize) * 4;
            out[i] = (b.min(1.0).sqrt() * k * 255.0) as u8; // B
            out[i + 1] = (g.min(1.0).sqrt() * k * 255.0) as u8; // G
            out[i + 2] = (r.min(1.0).sqrt() * k * 255.0) as u8; // R
        }
    }
    out
}

// ── self-test proof sheet — render every material as actual STROKES (not gallery metaballs) ──────

/// THE PROOF SHEET — every material (rows) in every weave colour (columns), each as ONE flowing stroke
/// that TAPERS through all widths (thin radial-label → thick cast glyph) in a single mark, over
/// near-black, at time `t` (animated). The gallery only shows metaballs; this judges the materials as
/// the INK they really are — across size AND weave-colour, all on one screen, at a glance. Px is built
/// exactly the way `whiteboard.rs material_core` builds it from a stroke's signed-distance field.
pub fn weave_proof_sheet(t: f32) -> (usize, usize, Vec<u8>) {
    // the weave colours shown across the columns (white = the un-tinted material)
    let palette: [(f32, f32, f32); 6] = [
        (0.92, 0.92, 0.97), // white
        (0.29, 0.95, 0.69), // mint (the house default)
        (0.97, 0.27, 0.33), // red
        (0.32, 0.55, 1.00), // blue
        (1.00, 0.72, 0.20), // amber
        (0.74, 0.42, 1.00), // violet
    ];
    let (cell_w, cell_h) = (220usize, 112usize);
    let (ncol, nrow) = (palette.len(), Surface::ALL.len());
    let (w, h) = (cell_w * ncol, cell_h * nrow);
    let bg = 5.0 / 255.0;
    let mut out = vec![0u8; w * h * 4];
    for px in out.chunks_mut(4) {
        px[0] = (bg * 255.0) as u8;
        px[1] = (bg * 255.0) as u8;
        px[2] = (bg * 255.0) as u8;
        px[3] = 255;
    }
    let flow = t * 13.0; // the stroke flows over time (alive)
    for (ri, s) in Surface::ALL.into_iter().enumerate() {
        for (ci, &acc) in palette.iter().enumerate() {
            let m = preset(s).with_accent(acc);
            let (ox, oy) = (ci * cell_w, ri * cell_h);
            for ly in 0..cell_h {
                for lx in 0..cell_w {
                    let (fx, fy) = (lx as f32, ly as f32);
                    // the stroke TAPERS thin (left) → thick (right): every width in one mark.
                    let rad = 2.5 + (fx / cell_w as f32) * 17.0;
                    let ph = (fx + flow) * 0.055;
                    let spine = cell_h as f32 * 0.5 + 15.0 * ph.sin();
                    let slope = 15.0 * 0.055 * ph.cos();
                    let side = if fy >= spine { 1.0 } else { -1.0 };
                    let n = (slope * slope + 1.0).sqrt();
                    let d = (fy - spine).abs() / n;
                    if d > rad + 1.0 {
                        continue;
                    }
                    let (nx, ny) = (-slope / n * side, 1.0 / n * side);
                    let dens = |dd: f32| (1.0 - dd / rad).clamp(0.0, 1.0);
                    let here = dens(d);
                    let dn = (d / rad).clamp(0.0, 1.0);
                    let (ux, uy, facet_u) = facet(nx, ny, m.facets);
                    let gl = (nx * nx + ny * ny).sqrt().max(1e-4);
                    let proj = (ux * nx + uy * ny) / gl * m.dispersion;
                    let densb = dens(d - proj);
                    let densr = dens(d + proj);
                    let heat = ((0.55 - dn) / 0.55).max(0.0);
                    let grad = (here - densr).abs() + (densb - here).abs();
                    let pxs = Px {
                        d: here,
                        dr: densr,
                        db: densb,
                        gx: nx,
                        gy: ny,
                        grad,
                        facet_u,
                        heat,
                        x: (ox + lx) as f32,
                        y: (oy + ly) as f32,
                        t,
                    };
                    let (cr, cg, cb, lum) = shade_surface(&pxs, &m);
                    let cov = (rad + 0.5 - d).clamp(0.0, 1.0);
                    let a = (lum * cov).clamp(0.0, 1.0);
                    let idx = ((oy + ly) * w + (ox + lx)) * 4;
                    out[idx] = ((cr.min(1.0) * a + bg * (1.0 - a)) * 255.0) as u8;
                    out[idx + 1] = ((cg.min(1.0) * a + bg * (1.0 - a)) * 255.0) as u8;
                    out[idx + 2] = ((cb.min(1.0) * a + bg * (1.0 - a)) * 255.0) as u8;
                    out[idx + 3] = 255;
                }
            }
        }
    }
    (w, h, out)
}

/// Write the proof as an ANIMATED GIF (one screen: every material × every colour, the stroke tapering
/// through all sizes, looping ~2.4s) + a single PNG still. Rows = materials (DirectedIntent, Fluid
/// Thought, Materialized Desire, Gentle Breeze, Sudden Insight, Molten Resolve); columns = weave colour.
pub fn write_proof_sheets() {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};
    let nframes = 24;
    let mut frames = Vec::with_capacity(nframes);
    for i in 0..nframes {
        let t = i as f32 * 0.10; // ~10fps, ~2.4s loop
        let (w, h, buf) = weave_proof_sheet(t);
        if let Some(img) = RgbaImage::from_raw(w as u32, h as u32, buf) {
            frames.push(Frame::from_parts(
                img,
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            ));
        }
    }
    if let Ok(file) = std::fs::File::create("_weave_proof.gif") {
        let mut enc = GifEncoder::new_with_speed(file, 12);
        let _ = enc.set_repeat(Repeat::Infinite);
        let _ = enc.encode_frames(frames);
    }
    // a single still for quick inspection
    let (w, h, buf) = weave_proof_sheet(0.7);
    if let Some(img) = RgbaImage::from_raw(w as u32, h as u32, buf) {
        let _ = img.save("_weave_proof.png");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shoulder_is_monotonic_and_bounded() {
        let m = Material::neuron();
        let mut last = -1.0f32;
        for i in 0..100 {
            let s = shoulder(i as f32 * 0.02, m.shoulder);
            assert!(s >= last && (0.0..1.0).contains(&s), "s={s} at i={i}");
            last = s;
        }
    }

    #[test]
    fn ramp_is_white_intent_never_pigment() {
        // brightness must rise monotonically, and the BODY must never be warmer than white
        // (no honey: r <= g <= b ordering of the cool body preserved at the dim end).
        let m = Material::neuron();
        assert!(
            m.body.0 <= m.body.1 && m.body.1 <= m.body.2,
            "body must be cool, not amber"
        );
        let mut lastsum = -1.0;
        for i in 0..=32 {
            let (r, g, b, _) = shade(
                i as f32 / 20.0,
                i as f32 / 20.0,
                i as f32 / 20.0,
                0.0,
                0.0,
                0.5,
                &m,
            );
            let s = r + g + b;
            assert!(s >= lastsum - 1e-4, "ramp dipped at i={i}");
            lastsum = s;
        }
    }

    #[test]
    fn no_gradient_means_no_fire() {
        let m = Material::neuron();
        // equal taps + zero gradient ⇒ NO refraction: the colour must stay the BODY's hue (the
        // halo and surface are achromatic — they scale every channel by the body, injecting no
        // spectral flash). Assert the r:g:b RATIO matches the body ramp's, not an exact value.
        let (r, g, b, _) = shade(0.5, 0.5, 0.5, 0.0, 0.0, 0.0, &m);
        let t = (0.5f32 * 0.7 / 1.5).clamp(0.0, 1.0);
        let u = t * t * (3.0 - 2.0 * t);
        let want = (
            m.body.0 + (m.white.0 - m.body.0) * u,
            m.body.1 + (m.white.1 - m.body.1) * u,
            m.body.2 + (m.white.2 - m.body.2) * u,
        );
        // proportional: r/g == want.r/want.g (and b), to a small tolerance
        assert!((r * want.1 - g * want.0).abs() < 1e-4, "hue drifted in g");
        assert!((b * want.1 - g * want.2).abs() < 1e-4, "hue drifted in b");
    }

    #[test]
    fn edges_ignite_fire() {
        let m = Material::neuron();
        // disagreeing taps (an edge) must emit MORE total light than the flat interior tap
        let (r1, g1, b1, _) = shade(0.5, 0.5, 0.5, 0.0, 0.2, 0.3, &m);
        let (r2, g2, b2, _) = shade(0.5, 0.9, 0.15, 0.0, 0.2, 0.3, &m);
        assert!(r2 + g2 + b2 > r1 + g1 + b1, "an edge must flash");
    }

    #[test]
    fn facets_quantize_and_cover_the_circle() {
        let n = 12;
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..720 {
            let a = i as f32 * std::f32::consts::TAU / 720.0;
            let (ux, uy, fu) = facet(a.cos(), a.sin(), n);
            assert!(
                (ux * ux + uy * uy - 1.0).abs() < 1e-4,
                "facet dir must stay unit"
            );
            assert!((0.0..1.0).contains(&fu));
            seen.insert((fu * 1000.0) as i32);
        }
        // exactly n distinct planes — geometric, not smooth
        assert_eq!(
            seen.len(),
            n as usize,
            "expected {n} facets, saw {}",
            seen.len()
        );
    }

    #[test]
    fn shade_lum_bounded_and_text_survives() {
        let m = Material::neuron();
        for d in [0.0, 0.2, 0.8, 1.6] {
            for h in [0.0, 0.5, 1.4] {
                let (_, _, _, lum) = shade(d, d, d, h, 0.4, 0.2, &m);
                assert!((0.0..=1.0).contains(&lum));
            }
        }
        // a pure text mask (heat only, zero field) still emits bright white
        let (r, g, b, lum) = shade(0.0, 0.0, 0.0, 1.0, 0.0, 0.5, &m);
        assert!(r >= 0.9 && g >= 0.9 && b >= 0.9 && lum >= 0.9);
    }

    #[test]
    fn hue_roundtrip_phosphor() {
        // the precomputed accent hue must match the actual phosphor hue
        let m = Material::neuron();
        assert!((hue_of(m.accent) - m.accent_hue).abs() < 0.01);
    }

    #[test]
    fn weave_accent_overrides_fire_hue_and_rim_only() {
        // no override: the live material uses the stock phosphor accent (the accent path is opt-in).
        set_weave_surface(Surface::DirectedIntent);
        clear_weave_accent();
        let base = *material();
        assert!(
            (live_material().accent_hue - hue_u32(0x4A_F2B0)).abs() < 1e-6,
            "no override = stock phosphor"
        );
        // override with amber: the fire-centre hue + rim follow it — but the body, white-hot cores,
        // facets and dispersion are untouched (colour is the EDGE, never the substance).
        set_weave_accent(0xFF9F0A);
        let m = live_material();
        assert!(
            (m.accent_hue - hue_u32(0xFF9F0A)).abs() < 1e-6,
            "fire centre follows the weave accent"
        );
        assert!(m.accent.0 > m.accent.2, "amber rim reads warm (red > blue)");
        assert_eq!(m.body, base.body, "body stays cool-white — not pigment");
        assert_eq!(m.white, base.white, "cores stay white-hot");
        assert_eq!(m.facets, base.facets, "geometry untouched");
        clear_weave_accent();
        assert!(
            (live_material().accent_hue - hue_u32(0x4A_F2B0)).abs() < 1e-6,
            "cleared = stock phosphor again"
        );
    }

    #[test]
    fn intent_is_opt_in_and_communicates() {
        // OFF: with_intent(.., 0.0) MUST be byte-for-byte the plain material at an edge pixel —
        // the normal white→aberration look is sacred; tinting may never touch the default path.
        let m = Material::neuron();
        let off = m.with_intent(0.02, 0.0); // a red hue, but zero amount
        let a = shade(0.6, 0.9, 0.3, 0.2, 0.3, 0.4, &m);
        let b = shade(0.6, 0.9, 0.3, 0.2, 0.3, 0.4, &off);
        assert_eq!(a, b, "intent==0 must not alter the material");
        // ON: a warm-red intent must pull the fire+rim warm — more red than the phosphor default.
        let warn = m.with_intent(0.0, 1.0);
        let (r0, _, _, _) = shade(0.6, 0.95, 0.2, 0.0, 0.5, 0.5, &m);
        let (r1, _, _, _) = shade(0.6, 0.95, 0.2, 0.0, 0.5, 0.5, &warn);
        assert!(
            r1 > r0,
            "an intent tint must redden the edge ({r1} !> {r0})"
        );
    }
}
