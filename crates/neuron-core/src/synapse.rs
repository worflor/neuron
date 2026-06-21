//! Version-agnostic Synapse config ingest ("eat the data"). We never hard-code a Synapse
//! version's schema or install path — only the stable VENDOR root ("Razer" under the standard
//! OS data dirs). Files are classified and mined by their *content shape* (the element/key names
//! they carry), the same self-emergent spirit as device discovery: meaning, not version. So it
//! eats Synapse 2's XML, 3's XML/JSON, 4's whatever, and future n the same way.
//!
//! Encrypted / account-synced profiles (Razer's lock-in) are detected and skipped — the
//! readable device/mapping/lighting/audio config is the valuable, portable surface.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A kind of configuration inferred from a file's content (not its name or Synapse version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigKind {
    Dpi,
    Lighting,
    Mapping,
    Macro,
    Audio,
    Profile,
}

impl ConfigKind {
    pub fn label(self) -> &'static str {
        match self {
            ConfigKind::Dpi => "dpi",
            ConfigKind::Lighting => "lighting",
            ConfigKind::Mapping => "mapping",
            ConfigKind::Macro => "macro",
            ConfigKind::Audio => "audio",
            ConfigKind::Profile => "profile",
        }
    }
}

/// One harvested config file: where it is, how big, what it likely holds, and a readable head.
#[derive(Debug, Clone)]
pub struct Found {
    pub path: PathBuf,
    pub size: u64,
    pub kinds: Vec<ConfigKind>,
    pub encrypted: bool,
    /// bounded readable prefix (empty if encrypted/binary) — what `extract` mines.
    pub head: String,
}

/// Stable vendor data roots, resolved from the environment. Only "Razer" is fixed; the OS data
/// dirs come from env, so this is correct across Windows versions and Synapse versions.
pub fn locate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["PROGRAMDATA", "LOCALAPPDATA", "APPDATA", "USERPROFILE"] {
        if let Ok(base) = std::env::var(var) {
            let p = PathBuf::from(base).join("Razer");
            if p.is_dir() {
                roots.push(p);
            }
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn is_config_ext(p: &Path) -> bool {
    matches!(
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .as_deref(),
        Some("xml" | "json" | "dat" | "config")
    )
}

fn printable_ratio(s: &str) -> f32 {
    if s.is_empty() {
        return 1.0;
    }
    let ok = s
        .bytes()
        .filter(|b| b.is_ascii_graphic() || b.is_ascii_whitespace())
        .count();
    ok as f32 / s.len() as f32
}

fn read_head(p: &Path, max_bytes: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(p) else {
        return String::new();
    };
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Classify a config text by the signals it carries (lowercased substring match on element /
/// key names). These names are stable in MEANING across Synapse versions even as schemas change.
pub fn classify_text(text: &str) -> Vec<ConfigKind> {
    let t = text.to_lowercase();
    let has = |kws: &[&str]| kws.iter().any(|k| t.contains(k));
    let mut kinds = Vec::new();
    if has(&["\"dpi\"", "<dpi", "dpistage", "cpi", "sensitivitystage"]) {
        kinds.push(ConfigKind::Dpi);
    }
    if has(&[
        "rgb",
        "<led",
        "chroma",
        "<color",
        "effecttype",
        "brightness",
        "spectrum",
        "lighting",
    ]) {
        kinds.push(ConfigKind::Lighting);
    }
    if has(&[
        "<mapping",
        "assignment",
        "broadcasterinput",
        "hypershift",
        "keymap",
        "remap",
        "buttonassignment",
    ]) {
        kinds.push(ConfigKind::Mapping);
    }
    if has(&["<macro", "keystroke", "macroassignment"]) {
        kinds.push(ConfigKind::Macro);
    }
    if has(&[
        "micvolume",
        "headphonevolume",
        "sidetone",
        "equalizer",
        "surround",
        "samplingrate",
        "noisereduction",
    ]) {
        kinds.push(ConfigKind::Audio);
    }
    if has(&[
        "<profile",
        "profilename",
        "pollingrate",
        "devicesettings",
        "calibration",
    ]) {
        kinds.push(ConfigKind::Profile);
    }
    kinds
}

/// Walk the roots and classify candidate config files (bounded for safety).
pub fn harvest(roots: &[PathBuf], max_files: usize, max_bytes: usize) -> Vec<Found> {
    let mut out = Vec::new();
    for r in roots {
        walk(r, 0, 8, &mut out, max_files, max_bytes);
        if out.len() >= max_files {
            break;
        }
    }
    out
}

fn walk(
    dir: &Path,
    depth: u32,
    max_depth: u32,
    out: &mut Vec<Found>,
    max_files: usize,
    max_bytes: usize,
) {
    if depth > max_depth || out.len() >= max_files {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in rd.flatten() {
        if out.len() >= max_files {
            return;
        }
        let p = entry.path();
        if p.is_dir() {
            walk(&p, depth + 1, max_depth, out, max_files, max_bytes);
        } else if is_config_ext(&p) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();
            let name_enc = name.contains("enc");
            let head = if name_enc {
                String::new()
            } else {
                read_head(&p, max_bytes)
            };
            let encrypted = name_enc || (!head.is_empty() && printable_ratio(&head) < 0.7);
            let kinds = if encrypted {
                Vec::new()
            } else {
                classify_text(&head)
            };
            out.push(Found {
                path: p,
                size,
                kinds,
                encrypted,
                head,
            });
        }
    }
}

/// Mine (key, value) pairs from a harvested file — the actual eatable values. Handles JSON
/// (parse + recurse for signal keys) and XML/text (known element tags). Bounded to the head.
pub fn extract(f: &Found) -> Vec<(String, String)> {
    if f.encrypted || f.head.is_empty() {
        return Vec::new();
    }
    let trimmed = f.head.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let mut out = Vec::new();
            json_walk(&v, &mut out);
            return out;
        }
    }
    // XML / text: pull known scalar tags
    const TAGS: &[&str] = &[
        "MicVolume",
        "HeadphoneVolume",
        "MicState",
        "HeadphoneState",
        "SamplingRate",
        "Dpi",
        "CPI",
        "PollingRate",
        "Brightness",
        "Sensitivity",
        "Sidetone",
        "MappingGroup",
        "InputType",
        "BroadcasterInput",
        "MultimediaAssignment",
        "ButtonAssignment",
        "ProfileName",
        "EffectType",
    ];
    let mut out = Vec::new();
    for tag in TAGS {
        for v in xml_tag_all(&f.head, tag) {
            out.push((tag.to_string(), v));
        }
    }
    out
}

/// Signal keys we care about when mining JSON (case-insensitive substring of the key).
fn json_key_signal(key: &str) -> bool {
    let k = key.to_lowercase();
    [
        "dpi",
        "cpi",
        "pollingrate",
        "brightness",
        "color",
        "effect",
        "micvolume",
        "volume",
        "sensitivity",
        "sidetone",
        "macro",
        "assignment",
        "binding",
        "profilename",
    ]
    .iter()
    .any(|s| k.contains(s))
}

fn json_walk(v: &serde_json::Value, out: &mut Vec<(String, String)>) {
    match v {
        serde_json::Value::Object(map) => {
            for (k, val) in map {
                if json_key_signal(k) {
                    if let Some(scalar) = json_scalar(val) {
                        out.push((k.clone(), scalar));
                    }
                }
                json_walk(val, out);
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|x| json_walk(x, out)),
        _ => {}
    }
}

fn json_scalar(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) if s.len() <= 64 => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// All `<Tag>scalar</Tag>` inner values for a tag (scalar = short, no nested element).
fn xml_tag_all(text: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(s) = text[from..].find(&open) {
        let start = from + s + open.len();
        let Some(e) = text[start..].find(&close) else {
            break;
        };
        let inner = text[start..start + e].trim();
        if !inner.is_empty() && inner.len() <= 64 && !inner.contains('<') {
            out.push(inner.to_string());
        }
        from = start + e + close.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE_SETTINGS: &str = r#"<?xml version="1.0"?>
<DeviceSettings><Id>262145</Id><MicVolume>65</MicVolume><HeadphoneVolume>50</HeadphoneVolume>
<MicState>true</MicState><SamplingRate>SamplingRate_48</SamplingRate></DeviceSettings>"#;

    const MAPPINGS: &str = r#"<DefaultMappings><MappingList><Mapping>
<MappingGroup>Multimedia</MappingGroup><InputType>BroadcasterInput</InputType>
<BroadcasterInput>TopButtonClick</BroadcasterInput>
<MultimediaAssignment>MuteMic</MultimediaAssignment></Mapping></MappingList></DefaultMappings>"#;

    #[test]
    fn classifies_audio_and_profile_settings() {
        let k = classify_text(DEVICE_SETTINGS);
        assert!(k.contains(&ConfigKind::Audio), "MicVolume => audio");
        assert!(
            k.contains(&ConfigKind::Profile),
            "DeviceSettings => profile"
        );
    }

    #[test]
    fn classifies_mapping() {
        let k = classify_text(MAPPINGS);
        assert!(
            k.contains(&ConfigKind::Mapping),
            "BroadcasterInput/Assignment => mapping"
        );
    }

    #[test]
    fn extracts_xml_scalar_settings() {
        let f = Found {
            path: "x.xml".into(),
            size: 0,
            kinds: vec![],
            encrypted: false,
            head: DEVICE_SETTINGS.into(),
        };
        let vals = extract(&f);
        assert!(vals.contains(&("MicVolume".into(), "65".into())));
        assert!(vals.contains(&("HeadphoneVolume".into(), "50".into())));
        assert!(vals.contains(&("SamplingRate".into(), "SamplingRate_48".into())));
    }

    #[test]
    fn extracts_mapping_pairs() {
        let f = Found {
            path: "m.xml".into(),
            size: 0,
            kinds: vec![],
            encrypted: false,
            head: MAPPINGS.into(),
        };
        let vals = extract(&f);
        assert!(vals
            .iter()
            .any(|(k, v)| k == "BroadcasterInput" && v == "TopButtonClick"));
        assert!(vals
            .iter()
            .any(|(k, v)| k == "MultimediaAssignment" && v == "MuteMic"));
    }

    #[test]
    fn extracts_json_signal_keys() {
        let f = Found {
            path: "p.json".into(),
            size: 0,
            kinds: vec![],
            encrypted: false,
            head: r#"{"profile":{"dpi":[800,1600],"pollingRate":1000,"led":{"color":"FF0000"}}}"#
                .into(),
        };
        let vals = extract(&f);
        assert!(vals
            .iter()
            .any(|(k, _)| k.to_lowercase().contains("pollingrate")));
        assert!(vals
            .iter()
            .any(|(k, v)| k.to_lowercase().contains("color") && v == "FF0000"));
    }

    #[test]
    fn encrypted_files_are_not_mined() {
        let f = Found {
            path: "e.xml".into(),
            size: 0,
            kinds: vec![],
            encrypted: true,
            head: String::new(),
        };
        assert!(extract(&f).is_empty());
    }
}
