// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Managing the rest of the authored state: profiles and their binds, auto-switch routes, the feel
//! timing windows, and the GUI's `app.toml` preferences. Each function validates before it writes,
//! writes atomically, and reports what changed, so the CLI (and any other client) shares one
//! behaviour with the GUI's own editors.

use crate::engine::Rule;
use crate::profile::{AppRule, AppRules, Profile};
use serde::{Deserialize, Serialize};

// ── PROFILES ────────────────────────────────────────────────────────────────────────────────

/// The settings a profile can carry, as `(key, value grammar)` for `neuron profile set`.
pub const PROFILE_FIELDS: &[(&str, &str)] = &[
    ("dpi", "100-30000 | unset"),
    ("dpi-stages", "800,1600,3200 | unset"),
    ("polling", "125 | 250 | 500 | 1000 | unset"),
    ("brightness", "0-100 | unset"),
    ("idle-secs", "seconds | unset"),
    ("in-game-polling", "WIRED,DONGLE hz | unset"),
    ("disable-alt-tab", "true | false"),
    ("disable-win", "true | false"),
    ("disable-alt-f4", "true | false"),
    ("disable-alt-esc", "true | false"),
    ("persist", "true | false"),
];

fn parse_bool(v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "yes" | "1" => Ok(true),
        "false" | "off" | "no" | "0" => Ok(false),
        other => Err(format!("'{other}' is not a boolean (true | false)")),
    }
}

fn is_unset(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "unset" | "none" | "null" | "")
}

fn parse_dpi(v: &str) -> Result<u16, String> {
    match v.trim().parse::<u16>() {
        Ok(d) if (100..=30_000).contains(&d) => Ok(d),
        _ => Err(format!("dpi '{v}' must be 100-30000")),
    }
}

fn parse_polling(v: &str) -> Result<u32, String> {
    match v.trim().parse::<u32>() {
        Ok(h @ (125 | 250 | 500 | 1000)) => Ok(h),
        _ => Err(format!("polling '{v}' must be 125, 250, 500 or 1000 Hz")),
    }
}

/// Set one profile field from text. `unset` clears an optional setting. The value ranges match the
/// device verbs (DPI 100-30000, polling 125/250/500/1000, brightness 0-100).
pub fn set_profile_field(p: &mut Profile, key: &str, value: &str) -> Result<(), String> {
    let unset = is_unset(value);
    match key {
        "dpi" => p.dpi = if unset { None } else { Some(parse_dpi(value)?) },
        "dpi-stages" => {
            p.dpi_stages = if unset {
                Vec::new()
            } else {
                let stages = value.split(',').map(parse_dpi).collect::<Result<Vec<_>, _>>()?;
                if stages.len() > 5 {
                    return Err("a DPI stage list holds at most 5 stages".into());
                }
                stages
            };
        }
        "polling" => p.polling_hz = if unset { None } else { Some(parse_polling(value)?) },
        "brightness" => {
            p.brightness = if unset {
                None
            } else {
                match value.trim().parse::<u8>() {
                    Ok(b) if b <= 100 => Some(b),
                    _ => return Err(format!("brightness '{value}' must be 0-100")),
                }
            };
        }
        "idle-secs" => {
            p.idle_secs = if unset {
                None
            } else {
                Some(value.trim().parse::<u32>().map_err(|_| format!("idle-secs '{value}' must be a number of seconds"))?)
            };
        }
        "in-game-polling" => {
            p.in_game_polling = if unset {
                None
            } else {
                let (w, d) = value
                    .split_once(',')
                    .ok_or_else(|| "in-game-polling is WIRED,DONGLE, e.g. 1000,500".to_string())?;
                Some((parse_polling(w)?, parse_polling(d)?))
            };
        }
        "disable-alt-tab" => p.disable_alt_tab = parse_bool(value)?,
        "disable-win" => p.disable_win = parse_bool(value)?,
        "disable-alt-f4" => p.disable_alt_f4 = parse_bool(value)?,
        "disable-alt-esc" => p.disable_alt_esc = parse_bool(value)?,
        "persist" => p.persist = parse_bool(value)?,
        other => {
            let keys: Vec<&str> = PROFILE_FIELDS.iter().map(|f| f.0).collect();
            return Err(format!("unknown profile field '{other}' (fields: {})", keys.join(", ")));
        }
    }
    Ok(())
}

/// Create an empty profile. Errors on a reserved/invalid name or one that already exists.
pub fn create_profile(name: &str) -> Result<Profile, String> {
    if let Some(why) = crate::profile::name_conflict(name) {
        return Err(why);
    }
    if Profile::path(name).exists() {
        return Err(format!("profile '{name}' already exists"));
    }
    let p = Profile { name: name.trim().to_string(), ..Profile::default() };
    p.save()?;
    Ok(p)
}

/// A profile and its own binds as one portable document (`[profile]` + `[[rules]]`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileBundle {
    pub profile: Profile,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

/// Read a profile and its rules sidecar.
pub fn export_profile(name: &str) -> Result<ProfileBundle, String> {
    let profile = Profile::load(name).map_err(|e| e.to_string())?;
    let rules = crate::authoring::RuleStore::Profile(name.to_string()).load()?;
    Ok(ProfileBundle { profile, rules })
}

/// Write a bundle as a profile. An existing profile of that name is replaced only with `replace`.
/// Returns the name it landed under.
pub fn import_bundle(b: &ProfileBundle, replace: bool) -> Result<String, String> {
    let name = b.profile.name.trim();
    if let Some(why) = crate::profile::name_conflict(name) {
        return Err(why);
    }
    if Profile::path(name).exists() && !replace {
        return Err(format!("profile '{name}' already exists (pass --replace to overwrite it)"));
    }
    save_bundle(b)?;
    Ok(name.to_string())
}

/// Save a bundle unconditionally (the caller has decided overwriting is fine).
pub fn save_bundle(b: &ProfileBundle) -> Result<(), String> {
    b.profile.save()?;
    let name = &b.profile.name;
    if b.rules.is_empty() {
        // an empty bundle must not leave a previous sidecar's binds behind
        let path = Profile::rules_path(name);
        if path.exists() {
            crate::authoring::RuleStore::Profile(name.clone()).save(&[])?;
        }
        return Ok(());
    }
    crate::authoring::RuleStore::Profile(name.clone()).save(&b.rules)
}

/// What a rename did beyond moving the files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RenameReport {
    pub landed: String,
    pub routes_followed: usize,
}

/// Rename a profile with its binds; auto-switch routes and the fallback that named it follow. If
/// `apps.toml` can't be written the rename is rolled back, so no route is left dangling.
pub fn rename_profile(from: &str, to: &str) -> Result<RenameReport, String> {
    let landed = Profile::rename(from, to)?;
    let mut rules = AppRules::load();
    let mut moved = 0;
    for r in rules.rules.iter_mut().filter(|r| r.profile == from) {
        r.profile.clone_from(&landed);
        moved += 1;
    }
    if rules.default.as_deref() == Some(from) {
        rules.default = Some(landed.clone());
        moved += 1;
    }
    if moved > 0 {
        if let Err(e) = rules.save() {
            return match Profile::rename(&landed, from) {
                Ok(_) => Err(format!("rename rolled back; apps.toml could not be written: {e}")),
                Err(re) => Err(format!(
                    "'{from}' is now '{landed}' but apps.toml could not be updated ({e}) and the rollback failed ({re}); change '{from}' to '{landed}' in {}",
                    AppRules::path().display()
                )),
            };
        }
    }
    Ok(RenameReport { landed, routes_followed: moved })
}

/// What deleting a profile left behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeleteReport {
    pub binds_removed: usize,
    pub dangling_routes: Vec<String>,
    /// It was the fallback, and the fallback is now "stay put".
    pub was_fallback: bool,
    /// Clearing that fallback didn't save, so `apps.toml` still names the deleted profile.
    pub fallback_save_error: Option<String>,
}

/// Delete a profile and its binds sidecar. Routes are left in place (re-pointing beats losing them)
/// and reported as dangling; a fallback that named it is cleared, because a dangling fallback would
/// fail on every unmatched focus change with nothing in the route list to show why.
pub fn delete_profile(name: &str) -> Result<DeleteReport, String> {
    Profile::load(name).map_err(|_| format!("no profile '{name}'"))?;
    let binds_removed = crate::authoring::RuleStore::Profile(name.to_string()).load().map_or(0, |r| r.len());
    Profile::delete(name)?;
    let mut rules = AppRules::load();
    let was_fallback = rules.default.as_deref() == Some(name);
    let fallback_save_error = if was_fallback {
        rules.default = None;
        rules.save().err()
    } else {
        None
    };
    Ok(DeleteReport {
        binds_removed,
        dangling_routes: rules.rules.iter().filter(|r| r.profile == name).map(|r| r.app.clone()).collect(),
        was_fallback,
        fallback_save_error,
    })
}

// ── ROUTES (apps.toml) ──────────────────────────────────────────────────────────────────────

/// Route a focused app to a profile. The profile must exist; an identical route is refused.
pub fn route_add(app: &str, profile: &str) -> Result<usize, String> {
    let (app, profile) = (app.trim(), profile.trim());
    if app.is_empty() || profile.is_empty() {
        return Err("a route needs an app needle and a profile".into());
    }
    if !Profile::path(profile).exists() {
        return Err(format!("no profile '{profile}' (neuron profile new {profile})"));
    }
    let mut rules = AppRules::load();
    if rules.rules.iter().any(|r| r.app.eq_ignore_ascii_case(app) && r.profile == profile) {
        return Err(format!("route {app} -> {profile} already exists"));
    }
    rules.rules.push(AppRule { app: app.to_string(), profile: profile.to_string() });
    rules.save()?;
    Ok(rules.rules.len() - 1)
}

/// Remove the route at `index`.
pub fn route_remove(index: usize) -> Result<AppRule, String> {
    let mut rules = AppRules::load();
    if index >= rules.rules.len() {
        return Err(format!("no route at index {index} ({} routes)", rules.rules.len()));
    }
    let gone = rules.rules.remove(index);
    rules.save()?;
    Ok(gone)
}

/// Move the route at `from` to `to` (first match wins, so order is priority).
pub fn route_move(from: usize, to: usize) -> Result<(), String> {
    let mut rules = AppRules::load();
    let n = rules.rules.len();
    if from >= n || to >= n {
        return Err(format!("index out of range ({n} routes)"));
    }
    let r = rules.rules.remove(from);
    rules.rules.insert(to, r);
    rules.save()
}

/// Set (or clear, with `None`) the fallback profile for a focused app that matches no route.
pub fn route_default(profile: Option<&str>) -> Result<(), String> {
    let mut rules = AppRules::load();
    match profile.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            if !Profile::path(p).exists() {
                return Err(format!("no profile '{p}'"));
            }
            rules.default = Some(p.to_string());
        }
        None => rules.default = None,
    }
    rules.save()
}

// ── FEEL (feel.toml) ────────────────────────────────────────────────────────────────────────

/// One edit to the timing windows or the HyperShift stance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeelSet {
    HoldMs(u64),
    GapMs(u64),
    CoyoteMs(u64),
    Hypershift(crate::feel::LayerMode),
}

/// Apply one edit in memory (the caller saves), validating the millisecond windows to 1-5000.
pub fn apply_feel_set(cfg: &mut crate::feel::FeelConfig, set: FeelSet) -> Result<String, String> {
    let window = |ms: u64, what: &str| {
        if (1..=5000).contains(&ms) {
            Ok(ms)
        } else {
            Err(format!("{what} must be 1-5000 ms, not {ms}"))
        }
    };
    match set {
        FeelSet::HoldMs(ms) => {
            cfg.hold_ms = window(ms, "hold-ms")?;
            Ok(format!("hold window -> {ms} ms"))
        }
        FeelSet::GapMs(ms) => {
            cfg.gap_ms = window(ms, "gap-ms")?;
            Ok(format!("gap window -> {ms} ms"))
        }
        FeelSet::CoyoteMs(ms) => {
            cfg.coyote_ms = window(ms, "coyote-ms")?;
            Ok(format!("coyote tail -> {ms} ms"))
        }
        FeelSet::Hypershift(m) => {
            cfg.hypershift = m;
            Ok(format!("hypershift stance -> {}", m.describe()))
        }
    }
}

// ── APP PREFERENCES (app.toml) ──────────────────────────────────────────────────────────────

/// Every top-level key the GUI persists in `app.toml`, with its TOML type. The app's tests hold
/// this list to `Prefs` field for field.
pub const APP_PREF_KINDS: &[(&str, &str)] = &[
    ("start_minimized", "bool"),
    ("ui_accent", "string"),
    ("weave_accent", "string"),
    ("weave_material", "string"),
    ("phoenix", "bool"),
    ("notif_enabled", "bool"),
    ("notif_placement", "string"),
    ("notif_x", "float"),
    ("notif_y", "float"),
    ("notif_audio", "bool"),
    ("notif_dpi", "bool"),
    ("notif_sniper", "bool"),
    ("notif_scroll", "bool"),
    ("notif_polling", "bool"),
    ("notif_brightness", "bool"),
    ("notif_profile", "bool"),
    ("notif_layer", "bool"),
    ("notif_macro", "bool"),
    ("notif_battery", "bool"),
    ("notif_side_plate", "bool"),
    ("notif_game", "bool"),
    ("notif_volume", "float"),
    ("notif_sound", "string"),
    ("notif_panel", "bool"),
    ("notif_stack", "string"),
    ("host_enabled", "bool"),
    ("host_chroma", "bool"),
    ("host_openrgb", "bool"),
    ("host_obs", "bool"),
    ("host_chroma_paint_mode", "string"),
    ("host_chroma_paint_strength", "int"),
    ("host_chroma_paint_fade_ms", "int"),
    ("host_chroma_lens_hue", "int"),
    ("host_chroma_lens_saturation", "int"),
    ("host_chroma_lens_brightness", "int"),
    ("host_openrgb_paint_mode", "string"),
    ("host_openrgb_paint_strength", "int"),
    ("host_openrgb_paint_fade_ms", "int"),
    ("host_paint_disabled_devices", "list"),
    ("host_base_always_wins", "bool"),
    ("host_obs_password", "string"),
    ("lighting", "table"),
];

/// Keys that are secrets: never printed, never dumped, never set from the command line.
pub const SECRET_PREF_KEYS: &[&str] = &["host_obs_password"];

/// The per-device lighting stacks live under `lighting`; `neuron light` owns them.
const PREF_MANAGED_ELSEWHERE: &[&str] = &["lighting"];

/// The known preference keys, for the drift test and `config app list`.
#[must_use]
pub fn app_pref_keys() -> Vec<&'static str> {
    APP_PREF_KINDS.iter().map(|k| k.0).collect()
}

/// `app.toml` in the run root.
#[must_use]
pub fn app_toml_path() -> std::path::PathBuf {
    crate::runroot::run_root().join("app.toml")
}

/// The raw `app.toml` table (empty when the file is absent).
pub fn app_table() -> Result<toml::Table, String> {
    let path = app_toml_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => s.parse::<toml::Table>().map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Write the raw table back atomically.
pub fn save_app_table(t: &toml::Table) -> Result<(), String> {
    let body = toml::to_string_pretty(t).map_err(|e| e.to_string())?;
    crate::salvage::atomic_write(&app_toml_path(), body.as_bytes()).map_err(|e| e.to_string())
}

fn pref_kind(key: &str) -> Result<&'static str, String> {
    APP_PREF_KINDS
        .iter()
        .find(|k| k.0 == key)
        .map(|k| k.1)
        .ok_or_else(|| format!("unknown preference '{key}' (`neuron config app list`)"))
}

fn one_of(key: &str, v: &str, allowed: &[&str]) -> Result<(), String> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(format!("{key} must be one of: {}", allowed.join(", ")))
    }
}

/// Parse `raw` into the TOML value preference `key` holds, validating range and vocabulary.
pub fn parse_pref_value(key: &str, raw: &str) -> Result<toml::Value, String> {
    if SECRET_PREF_KEYS.contains(&key) {
        return Err(format!("'{key}' is a secret; set it in the app (SYSTEM -> CONNECTIONS)"));
    }
    if PREF_MANAGED_ELSEWHERE.contains(&key) {
        return Err(format!("'{key}' is managed by `neuron light`"));
    }
    let kind = pref_kind(key)?;
    let raw = raw.trim();
    let value = match kind {
        "bool" => toml::Value::Boolean(parse_bool(raw)?),
        "int" => toml::Value::Integer(raw.parse::<i64>().map_err(|_| format!("{key} needs a whole number"))?),
        "float" => toml::Value::Float(raw.parse::<f64>().map_err(|_| format!("{key} needs a number"))?),
        "list" => toml::Value::Array(
            raw.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        ),
        _ => toml::Value::String(raw.to_string()),
    };
    match (key, &value) {
        ("ui_accent" | "weave_accent", toml::Value::String(s)) => {
            let hex = s.trim_start_matches('#');
            if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("{key} must be a 6-digit hex colour like 4af2b0"));
            }
            return Ok(toml::Value::String(hex.to_ascii_lowercase()));
        }
        ("notif_placement", toml::Value::String(s)) => {
            one_of(key, s, &["off", "top-left", "top-right", "bottom-left", "bottom-right", "inline", "custom"])?;
        }
        ("notif_stack", toml::Value::String(s)) => one_of(key, s, &["stack", "latest", "digest"])?,
        ("notif_sound", toml::Value::String(s)) => one_of(key, s, &["pulse", "warm", "glass"])?,
        ("host_chroma_paint_mode" | "host_openrgb_paint_mode", toml::Value::String(s)) => {
            one_of(key, s, &["replace", "merge", "boost", "tint"])?;
        }
        ("notif_x" | "notif_y" | "notif_volume", toml::Value::Float(f)) if !(0.0..=1.0).contains(f) => {
            return Err(format!("{key} must be 0.0-1.0"));
        }
        ("host_chroma_paint_strength" | "host_openrgb_paint_strength", toml::Value::Integer(i)) if !(0..=100).contains(i) => {
            return Err(format!("{key} must be 0-100"));
        }
        ("host_chroma_paint_fade_ms" | "host_openrgb_paint_fade_ms", toml::Value::Integer(i)) if !(0..=2500).contains(i) => {
            return Err(format!("{key} must be 0-2500"));
        }
        ("host_chroma_lens_hue", toml::Value::Integer(i)) if !(-180..=180).contains(i) => {
            return Err(format!("{key} must be -180..180"));
        }
        ("host_chroma_lens_saturation" | "host_chroma_lens_brightness", toml::Value::Integer(i)) if !(0..=200).contains(i) => {
            return Err(format!("{key} must be 0-200"));
        }
        _ => {}
    }
    Ok(value)
}

/// Set one preference and save. Returns the value written.
pub fn set_app_pref(key: &str, raw: &str) -> Result<toml::Value, String> {
    let value = parse_pref_value(key, raw)?;
    let mut t = app_table()?;
    t.insert(key.to_string(), value.clone());
    save_app_table(&t)?;
    Ok(value)
}

/// Remove one preference from the file so the app falls back to its default.
pub fn unset_app_pref(key: &str) -> Result<bool, String> {
    pref_kind(key)?;
    if SECRET_PREF_KEYS.contains(&key) || PREF_MANAGED_ELSEWHERE.contains(&key) {
        return Err(format!("'{key}' cannot be unset here"));
    }
    let mut t = app_table()?;
    let had = t.remove(key).is_some();
    if had {
        save_app_table(&t)?;
    }
    Ok(had)
}

/// The preferences as JSON, secrets redacted.
pub fn app_prefs_json() -> Result<serde_json::Value, String> {
    let mut t = app_table()?;
    for k in SECRET_PREF_KEYS {
        if t.contains_key(*k) {
            t.insert((*k).to_string(), toml::Value::String("<redacted>".into()));
        }
    }
    serde_json::to_value(&t).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> crate::authoring::TestRoot {
        crate::authoring::test_run_root()
    }

    #[test]
    fn profile_fields_validate_and_unset() {
        let mut p = Profile::default();
        set_profile_field(&mut p, "dpi", "1600").unwrap();
        set_profile_field(&mut p, "dpi-stages", "800,1600,3200").unwrap();
        set_profile_field(&mut p, "polling", "500").unwrap();
        set_profile_field(&mut p, "in-game-polling", "1000,500").unwrap();
        set_profile_field(&mut p, "disable-win", "true").unwrap();
        assert_eq!(p.dpi, Some(1600));
        assert_eq!(p.dpi_stages, vec![800, 1600, 3200]);
        assert_eq!(p.in_game_polling, Some((1000, 500)));
        assert!(p.disable_win);
        assert!(set_profile_field(&mut p, "dpi", "99").is_err());
        assert!(set_profile_field(&mut p, "polling", "333").is_err());
        assert!(set_profile_field(&mut p, "brightness", "101").is_err());
        assert!(set_profile_field(&mut p, "bogus", "1").is_err());
        set_profile_field(&mut p, "dpi", "unset").unwrap();
        assert_eq!(p.dpi, None);
    }

    #[test]
    fn profile_bundle_round_trips_with_binds() {
        let _r = root();
        create_profile("game").unwrap();
        assert!(create_profile("game").is_err(), "a second create must not clobber");
        assert!(create_profile("gui").is_err(), "the reserved name is refused");
        let mut b = export_profile("game").unwrap();
        b.profile.dpi = Some(800);
        b.rules.push(Rule::new(
            crate::engine::Trigger::MicTap,
            crate::action::Action::Key { key: "f".into() },
        ));
        save_bundle(&b).unwrap();
        let back = export_profile("game").unwrap();
        assert_eq!(back, b);
        let text = toml::to_string_pretty(&back).unwrap();
        assert_eq!(toml::from_str::<ProfileBundle>(&text).unwrap(), back, "bundle survives TOML");
    }

    #[test]
    fn rename_retargets_routes_and_delete_reports_dangling() {
        let _r = root();
        create_profile("a").unwrap();
        route_add("valorant", "a").unwrap();
        assert!(route_add("valorant", "a").is_err(), "duplicate route refused");
        assert!(route_add("x", "nope").is_err(), "route to a missing profile refused");
        route_default(Some("a")).unwrap();
        let rep = rename_profile("a", "b").unwrap();
        assert_eq!(rep.landed, "b");
        assert_eq!(rep.routes_followed, 2);
        let rules = AppRules::load();
        assert_eq!(rules.rules[0].profile, "b");
        assert_eq!(rules.default.as_deref(), Some("b"));
        let del = delete_profile("b").unwrap();
        assert_eq!(del.dangling_routes, vec!["valorant".to_string()]);
        assert!(del.was_fallback);
    }

    #[test]
    fn feel_windows_are_bounded() {
        let mut cfg = crate::feel::FeelConfig::default();
        apply_feel_set(&mut cfg, FeelSet::HoldMs(250)).unwrap();
        assert_eq!(cfg.hold_ms, 250);
        assert!(apply_feel_set(&mut cfg, FeelSet::GapMs(0)).is_err());
        assert!(apply_feel_set(&mut cfg, FeelSet::CoyoteMs(9000)).is_err());
    }

    #[test]
    fn app_prefs_validate_and_never_touch_secrets() {
        let _r = root();
        set_app_pref("notif_volume", "0.4").unwrap();
        set_app_pref("phoenix", "off").unwrap();
        set_app_pref("ui_accent", "#FF8800").unwrap();
        let t = app_table().unwrap();
        assert_eq!(t["ui_accent"].as_str(), Some("ff8800"));
        assert_eq!(t["phoenix"].as_bool(), Some(false));
        assert!(set_app_pref("notif_volume", "loud").is_err());
        assert!(set_app_pref("notif_volume", "2.0").is_err());
        assert!(set_app_pref("notif_placement", "middle").is_err());
        assert!(set_app_pref("no_such_pref", "1").is_err());
        assert!(set_app_pref("host_obs_password", "hunter2").is_err(), "secrets are not settable here");
        assert!(set_app_pref("lighting", "x").is_err());
        assert!(unset_app_pref("phoenix").unwrap());
        assert!(app_table().unwrap().get("phoenix").is_none());
    }
}
