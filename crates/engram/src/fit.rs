// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! The core AR(2) fitting algorithm.
//!
//! Five dot products and a division. This is the entire engine.
//!
//! Optimization hierarchy (most impactful first):
//!
//! 1. VECTORIZE ACROSS PAIRS: process all P pairs per timestep in SIMD.
//!    the pair dimension IS the SIMD dimension. one pass through time,
//!    all 150 pairs simultaneously. no threading overhead, perfect cache.
//!
//! 2. SINGLE-PASS RMS: algebraic RMS from cross-correlation terms.
//!    no second traversal of the data.
//!
//! 3. SoA LAYOUT: separate real/imaginary arrays for auto-vectorization.
//!    LLVM vectorizes simple f64 loops but not Complex64 loops.
//!
//! 4. FMA: mul_add patterns that compile to fused multiply-add.
//!
//! 5. RECIPROCAL DIVISION: one 1/det instead of two divisions.

use crate::types::{FitAllResult, FitResult, LINEAR_K, MACHINE_EPS, RIDGE_SCALE, SEED_COUNT};
use num_complex::Complex64;

/// Fit a single AR(2) oscillator to a contiguous complex time series.
/// Single pass with algebraic RMS. Used for individual pair fitting.
#[inline]
pub fn fit_pair(z: &[Complex64]) -> FitResult {
    let t = z.len();
    if t < SEED_COUNT + 1 {
        return FitResult::linear();
    }

    let n = t - 2;

    let mut s_aa: f64 = 0.0;
    let mut s_bb: f64 = 0.0;
    let mut s_tt: f64 = 0.0;
    let mut s_ab_re: f64 = 0.0;
    let mut s_ab_im: f64 = 0.0;
    let mut s_ta_re: f64 = 0.0;
    let mut s_ta_im: f64 = 0.0;
    let mut s_tb_re: f64 = 0.0;
    let mut s_tb_im: f64 = 0.0;

    for i in 0..n {
        let (a_re, a_im) = (z[i + 1].re, z[i + 1].im);
        let (b_re, b_im) = (z[i].re, z[i].im);
        let (t_re, t_im) = (z[i + 2].re, z[i + 2].im);

        // norms (compile to FMA)
        s_aa = a_re.mul_add(a_re, a_im.mul_add(a_im, s_aa));
        s_bb = b_re.mul_add(b_re, b_im.mul_add(b_im, s_bb));
        s_tt = t_re.mul_add(t_re, t_im.mul_add(t_im, s_tt));

        // complex inner products decomposed to real ops (SIMD-friendly)
        // a * conj(b) = (a_re*b_re + a_im*b_im) + i*(a_im*b_re - a_re*b_im)
        s_ab_re = a_re.mul_add(b_re, a_im.mul_add(b_im, s_ab_re));
        s_ab_im = a_im.mul_add(b_re, (-a_re).mul_add(b_im, s_ab_im));

        s_ta_re = t_re.mul_add(a_re, t_im.mul_add(a_im, s_ta_re));
        s_ta_im = t_im.mul_add(a_re, (-t_re).mul_add(a_im, s_ta_im));

        s_tb_re = t_re.mul_add(b_re, t_im.mul_add(b_im, s_tb_re));
        s_tb_im = t_im.mul_add(b_re, (-t_re).mul_add(b_im, s_tb_im));
    }

    let s_ab = Complex64::new(s_ab_re, s_ab_im);
    let s_ta = Complex64::new(s_ta_re, s_ta_im);
    let s_tb = Complex64::new(s_tb_re, s_tb_im);

    solve_and_rms(s_aa, s_bb, s_tt, s_ab, s_ta, s_tb, n)
}

/// The 2x2 Cramer solve + algebraic RMS. Shared by all fit paths.
#[inline]
fn solve_and_rms(
    s_aa: f64,
    s_bb: f64,
    s_tt: f64,
    s_ab: Complex64,
    s_ta: Complex64,
    s_tb: Complex64,
    n: usize,
) -> FitResult {
    let trace = s_aa + s_bb;
    let ridge = MACHINE_EPS.max(RIDGE_SCALE * trace * 0.5);
    let s_aa_r = s_aa + ridge;
    let s_bb_r = s_bb + ridge;

    let det_re = s_aa_r.mul_add(s_bb_r, -s_ab.norm_sqr());

    if det_re.abs() < MACHINE_EPS * trace * trace {
        return FitResult::linear();
    }

    let inv_det = 1.0 / det_re;
    let inv_det_c = Complex64::new(inv_det, 0.0);

    let k = (s_ta * s_bb_r - s_ab.conj() * s_tb) * inv_det_c;
    let g = (s_ab * s_ta - Complex64::new(s_aa_r, 0.0) * s_tb) * inv_det_c;

    // algebraic RMS: Σ|tgt - K·A + G·B|² from the 6 dot products
    let err_sq = s_tt + k.norm_sqr() * s_aa + g.norm_sqr() * s_bb - 2.0 * (k.conj() * s_ta).re
        + 2.0 * (g.conj() * s_tb).re
        - 2.0 * (k * g.conj() * s_ab).re;

    FitResult {
        k,
        g,
        rms: (err_sq.max(0.0) / n as f64).sqrt(),
    }
}

/// Fit P independent AR(2) oscillators from a row-major [T x P] matrix.
///
/// THE ELDRITCH OPTIMIZATION: processes all P pairs per timestep.
/// The pair dimension IS the SIMD dimension. SoA layout enables
/// LLVM auto-vectorization: each inner loop iteration processes
/// P/SIMD_WIDTH pairs in parallel (P=150, AVX2=4 lanes → ~38 ops).
///
/// One pass through time. All pairs at once. Perfect cache access.
/// No threading overhead. No allocation beyond the accumulators.
pub fn fit_all(z_c: &[Complex64], t: usize, p: usize) -> FitAllResult {
    debug_assert_eq!(z_c.len(), t * p, "z_c length must be T * P");

    if t < SEED_COUNT + 1 || p == 0 {
        return FitAllResult {
            k: vec![LINEAR_K; p],
            g: vec![Complex64::ZERO; p],
            mean_rms: 0.0,
        };
    }

    let n = t - 2;

    // SoA accumulators: one value per pair, contiguous for SIMD
    let mut s_aa = vec![0.0_f64; p];
    let mut s_bb = vec![0.0_f64; p];
    let mut s_tt = vec![0.0_f64; p];
    let mut s_ab_re = vec![0.0_f64; p];
    let mut s_ab_im = vec![0.0_f64; p];
    let mut s_ta_re = vec![0.0_f64; p];
    let mut s_ta_im = vec![0.0_f64; p];
    let mut s_tb_re = vec![0.0_f64; p];
    let mut s_tb_im = vec![0.0_f64; p];

    // OUTER LOOP: time. INNER LOOP: all pairs (SIMD-vectorized by LLVM).
    // at each timestep, process all P pairs simultaneously.
    for i in 0..n {
        let a_row = &z_c[(i + 1) * p..(i + 2) * p]; // z[n-1] for all pairs
        let b_row = &z_c[i * p..(i + 1) * p]; // z[n-2] for all pairs
        let t_row = &z_c[(i + 2) * p..(i + 3) * p]; // z[n] for all pairs

        // this inner loop auto-vectorizes: P contiguous f64 ops,
        // no loop-carried deps across pairs, pure arithmetic.
        for j in 0..p {
            let (a_re, a_im) = (a_row[j].re, a_row[j].im);
            let (b_re, b_im) = (b_row[j].re, b_row[j].im);
            let (t_re, t_im) = (t_row[j].re, t_row[j].im);

            s_aa[j] = a_re.mul_add(a_re, a_im.mul_add(a_im, s_aa[j]));
            s_bb[j] = b_re.mul_add(b_re, b_im.mul_add(b_im, s_bb[j]));
            s_tt[j] = t_re.mul_add(t_re, t_im.mul_add(t_im, s_tt[j]));

            s_ab_re[j] = a_re.mul_add(b_re, a_im.mul_add(b_im, s_ab_re[j]));
            s_ab_im[j] = a_im.mul_add(b_re, (-a_re).mul_add(b_im, s_ab_im[j]));

            s_ta_re[j] = t_re.mul_add(a_re, t_im.mul_add(a_im, s_ta_re[j]));
            s_ta_im[j] = t_im.mul_add(a_re, (-t_re).mul_add(a_im, s_ta_im[j]));

            s_tb_re[j] = t_re.mul_add(b_re, t_im.mul_add(b_im, s_tb_re[j]));
            s_tb_im[j] = t_im.mul_add(b_re, (-t_re).mul_add(b_im, s_tb_im[j]));
        }
    }

    // SOLVE PHASE: also vectorizable across P (batched 2x2 Cramer)
    let mut k_out = Vec::with_capacity(p);
    let mut g_out = Vec::with_capacity(p);
    let mut total_rms = 0.0;

    for j in 0..p {
        let s_ab = Complex64::new(s_ab_re[j], s_ab_im[j]);
        let s_ta = Complex64::new(s_ta_re[j], s_ta_im[j]);
        let s_tb = Complex64::new(s_tb_re[j], s_tb_im[j]);

        let result = solve_and_rms(s_aa[j], s_bb[j], s_tt[j], s_ab, s_ta, s_tb, n);
        k_out.push(result.k);
        g_out.push(result.g);
        total_rms += result.rms;
    }

    FitAllResult {
        k: k_out,
        g: g_out,
        mean_rms: total_rms / p as f64,
    }
}

/// Fit a single pair from a strided matrix. Zero allocation.
#[inline]
pub fn fit_pair_strided(z_c: &[Complex64], t: usize, p: usize, pair: usize) -> FitResult {
    if t < SEED_COUNT + 1 {
        return FitResult::linear();
    }

    let n = t - 2;
    let mut s_aa: f64 = 0.0;
    let mut s_bb: f64 = 0.0;
    let mut s_tt: f64 = 0.0;
    let mut s_ab_re: f64 = 0.0;
    let mut s_ab_im: f64 = 0.0;
    let mut s_ta_re: f64 = 0.0;
    let mut s_ta_im: f64 = 0.0;
    let mut s_tb_re: f64 = 0.0;
    let mut s_tb_im: f64 = 0.0;

    for i in 0..n {
        let (a_re, a_im) = (z_c[(i + 1) * p + pair].re, z_c[(i + 1) * p + pair].im);
        let (b_re, b_im) = (z_c[i * p + pair].re, z_c[i * p + pair].im);
        let (t_re, t_im) = (z_c[(i + 2) * p + pair].re, z_c[(i + 2) * p + pair].im);

        s_aa = a_re.mul_add(a_re, a_im.mul_add(a_im, s_aa));
        s_bb = b_re.mul_add(b_re, b_im.mul_add(b_im, s_bb));
        s_tt = t_re.mul_add(t_re, t_im.mul_add(t_im, s_tt));

        s_ab_re = a_re.mul_add(b_re, a_im.mul_add(b_im, s_ab_re));
        s_ab_im = a_im.mul_add(b_re, (-a_re).mul_add(b_im, s_ab_im));

        s_ta_re = t_re.mul_add(a_re, t_im.mul_add(a_im, s_ta_re));
        s_ta_im = t_im.mul_add(a_re, (-t_re).mul_add(a_im, s_ta_im));

        s_tb_re = t_re.mul_add(b_re, t_im.mul_add(b_im, s_tb_re));
        s_tb_im = t_im.mul_add(b_re, (-t_re).mul_add(b_im, s_tb_im));
    }

    let s_ab = Complex64::new(s_ab_re, s_ab_im);
    let s_ta = Complex64::new(s_ta_re, s_ta_im);
    let s_tb = Complex64::new(s_tb_re, s_tb_im);

    solve_and_rms(s_aa, s_bb, s_tt, s_ab, s_ta, s_tb, n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn fit_pair_linear_ramp() {
        let z: Vec<Complex64> = (0..50)
            .map(|i| Complex64::new(i as f64 * 0.01, 0.0))
            .collect();
        let result = fit_pair(&z);
        assert!(
            (result.k.re - 1.4408).abs() < 0.01,
            "K.re = {} (expected ~1.4408)",
            result.k.re
        );
    }

    #[test]
    fn fit_pair_pure_cosine() {
        let z: Vec<Complex64> = (0..100)
            .map(|i| Complex64::new((2.0 * PI * i as f64 / 10.0).cos(), 0.0))
            .collect();
        let result = fit_pair(&z);
        assert!(result.rms < 0.01, "rms = {}", result.rms);
        let disc = result.k * result.k - Complex64::new(4.0, 0.0) * result.g;
        let l1 = (result.k + disc.sqrt()) / Complex64::new(2.0, 0.0);
        assert!((l1.norm() - 1.0).abs() < 0.01, "|λ| = {}", l1.norm());
    }

    #[test]
    fn fit_pair_complex_exponential() {
        let z: Vec<Complex64> = (0..80)
            .map(|i| {
                let theta = 2.0 * PI * i as f64 / 8.0;
                Complex64::new(theta.cos(), theta.sin())
            })
            .collect();
        let result = fit_pair(&z);
        assert!(result.rms < 0.01, "rms = {}", result.rms);
    }

    #[test]
    fn fit_pair_damped() {
        let z: Vec<Complex64> = (0..60)
            .map(|i| {
                let decay = 0.95_f64.powi(i);
                Complex64::new(decay * (2.0 * PI * i as f64 / 12.0).cos(), 0.0)
            })
            .collect();
        let result = fit_pair(&z);
        assert!(result.rms < 0.1);
        let disc = result.k * result.k - Complex64::new(4.0, 0.0) * result.g;
        let l1 = (result.k + disc.sqrt()) / Complex64::new(2.0, 0.0);
        assert!(l1.norm() < 1.0, "|λ| = {} (should decay)", l1.norm());
    }

    #[test]
    fn fit_pair_degenerate() {
        let z = vec![Complex64::new(5.0, 0.0); 20];
        let result = fit_pair(&z);
        assert!(result.k.re.is_finite());
    }

    #[test]
    fn fit_pair_too_short() {
        let z = vec![Complex64::new(1.0, 0.0); 2];
        let result = fit_pair(&z);
        assert_eq!(result.k, LINEAR_K);
    }

    #[test]
    fn algebraic_rms_matches_direct() {
        let z: Vec<Complex64> = (0..40)
            .map(|i| {
                Complex64::new(
                    (i as f64 * 0.7).sin() + (i as f64 * 0.3).cos(),
                    (i as f64 * 0.5).sin(),
                )
            })
            .collect();

        let result = fit_pair(&z);
        let n = z.len() - 2;
        let direct_err_sq: f64 = (0..n)
            .map(|i| (z[i + 2] - (result.k * z[i + 1] - result.g * z[i])).norm_sqr())
            .sum();
        let direct_rms = (direct_err_sq / n as f64).sqrt();

        assert!(
            (result.rms - direct_rms).abs() < 1e-10,
            "algebraic={}, direct={}",
            result.rms,
            direct_rms
        );
    }

    #[test]
    fn fit_all_matches_individual() {
        let p = 8;
        let t = 50;
        let z: Vec<Complex64> = (0..t * p)
            .map(|i| {
                let row = i / p;
                let col = i % p;
                Complex64::new(
                    (row as f64 * (col + 1) as f64 * 0.3).sin(),
                    (row as f64 * (col + 1) as f64 * 0.2).cos(),
                )
            })
            .collect();

        let result = fit_all(&z, t, p);

        for pair in 0..p {
            let col: Vec<Complex64> = (0..t).map(|row| z[row * p + pair]).collect();
            let individual = fit_pair(&col);
            assert!(
                (result.k[pair] - individual.k).norm() < 1e-10,
                "pair {} K mismatch",
                pair
            );
            assert!(
                (result.g[pair] - individual.g).norm() < 1e-10,
                "pair {} G mismatch",
                pair
            );
        }
    }

    #[test]
    fn fit_all_150_pairs() {
        // realistic: 150 pairs, 100 timesteps
        let p = 150;
        let t = 100;
        let z: Vec<Complex64> = (0..t * p)
            .map(|i| {
                let row = i / p;
                let col = i % p;
                Complex64::new(
                    (row as f64 * 0.1 + col as f64 * 0.05).sin(),
                    (row as f64 * 0.07 + col as f64 * 0.03).cos(),
                )
            })
            .collect();

        let result = fit_all(&z, t, p);
        assert_eq!(result.k.len(), p);
        assert_eq!(result.g.len(), p);
        assert!(result.mean_rms.is_finite());
        // spot check: first pair
        let col0: Vec<Complex64> = (0..t).map(|row| z[row * p]).collect();
        let individual = fit_pair(&col0);
        assert!((result.k[0] - individual.k).norm() < 1e-10);
    }
}
