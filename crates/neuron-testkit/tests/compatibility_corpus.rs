// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use anyhow::{ensure, Context, Result};
use neuron::{capability, device::Device, lighting::{Effect, Rgb}, registry::DeviceDef, transport::{self, DevicePath, HidDeviceInfo}};
use neuron_testkit::{compat::{Channel, Exchange, ProtocolTape, RecordedProtocol, Reply}, compat_bus::ReplayBackend};
use serde::Deserialize;
use std::{collections::BTreeSet, path::{Path, PathBuf}, sync::Arc};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Source { repository: String, commit: String, path: String, symbol: String, claim: String }

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    schema: u32,
    id: String,
    evidence: String,
    replies: String,
    definition: String,
    sources: Vec<Source>,
    operation: Operation,
    exchanges: Vec<WireExchange>,
    expected_value: Option<String>,
    expected_error: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Firmware, Battery, Dpi,
    Static { rgb: [u8; 3] },
    Row { pixels: Vec<[u8; 3]> },
    CustomDisplay,
    Brightness { percent: u8 },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExchange { channel: Channel, request: String, replies: Vec<WireReply> }

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireReply { Bytes { hex: String }, Timeout, Disconnected }

fn hex(bytes: &str) -> Result<Vec<u8>> {
    bytes.split_ascii_whitespace().map(|b| {
        ensure!(b.len() == 2, "hex bytes must have two digits");
        Ok(u8::from_str_radix(b, 16)?)
    }).collect()
}

fn load(path: &Path) -> Result<(Fixture, DeviceDef, RecordedProtocol)> {
    let fixture: Fixture = toml::from_str(&std::fs::read_to_string(path)?)?;
    ensure!(fixture.schema == 1 && !fixture.id.is_empty(), "invalid fixture identity/schema");
    ensure!(fixture.evidence == "source-derived" && fixture.replies == "synthetic", "this corpus accepts only source-derived requests and synthetic replies");
    ensure!(fixture.expected_value.is_some() != fixture.expected_error.is_some(), "one expected outcome is required");
    ensure!(!fixture.sources.is_empty(), "fixture has no independent source");
    for source in &fixture.sources {
        ensure!(source.repository.split('/').count() == 2 && source.repository.split('/').all(|part| !part.is_empty()) && !source.path.is_empty()
            && !source.symbol.is_empty() && !source.claim.is_empty()
            && source.commit.len() == 40 && source.commit.bytes().all(|b| b.is_ascii_hexdigit()), "incomplete source provenance");
    }
    ensure!(fixture.definition.ends_with(".toml") && !fixture.definition.contains(['/', '\\'])
        && !fixture.definition.contains(".."), "definition must be a local filename");
    let def: DeviceDef = toml::from_str(&std::fs::read_to_string(path.parent().unwrap().join("definitions").join(&fixture.definition))?)?;
    let pid = def.product_ids().next().context("definition has no mode")?;
    let exchanges = fixture.exchanges.iter().map(|e| Ok(Exchange {
        channel: e.channel, request: hex(&e.request)?, replies: e.replies.iter().map(|r| Ok(match r {
            WireReply::Bytes { hex: bytes } => Reply::Bytes(hex(bytes)?),
            WireReply::Timeout => Reply::Timeout,
            WireReply::Disconnected => Reply::Disconnected,
        })).collect::<Result<_>>()?,
    })).collect::<Result<_>>()?;
    let tape = RecordedProtocol::new(ProtocolTape { schema: 1, vid: def.vendor_id, pid, exchanges })?;
    Ok((fixture, def, tape))
}

fn run(operation: &Operation, device: &Device) -> Result<String> {
    match operation {
        Operation::Firmware => capability::firmware(device),
        Operation::Battery => Ok(capability::battery_percent(device)?.to_string()),
        Operation::Dpi => { let (x, y) = capability::dpi(device)?; Ok(format!("{x}x{y}")) }
        Operation::Brightness { percent } => { capability::set_brightness(device, *percent, capability::Store::Volatile)?; Ok("ok".into()) }
        operation => {
            let lighting = device.def.lighting.as_ref().context("lighting definition required")?;
            let report = match operation {
                Operation::Static { rgb } => lighting.native_effect_report(Effect::Static, Some(Rgb::new(rgb[0], rgb[1], rgb[2])), false).context("static effect unavailable")?,
                Operation::Row { pixels } => lighting.row_report(&pixels.iter().map(|c| Rgb::new(c[0], c[1], c[2])).collect::<Vec<_>>(), 0).context("row unavailable")?,
                Operation::CustomDisplay => lighting.custom_display_report(),
                _ => unreachable!(),
            };
            device.apply_lighting(&report)?;
            Ok("ok".into())
        }
    }
}

fn info(def: &DeviceDef, pid: u16, path: &str) -> HidDeviceInfo {
    HidDeviceInfo { vid: def.vendor_id, pid, path: DevicePath::from_str_for_tests(path), product: "source fixture".into(),
        usage_page: def.control_interface.usage_page, usage: def.control_interface.usage,
        feature_len: def.control_interface.feature_report_len,
        input_len: if def.dialect == "hidpp" { 20 } else { 0 }, output_len: if def.dialect == "hidpp" { 20 } else { 0 } }
}

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testdata/compat")
}

fn exercise(path: &Path, ids: &mut BTreeSet<String>) -> Result<()> {
    let (fixture, def, tape) = load(path)?;
    ensure!(ids.insert(fixture.id.clone()), "duplicate fixture identity");
    let pid = def.product_ids().next().unwrap();
    let bus = Arc::new(ReplayBackend::new(vec![(info(&def, pid, &format!("replay:{}", fixture.id)), tape.clone())])?);
    let _bus = transport::install_backend(bus);
    let device = Device::open(def, pid).with_context(|| fixture.id.clone())?;
    let result = run(&fixture.operation, &device);
    let verification = tape.verify_complete();
    match (&fixture.expected_value, &fixture.expected_error) {
        (Some(value), None) => ensure!(result.as_ref().is_ok_and(|got| got == value), "{}: expected {value}, got {result:?}", fixture.id),
        (None, Some(error)) => ensure!(result.as_ref().is_err_and(|got| got.to_string().contains(error)), "{}: expected error containing {error:?}, got {result:?}", fixture.id),
        _ => unreachable!(),
    }
    verification.with_context(|| fixture.id.clone())?;
    eprintln!("{}: source-derived request / synthetic replies passed", fixture.id);
    Ok(())
}

#[test]
fn source_corpus_drives_production_selection_framing_and_semantics() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap();
    let _deny = transport::deny_hardware();
    let root = corpus_root();
    let mut paths = std::fs::read_dir(&root)?.map(|e| Ok(e?.path())).collect::<Result<Vec<_>>>()?;
    paths.retain(|p| p.extension().is_some_and(|ext| ext == "toml"));
    paths.sort();
    ensure!(!paths.is_empty(), "compatibility corpus is empty");
    let mut ids = BTreeSet::new();
    let failures: Vec<_> = paths.iter().filter_map(|path| exercise(path, &mut ids).err()
        .map(|error| format!("{}: {error:#}", path.display()))).collect();
    ensure!(failures.is_empty(), "compatibility failures:\n{}", failures.join("\n"));
    Ok(())
}

#[test]
fn same_pid_units_and_unrelated_collections_do_not_steal_a_reopened_command() -> Result<()> {
    let _serial = transport::mock::test_lock().lock().unwrap();
    let _deny = transport::deny_hardware();
    let path = corpus_root().join("razer-invalid-then-valid.toml");
    let (_, def, first) = load(&path)?;
    let (_, _, second) = load(&path)?;
    let pid = def.product_ids().next().unwrap();
    let a = info(&def, pid, "replay:unit-a");
    let b = info(&def, pid, "replay:unit-b");
    let path_a = a.path.clone();
    let unit_a = transport::path_instance("replay:unit-a");
    let unit_b = transport::path_instance("replay:unit-b");
    ensure!(unit_a != unit_b, "fixture units must be distinct");
    let mut decoy = info(&def, pid, "replay:standard-keyboard");
    decoy.usage_page = 1;
    decoy.usage = 6;
    let unused = RecordedProtocol::new(ProtocolTape { schema: 1, vid: def.vendor_id, pid, exchanges: Vec::new() })?;
    let bus = Arc::new(ReplayBackend::new(vec![(decoy, unused.clone()), (b, second.clone()), (a, first.clone())])?);
    let _bus = transport::install_backend(bus.clone());
    let registry = neuron::registry::Registry { devices: vec![def] };
    let retained = Device::open_with_command_unit(&registry, "firmware_version", pid, &unit_a)?;
    bus.set_connected(&path_a, false)?;
    ensure!(capability::firmware(&retained).is_err(), "disconnected handles must fail");
    ensure!(Device::open_with_command_unit(&registry, "firmware_version", pid, &unit_a).is_err(), "a missing unit must not fall back to its same-PID peer");
    let peer = Device::open_with_command_unit(&registry, "firmware_version", pid, &unit_b)?;
    ensure!(capability::firmware(&peer)? == "1.02", "peer firmware decode");
    second.verify_complete()?;
    bus.set_connected(&path_a, true)?;
    ensure!(capability::firmware(&retained).is_err(), "replug must not revive an old handle");
    let reopened = Device::open_with_command_unit(&registry, "firmware_version", pid, &unit_a)?;
    ensure!(capability::firmware(&reopened)? == "1.02", "reopened firmware decode");
    first.verify_complete()?;
    unused.verify_complete()?;
    Ok(())
}
