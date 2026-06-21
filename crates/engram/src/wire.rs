#![allow(clippy::needless_range_loop)]
//! Wire format serialization and deserialization.
//!
//! Binary format: header + pairing + seeds + per-block data.
//! Varint-encoded residuals with zigzag signed→unsigned mapping.
//! Supports full (v1) and compact/brain (v2) formats.

use crate::types::{Block, Mode, Packet};
use num_complex::Complex64;

/// Wire format magic bytes.
const WIRE_MAGIC: [u8; 2] = *b"EN";

/// Full format (with residuals).
const WIRE_VERSION_FULL: u8 = 1;

/// Compact/brain format (K,G + pair_rms only, no residuals).
const WIRE_VERSION_COMPACT: u8 = 2;

// ─── Zigzag + Varint ────────────────────────────────────────────────

/// Zigzag encode: 0→0, -1→1, 1→2, -2→3, 2→4, ...
#[inline]
fn zigzag_encode(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// Zigzag decode: inverse of zigzag_encode.
#[inline]
fn zigzag_decode(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Pack a slice of i32 values as zigzag + varint bytes.
fn pack_varints(values: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 2);
    for &v in values {
        let mut zz = zigzag_encode(v);
        loop {
            if zz < 0x80 {
                out.push(zz as u8);
                break;
            }
            out.push((zz as u8 & 0x7F) | 0x80);
            zz >>= 7;
        }
    }
    out
}

/// Unpack varint bytes back to i32 values.
fn unpack_varints(data: &[u8], count: usize) -> (Vec<i32>, usize) {
    let mut values = Vec::with_capacity(count);
    let mut pos = 0;
    for _ in 0..count {
        let mut result: u32 = 0;
        let mut shift = 0;
        loop {
            if pos >= data.len() {
                values.push(0);
                break;
            }
            let byte = data[pos];
            pos += 1;
            result |= ((byte & 0x7F) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        values.push(zigzag_decode(result));
    }
    (values, pos)
}

// ─── Little-endian helpers ──────────────────────────────────────────

fn write_u16_le(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_u32_le(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_f32_le(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn read_u16_le(data: &[u8], pos: &mut usize) -> u16 {
    let v = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    v
}

fn read_u32_le(data: &[u8], pos: &mut usize) -> u32 {
    let v = u32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_f32_le(data: &[u8], pos: &mut usize) -> f32 {
    let v = f32::from_le_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
    *pos += 4;
    v
}

fn read_f16_le(data: &[u8], pos: &mut usize) -> f32 {
    let bits = u16::from_le_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    f16_to_f32(bits)
}

/// Write a complex value as two f32 (re, im).
fn write_complex_f32(buf: &mut Vec<u8>, c: Complex64) {
    write_f32_le(buf, c.re as f32);
    write_f32_le(buf, c.im as f32);
}

/// Read a complex value from two f32 (re, im).
fn read_complex_f32(data: &[u8], pos: &mut usize) -> Complex64 {
    let re = read_f32_le(data, pos) as f64;
    let im = read_f32_le(data, pos) as f64;
    Complex64::new(re, im)
}

// ─── Float16 conversion (minimal, no dependency) ───────────────────

fn f32_to_f16(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    let frac = (bits >> 13) & 0x3FF;

    if exp <= 0 {
        sign as u16
    } else if exp >= 31 {
        (sign | 0x7C00) as u16 // infinity
    } else {
        (sign | (exp as u32) << 10 | frac) as u16
    }
}

fn f16_to_f32(bits: u16) -> f32 {
    let sign = ((bits >> 15) & 1) as u32;
    let exp = ((bits >> 10) & 0x1F) as u32;
    let frac = (bits & 0x3FF) as u32;

    if exp == 0 {
        if frac == 0 {
            f32::from_bits(sign << 31)
        } else {
            // subnormal
            let mut e = 1_u32;
            let mut f = frac;
            while f & 0x400 == 0 {
                f <<= 1;
                e += 1;
            }
            f &= 0x3FF;
            f32::from_bits((sign << 31) | ((127 - 15 + 1 - e) << 23) | (f << 13))
        }
    } else if exp == 31 {
        f32::from_bits((sign << 31) | 0x7F800000 | (frac << 13))
    } else {
        f32::from_bits((sign << 31) | ((exp + 127 - 15) << 23) | (frac << 13))
    }
}

// ─── Serialization ─────────────────────────────────────────────────

/// Serialize a Packet to wire bytes (full format).
pub fn to_wire(packet: &Packet) -> Vec<u8> {
    to_wire_inner(packet, false)
}

/// Serialize a Packet to compact/brain wire bytes (no residuals).
pub fn to_wire_compact(packet: &Packet) -> Vec<u8> {
    to_wire_inner(packet, true)
}

fn to_wire_inner(packet: &Packet, compact: bool) -> Vec<u8> {
    let dim = packet.dim;
    let p = packet.pairs;
    let version = if compact {
        WIRE_VERSION_COMPACT
    } else {
        WIRE_VERSION_FULL
    };

    let mut buf = Vec::with_capacity(1024);

    // Header: 19 bytes
    buf.extend_from_slice(&WIRE_MAGIC);
    buf.push(version);
    write_u16_le(&mut buf, dim as u16);
    write_u32_le(&mut buf, packet.length as u32);
    write_f32_le(&mut buf, packet.alpha);
    write_u16_le(&mut buf, packet.macro_block as u16);
    write_u16_le(&mut buf, packet.micro_block as u16);
    write_u16_le(&mut buf, packet.blocks.len() as u16);

    if !compact {
        // Pairing (2D bytes)
        for &idx in &packet.pairing {
            write_u16_le(&mut buf, idx as u16);
        }

        // Global seeds from first block
        if let Some(first) = packet.blocks.first() {
            for &v in &first.seed1 {
                write_f32_le(&mut buf, v);
            }
            for &v in &first.seed2 {
                write_f32_le(&mut buf, v);
            }
        } else {
            for _ in 0..dim * 2 {
                write_f32_le(&mut buf, 0.0);
            }
        }
    }

    // Per-block data
    for blk in &packet.blocks {
        buf.push(blk.mode as u8);
        write_u16_le(&mut buf, blk.length as u16);

        match blk.mode {
            Mode::Cascaded => {
                // Reset flag = 1 (always full K,G for now, delta-coding TBD)
                buf.push(1);
                for j in 0..p {
                    write_complex_f32(&mut buf, blk.macro_k[j]);
                }
                for j in 0..p {
                    write_complex_f32(&mut buf, blk.macro_g[j]);
                }

                if compact {
                    // Pair RMS as float16
                    if let Some(ref rms) = blk.pair_rms {
                        for j in 0..p {
                            write_u16_le(&mut buf, f32_to_f16(rms[j]));
                        }
                    } else {
                        for _ in 0..p {
                            write_u16_le(&mut buf, 0);
                        }
                    }
                } else {
                    // Micro K,G
                    write_u16_le(&mut buf, blk.micro_ks.len() as u16);
                    for si in 0..blk.micro_ks.len() {
                        for j in 0..p {
                            write_complex_f32(&mut buf, blk.micro_ks[si][j]);
                        }
                        for j in 0..p {
                            write_complex_f32(&mut buf, blk.micro_gs[si][j]);
                        }
                    }
                }
            }
            Mode::Linear => {
                // No K,G needed (implied K=2, G=1)
                if compact {
                    if let Some(ref rms) = blk.pair_rms {
                        for j in 0..p {
                            write_u16_le(&mut buf, f32_to_f16(rms[j]));
                        }
                    } else {
                        for _ in 0..p {
                            write_u16_le(&mut buf, 0);
                        }
                    }
                }
            }
            Mode::Static | Mode::Raw => {
                // Nothing extra for compact
            }
        }

        if !compact {
            // Scales + varint-encoded residuals
            for j in 0..p {
                write_f32_le(&mut buf, blk.scales[j]);
            }

            // Quantize residuals to i32 and pack as varints
            let quant_i32: Vec<i32> = blk.residuals.iter().map(|&v| v.round() as i32).collect();
            let varint_bytes = pack_varints(&quant_i32);
            write_u32_le(&mut buf, varint_bytes.len() as u32);
            buf.extend_from_slice(&varint_bytes);
        }
    }

    buf
}

/// Deserialize wire bytes back to a Packet.
pub fn from_wire(data: &[u8]) -> Option<Packet> {
    from_wire_inner(data, false)
}

/// Deserialize compact wire bytes back to a Packet.
pub fn from_wire_compact(data: &[u8]) -> Option<Packet> {
    from_wire_inner(data, true)
}

fn from_wire_inner(data: &[u8], expect_compact: bool) -> Option<Packet> {
    if data.len() < 19 {
        return None;
    }

    // Magic
    if data[0] != WIRE_MAGIC[0] || data[1] != WIRE_MAGIC[1] {
        return None;
    }
    let mut pos = 2;

    let version = data[pos];
    pos += 1;
    let compact = version == WIRE_VERSION_COMPACT;
    if expect_compact && !compact {
        return None;
    }
    if !expect_compact && compact {
        return None;
    }

    let dim = read_u16_le(data, &mut pos) as usize;
    let length = read_u32_le(data, &mut pos) as usize;
    let alpha = read_f32_le(data, &mut pos);
    let macro_block = read_u16_le(data, &mut pos) as usize;
    let micro_block = read_u16_le(data, &mut pos) as usize;
    let n_blocks = read_u16_le(data, &mut pos) as usize;
    let p = dim / 2;

    let mut pairing: Vec<i32> = (0..dim as i32).collect();
    let mut global_seed1 = vec![0.0_f32; dim];
    let mut global_seed2 = vec![0.0_f32; dim];

    if !compact {
        // Pairing
        pairing.clear();
        for _ in 0..dim {
            pairing.push(read_u16_le(data, &mut pos) as i32);
        }
        // Seeds
        for d in 0..dim {
            global_seed1[d] = read_f32_le(data, &mut pos);
        }
        for d in 0..dim {
            global_seed2[d] = read_f32_le(data, &mut pos);
        }
    }

    let mut blocks = Vec::with_capacity(n_blocks);
    for bi in 0..n_blocks {
        let mode_byte = data[pos];
        pos += 1;
        let mode = match mode_byte {
            0 => Mode::Cascaded,
            1 => Mode::Linear,
            2 => Mode::Static,
            3 => Mode::Raw,
            _ => return None,
        };
        let blk_length = read_u16_le(data, &mut pos) as usize;

        let mut macro_k = vec![Complex64::ZERO; p];
        let mut macro_g = vec![Complex64::ZERO; p];
        let mut micro_ks = Vec::new();
        let mut micro_gs = Vec::new();
        let mut pair_rms: Option<Vec<f32>> = None;

        match mode {
            Mode::Cascaded => {
                let reset_flag = data[pos];
                pos += 1;
                if reset_flag == 1 {
                    for j in 0..p {
                        macro_k[j] = read_complex_f32(data, &mut pos);
                    }
                    for j in 0..p {
                        macro_g[j] = read_complex_f32(data, &mut pos);
                    }
                }
                // else: delta-coded (TBD)

                if compact {
                    let mut rms = vec![0.0_f32; p];
                    for j in 0..p {
                        rms[j] = read_f16_le(data, &mut pos);
                    }
                    pair_rms = Some(rms);
                } else {
                    let n_micro = read_u16_le(data, &mut pos) as usize;
                    for _ in 0..n_micro {
                        let mut mk = vec![Complex64::ZERO; p];
                        let mut mg = vec![Complex64::ZERO; p];
                        for j in 0..p {
                            mk[j] = read_complex_f32(data, &mut pos);
                        }
                        for j in 0..p {
                            mg[j] = read_complex_f32(data, &mut pos);
                        }
                        micro_ks.push(mk);
                        micro_gs.push(mg);
                    }
                }
            }
            Mode::Linear => {
                macro_k = vec![Complex64::new(2.0, 0.0); p];
                macro_g = vec![Complex64::new(1.0, 0.0); p];

                if compact {
                    let mut rms = vec![0.0_f32; p];
                    for j in 0..p {
                        rms[j] = read_f16_le(data, &mut pos);
                    }
                    pair_rms = Some(rms);
                }
            }
            Mode::Static => {
                macro_k = vec![Complex64::new(1.0, 0.0); p];
                macro_g = vec![Complex64::ZERO; p];
            }
            Mode::Raw => {}
        }

        let mut scales = vec![0.0_f32; p];
        let mut residuals = vec![0.0_f32; blk_length * dim];

        if !compact {
            // Scales
            for j in 0..p {
                scales[j] = read_f32_le(data, &mut pos);
            }

            // Varint residuals
            let varint_len = read_u32_le(data, &mut pos) as usize;
            let (quant_vals, _bytes_read) =
                unpack_varints(&data[pos..pos + varint_len], blk_length * dim);
            pos += varint_len;

            for (i, &v) in quant_vals.iter().enumerate() {
                residuals[i] = v as f32;
            }
        }

        let seed1 = if bi == 0 {
            global_seed1.clone()
        } else {
            vec![0.0; dim]
        };
        let seed2 = if bi == 0 {
            global_seed2.clone()
        } else {
            vec![0.0; dim]
        };

        blocks.push(Block {
            mode,
            start: 0, // caller should set from context
            length: blk_length,
            macro_k,
            macro_g,
            micro_ks,
            micro_gs,
            residuals,
            seed1,
            seed2,
            scales,
            pair_rms,
            signal_energy: 0.0,
            residual_energy: 0.0,
            macro_capture: 0.0,
            micro_capture: 0.0,
        });
    }

    Some(Packet {
        dim,
        pairs: p,
        length,
        alpha,
        macro_block,
        micro_block,
        pairing,
        blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::encode;

    #[test]
    fn zigzag_roundtrip() {
        for v in [-100, -1, 0, 1, 42, i32::MAX, i32::MIN + 1] {
            assert_eq!(zigzag_decode(zigzag_encode(v)), v);
        }
    }

    #[test]
    fn varint_roundtrip() {
        let values = vec![0, -1, 1, -128, 127, 1000, -1000, 0, 42];
        let packed = pack_varints(&values);
        let (unpacked, _) = unpack_varints(&packed, values.len());
        assert_eq!(values, unpacked);
    }

    #[test]
    fn varint_single_byte() {
        // Values 0..63 should be 1 byte each in zigzag
        let packed = pack_varints(&[0]);
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0], 0);

        let packed = pack_varints(&[1]);
        assert_eq!(packed.len(), 1);
        assert_eq!(packed[0], 2); // zigzag(1) = 2
    }

    #[test]
    fn f16_roundtrip() {
        for v in [0.0_f32, 1.0, -1.0, 0.5, 100.0, 0.001] {
            let bits = f32_to_f16(v);
            let back = f16_to_f32(bits);
            let err = (v - back).abs();
            // f16 has ~3 decimal digits of precision
            assert!(
                err < v.abs() * 0.01 + 0.001,
                "f16 roundtrip: {} → {} (err={})",
                v,
                back,
                err
            );
        }
    }

    #[test]
    fn wire_full_roundtrip() {
        let dim = 4;
        let t = 60;
        let w: Vec<f32> = (0..t * dim)
            .map(|i| {
                let row = i / dim;
                (row as f64 * 0.3).sin() as f32
            })
            .collect();

        let packet = encode(&w, t, dim, None);
        let bytes = to_wire(&packet);

        assert!(bytes.len() > 19, "wire should have header + data");
        assert_eq!(bytes[0], b'E');
        assert_eq!(bytes[1], b'N');
        assert_eq!(bytes[2], WIRE_VERSION_FULL);

        let decoded = from_wire(&bytes).expect("should parse");
        assert_eq!(decoded.dim, dim);
        assert_eq!(decoded.length, t);
        assert_eq!(decoded.blocks.len(), packet.blocks.len());

        // Verify K,G roundtrip
        for (orig, dec) in packet.blocks.iter().zip(decoded.blocks.iter()) {
            assert_eq!(orig.mode, dec.mode);
            assert_eq!(orig.length, dec.length);
            for j in 0..dim / 2 {
                // f32 precision loss from wire
                assert!((orig.macro_k[j].re as f32 - dec.macro_k[j].re as f32).abs() < 1e-5);
            }
        }
    }

    #[test]
    fn wire_compact_roundtrip() {
        let dim = 4;
        let t = 40;
        let w: Vec<f32> = (0..t * dim)
            .map(|i| (i as f64 * 0.1).cos() as f32)
            .collect();

        let packet = encode(&w, t, dim, None);
        let bytes = to_wire_compact(&packet);

        assert!(
            bytes.len() < to_wire(&packet).len(),
            "compact should be smaller than full"
        );

        let decoded = from_wire_compact(&bytes).expect("should parse compact");
        assert_eq!(decoded.dim, dim);
        assert_eq!(decoded.length, t);
    }

    #[test]
    fn wire_empty_packet() {
        let packet = encode(&[], 0, 4, None);
        let bytes = to_wire(&packet);
        let decoded = from_wire(&bytes).expect("should parse empty");
        assert_eq!(decoded.length, 0);
        assert!(decoded.blocks.is_empty());
    }
}
