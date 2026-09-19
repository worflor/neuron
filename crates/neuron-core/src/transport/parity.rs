// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../../LICENSE.md.

//! Cross-platform HID parity fixtures: Windows' own HID parser against ours, on the same physical
//! device.
//!
//! Report lengths decide how many bytes a feature write puts on the wire, so a wrong one is a wrong
//! write to real hardware. On Windows those lengths come from `HidP_GetCaps` — the OS parses the
//! report descriptor and we trust it. On Linux nothing parses it for us: [`hid_descriptor::parse`]
//! does, and nothing external confirms the answer.
//!
//! A fixture pairs the two halves for one device, captured from one machine that has it:
//!   * `<pid>-windows.json` — the caps Windows reports, one entry per top-level collection.
//!   * `<pid>-linux.json`   — the raw report descriptor bytes read from sysfs, plus what our
//!     parser made of them.
//!
//! The descriptor bytes are the fixture, so [`tests::parser_agrees_with_windows_caps`] re-parses
//! them on every platform, on every CI run, with no device attached. Capture is manual and needs
//! the hardware; see the `#[ignore]`d tests in `windows_hid` and `hidraw`.

use super::hid_descriptor;
use serde::{Deserialize, Serialize};

/// One top-level collection's report shape. Lengths include the report-ID byte, the `HIDP_CAPS`
/// convention both sides follow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collection {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub input_len: u16,
    pub output_len: u16,
}

/// One device's half of a parity pair.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Where the numbers came from: `windows-hidp-getcaps` or `linux-report-descriptor`.
    pub source: String,
    pub vid: u16,
    pub pid: u16,
    pub product: String,
    /// The raw report descriptor, lowercase hex, no separators. Linux captures only — Windows
    /// exposes a parsed view (`HIDP_PREPARSED_DATA`) and never the bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_descriptor_hex: Option<String>,
    pub collections: Vec<Collection>,
}

impl Capture {
    /// The fixture directory, `crates/neuron-core/testdata/hid`.
    pub fn dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("hid")
    }

    /// `<pid>-<platform>.json` in the fixture directory.
    pub fn path(pid: u16, platform: &str) -> std::path::PathBuf {
        Self::dir().join(format!("{pid:04x}-{platform}.json"))
    }

    pub fn write(&self, platform: &str) -> std::io::Result<std::path::PathBuf> {
        let path = Self::path(self.pid, platform);
        std::fs::create_dir_all(Self::dir())?;
        let json = serde_json::to_string_pretty(self).expect("a Capture always serializes");
        std::fs::write(&path, format!("{json}\n"))?;
        Ok(path)
    }

    pub fn read(path: &std::path::Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Parse `report_descriptor_hex` into the collections it declares. `None` when this half
    /// carries no descriptor (every Windows capture).
    pub fn reparse(&self) -> Option<Vec<Collection>> {
        let hex = self.report_descriptor_hex.as_ref()?;
        let bytes = decode_hex(hex)?;
        Some(
            hid_descriptor::parse(&bytes)
                .into_iter()
                .map(|c| Collection {
                    usage_page: c.usage_page,
                    usage: c.usage,
                    feature_len: c.feature_len,
                    input_len: c.input_len,
                    output_len: c.output_len,
                })
                .collect(),
        )
    }
}

pub fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `None` on an odd length or a non-hex digit.
pub fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every collection our parser derives must appear in the Windows caps for the same device,
    /// with identical report lengths. The claim is that we never invent a report shape, which is
    /// the one that matters: a length we made up is a wrong number of bytes on the wire.
    ///
    /// The other direction is NOT asserted, because Windows genuinely reports collections that do
    /// not exist at the USB level. A Naga V2 Pro shows two — `0x000c/0x0001` at 3 bytes and
    /// `0x0001/0x0080` at 2 — on paths under `mi_00&col03&colNN`, whose `PnP` parent is Razer's own
    /// `RZVIRTUAL` bus, not the USB device. They are fabricated by the vendor driver and no Linux
    /// kernel will ever enumerate them. Windows-only collections are printed, so the asymmetry
    /// stays visible rather than being asserted away.
    ///
    /// With no fixtures captured yet this passes while saying so — the pair needs a physical
    /// device on a machine running both platforms. Capturing one is the point of the `#[ignore]`d
    /// capture tests.
    #[test]
    fn parser_agrees_with_windows_caps() {
        let dir = Capture::dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            println!("no HID parity fixtures in {} — nothing to cross-check", dir.display());
            return;
        };
        let mut pairs = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if !name.ends_with("-windows.json") {
                continue;
            }
            let win = Capture::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let linux_path = Capture::path(win.pid, "linux");
            let Ok(linux) = Capture::read(&linux_path) else {
                println!("{:04x}: windows half only, no linux half to compare", win.pid);
                continue;
            };
            let parsed = linux
                .reparse()
                .unwrap_or_else(|| panic!("{} has no usable report_descriptor_hex", linux_path.display()));
            pairs += 1;

            // Membership, not position: one device really does expose the same usage pair more
            // than once with different report shapes (a Naga V2 Pro reports 0x0001/0x0002 twice,
            // once with a 91-byte feature report and once with none), so the pair alone does not
            // identify a collection.
            for got in &parsed {
                if win.collections.contains(got) {
                    continue;
                }
                let same_usage: Vec<String> = win
                    .collections
                    .iter()
                    .filter(|c| c.usage_page == got.usage_page && c.usage == got.usage)
                    .map(|c| format!("feature={} input={} output={}", c.feature_len, c.input_len, c.output_len))
                    .collect();
                panic!(
                    "{} {:04x}:{:04x} collection {:#06x}/{:#06x}: we derived feature={} input={} \
                     output={} from the report descriptor, Windows reports {}",
                    win.product,
                    win.vid,
                    win.pid,
                    got.usage_page,
                    got.usage,
                    got.feature_len,
                    got.input_len,
                    got.output_len,
                    if same_usage.is_empty() {
                        "no collection with that usage at all".to_string()
                    } else {
                        format!("only [{}]", same_usage.join("; "))
                    }
                );
            }
            for extra in &win.collections {
                if !parsed.contains(extra) {
                    println!(
                        "{:04x}: windows reports {:#06x}/{:#06x} feature={} input={} output={}, which no \
                         USB report descriptor declares (RZVIRTUAL, or an interface linux did not see)",
                        win.pid, extra.usage_page, extra.usage, extra.feature_len, extra.input_len, extra.output_len
                    );
                }
            }
        }
        println!("HID parity: {pairs} device(s) cross-checked against Windows HidP_GetCaps");
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0x05u8, 0x01, 0xA1, 0x00, 0xFF];
        assert_eq!(encode_hex(&bytes), "0501a100ff");
        assert_eq!(decode_hex("0501a100ff").as_deref(), Some(&bytes[..]));
        assert_eq!(decode_hex("abc"), None, "odd length is not hex");
        assert_eq!(decode_hex("zz"), None, "non-hex digits are rejected");
    }
}
