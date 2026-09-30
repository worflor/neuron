// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only: dump raw bytes for ledger conflicts on the Naga — battery (07/80), charging (07/84,
//! which byte carries the flag), the on-device button map (02/8C) for a few thumb keys, and the
//! onboard profile slot (05/84). Run: cargo run -p neuron --example naga_reads
use neuron::{device::Device, registry::Registry, transport};

fn main() -> anyhow::Result<()> {
    let reg = Registry::load()?;
    let mut dev = None;
    for i in &transport::enumerate()? {
        if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
            if def.matches_control(i) && def.supports(neuron::registry::Capability::SetScrollStage) {
                dev = Some(Device::open(def.clone(), i.pid)?);
                break;
            }
        }
    }
    let d = dev.ok_or_else(|| anyhow::anyhow!("no device"))?;
    let show = |label: &str, class: u8, id: u8, size: u8, args: &[u8]| match d.exec_dynamic(class, id, size, args) {
        Ok(a) => println!("{label:<28} {class:02x}/{id:02x} {args:02x?} -> {:02x?}", &a[..12]),
        Err(e) => println!("{label:<28} {class:02x}/{id:02x} {args:02x?} -> ERR {e}"),
    };
    show("battery", 0x07, 0x80, 0x02, &[]);
    show("charging", 0x07, 0x84, 0x02, &[]);
    show("profile slot", 0x05, 0x84, 0x01, &[]);
    for btn in [0x40u8, 0x41, 0x4B] {
        show(&format!("button map p0 {btn:#04x}"), 0x02, 0x8C, 0x0A, &[0x00, btn, 0x00]);
        show(&format!("button map p1 {btn:#04x}"), 0x02, 0x8C, 0x0A, &[0x01, btn, 0x00]);
    }
    Ok(())
}
