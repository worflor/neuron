// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Streaming encoder: incremental block emission.
//!
//! Buffers samples and emits blocks on buffer full or phase transition.
//! Tracks linear prediction error statistics for adaptive threshold.

use crate::encode::encode_block;
use crate::segment::derive_block_sizes;
use crate::types::{Block, MIN_BLOCK, SEED_COUNT};

/// Incremental streaming encoder.
///
/// Push one sample at a time, get blocks out when they're ready.
pub struct StreamEncoder {
    dim: usize,
    micro_size: usize,

    /// Sample buffer: accumulates rows of [dim].
    buf: Vec<f32>,

    /// Maximum block length (derived from first buffer fill).
    max_block: usize,

    /// Phase transition threshold (3-sigma rule).
    threshold: f64,

    /// Running error statistics for threshold computation.
    err_sum: f64,
    err_sq_sum: f64,
    err_count: usize,

    /// Number of emitted data samples, excluding the overlapping seed rows.
    emitted_samples: usize,

    /// Whether block sizes have been calibrated.
    calibrated: bool,
}

impl StreamEncoder {
    /// Create a new streaming encoder for dimension D.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            micro_size: MIN_BLOCK,
            buf: Vec::new(),
            max_block: MIN_BLOCK,
            threshold: f64::INFINITY,
            err_sum: 0.0,
            err_sq_sum: 0.0,
            err_count: 0,
            emitted_samples: 0,
            calibrated: false,
        }
    }

    /// Number of buffered samples.
    #[must_use]
    pub fn buffered(&self) -> usize {
        self.buf.len() / self.dim
    }

    /// Push a single sample \[dim\]. Returns a Block if one was emitted.
    pub fn push(&mut self, sample: &[f32]) -> Option<Block> {
        debug_assert_eq!(sample.len(), self.dim);
        self.buf.extend_from_slice(sample);
        let t = self.buffered();
        let dim = self.dim;

        if t < SEED_COUNT + MIN_BLOCK {
            return None;
        }

        // Calibrate block sizes on first fill
        if !self.calibrated && t == SEED_COUNT + MIN_BLOCK {
            let (mac, mic) = derive_block_sizes(&self.buf, t, dim);
            self.max_block = mac;
            self.micro_size = mic;
            self.calibrated = true;
        }

        // Track linear prediction error
        if t >= SEED_COUNT + 2 {
            let curr = &self.buf[(t - 1) * dim..t * dim];
            let prev1 = &self.buf[(t - 2) * dim..(t - 1) * dim];
            let prev2 = &self.buf[(t - 3) * dim..(t - 2) * dim];

            let mut sq = 0.0_f64;
            for d in 0..dim {
                let pred = 2.0 * f64::from(prev1[d]) - f64::from(prev2[d]);
                let diff = f64::from(curr[d]) - pred;
                sq = diff.mul_add(diff, sq);
            }
            let err = sq.sqrt();

            self.err_sum += err;
            self.err_sq_sum += err * err;
            self.err_count += 1;

            // Update threshold from statistics
            if self.err_count >= 3 {
                let mean = self.err_sum / self.err_count as f64;
                let var = (self.err_sq_sum / self.err_count as f64) - mean * mean;
                self.threshold = mean + 3.0 * var.max(0.0).sqrt();
            }
        }

        // Decide whether to emit
        let block_len = t - SEED_COUNT;
        let mut should_emit = block_len >= self.max_block;

        if !should_emit && block_len >= MIN_BLOCK && self.err_count > 0 {
            // Check last error against threshold
            let curr = &self.buf[(t - 1) * dim..t * dim];
            let prev1 = &self.buf[(t - 2) * dim..(t - 1) * dim];
            let prev2 = &self.buf[(t - 3) * dim..(t - 2) * dim];

            let mut sq = 0.0_f64;
            for d in 0..dim {
                let pred = 2.0 * f64::from(prev1[d]) - f64::from(prev2[d]);
                let diff = f64::from(curr[d]) - pred;
                sq = diff.mul_add(diff, sq);
            }
            if sq.sqrt() > self.threshold {
                should_emit = true;
            }
        }

        if should_emit { Some(self.emit()) } else { None }
    }

    /// Flush remaining buffer. Returns a Block if enough data remains.
    pub fn flush(&mut self) -> Option<Block> {
        if self.buffered() > SEED_COUNT {
            Some(self.emit())
        } else {
            None
        }
    }

    /// Emit a block from the current buffer and retain last 2 samples as seeds.
    fn emit(&mut self) -> Block {
        let t = self.buffered();
        let dim = self.dim;
        let length = t - SEED_COUNT;

        let seed2 = &self.buf[0..dim];
        let seed1 = &self.buf[dim..2 * dim];
        let data = &self.buf[SEED_COUNT * dim..];

        let mut blk = encode_block(data, seed1, seed2, length, dim, self.micro_size);
        blk.start = self.emitted_samples + SEED_COUNT;

        // Keep last two samples as seeds for next block
        let keep_start = (t - 2) * dim;
        let kept: Vec<f32> = self.buf[keep_start..].to_vec();
        self.buf.clear();
        self.buf.extend_from_slice(&kept);

        self.emitted_samples += length;
        blk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn streaming_emits_blocks() {
        let dim = 4;
        let mut enc = StreamEncoder::new(dim);
        let mut blocks = Vec::new();

        for i in 0..200 {
            let sample: Vec<f32> = (0..dim)
                .map(|d| (0.3 * f64::from(i) + d as f64 * 0.5).sin() as f32)
                .collect();

            if let Some(blk) = enc.push(&sample) {
                blocks.push(blk);
            }
        }

        if let Some(blk) = enc.flush() {
            blocks.push(blk);
        }

        assert!(!blocks.is_empty(), "should emit at least one block");
        for blk in &blocks {
            assert!(blk.length >= MIN_BLOCK || blk.length > 0);
        }
    }

    #[test]
    fn streaming_handles_phase_transition() {
        let dim = 4;
        let mut enc = StreamEncoder::new(dim);
        let mut blocks = Vec::new();

        // Smooth signal for 80 samples
        for i in 0..80 {
            let sample: Vec<f32> = (0..dim)
                .map(|d| (0.1 * f64::from(i) + d as f64 * 0.2).sin() as f32)
                .collect();
            if let Some(blk) = enc.push(&sample) {
                blocks.push(blk);
            }
        }

        // Sudden jump
        for i in 80..160 {
            let sample: Vec<f32> = (0..dim)
                .map(|d| 100.0 + (2.0 * f64::from(i) + d as f64).cos() as f32)
                .collect();
            if let Some(blk) = enc.push(&sample) {
                blocks.push(blk);
            }
        }

        if let Some(blk) = enc.flush() {
            blocks.push(blk);
        }

        assert!(
            blocks.len() >= 2,
            "should detect at least one phase transition"
        );
    }

    #[test]
    fn streaming_flush_empty() {
        let mut enc = StreamEncoder::new(4);
        assert!(enc.flush().is_none());
    }

    #[test]
    fn streaming_single_block() {
        let dim = 4;
        let mut enc = StreamEncoder::new(dim);
        let mut blocks = Vec::new();

        for i in 0..10 {
            let sample = vec![i as f32 * 0.1; dim];
            if let Some(blk) = enc.push(&sample) {
                blocks.push(blk);
            }
        }
        if let Some(blk) = enc.flush() {
            blocks.push(blk);
        }

        assert!(
            !blocks.is_empty(),
            "should emit at least one block, got {}",
            blocks.len()
        );
    }

    #[test]
    fn phase_short_block_keeps_following_start_contiguous() {
        let dim = 4;
        let mut enc = StreamEncoder::new(dim);
        enc.calibrated = true;
        enc.max_block = MIN_BLOCK + 10;
        enc.threshold = 0.0;
        enc.err_count = 1;

        let mut blocks = Vec::new();
        for i in 0..(SEED_COUNT + MIN_BLOCK + enc.max_block) {
            let value = if i == SEED_COUNT + MIN_BLOCK - 1 { 10.0 } else { 0.0 };
            let sample = vec![value; dim];
            if let Some(block) = enc.push(&sample) {
                blocks.push(block);
            }
        }

        assert!(blocks.len() >= 2, "expected a short block and its successor");
        assert_eq!(blocks[0].length, MIN_BLOCK);
        assert_eq!(blocks[1].start, blocks[0].start + blocks[0].length);
    }
}
