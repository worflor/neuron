// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The resident-citizenship BUDGET LANE — opt-in (it launches the real binary and needs
//! ~90 s of wall clock), run with:
//!
//! ```text
//! NEURON_BUDGET_EXE=target\release\neuron-app.exe cargo test -p neuron-testkit --test budget_lane -- --ignored --nocapture
//! ```
//!
//! IMPORTANT: stop any resident neuron instance first (the wire lock serializes them, but
//! two instances double the measured footprint and the census blames the harness copy).
//!
//! Phases: boot settle → tray idle (the phase users live in 99% of the time) → a second
//! idle window (a cheap two-point leak slope). Observations are printed as JSON so a CI
//! archive can diff runs over time; only the ceilings assert.

#![cfg(windows)]

use std::path::PathBuf;
use std::time::Duration;

use neuron_testkit::budget::{Budgets, Resident};

#[test]
#[ignore = "launches the real binary; opt in with NEURON_BUDGET_EXE"]
fn resident_footprint_stays_within_budget() {
    let Some(exe) = std::env::var_os("NEURON_BUDGET_EXE") else {
        panic!("set NEURON_BUDGET_EXE to the built neuron-app.exe");
    };
    let exe = PathBuf::from(exe);
    assert!(exe.exists(), "{} does not exist — build release first", exe.display());

    let mut app = Resident::launch(&exe, &["--tray"]).expect("launch in job");
    // boot settle: registry load, device scan, host servers, sidecar warm.
    std::thread::sleep(Duration::from_secs(8));

    let budgets = Budgets::default();
    let idle1 = app.sample("tray-idle-1", Duration::from_secs(30)).expect("sample 1");
    let idle2 = app.sample("tray-idle-2", Duration::from_secs(30)).expect("sample 2");

    for s in [&idle1, &idle2] {
        println!("{}", serde_json::to_string_pretty(s).unwrap());
        app.assert_within(s, &budgets).unwrap();
    }

    // Two-point leak slope: a steady resident may jitter, but a second idle window that
    // grew by >10% in handles or >32 MB private in 30 s is a leak, not jitter.
    assert!(
        idle2.handles as f64 <= idle1.handles as f64 * 1.10 + 16.0,
        "handle growth across idle windows: {} -> {}",
        idle1.handles,
        idle2.handles
    );
    assert!(
        idle2.private_bytes <= idle1.private_bytes + 32 * 1024 * 1024,
        "private-commit growth across idle windows: {} -> {} bytes",
        idle1.private_bytes,
        idle2.private_bytes
    );
    // Resident drops here -> TerminateJobObject reaps the app and the sidecar, no orphans.
}
