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
        "import os\ndef macro(ctx):\n    return 'pid=%d app=%s' % (os.getpid(), ctx.app)\n";
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
        "def macro(ctx):\n    raise ValueError('intentional')\n",
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

    host.unregister("e2e_ok");
    host.unregister("e2e_boom");
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
