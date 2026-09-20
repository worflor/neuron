// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! The Engram brain: wells, dream buffer, measurement, and absorption.
//!
//! The brain is a constant-size knowledge base that stores:
//! - Wells: per-domain running means of eigenvalue centroids
//! - Dream buffer: circular history of recent K,G,S entries
//! - Reference pairing: shared coordinate system for all encodes
//!
//! No training loop. All thresholds derive from the data.

use num_complex::Complex64;
use std::collections::HashMap;

use crate::encode::encode;
use crate::fit::fit_all;
use crate::predict::{energy, to_complex};
use crate::segment::derive_pairing;
use crate::types::{Block, DRIFT_WEIGHT, MACHINE_EPS, MIN_BLOCK, Mode, Packet, TEXTURE_WEIGHT};

/// Minimum blocks before a well is considered "established" for threshold calibration.
const MIN_WELL_BLOCKS: usize = 10;

/// Dream buffer capacity per dimension (cap = max(500, dim * this)).
const DREAM_CAP_PER_DIM: usize = 4;

/// A single well: running sufficient statistics in eigenvalue space.
#[derive(Debug, Clone)]
pub struct Well {
    /// Running sum of K eigenvalue vectors (complex, length P).
    pub sum_k: Vec<Complex64>,
    /// Observation count (number of blocks absorbed).
    pub count: usize,
}

impl Well {
    /// Create an empty well for P pairs.
    #[must_use]
    pub fn new(p: usize) -> Self {
        Self {
            sum_k: vec![Complex64::ZERO; p],
            count: 0,
        }
    }

    /// The centroid: mean K across all absorbed blocks.
    #[must_use]
    pub fn centroid(&self) -> Vec<Complex64> {
        if self.count == 0 {
            return self.sum_k.clone();
        }
        let inv = 1.0 / self.count as f64;
        self.sum_k.iter().map(|&k| k * inv).collect()
    }

    /// Absorb a K vector into this well.
    pub fn absorb(&mut self, k: &[Complex64]) {
        for (s, &kv) in self.sum_k.iter_mut().zip(k.iter()) {
            *s += kv;
        }
        self.count += 1;
    }
}

/// A dream buffer entry: one article's K, G, and `pair_rms` means.
#[derive(Debug, Clone)]
pub struct DreamEntry {
    pub k: Vec<Complex64>,
    pub g: Vec<Complex64>,
    pub s: Vec<f32>, // pair_rms
}

/// The Engram brain.
#[derive(Debug, Clone)]
pub struct Brain {
    /// Embedding dimension.
    pub dim: usize,
    /// Number of oscillator pairs (dim / 2).
    pub pairs: usize,
    /// Projection temperature.
    pub alpha: f32,

    /// Identity metadata.
    pub name: String,

    /// Wells: name → sufficient statistics.
    pub wells: HashMap<String, Well>,

    /// Reference pairing for consistent coordinate system.
    pub reference_pairing: Option<Vec<i32>>,

    /// Dream buffer (circular, newest at end).
    pub dream: Vec<DreamEntry>,

    /// Total articles absorbed.
    pub total_absorbed: usize,

    /// Next auto-generated well ID.
    pub(crate) next_well_id: usize,

    // --- Caches (invalidated on absorb) ---
    centroid_cache: Option<HashMap<String, Vec<Complex64>>>,
    global_centroid_cache: Option<Vec<Complex64>>,
}

impl Brain {
    /// Create a new empty brain.
    #[must_use]
    pub fn new(dim: usize, alpha: f32) -> Self {
        Self {
            dim,
            pairs: dim / 2,
            alpha,
            name: String::new(),
            wells: HashMap::new(),
            reference_pairing: None,
            dream: Vec::new(),
            total_absorbed: 0,
            next_well_id: 0,
            centroid_cache: None,
            global_centroid_cache: None,
        }
    }

    /// Dream buffer capacity.
    #[must_use]
    pub fn dream_capacity(&self) -> usize {
        500_usize.max(self.dim * DREAM_CAP_PER_DIM)
    }

    /// Invalidate all caches (must be called after any absorb).
    fn invalidate_caches(&mut self) {
        self.centroid_cache = None;
        self.global_centroid_cache = None;
    }

    /// Get all well centroids (cached).
    pub fn well_centroids(&mut self) -> &HashMap<String, Vec<Complex64>> {
        self.centroid_cache.get_or_insert_with(|| {
            self.wells.iter().map(|(name, well)| (name.clone(), well.centroid())).collect()
        })
    }

    /// Global centroid: mean across all well centroids.
    pub fn global_centroid(&mut self) -> Option<Vec<Complex64>> {
        if self.global_centroid_cache.is_some() {
            return self.global_centroid_cache.clone();
        }

        let centroids = self.well_centroids().clone();
        if centroids.is_empty() {
            return None;
        }

        let p = self.pairs;
        let n = centroids.len() as f64;
        let mut gc = vec![Complex64::ZERO; p];
        for c in centroids.values() {
            for j in 0..p {
                gc[j] += c[j];
            }
        }
        for g in &mut gc {
            *g /= n;
        }

        self.global_centroid_cache = Some(gc.clone());
        Some(gc)
    }

    /// Find the nearest well to an observation centroid.
    /// Returns (`well_name`, `rms_distance`).
    pub fn nearest_well(&mut self, obs_centroid: &[Complex64]) -> (String, f64) {
        let centroids = self.well_centroids().clone();
        if centroids.is_empty() {
            return ("unknown".into(), 50.0);
        }

        let p = self.pairs;
        let mut best_name = String::new();
        let mut best_dist = f64::MAX;

        for (name, centroid) in &centroids {
            let mut sum_sq = 0.0;
            for j in 0..p {
                let diff = obs_centroid[j] - centroid[j];
                sum_sq += diff.norm_sqr();
            }
            let dist = (sum_sq / p as f64).sqrt();
            if dist < best_dist {
                best_dist = dist;
                best_name.clone_from(name);
            }
        }

        (best_name, best_dist)
    }

    /// Distance from an observation centroid to every established well.
    /// Returns `HashMap`<`well_name`, distance> for wells with count >= `MIN_WELL_BLOCKS`.
    pub fn well_profile(&mut self, obs_centroid: &[Complex64]) -> HashMap<String, f64> {
        let centroids = self.well_centroids().clone();
        let p = self.pairs;
        let mut profile = HashMap::new();

        for (name, centroid) in &centroids {
            if self
                .wells
                .get(name)
                .is_some_and(|w| w.count >= MIN_WELL_BLOCKS)
            {
                let mut sum_sq = 0.0;
                for j in 0..p {
                    let diff = obs_centroid[j] - centroid[j];
                    sum_sq += diff.norm_sqr();
                }
                profile.insert(name.clone(), (sum_sq / p as f64).sqrt());
            }
        }

        profile
    }

    /// Absorb a packet into the brain.
    ///
    /// Routes to the nearest well, or creates a new well if the observation
    /// is far enough from all established wells (2x median NN gap).
    pub fn absorb(&mut self, packet: &Packet, source: Option<&str>) -> String {
        let p = self.pairs;

        // Collect per-block K values
        let obs_ks: Vec<&Vec<Complex64>> = packet
            .blocks
            .iter()
            .filter(|b| b.mode == Mode::Cascaded || b.mode == Mode::Linear)
            .map(|b| &b.macro_k)
            .collect();

        if obs_ks.is_empty() {
            self.total_absorbed += 1;
            return "skipped".into();
        }

        // Observation centroid: mean of block K values
        let mut obs_centroid = vec![Complex64::ZERO; p];
        for k in &obs_ks {
            for j in 0..p {
                obs_centroid[j] += k[j];
            }
        }
        let inv = 1.0 / obs_ks.len() as f64;
        for c in &mut obs_centroid {
            *c *= inv;
        }

        // Route to well
        let well_name = if let Some(src) = source {
            src.to_string()
        } else {
            self.route_unsupervised(&obs_centroid)
        };

        // Absorb all block K values into the well
        let well = self.wells.entry(well_name.clone()).or_insert_with(|| Well::new(p));
        for k in &obs_ks {
            well.absorb(k);
        }

        // Dream buffer: add per-article mean K, G, S
        let obs_gs: Vec<&Vec<Complex64>> = packet
            .blocks
            .iter()
            .filter(|b| b.mode == Mode::Cascaded || b.mode == Mode::Linear)
            .map(|b| &b.macro_g)
            .collect();

        let mut mean_k = vec![Complex64::ZERO; p];
        let mut mean_g = vec![Complex64::ZERO; p];
        let mut mean_s = vec![0.0_f32; p];
        let n = obs_ks.len() as f64;

        for k in &obs_ks {
            for j in 0..p {
                mean_k[j] += k[j];
            }
        }
        for g in &obs_gs {
            for j in 0..p {
                mean_g[j] += g[j];
            }
        }
        for b in &packet.blocks {
            if let Some(ref rms) = b.pair_rms {
                for j in 0..p {
                    mean_s[j] += rms[j];
                }
            }
        }
        for j in 0..p {
            mean_k[j] /= n;
            mean_g[j] /= n;
            mean_s[j] /= n as f32;
        }

        self.dream.push(DreamEntry {
            k: mean_k,
            g: mean_g,
            s: mean_s,
        });

        // Trim dream buffer
        let cap = self.dream_capacity();
        if self.dream.len() > cap {
            let excess = self.dream.len() - cap;
            self.dream.drain(..excess);
        }

        self.total_absorbed += 1;
        self.invalidate_caches();

        well_name
    }

    /// Unsupervised well routing: nearest well or new well if 2x median NN gap.
    fn route_unsupervised(&mut self, obs_centroid: &[Complex64]) -> String {
        if self.wells.is_empty() {
            let name = format!("well_{}", self.next_well_id);
            self.next_well_id += 1;
            return name;
        }

        if self.wells.len() < 2 {
            let name = format!("well_{}", self.next_well_id);
            self.next_well_id += 1;
            return name;
        }

        let (nearest, dist) = self.nearest_well(obs_centroid);

        // Only established wells (>= MIN_WELL_BLOCKS) set the gap threshold
        let established: Vec<(String, Vec<Complex64>)> = {
            let centroids = self.well_centroids().clone();
            centroids
                .into_iter()
                .filter(|(name, _)| {
                    self.wells
                        .get(name)
                        .is_some_and(|w| w.count >= MIN_WELL_BLOCKS)
                })
                .collect()
        };

        if established.len() < 2 {
            return nearest;
        }

        // Median nearest-neighbor gap among established wells
        let p = self.pairs;
        let mut nn_dists = Vec::with_capacity(established.len());
        for (i, (_, ci)) in established.iter().enumerate() {
            let mut best = f64::MAX;
            for (j, (_, cj)) in established.iter().enumerate() {
                if i == j {
                    continue;
                }
                let mut sq = 0.0;
                for k in 0..p {
                    let d = ci[k] - cj[k];
                    sq += d.norm_sqr();
                }
                let d = (sq / p as f64).sqrt();
                if d < best {
                    best = d;
                }
            }
            nn_dists.push(best);
        }

        nn_dists.sort_by(f64::total_cmp);
        let typical_gap = if nn_dists.len() % 2 == 0 {
            f64::midpoint(nn_dists[nn_dists.len() / 2 - 1], nn_dists[nn_dists.len() / 2])
        } else {
            nn_dists[nn_dists.len() / 2]
        };

        if dist > typical_gap * 2.0 {
            let name = format!("well_{}", self.next_well_id);
            self.next_well_id += 1;
            name
        } else {
            nearest
        }
    }

    /// Fast absorb: fit K on entire trajectory, skip segmentation/micro/residuals.
    /// ~15x faster than full encode + absorb.
    pub fn fast_absorb(&mut self, trajectory: &[f32], t: usize, source: Option<&str>) -> String {
        let dim = self.dim;
        let p = self.pairs;

        if t < MIN_BLOCK || dim < 2 {
            self.total_absorbed += 1;
            return "skipped".into();
        }

        // Derive reference pairing on first trajectory
        let pairing = self.reference_pairing
            .get_or_insert_with(|| derive_pairing(trajectory, t, dim))
            .clone();

        let wp = crate::segment::apply_pairing(
            trajectory,
            t,
            dim,
            &pairing,
        );

        let z = to_complex(&wp, t, dim);
        let fit = fit_all(&z, t, p);

        // Build minimal single-block packet
        let blk = Block {
            mode: Mode::Cascaded,
            start: 0,
            length: t,
            macro_k: fit.k,
            macro_g: fit.g,
            micro_ks: Vec::new(),
            micro_gs: Vec::new(),
            residuals: Vec::new(),
            seed1: wp[dim..2 * dim].to_vec(),
            seed2: wp[..dim].to_vec(),
            scales: vec![0.0; p],
            pair_rms: None,
            signal_energy: energy(&wp),
            residual_energy: 0.0,
            macro_capture: 0.0,
            micro_capture: 0.0,
        };

        let pkt = Packet {
            dim,
            pairs: p,
            length: t,
            alpha: self.alpha,
            macro_block: t,
            micro_block: MIN_BLOCK,
            pairing,
            blocks: vec![blk],
        };

        self.absorb(&pkt, source)
    }

    /// Measure an observation against the brain's wells.
    ///
    /// Returns (drift, texture, combined, `nearest_well`, `well_distance`, capture).
    pub fn measure(&mut self, observation: &[f32], t: usize) -> MeasureResult {
        let dim = self.dim;
        let p = self.pairs;

        if t < MIN_BLOCK || dim < 2 {
            return MeasureResult::default();
        }

        // Derive reference pairing on first use
        if self.reference_pairing.is_none() {
            self.reference_pairing = Some(derive_pairing(observation, t, dim));
        }

        // Encode with reference pairing
        let packet = encode(observation, t, dim, self.reference_pairing.as_deref());

        let sig_energy = packet.total_energy();
        let _res_energy = packet.residual_energy();
        let capture = packet.capture();
        let texture = 100.0 - capture;

        // Observation K centroid
        let obs_ks: Vec<&Vec<Complex64>> = packet
            .blocks
            .iter()
            .filter(|b| b.mode == Mode::Cascaded || b.mode == Mode::Linear)
            .map(|b| &b.macro_k)
            .collect();

        if obs_ks.is_empty() {
            return MeasureResult {
                drift: 50.0,
                texture,
                combined: DRIFT_WEIGHT * 50.0 + TEXTURE_WEIGHT * texture,
                nearest_well: "unknown".into(),
                well_distance: 50.0,
                capture,
                energy: sig_energy,
                well_profile: HashMap::new(),
            };
        }

        let mut obs_centroid = vec![Complex64::ZERO; p];
        let inv = 1.0 / obs_ks.len() as f64;
        for k in &obs_ks {
            for j in 0..p {
                obs_centroid[j] += k[j];
            }
        }
        for c in &mut obs_centroid {
            *c *= inv;
        }

        let (nearest_well, well_distance) = self.nearest_well(&obs_centroid);
        let profile = self.well_profile(&obs_centroid);

        // Drift: normalized by global centroid scale
        let drift = if let Some(gc) = self.global_centroid() {
            let scale = (gc.iter().map(num_complex::Complex::norm_sqr).sum::<f64>() / p as f64)
                .sqrt()
                .max(MACHINE_EPS);
            (well_distance / scale * 100.0).min(100.0)
        } else {
            50.0
        };

        let combined = DRIFT_WEIGHT * drift + TEXTURE_WEIGHT * texture;

        MeasureResult {
            drift,
            texture,
            combined,
            nearest_well,
            well_distance,
            capture,
            energy: sig_energy,
            well_profile: profile,
        }
    }

    /// Number of established wells (count >= `MIN_WELL_BLOCKS`).
    #[must_use]
    pub fn established_well_count(&self) -> usize {
        self.wells
            .values()
            .filter(|w| w.count >= MIN_WELL_BLOCKS)
            .count()
    }
}

/// Measurement result.
#[derive(Debug, Clone)]
pub struct MeasureResult {
    /// Eigenvalue centroid distance from nearest well (0-100%).
    pub drift: f64,
    /// Residual energy (100 - capture%).
    pub texture: f64,
    /// Weighted combination: 0.7 * drift + 0.3 * texture.
    pub combined: f64,
    /// Name of nearest well.
    pub nearest_well: String,
    /// Raw RMS distance to nearest well.
    pub well_distance: f64,
    /// Signal capture percentage.
    pub capture: f64,
    /// Total signal energy.
    pub energy: f64,
    /// Distances to all established wells.
    pub well_profile: HashMap<String, f64>,
}

impl Default for MeasureResult {
    fn default() -> Self {
        Self {
            drift: 0.0,
            texture: 0.0,
            combined: 0.0,
            nearest_well: "unknown".into(),
            well_distance: 0.0,
            capture: 0.0,
            energy: 0.0,
            well_profile: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make_trajectory(t: usize, dim: usize, freq_scale: f64) -> Vec<f32> {
        (0..t * dim)
            .map(|i| {
                let row = i / dim;
                let col = i % dim;
                (freq_scale * row as f64 + col as f64 * 0.3).sin() as f32
            })
            .collect()
    }

    #[test]
    fn brain_create_empty() {
        let brain = Brain::new(300, 0.005);
        assert_eq!(brain.dim, 300);
        assert_eq!(brain.pairs, 150);
        assert!(brain.wells.is_empty());
        assert!(brain.dream.is_empty());
        assert_eq!(brain.dream_capacity(), 1200);
    }

    #[test]
    fn brain_absorb_creates_well() {
        let mut brain = Brain::new(8, 0.005);
        let traj = make_trajectory(50, 8, 0.3);

        let well = brain.fast_absorb(&traj, 50, Some("test_domain"));
        assert_eq!(well, "test_domain");
        assert!(brain.wells.contains_key("test_domain"));
        assert!(brain.wells["test_domain"].count > 0);
        assert_eq!(brain.total_absorbed, 1);
    }

    #[test]
    fn brain_unsupervised_routing() {
        let mut brain = Brain::new(8, 0.005);

        // Feed two very different trajectories
        let traj_a = make_trajectory(50, 8, 0.1);
        let traj_b = make_trajectory(50, 8, 3.0);

        let well_a = brain.fast_absorb(&traj_a, 50, None);
        let well_b = brain.fast_absorb(&traj_b, 50, None);

        // Should create two different wells (auto-named)
        assert!(well_a.starts_with("well_"));
        assert!(well_b.starts_with("well_"));
        assert!(brain.wells.len() >= 2, "got {} wells", brain.wells.len());
    }

    #[test]
    fn brain_dream_buffer_trims() {
        let mut brain = Brain::new(8, 0.005);
        let cap = brain.dream_capacity();

        for i in 0..(cap + 10) {
            let traj = make_trajectory(50, 8, 0.1 + i as f64 * 0.001);
            brain.fast_absorb(&traj, 50, Some("domain"));
        }

        assert!(
            brain.dream.len() <= cap,
            "dream {} > cap {}",
            brain.dream.len(),
            cap
        );
    }

    #[test]
    fn brain_measure_returns_sane() {
        let mut brain = Brain::new(8, 0.005);

        // Absorb some data first
        for i in 0..20 {
            let traj = make_trajectory(50, 8, 0.3 + f64::from(i) * 0.01);
            brain.fast_absorb(&traj, 50, Some("music"));
        }

        // Measure a similar trajectory
        let obs = make_trajectory(50, 8, 0.35);
        let result = brain.measure(&obs, 50);

        assert!(result.drift >= 0.0);
        assert!(result.texture >= 0.0);
        assert!(result.capture >= 0.0);
        assert!(!result.nearest_well.is_empty());
    }

    #[test]
    fn well_centroid_is_mean() {
        let p = 4;
        let mut well = Well::new(p);

        let k1 = vec![Complex64::new(1.0, 0.0); p];
        let k2 = vec![Complex64::new(3.0, 0.0); p];

        well.absorb(&k1);
        well.absorb(&k2);

        let centroid = well.centroid();
        for c in &centroid {
            assert!((c.re - 2.0).abs() < 1e-10);
        }
    }

    #[test]
    fn nearest_well_finds_closest() {
        let mut brain = Brain::new(4, 0.005);
        let p = 2;

        // Manually insert two wells far apart
        let mut w1 = Well::new(p);
        w1.sum_k = vec![Complex64::new(1.0, 0.0); p];
        w1.count = 1;

        let mut w2 = Well::new(p);
        w2.sum_k = vec![Complex64::new(10.0, 0.0); p];
        w2.count = 1;

        brain.wells.insert("near".into(), w1);
        brain.wells.insert("far".into(), w2);
        brain.invalidate_caches();

        let obs = vec![Complex64::new(1.5, 0.0); p];
        let (name, _dist) = brain.nearest_well(&obs);
        assert_eq!(name, "near");
    }
}
