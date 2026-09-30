// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! On-device button remap probe (02/0C set, 02/8C get) on the Naga's volatile profile 0.
//!   remap_probe read            dump the thumb grid bindings (0x40..0x4B)
//!   remap_probe set             bind thumb button 0x4B to 'g' (usage 0x0A), print the read-back
//!   remap_probe restore <hex..> write back the 10 original arg bytes printed by `set`
use neuron::{device::Device, registry::Registry, transport};

fn open(reg: &Registry) -> anyhow::Result<Device> {
    let mut infos = transport::enumerate()?;
    reg.prefer_live_links(&mut infos);
    for i in &infos {
        if let Some(def) = reg.find_for_pipe(i) {
            if def.supports(neuron::registry::Capability::SetScrollStage) {
                return Device::open_path(def.clone(), i.pid, &i.path);
            }
        }
    }
    anyhow::bail!("no Naga")
}

fn get(d: &Device, button: u8) -> anyhow::Result<[u8; 10]> {
    let a = d.exec_dynamic(0x02, 0x8C, 0x0A, &[0x00, button, 0x00])?;
    let mut out = [0u8; 10];
    out.copy_from_slice(&a[..10]);
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let reg = Registry::load()?;
    let d = open(&reg)?;
    println!("pid {:04x}", d.pid);
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("read") | None => {
            for b in 0x40..=0x4Bu8 {
                println!("  {b:#04x}: {:02x?}", get(&d, b)?);
            }
        }
        Some("set") => {
            let orig = get(&d, 0x4B)?;
            println!("original 0x4b: {}", orig.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
            let r = d.exec_dynamic(0x02, 0x0C, 0x0A, &[0x00, 0x4B, 0x00, 0x02, 0x02, 0x00, 0x0A, 0x00, 0x00, 0x00]);
            println!("set -> {:?}", r.map(|a| a[..10].to_vec()));
            println!("read-back 0x4b: {:02x?}", get(&d, 0x4B)?);
        }
        Some("restore") => {
            let bytes: Vec<u8> = args[1..].iter().map(|h| u8::from_str_radix(h, 16)).collect::<Result<_, _>>()?;
            anyhow::ensure!(bytes.len() == 10 && bytes[1] == 0x4B, "need the 10 original bytes of 0x4b");
            let r = d.exec_dynamic(0x02, 0x0C, 0x0A, &bytes);
            println!("restore -> {:?}", r.map(|a| a[..10].to_vec()));
            println!("read-back 0x4b: {:02x?}", get(&d, 0x4B)?);
        }
        Some(other) => anyhow::bail!("unknown verb {other}"),
    }
    Ok(())
}
