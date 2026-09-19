// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! NON-DESTRUCTIVE stress tests for the BEACON dimension of the macro system — the two-part
//! "prime, then activate" surface (`neuron.ask`/`choose`/`confirm`) and the radial answer-wheel
//! geometry that the GUI uses to turn a flick into an option index.
//!
//! Two tiers live here:
//!   * PURE RADIAL MATH — `pick_wedge`/`wedge_bearing`/`wedge_arc`/`intent_vector` are pure functions
//!     (no sidecar, no IO), so each is its own fast `#[test]`. They prove the yes/no/pass rule, the
//!     N-way fan-out, the single-option confirm, jitter tolerance at the commit radius, and that the
//!     intent vector reads where the hand MEANT to end — then feed that into the wheel.
//!   * SIDECAR E2E — everything that talks to the real python sidecar runs in ONE big `#[test]`
//!     (`beacon_stress_e2e`) with sequential phases. The Macro Host is a process-global singleton and
//!     the cwd / beacon-listener slot are process-global too, so a single serial test is the only way
//!     to keep phases from racing each other (this mirrors `macro_host_beacon_e2e.rs`'s one-big-test
//!     idiom). Everything is DISARMED (`set_armed(false)`) and isolated to a private temp cwd, so no
//!     key/click/device write can occur and the repo's macros dir is never touched.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::glyph::C;
use neuron::macros::{macro_host, BeaconEvent, Context};
use neuron::radial::{
    intent_vector, max_sectors, net_displacement, pick_wedge, sector_for, wedge_arc, wedge_bearing,
    FLICK_JITTER,
};
use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// BOUND effects hit the host arm gate before any platform-specific input adapter.
const KEY_NO_OP: &str = "[disarmed]";

// ─────────────────────────────────────────────────────────────────────────────────────────────
// PURE RADIAL MATH — the answer wheel's geometry (no sidecar; each is independent + parallel-safe)
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// A synthetic mouse flick: `steps`+1 cumulative points from the origin out to `dist` along the
/// (ux,uy) direction — the shape the capture layer hands the wheel. `net_displacement` reduces it
/// back to the release offset the wheel actually buckets.
fn flick_path(ux: f64, uy: f64, dist: f64, steps: usize) -> Vec<C> {
    let len = (ux * ux + uy * uy).sqrt().max(1e-9);
    let (ux, uy) = (ux / len, uy / len);
    (0..=steps)
        .map(|i| {
            let t = i as f64 / steps as f64 * dist;
            C {
                re: ux * t,
                im: uy * t,
            }
        })
        .collect()
}

#[test]
fn n2_yes_no_radial_math_byte_for_byte() {
    // THE legacy rule, reproduced exactly by the N=2 prompt wheel: option 0 anchors WEST (yes),
    // option 1 EAST (no); the whole vertical axis (and the centre deadzone) is PASS.
    let dz = 40.0;
    assert_eq!(pick_wedge(-100.0, 0.0, dz, 2), Some(0), "west = yes(0)");
    assert_eq!(pick_wedge(100.0, 0.0, dz, 2), Some(1), "east = no(1)");
    assert_eq!(pick_wedge(0.0, -100.0, dz, 2), None, "north = pass");
    assert_eq!(pick_wedge(0.0, 100.0, dz, 2), None, "south = pass");
    // a VERTICAL-dominant diagonal passes; a HORIZONTAL-dominant one commits (the 90° cap == the
    // legacy |dx|>|dy| quadrant rule). (A clean 45° lands ON the east arc edge — inclusive — so we
    // assert with dominant diagonals, never the exact boundary.)
    assert_eq!(pick_wedge(-100.0, -60.0, dz, 2), Some(0), "WNW horizontal-dominant = yes");
    assert_eq!(pick_wedge(100.0, -60.0, dz, 2), Some(1), "ENE horizontal-dominant = no");
    assert_eq!(pick_wedge(-60.0, -100.0, dz, 2), None, "NNW vertical-dominant = pass");
    assert_eq!(pick_wedge(60.0, 100.0, dz, 2), None, "SSE vertical-dominant = pass");
    assert_eq!(pick_wedge(6.0, -2.0, dz, 2), None, "under deadzone = pass");

    // the SAME rule, but through the real pipeline a flick takes: path -> net_displacement -> wheel.
    for (ux, uy, want, lbl) in [
        (-1.0, 0.0, Some(0usize), "west flick path -> yes"),
        (1.0, 0.0, Some(1usize), "east flick path -> no"),
        (0.0, -1.0, None, "north flick path -> pass"),
        (0.0, 1.0, None, "south flick path -> pass"),
    ] {
        let (dx, dy) = net_displacement(&flick_path(ux, uy, 100.0, 30));
        assert_eq!(pick_wedge(dx, dy, dz, 2), want, "{lbl}");
    }
}

#[test]
fn n345_fan_out_choose_wedges() {
    // A `choose` of N options fans the wheel: each wedge CENTRE picks its own distinct index, and a
    // gap between wedges (where N is small enough that the 90° arc cap leaves room) PASSES.
    let dz = 40.0;
    for n in [3usize, 4, 5] {
        let mut seen = HashSet::new();
        for i in 0..n {
            let b = wedge_bearing(i, n);
            let got = pick_wedge(b.cos() * 100.0, b.sin() * 100.0, dz, n);
            assert_eq!(got, Some(i), "N={n}: wedge {i} centre must pick option {i}");
            seen.insert(got);
        }
        assert_eq!(seen.len(), n, "N={n}: every wedge centre picks a DISTINCT option");
    }
    // N=3: arcs are capped at 90°, so 3·90 = 270° < 360° leaves three pass-gaps. 120° lands in the
    // gap between wedge 1 (60°) and wedge 0 (180°).
    let gap = 2.0 * std::f64::consts::PI / 3.0; // 120°
    assert_eq!(
        pick_wedge(gap.cos() * 100.0, gap.sin() * 100.0, dz, 3),
        None,
        "N=3: a release in the gap between wedges passes"
    );
    // the ceiling stays honest — the default 40-count deadzone carries exactly 16 reliable wedges.
    assert_eq!(max_sectors(dz), 16, "max_sectors is derived from the deadzone, not hand-capped");
}

#[test]
fn single_option_confirm_wheel() {
    // A 1-option prompt (a `confirm`) owns the WHOLE circle: any committed flick confirms it; only a
    // release under the deadzone passes. No arbitrary "flick west to confirm".
    use std::f64::consts::TAU;
    let dz = 40.0;
    assert_eq!(wedge_arc(1), TAU, "a single option owns the whole circle (arc == TAU)");
    for (dx, dy, lbl) in [
        (-100.0, 0.0, "west"),
        (100.0, 0.0, "east"),
        (0.0, -100.0, "north"),
        (0.0, 100.0, "south"),
        (70.0, 70.0, "SE diagonal"),
        (-70.0, -70.0, "NW diagonal"),
    ] {
        assert_eq!(pick_wedge(dx, dy, dz, 1), Some(0), "{lbl} committed flick confirms the lone option");
    }
    assert_eq!(pick_wedge(6.0, -2.0, dz, 1), None, "under-deadzone still passes a confirm");
}

#[test]
fn wedge_jitter_tolerance_at_commit_radius() {
    // The FLICK_JITTER constant (±15 counts of lateral hand noise at the ~40-count commit radius) is
    // exactly what `max_sectors` is derived from. Prove it: a flick aimed at a wedge centre, landed
    // at the commit radius with ±FLICK_JITTER of lateral wobble, still resolves to THAT wedge — never
    // a neighbour — for wheel sizes well inside the honest ceiling.
    let dz = 40.0;
    let r = dz; // the commit radius the jitter budget is calibrated at
    for n in [4usize, 8] {
        for i in 0..n {
            let b = wedge_bearing(i, n);
            let (cx, cy) = (b.cos(), b.sin()); // along the wedge bearing
            let (px, py) = (-cy, cx); // the unit perpendicular — pure lateral jitter
            for &j in &[FLICK_JITTER, -FLICK_JITTER] {
                let (dx, dy) = (cx * r + px * j, cy * r + py * j);
                assert_eq!(
                    pick_wedge(dx, dy, dz, n),
                    Some(i),
                    "N={n} wedge {i}: a commit-radius flick with {j:+} lateral jitter still picks {i}"
                );
            }
        }
    }
}

#[test]
fn intent_vector_survives_complex_strokes_into_wedges() {
    // The wheel reads the stroke's INTENT (attention-weighted, recency-dominant), not its raw
    // geometry — so a changed mind, a circling approach, and a wobbling flick all resolve to where
    // the hand MEANT to end. Each intent is then fed straight into the answer wheel.
    let dz = 40.0;

    // CHANGED MIND: far left… no, RIGHT. The hand meant east.
    let mut changed: Vec<C> = (0..40).map(|i| C { re: -f64::from(i) * 6.0, im: 0.0 }).collect();
    changed.extend((0..80).map(|i| C { re: -240.0 + f64::from(i) * 6.0, im: 0.0 }));
    let (ix, iy) = intent_vector(&changed);
    assert!(ix > 0.0, "changed-mind intent points right (ix={ix})");
    assert_eq!(pick_wedge(ix, iy, dz, 2), Some(1), "changed-mind -> N=2 wheel reads NO (east)");
    assert_eq!(sector_for(ix, iy, 8), 2, "changed-mind -> 8-way wheel reads EAST");

    // CIRCLING approach that EXITS straight up — the exit flick dominates -> north.
    let mut circ: Vec<C> = (0..60)
        .map(|i| {
            let a = f64::from(i) / 60.0 * std::f64::consts::TAU;
            C { re: 60.0 * a.sin(), im: 60.0 * (1.0 - a.cos()) }
        })
        .collect();
    circ.extend((0..50).map(|i| C { re: 0.0, im: -f64::from(i) * 5.0 }));
    let (ix, iy) = intent_vector(&circ);
    assert!(iy < 0.0, "circle-exit intent points up (iy={iy})");
    // N=4 choose wheel: west=0, south=1, east=2, NORTH=3.
    assert_eq!(pick_wedge(ix, iy, dz, 4), Some(3), "circle-exit-up -> N=4 wheel picks the north option");

    // WOBBLE: a rightward flick carrying lateral noise still reads as a clean east intent.
    let wobble: Vec<C> = (0..40)
        .map(|i| {
            let t = f64::from(i);
            C { re: t * 5.0, im: (t * 0.7).sin() * 8.0 }
        })
        .collect();
    let (ix, iy) = intent_vector(&wobble);
    assert!(ix > 0.0 && ix.abs() > iy.abs(), "wobble -> clean rightward intent (ix={ix}, iy={iy})");
    assert_eq!(pick_wedge(ix, iy, dz, 2), Some(1), "wobble flick -> NO (east) on the yes/no wheel");
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// SIDECAR E2E — one serial test, sequential phases (the global host + cwd + listener slot forbid
// parallel sidecar tests in one binary). DISARMED + temp-cwd isolated.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// All the fields of a `BeaconEvent::Ask`, for terse pattern-free assertions.
type AskInfo = (u64, String, String, Vec<String>, String, Option<u64>);

/// Receive the next `Ask` event (skipping Retire/Notify noise), or panic on timeout.
fn recv_ask(rx: &Receiver<BeaconEvent>, dur: Duration) -> AskInfo {
    let deadline = Instant::now() + dur;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(BeaconEvent::Ask { pid, macro_id, text, options, detail, timeout_ms }) => {
                return (pid, macro_id, text, options, detail, timeout_ms)
            }
            Ok(_) => continue,
            Err(_) => panic!("timed out waiting for an Ask event"),
        }
    }
}

/// Run `invoke(id)` on its own thread; the returned channel yields the macro's result string. Lets
/// the test thread present/answer the beacon while the (blocking) macro waits on it.
fn invoke_async(id: &str, ctx: &Context) -> Receiver<String> {
    let id = id.to_string();
    let ctx = ctx.clone();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let _ = tx.send(macro_host().invoke(&id, &ctx));
    });
    rx
}

/// Block for a macro result with a generous budget (the macro itself bounds its own ask timeouts).
fn result(rx: &Receiver<String>) -> String {
    rx.recv_timeout(Duration::from_secs(20))
        .expect("the invoked macro returned a result")
}

#[test]
fn beacon_stress_e2e() {
    // bundled python + host scripts materialize from the binary; isolate the macros dir into a temp
    // cwd so we never touch the repo's.
    let tmp = std::env::temp_dir().join(format!("neuron_beacon_stress_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping beacon stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false); // DISARMED: no key/click/device write can occur anywhere below.
    let ctx = Context::synthetic(Some("stress.exe".into()), None, None, None, None);

    // ── PHASE A: FAST AUTO-DISMISS WITHOUT A LISTENER ──────────────────────────────────────────
    // No beacon listener is installed (the slot starts empty). A macro's ask must auto-dismiss to its
    // default IMMEDIATELY (the reader answers it down the pipe), not hang until the timeout — and the
    // dismissal must be logged. This is the no-UI honesty guarantee, under a tight time bound.
    host.register("bs_auto", "def macro(ctx):\n    return 'a=%r' % ask('auto?', timeout=15)\n")
        .expect("register bs_auto");
    let t0 = Instant::now();
    let r = host.invoke("bs_auto", &ctx);
    assert!(r.contains("a=None"), "no-UI ask must return its default (None): {r}");
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "auto-dismiss must be immediate, not a timeout wait ({:?})",
        t0.elapsed()
    );
    assert!(
        host.drain_log().iter().any(|l| l.contains("dismissed")),
        "the no-UI dismissal must land in the macro log"
    );

    // ── PHASE B: DESCRIPTION + OPTIONS RIDE THE EVENT (a `choose`) ─────────────────────────────
    // A choose's labels, the description, and the question text must arrive on the BeaconEvent
    // verbatim — no truncation, no corruption — and answering the chosen index returns that option.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_choose",
            "def macro(ctx):\n    return 'c=%r' % choose('pick one', ['alpha','beta','gamma'], description='context text', timeout=15)\n",
        )
        .expect("register bs_choose");
        let res = invoke_async("bs_choose", &ctx);
        let (pid, mid, text, options, detail, _) = recv_ask(&rx, Duration::from_secs(10));
        assert_eq!(mid, "bs_choose");
        assert_eq!(text, "pick one", "the question rides the event verbatim");
        assert_eq!(options, vec!["alpha", "beta", "gamma"], "the wedge labels ride the event in order");
        assert_eq!(detail, "context text", "the description rides the event as `detail`");
        host.answer(pid, Some(1)); // beta
        assert!(result(&res).contains("c='beta'"), "answering index 1 returns option 'beta'");
    }

    // ── PHASE C: EMPTY OPTIONS — bare ask defaults to yes/no; choose([]) short-circuits ────────
    // A bare `ask` carries the two implicit wedges ["yes","no"]. A `choose` with an EMPTY option list
    // never even reaches the wheel: the python prompt core short-circuits to the default and emits NO
    // event (so the wheel is never asked to render zero wedges).
    {
        let rx = host.beacon_events();
        host.register("bs_bare", "def macro(ctx):\n    return 'b=%r' % ask('bare?', timeout=15)\n")
            .expect("register bs_bare");
        let res = invoke_async("bs_bare", &ctx);
        let (pid, _, _, options, _, _) = recv_ask(&rx, Duration::from_secs(10));
        assert_eq!(options, vec!["yes", "no"], "a bare ask carries the implicit yes/no wedges");
        host.answer(pid, Some(0));
        assert!(result(&res).contains("b=True"));

        host.register(
            "bs_empty",
            "def macro(ctx):\n    return 'e=%r' % choose('?', [], default='FELLBACK', timeout=15)\n",
        )
        .expect("register bs_empty");
        let res = invoke_async("bs_empty", &ctx);
        assert!(
            result(&res).contains("e='FELLBACK'"),
            "choose([]) returns its default immediately"
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(500)).is_err(),
            "choose([]) must emit NO beacon event (it never reaches the wheel)"
        );
    }

    // ── PHASE D: RADIAL FLICK DRIVES THE VERDICT END-TO-END ────────────────────────────────────
    // The GUI converts a flick to an option index with `pick_wedge`, then calls `answer`. Tie the two
    // together: synthesize a flick, run the SAME geometry the overlay runs, and feed its index to the
    // live beacon — the macro's verdict must match the flick's direction.
    {
        let rx = host.beacon_events();
        host.register("bs_n2", "def macro(ctx):\n    return 'v=%r' % ask('q', timeout=15)\n")
            .expect("register bs_n2");

        // a WEST flick -> pick_wedge -> Some(0) -> ask True
        let res = invoke_async("bs_n2", &ctx);
        let (pid, ..) = recv_ask(&rx, Duration::from_secs(10));
        let idx = pick_wedge(-100.0, 0.0, 40.0, 2);
        assert_eq!(idx, Some(0), "west flick resolves to option 0");
        host.answer(pid, idx);
        assert!(result(&res).contains("v=True"), "a west flick answers YES end-to-end");

        // a VERTICAL flick -> pick_wedge -> None (pass) -> ask default (None)
        let res = invoke_async("bs_n2", &ctx);
        let (pid, ..) = recv_ask(&rx, Duration::from_secs(10));
        let idx = pick_wedge(0.0, -100.0, 40.0, 2);
        assert_eq!(idx, None, "a vertical flick passes");
        host.answer(pid, idx);
        assert!(result(&res).contains("v=None"), "a vertical flick passes to the default end-to-end");

        // a 4-way choose: a SOUTH flick picks option 1, a NORTH flick picks option 3.
        host.register(
            "bs_n4",
            "def macro(ctx):\n    return 'v=%r' % choose('q', ['opt0','opt1','opt2','opt3'], timeout=15)\n",
        )
        .expect("register bs_n4");
        for (dx, dy, want_idx, want_opt) in [
            (0.0, 100.0, Some(1usize), "opt1"),  // south
            (0.0, -100.0, Some(3usize), "opt3"), // north
        ] {
            let res = invoke_async("bs_n4", &ctx);
            let (pid, ..) = recv_ask(&rx, Duration::from_secs(10));
            let idx = pick_wedge(dx, dy, 40.0, 4);
            assert_eq!(idx, want_idx, "N=4 flick ({dx},{dy}) resolves to {want_idx:?}");
            host.answer(pid, idx);
            assert!(result(&res).contains(want_opt), "N=4 flick commits {want_opt} end-to-end");
        }
    }

    // ── PHASE E: A TIMEOUT ACTUALLY RESOLVES THE PROMPT ────────────────────────────────────────
    // With a listener attached but DELIBERATELY NOT answering, the sidecar-side timeout must fire:
    // the macro returns its default well under the host's fire budget, and the host emits a Retire
    // for that prompt so any UI still showing it withdraws.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_timeout",
            "def macro(ctx):\n    return 't=%r' % ask('time?', default='TIMED_OUT', timeout=0.4)\n",
        )
        .expect("register bs_timeout");
        let t0 = Instant::now();
        let res = invoke_async("bs_timeout", &ctx);
        let (pid, ..) = recv_ask(&rx, Duration::from_secs(10)); // delivered, but we never answer
        // the sidecar times out at ~0.4s and tells us to retire this exact prompt.
        let mut retired = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(BeaconEvent::Retire { pid }) => {
                    retired = Some(pid);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert_eq!(retired, Some(pid), "the timeout must Retire exactly the timed-out prompt");
        let r = result(&res);
        assert!(r.contains("t='TIMED_OUT'"), "the timed-out ask returns its default: {r}");
        assert!(
            t0.elapsed() < Duration::from_millis(1500),
            "the macro resolved on its own short timeout, not the long fire budget ({:?})",
            t0.elapsed()
        );
    }

    // ── PHASE F: A LATE ANSWER (AFTER TIMEOUT) IS IGNORED ──────────────────────────────────────
    // Once a prompt has timed out and the macro has taken its default, an answer arriving for that
    // (now-expired) pid is silently dropped by the sidecar — no crosstalk, no second resolution.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_late",
            "def macro(ctx):\n    return 'L=%r' % ask('late?', default='DEFAULTED', timeout=0.4)\n",
        )
        .expect("register bs_late");
        let res = invoke_async("bs_late", &ctx);
        let (pid, ..) = recv_ask(&rx, Duration::from_secs(10));
        // wait for the timeout's Retire, THEN answer the dead pid.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(BeaconEvent::Retire { pid: p }) if p == pid => break,
                Ok(_) => continue,
                Err(_) => panic!("expected a Retire for the timed-out prompt"),
            }
        }
        let r = result(&res);
        assert!(r.contains("L='DEFAULTED'"), "the macro already took its default: {r}");
        host.answer(pid, Some(0)); // LATE — must be a no-op (the macro is long gone)
        // give the late frame time to (not) do anything; the macro can't change its returned value.
        std::thread::sleep(Duration::from_millis(150));
        assert!(r.contains("L='DEFAULTED'"), "a late answer must not change the verdict");
    }

    // ── PHASE G: AN OUT-OF-RANGE ANSWER INDEX IS TREATED AS A PASS ─────────────────────────────
    // A 3-option choose answered with index 5 (out of range) must fall through to the default — the
    // wrapper bounds-checks the index rather than indexing past the option list.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_oor",
            "def macro(ctx):\n    return 'o=%r' % choose('q', ['a','b','c'], default='OOR_DEFAULT', timeout=15)\n",
        )
        .expect("register bs_oor");
        let res = invoke_async("bs_oor", &ctx);
        let (pid, _, _, options, ..) = recv_ask(&rx, Duration::from_secs(10));
        assert_eq!(options.len(), 3);
        host.answer(pid, Some(5)); // out of range
        let r = result(&res);
        assert!(r.contains("o='OOR_DEFAULT'"), "an out-of-range index passes to the default: {r}");
    }

    // ── PHASE H: PID UNIQUENESS + WRONG-PID ROUTING ────────────────────────────────────────────
    // Ten serial fires of one asking macro each get a UNIQUE pid; answering a non-existent pid never
    // resolves the open ask (it keeps waiting), and only the correct pid resumes it.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_pid",
            "def macro(ctx):\n    print('pid round %r' % ask('p', timeout=15))\n",
        )
        .expect("register bs_pid");
        host.drain_log();
        for _ in 0..10 {
            assert!(host.fire_async("bs_pid", &ctx).contains("dispatched"));
        }
        let mut pids = HashSet::new();
        for round in 0..10 {
            let (pid, _, text, ..) = recv_ask(&rx, Duration::from_secs(10));
            assert_eq!(text, "p");
            assert!(pids.insert(pid), "pid {pid} repeated — pids must be unique");
            // every few rounds, prove a WRONG pid is ignored (the ask stays open, serial discipline
            // means no other ask can leak out while this one is unanswered).
            if round % 3 == 0 {
                host.answer(pid + 1_000_000, Some(0)); // a pid that was never issued
                assert!(
                    rx.recv_timeout(Duration::from_millis(250)).is_err(),
                    "a wrong-pid answer must not resolve the open ask (no event should leak)"
                );
            }
            host.answer(pid, Some(1)); // the right pid -> resume with False
        }
        assert_eq!(pids.len(), 10, "all ten asks had distinct pids");
        std::thread::sleep(Duration::from_millis(400));
        let log = host.drain_log();
        let resumed = log.iter().filter(|l| l.contains("pid round False")).count();
        assert_eq!(resumed, 10, "every fire resumed on ITS own correct-pid answer: {log:?}");
    }

    // ── PHASE I: RETIRE-FROM-THE-MIDDLE (per-pid Retire isolation) ─────────────────────────────
    // The core has no host-side "retire(pid)" — a single-pid Retire is produced by a sidecar-side
    // TIMEOUT. With two prompts open at once, the SHORT one timing out must Retire only ITS pid and
    // leave the other prompt fully answerable. (The presenter QUEUE that drops it from the middle
    // lives in neuron-app/beacon.rs; the core's contract under test here is the scoped Retire event.)
    {
        let rx = host.beacon_events();
        host.register(
            "bs_short",
            "def macro(ctx):\n    return 's=%r' % ask('short', default='SHORT_TO', timeout=0.5)\n",
        )
        .expect("register bs_short");
        host.register(
            "bs_long",
            "def macro(ctx):\n    return 'l=%r' % ask('long', timeout=15)\n",
        )
        .expect("register bs_long");
        let res_short = invoke_async("bs_short", &ctx);
        let res_long = invoke_async("bs_long", &ctx);
        // collect both open prompts (distinct macros run concurrently).
        let mut short_pid = None;
        let mut long_pid = None;
        while short_pid.is_none() || long_pid.is_none() {
            let (pid, _, text, ..) = recv_ask(&rx, Duration::from_secs(10));
            match text.as_str() {
                "short" => short_pid = Some(pid),
                "long" => long_pid = Some(pid),
                other => panic!("unexpected prompt {other:?}"),
            }
        }
        // the short prompt times out -> a Retire for EXACTLY its pid; the long one is untouched.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(BeaconEvent::Retire { pid }) => {
                    assert_eq!(Some(pid), short_pid, "only the SHORT prompt may retire");
                    break;
                }
                Ok(_) => continue,
                Err(_) => panic!("expected the short prompt to retire on timeout"),
            }
        }
        assert!(result(&res_short).contains("s='SHORT_TO'"), "short took its default on timeout");
        // the long prompt survived the neighbour's retire and is still answerable.
        host.answer(long_pid.unwrap(), Some(0));
        assert!(result(&res_long).contains("l=True"), "the surviving prompt answers normally");
    }

    // ── PHASE J: DROPPED RECEIVER -> RE-LISTEN ─────────────────────────────────────────────────
    // Listener 1 takes A1 and answers it. The receiver is then dropped; a fresh fire A2 finds a dead
    // slot and AUTO-DISMISSES (delivery fails -> the reader answers it down the pipe). A new listener
    // then cleanly takes A3. Proves the slot recovers across listener handoffs.
    {
        host.register("bs_relisten", "def macro(ctx):\n    return 'r=%r' % ask('q', timeout=15)\n")
            .expect("register bs_relisten");
        // A1 on listener 1
        let rx1 = host.beacon_events();
        let res1 = invoke_async("bs_relisten", &ctx);
        let (pid1, ..) = recv_ask(&rx1, Duration::from_secs(10));
        host.answer(pid1, Some(0));
        assert!(result(&res1).contains("r=True"), "A1 answered on listener 1");
        drop(rx1); // listener 1 goes away

        // A2 with the slot now holding a dead sender -> auto-dismiss, fast, to default.
        let t0 = Instant::now();
        let r2 = host.invoke("bs_relisten", &ctx);
        assert!(r2.contains("r=None"), "A2 auto-dismissed to default with no live listener: {r2}");
        assert!(t0.elapsed() < Duration::from_secs(2), "A2 dismissal was immediate");

        // A3 on a fresh listener 2 works normally.
        let rx2 = host.beacon_events();
        let res3 = invoke_async("bs_relisten", &ctx);
        let (pid3, ..) = recv_ask(&rx2, Duration::from_secs(10));
        host.answer(pid3, Some(1));
        assert!(result(&res3).contains("r=False"), "A3 reached the new listener");
    }

    // ── PHASE K: RECEIVER DROPPED MID-PROMPT (the honest behaviour) ────────────────────────────
    // NOTE ON THE ARCHITECTURE: the no-UI auto-answer fires only at DELIVERY time (when the slot has
    // no live receiver). Once an Ask has been delivered to a live listener that is THEN dropped, the
    // in-flight macro is NOT retroactively answered — it relies on its OWN timeout to unblock. We
    // assert that real behaviour (default via timeout, not an instant auto-answer), then show the
    // NEXT prompt (now listener-less) does auto-dismiss instantly.
    {
        let rx = host.beacon_events();
        host.register(
            "bs_middrop",
            "def macro(ctx):\n    return 'm=%r' % ask('mid', default='MID_TO', timeout=0.6)\n",
        )
        .expect("register bs_middrop");
        let t0 = Instant::now();
        let res = invoke_async("bs_middrop", &ctx);
        let _ = recv_ask(&rx, Duration::from_secs(10)); // delivered to this listener…
        drop(rx); // …which is then dropped mid-prompt
        let r = result(&res);
        assert!(r.contains("m='MID_TO'"), "an orphaned in-flight prompt unblocks via its own timeout: {r}");
        assert!(
            t0.elapsed() >= Duration::from_millis(450),
            "it waited out the timeout (NOT an instant auto-answer): {:?}",
            t0.elapsed()
        );
        // and the next prompt, with no listener at all, auto-dismisses immediately.
        let t1 = Instant::now();
        let r2 = host.invoke("bs_middrop", &ctx);
        assert!(r2.contains("m='MID_TO'"));
        assert!(t1.elapsed() < Duration::from_secs(2), "the listener-less prompt auto-dismissed fast");
    }

    // ── PHASE L: MOCK-FIRE RAISES THE BEACON BUT SUPPRESSES SIDE EFFECTS ───────────────────────
    // fire_mock runs the macro with effects forced off for that fire only — yet ask/notify still
    // reach the human. So the beacon must rise (and be answerable) while an input verb no-ops.
    // (We can only verify the non-destructive observable: the whole test is DISARMED, so we cannot
    // arm real input to distinguish mock from disarm for key synthesis — we verify the beacon rises
    // under mock and the device verb returns a suppressed marker, whichever one the platform has.)
    {
        let rx = host.beacon_events();
        host.register(
            "bs_mock",
            "def macro(ctx):\n    r = ask('mock?', timeout=15)\n    k = neuron.key('a')\n    print('mock ask=%r key=%r' % (r, k))\n",
        )
        .expect("register bs_mock");
        host.drain_log();
        let kicked = host.fire_mock("bs_mock", &ctx);
        assert!(kicked.contains("dispatched"), "mock fire dispatches: {kicked}");
        let (pid, mid, ..) = recv_ask(&rx, Duration::from_secs(10));
        assert_eq!(mid, "bs_mock", "the mock fire raised a REAL beacon");
        host.answer(pid, Some(0));
        std::thread::sleep(Duration::from_millis(400));
        let log = host.drain_log();
        let no_op = format!("mock ask=True key='{KEY_NO_OP}'");
        assert!(
            log.iter().any(|l| l.contains(&no_op)),
            "mock: beacon answered True AND the input verb no-opped ({no_op}): {log:?}"
        );
    }

    // ── PHASE M: INTERLEAVED MULTI-MACRO RAPID FIRE ────────────────────────────────────────────
    // Three macros, each fired three times (nine asks). They run concurrently (distinct serial
    // queues), so several prompts are open at once; answers, routed by macro_id, must each reach the
    // right asker — and each macro's three fires all resume with its own verdict.
    {
        let rx = host.beacon_events();
        for (id, name) in [("bs_mm_a", "A"), ("bs_mm_b", "B"), ("bs_mm_c", "C")] {
            let src = format!("def macro(ctx):\n    print('{name} got %r' % ask('q', timeout=15))\n");
            host.register(id, &src).unwrap_or_else(|e| panic!("register {id}: {e}"));
        }
        host.drain_log();
        for id in ["bs_mm_a", "bs_mm_b", "bs_mm_c"] {
            for _ in 0..3 {
                assert!(host.fire_async(id, &ctx).contains("dispatched"));
            }
        }
        // answer nine prompts, verdict chosen by which macro asked (A->yes, B->no, C->yes).
        let mut answered = 0;
        while answered < 9 {
            let (pid, mid, ..) = recv_ask(&rx, Duration::from_secs(10));
            let choice = usize::from(mid == "bs_mm_b");
            host.answer(pid, Some(choice));
            answered += 1;
        }
        std::thread::sleep(Duration::from_millis(600));
        let log = host.drain_log();
        let count = |needle: &str| log.iter().filter(|l| l.contains(needle)).count();
        assert_eq!(count("A got True"), 3, "macro A's three fires each got YES: {log:?}");
        assert_eq!(count("B got False"), 3, "macro B's three fires each got NO: {log:?}");
        assert_eq!(count("C got True"), 3, "macro C's three fires each got YES: {log:?}");
    }

    // ── PHASE N: HIGH-VOLUME AUTO-DISMISS (no listener) ────────────────────────────────────────
    // One thousand asks with NO listener and a tiny self-timeout: every one must auto-dismiss to its
    // default with no hang, no panic, and no respawn (same warm sidecar pid throughout). The bounded
    // log ring absorbs the flood without growing memory. Uses blocking invoke so none are dropped by
    // the per-macro queue cap (that backpressure path is covered by the rapid-fire phases above).
    {
        // ensure no live listener: a fresh receiver, immediately dropped, leaves a dead slot that the
        // first ask clears — every ask then takes the no-UI path.
        drop(host.beacon_events());
        host.register(
            "bs_flood",
            "# neuron: raw\nimport os\ndef macro(ctx):\n    return 'pid=%d a=%r' % (os.getpid(), ask('q', timeout=0.05))\n",
        )
        .expect("register bs_flood");
        host.drain_log();
        const N: usize = 1000;
        let t0 = Instant::now();
        let mut sidecar_pid: Option<String> = None;
        for i in 0..N {
            let r = host.invoke("bs_flood", &ctx);
            assert!(r.contains("a=None"), "flood ask {i} must auto-dismiss to default: {r}");
            let pid = r.split("pid=").nth(1).and_then(|s| s.split(' ').next()).map(str::to_string);
            match &sidecar_pid {
                None => sidecar_pid = pid,
                Some(p) => assert_eq!(pid.as_ref(), Some(p), "flood ask {i}: sidecar respawned (not warm)"),
            }
        }
        eprintln!("beacon flood: {N} asks auto-dismissed in {:?} (same sidecar {sidecar_pid:?})", t0.elapsed());
        // the bounded ring never exceeds its cap, even after 1000 dismiss lines.
        assert!(host.drain_log().len() <= 400, "the macro-log ring stayed bounded under the flood");
    }

    // ── PHASE O: RAW CRASH RETIRES RAW ONLY; BOUND ASKS SURVIVE ────────────────────────────────
    {
        let rx = host.beacon_events();
        host.register("bs_inflight_a", "def macro(ctx):\n    ask('a', timeout=15)\n").unwrap();
        host.register("bs_inflight_b", "def macro(ctx):\n    ask('b', timeout=15)\n").unwrap();
        assert!(host.fire_async("bs_inflight_a", &ctx).contains("dispatched"));
        assert!(host.fire_async("bs_inflight_b", &ctx).contains("dispatched"));
        let (pid_a, ..) = recv_ask(&rx, Duration::from_secs(10));
        let (pid_b, ..) = recv_ask(&rx, Duration::from_secs(10));

        host.register("bs_crash", "# neuron: raw\nimport os\ndef macro(ctx):\n    os._exit(1)\n").unwrap();
        let _ = host.fire_async("bs_crash", &ctx);

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut saw_raw_retire = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(BeaconEvent::RetireDomain { mode: neuron::macros::MacroMode::Raw }) => {
                    saw_raw_retire = true;
                    break;
                }
                Ok(BeaconEvent::RetireDomain { mode: neuron::macros::MacroMode::Bound }) => {
                    panic!("RAW crash retired the healthy BOUND prompt domain");
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_raw_retire, "RAW sidecar crash must retire only the RAW prompt domain");

        // Both BOUND asks are still owned by the healthy interpreter and remain answerable.
        host.answer(pid_a, Some(0));
        host.answer(pid_b, Some(0));
        drop(rx);

        // The RAW lane itself respawns and serves a fresh RAW ask.
        let rx = host.beacon_events();
        host.register(
            "bs_revive",
            "# neuron: raw\ndef macro(ctx):\n    return 'rv=%r' % ask('revive?', timeout=15)\n",
        )
        .expect("register on the respawned RAW sidecar");
        let res = invoke_async("bs_revive", &ctx);
        let (pid, ..) = recv_ask(&rx, Duration::from_secs(15));
        host.answer(pid, Some(0));
        assert!(result(&res).contains("rv=True"), "the respawned RAW sidecar serves a fresh beacon");
    }

    // ── teardown ───────────────────────────────────────────────────────────────────────────────
    for id in [
        "bs_auto", "bs_choose", "bs_bare", "bs_empty", "bs_n2", "bs_n4", "bs_timeout", "bs_late",
        "bs_oor", "bs_pid", "bs_short", "bs_long", "bs_relisten", "bs_middrop", "bs_mock", "bs_mm_a",
        "bs_mm_b", "bs_mm_c", "bs_flood", "bs_inflight_a", "bs_inflight_b", "bs_crash", "bs_revive",
    ] {
        host.unregister(id);
    }
    std::env::set_current_dir(prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
}
