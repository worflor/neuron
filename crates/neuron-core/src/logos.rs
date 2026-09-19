// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../../../LICENSE.md and ../../../LICENSES/WLCSL-1.0.md.

//! Logos — the attention organ. A Rust port of the Whisper Logos 0D adaptive entropy
//! predictor (`logos.wat`, by Woflo Labs), specialized for KNOCKBACK's
//! one new need: a
//! **per-byte surprise probe**.
//!
//! Logos models a byte stream with eight axes of attention and mixes them with the **Born
//! rule** in amplitude space (`p = (Σwᵢ√pᵢ)² / ((Σwᵢ√pᵢ)² + (Σwᵢ√(1−pᵢ))²)`), exactly the
//! inclusion-exclusion structure of the spatial Möbius codecs folded into time. Each axis
//! is a KT-smoothed bit-tree predictor; weights are `wᵢ = |pᵢ − 0.5| · min(ln(1+nᵢ), cap)`
//! — confidence × capped evidence. A deep-match axis (M) injects independent log-odds.
//!
//! The original is a lossless arithmetic coder. KNOCKBACK does not compress anything; it
//! reads the model's **information content** `−log₂ p(byte)` as a novelty signal, and the
//! cross-prediction of two streams as a *sync* signal (mutual compressibility = flow). So
//! this port exposes [`LogosStream::surprise`] as the primary surface. A real binary range
//! coder ([`encode`]/[`decode`]) drives the same predictor for a genuine lossless
//! round-trip — proving the model is well-formed — but the codec is **not byte-identical to
//! the reference WASM**, so it does not (yet) back the production `.gwyph` writer; that flip
//! awaits validation against WASM test vectors.

// ── KT priors and evidence caps (from the logos.wat header, verbatim) ────────

const ALPHA_F0: f64 = 0.5;
const ALPHA_O2: f64 = 0.125;
const ALPHA_E: f64 = 0.5;
const ALPHA_P2N: f64 = 0.25;
const ALPHA_U: f64 = 0.5;
const ALPHA_V: f64 = 0.25;
const ALPHA_AB: f64 = 0.25;

// split evidence caps: F0=ln2, U=ln3, the dense byte-context trees=ln4.
const CAP_F0: f64 = std::f64::consts::LN_2; // ≈0.693
const CAP_U: f64 = 1.0986122886681098; // ln 3
const CAP_DENSE: f64 = 1.3862943611198906; // ln 4

/// Evaporation cadence: every this many bytes, counts relax toward the prior by `f`.
const EVAP_PERIOD: usize = 64;

// ── a single KT bit-tree context table ───────────────────────────────────────
//
// A byte is predicted MSB-first down a binary tree: ctx starts at 1, and after each bit
// ctx = (ctx<<1)|bit. The 255 internal nodes (ctx 1..=255) are exactly the non-trivial
// elements of the Boolean lattice Λ*(R⁸) — the same 255 neighbours the 8D Möbius predictor
// interrogates, folded into time. Each node holds two soft counts (c0, c1).

#[derive(Clone)]
struct Tree {
    /// `contexts × 256 × 2` soft counts, row-major: `[(ctx*256 + node)*2 + bit]`.
    c: Vec<f32>,
    alpha: f64,
    cap: f64,
}

impl Tree {
    fn new(contexts: usize, alpha: f64, cap: f64) -> Tree {
        Tree {
            c: vec![0.0; contexts * 256 * 2],
            alpha,
            cap,
        }
    }

    #[inline]
    fn cell(&self, ctx: usize, node: usize) -> usize {
        (ctx * 256 + node) * 2
    }

    /// KT estimate that the next bit is 1, plus the evidence count n = c0+c1.
    #[inline]
    fn predict(&self, ctx: usize, node: usize) -> (f64, f64) {
        let i = self.cell(ctx, node);
        let c0 = f64::from(self.c[i]);
        let c1 = f64::from(self.c[i + 1]);
        let n = c0 + c1;
        let p1 = (c1 + self.alpha) / (n + 2.0 * self.alpha);
        (p1, n)
    }

    /// The Born-pool weight of this axis: confidence × capped evidence.
    #[inline]
    fn weight(&self, p1: f64, n: f64) -> f64 {
        (p1 - 0.5).abs() * (1.0 + n).ln().min(self.cap)
    }

    #[inline]
    fn update(&mut self, ctx: usize, node: usize, bit: u8) {
        let i = self.cell(ctx, node) + bit as usize;
        self.c[i] += 1.0;
    }

    /// Relax all counts toward the prior by factor `f` (thermodynamic evaporation).
    fn evaporate(&mut self, f: f32) {
        for v in &mut self.c {
            *v *= f;
        }
    }
}

// ── the deep-match axis (M): independent log-odds injection ───────────────────
//
// A small PPM-style exact-match head: hash the last few bytes, remember where that context
// last occurred, and predict the byte that followed it. Confidence grows with the run of
// consecutive correct matches (the "crystal" regime). Independent of the amplitude pool, so
// it injects log-odds rather than joining the Born mix.

struct MatchAxis {
    hist: Vec<u8>,
    last_seen: Vec<i64>, // hash(order-4) → last index, -1 if none
    match_pos: i64,
    run: u32,
    mask: u64,
}

impl MatchAxis {
    fn new() -> MatchAxis {
        MatchAxis {
            hist: Vec::new(),
            last_seen: vec![-1; 1 << 20],
            match_pos: -1,
            run: 0,
            mask: (1 << 20) - 1,
        }
    }

    #[inline]
    fn hash4(&self) -> Option<usize> {
        let n = self.hist.len();
        if n < 4 {
            return None;
        }
        let h = (u64::from(self.hist[n - 1]) << 24)
            ^ (u64::from(self.hist[n - 2]) << 16)
            ^ (u64::from(self.hist[n - 3]) << 8)
            ^ u64::from(self.hist[n - 4]);
        let h = (h.wrapping_mul(2654435761)) & self.mask;
        Some(h as usize)
    }

    /// The predicted next byte (if a match is active) and a confidence in `[0, 1]`.
    fn predict(&self) -> Option<(u8, f64)> {
        if self.match_pos >= 0 && (self.match_pos as usize) < self.hist.len() {
            let b = self.hist[self.match_pos as usize];
            let conf = 1.0 - (-f64::from(self.run) / 4.0).exp(); // grows with run length
            Some((b, conf))
        } else {
            None
        }
    }

    /// After a byte is finalized: verify/extend the match, then record the new context.
    fn end_byte(&mut self, byte: u8) {
        // did the active match correctly predict this byte?
        if let Some((pred, _)) = self.predict() {
            if pred == byte {
                self.run += 1;
                self.match_pos += 1;
            } else {
                self.run = 0;
                self.match_pos = -1;
            }
        }
        self.hist.push(byte);
        if let Some(h) = self.hash4() {
            let prev = self.last_seen[h];
            self.last_seen[h] = self.hist.len() as i64 - 1;
            if self.match_pos < 0 && prev >= 0 {
                // start a new match just past the remembered context
                self.match_pos = prev + 1;
                self.run = 0;
            }
        }
    }
}

// ── the model: all axes, the Born mix, the per-bit interface ─────────────────

/// The Logos predictor over a byte stream. Drives [`LogosStream`], [`encode`], [`decode`]
/// identically so surprise and coding agree bit-for-bit.
struct Model {
    f0: Tree,
    o2: Tree,
    e: Tree,
    p2n: Tree,
    u: Tree, // bit-lane AR(2): 4 slots × 8 lanes = 32 contexts (we index by lane*4+slot)
    v: Tree,
    ab: Tree,

    m: MatchAxis,

    // cross-byte context
    prev1: u8,
    prev2: u8,
    // E-axis decayed AR(2) over byte values
    e_saa: f64,
    e_sbb: f64,
    e_sab: f64,
    e_sta: f64,
    e_stb: f64,
    // V-axis rolling L1 window (last 16 byte magnitudes around the running mean)
    vwin: [u8; 16],
    vpos: usize,
    // spatial stride (0 = pure temporal; Ab inert)
    stride: usize,

    // within-byte tree context (1..255)
    ctx: usize,
    bit_k: u32, // current bit position 7..0
    nbytes: usize,
}

impl Model {
    fn new(stride: usize) -> Model {
        Model {
            f0: Tree::new(1, ALPHA_F0, CAP_F0),
            o2: Tree::new(256, ALPHA_O2, CAP_DENSE),
            e: Tree::new(256, ALPHA_E, CAP_DENSE),
            p2n: Tree::new(16, ALPHA_P2N, CAP_DENSE),
            u: Tree::new(32, ALPHA_U, CAP_U),
            v: Tree::new(16, ALPHA_V, CAP_DENSE),
            ab: Tree::new(256, ALPHA_AB, CAP_DENSE),
            m: MatchAxis::new(),
            prev1: 0,
            prev2: 0,
            e_saa: 0.0,
            e_sbb: 0.0,
            e_sab: 0.0,
            e_sta: 0.0,
            e_stb: 0.0,
            vwin: [0; 16],
            vpos: 0,
            stride,
            ctx: 1,
            bit_k: 7,
            nbytes: 0,
        }
    }

    /// Predicted byte from the decayed AR(2) over byte values (E-axis context).
    #[inline]
    fn eng_pred(&self) -> usize {
        let det = self.e_saa * self.e_sbb - self.e_sab * self.e_sab;
        if det.abs() < 1e-9 {
            return self.prev1 as usize;
        }
        let inv = 1.0 / det;
        let k = (self.e_sta * self.e_sbb - self.e_sab * self.e_stb) * inv;
        let g = (self.e_saa * self.e_stb - self.e_sab * self.e_sta) * inv;
        let pred = k * f64::from(self.prev1) - g * f64::from(self.prev2);
        (pred.round().clamp(0.0, 255.0)) as usize
    }

    /// V-axis volatility bin: log2 of the L1 sum over the last 16 bytes, 16 bins.
    #[inline]
    fn vol_bin(&self) -> usize {
        let mut s: u32 = 0;
        for &b in &self.vwin {
            s += u32::from(b);
        }
        // log2 of the sum, capped to 16 bins
        let l = if s == 0 { 0 } else { 32 - s.leading_zeros() };
        (l as usize).min(15)
    }

    /// The U-axis context for the current bit lane: (`prev1_bit`<<`1)|prev2_bit` within lane k.
    #[inline]
    fn u_ctx(&self) -> usize {
        let b1 = ((self.prev1 >> self.bit_k) & 1) as usize;
        let b2 = ((self.prev2 >> self.bit_k) & 1) as usize;
        let slot = (b1 << 1) | b2;
        (self.bit_k as usize) * 4 + slot
    }

    /// Probability the next bit is 1, mixing every axis (Born pool + M log-odds).
    fn p1(&self) -> f64 {
        let node = self.ctx;
        let eng = self.eng_pred();
        let vb = self.vol_bin();
        let uc = self.u_ctx();

        // amplitude pool
        let axes: [(f64, f64); 7] = [
            self.f0.predict(0, node),
            self.o2.predict(self.prev1 as usize, node),
            self.e.predict(eng, node),
            self.p2n.predict((self.prev2 >> 4) as usize, node),
            self.u.predict(uc, node),
            self.v.predict(vb, node),
            if self.stride > 0 {
                self.ab.predict(self.above_byte() as usize, node)
            } else {
                (0.5, 0.0)
            },
        ];
        let trees: [&Tree; 7] = [
            &self.f0, &self.o2, &self.e, &self.p2n, &self.u, &self.v, &self.ab,
        ];

        // Born-rule amplitude mixing: interfere √p and √(1−p).
        let mut amp1 = 0.0f64;
        let mut amp0 = 0.0f64;
        for (i, &(p, n)) in axes.iter().enumerate() {
            if i == 6 && self.stride == 0 {
                continue; // Ab inert
            }
            let w = trees[i].weight(p, n);
            if w <= 0.0 {
                continue;
            }
            amp1 += w * p.sqrt();
            amp0 += w * (1.0 - p).sqrt();
        }
        let mut p = if amp1 + amp0 < 1e-12 {
            0.5
        } else {
            let s1 = amp1 * amp1;
            let s0 = amp0 * amp0;
            s1 / (s1 + s0)
        };

        // M-axis: independent log-odds injection.
        if let Some((mbyte, conf)) = self.m.predict() {
            let mbit = f64::from((mbyte >> self.bit_k) & 1);
            // a confident match pushes the bit toward its predicted value
            let strength = 3.0 * conf; // log-odds magnitude
            let logit_m = if mbit > 0.5 { strength } else { -strength };
            let logit = logit(p) + logit_m;
            p = sigmoid(logit);
        }

        p.clamp(1e-6, 1.0 - 1e-6)
    }

    #[inline]
    fn above_byte(&self) -> u8 {
        let n = self.m.hist.len();
        if self.stride > 0 && n >= self.stride {
            self.m.hist[n - self.stride]
        } else {
            0
        }
    }

    /// Advance one bit: update every axis's tree, walk the within-byte context.
    fn push_bit(&mut self, bit: u8) {
        let node = self.ctx;
        let eng = self.eng_pred();
        let vb = self.vol_bin();
        let uc = self.u_ctx();
        self.f0.update(0, node, bit);
        self.o2.update(self.prev1 as usize, node, bit);
        self.e.update(eng, node, bit);
        self.p2n.update((self.prev2 >> 4) as usize, node, bit);
        self.u.update(uc, node, bit);
        self.v.update(vb, node, bit);
        if self.stride > 0 {
            let ab = self.above_byte() as usize;
            self.ab.update(ab, node, bit);
        }
        self.ctx = (self.ctx << 1) | bit as usize;
        if self.bit_k == 0 {
            self.bit_k = 7;
        } else {
            self.bit_k -= 1;
        }
    }

    /// Finalize a byte: update cross-byte state, match chains, E/V trackers, evaporation.
    fn end_byte(&mut self, byte: u8) {
        // E-axis decayed AR(2) stats (decay older evidence, fold in the new triple)
        const DECAY: f64 = 0.98;
        let t = f64::from(byte);
        let a = f64::from(self.prev1);
        let b = f64::from(self.prev2);
        self.e_saa = self.e_saa * DECAY + a * a;
        self.e_sbb = self.e_sbb * DECAY + b * b;
        self.e_sab = self.e_sab * DECAY + a * b;
        self.e_sta = self.e_sta * DECAY + t * a;
        self.e_stb = self.e_stb * DECAY + t * b;

        // V-axis window
        self.vwin[self.vpos] = byte;
        self.vpos = (self.vpos + 1) & 15;

        // M-axis chains
        self.m.end_byte(byte);

        // shift byte context
        self.prev2 = self.prev1;
        self.prev1 = byte;

        // within-byte ctx reset
        self.ctx = 1;
        self.bit_k = 7;
        self.nbytes += 1;

        // thermodynamic evaporation: structured → freeze (f→1), random → melt (f→1/e).
        if self.nbytes.is_multiple_of(EVAP_PERIOD) {
            let c = self.confidence();
            let one_minus = 1.0 - c;
            let f = (-(one_minus * one_minus)).exp() as f32; // e^(−(1−c)²)
            self.o2.evaporate(f);
            self.e.evaporate(f);
            self.p2n.evaporate(f);
            self.v.evaporate(f);
            if self.stride > 0 {
                self.ab.evaporate(f);
            }
        }
    }

    /// A coarse confidence proxy for evaporation: how peaked F0 has become.
    fn confidence(&self) -> f64 {
        // use the order-0 root node spread as a cheap thermometer
        let (p, n) = self.f0.predict(0, 1);
        let peak = (p - 0.5).abs() * 2.0; // 0..1
        let evidence = (1.0 + n).ln().min(4.0) / 4.0;
        (2.0 * peak * evidence).min(1.0)
    }
}

#[inline]
fn logit(p: f64) -> f64 {
    (p / (1.0 - p)).ln()
}
#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

// ── public surface ────────────────────────────────────────────────────────────

/// A streaming Logos predictor. Feed it bytes; read the information content of each.
pub struct LogosStream {
    model: Model,
}

impl LogosStream {
    #[must_use]
    pub fn new() -> LogosStream {
        LogosStream {
            model: Model::new(0),
        }
    }

    /// The information content of `byte` under the current model, in **bits**
    /// (`−log₂ p(byte)`), then learn from it. Low for an expected byte (you are repeating a
    /// groove), high for a surprising one (you are breaking new ground). This is the raw
    /// novelty signal the rhythm familiar's attention system reads.
    pub fn surprise(&mut self, byte: u8) -> f32 {
        let mut bits = 0.0f64;
        for k in (0..8).rev() {
            let p = self.model.p1();
            let bit = (byte >> k) & 1;
            let pb = if bit == 1 { p } else { 1.0 - p };
            bits += -pb.log2();
            self.model.push_bit(bit);
            let _ = k;
        }
        self.model.end_byte(byte);
        bits as f32
    }

    /// Feed a byte without reading its surprise (state update only).
    pub fn observe(&mut self, byte: u8) {
        let _ = self.surprise(byte);
    }

    /// Mean surprise (bits/byte) of a whole slice, learning as it goes. A *fresh* stream's
    /// cross-entropy against everything seen so far — the bedrock of the sync signal.
    pub fn surprise_of(&mut self, bytes: &[u8]) -> f32 {
        if bytes.is_empty() {
            return 0.0;
        }
        let mut s = 0.0f32;
        for &b in bytes {
            s += self.surprise(b);
        }
        s / bytes.len() as f32
    }
}

impl Default for LogosStream {
    fn default() -> Self {
        Self::new()
    }
}

// ── a real lossless round-trip (binary range coder over the same predictor) ──
//
// Not byte-identical to the reference WASM, but a genuine arithmetic coder driven by the
// exact predictor above — it proves the model is a proper probability distribution. The
// `0x00`/`0xFF` self-describing framing matches the reference container so a future
// byte-exact port can drop in behind the same API.

#[cfg(test)]
const RAW_MODE: u8 = 0xFF;
#[cfg(test)]
const LOGOS_MODE: u8 = 0x00;

/// Compress `data`. Returns `[mode byte][payload]`; falls back to raw passthrough when
/// coding does not help, so output is never more than `len + 1`. Test-only forward-work for
/// the `.gwyph` codec — no runtime caller yet.
#[cfg(test)]
#[must_use]
pub fn encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }
    let mut model = Model::new(0);
    let mut enc = RangeEncoder::new();
    for &byte in data {
        for k in (0..8).rev() {
            let p = model.p1();
            let bit = (byte >> k) & 1;
            enc.encode_bit(p, bit);
            model.push_bit(bit);
            let _ = k;
        }
        model.end_byte(byte);
    }
    let payload = enc.finish();
    if payload.len() >= data.len() {
        let mut out = Vec::with_capacity(1 + data.len());
        out.push(RAW_MODE);
        out.extend_from_slice(data);
        out
    } else {
        let mut out = Vec::with_capacity(1 + payload.len());
        out.push(LOGOS_MODE);
        out.extend_from_slice(&payload);
        out
    }
}

/// Decompress `len` bytes from `[mode byte][payload]`. Test-only (see [`encode`]).
#[cfg(test)]
#[must_use]
pub fn decode(data: &[u8], len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    match data[0] {
        RAW_MODE => data[1..=len].to_vec(),
        LOGOS_MODE => {
            let mut model = Model::new(0);
            let mut dec = RangeDecoder::new(&data[1..]);
            let mut out = Vec::with_capacity(len);
            for _ in 0..len {
                let mut byte = 0u8;
                for _ in 0..8 {
                    let p = model.p1();
                    let bit = dec.decode_bit(p);
                    byte = (byte << 1) | bit;
                    model.push_bit(bit);
                }
                model.end_byte(byte);
                out.push(byte);
            }
            out
        }
        other => panic!("logos::decode: unknown mode byte 0x{other:02x}"),
    }
}

// 32-bit carryless binary range coder (Subbotin-style).
#[cfg(test)]
struct RangeEncoder {
    low: u64,
    range: u32,
    out: Vec<u8>,
}

#[cfg(test)]
impl RangeEncoder {
    fn new() -> RangeEncoder {
        RangeEncoder {
            low: 0,
            range: 0xFFFF_FFFF,
            out: Vec::new(),
        }
    }

    fn encode_bit(&mut self, p1: f64, bit: u8) {
        // split point for bit==1 region
        let mut split = (f64::from(self.range) * p1) as u32;
        if split == 0 {
            split = 1;
        }
        if split >= self.range {
            split = self.range - 1;
        }
        if bit == 1 {
            self.range = split;
        } else {
            self.low += u64::from(split);
            self.range -= split;
        }
        // renormalize: emit top byte whenever range shrinks below 2^24
        while self.range < (1 << 24) {
            self.out.push((self.low >> 32) as u8);
            self.low = (self.low << 8) & 0xFF_FFFF_FFFF;
            self.range <<= 8;
        }
    }

    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.out.push((self.low >> 32) as u8);
            self.low = (self.low << 8) & 0xFF_FFFF_FFFF;
        }
        self.out
    }
}

#[cfg(test)]
struct RangeDecoder<'a> {
    code: u64,
    low: u64,
    range: u32,
    inp: &'a [u8],
    pos: usize,
}

#[cfg(test)]
impl<'a> RangeDecoder<'a> {
    fn new(inp: &'a [u8]) -> RangeDecoder<'a> {
        let mut d = RangeDecoder {
            code: 0,
            low: 0,
            range: 0xFFFF_FFFF,
            inp,
            pos: 0,
        };
        for _ in 0..5 {
            d.code = (d.code << 8) | u64::from(d.next_byte());
        }
        d
    }

    #[inline]
    fn next_byte(&mut self) -> u8 {
        let b = self.inp.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn decode_bit(&mut self, p1: f64) -> u8 {
        let mut split = (f64::from(self.range) * p1) as u32;
        if split == 0 {
            split = 1;
        }
        if split >= self.range {
            split = self.range - 1;
        }
        let offset = self.code - self.low;
        let bit = if offset < u64::from(split) {
            self.range = split;
            1
        } else {
            self.low += u64::from(split);
            self.range -= split;
            0
        };
        while self.range < (1 << 24) {
            self.code = ((self.code << 8) | u64::from(self.next_byte())) & 0xFF_FFFF_FFFF;
            self.low = (self.low << 8) & 0xFF_FFFF_FFFF;
            self.range <<= 8;
        }
        bit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(data: &[u8]) {
        let enc = encode(data);
        let dec = decode(&enc, data.len());
        assert_eq!(dec, data, "round-trip mismatch (mode {:#x})", enc[0]);
    }

    #[test]
    fn roundtrip_empty_and_single() {
        assert!(encode(&[]).is_empty());
        round_trip(&[0x42]);
    }

    #[test]
    fn roundtrip_zeros() {
        round_trip(&vec![0u8; 1024]);
    }

    #[test]
    fn roundtrip_text() {
        let s = b"the quick brown fox jumps over the lazy dog. the quick brown fox again.";
        round_trip(s);
    }

    #[test]
    fn roundtrip_alternating() {
        let d: Vec<u8> = (0..1000).map(|i| (i & 1) as u8).collect();
        round_trip(&d);
    }

    #[test]
    fn roundtrip_random() {
        // LCG pseudo-random — incompressible, must still round-trip (raw fallback ok).
        let mut s: u32 = 12345;
        let d: Vec<u8> = (0..2048)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 24) as u8
            })
            .collect();
        round_trip(&d);
    }

    #[test]
    fn roundtrip_gradient() {
        let d: Vec<u8> = (0..1024).map(|i| (i & 0xFF) as u8).collect();
        round_trip(&d);
    }

    #[test]
    fn structured_compresses() {
        // a repeating pattern should compress well below 8 bits/byte
        let unit = b"KNOCKBACK";
        let mut d = Vec::new();
        for _ in 0..200 {
            d.extend_from_slice(unit);
        }
        let enc = encode(&d);
        assert_eq!(enc[0], LOGOS_MODE, "structured data should use the coder");
        let bps = (enc.len() as f64 * 8.0) / d.len() as f64;
        assert!(
            bps < 2.0,
            "repeating pattern should compress hard, got {bps:.2} b/s"
        );
    }

    #[test]
    fn surprise_falls_as_pattern_repeats() {
        // The signal the game lives on: a groove gets less surprising the more you play it.
        let mut s = LogosStream::new();
        let unit: [u8; 4] = [0x10, 0x20, 0x10, 0x30];
        // warm up
        let mut first = 0.0;
        let mut last = 0.0;
        for rep in 0..64 {
            let mut total = 0.0;
            for &b in &unit {
                total += s.surprise(b);
            }
            if rep == 0 {
                first = total;
            }
            last = total;
        }
        assert!(
            last < first,
            "groove must become predictable: {first:.2} → {last:.2}"
        );
        assert!(
            last < first * 0.6,
            "and substantially so: {first:.2} → {last:.2}"
        );
    }

    #[test]
    fn random_is_more_surprising_than_structure() {
        let mut s_struct = LogosStream::new();
        let mut s_rand = LogosStream::new();
        let structured: Vec<u8> = (0..512).map(|i| (i % 4) as u8 * 16).collect();
        let mut seed: u32 = 99;
        let random: Vec<u8> = (0..512)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8
            })
            .collect();
        let bps_struct = s_struct.surprise_of(&structured);
        let bps_rand = s_rand.surprise_of(&random);
        assert!(
            bps_rand > bps_struct + 1.0,
            "random ({bps_rand:.2}) should out-surprise structure ({bps_struct:.2})"
        );
    }

    #[test]
    fn surprise_is_bounded_and_finite() {
        let mut s = LogosStream::new();
        for b in 0u16..600 {
            let v = s.surprise((b & 0xFF) as u8);
            assert!(
                v.is_finite() && (0.0..=64.0).contains(&v),
                "surprise {v} out of range"
            );
        }
    }
}
