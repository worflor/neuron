// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../../LICENSE.md.

use super::{DescriptorSource, Field, Grouping, LinkCollection, Metadata, ReportKind};
use crate::transport::DevicePath;
use anyhow::{bail, Result};
use std::ptr;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::*;
use windows_sys::Win32::Devices::HumanInterfaceDevice::*;
use windows_sys::Win32::Devices::Properties::*;
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};

const HIDP_OK: i32 = 0x0011_0000;

pub(super) fn metadata(path: &DevicePath) -> Result<Metadata> {
    let wide = path.to_wide_nul();
    // SAFETY: enumerated NUL-terminated path, zero-access metadata handle; every acquired
    // handle and preparsed block is released below. No feature, output or input I/O occurs.
    unsafe {
        let handle = CreateFileW(wide.as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(), OPEN_EXISTING, 0, ptr::null_mut());
        if handle == INVALID_HANDLE_VALUE { bail!("HID metadata open failed"); }
        let result = describe_handle(handle);
        CloseHandle(handle);
        let mut result = result?;
        pnp_metadata(&wide, &mut result);
        Ok(result)
    }
}

unsafe fn describe_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> Result<Metadata> {
    let mut result = Metadata::unavailable();
    let mut attrs: HIDD_ATTRIBUTES = std::mem::zeroed();
    attrs.Size = std::mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    if HidD_GetAttributes(handle, &raw mut attrs) != 0 { result.revision = Some(attrs.VersionNumber); }
    let mut manufacturer = [0u16; 127];
    if HidD_GetManufacturerString(handle, manufacturer.as_mut_ptr().cast(), 254) != 0 {
        let end = manufacturer.iter().position(|&c| c == 0).unwrap_or(manufacturer.len());
        result.manufacturer = Some(String::from_utf16_lossy(&manufacturer[..end]));
    }
    let mut pp = 0isize;
    if HidD_GetPreparsedData(handle, &raw mut pp) == 0 { return Ok(result); }
    let parsed = describe_preparsed(pp, &mut result);
    HidD_FreePreparsedData(pp);
    parsed?;
    Ok(result)
}

unsafe fn describe_preparsed(pp: isize, result: &mut Metadata) -> Result<()> {
    let mut caps: HIDP_CAPS = std::mem::zeroed();
    if HidP_GetCaps(pp, &raw mut caps) != HIDP_OK { return Ok(()); }
    result.source = DescriptorSource::WindowsHidParser;
    result.complete = true;
    let reports = [
        (HidP_Input, ReportKind::Input, caps.NumberInputButtonCaps, caps.NumberInputValueCaps),
        (HidP_Output, ReportKind::Output, caps.NumberOutputButtonCaps, caps.NumberOutputValueCaps),
        (HidP_Feature, ReportKind::Feature, caps.NumberFeatureButtonCaps, caps.NumberFeatureValueCaps),
    ];
    for (report_type, kind, button_count, value_count) in reports {
        let mut n = button_count;
        let mut buttons: Vec<HIDP_BUTTON_CAPS> = vec![std::mem::zeroed(); n as usize];
        if n > 0 {
            if HidP_GetButtonCaps(report_type, buttons.as_mut_ptr(), &raw mut n, pp) == HIDP_OK
                && n as usize <= buttons.len() {
                for b in &buttons[..n as usize] {
                    let (lo, hi) = if b.IsRange != 0 {
                        (b.Anonymous.Range.UsageMin, b.Anonymous.Range.UsageMax)
                    } else { (b.Anonymous.NotRange.Usage, b.Anonymous.NotRange.Usage) };
                    let (data_min, data_max) = if b.IsRange != 0 {
                        (b.Anonymous.Range.DataIndexMin, b.Anonymous.Range.DataIndexMax)
                    } else { (b.Anonymous.NotRange.DataIndex, b.Anonymous.NotRange.DataIndex) };
                    result.fields.push(Field {
                        kind, button: true, alias: b.IsAlias != 0, is_range: b.IsRange != 0,
                        data_index_min: data_min, data_index_max: data_max,
                        report_id: b.ReportID, usage_page: b.UsagePage,
                        usage_min: lo, usage_max: hi, link_collection: b.LinkCollection,
                        bit_field: b.BitField, absolute: b.IsAbsolute != 0, bit_size: None,
                        report_count: b.ReportCount, logical_min: None, logical_max: None,
                        physical_min: None, physical_max: None, units: None, units_exponent: None,
                        has_null: false,
                    });
                }
            } else { result.complete = false; }
        }
        let mut n = value_count;
        let mut values: Vec<HIDP_VALUE_CAPS> = vec![std::mem::zeroed(); n as usize];
        if n > 0 {
            if HidP_GetValueCaps(report_type, values.as_mut_ptr(), &raw mut n, pp) == HIDP_OK
                && n as usize <= values.len() {
                for v in &values[..n as usize] {
                    let (lo, hi) = if v.IsRange != 0 {
                        (v.Anonymous.Range.UsageMin, v.Anonymous.Range.UsageMax)
                    } else { (v.Anonymous.NotRange.Usage, v.Anonymous.NotRange.Usage) };
                    let (data_min, data_max) = if v.IsRange != 0 {
                        (v.Anonymous.Range.DataIndexMin, v.Anonymous.Range.DataIndexMax)
                    } else { (v.Anonymous.NotRange.DataIndex, v.Anonymous.NotRange.DataIndex) };
                    result.fields.push(Field {
                        kind, button: false, alias: v.IsAlias != 0, is_range: v.IsRange != 0,
                        data_index_min: data_min, data_index_max: data_max,
                        report_id: v.ReportID, usage_page: v.UsagePage,
                        usage_min: lo, usage_max: hi, link_collection: v.LinkCollection,
                        bit_field: v.BitField, absolute: v.IsAbsolute != 0, bit_size: Some(v.BitSize),
                        report_count: v.ReportCount, logical_min: Some(v.LogicalMin), logical_max: Some(v.LogicalMax),
                        physical_min: Some(v.PhysicalMin), physical_max: Some(v.PhysicalMax),
                        units: Some(v.Units), units_exponent: Some(v.UnitsExp), has_null: v.HasNull != 0,
                    });
                }
            } else { result.complete = false; }
        }
    }
    let mut n = u32::from(caps.NumberLinkCollectionNodes);
    let mut links: Vec<HIDP_LINK_COLLECTION_NODE> = vec![std::mem::zeroed(); n as usize];
    if n > 0 {
        if HidP_GetLinkCollectionNodes(links.as_mut_ptr(), &raw mut n, pp) == HIDP_OK
            && n as usize <= links.len() {
            for link in &links[..n as usize] {
                result.links.push(LinkCollection {
                    usage_page: link.LinkUsagePage, usage: link.LinkUsage, parent: link.Parent,
                    children: link.NumberOfChildren, first_child: link.FirstChild,
                    next_sibling: link.NextSibling, collection_type: (link._bitfield & 0xff) as u8,
                    alias: link._bitfield & 0x100 != 0,
                });
            }
        } else { result.complete = false; }
    }
    Ok(())
}

unsafe fn pnp_metadata(path: &[u16], result: &mut Metadata) {
    let mut guid: GUID = std::mem::zeroed();
    HidD_GetHidGuid(&raw mut guid);
    let set = SetupDiGetClassDevsW(&raw const guid, ptr::null(), ptr::null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
    if set == -1 { return; }
    let mut interface: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
    interface.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
    if SetupDiOpenDeviceInterfaceW(set, path.as_ptr(), 0, &raw mut interface) != 0 {
        let mut node: SP_DEVINFO_DATA = std::mem::zeroed();
        node.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
        let mut required = 0;
        SetupDiGetDeviceInterfaceDetailW(set, &raw const interface, ptr::null_mut(), 0, &raw mut required, &raw mut node);
        // The first detail call retrieves SP_DEVINFO_DATA even when the path buffer is too small.
        if node.DevInst != 0 {
            if let Some(bytes) = property(set, &node, &DEVPKEY_Device_ContainerId, DEVPROP_TYPE_GUID) {
                if bytes.len() == 16 && bytes.iter().any(|&b| b != 0) {
                    result.grouping = Grouping::WindowsContainer;
                    result.group_key = Some(bytes.iter().map(|b| format!("{b:02x}")).collect());
                }
            }
            result.driver_version = string_property(set, &node, &DEVPKEY_Device_DriverVersion);
            result.driver_provider = string_property(set, &node, &DEVPKEY_Device_DriverProvider);
        }
    }
    SetupDiDestroyDeviceInfoList(set);
}

unsafe fn property(set: HDEVINFO, node: &SP_DEVINFO_DATA, key: &DEVPROPKEY, expected: u32) -> Option<Vec<u8>> {
    let mut ty = 0;
    let mut size = 0;
    SetupDiGetDevicePropertyW(set, node, key, &raw mut ty, ptr::null_mut(), 0, &raw mut size, 0);
    if size == 0 || size > 4096 { return None; }
    let mut bytes = vec![0u8; size as usize];
    if SetupDiGetDevicePropertyW(set, node, key, &raw mut ty, bytes.as_mut_ptr(), size, &raw mut size, 0) == 0
        || ty != expected || size as usize > bytes.len() { return None; }
    bytes.truncate(size as usize);
    Some(bytes)
}

unsafe fn string_property(set: HDEVINFO, node: &SP_DEVINFO_DATA, key: &DEVPROPKEY) -> Option<String> {
    let bytes = property(set, node, key, DEVPROP_TYPE_STRING)?;
    let wide: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).take_while(|&c| c != 0).collect();
    Some(String::from_utf16_lossy(&wide))
}
