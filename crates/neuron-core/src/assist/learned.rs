// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A learned [`Engine`]: a small attention network that reads a palette and says which colours
//! the stop being worked on could be instead, and whether any of them beats leaving it alone.
//!
//! This is a port of Luma's palette core (layout 3, the `refine` request) from its bridge
//! specification. The network is data: [`Learned::load`] reads a `.lumacore` bundle from a folder,
//! and no trained weights are compiled in. Each stop becomes one token (its colour, position,
//! flags, the colour its neighbours imply, and what it was before the edit). The answer for a stop
//! is a mixture of Gaussians in OKLab whose last component sits on the stop's current colour, so
//! "keep" is always one of the answers. Candidates are the mixture's modes, held within an edit
//! budget of the current colour; each is scored against the current palette by the density the
//! network gives every free stop, and the bundle's margins decide what is worth offering.
//!
//! Colour conversion and the budget arithmetic are `f64` and follow the reference step for step:
//! their thresholds sit below `f32` resolution, and a colour on the gamut edge must land on the
//! same side in both.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::{Engine, Offer, Scene, Scope, MAX_OFFERS};
use crate::lighting::Rgb;

/// The bundle [`Learned::load`] looks for in its folder.
pub const BUNDLE: &str = "colour-model.lumacore";
const MAGIC: &[u8; 8] = b"LUMACORE";
const FORMAT: &str = "luma.core-bridge.v1";
/// Token width of the one layout this reads.
const FEATURES: usize = 40;
const SCALE: [f64; 3] = [2.0, 4.0, 4.0];
const SHIFT: [f64; 3] = [0.5, 0.0, 0.0];
/// Modes read from the active stop's mixture before scoring, whatever the caller asks to see.
const MODES: usize = 5;
/// How far, in OKLab, a suggestion may sit from the colour it would replace. The policy margins
/// were fitted on requests with this budget.
const BUDGET: f64 = 0.18;

type Lab = [f64; 3];

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
/// component is the stop's current colour.
struct Mixture {
    logits: Vec<f64>,
    means: Vec<Lab>,
    sigmas: Vec<Lab>,
}

/// A refine request: a palette in display sRGB (`0..1`), in position order.
struct Request<'a> {
    rgb: &'a [[f64; 3]],
    /// The palette before the last edit; `None` when there was none.
    before: Option<&'a [[f64; 3]]>,
    /// Stop positions on `-1..=1`.
    positions: &'a [f64],
    pins: &'a [bool],
    /// The stops the request may change. Their colours count toward a candidate's score.
    free: &'a [bool],
    active: usize,
    /// Stops whose colour their owner has settled on.
    recent: &'a [bool],
    /// The edit budget; `None` for no limit.
    radius: Option<f64>,
}

/// A colour the active stop could take, and how the network rates it against keeping the current
/// one (log-density difference in nats; positive prefers the candidate).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Candidate {
    rgb: [f64; 3],
    score: f64,
}

struct Decision {
    /// True when nothing clears the margin over leaving the palette alone. Options may still be
    /// listed: they are what the model would show if asked anyway.
    keep: bool,
    options: Vec<Candidate>,
}

/// A loaded colour model.
pub struct Learned {
    name: String,
    heads: usize,
    components: usize,
    sigma: [f64; 2],
    merge: f64,
    duplicate: f64,
    spoil: f64,
    keep: f64,
    embed: [Linear; 2],
    blocks: Vec<Block>,
    norm: Linear,
    head: [Linear; 2],
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
    /// little-endian `f32`. Refuses an unknown container, format or layout, a payload whose length
    /// or hash the header does not name, and arrays of the wrong shape.
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
        let (width, heads, blocks, components) = (size("width")?, size("heads")?, size("blocks")?, size("components")?);
        let policy = &config["policy"];
        if config["layout"] != 3 || width % heads != 0 || policy["measure"] != "density" || policy["joint"] != "pseudo" {
            return Err("unsupported colour model layout or policy".into());
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
            let bytes = 4 * shape.iter().product::<usize>();
            let raw = offset.checked_add(bytes).and_then(|end| payload.get(*offset..end)).filter(|_| found == shape);
            let raw = raw.ok_or_else(|| format!("bundle array {name} has the wrong shape"))?;
            Ok(raw.chunks_exact(4).map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))).collect())
        };
        let linear = |name: &str, out: usize, inp: usize| -> Result<Linear, String> {
            Ok(Linear { weight: take(&format!("{name}.weight"), &[out, inp])?, bias: take(&format!("{name}.bias"), &[out])? })
        };
        let scale = |name: &str| -> Result<Linear, String> {
            Ok(Linear { weight: take(&format!("{name}.weight"), &[width])?, bias: take(&format!("{name}.bias"), &[width])? })
        };
        Ok(Learned {
            name: doc["name"].as_str().unwrap_or("colour model").to_string(),
            heads,
            components,
            sigma: [number(&config["sigma"][0], "sigma")?, number(&config["sigma"][1], "sigma")?],
            merge: number(&config["merge"], "merge")?,
            duplicate: number(&policy["duplicate"], "policy.duplicate")?,
            spoil: number(&policy["refine"]["spoil"], "policy.refine.spoil")?,
            keep: number(&policy["refine"]["keep"], "policy.refine.keep")?,
            embed: [linear("embed.0", width, FEATURES)?, linear("embed.2", width, width)?],
            blocks: (0..blocks).map(|i| {
                let p = format!("blocks.{i}");
                Ok(Block {
                    norm1: scale(&format!("{p}.norm1"))?,
                    qkv: linear(&format!("{p}.qkv"), 3 * width, width)?,
                    proj: linear(&format!("{p}.proj"), width, width)?,
                    norm2: scale(&format!("{p}.norm2"))?,
                    wide: linear(&format!("{p}.wide"), 2 * width, width)?,
                    narrow: linear(&format!("{p}.narrow"), width, 2 * width)?,
                })
            }).collect::<Result<_, String>>()?,
            norm: scale("norm")?,
            head: [linear("head.0", width, width)?, linear("head.2", 7 * components + 4, width)?],
        })
    }

    /// The name its bundle gives it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// One token per stop. Every stop shows a colour and has a previous one.
    fn tokens(&self, lab: &[Lab], previous: &[Lab], positions: &[f64], pinned: &[bool], free: &[bool], active: usize, recent: &[bool]) -> Vec<[f64; FEATURES]> {
        let between = neighbours(lab, positions);
        let flag = |on: bool| f64::from(u8::from(on));
        (0..lab.len()).map(|i| {
            let mut t = [0.0; FEATURES];
            let relative = positions[i] - positions[active];
            for d in 0..3 {
                t[d] = (lab[i][d] - SHIFT[d]) * SCALE[d];
                t[32 + d] = (previous[i][d] - SHIFT[d]) * SCALE[d];
                t[37 + d] = (lab[i][d] - previous[i][d]) * SCALE[d];
            }
            (t[3], t[4], t[5], t[6], t[7], t[8]) = (positions[i], flag(pinned[i]), flag(free[i]), 1.0, flag(i == active), relative);
            for (w, octave) in [1.0, 2.0, 4.0, 8.0].into_iter().enumerate() {
                let (at, from) = (positions[i] * std::f64::consts::PI * octave, relative * std::f64::consts::PI * octave / 2.0);
                (t[9 + w], t[13 + w], t[17 + w], t[21 + w]) = (at.sin(), at.cos(), from.sin(), from.cos());
            }
            if let Some(mid) = between[i] {
                for d in 0..3 {
                    t[25 + d] = (mid[d] - SHIFT[d]) * SCALE[d];
                    t[29 + d] = (lab[i][d] - mid[d]) * SCALE[d];
                }
                t[28] = 1.0;
            }
            (t[35], t[36]) = (1.0, flag(recent[i]));
            t
        }).collect()
    }

    /// The network: tokens in, one mixture per stop out.
    fn forward(&self, tokens: &[[f64; FEATURES]]) -> Vec<Mixture> {
        let k = self.components;
        let mut x: Vec<Vec<f64>> = tokens.iter().map(|t| self.embed[1].apply(&gelu(self.embed[0].apply(t)))).collect();
        let width = x.first().map_or(0, Vec::len);
        let part = width / self.heads;
        for block in &self.blocks {
            // queries, keys and values, each a third of the row; a head owns `part` of each third
            let qkv: Vec<Vec<f64>> = x.iter().map(|row| block.qkv.apply(&norm(row, &block.norm1))).collect();
            for (i, row) in x.iter_mut().enumerate() {
                let mut mixed = vec![0.0; width];
                for head in 0..self.heads {
                    let of = |third: usize| third * width + head * part..third * width + (head + 1) * part;
                    let score: Vec<f64> = qkv.iter().map(|other| dot(&qkv[i][of(0)], &other[of(1)]) / (part as f64).sqrt()).collect();
                    for (other, w) in qkv.iter().zip(softmax(&score)) {
                        for (m, v) in mixed[head * part..(head + 1) * part].iter_mut().zip(&other[of(2)]) {
                            *m += w * v;
                        }
                    }
                }
                add(row, &block.proj.apply(&mixed));
            }
            for row in &mut x {
                let hidden = block.narrow.apply(&gelu(block.wide.apply(&norm(row, &block.norm2))));
                add(row, &hidden);
            }
        }
        let (low, high) = (self.sigma[0].ln(), self.sigma[1].ln());
        x.iter().zip(tokens).map(|(row, token)| {
            let out = self.head[1].apply(&gelu(self.head[0].apply(&norm(row, &self.norm))));
            let base: Lab = std::array::from_fn(|d| token[d] / SCALE[d] + SHIFT[d]);
            Mixture {
                logits: out[..=k].to_vec(),
                means: (0..=k).map(|c| if c < k { std::array::from_fn(|d| base[d] + out[k + 1 + 3 * c + d] / SCALE[d]) } else { base }).collect(),
                sigmas: (0..=k).map(|c| std::array::from_fn(|d| (low + (high - low) / (1.0 + (-out[4 * k + 1 + 3 * c + d]).exp())).exp())).collect(),
            }
        }).collect()
    }

    /// The mixture's components as distinct colours, heaviest first: components closer than the
    /// merge distance to a group's representative join it, and a group the keep component joins is
    /// represented by it. Returns each group's representative component.
    fn modes(&self, mix: &Mixture) -> Vec<usize> {
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
        kept.sort_by(|a, b| b.1.total_cmp(&a.1));
        kept.into_iter().map(|row| row.0).collect()
    }

    /// Candidates for the active stop, scored against leaving the palette as it is.
    fn refine(&self, request: &Request, count: usize) -> Decision {
        let n = request.rgb.len();
        let active = request.active;
        if n < 2 || active >= n || !request.free[active] {
            return Decision { keep: true, options: Vec::new() };
        }
        let lab: Vec<Lab> = request.rgb.iter().map(|c| oklab(*c)).collect();
        let previous: Vec<Lab> = request.before.map_or_else(|| lab.clone(), |b| b.iter().map(|c| oklab(*c)).collect());
        let here: Vec<bool> = (0..n).map(|i| i == active).collect();

        // the pass the candidates come from: only the active stop is asked about
        let pinned: Vec<bool> = (0..n).map(|i| request.pins[i] && i != active).collect();
        let settled: Vec<bool> = (0..n).map(|i| i == active && request.recent[active]).collect();
        let asked = self.forward(&self.tokens(&lab, &previous, request.positions, &pinned, &here, active, &settled));
        let current = request.rgb[active];
        let candidates: Vec<[f64; 3]> = self.modes(&asked[active]).into_iter().take(MODES).map(|component| {
            // "leave it" is the stop's colour itself, not its round trip through the display
            let mean = asked[active].means[component];
            let legal = if component == self.components { mean } else { oklab(display(mean)) };
            within(current, display(legal), request.radius)
        }).collect();

        // The pass they are scored under asks about every free stop. A candidate is rated by the
        // density of each free stop's colour in a palette where the other free stops already hold
        // the candidate's colours; it differs from the request only at the active stop, so that
        // stop's term comes from the request as it stands and the others from the candidate palette.
        let pinned: Vec<bool> = (0..n).map(|i| request.pins[i] && !request.free[i]).collect();
        let judge = |palette: &[Lab]| self.forward(&self.tokens(palette, &previous, request.positions, &pinned, request.free, active, request.recent));
        let alone = request.free == here.as_slice() && request.recent == settled.as_slice();
        let standing = if alone { asked } else { judge(&lab) };
        let free = || (0..n).filter(|&j| request.free[j]);
        let keep_score: f64 = free().map(|j| log_density(&standing[j], lab[j])).sum();
        let mut scored: Vec<(Candidate, Lab)> = candidates.into_iter().map(|rgb| {
            let colour = oklab(rgb);
            let mut palette = lab.clone();
            palette[active] = colour;
            let around = if free().count() == 1 { Vec::new() } else { judge(&palette) };
            let score: f64 = free().map(|j| if j == active { log_density(&standing[j], colour) } else { log_density(&around[j], lab[j]) }).sum();
            (Candidate { rgb, score: score - keep_score }, colour)
        }).collect();
        scored.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
        let mut kept: Vec<(Candidate, Lab)> = Vec::new();
        for (option, colour) in scored {
            let repeats = distance(colour, lab[active]) < self.duplicate || kept.iter().any(|other| distance(colour, other.1) < self.duplicate);
            if option.score > self.spoil && !repeats {
                kept.push((option, colour));
            }
        }
        Decision {
            keep: kept.first().is_none_or(|best| best.0.score <= self.keep),
            options: kept.into_iter().take(count).map(|row| row.0).collect(),
        }
    }
}

impl Engine for Learned {
    /// Speaks only about a free stop the edit in progress has recoloured, and only when the model
    /// rates an alternative above keeping it. The colour is put to the model as one being tried:
    /// told a colour is settled it never argues, and a colour still under the pointer is not. The
    /// ends are marked pinned when they are not the stop asked about, as in the requests the
    /// model's margins were fitted on.
    fn offers(&self, scene: &Scene) -> Vec<Offer> {
        let n = scene.stops.len();
        let Some(before) = scene.before.as_ref().filter(|b| b.len() == n && b.iter().zip(&scene.stops).all(|(x, y)| x.at == y.at)) else {
            return Vec::new();
        };
        let active = scene.selected;
        if n < 2 || active >= n || scene.pinned(active) || !scene.selected_is_edited() || scene.stops.windows(2).any(|pair| pair[1].at <= pair[0].at) {
            return Vec::new();
        }
        let unit = |c: Rgb| [f64::from(c.r) / 255.0, f64::from(c.g) / 255.0, f64::from(c.b) / 255.0];
        let rgb: Vec<[f64; 3]> = scene.stops.iter().map(|s| unit(s.col)).collect();
        let was: Vec<[f64; 3]> = before.iter().map(|s| unit(s.col)).collect();
        let positions: Vec<f64> = scene.stops.iter().map(|s| 2.0 * f64::from(s.at) - 1.0).collect();
        let pins: Vec<bool> = (0..n).map(|i| i != active && (scene.pinned(i) || i == 0 || i == n - 1)).collect();
        let here: Vec<bool> = (0..n).map(|i| i == active).collect();
        let request = Request { rgb: &rgb, before: Some(&was), positions: &positions, pins: &pins, free: &here, active, recent: &vec![false; n], radius: Some(BUDGET) };
        let decision = self.refine(&request, MAX_OFFERS);
        if decision.keep {
            return Vec::new();
        }
        decision.options.iter().map(|option| {
            let mut stops = scene.stops.clone();
            let byte = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) as u8;
            stops[active].col = Rgb::new(byte(option.rgb[0]), byte(option.rgb[1]), byte(option.rgb[2]));
            Offer { id: "learned", title: "fits this palette", scope: Scope::Stop, stops, interp: scene.interp }
        }).collect()
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

/// Natural-log density of an OKLab colour under a mixture.
fn log_density(mix: &Mixture, colour: Lab) -> f64 {
    let weight = softmax(&mix.logits);
    let parts: Vec<f64> = (0..weight.len()).map(|c| {
        let z: f64 = (0..3).map(|d| ((colour[d] - mix.means[c][d]) / mix.sigmas[c][d]).powi(2)).sum();
        let volume: f64 = mix.sigmas[c].iter().map(|s| s.ln()).sum();
        weight[c].ln() - 0.5 * z - volume - 1.5 * std::f64::consts::TAU.ln()
    }).collect();
    let peak = parts.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    peak + parts.iter().map(|p| (p - peak).exp()).sum::<f64>().ln()
}

/// For each stop, the colour its nearest neighbours on either side imply at its position
/// (one-sided at the ends), or `None` for a palette of one. This is the part of a token that
/// assumes stops lie on a line.
fn neighbours(lab: &[Lab], positions: &[f64]) -> Vec<Option<Lab>> {
    let last = lab.len().saturating_sub(1);
    (0..lab.len()).map(|i| match (i > 0, i < last) {
        (true, true) => {
            let (start, end) = (positions[i - 1], positions[i + 1]);
            let part = ((positions[i] - start) / if end > start { end - start } else { 1.0 }).clamp(0.0, 1.0);
            Some(std::array::from_fn(|d| lab[i - 1][d] * (1.0 - part) + lab[i + 1][d] * part))
        }
        (true, false) => Some(lab[i - 1]),
        (false, true) => Some(lab[i + 1]),
        (false, false) => None,
    }).collect()
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

/// `target` pulled to within `radius` (OKLab) of `current`, both display sRGB. A colour that
/// cannot be brought inside the budget comes back as `current`.
fn within(current: [f64; 3], target: [f64; 3], radius: Option<f64>) -> [f64; 3] {
    let Some(radius) = radius.filter(|r| *r < 2.0) else { return target };
    if radius == 0.0 {
        return current;
    }
    let base = oklab(current);
    let bound = radius * (radius / (radius + 2e-7));
    let from_base = |rgb: [f64; 3]| distance(oklab(rgb), base);
    let aim = oklab(target);
    let shrink = (bound / (distance(aim, base) + 1e-30)).min(1.0);
    let moved = display(std::array::from_fn(|d| base[d] + (aim[d] - base[d]) * shrink));
    let home = display(base);
    let mut rgb: [f64; 3] = std::array::from_fn(|d| (current[d] + moved[d] - home[d]).clamp(0.0, 1.0));
    for _ in 0..16 {
        let far = from_base(rgb);
        if far <= radius + 1e-10 {
            break;
        }
        let ratio = (bound / (far + 1e-30)).min(1.0);
        rgb = std::array::from_fn(|d| current[d] + ratio * (rgb[d] - current[d]));
    }
    if from_base(rgb) > radius + 1e-9 { current } else { rgb }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spectrum::{Interp, Stop};

    const SMALL: &[u8] = include_bytes!("../../data/colour-model.golden.lumacore");
    const SMALL_CASES: &str = include_str!("../../data/colour-model.golden.jsonl");

    /// Run every refine request in `cases` through `model` and compare with the recorded answers.
    /// Returns (requests, keeps, options).
    fn check(model: &Learned, cases: &str) -> (usize, usize, usize) {
        let (mut requests, mut keeps, mut options) = (0, 0, 0);
        for line in cases.lines() {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let input = &case["input"];
            if input["task"] != "refine" {
                continue;
            }
            // Luma's own vectors nest the answer; the small set keeps it beside the input.
            let (answer, whole) = if case["output"].is_null() { (&case, false) } else { (&case["output"], true) };
            let id = case["id"].as_str().unwrap();
            let palette = |v: &serde_json::Value| -> Vec<[f64; 3]> {
                v.as_array().unwrap().iter().map(|c| std::array::from_fn(|d| c[d].as_f64().unwrap())).collect()
            };
            let flags = |v: &serde_json::Value| -> Vec<bool> { v.as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect() };
            let rgb = palette(&input["rgb"]);
            let before = (!input["before_rgb"].is_null()).then(|| palette(&input["before_rgb"]));
            let positions: Vec<f64> = input["positions"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
            let (scope, pins) = (flags(&input["scope"]), flags(&input["pins"]));
            let free: Vec<bool> = scope.iter().zip(&pins).map(|(s, p)| *s && !p).collect();
            let active = input["active"].as_u64().unwrap() as usize;
            let recent: Vec<bool> = (0..rgb.len()).map(|i| input["recent"].as_array().unwrap().iter().any(|r| r.as_u64() == Some(i as u64))).collect();
            assert!(input["weights"].is_null(), "{id}: refine requests carry no stop weights");
            let decision = model.refine(&Request {
                rgb: &rgb,
                before: before.as_deref(),
                positions: &positions,
                pins: &pins,
                free: &free,
                active,
                recent: &recent,
                radius: input["radius"].as_f64(),
            }, input["count"].as_u64().unwrap() as usize);
            let expected = answer["options"].as_array().unwrap();
            assert_eq!(decision.keep, answer["keep"].as_bool().unwrap(), "{id}: keep");
            assert_eq!(decision.options.len(), expected.len(), "{id}: option count");
            for (got, want) in decision.options.iter().zip(expected) {
                assert!((got.score - want["score"].as_f64().unwrap()).abs() < 1e-6, "{id}: score {} vs {}", got.score, want["score"]);
                let colour: [f64; 3] = std::array::from_fn(|d| if whole { want["rgb"][active][d].as_f64().unwrap() } else { want["rgb"][d].as_f64().unwrap() });
                assert!((0..3).all(|d| (got.rgb[d] - colour[d]).abs() < 1e-9), "{id}: {:?} vs {colour:?}", got.rgb);
            }
            requests += 1;
            keeps += usize::from(decision.keep);
            options += decision.options.len();
        }
        (requests, keeps, options)
    }

    #[test]
    fn matches_the_reference_implementation() {
        let model = Learned::parse(SMALL).expect("the small bundle loads");
        let (requests, keeps, options) = check(&model, SMALL_CASES);
        assert!(requests >= 150 && options >= 25 && keeps < requests, "the cases cover both answers: {requests} requests, {keeps} keeps, {options} options");
    }

    /// The same check for a trained bundle against the vectors Luma publishes with it. Point
    /// `NEURON_LUMA_BRIDGE` at the folder holding `palette-core-v2.lumacore` and `golden-v2.jsonl`.
    #[test]
    #[ignore = "needs a Luma bridge folder in NEURON_LUMA_BRIDGE"]
    fn matches_luma_vectors() {
        let folder = std::path::PathBuf::from(std::env::var_os("NEURON_LUMA_BRIDGE").expect("NEURON_LUMA_BRIDGE"));
        let model = Learned::parse(&std::fs::read(folder.join("palette-core-v2.lumacore")).unwrap()).unwrap();
        let (requests, keeps, options) = check(&model, &std::fs::read_to_string(folder.join("golden-v2.jsonl")).unwrap());
        println!("{}: {requests} refine requests, {keeps} keeps, {options} options", model.name());
        assert!(requests >= 100);
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

    #[test]
    fn the_engine_only_speaks_about_a_free_stop_being_edited() {
        let model = Learned::parse(SMALL).unwrap();
        let stops: Vec<Stop> = [(12, 45, 34), (200, 40, 190), (67, 190, 116), (206, 229, 128)]
            .iter().enumerate().map(|(i, c)| Stop::new(Rgb::new(c.0, c.1, c.2), i as f32 / 3.0)).collect();
        let mut scene = Scene { stops: stops.clone(), interp: Interp::Oklab, before: None, selected: 1, pins: Vec::new() };
        assert!(model.offers(&scene).is_empty(), "a resting palette");
        scene.before = Some(stops.clone());
        assert!(model.offers(&scene).is_empty(), "an edit that has not touched the selected stop");
        scene.before.as_mut().unwrap()[1].col = Rgb::new(40, 120, 80);
        for offer in model.offers(&scene) {
            assert!(offer.scope == Scope::Stop && offer.stops.len() == 4);
            assert!((0..4).all(|i| i == 1 || offer.stops[i] == scene.stops[i]), "only the selected stop changes");
        }
        scene.pins = vec![false, true];
        assert!(model.offers(&scene).is_empty(), "a pinned stop");
        scene.pins.clear();
        scene.stops.truncate(1);
        scene.before.as_mut().unwrap().truncate(1);
        scene.selected = 0;
        assert!(model.offers(&scene).is_empty(), "a palette of one");
    }

    #[test]
    fn a_suggestion_stays_inside_the_edit_budget() {
        let current = [0.2, 0.5, 0.3];
        for target in [[1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0, 1.0], [0.21, 0.5, 0.3]] {
            let got = within(current, target, Some(BUDGET));
            assert!(distance(oklab(got), oklab(current)) <= BUDGET + 1e-9, "{target:?} -> {got:?}");
            assert!(got.iter().all(|v| (0.0..=1.0).contains(v)));
        }
        let near = within(current, [0.21, 0.5, 0.3], Some(BUDGET));
        assert!((0..3).all(|d| (near[d] - [0.21, 0.5, 0.3][d]).abs() < 1e-6), "a near colour is not moved: {near:?}");
        assert_eq!(within(current, [1.0, 0.0, 0.0], None), [1.0, 0.0, 0.0]);
        assert_eq!(within(current, [1.0, 0.0, 0.0], Some(0.0)), current);
    }
}
