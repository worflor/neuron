// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

//! Compatibility evidence: unfiltered inventory, bounded passive observations and checked export.
//! This service never adopts devices, sends protocol commands or enables dispatch.

use crate::registry::Registry;
use crate::transport::{self, research::{DescriptorSource, Field, Grouping, LinkCollection, Metadata, ReportKind}, HidDeviceInfo, InputReader};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SCHEMA: u32 = 1;
const MAX_FILE: u64 = 16 * 1024 * 1024;
const MAX_ARCHIVE: u64 = 64 * 1024 * 1024;
const MAX_REPORTS: usize = 2048;
const MAX_REPORT_BYTES: usize = 4096;
const MAX_CAPTURE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDossier {
    pub schema: u32,
    pub neuron_version: String,
    pub platform: String,
    pub environment: Environment,
    pub registry_available: bool,
    pub collected_unix_seconds: u64,
    pub interfaces: Vec<LocalInterface>,
    pub recordings: Vec<InputRecording>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalInterface {
    pub id: u32,
    pub device: u32,
    pub vid: u16,
    pub pid: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub path: String,
    pub product: String,
    pub metadata: Metadata,
    pub recognition: Recognition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recognition {
    pub signature_dialects: Vec<String>,
    pub registry_match: bool,
    pub declared_capabilities: Vec<String>,
    /// No protocol I/O is performed by collection. Signature matches are not answered probes.
    pub protocol_tested: bool,
    pub device_write_tested: bool,
    pub physical_output_tested: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSample {
    pub elapsed_us: u64,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingEnd { Deadline, ReportLimit, ByteLimit, ReadFailed }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRecording {
    pub interface: u32,
    pub recipe: String,
    pub duration_ms: u64,
    pub end: RecordingEnd,
    pub samples: Vec<InputSample>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    pub schema: u32,
    pub neuron_version: String,
    pub platform: String,
    pub environment: Environment,
    pub registry_available: bool,
    pub interfaces: Vec<Interface>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub architecture: String,
    pub os_version: Option<[u32; 3]>,
    pub hyperscroll_write_compiled: bool,
    pub idle_power_write_compiled: bool,
    pub in_game_poll_write_compiled: bool,
    pub snap_tap_write_compiled: bool,
}

impl Environment {
    fn current() -> Self {
        Self { architecture: std::env::consts::ARCH.into(), os_version: os_version(),
            hyperscroll_write_compiled: cfg!(feature = "hyperscroll-write"),
            idle_power_write_compiled: cfg!(feature = "idle-power-write"),
            in_game_poll_write_compiled: cfg!(feature = "ingame-poll-write"),
            snap_tap_write_compiled: cfg!(feature = "snap-tap-write") }
    }
}

fn os_version() -> Option<[u32; 3]> {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct Version { size: u32, major: u32, minor: u32, build: u32, platform: u32, service_pack: [u16; 128] }
        #[link(name = "ntdll")]
        extern "system" { fn RtlGetVersion(version: *mut Version) -> i32; }
        let mut version = Version { size: std::mem::size_of::<Version>() as u32, major: 0, minor: 0,
            build: 0, platform: 0, service_pack: [0; 128] };
        // SAFETY: correctly sized RTL_OSVERSIONINFOW buffer; this query reads OS version only.
        if unsafe { RtlGetVersion(&raw mut version) } == 0 { Some([version.major, version.minor, version.build]) } else { None }
    }
    #[cfg(target_os = "linux")]
    {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
        let mut parts = release.trim().split(['.', '-']);
        Some([parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?])
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    { None }
}

/// Exportable interface: deliberately has no raw path, container ID or device-provided string.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub id: u32,
    pub device: u32,
    pub vid: u16,
    pub pid: u16,
    pub revision: Option<u16>,
    pub bus_type: Option<u16>,
    pub grouping: Grouping,
    pub usage_page: u16,
    pub usage: u16,
    pub input_len: u16,
    pub output_len: u16,
    pub feature_len: u16,
    pub descriptor_source: DescriptorSource,
    pub descriptor_complete: bool,
    pub fields: Vec<Field>,
    pub links: Vec<LinkCollection>,
    pub descriptor_fingerprint: String,
    pub recognition: Recognition,
    pub input_capture_eligible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode { UnknownProtocol, UnboundVendorInterface, DescriptorIncomplete, DeclaredUnverified, InputVariation }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextExperiment { ReviewProtocolEvidence, PassiveInputCapture, CollectDescriptor, ReadOnlySystemBench, RepeatIsolatedControl }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: u32,
    pub device: u32,
    pub interfaces: Vec<u32>,
    pub code: FindingCode,
    pub next_experiment: NextExperiment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BitVariation {
    pub report_id: u8,
    pub report_len: usize,
    pub byte: usize,
    pub bit: u8,
    pub rising: u32,
    pub falling: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputAnalysis {
    pub interface: u32,
    pub sample_count: usize,
    pub end: RecordingEnd,
    pub variations: Vec<BitVariation>,
}

/// Enumerate through the normal transport policy. No registry filtering or auto-adoption occurs.
pub fn collect(registry: &Registry) -> Result<LocalDossier> {
    collect_with_registry(Some(registry))
}

/// Descriptor discovery remains usable when the local device registry cannot be loaded.
pub fn collect_with_registry(registry: Option<&Registry>) -> Result<LocalDossier> {
    let mut infos = transport::enumerate()?;
    infos.sort_by_key(|i| (i.vid, i.pid, i.path.as_os_str().to_os_string()));
    let mut groups = BTreeMap::new();
    let mut interfaces = Vec::new();
    for (index, info) in infos.into_iter().enumerate() {
        let metadata = transport::research_metadata(&info.path).unwrap_or_else(|_| Metadata::unavailable());
        let key = metadata.group_key.clone().unwrap_or_else(|| info.instance());
        let next = groups.len() as u32 + 1;
        let device = *groups.entry((metadata.grouping, key)).or_insert(next);
        let definition = registry.and_then(|r| r.find_for_pipe(&info));
        let recognition = Recognition {
            signature_dialects: crate::dialect::dialects().iter().filter(|d| d.claims(&info)).map(|d| d.id().into()).collect(),
            registry_match: definition.is_some(),
            declared_capabilities: definition.map(|d| d.capabilities().iter().map(|c| c.label().into()).collect()).unwrap_or_default(),
            protocol_tested: false, device_write_tested: false, physical_output_tested: false,
        };
        interfaces.push(LocalInterface {
            id: index as u32 + 1, device, vid: info.vid, pid: info.pid,
            usage_page: info.usage_page, usage: info.usage, input_len: info.input_len,
            output_len: info.output_len, feature_len: info.feature_len,
            path: info.path.as_os_str().to_string_lossy().into_owned(), product: info.product,
            metadata, recognition,
        });
    }
    Ok(LocalDossier {
        schema: SCHEMA, neuron_version: env!("CARGO_PKG_VERSION").into(), platform: std::env::consts::OS.into(), environment: Environment::current(),
        registry_available: registry.is_some(),
        collected_unix_seconds: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        interfaces, recordings: Vec::new(),
    })
}

fn fingerprint(interface: &Interface) -> Result<String> {
    let shape = (&interface.descriptor_source, interface.bus_type, interface.usage_page, interface.usage,
        interface.input_len, interface.output_len, interface.feature_len, &interface.fields, &interface.links);
    Ok(hash(&serde_json::to_vec(&shape)?))
}

pub fn inventory(local: &LocalDossier) -> Result<Inventory> {
    validate_local(local)?;
    let mut interfaces = Vec::new();
    for i in &local.interfaces {
        let mut shared = Interface {
            id: i.id, device: i.device, vid: i.vid, pid: i.pid, revision: i.metadata.revision, bus_type: i.metadata.bus_type,
            grouping: i.metadata.grouping, usage_page: i.usage_page, usage: i.usage,
            input_len: i.input_len, output_len: i.output_len, feature_len: i.feature_len,
            descriptor_source: i.metadata.source, descriptor_complete: i.metadata.complete,
            fields: i.metadata.fields.clone(), links: i.metadata.links.clone(), descriptor_fingerprint: String::new(),
            recognition: i.recognition.clone(), input_capture_eligible: capture_eligible(i),
        };
        shared.descriptor_fingerprint = fingerprint(&shared)?;
        interfaces.push(shared);
    }
    Ok(Inventory { schema: SCHEMA, neuron_version: local.neuron_version.clone(), platform: local.platform.clone(), environment: local.environment.clone(), registry_available: local.registry_available, interfaces })
}

/// Select one dossier-scoped device group without renumbering its interfaces or losing trials.
/// This limits disclosure; it does not establish a physical association beyond the OS evidence.
pub fn select_device(local: &LocalDossier, device: u32) -> Result<LocalDossier> {
    validate_local(local)?;
    let interfaces = local.interfaces.iter().filter(|i| i.device == device).cloned().collect::<Vec<_>>();
    ensure!(!interfaces.is_empty(), "device ID not found in this dossier");
    let ids = interfaces.iter().map(|i| i.id).collect::<BTreeSet<_>>();
    let recordings = local.recordings.iter().filter(|r| ids.contains(&r.interface)).cloned().collect();
    Ok(LocalDossier { interfaces, recordings, ..local.clone() })
}

fn validate_inventory(inv: &Inventory) -> Result<()> {
    let local = LocalDossier {
        schema: inv.schema, neuron_version: inv.neuron_version.clone(), platform: inv.platform.clone(),
        environment: inv.environment.clone(),
        registry_available: inv.registry_available,
        collected_unix_seconds: 0, recordings: Vec::new(),
        interfaces: inv.interfaces.iter().map(|i| LocalInterface {
            id: i.id, device: i.device, vid: i.vid, pid: i.pid, usage_page: i.usage_page,
            usage: i.usage, input_len: i.input_len, output_len: i.output_len, feature_len: i.feature_len,
            path: String::new(), product: String::new(), recognition: i.recognition.clone(),
            metadata: Metadata { source: i.descriptor_source, grouping: i.grouping, revision: i.revision, bus_type: i.bus_type,
                fields: i.fields.clone(), links: i.links.clone(), complete: i.descriptor_complete,
                ..Metadata::unavailable() },
        }).collect(),
    };
    validate_local(&local)?;
    for (i, l) in inv.interfaces.iter().zip(&local.interfaces) {
        ensure!(fingerprint(i)? == i.descriptor_fingerprint && capture_eligible(l) == i.input_capture_eligible,
            "descriptor fingerprint or capture eligibility mismatch");
    }
    Ok(())
}

fn capture_eligible(i: &LocalInterface) -> bool {
    i.input_len > 0 && i.input_len as usize <= MAX_REPORT_BYTES
        && i.usage_page >= 0xff00 && i.metadata.complete
        && i.metadata.fields.iter().any(|f| f.kind == ReportKind::Input)
        && !i.metadata.fields.iter().any(|f| f.kind == ReportKind::Input && f.usage_page == 0x07)
        && !i.metadata.links.iter().any(|c| c.usage_page == 0x07 || (c.usage_page == 1 && c.usage == 6))
}

/// Passive vendor-input recording only. Caller must obtain consent before invoking this function.
/// The selected path must still match the original dossier; a reconnect requires a new collection.
pub fn capture(local: &LocalDossier, interface: u32, seconds: u64) -> Result<InputRecording> {
    validate_local(local)?;
    ensure!((1..=30).contains(&seconds), "capture duration must be 1..30 seconds");
    let selected = local.interfaces.iter().find(|i| i.id == interface).context("interface ID not found")?;
    ensure!(capture_eligible(selected), "capture requires a complete vendor-input descriptor without keyboard fields");
    let current = transport::enumerate()?.into_iter().find(|i| i.path.as_os_str() == std::ffi::OsStr::new(&selected.path))
        .context("selected interface disappeared; collect a new dossier")?;
    ensure!(same_shape(selected, &current), "interface identity or report shape changed; collect a new dossier");
    let metadata = transport::research_metadata(&current.path)?;
    let check = LocalInterface { metadata, ..selected.clone() };
    ensure!(capture_eligible(&check) && check.metadata.fields == selected.metadata.fields
        && check.metadata.links == selected.metadata.links && check.metadata.revision == selected.metadata.revision
        && check.metadata.grouping == selected.metadata.grouping && check.metadata.group_key == selected.metadata.group_key,
        "descriptor or device association changed; collect a new dossier");
    let reader = transport::open_reader(&current.path).context("selected interface is not readable")?;
    record_from(reader.as_ref(), interface, seconds, selected.input_len as usize)
}

fn same_shape(i: &LocalInterface, current: &HidDeviceInfo) -> bool {
    (i.vid, i.pid, i.usage_page, i.usage, i.input_len, i.output_len, i.feature_len)
        == (current.vid, current.pid, current.usage_page, current.usage, current.input_len, current.output_len, current.feature_len)
}

fn record_from(reader: &dyn InputReader, interface: u32, seconds: u64, length: usize) -> Result<InputRecording> {
    ensure!((1..=30).contains(&seconds) && (1..=MAX_REPORT_BYTES).contains(&length), "invalid capture bounds");
    let start = Instant::now();
    let deadline = Duration::from_secs(seconds);
    let mut recording = InputRecording { interface, recipe: "vendor-input-v1".into(), duration_ms: 0,
        end: RecordingEnd::Deadline, samples: Vec::new() };
    let mut buffer = vec![0u8; length];
    let mut captured = 0;
    while start.elapsed() < deadline {
        match reader.read(&mut buffer) {
            Ok(Some(n)) if n > 0 && n <= length => {
                if captured + n > MAX_CAPTURE_BYTES { recording.end = RecordingEnd::ByteLimit; break; }
                captured += n;
                recording.samples.push(InputSample {
                    elapsed_us: start.elapsed().as_micros() as u64, bytes: buffer[..n].to_vec(),
                });
            }
            Ok(Some(n)) if n > length => { recording.end = RecordingEnd::ReadFailed; break; }
            Ok(_) => std::thread::sleep(Duration::from_millis(1)),
            Err(_) => { recording.end = RecordingEnd::ReadFailed; break; }
        }
        if recording.samples.len() == MAX_REPORTS { recording.end = RecordingEnd::ReportLimit; break; }
    }
    recording.duration_ms = start.elapsed().as_millis() as u64;
    Ok(recording)
}

/// Replay observations into a neutral bit-variation analyzer. Variation is not a semantic mapping.
pub fn analyze(recording: &InputRecording) -> Result<InputAnalysis> {
    validate_recording(recording)?;
    let mut previous: BTreeMap<(u8, usize), &[u8]> = BTreeMap::new();
    let mut changes: BTreeMap<(u8, usize, usize, u8), (u32, u32)> = BTreeMap::new();
    for sample in &recording.samples {
        let bytes = &sample.bytes;
        let key = (bytes[0], bytes.len());
        if let Some(last) = previous.insert(key, bytes) {
            for (byte, (&old, &new)) in last.iter().zip(bytes).enumerate().skip(1) {
                for bit in 0..8 {
                    let mask = 1 << bit;
                    if (old ^ new) & mask != 0 {
                        let count = changes.entry((key.0, key.1, byte, bit)).or_default();
                        if new & mask != 0 { count.0 += 1; } else { count.1 += 1; }
                    }
                }
            }
        }
    }
    Ok(InputAnalysis {
        interface: recording.interface, sample_count: recording.samples.len(), end: recording.end,
        variations: changes.into_iter().map(|((report_id, report_len, byte, bit), (rising, falling))|
            BitVariation { report_id, report_len, byte, bit, rising, falling }).collect(),
    })
}

pub fn findings(inventory: &Inventory, analyses: &[InputAnalysis]) -> Vec<Finding> {
    let mut grouped: BTreeMap<(u32, u8), Vec<u32>> = BTreeMap::new();
    for i in &inventory.interfaces {
        let vendor = i.usage_page >= 0xff00;
        if vendor && i.recognition.signature_dialects.is_empty() && (i.feature_len > 0 || i.output_len > 0) {
            grouped.entry((i.device, 0)).or_default().push(i.id);
        } else if vendor && inventory.registry_available && !i.recognition.registry_match {
            grouped.entry((i.device, 1)).or_default().push(i.id);
        }
        if !i.descriptor_complete { grouped.entry((i.device, 2)).or_default().push(i.id); }
        if i.recognition.registry_match { grouped.entry((i.device, 3)).or_default().push(i.id); }
    }
    for a in analyses {
        if !a.variations.is_empty() {
            if let Some(i) = inventory.interfaces.iter().find(|i| i.id == a.interface) {
                grouped.entry((i.device, 4)).or_default().push(i.id);
            }
        }
    }
    grouped.into_iter().enumerate().map(|(index, ((device, code), mut interfaces))| {
        interfaces.sort_unstable();
        interfaces.dedup();
        let capture_available = inventory.interfaces.iter().any(|i| interfaces.contains(&i.id) && i.input_capture_eligible);
        let (code, next_experiment) = match code {
            0 => (FindingCode::UnknownProtocol, NextExperiment::ReviewProtocolEvidence),
            1 => (FindingCode::UnboundVendorInterface, if capture_available {
                NextExperiment::PassiveInputCapture
            } else { NextExperiment::ReviewProtocolEvidence }),
            2 => (FindingCode::DescriptorIncomplete, NextExperiment::CollectDescriptor),
            3 => (FindingCode::DeclaredUnverified, NextExperiment::ReadOnlySystemBench),
            _ => (FindingCode::InputVariation, NextExperiment::RepeatIsolatedControl),
        };
        Finding { id: index as u32 + 1, device, interfaces, code, next_experiment }
    }).collect()
}

fn validate_recording(recording: &InputRecording) -> Result<()> {
    ensure!(recording.recipe == "vendor-input-v1", "unsupported input recipe");
    ensure!(recording.duration_ms <= 35_000 && recording.samples.len() <= MAX_REPORTS, "recording exceeds bounds");
    ensure!(recording.samples.iter().map(|s| s.bytes.len()).sum::<usize>() <= MAX_CAPTURE_BYTES, "recording exceeds byte budget");
    let mut last = 0;
    for sample in &recording.samples {
        ensure!(!sample.bytes.is_empty() && sample.bytes.len() <= MAX_REPORT_BYTES, "invalid input report size");
        ensure!(sample.elapsed_us >= last && sample.elapsed_us <= recording.duration_ms * 1000 + 1000, "invalid sample time");
        last = sample.elapsed_us;
    }
    Ok(())
}

fn validate_local(local: &LocalDossier) -> Result<()> {
    ensure!(local.schema == SCHEMA, "unsupported compatibility schema");
    ensure!(local.neuron_version.len() <= 40 && local.neuron_version.bytes().all(|b| b.is_ascii_digit() || b".-+abcdefghijklmnopqrstuvwxyz".contains(&b)), "invalid build version");
    ensure!(["windows", "linux", "macos"].contains(&local.platform.as_str()), "unsupported platform field");
    ensure!(["x86", "x86_64", "arm", "aarch64", "riscv64"].contains(&local.environment.architecture.as_str()), "unsupported architecture field");
    ensure!(local.interfaces.len() <= 4096 && local.recordings.len() <= 32, "dossier exceeds bounds");
    let mut ids = BTreeSet::new();
    for i in &local.interfaces {
        ensure!(i.id > 0 && i.device > 0 && ids.insert(i.id), "duplicate or invalid interface ID");
        ensure!(i.metadata.fields.len() <= 65_536 && i.metadata.links.len() <= 65_536, "descriptor exceeds bounds");
        ensure!(i.recognition.signature_dialects.iter().all(|s| crate::dialect::by_id(s).is_some()), "unknown dialect in evidence");
        // These fields are authored by collection, never accepted as claims from imported JSON.
        ensure!(!i.recognition.protocol_tested && !i.recognition.device_write_tested && !i.recognition.physical_output_tested,
            "this collector cannot attest live protocol or output verification");
        ensure!(local.registry_available || (!i.recognition.registry_match && i.recognition.declared_capabilities.is_empty()), "capability claims without a loaded registry");
        let capability_labels: BTreeSet<_> = crate::registry::Capability::ALL.iter().map(|c| c.label()).collect();
        ensure!(i.recognition.declared_capabilities.iter().all(|s| capability_labels.contains(s.as_str())), "unknown capability label");
    }
    for r in &local.recordings {
        validate_recording(r)?;
        let i = local.interfaces.iter().find(|i| i.id == r.interface).context("recording has no interface")?;
        ensure!(capture_eligible(i), "recording targets an ineligible interface");
        ensure!(r.samples.iter().all(|s| s.bytes.len() <= i.input_len as usize), "report exceeds declared input length");
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> String { format!("{:x}", Sha256::digest(bytes)) }

pub fn read_local(directory: &Path) -> Result<LocalDossier> {
    let local: LocalDossier = read_json(&directory.join("local.json"))?;
    validate_local(&local)?;
    Ok(local)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    ensure!(std::fs::symlink_metadata(path)?.file_type().is_file(), "evidence must be a regular file");
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "evidence file exceeds 16 MiB");
    serde_json::from_slice(&bytes).context("invalid evidence JSON")
}

fn json<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Write a new local dossier; existing destinations are never overwritten.
pub fn write_local(directory: &Path, local: &LocalDossier) -> Result<()> {
    validate_local(local)?;
    let files = BTreeMap::from([("local.json".into(), json(local)?)]);
    write_directory(directory, &files)
}

/// Append to the local dossier atomically, preserving the previous file on failure.
pub fn append_recording(directory: &Path, recording: InputRecording) -> Result<()> {
    let lock_path = directory.join("capture.lock");
    let lock = std::fs::OpenOptions::new().write(true).create_new(true).open(&lock_path)
        .context("dossier is being updated; a stale capture.lock requires manual review")?;
    struct Lock<'a>(&'a Path, std::fs::File);
    impl Drop for Lock<'_> {
        fn drop(&mut self) { let _ = self.1.sync_all(); let _ = std::fs::remove_file(self.0); }
    }
    let _lock = Lock(&lock_path, lock);
    let mut local = read_local(directory)?;
    local.recordings.push(recording);
    validate_local(&local)?;
    let bytes = json(&local)?;
    ensure!(bytes.len() as u64 <= MAX_FILE, "dossier exceeds 16 MiB; collect a new dossier for further trials");
    crate::salvage::atomic_write(&directory.join("local.json"), &bytes)?;
    Ok(())
}

#[allow(clippy::create_dir)] // The staging directory must be exclusive, not a reused partial export.
fn write_directory(directory: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    ensure!(!directory.exists(), "destination already exists; choose a new directory");
    let parent = directory.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let staging = parent.join(format!(".neuron-compat-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&staging)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))?;
    }
    let result = (|| {
        for (name, bytes) in files {
            ensure!(safe_name(name), "invalid artifact name");
            ensure!(bytes.len() as u64 <= MAX_FILE, "artifact exceeds 16 MiB");
            let path = staging.join(name);
            if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(path)?;
            use std::io::Write as _;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&staging, directory)?;
        Ok(())
    })();
    if result.is_err() && std::fs::canonicalize(&staging)?.parent() == Some(std::fs::canonicalize(parent)?.as_path()) {
        let _ = std::fs::remove_dir_all(staging);
    }
    result
}

fn safe_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 200 && name.split('/').all(|p|
        !p.is_empty() && p != "." && p != ".." && p.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u32,
    pub collector_version: String,
    pub recipe: String,
    pub includes_raw_input: bool,
    pub includes_raw_descriptors: bool,
    pub artifacts: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawDescriptor {
    pub interface: u32,
    pub hex: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ExportOptions {
    pub include_input: bool,
    pub include_descriptors: bool,
}

fn privacy(options: ExportOptions) -> serde_json::Value {
    serde_json::json!({
        "projection": "typed-allowlist-v1",
        "excluded": ["serials", "paths", "container_ids", "device_strings", "driver_strings", "config", "logs", "wall_clock", "host_identity"],
        "raw_input_included": options.include_input,
        "raw_descriptors_included": options.include_descriptors,
        "raw_data_warning": "Vendor input and raw descriptor payloads may contain identifiers or private data; review before sharing."
    })
}

/// Construct a shareable allowlist, never a redacted copy of the local filesystem.
/// Raw input is optional and requires separate review and consent to share.
pub fn export(directory: &Path, local: &LocalDossier, include_input: bool) -> Result<Manifest> {
    export_with(directory, local, ExportOptions { include_input, include_descriptors: false })
}

pub fn export_with(directory: &Path, local: &LocalDossier, options: ExportOptions) -> Result<Manifest> {
    let inv = inventory(local)?;
    let analyses = local.recordings.iter().map(analyze).collect::<Result<Vec<_>>>()?;
    let findings = findings(&inv, &analyses);
    let mut files = BTreeMap::new();
    files.insert("inventory.json".into(), json(&inv)?);
    files.insert("findings.json".into(), json(&findings)?);
    files.insert("observations/analysis.json".into(), json(&analyses)?);
    if options.include_input { files.insert("observations/input.json".into(), json(&local.recordings)?); }
    if options.include_descriptors {
        let descriptors = local.interfaces.iter().filter_map(|i| i.metadata.raw_descriptor_hex.as_ref()
            .map(|hex| RawDescriptor { interface: i.id, hex: hex.clone() })).collect::<Vec<_>>();
        for descriptor in &descriptors { transport::research::descriptor_shapes(&descriptor.hex)?; }
        files.insert("observations/raw-descriptors.json".into(), json(&descriptors)?);
    }
    files.insert("privacy.json".into(), json(&privacy(options))?);
    files.insert("summary.md".into(), summary(&inv, &findings).into_bytes());
    for i in &inv.interfaces {
        let name = format!("descriptors/{:04x}-{:04x}-{}-{}.json", i.vid, i.pid,
            i.revision.map_or("unknown".into(), |r| format!("{r:04x}")), &i.descriptor_fingerprint[..16]);
        files.entry(name).or_insert(json(i)?);
    }
    for finding in &findings {
        if finding.code != FindingCode::DeclaredUnverified {
            files.insert(format!("issues/finding-{}.md", finding.id), issue(&inv, finding).into_bytes());
        }
    }
    let manifest = Manifest {
        schema: SCHEMA, collector_version: local.neuron_version.clone(), recipe: "inventory-v1".into(),
        includes_raw_input: options.include_input, includes_raw_descriptors: options.include_descriptors,
        artifacts: files.iter().map(|(name, bytes)| (name.clone(), hash(bytes))).collect(),
    };
    files.insert("manifest.json".into(), json(&manifest)?);
    write_directory(directory, &files)?;
    Ok(manifest)
}

pub fn summary(inv: &Inventory, findings: &[Finding]) -> String {
    let mut text = format!("# Neuron compatibility evidence\n\nNeuron {} on {} / {}, OS version {:?}. {} HID collections observed.\n\n", inv.neuron_version, inv.platform, inv.environment.architecture, inv.environment.os_version, inv.interfaces.len());
    text.push_str("Inventory only: no protocol probes, device writes, live dispatch or physical-output verification. Device groups carry their source; path groups remain heuristic.\n\n");
    for i in &inv.interfaces {
        let _ = writeln!(text, "- Device {} / interface {}: `{:04x}:{:04x}`, usage `{:04x}/{:04x}`, input/output/feature {}/{}/{} bytes, descriptor {:?}; registry match {}.",
            i.device, i.id, i.vid, i.pid, i.usage_page, i.usage, i.input_len, i.output_len, i.feature_len, i.descriptor_source, i.recognition.registry_match);
    }
    text.push_str("\nFindings are research leads, not diagnosed faults or verified capability claims.\n\n");
    for f in findings { let _ = writeln!(text, "- Finding {} / device {}: {:?}; next {:?}.", f.id, f.device, f.code, f.next_experiment); }
    text
}

fn issue(inv: &Inventory, f: &Finding) -> String {
    let (title, observation, task, limits) = match f.code {
        FindingCode::UnknownProtocol => ("Investigate an unrecognized HID command surface",
            "A vendor collection exposes output or feature reports, but no current Neuron dialect claims its signature.",
            "Identify an authoritative protocol reference or an approved vendor-software capture. Implement a dialect only after the framing and command semantics are established.",
            "An exposed report is not evidence of lighting, DPI or any particular command. No output experiment was performed."),
        FindingCode::UnboundVendorInterface => ("Investigate an unbound vendor HID collection",
            "The collection has no matching Neuron control definition. This may be an input-only or ancillary function, not a missing device driver.",
            "Review the numeric descriptor. If capture is eligible, compare idle and three isolated press/release trials, then add a decoder fixture before proposing a mapping.",
            "A control-definition mismatch does not establish whether the running app already observes this collection through another source."),
        FindingCode::DescriptorIncomplete => ("Collect the missing HID descriptor evidence",
            "Complete normalized descriptor fields were unavailable for one or more collections.",
            "Compare a new collection on the same device/link mode. On Linux, retain and review the local raw descriptor; on Windows, check the HID parser/access result.",
            "Missing metadata does not prove the device lacks controls. The collector cannot report interfaces the OS backend skipped."),
        FindingCode::InputVariation => ("Investigate recorded vendor-input bit changes",
            "The passive recording contains changing report bits, summarized in observations/analysis.json.",
            "Compare isolated repeated trials with an idle control. Rule out counters, checksums and telemetry before connecting a bit to a semantic input. Add a replay regression for the actual decoder.",
            "Variation alone is not a button mapping. Trial intent and physical action need the reporter's confirmation."),
        FindingCode::DeclaredUnverified => ("Verify declared capabilities", "A registry entry exists.", "Run the existing read-only SYSTEM bench.", "No hardware verification was performed."),
    };
    let hardware = inv.interfaces.iter().filter(|i| f.interfaces.contains(&i.id)).map(|i|
        format!("- Interface {}: VID/PID `{:04x}:{:04x}`, revision {:?}, usage `{:04x}/{:04x}`, grouping {:?}, descriptor fingerprint `{}`.\n",
            i.id, i.vid, i.pid, i.revision, i.usage_page, i.usage, i.grouping, i.descriptor_fingerprint)).collect::<String>();
    let risk = if f.code == FindingCode::UnknownProtocol { ", `risky`" } else { "" };
    format!("# {title}\n\nSuggested labels: `enhancement`, `area:device`, `intake:compat-lab`, `needs-triage`{risk}. Change the type/area if the reporter establishes an existing regression.\n\n## Observed evidence\n\n{observation}\n\n{hardware}\nNeuron {} on {}. Finding {} in `findings.json`.\n\n## Investigation\n\n{task}\n\n## Starting points\n\n`neuron-core::transport::research`, `dialect::Dialect::claims`, `registry::Registry::find_for_pipe` and `neuron-testkit::compat` provide the observation, route and replay seams. Recheck current source before editing. Device-write or interception changes require owner approval.\n\n## Unverified\n\n{limits}\n\n## Reporter context\n\nCopy this draft outside the bundle before editing. Add the model name, connection mode, expected behaviour and what physically happened. Attach the verified shareable bundle or its accessible link, including `manifest.json`. Do not attach `local.json`.\n\n## Acceptance\n\nA fixture-backed implementation or a documented protocol gap; physical verification stays outstanding until the reporter confirms it. No guessed firmware writes.\n",
        inv.neuron_version, inv.platform, f.id)
}

/// Check bundle hashes and schemas offline. Integrity is not hardware attestation.
pub fn verify(directory: &Path) -> Result<Manifest> {
    if directory.is_file() { return verify_archive(directory); }
    let manifest: Manifest = read_json(&directory.join("manifest.json"))?;
    ensure!(manifest.schema == SCHEMA && manifest.recipe == "inventory-v1", "unsupported bundle schema or recipe");
    ensure!(manifest.artifacts.len() <= 8192, "too many artifacts");
    let root = std::fs::canonicalize(directory)?;
    for (name, expected) in &manifest.artifacts {
        ensure!(safe_name(name) && allowed_artifact(name), "unexpected artifact name");
        let path = directory.join(name);
        ensure!(std::fs::canonicalize(&path)?.starts_with(&root), "artifact escapes bundle");
        ensure!(std::fs::symlink_metadata(&path)?.file_type().is_file(), "artifact must be a regular file");
        let mut bytes = Vec::new();
        std::fs::File::open(path)?.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() as u64 <= MAX_FILE && hash(&bytes) == *expected, "artifact hash mismatch or size exceeded: {name}");
    }
    let inv: Inventory = read_json(&directory.join("inventory.json"))?;
    validate_inventory(&inv)?;
    let privacy_record: serde_json::Value = read_json(&directory.join("privacy.json"))?;
    ensure!(privacy_record == privacy(ExportOptions { include_input: manifest.includes_raw_input,
        include_descriptors: manifest.includes_raw_descriptors }), "privacy projection mismatch");
    for name in manifest.artifacts.keys().filter(|name| name.starts_with("descriptors/")) {
        let descriptor: Interface = read_json(&directory.join(name))?;
        ensure!(inv.interfaces.iter().any(|i| i.id == descriptor.id
            && serde_json::to_value(i).ok() == serde_json::to_value(&descriptor).ok()), "descriptor does not match inventory");
    }
    ensure!(inv.schema == SCHEMA && inv.neuron_version == manifest.collector_version, "inventory version mismatch");
    let observed_findings: Vec<Finding> = read_json(&directory.join("findings.json"))?;
    let analyses: Vec<InputAnalysis> = read_json(&directory.join("observations/analysis.json"))?;
    ensure!(analyses.len() <= 32, "too many input analyses");
    for a in &analyses {
        let i = inv.interfaces.iter().find(|i| i.id == a.interface).context("analysis has no interface")?;
        ensure!(i.input_capture_eligible && a.sample_count <= MAX_REPORTS, "analysis targets an ineligible interface or exceeds bounds");
        let mut bits = BTreeSet::new();
        for v in &a.variations {
            ensure!(v.report_len <= i.input_len as usize && v.byte > 0 && v.byte < v.report_len && v.bit < 8
                && (v.rising as u64 + v.falling as u64) > 0
                && (v.rising as u64 + v.falling as u64) < a.sample_count as u64
                && bits.insert((v.report_id, v.report_len, v.byte, v.bit)), "invalid bit-variation analysis");
        }
    }
    ensure!(observed_findings == findings(&inv, &analyses), "findings do not match descriptor and analysis evidence");
    ensure!(manifest.includes_raw_input == manifest.artifacts.contains_key("observations/input.json"), "input privacy flag mismatch");
    if manifest.includes_raw_input {
        let recordings: Vec<InputRecording> = read_json(&directory.join("observations/input.json"))?;
        ensure!(recordings.len() <= 32, "too many recordings");
        for r in &recordings {
            validate_recording(r)?;
            let i = inv.interfaces.iter().find(|i| i.id == r.interface).context("recording has no interface")?;
            ensure!(i.input_capture_eligible && r.samples.iter().all(|s| s.bytes.len() <= i.input_len as usize),
                "recording is incompatible with descriptor");
        }
        ensure!(recordings.iter().map(analyze).collect::<Result<Vec<_>>>()? == analyses,
            "input analysis does not match recorded reports");
    }
    ensure!(manifest.includes_raw_descriptors == manifest.artifacts.contains_key("observations/raw-descriptors.json"), "descriptor privacy flag mismatch");
    if manifest.includes_raw_descriptors {
        let descriptors: Vec<RawDescriptor> = read_json(&directory.join("observations/raw-descriptors.json"))?;
        ensure!(descriptors.len() <= 4096, "too many raw descriptors");
        for descriptor in descriptors {
            ensure!(inv.interfaces.iter().any(|i| i.id == descriptor.interface && i.descriptor_source == DescriptorSource::LinuxSysfs), "raw descriptor has no matching Linux interface");
            transport::research::descriptor_shapes(&descriptor.hex)?;
        }
    }
    let listed: BTreeSet<_> = manifest.artifacts.keys().cloned().chain(std::iter::once("manifest.json".into())).collect();
    let mut present = BTreeSet::new();
    list_files(directory, directory, &mut present)?;
    ensure!(listed == present, "unlisted or missing files in shareable bundle");
    Ok(manifest)
}

/// Package only the verified shareable artifacts. The private dossier is never traversed.
pub fn pack(directory: &Path, destination: &Path) -> Result<Manifest> {
    use std::io::Write as _;
    ensure!(directory.is_dir(), "pack requires a shareable bundle directory");
    let manifest = verify(directory)?;
    let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut total = 0;
    for name in manifest.artifacts.keys().map(String::as_str).chain(std::iter::once("manifest.json")) {
        let mut bytes = Vec::new();
        std::fs::File::open(directory.join(name))?.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        total += bytes.len() as u64;
        ensure!(bytes.len() as u64 <= MAX_FILE && total <= MAX_ARCHIVE, "archive exceeds evidence limits");
        if name != "manifest.json" {
            ensure!(manifest.artifacts.get(name) == Some(&hash(&bytes)), "artifact changed during packaging");
        } else {
            ensure!(bytes == json(&manifest)?, "manifest changed during packaging");
        }
        archive.start_file(name, options)?;
        archive.write_all(&bytes)?;
    }
    let bytes = archive.finish()?.into_inner();
    ensure!(bytes.len() as u64 <= MAX_ARCHIVE, "compressed archive exceeds limits");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(destination).context("archive destination must be a new file in an existing directory")?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(manifest)
}

fn verify_archive(path: &Path) -> Result<Manifest> {
    with_archive(path, verify)
}

fn with_archive<T>(path: &Path, action: fn(&Path) -> Result<T>) -> Result<T> {
    ensure!(std::fs::symlink_metadata(path)?.file_type().is_file(), "archive must be a regular file");
    ensure!(std::fs::metadata(path)?.len() <= MAX_ARCHIVE, "archive exceeds 64 MiB");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    ensure!(archive.len() <= 8193, "too many archive entries");
    let mut files = BTreeMap::new();
    let mut total = 0;
    for n in 0..archive.len() {
        let entry = archive.by_index(n)?;
        let name = entry.name().to_string();
        ensure!(safe_name(&name) && (name == "manifest.json" || allowed_artifact(&name)), "unexpected archive entry");
        ensure!(!entry.is_dir() && entry.size() <= MAX_FILE, "invalid archive entry size or kind");
        ensure!(entry.unix_mode().is_none_or(|mode| mode & 0o170000 != 0o120000), "archive contains a symlink");
        let mut bytes = Vec::new();
        entry.take(MAX_FILE + 1).read_to_end(&mut bytes)?;
        total += bytes.len() as u64;
        ensure!(bytes.len() as u64 <= MAX_FILE && total <= MAX_ARCHIVE, "expanded archive exceeds evidence limits");
        ensure!(files.insert(name, bytes).is_none(), "duplicate archive entry");
    }
    let temp = std::fs::canonicalize(std::env::temp_dir())?;
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let scratch = temp.join(format!("neuron-compat-verify-{}-{nonce}", std::process::id()));
    write_directory(&scratch, &files)?;
    let result = action(&scratch);
    let resolved = std::fs::canonicalize(&scratch)?;
    ensure!(resolved.parent() == Some(temp.as_path()), "verification directory escaped temp root");
    std::fs::remove_dir_all(resolved)?;
    result
}

/// Reproduce analysis from the exact shared reports, with no enumeration or device handles.
pub fn replay(directory: &Path) -> Result<Vec<InputAnalysis>> {
    if directory.is_file() { return with_archive(directory, replay); }
    let manifest = verify(directory)?;
    ensure!(manifest.includes_raw_input, "bundle has no approved raw input; summary counts cannot be replayed");
    let recordings: Vec<InputRecording> = read_json(&directory.join("observations/input.json"))?;
    recordings.iter().map(analyze).collect()
}

fn allowed_artifact(name: &str) -> bool {
    if ["inventory.json", "findings.json", "privacy.json", "summary.md", "observations/analysis.json", "observations/input.json", "observations/raw-descriptors.json"].contains(&name) { return true; }
    if let Some(id) = name.strip_prefix("issues/finding-").and_then(|s| s.strip_suffix(".md")) {
        return id.parse::<u32>().is_ok_and(|id| id > 0);
    }
    if let Some(fixture) = name.strip_prefix("descriptors/").and_then(|s| s.strip_suffix(".json")) {
        let parts = fixture.split('-').collect::<Vec<_>>();
        let hex = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
        // Fixed numeric names also exclude Windows DOS devices (CON, COM1, NUL, etc.).
        return parts.len() == 4 && hex(parts[0], 4) && hex(parts[1], 4)
            && (parts[2] == "unknown" || hex(parts[2], 4)) && hex(parts[3], 16);
    }
    false
}

fn list_files(root: &Path, directory: &Path, files: &mut BTreeSet<String>) -> Result<()> {
    ensure!(std::fs::symlink_metadata(directory)?.file_type().is_dir(), "bundle directories must not be symlinks");
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            ensure!(directory == root && ["descriptors", "issues", "observations"].contains(&entry.file_name().to_string_lossy().as_ref()),
                "unexpected bundle directory");
            list_files(root, &entry.path(), files)?;
        }
        else {
            ensure!(ty.is_file(), "bundle contains a non-regular file");
            files.insert(entry.path().strip_prefix(root)?.to_string_lossy().replace('\\', "/"));
        }
        ensure!(files.len() <= 8193, "too many files in bundle");
    }
    Ok(())
}

#[cfg(test)]
#[path = "compat_tests.rs"]
mod tests;
