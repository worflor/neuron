// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Stand up the neuron Chroma SHM server and print any lighting a game paints.
//!
//! This is the anti-cheat-safe path: neuron creates the named shared objects a game
//! opens (at their exact sizes), then *reads* the device buffers the game writes.
//! Nothing enters the game process.
//!
//!     cargo run -p neuron-host --features bridge --example chroma_serve
//!
//! Requires elevation (the `Global\` namespace needs SeCreateGlobalPrivilege). Stands
//! down if another Chroma server is already serving.

#[cfg(all(windows, feature = "bridge"))]
fn main() {
    use neuron_host::adapters::chroma_shm::server::ShmServer;
    use std::{thread, time::Duration};

    let srv = match ShmServer::create() {
        Ok(s) => {
            println!("neuron Chroma server up — 22 objects created at exact sizes.");
            println!("Launch a Chroma game. Watching 120s...\n");
            s
        }
        Err(e) => {
            eprintln!("create failed: {e}");
            eprintln!("(need elevation; stands down if another Chroma server is serving)");
            return;
        }
    };

    let mut last = String::new();
    for t in 0..240 {
        let apps: Vec<String> = srv.registered_apps().into_iter().map(|a| a.name).collect();
        let frames = srv.read_device_frames();
        let session = srv.latest_session();

        let mut line = String::new();
        if !apps.is_empty() {
            line += &format!("apps={apps:?} ");
        }
        if let Some(s) = session {
            line += &format!("session(pid={} access={}) ", s.session_id, s.active_count);
        }
        for (dt, leds) in &srv.frames() {
            let class = match dt {
                0x01 => "kbd", 0x02 => "mouse", 0x04 => "headset",
                0x08 => "mousepad", 0x10 => "keypad", 0x80 => "chromalink", _ => "dev",
            };
            let (r, g, b) = leds.first().copied().unwrap_or((0, 0, 0));
            line += &format!("[{class}: {} LEDs rgb({r},{g},{b})] ", leds.len());
        }
        let _ = &frames;
        if !line.is_empty() && line != last {
            println!("t={:>3}s  {line}", t / 2);
            last = line;
        }
        thread::sleep(Duration::from_millis(500));
    }
    println!("\ndone.");
}

#[cfg(not(all(windows, feature = "bridge")))]
fn main() {
    eprintln!("build with: cargo run -p neuron-host --features bridge --example chroma_serve (Windows only)");
}
