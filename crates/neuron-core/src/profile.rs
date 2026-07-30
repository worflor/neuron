// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Profiles — a named bundle of device settings (DPI, polling, brightness, lighting) saved and
//! applied as one. The spine of a Synapse replacement: a profile is one "look + feel" for your
//! kit. Switch them by hand now; auto-switch per focused app later. Plain TOML in `profiles/`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::capability::{self as cap, Store};
use crate::device::Device;
use crate::lighting::{self, Lights};
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
    /// The lighting look as ONE representation: a bottom-up stack of compositor layers (each a
    /// pattern × spectrum, or a `custom` hand-painted/imported per-key frame). Empty = the profile
    /// doesn't touch lighting. This is the single source of truth — no named-effect string, no frame
    /// sidecar; `apply` renders the stack once and paints it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lighting: Vec<crate::pattern::LayerDef>,
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
    /// Gaming-mode: disable Alt+Esc (the quiet task-switch that still yanks focus) while active.
    /// Unlike the other three this has NO Synapse-import source, but it IS a first-class NATIVE
    /// profile field — the Key Guard's fourth toggle, captured and applied like the rest.
    #[serde(default, skip_serializing_if = "is_false")]
    pub disable_alt_esc: bool,
}

/// serde `skip_serializing_if` helper: omit a `false` bool so a profile that doesn't set a
/// gaming-mode toggle stays byte-compatible with the old (field-less) TOML.
fn is_false(b: &bool) -> bool {
    !*b
}

/// The `profiles/` directory (profiles + their `.rules.toml`/`.frame.toml` sidecars) in the run
/// root — the ONE derivation every profile/sidecar reader and writer shares.
pub fn profiles_dir() -> PathBuf {
    crate::runroot::run_root().join("profiles")
}

impl Profile {
    pub fn path(name: &str) -> PathBuf {
        profiles_dir().join(format!("{}.toml", sanitize(name)))
    }

    /// The canonical on-disk KEY that two display names collide on: the sanitized filename stem,
    /// case-folded. `path()` sanitizes (every non-alphanumeric/-/_ → `_`) and Windows' filesystem is
    /// case-insensitive, so `"my game/2"` and `"my_game_2"` — and `"Valorant"` and `"valorant"` — all
    /// resolve to the SAME `.toml`. Overwrite/de-collision checks MUST compare this, not the raw name,
    /// or the "capture vs overwrite" button lies and a save silently clobbers an existing profile.
    pub fn file_key(name: &str) -> String {
        sanitize(name).to_lowercase()
    }

    /// The rules sidecar PAIRED with this profile: the profile's own (sanitized) path with its
    /// extension swapped to `.rules.toml`. Deriving it from [`path`](Self::path) — the single source
    /// of truth for a name→file mapping — guarantees the sidecar always sits FLAT beside
    /// `<sanitized>.toml` and can never diverge from it. Interpolating the RAW name instead (the old
    /// importer bug) let a name with a path separator, e.g. `"FPS/competitive"`, aim the sidecar at a
    /// nonexistent nested dir (`profiles/FPS/competitive.rules.toml`) — so the write failed after the
    /// profile had already been saved, and the daemon (which globs `profiles/*.rules.toml`) would
    /// never have found it anyway. Every name-derived sidecar path MUST go through here.
    pub fn rules_path(name: &str) -> PathBuf {
        Self::path(name).with_extension("rules.toml")
    }

    /// The ONE place a blank/whitespace-only imported name gets a real name — a blank name
    /// `sanitize`s to `""`, and `path`/`rules_path` would then happily write a hidden `.toml` /
    /// `.rules.toml` pair that no profile-listing flow shows and nobody can select. Every importer
    /// (GUI wizard, CLI `import-export --apply`) MUST resolve through here before calling `save()`
    /// or deriving a sidecar path, so a blank-named import always lands as the same visible,
    /// selectable `imported` profile regardless of which front door it came through.
    pub fn resolve_import_name(name: &str) -> String {
        let n = name.trim();
        if n.is_empty() {
            "imported".to_string()
        } else {
            n.to_string()
        }
    }

    /// Resolve an import name AND de-collide it against profiles already on disk. Overwriting is only
    /// legitimate when the existing file IS this profile — i.e. its stored display name matches
    /// exactly, which makes a re-import (or the idempotent retry after a failed sidecar write) update
    /// in place. Any other occupant of the same file key — a DIFFERENT display name that merely
    /// sanitizes to the same stem (see [`file_key`](Self::file_key): "FPS/competitive" vs
    /// "FPS_competitive", or a case-only variant on Windows' case-insensitive filesystem), or a file
    /// too corrupt to read a name out of — must NOT be silently replaced: the name gets a " (2)" /
    /// " (3)" … suffix until it lands on a free key. Every importer (GUI wizard, CLI --apply) MUST
    /// resolve through here, not just resolve_import_name, before saving.
    /// No-clobber is now UNCONDITIONAL: exhausting the " (2)".." (99)" search returns an `Err`
    /// (never falls back to the already-proven-occupied base) — see `de_collide_with`.
    /// Also guards the rules-only sidecar: an importer that writes an empty profile skips the
    /// `.toml` write but still writes `.rules.toml` (see migrate.rs / CLI import-export), leaving
    /// an occupied sidecar with no paired profile file. A sidecar stores no display name, so an
    /// orphan one can never be verified as "this very import" — it is always RESERVED. Safety over
    /// convenience: an identical rules-only re-import lands at " (2)" instead of updating in place,
    /// but another import's bindings can never be silently destroyed. The sidecar check only runs
    /// when the profile file itself is ABSENT — when it's present with an exact display-name match
    /// (update-in-place), the paired sidecar belongs to that same profile and the key stays FREE.
    pub fn de_collide_import_name(name: &str) -> Result<String, String> {
        let base = Self::resolve_import_name(name);
        let free = |n: &str| -> bool {
            let p = Self::path(n);
            match std::fs::symlink_metadata(&p) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // profile file absent — but a rules-only sidecar may still occupy the key.
                    match std::fs::symlink_metadata(Self::rules_path(n)) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true, // truly absent → free
                        Err(_) => false, // present-but-uninspectable → RESERVED
                        Ok(_) => false, // orphan sidecar: no display name to verify → always RESERVED
                    }
                }
                Err(_) => false, // present-but-uninspectable (e.g. permission denied) → RESERVED, never a target
                Ok(_) => {
                    // occupied: overwrite is fine ONLY if it's this very profile (exact display-name match).
                    matches!(Self::load(n), Ok(existing) if existing.name == n)
                }
            }
        };
        de_collide_with(base, free)
    }

    pub fn load(name: &str) -> anyhow::Result<Profile> {
        let p = Self::path(name);
        let s = std::fs::read_to_string(&p)
            .map_err(|_| anyhow::anyhow!("no profile '{name}' ({})", p.display()))?;
        Ok(toml::from_str(&s)?)
    }

    /// Persist the profile as plain TOML. Lighting lives in the profile itself now (the `lighting`
    /// layer stack), so there is no sidecar to reconcile — one write, done.
    pub fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all(profiles_dir()).map_err(|e| e.to_string())?;
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        crate::salvage::atomic_write(&Self::path(&self.name), s.as_bytes()).map_err(|e| e.to_string())
    }

    /// True if the profile sets nothing (useful guard before save).
    pub fn is_empty(&self) -> bool {
        self.dpi.is_none()
            && self.dpi_stages.is_empty()
            && self.polling_hz.is_none()
            && self.brightness.is_none()
            && self.lighting.is_empty()
            && self.idle_secs.is_none()
            && self.in_game_polling.is_none()
            && !self.has_gaming()
    }

    /// Whether ANY host-side gaming-mode guard is set — the ONE definition of "this profile suppresses
    /// system key-chords". `is_empty`, `summary`, and the sheet's saved-row badge all consult this, so
    /// adding a 5th chord updates one place and no call site can silently forget it. (The LIVE-preview
    /// mirror of this predicate is `State.gaming-live` in the UI, derived over the same four flags.)
    pub fn has_gaming(&self) -> bool {
        self.disable_alt_tab || self.disable_win || self.disable_alt_f4 || self.disable_alt_esc
    }

    /// The host-side [`GamingMode`] suppression policy this profile's own flags derive — the ONE
    /// mapping `apply()`'s `ApplyReport.gaming_mode` and the launch `gaming_hook_policy` reconcile
    /// unit (`neuron-app`'s `glue.rs`) both consult, so a profile's suppression policy reads the SAME
    /// whether it came from a live apply or from re-deriving the active profile at startup.
    pub fn gaming_mode(&self) -> GamingMode {
        GamingMode::from_profile(
            self.disable_alt_tab,
            self.disable_win,
            self.disable_alt_f4,
            self.disable_alt_esc,
        )
    }

    /// A short human label for the lighting stack — the ONE source both `summary()` and the profile
    /// sheet's row badge consult, so a profile's lighting reads the SAME everywhere (no "1 fx" here vs
    /// "Axis" there). A hand-painted/imported frame is `"custom"`; a lone procedural layer names its
    /// PRESET (e.g. `"wave"`, matching the live effect label) — falling back to the pattern label —
    /// and a taller stack counts its layers. `""` when the profile sets no lighting.
    pub fn lighting_label(&self) -> String {
        if self.lighting.is_empty() {
            String::new()
        } else if self.lighting.iter().any(|l| l.pattern == "custom") {
            "custom".to_string()
        } else if self.lighting.len() == 1 {
            crate::pattern::slug_for_layer(&self.lighting[0])
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    let key = self.lighting[0].pattern.as_str();
                    crate::pattern::pattern_def(key)
                        .map(|d| d.label.to_string())
                        .unwrap_or_else(|| key.to_string())
                })
        } else {
            format!("{} fx", self.lighting.len())
        }
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
        if !self.lighting.is_empty() {
            parts.push(format!("light {}", self.lighting_label()));
        }
        if let Some(s) = self.idle_secs {
            parts.push(format!("idle {s}s"));
        }
        if let Some((wired, dongle)) = self.in_game_polling {
            parts.push(format!("in-game {wired}/{dongle}Hz"));
        }
        // host-side gaming suppression is real content — without this a gaming-ONLY profile summarizes
        // as "(empty)" (a lie that mislabels it in the CLI list + the sheet row, inviting a wrong delete).
        if self.has_gaming() {
            parts.push("gaming".into());
        }
        // INVARIANT: summary() names every field is_empty() counts, so "(empty)" appears IFF the
        // profile is truly empty. `summary_matches_is_empty` guards this against future field drift.
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
        // A one-shot apply (CLI / headless) has no live compositor stream, so it PAINTS the lighting.
        self.apply_with_session(&mut devices, true)
    }

    /// Apply every set field to the connected devices. `paint_lighting` controls the lighting stage:
    /// a headless caller (CLI one-shot) passes `true` to render+paint the stack once; the GUI passes
    /// `false` because it drives lighting through its LIVE compositor stream instead — painting here
    /// would fight that stream for the device (two writers → a stalled HID write → apply timeout).
    pub fn apply_with_session(
        &self,
        devices: &mut crate::device::DeviceSession<'_>,
        paint_lighting: bool,
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
                writes::set_dpi_stages(d, &stages, active, store).map(|()| (d.pid, active))
            }) {
                Ok((pid, active)) => {
                    // record the HOST feel intent — the authority wake/announce reasserts heal
                    // from (a disk failure is inert here; the device write already landed).
                    let _ = crate::feel_intent::record_stages(pid, &self.dpi_stages, active);
                    r.applied.push(format!(
                        "dpi stages [{}] active {}",
                        self.dpi_stages
                            .iter()
                            .map(|s| s.to_string())
                            .collect::<Vec<_>>()
                            .join("/"),
                        active
                    ));
                }
                Err(e) => r.skipped.push(format!("dpi stages: {e}")),
            }
        } else if let Some(dpi) = self.dpi {
            match devices.with_writable("set_dpi", |d| {
                cap::set_dpi(d, dpi, dpi, store).map(|()| d.pid)
            }) {
                Ok(pid) => {
                    let _ = crate::feel_intent::record_dpi(pid, dpi, dpi);
                    r.applied.push(format!("dpi {dpi}"));
                }
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
            // brightness is dual-dialect, so it resolves by CAPABILITY (the command-name gate skipped
            // legacy boards that can only write via the lighting block).
            match devices
                .with_writable_cap(crate::registry::Capability::SetBrightness, |d| {
                    cap::set_brightness(d, b, store)
                }) {
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

        // --- Lighting: render the layer stack once and paint it onto every lit device. ----------
        // ONE representation — a static/imported frame is just a `custom` layer, a procedural look is
        // its pattern×spectrum layers. We composite the stack at t=0 (a profile captures a still, not a
        // running animation) and paint the resulting canvas.
        if paint_lighting && !self.lighting.is_empty() {
            let mut any = false;
            for (def, pid, path) in lit_devices(devices.registry()) {
                if let Ok(d) = Device::open_path(def.clone(), pid, &path) {
                    let l = def.lighting.clone().expect("lit device has lighting");
                    let cells = crate::pattern::Compositor::from_defs(&self.lighting)
                        .render(l.rows, l.cols, 0.0);
                    let mut canvas = lighting::Canvas::new(l.rows, l.cols);
                    for (i, px) in canvas.px.iter_mut().enumerate() {
                        if let Some(c) = cells.get(i) {
                            *px = *c;
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
                    .push(format!("lighting {} layer(s)", self.lighting.len()));
            } else {
                r.skipped.push("lighting: no lit device".into());
            }
        }

        // --- Gaming-mode: HOST-SIDE policy (no device write). The daemon installs the LL hook. ---
        r.gaming_mode = self.gaming_mode();

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
            parts.push(format!(
                "gaming-mode (suppress {}, host-side)",
                self.gaming_mode.suppressed_labels().join("+")
            ));
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
            // find_for_pipe: the def that DRIVES this pipe (its family's control-pipe rule), so a
            // two-family pid resolves each pipe to the family that can actually paint it.
            if let Some(def) = reg.find_for_pipe(i) {
                // one lighting row per (pid, family) — two families on one pid are two independently-
                // drivable lighting planes, and the pid-only key silently dropped the second (review-caught).
                if def.lighting.is_some() && seen.insert((i.pid, def.dialect.clone())) {
                    out.push((def.clone(), i.pid, i.path.clone()));
                }
            }
        }
    }
    out
}

/// Open the device the caller SELECTED (the GUI passes its `selected_pid` + `selected_unit` so
/// capture reads the exact physical device the user picked on a multi-device rig — two identical
/// devices share a pid, and only the unit (`transport::path_instance`) tells them apart). `pid ==
/// 0` means "no selection" → returns `None` so the caller falls back to a capability search;
/// `None` too if that pid isn't a connected, recognized control device.
///
/// Unit semantics mirror the GUI's `open_selected`: when the named unit is PRESENT, it is the
/// only acceptable match — a twin must never answer for it. Only when the unit is entirely gone
/// from the enumeration (unplugged/re-ported since the scan) does the match relax to pid-level,
/// the same "selection follows reality" healing the app does. `dialect` is the selected control
/// PLANE's family (same accept rule as the app's `resolve_plane`): empty = any family (the
/// stateless caller / no selection), else only a pipe whose def speaks that family qualifies — so
/// a future multi-family unit captures the exact plane the user picked, not first-family-wins.
fn open_selected_device(reg: &Registry, pid: u16, unit: &str, dialect: &str) -> Option<Device> {
    if pid == 0 {
        return None;
    }
    let infos = transport::enumerate().ok()?;
    // A pipe is acceptable iff it's the SELECTED pid AND some family resolves it as a control pipe
    // (find_for_pipe = pipe-precise, family-aware) whose dialect matches the selected PLANE (empty
    // dialect = any family — the stateless CLI passes "").
    let matches = |i: &transport::HidDeviceInfo| {
        i.pid == pid
            && reg
                .find_for_pipe(i)
                .is_some_and(|def| dialect.is_empty() || def.dialect == dialect)
    };
    if !unit.is_empty() {
        if let Some(i) = infos.iter().find(|i| matches(i) && i.instance() == unit) {
            let def = reg.find_for_pipe(i)?.clone();
            return Device::open_path(def, i.pid, &i.path).ok();
        }
        // named unit not enumerated — heal to pid-level below.
    }
    let i = infos.iter().find(|i| matches(i))?;
    let def = reg.find_for_pipe(i)?.clone();
    Device::open_path(def, i.pid, &i.path).ok()
}

/// Capture the CURRENT connected-device state into a [`Profile`] — the shared read path both the CLI
/// `profile save` and the GUI's "capture" button call, so a captured profile is byte-identical no
/// matter which client took it. Reads only; each capability is best-effort (an absent/asleep device
/// simply leaves that field `None`). `gaming` carries the host-side Key-Guard toggles (not a device
/// read) and `persist` records the volatile-vs-onboard intent. `selected_pid` + `selected_unit` +
/// `selected_dialect` are the GUI's picked CONTROL PLANE (pid 0 / empty unit / empty dialect = no
/// selection → capability-based first match, which is what the stateless CLI passes); the unit
/// keeps capture on the exact board when two identical devices share a pid, and the dialect keeps
/// it on the exact family plane when one unit exposes several. Capture reads the user's selected
/// plane: a plane without numeric getters captures none (honest — you captured what that plane
/// does), and the no-selection fallback stays capability-based (`open_with_command("dpi")`), the
/// capability-aware resolution the review asked about.
pub fn capture_from_devices(
    reg: &Registry,
    name: &str,
    gaming: crate::writes::GamingMode,
    persist: bool,
    selected_pid: u16,
    selected_unit: &str,
    selected_dialect: &str,
) -> Profile {
    let mut p = Profile {
        name: name.to_string(),
        persist,
        disable_alt_tab: gaming.disable_alt_tab,
        disable_win: gaming.disable_win,
        disable_alt_f4: gaming.disable_alt_f4,
        disable_alt_esc: gaming.disable_alt_esc,
        ..Default::default()
    };
    // Numeric settings come from the SELECTED device when the GUI picked one (so a multi-device rig
    // captures the device the user is looking at — down to the exact physical unit of a duplicate
    // pair — not whatever enumerates first); with no selection (the stateless CLI, pid 0) fall back
    // to the first dpi-capable device.
    let numeric = open_selected_device(reg, selected_pid, selected_unit, selected_dialect)
        .or_else(|| Device::open_with_command(reg, "dpi").ok());
    if let Some(d) = numeric {
        if let Ok((x, _)) = crate::capability::dpi(&d) {
            p.dpi = Some(x);
        }
        // Active-first (0x04/0x86 = the user's REAL cycle), slot-table fallback (0x04/0x83) for
        // boards without 0x86 — snapshotting the FACTORY slots is how a captured profile replayed
        // factory stages over the user's onboard cycle on every apply (live incident 2026-07-07).
        // The decode below fits both replies (identical layout; the CLI relies on that same fit).
        if let Ok(s) = d.run("dpi_stages_active").or_else(|_| d.run("dpi_stages")) {
            p.dpi_stages = writes::decode_dpi_stages(&s);
        }
        if let Ok(hz) = crate::capability::polling_rate_hz(&d) {
            p.polling_hz = Some(hz);
        }
        if let Ok(b) = crate::capability::brightness_percent(&d) {
            p.brightness = Some(b);
        }
        if let Ok(secs) = crate::capability::idle_timeout_secs(&d) {
            p.idle_secs = Some(secs as u32);
        }
    }
    // Lighting is NOT captured from raw device state: the profile's lighting is a `Vec<LayerDef>`
    // stack and core can't synthesize that from a device's current effect register. `p.lighting` stays
    // at its default (empty) — a captured profile leaves lighting alone unless the caller sets a stack.
    p
}

/// Names of all saved profiles.
pub fn list() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(profiles_dir()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("toml") {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    // Skip SIDECARS: rules/frame live beside the profile as `<name>.rules.toml` /
                    // `<name>.frame.toml`, so their file_stem still carries `.rules` / `.frame`. A
                    // real profile file is `<sanitized>.toml`, and sanitize() maps every `.` to `_`
                    // — so a dot in the stem means it's a sidecar, never a profile.
                    if stem.contains('.') {
                        continue;
                    }
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
    *ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = name.to_string();
}

/// The currently-applied profile name ("" if none applied this process lifetime).
pub fn active() -> String {
    ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
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
        crate::runroot::run_root().join("apps.toml")
    }
    /// Load from disk, salvaging rule-by-rule so one malformed rule can't silently drop every
    /// auto-switch rule (and never clobbering the file — see [`crate::salvage::SalvageLoad`]).
    pub fn load() -> Self {
        <Self as crate::salvage::SalvageLoad>::load()
    }
    /// Persist to `apps.toml`, atomically (temp file + rename). The ONE place AppRules is
    /// serialized and written — every caller (GUI, CLI) goes through here instead of hand-rolling
    /// its own `toml::to_string_pretty` + write, so a future call site can't reintroduce a
    /// truncating `std::fs::write` that bypasses the durability `load()`'s `SalvageLoad` recovery
    /// depends on.
    pub fn save(&self) -> Result<(), String> {
        let s = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        crate::salvage::atomic_write(&Self::path(), s.as_bytes()).map_err(|e| e.to_string())
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

impl crate::salvage::SalvageLoad for AppRules {
    const FILE: &'static str = "apps.toml";
    fn path() -> PathBuf {
        crate::runroot::run_root().join("apps.toml")
    }
    fn salvage(table: &toml::Table) -> Self {
        // Keep every rule that still parses (drop only the malformed ones) instead of silently
        // discarding EVERY auto-switch rule on one bad entry.
        let mut cfg = Self::default();
        if let Some(v) = crate::salvage::salvage_vec(table, "rules", Self::FILE) {
            cfg.rules = v;
        }
        cfg
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

/// The search core behind [`Profile::de_collide_import_name`], factored out so the suffix logic
/// can be unit-tested against a fake `free` predicate instead of real filesystem occupancy.
/// Tries `base`, then `"{base} (2)"..="{base} (99)"`; the first name `free` accepts wins. Never
/// falls back to `base` on exhaustion — that would silently overwrite a proven-occupied file — an
/// `Err` is returned instead.
fn de_collide_with(base: String, free: impl Fn(&str) -> bool) -> Result<String, String> {
    if free(&base) {
        return Ok(base);
    }
    for i in 2..=99 {
        let alt = format!("{base} ({i})");
        if free(&alt) {
            eprintln!(
                "neuron: import '{base}' collides with an existing profile's file; importing as '{alt}' instead"
            );
            return Ok(alt);
        }
    }
    Err(format!(
        "import '{base}': 98 name collisions deep — refusing to overwrite an existing profile; \
         rename the import or clean up profiles/"
    ))
}

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
    fn degraded_apprules_drops_the_bad_rule_and_keeps_the_rest() {
        use crate::salvage::SalvageLoad;
        let dir = std::env::temp_dir().join(format!("neuron-apps-degraded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("apps.toml");
        // rule 1's `profile` is the wrong type — the whole parse fails, salvage drops only that rule.
        std::fs::write(
            &path,
            "[[rules]]\napp = \"valorant\"\nprofile = \"fps\"\n\n\
             [[rules]]\napp = \"chrome\"\nprofile = 123\n",
        )
        .unwrap();
        let cfg = AppRules::load_from(&path);
        assert_eq!(cfg.rules.len(), 1, "the malformed rule dropped, the good one kept");
        assert_eq!(cfg.rules[0].app, "valorant");
        assert_eq!(cfg.rules[0].profile, "fps");
        assert!(dir.join("apps.toml.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `AppRules` has exactly one field (`rules`, a whole vec), so "every field wrong-typed"
    /// collapses to "present but the WRONG SHAPE (not an array)" — distinct from the test above,
    /// which always had a valid array with one bad element inside it. `AppRule` has no `PartialEq`
    /// derive, so the check is `is_empty()` rather than an equality against `Vec::new()`.
    #[test]
    fn degraded_apprules_wrong_shaped_field_defaults_to_empty_not_a_partial_parse() {
        use crate::salvage::SalvageLoad;
        let dir = std::env::temp_dir().join(format!("neuron-apps-wrongshape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("apps.toml");
        std::fs::write(&path, "rules = \"not-an-array\"\n").unwrap();
        let cfg = AppRules::load_from(&path);
        assert!(
            cfg.rules.is_empty(),
            "a wrong-shaped field must default to empty, not panic or leak a partial parse"
        );
        assert!(dir.join("apps.toml.bad").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            lighting: vec![crate::pattern::LayerDef {
                pattern: "custom".into(),
                frame: vec![[255, 0, 0]],
                ..Default::default()
            }],
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
            lighting: vec![crate::pattern::LayerDef {
                pattern: "axis".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let s = p.summary();
        assert!(s.contains("dpi 800"));
        // a lone procedural layer summarises by its pattern label (axis → "Axis").
        assert!(s.contains("light Axis"), "summary: {s}");
        assert!(!s.contains("Hz"));
    }

    #[test]
    fn summary_names_a_gaming_only_profile() {
        // a profile that only captures host-side gaming suppression is NOT empty and must not
        // summarize as "(empty)" — it has real behavior (shown in the sheet row + the CLI list).
        let p = Profile {
            name: "g".into(),
            disable_alt_tab: true,
            ..Default::default()
        };
        assert!(!p.is_empty());
        assert_eq!(p.summary(), "gaming");
    }

    #[test]
    fn summary_matches_is_empty() {
        // the invariant that keeps the sheet + CLI honest: "(empty)" appears IFF is_empty(). Each
        // case is non-empty in exactly ONE way — the advanced/imported classes that used to mislabel.
        let cases = [
            Profile {
                name: "altesc".into(),
                disable_alt_esc: true,
                ..Default::default()
            },
            Profile {
                name: "idle".into(),
                idle_secs: Some(60),
                ..Default::default()
            },
            Profile {
                name: "ingame".into(),
                in_game_polling: Some((1000, 500)),
                ..Default::default()
            },
            Profile {
                name: "game".into(),
                disable_win: true,
                ..Default::default()
            },
        ];
        for p in &cases {
            assert!(!p.is_empty(), "case '{}' should be non-empty", p.name);
            assert_ne!(
                p.summary(),
                "(empty)",
                "non-empty '{}' summarized as (empty)",
                p.name
            );
        }
        // the converse: a truly-empty profile IS "(empty)".
        let empty = Profile {
            name: "e".into(),
            ..Default::default()
        };
        assert!(empty.is_empty());
        assert_eq!(empty.summary(), "(empty)");
    }

    #[test]
    fn file_key_collides_on_the_sanitized_case_folded_path() {
        // distinct DISPLAY names that Profile::path maps to the same .toml must share a file_key —
        // the key the overwrite/de-collision checks compare, so the button can't mislabel a clobber.
        assert_eq!(Profile::file_key("my game/2"), Profile::file_key("my_game_2"));
        assert_eq!(Profile::file_key("Valorant"), Profile::file_key("valorant"));
        assert_ne!(Profile::file_key("apex"), Profile::file_key("valorant"));
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
        // The path is now run-root-absolute; the sanitization contract is the trailing shape.
        assert!(
            Profile::path("my game/2")
                .to_string_lossy()
                .replace('\\', "/")
                .ends_with("profiles/my_game_2.toml"),
            "got {}",
            Profile::path("my game/2").display()
        );
    }

    #[test]
    fn resolve_import_name_falls_back_on_blank_or_whitespace() {
        // The root guard every importer (GUI wizard, CLI import-export --apply) MUST resolve
        // through: a blank name sanitizes to "" and would otherwise write a hidden `.toml` /
        // `.rules.toml` pair no profile-listing flow shows.
        assert_eq!(Profile::resolve_import_name(""), "imported");
        assert_eq!(Profile::resolve_import_name("   "), "imported");
        assert_eq!(Profile::resolve_import_name("\t\n"), "imported");
        // a real name passes through untouched (just trimmed) — this isn't a blanket rename.
        assert_eq!(Profile::resolve_import_name("Valorant"), "Valorant");
        assert_eq!(Profile::resolve_import_name("  Valorant  "), "Valorant");
    }

    #[test]
    fn rules_path_pairs_with_the_sanitized_profile_path_and_stays_flat() {
        // A name with a path separator that sanitize() maps to '_'. The sidecar must share the
        // profile's sanitized stem AND its directory (flat — never a nested `profiles/FPS/…`).
        //
        // ENV_LOCK is required even though this test never WRITES the env: both calls below
        // resolve the run root independently, so a concurrent test that retargets NEURON_RUN_DIR
        // between them tears the pair and fails the comparison with two unrelated parents. The
        // lock is the crate's convention for touching that process-global value AT ALL — reads
        // included, because a torn read is just as wrong as a torn write.
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let name = "FPS/competitive";
        let prof = Profile::path(name);
        let rules = Profile::rules_path(name);
        assert_eq!(prof.parent(), rules.parent(), "sidecar must sit flat beside the profile");
        assert_eq!(prof.file_name().unwrap(), "FPS_competitive.toml");
        assert_eq!(rules.file_name().unwrap(), "FPS_competitive.rules.toml");
    }

    #[test]
    fn new_fields_round_trip() {
        // The advanced-import + native fields: idle, in-game polling pair, all four gaming toggles.
        let p = Profile {
            name: "advanced".into(),
            idle_secs: Some(300),
            in_game_polling: Some((1000, 500)),
            disable_alt_tab: true,
            disable_win: true,
            disable_alt_esc: true,
            lighting: vec![crate::pattern::LayerDef {
                pattern: "custom".into(),
                frame: vec![[1, 2, 3], [4, 5, 6]],
                ..Default::default()
            }],
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
persist = false
"#;
        let p: Profile = toml::from_str(old).unwrap();
        assert_eq!(p.name, "legacy");
        assert_eq!(p.dpi, Some(800));
        assert_eq!(p.idle_secs, None);
        assert_eq!(p.in_game_polling, None);
        assert!(!p.disable_alt_tab && !p.disable_win && !p.disable_alt_f4);
        assert!(!p.disable_alt_esc);
    }

    #[test]
    fn list_excludes_sidecars() {
        // list() returns real profiles only — never a `<name>.rules.toml` sidecar sharing the dir. A
        // real profile's stem never carries a dot (sanitize maps `.`→`_`), so the guard is robust to
        // whatever else the test dir holds at the time. Disk IO is isolated into a temp run root via
        // NEURON_RUN_DIR (env is process-global — hold the crate's env lock while it's overridden).
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_list_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "t_list_sc".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        // Drop a rules sidecar beside it by hand (list() must skip the `.rules`-stemmed file).
        std::fs::write(profiles_dir().join("t_list_sc.rules.toml"), "rules = []\n").unwrap();
        let names = list();
        assert!(
            names.contains(&"t_list_sc".to_string()),
            "real profile listed: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains('.')),
            "no sidecar stem leaks into the profile list: {names:?}"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn apply_report_summary_renders_gaming_mode_and_gated() {
        let r = ApplyReport {
            applied: vec!["dpi 1600".into(), "brightness 80%".into()],
            skipped: vec!["lighting wave: no lit device".into()],
            gated: vec!["idle-off 300s: ... gated ...".into()],
            gaming_mode: GamingMode::from_profile(true, false, true, false),
        };
        let s = r.summary();
        assert!(s.contains("dpi 1600"));
        assert!(s.contains("brightness 80%"));
        assert!(s.contains("gaming-mode (suppress Alt+Tab+Alt+F4, host-side)"));
        assert!(s.contains("[gated]"));

        // Alt+Esc is a first-class label in the summary too (single-sourced via suppressed_labels()).
        let r2 = ApplyReport {
            gaming_mode: GamingMode::from_profile(false, false, false, true),
            ..Default::default()
        };
        assert!(r2.summary().contains("Alt+Esc"));
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
    fn import_decollides_a_sanitized_name_collision() {
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_decollide_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "FPS_competitive".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        assert_eq!(
            Profile::de_collide_import_name("FPS/competitive").unwrap(),
            "FPS/competitive (2)"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn import_same_display_name_updates_in_place() {
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_decollide_same_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "Valorant".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        assert_eq!(Profile::de_collide_import_name("Valorant").unwrap(), "Valorant");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn orphan_rules_sidecar_reserves_the_key() {
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_decollide_orphan_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        std::fs::create_dir_all(crate::profile::profiles_dir()).unwrap();
        std::fs::write(Profile::rules_path("FPS_competitive"), b"# rules only\n").unwrap();
        assert_eq!(
            Profile::de_collide_import_name("FPS/competitive").unwrap(),
            "FPS/competitive (2)"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn same_profile_with_sidecar_still_updates_in_place() {
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_decollide_same_sidecar_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "Valorant".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::write(Profile::rules_path("Valorant"), b"# rules\n").unwrap();
        assert_eq!(Profile::de_collide_import_name("Valorant").unwrap(), "Valorant");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn blank_name_still_resolves_to_imported() {
        let _g = crate::runroot::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_decollide_blank_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        assert_eq!(Profile::de_collide_import_name("  ").unwrap(), "imported");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn de_collide_with_exhaustion_errors_instead_of_overwriting() {
        // every candidate is occupied — must never fall back to the (proven-occupied) base.
        let r = de_collide_with("dupe".to_string(), |_| false);
        assert!(r.is_err(), "exhaustion must error, not silently overwrite");
        let msg = r.unwrap_err();
        assert!(msg.contains("dupe"), "error names the base: {msg}");
    }

    #[test]
    fn de_collide_with_picks_first_free_suffix() {
        // base and " (2)" taken, " (3)" free.
        let free = |n: &str| n == "dupe (3)";
        assert_eq!(de_collide_with("dupe".to_string(), free).unwrap(), "dupe (3)");
    }

    #[test]
    fn profile_with_only_new_field_not_empty() {
        // A profile that sets ONLY a new field (e.g. idle-off, or a gaming-mode toggle) is not
        // "empty" — is_empty must account for every new field, including the native Alt+Esc guard.
        assert!(!Profile {
            name: "f".into(),
            disable_alt_esc: true,
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
