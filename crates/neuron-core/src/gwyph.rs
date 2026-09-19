// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../../../LICENSE.md and ../../../LICENSES/WLCSL-1.0.md.

//! `.gwyph` writer — emit a Whisper Glyph v3 file from a stroke, in pure Rust.
//!
//! `.gwyph` (a *whisper glyph file*, MIME `application/x-whisper-gwyph`) is the on-disk format the
//! external Whisper toolchain reads (`live-gwyph.ts` `parseGwyphPayload`, the drawing app's
//! `live-draw.ts` `encodeGwyphPayload`). It carries time-ordered pen strokes compressed by the
//! eigenmotion codec. This module produces **byte-spec-compliant** files that round-trip through
//! that toolchain, so a captured Neuron weave can be loaded into the glyph/eigenmotion experiments.
//!
//! ── two deliberate simplifications, both lossless ────────────────────────────────────────────
//!
//! The reference writer runs two codecs we do **not** reproduce bit-for-bit, and don't need to:
//!
//!   * **Logos 0D entropy coder.** Our [`crate::logos`] port is a faithful *predictor* but not a
//!     byte-identical *coder* (see its module docs), so a Logos-*compressed* payload would be
//!     undecodable by the reference WASM. But `decode0D` also accepts a **raw passthrough** mode
//!     (`0xFF` + bytes), byte-exact on both sides. We emit that. The file is uncompressed; for
//!     stroke transport that is irrelevant (the experiments re-encode anyway).
//!   * **Harmonic block predictor.** The reference fits a damped complex oscillator per block and
//!     stores K/G + residuals. We instead emit **LINEAR-mode blocks** (predictor `2·z[n-1] −
//!     z[n-2]`, integer-exact, no stored coefficients), which the reference decoder reconstructs
//!     losslessly. The eigenmotion is recovered downstream by re-encoding with the real codec —
//!     keeping those numbers authoritative to *that* codec, not to this writer.
//!
//! Net: the file carries the **exact q15 points**, decodes byte-for-byte in the Whisper reader, and
//! imposes no fit of our own on the data. [`roundtrips`](tests) proves the encode against a mirror
//! decoder; the authoritative check is loading the output through `parseGwyphPayload`.
//!
//! Format (little-endian; varuint = unsigned LEB128), mirroring `live-draw.ts`:
//! ```text
//! file:    "GWYP"(47 57 59 50) ver=3  rawLen:u32  encode0D(payload)        // we: 0xFF ++ payload
//! payload: mode:u8(0=blank)  logicalW:u16  logicalH:u16  paletteN:var  RGB×N  strokeN:var  stroke…
//! pen:     tag:u8(0)  colorIdx:var  width:u16(=w·256)  pointN:var
//!          seed0[5×u16 q15]  seed1[5×u16 q15]  packedLen:var  packed
//! packed:  [rawLen:u16 metaLen:u8 stride:u8] encode0D(meta‖data)           // long form if it overflows
//! ```

/// Channels per glyph point: `x, y, p(ressure), tilt, azimuth`. Mouse motion is 2D, so the last
/// three are always 0 — but the seed/point layout still carries all five, per spec.
pub const GLYPH_CHANNELS: usize = 5;

/// Options for the single pen stroke a file carries.
#[derive(Clone, Copy, Debug)]
pub struct StrokeStyle {
    /// Logical canvas the normalized points map onto (square keeps drawn aspect on render).
    pub logical_w: u16,
    pub logical_h: u16,
    /// Pen colour (the one-entry palette).
    pub color_rgb: [u8; 3],
    /// Pen width in logical px (stored as `u16 = round(width·256)`).
    pub width: f32,
}

impl Default for StrokeStyle {
    fn default() -> Self {
        StrokeStyle {
            logical_w: 1024,
            logical_h: 1024,
            color_rgb: [0xE8, 0xE8, 0xE8],
            width: 2.0,
        }
    }
}

// ── byte writer (matches live-draw.ts ByteWriter) ────────────────────────────────────────────

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn new() -> Writer {
        Writer { out: Vec::new() }
    }
    fn u8(&mut self, v: u8) {
        self.out.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.out.push((v & 0xff) as u8);
        self.out.push((v >> 8) as u8);
    }
    fn bytes(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }
    /// Unsigned LEB128.
    fn varuint(&mut self, v: u32) {
        let mut n = v;
        while n >= 0x80 {
            self.out.push(((n & 0x7f) | 0x80) as u8);
            n >>= 7;
        }
        self.out.push(n as u8);
    }
}

/// q15 quantize a normalized coordinate: `round(clamp01(v)·32767)`, exactly as the reference.
#[inline]
fn quant_q15(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * 32767.0).round() as u16
}

/// Zigzag encode an i32 to the unsigned form the codec stores (`(v<<1) ^ (v>>31)`).
#[inline]
fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

// ── the inner block stream: LINEAR-mode pack (headers-first, then raw-Logos) ──────────────────

const GLYPH_BLOCK_SIZE: usize = 16;
const MODE_LINEAR: u8 = 1; // GlyphMode.LINEAR
const CHMASK_XY: u8 = 0b10; // x,y only (mouse/finger) → 2 wire channels

/// LINEAR predictor for one channel at absolute point `i`: `2·q[i-1] − q[i-2]` (integer-exact,
/// matches the reference WASM's LINEAR decode with K=2,G=1 in Q14).
#[inline]
fn lin_pred(q: &[[i32; GLYPH_CHANNELS]], i: usize, c: usize) -> i32 {
    2 * q[i - 1][c] - q[i - 2][c]
}

/// Pack `q` (q15 points, the first two are the seeds the caller already wrote) into the codec's
/// headers-first block stream, then wrap in a raw-Logos container — i.e. produce the `packed`
/// bytes a pen stroke carries for `pointCount ≥ 3`.
fn pack_linear(q: &[[i32; GLYPH_CHANNELS]]) -> Vec<u8> {
    let n = q.len();
    let mut meta: Vec<u8> = Vec::new();
    let mut data: Vec<u8> = Vec::new();
    let mut stride: u8 = 0; // residual bytes of the first block's first channel (Ab-axis stride)

    let mut i = 2usize;
    while i < n {
        let len = GLYPH_BLOCK_SIZE.min(n - i);
        // header: modeBit(LINEAR) | (count-1)<<1 | repeat(0) | chMask<<6   (no ext: features=0, default lane)
        meta.push(MODE_LINEAR | (((len as u8) - 1) << 1) | (CHMASK_XY << 6));
        // residuals, channel-major over the 2 wire channels, zigzag varint
        for c in 0..2 {
            let pre = data.len();
            for j in 0..len {
                let abspt = i + j;
                let res = q[abspt][c] - lin_pred(q, abspt, c);
                push_varuint(&mut data, zigzag(res));
            }
            if c == 0 && stride == 0 {
                stride = (data.len() - pre) as u8;
            }
        }
        i += len;
    }

    // raw = [meta][data]; single (raw-mode) Logos call over the combined stream.
    let raw_len = meta.len() + data.len();
    let mut raw = Vec::with_capacity(raw_len);
    raw.extend_from_slice(&meta);
    raw.extend_from_slice(&data);

    let mut out = Vec::with_capacity(raw_len + 10);
    if raw_len <= 0xFFFF && meta.len() <= 0xFE {
        // short header: rawLen u16, metaLen u8, stride u8
        out.push((raw_len & 0xFF) as u8);
        out.push((raw_len >> 8) as u8);
        out.push(meta.len() as u8);
        out.push(stride);
    } else {
        // long header: 0,0, rawLen u32, metaLen u16, stride u8
        out.push(0);
        out.push(0);
        out.extend_from_slice(&(raw_len as u32).to_le_bytes());
        out.extend_from_slice(&(meta.len() as u16).to_le_bytes());
        out.push(stride);
    }
    // encode0D(raw): raw passthrough — mode byte 0xFF then the bytes verbatim.
    out.push(0xFF);
    out.extend_from_slice(&raw);
    out
}

fn push_varuint(buf: &mut Vec<u8>, v: u32) {
    let mut n = v;
    while n >= 0x80 {
        buf.push(((n & 0x7f) | 0x80) as u8);
        n >>= 7;
    }
    buf.push(n as u8);
}

// ── public surface ───────────────────────────────────────────────────────────────────────────

/// Encode a single pen stroke (normalized `[0,1]` `x,y` points) as a complete `.gwyph` v3 file.
///
/// Points outside `[0,1]` are clamped by the q15 quantizer; normalize to taste before calling
/// (preserve aspect, and record the transform alongside, if the absolute geometry matters).
#[must_use]
pub fn encode_stroke(points: &[[f32; 2]], style: &StrokeStyle) -> Vec<u8> {
    let n = points.len();

    // q15 points, all five channels (x, y, then p/tilt/azimuth = 0 for mouse).
    let q: Vec<[i32; GLYPH_CHANNELS]> = points
        .iter()
        .map(|p| {
            [
                i32::from(quant_q15(p[0])),
                i32::from(quant_q15(p[1])),
                0,
                0,
                0,
            ]
        })
        .collect();

    // ── payload (v3 raw) ──
    let mut w = Writer::new();
    w.u8(0); // mode: blank
    w.u16(style.logical_w.max(1));
    w.u16(style.logical_h.max(1));
    w.varuint(1); // palette: one colour
    w.bytes(&style.color_rgb);
    w.varuint(1); // one stroke

    // pen stroke
    w.u8(0x00); // tag: pen (bit0=0 not-fill, bit1=0 not-eraser)
    w.varuint(0); // colour index 0
    w.u16(((style.width * 256.0).round() as i64).clamp(1, 65535) as u16);
    w.varuint(n as u32);
    if n >= 1 {
        for c in 0..GLYPH_CHANNELS {
            w.u16(q[0][c] as u16);
        }
    }
    if n >= 2 {
        for c in 0..GLYPH_CHANNELS {
            w.u16(q[1][c] as u16);
        }
    }
    if n >= 3 {
        let packed = pack_linear(&q);
        w.varuint(packed.len() as u32);
        w.bytes(&packed);
    }
    let payload = w.out;

    // ── file: header + raw-Logos(payload) ──
    let mut file = Vec::with_capacity(payload.len() + 10);
    file.extend_from_slice(&[0x47, 0x57, 0x59, 0x50, 3]); // "GWYP", version 3
    file.extend_from_slice(&(payload.len() as u32).to_le_bytes()); // rawLen
    file.push(0xFF); // encode0D raw-passthrough mode
    file.extend_from_slice(&payload);
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── a mirror decoder: the minimal half of `parseGwyphPayload` + `GlyphCodec` needed to prove
    //    the writer round-trips. Reads our own raw-Logos + LINEAR output back to q15 points. ──

    fn read_varuint(d: &[u8], off: &mut usize) -> u32 {
        let mut shift = 0u32;
        let mut out = 0u32;
        loop {
            let b = d[*off];
            *off += 1;
            out |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        out
    }
    fn read_u16(d: &[u8], off: &mut usize) -> u16 {
        let v = u16::from(d[*off]) | (u16::from(d[*off + 1]) << 8);
        *off += 2;
        v
    }
    fn zzdec(v: u32) -> i32 {
        ((v >> 1) as i32) ^ -((v & 1) as i32)
    }

    /// Decode `packed` (our raw-Logos + LINEAR blocks) given the two seeds → full q15 point list.
    fn unpack_linear(packed: &[u8], seed0: [i32; 5], seed1: [i32; 5]) -> Vec<[i32; 5]> {
        // header
        let (raw_len, meta_len, mut p) = if packed[0] == 0 && packed[1] == 0 {
            let raw_len = u32::from_le_bytes([packed[2], packed[3], packed[4], packed[5]]) as usize;
            let meta_len = u16::from_le_bytes([packed[6], packed[7]]) as usize;
            (raw_len, meta_len, 9usize) // +1 stride byte at [8], payload at 9
        } else {
            (packed[0] as usize | ((packed[1] as usize) << 8), packed[2] as usize, 4usize)
        };
        // raw-Logos: mode byte 0xFF then the bytes
        assert_eq!(packed[p], 0xFF, "expected raw-Logos passthrough");
        p += 1;
        let raw = &packed[p..p + raw_len];

        let meta = &raw[..meta_len];
        let data = &raw[meta_len..];
        let mut pts: Vec<[i32; 5]> = vec![seed0, seed1];
        let mut moff = 0usize;
        let mut doff = 0usize;
        while moff < meta.len() {
            let header = meta[moff];
            moff += 1;
            let count = (((header >> 1) & 0x0F) + 1) as usize;
            // LINEAR, chMask=0b10 (2 channels), no ext — what the writer emits
            let mut block = vec![[0i32; 5]; count];
            for c in 0..2 {
                for blk in block.iter_mut().take(count) {
                    blk[c] = zzdec(read_varuint(data, &mut doff));
                }
            }
            for (j, blk) in block.iter().enumerate() {
                let _ = j;
                let n = pts.len();
                let mut pt = [0i32; 5];
                for c in 0..5 {
                    let pred = 2 * pts[n - 1][c] - pts[n - 2][c];
                    pt[c] = pred + blk[c];
                }
                pts.push(pt);
            }
        }
        pts
    }

    /// Parse a whole file back to q15 points (single pen stroke).
    fn decode_file(file: &[u8]) -> Vec<[i32; 5]> {
        assert_eq!(&file[0..4], &[0x47, 0x57, 0x59, 0x50], "magic");
        assert_eq!(file[4], 3, "version");
        let raw_len = u32::from_le_bytes([file[5], file[6], file[7], file[8]]) as usize;
        assert_eq!(file[9], 0xFF, "raw-Logos outer");
        let payload = &file[10..10 + raw_len];

        let mut off = 0usize;
        let _mode = payload[off];
        off += 1;
        let _w = read_u16(payload, &mut off);
        let _h = read_u16(payload, &mut off);
        let pal = read_varuint(payload, &mut off);
        off += (pal as usize) * 3;
        let strokes = read_varuint(payload, &mut off);
        assert_eq!(strokes, 1);
        let tag = payload[off];
        off += 1;
        assert_eq!(tag & 0x01, 0, "pen");
        let _color_idx = read_varuint(payload, &mut off);
        let _width = read_u16(payload, &mut off);
        let n = read_varuint(payload, &mut off) as usize;
        if n == 0 {
            return Vec::new();
        }
        let mut seed0 = [0i32; 5];
        for s in &mut seed0 {
            *s = i32::from(read_u16(payload, &mut off));
        }
        if n == 1 {
            return vec![seed0];
        }
        let mut seed1 = [0i32; 5];
        for s in &mut seed1 {
            *s = i32::from(read_u16(payload, &mut off));
        }
        if n == 2 {
            return vec![seed0, seed1];
        }
        let packed_len = read_varuint(payload, &mut off) as usize;
        let packed = &payload[off..off + packed_len];
        unpack_linear(packed, seed0, seed1)
    }

    fn roundtrip_points(points: &[[f32; 2]]) {
        let file = encode_stroke(points, &StrokeStyle::default());
        let got = decode_file(&file);
        assert_eq!(got.len(), points.len(), "point count");
        for (i, (g, p)) in got.iter().zip(points.iter()).enumerate() {
            let want = [i32::from(quant_q15(p[0])), i32::from(quant_q15(p[1])), 0, 0, 0];
            assert_eq!(*g, want, "point {i} mismatch (q15)");
        }
    }

    #[test]
    fn roundtrips_a_circle() {
        let pts: Vec<[f32; 2]> = (0..200)
            .map(|i| {
                let t = (i as f32 / 200.0) * std::f32::consts::TAU;
                [0.5 + 0.4 * t.cos(), 0.5 + 0.4 * t.sin()]
            })
            .collect();
        roundtrip_points(&pts);
    }

    #[test]
    fn roundtrips_a_line() {
        let pts: Vec<[f32; 2]> = (0..64).map(|i| [i as f32 / 63.0, i as f32 / 63.0]).collect();
        roundtrip_points(&pts);
    }

    #[test]
    fn roundtrips_long_stroke_long_header() {
        // > ~4066 points forces the long pack header (metaLen > 0xFE) — exercise that path.
        let pts: Vec<[f32; 2]> = (0..9000)
            .map(|i| {
                let t = (i as f32 / 300.0) * std::f32::consts::TAU;
                let r = 0.05 + 0.4 * (i as f32 / 9000.0);
                [0.5 + r * t.cos(), 0.5 + r * t.sin()]
            })
            .collect();
        roundtrip_points(&pts);
    }

    #[test]
    fn handles_tiny_strokes() {
        roundtrip_points(&[[0.1, 0.2]]);
        roundtrip_points(&[[0.1, 0.2], [0.3, 0.4]]);
        roundtrip_points(&[[0.1, 0.2], [0.3, 0.4], [0.5, 0.6]]);
    }

    /// Emit a sample `.gwyph` + the expected q15 points to the scratchpad, for cross-checking
    /// against the REAL Whisper reader (`parseGwyphPayload`). Run explicitly:
    /// `cargo test -p neuron --lib gwyph::tests::emit_sample -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn emit_sample_for_reference_reader() {
        let dir = std::env::var("GWYPH_SAMPLE_DIR").expect("set GWYPH_SAMPLE_DIR");
        // a spiral-ish circle — enough segmentation to exercise multiple blocks.
        let pts: Vec<[f32; 2]> = (0..73)
            .map(|i| {
                let t = (i as f32 / 73.0) * std::f32::consts::TAU * 1.5;
                let r = 0.1 + 0.38 * (i as f32 / 73.0);
                [0.5 + r * t.cos(), 0.5 + r * t.sin()]
            })
            .collect();
        let file = encode_stroke(&pts, &StrokeStyle::default());
        std::fs::write(format!("{dir}/sample.gwyph"), &file).unwrap();
        // expected q15 points the reference reader should reproduce.
        let q15: Vec<[i32; 2]> = pts
            .iter()
            .map(|p| [i32::from(quant_q15(p[0])), i32::from(quant_q15(p[1]))])
            .collect();
        let json: Vec<String> = q15.iter().map(|p| format!("[{},{}]", p[0], p[1])).collect();
        std::fs::write(
            format!("{dir}/expected.json"),
            format!("[{}]", json.join(",")),
        )
        .unwrap();
        eprintln!("wrote {} bytes, {} points", file.len(), pts.len());
    }
}
