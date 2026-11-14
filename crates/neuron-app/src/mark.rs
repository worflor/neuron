// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! neuron's mark, drawn in code so the window, the tray and the executable share one source: a
//! lowercase `n.` in one stroke. The stroke is the trigger travelling, a wisp that warms from blue
//! through violet and pink to orange; the full stop is the action it lands on, a lit ember
//! warming the ground around it. Small sizes draw it heavier and plainer; the spark shows from 48 px. `build.rs` includes this file to write the
//! executable's `.ico`, so it uses only `std`.

/// The stroke's colours along its length, start to finish.
const RAMP: [(f32, [f32; 3]); 5] = [
    (0.0, [79.0, 140.0, 242.0]),
    (0.3, [150.0, 96.0, 240.0]),
    (0.6, [236.0, 84.0, 150.0]),
    (0.85, [255.0, 112.0, 70.0]),
    (1.0, [255.0, 150.0, 64.0]),
];
const EMBER: [f32; 3] = [255.0, 160.0, 70.0];
const CORE: [f32; 3] = [255.0, 236.0, 205.0];
const GROUND_TOP: [f32; 3] = [52.0, 26.0, 58.0];
const GROUND_FOOT: [f32; 3] = [20.0, 10.0, 26.0];

fn ramp(t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    for pair in RAMP.windows(2) {
        let ((a, ca), (b, cb)) = (pair[0], pair[1]);
        if t <= b {
            return lerp(ca, cb, (t - a) / (b - a));
        }
    }
    RAMP[RAMP.len() - 1].1
}

/// Signed distance to a capsule from `a` to `b` of radius `r`.
fn capsule(p: (f32, f32), a: (f32, f32), b: (f32, f32), r: f32) -> f32 {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    (pa.0 - ba.0 * h).hypot(pa.1 - ba.1 * h) - r
}

fn circle(p: (f32, f32), c: (f32, f32), r: f32) -> f32 {
    (p.0 - c.0).hypot(p.1 - c.1) - r
}

/// Signed distance to a rounded square inset `m` from the unit square, corner radius `r`.
fn tile(p: (f32, f32), m: f32, r: f32) -> f32 {
    let half = 0.5 - m;
    let q = ((p.0 - 0.5).abs() - half + r, (p.1 - 0.5).abs() - half + r);
    q.0.max(0.0).hypot(q.1.max(0.0)) + q.0.max(q.1).min(0.0) - r
}

/// Signed distance to the upper half of a ring centred on `c`, radius `r`, half-width `w`.
fn arch(p: (f32, f32), c: (f32, f32), r: f32, w: f32) -> f32 {
    if p.1 <= c.1 {
        circle(p, c, r).abs() - w
    } else {
        circle(p, (c.0 - r, c.1), w).min(circle(p, (c.0 + r, c.1), w))
    }
}

fn lerp(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

/// Signed distance to a segment from `a` (radius `ra`) to `b` (radius `rb`), and how far along it.
fn taper(p: (f32, f32), a: (f32, f32), b: (f32, f32), ra: f32, rb: f32) -> (f32, f32) {
    let (pa, ba) = ((p.0 - a.0, p.1 - a.1), (b.0 - a.0, b.1 - a.1));
    let h = ((pa.0 * ba.0 + pa.1 * ba.1) / (ba.0 * ba.0 + ba.1 * ba.1)).clamp(0.0, 1.0);
    ((pa.0 - ba.0 * h).hypot(pa.1 - ba.1 * h) - (ra + (rb - ra) * h), h)
}

/// The colour (premultiplied, alpha) at unit-square point `p`.
fn sample(p: (f32, f32), size: u32) -> ([f32; 3], f32) {
    let small = size < 32;
    let edge = tile(p, 0.03, 0.22);
    if edge > 0.0 {
        return ([0.0; 3], 0.0);
    }
    let mut c = lerp(GROUND_TOP, GROUND_FOOT, p.1);
    // a faint rim, so the tile holds its edge on a dark taskbar
    if !small && edge > -0.012 {
        c = lerp(c, ramp(0.6), 0.18);
    }
    // the mark's own frame, shifted so `n.` sits optically centred on the tile
    let q = (p.0 + 0.04, p.1 + 0.02);
    let (w, r) = if small { (0.08, 0.085) } else { (0.065, 0.075) };
    let (c_arch, r_arch) = ((0.48, 0.48), 0.20);
    let dot = (0.80, 0.76);
    // the leg rises from a thin tail, like a wisp catching
    let (leg, up) = taper(q, (0.28, 0.80), (0.28, 0.48), if small { w } else { w * 0.35 }, w);
    let bow = arch(q, c_arch, r_arch, w);
    let down = capsule(q, (0.68, 0.48), (0.68, 0.60), w);
    let stop = circle(q, dot, r);
    if !small {
        // the ember is lit: a bloom that hugs it, and the ground warmed where it would fall
        let bloom = (-(stop.max(0.0) / 0.022).powi(2)).exp() * 0.3;
        let fall = (-(((q.0 - dot.0) / 0.2).powi(2) + ((q.1 - dot.1 - 0.02) / 0.12).powi(2))).exp() * 0.12;
        c = lerp(c, EMBER, bloom.max(fall));
        let spark = circle(q, (0.905, 0.6), 0.017).min(circle(q, (0.87, 0.53), 0.01));
        if size >= 48 && spark <= 0.0 {
            c = EMBER;
        }
    }
    if stop <= 0.0 {
        let core = (1.0 - (q.0 - dot.0).hypot(q.1 - dot.1) / r).clamp(0.0, 1.0);
        c = lerp(EMBER, CORE, core.powf(0.7));
    } else if leg.min(bow).min(down) <= 0.0 {
        // how far along the stroke: up the leg, over the arch, down to the stop
        let t = if leg <= 0.0 && q.1 > c_arch.1 {
            0.25 * up
        } else if q.1 <= c_arch.1 {
            let a = (c_arch.1 - q.1).atan2(q.0 - c_arch.0); // pi on the left, 0 on the right
            0.25 + 0.6 * (1.0 - a / std::f32::consts::PI)
        } else {
            0.85 + 0.15 * (q.1 - c_arch.1) / 0.12
        };
        c = ramp(t);
    }
    (c, 1.0)
}

/// The mark as straight (not premultiplied) RGBA, `size` x `size`, rows top to bottom.
pub fn rgba(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (mut acc, mut alpha) = ([0.0f32; 3], 0.0);
            for sy in 0..SS {
                for sx in 0..SS {
                    let p = ((x as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32, (y as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32);
                    let (c, a) = sample(p, size);
                    for i in 0..3 { acc[i] += c[i] * a; }
                    alpha += a;
                }
            }
            let n = (SS * SS) as f32;
            out.extend(acc.map(|v| if alpha > 0.0 { (v / alpha).round() as u8 } else { 0 }));
            out.push((alpha / n * 255.0).round() as u8);
        }
    }
    out
}

/// An `.ico` holding the mark at each of `sizes` (each at most 256), as 32-bit BMP entries.
#[allow(dead_code)] // build.rs writes the executable's icon with it; the app only tests it
pub fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| {
        let px = rgba(s);
        let mut bmp = Vec::new();
        for v in [40u32, s, s * 2] { bmp.extend(v.to_le_bytes()); }
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(32u16.to_le_bytes());
        bmp.extend([0u8; 24]);
        for y in (0..s).rev() {
            for x in 0..s {
                let i = ((y * s + x) * 4) as usize;
                bmp.extend([px[i + 2], px[i + 1], px[i], px[i + 3]]);
            }
        }
        // the 1-bit mask, all clear: the alpha channel decides
        let mask_row = (s.div_ceil(32) * 4) as usize;
        bmp.extend(std::iter::repeat_n(0u8, mask_row * s as usize));
        bmp
    }).collect();
    let mut out = Vec::new();
    out.extend([0u16, 1, sizes.len() as u16].iter().flat_map(|v| v.to_le_bytes()));
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let side = if s >= 256 { 0 } else { s as u8 };
        out.extend([side, side, 0, 0]);
        out.extend(1u16.to_le_bytes());
        out.extend(32u16.to_le_bytes());
        out.extend((img.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images { out.extend(img); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_has_ground_and_ink_and_clear_corners() {
        let px = rgba(32);
        let at = |x: usize, y: usize| &px[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "the corner is clear");
        let ground = at(4, 16);
        assert!(ground[3] == 255 && ground[1] < 40, "the tile is dark ground: {ground:?}");
        let stop = at(24, 23);
        assert!(stop[3] == 255 && stop[0] > 240 && stop[1] > 180, "the full stop is the warm, bright ember: {stop:?}");
        let start = at(8, 22);
        assert!(start[2] > start[0], "the stroke starts cool: {start:?}");
    }

    #[test]
    fn the_ico_lists_every_size_and_points_inside_itself() {
        let sizes = [16, 32, 256];
        let ico = ico(&sizes);
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 3, 0]);
        for (k, &s) in sizes.iter().enumerate() {
            let e = &ico[6 + 16 * k..22 + 16 * k];
            assert_eq!(e[0], if s == 256 { 0 } else { s as u8 });
            let len = u32::from_le_bytes(e[8..12].try_into().unwrap()) as usize;
            let off = u32::from_le_bytes(e[12..16].try_into().unwrap()) as usize;
            assert_eq!(len, 40 + (s * s * 4) as usize + (s.div_ceil(32) * 4 * s) as usize);
            assert!(off + len <= ico.len());
            assert_eq!(u32::from_le_bytes(ico[off..off + 4].try_into().unwrap()), 40);
        }
    }
}
