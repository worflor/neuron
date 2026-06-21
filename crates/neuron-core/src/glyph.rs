//! Glyph eigenmotion — faithful Rust port of `glyph.wat` (+ the lane/segment logic
//! from `live-wasm-glyph.ts`), specialized to 2D mouse motion and gesture recognition.
//!
//! A motion path is the complex sequence z[n] = x + i·y. Each *block* is modeled by a
//! damped complex harmonic oscillator z[n] = K·z[n-1] − G·z[n-2], with K,G fit by
//! complex least-squares (Cramer + Tikhonov), quantized to Q14 and clamped to the
//! stability manifold exactly as the codec does. The eigenvalues of λ² − Kλ + G = 0
//! are the block's natural frequencies.
//!
//! For recognition we fit on **velocity** (translation-invariant) over an **arc-length
//! resampled** path (speed/size-invariant) and keep the **sign** of the rotation so a
//! CW loop differs from a CCW loop. A gesture is a *sequence* of eigen-modes, matched
//! by DTW under a user-attunable [`GlyphConfig`].

use serde::{Deserialize, Serialize};

pub const Q14: f64 = 16384.0;
pub const GLYPH_BLOCK_SIZE: usize = 16;

const G_MAG2_MAX: f64 = 268_435_456.0; // Q14²        (|G| ≤ 1)
const K_MAG2_MAX: f64 = 1_073_741_824.0; // (2·Q14)²   (|K| ≤ 2)

// ── complex ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct C {
    pub re: f64,
    pub im: f64,
}

// Compact complex-math helper: add/sub/mul/scale read clearly namespaced as `C::op`;
// not worth operator-overloading churn across the eigenmotion math.
#[allow(clippy::should_implement_trait)]
impl C {
    pub fn new(re: f64, im: f64) -> C {
        C { re, im }
    }
    pub fn abs(self) -> f64 {
        self.re.hypot(self.im)
    }
    pub fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }
    pub fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    pub fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    pub fn mul(self, o: C) -> C {
        C::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    pub fn scale(self, s: f64) -> C {
        C::new(self.re * s, self.im * s)
    }
    /// Principal complex square root.
    pub fn sqrt(self) -> C {
        let r = self.abs();
        let re = ((r + self.re) * 0.5).max(0.0).sqrt();
        let mut im = ((r - self.re) * 0.5).max(0.0).sqrt();
        if self.im < 0.0 {
            im = -im;
        }
        C::new(re, im)
    }
}

// ── faithful fixed-point helpers (match glyph.wat $fit tail) ─────────────────

#[inline]
fn clamp_i16(v: f64) -> i32 {
    v.round_ties_even().clamp(-32768.0, 32767.0) as i32
}

fn stabilize(mut k: C, mut g: C) -> (C, C) {
    let gm = g.abs();
    if gm > 1.0 {
        g = g.scale(1.0 / gm);
    }
    let km = k.abs();
    if km > 2.0 {
        k = k.scale(2.0 / km);
    }
    (k, g)
}

fn quantize_q14(k: C, g: C) -> (i32, i32, i32, i32) {
    let mut kr = clamp_i16(k.re * Q14);
    let mut ki = clamp_i16(k.im * Q14);
    let mut gr = clamp_i16(g.re * Q14);
    let mut gi = clamp_i16(g.im * Q14);
    let gm2 = (gr as f64) * (gr as f64) + (gi as f64) * (gi as f64);
    if gm2 > G_MAG2_MAX {
        let s = Q14 / gm2.sqrt();
        gr = (gr as f64 * s).round_ties_even() as i32;
        gi = (gi as f64 * s).round_ties_even() as i32;
    }
    let km2 = (kr as f64) * (kr as f64) + (ki as f64) * (ki as f64);
    if km2 > K_MAG2_MAX {
        let s = (2.0 * Q14) / km2.sqrt();
        kr = (kr as f64 * s).round_ties_even() as i32;
        ki = (ki as f64 * s).round_ties_even() as i32;
    }
    (kr, ki, gr, gi)
}

// ── fit ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct GlyphFit {
    pub k: C,
    pub g: C,
    pub kq: (i32, i32),
    pub gq: (i32, i32),
    pub lambda1: C,
    pub lambda2: C,
    pub residual: f64,
    pub mean_step: f64,
    pub n: usize,
}

/// Fit z[n] = K·z[n-1] − G·z[n-2] by complex least-squares over the whole slice.
/// `z` must include the 2 history samples at its front.
pub fn fit(z: &[C]) -> Option<GlyphFit> {
    let len = z.len();
    if len < 3 {
        return None;
    }
    let (mut s_aa, mut s_bb) = (0.0f64, 0.0f64);
    let (mut s_abr, mut s_abi) = (0.0f64, 0.0f64);
    let (mut s_tar, mut s_tai) = (0.0f64, 0.0f64);
    let (mut s_tbr, mut s_tbi) = (0.0f64, 0.0f64);

    for j in 2..len {
        let t = z[j];
        let a = z[j - 1];
        let b = z[j - 2];
        s_aa += a.re * a.re + a.im * a.im;
        s_bb += b.re * b.re + b.im * b.im;
        s_abr += a.re * b.re + a.im * b.im;
        s_abi += a.im * b.re - a.re * b.im;
        s_tar += t.re * a.re + t.im * a.im;
        s_tai += t.im * a.re - t.re * a.im;
        s_tbr += t.re * b.re + t.im * b.im;
        s_tbi += t.im * b.re - t.re * b.im;
    }

    let eps = s_aa.max(s_bb) * 1e-6;
    s_aa += eps;
    s_bb += eps;

    let det = s_aa * s_bb - (s_abr * s_abr + s_abi * s_abi);
    let (kf, gf) = if det.abs() < 1e-30 {
        (C::new(2.0, 0.0), C::new(1.0, 0.0))
    } else {
        let inv = 1.0 / det;
        let kr = (s_tar * s_bb - s_abr * s_tbr - s_abi * s_tbi) * inv;
        let ki = (s_tai * s_bb - s_abr * s_tbi + s_abi * s_tbr) * inv;
        let gr = (-(s_aa * s_tbr) + s_tar * s_abr - s_tai * s_abi) * inv;
        let gi = (-(s_aa * s_tbi) + s_tar * s_abi + s_tai * s_abr) * inv;
        (C::new(kr, ki), C::new(gr, gi))
    };

    let (ks, gs) = stabilize(kf, gf);
    let (kr, ki, gr, gi) = quantize_q14(ks, gs);
    let k = C::new(kr as f64 / Q14, ki as f64 / Q14);
    let g = C::new(gr as f64 / Q14, gi as f64 / Q14);
    Some(finalize(z, k, g, (kr, ki), (gr, gi)))
}

fn finalize(z: &[C], k: C, g: C, kq: (i32, i32), gq: (i32, i32)) -> GlyphFit {
    let disc = k.mul(k).sub(g.scale(4.0)).sqrt();
    let lambda1 = k.add(disc).scale(0.5);
    let lambda2 = k.sub(disc).scale(0.5);
    let mut err = 0.0;
    let mut cnt = 0usize;
    for j in 2..z.len() {
        let pred = k.mul(z[j - 1]).sub(g.mul(z[j - 2]));
        err += pred.sub(z[j]).abs();
        cnt += 1;
    }
    let mut step = 0.0;
    for j in 1..z.len() {
        step += z[j].sub(z[j - 1]).abs();
    }
    GlyphFit {
        k,
        g,
        kq,
        gq,
        lambda1,
        lambda2,
        residual: if cnt > 0 { err / cnt as f64 } else { 0.0 },
        mean_step: if z.len() > 1 {
            step / (z.len() - 1) as f64
        } else {
            0.0
        },
        n: z.len(),
    }
}

// ── config (the three attunable properties) ─────────────────────────────────

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct GlyphConfig {
    /// weight on damping |λ| (open arc vs sustained loop)
    pub w_damping: f64,
    /// weight on signed curvature arg(λ) (handedness + tightness)
    pub w_curve: f64,
    /// weight on irregularity (residual / step)
    pub w_resid: f64,
    /// DTW score above which a match is rejected as "unknown"
    pub threshold: f64,
    /// arc-length resample count (speed/size normalization)
    pub resample: usize,
    /// weight on the physical invariants (winding/bending/closure) in matching
    pub w_invariant: f64,
}

impl Default for GlyphConfig {
    fn default() -> Self {
        GlyphConfig {
            w_damping: 1.0,
            w_curve: 1.0,
            w_resid: 0.25,
            threshold: 0.30,
            resample: 24, // points per gesture-extent (size-relative density)
            w_invariant: 0.5,
        }
    }
}

// ── signature + recognition ──────────────────────────────────────────────────

/// Scale/speed-invariant, direction-sensitive signature of one block.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Sig {
    pub mag: f64,        // dominant |λ| — damping
    pub rot: f64,        // dominant arg(λ) — SIGNED rotation rad/sample (handedness)
    pub resid_norm: f64, // residual / mean step — irregularity
}

pub fn signature(f: &GlyphFit) -> Sig {
    let dom = if f.lambda1.abs() >= f.lambda2.abs() {
        f.lambda1
    } else {
        f.lambda2
    };
    Sig {
        mag: dom.abs(),
        rot: dom.arg(),
        resid_norm: f.residual / (f.mean_step + 1e-9),
    }
}

fn ang_dist(a: f64, b: f64) -> f64 {
    let mut d = (a - b).abs();
    if d > std::f64::consts::PI {
        d = std::f64::consts::TAU - d;
    }
    d
}

/// Weighted distance between window signatures. Curvature keeps its sign, so +ω (one
/// handedness) and −ω (the other) are far apart — CW and CCW are different gestures.
pub fn sig_distance(a: Sig, b: Sig, cfg: &GlyphConfig) -> f64 {
    let dm = a.mag - b.mag;
    let dr = ang_dist(a.rot, b.rot);
    let dn = (a.resid_norm - b.resid_norm).abs().min(2.0);
    (cfg.w_damping * dm * dm + cfg.w_curve * dr * dr + cfg.w_resid * dn * dn).sqrt()
}

/// Length-normalized DTW between two signature sequences (gesture words).
pub fn dtw(a: &[Sig], b: &[Sig], cfg: &GlyphConfig) -> f64 {
    let (n, m) = (a.len(), b.len());
    if n == 0 || m == 0 {
        return f64::INFINITY;
    }
    let at = |i: usize, j: usize| i * (m + 1) + j;
    let mut dp = vec![f64::INFINITY; (n + 1) * (m + 1)];
    dp[at(0, 0)] = 0.0;
    for i in 1..=n {
        for j in 1..=m {
            let cost = sig_distance(a[i - 1], b[j - 1], cfg);
            let best = dp[at(i - 1, j)]
                .min(dp[at(i, j - 1)])
                .min(dp[at(i - 1, j - 1)]);
            dp[at(i, j)] = cost + best;
        }
    }
    // Normalize by the QUERY length (first arg). For 1-NN this is constant across
    // templates, so ranking == raw DTW: a long, varied "superset" template (e.g. an S
    // that contains both a CW and a CCW arc) must consume ALL its windows against the
    // query and pays full cost for the unmatched ones — it can't masquerade as a circle.
    // (A template-dependent divisor like (n+m) or max(n,m) would dilute that and make
    // the longest template a magnet.)
    dp[at(n, m)] / n.max(1) as f64
}

// ── arc-length resampling (speed + size normalization) ──────────────────────

/// Resample a path to `n` points equidistant along its arc length ($1-recognizer style).
/// This is what makes a gesture read the same drawn fast or slow, large or small —
/// while preserving direction and shape.
pub fn resample_uniform(points: &[C], n: usize) -> Vec<C> {
    if points.len() < 2 || n < 2 {
        return points.to_vec();
    }
    let mut path_len = 0.0;
    for i in 1..points.len() {
        path_len += points[i].sub(points[i - 1]).abs();
    }
    if path_len <= 1e-9 {
        return vec![points[0]; n];
    }
    let interval = path_len / (n - 1) as f64;
    let mut out = Vec::with_capacity(n);
    out.push(points[0]);
    let mut prev = points[0];
    let mut dist = 0.0;
    let mut i = 1;
    while i < points.len() {
        let cur = points[i];
        let d = cur.sub(prev).abs();
        if dist + d >= interval && d > 0.0 {
            let t = (interval - dist) / d;
            let np = C::new(
                prev.re + t * (cur.re - prev.re),
                prev.im + t * (cur.im - prev.im),
            );
            out.push(np);
            prev = np;
            dist = 0.0;
        } else {
            dist += d;
            prev = cur;
            i += 1;
        }
    }
    while out.len() < n {
        out.push(*points.last().unwrap());
    }
    out.truncate(n);
    out
}

// ── segmentation (port of phaseBoundaryScore / chooseBlockLen) ───────────────

pub fn phase_boundary_score(z: &[C], at: usize) -> f64 {
    if at < 1 || at + 2 >= z.len() {
        return 0.0;
    }
    let v0 = z[at].sub(z[at - 1]);
    let v1 = z[at + 1].sub(z[at]);
    let v2 = z[at + 2].sub(z[at + 1]);
    let (s0, s1, s2) = (v0.abs(), v1.abs(), v2.abs());
    let cross01 = v0.re * v1.im - v0.im * v1.re;
    let cross12 = v1.re * v2.im - v1.im * v2.re;
    let turn = cross01.abs() / (s0 * s1 + 1.0);
    let jerk = (v2.sub(v1.scale(2.0)).add(v0)).abs() / (s0 + s1 + s2 + 1.0);
    let speed_shock = (s1 - s0).abs() / (s0.max(s1) + 1.0);
    let speed_valley = if s1 < s0.min(s2) * 0.65 { 0.35 } else { 0.0 };
    let curvature_flip = if cross01 != 0.0 && cross12 != 0.0 && cross01 * cross12 < 0.0 {
        0.28
    } else {
        0.0
    };
    let salient = turn > 0.55 || jerk > 0.45 || speed_shock > 0.6 || curvature_flip > 0.0;
    if !salient {
        return 0.0;
    }
    turn * 1.05 + jerk * 0.9 + speed_shock * 0.55 + speed_valley + curvature_flip
}

pub fn choose_block_len(z: &[C], start: usize) -> usize {
    let remaining = z.len() - start;
    let max_len = GLYPH_BLOCK_SIZE.min(remaining);
    if max_len <= 4 {
        return max_len;
    }
    let mut best_len = max_len;
    let mut best_score = 1.1;
    for len in 4..max_len {
        let score = phase_boundary_score(z, start + len - 1);
        if score > best_score {
            best_score = score;
            best_len = len;
            if score > 2.2 {
                break;
            }
        }
    }
    let tail = remaining - best_len;
    if best_len < max_len && tail > 0 && tail < 4 && best_score < 1.8 {
        return max_len;
    }
    best_len
}

pub fn segment(z: &[C]) -> Vec<(usize, usize)> {
    let mut segs = Vec::new();
    if z.len() < 3 {
        return segs;
    }
    let mut i = 2usize;
    while i < z.len() {
        let len = choose_block_len(z, i);
        if len < 1 {
            break;
        }
        segs.push((i, len));
        i += len;
    }
    segs
}

pub fn velocities(z: &[C]) -> Vec<C> {
    if z.len() < 2 {
        return Vec::new();
    }
    (1..z.len()).map(|i| z[i].sub(z[i - 1])).collect()
}

fn position_mean_step(z: &[C]) -> f64 {
    if z.len() < 2 {
        return 0.0;
    }
    let mut s = 0.0;
    for i in 1..z.len() {
        s += z[i].sub(z[i - 1]).abs();
    }
    s / (z.len() - 1) as f64
}

/// Per-block velocity-domain fits over the raw path (no resampling).
pub fn fit_sequence(z: &[C]) -> Vec<GlyphFit> {
    segment(z)
        .into_iter()
        .filter_map(|(start, len)| {
            let win = &z[start - 2..start + len];
            let mut f = fit(&velocities(win))?;
            f.mean_step = position_mean_step(win);
            Some(f)
        })
        .collect()
}

fn bbox_diag(z: &[C]) -> f64 {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for p in z {
        x0 = x0.min(p.re);
        y0 = y0.min(p.im);
        x1 = x1.max(p.re);
        y1 = y1.max(p.im);
    }
    (x1 - x0).hypot(y1 - y0)
}

fn arc_len(z: &[C]) -> f64 {
    let mut s = 0.0;
    for i in 1..z.len() {
        s += z[i].sub(z[i - 1]).abs();
    }
    s
}

/// Binomial [1 2 1]/4 low-pass — a discrete diffusion step. Suppresses high-frequency
/// sensor jitter while preserving the gesture's curvature (the low-frequency physics).
fn smooth(z: &[C], passes: usize) -> Vec<C> {
    if z.len() < 3 {
        return z.to_vec();
    }
    let mut cur = z.to_vec();
    for _ in 0..passes {
        let mut nxt = cur.clone();
        for i in 1..cur.len() - 1 {
            nxt[i] = cur[i - 1]
                .scale(0.25)
                .add(cur[i].scale(0.5))
                .add(cur[i + 1].scale(0.25));
        }
        cur = nxt;
    }
    cur
}

/// Normalize a raw path for recognition: resample to **uniform spacing relative to the
/// gesture's own size** (`spacing = bbox_diag / cfg.resample`) then lightly smooth.
/// Speed-invariant (uniform spacing), size-invariant (spacing scales with the gesture),
/// and short strokes in a compound gesture keep proportional representation — unlike a
/// fixed-count resample, which lets a long loop swallow a short stroke.
pub fn prepare(z: &[C], cfg: &GlyphConfig) -> Vec<C> {
    if z.len() < 3 {
        return z.to_vec();
    }
    let diag = bbox_diag(z);
    let arc = arc_len(z);
    if diag < 1e-6 || arc < 1e-6 {
        return z.to_vec();
    }
    let spacing = diag / cfg.resample.max(1) as f64;
    let n = ((arc / spacing).round() as usize).clamp(8, 4096);
    smooth(&resample_uniform(z, n), 2)
}

/// The full gesture word for recognition: normalize → **fixed overlapping windows** →
/// per-window eigen-signature. Fixed windows (vs phase-boundary segmentation) give a
/// stable-length curvature/damping profile that doesn't reshuffle under noise, which is
/// what makes DTW matching reliable. (Phase-boundary [`fit_sequence`] stays for the
/// codec / structural view.)
pub fn signature_sequence(z: &[C], cfg: &GlyphConfig) -> Vec<Sig> {
    windows_sigs(&prepare(z, cfg))
}

/// Fixed overlapping-window eigen-signatures over an already-prepared path.
fn windows_sigs(r: &[C]) -> Vec<Sig> {
    const W: usize = 8;
    if r.len() < 4 {
        return Vec::new();
    }
    let w = W.min(r.len());
    let stride = (w / 2).max(1);
    let mut sigs = Vec::new();
    let mut i = 0;
    while i + w <= r.len() {
        if let Some(mut f) = fit(&velocities(&r[i..i + w])) {
            f.mean_step = position_mean_step(&r[i..i + w]);
            sigs.push(signature(&f));
        }
        i += stride;
    }
    if sigs.is_empty() {
        if let Some(f) = fit(&velocities(r)) {
            sigs.push(signature(&f));
        }
    }
    sigs
}

// ── physical invariants (first-principles, conserved/topological) ────────────

/// Conserved/topological motion invariants of a gesture. All are translation-, scale-,
/// and speed-invariant; `winding` and `bending` are also rotation-invariant.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Invariants {
    /// signed turning ∮dθ/2π — loop count × handedness (+1 = one CW loop, −2 = two CCW)
    pub winding: f64,
    /// total absolute turning ∫|dθ|/2π — bending energy (wiggliness)
    pub bending: f64,
    /// 1 − |end−start|/arclen — closure (1 = closed loop, 0 = straight open stroke)
    pub closure: f64,
}

/// Compute the invariants from a prepared (resampled+smoothed) path.
pub fn invariants(r: &[C]) -> Invariants {
    use std::f64::consts::{PI, TAU};
    let v = velocities(r);
    let mut signed = 0.0;
    let mut total = 0.0;
    for i in 1..v.len() {
        if v[i - 1].abs() < 1e-9 || v[i].abs() < 1e-9 {
            continue;
        }
        let mut dth = v[i].arg() - v[i - 1].arg();
        while dth > PI {
            dth -= TAU;
        }
        while dth < -PI {
            dth += TAU;
        }
        signed += dth;
        total += dth.abs();
    }
    let arc = arc_len(r);
    let disp = if r.len() >= 2 {
        r[r.len() - 1].sub(r[0]).abs()
    } else {
        0.0
    };
    Invariants {
        winding: signed / TAU,
        bending: total / TAU,
        closure: (1.0 - disp / (arc + 1e-9)).clamp(0.0, 1.0),
    }
}

pub fn invariant_distance(a: Invariants, b: Invariants) -> f64 {
    // winding weighted hardest: it's the topological class (loops + handedness).
    let dw = a.winding - b.winding;
    let db = a.bending - b.bending;
    let dc = a.closure - b.closure;
    (dw * dw + 0.5 * db * db + 0.5 * dc * dc).sqrt()
}

/// A complete gesture descriptor: the local eigenmotion window sequence (fine structure)
/// plus the global physical invariants (coarse topology). The two are complementary:
/// invariants catch what local windows can't (e.g. one loop vs two), and vice versa.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GestureWord {
    pub sigs: Vec<Sig>,
    pub inv: Invariants,
}

/// Full analysis of a raw path into a [`GestureWord`].
pub fn analyze(z: &[C], cfg: &GlyphConfig) -> GestureWord {
    let r = prepare(z, cfg);
    GestureWord {
        sigs: windows_sigs(&r),
        inv: invariants(&r),
    }
}

/// Distance between two gesture words: eigenmotion DTW + weighted invariant distance.
pub fn word_distance(q: &GestureWord, t: &GestureWord, cfg: &GlyphConfig) -> f64 {
    dtw(&q.sigs, &t.sigs, cfg) + cfg.w_invariant * invariant_distance(q.inv, t.inv)
}

/// A drawable EXEMPLAR of a raw stroke: smoothed + arc-length resampled (via [`prepare`]) then
/// normalized to a centered unit box (the longer axis spans roughly -0.5..0.5, aspect preserved).
/// Stored on a template so a live overlay can ghost "the ideal shape this is becoming", scaled to
/// wherever the hand is actually drawing. Empty for a degenerate stroke.
pub fn exemplar_path(z: &[C], cfg: &GlyphConfig) -> Vec<[f32; 2]> {
    let p = prepare(z, cfg);
    if p.len() < 2 {
        return Vec::new();
    }
    let (mut minx, mut maxx, mut miny, mut maxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for c in &p {
        minx = minx.min(c.re);
        maxx = maxx.max(c.re);
        miny = miny.min(c.im);
        maxy = maxy.max(c.im);
    }
    let (cx, cy) = ((minx + maxx) * 0.5, (miny + maxy) * 0.5);
    let s = (maxx - minx).max(maxy - miny).max(1.0);
    p.iter()
        .map(|c| [((c.re - cx) / s) as f32, ((c.im - cy) / s) as f32])
        .collect()
}

// ── synthetic shapes (validation) ────────────────────────────────────────────

pub fn synth_line(n: usize) -> Vec<C> {
    (0..n)
        .map(|i| C::new(3.0 * i as f64, 1.5 * i as f64))
        .collect()
}

/// Circle of radius r. ω > 0 is one handedness, ω < 0 the other (CW vs CCW).
pub fn synth_circle(n: usize, r: f64, omega: f64) -> Vec<C> {
    (0..n)
        .map(|i| C::new(r * (i as f64 * omega).cos(), r * (i as f64 * omega).sin()))
        .collect()
}

pub fn synth_spiral(n: usize, r0: f64, rho: f64, omega: f64) -> Vec<C> {
    (0..n)
        .map(|i| {
            let rad = r0 * rho.powi(i as i32);
            C::new(
                rad * (i as f64 * omega).cos(),
                rad * (i as f64 * omega).sin(),
            )
        })
        .collect()
}

pub fn synth_line_then_circle(line_n: usize, circ_n: usize, r: f64, omega: f64) -> Vec<C> {
    let mut v = synth_line(line_n);
    let last = *v.last().unwrap();
    let circ = synth_circle(circ_n, r, omega);
    let shift = last.sub(circ[0]);
    for p in circ {
        v.push(p.add(shift));
    }
    v
}

/// A "V" / checkmark: down-right reach, sharp corner, up-right reach.
pub fn synth_vee(n: usize, size: f64) -> Vec<C> {
    let half = n / 2;
    let mut v = Vec::new();
    for i in 0..half {
        v.push(C::new(size * i as f64, size * i as f64));
    }
    let apex = *v.last().unwrap();
    for i in 1..=half {
        v.push(C::new(apex.re + size * i as f64, apex.im - size * i as f64));
    }
    v
}

/// An "S": one arc, then the opposite-handed arc (curvature flips sign mid-gesture).
pub fn synth_ess(n: usize, r: f64, omega: f64) -> Vec<C> {
    let half = n / 2;
    let mut v = synth_circle(half, r, omega);
    let last = *v.last().unwrap();
    let b = synth_circle(half, r, -omega);
    let sh = last.sub(b[0]);
    for p in b {
        v.push(p.add(sh));
    }
    v
}

/// Deterministic LCG jitter for noise-robustness tests (no rng dep, seed-varied).
pub fn add_noise(z: &[C], sigma: f64, seed: u64) -> Vec<C> {
    let mut s = seed.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(1);
    let mut nxt = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        // ~uniform [-1,1]
        ((s >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    z.iter()
        .map(|p| C::new(p.re + nxt() * sigma, p.im + nxt() * sigma))
        .collect()
}

// ── live capture (Windows Raw Input; phrase-activated, game-feel) ───────────

/// Hold-and-do: wait for `trigger_vk` to be pressed, capture sensor-true motion while
/// it's held, stop on release. The classic activation — `capture_phrase` with a plain hold.
#[cfg(windows)]
pub fn capture_held(trigger_vk: i32, max_pts: usize) -> Vec<C> {
    capture_phrase(
        trigger_vk,
        &crate::feel::Phrase::hold(),
        &crate::feel::FeelConfig::default(),
        max_pts,
        |_| {},
    )
}

/// Like [`capture_held`], but **streams the live stroke**: `on_progress(&pts)` is called each time
/// new motion arrives (the accumulated points so far), so a live overlay can draw the weave as it
/// forms. The callback runs on the capture thread between motion drains.
#[cfg(windows)]
pub fn capture_held_with(trigger_vk: i32, max_pts: usize, on_progress: impl FnMut(&[C])) -> Vec<C> {
    capture_phrase(
        trigger_vk,
        &crate::feel::Phrase::hold(),
        &crate::feel::FeelConfig::default(),
        max_pts,
        on_progress,
    )
}

/// The full-feel capture: wait for the activation RHYTHM (`phrase` — "hold", "tap tap hold",
/// "tap tap" toggle, …), then capture sensor-true motion. Hold-ending phrases capture while the
/// final press is held and keep a COYOTE TAIL of `cfg.coyote_ms` after release (letting go a hair
/// early must not eat the stroke's end); all-tap phrases TOGGLE capture (the next tap ends it).
/// A broken rhythm resets silently and instantly — fidgeting costs nothing. ESC aborts.
#[cfg(windows)]
pub fn capture_phrase(
    trigger_vk: i32,
    phrase: &crate::feel::Phrase,
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    raw_input::capture_phrase(trigger_vk, phrase, cfg, max_pts, &|| false, on_progress)
        .unwrap_or_default()
}

/// [`capture_phrase`] with an external CANCEL predicate: when `stop()` turns true (checked every
/// poll tick), the rhythm wait — or the live capture — aborts and returns the empty path, exactly
/// like ESC. A PREDICATE rather than a flag so a caller can compose conditions: a beacon ask is
/// withdrawn by its timeout/retire flag OR stands down while the GUI editor owns the trigger —
/// one press must never feed two captures.
#[cfg(windows)]
pub fn capture_phrase_until(
    trigger_vk: i32,
    phrase: &crate::feel::Phrase,
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized),
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    raw_input::capture_phrase(trigger_vk, phrase, cfg, max_pts, stop, on_progress)
        .unwrap_or_default()
}

#[cfg(not(windows))]
pub fn capture_held(_trigger_vk: i32, _max_pts: usize) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_held_with(
    _trigger_vk: i32,
    _max_pts: usize,
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_phrase(
    _trigger_vk: i32,
    _phrase: &crate::feel::Phrase,
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_phrase_until(
    _trigger_vk: i32,
    _phrase: &crate::feel::Phrase,
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _stop: &(impl Fn() -> bool + ?Sized),
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

/// One activation slot the multi-instrument watcher listens for: `id` is returned on activation,
/// `vk` is the key, `taps` is how many quick taps precede the final hold (0 = plain hold).
#[derive(Clone, Copy, Debug)]
pub struct CaptureSlot {
    pub id: u32,
    pub vk: i32,
    pub taps: u8,
}

/// Watch a SET of activation slots at once — possibly across DIFFERENT keys — and capture the
/// stroke of whichever lands first. This is the engine under "every instrument has its own
/// binding, and bindings may share a key when their rhythms differ":
///   * per-key tap counting (a press shorter than `hold_ms` = tap; `gap_ms` of silence resets);
///   * a press becomes THE HOLD the moment it outlives `hold_ms` — or the moment it MOVES
///     (≥8 counts): you don't flick mid-tap, so motion disambiguates instantly and the plain
///     plain-hold weave keeps its zero-latency feel even when rhythms share its key;
///   * motion during the disambiguation window is PREBUFFERED and becomes the stroke's head —
///     an early flick is never eaten;
///   * a press whose tap-count matches NO slot on that key belongs to the key's normal job —
///     ignored entirely (taps reset on release);
///   * ESC or `stop()` abort (None), exactly like the single-phrase captures.
///
/// On activation: cursor locks, `on_activated(id)` fires once (overlay morphs), the capture
/// streams via `on_progress`, and the function returns `Some((id, path))` on release (+coyote).
#[cfg(windows)]
pub fn capture_slots_until(
    slots: &[CaptureSlot],
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized),
    on_activated: impl FnMut(u32),
    on_progress: impl FnMut(&[C]),
) -> Option<(u32, Vec<C>)> {
    raw_input::capture_slots(slots, cfg, max_pts, stop, on_activated, on_progress)
}

#[cfg(not(windows))]
pub fn capture_slots_until(
    _slots: &[CaptureSlot],
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _stop: &(impl Fn() -> bool + ?Sized),
    _on_activated: impl FnMut(u32),
    _on_progress: impl FnMut(&[C]),
) -> Option<(u32, Vec<C>)> {
    None
}

/// Take (and clear) the scroll-wheel notches the capture's raw-input drain accumulated — the
/// DEPTH DIAL a live consumer (teleport's stack descent) reads mid-capture. Positive = wheel up.
#[cfg(windows)]
pub fn take_wheel_ticks() -> i32 {
    raw_input::take_wheel_ticks()
}

#[cfg(not(windows))]
pub fn take_wheel_ticks() -> i32 {
    0
}

/// Take (and clear) the left/right click EDGES the capture's raw-input drain accumulated —
/// the SPECTRAL VERBS a live consumer (the teleport aim) reads mid-capture while a click-guard
/// keeps those clicks from reaching the apps under the pinned cursor.
/// Returns (left-downs, right-downs, right-ups).
#[cfg(windows)]
pub fn take_click_edges() -> (i32, i32, i32) {
    raw_input::take_click_edges()
}

#[cfg(not(windows))]
pub fn take_click_edges() -> (i32, i32, i32) {
    (0, 0, 0)
}

#[cfg(windows)]
pub fn key_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

#[cfg(not(windows))]
pub fn key_down(_vk: i32) -> bool {
    false
}

#[cfg(windows)]
mod raw_input {
    use super::C;
    use std::ffi::c_void;
    use std::time::Duration;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Input::{
        GetRawInputData, RegisterRawInputDevices, HRAWINPUT, RAWINPUT, RAWINPUTDEVICE,
        RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, PeekMessageW,
        RegisterClassW, TranslateMessage, MSG, PM_REMOVE, WM_INPUT, WNDCLASSW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_POPUP,
    };

    unsafe fn setup() -> Option<HWND> {
        // A REAL (never-shown) top-level window, NOT a message-only one: Windows drops WM_INPUT
        // delivery to message-only windows in edge cases (foreground fullscreen apps, session
        // events) — the controls listener hit exactly this and was fixed the same way. The
        // symptom here was the weave engine "going dark": keys still polled (rhythm landed, the
        // cursor locked) but the raw-input drain got NOTHING, so no trail, no glyph, all modes.
        let cls: Vec<u16> = "NeuronGlyphInput\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc); // idempotent — "already exists" is fine, we only need the name live
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            cls.as_ptr(),
            std::ptr::null(),
            WS_POPUP, // created hidden (no WS_VISIBLE) and never shown
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        let rid = RAWINPUTDEVICE {
            usUsagePage: 0x01,
            usUsage: 0x02,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        if RegisterRawInputDevices(&rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
            DestroyWindow(hwnd);
            return None;
        }
        crate::prof::bump(&crate::prof::CAPTURE_ARM);
        Some(hwnd)
    }

    /// A stroke NEVER ends because a buffer filled — a gesture ends when the HAND ends it
    /// (fidget tool first). When the point buffer reaches its cap, thin it in place (keep the
    /// first point + every 2nd) and keep capturing: net displacement (radial/teleport) stays
    /// EXACT, shape fidelity degrades gracefully (the recognizer is window/scale-invariant),
    /// and a high-polling mouse can no longer blow through the cap in under a second and
    /// self-terminate the capture mid-gesture (the bug this replaces: ~600 pts at 1000+ Hz
    /// ended strokes early — radial fired half-aimed, teleport warped MID-DRAG).
    pub(super) fn compact(pts: &mut Vec<C>) {
        let mut keep = 0usize;
        for i in (0..pts.len()).step_by(2) {
            pts[keep] = pts[i];
            keep += 1;
        }
        // always keep the true latest point (the live head must not lag the hand)
        if let Some(&last) = pts.last() {
            if keep == 0 || pts[keep - 1].re != last.re || pts[keep - 1].im != last.im {
                pts[keep] = last;
                keep += 1;
            }
        }
        pts.truncate(keep);
    }

    /// Scroll-wheel notches accumulated by [`drain`] during a capture — the DEPTH DIAL: a
    /// consumer (the teleport aim) takes them with [`take_wheel_ticks`] to dial through the
    /// window stack under the ghost. Global because the wheel is global; reset on take.
    static WHEEL: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    /// Take (and clear) the wheel notches captured since the last take. Positive = wheel up.
    pub fn take_wheel_ticks() -> i32 {
        WHEEL.swap(0, std::sync::atomic::Ordering::SeqCst) / 120
    }

    /// Click EDGES accumulated by [`drain`] during a capture — the SPECTRAL VERBS: left/right
    /// button activity while a weave holds the cursor pinned. Raw input sees the buttons even
    /// when a click-guard hook swallows them from the apps below. Packed counters, reset on take.
    static L_DOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    static R_DOWN: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
    static R_UP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

    /// Take (and clear) the click edges since the last take: (left-downs, right-downs, right-ups).
    pub fn take_click_edges() -> (i32, i32, i32) {
        use std::sync::atomic::Ordering::SeqCst;
        (
            L_DOWN.swap(0, SeqCst),
            R_DOWN.swap(0, SeqCst),
            R_UP.swap(0, SeqCst),
        )
    }

    /// Drain pending WM_INPUT; push accumulated absolute positions. Returns true if any
    /// motion arrived. `acc` is the running (x,y) integral of relative deltas.
    unsafe fn drain(hwnd: HWND, acc: &mut (f64, f64), pts: &mut Vec<C>) -> bool {
        crate::prof::bump(&crate::prof::CAPTURE_POLL);
        let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let mut moved = false;
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_INPUT {
                let mut size: u32 = 0;
                GetRawInputData(
                    msg.lParam as HRAWINPUT,
                    RID_INPUT,
                    std::ptr::null_mut(),
                    &mut size,
                    header,
                );
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    let got = GetRawInputData(
                        msg.lParam as HRAWINPUT,
                        RID_INPUT,
                        buf.as_mut_ptr() as *mut c_void,
                        &mut size,
                        header,
                    );
                    if got != u32::MAX && got > 0 {
                        let ri = &*(buf.as_ptr() as *const RAWINPUT);
                        if ri.header.dwType == RIM_TYPEMOUSE {
                            let dx = ri.data.mouse.lLastX as f64;
                            let dy = ri.data.mouse.lLastY as f64;
                            if dx != 0.0 || dy != 0.0 {
                                acc.0 += dx;
                                acc.1 += dy;
                                pts.push(C::new(acc.0, acc.1));
                                moved = true;
                            }
                            // wheel notches feed the depth dial (usButtonData is a signed
                            // delta when RI_MOUSE_WHEEL is flagged).
                            let flags = ri.data.mouse.Anonymous.Anonymous.usButtonFlags;
                            if flags & 0x0400 != 0 {
                                let delta =
                                    ri.data.mouse.Anonymous.Anonymous.usButtonData as i16 as i32;
                                WHEEL.fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
                            }
                            // click edges feed the spectral verbs (RI_MOUSE_*_BUTTON_DOWN/UP)
                            if flags & 0x0001 != 0 {
                                L_DOWN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            if flags & 0x0004 != 0 {
                                R_DOWN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                            if flags & 0x0008 != 0 {
                                R_UP.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            }
                        }
                    }
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        moved
    }

    /// The multi-slot dispatcher body (see [`super::capture_slots_until`]). Per-key tap-count
    /// state machines + global motion prebuffer; activation = the press that outlives `hold_ms`
    /// OR moves ≥8 counts while a slot matches the key's current tap count.
    pub fn capture_slots(
        slots: &[super::CaptureSlot],
        cfg: &crate::feel::FeelConfig,
        max_pts: usize,
        stop: &(impl Fn() -> bool + ?Sized),
        mut on_activated: impl FnMut(u32),
        mut on_progress: impl FnMut(&[C]),
    ) -> Option<(u32, Vec<C>)> {
        use std::time::Instant;
        if slots.is_empty() {
            return None;
        }
        struct KeyState {
            vk: i32,
            down: bool,
            t_down: Instant,
            last_release: Instant,
            taps: u8,
            /// this press matched no slot at its tap count — it's the key's normal job; stand off.
            dead: bool,
        }
        unsafe {
            let hwnd = setup()?;
            let t0 = Instant::now();
            let mut keys: Vec<KeyState> = Vec::new();
            for s in slots {
                if !keys.iter().any(|k| k.vk == s.vk) {
                    keys.push(KeyState {
                        vk: s.vk,
                        down: false,
                        t_down: t0,
                        last_release: t0,
                        taps: 0,
                        dead: false,
                    });
                }
            }
            // the PREBUFFER: motion since the most recent press-down. Becomes the stroke's head
            // on a motion-activate, so an instant flick loses nothing to disambiguation.
            let mut acc = (0.0, 0.0);
            let mut pre: Vec<C> = Vec::new();

            let activated: (u32, Vec<C>) = 'wait: loop {
                drain(hwnd, &mut acc, &mut pre);
                if pre.len() > 256 {
                    // bound the prebuffer (idle mouse noise between presses means nothing)
                    pre.drain(..pre.len() - 256);
                }
                if super::key_down(0x1B) || stop() {
                    DestroyWindow(hwnd);
                    return None;
                }
                let now = Instant::now();
                for k in keys.iter_mut() {
                    let is_down = super::key_down(k.vk);
                    if is_down && !k.down {
                        // press edge: stale taps die after gap_ms of silence
                        if now.duration_since(k.last_release).as_millis() as u64 > cfg.gap_ms {
                            k.taps = 0;
                        }
                        k.down = true;
                        k.dead = false;
                        k.t_down = now;
                        acc = (0.0, 0.0);
                        pre.clear();
                    } else if !is_down && k.down {
                        // release edge: a short press is a tap; a dead press resets the count
                        k.down = false;
                        let held = now.duration_since(k.t_down).as_millis() as u64;
                        if k.dead || held >= cfg.hold_ms {
                            k.taps = 0;
                        } else {
                            k.taps = k.taps.saturating_add(1);
                            // more taps than any slot on this key wants = not ours; reset
                            let max_taps = slots
                                .iter()
                                .filter(|s| s.vk == k.vk)
                                .map(|s| s.taps)
                                .max()
                                .unwrap_or(0);
                            if k.taps > max_taps {
                                k.taps = 0;
                            }
                        }
                        k.dead = false;
                        k.last_release = now;
                    } else if !is_down && k.taps > 0 {
                        // idle: an unfinished rhythm dies quietly after gap_ms
                        if now.duration_since(k.last_release).as_millis() as u64 > cfg.gap_ms {
                            k.taps = 0;
                        }
                    }
                    if k.down && !k.dead {
                        let held = now.duration_since(k.t_down).as_millis() as u64;
                        let moved = match (pre.first(), pre.last()) {
                            (Some(f), Some(l)) => {
                                let (dx, dy) = (l.re - f.re, l.im - f.im);
                                (dx * dx + dy * dy).sqrt() >= 8.0
                            }
                            _ => false,
                        };
                        if held >= cfg.hold_ms || moved {
                            match slots.iter().find(|s| s.vk == k.vk && s.taps == k.taps) {
                                Some(s) => break 'wait (s.id, std::mem::take(&mut pre)),
                                None => k.dead = true, // the key's normal job — not ours
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(3));
            };

            let (id, mut pts) = activated;
            let vk = slots.iter().find(|s| s.id == id).map(|s| s.vk).unwrap_or(0);
            // cursor pinned for the stroke, exactly like every other weave.
            let _cursor = super::cursor_lock::CursorLock::engage();
            on_activated(id);
            on_progress(&pts);

            // ── hold capture + coyote tail. The stroke ends when the HAND ends it — a full
            // buffer thins (compact) and keeps going, it never self-terminates the gesture.
            // SAFETY DEADMAN: the loop ends on key-release or `stop()`, but if the trigger's
            // key-state ever reads "held" forever (a swallowed/missed up-edge), this would spin
            // with the CURSOR LOCKED — a frozen mouse + a dead cast key until restart. No real
            // hold lasts 30s, so cap it: a stuck capture self-releases (cursor unlocks, thread
            // returns) instead of bricking every mode. ──
            let hold_start = Instant::now();
            while super::key_down(vk) {
                if stop() || hold_start.elapsed() > Duration::from_secs(30) {
                    DestroyWindow(hwnd);
                    return None;
                }
                if drain(hwnd, &mut acc, &mut pts) {
                    if pts.len() >= max_pts {
                        compact(&mut pts);
                    }
                    on_progress(&pts);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let tail_end = Instant::now() + Duration::from_millis(cfg.coyote_ms);
            while Instant::now() < tail_end {
                if super::key_down(vk) {
                    break;
                }
                if drain(hwnd, &mut acc, &mut pts) {
                    if pts.len() >= max_pts {
                        compact(&mut pts);
                    }
                    on_progress(&pts);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            DestroyWindow(hwnd);
            Some((id, pts))
        }
    }

    pub fn capture_phrase(
        trigger_vk: i32,
        phrase: &crate::feel::Phrase,
        cfg: &crate::feel::FeelConfig,
        max_pts: usize,
        stop: &(impl Fn() -> bool + ?Sized),
        mut on_progress: impl FnMut(&[C]),
    ) -> Result<Vec<C>, ()> {
        use crate::feel::{PhraseWatcher, Watch};
        use std::time::Instant;
        unsafe {
            let hwnd = setup().ok_or(())?;
            let mut acc = (0.0, 0.0);
            let mut sink = Vec::new();

            // ── wait for the activation rhythm (drain & discard motion meanwhile) ──
            let t0 = Instant::now();
            let mut watcher = PhraseWatcher::new(phrase.clone(), cfg);
            let toggle = loop {
                drain(hwnd, &mut acc, &mut sink);
                sink.clear();
                if super::key_down(0x1B) || stop() {
                    // ESC (or an external cancel) aborts — quietly, instantly.
                    DestroyWindow(hwnd);
                    return Ok(Vec::new());
                }
                let now = t0.elapsed().as_millis() as u64;
                match watcher.feed(now, super::key_down(trigger_vk)) {
                    Watch::Activated { toggle } => break toggle,
                    // a broken rhythm costs nothing — the watcher already re-armed itself.
                    Watch::Pending | Watch::Reset => {}
                }
                std::thread::sleep(Duration::from_millis(3));
            };

            // Lock the OS cursor for the duration of the weave: physical motion still drives the
            // gesture (Raw Input above reads it), but the cursor doesn't move, drag a window, or
            // click. Released on drop when capture ends. (Cross-platform seam in `cursor_lock`.)
            let _cursor = super::cursor_lock::CursorLock::engage();

            // ACTIVATION TICK: one empty-progress call the moment the rhythm lands (before any
            // motion), so an overlay can materialize on the PRESS — not a beat later on the first
            // mouse twitch. Existing callers see a zero-length stroke, which they already ignore.
            on_progress(&[]);

            let mut pts: Vec<C> = Vec::new();
            acc = (0.0, 0.0);

            if !toggle {
                // ── hold capture: while the final press is held. A full buffer THINS
                // (compact) and keeps capturing — no gesture ever self-terminates. ──
                // SAFETY DEADMAN (see capture_slots): a 30s cap so a stuck key-state can't spin
                // here forever with the cursor LOCKED (frozen mouse + dead modes until restart).
                let hold_start = Instant::now();
                while super::key_down(trigger_vk) {
                    if stop() || hold_start.elapsed() > Duration::from_secs(30) {
                        // retired mid-weave: the stroke must NOT commit (its owner withdrew it).
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    if drain(hwnd, &mut acc, &mut pts) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts); // stream the live stroke to any overlay
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // ── COYOTE TAIL: motion just after release still belongs to the stroke ──
                // (cut short instantly by a re-press — the next weave must never wait on this)
                let tail_end = Instant::now() + Duration::from_millis(cfg.coyote_ms);
                while Instant::now() < tail_end {
                    if super::key_down(trigger_vk) {
                        break; // spam: the user is already starting the next weave
                    }
                    if drain(hwnd, &mut acc, &mut pts) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            } else {
                // ── toggle capture: runs until the NEXT tap of the trigger (or ESC) ──
                // First let the activating press release (its motion already counts).
                while super::key_down(trigger_vk) {
                    if drain(hwnd, &mut acc, &mut pts) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // capture until the closing tap's DOWN edge (responsive close) or ESC.
                loop {
                    if stop() {
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    if super::key_down(0x1B) || super::key_down(trigger_vk) {
                        break;
                    }
                    if drain(hwnd, &mut acc, &mut pts) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // swallow the closing press so it can't double as the next phrase's first tap
                // (activation-to-deactivate must be free, not a hidden re-activation).
                while super::key_down(trigger_vk) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            DestroyWindow(hwnd);
            Ok(pts)
        }
    }
}

/// Pointer suppression for the duration of a weave — so drawing a glyph drives the gesture but the
/// real cursor doesn't move (no window drag, no stray clicks). The physical motion is still read
/// (Raw Input on Windows; the relative-motion path on other OSes), so only the *cursor* is pinned.
///
/// Cross-platform plan — the capture path is the seam, each platform's `capture_held` engages its
/// own lock: **Windows** = `ClipCursor` to a 1px box + hide (implemented here). **macOS** =
/// `CGAssociateMouseAndMouseCursorPosition(false)` + `CGDisplayHideCursor`. **X11** = `XGrabPointer`
/// confine-to a 1px window. **Wayland** = `zwp_pointer_constraints_v1` lock + `zwp_relative_pointer_v1`.
#[cfg(windows)]
mod cursor_lock {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::WindowsAndMessaging::{ClipCursor, GetCursorPos, ShowCursor};

    /// RAII pointer lock. Engage pins+hides the cursor; drop restores free movement + visibility.
    pub struct CursorLock {
        engaged: bool,
    }

    impl CursorLock {
        /// Pin the cursor to a 1px box at its current position (and hide it). If the position can't
        /// be read it's a clean no-op — the gesture still captures; the cursor just isn't suppressed.
        pub fn engage() -> Self {
            unsafe {
                let mut p = POINT { x: 0, y: 0 };
                if GetCursorPos(&mut p) != 0 {
                    let r = RECT {
                        left: p.x,
                        top: p.y,
                        right: p.x + 1,
                        bottom: p.y + 1,
                    };
                    ClipCursor(&r);
                    ShowCursor(0);
                    return CursorLock { engaged: true };
                }
            }
            CursorLock { engaged: false }
        }
    }

    impl Drop for CursorLock {
        fn drop(&mut self) {
            if self.engaged {
                unsafe {
                    ClipCursor(std::ptr::null());
                    ShowCursor(1);
                }
            }
        }
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> GlyphConfig {
        GlyphConfig::default()
    }

    /// The capture's buffer-thinning must preserve the gesture's TRUTH: the first point and the
    /// LIVE last point survive exactly (net displacement — what radial/teleport commit on — is
    /// untouched), the count roughly halves, and order holds. This is the guard against the bug
    /// where a 1000 Hz mouse filled the buffer in under a second and the capture self-terminated
    /// mid-gesture (radial fired half-aimed; teleport warped mid-drag).
    #[cfg(windows)]
    #[test]
    fn compact_preserves_endpoints_and_order() {
        let mut pts: Vec<C> = (0..601).map(|i| C::new(i as f64, (i * 2) as f64)).collect();
        let (first, last) = (pts[0], pts[600]);
        raw_input::compact(&mut pts);
        assert!(pts.len() <= 302, "must roughly halve: {}", pts.len());
        assert_eq!(
            (pts[0].re, pts[0].im),
            (first.re, first.im),
            "first point survives"
        );
        let l = pts.last().unwrap();
        assert_eq!(
            (l.re, l.im),
            (last.re, last.im),
            "the LIVE head survives exactly"
        );
        assert!(pts.windows(2).all(|w| w[1].re > w[0].re), "order holds");
        // repeated compaction (a very long fidget) stays sane and keeps the endpoints
        for _ in 0..8 {
            raw_input::compact(&mut pts);
        }
        assert!(pts.len() >= 2);
        assert_eq!(pts.last().map(|c| c.re), Some(last.re));
    }
    fn sig1(z: &[C]) -> Sig {
        signature(&fit(z).unwrap())
    }

    // ── faithful single-block math (raw position fit on origin shapes) ──

    #[test]
    fn line_unit_eigen_no_rotation() {
        let s = sig1(&synth_line(64));
        assert!((s.mag - 1.0).abs() < 0.03);
        assert!(s.rot.abs() < 0.03);
    }

    #[test]
    fn circle_eigen_on_unit_at_omega() {
        for w in [0.2_f64, 0.5, 1.0] {
            let s = sig1(&synth_circle(256, 500.0, w));
            assert!((s.mag - 1.0).abs() < 0.02);
            assert!((s.rot.abs() - w).abs() < 0.02);
        }
    }

    #[test]
    fn spiral_eigen_is_decay() {
        for (rho, w) in [(0.99_f64, 0.3_f64), (0.97, 0.6)] {
            let s = sig1(&synth_spiral(256, 600.0, rho, w));
            assert!((s.mag - rho).abs() < 0.02);
        }
    }

    #[test]
    fn coefficients_never_leave_stability_manifold() {
        for w in [0.1_f64, 0.7, 1.3, -0.5] {
            let f = fit(&synth_circle(128, 300.0, w)).unwrap();
            assert!(f.g.abs() <= 1.0 + 1e-9);
            assert!(f.k.abs() <= 2.0 + 1e-9);
        }
    }

    // ── recognition pipeline (resample → segment → DTW) ──

    #[test]
    fn dtw_zero_to_self_and_orders_shapes() {
        use std::f64::consts::TAU;
        let circ = signature_sequence(&synth_circle(120, 400.0, TAU / 120.0), &cfg());
        let line = signature_sequence(&synth_line(120), &cfg());
        assert!(dtw(&circ, &circ, &cfg()) < 1e-9);
        // a circle is nearer another circle than a line (query-normalized DTW)
        let circ2 = signature_sequence(&synth_circle(90, 250.0, TAU / 90.0), &cfg());
        assert!(dtw(&circ, &circ2, &cfg()) < dtw(&circ, &line, &cfg()));
    }

    #[test]
    fn handedness_cw_differs_from_ccw() {
        let cw = signature_sequence(&synth_circle(160, 400.0, 0.18), &cfg());
        let ccw = signature_sequence(&synth_circle(160, 400.0, -0.18), &cfg());
        let cw2 = signature_sequence(&synth_circle(160, 360.0, 0.20), &cfg());
        // same handedness (different size/speed) closer than opposite handedness
        assert!(
            dtw(&cw, &cw2, &cfg()) < dtw(&cw, &ccw, &cfg()),
            "CW↔CW {} should beat CW↔CCW {}",
            dtw(&cw, &cw2, &cfg()),
            dtw(&cw, &ccw, &cfg())
        );
    }

    #[test]
    fn speed_invariant() {
        use std::f64::consts::TAU;
        // same single loop, drawn slow (dense) vs fast (sparse) → same word after prepare
        let slow = signature_sequence(&synth_circle(240, 400.0, TAU / 240.0), &cfg());
        let fast = signature_sequence(&synth_circle(60, 400.0, TAU / 60.0), &cfg());
        assert!(
            dtw(&slow, &fast, &cfg()) < 0.06,
            "speed: {}",
            dtw(&slow, &fast, &cfg())
        );
    }

    #[test]
    fn size_invariant() {
        use std::f64::consts::TAU;
        let small = signature_sequence(&synth_circle(160, 80.0, TAU / 160.0), &cfg());
        let big = signature_sequence(&synth_circle(160, 900.0, TAU / 160.0), &cfg());
        assert!(
            dtw(&small, &big, &cfg()) < 0.06,
            "size: {}",
            dtw(&small, &big, &cfg())
        );
    }

    #[test]
    fn handedness_single_loop() {
        use std::f64::consts::TAU;
        let cw = signature_sequence(&synth_circle(140, 400.0, TAU / 140.0), &cfg());
        let ccw = signature_sequence(&synth_circle(140, 400.0, -TAU / 140.0), &cfg());
        let cw2 = signature_sequence(&synth_circle(90, 250.0, TAU / 90.0), &cfg());
        assert!(dtw(&cw, &cw2, &cfg()) < dtw(&cw, &ccw, &cfg()));
    }

    #[test]
    fn robust_to_noise() {
        let base = synth_circle(160, 400.0, std::f64::consts::TAU / 160.0);
        let step = arc_len(&base) / (base.len() - 1) as f64;
        let clean = signature_sequence(&base, &cfg());
        let noisy = signature_sequence(&add_noise(&base, 0.06 * step, 7), &cfg());
        let line = signature_sequence(&synth_line(160), &cfg());
        assert!(
            dtw(&clean, &noisy, &cfg()) < dtw(&clean, &line, &cfg()),
            "noisy circle {} should still beat a line {}",
            dtw(&clean, &noisy, &cfg()),
            dtw(&clean, &line, &cfg())
        );
    }

    #[test]
    fn compound_segments_into_distinct_modes() {
        let z = synth_line_then_circle(24, 48, 300.0, 0.45);
        let fits = fit_sequence(&z);
        assert!(fits.len() >= 2);
        let rots: Vec<f64> = fits.iter().map(|f| signature(f).rot).collect();
        let straight = rots.iter().any(|r| r.abs() < 0.05);
        let turning = rots.iter().any(|r| r.abs() > 0.3);
        assert!(
            straight && turning,
            "expected both straight and turning modes: {rots:?}"
        );
    }

    #[test]
    fn invariants_distinguish_loop_count() {
        use std::f64::consts::TAU;
        let cfg = cfg();
        let one = analyze(&synth_circle(120, 300.0, TAU / 120.0), &cfg); // 1 loop
        let two = analyze(&synth_circle(240, 300.0, TAU / 120.0), &cfg); // 2 loops
                                                                         // winding is the topological invariant: ~1 vs ~2
        assert!(
            (one.inv.winding.abs() - 1.0).abs() < 0.15,
            "one winding {}",
            one.inv.winding
        );
        assert!(
            (two.inv.winding.abs() - 2.0).abs() < 0.25,
            "two winding {}",
            two.inv.winding
        );
        // local eigenmotion DTW conflates them (every window looks the same)...
        let local = dtw(&one.sigs, &two.sigs, &cfg);
        // ...but the full physics-aware distance separates them.
        let full = word_distance(&one, &two, &cfg);
        assert!(
            full > local + 0.2,
            "invariants should separate loop counts: local DTW {local:.3}, full {full:.3}"
        );
    }

    #[test]
    fn invariants_are_speed_and_size_invariant() {
        use std::f64::consts::TAU;
        let a = analyze(&synth_circle(240, 100.0, TAU / 240.0), &cfg()); // small, dense
        let b = analyze(&synth_circle(60, 800.0, TAU / 60.0), &cfg()); // big, sparse
        assert!(invariant_distance(a.inv, b.inv) < 0.15);
    }

    #[test]
    fn attunement_changes_outcome() {
        // Two circles same shape but different irregularity. With residual weight high,
        // the irregular one should read as further from clean than with it low.
        let clean = signature_sequence(&synth_circle(160, 400.0, 0.2), &cfg());
        let rough = signature_sequence(&add_noise(&synth_circle(160, 400.0, 0.2), 10.0, 3), &cfg());
        let mut low = cfg();
        low.w_resid = 0.0;
        let mut high = cfg();
        high.w_resid = 3.0;
        assert!(dtw(&clean, &rough, &high) > dtw(&clean, &rough, &low));
    }

    /// Real classifier accuracy: a labeled set of structurally-distinct gestures, each
    /// matched (1-NN by DTW) against noisy + scaled + speed-varied variants never seen.
    #[test]
    fn classifier_accuracy_on_variant_set() {
        use std::f64::consts::TAU;
        let cfg = cfg();
        let templates: Vec<(&str, Vec<Sig>)> = vec![
            (
                "circle_cw",
                signature_sequence(&synth_circle(64, 300.0, TAU / 64.0), &cfg),
            ),
            (
                "circle_ccw",
                signature_sequence(&synth_circle(64, 300.0, -TAU / 64.0), &cfg),
            ),
            ("line", signature_sequence(&synth_line(60), &cfg)),
            ("vee", signature_sequence(&synth_vee(60, 8.0), &cfg)),
            (
                "ess",
                signature_sequence(&synth_ess(80, 250.0, TAU / 40.0), &cfg),
            ),
        ];

        // variants differ in size, speed (sample count), and noise — never identical.
        let make_variant = |label: &str, seed: u64| -> Vec<C> {
            let base = match label {
                "circle_cw" => synth_circle(44, 180.0, TAU / 44.0),
                "circle_ccw" => synth_circle(96, 520.0, -TAU / 96.0),
                "line" => synth_line(48),
                "vee" => synth_vee(48, 13.0),
                _ => synth_ess(64, 360.0, TAU / 32.0),
            };
            // realistic sensor jitter: a few % of the inter-sample step, not the extent
            let step = arc_len(&base) / (base.len().max(2) - 1) as f64;
            add_noise(&base, 0.06 * step, seed)
        };

        let mut correct = 0;
        let mut total = 0;
        for (label, _) in &templates {
            for seed in 0..6u64 {
                let q = signature_sequence(&make_variant(label, seed), &cfg);
                let mut scored: Vec<(&str, f64)> = templates
                    .iter()
                    .map(|t| (t.0, dtw(&q, &t.1, &cfg)))
                    .collect();
                scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
                if scored[0].0 == *label {
                    correct += 1;
                } else {
                    eprintln!(
                        "MISS {label}/{seed}: got {} ({:.4}) vs true {label} ({:.4})",
                        scored[0].0,
                        scored[0].1,
                        scored.iter().find(|s| s.0 == *label).unwrap().1
                    );
                }
                total += 1;
            }
        }
        let acc = correct as f64 / total as f64;
        assert!(
            acc >= 0.9,
            "classifier accuracy {acc:.2} ({correct}/{total}) below 0.9"
        );
    }
}
