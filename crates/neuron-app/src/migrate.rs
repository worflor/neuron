//! Synapse import bridge — drives `neuron::import::import_export` and turns the result into a
//! preview the wizard renders, plus the apply step that writes the normalized config.
//!
//! `neuron-core`'s importer is the authority on the (plaintext, no-crypto) Synapse export format.
//! This calls `import::import_export` and maps `Imported { profile, rules, notes }` into the view;
//! on any importer error `parse` surfaces the engine's own message verbatim — transparent, not faked.

use neuron::import::{self, Imported};
use std::path::Path;

/// One preview line for the wizard. `category` groups the preview spatially:
/// "perf" (scalar settings), "lighting", "binding" (trigger→action), "note" (not migrated).
pub struct PreviewLine {
    pub label: String,
    pub detail: String,
    pub ok: bool,
    pub category: &'static str,
}

pub struct ParseResult {
    pub status: String,
    pub lines: Vec<PreviewLine>,
    pub ready: bool,
    pub imported: Option<Imported>,
}

/// Parse a Synapse export into a preview. On the engine error path (importer not yet wired) we
/// report it honestly rather than fabricate a migration.
pub fn parse(path: &str) -> ParseResult {
    let p = Path::new(path);
    if path.trim().is_empty() {
        return ParseResult {
            status: "No file selected.".into(),
            lines: Vec::new(),
            ready: false,
            imported: None,
        };
    }
    if !p.exists() {
        return ParseResult {
            status: format!("File not found: {path}"),
            lines: Vec::new(),
            ready: false,
            imported: None,
        };
    }
    match import::import_export(p) {
        Ok(imp) => {
            let mut lines = Vec::new();
            let s = &imp.profile;
            if let Some(d) = s.dpi {
                lines.push(PreviewLine {
                    label: "DPI".into(),
                    detail: d.to_string(),
                    ok: true,
                    category: "perf",
                });
            }
            if !s.dpi_stages.is_empty() {
                let stg: Vec<String> = s.dpi_stages.iter().map(|x| x.to_string()).collect();
                lines.push(PreviewLine {
                    label: "STAGES".into(),
                    detail: stg.join("/"),
                    ok: true,
                    category: "perf",
                });
            }
            if let Some(hz) = s.polling_hz {
                lines.push(PreviewLine {
                    label: "POLL".into(),
                    detail: format!("{hz} Hz"),
                    ok: true,
                    category: "perf",
                });
            }
            if let Some(b) = s.brightness {
                lines.push(PreviewLine {
                    label: "LIGHT".into(),
                    detail: format!("{b}%"),
                    ok: true,
                    category: "perf",
                });
            }
            if !s.lighting.is_empty() {
                // lighting is the compositor STACK now — the ONE label (custom / preset name / N fx).
                lines.push(PreviewLine {
                    label: "EFFECT".into(),
                    detail: s.lighting_label(),
                    ok: true,
                    category: "lighting",
                });
            }
            for r in &imp.rules {
                lines.push(PreviewLine {
                    label: r.trigger.describe(),
                    detail: r.action.describe(),
                    ok: true,
                    category: "binding",
                });
            }
            for n in &imp.notes {
                lines.push(PreviewLine {
                    label: "note".into(),
                    detail: n.clone(),
                    ok: false,
                    category: "note",
                });
            }
            let status = format!(
                "Parsed: {} setting(s), {} binding(s). Review and apply.",
                lines
                    .iter()
                    .filter(|l| l.ok && l.category != "binding")
                    .count(),
                imp.rules.len()
            );
            ParseResult {
                status,
                lines,
                ready: true,
                imported: Some(imp),
            }
        }
        Err(e) => ParseResult {
            status: format!("Import unavailable: {e}"),
            lines: Vec::new(),
            ready: false,
            imported: None,
        },
    }
}

/// Apply a parsed import: save the profile and write the imported spine rules to a sidecar next to
/// it (the same `<name>.rules.toml` the CLI writes), so the run-daemon can load the migrated binds.
/// Success/failure is structural: `Ok` consumes the one-time flow ("nothing to import" is terminal
/// too — retrying cannot help), `Err` keeps the wizard armed so the user can retry.
///
/// NON-TRANSACTIONAL BY DESIGN: the profile and its rules sidecar are two separate files, written in
/// turn. Each write is atomic ON ITS OWN (temp + rename — `Profile::save` and `save_rules_sidecar`
/// both go through `neuron::salvage::atomic_write`), so neither file is ever left torn or truncated.
/// But if the sidecar write fails after the profile is saved, the profile persists and `apply`
/// returns `Err` — deliberately: the wizard stays armed and re-applying is IDEMPOTENT (both files
/// rewrite to the same resolved names), so one retry fully reconciles. Cross-file all-or-nothing
/// would need a write-ahead journal, unwarranted for a one-shot, retryable import.
pub fn apply(imp: &Imported) -> Result<String, String> {
    let mut wrote = Vec::new();
    // Resolve the name once so the profile and its rules sidecar agree — the SAME resolution
    // every importer must use (see `Profile::de_collide_import_name`), so a blank name can't land
    // differently depending on which front door (GUI wizard vs CLI) applied the import, and a name
    // that merely SANITIZES to an existing profile's file key gets de-collided instead of silently
    // clobbering that profile (and its rules sidecar).
    let name = neuron::profile::Profile::de_collide_import_name(&imp.profile.name)?;
    // Lighting rides IN the profile now (the `lighting` layer stack) — there's no separate frame to
    // persist, so a plain `save()` writes everything in one step.
    if !imp.profile.is_empty() {
        let mut p = imp.profile.clone();
        p.name = name.clone();
        match p.save() {
            Ok(_) => wrote.push(format!("profile '{}'", p.name)),
            Err(e) => return Err(format!("profile save failed: {e}")),
        }
    }
    if !imp.rules.is_empty() {
        match save_rules_sidecar(&name, &imp.rules) {
            Ok(path) => wrote.push(format!("{} binding(s) -> {}", imp.rules.len(), path)),
            Err(e) => return Err(format!("rules save failed: {e}")),
        }
    }
    if wrote.is_empty() {
        return Ok("nothing to import (empty profile, no binds)".into());
    }
    Ok(format!("imported {}", wrote.join(", ")))
}

/// Write the imported rules to the sidecar PAIRED with the profile (`<sanitized-name>.rules.toml`,
/// flat in `profiles/`). Returns the path written.
fn save_rules_sidecar(name: &str, rules: &[neuron::engine::Rule]) -> Result<String, String> {
    use neuron::engine::RuleDoc;
    std::fs::create_dir_all(neuron::profile::profiles_dir()).map_err(|e| e.to_string())?;
    // Derive from the profile's canonical path (same sanitization) so the sidecar can never diverge
    // from `<name>.toml` — a raw-name join broke on any name with a path separator.
    let path = neuron::profile::Profile::rules_path(name);
    let doc = RuleDoc {
        rules: rules.to_vec(),
    };
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    neuron::salvage::atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty path is reported honestly (no parse, not ready) — never fabricated.
    #[test]
    fn empty_path_is_not_ready() {
        let r = parse("");
        assert!(!r.ready);
        assert!(r.imported.is_none());
        assert!(r.lines.is_empty());
        assert!(r.status.contains("No file"));
    }

    /// A missing file is reported as not-found, transparently.
    #[test]
    fn missing_file_is_reported() {
        let r = parse("Z:/definitely/not/here.synapse3");
        assert!(!r.ready);
        assert!(r.imported.is_none());
        assert!(r.status.contains("not found"));
    }

    /// A whitespace-only path must hit the SAME "no file selected" exit as a literal empty string —
    /// pins that the guard is `path.trim().is_empty()`, not a bare `path.is_empty()` (which would
    /// let "   " fall through to `Path::new("   ")` and produce a confusing not-found instead).
    #[test]
    fn whitespace_only_path_is_not_ready() {
        let r = parse("   ");
        assert!(!r.ready);
        assert!(r.imported.is_none());
        assert!(r.status.contains("No file"));
    }

    /// A path that exists but isn't a readable file (a directory) must degrade to the honest
    /// importer-error branch, not panic and not be silently treated as "not found" (a directory
    /// passes `Path::exists()`, so it takes a different path through `parse` than a missing file).
    #[test]
    fn existing_directory_is_reported_as_an_import_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!(
            "neuron_migrate_test_dir_{}_{}",
            std::process::id(),
            line!()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let r = parse(dir.to_str().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!r.ready);
        assert!(r.imported.is_none());
        assert!(
            r.status.starts_with("Import unavailable:"),
            "status: {}",
            r.status
        );
    }

    /// A file that exists and is readable but isn't a ZIP (every real Synapse export is a ZIP,
    /// however named) must surface the importer's own error text rather than fabricate a parse —
    /// the "transparent, not faked" contract this module's docstring promises.
    #[test]
    fn non_zip_file_reports_the_importer_error_verbatim() {
        let path = std::env::temp_dir().join(format!(
            "neuron_migrate_test_junk_{}_{}.synapse3",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, b"not a zip, just plain bytes").unwrap();
        let r = parse(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);
        assert!(!r.ready);
        assert!(r.imported.is_none());
        assert!(
            r.status.starts_with("Import unavailable:"),
            "status: {}",
            r.status
        );
    }

    /// `apply` on a fully-empty `Imported` (no profile settings, no rules) must take the
    /// "nothing to import" exit WITHOUT touching disk at all — pins that the emptiness guards
    /// really skip both writes rather than writing an empty profile/sidecar and reporting a nice
    /// message afterward.
    #[test]
    fn apply_on_default_imported_is_a_pure_noop_that_writes_nothing() {
        let _cwd = crate::testsupport::cwd_guard("migrate_apply_empty");
        let imp = Imported::default();
        let result = apply(&imp);
        assert_eq!(
            result,
            Ok("nothing to import (empty profile, no binds)".to_string())
        );
        assert!(
            !neuron::profile::profiles_dir().exists(),
            "apply must not create the profiles dir when there is nothing to import"
        );
    }

    /// A blank/whitespace profile name falls back to "imported", and that SAME resolved name must
    /// be used for BOTH the profile file and its `.rules.toml` sidecar — the daemon's loader pairs
    /// them by name, so a mismatch (e.g. profile saved as "imported" but rules saved under the
    /// original blank name) would silently orphan the imported binds.
    #[test]
    fn apply_writes_profile_and_rules_sidecar_under_the_same_resolved_name() {
        let _cwd = crate::testsupport::cwd_guard("migrate_apply_full");
        let mut profile = neuron::profile::Profile::default();
        profile.name = "   ".to_string(); // blank -> must fall back to "imported"
        profile.dpi = Some(1600);
        let rule = neuron::engine::Rule::new(
            neuron::engine::Trigger::Input {
                page: 0x07,
                usage: 0x04,
                pid: None,
            },
            neuron::action::Action::Key {
                key: "a".to_string(),
            },
        );
        let imp = Imported {
            profile,
            rules: vec![rule],
            notes: vec![],
        };

        let result = apply(&imp).expect("apply should succeed");
        assert!(result.contains("profile 'imported'"), "message: {result}");
        assert!(result.contains("1 binding(s)"), "message: {result}");

        let profile_path = neuron::profile::Profile::path("imported");
        assert!(
            profile_path.exists(),
            "profile file must be written under the resolved name"
        );
        let saved = std::fs::read_to_string(&profile_path).unwrap();
        assert!(
            saved.contains("dpi = 1600"),
            "saved profile must carry the imported dpi field: {saved}"
        );

        let rules_path = neuron::profile::profiles_dir().join("imported.rules.toml");
        assert!(
            rules_path.exists(),
            "rules sidecar must be written next to the profile, under the SAME resolved name"
        );
        let rules_body = std::fs::read_to_string(&rules_path).unwrap();
        assert!(
            rules_body.contains("kind = \"input\"") && rules_body.contains("key = \"a\""),
            "rules sidecar must carry the imported bind losslessly: {rules_body}"
        );
    }

    /// REGRESSION (codex/gpt-5.6-terra): a Synapse profile name carries straight from XML, so it can
    /// contain a path separator. The profile saves as `<sanitized>.toml`, and the sidecar MUST pair
    /// with it (`<sanitized>.rules.toml`, flat). The old code interpolated the RAW name, aiming the
    /// sidecar at a nonexistent nested dir (`profiles/FPS/competitive.rules.toml`) so `apply` FAILED
    /// after the profile had already been saved — a user-visible partial import.
    #[test]
    fn apply_sanitizes_a_slashed_name_for_both_profile_and_sidecar() {
        let _cwd = crate::testsupport::cwd_guard("migrate_apply_slashed");
        let mut profile = neuron::profile::Profile::default();
        profile.name = "FPS/competitive".to_string();
        profile.dpi = Some(800);
        let rule = neuron::engine::Rule::new(
            neuron::engine::Trigger::Input {
                page: 0x07,
                usage: 0x04,
                pid: None,
            },
            neuron::action::Action::Key {
                key: "b".to_string(),
            },
        );
        let imp = Imported {
            profile,
            rules: vec![rule],
            notes: vec![],
        };

        // must SUCCEED — the old raw-name join failed here on the missing nested dir.
        let result = apply(&imp).expect("apply must succeed for a name with a separator");
        assert!(result.contains("1 binding(s)"), "message: {result}");

        let dir = neuron::profile::profiles_dir();
        assert!(
            dir.join("FPS_competitive.toml").exists(),
            "profile written at the sanitized flat path"
        );
        assert!(
            dir.join("FPS_competitive.rules.toml").exists(),
            "sidecar paired with the sanitized profile stem, flat beside it"
        );
        assert!(
            !dir.join("FPS").exists(),
            "the raw name must never create a nested `profiles/FPS/` dir"
        );
    }
}
