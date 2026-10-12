// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../../LICENSE.md.

//! OS observations for compatibility research, separate from device adoption and command I/O.

use super::DevicePath;
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorSource { WindowsHidParser, LinuxSysfs, Unavailable }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grouping { WindowsContainer, LinuxUsbAncestor, PathHeuristic }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind { Input, Output, Feature }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub kind: ReportKind,
    pub button: bool,
    pub alias: bool,
    pub is_range: bool,
    pub report_id: u8,
    pub usage_page: u16,
    pub usage_min: u16,
    pub usage_max: u16,
    pub link_collection: u16,
    pub data_index_min: u16,
    pub data_index_max: u16,
    pub bit_field: u16,
    pub absolute: bool,
    pub bit_size: Option<u16>,
    pub report_count: u16,
    pub logical_min: Option<i32>,
    pub logical_max: Option<i32>,
    pub physical_min: Option<i32>,
    pub physical_max: Option<i32>,
    pub units: Option<u32>,
    pub units_exponent: Option<u32>,
    pub has_null: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkCollection {
    pub usage_page: u16,
    pub usage: u16,
    pub parent: u16,
    pub children: u16,
    pub first_child: u16,
    pub next_sibling: u16,
    pub collection_type: u8,
    pub alias: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionShape {
    pub usage_page: u16,
    pub usage: u16,
    pub feature_len: u16,
    pub input_len: u16,
    pub output_len: u16,
}

/// Local-only strings and topology keys must not be serialized into a shareable bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub source: DescriptorSource,
    pub grouping: Grouping,
    pub group_key: Option<String>,
    pub revision: Option<u16>,
    pub manufacturer: Option<String>,
    pub driver_version: Option<String>,
    pub driver_provider: Option<String>,
    pub bus_type: Option<u16>,
    pub raw_descriptor_hex: Option<String>,
    pub fields: Vec<Field>,
    pub links: Vec<LinkCollection>,
    /// False if any descriptor API failed. An empty field list alone is not proof of no fields.
    pub complete: bool,
}

impl Metadata {
    pub fn unavailable() -> Self {
        Self {
            source: DescriptorSource::Unavailable, grouping: Grouping::PathHeuristic,
            group_key: None, revision: None, manufacturer: None, driver_version: None,
            driver_provider: None, bus_type: None, raw_descriptor_hex: None,
            fields: Vec::new(), links: Vec::new(), complete: false,
        }
    }
}

/// Parse a saved raw descriptor through the production Linux HID parser, without hardware.
pub fn descriptor_shapes(hex: &str) -> Result<Vec<CollectionShape>> {
    anyhow::ensure!(hex.len() <= 131_072, "descriptor exceeds 64 KiB");
    let bytes = super::parity::decode_hex(hex)
        .ok_or_else(|| anyhow::anyhow!("descriptor is not hexadecimal"))?;
    anyhow::ensure!(bytes.len() <= 65_536, "descriptor exceeds 64 KiB");
    Ok(super::hid_descriptor::parse(&bytes).into_iter().map(|c| CollectionShape {
        usage_page: c.usage_page, usage: c.usage, feature_len: c.feature_len,
        input_len: c.input_len, output_len: c.output_len,
    }).collect())
}

#[cfg(windows)]
#[path = "research_windows.rs"]
mod windows;

pub(super) fn platform_metadata(path: &DevicePath) -> Result<Metadata> {
    #[cfg(windows)]
    { windows::metadata(path) }
    #[cfg(target_os = "linux")]
    { linux_metadata(path) }
    #[cfg(not(any(windows, target_os = "linux")))]
    { let _ = path; Ok(Metadata::unavailable()) }
}

#[cfg(target_os = "linux")]
fn linux_metadata(path: &DevicePath) -> Result<Metadata> {
    use std::io::Read as _;
    use std::path::Path;
    let raw = path.as_os_str().to_string_lossy();
    let (node, _) = raw.rsplit_once("#col")
        .ok_or_else(|| anyhow::anyhow!("not a hidraw collection path"))?;
    let device = Path::new(node).parent().and_then(Path::parent)
        .ok_or_else(|| anyhow::anyhow!("not a hidraw sysfs node"))?;
    let mut descriptor = Vec::new();
    std::fs::File::open(device.join("report_descriptor"))?.take(65_537).read_to_end(&mut descriptor)?;
    anyhow::ensure!(descriptor.len() <= 65_536, "descriptor exceeds 64 KiB");
    let mut result = Metadata::unavailable();
    result.source = DescriptorSource::LinuxSysfs;
    result.raw_descriptor_hex = Some(super::parity::encode_hex(&descriptor));
    result.complete = false; // Raw bytes are retained; normalized field parsing is not yet provided.
    if let Ok(text) = std::fs::read_to_string(device.join("uevent")) {
        result.bus_type = text.lines().find_map(|line| line.strip_prefix("HID_ID=")
            .and_then(|id| id.split(':').next()).and_then(|bus| u16::from_str_radix(bus, 16).ok()));
    }
    for parent in device.ancestors().take(8) {
        if parent.join("idVendor").is_file() && parent.join("idProduct").is_file() {
            result.grouping = Grouping::LinuxUsbAncestor;
            result.group_key = Some(parent.to_string_lossy().into_owned());
            result.manufacturer = std::fs::read_to_string(parent.join("manufacturer")).ok().map(|s| s.trim().into());
            result.revision = std::fs::read_to_string(parent.join("bcdDevice")).ok()
                .and_then(|s| u16::from_str_radix(s.trim(), 16).ok());
            break;
        }
    }
    Ok(result)
}
