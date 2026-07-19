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

/// Measurement harness for the input-pump rewrite (baseline instrumentation, NOT a debug toggle —
/// this is always-on, like the flight recorder's pulses). Counts pump-loop wakeups by reason,
/// histograms wake→first-edge latency, and watches tick-to-tick cadence for starvation. Every hook
/// is a relaxed atomic (or, on a starvation transition, one `eprintln!`) — cheap enough to leave in
/// permanently and to NOT alter the pump's control flow, sleeps, or dispatch order.
///
/// `crates/neuron-core/src/controls.rs`'s `win::listen` (the Win32 Raw-Input pump) is the only
/// wired-in caller today: [`pump::record_wake`] once per outer loop iteration, [`pump::record_latency_us`]
/// via a thin wrapper around the caller's `on_event`, and [`pump::record_tick`] alongside `on_tick()`.
pub mod pump {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    use std::time::Instant;

    // ── wake counters, by reason ────────────────────────────────────────────────────────────
    /// One pump-loop iteration where `PeekMessage` drained >=1 message this iteration.
    pub static WAKE_INPUT: AtomicU64 = AtomicU64::new(0);
    /// One pump-loop iteration with nothing to drain — just the periodic tick.
    pub static WAKE_TICK_ONLY: AtomicU64 = AtomicU64::new(0);
    /// Total pump-loop iterations. Tracked as its own counter (not `WAKE_INPUT + WAKE_TICK_ONLY`)
    /// so a reader never has to add two atomics to get a number that was never torn.
    pub static WAKE_TOTAL: AtomicU64 = AtomicU64::new(0);

    /// Tally one pump-loop iteration. `had_input` = this iteration's `PeekMessage` drain saw at
    /// least one message.
    #[inline]
    pub fn record_wake(had_input: bool) {
        if had_input {
            WAKE_INPUT.fetch_add(1, Ordering::Relaxed);
        } else {
            WAKE_TICK_ONLY.fetch_add(1, Ordering::Relaxed);
        }
        WAKE_TOTAL.fetch_add(1, Ordering::Relaxed);
    }

    // ── wake -> first-edge latency histogram, powers-of-two microsecond buckets ─────────────
    const BUCKETS: usize = 24; // 2^0..2^23 us (~8.4s ceiling) — generous headroom over a 5ms pump
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU64 = AtomicU64::new(0);
    static LATENCY_BUCKETS: [AtomicU64; BUCKETS] = [ZERO; BUCKETS];

    #[inline]
    fn bucket_for(us: u64) -> usize {
        if us == 0 {
            0
        } else {
            (64 - us.leading_zeros() as usize).min(BUCKETS - 1)
        }
    }

    /// Record one wake -> first-edge latency sample (microseconds) into the histogram.
    #[inline]
    pub fn record_latency_us(us: u64) {
        LATENCY_BUCKETS[bucket_for(us)].fetch_add(1, Ordering::Relaxed);
    }

    /// `(bucket upper-bound microseconds, count)` pairs, low to high.
    pub fn latency_snapshot() -> Vec<(u64, u64)> {
        LATENCY_BUCKETS
            .iter()
            .enumerate()
            .map(|(i, c)| (1u64 << i, c.load(Ordering::Relaxed)))
            .collect()
    }

    // ── tick-starvation watchdog (permanent — a silent-failure detector, not a debug aid) ───
    // PER-LISTENER and CADENCE-AWARE. Two properties of the blocking pump force both:
    //  * its tick cadence is VARIABLE (turbo ~8ms / mic-or-appfocus ~50ms / idle up to ~1000ms), so
    //    a fixed absolute threshold would false-flag every idle second; and
    //  * multiple listeners tick independently (the resident dispatch pump + a transient press-to-
    //    bind capture pump), so a single shared timestamp would let a healthy listener MASK a stalled
    //    one.
    // So state is keyed by listener id, and each gap is judged against THAT listener's own cadence.

    /// A gap beyond `cadence * FACTOR` (and at least `FLOOR_MS`) is starvation — enough headroom over
    /// ordinary scheduler jitter and one skipped wait to mean a genuine stall, not noise.
    const STARVATION_FACTOR: u64 = 4;
    const STARVATION_FLOOR_MS: u64 = 250;

    struct TickState {
        /// When this listener last ticked. `None` = never — an OPTION, not a `0` sentinel: `now_ms`
        /// is measured from a lazily-started epoch, so it legitimately IS `0` for the first
        /// millisecond. A `0`-means-never sentinel therefore collided with a real timestamp — the
        /// first tick stored `0`, the next read it back as "never ticked" and returned early, and
        /// the gap was never measured. (A test hid this by sleeping before starting the clock;
        /// deleting that crutch surfaced it.)
        last_ms: Option<u64>,
        starved: bool, // currently past-threshold (edge-triggers the one-shot log)
    }

    fn tick_state() -> &'static std::sync::Mutex<std::collections::HashMap<u64, TickState>> {
        static S: OnceLock<std::sync::Mutex<std::collections::HashMap<u64, TickState>>> =
            OnceLock::new();
        S.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    static STARVATION_EVENTS: AtomicU64 = AtomicU64::new(0);

    fn epoch() -> &'static Instant {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now)
    }

    /// Call once per `on_tick`, with the listener's id and the cadence (ms) it is about to wait.
    /// Records the gap since THIS listener's previous tick; past `cadence * FACTOR` (min FLOOR_MS)
    /// it counts the event and — only on that listener's silent→starved transition — logs ONE line,
    /// so a wedged pump is never silent, never spams, and a healthy listener never masks it.
    pub fn record_tick(listener_id: u64, cadence_ms: u32) {
        let now_ms = epoch().elapsed().as_millis() as u64;
        let mut map = tick_state().lock().unwrap_or_else(|e| e.into_inner());
        let entry = map
            .entry(listener_id)
            .or_insert(TickState { last_ms: None, starved: false });
        let last = entry.last_ms.replace(now_ms);
        let Some(last) = last else {
            return; // first tick for this listener — no prior gap to measure
        };
        let gap = now_ms.saturating_sub(last);
        let threshold = (cadence_ms as u64)
            .saturating_mul(STARVATION_FACTOR)
            .max(STARVATION_FLOOR_MS);
        if gap > threshold {
            STARVATION_EVENTS.fetch_add(1, Ordering::Relaxed);
            if !entry.starved {
                entry.starved = true;
                eprintln!(
                    "[pump] tick starvation (listener {listener_id}): {gap}ms gap, expected ~{cadence_ms}ms cadence"
                );
            }
        } else {
            entry.starved = false;
        }
    }

    /// Drop a listener's watchdog state when its loop exits, so the per-listener map can't grow
    /// unbounded across repeated transient capture listens.
    pub fn forget_listener(listener_id: u64) {
        tick_state()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&listener_id);
    }

    pub fn starvation_count() -> u64 {
        STARVATION_EVENTS.load(Ordering::Relaxed)
    }

    /// Everything the read-out surface (CLI / tests) needs, gathered in one call.
    pub struct Snapshot {
        pub wake_input: u64,
        pub wake_tick_only: u64,
        pub wake_total: u64,
        pub latency_buckets_us: Vec<(u64, u64)>,
        pub starvation_events: u64,
    }

    /// Read every pump counter now. Cheap (a handful of relaxed loads) — safe to call from a CLI
    /// command or a UI panel, not just tests.
    pub fn snapshot() -> Snapshot {
        Snapshot {
            wake_input: WAKE_INPUT.load(Ordering::Relaxed),
            wake_tick_only: WAKE_TICK_ONLY.load(Ordering::Relaxed),
            wake_total: WAKE_TOTAL.load(Ordering::Relaxed),
            latency_buckets_us: latency_snapshot(),
            starvation_events: starvation_count(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Serializes the watchdog tests: each asserts an exact delta on the process-global
        /// STARVATION_EVENTS counter, so they must not run concurrently with each other.
        static WATCHDOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        #[test]
        fn wake_counters_tally_by_reason() {
            let before = snapshot();
            record_wake(true);
            record_wake(false);
            record_wake(false);
            let after = snapshot();
            assert_eq!(after.wake_input, before.wake_input + 1);
            assert_eq!(after.wake_tick_only, before.wake_tick_only + 2);
            assert_eq!(after.wake_total, before.wake_total + 3);
        }

        #[test]
        fn latency_lands_in_the_expected_bucket() {
            let before = latency_snapshot();
            record_latency_us(100); // 2^6=64 < 100 <= 2^7=128 -> bucket 7
            let after = latency_snapshot();
            assert_eq!(after[7].1, before[7].1 + 1);
        }

        #[test]
        fn tick_gap_far_past_cadence_counts_as_starvation() {
            let _g = WATCHDOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let _ = epoch(); // start the clock before we measure real elapsed time
            let listener = 0xD060_0001; // a unique id so this test can't collide with others
            forget_listener(listener); // clean slate
            let before = starvation_count();
            record_tick(listener, 5); // first tick — establishes last_ms, no gap yet
            std::thread::sleep(std::time::Duration::from_millis(STARVATION_FLOOR_MS + 60));
            record_tick(listener, 5); // gap ~310ms >> max(5*4, 250)=250 → starvation
            assert_eq!(starvation_count(), before + 1, "a gap far past the cadence counts once");
            // a prompt follow-up tick sees a healthy gap and clears the per-listener starved flag;
            // the count itself must never go backward.
            record_tick(listener, 5);
            assert!(starvation_count() >= before + 1);
            forget_listener(listener);
        }

        #[test]
        fn idle_cadence_gaps_are_not_starvation() {
            // The blocking pump legitimately idles up to ~1000ms between ticks; with a matching
            // cadence hint that gap is NORMAL, not starvation (the old fixed 50ms threshold would
            // have false-flagged it every idle second).
            let _g = WATCHDOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let _ = epoch();
            let listener = 0xD060_0004;
            forget_listener(listener);
            let before = starvation_count();
            record_tick(listener, 1000); // idle cadence
            std::thread::sleep(std::time::Duration::from_millis(300)); // < 1000*4 threshold
            record_tick(listener, 1000);
            assert_eq!(starvation_count(), before, "a normal idle gap is not starvation");
            forget_listener(listener);
        }

        #[test]
        fn one_listeners_health_does_not_mask_anothers_starvation() {
            // The multi-listener fix: a healthy listener ticking briskly must NOT hide a different
            // listener that has gone silent (the old shared-timestamp watchdog did exactly that).
            let _g = WATCHDOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let _ = epoch();
            let healthy = 0xD060_0002;
            let stalled = 0xD060_0003;
            forget_listener(healthy);
            forget_listener(stalled);
            let before = starvation_count();
            record_tick(healthy, 5); // both establish a baseline
            record_tick(stalled, 5);
            // the healthy listener keeps ticking well inside its threshold while the stalled one
            // stays silent across a > FLOOR_MS span
            for _ in 0..6 {
                std::thread::sleep(std::time::Duration::from_millis(60));
                record_tick(healthy, 5); // gaps ~60ms < 250 → healthy never flagged
            }
            record_tick(stalled, 5); // ~360ms silent → flagged, despite `healthy` staying fresh
            assert_eq!(
                starvation_count(),
                before + 1,
                "the stalled listener is flagged even though a concurrent one stayed healthy"
            );
            forget_listener(healthy);
            forget_listener(stalled);
        }
    }
}
