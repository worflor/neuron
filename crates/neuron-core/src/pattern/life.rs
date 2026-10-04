// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Wildlife's deterministic, host-rendered cellular habitat.

use super::{Bounds, Cell, Field, Params, Pattern};
use std::collections::VecDeque;

const DT: f64 = 1.0 / 60.0;
const YEAR: f64 = 64.0;
const CONTACT_DECAY: f32 = 4.5;

/// The stateful pattern wrapper. Simulation logic is separately constructible for deterministic tests.
pub(super) struct Life {
    sim: LifeSim,
    cursor: u64,
    speed: f32,
    density: f32,
    prev: Vec<bool>,
    fallback_baseline: bool,
    fallback_was_active: bool,
    visible_region: Vec<u32>,
    visible_dims: Option<(u8, u8)>,
    pending_events: VecDeque<crate::capture::KeyObservation>,
    render_offset: Option<f64>,
    last_render_t: Option<f64>,
}

impl Default for Life {
    fn default() -> Self {
        Self {
            sim: LifeSim::new(0, 0, 0x4C49_4645),
            cursor: crate::capture::key_observation_head(),
            speed: 1.0,
            density: 1.0,
            prev: vec![false; super::KEY_SCAN_SLOTS],
            fallback_baseline: true,
            fallback_was_active: false,
            visible_region: Vec::new(),
            visible_dims: None,
            pending_events: VecDeque::new(),
            render_offset: None,
            last_render_t: None,
        }
    }
}

impl Pattern for Life {
    fn configure(&mut self, params: &Params) {
        self.speed = params.f32("speed", 1.0).clamp(0.25, 4.0);
        self.density = params.f32("density", 1.0).clamp(0.25, 3.0);
    }

    fn set_bounds(&mut self, _bounds: Bounds) {}

    fn set_visible_region(&mut self, region: &[u32], rows: u8, cols: u8) {
        self.visible_region.clear();
        self.visible_region.extend_from_slice(region);
        self.visible_dims = Some((rows, cols));
        self.sim
            .set_visible_region(region, rows as usize, cols as usize);
    }

    fn field(&mut self, rows: u8, cols: u8, t: f32) -> Field {
        let (events, missed, _) = crate::capture::key_observations_since(&mut self.cursor);
        if missed > 0 {
            self.sim.input_gap();
        }
        if crate::capture::key_reads_suppressed() {
            self.pending_events.clear();
        }
        self.pending_events.extend(events);
        while self.pending_events.len() > 512 {
            self.pending_events.pop_front();
        }
        let render_now = t as f64;
        let observation_now = crate::capture::key_observation_now();
        if self.render_offset.is_none() {
            self.render_offset = Some(((observation_now - render_now) / 4096.0).round() * 4096.0);
        }
        if self
            .last_render_t
            .is_some_and(|last| last > 4000.0 && render_now < 96.0)
        {
            self.render_offset = Some(self.render_offset.unwrap_or(0.0) + 4096.0);
        }
        self.last_render_t = Some(render_now);
        let sim_now = render_now + self.render_offset.unwrap_or(0.0);
        if self.sim.rows != rows as usize || self.sim.cols != cols as usize {
            let seed = 0x4C49_4645 ^ ((rows as u32) << 8) ^ cols as u32;
            self.sim = LifeSim::new(rows as usize, cols as usize, seed);
            let region = if self.visible_dims == Some((rows, cols)) {
                &self.visible_region[..]
            } else {
                &[]
            };
            self.sim
                .set_visible_region(region, rows as usize, cols as usize);
            self.cursor = crate::capture::key_observation_head();
            self.pending_events.clear();
            self.prev = vec![false; super::KEY_SCAN_SLOTS];
            self.fallback_baseline = true;
            self.fallback_was_active = false;
        }
        self.sim.speed = self.speed;
        self.sim.density = self.density;
        if self
            .sim
            .last_t
            .is_some_and(|last| sim_now - last > 2.0 || sim_now < last)
        {
            self.pending_events.clear();
            self.sim.advance_to(sim_now);
        }
        while self
            .pending_events
            .front()
            .is_some_and(|event| event.at <= sim_now)
        {
            if let Some(event) = self.pending_events.pop_front() {
                if event.down {
                    if let Some((row, col, kind)) = observation_cell(event.page, event.usage) {
                        let start = self.sim.last_t.unwrap_or(event.at);
                        let event_t = event.at.max(start).min(sim_now);
                        self.sim.advance_to(event_t);
                        self.sim.contact(row, col, kind, event_t);
                    }
                }
            }
        }
        self.sim.advance_to(sim_now);
        if crate::capture::key_reads_suppressed() {
            self.fallback_baseline = true;
        }
        let fallback = !crate::controls::held_registry_live();
        if !fallback {
            self.fallback_baseline = true;
            self.fallback_was_active = false;
        }
        if fallback && !self.fallback_was_active {
            self.fallback_baseline = true;
            self.fallback_was_active = true;
        }
        if !crate::capture::key_reads_suppressed() && fallback {
            for (vk, was_down) in self.prev.iter_mut().take(256).enumerate().skip(1) {
                let cell = crate::lighting::vk_to_key_cell(vk as i32);
                if cell.is_none() {
                    continue;
                }
                let down = crate::capture::key_down(vk as i32);
                if down && !*was_down && !self.fallback_baseline {
                    let (row, col) = cell.unwrap_or((0, 0));
                    let kind = match vk {
                        0x0D => ContactKind::Enter,
                        0x08 => ContactKind::Backspace,
                        _ => ContactKind::Key,
                    };
                    self.sim.contact(row as usize, col as usize, kind, sim_now);
                }
                *was_down = down;
            }
            self.fallback_baseline = false;
        }
        Field::Scalar(self.sim.render_cells(t as f64))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContactKind {
    Key,
    Enter,
    Backspace,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct LifeSim {
    rows: usize,
    cols: usize,
    live: Vec<bool>,
    next: Vec<bool>,
    death_scratch: Vec<usize>,
    age: Vec<u8>,
    lifespan: Vec<u8>,
    nutrient: Vec<f32>,
    nutrient_scratch: Vec<f32>,
    ghost: Vec<f32>,
    ghost_scratch: Vec<f32>,
    contact: Vec<f32>,
    visible: Vec<bool>,
    visibility_initialized: bool,
    tick_accum: f64,
    generation_acc: f64,
    last_t: Option<f64>,
    sim_t: f64,
    biological_t: f64,
    speed: f32,
    density: f32,
    weather_rng: u32,
    immigration_rng: u32,
    autumn_year: Option<i64>,
    autumn_direction: isize,
    quiet_for: f64,
    occupancy_ema: f32,
    ghost_ema: f32,
    last_key_at: Option<f64>,
    recent: VecDeque<(f64, usize)>,
    enter_at: f64,
    prune_at: f64,
    gust_at: f64,
    squall_at: f64,
    squall_charge: f64,
    scene: Option<Scene>,
    pending_star: Option<PendingStar>,
    star_due: f64,
    recovery_at: f64,
    empty_for: f64,
    renewal_at: f64,
    recurrence: VecDeque<Vec<u64>>,
    recurrence_at: f64,
    birth_envelope: Vec<f32>,
    death_envelope: Vec<f32>,
}

#[derive(Clone, Debug, PartialEq)]
struct Scene {
    kind: SceneKind,
    from: (f32, f32),
    to: (f32, f32),
    age: f64,
    duration: f64,
    admission: Vec<bool>,
    visited: Vec<bool>,
    mortality_left: usize,
    footprint_left: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct PendingStar {
    due: f64,
    origins: Vec<usize>,
}

#[derive(Clone, Copy)]
struct LocalMotif {
    cells: &'static [(isize, isize)],
    orbit: &'static [(isize, isize)],
}

const BLOCK: &[(isize, isize)] = &[(0, 0), (0, 1), (1, 0), (1, 1)];
const BLINKER: &[(isize, isize)] = &[(1, 0), (1, 1), (1, 2)];
const BLINKER_ORBIT: &[(isize, isize)] = &[(0, 1), (1, 0), (1, 1), (1, 2), (2, 1)];
const BOAT: &[(isize, isize)] = &[(0, 0), (0, 1), (1, 0), (1, 2), (2, 1)];
const TOAD: &[(isize, isize)] = &[(1, 1), (1, 2), (1, 3), (2, 0), (2, 1), (2, 2)];
const TOAD_ORBIT: &[(isize, isize)] = &[
    (0, 2), (1, 0), (1, 1), (1, 2), (1, 3),
    (2, 0), (2, 1), (2, 2), (2, 3), (3, 1),
];
const LOCAL_MOTIFS: &[LocalMotif] = &[
    LocalMotif {
        cells: BLOCK,
        orbit: BLOCK,
    },
    LocalMotif {
        cells: BLINKER,
        orbit: BLINKER_ORBIT,
    },
    LocalMotif {
        cells: BOAT,
        orbit: BOAT,
    },
    LocalMotif {
        cells: TOAD,
        orbit: TOAD_ORBIT,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SceneKind {
    Squall,
    Renewal,
    Star,
}

impl LifeSim {
    pub(super) fn new(rows: usize, cols: usize, seed: u32) -> Self {
        let n = rows.saturating_mul(cols);
        let mut s = Self {
            rows,
            cols,
            live: vec![false; n],
            next: vec![false; n],
            death_scratch: Vec::with_capacity(n),
            age: vec![0; n],
            lifespan: vec![30; n],
            nutrient: vec![0.0; n],
            nutrient_scratch: vec![0.0; n],
            ghost: vec![0.0; n],
            ghost_scratch: vec![0.0; n],
            contact: vec![0.0; n],
            visible: vec![true; n],
            visibility_initialized: false,
            tick_accum: 0.0,
            generation_acc: 0.0,
            last_t: None,
            sim_t: 0.0,
            biological_t: 0.0,
            speed: 1.0,
            density: 1.0,
            weather_rng: seed ^ 0xA53C_9E17,
            immigration_rng: seed ^ 0x1B56_C4E9,
            autumn_year: None,
            autumn_direction: 1,
            quiet_for: 0.0,
            occupancy_ema: 0.0,
            ghost_ema: 0.0,
            last_key_at: None,
            recent: VecDeque::new(),
            enter_at: -10.0,
            prune_at: -10.0,
            gust_at: -10.0,
            squall_at: -100.0,
            squall_charge: 0.0,
            scene: None,
            pending_star: None,
            star_due: 160.0,
            recovery_at: 0.0,
            empty_for: 0.0,
            renewal_at: 0.0,
            recurrence: VecDeque::new(),
            recurrence_at: 0.0,
            birth_envelope: vec![0.0; n],
            death_envelope: vec![0.0; n],
        };
        s.seed_initial();
        s
    }

    fn set_visible_region(&mut self, region: &[u32], rows: usize, cols: usize) {
        if rows != self.rows || cols != self.cols {
            return;
        }
        let first = !self.visibility_initialized;
        self.visible.fill(region.is_empty());
        if !region.is_empty() {
            for &i in region {
                if let Some(v) = self.visible.get_mut(i as usize) {
                    *v = true;
                }
            }
        }
        if first && !region.is_empty() {
            self.live.fill(false);
            self.next.fill(false);
            self.age.fill(0);
            self.seed_initial();
        }
        self.visibility_initialized = true;
    }

    fn seed_initial(&mut self) {
        if self.rows < 3 || self.cols < 3 {
            return;
        }
        let motifs: &[&[(isize, isize)]] = &[
            &[(0, 0), (0, 1), (1, 0), (1, 1)],
            &[(0, 0), (0, 1), (0, 2)],
            &[(0, 1), (1, 2), (2, 0), (2, 1), (2, 2)],
        ];
        let origins = [
            (self.rows / 3, self.cols / 4),
            (self.rows / 2, self.cols / 2),
            (self.rows.saturating_sub(3), self.cols.saturating_sub(5)),
        ];
        for (which, motif) in motifs.iter().enumerate() {
            self.seed_cells(origins[which], motif, (which * 6) as u8);
        }
    }

    fn advance_to(&mut self, t: f64) {
        if !t.is_finite() {
            return;
        }
        if self.last_t.is_none() {
            self.last_t = Some(t);
            self.sim_t = t;
            return;
        }
        if let Some(last) = self.last_t {
            if t < last || t - last > 2.0 {
                self.last_t = Some(t);
                self.sim_t = t;
                self.biological_t = t;
                self.tick_accum = 0.0;
                self.generation_acc = 0.0;
                self.contact.fill(0.0);
                self.ghost.fill(0.0);
                self.nutrient.fill(0.0);
                self.birth_envelope.fill(0.0);
                self.death_envelope.fill(0.0);
                self.recent.clear();
                self.squall_charge = 0.0;
                self.scene = None;
                self.pending_star = None;
                self.empty_for = 0.0;
                self.recurrence.clear();
                self.recurrence_at = t;
                return;
            }
        }
        let dt = self.last_t.map_or(0.0, |last| (t - last).max(0.0));
        self.last_t = Some(t);
        self.tick_accum += dt;
        let ticks = (((self.tick_accum + 1e-9) / DT).floor() as usize).min(120);
        self.tick_accum -= ticks as f64 * DT;
        for _ in 0..ticks {
            self.sim_t += DT;
            self.tick(DT);
        }
    }

    fn tick(&mut self, dt: f64) {
        let dtf = dt as f32;
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| self.sim_t - *at > 2.5)
        {
            self.recent.pop_front();
        }
        let one_second_rate = self
            .recent
            .iter()
            .filter(|(at, _)| self.sim_t - *at <= 1.0)
            .count();
        if one_second_rate >= 18 {
            self.squall_charge += dt;
        } else {
            self.squall_charge = 0.0;
        }
        if self.squall_charge >= 2.5 && self.sim_t - self.squall_at > 20.0 {
            if let Some((_, i)) = self.recent.back().copied() {
                self.start_squall(i);
            }
        }
        self.biological_t += dt * f64::from(self.speed);
        let seasons = season_weights(self.sim_t);
        self.generation_acc += dt * f64::from(self.speed) * season_generation(seasons);
        let ghost_rate =
            0.34 * (seasons[0] * 0.95 + seasons[1] * 1.15 + seasons[2] * 0.72 + seasons[3] * 0.86);
        for i in 0..self.live.len() {
            self.nutrient[i] =
                (self.nutrient[i] * (-std::f32::consts::LN_2 * dtf / 12.0).exp()).clamp(0.0, 1.0);
            self.ghost[i] = (self.ghost[i] * (-ghost_rate * dtf).exp()).clamp(0.0, 1.0);
            self.contact[i] = (self.contact[i] * (-CONTACT_DECAY * dtf).exp()).clamp(0.0, 1.0);
            self.birth_envelope[i] = (self.birth_envelope[i] - dtf * 3.3).max(0.0);
            self.death_envelope[i] = (self.death_envelope[i] - dtf * 1.8).max(0.0);
        }
        if seasons[2] > 0.0 {
            self.advect_ghosts(dtf, seasons[2]);
        }
        self.diffuse_nutrients(dtf);
        self.advance_scene(dtf);
        if self.generation_acc >= 1.0 / 3.0 {
            self.generation_acc -= 1.0 / 3.0;
            self.generation();
        }
        self.governor(dt);
    }

    fn advect_ghosts(&mut self, dt: f32, season_strength: f32) {
        let year = (self.sim_t / YEAR).floor() as i64;
        if self.autumn_year != Some(year) {
            self.autumn_year = Some(year);
            self.autumn_direction = if self.rand_weather() < 0.5 { -1 } else { 1 };
        }
        self.ghost_scratch.fill(0.0);
        let amount = (0.07 * dt * season_strength).clamp(0.0, 0.01);
        for i in 0..self.ghost.len() {
            let col = i % self.cols;
            let target_col = col as isize + self.autumn_direction;
            if target_col < 0 || target_col >= self.cols as isize {
                continue;
            }
            let moved = self.ghost[i] * amount;
            let target = (i / self.cols) * self.cols + target_col as usize;
            self.ghost_scratch[i] -= moved;
            self.ghost_scratch[target] += moved;
        }
        for i in 0..self.ghost.len() {
            self.ghost[i] = (self.ghost[i] + self.ghost_scratch[i]).clamp(0.0, 1.0);
        }
    }

    fn generation(&mut self) {
        let seasons = season_weights(self.sim_t);
        self.death_scratch.clear();
        for r in 0..self.rows {
            for c in 0..self.cols {
                let i = r * self.cols + c;
                let mut neighbors = 0;
                for dr in -1isize..=1 {
                    for dc in -1isize..=1 {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let nr = r as isize + dr;
                        let nc = c as isize + dc;
                        if nr >= 0
                            && nc >= 0
                            && nr < self.rows as isize
                            && nc < self.cols as isize
                            && self.live[nr as usize * self.cols + nc as usize]
                        {
                            neighbors += 1;
                        }
                    }
                }
                let born = !self.live[i] && neighbors == 3;
                let survives = self.live[i] && (neighbors == 2 || neighbors == 3);
                let age = if survives {
                    self.age[i].saturating_add(1 + u8::from(self.sun(i) > 0.7))
                } else if born {
                    1
                } else {
                    0
                };
                let nutrient_bonus = (self.nutrient[i] * 5.0) as u8;
                let dies_old =
                    survives && age >= self.lifespan[i].saturating_add(nutrient_bonus).min(44);
                self.next[i] = (born || survives) && !dies_old;
                if born {
                    self.age[i] = 1;
                    self.lifespan[i] = 26 + (self.rand_immigration() * 13.0) as u8;
                    self.nutrient[i] = (self.nutrient[i] - 0.05).max(0.0);
                    self.birth_envelope[i] = 1.0;
                } else if survives && !dies_old {
                    self.age[i] = age;
                    self.nutrient[i] = (self.nutrient[i] - 0.0008).max(0.0);
                } else if self.live[i] {
                    self.death_scratch.push(i);
                }
            }
        }
        for j in 0..self.death_scratch.len() {
            let i = self.death_scratch[j];
            self.kill_index(i, 0.25);
        }
        std::mem::swap(&mut self.live, &mut self.next);
        self.next.fill(false);
        if seasons[3] > 0.0 && self.rand_weather() < 0.0008 * seasons[3] {
            self.winter_seed();
        }
        self.record_recurrence();
    }

    fn diffuse_nutrients(&mut self, dt: f32) {
        if self.rows == 0 || self.cols == 0 {
            return;
        }
        self.nutrient_scratch.copy_from_slice(&self.nutrient);
        for r in 0..self.rows {
            for c in 0..self.cols {
                let i = r * self.cols + c;
                let mut sum = 0.0;
                let mut n = 0.0;
                for (dr, dc) in [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)] {
                    let nr = r as isize + dr;
                    let nc = c as isize + dc;
                    if nr >= 0 && nc >= 0 && nr < self.rows as isize && nc < self.cols as isize {
                        sum += self.nutrient[nr as usize * self.cols + nc as usize];
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    self.nutrient_scratch[i] += (sum / n - self.nutrient[i]) * (0.025 * dt);
                }
            }
        }
        std::mem::swap(&mut self.nutrient, &mut self.nutrient_scratch);
    }

    fn contact(&mut self, row: usize, col: usize, kind: ContactKind, event_t: f64) {
        if row >= self.rows || col >= self.cols {
            return;
        }
        let i = row * self.cols + col;
        if !self.visible[i] {
            return;
        }
        let rhythmic = self.last_key_at.is_some_and(|at| event_t - at < 0.20);
        self.contact[i] = 1.0;
        self.last_key_at = Some(event_t);
        self.quiet_for = 0.0;
        self.recent.push_back((event_t, i));
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| event_t - *at > 2.5)
        {
            self.recent.pop_front();
        }
        match kind {
            ContactKind::Enter => {
                if self.sim_t - self.enter_at >= 3.0 {
                    self.enter_at = self.sim_t;
                    if !self.depart(i) {
                        self.ghost[i] = self.ghost[i].max(0.36);
                    }
                } else {
                    self.ghost[i] = self.ghost[i].max(0.28);
                }
                self.feed(i, 0.12);
            }
            ContactKind::Backspace => {
                if self.sim_t - self.prune_at >= 1.5 {
                    self.prune_at = self.sim_t;
                    self.prune(i);
                }
                self.feed(i, 0.10);
            }
            ContactKind::Key => {
                self.feed(i, if rhythmic { 0.12 } else { 0.10 });
                if self.live[i] {
                    self.kill_index(i, 0.25);
                }
                if self.sim_t - self.enter_at < 0.35 {
                    self.ghost[i] = self.ghost[i].max(0.2);
                    return;
                }
                if self.recent.len() >= 4
                    && self.sim_t - self.gust_at >= 2.5
                    && self.positive_rise(event_t)
                {
                    self.gust_at = self.sim_t;
                    if !self.launch_from(i) {
                        self.ghost[i] = self.ghost[i].max(0.25);
                    }
                }
            }
        }
    }

    fn input_gap(&mut self) {
        self.recent.clear();
        self.last_key_at = None;
        self.squall_charge = 0.0;
    }

    fn feed(&mut self, center: usize, amount: f32) {
        let r = center / self.cols;
        let c = center % self.cols;
        for dr in -2isize..=2 {
            for dc in -2isize..=2 {
                let nr = r as isize + dr;
                let nc = c as isize + dc;
                if nr < 0 || nc < 0 || nr >= self.rows as isize || nc >= self.cols as isize {
                    continue;
                }
                let d2 = (dr * dr + dc * dc) as f32;
                if d2 <= 4.0 {
                    let i = nr as usize * self.cols + nc as usize;
                    self.nutrient[i] =
                        (self.nutrient[i] + amount * (1.0 - d2 / 5.0)).clamp(0.0, 1.0);
                }
            }
        }
    }

    fn kill_index(&mut self, i: usize, recycle: f32) {
        if !self.live[i] {
            return;
        }
        let maturity = (self.age[i] as f32 / 30.0).clamp(0.0, 1.0);
        self.live[i] = false;
        self.age[i] = 0;
        self.death_envelope[i] = 1.0;
        self.ghost[i] = (self.ghost[i] + 0.22 + 0.3 * maturity).min(1.0);
        self.nutrient[i] = (self.nutrient[i] + maturity * recycle).min(1.0);
    }

    fn seed_cells(&mut self, origin: (usize, usize), motif: &[(isize, isize)], age: u8) -> bool {
        if motif.is_empty() {
            return false;
        }
        for &(dr, dc) in motif {
            let r = origin.0 as isize + dr;
            let c = origin.1 as isize + dc;
            if r < 0
                || c < 0
                || r >= self.rows as isize
                || c >= self.cols as isize
                || !self.visible[r as usize * self.cols + c as usize]
                || self.live[r as usize * self.cols + c as usize]
            {
                return false;
            }
        }
        for &(dr, dc) in motif {
            let i =
                (origin.0 as isize + dr) as usize * self.cols + (origin.1 as isize + dc) as usize;
            self.live[i] = true;
            self.age[i] = age.max(1);
            self.lifespan[i] = 26 + (self.rand_immigration() * 13.0) as u8;
            self.birth_envelope[i] = 0.5;
        }
        true
    }

    fn depart(&mut self, source: usize) -> bool {
        let c = source % self.cols;
        if self.rows >= 6 && self.cols >= 14 {
            const LWSS_EAST: &[(isize, isize)] = &[
                (0, 5),
                (0, 2),
                (1, 6),
                (2, 6),
                (2, 2),
                (3, 6),
                (3, 5),
                (3, 4),
                (3, 3),
            ];
            let cols = if c < self.cols / 2 {
                [c.saturating_sub(3), 0, self.cols.saturating_sub(9)]
            } else {
                [c.saturating_sub(6), self.cols.saturating_sub(9), 0]
            };
            for col in cols {
                for row in 0..=self.rows.saturating_sub(5) {
                    if self.template_clear((row, col), LWSS_EAST, 9, 5)
                        && rollout_clear(LWSS_EAST, 9, 5, 0, 2, 4)
                        && rollout_candidate_clear(
                            &self.live,
                            self.rows,
                            self.cols,
                            (row, col),
                            LWSS_EAST,
                            (0, 2),
                            4,
                        )
                        && self.seed_cells((row, col), LWSS_EAST, 1)
                    {
                        return true;
                    }
                }
            }
        }
        const GLIDER: &[(isize, isize)] = &[(0, 1), (1, 2), (2, 0), (2, 1), (2, 2)];
        if self.rows >= 5 && self.cols >= 5 {
            for row in 0..=self.rows - 5 {
                for col in [c.saturating_sub(1), self.cols - 5, 0] {
                    if self.template_clear((row, col), GLIDER, 5, 5)
                        && rollout_clear(GLIDER, 5, 5, 1, 1, 4)
                        && rollout_candidate_clear(
                            &self.live,
                            self.rows,
                            self.cols,
                            (row, col),
                            GLIDER,
                            (1, 1),
                            4,
                        )
                        && self.seed_cells((row, col), GLIDER, 1)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn template_clear(
        &self,
        o: (usize, usize),
        motif: &[(isize, isize)],
        width: usize,
        height: usize,
    ) -> bool {
        if o.0 + height > self.rows || o.1 + width > self.cols {
            return false;
        }
        for r in o.0..o.0 + height {
            for c in o.1..o.1 + width {
                let i = r * self.cols + c;
                if !self.visible[i] || self.live[i] {
                    return false;
                }
            }
        }
        motif.iter().all(|&(dr, dc)| {
            let i = (o.0 as isize + dr) as usize * self.cols + (o.1 as isize + dc) as usize;
            !self.live[i]
        })
    }

    fn launch_from(&mut self, source: usize) -> bool {
        let (r, c) = (source / self.cols, source % self.cols);
        const GLIDER: &[(isize, isize)] = &[(0, 1), (1, 2), (2, 0), (2, 1), (2, 2)];
        for (dr, dc) in [(0isize, 3isize), (0, -3), (3, 0), (-3, 0)] {
            if self.rows < 5 || self.cols < 5 {
                continue;
            }
            let rr = (r as isize + dr).clamp(0, (self.rows - 5) as isize) as usize;
            let cc = (c as isize + dc).clamp(0, (self.cols - 5) as isize) as usize;
            if self.template_clear((rr, cc), GLIDER, 5, 5)
                && rollout_clear(GLIDER, 5, 5, 1, 1, 4)
                && rollout_candidate_clear(
                    &self.live,
                    self.rows,
                    self.cols,
                    (rr, cc),
                    GLIDER,
                    (1, 1),
                    4,
                )
                && self.seed_cells((rr, cc), GLIDER, 1)
            {
                return true;
            }
        }
        false
    }

    fn prune(&mut self, center: usize) {
        let r = center / self.cols;
        let c = center % self.cols;
        let mut nearby = [usize::MAX; 5];
        let mut len = 0;
        for (dr, dc) in [(0isize, 0isize), (-1, 0), (1, 0), (0, -1), (0, 1)] {
            let nr = r as isize + dr;
            let nc = c as isize + dc;
            if nr >= 0 && nc >= 0 && nr < self.rows as isize && nc < self.cols as isize {
                let i = nr as usize * self.cols + nc as usize;
                if self.live[i] {
                    nearby[len] = i;
                    len += 1;
                }
            }
        }
        nearby[..len].sort_by_key(|&i| (std::cmp::Reverse(self.age[i]), i));
        for &i in nearby[..len].iter().take(2) {
            self.kill_index(i, 0.18);
        }
    }

    fn start_squall(&mut self, center: usize) {
        if self.scene.is_some() || self.live.is_empty() {
            return;
        }
        let from = ((center % self.cols) as f32, (center / self.cols) as f32);
        let to = (
            self.cols.saturating_sub(1) as f32,
            self.rows.saturating_sub(1) as f32,
        );
        let visible_live = (0..self.live.len())
            .filter(|&i| self.visible[i] && self.live[i])
            .count();
        let mortality = (visible_live * 17 / 100).max(1);
        let footprint_left = (self.visible.iter().filter(|v| **v).count() * 23 / 100).max(1);
        self.scene = Some(Scene {
            kind: SceneKind::Squall,
            from,
            to,
            age: 0.0,
            duration: 2.5,
            admission: self.visible.clone(),
            visited: vec![false; self.live.len()],
            mortality_left: mortality,
            footprint_left,
        });
        self.squall_at = self.sim_t;
        self.squall_charge = 0.0;
    }

    fn advance_scene(&mut self, dt: f32) {
        let Some(mut scene) = self.scene.take() else {
            return;
        };
        scene.age += f64::from(dt);
        let u = (scene.age / scene.duration).clamp(0.0, 1.0) as f32;
        let x = scene.from.0 + (scene.to.0 - scene.from.0) * u;
        let y = scene.from.1 + (scene.to.1 - scene.from.1) * u;
        let radius = 1.4;
        for i in 0..self.live.len() {
            if scene.visited[i] || !scene.admission[i] || scene.footprint_left == 0 {
                continue;
            }
            let r = i / self.cols;
            let c = i % self.cols;
            if ((c as f32 - x).powi(2) + (r as f32 - y).powi(2)).sqrt() > radius {
                continue;
            }
            scene.visited[i] = true;
            scene.footprint_left -= 1;
            self.ghost[i] = self.ghost[i].max(if scene.kind == SceneKind::Star {
                0.14
            } else {
                0.24
            });
            self.nutrient[i] = (self.nutrient[i] + 0.18).min(1.0);
            if scene.mortality_left > 0 && self.live[i] && self.age[i] > 18 {
                self.kill_index(i, 0.2);
                scene.mortality_left -= 1;
            }
        }
        if scene.age < scene.duration {
            self.scene = Some(scene);
        } else if scene.kind == SceneKind::Star {
            let mut origins = Vec::new();
            let mut considered = vec![false; self.live.len()];
            for i in 0..self.live.len() {
                if !scene.visited[i] || !scene.admission[i] || !self.visible[i] {
                    continue;
                }
                let r = i / self.cols;
                let c = i % self.cols;
                for dr in [-1isize, 0] {
                    for dc in [-1isize, 0] {
                        let rr = r as isize + dr;
                        let cc = c as isize + dc;
                        if rr < 0
                            || cc < 0
                            || rr + 1 >= self.rows as isize
                            || cc + 1 >= self.cols as isize
                        {
                            continue;
                        }
                        let origin = rr as usize * self.cols + cc as usize;
                        if considered[origin] {
                            continue;
                        }
                        considered[origin] = true;
                        let clear = (0..2).all(|dy| {
                            (0..2).all(|dx| {
                                let cell = (rr as usize + dy) * self.cols + cc as usize + dx;
                                self.visible[cell] && !self.live[cell]
                            })
                        });
                        let mut fertility = 0.0;
                        for dy in 0..2 {
                            for dx in 0..2 {
                                fertility += self.nutrient
                                    [(rr as usize + dy) * self.cols + cc as usize + dx];
                            }
                        }
                        let fertile = fertility >= 0.14;
                        if clear && fertile {
                            origins.push(origin);
                        }
                    }
                }
            }
            if !origins.is_empty() {
                self.pending_star = Some(PendingStar {
                    due: self.sim_t + 2.0 + self.rand_weather() as f64 * 2.0,
                    origins,
                });
            }
        } else if scene.kind == SceneKind::Squall {
            let c = scene
                .from
                .0
                .round()
                .clamp(0.0, self.cols.saturating_sub(1) as f32) as usize;
            let r = scene
                .from
                .1
                .round()
                .clamp(0.0, self.rows.saturating_sub(1) as f32) as usize;
            self.launch_from(r * self.cols + c);
        }
    }

    fn winter_seed(&mut self) {
        if self.rows < 3 || self.cols < 3 {
            return;
        }
        let eligible: Vec<_> = (0..self.live.len())
            .filter(|&i| self.visible[i] && !self.live[i] && self.neighbors(i) == 0)
            .collect();
        if let Some(&i) = eligible.get((self.rand_immigration() * eligible.len() as f32) as usize) {
            self.live[i] = true;
            self.age[i] = 1;
            self.lifespan[i] = 2;
            self.birth_envelope[i] = 0.25;
        }
    }

    fn governor(&mut self, dt: f64) {
        if self.last_key_at.is_some_and(|at| self.sim_t - at < 3.0) {
            self.quiet_for = 0.0;
        } else {
            self.quiet_for += dt;
        }
        let visible_count = self.visible.iter().filter(|x| **x).count();
        let visible_live = (0..self.live.len())
            .filter(|&i| self.live[i] && self.visible[i])
            .count();
        let ghost_energy = (0..self.ghost.len())
            .filter(|&i| self.visible[i])
            .map(|i| self.ghost[i])
            .sum::<f32>();
        let occupancy = if visible_count == 0 {
            0.0
        } else {
            visible_live as f32 / visible_count as f32
        };
        let ghost_density = if visible_count == 0 {
            0.0
        } else {
            ghost_energy / visible_count as f32
        };
        self.occupancy_ema += (occupancy - self.occupancy_ema) * (dt as f32 / 12.0).clamp(0.0, 1.0);
        self.ghost_ema += (ghost_density - self.ghost_ema) * (dt as f32 / 8.0).clamp(0.0, 1.0);
        let no_recent_input = !self.last_key_at.is_some_and(|at| self.sim_t - at < 3.0);
        let sparse = self.occupancy_ema < 0.06 && visible_count >= 12;
        let star_ready = self.quiet_for > 45.0 && self.sim_t >= self.star_due && self.scene.is_none();
        let eligible = no_recent_input && self.scene.is_none()
            && self.pending_star.is_none() && !star_ready;
        if visible_live == 0 && visible_count >= 4 && eligible {
            self.empty_for += dt;
        } else {
            self.empty_for = 0.0;
        }
        let recovering = self.empty_for >= 1.25;
        // Extinction bypasses sparse immigration's probability and cooldown, after a quiet pause.
        if eligible && (recovering || (sparse && self.sim_t >= self.recovery_at)) {
            self.recovery_at = self.sim_t
                + (6.0 + self.rand_immigration() as f64 * 6.0) / f64::from(self.speed);
            if self.seed_local_motif(recovering) || recovering {
                self.empty_for = 0.0;
            }
        }
        if star_ready {
            if let Some(i) = self.visible.iter().position(|v| *v) {
                let y = (i / self.cols) as f32;
                self.scene = Some(Scene {
                    kind: SceneKind::Star,
                    from: (0.0, y),
                    to: (self.cols.saturating_sub(1) as f32, y),
                    age: 0.0,
                    duration: 1.5,
                    admission: self.visible.clone(),
                    visited: vec![false; self.live.len()],
                    mortality_left: 0,
                    footprint_left: self.visible.iter().filter(|v| **v).count(),
                });
                self.star_due = self.sim_t + 120.0 + self.rand_weather() as f64 * 180.0;
            }
        }
        if let Some(wake) = self.pending_star.take() {
            if self.sim_t >= wake.due && self.rows >= 3 && self.cols >= 3 {
                const BLOCK: &[(isize, isize)] = &[(0, 0), (0, 1), (1, 0), (1, 1)];
                let start = (self.rand_immigration() * wake.origins.len() as f32) as usize;
                for offset in 0..wake.origins.len() {
                    let i = wake.origins[(start + offset) % wake.origins.len()];
                    if self.seed_cells((i / self.cols, i % self.cols), BLOCK, 1) {
                        self.feed(i, 0.22);
                        self.ghost[i] = self.ghost[i].max(0.35);
                        break;
                    }
                }
            } else {
                self.pending_star = Some(wake);
            }
        }
        let congested =
            self.occupancy_ema > 0.48 && (self.ghost_ema > 0.08 || self.occupancy_ema > 0.58);
        if self.sim_t - self.renewal_at > 60.0
            && congested
            && self.quiet_for > 8.0
            && self.scene.is_none()
            && self.pending_star.is_none()
        {
            self.start_renewal(visible_live);
        }
    }

    fn seed_local_motif(&mut self, recovering: bool) -> bool {
        if self.rows < 3 || self.cols < 3 {
            return false;
        }
        let seasons = season_weights(self.sim_t);
        let pressure = establishment_pressure(self.density, seasons);
        if !recovering && self.rand_immigration() > pressure {
            return false;
        }
        let mut candidates = Vec::new();
        for r in 0..self.rows {
            for c in 0..self.cols {
                for (motif_index, motif) in LOCAL_MOTIFS.iter().enumerate() {
                    let mut fertile = 0.0;
                    let admitted = motif.orbit.iter().all(|&(dr, dc)| {
                        let rr = r as isize + dr;
                        let cc = c as isize + dc;
                        if rr < 0 || cc < 0 || rr >= self.rows as isize || cc >= self.cols as isize
                        {
                            return false;
                        }
                        let i = rr as usize * self.cols + cc as usize;
                        fertile += self.nutrient[i];
                        self.visible[i] && !self.live[i]
                    });
                    if !admitted {
                        continue;
                    }
                    let separated = motif.orbit.iter().all(|&(dr, dc)| {
                        let rr = r as isize + dr;
                        let cc = c as isize + dc;
                        (-1..=1).all(|dy| {
                            (-1..=1).all(|dx| {
                                let nr = rr + dy;
                                let nc = cc + dx;
                                nr < 0
                                    || nc < 0
                                    || nr >= self.rows as isize
                                    || nc >= self.cols as isize
                                    || !self.live[nr as usize * self.cols + nc as usize]
                            })
                        })
                    });
                    if separated {
                        candidates.push((r, c, motif_index, fertile));
                    }
                }
            }
        }
        if candidates.is_empty() {
            return false;
        }
        let prefer_fertile = candidates.iter().any(|candidate| candidate.3 >= 0.18);
        let mut viable = [false; LOCAL_MOTIFS.len()];
        for candidate in &candidates {
            if !prefer_fertile || candidate.3 >= 0.18 {
                viable[candidate.2] = true;
            }
        }
        // Choose the species before its site so small footprints do not crowd out larger motifs.
        let motif_choice =
            (self.rand_immigration() * viable.iter().filter(|x| **x).count() as f32) as usize;
        let motif_index = viable.iter().enumerate().filter(|(_, v)| **v)
            .nth(motif_choice).map(|(i, _)| i).expect("at least one viable motif");
        let mut sites = candidates.iter().filter(|candidate| {
            candidate.2 == motif_index && (!prefer_fertile || candidate.3 >= 0.18)
        });
        let site_choice = (self.rand_immigration() * sites.clone().count() as f32) as usize;
        let &(r, c, motif_index, _) = sites.nth(site_choice)
            .expect("viable motif has an admitted site");
        self.seed_cells((r, c), LOCAL_MOTIFS[motif_index].cells, 1)
    }

    fn record_recurrence(&mut self) {
        if self.sim_t - self.recurrence_at < 8.0 {
            return;
        }
        self.recurrence_at = self.sim_t;
        let bits = (0..self.live.len())
            .step_by(64)
            .map(|base| {
                (0..64).fold(0u64, |bits, bit| {
                    if base + bit < self.live.len() && self.live[base + bit] {
                        bits | (1u64 << bit)
                    } else {
                        bits
                    }
                })
            })
            .collect::<Vec<_>>();
        let visible_live = (0..self.live.len())
            .filter(|&i| self.visible[i] && self.live[i]).count();
        if visible_live > 0
            && self.quiet_for > 8.0
            && self.scene.is_none()
            && self.pending_star.is_none()
            && self.recurrence.iter().any(|old| old == &bits)
            && self.sim_t - self.recovery_at > 8.0
        {
            self.seed_local_motif(false);
            self.recovery_at = self.sim_t
                + (6.0 + self.rand_immigration() as f64 * 6.0) / f64::from(self.speed);
            if self.sim_t - self.renewal_at > 60.0 {
                self.start_renewal(visible_live);
            }
        }
        self.recurrence.push_back(bits);
        while self.recurrence.len() > 8 {
            self.recurrence.pop_front();
        }
    }

    fn start_renewal(&mut self, visible_live: usize) {
        if self.live.is_empty() || self.scene.is_some() {
            return;
        }
        self.scene = Some(Scene {
            kind: SceneKind::Renewal,
            from: (0.0, 0.0),
            to: (
                self.cols.saturating_sub(1) as f32,
                self.rows.saturating_sub(1) as f32,
            ),
            age: 0.0,
            duration: 4.0,
            admission: self.visible.clone(),
            visited: vec![false; self.live.len()],
            mortality_left: (visible_live / 10).max(1),
            footprint_left: self.visible.iter().filter(|v| **v).count(),
        });
        self.renewal_at = self.sim_t;
    }

    fn render_cells(&self, t: f64) -> Vec<Cell> {
        let seasons = season_weights(self.sim_t);
        let mut out = Vec::with_capacity(self.live.len());
        for i in 0..self.live.len() {
            if !self.visible[i] {
                out.push(Cell::default());
                continue;
            }
            let warm = self.sun(i);
            let n = self.nutrient[i].clamp(0.0, 1.0);
            // Habitat light follows the existing sunlight and soil without keeping cells alive.
            let habitat = 0.065 + 0.020 * warm + 0.025 * n;
            let contact = self.contact[i];
            let alive = self.live[i];
            let ghost = self.ghost[i]
                .max(self.death_envelope[i] * 0.5)
                .clamp(0.0, 1.0);
            let (mut u, mut intensity) = if contact > 0.045 {
                (0.94, (0.68 + contact * 0.32).min(1.0))
            } else if alive {
                let maturity = (self.age[i] as f32 / 30.0).clamp(0.0, 1.0);
                let emergence = 0.80 + 0.20 * (1.0 - self.birth_envelope[i]);
                (
                    0.48 + 0.38 * maturity,
                    ((0.58 + 0.22 * maturity + 0.10 * warm + 0.06 * n) * emergence)
                        .clamp(0.0, 0.90),
                )
            } else if ghost > 0.05 {
                (0.14, (ghost * 0.24).clamp(0.0, 0.22))
            } else {
                (0.08, habitat)
            };
            intensity = intensity.max(habitat);
            if contact <= 0.045 {
                if let Some(scene) = &self.scene {
                    if matches!(scene.kind, SceneKind::Star | SceneKind::Squall) {
                        let progress = (scene.age / scene.duration).clamp(0.0, 1.0) as f32;
                        let head_x = scene.from.0 + (scene.to.0 - scene.from.0) * progress;
                        let head_y = scene.from.1 + (scene.to.1 - scene.from.1) * progress;
                        let distance = ((i % self.cols) as f32 - head_x)
                            .hypot((i / self.cols) as f32 - head_y);
                        let head = (1.0 - distance / 1.4).clamp(0.0, 1.0) * 0.38;
                        if head > intensity {
                            u = 0.98;
                            intensity = head;
                        }
                    }
                }
            }
            let spring_phase = self.sim_t.rem_euclid(YEAR);
            let dawn = (1.0 - (spring_phase - 1.5).abs() / 1.5).clamp(0.0, 1.0) as f32 * seasons[0];
            let _ = t;
            out.push(Cell::new(
                u,
                (intensity + dawn * (0.04 + 0.06 * n)).clamp(0.0, 1.0),
            ));
        }
        out
    }

    fn sun(&self, i: usize) -> f32 {
        if self.rows == 0 || self.cols == 0 {
            return 0.0;
        }
        let x = (i % self.cols) as f32;
        let y = (i / self.cols) as f32;
        let cx = (self.cols as f32 * 0.5)
            + (self.biological_t as f32 * 0.11).sin() * self.cols as f32 * 0.25;
        let cy = (self.rows as f32 * 0.5)
            + (self.biological_t as f32 * 0.07).cos() * self.rows as f32 * 0.20;
        let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
        (1.0 - d / (self.rows.max(self.cols) as f32 * 0.75)).clamp(0.0, 1.0)
    }

    fn neighbors(&self, i: usize) -> usize {
        if self.cols == 0 {
            return 0;
        }
        let r = i / self.cols;
        let c = i % self.cols;
        let mut n = 0;
        for dr in -1isize..=1 {
            for dc in -1isize..=1 {
                if dr == 0 && dc == 0 {
                    continue;
                }
                let nr = r as isize + dr;
                let nc = c as isize + dc;
                if nr >= 0
                    && nc >= 0
                    && nr < self.rows as isize
                    && nc < self.cols as isize
                    && self.live[nr as usize * self.cols + nc as usize]
                {
                    n += 1
                }
            }
        }
        n
    }

    fn positive_rise(&self, t: f64) -> bool {
        let fast_count = self.recent.iter().filter(|(at, _)| t - *at <= 0.35).count();
        let fast = fast_count as f64 / 0.35;
        let slow = self.recent.iter().filter(|(at, _)| t - *at <= 1.8).count() as f64 / 1.8;
        fast_count >= 4 && fast > slow + 1.0
    }
    fn rand_weather(&mut self) -> f32 {
        random(&mut self.weather_rng)
    }
    fn rand_immigration(&mut self) -> f32 {
        random(&mut self.immigration_rng)
    }
}

fn season_weights(t: f64) -> [f32; 4] {
    let phase = t.rem_euclid(YEAR);
    let current = (phase / 16.0) as usize;
    let local = phase - current as f64 * 16.0;
    let mut weights = [0.0; 4];
    if local < 3.0 {
        let x = (local as f32 / 3.0).clamp(0.0, 1.0);
        let incoming = x * x * (3.0 - 2.0 * x);
        weights[(current + 3) % 4] = 1.0 - incoming;
        weights[current] = incoming;
    } else {
        weights[current] = 1.0;
    }
    weights
}

fn season_generation(weights: [f32; 4]) -> f64 {
    f64::from(weights[0] + weights[2]) + f64::from(weights[1]) * 1.25 + f64::from(weights[3]) * 0.65
}

fn establishment_pressure(density: f32, weights: [f32; 4]) -> f32 {
    let seasonal = weights[0] + weights[1] * 0.9 + weights[2] * 0.72 + weights[3] * 0.58;
    (0.42 + 0.15 * density).clamp(0.42, 0.90) * seasonal
}
fn random(state: &mut u32) -> f32 {
    let mut x = if *state == 0 { 0x9E37_79B9 } else { *state };
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

fn rollout_clear(
    motif: &[(isize, isize)],
    width: usize,
    height: usize,
    dr: isize,
    dc: isize,
    steps: usize,
) -> bool {
    let mut live = vec![false; width * height];
    for &(r, c) in motif {
        if r < 0 || c < 0 || r >= height as isize || c >= width as isize {
            return false;
        }
        live[r as usize * width + c as usize] = true;
    }
    for _ in 0..steps {
        let mut next = vec![false; live.len()];
        for r in 0..height {
            for c in 0..width {
                let mut n = 0;
                for dr in -1isize..=1 {
                    for dc in -1isize..=1 {
                        if dr == 0 && dc == 0 {
                            continue;
                        }
                        let rr = r as isize + dr;
                        let cc = c as isize + dc;
                        if rr >= 0
                            && cc >= 0
                            && rr < height as isize
                            && cc < width as isize
                            && live[rr as usize * width + cc as usize]
                        {
                            n += 1
                        }
                    }
                }
                let i = r * width + c;
                next[i] = n == 3 || (live[i] && n == 2);
            }
        }
        live = next;
    }
    let expected: std::collections::HashSet<_> =
        motif.iter().map(|&(r, c)| (r + dr, c + dc)).collect();
    let actual: std::collections::HashSet<_> = live
        .iter()
        .enumerate()
        .filter_map(|(i, v)| v.then_some(((i / width) as isize, (i % width) as isize)))
        .collect();
    actual == expected
}

fn rollout_candidate_clear(
    board: &[bool],
    rows: usize,
    cols: usize,
    origin: (usize, usize),
    motif: &[(isize, isize)],
    shift: (isize, isize),
    steps: usize,
) -> bool {
    let mut baseline = board.to_vec();
    let mut candidate = baseline.clone();
    for &(dr, dc) in motif {
        let r = origin.0 as isize + dr;
        let c = origin.1 as isize + dc;
        if r < 0 || c < 0 || r >= rows as isize || c >= cols as isize {
            return false;
        }
        candidate[r as usize * cols + c as usize] = true;
    }
    for _ in 0..steps {
        baseline = conway_step(&baseline, rows, cols);
        candidate = conway_step(&candidate, rows, cols);
    }
    let expected: std::collections::HashSet<_> = motif
        .iter()
        .map(|&(r, c)| {
            (
                (origin.0 as isize + r + shift.0) as usize,
                (origin.1 as isize + c + shift.1) as usize,
            )
        })
        .collect();
    let baseline_cells: std::collections::HashSet<_> = baseline
        .iter()
        .enumerate()
        .filter_map(|(i, v)| (*v).then_some((i / cols, i % cols)))
        .collect();
    let candidate_cells: std::collections::HashSet<_> = candidate
        .iter()
        .enumerate()
        .filter_map(|(i, v)| (*v).then_some((i / cols, i % cols)))
        .collect();
    let delta: std::collections::HashSet<_> = baseline_cells
        .symmetric_difference(&candidate_cells)
        .copied()
        .collect();
    delta == expected
}

fn conway_step(board: &[bool], rows: usize, cols: usize) -> Vec<bool> {
    let mut next = vec![false; board.len()];
    for r in 0..rows {
        for c in 0..cols {
            let mut n = 0;
            for dr in -1isize..=1 {
                for dc in -1isize..=1 {
                    if dr == 0 && dc == 0 {
                        continue;
                    }
                    let rr = r as isize + dr;
                    let cc = c as isize + dc;
                    if rr >= 0
                        && cc >= 0
                        && rr < rows as isize
                        && cc < cols as isize
                        && board[rr as usize * cols + cc as usize]
                    {
                        n += 1
                    }
                }
            }
            let i = r * cols + c;
            next[i] = n == 3 || (board[i] && n == 2);
        }
    }
    next
}

fn observation_cell(page: u16, usage: u16) -> Option<(usize, usize, ContactKind)> {
    if page == 0xFF00 && (0x20..=0x25).contains(&usage) {
        let name = crate::lighting::MACRO_KEY_NAMES[(usage - 0x20) as usize];
        let (row, col) = crate::lighting::razer_key_cell(name)?;
        return Some((row as usize, col as usize, ContactKind::Key));
    }
    if page != 0x07 {
        return None;
    }
    let kind = match usage {
        0x28 | 0x58 => ContactKind::Enter,
        0x2A => ContactKind::Backspace,
        _ => ContactKind::Key,
    };
    if usage == 0x58 {
        let (row, col) = crate::lighting::razer_key_cell("NUMENTER")?;
        return Some((row as usize, col as usize, kind));
    }
    let vk = crate::controls::control_to_vk(page, usage)?;
    let (row, col) = crate::lighting::vk_to_key_cell(vk)?;
    Some((row as usize, col as usize, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board(rows: usize, cols: usize) -> LifeSim {
        LifeSim::new(rows, cols, 42)
    }
    fn clear(s: &mut LifeSim) {
        s.live.fill(false);
        s.age.fill(0);
        s.next.fill(false);
        s.birth_envelope.fill(0.0);
        s.death_envelope.fill(0.0);
    }
    fn put(s: &mut LifeSim, coords: &[(usize, usize)]) {
        for &(r, c) in coords {
            let i = r * s.cols + c;
            s.live[i] = true;
            s.age[i] = 1;
            s.lifespan[i] = 200;
        }
    }
    fn step(s: &mut LifeSim) {
        s.generation();
    }

    #[test]
    fn canonical_block_blinker_glider_and_lwss_follow_b3_s23() {
        let mut block = board(10, 20);
        clear(&mut block);
        put(&mut block, &[(2, 2), (2, 3), (3, 2), (3, 3)]);
        let before = block.live.clone();
        step(&mut block);
        assert_eq!(block.live, before);
        let mut blinker = board(10, 20);
        clear(&mut blinker);
        put(&mut blinker, &[(4, 3), (4, 4), (4, 5)]);
        step(&mut blinker);
        assert_eq!(
            live_coords(&blinker),
            [(3, 4), (4, 4), (5, 4)].into_iter().collect()
        );
        let mut glider = board(10, 20);
        clear(&mut glider);
        put(&mut glider, &[(1, 2), (2, 3), (3, 1), (3, 2), (3, 3)]);
        for _ in 0..4 {
            step(&mut glider);
        }
        assert_eq!(
            live_coords(&glider),
            [(2, 3), (3, 4), (4, 2), (4, 3), (4, 4)]
                .into_iter()
                .collect()
        );
        let mut lwss = board(8, 24);
        clear(&mut lwss);
        put(
            &mut lwss,
            &[
                (2, 3),
                (2, 6),
                (3, 2),
                (4, 2),
                (4, 6),
                (5, 2),
                (5, 3),
                (5, 4),
                (5, 5),
            ],
        );
        for _ in 0..4 {
            step(&mut lwss);
        }
        assert_eq!(
            live_coords(&lwss),
            [
                (2, 1),
                (2, 4),
                (3, 0),
                (4, 0),
                (4, 4),
                (5, 0),
                (5, 1),
                (5, 2),
                (5, 3)
            ]
            .into_iter()
            .collect()
        );
        let mut singleton = board(8, 20);
        clear(&mut singleton);
        put(&mut singleton, &[(4, 8)]);
        step(&mut singleton);
        assert!(live_coords(&singleton).is_empty());
    }

    #[test]
    fn biological_seasons_match_the_spectrum_incoming_fade_and_wrap() {
        assert_eq!(season_weights(0.0), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(season_weights(13.0), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(season_weights(14.5), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(season_weights(16.0), [1.0, 0.0, 0.0, 0.0]);
        assert_eq!(season_weights(62.5), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(season_weights(64.0), [0.0, 0.0, 0.0, 1.0]);
        let spectrum = super::super::life_spectrum();
        let spring = spectrum.seq[0].palette.at(0.0, 0.5);
        let summer = spectrum.seq[1].palette.at(0.0, 0.5);
        let winter = spectrum.seq[3].palette.at(0.0, 0.5);
        assert_eq!(spectrum.at(13.0, 0.5), spring);
        assert_eq!(spectrum.at(14.5, 0.5), spring);
        assert_eq!(spectrum.at(16.0, 0.5), spring);
        assert_eq!(
            spectrum.at(17.5, 0.5),
            crate::lighting::Rgb::lerp(spring, summer, 0.5)
        );
        assert_eq!(spectrum.at(62.5, 0.5), winter);
        assert_eq!(spectrum.at(0.0, 0.5), winter);
        assert_eq!(spectrum.at(64.0, 0.5), spectrum.at(0.0, 0.5));
    }

    #[test]
    fn density_changes_establishment_pressure_without_clamping_occupancy() {
        let spring = season_weights(13.0);
        let winter = season_weights(62.5);
        let sparse = establishment_pressure(0.25, spring);
        let fertile = establishment_pressure(3.0, spring);
        assert!(fertile - sparse > 0.4);
        assert!(establishment_pressure(3.0, winter) < fertile);
        assert!(season_generation(season_weights(17.5)) > 1.0);
    }

    fn live_coords(s: &LifeSim) -> std::collections::HashSet<(usize, usize)> {
        s.live
            .iter()
            .enumerate()
            .filter_map(|(i, live)| live.then_some((i / s.cols, i % s.cols)))
            .collect()
    }

    #[test]
    fn identical_time_and_configure_preserve_state_and_cadence_matches() {
        let run = |fps: u32| {
            let mut s = board(8, 22);
            s.advance_to(0.0);
            let mut events = vec![
                (1.07, 2, 4, ContactKind::Key),
                (1.11, 2, 5, ContactKind::Key),
                (1.16, 3, 4, ContactKind::Key),
                (1.21, 3, 5, ContactKind::Enter),
                (4.0, 2, 2, ContactKind::Backspace),
            ];
            for i in 0..72 {
                events.push((6.01 + i as f64 / 18.0, 4, 4 + i % 14, ContactKind::Key));
            }
            let mut next = 0;
            for frame in 1..=fps * 16 {
                let now = frame as f64 / fps as f64;
                while next < events.len() && events[next].0 <= now {
                    let (at, r, c, kind) = events[next];
                    s.advance_to(at);
                    s.contact(r, c, kind, at);
                    next += 1;
                }
                s.advance_to(now);
            }
            s
        };
        let a = run(6);
        let b = run(15);
        let c = run(30);
        let d = run(60);
        for other in [&b, &c, &d] {
            assert_eq!(a.live, other.live);
            assert_eq!(a.age, other.age);
            assert_eq!(a.nutrient, other.nutrient);
            assert_eq!(a.ghost, other.ghost);
            assert_eq!(a.contact, other.contact);
            assert_eq!(a.scene, other.scene);
            assert_eq!(a.pending_star, other.pending_star);
            assert_eq!(a.recent, other.recent);
            assert_eq!(a.squall_at, other.squall_at);
            assert_eq!(a.sim_t, other.sim_t);
            assert!((a.tick_accum - other.tick_accum).abs() < 1e-8);
            assert_eq!(a.weather_rng, other.weather_rng);
            assert_eq!(a.immigration_rng, other.immigration_rng);
        }
        let first = a.clone();
        let mut same = a.clone();
        same.advance_to(same.last_t.unwrap_or(0.0));
        assert_eq!(same.live, first.live);
        assert_eq!(same.age, first.age);
        assert_eq!(same.nutrient, first.nutrient);

        let idle = |fps: u32| {
            let mut s = board(6, 22);
            s.advance_to(0.0);
            for frame in 1..=fps * 192 {
                s.advance_to(frame as f64 / f64::from(fps));
            }
            s
        };
        let baseline = idle(60);
        for fps in [6, 15, 30] {
            let mut other = idle(fps);
            assert!((other.tick_accum - baseline.tick_accum).abs() < 1e-8);
            other.tick_accum = baseline.tick_accum;
            assert_eq!(other, baseline, "idle ecology must not depend on render cadence");
        }
    }

    #[test]
    fn visible_mask_blocks_contact_and_tiny_geometry_stays_finite() {
        let mut s = board(5, 9);
        s.set_visible_region(&[0, 1, 9, 10], 5, 9);
        s.contact(4, 8, ContactKind::Key, 0.0);
        assert_eq!(s.contact[44], 0.0);
        s.advance_to(1.0);
        assert!(s
            .render_cells(1.0)
            .iter()
            .all(|c| c.u.is_finite() && c.intensity.is_finite()));
        let mut tiny = board(1, 22);
        clear(&mut tiny);
        for _ in 0..100 {
            tiny.generation();
        }
        assert!(tiny.live.iter().all(|x| !*x));
    }

    #[test]
    fn tiny_board_never_receives_autonomous_motifs_or_winter_seeds() {
        let mut tiny = board(2, 12);
        clear(&mut tiny);
        tiny.last_t = Some(0.0);
        for tick in 1..=3600 {
            tiny.advance_to(tick as f64 / 60.0);
        }
        assert!(tiny.live.iter().all(|x| !*x));
    }

    #[test]
    fn initial_motifs_are_admitted_only_as_whole_visible_templates() {
        let mut clipped = board(6, 22);
        clipped.set_visible_region(&[2 * 22 + 5, 2 * 22 + 6, 3 * 22 + 5], 6, 22);
        assert!(clipped.live.iter().all(|live| !*live));
        let mut complete = board(6, 22);
        complete.set_visible_region(&[2 * 22 + 5, 2 * 22 + 6, 3 * 22 + 5, 3 * 22 + 6], 6, 22);
        assert_eq!(complete.live.iter().filter(|live| **live).count(), 4);
    }

    #[test]
    fn local_establishment_varies_viable_catalog_motifs_across_seeds() {
        let mut counts = [0usize; LOCAL_MOTIFS.len()];
        let mut sampled = 0;
        for seed in 1..=64 {
            let mut s = LifeSim::new(12, 22, seed * 17);
            clear(&mut s);
            s.density = 3.0;
            if !s.seed_local_motif(false) {
                continue;
            }
            sampled += 1;
            let population = s.live.iter().filter(|alive| **alive).count();
            let catalog_index = match population {
                3 => 1,
                4 => 0,
                5 => 2,
                6 => 3,
                other => panic!("unexpected local motif population {other}"),
            };
            counts[catalog_index] += 1;
            let initial = s.live.clone();
            if catalog_index == 1 || catalog_index == 3 {
                s.generation();
                s.generation();
            } else {
                s.generation();
            }
            assert_eq!(s.live, initial, "local catalog motif must be viable");
        }
        assert!(
            sampled >= 32,
            "enough seeded trials should pass pressure: {sampled}"
        );
        assert!(
            counts.iter().all(|count| *count > 0),
            "catalog choice should vary: {counts:?}"
        );
    }

    #[test]
    fn thin_region_cannot_admit_only_one_phase_of_a_blinker() {
        let mut s = board(10, 22);
        s.set_visible_region(&[5 * 22 + 5, 5 * 22 + 6, 5 * 22 + 7], 10, 22);
        clear(&mut s);
        s.density = 3.0;
        assert!(!s.seed_local_motif(true));
        assert!(s.live.iter().all(|alive| !*alive));
    }

    #[test]
    fn local_catalog_orbits_cover_every_phase_without_extra_cells() {
        for motif in LOCAL_MOTIFS {
            let mut phase = vec![false; 25];
            for &(r, c) in motif.cells {
                phase[r as usize * 5 + c as usize] = true;
            }
            let first = phase.clone();
            let mut orbit = phase.clone();
            for _ in 0..2 {
                phase = conway_step(&phase, 5, 5);
                for (seen, live) in orbit.iter_mut().zip(&phase) {
                    *seen |= *live;
                }
            }
            assert_eq!(phase, first, "local species must settle or repeat within two generations");
            let expected = motif.orbit.iter().map(|&(r, c)| r as usize * 5 + c as usize)
                .collect::<std::collections::HashSet<_>>();
            let actual = orbit.iter().enumerate().filter_map(|(i, v)| v.then_some(i))
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn extinction_recovers_in_winter_despite_density_and_sparse_cooldown() {
        for seed in 1..=16 {
            let mut s = LifeSim::new(6, 22, seed);
            clear(&mut s);
            s.density = 0.25;
            s.recovery_at = 1000.0;
            for tick in 1..=74 {
                s.sim_t = 60.0 + tick as f64 * DT;
                s.governor(DT);
                assert!(s.live.iter().all(|live| !*live));
            }
            for tick in 75..=77 {
                s.sim_t = 60.0 + tick as f64 * DT;
                s.governor(DT);
            }
            assert!(s.live.iter().any(|live| *live), "winter recovery failed for seed {seed}");
            assert_eq!(s.empty_for, 0.0);
        }
        let mut typed = board(6, 22);
        clear(&mut typed);
        for tick in 1..=120 {
            typed.sim_t = tick as f64 * DT;
            typed.last_key_at = Some(typed.sim_t);
            typed.governor(DT);
        }
        assert!(typed.live.iter().all(|live| !*live));
        assert_eq!(typed.empty_for, 0.0);
    }

    #[test]
    fn repeat_interventions_wait_for_quiet_and_do_not_renew_an_empty_board() {
        for empty in [false, true] {
            let mut s = board(6, 22);
            if empty {
                clear(&mut s);
                s.quiet_for = 80.0;
            }
            s.sim_t = 64.0;
            s.record_recurrence();
            let before = s.live.clone();
            s.sim_t = 72.0;
            s.record_recurrence();
            assert!(s.scene.is_none());
            assert_eq!(s.live, before);
            assert_eq!(s.recovery_at, 0.0);
        }
        let mut wake = board(6, 22);
        wake.quiet_for = 80.0;
        wake.pending_star = Some(PendingStar { due: 74.0, origins: vec![0] });
        wake.sim_t = 64.0;
        wake.record_recurrence();
        wake.sim_t = 72.0;
        let before = wake.live.clone();
        wake.record_recurrence();
        assert!(wake.scene.is_none());
        assert_eq!(wake.live, before);
        assert_eq!(wake.recovery_at, 0.0);
    }

    #[test]
    fn a_ready_star_defers_extinction_recovery_on_its_first_tick() {
        let mut s = board(6, 22);
        clear(&mut s);
        s.sim_t = 160.0;
        s.quiet_for = 80.0;
        s.empty_for = 1.24;
        s.recovery_at = 1000.0;
        s.governor(DT);
        assert_eq!(s.scene.as_ref().map(|scene| scene.kind), Some(SceneKind::Star));
        assert!(s.live.iter().all(|live| !*live));
        assert_eq!(s.empty_for, 0.0);
        assert_eq!(s.recovery_at, 1000.0);
    }

    #[test]
    fn idle_clock_and_recovery_follow_speed_without_moving_the_seasons() {
        for speed in [0.25, 1.0, 4.0] {
            let mut s = board(6, 22);
            s.speed = speed;
            s.advance_to(0.0);
            for tick in 1..=600 {
                s.advance_to(tick as f64 * DT);
            }
            assert!((s.biological_t - 10.0 * f64::from(speed)).abs() < 1e-8);
            assert!((s.sim_t - 10.0).abs() < 1e-8);
            clear(&mut s);
            s.sim_t = 20.0;
            s.recovery_at = 0.0;
            s.occupancy_ema = 0.0;
            s.governor(DT);
            let delay = (s.recovery_at - s.sim_t) * f64::from(speed);
            assert!((6.0..=12.0).contains(&delay));
        }
    }

    #[test]
    fn idle_keyboard_trace_avoids_prolonged_extinction_across_speed_and_density() {
        let region = crate::lighting::razer_keyboard_keys().iter()
            .filter_map(|key| crate::lighting::razer_key_cell(key))
            .map(|(r, c)| u32::from(r) * 22 + u32::from(c)).collect::<Vec<_>>();
        for speed in [0.25, 1.0, 4.0] {
            for density in [0.25, 1.0, 3.0] {
                let mut s = LifeSim::new(6, 22, 0x4C49_4645 ^ (6 << 8) ^ 22);
                s.speed = speed;
                s.density = density;
                s.set_visible_region(&region, 6, 22);
                s.advance_to(0.0);
                let mut empty_for = 0.0;
                for frame in 1..=14_400 {
                    s.advance_to(frame as f64 * DT);
                    if s.live.iter().zip(&s.visible).any(|(live, visible)| *live && *visible) {
                        empty_for = 0.0;
                    } else {
                        empty_for += DT;
                    }
                    assert!(empty_for < 7.0, "idle extinction: speed={speed}, density={density}");
                }
            }
        }
    }

    #[test]
    fn sparse_establishment_can_add_one_separated_colony_but_respects_typing_and_cooldown() {
        let mut s = board(10, 22);
        clear(&mut s);
        s.density = 3.0;
        put(&mut s, &[(1, 1), (1, 2), (2, 1), (2, 2)]);
        s.occupancy_ema = 0.01;
        s.sim_t = 20.0;
        s.governor(1.0 / 60.0);
        let population = s.live.iter().filter(|alive| **alive).count();
        assert!(
            population > 4,
            "a sparse world can establish alongside a refuge"
        );
        assert!((26.0..=32.0).contains(&s.recovery_at));
        let deadline = s.recovery_at;
        for tick in 1..=300 {
            s.sim_t = 20.0 + tick as f64 / 60.0;
            s.governor(1.0 / 60.0);
        }
        assert_eq!(s.recovery_at, deadline);
        assert_eq!(s.live.iter().filter(|alive| **alive).count(), population);

        let mut typed = board(10, 22);
        clear(&mut typed);
        typed.density = 3.0;
        put(&mut typed, &[(1, 1), (1, 2), (2, 1), (2, 2)]);
        typed.occupancy_ema = 0.01;
        typed.sim_t = 20.0;
        typed.last_key_at = Some(19.0);
        typed.governor(1.0 / 60.0);
        assert_eq!(typed.live.iter().filter(|alive| **alive).count(), 4);
        assert_eq!(typed.recovery_at, 0.0);
    }

    #[test]
    fn enter_admits_a_full_lwss_runway_then_uses_a_glider_on_smaller_boards() {
        let mut large = board(6, 22);
        clear(&mut large);
        assert!(large.depart(0));
        assert_eq!(large.live.iter().filter(|live| **live).count(), 9);

        let mut smaller = board(5, 10);
        clear(&mut smaller);
        assert!(smaller.depart(0));
        assert_eq!(smaller.live.iter().filter(|live| **live).count(), 5);

        let mut narrow = board(6, 22);
        let region = (0..6)
            .flat_map(|r| (0..4).map(move |c| (r * 22 + c) as u32))
            .collect::<Vec<_>>();
        narrow.set_visible_region(&region, 6, 22);
        clear(&mut narrow);
        narrow.contact(0, 0, ContactKind::Enter, 0.0);
        assert_eq!(narrow.live.iter().filter(|live| **live).count(), 0);
        assert!(narrow.ghost.iter().any(|ghost| *ghost > 0.0));
    }

    #[test]
    fn burst_is_a_gust_while_sustained_rate_starts_one_bounded_squall() {
        let mut quick = board(10, 22);
        quick.advance_to(0.0);
        for i in 0..4 {
            let at = 0.01 + i as f64 * 0.09;
            quick.advance_to(at);
            quick.contact(4, 4 + i, ContactKind::Key, at);
        }
        assert!(quick.gust_at >= 0.0);
        let mut gradual = board(10, 22);
        gradual.advance_to(0.0);
        for i in 0..4 {
            let at = 0.01 + i as f64 * 0.25;
            gradual.advance_to(at);
            gradual.contact(4, 4 + i, ContactKind::Key, at);
        }
        assert!(gradual.gust_at < 0.0);

        let mut burst = board(10, 22);
        burst.advance_to(0.0);
        for i in 0..18 {
            let at = 0.01 + i as f64 * 0.02;
            burst.advance_to(at);
            burst.contact(4, 4 + i % 8, ContactKind::Key, at);
        }
        burst.advance_to(3.2);
        assert!(burst.scene.is_none());

        let mut sustained = board(10, 22);
        sustained.advance_to(0.0);
        for i in 0..72 {
            let at = 0.01 + i as f64 / 18.0;
            sustained.advance_to(at);
            sustained.contact(4, 4 + i % 8, ContactKind::Key, at);
        }
        sustained.advance_to(4.8);
        assert_eq!(
            sustained.scene.as_ref().map(|s| s.kind),
            Some(SceneKind::Squall)
        );
        let started = sustained.squall_at;
        for i in 0..18 {
            let at = 5.0 + i as f64 / 18.0;
            sustained.advance_to(at);
            sustained.contact(4, 4 + i % 8, ContactKind::Key, at);
        }
        sustained.advance_to(8.0);
        assert!(sustained.scene.is_none());
        assert_eq!(sustained.squall_at, started);
    }

    #[test]
    fn squall_budgets_use_visible_population_and_visible_area() {
        let mut s = board(8, 16);
        let region = (0..5)
            .flat_map(|r| (0..10).map(move |c| (r * 16 + c) as u32))
            .collect::<Vec<_>>();
        s.set_visible_region(&region, 8, 16);
        clear(&mut s);
        put(
            &mut s,
            &[
                (1, 1),
                (1, 2),
                (1, 3),
                (2, 2),
                (3, 4),
                (3, 5),
                (3, 6),
                (4, 7),
                (5, 12),
                (6, 13),
            ],
        );
        let visible_live = s
            .live
            .iter()
            .enumerate()
            .filter(|(i, live)| **live && s.visible[*i])
            .count();
        let visible_area = s.visible.iter().filter(|v| **v).count();
        s.start_squall(s.cols + 1);
        let scene = s.scene.as_ref().expect("squall starts");
        assert_eq!(scene.mortality_left, (visible_live * 17 / 100).max(1));
        assert_eq!(scene.footprint_left, (visible_area * 23 / 100).max(1));
        assert_eq!(scene.admission, s.visible);
    }

    #[test]
    fn long_pause_rebases_and_expires_transients_without_sampling_new_events() {
        let mut s = board(6, 22);
        s.advance_to(0.0);
        s.contact(2, 4, ContactKind::Key, 0.0);
        s.pending_star = Some(PendingStar {
            due: 1.0,
            origins: vec![10],
        });
        s.scene = Some(Scene {
            kind: SceneKind::Star,
            from: (0.0, 0.0),
            to: (21.0, 0.0),
            age: 0.2,
            duration: 1.5,
            admission: s.visible.clone(),
            visited: vec![false; s.live.len()],
            mortality_left: 0,
            footprint_left: s.live.len(),
        });
        s.nutrient[10] = 0.7;
        let rng = (s.weather_rng, s.immigration_rng);
        let live = s.live.clone();
        s.advance_to(1000.0);
        assert_eq!((s.weather_rng, s.immigration_rng), rng);
        assert_eq!(s.live, live);
        assert!(s.scene.is_none() && s.pending_star.is_none());
        assert!(s
            .contact
            .iter()
            .chain(&s.ghost)
            .chain(&s.nutrient)
            .all(|v| *v == 0.0));
        s.advance_to(1000.0);
        assert_eq!((s.weather_rng, s.immigration_rng), rng);
    }

    #[test]
    fn delayed_star_wake_seeds_only_inside_its_swept_visible_footprint() {
        let mut s = board(6, 12);
        let region = (1..4)
            .flat_map(|r| (2..10).map(move |c| (r * 12 + c) as u32))
            .collect::<Vec<_>>();
        s.set_visible_region(&region, 6, 12);
        clear(&mut s);
        s.recovery_at = 100.0;
        s.advance_to(0.0);
        s.scene = Some(Scene {
            kind: SceneKind::Star,
            from: (0.0, 2.0),
            to: (11.0, 2.0),
            age: 0.0,
            duration: 1.5,
            admission: s.visible.clone(),
            visited: vec![false; s.live.len()],
            mortality_left: 0,
            footprint_left: s.visible.iter().filter(|v| **v).count(),
        });
        let mut completed_at = None;
        for frame in 1..=120 {
            s.advance_to(frame as f64 / 60.0);
            if s.pending_star.is_some() {
                completed_at = Some(s.sim_t);
                break;
            }
        }
        let completed_at = completed_at.expect("completed swept star schedules one wake");
        let wake = s
            .pending_star
            .as_ref()
            .expect("one delayed wake is pending");
        assert!((2.0..=4.0).contains(&(wake.due - completed_at)));
        for &origin in &wake.origins {
            let r = origin / s.cols;
            let c = origin % s.cols;
            assert!((0..2).all(|dr| (0..2).all(|dc| s.visible[(r + dr) * s.cols + c + dc])));
        }
        assert!(!wake.origins.is_empty());
        let due = wake.due;
        while s.last_t.unwrap_or(0.0) < due + 0.1 {
            let next = (s.last_t.unwrap_or(0.0) + 0.5).min(due + 0.1);
            s.advance_to(next);
        }
        assert!(s.pending_star.is_none());
        assert_eq!(s.live.iter().filter(|v| **v).count(), 4);
        assert!(s
            .live
            .iter()
            .enumerate()
            .all(|(i, live)| !*live || s.visible[i]));
    }

    #[test]
    fn swept_star_head_is_visible_without_outranking_a_key_contact() {
        let mut life = Life::default();
        let _ = life.field(1, 6, 0.0);
        life.sim.contact[2] = 1.0;
        life.sim.scene = Some(Scene {
            kind: SceneKind::Star,
            from: (0.0, 0.0),
            to: (5.0, 0.0),
            age: 0.5,
            duration: 1.0,
            admission: vec![true; 6],
            visited: vec![false; 6],
            mortality_left: 0,
            footprint_left: 6,
        });
        let mut compositor = super::super::Compositor {
            layers: vec![super::super::Layer {
                pattern: Box::new(life),
                spectrum: super::super::life_spectrum(),
                palette_addressing: super::super::PaletteAddressing::Field,
                region: Vec::new(),
                blend: crate::effects::Blend::Normal,
                enabled: true,
            }],
        };
        let first = compositor.render(1, 6, 0.0);
        let repeated = compositor.render(1, 6, 0.0);
        assert_eq!(first, repeated);
        let low = first.iter().map(|c| c.scale_f(0.16)).collect::<Vec<_>>();
        let energy = |c: crate::lighting::Rgb| u16::from(c.r) + u16::from(c.g) + u16::from(c.b);
        assert!(energy(low[2]) > energy(low[3]));
        assert!(energy(low[3]) > energy(low[1]));
        assert!(energy(low[3]) > energy(low[0]));
        assert!(energy(low[0]) > 0);
    }

    #[test]
    fn long_seeded_activity_trace_stays_bounded_and_replays_exactly() {
        let run = || {
            let mut s = LifeSim::new(6, 22, 0x00C0_FFEE);
            s.set_visible_region(&(0..132).filter(|i| i % 22 < 18).collect::<Vec<_>>(), 6, 22);
            s.advance_to(0.0);
            let mut event = 0usize;
            for frame in 1..=36_000 {
                let now = frame as f64 / 60.0;
                if (100.0..104.0).contains(&now) && event < 72 {
                    let at = 100.01 + event as f64 / 18.0;
                    if at <= now {
                        s.advance_to(at);
                        s.contact(4, 3 + event % 12, ContactKind::Key, at);
                        event += 1;
                    }
                }
                if (220.0..222.0).contains(&now) && frame % 15 == 0 {
                    s.advance_to(now);
                    s.contact(4, 8, ContactKind::Enter, now);
                }
                s.advance_to(now);
                if frame % 600 == 0 {
                    assert!(s
                        .nutrient
                        .iter()
                        .chain(s.ghost.iter())
                        .chain(s.contact.iter())
                        .all(|x| x.is_finite() && (0.0..=1.0).contains(x)));
                    if let Some(scene) = &s.scene {
                        assert_eq!(scene.admission, s.visible);
                        assert!(scene.footprint_left <= s.visible.iter().filter(|v| **v).count());
                    }
                    assert!(s.recent.len() <= 64);
                }
            }
            s
        };
        let a = run();
        let b = run();
        assert_eq!(a.live, b.live);
        assert_eq!(a.age, b.age);
        assert_eq!(a.nutrient, b.nutrient);
        assert_eq!(a.ghost, b.ghost);
        assert_eq!(a.weather_rng, b.weather_rng);
        assert_eq!(a.immigration_rng, b.immigration_rng);
        if let Some(scene) = &a.scene {
            assert_eq!(scene.admission, a.visible);
            assert!(scene.footprint_left <= a.visible.iter().filter(|v| **v).count());
        }
        assert!(a.live.iter().filter(|x| **x).count() < a.live.len());
    }

    #[test]
    fn pre_geometry_region_survives_reset_and_configure_preserves_simulation() {
        let mut pattern = Life::default();
        pattern.set_visible_region(&[20, 21, 42, 43], 6, 22);
        pattern.configure(&Params::default());
        let _ = pattern.field(6, 22, 0.0);
        assert_eq!(pattern.sim.visible.iter().filter(|x| **x).count(), 4);
        assert!(pattern
            .sim
            .live
            .iter()
            .enumerate()
            .all(|(i, live)| !*live || pattern.sim.visible[i]));
        let before = pattern.sim.clone();
        pattern.configure(&Params::default());
        let _ = pattern.field(6, 22, 0.0);
        assert_eq!(pattern.sim.live, before.live);
        assert_eq!(pattern.sim.age, before.age);
        assert_eq!(pattern.sim.weather_rng, before.weather_rng);
    }

    #[test]
    fn observation_mapping_excludes_nonkeys_and_distinguishes_rituals() {
        assert_eq!(observation_cell(0x07, 0x28).unwrap().2, ContactKind::Enter);
        assert_eq!(observation_cell(0x07, 0x58).unwrap().2, ContactKind::Enter);
        assert_eq!(
            observation_cell(0x07, 0x2A).unwrap().2,
            ContactKind::Backspace
        );
        assert!(observation_cell(0x07, 0xE1).is_some());
        assert!(observation_cell(0x07, 0x4B).is_some());
        assert!(observation_cell(0x07, 0x59).is_some());
        assert!(observation_cell(0x07, 0x58).is_some_and(|(_, _, kind)| kind == ContactKind::Enter));
        assert!(observation_cell(0x07, 0x63).is_some());
        assert!(observation_cell(0x07, 0xFFFF).is_none());
        assert!(observation_cell(0x09, 1).is_none());
        assert!(observation_cell(0xFF00, 0x20).is_some());
    }

    #[test]
    fn empty_habitat_stays_visible_through_seasons_without_changing_ecology() {
        let mut s = board(6, 22);
        s.set_visible_region(&[0, 1, 22, 23], 6, 22);
        clear(&mut s);
        s.nutrient.fill(0.0);
        let spectrum = super::super::life_spectrum();
        for ghost in [0.0, 0.06] {
            s.ghost.fill(ghost);
            for quarter in 0..=256 {
                let t = quarter as f64 / 4.0;
                s.sim_t = t;
                s.biological_t = t;
                let before = s.clone();
                let cells = s.render_cells(t);
                assert_eq!(s, before, "ambient rendering must not feed the simulation");
                let pixels = Field::Scalar(cells.clone()).render(&spectrum, t as f32);
                for (i, pixel) in pixels.iter().enumerate() {
                    if s.visible[i] {
                        assert_ne!(pixel.scale_f(0.16), crate::lighting::Rgb::BLACK,
                            "empty habitat vanished at t={t}, cell={i}, ghost={ghost}");
                        assert!(cells[i].intensity < 0.23, "ambient must stay below living structure");
                    } else {
                        assert_eq!(*pixel, crate::lighting::Rgb::BLACK);
                    }
                }
            }
        }
    }

    #[test]
    fn actual_spectrum_bytes_keep_contact_live_and_ghost_separated() {
        struct Fixture(Vec<Cell>);
        impl Pattern for Fixture {
            fn field(&mut self, _rows: u8, _cols: u8, _t: f32) -> Field {
                Field::Scalar(self.0.clone())
            }
        }
        let cells = vec![
            Cell::new(0.0, 0.0),
            Cell::new(0.94, 1.0),
            Cell::new(0.78, 0.78),
            Cell::new(0.14, 0.20),
        ];
        let comp_layer = super::super::Layer {
            pattern: Box::new(Fixture(cells)),
            spectrum: super::super::life_spectrum(),
            palette_addressing: super::super::PaletteAddressing::Field,
            region: vec![1, 2, 3],
            blend: crate::effects::Blend::Normal,
            enabled: true,
        };
        let mut compositor = super::super::Compositor {
            layers: vec![comp_layer],
        };
        let px = compositor.render(1, 4, 0.0);
        let low = px.iter().map(|c| c.scale_f(0.16)).collect::<Vec<_>>();
        let energy = |c: crate::lighting::Rgb| u16::from(c.r) + u16::from(c.g) + u16::from(c.b);
        assert_eq!(low[0], crate::lighting::Rgb::BLACK);
        assert!(energy(low[1]) > energy(low[2]));
        assert!(energy(low[2]) > energy(low[3]));
        assert!(energy(low[3]) > 0);

        let mut life = Life::default();
        let _ = life.field(1, 5, 0.0);
        life.sim.nutrient[0] = 1.0;
        life.sim.contact[1] = 1.0;
        life.sim.live[2] = true;
        life.sim.age[2] = 15;
        life.sim.ghost[3] = 0.9;
        let mut wildlife = super::super::Compositor {
            layers: vec![super::super::Layer {
                pattern: Box::new(life),
                spectrum: super::super::life_spectrum(),
                palette_addressing: super::super::PaletteAddressing::Field,
                region: Vec::new(),
                blend: crate::effects::Blend::Normal,
                enabled: true,
            }],
        };
        let frame = wildlife.render(1, 5, 0.0);
        let low = frame.iter().map(|c| c.scale_f(0.16)).collect::<Vec<_>>();
        assert!(energy(low[1]) > energy(low[2]));
        assert!(energy(low[2]) > energy(low[3]));
        assert!(energy(low[3]) > energy(low[0]));
        assert!(energy(low[0]) > 0);
    }
}
