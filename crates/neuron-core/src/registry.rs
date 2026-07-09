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

/// The serde default for [`DeviceDef::dialect`]: every TOML that predates the dialect field
/// speaks razer_report, so an absent key means "razer".
fn default_dialect() -> String {
    "razer".into()
}

/// Where a def was loaded from — TRUST provenance. Builtin/Curated are board-verified data a
/// self-heal must never rewrite; Auto is synthesized config the system may improve in place
/// (e.g. the first-light tx heal). serde(skip): origin is a LOAD fact, never file content.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum DefOrigin {
    #[default]
    Builtin,
    Curated,
    Auto,
}

/// One device-PUSHED HID event vocabulary (an `[events]` TOML block) — the DATA half of the
/// per-device report map `hidwatch`'s module header says "should move to the registry". The arming
/// triple (`usage_page`/`usage`/`feature_len`) pins WHICH collection the pushes ride (e.g. the Seiren
/// V3 Mini's Consumer-Control collection, distinct from its vendor control pipe); `reports` maps the
/// pushed report's first two bytes (lowercase hex, e.g. `"0511"`) to the semantic [`EventKind`] it
/// carries.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq)]
pub struct EventMap {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    reports: BTreeMap<String, EventKind>,
}

/// A semantic device-pushed event. Deserialized from its TOML string name (`rename_all =
/// "snake_case"`) — an unregistered name (an event typo in a device TOML) is a serde LOAD ERROR, never
/// a silent vanish, the same fail-loud stance [`Registry::load`] takes on an unroutable `dialect`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// An AUDIO device's capacitive tap-mute toggled (the Seiren V3 Mini family's `05 11 <state>`
    /// push; `state` 0=live, 1=muted).
    MuteState,
}

/// A complete device definition (the unit you add to support a new device).
#[derive(Debug, Deserialize, Clone, PartialEq)]
pub struct DeviceDef {
    pub name: String,
    pub codename: String,
    /// The wire-protocol FAMILY this def speaks — resolved to a [`crate::dialect::Dialect`] impl by
    /// [`crate::dialect::by_id`]. Serde-defaults to "razer" so every existing TOML (all of which
    /// predate dialects) stays valid and razer-spoken; a non-razer board sets it explicitly (e.g.
    /// `dialect = "hidpp"`). Bytes live behind the dialect; semantics (Capability) stay above it.
    #[serde(default = "default_dialect")]
    pub dialect: String,
    /// TRUST provenance — where [`Registry::load`] read this def from (never a file field, hence
    /// `serde(skip)`: a reload always re-derives it from the load path). The first-light self-heal
    /// gates on it: only [`DefOrigin::Auto`] defs may be rewritten in place, so a curated/builtin
    /// board's board-verified bytes are never clobbered by an inference the heal proved wrong.
    #[serde(skip)]
    pub origin: DefOrigin,
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
    /// Optional device-PUSHED event vocabulary (an `[events]` table). Devices that push no such
    /// reports simply omit the table (`None`) — no every-device tax for a Naga-only or Seiren-only
    /// behaviour.
    #[serde(default)]
    pub events: Option<EventMap>,
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
    /// Is `info` the CONTROL pipe this def drives? Which HID collection of a composite device is the
    /// control pipe is DIALECT knowledge, not a universal triple compare: razer matches by the
    /// `usage_page`/`usage`/`feature_report_len` triple stored in the def; HID++ matches by report
    /// SHAPE (VID + 7/20-byte output/input reports) and its stored feature length is meaningless.
    /// Route through the family seam so no caller has to know which rule applies. FAIL CLOSED on an
    /// unknown dialect (Finding 2's sibling): a def whose family we can't identify matches NOTHING —
    /// it must never be selected and opened into bytes we can't safely frame.
    pub fn matches_control(&self, info: &crate::transport::HidDeviceInfo) -> bool {
        crate::dialect::by_id(&self.dialect).is_some_and(|d| d.matches_control(self, info))
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

    /// Decode a device-PUSHED report against this def's `[events]` vocabulary. Matches on `report`'s
    /// first two bytes only (the report-kind lead byte + sub-kind byte, e.g. `05 11`) — any further
    /// length/shape policy for the resolved [`EventKind`] (e.g. reading `report[2]` for MuteState's
    /// state bit) belongs to the caller, not this lookup. `None` for a too-short report, a device with
    /// no `[events]` block, or a lead-byte pair the vocabulary doesn't name.
    pub fn event_for(&self, report: &[u8]) -> Option<EventKind> {
        if report.len() < 2 {
            return None;
        }
        let key = format!("{:02x}{:02x}", report[0], report[1]);
        self.events.as_ref()?.reports.get(&key).copied()
    }

    /// Does this def's `[events]` vocabulary ride the collection (`page`, `usage`, `flen`)? The arming
    /// check a collection-enumeration filter (`hidwatch`'s) tests before spawning a reader — false for
    /// a device with no `[events]` block, so a def that never pushes anything never arms one.
    pub fn event_pipe_matches(&self, page: u16, usage: u16, flen: u16) -> bool {
        self.events
            .as_ref()
            .is_some_and(|e| e.usage_page == page && e.usage == usage && e.feature_len == flen)
    }

    /// Does this def's `[events]` vocabulary declare `kind` at all (any collection)? The CAPABILITY
    /// check for a UI gate ("is this device's mute hardware-owned") — distinct from
    /// `event_pipe_matches`, which tests the arming collection rather than the vocabulary's contents.
    pub fn has_event(&self, kind: EventKind) -> bool {
        self.events.as_ref().is_some_and(|e| e.reports.values().any(|k| *k == kind))
    }

    /// Does this def expose ANY operable control surface (a command to run, or a lighting block to
    /// paint)? A def can be DATA-ONLY — the Seiren V3 Mini's carries just an `[events]` vocabulary
    /// for hidwatch, with an honest empty `[commands]` — and such a def must not grow a device-list
    /// row: its user-facing face is its Core-Audio endpoint row, and a second, knob-less HID row is
    /// exactly the double-listing the unclaimed-footnote rework removed.
    pub fn is_operable(&self) -> bool {
        !self.commands.is_empty() || self.lighting.is_some()
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
    /// Read the keyboard's FIRMWARE game mode (the FN+F10 Win-key kill / GAME_LED state).
    GameMode,
    /// Write the keyboard's firmware game mode (the Win-key kill) — the getter verifies the write.
    SetGameMode,
}

impl Capability {
    /// All capabilities, for enumeration ([`DeviceDef::capabilities`]).
    pub const ALL: [Capability; 16] = [
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
        Capability::GameMode,
        Capability::SetGameMode,
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
            Capability::GameMode => &["game_mode"],
            Capability::SetGameMode => &["set_game_mode"],
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
            Capability::GameMode => "game mode (read)",
            Capability::SetGameMode => "game mode (set)",
        }
    }
}

pub struct Registry {
    pub devices: Vec<DeviceDef>,
}

/// Is `def` fully SUBSUMED by already-loaded defs — i.e. does every (vendor, pid) it declares
/// already resolve WITHIN ITS OWN WIRE FAMILY? Load order is trust order (builtins → devices/ →
/// devices/auto/), and `find_for_pipe` is first-match-wins per (pid, family) — so a fully-covered
/// def is dead weight (the curated-shadows-auto rule), while a def bringing ANY new (pid, dialect)
/// must load even if its human name collides (auto defs are NAMED from the USB product string,
/// which legitimately repeats across revisions and link modes; a name was never an identity).
///
/// The FAMILY dimension is load-bearing: identity moved from `(vid, pid) → one def` to
/// `(dialect, vid, pid)` (the adoption ledgers already key on it). Two families sharing one pid —
/// one physical device speaking two protocols on different pipes (the razer-audio-sidecar future
/// this repo documents) — are NOT subsumption partners: each drives a different control pipe via
/// its own `matches_control` rule, so BOTH must load or the second family is permanently
/// unopenable. Subsumption (curated-shadows-auto) stays exactly as before WITHIN a single family.
fn subsumed(existing: &[DeviceDef], def: &DeviceDef) -> bool {
    // Every pid this def brings must already be covered by some existing def of the same vendor
    // AND THE SAME DIALECT — a different family on the same pid covers a DIFFERENT control pipe, so
    // it never subsumes. An empty def (no modes) declares no pid and resolves nothing — treat it as
    // subsumed so the all()-over-empty vacuous-true is the intended answer, not an accident.
    def.product_ids().all(|pid| {
        existing.iter().any(|d| {
            d.vendor_id == def.vendor_id
                && d.dialect == def.dialect
                && d.product_ids().any(|p| p == pid)
        })
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
                let def = toml::from_str::<DeviceDef>(src)?;
                // Builtins are compile-embedded with KNOWN dialect ids — unlike external files they
                // need no load-time route filter, but assert the invariant so a future builtin with a
                // typo'd/unregistered dialect trips in dev instead of silently occupying its pid.
                debug_assert!(
                    crate::dialect::by_id(&def.dialect).is_some(),
                    "builtin def '{}' has unroutable dialect '{}'",
                    def.name,
                    def.dialect
                );
                devices.push(def);
            }
        }

        // Optional external definitions: extend coverage without recompiling. Load order is
        // trust order — `find_for_pipe`/`find_by_pid` are first-match-wins, so curated
        // `devices/*.toml` shadow the auto-synthesized `devices/auto/*.toml` (see `crate::synth`)
        // for the same pid WITHIN A FAMILY. Shadowing is BY (PID, DIALECT), never by name: a def
        // loads unless it adds no new pid IN ITS OWN FAMILY at all (`subsumed`). Two families that
        // share a pid (one unit, two protocols) BOTH load — neither shadows the other, each
        // resolves its own control pipe. Auto defs are named from the USB product string, which
        // repeats across revisions/link-modes, so a name collision must NOT drop a def that brings
        // a fresh (pid, dialect).
        for dir in ["devices", "devices/auto"] {
            // Stamp origin by the DIRECTORY the file lives in — a LOAD fact serde can't carry
            // (`origin` is `serde(skip)`, so every parse yields the Builtin default). `devices/`
            // is the user's curated shelf (board-verified, self-heal must never touch it);
            // `devices/auto/` is synthesized config the heal MAY rewrite in place.
            let origin = if dir == "devices/auto" {
                DefOrigin::Auto
            } else {
                DefOrigin::Curated
            };
            if let Ok(rd) = std::fs::read_dir(dir) {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if p.extension().is_some_and(|x| x == "toml") && p.is_file() {
                        if let Ok(txt) = std::fs::read_to_string(&p) {
                            if let Ok(mut def) = toml::from_str::<DeviceDef>(&txt) {
                                def.origin = origin.clone();
                                // REJECT UNROUTABLE at the LOAD boundary: a def we cannot ROUTE (its
                                // `dialect` resolves to no registered family — a typo or a stale
                                // user-editable auto file) is dead weight that would OCCUPY its pid.
                                // `find_by_pid` would resolve it while matching/opening fail closed and
                                // adoption's already-known short-circuit (`find_by_pid(...).is_some()`)
                                // refuses to regenerate — the stranded-device trap. Skipping at load
                                // leaves the pid UNRESOLVED, so the adoption pass hits its existing
                                // "auto def exists but the registry does not resolve it — fix or delete
                                // that file" surface (synth's `adopt_filtered`) and the failure is
                                // REPORTED instead of silent.
                                if crate::dialect::by_id(&def.dialect).is_none() {
                                    continue;
                                }
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

    /// Every def covering (vid, pid) — the multi-family view. `find_by_pid` keeps its
    /// first-match semantics for PID-level questions (labels, capability display, healing);
    /// PIPE resolution must use find_for_pipe, which lets each family's def test the collection
    /// with its own matches_control rule.
    pub fn defs_for_pid(&self, vid: u16, pid: u16) -> impl Iterator<Item = &DeviceDef> {
        self.devices
            .iter()
            .filter(move |d| d.vendor_id == vid && d.product_ids().any(|p| p == pid))
    }

    /// THE pipe-precise resolver: the first def (trust order) whose family claims this exact
    /// collection as its control pipe. With one def per pid this is exactly the old
    /// find_by_pid + matches_control pair; with two families on one pid, each pipe reaches
    /// the def that can actually drive it (the review-blocking gap: first-match-by-pid made
    /// the second family permanently unopenable).
    pub fn find_for_pipe(&self, info: &crate::transport::HidDeviceInfo) -> Option<&DeviceDef> {
        self.defs_for_pid(info.vid, info.pid)
            .find(|d| d.matches_control(info))
    }

    /// Is the FAMILY that claims a pipe on (vid, pid) already covered by a loaded def? Family-scoped,
    /// not pid-scoped: `defs_for_pid(vid, pid).any(|def| def.dialect == dialect_id)`. This fixes the
    /// review-blocking adoption SUPPRESSION — the old `find_by_pid(...).is_some()` already-known gate
    /// is dialect-blind, so a razer def on a pid would block adopting the SAME physical device's
    /// second-family pipe (a hidpp/audio-sidecar collection on that same pid), stranding it forever.
    /// Keyed on the CLAIMING dialect (from `synth::adopt_key`), a pipe is known only when ITS family
    /// is already in the registry, so a still-unadopted family on a shared pid stays adoptable.
    pub fn knows_family(&self, vid: u16, pid: u16, dialect_id: &str) -> bool {
        self.defs_for_pid(vid, pid).any(|d| d.dialect == dialect_id)
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
        // FIRMWARE GAME MODE — the keyboard's FN+F10 Win-key kill. The BlackWidow builtin carries
        // both the getter (0x03/0x80) and setter (0x03/0x00), so BOTH directions are supported.
        assert!(bw.supports(Capability::GameMode));
        assert!(bw.supports(Capability::SetGameMode));
        assert!(!bw.supports(Capability::SetDpi));
        assert!(!bw.supports(Capability::SetDpiStages));
        assert!(!bw.supports(Capability::SetScrollStage));
        assert!(!bw.supports(Capability::Storage));
        assert!(!bw.supports(Capability::Battery));
        // the mouse has no firmware game mode (a keyboard-only Win-key kill) — neither direction.
        assert!(!naga.supports(Capability::GameMode));
        assert!(!naga.supports(Capability::SetGameMode));
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

    /// Like `mini` but with an EXPLICIT `dialect` id — for exercising the unroutable-def trap
    /// (Finding 2): a def whose family the registry can't resolve.
    fn mini_dialect(name: &str, codename: &str, vid: u16, pid: u16, dialect: &str) -> DeviceDef {
        let src = format!(
            "name = \"{name}\"\n\
             codename = \"{codename}\"\n\
             vendor_id = {vid}\n\
             dialect = \"{dialect}\"\n\
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
    fn unknown_dialect_is_the_load_skip_condition() {
        // Pin the exact predicate `Registry::load`'s dir loop uses to REJECT unroutable defs:
        // `if crate::dialect::by_id(&def.dialect).is_none() { continue; }`. A bogus id resolves to
        // nothing (the load skip fires) while a registered family resolves (the def loads). Mirrors
        // dialect.rs's `by_id_resolves_registered_families`, asserted here at the registry's own load
        // boundary so the contract is tested where it's enforced.
        assert!(
            crate::dialect::by_id("nope").is_none(),
            "unknown dialect id → the load-time skip fires (def rejected)"
        );
        assert!(
            crate::dialect::by_id("razer").is_some(),
            "a registered family → the def loads"
        );
    }

    #[test]
    fn unrouted_def_occupies_pid_but_matches_no_pipe_why_load_filters() {
        // WHY load MUST filter (the in-memory trap the loader now prevents): a def with an unroutable
        // dialect, if it ever reached the registry, STILL resolves via `find_by_pid` (pid-keyed,
        // dialect-blind) yet matches NO control pipe (`matches_control` fails closed on an unknown
        // family). That's the stranded-device trap — pid occupied, nothing openable, adoption's
        // `find_by_pid(...).is_some()` short-circuit refusing to regenerate. Load-time
        // `by_id(...).is_none()` skip keeps such a def out of `devices`, so the pid stays UNRESOLVED
        // and the adoption pass reports the failure instead of silently stranding the device.
        let bogus = mini_dialect("Ghost", "Ghost", 0x1532, 0x0999, "nope");
        let reg = Registry {
            devices: vec![bogus],
        };
        // First jaw: find_by_pid (pure pid lookup) resolves it — the pid is OCCUPIED.
        assert!(
            reg.find_by_pid(0x1532, 0x0999).is_some(),
            "an unrouted def still occupies its pid via find_by_pid"
        );
        // Second jaw: even a byte-perfect control pipe selects it NOT AT ALL — fail closed on the
        // unknown dialect (nothing openable). Occupied-but-unopenable = the trap load now prevents.
        let info = crate::transport::HidDeviceInfo {
            vid: 0x1532,
            pid: 0x0999,
            usage_page: 1,
            usage: 2,
            feature_len: 91,
            input_len: 0,
            output_len: 0,
            path: crate::transport::DevicePath::from_str_for_tests("x"),
            product: String::new(),
        };
        assert!(
            !reg.find_by_pid(0x1532, 0x0999).unwrap().matches_control(&info),
            "unknown dialect → matches no pipe (pid occupied, nothing openable — why load filters)"
        );
    }

    #[test]
    fn dialect_defaults_to_razer_and_honors_explicit() {
        // A def WITHOUT a dialect key parses as razer — the serde default that keeps every
        // pre-dialect TOML valid and razer-spoken (mini() emits no dialect key).
        let d = mini("x", "X", 0x1532, 0x0001);
        assert_eq!(d.dialect, "razer", "absent dialect key defaults to razer");
        // An explicit `dialect = "hidpp"` parses as given — the seam a non-razer family sets.
        let src = "\
            name = \"h\"\n\
            codename = \"H\"\n\
            dialect = \"hidpp\"\n\
            vendor_id = 0x046D\n\
            transaction_id = 0x00\n\
            [[modes]]\n\
            name = \"wired\"\n\
            product_id = 0x0001\n\
            [control_interface]\n\
            usage_page = 1\n\
            usage = 2\n\
            feature_report_len = 20\n\
            [commands]\n";
        let h: DeviceDef = toml::from_str(src).unwrap();
        assert_eq!(h.dialect, "hidpp");
    }

    #[test]
    fn origin_is_serde_skipped_and_defaults_builtin() {
        // `origin` is a LOAD fact, never file content: a TOML with no `origin` key parses fine
        // (serde(skip) means it's never read from the file), and the parsed def defaults to
        // Builtin — Registry::load then STAMPS Curated/Auto by directory. `mini()` emits no
        // origin key, so this proves the skip: parsing never fails for a missing origin, and the
        // default is the trust-safe Builtin (a stray def nobody stamped is treated as untouchable).
        let d = mini("x", "X", 0x1532, 0x0001);
        assert_eq!(d.origin, DefOrigin::Builtin, "unstamped parse defaults to Builtin");
        // An explicit `origin = "auto"` in the TOML must be IGNORED (skip = the file can't set it),
        // so even a hand-forged key can't fake provenance — the load path is the sole authority.
        let src = "\
            name = \"x\"\n\
            codename = \"X\"\n\
            origin = \"auto\"\n\
            vendor_id = 0x1532\n\
            transaction_id = 0x1F\n\
            [[modes]]\n\
            name = \"wired\"\n\
            product_id = 0x0001\n\
            [control_interface]\n\
            usage_page = 1\n\
            usage = 2\n\
            feature_report_len = 91\n\
            [commands]\n";
        let d: DeviceDef = toml::from_str(src).unwrap();
        assert_eq!(d.origin, DefOrigin::Builtin, "a file-set origin key is skipped, not honored");
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
    fn subsumed_is_family_scoped_two_families_one_pid_both_load() {
        // Ruling A: subsumption keys on (vendor, pid, DIALECT), not (vendor, pid). Two families on
        // ONE pid — the same physical unit speaking two protocols on different pipes (the
        // razer-audio-sidecar future) — are not subsumption partners: each drives a different
        // control pipe, so BOTH must load. Shared vid 0x046D so both defs group on one (vid, pid).
        let razer = mini_dialect("Combo", "razer-side", 0x046D, 0x0042, "razer");
        let hidpp = mini_dialect("Combo", "hidpp-side", 0x046D, 0x0042, "hidpp");
        // same pid + same NAME + DIFFERENT dialect → NOT subsumed (the second family still loads).
        assert!(
            !subsumed(&[razer.clone()], &hidpp),
            "a different-dialect def on the same pid brings a new family — not subsumed"
        );
        // same dialect + same pid → still subsumed (curated-shadows-auto unchanged WITHIN a family).
        let razer_again = mini_dialect("Combo", "razer-dup", 0x046D, 0x0042, "razer");
        assert!(
            subsumed(&[razer.clone()], &razer_again),
            "same family + same pid = dead weight (first-match resolves it) — unchanged within a family"
        );
    }

    #[test]
    fn find_for_pipe_and_knows_family_route_two_families_on_one_pid() {
        // Ruling B/C: with two families sharing (vid, pid), `defs_for_pid` yields both, `find_for_pipe`
        // routes each pipe SHAPE to the family that can drive it, and `knows_family` answers
        // per-family. Shared vid 0x046D is the one value that lets BOTH resolve: hidpp's claims() is
        // vid-gated to Logitech (0x046D), while razer's matches_control is a usage-triple compare
        // that ignores vid — so 0x046D is the physical "one unit, two protocols" case the routing
        // must handle. pid 0x0042 is not a hidpp receiver pid.
        let razer = mini_dialect("Combo", "razer-side", 0x046D, 0x0042, "razer");
        let hidpp = mini_dialect("Combo", "hidpp-side", 0x046D, 0x0042, "hidpp");
        let reg = Registry {
            devices: vec![razer, hidpp],
        };
        // defs_for_pid is the multi-family view: BOTH defs cover the shared (vid, pid).
        assert_eq!(
            reg.defs_for_pid(0x046D, 0x0042).count(),
            2,
            "both families cover the shared pid"
        );
        // A razer-shaped control pipe (91-byte feature report, usage 1/2, no output/input reports):
        // razer's triple matches; hidpp's claims fails (no HID++ output report) → routes to razer.
        let razer_info = crate::transport::HidDeviceInfo {
            vid: 0x046D,
            pid: 0x0042,
            usage_page: 1,
            usage: 2,
            feature_len: 91,
            input_len: 0,
            output_len: 0,
            path: crate::transport::DevicePath::from_str_for_tests("razer"),
            product: String::new(),
        };
        assert_eq!(
            reg.find_for_pipe(&razer_info).map(|d| d.codename.as_str()),
            Some("razer-side"),
            "a razer-shaped pipe reaches the razer family"
        );
        // A hidpp-shaped control pipe (20-byte output+input HID++ long reports, usage 1/2, no razer
        // 91-byte feature report): razer's triple fails (feature_len 0 ≠ 91); hidpp claims + usage
        // match → routes to hidpp. Mirrors dialect.rs's hidpp HidDeviceInfo constructor.
        let hidpp_info = crate::transport::HidDeviceInfo {
            vid: 0x046D,
            pid: 0x0042,
            usage_page: 1,
            usage: 2,
            feature_len: 0,
            input_len: 20,
            output_len: 20,
            path: crate::transport::DevicePath::from_str_for_tests("hidpp"),
            product: String::new(),
        };
        assert_eq!(
            reg.find_for_pipe(&hidpp_info).map(|d| d.codename.as_str()),
            Some("hidpp-side"),
            "a hidpp-shaped pipe reaches the hidpp family — the second family is resolvable, not stranded"
        );
        // knows_family answers per FAMILY, not per pid: both families known, a third is not.
        assert!(reg.knows_family(0x046D, 0x0042, "razer"), "razer family is loaded");
        assert!(reg.knows_family(0x046D, 0x0042, "hidpp"), "hidpp family is loaded");
        assert!(
            !reg.knows_family(0x046D, 0x0042, "someother"),
            "a family NOT on this pid is unknown — so its pipe stays adoptable"
        );
    }

    #[test]
    fn events_table_parses_and_event_for_matches_lead_bytes() {
        let src = "\
            name = \"x\"\n\
            codename = \"X\"\n\
            vendor_id = 0x1532\n\
            transaction_id = 0x1F\n\
            [[modes]]\n\
            name = \"usb\"\n\
            product_id = 0x0001\n\
            [control_interface]\n\
            usage_page = 1\n\
            usage = 2\n\
            feature_report_len = 64\n\
            [commands]\n\
            [events]\n\
            usage_page = 0x000C\n\
            usage = 0x0001\n\
            feature_len = 64\n\
            [events.reports]\n\
            \"0511\" = \"mute_state\"\n";
        let d: DeviceDef = toml::from_str(src).unwrap();
        assert_eq!(d.event_for(&[0x05, 0x11, 0x01]), Some(EventKind::MuteState));
        assert_eq!(d.event_for(&[0x05, 0x11]), Some(EventKind::MuteState), "2 lead bytes is enough");
        // a lead-byte pair the vocabulary doesn't name.
        assert_eq!(d.event_for(&[0x05, 0x02, 0x00]), None);
        // too short to carry even the lead pair.
        assert_eq!(d.event_for(&[0x05]), None);
        // arming triple: matches only the exact (page, usage, flen) the [events] block declares.
        assert!(d.event_pipe_matches(0x000C, 0x0001, 64));
        assert!(!d.event_pipe_matches(0x0001, 0x0002, 64), "wrong collection");
        assert!(!d.event_pipe_matches(0x000C, 0x0001, 91), "wrong feature length");
    }

    #[test]
    fn unknown_event_kind_is_a_load_error() {
        // An event-name typo must not silently vanish — it fails the whole file's parse (mirrors the
        // registry's fail-loud stance on an unroutable `dialect`), not a quietly-dropped table entry.
        let src = "\
            name = \"x\"\n\
            codename = \"X\"\n\
            vendor_id = 0x1532\n\
            transaction_id = 0x1F\n\
            [[modes]]\n\
            name = \"usb\"\n\
            product_id = 0x0001\n\
            [control_interface]\n\
            usage_page = 1\n\
            usage = 2\n\
            feature_report_len = 64\n\
            [commands]\n\
            [events]\n\
            usage_page = 0x000C\n\
            usage = 0x0001\n\
            feature_len = 64\n\
            [events.reports]\n\
            \"0511\" = \"mute_stat3\"\n";
        assert!(toml::from_str::<DeviceDef>(src).is_err());
    }

    #[test]
    fn devices_without_events_round_trip_unchanged() {
        // mini()/builtins() emit no `[events]` key at all — the field must stay None, not error.
        let d = mini("x", "X", 0x1532, 0x0001);
        assert!(d.events.is_none());
        assert_eq!(d.event_for(&[0x05, 0x11, 0x01]), None);
        assert!(!d.event_pipe_matches(0x000C, 0x0001, 64));
        let (naga, bw) = builtins();
        assert!(naga.events.is_none(), "the Naga's own report map isn't in the registry yet");
        assert!(bw.events.is_none());
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
