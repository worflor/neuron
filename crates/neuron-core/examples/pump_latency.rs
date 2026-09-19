// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! PUMP LATENCY — the cross-thread hop from an injected input edge to a dispatched action, measured
//! against the REAL Win32 pump.
//!
//! ## What is real here, and what is not
//!
//! This is not a simulation of the dispatch path. It runs the actual [`neuron::controls::listen_until`]
//! pump on its own thread — real hidden message window, real Raw-Input registration, real
//! `MsgWaitForMultipleObjectsEx` blocking wait, real per-listener wake event — and publishes edges
//! through the actual [`neuron::controls::inject_event`], which is *the same function the Razer
//! macro-key reader calls*. The edge is then diffed by the real [`neuron::controls::HoldEdges`],
//! matched by the real [`neuron::engine::Engine`], and run by the real
//! [`neuron::executor::DispatchExecutor`].
//!
//! So `inject_hop` here is not a proxy for the macro-key hop — it IS the macro-key hop. What this
//! cannot cover is the leg before `inject_event`: USB transfer, the HID class driver, and the reader
//! thread's blocking read. Those need a physical key press, and Raw Input does not report synthesised
//! keystrokes as device events, so no harness can manufacture them. `hid_decode` and `raw_decode` stay
//! empty here on purpose rather than being faked.
//!
//! ## The experiment
//!
//! The pump's wake latency is a SCHEDULING property, so measuring it on an idle machine measures
//! nothing interesting — the interesting case is the one users report: a macro firing late while a
//! game has every core busy. So this runs a matrix:
//!
//! * **idle** vs **loaded** (`--load N` spins N CPU-burning threads at normal priority), and
//! * **boosted** vs **default** thread priority (`NEURON_INPUT_PRIORITY=default` opts out).
//!
//! That last axis is the point: it is the only way to show whether raising input-thread priority does
//! anything measurable, instead of asserting that it should.
//!
//! Run: `cargo run --release -p neuron --example pump_latency -- [--load 16] [--presses 400] [--arm]`

use neuron::controls::{self, ControlEvent, HoldEdges, InputEdge};
use neuron::engine::{Engine, Rule, Trigger};
use neuron::executor::{DispatchExecutor, IntentRunner};
use neuron::latency;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The macro-key control this harness presses: page = the Razer macro page, usage `0x20` = M1 —
/// byte-for-byte what `macrokeys::decode` injects for a real M1 press.
const M1_USAGE: u16 = 0x20;

/// The synthetic pid bucket the macro reader uses (`0xF000 | pid`), so the edge lands in the same
/// `HoldEdges` bucket a real macro key would.
const MACRO_PID: &str = "f042";

/// An undefined virtual key — carried through the input stream but mapped by no application, so an
/// armed run cannot type at the user or trigger a shortcut.
const VK_UNDEFINED_NOOP: &str = "0x07";

#[derive(Default)]
struct NullIntents;
impl IntentRunner for NullIntents {
    fn run_intent(&mut self, _i: &neuron::action::Intent) -> String {
        String::new()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str, default: usize| -> usize {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let arm = args.iter().any(|a| a == "--arm");
    let presses = flag("--presses", 400);
    let load = flag("--load", 0);
    // `--load-priority above` raises the CONTENDING threads to above-normal.
    //
    // This is the scenario the input-priority boost actually claims to fix, and it is not the same as
    // plain CPU load. Windows already grants a temporary priority boost to a thread waking from a wait,
    // so against normal-priority spinners a blocked pump competes fine whether or not its BASE priority
    // was raised — measured, and it does. The case where the base priority should matter is when the
    // competing work is ITSELF above normal (a game's own worker threads), because then an unboosted
    // pump is outranked even after its wake boost. Without this axis the experiment cannot distinguish
    // "the boost is unnecessary" from "the load model was too weak to need it".
    let load_above_normal = args
        .iter()
        .position(|a| a == "--load-priority")
        .and_then(|i| args.get(i + 1))
        .is_some_and(|v| v == "above");

    println!("neuron pump latency — the REAL listen_until pump, real inject_event, real engine");
    println!(
        "  presses={presses}  load={load} busy thread(s) at {} priority  input={}",
        if load_above_normal { "ABOVE-NORMAL" } else { "normal" },
        if arm { "ARMED (VK 0x07 only)" } else { "disarmed" }
    );
    println!(
        "  input-thread priority: {}\n",
        if std::env::var("NEURON_INPUT_PRIORITY").as_deref() == Ok("default") {
            "DEFAULT (NEURON_INPUT_PRIORITY=default)"
        } else {
            "BOOSTED (above-normal)"
        }
    );
    if arm {
        neuron::action::arm_input(true);
    }

    // Optional CPU contention at NORMAL priority — the stand-in for "a game is running". Started
    // before the pump so the pump has to compete for a core from the moment it arms.
    let stop_load = Arc::new(AtomicBool::new(false));
    let mut load_threads = Vec::new();
    for i in 0..load {
        let stop = stop_load.clone();
        if let Ok(h) = neuron::worker::spawn_named(&format!("probe-load-{i}"), move || {
            if load_above_normal {
                // Contend at the SAME level the input threads run at, so the pump's base priority is
                // no longer an advantage — the condition a real foreground game creates.
                //
                // Raised DIRECTLY rather than through `timing::boost_input_thread`, because that
                // honours `NEURON_INPUT_PRIORITY=default` — and in the unboosted arm of this
                // experiment that would silently drop the load threads to normal too, quietly turning
                // the decisive comparison into a no-op that looks like a result.
                raise_to_above_normal();
            }
            // A real spin: no sleeping, no yielding. Sleeping would leave the core free exactly when
            // the pump wants it, which is the opposite of the condition being reproduced.
            let mut x = 0u64;
            while !stop.load(Ordering::Relaxed) {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                std::hint::black_box(x);
            }
        }) {
            load_threads.push(h);
        }
    }
    if load > 0 {
        // Let the load threads actually get scheduled onto cores before measuring.
        std::thread::sleep(Duration::from_millis(300));
    }

    // ── the real pump, on its own thread ────────────────────────────────────────────────────────
    let engine = Engine::from_rules(vec![Rule::new(
        Trigger::Input {
            page: controls::RAZER_MACRO_PAGE,
            usage: M1_USAGE,
            pid: None, // macro binds are device-ANY, exactly as the real ones are
        },
        neuron::action::Action::Key {
            key: VK_UNDEFINED_NOOP.into(),
        },
    )]);
    let stop_pump = Arc::new(AtomicBool::new(false));
    let dispatched = Arc::new(AtomicU64::new(0));
    let ready = Arc::new(AtomicBool::new(false));
    let pump_boosted = Arc::new(AtomicBool::new(false));

    let pump = {
        let stop = stop_pump.clone();
        let dispatched = dispatched.clone();
        let ready = ready.clone();
        let pump_boosted = pump_boosted.clone();
        neuron::worker::spawn_named("probe-pump", move || {
            // The same posture the live dispatch worker takes.
            pump_boosted.store(neuron::timing::boost_input_thread(), Ordering::SeqCst);
            let edges = Mutex::new(HoldEdges::new());
            let exec = Mutex::new(DispatchExecutor::new());
            controls::listen_until(
                None,
                &stop,
                false, // resident posture: a stray ESC must not kill it
                |ev| {
                    // The real edge diff, then the real resolve + run. Locks are uncontended (only
                    // this thread touches them) — they exist because `listen_until` wants Fn closures.
                    let list = {
                        let _t = latency::start(&latency::EDGE_DIFF);
                        edges.lock().unwrap_or_else(std::sync::PoisonError::into_inner).edges(ev)
                    };
                    for edge in list {
                        if let InputEdge::Down(trigger) = edge {
                            let mut exec = exec.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                            if exec.fire(&engine, &trigger, &mut NullIntents).is_some() {
                                dispatched.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                },
                || {
                    ready.store(true, Ordering::SeqCst);
                    // The live worker's idle cadence. Deliberately LONG: it proves the wake event —
                    // not a timeout — is what returns the wait. If the hop were riding the cadence
                    // instead, `inject_hop` would read as hundreds of milliseconds.
                    Duration::from_secs(1)
                },
            );
        })
    };

    // Wait for the pump to arm (it must have registered its inject sink before the first press, or
    // early edges would land in the pre-registration buffer and not measure a real hop).
    let armed_at = Instant::now();
    while !ready.load(Ordering::SeqCst) && armed_at.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(5));
    }
    if !ready.load(Ordering::SeqCst) {
        eprintln!("pump did not arm within 5s — aborting");
        stop_pump.store(true, Ordering::SeqCst);
        return;
    }
    println!(
        "pump armed (priority {}); pressing M1 {presses}x\n",
        if pump_boosted.load(Ordering::SeqCst) { "boosted" } else { "default" }
    );

    // Everything before this is setup, not input latency.
    latency::reset_all();

    // ── press M1, `presses` times, as press+release pairs ───────────────────────────────────────
    for _ in 0..presses {
        inject(vec![(controls::RAZER_MACRO_PAGE, M1_USAGE)]); // down
        // Space the presses out so each one measures a hop from a genuinely BLOCKED pump — the real
        // condition. Back-to-back injects would find the pump already awake and mid-drain, which
        // measures queue throughput instead of wake latency.
        std::thread::sleep(Duration::from_millis(6));
        inject(Vec::new()); // all-released report → the Up edge
        std::thread::sleep(Duration::from_millis(6));
    }
    // Let the last edge land.
    std::thread::sleep(Duration::from_millis(200));

    stop_pump.store(true, Ordering::SeqCst);
    controls::wake_pump(); // don't wait out the 1s cadence for teardown
    if let Ok(h) = pump {
        let _ = h.join();
    }
    stop_load.store(true, Ordering::Relaxed);
    for h in load_threads {
        let _ = h.join();
    }

    let got = dispatched.load(Ordering::Relaxed);
    println!("{got}/{presses} presses dispatched an action");
    if got != presses as u64 {
        println!(
            "  NOTE: a shortfall means edges were lost or coalesced — the latency below only \
             describes the ones that arrived."
        );
    }
    println!("\n{}", latency::report());
    println!(
        "inject_hop = inject_event -> the pump drained it (channel + SetEvent + being scheduled).\n\
         This is the same hop a real Razer macro key takes. hid_decode/raw_decode are empty because\n\
         they need a physical device report, which no harness can synthesise."
    );
}

/// Raise the calling thread to above-normal, bypassing the `NEURON_INPUT_PRIORITY` opt-out. Used only
/// for this harness's contending load threads — see the call site for why it must not go through
/// `timing::boost_input_thread`.
#[cfg(windows)]
fn raise_to_above_normal() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: FFI. `GetCurrentThread` is a pseudo-handle needing no close, always valid for the
    // calling thread; `SetThreadPriority` only reads it.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
}

#[cfg(not(windows))]
fn raise_to_above_normal() {}

fn inject(hits: Vec<(u16, u16)>) {
    let raw = std::iter::once(0x04u8)
        .chain(hits.iter().map(|&(_, u)| u as u8))
        .collect();
    controls::inject_event(ControlEvent {
        // The deferred-button stream is a STREAM now, not a pid prefix (see `controls::Stream`).
        pid: u16::from_str_radix(MACRO_PID, 16)
            .ok()
            .map(neuron::registry::CanonicalPid::of),
        stream: controls::Stream::Deferred,
        hits,
        raw,
    });
}
