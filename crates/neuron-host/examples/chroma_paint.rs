//! The "game paints your hardware" pipeline — neuron as the Chroma server.
//!
//! neuron creates the named shared objects a native game opens, holds the arbitration
//! mask so the game paints in full colour, decodes the per-key frames it writes, and
//! mirrors them onto the hardware like any other effect. Nothing enters the game process
//! (anti-cheat-safe). If a vendor server is already serving, neuron reads alongside it.
//!
//! Flow: game → SHM device buffer → `ChromaShmLayer` (decode) → arbiter → paced HID
//! writer → your keyboard/mouse.
//!
//!     cargo run -p neuron-host --features bridge --example chroma_paint
//!
//! Needs elevation to create the `Global\` objects (else it stands down).

#[cfg(all(windows, feature = "bridge"))]
fn main() -> anyhow::Result<()> {
    use neuron_host::adapters::chroma_shm::server::{ChromaShmLayer, CreateError, ShmServer};
    use neuron_host::paint::PaintPolicy;
    use neuron_host::api::{HostApi, LeaseSpec, SurfaceKind};
    use neuron_host::arbiter::{band, Content, Rgb};
    use neuron_host::bridge;
    use neuron_host::shell::Host;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // 1. Be the whole thing: create the objects AND wear the arbitration mask so a game
    //    paints colour against neuron with no vendor server running. If a server already
    //    owns the objects, fall back to read-alongside (open + mirror).
    let server = match ShmServer::create() {
        Ok(s) => {
            println!("neuron IS the Chroma server (objects + mask) — no vendor server needed.");
            Arc::new(s)
        }
        Err(CreateError::AlreadyServing) => match ShmServer::open() {
            Ok(s) => {
                println!("a server is up — attaching read-alongside (mirroring its buffers).");
                Arc::new(s)
            }
            Err(e) => {
                eprintln!("chroma attach: {e}");
                return Ok(());
            }
        },
        Err(CreateError::Io(e)) => {
            eprintln!("chroma create: {e}");
            eprintln!("(Global\\ objects need elevation — run elevated)");
            return Ok(());
        }
    };

    // 2. Discover real hardware → surfaces with paced HID writers.
    let reg = neuron::registry::Registry::load()?;
    let host = Host::spawn();
    let bridged = bridge::attach(&reg, &host.handle(), 30);
    if bridged.surfaces.is_empty() {
        eprintln!("no bridgeable devices found — nothing to paint.");
        return Ok(());
    }
    let mut h = host.handle();
    let policy = PaintPolicy::new();

    // A calm base layer so unclaimed LEDs aren't black before a game connects.
    let base = h.next_source();
    for s in &bridged.surfaces {
        let _ = h.claim(&s.key, base, band::BASE, LeaseSpec::Pinned, Content::Fill(Rgb(0, 40, 30)), Instant::now());
    }

    // 3. Claim a live Chroma layer per device at the session band. `render` pulls
    //    the game's current frame each tick; the writer paints it to the HID.
    for s in &bridged.surfaces {
        let device_type = match s.kind {
            SurfaceKind::Keyboard => 0x01,
            SurfaceKind::Mouse => 0x02,
            SurfaceKind::Headset => 0x04,
            SurfaceKind::Mousepad => 0x08,
            SurfaceKind::Keypad => 0x10,
            SurfaceKind::Generic => 0x80,
        };
        // Start faded out so the game crossfades IN over the base when it first connects.
        let layer = ChromaShmLayer::new(
            Arc::clone(&server),
            s.key.clone(),
            device_type,
            s.leds,
            Arc::clone(&policy),
            0.0,
        );
        let src = h.next_source();
        let _ = h.claim(
            &s.key,
            src,
            band::SESSION,
            LeaseSpec::Pinned,
            Content::Live(Box::new(layer)),
            Instant::now(),
        );
        println!("  painting {} ({} LEDs) ← Chroma device 0x{device_type:02x}", s.name, s.leds);
    }

    println!("\nLive. Launch a Chroma game — it now paints your hardware through neuron.");
    println!("Below: game events INFERRED from the decoded light alone (no game API):");
    println!("  ⏱ measured cooldowns, ▲ charging fills, ◈ alert pulses, plus a device summary.");
    println!("Ctrl-C to stop.\n");

    use neuron_host::adapters::chroma_analyze::{ChromaAnalyzer, LightEvent};
    use std::collections::HashMap;

    let device_name = |dt: u8| match dt {
        0x01 => "keyboard",
        0x02 => "mouse",
        0x04 => "headset",
        0x08 => "mousepad",
        0x10 => "keypad",
        _ => "device",
    };
    // 6x22 keyboard grid → a rough label so events point at a place, not just an index.
    let cols = 22usize;
    let key_at = move |led: usize| format!("key r{} c{}", led / cols, led % cols);

    let kbd_leds = bridged
        .surfaces
        .iter()
        .find(|s| matches!(s.kind, SurfaceKind::Keyboard))
        .map(|s| s.leds)
        .unwrap_or(132);
    let mut analyzer = ChromaAnalyzer::new(kbd_leds);

    let mut last_ts: HashMap<u8, u32> = HashMap::new();
    let mut last_summary = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(30)); // ~33 Hz analysis

        // Feed the analyzer the newest decoded keyboard frame + its own timestamp.
        let kbd_ts = server
            .device_activity()
            .into_iter()
            .find(|a| a.device_type == 0x01)
            .map(|a| a.timestamp_ms);
        let kbd_frame = server
            .read_device_frames_decoded()
            .into_iter()
            .find(|(dt, _)| *dt == 0x01);
        if let (Some(ts), Some((_, units))) = (kbd_ts, kbd_frame) {
            // skip the leading pad unit, same as the painter, so led i == physical key i.
            let frame: Vec<(u8, u8, u8)> = units.iter().skip(1).map(|u| u.rgb()).collect();
            for ev in analyzer.ingest(ts, &frame) {
                match ev {
                    LightEvent::Cooldown { led, duration_ms, .. } => println!(
                        "⏱  {} cooldown = {:.1}s  (measured from light)",
                        key_at(led),
                        duration_ms as f32 / 1000.0
                    ),
                    LightEvent::Ramp { led, eta_ms, .. } => println!(
                        "▲  {} charging → full in ~{:.1}s",
                        key_at(led),
                        eta_ms as f32 / 1000.0
                    ),
                    LightEvent::Pulse { led, hz, .. } => {
                        println!("◈  {} pulsing @ {hz:.1} Hz (alert / ready?)", key_at(led))
                    }
                    _ => {} // Onset/Offset are the raw substrate — too chatty to print
                }
            }
        }

        // Periodic device summary: what each device class is painting + live/idle.
        if last_summary.elapsed() >= Duration::from_secs(5) {
            last_summary = Instant::now();
            let acts = server.device_activity();
            if !acts.is_empty() {
                let app = server
                    .registered_apps()
                    .into_iter()
                    .next()
                    .map(|a| a.name)
                    .unwrap_or_else(|| "(unregistered)".to_string());
                let mut line = format!("· {app}  ");
                for a in &acts {
                    let live = last_ts.get(&a.device_type).is_none_or(|&p| a.timestamp_ms != p);
                    last_ts.insert(a.device_type, a.timestamp_ms);
                    line += &format!(
                        "{}={}{}  ",
                        device_name(a.device_type),
                        a.effect(),
                        if live { "•" } else { "×idle" }
                    );
                }
                println!("{line}");
            }
        }
    }
}

#[cfg(not(all(windows, feature = "bridge")))]
fn main() {
    eprintln!("build with: cargo run -p neuron-host --features bridge --example chroma_paint (Windows only)");
}
