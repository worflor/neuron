// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Block encoding: fit → predict → residual → mode selection → quantize.
//!
//! The encoder takes a trajectory segment and produces a Block with:
//! - Macro K,G: the fundamental orbit
//! - Micro K,G per sub-block: harmonics in the residual
//! - Quantized residuals: the irreducible noise
//! - Mode: Cascaded, Linear, Static, or Raw

use crate::fit::fit_all;
use crate::predict::{compute_pair_rms, energy, from_complex, predict_all, to_complex};
use crate::segment::{apply_pairing, derive_block_sizes, derive_pairing, segment};
use crate::types::{
    Block, LINEAR_G, LINEAR_K, MACHINE_EPS, MIN_BLOCK, Mode, Packet, SEED_COUNT, STATIC_G, STATIC_K,
};
use num_complex::Complex64;
use rayon::prelude::*;

/// Quantization bit depth.
const QUANT_BITS: u32 = 8;

/// Maximum quantized value (127 for int8).
const MAX_QUANT: f32 = ((1_u32 << (QUANT_BITS - 1)) - 1) as f32;

/// Quantize residuals per-pair. Returns (quantized i8 as f32, scales\[P\]).
///
/// Each pair gets its own scale factor = max_abs / 127.
/// Zero-scale pairs (silence) produce zero output.
fn quantize(residuals: &[f32], length: usize, dim: usize) -> (Vec<f32>, Vec<f32>) {
    let p = dim / 2;
    let mut scales = vec![0.0_f32; p];

    // Find max abs per pair
    for t in 0..length {
        let base = t * dim;
        for j in 0..p {
            let re = residuals[base + j * 2].abs();
            let im = residuals[base + j * 2 + 1].abs();
            if re > scales[j] {
                scales[j] = re;
            }
            if im > scales[j] {
                scales[j] = im;
            }
        }
    }

    // Compute scale: max_abs / 127
    let mut inv_scales = vec![0.0_f32; p];
    for j in 0..p {
        if scales[j] > MACHINE_EPS as f32 {
            inv_scales[j] = MAX_QUANT / scales[j];
            scales[j] /= MAX_QUANT;
        }
    }

    // Quantize to int8 range, store as f32
    let mut quant = vec![0.0_f32; length * dim];
    for t in 0..length {
        let base = t * dim;
        for j in 0..p {
            let qre = (residuals[base + j * 2] * inv_scales[j])
                .round()
                .clamp(-MAX_QUANT, MAX_QUANT);
            let qim = (residuals[base + j * 2 + 1] * inv_scales[j])
                .round()
                .clamp(-MAX_QUANT, MAX_QUANT);
            quant[base + j * 2] = qre;
            quant[base + j * 2 + 1] = qim;
        }
    }

    (quant, scales)
}

/// Dequantize: reconstruct float residuals from quantized values + scales.
pub fn dequantize(quant: &[f32], scales: &[f32], length: usize, dim: usize) -> Vec<f32> {
    let p = dim / 2;
    let mut out = vec![0.0_f32; length * dim];

    for t in 0..length {
        let base = t * dim;
        for j in 0..p {
            out[base + j * 2] = quant[base + j * 2] * scales[j];
            out[base + j * 2 + 1] = quant[base + j * 2 + 1] * scales[j];
        }
    }

    out
}

/// Encode a single trajectory block.
///
/// Tries Cascaded (macro + micro oscillators) and Linear (constant velocity),
/// picks the mode with lower residual energy. Static and Raw are early exits.
pub fn encode_block(
    data: &[f32],  // [length × dim] row-major
    seed1: &[f32], // [dim] — z[n-1]
    seed2: &[f32], // [dim] — z[n-2]
    length: usize,
    dim: usize,
    micro_size: usize,
) -> Block {
    let p = dim / 2;
    let signal_energy = energy(data);

    // --- RAW fallback ---
    if length < SEED_COUNT + 1 {
        return Block {
            mode: Mode::Raw,
            start: 0,
            length,
            macro_k: vec![Complex64::ZERO; p],
            macro_g: vec![Complex64::ZERO; p],
            micro_ks: Vec::new(),
            micro_gs: Vec::new(),
            residuals: data.to_vec(),
            seed1: seed1.to_vec(),
            seed2: seed2.to_vec(),
            scales: vec![0.0; p],
            pair_rms: None,
            signal_energy,
            residual_energy: signal_energy,
            macro_capture: 0.0,
            micro_capture: 0.0,
        };
    }

    // --- STATIC detection ---
    let signal_scale_sq = signal_energy / (length * dim) as f64;
    let variance = compute_variance(data, length, dim);
    if variance < MACHINE_EPS * signal_scale_sq.max(MACHINE_EPS) {
        return Block {
            mode: Mode::Static,
            start: 0,
            length,
            macro_k: vec![STATIC_K; p],
            macro_g: vec![STATIC_G; p],
            micro_ks: Vec::new(),
            micro_gs: Vec::new(),
            residuals: vec![0.0; length * dim],
            seed1: seed1.to_vec(),
            seed2: seed2.to_vec(),
            scales: vec![0.0; p],
            pair_rms: None,
            signal_energy,
            residual_energy: 0.0,
            macro_capture: 100.0,
            micro_capture: 0.0,
        };
    }

    // --- Build extended sequence: [seed2, seed1, data] for fitting ---
    let ext_len = length + 2;
    let mut extended = Vec::with_capacity(ext_len * dim);
    extended.extend_from_slice(seed2);
    extended.extend_from_slice(seed1);
    extended.extend_from_slice(data);

    let z_c = to_complex(&extended, ext_len, dim);

    // --- MACRO fit ---
    let macro_fit = fit_all(&z_c, ext_len, p);

    // Seeds in complex domain (row 0 = seed2, row 1 = seed1)
    let s1_c: Vec<Complex64> = z_c[p..2 * p].to_vec();
    let s2_c: Vec<Complex64> = z_c[..p].to_vec();

    let macro_pred_c = predict_all(&s1_c, &s2_c, &macro_fit.k, &macro_fit.g, length);
    let macro_pred = from_complex(&macro_pred_c, length, p);

    // Macro residual
    let mut macro_resid = vec![0.0_f32; length * dim];
    for i in 0..length * dim {
        macro_resid[i] = data[i] - macro_pred[i];
    }
    let macro_resid_energy = energy(&macro_resid);
    let macro_capture = if signal_energy > MACHINE_EPS {
        ((1.0 - macro_resid_energy / signal_energy) * 100.0).max(0.0)
    } else {
        0.0
    };

    // --- MICRO fit on sub-blocks of macro residual ---
    let n_sub = (length / micro_size).max(1);
    let mut micro_ks = Vec::with_capacity(n_sub);
    let mut micro_gs = Vec::with_capacity(n_sub);
    let mut micro_pred = vec![0.0_f32; length * dim];

    for si in 0..n_sub {
        let ss = si * micro_size;
        let se = (ss + micro_size).min(length);
        let sl = se - ss;
        if sl < 1 {
            continue;
        }

        // Micro seeds from macro residuals (causal)
        let (ms2, ms1) = micro_seeds(&macro_resid, ss, dim);

        // Build sub-sequence: [ms2, ms1, sub_block]
        let sub_ext_len = sl + 2;
        let mut sub_ext = Vec::with_capacity(sub_ext_len * dim);
        sub_ext.extend_from_slice(&ms2);
        sub_ext.extend_from_slice(&ms1);
        sub_ext.extend_from_slice(&macro_resid[ss * dim..se * dim]);

        let sub_z = to_complex(&sub_ext, sub_ext_len, dim);
        let sub_fit = fit_all(&sub_z, sub_ext_len, p);

        // Predict micro
        let ms1_c = to_complex(&ms1, 1, dim);
        let ms2_c = to_complex(&ms2, 1, dim);
        let mp_c = predict_all(&ms1_c, &ms2_c, &sub_fit.k, &sub_fit.g, sl);
        let mp = from_complex(&mp_c, sl, p);

        micro_pred[ss * dim..se * dim].copy_from_slice(&mp);
        micro_ks.push(sub_fit.k);
        micro_gs.push(sub_fit.g);
    }

    // Cascaded residual
    let mut cascaded_resid = vec![0.0_f32; length * dim];
    for i in 0..length * dim {
        cascaded_resid[i] = macro_resid[i] - micro_pred[i];
    }
    let cascaded_energy = energy(&cascaded_resid);
    let micro_capture = if macro_resid_energy > MACHINE_EPS {
        ((1.0 - cascaded_energy / macro_resid_energy) * 100.0).max(0.0)
    } else {
        0.0
    };

    // --- LINEAR comparison ---
    let linear_k = vec![LINEAR_K; p];
    let linear_g = vec![LINEAR_G; p];
    let linear_pred_c = predict_all(&s1_c, &s2_c, &linear_k, &linear_g, length);
    let linear_pred = from_complex(&linear_pred_c, length, p);
    let mut linear_resid = vec![0.0_f32; length * dim];
    for i in 0..length * dim {
        linear_resid[i] = data[i] - linear_pred[i];
    }
    let linear_energy = energy(&linear_resid);

    // --- MODE SELECTION ---
    if cascaded_energy <= linear_energy {
        let pair_rms = compute_pair_rms(&cascaded_resid, length, dim);
        let (quant, scales) = quantize(&cascaded_resid, length, dim);
        let quant_energy = energy(&quant); // approx, not dequantized
        let _ = quant_energy; // used for debugging

        Block {
            mode: Mode::Cascaded,
            start: 0,
            length,
            macro_k: macro_fit.k,
            macro_g: macro_fit.g,
            micro_ks,
            micro_gs,
            residuals: quant,
            seed1: seed1.to_vec(),
            seed2: seed2.to_vec(),
            scales,
            pair_rms: Some(pair_rms),
            signal_energy,
            residual_energy: cascaded_energy,
            macro_capture,
            micro_capture,
        }
    } else {
        let pair_rms = compute_pair_rms(&linear_resid, length, dim);
        let (quant, scales) = quantize(&linear_resid, length, dim);

        Block {
            mode: Mode::Linear,
            start: 0,
            length,
            macro_k: linear_k,
            macro_g: linear_g,
            micro_ks: Vec::new(),
            micro_gs: Vec::new(),
            residuals: quant,
            seed1: seed1.to_vec(),
            seed2: seed2.to_vec(),
            scales,
            pair_rms: Some(pair_rms),
            signal_energy,
            residual_energy: linear_energy,
            macro_capture: 0.0,
            micro_capture: 0.0,
        }
    }
}

/// Extract causal micro seeds from macro residuals at sub-block start `ss`.
fn micro_seeds(macro_resid: &[f32], ss: usize, dim: usize) -> (Vec<f32>, Vec<f32>) {
    if ss >= SEED_COUNT {
        let s2 = macro_resid[(ss - 2) * dim..(ss - 1) * dim].to_vec();
        let s1 = macro_resid[(ss - 1) * dim..ss * dim].to_vec();
        (s2, s1)
    } else if ss == 1 {
        let s2 = vec![0.0_f32; dim];
        let s1 = macro_resid[0..dim].to_vec();
        (s2, s1)
    } else {
        (vec![0.0_f32; dim], vec![0.0_f32; dim])
    }
}

/// Variance of data (sum of squared deviations from mean, normalized).
fn compute_variance(data: &[f32], length: usize, dim: usize) -> f64 {
    let n = (length * dim) as f64;
    let mean: f64 = data.iter().map(|&x| x as f64).sum::<f64>() / n;
    data.iter()
        .map(|&x| {
            let d = x as f64 - mean;
            d * d
        })
        .sum::<f64>()
        / n
}

/// Encode a full trajectory into a Packet.
///
/// The main entry point: pairing → segmentation → block encoding → assembly.
pub fn encode(
    trajectory: &[f32], // [T × D] row-major
    t: usize,
    dim: usize,
    pairing: Option<&[i32]>,
) -> Packet {
    let p = dim / 2;

    if t == 0 {
        return Packet {
            dim,
            pairs: p,
            length: 0,
            alpha: 1.0,
            macro_block: MIN_BLOCK,
            micro_block: MIN_BLOCK,
            pairing: (0..dim as i32).collect(),
            blocks: Vec::new(),
        };
    }

    // Derive or use given pairing
    let owned_pairing;
    let pair_ref = match pairing {
        Some(p) => p,
        None => {
            owned_pairing = derive_pairing(trajectory, t, dim);
            &owned_pairing
        }
    };

    // Apply pairing to reorder dimensions
    let wp = apply_pairing(trajectory, t, dim, pair_ref);

    // Edge case: too short for real encoding
    if t <= SEED_COUNT {
        let blk = Block {
            mode: Mode::Raw,
            start: 0,
            length: t,
            macro_k: vec![Complex64::ZERO; p],
            macro_g: vec![Complex64::ZERO; p],
            micro_ks: Vec::new(),
            micro_gs: Vec::new(),
            residuals: wp.clone(),
            seed1: if t >= 1 {
                wp[..dim].to_vec()
            } else {
                vec![0.0; dim]
            },
            seed2: vec![0.0; dim],
            scales: vec![0.0; p],
            pair_rms: None,
            signal_energy: energy(&wp),
            residual_energy: energy(&wp),
            macro_capture: 0.0,
            micro_capture: 0.0,
        };
        return Packet {
            dim,
            pairs: p,
            length: t,
            alpha: 1.0,
            macro_block: MIN_BLOCK,
            micro_block: MIN_BLOCK,
            pairing: pair_ref.to_vec(),
            blocks: vec![blk],
        };
    }

    let (macro_size, micro_size) = derive_block_sizes(&wp, t, dim);
    let segments = segment(&wp, t, dim, macro_size);

    // Parallel block encoding across segments (rayon).
    // Each segment is independent: its own data slice, its own seeds.
    let blocks: Vec<Block> = segments
        .par_iter()
        .map(|(ss, se)| {
            let length = se - ss;
            let data = &wp[ss * dim..se * dim];

            let seed1 = if *ss >= 1 {
                &wp[(ss - 1) * dim..ss * dim]
            } else {
                &wp[..dim]
            };
            let seed2 = if *ss >= 2 {
                &wp[(ss - 2) * dim..(ss - 1) * dim]
            } else if *ss == 1 {
                &wp[..dim]
            } else {
                seed1
            };

            let mut blk = encode_block(data, seed1, seed2, length, dim, micro_size);
            blk.start = *ss;
            blk
        })
        .collect();

    Packet {
        dim,
        pairs: p,
        length: t,
        alpha: 1.0,
        macro_block: macro_size,
        micro_block: micro_size,
        pairing: pair_ref.to_vec(),
        blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn quantize_dequantize_roundtrip() {
        let length = 10;
        let dim = 4;
        let resid: Vec<f32> = (0..length * dim)
            .map(|i| ((i as f32) * 0.37).sin() * 5.0)
            .collect();

        let (quant, scales) = quantize(&resid, length, dim);
        let back = dequantize(&quant, &scales, length, dim);

        // Quantization error should be bounded by scale / 127
        for (a, b) in resid.iter().zip(back.iter()) {
            let err = (a - b).abs();
            let max_err = scales.iter().copied().fold(0.0_f32, f32::max) * 2.0;
            assert!(err < max_err + 1e-5, "err = {}, max = {}", err, max_err);
        }
    }

    #[test]
    fn encode_block_static() {
        let dim = 4;
        let length = 20;
        let data = vec![0.0_f32; length * dim];
        let seed1 = vec![0.0_f32; dim];
        let seed2 = vec![0.0_f32; dim];

        let blk = encode_block(&data, &seed1, &seed2, length, dim, 4);
        assert_eq!(blk.mode, Mode::Static);
        assert!((blk.macro_capture - 100.0).abs() < 1e-6);
    }

    #[test]
    fn encode_block_raw() {
        let dim = 4;
        let length = 2; // too short
        let data = vec![1.0_f32; length * dim];
        let seed1 = vec![0.0_f32; dim];
        let seed2 = vec![0.0_f32; dim];

        let blk = encode_block(&data, &seed1, &seed2, length, dim, 4);
        assert_eq!(blk.mode, Mode::Raw);
    }

    #[test]
    fn encode_block_cosine() {
        let dim = 4;
        let t = 50;
        let freq = 2.0 * PI / 10.0;

        let mut w = vec![0.0_f32; t * dim];
        for i in 0..t {
            let phase = freq * i as f64;
            w[i * dim] = phase.cos() as f32;
            w[i * dim + 1] = phase.sin() as f32;
            w[i * dim + 2] = (phase * 0.5).cos() as f32;
            w[i * dim + 3] = (phase * 0.5).sin() as f32;
        }

        let length = t - 2;
        let data = &w[2 * dim..];
        let seed1 = &w[dim..2 * dim];
        let seed2 = &w[..dim];

        let blk = encode_block(data, seed1, seed2, length, dim, 8);
        assert!(blk.mode == Mode::Cascaded || blk.mode == Mode::Linear);
        assert!(
            blk.macro_capture > 50.0,
            "cosine should have high capture: {}%",
            blk.macro_capture
        );
    }

    #[test]
    fn encode_full_trajectory() {
        let dim = 6;
        let t = 100;
        let freq = 2.0 * PI / 20.0;

        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                let col = i % dim;
                (freq * row as f64 + col as f64 * 0.5).sin() as f32
            })
            .collect();

        let packet = encode(&w, t, dim, None);
        assert_eq!(packet.dim, dim);
        assert_eq!(packet.length, t);
        assert!(!packet.blocks.is_empty());
        assert!(packet.capture() > 0.0);
    }

    #[test]
    fn encode_tiny() {
        let w = vec![1.0_f32, 2.0, 3.0, 4.0]; // 1 sample, 4 dims
        let packet = encode(&w, 1, 4, None);
        assert_eq!(packet.length, 1);
        assert_eq!(packet.blocks.len(), 1);
        assert_eq!(packet.blocks[0].mode, Mode::Raw);
    }

    #[test]
    fn encode_empty() {
        let packet = encode(&[], 0, 4, None);
        assert_eq!(packet.length, 0);
        assert!(packet.blocks.is_empty());
    }
}
