// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Live test of the USBPcap-captured scroll-stage command (class 0x15/0x00 [store, stage]).
//! Writes stage 1 then stage 2 (persist, matching what Synapse sent) — you should FEEL the wheel
//! change between the two enabled stages. Run: cargo run -p neuron --example hw_verify
use neuron::{capability::Store, device::Device, registry::Registry, transport, writes};

fn open_naga(reg: &Registry) -> anyhow::Result<Device> {
    for i in &transport::enumerate()? {
        if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
            if def.matches_control(i)
                && def.supports(neuron::registry::Capability::SetScrollStage)
            {
                return Device::open(def.clone(), i.pid);
            }
        }
    }
    anyhow::bail!("no awake Naga found")
}

fn main() -> anyhow::Result<()> {
    let reg = Registry::load()?;
    let d = open_naga(&reg)?;
    println!("scroll-stage write test (0x15/0x00, the bytes Synapse sent):");
    for stage in [1u8, 2u8] {
        match writes::set_scroll_stage(&d, stage, Store::Persist) {
            Ok(()) => println!("  stage {stage}: ACCEPTED by device"),
            Err(e) => println!("  stage {stage}: {e}"),
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }
    println!("(if the wheel feel changed between stage 1 and 2, tell me which felt like which)");
    Ok(())
}
