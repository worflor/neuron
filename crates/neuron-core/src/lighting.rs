// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Unified lighting — one model across Razer's two Chroma eras without losing fidelity.
//!
//! The hardware speaks two dialects (we confirmed both live): the **legacy** keyboard class
//! `0x03` (effect-first, key-grid) and the **matrix** mouse class `0x0F` (topology-first,
//! per-LED). Neuron presents ONE model and preserves fidelity by three rules:
//!   1. native effect -> firmware effect (runs on-device, survives Synapse removal),
//!   2. custom frames are painted at the device's TRUE LED count (never downsampled),
//!   3. effects a device's firmware lacks are EMULATED here by streaming computed frames.
//!
//! Per the project law (semantics in code, wiring in data): the unified model + the frame
//! math live here; every device-specific opcode / dimension / effect-id lives in the registry
//! TOML `[lighting]` block. New device = new TOML, no recompile.

use crate::registry::CommandSpec;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// 24-bit colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }
    pub const BLACK: Rgb = Rgb::new(0, 0, 0);

    /// Scale brightness by a 0..=100 percentage.
    pub fn scale(self, pct: u8) -> Rgb {
        let p = pct.min(100) as u16;
        Rgb {
            r: (self.r as u16 * p / 100) as u8,
            g: (self.g as u16 * p / 100) as u8,
            b: (self.b as u16 * p / 100) as u8,
        }
    }

    /// Scale brightness by a 0.0..=1.0 factor (float precision — for smooth sinusoidal glow).
    pub fn scale_f(self, f: f32) -> Rgb {
        let f = f.clamp(0.0, 1.0);
        let m = |x: u8| (x as f32 * f).round() as u8;
        Rgb {
            r: m(self.r),
            g: m(self.g),
            b: m(self.b),
        }
    }

    /// Linear interpolate a..b by t in 0.0..=1.0.
    pub fn lerp(a: Rgb, b: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Rgb {
            r: m(a.r, b.r),
            g: m(a.g, b.g),
            b: m(a.b, b.b),
        }
    }

    /// HSV with h in 0..360, s,v in 0..=1 — the basis for spectrum/wave emulation.
    pub fn from_hsv(h: f32, s: f32, v: f32) -> Rgb {
        let h = h.rem_euclid(360.0);
        let c = v * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = v - c;
        let (r, g, b) = match (h / 60.0) as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let q = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Rgb {
            r: q(r),
            g: q(g),
            b: q(b),
        }
    }

    pub fn to_hex(self) -> String {
        format!("{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Parse "RRGGBB" (optional leading #).
    pub fn parse(s: &str) -> Option<Rgb> {
        let s = s.trim_start_matches('#');
        if s.len() != 6 {
            return None;
        }
        let n = u32::from_str_radix(s, 16).ok()?;
        Some(Rgb {
            r: (n >> 16) as u8,
            g: (n >> 8) as u8,
            b: n as u8,
        })
    }
}

// A colour serialises as a single bare hex STRING ("RRGGBB"), never a {r,g,b} sub-table. That keeps
// it hand-editable (matches the rest of the project's hex-colour configs) AND — load-bearing — keeps
// a `LayerDef` flat for TOML: a nested struct field would force a sub-table mid-record and TOML
// forbids a scalar after a table within one record, so persisting a layer stack would fail. Lossless.
impl serde::Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(&self.to_hex())
    }
}

impl<'de> serde::Deserialize<'de> for Rgb {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Rgb::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("invalid rgb hex '{s}'")))
    }
}

// ── TWO COEXISTING LIGHTING MODELS (by design, NOT leftover dead code) ─────────────────────
//
// Neuron drives lighting through two DISTINCT, intentionally-separate models — keep both:
//
//   (b) the FIRMWARE EFFECT model — this `Effect` enum + `native_effect_report` + `set_effect`.
//       A NAMED effect (off/static/breathing/…) set by its real effect-id byte and run ON-DEVICE by
//       the firmware (survives Synapse removal; persists onboard on matrix devices). Where a board
//       lacks an effect natively, `render_frame` host-EMULATES that SAME named effect as one computed
//       frame. It's a one-shot "set this effect" — used by the CLI `effect` cmd + runtime `apply_effect`.
//
//   (a) the PATTERN × SPECTRUM model — `Compositor::render` (in `pattern.rs`), STREAMED here by
//       `Lights::animate`. A live stack of programmable Pattern × Spectrum layers composited to custom
//       frames at the device's true rate — the GUI lighting studio + the CLI `animate` cmd drive this.
//
// (b) is a hardware capability addressed over the protocol; (a) is host-rendered. Different things for
// different jobs, so they COEXIST — neither supersedes the other, and neither is the other's leftover.

/// The superset of named effects across both eras (model (b) above). A device runs an effect natively
/// if its registry `effects` map names it; otherwise Neuron emulates it via a computed frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Off,
    Static,
    Breathing,
    Spectrum,
    Wave,
    Reactive,
    Starlight,
}

impl Effect {
    pub const ALL: [Effect; 7] = [
        Effect::Off,
        Effect::Static,
        Effect::Breathing,
        Effect::Spectrum,
        Effect::Wave,
        Effect::Reactive,
        Effect::Starlight,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Effect::Off => "off",
            Effect::Static => "static",
            Effect::Breathing => "breathing",
            Effect::Spectrum => "spectrum",
            Effect::Wave => "wave",
            Effect::Reactive => "reactive",
            Effect::Starlight => "starlight",
        }
    }

    pub fn from_name(s: &str) -> Option<Effect> {
        Effect::ALL
            .into_iter()
            .find(|e| e.name() == s.to_lowercase())
    }

    /// Does this effect take a base colour argument?
    pub fn uses_color(self) -> bool {
        matches!(self, Effect::Static | Effect::Breathing | Effect::Reactive)
    }

    /// Can Neuron synthesize this effect host-side (for emulation on devices lacking it)?
    /// Reactive needs keypress input from firmware; Starlight's emulation renders a flat static
    /// fill (see `render_frame`), which is NOT starlight — advertising it would be a silent lie,
    /// the same reason Reactive is excluded. Both come back if/when a real host animation ships.
    pub fn is_emulatable(self) -> bool {
        !matches!(self, Effect::Reactive | Effect::Starlight)
    }
}

/// Wire dialect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// class 0x03 — older keyboards (effect-first, row-addressed custom frames).
    Legacy,
    /// class 0x0F — newer devices (per-LED matrix).
    Matrix,
}

/// Per-device lighting definition (registry TOML `[lighting]`). Semantics are in code; these
/// opcodes/dims/effect-ids are the only device-specific bits.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct LightingDef {
    pub protocol: Protocol,
    pub rows: u8,
    pub cols: u8,
    /// 0x00 = volatile (NOSTORE), 0x01 = persist to onboard (VARSTORE).
    #[serde(default)]
    pub varstore: u8,
    /// LED id / zone the whole-device effect targets (matrix devices).
    #[serde(default)]
    pub led_id: u8,
    /// effect name -> firmware effect-id byte (the device's NATIVE effect set).
    #[serde(default)]
    pub effects: BTreeMap<String, u8>,
    /// Effect-id that DISPLAYS a written custom frame. PER-ERA (this is the easy one to get wrong):
    /// OpenRazer's *standard* matrix (legacy keyboards) uses 0x05 (CUSTOMFRAME) — the default — but the
    /// *extended* matrix (newer mice, class 0x0F) uses **0x08**, and on those devices 0x05 often means a
    /// real native effect (REACTIVE on the Naga), so leaving the default there sets reactive every frame
    /// and the stream flickers. Extended-matrix devices MUST set `custom_id = 0x08` in their TOML.
    #[serde(default = "default_custom_id")]
    pub custom_id: u8,
    /// command to set a whole-device effect.
    pub effect: CommandSpec,
    /// command to set one custom frame row (optional; enables custom/emulated effects).
    #[serde(default)]
    pub custom_frame: Option<CommandSpec>,
    /// command to set brightness (optional).
    #[serde(default)]
    pub brightness: Option<CommandSpec>,
}

fn default_custom_id() -> u8 {
    0x05
}

impl LightingDef {
    pub fn led_count(&self) -> usize {
        self.rows as usize * self.cols as usize
    }

    /// The command to DISPLAY a written custom frame (CUSTOMFRAME effect). Legacy keyboards
    /// take `[custom_id, varstore]` (effect-first, OpenRazer standard: `0x03/0x0A` args `05 00`,
    /// data_size 0x02); matrix takes the prefix then the id.
    pub fn custom_display_report(&self) -> Report {
        let (args, size) = match self.protocol {
            // OpenRazer standard_matrix_effect_custom_frame: [CUSTOMFRAME, varstore], data_size 0x02.
            Protocol::Legacy => (vec![self.custom_id, self.varstore], Some(0x02u8)),
            Protocol::Matrix => {
                let mut a = self.effect.args.clone();
                a.push(self.custom_id);
                (a, None)
            }
        };
        Report {
            class: self.effect.class,
            id: self.effect.id,
            args,
            tx: self.effect.transaction_id,
            size,
        }
    }

    /// Native (firmware) support for an effect?
    pub fn supports_native(&self, e: Effect) -> bool {
        self.effects.contains_key(e.name())
    }

    /// Effects this device can do at all (native OR Neuron-emulated via custom frames).
    pub fn available(&self) -> Vec<Effect> {
        Effect::ALL
            .into_iter()
            .filter(|&e| {
                self.supports_native(e) || (self.custom_frame.is_some() && e.is_emulatable())
            })
            .collect()
    }

    /// Build the report (class,id,args) to set a NATIVE effect. The CommandSpec.args is the
    /// fixed protocol prefix (e.g. matrix `[varstore, led_id]`); we append effect-id + colour.
    /// Returns None if the device lacks the effect natively.
    ///
    /// The two eras lay the effect command out differently (OpenRazer `razerchromacommon.c`):
    ///   * MATRIX (extended, class 0x0F): `[varstore, led, effect_id, 00, 00, 01, r, g, b]` with
    ///     the colour effects carrying a `00 00 01` one-colour preamble; `data_size = args.len()`.
    ///   * LEGACY (standard, class 0x03/0x0A): `effect_id` is `arguments[0]` directly (NO prefix),
    ///     followed by each effect's OWN sub-args, and OpenRazer sends a FIXED per-effect
    ///     `data_size` (static 0x04, off/spectrum 0x01, wave 0x02, reactive 0x05, breathing 0x08).
    ///     We reproduce those byte-for-byte so the board actually repaints instead of ACK-and-ignore.
    pub fn native_effect_report(&self, e: Effect, color: Option<Rgb>, persist: bool) -> Option<Report> {
        let id = *self.effects.get(e.name())?;
        match self.protocol {
            Protocol::Matrix => {
                let mut args = self.effect.args.clone(); // [varstore, led]
                args.push(id);
                if e.uses_color() {
                    let c = color.unwrap_or(Rgb::new(0, 255, 0));
                    // extended-matrix colour effects carry a `00 00 01` (one-colour) preamble,
                    // then RGB — confirmed live on the Naga (static red rendered).
                    args.extend_from_slice(&[0x00, 0x00, 0x01, c.r, c.g, c.b]);
                }
                // VARSTORE persists the effect to onboard memory (survives with no software) —
                // only the Matrix dialect has storage, and its varstore byte is args[0] (the very
                // prefix this builder laid down above). Owned HERE, inside the translation layer,
                // so no caller ever needs to know which byte means "store" (the one era leak the
                // stack audit found: `set_effect` used to reach in and poke args[0] itself).
                if persist {
                    if let Some(varstore) = args.first_mut() {
                        *varstore = 0x01;
                    }
                }
                Some(Report {
                    class: self.effect.class,
                    id: self.effect.id,
                    args,
                    tx: self.effect.transaction_id,
                    size: None, // matrix keeps data_size = args.len() (Naga byte-identical)
                })
            }
            Protocol::Legacy => {
                let c = color.unwrap_or(Rgb::new(0, 255, 0));
                // OpenRazer standard_matrix_effect_* — exact arg layout + FIXED data_size per effect.
                let (mut args, size): (Vec<u8>, u8) = match e {
                    Effect::Off => (vec![id], 0x01),
                    Effect::Spectrum => (vec![id], 0x01),
                    Effect::Static => (vec![id, c.r, c.g, c.b], 0x04),
                    // wave: [WAVE, direction] (1=left,2=right); default to 0x01.
                    Effect::Wave => (vec![id, 0x01], 0x02),
                    // reactive: [REACTIVE, speed(1..4), r, g, b]; default speed 0x01.
                    Effect::Reactive => (vec![id, 0x01, c.r, c.g, c.b], 0x05),
                    // breathing (single colour): [BREATHING, type=0x01, r, g, b], data_size 0x08.
                    Effect::Breathing => (vec![id, 0x01, c.r, c.g, c.b], 0x08),
                    // starlight (single colour): [STARLIGHT, type=0x01, speed, r, g, b, 0,0,0].
                    Effect::Starlight => (vec![id, 0x01, 0x01, c.r, c.g, c.b, 0x00, 0x00, 0x00], 0x01),
                };
                // Honour any TOML prefix (normally empty for legacy) by prepending it verbatim.
                if !self.effect.args.is_empty() {
                    let mut prefixed = self.effect.args.clone();
                    prefixed.append(&mut args);
                    args = prefixed;
                }
                Some(Report {
                    class: self.effect.class,
                    id: self.effect.id,
                    args,
                    tx: self.effect.transaction_id,
                    size: Some(size),
                })
            }
        }
    }

    /// Break a full-device frame (`led_count` colours, row-major) into per-row custom-frame
    /// reports. This is how both eras paint at true resolution + how emulation streams.
    pub fn frame_reports(&self, frame: &[Rgb]) -> Vec<Report> {
        (0..self.rows as usize)
            .filter_map(|row| self.row_report(frame, row))
            .collect()
    }

    /// Build the custom-frame report for ONE matrix row, or `None` when there's nothing to paint
    /// there — no `custom_frame` command, the row is past the frame's end, or its span is empty (a
    /// misconfigured `cols == 0` / 0-length frame). Splitting one row out (vs the whole frame) is
    /// what lets the animate loop DEDUP: it sends only the rows whose bytes actually changed since
    /// the last paint, so a static effect re-sends nothing after the first frame.
    pub fn row_report(&self, frame: &[Rgb], row: usize) -> Option<Report> {
        let cf = self.custom_frame.as_ref()?;
        // Stay within the device's real row count even if a generator handed us an over-long frame
        // (every built-in sizes to rows*cols, but be defensive — never emit a row the board lacks).
        if row >= self.rows as usize {
            return None;
        }
        let cols = self.cols as usize;
        let start = row * cols;
        if start >= frame.len() {
            return None;
        }
        let end = (start + cols).min(frame.len());
        let span = &frame[start..end];
        // An empty span (a misconfigured `cols == 0`, or a 0-length frame reaching here) would
        // underflow `span.len() - 1` — panic in debug, wrap to 255 in release. Skip it: there is
        // no row to paint, and a bogus stop_col of 255 would corrupt the frame either way.
        if span.is_empty() {
            return None;
        }
        // Custom-frame row layout (both eras): [<prefix>, row, start_col, stop_col, RGB..].
        // LEGACY (standard 0x03/0x0B) prefix = [0xFF frame-id] and OpenRazer sends a FIXED
        // data_size of 0x46 regardless of the painted span — short rows still ship 0x46 or
        // the firmware ACKs and ignores the frame. MATRIX (extended 0x0F/0x03) keeps the
        // length-derived data_size (Naga byte-identical).
        let mut args = cf.args.clone();
        args.push(row as u8);
        args.push(0);
        args.push((span.len() - 1) as u8);
        for c in span {
            args.extend_from_slice(&[c.r, c.g, c.b]);
        }
        let size = match self.protocol {
            Protocol::Legacy => Some(cf.size),
            Protocol::Matrix => None,
        };
        Some(Report {
            class: cf.class,
            id: cf.id,
            args,
            tx: cf.transaction_id,
            size,
        })
    }

    /// Build the brightness report (level 0..=255), if the device exposes the command.
    pub fn brightness_report(&self, pct: u8) -> Option<Report> {
        let b = self.brightness.as_ref()?;
        let level = (pct.min(100) as u16 * 255 / 100) as u8;
        let mut args = b.args.clone();
        args.push(level);
        Some(Report {
            class: b.class,
            id: b.id,
            args,
            tx: b.transaction_id,
            size: None, // [varstore, led, level] = 3 bytes => data_size 0x03 either way
        })
    }
}

/// A built command ready to send (or dry-run preview). Not yet a wire buffer — `Device`
/// turns it into a `protocol::Report` so writes route through the one gated path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub class: u8,
    pub id: u8,
    pub args: Vec<u8>,
    /// Optional transaction_id override carried from the source [`CommandSpec`]. `None` =
    /// use the device default (so non-overriding devices stay byte-identical). The Chroma V2's
    /// effect / custom-frame commands set this to 0x3F; brightness/getters leave it `None`.
    pub tx: Option<u8>,
    /// Optional `data_size` override (the razer_report `data_size` byte). `None` = derive it
    /// from `args.len()` (what every Matrix-era write did, so the Naga stays byte-identical).
    /// LEGACY (class 0x03) standard-matrix commands MUST send OpenRazer's FIXED data_size —
    /// e.g. custom-frame is always 0x46, breathing always 0x08 — regardless of the actual arg
    /// count, or the firmware ACKs the malformed packet and never repaints. Builders that need
    /// the fixed value set this; matrix builders leave it `None`.
    pub size: Option<u8>,
}

impl Report {
    /// Transparent hex preview (the "raw, transparent" motto) — what would hit the wire.
    pub fn preview(&self) -> String {
        let a: String = self.args.iter().map(|b| format!("{b:02X} ")).collect();
        // data_size is the explicit size if set (legacy fixed value) else the arg count.
        let ds = self.size.unwrap_or_else(|| self.args.len().min(80) as u8);
        format!(
            "class={:02X} id={:02X} data_size={:02X} args[{}]: {}",
            self.class,
            self.id,
            ds,
            self.args.len(),
            a.trim_end()
        )
    }
}

// --- Emulation engine: synthesize a frame for any device, any effect, host-side ----------

/// Render one animation frame of an emulatable effect into `rows*cols` colours (row-major).
/// `phase` is 0.0..1.0 animation progress; `base` the user colour. This is the unifier — it
/// gives the legacy keyboard effects its firmware never had, and lets the mic/gesture engines
/// drive lighting later (a frame is just `Vec<Rgb>`).
pub fn render_frame(effect: Effect, rows: u8, cols: u8, phase: f32, base: Rgb) -> Vec<Rgb> {
    let (rows, cols) = (rows as usize, cols as usize);
    let n = rows * cols;
    let mut f = vec![Rgb::BLACK; n];
    match effect {
        Effect::Off => {}
        Effect::Static => f.iter_mut().for_each(|c| *c = base),
        Effect::Breathing => {
            // smooth raised-cosine glow, float precision: brightness 0->1->0
            let b = 0.5 - 0.5 * (phase * std::f32::consts::TAU).cos();
            f.iter_mut().for_each(|c| *c = base.scale_f(b));
        }
        Effect::Spectrum => {
            // whole device cycles hue together (continuous; hue wraps in from_hsv)
            let c = Rgb::from_hsv(phase * 360.0, 1.0, 1.0);
            f.iter_mut().for_each(|x| *x = c);
        }
        Effect::Wave => {
            // a travelling rainbow: hue = column position + time, scrolling smoothly
            for r in 0..rows {
                for col in 0..cols {
                    let h = (phase + col as f32 / cols.max(1) as f32) * 360.0;
                    f[r * cols + col] = Rgb::from_hsv(h, 1.0, 1.0);
                }
            }
        }
        Effect::Starlight | Effect::Reactive => {
            // not deterministically emulatable here; caller should prefer native.
            f.iter_mut().for_each(|c| *c = base);
        }
    }
    f
}

// Full-size BlackWidow boards share this 6×22 ANSI matrix. Missing LEDs simply ignore their cell.
// Verified on hardware and cross-checked against OpenRazer's `KEY_MAPPING`.

/// Resolve a key name or alias to its standard `(row, column)` LED cell.
pub fn razer_key_cell(name: &str) -> Option<(u8, u8)> {
    Some(match name {
        // ── Row 0: macro M6, ESC, F-row, the print/scroll/pause cluster, logo ──
        "M6" => (0, 0),
        "ESC" => (0, 1),
        "F1" => (0, 3),
        "F2" => (0, 4),
        "F3" => (0, 5),
        "F4" => (0, 6),
        "F5" => (0, 7),
        "F6" => (0, 8),
        "F7" => (0, 9),
        "F8" => (0, 10),
        "F9" => (0, 11),
        "F10" => (0, 12),
        "F11" => (0, 13),
        "F12" => (0, 14),
        "PRINTSCREEN" => (0, 15),
        "SCROLLLOCK" => (0, 16),
        "PAUSE" => (0, 17),
        "LOGO" => (0, 20),
        // ── Row 1: macro M1, number row, backspace, ins/home/pgup, numpad top ──
        "M1" => (1, 0),
        "BACKTICK" | "GRAVE" | "`" => (1, 1),
        "1" => (1, 2),
        "2" => (1, 3),
        "3" => (1, 4),
        "4" => (1, 5),
        "5" => (1, 6),
        "6" => (1, 7),
        "7" => (1, 8),
        "8" => (1, 9),
        "9" => (1, 10),
        "0" => (1, 11),
        "DASH" | "MINUS" | "-" => (1, 12),
        "EQUALS" | "=" => (1, 13),
        "BACKSPACE" => (1, 14),
        "INSERT" => (1, 15),
        "HOME" => (1, 16),
        "PAGEUP" => (1, 17),
        "NUMLOCK" => (1, 18),
        "NUMDIVIDE" => (1, 19),
        "NUMMULTIPLY" => (1, 20),
        "NUMSUBTRACT" => (1, 21),
        // ── Row 2: macro M2, TAB, QWERTY row, brackets/backslash, del/end/pgdn, numpad 7-9 + ──
        "M2" => (2, 0),
        "TAB" => (2, 1),
        "Q" => (2, 2),
        "W" => (2, 3),
        "E" => (2, 4),
        "R" => (2, 5),
        "T" => (2, 6),
        "Y" => (2, 7),
        "U" => (2, 8),
        "I" => (2, 9),
        "O" => (2, 10),
        "P" => (2, 11),
        "LEFTBRACKET" | "[" => (2, 12),
        "RIGHTBRACKET" | "]" => (2, 13),
        "BACKSLASH" | "\\" => (2, 14),
        "DELETE" => (2, 15),
        "END" => (2, 16),
        "PAGEDOWN" => (2, 17),
        "NUM7" => (2, 18),
        "NUM8" => (2, 19),
        "NUM9" => (2, 20),
        "NUMADD" => (2, 21),
        // ── Row 3: macro M3, CAPSLOCK, home row, ;/' , ENTER, numpad 4-6 ──
        "M3" => (3, 0),
        "CAPSLOCK" => (3, 1),
        "A" => (3, 2),
        "S" => (3, 3),
        "D" => (3, 4),
        "F" => (3, 5),
        "G" => (3, 6),
        "H" => (3, 7),
        "J" => (3, 8),
        "K" => (3, 9),
        "L" => (3, 10),
        "SEMICOLON" | ";" => (3, 11),
        "QUOTE" | "'" => (3, 12),
        "ENTER" => (3, 14),
        "NUM4" => (3, 18),
        "NUM5" => (3, 19),
        "NUM6" => (3, 20),
        // ── Row 4: macro M4, LSHIFT, bottom row, ,/./ , RSHIFT, UP, numpad 1-3 + enter ──
        "M4" => (4, 0),
        "LSHIFT" => (4, 1),
        "Z" => (4, 3),
        "X" => (4, 4),
        "C" => (4, 5),
        "V" => (4, 6),
        "B" => (4, 7),
        "N" => (4, 8),
        "M" => (4, 9),
        "COMMA" | "," => (4, 10),
        "PERIOD" | "." => (4, 11),
        "SLASH" | "/" => (4, 12),
        "RSHIFT" => (4, 14),
        "UP" => (4, 16),
        "NUM1" => (4, 18),
        "NUM2" => (4, 19),
        "NUM3" => (4, 20),
        "NUMENTER" => (4, 21),
        // ── Row 5: macro M5, modifiers, space, fn/menu cluster, arrows, numpad 0 + decimal ──
        "M5" => (5, 0),
        "LCTRL" => (5, 1),
        "WIN" => (5, 2),
        "LALT" => (5, 3),
        "SPACE" => (5, 7),
        "RALT" => (5, 11),
        "FN" => (5, 12),
        "MENU" => (5, 13),
        "RCTRL" => (5, 14),
        "LEFT" => (5, 15),
        "DOWN" => (5, 16),
        "RIGHT" => (5, 17),
        "NUM0" => (5, 19),
        "NUMDECIMAL" => (5, 20),
        _ => return None,
    })
}

/// The Razer macro-key NAMES, indexed by held-state bit / report order: `MACRO_KEY_NAMES[i]` is the name
/// for the i-th macro key, resolved to a cell through the existing [`razer_key_cell`] ("M1"=(1,0) …
/// "M6"=(0,0)). The board's Driver-Mode `0x04` report numbers them sequentially (see [`macro_code_index`]).
/// A name with no cell on a given board simply lights nothing (graceful), so this stays forward-safe if a
/// board reports more or fewer macro keys.
pub const MACRO_KEY_NAMES: [&str; 6] = ["M1", "M2", "M3", "M4", "M5", "M6"];

/// The held-state index (0-based) for a Razer Driver-Mode macro report CODE: `0x20`→0 (M1), `0x21`→1
/// (M2) … `0x25`→5 (M6). Any other code — including FN (`0x01`) and released (`0x00`) — returns `None`.
/// The sequential `0x20..=0x25` numbering is the Razer PROTOCOL convention (matches OpenRazer's
/// `razer_raw_event`), not a per-board fact. Pairs with [`MACRO_KEY_NAMES`] to map a held code to its cell.
pub fn macro_code_index(code: u8) -> Option<usize> {
    // `then` (lazy) not `then_some` (eager): `code - 0x20` underflows u8 for codes below 0x20
    // (e.g. released 0x00, FN 0x01) and would panic in debug if evaluated unconditionally.
    (0x20..=0x25).contains(&code).then(|| (code - 0x20) as usize)
}

/// The full key map walked in reading order (row 0 → row 5, left → right), one CANONICAL name per
/// physical key (no aliases). This is the order `neuron lighting keytest` lights the board in, and the
/// list the duplicate-cell test iterates. Every name here resolves through [`razer_key_cell`].
pub fn razer_keyboard_keys() -> &'static [&'static str] {
    &[
        // row 0
        "M6", "ESC", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
        "PRINTSCREEN", "SCROLLLOCK", "PAUSE", "LOGO",
        // row 1
        "M1", "BACKTICK", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "MINUS", "EQUALS",
        "BACKSPACE", "INSERT", "HOME", "PAGEUP", "NUMLOCK", "NUMDIVIDE", "NUMMULTIPLY", "NUMSUBTRACT",
        // row 2
        "M2", "TAB", "Q", "W", "E", "R", "T", "Y", "U", "I", "O", "P", "LEFTBRACKET", "RIGHTBRACKET",
        "BACKSLASH", "DELETE", "END", "PAGEDOWN", "NUM7", "NUM8", "NUM9", "NUMADD",
        // row 3
        "M3", "CAPSLOCK", "A", "S", "D", "F", "G", "H", "J", "K", "L", "SEMICOLON", "QUOTE", "ENTER",
        "NUM4", "NUM5", "NUM6",
        // row 4
        "M4", "LSHIFT", "Z", "X", "C", "V", "B", "N", "M", "COMMA", "PERIOD", "SLASH", "RSHIFT", "UP",
        "NUM1", "NUM2", "NUM3", "NUMENTER",
        // row 5
        "M5", "LCTRL", "WIN", "LALT", "SPACE", "RALT", "FN", "MENU", "RCTRL", "LEFT", "DOWN", "RIGHT",
        "NUM0", "NUMDECIMAL",
    ]
}

// Static name tables for the formula-mapped VK ranges, so `vk_to_name` can return a `&'static str`
// (no per-call allocation) instead of formatting one. Each name resolves in `razer_key_cell`.
const VK_LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const VK_DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const VK_FKEYS: [&str; 12] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
];
const VK_NUMPAD: [&str; 10] = [
    "NUM0", "NUM1", "NUM2", "NUM3", "NUM4", "NUM5", "NUM6", "NUM7", "NUM8", "NUM9",
];

/// The map's canonical key NAME a Windows VIRTUAL-KEY corresponds to — `None` if the VK isn't a board
/// key. This is the UNIVERSAL OS keyboard standard: VK codes are an OS convention, identical across
/// every Razer keyboard, so this mapping needs no per-device data. It is the single VK→name resolution
/// [`vk_to_key_cell`] builds on (name→cell). The letter / digit / F-row / numpad-digit ranges map by
/// formula (via the static name tables above); everything else by an explicit table. Full
/// standard-keyboard coverage (letters, digits, F-row, numpad, the L/R-specific modifiers, arrows, the
/// nav cluster, OEM punctuation). Keys NOT on the board (mouse buttons, media keys) and the GENERIC
/// modifiers `0x10`/`0x11`/`0x12` return `None` — the generics fire ALONGSIDE the specific L/R variant,
/// so mapping them too would double-light. An accurate reactive surface never lies. Every produced name
/// is guaranteed to resolve in [`razer_key_cell`].
pub fn vk_to_name(vk: i32) -> Option<&'static str> {
    Some(match vk {
        0x41..=0x5A => VK_LETTERS[(vk - 0x41) as usize], // A..Z
        0x30..=0x39 => VK_DIGITS[(vk - 0x30) as usize],  // 0..9
        0x70..=0x7B => VK_FKEYS[(vk - 0x70) as usize],   // F1..F12
        0x60..=0x69 => VK_NUMPAD[(vk - 0x60) as usize],  // numpad 0..9
        0x1B => "ESC",
        0x09 => "TAB",
        0x14 => "CAPSLOCK",
        0x20 => "SPACE",
        0x0D => "ENTER",
        0x08 => "BACKSPACE",
        // SPECIFIC L/R modifiers only — the generic 0x10/0x11/0x12 are intentionally absent.
        0xA0 => "LSHIFT",
        0xA1 => "RSHIFT",
        0xA2 => "LCTRL",
        0xA3 => "RCTRL",
        0xA4 => "LALT",
        0xA5 => "RALT",
        0x5B => "WIN", // VK_LWIN
        0x5C => "WIN", // VK_RWIN → the board has one WIN key
        0x5D => "MENU",
        // arrows
        0x25 => "LEFT",
        0x26 => "UP",
        0x27 => "RIGHT",
        0x28 => "DOWN",
        // nav cluster
        0x2D => "INSERT",
        0x2E => "DELETE",
        0x24 => "HOME",
        0x23 => "END",
        0x21 => "PAGEUP",
        0x22 => "PAGEDOWN",
        // system keys
        0x2C => "PRINTSCREEN",
        0x91 => "SCROLLLOCK",
        0x13 => "PAUSE",
        0x90 => "NUMLOCK",
        // OEM punctuation
        0xC0 => "BACKTICK",
        0xBD => "MINUS",
        0xBB => "EQUALS",
        0xDB => "LEFTBRACKET",
        0xDD => "RIGHTBRACKET",
        0xDC => "BACKSLASH",
        0xBA => "SEMICOLON",
        0xDE => "QUOTE",
        0xBC => "COMMA",
        0xBE => "PERIOD",
        0xBF => "SLASH",
        // numpad operators
        0x6A => "NUMMULTIPLY",
        0x6B => "NUMADD",
        0x6D => "NUMSUBTRACT",
        0x6E => "NUMDECIMAL",
        0x6F => "NUMDIVIDE",
        _ => return None,
    })
}

/// The keyboard cell `(row, col)` a Windows VIRTUAL-KEY lands on in the standard Razer 6×22 ANSI matrix
/// — `None` if the VK isn't a board key. Bridges the live keyboard read (`GetAsyncKeyState` VKs) to the
/// key→cell table so vitals / keytest / Reactive land on a key's true position. Razer keyboards have ONE
/// LED per key, so this resolves to exactly ONE cell per pressed key; a key whose board has no LED at
/// that cell simply lights nothing.
pub fn vk_to_key_cell(vk: i32) -> Option<(u8, u8)> {
    razer_key_cell(vk_to_name(vk)?)
}

/// The number-row keys, left→right, that form the battery gauge track: `1 2 3 4 5 6 7 8 9 0 - =`
/// (12 keys). Backtick is deliberately excluded so the gauge is the clean digit span. Order IS the
/// fill order, so cell `i` is the `i`-th key the battery bar lights.
const BATTERY_TRACK_KEYS: [&str; 12] = [
    "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "=",
];

/// The function-row keys F1..F12, left→right — the DPI-stage pip strip. Stage `i` maps to `Fi+1`.
const DPI_PIP_KEYS: [&str; 12] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
];

// ── CROSS-DEVICE DATA SURFACE: paint one device's live vitals onto another's LED matrix ───
//
// The on-thesis flagship: neuron is ONE process speaking BOTH devices, so it can read the Naga's
// battery/charge/DPI-stage and PAINT them onto the BlackWidow's key matrix — a cross-device layer
// Synapse (which silos devices) and OpenRazer (no cross-device layer) structurally cannot do.
//
// This is the reusable core. It is a PURE function — `{battery, charging, stage} -> Vec<Rgb>` for a
// rows×cols matrix — so the CLI `lighting mirror` loop and (next phase) the GUI lighting page paint
// the IDENTICAL surface behind the same call. Device lighting is theme-agnostic: raw `Rgb`, never the
// GUI palette. The surface is painted ON-DEMAND when state changes, never streamed — vitals change
// on the seconds scale, so a paced stream would be pure waste. (Historically this was justified as
// "the legacy V2 is a slow board" — folklore since falsified by the wire probe; the on-demand
// design stands on its own merits.)

/// One snapshot of a source device's live vitals — what the data surface visualises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vitals {
    /// Battery charge, 0..=100.
    pub battery_pct: u8,
    /// Whether the source device is currently charging.
    pub charging: bool,
    /// Active DPI stage index, 0-based (the live stage the wheel is on).
    pub active_stage: u8,
    /// How many DPI stages are configured (the length of the cycle).
    pub stage_count: u8,
}

/// The battery-gauge fill colour for a 0..=100 level, tuned so the colour itself reads "how much is
/// left" at a glance with the DANGER zone unmistakable: **≤25% holds solid RED**, 25–55% ramps
/// RED→AMBER, >55% ramps AMBER→GREEN. The flat red plateau below 25% means a low pack (e.g. the
/// Naga's live 20%) is clearly red, never an ambiguous orange. Continuous and pure.
pub fn battery_color(pct: u8) -> Rgb {
    const GREEN: Rgb = Rgb::new(0, 255, 40);
    const AMBER: Rgb = Rgb::new(255, 140, 0);
    const RED: Rgb = Rgb::new(255, 0, 0);
    let p = pct.min(100) as f32 / 100.0;
    if p <= 0.25 {
        RED // flat red danger plateau — anything at/under a quarter pack reads as RED, not orange.
    } else if p <= 0.55 {
        Rgb::lerp(RED, AMBER, (p - 0.25) / 0.30) // 25→55%: red warms to amber.
    } else {
        Rgb::lerp(AMBER, GREEN, (p - 0.55) / 0.45) // 55→100%: amber rises to green.
    }
}

/// Cyan accent shared by the DPI pips and the charging crest — the "neuron is talking" highlight.
/// `pub(crate)` so the resolution-independent `vitals` PATTERN renderer (in `crate::pattern`) shares
/// the exact charging-crest hue rather than duplicating the magic number.
pub(crate) const VITALS_CYAN: Rgb = Rgb::new(0, 200, 255);

/// Render the cross-device VITALS surface into a `rows*cols` frame (row-major), the SAME shape every
/// other effect produces — so it paints through the existing `frame_reports` / custom-display path.
///
/// Unlike a raw row/col grid, this places vitals on MEANINGFUL physical keys via the standard Razer
/// key map ([`razer_key_cell`]) so the layout reads as DESIGNED on the actual board:
///
/// * **Battery gauge across the NUMBER ROW** (`1 2 3 4 5 6 7 8 9 0 - =`, 12 keys). The leftmost
///   `round(pct% × 12)` keys light in [`battery_color`] (RED low → AMBER mid → GREEN full); the
///   remaining number-row keys are **OFF** (dark), so the lit run alone shows the level — no blue
///   ghost track. Any non-zero battery lights ≥1 key (1% ≠ fully empty).
/// * **DPI stage on the F-KEYS** (F1…F`stage_count`). The active stage's F-key is bright cyan; the
///   other in-range F-keys are a dim cyan. (2 stages → F1 dim, F2 bright when stage 2 is active.)
/// * **Charging crest**: while charging, a bright CYAN crest sweeps left→right ALONG the lit battery
///   keys (phase-driven), so plugging in is unmistakable without hiding the level the run shows.
///
/// Every other key is OFF. The map is consulted per cell, so if a device's matrix doesn't carry a
/// given key the surface simply skips it — never out-of-bounds. `phase` is 0.0..1.0 (only the
/// charging crest uses it; pass 0.0 for a static paint). Pure — no I/O, fully unit-testable.
pub fn render_vitals(v: Vitals, rows: u8, cols: u8, phase: f32) -> Vec<Rgb> {
    let (rows_u, cols_u) = (rows as usize, cols as usize);
    let mut f = vec![Rgb::BLACK; rows_u * cols_u];
    if cols_u == 0 || rows_u == 0 {
        return f;
    }
    // Write `c` to the cell named `name` if the map knows it AND it fits this matrix. The clamp keeps
    // the surface safe on any dimensions while the real board uses the verified hardware cells.
    let mut put = |name: &str, c: Rgb| {
        if let Some((r, col)) = razer_key_cell(name) {
            let (r, col) = (r as usize, col as usize);
            if r < rows_u && col < cols_u {
                f[r * cols_u + col] = c;
            }
        }
    };

    // BATTERY GAUGE — the number row. round(pct% × N) keys lit; any non-zero battery lights ≥1.
    let n = BATTERY_TRACK_KEYS.len();
    let pct = v.battery_pct.min(100) as f32;
    let lit = if v.battery_pct == 0 {
        0
    } else {
        ((pct / 100.0 * n as f32).round() as usize).clamp(1, n)
    };
    let fill = battery_color(v.battery_pct);
    // The charging crest position travels across the lit run; ~2-key-wide bright cyan peak.
    let crest = phase * lit.max(1) as f32;
    for (i, &key) in BATTERY_TRACK_KEYS.iter().enumerate() {
        if i >= lit {
            continue; // unlit number-row keys stay OFF (dark), not a ghost track.
        }
        let color = if v.charging {
            let d = (i as f32 - crest).abs();
            let glow = (1.0 - d / 2.0).clamp(0.0, 1.0);
            Rgb::lerp(fill, VITALS_CYAN, 0.30 + 0.70 * glow)
        } else {
            fill
        };
        put(key, color);
    }

    // DPI STAGE — F-key pips. Active stage bright cyan, the other in-range F-keys a dim cyan.
    const PIP_DIM: Rgb = Rgb::new(0, 22, 45); // inactive stage — dim cyan/blue
    let stages = (v.stage_count as usize).min(DPI_PIP_KEYS.len());
    for (i, &key) in DPI_PIP_KEYS.iter().take(stages).enumerate() {
        let color = if i as u8 == v.active_stage {
            VITALS_CYAN
        } else {
            PIP_DIM
        };
        put(key, color);
    }
    f
}

// ── the live VITALS feed: the app pushes, the `vitals` lighting pattern pulls ────────────
//
// The cross-device surface above is a PURE function of a [`Vitals`] snapshot. To make it a
// first-class, compositable LAYER (the `vitals` pattern), the render thread needs the freshest
// snapshot without threading it through every call. This mirrors the pull providers
// (`audio_level` / `sys_stats` / `screen_ambient`) EXCEPT the direction: device vitals are PUSHED —
// the app already reads battery/charge/DPI-stage off its own device I/O — so there's no sampler
// thread here, just a published latest-snapshot the pattern reads each frame. Before the first
// publish it reads `None`, and the board idles honestly dark (a live-input pattern with no source,
// like the quiet audio meter).

/// The latest device-vitals snapshot the app has published, or `None` before the first publish.
fn vitals_slot() -> &'static Mutex<Option<Vitals>> {
    static V: OnceLock<Mutex<Option<Vitals>>> = OnceLock::new();
    V.get_or_init(|| Mutex::new(None))
}

/// Publish the freshest device vitals for the `vitals` lighting pattern to visualise. The app calls
/// this whenever it reads a source device's battery / charge / DPI-stage (the same reads that feed
/// the [`crate::vitals`] battery cards); the pattern reads the latest each frame via [`latest_vitals`].
/// Cheap (one lock, one copy); overwrites the prior snapshot, so only the newest is ever shown.
pub fn publish_vitals(v: Vitals) {
    // poison-tolerant (a panic while another thread held the lock must not wedge the vitals feed) — the
    // snapshot is plain Copy data, so recovering the guard can't observe a torn value.
    *vitals_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(v);
}

/// The most recently published device vitals, or `None` if nothing has been published yet. The
/// `vitals` pattern reads this per frame and renders dark on `None` (honest: a live readout with no
/// source). `pub(crate)` — only the pattern pulls it; the app is the writer via [`publish_vitals`].
pub(crate) fn latest_vitals() -> Option<Vitals> {
    *vitals_slot().lock().unwrap_or_else(|p| p.into_inner())
}

/// Test-only reset of the published snapshot back to `None`, so a test can exercise the no-source
/// (idle-dark) path deterministically regardless of what other tests have published into the global.
#[cfg(test)]
pub(crate) fn clear_vitals() {
    *vitals_slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
}

// ── the live BROADCAST feed: the app pushes, the `onair` lighting pattern pulls ──────────
//
// Same push shape as the vitals feed above, for a different truth: the broadcast state OBS
// announces over its websocket (streaming / recording), mirrored app-side and pushed here on
// every change. The `onair` pattern reads it per frame, so "you are live" becomes a LAYER the
// user paints — their region, their spectrum, their blend — instead of a colour forced on them.

/// One snapshot of the broadcast state — what the `onair` pattern visualises. Everything in it
/// was ANNOUNCED by OBS (resynced at connect), never assumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Broadcast {
    /// The websocket to OBS is authenticated — we actually KNOW the flags below. `false` means
    /// unknown, and the pattern renders dark rather than guessing (a tally that might be wrong
    /// is worse than none).
    pub connected: bool,
    /// The stream is live to the public.
    pub streaming: bool,
    /// A recording is running.
    pub recording: bool,
}

fn broadcast_slot() -> &'static Mutex<Option<Broadcast>> {
    static B: OnceLock<Mutex<Option<Broadcast>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(None))
}

/// Publish the freshest broadcast state for the `onair` pattern to visualise. The app's OBS
/// follower calls this on every announced change, and pushes a disconnected default when the
/// connection tears down — so the pattern can never render a stale "live".
pub fn publish_broadcast(b: Broadcast) {
    *broadcast_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(b);
}

/// The most recently published broadcast state (`None` before the first publish — renders dark,
/// honest). `pub(crate)` — only the pattern pulls it; the app is the writer.
pub(crate) fn latest_broadcast() -> Option<Broadcast> {
    *broadcast_slot().lock().unwrap_or_else(|p| p.into_inner())
}

/// Test-only reset, mirroring [`clear_vitals`].
#[cfg(test)]
pub(crate) fn clear_broadcast() {
    *broadcast_slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
}

// ── the live HOLD-STATE feed: the dispatch loop pushes, the `modeheld` pattern pulls ─────
//
// Edge-accurate input-mode truth: is a hold layer (HyperShift) engaged, is a sniper hold live?
// The app's dispatch loop owns those edges (it IS the thing tracking them), pushes here on every
// tick/edge, and pushes the default when the live loop stops — so the layer can never show a
// mode that isn't really held.

/// The held input modes the `modeheld` pattern visualises.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldState {
    /// ANY hold layer is engaged (HyperShift and friends — the header SHIFT pill's truth).
    pub layer: bool,
    /// A sniper hold is live (DPI dropped until release).
    pub sniper: bool,
}

fn hold_slot() -> &'static Mutex<Option<HoldState>> {
    static H: OnceLock<Mutex<Option<HoldState>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(None))
}

/// Publish the current hold state — the dispatch loop calls this on edges (and cheaply per
/// tick: one lock, one copy).
pub fn publish_hold(h: HoldState) {
    *hold_slot().lock().unwrap_or_else(|p| p.into_inner()) = Some(h);
}

/// The latest hold state (`None` before the live loop first publishes — renders dark).
pub(crate) fn latest_hold() -> Option<HoldState> {
    *hold_slot().lock().unwrap_or_else(|p| p.into_inner())
}

/// Test-only reset, mirroring [`clear_vitals`].
#[cfg(test)]
pub(crate) fn clear_hold() {
    *hold_slot().lock().unwrap_or_else(|p| p.into_inner()) = None;
}

// ── the SIGNAL channels: macros write, the `signal` pattern pulls ────────────────────────
//
// Four numbered 0..=1 channels ANY macro can drive (`neuron.signal(2, 0.8)` → the `signal` act
// verb → here). This is the emergence seam: neuron doesn't enumerate what a light can mean — CI
// status, a pomodoro, a boss timer, "someone joined voice" — the user's own scripts decide, and
// the engine renders it wherever they painted that channel's layer. Values persist until
// overwritten (a CI light STAYS red until a macro turns it green); process-state only, cleared
// by a relaunch, never persisted.

/// How many macro-drivable signal channels exist (0-indexed here; 1-indexed in the macro API
/// and the layer's knob).
pub const SIGNAL_CHANNELS: usize = 4;

fn signal_slots() -> &'static [std::sync::atomic::AtomicU32; SIGNAL_CHANNELS] {
    static S: OnceLock<[std::sync::atomic::AtomicU32; SIGNAL_CHANNELS]> = OnceLock::new();
    S.get_or_init(|| std::array::from_fn(|_| std::sync::atomic::AtomicU32::new(0f32.to_bits())))
}

/// Set a signal channel (0-indexed; out-of-range ignored; value clamped to 0..=1). Lock-free.
pub fn set_signal(channel: usize, value: f32) {
    if let Some(slot) = signal_slots().get(channel) {
        slot.store(
            value.clamp(0.0, 1.0).to_bits(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// Read a signal channel (0-indexed; out-of-range reads 0.0). Lock-free, cheap per frame.
pub fn signal(channel: usize) -> f32 {
    signal_slots()
        .get(channel)
        .map(|s| f32::from_bits(s.load(std::sync::atomic::Ordering::Relaxed)))
        .unwrap_or(0.0)
}

// ── backend facade: one lighting API over both Chroma eras ──────────────────────────────

/// A logical RGB surface. Callers paint this; the backend translates it to whichever wire
/// protocol the device speaks (legacy 0x03 or matrix 0x0F). To the caller there is NO diff.
#[derive(Clone, Debug)]
pub struct Canvas {
    pub rows: u8,
    pub cols: u8,
    pub px: Vec<Rgb>,
}

impl Canvas {
    pub fn new(rows: u8, cols: u8) -> Self {
        Canvas {
            rows,
            cols,
            px: vec![Rgb::BLACK; rows as usize * cols as usize],
        }
    }
    pub fn fill(&mut self, c: Rgb) {
        self.px.iter_mut().for_each(|p| *p = c);
    }
    pub fn set(&mut self, row: u8, col: u8, c: Rgb) {
        let i = row as usize * self.cols as usize + col as usize;
        if i < self.px.len() {
            self.px[i] = c;
        }
    }
}

/// The lighting backend: one handle over a device + its lighting def, hiding every protocol
/// detail behind `set_effect` / `paint` / `animate`. The translation layer that makes legacy
/// and matrix identical to any caller (the CLI today, the in-process Slint GUI — no IPC).
pub struct Lights<'a> {
    dev: &'a crate::device::Device,
    def: LightingDef,
    /// Driver-mode memo: `true` once [`ensure_control`](Lights::ensure_control) has verified/taken
    /// host control on this handle, so every render path can call it unconditionally and only the
    /// first call pays the round-trip.
    controlled: std::cell::Cell<bool>,
}

// ── animate-loop helpers: pure, testable row-dedup + deadline pacing ──────────────────────

/// The row indices whose colour bytes changed between the last-SENT frame `prev` and the current
/// frame `cur` — the dedup core, written into a caller-owned `out` so the hot loop reuses one Vec
/// instead of allocating per tick. `prev = None` (a fresh/(re)started stream, or a live fps change
/// that re-quantizes the phase) yields EVERY row, so the first paint is complete and the board is
/// never left partially stale; a length mismatch (a dimension change) likewise yields all rows. The
/// per-row comparison is a cheap `cols`-wide `Rgb` slice equality. Pure — unit-tested directly.
pub fn changed_rows_into(prev: Option<&[Rgb]>, cur: &[Rgb], cols: usize, out: &mut Vec<usize>) {
    out.clear();
    if cols == 0 {
        return; // a zero-width matrix has no rows to paint (mirrors `row_report`).
    }
    let nrows = cur.len().div_ceil(cols); // ceil-div: the rows this frame spans.
    let same = match prev {
        Some(p) => p.len() == cur.len(),
        None => false,
    };
    for row in 0..nrows {
        if !same {
            out.push(row); // no comparable previous frame → resend every row.
            continue;
        }
        let p = prev.unwrap();
        let start = row * cols;
        let end = (start + cols).min(cur.len());
        if cur[start..end] != p[start..end] {
            out.push(row);
        }
    }
}

/// Owned-Vec convenience wrapper over [`changed_rows_into`] (the hot loop uses the `_into` form to
/// reuse its buffer; tests and one-shot callers use this). Same semantics.
pub fn changed_rows(prev: Option<&[Rgb]>, cur: &[Rgb], cols: usize) -> Vec<usize> {
    let mut out = Vec::new();
    changed_rows_into(prev, cur, cols, &mut out);
    out
}

/// THE streaming rate ceiling — the ONE constant every layer of the lighting pipeline clamps fps
/// against: `Lights::animate`, the host bridge's `CompositorContent` quantization + pace atomic,
/// the host writer's tick, and the app's `host::set_lighting`. It exists so the render
/// quantization and the write cadence can never be clamped into DIFFERENT domains (the old split —
/// writer at 1..=60, everything else at 1..=30 — let a >30 pace burn kernel resolves on frames the
/// content layer never rendered). 30 is wire-verified on every shipped board (see
/// `device::tests::live_stream_strategy_probe`).
pub const MAX_STREAM_FPS: u32 = 30;

/// Deadline-pacing math (pure): given the frame's target `deadline`, the time `now` after its work,
/// and the frame interval `dt`, return `(updated_deadline, sleep)`. Ahead of schedule → sleep the
/// remainder up to `deadline`. Overran (`now >= deadline`) → no sleep (`Duration::ZERO`) AND the
/// deadline is nudged forward so accumulated lag can never exceed one `dt` — a slow frame is
/// absorbed without letting the cadence drift or burst-fire a catch-up storm after a stall. The
/// caller advances `deadline += dt` for the next frame before calling.
pub fn pace(
    deadline: std::time::Instant,
    now: std::time::Instant,
    dt: std::time::Duration,
) -> (std::time::Instant, std::time::Duration) {
    use std::time::Duration;
    if deadline > now {
        (deadline, deadline - now)
    } else {
        let lag = now - deadline;
        let clamped = if lag > dt { deadline + (lag - dt) } else { deadline };
        (clamped, Duration::ZERO)
    }
}

impl<'a> Lights<'a> {
    pub fn new(dev: &'a crate::device::Device, def: LightingDef) -> Self {
        Lights { dev, def, controlled: std::cell::Cell::new(false) }
    }
    pub fn def(&self) -> &LightingDef {
        &self.def
    }

    /// Take host control (the driver-mode switch Synapse hides behind). Idempotent AND memoized
    /// per handle: the first call round-trips the device, later calls are free — so every render
    /// path below calls it unconditionally. That guarantee matters: OUTSIDE driver mode the
    /// firmware ACKs lighting writes and silently ignores them, so a path that forgot to take
    /// control "succeeded" with a dark board (the silent-no-op trap the stack audit found on the
    /// ACK'd paint/effect paths, which used to rely on callers remembering).
    pub fn ensure_control(&self) -> anyhow::Result<()> {
        if self.controlled.get() {
            return Ok(());
        }
        if self.dev.run("device_mode").map(|m| m[0]).unwrap_or(0) != 0x03 {
            crate::writes::set_device_mode(self.dev, 0x03)?;
        }
        self.controlled.set(true);
        Ok(())
    }

    /// Set an effect: a native firmware effect if the device has it, else emulate it by painting
    /// a computed frame. This is the legacy<->matrix translation core — anything a device lacks
    /// natively becomes a custom frame, which BOTH protocols support. An effect that is neither
    /// native NOR faithfully emulatable (Reactive, Starlight — see [`Effect::is_emulatable`]) is
    /// an ERROR, not a silent flat-fill approximation: `set_effect` must agree with `available()`.
    pub fn set_effect(&self, e: Effect, color: Option<Rgb>, persist: bool) -> anyhow::Result<()> {
        self.ensure_control()?;
        if let Some(rep) = self.def.native_effect_report(e, color, persist) {
            self.dev.apply_lighting(&rep)?;
        } else if e.is_emulatable() {
            let frame = render_frame(
                e,
                self.def.rows,
                self.def.cols,
                0.0,
                color.unwrap_or(Rgb::new(0, 255, 0)),
            );
            self.paint_px(&frame)?;
        } else {
            anyhow::bail!(
                "effect '{}' is not available on this device (no native support, no faithful emulation)",
                e.name()
            );
        }
        Ok(())
    }

    /// Paint an arbitrary per-LED canvas — the universal path, protocol-translated.
    pub fn paint(&self, canvas: &Canvas) -> anyhow::Result<()> {
        self.paint_px(&canvas.px)
    }

    /// Paint a raw row-major `rows*cols` frame through the ON-DEMAND, ACK'd custom-frame path (each
    /// row + the display report is sent via `apply_lighting`, which waits for the device SUCCESS ack —
    /// NOT the fast fire-and-forget stream). This is what the cross-device data surface uses to paint
    /// `render_vitals` output reliably onto the slow legacy board.
    pub fn paint_frame(&self, frame: &[Rgb]) -> anyhow::Result<()> {
        self.paint_px(frame)
    }

    fn paint_px(&self, px: &[Rgb]) -> anyhow::Result<()> {
        // Driver mode is a render-path guarantee, not caller homework (memoized — free after the
        // first call on this handle). Without it the firmware ACKs every row and paints nothing.
        self.ensure_control()?;
        for r in self.def.frame_reports(px) {
            self.dev.apply_lighting(&r)?;
        }
        self.dev.apply_lighting(&self.def.custom_display_report())?;
        Ok(())
    }

    /// Stream a [`Compositor`](crate::pattern::Compositor) smoothly (fire-and-forget writes, consistent
    /// timing). The compositor (a stack of Pattern × Spectrum layers) decides the visuals; the backend
    /// handles control, translation, and streaming. `fps` is a CLOSURE read once PER FRAME (not a fixed
    /// value) so a running stream can be re-paced live — the GUI's fps control writes a shared atomic
    /// this closure reads. `secs` bounds the run by wall clock; `stop()` aborts early.
    pub fn animate(
        &self,
        comp: &mut crate::pattern::Compositor,
        fps: impl Fn() -> u32,
        secs: u64,
        mut stop: impl FnMut() -> bool,
    ) -> anyhow::Result<()> {
        use std::time::{Duration, Instant};
        self.ensure_control()?;
        let display = self.def.custom_display_report();
        let cols = self.def.cols as usize;
        // TUNABLE rate: `fps()` is read EVERY frame (not captured once) so the user can re-pace a
        // RUNNING stream without restarting it, clamped to a sane absolute range. The old "legacy
        // boards drop above ~6fps" belief was FOLKLORE: the live wire probe
        // (`device::tests::live_stream_strategy_probe`) measured ~1ms per fire-and-drain feature
        // report on the BlackWidow — a full 7-report frame in <10ms, 30fps sustained clean. The
        // historical 6fps ceiling came from the ACK'd path (10ms first-poll sleep × 7 reports),
        // not the silicon. The data-surface paints on-demand (not via this loop), so this only
        // paces continuous EFFECTS.
        //
        // ROW-LEVEL DEDUP: the device LATCHES and holds its buffer, so a row whose bytes are
        // unchanged needn't be re-sent — it keeps displaying. We cache the last frame we actually
        // SENT and each tick push only the rows that changed (and latch only then). A static effect
        // collapses to ~zero HID traffic after the first paint; a partial-change effect pushes just
        // the moving rows. `last_frame = None` until the first paint (and on any fps change, which
        // re-quantizes the phase) forces a FULL resend so the board is never left partially stale.
        // Both work buffers (`changed`, the cache) are reused across ticks — the only per-frame
        // Vec<Rgb> we can't avoid is the Compositor's `render()` output (it owns the composited frame).
        let mut last_frame: Option<Vec<Rgb>> = None;
        let mut changed: Vec<usize> = Vec::new();
        let mut last_fps: u32 = 0; // fps is clamped to 1..=30, so 0 forces a full first paint.
        let run_start = Instant::now();
        let mut next = run_start; // deadline-pacing anchor (separate from the wall-clock phase).
        while !stop() && run_start.elapsed().as_secs_f64() < secs as f64 {
            let fps = fps().clamp(1, MAX_STREAM_FPS);
            let dt = Duration::from_millis(1000 / fps as u64);
            // Quantize the SHARED render clock to 1/fps steps via the ONE helper the GUI preview also
            // calls (`quantized_t` off the process-global `render_epoch`), so the on-screen mirror steps
            // in the identical discrete frames the device does (chunky at 6fps, smooth at 30) — same
            // epoch + same formula ⇒ the preview provably matches the board. Wall-clock (not
            // frame_index/fps) keeps the phase CONTINUOUS when fps is re-tuned mid-stream, and the shared
            // epoch (not this stream's start) means a restart can't jump it.
            // `render_elapsed()` wraps at 4096s in the duration domain, so the f32 phase stays frame-precise
            // at any uptime (a raw `.as_secs_f32()` decays to a stutter after ~a day).
            let elapsed = crate::pattern::quantized_t(crate::pattern::render_elapsed(), fps);
            let frame = comp.render(self.def.rows, self.def.cols, elapsed);

            // Which rows differ from what's already on the board? A live fps change re-quantizes the
            // phase, so drop the cache (prev = None) → resend everything that tick.
            let prev = if fps == last_fps { last_frame.as_deref() } else { None };
            changed_rows_into(prev, &frame, cols, &mut changed);
            last_fps = fps;

            // Push only the changed rows; LATCH (commit the frame) only if we actually wrote ≥1 row.
            let mut sent_any = false;
            for &row in &changed {
                if let Some(rep) = self.def.row_report(&frame, row) {
                    self.dev.send_lighting_fast(&rep);
                    sent_any = true;
                }
            }
            if sent_any {
                self.dev.send_lighting_fast(&display);
            }

            // Refresh the dedup cache IN PLACE (reuse the buffer; don't realloc each tick).
            let buf = last_frame.get_or_insert_with(|| Vec::with_capacity(frame.len()));
            buf.clear();
            buf.extend_from_slice(&frame);

            // DEADLINE pacing: anchor the next wake to `next += dt` rather than `sleep(dt - work)`,
            // so a slow frame is compensated and the cadence can't drift; an overrun skips the sleep
            // and `pace` clamps the lag to one frame so there's no catch-up burst after a stall.
            next += dt;
            let (nd, nap) = pace(next, Instant::now(), dt);
            next = nd;
            if !nap.is_zero() {
                std::thread::sleep(nap);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::CommandSpec;

    fn matrix_def() -> LightingDef {
        let mut effects = BTreeMap::new();
        effects.insert("off".into(), 0x00);
        effects.insert("static".into(), 0x01);
        effects.insert("spectrum".into(), 0x03);
        LightingDef {
            protocol: Protocol::Matrix,
            rows: 1,
            cols: 2,
            varstore: 0,
            led_id: 0,
            custom_id: 0x05,
            effects,
            effect: CommandSpec {
                class: 0x0F,
                id: 0x02,
                size: 0x0C,
                args: vec![0x00, 0x00],
                transaction_id: None,
            },
            custom_frame: Some(CommandSpec {
                class: 0x0F,
                id: 0x03,
                size: 0x0C,
                args: vec![0x00],
                transaction_id: None,
            }),
            brightness: Some(CommandSpec {
                class: 0x0F,
                id: 0x04,
                size: 0x03,
                args: vec![0x00, 0x00],
                transaction_id: None,
            }),
        }
    }

    #[test]
    fn rgb_parse_hsv_scale() {
        assert_eq!(Rgb::parse("#FF8800"), Some(Rgb::new(0xFF, 0x88, 0x00)));
        assert_eq!(Rgb::parse("bad"), None);
        assert_eq!(Rgb::new(200, 100, 50).scale(50), Rgb::new(100, 50, 25));
        // hue 0 = red, 120 = green, 240 = blue
        assert_eq!(Rgb::from_hsv(0.0, 1.0, 1.0), Rgb::new(255, 0, 0));
        assert_eq!(Rgb::from_hsv(120.0, 1.0, 1.0), Rgb::new(0, 255, 0));
    }

    /// A standard-matrix (class 0x03) keyboard like the BlackWidow Chroma V2: effect-first,
    /// no varstore/led prefix, FIXED per-command data_size. Mirrors the V2 registry TOML.
    fn legacy_def() -> LightingDef {
        let mut effects = BTreeMap::new();
        effects.insert("off".into(), 0x00);
        effects.insert("wave".into(), 0x01);
        effects.insert("reactive".into(), 0x02);
        effects.insert("breathing".into(), 0x03);
        effects.insert("spectrum".into(), 0x04);
        effects.insert("static".into(), 0x06);
        LightingDef {
            protocol: Protocol::Legacy,
            rows: 6,
            cols: 22,
            varstore: 0x00,
            led_id: 0x00,
            custom_id: 0x05,
            effects,
            effect: CommandSpec {
                class: 0x03,
                id: 0x0A,
                size: 0x08,
                args: vec![],
                transaction_id: Some(0x3F),
            },
            custom_frame: Some(CommandSpec {
                class: 0x03,
                id: 0x0B,
                size: 0x46,
                args: vec![0xFF],
                transaction_id: Some(0x3F),
            }),
            brightness: Some(CommandSpec {
                class: 0x03,
                id: 0x03,
                size: 0x03,
                args: vec![0x01, 0x05],
                transaction_id: None,
            }),
        }
    }

    #[test]
    fn legacy_effects_match_openrazer_standard_matrix() {
        let d = legacy_def();
        // STATIC red: 0x03/0x0A, args [STATIC=0x06, FF,00,00], data_size 0x04, tx 0x3F.
        let r = d
            .native_effect_report(Effect::Static, Some(Rgb::new(0xFF, 0, 0)), false)
            .unwrap();
        assert_eq!((r.class, r.id), (0x03, 0x0A));
        assert_eq!(r.args, vec![0x06, 0xFF, 0x00, 0x00]);
        assert_eq!(r.size, Some(0x04));
        assert_eq!(r.tx, Some(0x3F));
        // OFF / SPECTRUM: single effect-id byte, data_size 0x01.
        let off = d.native_effect_report(Effect::Off, None, false).unwrap();
        assert_eq!((off.args.clone(), off.size), (vec![0x00], Some(0x01)));
        let spec = d.native_effect_report(Effect::Spectrum, None, false).unwrap();
        assert_eq!((spec.args.clone(), spec.size), (vec![0x04], Some(0x01)));
        // WAVE: [WAVE, dir=1], data_size 0x02.
        let w = d.native_effect_report(Effect::Wave, None, false).unwrap();
        assert_eq!((w.args.clone(), w.size), (vec![0x01, 0x01], Some(0x02)));
        // REACTIVE: [REACTIVE, speed=1, r,g,b], data_size 0x05.
        let re = d
            .native_effect_report(Effect::Reactive, Some(Rgb::new(1, 2, 3)), false)
            .unwrap();
        assert_eq!((re.args.clone(), re.size), (vec![0x02, 0x01, 1, 2, 3], Some(0x05)));
        // BREATHING single: [BREATHING, type=1, r,g,b], data_size 0x08.
        let br = d
            .native_effect_report(Effect::Breathing, Some(Rgb::new(9, 8, 7)), false)
            .unwrap();
        assert_eq!((br.args.clone(), br.size), (vec![0x03, 0x01, 9, 8, 7], Some(0x08)));
    }

    #[test]
    fn legacy_custom_frame_and_display_match_openrazer() {
        let d = legacy_def();
        // custom-frame row: [0xFF frame-id, row, start, stop, RGB...], FIXED data_size 0x46.
        let frame = vec![Rgb::new(1, 2, 3); 22 * 6];
        let reps = d.frame_reports(&frame);
        assert_eq!(reps.len(), 6);
        assert_eq!((reps[0].class, reps[0].id), (0x03, 0x0B));
        assert_eq!(reps[0].args[0..4], [0xFF, 0, 0, 21]); // frame-id, row0, start0, stop21
        assert_eq!(reps[0].args.len(), 4 + 22 * 3);
        assert_eq!(reps[0].size, Some(0x46)); // fixed regardless of span
        assert_eq!(reps[0].tx, Some(0x3F));
        // A SHORT row must STILL ship data_size 0x46 (the bug: shipping args.len() no-ops it).
        let short = vec![Rgb::new(1, 2, 3); 2];
        let sreps = d.frame_reports(&short);
        assert_eq!(sreps[0].size, Some(0x46));
        // display the uploaded frame: [CUSTOMFRAME=0x05, varstore=0x00], data_size 0x02.
        let disp = d.custom_display_report();
        assert_eq!((disp.class, disp.id), (0x03, 0x0A));
        assert_eq!((disp.args.clone(), disp.size), (vec![0x05, 0x00], Some(0x02)));
        assert_eq!(disp.tx, Some(0x3F));
    }

    #[test]
    fn matrix_reports_leave_size_unset_so_data_size_stays_arglen() {
        // Naga (extended) path: size None => apply_lighting derives data_size from args.len(),
        // byte-identical to before. Guards rule #3 (do not change the Naga).
        let d = matrix_def();
        let r = d
            .native_effect_report(Effect::Static, Some(Rgb::new(1, 2, 3)), false)
            .unwrap();
        assert_eq!(r.size, None);
        let reps = d.frame_reports(&[Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)]);
        assert_eq!(reps[0].size, None);
        assert_eq!(d.custom_display_report().size, None);
    }

    #[test]
    fn native_effect_args_are_prefix_plus_id_plus_color() {
        let d = matrix_def();
        // spectrum (no colour): args = prefix [00,00] + id 03
        let r = d.native_effect_report(Effect::Spectrum, None, false).unwrap();
        assert_eq!(r.class, 0x0F);
        assert_eq!(r.id, 0x02);
        assert_eq!(r.args, vec![0x00, 0x00, 0x03]);
        // static (colour) on matrix: prefix + id 01 + [00 00 01] preamble + RGB
        let r = d
            .native_effect_report(Effect::Static, Some(Rgb::new(10, 20, 30)), false)
            .unwrap();
        assert_eq!(r.args, vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 10, 20, 30]);
        // breathing not in this device's native set
        assert!(d.native_effect_report(Effect::Breathing, None, false).is_none());
    }

    #[test]
    fn available_includes_emulated_when_custom_frame_present() {
        let d = matrix_def();
        // breathing/wave aren't native but custom_frame exists => emulatable => available
        let av = d.available();
        assert!(av.contains(&Effect::Breathing));
        assert!(av.contains(&Effect::Wave));
        assert!(!d.supports_native(Effect::Wave));
    }

    #[test]
    fn frame_reports_paint_true_resolution() {
        let d = matrix_def(); // 1x2 => 2 LEDs, 1 row
        let frame = vec![Rgb::new(1, 2, 3), Rgb::new(4, 5, 6)];
        let reps = d.frame_reports(&frame);
        assert_eq!(reps.len(), 1); // one row
                                   // prefix [00] + row 0 + start 0 + end 1 + two RGB triples
        assert_eq!(reps[0].args, vec![0x00, 0, 0, 1, 1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn frame_reports_zero_cols_or_empty_frame_does_not_panic() {
        // A misconfigured `cols == 0` made `span.len() - 1` underflow (panic in debug / 255 in
        // release). It must now be skipped cleanly: an empty span paints no row.
        let mut d = matrix_def();
        d.cols = 0;
        let frame = vec![Rgb::new(1, 2, 3); 6]; // some pixels, but zero-width rows
        let reps = d.frame_reports(&frame); // must NOT panic
        assert!(reps.is_empty(), "zero-col device emits no rows, never an underflowed stop_col");
        // an empty frame on a normal device is also safe (every row's span is empty → skipped).
        let d2 = matrix_def();
        assert!(d2.frame_reports(&[]).is_empty());
    }

    #[test]
    fn frame_reports_equals_per_row_reports() {
        // `frame_reports` is now just `row_report` over every row — the split must be lossless so the
        // dedup path (which calls `row_report` directly) ships byte-identical rows to the old loop.
        let d = legacy_def();
        let frame: Vec<Rgb> = (0..6 * 22).map(|i| Rgb::new(i as u8, 0, 0)).collect();
        let whole = d.frame_reports(&frame);
        let per_row: Vec<Report> = (0..6).filter_map(|r| d.row_report(&frame, r)).collect();
        assert_eq!(whole, per_row);
        // a row past the matrix (and a no-custom-frame device) yield nothing.
        assert!(d.row_report(&frame, 6).is_none());
    }

    // ── ROW-LEVEL FRAME DEDUP: `changed_rows` ────────────────────────────────────────────

    #[test]
    fn changed_rows_first_frame_sends_all() {
        // prev = None (a fresh/(re)started stream) → every row the frame spans is "changed".
        let cur = vec![Rgb::BLACK; 3 * 4]; // 3 rows × 4 cols
        assert_eq!(changed_rows(None, &cur, 4), vec![0, 1, 2]);
    }

    #[test]
    fn changed_rows_identical_frames_send_none() {
        let a = vec![Rgb::new(7, 7, 7); 3 * 4];
        let b = a.clone();
        assert!(changed_rows(Some(&a), &b, 4).is_empty(), "no change → no rows");
    }

    #[test]
    fn changed_rows_one_cell_change_sends_exactly_that_row() {
        let prev = vec![Rgb::BLACK; 3 * 4];
        let mut cur = prev.clone();
        cur[4 + 2] = Rgb::new(1, 2, 3); // a single cell in row 1
        assert_eq!(changed_rows(Some(&prev), &cur, 4), vec![1]);
    }

    #[test]
    fn changed_rows_multi_row_change() {
        let prev = vec![Rgb::BLACK; 4 * 3];
        let mut cur = prev.clone();
        cur[0] = Rgb::new(9, 0, 0); // row 0
        cur[3 * 3 + 1] = Rgb::new(0, 9, 0); // row 3
        assert_eq!(changed_rows(Some(&prev), &cur, 3), vec![0, 3]);
    }

    #[test]
    fn changed_rows_length_mismatch_resends_all() {
        // a dimension change mid-stream (prev.len() != cur.len()) can't be diffed → resend every row.
        let prev = vec![Rgb::BLACK; 2 * 4];
        let cur = vec![Rgb::BLACK; 3 * 4];
        assert_eq!(changed_rows(Some(&prev), &cur, 4), vec![0, 1, 2]);
    }

    #[test]
    fn changed_rows_zero_cols_is_empty() {
        let cur = vec![Rgb::BLACK; 6];
        assert!(changed_rows(None, &cur, 0).is_empty());
    }

    #[test]
    fn changed_rows_into_reuses_its_buffer() {
        // the hot loop reuses one Vec across ticks — `_into` must `clear()` first so stale indices
        // from a busy frame never leak into a later quiet one.
        let mut out = Vec::new();
        let prev = vec![Rgb::BLACK; 2 * 2];
        let mut cur = prev.clone();
        cur[0] = Rgb::new(1, 1, 1); // row 0 changes
        changed_rows_into(Some(&prev), &cur, 2, &mut out);
        assert_eq!(out, vec![0]);
        // now an identical frame must empty the buffer, not append to it.
        let same = cur.clone();
        changed_rows_into(Some(&cur), &same, 2, &mut out);
        assert!(out.is_empty(), "reused buffer must be cleared when nothing changed");
    }

    #[test]
    fn static_stream_dedups_to_zero_after_first_frame() {
        // Drive the helper exactly as `animate` does over a STATIC frame stream: the first tick
        // sends all rows, every tick after sends NONE — the writes-saved story for a static effect.
        let cols = 22usize;
        let frame = vec![Rgb::new(0x4a, 0xf2, 0xb0); 6 * cols];
        let mut last: Option<Vec<Rgb>> = None;
        let mut sends = Vec::new();
        for _ in 0..10 {
            let changed = changed_rows(last.as_deref(), &frame, cols);
            sends.push(changed.len());
            // refresh the cache like the loop does.
            let buf = last.get_or_insert_with(|| Vec::with_capacity(frame.len()));
            buf.clear();
            buf.extend_from_slice(&frame);
        }
        assert_eq!(sends[0], 6, "first frame paints all 6 rows");
        assert!(sends[1..].iter().all(|&n| n == 0), "static stream sends nothing after the first");
    }

    // ── DEADLINE PACING: `pace` ───────────────────────────────────────────────────────────

    #[test]
    fn pace_ahead_sleeps_the_remainder() {
        use std::time::{Duration, Instant};
        let base = Instant::now();
        let dt = Duration::from_millis(16);
        // work finished 6ms before the deadline → sleep that 6ms, deadline unchanged.
        let deadline = base + Duration::from_millis(10);
        let (nd, nap) = pace(deadline, base, dt);
        assert_eq!(nd, deadline);
        assert_eq!(nap, Duration::from_millis(10));
    }

    #[test]
    fn pace_overrun_does_not_sleep() {
        use std::time::{Duration, Instant};
        let base = Instant::now();
        let dt = Duration::from_millis(16);
        // work ran 5ms PAST the deadline but still within one dt → no sleep, deadline untouched
        // (the next `deadline += dt` naturally absorbs the small lag).
        let deadline = base;
        let now = base + Duration::from_millis(5);
        let (nd, nap) = pace(deadline, now, dt);
        assert_eq!(nap, Duration::ZERO);
        assert_eq!(nd, deadline);
    }

    #[test]
    fn pace_runaway_lag_is_clamped_to_one_frame() {
        use std::time::{Duration, Instant};
        let base = Instant::now();
        let dt = Duration::from_millis(16);
        // a long stall: we're 50ms past a 16ms deadline. No sleep, and the deadline is nudged so the
        // remaining lag is EXACTLY one dt — preventing a no-sleep catch-up burst on the next frames.
        let deadline = base;
        let now = base + Duration::from_millis(50);
        let (nd, nap) = pace(deadline, now, dt);
        assert_eq!(nap, Duration::ZERO);
        assert_eq!(now - nd, dt, "lag must be clamped to one frame interval");
    }

    #[test]
    fn vk_to_key_cell_bridges_live_keys_to_true_cells() {
        // The VK→cell bridge Reactive uses: every standard keyboard key resolves to the SAME cell the
        // name map gives — letters, digits, the F-row, the numpad, the L/R modifiers, arrows and the
        // OEM punctuation all bridge to their TRUE position (no hashed-random fallback any more). One
        // LED per key, so each maps to a SINGLE cell.
        assert_eq!(vk_to_key_cell(0x31), Some((1, 2))); // VK '1'
        assert_eq!(vk_to_key_cell(0x30), Some((1, 11))); // VK '0'
        assert_eq!(vk_to_key_cell(0x41), Some((3, 2))); // 'A' (was None before the full map)
        assert_eq!(vk_to_key_cell(0x5A), Some((4, 3))); // 'Z'
        assert_eq!(vk_to_key_cell(0x1B), Some((0, 1))); // VK_ESCAPE
        assert_eq!(vk_to_key_cell(0x70), Some((0, 3))); // VK_F1
        assert_eq!(vk_to_key_cell(0x7B), Some((0, 14))); // VK_F12
        assert_eq!(vk_to_key_cell(0x60), razer_key_cell("NUM0")); // numpad 0
        assert_eq!(vk_to_key_cell(0x6A), razer_key_cell("NUMMULTIPLY"));
        assert_eq!(vk_to_key_cell(0xA0), Some((4, 1))); // LSHIFT
        assert_eq!(vk_to_key_cell(0xA3), Some((5, 14))); // RCTRL
        assert_eq!(vk_to_key_cell(0x5B), razer_key_cell("WIN")); // VK_LWIN
        assert_eq!(vk_to_key_cell(0x5C), razer_key_cell("WIN")); // VK_RWIN → same WIN cell
        assert_eq!(vk_to_key_cell(0x25), Some((5, 15))); // LEFT arrow
        assert_eq!(vk_to_key_cell(0x26), Some((4, 16))); // UP arrow
        assert_eq!(vk_to_key_cell(0xBD), razer_key_cell("-")); // VK_OEM_MINUS
        assert_eq!(vk_to_key_cell(0xBB), razer_key_cell("=")); // VK_OEM_PLUS
        assert_eq!(vk_to_key_cell(0xC0), razer_key_cell("`")); // VK_OEM_3 backtick
        assert_eq!(vk_to_key_cell(0xDB), razer_key_cell("[")); // VK_OEM_4 left bracket
        // SPACE bridges to its SINGLE standard cell (5,7) — not a multi-cell footprint. On a board
        // whose space bar has no LED there, that cell is dark, so pressing space lights nothing.
        assert_eq!(vk_to_key_cell(0x20), Some((5, 7))); // VK_SPACE → single standard cell
        // the GENERIC modifiers are deliberately unmapped (they fire alongside the L/R specifics,
        // so mapping them too would double-light) — Reactive lights NOTHING for them.
        assert_eq!(vk_to_key_cell(0x10), None); // generic SHIFT
        assert_eq!(vk_to_key_cell(0x11), None); // generic CTRL
        assert_eq!(vk_to_key_cell(0x12), None); // generic ALT
        // a non-keyboard VK (left mouse button) is not on the board → None, lights nothing.
        assert_eq!(vk_to_key_cell(0x01), None);
    }

    #[test]
    fn razer_key_map_has_no_duplicate_cells() {
        // Every CANONICAL key (aliases excluded — they intentionally share a cell) must own a UNIQUE
        // cell, and every cell must fit the 6×22 matrix. Catches transcription collisions in the map.
        use std::collections::HashMap;
        let mut seen: HashMap<(u8, u8), &str> = HashMap::new();
        for &name in razer_keyboard_keys() {
            let cell = razer_key_cell(name)
                .unwrap_or_else(|| panic!("'{name}' is listed in razer_keyboard_keys() but not in the map"));
            assert!(cell.0 < 6 && cell.1 < 22, "'{name}' cell {cell:?} is out of the 6x22 matrix");
            if let Some(prev) = seen.insert(cell, name) {
                panic!("cell {cell:?} maps to BOTH '{prev}' and '{name}' — transcription collision");
            }
        }
    }

    #[test]
    fn emulation_produces_full_frames() {
        // wave fills every LED; spectrum is uniform; static is the base colour
        let f = render_frame(Effect::Wave, 2, 3, 0.25, Rgb::new(255, 0, 0));
        assert_eq!(f.len(), 6);
        let s = render_frame(Effect::Static, 2, 3, 0.0, Rgb::new(9, 9, 9));
        assert!(s.iter().all(|&c| c == Rgb::new(9, 9, 9)));
        let sp = render_frame(Effect::Spectrum, 1, 4, 0.0, Rgb::BLACK);
        assert!(sp.windows(2).all(|w| w[0] == w[1])); // uniform across device
    }

    #[test]
    fn brightness_report_scales_to_255() {
        let d = matrix_def();
        let r = d.brightness_report(100).unwrap();
        assert_eq!(*r.args.last().unwrap(), 0xFF);
        let r = d.brightness_report(50).unwrap();
        assert_eq!(*r.args.last().unwrap(), 127);
    }

    // ── CROSS-DEVICE DATA SURFACE: render_vitals ─────────────────────────────────────────

    // cell index helper for a 6×22 matrix, by Neuron's canonical key name (panics if the key isn't mapped —
    // tests only name keys we know are in the map).
    fn cell(name: &str) -> usize {
        let (r, c) = razer_key_cell(name).unwrap();
        r as usize * 22 + c as usize
    }

    #[test]
    fn razer_key_map_matches_verified_matrix_anchors() {
        // Sanity anchors for the hardware-verified BlackWidow 6×22 matrix.
        assert_eq!(razer_key_cell("ESC"), Some((0, 1)));
        assert_eq!(razer_key_cell("M6"), Some((0, 0)));
        assert_eq!(razer_key_cell("LOGO"), Some((0, 20)));
        assert_eq!(razer_key_cell("M1"), Some((1, 0)));
        assert_eq!(razer_key_cell("M5"), Some((5, 0)));
        // F1 starts at col 3; number row is row 1 with '1' at col 2 through '=' at col 13.
        assert_eq!(razer_key_cell("F1"), Some((0, 3)));
        assert_eq!(razer_key_cell("F12"), Some((0, 14)));
        assert_eq!(razer_key_cell("1"), Some((1, 2)));
        assert_eq!(razer_key_cell("="), Some((1, 13)));
        // a few aliases + an unknown.
        assert_eq!(razer_key_cell("-"), razer_key_cell("DASH"));
        assert_eq!(razer_key_cell("`"), Some((1, 1)));
        // SPACE has a SINGLE standard cell (5,7); it's the canonical name (dark on boards without
        // that LED). "SPACEBAR" is not a canonical name → unknown → None.
        assert_eq!(razer_key_cell("SPACE"), Some((5, 7)));
        assert_eq!(razer_key_cell("SPACEBAR"), None);
    }

    #[test]
    fn battery_color_low_reads_red_then_ramps() {
        // ≤25% is the flat RED danger plateau — 20% (the live Naga reading) is unmistakably red.
        assert_eq!(battery_color(0), Rgb::new(255, 0, 0));
        assert_eq!(battery_color(20), Rgb::new(255, 0, 0));
        assert_eq!(battery_color(25), Rgb::new(255, 0, 0));
        assert_eq!(battery_color(100), Rgb::new(0, 255, 40)); // full = green
        // mid (40%) sits in the red→amber ramp: warm (high R, some G), not yet green.
        let mid = battery_color(40);
        assert!(mid.r > 200 && mid.g > 0 && mid.b == 0);
        // monotone-ish: low is redder (more R, less G) than high.
        let low = battery_color(15);
        let high = battery_color(85);
        assert!(low.r >= high.r);
        assert!(low.g <= high.g);
    }

    #[test]
    fn vitals_battery_gauge_lights_number_row_keys() {
        // 50% lights round(0.5 × 12) = 6 number-row keys ('1'..'6'); the rest of the row is OFF.
        let v = Vitals { battery_pct: 50, charging: false, active_stage: 0, stage_count: 2 };
        let f = render_vitals(v, 6, 22, 0.0);
        assert_eq!(f.len(), 6 * 22);
        let fill = battery_color(50);
        for key in ["1", "2", "3", "4", "5", "6"] {
            assert_eq!(f[cell(key)], fill, "{key} lit with the battery fill");
        }
        // the 7th key onward (and the backtick, never part of the gauge) are OFF — no ghost track.
        for key in ["7", "8", "9", "0", "-", "=", "`"] {
            assert_eq!(f[cell(key)], Rgb::BLACK, "{key} unlit / off");
        }
    }

    #[test]
    fn vitals_full_and_empty_battery_bounds() {
        let full = render_vitals(
            Vitals { battery_pct: 100, charging: false, active_stage: 0, stage_count: 1 },
            6,
            22,
            0.0,
        );
        // 100% lights every number-row gauge key with the green fill.
        for key in ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "-", "="] {
            assert_eq!(full[cell(key)], Rgb::new(0, 255, 40), "{key} green at 100%");
        }
        // 1% still lights at least one key (never reads as fully empty), and ONLY the first.
        let one = render_vitals(
            Vitals { battery_pct: 1, charging: false, active_stage: 0, stage_count: 1 },
            6,
            22,
            0.0,
        );
        assert_ne!(one[cell("1")], Rgb::BLACK, "1% lights the first number key");
        assert_eq!(one[cell("2")], Rgb::BLACK, "and nothing past it");
    }

    #[test]
    fn vitals_active_stage_pip_is_bright_others_dim_on_fkeys() {
        // 3 stages, stage index 1 active -> F1 dim, F2 BRIGHT cyan, F3 dim, F4 off.
        let v = Vitals { battery_pct: 80, charging: false, active_stage: 1, stage_count: 3 };
        let f = render_vitals(v, 6, 22, 0.0);
        assert_eq!(f[cell("F1")], Rgb::new(0, 22, 45)); // stage 0 dim
        assert_eq!(f[cell("F2")], Rgb::new(0, 200, 255)); // stage 1 ACTIVE bright cyan
        assert_eq!(f[cell("F3")], Rgb::new(0, 22, 45)); // stage 2 dim
        assert_eq!(f[cell("F4")], Rgb::BLACK); // no 4th stage
    }

    #[test]
    fn vitals_charging_crest_alters_the_lit_bar() {
        // charging shifts at least one lit number-row key toward cyan (the travelling crest).
        let off = render_vitals(
            Vitals { battery_pct: 60, charging: false, active_stage: 0, stage_count: 2 },
            6,
            22,
            0.0,
        );
        let on = render_vitals(
            Vitals { battery_pct: 60, charging: true, active_stage: 0, stage_count: 2 },
            6,
            22,
            0.0,
        );
        let gauge = ["1", "2", "3", "4", "5", "6", "7", "8"];
        assert!(
            gauge.iter().any(|&k| off[cell(k)] != on[cell(k)]),
            "charging crest must alter the lit bar"
        );
    }

    #[test]
    fn vitals_handles_small_and_zero_dims() {
        // a tiny matrix paints whatever cells fit and skips the rest — no panic, no out-of-bounds.
        let f = render_vitals(
            Vitals { battery_pct: 50, charging: false, active_stage: 0, stage_count: 2 },
            1,
            2,
            0.0,
        );
        assert_eq!(f.len(), 2);
        // degenerate dims return an empty/clean frame, never panic.
        assert!(render_vitals(
            Vitals { battery_pct: 50, charging: false, active_stage: 0, stage_count: 2 },
            0,
            0,
            0.0
        )
        .is_empty());
    }
}
