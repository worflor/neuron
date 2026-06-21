//! The cast engine — **the [`crate::spellweaving`] resolver**, the one entry point for the whole
//! weave continuum. Hold the trigger and weave a stroke; on release it resolves to an `Action`,
//! whether that stroke is the degenerate weave (a directional *flick* → radial sector) or a rich
//! weave (a drawn *shape* → glyph). Radial and glyph aren't two engines here — they're two points
//! on one continuum this resolver reads. Pure host-side, no device writes.
//!
//! Auto mode branches by stroke geometry: a confidently-recognized glyph wins; otherwise a
//! straight, committed flick reads as a radial pick. Config is serde data (GUI-ready); the
//! gesture *templates* live in the gesture Vault, this maps their names to actions.

use crate::action::Action;
use crate::gesture::Vault;
use crate::glyph::{self, C};
use crate::radial;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// glyph if confidently recognized, else a clear flick = radial.
    #[default]
    Auto,
    /// always treat the stroke as a directional flick.
    Radial,
    /// always run glyph recognition.
    Gesture,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CastConfig {
    /// activation button (VK): hold to capture. Default XBUTTON1 (0x05).
    pub trigger: i32,
    /// the activation RHYTHM on that button — a [`crate::feel::Phrase`] ("hold", "tap hold",
    /// "tap tap hold", "tap tap", …). The classic single hold is the default; rhythms let the
    /// same physical button keep its normal click AND open a weave (e.g. double-tap-then-hold).
    #[serde(default = "d_activation")]
    pub activation: String,
    /// radial sector count N.
    pub sectors: usize,
    /// minimum net flick distance to count as a radial pick.
    #[serde(default = "d_deadzone")]
    pub deadzone: f64,
    /// SPELL ASSIST — the aim-assist for glyphs. A stroke that misses the recognition threshold
    /// by up to this fraction still SNAPS to the closest spell, but only when that spell is a
    /// clear winner (≥25% separation from the runner-up). 0 disables. The Overwatch rule:
    /// predict intent, snap only when unambiguous, never fight a deliberate miss.
    #[serde(default = "d_assist")]
    pub assist: f64,
    #[serde(default)]
    pub mode: Mode,
    /// per-sector actions (index = sector; sector 0 = North, clockwise).
    #[serde(default)]
    pub radial: Vec<Action>,
    /// HYPERSHIFT RADIAL — a parallel wedge set (SAME sector count + same backend as `radial`) shown
    /// only while a HyperShift layer is held, when `hyper_radial_on`. Just a second action vector the
    /// overlay swaps to; everything else (resolve/render/edit) is reused. Off by default.
    #[serde(default)]
    pub hyper_radial: Vec<Action>,
    /// Enable the HyperShift radial swap. OFF by default — the wheel stays the same under HyperShift
    /// until you opt in.
    #[serde(default)]
    pub hyper_radial_on: bool,
    /// gesture name -> action (names refer to templates in the gesture Vault).
    #[serde(default)]
    pub gestures: BTreeMap<String, Action>,

    // ── CAST RHYTHM MAP — each rhythm on the cast trigger (N taps then hold) is a FIRST-CLASS
    // `Trigger::Cast { taps }` the engine resolves to an `Action` (remappable to anything). The
    // plain hold (taps=0) stays the WEAVE itself; the other rhythms are the source of truth for
    // what the cast trigger opens after a tap-then-hold. See [`mode_slots`] for validation.
    /// rhythm bindings on the cast trigger: each `taps`-then-hold maps to an `Action`. Default =
    /// teleport on tap-then-hold (taps=1) — preserving today's out-of-the-box behaviour.
    #[serde(default = "d_rhythm_actions")]
    pub rhythm_actions: Vec<RhythmBind>,
}

/// One CAST RHYTHM binding: `taps` quick taps then hold on the cast trigger, resolving to
/// `action`. taps=0 is reserved for the weave itself (the plain hold) — a `rhythm_actions` entry
/// with taps=0 is ignored by [`CastConfig::mode_slots`] so the weave always owns the plain hold.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RhythmBind {
    pub taps: u8,
    pub action: Action,
}

/// One validated activation slot: WHICH action, on WHICH key, after HOW MANY taps (then hold),
/// and whether this is the weave slot itself. Produced by [`CastConfig::mode_slots`].
#[derive(Clone, Debug, PartialEq)]
pub struct ModeSlot {
    /// The action this rhythm fires (`Action::Noop` for the weave slot — the weave resolves its
    /// own stroke, it has no fixed action).
    pub action: Action,
    pub vk: i32,
    pub taps: u8,
    /// true ⇔ the weave slot (the plain-hold cast capture itself).
    pub is_weave: bool,
}

impl ModeSlot {
    /// The capture-state-machine id this slot activates with (the contract `beacon::live_weave`
    /// dispatches on): 0 = the weave; 1 = teleport's dedicated aim; 2 = whiteboard's session;
    /// `100 + taps` = a generic "fire via `Trigger::Cast { taps }`" rhythm (any other bound
    /// action — routed back through the engine spine rather than a hard-wired instrument).
    pub fn capture_id(&self) -> u32 {
        if self.is_weave {
            0
        } else {
            match self.action {
                Action::Teleport => 1,
                Action::Whiteboard => 2,
                _ => 100 + self.taps as u32,
            }
        }
    }
}

/// Human label for a "N taps then hold" rhythm — "hold", "tap hold", "tap tap hold", … (used in
/// honest collision complaints and the GUI rhythm-map rows).
pub fn taps_phrase(taps: u8) -> String {
    let mut parts: Vec<&str> = vec!["tap"; taps as usize];
    parts.push("hold");
    parts.join(" ")
}

fn d_deadzone() -> f64 {
    40.0
}
fn d_activation() -> String {
    "hold".into()
}
fn d_assist() -> f64 {
    0.35
}
/// The default cast rhythm map: teleport on tap-then-hold (taps=1) — exactly today's default
/// (weave on the plain hold + teleport on tap-hold). Whiteboard stays OFF by default.
fn d_rhythm_actions() -> Vec<RhythmBind> {
    vec![RhythmBind {
        taps: 1,
        action: Action::Teleport,
    }]
}

impl Default for CastConfig {
    fn default() -> Self {
        CastConfig {
            trigger: 0x05,
            activation: d_activation(),
            sectors: 8,
            deadzone: d_deadzone(),
            assist: d_assist(),
            mode: Mode::Auto,
            radial: Vec::new(),
            hyper_radial: Vec::new(),
            hyper_radial_on: false,
            gestures: BTreeMap::new(),
            rhythm_actions: d_rhythm_actions(),
        }
    }
}

/// What a stroke resolved to.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    pub kind: &'static str, // "radial" | "gesture"
    pub label: String,
    pub action: Action,
    /// true when SPELL ASSIST snapped a near-miss to the closest spell (reported honestly —
    /// the user should be able to see when the engine met them halfway).
    pub assisted: bool,
    /// the picked wedge index for a radial resolve (structured — so a live dispatcher can emit
    /// `Trigger::RadialSector` without parsing the human label). None for gestures.
    pub sector: Option<usize>,
}

impl CastConfig {
    pub fn path() -> PathBuf {
        PathBuf::from("cast.toml")
    }
    /// The radial action set to use RIGHT NOW: the HyperShift set when a HyperShift layer is held AND
    /// it's enabled AND populated; otherwise the base set. An empty/disabled hyper set transparently
    /// falls back, so the wheel never goes blank under HyperShift just because it isn't authored yet.
    pub fn active_radial(&self, hypershift_held: bool) -> &[Action] {
        if hypershift_held && self.hyper_radial_on && !self.hyper_radial.is_empty() {
            &self.hyper_radial
        } else {
            &self.radial
        }
    }
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("cast.toml parse error ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// The VALIDATED activation slots — THE one place rhythm conflicts are decided. Rules:
    ///   * the weave slot always exists (the cast trigger + its activation; a non-"taps then
    ///     hold" weave rhythm — e.g. a toggle phrase — yields taps=0 so the engine stays usable);
    ///   * one slot per `rhythm_actions` entry that binds a real action (`Action::Noop` and a
    ///     taps=0 entry are skipped — taps=0 is reserved for the weave's own plain hold);
    ///   * every rhythm slot rides the cast trigger — the rhythm IS the disambiguator;
    ///   * slots on the cast trigger must have DISTINCT tap counts — a collision drops the LATER
    ///     slot (the weave always wins; an earlier rhythm wins over a later same-tap one) and
    ///     says so.
    ///
    /// Returns (slots, complaints) — complaints are honest status lines, never silent.
    pub fn mode_slots(&self) -> (Vec<ModeSlot>, Vec<String>) {
        let mut slots: Vec<ModeSlot> = Vec::new();
        let mut complaints = Vec::new();
        let weave_taps = self.phrase().taps_then_hold().unwrap_or(0);
        // the weave slot ALWAYS exists — the cast engine must never vanish (an exotic/toggle
        // weave rhythm degrades to a plain hold for slot purposes).
        slots.push(ModeSlot {
            action: Action::Noop,
            vk: self.trigger,
            taps: weave_taps,
            is_weave: true,
        });
        for rb in &self.rhythm_actions {
            if rb.action == Action::Noop {
                continue; // an unbound rhythm contributes no slot — silence is correct
            }
            if rb.taps == 0 {
                // taps=0 is the weave's own plain hold — a rhythm can't steal it. Say so rather
                // than silently drop a config the user might think took effect.
                complaints.push(format!(
                    "rhythm 'hold' ({}) clashes with the weave's plain hold — disabled",
                    rb.action.describe()
                ));
                continue;
            }
            if let Some(clash) = slots.iter().find(|s| s.vk == self.trigger && s.taps == rb.taps) {
                let with = if clash.is_weave {
                    "the weave".to_string()
                } else {
                    clash.action.describe()
                };
                complaints.push(format!(
                    "rhythm '{}' ({}) clashes with {with} (same {}-tap rhythm) — disabled",
                    taps_phrase(rb.taps),
                    rb.action.describe(),
                    rb.taps
                ));
                continue;
            }
            slots.push(ModeSlot {
                action: rb.action.clone(),
                vk: self.trigger,
                taps: rb.taps,
                is_weave: false,
            });
        }
        (slots, complaints)
    }

    /// The parsed activation rhythm (an unparseable string degrades to the classic hold —
    /// a config typo must never brick the cast trigger).
    pub fn phrase(&self) -> crate::feel::Phrase {
        crate::feel::Phrase::parse(&self.activation).unwrap_or_else(|_| crate::feel::Phrase::hold())
    }

    fn radial_action(&self, sector: usize) -> Action {
        self.radial.get(sector).cloned().unwrap_or_default()
    }

    fn try_radial(&self, dx: f64, dy: f64, net: f64) -> Option<Resolved> {
        if net < self.deadzone {
            return None;
        }
        let s = radial::sector_for(dx, dy, self.sectors);
        Some(Resolved {
            kind: "radial",
            label: format!("sector {s} ({})", radial::compass(s, self.sectors)),
            action: self.radial_action(s),
            assisted: false,
            sector: Some(s),
        })
    }

    fn try_gesture(&self, path: &[C], vault: &Vault) -> Option<Resolved> {
        let q = glyph::analyze(path, &vault.config);
        let r = vault.recognize(&q);
        if let Some(name) = r.name {
            let action = self.gestures.get(&name).cloned().unwrap_or_default();
            return Some(Resolved {
                kind: "gesture",
                label: name,
                action,
                assisted: false,
                sector: None,
            });
        }
        // SPELL ASSIST: the stroke missed the gate, but if the closest spell is within the
        // assist margin AND a clear winner over the runner-up, snap to it. Predict intent;
        // never guess between two contenders (an ambiguous miss stays a miss).
        if self.assist > 0.0 {
            if let Some((name, score, runner_up)) = vault.predict(&q) {
                let within = score <= vault.config.threshold * (1.0 + self.assist);
                let clear = runner_up
                    .map(|ru| ru - score >= 0.25 * score)
                    .unwrap_or(true);
                if within && clear {
                    let action = self.gestures.get(&name).cloned().unwrap_or_default();
                    return Some(Resolved {
                        kind: "gesture",
                        label: name,
                        action,
                        assisted: true,
                        sector: None,
                    });
                }
            }
        }
        None
    }

    /// Pure resolution of a captured stroke into an action (given the gesture vault).
    ///
    /// The radial reads INTENT, not geometric purity: direction comes from
    /// [`radial::intent_vector`] (arc-recency attention over the WHOLE stroke — the latest
    /// motion dominates, the history still votes) and the commit gate stays the NET
    /// displacement (ending back at the center is a cancel, never a misfire). A circling
    /// approach, a changed mind, a wandering start all land the wedge the hand MEANT — no
    /// geometric-purity gate demands penmanship.
    pub fn resolve(&self, path: &[C], vault: &Vault) -> Option<Resolved> {
        if path.len() < 3 {
            return None;
        }
        let (dx, dy) = radial::net_displacement(path);
        let net = (dx * dx + dy * dy).sqrt();
        let (ix, iy) = radial::intent_vector(path);
        // degenerate guard: a stroke with no attention mass falls back to its net direction
        let (ix, iy) = if ix * ix + iy * iy > 1e-12 {
            (ix, iy)
        } else {
            (dx, dy)
        };

        match self.mode {
            Mode::Radial => self.try_radial(ix, iy, net),
            Mode::Gesture => self.try_gesture(path, vault),
            // glyphs get first claim (a recognized/assisted spell is the stronger intent);
            // everything else resolves as the wedge the stroke was reaching for.
            Mode::Auto => self
                .try_gesture(path, vault)
                .or_else(|| self.try_radial(ix, iy, net)),
        }
    }
}

/// Hand-written template for `cast init` (comments survive vs serialize).
pub const TEMPLATE_TOML: &str = r#"# Neuron cast engine — hold the trigger, flick a direction (radial) or draw a shape (glyph).
#
# trigger: VK of the hold button. 0x05 = mouse thumb button 1 (XBUTTON1).
# mode: "auto" (glyph if recognized, else a straight flick = radial) | "radial" | "gesture"
# sectors: radial wedge count (8/10/12). Sector 0 = North/up, clockwise.
#
# Record glyphs with:  neuron gesture record <name>
# Then map their names under [gestures] below. Map radial wedges under [[radial]] (index order:
# 0=N, 1=NE, 2=E, ...). Actions = any typed Action variant (see action.rs), e.g.
#   key | mouse-button | media | run | sequence | script | mic-mute | mic-gain |
#   output-mute | output-gain | dpi-set | dpi-cycle | scroll-stage-cycle |
#   profile-switch | profile-cycle | turbo | noop
# written as { type = "<variant>", ... }, e.g. { type = "key", key = "1" }.

trigger = 0x05
activation = "hold"   # trigger rhythm: "hold" | "tap hold" | "tap tap hold" …
sectors = 8
mode = "auto"
deadzone = 40.0
assist = 0.0          # spell-assist margin (0 = off; snaps a near-miss to a clear winner)

# radial wedges (positional: index = sector). An in-game comms wheel of keybinds:
[[radial]]            # 0  N
type = "key"
key = "1"
[[radial]]            # 1  NE
type = "key"
key = "2"
[[radial]]            # 2  E
type = "key"
key = "3"
[[radial]]            # 3  SE
type = "key"
key = "4"
[[radial]]            # 4  S
type = "key"
key = "5"
[[radial]]            # 5  SW
type = "noop"
[[radial]]            # 6  W
type = "mic-mute"
mode = "toggle"
[[radial]]            # 7  NW
type = "noop"

# glyph spells: gesture name (from the Vault) -> action
[gestures.circle_cw]
type = "run"
cmd = "echo cast circle"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyph::{add_noise, synth_circle, synth_line};

    #[test]
    fn cast_with_a_wedge_serializes_to_valid_toml() {
        // a non-empty radial makes `radial`/`gestures` serialize as TABLES; every scalar field
        // MUST come before them or TOML rejects "value after table" — the wedge-save crash.
        let mut c = CastConfig::default();
        c.radial = vec![
            Action::Tether {
                slot: String::new(),
                mode: Default::default(),
            },
            Action::Noop,
        ];
        let body = toml::to_string_pretty(&c).expect("cast must serialize to TOML");
        let back: CastConfig = toml::from_str(&body).expect("and round-trip back");
        assert_eq!(back.radial.len(), 2);
    }

    fn vault_with_shapes() -> Vault {
        use std::f64::consts::TAU;
        let gc = glyph::GlyphConfig::default();
        let mut v = Vault::default();
        v.upsert(
            "circle_cw",
            glyph::analyze(&synth_circle(140, 400.0, TAU / 140.0), &gc),
        );
        v.upsert(
            "circle_ccw",
            glyph::analyze(&synth_circle(140, 400.0, -TAU / 140.0), &gc),
        );
        v
    }

    fn cfg(mode: Mode) -> CastConfig {
        let mut c = CastConfig {
            mode,
            ..Default::default()
        };
        c.radial = vec![Action::Key { key: "1".into() }; 8];
        c.gestures
            .insert("circle_cw".into(), Action::Run { cmd: "x".into() });
        c
    }

    fn flick(dx: f64, dy: f64) -> Vec<C> {
        // a straight committed flick: ~100 units in a direction
        (0..20)
            .map(|i| C::new(dx * i as f64 * 5.0, dy * i as f64 * 5.0))
            .collect()
    }

    /// Defaults: three slots on the ONE cast trigger, disambiguated by tap count (0/1/2),
    /// zero complaints — the out-of-the-box experience. Whiteboard is OFF by default now (its
    /// tap-tap-hold auto-triggered too easily — it's opt-in / action-only), so the defaults are
    /// the weave (hold) + teleport (tap-hold) on the one trigger.
    #[test]
    fn mode_slots_default_share_the_cast_trigger() {
        let c = CastConfig::default();
        let (slots, complaints) = c.mode_slots();
        assert!(
            complaints.is_empty(),
            "defaults must be clean: {complaints:?}"
        );
        assert_eq!(
            slots.len(),
            2,
            "weave + teleport; whiteboard is off by default"
        );
        assert!(
            slots.iter().all(|s| s.vk == c.trigger),
            "default = ride the cast trigger"
        );
        let taps: Vec<u8> = slots.iter().map(|s| s.taps).collect();
        assert_eq!(taps, vec![0, 1], "hold / tap-hold");
        // the weave owns the plain hold; teleport rides the tap-then-hold rhythm.
        assert!(slots.iter().any(|s| s.is_weave && s.taps == 0), "weave on hold");
        assert!(
            slots
                .iter()
                .any(|s| !s.is_weave && s.action == Action::Teleport && s.taps == 1),
            "teleport on tap-then-hold"
        );
        assert!(
            !slots.iter().any(|s| s.action == Action::Whiteboard),
            "whiteboard must not auto-bind to the cast trigger"
        );
    }

    /// Whiteboard is still a first-class slot when the user OPTS IN with a rhythm — it's only the
    /// default that changed, the capability is intact (the note: "keep it available / bindable").
    /// In the new model an opt-in = a `RhythmBind` mapping a free tap-count to `Action::Whiteboard`.
    #[test]
    fn mode_slots_whiteboard_opt_in_still_works() {
        let mut c = CastConfig::default();
        c.rhythm_actions.push(RhythmBind {
            taps: 2,
            action: Action::Whiteboard,
        });
        let (slots, complaints) = c.mode_slots();
        assert!(complaints.is_empty(), "{complaints:?}");
        assert_eq!(slots.len(), 3, "weave + teleport + the opted-in whiteboard");
        let wb = slots
            .iter()
            .find(|s| s.action == Action::Whiteboard)
            .expect("whiteboard");
        assert_eq!(wb.taps, 2, "tap-tap-hold = 2 taps then hold");
        assert_eq!(wb.capture_id(), 2, "whiteboard keeps its dedicated capture id");
    }

    /// Any action can ride a rhythm — a rhythm bound to a NON-instrument action becomes a generic
    /// fire-via-`Trigger::Cast` slot (capture id `100 + taps`), the proof of the rhythm→action spine.
    #[test]
    fn mode_slots_arbitrary_action_is_a_fire_slot() {
        let c = CastConfig {
            rhythm_actions: vec![RhythmBind {
                taps: 1,
                action: Action::Key { key: "f".into() },
            }],
            ..Default::default()
        };
        let (slots, complaints) = c.mode_slots();
        assert!(complaints.is_empty(), "{complaints:?}");
        let fire = slots
            .iter()
            .find(|s| !s.is_weave)
            .expect("the bound rhythm");
        assert_eq!(fire.capture_id(), 101, "fire-via-cast sentinel = 100 + taps");
    }

    /// A same-rhythm collision on the cast trigger DISABLES the later slot and SAYS SO — never
    /// guesses, never half-works. The weave always wins the plain hold; an earlier rhythm wins.
    #[test]
    fn mode_slots_collisions_disable_honestly() {
        // teleport on the plain hold collides with the weave's own hold (taps=0 is reserved).
        let c = CastConfig {
            rhythm_actions: vec![
                RhythmBind {
                    taps: 0,
                    action: Action::Teleport,
                },
                RhythmBind {
                    taps: 2,
                    action: Action::Whiteboard,
                },
            ],
            ..Default::default()
        };
        let (slots, complaints) = c.mode_slots();
        assert_eq!(
            slots.len(),
            2,
            "teleport must be dropped (steals the weave's hold), whiteboard survives"
        );
        assert!(!slots.iter().any(|s| s.action == Action::Teleport));
        assert!(slots.iter().any(|s| s.action == Action::Whiteboard));
        assert!(
            complaints.iter().any(|m| m.contains("disabled")),
            "the drop must be reported: {complaints:?}"
        );

        // two rhythms on the SAME tap count: the earlier wins, the later is dropped honestly.
        let c2 = CastConfig {
            rhythm_actions: vec![
                RhythmBind {
                    taps: 1,
                    action: Action::Teleport,
                },
                RhythmBind {
                    taps: 1,
                    action: Action::Whiteboard,
                },
            ],
            ..Default::default()
        };
        let (slots, complaints) = c2.mode_slots();
        assert!(slots.iter().any(|s| s.action == Action::Teleport));
        assert!(!slots.iter().any(|s| s.action == Action::Whiteboard));
        assert!(
            complaints.iter().any(|m| m.contains("disabled")),
            "{complaints:?}"
        );
    }

    /// An unbound (`Noop`) rhythm = silently off; a taps=0 rhythm = disabled WITH a complaint (it
    /// would steal the weave's plain hold — say it, don't guess).
    #[test]
    fn mode_slots_disable_paths() {
        // an explicitly empty rhythm map → only the weave slot remains, no complaint (an
        // explicit/default off is silence, never a nag).
        let off = CastConfig {
            rhythm_actions: vec![],
            ..Default::default()
        };
        let (slots, complaints) = off.mode_slots();
        assert_eq!(slots.len(), 1, "just the weave when no rhythms are bound");
        assert!(complaints.is_empty(), "explicit off is not a complaint");

        // a Noop rhythm is silently skipped (an unbound row contributes nothing).
        let noop = CastConfig {
            rhythm_actions: vec![RhythmBind {
                taps: 1,
                action: Action::Noop,
            }],
            ..Default::default()
        };
        let (slots, complaints) = noop.mode_slots();
        assert_eq!(slots.len(), 1, "a Noop rhythm adds no slot");
        assert!(complaints.is_empty(), "a Noop rhythm is silent, not a nag");

        // a taps=0 rhythm a user binds is disabled WITH a complaint (it would steal the hold).
        let steal = CastConfig {
            rhythm_actions: vec![RhythmBind {
                taps: 0,
                action: Action::Whiteboard,
            }],
            ..Default::default()
        };
        let (slots, complaints) = steal.mode_slots();
        assert!(!slots.iter().any(|s| s.action == Action::Whiteboard));
        assert!(
            complaints.iter().any(|m| m.contains("hold")),
            "{complaints:?}"
        );
    }

    /// A toggle-phrase WEAVE activation degrades to taps=0 for slot purposes — the weave slot
    /// always exists (the cast engine must never vanish because of an exotic rhythm).
    #[test]
    fn mode_slots_weave_always_exists() {
        let c = CastConfig {
            activation: "tap tap".into(),
            ..Default::default()
        };
        let (slots, _) = c.mode_slots();
        assert!(slots.iter().any(|s| s.is_weave && s.taps == 0));
    }

    #[test]
    fn radial_mode_picks_sector_action() {
        let c = cfg(Mode::Radial);
        let v = vault_with_shapes();
        let r = c.resolve(&flick(1.0, 0.0), &v).unwrap(); // east
        assert_eq!(r.kind, "radial");
        assert!(r.label.contains("sector 2")); // E for n=8
        assert_eq!(r.action, Action::Key { key: "1".into() });
    }

    #[test]
    fn gesture_mode_recognizes_shape() {
        let c = cfg(Mode::Gesture);
        let v = vault_with_shapes();
        use std::f64::consts::TAU;
        let stroke = synth_circle(120, 300.0, TAU / 120.0); // a cw circle
        let r = c.resolve(&stroke, &v).unwrap();
        assert_eq!(r.kind, "gesture");
        assert_eq!(r.label, "circle_cw");
        assert_eq!(r.action, Action::Run { cmd: "x".into() });
    }

    #[test]
    fn auto_branches_flick_to_radial_and_shape_to_gesture() {
        let c = cfg(Mode::Auto);
        let v = vault_with_shapes();
        // a straight flick is not a confident circle -> radial
        let r = c.resolve(&flick(0.0, -1.0), &v).unwrap(); // north
        assert_eq!(r.kind, "radial", "straight flick should be radial");
        // a drawn circle -> gesture
        use std::f64::consts::TAU;
        let r2 = c
            .resolve(&synth_circle(120, 300.0, TAU / 120.0), &v)
            .unwrap();
        assert_eq!(r2.kind, "gesture", "circle should be a glyph");
    }

    #[test]
    fn deadzone_returns_none_in_radial() {
        let c = cfg(Mode::Radial);
        let v = vault_with_shapes();
        let tiny = vec![C::new(0.0, 0.0), C::new(3.0, 2.0), C::new(5.0, 1.0)];
        assert_eq!(c.resolve(&tiny, &v), None);
    }

    #[test]
    fn assist_snaps_a_near_miss_to_the_closest_spell() {
        use std::f64::consts::TAU;
        let gc = glyph::GlyphConfig::default();
        let mut v = Vault::default();
        v.upsert(
            "circle_cw",
            glyph::analyze(&synth_circle(140, 400.0, TAU / 140.0), &gc),
        );
        // tighten the gate so a noisy circle MISSES strict recognition…
        let strict = {
            let mut q = v.clone();
            q.config.threshold *= 0.05;
            q
        };
        let mut c = cfg(Mode::Gesture);
        let sloppy = add_noise(&synth_circle(90, 250.0, TAU / 90.0), 14.0, 7);
        // …confirm the strict gate refuses it on its own…
        c.assist = 0.0;
        assert_eq!(
            c.resolve(&sloppy, &strict),
            None,
            "strict gate refuses the sloppy circle"
        );
        // …then assist (generous margin for the test) snaps it to the closest spell, HONESTLY
        // marked as assisted.
        c.assist = 30.0;
        let r = c
            .resolve(&sloppy, &strict)
            .expect("assist should snap the near-miss");
        assert_eq!(r.kind, "gesture");
        assert_eq!(r.label, "circle_cw");
        assert!(r.assisted, "an assist snap reports itself");
    }

    #[test]
    fn assist_never_guesses_between_two_contenders() {
        use std::f64::consts::TAU;
        let gc = glyph::GlyphConfig::default();
        let mut v = Vault::default();
        // two near-identical templates: any near-miss is AMBIGUOUS between them.
        v.upsert(
            "a",
            glyph::analyze(&synth_circle(140, 400.0, TAU / 140.0), &gc),
        );
        v.upsert(
            "b",
            glyph::analyze(&synth_circle(141, 401.0, TAU / 141.0), &gc),
        );
        v.config.threshold *= 0.0001; // nothing passes the strict gate
        let mut c = cfg(Mode::Gesture);
        c.assist = 1000.0; // even an unbounded margin must not break the clear-winner rule
        let q = add_noise(&synth_circle(120, 300.0, TAU / 120.0), 8.0, 3);
        assert_eq!(
            c.resolve(&q, &v),
            None,
            "two contenders within 25% of each other = ambiguous = no snap"
        );
    }

    #[test]
    fn predict_names_the_closest_spell_regardless_of_gate() {
        use std::f64::consts::TAU;
        let gc = glyph::GlyphConfig::default();
        let mut v = Vault::default();
        v.upsert(
            "circle_cw",
            glyph::analyze(&synth_circle(140, 400.0, TAU / 140.0), &gc),
        );
        v.config.threshold = 0.0; // the gate passes nothing…
        let q = glyph::analyze(&synth_circle(100, 280.0, TAU / 100.0), &gc);
        assert!(v.recognize(&q).name.is_none());
        // …but predict still names what the stroke is BECOMING (the live autopredict line).
        let (name, score, _ru) = v.predict(&q).unwrap();
        assert_eq!(name, "circle_cw");
        assert!(score.is_finite());
        assert!(
            Vault::default().predict(&q).is_none(),
            "empty vault predicts nothing"
        );
    }

    #[test]
    fn template_parses_to_eight_wedges() {
        let c: CastConfig = toml::from_str(TEMPLATE_TOML).unwrap();
        assert_eq!(c.sectors, 8);
        assert_eq!(c.radial.len(), 8);
        assert_eq!(c.radial[0], Action::Key { key: "1".into() });
        assert!(c.gestures.contains_key("circle_cw"));
        let _ = add_noise(&synth_line(10), 0.0, 1); // keep import used across cfgs
    }
}
