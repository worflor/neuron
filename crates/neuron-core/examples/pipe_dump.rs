// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Dump every enumerated Razer HID collection — the raw bus truth `synth`/`discover` filter
//! from. Diagnostic for emergent-discovery work: shows which pipes carry the razer_report
//! signature (91-byte feature report) and which are other HID (consumer controls, audio
//! knobs) that adoption must ignore. Read-only; no reports are sent.
//!
//! `cargo run -p neuron --example pipe_dump`

fn main() -> anyhow::Result<()> {
    let mut infos = neuron::transport::enumerate()?;
    infos.retain(|i| i.vid == neuron::synth::RAZER_VID);
    infos.sort_by_key(|i| (i.pid, i.usage_page, i.usage));
    if infos.is_empty() {
        println!("no Razer (vid 1532) HID collections enumerated.");
        return Ok(());
    }
    println!("{} Razer HID collections:", infos.len());
    for i in &infos {
        println!(
            "pid {:04x}  usage {:04x}/{:04x}  feature_len {:>3}  {}  \"{}\"  [{}]",
            i.pid,
            i.usage_page,
            i.usage,
            i.feature_len,
            if i.feature_len == neuron::synth::RAZER_FEATURE_LEN { "razer_report-shaped" } else { "other HID           " },
            i.product,
            i.instance(),
        );
    }
    Ok(())
}
