// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() {
    use neuron_host::adapters::chroma_shm::server::{CreateError, SectionSeed};
    use std::time::Duration;

    loop {
        match SectionSeed::create() {
            Ok(seed) => {
                // The named sections must outlive startup and remain available when the tray
                // restarts. No polling or user-supplied commands run after this point.
                let _seed = seed;
                loop { std::thread::park(); }
            }
            Err(CreateError::AlreadyServing) => std::thread::sleep(Duration::from_secs(5)),
            Err(CreateError::Io(_)) => {
                std::process::exit(1);
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {}
