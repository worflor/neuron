// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Block decoding: the inverse of encode.
//!
//! Reconstructs the trajectory from oscillator parameters + quantized residuals.
//! decode_block is purely causal: micro seeds come from already-decoded data.

use crate::encode::dequantize;
use crate::predict::{from_complex, predict_all, to_complex};
use crate::segment::undo_pairing;
use crate::types::{Block, MIN_BLOCK, Mode, Packet, SEED_COUNT};

/// Decode a single block back to a trajectory segment \[length × dim\].
pub fn decode_block(block: &Block, dim: usize) -> Vec<f32> {
    let length = block.length;
    let p = dim / 2;

    match block.mode {
        Mode::Raw => block.residuals[..length * dim].to_vec(),

        Mode::Static => {
            // Repeat seed1 for every timestep
            let mut out = Vec::with_capacity(length * dim);
            for _ in 0..length {
                out.extend_from_slice(&block.seed1);
            }
            out
        }

        Mode::Linear | Mode::Cascaded => {
            // Macro prediction
            let s1_c = to_complex(&block.seed1, 1, dim);
            let s2_c = to_complex(&block.seed2, 1, dim);
            let macro_pred_c = predict_all(&s1_c, &s2_c, &block.macro_k, &block.macro_g, length);
            let macro_pred = from_complex(&macro_pred_c, length, p);

            // Dequantize residuals
            let resid = dequantize(&block.residuals, &block.scales, length, dim);

            if block.mode == Mode::Linear || block.micro_ks.is_empty() {
                // macro + residuals
                let mut out = vec![0.0_f32; length * dim];
                for i in 0..length * dim {
                    out[i] = macro_pred[i] + resid[i];
                }
                return out;
            }

            // Cascaded: spin up micro oscillators on sub-blocks
            let n_sub = block.micro_ks.len();
            let micro_size = if n_sub > 0 {
                (length / n_sub).max(MIN_BLOCK)
            } else {
                length
            };
            let mut micro_pred = vec![0.0_f32; length * dim];

            for si in 0..n_sub {
                let ss = si * micro_size;
                let se = (ss + micro_size).min(length);
                let sl = se - ss;
                if sl < 1 {
                    continue;
                }

                // Causal micro seeds from reconstructed macro_resid = micro_pred + resid
                let (ms2, ms1) = if ss >= SEED_COUNT {
                    let mut s2 = vec![0.0_f32; dim];
                    let mut s1 = vec![0.0_f32; dim];
                    for d in 0..dim {
                        s2[d] = micro_pred[(ss - 2) * dim + d] + resid[(ss - 2) * dim + d];
                        s1[d] = micro_pred[(ss - 1) * dim + d] + resid[(ss - 1) * dim + d];
                    }
                    (s2, s1)
                } else if ss == 1 {
                    let s2 = vec![0.0_f32; dim];
                    let mut s1 = vec![0.0_f32; dim];
                    for d in 0..dim {
                        s1[d] = micro_pred[d] + resid[d];
                    }
                    (s2, s1)
                } else {
                    (vec![0.0_f32; dim], vec![0.0_f32; dim])
                };

                let ms1_c = to_complex(&ms1, 1, dim);
                let ms2_c = to_complex(&ms2, 1, dim);
                let mp_c =
                    predict_all(&ms1_c, &ms2_c, &block.micro_ks[si], &block.micro_gs[si], sl);
                let mp = from_complex(&mp_c, sl, p);
                micro_pred[ss * dim..se * dim].copy_from_slice(&mp);
            }

            // macro + micro + residuals
            let mut out = vec![0.0_f32; length * dim];
            for i in 0..length * dim {
                out[i] = macro_pred[i] + micro_pred[i] + resid[i];
            }
            out
        }
    }
}

/// Decode a full Packet back to trajectory \[T × D\].
pub fn decode(packet: &Packet) -> Vec<f32> {
    if packet.length == 0 {
        return Vec::new();
    }

    let dim = packet.dim;
    let mut out = vec![0.0_f32; packet.length * dim];

    for blk in &packet.blocks {
        let decoded = decode_block(blk, dim);
        let start = blk.start * dim;
        let end = start + blk.length * dim;
        out[start..end].copy_from_slice(&decoded[..blk.length * dim]);
    }

    // Restore seed samples for first block
    if let Some(first) = packet.blocks.first()
        && first.start >= SEED_COUNT
    {
        out[..dim].copy_from_slice(&first.seed2);
        out[dim..2 * dim].copy_from_slice(&first.seed1);
    }

    // Undo pairing to restore original dimension order
    undo_pairing(&out, packet.length, dim, &packet.pairing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{encode, encode_block};
    use std::f64::consts::PI;

    #[test]
    fn decode_raw_block() {
        let dim = 4;
        let data = vec![1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]; // 2 samples
        let seed1 = vec![0.0_f32; dim];
        let seed2 = vec![0.0_f32; dim];

        let blk = encode_block(&data, &seed1, &seed2, 2, dim, 4);
        let decoded = decode_block(&blk, dim);
        assert_eq!(decoded, data);
    }

    #[test]
    fn decode_static_block() {
        let dim = 4;
        let length = 10;
        let data = vec![0.0_f32; length * dim];
        let seed1 = vec![0.0_f32; dim];
        let seed2 = vec![0.0_f32; dim];

        let blk = encode_block(&data, &seed1, &seed2, length, dim, 4);
        assert_eq!(blk.mode, Mode::Static);
        let decoded = decode_block(&blk, dim);
        assert_eq!(decoded.len(), length * dim);
        for &v in &decoded {
            assert!(v.abs() < 1e-6);
        }
    }

    #[test]
    fn encode_decode_cosine_roundtrip() {
        let dim = 4;
        let t = 60;
        let freq = 2.0 * PI / 12.0;

        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                let col = i % dim;
                (freq * row as f64 + col as f64 * 0.7).sin() as f32
            })
            .collect();

        let packet = encode(&w, t, dim, None);
        let decoded = decode(&packet);

        assert_eq!(decoded.len(), w.len());

        // Roundtrip error should be bounded (quantization + prediction)
        let mut max_err = 0.0_f32;
        for (a, b) in w.iter().zip(decoded.iter()) {
            let err = (a - b).abs();
            if err > max_err {
                max_err = err;
            }
        }

        // With 8-bit quantization, error is bounded
        assert!(
            max_err < 1.0,
            "max roundtrip error = {} (should be small for smooth signal)",
            max_err
        );
    }

    #[test]
    fn encode_decode_ramp_roundtrip() {
        let dim = 6;
        let t = 80;

        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                (row as f32) * 0.01
            })
            .collect();

        let packet = encode(&w, t, dim, None);
        let decoded = decode(&packet);

        let mut max_err = 0.0_f32;
        for (a, b) in w.iter().zip(decoded.iter()) {
            let err = (a - b).abs();
            if err > max_err {
                max_err = err;
            }
        }

        // Linear ramp should be nearly perfect (linear mode)
        assert!(max_err < 0.5, "ramp roundtrip error = {}", max_err);
    }

    #[test]
    fn encode_decode_preserves_energy() {
        let dim = 4;
        let t = 100;
        let freq = 2.0 * PI / 16.0;

        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                let col = i % dim;
                (freq * row as f64 * (col + 1) as f64).cos() as f32
            })
            .collect();

        let orig_energy: f64 = w.iter().map(|&x| (x as f64) * (x as f64)).sum();

        let packet = encode(&w, t, dim, None);
        let decoded = decode(&packet);
        let dec_energy: f64 = decoded.iter().map(|&x| (x as f64) * (x as f64)).sum();

        // Energy should be within 50% (quantization loses some)
        let ratio = dec_energy / orig_energy;
        assert!(
            ratio > 0.5 && ratio < 2.0,
            "energy ratio = {} (should be ~1.0)",
            ratio
        );
    }
}
