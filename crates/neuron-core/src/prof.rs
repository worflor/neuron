//! A lightweight hot-path profiler, INERT unless `NEURON_PROFILE` is set in the environment.
//!
//! Each instrumented loop calls [`bump`] on a named [`AtomicU64`] counter. When profiling is on,
//! a 1 Hz logger (in neuron-app, which owns the Win32 thread/process query features) reads these
//! as per-second DELTAS alongside this process's per-thread CPU and the Python sidecar's CPU. A
//! bug that "never stops" — a loop spinning after a beacon, a thread burning while the app should
//! be idle — then shows up plainly as a counter racing or a thread/sidecar pinned, instead of
//! something we have to guess at. `bump` is a single relaxed increment, so it's free to leave in.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

macro_rules! prof_counters {
    ($($name:ident = $label:literal),+ $(,)?) => {
        $( pub static $name: AtomicU64 = AtomicU64::new(0); )+
        /// (label, counter) pairs the logger walks each tick — the full instrumented surface.
        pub static COUNTERS: &[(&str, &AtomicU64)] = &[ $( ($label, &$name) ),+ ];
    };
}

prof_counters!(
    PRESENTER_CYCLE = "presenter_cycle", // beacon.rs presenter loop — one turn per present/live_weave
    LIVE_WEAVE = "live_weave",           // beacon.rs live_weave() entry — the idle radial/spellweave watch
    PRESENT = "present",                 // beacon.rs present() entry — one per beacon shown
    CAPTURE_ARM = "capture_arm",         // glyph.rs setup() — a raw-input window created + registered
    CAPTURE_POLL = "capture_poll",       // glyph.rs activation-wait poll tick (~333/s per armed capture)
    ROUTER_EVENT = "router_event",       // beacon.rs router — one per BeaconEvent drained
    OVERLAY_FRAME = "overlay_frame",     // overlay.rs render loop — one per frame (~60/s active, ~60/s idle)
    MACRO_FIRE = "macro_fire",           // macro_host fire_dispatch — one per macro fire sent
    READER_FRAME = "reader_frame",       // macro_host reader_loop — one per protocol frame from the sidecar
);

/// PID of the bundled-CPython sidecar, set when it is spawned, so the logger can sample its CPU —
/// a "never stops" spin that lives in the Python process (not in Rust) shows here.
pub static SIDECAR_PID: AtomicU32 = AtomicU32::new(0);

/// Whether `NEURON_PROFILE` was requested. The logger checks this before spawning; hot paths don't
/// need to (a relaxed increment costs nothing meaningful even when nobody reads it).
pub fn enabled() -> bool {
    std::env::var_os("NEURON_PROFILE").is_some()
}

#[inline]
pub fn bump(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}
