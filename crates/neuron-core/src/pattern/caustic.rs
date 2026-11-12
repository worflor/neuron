// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Shallow-water tank lit from above (`caustic` pattern / `tidepool` preset).
//!
//! One surface drives everything: a slow analytic swell plus a damped wave sim that keystrokes
//! disturb. The floor brightness is the ray-density of refracting sunlight through that surface
//! (`1 / |det(I - D·Hess)|`), so a splash reshapes the real caustic web instead of sitting on it.

use std::f32::consts::TAU;
use std::time::Duration;

use crate::effects::{Param, ParamKind};
use crate::lighting::Rgb;
use crate::spectrum::{Motion, Palette, Spectrum, Stop};

use super::{
    default_control_to_matrix, render_input, speed_param, xorshift, Bounds, Cell, EmitContext,
    Field, FrameContext, LayerGeometry, MatrixInputSnapshot, Params, Pattern, PatternReconfigure,
    ProcessInputHub, ReconfigurePolicy, StepClock,
};

const SUBSTEP_HZ: f32 = 120.0;
const DT: f32 = 1.0 / SUBSTEP_HZ;
const MAX_SUBSTEPS_PER_FRAME: u32 = 32;

/// Wave speed (cells/s), velocity damping (1/s), viscosity, and the restoring term that keeps the
/// mean level flat. Damping is low enough that a strike rings for two or three visible crests.
const C0: f32 = 5.0;
const GAMMA0: f32 = 0.85;
const NU: f32 = 0.42;
const KAPPA: f32 = 0.9;

const A_STRIKE: f32 = 8.0;
const SIGMA_STRIKE: f32 = 0.85;
const A_DROP_FRAC: f32 = 0.45;
const SIGMA_DROP: f32 = 1.0;

const K_PIN: f32 = 22.0;
const H_PIN: f32 = -0.22;
const HOLD_SUSTAIN: f32 = 0.5;
const HOLD_SETTLE_RATE: f32 = 3.0;
const CONTACT_DECAY_RATE: f32 = 3.0;

const DROP_RATE_BASE: f32 = 0.16;

/// Optical depth: how far a unit surface slope throws a ray on the floor, in cells.
const DEPTH_D: f32 = 1.1;
/// Softens the focal singularity so filaments have width instead of a one-LED needle.
const DET_SOFT: f32 = 0.30;
const FLOOR_BASE: f32 = 0.42;
const FOAM_THRESH: f32 = 0.22;
const FOAM_DECAY: f32 = 1.6;

const SEED_INIT: u32 = 0x7D1D_E9A3;

/// (wavelength in cells, direction in degrees, angular frequency rad/s, amplitude)
const SWELL_MODES: [(f32, f32, f32, f32); 5] = [
    (7.5, 20.0, 0.70, 0.55),
    (5.8, 95.0, 0.80, 0.40),
    (6.6, 150.0, 0.62, 0.45),
    (4.6, 215.0, 0.90, 0.28),
    (8.5, 290.0, 0.55, 0.45),
];

pub fn swell_param() -> Param {
    Param {
        key: "swell",
        label: "swell",
        only_when: None,
        kind: ParamKind::Range {
            min: 0.0,
            max: 2.0,
            default: 1.0,
        },
    }
}

pub fn sharpness_param() -> Param {
    Param {
        key: "sharpness",
        label: "focus",
        only_when: None,
        kind: ParamKind::Range {
            min: 0.25,
            max: 3.0,
            default: 1.0,
        },
    }
}

pub fn caustic_params() -> Vec<Param> {
    vec![speed_param(), swell_param(), sharpness_param()]
}

/// Deep trench navy -> submerged teal -> lagoon cyan -> aquamarine -> foam white.
pub fn caustic_spectrum() -> Spectrum {
    Spectrum::from_palette(Palette::new(
        vec![
            Stop::new(Rgb::new(6, 24, 50), 0.0),
            Stop::new(Rgb::new(8, 74, 122), 0.28),
            Stop::new(Rgb::new(18, 168, 204), 0.58),
            Stop::new(Rgb::new(120, 244, 226), 0.84),
            Stop::new(Rgb::new(240, 253, 255), 1.0),
        ],
        Motion::Hold,
    ))
}

pub struct Caustic {
    speed: f32,
    sharpness: f32,
    swell: f32,
    dims: (u8, u8),
    bounds: Bounds,
    visible_region: Vec<u32>,
    active: Vec<bool>,
    active_indices: Vec<usize>,
    h: Vec<f32>,
    v: Vec<f32>,
    h_lap: Vec<f32>,
    v_lap: Vec<f32>,
    contact: Vec<f32>,
    foam: Vec<f32>,
    held_mask: Vec<bool>,
    tide_phase: [f32; SWELL_MODES.len()],
    next_drop_wait: Option<f32>,
    last_press_at: Option<Duration>,
    rng: u32,
    clock: StepClock,
    last_field_t: Option<f32>,
    field_input: ProcessInputHub,
    field_input_generation: u64,
}

impl Default for Caustic {
    fn default() -> Self {
        Caustic {
            speed: 1.0,
            sharpness: 1.0,
            swell: 1.0,
            dims: (0, 0),
            bounds: Bounds::board(0, 0),
            visible_region: Vec::new(),
            active: Vec::new(),
            active_indices: Vec::new(),
            h: Vec::new(),
            v: Vec::new(),
            h_lap: Vec::new(),
            v_lap: Vec::new(),
            contact: Vec::new(),
            foam: Vec::new(),
            held_mask: Vec::new(),
            tide_phase: [0.0, 1.3, 2.6, 3.9, 5.2],
            next_drop_wait: None,
            last_press_at: None,
            rng: SEED_INIT,
            clock: StepClock::default(),
            last_field_t: None,
            field_input: ProcessInputHub::new(),
            field_input_generation: 0,
        }
    }
}

impl Caustic {
    fn rand(&mut self) -> f32 {
        xorshift(&mut self.rng)
    }

    fn rebuild_active_mask(&mut self, rows: u8, cols: u8, bounds: Bounds, region: &[u32]) {
        let (r, c) = (rows as usize, cols as usize);
        let n = r * c;
        self.active.resize(n, false);
        self.active.fill(false);
        self.active_indices.clear();

        if n == 0 {
            return;
        }

        if region.is_empty() {
            let r0 = (bounds.row0 as usize).min(r);
            let c0 = (bounds.col0 as usize).min(c);
            let r1 = (r0 + bounds.rows as usize).min(r);
            let c1 = (c0 + bounds.cols as usize).min(c);
            for row in r0..r1 {
                for col in c0..c1 {
                    let idx = row * c + col;
                    self.active[idx] = true;
                    self.active_indices.push(idx);
                }
            }
        } else {
            for &cell in region {
                let idx = cell as usize;
                if idx < n && !self.active[idx] {
                    self.active[idx] = true;
                    self.active_indices.push(idx);
                }
            }
        }

        for i in 0..n {
            if !self.active[i] {
                self.h[i] = 0.0;
                self.v[i] = 0.0;
                self.contact[i] = 0.0;
                self.foam[i] = 0.0;
                self.held_mask[i] = false;
            }
        }
    }

    fn ensure_geometry(&mut self, geom: &LayerGeometry) {
        let (rows, cols) = geom.board_dims;
        let n = rows as usize * cols as usize;
        let effective_region: Vec<u32> = if !geom.region.is_empty() {
            geom.region.clone()
        } else {
            self.visible_region.clone()
        };
        let effective_bounds = if geom.bounds.rows > 0 && geom.bounds.cols > 0 {
            geom.bounds
        } else if self.bounds.rows > 0 && self.bounds.cols > 0 {
            self.bounds
        } else {
            Bounds::board(rows, cols)
        };

        if self.dims != geom.board_dims {
            self.dims = geom.board_dims;
            self.bounds = effective_bounds;
            self.visible_region = effective_region.clone();
            self.rng = SEED_INIT;
            self.clock.reset();
            self.tide_phase = Caustic::default().tide_phase;
            self.next_drop_wait = None;
            self.last_press_at = None;
            self.h = vec![0.0; n];
            self.v = vec![0.0; n];
            self.h_lap = vec![0.0; n];
            self.v_lap = vec![0.0; n];
            self.contact = vec![0.0; n];
            self.foam = vec![0.0; n];
            self.held_mask = vec![false; n];
            self.rebuild_active_mask(rows, cols, effective_bounds, &effective_region);
        } else {
            let need_mask_rebuild = self.active.len() != n
                || self.bounds != effective_bounds
                || self.visible_region != effective_region;
            if need_mask_rebuild {
                self.bounds = effective_bounds;
                if self.visible_region != effective_region {
                    self.visible_region = effective_region.clone();
                }
                self.rebuild_active_mask(rows, cols, effective_bounds, &effective_region);
            }
        }
    }

    /// Neumann neighbour: an off-board or inactive neighbour reflects the centre value, so the
    /// tank (and any region mask) behaves as a closed basin with reflecting walls.
    #[inline]
    fn sample_field(
        field: &[f32],
        active: &[bool],
        r: usize,
        c: usize,
        dr: isize,
        dc: isize,
        rows: usize,
        cols: usize,
    ) -> f32 {
        let rr = r as isize + dr;
        let cc = c as isize + dc;
        if rr >= 0 && rr < rows as isize && cc >= 0 && cc < cols as isize {
            let idx = rr as usize * cols + cc as usize;
            if active[idx] {
                return field[idx];
            }
        }
        if dr != 0 && dc != 0 {
            if cc >= 0 && cc < cols as isize {
                let idx_h = r * cols + cc as usize;
                if active[idx_h] {
                    return field[idx_h];
                }
            }
            if rr >= 0 && rr < rows as isize {
                let idx_v = rr as usize * cols + c;
                if active[idx_v] {
                    return field[idx_v];
                }
            }
        }
        field[r * cols + c]
    }

    #[inline]
    fn vertical_metric(rows: usize, cols: usize) -> f32 {
        if rows >= 4 && cols >= 2 * rows { 1.18 } else { 1.0 }
    }

    fn compute_laplacian(
        field: &[f32],
        active: &[bool],
        active_indices: &[usize],
        rows: usize,
        cols: usize,
        out: &mut [f32],
    ) {
        out.fill(0.0);
        let ky = Self::vertical_metric(rows, cols);
        for &idx in active_indices {
            let r = idx / cols;
            let c = idx % cols;
            let center = field[idx];
            let s = |dr, dc| Self::sample_field(field, active, r, c, dr, dc, rows, cols);

            let hxx = s(0, 1) - 2.0 * center + s(0, -1);
            let hyy = (s(1, 0) - 2.0 * center + s(-1, 0)) * ky;
            let diag = (s(-1, -1) + s(-1, 1) + s(1, -1) + s(1, 1)) - 4.0 * center;
            out[idx] = (2.0 * (hxx + hyy) + diag) / 3.0;
        }
    }

    /// Inject a mass-conserving Ricker (Mexican-hat) velocity impulse centred at `(r0, c0)`.
    pub fn inject_impulse(&mut self, r0: f32, c0: f32, amp: f32, sigma: f32) {
        let (rows, cols) = (self.dims.0 as usize, self.dims.1 as usize);
        if self.active_indices.len() <= 1 || rows == 0 || cols == 0 {
            return;
        }

        let inv_2sig2 = 1.0 / (2.0 * sigma * sigma).max(1e-4);
        let reach = (3.2 * sigma).ceil() as isize;
        let rc = r0.round() as isize;
        let cc = c0.round() as isize;

        let r_min = (rc - reach).max(0) as usize;
        let r_max = (rc + reach).min(rows as isize - 1).max(0) as usize;
        let c_min = (cc - reach).max(0) as usize;
        let c_max = (cc + reach).min(cols as isize - 1).max(0) as usize;

        let mut stencil = Vec::with_capacity((r_max - r_min + 1) * (c_max - c_min + 1));
        let mut sum_dv = 0.0f32;
        for r in r_min..=r_max {
            for c in c_min..=c_max {
                let idx = r * cols + c;
                if !self.active[idx] {
                    continue;
                }
                let dr = r as f32 - r0;
                let dc = c as f32 - c0;
                let q = (dr * dr + dc * dc) * inv_2sig2;
                if q <= 4.5 {
                    let dv = -amp * (1.0 - 1.65 * q) * (-q).exp();
                    stencil.push((idx, dv));
                    sum_dv += dv;
                }
            }
        }

        if !stencil.is_empty() {
            let local_mean = sum_dv / stencil.len() as f32;
            for (idx, dv) in stencil {
                self.v[idx] += dv - local_mean;
            }
        }
    }

    /// Strike a key cell: an immediate aqua contact flash on the key, a faint halo on its
    /// neighbours, and a plunge into the water. Spacebar and Enter spans hit harder and wider.
    pub fn strike_cell(&mut self, row: usize, col: usize) {
        let (rows, cols) = (self.dims.0 as usize, self.dims.1 as usize);
        if row >= rows || col >= cols {
            return;
        }
        let idx = row * cols + col;
        if !self.active[idx] {
            return;
        }
        self.contact[idx] = 1.0;

        for dr in -1isize..=1 {
            for dc in -1isize..=1 {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let nr = row as isize + dr;
                let nc = col as isize + dc;
                if nr >= 0 && (nr as usize) < rows && nc >= 0 && (nc as usize) < cols {
                    let nidx = nr as usize * cols + nc as usize;
                    if self.active[nidx] {
                        let halo = if dr == 0 || dc == 0 { 0.34 } else { 0.16 };
                        self.contact[nidx] = self.contact[nidx].max(halo);
                    }
                }
            }
        }

        let wide = rows >= 4
            && ((row == rows - 1 && col >= cols / 4 && col <= (cols * 3) / 4)
                || ((row == rows / 2 || row + 1 == rows / 2) && col + 3 >= cols));
        let (amp, sigma) = if wide {
            (A_STRIKE * 1.2, SIGMA_STRIKE * 1.3)
        } else {
            (A_STRIKE, SIGMA_STRIKE)
        };
        self.inject_impulse(row as f32, col as f32, amp, sigma);
    }

    fn substep(&mut self) {
        let (rows, cols) = (self.dims.0 as usize, self.dims.1 as usize);
        let active_len = self.active_indices.len();
        if active_len == 0 || rows == 0 || cols == 0 {
            return;
        }

        Self::compute_laplacian(&self.h, &self.active, &self.active_indices, rows, cols, &mut self.h_lap);
        Self::compute_laplacian(&self.v, &self.active, &self.active_indices, rows, cols, &mut self.v_lap);

        let swell = self.swell.clamp(0.0, 2.0);
        let c2 = C0 * C0;
        let v_decay = (-GAMMA0 * DT).exp();
        let foam_decay = (-FOAM_DECAY * DT).exp();
        let pin_blend = 1.0 - (-K_PIN * DT).exp();
        let pin_v_damp = (-K_PIN * DT).exp();

        let mut sum_free = 0.0f32;
        let mut free_count = 0usize;
        for &idx in &self.active_indices {
            let acc = c2 * self.h_lap[idx] + NU * self.v_lap[idx] - KAPPA * self.h[idx];

            let mut v_next = (self.v[idx] + acc * DT) * v_decay;
            let mut h_next = self.h[idx] + v_next * DT;

            let crest_conv = (-self.h_lap[idx]).max(0.0);
            let breaker = (h_next.max(0.0) * crest_conv - FOAM_THRESH).max(0.0);
            self.foam[idx] = (self.foam[idx] * foam_decay + 0.9 * breaker * DT).min(1.0);
            if self.foam[idx] < 0.005 {
                self.foam[idx] = 0.0;
            }

            let pinned = self.held_mask[idx] && self.contact[idx] <= HOLD_SUSTAIN + 0.04;
            if self.held_mask[idx] {
                self.contact[idx] =
                    (self.contact[idx] - HOLD_SETTLE_RATE * DT).max(HOLD_SUSTAIN);
            } else if self.contact[idx] > 0.0 {
                self.contact[idx] = (self.contact[idx] - CONTACT_DECAY_RATE * DT).max(0.0);
            }
            if pinned {
                h_next += (H_PIN - h_next) * pin_blend;
                v_next *= pin_v_damp;
            } else {
                free_count += 1;
            }

            self.v[idx] = v_next.clamp(-400.0, 400.0);
            self.h[idx] = h_next.clamp(-4.0, 4.0);
            if !pinned {
                sum_free += self.h[idx];
            }
        }

        // Level is conserved over the free surface; a held modifier only dents its own cell.
        if free_count > 1 {
            let mean_h = sum_free / free_count as f32;
            if mean_h.abs() > 1e-6 {
                for &idx in &self.active_indices {
                    if !(self.held_mask[idx] && self.contact[idx] <= HOLD_SUSTAIN + 0.04) {
                        self.h[idx] -= mean_h;
                    }
                }
            }
        }

        for (m, &(_, _, omega, _)) in SWELL_MODES.iter().enumerate() {
            self.tide_phase[m] = (self.tide_phase[m] + omega * DT).rem_euclid(TAU);
        }

        if swell > 0.0 && active_len > 1 {
            let rate = (swell * DROP_RATE_BASE * (active_len as f32) / 132.0).max(1e-4);
            if self.next_drop_wait.is_none() {
                let u = self.rand().max(1e-6);
                self.next_drop_wait = Some(-u.ln() / rate);
            }
            if let Some(mut wait) = self.next_drop_wait {
                wait -= DT;
                while wait <= 0.0 {
                    let pick = (self.rand() * (active_len as f32)) as usize % active_len;
                    let cell_idx = self.active_indices[pick];
                    let dr = (cell_idx / cols) as f32;
                    let dc = (cell_idx % cols) as f32;
                    self.inject_impulse(dr, dc, A_DROP_FRAC * A_STRIKE, SIGMA_DROP);
                    let u = self.rand().max(1e-6);
                    wait += -u.ln() / rate;
                }
                self.next_drop_wait = Some(wait);
            }
        }
    }

    fn apply_input_snapshot(&mut self, input: &MatrixInputSnapshot) {
        let (rows, cols) = (self.dims.0 as usize, self.dims.1 as usize);
        if rows == 0 || cols == 0 {
            return;
        }
        self.held_mask.fill(false);
        if crate::capture::key_reads_suppressed() {
            return;
        }
        for held in &input.held {
            let (r, c) = (held.row as usize, held.col as usize);
            if r < rows && c < cols {
                let idx = r * cols + c;
                if self.active[idx] {
                    self.held_mask[idx] = true;
                    if self.contact[idx] < HOLD_SUSTAIN {
                        self.contact[idx] = HOLD_SUSTAIN;
                    }
                }
            }
        }
        // The same sampled snapshot can be rendered twice; strike each press once.
        let seen = self.last_press_at;
        for press in &input.pressed {
            if seen.is_some_and(|s| press.at <= s) {
                continue;
            }
            self.strike_cell(press.row as usize, press.col as usize);
        }
        if let Some(latest) = input.pressed.iter().map(|p| p.at).max() {
            self.last_press_at = Some(self.last_press_at.map_or(latest, |s| s.max(latest)));
        }
    }

    /// Analytic swell at cell `(x, y)`: `(height, [hxx, hyy, hxy])`.
    #[inline]
    fn swell_at(&self, x: f32, y: f32, amp: f32) -> (f32, [f32; 3]) {
        let mut h = 0.0;
        let mut hess = [0.0f32; 3];
        for (m, &(wl, deg, _, a)) in SWELL_MODES.iter().enumerate() {
            let k = TAU / wl;
            let (dy, dx) = deg.to_radians().sin_cos();
            let s = (k * (dx * x + dy * y) - self.tide_phase[m]).sin();
            let a = a * amp;
            h += a * s;
            let w = -a * k * k * s;
            hess[0] += w * dx * dx;
            hess[1] += w * dy * dy;
            hess[2] += w * dx * dy;
        }
        (h, hess)
    }
}

impl Pattern for Caustic {
    #[cfg(test)]
    fn state_witness(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            self.rng, self.tide_phase, self.h, self.v, self.contact, self.foam
        )
    }

    fn rebase_legacy_clock(&mut self, t: Duration) {
        self.last_field_t = Some(t.as_secs_f32());
        self.field_input = ProcessInputHub::new();
        self.field_input_generation = 0;
    }

    fn configure(&mut self, p: &Params) {
        self.speed = p.f32("speed", 1.0).clamp(0.25, 4.0);
        self.sharpness = p.f32("sharpness", 1.0).clamp(0.25, 3.0);
        self.swell = p.f32("swell", 1.0).clamp(0.0, 2.0);
    }

    fn set_bounds(&mut self, b: Bounds) {
        if self.bounds != b {
            self.bounds = b;
            let (rows, cols) = self.dims;
            if rows > 0 && cols > 0 {
                let region = self.visible_region.clone();
                self.rebuild_active_mask(rows, cols, b, &region);
            }
        }
    }

    fn set_visible_region(&mut self, region: &[u32], rows: u8, cols: u8) {
        if self.visible_region.as_slice() != region || self.dims != (rows, cols) {
            self.visible_region = region.to_vec();
            if self.dims == (rows, cols) && rows > 0 && cols > 0 {
                let b = self.bounds;
                self.rebuild_active_mask(rows, cols, b, region);
            }
        }
    }

    fn reconfigure_policy(&self, _delta: &PatternReconfigure) -> ReconfigurePolicy {
        ReconfigurePolicy::Preserve
    }

    fn apply_reconfigure(&mut self, delta: &PatternReconfigure) {
        self.configure(delta.params);
        if delta.geometry_changed {
            self.ensure_geometry(delta.next_geometry);
        }
    }

    fn on_discontinuity(&mut self) {
        self.clock.acc = 0.0;
        self.held_mask.fill(false);
        for c in &mut self.contact {
            *c *= 0.25;
            if *c < 0.02 {
                *c = 0.0;
            }
        }
        for f in &mut self.foam {
            *f *= 0.25;
            if *f < 0.02 {
                *f = 0.0;
            }
        }
        for v in &mut self.v {
            *v *= 0.5;
        }
    }

    fn advance(&mut self, ctx: &FrameContext, geom: &LayerGeometry) {
        let (rows, cols) = geom.board_dims;
        if rows == 0 || cols == 0 {
            return;
        }
        self.ensure_geometry(geom);

        if ctx.discontinuity {
            self.on_discontinuity();
        }

        let steps = self
            .clock
            .accrue(ctx.dt.as_secs_f32(), SUBSTEP_HZ, self.speed)
            .min(MAX_SUBSTEPS_PER_FRAME);
        for _ in 0..steps {
            self.substep();
        }
        // After the steps, so a fresh strike shows at full contact on the frame it lands.
        self.apply_input_snapshot(ctx.input);
    }

    fn emit(&self, _ctx: &EmitContext, geom: &LayerGeometry) -> Field {
        let (rows, cols) = (geom.board_dims.0 as usize, geom.board_dims.1 as usize);
        let n = rows * cols;
        if n == 0 {
            return Field::Scalar(Vec::new());
        }
        if self.h.len() != n || self.active.len() != n {
            return Field::Scalar(vec![Cell::default(); n]);
        }

        let sharp = self.sharpness.clamp(0.25, 3.0);
        let amp = self.swell.clamp(0.0, 2.0);
        let ky = Self::vertical_metric(rows, cols);
        let gain = 0.9 + 0.5 * sharp;

        let mut cells = vec![Cell::default(); n];
        for &idx in &self.active_indices {
            let r = idx / cols;
            let c = idx % cols;
            let s = |dr, dc| Self::sample_field(&self.h, &self.active, r, c, dr, dc, rows, cols);

            let center = self.h[idx];
            let hxx_s = s(0, 1) - 2.0 * center + s(0, -1);
            let hyy_s = (s(1, 0) - 2.0 * center + s(-1, 0)) * ky;
            let hxy_s = 0.25 * (s(1, 1) - s(1, -1) - s(-1, 1) + s(-1, -1));

            let (h_amb, [hxx_a, hyy_a, hxy_a]) = self.swell_at(c as f32, r as f32, amp);
            let hxx = hxx_s + hxx_a;
            let hyy = hyy_s + hyy_a;
            let hxy = hxy_s + hxy_a;

            // Floor ray map x -> x - D·grad(H) has Jacobian I - D·Hess; light piles up where
            // its determinant collapses.
            let det = (1.0 - DEPTH_D * hxx) * (1.0 - DEPTH_D * hyy) - DEPTH_D * DEPTH_D * hxy * hxy;
            let density = 1.0 / (det.abs() + DET_SOFT);
            let focus = ((density - 1.0 / (1.0 + DET_SOFT)) * gain).max(0.0);
            let lit = 1.0 - (-0.85 * focus).exp();

            let h_total = center + h_amb;
            let crest = (h_total / 0.45).tanh();
            let trough_dim = if center < 0.0 { (1.2 * center).exp() } else { 1.0 };

            // Splash crests of the sim surface read as light rings on top of the web.
            let ring = (center / 0.2).tanh().max(0.0);

            let mut intensity = FLOOR_BASE + (0.98 - FLOOR_BASE) * lit * trough_dim;
            intensity += 0.06 * crest + 0.50 * ring;
            let mut u = 0.16 + 0.56 * lit * trough_dim + 0.07 * crest + 0.30 * ring;

            let contact = self.contact[idx];
            if contact > 0.0 {
                u = u.max(0.58 + 0.26 * contact);
                intensity = intensity.max(0.28 + 0.68 * contact);
            }

            let foam = self.foam[idx];
            if foam > 0.0 {
                u += (1.0 - u) * foam;
                intensity = intensity.max(0.45 + 0.5 * foam);
            }

            cells[idx] = Cell::new(u.clamp(0.0, 1.0), intensity.clamp(0.0, 1.0));
        }

        Field::Scalar(cells)
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let geom = LayerGeometry {
            bounds: if self.bounds.rows > 0 && self.bounds.cols > 0 {
                self.bounds
            } else {
                Bounds::board(rows, cols)
            },
            region: self.visible_region.clone(),
            board_dims: (rows, cols),
        };

        if self.last_field_t.is_some_and(|last| t < last || t - last > 2.0) {
            self.on_discontinuity();
            self.field_input = ProcessInputHub::new();
            self.field_input_generation = 0;
        }

        let input = if let Some(input) = render_input() {
            (*input).clone()
        } else {
            if !crate::capture::key_reads_suppressed() {
                self.field_input
                    .poll_system_keys(Duration::from_secs_f32(t.max(0.0)));
            }
            let _ = self.field_input.publish_tick();
            let (input, generation) = self.field_input.snapshot_for_matrix(
                self.field_input_generation,
                default_control_to_matrix,
            );
            self.field_input_generation = generation;
            input
        };

        let dt = match self.last_field_t {
            Some(last) if t > last => (t - last).min(0.25),
            Some(_) => 0.0,
            None => 1.0 / 60.0,
        };
        self.last_field_t = Some(t);

        let ctx = FrameContext {
            session_epoch: 0,
            tick: 0,
            elapsed: Duration::from_secs_f32(t.max(0.0)),
            dt: Duration::from_secs_f32(dt),
            discontinuity: false,
            input: &input,
        };
        self.advance(&ctx, &geom);
        let emit_ctx = EmitContext {
            elapsed: Duration::from_secs_f32(t.max(0.0)),
        };
        self.emit(&emit_ctx, &geom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROWS: u8 = 6;
    const COLS: u8 = 22;

    fn geom() -> LayerGeometry {
        LayerGeometry {
            bounds: Bounds::board(ROWS, COLS),
            region: Vec::new(),
            board_dims: (ROWS, COLS),
        }
    }

    fn calm_tank() -> Caustic {
        let mut tank = Caustic::default();
        let mut p = Params::default();
        p.set("swell", 0.0);
        tank.configure(&p);
        tank
    }

    fn step(tank: &mut Caustic, i: u64, dt: Duration, input: &MatrixInputSnapshot) {
        let ctx = FrameContext {
            session_epoch: 0,
            tick: i,
            elapsed: dt * i as u32,
            dt,
            discontinuity: false,
            input,
        };
        tank.advance(&ctx, &geom());
    }

    fn scalar(f: Field) -> Vec<Cell> {
        match f {
            Field::Scalar(c) => c,
            _ => panic!("expected scalar field"),
        }
    }

    #[test]
    fn strike_conserves_level_and_reflects_off_the_wall() {
        let mut tank = calm_tank();
        let input = MatrixInputSnapshot::empty();
        step(&mut tank, 0, Duration::ZERO, &input);
        tank.inject_impulse(3.0, 1.0, A_STRIKE, SIGMA_STRIKE);
        step(&mut tank, 1, Duration::from_secs(1), &input);

        let mean: f32 = tank.h.iter().sum::<f32>() / tank.h.len() as f32;
        assert!(mean.abs() < 1e-4, "level must stay conserved (mean {mean})");
        assert!(tank.h[3 * COLS as usize].abs() > 1e-3, "wave should reach the left wall");
    }

    #[test]
    fn a_strike_rings_more_than_one_crest() {
        let mut tank = calm_tank();
        let input = MatrixInputSnapshot::empty();
        step(&mut tank, 0, Duration::ZERO, &input);
        tank.strike_cell(3, 11);
        let idx = 3 * COLS as usize + 11;
        let (mut sign_changes, mut prev) = (0, 0.0f32);
        for i in 1..=120 {
            step(&mut tank, i, Duration::from_millis(16), &input);
            let h = tank.h[idx];
            if h.abs() > 0.01 {
                if prev != 0.0 && h.signum() != prev.signum() {
                    sign_changes += 1;
                }
                prev = h;
            }
        }
        assert!(sign_changes >= 2, "strike cell should oscillate (crossed zero {sign_changes}x)");
    }

    #[test]
    fn region_mask_confines_the_water() {
        let mut tank = calm_tank();
        let g = LayerGeometry {
            bounds: Bounds { row0: 1, col0: 2, rows: 2, cols: 2 },
            region: vec![22 + 2, 22 + 3, 2 * 22 + 2, 2 * 22 + 3],
            board_dims: (ROWS, COLS),
        };
        let input = MatrixInputSnapshot::empty();
        let mut ctx = FrameContext {
            session_epoch: 0,
            tick: 0,
            elapsed: Duration::ZERO,
            dt: Duration::ZERO,
            discontinuity: false,
            input: &input,
        };
        tank.advance(&ctx, &g);
        tank.strike_cell(1, 2);
        for i in 1..=30u64 {
            ctx.tick = i;
            ctx.dt = Duration::from_millis(16);
            tank.advance(&ctx, &g);
        }
        for (i, &a) in tank.active.iter().enumerate() {
            if !a {
                assert_eq!(tank.h[i], 0.0, "water leaked into inactive cell {i}");
            }
        }
        let emit = EmitContext { elapsed: Duration::from_millis(480) };
        let cells = scalar(tank.emit(&emit, &g));
        for (i, &a) in tank.active.iter().enumerate() {
            if !a {
                assert_eq!(cells[i].intensity, 0.0, "inactive cell {i} must stay dark");
            }
        }
    }

    #[test]
    fn cadence_does_not_change_the_water() {
        let run = |dt: f32, count: usize| {
            let mut tank = calm_tank();
            let input = MatrixInputSnapshot::empty();
            step(&mut tank, 0, Duration::ZERO, &input);
            tank.strike_cell(3, 11);
            for i in 1..=count {
                step(&mut tank, i as u64, Duration::from_secs_f32(dt), &input);
            }
            tank.h
        };
        let (a, b) = (run(1.0 / 6.0, 6), run(1.0 / 60.0, 60));
        for i in 0..a.len() {
            assert!((a[i] - b[i]).abs() < 1e-4, "cadence mismatch at {i}: {} vs {}", a[i], b[i]);
        }
    }

    #[test]
    fn swell_phase_wraps_without_a_pop() {
        let mut tank = Caustic::default();
        tank.tide_phase = [TAU - 1e-3; SWELL_MODES.len()];
        let (before, _) = tank.swell_at(4.0, 2.0, 1.0);
        tank.tide_phase = [0.0; SWELL_MODES.len()];
        let (after, _) = tank.swell_at(4.0, 2.0, 1.0);
        assert!((before - after).abs() < 1e-2, "swell jumped across the wrap ({before} vs {after})");
    }

    #[test]
    fn idle_water_is_lit_and_has_structure() {
        let mut tank = Caustic::default();
        let input = MatrixInputSnapshot::empty();
        step(&mut tank, 0, Duration::ZERO, &input);
        let emit = EmitContext { elapsed: Duration::ZERO };
        let cells = scalar(tank.emit(&emit, &geom()));
        let lo = cells.iter().map(|c| c.intensity).fold(1.0f32, f32::min);
        let hi = cells.iter().map(|c| c.intensity).fold(0.0f32, f32::max);
        assert!(lo >= FLOOR_BASE - 0.1, "no cell may fall far below the lit floor (min {lo})");
        assert!(hi - lo > 0.25, "idle water needs visible caustic contrast ({lo}..{hi})");
    }

    #[test]
    fn a_repeated_snapshot_strikes_once() {
        let mut tank = calm_tank();
        let control = crate::pattern::ControlId::Key(crate::pattern::KeyId(65));
        let input = MatrixInputSnapshot {
            pressed: vec![crate::pattern::MatrixPress {
                control,
                row: 3,
                col: 11,
                at: Duration::from_millis(5),
            }],
            total_press_edges: 1,
            ..MatrixInputSnapshot::empty()
        };
        step(&mut tank, 0, Duration::ZERO, &MatrixInputSnapshot::empty());
        step(&mut tank, 1, Duration::from_millis(16), &input);
        let once = tank.v.clone();
        step(&mut tank, 2, Duration::ZERO, &input);
        assert_eq!(tank.v, once, "the same press must not strike twice");
    }

    #[test]
    fn a_held_key_dents_only_its_own_cell() {
        let mut tank = calm_tank();
        let control = crate::pattern::ControlId::Key(crate::pattern::KeyId(65));
        let held = MatrixInputSnapshot {
            held: vec![crate::pattern::MatrixHeld { control, row: 2, col: 8 }],
            ..MatrixInputSnapshot::empty()
        };
        step(&mut tank, 0, Duration::ZERO, &held);
        for i in 1..=90 {
            step(&mut tank, i, Duration::from_millis(16), &held);
        }
        let idx = 2 * COLS as usize + 8;
        assert!(tank.h[idx] < -0.1, "held cell keeps its meniscus (got {})", tank.h[idx]);
        let free: Vec<f32> = (0..tank.h.len()).filter(|&i| i != idx).map(|i| tank.h[i]).collect();
        let mean = free.iter().sum::<f32>() / free.len() as f32;
        assert!(mean.abs() < 0.02, "rest of the tank stays level (mean {mean})");
    }

    /// Writes an upscaled contact sheet for eyeballing: set `NEURON_CAUSTIC_PROOF=<dir>`, then
    /// `cargo test -p neuron caustic_proof -- --ignored`. Frames run idle, then two strikes.
    #[test]
    #[ignore = "writes images; run by hand"]
    fn caustic_proof() {
        let Ok(dir) = std::env::var("NEURON_CAUSTIC_PROOF") else { return };
        let spectrum = caustic_spectrum();
        let mut tank = Caustic::default();
        let control = crate::pattern::ControlId::Key(crate::pattern::KeyId(65));
        let mut frames: Vec<Vec<Rgb>> = Vec::new();
        step(&mut tank, 0, Duration::ZERO, &MatrixInputSnapshot::empty());
        let (mut t, mut tick) = (0.0f32, 1u64);
        // (seconds, keys pressed on the first frame of each 0.25s beat)
        let burst: [(u8, u8); 6] = [(3, 3), (2, 8), (3, 13), (2, 5), (4, 10), (3, 17)];
        let mut prev: Option<Vec<Cell>> = None;
        let mut max_idle_jump = 0.0f32;
        let total = 3.0 + 1.5 + 2.5;
        for k in 0..(total * 30.0) as usize {
            t += 1.0 / 30.0;
            let beat = ((t - 3.0) / 0.25).floor();
            let on_beat = t >= 3.0 && (t - 3.0) % 0.25 < 1.0 / 30.0 && (beat as usize) < burst.len();
            let input = if on_beat {
                let (row, col) = burst[beat as usize];
                MatrixInputSnapshot {
                    pressed: vec![crate::pattern::MatrixPress {
                        control, row, col, at: Duration::from_secs_f32(t),
                    }],
                    total_press_edges: 1,
                    ..MatrixInputSnapshot::empty()
                }
            } else {
                MatrixInputSnapshot::empty()
            };
            step(&mut tank, tick, Duration::from_secs_f32(1.0 / 30.0), &input);
            tick += 1;
            let f = tank.emit(&EmitContext { elapsed: Duration::ZERO }, &geom());
            if t < 3.0 {
                if let (Field::Scalar(now), Some(before)) = (&f, &prev) {
                    for (a, b) in now.iter().zip(before) {
                        max_idle_jump = max_idle_jump.max((a.intensity - b.intensity).abs());
                    }
                }
                if let Field::Scalar(now) = &f { prev = Some(now.clone()); }
            }
            if k % 3 == 0 {
                frames.push(f.render(&spectrum, 0.0));
            }
        }
        eprintln!("max idle frame-to-frame intensity jump: {max_idle_jump:.3}");

        let scale = 18usize;
        let per_row = 8usize;
        let (w, h) = (COLS as usize, ROWS as usize);
        let (cw, ch) = (w * scale + 6, h * scale + 6);
        let (sw, sh) = (per_row * cw, frames.len().div_ceil(per_row) * ch);
        let mut px = vec![0u8; sw * sh * 3];
        for (i, f) in frames.iter().enumerate() {
            let (ox, oy) = ((i % per_row) * cw, (i / per_row) * ch);
            for y in 0..h * scale {
                for x in 0..w * scale {
                    let c = f[(y / scale) * w + x / scale];
                    let o = ((oy + y) * sw + ox + x) * 3;
                    px[o] = c.r;
                    px[o + 1] = c.g;
                    px[o + 2] = c.b;
                }
            }
        }
        let mut out = format!("P6\n{sw} {sh}\n255\n").into_bytes();
        out.extend_from_slice(&px);
        std::fs::write(std::path::Path::new(&dir).join("caustic.ppm"), out).unwrap();
    }
}
