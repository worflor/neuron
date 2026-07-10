//! Binding config — flat remap rows lifted into the typed `Trigger -> Action` engine.
//! Config lives in a hand-editable `bindings.toml`.
//!
//! ## Legacy vs the spine
//! [`Binding`]/[`Bindings`] are the user-facing flat syntax. The runtime model is
//! [`crate::engine::Rule`] (typed [`crate::engine::Trigger`] -> [`crate::action::Action`]);
//! [`crate::controls::binding_rule`] lifts each known binding action onto that typed engine.
//!
//! ## Defaults are intentionally EMPTY (not the knob/mute remaps)
//! The shipped default is no bindings, because the BlackShark V2 knob + mute toggle are
//! hardware-internal and emit NOTHING to the host (confirmed via Raw Input / Core-Audio monitor / no
//! Windows OSD) — so the once-imagined "knob -> Seiren gain" / "mute -> Seiren mute" defaults are
//! dead at the hardware level and there is no honest universal default to ship. The daemon still
//! detects + logs the mic tap (the one interceptable audio-device control), and `bind init` writes a
//! commented template with working examples.

use crate::controls::{usage_name, ControlEvent};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One remap rule. Flat by design so the TOML reads like what it does.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Binding {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub desc: String,
    /// HID usage page of the trigger (0x0C Consumer, 0x0B Telephony).
    pub page: u16,
    /// HID usage of the trigger (0xE9 Vol+, 0xEA Vol-, 0x2F Phone-Mute, ...).
    pub usage: u16,
    /// Optional: restrict to a source device PID, e.g. "0529" (the headset).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
    /// "mic-gain" | "mic-gain-set" | "mic-mute" | "run". Lifted to typed
    /// [`crate::action::Action`] by [`crate::controls::binding_rule`].
    pub action: String,
    /// Target capture device name substring (default: Razer/Seiren).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// mic-gain: +/- percentage points per event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_pct: Option<f32>,
    /// mic-gain-set: absolute percentage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct: Option<f32>,
    /// mic-mute: "toggle" | "on" | "off".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// run: shell command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
}

impl Binding {
    pub fn summary(&self) -> String {
        let src = format!(
            "{} (0x{:02X}/0x{:02X}){}",
            usage_name(self.page, self.usage),
            self.page,
            self.usage,
            self.pid
                .as_ref()
                .map(|p| format!(" @PID {p}"))
                .unwrap_or_default()
        );
        let dst = match self.action.as_str() {
            "mic-gain" => format!(
                "mic gain {:+}%{}",
                self.delta_pct.unwrap_or(0.0),
                dev(&self.device)
            ),
            "mic-gain-set" => format!(
                "mic gain = {}%{}",
                self.pct.unwrap_or(0.0),
                dev(&self.device)
            ),
            "mic-mute" => format!(
                "mic mute [{}]{}",
                self.mode.as_deref().unwrap_or("toggle"),
                dev(&self.device)
            ),
            "run" => format!("run `{}`", self.cmd.as_deref().unwrap_or("")),
            other => format!("?{other}"),
        };
        format!("{src:<34} -> {dst}")
    }

    /// Does this binding's trigger match a control event? (pure, side-effect free)
    pub fn matches(&self, ev: &ControlEvent) -> bool {
        let pid_ok = self
            .pid
            .as_ref()
            .is_none_or(|p| p.eq_ignore_ascii_case(&ev.pid));
        ev.is_press() && pid_ok && ev.has(self.page, self.usage)
    }
}

fn dev(d: &Option<String>) -> String {
    d.as_ref().map(|s| format!(" [{s}]")).unwrap_or_default()
}

/// Hand-written template for `bind init` — comments survive (serializing would drop them).
pub const TEMPLATE_TOML: &str = r#"# Neuron bindings — control event -> action.
#
# Each [[bindings]] maps a trigger (HID usage_page + usage, optionally a source PID) to an
# action. List live triggers with: neuron watch   (turn/press/tap your controls).
#
# IMPORTANT — what canNOT be bound on the BlackShark V2:
#   * the volume KNOB     -> hardware-internal headset volume, emits nothing to the host.
#   * the MUTE TOGGLE     -> hardwired to the detachable boom-mic, emits nothing to the host.
# Neither (nor any host software) can intercept these. Use the mic tap / keyboard / mouse.
#
# Triggers that DO work:
#   * Mic tap   -> page 0xF000 (61440), usage 0x01 (1)   [Seiren, detected via Core Audio]
#   * Keyboard media/macro keys, mouse buttons -> real HID (see `neuron watch`)
#
# Actions: mic-gain (delta_pct) | mic-gain-set (pct) | mic-mute (mode on|off|toggle) | run (cmd)
# Optional: device = "seiren" (capture-name substring), pid = "0221" (restrict source device).

# Example: tapping the Seiren keeps it LIVE instead of muting (auto-undo).
# [[bindings]]
# desc = "Mic tap -> stay un-muted"
# page = 61440   # 0xF000
# usage = 1
# action = "mic-mute"
# device = "seiren"
# mode = "off"

# Example: tapping the Seiren also runs a command.
# [[bindings]]
# desc = "Mic tap -> notify"
# page = 61440
# usage = 1
# action = "run"
# cmd = "echo tapped"
"#;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Bindings {
    #[serde(default)]
    pub bindings: Vec<Binding>,
}

impl Bindings {
    pub fn path() -> PathBuf {
        crate::runroot::run_root().join("bindings.toml")
    }

    /// Load from disk, or fall back to the sensible defaults for this user.
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                eprintln!("bindings.toml parse error ({e}); using defaults");
                Self::default_for_user()
            }),
            Err(_) => Self::default_for_user(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let s = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(Self::path(), s).map_err(|e| e.to_string())
    }

    /// Default bindings. Empty on purpose: the BlackShark knob/mute are hardware-internal
    /// (no host-visible event — see `bind init` template), so there's no honest universal
    /// default to ship. The daemon still detects + logs mic taps so the live capability is
    /// visible; `bind init` writes a commented template with working examples.
    pub fn default_for_user() -> Self {
        Bindings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(pid: &str, page: u16, usage: u16) -> ControlEvent {
        ControlEvent {
            pid: pid.into(),
            hits: vec![(page, usage)],
            raw: vec![],
        }
    }
    fn release(pid: &str) -> ControlEvent {
        ControlEvent {
            pid: pid.into(),
            hits: vec![],
            raw: vec![],
        }
    }
    /// Minimal binding builder for tests.
    fn bind(page: u16, usage: u16, action: &str) -> Binding {
        Binding {
            desc: String::new(),
            page,
            usage,
            pid: None,
            action: action.into(),
            device: None,
            delta_pct: None,
            pct: None,
            mode: None,
            cmd: None,
        }
    }

    #[test]
    fn mic_tap_trigger_is_bindable() {
        // The one interceptable audio-device control: the synthetic mic-tap trigger.
        let (page, usage) = crate::controls::MIC_TAP;
        let b = bind(page, usage, "mic-mute");
        assert!(
            b.matches(&ev("056a", page, usage)),
            "mic tap should match its binding"
        );
        assert!(!b.matches(&release("056a")), "no-hit report must not fire");
    }

    #[test]
    fn matches_only_on_press_and_exact_usage() {
        let b = bind(0x0C, 0xE9, "mic-gain");
        assert!(
            b.matches(&ev("0529", 0x0C, 0xE9)),
            "should match its trigger"
        );
        assert!(
            !b.matches(&ev("0529", 0x0C, 0xEA)),
            "wrong usage must not match"
        );
        assert!(
            !b.matches(&ev("0529", 0x0B, 0xE9)),
            "wrong page must not match"
        );
        assert!(!b.matches(&release("0529")), "release report must not fire");
    }

    #[test]
    fn pid_filter_restricts_source_device() {
        let mut b = bind(0x0C, 0xE9, "mic-gain");
        b.pid = Some("0221".into());
        assert!(b.matches(&ev("0221", 0x0C, 0xE9)), "matching pid passes");
        assert!(
            !b.matches(&ev("0529", 0x0C, 0xE9)),
            "non-matching pid is filtered out"
        );
        b.pid = None;
        assert!(
            b.matches(&ev("anything", 0x0C, 0xE9)),
            "no pid filter = any source"
        );
    }

    #[test]
    fn toml_round_trips() {
        let mut t = bind(0x0C, 0xEA, "mic-gain");
        t.delta_pct = Some(-4.0);
        t.device = Some("seiren".into());
        let original = Bindings { bindings: vec![t] };
        let s = toml::to_string_pretty(&original).unwrap();
        let parsed: Bindings = toml::from_str(&s).unwrap();
        assert_eq!(parsed.bindings.len(), 1);
        let (a, b) = (&parsed.bindings[0], &original.bindings[0]);
        assert_eq!(a.page, b.page);
        assert_eq!(a.usage, b.usage);
        assert_eq!(a.action, b.action);
        assert_eq!(a.delta_pct, b.delta_pct);
        assert_eq!(a.device, b.device);
    }

    #[test]
    fn macro_key_binding_round_trips() {
        // Razer macro keys bind on the synthetic page 0xFF1A; the GUI binds them DEVICE-ANY (pid
        // None), a hand-written one may pin a device. Both must survive a TOML round-trip so the
        // binding persists across a restart (page + usage carry "which macro key").
        let any = bind(0xFF1A, 0x20, "mic-mute"); // M1, device-any (pid None by default)
        let mut pinned = bind(0xFF1A, 0x24, "mic-mute"); // M5, device-pinned
        pinned.pid = Some("f221".into());
        let original = Bindings {
            bindings: vec![any, pinned],
        };
        let s = toml::to_string_pretty(&original).unwrap();
        let parsed: Bindings = toml::from_str(&s).unwrap();
        assert_eq!(parsed.bindings.len(), 2);
        assert_eq!(parsed.bindings[0].page, 0xFF1A);
        assert_eq!(parsed.bindings[0].usage, 0x20);
        assert_eq!(parsed.bindings[0].pid, None);
        assert_eq!(parsed.bindings[1].usage, 0x24);
        assert_eq!(parsed.bindings[1].pid, Some("f221".into()));
    }

    #[test]
    fn summary_is_human_readable() {
        let mut b = bind(0x0B, 0x2F, "mic-mute");
        b.mode = Some("toggle".into());
        let s = b.summary();
        assert!(s.contains("0x0B/0x2F"));
        assert!(s.contains("mic mute"));
    }

    #[test]
    fn template_parses() {
        // `bind init` ships TEMPLATE_TOML; it must be valid (commented examples => 0 bindings).
        let parsed: Bindings = toml::from_str(TEMPLATE_TOML).unwrap();
        assert_eq!(parsed.bindings.len(), 0);
    }
}
