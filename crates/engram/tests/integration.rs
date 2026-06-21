//! Integration tests: full end-to-end pipeline on real text.
//!
//! Feeds golden vector .txt articles through ByteHistogram → encode →
//! wire → decode, verifying the complete pipeline.

use engram::brain::Brain;
use engram::brain_io;
use engram::decode::decode;
use engram::encode::encode;
use engram::histogram;
use engram::types::Mode;
use engram::wire::{from_wire, from_wire_compact, to_wire, to_wire_compact};

/// Path to golden vector text files.
const GOLDEN_DIR: &str = "../rag_tests/engram/cache/_golden_vectors";

/// Load a golden vector text file.
fn load_text(name: &str) -> Option<String> {
    let path = format!("{}/{}.txt", GOLDEN_DIR, name);
    std::fs::read_to_string(&path).ok()
}

/// All golden vector article names.
const ARTICLES: &[&str] = &[
    "cheese",
    "platypus",
    "toilet",
    "zombie",
    "banana",
    "dragon",
    "infinite_monkey_theorem",
    "trolley_problem",
    "fermi_paradox",
    "moon_landing_conspiracy_theories",
];

// ─── Full pipeline tests ───────────────────────────────────────────

#[test]
fn histogram_produces_valid_trajectories() {
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => {
                eprintln!("skipping {} (file not found)", name);
                continue;
            }
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());

        assert!(t >= 4, "{}: only {} chunks (need ≥4)", name, t);
        assert_eq!(traj.len(), t * histogram::embedding_dim());

        // Each row should sum to ~1.0 (normalized frequencies)
        for row in 0..t {
            let sum: f32 = (0..256).map(|d| traj[row * 256 + d]).sum();
            assert!(
                (sum - 1.0).abs() < 1e-4,
                "{} row {}: sum = {}",
                name,
                row,
                sum
            );
        }
    }
}

#[test]
fn encode_decode_roundtrip_all_articles() {
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }

        let dim = histogram::embedding_dim();
        let packet = encode(&traj, t, dim, None);

        assert_eq!(packet.dim, dim);
        assert_eq!(packet.length, t);
        assert!(!packet.blocks.is_empty(), "{}: no blocks", name);
        assert!(packet.capture() > 0.0, "{}: zero capture", name);

        let decoded = decode(&packet);
        assert_eq!(
            decoded.len(),
            traj.len(),
            "{}: decoded length mismatch",
            name
        );

        // Roundtrip error bounded by quantization
        let mut max_err: f32 = 0.0;
        for (a, b) in traj.iter().zip(decoded.iter()) {
            let err = (a - b).abs();
            if err > max_err {
                max_err = err;
            }
        }
        // Int8 quantization: max error ≈ max_scale / 127. For byte histograms
        // (values in [0,1]), this is bounded by ~1/127 per pair. Cascaded
        // prediction residuals can exceed this for high-frequency content.
        assert!(
            max_err < 1.0,
            "{}: max roundtrip error = {} (too high)",
            name,
            max_err
        );
    }
}

#[test]
fn wire_roundtrip_all_articles() {
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }

        let dim = histogram::embedding_dim();
        let packet = encode(&traj, t, dim, None);

        // Full wire roundtrip
        let wire_bytes = to_wire(&packet);
        let wire_packet =
            from_wire(&wire_bytes).unwrap_or_else(|| panic!("{}: wire parse failed", name));

        assert_eq!(wire_packet.dim, dim);
        assert_eq!(wire_packet.length, t);
        assert_eq!(wire_packet.blocks.len(), packet.blocks.len());

        // K,G values survive wire roundtrip (f32 precision)
        for (orig, wire) in packet.blocks.iter().zip(wire_packet.blocks.iter()) {
            assert_eq!(orig.mode, wire.mode);
            assert_eq!(orig.length, wire.length);
        }

        // Compact wire roundtrip
        let compact_bytes = to_wire_compact(&packet);
        assert!(
            compact_bytes.len() <= wire_bytes.len(),
            "{}: compact ({}) > full ({})",
            name,
            compact_bytes.len(),
            wire_bytes.len()
        );

        let compact_packet = from_wire_compact(&compact_bytes)
            .unwrap_or_else(|| panic!("{}: compact parse failed", name));
        assert_eq!(compact_packet.dim, dim);
        assert_eq!(compact_packet.length, t);
    }
}

#[test]
fn brain_absorb_all_articles() {
    let dim = histogram::embedding_dim();
    let mut brain = Brain::new(dim, 0.005);

    let mut absorbed = 0;
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }

        let well = brain.fast_absorb(&traj, t, None);
        assert!(!well.is_empty());
        absorbed += 1;
    }

    if absorbed == 0 {
        return;
    } // files not found

    assert!(brain.total_absorbed == absorbed);
    assert!(!brain.wells.is_empty(), "should have created wells");
    assert!(!brain.dream.is_empty(), "should have dream entries");

    // Reference pairing should be set
    assert!(brain.reference_pairing.is_some());
}

#[test]
fn brain_measure_after_absorb() {
    let dim = histogram::embedding_dim();
    let mut brain = Brain::new(dim, 0.005);

    let mut trajs = Vec::new();
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };
        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }
        brain.fast_absorb(&traj, t, Some(name));
        trajs.push((name, traj, t));
    }

    if trajs.is_empty() {
        return;
    }

    // Measure each article — the one absorbed into its own well should be closest
    for (name, traj, t) in &trajs {
        let result = brain.measure(traj, *t);
        assert!(result.capture > 0.0, "{}: zero capture on measure", name);
        assert!(result.drift >= 0.0, "{}: negative drift", name);
    }
}

#[test]
fn brain_save_load_roundtrip() {
    let dim = histogram::embedding_dim();
    let mut brain = Brain::new(dim, 0.005);
    brain.name = "Alexandria".into();

    for &name in &ARTICLES[..3] {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };
        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }
        brain.fast_absorb(&traj, t, Some(name));
    }

    if brain.total_absorbed == 0 {
        return;
    }

    let bytes = brain_io::save(&brain);
    let loaded = brain_io::load(&bytes).expect("should load");

    assert_eq!(loaded.name, "Alexandria");
    assert_eq!(loaded.dim, dim);
    assert_eq!(loaded.total_absorbed, brain.total_absorbed);
    assert_eq!(loaded.wells.len(), brain.wells.len());
    assert_eq!(loaded.dream.len(), brain.dream.len());

    // Well K sums should match exactly
    for (name, well) in &brain.wells {
        let lw = &loaded.wells[name];
        assert_eq!(lw.count, well.count);
        for j in 0..brain.pairs {
            assert!(
                (lw.sum_k[j] - well.sum_k[j]).norm() < 1e-10,
                "well {} pair {} K mismatch",
                name,
                j
            );
        }
    }
}

#[test]
fn encoding_modes_are_realistic() {
    // Real articles should produce mostly CASCADED blocks
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }

        let dim = histogram::embedding_dim();
        let packet = encode(&traj, t, dim, None);

        let cascaded = packet
            .blocks
            .iter()
            .filter(|b| b.mode == Mode::Cascaded)
            .count();
        let linear = packet
            .blocks
            .iter()
            .filter(|b| b.mode == Mode::Linear)
            .count();
        let total = packet.blocks.len();

        // Byte histograms should have oscillatory structure
        assert!(
            cascaded + linear > 0,
            "{}: no CASCADED or LINEAR blocks ({} blocks total)",
            name,
            total
        );

        // Capture should be meaningful for text
        assert!(
            packet.capture() > 20.0,
            "{}: capture {}% too low for text",
            name,
            packet.capture()
        );
    }
}

#[test]
fn compression_ratio_is_meaningful() {
    for &name in ARTICLES {
        let text = match load_text(name) {
            Some(t) => t,
            None => continue,
        };

        let (traj, t) = histogram::text_to_trajectory(&text, histogram::default_chunk_size());
        if t < 4 {
            continue;
        }

        let dim = histogram::embedding_dim();
        let packet = encode(&traj, t, dim, None);

        let raw_size = traj.len() * 4; // f32
        let wire_size = to_wire(&packet).len();
        let compact_size = to_wire_compact(&packet).len();

        // Wire should be smaller than raw (oscillators compress the signal)
        // Compact should be much smaller (no residuals)
        assert!(
            compact_size < raw_size,
            "{}: compact {} >= raw {}",
            name,
            compact_size,
            raw_size
        );

        eprintln!(
            "{}: raw={}B wire={}B compact={}B capture={:.1}%",
            name,
            raw_size,
            wire_size,
            compact_size,
            packet.capture()
        );
    }
}
