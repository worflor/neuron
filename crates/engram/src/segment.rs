// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Trajectory segmentation and dimension pairing.
//!
//! Three responsibilities:
//! 1. `derive_pairing`: find natural complex pairs from correlation structure
//! 2. `derive_block_sizes`: autocorrelation-driven macro/micro sizing
//! 3. segment: cut trajectory at phase transitions (3-sigma rule)

use crate::types::MIN_BLOCK;

/// Maximum macro block size. Caps autocorrelation length.
const MAX_BLOCK: usize = 128;

/// Maximum autocorrelation lag to search.
const MAX_LAG: usize = 128;

/// Derive natural complex pairs from the trajectory's correlation structure.
///
/// Greedily matches the two most correlated dimensions, removes them,
/// and repeats. Output: pairing\[D\] where pairing\[2k\], pairing\[2k+1\]
/// are the original dimension indices for pair k.
///
/// Cost: O(D² · T) for correlation + O(D²) for greedy matching.
#[must_use]
pub fn derive_pairing(w: &[f32], t: usize, dim: usize) -> Vec<i32> {
    debug_assert_eq!(dim % 2, 0);
    let p = dim / 2;

    if t < 2 || dim < 2 {
        return (0..dim as i32).collect();
    }

    // 1. Center each dimension: mean subtraction
    let mut means = vec![0.0_f64; dim];
    for row in 0..t {
        let base = row * dim;
        for d in 0..dim {
            means[d] += f64::from(w[base + d]);
        }
    }
    let inv_t = 1.0 / t as f64;
    for m in &mut means {
        *m *= inv_t;
    }

    // 2. Compute column norms (for normalization)
    let mut norms = vec![0.0_f64; dim];
    for row in 0..t {
        let base = row * dim;
        for d in 0..dim {
            let v = f64::from(w[base + d]) - means[d];
            norms[d] = v.mul_add(v, norms[d]);
        }
    }
    for n in &mut norms {
        *n = n.sqrt().max(1e-30);
    }

    // 3. Correlation matrix: |corr[i][j]| = |Σ centered_i · centered_j / (norm_i · norm_j · (T-1))|
    //    Only upper triangle needed, symmetric.
    let inv_tm1 = 1.0 / (t as f64 - 1.0).max(1.0);
    let mut corr = vec![0.0_f64; dim * dim];

    for row in 0..t {
        let base = row * dim;
        for i in 0..dim {
            let vi = (f64::from(w[base + i]) - means[i]) / norms[i];
            // self-correlation = 1.0, skip it (we zero diagonal below)
            for j in (i + 1)..dim {
                let vj = (f64::from(w[base + j]) - means[j]) / norms[j];
                corr[i * dim + j] = vi.mul_add(vj, corr[i * dim + j]);
            }
        }
    }

    // Scale and absolute-value, mirror to lower triangle, zero diagonal
    for i in 0..dim {
        corr[i * dim + i] = 0.0;
        for j in (i + 1)..dim {
            let v = (corr[i * dim + j] * inv_tm1).abs();
            corr[i * dim + j] = v;
            corr[j * dim + i] = v;
        }
    }

    // 4. Greedy matching: repeatedly pick the strongest pair
    let mut pairing = vec![0_i32; dim];
    let mut used = vec![false; dim];
    let mut pair_idx = 0;

    for _ in 0..p {
        let mut best_val = -1.0_f64;
        let mut best_i = 0;
        let mut best_j = 0;

        for i in 0..dim {
            if used[i] {
                continue;
            }
            for j in (i + 1)..dim {
                if used[j] {
                    continue;
                }
                if corr[i * dim + j] > best_val {
                    best_val = corr[i * dim + j];
                    best_i = i;
                    best_j = j;
                }
            }
        }

        pairing[pair_idx * 2] = best_i as i32;
        pairing[pair_idx * 2 + 1] = best_j as i32;
        used[best_i] = true;
        used[best_j] = true;
        pair_idx += 1;
    }

    pairing
}

/// Apply a pairing permutation to reorder dimensions.
///
/// Input: w\[T × D\] row-major, pairing\[D\].
/// Output: `w_p`\[T × D\] with columns reordered by pairing.
#[must_use]
pub fn apply_pairing(w: &[f32], t: usize, dim: usize, pairing: &[i32]) -> Vec<f32> {
    let mut out = vec![0.0_f32; t * dim];
    for row in 0..t {
        for d in 0..dim {
            out[row * dim + d] = w[row * dim + pairing[d] as usize];
        }
    }
    out
}

/// Undo a pairing permutation (inverse permutation).
#[must_use]
pub fn undo_pairing(w: &[f32], t: usize, dim: usize, pairing: &[i32]) -> Vec<f32> {
    let mut out = vec![0.0_f32; t * dim];
    for row in 0..t {
        for d in 0..dim {
            out[row * dim + pairing[d] as usize] = w[row * dim + d];
        }
    }
    out
}

/// Velocity autocorrelation length: first lag where ACF ≤ 0.
///
/// This finds the natural decorrelation timescale of the trajectory —
/// the point where the signal has "forgotten" its initial direction.
fn autocorrelation_length(w: &[f32], t: usize, dim: usize) -> usize {
    if t < 3 {
        return MIN_BLOCK;
    }

    // Compute velocity magnitudes: ||w[i+1] - w[i]||₂
    let vel_len = t - 1;
    let mut vel_norm = Vec::with_capacity(vel_len);

    for i in 0..vel_len {
        let base_curr = i * dim;
        let base_next = (i + 1) * dim;
        let mut sq_sum = 0.0_f64;
        for d in 0..dim {
            let diff = f64::from(w[base_next + d]) - f64::from(w[base_curr + d]);
            sq_sum = diff.mul_add(diff, sq_sum);
        }
        vel_norm.push(sq_sum.sqrt());
    }

    // Center
    let mean: f64 = vel_norm.iter().sum::<f64>() / vel_len as f64;
    let centered: Vec<f64> = vel_norm.iter().map(|&v| v - mean).collect();

    // Variance
    let var: f64 = centered.iter().map(|&v| v * v).sum();
    if var < 1e-30 {
        return MIN_BLOCK;
    }

    // Find first lag where ACF ≤ 0
    let max_lag = MAX_LAG.min(vel_len / 2);
    for lag in 1..=max_lag {
        let acf: f64 = centered[..vel_len - lag]
            .iter()
            .zip(&centered[lag..])
            .map(|(&a, &b)| a * b)
            .sum::<f64>()
            / var;
        if acf <= 0.0 {
            return lag.max(MIN_BLOCK);
        }
    }

    max_lag.max(MIN_BLOCK)
}

/// Derive macro and micro block sizes from the trajectory.
///
/// macro = autocorrelation length (capped at 128)
/// micro = macro / 4 (harmonic separation ratio)
#[must_use]
pub fn derive_block_sizes(w: &[f32], t: usize, dim: usize) -> (usize, usize) {
    let mac = MAX_BLOCK.min(autocorrelation_length(w, t, dim).max(MIN_BLOCK));
    let mic = (mac / 4).max(MIN_BLOCK);
    (mac, mic)
}

/// Linear prediction error at each timestep.
///
/// err\[i\] = ||w\[i\] - (2·w\[i-1\] - w\[i-2\])||₂ for i ∈ \[2, T\).
/// Returns errors for indices \[0, T-2\) where index 0 corresponds to t=2.
fn linear_prediction_errors(w: &[f32], t: usize, dim: usize) -> Vec<f64> {
    if t < 3 {
        return Vec::new();
    }

    let n = t - 2;
    let mut errors = Vec::with_capacity(n);

    for i in 2..t {
        let base = i * dim;
        let base_1 = (i - 1) * dim;
        let base_2 = (i - 2) * dim;
        let mut sq = 0.0_f64;
        for d in 0..dim {
            let pred = 2.0 * f64::from(w[base_1 + d]) - f64::from(w[base_2 + d]);
            let diff = f64::from(w[base + d]) - pred;
            sq = diff.mul_add(diff, sq);
        }
        errors.push(sq.sqrt());
    }

    errors
}

/// Segment a trajectory into blocks at phase transitions.
///
/// Uses the 3-sigma rule: a new segment starts when the linear prediction
/// error exceeds mean + 3·std. Also forces a split at `max_block` length.
///
/// Returns Vec<(start, end)> segment boundaries.
#[must_use]
pub fn segment(w: &[f32], t: usize, dim: usize, max_block: usize) -> Vec<(usize, usize)> {
    if t <= MIN_BLOCK {
        return if t > 0 { vec![(0, t)] } else { Vec::new() };
    }

    let errors = linear_prediction_errors(w, t, dim);
    if errors.is_empty() {
        return vec![(0, t)];
    }

    // Statistics for threshold
    let n = errors.len() as f64;
    let e_mean: f64 = errors.iter().sum::<f64>() / n;
    let e_var: f64 = errors
        .iter()
        .map(|&e| (e - e_mean) * (e - e_mean))
        .sum::<f64>()
        / n;
    let threshold = e_mean + 3.0 * e_var.sqrt();

    let mut segments = Vec::new();
    let mut block_start: usize = 0;

    // errors[i] corresponds to t=i+2 in the original trajectory
    for (ei, &err) in errors.iter().enumerate() {
        let t_idx = ei + 2; // actual index in w
        let block_len = t_idx - block_start;

        // Force split at max_block
        if block_len >= max_block {
            segments.push((block_start, t_idx));
            block_start = t_idx;
            continue;
        }

        // Phase transition split (only if block is already MIN_BLOCK long)
        if block_len >= MIN_BLOCK && err > threshold {
            segments.push((block_start, t_idx));
            block_start = t_idx;
        }
    }

    // Final segment
    if block_start < t {
        // Merge trailing short segment with previous
        if t - block_start < MIN_BLOCK && !segments.is_empty() {
            let last = segments.last_mut().unwrap();
            last.1 = t;
        } else {
            segments.push((block_start, t));
        }
    }

    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn pairing_identity_on_already_paired() {
        // 4 dims: (0,1) are highly correlated, (2,3) are highly correlated
        let t = 50;
        let dim = 4;
        let mut w = vec![0.0_f32; t * dim];
        for i in 0..t {
            let x = (i as f64 * 0.3).sin() as f32;
            let y = (i as f64 * 0.3 + 0.1).sin() as f32; // close to x
            let a = (i as f64 * 0.9).cos() as f32;
            let b = (i as f64 * 0.9 + 0.05).cos() as f32; // close to a
            w[i * dim] = x;
            w[i * dim + 1] = y;
            w[i * dim + 2] = a;
            w[i * dim + 3] = b;
        }

        let pairing = derive_pairing(&w, t, dim);
        assert_eq!(pairing.len(), dim);

        // Should pair (0,1) and (2,3) in some order
        let pair0 = (pairing[0].min(pairing[1]), pairing[0].max(pairing[1]));
        let pair1 = (pairing[2].min(pairing[3]), pairing[2].max(pairing[3]));
        let mut pairs = [pair0, pair1];
        pairs.sort_unstable();
        assert_eq!(pairs, [(0, 1), (2, 3)]);
    }

    #[test]
    fn pairing_roundtrip() {
        let t = 10;
        let dim = 6;
        let w: Vec<f32> = (0..t * dim).map(|i| i as f32 * 0.1).collect();
        let pairing = derive_pairing(&w, t, dim);

        let paired = apply_pairing(&w, t, dim, &pairing);
        let restored = undo_pairing(&paired, t, dim, &pairing);

        for (a, b) in w.iter().zip(restored.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn block_sizes_sane() {
        let t = 200;
        let dim = 4;
        // smooth cosine — long autocorrelation
        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                (row as f64 * 2.0 * PI / 50.0).cos() as f32
            })
            .collect();

        let (mac, mic) = derive_block_sizes(&w, t, dim);
        assert!(mac >= MIN_BLOCK);
        assert!(mac <= MAX_BLOCK);
        assert!(mic >= MIN_BLOCK);
        assert!(mic <= mac);
    }

    #[test]
    fn segment_covers_all() {
        let t = 200;
        let dim = 4;
        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                (row as f64 * 0.1).sin() as f32
            })
            .collect();

        let segs = segment(&w, t, dim, 50);
        assert!(!segs.is_empty());
        assert_eq!(segs[0].0, 0);
        assert_eq!(segs.last().unwrap().1, t);

        // No gaps
        for i in 1..segs.len() {
            assert_eq!(segs[i].0, segs[i - 1].1);
        }

        // All segments ≥ MIN_BLOCK (except possibly the last if merged)
        for (s, e) in &segs {
            assert!(
                e - s >= MIN_BLOCK || *e == t,
                "segment ({s}, {e}) too short"
            );
        }
    }

    #[test]
    fn segment_detects_phase_transition() {
        let t = 100;
        let dim = 2;
        let mut w = vec![0.0_f32; t * dim];

        // Smooth for first 50, then sudden jump + different dynamics
        for i in 0..50 {
            w[i * dim] = (i as f32) * 0.01;
            w[i * dim + 1] = 0.0;
        }
        for i in 50..t {
            w[i * dim] = 100.0 + (i as f32 - 50.0) * 0.01;
            w[i * dim + 1] = 50.0;
        }

        let segs = segment(&w, t, dim, 128);
        // Should detect the jump around index 50
        assert!(
            segs.len() >= 2,
            "expected phase split, got {} segments",
            segs.len()
        );
    }

    #[test]
    fn segment_tiny_trajectory() {
        let w = vec![1.0_f32; 3 * 2]; // 3 samples, 2 dims
        let segs = segment(&w, 3, 2, 128);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], (0, 3));
    }

    #[test]
    fn linear_errors_correct() {
        // For a perfect linear ramp, prediction errors should be ~0
        let t = 20;
        let dim = 2;
        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                row as f32 * 0.5
            })
            .collect();

        let errors = linear_prediction_errors(&w, t, dim);
        assert_eq!(errors.len(), t - 2);
        for &e in &errors {
            assert!(e < 1e-5, "linear ramp should have ~0 prediction error");
        }
    }
}
