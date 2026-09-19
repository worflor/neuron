// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! End-to-end authority proof for the two Python macro domains. One test owns this integration-test
//! process so its process-global MacroHost, run-root override and macro-state override cannot race a
//! sibling test. No hardware is touched and input stays disarmed for the entire run.

use neuron::macros::{macro_host, Context};
use std::time::Duration;

#[test]
fn bound_and_raw_are_distinct_authority_domains() {
    let tmp = std::env::temp_dir().join(format!(
        "neuron_bound_raw_e2e_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);
    std::env::set_var("NEURON_MACRO_STATE", tmp.join("state"));

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping bound/raw e2e: bundled python runtime did not materialize");
        std::env::remove_var("NEURON_MACRO_STATE");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }

    neuron::action::arm_input(false);
    host.set_armed(false);
    let ctx = Context::synthetic(
        Some("authority.exe".into()),
        None,
        Some(tmp.clone()),
        None,
        None,
    );

    // The first scan establishes the one-time migration marker while the directory is empty.
    // Everything registered after this point is genuinely new source and absence of a pragma means BOUND.
    assert!(neuron::macros::macro_host::scan_macro_dir().is_empty());

    host.register(
        "bound_surface",
        r#"def macro(ctx):
    out = []
    try:
        f = open(ctx.cwd + "/bound_touch.txt", "w")
        f.write("bad")
        f.close()
        out.append("open=BAD")
    except Exception:
        out.append("open=blocked")
    try:
        import os
        out.append("import=BAD")
    except Exception:
        out.append("import=blocked")
    out.append("run=%s" % neuron.run("echo should-not-run"))
    return "|".join(out)
"#,
    )
    .expect("register BOUND surface probe");
    let bounded = host.invoke("bound_surface", &ctx);
    assert!(bounded.contains("open=blocked"), "BOUND retained file authority: {bounded}");
    assert!(bounded.contains("import=blocked"), "BOUND imported ambient os authority: {bounded}");
    assert!(
        bounded.contains("run=[requires RAW]"),
        "BOUND process execution did not require RAW: {bounded}"
    );
    assert!(
        !tmp.join("bound_touch.txt").exists(),
        "BOUND wrote a file through ambient Python authority"
    );

    host.register(
        "raw_surface",
        r#"# neuron: raw
import os

def macro(ctx):
    p = ctx.cwd + "/raw_touch.txt"
    with open(p, "w", encoding="utf-8") as f:
        f.write(str(os.getpid()))
    return "raw-ok"
"#,
    )
    .expect("register RAW surface probe");
    let raw = host.invoke("raw_surface", &ctx);
    assert!(raw.contains("raw-ok"), "RAW lost ordinary Python power: {raw}");
    assert!(tmp.join("raw_touch.txt").exists(), "RAW file write did not land");

    // Same-domain synchronous invocation keeps native Python return values.
    host.register("bound_num", "def macro(ctx):\n    return 7\n")
        .expect("register BOUND numeric callee");
    host.register(
        "bound_same_domain",
        "def macro(ctx):\n    return neuron.invoke('bound_num') + 1\n",
    )
    .expect("register BOUND same-domain caller");
    assert!(
        host.invoke("bound_same_domain", &ctx).contains("8"),
        "same-domain BOUND invoke lost native Python return semantics"
    );

    host.register(
        "bound_target",
        "def macro(ctx):\n    return 'bound:%s:%s' % (neuron.option('x', '?'), ctx.app)\n",
    )
    .expect("register BOUND bridge target");
    host.register(
        "raw_calls_bound",
        "# neuron: raw\ndef macro(ctx):\n    return neuron.invoke('bound_target', x='X')\n",
    )
    .expect("register RAW caller");
    let down = host.invoke("raw_calls_bound", &ctx);
    assert!(
        down.contains("bound:X:authority.exe"),
        "RAW -> BOUND did not execute in the target domain: {down}"
    );

    host.register(
        "raw_target",
        "# neuron: raw\ndef macro(ctx):\n    return 'raw-target'\n",
    )
    .expect("register RAW target");
    host.register(
        "bound_calls_raw",
        "def macro(ctx):\n    return repr(neuron.invoke('raw_target'))\n",
    )
    .expect("register BOUND caller");
    let up = host.invoke("bound_calls_raw", &ctx);
    assert!(
        up.contains("None"),
        "BOUND -> RAW crossed the authority boundary: {up}"
    );

    // BOUND state is host-owned but file-compatible with RAW, so changing a macro's mode does not
    // silently fork or lose its persistent namespace.
    host.register(
        "state_bridge",
        "def macro(ctx):\n    n = neuron.load('n', 0) + 1\n    neuron.store('n', n)\n    return n\n",
    )
    .expect("register BOUND state macro");
    assert!(host.invoke("state_bridge", &ctx).contains("1"));
    assert!(host.invoke("state_bridge", &ctx).contains("2"));
    host.register(
        "state_bridge",
        "# neuron: raw\ndef macro(ctx):\n    return neuron.load('n', 0)\n",
    )
    .expect("switch state macro to RAW");
    assert!(
        host.invoke("state_bridge", &ctx).contains("2"),
        "mode switch lost the macro's persistent state"
    );

    // A BOUND async invocation remains on the callee's serial worker and is observable through state.
    host.register(
        "bound_async_target",
        "def macro(ctx):\n    neuron.store('ran', 1)\n",
    )
    .expect("register BOUND async target");
    host.register(
        "raw_async_caller",
        "# neuron: raw\ndef macro(ctx):\n    neuron.invoke('bound_async_target', wait=False)\n    return 'queued'\n",
    )
    .expect("register RAW async caller");
    assert!(host.invoke("raw_async_caller", &ctx).contains("queued"));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let state_path = tmp.join("state").join("bound_async_target.json");
        if std::fs::read_to_string(&state_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("ran").and_then(serde_json::Value::as_i64))
            == Some(1)
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("cross-domain wait=False never reached the BOUND target");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    for id in [
        "bound_surface",
        "raw_surface",
        "bound_num",
        "bound_same_domain",
        "bound_target",
        "raw_calls_bound",
        "raw_target",
        "bound_calls_raw",
        "state_bridge",
        "bound_async_target",
        "raw_async_caller",
    ] {
        host.unregister(id);
    }

    std::env::remove_var("NEURON_MACRO_STATE");
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
