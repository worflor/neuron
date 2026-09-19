// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: LicenseRef-WLCSL-1.0
// See ../../../LICENSE.md and ../../../LICENSES/WLCSL-1.0.md.

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
    #[must_use]
    pub fn new(re: f64, im: f64) -> C {
        C { re, im }
    }
    #[must_use]
    pub fn abs(self) -> f64 {
        self.re.hypot(self.im)
    }
    #[must_use]
    pub fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }
    #[must_use]
    pub fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    #[must_use]
    pub fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    #[must_use]
    pub fn mul(self, o: C) -> C {
        C::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    #[must_use]
    pub fn scale(self, s: f64) -> C {
        C::new(self.re * s, self.im * s)
    }
    /// Principal complex square root.
    #[must_use]
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
    let gm2 = f64::from(gr) * f64::from(gr) + f64::from(gi) * f64::from(gi);
    if gm2 > G_MAG2_MAX {
        let s = Q14 / gm2.sqrt();
        gr = (f64::from(gr) * s).round_ties_even() as i32;
        gi = (f64::from(gi) * s).round_ties_even() as i32;
    }
    let km2 = f64::from(kr) * f64::from(kr) + f64::from(ki) * f64::from(ki);
    if km2 > K_MAG2_MAX {
        let s = (2.0 * Q14) / km2.sqrt();
        kr = (f64::from(kr) * s).round_ties_even() as i32;
        ki = (f64::from(ki) * s).round_ties_even() as i32;
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
#[must_use]
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
    let k = C::new(f64::from(kr) / Q14, f64::from(ki) / Q14);
    let g = C::new(f64::from(gr) / Q14, f64::from(gi) / Q14);
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

#[must_use]
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
#[must_use]
pub fn sig_distance(a: Sig, b: Sig, cfg: &GlyphConfig) -> f64 {
    let dm = a.mag - b.mag;
    let dr = ang_dist(a.rot, b.rot);
    let dn = (a.resid_norm - b.resid_norm).abs().min(2.0);
    (cfg.w_damping * dm * dm + cfg.w_curve * dr * dr + cfg.w_resid * dn * dn).sqrt()
}

/// Length-normalized DTW between two signature sequences (gesture words).
#[must_use]
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
#[must_use]
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

#[must_use]
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

#[must_use]
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

#[must_use]
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

#[must_use]
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
#[must_use]
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

// ── curvature-arc resampling (the eigenmotion's native coordinate) ───────────────────────────
// Uniform arc-length is the WRONG coordinate for this engine. The recurrence z[n]=K·z[n-1]−G·z[n-2]
// is *exact* for constant-angular-rate motion (a circular arc at equal Δθ), so it wants resolution
// where the path TURNS and needs almost none where it's straight (there K=2, G=1 is already exact).
// So we resample in `ds + k·|dθ|·mean_step` — arc length blended with turning — which packs samples
// into cusps and loops and leaves straight runs sparse. Because `dθ` is scale-free and `ds` scales
// with size, this stays size/speed/translation-invariant, exactly like plain arc length.

/// How hard a bend "pulls" extra samples relative to plain arc length (the `k` above). Higher =
/// more resolution concentrated in cusps/loops.
const CURV_K: f64 = 6.0;
/// Sample-budget growth per full turn (2π) of accumulated turning, on top of the arc-length base.
const CURV_GROWTH: f64 = 0.5;
/// Ceiling on that growth (× the arc-length base) so a pathological scribble can't explode the count.
const CURV_GROWTH_CAP: usize = 3;

/// Per-step curvature-arc weights over `p`: each step's arc length plus `CURV_K·|turn|·mean_step`
/// (turn = exterior angle at the step's start vertex). Returns `(weights, total_turning)` with
/// `weights.len() == p.len()-1`. Uniformly resampling in the cumulative weight ([`resample_weighted`])
/// advances faster through bends, so more samples land where the path curves.
fn curvature_arc_weights(p: &[C]) -> (Vec<f64>, f64) {
    let m = p.len();
    if m < 2 {
        return (Vec::new(), 0.0);
    }
    let mut w: Vec<f64> = (1..m).map(|i| p[i].sub(p[i - 1]).abs()).collect();
    let mean_step = w.iter().sum::<f64>() / w.len() as f64 + 1e-9;
    let mut total_turn = 0.0;
    for i in 1..m - 1 {
        let (v0, v1) = (p[i].sub(p[i - 1]), p[i + 1].sub(p[i]));
        if v0.abs() < 1e-9 || v1.abs() < 1e-9 {
            continue;
        }
        let mut dth = v1.arg() - v0.arg();
        while dth > std::f64::consts::PI {
            dth -= std::f64::consts::TAU;
        }
        while dth < -std::f64::consts::PI {
            dth += std::f64::consts::TAU;
        }
        total_turn += dth.abs();
        w[i] += CURV_K * dth.abs() * mean_step; // charge the turn to the step leaving vertex i
    }
    (w, total_turn)
}

/// Resample `points` to `n` points equidistant in a cumulative WEIGHT parameter (one weight per step,
/// `weights.len() == points.len()-1`). [`resample_uniform`] is the special case where each weight is
/// the step's arc length; a curvature-arc weighting biases the spacing toward bends. Endpoints exact.
fn resample_weighted(points: &[C], weights: &[f64], n: usize) -> Vec<C> {
    let m = points.len();
    if m < 2 || n < 2 || weights.len() != m - 1 {
        return points.to_vec();
    }
    let mut cum = vec![0.0f64; m];
    for i in 1..m {
        cum[i] = cum[i - 1] + weights[i - 1].max(0.0);
    }
    let total = cum[m - 1];
    if total <= 1e-9 {
        return vec![points[0]; n];
    }
    let step = total / (n - 1) as f64;
    let mut out = Vec::with_capacity(n);
    out.push(points[0]);
    let mut j = 1usize;
    for k in 1..n - 1 {
        let target = step * k as f64;
        while j < m - 1 && cum[j] < target {
            j += 1;
        }
        let seg = cum[j] - cum[j - 1];
        let t = if seg > 1e-12 {
            (target - cum[j - 1]) / seg
        } else {
            0.0
        };
        out.push(C::new(
            points[j - 1].re + t * (points[j].re - points[j - 1].re),
            points[j - 1].im + t * (points[j].im - points[j - 1].im),
        ));
    }
    out.push(points[m - 1]);
    out
}

/// Normalize a raw path for recognition: **curvature-arc resample** (samples concentrate where the
/// path turns — the oscillator's native coordinate) then lightly smooth. Size-, speed-, and
/// translation-invariant like plain arc-length resampling, but it stops blunting the cusps/loops of
/// complex strokes — the shredded-into-tiny-blocks jaggedness — because the detail actually gets
/// sampled. `cfg.resample` still sets the base density; the turning term adds resolution on top.
#[must_use]
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
    let base = ((arc / spacing).round() as usize).clamp(8, 4096);
    // Denoise at the RAW sample rate before measuring curvature. `curvature_arc_weights` sums
    // UNSIGNED |dθ| per step, so sensor/hand jitter (which a signed integral like `winding` mostly
    // cancels) instead piles up — once positional noise is comparable to the inter-sample spacing,
    // the per-vertex kink it creates can dwarf a real circle's genuine turning by an order of
    // magnitude (measured: an 11x inflation on a noisy circle whose true turning is one revolution).
    // A light low-pass HERE, at the path's native density, is what actually fixes it: applying the
    // same smooth to the (10x-oversampled) canonicalized path below only reaches a fraction of one
    // raw inter-sample gap per pass, so it barely touches noise that lives at raw resolution.
    let raw = smooth(z, 3);
    // Stage 1 — CANONICALIZE DENSITY: a dense uniform-arc pre-resample erases the draw-SPEED bias
    // (raw sample density depends on how fast you moved), so the turning we measure next is a
    // property of the SHAPE, not the sampling. This is what keeps the whole thing speed-invariant.
    let h = (base * 10).clamp(400, 8000);
    let canon = resample_uniform(&raw, h);
    // Stage 2 — weight by arc-length + turning, the eigenmotion's native coordinate.
    let (weights, total_turn) = curvature_arc_weights(&canon);
    // Stage 3 — grow the budget mildly with total turning (a line keeps `base`; a busy signature
    // earns up to CURV_GROWTH_CAP× more), then resample uniformly in the curvature-arc weight.
    let grown = (base as f64 * (1.0 + CURV_GROWTH * total_turn / std::f64::consts::TAU)) as usize;
    let n = grown.clamp(8, (base * CURV_GROWTH_CAP).max(64));
    smooth(&resample_weighted(&canon, &weights, n), 2)
}

/// The full gesture word for recognition: normalize → **fixed overlapping windows** →
/// per-window eigen-signature. Fixed windows (vs phase-boundary segmentation) give a
/// stable-length curvature/damping profile that doesn't reshuffle under noise, which is
/// what makes DTW matching reliable. (Phase-boundary [`fit_sequence`] stays for the
/// codec / structural view.)
#[must_use]
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
/// and speed-invariant; `winding` and `bending` are also rotation-invariant — with one
/// intrinsic caveat: a turn of exactly ±π (a perfect reversal) sits on the angle-wrap's
/// branch cut, where handedness is mathematically ambiguous, so `winding` can shift by a
/// whole turn under rotation for strokes containing such reversals (real captured strokes
/// essentially never do; axis-aligned synthetic ones can).
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
#[must_use]
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

#[must_use]
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
#[must_use]
pub fn analyze(z: &[C], cfg: &GlyphConfig) -> GestureWord {
    let r = prepare(z, cfg);
    GestureWord {
        sigs: windows_sigs(&r),
        inv: invariants(&r),
    }
}

/// Distance between two gesture words: eigenmotion DTW + weighted invariant distance.
#[must_use]
pub fn word_distance(q: &GestureWord, t: &GestureWord, cfg: &GlyphConfig) -> f64 {
    dtw(&q.sigs, &t.sigs, cfg) + cfg.w_invariant * invariant_distance(q.inv, t.inv)
}

/// A drawable EXEMPLAR of a raw stroke: smoothed + arc-length resampled (via [`prepare`]) then
/// normalized to a centered unit box (the longer axis spans roughly -0.5..0.5, aspect preserved).
/// Stored on a template so a live overlay can ghost "the ideal shape this is becoming", scaled to
/// wherever the hand is actually drawing. Empty for a degenerate stroke.
#[must_use]
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

#[must_use]
pub fn synth_line(n: usize) -> Vec<C> {
    (0..n)
        .map(|i| C::new(3.0 * i as f64, 1.5 * i as f64))
        .collect()
}

/// Circle of radius r. ω > 0 is one handedness, ω < 0 the other (CW vs CCW).
#[must_use]
pub fn synth_circle(n: usize, r: f64, omega: f64) -> Vec<C> {
    (0..n)
        .map(|i| C::new(r * (i as f64 * omega).cos(), r * (i as f64 * omega).sin()))
        .collect()
}

#[must_use]
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

#[must_use]
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
#[must_use]
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
#[must_use]
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
#[must_use]
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

/// Hold-and-do: wait for the `trigger` control to be pressed, capture sensor-true motion while
/// it's held, stop on release. The classic activation — `capture_phrase` with a plain hold.
#[cfg(windows)]
#[must_use]
pub fn capture_held(trigger: crate::controls::ControlRef, max_pts: usize) -> Vec<C> {
    capture_phrase(
        trigger,
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
pub fn capture_held_with(
    trigger: crate::controls::ControlRef,
    max_pts: usize,
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    capture_phrase(
        trigger,
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
    trigger: crate::controls::ControlRef,
    phrase: &crate::feel::Phrase,
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    raw_input::capture_phrase(
        trigger,
        phrase,
        cfg,
        max_pts,
        &|| false,
        &mut Vec::new(),
        false,
        on_progress,
    )
    .unwrap_or_default()
}

/// [`capture_phrase`] with an external CANCEL predicate: when `stop()` turns true (checked every
/// poll tick), the rhythm wait — or the live capture — aborts and returns the empty path, exactly
/// like ESC. A PREDICATE rather than a flag so a caller can compose conditions: a beacon ask is
/// withdrawn by its timeout/retire flag OR stands down while the GUI editor owns the trigger —
/// one press must never feed two captures.
#[cfg(windows)]
pub fn capture_phrase_until(
    trigger: crate::controls::ControlRef,
    phrase: &crate::feel::Phrase,
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized),
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    raw_input::capture_phrase(
        trigger,
        phrase,
        cfg,
        max_pts,
        stop,
        &mut Vec::new(),
        false,
        on_progress,
    )
    .unwrap_or_default()
}

/// Like [`capture_phrase_until`], but also returns **per-sample timestamps** (ms since boot, from
/// each motion event's `WM_INPUT` `msg.time`), index-aligned with the returned points. This is the
/// research-capture surface ([`crate::gwyph`] / strokelab): real inter-sample Δt for eigenmotion
/// analysis, which the recognizer never needed and so the normal path doesn't collect. Run it with
/// a `max_pts` high enough that the buffer never thins (`compact` would desync points from stamps).
#[cfg(windows)]
pub fn capture_phrase_until_stamped(
    trigger: crate::controls::ControlRef,
    phrase: &crate::feel::Phrase,
    cfg: &crate::feel::FeelConfig,
    max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized),
    on_progress: impl FnMut(&[C]),
) -> (Vec<C>, Vec<u32>) {
    let mut stamps = Vec::new();
    let pts = raw_input::capture_phrase(
        trigger,
        phrase,
        cfg,
        max_pts,
        stop,
        &mut stamps,
        true,
        on_progress,
    )
    .unwrap_or_default();
    // stamps must be EXACTLY 1:1 with points. On any mismatch — a cancelled capture that left
    // stamps populated while returning no points, or `compact` thinning points on a pathologically
    // long stroke — drop them entirely, so the consumer records `null` rather than misaligned times
    // (truncating to length would masquerade as aligned and emit a wrong per-sample timeline).
    if stamps.len() != pts.len() {
        stamps.clear();
    }
    (pts, stamps)
}

#[cfg(not(windows))]
pub fn capture_held(_trigger: crate::controls::ControlRef, _max_pts: usize) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_held_with(
    _trigger: crate::controls::ControlRef,
    _max_pts: usize,
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_phrase(
    _trigger: crate::controls::ControlRef,
    _phrase: &crate::feel::Phrase,
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_phrase_until(
    _trigger: crate::controls::ControlRef,
    _phrase: &crate::feel::Phrase,
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _stop: &(impl Fn() -> bool + ?Sized),
    _on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn capture_phrase_until_stamped(
    _trigger: crate::controls::ControlRef,
    _phrase: &crate::feel::Phrase,
    _cfg: &crate::feel::FeelConfig,
    _max_pts: usize,
    _stop: &(impl Fn() -> bool + ?Sized),
    _on_progress: impl FnMut(&[C]),
) -> (Vec<C>, Vec<u32>) {
    (Vec::new(), Vec::new())
}

/// One activation slot the multi-instrument watcher listens for: `id` is returned on activation,
/// `ctl` is the control (page/usage/pid — device-aware), `taps` is how many quick taps precede
/// the final hold (0 = plain hold).
#[derive(Clone, Copy, Debug)]
pub struct CaptureSlot {
    pub id: u32,
    pub ctl: crate::controls::ControlRef,
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
#[must_use]
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
#[must_use]
pub fn take_click_edges() -> (i32, i32, i32) {
    raw_input::take_click_edges()
}

#[cfg(not(windows))]
pub fn take_click_edges() -> (i32, i32, i32) {
    (0, 0, 0)
}

#[cfg(windows)]
#[must_use]
pub fn key_down(vk: i32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(vk) as u16 & 0x8000) != 0 }
}

#[cfg(not(windows))]
pub fn key_down(_vk: i32) -> bool {
    false
}

/// Device-aware held-state for one control — the read every capture loop uses for its trigger.
/// Primary source: the shared Raw-Input held registry (`controls::control_held`), which knows the
/// source device (so a pid-bound trigger ignores the same key on other devices) and still sees
/// keystrokes a low-level hook swallows. When NO pump feeds the registry (CLI one-shots), it
/// degrades to the legacy `GetAsyncKeyState` poll via the control's VK equivalent — device-blind,
/// exactly the historical behaviour.
pub fn control_down(ctl: crate::controls::ControlRef) -> bool {
    match crate::controls::control_held(ctl.page, ctl.usage, ctl.pid) {
        Some(down) => down,
        None => ctl.vk_hint().is_some_and(key_down),
    }
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
        RegisterClassW(&raw const wc); // idempotent — "already exists" is fine, we only need the name live
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
        if RegisterRawInputDevices(&raw const rid, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) == 0 {
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

    /// Drain pending `WM_INPUT`; push accumulated absolute positions. Returns true if any
    /// motion arrived. `acc` is the running (x,y) integral of relative deltas.
    ///
    /// When `want_stamps`, each pushed point also appends its source `WM_INPUT`'s `msg.time` (ms
    /// since boot, the OS's per-event timestamp) to `stamps`, kept index-aligned with `pts` — the
    /// research-capture path ([`super::capture_phrase_until_stamped`]) reads it for true Δt. The
    /// normal path passes `false` (and a throwaway buffer), so its behaviour is unchanged. NB:
    /// `compact()` would desync the two, so a stamped capture must run with a `max_pts` high enough
    /// never to thin (the research caller does).
    unsafe fn drain(
        hwnd: HWND,
        acc: &mut (f64, f64),
        pts: &mut Vec<C>,
        stamps: &mut Vec<u32>,
        want_stamps: bool,
    ) -> bool {
        crate::prof::bump(&crate::prof::CAPTURE_POLL);
        let header = std::mem::size_of::<RAWINPUTHEADER>() as u32;
        let mut moved = false;
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&raw mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
            if msg.message == WM_INPUT {
                let mut size: u32 = 0;
                GetRawInputData(
                    msg.lParam as HRAWINPUT,
                    RID_INPUT,
                    std::ptr::null_mut(),
                    &raw mut size,
                    header,
                );
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    let got = GetRawInputData(
                        msg.lParam as HRAWINPUT,
                        RID_INPUT,
                        buf.as_mut_ptr().cast::<c_void>(),
                        &raw mut size,
                        header,
                    );
                    if got != u32::MAX && got > 0 {
                        let ri = &*buf.as_ptr().cast::<RAWINPUT>();
                        if ri.header.dwType == RIM_TYPEMOUSE {
                            let dx = f64::from(ri.data.mouse.lLastX);
                            let dy = f64::from(ri.data.mouse.lLastY);
                            if dx != 0.0 || dy != 0.0 {
                                acc.0 += dx;
                                acc.1 += dy;
                                pts.push(C::new(acc.0, acc.1));
                                if want_stamps {
                                    stamps.push(msg.time);
                                }
                                moved = true;
                            }
                            // wheel notches feed the depth dial (usButtonData is a signed
                            // delta when RI_MOUSE_WHEEL is flagged).
                            let flags = ri.data.mouse.Anonymous.Anonymous.usButtonFlags;
                            if flags & 0x0400 != 0 {
                                let delta =
                                    i32::from(ri.data.mouse.Anonymous.Anonymous.usButtonData as i16);
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
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
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
            ctl: crate::controls::ControlRef,
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
                if !keys.iter().any(|k| k.ctl == s.ctl) {
                    keys.push(KeyState {
                        ctl: s.ctl,
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
                drain(hwnd, &mut acc, &mut pre, &mut Vec::new(), false);
                if pre.len() > 256 {
                    // bound the prebuffer (idle mouse noise between presses means nothing)
                    pre.drain(..pre.len() - 256);
                }
                if super::key_down(0x1B) || stop() {
                    DestroyWindow(hwnd);
                    return None;
                }
                let now = Instant::now();
                for k in &mut keys {
                    let is_down = super::control_down(k.ctl);
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
                                .filter(|s| s.ctl == k.ctl)
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
                            match slots.iter().find(|s| s.ctl == k.ctl && s.taps == k.taps) {
                                Some(s) => break 'wait (s.id, std::mem::take(&mut pre)),
                                None => k.dead = true, // the key's normal job — not ours
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(3));
            };

            let (id, mut pts) = activated;
            let ctl = slots
                .iter()
                .find(|s| s.id == id)
                .map_or(crate::controls::ControlRef {
                    page: 0,
                    usage: 0,
                    pid: None,
                }, |s| s.ctl);
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
            while super::control_down(ctl) {
                if stop() || hold_start.elapsed() > Duration::from_secs(30) {
                    DestroyWindow(hwnd);
                    return None;
                }
                if drain(hwnd, &mut acc, &mut pts, &mut Vec::new(), false) {
                    if pts.len() >= max_pts {
                        compact(&mut pts);
                    }
                    on_progress(&pts);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let tail_end = Instant::now() + Duration::from_millis(cfg.coyote_ms);
            while Instant::now() < tail_end {
                if super::control_down(ctl) {
                    break;
                }
                if drain(hwnd, &mut acc, &mut pts, &mut Vec::new(), false) {
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
        trigger: crate::controls::ControlRef,
        phrase: &crate::feel::Phrase,
        cfg: &crate::feel::FeelConfig,
        max_pts: usize,
        stop: &(impl Fn() -> bool + ?Sized),
        stamps_out: &mut Vec<u32>,
        want_stamps: bool,
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
                drain(hwnd, &mut acc, &mut sink, &mut Vec::new(), false);
                sink.clear();
                if super::key_down(0x1B) || stop() {
                    // ESC (or an external cancel) aborts — quietly, instantly.
                    DestroyWindow(hwnd);
                    return Ok(Vec::new());
                }
                let now = t0.elapsed().as_millis() as u64;
                match watcher.feed(now, super::control_down(trigger)) {
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

            if toggle {
                // ── toggle capture: runs until the NEXT tap of the trigger (or ESC) ──
                // First let the activating press release (its motion already counts).
                while super::control_down(trigger) {
                    if drain(hwnd, &mut acc, &mut pts, stamps_out, want_stamps) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // capture until the closing tap's DOWN edge (responsive close) or ESC.
                // SAFETY DEADMAN (mirrors the hold branch): a 60s cap so a closing tap that never
                // registers — a flickered/missed key edge mid-stroke — can't strand this loop with the
                // cursor LOCKED. 60s is far longer than any real glyph, even a deliberately slow one.
                let toggle_start = Instant::now();
                let mut last_motion = Instant::now();
                loop {
                    if stop() {
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    // deadman (see the hold branch): research path commits on 30s idle, normal path
                    // discards on a hard 60s cap.
                    let timed_out = if want_stamps {
                        last_motion.elapsed() > Duration::from_secs(30)
                    } else {
                        toggle_start.elapsed() > Duration::from_mins(1)
                    };
                    if timed_out {
                        if want_stamps {
                            break; // commit
                        }
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    if super::key_down(0x1B) || super::control_down(trigger) {
                        break;
                    }
                    if drain(hwnd, &mut acc, &mut pts, stamps_out, want_stamps) {
                        last_motion = Instant::now();
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // swallow the closing press so it can't double as the next phrase's first tap
                // (activation-to-deactivate must be free, not a hidden re-activation).
                while super::control_down(trigger) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            } else {
                // ── hold capture: while the final press is held. A full buffer THINS
                // (compact) and keeps capturing — no gesture ever self-terminates. ──
                // SAFETY DEADMAN (see capture_slots): a 30s cap so a stuck key-state can't spin
                // here forever with the cursor LOCKED (frozen mouse + dead modes until restart).
                let hold_start = Instant::now();
                let mut last_motion = Instant::now();
                while super::control_down(trigger) {
                    if stop() {
                        // retired mid-weave: the stroke must NOT commit (its owner withdrew it).
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    // SAFETY DEADMAN. NORMAL path: a hard 30s from press — a stuck key can't spin here
                    // with the cursor LOCKED, and a 30s hold isn't a real flick, so discard. RESEARCH
                    // path (want_stamps): there is NO cap on ACTIVE drawing — the deadman is 30s of NO
                    // MOTION (a stuck key or a long mid-stroke pause), and it COMMITS the captured
                    // stroke rather than losing a long deliberate trace. The stuck-cursor bound holds
                    // either way (a wedged key produces no motion → releases in 30s).
                    let timed_out = if want_stamps {
                        last_motion.elapsed() > Duration::from_secs(30)
                    } else {
                        hold_start.elapsed() > Duration::from_secs(30)
                    };
                    if timed_out {
                        if want_stamps {
                            break; // commit what we have → coyote tail → Ok(pts)
                        }
                        DestroyWindow(hwnd);
                        return Ok(Vec::new());
                    }
                    if drain(hwnd, &mut acc, &mut pts, stamps_out, want_stamps) {
                        last_motion = Instant::now();
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
                    if super::control_down(trigger) {
                        break; // spam: the user is already starting the next weave
                    }
                    if drain(hwnd, &mut acc, &mut pts, stamps_out, want_stamps) {
                        if pts.len() >= max_pts {
                            compact(&mut pts);
                        }
                        on_progress(&pts);
                    }
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
                if GetCursorPos(&raw mut p) != 0 {
                    let r = RECT {
                        left: p.x,
                        top: p.y,
                        right: p.x + 1,
                        bottom: p.y + 1,
                    };
                    ClipCursor(&raw const r);
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
        let mut pts: Vec<C> = (0..601).map(|i| C::new(f64::from(i), f64::from(i * 2))).collect();
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

    // ── direct witnesses for `prepare` (the curvature-arc resampler) ──────────────────────────

    /// Center + unit-scale a path (longer axis → 1) so differently-sized strokes compare directly.
    fn unit_box(z: &[C]) -> Vec<C> {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for c in z {
            x0 = x0.min(c.re);
            y0 = y0.min(c.im);
            x1 = x1.max(c.re);
            y1 = y1.max(c.im);
        }
        let (cx, cy) = ((x0 + x1) * 0.5, (y0 + y1) * 0.5);
        let s = (x1 - x0).max(y1 - y0).max(1e-9);
        z.iter()
            .map(|c| C::new((c.re - cx) / s, (c.im - cy) / s))
            .collect()
    }

    #[test]
    fn prepare_preserves_endpoints() {
        let z = synth_circle(120, 300.0, std::f64::consts::TAU / 120.0);
        let p = prepare(&z, &cfg());
        assert!(p.len() >= 8);
        // resample_weighted pins the true endpoints; smooth() never moves them.
        assert!(p[0].sub(z[0]).abs() < 1e-9, "start wandered");
        assert!(
            p.last().unwrap().sub(*z.last().unwrap()).abs() < bbox_diag(&z) * 0.03,
            "end wandered"
        );
    }

    #[test]
    fn prepare_speed_and_size_invariant() {
        use std::f64::consts::TAU;
        // the SAME circle, drawn small+slow (dense samples) vs big+fast (sparse). The curvature-arc
        // resample must yield the same SHAPE — the property the recognizer's invariance rests on.
        let slow_small = prepare(&synth_circle(320, 90.0, TAU / 320.0), &cfg());
        let fast_big = prepare(&synth_circle(70, 360.0, TAU / 70.0), &cfg());
        let a = unit_box(&resample_uniform(&slow_small, 64));
        let b = unit_box(&resample_uniform(&fast_big, 64));
        let maxd = a
            .iter()
            .zip(&b)
            .map(|(x, y)| x.sub(*y).abs())
            .fold(0.0_f64, f64::max);
        assert!(maxd < 0.08, "prepared shape drifted across size/speed: {maxd}");
    }

    #[test]
    fn prepare_concentrates_samples_where_the_path_turns() {
        // a V: two straight arms, one sharp apex. Curvature-arc resampling must PACK samples at the
        // apex (fine spacing) and leave the arms coarse — the whole point of the change (uniform
        // arc-length spaces them evenly and blunts the corner).
        let p = prepare(&synth_vee(160, 5.0), &cfg());
        let steps: Vec<f64> = (1..p.len()).map(|i| p[i].sub(p[i - 1]).abs()).collect();
        let mut sorted = steps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2].max(1e-9);
        let (mut apex, mut max_turn) = (1usize, 0.0);
        for i in 1..p.len() - 1 {
            let (v0, v1) = (p[i].sub(p[i - 1]), p[i + 1].sub(p[i]));
            if v0.abs() < 1e-9 || v1.abs() < 1e-9 {
                continue;
            }
            let mut d = v1.arg() - v0.arg();
            while d > std::f64::consts::PI {
                d -= std::f64::consts::TAU;
            }
            while d < -std::f64::consts::PI {
                d += std::f64::consts::TAU;
            }
            if d.abs() > max_turn {
                max_turn = d.abs();
                apex = i;
            }
        }
        let local = (steps[apex - 1] + steps[apex.min(steps.len() - 1)]) * 0.5;
        assert!(
            local < median * 0.7,
            "apex not densified: local step {local} vs median {median}"
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
        let acc = f64::from(correct) / f64::from(total);
        assert!(
            acc >= 0.9,
            "classifier accuracy {acc:.2} ({correct}/{total}) below 0.9"
        );
    }

    // ── metamorphic laws (no oracle exists for "the right score", so these assert RELATIONS
    // between outputs instead) + robustness properties, over random-walk strokes. ─────────────
    mod props {
        use super::*;
        use proptest::prelude::*;

        /// Every law below runs the full `analyze`/`prepare`/`dtw` pipeline at least once
        /// (curvature-arc resampling + windowed eigen-fits + O(n·m) DTW) — cheap per-case at
        /// these stroke sizes (dtw ends up on ~10-50-element Sig sequences after windowing), but
        /// kept at the house "heavy math" case count rather than the 256-case "cheap" one.
        fn cfg_heavy() -> ProptestConfig {
            ProptestConfig { cases: 128, ..ProptestConfig::default() }
        }

        /// Random-walk stroke as a HEADING walk, not independent step deltas: each step turns the
        /// current heading by a bounded angle and advances at a bounded positive speed. This makes
        /// the stroke smooth BY CONSTRUCTION (no zero-length segments, every turn well inside the
        /// ±π velocity-angle branch cut) — an earlier independent-delta generator drew consecutive
        /// directions uncorrelated, so near-π reversals were routine, which the continuity-sensitive
        /// invariants (winding) can't survive. It also reads more like an actual hand motion (a pen
        /// has momentum; its direction is continuous). 8..128 points mirrors real capture lengths;
        /// the ±1.8rad turn bound stays clear of the ±π branch cut while still allowing loops,
        /// spirals, and zigzags. Used by the translation/scale/premetric/DTW laws (rotation and
        /// resample state their claims on canonical shapes instead — see those tests).
        fn arb_stroke() -> impl Strategy<Value = Vec<C>> {
            (
                0.0f64..std::f64::consts::TAU,
                proptest::collection::vec((-1.8f64..1.8f64, 0.5f64..8.0f64), 8..128),
            )
                .prop_map(|(heading0, steps)| {
                    let mut heading = heading0;
                    let mut acc = C::new(0.0, 0.0);
                    steps
                        .into_iter()
                        .map(|(turn, speed)| {
                            heading += turn;
                            acc = acc.add(C::new(heading.cos() * speed, heading.sin() * speed));
                            acc
                        })
                        .collect()
                })
        }

        fn rotate(z: &[C], theta: f64) -> Vec<C> {
            let r = C::new(theta.cos(), theta.sin());
            z.iter().map(|p| p.mul(r)).collect()
        }
        fn translate(z: &[C], dx: f64, dy: f64) -> Vec<C> {
            let d = C::new(dx, dy);
            z.iter().map(|p| p.add(d)).collect()
        }
        fn scale_stroke(z: &[C], s: f64) -> Vec<C> {
            z.iter().map(|p| p.scale(s)).collect()
        }

        /// A small, structurally-distinct dictionary (mirrors `classifier_accuracy_on_variant_set`
        /// above) for the ranking-stability laws.
        fn dictionary(cfg: &GlyphConfig) -> Vec<(&'static str, GestureWord)> {
            use std::f64::consts::TAU;
            vec![
                ("circle_cw", analyze(&synth_circle(64, 300.0, TAU / 64.0), cfg)),
                ("circle_ccw", analyze(&synth_circle(64, 300.0, -TAU / 64.0), cfg)),
                ("line", analyze(&synth_line(60), cfg)),
                ("vee", analyze(&synth_vee(60, 8.0), cfg)),
            ]
        }

        /// Rank a word against `dictionary` by `word_distance`, ascending. Only meaningful when
        /// there's a clear margin between 1st and 2nd place — callers `prop_assume!` on that.
        fn ranked(
            dict: &[(&'static str, GestureWord)],
            w: &GestureWord,
            cfg: &GlyphConfig,
        ) -> Vec<(&'static str, f64)> {
            let mut scored: Vec<(&'static str, f64)> = dict
                .iter()
                .map(|(n, t)| (*n, word_distance(w, t, cfg)))
                .collect();
            scored.sort_by(|a, b| a.1.total_cmp(&b.1));
            scored
        }


        proptest! {
            #![proptest_config(cfg_heavy())]

            /// (a) TASK1: rotating a stroke by an arbitrary angle must not change its physical
            /// invariants (glyph.rs doc comment above `Invariants`, ~line 652: "All are
            /// translation-, scale-, and speed-invariant; `winding` and `bending` are also
            /// rotation-invariant" — `closure` is a ratio of magnitudes so it's rotation-invariant
            /// too, trivially). The eigenmotion fit is exactly rotation-EQUIVARIANT by
            /// construction (`fit` solves a LINEAR least-squares recurrence on velocities: if
            /// v'[n] = R·v[n] for a rotation R, then K,G solving v'[n]=K·v'[n-1]-G·v'[n-2] are
            /// IDENTICAL to the unrotated K,G — R factors out of the whole linear system), so any
            /// drift here is floating-point/resampling-boundary noise, not model error. Epsilon
            /// 0.15 matches the existing `invariants_are_speed_and_size_invariant` precedent
            /// (glyph.rs ~1941), which already tolerates that much drift between MUCH more
            /// different strokes (a small/dense vs. a big/sparse circle); a same-shape rotation
            /// should sit well inside it.
            #[test]
            fn invariant_distance_is_rotation_invariant(
                which in 0usize..4,
                theta in 0.0f64..std::f64::consts::TAU,
            ) {
                // Stated on CANONICAL shapes (same reasoning as `resample_preserves_the_winner`):
                // `winding` integrates wrapped velocity-angle deltas and is discontinuous at the
                // ±π branch cut, so an ARBITRARY high-curvature scribble can sit exactly on that
                // cut and legitimately shift winding by a whole turn under rotation — a fact about
                // the winding functional, not a recognizer bug (documented on `Invariants`). The
                // meaningful claim is that the recognizable gestures — where handedness is
                // unambiguous and well-separated from the cut — read identically at every
                // orientation. Canonical shapes are exactly that domain; no prop_assume needed.
                use std::f64::consts::TAU;
                let cfgv = cfg();
                let (name, stroke): (&str, Vec<C>) = match which {
                    0 => ("circle_cw", synth_circle(96, 300.0, TAU / 96.0)),
                    1 => ("circle_ccw", synth_circle(96, 300.0, -TAU / 96.0)),
                    2 => ("line", synth_line(90)),
                    _ => ("vee", synth_vee(90, 8.0)),
                };
                let rotated = rotate(&stroke, theta);
                let a = analyze(&stroke, &cfgv);
                let b = analyze(&rotated, &cfgv);
                prop_assert!(
                    invariant_distance(a.inv, b.inv) < 0.15,
                    "rotation drifted {}'s invariants: {:?} vs {:?}", name, a.inv, b.inv
                );

                // And the winner is orientation-independent: a rotated canonical shape still reads
                // as its own template (ground truth exists here, unlike for a scribble).
                let dict = dictionary(&cfgv);
                let after = ranked(&dict, &b, &cfgv);
                prop_assert_eq!(after[0].0, name, "{} misread after rotation", name);
            }

            /// (b) TASK1: same law, for translation. Velocities cancel a constant offset exactly
            /// (z[n]-z[n-1] removes any additive shift), so the eigenmotion fit — and therefore
            /// every invariant/signature derived from it — is translation-invariant by
            /// construction; only resampling-boundary float noise can move the result. Same
            /// epsilon/margin rationale as the rotation law above. Offsets kept to a "sane" range
            /// (matching the stroke's own coordinate magnitude) so this isn't secretly testing
            /// catastrophic-cancellation behavior at extreme magnitudes — that's TASK2(f)'s job.
            #[test]
            fn invariant_distance_is_translation_invariant(
                stroke in arb_stroke(),
                dx in -500.0f64..500.0f64,
                dy in -500.0f64..500.0f64,
            ) {
                let cfgv = cfg();
                let shifted = translate(&stroke, dx, dy);
                let a = analyze(&stroke, &cfgv);
                let b = analyze(&shifted, &cfgv);
                prop_assert!(
                    invariant_distance(a.inv, b.inv) < 0.15,
                    "translation drifted invariants: {:?} vs {:?}", a.inv, b.inv
                );

                let dict = dictionary(&cfgv);
                let before = ranked(&dict, &a, &cfgv);
                prop_assume!(before.len() >= 2 && before[1].1 - before[0].1 > 0.05);
                let after = ranked(&dict, &b, &cfgv);
                prop_assert_eq!(after[0].0, before[0].0, "ranking flipped under translation");
            }

            /// (c) TASK1: same law, for scale (0.1x–10x). Scaling multiplies every velocity by
            /// the same real factor `s`; the least-squares system for K,G is homogeneous in the
            /// data (both sides scale by `s`), so K,G — and hence `mag`/`rot` — are exactly
            /// scale-invariant by construction. `resid_norm` is `residual/mean_step`, a ratio of
            /// two quantities that both scale by `s`, so it's scale-invariant too. Same
            /// epsilon/margin rationale as above.
            #[test]
            fn invariant_distance_is_scale_invariant(
                stroke in arb_stroke(),
                s in 0.1f64..10.0f64,
            ) {
                let cfgv = cfg();
                let scaled = scale_stroke(&stroke, s);
                let a = analyze(&stroke, &cfgv);
                let b = analyze(&scaled, &cfgv);
                prop_assert!(
                    invariant_distance(a.inv, b.inv) < 0.15,
                    "scale drifted invariants: {:?} vs {:?}", a.inv, b.inv
                );

                let dict = dictionary(&cfgv);
                let before = ranked(&dict, &a, &cfgv);
                prop_assume!(before.len() >= 2 && before[1].1 - before[0].1 > 0.05);
                let after = ranked(&dict, &b, &cfgv);
                prop_assert_eq!(after[0].0, before[0].0, "ranking flipped under scaling");
            }

            /// (d) TASK1 premetric, part 1: d(a,a) ≈ 0. Already spot-checked for synthetic shapes
            /// by `dtw_zero_to_self_and_orders_shapes` above; this generalizes it to arbitrary
            /// strokes. The DP table's diagonal path (matching each window to itself) costs
            /// exactly 0 by construction (`sig_distance(x,x,_) == 0`), so this should hold near
            /// machine epsilon, not just "small".
            #[test]
            fn distance_is_a_premetric(stroke in arb_stroke()) {
                let sigs = signature_sequence(&stroke, &cfg());
                prop_assert!(dtw(&sigs, &sigs, &cfg()) < 1e-9);
            }

            /// (d) TASK1 premetric, part 2 — documenting REALITY, not the naive claim: `dtw` is
            /// NOT symmetric in general. Its own doc comment (glyph.rs ~295-301) says so: the DP
            /// score is normalized by the QUERY length only ("For 1-NN this is constant across
            /// templates, so ranking == raw DTW... A template-dependent divisor... would dilute
            /// that"), so `dtw(a,b) != dtw(b,a)` whenever `len(a) != len(b)`. What IS symmetric by
            /// construction is the UNNORMALIZED DP table: the recurrence's cost function
            /// (`sig_distance`) is symmetric, and swapping the two input sequences is exactly a
            /// transpose of the same recurrence, so `dp[n][m]` (== `dtw(a,b) * len(a)`) must equal
            /// `dp'[m][n]` (== `dtw(b,a) * len(b)`) up to float summation-order noise. This is the
            /// real (asymmetric) premetric law this codebase implements.
            #[test]
            fn dtw_asymmetry_is_exactly_the_query_length_normalization(
                stroke_a in arb_stroke(),
                stroke_b in arb_stroke(),
            ) {
                let cfgv = cfg();
                let a = signature_sequence(&stroke_a, &cfgv);
                let b = signature_sequence(&stroke_b, &cfgv);
                prop_assume!(!a.is_empty() && !b.is_empty());
                let d_ab = dtw(&a, &b, &cfgv);
                let d_ba = dtw(&b, &a, &cfgv);
                prop_assert!(d_ab >= 0.0 && d_ba >= 0.0);
                let (na, nb) = (a.len() as f64, b.len() as f64);
                let (raw_ab, raw_ba) = (d_ab * na, d_ba * nb);
                let tol = 1e-6 * raw_ab.abs().max(raw_ba.abs()).max(1.0);
                prop_assert!(
                    (raw_ab - raw_ba).abs() < tol,
                    "unnormalized DTW should be swap-symmetric: {raw_ab} vs {raw_ba}"
                );
            }

            /// (e) TASK1: uniformly resampling a stroke to a different density (drawn "slower" or
            /// "faster") must not change which dictionary word wins — the whole point of
            /// `resample_uniform`/`prepare`'s speed-invariance design (see the `speed_invariant`
            /// and `prepare_speed_and_size_invariant` tests above). Ranking, not raw score, per
            /// the house rule: exact-distance equality would be flaky (resampling genuinely
            /// perturbs the shape a little), but which template wins should not, away from
            /// near-ties.
            #[test]
            fn resample_preserves_the_winner(
                which in 0usize..4,
                scale in 0.5f64..2.0,
                n_sparse in 20usize..40,
                n_dense in 60usize..120,
            ) {
                // Stated on CANONICAL shapes, not arbitrary random walks: earlier rounds of this
                // law kept rediscovering that an arbitrary scribble sitting near a classifier
                // decision boundary flips winners under resampling — true, but that's a fact
                // about decision boundaries, not a recognizer bug (there is no "right answer" to
                // preserve for a shapeless scribble). The meaningful claim is that for each
                // dictionary shape — where ground truth EXISTS — the right template wins at every
                // reasonable capture density and size. No prop_assume domain-carving needed.
                use std::f64::consts::TAU;
                let cfgv = cfg();
                let (name, stroke): (&str, Vec<C>) = match which {
                    0 => ("circle_cw", synth_circle(96, 300.0, TAU / 96.0)),
                    1 => ("circle_ccw", synth_circle(96, 300.0, -TAU / 96.0)),
                    2 => ("line", synth_line(90)),
                    _ => ("vee", synth_vee(90, 8.0)),
                };
                let stroke = scale_stroke(&stroke, scale);
                let dict = dictionary(&cfgv);
                for n in [n_sparse, n_dense] {
                    let variant = resample_uniform(&stroke, n);
                    let word = analyze(&variant, &cfgv);
                    let ranking = ranked(&dict, &word, &cfgv);
                    prop_assert_eq!(
                        ranking[0].0, name,
                        "canonical {} at {} samples (scale {}) was misread", name, n, scale
                    );
                }
            }

            /// (g) TASK2: dtw's basic algebraic laws that DO hold by construction: non-negativity
            /// (every DP cell is a sum of `sig_distance` outputs, which is a `.sqrt()` of a
            /// nonnegative sum, so never negative) is checked inline above; this is the standalone
            /// coverage over arbitrary stroke pairs (not just the fixed synthetic shapes in
            /// `dtw_zero_to_self_and_orders_shapes`). NOTE: "monotone under concatenating
            /// identical suffixes" (the other candidate law from the task) is NOT true by
            /// construction here and is deliberately NOT encoded — `signature_sequence` runs the
            /// whole stroke through curvature-arc resampling (`prepare`) before windowing, so
            /// appending a suffix to two strokes does not append anything to their Sig sequences;
            /// it can rescale/reshape the ENTIRE resampled representation (the base sample count,
            /// the turning-driven growth factor, and every window's boundary all depend globally
            /// on the whole path). Asserting it would be asserting something the code doesn't
            /// promise.
            #[test]
            fn dtw_is_nonnegative(stroke_a in arb_stroke(), stroke_b in arb_stroke()) {
                let cfgv = cfg();
                let a = signature_sequence(&stroke_a, &cfgv);
                let b = signature_sequence(&stroke_b, &cfgv);
                prop_assert!(dtw(&a, &b, &cfgv) >= 0.0);
            }
        }

        // ── (f) TASK2: robustness — no panics, no infinite loops, on degenerate input ──────────

        /// Run every public (+ intra-module-private, reachable via `super::*`) kernel over `z`.
        /// `NaN`-in may produce `NaN`-out (that's fine, and observed for several of these paths —
        /// e.g. `fit`'s Q14 quantization casts a `NaN` intermediate to `0` via Rust's saturating
        /// float-to-int cast, `C::sqrt` launders a `NaN` discriminant to `0` via `.max(0.0)`); the
        /// only thing this asserts is that nothing PANICS and nothing loops forever. Indices for
        /// `phase_boundary_score`/`choose_block_len` are swept only over `0..z.len()`, mirroring
        /// how `segment` actually calls them (both have `usize` subtraction that underflows for
        /// out-of-range indices no real caller ever passes — see `choose_block_len`'s
        /// `z.len() - start` — so that's a separate, not-reachable-from-any-caller finding, not
        /// exercised here).
        fn exercise_all_kernels(z: &[C], cfg: &GlyphConfig) {
            let _ = fit(z);
            let _ = fit(&velocities(z));
            let _ = smooth(z, 3);
            let _ = resample_uniform(z, 24);
            if z.len() >= 2 {
                let w = vec![1.0f64; z.len() - 1];
                let _ = resample_weighted(z, &w, 24);
            }
            let _ = segment(z);
            for at in 0..z.len() {
                let _ = phase_boundary_score(z, at);
            }
            for start in 0..z.len() {
                let _ = choose_block_len(z, start);
            }
            let _ = fit_sequence(z);
            let _ = prepare(z, cfg);
            let _ = signature_sequence(z, cfg);
            let word = analyze(z, cfg);
            let _ = invariant_distance(word.inv, word.inv);
            let _ = word_distance(&word, &word, cfg);
            let _ = dtw(&word.sigs, &word.sigs, cfg);
            let _ = exemplar_path(z, cfg);
        }

        #[test]
        fn no_kernel_panics_on_degenerate_input() {
            let c = cfg();
            let cases: Vec<(&str, Vec<C>)> = vec![
                ("empty", vec![]),
                ("single_point", vec![C::new(1.0, 2.0)]),
                ("two_identical_points", vec![C::new(3.0, 3.0), C::new(3.0, 3.0)]),
                ("all_collinear", (0..40).map(|i| C::new(f64::from(i), 2.0 * f64::from(i))).collect()),
                (
                    "extreme_magnitude",
                    vec![
                        C::new(1e15, -1e15),
                        C::new(-1e15, 1e15),
                        C::new(1e15, 1e15),
                        C::new(-1e15, -1e15),
                        C::new(0.0, 1e15),
                    ],
                ),
                (
                    "nan_and_infinity",
                    vec![
                        C::new(0.0, 0.0),
                        C::new(f64::NAN, 1.0),
                        C::new(2.0, f64::INFINITY),
                        C::new(f64::NEG_INFINITY, f64::NAN),
                        C::new(3.0, 4.0),
                    ],
                ),
                ("all_nan", vec![C::new(f64::NAN, f64::NAN); 12]),
                ("all_infinite", vec![C::new(f64::INFINITY, f64::INFINITY); 8]),
            ];
            for (label, z) in cases {
                // catch_unwind so one failing case doesn't hide the label of the others' results.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    exercise_all_kernels(&z, &c);
                }));
                assert!(result.is_ok(), "kernel panicked on degenerate input case: {label}");
            }
        }
    }
}
