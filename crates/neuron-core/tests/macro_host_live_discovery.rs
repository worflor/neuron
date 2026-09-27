// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A macro copied onto disk after its authority lane is warm must be discovered, registered, and
//! fired by the trigger that found it. The input path may queue that work but may not drop it.

use neuron::macros::{macro_host, Context};
use std::time::{Duration, Instant};

#[test]
fn first_fire_discovers_a_macro_added_after_startup() {
    let tmp = std::env::temp_dir().join(format!(
        "neuron_macro_live_discovery_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let scripts = tmp.join("macros").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    std::fs::write(
        scripts.join("seed.py"),
        "# neuron: raw\ndef macro(ctx):\n    return 'seed'\n",
    ).unwrap();

    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);
    let host = macro_host();
    if !host.available() {
        eprintln!("skipping live-discovery Macro Host e2e: runtime unavailable");
        std::env::set_current_dir(previous).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false);
    host.ensure_warm().expect("seed should warm the RAW lane");
    host.drain_log();

    std::fs::write(
        scripts.join("late_drop.py"),
        "# neuron: raw\ndef macro(ctx):\n    print('late-drop:' + ctx.app)\n",
    ).unwrap();
    let ctx = Context::synthetic(Some("discovery.exe".into()), None, None, None, None);
    let status = host.fire_mock("late_drop", &ctx);
    assert!(status.contains("queued"), "first fire was not retained: {status}");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut log = Vec::new();
    while Instant::now() < deadline {
        log.extend(host.drain_log());
        if log.iter().any(|line| line.contains("late-drop:discovery.exe")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(100));
    log.extend(host.drain_log());
    assert_eq!(
        log.iter().filter(|line| line.contains("late-drop:discovery.exe")).count(),
        1,
        "the first discovered fire must execute exactly once: {log:?}"
    );

    std::env::set_current_dir(previous).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
