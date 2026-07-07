//! Data-driven device registry. Device definitions are TOML — built-ins are embedded,
//! and any `devices/*.toml` next to the working dir is loaded too (extend without recompile).

use anyhow::Result;
use serde::Deserialize;
use std::collections::BTreeMap;

/// A single vendor command: class / id / requested data size, plus any fixed leading
/// argument bytes (e.g. varstore + led_id for lighting).
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub class: u8,
    pub id: u8,
    pub size: u8,
    #[serde(default)]
    pub args: Vec<u8>,
    /// Optional per-command transaction_id override. Most commands use the device-default
    /// [`DeviceDef::transaction_id`]; a few command families (e.g. the Chroma V2's lighting
    /// EFFECT / CUSTOM-FRAME writes, which need 0x3F while its getters use 0xFF) wire a
    /// different tx. When `None`, the device default is used — so devices that set no override
    /// are byte-identical to before.
    #[serde(default)]
    pub transaction_id: Option<u8>,
}

/// One USB link-mode personality (wired / dongle / bluetooth) with its PID.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct Mode {
    pub name: String,
    pub product_id: u16,
}

/// How to pick the vendor control collection out of a composite device.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct ControlInterface {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_report_len: u16,
}

/// A complete device definition (the unit you add to support a new device).
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct DeviceDef {
    pub name: String,
    pub codename: String,
    pub vendor_id: u16,
    pub transaction_id: u8,
    /// Microseconds the transport must WAIT between a feature-report SET and the GET that drains its
    /// reply — the command's round-trip + processing time. A wireless dongle is SLOW here: reading the
    /// reply (or firing the next command) before that completes OVERRUNS the device and drops frames —
    /// the lighting FLICKER. OpenRazer calibrates this per receiver (a "new mouse receiver" like the
    /// Naga V2 Pro's dongle ≈ 31000µs; a wired board ≈ 0). The fast streaming write
    /// ([`crate::device::Device::send_lighting_fast`]) sleeps this. `0` (the default) = no wait, correct
    /// for wired/legacy boards, which are paced slowly anyway.
    #[serde(default)]
    pub stream_wait_us: u64,
    pub modes: Vec<Mode>,
    pub control_interface: ControlInterface,
    pub commands: BTreeMap<String, CommandSpec>,
    /// Optional unified-lighting wiring (the device-specific dialect of class 0x03 / 0x0F).
    #[serde(default)]
    pub lighting: Option<crate::lighting::LightingDef>,
    /// Optional swappable SIDE-PLATE id→label map (e.g. the Naga V2 Pro's magnetic side plates).
    /// The plate is detected ONLY via a device-PUSHED HID report (`05 0e <strap_id>`) on the input
    /// interface — there is NO feature-getter (a full getter sweep + swap-diff confirmed zero
    /// change), so the report itself IS the detection (decoded in the app's `hidwatch`). This table
    /// is the DATA half of that: hardware strap-code → human label. Keys are the strap-codes as
    /// strings (TOML table keys are strings); `0`/none = "detached", handled in code, so it need not
    /// appear here. Devices without swappable plates simply omit the table.
    #[serde(default)]
    pub side_plates: Option<BTreeMap<String, String>>,
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

    /// Does this device have a swappable SIDE-PLATE map (a `[side_plates]` table)? The push-only
    /// plate detection (see [`DeviceDef::side_plates`]) is the gate for surfacing the plate readout.
    pub fn has_side_plates(&self) -> bool {
        self.side_plates.is_some()
    }

    /// Resolve a side-plate hardware strap-code to its human label via the `[side_plates]` DATA map
    /// (never a hardcoded id→label table in logic). `None` for an unknown code or a device with no
    /// plates — the caller decides how to degrade (the decode site shows a transparent "plate N").
    /// Strap-code `0` (none/detached) is handled at the call site, so it is absent from the map.
    pub fn side_plate_label(&self, id: u8) -> Option<&str> {
        self.side_plates
            .as_ref()?
            .get(&id.to_string())
            .map(|s| s.as_str())
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
        // SetBrightness has TWO honest write paths — the matrix top-level command OR the
        // lighting block's brightness spec (the legacy dialect; see capability::set_brightness).
        // Either satisfies it, so a legacy keyboard doesn't read as "can't set brightness".
        if cap == Capability::SetBrightness
            && self
                .lighting
                .as_ref()
                .is_some_and(|l| l.brightness.is_some())
        {
            return true;
        }
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

/// Is `def` fully SUBSUMED by already-loaded defs — i.e. does every (vendor, pid) it declares
/// already resolve? Load order is trust order (builtins → devices/ → devices/auto/), and
/// `find_by_pid` is first-match-wins per pid — so a fully-covered def is dead weight (the
/// curated-shadows-auto rule), while a def bringing ANY new pid must load even if its human
/// name collides (auto defs are NAMED from the USB product string, which legitimately repeats
/// across revisions and link modes; a name was never an identity).
fn subsumed(existing: &[DeviceDef], def: &DeviceDef) -> bool {
    // Every pid this def brings must already be covered by some existing def of the same vendor.
    // An empty def (no modes) declares no pid and resolves nothing — treat it as subsumed so the
    // all()-over-empty vacuous-true is the intended answer, not an accident.
    def.product_ids().all(|pid| {
        existing
            .iter()
            .any(|d| d.vendor_id == def.vendor_id && d.product_ids().any(|p| p == pid))
    })
}

impl Registry {
    pub fn load() -> Result<Self> {
        let mut devices = Vec::new();

        // Built-in definitions (embedded so the binary is self-contained). Dev/test escape:
        // NEURON_PURE_DISCOVERY=1 skips them, forcing EVERY device through the emergent path
        // (probe → `crate::synth` → devices/auto/) — the live end-to-end test for auto-adoption
        // on hardware that normally has a curated def. Off (unset) in any real run.
        if std::env::var("NEURON_PURE_DISCOVERY").map(|v| v != "1").unwrap_or(true) {
            const BUILTINS: &[&str] = &[
                include_str!("../devices/razer-naga-v2-pro.toml"),
                include_str!("../devices/razer-blackwidow-chroma-v2.toml"),
            ];
            for src in BUILTINS {
                devices.push(toml::from_str::<DeviceDef>(src)?);
            }
        }

        // Optional external definitions: extend coverage without recompiling. Load order is
        // trust order — `find_by_pid` is first-match-wins, so curated `devices/*.toml` shadow
        // the auto-synthesized `devices/auto/*.toml` (see `crate::synth`) for the same pid.
        // Shadowing is BY PID, never by name: a def loads unless it adds no new pid at all
        // (`subsumed`). Auto defs are named from the USB product string, which repeats across
        // revisions/link-modes, so a name collision must NOT drop a def that brings a fresh pid.
        for dir in ["devices", "devices/auto"] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.extension().is_some_and(|x| x == "toml") && p.is_file() {
                        if let Ok(txt) = std::fs::read_to_string(&p) {
                            if let Ok(def) = toml::from_str::<DeviceDef>(&txt) {
                                if !subsumed(&devices, &def) {
                                    devices.push(def);
                                }
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
        // SetBrightness is satisfied by the LIGHTING BLOCK's brightness spec (the legacy
        // dialect) — the board has no top-level set_brightness command yet CAN set brightness.
        assert!(bw.supports(Capability::SetBrightness));
        assert!(!bw.supports(Capability::Brightness), "no getter — the readout stays hidden");
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
    fn side_plates_map_is_data_driven_and_graceful() {
        let (naga, bw) = builtins();
        // the Naga V2 Pro ships the swappable side-plate map; the keyboard has none.
        assert!(naga.has_side_plates());
        assert!(!bw.has_side_plates());
        // id → label resolves straight from the [side_plates] TOML table (the DATA, not logic).
        // These are hardware STRAP-CODES (verified live), NOT button counts.
        assert_eq!(naga.side_plate_label(1), Some("2-button"));
        assert_eq!(naga.side_plate_label(3), Some("12-button"));
        assert_eq!(naga.side_plate_label(4), Some("6-button"));
        // an UNKNOWN strap-code degrades gracefully to None (the decode site shows "plate N", never
        // a wrong label). 0/detached is intentionally absent (handled in code).
        assert_eq!(naga.side_plate_label(0), None);
        assert_eq!(naga.side_plate_label(2), None);
        assert_eq!(naga.side_plate_label(9), None);
        // a device with no plate map never claims one.
        assert_eq!(bw.side_plate_label(3), None);
    }

    /// A minimal-but-valid DeviceDef: just the identity fields `subsumed`/`find_by_pid` read
    /// (vendor + a single mode pid), everything else stubbed. Distinct `codename` so end-to-end
    /// resolution can tell two same-NAMED defs apart — the whole point of the fix.
    fn mini(name: &str, codename: &str, vid: u16, pid: u16) -> DeviceDef {
        let src = format!(
            "name = \"{name}\"\n\
             codename = \"{codename}\"\n\
             vendor_id = {vid}\n\
             transaction_id = 0x1F\n\
             [[modes]]\n\
             name = \"wired\"\n\
             product_id = {pid}\n\
             [control_interface]\n\
             usage_page = 1\n\
             usage = 2\n\
             feature_report_len = 91\n\
             [commands]\n"
        );
        toml::from_str(&src).unwrap()
    }

    #[test]
    fn subsumed_is_by_pid_not_by_name() {
        let (naga, _) = builtins();
        // (a) THE review scenario: a def with the SAME human name as the Naga builtin but a pid
        // the Naga never declares (a new revision / different link-mode pid). Name collides, pid
        // is fresh — it MUST NOT be dropped.
        let same_name_new_pid = mini(&naga.name, "Ghost", naga.vendor_id, 0x0999);
        assert!(
            !subsumed(&[naga.clone()], &same_name_new_pid),
            "a fresh pid must load even when the display name collides"
        );

        // (b) curated-shadows-auto: re-parsing the same builtin brings no new pid → dead weight.
        let (naga_again, _) = builtins();
        assert!(
            subsumed(&[naga.clone()], &naga_again),
            "a fully-covered def is subsumed (first-match-wins already resolves its pids)"
        );

        // (c) partial overlap: one already-covered pid + one brand-new pid. It STILL loads — it
        // brings a new pid, and first-match-wins per pid handles the shared one.
        let partial: DeviceDef = toml::from_str(&format!(
            "name = \"partial\"\n\
             codename = \"Ghost\"\n\
             vendor_id = {}\n\
             transaction_id = 0x1F\n\
             [[modes]]\n\
             name = \"shared\"\n\
             product_id = 0x00A7\n\
             [[modes]]\n\
             name = \"fresh\"\n\
             product_id = 0x0999\n\
             [control_interface]\n\
             usage_page = 1\n\
             usage = 2\n\
             feature_report_len = 91\n\
             [commands]\n",
            naga.vendor_id
        ))
        .unwrap();
        assert!(
            !subsumed(&[naga.clone()], &partial),
            "a def bringing ANY new pid is not subsumed"
        );
    }

    #[test]
    fn find_by_pid_resolves_both_same_named_defs() {
        // (d) end-to-end resolution SHAPE: a registry holding the Naga plus a same-named def that
        // brings a new pid resolves BOTH pids — each to the right def. The old name-dedupe would
        // have dropped the second def, leaving 0x0999 unresolved ("permanently unadopted").
        let (naga, _) = builtins();
        let same_name_new_pid = mini(&naga.name, "Ghost", naga.vendor_id, 0x0999);
        let reg = Registry {
            devices: vec![naga.clone(), same_name_new_pid],
        };
        // The Naga's own pid resolves to the Naga (first match wins for a shared/curated pid).
        let a = reg.find_by_pid(naga.vendor_id, 0x00A7).unwrap();
        assert_eq!(a.codename, "Aria");
        // The new pid resolves to the second def — proving it was NOT dropped for the name clash.
        let b = reg.find_by_pid(naga.vendor_id, 0x0999).unwrap();
        assert_eq!(b.codename, "Ghost");
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
