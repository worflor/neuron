// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Read-only diagnostic: dump the class 0x15 scroll getters on BOTH varstore planes (volatile 0x00 /
//! persisted 0x01) plus device mode. `15/80` answers `[store, stage]`, the mirror of the
//! `15/00 [store, stage]` setter. Run: cargo run -p neuron --example scroll_planes
use neuron::{device::Device, registry::Registry, transport, writes};

fn open_scroll_device(reg: &Registry) -> anyhow::Result<Device> {
    for i in &transport::enumerate()? {
        if let Some(def) = reg.find_by_pid(i.vid, i.pid) {
            if def.matches_control(i) && def.supports(neuron::registry::Capability::SetScrollStage) {
                return Device::open(def.clone(), i.pid);
            }
        }
    }
    anyhow::bail!("no awake scroll-stage device found")
}

fn main() -> anyhow::Result<()> {
    let reg = Registry::load()?;
    let d = open_scroll_device(&reg)?;
    println!("pid {:04x}  device mode: {:?}", d.pid, writes::device_mode(&d));
    for id in [0x80u8, 0x81, 0x85, 0x86, 0x87] {
        for (plane, args) in [("no-arg   ", &[][..]), ("volatile ", &[0x00][..]), ("persisted", &[0x01][..])] {
            match d.exec_dynamic(0x15, id, 0x02, args) {
                Ok(a) => println!("15/{id:02x} {plane}: {:02x?}", &a[..8]),
                Err(e) => println!("15/{id:02x} {plane}: ERR {e}"),
            }
        }
    }
    // `--watch <secs>`: poll the volatile plane and print every change, to attribute a button press.
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--watch") {
        let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        let mut last = None;
        println!("watching volatile 15/80, 15/81, 15/87 + mode for {secs}s...");
        while std::time::Instant::now() < until {
            let read = |id| d.exec_dynamic(0x15, id, 0x02, &[0x00]).ok().map(|a| a[..6].to_vec());
            let now = (writes::device_mode(&d), read(0x80), read(0x81), read(0x87));
            if last.as_ref() != Some(&now) {
                println!("{:?}", now);
                last = Some(now);
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Ok(())
}
