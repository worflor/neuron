// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A learned [`Engine`]: a small attention network that reads a palette and answers four
//! questions about it. What colour could a stop nobody has coloured yet take (`masked`)? What could
//! the stop being worked on be instead (`refine`)? How should the other stops answer an edit
//! (`follow`)? Which stops look out of place (`repair`)? Every answer is weighed against leaving
//! the palette alone, and the bundle's margins decide what is worth offering.
//!
//! This is a port of Luma's core v4.1 to v4.3 (layout 5, the revision 3 runtime) from its bridge
//! specification, for stops on a line. The network is data: [`Learned::load`] reads a `.lumacore`
//! bundle from a folder, and no trained weights are compiled in. Each stop becomes one token (its
//! colour, position, flags, the colour its neighbours imply and what it was before the edit) and
//! each pair of stops a few numbers about how far apart they sit, which bias attention. A request
//! may carry an intent (a wanted change in OKLab, placed on the free stops) and a style vector
//! made from other palettes by the same person. The answer for a stop is a mixture of Gaussians in
//! OKLab whose last component sits on the stop's current colour, so "keep" is always one answer.
//!
//! Everything is `f64` and follows the reference step for step: the thresholds sit below `f32`
//! resolution, and a colour on the gamut edge must land on the same side in both.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Engine, Offer, Scene, Scope};
use crate::lighting::Rgb;

/// The bundle [`Learned::load`] looks for in its folder.
pub const BUNDLE: &str = "colour-model.lumacore";
const MAGIC: &[u8; 8] = b"LUMACORE";
const FORMAT: &str = "luma.core-bridge.v1";
/// Token values before the intent and the style vector, the intent's own, and a pair's.
const SPACE: usize = 87;
const INTENT: usize = 5;
const PAIR: usize = 14;
const SCALE: [f64; 3] = [2.0, 4.0, 4.0];
const SHIFT: [f64; 3] = [0.5, 0.0, 0.0];
const GREY: Lab = [0.5, 0.0, 0.0];
/// Scale of an OKLab change as a model input.
const MOVE: f64 = 20.0;
const RANGES: [f64; 5] = [0.1, 0.2, 0.4, 0.8, 1.6];
/// The most palettes, and colours of each, a style vector reads.
const SUPPORT: usize = 8;
const SUPPORT_COLOURS: usize = 12;
/// Modes read from a stop's mixture before scoring, whatever the caller asks to see.
const MODES: usize = 5;
/// How far, in OKLab, a suggestion may sit from the colour it would replace. The policy margins
/// were fitted on requests with this budget.
const BUDGET: f64 = 0.18;
/// The narrowest spread, in OKLab, a terrain is given.
const GRAIN: f64 = 0.03;
/// The largest per-stop OKLab distance under which two palettes look the same, and under which
/// two options are one.
const VISIBLE: f64 = 0.01;
const DISTINCT: f64 = 0.02;
/// The strengths a companion edit is also offered at.
const STRENGTHS: [f64; 2] = [0.5, 0.25];
/// The longest change the model was trained to take as an intent.
const NUDGE: f64 = 0.10;

type Lab = [f64; 3];
type Srgb = [f64; 3];

/// `y = W x + b`, `W` row-major. For a normalisation, the per-channel gain and offset.
struct Linear {
    weight: Vec<f64>,
    bias: Vec<f64>,
}

impl Linear {
    fn apply(&self, x: &[f64]) -> Vec<f64> {
        self.bias.iter().zip(self.weight.chunks_exact(x.len())).map(|(b, row)| b + dot(row, x)).collect()
    }
}

struct Block {
    norm1: Linear,
    qkv: Linear,
    proj: Linear,
    norm2: Linear,
    wide: Linear,
    narrow: Linear,
}

/// One stop's answer: mixture logits, and each component's mean and spread in OKLab. The last
/// component is "no change": the stop's colour, or what its neighbours imply when it has none.
struct Mixture {
    logits: Vec<f64>,
    means: Vec<Lab>,
    sigmas: Vec<Lab>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Task {
    Masked,
    Refine,
    Follow,
    Repair,
}

#[derive(Clone, Copy)]
struct Margins {
    /// An option must beat keeping by more than this to be listed at all.
    spoil: f64,
    /// The best option must beat keeping by more than this for the answer to be "change".
    keep: f64,
    /// Repair only: a stop may change when the model's belief that it should stay is under this.
    stop: f64,
    /// The least an option must change some stop by (OKLab) to be worth showing.
    visible: f64,
}

/// A request: a palette in display sRGB (`0..1`), in position order.
struct Ask<'a> {
    task: Task,
    rgb: &'a [Srgb],
    /// The palette before the last edit; `None` when there was none.
    before: Option<&'a [Srgb]>,
    /// Stop positions on `-1..=1`.
    positions: &'a [f64],
    pins: &'a [bool],
    /// The stops the request may change, pins aside.
    scope: &'a [bool],
    active: Option<usize>,
    /// Stops whose colour their owner has settled on.
    recent: &'a [usize],
    /// The edit budget; `None` for no limit.
    radius: Option<f64>,
    count: usize,
    /// A wanted change in OKLab, aimed at the free stops.
    intent: Option<Lab>,
    /// From [`Learned::style_vector`].
    style: &'a [f64],
}

/// A palette the request could become, and how the network rates it against the palette as it
/// stands (log-density difference in nats; positive prefers the candidate).
struct Candidate {
    rgb: Vec<Srgb>,
    #[cfg_attr(not(test), expect(dead_code, reason = "options arrive best first; the number is for the reference check"))]
    score: f64,
}

struct Answer {
    /// False when the request leaves nothing to suggest for (no free stop, a palette of one).
    offerable: bool,
    /// True when nothing clears the margin over leaving the palette alone. Options may still be
    /// listed: they are what the model would show if asked anyway.
    keep: bool,
    options: Vec<Candidate>,
    /// Per free stop of the candidate pass: the mixture mass on "no change".
    keep_probability: Vec<f64>,
}

/// One pass over the palette.
#[derive(Clone)]
struct Inputs {
    lab: Vec<Lab>,
    pinned: Vec<bool>,
    free: Vec<bool>,
    /// Whether the stop shows a colour.
    shown: Vec<bool>,
    /// Whether `previous` is known for the stop.
    known: Vec<bool>,
    active: Option<usize>,
    previous: Vec<Lab>,
    recent: Vec<bool>,
}

struct Condition<'a> {
    intent: Option<Lab>,
    style: &'a [f64],
}

/// Which stops are each other's neighbours on the line, with their distances and offsets.
struct Near {
    near: Vec<Vec<bool>>,
    distance: Vec<Vec<f64>>,
    delta: Vec<Vec<f64>>,
}

/// A loaded colour model.
pub struct Learned {
    name: String,
    heads: usize,
    components: usize,
    /// Width of the style vector without its presence flag.
    style: usize,
    features: usize,
    /// Trailing heads of each block that take no geometry bias.
    open_heads: usize,
    cone: f64,
    sigma: [f64; 2],
    merge: f64,
    /// Nats by which a move must outweigh "leave it" on a stop that shows a colour.
    favour: f64,
    duplicate: f64,
    masked: Option<Margins>,
    refine: Option<Margins>,
    follow: Option<Margins>,
    repair: Option<Margins>,
    intent: Option<Margins>,
    embed: [Linear; 2],
    bias: [Linear; 2],
    blocks: Vec<Block>,
    norm: Linear,
    head: [Linear; 2],
    style_token: [Linear; 2],
    style_query: Linear,
    style_palette: Linear,
    style_pick: Linear,
    style_out: Linear,
}

impl Learned {
    /// Read the bundle in `folder`. `Ok(None)` when there is none; an error when one is there and
    /// cannot be used.
    pub fn load(folder: &Path) -> Result<Option<Learned>, String> {
        match std::fs::read(folder.join(BUNDLE)) {
            Ok(bytes) => Learned::parse(&bytes).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{BUNDLE}: {e}")),
        }
    }

    /// Build a model from a `.lumacore` bundle: magic, container version and header length
    /// (little-endian `u32`s), a JSON header, zero padding to a multiple of 64, then the arrays as
    /// little-endian `f32`. Refuses an unknown container, format, layout or runtime revision, a
    /// payload whose length or hash the header does not name, and arrays of the wrong shape.
    pub fn parse(bundle: &[u8]) -> Result<Learned, String> {
        let word = |at: usize| bundle.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        if bundle.get(..8) != Some(MAGIC.as_slice()) || word(8) != Some(1) {
            return Err("not a colour model bundle this build reads".into());
        }
        let header = word(12).and_then(|len| bundle.get(16..16usize.checked_add(len)?)).ok_or("bundle header is cut short")?;
        let payload = bundle.get((16 + header.len()).div_ceil(64) * 64..).ok_or("bundle payload is missing")?;
        let doc: serde_json::Value = serde_json::from_slice(header).map_err(|e| format!("bundle header: {e}"))?;
        if doc["format"] != FORMAT {
            return Err(format!("not a {FORMAT} bundle"));
        }
        let digest: String = Sha256::digest(payload).iter().map(|b| format!("{b:02x}")).collect();
        if doc["payload_bytes"].as_u64() != Some(payload.len() as u64) || doc["payload_sha256"] != digest.as_str() {
            return Err("bundle payload does not match its header".into());
        }
        let config = &doc["config"];
        let number = |v: &serde_json::Value, what: &str| v.as_f64().ok_or_else(|| format!("bundle config: {what}"));
        let size = |what: &str| config[what].as_u64().map(|v| v as usize).filter(|v| (1..=4096).contains(v)).ok_or_else(|| format!("bundle config: {what}"));
        let (width, heads, blocks, components, style) = (size("width")?, size("heads")?, size("blocks")?, size("components")?, size("style")?);
        let policy = &config["policy"];
        let stated = |v: &serde_json::Value, word: &str| v.is_null() || v == word;
        if config["layout"] != 5 || config["revision"] != 3 || config["order"] != "weight" || width % heads != 0
            || !stated(&policy["measure"], "density") || !stated(&policy["joint"], "pseudo")
        {
            return Err("unsupported colour model layout, revision or policy".into());
        }
        let mut arrays: HashMap<&str, (Vec<usize>, usize)> = HashMap::new();
        for a in doc["arrays"].as_array().ok_or("bundle header: arrays")? {
            let shape: Option<Vec<usize>> = a["shape"].as_array().map(|s| s.iter().filter_map(|v| v.as_u64().map(|v| v as usize)).collect());
            let (Some(name), Some(shape), Some(offset), "<f4") = (a["name"].as_str(), shape, a["offset"].as_u64(), a["dtype"].as_str().unwrap_or("")) else {
                return Err("bundle header: array entry".into());
            };
            arrays.insert(name, (shape, offset as usize));
        }
        let take = |name: &str, shape: &[usize]| -> Result<Vec<f64>, String> {
            let (found, offset) = arrays.get(name).ok_or_else(|| format!("bundle has no array {name}"))?;
            let bytes = shape.iter().try_fold(4usize, |total, side| total.checked_mul(*side));
            let raw = bytes.and_then(|bytes| offset.checked_add(bytes)).and_then(|end| payload.get(*offset..end)).filter(|_| found == shape);
            let raw = raw.ok_or_else(|| format!("bundle array {name} has the wrong shape"))?;
            Ok(raw.chunks_exact(4).map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))).collect())
        };
        // `out` of `None` takes the row count the bundle states; the next layer's input checks it.
        let linear = |name: &str, out: Option<usize>, inp: usize| -> Result<Linear, String> {
            let stated = arrays.get(format!("{name}.weight").as_str()).and_then(|(shape, _)| shape.first().copied());
            let out = out.or(stated).filter(|rows| (1..=4096).contains(rows)).ok_or_else(|| format!("bundle has no array {name}.weight"))?;
            Ok(Linear { weight: take(&format!("{name}.weight"), &[out, inp])?, bias: take(&format!("{name}.bias"), &[out])? })
        };
        let scale = |name: &str| -> Result<Linear, String> {
            Ok(Linear { weight: take(&format!("{name}.weight"), &[width])?, bias: take(&format!("{name}.bias"), &[width])? })
        };
        let margins = |task: &str| -> Result<Option<Margins>, String> {
            let entry = &policy[task];
            if entry.is_null() {
                return Ok(None);
            }
            let stop = if entry["stop"].is_null() { 1.0 } else { number(&entry["stop"], "policy margin")? };
            let visible = if entry["visible"].is_null() { VISIBLE } else { number(&entry["visible"], "policy margin")? };
            Ok(Some(Margins { spoil: number(&entry["spoil"], "policy margin")?, keep: number(&entry["keep"], "policy margin")?, stop, visible }))
        };
        let optional = |v: &serde_json::Value, what: &str, otherwise: f64| if v.is_null() { Ok(otherwise) } else { number(v, what) };
        let features = SPACE + INTENT + style + 1;
        let merge = number(&config["merge"], "merge")?;
        let bias = linear("bias.0", None, PAIR)?;
        let lanes = bias.bias.len();
        let token = linear("style.token.0", None, 4)?;
        let lane = token.bias.len();
        let palette = linear("style.palette", None, 2 * lane)?;
        let pooled = palette.bias.len();
        Ok(Learned {
            name: doc["name"].as_str().unwrap_or("colour model").to_string(),
            heads,
            components,
            style,
            features,
            open_heads: config["open_heads"].as_u64().unwrap_or(0).min(heads as u64) as usize,
            cone: optional(&config["cone"], "cone", 0.5)?,
            sigma: [number(&config["sigma"][0], "sigma")?, number(&config["sigma"][1], "sigma")?],
            merge,
            favour: optional(&config["favour"], "favour", 0.0)?,
            duplicate: optional(&policy["duplicate"], "policy.duplicate", merge)?,
            masked: margins("masked")?,
            refine: margins("refine")?,
            follow: margins("follow")?,
            repair: margins("repair")?,
            intent: margins("intent")?,
            embed: [linear("embed.0", Some(width), features)?, linear("embed.2", Some(width), width)?],
            bias: [bias, linear("bias.2", Some(blocks * heads), lanes)?],
            blocks: (0..blocks).map(|i| {
                let p = format!("blocks.{i}");
                Ok(Block {
                    norm1: scale(&format!("{p}.norm1"))?,
                    qkv: linear(&format!("{p}.qkv"), Some(3 * width), width)?,
                    proj: linear(&format!("{p}.proj"), Some(width), width)?,
                    norm2: scale(&format!("{p}.norm2"))?,
                    wide: linear(&format!("{p}.wide"), Some(2 * width), width)?,
                    narrow: linear(&format!("{p}.narrow"), Some(width), 2 * width)?,
                })
            }).collect::<Result<_, String>>()?,
            norm: scale("norm")?,
            head: [linear("head.0", Some(width), width)?, linear("head.2", Some(7 * components + 4), width)?],
            style_token: [token, linear("style.token.2", Some(lane), lane)?],
            style_query: linear("style.query", Some(1), lane)?,
            style_pick: linear("style.pick", Some(1), pooled)?,
            style_out: linear("style.out", Some(style), 2 * pooled)?,
            style_palette: palette,
        })
    }

    /// The name its bundle gives it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What a set of palettes by one person says about their taste, as the model reads it: the
    /// style values and a presence flag, all zero for an empty set. The order of palettes and of
    /// colours does not matter; palettes and colours past the limits are not read.
    fn style_vector(&self, support: &[Vec<Lab>]) -> Vec<f64> {
        let mut out = vec![0.0; self.style + 1];
        let palettes: Vec<&[Lab]> = support.iter().map(|p| &p[..p.len().min(SUPPORT_COLOURS)]).filter(|p| !p.is_empty()).take(SUPPORT).collect();
        if palettes.is_empty() {
            return out;
        }
        // a set of rows as their attention-weighted sum followed by their mean
        let pooled = |rows: &[Vec<f64>], query: &Linear| -> Vec<f64> {
            let share = softmax(&rows.iter().map(|row| query.apply(row)[0]).collect::<Vec<f64>>());
            let width = rows[0].len();
            let mut both = vec![0.0; 2 * width];
            for (row, s) in rows.iter().zip(&share) {
                for d in 0..width {
                    both[d] += s * row[d];
                    both[width + d] += row[d] / rows.len() as f64;
                }
            }
            both
        };
        let each: Vec<Vec<f64>> = palettes.iter().map(|palette| {
            let hidden: Vec<Vec<f64>> = palette.iter().map(|lab| {
                let token = [(lab[0] - SHIFT[0]) * SCALE[0], (lab[1] - SHIFT[1]) * SCALE[1], (lab[2] - SHIFT[2]) * SCALE[2], 4.0 * lab[1].hypot(lab[2])];
                self.style_token[1].apply(&gelu(self.style_token[0].apply(&token)))
            }).collect();
            gelu(self.style_palette.apply(&pooled(&hidden, &self.style_query)))
        }).collect();
        for (o, z) in out.iter_mut().zip(self.style_out.apply(&pooled(&each, &self.style_pick))) {
            *o = z.tanh();
        }
        out[self.style] = 1.0;
        out
    }

    /// One token per stop, and the pairwise geometry attention is biased by.
    fn tokens(&self, inp: &Inputs, positions: &[f64], condition: &Condition) -> (Vec<Vec<f64>>, Vec<Vec<[f64; PAIR]>>) {
        let n = inp.lab.len();
        let seen = neighbours(positions, &inp.shown, self.cone);
        let everyone = neighbours(positions, &vec![true; n], self.cone);
        let (between, found) = interpolate(&inp.lab, &seen);
        let flag = |on: bool| f64::from(u8::from(on));
        let nearest: Vec<f64> = (0..n).map(|i| if n > 1 { (0..n).filter(|j| *j != i).map(|j| seen.distance[i][j]).fold(f64::INFINITY, f64::min) } else { 0.0 }).collect();
        let pi = std::f64::consts::PI;
        let tokens = (0..n).map(|i| {
            let mut t = vec![0.0; self.features];
            let lab = inp.lab[i];
            // a stop sits on a line: its second and third coordinates are zero
            let coords = [positions[i], 0.0, 0.0];
            let relative = inp.active.map_or([0.0; 3], |a| [positions[i] - positions[a], 0.0, 0.0]);
            for c in 0..3 {
                if inp.shown[i] {
                    t[c] = (lab[c] - SHIFT[c]) * SCALE[c];
                }
                t[3 + c] = coords[c];
                if found[i] {
                    t[10 + c] = (between[i][c] - SHIFT[c]) * SCALE[c];
                }
                if inp.shown[i] && found[i] {
                    t[14 + c] = (lab[c] - between[i][c]) * SCALE[c];
                }
                if inp.known[i] {
                    t[17 + c] = (inp.previous[i][c] - SHIFT[c]) * SCALE[c];
                }
                if inp.known[i] && inp.shown[i] {
                    t[22 + c] = (lab[c] - inp.previous[i][c]) * SCALE[c];
                }
                t[25 + c] = relative[c];
                for (w, wave) in [1.0, 2.0, 4.0, 8.0].into_iter().enumerate() {
                    t[39 + 4 * c + w] = (coords[c] * (pi * wave)).sin();
                    t[51 + 4 * c + w] = (coords[c] * (pi * wave)).cos();
                }
                if inp.active.is_some() {
                    for (w, wave) in [0.5, 1.0, 2.0, 4.0].into_iter().enumerate() {
                        t[63 + 4 * c + w] = (relative[c] * (pi * wave)).sin();
                        t[75 + 4 * c + w] = (relative[c] * (pi * wave)).cos();
                    }
                }
            }
            (t[6], t[7], t[8], t[9]) = (flag(inp.pinned[i]), flag(inp.free[i]), flag(inp.shown[i]), flag(inp.active == Some(i)));
            (t[13], t[20], t[21]) = (flag(found[i]), flag(inp.known[i]), flag(inp.recent[i]));
            t[28] = relative[0].abs();
            // surface 0 of five: a gradient. Its stops have no size and no weight (34 to 37).
            t[29] = 1.0;
            t[38] = nearest[i];
            if let (Some(intent), true) = (condition.intent, inp.free[i]) {
                let push = intent.map(|v| v * MOVE);
                t[SPACE] = 1.0;
                t[SPACE + 1..SPACE + 4].copy_from_slice(&push);
                t[SPACE + 4] = (push[0] * push[0] + push[1] * push[1] + push[2] * push[2]).sqrt();
            }
            t[SPACE + INTENT..].copy_from_slice(condition.style);
            t
        }).collect();
        let pair = (0..n).map(|i| (0..n).map(|j| {
            let d = seen.distance[i][j];
            let mut p = [0.0; PAIR];
            (p[0], p[1]) = (d, d * d);
            for (r, range) in RANGES.into_iter().enumerate() {
                p[2 + r] = (-(d / range) * (d / range)).exp();
            }
            p[7] = seen.delta[i][j];
            p[10] = (d.max(1e-6) / nearest[i].max(1e-6)).log2().clamp(-1.0, 5.0) / 4.0;
            (p[11], p[12], p[13]) = (flag(everyone.near[i][j]), flag(seen.near[i][j]), flag(i == j));
            p
        }).collect()).collect();
        (tokens, pair)
    }

    /// The network: tokens and pair geometry in, one mixture per stop out.
    fn forward(&self, tokens: &[Vec<f64>], pair: &[Vec<[f64; PAIR]>]) -> Vec<Mixture> {
        let k = self.components;
        let bias: Vec<Vec<Vec<f64>>> = pair.iter().map(|row| row.iter().map(|p| self.bias[1].apply(&gelu(self.bias[0].apply(p)))).collect()).collect();
        let mut x: Vec<Vec<f64>> = tokens.iter().map(|t| self.embed[1].apply(&gelu(self.embed[0].apply(t)))).collect();
        let width = x.first().map_or(0, Vec::len);
        let part = width / self.heads;
        for (b, block) in self.blocks.iter().enumerate() {
            // queries, keys and values, each a third of the row; a head owns `part` of each third
            let qkv: Vec<Vec<f64>> = x.iter().map(|row| block.qkv.apply(&norm(row, &block.norm1))).collect();
            for (i, row) in x.iter_mut().enumerate() {
                let mut mixed = vec![0.0; width];
                for head in 0..self.heads {
                    let of = |third: usize| third * width + head * part..third * width + (head + 1) * part;
                    let lean = |j: usize| if head < self.heads - self.open_heads { bias[i][j][b * self.heads + head] } else { 0.0 };
                    let score: Vec<f64> = qkv.iter().enumerate().map(|(j, other)| dot(&qkv[i][of(0)], &other[of(1)]) / (part as f64).sqrt() + lean(j)).collect();
                    for (other, w) in qkv.iter().zip(softmax(&score)) {
                        for (m, v) in mixed[head * part..(head + 1) * part].iter_mut().zip(&other[of(2)]) {
                            *m += w * v;
                        }
                    }
                }
                add(row, &block.proj.apply(&mixed));
                let hidden = block.narrow.apply(&gelu(block.wide.apply(&norm(row, &block.norm2))));
                add(row, &hidden);
            }
        }
        let (low, high) = (self.sigma[0].ln(), self.sigma[1].ln());
        x.iter().zip(tokens).map(|(row, token)| {
            let out = self.head[1].apply(&gelu(self.head[0].apply(&norm(row, &self.norm))));
            // a stop without a colour starts from what its neighbours imply
            let from = if token[8] > 0.0 { 0 } else { 10 };
            let base: Lab = std::array::from_fn(|d| token[from + d] / SCALE[d] + SHIFT[d]);
            Mixture {
                logits: out[..=k].to_vec(),
                means: (0..=k).map(|c| if c < k { std::array::from_fn(|d| base[d] + out[k + 1 + 3 * c + d] / SCALE[d]) } else { base }).collect(),
                sigmas: (0..=k).map(|c| std::array::from_fn(|d| (low + (high - low) / (1.0 + (-out[4 * k + 1 + 3 * c + d]).exp())).exp())).collect(),
            }
        }).collect()
    }

    fn run(&self, inp: &Inputs, positions: &[f64], condition: &Condition) -> Vec<Mixture> {
        let (tokens, pair) = self.tokens(inp, positions, condition);
        self.forward(&tokens, &pair)
    }

    /// The mixture's components as distinct colours, heaviest first, each with its weight:
    /// components closer than the merge distance to a group's representative join it, and a group
    /// the "no change" component joins is represented by it. `favour` lifts that group in the
    /// order without changing its weight.
    fn modes(&self, mix: &Mixture, favour: f64) -> Vec<(usize, f64)> {
        let weight = softmax(&mix.logits);
        let mut order: Vec<usize> = (0..weight.len()).collect();
        order.sort_by(|&a, &b| weight[b].total_cmp(&weight[a]));
        let mut kept: Vec<(usize, f64)> = Vec::new();
        for i in order {
            match kept.iter_mut().find(|row| distance(mix.means[row.0], mix.means[i]) < self.merge) {
                Some(row) => {
                    row.1 += weight[i];
                    if i == self.components {
                        row.0 = i;
                    }
                }
                None => kept.push((i, weight[i])),
            }
        }
        let rank = |row: &(usize, f64)| if row.0 == self.components && favour != 0.0 { row.1 * favour.exp() } else { row.1 };
        kept.sort_by(|a, b| rank(b).total_cmp(&rank(a)));
        kept
    }

    /// How much of a stop's answer is "no change": the last component and every one that sits on it.
    fn keep_probability(&self, mix: &Mixture) -> f64 {
        let anchor = mix.means[self.components];
        softmax(&mix.logits).iter().zip(&mix.means).filter(|(_, mean)| distance(**mean, anchor) < self.merge).map(|(w, _)| w).sum()
    }

    /// For each palette in `candidates`, the summed log-density of its free stops' colours. With
    /// several free stops, each is rated with the other free stops already at the candidate's colours.
    fn score(&self, inp: &Inputs, positions: &[f64], condition: &Condition, candidates: &[Vec<Lab>]) -> Vec<f64> {
        let stops: Vec<usize> = (0..inp.free.len()).filter(|k| inp.free[*k]).collect();
        if let [only] = stops[..] {
            let mix = self.run(inp, positions, condition);
            return candidates.iter().map(|c| log_density(&mix[only], c[only])).collect();
        }
        candidates.iter().map(|candidate| {
            stops.iter().map(|&s| {
                let mut scene = inp.clone();
                for k in stops.iter().copied().filter(|k| *k != s) {
                    scene.lab[k] = candidate[k];
                    scene.shown[k] = true;
                }
                log_density(&self.run(&scene, positions, condition)[s], candidate[s])
            }).sum()
        }).collect()
    }

    /// Candidates for the request, scored against the palette as it stands.
    fn answer(&self, ask: &Ask) -> Answer {
        let n = ask.rgb.len();
        let mut out = Answer { offerable: false, keep: true, options: Vec::new(), keep_probability: Vec::new() };
        let free: Vec<bool> = (0..n).map(|i| ask.scope[i] && !ask.pins[i]).collect();
        let one = matches!(ask.task, Task::Masked | Task::Refine);
        let able = match ask.task {
            Task::Masked | Task::Refine => n >= 2 && ask.active.is_some_and(|a| free[a]),
            Task::Follow => ask.active.is_some(),
            Task::Repair => n >= 2,
        };
        if !able {
            return out;
        }
        let inputs = inputs(ask, &free, one);
        let stops: Vec<usize> = (0..n).filter(|k| inputs.free[*k]).collect();
        if stops.is_empty() {
            return out;
        }
        out.offerable = true;
        let condition = Condition { intent: ask.intent, style: ask.style };
        let mix = self.run(&inputs, ask.positions, &condition);
        out.keep_probability = stops.iter().map(|&k| self.keep_probability(&mix[k])).collect();
        let margins = match ask.task {
            Task::Masked => self.masked,
            Task::Refine => ask.intent.and(self.intent).or(self.refine),
            Task::Follow => self.follow,
            Task::Repair => self.repair,
        };
        let Some(margins) = margins else { return out };
        let anchor = self.components;
        let current = &inputs.lab;
        let asked = ask.intent.is_some();
        let far = |a: &[Lab], b: &[Lab]| a.iter().zip(b).map(|(x, y)| distance(*x, *y)).fold(0.0, f64::max);
        // every stop outside `changing` comes back exactly as it was sent
        let settle = |mut rgb: Vec<Srgb>, changing: &dyn Fn(usize) -> bool| {
            for j in (0..n).filter(|j| !changing(*j)) {
                rgb[j] = ask.rgb[j];
            }
            rgb
        };
        let mut candidates: Vec<Vec<Srgb>> = Vec::new();
        match ask.task {
            Task::Masked | Task::Refine => {
                let active = stops[0];
                let refine = ask.task == Task::Refine;
                for (component, _) in self.modes(&mix[active], if refine && !asked { self.favour } else { 0.0 }).into_iter().take(MODES) {
                    let centre = mix[active].means[component];
                    let mut sample = current.clone();
                    // "leave it" is the stop's colour itself, not its round trip through the display
                    sample[active] = if refine && component == anchor { centre } else { oklab(display(centre)) };
                    let rgb = legal(ask, &free, &sample, (!refine).then_some(active));
                    candidates.push(settle(rgb, &|j| j == active));
                }
            }
            Task::Follow => {
                // Each stop's own leading mode, each stop's leading move, and that move at half and
                // quarter strength. The edit was the owner's, so "leave it" gets no head start.
                let (mut mixed, mut carry) = (current.clone(), current.clone());
                for &stop in &stops {
                    let ranked = self.modes(&mix[stop], 0.0);
                    if let Some((component, _)) = ranked.iter().find(|row| row.0 != anchor) {
                        carry[stop] = oklab(display(mix[stop].means[*component]));
                        if ranked[0].0 != anchor {
                            mixed[stop] = carry[stop];
                        }
                    }
                }
                let graded = |strength: f64| -> Vec<Lab> {
                    (0..n).map(|k| {
                        if distance(carry[k], current[k]) > 0.0 { oklab(display(std::array::from_fn(|c| current[k][c] + strength * (carry[k][c] - current[k][c])))) } else { current[k] }
                    }).collect()
                };
                let mut chosen: Vec<Vec<Lab>> = Vec::new();
                for reading in [mixed, carry.clone(), graded(STRENGTHS[0]), graded(STRENGTHS[1])] {
                    if far(&reading, current) >= VISIBLE && chosen.iter().all(|other| far(&reading, other) >= DISTINCT) {
                        chosen.push(reading);
                    }
                }
                for reading in chosen {
                    candidates.push(settle(legal(ask, &free, &reading, None), &|j| free[j] && Some(j) != ask.active));
                }
            }
            Task::Repair => {
                // A stop may change only when the model is unsure it should stay. Of those stops,
                // "top" moves the ones whose leading mode is a move and "wide" moves every one; the
                // third reading is top at half strength.
                let (mut top, mut wide) = (current.clone(), current.clone());
                for (&stop, keep) in stops.iter().zip(&out.keep_probability) {
                    let stay = inputs.shown[stop];
                    if stay && *keep >= margins.stop {
                        continue;
                    }
                    let ranked = self.modes(&mix[stop], if stay && !asked { self.favour } else { 0.0 });
                    if let Some(first) = ranked.iter().map(|row| row.0).find(|c| !(stay && *c == anchor)) {
                        wide[stop] = oklab(display(mix[stop].means[first]));
                        if ranked[0].0 == first {
                            top[stop] = wide[stop];
                        }
                    }
                }
                let half: Vec<Lab> = (0..n).map(|k| std::array::from_fn(|c| current[k][c] + 0.5 * (top[k][c] - current[k][c]))).collect();
                for reading in [top, half, wide] {
                    candidates.push(settle(legal(ask, &free, &reading, None), &|j| inputs.free[j]));
                }
            }
        }
        if candidates.is_empty() {
            return out;
        }
        let in_lab = |rgb: &[Srgb]| -> Vec<Lab> { rgb.iter().map(|c| oklab(*c)).collect() };
        let palettes: Vec<Vec<Lab>> = std::iter::once(ask.rgb).chain(candidates.iter().map(Vec::as_slice)).map(in_lab).collect();
        let mut scores: Vec<f64> = if ask.task == Task::Repair {
            // A repair is rated by the model's doubt about the stops it changes: the mean, over
            // those stops, of the log-odds that the stop should not stay as it is.
            let odds: Vec<f64> = out.keep_probability.iter().map(|keep| {
                let keep = keep.clamp(1e-6, 1.0 - 1e-6);
                ((1.0 - keep) / keep).ln()
            }).collect();
            palettes.iter().map(|palette| {
                let changed: Vec<f64> = stops.iter().zip(&odds).filter(|(k, _)| (0..3).any(|c| (palette[**k][c] - current[**k][c]).abs() > 1e-12)).map(|(_, odds)| *odds).collect();
                if changed.is_empty() { 0.0 } else { changed.iter().sum::<f64>() / changed.len() as f64 }
            }).collect()
        } else {
            self.score(&inputs, ask.positions, &condition, &palettes)
        };
        if ask.task == Task::Follow {
            scores.iter_mut().for_each(|s| *s /= stops.len() as f64);
        }
        let duplicate = match ask.radius {
            Some(radius) if radius > 0.0 && radius < 2.0 => self.duplicate.min(0.5 * radius),
            _ => self.duplicate,
        };
        // an option has to look different from the palette as it stands and from every better option
        let (near, twin) = (margins.visible.max(duplicate), DISTINCT.max(duplicate));
        let mut order: Vec<usize> = (1..palettes.len()).collect();
        order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]));
        let mut chosen: Vec<usize> = Vec::new();
        for i in order {
            let repeats = far(&palettes[i], &palettes[0]) < near || chosen.iter().any(|&j| far(&palettes[i], &palettes[j]) < twin);
            if scores[i] - scores[0] > margins.spoil && !repeats {
                chosen.push(i);
            }
        }
        out.keep = chosen.first().is_none_or(|&best| scores[best] - scores[0] <= margins.keep);
        chosen.truncate(ask.count);
        let mut candidates: Vec<Option<Vec<Srgb>>> = candidates.into_iter().map(Some).collect();
        out.options = chosen.into_iter().filter_map(|i| Some(Candidate { rgb: candidates[i - 1].take()?, score: scores[i] - scores[0] })).collect();
        out
    }
}

/// The flags of the pass a request's candidates come from. `single` frees only the active stop,
/// which is how a one-stop request is asked whatever its scope. A repair has no active stop, and
/// a stop its owner just set is held: context for the others, never a target.
fn inputs(ask: &Ask, free: &[bool], single: bool) -> Inputs {
    let n = ask.rgb.len();
    let mut free = free.to_vec();
    let mut pinned = ask.pins.to_vec();
    let mut recent: Vec<bool> = (0..n).map(|i| ask.recent.contains(&i)).collect();
    let mut shown = vec![true; n];
    let repair = ask.task == Task::Repair;
    let active = ask.active.filter(|_| !repair);
    if let Some(k) = active {
        match ask.task {
            // the stop just committed is the context the others answer
            Task::Follow => (free[k], pinned[k], recent[k]) = (false, true, true),
            Task::Masked => shown[k] = false,
            Task::Refine | Task::Repair => {}
        }
        if single {
            for j in 0..n {
                (free[j], pinned[j], recent[j]) = (free[j] && j == k, pinned[j] && j != k, recent[j] && j == k);
            }
        }
    }
    for k in 0..n {
        if repair && recent[k] {
            (free[k], pinned[k]) = (false, true);
        }
        pinned[k] = pinned[k] && !free[k];
    }
    let lab: Vec<Lab> = ask.rgb.iter().map(|c| oklab(*c)).collect();
    Inputs {
        previous: ask.before.map_or_else(|| lab.clone(), |before| before.iter().map(|c| oklab(*c)).collect()),
        lab,
        pinned,
        free,
        known: shown.clone(),
        shown,
        active,
        recent,
    }
}

/// Visible stop `j` is a neighbour of `i` unless a nearer visible stop lies the same way.
fn neighbours(positions: &[f64], visible: &[bool], cone: f64) -> Near {
    let n = positions.len();
    let delta: Vec<Vec<f64>> = (0..n).map(|i| (0..n).map(|j| positions[j] - positions[i]).collect()).collect();
    let distance: Vec<Vec<f64>> = delta.iter().map(|row| row.iter().map(|d| d.abs()).collect()).collect();
    let unit = |i: usize, j: usize| delta[i][j] / distance[i][j].max(1e-12);
    let near = (0..n).map(|i| (0..n).map(|j| {
        visible[j] && j != i && !(0..n).any(|k| visible[k] && k != i && distance[i][k] < distance[i][j] && unit(i, j) * unit(i, k) > cone)
    }).collect()).collect();
    Near { near, distance, delta }
}

/// For each stop, the inverse-distance mean of its neighbours' colours and whether it has any;
/// mid grey without one.
fn interpolate(lab: &[Lab], found: &Near) -> (Vec<Lab>, Vec<bool>) {
    let n = lab.len();
    let mut between = vec![GREY; n];
    let mut has = vec![false; n];
    for i in 0..n {
        let (mut total, mut sum) = (0.0, [0.0; 3]);
        for j in (0..n).filter(|j| found.near[i][*j]) {
            let pull = 1.0 / found.distance[i][j].max(1e-9);
            total += pull;
            for c in 0..3 {
                sum[c] += pull * lab[j][c];
            }
            has[i] = true;
        }
        if has[i] {
            between[i] = sum.map(|v| v / total.max(1e-300));
        }
    }
    (between, has)
}

/// `sample` (OKLab) as display colours: stops that are not free stay as they are, and free ones
/// are pulled to within `radius` of where they are now. If any stop cannot be brought inside the
/// budget the whole palette comes back unchanged.
fn project(current: &[Srgb], free: &[bool], radius: Option<f64>, sample: &[Lab]) -> Vec<Srgb> {
    let targets: Vec<Srgb> = (0..current.len()).map(|k| if free[k] { display(sample[k]) } else { current[k] }).collect();
    let Some(radius) = radius.filter(|r| *r < 2.0) else { return targets };
    if radius == 0.0 {
        return current.to_vec();
    }
    let bound = radius * (radius / (radius + 2e-7));
    let mut rgb = current.to_vec();
    for k in (0..current.len()).filter(|k| free[*k]) {
        let base = oklab(current[k]);
        let target = oklab(targets[k]);
        let shrink = (bound / (distance(target, base) + 1e-30)).min(1.0);
        let (to, from) = (display(std::array::from_fn(|d| base[d] + (target[d] - base[d]) * shrink)), display(base));
        rgb[k] = std::array::from_fn(|d| (current[k][d] + to[d] - from[d]).clamp(0.0, 1.0));
        for _ in 0..16 {
            let far = distance(oklab(rgb[k]), base);
            if far <= radius + 1e-10 {
                break;
            }
            let ratio = (bound / (far + 1e-30)).min(1.0);
            rgb[k] = std::array::from_fn(|d| current[k][d] + ratio * (rgb[k][d] - current[k][d]));
        }
        if distance(oklab(rgb[k]), base) > radius + 1e-9 {
            return current.to_vec();
        }
    }
    rgb
}

/// [`project`] for a request. A stop with no colour of its own (`masked`) has its budget measured
/// from what its neighbours imply; when one stop voids the palette, each stop meant to move is
/// clipped on its own along the line to its target; a stop that was not asked to move comes back
/// exactly.
fn legal(ask: &Ask, free: &[bool], sample: &[Lab], masked: Option<usize>) -> Vec<Srgb> {
    let n = ask.rgb.len();
    let mut origin = ask.rgb.to_vec();
    if let Some(active) = masked.filter(|a| free[*a]) {
        let lab: Vec<Lab> = ask.rgb.iter().map(|c| oklab(*c)).collect();
        let others: Vec<bool> = (0..n).map(|i| i != active).collect();
        let (between, found) = interpolate(&lab, &neighbours(ask.positions, &others, 0.5));
        if found[active] {
            origin[active] = display(between[active]);
        }
    }
    let mut rgb = project(&origin, free, ask.radius, sample);
    if let Some(radius) = ask.radius.filter(|r| *r > 0.0 && *r < 2.0) {
        let targets: Vec<Srgb> = sample.iter().map(|c| display(*c)).collect();
        let wanted: Vec<bool> = (0..n).map(|i| free[i] && (0..3).any(|c| (targets[i][c] - origin[i][c]).abs() > 1e-6)).collect();
        if wanted.contains(&true) && rgb == origin {
            for i in (0..n).filter(|i| wanted[*i]) {
                let base = oklab(origin[i]);
                let at = |part: f64| -> Srgb { std::array::from_fn(|c| origin[i][c] + part * (targets[i][c] - origin[i][c])) };
                let (mut low, mut high) = (0.0f64, 1.0f64);
                for _ in 0..30 {
                    let mid = (low + high) / 2.0;
                    if distance(oklab(at(mid)), base) <= radius { low = mid } else { high = mid }
                }
                rgb[i] = at(low);
            }
        }
    }
    for i in 0..n {
        if distance(sample[i], oklab(ask.rgb[i])) < 1e-9 {
            rgb[i] = ask.rgb[i];
        }
    }
    rgb
}

/// A scene in the model's terms.
struct Frame {
    rgb: Vec<Srgb>,
    /// The palette before the edit in progress, when the selected stop is the one it recoloured.
    was: Option<Vec<Srgb>>,
    positions: Vec<f64>,
    style: Vec<f64>,
    pins: Vec<bool>,
    /// The pins of a one-stop request: the palette's ends too, unless one is the stop asked about,
    /// as in the requests the margins were fitted on.
    ends: Vec<bool>,
    here: Vec<bool>,
    everywhere: Vec<bool>,
    active: usize,
}

impl Frame {
    fn ask(&self) -> Ask<'_> {
        Ask { task: Task::Refine, rgb: &self.rgb, before: None, positions: &self.positions, pins: &self.pins, scope: &self.everywhere, active: Some(self.active), recent: &[], radius: Some(BUDGET), count: 1, intent: None, style: &self.style }
    }

    /// The request about the selected stop alone: a colour for a stop that has none, or another
    /// colour for one that does.
    fn stop(&self, open: bool) -> Ask<'_> {
        let task = if open { Task::Masked } else { Task::Refine };
        Ask { task, before: self.was.as_deref().filter(|_| !open), pins: &self.ends, scope: &self.here, ..self.ask() }
    }
}

/// Where the model would put a colour at the selected stop's place if the stop had none: its
/// answer as a density over OKLab. It reads the rest of the palette and not the stop's own colour,
/// so it holds still while that colour is dragged. Drawn behind a picker it is the ground a
/// suggestion stands on.
pub struct Terrain {
    /// Weight, mean and spread of each part.
    parts: Vec<(f64, Lab, Lab)>,
}

impl Terrain {
    /// The density at its highest part's centre, in the units of [`Terrain::density`]: what a
    /// colour's density is to be compared with. Zero for an empty terrain.
    #[must_use]
    pub fn peak(&self) -> f32 {
        self.parts.iter().map(|part| self.density(part.1.map(|v| v as f32))).fold(0.0, f32::max)
    }

    /// The density at an OKLab colour, up to a constant factor.
    #[must_use]
    pub fn density(&self, lab: [f32; 3]) -> f32 {
        self.parts.iter().map(|(weight, mean, sigma)| {
            let z: f64 = (0..3).map(|d| ((f64::from(lab[d]) - mean[d]) / sigma[d]).powi(2)).sum();
            weight * (-0.5 * z).exp() / (sigma[0] * sigma[1] * sigma[2])
        }).sum::<f64>() as f32
    }
}

impl Learned {
    fn frame(&self, scene: &Scene) -> Option<Frame> {
        let n = scene.stops.len();
        let active = scene.selected;
        if n < 2 || active >= n || scene.stops.windows(2).any(|pair| pair[1].at <= pair[0].at) {
            return None;
        }
        let unit = |c: Rgb| [f64::from(c.r) / 255.0, f64::from(c.g) / 255.0, f64::from(c.b) / 255.0];
        let support: Vec<Vec<Lab>> = scene.library.iter().map(|palette| palette.iter().map(|c| oklab(unit(*c))).collect()).collect();
        let pins: Vec<bool> = (0..n).map(|i| scene.pinned(i)).collect();
        let edited = scene.before.as_ref().filter(|before| scene.selected_is_edited() && before.iter().zip(&scene.stops).all(|(was, now)| was.at == now.at));
        Some(Frame {
            rgb: scene.stops.iter().map(|s| unit(s.col)).collect(),
            was: edited.map(|before| before.iter().map(|s| unit(s.col)).collect()),
            positions: scene.stops.iter().map(|s| 2.0 * f64::from(s.at) - 1.0).collect(),
            style: self.style_vector(&support),
            ends: (0..n).map(|i| i != active && (pins[i] || i == 0 || i == n - 1)).collect(),
            pins,
            here: (0..n).map(|i| i == active).collect(),
            everywhere: vec![true; n],
            active,
        })
    }

    /// The model's view of where a colour at the selected stop's place belongs. `None` when it has
    /// no say: a held stop, a palette of one.
    #[must_use]
    pub fn terrain(&self, scene: &Scene) -> Option<Terrain> {
        let frame = self.frame(scene).filter(|frame| !frame.pins[frame.active])?;
        let ask = frame.stop(true);
        let inputs = inputs(&ask, &frame.here, true);
        let mix = self.run(&inputs, ask.positions, &Condition { intent: None, style: ask.style }).swap_remove(frame.active);
        // A part narrower than a picker can show is widened to that: drawn at its true spread it
        // is a spike no pointer finds, and it would dwarf the ground around it.
        let parts = softmax(&mix.logits).into_iter().zip(mix.means.iter().zip(&mix.sigmas)).map(|(weight, (mean, sigma))| (weight, *mean, sigma.map(|s| s.max(GRAIN)))).collect();
        Some(Terrain { parts })
    }

    /// The free stops a repair would change first: the model is unsure they should stay, and
    /// would move them even with "leave it" favoured. Each comes with the colour it would rather
    /// see there.
    fn doubts(&self, ask: &Ask) -> Vec<(usize, Srgb)> {
        let n = ask.rgb.len();
        let free: Vec<bool> = (0..n).map(|i| ask.scope[i] && !ask.pins[i]).collect();
        let Some(margins) = self.repair.filter(|_| n >= 2) else { return Vec::new() };
        let inputs = inputs(ask, &free, false);
        if !inputs.free.contains(&true) {
            return Vec::new();
        }
        let mix = self.run(&inputs, ask.positions, &Condition { intent: None, style: ask.style });
        let mut sample = inputs.lab.clone();
        let mut moved = Vec::new();
        for stop in (0..n).filter(|stop| inputs.free[*stop]) {
            let leading = self.modes(&mix[stop], self.favour)[0].0;
            if leading != self.components && self.keep_probability(&mix[stop]) < margins.stop {
                sample[stop] = oklab(display(mix[stop].means[leading]));
                moved.push(stop);
            }
        }
        let rgb = legal(ask, &free, &sample, None);
        moved.into_iter().filter(|&stop| distance(oklab(rgb[stop]), inputs.lab[stop]) >= self.duplicate).map(|stop| (stop, rgb[stop])).collect()
    }
}

impl Engine for Learned {
    /// What the model has to say about `scene`:
    ///
    /// - A stop nobody has coloured yet (`scene.open`): colours it could take.
    /// - The stop the edit in progress has recoloured: colours it could be instead. A short move is
    ///   put to the model as a request made from the colour the stop had (roughly this way, about
    ///   this far), which it answers readily; a long one as the colour now tried, which it argues
    ///   with only when sure. The colour is never called settled: told that, the model never argues.
    /// - How the other stops could answer that edit, and a tidy of the stops beside it.
    /// - Its doubts, edit or no edit: each stop it would move even with "leave it" favoured, as an
    ///   offer that changes that stop alone ([`Scope::Aside`] unless the stop is the selected one).
    ///   The stop being edited is held and never doubted.
    ///
    /// Other palettes in `scene.library` set the style.
    fn offers(&self, scene: &Scene) -> Vec<Offer> {
        let Some(frame) = self.frame(scene) else { return Vec::new() };
        let (active, ask) = (frame.active, frame.ask());
        let byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
        let colour = |c: &Srgb| Rgb::new(byte(c[0]), byte(c[1]), byte(c[2]));
        let mut out = Vec::new();
        let mut offer = |answer: Answer, id: &'static str, title: &'static str, scope: Scope| {
            for option in answer.options.iter().filter(|_| !answer.keep) {
                let stops = scene.stops.iter().zip(&option.rgb).zip(&frame.rgb).map(|((stop, new), old)| {
                    let mut stop = *stop;
                    if scope == Scope::Stop || new != old {
                        stop.col = colour(new);
                    }
                    stop
                }).collect();
                out.push(Offer { id, title, scope, stops, interp: scene.interp });
            }
        };
        if scene.open {
            if !frame.pins[active] {
                offer(self.answer(&Ask { count: 3, ..frame.stop(true) }), "fits", "a colour for this stop", Scope::Stop);
            }
            return out;
        }
        let held: Vec<bool> = (0..frame.rgb.len()).map(|i| frame.pins[i] || (i == active && frame.was.is_some())).collect();
        if let Some(was) = &frame.was {
            if !frame.pins[active] {
                let (from, to) = (oklab(was[active]), oklab(frame.rgb[active]));
                let answer = if distance(from, to) <= NUDGE {
                    let mut origin = frame.rgb.clone();
                    origin[active] = was[active];
                    let wanted: Lab = std::array::from_fn(|d| to[d] - from[d]);
                    self.answer(&Ask { rgb: &origin, before: None, count: 2, intent: Some(wanted), ..frame.stop(false) })
                } else {
                    self.answer(&Ask { count: 2, ..frame.stop(false) })
                };
                offer(answer, "fits", "fits this palette", Scope::Stop);
            }
            offer(self.answer(&Ask { task: Task::Follow, before: Some(was), ..ask }), "follow", "carry the change through", Scope::Palette);
            offer(self.answer(&Ask { task: Task::Repair, before: Some(was), pins: &held, active: None, recent: &[active], ..ask }), "tidy", "tidy the rest", Scope::Palette);
        }
        let recent: &[usize] = if frame.was.is_some() { &[active] } else { &[] };
        for (stop, better) in self.doubts(&Ask { task: Task::Repair, before: frame.was.as_deref(), pins: &held, active: None, recent, ..ask }) {
            let mut stops = scene.stops.clone();
            stops[stop].col = colour(&better);
            let scope = if stop == active { Scope::Stop } else { Scope::Aside };
            out.push(Offer { id: "doubt", title: "would sit better here", scope, stops, interp: scene.interp });
        }
        out
    }
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn add(into: &mut [f64], from: &[f64]) {
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

/// The tanh approximation, which is the one the model was trained with.
fn gelu(mut x: Vec<f64>) -> Vec<f64> {
    for v in &mut x {
        *v = 0.5 * *v * (1.0 + ((2.0 / std::f64::consts::PI).sqrt() * (*v + 0.044715 * v.powi(3))).tanh());
    }
    x
}

/// Layer normalisation: population variance, epsilon inside the root.
fn norm(x: &[f64], scale: &Linear) -> Vec<f64> {
    let mean = x.iter().sum::<f64>() / x.len() as f64;
    let spread = (x.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / x.len() as f64 + 1e-5).sqrt();
    x.iter().zip(scale.weight.iter().zip(&scale.bias)).map(|(v, (w, b))| (v - mean) / spread * w + b).collect()
}

fn softmax(logits: &[f64]) -> Vec<f64> {
    let top = logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let weight: Vec<f64> = logits.iter().map(|l| (l - top).exp()).collect();
    let total: f64 = weight.iter().sum();
    weight.into_iter().map(|w| w / total).collect()
}

fn distance(a: Lab, b: Lab) -> f64 {
    (0..3).map(|d| (a[d] - b[d]).powi(2)).sum::<f64>().sqrt()
}

fn oklab(rgb: [f64; 3]) -> Lab {
    let [r, g, b] = rgb.map(|v| if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) });
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
    ]
}

/// OKLab to linear sRGB, unclamped.
fn linear_rgb(lab: Lab) -> [f64; 3] {
    let l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]).powi(3);
    let m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]).powi(3);
    let s = (lab[0] - 0.0894841775 * lab[1] - 1.291485548 * lab[2]).powi(3);
    [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
    ]
}

/// The nearest displayable sRGB colour: chroma is reduced until it fits, lightness and hue kept.
/// The search is the reference's twelve three-way rounds, so both land on the same colour.
fn display(lab: Lab) -> [f64; 3] {
    let lab = [lab[0].clamp(0.0, 1.0), lab[1], lab[2]];
    let fits = |rgb: [f64; 3]| rgb.iter().all(|v| (0.0..=1.0).contains(v));
    let at = |cut: f64| linear_rgb([lab[0], lab[1] * cut, lab[2] * cut]);
    let mut rgb = linear_rgb(lab);
    if !fits(rgb) {
        let (mut low, mut high) = (0.0f64, 1.0f64);
        for _ in 0..12 {
            let mid = (low + high) / 2.0;
            let (inner, outer) = ((low + mid) / 2.0, (mid + high) / 2.0);
            if fits(at(mid)) {
                if fits(at(outer)) { low = outer } else { (low, high) = (mid, outer) }
            } else if fits(at(inner)) {
                (low, high) = (inner, mid);
            } else {
                high = inner;
            }
        }
        rgb = at(low);
    }
    rgb.map(|v| {
        let v = v.clamp(0.0, 1.0);
        (if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }).clamp(0.0, 1.0)
    })
}

/// Natural-log density of an OKLab colour under a mixture.
fn log_density(mix: &Mixture, colour: Lab) -> f64 {
    let top = mix.logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let total = mix.logits.iter().map(|l| (l - top).exp()).sum::<f64>().ln();
    let parts: Vec<f64> = (0..mix.logits.len()).map(|c| {
        let mut part = mix.logits[c] - top - total - 1.5 * std::f64::consts::TAU.ln();
        for d in 0..3 {
            let z = (colour[d] - mix.means[c][d]) / mix.sigmas[c][d];
            part -= 0.5 * z * z;
        }
        part - (mix.sigmas[c][0].ln() + mix.sigmas[c][1].ln() + mix.sigmas[c][2].ln())
    }).collect();
    let peak = parts.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    peak + parts.iter().map(|p| (p - peak).exp()).sum::<f64>().ln()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrum::{Interp, Stop};

    const SMALL: &[u8] = include_bytes!("../../data/colour-model.golden.lumacore");
    const SMALL_CASES: &str = include_str!("../../data/colour-model.golden.jsonl");

    /// Run every gradient request in `cases` through `model` and compare with the recorded
    /// answers. Returns, per task, (requests, answers that change something, options).
    fn check(model: &Learned, cases: &str) -> HashMap<String, [usize; 3]> {
        let mut seen: HashMap<String, [usize; 3]> = HashMap::new();
        let triple = |v: &serde_json::Value| -> [f64; 3] { std::array::from_fn(|d| v[d].as_f64().unwrap()) };
        let triples = |v: &serde_json::Value| -> Vec<[f64; 3]> { v.as_array().unwrap().iter().map(triple).collect() };
        let numbers = |v: &serde_json::Value| -> Vec<f64> { v.as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect() };
        let flags = |v: &serde_json::Value| -> Vec<bool> { v.as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect() };
        for line in cases.lines() {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = &case["input"];
            // stops on a line that all count: the only requests Neuron makes
            if input["geometry"]["kind"] != "ordered_gradient" || !input["weights"].is_null() {
                continue;
            }
            let id = case["id"].as_str().unwrap();
            let task = match input["task"].as_str().unwrap() {
                "masked" => Task::Masked,
                "refine" => Task::Refine,
                "follow" => Task::Follow,
                _ => Task::Repair,
            };
            let rgb = triples(&input["rgb"]);
            let before = (!input["before_rgb"].is_null()).then(|| triples(&input["before_rgb"]));
            let support: Vec<Vec<Lab>> = input["support_lab"].as_array().map(|set| set.iter().map(triples).collect()).unwrap_or_default();
            let style = model.style_vector(&support);
            // Luma's own vectors keep the style with the other checkpoints; the small set beside the input
            let recorded = if case["checkpoint"].is_null() { &case["style"] } else { &case["checkpoint"]["style"] };
            if let Some(recorded) = recorded.as_array() {
                assert!(recorded.iter().zip(&style).all(|(want, got)| (want.as_f64().unwrap() - got).abs() < 1e-9), "{id}: style");
            }
            let recent: Vec<usize> = input["recent"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
            let answer = model.answer(&Ask {
                task,
                rgb: &rgb,
                before: before.as_deref(),
                positions: &numbers(&input["positions"]),
                pins: &flags(&input["pins"]),
                scope: &flags(&input["scope"]),
                active: input["active"].as_u64().map(|v| v as usize),
                recent: &recent,
                radius: input["radius"].as_f64(),
                count: input["count"].as_u64().unwrap_or(3) as usize,
                intent: (!input["intent"].is_null()).then(|| triple(&input["intent"])),
                style: &style,
            });
            let output = &case["output"];
            let expected = output["options"].as_array().unwrap();
            assert_eq!(answer.offerable, output["offerable"].as_bool().unwrap(), "{id}: offerable");
            assert_eq!(answer.keep, output["keep"].as_bool().unwrap(), "{id}: keep");
            assert_eq!(answer.options.len(), expected.len(), "{id}: option count");
            for (got, want) in answer.options.iter().zip(expected) {
                assert!((got.score - want["score"].as_f64().unwrap()).abs() < 1e-7, "{id}: score {} vs {}", got.score, want["score"]);
                let palette = triples(&want["rgb"]);
                assert!(got.rgb.iter().zip(&palette).all(|(a, b)| (0..3).all(|d| (a[d] - b[d]).abs() < 1e-9)), "{id}: {:?} vs {palette:?}", got.rgb);
            }
            if let Some(recorded) = output["keep_probability"].as_array() {
                assert_eq!(recorded.len(), answer.keep_probability.len(), "{id}: free stops");
                assert!(recorded.iter().zip(&answer.keep_probability).all(|(want, got)| (want.as_f64().unwrap() - got).abs() < 1e-9), "{id}: keep probability");
            }
            let tally = seen.entry(input["task"].as_str().unwrap().to_string()).or_default();
            tally[0] += 1;
            tally[1] += usize::from(!answer.keep);
            tally[2] += answer.options.len();
        }
        seen
    }

    #[test]
    fn matches_the_reference_implementation() {
        let model = Learned::parse(SMALL).expect("the small bundle loads");
        let seen = check(&model, SMALL_CASES);
        for task in ["masked", "refine", "follow", "repair"] {
            let [requests, changes, options] = seen[task];
            assert!(requests >= 40 && changes >= 5 && changes < requests && options >= 10, "{task} covers both answers: {requests} requests, {changes} changes, {options} options");
        }
    }

    /// The same check for a trained bundle against the vectors Luma publishes with it. Point
    /// `NEURON_LUMA_BRIDGE` at the folder holding `core-v4.3.lumacore` and `golden-v4.3.jsonl`.
    #[test]
    #[ignore = "needs a Luma bridge folder in NEURON_LUMA_BRIDGE"]
    fn matches_luma_vectors() {
        let folder = std::path::PathBuf::from(std::env::var_os("NEURON_LUMA_BRIDGE").expect("NEURON_LUMA_BRIDGE"));
        let model = Learned::parse(&std::fs::read(folder.join("core-v4.3.lumacore")).unwrap()).unwrap();
        let started = std::time::Instant::now();
        let seen = check(&model, &std::fs::read_to_string(folder.join("golden-v4.3.jsonl")).unwrap());
        println!("{}: {seen:?} in {:?}", model.name(), started.elapsed());
        assert!(seen.values().map(|tally| tally[0]).sum::<usize>() >= 30);
    }

    #[test]
    fn a_bundle_that_does_not_describe_its_payload_is_refused() {
        assert!(Learned::parse(SMALL).is_ok());
        let mut damaged = SMALL.to_vec();
        *damaged.last_mut().unwrap() ^= 1;
        assert!(Learned::parse(&damaged).is_err(), "a changed weight");
        assert!(Learned::parse(&SMALL[..SMALL.len() - 4]).is_err(), "a cut payload");
        assert!(Learned::parse(&SMALL[..40]).is_err(), "a cut header");
        let mut other = SMALL.to_vec();
        other[0] = b'X';
        assert!(Learned::parse(&other).is_err(), "another container");
        other = SMALL.to_vec();
        other[8] = 2;
        assert!(Learned::parse(&other).is_err(), "a later container version");
        assert!(Learned::parse(b"").is_err());
        assert!(Learned::load(Path::new("no-such-folder")).unwrap().is_none(), "no model is not an error");
    }

    fn scene(colours: &[(u8, u8, u8)]) -> Scene {
        let last = (colours.len() - 1).max(1) as f32;
        let stops = colours.iter().enumerate().map(|(i, c)| Stop::new(Rgb::new(c.0, c.1, c.2), i as f32 / last)).collect();
        Scene { stops, interp: Interp::Oklab, before: None, selected: 1, pins: Vec::new(), open: false, library: Vec::new() }
    }

    #[test]
    fn the_engine_speaks_only_about_an_edit_or_a_new_stop() {
        let model = Learned::parse(SMALL).unwrap();
        let mut s = scene(&[(12, 45, 34), (200, 40, 190), (67, 190, 116), (206, 229, 128)]);
        assert!(model.offers(&s).iter().all(|offer| offer.id == "doubt"), "a resting palette draws doubts at most");
        s.before = Some(s.stops.clone());
        assert!(model.offers(&s).iter().all(|offer| offer.id == "doubt"), "an edit that has not touched the selected stop");
        s.stops.truncate(1);
        s.before.as_mut().unwrap().truncate(1);
        s.selected = 0;
        s.open = true;
        assert!(model.offers(&s).is_empty(), "a palette of one");
    }

    /// Every offer the engine makes over a spread of edits keeps to what its kind may touch.
    #[test]
    fn each_kind_of_offer_changes_only_what_it_may() {
        let model = Learned::parse(SMALL).unwrap();
        let base = [(12, 45, 34), (60, 120, 80), (67, 190, 116), (150, 210, 120), (206, 229, 128)];
        let mut kinds: HashMap<&str, usize> = HashMap::new();
        for (edited, colour) in [(1, (200, 40, 190)), (2, (70, 180, 120)), (3, (20, 20, 160)), (0, (240, 240, 240)), (2, (255, 0, 0)), (4, (90, 60, 30))] {
            for pin in [None, Some(2usize)] {
                let mut s = scene(&base);
                s.before = Some(s.stops.clone());
                s.selected = edited;
                s.stops[edited].col = Rgb::new(colour.0, colour.1, colour.2);
                s.pins = (0..5).map(|i| Some(i) == pin).collect();
                s.library = vec![vec![Rgb::new(250, 120, 20), Rgb::new(20, 60, 200)]];
                for offer in model.offers(&s) {
                    *kinds.entry(offer.id).or_default() += 1;
                    assert_eq!(offer.stops.len(), 5);
                    let changed: Vec<usize> = (0..5).filter(|&i| offer.stops[i].col != s.stops[i].col).collect();
                    assert!(changed.iter().all(|i| Some(*i) != pin), "{}: a pinned stop moved", offer.id);
                    match offer.scope {
                        Scope::Stop => assert!(changed.iter().all(|i| *i == edited), "{}: {changed:?}", offer.id),
                        Scope::Palette => assert!(!changed.contains(&edited), "{}: the edited stop moved", offer.id),
                        Scope::Aside => assert!(changed.len() == 1 && changed[0] != edited, "{}: {changed:?}", offer.id),
                    }
                }
                let terrain = model.terrain(&s);
                assert_eq!(terrain.is_some(), pin != Some(edited), "a terrain for a free stop, none for a held one");
                if let Some(terrain) = terrain {
                    let here = oklab([f64::from(s.stops[edited].col.r) / 255.0, f64::from(s.stops[edited].col.g) / 255.0, f64::from(s.stops[edited].col.b) / 255.0]).map(|v| v as f32);
                    assert!(terrain.density(here).is_finite() && terrain.density(here) >= 0.0);
                    assert!(terrain.peak() > 0.0 && terrain.peak().is_finite());
                    s.stops[edited].col = Rgb::new(1, 2, 3);
                    let moved = model.terrain(&s).unwrap();
                    assert_eq!(moved.density(here), terrain.density(here), "the stop's own colour does not shape its terrain");
                    s.stops[edited].col = Rgb::new(colour.0, colour.1, colour.2);
                }
                // a resting palette: only doubts, one stop each, never a held one
                s.before = None;
                for offer in model.offers(&s) {
                    *kinds.entry(offer.id).or_default() += 1;
                    let changed: Vec<usize> = (0..5).filter(|&i| offer.stops[i].col != s.stops[i].col).collect();
                    assert!(offer.id == "doubt" && changed.len() == 1 && Some(changed[0]) != pin, "{}: {changed:?}", offer.id);
                    assert_eq!(offer.scope == Scope::Stop, changed[0] == edited);
                }
                s.open = true;
                for offer in model.offers(&s) {
                    *kinds.entry("new").or_default() += 1;
                    assert!(offer.scope == Scope::Stop && (0..5).all(|i| i == edited || offer.stops[i] == s.stops[i]), "only the new stop changes");
                }
            }
        }
        let seen = |kind: &str| kinds.get(kind).copied().unwrap_or(0);
        assert!(seen("fits") > 0 && seen("new") > 0 && seen("follow") + seen("tidy") > 0, "the spread reaches every scope: {kinds:?}");

    }

    #[test]
    fn a_suggestion_stays_inside_the_edit_budget() {
        let current = [[0.2, 0.5, 0.3], [0.9, 0.9, 0.2]];
        let free = [true, false];
        for target in [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0], [0.21, 0.5, 0.3]] {
            let got = project(&current, &free, Some(BUDGET), &[oklab(target), oklab([0.0, 0.0, 0.0])]);
            assert!(distance(oklab(got[0]), oklab(current[0])) <= BUDGET + 1e-9, "{target:?} -> {got:?}");
            assert!(got[0].iter().all(|v| (0.0..=1.0).contains(v)));
            assert_eq!(got[1], current[1], "a stop that is not free stays");
        }
        let near = project(&current, &free, Some(BUDGET), &[oklab([0.21, 0.5, 0.3]), oklab(current[1])]);
        assert!((0..3).all(|d| (near[0][d] - [0.21, 0.5, 0.3][d]).abs() < 1e-6), "a near colour is not moved: {near:?}");
        assert_eq!(project(&current, &free, Some(0.0), &[oklab([1.0, 0.0, 0.0]), oklab(current[1])]), current.to_vec());
    }
}
