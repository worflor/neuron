// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! NON-DESTRUCTIVE EXECUTION stress tests for every macro STEP type, run against the REAL warm
//! CPython sidecar. The audit found ~11% execution coverage (only Ask/Notify were ever executed);
//! this drives every action (Type/Press/KeyPress/Click/Scroll/MoveTo/Copy/Paste/Open/Focus/Wait/
//! Notify), every flow node (If/RepeatN/RepeatWhile/ForEach/SetVar/Try/Stop/Ask), and the Value
//! system (ctx reads, call chains, binary exprs), plus combinations, error isolation, and the
//! disarm guard.
//!
//! STRICTLY NON-DESTRUCTIVE — the sidecar runs with input DISARMED for the whole test, so every
//! effectful helper (`type_text`/`hotkey`/`click`/`scroll`/`mouse_to`/`clipboard_set`/`run`/`focus`)
//! NO-OPS and returns its `[disarmed]` marker: NO real keystrokes, clicks, mouse moves, clipboard
//! writes, or process spawns ever happen. We assert behavior via the macro's RETURN value, the macro
//! LOG (notify lines), and BEACON events. The macros dir is isolated to a private temp cwd. Skips
//! cleanly if the bundled python can't materialize.
//!
//! ONE monolithic test: the sidecar, the macro log ring, and the cwd are process-global, so phases
//! run serially — the established pattern for this project's sidecar e2e tests.

use neuron::macros::{macro_host, BeaconEvent, Context, MacroHost};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

// ── helpers ───────────────────────────────────────────────────────────────────────────────────────

/// Register `src` as `id`, INVOKE it (blocking, disarmed), unregister, and return the one-line
/// invoke result (`macro '<id>': <value>` / `... ran` / `... error: <e>`).
fn run(host: &MacroHost, id: &str, src: &str, ctx: &Context) -> String {
    host.register(id, src).unwrap_or_else(|e| panic!("register {id}: {e}"));
    let r = host.invoke(id, ctx);
    host.unregister(id);
    r
}

/// Like [`run`] but returns the macro LOG lines produced by the fire (notify lines land as
/// `[<id>] <text>`). Drains the ring before + after so the lines belong to THIS fire only.
fn run_logs(host: &MacroHost, id: &str, src: &str, ctx: &Context) -> Vec<String> {
    host.drain_log();
    host.register(id, src).unwrap_or_else(|e| panic!("register {id}: {e}"));
    let _ = host.invoke(id, ctx);
    let logs = host.drain_log();
    host.unregister(id);
    logs
}

fn count(logs: &[String], marker: &str) -> usize {
    logs.iter().filter(|l| l.contains(marker)).count()
}
fn has(logs: &[String], marker: &str) -> bool {
    logs.iter().any(|l| l.contains(marker))
}

/// What an Ask beacon carried: (text, option labels, detail).
type AskInfo = (String, Vec<String>, String);

/// Register `src`, fire it on a worker thread, wait for its Ask beacon on `rx`, answer with
/// `answer` (`Some(i)` = option i, `None` = dismiss), and return (invoke result, the Ask info).
fn ask_run(
    rx: &Receiver<BeaconEvent>,
    id: &str,
    src: &str,
    ctx: &Context,
    answer: Option<usize>,
) -> (String, AskInfo) {
    let host = macro_host();
    host.register(id, src).unwrap_or_else(|e| panic!("register {id}: {e}"));
    let id2 = id.to_string();
    let ctx2 = ctx.clone();
    let (tx, res_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(macro_host().invoke(&id2, &ctx2));
    });
    let info: AskInfo;
    let pid = loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(BeaconEvent::Ask { pid, text, options, detail, .. }) => {
                info = (text, options, detail);
                break pid;
            }
            Ok(_) => continue, // skip notify / retire noise
            Err(e) => panic!("no Ask beacon arrived for {id}: {e}"),
        }
    };
    macro_host().answer(pid, answer);
    let r = res_rx.recv_timeout(Duration::from_secs(15)).expect("invoke result");
    host.unregister(id);
    (r, info)
}

fn ctx_app(app: &str) -> Context {
    Context::synthetic(Some(app.into()), None, None, None, None)
}
fn ctx_sel(sel: &str) -> Context {
    Context::synthetic(Some("test.exe".into()), None, None, None, Some(sel.into()))
}
fn ctx_clip(clip: &str) -> Context {
    Context::synthetic(Some("test.exe".into()), None, None, Some(clip.into()), None)
}

#[test]
fn steps_stress() {
    let tmp = std::env::temp_dir().join(format!("neuron_macro_stress_steps_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    // config resolves via the run root (NEURON_RUN_DIR, else the exe dir) — pin it to the same tmp
    let _run_pin = neuron::runroot::RunDirPin::to(&tmp);

    let host = macro_host();
    if !host.available() {
        eprintln!("skipping steps_stress: bundled python runtime did not materialize");
        std::env::set_current_dir(prev).ok();
        let _ = std::fs::remove_dir_all(&tmp);
        return;
    }
    host.set_armed(false); // DISARMED for the entire test — no real input synthesis ever.

    let result = std::panic::catch_unwind(|| run_step_phases(host));

    std::env::set_current_dir(&prev).ok();
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn run_step_phases(host: &MacroHost) {
    let dflt = ctx_app("test.exe");

    // ════════════════ TYPE ════════════════
    // Type_empty_string
    let r = run(host, "s_type_empty", "def macro(ctx):\n    return str(neuron.type_text(''))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "type('') disarmed: {r}");
    // Type_long_unicode_string (12k chars: emoji + combining + zero-width + Arabic)
    let r = run(
        host,
        "s_type_uni",
        "def macro(ctx):\n    s = ('\\U0001F600\\u0301\\u200B\\u0627') * 3000\n    return str(neuron.type_text(s))\n",
        &dflt,
    );
    assert!(r.contains("[disarmed]") && !r.contains("error"), "12k-unicode type disarmed completes: {r}");
    // Type_value_expression: ctx.selection.upper()
    let r = run(
        host,
        "s_type_val",
        "def macro(ctx):\n    s = ctx.selection.upper()\n    neuron.type_text(s)\n    return s\n",
        &ctx_sel("hello"),
    );
    assert!(r.contains("HELLO"), "value expression must evaluate to HELLO: {r}");
    // Type_ghost_speed_variations
    let r = run(
        host,
        "s_ghost_speeds",
        "def macro(ctx):\n    return '|'.join(str(neuron.type_ghost('t', sp)) for sp in ['fast','slow','borderline'])\n",
        &dflt,
    );
    assert!(r.contains("[disarmed]") && !r.contains("error"), "ghost speeds disarmed: {r}");
    // Type_ghost_speed_invalid (tolerant)
    let r = run(host, "s_ghost_bad", "def macro(ctx):\n    return str(neuron.type_ghost('x', 'invalid_speed'))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "invalid ghost speed tolerated disarmed: {r}");
    eprintln!("[steps] TYPE ok");

    // ════════════════ PRESS / KEY ════════════════
    // Press_empty_keys -> hotkey() with no keys: validity passes, gate returns [disarmed]
    let r = run(host, "s_press_empty", "def macro(ctx):\n    return str(neuron.hotkey())\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "empty hotkey disarmed no-op: {r}");
    // Press_valid_chord
    let r = run(host, "s_press_ok", "def macro(ctx):\n    return str(neuron.hotkey('ctrl','c'))\n", &dflt);
    assert!(r.contains("[disarmed]"), "valid chord disarmed: {r}");
    // Press_invalid_key_name -> validity is checked BEFORE the gate, so it surfaces clearly
    let r = run(host, "s_press_bad", "def macro(ctx):\n    return str(neuron.hotkey('ctrl','invalid_key_xyz'))\n", &dflt);
    assert!(r.contains("unknown key"), "invalid chord key must surface clearly: {r}");
    // KeyPress_valid_key
    let r = run(host, "s_key_ok", "def macro(ctx):\n    return str(neuron.key('enter'))\n", &dflt);
    assert!(r.contains("[disarmed]"), "valid key disarmed: {r}");
    // KeyPress_unknown_key
    let r = run(host, "s_key_bad", "def macro(ctx):\n    return str(neuron.key('not_a_real_key'))\n", &dflt);
    assert!(r.contains("unknown key"), "unknown key surfaces clearly: {r}");
    eprintln!("[steps] PRESS/KEY ok");

    // ════════════════ CLICK / SCROLL / MOVETO ════════════════
    let r = run(
        host,
        "s_click_all",
        "def macro(ctx):\n    return '|'.join(str(neuron.click(b)) for b in ['left','right','middle'])\n",
        &dflt,
    );
    assert_eq!(r.matches("[disarmed]").count(), 3, "all three buttons disarmed: {r}");
    let r = run(host, "s_click_bad", "def macro(ctx):\n    return str(neuron.click('top'))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "invalid button disarmed no-op: {r}");
    let r = run(
        host,
        "s_scroll",
        "def macro(ctx):\n    return '|'.join(str(neuron.scroll(n)) for n in [5,-3,0])\n",
        &dflt,
    );
    assert_eq!(r.matches("[disarmed]").count(), 3, "scroll +/-/0 disarmed: {r}");
    let r = run(host, "s_scroll_huge", "def macro(ctx):\n    return str(neuron.scroll(100000))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "huge scroll no overflow disarmed: {r}");
    let r = run(
        host,
        "s_moveto",
        "def macro(ctx):\n    return '|'.join(str(neuron.mouse_to(x,y)) for (x,y) in [(0,0),(1920,1080),(-100,5000)])\n",
        &dflt,
    );
    assert_eq!(r.matches("[disarmed]").count(), 3, "moveto coords disarmed: {r}");
    eprintln!("[steps] CLICK/SCROLL/MOVETO ok");

    // ════════════════ COPY / PASTE ════════════════
    let r = run(host, "s_copy_empty", "def macro(ctx):\n    return str(neuron.clipboard_set(''))\n", &dflt);
    assert!(r.contains("[disarmed]"), "copy('') disarmed: {r}");
    let r = run(host, "s_copy_big", "def macro(ctx):\n    s = 'x' * 5000000\n    return str(neuron.clipboard_set(s))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "5MB copy disarmed completes: {r}");
    let r = run(
        host,
        "s_copy_val",
        "def macro(ctx):\n    v = ctx.clipboard.upper()\n    neuron.clipboard_set(v)\n    return v\n",
        &ctx_clip("test"),
    );
    assert!(r.contains("TEST"), "copy value expression evaluates: {r}");
    let r = run(host, "s_paste", "def macro(ctx):\n    return str(neuron.hotkey('ctrl','v'))\n", &dflt);
    assert!(r.contains("[disarmed]"), "paste (ctrl+v) disarmed: {r}");
    eprintln!("[steps] COPY/PASTE ok");

    // ════════════════ OPEN (process spawn — disarmed = never spawns) ════════════════
    let r = run(host, "s_open_ff", "def macro(ctx):\n    return str(neuron.run('echo test'))\n", &dflt);
    assert!(r.contains("[disarmed]"), "fire-and-forget run disarmed never spawns: {r}");
    // Open_with_capture: disarmed run(wait=True) returns the [disarmed] marker (NOT a (code,stdout)
    // tuple — that only happens ARMED). Asserting the real, non-destructive disarmed contract.
    let r = run(host, "s_open_cap", "def macro(ctx):\n    out = neuron.run('echo test', wait=True)\n    return repr(out)\n", &dflt);
    assert!(r.contains("[disarmed]"), "captured disarmed run yields the marker: {r}");
    // Open_capture_failing_command: still disarmed (armed process-capture is out of scope for a
    // strictly non-destructive suite — it would spawn a real subprocess).
    let r = run(host, "s_open_fail", "def macro(ctx):\n    out = neuron.run('exit 1', wait=True)\n    return repr(out)\n", &dflt);
    assert!(r.contains("[disarmed]"), "failing-cmd capture disarmed: {r}");
    eprintln!("[steps] OPEN ok");

    // ════════════════ FOCUS ════════════════
    let r = run(host, "s_focus_ok", "def macro(ctx):\n    return str(neuron.focus('Notepad'))\n", &dflt);
    assert!(r.contains("[disarmed]"), "focus disarmed no-op: {r}");
    let r = run(host, "s_focus_no", "def macro(ctx):\n    return str(neuron.focus('WindowThatDoesNotExist_XYZ'))\n", &dflt);
    assert!(r.contains("[disarmed]") && !r.contains("error"), "focus missing window disarmed: {r}");
    eprintln!("[steps] FOCUS ok");

    // ════════════════ WAIT ════════════════
    // Wait_zero_milliseconds
    let r = run(host, "s_wait_zero", "def macro(ctx):\n    neuron.sleep(0)\n    return 'ok'\n", &dflt);
    assert!(r.contains("ok") && !r.contains("error"), "sleep(0) completes: {r}");
    // Wait_negative_milliseconds: sleep is NOT arm-gated, so a negative reaches time.sleep and
    // raises ValueError (Python rejects it). REAL behavior = "rejects"; the sidecar must survive.
    let r = run(host, "s_wait_neg", "def macro(ctx):\n    neuron.sleep(-100)\n    return 'ok'\n", &dflt);
    assert!(r.contains("error"), "negative sleep is rejected by Python (ValueError): {r}");
    let alive = run(host, "s_wait_alive", "def macro(ctx):\n    return 'alive'\n", &dflt);
    assert!(alive.contains("alive"), "sidecar survives a sleep error: {alive}");
    // Wait_long_sleep: ~700ms actually elapses (sleep is not gated).
    let t = Instant::now();
    let r = run(host, "s_wait_long", "def macro(ctx):\n    neuron.sleep(700)\n    return 'slept'\n", &dflt);
    let el = t.elapsed();
    assert!(r.contains("slept"), "long sleep completes: {r}");
    assert!(el >= Duration::from_millis(600), "sleep(700) must actually wait ({el:?})");
    // a sleep BEYOND the wait budget surfaces the "still running" message rather than hanging.
    host.register("s_wait_budget", "def macro(ctx):\n    neuron.sleep(1500)\n    return 'late'\n").unwrap();
    let r = host.invoke_with_budget("s_wait_budget", &dflt, Duration::from_millis(300));
    assert!(r.contains("running"), "a beyond-budget sleep reports still-running, not a hang: {r}");
    host.unregister("s_wait_budget");
    eprintln!("[steps] WAIT ok");

    // ════════════════ NOTIFY ════════════════
    let logs = run_logs(
        host,
        "s_notify",
        "def macro(ctx):\n    neuron.notify('NOTE-ok')\n    neuron.notify('L' * 5000)\n    return 'done'\n",
        &dflt,
    );
    assert!(has(&logs, "NOTE-ok"), "simple notify reaches the log: {logs:?}");
    assert!(has(&logs, &"L".repeat(200)), "a long (5k) notify is not truncated: {logs:?}");
    eprintln!("[steps] NOTIFY ok");

    // ════════════════ IF ════════════════
    let logs = run_logs(host, "s_if_t", "def macro(ctx):\n    if True:\n        neuron.notify('YES')\n    else:\n        neuron.notify('NO')\n", &dflt);
    assert!(has(&logs, "YES") && !has(&logs, "NO"), "if True takes the then branch: {logs:?}");
    let logs = run_logs(host, "s_if_f", "def macro(ctx):\n    if False:\n        neuron.notify('YES')\n    else:\n        neuron.notify('NO')\n", &dflt);
    assert!(has(&logs, "NO") && !has(&logs, "YES"), "if False takes the else branch: {logs:?}");
    let logs = run_logs(
        host,
        "s_if_val",
        "def macro(ctx):\n    if ctx.app == 'test.exe':\n        neuron.notify('MATCH')\n    else:\n        neuron.notify('MISS')\n",
        &ctx_app("test.exe"),
    );
    assert!(has(&logs, "MATCH") && !has(&logs, "MISS"), "value-expr condition evaluates: {logs:?}");
    eprintln!("[steps] IF ok");

    // ════════════════ REPEAT (N / While) ════════════════
    let logs = run_logs(host, "s_rn_zero", "def macro(ctx):\n    for _ in range(0):\n        neuron.notify('TICK')\n", &dflt);
    assert_eq!(count(&logs, "TICK"), 0, "range(0) never runs the body");
    let logs = run_logs(host, "s_rn_neg", "def macro(ctx):\n    for _ in range(-5):\n        neuron.notify('TICK')\n", &dflt);
    assert_eq!(count(&logs, "TICK"), 0, "range(-5) never runs the body (Python semantics)");
    // large count via a RETURNED counter (avoids flooding the bounded log ring)
    let r = run(host, "s_rn_big", "def macro(ctx):\n    c = 0\n    for _ in range(1000):\n        c += 1\n    return str(c)\n", &dflt);
    assert!(r.contains("1000"), "range(1000) runs 1000 times: {r}");
    let logs = run_logs(host, "s_rn_if", "def macro(ctx):\n    for _ in range(3):\n        if True:\n            neuron.notify('TICK')\n", &dflt);
    assert_eq!(count(&logs, "TICK"), 3, "nested if inside repeat runs each iteration");
    let logs = run_logs(host, "s_rw_bounded", "def macro(ctx):\n    i = 0\n    while i < 3:\n        i = i + 1\n        neuron.notify('TICK')\n", &dflt);
    assert_eq!(count(&logs, "TICK"), 3, "while loop with a counter runs exactly 3 times");
    // RepeatWhile_infinite_escape: `while True: break` exits cleanly (NO cpu-pegging infinite loop;
    // the timeout-message path is proven by the beyond-budget sleep above).
    let r = run(host, "s_rw_break", "def macro(ctx):\n    while True:\n        break\n    return 'broke'\n", &dflt);
    assert!(r.contains("broke"), "while True / break exits: {r}");
    eprintln!("[steps] REPEAT ok");

    // ════════════════ FOREACH ════════════════
    let logs = run_logs(host, "s_fe_list", "def macro(ctx):\n    for x in ['a','b','c']:\n        neuron.notify('E:' + x)\n", &dflt);
    assert!(has(&logs, "E:a") && has(&logs, "E:b") && has(&logs, "E:c"), "foreach list: {logs:?}");
    let logs = run_logs(host, "s_fe_empty", "def macro(ctx):\n    for x in []:\n        neuron.notify('NEVER')\n", &dflt);
    assert_eq!(count(&logs, "NEVER"), 0, "foreach over [] never runs");
    let logs = run_logs(host, "s_fe_str", "def macro(ctx):\n    for c in 'abc':\n        neuron.notify('C:' + c)\n", &dflt);
    assert!(has(&logs, "C:a") && has(&logs, "C:b") && has(&logs, "C:c"), "foreach string chars: {logs:?}");
    let logs = run_logs(
        host,
        "s_fe_split",
        "def macro(ctx):\n    for line in ctx.selection.splitlines():\n        neuron.notify('LN:' + line)\n",
        &ctx_sel("line1\nline2"),
    );
    assert!(has(&logs, "LN:line1") && has(&logs, "LN:line2"), "foreach ctx.splitlines(): {logs:?}");
    eprintln!("[steps] FOREACH ok");

    // ════════════════ SETVAR / STOP ════════════════
    let r = run(host, "s_set_use", "def macro(ctx):\n    x = 'hello'\n    return x\n", &dflt);
    assert!(r.contains("hello"), "set var then use: {r}");
    let r = run(host, "s_set_over", "def macro(ctx):\n    x = 1\n    x = 2\n    return str(x)\n", &dflt);
    assert!(r.contains("2"), "overwrite var: {r}");
    let r = run(host, "s_set_undef", "def macro(ctx):\n    return str(undefined_var)\n", &dflt);
    assert!(r.contains("error"), "undefined var raises NameError surfaced as an error: {r}");
    let logs = run_logs(host, "s_stop", "def macro(ctx):\n    neuron.notify('BEFORE')\n    return\n    neuron.notify('AFTER')\n", &dflt);
    assert!(has(&logs, "BEFORE") && !has(&logs, "AFTER"), "return stops execution early: {logs:?}");
    eprintln!("[steps] SETVAR/STOP ok");

    // ════════════════ TRY ════════════════
    let logs = run_logs(host, "s_try_ok", "def macro(ctx):\n    try:\n        neuron.notify('OK')\n    except Exception:\n        neuron.notify('ERR')\n", &dflt);
    assert!(has(&logs, "OK") && !has(&logs, "ERR"), "try with no exception: {logs:?}");
    let logs = run_logs(host, "s_try_catch", "def macro(ctx):\n    try:\n        raise ValueError('oops')\n    except Exception:\n        neuron.notify('CAUGHT')\n", &dflt);
    assert!(has(&logs, "CAUGHT"), "try catches a raised exception: {logs:?}");
    let logs = run_logs(
        host,
        "s_try_nested",
        "def macro(ctx):\n    try:\n        try:\n            raise ValueError()\n        except Exception:\n            neuron.notify('INNER')\n    except Exception:\n        neuron.notify('OUTER')\n",
        &dflt,
    );
    assert!(has(&logs, "INNER") && !has(&logs, "OUTER"), "inner try catches, outer not reached: {logs:?}");
    eprintln!("[steps] TRY ok");

    // ════════════════ VALUE expressions ════════════════
    let r = run(host, "s_val_none", "def macro(ctx):\n    s = ctx.selection or 'fallback'\n    neuron.type_text(s)\n    return s\n", &ctx_app("test.exe"));
    assert!(r.contains("fallback"), "None ctx field falls back: {r}");
    let r = run(host, "s_val_chain", "def macro(ctx):\n    return ctx.selection.upper().replace('A', 'X')\n", &ctx_sel("abc"));
    assert!(r.contains("XBC"), "call chain evaluates ('abc'->'ABC'->'XBC'): {r}");
    let r = run(host, "s_val_bin", "def macro(ctx):\n    return 'result: ' + str(1 + 2)\n", &dflt);
    assert!(r.contains("result: 3"), "binary expr string concat: {r}");
    let logs = run_logs(host, "s_truthy", "def macro(ctx):\n    if 'string':\n        neuron.notify('TRUTHY')\n", &dflt);
    assert!(has(&logs, "TRUTHY"), "a non-empty string is truthy: {logs:?}");
    eprintln!("[steps] VALUE ok");

    // ════════════════ COMBINATIONS ════════════════
    let logs = run_logs(host, "s_combo_rn", "def macro(ctx):\n    n = 3\n    for _ in range(n):\n        neuron.notify('TICK')\n", &dflt);
    assert_eq!(count(&logs, "TICK"), 3, "setvar feeds range count");
    let logs = run_logs(
        host,
        "s_combo_iftry",
        "def macro(ctx):\n    if ctx.app:\n        try:\n            raise ValueError()\n        except Exception:\n            neuron.notify('CAUGHT')\n    else:\n        neuron.notify('NOAPP')\n",
        &ctx_app("test.exe"),
    );
    assert!(has(&logs, "CAUGHT") && !has(&logs, "NOAPP"), "if(app)+nested try: {logs:?}");
    eprintln!("[steps] COMBINATIONS ok");

    // ════════════════ ERROR ISOLATION ════════════════
    host.register("s_iso_bad", "def macro(ctx):\n    raise RuntimeError('boom')\n").unwrap();
    host.register("s_iso_good", "def macro(ctx):\n    return 'survived'\n").unwrap();
    let bad = host.invoke("s_iso_bad", &dflt);
    assert!(bad.contains("error"), "a raising macro surfaces an error: {bad}");
    let good = host.invoke("s_iso_good", &dflt);
    assert!(good.contains("survived"), "the next macro runs on the same warm sidecar: {good}");
    host.unregister("s_iso_bad");
    host.unregister("s_iso_good");
    eprintln!("[steps] ERROR ISOLATION ok");

    // ════════════════ DISARM verifies ALL effectful steps no-op ════════════════
    let r = run(
        host,
        "s_disarm_all",
        "def macro(ctx):\n    r = []\n    r.append(str(neuron.type_text('x')))\n    r.append(str(neuron.hotkey('ctrl','c')))\n    r.append(str(neuron.click('left')))\n    r.append(str(neuron.scroll(3)))\n    r.append(str(neuron.mouse_to(1,1)))\n    r.append(str(neuron.clipboard_set('x')))\n    r.append(str(neuron.run('echo hi')))\n    r.append(str(neuron.focus('nope')))\n    neuron.sleep(0)\n    return '|'.join(r)\n",
        &dflt,
    );
    assert!(!r.contains("error"), "the all-effects macro completes: {r}");
    assert_eq!(r.matches("[disarmed]").count(), 8, "all 8 effectful steps must no-op (disarmed): {r}");
    eprintln!("[steps] DISARM-ALL ok");

    // ════════════════ BEACON (Ask / choose) ════════════════
    // Ask_timeout_auto_dismiss FIRST, with NO listener installed: must return the default immediately.
    let t = Instant::now();
    let r = run(host, "s_ask_auto", "def macro(ctx):\n    return 'answer=%r' % neuron.ask('q', timeout=1)\n", &dflt);
    assert!(r.contains("answer=None"), "no-UI ask returns its default: {r}");
    assert!(t.elapsed() < Duration::from_secs(3), "auto-dismiss must not hang ({:?})", t.elapsed());

    // now install a listener and answer prompts.
    let rx = host.beacon_events();
    // Ask_yes_no_explicit
    let (r, _) = ask_run(&rx, "s_ask_yes", "def macro(ctx):\n    return 'yes' if neuron.ask('go?') else 'no'\n", &dflt, Some(0));
    assert!(r.contains("yes"), "answered YES reaches the macro: {r}");
    let (r, _) = ask_run(&rx, "s_ask_no", "def macro(ctx):\n    return 'yes' if neuron.ask('go?') else 'no'\n", &dflt, Some(1));
    assert!(r.contains("no"), "answered NO reaches the macro: {r}");
    // Ask_description_parameter: the detail rides the beacon.
    let (r, info) = ask_run(
        &rx,
        "s_ask_desc",
        "def macro(ctx):\n    return 'a=%r' % neuron.ask('confirm?', description='3 files will be deleted')\n",
        &dflt,
        Some(0),
    );
    assert!(r.contains("a=True"), "answered ask returns True: {r}");
    assert_eq!(info.0, "confirm?", "question text rides the beacon");
    assert_eq!(info.2, "3 files will be deleted", "description rides the beacon as detail");
    // Beacon_ask_three_plus_options: choose() with 4 options, answer index 2 -> 'c'.
    let (r, info) = ask_run(
        &rx,
        "s_choose",
        "def macro(ctx):\n    return 'choice=%r' % neuron.choose('pick', ['a','b','c','d'])\n",
        &dflt,
        Some(2),
    );
    assert_eq!(info.1, vec!["a", "b", "c", "d"], "all 4 wedges ride the beacon");
    assert!(r.contains("'c'"), "the chosen option (index 2 = 'c') reaches the macro: {r}");
    eprintln!("[steps] BEACON ok");

    // Combo_ForEach_Ask: two iterations, each asks; answer YES to both -> both notify.
    {
        host.drain_log();
        host.register(
            "s_combo_fe_ask",
            "def macro(ctx):\n    for x in ['a','b']:\n        if neuron.ask('continue?'):\n            neuron.notify('GO:' + x)\n",
        )
        .unwrap();
        let (tx, res_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(macro_host().invoke("s_combo_fe_ask", &Context::synthetic(Some("test.exe".into()), None, None, None, None)));
        });
        for _ in 0..2 {
            let pid = loop {
                match rx.recv_timeout(Duration::from_secs(15)) {
                    Ok(BeaconEvent::Ask { pid, .. }) => break pid,
                    Ok(_) => continue,
                    Err(e) => panic!("combo foreach-ask: missing prompt: {e}"),
                }
            };
            macro_host().answer(pid, Some(0)); // yes
        }
        let _ = res_rx.recv_timeout(Duration::from_secs(15)).expect("combo result");
        let logs = host.drain_log();
        assert!(has(&logs, "GO:a") && has(&logs, "GO:b"), "each yes-answered iteration notifies: {logs:?}");
        host.unregister("s_combo_fe_ask");
        eprintln!("[steps] COMBO ForEach+Ask ok");
    }

    eprintln!("[steps] ALL PHASES PASSED");
}
