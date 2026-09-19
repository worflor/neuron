// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! The AR(2) prediction engine.
//!
//! Given fitted K, G coefficients and two seed values, spin the oscillator
//! forward to generate predicted trajectories. The residual between
//! prediction and reality is what gets encoded.
//!
//! z[n] = K · z[n-1] − G · z[n-2]
//!
//! Same optimization philosophy as fit.rs:
//! - Vectorize across pairs: inner loop over P pairs, SIMD-friendly
//! - `SoA` layout: separate real/imaginary for auto-vectorization
//! - FMA: `mul_add` patterns throughout
//! - Zero unnecessary allocation

use num_complex::Complex64;

/// Predict a single complex time series forward using the AR(2) recurrence.
///
/// Returns a vector of `length` predicted values.
#[inline]
#[must_use]
pub fn predict_pair(
    seed1: Complex64, // z[n-1] (most recent)
    seed2: Complex64, // z[n-2]
    k: Complex64,
    g: Complex64,
    length: usize,
) -> Vec<Complex64> {
    let mut out = Vec::with_capacity(length);
    let mut prev1 = seed1;
    let mut prev2 = seed2;

    for _ in 0..length {
        let next = k * prev1 - g * prev2;
        out.push(next);
        prev2 = prev1;
        prev1 = next;
    }

    out
}

/// Predict P independent AR(2) oscillators forward simultaneously.
///
/// THE SAME ELDRITCH OPTIMIZATION as `fit_all`: the pair dimension IS
/// the SIMD dimension. Outer loop over time, inner loop over all P pairs.
///
/// Input layout (all length P, contiguous for SIMD):
///   seed1[P] — z[n-1] for each pair
///   seed2[P] — z[n-2] for each pair
///   k[P], g[P] — oscillator coefficients
///
/// Output: row-major [length × P] complex, matching `fit_all`'s input format.
#[must_use]
pub fn predict_all(
    seed1: &[Complex64],
    seed2: &[Complex64],
    k: &[Complex64],
    g: &[Complex64],
    length: usize,
) -> Vec<Complex64> {
    let p = seed1.len();
    debug_assert_eq!(seed2.len(), p);
    debug_assert_eq!(k.len(), p);
    debug_assert_eq!(g.len(), p);

    if length == 0 || p == 0 {
        return Vec::new();
    }

    // SoA state: separate real/imaginary for SIMD auto-vectorization.
    // LLVM won't vectorize Complex64 ops but will vectorize paired f64 loops.
    let mut p1_re: Vec<f64> = seed1.iter().map(|c| c.re).collect();
    let mut p1_im: Vec<f64> = seed1.iter().map(|c| c.im).collect();
    let mut p2_re: Vec<f64> = seed2.iter().map(|c| c.re).collect();
    let mut p2_im: Vec<f64> = seed2.iter().map(|c| c.im).collect();

    // Pre-extract K, G into SoA for the inner loop
    let k_re: Vec<f64> = k.iter().map(|c| c.re).collect();
    let k_im: Vec<f64> = k.iter().map(|c| c.im).collect();
    let g_re: Vec<f64> = g.iter().map(|c| c.re).collect();
    let g_im: Vec<f64> = g.iter().map(|c| c.im).collect();

    let mut out = Vec::with_capacity(length * p);

    for _ in 0..length {
        // Inner loop: all P pairs simultaneously.
        // z[n] = K·prev1 − G·prev2
        // (k_re + i·k_im)(p1_re + i·p1_im) = k_re·p1_re − k_im·p1_im + i·(k_re·p1_im + k_im·p1_re)
        // (g_re + i·g_im)(p2_re + i·p2_im) = g_re·p2_re − g_im·p2_im + i·(g_re·p2_im + g_im·p2_re)
        for j in 0..p {
            // K · prev1 (complex multiply, FMA)
            let kp_re = k_re[j].mul_add(p1_re[j], -(k_im[j] * p1_im[j]));
            let kp_im = k_re[j].mul_add(p1_im[j], k_im[j] * p1_re[j]);

            // G · prev2 (complex multiply, FMA)
            let gp_re = g_re[j].mul_add(p2_re[j], -(g_im[j] * p2_im[j]));
            let gp_im = g_re[j].mul_add(p2_im[j], g_im[j] * p2_re[j]);

            // z[n] = K·prev1 − G·prev2
            let next_re = kp_re - gp_re;
            let next_im = kp_im - gp_im;

            out.push(Complex64::new(next_re, next_im));

            // shift: prev2 ← prev1, prev1 ← next
            p2_re[j] = p1_re[j];
            p2_im[j] = p1_im[j];
            p1_re[j] = next_re;
            p1_im[j] = next_im;
        }
    }

    out
}

/// Predict P oscillators into a pre-allocated output slice.
///
/// Same as `predict_all` but writes into `out[offset..offset + length*p]`,
/// avoiding allocation. Used by the block encoder for cascaded prediction.
pub fn predict_all_into(
    seed1: &[Complex64],
    seed2: &[Complex64],
    k: &[Complex64],
    g: &[Complex64],
    length: usize,
    out: &mut [Complex64],
) {
    let p = seed1.len();
    debug_assert_eq!(seed2.len(), p);
    debug_assert_eq!(k.len(), p);
    debug_assert_eq!(g.len(), p);
    debug_assert!(out.len() >= length * p);

    if length == 0 || p == 0 {
        return;
    }

    let mut p1_re: Vec<f64> = seed1.iter().map(|c| c.re).collect();
    let mut p1_im: Vec<f64> = seed1.iter().map(|c| c.im).collect();
    let mut p2_re: Vec<f64> = seed2.iter().map(|c| c.re).collect();
    let mut p2_im: Vec<f64> = seed2.iter().map(|c| c.im).collect();

    let k_re: Vec<f64> = k.iter().map(|c| c.re).collect();
    let k_im: Vec<f64> = k.iter().map(|c| c.im).collect();
    let g_re: Vec<f64> = g.iter().map(|c| c.re).collect();
    let g_im: Vec<f64> = g.iter().map(|c| c.im).collect();

    for n in 0..length {
        let row = n * p;
        for j in 0..p {
            let kp_re = k_re[j].mul_add(p1_re[j], -(k_im[j] * p1_im[j]));
            let kp_im = k_re[j].mul_add(p1_im[j], k_im[j] * p1_re[j]);

            let gp_re = g_re[j].mul_add(p2_re[j], -(g_im[j] * p2_im[j]));
            let gp_im = g_re[j].mul_add(p2_im[j], g_im[j] * p2_re[j]);

            let next_re = kp_re - gp_re;
            let next_im = kp_im - gp_im;

            out[row + j] = Complex64::new(next_re, next_im);

            p2_re[j] = p1_re[j];
            p2_im[j] = p1_im[j];
            p1_re[j] = next_re;
            p1_im[j] = next_im;
        }
    }
}

/// Compute per-pair RMS of a residual matrix [length × dim] in float domain.
///
/// Each pair spans two adjacent dimensions (re, im). Returns [P] RMS values.
#[inline]
#[must_use]
pub fn compute_pair_rms(residuals: &[f32], length: usize, dim: usize) -> Vec<f32> {
    let p = dim / 2;
    let mut rms = vec![0.0_f64; p];

    for t in 0..length {
        let row = t * dim;
        for j in 0..p {
            let re = f64::from(residuals[row + j * 2]);
            let im = f64::from(residuals[row + j * 2 + 1]);
            rms[j] = re.mul_add(re, im.mul_add(im, rms[j]));
        }
    }

    let inv_len = 1.0 / length as f64;
    rms.iter().map(|&s| (s * inv_len).sqrt() as f32).collect()
}

/// Convert a float trajectory [T × D] to complex pairs [T × P].
///
/// D must be even. Adjacent dimensions pair into complex: (d[2i], d[2i+1]) → re + i·im.
/// This is the coordinate system: even dims are real, odd dims are imaginary.
#[inline]
#[must_use]
pub fn to_complex(data: &[f32], t: usize, dim: usize) -> Vec<Complex64> {
    debug_assert_eq!(dim % 2, 0, "dimension must be even for complex pairing");
    let p = dim / 2;
    let mut out = Vec::with_capacity(t * p);

    for row in 0..t {
        let base = row * dim;
        for j in 0..p {
            out.push(Complex64::new(
                f64::from(data[base + j * 2]),
                f64::from(data[base + j * 2 + 1]),
            ));
        }
    }

    out
}

/// Convert complex pairs [T × P] back to float trajectory [T × D].
///
/// Inverse of `to_complex`. Interleaves real and imaginary back to [T × D] f32.
#[inline]
#[must_use]
pub fn from_complex(z: &[Complex64], t: usize, p: usize) -> Vec<f32> {
    let dim = p * 2;
    let mut out = Vec::with_capacity(t * dim);

    for row in 0..t {
        let base = row * p;
        for j in 0..p {
            out.push(z[base + j].re as f32);
            out.push(z[base + j].im as f32);
        }
    }

    out
}

/// Compute the residual between actual data and prediction in float domain.
///
/// Both inputs are [length × dim] f32. Returns [length × dim] f32 residuals.
#[inline]
#[must_use]
pub fn compute_residuals(actual: &[f32], predicted: &[f32]) -> Vec<f32> {
    debug_assert_eq!(actual.len(), predicted.len());
    actual
        .iter()
        .zip(predicted.iter())
        .map(|(a, p)| a - p)
        .collect()
}

/// Sum of squared values (energy) in a float slice.
#[inline]
#[must_use]
pub fn energy(data: &[f32]) -> f64 {
    data.iter()
        .map(|&x| {
            let xd = f64::from(x);
            xd * xd
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{LINEAR_G, LINEAR_K, STATIC_G, STATIC_K};
    use std::f64::consts::PI;

    #[test]
    fn predict_pair_linear() {
        // constant velocity: z[n] = 2·z[n-1] − z[n-2]
        let s1 = Complex64::new(1.0, 0.0);
        let s2 = Complex64::new(0.0, 0.0);
        let pred = predict_pair(s1, s2, LINEAR_K, LINEAR_G, 5);

        assert_eq!(pred.len(), 5);
        // z[0] = 2·1 − 1·0 = 2
        // z[1] = 2·2 − 1·1 = 3
        // z[2] = 2·3 − 1·2 = 4
        for (i, z) in pred.iter().enumerate() {
            let expected = (i + 2) as f64;
            assert!(
                (z.re - expected).abs() < 1e-12,
                "step {}: got {}, expected {}",
                i,
                z.re,
                expected
            );
        }
    }

    #[test]
    fn predict_pair_static_hold() {
        // hold: z[n] = z[n-1]
        let s1 = Complex64::new(42.0, -7.0);
        let s2 = Complex64::new(100.0, 200.0); // irrelevant, G=0
        let pred = predict_pair(s1, s2, STATIC_K, STATIC_G, 10);

        for z in &pred {
            assert!((z.re - 42.0).abs() < 1e-12);
            assert!((z.im - (-7.0)).abs() < 1e-12);
        }
    }

    #[test]
    fn predict_pair_cosine_roundtrip() {
        // fit a cosine, then predict — should match the original
        let freq = 2.0 * PI / 10.0;
        let z: Vec<Complex64> = (0..100)
            .map(|i| Complex64::new((freq * f64::from(i)).cos(), 0.0))
            .collect();

        let result = crate::fit::fit_pair(&z);
        let pred = predict_pair(z[1], z[0], result.k, result.g, 98);

        for i in 0..98 {
            let err = (pred[i] - z[i + 2]).norm();
            assert!(err < 0.05, "step {i}: error = {err}");
        }
    }

    #[test]
    fn predict_all_matches_individual() {
        let p = 4;
        let length = 20;

        let seed1: Vec<Complex64> = (0..p)
            .map(|j| Complex64::new(j as f64 * 0.5 + 1.0, j as f64 * 0.3))
            .collect();
        let seed2: Vec<Complex64> = (0..p)
            .map(|j| Complex64::new(j as f64 * 0.2, j as f64 * 0.1 + 0.5))
            .collect();
        let k: Vec<Complex64> = (0..p)
            .map(|j| Complex64::new(1.5 + j as f64 * 0.1, 0.2))
            .collect();
        let g: Vec<Complex64> = (0..p)
            .map(|j| Complex64::new(0.8 + j as f64 * 0.05, -0.1))
            .collect();

        let all = predict_all(&seed1, &seed2, &k, &g, length);
        assert_eq!(all.len(), length * p);

        for j in 0..p {
            let individual = predict_pair(seed1[j], seed2[j], k[j], g[j], length);
            for n in 0..length {
                let err = (all[n * p + j] - individual[n]).norm();
                assert!(err < 1e-12, "pair {j} step {n}: error = {err}");
            }
        }
    }

    #[test]
    fn predict_all_into_matches() {
        let p = 8;
        let length = 30;

        let seed1: Vec<Complex64> = (0..p)
            .map(|j| Complex64::new((j as f64).sin(), (j as f64).cos()))
            .collect();
        let seed2: Vec<Complex64> = vec![Complex64::ZERO; p];
        let k: Vec<Complex64> = vec![Complex64::new(1.8, 0.1); p];
        let g: Vec<Complex64> = vec![Complex64::new(0.9, 0.0); p];

        let allocating = predict_all(&seed1, &seed2, &k, &g, length);
        let mut into_buf = vec![Complex64::ZERO; length * p];
        predict_all_into(&seed1, &seed2, &k, &g, length, &mut into_buf);

        for i in 0..allocating.len() {
            assert!((allocating[i] - into_buf[i]).norm() < 1e-14);
        }
    }

    #[test]
    fn to_from_complex_roundtrip() {
        let dim = 6;
        let t = 10;
        let data: Vec<f32> = (0..t * dim).map(|i| (i as f32) * 0.1 + 0.5).collect();

        let z = to_complex(&data, t, dim);
        assert_eq!(z.len(), t * (dim / 2));

        let back = from_complex(&z, t, dim / 2);
        assert_eq!(back.len(), data.len());

        for (a, b) in data.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-6, "{a} != {b}");
        }
    }

    #[test]
    fn compute_pair_rms_uniform() {
        let dim = 4;
        let length = 100;
        // constant residual of 1.0 everywhere
        let resid = vec![1.0_f32; length * dim];
        let rms = compute_pair_rms(&resid, length, dim);

        assert_eq!(rms.len(), 2); // 4 dims → 2 pairs
        for &r in &rms {
            // each pair sums re² + im² = 1 + 1 = 2 per timestep
            // mean = 2, sqrt(2) ≈ 1.4142
            assert!((r - std::f32::consts::SQRT_2).abs() < 1e-5, "rms = {r}");
        }
    }

    #[test]
    fn energy_computation() {
        let data = vec![3.0_f32, 4.0];
        assert!((energy(&data) - 25.0).abs() < 1e-10);
    }

    #[test]
    fn predict_all_150_pairs() {
        // realistic: fit 150 pairs, predict, verify residual is small
        let p = 150;
        let t = 100;
        let freq = 2.0 * PI / 16.0;

        let z: Vec<Complex64> = (0..t * p)
            .map(|i| {
                let row = i / p;
                let col = i % p;
                let phase = freq * row as f64 + col as f64 * 0.1;
                Complex64::new(phase.cos(), phase.sin() * 0.5)
            })
            .collect();

        let fit = crate::fit::fit_all(&z, t, p);

        // extract seeds: row 0 and row 1
        let seed2: Vec<Complex64> = z[..p].to_vec();
        let seed1: Vec<Complex64> = z[p..2 * p].to_vec();

        let pred = predict_all(&seed1, &seed2, &fit.k, &fit.g, t - 2);

        // prediction should closely match rows 2..T
        let mut max_err = 0.0_f64;
        for n in 0..(t - 2) {
            for j in 0..p {
                let err = (pred[n * p + j] - z[(n + 2) * p + j]).norm();
                if err > max_err {
                    max_err = err;
                }
            }
        }
        assert!(
            max_err < 0.2,
            "max prediction error = {max_err} (should be small for smooth signal)"
        );
    }
}
