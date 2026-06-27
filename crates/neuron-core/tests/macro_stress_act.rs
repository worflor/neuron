//! NON-DESTRUCTIVE stress tests for the `act` DIMENSION of the macro system — the device/audio/
//! brightness/sense bridge a macro reaches through `neuron.dpi/profile/battery/…`. The path under
//! test is the full round-trip:
//!
//!   macro helper -> `neuron._act(verb, arg, gated)` (runtime/host/neuron.py) -> a framed
//!   `{"t":"act","rid":…,"verb":…,"arg":…}` -> the host reader's `Some("act")` arm
//!   (`macro_host.rs::reader_loop`) -> `run_act(verb, arg)` (off the reader thread) ->
//!   `{"t":"act_result","rid":…,"ok":…,"msg":…}` -> `_deliver_act` wakes the blocked helper.
//!
//! What these phases prove against the REAL warm sidecar, DISARMED throughout:
//!   * VERB ROUTING + REPLY SHAPE — every verb name reaches `run_act` and the right marker comes back.
//!     The effectful verbs are routed via their VALIDATION rejects (a malformed arg returns run_act's
//!     "…must be a number/non-empty string" BEFORE any device write is built), so the routing is proven
//!     with ZERO hardware access. Senses are checked for their documented return SHAPE (real read OR an
//!     absent/None marker — both are valid and non-destructive).
//!   * GATED (writes) vs UNGATED (reads) — disarmed, every effectful verb no-ops at the python gate and
//!     returns "[disarmed]" (the frame is never even sent → nothing touches a device/the mixer), while
//!     a read verb bypasses the gate and reaches `run_act` regardless of arm state.
//!   * rid-KEYED CORRELATION — sequential acts on one worker, and concurrent acts from MANY workers,
//!     each get their OWN reply (the verb echoes back in the unknown-verb marker, so a cross-wire would
//!     show a mismatched verb).
//!   * TIMEOUT + LATE-REPLY — a forced-short `_act` returns "[timed out]" (not a hang); the late reply
//!     for the now-expired rid is dropped, and the very next act still works.
//!   * UNKNOWN / MALFORMED VERB — a clean error, no panic, and the sidecar SURVIVES (same pid across
//!     the whole suite) and keeps serving.
//!
//! NON-DESTRUCTIVE by construction: `set_armed(false)` the whole time, so the public write helpers
//! never fire; the few writes routed deep enough to reach `run_act` are sent with arguments that
//! `run_act` REJECTS before constructing a device write; senses only READ. No hardware is required —
//! on a machine with no Razer device the senses return their absent markers and everything still
//! passes. The macros dir is isolated into a private temp cwd (and `NEURON_MACRO_STATE` into a second
//! temp dir) and both are removed after.
//!
//! IDIOM (mirrors the other `macro_stress_*` suites): the Macro Host is a process-global singleton and
//! the cwd/env are process-global, so this is ONE serial `#[test]` with sequential phases.
//!
//! Skips cleanly (not a failure) when no python runtime is resolvable.

use neuron::macros::{macro_host, Context, MacroHost};
use std::time::Duration;

/// A generous per-fire wait. The act macros do a handful of fast round-trips (a malformed/unknown verb
/// returns from `run_act` before any device access); the sense macro may do a real (still fast) device
/// READ on a machine that has one, so give it plenty of head-room over the default FIRE_BUDGET.
const ACT_BUDGET: Duration = Duration::from_secs(15);

/// Invoke a macro and wait (generously) for its return value — the act round-trips finish well inside.
fn run(host: &MacroHost, id: &str, ctx: &Context) -> String {
    host.invoke_with_budget(id, ctx, ACT_BUDGET)
}

/// First run of ASCII digits after `key` in `s` (e.g. the sidecar pid out of `pid=4242`).
fn num_after(s: &str, key: &str) -> Option<u64> {
    s.split(key)
        .nth(1)
        .and_then(|x| x.split(|c: char| !c.is_ascii_digit()).next())
        .filter(|d| !d.is_empty())
        .and_then(|d| d.parse().ok())
}

#[test]
fn act_protocol_stress_e2e() {
    let pid = std::process::id();
    let tmp = std::env::temp_dir().join(format!("neuron_act_{pid}"));
    let state = std::env::temp_dir().join(format!("neuron_act_state_{pid}"));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(&state).unwrap();

    // Set BEFORE the sidecar spawns (it inherits the env at spawn) — any store I/O lands in our temp,
    // never the user's real data dir. (No phase here uses the store, but we isolate it on principle.)
    std::env::set_var("NEURON_MACRO_STATE", &state);
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping act protocol stress e2e: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        std::env::remove_var("NEURON_MACRO_STATE");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&state);
        return;
    }
    host.set_armed(false); // DISARMED throughout — no real device/audio write can occur.
    let ctx = Context::synthetic(Some("act.exe".into()), None, None, None, None);

    // a pid reporter that survives respawns (in the manifest) — its pid must be IDENTICAL at the end,
    // proving NO act (unknown verb, malformed arg, forced timeout, concurrency) ever crashed the sidecar.
    host.register("act_pid", "import os\ndef macro(ctx):\n    return 'pid=%d' % os.getpid()\n")
        .expect("register act_pid");
    let pid_start = num_after(&run(host, "act_pid", &ctx), "pid=").expect("warm sidecar pid");

    // ── VERB ROUTING through act -> run_act -> reply (zero hardware access) ───────────────────────
    // Each effectful verb is sent UNGATED with an argument run_act REJECTS before building any device
    // write, so the exact run_act marker proves the verb name routed end-to-end without touching
    // hardware. The unknown verb proves the catch-all error arm. `_act` is the protocol entry point.
    host.register(
        "act_route",
        "def macro(ctx):\n\
        \x20   dpi_bad = neuron._act('dpi', 'NaN', gated=False)\n\
        \x20   bri_bad = neuron._act('brightness', 'NaN', gated=False)\n\
        \x20   prof_empty = neuron._act('profile', '', gated=False)\n\
        \x20   unknown = neuron._act('totally_bogus_verb', gated=False)\n\
        \x20   return 'dpi=[%s] bri=[%s] prof=[%s] unk=[%s]' % (dpi_bad, bri_bad, prof_empty, unknown)\n",
    )
    .unwrap();
    let r = run(host, "act_route", &ctx);
    assert!(r.contains("dpi(n): n must be a number"), "dpi verb routes to run_act + validates: {r}");
    assert!(r.contains("brightness(pct): pct must be a number"), "brightness verb routes + validates: {r}");
    assert!(
        r.contains("profile(name): name must be a non-empty string"),
        "profile verb routes + validates: {r}"
    );
    assert!(r.contains("unknown device verb 'totally_bogus_verb'"), "an unknown verb routes to a clean error: {r}");

    // ── SENSE reads (ungated): documented return SHAPE, device-present OR absent ──────────────────
    // The macro itself type-checks each sense so the assertion is robust on any machine: battery is a
    // {percent,charging} dict or None; current_dpi/scroll_stage are int or None; active_profile is a
    // str. A real read (device present) and an absent marker (no device) both satisfy the contract —
    // either way the act frame routed to run_act and a reply came back through the rid correlation.
    host.register(
        "act_senses",
        "def macro(ctx):\n\
        \x20   b = neuron.battery()\n\
        \x20   d = neuron.current_dpi()\n\
        \x20   p = neuron.active_profile()\n\
        \x20   s = neuron.scroll_stage()\n\
        \x20   bok = b is None or (isinstance(b, dict) and 'percent' in b and 'charging' in b)\n\
        \x20   dok = d is None or isinstance(d, int)\n\
        \x20   pok = isinstance(p, str)\n\
        \x20   sok = s is None or isinstance(s, int)\n\
        \x20   return 'bok=%s dok=%s pok=%s sok=%s' % (bok, dok, pok, sok)\n",
    )
    .unwrap();
    let r = run(host, "act_senses", &ctx);
    assert!(r.contains("bok=True"), "battery() returns a {{percent,charging}} dict or None: {r}");
    assert!(r.contains("dok=True"), "current_dpi() returns an int or None: {r}");
    assert!(r.contains("pok=True"), "active_profile() returns a string: {r}");
    assert!(r.contains("sok=True"), "scroll_stage() returns an int or None: {r}");

    // ── GATED writes, DISARMED -> "[disarmed]" (the frame is never sent → nothing is touched) ────
    // Every effectful verb (device + audio + brightness) no-ops at the python arm gate. This is the
    // primary non-destructive guarantee: a gated helper returns "[disarmed]" BEFORE _host_send, so no
    // act frame reaches run_act and no device/mixer write is even attempted.
    host.register(
        "act_gated",
        "def macro(ctx):\n\
        \x20   r = []\n\
        \x20   r.append('dpi=' + str(neuron.dpi(1600)))\n\
        \x20   r.append('dpi_cycle=' + str(neuron.dpi_cycle('up')))\n\
        \x20   r.append('scroll_cycle=' + str(neuron.scroll_cycle('up')))\n\
        \x20   r.append('profile=' + str(neuron.profile('Gaming')))\n\
        \x20   r.append('profile_cycle=' + str(neuron.profile_cycle('up')))\n\
        \x20   r.append('brightness=' + str(neuron.brightness(50)))\n\
        \x20   r.append('mic_mute=' + str(neuron.mic_mute('toggle')))\n\
        \x20   r.append('out_mute=' + str(neuron.out_mute('toggle')))\n\
        \x20   r.append('mic_gain=' + str(neuron.mic_gain(5)))\n\
        \x20   r.append('out_gain=' + str(neuron.out_gain(-5)))\n\
        \x20   return ' '.join(r)\n",
    )
    .unwrap();
    let r = run(host, "act_gated", &ctx);
    for verb in [
        "dpi", "dpi_cycle", "scroll_cycle", "profile", "profile_cycle", "brightness", "mic_mute",
        "out_mute", "mic_gain", "out_gain",
    ] {
        assert!(
            r.contains(&format!("{verb}=[disarmed]")),
            "disarmed: '{verb}' must no-op at the gate (got: {r})"
        );
    }

    // ── GATED vs UNGATED contrast ────────────────────────────────────────────────────────────────
    // Same verb family, both disarmed: a gated write is blocked ("[disarmed]"), an ungated read
    // reaches run_act and returns something else entirely (a dpi value, or "no dpi-capable device" —
    // never "[disarmed]"). That's the whole gated/ungated distinction in one fire.
    host.register(
        "act_gate_contrast",
        "def macro(ctx):\n\
        \x20   w = neuron._act('dpi', 1600, gated=True)\n\
        \x20   r = neuron._act('current_dpi', gated=False)\n\
        \x20   return 'write=[%s] read=[%s]' % (w, r)\n",
    )
    .unwrap();
    let r = run(host, "act_gate_contrast", &ctx);
    assert!(r.contains("write=[[disarmed]]"), "a gated write is blocked when disarmed: {r}");
    assert!(
        !r.contains("read=[[disarmed]]"),
        "an ungated read bypasses the gate and reaches run_act even when disarmed: {r}"
    );

    // ── rid-KEYED CORRELATION, sequential ────────────────────────────────────────────────────────
    // Four distinct verbs back-to-back on one worker. run_act echoes each verb in its unknown-verb
    // marker, so a cross-wired rid would surface the WRONG verb in a slot. Each must carry its own.
    host.register(
        "act_rid_seq",
        "def macro(ctx):\n\
        \x20   a = neuron._act('rid_alpha', gated=False)\n\
        \x20   b = neuron._act('rid_bravo', gated=False)\n\
        \x20   c = neuron._act('rid_charlie', gated=False)\n\
        \x20   d = neuron._act('rid_delta', gated=False)\n\
        \x20   return 'a=[%s] b=[%s] c=[%s] d=[%s]' % (a, b, c, d)\n",
    )
    .unwrap();
    let r = run(host, "act_rid_seq", &ctx);
    assert!(r.contains("a=[unknown device verb 'rid_alpha']"), "rid slot a is its own reply: {r}");
    assert!(r.contains("b=[unknown device verb 'rid_bravo']"), "rid slot b is its own reply: {r}");
    assert!(r.contains("c=[unknown device verb 'rid_charlie']"), "rid slot c is its own reply: {r}");
    assert!(r.contains("d=[unknown device verb 'rid_delta']"), "rid slot d is its own reply: {r}");

    // ── TIMEOUT + LATE REPLY ─────────────────────────────────────────────────────────────────────
    // A forced-zero timeout makes `_act` give up before any reply can possibly round-trip — it returns
    // "[timed out]" instead of hanging. The host's late act_result for that now-expired rid is dropped
    // (the slot was already popped), and the very next act on the same worker still works cleanly.
    host.register(
        "act_timeout",
        "def macro(ctx):\n\
        \x20   t = neuron._act('active_profile', timeout=0, gated=False)\n\
        \x20   n = neuron._act('after_timeout_probe', gated=False)\n\
        \x20   return 'timed=[%s] next=[%s]' % (t, n)\n",
    )
    .unwrap();
    let r = run(host, "act_timeout", &ctx);
    assert!(r.contains("timed=[[timed out]]"), "a reply that can't arrive in time yields a timeout marker, not a hang: {r}");
    assert!(
        r.contains("next=[unknown device verb 'after_timeout_probe']"),
        "after a timeout (+ a dropped late reply), the next act still works: {r}"
    );

    // ── CONCURRENT acts from MANY worker threads don't cross wires ────────────────────────────────
    // One macro per tag, each doing a single act that echoes its OWN tag; fired CONCURRENTLY from
    // separate Rust threads (distinct ids → distinct sidecar serial workers → concurrent acts, each
    // allocating its own rid under the sidecar's _act_lock). Every result must carry its own tag.
    let tags = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];
    for tag in tags {
        host.register(
            &format!("act_conc_{tag}"),
            &format!("def macro(ctx):\n    return neuron._act('conc_{tag}', gated=False)\n"),
        )
        .unwrap();
    }
    let handles: Vec<_> = tags
        .iter()
        .map(|tag| {
            let tag = tag.to_string();
            std::thread::spawn(move || {
                let host = macro_host();
                let ctx = Context::synthetic(Some("act.exe".into()), None, None, None, None);
                (tag.clone(), run(host, &format!("act_conc_{tag}"), &ctx))
            })
        })
        .collect();
    for h in handles {
        let (tag, r) = h.join().expect("concurrent act thread joins");
        assert!(
            r.contains(&format!("unknown device verb 'conc_{tag}'")),
            "concurrent act for '{tag}' got its OWN reply (no cross-wire): {r}"
        );
    }

    // ── SIDECAR SURVIVED EVERYTHING ──────────────────────────────────────────────────────────────
    // No malformed arg, unknown verb, forced timeout, or burst of concurrent acts may crash/respawn
    // the firewalled sidecar — the pid must be unchanged from the very first fire.
    let pid_end = num_after(&run(host, "act_pid", &ctx), "pid=").expect("sidecar still warm");
    assert_eq!(
        pid_end, pid_start,
        "the act protocol must never crash or respawn the sidecar (pid {pid_start} -> {pid_end})"
    );

    // ── teardown ─────────────────────────────────────────────────────────────────────────────────
    let mut ids: Vec<String> = vec![
        "act_pid", "act_route", "act_senses", "act_gated", "act_gate_contrast", "act_rid_seq",
        "act_timeout",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    ids.extend(tags.iter().map(|t| format!("act_conc_{t}")));
    for id in ids {
        host.unregister(&id);
    }
    std::env::set_current_dir(prev).ok();
    std::env::remove_var("NEURON_MACRO_STATE");
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&state);
}
