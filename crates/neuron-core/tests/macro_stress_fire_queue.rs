// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! NON-DESTRUCTIVE stress tests for the FIRE-QUEUE dimension of the macro system — the per-macro
//! SERIAL worker + bounded `queue.Queue(maxsize=256)` that live in the python sidecar
//! (`runtime/host/neuron_host.py`), driven through the Rust [`MacroHost`].
//!
//! What the fire queue promises (and what these phases prove against the REAL warm sidecar):
//!   * BOUNDED + DROP-NEWEST — a macro fired faster than it runs can't grow its backlog without
//!     bound: the per-macro queue caps at `_FIRE_QUEUE_MAX = 256`, and a full queue drops the NEWEST
//!     fire (with a log line) rather than blocking the protocol loop (a blocked loop would deafen the
//!     whole sidecar). Verified by holding the worker, overflowing the queue, and proving exactly the
//!     OLDEST 256 survive in order while the newest are dropped + logged.
//!   * PER-MACRO INDEPENDENCE — different macros have different queues + workers, so saturating one
//!     never blocks another (the fire-storm phase saturates THREE at once).
//!   * IN-ORDER PER MACRO — one macro's queued fires execute strictly in enqueue order.
//!   * CRASH DROPS THE QUEUE (no zombie re-run) — a sidecar crash with a full queue drops every
//!     in-flight fire (they are NOT re-run on respawn) and surfaces RetireDomain.
//!   * FIRE_BUDGET timeout under pressure — a blocking `invoke` behind a busy queue times out at the
//!     budget (not earlier, not hung) and its fire is not stranded (it lands in the log when it runs).
//!   * PER-THREAD STDOUT ISOLATION — concurrent fires each capture their own stdout (`_ThreadStdout`),
//!     so 5 macros printing at once never interleave a line into another's result.
//!   * MOCK ↔ REAL on one queue — a mock fire and a (disarmed) real fire interleave on the queue, both
//!     raise their REAL beacon, and neither synthesizes input.
//!
//! IDIOM (mirrors `macro_stress_beacons.rs` / `macro_host_beacon_e2e.rs`): the Macro Host is a
//! process-global singleton and the cwd / beacon slot / bounded log ring are process-global too, so
//! ALL sidecar work runs in ONE serial `#[test]` with sequential phases — parallel sidecar tests in
//! one binary would race the shared ring + listener. Everything is DISARMED (`set_armed(false)`) and
//! isolated to a private temp cwd, so no key/click/device write can occur and the repo's macros dir is
//! never touched. High-volume completions are counted through the UNBOUNDED beacon channel
//! (`neuron.notify`), not the 400-line log ring, so the ring's cap can't lose a completion.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::macros::macro_host::FIRE_BUDGET;
use neuron::macros::{macro_host, BeaconEvent, Context, MacroHost};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// BOUND effects hit the host arm gate before any platform-specific input adapter.
const KEY_NO_OP: &str = "[disarmed]";

/// The sidecar's per-macro queue cap (`_FIRE_QUEUE_MAX` in `neuron_host.py`). Mirror it here; if you
/// change one, change both — these tests assert exact cap behaviour against it.
const QMAX: usize = 256;

/// The hold-or-notify macro every queue phase uses. A fire whose `ctx.app` starts with "HOLD" BLOCKS
/// its worker on a beacon `ask` (held until the test answers it) — that's how we freeze a worker so
/// the queue fills deterministically. Any other fire just notifies its sequence tag + the sidecar pid
/// (so completions ride the unbounded beacon channel and prove the same warm process served them).
const HOLDFIRE: &str = r#"# neuron: raw
import os
def macro(ctx):
    a = ctx.app or ""
    if a.startswith("HOLD"):
        ask("hold", timeout=60)
        notify("hold pid=%d" % os.getpid())
    else:
        notify("done %s pid=%d" % (a, os.getpid()))
"#;

// ── helpers ─────────────────────────────────────────────────────────────────────────────────────

/// First run of ASCII digits after `key` in `s`, parsed (e.g. `num_after("a pid=42 b", "pid=") == 42`).
fn num_after(s: &str, key: &str) -> Option<u64> {
    s.split(key)
        .nth(1)
        .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
        .filter(|d| !d.is_empty())
        .and_then(|d| d.parse().ok())
}

/// Receive the next `Ask` (skipping Notify/Retire noise) as (pid, macro_id), or panic on timeout.
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

/// Receive the next `RetireDomain`, returning whether it arrived before `dur`.
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

/// Drain `Notify` events into (macro_id, text) pairs until `settle` passes with no new event (or the
/// `max` budget expires). Bursty completions with gaps < `settle` keep it collecting; a quiet gap
/// ends it. Non-Notify events are ignored.
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

/// Drain the macro-log ring repeatedly until `settle` passes with no new lines (or `max` expires).
/// Decouples collection from the bounded ring's eviction for moderate volumes (drop lines).
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

/// Fire `id` `n` times rapidly with `app = "seq=NNNN"` (the queue-filling burst).
fn burst(host: &MacroHost, id: &str, n: usize) {
    for i in 0..n {
        let c = Context::synthetic(Some(format!("seq={i:04}")), None, None, None, None);
        let _ = host.fire_async(id, &c);
    }
}

/// A synthetic disarmed context tagged with `app`.
fn cx(app: &str) -> Context {
    Context::synthetic(Some(app.to_string()), None, None, None, None)
}

#[test]
fn fire_queue_stress_e2e() {
    // bundled python + host scripts materialize from the binary; isolate the macros dir into a temp
    // cwd so we never touch the repo's.
    let tmp = std::env::temp_dir().join(format!("neuron_fq_stress_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping fire-queue stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false); // DISARMED: no key/click/device write can occur anywhere below.
    let _ = host.ensure_warm();

    // ── PHASE A: QUEUE OVERFLOW — bounded cap, DROP-NEWEST, no respawn, recovery ─────────────────
    // Hold the worker on a beacon, burst N > 256 fires behind it: exactly the OLDEST 256 must survive
    // (in enqueue order) and the newest N-256 must be dropped + logged, all on the SAME warm sidecar.
    // (The spec's "257 fires -> 1 drop" undercounts by one: the in-flight worker slot is separate from
    // the 256-deep queue, so a single fire is held in the worker while 256 more buffer. We assert the
    // real, deterministic cap: with the worker frozen, 256 buffer and N-256 drop.)
    {
        const N: usize = 300;
        host.register("fqa", HOLDFIRE).expect("register fqa");
        let rx = host.beacon_events();
        host.drain_log();

        assert!(host.fire_async("fqa", &cx("HOLD")).contains("dispatched"), "HOLD fire must dispatch warm");
        let (hold_pid, mid) = recv_ask(&rx, Duration::from_secs(15));
        assert_eq!(mid, "fqa", "the HOLD fire's worker is the one blocked on the beacon");

        burst(host, "fqa", N); // 256 buffer behind the frozen worker, 44 overflow
        // drops happen as the protocol loop processes the burst (worker frozen → nothing drains).
        let drop_lines = drain_log_for(host, Duration::from_millis(500), Duration::from_secs(8));
        let drops = drop_lines.iter().filter(|l| l.contains("fire queue full")).count();
        assert!(drops >= 1, "overflow must log at least one drop: {drop_lines:?}");
        assert_eq!(drops, N - QMAX, "with the worker frozen, exactly N-256 fires drop");
        assert!(
            drop_lines.iter().any(|l| l.contains("fire queue full (256)")),
            "the drop line must name the real cap (256): {drop_lines:?}"
        );

        host.answer(hold_pid, Some(0)); // release the worker → it drains the buffered 256 in order
        let notifs = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(30));

        let seqs: Vec<u64> = notifs
            .iter()
            .filter_map(|(_, t)| num_after(t, "seq="))
            .collect();
        let holds = notifs.iter().filter(|(_, t)| t.starts_with("hold")).count();
        assert_eq!(holds, 1, "the held fire itself completes exactly once");
        assert_eq!(seqs.len(), QMAX, "exactly the cap (256) of the buffered fires complete: got {}", seqs.len());
        assert_eq!(
            seqs,
            (0..QMAX as u64).collect::<Vec<_>>(),
            "the OLDEST 256 survive IN ORDER; the newest were the ones dropped (drop-newest)"
        );
        assert_eq!(seqs.len() + drops, N, "conservation: every fire either completed or was dropped");

        // no respawn: every completion reports the SAME sidecar pid.
        let pids: Vec<u64> = notifs.iter().filter_map(|(_, t)| num_after(t, "pid=")).collect();
        assert!(!pids.is_empty());
        assert!(pids.iter().all(|p| *p == pids[0]), "queue overflow must NOT respawn the sidecar");

        // post-overflow the queue accepts a fresh fire on the next press.
        assert!(host.fire_async("fqa", &cx("post")).contains("dispatched"));
        let after = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(10));
        assert!(
            after.iter().any(|(_, t)| t.contains("done post") && num_after(t, "pid=") == Some(pids[0])),
            "a fire after the overflow completes on the same warm sidecar: {after:?}"
        );
        host.unregister("fqa");
    }

    // ── PHASE B: FIRE-STORM — three queues saturate INDEPENDENTLY, each in order ─────────────────
    // Freeze three different macros at once and overflow all three. No cross-macro blocking (all three
    // complete), each macro keeps its own order, and each logs its own drops.
    {
        const N: usize = 300;
        let ids = ["fqb1", "fqb2", "fqb3"];
        for id in ids {
            host.register(id, HOLDFIRE).unwrap_or_else(|e| panic!("register {id}: {e}"));
        }
        let rx = host.beacon_events();
        host.drain_log();

        // freeze all three workers (collect their three holds by macro id).
        for id in ids {
            assert!(host.fire_async(id, &cx("HOLD")).contains("dispatched"));
        }
        let mut holds: BTreeMap<String, u64> = BTreeMap::new();
        while holds.len() < ids.len() {
            let (pid, mid) = recv_ask(&rx, Duration::from_secs(15));
            holds.insert(mid, pid);
        }
        for id in ids {
            assert!(holds.contains_key(id), "macro {id} must be frozen on its own beacon");
        }

        for id in ids {
            burst(host, id, N);
        }
        let drop_lines = drain_log_for(host, Duration::from_millis(600), Duration::from_secs(10));
        for id in ids {
            let d = drop_lines.iter().filter(|l| l.contains("fire queue full") && l.contains(&format!("'{id}'"))).count();
            assert_eq!(d, N - QMAX, "macro {id} must drop exactly N-256 (independent cap): {drop_lines:?}");
        }

        for pid in holds.values() {
            host.answer(*pid, Some(0));
        }
        let notifs = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(45));

        // group completions per macro and prove each is the in-order oldest-256 + its single hold.
        let mut per: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut hold_seen: BTreeMap<String, usize> = BTreeMap::new();
        for (mid, text) in &notifs {
            if let Some(seq) = num_after(text, "seq=") {
                per.entry(mid.clone()).or_default().push(seq);
            } else if text.starts_with("hold") {
                *hold_seen.entry(mid.clone()).or_default() += 1;
            }
        }
        for id in ids {
            let seqs = per.get(id).cloned().unwrap_or_default();
            assert_eq!(seqs.len(), QMAX, "macro {id} completed exactly the cap (independence — not starved)");
            assert_eq!(seqs, (0..QMAX as u64).collect::<Vec<_>>(), "macro {id} kept strict per-macro order");
            assert_eq!(hold_seen.get(id).copied().unwrap_or(0), 1, "macro {id}'s held fire completed once");
        }
        for id in ids {
            host.unregister(id);
        }
    }

    // ── PHASE C: SIDECAR CRASH WITH A FULL QUEUE — drop, RetireDomain, respawn, NO zombie re-run ────
    // Fill one macro's queue, then crash the sidecar. The 256 buffered fires die WITH the process —
    // they must NOT be re-run on respawn. The crash surfaces RetireDomain; the respawned sidecar (new
    // pid) serves a fresh fire.
    {
        const N: usize = 300;
        host.register("fqc", HOLDFIRE).expect("register fqc");
        host.register("fqc_pid", "# neuron: raw\nimport os\ndef macro(ctx):\n    return 'pid=%d' % os.getpid()\n")
            .expect("register fqc_pid");
        host.register("fqc_boom", "# neuron: raw\nimport os\ndef macro(ctx):\n    os._exit(7)\n").expect("register fqc_boom");
        let rx = host.beacon_events();
        host.drain_log();

        let old_pid = neuron::prof::SIDECAR_PID.load(Ordering::Relaxed);
        assert!(host.fire_async("fqc", &cx("HOLD")).contains("dispatched"));
        let (_hold_pid, _) = recv_ask(&rx, Duration::from_secs(15)); // worker frozen
        burst(host, "fqc", N); // 256 buffered behind the frozen worker

        let _ = host.fire_async("fqc_boom", &cx("x")); // a DIFFERENT macro's worker hard-exits the process
        assert!(
            wait_retire_all(&rx, Duration::from_secs(15)),
            "a sidecar crash with a full queue must surface RetireDomain (every open prompt is void)"
        );

        // respawn: the next blocking call re-warms + re-registers; loop until the pid actually changes.
        let mut new_pid = old_pid;
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            let r = host.invoke("fqc_pid", &cx("p"));
            if let Some(p) = num_after(&r, "pid=") {
                if p as u32 != old_pid {
                    new_pid = p as u32;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        assert_ne!(new_pid, old_pid, "the crashed sidecar must respawn with a NEW pid");
        assert_ne!(new_pid, 0);

        // the 256 buffered fires must NOT have re-run: zero seq completions ever reach the beacon.
        let stray = drain_notifies(&rx, Duration::from_millis(800), Duration::from_secs(4));
        assert!(
            !stray.iter().any(|(_, t)| t.contains("seq=")),
            "in-flight queue must be DROPPED on crash, never re-run on respawn: {stray:?}"
        );

        // the respawned sidecar serves a fresh fire (new pid, completes).
        host.drain_log();
        let rx2 = host.beacon_events();
        assert!(host.fire_async("fqc", &cx("afterC")).contains("dispatched"));
        let after = drain_notifies(&rx2, Duration::from_millis(900), Duration::from_secs(15));
        assert!(
            after.iter().any(|(_, t)| t.contains("done afterC") && num_after(t, "pid=") == Some(new_pid as u64)),
            "the respawned sidecar serves a fresh fire on the new pid: {after:?}"
        );
        for id in ["fqc", "fqc_pid", "fqc_boom"] {
            host.unregister(id);
        }
    }

    // ── PHASE D: FIRE_BUDGET TIMEOUT under queue pressure — bounded wait, fire not stranded ──────
    // A blocking `invoke` enqueued behind a busy worker must time out at exactly FIRE_BUDGET (~2.5s) —
    // not earlier, not hung — and its fire is NOT lost: it completes later and lands in the log/beacon
    // (proving the timed-out waiter was cleaned from the pending map, not stranded). (The timeout is
    // independent of queue DEPTH; we add a few pending fires as representative pressure — filling all
    // 255 with sleeping fires would take minutes and prove nothing extra.)
    {
        host.register(
            "fqd",
            "# neuron: raw\nimport os, time\ndef macro(ctx):\n    time.sleep(1.2)\n    notify('slept %s pid=%d' % (ctx.app, os.getpid()))\n",
        )
        .expect("register fqd");
        let rx = host.beacon_events();
        host.drain_log();

        for tag in ["p0", "p1", "p2"] {
            assert!(host.fire_async("fqd", &cx(tag)).contains("dispatched")); // ~3.6s of work queued
        }
        let t0 = Instant::now();
        let r = host.invoke("fqd", &cx("invk")); // enqueues 4th; waits FIRE_BUDGET then gives up
        let waited = t0.elapsed();
        assert!(r.contains("still running"), "invoke behind a busy queue must report still-running, not error: {r}");
        assert!(
            waited >= FIRE_BUDGET - Duration::from_millis(250),
            "invoke must wait its full budget, not return early ({waited:?} < {FIRE_BUDGET:?})"
        );
        assert!(
            waited <= FIRE_BUDGET + Duration::from_millis(1200),
            "invoke must give up at the budget, not hang on the busy queue ({waited:?})"
        );

        // all four fires (3 primed + the invoke's own) complete — none stranded by the timeout.
        let notifs = drain_notifies(&rx, Duration::from_millis(2200), Duration::from_secs(20));
        let mut tags: Vec<String> = notifs.iter().filter_map(|(_, t)| t.split("slept ").nth(1).and_then(|s| s.split(' ').next()).map(str::to_string)).collect();
        tags.sort();
        assert_eq!(tags, vec!["invk", "p0", "p1", "p2"], "every queued fire completes after the timeout (invk not stranded): {notifs:?}");
        let pids: Vec<u64> = notifs.iter().filter_map(|(_, t)| num_after(t, "pid=")).collect();
        assert!(pids.iter().all(|p| *p == pids[0]), "the budget timeout must NOT respawn the sidecar");
        host.unregister("fqd");
    }

    // ── PHASE E: MOCK ↔ REAL interleaved on ONE queue ───────────────────────────────────────────
    // A mock fire and a (disarmed) real fire of the same macro queue together. Both raise their REAL
    // beacon (ask reaches the listener) and BOTH suppress input (the real fire because the whole test
    // is disarmed; the mock fire because `mock` forces it) — proving the per-fire mock flag is honoured
    // per worker with no cross-bleed. (We keep the whole test disarmed: arming to distinguish mock from
    // a live real fire would synthesize an actual keystroke — forbidden by the non-destructive mandate.
    // So we assert the safe observable: beacon up on both, input no-op on both, results not crossed.)
    {
        host.register(
            "fqe",
            "def macro(ctx):\n    r = ask('e?', timeout=15)\n    k = neuron.key('a')\n    notify('e app=%s ask=%r key=%r' % (ctx.app, r, k))\n",
        )
        .expect("register fqe");
        let rx = host.beacon_events();
        host.drain_log();

        assert!(host.fire_async("fqe", &cx("real")).contains("dispatched"));
        assert!(host.fire_mock("fqe", &cx("mock")).contains("dispatched"));
        // ONE unified loop: answer both asks (mock raises the REAL beacon) AND collect both notifies.
        // (A separate recv_ask pass would consume — and discard — the first fire's Notify that arrives
        // while waiting for the second fire's Ask, since the two fires share one serial worker.)
        let mut notifs: Vec<(String, String)> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while notifs.len() < 2 && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(BeaconEvent::Ask { pid, macro_id, .. }) => {
                    assert_eq!(macro_id, "fqe");
                    host.answer(pid, Some(0));
                }
                Ok(BeaconEvent::Notify { macro_id, text }) => notifs.push((macro_id, text)),
                Ok(_) => {}
                Err(_) => {}
            }
        }
        let find = |app: &str| notifs.iter().find(|(_, t)| t.contains(&format!("app={app} "))).map(|(_, t)| t.clone());
        let real = find("real").expect("the real fire completed");
        let mock = find("mock").expect("the mock fire completed");
        assert!(real.contains("ask=True"), "real fire's beacon answered: {real}");
        assert!(mock.contains("ask=True"), "mock fire raises the REAL beacon and is answerable: {mock}");
        let no_op = format!("key='{KEY_NO_OP}'");
        assert!(real.contains(&no_op), "real fire is disarmed → input no-op (non-destructive): {real}");
        assert!(mock.contains(&no_op), "mock fire forces input no-op for that fire: {mock}");
        host.unregister("fqe");
    }

    // ── PHASE F: QUEUE RECOVERY after overflow — depth resets, fresh fires in order, no new drops ─
    {
        const N: usize = 300;
        host.register("fqf", HOLDFIRE).expect("register fqf");
        let rx = host.beacon_events();
        host.drain_log();

        // overflow once.
        assert!(host.fire_async("fqf", &cx("HOLD")).contains("dispatched"));
        let (hold_pid, _) = recv_ask(&rx, Duration::from_secs(15));
        burst(host, "fqf", N);
        let drops = drain_log_for(host, Duration::from_millis(500), Duration::from_secs(8))
            .iter()
            .filter(|l| l.contains("fire queue full"))
            .count();
        assert!(drops >= 1, "the overflow phase must drop");
        host.answer(hold_pid, Some(0));
        let _ = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(30)); // let it fully drain

        // now the drained queue must accept fresh fires cleanly — in order, with NO new drops.
        host.drain_log();
        for i in 0..10 {
            assert!(host.fire_async("fqf", &cx(&format!("r{i}"))).contains("dispatched"));
        }
        let after = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(15));
        let order: Vec<String> = after
            .iter()
            .filter_map(|(_, t)| t.split("done ").nth(1).and_then(|s| s.split(' ').next()).map(str::to_string))
            .filter(|s| s.starts_with('r'))
            .collect();
        assert_eq!(
            order,
            (0..10).map(|i| format!("r{i}")).collect::<Vec<_>>(),
            "post-overflow fires execute in order on a recovered queue: {order:?}"
        );
        let post_drops = drain_log_for(host, Duration::from_millis(300), Duration::from_secs(2))
            .iter()
            .filter(|l| l.contains("fire queue full"))
            .count();
        assert_eq!(post_drops, 0, "a recovered queue (depth reset) must not drop the 10 fresh fires");
        host.unregister("fqf");
    }

    // ── PHASE G: PER-THREAD STDOUT ISOLATION under concurrent fires ──────────────────────────────
    // Five macros on five workers each print 50 tagged lines AT ONCE. Each fire's stdout is captured to
    // its OWN per-thread buffer (`_ThreadStdout`), so the lines never interleave: every fire's 50 lines
    // appear as one CONTIGUOUS, in-order block in the log (the reader pushes each result's captured
    // output atomically). 5×50 = 250 lines stays under the 400-line ring.
    {
        let ids = ["fqg0", "fqg1", "fqg2", "fqg3", "fqg4"];
        let tags = ["G0", "G1", "G2", "G3", "G4"];
        for id in ids {
            host.register(
                id,
                "def macro(ctx):\n    for i in range(50):\n        print('ISO%s-%02d' % (ctx.app, i))\n",
            )
            .unwrap_or_else(|e| panic!("register {id}: {e}"));
        }
        host.drain_log();
        for (id, tag) in ids.iter().zip(tags.iter()) {
            assert!(host.fire_async(id, &cx(tag)).contains("dispatched"));
        }
        let log = drain_log_for(host, Duration::from_millis(900), Duration::from_secs(20));
        // keep only the ISO lines, preserving the ring's order.
        let iso: Vec<&String> = log.iter().filter(|l| l.contains("ISO")).collect();
        for tag in tags {
            let needle = format!("ISO{tag}-");
            let positions: Vec<usize> = iso.iter().enumerate().filter(|(_, l)| l.contains(&needle)).map(|(i, _)| i).collect();
            assert_eq!(positions.len(), 50, "macro {tag} must contribute all 50 captured lines: {}", positions.len());
            // CONTIGUITY: the 50 lines occupy a solid run (no other macro's line spliced in).
            assert_eq!(
                positions.last().unwrap() - positions.first().unwrap(),
                49,
                "macro {tag}'s 50 lines must be CONTIGUOUS (per-thread capture, no interleave)"
            );
            // ORDER: 00,01,…,49 within the block.
            let nums: Vec<u64> = (*positions.first().unwrap()..=*positions.last().unwrap())
                .filter_map(|i| num_after(iso[i], &needle))
                .collect();
            assert_eq!(nums, (0..50).collect::<Vec<_>>(), "macro {tag}'s captured lines must be in order");
        }
        for id in ids {
            host.unregister(id);
        }
    }

    // ── PHASE H: NO COLLAPSE-TO-ONE — a 1000-fire storm yields the bounded cap, not a magic merge ─
    // The spec floated a "collapse redundant fires to ONE pending" optimization. The code does NOT do
    // that: each fire is a distinct queue slot, capped at 256, newest dropped. Prove the REAL behaviour
    // by firing 1000 behind a frozen worker — exactly 256 (the oldest) complete; if a collapse existed,
    // only ~1–3 would. (Reported as a design note, not a bug: bounded drop-newest is the contract.)
    {
        const N: usize = 1000;
        host.register("fqh", HOLDFIRE).expect("register fqh");
        let rx = host.beacon_events();
        host.drain_log();

        assert!(host.fire_async("fqh", &cx("HOLD")).contains("dispatched"));
        let (hold_pid, _) = recv_ask(&rx, Duration::from_secs(15));
        burst(host, "fqh", N);
        // drops far exceed the 400-line ring, so we only assert the overflow HAPPENED (not the exact count).
        let drops = drain_log_for(host, Duration::from_millis(600), Duration::from_secs(10))
            .iter()
            .filter(|l| l.contains("fire queue full"))
            .count();
        assert!(drops >= 1, "a 1000-fire storm must overflow the 256 cap");

        host.answer(hold_pid, Some(0));
        let notifs = drain_notifies(&rx, Duration::from_millis(900), Duration::from_secs(45));
        let seqs: Vec<u64> = notifs.iter().filter_map(|(_, t)| num_after(t, "seq=")).collect();
        assert_eq!(
            seqs.len(),
            QMAX,
            "NO collapse-to-one: a huge storm completes exactly the bounded cap (256), not a merged handful"
        );
        assert_eq!(
            seqs,
            (0..QMAX as u64).collect::<Vec<_>>(),
            "the surviving 256 are the OLDEST, in order (drop-newest holds at scale)"
        );
        host.unregister("fqh");
    }

    // ── teardown ────────────────────────────────────────────────────────────────────────────────
    host.drain_log();
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
