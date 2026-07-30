// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! ByteHistogram: zero-dependency text embedding.
//!
//! Byte frequency histograms over 512-byte windows.
//! 256 dimensions, one per byte value. No external model needed.
//! Domain-agnostic: works on any byte stream in any language.

/// Default chunk size in bytes.
const DEFAULT_CHUNK: usize = 512;

/// Minimum chunks required for a valid trajectory.
const MIN_CHUNKS: usize = 4;

/// Convert text (UTF-8 bytes) to a trajectory of byte frequency histograms.
///
/// Returns \[T × 256\] f32 row-major, where T = floor(len / chunk_size).
/// Returns empty vec if text is too short (< chunk_size * MIN_CHUNKS bytes).
pub fn text_to_trajectory(text: &str, chunk_size: usize) -> (Vec<f32>, usize) {
    let data = text.as_bytes();
    bytes_to_trajectory(data, chunk_size)
}

/// Convert raw bytes to a trajectory of byte frequency histograms.
pub fn bytes_to_trajectory(data: &[u8], chunk_size: usize) -> (Vec<f32>, usize) {
    let cs = if chunk_size == 0 {
        DEFAULT_CHUNK
    } else {
        chunk_size
    };
    let n_chunks = data.len() / cs;

    if n_chunks < MIN_CHUNKS {
        return (Vec::new(), 0);
    }

    let dim = 256;
    let t = n_chunks;
    let inv_cs = 1.0 / cs as f32;
    let mut out = vec![0.0_f32; t * dim];

    for chunk_idx in 0..t {
        let start = chunk_idx * cs;
        let row_base = chunk_idx * dim;

        // Count byte frequencies in this chunk
        for i in 0..cs {
            let byte = data[start + i] as usize;
            out[row_base + byte] += 1.0;
        }

        // Normalize to frequencies
        for d in 0..dim {
            out[row_base + d] *= inv_cs;
        }
    }

    (out, t)
}

/// Default chunk size.
pub const fn default_chunk_size() -> usize {
    DEFAULT_CHUNK
}

/// Embedding dimension (always 256).
pub const fn embedding_dim() -> usize {
    256
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text() {
        let (traj, t) = text_to_trajectory("", DEFAULT_CHUNK);
        assert_eq!(t, 0);
        assert!(traj.is_empty());
    }

    #[test]
    fn short_text() {
        let (traj, t) = text_to_trajectory("hello", DEFAULT_CHUNK);
        assert_eq!(t, 0);
        assert!(traj.is_empty());
    }

    #[test]
    fn sufficient_text() {
        // Need >= 4 * 512 = 2048 bytes
        let text = "a".repeat(2048);
        let (traj, t) = text_to_trajectory(&text, DEFAULT_CHUNK);
        assert_eq!(t, 4);
        assert_eq!(traj.len(), 4 * 256);

        // All 'a' = byte 97, so hist[97] should be 1.0
        assert!((traj[97] - 1.0).abs() < 1e-5);
        // Other bytes should be 0
        assert!(traj[0].abs() < 1e-5);
        assert!(traj[98].abs() < 1e-5);
    }

    #[test]
    fn frequencies_sum_to_one() {
        let text = "The quick brown fox jumps over the lazy dog. ".repeat(100);
        let (traj, t) = text_to_trajectory(&text, DEFAULT_CHUNK);
        assert!(t > 0);

        for chunk in 0..t {
            let sum: f32 = (0..256).map(|d| traj[chunk * 256 + d]).sum();
            assert!((sum - 1.0).abs() < 1e-5, "chunk {} sum = {}", chunk, sum);
        }
    }

    #[test]
    fn different_languages_different_histograms() {
        let english =
            "The quick brown fox jumps over the lazy dog repeatedly forever and ever. ".repeat(40);
        let binary = (0..2560_u16).map(|i| (i % 256) as u8).collect::<Vec<u8>>();

        let (eng_traj, eng_t) = text_to_trajectory(&english, DEFAULT_CHUNK);
        let (bin_traj, bin_t) = bytes_to_trajectory(&binary, DEFAULT_CHUNK);

        assert!(eng_t > 0);
        assert!(bin_t > 0);

        // English should have most mass in ASCII range
        let eng_ascii_mass: f32 = (32..127).map(|d| eng_traj[d]).sum();
        assert!(
            eng_ascii_mass > 0.9,
            "english ascii mass = {}",
            eng_ascii_mass
        );

        // Uniform binary should be flat
        let bin_max: f32 = (0..256).map(|d| bin_traj[d]).fold(0.0_f32, f32::max);
        let bin_min: f32 = (0..256).map(|d| bin_traj[d]).fold(1.0_f32, f32::min);
        assert!((bin_max - bin_min) < 0.01, "binary should be ~uniform");
    }
}
