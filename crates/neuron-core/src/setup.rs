// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The whole authored setup as one document: [`dump`] reads it, [`apply`] makes the machine match
//! it. A section present in the document is authoritative for that part of the config; a section
//! left out is not touched. Applying is idempotent (a section already equal to disk is reported
//! unchanged and not rewritten) and all-or-nothing up front: every section is validated with the
//! same checks the individual verbs use before the first file is written.
//!
//! Sections: `rules` (the always-live binds), `cast`, `feel`, `apps` (auto-switch routes),
//! `bindings` (the legacy `bindings.toml`), `profiles` (each with its lighting and its own binds),
//! `badges`, `macros` (Python sources), `app` (preferences, secrets excluded), `gestures`.

use crate::authoring::{self, Issue, Refs, RuleStore};
use crate::bindings::Bindings;
use crate::cast::CastConfig;
use crate::engine::Rule;
use crate::feel::FeelConfig;
use crate::gesture::Vault;
use crate::manage::ProfileBundle;
use crate::profile::{AppRules, Profile};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The document format version `dump` writes and `apply` accepts.
pub const FORMAT: u32 = 1;

fn format_one() -> u32 {
    FORMAT
}

/// One device's badge as stored in `badges.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BadgeEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emblem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The document. Every section is optional; scalars come first so it serializes as TOML.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Setup {
    #[serde(default = "format_one")]
    pub format: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cast: Option<CastConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feel: Option<FeelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<AppRules>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bindings: Option<Bindings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profiles: Option<Vec<ProfileBundle>>,
    /// pid (4-digit hex) -> badge
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badges: Option<BTreeMap<String, BadgeEntry>>,
    /// macro name -> Python source
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macros: Option<BTreeMap<String, String>>,
    /// macro name -> the option values chosen for it (`{key: value}`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macro_options: Option<BTreeMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<toml::Table>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gestures: Option<Vault>,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            format: FORMAT,
            rules: None,
            cast: None,
            feel: None,
            apps: None,
            bindings: None,
            profiles: None,
            badges: None,
            macros: None,
            macro_options: None,
            app: None,
            gestures: None,
        }
    }
}

/// A part of the setup, for `--only` selection and reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Rules,
    Cast,
    Feel,
    Apps,
    Bindings,
    Profiles,
    Badges,
    Macros,
    App,
    Gestures,
}

impl Section {
    /// Every section `dump` includes by default (the gesture vault is opt-in: it is bulky
    /// recorded-stroke data, not authored intent).
    pub const DEFAULT: [Section; 9] = [
        Section::Rules,
        Section::Cast,
        Section::Feel,
        Section::Apps,
        Section::Bindings,
        Section::Profiles,
        Section::Badges,
        Section::Macros,
        Section::App,
    ];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Section::Rules => "rules",
            Section::Cast => "cast",
            Section::Feel => "feel",
            Section::Apps => "apps",
            Section::Bindings => "bindings",
            Section::Profiles => "profiles",
            Section::Badges => "badges",
            Section::Macros => "macros",
            Section::App => "app",
            Section::Gestures => "gestures",
        }
    }

    pub fn parse(s: &str) -> Result<Section, String> {
        Section::DEFAULT
            .iter()
            .chain([&Section::Gestures])
            .find(|x| x.name() == s.trim())
            .copied()
            .ok_or_else(|| {
                let names: Vec<&str> = Section::DEFAULT.iter().map(|x| x.name()).chain(["gestures"]).collect();
                format!("unknown section '{s}' (sections: {})", names.join(", "))
            })
    }
}

/// The user-set badges in `badges.toml`, keyed by 4-digit hex pid.
pub fn badges_table() -> BTreeMap<String, BadgeEntry> {
    let path = crate::runroot::run_root().join("badges.toml");
    let Ok(text) = std::fs::read_to_string(path) else { return BTreeMap::new() };
    #[derive(Deserialize, Default)]
    struct File {
        #[serde(default)]
        device: BTreeMap<String, BadgeEntry>,
    }
    toml::from_str::<File>(&text).map(|f| f.device).unwrap_or_default()
}

/// Read the setup from disk: only the requested sections.
pub fn dump(sections: &[Section]) -> Result<Setup, String> {
    let mut s = Setup::default();
    for sec in sections {
        match sec {
            Section::Rules => s.rules = Some(RuleStore::Gui.load()?),
            Section::Cast => s.cast = Some(CastConfig::load()),
            Section::Feel => s.feel = Some(FeelConfig::load()),
            Section::Apps => s.apps = Some(AppRules::load()),
            Section::Bindings => s.bindings = Some(Bindings::load()),
            Section::Profiles => {
                let mut out = Vec::new();
                for name in crate::profile::try_list().map_err(|e| e.to_string())? {
                    // a profile that won't parse is reported, never silently dropped from the dump
                    out.push(crate::manage::export_profile(&name).map_err(|e| format!("profile '{name}': {e}"))?);
                }
                s.profiles = Some(out);
            }
            Section::Badges => s.badges = Some(badges_table()),
            Section::Macros => {
                let mut m = BTreeMap::new();
                let mut opts = BTreeMap::new();
                for id in crate::macros::macro_host::list_macros() {
                    if let Some(src) = crate::macros::macro_host::load_macro(&id) {
                        m.insert(id.clone(), src);
                    }
                    let v = crate::macros::macro_host::macro_host().option_values(&id);
                    if v.as_object().is_some_and(|o| !o.is_empty()) {
                        opts.insert(id, v);
                    }
                }
                s.macros = Some(m);
                s.macro_options = Some(opts);
            }
            Section::App => {
                let mut t = crate::manage::app_table()?;
                for k in crate::manage::SECRET_PREF_KEYS {
                    t.remove(*k);
                }
                s.app = Some(t);
            }
            Section::Gestures => s.gestures = Some(load_vault()?),
        }
    }
    Ok(s)
}

fn load_vault() -> Result<Vault, String> {
    let p = Vault::path();
    if p.exists() {
        Vault::load_from(&p).map_err(|e| format!("{e:#}"))
    } else {
        Ok(Vault::default())
    }
}

/// Parse a setup document written by `dump`: TOML, or JSON when it starts with `{`.
pub fn parse(text: &str) -> Result<Setup, String> {
    let t = text.trim_start_matches('\u{feff}').trim();
    let doc: Setup = if t.starts_with('{') {
        serde_json::from_str(t).map_err(|e| format!("setup JSON: {e}"))?
    } else {
        toml::from_str(t).map_err(|e| format!("setup TOML: {e}"))?
    };
    if doc.format != FORMAT {
        return Err(format!("setup format {} is not supported (this build reads format {FORMAT})", doc.format));
    }
    Ok(doc)
}

/// Render a setup as TOML.
pub fn to_toml(s: &Setup) -> Result<String, String> {
    toml::to_string_pretty(s).map_err(|e| e.to_string())
}

/// What applying did (or, on a dry run, would do) to one section.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SectionReport {
    pub section: &'static str,
    /// `unchanged` | `updated` | `would-update` (dry run)
    pub status: &'static str,
    pub detail: String,
}

/// Options for [`apply`].
#[derive(Clone, Copy, Debug, Default)]
pub struct ApplyOpts {
    /// Validate and diff, write nothing.
    pub dry_run: bool,
    /// Also delete what the document does not list, within the sections it does contain
    /// (profiles, macros, badges, preference keys).
    pub prune: bool,
    /// Syntax-check macro sources with the Python runtime before writing.
    pub check_macros: bool,
    /// Treat a macro, profile or route target that resolves to nothing as an error. By default it
    /// is a warning, so a setup that already had a dangling name round-trips through dump and apply.
    pub strict_refs: bool,
}

/// The result of an apply: per-section reports plus any validation issues that stopped it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyReport {
    pub sections: Vec<SectionReport>,
    pub issues: Vec<Issue>,
    pub applied: bool,
}

fn json<T: Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

fn issue(out: &mut Vec<Issue>, at: &str, list: Vec<Issue>) {
    out.extend(list.into_iter().map(|i| Issue { message: format!("{at}: {}", i.message), ..i }));
}

/// Every problem with the document, before anything is written.
#[must_use]
pub fn validate(doc: &Setup) -> Vec<Issue> {
    let mut out = Vec::new();
    let refs = Refs {
        macros: Some(
            doc.macros
                .as_ref()
                .map_or_else(|| crate::macros::macro_host::list_macros().into_iter().collect(), |m| m.keys().cloned().collect::<BTreeSet<_>>()),
        ),
        profiles: Some(doc.profiles.as_ref().map_or_else(
            || crate::profile::list().into_iter().collect(),
            |p| p.iter().map(|b| b.profile.name.clone()).collect::<BTreeSet<_>>(),
        )),
    };
    if let Some(rules) = &doc.rules {
        for (i, r) in rules.iter().enumerate() {
            issue(&mut out, &format!("rules[{i}]"), authoring::check_rule(r, &refs));
        }
    }
    if let Some(cast) = &doc.cast {
        for (i, a) in cast.radial.iter().enumerate() {
            issue(&mut out, &format!("cast.radial[{i}]"), authoring::check_action(a, &refs));
        }
        for (i, a) in cast.hyper_radial.iter().enumerate() {
            issue(&mut out, &format!("cast.hyper_radial[{i}]"), authoring::check_action(a, &refs));
        }
        for (n, a) in &cast.gestures {
            issue(&mut out, &format!("cast.gestures.{n}"), authoring::check_action(a, &refs));
        }
        for rb in &cast.rhythm_actions {
            issue(&mut out, &format!("cast.rhythm_actions[{}]", rb.taps), authoring::check_action(&rb.action, &refs));
        }
        let mut probe = CastConfig { deadzone: cast.deadzone, ..CastConfig::default() };
        if let Err(e) = authoring::apply_cast_set(&mut probe, authoring::CastSet::Sectors(cast.sectors)) {
            out.push(Issue::error(format!("cast: {e}")));
        }
        if let Err(e) = authoring::apply_cast_set(&mut probe, authoring::CastSet::Assist(cast.assist)) {
            out.push(Issue::error(format!("cast: {e}")));
        }
        if let Err(e) = crate::feel::Phrase::parse(&cast.activation) {
            out.push(Issue::error(format!("cast.activation: {e}")));
        }
    }
    if let Some(feel) = &doc.feel {
        let mut probe = FeelConfig::default();
        for set in [
            crate::manage::FeelSet::HoldMs(feel.hold_ms),
            crate::manage::FeelSet::GapMs(feel.gap_ms),
            crate::manage::FeelSet::CoyoteMs(feel.coyote_ms),
        ] {
            if let Err(e) = crate::manage::apply_feel_set(&mut probe, set) {
                out.push(Issue::error(format!("feel: {e}")));
            }
        }
    }
    if let Some(apps) = &doc.apps {
        for (i, r) in apps.rules.iter().enumerate() {
            if r.app.trim().is_empty() {
                out.push(Issue::error(format!("apps.rules[{i}]: empty app needle")));
            }
            if !refs.profiles.as_ref().is_some_and(|p| p.iter().any(|n| Profile::file_key(n) == Profile::file_key(&r.profile))) {
                out.push(Issue::missing(format!("apps.rules[{i}]: no profile '{}'", r.profile)));
            }
        }
    }
    if let Some(profiles) = &doc.profiles {
        let mut seen = BTreeSet::new();
        for (i, b) in profiles.iter().enumerate() {
            let at = format!("profiles[{i}] '{}'", b.profile.name);
            if let Some(why) = crate::profile::name_conflict(&b.profile.name) {
                out.push(Issue::error(format!("{at}: {why}")));
            }
            if !seen.insert(Profile::file_key(&b.profile.name)) {
                out.push(Issue::error(format!("{at}: two profiles file under the same name")));
            }
            issue(&mut out, &format!("{at} lighting"), crate::layers::check_stack(&b.profile.lighting));
            for (j, r) in b.rules.iter().enumerate() {
                issue(&mut out, &format!("{at} rules[{j}]"), authoring::check_rule(r, &refs));
            }
        }
    }
    if let Some(badges) = &doc.badges {
        for (pid, b) in badges {
            if u16::from_str_radix(pid, 16).is_err() {
                out.push(Issue::error(format!("badges.{pid}: not a hex pid")));
            }
            if let Some(e) = &b.emblem {
                if crate::badge::Emblem::parse(e).is_none() {
                    out.push(Issue::error(format!("badges.{pid}: unknown emblem '{e}'")));
                }
            }
        }
    }
    if let Some(macros) = &doc.macros {
        for (id, src) in macros {
            if let Err(e) = crate::macros::macro_host::validate_macro_id(id) {
                out.push(Issue::error(format!("macros.{id}: {e}")));
            }
            if let Err(e) = crate::macros::mode_from_source(src) {
                out.push(Issue::error(format!("macros.{id}: {e}")));
            }
        }
    }
    if let Some(opts) = &doc.macro_options {
        for (id, v) in opts {
            if let Err(e) = crate::macros::macro_host::validate_macro_id(id) {
                out.push(Issue::error(format!("macro_options.{id}: {e}")));
            } else if !v.is_object() {
                out.push(Issue::error(format!("macro_options.{id}: option values are a {{key: value}} table")));
            }
        }
    }
    if let Some(app) = &doc.app {
        for (k, v) in app {
            if crate::manage::SECRET_PREF_KEYS.contains(&k.as_str()) {
                out.push(Issue::error(format!("app.{k}: secrets are not part of a setup document")));
            } else if k != "lighting" {
                let raw = match v {
                    toml::Value::String(s) => s.clone(),
                    toml::Value::Array(a) => a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(","),
                    toml::Value::Table(_) | toml::Value::Datetime(_) => {
                        out.push(Issue::error(format!("app.{k}: unsupported value")));
                        continue;
                    }
                    other => other.to_string(),
                };
                if let Err(e) = crate::manage::parse_pref_value(k, &raw) {
                    out.push(Issue::error(format!("app.{k}: {e}")));
                }
            } else if let Some(t) = v.as_table() {
                for (pid, dev) in t {
                    if let Some(layers) = dev.get("layers") {
                        match layers.clone().try_into::<Vec<crate::pattern::LayerDef>>() {
                            Ok(stack) => issue(&mut out, &format!("app.lighting.{pid}"), crate::layers::check_stack(&stack)),
                            Err(e) => out.push(Issue::error(format!("app.lighting.{pid}: {e}"))),
                        }
                    }
                }
            }
        }
    }
    out
}

/// Make the machine match `doc`. Validation runs first; any error stops the apply before a write.
pub fn apply(doc: &Setup, opts: ApplyOpts) -> Result<ApplyReport, String> {
    let issues = validate(doc)
        .into_iter()
        .map(|i| if opts.strict_refs { i } else { i.allowing_missing_refs() })
        .collect();
    let mut report = ApplyReport { issues, ..ApplyReport::default() };
    if authoring::has_errors(&report.issues) {
        return Ok(report);
    }
    if opts.check_macros {
        if let Some(macros) = &doc.macros {
            for (id, src) in macros {
                if let Err(e) = crate::macros::macro_host::macro_host().check(src) {
                    report.issues.push(Issue::error(format!("macros.{id}: {e}")));
                }
            }
            if authoring::has_errors(&report.issues) {
                return Ok(report);
            }
        }
    }
    let mut push = |section: &'static str, changed: bool, detail: String| {
        let status = match (changed, opts.dry_run) {
            (false, _) => "unchanged",
            (true, false) => "updated",
            (true, true) => "would-update",
        };
        report.sections.push(SectionReport { section, status, detail });
    };
    let write = !opts.dry_run;

    if let Some(rules) = &doc.rules {
        let cur = RuleStore::Gui.load()?;
        let changed = json(&cur) != json(rules);
        if changed && write {
            RuleStore::Gui.save(rules)?;
        }
        push("rules", changed, format!("{} rules", rules.len()));
    }
    if let Some(cast) = &doc.cast {
        let changed = json(&CastConfig::load()) != json(cast);
        if changed && write {
            authoring::save_cast(cast)?;
        }
        push("cast", changed, format!("{} wedges, {} glyph binds", cast.radial.len(), cast.gestures.len()));
    }
    if let Some(feel) = &doc.feel {
        let changed = json(&FeelConfig::load()) != json(feel);
        if changed && write {
            feel.save()?;
        }
        push("feel", changed, "timing windows and stance".into());
    }
    if let Some(bindings) = &doc.bindings {
        let changed = json(&Bindings::load()) != json(bindings);
        if changed && write {
            bindings.save()?;
        }
        push("bindings", changed, format!("{} legacy bindings", bindings.bindings.len()));
    }
    if let Some(gestures) = &doc.gestures {
        let changed = json(&load_vault()?) != json(gestures);
        if changed && write {
            gestures.save()?;
        }
        push("gestures", changed, format!("{} glyph templates", gestures.templates.len()));
    }
    if let Some(macros) = &doc.macros {
        let mut changed = 0;
        // reading the directory first lets the one-time authority migration see it BEFORE any new
        // file lands there (it stamps every file it finds on first sight as raw)
        let _ = crate::macros::macro_host::list_macros();
        for (id, src) in macros {
            if crate::macros::macro_host::load_macro(id).as_deref() != Some(src.as_str()) {
                changed += 1;
                if write {
                    crate::macros::macro_host::write_macro_file(id, src)?;
                }
            }
        }
        if opts.prune {
            for id in crate::macros::macro_host::list_macros() {
                if !macros.contains_key(&id) {
                    changed += 1;
                    if write {
                        crate::macros::macro_host::macro_host().delete(&id)?;
                    }
                }
            }
        }
        push("macros", changed > 0, format!("{changed} of {} macros differ", macros.len()));
    }
    if let Some(opts) = &doc.macro_options {
        let host = crate::macros::macro_host::macro_host();
        let mut changed = 0;
        for (id, v) in opts {
            if host.option_values(id) != *v {
                changed += 1;
                if write {
                    host.set_option_values(id, v)?;
                }
            }
        }
        push("macro_options", changed > 0, format!("{changed} of {} option sets differ", opts.len()));
    }
    if let Some(profiles) = &doc.profiles {
        let mut changed = 0;
        for b in profiles {
            let same = crate::manage::export_profile(&b.profile.name).is_ok_and(|cur| json(&cur) == json(b));
            if !same {
                changed += 1;
                if write {
                    crate::manage::save_bundle(b)?;
                }
            }
        }
        if opts.prune {
            let keep: BTreeSet<String> = profiles.iter().map(|b| Profile::file_key(&b.profile.name)).collect();
            for name in crate::profile::try_list().map_err(|e| e.to_string())? {
                if !keep.contains(&Profile::file_key(&name)) {
                    changed += 1;
                    if write {
                        crate::manage::delete_profile(&name)?;
                    }
                }
            }
        }
        push("profiles", changed > 0, format!("{changed} of {} profiles differ", profiles.len()));
    }
    // routes are written after profiles so a route never points at a profile that is not there yet
    if let Some(apps) = &doc.apps {
        let changed = json(&AppRules::load()) != json(apps);
        if changed && write {
            apps.save()?;
        }
        push("apps", changed, format!("{} routes", apps.rules.len()));
    }
    if let Some(badges) = &doc.badges {
        let cur = badges_table();
        let mut changed = 0;
        for (pid, b) in badges {
            if cur.get(pid) != Some(b) {
                changed += 1;
                if write {
                    let raw = u16::from_str_radix(pid, 16).map_err(|e| e.to_string())?;
                    let emblem = b.emblem.as_deref().and_then(crate::badge::Emblem::parse);
                    crate::badge::set(crate::registry::CanonicalPid::of(raw), emblem, b.name.as_deref())
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        if opts.prune {
            for pid in cur.keys().filter(|p| !badges.contains_key(*p)) {
                changed += 1;
                if write {
                    let raw = u16::from_str_radix(pid, 16).map_err(|e| e.to_string())?;
                    crate::badge::set(crate::registry::CanonicalPid::of(raw), None, None).map_err(|e| e.to_string())?;
                }
            }
        }
        push("badges", changed > 0, format!("{changed} of {} badges differ", badges.len()));
    }
    if let Some(app) = &doc.app {
        let mut cur = crate::manage::app_table()?;
        let mut next = cur.clone();
        for (k, v) in app {
            next.insert(k.clone(), v.clone());
        }
        if opts.prune {
            let keys: Vec<String> = next.keys().cloned().collect();
            for k in keys {
                if !app.contains_key(&k) && !crate::manage::SECRET_PREF_KEYS.contains(&k.as_str()) {
                    next.remove(&k);
                }
            }
        }
        // a dump omits secrets, so an apply must never delete or change one
        for k in crate::manage::SECRET_PREF_KEYS {
            if let Some(v) = cur.remove(*k) {
                next.insert((*k).to_string(), v);
            }
        }
        let changed = json(&crate::manage::app_table()?) != json(&next);
        if changed && write {
            crate::manage::save_app_table(&next)?;
        }
        push("app", changed, format!("{} preference keys", app.len()));
    }
    report.applied = write;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::Action;
    use crate::engine::Trigger;

    fn author_something() {
        // binds (base + hypershift), a wedge, a rhythm, a glyph bind, feel, a profile with lighting
        // and its own bind, a route, prefs, a badge and a macro file: one of everything.
        authoring::store_add_rule(
            &RuleStore::Gui,
            Trigger::Input { page: 9, usage: 4, pid: None },
            Action::Key { key: "f5".into() },
            None,
        )
        .unwrap();
        authoring::store_add_rule(
            &RuleStore::Gui,
            Trigger::Input { page: 9, usage: 5, pid: None },
            Action::Sequence { steps: authoring::parse_keyseq("w:180 ~90 a") },
            Some("hypershift".into()),
        )
        .unwrap();
        let mut cast = CastConfig::load();
        authoring::set_sector_action_on(&mut cast, 0, Action::Curtain, false).unwrap();
        authoring::set_rhythm_action(&mut cast, 2, Action::Echo).unwrap();
        authoring::set_gesture_action(&mut cast, "circle", Action::Lock).unwrap();
        let mut feel = FeelConfig::load();
        feel.hold_ms = 240;
        feel.save().unwrap();
        crate::manage::create_profile("game").unwrap();
        let mut b = crate::manage::export_profile("game").unwrap();
        b.profile.dpi = Some(800);
        b.profile.lighting = vec![crate::layers::build_layer(&crate::layers::LayerMods {
            preset: Some("fire".into()),
            ..Default::default()
        })
        .unwrap()];
        b.rules.push(Rule::new(Trigger::MicTap, Action::Echo));
        crate::manage::save_bundle(&b).unwrap();
        crate::manage::route_add("valorant", "game").unwrap();
        crate::manage::set_app_pref("notif_volume", "0.3").unwrap();
        crate::macros::macro_host::write_macro_file("hello", "def macro(ctx):\n    return 'hi'\n").unwrap();
    }

    #[test]
    fn dump_then_apply_into_a_fresh_root_reproduces_the_setup_and_is_idempotent() {
        let first = crate::authoring::test_run_root();
        author_something();
        let sections = [Section::DEFAULT.as_slice(), &[Section::Gestures]].concat();
        let dumped = dump(&sections).unwrap();
        let text = to_toml(&dumped).unwrap();
        let json_text = serde_json::to_string(&dumped).unwrap();
        drop(first);

        for doc_text in [text, json_text] {
            let _fresh = crate::authoring::test_run_root();
            let doc = parse(&doc_text).unwrap();
            let opts = ApplyOpts::default();
            let dry = apply(&doc, ApplyOpts { dry_run: true, ..opts }).unwrap();
            assert!(!dry.applied);
            assert!(dry.sections.iter().any(|s| s.status == "would-update"), "a fresh root differs: {dry:?}");
            assert!(crate::profile::list().is_empty(), "a dry run wrote nothing");

            let done = apply(&doc, opts).unwrap();
            assert!(done.applied && done.issues.is_empty(), "{done:?}");
            let again = dump(&sections).unwrap();
            assert_eq!(json(&again), json(&doc), "what was applied is exactly what the document said");
            let second = apply(&doc, opts).unwrap();
            assert!(
                second.sections.iter().all(|s| s.status == "unchanged"),
                "a second apply changes nothing: {second:?}"
            );
        }
    }

    #[test]
    fn validation_stops_the_apply_before_any_write() {
        let _r = crate::authoring::test_run_root();
        let mut doc = Setup::default();
        doc.rules = Some(vec![Rule::new(Trigger::MicTap, Action::Key { key: "not-a-key".into() })]);
        doc.feel = Some(FeelConfig { hold_ms: 0, ..FeelConfig::default() });
        doc.apps = Some(AppRules { default: None, rules: vec![crate::profile::AppRule { app: "x".into(), profile: "ghost".into() }] });
        doc.macros = Some([("bad name".to_string(), "x".to_string())].into());
        let rep = apply(&doc, ApplyOpts::default()).unwrap();
        assert!(!rep.applied && rep.sections.is_empty());
        assert!(rep.issues.len() >= 4, "{:?}", rep.issues);
        assert!(RuleStore::Gui.load().unwrap().is_empty(), "nothing was written");
    }

    #[test]
    fn a_rule_may_reference_a_macro_and_profile_the_same_document_defines() {
        let _r = crate::authoring::test_run_root();
        let mut doc = Setup::default();
        doc.macros = Some([("m".to_string(), "def macro(ctx):\n    pass\n".to_string())].into());
        doc.profiles = Some(vec![ProfileBundle { profile: Profile { name: "p".into(), ..Profile::default() }, rules: vec![] }]);
        doc.rules = Some(vec![
            Rule::new(
                Trigger::MicTap,
                Action::Script { script: crate::action::ScriptRef { id: "m".into(), kind: crate::action::ScriptKind::Python } },
            ),
            Rule::new(Trigger::Cast { taps: 3 }, Action::ProfileSwitch { name: "p".into() }),
        ]);
        let rep = apply(&doc, ApplyOpts::default()).unwrap();
        assert!(rep.applied && rep.issues.is_empty(), "{rep:?}");
    }

    #[test]
    fn prune_removes_what_the_document_omits_but_never_secrets() {
        let _r = crate::authoring::test_run_root();
        author_something();
        let mut t = crate::manage::app_table().unwrap();
        t.insert("host_obs_password".into(), toml::Value::String("s3cret".into()));
        crate::manage::save_app_table(&t).unwrap();
        let mut doc = dump(&Section::DEFAULT).unwrap();
        assert!(doc.app.as_ref().is_some_and(|a| !a.contains_key("host_obs_password")), "a dump never carries secrets");
        doc.profiles = Some(vec![]);
        doc.macros = Some(BTreeMap::new());
        doc.app = Some(toml::Table::new());
        doc.apps = Some(AppRules::default());
        let rep = apply(&doc, ApplyOpts { prune: true, ..Default::default() }).unwrap();
        assert!(rep.applied, "{rep:?}");
        assert!(crate::profile::list().is_empty());
        assert!(crate::macros::macro_host::list_macros().is_empty());
        let after = crate::manage::app_table().unwrap();
        assert!(after.get("notif_volume").is_none());
        assert_eq!(after["host_obs_password"].as_str(), Some("s3cret"), "the secret survives a prune");
    }

    #[test]
    fn a_dangling_name_round_trips_but_strict_mode_refuses_it() {
        let _r = crate::authoring::test_run_root();
        let mut doc = Setup::default();
        doc.rules = Some(vec![Rule::new(
            Trigger::MicTap,
            Action::Script { script: crate::action::ScriptRef { id: "ghost".into(), kind: crate::action::ScriptKind::Python } },
        )]);
        let lenient = apply(&doc, ApplyOpts { dry_run: true, ..Default::default() }).unwrap();
        assert!(lenient.issues.iter().all(|i| i.severity == crate::authoring::Severity::Warning), "{lenient:?}");
        assert_eq!(lenient.sections[0].status, "would-update");
        let strict = apply(&doc, ApplyOpts { strict_refs: true, ..Default::default() }).unwrap();
        assert!(!strict.applied && authoring::has_errors(&strict.issues));
    }

    #[test]
    fn unsupported_formats_and_junk_are_refused() {
        assert!(parse("format = 2").is_err());
        assert!(parse("{\"format\": 9}").is_err());
        assert!(parse("this is not toml [").is_err());
        assert!(Section::parse("nope").is_err());
        assert_eq!(Section::parse("cast").unwrap(), Section::Cast);
    }
}
