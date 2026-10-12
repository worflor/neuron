// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo Research Components Exception 1.0.
// See ../../../LICENSE.md.

use anyhow::{ensure, Result};
use clap::Subcommand;
use neuron::compat;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum CompatCmd {
    /// Collect every enumerable HID interface and OS descriptor; no protocol probes or adoption
    Collect {
        /// new local-only directory; contains paths and device strings, do not upload it
        #[arg(long)]
        out: PathBuf,
    },
    /// Inspect sanitized inventory and native findings from a local dossier, offline
    Inspect {
        dossier: PathBuf,
        /// limit inventory and findings to this dossier-scoped device group
        #[arg(long)]
        device: Option<u32>,
        /// show local product names for target selection; never include them in an attachment
        #[arg(long)]
        local_names: bool,
        /// include the full numeric descriptor for just this interface; default output is compact
        #[arg(long)]
        interface: Option<u32>,
    },
    /// Record a bounded, selected vendor-input experiment; never dispatches or writes hardware
    Capture {
        dossier: PathBuf,
        #[arg(long)]
        interface: u32,
        #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(1..=30))]
        seconds: u64,
        /// required consent to record potentially private bytes from this selected interface
        #[arg(long)]
        consent_input: bool,
    },
    /// Build a sanitized shareable bundle and focused issue drafts; never uploads or files issues
    Export {
        dossier: PathBuf,
        /// share only this dossier-scoped device group, keeping its related collections and trials
        #[arg(long)]
        device: Option<u32>,
        #[arg(long)]
        out: PathBuf,
        /// include raw vendor input only after separately reviewing and approving it for sharing
        #[arg(long)]
        include_input: bool,
        /// include reviewed raw descriptors; unknown descriptor payloads can contain private data
        #[arg(long)]
        include_descriptors: bool,
    },
    /// Verify hashes, schemas and artifact membership in a shareable bundle, offline
    Verify { bundle: PathBuf },
    /// Package a verified shareable directory as one ZIP attachment; never includes the local dossier
    Pack {
        bundle: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Replay exported input through the neutral bit-variation analyzer, without hardware
    Replay { bundle: PathBuf },
}

pub fn run(action: CompatCmd) -> Result<()> {
    match action {
        CompatCmd::Collect { out } => {
            let registry = neuron::registry::Registry::load().ok();
            let dossier = compat::collect_with_registry(registry.as_ref())?;
            compat::write_local(&out, &dossier)?;
            let inventory = compat::inventory(&dossier)?;
            crate::out::done(serde_json::json!({ "dossier": out, "schema": compat::SCHEMA, "interface_count": inventory.interfaces.len(), "registry_available": inventory.registry_available }),
                format!("Collected {} interfaces. Local-only dossier: {}", inventory.interfaces.len(), out.display()));
        }
        CompatCmd::Inspect { dossier, device, local_names, interface } => {
            let collected = compat::read_local(&dossier)?;
            let local = device.map(|id| compat::select_device(&collected, id)).transpose()?.unwrap_or(collected);
            let inventory = compat::inventory(&local)?;
            let analyses = local.recordings.iter().map(compat::analyze).collect::<Result<Vec<_>>>()?;
            let findings = compat::findings(&inventory, &analyses);
            let detail = interface.map(|id| inventory.interfaces.iter().find(|i| i.id == id)
                .ok_or_else(|| anyhow::anyhow!("interface ID not found"))).transpose()?;
            let summaries = inventory.interfaces.iter().map(|i| {
                let mut summary = serde_json::json!({
                "id": i.id, "device": i.device, "vid": format!("{:04x}", i.vid), "pid": format!("{:04x}", i.pid),
                "usage_page": i.usage_page, "usage": i.usage, "registry_match": i.recognition.registry_match,
                "descriptor_source": i.descriptor_source, "descriptor_complete": i.descriptor_complete,
                "input_capture_eligible": i.input_capture_eligible
                });
                if local_names {
                    summary["local_product"] = serde_json::json!(local.interfaces.iter().find(|l| l.id == i.id).map(|l| &l.product));
                }
                summary
            }).collect::<Vec<_>>();
            crate::out::emit(&serde_json::json!({ "neuron_version": inventory.neuron_version, "platform": inventory.platform,
                "environment": inventory.environment, "registry_available": inventory.registry_available,
                "contains_local_names": local_names,
                "interfaces": summaries, "detail": detail, "findings": findings, "input_analysis": analyses }), || {
                println!("{}", compat::summary(&inventory, &findings));
                if local_names {
                    println!("Local-only product names (not attachment content):");
                    for i in &local.interfaces { println!("Device {} / interface {}: {:?}", i.device, i.id, i.product); }
                }
            })?;
        }
        CompatCmd::Capture { dossier, interface, seconds, consent_input } => {
            ensure!(consent_input, "capture requires explicit consent; pass --consent-input after the user agrees");
            let local = compat::read_local(&dossier)?;
            crate::out::note(format!("Recording interface {interface} for {seconds}s. Press/release only the control under investigation three times; avoid unrelated activity."));
            let recording = compat::capture(&local, interface, seconds)?;
            let analysis = compat::analyze(&recording)?;
            compat::append_recording(&dossier, recording)?;
            crate::out::done(serde_json::to_value(&analysis)?, format!("Recorded {} reports; {:?}. Bytes remain local until separately approved for export.", analysis.sample_count, analysis.end));
        }
        CompatCmd::Export { dossier, device, out, include_input, include_descriptors } => {
            let collected = compat::read_local(&dossier)?;
            let local = device.map(|id| compat::select_device(&collected, id)).transpose()?.unwrap_or(collected);
            let manifest = compat::export_with(&out, &local, compat::ExportOptions { include_input, include_descriptors })?;
            compat::verify(&out)?;
            crate::out::done(serde_json::json!({ "bundle": out, "manifest": manifest }), format!("Verified shareable bundle: {}. Nothing was uploaded.", out.display()));
        }
        CompatCmd::Verify { bundle } => {
            let manifest = compat::verify(&bundle)?;
            crate::out::done(serde_json::to_value(&manifest)?, "Bundle integrity passed; this does not attest hardware behaviour.");
        }
        CompatCmd::Pack { bundle, out } => {
            let manifest = compat::pack(&bundle, &out)?;
            compat::verify(&out)?;
            crate::out::done(serde_json::json!({ "archive": out, "manifest": manifest }),
                format!("Verified ZIP attachment: {}. Nothing was uploaded.", out.display()));
        }
        CompatCmd::Replay { bundle } => {
            let analyses = compat::replay(&bundle)?;
            crate::out::emit(&analyses, || {
                for a in &analyses { println!("Interface {}: {} reports, {} changing bits.", a.interface, a.sample_count, a.variations.len()); }
            })?;
        }
    }
    Ok(())
}
