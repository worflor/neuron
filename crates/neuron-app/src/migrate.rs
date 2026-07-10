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
pub fn apply(imp: &Imported) -> Result<String, String> {
    let mut wrote = Vec::new();
    // Resolve the name once so the profile and its rules sidecar agree.
    let name = {
        let n = imp.profile.name.trim();
        if n.is_empty() {
            "imported".to_string()
        } else {
            n.to_string()
        }
    };
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

/// Write the imported rules to `profiles/<name>.rules.toml`. Returns the path written.
fn save_rules_sidecar(name: &str, rules: &[neuron::engine::Rule]) -> Result<String, String> {
    use neuron::engine::RuleDoc;
    let dir = neuron::profile::profiles_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{name}.rules.toml"));
    let doc = RuleDoc {
        rules: rules.to_vec(),
    };
    let body = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
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
}
