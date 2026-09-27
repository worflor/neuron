// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Cold registration uses acknowledged control-plane flow. A catalog larger than the live writer
//! queue must warm completely instead of overflowing that queue and killing a healthy sidecar.

use neuron::macros::{macro_host, Context};

#[test]
fn cold_catalog_larger_than_the_live_queue_registers_completely() {
    // Production's bounded writer holds 1,024 frames. Crossing that exact boundary proves cold
    // registration is acknowledged serially instead of filling the live queue.
    const COUNT: usize = 1025;
    let tmp = std::env::temp_dir().join(format!(
        "neuron_macro_large_catalog_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let scripts = tmp.join("macros").join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    for i in 0..COUNT {
        std::fs::write(
            scripts.join(format!("catalog_{i:03}.py")),
            format!("def macro(ctx):\n    return 'catalog-{i:03}'\n"),
        ).unwrap();
    }

    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);
    let host = macro_host();
    if !host.available() {
        eprintln!("skipping large-catalog Macro Host e2e: runtime unavailable");
        std::env::set_current_dir(previous).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false);
    host.ensure_warm().expect("cold catalog should register without saturating the live queue");

    let last_id = format!("catalog_{:03}", COUNT - 1);
    let result = host.invoke(
        &last_id,
        &Context::synthetic(Some("catalog.exe".into()), None, None, None, None),
    );
    assert!(
        result.contains(&format!("catalog-{:03}", COUNT - 1)),
        "last catalog entry was not registered: {result}"
    );

    std::env::set_current_dir(previous).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
