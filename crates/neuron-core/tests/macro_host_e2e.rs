// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! End-to-end proof of the Macro Host's load-bearing claims against the REAL python sidecar:
//!   * WARM PERSISTENCE — one sidecar serves many fires with NO respawn (the same os.getpid()
//!     every time), so a trigger never pays a spawn/import cost. This is the real-time guarantee.
//!   * REAL-TIME DISPATCH — warm round-trips are fast (well under a frame).
//!   * CRASH/ERROR ISOLATION — a raising macro surfaces its error but does NOT kill or respawn the
//!     sidecar; the next macro still runs on the same warm process.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable — so CI without python is fine.

use neuron::macros::{macro_host, Context};
use std::time::{Duration, Instant};

/// Extract the `pid=<n>` the test macro returns (= the sidecar's os.getpid()).
fn pid_of(line: &str) -> Option<String> {
    line.split("pid=")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .map(String::from)
}

#[test]
fn macro_host_warm_persists_and_isolates_errors() {
    // The interpreter + host scripts are BUNDLED in the binary and materialized on first use, so
    // there's nothing to point at — just isolate the macros/scripts dir into a private temp cwd so
    // we never touch the repo's.
    let tmp = std::env::temp_dir().join(format!("neuron_macro_host_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping Macro Host e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false); // read-only macros; no input synthesis needed for the proof

    // A macro that reports the SIDECAR's pid + reads the marshalled context.
    let ok_src =
        "# neuron: raw\nimport os\ndef macro(ctx):\n    return 'pid=%d app=%s' % (os.getpid(), ctx.app)\n";
    host.register("e2e_ok", ok_src)
        .expect("register clean macro");

    let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);

    // Fire many times; the sidecar pid must be IDENTICAL every fire (no respawn => warm).
    const N: usize = 50;
    let mut sidecar_pid: Option<String> = None;
    let t0 = Instant::now();
    for i in 0..N {
        let r = host.invoke("e2e_ok", &ctx);
        assert!(
            r.contains("app=e2e.exe"),
            "fire {i} did not see the context: {r}"
        );
        let pid = pid_of(&r);
        assert!(pid.is_some(), "fire {i} had no pid: {r}");
        match &sidecar_pid {
            None => sidecar_pid = pid,
            Some(p) => assert_eq!(
                pid.as_ref(),
                Some(p),
                "fire {i}: sidecar pid changed — it RESPAWNED (not warm)"
            ),
        }
    }
    let avg = t0.elapsed() / N as u32;
    eprintln!(
        "Macro Host warm dispatch: {N} fires, avg {avg:?}/fire, same sidecar pid {sidecar_pid:?}"
    );
    assert!(
        avg < Duration::from_millis(40),
        "warm dispatch too slow ({avg:?}/fire) — not warm?"
    );

    // Error isolation: a raising macro surfaces its error, but the SAME sidecar keeps serving.
    host.register(
        "e2e_boom",
        "# neuron: raw\ndef macro(ctx):\n    raise ValueError('intentional')\n",
    )
    .unwrap();
    let err = host.invoke("e2e_boom", &ctx);
    assert!(
        err.contains("error"),
        "a raising macro must surface an error: {err}"
    );
    let after = host.invoke("e2e_ok", &ctx);
    assert_eq!(
        pid_of(&after),
        sidecar_pid,
        "a raising macro must NOT respawn the sidecar"
    );

    // A failed REPLACEMENT is different from a macro that raises when fired: module top-level
    // execution happens during register. The old callable + file are the last-known-good revision
    // and must survive a broken edit.
    let stable_src = "def macro(ctx):\n    return 'stable-old'\n";
    host.register("e2e_stable", stable_src).expect("register stable baseline");
    let bad = host.register(
        "e2e_stable",
        "raise RuntimeError('broken candidate')\ndef macro(ctx):\n    return 'never'\n",
    );
    assert!(bad.is_err(), "broken replacement must be rejected");
    let stable_after = host.invoke("e2e_stable", &ctx);
    assert!(
        stable_after.contains("stable-old"),
        "failed replacement destroyed the live last-known-good macro: {stable_after}"
    );
    assert_eq!(
        neuron::macros::macro_host::load_macro("e2e_stable").as_deref(),
        Some(stable_src),
        "failed replacement must not overwrite the durable source"
    );

    // A queued fire belongs to the revision that accepted it. Hold generation N's first fire long
    // enough to queue a second, publish N+1, then prove that queued N work is REFUSED rather than
    // silently executing the new callable.
    host.register(
        "e2e_generation",
        "import time\ndef macro(ctx):\n    print('old:' + ctx.app)\n    time.sleep(0.4)\n",
    )
    .expect("register generation baseline");
    host.drain_log();
    let first = Context::synthetic(Some("first".into()), None, None, None, None);
    let second = Context::synthetic(Some("second".into()), None, None, None, None);
    assert!(host.fire_async("e2e_generation", &first).contains("dispatched"));
    assert!(host.fire_async("e2e_generation", &second).contains("dispatched"));
    std::thread::sleep(Duration::from_millis(80));
    host.register(
        "e2e_generation",
        "def macro(ctx):\n    return 'new:' + ctx.app\n",
    )
    .expect("publish generation replacement");
    std::thread::sleep(Duration::from_millis(700));
    let generation_log = host.drain_log();
    assert!(
        generation_log.iter().any(|l| l.contains("old:first")),
        "the already-running old revision should finish: {generation_log:?}"
    );
    assert!(
        generation_log.iter().any(|l| l.contains("queued for generation")),
        "queued old work must be refused at the revision boundary: {generation_log:?}"
    );
    assert!(
        !generation_log.iter().any(|l| l.contains("new:second")),
        "a queued generation-N fire executed generation N+1: {generation_log:?}"
    );
    let newest = host.invoke(
        "e2e_generation",
        &Context::synthetic(Some("third".into()), None, None, None, None),
    );
    assert!(newest.contains("new:third"), "new fires use the published revision: {newest}");

    // Source execution is the editor/--file path: it runs real Python but is never registered or
    // persisted as a side effect of testing it.
    let candidate = host.invoke_source(
        "e2e_candidate",
        "def macro(ctx):\n    return 'candidate-only'\n",
        &ctx,
    );
    assert!(candidate.contains("candidate-only"), "source candidate did not run: {candidate}");
    assert!(
        neuron::macros::macro_host::load_macro("e2e_candidate").is_none(),
        "source test unexpectedly created macros/scripts/e2e_candidate.py"
    );

    // Delete is runtime truth immediately, not a hint for the next process restart.
    host.register("e2e_delete", "def macro(ctx):\n    return 'present'\n")
        .expect("register delete target");
    neuron::macros::macro_host::delete_macro("e2e_delete").expect("delete target");
    let deleted = host.invoke("e2e_delete", &ctx);
    assert!(
        deleted.contains("not registered"),
        "deleted macro remained callable in the warm sidecar: {deleted}"
    );

    host.unregister("e2e_generation");
    host.unregister("e2e_stable");
    host.unregister("e2e_ok");
    host.unregister("e2e_boom");
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
