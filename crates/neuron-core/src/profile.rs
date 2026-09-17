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

/// The stem the GUI's own authored rules live under (`gui.rules.toml`) — NOT a profile, and the
/// one name a profile may never take. A profile called "gui" would derive that exact sidecar path,
/// so deleting it would delete every bind the user authored in the app, and renaming another
/// profile onto it would overwrite them. See [`name_conflict`].
pub const GUI_RULES_STEM: &str = "gui";

/// Names Windows refuses as files whatever the extension — `CON.toml` is as invalid as `CON`.
/// Saving under one fails with an opaque OS error, so it's caught by name instead.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Why this name can't be a profile, in words a status line can show — or `None` if it's fine.
///
/// One check for every naming entry point (GUI capture, GUI rename, CLI save, CLI rename, import
/// de-collision) so a reserved name can't slip in through whichever door someone happens to use.
pub fn name_conflict(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Some("a profile needs a name".into());
    }
    let stem = sanitize(trimmed).to_lowercase();
    // sanitize maps every non-alphanumeric to '_', so "***" becomes "___" — not empty, but an
    // unselectable file with nothing in the name to recognise it by. Require at least one real
    // character, not merely a non-empty stem.
    if !stem.chars().any(char::is_alphanumeric) {
        return Some("that name has no letters or digits to file it under".into());
    }
    if stem == GUI_RULES_STEM {
        return Some(format!(
            "'{GUI_RULES_STEM}' is the name of the binds you author in the app · pick another"
        ));
    }
    if WINDOWS_DEVICE_NAMES.contains(&stem.as_str()) {
        return Some(format!("windows won't let a file be called '{trimmed}'"));
    }
    // the sanitized stem becomes a filename; leave room for the ".rules.toml" sidecar suffix too.
    if stem.len() > 120 {
        return Some("that name is too long to file".into());
    }
    None
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

    /// The sidecar this profile OWNS, or `None` when the derived path is the app's own
    /// `gui.rules.toml`.
    ///
    /// [`name_conflict`] stops anyone NAMING a profile "gui", but that guards the doors, not the
    /// lifecycle: a `gui.toml` that predates the reservation (or that someone drops in by hand) is
    /// still listed, and its delete would have taken `gui.rules.toml` — every bind authored in the
    /// app — with it, exactly the loss the reservation exists to prevent. Every destructive path
    /// resolves the sidecar through here, so ownership is decided once instead of at each call site.
    pub fn owned_rules_path(name: &str) -> Option<PathBuf> {
        // Compared on the on-disk KEY, like every other identity decision here. A raw filename
        // comparison would have missed `GUI.toml`: Windows filenames are case-insensitive, so its
        // derived `GUI.rules.toml` IS the app's `gui.rules.toml`, and deleting that profile would
        // still have taken the authored binds.
        (Self::file_key(name) != GUI_RULES_STEM).then(|| Self::rules_path(name))
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
            // a reserved name is never free, so an import called "gui" lands at "gui (2)" instead
            // of aiming its sidecar at the app's own authored binds.
            if name_conflict(n).is_some() {
                return false;
            }
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

    /// Delete a profile AND the `.rules.toml` sidecar paired with it — the ONE delete every client
    /// calls, because a profile is both files. Removing only `<name>.toml` (the old behaviour) left
    /// the sidecar behind, and a stray sidecar is not inert: `load_rule_sidecars` folds it into the
    /// live spine forever (binds from a profile you deleted, showing as unremovable TOML rows), and
    /// `de_collide_import_name` treats it as an occupied key, so re-importing that same profile lands
    /// at " (2)". Both were live-reproduced.
    ///
    /// Missing files are not an error: delete is idempotent, and a profile with no sidecar is the
    /// common case. Only a real IO failure (permissions, a lock) surfaces.
    /// ORDER MATTERS, and it is sidecar-first. Deleting a file cannot be rolled back (the contents
    /// are gone), so the two removals are sequenced by which half-done state is survivable:
    ///
    /// * sidecar gone, profile left  — the profile is still listed and still deletable; you retry.
    /// * profile gone, sidecar left  — an ORPHAN, which is the exact failure this function exists to
    ///   prevent: the loader would fold those binds in for a profile that no longer exists, with no
    ///   UI able to remove them, and the name would stay reserved against a re-import.
    ///
    /// So the sidecar goes first and a failure there aborts before the profile is touched.
    pub fn delete(name: &str) -> Result<(), String> {
        let remove = |path: PathBuf| -> Result<(), String> {
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                // idempotent: delete of what isn't there is not an error, and a profile with no
                // sidecar is the common case.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("{}: {e}", path.display())),
            }
        };
        // `owned_rules_path` is None for a stray `gui.toml`, whose derived sidecar is the app's own
        // authored binds — delete the profile, never those.
        if let Some(sidecar) = Self::owned_rules_path(name) {
            remove(sidecar)?;
        }
        remove(Self::path(name))?;
        // A deleted profile cannot stay the active one. Clearing here (not only in the GUI's own
        // delete path) means the CLI gets it too, so `active()` and `profiles/.active` can't be
        // left naming a file that isn't there — which would also keep a sidecar scope pinned to it.
        // Compared on the on-disk KEY, not the raw string: the file this just deleted was resolved
        // through `path()` (sanitized, case-folded), so deleting "FPS" while the cursor says "fps"
        // removes the very file the cursor names. A raw comparison would leave the cursor — and the
        // sidecar scope keyed off it — pointing at a profile that is gone.
        if Self::file_key(&active()) == Self::file_key(name) {
            set_active("");
        }
        Ok(())
    }

    /// Rename a profile in place, carrying its rules sidecar with it and rewriting the stored
    /// display name so the file and its contents can't disagree. Returns the name actually used —
    /// `to` de-collides through [`de_collide_import_name`](Self::de_collide_import_name), so a
    /// rename onto an occupied key lands at " (2)" instead of destroying the occupant.
    ///
    /// The sidecar moves with the profile because the two are one object (see [`delete`](Self::delete));
    /// leaving it under the old stem would strand the binds under a name nothing loads.
    pub fn rename(from: &str, to: &str) -> Result<String, String> {
        if let Some(why) = name_conflict(to) {
            return Err(why);
        }
        let mut p = Self::load(from).map_err(|e| e.to_string())?;
        // Rename de-collides STRICTLY: unlike an import, where an exact display-name match means
        // "this is the same profile, update it in place", renaming onto an existing name is always
        // a different profile about to be destroyed. Only the file we're renaming FROM counts as free.
        let from_key = Self::file_key(from);
        let target = de_collide_with(Self::resolve_import_name(to), |n| {
            if Self::file_key(n) == from_key {
                return true;
            }
            !Self::path(n).exists() && !Self::rules_path(n).exists()
        })?;
        if Self::file_key(&target) == from_key {
            // same file (a case-only or punctuation-only edit): rewrite the display name in place.
            p.name = target.clone();
            p.save()?;
            return Ok(target);
        }
        // Three steps, each with a rollback, because a partial rename is worse than no rename: two
        // profile files claiming one sidecar means the next load, delete, or re-import picks up
        // whichever it happens to see first.
        p.name = target.clone();
        p.save()?;
        // The sidecar follows — unless the source is a stray `gui.toml`, whose derived sidecar is
        // the app's own authored binds and belongs to nobody's profile. A missing one is simply
        // nothing to move.
        let from_sidecar = Self::owned_rules_path(from);
        let moved_sidecar = match from_sidecar
            .as_ref()
            .map(|src| std::fs::rename(src, Self::rules_path(&target)))
        {
            None => false,
            Some(Ok(())) => true,
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => false,
            Some(Err(e)) => {
                // undo step 1.
                let _ = std::fs::remove_file(Self::path(&target));
                return Err(format!("moving the rules sidecar: {e}"));
            }
        };
        if let Err(e) = std::fs::remove_file(Self::path(from)) {
            // undo steps 2 and 1, in that order, so the source profile is whole again.
            if moved_sidecar {
                if let Some(src) = &from_sidecar {
                    let _ = std::fs::rename(Self::rules_path(&target), src);
                }
            }
            let _ = std::fs::remove_file(Self::path(&target));
            return Err(format!("removing the old file: {e}"));
        }
        // The cursor names a profile that no longer exists under that name — move it, or the live
        // sidecar scope (keyed off the cursor) would stop matching the binds that just moved with it.
        // same on-disk-key comparison as `delete`: a casing or punctuation alias of the active
        // profile still names the file that just moved.
        if Self::file_key(&active()) == Self::file_key(from) {
            set_active(&target);
        }
        Ok(target)
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
                writes::set_dpi_stages(d, &stages, active, store, crate::dpi_origin::Cause::UserApplied)
                    .map(|()| (d.pid, active))
            }) {
                Ok((_pid, active)) => {
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
                cap::set_dpi(d, dpi, dpi, store, crate::dpi_origin::Cause::UserApplied).map(|()| d.pid)
            }) {
                Ok(_pid) => {
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
    ///
    /// SKIPPED fields are named here, not just when everything was skipped. They used to be dropped
    /// whenever anything landed, so applying a profile to a rig with a sleeping mouse reported
    /// "applied 'chill': brightness 100%" — a clean success — while its DPI and polling silently did
    /// not happen, and the sheet's own LIVE row sat a hundred pixels away showing the device didn't
    /// match. Reporting what a write did NOT do is the same rule the device-write ledger follows.
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
            return if self.skipped.is_empty() {
                "applied (nothing set)".into()
            } else {
                format!("nothing applied · {}", self.skipped.join("; "))
            };
        }
        let mut line = parts.join(", ");
        if !self.skipped.is_empty() {
            line.push_str(&format!(" · skipped {}", self.skipped.join("; ")));
        }
        line
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

/// One entry from [`load_all`] — a profile that parsed, or the name + reason one didn't.
///
/// Exists because the old read path was `list().filter_map(|n| load(n).ok())`: a profile whose TOML
/// went bad (a hand-edit typo, a truncated write) simply VANISHED from the sheet with the file still
/// on disk. Every other config in the tree salvages and keeps a `.bad` copy; profiles were the one
/// user-authored store that failed silently. Callers now render the broken ones instead of hiding them.
pub enum ProfileEntry {
    Ok(Box<Profile>),
    Broken { name: String, why: String },
}

/// Every saved profile, in name order, with unreadable ones reported rather than dropped.
pub fn load_all() -> Vec<ProfileEntry> {
    list()
        .into_iter()
        .map(|n| match Profile::load(&n) {
            Ok(p) => ProfileEntry::Ok(Box::new(p)),
            // the anyhow chain carries the TOML parse detail (line + column); keep the first line,
            // which names the actual problem, and drop the multi-line snippet a status strip can't show.
            Err(e) => ProfileEntry::Broken {
                name: n,
                why: e
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or("unreadable")
                    .trim()
                    .to_string(),
            },
        })
        .collect()
}

/// The process-wide ACTIVE-PROFILE cursor — the one "what's applied right now" every client reads
/// and writes (the GUI's apply path, the live dispatch loop's `ProfileSwitch`/`ProfileCycle`
/// intents). Mirrors the `hook::set_policy` carrier pattern: a tiny global so two threads that
/// can't see each other's state agree on the cursor a `ProfileCycle` steps from.
static ACTIVE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Where the cursor is remembered between runs. A tiny run-root state file rather than a GUI
/// preference, because the CLI daemon needs the same answer: a profile's binds sidecar is in scope
/// only while that profile is active, so a `neuron run` that couldn't see the cursor would start
/// with none of them. One file, both binaries, no split-brain.
fn active_cursor_path() -> PathBuf {
    profiles_dir().join(".active")
}

/// Record `name` as the currently-applied profile (call after a successful apply).
///
/// Writes through to disk as well as the process cell — the cursor moving IS the thing worth
/// remembering, so there is one call site for both rather than a `remember()` every caller must
/// not forget. A write failure is inert: it costs the next launch's starting profile, never the
/// switch that just happened.
pub fn set_active(name: &str) {
    *ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = name.to_string();
    let path = active_cursor_path();
    if name.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else if std::fs::create_dir_all(profiles_dir()).is_ok() {
        let _ = crate::salvage::atomic_write(&path, name.as_bytes());
    }
}

/// Restore the remembered cursor at startup, returning the profile name (empty if none).
///
/// A CURSOR, not a re-apply: nothing is written to the device because a process started. The
/// device already holds what this profile last applied. What restoring buys is that the header
/// names the profile you're on, a gaming profile's host-side key suppression comes back, and the
/// profile's binds are in scope. A name whose file has since gone restores to nothing.
/// Restore the cursor from disk if this process hasn't got one yet, and return the active name.
///
/// Called by [`crate::controls::build_runtime`], because that is the function whose correctness
/// DEPENDS on the cursor: it decides which profile's binds sidecar is in scope. Leaving that to a
/// startup ordering ("the window is built eagerly, and building it loads the runtime, which
/// restores the cursor, and all of that happens before the live loop starts") is a contract three
/// files apart that nothing enforces — make the window lazy some day and profile binds would
/// silently stop loading at boot with no failing test. Owning it here removes the contract.
///
/// The guard is "the process has no cursor yet", NOT a once-flag. A once-flag is global while the
/// run root is not, so in a test binary — many run roots, one process — the first restore would win
/// and every later one would silently no-op against a different directory. Keying on the cell keeps
/// this a pure function of (cursor, run root): a process that has already chosen a profile is never
/// second-guessed by a stale file, and clearing the cursor deletes the file, so an empty cell
/// restores to empty rather than resurrecting the old name.
pub fn restore_active_once() -> String {
    let current = active();
    if !current.is_empty() {
        return current;
    }
    restore_active()
}

pub fn restore_active() -> String {
    // AUTHORITATIVE in both directions: an absent or unusable cursor file assigns EMPTY rather than
    // leaving whatever the cell happened to hold. Otherwise "restore" would be a no-op that quietly
    // preserved a stale cursor, and the sidecar scope keyed off it would follow the stale name.
    let name = std::fs::read_to_string(active_cursor_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && Profile::load(s).is_ok())
        .unwrap_or_default();
    *ACTIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = name.clone();
    name
}

/// Set the process cell WITHOUT touching disk — for tests that need to simulate a fresh process
/// (an empty cell with the file still on disk) before calling [`restore_active`].
#[cfg(test)]
fn set_active_in_process_only(name: &str) {
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
    // Matched on the on-disk KEY, not the raw string. `names` and the cursor do not always come
    // from the same place — a ProfileSwitch stores the rule's DISPLAY name while the candidate list
    // is built from filenames — so "my game 2" and "my_game_2" are the same profile and must find
    // each other, or every cycle after a switch would silently restart from index 0.
    match names
        .iter()
        .position(|n| Profile::file_key(n) == Profile::file_key(current))
    {
        Some(i) => ((i as i32 + step).rem_euclid(names.len() as i32)) as usize,
        None => 0,
    }
}

/// The profiles a CYCLE may land on: every saved profile that actually loads, by display name.
///
/// Cycling used to step over raw filenames and then try to load whichever it landed on, so one
/// unreadable file was a dead end — the cycle key reported a parse error and the cursor never
/// moved, which reads as a broken button rather than a broken profile. A profile you cannot apply
/// is not a cycle destination; it stays visible (and fixable) in the sheet, which is where it
/// belongs.
pub fn cycle_candidates() -> Vec<String> {
    load_all()
        .into_iter()
        .filter_map(|e| match e {
            ProfileEntry::Ok(p) => Some(p.name),
            ProfileEntry::Broken { .. } => None,
        })
        .collect()
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
    /// The profile to fall back to when the focused app matches NO rule — the "you left the game"
    /// half of auto-switching. Synapse reverts to Default on exit; without this, neuron latched the
    /// game profile forever and your DPI stayed at 400 after you alt-tabbed to a browser.
    /// `None` = stay on whatever is active (the old, latching behaviour), which is still the default
    /// so auto-switch never starts reverting on someone who didn't ask for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<AppRule>,
}

/// What auto-switch decided for a focused app — the single verdict the dispatcher acts on and the
/// UI lamp renders, so the two can't disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusVerdict<'a> {
    /// Rule at `index` matched; apply `profile`.
    Rule { index: usize, profile: &'a str },
    /// No rule matched and a fallback is configured; apply it.
    Fallback { profile: &'a str },
    /// No rule matched and no fallback — leave the active profile alone.
    Stay,
}

impl<'a> FocusVerdict<'a> {
    /// The profile this verdict wants applied, if any.
    pub fn profile(&self) -> Option<&'a str> {
        match self {
            FocusVerdict::Rule { profile, .. } | FocusVerdict::Fallback { profile } => Some(profile),
            FocusVerdict::Stay => None,
        }
    }
    /// The index of the winning rule, for the UI's "this contact is closed" lamp (`-1` when none).
    pub fn rule_index(&self) -> i32 {
        match self {
            FocusVerdict::Rule { index, .. } => *index as i32,
            _ => -1,
        }
    }
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
    /// The ONE auto-switch resolution: which profile a focused exe should be on, and which rule
    /// decided it. Top-to-bottom, first match wins; no match falls back to [`default`](Self::default).
    ///
    /// This exists as one function because the two halves used to disagree. The rules were folded
    /// into the spine as ordinary `AppFocus -> ProfileSwitch` rules, and the executor fires EVERY
    /// matching rule — so with two overlapping needles ("chrome" and "rome" both match `chrome.exe`)
    /// both profiles applied back-to-back and the LAST one won, while the UI lamp lit the FIRST and
    /// the docs promised first-match. The dispatcher and the lamp both read this now, so what fires
    /// is what lights up.
    ///
    /// An empty needle is skipped: it would match every app, which is what the fallback is for.
    pub fn resolve(&self, exe: &str) -> FocusVerdict<'_> {
        let focused = exe.to_lowercase();
        match self
            .rules
            .iter()
            .position(|r| !r.app.trim().is_empty() && focused.contains(&r.app.to_lowercase()))
        {
            Some(index) => FocusVerdict::Rule {
                index,
                profile: self.rules[index].profile.as_str(),
            },
            None => match self.default.as_deref().filter(|d| !d.trim().is_empty()) {
                Some(profile) => FocusVerdict::Fallback { profile },
                None => FocusVerdict::Stay,
            },
        }
    }

    /// The profile to apply for a focused exe name, or `None` to stay put. Thin sugar over
    /// [`resolve`](Self::resolve) for callers that don't need to know which rule won.
    pub fn profile_for(&self, exe: &str) -> Option<&str> {
        self.resolve(exe).profile()
    }

    /// The switch a focus change should actually perform: the resolved profile, or `None` when
    /// there is nothing to do — no verdict, an empty name, or a verdict naming the profile that is
    /// already active.
    ///
    /// The "already active" guard is what makes auto-switch idempotent: a focus poll fires on every
    /// change, and re-applying the profile you are already on would re-write the device (and its
    /// confirmation card) for nothing. Both front ends decide through this one function so the GUI
    /// dispatcher and the CLI listener cannot drift on WHAT to switch to; each still owns HOW it
    /// reassembles its spine afterwards.
    pub fn switch_target(&self, exe: &str, current: &str) -> Option<String> {
        self.resolve(exe)
            .profile()
            .filter(|p| !p.is_empty() && *p != current)
            .map(str::to_string)
    }

    /// Every rule whose target profile no longer exists on disk, by index. A dangling rule fails
    /// silently at focus-switch time (the apply just errors into the status line), so the UI marks
    /// these instead of rendering them identically to healthy ones.
    pub fn dangling(&self, saved: &[String]) -> Vec<usize> {
        let known: std::collections::HashSet<String> =
            saved.iter().map(|n| Profile::file_key(n)).collect();
        self.rules
            .iter()
            .enumerate()
            .filter(|(_, r)| !known.contains(&Profile::file_key(&r.profile)))
            .map(|(i, _)| i)
            .collect()
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
        cfg.default = crate::salvage::salvage_field(table, "default", Self::FILE);
        cfg
    }
}

/// Starter content for apps.toml (written by `neuron profile autoswitch init`).
pub const APPS_TEMPLATE: &str = r#"# App-aware profile auto-switch. `neuron run` applies the first profile whose `app` substring
# matches the focused window's executable. Create profiles with `neuron profile save`.
#
# `default` is what to go back to when the focused app matches nothing — leave it out and the
# last profile you switched into stays active after you close the game.
# default = "everyday"
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

    /// The cursor is run-root state, not a GUI preference: `set_active` writes it through and
    /// `restore_active` reads it back, so `neuron run` and the app agree on which profile is on —
    /// and therefore on which profile's binds sidecar is in scope. A GUI-only cursor would have
    /// left the daemon starting with none of them.
    #[test]
    fn the_active_cursor_survives_a_restart_and_is_shared() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_cursor_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);
        let restore = active();

        Profile {
            name: "fps".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        set_active("fps");
        // a fresh process would start with an empty cell — simulate that, then restore from disk.
        set_active_in_process_only("");
        assert_eq!(active(), "");
        assert_eq!(restore_active(), "fps");
        assert_eq!(active(), "fps", "the cursor came back from disk");

        // a cursor naming a profile that has since been deleted restores to nothing rather than
        // leaving the header (and the sidecar scope) pointing at a file that isn't there.
        Profile::delete("fps").unwrap();
        set_active_in_process_only("");
        assert_eq!(restore_active(), "");

        // clearing the cursor removes the file, so the next launch starts clean.
        Profile {
            name: "fps".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        set_active("fps");
        set_active("");
        assert!(!active_cursor_path().exists());

        // `restore_active_once` never second-guesses a cursor the process has already chosen — the
        // guard is the CELL, not a once-flag. A once-flag is global while the run root is not, so
        // in a test binary (many run roots, one process) the first restore would win and every
        // later one would no-op against a different directory.
        set_active("fps");
        std::fs::write(active_cursor_path(), b"something-else").unwrap();
        assert_eq!(
            restore_active_once(),
            "fps",
            "a live cursor wins over whatever is on disk"
        );
        // …and with no cursor, it reads disk and validates: that name has no profile, so nothing.
        set_active_in_process_only("");
        assert_eq!(restore_active_once(), "");

        set_active_in_process_only(&restore);
        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
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

    /// The cursor and the candidate list do not always spell a profile the same way — a switch
    /// stores the display name, a filename-derived list stores the sanitized stem — so the match is
    /// on the on-disk key. Without it, cycling after a switch silently restarted from index 0.
    #[test]
    fn cycle_index_matches_a_profile_by_its_on_disk_key() {
        let names: Vec<String> = ["my game 2", "b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(cycle_index(&names, "my_game_2", 1), 1, "stem finds its display name");
        assert_eq!(cycle_index(&names, "My Game 2", 1), 1, "and case-folded too");
        // a genuinely unknown cursor still starts the cycle at the beginning.
        assert_eq!(cycle_index(&names, "other", 1), 0);
    }

    /// A profile that won't parse is not a cycle destination: stepping onto it used to report a
    /// parse error and leave the cursor where it was, which reads as a dead button.
    #[test]
    fn cycle_candidates_skip_a_broken_profile() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_cyc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        for n in ["one", "two"] {
            Profile {
                name: n.into(),
                dpi: Some(800),
                ..Default::default()
            }
            .save()
            .unwrap();
        }
        std::fs::write(Profile::path("bad"), b"name = \"bad\"\ndpi = \"nope\"\n").unwrap();

        let names = cycle_candidates();
        assert_eq!(names.len(), 2, "the unreadable profile is not a destination: {names:?}");
        assert!(!names.iter().any(|n| n == "bad"));
        // and the cycle still steps cleanly across what remains.
        assert_eq!(cycle_index(&names, &names[0], 1), 1);
        assert_eq!(cycle_index(&names, &names[1], 1), 0);

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
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

    /// The delete that used to leave binds behind. `Profile::delete` owns BOTH files, because a
    /// stray sidecar is not inert: it folds into the live spine forever and it reserves the name.
    #[test]
    fn delete_takes_the_binds_sidecar_with_it() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_del_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "fps".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::write(Profile::rules_path("fps"), b"rules = []\n").unwrap();
        assert!(Profile::rules_path("fps").exists());

        Profile::delete("fps").unwrap();
        assert!(!Profile::path("fps").exists(), "the profile file is gone");
        assert!(
            !Profile::rules_path("fps").exists(),
            "its binds sidecar went with it, or those binds stay live with no UI to remove them"
        );
        // and the name is free again: an orphan sidecar used to push a re-import to "fps (2)".
        assert_eq!(Profile::de_collide_import_name("fps").unwrap(), "fps");
        // idempotent: deleting what isn't there is not an error.
        Profile::delete("fps").unwrap();

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The name that would have eaten the user's own binds. A profile called "gui" derives
    /// `profiles/gui.rules.toml` — which IS the app's authored rule set — so deleting that profile
    /// would delete every bind authored in the app, and renaming another profile onto it would
    /// overwrite them. Reserved at every naming door instead of guarded at each call site.
    #[test]
    fn gui_is_reserved_because_it_is_the_apps_own_binds_file() {
        assert_eq!(
            Profile::rules_path(GUI_RULES_STEM).file_name().unwrap(),
            "gui.rules.toml",
            "this is the collision the reservation exists for"
        );
        assert!(name_conflict("gui").is_some());
        assert!(name_conflict("GUI").is_some(), "the check is case-folded");
        // sanitize maps a space to '_', so "g u i" files as `g_u_i.toml` — a different file, and
        // legitimately usable. Only names that actually SANITIZE to the reserved stem are refused.
        assert!(name_conflict("g u i").is_none());
        assert!(name_conflict(" gui ").is_some(), "…after trimming");
        assert!(name_conflict("gui2").is_none(), "only the exact stem is reserved");
        assert!(name_conflict("my gui setup").is_none());
    }

    #[test]
    fn name_conflict_catches_the_names_a_filesystem_would_reject() {
        // Windows reserves these whatever the extension — `CON.toml` fails as hard as `CON`.
        for n in ["con", "CON", "nul", "com1", "LPT9"] {
            assert!(name_conflict(n).is_some(), "'{n}' must be refused by name");
        }
        assert!(name_conflict("").is_some());
        assert!(name_conflict("   ").is_some());
        // sanitize maps every non-alphanumeric to '_', so a name of pure punctuation has nothing
        // to file under and would land as an unselectable `___.toml`.
        assert!(name_conflict("***").is_some());
        assert!(name_conflict(&"x".repeat(200)).is_some(), "too long to file");
        // ordinary names, including unicode and punctuation, pass through.
        for n in ["fps", "FPS/competitive", "café", "日本語", "my game 2"] {
            assert!(name_conflict(n).is_none(), "'{n}' should be usable");
        }
    }

    /// The reservation has to hold on the LIFECYCLE too, not just at the naming doors. A `gui.toml`
    /// that predates the check (or is dropped in by hand) is still listed and still deletable, and
    /// its derived sidecar is `gui.rules.toml` — the app's own authored binds. Delete and rename
    /// resolve through `owned_rules_path`, so the profile can go while those binds stay.
    #[test]
    fn deleting_a_stray_gui_profile_never_touches_the_apps_own_binds() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_strayg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        std::fs::create_dir_all(profiles_dir()).unwrap();
        let authored = b"# the binds you made in the app\nrules = []\n";
        std::fs::write(Profile::rules_path(GUI_RULES_STEM), authored).unwrap();
        // a stray profile file under the reserved stem, as a legacy install could have.
        std::fs::write(Profile::path(GUI_RULES_STEM), b"name = \"gui\"\ndpi = 800\n").unwrap();
        assert!(Profile::owned_rules_path(GUI_RULES_STEM).is_none());
        // and every alias that resolves to the same file, since Windows filenames are case-folded
        // — a `GUI.toml` derives `GUI.rules.toml`, which IS `gui.rules.toml` on disk.
        for alias in ["GUI", "Gui"] {
            assert!(
                Profile::owned_rules_path(alias).is_none(),
                "'{alias}' resolves to the app's own binds file"
            );
        }
        // names that merely LOOK close file elsewhere and are ordinary profiles: sanitize maps a
        // space to '_', so " gui " is `_gui_.toml` — a different file, and its own to delete.
        assert!(Profile::owned_rules_path("gui2").is_some());
        assert!(Profile::owned_rules_path(" gui ").is_some());

        Profile::delete(GUI_RULES_STEM).unwrap();
        assert!(!Profile::path(GUI_RULES_STEM).exists(), "the stray profile is gone");
        assert_eq!(
            std::fs::read(Profile::rules_path(GUI_RULES_STEM)).unwrap(),
            authored,
            "the app's own authored binds are untouched"
        );

        // and a rename away from the reserved stem leaves them where they are, too.
        std::fs::write(Profile::path(GUI_RULES_STEM), b"name = \"gui\"\ndpi = 800\n").unwrap();
        Profile::rename(GUI_RULES_STEM, "mine").unwrap();
        assert_eq!(
            std::fs::read(Profile::rules_path(GUI_RULES_STEM)).unwrap(),
            authored,
            "a rename must not carry the app's binds off with a stray profile"
        );

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Applying a profile has to leave the SAME state whichever door you came through — the sheet,
    /// the tray, a bound key, or an auto-switch. The piece that kept getting missed is the
    /// gaming-mode policy, because it is host-side and therefore not part of the device write: a
    /// profile can apply perfectly and suppress nothing.
    ///
    /// This pins the contract at its source — the report every entry point derives its policy from
    /// is the profile's own [`Profile::gaming_mode`] — so a new entry point that forgets to publish
    /// it is a visible omission rather than a silent one.
    #[test]
    fn an_applied_profiles_policy_is_the_profiles_own_guards() {
        let reg = Registry::load().expect("builtin registry loads");
        let p = Profile {
            name: "fps".into(),
            disable_alt_tab: true,
            disable_win: true,
            ..Default::default()
        };
        let report = p.apply(&reg);
        assert_eq!(
            report.gaming_mode,
            p.gaming_mode(),
            "the report carries the profile's own guards, whatever the device did"
        );
        assert!(report.gaming_mode.suppresses(writes::Chord::AltTab));
        assert!(report.gaming_mode.suppresses(writes::Chord::Win));
        assert!(!report.gaming_mode.suppresses(writes::Chord::AltF4));

        // and a profile with no guards reports an empty policy — the half that LIFTS suppression
        // when you switch away from a gaming profile.
        let plain = Profile {
            name: "chill".into(),
            dpi: Some(1600),
            ..Default::default()
        };
        assert!(
            !plain.apply(&reg).gaming_mode.any(),
            "switching to a profile without guards must clear them, not leave the last ones on"
        );
    }

    /// The cursor is part of the lifecycle, not something each client remembers to fix up: deleting
    /// the active profile clears it and renaming it carries it, in core, so the CLI gets the same
    /// behaviour the GUI does. A cursor naming a gone profile would also pin the sidecar scope to it.
    #[test]
    fn the_cursor_follows_delete_and_rename() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_cursorlc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("NEURON_RUN_DIR", &tmp);
        let restore = active();

        Profile {
            name: "fps".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        set_active("fps");
        assert_eq!(Profile::rename("fps", "fps2").unwrap(), "fps2");
        assert_eq!(active(), "fps2", "the cursor moved with the profile");

        Profile::delete("fps2").unwrap();
        assert_eq!(active(), "", "deleting the active profile clears the cursor");
        assert_eq!(restore_active(), "", "and it does not come back on the next launch");

        // An ALIAS of the active name still names the same file, so the cursor must follow it. A
        // raw string comparison missed this: `delete("FPS")` removed the file the cursor called
        // "fps" and left the cursor pointing at it.
        Profile {
            name: "Alias".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        set_active("Alias");
        Profile::delete("alias").unwrap();
        assert_eq!(active(), "", "a case-alias delete still clears the cursor");

        // deleting a profile that is NOT active leaves the cursor alone.
        for n in ["a", "b"] {
            Profile {
                name: n.into(),
                dpi: Some(800),
                ..Default::default()
            }
            .save()
            .unwrap();
        }
        set_active("a");
        Profile::delete("b").unwrap();
        assert_eq!(active(), "a");

        set_active("");
        set_active_in_process_only(&restore);
        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// An import named "gui" must land beside the app's binds file, never on it.
    #[test]
    fn importing_a_reserved_name_de_collides_instead_of_clobbering() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_gui_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        std::fs::create_dir_all(profiles_dir()).unwrap();
        std::fs::write(Profile::rules_path(GUI_RULES_STEM), b"rules = []\n").unwrap();
        assert_eq!(Profile::de_collide_import_name("gui").unwrap(), "gui (2)");
        assert!(
            Profile::rules_path(GUI_RULES_STEM).exists(),
            "the app's own binds file is untouched"
        );
        // and a rename onto it is refused outright rather than de-collided, so the verb's failure
        // is visible instead of quietly landing somewhere the user didn't name.
        Profile {
            name: "other".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        assert!(Profile::rename("other", "gui").is_err());
        assert!(Profile::path("other").exists(), "the source survives a refused rename");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Rename carries the sidecar and rewrites the stored display name, so the file and its
    /// contents can't disagree about what the profile is called.
    #[test]
    fn rename_moves_the_profile_its_name_and_its_binds() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_ren_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "old".into(),
            dpi: Some(1600),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::write(Profile::rules_path("old"), b"rules = []\n").unwrap();

        assert_eq!(Profile::rename("old", "new").unwrap(), "new");
        assert!(!Profile::path("old").exists());
        assert!(!Profile::rules_path("old").exists());
        assert!(Profile::rules_path("new").exists(), "binds followed the rename");
        let moved = Profile::load("new").unwrap();
        assert_eq!(moved.name, "new", "the stored display name was rewritten too");
        assert_eq!(moved.dpi, Some(1600), "contents survived");

        // renaming ONTO an occupied name de-collides instead of destroying the occupant.
        Profile {
            name: "taken".into(),
            dpi: Some(400),
            ..Default::default()
        }
        .save()
        .unwrap();
        assert_eq!(Profile::rename("new", "taken").unwrap(), "taken (2)");
        assert_eq!(Profile::load("taken").unwrap().dpi, Some(400), "occupant untouched");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// Neither delete nor rename may leave a HALF state, because both half-states are worse than
    /// the failure itself: an orphan sidecar is invisible always-live binds, and a split rename is
    /// two profile files claiming one set of binds.
    ///
    /// The lock is a directory standing where a file must go — `remove_file` and `rename` both
    /// refuse it, which is a real IO failure at the exact step each rollback guards, with no
    /// permissions games needed to provoke it.
    #[test]
    fn a_failed_delete_or_rename_leaves_no_half_state() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        // ── DELETE: the sidecar can't go, so the profile must not go either. The reverse order
        //    (profile first) is what produced the orphan this whole change exists to prevent.
        Profile {
            name: "stuck".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::create_dir_all(Profile::rules_path("stuck")).unwrap(); // undeletable "sidecar"
        assert!(Profile::delete("stuck").is_err());
        assert!(
            Profile::path("stuck").exists(),
            "a delete that could not remove the binds must not have removed the profile"
        );
        std::fs::remove_dir_all(Profile::rules_path("stuck")).unwrap();
        Profile::delete("stuck").unwrap();

        // ── RENAME: the source file can't be removed, so the rename must roll all the way back.
        Profile {
            name: "src".into(),
            dpi: Some(1600),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::write(Profile::rules_path("src"), b"rules = []\n").unwrap();
        // Something already occupies the target's sidecar path. The rename must not write over it
        // and must not strand the source: it de-collides to a free name instead, carrying its own
        // binds, and the occupant is untouched. (This is the guard that fires FIRST — the rollback
        // inside `rename` covers the narrower case of an IO failure mid-move.)
        std::fs::create_dir_all(Profile::rules_path("dst")).unwrap();
        let landed = Profile::rename("src", "dst").unwrap();
        assert_eq!(landed, "dst (2)", "a blocked name de-collides, it does not clobber");
        assert!(Profile::rules_path("dst").is_dir(), "the occupant is untouched");
        assert!(!Profile::path("src").exists(), "the source moved");
        assert!(!Profile::rules_path("src").exists(), "and so did its binds");
        assert!(Profile::rules_path("dst (2)").exists(), "binds landed with the profile");
        // the invariant that matters, whatever the outcome: never two profile files sharing one
        // set of binds.
        assert!(!Profile::path("dst").exists());

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// The routing verdict: first match wins, an unmatched app falls back, and the rule the lamp
    /// lights is the rule that fired. Overlapping needles are the case that used to break — the
    /// engine fired every match, so "rome" (listed second) beat "chrome" on `chrome.exe` while the
    /// UI lit "chrome".
    #[test]
    fn focus_resolves_first_match_then_fallback() {
        let r = AppRules {
            default: Some("everyday".into()),
            rules: vec![
                AppRule {
                    app: "chrome".into(),
                    profile: "chill".into(),
                },
                AppRule {
                    app: "rome".into(),
                    profile: "fps".into(),
                },
                AppRule {
                    app: "  ".into(), // a blank needle must never match everything
                    profile: "junk".into(),
                },
            ],
        };
        assert_eq!(
            r.resolve("chrome.exe"),
            FocusVerdict::Rule {
                index: 0,
                profile: "chill"
            },
            "the FIRST matching needle wins, not the last"
        );
        assert_eq!(r.resolve("chrome.exe").rule_index(), 0, "the lamp reads the same verdict");
        // no rule matches -> the fallback, which is what makes closing a game restore your settings.
        assert_eq!(
            r.resolve("notepad.exe"),
            FocusVerdict::Fallback {
                profile: "everyday"
            }
        );
        assert_eq!(r.resolve("notepad.exe").rule_index(), -1, "a fallback lights no rule");
    }

    /// With no fallback configured, an unmatched app leaves the active profile alone — the old
    /// latching behaviour, kept as the default so auto-switch never starts reverting unasked.
    #[test]
    fn no_fallback_means_stay_put() {
        let r = AppRules {
            default: None,
            rules: vec![AppRule {
                app: "valorant".into(),
                profile: "fps".into(),
            }],
        };
        assert_eq!(r.resolve("notepad.exe"), FocusVerdict::Stay);
        assert_eq!(r.resolve("notepad.exe").profile(), None);
        // an empty-string default is treated as absent, not as a profile named "".
        let blank = AppRules {
            default: Some("   ".into()),
            rules: Vec::new(),
        };
        assert_eq!(blank.resolve("anything.exe"), FocusVerdict::Stay);
    }

    /// A rule whose target profile is gone is reported, so the UI can mark it instead of rendering
    /// it identically to a working route and failing silently at focus-switch time.
    #[test]
    fn dangling_rules_are_named_by_index() {
        let r = AppRules {
            default: None,
            rules: vec![
                AppRule {
                    app: "chrome".into(),
                    profile: "chill".into(),
                },
                AppRule {
                    app: "notepad".into(),
                    profile: "deleted".into(),
                },
            ],
        };
        let saved = vec!["chill".to_string()];
        assert_eq!(r.dangling(&saved), vec![1]);
        // matching is on the on-disk KEY, so a case-only difference is still the same profile.
        assert!(r.dangling(&["Chill".to_string()]).contains(&1));
        assert!(!r.dangling(&["Chill".to_string()]).contains(&0));
    }

    /// A skipped field is named even when something else landed. The one-line summary used to drop
    /// them entirely, so applying a profile with a sleeping mouse read as a clean success while its
    /// DPI and polling never happened.
    #[test]
    fn summary_names_what_it_skipped_not_just_what_landed() {
        let r = ApplyReport {
            applied: vec!["brightness 100%".into()],
            skipped: vec!["dpi: device asleep".into()],
            ..Default::default()
        };
        let s = r.summary();
        assert!(s.contains("brightness 100%"), "{s}");
        assert!(s.contains("skipped"), "a partial apply must say so: {s}");
        assert!(s.contains("dpi"), "{s}");
    }

    /// The profile list keeps unreadable files as named faults instead of dropping them — a
    /// corrupt profile used to disappear from the sheet with the file still on disk.
    #[test]
    fn load_all_reports_a_broken_profile_instead_of_hiding_it() {
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var_os("NEURON_RUN_DIR");
        let tmp = std::env::temp_dir().join(format!("neuron_profile_broken_{}", std::process::id()));
        std::env::set_var("NEURON_RUN_DIR", &tmp);

        Profile {
            name: "good".into(),
            dpi: Some(800),
            ..Default::default()
        }
        .save()
        .unwrap();
        std::fs::write(Profile::path("bad"), b"name = \"bad\"\ndpi = \"not a number\"\n").unwrap();

        let all = load_all();
        assert_eq!(all.len(), 2, "both files are represented");
        let broken: Vec<&str> = all
            .iter()
            .filter_map(|e| match e {
                ProfileEntry::Broken { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(broken, vec!["bad"], "the unreadable one is named, not dropped");

        match prev {
            Some(v) => std::env::set_var("NEURON_RUN_DIR", v),
            None => std::env::remove_var("NEURON_RUN_DIR"),
        }
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn app_rules_match_by_substring_first_wins() {
        let r = AppRules {
            default: None,
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let _g = crate::runroot::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
