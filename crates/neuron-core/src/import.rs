// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Synapse migration — ingest Synapse's official **Export** files into Neuron config.
//!
//! Unlike Synapse's cloud cache (rennab.json / *Enc* = AES, the lock-in we refuse to crack),
//! Synapse's in-app Export produces ZIP files with fake extensions that are **plain XML** —
//! fully readable, no crypto. This module is the clean, no-gimmick importer (sibling to
//! `synapse.rs`, which scrapes the live install; this one ingests a user-handed export file).
//!
//! ## Formats (cracked, see project memory)
//! * `*.synapse3` — a device profile ZIP: `DeviceInfo.xml` (Name/PID/VID/Serial),
//!   `Profiles/<guid>.xml`, `Macros/` (often empty), and `Features/<profileGuid>/<featureGuid>.xml`
//!   one per capability. Capabilities are keyed by a **stable feature-GUID** identical across
//!   devices (LedBrightness 6c91bf99, PollingRate 1ca05056, DPI bc7fc799, DPIStages 25f22ab7,
//!   ScrollWheel 27838d6a, ScrollWheelStages 429f8d88, GamingMode a04163a1, LedPowerSettings
//!   a8664fc4, InGamePollingRate 8997620a, LightingEffects 03bec892, Mappings 762555eb) — import
//!   by GUID is version/device-agnostic for free.
//! * `*.ChromaEffects` — one XML: `Mode` basic (named effect + palette) or advanced (per-cell /
//!   per-layer). An advanced stack maps WHOLE onto Neuron's compositor — every animated layer
//!   (fire/wave/spectrum/colorwheel/starlight, and the live `reactive`/`audiometer`) has a host
//!   Pattern × Spectrum — and a static layer survives losslessly as the per-LED paint frame.
//!
//! ## Normalization (drop the noise)
//! Mappings: drop identity binds (a key that maps to its own default scancode = Synapse's default
//! fill, not user intent) and the HyperShift identity fill; split base vs `IsHyperShift`; resolve
//! DKM/HID/Mouse input -> a friendly [`crate::engine::Trigger`] + typed [`crate::action::Action`].
//! Lighting: `basic` -> a named effect + colour on the [`Profile`]; `advanced` -> a Neuron
//! compositor layer stack (one [`crate::pattern::LayerDef`] per animated layer) plus, if present, a
//! lossless static per-LED frame.
//!
//! Output: a normalized [`Imported`] bundle (a [`crate::profile::Profile`] + a `Vec<Rule>` of
//! bindings) the caller writes to Neuron config and applies host-side.
//!
//! Ownership: the MIGRATE agent owns this file. Uses the frozen `zip` + `quick-xml` deps.

use crate::action::{Action, Direction, MediaKind, MouseButtonKind};
use crate::engine::{Rule, Trigger};
use crate::lighting::Rgb;
use crate::profile::Profile;
use anyhow::{Context as _, Result};
use std::io::{Cursor, Read};
use std::path::Path;

// ───────────────────────────────────────── feature GUIDs ─────────────────────────────────────

/// A stable Synapse feature-GUID — the version/device-agnostic import contract. Each capability
/// has the same GUID on every device, so the importer dispatches on it rather than on a v3
/// schema path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeatureGuid {
    LedBrightness,
    PollingRate,
    Dpi,
    DpiStages,
    ScrollWheel,
    ScrollWheelStages,
    GamingMode,
    LedPowerSettings,
    InGamePollingRate,
    LightingEffects,
    Mappings,
}

impl FeatureGuid {
    /// Resolve a feature file's GUID (the `<guid>` in `Features/<profile>/<guid>.xml`) to a
    /// known capability. Matches on the leading GUID segment (the first dash-group), which is the
    /// stable part identical across devices. Returns `None` for GUIDs Neuron doesn't ingest
    /// (forward-compatible: an unknown feature is logged + skipped, not an error).
    pub fn from_guid(guid: &str) -> Option<FeatureGuid> {
        // The first 8 hex chars (before the first '-') uniquely identify the capability.
        let head = guid.split('-').next().unwrap_or(guid).to_ascii_lowercase();
        Some(match head.as_str() {
            "6c91bf99" => FeatureGuid::LedBrightness,
            "1ca05056" => FeatureGuid::PollingRate,
            "bc7fc799" => FeatureGuid::Dpi,
            "25f22ab7" => FeatureGuid::DpiStages,
            "27838d6a" => FeatureGuid::ScrollWheel,
            "429f8d88" => FeatureGuid::ScrollWheelStages,
            "a04163a1" => FeatureGuid::GamingMode,
            "a8664fc4" => FeatureGuid::LedPowerSettings,
            "8997620a" => FeatureGuid::InGamePollingRate,
            "03bec892" => FeatureGuid::LightingEffects,
            "762555eb" => FeatureGuid::Mappings,
            _ => return None,
        })
    }

    /// Human label for notes/logs.
    pub fn label(self) -> &'static str {
        match self {
            FeatureGuid::LedBrightness => "LedBrightness",
            FeatureGuid::PollingRate => "PollingRate",
            FeatureGuid::Dpi => "DPI",
            FeatureGuid::DpiStages => "DPIStages",
            FeatureGuid::ScrollWheel => "ScrollWheel",
            FeatureGuid::ScrollWheelStages => "ScrollWheelStages",
            FeatureGuid::GamingMode => "GamingMode",
            FeatureGuid::LedPowerSettings => "LedPowerSettings",
            FeatureGuid::InGamePollingRate => "InGamePollingRate",
            FeatureGuid::LightingEffects => "LightingEffects",
            FeatureGuid::Mappings => "Mappings",
        }
    }
}

// ───────────────────────────────────────── result bundle ─────────────────────────────────────

/// The normalized result of importing one Synapse export — what gets written into Neuron config.
#[derive(Clone, Debug, Default)]
pub struct Imported {
    /// The settings bundle (DPI/polling/brightness/lighting/idle/...), all optional.
    pub profile: Profile,
    /// The remap/macro surface as spine rules (base + HyperShift split is encoded in each
    /// rule's `Trigger`/`Action`).
    pub rules: Vec<Rule>,
    /// Non-fatal notes (skipped layers, unrecognized GUIDs, fields Neuron can't yet store) for
    /// transparency in the import wizard.
    pub notes: Vec<String>,
}

impl Imported {
    fn note(&mut self, msg: impl Into<String>) {
        self.notes.push(msg.into());
    }
}

// ───────────────────────────────────────── entry points ──────────────────────────────────────

/// Import a Synapse export file (`*.synapse3` or `*.ChromaEffects`) into a normalized
/// [`Imported`] bundle. Detects the format by content (it's a ZIP regardless of extension),
/// unzips it, parses the plaintext XML, and normalizes.
pub fn import_export(path: &Path) -> Result<Imported> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading Synapse export {}", path.display()))?;
    import_export_bytes(&bytes)
}

/// Content-detecting import over raw bytes (so the GUI can hand us an in-memory upload). Both
/// export kinds are ZIPs; we route by which member files are present.
pub fn import_export_bytes(bytes: &[u8]) -> Result<Imported> {
    let zip = zip::ZipArchive::new(Cursor::new(bytes))
        .context("export is not a ZIP (Synapse exports are ZIPs with a fake extension)")?;
    let names: Vec<String> = zip.file_names().map(|s| s.to_string()).collect();
    let has_device_info = names.iter().any(|n| n.ends_with("DeviceInfo.xml"));
    if has_device_info {
        import_synapse3(bytes)
    } else {
        // A `.ChromaEffects` export is a bare lighting XML (one or more, no DeviceInfo.xml).
        import_chroma_effects(bytes)
    }
}

/// Parse a `*.synapse3` device-profile ZIP into an [`Imported`] bundle (DeviceInfo + Features +
/// Mappings).
pub fn import_synapse3(zip_bytes: &[u8]) -> Result<Imported> {
    let mut zip = zip::ZipArchive::new(Cursor::new(zip_bytes)).context("opening .synapse3 ZIP")?;
    let mut out = Imported::default();

    // ── identity: DeviceInfo.xml + the profile name ──────────────────────────────────────────
    if let Some(info) = read_member_ending(&mut zip, "DeviceInfo.xml") {
        let name = scalar(&info, "Name");
        let pid = scalar(&info, "Product_ID");
        if let Some(n) = &name {
            out.note(format!(
                "device: {n}{}",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            ));
        }
    }
    // The profile name lives in Profiles/<guid>.xml; use it for the Neuron profile name.
    let profile_member = zip
        .file_names()
        .find(|n| n.contains("Profiles/") && n.ends_with(".xml"))
        .map(|s| s.to_string());
    let profile_name = profile_member
        .and_then(|n| read_member(&mut zip, &n))
        .and_then(|xml| scalar(&xml, "Name"));
    out.profile.name = profile_name
        .clone()
        .unwrap_or_else(|| "synapse-import".to_string());

    // ── features: dispatch each Features/<profile>/<guid>.xml on its stable GUID ──────────────
    let feature_members: Vec<String> = zip
        .file_names()
        .filter(|n| n.contains("Features/") && n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();

    for member in feature_members {
        // The GUID is the file stem.
        let stem = member
            .rsplit('/')
            .next()
            .and_then(|f| f.strip_suffix(".xml"))
            .unwrap_or("");
        let Some(xml) = read_member(&mut zip, &member) else {
            continue;
        };
        match FeatureGuid::from_guid(stem) {
            Some(FeatureGuid::Mappings) => ingest_mappings(&xml, &mut out),
            Some(feat) => ingest_feature(feat, &xml, &mut out),
            None => out.note(format!(
                "unknown feature GUID {stem} — skipped (forward-compatible)"
            )),
        }
    }

    Ok(out)
}

/// Parse a `*.ChromaEffects` ZIP into lighting on the [`Imported`] profile, dropping
/// reactive/audiometer/colorwheel layers (keeping only a basic named effect or a resolvable
/// static frame).
pub fn import_chroma_effects(zip_bytes: &[u8]) -> Result<Imported> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(zip_bytes)).context("opening .ChromaEffects ZIP")?;
    let mut out = Imported::default();
    out.profile.name = "chroma-import".to_string();

    // One or more lighting XMLs (named by GUID). Ingest each into the profile's lighting fields;
    // the last static frame wins (these exports are typically one effect file).
    let members: Vec<String> = zip
        .file_names()
        .filter(|n| n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();
    if members.is_empty() {
        anyhow::bail!("no XML inside .ChromaEffects archive");
    }
    for member in members {
        if let Some(xml) = read_member(&mut zip, &member) {
            ingest_lighting(&xml, &mut out);
        }
    }
    Ok(out)
}

// ─────────────────────────────────────── feature ingestion ───────────────────────────────────

/// Normalize one non-Mappings feature file into the [`Profile`] (or a note for things Neuron's
/// Profile struct can't yet carry).
fn ingest_feature(feat: FeatureGuid, xml: &str, out: &mut Imported) {
    match feat {
        FeatureGuid::LedBrightness => {
            if let Some(b) = scalar(xml, "Brightness").and_then(|s| s.parse::<u32>().ok()) {
                out.profile.brightness = Some(b.min(100) as u8);
            }
        }
        FeatureGuid::Dpi => {
            // Active DPI = the <X> value (Razer keeps X==Y for symmetric DPI).
            if let Some(d) = scalar(xml, "X").and_then(|s| s.parse::<u16>().ok()) {
                out.profile.dpi = Some(d);
            }
        }
        FeatureGuid::DpiStages => {
            // Each <DPIStage> has <X>/<Y>; stages tagged <Active>false</Active> are *disabled*
            // slots (Synapse keeps 5 slots, only the first N enabled). Keep the enabled stages
            // in order; the TOP-LEVEL <Active>index</Active> (sibling of <Stages>, NOT the
            // per-stage <Active>false</Active>) selects the live one.
            let stages = parse_dpi_stages(xml);
            if !stages.is_empty() {
                out.profile.dpi_stages = stages.iter().map(|s| s.dpi).collect();
                // Active index -> the live DPI (if the plain DPI file didn't already set it).
                if out.profile.dpi.is_none() {
                    let active = top_level_dpi_active(xml).unwrap_or(0);
                    if let Some(s) = stages.get(active) {
                        out.profile.dpi = Some(s.dpi);
                    }
                }
            }
        }
        FeatureGuid::PollingRate => {
            if let Some(hz) = scalar(xml, "Value").and_then(|s| s.parse::<u32>().ok()) {
                out.profile.polling_hz = Some(hz);
            }
        }
        FeatureGuid::InGamePollingRate => {
            // Wired/Dongle in-game poll override — stored losslessly as a (wired, dongle) pair on
            // the Profile (no longer collapsed to a note). The wired value also seeds the plain
            // poll rate if PollingRate wasn't present, so a single-rate apply still works.
            let wired = scalar(xml, "WiredValue").and_then(|s| s.parse::<u32>().ok());
            let dongle = scalar(xml, "DongleValue").and_then(|s| s.parse::<u32>().ok());
            if let (Some(w), Some(d)) = (wired, dongle) {
                out.profile.in_game_polling = Some((w, d));
            }
            if let Some(hz) = wired {
                if out.profile.polling_hz.is_none() {
                    out.profile.polling_hz = Some(hz);
                }
            }
        }
        FeatureGuid::GamingMode => {
            // Gaming-mode toggles -> the Profile's first-class gaming flags (host-side enforced by
            // the daemon). Synapse uses 1/true for "disable this". An empty <GamingMode/> (the
            // mouse export) sets nothing.
            let on = |tag: &str| {
                scalar(xml, tag)
                    .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
            };
            out.profile.disable_alt_tab = on("DisableAltTabState");
            out.profile.disable_win = on("DisableWinState") || on("DisableWindowsKeyState");
            out.profile.disable_alt_f4 = on("DisableAltF4State");
            // Deliberately the three SYNAPSE chords, not `has_gaming()`: Synapse has no Alt+Esc guard,
            // so an import never sets it — and the note below reports exactly what Synapse carried.
            if out.profile.disable_alt_tab || out.profile.disable_win || out.profile.disable_alt_f4
            {
                out.note(format!(
                    "gaming mode: alt-tab={} win={} alt-f4={}",
                    out.profile.disable_alt_tab,
                    out.profile.disable_win,
                    out.profile.disable_alt_f4
                ));
            } else {
                out.note("gaming mode present (no overrides set)".to_string());
            }
        }
        FeatureGuid::LedPowerSettings => {
            // LED idle-off timeout -> Profile.idle_secs (lossless). Only when the idle state is
            // enabled (<IdleState>1</IdleState>); a disabled idle leaves it unset.
            let enabled = scalar(xml, "IdleState")
                .map(|s| s == "1" || s.eq_ignore_ascii_case("true"))
                .unwrap_or(true);
            let secs = scalar(xml, "IdleStateValue").and_then(|s| s.parse::<u32>().ok());
            if let Some(secs) = secs {
                if enabled {
                    out.profile.idle_secs = Some(secs);
                }
                out.note(format!(
                    "LED idle-off after {secs}s (idle {})",
                    if enabled { "on" } else { "off" }
                ));
            }
        }
        FeatureGuid::ScrollWheel => {
            out.note(
                "scroll-wheel browsing-mode app list dropped (per-app feature, not a setting)"
                    .to_string(),
            );
        }
        FeatureGuid::ScrollWheelStages => {
            // HyperScroll stages (Adaptive/UltraFine/Distinct/Custom/FreeScroll). The active stage
            // is the `<Mode>` INSIDE the `<ActiveScrollStage>` block — NOT the first `<Mode>` in
            // the document (which is stage 2 "Adaptive"; stage 1 has no Mode tag at all). Reading
            // the first Mode was the latent bug (it reported "Adaptive" when the real active stage
            // is "FreeScroll"). Neuron can select an active stage via the confirmed 0x15/0x00
            // command, but full imported stage-table programming still needs class 0x0B RE.
            let active = active_scroll_mode(xml);
            out.note(format!(
                "scroll-wheel stages present{} — active-stage select exists; full table import still pending HyperScroll class 0x0B RE",
                active.map(|m| format!(" (active {m})")).unwrap_or_default()
            ));
        }
        FeatureGuid::LightingEffects => ingest_lighting(xml, out),
        FeatureGuid::Mappings => unreachable!("Mappings routed to ingest_mappings"),
    }
}

/// The `<Mode>` of the active HyperScroll stage — read from inside the `<ActiveScrollStage>`
/// block (the authoritative active marker), not the first `<Mode>` in the document. Returns the
/// mode name (e.g. "FreeScroll") or `None` if the block/mode is absent.
fn active_scroll_mode(xml: &str) -> Option<String> {
    let block = split_blocks(xml, "ActiveScrollStage").into_iter().next()?;
    scalar(&block, "Mode")
}

/// One DPI stage row.
struct DpiStage {
    dpi: u16,
    active_flag_false: bool,
}

/// The TOP-LEVEL `<Active>index</Active>` of a `<DPIStages>` file — the sibling of `<Stages>`
/// that selects the live stage by its index in the **enabled** list. This is NOT the per-stage
/// `<Active>false</Active>` flag (a disabled-slot marker living *inside* each `<DPIStage>`); a
/// naive `scalar(xml, "Active")` returns that per-stage `false` first, which is the latent bug.
/// We isolate it by slicing the `<Stages>…</Stages>` block out first, then reading `Active` from
/// what remains (the `<DPIStages>`-level scalars). Returns `None` if absent/unparseable.
fn top_level_dpi_active(xml: &str) -> Option<usize> {
    // Remove the inner <Stages>...</Stages> (which holds the per-stage <Active>false</Active>),
    // leaving only the file-level <Active>index</Active>.
    let outer = match (xml.find("<Stages>"), xml.find("</Stages>")) {
        (Some(open), Some(close)) if close > open => {
            let mut s = String::with_capacity(xml.len());
            s.push_str(&xml[..open]);
            s.push_str(&xml[close + "</Stages>".len()..]);
            s
        }
        // No <Stages> wrapper: fall back to the whole document.
        _ => xml.to_string(),
    };
    scalar(&outer, "Active").and_then(|s| s.parse::<usize>().ok())
}

/// Parse `<Stages><DPIStage><X>..</X>...<Active>false</Active></DPIStage>...` into enabled
/// stages (those NOT tagged `<Active>false</Active>`, in document order).
fn parse_dpi_stages(xml: &str) -> Vec<DpiStage> {
    let mut stages = Vec::new();
    for block in split_blocks(xml, "DPIStage") {
        let dpi = scalar(&block, "X").and_then(|s| s.parse::<u16>().ok());
        // A per-stage <Active>false</Active> marks a DISABLED slot.
        let disabled = scalar(&block, "Active")
            .map(|s| s.eq_ignore_ascii_case("false"))
            .unwrap_or(false);
        if let Some(dpi) = dpi {
            stages.push(DpiStage {
                dpi,
                active_flag_false: disabled,
            });
        }
    }
    // Keep only enabled stages (Synapse fills 5 slots; the user's real list = the enabled ones).
    stages
        .into_iter()
        .filter(|s| !s.active_flag_false)
        .collect()
}

// ─────────────────────────────────────── lighting ingestion ──────────────────────────────────

/// Normalize a `LightingEffects` block into `Profile.lighting` — the ONE representation, a
/// `Vec<LayerDef>` stack. `basic` -> a single mapped layer (a preset when the named effect has one,
/// else a solid `uniform` in the imported colour). `advanced` -> map each animated `EffectLayer` onto a
/// Neuron compositor layer (fire→heat, colorwheel→radial, reactive→ignite, audiometer→meter, …) AND
/// capture any static layer losslessly as a `custom` per-LED frame layer — nothing is dropped now that
/// every Synapse effect has a host generator (or a custom frame).
fn ingest_lighting(xml: &str, out: &mut Imported) {
    let mode = scalar(xml, "Mode").unwrap_or_default().to_lowercase();
    if mode == "basic" {
        // BASIC named effect -> ONE LayerDef. We reuse the existing effect→preset path
        // (`map_synapse_effect` + `pattern::preset_layer`), re-tinting a colour-driven preset to the
        // imported RzColor. A name with no preset (e.g. "static") falls back to a solid `uniform` in the
        // imported colour (or the house accent when none was carried).
        let effect = scalar(xml, "Effect").unwrap_or_else(|| "static".to_string());
        let color = first_rzcolor(xml);
        let layer = map_synapse_effect(&effect)
            .and_then(crate::pattern::preset_layer)
            .map(|mut l| {
                if synapse_effect_is_colored(&effect) {
                    if let Some(c) = color {
                        l.spectrum = l.spectrum.recolored(c);
                    }
                }
                l
            })
            .unwrap_or_else(|| crate::pattern::LayerDef {
                pattern: "uniform".into(),
                spectrum: crate::spectrum::Spectrum::solid(
                    color.unwrap_or_else(|| Rgb::new(0x4A, 0xF2, 0xB0)),
                ),
                ..Default::default()
            });
        out.profile.lighting = vec![layer];
        return;
    }

    // ADVANCED: a layered composite. Synapse advanced stacks ARE Neuron's compositor — so we map
    // each EffectLayer onto a Neuron Pattern × Spectrum layer (fire→heat, colorwheel→radial, wave→axis,
    // breathing→uniform+breathe, and the live reactive→ignite / audiometer→meter), instead of dropping
    // the lot. A STATIC layer is captured losslessly as a `custom` per-LED frame layer. Only a name we
    // genuinely don't generate is NOTED — never silently discarded.
    let mut layers: Vec<crate::pattern::LayerDef> = Vec::new();
    let mut unsupported: Vec<String> = Vec::new();
    let mut static_name: Option<String> = None;
    for block in split_blocks(xml, "EffectLayer") {
        let Some(eff) = scalar(&block, "Effect") else {
            continue;
        };
        if eff.to_lowercase() == "static" {
            static_name = Some(eff); // handled below as a lossless `custom` frame layer
            continue;
        }
        match map_synapse_effect(&eff) {
            Some(slug) => {
                // build the preset's Pattern × Spectrum layer; a colour-driven effect (one carrying a
                // single meaningful hue) is RE-TINTED to the imported RzColor so the user's hue survives —
                // recolouring the stops, NOT replacing the spectrum, so an effect whose motion lives in its
                // spectrum (breathing → Breathe) keeps animating instead of flattening to a static colour.
                let mut layer = crate::pattern::preset_layer(slug).unwrap_or_default();
                if synapse_effect_is_colored(&eff) {
                    if let Some(c) = first_rzcolor(&block) {
                        layer.spectrum = layer.spectrum.recolored(c);
                    }
                }
                // SCREEN combines lit layers (so a stack reads as light, not the top one only)
                layer.blend = crate::effects::Blend::Screen;
                layers.push(layer);
            }
            None => unsupported.push(eff),
        }
    }
    let had_layers = !layers.is_empty();
    if had_layers {
        out.note(format!(
            "advanced composite imported as {} Neuron layer(s)",
            layers.len()
        ));
    }
    if !unsupported.is_empty() {
        out.note(format!(
            "{} layer(s) have no host pattern yet [{}] — re-author them as a Neuron pattern",
            unsupported.len(),
            unsupported.join(", ")
        ));
    }
    // a static layer survives LOSSLESS as a `custom` frame layer in the stack (the unified representation)
    let mut had_frame = false;
    if let Some(name) = static_name {
        let frame = static_layer_frame(xml, &name);
        if !frame.is_empty() {
            let n = frame.len();
            layers.push(crate::pattern::LayerDef {
                pattern: "custom".into(),
                frame,
                ..Default::default()
            });
            had_frame = true;
            out.note(format!(
                "static layer imported losslessly ({n} per-LED cell(s))"
            ));
        }
    }
    out.profile.lighting = layers;
    if !had_layers && !had_frame {
        out.note(
            "no host-renderable lighting in this composite — only reactive/audio layers"
                .to_string(),
        );
    }
}

/// Map a Synapse advanced-layer effect name to a Neuron PRESET slug (a Pattern × Spectrum look — see
/// [`crate::pattern::presets`]). Every animated Synapse layer now has a host pattern — including the
/// input/audio-driven ones (`reactive`→ignite polls the live keyboard, `audiometer`→meter the live
/// output peak) — so a full Synapse stack comes across whole, nothing dropped. `None` only for a name
/// we genuinely don't generate yet.
fn map_synapse_effect(name: &str) -> Option<&'static str> {
    match name.to_lowercase().as_str() {
        "breathing" => Some("breathing"),
        "spectrum" | "spectrumcycling" => Some("cycle"),
        "colorwheel" | "wheel" => Some("colorwheel"),
        "wave" => Some("wave"),
        "fire" => Some("fire"),
        "starlight" | "stars" => Some("starlight"),
        "reactive" => Some("reactive"),
        "audiometer" | "audio" | "vu" => Some("audiometer"),
        _ => None,
    }
}

/// Whether a Synapse advanced-layer effect carries a meaningful SINGLE colour (so its imported RzColor
/// should become the layer's solid spectrum). The rainbow/ramp effects (wave/colorwheel/cycle/fire) own
/// their palette, so their imported colour is ignored.
fn synapse_effect_is_colored(name: &str) -> bool {
    matches!(
        name.to_lowercase().as_str(),
        "breathing" | "reactive" | "starlight" | "stars"
    )
}

/// Extract a lossless per-LED colour frame from a static `advanced` [`EffectLayer`]. Finds the
/// first layer whose `<Effect>` equals `effect_name`, then reads every `<RzColor>` under it in
/// document order (one per painted cell). Missing channels default to 0 (Synapse omits a 0
/// channel). Returns an empty vec if the layer/colours are absent.
fn static_layer_frame(xml: &str, effect_name: &str) -> Vec<[u8; 3]> {
    let target = effect_name.to_lowercase();
    let layer = split_blocks(xml, "EffectLayer")
        .into_iter()
        .find(|b| scalar(b, "Effect").map(|e| e.to_lowercase()) == Some(target.clone()));
    let Some(layer) = layer else {
        return Vec::new();
    };
    split_blocks(&layer, "RzColor")
        .into_iter()
        .map(|c| {
            let ch = |tag: &str| {
                scalar(&c, tag)
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(0)
            };
            [ch("Red"), ch("Green"), ch("Blue")]
        })
        .collect()
}

/// The first `<RzColor>` in the document, defaulting missing channels to 0 (Synapse omits a
/// channel whose value is 0, e.g. pure green is just `<Green>255</Green>`).
fn first_rzcolor(xml: &str) -> Option<Rgb> {
    let block = split_blocks(xml, "RzColor").into_iter().next()?;
    let ch = |tag: &str| {
        scalar(&block, tag)
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(0)
    };
    Some(Rgb::new(ch("Red"), ch("Green"), ch("Blue")))
}

// ─────────────────────────────────────── mappings ingestion ──────────────────────────────────

/// Parse the Mappings file into spine [`Rule`]s. Each `<Mapping>` is a physical input -> a typed
/// assignment group; we drop identity/default binds (a key -> its own default scancode, or a
/// HyperShift slot left as identity/Disable) and split base vs HyperShift.
fn ingest_mappings(xml: &str, out: &mut Imported) {
    let mut base = 0usize;
    let mut hyper = 0usize;
    let mut dropped_identity = 0usize;
    let mut dropped_unhandled = 0usize;

    for block in split_blocks(xml, "Mapping") {
        let is_hyper = scalar(&block, "IsHyperShift")
            .map(|s| s == "true")
            .unwrap_or(false);

        let Some((trigger, input_default_scancode)) = mapping_trigger(&block, is_hyper) else {
            dropped_unhandled += 1;
            continue;
        };

        let group = scalar(&block, "MappingGroup").unwrap_or_default();
        let action = match mapping_action(&block, &group) {
            ActionOutcome::Action(a) => a,
            ActionOutcome::Identity(scancode) => {
                // A keyboard key mapped to a scancode == its own default = Synapse's default fill.
                // For KeyInput we know the default (HID usage -> scancode); drop only true matches.
                if input_default_scancode == Some(scancode) {
                    dropped_identity += 1;
                    continue;
                }
                // Non-identity remap to a scancode we can't name as a VK -> note + drop.
                dropped_unhandled += 1;
                continue;
            }
            ActionOutcome::Drop => {
                dropped_identity += 1;
                continue;
            }
            ActionOutcome::Unhandled => {
                dropped_unhandled += 1;
                continue;
            }
        };

        if is_hyper {
            hyper += 1;
            // Tag HyperShift binds onto the "hypershift" layer so base vs held-layer is preserved
            // first-class in the output (the engine groups by Rule.layer), not flattened together.
            out.rules
                .push(Rule::on_layer("hypershift", trigger, action));
        } else {
            base += 1;
            out.rules.push(Rule::new(trigger, action));
        }
    }

    out.note(format!(
        "mappings: {} base + {} hypershift bind(s) imported; dropped {} identity/default + {} unhandled",
        base, hyper, dropped_identity, dropped_unhandled
    ));
}

/// Build the [`Trigger`] for a `<Mapping>` block. Returns the trigger plus, for `KeyInput`, the
/// key's DEFAULT scancode (so an assignment to that same scancode can be recognized as identity
/// fill and dropped). HyperShift membership is encoded first-class on the emitted [`Rule`] via
/// `Rule::on_layer("hypershift", …)` (see [`ingest_mappings`]) — the engine groups by that layer
/// tag — so the trigger here is just the physical input; base vs held-layer is preserved by the
/// rule's `layer`, not folded into the trigger.
fn mapping_trigger(block: &str, _is_hyper: bool) -> Option<(Trigger, Option<u16>)> {
    let input_type = scalar(block, "InputType").unwrap_or_default();
    match input_type.as_str() {
        "KeyInput" => {
            let page = scalar(block, "HID_Page")?.parse::<u16>().ok()?;
            let id = scalar(block, "HID_Id")?.parse::<u16>().ok()?;
            let default_scancode = hid_usage_default_scancode(id);
            Some((
                Trigger::Input {
                    page,
                    usage: id,
                    pid: None,
                },
                default_scancode,
            ))
        }
        "MouseInput" => {
            // A named mouse control (LeftClick/Button4/ScrollUp/...). Map to a synthetic usage on
            // the generic-desktop/button page so the spine can carry it; we encode the button
            // ordinal in `usage` and leave page = button page (0x09).
            let name = scalar(block, "MouseInput").unwrap_or_default();
            let usage = mouse_input_usage(&name)?;
            Some((
                Trigger::Input {
                    page: 0x09,
                    usage,
                    pid: None,
                },
                None,
            ))
        }
        "DKMInput" => {
            // A device-key-map slot (DKM_M_01, DKM_SB_03, DKM_RZR, ...). Encode the slot ordinal
            // on a private usage page (0xFF00) so it round-trips as a stable Input trigger.
            let slot = scalar(block, "DKMInput").unwrap_or_default();
            let usage = dkm_slot_usage(&slot)?;
            Some((
                Trigger::Input {
                    page: 0xFF00,
                    usage,
                    pid: None,
                },
                None,
            ))
        }
        _ => None,
    }
}

/// What an assignment group resolved to.
#[derive(Debug)]
enum ActionOutcome {
    Action(Action),
    /// A KeyGroup that just re-types a scancode (carry it so the caller can detect identity fill).
    Identity(u16),
    /// Explicitly drop (Disable group, or a HyperShift identity slot).
    Drop,
    /// A group we recognize but can't yet represent as an Action.
    Unhandled,
}

/// Resolve a `<Mapping>`'s assignment group to an [`Action`].
fn mapping_action(block: &str, group: &str) -> ActionOutcome {
    match group {
        "Disable" => ActionOutcome::Drop,
        "Keyboard" => key_group_action(block),
        "Mouse" => mouse_group_action(block),
        "Multimedia" => match scalar(block, "MultimediaAssignment").as_deref() {
            Some(m) => ActionOutcome::Action(multimedia_action(m)),
            None => ActionOutcome::Unhandled,
        },
        "Win8Shortcuts" => match scalar(block, "WindowsShortcutAssignment").as_deref() {
            Some(s) => ActionOutcome::Action(windows_shortcut_action(s)),
            None => ActionOutcome::Unhandled,
        },
        // Sensitivity / Scrolling / ProfileNavigation = device-internal cycles (DPI cycle,
        // scroll-stage cycle, profile cycle). These are first-class Neuron daemon-intent Actions
        // now (DpiCycle / ScrollStageCycle / ProfileCycle) — no longer silently dropped.
        "Sensitivity" => sensitivity_action(block),
        "Scrolling" => scrolling_action(block),
        "ProfileNavigation" => profile_nav_action(block),
        // A `<Profile>` group binds an input straight to a named profile (Synapse's per-key
        // profile launcher): <ProfileGroup><Name>launcher</Name>. -> Action::ProfileSwitch.
        "Profile" => profile_switch_action(block),
        _ => ActionOutcome::Unhandled,
    }
}

/// `<SensitivityGroup><SensitivityAssignment>DPI_CycleUp/Down/...</>`. -> a DPI Action.
/// Cycle-up/down become [`Action::DpiCycle`]; a `DPI_Stage_N` / `Sensitivity_Clutch` style absolute
/// would become [`Action::DpiSet`] but the real exports only carry cycles, so unknowns note out.
fn sensitivity_action(block: &str) -> ActionOutcome {
    match scalar(block, "SensitivityAssignment").as_deref() {
        Some(a) => {
            let lower = a.to_ascii_lowercase();
            if lower.contains("cycleup") || lower.contains("cycle_up") || lower.contains("up") {
                ActionOutcome::Action(Action::DpiCycle { dir: Direction::Up })
            } else if lower.contains("cycledown")
                || lower.contains("cycle_down")
                || lower.contains("down")
            {
                ActionOutcome::Action(Action::DpiCycle {
                    dir: Direction::Down,
                })
            } else if let Some(dpi) = lower
                .split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
                .and_then(|s| s.parse::<u16>().ok())
            {
                // A "Sensitivity_<dpi>" / "DPI_<value>" style absolute set.
                ActionOutcome::Action(Action::DpiSet { dpi })
            } else {
                ActionOutcome::Unhandled
            }
        }
        None => ActionOutcome::Unhandled,
    }
}

/// `<ScrollingGroup><ScrollingAssignment>Cycle_Up_Scroll_Wheel_Stages/...</>`. -> a scroll-stage
/// cycle Action (a gated daemon intent).
fn scrolling_action(block: &str) -> ActionOutcome {
    match scalar(block, "ScrollingAssignment").as_deref() {
        Some(a) => {
            let lower = a.to_ascii_lowercase();
            if lower.contains("up") {
                ActionOutcome::Action(Action::ScrollStageCycle { dir: Direction::Up })
            } else if lower.contains("down") {
                ActionOutcome::Action(Action::ScrollStageCycle {
                    dir: Direction::Down,
                })
            } else {
                ActionOutcome::Unhandled
            }
        }
        None => ActionOutcome::Unhandled,
    }
}

/// `<ProfileNavigationGroup><ProfileNavigationAssignment>CycleUp/CycleDown</>`. ->
/// [`Action::ProfileCycle`].
fn profile_nav_action(block: &str) -> ActionOutcome {
    match scalar(block, "ProfileNavigationAssignment").as_deref() {
        Some(a) => {
            let lower = a.to_ascii_lowercase();
            if lower.contains("up") {
                ActionOutcome::Action(Action::ProfileCycle { dir: Direction::Up })
            } else if lower.contains("down") {
                ActionOutcome::Action(Action::ProfileCycle {
                    dir: Direction::Down,
                })
            } else {
                ActionOutcome::Unhandled
            }
        }
        None => ActionOutcome::Unhandled,
    }
}

/// `<ProfileGroup><Name>launcher</Name>...`. -> [`Action::ProfileSwitch`] to that named profile,
/// so the keyboard's HID101->"launcher" and DKM_M_02->"other" binds migrate instead of vanishing.
fn profile_switch_action(block: &str) -> ActionOutcome {
    let group = split_blocks(block, "ProfileGroup").into_iter().next();
    match group.as_deref().and_then(|g| scalar(g, "Name")) {
        Some(name) if !name.is_empty() => ActionOutcome::Action(Action::ProfileSwitch { name }),
        _ => ActionOutcome::Unhandled,
    }
}

/// A `<KeyGroup><KeyAssignment>` -> press a key. Prefer the explicit `<VirtualKey>` (custom
/// remaps carry it); fall back to mapping the scancode. Returns `Identity(scancode)` when only a
/// bare scancode is present and no VirtualKey (so the caller can drop default fill).
fn key_group_action(block: &str) -> ActionOutcome {
    let vk = scalar(block, "VirtualKey").and_then(|s| s.parse::<u16>().ok());
    if let Some(vk) = vk {
        if let Some(key) = vk_name(vk) {
            return ActionOutcome::Action(Action::Key { key });
        }
        return ActionOutcome::Unhandled;
    }
    // No VirtualKey: a bare scancode. This is the default-fill pattern (HID key -> its own
    // scancode). Carry the scancode up so identity can be detected against the input's default.
    match scalar(block, "Scancode").and_then(|s| s.parse::<u16>().ok()) {
        Some(sc) => ActionOutcome::Identity(sc),
        None => ActionOutcome::Unhandled,
    }
}

/// A `<MouseGroup><MouseAssignment>` -> a mouse action. Plain Click/Menu/ScrollClick on the
/// native mouse buttons = default fill (drop); Previous/Next/ScrollLeft/Right and the cross-class
/// remaps become keystrokes/notes.
fn mouse_group_action(block: &str) -> ActionOutcome {
    let assign = scalar(block, "MouseAssignment").unwrap_or_default();
    match assign.as_str() {
        // Native button identity (left=click, right=menu, wheel=scrollclick, scroll up/down) =
        // default fill on a mouse-input row. Drop as noise.
        "Click" | "Menu" | "ScrollClick" | "ScrollUp" | "ScrollDown" => ActionOutcome::Drop,
        // Back/Forward and tilt-scroll: real, first-class mouse-button outputs now. Synthesized
        // via SendInput by Action::MouseButton (Back=X1, Forward=X2, tilt=HWHEEL notch).
        "Previous" => ActionOutcome::Action(Action::MouseButton {
            button: MouseButtonKind::Back,
        }),
        "Next" => ActionOutcome::Action(Action::MouseButton {
            button: MouseButtonKind::Forward,
        }),
        "ScrollLeft" => ActionOutcome::Action(Action::MouseButton {
            button: MouseButtonKind::ScrollLeft,
        }),
        "ScrollRight" => ActionOutcome::Action(Action::MouseButton {
            button: MouseButtonKind::ScrollRight,
        }),
        _ => ActionOutcome::Unhandled,
    }
}

/// Map a Synapse MultimediaAssignment to a first-class typed [`Action::Media`] (was previously a
/// stringly `Key { key: "media-*" }`). The typed variant is the clean config-row the GUI edits.
fn multimedia_action(m: &str) -> Action {
    let kind = match m {
        "Play" | "PlayPause" => MediaKind::PlayPause,
        "Stop" => MediaKind::Stop,
        "Next" => MediaKind::Next,
        "Previous" => MediaKind::Prev,
        "VolumeUp" => MediaKind::VolumeUp,
        "VolumeDown" => MediaKind::VolumeDown,
        "Mute" => MediaKind::VolumeMute,
        other => {
            return Action::Run {
                cmd: format!("# multimedia:{other} (unmapped)"),
            }
        }
    };
    Action::Media { key: kind }
}

/// Map a Windows-shortcut assignment to a concrete Action.
fn windows_shortcut_action(s: &str) -> Action {
    match s {
        "CycleApps" => Action::Key {
            key: "alt-tab".to_string(),
        },
        "CloseApp" => Action::Key {
            key: "alt-f4".to_string(),
        },
        other => Action::Run {
            cmd: format!("# winshortcut:{other} (unmapped)"),
        },
    }
}

// ─────────────────────────────────────── input encoders ──────────────────────────────────────

/// Map a named Synapse mouse control to a stable button-page usage ordinal. Keeps a 1:1,
/// reversible numbering so the GUI can show a friendly name back.
fn mouse_input_usage(name: &str) -> Option<u16> {
    Some(match name {
        "LeftClick" => 1,
        "RightClick" => 2,
        "ScrollButton" => 3,
        "Button4" => 4,
        "Button5" => 5,
        "Button6" => 6,
        "Button7" => 7,
        "ScrollUp" => 0x10,
        "ScrollDown" => 0x11,
        "ScrollLeft" => 0x12,
        "ScrollRight" => 0x13,
        _ => return None,
    })
}

/// Map a DKM slot id (DKM_M_01, DKM_SB_03, DKM_RZR, DKM_GAME, DKM_57, ...) to a stable private
/// usage ordinal. We hash the trailing token: numeric suffixes map directly; named slots get a
/// small fixed table.
fn dkm_slot_usage(slot: &str) -> Option<u16> {
    let body = slot.strip_prefix("DKM_")?;
    // DKM_M_01 .. DKM_M_12 -> 0x01..0x0C ; DKM_SB_01.. -> 0x40+n ; DKM_57 -> 57 ; named -> table.
    if let Some(n) = body.strip_prefix("M_") {
        return n.parse::<u16>().ok();
    }
    if let Some(n) = body.strip_prefix("SB_") {
        return n.parse::<u16>().ok().map(|v| 0x40 + v);
    }
    if let Ok(n) = body.parse::<u16>() {
        return Some(0x100 + n); // raw DKM_<n> scancode slots
    }
    Some(match body {
        "RZR" => 0x200,
        "GAME" => 0x201,
        "MACRO" => 0x202,
        "FN" => 0x203,
        "DISP_INT_EXT" => 0x210,
        "DISP_BRIGHT_UP" => 0x211,
        "DISP_BRIGHT_DOWN" => 0x212,
        _ => return None,
    })
}

/// The DEFAULT scancode Synapse assigns to a keyboard HID usage id, for identity-fill detection.
/// This is the standard HID-usage -> PS/2 set-1 scancode table for the printable/function block
/// (the same mapping seen across the base Mappings file). We only need it to recognize "this
/// assignment equals the key's own default" so we can drop the ~100-key default fill.
fn hid_usage_default_scancode(usage: u16) -> Option<u16> {
    // Source: the base (non-hypershift) mappings in the real keyboard export — each HID_Id ->
    // its Scancode. We capture the full A-row..F-row block; modifiers and extended keys included.
    Some(match usage {
        4 => 30,
        5 => 48,
        6 => 46,
        7 => 32,
        8 => 18,
        9 => 33,
        10 => 34,
        11 => 35,
        12 => 23,
        13 => 36,
        14 => 37,
        15 => 38,
        16 => 50,
        17 => 49,
        18 => 24,
        19 => 25,
        20 => 16,
        21 => 19,
        22 => 31,
        23 => 20,
        24 => 22,
        25 => 47,
        26 => 17,
        27 => 45,
        28 => 21,
        29 => 44,
        30 => 2,
        31 => 3,
        32 => 4,
        33 => 5,
        34 => 6,
        35 => 7,
        36 => 8,
        37 => 9,
        38 => 10,
        39 => 11,
        40 => 28,
        41 => 1,
        42 => 14,
        43 => 15,
        44 => 57,
        45 => 12,
        46 => 13,
        47 => 26,
        48 => 27,
        49 => 43,
        50 => 43,
        51 => 39,
        52 => 40,
        53 => 41,
        54 => 51,
        55 => 52,
        56 => 53,
        57 => 58,
        58 => 59,
        59 => 60,
        60 => 61,
        61 => 62,
        62 => 63,
        63 => 64,
        64 => 65,
        65 => 66,
        66 => 67,
        67 => 68,
        68 => 87,
        69 => 88,
        79 => 77,
        80 => 75,
        81 => 80,
        82 => 72,
        85 => 55,
        86 => 74,
        87 => 78,
        89 => 79,
        90 => 80,
        91 => 81,
        92 => 75,
        93 => 76,
        94 => 77,
        95 => 71,
        96 => 72,
        97 => 73,
        98 => 82,
        99 => 83,
        100 => 86,
        103 => 89,
        104 => 100,
        105 => 101,
        106 => 102,
        107 => 103,
        108 => 104,
        109 => 105,
        110 => 106,
        111 => 107,
        112 => 108,
        113 => 109,
        114 => 110,
        115 => 118,
        224 => 29,
        225 => 42,
        226 => 56,
        229 => 54,
        _ => return None,
    })
}

/// Map a Windows virtual-key code (from `<VirtualKey>`) to a Neuron key name the [`Action::Key`]
/// executor understands (`vk_for` is the inverse). Covers letters, digits, common named keys, and
/// modifiers — enough for the real custom binds (W/A/S/D, J, H, E, Space, Ctrl/Alt).
fn vk_name(vk: u16) -> Option<String> {
    // Letters A-Z (0x41..0x5A) and digits 0-9 (0x30..0x39) map to their char.
    if (0x41..=0x5A).contains(&vk) {
        return Some(((vk as u8) as char).to_ascii_lowercase().to_string());
    }
    if (0x30..=0x39).contains(&vk) {
        return Some(((vk as u8) as char).to_string());
    }
    if (0x70..=0x87).contains(&vk) {
        return Some(format!("f{}", vk - 0x70 + 1)); // VK_F1..VK_F24
    }
    Some(
        match vk {
            0x0D => "enter",
            0x20 => "space",
            0x09 => "tab",
            0x1B => "esc",
            0x08 => "backspace",
            0x10 | 0xA0 | 0xA1 => "shift", // VK_SHIFT / L / R
            0x11 | 0xA2 | 0xA3 => "ctrl",  // VK_CONTROL / L / R
            0x12 | 0xA4 | 0xA5 => "alt",   // VK_MENU / L / R
            0x26 => "up",
            0x28 => "down",
            0x25 => "left",
            0x27 => "right",
            _ => return None,
        }
        .to_string(),
    )
}

// ─────────────────────────────────────── XML helpers ─────────────────────────────────────────

/// The most a single ZIP member may DECOMPRESS to. `.ChromaEffects`/`.synapse3` files come off the
/// internet, and a zip bomb turns a few KB on disk into GBs in memory via `read_to_end` — real
/// exports' XML members are tens of KB, so 16 MiB is orders of magnitude of headroom while keeping
/// a hostile file from exhausting memory.
const MAX_MEMBER_BYTES: u64 = 16 * 1024 * 1024;

/// Read a ZIP member by exact name to a UTF-8 string (lossy). `None` if absent/unreadable — or if
/// it inflates past [`MAX_MEMBER_BYTES`] (a bomb is "unreadable", never a partial parse: truncating
/// mid-document would hand the XML layer a corrupted record and call it the file's content).
fn read_member(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, name: &str) -> Option<String> {
    let f = zip.by_name(name).ok()?;
    // Reject on the DECLARED size first (cheap), then cap the actual read too — the declared size
    // is attacker-controlled metadata and may lie small.
    if f.size() > MAX_MEMBER_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    // +1 so a stream that lies about its size and runs past the cap is detected (the extra byte
    // arrives) instead of silently truncated at exactly the cap.
    f.take(MAX_MEMBER_BYTES + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > MAX_MEMBER_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Read the first member whose name ends with `suffix` (e.g. "DeviceInfo.xml", which may be at a
/// nested path).
fn read_member_ending(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, suffix: &str) -> Option<String> {
    let name = zip.file_names().find(|n| n.ends_with(suffix))?.to_string();
    read_member(zip, &name)
}

/// Extract the trimmed text of the FIRST `<tag>...</tag>` in `xml`. Lightweight pull-parse — the
/// schema is shallow per-block, and we slice blocks first with [`split_blocks`] for nested fields.
/// Returns `None` for self-closing or absent tags.
fn scalar(xml: &str, tag: &str) -> Option<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut buf = Vec::new();
    let mut in_tag = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) if e.name().as_ref() == tag.as_bytes() => in_tag = true,
            Ok(Event::Text(t)) if in_tag => {
                return t
                    .unescape()
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
            }
            Ok(Event::End(e)) if e.name().as_ref() == tag.as_bytes() => {
                // Start immediately followed by End (e.g. `<Active />` was Empty; an empty
                // `<Tag></Tag>` lands here) -> no text.
                return None;
            }
            Ok(Event::Eof) => return None,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }
}

/// Slice `xml` into the inner-text of each top-level `<tag>...</tag>` occurrence (depth-aware, so
/// nested `<tag>` inside a `<tag>` doesn't split early). Returns each block's INNER XML (between
/// the matched open and its paired close), so callers can run [`scalar`] within one record.
fn split_blocks(xml: &str, tag: &str) -> Vec<String> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut blocks = Vec::new();
    // Stack of (depth-at-open) for active captures; we only capture the OUTERMOST matching tag to
    // avoid duplicating nested same-name records (e.g. CenterPoint inside CenterPoint).
    let mut capture_depth: Option<usize> = None;
    let mut depth = 0usize;
    let mut current = String::new();
    let target = tag.as_bytes();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let is_target = e.name().as_ref() == target;
                if capture_depth.is_some() {
                    // Re-emit nested markup verbatim into the current block.
                    current.push('<');
                    current.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                    current.push('>');
                }
                if is_target && capture_depth.is_none() {
                    capture_depth = Some(depth);
                    current.clear();
                }
                depth += 1;
            }
            Ok(Event::End(e)) => {
                // saturating: quick_xml's name-checking rejects a stray close before we see it,
                // but this parser must never be one config-default away from a usize underflow
                // panic on attacker-supplied XML.
                depth = depth.saturating_sub(1);
                if let Some(cd) = capture_depth {
                    if e.name().as_ref() == target && depth == cd {
                        blocks.push(std::mem::take(&mut current));
                        capture_depth = None;
                    } else {
                        current.push_str("</");
                        current.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                        current.push('>');
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                if capture_depth.is_some() {
                    current.push('<');
                    current.push_str(&String::from_utf8_lossy(e.name().as_ref()));
                    current.push_str("/>");
                }
            }
            Ok(Event::Text(t)) => {
                if capture_depth.is_some() {
                    current.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    blocks
}

// ─────────────────────────────────────────── tests ───────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // The real decoded Synapse exports live on the user's Desktop. These tests parse them
    // directly and assert ground-truth values. They're gated on the files existing so the suite
    // still passes on machines without them (CI), but run live on the dev box.
    const SYNX: &str = r"C:\Users\<user>\Desktop\_synx"; // keyboard .synapse3 (extracted)
    const MOUSE: &str = r"C:\Users\<user>\Desktop\_mouse"; // Naga .synapse3 (extracted)
    const SYNXL: &str = r"C:\Users\<user>\Desktop\_synxL"; // .ChromaEffects (extracted)

    fn read(path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    #[test]
    fn guid_table_resolves_all_known_features() {
        assert_eq!(
            FeatureGuid::from_guid("bc7fc799-b384-4bf0-ae25-7839eb32611e"),
            Some(FeatureGuid::Dpi)
        );
        assert_eq!(
            FeatureGuid::from_guid("25f22ab7-9be1-4b4c-8002-f619b7b7b949"),
            Some(FeatureGuid::DpiStages)
        );
        assert_eq!(
            FeatureGuid::from_guid("762555eb-82f2-4fa6-9741-e009d579f188"),
            Some(FeatureGuid::Mappings)
        );
        assert_eq!(
            FeatureGuid::from_guid("6C91BF99-UPPER"),
            Some(FeatureGuid::LedBrightness)
        ); // case-insensitive
        assert_eq!(FeatureGuid::from_guid("deadbeef-0000"), None);
    }

    #[test]
    fn scalar_extracts_first_tag_text() {
        let xml = "<A><X>800</X><Y>800</Y></A>";
        assert_eq!(scalar(xml, "X").as_deref(), Some("800"));
        assert_eq!(scalar(xml, "Z"), None);
        // green-only RzColor (missing channels)
        let c = "<RzColor><Green>255</Green></RzColor>";
        assert_eq!(scalar(c, "Green").as_deref(), Some("255"));
        assert_eq!(scalar(c, "Red"), None);
    }

    #[test]
    fn split_blocks_is_depth_aware() {
        let xml = "<L><S><X>1</X></S><S><X>2</X></S></L>";
        let blocks = split_blocks(xml, "S");
        assert_eq!(blocks.len(), 2);
        assert_eq!(scalar(&blocks[0], "X").as_deref(), Some("1"));
        assert_eq!(scalar(&blocks[1], "X").as_deref(), Some("2"));
    }

    #[test]
    fn first_rzcolor_defaults_missing_channels_to_zero() {
        let xml = "<Colors><RzColor><Green>255</Green></RzColor></Colors>";
        assert_eq!(first_rzcolor(xml), Some(Rgb::new(0, 255, 0)));
    }

    #[test]
    fn vk_name_covers_real_custom_binds() {
        assert_eq!(vk_name(87).as_deref(), Some("w")); // W
        assert_eq!(vk_name(65).as_deref(), Some("a")); // A
        assert_eq!(vk_name(83).as_deref(), Some("s")); // S
        assert_eq!(vk_name(68).as_deref(), Some("d")); // D
        assert_eq!(vk_name(74).as_deref(), Some("j")); // J
        assert_eq!(vk_name(72).as_deref(), Some("h")); // H
        assert_eq!(vk_name(69).as_deref(), Some("e")); // E
        assert_eq!(vk_name(32).as_deref(), Some("space"));
        assert_eq!(vk_name(160).as_deref(), Some("shift")); // VK_LSHIFT
        assert_eq!(vk_name(162).as_deref(), Some("ctrl")); // VK_LCONTROL
    }

    #[test]
    fn dpi_stages_keep_only_enabled() {
        let Some(xml) = read(&format!(
            r"{MOUSE}\Features\b04fe61a-a9d0-45e0-9dd5-9433cf1ca357\25f22ab7-9be1-4b4c-8002-f619b7b7b949.xml"
        )) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let stages = parse_dpi_stages(&xml);
        let dpis: Vec<u16> = stages.iter().map(|s| s.dpi).collect();
        // Two enabled stages: 800 and 30000 (the three Active=false slots dropped).
        assert_eq!(
            dpis,
            vec![800, 30000],
            "enabled DPI stages should be 800 & 30000"
        );
    }

    #[test]
    fn mouse_export_imports_dpi_brightness_and_wasd() {
        let Some(_) = read(&format!(r"{MOUSE}\DeviceInfo.xml")) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let bytes = zip_dir(MOUSE);
        let imp = import_synapse3(&bytes).expect("import mouse");

        // DPI active = 800, stages = [800, 30000].
        assert_eq!(imp.profile.dpi, Some(800), "active DPI");
        assert_eq!(imp.profile.dpi_stages, vec![800, 30000], "DPI stage list");
        // Brightness 33.
        assert_eq!(imp.profile.brightness, Some(33), "mouse brightness");
        // Polling 1000.
        assert_eq!(imp.profile.polling_hz, Some(1000), "polling");

        // The thumb-grid WASD binds: HID usages 36/37/38/34 -> a/s/d/w via VirtualKey.
        let key_of = |usage: u16| -> Option<String> {
            imp.rules
                .iter()
                .find_map(|r| match (&r.trigger, &r.action) {
                    (
                        Trigger::Input {
                            page: 7, usage: u, ..
                        },
                        Action::Key { key },
                    ) if *u == usage => Some(key.clone()),
                    _ => None,
                })
        };
        assert_eq!(key_of(34).as_deref(), Some("w"), "thumb HID 34 -> W");
        assert_eq!(key_of(36).as_deref(), Some("a"), "thumb HID 36 -> A");
        assert_eq!(key_of(37).as_deref(), Some("s"), "thumb HID 37 -> S");
        assert_eq!(key_of(38).as_deref(), Some("d"), "thumb HID 38 -> D");
    }

    #[test]
    fn keyboard_export_imports_brightness_gamingmode_and_custom_binds() {
        let Some(_) = read(&format!(r"{SYNX}\DeviceInfo.xml")) else {
            eprintln!("skip: keyboard export not present");
            return;
        };
        let bytes = zip_dir(SYNX);
        let imp = import_synapse3(&bytes).expect("import keyboard");

        assert_eq!(
            imp.profile.name, "gamer ++ J",
            "profile name from Profiles/<guid>.xml"
        );
        assert_eq!(imp.profile.brightness, Some(65), "keyboard brightness");
        // GamingMode (DisableAltTab) surfaces as a note.
        assert!(
            imp.notes.iter().any(|n| n.contains("gaming mode")),
            "gaming mode noted"
        );

        // The keyboard's only real custom binds are the 4 DKM remaps:
        //   DKM_M_01 -> CycleApps (alt-tab), DKM_M_03 -> J, DKM_M_04 -> Play, DKM_M_05 -> CloseApp.
        let has_alt_tab = imp
            .rules
            .iter()
            .any(|r| matches!(&r.action, Action::Key { key } if key == "alt-tab"));
        let has_j = imp
            .rules
            .iter()
            .any(|r| matches!(&r.action, Action::Key { key } if key == "j"));
        let has_close = imp
            .rules
            .iter()
            .any(|r| matches!(&r.action, Action::Key { key } if key == "alt-f4"));
        // DKM_M_04 -> Play now imports as the first-class typed Action::Media (was a stringly
        // Key { key: "media-play-pause" } before Phase-2's Media variant).
        let has_play = imp.rules.iter().any(|r| {
            matches!(
                &r.action,
                Action::Media {
                    key: MediaKind::PlayPause
                }
            )
        });
        assert!(has_alt_tab, "DKM_M_01 -> CycleApps/alt-tab");
        assert!(has_j, "DKM_M_03 -> J");
        assert!(has_close, "DKM_M_05 -> CloseApp/alt-f4");
        assert!(has_play, "DKM_M_04 -> Play (typed Media)");

        // The ~100-key default fill + the all-identity HyperShift fill must be dropped: the
        // keyboard has only a handful of real binds, not 100+.
        assert!(
            imp.rules.len() < 12,
            "default/identity fill dropped (got {} rules)",
            imp.rules.len()
        );
    }

    /// Every animated Synapse effect now maps to a Neuron compositor layer — including the live
    /// reactive/audiometer ones — so a synthetic fire+reactive+audiometer+colorwheel stack comes
    /// across whole (4 layers), nothing dropped. Synthetic so it runs on CI (no user file needed).
    #[test]
    fn chroma_advanced_maps_every_animated_layer() {
        let xml = r#"<LightingEffects><Mode>advanced</Mode><EffectLayers>
            <EffectLayer><Effect>fire</Effect></EffectLayer>
            <EffectLayer><Effect>reactive</Effect>
                <Colors><RzColor><Green>255</Green></RzColor></Colors></EffectLayer>
            <EffectLayer><Effect>audiometer</Effect>
                <Colors><RzColor><Blue>255</Blue></RzColor></Colors></EffectLayer>
            <EffectLayer><Effect>colorwheel</Effect></EffectLayer>
        </EffectLayers></LightingEffects>"#;
        let mut out = Imported::default();
        ingest_lighting(xml, &mut out);
        // each Synapse effect maps onto its preset's PATTERN: fire→heat, reactive→ignite,
        // audiometer→meter, colorwheel→radial — landing directly in the profile's layer stack.
        let pats: Vec<&str> = out
            .profile
            .lighting
            .iter()
            .map(|l| l.pattern.as_str())
            .collect();
        assert_eq!(
            pats,
            vec!["heat", "ignite", "meter", "radial"],
            "all 4 animated layers mapped to their patterns"
        );
        // the colour-driven reactive layer took the imported green as a solid spectrum.
        assert_eq!(
            out.profile.lighting[1].spectrum,
            crate::spectrum::Spectrum::solid(Rgb::new(0, 255, 0)),
            "reactive recoloured to the imported RzColor"
        );
        // none are unsupported -> no "no host pattern" note.
        assert!(
            !out.notes.iter().any(|n| n.contains("no host pattern")),
            "nothing dropped; notes={:?}",
            out.notes
        );
        // every mapped layer builds a real pattern.
        for l in &out.profile.lighting {
            assert!(
                crate::pattern::make_pattern(&l.pattern).is_some(),
                "{} resolves to a pattern",
                l.pattern
            );
        }
    }

    /// `map_synapse_effect` resolves every Synapse advanced effect name (case-insensitive) to a
    /// Neuron preset slug — the guarantee behind "nothing dropped".
    #[test]
    fn synapse_effect_names_all_resolve() {
        for (synapse, slug) in [
            ("Fire", "fire"),
            ("Reactive", "reactive"),
            ("Audiometer", "audiometer"),
            ("ColorWheel", "colorwheel"),
            ("Wheel", "colorwheel"),
            ("Starlight", "starlight"),
            ("Wave", "wave"),
            ("Breathing", "breathing"),
            ("Spectrum", "cycle"),
            ("SpectrumCycling", "cycle"),
        ] {
            assert_eq!(
                map_synapse_effect(synapse),
                Some(slug),
                "{synapse} -> {slug}"
            );
            assert!(
                crate::pattern::preset_by_slug(slug).is_some(),
                "{slug} is a real preset"
            );
        }
    }

    #[test]
    fn basic_lighting_imports_named_effect_and_color() {
        // Mouse LightingEffects = basic / static / green.
        let xml = r#"<LightingEffects><Mode>basic</Mode><Effect>static</Effect>
            <Colors><RzColor><Green>255</Green></RzColor></Colors></LightingEffects>"#;
        let mut out = Imported::default();
        ingest_lighting(xml, &mut out);
        // a basic "static" effect has no preset -> one solid `uniform` layer in the imported colour.
        assert_eq!(out.profile.lighting.len(), 1);
        assert_eq!(out.profile.lighting[0].pattern, "uniform");
        assert_eq!(
            out.profile.lighting[0].spectrum,
            crate::spectrum::Spectrum::solid(Rgb::new(0, 255, 0)),
            "pure green"
        );
    }

    // ── Phase-2 fixes: real-file ground truth ──────────────────────────────────────────────────

    /// Fix #1: the live DPI = the TOP-LEVEL `<Active>0</Active>` index into the enabled stage list
    /// (-> 800), NOT the first per-stage `<Active>false</Active>` (which the old code parsed).
    #[test]
    fn dpi_top_level_active_index_parsed() {
        // Synthetic, version-independent: enabled stages have NO <Active>; disabled have
        // <Active>false</Active>; the file-level <Active> is the index into the enabled list.
        let xml = r#"<DPIStages><Stages>
            <DPIStage><X>800</X><Y>800</Y></DPIStage>
            <DPIStage><X>1600</X><Y>1600</Y></DPIStage>
            <DPIStage><X>3200</X><Y>3200</Y><Active>false</Active></DPIStage>
        </Stages><Active>1</Active></DPIStages>"#;
        assert_eq!(
            top_level_dpi_active(xml),
            Some(1),
            "top-level index, not per-stage false"
        );
        let mut out = Imported::default();
        ingest_feature(FeatureGuid::DpiStages, xml, &mut out);
        assert_eq!(out.profile.dpi_stages, vec![800, 1600], "enabled stages");
        assert_eq!(
            out.profile.dpi,
            Some(1600),
            "active index 1 -> second enabled stage"
        );

        // The real mouse export: enabled [800, 30000], top-level Active=0 -> 800.
        if let Some(real) = read(&format!(
            r"{MOUSE}\Features\b04fe61a-a9d0-45e0-9dd5-9433cf1ca357\25f22ab7-9be1-4b4c-8002-f619b7b7b949.xml"
        )) {
            assert_eq!(
                top_level_dpi_active(&real),
                Some(0),
                "real file top-level Active=0"
            );
            let mut o = Imported::default();
            ingest_feature(FeatureGuid::DpiStages, &real, &mut o);
            assert_eq!(
                o.profile.dpi,
                Some(800),
                "real active = 800 (index 0), not a per-stage flag"
            );
        }
    }

    /// Fix #2: the active HyperScroll stage is the `<Mode>` inside `<ActiveScrollStage>`
    /// (FreeScroll), not the first `<Mode>` in the document (Adaptive).
    #[test]
    fn scroll_stage_active_is_freescroll_not_adaptive() {
        let Some(xml) = read(&format!(
            r"{MOUSE}\Features\b04fe61a-a9d0-45e0-9dd5-9433cf1ca357\429f8d88-59df-4b73-b50b-a6326feee35e.xml"
        )) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        assert_eq!(
            active_scroll_mode(&xml).as_deref(),
            Some("FreeScroll"),
            "active = ActiveScrollStage Mode"
        );
        let mut out = Imported::default();
        ingest_feature(FeatureGuid::ScrollWheelStages, &xml, &mut out);
        let note = out
            .notes
            .iter()
            .find(|n| n.contains("scroll-wheel stages"))
            .expect("scroll note");
        assert!(
            note.contains("FreeScroll"),
            "note names the real active stage: {note}"
        );
        assert!(
            !note.contains("Adaptive"),
            "must NOT report the first Mode (Adaptive): {note}"
        );
    }

    /// Fix #3: the keyboard's Profile-group binds (HID101->"launcher", DKM_M_02->"other") migrate
    /// as Action::ProfileSwitch instead of vanishing.
    #[test]
    fn keyboard_profile_switch_binds_present() {
        let Some(_) = read(&format!(r"{SYNX}\DeviceInfo.xml")) else {
            eprintln!("skip: keyboard export not present");
            return;
        };
        let imp = import_synapse3(&zip_dir(SYNX)).expect("import keyboard");
        let switches: Vec<&str> = imp
            .rules
            .iter()
            .filter_map(|r| match &r.action {
                Action::ProfileSwitch { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            switches.contains(&"launcher"),
            "HID101 -> profile 'launcher'; got {switches:?}"
        );
        assert!(
            switches.contains(&"other"),
            "DKM_M_02 -> profile 'other'; got {switches:?}"
        );
    }

    /// Profile-switch resolution directly (synthetic, both InputTypes).
    #[test]
    fn profile_group_resolves_to_profile_switch() {
        let block = r#"<MappingGroup>Profile</MappingGroup><InputType>KeyInput</InputType>
            <KeyInput><HID_Page>7</HID_Page><HID_Id>101</HID_Id></KeyInput>
            <ProfileGroup><Name>launcher</Name><ProfileId>x</ProfileId></ProfileGroup>"#;
        match mapping_action(block, "Profile") {
            ActionOutcome::Action(Action::ProfileSwitch { name }) => assert_eq!(name, "launcher"),
            other => panic!("expected ProfileSwitch, got {other:?}"),
        }
    }

    /// Fix #4: imported HyperShift binds are tagged onto the "hypershift" layer (base vs held-layer
    /// preserved first-class, regroup-able by `Engine::from_rules`).
    #[test]
    fn mouse_hypershift_binds_tagged_on_layer() {
        let Some(_) = read(&format!(r"{MOUSE}\DeviceInfo.xml")) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let imp = import_synapse3(&zip_dir(MOUSE)).expect("import mouse");
        let hyper = imp
            .rules
            .iter()
            .filter(|r| r.layer.as_deref() == Some("hypershift"))
            .count();
        let base = imp.rules.iter().filter(|r| r.layer.is_none()).count();
        assert!(
            hyper > 0,
            "at least one hypershift-tagged rule (got {hyper})"
        );
        assert!(base > 0, "base rules still present (got {base})");
        // The engine must group them: hypershift rules land in the layer, base in the base set.
        let eng = crate::engine::Engine::from_rules(imp.rules.clone());
        assert_eq!(
            eng.layers.get("hypershift").map(|v| v.len()),
            Some(hyper),
            "layer grouped"
        );
        assert_eq!(eng.rules.len(), base, "base grouped");
    }

    /// Fix #5: the device-internal cycle/mouse groups emit the new first-class Action variants
    /// (no more silent drops). Tested at the resolver level against the real Synapse strings.
    #[test]
    fn cycle_and_mouse_groups_emit_new_variants() {
        // Sensitivity DPI_CycleUp -> DpiCycle{Up}
        let sens = r#"<SensitivityGroup><SensitivityAssignment>DPI_CycleUp</SensitivityAssignment></SensitivityGroup>"#;
        assert!(matches!(
            mapping_action(sens, "Sensitivity"),
            ActionOutcome::Action(Action::DpiCycle { dir: Direction::Up })
        ));
        // Scrolling Cycle_Up_Scroll_Wheel_Stages -> ScrollStageCycle{Up}
        let scr = r#"<ScrollingGroup><ScrollingAssignment>Cycle_Up_Scroll_Wheel_Stages</ScrollingAssignment></ScrollingGroup>"#;
        assert!(matches!(
            mapping_action(scr, "Scrolling"),
            ActionOutcome::Action(Action::ScrollStageCycle { dir: Direction::Up })
        ));
        // ProfileNavigation CycleUp -> ProfileCycle{Up}
        let nav = r#"<ProfileNavigationGroup><ProfileNavigationAssignment>CycleUp</ProfileNavigationAssignment></ProfileNavigationGroup>"#;
        assert!(matches!(
            mapping_action(nav, "ProfileNavigation"),
            ActionOutcome::Action(Action::ProfileCycle { dir: Direction::Up })
        ));
        // Mouse Previous/Next -> Back/Forward; tilt -> ScrollLeft/Right.
        let prev = "<MouseGroup><MouseAssignment>Previous</MouseAssignment></MouseGroup>";
        assert!(matches!(
            mapping_action(prev, "Mouse"),
            ActionOutcome::Action(Action::MouseButton {
                button: MouseButtonKind::Back
            })
        ));
        let next = "<MouseGroup><MouseAssignment>Next</MouseAssignment></MouseGroup>";
        assert!(matches!(
            mapping_action(next, "Mouse"),
            ActionOutcome::Action(Action::MouseButton {
                button: MouseButtonKind::Forward
            })
        ));
        let tilt = "<MouseGroup><MouseAssignment>ScrollLeft</MouseAssignment></MouseGroup>";
        assert!(matches!(
            mapping_action(tilt, "Mouse"),
            ActionOutcome::Action(Action::MouseButton {
                button: MouseButtonKind::ScrollLeft
            })
        ));
        // Multimedia Play -> typed Action::Media (not a stringly Key any more).
        let mm = "<MultimediaAssignment>Play</MultimediaAssignment>";
        assert!(matches!(
            mapping_action(mm, "Multimedia"),
            ActionOutcome::Action(Action::Media {
                key: MediaKind::PlayPause
            })
        ));
    }

    /// The real mouse export carries the cycle binds; they now appear as real Actions in the
    /// imported rule set (base or hypershift), not as drops.
    #[test]
    fn mouse_export_emits_cycle_actions() {
        let Some(_) = read(&format!(r"{MOUSE}\DeviceInfo.xml")) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let imp = import_synapse3(&zip_dir(MOUSE)).expect("import mouse");
        let has = |pred: fn(&Action) -> bool| imp.rules.iter().any(|r| pred(&r.action));
        assert!(
            has(|a| matches!(a, Action::DpiCycle { .. })),
            "DPI cycle present"
        );
        assert!(
            has(|a| matches!(a, Action::ScrollStageCycle { .. })),
            "scroll-stage cycle present"
        );
        assert!(
            has(|a| matches!(a, Action::ProfileCycle { .. })),
            "profile cycle present"
        );
        assert!(
            has(|a| matches!(a, Action::MouseButton { .. })),
            "mouse back/forward/tilt present"
        );
    }

    /// Fix #6a: LedPowerSettings IdleStateValue -> Profile.idle_secs (60s on the real mouse).
    #[test]
    fn led_power_populates_idle_secs() {
        let Some(xml) = read(&format!(
            r"{MOUSE}\Features\b04fe61a-a9d0-45e0-9dd5-9433cf1ca357\a8664fc4-37d2-4bb7-978f-5ad11d7383ef.xml"
        )) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let mut out = Imported::default();
        ingest_feature(FeatureGuid::LedPowerSettings, &xml, &mut out);
        assert_eq!(out.profile.idle_secs, Some(60), "idle-off after 60s");
    }

    /// Fix #6b: GamingMode DisableAltTabState=1 -> Profile.disable_alt_tab (keyboard export).
    #[test]
    fn gaming_mode_populates_disable_alt_tab() {
        let Some(xml) = read(&format!(
            r"{SYNX}\Features\432bbcf0-95f4-4493-a54a-b1ee288dbc69\a04163a1-1bc0-4155-bdb6-55385f77472a.xml"
        )) else {
            eprintln!("skip: keyboard export not present");
            return;
        };
        let mut out = Imported::default();
        ingest_feature(FeatureGuid::GamingMode, &xml, &mut out);
        assert!(
            out.profile.disable_alt_tab,
            "DisableAltTabState=1 -> disable_alt_tab"
        );
        // The empty <GamingMode/> mouse file sets nothing.
        let mut empty = Imported::default();
        ingest_feature(FeatureGuid::GamingMode, "<GamingMode />", &mut empty);
        assert!(
            !empty.profile.disable_alt_tab,
            "empty gaming mode = no override"
        );
    }

    /// Fix #6c: InGamePollingRate -> Profile.in_game_polling lossless pair (1000/1000 on mouse).
    #[test]
    fn in_game_polling_populates_pair() {
        let Some(xml) = read(&format!(
            r"{MOUSE}\Features\b04fe61a-a9d0-45e0-9dd5-9433cf1ca357\8997620a-f08d-49bd-aaec-0c5b0e55bac2.xml"
        )) else {
            eprintln!("skip: mouse export not present");
            return;
        };
        let mut out = Imported::default();
        ingest_feature(FeatureGuid::InGamePollingRate, &xml, &mut out);
        assert_eq!(
            out.profile.in_game_polling,
            Some((1000, 1000)),
            "wired/dongle pair"
        );
        assert_eq!(
            out.profile.polling_hz,
            Some(1000),
            "wired seeds plain poll rate"
        );
    }

    /// Fix #6d: an advanced STATIC frame imports losslessly as a first-class `custom` layer in the
    /// profile's lighting stack (one [R,G,B] per painted cell) instead of flattening to one colour.
    #[test]
    fn advanced_static_frame_is_lossless() {
        let xml = r#"<LightingEffects><Mode>advanced</Mode><EffectLayers>
            <EffectLayer><Effect>static</Effect><Regions><EffectRegion><Colors>
                <RzColor><Red>255</Red></RzColor>
                <RzColor><Green>255</Green></RzColor>
                <RzColor><Blue>255</Blue></RzColor>
            </Colors></EffectRegion></Regions></EffectLayer>
        </EffectLayers></LightingEffects>"#;
        let mut out = Imported::default();
        ingest_lighting(xml, &mut out);
        // one custom layer whose frame is the per-LED cells, verbatim.
        let custom = out
            .profile
            .lighting
            .iter()
            .find(|l| l.pattern == "custom")
            .expect("static layer became a custom layer");
        assert_eq!(
            custom.frame,
            vec![[255, 0, 0], [0, 255, 0], [0, 0, 255]],
            "per-LED frame preserved losslessly on the custom layer"
        );
    }

    /// The user's real .ChromaEffects export (fire + reactive + audiometer + colorwheel, no static
    /// layer) now comes across WHOLE as a Neuron compositor stack — every animated layer mapped, a
    /// composite lighting mode set, and (since no layer is static) no per-LED frame fabricated.
    #[test]
    fn real_chroma_imports_full_compositor_stack() {
        let Some(xml) = read(&format!(
            r"{SYNXL}\3e2682be-b765-4e08-886c-5151dfe061ed.xml"
        )) else {
            eprintln!("skip: chroma export not present");
            return;
        };
        let mut out = Imported::default();
        ingest_lighting(&xml, &mut out);
        assert!(
            !out.profile.lighting.is_empty(),
            "advanced stack mapped to compositor layers"
        );
        // every mapped layer builds a real pattern (nothing left dangling).
        for l in &out.profile.lighting {
            assert!(
                crate::pattern::make_pattern(&l.pattern).is_some(),
                "layer {} resolves",
                l.pattern
            );
        }
        assert!(
            !out.profile.lighting.iter().any(|l| l.pattern == "custom"),
            "no static layer -> no custom frame layer"
        );
        assert!(
            !out.notes.iter().any(|n| n.contains("no host generator")),
            "nothing dropped; notes={:?}",
            out.notes
        );
    }

    /// A lightweight, dependency-free **property test** for the XML scalar/block helpers: for many
    /// randomized (tag, value) records, `scalar` round-trips the first value and `split_blocks`
    /// recovers each record's inner scalar. Deterministic LCG so it's reproducible (no proptest
    /// dep — the frozen dep set stays unchanged).
    #[test]
    fn prop_scalar_and_split_blocks_round_trip() {
        // simple deterministic PRNG
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let tags = ["X", "Y", "Mode", "Value", "Name", "Brightness", "Order"];
        for _ in 0..500 {
            let tag = tags[(next() as usize) % tags.len()];
            // value: digits or a short safe word (no XML-special chars)
            let val = if next() % 2 == 0 {
                (next() % 100000).to_string()
            } else {
                let words = ["FreeScroll", "Adaptive", "static", "launcher", "alpha"];
                words[(next() as usize) % words.len()].to_string()
            };
            // scalar finds the first occurrence's text.
            let single = format!("<R><{tag}>{val}</{tag}><Other>z</Other></R>");
            assert_eq!(
                scalar(&single, tag).as_deref(),
                Some(val.as_str()),
                "scalar first-text"
            );

            // split_blocks recovers each record and scalar reads inside it.
            let n = 1 + (next() % 3) as usize;
            let mut doc = String::from("<L>");
            let mut expected = Vec::new();
            for i in 0..n {
                let v = format!("{val}{i}");
                doc.push_str(&format!("<S><{tag}>{v}</{tag}></S>"));
                expected.push(v);
            }
            doc.push_str("</L>");
            let blocks = split_blocks(&doc, "S");
            assert_eq!(blocks.len(), n, "block count for {doc}");
            for (b, want) in blocks.iter().zip(&expected) {
                assert_eq!(
                    scalar(b, tag).as_deref(),
                    Some(want.as_str()),
                    "inner scalar in {doc}"
                );
            }
        }
    }

    /// Re-zip an extracted export dir in-memory so the import path (which expects a ZIP) can run
    /// against the on-disk extracted fixtures.
    fn zip_dir(root: &str) -> Vec<u8> {
        use std::io::Write;
        let mut cur = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut cur);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            let base = Path::new(root);
            for entry in walk(base) {
                let rel = entry
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                if let Ok(data) = std::fs::read(&entry) {
                    zw.start_file(rel, opts).unwrap();
                    zw.write_all(&data).unwrap();
                }
            }
            zw.finish().unwrap();
        }
        cur.into_inner()
    }

    /// Recursively list files under `dir`.
    fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    out.extend(walk(&p));
                } else {
                    out.push(p);
                }
            }
        }
        out
    }

    // ── ADVERSARIAL input — `.ChromaEffects` files come off the internet ─────────────────────
    // Everything above parses TRUSTED fixtures (the user's own exports). These feed the import
    // path what an attacker would: a zip bomb, truncated/garbage archives, unbalanced XML. The
    // properties are always the same two — never panic, never let a hostile file masquerade as
    // a usable import.

    /// A tiny-on-disk archive whose one XML member INFLATES far past [`MAX_MEMBER_BYTES`]. The
    /// member must be rejected (not read into memory, not parsed) — the import completes with
    /// nothing ingested rather than ballooning to the inflated size.
    #[test]
    fn zip_bomb_member_is_rejected_not_inflated() {
        use std::io::Write;
        let mut cur = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut cur);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            zw.start_file("boom.xml", opts).unwrap();
            // VALID lighting XML up front, then 64 MiB of padding (deflates to ~64 KiB on the
            // wire, 4× past the inflate cap). The valid prefix is the tripwire: if the cap ever
            // regresses, the member inflates, the prefix PARSES, and the assertion below sees an
            // ingested layer — "rejected" and "inflated-but-harmless" are distinguishable.
            zw.write_all(
                b"<LightingEffects><Mode>basic</Mode><Effect>static</Effect>\
                  <Colors><RzColor><Green>255</Green></RzColor></Colors></LightingEffects>",
            )
            .unwrap();
            let pad = vec![b' '; 1024 * 1024];
            for _ in 0..64 {
                zw.write_all(&pad).unwrap();
            }
            zw.finish().unwrap();
        }
        let bytes = cur.into_inner();
        assert!(
            bytes.len() < 1024 * 1024,
            "the bomb must be small on the wire for this test to mean anything ({} bytes)",
            bytes.len()
        );
        let out = import_chroma_effects(&bytes).expect("a rejected member is skipped, not a crash");
        assert!(
            out.profile.lighting.is_empty(),
            "nothing from the bomb may be ingested as content"
        );
    }

    #[test]
    fn non_zip_and_truncated_zip_fail_loudly_never_panic() {
        // plain garbage
        assert!(import_chroma_effects(b"this is not a zip archive").is_err());
        // empty input
        assert!(import_chroma_effects(&[]).is_err());
        // a real archive cut in half — the central directory (at the tail) is gone
        let mut cur = Cursor::new(Vec::new());
        {
            use std::io::Write;
            let mut zw = zip::ZipWriter::new(&mut cur);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file("effect.xml", opts).unwrap();
            zw.write_all(b"<LightingList></LightingList>").unwrap();
            zw.finish().unwrap();
        }
        let whole = cur.into_inner();
        assert!(import_chroma_effects(&whole[..whole.len() / 2]).is_err());
        // the ZIP magic followed by garbage — passes the sniff, fails the parse
        let mut fake = b"PK\x03\x04".to_vec();
        fake.extend_from_slice(&[0xA5; 512]);
        assert!(import_chroma_effects(&fake).is_err());
    }

    /// Unbalanced / hostile XML through the block splitter and scalar puller: stray closes,
    /// deep nesting, interleaved tags, NUL-laden text. The parsers are iterative and quick_xml
    /// name-checking rejects mismatches — this pins that NO shape panics or hangs, including the
    /// stray-close case that a `usize` depth underflow would have turned into a crash.
    #[test]
    fn hostile_xml_never_panics_the_block_splitter() {
        let cases: &[&str] = &[
            "</S></S></S>",                            // closes with no opens
            "<S>",                                     // open with no close
            "<S><S><S></S>",                           // under-closed nesting
            "<S></X>",                                 // mismatched close
            "<S><V>1</V></S></S><S><V>2</V></S>",      // stray close BETWEEN records
            "\u{0}\u{0}<S>\u{0}</S>",                  // NULs
            "<S V=\"<S>\"></S>",                       // tag-in-attribute
        ];
        for xml in cases {
            let _ = split_blocks(xml, "S");
            let _ = scalar(xml, "S");
        }
        // deep nesting — iterative parse must survive 10k levels without recursion or panic
        let mut deep = String::new();
        for _ in 0..10_000 {
            deep.push_str("<S>");
        }
        for _ in 0..10_000 {
            deep.push_str("</S>");
        }
        let _ = split_blocks(&deep, "S");
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
        /// ANY byte string through the whole import entry point: never a panic, never an OOM —
        /// only `Ok` (something parseable was salvaged) or a loud `Err`.
        #[test]
        fn arbitrary_bytes_never_panic_the_importer(bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4096)) {
            let _ = import_chroma_effects(&bytes);
        }
    }
}
