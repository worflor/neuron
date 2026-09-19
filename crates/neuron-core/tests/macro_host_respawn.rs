// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Proof that a DEAD sidecar never hangs the caller, and that the Macro Host recovers — against
//! the REAL python sidecar (modeled on `macro_host_e2e.rs`; same skip-cleanly-with-no-python
//! contract, same private-temp-cwd isolation).
//!
//! Sidecar death and respawn must be non-blocking. Two claims, both read off the actual
//! code in `crate::macros::macro_host` (nothing here is invented):
//!
//!   (a) NON-BLOCKING ON DEATH — [`neuron::macros::MacroHost::fire_async`] is documented as never
//!       blocking the caller: it `try_lock`s, and on a broken pipe (the sidecar just died) it
//!       drops the dead session and kicks a BACKGROUND warm, returning a string immediately. We
//!       kill the sidecar out from under a warm host and prove `fire_async` still returns fast
//!       (wrapped in a thread + `recv_timeout`, so even if this assumption were wrong the test
//!       fails instead of hanging forever).
//!
//!   (b) RECOVERY IS AUTOMATIC ON NEXT USE — every BLOCKING entry point (`register`, `check`,
//!       `parse_macro`, `invoke_with_budget`, `ensure_warm`) calls `ensure_locked`, which checks
//!       the session's `dead` flag and transparently respawns + re-registers every known macro
//!       before serving the request. So the promise this test pins is: call `invoke` again after
//!       the kill, with NO explicit re-warm step, and it must succeed against a freshly spawned
//!       sidecar (a different PID than the one we killed). We do NOT test an unprompted background
//!       auto-respawn — the code doesn't promise one (nothing polls session health), only that the
//!       NEXT use heals it.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable, exactly like the other e2e
//! sidecar tests in this file's neighborhood.

use neuron::macros::{macro_host, Context};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Extract the `pid=<n>` a test macro reports (its sidecar's `os.getpid()`), same convention as
/// `macro_host_e2e.rs`.
fn pid_of(line: &str) -> Option<u32> {
    line.split("pid=")
        .nth(1)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|s| s.parse().ok())
}

#[test]
fn dead_sidecar_never_blocks_and_the_next_use_heals_it() {
    // Bundled interpreter + host scripts materialize on first use; isolate the macros dir into a
    // private temp cwd so this test never touches (or races) the repo's own macros/scripts.
    let tmp = std::env::temp_dir().join(format!("neuron_macro_host_respawn_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping macro_host_respawn: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false); // read-only macro; no input synthesis needed for this proof

    let src = "import os\ndef macro(ctx):\n    return 'pid=%d app=%s' % (os.getpid(), ctx.app)\n";
    host.register("respawn_ok", src).expect("register clean macro");
    let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);

    // ── warm baseline: prove it works, and learn the sidecar's PID ──────────────────────────
    let baseline = host.invoke("respawn_ok", &ctx);
    let pid_before = pid_of(&baseline)
        .unwrap_or_else(|| panic!("baseline invoke did not report a pid: {baseline}"));
    // cross-check against the profiler's record of the spawned child (see `spawn_session`,
    // `crate::prof::SIDECAR_PID.store(child.id(), ...)`) — same fact, two readouts.
    let profiled_pid = neuron::prof::SIDECAR_PID.load(std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        profiled_pid, pid_before,
        "the profiler's SIDECAR_PID must track the same child the macro's os.getpid() reports"
    );

    // ── KILL the sidecar out from under the host, externally ────────────────────────────────
    #[cfg(windows)]
    let killed = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid_before.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    #[cfg(not(windows))]
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid_before.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(killed, "could not externally kill the sidecar (pid {pid_before}) — test can't proceed");

    // give the OS + the host's stdout-reader thread a moment to notice the death (EOF/broken
    // pipe) — best-effort; the assertions below are bounded regardless, so a slow notice can't
    // hang the test, only (at worst) race `fire_async`'s try_lock into a "still looks warm" path,
    // which is itself still required to return immediately (see claim (a)'s own doc comment).
    std::thread::sleep(Duration::from_millis(300));

    // ── (a) NON-BLOCKING: a fire_async call must return promptly, no matter what it reports ──
    let (tx, rx) = mpsc::channel();
    let t0 = Instant::now();
    std::thread::spawn(move || {
        let host = macro_host();
        let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);
        let r = host.fire_async("respawn_ok", &ctx);
        let _ = tx.send(r);
    });
    let fire_result = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("fire_async on a dead sidecar did not return within 5s — it blocked"));
    eprintln!(
        "fire_async on a just-killed sidecar returned in {:?}: {fire_result:?}",
        t0.elapsed()
    );

    // ── (b) RECOVERY ON NEXT USE: invoke() (a blocking entry point) must self-heal via
    // ensure_locked's automatic respawn — no explicit re-warm call here. Bounded generously (the
    // code's own WARM_TIMEOUT is 20s for a cold spawn) so a genuinely broken respawn fails loudly
    // instead of hanging the test suite. ──
    let (tx2, rx2) = mpsc::channel();
    std::thread::spawn(move || {
        let host = macro_host();
        let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);
        let r = host.invoke("respawn_ok", &ctx);
        let _ = tx2.send(r);
    });
    let healed = rx2
        .recv_timeout(Duration::from_secs(30))
        .unwrap_or_else(|_| panic!("invoke() did not return within 30s after the sidecar died — recovery hung"));
    let pid_after = pid_of(&healed)
        .unwrap_or_else(|| panic!("post-kill invoke did not report a pid (recovery failed): {healed}"));
    assert_ne!(
        pid_after, pid_before,
        "recovery must spawn a NEW sidecar process, not somehow reuse the killed pid"
    );
    eprintln!("recovered: pid {pid_before} -> {pid_after} (automatic on next use, no explicit re-warm)");

    // A process can be alive while its CONTROL LOOP is dead: registration executes module top-level
    // code on that loop, so an infinite top-level statement used to leave a forever-"warm" but deaf
    // sidecar. The register deadline must retire that process, leave no durable candidate behind,
    // and let the next normal use heal exactly like a crash does.
    let hang_t0 = Instant::now();
    let hung = host.register(
        "respawn_hung",
        "while True:\n    pass\n\ndef macro(ctx):\n    return 'never'\n",
    );
    assert!(hung.is_err(), "hung top-level registration unexpectedly succeeded");
    assert!(
        hang_t0.elapsed() < Duration::from_secs(8),
        "hung registration exceeded its bounded control-plane deadline: {:?}",
        hang_t0.elapsed()
    );
    assert!(
        neuron::macros::macro_host::load_macro("respawn_hung").is_none(),
        "a timed-out candidate must never become durable"
    );

    let healed_again = host.invoke("respawn_ok", &ctx);
    let pid_after_hang = pid_of(&healed_again)
        .unwrap_or_else(|| panic!("invoke after control-plane timeout did not recover: {healed_again}"));
    assert_ne!(
        pid_after_hang, pid_after,
        "control-plane timeout must retire the deaf sidecar before the next use"
    );

    host.unregister("respawn_ok");
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
