// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! SHAPE SNAP — the recognition engine used as a drawing tool.
//!
//! v2: recognition is a CONVERSATION between two signals, both native to the codec's worldview:
//!
//!   * **structure** — corner detection over an arc-length-resampled, smoothed stroke (turning
//!     angle maxima). Corners are the stroke's *geometric identity*: 0–2 corners reads round,
//!     3 reads triangular, 4 reads quadrilateral (axis-aligned edges = rect, rotated = diamond).
//!   * **the eigen corpus** — the SAME eigenmotion engine that recognizes spells, matched against
//!     a corpus of synthesized UNICODE GEOMETRY (`○ □ △` in both windings) through
//!     [`crate::glyph::analyze`] / [`crate::glyph::word_distance`]. The corpus characters aren't
//!     decoration: each template is the canonical geometric identity of a codepoint, so the
//!     matcher is a tiny "geometric alphabet" recognizer — the seed of the utf-8-corpus idea.
//!
//! Structure leads (corners are cheap and decisive when present); the eigen corpus arbitrates the
//! soft cases (2-ish corners: a lazy rectangle vs a wobbled circle). The codec's own
//! [`crate::glyph::Invariants::closure`] decides open vs closed — not an ad-hoc gap test alone.
//!
//! Honesty rule: **ambiguity keeps the freehand.** A stroke that isn't clearly one primitive
//! returns `None` and the ink stays exactly as drawn — the tool never fights a deliberate scribble.

use crate::glyph::{self, C};

/// A snapped primitive, in the stroke's own coordinate space.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Line {
        a: (f64, f64),
        b: (f64, f64),
    },
    /// A shaft with an ideal head at `b` — whiteboards are 90% arrows.
    Arrow {
        a: (f64, f64),
        b: (f64, f64),
    },
    Ellipse {
        cx: f64,
        cy: f64,
        rx: f64,
        ry: f64,
    },
    Rect {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    /// Vertices in stroke order (the drawn corners, kept where the hand put them).
    Triangle {
        a: (f64, f64),
        b: (f64, f64),
        c: (f64, f64),
    },
    /// A rotated quadrilateral fit to the bbox midpoints (the ◇ identity).
    Diamond {
        cx: f64,
        cy: f64,
        rx: f64,
        ry: f64,
    },
}

/// max perpendicular deviation from the endpoint chord, as a fraction of chord length, at/below
/// which a stroke is a LINE. Deviation-based (not net/arc) because hand jitter inflates arc
/// length without making the stroke any less of a line.
const LINE_GATE: f64 = 0.085;
/// codec closure (1 − gap/arc) at/above which a figure reads CLOSED.
const CLOSURE_GATE: f64 = 0.78;
/// the eigen winner must beat the nearest OTHER identity by this relative margin in soft cases.
const CLEAR_WINNER: f64 = 0.12;
/// a turning maximum must turn at least this much (radians, ≈50°) to count as a corner.
const CORNER_TURN: f64 = 0.88;

/// Classify + fit. `None` = keep the freehand (the honest default).
pub fn snap(path: &[C]) -> Option<Shape> {
    if path.len() < 8 {
        return None;
    }
    let arc = arc_len(path);
    if arc < 24.0 {
        return None; // a dot/jitter is not a shape
    }
    // one resampled+smoothed working copy (uniform spacing = stable corner geometry)
    let r = resample(&smooth(path, 5), 96);
    if r.len() < 12 {
        return None;
    }
    let inv = glyph::invariants(&r);

    // ── open strokes: line, then arrow ────────────────────────────────────
    if inv.closure < CLOSURE_GATE {
        if let Some(s) = try_line(path, &r) {
            return Some(s);
        }
        if let Some(s) = try_arrow(path, &r) {
            return Some(s);
        }
        return None; // open and curved — a hook, an arc: freehand
    }

    // ── closed figures: corners lead, the eigen corpus arbitrates ────────
    let corners = corners_cyclic(&r);
    let (x0, y0, x1, y1) = bbox(path);
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    match corners.len() {
        3 => Some(Shape::Triangle {
            a: (r[corners[0]].re, r[corners[0]].im),
            b: (r[corners[1]].re, r[corners[1]].im),
            c: (r[corners[2]].re, r[corners[2]].im),
        }),
        4 => {
            // edge orientation decides □ vs ◇: chords between consecutive corners measured
            // against the axes (the codec can't see global rotation — by design — so this
            // is the structural signal's job).
            let mut dev = 0.0;
            for w in 0..4 {
                let a = r[corners[w]];
                let b = r[corners[(w + 1) % 4]];
                let ang = (b.im - a.im).atan2(b.re - a.re).abs();
                // distance to the nearest axis direction (0 or π/2), in radians
                let d = [0.0, std::f64::consts::FRAC_PI_2, std::f64::consts::PI]
                    .iter()
                    .map(|t| (ang - t).abs())
                    .fold(f64::MAX, f64::min);
                dev += d;
            }
            if dev / 4.0 <= 0.35 {
                Some(Shape::Rect { x0, y0, x1, y1 })
            } else {
                Some(Shape::Diamond {
                    cx,
                    cy,
                    rx: (x1 - x0) / 2.0,
                    ry: (y1 - y0) / 2.0,
                })
            }
        }
        n if n <= 2 => {
            // round-ish or sloppy-cornered: ask the eigen corpus which identity this IS.
            match corpus_best(path) {
                Some('○') => Some(Shape::Ellipse {
                    cx,
                    cy,
                    rx: (x1 - x0) / 2.0,
                    ry: (y1 - y0) / 2.0,
                }),
                Some('□') => Some(Shape::Rect { x0, y0, x1, y1 }),
                Some('△') => Some(Shape::Triangle {
                    a: (cx, y0),
                    b: (x1, y1),
                    c: (x0, y1),
                }),
                _ => None,
            }
        }
        _ => None, // 5+ corners: a star, a scribble — freehand keeps its dignity
    }
}

/// The UNICODE GEOMETRY corpus — each entry is a codepoint's canonical shape, synthesized, in
/// both windings (CW ≠ CCW in the eigen space, by design). This is the seed corpus for
/// geometric-identity recognition; the spellcasting engine can train against the same alphabet.
pub fn corpus() -> Vec<(char, Vec<C>)> {
    let n = 96;
    let tau = std::f64::consts::TAU;
    vec![
        ('○', glyph::synth_circle(n, 200.0, tau / n as f64)),
        ('○', glyph::synth_circle(n, 200.0, -(tau / n as f64))),
        ('□', synth_rect(n, 280.0, 280.0, false)),
        ('□', synth_rect(n, 280.0, 280.0, true)),
        ('△', synth_triangle(n, 300.0, false)),
        ('△', synth_triangle(n, 300.0, true)),
    ]
}

/// Best corpus identity for a stroke, or `None` when no identity clearly owns it. The margin is
/// judged against the nearest OTHER identity (the same character's other winding agreeing with
/// the winner is confirmation, not competition).
pub fn corpus_best(path: &[C]) -> Option<char> {
    let cfg = glyph::GlyphConfig::default();
    let word = glyph::analyze(path, &cfg);
    let mut scored: Vec<(char, f64)> = corpus()
        .iter()
        .map(|(ch, pts)| {
            (
                *ch,
                glyph::word_distance(&word, &glyph::analyze(pts, &cfg), &cfg),
            )
        })
        .collect();
    scored.sort_by(|a, b| a.1.total_cmp(&b.1));
    let (best, best_d) = scored[0];
    let rival = scored
        .iter()
        .find(|(k, _)| *k != best)
        .map(|(_, d)| *d)
        .unwrap_or(f64::MAX);
    if rival - best_d < CLEAR_WINNER * best_d.max(1e-9) {
        return None;
    }
    Some(best)
}

/// STRAIGHT → Line through the actual drawn endpoints.
fn try_line(path: &[C], r: &[C]) -> Option<Shape> {
    let (ax, ay) = (r[0].re, r[0].im);
    let (bx, by) = (r[r.len() - 1].re, r[r.len() - 1].im);
    let chord = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt().max(1e-9);
    if chord < 24.0 {
        return None;
    }
    let max_dev = r
        .iter()
        .map(|p| ((bx - ax) * (ay - p.im) - (ax - p.re) * (by - ay)).abs() / chord)
        .fold(0.0f64, f64::max);
    if max_dev / chord <= LINE_GATE {
        let a = (path[0].re, path[0].im);
        let b = (path[path.len() - 1].re, path[path.len() - 1].im);
        return Some(Shape::Line { a, b });
    }
    None
}

/// ARROW → an open stroke whose FAR point ends a straight shaft, with the rest of the stroke
/// (the drawn head) staying close to that tip. The head is re-rendered IDEAL — the codec's gift:
/// you sketch the intent, the identity supplies the form.
fn try_arrow(path: &[C], r: &[C]) -> Option<Shape> {
    let start = r[0];
    // the tip is the farthest point from the start
    let (tip_i, tip) = r
        .iter()
        .enumerate()
        .max_by(|a, b| dist(*a.1, start).total_cmp(&dist(*b.1, start)))
        .map(|(i, p)| (i, *p))?;
    let shaft = dist(tip, start);
    if shaft < 40.0 || tip_i < r.len() / 3 || tip_i + 2 >= r.len() {
        return None; // no head drawn after the tip, or the tip isn't out front
    }
    // shaft straightness up to the tip
    let chord = shaft.max(1e-9);
    let max_dev = r[..=tip_i]
        .iter()
        .map(|p| {
            ((tip.re - start.re) * (start.im - p.im) - (start.re - p.re) * (tip.im - start.im))
                .abs()
                / chord
        })
        .fold(0.0f64, f64::max);
    if max_dev / chord > 0.10 {
        return None;
    }
    // the drawn head: everything after the tip stays near the tip and is a real flick
    let head_arc: f64 = r[tip_i..].windows(2).map(|w| dist(w[0], w[1])).sum();
    let head_near = r[tip_i..].iter().all(|p| dist(*p, tip) <= 0.45 * shaft);
    if head_arc < 0.05 * shaft || head_arc > 0.9 * shaft || !head_near {
        return None;
    }
    Some(Shape::Arrow {
        a: (path[0].re, path[0].im),
        b: (tip.re, tip.im),
    })
}

/// Render a snapped shape back into a polyline (the whiteboard draws polylines — one ink model).
pub fn polyline(shape: &Shape) -> Vec<(f64, f64)> {
    match shape {
        Shape::Line { a, b } => vec![*a, *b],
        Shape::Arrow { a, b } => {
            // ideal head: two barbs swept back ±28° from the shaft direction
            let ang = (b.1 - a.1).atan2(b.0 - a.0);
            let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            let hl = (0.18 * len).clamp(14.0, 64.0);
            let barb = |da: f64| {
                (
                    b.0 + hl * (ang + std::f64::consts::PI + da).cos(),
                    b.1 + hl * (ang + std::f64::consts::PI + da).sin(),
                )
            };
            vec![*a, *b, barb(0.49), *b, barb(-0.49)]
        }
        Shape::Rect { x0, y0, x1, y1 } => {
            vec![(*x0, *y0), (*x1, *y0), (*x1, *y1), (*x0, *y1), (*x0, *y0)]
        }
        Shape::Triangle { a, b, c } => vec![*a, *b, *c, *a],
        Shape::Diamond { cx, cy, rx, ry } => vec![
            (*cx, cy - ry),
            (cx + rx, *cy),
            (*cx, cy + ry),
            (cx - rx, *cy),
            (*cx, cy - ry),
        ],
        Shape::Ellipse { cx, cy, rx, ry } => (0..=64)
            .map(|i| {
                let a = i as f64 / 64.0 * std::f64::consts::TAU;
                (cx + rx * a.cos(), cy + ry * a.sin())
            })
            .collect(),
    }
}

/// The unicode identity of a snapped shape — for status lines ("✨ set: △") and the corpus story.
pub fn identity(shape: &Shape) -> char {
    match shape {
        Shape::Line { .. } => '─',
        Shape::Arrow { .. } => '→',
        Shape::Ellipse { .. } => '○',
        Shape::Rect { .. } => '□',
        Shape::Triangle { .. } => '△',
        Shape::Diamond { .. } => '◇',
    }
}

/// A synthetic rectangle perimeter (n points, w×h, optionally reversed winding) — the □ template.
pub fn synth_rect(n: usize, w: f64, h: f64, reverse: bool) -> Vec<C> {
    let per = 2.0 * (w + h);
    let mut pts: Vec<C> = (0..n)
        .map(|i| {
            let d = i as f64 / n as f64 * per;
            let (x, y) = if d < w {
                (d, 0.0)
            } else if d < w + h {
                (w, d - w)
            } else if d < 2.0 * w + h {
                (w - (d - w - h), h)
            } else {
                (0.0, h - (d - 2.0 * w - h))
            };
            C::new(x, y)
        })
        .collect();
    if reverse {
        pts.reverse();
    }
    pts
}

/// A synthetic equilateral-ish triangle perimeter — the △ template.
pub fn synth_triangle(n: usize, side: f64, reverse: bool) -> Vec<C> {
    let h = side * 0.866;
    let verts = [(side / 2.0, 0.0), (side, h), (0.0, h)];
    let per = 3.0 * side;
    let mut pts: Vec<C> = (0..n)
        .map(|i| {
            let d = i as f64 / n as f64 * per;
            let edge = (d / side) as usize % 3;
            let t = (d % side) / side;
            let (ax, ay) = verts[edge];
            let (bx, by) = verts[(edge + 1) % 3];
            C::new(ax + (bx - ax) * t, ay + (by - ay) * t)
        })
        .collect();
    if reverse {
        pts.reverse();
    }
    pts
}

/// Corner detection on a CLOSED, uniformly-resampled ring: turning angle at each sample over a
/// small lookahead, then non-maximum suppression with a minimum separation. Returns sample
/// indices in stroke order.
fn corners_cyclic(r: &[C]) -> Vec<usize> {
    let n = r.len();
    if n < 12 {
        return Vec::new();
    }
    let w = (n / 24).max(2);
    let sep = (n / 12).max(4);
    let turn = |i: usize| -> f64 {
        let a = r[(i + n - w) % n];
        let b = r[i];
        let c = r[(i + w) % n];
        let (v1x, v1y) = (b.re - a.re, b.im - a.im);
        let (v2x, v2y) = (c.re - b.re, c.im - b.im);
        let l1 = (v1x * v1x + v1y * v1y).sqrt();
        let l2 = (v2x * v2x + v2y * v2y).sqrt();
        if l1 < 1e-9 || l2 < 1e-9 {
            return 0.0;
        }
        ((v1x * v2x + v1y * v2y) / (l1 * l2))
            .clamp(-1.0, 1.0)
            .acos()
    };
    let turns: Vec<f64> = (0..n).map(turn).collect();
    // non-maximum suppression over the ring
    let mut picks: Vec<usize> = (0..n)
        .filter(|&i| {
            turns[i] >= CORNER_TURN
                && (1..=sep)
                    .all(|d| turns[i] >= turns[(i + d) % n] && turns[i] >= turns[(i + n - d) % n])
        })
        .collect();
    // suppression ties (flat maxima) can pick neighbours — keep the first of any cluster
    picks.dedup_by(|a, b| a.saturating_sub(*b) < sep);
    if picks.len() >= 2 && picks[picks.len() - 1] + sep > picks[0] + r.len() {
        picks.pop();
    }
    picks
}

/// Arc-length resample to exactly `n` points (uniform spacing — corner geometry needs it).
fn resample(p: &[C], n: usize) -> Vec<C> {
    if p.len() < 2 || n < 2 {
        return p.to_vec();
    }
    let total = arc_len(p);
    if total < 1e-9 {
        return vec![p[0]; n.min(4)];
    }
    let step = total / (n - 1) as f64;
    let mut out = Vec::with_capacity(n);
    out.push(p[0]);
    let mut acc = 0.0;
    let mut next = step;
    for w in p.windows(2) {
        let seg = ((w[1].re - w[0].re).powi(2) + (w[1].im - w[0].im).powi(2)).sqrt();
        if seg < 1e-12 {
            continue;
        }
        while next <= acc + seg && out.len() < n {
            let t = (next - acc) / seg;
            out.push(C::new(
                w[0].re + (w[1].re - w[0].re) * t,
                w[0].im + (w[1].im - w[0].im) * t,
            ));
            next += step;
        }
        acc += seg;
    }
    while out.len() < n {
        out.push(p[p.len() - 1]);
    }
    out
}

/// Light moving-average smoothing (window k) — denoises hand jitter without moving the stroke.
fn smooth(p: &[C], k: usize) -> Vec<C> {
    let k = k.max(1);
    (0..p.len())
        .map(|i| {
            let lo = i.saturating_sub(k / 2);
            let hi = (i + k / 2 + 1).min(p.len());
            let n = (hi - lo) as f64;
            let (sx, sy) = p[lo..hi]
                .iter()
                .fold((0.0, 0.0), |a, c| (a.0 + c.re, a.1 + c.im));
            C::new(sx / n, sy / n)
        })
        .collect()
}

fn dist(a: C, b: C) -> f64 {
    ((a.re - b.re).powi(2) + (a.im - b.im).powi(2)).sqrt()
}

fn arc_len(p: &[C]) -> f64 {
    p.windows(2)
        .map(|w| ((w[1].re - w[0].re).powi(2) + (w[1].im - w[0].im).powi(2)).sqrt())
        .sum()
}

fn bbox(p: &[C]) -> (f64, f64, f64, f64) {
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for c in p {
        x0 = x0.min(c.re);
        y0 = y0.min(c.im);
        x1 = x1.max(c.re);
        y1 = y1.max(c.im);
    }
    (x0, y0, x1, y1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{add_noise, synth_circle, synth_line};

    /// A noisy hand-drawn-ish line snaps to a Line through its endpoints.
    #[test]
    fn line_snaps() {
        let stroke = add_noise(&synth_line(80), 2.0, 7);
        match snap(&stroke) {
            Some(Shape::Line { a, b }) => {
                assert!((a.0 - stroke[0].re).abs() < 1e-9);
                assert!((b.0 - stroke[79].re).abs() < 1e-9);
            }
            other => panic!("a straight stroke must snap to Line, got {other:?}"),
        }
    }

    /// A noisy circle (either winding) snaps to an Ellipse fit to its bbox.
    #[test]
    fn circle_snaps_both_windings() {
        for omega in [
            std::f64::consts::TAU / 120.0,
            -std::f64::consts::TAU / 120.0,
        ] {
            let stroke = add_noise(&synth_circle(120, 150.0, omega), 4.0, 11);
            match snap(&stroke) {
                Some(Shape::Ellipse { rx, ry, .. }) => {
                    assert!(rx > 100.0 && ry > 100.0, "fit must track the drawn size");
                }
                other => panic!("a circle must snap to Ellipse, got {other:?}"),
            }
        }
    }

    /// A noisy rectangle snaps to a Rect on its bbox — corners carry it even when sloppy.
    #[test]
    fn rect_snaps() {
        for seed in [3u64, 9, 21] {
            let stroke = add_noise(&synth_rect(160, 360.0, 200.0, false), 4.0, seed);
            match snap(&stroke) {
                Some(Shape::Rect { x0, y0, x1, y1 }) => {
                    assert!(x1 - x0 > 300.0 && y1 - y0 > 150.0);
                }
                other => panic!("a rectangle must snap to Rect (seed {seed}), got {other:?}"),
            }
        }
    }

    /// A noisy triangle snaps to a Triangle whose vertices track the drawn corners.
    #[test]
    fn triangle_snaps() {
        for seed in [5u64, 13] {
            let stroke = add_noise(&synth_triangle(150, 320.0, false), 4.0, seed);
            match snap(&stroke) {
                Some(Shape::Triangle { a, b, c }) => {
                    // the three vertices must be far apart (a real triangle, not a degenerate)
                    let d = |p: (f64, f64), q: (f64, f64)| {
                        ((p.0 - q.0).powi(2) + (p.1 - q.1).powi(2)).sqrt()
                    };
                    assert!(d(a, b) > 100.0 && d(b, c) > 100.0 && d(c, a) > 100.0);
                }
                other => panic!("a triangle must snap to Triangle (seed {seed}), got {other:?}"),
            }
        }
    }

    /// A rotated square (diamond orientation) snaps to Diamond, not Rect.
    #[test]
    fn diamond_snaps() {
        // synthesize a diamond: square perimeter rotated 45°
        let sq = synth_rect(160, 280.0, 280.0, false);
        let (s, c) = (
            std::f64::consts::FRAC_PI_4.sin(),
            std::f64::consts::FRAC_PI_4.cos(),
        );
        let rot: Vec<C> = sq
            .iter()
            .map(|p| C::new(p.re * c - p.im * s, p.re * s + p.im * c))
            .collect();
        let stroke = add_noise(&rot, 3.0, 17);
        match snap(&stroke) {
            Some(Shape::Diamond { rx, ry, .. }) => {
                assert!(rx > 100.0 && ry > 100.0);
            }
            other => panic!("a rotated square must snap to Diamond, got {other:?}"),
        }
    }

    /// A shaft with a flicked head snaps to an Arrow at the tip.
    #[test]
    fn arrow_snaps() {
        // shaft straight right, then a small back-flick (the drawn head)
        let mut pts: Vec<C> = (0..70).map(|i| C::new(i as f64 * 6.0, 0.0)).collect();
        let tip = *pts.last().unwrap();
        for i in 1..=12 {
            pts.push(C::new(tip.re - i as f64 * 4.0, tip.im - i as f64 * 3.0));
        }
        let stroke = add_noise(&pts, 1.5, 23);
        match snap(&stroke) {
            Some(Shape::Arrow { a, b }) => {
                assert!(b.0 - a.0 > 300.0, "tip must be out front of the start");
            }
            other => panic!("a flicked shaft must snap to Arrow, got {other:?}"),
        }
    }

    /// Ambiguity keeps the freehand: an open hook and a tiny jitter both refuse to snap.
    #[test]
    fn ambiguity_stays_freehand() {
        // half a circle: curved AND open — neither line, arrow, nor closed figure
        let hook: Vec<C> = synth_circle(120, 150.0, std::f64::consts::TAU / 240.0);
        assert_eq!(snap(&hook), None, "an open arc must stay freehand");
        let dot = synth_line(4);
        assert_eq!(snap(&dot), None, "jitter is not a shape");
    }

    /// A 5-pointed zigzag star outline stays freehand (5+ corners = not a snappable primitive).
    #[test]
    fn star_stays_freehand() {
        let n = 200;
        let stroke: Vec<C> = (0..n)
            .map(|i| {
                let t = i as f64 / n as f64 * std::f64::consts::TAU;
                // a 10-vertex star polygon radius alternation
                let k = (i * 10 / n) % 2;
                let r = if k == 0 { 200.0 } else { 90.0 };
                C::new(r * t.cos(), r * t.sin())
            })
            .collect();
        assert_eq!(snap(&stroke), None, "a star is the hand's own — freehand");
    }

    /// The corpus identities are mutually distinguishable through the eigen engine.
    #[test]
    fn corpus_identities_separate() {
        assert_eq!(
            corpus_best(&synth_circle(96, 180.0, std::f64::consts::TAU / 96.0)),
            Some('○')
        );
        assert_eq!(corpus_best(&synth_triangle(96, 300.0, false)), Some('△'));
        // the square may resolve □ or stay uncertain under noise, but must NEVER read ○
        let sq = corpus_best(&synth_rect(96, 280.0, 280.0, false));
        assert_ne!(sq, Some('○'), "a square must never read as a circle");
        assert_ne!(sq, Some('△'), "a square must never read as a triangle");
    }

    /// The polyline renderer closes rects/triangles/diamonds, rounds ellipses, barbs arrows.
    #[test]
    fn polylines_render_sanely() {
        assert_eq!(
            polyline(&Shape::Line {
                a: (0.0, 0.0),
                b: (5.0, 5.0)
            })
            .len(),
            2
        );
        let r = polyline(&Shape::Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        });
        assert_eq!(r.first(), r.last(), "rect closes");
        let t = polyline(&Shape::Triangle {
            a: (0.0, 0.0),
            b: (10.0, 0.0),
            c: (5.0, 8.0),
        });
        assert_eq!(t.first(), t.last(), "triangle closes");
        let d = polyline(&Shape::Diamond {
            cx: 0.0,
            cy: 0.0,
            rx: 5.0,
            ry: 5.0,
        });
        assert_eq!(d.first(), d.last(), "diamond closes");
        let e = polyline(&Shape::Ellipse {
            cx: 0.0,
            cy: 0.0,
            rx: 5.0,
            ry: 5.0,
        });
        assert_eq!(e.len(), 65);
        let a = polyline(&Shape::Arrow {
            a: (0.0, 0.0),
            b: (100.0, 0.0),
        });
        assert_eq!(a.len(), 5, "shaft + two barbs");
        // identities for the status line
        assert_eq!(
            identity(&Shape::Triangle {
                a: (0.0, 0.0),
                b: (1.0, 0.0),
                c: (0.5, 1.0)
            }),
            '△'
        );
        assert_eq!(
            identity(&Shape::Arrow {
                a: (0.0, 0.0),
                b: (1.0, 0.0)
            }),
            '→'
        );
    }
}
