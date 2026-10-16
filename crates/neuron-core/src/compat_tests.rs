// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use super::*;
use crate::transport::{Backend, DevicePath, Transport};
use std::sync::{Arc, Mutex};

fn field(page: u16) -> Field {
    Field { kind: ReportKind::Input, button: true, alias: false, is_range: true,
        data_index_min: 0, data_index_max: 7, report_id: 5, usage_page: page,
        usage_min: 1, usage_max: 8, link_collection: 0, bit_field: 2, absolute: true,
        bit_size: None, report_count: 8, logical_min: None, logical_max: None,
        physical_min: None, physical_max: None, units: None, units_exponent: None, has_null: false }
}

fn local() -> LocalDossier {
    LocalDossier { schema: SCHEMA, neuron_version: env!("CARGO_PKG_VERSION").into(), platform: "windows".into(),
        environment: Environment::current(),
        registry_available: true,
        collected_unix_seconds: 1234,
        interfaces: vec![LocalInterface { id: 1, device: 1, vid: 0x1234, pid: 0x9876,
            usage_page: 0xff00, usage: 2, input_len: 3, output_len: 3, feature_len: 0,
            path: "PRIVATE_PATH_AND_SERIAL".into(), product: "PRIVATE_PRODUCT_STRING".into(),
            metadata: Metadata { source: DescriptorSource::WindowsHidParser, grouping: Grouping::WindowsContainer,
                group_key: Some("PRIVATE_CONTAINER".into()), revision: Some(0x100), manufacturer: Some("PRIVATE_MANUFACTURER".into()),
                driver_version: Some("PRIVATE_DRIVER".into()), raw_descriptor_hex: Some("PRIVATE_RAW_DESCRIPTOR".into()),
                fields: vec![field(0xff00)], complete: true, ..Metadata::unavailable() },
            recognition: Recognition { signature_dialects: Vec::new(), registry_match: false,
                declared_capabilities: Vec::new(), protocol_tested: false, device_write_tested: false, physical_output_tested: false } }],
        recordings: Vec::new() }
}

fn recording() -> InputRecording {
    InputRecording { interface: 1, recipe: "vendor-input-v1".into(), duration_ms: 10,
        end: RecordingEnd::Deadline,
        samples: [0, 1, 0, 1, 0, 1, 0].into_iter().enumerate().map(|(n, value)|
            InputSample { elapsed_us: n as u64 * 1000, bytes: vec![5, value, 0] }).collect() }
}

struct Scratch(std::path::PathBuf);
impl Scratch {
    #[allow(clippy::create_dir)] // Exclusive scratch paths must not reuse another test's evidence.
    fn new() -> Self {
        static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!("neuron-compat-test-{}-{}", std::process::id(), ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> std::path::PathBuf { self.0.join(name) }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let resolved = std::fs::canonicalize(&self.0).unwrap();
        assert!(resolved.starts_with(std::fs::canonicalize(std::env::temp_dir()).unwrap()));
        let _ = std::fs::remove_dir_all(resolved);
    }
}

#[test]
fn export_projects_only_allowlisted_evidence_and_is_replayable_after_consent() {
    let scratch = Scratch::new();
    let mut dossier = local();
    dossier.recordings.push(recording());
    let private = scratch.path("private");
    write_local(&private, &dossier).unwrap();
    let shared = scratch.path("shared");
    let manifest = export(&shared, &read_local(&private).unwrap(), false).unwrap();
    verify(&shared).unwrap();
    for name in manifest.artifacts.keys() {
        let text = std::fs::read_to_string(shared.join(name)).unwrap();
        assert!(!text.contains("PRIVATE_"), "leaked local data into {name}");
        assert!(!text.contains("elapsed_us"));
    }
    assert!(!manifest.includes_raw_input);
    assert!(replay(&shared).is_err());
    let approved = scratch.path("approved");
    export(&approved, &dossier, true).unwrap();
    let replayed = replay(&approved).unwrap();
    assert_eq!(replayed[0].sample_count, 7);
    assert_eq!(replayed[0].variations.len(), 1);
    assert_eq!((replayed[0].variations[0].rising, replayed[0].variations[0].falling), (3, 3));
    let archive = scratch.path("approved.zip");
    pack(&approved, &archive).unwrap();
    assert_eq!(replay(&archive).unwrap()[0].sample_count, 7);
    assert!(read_local(&private).unwrap().interfaces[0].product.contains("PRIVATE_"));
}

#[test]
fn archive_scratch_cleanup_retries_only_bounded_directory_not_empty_errors() {
    let path = std::path::Path::new("scratch");
    let mut attempts = 0;
    remove_archive_scratch(path, |_| {
        attempts += 1;
        if attempts == 1 {
            Err(std::io::Error::new(std::io::ErrorKind::DirectoryNotEmpty, "transient"))
        }
        else { Ok(()) }
    }).unwrap();
    assert_eq!(attempts, 2);

    attempts = 0;
    let error = remove_archive_scratch(path, |_| {
        attempts += 1;
        Err(std::io::Error::new(std::io::ErrorKind::DirectoryNotEmpty, "persistent"))
    }).unwrap_err();
    assert_eq!(attempts, 3);
    assert_eq!(error.kind(), std::io::ErrorKind::DirectoryNotEmpty);

    attempts = 0;
    let error = remove_archive_scratch(path, |_| {
        attempts += 1;
        Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "unrelated"))
    }).unwrap_err();
    assert_eq!(attempts, 1);
    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn integrity_rejects_tampering_extra_files_and_traversal() {
    let scratch = Scratch::new();
    let shared = scratch.path("shared");
    export(&shared, &local(), false).unwrap();
    std::fs::write(shared.join("unapproved.txt"), "secret").unwrap();
    assert!(verify(&shared).is_err());
    std::fs::remove_file(shared.join("unapproved.txt")).unwrap();
    let old = std::fs::read(shared.join("inventory.json")).unwrap();
    std::fs::write(shared.join("inventory.json"), "{}").unwrap();
    assert!(verify(&shared).is_err());
    std::fs::write(shared.join("inventory.json"), old).unwrap();
    verify(&shared).unwrap();
    let mut manifest: Manifest = read_json(&shared.join("manifest.json")).unwrap();
    manifest.artifacts.insert("../secret.json".into(), "a".repeat(64));
    std::fs::write(shared.join("manifest.json"), json(&manifest).unwrap()).unwrap();
    assert!(verify(&shared).is_err());
}

#[test]
fn file_writes_never_overwrite_a_dossier_and_append_is_atomic() {
    let scratch = Scratch::new();
    let directory = scratch.path("private");
    write_local(&directory, &local()).unwrap();
    assert!(write_local(&directory, &local()).is_err());
    append_recording(&directory, recording()).unwrap();
    assert_eq!(read_local(&directory).unwrap().recordings.len(), 1);
    std::fs::write(directory.join("capture.lock"), "busy").unwrap();
    assert!(append_recording(&directory, recording()).is_err());
    assert_eq!(read_local(&directory).unwrap().recordings.len(), 1);
}

#[test]
fn recordings_cannot_target_keyboard_mouse_or_incomplete_vendor_metadata() {
    let mut dossier = local();
    assert!(capture_eligible(&dossier.interfaces[0]));
    dossier.interfaces[0].metadata.fields.push(field(7));
    assert!(!capture_eligible(&dossier.interfaces[0]));
    dossier.recordings.push(recording());
    assert!(inventory(&dossier).is_err());
    dossier.recordings.clear();
    dossier.interfaces[0].metadata.fields.clear();
    dossier.interfaces[0].usage_page = 1;
    dossier.interfaces[0].usage = 2;
    assert!(!capture_eligible(&dossier.interfaces[0]));
    dossier.interfaces[0].usage_page = 0xff00;
    dossier.interfaces[0].metadata.complete = false;
    assert!(!capture_eligible(&dossier.interfaces[0]));
}

#[test]
fn malformed_imports_cannot_claim_verification_or_smuggle_unknown_fields() {
    let mut dossier = local();
    dossier.interfaces[0].recognition.physical_output_tested = true;
    assert!(inventory(&dossier).is_err());
    dossier.interfaces[0].recognition.physical_output_tested = false;
    dossier.interfaces[0].recognition.declared_capabilities.push("PRIVATE_SECRET".into());
    assert!(inventory(&dossier).is_err());
    let mut value = serde_json::to_value(local()).unwrap();
    value["private_extra"] = serde_json::json!("secret");
    assert!(serde_json::from_value::<LocalDossier>(value).is_err());
    let mut r = recording();
    r.samples[1].elapsed_us = 99_000;
    assert!(analyze(&r).is_err());
    r.samples[1].elapsed_us = 1000;
    r.samples[1].bytes.clear();
    assert!(analyze(&r).is_err());
}

#[test]
fn variation_keeps_report_ids_and_lengths_separate_and_does_not_invent_mapping() {
    let mut r = recording();
    r.samples = vec![
        InputSample { elapsed_us: 0, bytes: vec![5, 0, 0] },
        InputSample { elapsed_us: 1, bytes: vec![6, 1, 0] },
        InputSample { elapsed_us: 2, bytes: vec![5, 1] },
        InputSample { elapsed_us: 3, bytes: vec![5, 1, 0] },
        InputSample { elapsed_us: 4, bytes: vec![5, 0, 0] },
    ];
    let analysis = analyze(&r).unwrap();
    assert_eq!(analysis.variations.len(), 1);
    let bit = &analysis.variations[0];
    assert_eq!((bit.report_id, bit.report_len, bit.byte, bit.bit, bit.rising, bit.falling), (5, 3, 1, 0, 1, 1));
}

#[test]
fn selected_export_keeps_auxiliary_collections_and_trials_but_excludes_other_units() {
    let mut dossier = local();
    let mut auxiliary = dossier.interfaces[0].clone();
    auxiliary.id = 2;
    let mut other_unit = auxiliary.clone();
    other_unit.id = 3;
    other_unit.device = 2;
    other_unit.metadata.group_key = Some("OTHER_PRIVATE_UNIT".into());
    dossier.interfaces.extend([auxiliary, other_unit]);
    dossier.recordings.push(recording());
    let mut other_trial = recording();
    other_trial.interface = 3;
    dossier.recordings.push(other_trial);
    let selected = select_device(&dossier, 1).unwrap();
    assert_eq!(selected.interfaces.iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(selected.recordings.len(), 1);
    assert_eq!(dossier.interfaces.len(), 3);
    assert!(select_device(&dossier, 0).is_err());
    assert!(select_device(&dossier, 99).is_err());
    let scratch = Scratch::new();
    let shared = scratch.path("selected");
    export(&shared, &selected, true).unwrap();
    verify(&shared).unwrap();
    let inv: Inventory = read_json(&shared.join("inventory.json")).unwrap();
    assert!(inv.interfaces.iter().all(|i| i.device == 1));
    assert_eq!(replay(&shared).unwrap().len(), 1);
}

#[test]
fn next_experiment_respects_capture_eligibility_and_repeated_trials_keep_unique_ids() {
    let mut dossier = local();
    dossier.interfaces[0].output_len = 0;
    let inv = inventory(&dossier).unwrap();
    assert_eq!(findings(&inv, &[])[0].next_experiment, NextExperiment::PassiveInputCapture);
    dossier.interfaces[0].metadata.fields.clear();
    let inv = inventory(&dossier).unwrap();
    assert!(!inv.interfaces[0].input_capture_eligible);
    assert_eq!(findings(&inv, &[])[0].next_experiment, NextExperiment::ReviewProtocolEvidence);
    dossier.interfaces[0].metadata.fields.push(field(0xff00));
    let inv = inventory(&dossier).unwrap();
    let analysis = analyze(&recording()).unwrap();
    let leads = findings(&inv, &[analysis.clone(), analysis]);
    let variation = leads.iter().find(|f| f.code == FindingCode::InputVariation).unwrap();
    assert_eq!(variation.interfaces, [1]);
}

#[test]
fn integrity_rejects_consistently_rehashed_but_contradictory_analysis() {
    let scratch = Scratch::new();
    let shared = scratch.path("shared");
    let mut dossier = local();
    dossier.recordings.push(recording());
    export(&shared, &dossier, true).unwrap();
    let path = shared.join("observations/analysis.json");
    let mut analyses: Vec<InputAnalysis> = read_json(&path).unwrap();
    analyses[0].variations[0].rising = 2;
    let bytes = json(&analyses).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let mut manifest: Manifest = read_json(&shared.join("manifest.json")).unwrap();
    manifest.artifacts.insert("observations/analysis.json".into(), hash(&bytes));
    std::fs::write(shared.join("manifest.json"), json(&manifest).unwrap()).unwrap();
    assert!(verify(&shared).unwrap_err().to_string().contains("recorded reports"));
    analyses[0].interface = 999;
    let bytes = json(&analyses).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    manifest.artifacts.insert("observations/analysis.json".into(), hash(&bytes));
    std::fs::write(shared.join("manifest.json"), json(&manifest).unwrap()).unwrap();
    assert!(verify(&shared).unwrap_err().to_string().contains("no interface"));
}

struct FakeBus;
impl Backend for FakeBus {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>> {
        Ok([("a", 0x1234), ("b", 0x1234), ("c", 0x1532)].into_iter().map(|(path, vid)| HidDeviceInfo {
            vid, pid: 0x9999, usage_page: 0xff00, usage: 2, feature_len: 91, input_len: 3,
            output_len: 0, product: "private".into(), path: DevicePath::from_str_for_tests(path),
        }).collect())
    }
    fn open_path(&self, _: &DevicePath) -> Result<Box<dyn Transport>> { panic!("collector attempted command I/O"); }
    fn open_reader(&self, _: &DevicePath) -> Result<Box<dyn InputReader>> { panic!("inventory attempted input capture"); }
    fn research_metadata(&self, path: &DevicePath) -> Result<Metadata> {
        let key = if path.as_os_str() == "c" { "second-unit" } else { "first-unit" };
        Ok(Metadata { source: DescriptorSource::WindowsHidParser, grouping: Grouping::WindowsContainer,
            group_key: Some(key.into()), fields: vec![field(0xff00)], complete: true, ..Metadata::unavailable() })
    }
}

#[test]
fn discovery_keeps_unknown_interfaces_and_groups_by_evidence_without_opening_io() {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = transport::install_backend(Arc::new(FakeBus));
    let dossier = collect(&Registry { devices: Vec::new() }).unwrap();
    assert_eq!(dossier.interfaces.len(), 3);
    assert_eq!(dossier.interfaces[0].device, dossier.interfaces[1].device);
    assert_ne!(dossier.interfaces[0].device, dossier.interfaces[2].device);
    assert!(!dossier.interfaces[0].recognition.registry_match);
    assert_eq!(dossier.interfaces[2].recognition.signature_dialects, ["razer"]);
    assert!(findings(&inventory(&dossier).unwrap(), &[]).iter().any(|f| f.code == FindingCode::UnknownProtocol));
}

#[test]
fn metadata_seam_respects_denied_hardware_policy() {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = transport::deny_hardware();
    assert!(transport::research_metadata(&DevicePath::from_str_for_tests("not-real")).is_err());
}

struct ReplacedBus(std::sync::atomic::AtomicBool);
impl Backend for ReplacedBus {
    fn enumerate(&self) -> Result<Vec<HidDeviceInfo>> { FakeBus.enumerate() }
    fn open_path(&self, _: &DevicePath) -> Result<Box<dyn Transport>> { panic!("unexpected command channel"); }
    fn open_reader(&self, _: &DevicePath) -> Result<Box<dyn InputReader>> { panic!("capture crossed a changed device association"); }
    fn research_metadata(&self, path: &DevicePath) -> Result<Metadata> {
        let mut metadata = FakeBus.research_metadata(path)?;
        if self.0.load(std::sync::atomic::Ordering::Relaxed) { metadata.group_key = Some("replacement-unit".into()); }
        Ok(metadata)
    }
}
#[test]
fn capture_refuses_a_reused_path_with_a_different_device_association() {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let bus = Arc::new(ReplacedBus(std::sync::atomic::AtomicBool::new(false)));
    let _guard = transport::install_backend(bus.clone());
    let dossier = collect(&Registry { devices: Vec::new() }).unwrap();
    bus.0.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(capture(&dossier, 1, 1).unwrap_err().to_string().contains("association changed"));
}

struct Flood;
impl InputReader for Flood {
    fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> { buffer[0] = 5; Ok(Some(1)) }
}
#[test]
fn passive_recording_is_bounded_by_report_count() {
    let r = record_from(&Flood, 1, 30, 3).unwrap();
    assert_eq!(r.samples.len(), MAX_REPORTS);
    assert_eq!(r.end, RecordingEnd::ReportLimit);
    assert!(record_from(&Flood, 1, 31, 3).is_err());
}

struct LargeFlood;
impl InputReader for LargeFlood {
    fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> { buffer.fill(0); Ok(Some(buffer.len())) }
}
#[test]
fn passive_recording_is_bounded_by_bytes_before_json_can_overflow_the_dossier() {
    let r = record_from(&LargeFlood, 1, 30, MAX_REPORT_BYTES).unwrap();
    assert_eq!(r.end, RecordingEnd::ByteLimit);
    assert_eq!(r.samples.len(), MAX_CAPTURE_BYTES / MAX_REPORT_BYTES);
    assert!(json(&r).unwrap().len() as u64 <= MAX_FILE);
}

struct Failing(Mutex<u8>);
impl InputReader for Failing {
    fn read(&self, buffer: &mut [u8]) -> Result<Option<usize>> {
        let mut n = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        *n += 1;
        if *n == 1 { buffer[0] = 5; Ok(Some(1)) } else { anyhow::bail!("private path removed") }
    }
}
#[test]
fn read_failure_retains_observed_prefix_without_exporting_error_paths() {
    let r = record_from(&Failing(Mutex::new(0)), 1, 1, 3).unwrap();
    assert_eq!(r.end, RecordingEnd::ReadFailed);
    assert_eq!(r.samples.len(), 1);
    assert!(!serde_json::to_string(&r).unwrap().contains("private path"));
}

#[test]
fn fixture_identity_includes_vendor_revision_and_shape() {
    let mut a = local();
    let first = inventory(&a).unwrap().interfaces[0].descriptor_fingerprint.clone();
    a.interfaces[0].metadata.fields[0].report_id = 6;
    let second = inventory(&a).unwrap().interfaces[0].descriptor_fingerprint.clone();
    assert_ne!(first, second);
    let scratch = Scratch::new();
    let path = scratch.path("shared");
    let manifest = export(&path, &a, false).unwrap();
    assert!(manifest.artifacts.keys().any(|n| n.starts_with("descriptors/1234-9876-0100-")));
}

#[test]
fn raw_linux_descriptors_are_only_shared_by_separate_opt_in() {
    let mut dossier = local();
    dossier.interfaces[0].metadata.source = DescriptorSource::LinuxSysfs;
    dossier.interfaces[0].metadata.complete = false;
    // A vendor Application collection with one eight-bit input field.
    dossier.interfaces[0].metadata.raw_descriptor_hex = Some("0600ff0902a101750895018102c0".into());
    let scratch = Scratch::new();
    let default = scratch.path("default");
    assert!(!export(&default, &dossier, false).unwrap().includes_raw_descriptors);
    let approved = scratch.path("approved");
    let manifest = export_with(&approved, &dossier, ExportOptions { include_descriptors: true, include_input: false }).unwrap();
    assert!(manifest.includes_raw_descriptors);
    verify(&approved).unwrap();
    let shapes = transport::research::descriptor_shapes(dossier.interfaces[0].metadata.raw_descriptor_hex.as_ref().unwrap()).unwrap();
    assert_eq!(shapes.len(), 1);
    assert_eq!((shapes[0].usage_page, shapes[0].usage, shapes[0].input_len), (0xff00, 2, 2));
}

#[test]
fn descriptor_inventory_survives_missing_registry_without_false_binding_findings() {
    let _serial = transport::mock::test_lock().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _guard = transport::install_backend(Arc::new(FakeBus));
    let inv = inventory(&collect_with_registry(None).unwrap()).unwrap();
    assert_eq!(inv.interfaces.len(), 3);
    assert!(!inv.registry_available);
    assert!(!findings(&inv, &[]).iter().any(|f| f.code == FindingCode::UnboundVendorInterface));
}

#[test]
fn zip_attachment_contains_only_verified_artifacts_and_refuses_traversal() {
    let scratch = Scratch::new();
    let shared = scratch.path("shared");
    let archive = scratch.path("attachment.zip");
    export(&shared, &local(), false).unwrap();
    pack(&shared, &archive).unwrap();
    assert!(!verify(&archive).unwrap().includes_raw_input);
    assert!(pack(&shared, &archive).is_err());
    let bad = scratch.path("bad.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&bad).unwrap());
    zip.start_file("../escaped.txt", zip::write::SimpleFileOptions::default()).unwrap();
    use std::io::Write as _;
    zip.write_all(b"not an artifact").unwrap();
    zip.finish().unwrap();
    assert!(verify(&bad).is_err());
    assert!(!scratch.path("escaped.txt").exists());
    assert!(!allowed_artifact("descriptors/COM1.json"));
    assert!(!allowed_artifact("descriptors/NUL.json"));
    assert!(!allowed_artifact("descriptors/subdir/1234-5678-0100-0123456789abcdef.json"));
}
