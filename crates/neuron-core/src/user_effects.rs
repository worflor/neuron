// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! User-saved effects — a named, tagged layer stack the user built and wants to reuse or share.
//!
//! A user effect is a [`UserEffect`] (name + tags + [`Vec<LayerDef>`]) persisted as one TOML file
//! per effect under `effects/` in the run root. The format is the same [`LayerDef`] serialization
//! the profile system already uses, so a saved effect round-trips through the same compositor
//! without any new rendering path.
//!
//! The module is deliberately small: it owns the directory, the file naming, and the
//! export/import bundle format. Validation reuses [`crate::layers::check_stack`]; the catalog
//! reuses [`crate::layers::catalog_json`].

use crate::pattern::LayerDef;
use serde::{Deserialize, Serialize};

/// A user-saved effect: a named, tagged stack of layers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserEffect {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub layers: Vec<LayerDef>,
}

/// The portable bundle uses the same document as the local store.
pub type UserEffectBundle = UserEffect;

/// The `effects/` directory in the run root.
#[must_use]
pub fn effects_dir() -> std::path::PathBuf {
    crate::runroot::run_root().join("effects")
}

/// The file path for a user effect by slug.
#[must_use]
pub(crate) fn effect_path(slug: &str) -> std::path::PathBuf {
    effects_dir().join(format!("{slug}.toml"))
}

fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() || slug.len() > 120 || slugify(slug) != slug {
        return Err("an effect slug needs 1–120 lowercase ASCII letters, digits or single hyphens".into());
    }
    let reserved = matches!(slug, "con" | "prn" | "aux" | "nul")
        || ["com", "lpt"].iter().any(|prefix| {
            slug.strip_prefix(prefix).is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        });
    if reserved {
        return Err("that effect name is reserved by Windows".into());
    }
    Ok(())
}

/// A saved effect, and whether saving it landed ON TOP of an existing one.
///
/// The overwrite is REPORTED, never silent: a name that slugifies onto an existing effect
/// ("My Look" saved twice) would otherwise quietly replace a look the user still expected to be there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub slug: String,
    /// A previous effect by this slug was replaced.
    pub replaced: bool,
}

/// Slugify a user effect name: lowercase, alphanumeric + hyphen, trimmed.
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_hyphen = false;
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_hyphen = false;
        } else if matches!(c, ' ' | '-' | '_') && !prev_hyphen && !out.is_empty() {
            out.push('-');
            prev_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Every saved effect's `(slug, name, tags)`, sorted by name.
///
/// ONE unreadable file costs only ITSELF: a hand-edited, truncated, or newer-than-this-build entry is
/// skipped rather than failing the listing, because the alternative is one bad file hiding every look
/// the user actually saved. A missing `effects/` directory is likewise "none saved", not an error —
/// the store is created on first save, so its absence is the normal fresh-install state.
pub fn list() -> Result<Vec<(String, String, Vec<String>)>, String> {
    let dir = effects_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(slug) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if validate_slug(slug).is_err() { continue; }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(effect) = toml::from_str::<UserEffect>(&text) else {
            continue;
        };
        if validate_layers(&effect.layers).is_err() { continue; }
        out.push((slug.to_string(), effect.name, effect.tags));
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(out)
}

/// Load a user effect by slug.
pub fn load(slug: &str) -> Result<UserEffect, String> {
    validate_slug(slug)?;
    let path = effect_path(slug);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("no effect '{slug}' ({e})"))?;
    let effect: UserEffect = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    validate_layers(&effect.layers)?;
    Ok(effect)
}

fn validate_layers(layers: &[LayerDef]) -> Result<(), String> {
    if layers.is_empty() { return Err("an effect needs at least one layer".into()); }
    let issues = crate::layers::check_stack(layers);
    if crate::authoring::has_errors(&issues) {
        return Err(issues.iter().map(|i| i.message.as_str()).collect::<Vec<_>>().join("; "));
    }
    Ok(())
}

/// Save a user effect under `name`, creating `effects/` if needed. Reports whether it landed on top of
/// an existing effect of the same slug, so a caller can say so instead of the user discovering a
/// vanished look later.
///
/// The layers are validated first: a saved effect is a look you'd want back EXACTLY as it was, so a
/// stack that wouldn't render is refused here rather than written out and failing at paint time.
pub fn save(name: &str, tags: &[String], layers: &[LayerDef]) -> Result<Saved, String> {
    let slug = slugify(name);
    if slug.is_empty() {
        return Err("an effect needs a name".into());
    }
    validate_slug(&slug)?;
    validate_layers(layers)?;
    let dir = effects_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = effect_path(&slug);
    let replaced = path.exists();
    let effect = UserEffect {
        name: name.trim().to_string(),
        tags: tags.iter().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect(),
        layers: layers.to_vec(),
    };
    let body = toml::to_string_pretty(&effect).map_err(|e| e.to_string())?;
    crate::salvage::atomic_write(&path, body.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Saved { slug, replaced })
}

/// Delete a user effect by slug.
pub fn delete(slug: &str) -> Result<(), String> {
    validate_slug(slug)?;
    let path = effect_path(slug);
    if !path.exists() {
        return Err(format!("no saved effect '{slug}'"));
    }
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Export a user effect as a portable TOML bundle string.
pub fn export_bundle(slug: &str) -> Result<String, String> {
    let effect = load(slug)?;
    toml::to_string_pretty(&effect).map_err(|e| e.to_string())
}

/// Import a user effect from a TOML bundle string.
pub fn import_bundle(text: &str) -> Result<Saved, String> {
    let bundle: UserEffectBundle = toml::from_str(text).map_err(|e| format!("not a saved-effect bundle: {e}"))?;
    let name = bundle.name.trim();
    if name.is_empty() {
        return Err("the bundle has no name".into());
    }
    save(name, &bundle.tags, &bundle.layers)
}

/// Import a user effect from a file path.
pub fn import_from_file(path: &std::path::Path) -> Result<Saved, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    import_bundle(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effects::Blend;
    use crate::lighting::Rgb;
    use crate::spectrum::Spectrum;

    fn layer(pattern: &str, rgb: (u8, u8, u8), blend: Blend) -> LayerDef {
        LayerDef {
            pattern: pattern.into(),
            spectrum: Spectrum::solid(Rgb::new(rgb.0, rgb.1, rgb.2)),
            blend,
            ..LayerDef::default()
        }
    }

    fn stack() -> Vec<LayerDef> {
        vec![
            layer("comet", (0x00, 0x66, 0xFF), Blend::Add),
            layer("heat", (0xFF, 0x00, 0x00), Blend::Normal),
        ]
    }

    #[test]
    fn slugify_normalizes_names() {
        assert_eq!(slugify("Neon Tunnel"), "neon-tunnel");
        assert_eq!(slugify("  Fire Storm  "), "fire-storm");
        assert_eq!(slugify("Aurora Borealis!"), "aurora-borealis");
        assert_eq!(slugify("My_Look"), "my-look");
        assert_eq!(slugify("---"), "", "a name with nothing nameable in it has no slug");
    }

    #[test]
    fn save_load_delete_round_trip() {
        let _r = crate::authoring::test_run_root();
        let layers = stack();
        let saved = save("Neon Tunnel", &["ambient".into(), "  ".into(), "blue".into()], &layers).unwrap();
        assert_eq!(saved.slug, "neon-tunnel");
        assert!(!saved.replaced, "a first save replaces nothing");

        let loaded = load(&saved.slug).unwrap();
        assert_eq!(loaded.name, "Neon Tunnel");
        assert_eq!(loaded.tags, vec!["ambient", "blue"], "a blank tag is dropped, not stored");
        assert_eq!(loaded.layers, layers, "the stack comes back byte-for-byte");

        let listed = list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], (saved.slug.clone(), "Neon Tunnel".into(), vec!["ambient".into(), "blue".into()]));

        delete(&saved.slug).unwrap();
        assert!(load(&saved.slug).is_err());
        assert!(list().unwrap().is_empty());
        assert!(delete(&saved.slug).is_err(), "deleting it twice is an error, not a silent no-op");
    }

    /// The whole point of the store: a look the user dialed in comes back IDENTICAL, so the compositor
    /// that renders a saved effect renders the board the same way the live stack did.
    #[test]
    fn a_saved_effect_renders_exactly_as_the_stack_it_came_from() {
        let _r = crate::authoring::test_run_root();
        let layers = stack();
        let saved = save("Round Trip", &[], &layers).unwrap();
        let back = load(&saved.slug).unwrap().layers;
        assert_eq!(back, layers);
        for t in [0.0_f32, 1.7, 12.25] {
            let (rows, cols) = (6_u8, 22_u8);
            assert_eq!(
                crate::pattern::Compositor::from_defs(&layers).render(rows, cols, t),
                crate::pattern::Compositor::from_defs(&back).render(rows, cols, t),
                "the saved stack must render identically at t={t}"
            );
        }
    }

    /// A name that lands on an existing effect REPLACES it — and says so, so a caller can tell the user
    /// rather than letting a look vanish unnoticed.
    #[test]
    fn saving_the_same_name_again_reports_the_replacement() {
        let _r = crate::authoring::test_run_root();
        let first = save("Shared Name", &[], &stack()).unwrap();
        assert!(!first.replaced);
        let second = save("Shared Name", &[], &[layer("uniform", (1, 2, 3), Blend::Normal)]).unwrap();
        assert!(second.replaced, "a second save under the same name must report the overwrite");
        assert_eq!(load(&first.slug).unwrap().layers.len(), 1, "the replaced effect is the new one");
    }

    /// A stack that wouldn't render is refused AT SAVE TIME, not written out to fail at paint time.
    #[test]
    fn a_broken_stack_is_refused_and_nothing_is_written() {
        let _r = crate::authoring::test_run_root();
        let bad = vec![LayerDef { pattern: "no-such-pattern".into(), ..LayerDef::default() }];
        assert!(save("Broken", &[], &bad).is_err());
        assert!(!effect_path("broken").exists(), "a refused save must leave no file behind");
        assert!(list().unwrap().is_empty());
        assert!(save("   ", &[], &stack()).is_err(), "an unnameable effect is refused");
    }

    /// ONE unreadable file costs only itself. The alternative — failing the whole listing — would let a
    /// single hand-edited or truncated file hide every look the user actually saved.
    #[test]
    fn one_unreadable_file_does_not_hide_the_others() {
        let _r = crate::authoring::test_run_root();
        let good = save("Good One", &[], &stack()).unwrap();
        let other = save("Other One", &[], &stack()).unwrap();
        std::fs::write(effect_path("corrupt"), "this is not toml =").unwrap();
        std::fs::write(effects_dir().join("notes.txt"), "ignored: not an effect").unwrap();

        let listed: Vec<String> = list().unwrap().into_iter().map(|(s, _, _)| s).collect();
        assert_eq!(listed, vec![good.slug.clone(), other.slug], "sorted by name; the bad file is skipped");

        // A file that won't parse still READS as absent (so the tile never claims to be a look you can
        // load)… but it is still DELETABLE, because removing the broken file is exactly how a user
        // recovers from one. A delete that refused here would strand the bad file forever.
        assert!(load("corrupt").is_err());
        delete("corrupt").unwrap();
        assert!(!effect_path("corrupt").exists());
        assert_eq!(list().unwrap().len(), 2, "removing the bad file leaves the good ones alone");
    }

    /// A fresh install has no `effects/` at all — the store is created on first save — so listing must
    /// read that as "none saved", not as a read failure the GUI would surface as an error.
    #[test]
    fn an_absent_store_lists_as_empty_rather_than_failing() {
        let _r = crate::authoring::test_run_root();
        assert!(!effects_dir().exists());
        assert_eq!(list().unwrap(), Vec::<(String, String, Vec<String>)>::new());
    }

    #[test]
    fn export_import_bundle_round_trips_across_machines() {
        let _r = crate::authoring::test_run_root();
        let layers = stack();
        save("Shared Look", &["shared".into()], &layers).unwrap();

        let exported = export_bundle("shared-look").unwrap();
        // The export is a plain, readable TOML document — not a blob — so a person can read, diff and
        // hand-edit what they are sharing.
        assert!(exported.contains("name = \"Shared Look\""));
        assert!(exported.contains("[[layers]]"));
        delete("shared-look").unwrap();

        let landed = import_bundle(&exported).unwrap();
        assert_eq!(landed.slug, "shared-look");
        assert!(!landed.replaced, "importing into an empty store replaces nothing");
        let back = load(&landed.slug).unwrap();
        assert_eq!(back.name, "Shared Look");
        assert_eq!(back.tags, vec!["shared"]);
        assert_eq!(back.layers, layers);
        assert!(export_bundle("no-such-effect").is_err());
    }

    #[test]
    fn a_bundle_that_is_not_one_is_refused() {
        assert!(import_bundle("nonsense").is_err());
        assert!(import_bundle("layers = []").is_err(), "a nameless bundle is not a look");
        assert!(import_bundle("name = \"\"\nlayers = []\n").is_err());
    }

    #[test]
    fn unsafe_names_and_invalid_loaded_stacks_are_refused() {
        let _r = crate::authoring::test_run_root();
        for slug in ["../outside", "..\\outside", "C:/outside", "", "con", "com1", "lpt9"] {
            assert!(load(slug).is_err(), "{slug}");
            assert!(delete(slug).is_err(), "{slug}");
        }
        assert!(save("CON", &[], &stack()).is_err());
        assert!(save("Empty", &[], &[]).is_err());
        std::fs::create_dir_all(effects_dir()).unwrap();
        let invalid = UserEffect { name: "Invalid".into(), tags: vec![], layers: vec![layer("unknown", (1, 2, 3), Blend::Normal)] };
        std::fs::write(effect_path("invalid"), toml::to_string(&invalid).unwrap()).unwrap();
        assert!(load("invalid").is_err());
        delete("invalid").unwrap();
    }
}
