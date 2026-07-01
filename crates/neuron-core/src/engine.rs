//! The unified spine: `Trigger -> Action`.
//!
//! The keystone insight of the whole project — bindings, spellweaving (gestures), the radial
//! menu, app-aware switching, HyperShift layers, the mic tap, AND macros are all the SAME
//! primitive: *something happened* ([`Trigger`]) *so do this* ([`crate::action::Action`]). This
//! module is the one dispatcher they collapse into. `bindings.rs`, `cast.rs` and the run-daemon
//! are views over this spine; downstream they migrate to producing [`Trigger`]s and registering
//! [`Rule`]s here.
//!
//! Ownership: this file (plus `action.rs`) is the SPINE agent's. The macro engine
//! (`macros/*`), device-write completion (`writes.rs`), and migration (`import.rs`) feed it.

use crate::action::Action;
use crate::macros::context::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;

/// Everything that can fire an action. One enum so a single dispatcher serves every input
/// source — there is no second code path for "a gesture" vs "a button" vs "a hotkey".
///
/// Serde round-trippable so a [`Rule`] (trigger + action) is a config row the GUI reads/writes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Trigger {
    /// A raw HID control event: usage `page` + `usage`, optionally restricted to a source
    /// device `pid`. This is the decoded Raw Input semantic from `controls.rs` (a button press,
    /// the headset knob, the mic-tap synthetic usage), not byte-matching.
    Input {
        page: u16,
        usage: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u16>,
    },
    /// A global keyboard hotkey — a virtual-key plus modifier flags (Ctrl/Alt/Shift/Win),
    /// registered OS-wide. `mods` is a small bitset (bit0 Ctrl, bit1 Alt, bit2 Shift, bit3 Win).
    Hotkey { vk: u16, mods: u8 },
    /// A recognized drawn glyph (spellweaving) — the name of a template in the gesture Vault.
    /// Struct variant (`{ kind = "gesture", name = "..." }`) because `Trigger` is internally
    /// tagged and serde can't serialize a tagged newtype wrapping a `String`.
    Gesture { name: String },
    /// A radial (pie / comms-wheel) flick: `menu` names the wheel, `sector` is the chosen wedge.
    RadialSector { menu: String, sector: u8 },
    /// The foreground application changed to one matching this needle (exe-name substring).
    AppFocus { app: String },
    /// A physical tap on the Seiren mic (detected via the Core-Audio mute toggle).
    MicTap,
    /// A held HyperShift / momentary layer is active — the named layer's second-tier bindings
    /// apply while the trigger is down (the software HyperShift the cast hold-model provides).
    Hold { layer: String },
    /// A spellweaving CAST RHYTHM on the cast trigger — `taps` quick taps then hold (0 = the
    /// plain hold). This makes each weave rhythm a FIRST-CLASS trigger the engine resolves to an
    /// `Action`, so "tap-then-hold opens teleport" is a real `Trigger -> Action` rule (remappable
    /// to any action), not a hard-wired instrument route in the capture state machine.
    Cast { taps: u8 },
}

impl Trigger {
    /// A short human description (for `show`, logs, and the GUI rule list).
    pub fn describe(&self) -> String {
        match self {
            Trigger::Input { page, usage, pid } => {
                // a friendly, layout-independent control name ("F13", "Button 4", "Left Ctrl") instead
                // of raw hex — the ONE place a HID control becomes rule-list text.
                let name = crate::controls::control_label(*page, *usage);
                // Macro keys are device-any logical controls riding a synthetic edge-bucket pid; the
                // name already says which key, so the pid is noise — never tack it on for them.
                match pid {
                    Some(p) if *page != crate::controls::RAZER_MACRO_PAGE => {
                        format!("{name} @pid {p:04x}")
                    }
                    _ => name,
                }
            }
            Trigger::Hotkey { vk, mods } => format!("hotkey vk=0x{vk:02X} mods=0b{mods:04b}"),
            Trigger::Gesture { name } => format!("gesture '{name}'"),
            Trigger::RadialSector { menu, sector } => format!("radial '{menu}' sector {sector}"),
            Trigger::AppFocus { app } => format!("app focus '{app}'"),
            Trigger::MicTap => "mic tap".into(),
            Trigger::Hold { layer } => format!("hold layer '{layer}'"),
            Trigger::Cast { taps } => format!("cast {taps}-tap rhythm"),
        }
    }
}

/// One spine entry: when `trigger` fires, run `action`. This is the row the GUI edits and the
/// importer emits — the entire remap/macro/cast surface is a `Vec<Rule>`.
///
/// ## HyperShift fidelity (`layer`)
/// A rule may belong to a named **HyperShift layer** rather than the base map. When `layer` is
/// `Some("sniper")`, this rule only dispatches while that layer is *held* (see [`Engine`]). This
/// makes an imported held-layer bind FIRST-CLASS: the migration importer tags a Synapse
/// `IsHyperShift=true` mapping with the layer it belongs to, so a surviving HyperShift rule is no
/// longer emitted indistinguishably from a base rule. `None` = a base-layer rule (the default).
///
/// Serde: `layer` defaults to `None` and is skipped when absent, so every pre-existing serialized
/// `Rule` (which had no `layer` key) still round-trips unchanged.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub trigger: Trigger,
    pub action: Action,
    /// The HyperShift layer this rule belongs to, or `None` for a base-layer rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

impl Rule {
    /// A base-layer rule (no HyperShift layer).
    pub fn new(trigger: Trigger, action: Action) -> Self {
        Rule {
            trigger,
            action,
            layer: None,
        }
    }
    /// A rule that lives on a named HyperShift layer — only dispatches while that layer is held.
    pub fn on_layer(layer: impl Into<String>, trigger: Trigger, action: Action) -> Self {
        Rule {
            trigger,
            action,
            layer: Some(layer.into()),
        }
    }
    pub fn summary(&self) -> String {
        match &self.layer {
            Some(l) => format!(
                "[{l}] {}  ->  {}",
                self.trigger.describe(),
                self.action.describe()
            ),
            None => format!(
                "{}  ->  {}",
                self.trigger.describe(),
                self.action.describe()
            ),
        }
    }
}

/// The on-disk shape of a `profiles/*.rules.toml` spine sidecar: a flat `[[rules]]` array. This
/// is the ONE definition for that schema — the migration importer (CLI + GUI), the GUI bindings
/// editor, and the daemon's sidecar loader all share it instead of each re-declaring an identical
/// `struct RuleDoc { rules: Vec<Rule> }`. The field is named `rules` and `#[serde(default)]`, so a
/// missing/empty array degrades to no rules and every existing `.rules.toml` parses unchanged.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RuleDoc {
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// The unified dispatcher. Holds the base [`Rule`] set plus any named **HyperShift layers**
/// (parallel rule sets), and tracks which layers are currently *held*. On a [`Trigger`] it finds
/// every matching rule — across the base layer and all active held layers — and runs its action.
/// This replaces the per-subsystem dispatch in `bindings.rs` and `cast.rs`: they become producers
/// of [`Trigger`]s feeding one `Engine`.
///
/// ## HyperShift (held layers) — the keystone
/// HyperShift is, in Synapse's own model, a parallel `IsHyperShift=true` binding list: a second
/// tier of mappings that apply **while a hold key is down**. Neuron's cast hold-model is the same
/// idea. Here it is first-class: a [`Trigger::Hold { layer }`] press makes the named layer
/// *active*, and while active that layer's rules are dispatched ON TOP of the base. The same
/// physical input (`Input`/`Hotkey`/…) can therefore mean one thing normally and another while a
/// layer is held — the software-HyperShift the docs describe, with no firmware write.
///
/// Layer activation is itself just a rule firing: bind a `Trigger::Hold { layer }` and the
/// daemon, on the hold key's down/up edges, calls [`Engine::hold`] / [`Engine::release`]. A
/// `Hold` rule may *also* carry an action (e.g. a visual cue) — that action fires on the press
/// like any other rule.
#[derive(Clone, Debug, Default)]
pub struct Engine {
    /// The base rules (order = priority; all matches fire, like the old bindings dispatch).
    pub rules: Vec<Rule>,
    /// Named HyperShift layers: each is a parallel rule set that applies only while its layer is
    /// in [`held`](Engine::held). Sorted (BTreeMap) so dispatch order across layers is
    /// deterministic.
    pub layers: BTreeMap<String, Vec<Rule>>,
    /// Currently-held layer names. A layer's rules dispatch iff its name is in this set.
    held: BTreeSet<String>,
}

impl Engine {
    /// Build an engine from a base rule set (no layers).
    pub fn new(rules: Vec<Rule>) -> Self {
        Engine {
            rules,
            layers: BTreeMap::new(),
            held: BTreeSet::new(),
        }
    }

    /// Build an engine from a FLAT rule list, grouping rules into the base map vs named HyperShift
    /// layers by each rule's [`Rule::layer`] tag. This is the bridge from the importer / on-disk
    /// config (a single `Vec<Rule>` where HyperShift membership is carried per-rule) to the
    /// [`Engine`]'s base-plus-layers shape:
    ///
    /// * `layer == None`   -> appended to [`Engine::rules`] (the base map).
    /// * `layer == Some(l)` -> appended to `layers[l]` (created on first use).
    ///
    /// Document order is preserved within each group, so first-listed rules keep dispatch
    /// priority. This reconciles with [`with_layer_rule`](Engine::with_layer_rule): both feed the
    /// same `layers` map, so an engine built `from_rules` behaves identically to one assembled with
    /// the builders. The rules' `layer` tags are retained as-is (the grouping is non-destructive).
    pub fn from_rules(rules: Vec<Rule>) -> Self {
        let mut base = Vec::new();
        let mut layers: BTreeMap<String, Vec<Rule>> = BTreeMap::new();
        for rule in rules {
            match &rule.layer {
                Some(layer) => layers.entry(layer.clone()).or_default().push(rule),
                None => base.push(rule),
            }
        }
        Engine {
            rules: base,
            layers,
            held: BTreeSet::new(),
        }
    }

    /// The inverse of [`from_rules`](Engine::from_rules): flatten the base map + all named layers
    /// back into one `Vec<Rule>` (base first, then layers in sorted name order), each rule carrying
    /// its `layer` tag. Lets the GUI/importer round-trip an engine through a flat config list
    /// without losing HyperShift membership.
    pub fn to_rules(&self) -> Vec<Rule> {
        let mut out: Vec<Rule> = self
            .rules
            .iter()
            .map(|r| Rule {
                layer: None,
                ..r.clone()
            })
            .collect();
        for (name, rules) in &self.layers {
            out.extend(rules.iter().map(|r| Rule {
                layer: Some(name.clone()),
                ..r.clone()
            }));
        }
        out
    }

    /// Add a base rule (builder-style chaining for setup code).
    pub fn with_rule(mut self, rule: Rule) -> Self {
        self.rules.push(rule);
        self
    }

    /// Add a rule to a named HyperShift layer (builder-style). The layer is created on first use.
    pub fn with_layer_rule(mut self, layer: impl Into<String>, rule: Rule) -> Self {
        self.layers.entry(layer.into()).or_default().push(rule);
        self
    }

    // --- held-layer (HyperShift) state ---------------------------------------------------

    /// Mark a layer as held — its rules now dispatch. Idempotent (re-holding is a no-op).
    /// Called on the hold key's *down* edge.
    pub fn hold(&mut self, layer: impl Into<String>) {
        let layer = layer.into();
        // confirm only on a REAL transition (insert returns false when already held), so a
        // key-repeat down-edge can't fire a second card.
        if self.held.insert(layer.clone()) {
            crate::confirm::layer(&layer, true);
        }
    }

    /// Release a held layer — its rules stop dispatching. Called on the hold key's *up* edge.
    pub fn release(&mut self, layer: &str) {
        if self.held.remove(layer) {
            crate::confirm::layer(layer, false);
        }
    }

    /// Release every held layer (e.g. on focus loss / daemon pause, so a layer can't get stuck).
    pub fn release_all(&mut self) {
        self.held.clear();
    }

    /// Is this layer currently held?
    pub fn is_held(&self, layer: &str) -> bool {
        self.held.contains(layer)
    }

    /// The set of currently-held layer names (for the GUI/tray "HyperShift active" indicator).
    pub fn held_layers(&self) -> impl Iterator<Item = &str> {
        self.held.iter().map(String::as_str)
    }

    /// Convenience: drive a [`Trigger::Hold`] edge directly. `down=true` holds the layer,
    /// `down=false` releases it. For any other trigger this is a no-op returning `false`. The
    /// daemon calls this on the hold key's press/release so HyperShift activation stays in the
    /// one spine (a `Hold` trigger is both "fire its rule" AND "toggle its layer").
    pub fn drive_hold(&mut self, trigger: &Trigger, down: bool) -> bool {
        if let Trigger::Hold { layer } = trigger {
            if down {
                self.hold(layer.clone());
            } else {
                self.release(layer);
            }
            true
        } else {
            false
        }
    }

    // --- matching + resolution -----------------------------------------------------------

    /// Do two triggers match for dispatch purposes? Exact equality, except `Input` with no
    /// `pid` filter matches any source pid, and `AppFocus` uses a (case-insensitive) substring
    /// match (so a rule needle `"valorant"` fires on `"valorant.exe"`). Pure and side-effect
    /// free so it stays testable.
    pub fn matches(rule_trigger: &Trigger, fired: &Trigger) -> bool {
        match (rule_trigger, fired) {
            (
                Trigger::Input {
                    page: rp,
                    usage: ru,
                    pid: rpid,
                },
                Trigger::Input {
                    page: fp,
                    usage: fu,
                    pid: fpid,
                },
            ) => rp == fp && ru == fu && rpid.is_none_or(|p| Some(p) == *fpid),
            (Trigger::AppFocus { app: needle }, Trigger::AppFocus { app }) => {
                app.to_lowercase().contains(&needle.to_lowercase())
            }
            (a, b) => a == b,
        }
    }

    /// Find every rule whose trigger matches `fired`, across the base layer and all currently-held
    /// HyperShift layers. Read-only — for previewing what a trigger would do, and the basis of
    /// [`dispatch`](Engine::dispatch).
    ///
    /// Order is deterministic: held layers first (sorted by name), then the base — so a held
    /// HyperShift binding for an input is seen *before* the base binding for the same input. Use
    /// [`resolve_top`](Engine::resolve_top) when only the winning (override) action should fire.
    pub fn resolve(&self, fired: &Trigger) -> Vec<&Rule> {
        let mut out = Vec::new();
        for layer in self.held.iter() {
            if let Some(rules) = self.layers.get(layer) {
                out.extend(rules.iter().filter(|r| Self::matches(&r.trigger, fired)));
            }
        }
        out.extend(
            self.rules
                .iter()
                .filter(|r| Self::matches(&r.trigger, fired)),
        );
        out
    }

    /// Which named HyperShift layer(s) this trigger ACTIVATES — i.e. layers that contain a rule
    /// whose trigger matches `fired`. Unlike [`resolve`](Engine::resolve), this scans every layer
    /// UNCONDITIONALLY (held or not), because it answers "if this input is pressed, which layers
    /// should become held?" — the daemon's HyperShift hold-edge question. Returns sorted, de-duped
    /// layer names.
    pub fn layers_activated_by(&self, fired: &Trigger) -> Vec<String> {
        let mut out: Vec<String> = self
            .layers
            .iter()
            .filter(|(_, rules)| rules.iter().any(|r| Self::matches(&r.trigger, fired)))
            .map(|(name, _)| name.clone())
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Resolve to the single highest-priority matching rule, if any: a held-layer match wins over
    /// a base match for the same trigger (true HyperShift *override* semantics — while held, the
    /// layer's binding replaces the base one rather than firing alongside it). Within the same
    /// tier the first listed rule wins.
    pub fn resolve_top(&self, fired: &Trigger) -> Option<&Rule> {
        self.resolve(fired).into_iter().next()
    }

    // --- dispatch ------------------------------------------------------------------------

    /// Dispatch a trigger: run the action of every matching rule (base + held layers), with `ctx`
    /// — the captured world snapshot — threaded into each action via
    /// [`Action::run_ctx`](crate::action::Action::run_ctx). All matches fire (the old bindings
    /// dispatch semantics: e.g. a mic-tap can both un-mute and notify), held layers before base.
    ///
    /// `ctx` lets context-aware actions (a `Script`, a `Sequence` containing one) reason about a
    /// consistent foreground/cwd/clipboard/selection captured when the trigger fired. Returns one
    /// log line per fired rule.
    pub fn dispatch(&self, fired: &Trigger, ctx: &Context) -> Vec<String> {
        let mut log = Vec::new();
        for rule in self.resolve(fired) {
            let result = rule.action.run_ctx(ctx);
            log.push(format!("{}: {result}", rule.trigger.describe()));
        }
        log
    }

    /// Dispatch ONLY the winning override rule (see [`resolve_top`](Engine::resolve_top)) — for
    /// remap-style triggers where a held HyperShift binding should *replace* the base binding, not
    /// stack with it. Returns the single log line, or `None` if nothing matched.
    pub fn dispatch_top(&self, fired: &Trigger, ctx: &Context) -> Option<String> {
        let rule = self.resolve_top(fired)?;
        Some(format!(
            "{}: {}",
            rule.trigger.describe(),
            rule.action.run_ctx(ctx)
        ))
    }

    /// Capture the live world and dispatch — the daemon's one-call entry. Snapshots a
    /// [`Context`] now (foreground app / cwd / clipboard / selection / window-to-restore) so the
    /// fired actions see the moment the trigger occurred. Equivalent to
    /// `self.dispatch(fired, &Context::capture())`.
    pub fn fire(&self, fired: &Trigger) -> Vec<String> {
        self.dispatch(fired, &Context::capture())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;

    #[test]
    fn trigger_round_trips() {
        let t = Trigger::Input {
            page: 0x0C,
            usage: 0xE9,
            pid: Some(0x0529),
        };
        let s = toml::to_string(&t).unwrap();
        assert!(s.contains("kind = \"input\""));
        let back: Trigger = toml::from_str(&s).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn all_trigger_variants_serde_round_trip() {
        let variants = vec![
            Trigger::Input {
                page: 7,
                usage: 4,
                pid: None,
            },
            Trigger::Hotkey {
                vk: 0x70,
                mods: 0b0101,
            },
            Trigger::Gesture {
                name: "circle_cw".into(),
            },
            Trigger::RadialSector {
                menu: "comms".into(),
                sector: 3,
            },
            Trigger::AppFocus {
                app: "valorant".into(),
            },
            Trigger::MicTap,
            Trigger::Hold {
                layer: "sniper".into(),
            },
            Trigger::Cast { taps: 1 },
        ];
        for t in variants {
            let s = serde_json::to_string(&t).unwrap();
            let back: Trigger = serde_json::from_str(&s).unwrap();
            assert_eq!(back, t, "round-trip failed for {t:?}");
        }
    }

    #[test]
    fn input_pid_filter_matches_any_source_when_none() {
        let rule = Trigger::Input {
            page: 0x0C,
            usage: 0xE9,
            pid: None,
        };
        let fired = Trigger::Input {
            page: 0x0C,
            usage: 0xE9,
            pid: Some(0x0221),
        };
        assert!(Engine::matches(&rule, &fired));
        let restricted = Trigger::Input {
            page: 0x0C,
            usage: 0xE9,
            pid: Some(0x0529),
        };
        assert!(
            !Engine::matches(&restricted, &fired),
            "wrong pid must not match"
        );
    }

    #[test]
    fn app_focus_matches_by_substring() {
        let rule = Trigger::AppFocus {
            app: "valorant".into(),
        };
        assert!(Engine::matches(
            &rule,
            &Trigger::AppFocus {
                app: "valorant.exe".into()
            }
        ));
        assert!(!Engine::matches(
            &rule,
            &Trigger::AppFocus {
                app: "chrome.exe".into()
            }
        ));
    }

    #[test]
    fn dispatch_runs_all_matching_rules() {
        let engine = Engine::new(vec![
            Rule::new(Trigger::MicTap, Action::Noop),
            Rule::new(Trigger::MicTap, Action::Noop),
            Rule::new(Trigger::Gesture { name: "x".into() }, Action::Noop),
        ]);
        let ctx = Context::default();
        let log = engine.dispatch(&Trigger::MicTap, &ctx);
        assert_eq!(
            log.len(),
            2,
            "both mic-tap rules fire, the gesture rule does not"
        );
    }

    #[test]
    fn rule_round_trips() {
        let rule = Rule::new(
            Trigger::Gesture {
                name: "bolt".into(),
            },
            Action::Key { key: "f".into() },
        );
        let s = toml::to_string(&rule).unwrap();
        let back: Rule = toml::from_str(&s).unwrap();
        assert_eq!(back, rule);
    }

    #[test]
    fn base_rule_omits_layer_key_for_backcompat() {
        // A base rule serializes WITHOUT a `layer` key, so an old config (which had none) still
        // round-trips, and new base rules stay byte-compatible with the old shape.
        let rule = Rule::new(Trigger::MicTap, Action::Noop);
        let s = toml::to_string(&rule).unwrap();
        assert!(
            !s.contains("layer"),
            "base rule must not emit a layer key:\n{s}"
        );
        // An old serialized rule (no layer field) still deserializes, defaulting to base.
        let back: Rule =
            toml::from_str("[trigger]\nkind = \"mic-tap\"\n[action]\ntype = \"noop\"").unwrap();
        assert_eq!(back.layer, None);
    }

    #[test]
    fn layer_rule_round_trips_with_tag() {
        let rule = Rule::on_layer(
            "sniper",
            Trigger::Input {
                page: 0x09,
                usage: 0x05,
                pid: None,
            },
            Action::Key { key: "1".into() },
        );
        assert_eq!(rule.layer.as_deref(), Some("sniper"));
        let s = toml::to_string(&rule).unwrap();
        assert!(
            s.contains("layer = \"sniper\""),
            "layer tag serialized:\n{s}"
        );
        let back: Rule = toml::from_str(&s).unwrap();
        assert_eq!(back, rule);
    }

    #[test]
    fn from_rules_groups_base_vs_named_layers() {
        // A flat list mixing base + two layers (the shape the importer / config emits).
        let rules = vec![
            Rule::new(input(), Action::Key { key: "1".into() }),
            Rule::on_layer("sniper", input(), Action::Key { key: "9".into() }),
            Rule::on_layer("comms", Trigger::MicTap, Action::Noop),
            Rule::new(Trigger::MicTap, Action::Noop),
            Rule::on_layer(
                "sniper",
                Trigger::Gesture { name: "x".into() },
                Action::Noop,
            ),
        ];
        let engine = Engine::from_rules(rules);
        // Base got the two None-layer rules.
        assert_eq!(engine.rules.len(), 2, "two base rules");
        // Two named layers, sizes 2 (sniper) and 1 (comms).
        assert_eq!(engine.layers.len(), 2);
        assert_eq!(engine.layers["sniper"].len(), 2);
        assert_eq!(engine.layers["comms"].len(), 1);

        // Behaviour matches the builder-assembled engine: the sniper input only fires while held.
        assert_eq!(
            engine.resolve(&input()).len(),
            1,
            "base only before holding"
        );
        let mut engine = engine;
        engine.hold("sniper");
        assert_eq!(
            engine.resolve(&input()).len(),
            2,
            "base + held sniper layer"
        );
        assert_eq!(
            engine.resolve_top(&input()).unwrap().action,
            Action::Key { key: "9".into() },
            "held sniper layer overrides the base input bind"
        );
    }

    #[test]
    fn from_rules_reconciles_with_with_layer_rule() {
        // An engine built from a flat list must be equivalent (same dispatch) to one assembled with
        // the existing builders — proving from_rules feeds the SAME layers map.
        let flat = Engine::from_rules(vec![
            Rule::new(input(), Action::Noop),
            Rule::on_layer("L", input(), Action::Key { key: "a".into() }),
        ]);
        let built = Engine::new(vec![Rule::new(input(), Action::Noop)]).with_layer_rule(
            "L",
            Rule::on_layer("L", input(), Action::Key { key: "a".into() }),
        );
        // Same base + layer membership.
        assert_eq!(flat.rules.len(), built.rules.len());
        assert_eq!(flat.layers["L"].len(), built.layers["L"].len());
        // Same held-dispatch behaviour.
        let (mut a, mut b) = (flat, built);
        a.hold("L");
        b.hold("L");
        assert_eq!(a.resolve(&input()).len(), b.resolve(&input()).len());
    }

    #[test]
    fn to_rules_round_trips_through_from_rules() {
        // Flatten an engine and rebuild it — base + layer membership must survive intact.
        let engine = Engine::new(vec![Rule::new(input(), Action::Noop)])
            .with_layer_rule("L", Rule::on_layer("L", Trigger::MicTap, Action::Noop));
        let flat = engine.to_rules();
        // The flattened list carries layer tags (None for base, Some("L") for the layer rule).
        assert!(flat.iter().any(|r| r.layer.as_deref() == Some("L")));
        assert!(flat.iter().any(|r| r.layer.is_none()));
        let rebuilt = Engine::from_rules(flat);
        assert_eq!(rebuilt.rules.len(), engine.rules.len());
        assert_eq!(rebuilt.layers.len(), engine.layers.len());
        assert_eq!(rebuilt.layers["L"].len(), engine.layers["L"].len());
    }

    // --- HyperShift / held-layer tests --------------------------------------------------

    /// A common input trigger reused across the layer tests.
    fn input() -> Trigger {
        Trigger::Input {
            page: 0x09,
            usage: 0x05,
            pid: None,
        }
    }

    #[test]
    fn held_layer_rules_only_fire_while_held() {
        let mut engine = Engine::new(vec![Rule::new(input(), Action::Noop)])
            .with_layer_rule("sniper", Rule::new(input(), Action::Noop));

        // Base only when nothing is held.
        assert_eq!(
            engine.resolve(&input()).len(),
            1,
            "only the base rule before holding"
        );

        // Hold the layer -> both the layer rule and the base rule match.
        engine.hold("sniper");
        assert!(engine.is_held("sniper"));
        assert_eq!(engine.resolve(&input()).len(), 2, "layer + base while held");

        // Release -> back to base only.
        engine.release("sniper");
        assert!(!engine.is_held("sniper"));
        assert_eq!(engine.resolve(&input()).len(), 1, "base only after release");
    }

    #[test]
    fn held_layer_overrides_base_in_resolve_top() {
        // Same trigger, different actions in base vs layer. While held, the layer wins.
        let mut engine = Engine::new(vec![Rule::new(input(), Action::Key { key: "1".into() })])
            .with_layer_rule("shift", Rule::new(input(), Action::Key { key: "9".into() }));

        assert_eq!(
            engine.resolve_top(&input()).unwrap().action,
            Action::Key { key: "1".into() },
            "base binding wins when unheld"
        );

        engine.hold("shift");
        assert_eq!(
            engine.resolve_top(&input()).unwrap().action,
            Action::Key { key: "9".into() },
            "held layer binding overrides the base for the same input"
        );
    }

    #[test]
    fn resolve_orders_held_layers_before_base() {
        let mut engine = Engine::new(vec![Rule::new(input(), Action::Noop)])
            .with_layer_rule("a", Rule::new(input(), Action::Key { key: "a".into() }))
            .with_layer_rule("b", Rule::new(input(), Action::Key { key: "b".into() }));
        engine.hold("a");
        engine.hold("b");
        let resolved = engine.resolve(&input());
        // Two held layers (sorted a, b) then the base = 3 matches, layers first.
        assert_eq!(resolved.len(), 3);
        assert_eq!(
            resolved[0].action,
            Action::Key { key: "a".into() },
            "layer 'a' first (sorted)"
        );
        assert_eq!(
            resolved[1].action,
            Action::Key { key: "b".into() },
            "layer 'b' second"
        );
        assert_eq!(resolved[2].action, Action::Noop, "base last");
    }

    #[test]
    fn drive_hold_toggles_layer_from_a_hold_trigger() {
        let mut engine =
            Engine::new(vec![]).with_layer_rule("sniper", Rule::new(input(), Action::Noop));
        let hold = Trigger::Hold {
            layer: "sniper".into(),
        };

        assert!(
            engine.drive_hold(&hold, true),
            "drive_hold handles a Hold trigger"
        );
        assert!(engine.is_held("sniper"));
        assert!(engine.drive_hold(&hold, false));
        assert!(!engine.is_held("sniper"));

        // Non-Hold triggers are ignored by drive_hold.
        assert!(!engine.drive_hold(&input(), true));
    }

    #[test]
    fn layers_activated_by_scans_all_layers_unheld() {
        // A layer rule keyed to `input()` must be discoverable BEFORE the layer is held (resolve
        // can't see it until held — this is the daemon's "pressing this input holds which layers?").
        let engine = Engine::new(vec![])
            .with_layer_rule("sniper", Rule::new(input(), Action::Noop))
            .with_layer_rule("comms", Rule::new(Trigger::MicTap, Action::Noop));
        assert_eq!(
            engine.layers_activated_by(&input()),
            vec!["sniper".to_string()]
        );
        assert_eq!(
            engine.layers_activated_by(&Trigger::MicTap),
            vec!["comms".to_string()]
        );
        // An input with no layer rule activates nothing.
        assert!(engine
            .layers_activated_by(&Trigger::Input {
                page: 0xAB,
                usage: 0xCD,
                pid: None
            })
            .is_empty());
    }

    #[test]
    fn release_all_clears_every_held_layer() {
        let mut engine = Engine::default();
        engine.hold("x");
        engine.hold("y");
        assert_eq!(engine.held_layers().count(), 2);
        engine.release_all();
        assert_eq!(
            engine.held_layers().count(),
            0,
            "release_all unsticks all layers"
        );
    }

    #[test]
    fn hold_is_idempotent() {
        let mut engine = Engine::default();
        engine.hold("L");
        engine.hold("L");
        assert_eq!(
            engine.held_layers().count(),
            1,
            "re-holding the same layer is a no-op"
        );
    }

    #[test]
    fn dispatch_runs_base_and_held_layer_rules() {
        let mut engine = Engine::new(vec![Rule::new(input(), Action::Noop)])
            .with_layer_rule("L", Rule::new(input(), Action::Noop));
        let ctx = Context::default();

        let log = engine.dispatch(&input(), &ctx);
        assert_eq!(log.len(), 1, "base only when unheld");

        engine.hold("L");
        let log = engine.dispatch(&input(), &ctx);
        assert_eq!(log.len(), 2, "base + layer while held");
    }

    #[test]
    fn dispatch_top_fires_only_the_winner() {
        let mut engine = Engine::new(vec![Rule::new(input(), Action::Noop)])
            .with_layer_rule("L", Rule::new(input(), Action::Noop));
        engine.hold("L");
        let ctx = Context::default();
        // Both match, but dispatch_top fires exactly one (the override).
        assert!(engine.dispatch_top(&input(), &ctx).is_some());
        let none_trigger = Trigger::MicTap;
        assert!(
            engine.dispatch_top(&none_trigger, &ctx).is_none(),
            "no match -> None"
        );
    }

    #[test]
    fn unheld_layer_does_not_leak_into_dispatch() {
        // A layer that is configured but never held must contribute nothing.
        let engine =
            Engine::new(vec![]).with_layer_rule("never", Rule::new(Trigger::MicTap, Action::Noop));
        let log = engine.dispatch(&Trigger::MicTap, &Context::default());
        assert!(log.is_empty(), "an unheld layer's rules must not fire");
    }

    #[test]
    fn dispatch_threads_ctx_into_the_action() {
        // The spine's `dispatch` must thread the SAME ctx into the fired action's `run_ctx` (not
        // re-capture). We prove it with a Sequence macro of no-ops: it runs end-to-end against the
        // provided ctx and reports its step count — exercising the engine -> Action::run_ctx ->
        // run_sequence path with one consistent snapshot.
        let engine = Engine::new(vec![Rule::new(
            Trigger::MicTap,
            Action::Sequence {
                steps: vec![
                    crate::action::Step {
                        action: Box::new(Action::Noop),
                        delay_ms: 0,
                        hold_ms: 0,
                    },
                    crate::action::Step {
                        action: Box::new(Action::Noop),
                        delay_ms: 0,
                        hold_ms: 0,
                    },
                ],
            },
        )]);
        let ctx = Context::synthetic(Some("game.exe".into()), None, None, None, None);
        let log = engine.dispatch(&Trigger::MicTap, &ctx);
        assert_eq!(log.len(), 1, "one rule fired");
        assert!(
            // The spine threads the ctx into run_ctx, which now hands a long macro to a worker thread
            // (so dispatch never blocks) — it reports "running" with the right step count.
            log[0].contains("running macro (2 steps)"),
            "macro dispatched through the spine: {:?}",
            log
        );
    }

    #[test]
    fn dispatch_of_a_run_action_spawns_nothing_while_disarmed() {
        // INPUT-SAFETY: dispatching a `Run` action through the spine must honor the process-spawn arm
        // gate exactly like a direct `Action::run`. Tests are DISARMED (this never arms), so the
        // spine reports `[disarmed]` and spawns no process — the dispatcher can't bypass the gate.
        assert!(
            !crate::action::process_spawn_armed(),
            "tests dispatch disarmed"
        );
        let engine = Engine::new(vec![Rule::new(
            Trigger::MicTap,
            Action::Run {
                cmd: "echo neuron-spine-should-not-run".into(),
            },
        )]);
        let log = engine.dispatch(&Trigger::MicTap, &Context::default());
        assert_eq!(log.len(), 1);
        assert!(
            log[0].contains("[disarmed]"),
            "spine must not spawn while disarmed: {:?}",
            log
        );
    }

    #[test]
    fn gesture_and_radial_dispatch_through_one_engine() {
        // The whole point of the spine: heterogeneous triggers, one dispatcher.
        let engine = Engine::new(vec![
            Rule::new(
                Trigger::Gesture {
                    name: "bolt".into(),
                },
                Action::Noop,
            ),
            Rule::new(
                Trigger::RadialSector {
                    menu: "comms".into(),
                    sector: 2,
                },
                Action::Noop,
            ),
            Rule::new(Trigger::Hotkey { vk: 0x70, mods: 0 }, Action::Noop),
        ]);
        let ctx = Context::default();
        assert_eq!(
            engine
                .dispatch(
                    &Trigger::Gesture {
                        name: "bolt".into()
                    },
                    &ctx
                )
                .len(),
            1
        );
        assert_eq!(
            engine
                .dispatch(
                    &Trigger::RadialSector {
                        menu: "comms".into(),
                        sector: 2
                    },
                    &ctx
                )
                .len(),
            1
        );
        assert_eq!(
            engine
                .dispatch(&Trigger::Hotkey { vk: 0x70, mods: 0 }, &ctx)
                .len(),
            1
        );
        // A radial sector that isn't bound resolves to nothing.
        assert_eq!(
            engine
                .dispatch(
                    &Trigger::RadialSector {
                        menu: "comms".into(),
                        sector: 7
                    },
                    &ctx
                )
                .len(),
            0
        );
    }
}
