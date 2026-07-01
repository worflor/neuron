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

use std::time::{Duration, Instant};

use neuron_host::api::{HostApi, LeaseSpec};
use neuron_host::arbiter::{band, Content, Rgb};
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

    // A pinned base layer per surface: the demo's stand-in for the app's
    // configured lighting. Without it, nothing claims the LEDs and the writer
    // would honestly paint unclaimed-black — correct, but a rude first frame.
    // With it, the ownership story is visible on hardware: a client's paint
    // overrides this; the client's death/disconnect returns to it.
    let mut h = host.handle();
    let base_owner = h.next_source();
    const BASE: Rgb = Rgb(0, 90, 60); // calm static teal — proof, not garnish
    for s in &bridged.surfaces {
        let _ = h.claim(
            &s.key,
            base_owner,
            band::BASE,
            LeaseSpec::Pinned,
            Content::Fill(BASE),
            Instant::now(),
        );
        println!("surface: {} — {} LEDs [{}] — base = static teal", s.name, s.leds, s.key);
    }

    let server = OrgbServer::bind(OPENRGB_ADDR, host.handle()).map_err(|e| {
        anyhow::anyhow!(
            "bind {OPENRGB_ADDR}: {e} — another neuron host may already own this machine; \
             connect to it as a client instead"
        )
    })?;
    println!("OpenRGB SDK server listening on {} — press Enter to exit", server.addr());

    // Interactive: Enter exits. Non-interactive (spawned, stdin closed): park
    // until killed — read_line returning 0 bytes means EOF, not intent.
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    Ok(())
}
