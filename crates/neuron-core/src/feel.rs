// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Input FEEL — the press grammar + timing forgiveness that makes Neuron's triggers feel like a
//! good game, not a form. First principles:
//!
//!   1. **An input is a rhythm, not a switch.** Down/up edges over one control form a *phrase* of
//!      symbols (`tap`, `hold`): single click, double-tap-then-hold, triple-tap — all phrases, all
//!      bindable. Presets cover the sane ones; a recorded rhythm normalizes into the same grammar.
//!   2. **Activate at the earliest unambiguous moment.** A hold-ending phrase fires on its final
//!      press's DOWN edge, so plain `hold` keeps ZERO added latency and `tap tap hold` costs only
//!      the taps themselves. An all-tap phrase fires when its final press is released short, once
//!      its tap duration is known.
//!   3. **Forgiveness, not lag.** Coyote time keeps the stroke alive briefly after release ("let
//!      go a hair early" still counts); a failed rhythm resets instantly and silently.
//!   4. **Fidgeting is not an error.** Any phrase that resolves to nothing costs nothing: no
//!      lockout, no cooldown, no error spam. Spamming re-arms within one poll tick.
//!   5. **A layer is a stance.** `HyperShift` gets the four stance modes games standardized —
//!      hold (momentary), latch (tap on/off), smart (tap latches, hold is momentary), one-shot
//!      (the next press is shifted, then auto-drops). Razer ships hold-only; this is the fix.
//!
//! Everything here is PURE (fed timestamps, no clocks, no OS) so the grammar is exhaustively
//! testable; the capture loops feed it real polls.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One symbol of a press phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sym {
    /// A press released in less than `hold_ms`.
    Tap,
    /// A press held past `hold_ms`. Always the LAST symbol of a phrase (you can't keep tapping
    /// a rhythm while holding).
    Hold,
}

/// A press phrase — the activation rhythm for a trigger. Serialized as space-separated tokens
/// ("tap tap hold"), human-readable and hand-editable like every other Neuron config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phrase(pub Vec<Sym>);

impl Phrase {
    /// The classic: press and it's live (zero added latency).
    #[must_use]
    pub fn hold() -> Self {
        Phrase(vec![Sym::Hold])
    }

    /// Parse "tap tap hold" (any whitespace). Unknown tokens fail; an interior `hold` fails
    /// (a hold ends a phrase by construction); empty input yields the classic `hold`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut syms = Vec::new();
        for tok in s.split_whitespace() {
            match tok.to_lowercase().as_str() {
                "tap" => syms.push(Sym::Tap),
                "hold" => syms.push(Sym::Hold),
                other => return Err(format!("unknown press symbol '{other}' (tap | hold)")),
            }
        }
        if syms.is_empty() {
            return Ok(Self::hold());
        }
        if syms[..syms.len() - 1].contains(&Sym::Hold) {
            return Err(
                "'hold' can only end a phrase — you can't keep tapping while holding".into(),
            );
        }
        Ok(Phrase(syms))
    }

    /// "tap tap hold" — the inverse of [`parse`].
    #[must_use]
    pub fn describe(&self) -> String {
        self.0
            .iter()
            .map(|s| match s {
                Sym::Tap => "tap",
                Sym::Hold => "hold",
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Ends in a hold → capture lives WHILE the final press is held (release ends it).
    /// All-taps → the phrase TOGGLES capture after the final press is released short.
    #[must_use]
    pub fn ends_in_hold(&self) -> bool {
        self.0.last() == Some(&Sym::Hold)
    }

    /// The MODE shape of a phrase: `Some(n)` if it's exactly "n taps then a hold" — the grammar
    /// the multi-instrument trigger dispatcher multiplexes on (hold = weave, tap-hold = teleport,
    /// tap-tap-hold = whiteboard…). `None` for anything else (toggle phrases, hold-mid-phrase).
    #[must_use]
    pub fn taps_then_hold(&self) -> Option<u8> {
        let (last, taps) = self.0.split_last()?;
        if *last != Sym::Hold || taps.iter().any(|s| *s != Sym::Tap) || taps.len() > 250 {
            return None;
        }
        Some(taps.len() as u8)
    }

    /// Normalize a recorded edge sequence into a phrase: press durations quantize against
    /// `hold_ms` (shorter = tap, longer = hold-and-end). `presses` are (`down_ms`, `up_ms`) pairs in
    /// any consistent clock; an unterminated final press records as a Hold. Gaps are NOT encoded
    /// (the grammar is rhythm-shape, not tempo — matching applies the user's live tempo instead),
    /// which is what makes a sloppy re-performance still match a tight recording.
    #[must_use]
    pub fn from_recording(presses: &[(u64, Option<u64>)], cfg: &FeelConfig) -> Option<Self> {
        if presses.is_empty() {
            return None;
        }
        let mut syms = Vec::new();
        for (i, (down, up)) in presses.iter().enumerate() {
            let last = i == presses.len() - 1;
            match up {
                Some(up) if up.saturating_sub(*down) < cfg.hold_ms => syms.push(Sym::Tap),
                Some(_) | None => {
                    syms.push(Sym::Hold);
                    if !last {
                        // a mid-recording hold ends the phrase — ignore the rest.
                        break;
                    }
                }
            }
        }
        Some(Phrase(syms))
    }
}

/// The timing windows — tuned like a game, not a form. All milliseconds.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct FeelConfig {
    /// A press released within this is a TAP; held past it, a HOLD. (Also the smart-stance
    /// tap-vs-momentary discriminator.) ~200ms is the comfortable double-click-grade tap.
    #[serde(default = "d_hold_ms")]
    pub hold_ms: u64,
    /// Max silence between phrase presses. Longer = the phrase died (quietly). Generous by
    /// default — rhythm forgiveness beats strictness.
    #[serde(default = "d_gap_ms")]
    pub gap_ms: u64,
    /// COYOTE TAIL: motion within this window after release still belongs to the stroke
    /// ("released a hair early" must not eat the gesture's tail).
    #[serde(default = "d_coyote_ms")]
    pub coyote_ms: u64,
    /// The `HyperShift` stance (see [`LayerMode`]).
    #[serde(default)]
    pub hypershift: LayerMode,
}

fn d_hold_ms() -> u64 {
    200
}
fn d_gap_ms() -> u64 {
    280
}
fn d_coyote_ms() -> u64 {
    120
}

impl Default for FeelConfig {
    fn default() -> Self {
        FeelConfig {
            hold_ms: d_hold_ms(),
            gap_ms: d_gap_ms(),
            coyote_ms: d_coyote_ms(),
            hypershift: LayerMode::default(),
        }
    }
}

impl FeelConfig {
    #[must_use]
    pub fn path() -> PathBuf {
        crate::runroot::run_root().join("feel.toml")
    }

    /// Load from `feel.toml`, salvaging field-by-field (a malformed timing no longer silently resets
    /// every feel/hypershift setting) and never clobbering the file — see [`crate::salvage::SalvageLoad`].
    #[must_use]
    pub fn load() -> Self {
        <Self as crate::salvage::SalvageLoad>::load()
    }

    pub fn save(&self) -> Result<(), String> {
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        crate::salvage::atomic_write(&Self::path(), body.as_bytes()).map_err(|e| e.to_string())
    }
}

impl crate::salvage::SalvageLoad for FeelConfig {
    const FILE: &'static str = "feel.toml";
    fn path() -> PathBuf {
        crate::runroot::run_root().join("feel.toml")
    }
    fn salvage(table: &toml::Table) -> Self {
        let mut cfg = Self::default();
        crate::salvage_fields!(table, Self::FILE, cfg, {
            "hold_ms" => hold_ms,
            "gap_ms" => gap_ms,
            "coyote_ms" => coyote_ms,
            "hypershift" => hypershift,
        });
        cfg
    }
}

/// The `HyperShift` STANCE — how the layer trigger behaves. Razer ships `Hold` only (toggle has
/// been a years-old community request); games solved this long ago, so Neuron offers the set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayerMode {
    /// Momentary: the layer lives while the trigger is physically held (Razer's behaviour).
    #[default]
    Hold,
    /// Tap toggles the layer on/off. Activation just to deactivate is free.
    Latch,
    /// The game-standard unified stance: a TAP latches the layer; a HOLD is momentary
    /// (releases with the button). One trigger, both muscle memories.
    Smart,
    /// Sticky one-shot: a tap arms the layer for exactly the NEXT trigger, then it drops.
    OneShot,
}

impl LayerMode {
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "latch" => LayerMode::Latch,
            "smart" => LayerMode::Smart,
            "one-shot" | "oneshot" | "1-shot" => LayerMode::OneShot,
            _ => LayerMode::Hold,
        }
    }
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            LayerMode::Hold => "hold",
            LayerMode::Latch => "latch",
            LayerMode::Smart => "smart",
            LayerMode::OneShot => "one-shot",
        }
    }
}

/// What a [`PhraseWatcher::feed`] sample resolved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watch {
    /// Still reading the rhythm (or idle).
    Pending,
    /// The phrase completed on this sample — start the capture NOW.
    /// `toggle` = the phrase was all taps: activation is after a short release, and capture runs
    /// until the next tap, not while held.
    Activated { toggle: bool },
    /// The rhythm broke (wrong press shape / gap expired). Cost: nothing. The breaking press,
    /// if any, has already been re-considered as the start of a fresh phrase.
    Reset,
}

/// The pure activation state machine: feed it (time, is-down) samples from any poll loop and it
/// tells you the moment the configured phrase completes. Hold-ending phrases complete on the final
/// down edge; all-tap phrases complete on the final short release. Edge detection, classification,
/// gap expiry, and instant re-arm all live here — deterministic and unit-tested, no clocks.
#[derive(Clone, Debug)]
pub struct PhraseWatcher {
    phrase: Phrase,
    hold_ms: u64,
    gap_ms: u64,
    /// next symbol to satisfy
    idx: usize,
    /// the current press's down time (we are mid-press)
    down_at: Option<u64>,
    /// the last release time (we are mid-gap)
    up_at: Option<u64>,
    was_down: bool,
}

impl PhraseWatcher {
    #[must_use]
    pub fn new(phrase: Phrase, cfg: &FeelConfig) -> Self {
        PhraseWatcher {
            phrase,
            hold_ms: cfg.hold_ms,
            gap_ms: cfg.gap_ms,
            idx: 0,
            down_at: None,
            up_at: None,
            was_down: false,
        }
    }

    fn reset(&mut self) {
        self.idx = 0;
        self.down_at = None;
        self.up_at = None;
    }

    /// Feed one polled sample. `t_ms` is any monotonically-nondecreasing millisecond clock.
    pub fn feed(&mut self, t_ms: u64, down: bool) -> Watch {
        let pressed_edge = down && !self.was_down;
        let released_edge = !down && self.was_down;
        self.was_down = down;

        // gap expiry: waiting for the next press too long → the rhythm died, quietly.
        if !down && self.down_at.is_none() {
            if let Some(up) = self.up_at {
                if self.idx > 0 && t_ms.saturating_sub(up) > self.gap_ms {
                    self.reset();
                    return Watch::Reset;
                }
            }
        }

        if pressed_edge {
            // a press that arrives after the gap died restarts the phrase from this press.
            if let Some(up) = self.up_at {
                if self.idx > 0 && t_ms.saturating_sub(up) > self.gap_ms {
                    self.reset();
                }
            }
            self.down_at = Some(t_ms);
            self.up_at = None;
            // A final hold is unambiguous on down; a final tap must wait for its release.
            if self.idx == self.phrase.0.len() - 1
                && self.phrase.0[self.idx] == Sym::Hold
            {
                self.reset();
                self.was_down = down;
                return Watch::Activated { toggle: false };
            }
            return Watch::Pending;
        }

        if released_edge {
            if let Some(d) = self.down_at.take() {
                let dur = t_ms.saturating_sub(d);
                let final_tap = self.idx == self.phrase.0.len() - 1
                    && self.phrase.0[self.idx] == Sym::Tap;
                if final_tap {
                    if dur < self.hold_ms {
                        self.reset();
                        return Watch::Activated { toggle: true };
                    }
                    self.reset();
                    return Watch::Reset;
                }
                // Every non-final symbol must be a tap.
                if dur < self.hold_ms {
                    self.idx += 1;
                    self.up_at = Some(t_ms);
                    return Watch::Pending;
                }
                // a long press where a tap belonged: the rhythm broke. Reset costs nothing.
                self.reset();
                return Watch::Reset;
            }
        }

        // A press expected to be a tap cannot recover after hold_ms — break early so the user
        // isn't left holding a dead rhythm.
        if down {
            if let Some(d) = self.down_at {
                if self.phrase.0[self.idx] == Sym::Tap
                    && t_ms.saturating_sub(d) >= self.hold_ms
                {
                    self.reset();
                    self.was_down = true;
                    return Watch::Reset;
                }
            }
        }

        Watch::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degraded_feel_defaults_the_bad_field_and_keeps_the_rest() {
        use crate::salvage::SalvageLoad;
        let dir = std::env::temp_dir().join(format!("neuron-feel-degraded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feel.toml");
        // hold_ms is the wrong type (forces the degraded path); the sibling scalars must survive.
        std::fs::write(&path, "hold_ms = \"nope\"\ngap_ms = 250\ncoyote_ms = 80\n").unwrap();
        let cfg = FeelConfig::load_from(&path);
        assert_eq!(cfg.gap_ms, 250, "the good sibling scalar survived");
        assert_eq!(cfg.coyote_ms, 80);
        assert_eq!(
            cfg.hold_ms,
            FeelConfig::default().hold_ms,
            "the malformed scalar fell back to its own default"
        );
        assert!(dir.join("feel.toml.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every field wrong-typed at once (or absent) must land on the FULL default — no field's
    /// wiring may panic, and none may leak a partial (e.g. a half-defaulted `LayerMode`).
    #[test]
    fn degraded_feel_all_fields_wrong_typed_matches_full_default() {
        use crate::salvage::SalvageLoad;
        let dir = std::env::temp_dir().join(format!("neuron-feel-allbad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feel.toml");
        std::fs::write(
            &path,
            "hold_ms = \"nope\"\ngap_ms = \"nope\"\ncoyote_ms = \"nope\"\nhypershift = 123\n",
        )
        .unwrap();
        let cfg = FeelConfig::load_from(&path);
        assert_eq!(
            cfg,
            FeelConfig::default(),
            "every field malformed must salvage to exactly the default, not a partial mix"
        );
        assert!(dir.join("feel.toml.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Exactly one field (`coyote_ms`) is well-typed; every sibling — including the enum field
    /// `hypershift` — must default independently, and the survivor must not be disturbed by its
    /// malformed neighbours.
    #[test]
    fn degraded_feel_exactly_one_field_valid_survives_alone() {
        use crate::salvage::SalvageLoad;
        let dir = std::env::temp_dir().join(format!("neuron-feel-onegood-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("feel.toml");
        std::fs::write(
            &path,
            "hold_ms = \"nope\"\ngap_ms = \"nope\"\ncoyote_ms = 999\nhypershift = 123\n",
        )
        .unwrap();
        let cfg = FeelConfig::load_from(&path);
        let d = FeelConfig::default();
        assert_eq!(cfg.coyote_ms, 999, "the one valid field survived");
        assert_eq!(cfg.hold_ms, d.hold_ms);
        assert_eq!(cfg.gap_ms, d.gap_ms);
        assert_eq!(cfg.hypershift, d.hypershift, "the malformed enum field defaulted too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cfg() -> FeelConfig {
        FeelConfig::default() // hold 200 / gap 280 / coyote 120
    }

    // drive a watcher through (t, down) samples; collect non-pending results with timestamps.
    fn run(w: &mut PhraseWatcher, samples: &[(u64, bool)]) -> Vec<(u64, Watch)> {
        samples
            .iter()
            .filter_map(|&(t, d)| match w.feed(t, d) {
                Watch::Pending => None,
                other => Some((t, other)),
            })
            .collect()
    }

    #[test]
    fn phrase_parse_and_describe_roundtrip() {
        for s in ["hold", "tap hold", "tap tap hold", "tap tap", "tap"] {
            let p = Phrase::parse(s).unwrap();
            assert_eq!(p.describe(), s);
        }
        assert_eq!(Phrase::parse("").unwrap(), Phrase::hold());
        assert!(
            Phrase::parse("hold tap").is_err(),
            "interior hold is unparseable"
        );
        assert!(Phrase::parse("bonk").is_err());
    }

    #[test]
    fn plain_hold_activates_on_down_with_zero_latency() {
        let mut w = PhraseWatcher::new(Phrase::hold(), &cfg());
        let out = run(&mut w, &[(0, false), (10, true)]);
        assert_eq!(out, vec![(10, Watch::Activated { toggle: false })]);
    }

    #[test]
    fn tap_hold_activates_on_the_second_down() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap hold").unwrap(), &cfg());
        let out = run(
            &mut w,
            &[(0, true), (80, false), (160, true)], // tap (80ms), then the hold press
        );
        assert_eq!(out, vec![(160, Watch::Activated { toggle: false })]);
    }

    #[test]
    fn double_tap_toggle_waits_for_the_final_short_release() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap tap").unwrap(), &cfg());
        let out = run(&mut w, &[(0, true), (60, false), (140, true)]);
        assert!(out.is_empty(), "a press-down cannot yet be classified as a tap");
        assert_eq!(w.feed(200, false), Watch::Activated { toggle: true });
    }

    #[test]
    fn long_final_press_cannot_activate_a_tap_toggle() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap tap").unwrap(), &cfg());
        let out = run(&mut w, &[(0, true), (60, false), (140, true), (340, true)]);
        assert_eq!(out, vec![(340, Watch::Reset)]);
        assert_eq!(w.feed(350, false), Watch::Pending);
    }

    #[test]
    fn slow_gap_breaks_the_rhythm_then_rearms_instantly() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap hold").unwrap(), &cfg());
        // tap, then silence past gap_ms → reset; the NEXT press starts a fresh phrase.
        let out = run(&mut w, &[(0, true), (60, false), (500, false)]);
        assert_eq!(out, vec![(500, Watch::Reset)]);
        // fresh tap+press immediately works — no lockout, no cooldown.
        let out2 = run(&mut w, &[(600, true), (660, false), (740, true)]);
        assert_eq!(out2, vec![(740, Watch::Activated { toggle: false })]);
    }

    #[test]
    fn long_press_where_tap_expected_breaks_quietly() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap hold").unwrap(), &cfg());
        // held past hold_ms while a TAP was expected → early reset (not left holding a dead rhythm)
        let out = run(&mut w, &[(0, true), (100, true), (250, true)]);
        assert_eq!(out, vec![(250, Watch::Reset)]);
    }

    #[test]
    fn late_press_after_dead_gap_restarts_the_phrase() {
        let mut w = PhraseWatcher::new(Phrase::parse("tap hold").unwrap(), &cfg());
        // tap … long silence … then a press: that press is a FRESH first tap, not the hold.
        let mut fired = Vec::new();
        for (t, d) in [
            (0u64, true),
            (60, false),
            (700, true),
            (760, false),
            (840, true),
        ] {
            if let Watch::Activated { .. } = run_one(&mut w, t, d) {
                fired.push(t);
            }
        }
        assert_eq!(fired, vec![840], "the post-gap press restarted the phrase");
    }

    fn run_one(w: &mut PhraseWatcher, t: u64, d: bool) -> Watch {
        w.feed(t, d)
    }

    #[test]
    fn spam_friendly_repeated_activation() {
        // hold-activate, release, hold-activate again immediately: every press fires.
        let mut w = PhraseWatcher::new(Phrase::hold(), &cfg());
        let mut count = 0;
        let samples = [
            (0u64, true),
            (50, false),
            (60, true),
            (110, false),
            (120, true),
        ];
        for (t, d) in samples {
            if matches!(w.feed(t, d), Watch::Activated { .. }) {
                count += 1;
            }
        }
        assert_eq!(
            count, 3,
            "every spam press activates — no refractory period"
        );
    }

    #[test]
    fn recording_normalizes_into_a_phrase() {
        let c = cfg();
        // two quick presses then a long final press → tap tap hold
        let rec = [(0u64, Some(70u64)), (150, Some(230)), (320, None)];
        assert_eq!(
            Phrase::from_recording(&rec, &c).unwrap().describe(),
            "tap tap hold"
        );
        // a single long press → hold
        assert_eq!(
            Phrase::from_recording(&[(0, Some(400))], &c)
                .unwrap()
                .describe(),
            "hold"
        );
        // two quick taps → the toggle phrase
        assert_eq!(
            Phrase::from_recording(&[(0, Some(60)), (140, Some(200))], &c)
                .unwrap()
                .describe(),
            "tap tap"
        );
        assert!(Phrase::from_recording(&[], &c).is_none());
    }

    #[test]
    fn feel_config_roundtrips_and_defaults() {
        let c = FeelConfig::default();
        let s = toml::to_string_pretty(&c).unwrap();
        let back: FeelConfig = toml::from_str(&s).unwrap();
        assert_eq!(back, c);
        // partial files keep defaults for the rest.
        let part: FeelConfig = toml::from_str("hold_ms = 150").unwrap();
        assert_eq!(part.hold_ms, 150);
        assert_eq!(part.gap_ms, d_gap_ms());
        assert_eq!(part.hypershift, LayerMode::Hold);
        // stances parse from their kebab names.
        let m: FeelConfig = toml::from_str("hypershift = \"smart\"").unwrap();
        assert_eq!(m.hypershift, LayerMode::Smart);
    }

    #[test]
    fn layer_mode_parse_describe_roundtrip() {
        for m in [
            LayerMode::Hold,
            LayerMode::Latch,
            LayerMode::Smart,
            LayerMode::OneShot,
        ] {
            assert_eq!(LayerMode::parse(m.describe()), m);
        }
        assert_eq!(LayerMode::parse("1-shot"), LayerMode::OneShot);
        assert_eq!(LayerMode::parse("whatever"), LayerMode::Hold);
    }
}
