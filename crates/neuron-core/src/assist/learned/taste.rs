// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The taste head and a person's own taste: what Luma measures in its blind duels on top of the
//! core (Luma's bridge specification, section 20).
//!
//! The core proposes; the head chooses. For a request the core lists its readings (the modes of a
//! stop, a companion edit's graded family and the owner's change fading out across the palette, a
//! repair's few versions), and the head, a small network trained on blind judges' choices, gives
//! each one a probability that a judge would accept it and a pick score, and gives "keep" a pick
//! score on the same scale. The best option is offered only when it is likely acceptable, looks
//! different from the palette as it stands and from every better option, and beats keep.
//!
//! A person's own taste comes from the picking journal Neuron writes when asked to
//! ([`super::super::journal`]): every slate taken or dismissed is a few ordered pairs, and a small
//! linear preference over how an option changes the palette is fitted to them. It is added to the
//! head's pick scores against keep's zero, so it reorders the options and, for a person who keeps
//! dismissing them, offers less. It is used once the journal holds a few decided slates, and nothing
//! leaves the machine.
//!
//! Everything is `f64` and follows Luma's runtime step for step, so the golden vectors Luma
//! publishes with a head check every stage: the readings, both token groups, each member's logits,
//! the personal term and the decision.

use sha2::{Digest, Sha256};

use super::{
    display, distance, gelu, inputs, legal, log_density, norm, oklab, softmax, Answer, Ask, Candidate, Condition, Inputs, Lab,
    Learned, Linear, Mixture, Srgb, Task, DISTINCT, MOVE, VISIBLE,
};

/// The head bundle [`Learned::load`] looks for beside the core's.
pub const BUNDLE: &str = "colour-taste.lumataste";
/// The picking journal a person's taste is read from, beside the bundles.
pub const JOURNAL: &str = "picker-sessions.jsonl";
const MAGIC: &[u8; 8] = b"LUMATAST";
const FORMAT: &str = "luma.taste-bridge.v1";
const TASKS: [&str; 5] = ["masked", "refine", "follow", "profile", "repair"];
/// Values per stop of the core's opinion, and of the request-level group.
const CORE: usize = 21;
const REQUEST: usize = 22;
/// Modes a one-stop request lists.
const READINGS: usize = 8;
/// Features of an option's change, for a person's taste.
const PERSONAL: usize = 8;
/// The strengths a companion edit's carry is also listed at.
const GRADES: [(f64, &str); 2] = [(0.5, "carry@0.5"), (0.25, "carry@0.25")];

/// The palette before a slate was shown, and the slate.
type Shown = (Vec<Srgb>, Vec<Vec<Srgb>>);
/// Both token groups of a request (per row: per stop, then once) and its selected stop.
type Tokens = (Vec<Vec<[f64; CORE]>>, Vec<[f64; REQUEST]>, Option<usize>);

/// What a request asks, in the head's words: the core's four tasks, and a person's request about a
/// stop ("profile"), which the core reads as a refine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Masked,
    Refine,
    Follow,
    /// A person's request about a stop, read by the core as a refine. Luma's lab asks it; Neuron's
    /// picker asks refines, and a known person's taste reaches every request instead.
    #[cfg_attr(not(test), expect(dead_code, reason = "the head knows the task; only the reference checks ask it"))]
    Profile,
    Repair,
}

impl Kind {
    fn index(self) -> usize {
        match self {
            Kind::Masked => 0,
            Kind::Refine => 1,
            Kind::Follow => 2,
            Kind::Profile => 3,
            Kind::Repair => 4,
        }
    }

    pub(super) fn of(task: Task) -> Kind {
        match task {
            Task::Masked => Kind::Masked,
            Task::Refine => Kind::Refine,
            Task::Follow => Kind::Follow,
            Task::Repair => Kind::Repair,
        }
    }

    fn task(self) -> Task {
        match self {
            Kind::Masked => Task::Masked,
            Kind::Refine | Kind::Profile => Task::Refine,
            Kind::Follow => Task::Follow,
            Kind::Repair => Task::Repair,
        }
    }
}

struct Member {
    in_core: Linear,
    in_request: Linear,
    embed: Linear,
    norm: Linear,
    head_in: Linear,
    head_out: Linear,
}

/// How a person's taste is fitted and weighed.
struct Rules {
    scale: [f64; PERSONAL],
    ridge: f64,
    weight: f64,
    min_decided: usize,
}

/// A loaded taste head.
pub struct Taste {
    name: String,
    /// The sha256 of the core weights the head was trained with.
    core: String,
    members: Vec<Member>,
    acceptable: f64,
    margin: f64,
    visible: [f64; 5],
    spreads: Vec<(f64, f64)>,
    rules: Option<Rules>,
}

/// A person's taste: a weight per feature of an option's change.
#[derive(Clone, Debug, PartialEq)]
pub struct Person {
    weights: [f64; PERSONAL],
}

/// The head's scores for one request: per candidate, then for "keep".
pub(super) struct Scores {
    pub(super) acceptable: Vec<f64>,
    pub(super) pick: Vec<f64>,
    pub(super) keep: f64,
}

impl Taste {
    /// Build a head from a `.lumataste` bundle: the core's container with its own magic and format.
    /// Refuses an unknown container or format, a payload its header does not describe, a design
    /// this build does not run, and arrays of the wrong shape.
    pub fn parse(bundle: &[u8]) -> Result<Taste, String> {
        let word = |at: usize| bundle.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
        if bundle.get(..8) != Some(MAGIC.as_slice()) || word(8) != Some(1) {
            return Err("not a taste bundle this build reads".into());
        }
        let header = word(12).and_then(|len| bundle.get(16..16usize.checked_add(len)?)).ok_or("taste bundle header is cut short")?;
        let payload = bundle.get((16 + header.len()).div_ceil(64) * 64..).ok_or("taste bundle payload is missing")?;
        let doc: serde_json::Value = serde_json::from_slice(header).map_err(|e| format!("taste bundle header: {e}"))?;
        if doc["format"] != FORMAT {
            return Err(format!("not a {FORMAT} bundle"));
        }
        let digest: String = Sha256::digest(payload).iter().map(|b| format!("{b:02x}")).collect();
        if doc["payload_bytes"].as_u64() != Some(payload.len() as u64) || doc["payload_sha256"] != digest.as_str() {
            return Err("taste bundle payload does not match its header".into());
        }
        let config = &doc["config"];
        let size = |what: &str| config[what].as_u64().map(|v| v as usize).filter(|v| (1..=4096).contains(v)).ok_or_else(|| format!("taste config: {what}"));
        let (width, hidden) = (size("width")?, size("hidden")?);
        // Lists of words compare whole: an entry that is not a word fails the comparison.
        fn words(v: &serde_json::Value) -> Option<Vec<&str>> {
            v.as_array()?.iter().map(serde_json::Value::as_str).collect()
        }
        if words(&config["groups"]).as_deref() != Some(&["core", "request"][..]) || config["blocks"] != 0 {
            return Err("unsupported taste head design".into());
        }
        let constants = &doc["constants"];
        if words(&constants["tasks"]).as_deref() != Some(&TASKS[..]) || constants["move"].as_f64() != Some(MOVE) {
            return Err("taste bundle constants differ from this build's".into());
        }
        // Every array lies inside the payload, apart from every other, and is as long as its shape.
        let mut arrays = std::collections::HashMap::new();
        let mut spans = Vec::new();
        for a in doc["arrays"].as_array().ok_or("taste bundle header: arrays")? {
            let shape: Option<Vec<usize>> = a["shape"].as_array().and_then(|s| s.iter().map(|v| v.as_u64().and_then(|v| usize::try_from(v).ok())).collect());
            let (Some(name), Some(shape), Some(offset), "<f4") = (a["name"].as_str(), shape, a["offset"].as_u64(), a["dtype"].as_str().unwrap_or("")) else {
                return Err("taste bundle header: array entry".into());
            };
            let offset = usize::try_from(offset).map_err(|_| "taste bundle header: array entry")?;
            let end = shape.iter().try_fold(4usize, |total, side| total.checked_mul(*side)).and_then(|bytes| offset.checked_add(bytes));
            let end = end.filter(|end| *end <= payload.len()).ok_or_else(|| format!("taste bundle array {name} lies outside the payload"))?;
            spans.push((offset, end));
            if arrays.insert(name.to_string(), (shape, offset, end)).is_some() {
                return Err(format!("taste bundle lists array {name} twice"));
            }
        }
        spans.sort_unstable();
        if spans.windows(2).any(|w| w[1].0 < w[0].1) {
            return Err("taste bundle arrays overlap".into());
        }
        let take = |name: &str, shape: &[usize]| -> Result<Vec<f64>, String> {
            let (found, offset, end) = arrays.get(name).ok_or_else(|| format!("taste bundle has no array {name}"))?;
            if found != shape {
                return Err(format!("taste bundle array {name} has the wrong shape"));
            }
            let values: Vec<f64> = payload[*offset..*end].chunks_exact(4).map(|b| f64::from(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))).collect();
            if values.iter().any(|v| !v.is_finite()) {
                return Err(format!("taste bundle array {name} holds a value that is not a number"));
            }
            Ok(values)
        };
        let linear = |name: &str, out: usize, inp: usize| -> Result<Linear, String> {
            Ok(Linear { weight: take(&format!("{name}.weight"), &[out, inp])?, bias: take(&format!("{name}.bias"), &[out])? })
        };
        let count = doc["members"].as_u64().filter(|m| (1..=64).contains(m)).ok_or("taste bundle: members")? as usize;
        let members = (0..count).map(|i| {
            let p = if count > 1 { format!("m{i}.") } else { String::new() };
            Ok(Member {
                in_core: linear(&format!("{p}in_core"), width, CORE)?,
                in_request: linear(&format!("{p}in_request"), width, REQUEST)?,
                embed: linear(&format!("{p}embed"), width, width)?,
                norm: Linear { weight: take(&format!("{p}norm.weight"), &[width])?, bias: take(&format!("{p}norm.bias"), &[width])? },
                head_in: linear(&format!("{p}head_in"), hidden, 3 * width)?,
                head_out: linear(&format!("{p}head_out"), 3, hidden)?,
            })
        }).collect::<Result<Vec<Member>, String>>()?;
        let policy = &doc["policy"];
        // Numbers are finite, and a rule that is present but not a number is refused, not defaulted.
        let number = |v: &serde_json::Value, what: &str| v.as_f64().filter(|v| v.is_finite()).ok_or_else(|| format!("taste bundle: {what}"));
        let positive = |v: &serde_json::Value, what: &str| number(v, what).and_then(|v| if v > 0.0 { Ok(v) } else { Err(format!("taste bundle: {what}")) });
        let mut visible = [VISIBLE; 5];
        for (i, task) in TASKS.iter().enumerate() {
            match &policy["visible"][task] {
                serde_json::Value::Null => {}
                v => visible[i] = number(v, "visible floor")?.max(VISIBLE),
            }
        }
        let spreads = constants["spreads"].as_array().ok_or("taste bundle: spreads")?.iter()
            .map(|s| Ok((positive(&s[0], "spread width")?, number(&s[1], "spread amount")?))).collect::<Result<Vec<_>, String>>()?;
        let rules = match &constants["personal"] {
            serde_json::Value::Null => None,
            p => {
                let scale = p["scale"].as_array().ok_or("taste bundle: personal scale")?.iter()
                    .map(|v| positive(v, "personal scale")).collect::<Result<Vec<f64>, String>>()?;
                let scale: [f64; PERSONAL] = scale.try_into().map_err(|_| "taste bundle: personal scale")?;
                let min_decided = match &p["min_decided"] {
                    serde_json::Value::Null => 3,
                    v => v.as_u64().and_then(|v| usize::try_from(v).ok()).ok_or("taste bundle: personal min_decided")?,
                };
                Some(Rules { scale, ridge: positive(&p["ridge"], "personal ridge")?, weight: number(&p["weight"], "personal weight")?, min_decided })
            }
        };
        Ok(Taste {
            name: doc["name"].as_str().unwrap_or("taste head").to_string(),
            core: doc["core"]["sha256"].as_str().filter(|s| !s.is_empty()).ok_or("taste bundle: core")?.to_ascii_lowercase(),
            members,
            acceptable: number(&policy["acceptable"], "policy acceptable")?,
            margin: number(&policy["margin"], "policy margin")?,
            visible,
            spreads,
            rules,
        })
    }

    /// The name its bundle gives it.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The core weights (sha256) this head reads, which must be the loaded core's.
    #[must_use]
    pub fn core(&self) -> &str {
        &self.core
    }

    /// A person's taste from their picking journal, or `None` before they have taken or dismissed
    /// enough slates (or when the bundle carries no rules for it).
    #[must_use]
    pub fn person(&self, journal: &str) -> Option<Person> {
        let rules = self.rules.as_ref()?;
        let (pairs, decided) = journal_pairs(journal);
        if decided < rules.min_decided || pairs.is_empty() {
            return None;
        }
        Some(Person { weights: fit(&pairs, &rules.scale, rules.ridge) }).filter(|p| p.weights.iter().all(|w| w.is_finite()))
    }

    /// Member-averaged logits per row (row 0 is "keep"): acceptable, pick, needs work.
    fn logits(&self, core: &[Vec<[f64; CORE]>], request: &[[f64; REQUEST]], active: Option<usize>) -> Vec<[f64; 3]> {
        let rows = core.len();
        let mut total = vec![[0.0; 3]; rows];
        for member in &self.members {
            for (row, out) in total.iter_mut().enumerate() {
                let lifted = member.in_request.apply(&request[row]);
                let tokens: Vec<Vec<f64>> = core[row].iter().map(|stop| {
                    let mut x = member.in_core.apply(stop);
                    x.iter_mut().zip(&lifted).for_each(|(a, b)| *a += b);
                    norm(&member.embed.apply(&gelu(x)), &member.norm)
                }).collect();
                let width = tokens[0].len();
                let mut pooled = vec![0.0; 3 * width];
                for d in 0..width {
                    pooled[d] = tokens.iter().map(|t| t[d]).sum::<f64>() / tokens.len() as f64;
                    pooled[width + d] = tokens.iter().map(|t| t[d]).fold(f64::NEG_INFINITY, f64::max);
                    pooled[2 * width + d] = active.map_or(0.0, |a| tokens[a][d]);
                }
                let logit = member.head_out.apply(&gelu(member.head_in.apply(&pooled)));
                for k in 0..3 {
                    out[k] += logit[k] / self.members.len() as f64;
                }
            }
        }
        total
    }
}

impl Person {
    /// A person with these weights (for a host that keeps them elsewhere, and for the checks).
    #[must_use]
    pub fn new(weights: [f64; PERSONAL]) -> Person {
        Person { weights }
    }

    /// The fitted weights, one per feature of an option's change.
    #[must_use]
    pub fn weights(&self) -> &[f64; PERSONAL] {
        &self.weights
    }

    /// The person's preference for each palette against `current` (all `0..1` sRGB), on the scale
    /// of the head's pick scores, with keep at zero: it reorders the options and, for a person who
    /// keeps dismissing them, makes keeping likelier.
    fn preference(&self, rules: &Rules, current: &[Srgb], palettes: &[Vec<Srgb>]) -> Vec<f64> {
        let here: Vec<Lab> = current.iter().map(|c| oklab(*c)).collect();
        palettes.iter().map(|p| {
            let option: Vec<Lab> = p.iter().map(|c| oklab(*c)).collect();
            let f = features(&here, &option);
            rules.weight * (0..PERSONAL).map(|k| f[k] / rules.scale[k] * self.weights[k]).sum::<f64>()
        }).collect()
    }
}

/// Lightness range, mean lightness, mean and peak chroma, chroma spread, warmth and roughness.
fn describe(lab: &[Lab]) -> [f64; 7] {
    let n = lab.len() as f64;
    let chroma: Vec<f64> = lab.iter().map(|c| (c[1] * c[1] + c[2] * c[2]).sqrt()).collect();
    let mean = |v: &mut dyn Iterator<Item = f64>| v.sum::<f64>() / n;
    let chroma_mean = chroma.iter().sum::<f64>() / n;
    let spread = (chroma.iter().map(|c| (c - chroma_mean).powi(2)).sum::<f64>() / n).sqrt();
    let rough = if lab.len() > 2 {
        (0..lab.len() - 2).map(|i| (lab[i + 2][0] - 2.0 * lab[i + 1][0] + lab[i][0]).abs()).sum::<f64>() / (lab.len() - 2) as f64
    } else {
        0.0
    };
    let lightness = lab.iter().map(|c| c[0]);
    [
        lab.iter().map(|c| c[0]).fold(f64::NEG_INFINITY, f64::max) - lab.iter().map(|c| c[0]).fold(f64::INFINITY, f64::min),
        mean(&mut lightness.clone()),
        chroma_mean,
        chroma.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        spread,
        mean(&mut lab.iter().map(|c| c[2])) + 0.5 * mean(&mut lab.iter().map(|c| c[1])),
        rough,
    ]
}

/// How an option changes a palette (both OKLab): [`describe`]'s differences and the largest move.
fn features(current: &[Lab], option: &[Lab]) -> [f64; PERSONAL] {
    let (a, b) = (describe(current), describe(option));
    let mut out = [0.0; PERSONAL];
    for k in 0..7 {
        out[k] = b[k] - a[k];
    }
    out[7] = current.iter().zip(option).map(|(x, y)| distance(*x, *y)).fold(0.0, f64::max);
    out
}

/// Ordered pairs from a journal in Luma's session-log format, as feature differences (winner minus
/// loser) with weights, and the number of slates taken or dismissed. The palette is replayed from
/// the session's start through drags, undos and taken suggestions.
///
/// A record this format could not have written is passed over: a colour outside `0..1`, an empty
/// palette, a stop or a choice that is not there. A malformed undo leaves the palette as it was.
fn journal_pairs(journal: &str) -> (Vec<([f64; PERSONAL], f64)>, usize) {
    let unit = |v: &serde_json::Value| v.as_f64().filter(|v| (0.0..=1.0).contains(v));
    let triple = |v: &serde_json::Value| -> Option<Srgb> { Some([unit(&v[0])?, unit(&v[1])?, unit(&v[2])?]) };
    let palette_of = |v: &serde_json::Value| -> Option<Vec<Srgb>> {
        let stops: Vec<Srgb> = v.as_array()?.iter().map(triple).collect::<Option<_>>()?;
        (!stops.is_empty()).then_some(stops)
    };
    let (mut pairs, mut decided) = (Vec::new(), 0);
    let (mut palette, mut slate): (Option<Vec<Srgb>>, Option<Shown>) = (None, None);
    for line in journal.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        match record["event"].as_str() {
            Some("session_start") => (palette, slate) = (palette_of(&record["palette"]), None),
            Some("drag") => {
                let (Some(stops), Some(at), Some(rgb)) = (palette.as_mut(), record["active"].as_u64(), triple(&record["rgb"])) else { continue };
                if let Some(stop) = stops.get_mut(at as usize) {
                    *stop = rgb;
                }
            }
            Some("undo") => {
                if let (true, Some(back)) = (palette.is_some(), palette_of(&record["palette"])) {
                    palette = Some(back);
                }
            }
            Some("suggestion_shown") => {
                // a slate with an option this format could not have written is not read at all
                let options: Option<Vec<Vec<Srgb>>> = record["slate"].as_array().and_then(|o| o.iter().map(palette_of).collect());
                slate = palette.clone().zip(options);
            }
            Some(event @ ("suggestion_accepted" | "suggestion_dismissed")) => {
                let Some((current, options)) = slate.take() else { continue };
                let here: Vec<Lab> = current.iter().map(|c| oklab(*c)).collect();
                // `choice` counts in the slate as shown; options of another stop count are passed over
                let feats: Vec<Option<[f64; PERSONAL]>> = options.iter().map(|o| {
                    (o.len() == current.len()).then(|| features(&here, &o.iter().map(|c| oklab(*c)).collect::<Vec<_>>()))
                }).collect();
                let minus = |a: &[f64; PERSONAL], b: &[f64; PERSONAL]| -> [f64; PERSONAL] { std::array::from_fn(|k| a[k] - b[k]) };
                let keep = [0.0; PERSONAL];
                if event == "suggestion_accepted" {
                    let taken = record["choice"].as_u64().and_then(|c| usize::try_from(c).ok());
                    let Some((taken, Some(chosen))) = taken.and_then(|t| feats.get(t).map(|f| (t, *f))) else { continue };
                    for f in feats.iter().enumerate().filter(|(i, _)| *i != taken).filter_map(|(_, f)| f.as_ref()) {
                        pairs.push((minus(&chosen, f), 1.0));
                    }
                    pairs.push((minus(&chosen, &keep), 1.0));
                    palette = Some(options[taken].clone());
                    decided += 1;
                } else if feats.iter().any(Option::is_some) {
                    for f in feats.iter().flatten() {
                        pairs.push((minus(&keep, f), 0.5));
                    }
                    decided += 1;
                }
            }
            _ => {}
        }
    }
    (pairs, decided)
}

/// Weighted ridge logistic regression on pair differences, no intercept: fifty Newton steps.
fn fit(pairs: &[([f64; PERSONAL], f64)], scale: &[f64; PERSONAL], ridge: f64) -> [f64; PERSONAL] {
    let mut beta = [0.0; PERSONAL];
    for _ in 0..50 {
        let mut grad: [f64; PERSONAL] = std::array::from_fn(|k| ridge * beta[k]);
        let mut hess = [[0.0; PERSONAL]; PERSONAL];
        for (k, row) in hess.iter_mut().enumerate() {
            row[k] = ridge;
        }
        for (diff, weight) in pairs {
            let z: [f64; PERSONAL] = std::array::from_fn(|k| diff[k] / scale[k]);
            let s: f64 = (0..PERSONAL).map(|k| z[k] * beta[k]).sum::<f64>().clamp(-40.0, 40.0);
            let p = 1.0 / (1.0 + (-s).exp());
            for k in 0..PERSONAL {
                grad[k] += weight * (p - 1.0) * z[k];
                for j in 0..PERSONAL {
                    hess[k][j] += weight * p * (1.0 - p) * z[k] * z[j];
                }
            }
        }
        let step = solve(hess, grad);
        for k in 0..PERSONAL {
            beta[k] -= step[k];
        }
    }
    beta
}

/// `a x = b` by Gaussian elimination with partial pivoting (`a` is symmetric positive definite here).
fn solve(mut a: [[f64; PERSONAL]; PERSONAL], mut b: [f64; PERSONAL]) -> [f64; PERSONAL] {
    for col in 0..PERSONAL {
        let pivot = (col..PERSONAL).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs())).unwrap_or(col);
        a.swap(col, pivot);
        b.swap(col, pivot);
        let (head, rest) = a.split_at_mut(col + 1);
        let top = head[col];
        for (offset, row) in rest.iter_mut().enumerate() {
            let factor = row[col] / top[col];
            for (cell, above) in row.iter_mut().zip(&top).skip(col) {
                *cell -= factor * above;
            }
            b[col + 1 + offset] -= factor * b[col];
        }
    }
    let mut x = [0.0; PERSONAL];
    for row in (0..PERSONAL).rev() {
        x[row] = (b[row] - (row + 1..PERSONAL).map(|k| a[row][k] * x[k]).sum::<f64>()) / a[row][row];
    }
    x
}

/// The logarithm of the mixture weight that explains a colour: each component counts by its
/// likelihood relative to its own peak, so a narrow component is not favoured for being narrow.
fn log_mass(mix: &Mixture, colour: Lab) -> f64 {
    let top = mix.logits.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let total = mix.logits.iter().map(|l| (l - top).exp()).sum::<f64>().ln();
    let parts: Vec<f64> = (0..mix.logits.len()).map(|c| {
        mix.logits[c] - top - total - 0.5 * (0..3).map(|d| ((colour[d] - mix.means[c][d]) / mix.sigmas[c][d]).powi(2)).sum::<f64>()
    }).collect();
    let peak = parts.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    peak + parts.iter().map(|p| (p - peak).exp()).sum::<f64>().ln()
}

/// Each stop's interpolation of its nearest visible neighbours along the line, one-sided at the ends;
/// mid grey without one (Luma's layout 2 interpolation).
fn along_line(lab: &[Lab], positions: &[f64], visible: &[bool]) -> Vec<Lab> {
    let n = lab.len();
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| positions[a].total_cmp(&positions[b]));
    let mut out = vec![[0.5, 0.0, 0.0]; n];
    for (at, &i) in order.iter().enumerate() {
        let left = order[..at].iter().rev().copied().find(|&j| visible[j]);
        let right = order[at + 1..].iter().copied().find(|&j| visible[j]);
        out[i] = match (left, right) {
            (Some(l), Some(r)) => {
                let span = positions[r] - positions[l];
                let part = (if span > 0.0 { (positions[i] - positions[l]) / span } else { (positions[i] - positions[l]) / 1.0 }).clamp(0.0, 1.0);
                std::array::from_fn(|d| lab[l][d] * (1.0 - part) + lab[r][d] * part)
            }
            (Some(l), None) => lab[l],
            (None, Some(r)) => lab[r],
            (None, None) => [0.5, 0.0, 0.0],
        };
    }
    out
}

/// The length of an OKLab change at one stop, and its parts along and across the palette's own run
/// there (from the stop's neighbour before it to the one after it, one-sided at an end).
fn run_parts(change: Lab, lab: &[Lab], positions: &[f64], stop: usize) -> (f64, f64, f64) {
    let mut order: Vec<usize> = (0..lab.len()).collect();
    order.sort_by(|&a, &b| positions[a].total_cmp(&positions[b]));
    let at = order.iter().position(|&i| i == stop).unwrap_or(0);
    let (low, high) = (order[at.saturating_sub(1)], order[(at + 1).min(order.len() - 1)]);
    let run: Lab = std::array::from_fn(|d| lab[high][d] - lab[low][d]);
    let size = (0..3).map(|d| run[d] * run[d]).sum::<f64>().sqrt();
    let length = (0..3).map(|d| change[d] * change[d]).sum::<f64>().sqrt();
    let along = if size > 1e-6 { (0..3).map(|d| change[d] * run[d]).sum::<f64>().abs() / size } else { 0.0 };
    (length, along, (length * length - along * along).max(0.0).sqrt())
}

/// Python's `str` of a float in the readings' names (`0.33`, `1.0`).
fn decimal(v: f64) -> String {
    if v.fract() == 0.0 { format!("{v:.1}") } else { format!("{v}") }
}

impl Learned {
    /// The readings the head chooses among, by name, as display palettes: Luma's `core_readings`.
    pub(super) fn readings(&self, taste: &Taste, ask: &Ask, kind: Kind) -> Vec<(String, Vec<Srgb>)> {
        let n = ask.rgb.len();
        let free: Vec<bool> = (0..n).map(|i| ask.scope[i] && !ask.pins[i]).collect();
        let current: Vec<Lab> = ask.rgb.iter().map(|c| oklab(*c)).collect();
        let blank = vec![0.0; self.style + 1];
        let intent = if kind == Kind::Refine { ask.intent } else { None };
        let condition = Condition { intent, style: &blank };
        let anchor = self.components;
        let mut rows = Vec::new();
        let settle = |mut rgb: Vec<Srgb>, changing: &dyn Fn(usize) -> bool| {
            for j in (0..n).filter(|j| !changing(*j)) {
                rgb[j] = ask.rgb[j];
            }
            rgb
        };
        match kind {
            Kind::Masked | Kind::Refine | Kind::Profile => {
                let Some(active) = ask.active.filter(|a| free[*a] && n >= 2) else { return rows };
                let masked = kind == Kind::Masked;
                let single = Ask { task: kind.task(), ..*ask };
                let inp = inputs(&single, &free, true);
                let mix = self.run(&inp, ask.positions, &condition);
                let favour = if !masked && intent.is_none() { self.favour } else { 0.0 };
                for (i, (component, _)) in self.modes(&mix[active], favour).into_iter().take(READINGS).enumerate() {
                    let centre = mix[active].means[component];
                    let mut sample = current.clone();
                    sample[active] = if !masked && component == anchor { centre } else { oklab(display(centre)) };
                    let rgb = legal(ask, &free, &sample, masked.then_some(active));
                    rows.push((format!("mode-{i}"), settle(rgb, &|j| j == active)));
                }
            }
            Kind::Follow => {
                let Some(active) = ask.active else { return rows };
                if !(0..n).any(|j| free[j] && j != active) {
                    return rows;
                }
                // Luma's `readings`: the committed stop is pinned context, only it carries the
                // just-edited flag, and "leave it" gets no head start.
                let previous: Vec<Lab> = ask.before.map_or_else(|| current.clone(), |b| b.iter().map(|c| oklab(*c)).collect());
                let inp = Inputs {
                    lab: current.clone(),
                    pinned: (0..n).map(|j| ask.pins[j] || j == active).collect(),
                    free: (0..n).map(|j| free[j] && j != active).collect(),
                    shown: vec![true; n],
                    known: vec![true; n],
                    active: Some(active),
                    previous: previous.clone(),
                    recent: (0..n).map(|j| j == active).collect(),
                };
                let mix = self.run(&inp, ask.positions, &condition);
                let (mut mixed, mut carry) = (current.clone(), current.clone());
                for stop in (0..n).filter(|j| inp.free[*j]) {
                    let ranked = self.modes(&mix[stop], 0.0);
                    if let Some((component, _)) = ranked.iter().find(|row| row.0 != anchor) {
                        carry[stop] = oklab(display(mix[stop].means[*component]));
                        if ranked[0].0 != anchor {
                            mixed[stop] = carry[stop];
                        }
                    }
                }
                let mut readings: Vec<(String, Vec<Lab>)> = vec![("carry".into(), carry.clone()), ("mixed".into(), mixed)];
                for (strength, name) in GRADES {
                    readings.push((name.into(), (0..n).map(|k| std::array::from_fn(|d| current[k][d] + strength * (carry[k][d] - current[k][d]))).collect()));
                }
                // The owner's own change carried into the other stops, fading with distance.
                let change: Lab = std::array::from_fn(|d| current[active][d] - previous[active][d]);
                if (0..3).map(|d| change[d] * change[d]).sum::<f64>().sqrt() > 1e-6 {
                    let extent = (ask.positions.iter().copied().fold(f64::NEG_INFINITY, f64::max)
                        - ask.positions.iter().copied().fold(f64::INFINITY, f64::min)).max(1e-9);
                    for &(width, amount) in &taste.spreads {
                        let lab: Vec<Lab> = (0..n).map(|k| {
                            if k == active {
                                return current[k];
                            }
                            let fade = amount * (-(ask.positions[k] - ask.positions[active]).abs() / (width * extent)).exp();
                            std::array::from_fn(|d| current[k][d] + fade * change[d])
                        }).collect();
                        readings.push((format!("spread-{}@{}", decimal(width), decimal(amount)), lab));
                    }
                }
                for (name, lab) in readings {
                    rows.push((name, settle(legal(ask, &free, &lab, None), &|j| free[j] && j != active)));
                }
            }
            Kind::Repair => {
                let repair = Ask { task: Task::Repair, active: None, ..*ask };
                let inp = inputs(&repair, &free, false);
                if !inp.free.contains(&true) {
                    return rows;
                }
                let limit = self.repair.map_or(1.0, |m| m.stop);
                let held = |j: usize| ask.recent.contains(&j);
                let (top, wide) = self.repairs(&inp, ask.positions, &condition, limit);
                let half: Vec<Lab> = (0..n).map(|k| std::array::from_fn(|d| current[k][d] + 0.5 * (top[k][d] - current[k][d]))).collect();
                let mut readings: Vec<(String, Vec<Lab>)> = vec![("top".into(), top), ("top@0.5".into(), half), ("wide".into(), wide)];
                if !ask.recent.is_empty() {
                    // the same repair with the edit history withheld: the held stop is plain context
                    let mut plain = inp.clone();
                    plain.previous = plain.lab.clone();
                    plain.recent = vec![false; n];
                    readings.push(("top-nohistory".into(), self.repairs(&plain, ask.positions, &condition, limit).0));
                }
                for (name, lab) in readings {
                    rows.push((name, settle(legal(ask, &free, &lab, None), &|j| free[j] && !held(j))));
                }
            }
        }
        rows
    }

    /// Luma's `PaletteCore.repairs`: of the stops whose own keep evidence is under `limit` (and every
    /// stop that shows no colour), "top" moves those whose leading mode is a move and "wide" moves
    /// every one to its likeliest move.
    fn repairs(&self, inp: &Inputs, positions: &[f64], condition: &Condition, limit: f64) -> (Vec<Lab>, Vec<Lab>) {
        let mix = self.run(inp, positions, condition);
        let (mut top, mut wide) = (inp.lab.clone(), inp.lab.clone());
        let asked = condition.intent.is_some();
        for stop in (0..inp.lab.len()).filter(|k| inp.free[*k]) {
            let stay = inp.shown[stop];
            if stay && self.keep_probability(&mix[stop]) >= limit {
                continue;
            }
            let ranked = self.modes(&mix[stop], if stay && !asked { self.favour } else { 0.0 });
            if let Some(first) = ranked.iter().map(|row| row.0).find(|c| !(stay && *c == self.components)) {
                wide[stop] = oklab(display(mix[stop].means[first]));
                if ranked[0].0 == first {
                    top[stop] = wide[stop];
                }
            }
        }
        (top, wide)
    }

    /// Per candidate row and free stop: (log-density, log-mass) of the row's colour there. With
    /// several free stops each is rated with the other free stops at the row's colours.
    /// `mix` is the request's own pass, which is what rates a single free stop.
    fn stop_values(&self, inp: &Inputs, positions: &[f64], condition: &Condition, rows: &[Vec<Lab>], mix: &[Mixture]) -> Vec<Vec<(f64, f64)>> {
        let stops: Vec<usize> = (0..inp.free.len()).filter(|k| inp.free[*k]).collect();
        if let [only] = stops[..] {
            return rows.iter().map(|row| vec![(log_density(&mix[only], row[only]), log_mass(&mix[only], row[only]))]).collect();
        }
        rows.iter().map(|row| {
            stops.iter().map(|&s| {
                let mut scene = inp.clone();
                for k in stops.iter().copied().filter(|k| *k != s) {
                    scene.lab[k] = row[k];
                    scene.shown[k] = true;
                }
                let mix = &self.run(&scene, positions, condition)[s];
                (log_density(mix, row[s]), log_mass(mix, row[s]))
            }).collect()
        }).collect()
    }

    /// Both token groups for "keep" (row 0) and every candidate: Luma's `taste.describe`.
    pub(super) fn head_tokens(&self, ask: &Ask, kind: Kind, candidates: &[Vec<Srgb>]) -> Tokens {
        let n = ask.rgb.len();
        let free_scope: Vec<bool> = (0..n).map(|i| ask.scope[i] && !ask.pins[i]).collect();
        let request = Ask { task: kind.task(), ..*ask };
        let inp = inputs(&request, &free_scope, false);
        let blank = vec![0.0; self.style + 1];
        let intent = if kind == Kind::Refine { ask.intent } else { None };
        let condition = Condition { intent, style: &blank };
        let lab = inp.lab.clone();
        let rows: Vec<Vec<Lab>> = std::iter::once(lab.clone()).chain(candidates.iter().map(|c| c.iter().map(|v| oklab(*v)).collect())).collect();
        let stops: Vec<usize> = (0..n).filter(|k| inp.free[*k]).collect();
        let free_count = stops.len() as f64;
        let mut task = [0.0; 5];
        task[kind.index()] = 1.0;
        let moved: Vec<Vec<f64>> = rows.iter().map(|row| (0..n).map(|k| distance(row[k], lab[k])).collect()).collect();
        let mut core = vec![vec![[0.0; CORE]; n]; rows.len()];
        let (values, mix) = if stops.is_empty() {
            (vec![Vec::new(); rows.len()], Vec::new())
        } else {
            let mix = self.run(&inp, ask.positions, &condition);
            (self.stop_values(&inp, ask.positions, &condition, &rows, &mix), mix)
        };
        let cut = |v: f64, low: f64, high: f64, scale: f64| v.clamp(low, high) / scale;
        for r in 0..rows.len() {
            let is_keep = moved[r].iter().copied().fold(0.0, f64::max) < 1e-9;
            for k in 0..n {
                let t = &mut core[r][k];
                if let Some(slot) = stops.iter().position(|&s| s == k) {
                    let (d, m) = values[r][slot];
                    let (d0, m0) = values[0][slot];
                    let weight = softmax(&mix[k].logits);
                    let entropy = -weight.iter().map(|w| w * (w + 1e-12).ln()).sum::<f64>();
                    let fields = [1.0, cut(d, -40.0, 25.0, 10.0), cut(d0, -40.0, 25.0, 10.0), cut(d - d0, -40.0, 40.0, 10.0), cut(m, -30.0, 0.0, 5.0),
                        cut(m0, -30.0, 0.0, 5.0), cut(m - m0, -30.0, 30.0, 5.0), self.keep_probability(&mix[k]), entropy];
                    t[..9].copy_from_slice(&fields);
                }
                t[9] = moved[r][k] * MOVE;
                t[10] = f64::from(u8::from(inp.free[k]));
                t[11] = f64::from(u8::from(inp.pinned[k]));
                t[12] = f64::from(u8::from(inp.active == Some(k)));
                t[13] = f64::from(u8::from(inp.recent[k]));
                t[14] = f64::from(u8::from(is_keep));
                t[15..20].copy_from_slice(&task);
                t[20] = free_count / 12.0;
            }
        }
        // the request-level group (Luma's `taste.request_features`)
        let previous = &inp.previous;
        let mut commit = [0.0; 6];
        let change: Vec<f64> = (0..n).map(|k| if inp.recent[k] { distance(lab[k], previous[k]) } else { 0.0 }).collect();
        let largest = change.iter().copied().fold(0.0, f64::max);
        if largest > 0.0 {
            let stop = change.iter().position(|c| *c == largest).unwrap_or(0);
            let vector: Lab = std::array::from_fn(|d| lab[stop][d] - previous[stop][d]);
            let (length, along, across) = run_parts(vector, previous, ask.positions, stop);
            let others: Vec<bool> = (0..n).map(|j| j != stop).collect();
            let gap = |palette: &[Lab]| distance(palette[stop], along_line(palette, ask.positions, &others)[stop]);
            commit = [1.0, length * MOVE, along * MOVE, across * MOVE, gap(&lab) * MOVE, gap(previous) * MOVE];
        }
        let asked = kind == Kind::Refine && ask.intent.is_some_and(|i| i.iter().any(|v| *v != 0.0));
        let ask_size = if asked { ask.intent.map_or(0.0, |i| (0..3).map(|d| i[d] * i[d]).sum::<f64>().sqrt()) * MOVE } else { 0.0 };
        let recent_count = inp.recent.iter().filter(|r| **r).count() as f64;
        let mut fixed = Vec::with_capacity(16);
        fixed.extend_from_slice(&task);
        fixed.extend_from_slice(&commit);
        fixed.extend_from_slice(&[f64::from(u8::from(asked)), ask_size, recent_count.min(4.0) / 4.0, free_count / 12.0, free_count / n as f64]);
        let request: Vec<[f64; REQUEST]> = moved.iter().map(|m| {
            let changed: Vec<bool> = m.iter().map(|v| *v > 0.005).collect();
            let number = changed.iter().filter(|c| **c).count() as f64;
            let max = m.iter().copied().fold(0.0, f64::max);
            let own = [
                number / 12.0,
                (0..n).filter(|k| changed[*k] && inp.free[*k]).count() as f64 / free_count.max(1.0),
                max * MOVE,
                m.iter().sum::<f64>() / number.max(1.0) * MOVE,
                f64::from(u8::from((0..n).any(|k| changed[k] && inp.recent[k]))),
                f64::from(u8::from(max < 1e-9)),
            ];
            let mut out = [0.0; REQUEST];
            out[..16].copy_from_slice(&fixed);
            out[16..].copy_from_slice(&own);
            out
        }).collect();
        (core, request, inp.active)
    }

    /// The head's scores for a request's candidates.
    pub(super) fn scores(&self, taste: &Taste, ask: &Ask, kind: Kind, candidates: &[Vec<Srgb>]) -> Scores {
        let (core, request, active) = self.head_tokens(ask, kind, candidates);
        let logits = taste.logits(&core, &request, active);
        Scores {
            acceptable: logits[1..].iter().map(|l| 1.0 / (1.0 + (-l[0]).exp())).collect(),
            pick: logits[1..].iter().map(|l| l[1]).collect(),
            keep: logits[0][1],
        }
    }

    /// What the head offers for a request: its readings scored, a person's taste added when one is
    /// known, and Luma's `TasteHead.decide`.
    pub(super) fn tasted(&self, taste: &Taste, ask: &Ask, kind: Kind) -> Answer {
        let mut out = Answer { offerable: false, keep: true, options: Vec::new(), keep_probability: Vec::new() };
        let named = self.readings(taste, ask, kind);
        if named.is_empty() {
            return out;
        }
        out.offerable = true;
        let palettes: Vec<Vec<Srgb>> = named.iter().map(|(_, rgb)| rgb.clone()).collect();
        let mut scores = self.scores(taste, ask, kind, &palettes);
        if let (Some(person), Some(rules)) = (self.person.as_ref(), taste.rules.as_ref()) {
            for (pick, extra) in scores.pick.iter_mut().zip(person.preference(rules, ask.rgb, &palettes)) {
                *pick += extra;
            }
        }
        let floor = taste.visible[kind.index()];
        let here: Vec<Lab> = ask.rgb.iter().map(|c| oklab(*c)).collect();
        let labs: Vec<Vec<Lab>> = palettes.iter().map(|p| p.iter().map(|c| oklab(*c)).collect()).collect();
        let far = |a: &[Lab], b: &[Lab]| a.iter().zip(b).map(|(x, y)| distance(*x, *y)).fold(0.0, f64::max);
        let mut order: Vec<usize> = (0..palettes.len()).collect();
        order.sort_by(|&a, &b| scores.pick[b].total_cmp(&scores.pick[a]));
        let mut kept: Vec<usize> = Vec::new();
        // Every test asks for a pass, so a score that is not a number keeps instead of offering.
        for i in order {
            let passes = scores.acceptable[i] >= taste.acceptable
                && far(&labs[i], &here) >= floor
                && kept.iter().all(|&j| far(&labs[i], &labs[j]) >= DISTINCT);
            if passes {
                kept.push(i);
            }
        }
        out.keep = !kept.first().is_some_and(|&best| scores.pick[best] - scores.keep > taste.margin);
        kept.truncate(ask.count.max(1));
        out.options = kept.into_iter().map(|i| Candidate { rgb: palettes[i].clone(), score: scores.pick[i] - scores.keep }).collect();
        out
    }

    /// The names of the readings the head chose, best first (for the reference checks).
    #[cfg(test)]
    pub(super) fn tasted_names(&self, taste: &Taste, ask: &Ask, kind: Kind) -> (bool, Vec<String>) {
        let named = self.readings(taste, ask, kind);
        let answer = self.tasted(taste, ask, kind);
        let names = answer.options.iter().filter_map(|o| named.iter().find(|(_, rgb)| *rgb == o.rgb).map(|(n, _)| n.clone())).collect();
        (answer.keep, names)
    }
}

#[cfg(test)]
pub(super) mod check {
    use super::*;

    /// Compare every vector in a Luma taste golden file with this port: the readings, both token
    /// groups, the head's scores, the personal term and the decision. Returns (requests, offers).
    pub(in crate::assist::learned) fn vectors(model: &mut Learned, taste: &Taste, cases: &str) -> (usize, usize) {
        let triple = |v: &serde_json::Value| -> [f64; 3] { std::array::from_fn(|d| v[d].as_f64().unwrap()) };
        let triples = |v: &serde_json::Value| -> Vec<[f64; 3]> { v.as_array().unwrap().iter().map(triple).collect() };
        let numbers = |v: &serde_json::Value| -> Vec<f64> { v.as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect() };
        let flags = |v: &serde_json::Value| -> Vec<bool> { v.as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect() };
        let close = |a: f64, b: f64, what: &str| assert!((a - b).abs() < 1e-8, "{what}: {a} vs {b}");
        let (mut requests, mut offers) = (0, 0);
        for line in cases.lines().filter(|l| !l.trim().is_empty()) {
            let case: serde_json::Value = serde_json::from_str(line).unwrap();
            let id = case["id"].as_str().unwrap();
            let input = &case["input"];
            let kind = match case["task"].as_str().unwrap() {
                "masked" => Kind::Masked,
                "refine" => Kind::Refine,
                "follow" => Kind::Follow,
                "profile" => Kind::Profile,
                _ => Kind::Repair,
            };
            let rgb = triples(&input["rgb"]);
            let before = (!input["before_rgb"].is_null()).then(|| triples(&input["before_rgb"]));
            let recent: Vec<usize> = input["recent"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as usize).collect();
            let (positions, pins, scope) = (numbers(&input["positions"]), flags(&input["pins"]), flags(&input["scope"]));
            let ask = Ask {
                task: kind.task(),
                rgb: &rgb,
                before: before.as_deref(),
                positions: &positions,
                pins: &pins,
                scope: &scope,
                active: input["active"].as_u64().map(|v| v as usize),
                recent: &recent,
                radius: input["radius"].as_f64(),
                count: 3,
                intent: (!input["intent"].is_null()).then(|| triple(&input["intent"])),
                style: &[],
            };
            model.person = case["person_weights"].as_array().map(|w| Person::new(std::array::from_fn(|k| w[k].as_f64().unwrap())));
            let output = &case["output"];
            let named = model.readings(taste, &ask, kind);
            let want = output["candidates"].as_array().unwrap();
            assert_eq!(named.len(), want.len(), "{id}: readings");
            for ((name, got), want) in named.iter().zip(want) {
                assert_eq!(name, want["name"].as_str().unwrap(), "{id}: reading name");
                for (a, b) in got.iter().zip(&triples(&want["rgb"])) {
                    (0..3).for_each(|d| close(a[d], b[d], &format!("{id} {name} rgb")));
                }
            }
            if !named.is_empty() {
                let palettes: Vec<Vec<Srgb>> = named.iter().map(|(_, p)| p.clone()).collect();
                let (core, request, _) = model.head_tokens(&ask, kind, &palettes);
                let checkpoint = &case["checkpoint"];
                for (r, rows) in checkpoint["core"].as_array().into_iter().flatten().enumerate() {
                    for (k, stop) in rows.as_array().unwrap().iter().enumerate() {
                        for (j, v) in stop.as_array().unwrap().iter().enumerate() {
                            close(core[r][k][j], v.as_f64().unwrap(), &format!("{id} core token [{r}][{k}][{j}]"));
                        }
                    }
                }
                for (r, rows) in checkpoint["request"].as_array().into_iter().flatten().enumerate() {
                    let first = &rows.as_array().unwrap()[0];
                    for (j, v) in first.as_array().unwrap().iter().enumerate() {
                        close(request[r][j], v.as_f64().unwrap(), &format!("{id} request token [{r}][{j}]"));
                    }
                }
                let scores = model.scores(taste, &ask, kind, &palettes);
                for (a, b) in scores.pick.iter().zip(numbers(&output["pick"])) {
                    close(*a, b, &format!("{id} pick"));
                }
                for (a, b) in scores.acceptable.iter().zip(numbers(&output["acceptable"])) {
                    close(*a, b, &format!("{id} acceptable"));
                }
                close(scores.keep, output["keep_score"].as_f64().unwrap(), &format!("{id} keep score"));
                if let (Some(person), Some(rules), Some(want)) = (model.person.as_ref(), taste.rules.as_ref(), output["personal"].as_array()) {
                    for (a, b) in person.preference(rules, &rgb, &palettes).iter().zip(want) {
                        close(*a, b.as_f64().unwrap(), &format!("{id} personal"));
                    }
                }
            }
            let (keep, names) = model.tasted_names(taste, &ask, kind);
            assert_eq!(keep, output["keep"].as_bool().unwrap(), "{id}: keep");
            let want: Vec<&str> = output["options"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
            assert_eq!(names, want, "{id}: options");
            requests += 1;
            offers += usize::from(!keep);
        }
        model.person = None;
        (requests, offers)
    }
}
