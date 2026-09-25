// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! LATENCY PROBE — measure the dispatch chain's software leg on this machine, with no hardware.
//!
//! The physical leg of a keypress (firmware → USB → HID class driver) is not ours and not visible
//! from user mode. Everything *after* it is, and this probe drives exactly that: it builds a real
//! [`Engine`] with representative bindings, fires real triggers through the real
//! [`DispatchExecutor`], and prints [`neuron::latency`]'s per-stage percentiles.
//!
//! ## Safety: it does not type at you
//!
//! By default the process-spawn / input arm gate is left DISARMED, so `SendInput` is never called —
//! the probe measures resolve/context/dispatch/spawn cost without pushing keystrokes into whatever
//! window happens to be focused. `--arm` opts into measuring the real output path too, and even then
//! it only ever emits VK `0x07`, an UNDEFINED virtual key no application acts on, so an armed run
//! still cannot type into a document or trigger a shortcut.
//!
//! ## What it answers
//!
//! * Is the per-press dispatch cost (resolve → context → run) microseconds or milliseconds?
//! * What does a macro sequence's thread handoff cost before its first step can fire?
//! * **Is macro step timing honest?** A plain `Sleep(2)` is quantised to the system timer — the
//!   `sleep_error` row is that overshoot, measured rather than assumed.
//!
//! Run: `cargo run -p neuron --example latency_probe [-- --arm] [--iters N]`

use neuron::action::{Action, ScriptKind, ScriptRef, Step};
use neuron::engine::{Engine, Rule, Trigger};
use neuron::executor::{DispatchExecutor, IntentRunner};
use neuron::latency;
use std::time::{Duration, Instant};

/// A no-op intent runner: the probe measures the DISPATCH chain, and device intents (DPI, profile
/// switching) would otherwise reach real hardware. Counting them proves they were routed.
#[derive(Default)]
struct CountingIntents {
    count: usize,
}

impl IntentRunner for CountingIntents {
    fn run_intent(&mut self, _intent: &neuron::action::Intent) -> String {
        self.count += 1;
        String::new()
    }
}

/// An undefined virtual key. Windows will carry it through the input stream, but no application maps
/// it, so an armed probe cannot type text or fire a shortcut at the user.
const VK_UNDEFINED_NOOP: &str = "0x07";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arm = args.iter().any(|a| a == "--arm");
    let iters: usize = args
        .iter()
        .position(|a| a == "--iters")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);

    println!("neuron latency probe — {iters} iterations per case");
    println!(
        "output path: {}\n",
        if arm {
            "ARMED (SendInput live, emitting only the undefined VK 0x07)"
        } else {
            "disarmed (no SendInput; use --arm to measure the output leg too)"
        }
    );
    if arm {
        neuron::action::arm_input(true);
    }

    // ── case 1: a single-key binding, the simplest and most common shape ────────────────────────
    let key = Trigger::Input {
        page: 0x0C,
        usage: 0xE9,
        pid: None,
    };
    let single = Engine::from_rules(vec![Rule::new(
        key.clone(),
        Action::Key {
            key: VK_UNDEFINED_NOOP.into(),
        },
    )]);
    latency::reset_all();
    let mut exec = DispatchExecutor::new();
    let mut intents = CountingIntents::default();
    let wall = Instant::now();
    for _ in 0..iters {
        // Wrap each fire the way the pump does, so `press_to_output` is measured end to end.
        latency::with_edge(Instant::now(), || {
            exec.fire(&single, &key, &mut intents);
        });
    }
    report("CASE 1 — single key binding", iters, wall.elapsed());

    // ── case 2: a many-rule engine, to see whether matching cost scales into being felt ─────────
    // 400 rules is far past any real config; if resolve is still microseconds here, rule count is
    // provably not where a delay comes from, and we can stop suspecting it.
    let mut rules = vec![Rule::new(
        key.clone(),
        Action::Key {
            key: VK_UNDEFINED_NOOP.into(),
        },
    )];
    for i in 0..400u16 {
        rules.push(Rule::new(
            Trigger::Input {
                page: 0xFF00,
                usage: i,
                pid: None,
            },
            Action::Noop,
        ));
    }
    let big = Engine::from_rules(rules);
    latency::reset_all();
    let wall = Instant::now();
    for _ in 0..iters {
        latency::with_edge(Instant::now(), || {
            exec.fire(&big, &key, &mut intents);
        });
    }
    report("CASE 2 — 401-rule engine", iters, wall.elapsed());

    // ── case 3: a context-needing action, to price Context::capture ─────────────────────────────
    // A Script action forces the foreground-window + process-image + clipboard probe. Shell kind so
    // nothing reaches the Python sidecar, and the spawn gate stays disarmed so nothing is launched.
    let ctx_engine = Engine::from_rules(vec![Rule::new(
        key.clone(),
        Action::Script {
            script: ScriptRef {
                id: "rem neuron latency probe".into(),
                kind: ScriptKind::Shell,
            },
        },
    )]);
    latency::reset_all();
    // Fewer iterations: this one opens the clipboard every pass, and hammering it thousands of times
    // would contend with whatever else on the machine wants it (which is itself the finding).
    let ctx_iters = iters.min(300);
    let wall = Instant::now();
    for _ in 0..ctx_iters {
        latency::with_edge(Instant::now(), || {
            exec.fire(&ctx_engine, &key, &mut intents);
        });
    }
    report("CASE 3 — context-capturing action", ctx_iters, wall.elapsed());

    // ── case 4: a macro sequence — the shape the user actually reports as laggy ─────────────────
    let macro_engine = Engine::from_rules(vec![Rule::new(
        key.clone(),
        Action::Sequence {
            steps: vec![
                step(VK_UNDEFINED_NOOP, 0, 0),
                step(VK_UNDEFINED_NOOP, 2, 0), // a 2ms inter-step delay: the fidelity question
                step(VK_UNDEFINED_NOOP, 0, 0),
            ],
        },
    )]);
    latency::reset_all();
    // Sequences spawn a worker each, so keep the count modest — thousands of concurrent macro
    // threads would measure thread-pool thrash rather than dispatch latency.
    let macro_iters = iters.min(200);
    let wall = Instant::now();
    for _ in 0..macro_iters {
        latency::with_edge(Instant::now(), || {
            exec.fire(&macro_engine, &key, &mut intents);
        });
        // Let each macro finish before starting the next, so the steps' sleeps are measured on an
        // uncontended machine (a pile-up would inflate `sleep_error` for the wrong reason).
        std::thread::sleep(Duration::from_millis(12));
    }
    // The last macro's worker may still be mid-flight; give it room to land before reporting.
    std::thread::sleep(Duration::from_millis(60));
    report("CASE 4 — 3-step macro sequence (2ms inter-step delay)", macro_iters, wall.elapsed());

    // ── case 5: the raw timer-resolution question, isolated from everything else ────────────────
    // A macro's inter-step pause is the only place the app deliberately sleeps on the user's behalf,
    // so its accuracy IS macro fidelity. Measured directly here so the number can't be blamed on
    // dispatch overhead.
    // Macro step timing fidelity: requested vs actual, for the pauses a macro author writes.
    //
    // This deliberately does NOT compare `sleep_precise` against `std::thread::sleep`. It used to,
    // back when `sleep_precise` had its own Win32 high-resolution timer — the comparison is what
    // proved that timer redundant (Rust 1.75+ already uses one) and got it deleted. Now that
    // `sleep_precise` simply delegates, printing the two side by side would be measuring one function
    // against itself and dressing up the noise between two runs as a result.
    //
    // What still matters is the absolute number: if a 2ms pause ever starts taking ~15.6ms, macro
    // timing has regressed to the scheduler quantum, and that is visible right here.
    println!("CASE 5 — macro step timing fidelity (requested vs actual)");
    println!("  a pause landing near ~15.6ms would mean the scheduler-tick quantum is back");
    for req_ms in [1u64, 2, 5, 10, 16] {
        let want = Duration::from_millis(req_ms);
        let n = 20;
        let mut worst = Duration::ZERO;
        let mut total = Duration::ZERO;
        for _ in 0..n {
            let t = Instant::now();
            neuron::timing::sleep_precise(want);
            let took = t.elapsed();
            total += took;
            worst = worst.max(took);
        }
        let mean = total.as_secs_f64() * 1000.0 / f64::from(n);
        println!(
            "  asked {req_ms:>2}ms  ->  mean {mean:>6.2}ms  (over by {:>5.2}ms)   worst {:>6.2}ms",
            mean - req_ms as f64,
            worst.as_secs_f64() * 1000.0,
        );
    }
    println!();
    println!("(sleep_error across every step above)");
    println!("{}", latency::report());

    // ── case 8: the SHELL / RUN tier — a process spawn, on the dispatch thread ──────────────────
    // `Action::Run` and `Script{Shell}` reach the OS through a `CreateProcess`, and that happens
    // inline on the thread that services every other device edge. Process creation is milliseconds,
    // not microseconds, so if this is large it stalls all input for its duration — a very different
    // problem from the macro-sequence path, and one no other case here would reveal.
    // Only measured when armed, because measuring it means really launching processes.
    if arm {
        let run_engine = Engine::from_rules(vec![Rule::new(
            key.clone(),
            // A do-nothing command: this prices the SPAWN, not whatever the command would do.
            Action::Run {
                cmd: "cmd /c exit".into(),
            },
        )]);
        latency::reset_all();
        let run_iters = 30; // real processes — enough to see the distribution, few enough to be polite
        let wall = Instant::now();
        for _ in 0..run_iters {
            latency::with_edge(Instant::now(), || {
                exec.fire(&run_engine, &key, &mut intents);
            });
        }
        report("CASE 8 — shell/run tier (a real process spawn)", run_iters, wall.elapsed());
    }

    if arm {
        // These cases emit input. Keep them behind the explicit --arm opt-in.
        hook_cost_case();
    }

    println!("intents routed: {} (device work stayed out of the measurement)", intents.count);
}

/// CASE 6 — WHY `SendInput` costs milliseconds instead of microseconds.
///
/// `SendInput` must deliver each synthesised event through EVERY low-level keyboard hook installed on
/// the desktop, and each one is a synchronous call into the hook owner's thread — in another process.
/// So the cost of an injected keystroke is set by how many LL hooks exist system-wide, which includes
/// the ones neuron-app installs itself (gaming-mode suppression, the device remap shim).
///
/// This measures that directly: `SendInput` as-is, then again with ONE extra LL keyboard hook that
/// this probe installs on itself. The delta is the per-hook toll. It is measured this way rather than
/// by stopping the running app, so the user's live setup is never disturbed to take the reading.
#[cfg(windows)]
fn hook_cost_case() {
    assert!(neuron::action::input_armed(), "output probe requires --arm");
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, WH_KEYBOARD_LL,
    };

    // A do-nothing hook: it adds NO work of its own, so whatever cost appears is purely the price of
    // the extra cross-process hop. That is the point — the toll is structural, not our proc's fault.
    unsafe extern "system" fn noop_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, w, l) }
    }

    let measure = |label: &str| {
        latency::SEND_INPUT.reset();
        for _ in 0..200 {
            let _ = Action::Key {
                key: VK_UNDEFINED_NOOP.into(),
            }
            .run();
        }
        println!(
            "  {label:<28} mean {:>7} p50 {:>7} p90 {:>7} p99 {:>7} max {:>7}",
            fmt(latency::SEND_INPUT.mean_us()),
            fmt(latency::SEND_INPUT.quantile_us(0.50)),
            fmt(latency::SEND_INPUT.quantile_us(0.90)),
            fmt(latency::SEND_INPUT.quantile_us(0.99)),
            fmt(latency::SEND_INPUT.max_us()),
        );
    };

    println!("CASE 6 — SendInput cost vs the number of low-level keyboard hooks on the desktop");
    measure("as-is (existing hooks)");
    // SAFETY: a standard LL keyboard-hook install with a valid extern "system" proc, unhooked below
    // on the same thread that installed it. This thread pumps no messages, which is itself part of
    // the finding: a hook whose owner is not pumping is the WORST case for everyone else's SendInput.
    let extra: HHOOK = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(noop_hook), std::ptr::null_mut(), 0) };
    if extra.is_null() {
        println!("  (could not install the extra hook — no comparison available)");
        return;
    }
    measure("+1 extra no-op LL hook");
    unsafe { UnhookWindowsHookEx(extra) };
    measure("back to as-is (unhooked)");
    println!();

    // ── CASE 7: is the cost PER CALL or PER EVENT? ──────────────────────────────────────────────
    // This is the measurement that decides the fix. If the cost is per CALL, then packing a chord or a
    // run of keystrokes into ONE `SendInput` is a large win and worth restructuring for. If it is per
    // EVENT, batching buys nothing and the effort belongs elsewhere. Also included: a zero-event call,
    // which isolates raw syscall overhead from the cost of actually delivering an event.
    println!("CASE 7 — is SendInput's cost per CALL or per EVENT?");
    let batch = 10;
    let reps = 100;

    let t = Instant::now();
    for _ in 0..reps {
        // SAFETY: a well-formed zero-count SendInput — no events are delivered; this prices the
        // syscall boundary alone.
        unsafe { send_raw_noop() };
    }
    println!(
        "  empty call (0 events)         {:>8.1}us per call",
        t.elapsed().as_secs_f64() * 1e6 / reps as f64
    );

    latency::SEND_INPUT.reset();
    let t = Instant::now();
    for _ in 0..reps {
        for _ in 0..batch {
            let _ = Action::Key {
                key: VK_UNDEFINED_NOOP.into(),
            }
            .run(); // one SendInput per keystroke (2 events each)
        }
    }
    let separate = t.elapsed();
    println!(
        "  {batch} keystrokes, {batch} calls        {:>8.1}us per keystroke",
        separate.as_secs_f64() * 1e6 / (reps * batch) as f64
    );

    let t = Instant::now();
    for _ in 0..reps {
        send_armed_batch(batch);
    }
    let batched = t.elapsed();
    println!(
        "  {batch} keystrokes, 1 call          {:>8.1}us per keystroke",
        batched.as_secs_f64() * 1e6 / (reps * batch) as f64
    );
    println!(
        "  => batching is {:.1}x cheaper per keystroke.\n",
        separate.as_secs_f64() / batched.as_secs_f64().max(f64::EPSILON)
    );
}

/// A zero-event `SendInput`, to price the syscall boundary with no delivery.
#[cfg(windows)]
unsafe fn send_raw_noop() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT};
    unsafe { SendInput(0, std::ptr::null(), std::mem::size_of::<INPUT>() as i32) };
}

/// `n` full keystrokes (down+up of the undefined VK) delivered through the armed output gate.
#[cfg(windows)]
fn send_armed_batch(n: usize) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };
    let mk = |up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: 0x07,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut seq = Vec::with_capacity(n * 2);
    for _ in 0..n {
        seq.push(mk(false));
        seq.push(mk(true));
    }
    assert_eq!(neuron::action::send_win_input(&seq), seq.len() as u32,
        "the output probe did not deliver every event; its timing would be invalid");
}

#[cfg(not(windows))]
fn hook_cost_case() {}

#[cfg(windows)]
fn fmt(us: u64) -> String {
    if us < 1_000 {
        format!("{us}us")
    } else {
        format!("{:.1}ms", us as f64 / 1_000.0)
    }
}

fn step(key: &str, delay_ms: u32, hold_ms: u32) -> Step {
    Step {
        action: Box::new(Action::Key { key: key.into() }),
        delay_ms,
        hold_ms,
    }
}

fn report(title: &str, iters: usize, wall: Duration) {
    println!("{title}  ({iters} fires in {:.1}ms wall)", wall.as_secs_f64() * 1000.0);
    println!("{}", latency::report());
}
