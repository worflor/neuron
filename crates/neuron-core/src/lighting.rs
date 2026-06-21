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

/// The superset of named effects across both eras. A device runs an effect natively if its
/// registry `effects` map names it; otherwise Neuron emulates it via streamed frames.
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
    pub fn is_emulatable(self) -> bool {
        !matches!(self, Effect::Reactive) // reactive needs keypress input from firmware
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
#[derive(Clone, Debug, Deserialize)]
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
    /// effect-id that displays a written custom frame (CUSTOMFRAME, 0x05 per OpenRazer).
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
    /// take `[custom_id, varstore]` (effect-first); matrix takes the prefix then the id.
    /// Confirmed live on the BlackWidow (`0x03/0x0A` args `05 00`).
    pub fn custom_display_report(&self) -> Report {
        let args = match self.protocol {
            Protocol::Legacy => vec![self.custom_id, self.varstore],
            Protocol::Matrix => {
                let mut a = self.effect.args.clone();
                a.push(self.custom_id);
                a
            }
        };
        Report {
            class: self.effect.class,
            id: self.effect.id,
            args,
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
    pub fn native_effect_report(&self, e: Effect, color: Option<Rgb>) -> Option<Report> {
        let id = *self.effects.get(e.name())?;
        let mut args = self.effect.args.clone(); // [varstore, led]
        args.push(id);
        if e.uses_color() {
            let c = color.unwrap_or(Rgb::new(0, 255, 0));
            match self.protocol {
                // extended-matrix colour effects carry a `00 00 01` (one-colour) preamble,
                // then RGB — confirmed live on the Naga (static red rendered).
                Protocol::Matrix => args.extend_from_slice(&[0x00, 0x00, 0x01, c.r, c.g, c.b]),
                Protocol::Legacy => args.extend_from_slice(&[c.r, c.g, c.b]),
            }
        }
        Some(Report {
            class: self.effect.class,
            id: self.effect.id,
            args,
        })
    }

    /// Break a full-device frame (`led_count` colours, row-major) into per-row custom-frame
    /// reports. This is how both eras paint at true resolution + how emulation streams.
    pub fn frame_reports(&self, frame: &[Rgb]) -> Vec<Report> {
        let Some(cf) = &self.custom_frame else {
            return Vec::new();
        };
        let cols = self.cols as usize;
        let mut out = Vec::new();
        for row in 0..self.rows as usize {
            let start = row * cols;
            if start >= frame.len() {
                break;
            }
            let end = (start + cols).min(frame.len());
            let span = &frame[start..end];
            // Common razer custom-frame row layout: [<prefix>, row, start_col, end_col, RGB..]
            let mut args = cf.args.clone();
            match self.protocol {
                Protocol::Matrix => {
                    args.push(row as u8);
                    args.push(0);
                    args.push((span.len() - 1) as u8);
                }
                Protocol::Legacy => {
                    args.push(row as u8);
                    args.push(0);
                    args.push((span.len() - 1) as u8);
                }
            }
            for c in span {
                args.extend_from_slice(&[c.r, c.g, c.b]);
            }
            out.push(Report {
                class: cf.class,
                id: cf.id,
                args,
            });
        }
        out
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
}

impl Report {
    /// Transparent hex preview (the "raw, transparent" motto) — what would hit the wire.
    pub fn preview(&self) -> String {
        let a: String = self.args.iter().map(|b| format!("{b:02X} ")).collect();
        format!(
            "class={:02X} id={:02X} args[{}]: {}",
            self.class,
            self.id,
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
}

impl<'a> Lights<'a> {
    pub fn new(dev: &'a crate::device::Device, def: LightingDef) -> Self {
        Lights { dev, def }
    }
    pub fn def(&self) -> &LightingDef {
        &self.def
    }

    /// Take host control (the driver-mode switch Synapse hides behind). Idempotent.
    pub fn ensure_control(&self) -> anyhow::Result<()> {
        if self.dev.run("device_mode").map(|m| m[0]).unwrap_or(0) != 0x03 {
            self.dev.exec_dynamic(0x00, 0x04, 0x02, &[0x03, 0x00])?;
        }
        Ok(())
    }

    /// Set an effect: a native firmware effect if the device has it, else emulate it by painting
    /// a computed frame. This is the legacy<->matrix translation core — anything a device lacks
    /// natively becomes a custom frame, which BOTH protocols support.
    pub fn set_effect(&self, e: Effect, color: Option<Rgb>, persist: bool) -> anyhow::Result<()> {
        if let Some(mut rep) = self.def.native_effect_report(e, color) {
            // VARSTORE persists the effect to onboard memory (survives with no software). Only
            // matrix devices have onboard lighting storage; legacy keyboards have none.
            if persist && self.def.protocol == Protocol::Matrix && !rep.args.is_empty() {
                rep.args[0] = 0x01;
            }
            self.dev.apply_lighting(&rep)?;
        } else {
            let frame = render_frame(
                e,
                self.def.rows,
                self.def.cols,
                0.0,
                color.unwrap_or(Rgb::new(0, 255, 0)),
            );
            self.paint_px(&frame)?;
        }
        Ok(())
    }

    /// Paint an arbitrary per-LED canvas — the universal path, protocol-translated.
    pub fn paint(&self, canvas: &Canvas) -> anyhow::Result<()> {
        self.paint_px(&canvas.px)
    }

    fn paint_px(&self, px: &[Rgb]) -> anyhow::Result<()> {
        for r in self.def.frame_reports(px) {
            self.dev.apply_lighting(&r)?;
        }
        self.dev.apply_lighting(&self.def.custom_display_report())?;
        Ok(())
    }

    /// Stream any frame generator smoothly (fire-and-forget writes, consistent timing). The
    /// generator decides the visuals; the backend handles control, translation, and streaming.
    /// `stop()` aborts early. This is the open-effects engine running live.
    pub fn animate(
        &self,
        generator: &mut dyn crate::effects::FrameGen,
        color: Option<Rgb>,
        fps: u64,
        secs: u64,
        mut stop: impl FnMut() -> bool,
    ) -> anyhow::Result<()> {
        use std::time::{Duration, Instant};
        self.ensure_control()?;
        let display = self.def.custom_display_report();
        let base = color.unwrap_or(Rgb::new(0, 255, 0));
        let fps = fps.max(1);
        let dt = Duration::from_millis(1000 / fps);
        let total = secs * fps;
        for i in 0..total {
            if stop() {
                break;
            }
            let tick = Instant::now();
            let elapsed = i as f32 / fps as f32; // continuous seconds
            let frame = generator.frame(self.def.rows, self.def.cols, elapsed, base);
            for r in self.def.frame_reports(&frame) {
                self.dev.send_lighting_fast(&r);
            }
            self.dev.send_lighting_fast(&display);
            if let Some(rem) = dt.checked_sub(tick.elapsed()) {
                std::thread::sleep(rem);
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
            },
            custom_frame: Some(CommandSpec {
                class: 0x0F,
                id: 0x03,
                size: 0x0C,
                args: vec![0x00],
            }),
            brightness: Some(CommandSpec {
                class: 0x0F,
                id: 0x04,
                size: 0x03,
                args: vec![0x00, 0x00],
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

    #[test]
    fn native_effect_args_are_prefix_plus_id_plus_color() {
        let d = matrix_def();
        // spectrum (no colour): args = prefix [00,00] + id 03
        let r = d.native_effect_report(Effect::Spectrum, None).unwrap();
        assert_eq!(r.class, 0x0F);
        assert_eq!(r.id, 0x02);
        assert_eq!(r.args, vec![0x00, 0x00, 0x03]);
        // static (colour) on matrix: prefix + id 01 + [00 00 01] preamble + RGB
        let r = d
            .native_effect_report(Effect::Static, Some(Rgb::new(10, 20, 30)))
            .unwrap();
        assert_eq!(r.args, vec![0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 10, 20, 30]);
        // breathing not in this device's native set
        assert!(d.native_effect_report(Effect::Breathing, None).is_none());
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
}
