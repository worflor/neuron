// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../LICENSE.md.

//! Brain file I/O: save and load .engram files.
//!
//! Format: binary sections (not ZIP, for minimal dependencies).
//!
//! Layout:
//!   [MAGIC: 4B "ENBR"]
//!   [VERSION: 1B]
//!   [DIM: 4B LE]
//!   [PAIRS: 4B LE]
//!   [ALPHA: 4B LE f32]
//!   [TOTAL_ABSORBED: 4B LE]
//!   [NEXT_WELL_ID: 4B LE]
//!   [NAME_LEN: 2B LE] [NAME: utf8]
//!   [REF_PAIRING_PRESENT: 1B] [if present: DIM × 4B LE i32]
//!   [N_WELLS: 4B LE]
//!   for each well:
//!     [WELL_NAME_LEN: 2B LE] [WELL_NAME: utf8]
//!     [COUNT: 4B LE]
//!     [SUM_K: PAIRS × 16B (re:f64 + im:f64)]
//!   [N_DREAM: 4B LE]
//!   for each dream entry:
//!     [K: PAIRS × 16B] [G: PAIRS × 16B] [S: PAIRS × 4B f32]

use crate::brain::{Brain, DreamEntry, Well};
use num_complex::Complex64;

const MAGIC: [u8; 4] = *b"ENBR";
const VERSION: u8 = 1;

/// Serialize a Brain to bytes.
pub fn save(brain: &Brain) -> Vec<u8> {
    let p = brain.pairs;
    let mut buf = Vec::with_capacity(64 * 1024);

    // Header
    buf.extend_from_slice(&MAGIC);
    buf.push(VERSION);
    buf.extend_from_slice(&(brain.dim as u32).to_le_bytes());
    buf.extend_from_slice(&(brain.pairs as u32).to_le_bytes());
    buf.extend_from_slice(&brain.alpha.to_le_bytes());
    buf.extend_from_slice(&(brain.total_absorbed as u32).to_le_bytes());
    buf.extend_from_slice(&(brain.next_well_id as u32).to_le_bytes());

    // Name
    let name_bytes = brain.name.as_bytes();
    buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
    buf.extend_from_slice(name_bytes);

    // Reference pairing
    if let Some(ref pairing) = brain.reference_pairing {
        buf.push(1);
        for &v in pairing {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    } else {
        buf.push(0);
    }

    // Wells — emit in a canonical (name-sorted) order so the serialized form is
    // deterministic regardless of HashMap iteration order. Load rebuilds the map either
    // way, so this is a pure canonicalization (KNOCKBACK relies on it for reproducible
    // brain bytes; see neuron::twin determinism tests).
    buf.extend_from_slice(&(brain.wells.len() as u32).to_le_bytes());
    let mut wells: Vec<(&String, &Well)> = brain.wells.iter().collect();
    wells.sort_by(|a, b| a.0.cmp(b.0));
    for (name, well) in wells {
        let name_bytes = name.as_bytes();
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(&(well.count as u32).to_le_bytes());
        for j in 0..p {
            buf.extend_from_slice(&well.sum_k[j].re.to_le_bytes());
            buf.extend_from_slice(&well.sum_k[j].im.to_le_bytes());
        }
    }

    // Dream buffer
    buf.extend_from_slice(&(brain.dream.len() as u32).to_le_bytes());
    for entry in &brain.dream {
        for j in 0..p {
            buf.extend_from_slice(&entry.k[j].re.to_le_bytes());
            buf.extend_from_slice(&entry.k[j].im.to_le_bytes());
        }
        for j in 0..p {
            buf.extend_from_slice(&entry.g[j].re.to_le_bytes());
            buf.extend_from_slice(&entry.g[j].im.to_le_bytes());
        }
        for j in 0..p {
            buf.extend_from_slice(&entry.s[j].to_le_bytes());
        }
    }

    buf
}

/// Deserialize a Brain from bytes.
pub fn load(data: &[u8]) -> Option<Brain> {
    if data.len() < 5 || data[..4] != MAGIC {
        return None;
    }
    let mut pos = 4;

    let version = data[pos];
    pos += 1;
    if version != VERSION {
        return None;
    }

    let dim = read_u32(data, &mut pos) as usize;
    let pairs = read_u32(data, &mut pos) as usize;
    let alpha = read_f32(data, &mut pos);
    let total_absorbed = read_u32(data, &mut pos) as usize;
    let next_well_id = read_u32(data, &mut pos) as usize;

    // Name
    let name_len = read_u16(data, &mut pos) as usize;
    let name = std::str::from_utf8(&data[pos..pos + name_len])
        .ok()?
        .to_string();
    pos += name_len;

    // Reference pairing
    let has_pairing = data[pos];
    pos += 1;
    let reference_pairing = if has_pairing == 1 {
        let mut pairing = Vec::with_capacity(dim);
        for _ in 0..dim {
            pairing.push(read_i32(data, &mut pos));
        }
        Some(pairing)
    } else {
        None
    };

    // Wells
    let n_wells = read_u32(data, &mut pos) as usize;
    let mut wells = std::collections::HashMap::with_capacity(n_wells);
    for _ in 0..n_wells {
        let wname_len = read_u16(data, &mut pos) as usize;
        let wname = std::str::from_utf8(&data[pos..pos + wname_len])
            .ok()?
            .to_string();
        pos += wname_len;
        let count = read_u32(data, &mut pos) as usize;
        let mut sum_k = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            let re = read_f64(data, &mut pos);
            let im = read_f64(data, &mut pos);
            sum_k.push(Complex64::new(re, im));
        }
        wells.insert(wname, Well { sum_k, count });
    }

    // Dream buffer
    let n_dream = read_u32(data, &mut pos) as usize;
    let mut dream = Vec::with_capacity(n_dream);
    for _ in 0..n_dream {
        let mut k = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            let re = read_f64(data, &mut pos);
            let im = read_f64(data, &mut pos);
            k.push(Complex64::new(re, im));
        }
        let mut g = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            let re = read_f64(data, &mut pos);
            let im = read_f64(data, &mut pos);
            g.push(Complex64::new(re, im));
        }
        let mut s = Vec::with_capacity(pairs);
        for _ in 0..pairs {
            s.push(read_f32(data, &mut pos));
        }
        dream.push(DreamEntry { k, g, s });
    }

    let mut brain = Brain::new(dim, alpha);
    brain.name = name;
    brain.wells = wells;
    brain.reference_pairing = reference_pairing;
    brain.dream = dream;
    brain.total_absorbed = total_absorbed;
    brain.next_well_id = next_well_id;

    Some(brain)
}

// --- LE helpers ---

fn read_u16(data: &[u8], pos: &mut usize) -> u16 {
    let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    v
}

fn read_u32(data: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_i32(data: &[u8], pos: &mut usize) -> i32 {
    let v = i32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_f32(data: &[u8], pos: &mut usize) -> f32 {
    let v = f32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_f64(data: &[u8], pos: &mut usize) -> f64 {
    let bytes: [u8; 8] = data[*pos..*pos + 8].try_into().unwrap();
    let v = f64::from_le_bytes(bytes);
    *pos += 8;
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_empty_brain() {
        let brain = Brain::new(8, 0.005);
        let bytes = save(&brain);
        let loaded = load(&bytes).expect("should load");

        assert_eq!(loaded.dim, 8);
        assert_eq!(loaded.pairs, 4);
        assert!((loaded.alpha - 0.005).abs() < 1e-6);
        assert!(loaded.wells.is_empty());
        assert!(loaded.dream.is_empty());
    }

    #[test]
    fn save_load_with_wells() {
        let mut brain = Brain::new(8, 0.005);
        brain.name = "Alexandria".into();

        // Add some wells
        let mut w = Well::new(4);
        w.sum_k = vec![Complex64::new(1.5, -0.3); 4];
        w.count = 42;
        brain.wells.insert("physics".into(), w);

        let mut w2 = Well::new(4);
        w2.sum_k = vec![Complex64::new(-0.7, 2.1); 4];
        w2.count = 100;
        brain.wells.insert("biology".into(), w2);

        brain.reference_pairing = Some(vec![0, 1, 2, 3, 4, 5, 6, 7]);
        brain.total_absorbed = 500;

        let bytes = save(&brain);
        let loaded = load(&bytes).expect("should load");

        assert_eq!(loaded.name, "Alexandria");
        assert_eq!(loaded.wells.len(), 2);
        assert_eq!(loaded.wells["physics"].count, 42);
        assert!((loaded.wells["physics"].sum_k[0].re - 1.5).abs() < 1e-10);
        assert_eq!(loaded.total_absorbed, 500);
        assert!(loaded.reference_pairing.is_some());
    }

    #[test]
    fn save_load_with_dream() {
        let mut brain = Brain::new(4, 0.01);
        let p = 2;

        for i in 0..5 {
            brain.dream.push(DreamEntry {
                k: vec![Complex64::new(i as f64 * 0.1, 0.0); p],
                g: vec![Complex64::new(0.0, i as f64 * 0.2); p],
                s: vec![i as f32 * 0.5; p],
            });
        }

        let bytes = save(&brain);
        let loaded = load(&bytes).expect("should load");

        assert_eq!(loaded.dream.len(), 5);
        assert!((loaded.dream[3].k[0].re - 0.3).abs() < 1e-10);
        assert!((loaded.dream[3].s[0] - 1.5).abs() < 1e-5);
    }

    #[test]
    fn save_load_roundtrip_after_absorb() {
        let mut brain = Brain::new(8, 0.005);

        // Absorb a few trajectories
        for i in 0..10 {
            let traj: Vec<f32> = (0..50 * 8)
                .map(|j| ((j as f64 + i as f64 * 100.0) * 0.1).sin() as f32)
                .collect();
            brain.fast_absorb(&traj, 50, Some("domain_a"));
        }

        let bytes = save(&brain);
        let loaded = load(&bytes).expect("should load");

        assert_eq!(loaded.total_absorbed, brain.total_absorbed);
        assert_eq!(loaded.wells.len(), brain.wells.len());
        assert_eq!(loaded.dream.len(), brain.dream.len());

        // Check well K values match
        for (name, well) in &brain.wells {
            let lw = &loaded.wells[name];
            assert_eq!(lw.count, well.count);
            for j in 0..brain.pairs {
                assert!((lw.sum_k[j] - well.sum_k[j]).norm() < 1e-10);
            }
        }
    }

    #[test]
    fn bad_magic_returns_none() {
        assert!(load(b"XXXX").is_none());
        assert!(load(b"").is_none());
        assert!(load(b"ENB").is_none());
    }
}
