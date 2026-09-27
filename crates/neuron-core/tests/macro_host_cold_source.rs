// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The macro editor's unsaved-source path must retain its first action while its authority lane
//! starts. This has its own test process so no earlier test can accidentally prewarm that lane.

use neuron::macros::{macro_host, Context};
use std::time::{Duration, Instant};

#[test]
fn first_source_fire_survives_a_cold_sidecar() {
    let tmp = std::env::temp_dir().join(format!(
        "neuron_macro_host_cold_source_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping Macro Host cold-source e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false);

    let ctx = Context::synthetic(Some("first-source.exe".into()), None, None, None, None);
    let status = host.fire_source_mock(
        "e2e_cold_source",
        "# neuron: raw\ndef macro(ctx):\n    print('cold-source:' + ctx.app)\n",
        &ctx,
    );
    assert!(status.contains("queued"), "cold source fire was not retained: {status}");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut log = Vec::new();
    while Instant::now() < deadline {
        log.extend(host.drain_log());
        if log.iter().any(|line| line.contains("cold-source:first-source.exe")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    log.extend(host.drain_log());
    assert_eq!(
        log.iter().filter(|line| line.contains("cold-source:first-source.exe")).count(),
        1,
        "the first cold source fire must execute exactly once: {log:?}"
    );

    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
