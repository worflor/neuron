// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! NON-DESTRUCTIVE stress tests for the PYRUNTIME dimension — the bundled-CPython lifecycle the macro
//! sidecar runs on: extraction (atomic, idempotent, concurrent-safe), the slim runtime's stdlib
//! completeness, host-script co-location, and the warm sidecar's spawn / crash / respawn / circuit-
//! breaker lifecycle.
//!
//! TWO TIERS (mirroring `macro_stress_beacons.rs`):
//!   * PURE / INDEPENDENT — extraction + materialized-runtime checks that DON'T touch the global warm
//!     sidecar (they call `ensure_runtime()` and spawn the bundled interpreter directly with `-c`).
//!     Each is its own parallel-safe `#[test]`.
//!   * SIDECAR E2E — everything that drives the process-global Macro Host (register/fire/crash/respawn/
//!     breaker) runs in ONE serial `#[test]` (`pyruntime_sidecar_stress_e2e`) with sequential phases,
//!     because the host + cwd + beacon slot + bounded log ring are all process-global. DISARMED + temp
//!     cwd isolated, so no key/click/device write can occur. The circuit-breaker phase (which trips the
//!     global breaker and waits out its real 20s cooldown) is LAST and restores state via a drop guard.
//!
//! TESTABILITY NOTE (reported, not faked): a few spec cases — extraction into a READ-ONLY data dir and
//! `write_if_changed` PERMISSION-DENIED — can't be exercised non-destructively today: `runtime_base()`
//! is a fixed, non-overridable data-local path and `ensure_runtime()` memoizes globally, so after the
//! first (successful) materialization the failure paths are unreachable without making the user's REAL
//! runtime dir read-only (destructive) or adding a `NEURON_RUNTIME_BASE` test seam. We assert the
//! reachable contracts (idempotence, atomic-temp cleanup) and recommend the seam in the report.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::macros::{ensure_runtime, macro_host, BeaconEvent, Context, MacroHost};
use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// What `neuron.key` answers when it synthesizes nothing. `[disarmed]` is the arm gate; off Windows
/// the platform check runs first (there is no `SendInput` to reach) and answers `[unsupported]`.
/// Either marker proves the same thing here: no input reached the OS.
#[cfg(windows)]
const KEY_NO_OP: &str = "[disarmed]";
#[cfg(not(windows))]
const KEY_NO_OP: &str = "[unsupported]";

// ── shared helpers ───────────────────────────────────────────────────────────────────────────────

/// Run the bundled interpreter with `-c <code>`; return (success, stdout, stderr). No state change.
fn run_py(python: &Path, code: &str) -> (bool, String, String) {
    let mut cmd = Command::new(python);
    cmd.arg("-c").arg(code);
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn bundled python {}: {e}", python.display()));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// First run of ASCII digits after `key`, parsed.
fn num_after(s: &str, key: &str) -> Option<u64> {
    s.split(key)
        .nth(1)
        .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
        .filter(|d| !d.is_empty())
        .and_then(|d| d.parse().ok())
}

/// A disarmed synthetic context tagged with `app`.
fn cx(app: &str) -> Context {
    Context::synthetic(Some(app.to_string()), None, None, None, None)
}

/// A tiny "run F on drop" guard (restores global state even when a phase panics).
struct Defer<F: FnMut()>(F);
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)()
    }
}

// ──────────────────────────────────────────────────────────────────────────────────────────────
// PURE / INDEPENDENT — extraction + materialized-runtime, no warm sidecar (parallel-safe).
// ──────────────────────────────────────────────────────────────────────────────────────────────

#[test]
fn concurrent_extraction_race() {
    // Twelve threads call ensure_runtime() at once. The in-process Mutex + on-disk idempotence
    // (interpreter-binary-exists short-circuit) + atomic temp→rename mean only one real extraction can
    // happen and EVERY thread must resolve the identical, valid runtime paths — no half-state, no
    // divergence.
    let handles: Vec<_> = (0..12)
        .map(|_| std::thread::spawn(ensure_runtime))
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().expect("thread")).collect();

    let oks: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    if oks.is_empty() {
        eprintln!("skipping concurrent_extraction_race: bundled runtime unavailable");
        return;
    }
    assert_eq!(oks.len(), 12, "every concurrent ensure_runtime() must succeed: {results:?}");
    let first = oks[0];
    for rt in &oks {
        assert_eq!(rt.python, first.python, "all threads resolve the SAME interpreter path");
        assert_eq!(rt.host_script, first.host_script, "all threads resolve the SAME host script");
        assert_eq!(rt.host_dir, first.host_dir, "all threads resolve the SAME host dir");
    }
    assert!(first.python.exists(), "resolved interpreter must exist on disk");
    assert!(first.host_script.exists(), "resolved host script must exist on disk");
    assert!(first.host_dir.join("neuron.py").exists(), "neuron.py must be co-located");
}

#[test]
fn slim_imports_stdlib_essentials() {
    // The slim step (build.rs) PRUNES pip/test/.pdb but must KEEP everything a macro could import. The
    // existing tests only check the EMBEDDED tarball's file list; this checks the EXTRACTED interpreter
    // actually imports the load-bearing stdlib — including the C extensions (sqlite3/_ssl/ctypes/struct)
    // whose shared libs must have survived slimming.
    let rt = match ensure_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping slim_imports_stdlib_essentials: {e}");
            return;
        }
    };
    let (ok, so, se) = run_py(
        &rt.python,
        "import sqlite3, ssl, json, urllib.request, subprocess, ctypes, ast, struct, hashlib, socket; print('IMPORTS_OK')",
    );
    assert!(ok && so.contains("IMPORTS_OK"), "bundled runtime must import the slim-KEPT stdlib essentials\nstdout={so}\nstderr={se}");

    // sqlite3 + ssl must be FUNCTIONAL, not just importable (their C extensions + bundled libs work).
    let (ok2, so2, se2) = run_py(
        &rt.python,
        "import sqlite3, ssl; sqlite3.connect(':memory:').execute('create table t(x)'); ssl.create_default_context(); print('FUNC_OK')",
    );
    assert!(ok2 && so2.contains("FUNC_OK"), "sqlite3 + ssl must be functional on the slim runtime\nstdout={so2}\nstderr={se2}");

    // tkinter is kept by the slim step (a macro could use it). Import the MODULE only — Tk() needs a
    // display and would fail headless; importing proves the _tkinter extension + tcl runtime survived.
    let (ok3, _so3, se3) = run_py(&rt.python, "import tkinter; print('TK_OK')");
    assert!(ok3, "bundled runtime must import tkinter (kept by the slim step): {se3}");
}

#[test]
fn archive_extraction_permission_preservation() {
    // The pure-Rust untar preserves unix permission bits, so on Unix the interpreter stays executable;
    // on Windows it must materialize as a regular file. Read-only stat — touches no permissions.
    let rt = match ensure_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping archive_extraction_permission_preservation: {e}");
            return;
        }
    };
    let meta = std::fs::metadata(&rt.python).expect("stat the bundled interpreter");
    assert!(meta.is_file(), "the bundled interpreter must be a regular file: {}", rt.python.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert!(
            meta.permissions().mode() & 0o111 != 0,
            "extraction must preserve the +x bit on python3 (mode {:o})",
            meta.permissions().mode()
        );
    }
}

#[test]
fn host_scripts_codepaths_after_extract() {
    // neuron_host.py + neuron.py must be written, co-located, non-trivial, and VALID python (parsed by
    // the bundled interpreter's own ast — no exec, no side effects).
    let rt = match ensure_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping host_scripts_codepaths_after_extract: {e}");
            return;
        }
    };
    let neuron_py = rt.host_dir.join("neuron.py");
    assert!(rt.host_script.exists(), "neuron_host.py must exist after extract");
    assert!(neuron_py.exists(), "neuron.py must be co-located after extract");
    assert!(std::fs::metadata(&rt.host_script).unwrap().len() > 100, "neuron_host.py must be non-trivial");
    assert!(std::fs::metadata(&neuron_py).unwrap().len() > 100, "neuron.py must be non-trivial");
    for p in [&rt.host_script, &neuron_py] {
        let fwd = p.display().to_string().replace('\\', "/"); // forward slashes parse cleanly on Windows
        let code = format!("import ast,io; ast.parse(io.open(r'{fwd}', encoding='utf-8').read()); print('PARSE_OK')");
        let (ok, so, se) = run_py(&rt.python, &code);
        assert!(ok && so.contains("PARSE_OK"), "host script {} must be parseable python: {se}", p.display());
    }
}

#[test]
fn no_stale_extraction_temp_dirs() {
    // The atomic extract unpacks into a sibling `<py-dir>.<pid>.<nanos>.tmp` then renames it into place,
    // cleaning the temp on every path (success, race-loss, unpack error). After a successful
    // ensure_runtime() the runtime base must hold NO leftover .tmp dir — the cleanup contract for the
    // extract_aborted case, observed read-only on the real base.
    let rt = match ensure_runtime() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("skipping no_stale_extraction_temp_dirs: {e}");
            return;
        }
    };
    // host_dir == <base>/host, so the runtime base (where py-* and its temps live) is its parent.
    let base = rt.host_dir.parent().expect("runtime base").to_path_buf();
    let mut stale = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&base) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".tmp") {
                stale.push(name);
            }
        }
    }
    assert!(stale.is_empty(), "extraction must leave no stale temp dirs in {}: {stale:?}", base.display());
}

// ──────────────────────────────────────────────────────────────────────────────────────────────
// SIDECAR E2E — one serial test, sequential phases. DISARMED + temp-cwd isolated.
// ──────────────────────────────────────────────────────────────────────────────────────────────

/// Receive the next `Ask` (skipping noise) as (pid, macro_id), or panic on timeout.
fn recv_ask(rx: &Receiver<BeaconEvent>, dur: Duration) -> (u64, String) {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(BeaconEvent::Ask { pid, macro_id, .. }) => return (pid, macro_id),
            Ok(_) => continue,
            Err(_) => panic!("timed out waiting for an Ask event"),
        }
    }
}

/// Wait for `RetireDomain` within `dur`.
fn wait_retire_all(rx: &Receiver<BeaconEvent>, dur: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match rx.recv_timeout(left) {
            Ok(BeaconEvent::RetireDomain { mode: neuron::macros::MacroMode::Raw }) => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// Drain `Notify` events into (macro_id, text) until `settle` quiet (or `max` budget).
fn drain_notifies(rx: &Receiver<BeaconEvent>, settle: Duration, max: Duration) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let deadline = Instant::now() + max;
    loop {
        let left = settle.min(deadline.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(BeaconEvent::Notify { macro_id, text }) => out.push((macro_id, text)),
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// Drain the macro-log ring until `settle` quiet (or `max`).
fn drain_log_for(host: &MacroHost, settle: Duration, max: Duration) -> Vec<String> {
    let mut acc = Vec::new();
    let deadline = Instant::now() + max;
    let mut last = Instant::now();
    loop {
        let batch = host.drain_log();
        if !batch.is_empty() {
            acc.extend(batch);
            last = Instant::now();
        }
        if Instant::now() >= deadline || last.elapsed() >= settle {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    acc
}

/// The current warm sidecar pid (set by spawn_session), per the profiler hook.
fn current_pid() -> u32 {
    neuron::prof::SIDECAR_PID.load(Ordering::Relaxed)
}

/// Invoke a pid-reporting macro; returns the sidecar's os.getpid(), or None if it didn't answer.
fn pid_via_invoke(host: &MacroHost, pid_id: &str) -> Option<u32> {
    let r = host.invoke(pid_id, &cx("p"));
    num_after(&r, "pid=").map(|p| p as u32)
}

/// Fire the os._exit macro to crash the firewalled sidecar (its whole reason to exist).
fn fire_crash(host: &MacroHost, boom_id: &str) {
    let _ = host.fire_async(boom_id, &cx("x"));
}

/// Loop a pid-invoke until the sidecar reports a NEW pid (respawn confirmed), or panic past `25s`.
fn wait_respawn(host: &MacroHost, old: u32, pid_id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(p) = pid_via_invoke(host, pid_id) {
            if p != old && p != 0 {
                return p;
            }
        }
        if Instant::now() >= deadline {
            panic!("sidecar did not respawn with a new pid after crash (old={old})");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Crash + confirm respawn; returns (old_pid, new_pid). Sleeps briefly so mark_dead settles first.
fn crash_then_respawn(host: &MacroHost, boom_id: &str, pid_id: &str) -> (u32, u32) {
    let old = pid_via_invoke(host, pid_id).unwrap_or_else(current_pid);
    fire_crash(host, boom_id);
    std::thread::sleep(Duration::from_millis(500));
    let new = wait_respawn(host, old, pid_id);
    (old, new)
}

#[test]
fn pyruntime_sidecar_stress_e2e() {
    let tmp = std::env::temp_dir().join(format!("neuron_pyrt_stress_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping pyruntime sidecar stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false);
    let _ = host.ensure_warm();

    // utility macros that survive respawns (in the manifest) — a pid reporter and a hard-crasher.
    host.register("pr_pid", "# neuron: raw\nimport os\ndef macro(ctx):\n    return 'pid=%d' % os.getpid()\n")
        .expect("register pr_pid");
    host.register("pr_boom", "# neuron: raw\nimport os\ndef macro(ctx):\n    os._exit(7)\n")
        .expect("register pr_boom");

    // ── PHASE 1: REGISTER WITH A BAD IMPORT — surfaced as Err, sidecar stays warm ────────────────
    {
        let pid_before = pid_via_invoke(host, "pr_pid").expect("warm sidecar pid");
        let bad = host.register(
            "pr_bad",
            "import nonexistent_module_xyz\ndef macro(ctx):\n    return 'never'\n",
        );
        assert!(bad.is_err(), "a macro with a bad import must FAIL to register (the exec raises)");
        let msg = bad.unwrap_err();
        assert!(
            msg.contains("nonexistent_module_xyz")
                || msg.to_lowercase().contains("modulenotfound")
                || msg.to_lowercase().contains("no module"),
            "the register error must carry the import traceback: {msg}"
        );
        // the sidecar is unharmed: a good macro still registers + runs on the SAME warm pid.
        host.register("pr_good", "def macro(ctx):\n    return 'good'\n").expect("good macro registers");
        let r = host.invoke("pr_good", &cx("g"));
        assert!(r.contains("good"), "a good macro runs after a bad-import register: {r}");
        let pid_after = pid_via_invoke(host, "pr_pid").expect("still warm");
        assert_eq!(pid_after, pid_before, "a bad-import register must NOT crash/respawn the sidecar");
        host.unregister("pr_bad");
        host.unregister("pr_good");
    }

    // ── PHASE 2: NESTED / DYNAMIC IMPORT inside a macro body ─────────────────────────────────────
    {
        host.register(
            "pr_nested",
            "# neuron: raw\nimport ctypes\ndef macro(ctx):\n    import ssl\n    return 'sslctx=%s ctypes=%s' % (ssl.SSLContext.__name__, ctypes.__name__)\n",
        )
        .expect("register pr_nested");
        let r = host.invoke("pr_nested", &cx("n"));
        assert!(r.contains("sslctx=SSLContext"), "ssl imports dynamically inside the macro body: {r}");
        assert!(r.contains("ctypes=ctypes"), "module-scope ctypes import is available to the macro: {r}");
        host.unregister("pr_nested");
    }

    // ── PHASE 3: LOGGER THREAD long-line overflow — truncated at 8192 bytes, no corruption ───────
    {
        host.register(
            "pr_long",
            "import neuron\ndef macro(ctx):\n    neuron.log('LONGLINE-' + 'X' * 10000)\n    return 'logged'\n",
        )
        .expect("register pr_long");
        host.drain_log();
        let r = host.invoke("pr_long", &cx("l"));
        assert!(r.contains("logged"), "the macro completes despite the huge log line: {r}");
        let log = drain_log_for(host, Duration::from_millis(500), Duration::from_secs(8));
        let line = log
            .iter()
            .find(|l| l.starts_with("LONGLINE-"))
            .unwrap_or_else(|| panic!("the long log line must reach the ring: {log:?}"));
        assert!(
            line.len() <= 8192,
            "the logger must truncate a >8192-byte line at the cap (got {} bytes)",
            line.len()
        );
        // no corruption: the kept prefix is "LONGLINE-" followed only by the 'X' payload.
        assert!(
            line.strip_prefix("LONGLINE-").map(|tail| tail.bytes().all(|b| b == b'X')).unwrap_or(false),
            "the truncated line must be clean (prefix + only the payload bytes): {:?}",
            &line[..line.len().min(40)]
        );
        // sidecar survived the flood: still warm, still serving.
        assert!(pid_via_invoke(host, "pr_pid").is_some(), "the sidecar stays warm after the long line");
        host.unregister("pr_long");
    }

    // ── PHASE 4: 20 CONCURRENT FIRES land on ONE warm sidecar (no respawn) ───────────────────────
    {
        let ids: Vec<String> = (0..20).map(|i| format!("pr_cf{i}")).collect();
        for id in &ids {
            host.register(id, "# neuron: raw\nimport os\ndef macro(ctx):\n    notify('cf %s pid=%d' % (ctx.app, os.getpid()))\n")
                .unwrap_or_else(|e| panic!("register {id}: {e}"));
        }
        let rx = host.beacon_events();
        host.drain_log();
        let mut handles = Vec::new();
        for id in ids.clone() {
            handles.push(std::thread::spawn(move || {
                let h = macro_host();
                let c = cx(&id);
                let deadline = Instant::now() + Duration::from_secs(15);
                // retry under inner-lock contention until the warm dispatch actually goes out.
                while Instant::now() < deadline {
                    if h.fire_async(&id, &c).contains("dispatched") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }));
        }
        for h in handles {
            let _ = h.join();
        }
        let notifs = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(25));
        assert_eq!(notifs.len(), 20, "all 20 concurrent fires must complete: {}", notifs.len());
        let pids: Vec<u64> = notifs.iter().filter_map(|(_, t)| num_after(t, "pid=")).collect();
        assert!(pids.iter().all(|p| *p == pids[0]), "20 concurrent fires must share ONE sidecar (no respawn): {pids:?}");
        let distinct: std::collections::BTreeSet<_> = notifs.iter().map(|(m, _)| m.clone()).collect();
        assert_eq!(distinct.len(), 20, "each of the 20 distinct macros completed once");
        for id in &ids {
            host.unregister(id);
        }
    }

    // ── PHASE 5: READER-THREAD EOF on crash → clean respawn; ensure_runtime() idempotent ─────────
    {
        let old_rt = ensure_runtime().expect("runtime resolves before crash");
        let (old, new) = crash_then_respawn(host, "pr_boom", "pr_pid");
        assert_ne!(new, old, "an unexpected sidecar death must respawn with a new pid");
        assert_ne!(new, 0);
        // the reader hit EOF and the host transparently re-warmed: a fire works on the new pid.
        let r = host.invoke("pr_pid", &cx("after5"));
        assert_eq!(num_after(&r, "pid="), Some(new as u64), "the respawned sidecar serves invokes: {r}");
        // ensure_runtime is idempotent across the respawn — NO re-extraction, identical paths.
        let new_rt = ensure_runtime().expect("runtime resolves after respawn");
        assert_eq!(new_rt.python, old_rt.python, "ensure_runtime() must reuse the extracted interpreter across respawns");
        assert_eq!(new_rt.host_dir, old_rt.host_dir, "host dir is stable across respawns");
        assert_eq!(new_rt.host_script, old_rt.host_script, "host script is stable across respawns");
    }

    // ── PHASE 6: ARM STATE rides the respawn handshake; mock forces disarm ───────────────────────
    {
        let _armed_guard = Defer(|| macro_host().set_armed(false)); // restore even on panic
        host.register("pr_armed", "def macro(ctx):\n    notify('armed=%r' % neuron.armed())\n")
            .expect("register pr_armed");
        host.register("pr_mockkey", "def macro(ctx):\n    notify('mockkey=%r' % neuron.key('a'))\n")
            .expect("register pr_mockkey");

        // disarmed: the macro reads False.
        host.set_armed(false);
        let rx = host.beacon_events();
        assert!(host.fire_async("pr_armed", &cx("a")).contains("dispatched"));
        let n = drain_notifies(&rx, Duration::from_millis(800), Duration::from_secs(10));
        assert!(n.iter().any(|(_, t)| t.contains("armed=False")), "disarmed sidecar reports armed=False: {n:?}");

        // arm, crash, respawn — the NEW sidecar must learn armed=True from the spawn handshake.
        host.set_armed(true);
        let (_old, new) = crash_then_respawn(host, "pr_boom", "pr_pid");
        let rx = host.beacon_events();
        assert!(host.fire_async("pr_armed", &cx("a")).contains("dispatched"));
        let n = drain_notifies(&rx, Duration::from_millis(800), Duration::from_secs(10));
        assert!(
            n.iter().any(|(_, t)| t.contains("armed=True")),
            "the respawned sidecar must read the CURRENT arm state from its handshake: {n:?}"
        );
        // even while armed, a MOCK fire forces input off (no real keystroke ever leaves this test).
        assert!(host.fire_mock("pr_mockkey", &cx("m")).contains("dispatched"));
        let n = drain_notifies(&rx, Duration::from_millis(800), Duration::from_secs(10));
        assert!(
            n.iter().any(|(_, t)| t.contains(&format!("mockkey='{KEY_NO_OP}'"))),
            "a mock fire suppresses input even under armed (per-fire gate): {n:?}"
        );
        assert_eq!(num_after(&host.invoke("pr_pid", &cx("p")), "pid="), Some(new as u64), "same respawned pid throughout");
        host.set_armed(false);
        host.unregister("pr_armed");
        host.unregister("pr_mockkey");
    }

    // ── PHASE 7: BEACON delivery across a respawn — RetireDomain, then fresh asks work ──────────────
    {
        let rx = host.beacon_events();
        host.register("pr_inflight", "# neuron: raw\ndef macro(ctx):\n    ask('hold', timeout=30)\n").expect("register RAW pr_inflight");
        assert!(host.fire_async("pr_inflight", &cx("i")).contains("dispatched"));
        let _ = recv_ask(&rx, Duration::from_secs(15)); // an ask is open when the rug is pulled

        let old = pid_via_invoke(host, "pr_pid").unwrap_or_else(current_pid);
        fire_crash(host, "pr_boom");
        assert!(wait_retire_all(&rx, Duration::from_secs(15)), "a RAW crash must retire the RAW prompt domain");
        std::thread::sleep(Duration::from_millis(400));
        let new = wait_respawn(host, old, "pr_pid");
        assert_ne!(new, old, "respawn after the beacon crash");

        // the respawned sidecar takes a fresh ask and answers it.
        let rx = host.beacon_events();
        host.register("pr_revive", "# neuron: raw\ndef macro(ctx):\n    return 'rv=%r' % ask('revive?', timeout=15)\n")
            .expect("register RAW pr_revive");
        let (tx, done) = std::sync::mpsc::channel();
        {
            let c = cx("rv");
            std::thread::spawn(move || {
                let _ = tx.send(macro_host().invoke("pr_revive", &c));
            });
        }
        let (pid, _) = recv_ask(&rx, Duration::from_secs(15));
        host.answer(pid, Some(0));
        let r = done.recv_timeout(Duration::from_secs(20)).expect("the revived ask returns");
        assert!(r.contains("rv=True"), "the respawned sidecar serves a fresh beacon round-trip: {r}");
        host.unregister("pr_inflight");
        host.unregister("pr_revive");
    }

    // ── PHASE 8: BOGUS INTERPRETER → clean spawn failure, then transparent recovery ─────────────
    // A NEURON_PYTHON override pointing at a non-executable file must make spawn fail with a clear Err
    // (no panic, no hang). Clearing the override re-warms on the bundled interpreter.
    {
        let bogus = tmp.join("not_a_python_p8.bin");
        std::fs::write(&bogus, b"this is not an executable image").unwrap();
        let _env_guard = Defer(|| std::env::remove_var("NEURON_PYTHON"));

        let rx = host.beacon_events();
        fire_crash(host, "pr_boom");
        let _ = wait_retire_all(&rx, Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(500)); // let mark_dead settle so ensure takes the spawn path

        std::env::set_var("NEURON_PYTHON", &bogus);
        let res = host.ensure_warm();
        assert!(res.is_err(), "a bogus interpreter override must fail spawn cleanly (Err, no panic): {res:?}");
        let msg = res.unwrap_err();
        assert!(
            msg.to_lowercase().contains("spawn") || msg.to_lowercase().contains("python"),
            "the spawn-failure error must be descriptive: {msg}"
        );

        std::env::remove_var("NEURON_PYTHON");
        let mut recovered = false;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if host.ensure_warm().is_ok() {
                recovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        assert!(recovered, "clearing the bogus override must let the sidecar re-warm on the bundled interpreter");
        assert!(pid_via_invoke(host, "pr_pid").is_some(), "the recovered sidecar serves fires again");
    }

    // ── PHASE 9 (LAST): CIRCUIT BREAKER trips on repeated spawn failures, then recovers after cooldown ──
    // Hammer the spawn path with a bogus interpreter: after the crash ceiling the breaker trips and
    // ensure_warm() reports the disabled state. After the real 20s cooldown (waited out here), a clear
    // interpreter must warm again. The drop guard GUARANTEES the global breaker is reset + the env is
    // clean before this serial test releases — even if an assert panics — so nothing leaks out.
    {
        let bogus = tmp.join("not_a_python_p9.bin");
        std::fs::write(&bogus, b"this is not an executable image").unwrap();
        let _restore = Defer(|| {
            std::env::remove_var("NEURON_PYTHON");
            let dl = Instant::now() + Duration::from_secs(40);
            while Instant::now() < dl {
                if macro_host().ensure_warm().is_ok() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });

        let rx = host.beacon_events();
        fire_crash(host, "pr_boom");
        let _ = wait_retire_all(&rx, Duration::from_secs(15));
        std::thread::sleep(Duration::from_millis(500));

        std::env::set_var("NEURON_PYTHON", &bogus);
        let mut tripped = false;
        for _ in 0..16 {
            if let Err(e) = host.ensure_warm() {
                if e.contains("disabled") {
                    tripped = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(tripped, "repeated spawn failures must trip the circuit breaker into the 'disabled' state");

        // recover: clear the override and wait out BREAKER_COOLDOWN (20s in code); the next warm succeeds.
        std::env::remove_var("NEURON_PYTHON");
        let mut recovered = false;
        let deadline = Instant::now() + Duration::from_secs(35);
        while Instant::now() < deadline {
            if host.ensure_warm().is_ok() {
                recovered = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(300));
        }
        assert!(recovered, "after the breaker cooldown expires, a clear interpreter must re-warm the sidecar");
        assert!(pid_via_invoke(host, "pr_pid").is_some(), "the post-cooldown sidecar serves fires again");
    }

    // ── teardown ────────────────────────────────────────────────────────────────────────────────
    host.unregister("pr_pid");
    host.unregister("pr_boom");
    host.drain_log();
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
