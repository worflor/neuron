// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Gesture vault — named eigenmotion templates, persisted, recognized by DTW.
//!
//! A template is a gesture word (sequence of [`Sig`]). Recognition is 1-NN by the
//! attunable DTW distance; matches above `config.threshold` are rejected as "unknown",
//! so an unrecognized scribble doesn't fire a random action.

use crate::glyph::{word_distance, GestureWord, GlyphConfig};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub word: GestureWord,
    /// A drawable normalized polyline of the stroke as recorded (centered unit box; see
    /// [`crate::glyph::exemplar_path`]) — lets a live overlay ghost "the ideal shape" while you
    /// draw. `#[serde(default)]` so templates saved before this field load fine (empty = no ghost,
    /// the prediction name still shows; re-record to capture one).
    #[serde(default)]
    pub exemplar: Vec<[f32; 2]>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Vault {
    pub config: GlyphConfig,
    pub templates: Vec<Template>,
}

impl Vault {
    #[must_use]
    pub fn path() -> PathBuf {
        // alongside the binaries; keeps the gesture set portable with the install.
        crate::runroot::run_root().join("gestures.json")
    }

    #[must_use]
    pub fn load() -> Self {
        Self::load_from(&Self::path()).unwrap_or_default()
    }

    pub fn load_from(p: &Path) -> Result<Self> {
        let txt = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
        let v: Vault = serde_json::from_str(&txt).context("parse vault json")?;
        Ok(v)
    }

    pub fn save(&self) -> std::result::Result<(), String> {
        self.save_to(&Self::path()).map_err(|e| e.to_string())
    }

    pub fn save_to(&self, p: &Path) -> Result<()> {
        let txt = serde_json::to_string_pretty(self).context("serialize vault")?;
        std::fs::write(p, txt).with_context(|| format!("write {}", p.display()))?;
        Ok(())
    }

    /// Insert or replace a template by name (no exemplar — see [`upsert_with`] to store one).
    pub fn upsert(&mut self, name: &str, word: GestureWord) {
        self.upsert_with(name, word, Vec::new());
    }

    /// Insert or replace a template, carrying a drawable exemplar polyline for the shape-ghost.
    pub fn upsert_with(&mut self, name: &str, word: GestureWord, exemplar: Vec<[f32; 2]>) {
        if let Some(t) = self.templates.iter_mut().find(|t| t.name == name) {
            t.word = word;
            t.exemplar = exemplar;
        } else {
            self.templates.push(Template {
                name: name.to_string(),
                word,
                exemplar,
            });
        }
    }

    /// The exemplar polyline for a template by name (empty if absent / pre-exemplar template).
    #[must_use]
    pub fn exemplar(&self, name: &str) -> &[[f32; 2]] {
        self.templates
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.exemplar.as_slice())
            .unwrap_or(&[])
    }

    /// The CLOSEST template regardless of threshold — the autopredict primitive. Returns
    /// `(name, score, runner_up_score)`; `None` only for an empty vault. Use [`recognize`] for
    /// the gated verdict; use this to show "≈ what it's becoming" live, or to assist-snap a
    /// near-miss (see `CastConfig::assist`).
    #[must_use]
    pub fn predict(&self, query: &GestureWord) -> Option<(String, f64, Option<f64>)> {
        let mut ranked: Vec<(&str, f64)> = self
            .templates
            .iter()
            .map(|t| (t.name.as_str(), word_distance(query, &t.word, &self.config)))
            .collect();
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
        let (name, score) = ranked.first().map(|(n, s)| (n.to_string(), *s))?;
        Some((name, score, ranked.get(1).map(|(_, s)| *s)))
    }

    /// 1-NN recognition. Returns (name, score) for the best match within threshold,
    /// plus the runner-up score for confidence reporting.
    #[must_use]
    pub fn recognize(&self, query: &GestureWord) -> Recognition {
        let mut ranked: Vec<(&str, f64)> = self
            .templates
            .iter()
            .map(|t| (t.name.as_str(), word_distance(query, &t.word, &self.config)))
            .collect();
        ranked.sort_by(|a, b| a.1.total_cmp(&b.1));

        let best = ranked.first().map(|(n, s)| (n.to_string(), *s));
        let runner_up = ranked.get(1).map(|(_, s)| *s);
        match best {
            Some((name, score)) if score <= self.config.threshold => Recognition {
                name: Some(name),
                score,
                runner_up,
            },
            Some((_, score)) => Recognition {
                name: None,
                score,
                runner_up,
            },
            None => Recognition {
                name: None,
                score: f64::INFINITY,
                runner_up: None,
            },
        }
    }
}

#[derive(Debug)]
pub struct Recognition {
    pub name: Option<String>,
    pub score: f64,
    pub runner_up: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{analyze, synth_circle, synth_line};

    #[test]
    fn vault_roundtrips_through_json() {
        let cfg = GlyphConfig::default();
        let mut v = Vault::default();
        v.upsert("o", analyze(&synth_circle(160, 400.0, 0.2), &cfg));
        let json = serde_json::to_string(&v).unwrap();
        let back: Vault = serde_json::from_str(&json).unwrap();
        assert_eq!(back.templates.len(), 1);
        assert_eq!(back.templates[0].name, "o");
        assert_eq!(
            back.templates[0].word.sigs.len(),
            v.templates[0].word.sigs.len()
        );
    }

    #[test]
    fn recognizes_known_rejects_unknown() {
        use std::f64::consts::TAU;
        let cfg = GlyphConfig::default();
        let mut v = Vault::default();
        // single-loop smooth circles (what the sensor actually produces)
        v.upsert(
            "circle",
            analyze(&synth_circle(140, 400.0, TAU / 140.0), &cfg),
        );
        v.upsert("line", analyze(&synth_line(160), &cfg));

        // a CW circle variant it never saw (different size/speed) → recognized as "circle"
        let q = analyze(&synth_circle(90, 250.0, TAU / 90.0), &cfg);
        let r = v.recognize(&q);
        assert_eq!(r.name.as_deref(), Some("circle"), "score {}", r.score);

        // a CCW circle must NOT be confidently recognized as the CW "circle" (handedness)
        let ccw = analyze(&synth_circle(140, 400.0, -TAU / 140.0), &cfg);
        let r2 = v.recognize(&ccw);
        assert!(
            r2.name.as_deref() != Some("circle"),
            "CCW wrongly matched CW circle at score {}",
            r2.score
        );
    }

    // ── TASK1(h): metamorphic/robustness laws for the vault's public API ──────────────────────
    mod props {
        use super::*;
        use crate::glyph::{synth_vee, C};
        use proptest::prelude::*;

        fn cfg() -> ProptestConfig {
            ProptestConfig { cases: 256, ..ProptestConfig::default() }
        }

        /// Same random-walk generator as `glyph::tests::props::arb_stroke` (kept local — that one
        /// is private to `glyph.rs`'s test module).
        fn arb_stroke() -> impl Strategy<Value = Vec<C>> {
            proptest::collection::vec((-8.0f64..8.0f64, -8.0f64..8.0f64), 8..128).prop_map(
                |deltas| {
                    let mut acc = C::new(0.0, 0.0);
                    deltas
                        .into_iter()
                        .map(|(dx, dy)| {
                            acc = acc.add(C::new(dx, dy));
                            acc
                        })
                        .collect()
                },
            )
        }

        fn seeded_vault() -> Vault {
            use std::f64::consts::TAU;
            let cfgv = GlyphConfig::default();
            let mut v = Vault::default();
            v.config = cfgv;
            v.upsert("circle_cw", analyze(&synth_circle(64, 300.0, TAU / 64.0), &cfgv));
            v.upsert("circle_ccw", analyze(&synth_circle(64, 300.0, -TAU / 64.0), &cfgv));
            v.upsert("line", analyze(&synth_line(60), &cfgv));
            v.upsert("vee", analyze(&synth_vee(60, 8.0), &cfgv));
            v
        }

        proptest! {
            #![proptest_config(cfg())]

            /// TASK2(f)/(h): `predict`/`recognize` never panic on an arbitrary query, on either an
            /// empty vault (the documented "`None` only for an empty vault" case, see `predict`'s
            /// doc comment above) or a populated one.
            #[test]
            fn vault_never_panics_on_empty_or_populated_query(stroke in arb_stroke()) {
                let cfgv = GlyphConfig::default();
                let q = analyze(&stroke, &cfgv);

                let empty = Vault::default();
                prop_assert!(empty.predict(&q).is_none());
                let r = empty.recognize(&q);
                prop_assert!(r.name.is_none());
                prop_assert_eq!(r.score, f64::INFINITY);
                prop_assert!(r.runner_up.is_none());

                let v = seeded_vault();
                let _ = v.predict(&q);
                let _ = v.recognize(&q);
            }

            /// TASK1(h): `recognize` is exactly `predict`'s top result, threshold-gated — this is
            /// a direct restatement of `recognize`'s own logic (gesture.rs ~101-128: same ranked
            /// list, same top score; `name` is `Some` only when that score is `<= config.threshold`)
            /// but pinned as a property over arbitrary queries rather than the two fixed shapes
            /// `recognizes_known_rejects_unknown` above already covers.
            #[test]
            fn recognize_agrees_with_predict_gated_by_threshold(stroke in arb_stroke()) {
                let v = seeded_vault();
                let q = analyze(&stroke, &v.config);
                let (pred_name, pred_score, _) = v.predict(&q).expect("seeded vault is non-empty");
                let rec = v.recognize(&q);
                prop_assert_eq!(rec.score, pred_score);
                if pred_score <= v.config.threshold {
                    prop_assert_eq!(rec.name, Some(pred_name));
                } else {
                    prop_assert_eq!(rec.name, None);
                }
            }
        }
    }
}
