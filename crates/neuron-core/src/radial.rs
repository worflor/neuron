// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Radial (pie / comms-wheel) menu — the **degenerate weave**: the simplest case of the
//! [`crate::spellweaving`] system, NOT a separate one. Hold the trigger, flick a direction, release;
//! the flick's net direction buckets into one of N sectors, each bound to a quick action. It's a
//! one-segment glyph you only read directionally — no shape recognition, just *direction* — instant,
//! muscle-memory, like an in-game comms wheel but for real actions, tied to the mouse itself.
//!
//! Because it's a subset of spellweaving it shares everything: the same hold-to-do activation (no
//! 24/7 polling), the same Raw Input capture path (`glyph::capture_held`), and the same [`crate::cast`]
//! resolver (which auto-branches a stroke into radial-vs-glyph). Sector 0 = North (up), clockwise.

use crate::glyph::C;
use serde::{Deserialize, Serialize};
use std::f64::consts::TAU;

/// One wedge of the wheel.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct RadialItem {
    pub label: String,
    /// free-form action for now (a shell command); wired into dispatch later.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// A configured wheel.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RadialMenu {
    /// number of sectors N (8/10/12 typical).
    pub sectors: usize,
    /// minimum net flick distance (in accumulated mouse units) to count as a pick; below it
    /// the release is a cancel (you can bail by not moving).
    #[serde(default = "default_deadzone")]
    pub deadzone: f64,
    /// per-sector items; if shorter than `sectors`, missing wedges fall back to a bearing name.
    #[serde(default)]
    pub items: Vec<RadialItem>,
}

fn default_deadzone() -> f64 {
    40.0
}

impl Default for RadialMenu {
    fn default() -> Self {
        // an 8-way compass wheel out of the box
        let items = (0..8)
            .map(|i| RadialItem {
                label: compass(i, 8),
                action: None,
            })
            .collect();
        RadialMenu {
            sectors: 8,
            deadzone: default_deadzone(),
            items,
        }
    }
}

impl RadialMenu {
    /// Label for a sector: configured item, else its bearing name.
    #[must_use]
    pub fn label(&self, sector: usize) -> String {
        self.items
            .get(sector)
            .filter(|it| !it.label.is_empty()).map_or_else(|| compass(sector, self.sectors), |it| it.label.clone())
    }

    /// Resolve a captured flick to a sector, or None if it was under the deadzone (cancel).
    #[must_use]
    pub fn select(&self, path: &[C]) -> Option<usize> {
        let (dx, dy) = net_displacement(path);
        if (dx * dx + dy * dy).sqrt() < self.deadzone {
            return None;
        }
        Some(sector_for(dx, dy, self.sectors))
    }
}

/// Net displacement of a captured path (last − first). The path is cumulative positions
/// starting near the origin, so this is the cursor offset at release.
#[must_use]
pub fn net_displacement(path: &[C]) -> (f64, f64) {
    match (path.first(), path.last()) {
        (Some(a), Some(b)) => (b.re - a.re, b.im - a.im),
        _ => (0.0, 0.0),
    }
}

/// Bucket a screen-space direction (dx right+, dy down+) into a sector index.
/// Sector 0 = North (up), increasing clockwise. N must be >= 1.
#[must_use]
pub fn sector_for(dx: f64, dy: f64, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    // atan2(dx, -dy): up→0, right→+π/2, down→±π, left→−π/2; clockwise-positive from North.
    let ang = dx.atan2(-dy).rem_euclid(TAU);
    let step = TAU / n as f64;
    ((ang / step).round() as usize) % n
}

// ── PROMPT geometry: the ask/choose answer wheel, the radial core specialized for ANSWERING ──────
// A macro prompt (yes/no, an N-way choose, a single confirm) is ONE engine: N options at bearings,
// flick to commit one, no-commit = pass. It differs from the comms-wheel only in that ANSWERING needs
// a deliberate PASS — so options anchor at WEST and own a capped arc, leaving gaps for pass. At N=2
// that cap (90°) leaves the whole vertical axis as pass: byte-for-byte the legacy west=yes / east=no /
// vertical=pass rule. At N≥4 the arcs tile the circle and pass narrows to the deadzone. ONE rule; the
// simple and the rich both fall out, and `max_sectors` still bounds how many wedges stay honest.

/// The screen-space bearing (radians, `atan2(dy, dx)` with dy DOWN-positive: east=0, south=+π/2,
/// west=±π, north=−π/2) of prompt option `i` of `n`. Option 0 sits at WEST; the rest step clockwise.
#[must_use]
pub fn wedge_bearing(i: usize, n: usize) -> f64 {
    std::f64::consts::PI - (i as f64) * (TAU / n.max(1) as f64)
}

/// A prompt wedge's arc width (radians) — capped at 90° so small wheels keep deliberate pass gaps,
/// shrinking to tile the circle as `n` grows. A SINGLE option (a confirm) owns the WHOLE circle, so
/// any committed flick confirms it — a release without a flick still passes via the deadzone.
#[must_use]
pub fn wedge_arc(n: usize) -> f64 {
    if n <= 1 {
        return TAU;
    }
    (TAU / n as f64).min(std::f64::consts::FRAC_PI_2)
}

/// Resolve a prompt flick `(dx, dy)` to a chosen option index, or `None` = PASS (under the deadzone,
/// or in a gap between wedges → the macro's default). The single source of truth both the beacon
/// verdict and the overlay highlight read, so they can never disagree. Pure + testable.
#[must_use]
pub fn pick_wedge(dx: f64, dy: f64, deadzone: f64, n: usize) -> Option<usize> {
    if n == 0 || (dx * dx + dy * dy).sqrt() < deadzone {
        return None;
    }
    let ang = dy.atan2(dx);
    let half = wedge_arc(n) / 2.0;
    (0..n).find(|&i| ang_dist(ang, wedge_bearing(i, n)) <= half)
}

/// Smallest absolute angle (radians, in `[0, π]`) between two bearings.
fn ang_dist(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(TAU);
    d.min(TAU - d)
}

/// The stroke's INTENT direction — attention-weighted over the WHOLE path, recency dominant.
/// Each displacement increment is weighted by `exp(-s/τ)` where `s` is its arc distance from
/// the stroke's END and `τ` is 30% of the total arc: the latest motion speaks loudest, the
/// history still votes (the Logos-attention idea, arc-length-normalized so it's speed- and
/// scale-invariant). A change of mind (left… no, RIGHT), a circling approach, a wandering
/// start — all resolve to where the hand MEANT to end, not to geometric purity.
#[must_use]
pub fn intent_vector(path: &[C]) -> (f64, f64) {
    if path.len() < 2 {
        return (0.0, 0.0);
    }
    let total: f64 = path
        .windows(2)
        .map(|w| {
            let (dx, dy) = (w[1].re - w[0].re, w[1].im - w[0].im);
            (dx * dx + dy * dy).sqrt()
        })
        .sum();
    if total <= 1e-9 {
        return (0.0, 0.0);
    }
    let tau = (total * 0.30).max(1e-9);
    let mut s = 0.0; // arc distance from the END
    let (mut vx, mut vy) = (0.0, 0.0);
    for w in path.windows(2).rev() {
        let (dx, dy) = (w[1].re - w[0].re, w[1].im - w[0].im);
        let seg = (dx * dx + dy * dy).sqrt();
        let wgt = (-(s + seg * 0.5) / tau).exp();
        vx += dx * wgt;
        vy += dy * wgt;
        s += seg;
    }
    (vx, vy)
}

/// Lateral hand jitter of a committed flick, in counts at the deadzone radius. Empirically a
/// quick mouse flick lands within roughly ±7–8 counts of lateral noise by the time it clears a
/// ~40-count deadzone — the constant the sector cap is DERIVED from, not a designer's whim.
pub const FLICK_JITTER: f64 = 15.0;

/// The maximum reliable sector count for a given deadzone radius — COMPUTED, not capped by hand.
/// A wedge is only pickable if its arc at the commit radius exceeds the hand's lateral jitter:
/// `arc = 2π·deadzone / n ≥ FLICK_JITTER` → `n ≤ 2π·deadzone / FLICK_JITTER`. The default
/// deadzone (40) yields 16; raise the deadzone (longer, more deliberate flicks) and the wheel can
/// honestly carry more wedges. Floor of 4 (below that the formula is moot, a wheel needs quadrants).
#[must_use]
pub fn max_sectors(deadzone: f64) -> usize {
    ((TAU * deadzone.max(1.0)) / FLICK_JITTER).floor().max(4.0) as usize
}

/// A bearing name for a sector. Uses 8-wind compass names when they line up (n divides 8),
/// otherwise the wedge's centre bearing in degrees.
#[must_use]
pub fn compass(sector: usize, n: usize) -> String {
    const W8: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    if n != 0 && 8 % n == 0 {
        let stride = 8 / n;
        return W8[(sector * stride) % 8].to_string();
    }
    if n == 16 {
        const W16: [&str; 16] = [
            "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
            "NW", "NNW",
        ];
        return W16[sector % 16].to_string();
    }
    let deg = (sector as f64 * 360.0 / n.max(1) as f64).round() as i32;
    format!("{deg}\u{00b0}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(re: f64, im: f64) -> C {
        C { re, im }
    }

    #[test]
    fn max_sectors_is_derived_from_the_deadzone() {
        // the default deadzone (40) carries exactly 16 reliable wedges: 2π·40/15 = 16.75 → 16.
        assert_eq!(max_sectors(40.0), 16);
        // a longer commit radius honestly carries more; a shorter one fewer — never below 4.
        assert!(max_sectors(80.0) > max_sectors(40.0));
        assert_eq!(
            max_sectors(0.0),
            4,
            "degenerate deadzone floors at quadrants"
        );
        // monotonic in the deadzone (no weird cliffs a slider would trip over).
        let mut prev = 0;
        for dz in [10.0, 20.0, 40.0, 60.0, 100.0, 200.0] {
            let m = max_sectors(dz);
            assert!(m >= prev, "monotonic: {dz} -> {m} (prev {prev})");
            prev = m;
        }
    }

    /// The INTENT vector reads where the hand MEANT to end — recency dominates, history votes.
    #[test]
    fn intent_survives_a_changed_mind() {
        // go far LEFT… no wait, RIGHT: end far right. Net is rightward, intent must be too —
        // and so must the wedge, even though the stroke is anything but a pure flick.
        let mut pts: Vec<C> = (0..40)
            .map(|i| C {
                re: -f64::from(i) * 6.0,
                im: 0.0,
            })
            .collect();
        pts.extend((0..80).map(|i| C {
            re: -240.0 + f64::from(i) * 6.0,
            im: 0.0,
        }));
        let (ix, iy) = intent_vector(&pts);
        assert!(
            ix > 0.0,
            "the second thought wins: intent points right (ix {ix})"
        );
        assert_eq!(sector_for(ix, iy, 8), 2, "east wedge");
        // a circling approach that EXITS upward reads as north
        let mut circ: Vec<C> = (0..60)
            .map(|i| {
                let a = f64::from(i) / 60.0 * std::f64::consts::TAU;
                C {
                    re: 60.0 * a.sin(),
                    im: 60.0 * (1.0 - a.cos()),
                }
            })
            .collect();
        circ.extend((0..50).map(|i| C {
            re: 0.0,
            im: -f64::from(i) * 5.0,
        }));
        let (ix, iy) = intent_vector(&circ);
        assert!(
            iy < 0.0,
            "the exit flick dominates: intent points up (iy {iy})"
        );
        assert_eq!(sector_for(ix, iy, 8), 0, "north wedge");
        // a clean flick still reads exactly as itself
        let flick: Vec<C> = (0..30)
            .map(|i| C {
                re: f64::from(i) * 5.0,
                im: 0.0,
            })
            .collect();
        let (ix, iy) = intent_vector(&flick);
        assert_eq!(sector_for(ix, iy, 8), 2);
        // degenerate inputs stay quiet
        assert_eq!(intent_vector(&[]), (0.0, 0.0));
        assert_eq!(intent_vector(&[C { re: 1.0, im: 1.0 }]), (0.0, 0.0));
    }

    #[test]
    fn eight_way_compass_directions() {
        // dy is DOWN-positive (screen space)
        assert_eq!(sector_for(0.0, -1.0, 8), 0, "up = N");
        assert_eq!(sector_for(1.0, -1.0, 8), 1, "up-right = NE");
        assert_eq!(sector_for(1.0, 0.0, 8), 2, "right = E");
        assert_eq!(sector_for(1.0, 1.0, 8), 3, "down-right = SE");
        assert_eq!(sector_for(0.0, 1.0, 8), 4, "down = S");
        assert_eq!(sector_for(-1.0, 1.0, 8), 5, "down-left = SW");
        assert_eq!(sector_for(-1.0, 0.0, 8), 6, "left = W");
        assert_eq!(sector_for(-1.0, -1.0, 8), 7, "up-left = NW");
    }

    #[test]
    fn wraparound_is_stable_near_north() {
        // a hair clockwise and a hair counter-clockwise of straight up both read as N
        assert_eq!(sector_for(0.05, -1.0, 8), 0);
        assert_eq!(sector_for(-0.05, -1.0, 8), 0);
    }

    #[test]
    fn twelve_way_buckets_evenly() {
        // 12 sectors, 30° each; due-east (90°) = sector 3
        assert_eq!(sector_for(1.0, 0.0, 12), 3);
        assert_eq!(sector_for(0.0, -1.0, 12), 0);
        // every direction maps into range
        for deg in (0..360).step_by(7) {
            let r = f64::from(deg).to_radians();
            // bearing->screen: dx=sin, dy=-cos
            let s = sector_for(r.sin(), -r.cos(), 12);
            assert!(s < 12);
        }
    }

    #[test]
    fn deadzone_cancels_small_flicks() {
        let m = RadialMenu {
            sectors: 8,
            deadzone: 40.0,
            items: vec![],
        };
        let tiny = vec![c(0.0, 0.0), c(5.0, -3.0)]; // ~5.8 units < 40
        assert_eq!(m.select(&tiny), None, "small flick = cancel");
        let big = vec![c(0.0, 0.0), c(0.0, -100.0)]; // 100 up
        assert_eq!(m.select(&big), Some(0), "big up flick = N");
    }

    #[test]
    fn labels_fall_back_to_bearing() {
        let mut m = RadialMenu {
            sectors: 8,
            deadzone: 40.0,
            items: vec![],
        };
        assert_eq!(m.label(2), "E"); // no items -> compass
        m.items = vec![RadialItem {
            label: "Push".into(),
            action: None,
        }];
        assert_eq!(m.label(0), "Push"); // configured
        assert_eq!(m.label(2), "E"); // beyond items -> compass
    }

    #[test]
    fn net_displacement_is_last_minus_first() {
        let p = vec![c(10.0, 10.0), c(13.0, 7.0), c(40.0, -20.0)];
        let (dx, dy) = net_displacement(&p);
        assert_eq!((dx, dy), (30.0, -30.0));
    }

    #[test]
    fn default_menu_is_eight_compass() {
        let m = RadialMenu::default();
        assert_eq!(m.sectors, 8);
        assert_eq!(m.label(0), "N");
        assert_eq!(m.label(4), "S");
    }

    #[test]
    fn prompt_wheel_is_exactly_yes_no_pass_at_n2() {
        // THE emergence proof: the N=2 prompt wheel reproduces the legacy ask rule byte-for-byte —
        // west=yes(0), east=no(1), the whole vertical axis = pass, under-deadzone = pass.
        let dz = 40.0;
        assert_eq!(pick_wedge(-100.0, 0.0, dz, 2), Some(0), "west = yes");
        assert_eq!(pick_wedge(100.0, 0.0, dz, 2), Some(1), "east = no");
        assert_eq!(pick_wedge(0.0, -100.0, dz, 2), None, "up = pass");
        assert_eq!(pick_wedge(0.0, 100.0, dz, 2), None, "down = pass");
        assert_eq!(pick_wedge(6.0, -2.0, dz, 2), None, "under deadzone = pass");
        // the legacy quadrant rule was |dx|>|dy| → commit, else pass — the 90° cap IS that rule:
        assert_eq!(pick_wedge(-100.0, -60.0, dz, 2), Some(0), "horizontal-dominant WNW = yes");
        assert_eq!(pick_wedge(-60.0, -100.0, dz, 2), None, "vertical-dominant NNW = pass");
    }

    #[test]
    fn prompt_wheel_fans_out_for_more_options() {
        let dz = 40.0;
        // N=4: west, south, east, north — clockwise from west.
        assert_eq!(pick_wedge(-100.0, 0.0, dz, 4), Some(0), "west");
        assert_eq!(pick_wedge(0.0, 100.0, dz, 4), Some(1), "south");
        assert_eq!(pick_wedge(100.0, 0.0, dz, 4), Some(2), "east");
        assert_eq!(pick_wedge(0.0, -100.0, dz, 4), Some(3), "north");
        // every wedge centre commits to a distinct option, for any clean N (the wheel stays honest).
        for n in [2usize, 3, 4, 5, 6, 8] {
            let hits: std::collections::HashSet<_> = (0..n)
                .map(|i| {
                    let b = wedge_bearing(i, n);
                    pick_wedge(b.cos() * 100.0, b.sin() * 100.0, dz, n)
                })
                .collect();
            assert_eq!(hits.len(), n, "N={n}: each centre picks a distinct option");
            assert!(hits.iter().all(Option::is_some), "N={n}: every centre commits");
        }
    }

    #[test]
    fn single_option_confirms_on_any_committed_flick() {
        // a 1-option prompt (confirm) owns the whole circle: any committed direction picks it, while a
        // release under the deadzone still passes. No arbitrary "flick west to confirm".
        let dz = 40.0;
        for (dx, dy) in [
            (-100.0, 0.0),
            (100.0, 0.0),
            (0.0, -100.0),
            (0.0, 100.0),
            (70.0, 70.0),
        ] {
            assert_eq!(pick_wedge(dx, dy, dz, 1), Some(0), "any committed flick confirms");
        }
        assert_eq!(pick_wedge(6.0, -2.0, dz, 1), None, "under-deadzone = pass");
    }
}
