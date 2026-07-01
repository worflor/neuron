//! Run the protocol host against REAL hardware.
//!
//! Discovers registry-known devices, declares them as kernel surfaces, starts
//! one paced writer per device (the only writer that touches its HID handle),
//! and serves the OpenRGB SDK protocol on the well-known port — so a genuine
//! OpenRGB client (openrgb-python, Home Assistant) can drive the boards
//! through the arbiter, with the lease/ownership guarantees active.
//!
//!     cargo run -p neuron-host --features bridge --example host_serve
//!
//! A bind failure on 6742 means another host instance already owns this
//! machine — the single-instance signal (become its client instead).

use neuron_host::bridge;
use neuron_host::net::{OrgbServer, OPENRGB_ADDR};
use neuron_host::shell::Host;

fn main() -> anyhow::Result<()> {
    let reg = neuron::registry::Registry::load()?;
    let host = Host::spawn();

    let bridged = bridge::attach(&reg, &host.handle(), 30);
    if bridged.surfaces.is_empty() {
        eprintln!("no bridgeable devices found (registry knows none of the connected hardware)");
    }
    for s in &bridged.surfaces {
        println!("surface: {} — {} LEDs [{}]", s.name, s.leds, s.key);
    }

    let server = OrgbServer::bind(OPENRGB_ADDR, host.handle()).map_err(|e| {
        anyhow::anyhow!(
            "bind {OPENRGB_ADDR}: {e} — another neuron host may already own this machine; \
             connect to it as a client instead"
        )
    })?;
    println!("OpenRGB SDK server listening on {} — press Enter to exit", server.addr());

    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    Ok(())
}
