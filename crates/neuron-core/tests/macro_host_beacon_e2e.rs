//! End-to-end proof of the BEACON layer against the REAL python sidecar — the full prime→activate
//! protocol round trip, with no GUI and no human:
//!   * NO-UI HONESTY — with no beacon listener installed, a macro's `ask` is auto-dismissed and
//!     returns its `default` (a beacon never strands a macro because nobody is watching).
//!   * ASK ROUND TRIP — install a listener, answer the Ask event through `macro_host::answer`, and
//!     the blocking macro resumes with that verdict (True / False / default-on-dismiss).
//!   * CONCURRENCY — a macro blocked on its worker (sleep/ask) does NOT stall other macros
//!     (per-macro serial queues), and one macro's rapid fires execute IN ORDER.
//!   * NOTIFY — `neuron.notify` surfaces as a BeaconEvent + a macro-log line.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::macros::{macro_host, BeaconEvent, Context};
use std::time::{Duration, Instant};

#[test]
fn beacon_protocol_round_trips_without_a_gui() {
    // The interpreter + host scripts are BUNDLED in the binary and materialized on first use, so
    // there's nothing to point at — just isolate the macros dir into a private temp cwd.
    let tmp = std::env::temp_dir().join(format!("neuron_beacon_e2e_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping beacon e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false);
    let ctx = Context::synthetic(Some("e2e.exe".into()), None, None, None, None);

    // a macro that blocks on a beacon and reports the verdict verbatim.
    host.register(
        "e2e_ask",
        "def macro(ctx):\n    return 'answer=%r' % ask('go?', timeout=15)\n",
    )
    .expect("register ask macro");
    // same, but with a non-None default so a dismissal is distinguishable from 'no'.
    host.register(
        "e2e_ask_default",
        "def macro(ctx):\n    return 'answer=%r' % ask('sure?', default='maybe', timeout=15)\n",
    )
    .expect("register default-ask macro");

    // ── PHASE A: no listener installed -> auto-dismiss, default returned, and FAST ──
    let t0 = Instant::now();
    let r = host.invoke("e2e_ask", &ctx);
    assert!(
        r.contains("answer=None"),
        "no-UI ask must return its default (None): {r}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "auto-dismiss must be immediate, not a timeout wait ({:?})",
        t0.elapsed()
    );

    // ── PHASE B/C/D: a listener answers yes / no / dismiss ──
    let rx = host.beacon_events();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let answerer = std::thread::spawn(move || {
        // three scripted verdicts, in firing order: yes, no, dismiss. The answer wheel takes an
        // OPTION INDEX (Option<usize>): index 0 = "yes" (-> ask True), index 1 = "no" (-> ask False),
        // None = dismissed (-> the macro's default).
        for verdict in [Some(0usize), Some(1usize), None] {
            loop {
                match rx.recv_timeout(Duration::from_secs(10)) {
                    Ok(BeaconEvent::Ask {
                        pid,
                        macro_id,
                        text,
                        timeout_ms,
                        ..
                    }) => {
                        seen_tx.send((macro_id, text, timeout_ms)).unwrap();
                        macro_host().answer(pid, verdict);
                        break;
                    }
                    Ok(_) => continue, // retire/notify noise is fine
                    Err(e) => panic!("no Ask event arrived: {e}"),
                }
            }
        }
        rx // keep the receiver alive until all three rounds are done
    });

    let r = host.invoke("e2e_ask", &ctx);
    assert!(
        r.contains("answer=True"),
        "an answered YES must reach the macro: {r}"
    );
    let r = host.invoke("e2e_ask", &ctx);
    assert!(
        r.contains("answer=False"),
        "an answered NO must reach the macro: {r}"
    );
    let r = host.invoke("e2e_ask_default", &ctx);
    assert!(
        r.contains("answer='maybe'"),
        "a dismissal must return the macro's default: {r}"
    );
    let rx = answerer.join().expect("answerer thread");

    // the Ask events carried the right identity + question + timeout.
    let (mid, text, timeout_ms) = seen_rx.recv().unwrap();
    assert_eq!(mid, "e2e_ask");
    assert_eq!(text, "go?");
    assert_eq!(
        timeout_ms,
        Some(15_000),
        "the macro's timeout must ride the event"
    );
    let _ = seen_rx.recv().unwrap();
    let (mid, text, _) = seen_rx.recv().unwrap();
    assert_eq!(mid, "e2e_ask_default");
    assert_eq!(text, "sure?");

    // ── PHASE E: notify surfaces as an event + a log line ──
    host.register(
        "e2e_notify",
        "def macro(ctx):\n    notify('primed and ready')\n    return 'ok'\n",
    )
    .unwrap();
    let r = host.invoke("e2e_notify", &ctx);
    assert!(r.contains("ok"), "notify macro must complete: {r}");
    let ev = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("a Notify event");
    match ev {
        BeaconEvent::Notify { macro_id, text } => {
            assert_eq!(macro_id, "e2e_notify");
            assert_eq!(text, "primed and ready");
        }
        other => panic!("expected Notify, got {other:?}"),
    }
    assert!(
        host.drain_log()
            .iter()
            .any(|l| l.contains("primed and ready")),
        "notify must also land in the macro log"
    );

    // ── PHASE F: a blocked macro stalls NOBODY (per-macro workers) ──
    host.register(
        "e2e_slow",
        "def macro(ctx):\n    import time\n    time.sleep(0.6)\n    print('slow finished')\n    return 'slow done'\n",
    )
    .unwrap();
    host.register("e2e_fast", "def macro(ctx):\n    return 'fast done'\n")
        .unwrap();
    let kicked = host.fire_async("e2e_slow", &ctx);
    assert!(
        kicked.contains("dispatched"),
        "slow fire must dispatch warm: {kicked}"
    );
    let t0 = Instant::now();
    let r = host.invoke("e2e_fast", &ctx);
    assert!(r.contains("fast done"), "fast macro result: {r}");
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "a busy macro must not stall another macro's fire ({:?})",
        t0.elapsed()
    );
    std::thread::sleep(Duration::from_millis(900)); // let the async slow fire finish + log
    assert!(
        host.drain_log().iter().any(|l| l.contains("slow finished")),
        "the async fire's print must land in the macro log when it completes"
    );

    // ── PHASE G: rapid fires of ONE macro execute in order (serial per-macro queue) ──
    host.register(
        "e2e_order",
        "def macro(ctx):\n    print('saw %s' % ctx.app)\n",
    )
    .unwrap();
    for n in 1..=3 {
        let c = Context::synthetic(Some(format!("app{n}.exe")), None, None, None, None);
        let kicked = host.fire_async("e2e_order", &c);
        assert!(kicked.contains("dispatched"), "order fire {n}: {kicked}");
    }
    std::thread::sleep(Duration::from_millis(600));
    let log = host.drain_log();
    let seq: Vec<&String> = log.iter().filter(|l| l.contains("saw app")).collect();
    assert_eq!(seq.len(), 3, "all three fires must have run: {log:?}");
    assert!(
        seq[0].contains("app1") && seq[1].contains("app2") && seq[2].contains("app3"),
        "one macro's rapid fires must execute IN ORDER: {seq:?}"
    );

    // ── PHASE H: MANY BEACONS AT ONCE (the power-user load) ──
    // Two different macros ask CONCURRENTLY: both prompts must be outstanding simultaneously
    // (collected before either is answered), each answer must reach ITS asker, and neither
    // blocks the other. Then one macro re-fired rapidly: its asks must arrive strictly one at a
    // time (serial per-macro), each resuming on its own answer — no deadlock, no cross-talk.
    host.register(
        "e2e_ask_a",
        "def macro(ctx):\n    print('A got %r' % ask('question A', timeout=15))\n",
    )
    .unwrap();
    host.register(
        "e2e_ask_b",
        "def macro(ctx):\n    print('B got %r' % ask('question B', timeout=15))\n",
    )
    .unwrap();
    let rx = host.beacon_events(); // fresh listener (replaces the phase-E one)
    host.drain_log();

    // both fires dispatch async; the asks block only their own workers.
    assert!(host.fire_async("e2e_ask_a", &ctx).contains("dispatched"));
    assert!(host.fire_async("e2e_ask_b", &ctx).contains("dispatched"));
    // collect BOTH prompts before answering either — proves simultaneous outstanding asks.
    let mut open: Vec<(u64, String)> = Vec::new();
    while open.len() < 2 {
        if let BeaconEvent::Ask { pid, text, .. } = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("both asks must arrive")
        {
            open.push((pid, text));
        }
    }
    assert!(
        open.iter().any(|(_, t)| t == "question A") && open.iter().any(|(_, t)| t == "question B"),
        "both macros' prompts must be open at once: {open:?}"
    );
    for (pid, text) in &open {
        host.answer(*pid, Some(if text == "question A" { 0 } else { 1 })); // A -> yes(0), B -> no(1)
    }
    std::thread::sleep(Duration::from_millis(600));
    let log = host.drain_log();
    assert!(
        log.iter().any(|l| l.contains("A got True")),
        "A's answer must reach A: {log:?}"
    );
    assert!(
        log.iter().any(|l| l.contains("B got False")),
        "B's answer must reach B: {log:?}"
    );

    // rapid re-fire of ONE asking macro: asks arrive serially, each fire gets its own verdict.
    for _ in 0..3 {
        assert!(host.fire_async("e2e_ask_a", &ctx).contains("dispatched"));
    }
    for round in 0..3 {
        // exactly one prompt may be open at a time (serial per-macro queue).
        let pid = loop {
            if let BeaconEvent::Ask { pid, text, .. } = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("serial ask must arrive")
            {
                assert_eq!(text, "question A");
                break pid;
            }
        };
        // no second prompt while this one is unanswered (the next fire is queued behind it).
        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "round {round}: a second ask leaked out while one was still open (not serial)"
        );
        host.answer(pid, Some(1usize)); // "no"
    }
    std::thread::sleep(Duration::from_millis(600));
    let log = host.drain_log();
    let resumed = log.iter().filter(|l| l.contains("A got False")).count();
    assert_eq!(
        resumed, 3,
        "every queued fire must resume on its own answer: {log:?}"
    );

    for id in [
        "e2e_ask",
        "e2e_ask_default",
        "e2e_notify",
        "e2e_slow",
        "e2e_fast",
        "e2e_order",
        "e2e_ask_a",
        "e2e_ask_b",
    ] {
        host.unregister(id);
    }
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
