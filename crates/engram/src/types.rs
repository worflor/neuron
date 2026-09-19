// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Core data types for the Engram codec.
//!
//! These mirror the Python dataclasses but with Rust's type system
//! ensuring correctness at compile time.

use num_complex::Complex64;

/// Mathematical constants derived from the AR(2) oscillator structure.
pub const AR_ORDER: usize = 2;
pub const SEED_COUNT: usize = AR_ORDER;
pub const MIN_BLOCK: usize = SEED_COUNT + 2;
pub const MACHINE_EPS: f64 = f32::EPSILON as f64;

/// Tikhonov ridge regularization scale for the Cramer solver.
pub const RIDGE_SCALE: f64 = 1e-4;

/// Drift/texture blend weights for the measure function.
/// Drift captures the thought (eigenvalue centroid distance).
/// Texture captures the utterance (residual energy).
pub const DRIFT_WEIGHT: f64 = 0.7;
pub const TEXTURE_WEIGHT: f64 = 1.0 - DRIFT_WEIGHT;

/// Block encoding modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Macro + micro oscillators (the full hierarchical predictor).
    Cascaded = 0,
    /// K=2, G=1: constant velocity. The degenerate straight line.
    Linear = 1,
    /// K=1, G=0: hold. The trajectory is at rest.
    Static = 2,
    /// Passthrough. Not enough samples for prediction.
    Raw = 3,
}

/// A single encoded block within a packet.
#[derive(Debug, Clone)]
pub struct Block {
    pub mode: Mode,
    pub start: usize,
    pub length: usize,
    pub macro_k: Vec<Complex64>,
    pub macro_g: Vec<Complex64>,
    pub micro_ks: Vec<Vec<Complex64>>,
    pub micro_gs: Vec<Vec<Complex64>>,
    pub residuals: Vec<f32>,        // [length * dim] row-major flat
    pub seed1: Vec<f32>,            // [dim]
    pub seed2: Vec<f32>,            // [dim]
    pub scales: Vec<f32>,           // [pairs]
    pub pair_rms: Option<Vec<f32>>, // [pairs] per-pair residual RMS
    pub signal_energy: f64,
    pub residual_energy: f64,
    pub macro_capture: f64,
    pub micro_capture: f64,
}

/// The encoded trajectory. Orbital parameters + quantized residuals.
#[derive(Debug, Clone)]
pub struct Packet {
    pub dim: usize,
    pub pairs: usize,
    pub length: usize,
    pub alpha: f32,
    pub macro_block: usize,
    pub micro_block: usize,
    pub pairing: Vec<i32>,
    pub blocks: Vec<Block>,
}

impl Packet {
    /// Total signal energy across all blocks.
    #[must_use]
    pub fn total_energy(&self) -> f64 {
        self.blocks.iter().map(|b| b.signal_energy).sum()
    }

    /// Total residual energy across all blocks.
    #[must_use]
    pub fn residual_energy(&self) -> f64 {
        self.blocks.iter().map(|b| b.residual_energy).sum()
    }

    /// Energy capture percentage.
    #[must_use]
    pub fn capture(&self) -> f64 {
        let te = self.total_energy();
        if te < MACHINE_EPS {
            return 0.0;
        }
        ((1.0 - self.residual_energy() / te) * 100.0).max(0.0)
    }
}

/// The result of fitting a single AR(2) oscillator.
#[derive(Debug, Clone, Copy)]
pub struct FitResult {
    pub k: Complex64,
    pub g: Complex64,
    pub rms: f64,
}

impl FitResult {
    /// Linear fallback: constant velocity (K=2, G=1).
    #[inline]
    #[must_use]
    pub const fn linear() -> Self {
        Self {
            k: LINEAR_K,
            g: LINEAR_G,
            rms: 0.0,
        }
    }
}

/// The result of fitting P independent AR(2) oscillators.
#[derive(Debug, Clone)]
pub struct FitAllResult {
    pub k: Vec<Complex64>,
    pub g: Vec<Complex64>,
    pub mean_rms: f64,
}

/// Linear (constant velocity) K and G: z[n] = 2*z[n-1] - z[n-2].
pub const LINEAR_K: Complex64 = Complex64::new(2.0, 0.0);
pub const LINEAR_G: Complex64 = Complex64::new(1.0, 0.0);

/// Static (hold) K and G: z[n] = z[n-1].
pub const STATIC_K: Complex64 = Complex64::new(1.0, 0.0);
pub const STATIC_G: Complex64 = Complex64::new(0.0, 0.0);
