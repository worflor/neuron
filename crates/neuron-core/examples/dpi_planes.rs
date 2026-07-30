// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only diagnostic: dump BOTH varstore planes (volatile 0x00 / persisted 0x01) of the
//! DPI getters — active DPI (0x04/0x85), active stage list (0x04/0x86), full slot table
//! (0x04/0x83) — plus device mode. Run: cargo run -p neuron --example dpi_planes
use neuron::{device::Device, registry::Registry, transport, writes};

fn open_naga(reg: &Registry) -> anyhow::Result<Device> {
    for i in &transport::enumerate()? {
        if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
            if def.matches_control(i) && def.supports(neuron::registry::Capability::SetDpiStages) {
                return Device::open(def.clone(), i.pid);
            }
        }
    }
    anyhow::bail!("no awake Naga found")
}

fn dump(d: &Device, name: &str, id: u8, size: u8) {
    for (plane, vs) in [("volatile ", 0x00u8), ("persisted", 0x01)] {
        match d.exec_dynamic(0x04, id, size, &[vs]) {
            Ok(a) => {
                let stages = writes::decode_dpi_stages(&a);
                if stages.is_empty() && size == 0x07 {
                    let x = ((a[1] as u16) << 8) | a[2] as u16;
                    let y = ((a[3] as u16) << 8) | a[4] as u16;
                    println!("{name} {plane}: {x}x{y}");
                } else {
                    println!(
                        "{name} {plane}: {:?} active {:?}",
                        stages,
                        writes::decode_dpi_active(&a)
                    );
                }
            }
            Err(e) => println!("{name} {plane}: ERR {e}"),
        }
    }
}

fn main() -> anyhow::Result<()> {
    let reg = Registry::load()?;
    let d = open_naga(&reg)?;
    println!("device mode: {:?}", writes::device_mode(&d));
    dump(&d, "dpi        (04/85)", 0x85, 0x07);
    dump(&d, "stages-act (04/86)", 0x86, 0x26);
    dump(&d, "slot-table (04/83)", 0x83, 0x26);
    Ok(())
}
