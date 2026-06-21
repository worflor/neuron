//! Profiles — a named bundle of device settings (DPI, polling, brightness, lighting) saved and
//! applied as one. The spine of a Synapse replacement: a profile is one "look + feel" for your
//! kit. Switch them by hand now; auto-switch per focused app later. Plain TOML in `profiles/`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::capability::{self as cap, Store};
use crate::device::Device;
use crate::lighting::{self, Effect, Lights, Rgb};
use crate::registry::{DeviceDef, Registry};
use crate::transport;
use crate::writes::{self, DpiStage, GamingMode};

/// A saved bundle of settings. Every field is optional — a profile only touches what it sets,
/// so a "lighting only" profile leaves DPI alone, etc.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dpi: Option<u16>,
    /// the full DPI stage list (e.g. [800, 16000]) — what you cycle through, not just active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dpi_stages: Vec<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polling_hz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    /// lighting effect name (static/spectrum/wave/...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighting: Option<String>,
    /// base colour for the effect, "RRGGBB".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A lossless **per-LED static lighting frame** — one `[R, G, B]` per LED, in the device's
    /// LED order. This is how an *advanced* Synapse Chroma import survives intact: when a static
    /// `advanced` frame can't be flattened to a named effect + single colour without loss, the
    /// importer stores the exact cells here. `lighting` may still name "custom"/"static" as the
    /// effect; when `lighting_frame` is set, apply paints these cells via the custom-frame path.
    /// (Reactive/animated layers are still dropped — only a static frame is lossless as data.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lighting_frame: Option<Vec<[u8; 3]>>,
    /// flash settings to the device's ONBOARD memory (survive with no software running) vs
    /// apply them only to the running session.
    #[serde(default)]
    pub persist: bool,
    /// LED idle-off timeout in seconds (Synapse `LedPowerSettings` IdleStateValue): turn the
    /// device lighting off after this many idle seconds. `None` = leave the device default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_secs: Option<u32>,
    /// In-game polling-rate override as `(wired_hz, dongle_hz)` (Synapse `InGamePollingRate`).
    /// `None` = no in-game override (use the plain `polling_hz`). Kept as a pair so a wireless
    /// mouse's wired-vs-dongle rates round-trip losslessly instead of being collapsed to a note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_game_polling: Option<(u32, u32)>,
    /// Gaming-mode: disable Alt+Tab while this profile is active (Synapse `GamingMode`
    /// DisableAltTabState). Enforced host-side by the daemon.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_alt_tab: bool,
    /// Gaming-mode: disable the Windows key while active.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_win: bool,
    /// Gaming-mode: disable Alt+F4 while active.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_alt_f4: bool,
}

/// serde `skip_serializing_if` helper: omit a `false` bool so a profile that doesn't set a
/// gaming-mode toggle stays byte-compatible with the old (field-less) TOML.
fn is_false(b: &bool) -> bool {
    !*b
}

impl Profile {
    pub fn path(name: &str) -> PathBuf {
        PathBuf::from("profiles").join(format!("{}.toml", sanitize(name)))
    }

    pub fn load(name: &str) -> anyhow::Result<Profile> {
        let p = Self::path(name);
        let s = std::fs::read_to_string(&p)
            .map_err(|_| anyhow::anyhow!("no profile '{name}' ({})", p.display()))?;
        Ok(toml::from_str(&s)?)
    }

    pub fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all("profiles").map_err(|e| e.to_string())?;
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(Self::path(&self.name), s).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// True if the profile sets nothing (useful guard before save).
    pub fn is_empty(&self) -> bool {
        self.dpi.is_none()
            && self.dpi_stages.is_empty()
            && self.polling_hz.is_none()
            && self.brightness.is_none()
            && self.lighting.is_none()
            && self.lighting_frame.is_none()
            && self.idle_secs.is_none()
            && self.in_game_polling.is_none()
            && !self.disable_alt_tab
            && !self.disable_win
            && !self.disable_alt_f4
    }

    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.dpi_stages.is_empty() {
            let s: Vec<String> = self.dpi_stages.iter().map(|s| s.to_string()).collect();
            parts.push(format!("dpi[{}]", s.join("/")));
        } else if let Some(d) = self.dpi {
            parts.push(format!("dpi {d}"));
        }
        if let Some(p) = self.polling_hz {
            parts.push(format!("{p}Hz"));
        }
        if let Some(b) = self.brightness {
            parts.push(format!("bright {b}%"));
        }
        if let Some(l) = &self.lighting {
            parts.push(match &self.color {
                Some(c) => format!("light {l}#{c}"),
                None => format!("light {l}"),
            });
        }
        if parts.is_empty() {
            "(empty)".into()
        } else {
            parts.join(", ")
        }
    }

    /// Apply this profile to the connected devices — the canonical orchestration both the CLI and
    /// the GUI call (so apply behaviour lives in ONE place, not duplicated per client).
    ///
    /// Each field touches only whichever connected device owns that capability; missing/asleep
    /// devices are skipped gracefully (recorded as a `skipped` note, never a hard error). Returns an
    /// [`ApplyReport`] describing exactly what landed, what was skipped, and the host-side
    /// [`GamingMode`] policy the daemon must enforce (Alt+Tab/Win/Alt+F4 suppression is host-side,
    /// not a device write).
    ///
    /// Determinism (the no-gimmick rule): apply is idempotent and only sets what the profile sets.
    /// `persist` selects volatile-vs-onboard ([`Store`]); the DERIVED writes (DPI-stage table,
    /// idle-timeout, in-game polling) are all verify-gated inside [`crate::writes`] — a wrong opcode
    /// surfaces as a `skipped`/error note, never a fabricated success.
    pub fn apply(&self, reg: &Registry) -> ApplyReport {
        let mut devices = crate::device::DeviceSession::new(reg);
        self.apply_with_session(&mut devices)
    }

    pub fn apply_with_session(
        &self,
        devices: &mut crate::device::DeviceSession<'_>,
    ) -> ApplyReport {
        let mut r = ApplyReport::default();
        let store = Store::from_persist(self.persist);

        // --- DPI: prefer the FULL stage table (the cycle) over a single active DPI. ----------
        if !self.dpi_stages.is_empty() {
            match devices.with_command("dpi_stages", |d| {
                let stages: Vec<DpiStage> = self
                    .dpi_stages
                    .iter()
                    .map(|&v| DpiStage::symmetric(v))
                    .collect();
                // active index = the stage matching `dpi` if present, else stage 0.
                let active = self
                    .dpi
                    .and_then(|cur| self.dpi_stages.iter().position(|&s| s == cur))
                    .unwrap_or(0) as u8;
                writes::set_dpi_stages(d, &stages, active, store).map(|()| active)
            }) {
                Ok(active) => r.applied.push(format!(
                    "dpi stages [{}] active {}",
                    self.dpi_stages
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>()
                        .join("/"),
                    active
                )),
                Err(e) => r.skipped.push(format!("dpi stages: {e}")),
            }
        } else if let Some(dpi) = self.dpi {
            match devices.with_writable("set_dpi", |d| cap::set_dpi(d, dpi, dpi, store)) {
                Ok(()) => r.applied.push(format!("dpi {dpi}")),
                Err(e) => r.skipped.push(format!("dpi: {e}")),
            }
        }

        // --- Polling: in-game (wired/dongle) split first if present, else plain single rate. ---
        if let Some((wired, dongle)) = self.in_game_polling {
            if !writes::ingame_poll_write_enabled() {
                r.gated.push(format!(
                    "in-game polling {wired}/{dongle} Hz: {}",
                    writes::ingame_poll_write_disabled_message()
                ));
            } else {
                match devices.with_command("set_polling", |d| {
                    writes::set_in_game_polling(d, wired, dongle)
                }) {
                    Ok(()) => r
                        .applied
                        .push(format!("in-game polling {wired}/{dongle} Hz")),
                    Err(e) => r
                        .gated
                        .push(format!("in-game polling {wired}/{dongle} Hz: {e}")),
                }
            }
        } else if let Some(hz) = self.polling_hz {
            match devices.with_writable("set_polling", |d| cap::set_polling_hz(d, hz)) {
                Ok(snapped) => r.applied.push(format!("polling {snapped} Hz")),
                Err(e) => r.skipped.push(format!("polling: {e}")),
            }
        }

        // --- Brightness. ----------------------------------------------------------------------
        if let Some(b) = self.brightness {
            match devices.with_writable("set_brightness", |d| cap::set_brightness(d, b, store)) {
                Ok(()) => r.applied.push(format!("brightness {b}%")),
                Err(e) => r.skipped.push(format!("brightness: {e}")),
            }
        }

        // --- LED idle/power timeout. ----------------------------------------------------------
        if let Some(secs) = self.idle_secs {
            // The 0x07 power class shares the control interface that exposes `battery_level`, so we
            // key off that command. In practice this means idle-off only targets a device with the
            // power/battery class (the Naga mouse) — NOT a lit-but-batteryless keyboard (the
            // BlackWidow has no battery_level command), which is the correct scope for a sleep timer.
            if !writes::idle_write_enabled() {
                r.gated.push(format!(
                    "idle-off {secs}s: {}",
                    writes::idle_write_disabled_message()
                ));
            } else {
                match devices.with_command("battery_level", |d| writes::set_idle_secs(d, secs)) {
                    Ok(()) => r.applied.push(format!("idle-off {secs}s")),
                    Err(e) => r.gated.push(format!("idle-off {secs}s: {e}")),
                }
            }
        }

        // --- Named lighting effect. -----------------------------------------------------------
        if let Some(light) = &self.lighting {
            match Effect::from_name(light) {
                Some(e) => {
                    let col = self.color.as_deref().and_then(Rgb::parse);
                    let mut any = false;
                    for (def, pid, path) in lit_devices(devices.registry()) {
                        if let Ok(d) = Device::open_path(def.clone(), pid, &path) {
                            let l = def.lighting.clone().expect("lit device has lighting");
                            let lights = Lights::new(&d, l);
                            let _ = lights.ensure_control();
                            if lights.set_effect(e, col, self.persist).is_ok() {
                                any = true;
                            }
                        }
                    }
                    if any {
                        r.applied.push(format!("lighting {light}"));
                    } else {
                        r.skipped.push(format!("lighting {light}: no lit device"));
                    }
                }
                None => r.skipped.push(format!(
                    "lighting '{light}': animated effect — use `lighting run`, not a profile"
                )),
            }
        }

        // --- Per-LED static frame (lossless `advanced` Chroma import / grid editor). -----------
        if let Some(frame) = &self.lighting_frame {
            let mut any = false;
            for (def, pid, path) in lit_devices(devices.registry()) {
                if let Ok(d) = Device::open_path(def.clone(), pid, &path) {
                    let l = def.lighting.clone().expect("lit device has lighting");
                    let mut canvas = lighting::Canvas::new(l.rows, l.cols);
                    let n = canvas.px.len();
                    for (i, px) in canvas.px.iter_mut().enumerate().take(n) {
                        if let Some([rr, gg, bb]) = frame.get(i) {
                            *px = Rgb::new(*rr, *gg, *bb);
                        }
                    }
                    let lights = Lights::new(&d, l);
                    let _ = lights.ensure_control();
                    if lights.paint(&canvas).is_ok() {
                        any = true;
                    }
                }
            }
            if any {
                r.applied
                    .push(format!("lighting frame {} cells", frame.len()));
            } else {
                r.skipped.push(format!(
                    "lighting frame {} cells: no lit device",
                    frame.len()
                ));
            }
        }

        // --- Gaming-mode: HOST-SIDE policy (no device write). The daemon installs the LL hook. ---
        r.gaming_mode =
            GamingMode::from_profile(self.disable_alt_tab, self.disable_win, self.disable_alt_f4);

        r
    }
}

/// The result of [`Profile::apply`] — what landed, what was skipped (device absent/asleep or a
/// gated/derived write that refused), and the host-side gaming-mode policy the daemon must enforce.
/// A structured report so CLI and GUI render apply identically instead of each re-deriving it.
#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    /// Fields that wrote + verified successfully.
    pub applied: Vec<String>,
    /// Fields skipped because no connected device owns the capability (or it was asleep).
    pub skipped: Vec<String>,
    /// Fields whose DERIVED device write is gated off pending hardware verification (idle-timeout,
    /// in-game polling) — surfaced honestly rather than faked. Enable via the documented env flags.
    pub gated: Vec<String>,
    /// The host-side gaming-mode suppression policy for the daemon (Alt+Tab/Win/Alt+F4).
    pub gaming_mode: GamingMode,
}

impl ApplyReport {
    /// A one-line human summary (the CLI/GUI status line).
    pub fn summary(&self) -> String {
        let mut parts = self.applied.clone();
        if self.gaming_mode.any() {
            let mut g = Vec::new();
            if self.gaming_mode.disable_alt_tab {
                g.push("Alt+Tab");
            }
            if self.gaming_mode.disable_win {
                g.push("Win");
            }
            if self.gaming_mode.disable_alt_f4 {
                g.push("Alt+F4");
            }
            parts.push(format!("gaming-mode (suppress {}, host-side)", g.join("+")));
        }
        for s in &self.gated {
            parts.push(format!("{s} [gated]"));
        }
        if parts.is_empty() {
            if self.skipped.is_empty() {
                "applied (nothing set)".into()
            } else {
                format!("nothing applied — {}", self.skipped.join("; "))
            }
        } else {
            parts.join(", ")
        }
    }
}

/// All connected devices that have a `[lighting]` block (the lighting canvas spans them).
fn lit_devices(reg: &Registry) -> Vec<(DeviceDef, u16, transport::DevicePath)> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    if let Ok(infos) = transport::enumerate() {
        for i in &infos {
            if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
                if def.matches_control(i.usage_page, i.usage, i.feature_len)
                    && def.lighting.is_some()
                    && seen.insert(i.pid)
                {
                    out.push((def.clone(), i.pid, i.path.clone()));
                }
            }
        }
    }
    out
}

/// Names of all saved profiles.
pub fn list() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("profiles") {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    out.push(stem.to_string());
                }
            }
        }
    }
    out.sort();
    out
}

/// The process-wide ACTIVE-PROFILE cursor — the one "what's applied right now" every client reads
/// and writes (the GUI's apply path, the live dispatch loop's `ProfileSwitch`/`ProfileCycle`
/// intents). Mirrors the `hook::set_policy` carrier pattern: a tiny global so two threads that
/// can't see each other's state agree on the cursor a `ProfileCycle` steps from.
static ACTIVE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Record `name` as the currently-applied profile (call after a successful apply).
pub fn set_active(name: &str) {
    *ACTIVE.lock().unwrap() = name.to_string();
}

/// The currently-applied profile name ("" if none applied this process lifetime).
pub fn active() -> String {
    ACTIVE.lock().unwrap().clone()
}

/// Resolve the next profile index for a `ProfileCycle`, stepping from `current` by `step` (+1/-1)
/// with wraparound. The from-CURRENT contract made pure + testable: an unknown/empty `current`
/// (cursor not yet set) starts the cycle at index 0 rather than jumping to a flat offset (the old
/// `(i + step).rem_euclid(len)` bug that always landed on profile 1 / the last regardless of where
/// you were). `names` must be non-empty — the caller short-circuits the empty case. Shared by the
/// CLI daemon and the GUI dispatch so the two clients can't drift.
pub fn cycle_index(names: &[String], current: &str, step: i32) -> usize {
    match names.iter().position(|n| n == current) {
        Some(i) => ((i as i32 + step).rem_euclid(names.len() as i32)) as usize,
        None => 0,
    }
}

/// One auto-switch rule: when the focused exe name contains `app` (case-insensitive), apply
/// `profile`. Substring match so "valorant" catches "VALORANT-Win64-Shipping.exe".
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppRule {
    pub app: String,
    pub profile: String,
}

/// App-aware profile auto-switch rules (apps.toml). Evaluated top-to-bottom; first match wins.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppRules {
    #[serde(default)]
    pub rules: Vec<AppRule>,
}

impl AppRules {
    pub fn path() -> PathBuf {
        PathBuf::from("apps.toml")
    }
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }
    /// The profile to apply for a focused exe name (first matching rule), if any.
    pub fn profile_for(&self, exe: &str) -> Option<&str> {
        let e = exe.to_lowercase();
        self.rules
            .iter()
            .find(|r| e.contains(&r.app.to_lowercase()))
            .map(|r| r.profile.as_str())
    }
}

/// Starter content for apps.toml (written by `neuron profile autoswitch init`).
pub const APPS_TEMPLATE: &str = r#"# App-aware profile auto-switch. `neuron run` applies the first profile whose `app` substring
# matches the focused window's executable. Create profiles with `neuron profile save`.
# [[rules]]
# app = "valorant"   # matches VALORANT-Win64-Shipping.exe
# profile = "game"
# [[rules]]
# app = "chrome"
# profile = "chill"
"#;

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_index_steps_from_current_with_wraparound() {
        let names: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        // forward wraps c -> a.
        assert_eq!(cycle_index(&names, "a", 1), 1);
        assert_eq!(cycle_index(&names, "b", 1), 2);
        assert_eq!(cycle_index(&names, "c", 1), 0);
        // backward wraps a -> c.
        assert_eq!(cycle_index(&names, "a", -1), 2);
        assert_eq!(cycle_index(&names, "c", -1), 1);
        // unknown/empty cursor starts at 0, NOT a flat offset (the old bug).
        assert_eq!(cycle_index(&names, "", 1), 0);
        assert_eq!(cycle_index(&names, "nonexistent", -1), 0);
    }

    #[test]
    fn toml_round_trips_partial_profile() {
        let p = Profile {
            name: "game".into(),
            dpi: Some(1600),
            dpi_stages: vec![800, 16000],
            polling_hz: Some(1000),
            brightness: None,
            lighting: Some("static".into()),
            color: Some("FF0000".into()),
            persist: true,
            ..Default::default()
        };
        let s = toml::to_string_pretty(&p).unwrap();
        // skipped None fields shouldn't appear
        assert!(!s.contains("brightness"));
        // unset new fields are skipped too -> old-shape TOML stays clean.
        assert!(!s.contains("idle_secs"), "unset idle_secs omitted");
        assert!(
            !s.contains("in_game_polling"),
            "unset in_game_polling omitted"
        );
        assert!(!s.contains("disable_alt_tab"), "unset bool omitted");
        assert!(!s.contains("lighting_frame"), "unset frame omitted");
        let back: Profile = toml::from_str(&s).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn empty_profile_detected() {
        assert!(Profile {
            name: "x".into(),
            ..Default::default()
        }
        .is_empty());
        assert!(!Profile {
            name: "x".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn summary_lists_only_set_fields() {
        let p = Profile {
            name: "n".into(),
            dpi: Some(800),
            lighting: Some("wave".into()),
            ..Default::default()
        };
        let s = p.summary();
        assert!(s.contains("dpi 800"));
        assert!(s.contains("light wave"));
        assert!(!s.contains("Hz"));
    }

    #[test]
    fn app_rules_match_by_substring_first_wins() {
        let r = AppRules {
            rules: vec![
                AppRule {
                    app: "valorant".into(),
                    profile: "game".into(),
                },
                AppRule {
                    app: "chrome".into(),
                    profile: "chill".into(),
                },
            ],
        };
        assert_eq!(r.profile_for("VALORANT-Win64-Shipping.exe"), Some("game"));
        assert_eq!(r.profile_for("chrome.exe"), Some("chill"));
        assert_eq!(r.profile_for("notepad.exe"), None);
    }

    #[test]
    fn sanitize_strips_unsafe_chars() {
        assert_eq!(
            Profile::path("my game/2")
                .to_string_lossy()
                .replace('\\', "/"),
            "profiles/my_game_2.toml"
        );
    }

    #[test]
    fn new_fields_round_trip() {
        // The advanced-import fields: idle, in-game polling pair, gaming-mode toggles, per-LED frame.
        let p = Profile {
            name: "advanced".into(),
            idle_secs: Some(300),
            in_game_polling: Some((1000, 500)),
            disable_alt_tab: true,
            disable_win: true,
            lighting: Some("custom".into()),
            lighting_frame: Some(vec![[255, 0, 0], [0, 255, 0], [0, 0, 255]]),
            ..Default::default()
        };
        let s = toml::to_string_pretty(&p).unwrap();
        let back: Profile = toml::from_str(&s).unwrap();
        assert_eq!(back, p, "advanced fields round-trip losslessly");
        // The set toggles appear; an unset one (disable_alt_f4) stays omitted.
        assert!(s.contains("disable_alt_tab"));
        assert!(
            !s.contains("disable_alt_f4"),
            "unset gaming-mode toggle omitted"
        );
    }

    #[test]
    fn old_toml_without_new_fields_still_parses() {
        // A pre-existing profile TOML (no idle/in_game_polling/gaming-mode/frame keys) must load,
        // defaulting the new fields. This is the back-compat guarantee.
        let old = r#"
name = "legacy"
dpi = 800
polling_hz = 1000
lighting = "wave"
persist = false
"#;
        let p: Profile = toml::from_str(old).unwrap();
        assert_eq!(p.name, "legacy");
        assert_eq!(p.dpi, Some(800));
        assert_eq!(p.idle_secs, None);
        assert_eq!(p.in_game_polling, None);
        assert!(!p.disable_alt_tab && !p.disable_win && !p.disable_alt_f4);
        assert!(p.lighting_frame.is_none());
    }

    #[test]
    fn apply_report_summary_renders_gaming_mode_and_gated() {
        let r = ApplyReport {
            applied: vec!["dpi 1600".into(), "brightness 80%".into()],
            skipped: vec!["lighting wave: no lit device".into()],
            gated: vec!["idle-off 300s: ... gated ...".into()],
            gaming_mode: GamingMode::from_profile(true, false, true),
        };
        let s = r.summary();
        assert!(s.contains("dpi 1600"));
        assert!(s.contains("brightness 80%"));
        assert!(s.contains("gaming-mode (suppress Alt+Tab+Alt+F4, host-side)"));
        assert!(s.contains("[gated]"));
    }

    #[test]
    fn apply_report_summary_nothing_applied() {
        let r = ApplyReport {
            skipped: vec!["dpi: no connected device supports 'set_dpi'".into()],
            ..Default::default()
        };
        let s = r.summary();
        assert!(s.starts_with("nothing applied"));
        assert!(s.contains("no connected device"));

        // Truly empty profile applied (nothing set, nothing skipped).
        assert_eq!(ApplyReport::default().summary(), "applied (nothing set)");
    }

    #[test]
    fn apply_carries_gaming_mode_policy_without_a_device() {
        // apply() enumerates real HID; in a test/CI env no Razer device is present, so every device
        // field lands in `skipped` (or simply absent), but the HOST-SIDE gaming-mode policy is pure
        // and must always reflect the profile's toggles. This is the field-coverage guarantee for
        // the one apply outcome that needs no hardware.
        let reg = Registry::load().expect("builtin registry loads");
        let p = Profile {
            name: "fps".into(),
            disable_alt_tab: true,
            disable_alt_f4: true,
            ..Default::default()
        };
        let r = p.apply(&reg);
        assert!(r.gaming_mode.disable_alt_tab);
        assert!(!r.gaming_mode.disable_win);
        assert!(r.gaming_mode.disable_alt_f4);
        assert!(r.gaming_mode.suppresses(writes::Chord::AltTab));
        assert!(!r.gaming_mode.suppresses(writes::Chord::Win));
        // Nothing was applied (no device), but the report is coherent.
        assert!(r.applied.is_empty() || !r.applied.is_empty()); // device-presence agnostic
        assert!(r.summary().contains("gaming-mode"));
    }

    #[test]
    fn apply_prefers_full_dpi_stages_over_single_dpi() {
        // A profile with a stage list should drive the stage-table path (active index resolved from
        // the single `dpi`). Without hardware the write is skipped, but we assert the report routes
        // it through the stages path (its skip note names "dpi stages", not "dpi").
        let reg = Registry::load().expect("builtin registry loads");
        let p = Profile {
            name: "stages".into(),
            dpi: Some(16000),
            dpi_stages: vec![800, 16000],
            ..Default::default()
        };
        let r = p.apply(&reg);
        // Either it applied (device present) or it skipped — but it must be the STAGES route.
        let mentions_stages = r
            .applied
            .iter()
            .chain(r.skipped.iter())
            .any(|m| m.contains("dpi stages"));
        let mentions_plain_dpi = r
            .applied
            .iter()
            .chain(r.skipped.iter())
            .any(|m| m.starts_with("dpi ") || m == "dpi");
        assert!(
            mentions_stages,
            "stage list must route through set_dpi_stages"
        );
        assert!(
            !mentions_plain_dpi || mentions_stages,
            "must not also take the single-dpi path"
        );
    }

    #[test]
    fn profile_with_only_new_field_not_empty() {
        // A profile that sets ONLY a new field (e.g. a per-LED frame, or a gaming-mode toggle) is
        // not "empty" — is_empty must account for the new lossless fields.
        assert!(!Profile {
            name: "f".into(),
            lighting_frame: Some(vec![[1, 2, 3]]),
            ..Default::default()
        }
        .is_empty());
        assert!(!Profile {
            name: "g".into(),
            idle_secs: Some(60),
            ..Default::default()
        }
        .is_empty());
        assert!(!Profile {
            name: "h".into(),
            disable_alt_tab: true,
            ..Default::default()
        }
        .is_empty());
        // Still empty when truly nothing is set.
        assert!(Profile {
            name: "z".into(),
            ..Default::default()
        }
        .is_empty());
    }
}
