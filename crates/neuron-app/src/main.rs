//! Neuron resident app — the ONE long-lived process. Tray-resident: the Slint event loop runs
//! with NO window shown; the main window is built once before the event loop, may start hidden,
//! and is hidden on close while its handle/glue are retained. The tray menu
//! is the 90% surface; global hotkeys drive the quick toggles. Everything talks to neuron-core
//! through direct typed Rust calls — no IPC.
//!
//! Architecture:
//!   * `slint::run_event_loop()` keeps the process alive even with zero windows.
//!   * a Slint `Timer` on the UI thread drains tray + hotkey events (their crossbeam channels)
//!     and keeps the cheap truths honest (tray menu, ARMED pill, status-line freshness).
//!   * `glue` binds every `State` callback to the `Runtime` and refreshes the view models.
//!   * the `Runtime` lives behind the retained window; tray quick-actions act through the
//!     same `State` callbacks the UI uses — without yanking the window over the user's game.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod beacon;
mod capture;
mod control;
mod dialweave;
mod dispatch;
mod editor;
mod filedlg;
mod flight;
mod glance;
mod glue;
mod hidwatch;
mod host;
mod knockback;
mod macrokeys;
mod mic;
mod migrate;
mod notifs;
mod overlay;
mod prefs;
mod prof_log;
#[cfg(windows)]
mod purge;
mod runtime;
mod sound;
mod strokelab;
mod surface;
mod teleport;
mod tray;
mod ui;
mod weave;
mod whiteboard;
mod wm;

#[cfg(test)]
mod apptest;

#[cfg(test)]
mod testsupport;

use slint::ComponentHandle;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use tray::{Tray, TrayAction, TraySnapshot};
use ui::{AppWindow, State};

/// Holds the retained window + its installed glue. Close hides the window; Open shows it again.
/// The LIVE dispatch runtime lives here too — it is created once, on the real app-run path, and
/// survives window close/reopen (the device-event loop must keep firing remaps even with no window).
struct Resident {
    window: Option<AppWindow>,
    shared: Option<glue::SharedRt>,
    live: Option<dispatch::LiveRuntime>,
}

fn main() {
    // Pin the run directory to the exe's folder FIRST. Every Neuron config path is
    // run-directory-relative (bindings.toml, profiles/, gestures.json, app.toml, …) and an HKCU
    // Run autostart launches with cwd=C:\Windows\System32 — without the pin, an autostart boot
    // loaded EMPTY config and scattered saves into System32.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let _ = std::env::set_current_dir(dir);
        }
    }

    // PROFILER (inert unless NEURON_PROFILE is set): 1 Hz hot-path counters + per-thread/sidecar
    // CPU to neuron_profile.log, so a "never stops" spin localizes to a counter or a thread.
    prof_log::start();

    // SELF-TEST HARNESS: `neuron-app --weave-proof` renders every spellweaving material as actual ink
    // STROKES (a flowing stroke, a ring, a straight stroke) to PNGs in the run dir, then exits — so the
    // materials can be judged in their real ink form (the gallery only shows metaballs). No GUI/COM.
    if std::env::args().any(|a| a == "--weave-proof") {
        weave::write_proof_sheets();
        return;
    }

    // SYNAPSE PURGE (elevated arm): the settings button relaunches us with `runas` + this flag when
    // it isn't already admin — Razer's services run as SYSTEM. This instance stops every Razer
    // service (so they can't respawn helpers) then terminates every Synapse process, then exits. No
    // GUI/COM/tray on this path.
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--purge-synapse") {
        purge::run_purge_and_log();
        return;
    }

    // SYNAPSE SCAN (non-destructive preview): enumerate the rat process tree + Razer services that a
    // purge WOULD evict, write them to `neuron-synapse-scan.log`, and exit. Kills nothing — used to
    // verify detection on a live machine. No GUI/COM/tray on this path.
    #[cfg(windows)]
    if std::env::args().any(|a| a == "--scan-synapse") {
        purge::scan_and_log();
        return;
    }

    // CRASH LOGGER: capture every Rust panic (message + location + backtrace) to a file in the
    // run dir, plus the FLIGHT RECORDER's story of what led up to it, then chain to the default
    // hook. (Release builds unwind — see the workspace profile — so Drop guards run and
    // catch_unwind shells can contain a fault instead of the process dying.)
    {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("neuron-crash.log")
            {
                let loc = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "?".into());
                let msg = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic>".into());
                let _ = writeln!(
                    f,
                    "[panic] {loc}\n        {msg}\n{}",
                    std::backtrace::Backtrace::force_capture()
                );
                flight::dump(&mut f);
            }
            default(info);
        }));
    }

    // ── NATIVE-FAULT BREADCRUMB + OS-LEVEL PHOENIX ────────────────────────
    // A native fault (an access violation, or a fail-fast raised inside an OS DLL — seen live:
    // CoreMessaging.dll 0xE0464645 on tray-open) never reaches the Rust panic hook: the process
    // just vanishes. Two answers, both at the OS layer:
    //   1. an UNHANDLED-EXCEPTION FILTER writes the exception code + address + the flight
    //      recorder's story to neuron-crash.log, then lets WER proceed — native deaths become
    //      diagnosable from OUR log, not just Event Viewer;
    //   2. RegisterApplicationRestart asks WINDOWS ITSELF to relaunch us after a crash or hang
    //      (the same mechanism browsers/Office use). The OS enforces the anti-crash-loop rule
    //      (only fires after 60s of uptime), we pass `--tray --respawned` so the relaunch comes
    //      back quietly and says what happened. A fault in a DLL we don't own becomes a blip:
    //      the tray icon returns, configs reload, live dispatch re-arms.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
        };
        use windows_sys::Win32::System::Recovery::RegisterApplicationRestart;

        unsafe extern "system" fn seh_breadcrumb(info: *const EXCEPTION_POINTERS) -> i32 {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open("neuron-crash.log")
            {
                let (code, addr) = unsafe {
                    if !info.is_null() && !(*info).ExceptionRecord.is_null() {
                        let r = &*(*info).ExceptionRecord;
                        (r.ExceptionCode, r.ExceptionAddress as usize)
                    } else {
                        (0, 0)
                    }
                };
                let _ = writeln!(f, "[native fault] code={code:#010x} addr={addr:#x}");
                flight::dump(&mut f);
            }
            0 // EXCEPTION_CONTINUE_SEARCH — WER (and the restart registration) still run
        }
        SetUnhandledExceptionFilter(Some(seh_breadcrumb));

        // PHOENIX — RESTART_NO_PATCH (4) | RESTART_NO_REBOOT (8): respawn on crash/hang only.
        // User-disablable (SYSTEM → RELIABILITY); the registration is a one-shot at startup, so the
        // preference is read here and takes effect from the next launch.
        if prefs::phoenix() {
            let cmd: Vec<u16> = "--tray --respawned\0".encode_utf16().collect();
            let _ = RegisterApplicationRestart(cmd.as_ptr(), 4 | 8);
        }
    }
    flight::trace("life", "app start", 0);

    // ── POWER-THROTTLING OPT-OUT (Win11 background QoS) ───────────────────
    // When a fullscreen game has focus, Windows puts unfocused processes on
    // EcoQoS (efficiency cores, reduced speed) and — on Win11 — IGNORES their
    // timer-resolution requests, coarsening every `thread::sleep` to ~15.6ms.
    // This app IS a background process whose whole job is real-time while a
    // game runs: input dispatch, macro fire, and the paced device-lighting
    // writers all live on millisecond sleeps. Without this opt-out the host
    // writer blows its 33ms frame deadlines mid-game and lighting visibly
    // drops frames ("laggy in Overwatch, fine on the desktop" — diagnosed
    // live). ControlMask names both policies, StateMask 0 DISABLES them:
    // full-speed scheduling + honored timer resolution, game or no game. The
    // deliberate trade is a slightly less-eco idle; a resident input daemon
    // earns it.
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, SetProcessInformation, ProcessPowerThrottling,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        };
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED
                | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
            StateMask: 0,
        };
        let _ = SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }

    // Establish the main thread's COM apartment as STA up front. winit (drag-and-drop / OLE)
    // requires STA; tray-icon and neuron-core's Core-Audio path both request MTA, and whichever
    // initializes COM first wins the apartment. Doing STA here means winit is satisfied, and the
    // later MTA requests return RPC_E_CHANGED_MODE (ignored by those crates) yet still function.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Ole::OleInitialize(std::ptr::null_mut());
    }

    let resident = Rc::new(RefCell::new(Resident {
        window: None,
        shared: None,
        live: None,
    }));

    // ── RENDERER SELECTION (prefer GPU, fall back to software) ────────────
    // PREFER femtovg (GPU): it's why the UI feels instant on a real GPU — and the user's machine has
    // one, so this is the path that's taken there. But a VM / RDP / headless / bad-driver host has no
    // usable OpenGL context, and a GPU-only build can't open its window AT ALL there. So select the
    // backend BEFORE the first `AppWindow::new()`: ask for femtovg, and if that selection fails, fall
    // back to the software renderer so the UI ALWAYS opens (slower, but it opens). `select()` sets the
    // platform on success and does NOT on failure, so the software retry is safe to call afterwards.
    // (Belt-and-suspenders: even when femtovg is selected here, the winit backend itself auto-falls-
    // back to software at WINDOW-creation time if GL init then fails — that's why renderer-software is
    // compiled in. This explicit selection additionally covers an event-loop/init failure at select.)
    select_renderer_backend();

    // Bring up the PROTOCOL HOST (if the CONNECTIONS pref opts in — default OFF)
    // before the window builds: `build_window` runs `restore_lighting`, and when
    // the host is active that saved lighting must flow through the arbiter (as
    // the animated base layer) rather than start an app-owned stream. This is
    // also what serves Chroma (54235, games) + OpenRGB (6742, tools). The SYSTEM
    // → CONNECTIONS card toggles it at runtime; `NEURON_HOST` env overrides for
    // dev. Best-effort: if it can't come up, the app streams lighting itself.
    host::start();

    // Build the window eagerly — WITHOUT showing it — so the LIVE dispatch loop has a stable
    // handle to post status into (the software renderer makes a hidden window's idle cost
    // negligible). Whether it shows is decided below, so a `--tray` boot never flashes a frame.
    build_window(&resident);

    // Seed the tray from the real resident runtime instead of a throwaway config load. The window is
    // still hidden, so tray-start remains headless while startup avoids duplicate runtime IO.
    let (profiles, effects, active, paused) = {
        let r = resident.borrow();
        let shared = r.shared.as_ref().expect("window built eagerly above");
        let s = shared.borrow();
        (
            s.rt.profiles
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>(),
            s.rt.effects()
                .into_iter()
                .map(|(n, _, _)| n)
                .collect::<Vec<_>>(),
            s.rt.active_profile.clone(),
            neuron::writes::writes_paused(),
        )
    };
    let tray = Rc::new(Tray::build(&profiles, &effects, &active, paused, false));

    // Hidden start is an AUTOSTART behaviour: `--tray` (the Run-key launch) honours the persisted
    // start-minimized preference. A deliberate double-click ALWAYS shows the window — a brand-new
    // user must never be greeted by nothing but a tray icon they don't know exists.
    let start_hidden = std::env::args().any(|a| a == "--tray") && prefs::start_minimized();
    // An OS-relaunch after a crash announces itself honestly (and lands in the flight record).
    let respawned = std::env::args().any(|a| a == "--respawned");
    if respawned {
        flight::trace("life", "respawned after a crash", 0);
    }

    // ── START LIVE DISPATCH (the headline) ────────────────────────────────
    // The device-event runtime starts here, on the REAL app-run path only (never from a test —
    // tests construct State + glue but never reach main). It ARMS input by default so GUI-bound
    // remaps fire out of the box; the Settings SAFE-MODE toggle is the disarm switch. `--safe`
    // starts disarmed (observe/dry-run). The loop builds the unified Engine, installs the GamingMode
    // hook, and posts last-trigger/active-layer back to the UI.
    let safe = std::env::args().any(|a| a == "--safe");
    let armed = !safe;
    // Build the weak handle + set the view inside a SHORT borrow, then start the worker and store it
    // in a SEPARATE borrow (the worker must not be created while a borrow of `resident` is held).
    let weak = {
        let r = resident.borrow();
        let app = r.window.as_ref().expect("window built eagerly above");
        let st = app.global::<State>();
        st.set_input_armed(armed);
        st.set_runtime_active(cfg!(windows));
        st.set_status_line(
            if !cfg!(windows) {
                "live dispatch unavailable on this platform (device backend not implemented)"
            } else if armed {
                "live dispatch ARMED - GUI remaps fire (toggle safe-mode in Settings to disarm)"
            } else {
                "live dispatch running - SAFE MODE (observe/dry-run, nothing injected)"
            }
            .into(),
        );
        app.as_weak()
    };
    // Set the macro gate's atomic BEFORE the live runtime can route a single fire, so a python
    // macro triggered on the very first frame already sees the correct arm state (no startup window
    // where a fire could slip through against the intended gate). This only sets an atomic + a
    // non-blocking frame; the sidecar itself is warmed below, off-thread.
    neuron::macros::macro_host().set_armed(armed);

    let live = dispatch::LiveRuntime::start(weak.clone(), armed);
    resident.borrow_mut().live = Some(live);

    // ── WARM THE MACRO HOST (off the UI thread) ───────────────────────
    // Spawn the bundled-CPython sidecar once and register every macros/scripts/*.py into it, so a
    // triggered python macro fires warm (no per-press spawn/import). The input path + hardware
    // control are live immediately regardless; the macro tier just becomes ready a beat later.
    std::thread::spawn(|| {
        let _ = neuron::macros::macro_host().ensure_warm();
    });

    // ── NOTIFICATION ENGINE ───────────────────────────────────────────
    // Register the confirmation sink (the core's structural one-way door) and hand its receiver to a
    // dedicated engine thread that owns its OWN overlay window. State-change confirmations from the
    // verified commit points (DPI / profile / polling / brightness / layer) flow here; the engine
    // gates them on the live prefs and renders the chosen card. The CORE model stays change-only (no
    // "announce" path in `confirm`); the ONE app-layer exception is a macro's own `neuron.notify()`
    // (the user's code talking), posted as a Kind::Macro card via `notifs::post_macro` onto this same
    // surface — the notif unification: everything informational is a card, only the ask wheel (which
    // you answer) stays a separate radial.
    {
        // ONE pipeline: the engine drains a single `Note` channel carrying confirmations, macro
        // notifies, AND the beacon's ask announcement (only the answer wheel stays a separate
        // surface). `confirm::set_sink` stays PURE — it still speaks `Confirmation`; a tiny forwarder
        // thread maps each into a `Note::Confirm`, so the core never learns the engine's enum.
        let (note_tx, note_rx) = std::sync::mpsc::channel::<notifs::Note>();
        notifs::set_note_sink(note_tx.clone()); // backs post_macro / post_ask / clear_ask
        let (conf_tx, conf_rx) = std::sync::mpsc::channel::<neuron::confirm::Confirmation>();
        neuron::confirm::set_sink(Some(conf_tx));
        std::thread::spawn(move || {
            while let Ok(c) = conf_rx.recv() {
                if note_tx.send(notifs::Note::Confirm(c)).is_err() {
                    break; // the engine went away
                }
            }
        });
        std::thread::spawn(move || notifs::run(note_rx));
    }

    // ── HID EVENT LISTENER ────────────────────────────────────────────────
    // Listen for the device-pushed reports Synapse reads (onboard DPI/scroll-stage button → "now X")
    // and turn them into confirmations — so onboard changes earn a Neuron card, event-driven, no poll.
    // (NEURON_HIDWATCH=1 also dumps raw reports for decoding new devices.)
    hidwatch::start();

    // ── MACRO KEYS ────────────────────────────────────────────────────────
    // Put Razer keyboards into Driver Mode and read their vendor macro-key report (id 0x04), injecting
    // each held key as a bindable control — so dedicated macro keys (M1..M5/FN/…) work like any other.
    // Capability-driven + emergent: any Razer keyboard, however many macro keys (see macrokeys.rs).
    macrokeys::start();

    // ── CURTAIN LOOK ──────────────────────────────────────────────────
    // Teach the core curtain (the panic privacy screen) how to paint itself: the user's LIVE weave
    // material as procedural "signal" static. Core owns the mechanism (window / input / timing); this
    // hook owns the look, reading `live_material()` per frame so any material — current or future —
    // just works. Without this (CLI / tests) the curtain falls back to plain black.
    neuron::curtain::set_painter(Box::new(|f| {
        weave::material_static_bgra(&weave::live_material(), f.w, f.h, f.t, f.intensity)
    }));

    // WEAVE/BEACON service — the ONE owner of the cast trigger. While idle it is the LIVE
    // SPELLWEAVING watcher (hold trigger + flick/draw -> resolved through the cast engine ->
    // injected into the live dispatch Engine); when a macro asks (neuron.ask) the same thread
    // presents the binary radial instead (green=yes / red=no / vertical=pass). Live-path only,
    // like the dispatch runtime — tests never start it.
    beacon::start(weak);

    if !start_hidden {
        show_window(&resident);
    }

    // Pump tray + hotkey events from the event loop, and keep the cheap truths honest each tick:
    // the LINK lamp, the ARMED pill (the gate is a process-global others can flip), the tray menu
    // (profiles/effects/checks), and the status line's freshness. A 60ms cadence is invisibly
    // responsive and keeps idle cost near zero (true to the anti-bloat motto).
    let poll_tray = tray.clone();
    let poll_res = resident.clone();
    let timer = slint::Timer::default();
    // status-line freshness: (last text, when it appeared) — a readout that doesn't age is a lie.
    let status_seen: RefCell<(String, Instant)> = RefCell::new((String::new(), Instant::now()));
    // organ-stall surfacing: the name of the organ whose stall we've ALREADY dumped (so we dump
    // once per episode and never flood the flight ring that holds the diagnosis). None = armed.
    let stall_name: RefCell<Option<String>> = RefCell::new(None);
    // RELIABILITY panel freshness: refresh the uptime/heartbeats/crash readout ~1s (not every 60ms).
    let reliab_seen: RefCell<Instant> = RefCell::new(Instant::now() - Duration::from_secs(2));
    let slow_seen: RefCell<Instant> = RefCell::new(Instant::now() - Duration::from_secs(2));
    // CONNECTIONS event pokes: this consumer's last-seen host-bus stamp (each poller owns one so
    // two consumers can't swallow each other's wake-up).
    let host_events_seen: std::cell::Cell<u64> = std::cell::Cell::new(0);
    let tray_seen: RefCell<Instant> = RefCell::new(Instant::now() - Duration::from_secs(2));
    let status_tick_seen: RefCell<Instant> = RefCell::new(Instant::now() - Duration::from_secs(2));
    // VITALS pump edge-tracker: true once a `vitals` layer is live, so we FORCE a prompt read on the
    // rising edge (the surface just lit) and merely throttle-refresh after. false = no vitals surface up.
    let vitals_seen: RefCell<bool> = RefCell::new(false);
    // …and a ~1s steady-state gate so the pump enumerates at most ~1Hz while a vitals surface is up.
    let vitals_pump_seen: RefCell<Instant> = RefCell::new(Instant::now() - Duration::from_secs(2));
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(60),
        move || {
            // ── ARMORED TICK ── a panic in this closure would unwind into winit's FFI and take
            // the whole event loop with it. Contain it: the fault is logged (panic hook + flight
            // ring) and the NEXT tick runs anyway — the heartbeat of the app must not be the
            // app's weakest point.
            let tick = std::panic::AssertUnwindSafe(|| {
                let mut acted = false;
                for action in poll_tray.poll() {
                    acted = true;
                    handle_tray(&poll_res, action);
                }
                let r = poll_res.borrow();
                let (Some(app), Some(shared)) = (r.window.as_ref(), r.shared.as_ref()) else {
                    return;
                };
                let st = app.global::<State>();
                let now = Instant::now();
                let slow_due = {
                    let mut seen = slow_seen.borrow_mut();
                    if acted || seen.elapsed() >= Duration::from_millis(500) {
                        *seen = now;
                        true
                    } else {
                        false
                    }
                };
                let status_due = {
                    let mut seen = status_tick_seen.borrow_mut();
                    if acted || seen.elapsed() >= Duration::from_millis(250) {
                        *seen = now;
                        true
                    } else {
                        false
                    }
                };
                let tray_due = {
                    let mut seen = tray_seen.borrow_mut();
                    if acted || seen.elapsed() >= Duration::from_millis(250) {
                        *seen = now;
                        true
                    } else {
                        false
                    }
                };

                // POCKETS: if a portable-clipboard moved (the live dispatch worker fired a `pocket`
                // action), rebuild the strip's sigils. Generation-gated, so this is a cheap counter read
                // on the overwhelming majority of ticks where nothing changed.
                crate::glue::refresh_pockets_if_changed(app);

                // a respawned boot says so, once the window exists to say it on.
                if respawned && st.get_status_line().is_empty() {
                    st.set_status_line(
                        "recovered from a crash \u{2014} the story is in neuron-crash.log".into(),
                    );
                    st.set_status_kind("err".into());
                }

                // ORGAN WATCH: a worker thread that stopped beating is surfaced while the app lives —
                // "the weave engine doesn't seem to be working" must never be a mystery again. CRITICAL:
                // dump + trace EXACTLY ONCE per stall episode (when the stalled-organ name first appears).
                // The old per-30s re-trace FLOODED the flight ring with "organ stall surfaced" entries,
                // EVICTING the very events that caused the stall — the dump became useless. One shot keeps
                // the pre-stall story intact for diagnosis; the status line still updates the age live.
                if slow_due {
                    let stalls = flight::stalls(20_000);
                    if let Some((name, age)) = stalls.first() {
                        let mut last = stall_name.borrow_mut();
                        if last.as_deref() != Some(name) {
                            // a NEW organ went silent — capture the story ONCE, before any spam.
                            flight::dump_to_crash_log("organ stall (app alive)");
                            *last = Some(name.to_string());
                        }
                        st.set_status_line(
                            format!(
                        "\u{26a0} {name} silent {}s \u{2014} internal stall (flight log armed)",
                        age / 1000
                    )
                            .into(),
                        );
                        st.set_status_kind("err".into());
                        st.set_status_stale(false);
                    } else {
                        // all organs beating again — arm for the next (different) episode.
                        *stall_name.borrow_mut() = None;
                    }
                }

                // SIDE PLATE: the swappable plate is push-only (no getter), so the selected mouse's
                // readout follows the last plate the device pushed (recorded in the confirmation core
                // by hidwatch). Cheap last-known read; only writes on a change. The instant swap
                // feedback is the confirmation card — this keeps the DEVICE-page readout honest.
                if slow_due {
                    crate::glue::refresh_selected_plate(app);
                    // and the DEVICE-LIST row the plate belongs under — patched in place, not rebuilt.
                    crate::glue::refresh_plated_row(app);
                }

                // LINK lamp: lights only while the live loop is alive.
                if slow_due {
                    if let Some(live) = r.live.as_ref() {
                        let running = live.running();
                        if st.get_runtime_active() != running {
                            st.set_runtime_active(running);
                        }
                    }
                }
                // RELIABILITY panel: repaint uptime / organ heartbeats / crash record ~1s (a readout that
                // doesn't age is a lie — the flight recorder is live, so the panel must breathe with it).
                if slow_due {
                    let mut seen = reliab_seen.borrow_mut();
                    if seen.elapsed() >= Duration::from_secs(1) {
                        *seen = Instant::now();
                        crate::glue::refresh_reliability(app);
                        // HOST BASE RECOVERY: if the protocol-host kernel took a contained fault and
                        // was reborn, every lease was swept — including the app's own animated base
                        // layer (leases are never reborn). Re-claim any base whose kernel layer
                        // vanished so local lighting resumes on its own instead of freezing at its
                        // last latched frame. UNGATED by page: recovery can't wait for the user to
                        // open SYSTEM. No-op when the host is off or every base is still alive.
                        crate::host::heartbeat();
                        // CONNECTIONS statuses live too: the OBS connect state flips async when OBS
                        // answers, and a port can free up, so re-read the host's honest status while
                        // SYSTEM is on screen. Only when the page is up (idle cost stays zero).
                        if st.get_window_shown() && st.get_page() == 3 {
                            crate::glue::refresh_host_status(app);
                        }
                    }
                    // Event poke: a host-bus signal (client connect/disconnect/paint, layer
                    // released) refreshes CONNECTIONS immediately instead of waiting out the 1s
                    // cadence. The stamp is consumed regardless of page — the cadence above
                    // covers a page that opens later.
                    if crate::host::take_host_events_dirty(&host_events_seen)
                        && st.get_window_shown()
                        && st.get_page() == 3
                    {
                        crate::glue::refresh_host_status(app);
                    }
                }
                // VITALS provider: while a `vitals` layer is live (previewing or streaming), feed the core
                // lighting provider so the pattern has fresh battery/charge/stage. Cheap + gated — the read
                // is off-thread and further throttled to the battery cadence (never waking a sleeping mouse
                // more than the battery cards do). A FORCED read on the rising edge lights the surface
                // promptly; steady state re-reads at most ~1Hz (the device read is gated tighter still).
                if slow_due {
                    let live = shared.borrow().light_layers.iter().any(|l| l.pattern == "vitals");
                    let mut was = vitals_seen.borrow_mut();
                    let rising = live && !*was;
                    *was = live;
                    if rising {
                        shared.borrow().rt.pump_vitals(true); // prompt forced read on activation
                    } else if live {
                        let mut seen = vitals_pump_seen.borrow_mut();
                        if seen.elapsed() >= Duration::from_secs(1) {
                            *seen = Instant::now();
                            shared.borrow().rt.pump_vitals(false);
                        }
                    }
                }
                // ARMED pill: the arm gate is a process-global — re-read it so any writer (a future
                // bound disarm action, a teardown path) can't leave the pill lying.
                if slow_due {
                    let armed_now = neuron::action::input_armed();
                    if st.get_input_armed() != armed_now {
                        st.set_input_armed(armed_now);
                        neuron::macros::macro_host().set_armed(armed_now); // keep the macro sidecar's gate honest
                    }
                }

                // Status freshness: stamp on change, dim after 8s, settle to "ready" after 30s.
                if status_due {
                    let text = st.get_status_line().to_string();
                    let mut seen = status_seen.borrow_mut();
                    if text != seen.0 {
                        *seen = (text.clone(), Instant::now());
                        // classify once, centrally: failures get the alarm tint.
                        let t = text.to_lowercase();
                        let err = [
                            "failed",
                            "writes paused",
                            "no device",
                            "invalid",
                            "error",
                            "not saved",
                            "required",
                            "already exists",
                            "no profile",
                            "cancelled",
                        ]
                        .iter()
                        .any(|n| t.contains(n));
                        st.set_status_kind(if err { "err".into() } else { "info".into() });
                        st.set_status_stale(false);
                    } else {
                        let age = seen.1.elapsed();
                        if age > Duration::from_secs(30) && text != "ready" {
                            st.set_status_line("ready".into());
                            st.set_status_kind("info".into());
                            st.set_status_stale(true);
                            *seen = ("ready".into(), Instant::now());
                        } else if age > Duration::from_secs(8) && !st.get_status_stale() {
                            st.set_status_stale(true);
                        }
                    }
                }

                // Tray sync: rebuild the menu only when the truth it renders changed (or after a menu
                // click, repairing muda's client-side check auto-toggle). Idle cost: one Vec compare.
                if tray_due {
                    let snap = {
                        let s = shared.borrow();
                        TraySnapshot {
                            profiles: s.rt.profiles.iter().map(|p| p.name.clone()).collect(),
                            effects: s.rt.effects().into_iter().map(|(n, _, _)| n).collect(),
                            active: s.rt.active_profile.clone(),
                            paused: neuron::writes::writes_paused(),
                            hyper: st.get_hypershift_on(),
                        }
                    };
                    poll_tray.sync(&snap, acted);
                }
            });
            if std::panic::catch_unwind(tick).is_err() {
                flight::trace("life", "ui tick panicked (contained)", 0);
            }
        },
    );

    // Run the loop tray-resident. `quit_event_loop` (tray Quit) is the only exit.
    slint::run_event_loop_until_quit().expect("event loop failed");
    // Drop the UI tick timer FIRST, then flush. `flush_lighting_save` already stops its OWN debounce
    // timer (LIGHT_SAVE_TIMER) and runs single-threaded after the loop has ended, so no UI-timer tick
    // can fire during it — its RefCell borrow is uncontended. Dropping the tick timer here is belt-
    // and-suspenders, making the no-tick-during-flush invariant structural rather than incidental.
    drop(timer);
    // Flush any still-debounced lighting save before teardown — a quit must never strand the last
    // edit. Structural edits (stack/remove/tile-pick) already persist immediately; this catches a
    // knob (speed/stop/timing) tweaked within the 400ms debounce window right before quitting.
    glue::flush_lighting_save();
    // ── DEVICE-MODE RESTORE (the DPI-16000 trap) ──────────────────────────
    // Driver mode is a scoped LEASE (streams/writes), never a permanent state: while a device is held
    // in it, its onboard buttons/FN defer to software and it wakes with a stale factory volatile plane
    // unless something re-asserts config. On exit neuron is that "something" no longer, so hand every
    // device back to firmware ownership. Stop the live streams FIRST (each board's own teardown
    // releases its lease), then enumerate every recognized connected unit and set NORMAL mode as the
    // authoritative last word — all best-effort; this is exit, a few control round-trips are fine.
    if let Some(shared) = resident.borrow().shared.as_ref() {
        shared.borrow_mut().rt.stop_all_animation();
    }
    restore_devices_to_firmware();
    // keep the tray alive for the whole loop
    drop(tray);
}

/// Best-effort: release every recognized, connected device's CUSTODY back to firmware on app exit.
/// Custody is a per-FAMILY concept routed through the def's dialect (`Device::release_custody`): a
/// razer board returns its driver-mode lease (device_mode -> NORMAL), a never-in-custody family
/// no-ops rather than being handed a razer-framed mode packet. Driver mode is a lease held for the
/// duration of streams/writes; leaving a razer device in it orphans its onboard buttons/FN and the
/// wake-restore duty (the trap where the Naga woke announcing a stale DPI 16000). One release per
/// (physical UNIT, FAMILY); every step is `let _ =` — this runs during teardown and must never fail
/// the exit.
fn restore_devices_to_firmware() {
    let Ok(reg) = neuron::registry::Registry::load() else {
        return;
    };
    let Ok(infos) = neuron::transport::enumerate() else {
        return;
    };
    let mut done: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    for i in &infos {
        // find_for_pipe: the family-aware control-pipe def, so a two-family unit restores through the
        // family that can frame the mode switch.
        let Some(def) = reg.find_for_pipe(i) else {
            continue;
        };
        // Dedupe per (unit, FAMILY), seeded only AFTER a def resolves. A device's several HID
        // collections collapse via `instance()`, but a MULTI-FAMILY unit must release each family's
        // custody once. The review's failure shape: keying on `instance()` ALONE let whichever
        // family's pipe enumerated first claim the unit, so a sibling family's NO-OP release could
        // shadow razer's real mode restore — leaving the board stuck in driver mode. Keying on the
        // dialect too gives each family its own single release.
        if !done.insert((i.instance(), def.dialect.clone())) {
            continue;
        }
        if let Ok(d) = neuron::device::Device::open_path(def.clone(), i.pid, &i.path) {
            let _ = d.release_custody();
        }
    }
}

/// Choose the Slint renderer backend BEFORE any window is created: prefer femtovg (GPU), fall back to
/// the software renderer if femtovg can't be selected. Keeps the GPU path on a GPU machine while
/// guaranteeing the UI opens on a host with no usable OpenGL context (VM / RDP / headless / bad
/// drivers). Both renderers are compiled in (see Cargo.toml). Never panics — a host where even the
/// software backend can't be set is one no renderer choice could rescue, so we log and let the later
/// `AppWindow::new()` surface the failure honestly.
fn select_renderer_backend() {
    if let Err(gpu_err) = slint::BackendSelector::new()
        .renderer_name("femtovg".into())
        .select()
    {
        eprintln!(
            "neuron: GPU (femtovg) renderer unavailable ({gpu_err}); falling back to software renderer"
        );
        flight::trace("life", "femtovg unavailable — software fallback", 0);
        if let Err(sw_err) = slint::BackendSelector::new()
            .renderer_name("software".into())
            .select()
        {
            eprintln!("neuron: software renderer also unavailable ({sw_err}); the UI may not open");
        }
    }
}

/// Create the window + install glue if not already present — WITHOUT showing it. Quick actions
/// need the runtime, not a window in the user's face.
fn build_window(resident: &Rc<RefCell<Resident>>) {
    if resident.borrow().window.is_some() {
        return;
    }
    let app = AppWindow::new().expect("failed to create window");
    let shared = glue::install(&app);

    // DEV: `NEURON_START_PAGE=<n>` opens directly on that page (0 = bindings, 1 = lighting, …) —
    // for verify-by-running sessions that need a specific page on screen without driving the nav.
    if let Some(p) = std::env::var("NEURON_START_PAGE").ok().and_then(|v| v.parse::<i32>().ok()) {
        app.global::<State>().set_page(p);
    }

    // Closing the window hides it (the loop lives on, tray-resident). The handle + glue are
    // retained so runtime state (selected device, paused gate, parsed import) survives a
    // reopen; the software renderer makes a hidden window's idle cost negligible.
    let close_w = app.as_weak();
    app.window().on_close_requested(move || {
        // end any in-flight press-to-bind before hiding to tray — otherwise the capture worker keeps
        // the dispatcher gated (binds dead) while hidden, and you'd return to a stuck capture overlay.
        crate::capture::cancel();
        // hidden to tray → the lighting page's render timers stand down (no compute while unseen).
        if let Some(a) = close_w.upgrade() {
            a.global::<State>().set_window_shown(false);
        }
        slint::CloseRequestResponse::HideWindow
    });

    let mut r = resident.borrow_mut();
    r.window = Some(app);
    r.shared = Some(shared);
}

/// Show + focus the (already-built) window — the deliberate "open Neuron" action.
fn show_window(resident: &Rc<RefCell<Resident>>) {
    build_window(resident);
    if let Some(app) = resident.borrow().window.as_ref() {
        let _ = app.show();
        app.window().set_minimized(false);
        app.global::<State>().set_window_shown(true); // on screen now → page animation timers may run
    }
    // RAISE over whatever owns the foreground. A bare `show()` lands BEHIND a borderless-fullscreen
    // game, so "Open Neuron" reads as "nothing happened". Force it forward like a summon does.
    #[cfg(windows)]
    raise_self();
}

/// Find our own visible top-level "Neuron" window and force it to the front (the same focus-handoff
/// `teleport` uses for summon). Needed because `app.show()` won't beat a fullscreen game's z-order.
#[cfg(windows)]
fn raise_self() {
    use windows_sys::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    };
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let out = &mut *(l as *mut isize);
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, &mut pid);
        if pid == GetCurrentProcessId() && IsWindowVisible(h) != 0 {
            let mut buf = [0u16; 16];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
            if String::from_utf16_lossy(&buf[..n]) == "Neuron" {
                *out = h as isize;
                return 0; // found — stop enumerating
            }
        }
        1 // keep going
    }
    let mut found: isize = 0;
    unsafe {
        EnumWindows(Some(cb), &mut found as *mut isize as LPARAM);
    }
    if found != 0 {
        crate::teleport::force_foreground(found);
    }
}

/// Dispatch a tray/hotkey action. Quick toggles drive the same `State` callbacks the UI uses (so
/// tray and window stay perfectly in sync) but do NOT show the window — nudging brightness from
/// the tray mid-game must never yank a 1240px window over the game. Only Open/Settings show.
fn handle_tray(resident: &Rc<RefCell<Resident>>, action: TrayAction) {
    flight::trace("tray", "action", 0);
    // Every quick action drives the State callbacks, which need the window BUILT (not shown). In
    // tray-first launch (--tray) the window is never built at startup, so without this the quick
    // controls (brightness/DPI/profile/HyperShift/pause) silently no-op. `build_window` is
    // idempotent and does NOT show anything — it just makes the runtime/State reachable.
    build_window(resident);
    match action {
        TrayAction::Open => {
            flight::trace("life", "window show requested (tray)", 0);
            show_window(resident);
        }
        TrayAction::Quit => {
            flight::trace("life", "quit requested (tray)", 0);
            slint::quit_event_loop().unwrap();
        }
        TrayAction::GotoSettings => {
            show_window(resident);
            if let Some(app) = resident.borrow().window.as_ref() {
                app.global::<State>().invoke_goto_page(3); // SYSTEM (settings) is section index 3
            }
        }
        TrayAction::ToggleHyperShift => {
            // flip the REAL software latch on the live engine — the SHIFT pill lights from the
            // engine's held-layers via the status post (one source of truth; no UI-side write
            // that the next live event would clobber).
            let on = dispatch::toggle_hypershift_latch();
            if let Some(app) = resident.borrow().window.as_ref() {
                app.global::<State>().set_status_line(
                    format!("HyperShift {}", if on { "ON" } else { "off" }).into(),
                );
            }
        }
        TrayAction::ToggleWritesPaused => {
            if let Some(app) = resident.borrow().window.as_ref() {
                app.global::<State>().invoke_toggle_writes_paused();
            }
        }
        TrayAction::ApplyProfile(name) => {
            if let Some(app) = resident.borrow().window.as_ref() {
                app.global::<State>().invoke_apply_profile(name.into());
            }
        }
        TrayAction::SetEffect(name) => {
            if let Some(app) = resident.borrow().window.as_ref() {
                // find the effect index and apply it
                let st = app.global::<State>();
                let effects = st.get_effects();
                use slint::Model;
                for (i, e) in effects.iter().enumerate() {
                    if e.name == name.as_str() {
                        st.invoke_apply_effect(i as i32);
                        break;
                    }
                }
            }
        }
        TrayAction::BrightnessUp => nudge_brightness(resident, 10.0),
        TrayAction::BrightnessDown => nudge_brightness(resident, -10.0),
        TrayAction::DpiUp => nudge_dpi(resident, 200.0),
        TrayAction::DpiDown => nudge_dpi(resident, -200.0),
    }
}

fn nudge_brightness(resident: &Rc<RefCell<Resident>>, delta: f32) {
    if let Some(app) = resident.borrow().window.as_ref() {
        let st = app.global::<State>();
        if st.get_writes_paused() {
            st.set_status_line("writes paused — brightness unchanged".into());
            return; // don't move the slider to a value the device never received
        }
        let v = (st.get_brightness() + delta).clamp(0.0, 100.0);
        st.set_brightness(v);
        st.invoke_apply_brightness(v);
    }
}

fn nudge_dpi(resident: &Rc<RefCell<Resident>>, delta: f32) {
    if let Some(app) = resident.borrow().window.as_ref() {
        let st = app.global::<State>();
        if st.get_writes_paused() {
            st.set_status_line("writes paused — DPI unchanged".into());
            return;
        }
        let v = (st.get_dpi() + delta).clamp(100.0, 30000.0);
        st.set_dpi(v);
        st.invoke_apply_dpi(v);
    }
}
