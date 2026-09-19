// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The visual language of KNOCKBACK — and the template for Neuron's magic at large: **hard
//! light**. Symmetra's school of sorcery: nothing is a soft puff of smoke; everything is a
//! *constructed* thing made of light — faceted, crystalline, edge-lit, with a prismatic
//! refraction fringe where the light bends. Order out of the void.
//!
//! This module is pure, platform-neutral vector geometry. A [`Scene`] is a list of
//! [`Prim`]itives; it serializes to a standalone **SVG** (export, golden tests, the panel) and
//! the `neuron-app` overlay rasterizes the same vocabulary into the live layered window. One
//! source of truth, two surfaces — and the same hard-light grammar the spell overlay uses.
//!
//! ## The grammar
//! - A **beat** is a [`Prim::Construct`] — a faceted polygon ring of hard light. It is struck
//!   bright and *settles* (dims) but never blurs: hard light has crisp edges always.
//! - The **flourish** is a richer construct (more facets, brighter refraction) — the "new" in
//!   familiar-but-new.
//! - The **gap you answer** is a [`Prim::Blueprint`] — an *unbuilt* wireframe construct, the
//!   thing waiting to be materialized. The single most communicative mark in the game.
//! - The phrase is strung on a [`Prim::Beam`] — a hard-light filament with lit nodes.
//! - The **weave** is a row of interlocking [`Prim::Shard`]s; the **sigil** is a closed
//!   prismatic [`Prim::Path`] pinned at crystalline nodes.
//!
//! Player constructs are phosphor; the twin's are cold moonlight-violet — the one sanctioned
//! second hue, so *that's my rhythm… but it answered* reads in a glance.

use crate::twin::{Emergent, Judgment, Knockback, TwinTurn};
use std::fmt::Write as _;

// ── palette (the void, the player, the twin, the refraction split) ───────────

/// The deep near-black ground the constructs float in.
pub const VOID: Rgb = Rgb {
    r: 0x04,
    g: 0x05,
    b: 0x06,
};
/// Phosphor green — the player's hard light.
pub const PHOSPHOR: Rgb = Rgb {
    r: 0x4a,
    g: 0xf2,
    b: 0xb0,
};
/// Cold moonlight violet — the twin's hard light.
pub const TWIN: Rgb = Rgb {
    r: 0xb8,
    g: 0xa6,
    b: 0xff,
};
/// Storm amber — earned heat.
pub const STORM: Rgb = Rgb {
    r: 0xf2,
    g: 0xb3,
    b: 0x4a,
};
/// A hush — the stillpoint's held breath.
pub const HUSH: Rgb = Rgb {
    r: 0x9a,
    g: 0xe6,
    b: 0xd6,
};
/// The cool side of the prismatic refraction fringe.
pub const REFRACT_COOL: Rgb = Rgb {
    r: 0x5c,
    g: 0xf0,
    b: 0xff,
};
/// The warm side of the prismatic refraction fringe.
pub const REFRACT_WARM: Rgb = Rgb {
    r: 0xff,
    g: 0x6c,
    b: 0xd6,
};

/// A simple 8-bit RGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Rgb {
        Rgb { r, g, b }
    }
    /// `#rrggbb` for SVG.
    #[must_use]
    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
    /// Linear blend toward `other` by `t` in `[0,1]`.
    #[must_use]
    pub fn lerp(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let f = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb {
            r: f(self.r, other.r),
            g: f(self.g, other.g),
            b: f(self.b, other.b),
        }
    }
    /// Shift hue around the colour wheel by `deg` degrees (the weave's earned palette depth and
    /// per-voice tint). Cheap HSV round-trip.
    #[must_use]
    pub fn rotate_hue(self, deg: f32) -> Rgb {
        let (h, s, v) = rgb_to_hsv(self);
        hsv_to_rgb((h + deg).rem_euclid(360.0), s, v)
    }
}

fn rgb_to_hsv(c: Rgb) -> (f32, f32, f32) {
    let (r, g, b) = (f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d == 0.0 {
        0.0
    } else if max == r {
        60.0 * (((g - b) / d) % 6.0)
    } else if max == g {
        60.0 * (((b - r) / d) + 2.0)
    } else {
        60.0 * (((r - g) / d) + 4.0)
    };
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h.rem_euclid(360.0), s, max)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> Rgb {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match (h / 60.0) as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Rgb {
        r: ((r + m) * 255.0).round() as u8,
        g: ((g + m) * 255.0).round() as u8,
        b: ((b + m) * 255.0).round() as u8,
    }
}

/// A regular polygon's vertices — the skeleton of every hard-light construct.
fn ngon(cx: f32, cy: f32, r: f32, sides: u32, rot: f32) -> Vec<(f32, f32)> {
    let n = sides.max(3);
    (0..n)
        .map(|i| {
            let a = rot + std::f32::consts::TAU * i as f32 / n as f32;
            (cx + a.cos() * r, cy + a.sin() * r)
        })
        .collect()
}

fn poly_d(pts: &[(f32, f32)], close: bool) -> String {
    let mut d = String::new();
    for (i, (x, y)) in pts.iter().enumerate() {
        let _ = write!(d, "{}{:.1} {:.1} ", if i == 0 { "M" } else { "L" }, x, y);
    }
    if close {
        d.push('Z');
    }
    d
}

// ── primitives (the hard-light vocabulary) ───────────────────────────────────

/// One drawable element. Coordinates are scene pixels.
#[derive(Clone, Debug)]
pub enum Prim {
    /// A built hard-light construct: a faceted polygon ring, edge-lit with a prismatic
    /// refraction fringe and a lattice interior. `intensity` (0..1) is how bright it burns;
    /// `settle` (0..1) dims it as it ages — but the edges stay crisp (hard light never blurs).
    Construct {
        cx: f32,
        cy: f32,
        radius: f32,
        sides: u32,
        rot: f32,
        color: Rgb,
        intensity: f32,
        settle: f32,
    },
    /// An *unbuilt* construct — a thin wireframe blueprint with a centre tick. The open beat
    /// you are invited to materialize: the unfinished line of the duet.
    Blueprint {
        cx: f32,
        cy: f32,
        radius: f32,
        sides: u32,
        rot: f32,
        color: Rgb,
    },
    /// A hard-light filament: a crisp edge-lit beam through `points`, with lit nodes. The phrase
    /// strung in light; also any connective tether.
    Beam {
        points: Vec<(f32, f32)>,
        color: Rgb,
        intensity: f32,
    },
    /// A small crystal shard (a triangle/diamond of hard light) — a weave strand, a node, a
    /// spark. `size` is its radius, `rot` its facing.
    Shard {
        cx: f32,
        cy: f32,
        size: f32,
        rot: f32,
        color: Rgb,
        intensity: f32,
    },
    /// A soft ambient bloom — the only *non*-hard element, used sparingly for mood wash behind
    /// the constructs (a storm's heat, a stillpoint's hush).
    Glow {
        cx: f32,
        cy: f32,
        radius: f32,
        color: Rgb,
        strength: f32,
    },
    /// A prismatic vector path — a drawn stroke or the sigil. Refraction-fringed; `closed`
    /// fills faintly; `nodes` pins crystalline shards at its vertices.
    Path {
        points: Vec<(f32, f32)>,
        width: f32,
        color: Rgb,
        closed: bool,
        nodes: bool,
    },
    /// Quiet monospace readout — the machine shown honestly.
    Text {
        x: f32,
        y: f32,
        s: String,
        color: Rgb,
        size: f32,
        anchor: Anchor,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    Start,
    Middle,
    End,
}

/// A complete frame: the void plus a stack of hard-light primitives.
#[derive(Clone, Debug)]
pub struct Scene {
    pub w: f32,
    pub h: f32,
    pub bg: Rgb,
    pub prims: Vec<Prim>,
}

impl Scene {
    #[must_use]
    pub fn new(w: f32, h: f32) -> Scene {
        Scene {
            w,
            h,
            bg: VOID,
            prims: Vec::new(),
        }
    }
    pub fn push(&mut self, p: Prim) {
        self.prims.push(p);
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.prims.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prims.is_empty()
    }

    /// Serialize to a standalone SVG. A tight bloom (hard light has a *thin* halo, not a soft
    /// cloud) plus additive prismatic edges = the refraction look. Deterministic.
    #[must_use]
    pub fn to_svg(&self) -> String {
        let mut s = String::with_capacity(8192);
        let _ = write!(
            s,
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">"#,
            w = self.w as i32,
            h = self.h as i32
        );
        // a crisp, tight bloom — the edge glows but does not smear.
        s.push_str(r#"<defs><filter id="hl" x="-30%" y="-30%" width="160%" height="160%">"#);
        s.push_str(r#"<feGaussianBlur stdDeviation="1.4" result="b"/>"#);
        s.push_str(
            r#"<feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>"#,
        );
        // a softer one only for ambient Glow washes.
        s.push_str(r#"<filter id="soft" x="-60%" y="-60%" width="220%" height="220%"><feGaussianBlur stdDeviation="6"/></filter></defs>"#);
        let _ = write!(
            s,
            r#"<rect x="0" y="0" width="{}" height="{}" fill="{}"/>"#,
            self.w as i32,
            self.h as i32,
            self.bg.hex()
        );
        // everything additive: overlapping translucent light sums toward white, and the
        // offset cyan/magenta edges fringe — exactly the hard-light refraction read.
        s.push_str(r#"<g style="mix-blend-mode:screen">"#);
        for (i, p) in self.prims.iter().enumerate() {
            self.svg_prim(&mut s, p, i);
        }
        s.push_str("</g></svg>");
        s
    }

    fn svg_prim(&self, s: &mut String, p: &Prim, idx: usize) {
        match p {
            Prim::Construct {
                cx,
                cy,
                radius,
                sides,
                rot,
                color,
                intensity,
                settle,
            } => {
                let eff = (intensity * (1.0 - 0.45 * settle)).clamp(0.0, 1.0);
                let pts = ngon(*cx, *cy, *radius, *sides, *rot);
                let d = poly_d(&pts, true);
                // translucent inner pane
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="{c}" fill-opacity="{fo:.3}" stroke="none"/>"#,
                    c = color.hex(),
                    fo = 0.06 * eff,
                );
                // lattice: a concentric inner polygon + spokes (the constructed interior)
                let inner = ngon(*cx, *cy, radius * 0.52, *sides, *rot);
                let _ = write!(
                    s,
                    r#"<path d="{}" fill="none" stroke="{c}" stroke-width="0.8" stroke-opacity="{o:.3}"/>"#,
                    poly_d(&inner, true),
                    c = color.hex(),
                    o = 0.22 * eff,
                );
                let mut spokes = String::new();
                for v in &pts {
                    let _ = write!(spokes, "M{cx:.1} {cy:.1} L{:.1} {:.1} ", v.0, v.1);
                }
                let _ = write!(
                    s,
                    r#"<path d="{spokes}" stroke="{c}" stroke-width="0.6" stroke-opacity="{o:.3}"/>"#,
                    c = color.hex(),
                    o = 0.14 * eff,
                );
                // the prismatic edge: cyan + base + magenta, offset → refraction fringe
                prismatic_edge(s, &d, *color, eff, 1.6);
                // crystalline nodes at each vertex
                for v in &pts {
                    let _ = write!(
                        s,
                        r##"<circle cx="{:.1}" cy="{:.1}" r="1.5" fill="#ffffff" fill-opacity="{o:.3}" filter="url(#hl)"/>"##,
                        v.0,
                        v.1,
                        o = 0.85 * eff,
                    );
                }
            }
            Prim::Blueprint {
                cx,
                cy,
                radius,
                sides,
                rot,
                color,
            } => {
                let pts = ngon(*cx, *cy, *radius, *sides, *rot);
                // a thin dashed wireframe — the unbuilt thing, waiting to materialize.
                let _ = write!(
                    s,
                    r#"<path d="{}" fill="none" stroke="{c}" stroke-width="1.3" stroke-opacity="0.85" stroke-dasharray="2.5 5" filter="url(#hl)"/>"#,
                    poly_d(&pts, true),
                    c = color.hex(),
                );
                // faint construction spokes
                let mut spokes = String::new();
                for v in &pts {
                    let _ = write!(spokes, "M{cx:.1} {cy:.1} L{:.1} {:.1} ", v.0, v.1);
                }
                let _ = write!(
                    s,
                    r#"<path d="{spokes}" stroke="{c}" stroke-width="0.5" stroke-opacity="0.25"/>"#,
                    c = color.hex(),
                );
                // the centre tick: your turn
                let _ = write!(
                    s,
                    r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="2.0" fill="{c}" fill-opacity="0.8"/>"#,
                    c = color.hex(),
                );
            }
            Prim::Beam {
                points,
                color,
                intensity,
            } => {
                if points.len() < 2 {
                    return;
                }
                let d = poly_d(points, false);
                let eff = intensity.clamp(0.0, 1.0);
                // a thin outer glow, then a crisp bright core
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="none" stroke="{c}" stroke-width="3.0" stroke-opacity="{o:.3}" stroke-linecap="round" filter="url(#hl)"/>"#,
                    c = color.hex(),
                    o = 0.16 * eff,
                );
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="none" stroke="{c}" stroke-width="1.1" stroke-opacity="{o:.3}" stroke-linecap="round"/>"#,
                    c = color.hex(),
                    o = 0.85 * eff,
                );
                for v in points {
                    let _ = write!(
                        s,
                        r##"<circle cx="{:.1}" cy="{:.1}" r="1.4" fill="#ffffff" fill-opacity="{o:.3}"/>"##,
                        v.0,
                        v.1,
                        o = 0.6 * eff,
                    );
                }
            }
            Prim::Shard {
                cx,
                cy,
                size,
                rot,
                color,
                intensity,
            } => {
                let eff = intensity.clamp(0.0, 1.0);
                let pts = ngon(*cx, *cy, *size, 3, *rot); // a triangular crystal
                let d = poly_d(&pts, true);
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="{c}" fill-opacity="{fo:.3}"/>"#,
                    c = color.hex(),
                    fo = 0.18 * eff,
                );
                prismatic_edge(s, &d, *color, eff, 1.2);
            }
            Prim::Glow {
                cx,
                cy,
                radius,
                color,
                strength,
            } => {
                let gid = format!("gl{idx}");
                let _ = write!(
                    s,
                    r#"<radialGradient id="{gid}"><stop offset="0%" stop-color="{c}" stop-opacity="{st:.3}"/><stop offset="100%" stop-color="{c}" stop-opacity="0"/></radialGradient>"#,
                    c = color.hex(),
                    st = strength.clamp(0.0, 1.0) * 0.5,
                );
                let _ = write!(
                    s,
                    r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="{radius:.1}" fill="url(#{gid})"/>"#,
                );
            }
            Prim::Path {
                points,
                width,
                color,
                closed,
                nodes,
            } => {
                if points.len() < 2 {
                    return;
                }
                let d = poly_d(points, *closed);
                if *closed {
                    let _ = write!(
                        s,
                        r#"<path d="{d}" fill="{c}" fill-opacity="0.05" stroke="none"/>"#,
                        c = color.hex(),
                    );
                }
                prismatic_edge(s, &d, *color, 1.0, *width);
                if *nodes {
                    // pin crystalline shards at a sampling of vertices
                    let step = (points.len() / 12).max(1);
                    for (i, v) in points.iter().enumerate() {
                        if i % step == 0 {
                            let pts = ngon(v.0, v.1, 2.4, 3, i as f32 * 0.7);
                            let _ = write!(
                                s,
                                r##"<path d="{}" fill="#ffffff" fill-opacity="0.5" filter="url(#hl)"/>"##,
                                poly_d(&pts, true),
                            );
                        }
                    }
                }
            }
            Prim::Text {
                x,
                y,
                s: txt,
                color,
                size,
                anchor,
            } => {
                let anc = match anchor {
                    Anchor::Start => "start",
                    Anchor::Middle => "middle",
                    Anchor::End => "end",
                };
                let _ = write!(
                    s,
                    r#"<text x="{x:.1}" y="{y:.1}" fill="{c}" fill-opacity="0.8" font-family="Consolas,monospace" font-size="{size:.1}" letter-spacing="1.5" text-anchor="{anc}">{txt}</text>"#,
                    c = color.hex(),
                    txt = xml_escape(txt),
                );
            }
        }
    }
}

/// Draw a path's edge three times — cool-shifted, base, warm-shifted, each offset a hair — so
/// the additive blend fringes it with refracted colour. The signature hard-light read.
fn prismatic_edge(s: &mut String, d: &str, color: Rgb, eff: f32, width: f32) {
    let off = 1.1;
    let _ = write!(
        s,
        r#"<g transform="translate({:.1} {:.1})"><path d="{d}" fill="none" stroke="{c}" stroke-width="{w:.2}" stroke-opacity="{o:.3}" stroke-linejoin="round"/></g>"#,
        -off,
        -off * 0.5,
        c = REFRACT_COOL.hex(),
        w = width,
        o = 0.5 * eff,
    );
    let _ = write!(
        s,
        r#"<g transform="translate({:.1} {:.1})"><path d="{d}" fill="none" stroke="{c}" stroke-width="{w:.2}" stroke-opacity="{o:.3}" stroke-linejoin="round"/></g>"#,
        off,
        off * 0.5,
        c = REFRACT_WARM.hex(),
        w = width,
        o = 0.5 * eff,
    );
    let _ = write!(
        s,
        r#"<path d="{d}" fill="none" stroke="{c}" stroke-width="{w:.2}" stroke-opacity="{o:.3}" stroke-linejoin="round" filter="url(#hl)"/>"#,
        c = color.hex(),
        w = width + 0.2,
        o = 0.95 * eff,
    );
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ── the storyboard: a turn becomes hard light ────────────────────────────────

pub const SCENE_W: f32 = 1000.0;
pub const SCENE_H: f32 = 420.0;

/// How many facets a beat's construct has, by its strike energy — a soft tap is a simple
/// triangle of light; a hard hit is an elaborate octagon. The hand's force *builds* the shape.
fn facets(energy: f32) -> u32 {
    (3.0 + (energy.clamp(0.0, 1.0) * 5.0)).round() as u32
}

/// Render the twin's reply as a row of hard-light beat-constructs strung on a light-beam, the
/// open blueprint at the end. `frame` in `[0,1]` sweeps the play-head left→right (a sequence of
/// frames *builds* the phrase construct by construct). `mood` washes a faint ambient bloom.
#[must_use]
pub fn knockback_scene(kb: &Knockback, frame: f32, mood: Option<&Emergent>) -> Scene {
    let mut sc = Scene::new(SCENE_W, SCENE_H);
    let baseline = SCENE_H * 0.42;
    let margin = 90.0;
    let usable = SCENE_W - 2.0 * margin;

    if let Some(ev) = mood {
        let (c, st) = match ev {
            Emergent::Storm { .. } => (STORM, 0.5),
            Emergent::Stillpoint { depth } => (HUSH, 0.3 + 0.4 * depth),
            Emergent::Haunting { .. } => (TWIN, 0.18),
        };
        sc.push(Prim::Glow {
            cx: SCENE_W * 0.5,
            cy: baseline,
            radius: SCENE_W * 0.5,
            color: c,
            strength: st,
        });
    }

    let total = kb.duration_ms().max(1) as f32;
    let playhead = frame.clamp(0.0, 1.0);

    // the light-beam the constructs hang on (only as far as the play-head has built).
    let beam: Vec<(f32, f32)> = kb
        .onsets
        .iter()
        .filter(|o| (o.t_ms as f32 / total) <= playhead + 0.001)
        .map(|o| (margin + (o.t_ms as f32 / total) * usable, baseline))
        .collect();
    if beam.len() >= 2 {
        sc.push(Prim::Beam {
            points: beam,
            color: TWIN.lerp(VOID, 0.25),
            intensity: 0.7,
        });
    }

    for (i, o) in kb.onsets.iter().enumerate() {
        let struck_at = o.t_ms as f32 / total;
        if struck_at > playhead + 0.001 {
            continue;
        }
        let x = margin + struck_at * usable;
        let settle = ((playhead - struck_at) * 1.6).clamp(0.0, 0.85);
        let is_flourish = i >= kb.flourish_from;
        let color = if is_flourish {
            TWIN.lerp(HUSH, 0.4)
        } else {
            TWIN
        };
        let radius = if is_flourish { 40.0 } else { 32.0 };
        let sides = facets(o.energy) + u32::from(is_flourish);
        // a gentle per-beat rotation so the lattice reads as built, not stamped
        let rot = o.voice.spin * 0.6 + i as f32 * 0.3;
        sc.push(Prim::Construct {
            cx: x,
            cy: baseline,
            radius,
            sides,
            rot,
            color,
            intensity: (0.5 + 0.5 * o.energy).min(1.0),
            settle,
        });
    }

    if kb.open {
        let last_x = kb
            .onsets
            .last()
            .map_or(margin, |o| margin + (o.t_ms as f32 / total) * usable);
        let gap_x = (last_x + usable * 0.10).min(SCENE_W - margin * 0.4);
        sc.push(Prim::Blueprint {
            cx: gap_x,
            cy: baseline,
            radius: 30.0,
            sides: 6,
            rot: 0.4,
            color: PHOSPHOR,
        });
    }

    sc
}

/// One exchange's contribution to the weave, distilled for its shard.
#[derive(Clone, Copy, Debug)]
pub struct BraidSeg {
    pub hue: f32,
    pub shimmer: f32,
    pub amp: f32,
}

/// Render the living weave — a row of interlocking hard-light shards along the bottom, one per
/// exchange, oldest left. `palette_depth` rotates the hue as storms are survived. The weave is
/// the score, the save file, and the art, crystallized.
#[must_use]
pub fn weave_scene(segs: &[BraidSeg], palette_depth: u32) -> Scene {
    let mut sc = Scene::new(SCENE_W, 120.0);
    if segs.is_empty() {
        return sc;
    }
    let y = 64.0;
    let margin = 40.0;
    let usable = SCENE_W - 2.0 * margin;
    let w = usable / segs.len() as f32;
    let depth_rot = (palette_depth.saturating_sub(1)) as f32 * 14.0;
    // a faint spine beam runs the whole weave
    sc.push(Prim::Beam {
        points: vec![(margin, y), (SCENE_W - margin, y)],
        color: TWIN.lerp(VOID, 0.4),
        intensity: 0.5,
    });
    for (i, seg) in segs.iter().enumerate() {
        let cx = margin + (i as f32 + 0.5) * w;
        let color = TWIN.rotate_hue(seg.hue + depth_rot);
        // alternate the facing so the shards interlock like a braid
        let rot = if i % 2 == 0 {
            0.0
        } else {
            std::f32::consts::PI
        };
        sc.push(Prim::Shard {
            cx,
            cy: y,
            size: 6.0 + 20.0 * seg.amp,
            rot,
            color,
            intensity: (0.5 + 0.5 * seg.shimmer).min(1.0),
        });
    }
    sc
}

/// Render the personal sigil — a normalized `[-1,1]²` path from [`crate::twin::Familiar::sigil_path`]
/// — as a closed prismatic hard-light glyph pinned at crystalline nodes. The proof-of-self.
#[must_use]
pub fn sigil_scene(path: &[(f32, f32)], size: f32) -> Scene {
    let mut sc = Scene::new(size, size);
    if path.len() < 2 {
        return sc;
    }
    let c = size * 0.5;
    let r = size * 0.42;
    let pts: Vec<(f32, f32)> = path.iter().map(|&(x, y)| (c + x * r, c + y * r)).collect();
    sc.push(Prim::Glow {
        cx: c,
        cy: c,
        radius: size * 0.5,
        color: TWIN,
        strength: 0.22,
    });
    sc.push(Prim::Path {
        points: pts,
        width: 1.5,
        color: TWIN,
        closed: true,
        nodes: true,
    });
    sc
}

// ── the live stage, mirrored (preview + golden surface for the overlay's geometry) ──

/// One beat as the live overlay stages it. Stage-relative x/y around the canvas centre;
/// `kind`: 0 player construct, 1 twin construct, 2 twin flourish, 3 blueprint, 4 materializing.
#[derive(Clone, Copy, Debug)]
pub struct StageBeat {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub color: Rgb,
    pub weight: f32,
    pub phase: f32,
    pub kind: u8,
}

/// The overlay stage's canvas size and staff geometry — kept in lockstep with
/// `neuron-app/src/overlay.rs` (`W`/`H` 560, staff half-width 185). This renderer exists so
/// the live stage's layout can be SEEN and golden-tested without spawning a window.
pub const STAGE_SIZE: f32 = 560.0;
pub const STAGE_STAFF_HALF: f32 = 185.0;

/// Render the live session stage exactly as the overlay lays it out: staff beam, the familiar
/// at the staff's head, the beats standing on the beam, the seal-arc around the newest player
/// strike, the weave strip below, and the quiet caption. `seal` < 0 hides the arc.
#[must_use]
pub fn stage_scene(
    beats: &[StageBeat],
    seal: f32,
    wash: Option<(Rgb, f32)>,
    shards: &[BraidSeg],
    hint: &str,
    presence: f32,
) -> Scene {
    let mut sc = Scene::new(STAGE_SIZE, STAGE_SIZE);
    let c = STAGE_SIZE * 0.5;
    let (sl, sr) = (c - STAGE_STAFF_HALF, c + STAGE_STAFF_HALF);

    if let Some((rgb, strength)) = wash {
        sc.push(Prim::Glow {
            cx: c,
            cy: c,
            radius: 250.0,
            color: rgb,
            strength,
        });
    }

    // the staff beam
    sc.push(Prim::Beam {
        points: vec![(sl, c), (sr, c)],
        color: TWIN.lerp(VOID, 0.45),
        intensity: 0.55,
    });

    // the familiar at the staff's head (wireframe below presence 0.45, built above)
    let fx = sl - 30.0;
    if presence < 0.45 {
        sc.push(Prim::Blueprint {
            cx: fx,
            cy: c,
            radius: 13.0,
            sides: 6,
            rot: 0.2,
            color: TWIN,
        });
    } else {
        sc.push(Prim::Construct {
            cx: fx,
            cy: c,
            radius: 13.0,
            sides: 6,
            rot: 0.2,
            color: TWIN,
            intensity: presence.clamp(0.0, 1.0),
            settle: 0.0,
        });
    }

    // the beats
    let mut last_player: Option<(f32, f32, f32)> = None;
    for b in beats {
        let bx = (c + b.x).clamp(sl - 6.0, sr + 20.0);
        let by = c + b.y;
        let sides = (3.0 + b.weight.clamp(0.0, 1.0) * 5.0).round() as u32;
        match b.kind {
            3 => sc.push(Prim::Blueprint {
                cx: bx,
                cy: by,
                radius: b.r,
                sides: 6,
                rot: 0.4,
                color: b.color,
            }),
            4 => {
                // materializing: the built construct plus its expanding commitment ring
                sc.push(Prim::Construct {
                    cx: bx,
                    cy: by,
                    radius: b.r,
                    sides,
                    rot: 0.4,
                    color: b.color,
                    intensity: 1.0,
                    settle: 0.0,
                });
                let rr = b.r * (1.0 + b.phase.clamp(0.0, 1.0) * 1.6);
                sc.push(Prim::Path {
                    points: ngon(bx, by, rr, 24, 0.0),
                    width: 1.0,
                    color: b.color,
                    closed: true,
                    nodes: false,
                });
                last_player = Some((bx, by, b.r));
            }
            k => {
                let extra = u32::from(k == 2);
                sc.push(Prim::Construct {
                    cx: bx,
                    cy: by,
                    radius: b.r,
                    sides: sides + extra,
                    rot: bx * 0.01,
                    color: b.color,
                    intensity: (0.5 + 0.5 * b.weight).min(1.0),
                    settle: b.phase,
                });
                if k == 0 {
                    last_player = Some((bx, by, b.r));
                }
            }
        }
    }

    // the seal-arc: drains clockwise from 12 o'clock around the newest player strike
    if seal >= 0.0 {
        if let Some((px, py, pr)) = last_player {
            let rr = pr + 8.0;
            let n = ((seal.clamp(0.0, 1.0) * 40.0) as usize).max(1);
            let pts: Vec<(f32, f32)> = (0..=n)
                .map(|k| {
                    let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * k as f32 / 40.0;
                    (px + a.cos() * rr, py + a.sin() * rr)
                })
                .collect();
            sc.push(Prim::Beam {
                points: pts,
                color: PHOSPHOR,
                intensity: 0.8,
            });
        }
    }

    // the weave strip (newest right, alternating facing — mirrors the overlay)
    if !shards.is_empty() {
        let n = shards.len().min(24);
        let total_w = n as f32 * 14.0;
        let wx0 = c - total_w / 2.0 + 7.0;
        for (i, seg) in shards[shards.len() - n..].iter().enumerate() {
            let rot = if i % 2 == 0 {
                -0.35
            } else {
                0.35 + std::f32::consts::PI
            };
            sc.push(Prim::Shard {
                cx: wx0 + i as f32 * 14.0,
                cy: STAGE_SIZE - 46.0,
                size: 3.5 + 4.5 * seg.amp,
                rot,
                color: TWIN.rotate_hue(seg.hue),
                intensity: (0.5 + 0.5 * seg.shimmer).min(1.0),
            });
        }
    }

    if !hint.is_empty() {
        sc.push(Prim::Text {
            x: c,
            y: c + 150.0,
            s: hint.to_string(),
            color: TWIN.lerp(VOID, 0.25),
            size: 13.0,
            anchor: Anchor::Middle,
        });
    }

    sc
}

/// A compact readout strip for the panel: the live signals as quiet monospace.
#[must_use]
pub fn signals_strip(turn: &TwinTurn) -> Scene {
    let mut sc = Scene::new(SCENE_W, 40.0);
    sc.bg = VOID;
    let g = &turn.signals;
    let judged = match turn.judged {
        Some(Judgment::Harmony) => "harmony",
        Some(Judgment::Counterpoint) => "counterpoint",
        None => "—",
    };
    let line = format!(
        "sync {:>4.2}   heat {:>4.2}   novelty {:>4.1}b   drift {:>4.0}   weave {}   depth {}   {}",
        g.sync, g.heat, g.novelty, g.drift, g.exchanges, g.palette_depth, judged
    );
    let accent = if g.sync > 0.7 { HUSH } else { PHOSPHOR };
    sc.push(Prim::Text {
        x: 16.0,
        y: 26.0,
        s: line,
        color: accent,
        size: 14.0,
        anchor: Anchor::Start,
    });
    sc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rhythm::Motif;
    use crate::rhythm::{Onset, OnsetKind, Voice};
    use crate::twin::{Familiar, TwinConfig};

    fn groove(ioi: u64, count: usize) -> Motif {
        Motif {
            onsets: (0..count)
                .map(|i| Onset {
                    t_ms: i as u64 * ioi,
                    energy: 0.6,
                    kind: OnsetKind::Tap,
                    voice: Voice::neutral(),
                })
                .collect(),
        }
    }

    #[test]
    fn rgb_hex_and_lerp() {
        assert_eq!(PHOSPHOR.hex(), "#4af2b0");
        let mid = PHOSPHOR.lerp(TWIN, 0.5);
        assert!(mid != PHOSPHOR && mid != TWIN);
        // a 120° hue rotation of a saturated colour must actually move it (the fields are u8,
        // so a `<= 255` bound is vacuously true — assert the real behaviour instead).
        let r = PHOSPHOR.rotate_hue(120.0);
        assert!(r != PHOSPHOR, "120° rotation should change a saturated colour");
    }

    #[test]
    fn ngon_is_regular() {
        let p = ngon(0.0, 0.0, 10.0, 6, 0.0);
        assert_eq!(p.len(), 6);
        for (x, y) in &p {
            assert!((x.hypot(*y) - 10.0).abs() < 1e-3, "vertex off the circle");
        }
    }

    #[test]
    fn knockback_scene_builds_constructs_and_a_blueprint() {
        let mut fam = Familiar::new(TwinConfig::default());
        let turn = fam.receive(&groove(250, 5));
        let sc = knockback_scene(&turn.knockback, 1.0, turn.event.as_ref());
        let constructs = sc
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Construct { .. }))
            .count();
        let blueprints = sc
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Blueprint { .. }))
            .count();
        assert!(
            constructs >= 5,
            "every beat is a hard-light construct, got {constructs}"
        );
        assert_eq!(blueprints, 1, "exactly one unbuilt blueprint to answer");
    }

    #[test]
    fn svg_is_wellformed_deterministic_and_prismatic() {
        let mut fam = Familiar::new(TwinConfig::default());
        let turn = fam.receive(&groove(300, 4));
        let sc = knockback_scene(&turn.knockback, 0.7, None);
        let a = sc.to_svg();
        assert_eq!(a, sc.to_svg(), "SVG must be deterministic");
        assert!(a.starts_with("<svg") && a.ends_with("</svg>"));
        assert!(a.contains(&TWIN.hex()), "twin hard light present");
        assert!(
            a.contains(&PHOSPHOR.hex()),
            "the answer blueprint is phosphor"
        );
        // the prismatic refraction fringe must be there
        assert!(
            a.contains(&REFRACT_COOL.hex()) && a.contains(&REFRACT_WARM.hex()),
            "refraction fringe"
        );
        assert!(
            a.contains("mix-blend-mode:screen"),
            "additive hard-light compositing"
        );
    }

    #[test]
    fn facets_scale_with_force() {
        assert!(
            facets(0.1) < facets(0.95),
            "a harder hit builds a more elaborate construct"
        );
        assert!(
            facets(0.0) >= 3,
            "even the softest beat is at least a triangle"
        );
    }

    #[test]
    fn playhead_builds_left_to_right() {
        let mut fam = Familiar::new(TwinConfig::default());
        let turn = fam.receive(&groove(250, 6));
        let count = |f: f32| {
            knockback_scene(&turn.knockback, f, None)
                .prims
                .iter()
                .filter(|p| matches!(p, Prim::Construct { .. }))
                .count()
        };
        assert!(count(0.0) <= count(1.0), "the phrase builds across time");
    }

    #[test]
    fn weave_is_a_row_of_shards() {
        let segs: Vec<BraidSeg> = (0..6)
            .map(|i| BraidSeg {
                hue: i as f32 * 10.0,
                shimmer: 0.3,
                amp: 0.5,
            })
            .collect();
        let sc = weave_scene(&segs, 2);
        let shards = sc
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Shard { .. }))
            .count();
        assert_eq!(shards, 6);
    }

    #[test]
    fn sigil_is_a_closed_prismatic_glyph() {
        let mut fam = Familiar::new(TwinConfig::default());
        for i in 0..30 {
            fam.receive(&groove(180 + i * 7, 4));
        }
        let path = fam.sigil_path(200);
        let svg = sigil_scene(&path, 240.0).to_svg();
        assert!(svg.contains('Z'), "the sigil path is closed");
        assert!(svg.contains(&REFRACT_COOL.hex()), "the sigil refracts");
    }

    #[test]
    fn stage_scene_mirrors_the_overlay_layout() {
        // a representative moment: the twin's reply stands played (3 + 1 flourish), the
        // blueprint waits, the weave has history, the familiar is present.
        let beats: Vec<StageBeat> = (0..3)
            .map(|i| StageBeat {
                x: -STAGE_STAFF_HALF + i as f32 * 33.0,
                y: 0.0,
                r: 14.0,
                color: TWIN,
                weight: 0.6,
                phase: 0.3,
                kind: 1,
            })
            .chain(std::iter::once(StageBeat {
                x: -STAGE_STAFF_HALF + 99.0,
                y: 0.0,
                r: 17.0,
                color: TWIN.lerp(HUSH, 0.4),
                weight: 0.8,
                phase: 0.0,
                kind: 2,
            }))
            .chain(std::iter::once(StageBeat {
                x: -STAGE_STAFF_HALF + 138.0,
                y: 0.0,
                r: 13.0,
                color: PHOSPHOR,
                weight: 1.0,
                phase: 0.0,
                kind: 3,
            }))
            .collect();
        let shards: Vec<BraidSeg> = (0..7)
            .map(|i| BraidSeg {
                hue: i as f32 * 20.0,
                shimmer: 0.5,
                amp: 0.6,
            })
            .collect();
        let sc = stage_scene(
            &beats,
            -1.0,
            None,
            &shards,
            "answer it \u{2014} finish the line",
            1.0,
        );
        let svg = sc.to_svg();
        assert_eq!(svg, sc.to_svg(), "stage SVG deterministic");
        let blueprints = sc
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Blueprint { .. }))
            .count();
        assert_eq!(blueprints, 1, "one open blueprint on stage");
        let constructs = sc
            .prims
            .iter()
            .filter(|p| matches!(p, Prim::Construct { .. }))
            .count();
        assert!(
            constructs >= 5,
            "twin beats + flourish + familiar all built, got {constructs}"
        );
        assert!(svg.contains("answer it"));
    }

    #[test]
    fn signals_strip_reads_the_machine() {
        let mut fam = Familiar::new(TwinConfig::default());
        let turn = fam.receive(&groove(300, 4));
        let svg = signals_strip(&turn).to_svg();
        assert!(svg.contains("sync") && svg.contains("weave"));
    }
}
