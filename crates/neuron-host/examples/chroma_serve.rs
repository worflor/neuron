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
    use std::fmt::Write as _;
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
        let session = srv.latest_session();

        let mut line = String::new();
        if !apps.is_empty() {
            let _ = write!(line, "apps={apps:?} ");
        }
        if let Some(s) = session {
            let _ = write!(line, "session(pid={} access={}) ", s.session_id, s.active_count);
        }
        for f in &srv.frames() {
            let (r, g, b) = f.cells.first().copied().unwrap_or((0, 0, 0));
            let _ = write!(line, "[{}: {} {} cells rgb({r},{g},{b})] ", f.class.name(), f.effect.name(), f.len());
        }
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
