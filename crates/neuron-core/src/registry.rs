//! Data-driven device registry. Device definitions are TOML — built-ins are embedded,
//! and any `devices/*.toml` next to the working dir is loaded too (extend without recompile).

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A single vendor command: class / id / requested data size, plus any fixed leading
/// argument bytes (e.g. varstore + led_id for lighting).
#[derive(Debug, Deserialize, Clone)]
pub struct CommandSpec {
    pub class: u8,
    pub id: u8,
    pub size: u8,
    #[serde(default)]
    pub args: Vec<u8>,
}

/// One USB link-mode personality (wired / dongle / bluetooth) with its PID.
#[derive(Debug, Deserialize, Clone)]
pub struct Mode {
    pub name: String,
    pub product_id: u16,
}

/// How to pick the vendor control collection out of a composite device.
#[derive(Debug, Deserialize, Clone)]
pub struct ControlInterface {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_report_len: u16,
}

/// A complete device definition (the unit you add to support a new device).
#[derive(Debug, Deserialize, Clone)]
pub struct DeviceDef {
    pub name: String,
    pub codename: String,
    pub vendor_id: u16,
    pub transaction_id: u8,
    pub modes: Vec<Mode>,
    pub control_interface: ControlInterface,
    pub commands: BTreeMap<String, CommandSpec>,
    /// Optional unified-lighting wiring (the device-specific dialect of class 0x03 / 0x0F).
    #[serde(default)]
    pub lighting: Option<crate::lighting::LightingDef>,
}

impl DeviceDef {
    pub fn product_ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.modes.iter().map(|m| m.product_id)
    }
    pub fn mode_for(&self, pid: u16) -> Option<&Mode> {
        self.modes.iter().find(|m| m.product_id == pid)
    }
    pub fn command(&self, name: &str) -> Option<&CommandSpec> {
        self.commands.get(name)
    }
    pub fn matches_control(&self, usage_page: u16, usage: u16, feature_len: u16) -> bool {
        let c = &self.control_interface;
        c.usage_page == usage_page && c.usage == usage && c.feature_report_len == feature_len
    }

    /// Does this device's registry expose a command under `name`? The registry-driven answer to
    /// "can I run this on this device" — cheaper and more honest than a hardcoded per-device match.
    /// Callers (CLI/GUI) use it to grey-out a control the device can't do rather than firing a
    /// command that will time out or return Unsupported.
    pub fn has_command(&self, name: &str) -> bool {
        self.commands.contains_key(name)
    }

    /// Does this device expose a given semantic [`Capability`]? This is the additive, registry-driven
    /// way to ask "does this device support feature X" without each caller hardcoding the command
    /// name. It maps the capability to the registry command(s) it needs (a capability is present iff
    /// every required command is present), plus the structural facts (lighting block) that aren't
    /// commands. Purely a *read* over the registry — it changes nothing in the proven write path.
    pub fn supports(&self, cap: Capability) -> bool {
        cap.required_commands().iter().all(|c| self.has_command(c))
            && (!cap.requires_lighting() || self.lighting.is_some())
    }

    /// Every semantic [`Capability`] this device currently exposes (registry-driven). Lets a GUI
    /// enumerate "what can this device actually do" without probing hardware or hardcoding a table.
    pub fn capabilities(&self) -> Vec<Capability> {
        Capability::ALL
            .iter()
            .copied()
            .filter(|&c| self.supports(c))
            .collect()
    }
}

/// A semantic device capability — the *meaning* of a feature, decoupled from the per-device
/// class/id opcodes (which live in the registry TOML). [`DeviceDef::supports`] resolves a capability
/// to the registry command(s) it needs, so callers can ask "does this device do DPI-stage writes?"
/// in one place instead of re-deriving the command name. ADDITIVE: this does not move the proven
/// opcode constants out of [`crate::writes`]/[`crate::capability`]; it only adds a registry-driven
/// presence check on top of the existing command map.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Read the current sensitivity (DPI).
    Dpi,
    /// Write a single DPI value (proven 0x04/0x05).
    SetDpi,
    /// Read the configured DPI-stage list.
    DpiStages,
    /// Write the DPI-stage list / cycle (proven 0x04/0x06).
    SetDpiStages,
    /// Read the legacy polling-rate divisor.
    Polling,
    /// Write the legacy polling-rate divisor (0x00/0x05).
    SetPolling,
    /// Hi-res (HyperPolling) read (0x00/0xC0).
    Polling2,
    /// Hi-res (HyperPolling) write up to 8000Hz (0x00/0x40).
    SetPolling2,
    /// Select the active scroll-wheel stage (wire-confirmed 0x15/0x00).
    SetScrollStage,
    /// Read lighting brightness.
    Brightness,
    /// Write lighting brightness.
    SetBrightness,
    /// Battery level read.
    Battery,
    /// Onboard storage pool (macros/profiles live here).
    Storage,
    /// Any unified lighting (a `[lighting]` block is present).
    Lighting,
}

impl Capability {
    /// All capabilities, for enumeration ([`DeviceDef::capabilities`]).
    pub const ALL: [Capability; 14] = [
        Capability::Dpi,
        Capability::SetDpi,
        Capability::DpiStages,
        Capability::SetDpiStages,
        Capability::Polling,
        Capability::SetPolling,
        Capability::Polling2,
        Capability::SetPolling2,
        Capability::SetScrollStage,
        Capability::Brightness,
        Capability::SetBrightness,
        Capability::Battery,
        Capability::Storage,
        Capability::Lighting,
    ];

    /// The registry command name(s) this capability needs (all must be present). The single source
    /// that maps a semantic capability to the proven opcode names in the device TOMLs.
    pub fn required_commands(self) -> &'static [&'static str] {
        match self {
            Capability::Dpi => &["dpi"],
            Capability::SetDpi => &["set_dpi"],
            Capability::DpiStages => &["dpi_stages"],
            Capability::SetDpiStages => &["set_dpi_stages"],
            Capability::Polling => &["polling_rate"],
            Capability::SetPolling => &["set_polling"],
            Capability::Polling2 => &["polling2"],
            Capability::SetPolling2 => &["set_polling2"],
            Capability::SetScrollStage => &["set_scroll_stage"],
            Capability::Brightness => &["brightness"],
            Capability::SetBrightness => &["set_brightness"],
            Capability::Battery => &["battery_level"],
            Capability::Storage => &["storage_info"],
            Capability::Lighting => &[],
        }
    }

    /// Whether the capability additionally requires a `[lighting]` block (a structural fact, not a
    /// command). Only [`Capability::Lighting`] does.
    pub fn requires_lighting(self) -> bool {
        matches!(self, Capability::Lighting)
    }

    /// A short, human label for the capability (GUI/CLI surfacing).
    pub fn label(self) -> &'static str {
        match self {
            Capability::Dpi => "DPI (read)",
            Capability::SetDpi => "DPI (set)",
            Capability::DpiStages => "DPI stages (read)",
            Capability::SetDpiStages => "DPI stages (set)",
            Capability::Polling => "polling rate (read)",
            Capability::SetPolling => "polling rate (set)",
            Capability::Polling2 => "hi-res polling (read)",
            Capability::SetPolling2 => "hi-res polling (set)",
            Capability::SetScrollStage => "scroll stage (set)",
            Capability::Brightness => "brightness (read)",
            Capability::SetBrightness => "brightness (set)",
            Capability::Battery => "battery",
            Capability::Storage => "onboard storage",
            Capability::Lighting => "lighting",
        }
    }
}

pub struct Registry {
    pub devices: Vec<DeviceDef>,
}

impl Registry {
    pub fn load() -> Result<Self> {
        let mut devices = Vec::new();

        // Built-in definitions (embedded so the binary is self-contained).
        const BUILTINS: &[&str] = &[
            include_str!("../devices/razer-naga-v2-pro.toml"),
            include_str!("../devices/razer-blackwidow-chroma-v2.toml"),
        ];
        for src in BUILTINS {
            devices.push(toml::from_str::<DeviceDef>(src)?);
        }

        // Optional external definitions: extend coverage without recompiling.
        if let Ok(rd) = std::fs::read_dir("devices") {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().is_some_and(|x| x == "toml") {
                    if let Ok(txt) = std::fs::read_to_string(&p) {
                        if let Ok(def) = toml::from_str::<DeviceDef>(&txt) {
                            if !devices.iter().any(|d| d.name == def.name) {
                                devices.push(def);
                            }
                        }
                    }
                }
            }
        }

        Ok(Registry { devices })
    }

    pub fn find_by_pid(&self, vid: u16, pid: u16) -> Option<&DeviceDef> {
        self.devices
            .iter()
            .find(|d| d.vendor_id == vid && d.product_ids().any(|p| p == pid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse just the two embedded builtins (no `devices/` dir, no I/O) so capability tests are
    /// hermetic and deterministic regardless of the working directory.
    fn builtins() -> (DeviceDef, DeviceDef) {
        let naga: DeviceDef =
            toml::from_str(include_str!("../devices/razer-naga-v2-pro.toml")).unwrap();
        let bw: DeviceDef =
            toml::from_str(include_str!("../devices/razer-blackwidow-chroma-v2.toml")).unwrap();
        (naga, bw)
    }

    #[test]
    fn has_command_is_registry_driven() {
        let (naga, bw) = builtins();
        // The proven Naga opcodes are present by name.
        assert!(naga.has_command("set_dpi_stages"));
        assert!(naga.has_command("set_scroll_stage"));
        assert!(naga.has_command("set_polling2"));
        assert!(naga.has_command("storage_info"));
        // The keyboard has none of those (no onboard storage / no mouse perf).
        assert!(!bw.has_command("set_dpi_stages"));
        assert!(!bw.has_command("storage_info"));
        // A name no device defines.
        assert!(!naga.has_command("totally_not_a_command"));
    }

    #[test]
    fn supports_maps_capabilities_to_commands() {
        let (naga, bw) = builtins();
        // Naga = the full perf mouse: DPI, stages, hi-res polling, scroll stage, storage.
        assert!(naga.supports(Capability::SetDpi));
        assert!(naga.supports(Capability::SetDpiStages));
        assert!(naga.supports(Capability::SetPolling2));
        assert!(naga.supports(Capability::SetScrollStage));
        assert!(naga.supports(Capability::Storage));
        assert!(naga.supports(Capability::Battery));
        assert!(naga.supports(Capability::Lighting));
        // Keyboard = lighting only; no mouse perf, no onboard storage, no battery.
        assert!(bw.supports(Capability::Lighting));
        assert!(!bw.supports(Capability::SetDpi));
        assert!(!bw.supports(Capability::SetDpiStages));
        assert!(!bw.supports(Capability::SetScrollStage));
        assert!(!bw.supports(Capability::Storage));
        assert!(!bw.supports(Capability::Battery));
    }

    #[test]
    fn lighting_capability_needs_the_lighting_block_not_a_command() {
        // Capability::Lighting has no required command — it keys off the [lighting] block, which
        // both builtins have. Prove the structural gate works by stripping the block.
        let (mut naga, _) = builtins();
        assert!(naga.supports(Capability::Lighting));
        naga.lighting = None;
        assert!(
            !naga.supports(Capability::Lighting),
            "no [lighting] block -> not supported"
        );
    }

    #[test]
    fn capabilities_enumerates_only_present_features() {
        let (naga, bw) = builtins();
        let naga_caps = naga.capabilities();
        // Mouse exposes strictly more than the keyboard.
        assert!(naga_caps.contains(&Capability::SetDpiStages));
        assert!(naga_caps.contains(&Capability::Storage));
        let bw_caps = bw.capabilities();
        assert!(bw_caps.contains(&Capability::Lighting));
        assert!(!bw_caps.contains(&Capability::Storage));
        assert!(
            bw_caps.len() < naga_caps.len(),
            "keyboard does strictly less than the mouse"
        );
    }

    #[test]
    fn capability_required_commands_reference_real_registry_names() {
        // Guard against drift: every command name a capability claims must actually exist in at
        // least one shipped device (so the mapping can never silently rot to a typo).
        let (naga, bw) = builtins();
        for cap in Capability::ALL {
            for name in cap.required_commands() {
                assert!(
                    naga.has_command(name) || bw.has_command(name),
                    "capability {cap:?} references unknown command '{name}'",
                );
            }
        }
    }

    #[test]
    fn capability_labels_are_nonempty_and_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for cap in Capability::ALL {
            let l = cap.label();
            assert!(!l.is_empty());
            assert!(seen.insert(l), "duplicate capability label: {l}");
        }
        assert_eq!(seen.len(), Capability::ALL.len());
    }
}
