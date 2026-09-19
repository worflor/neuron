// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! LATENCY INSTRUMENT — where the milliseconds actually go, measured rather than guessed.
//!
//! [`crate::prof`] answers "is this loop spinning?" (throughput counters). [`crate::flight`]'s
//! sibling in the app answers "what happened before we died?" (an event ring). Neither answers the
//! question a user actually asks — *"why is there a delay between pressing my macro key and the
//! macro happening?"* — because that is a question about a DISTRIBUTION, not a count: the p50 can
//! be excellent while the p99 is what you feel.
//!
//! So this module records per-stage DURATIONS as histograms:
//!
//! * **Sub-bucketed log scale.** 4 buckets per octave over microseconds — 256 buckets covering 1µs
//!   to centuries, with at most +12.5% error above 4µs and exact resolution below it. Percentiles
//!   are reported at each bucket's UPPER bound, so a quoted number is always a number the stage
//!   really did stay under (never a flattering midpoint).
//! * **Lock-free and allocation-free.** One relaxed `fetch_add` per bucket, count, sum and a
//!   compare-exchange loop for the max. Any thread can record at any time; nothing blocks the
//!   input path, and there is no lock for a crashing process to be holding.
//! * **ALWAYS ON.** Recording costs an `Instant::now()` (a `QueryPerformanceCounter`, ~20-30ns) plus
//!   a handful of relaxed atomics. At input rates that is unmeasurable, and the payoff is large:
//!   when the user says "that felt laggy", the numbers for the press they just made are ALREADY
//!   recorded. A latency instrument you have to turn on and reproduce under is a latency instrument
//!   you never have data from. (Nothing here is in a per-pixel or per-frame path — see the module
//!   list below; those stay uninstrumented on purpose.)
//!
//! ## The chain being measured
//!
//! A macro key press travels: firmware → USB → HID class driver → our reader thread → the dispatch
//! pump → the engine → the action → `SendInput`. We own everything from the reader thread onward,
//! so that is what is instrumented, stage by stage. The headline number is
//! [`PRESS_TO_OUTPUT`]: from the moment an input edge became visible to us, to the moment the
//! first resulting keystroke hit the OS input stream. That is the part of "the delay" that is
//! OURS, stated honestly and separately from the USB/driver leg we cannot see.
//!
//! ## Cross-thread and cross-stage stamping
//!
//! [`PRESS_TO_OUTPUT`] spans threads (the pump dispatches; a macro worker emits), which would
//! otherwise mean threading an `Instant` through `Action::run_ctx` and every action in the tree.
//! Instead the origin rides a thread-local ([`with_edge`]) that output primitives consult
//! ([`mark_output`]), and a spawned macro worker inherits it explicitly ([`origin`] / [`adopt`]).
//! Zero signature churn on the action layer, and the stamp cannot leak between unrelated edges
//! because [`mark_output`] consumes it.
//!
//! There are TWO thread-locals, not one, and the distinction is load-bearing: the origin is a
//! MEASUREMENT consumed by the first output, while [`servicing_input_edge`] answers a SCHEDULING
//! question that must stay true for the whole edge (it decides whether an expensive side effect is
//! moved off the dispatch pump). Serving both from one value silently put a process spawn back on
//! the pump for any binding whose Key rule was ordered before its Shell rule — see `ON_INPUT_PATH`.
//!
//! Platform-neutral: pure `std::time` + atomics, no OS calls, identical on every target.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// ── the histogram ───────────────────────────────────────────────────────────────────────────────

/// Sub-buckets per octave, as a bit count: 2 bits => 4 buckets per power of two, so a value is
/// known to within +12.5%. One bit (2/octave, +25%) is too coarse to tell "p99 is 3ms" from "p99 is
/// 5ms" — the exact distinction that decides whether a delay is felt; three (8/octave) would double
/// the static footprint for resolution finer than the jitter of the thing being measured.
const SUB_BITS: u32 = 2;
/// Sub-buckets per octave (`1 << SUB_BITS`).
const SUB: u64 = 1 << SUB_BITS;
/// Buckets per histogram. `bucket_of` maxes out at 251 for `u64::MAX` microseconds; 256 rounds that
/// up and leaves the index arithmetic unable to run off the end.
const BUCKETS: usize = 256;

/// The largest value a single sample can contribute: one hour, in microseconds.
///
/// Two jobs. First, HONESTY: a stage that took an hour is not a latency measurement, it is a hung
/// thread or a machine that slept mid-span, and letting such a value through would drag `mean_us`
/// somewhere meaningless while telling the reader nothing they could act on.
///
/// Second, and the reason it is enforced in [`Hist::record`] rather than at the call sites: it makes
/// the running `sum_us` provably unable to overflow. `u64::MAX / SAMPLE_CEILING_US` is about 5×10⁹, so
/// the sum could only wrap after five billion samples that each took a full hour — which is more
/// hours than the process could possibly have existed for. That means the hot path keeps a plain
/// `fetch_add` (no compare-exchange loop for a saturating add) AND the reported mean can never be
/// corrupted by wraparound. Bounding the input is what makes the accumulator safe; guarding the
/// accumulator alone would have left a single absurd sample able to poison the mean.
const SAMPLE_CEILING_US: u64 = 60 * 60 * 1_000_000;

/// Which bucket a microsecond duration falls in. Monotone (a larger duration never lands in a lower
/// bucket) and total (every `u64` maps inside `0..BUCKETS`) — both pinned by tests, because a
/// non-monotone bucketing silently corrupts every percentile it feeds.
fn bucket_of(us: u64) -> usize {
    if us < SUB {
        // Below the first octave the value IS the bucket — exact, no error at all, which is where
        // the cheap stages (an atomic store, a hash lookup) live.
        return us as usize;
    }
    let octave = 63 - u64::from(us.leading_zeros()); // >= 2, since us >= SUB (4)
    let sub = (us >> (octave - u64::from(SUB_BITS))) & (SUB - 1);
    (((octave - 1) * SUB + sub) as usize).min(BUCKETS - 1)
}

/// The smallest duration that lands in `bucket` — the inverse of [`bucket_of`]. Saturating at the
/// top so the (unreachable in practice: ~500,000 years) high buckets can't overflow the shift.
fn bucket_floor_us(bucket: usize) -> u64 {
    let b = bucket as u64;
    if b < SUB {
        return b;
    }
    let octave = b / SUB + 1;
    let sub = b % SUB;
    let shift = octave - u64::from(SUB_BITS);
    let base = SUB + sub;
    // Saturate on overflow rather than wrapping. `checked_shl` is NOT enough here: it only rejects a
    // shift past the word width, so `4u64 << 62` "succeeds" by wrapping to 0 — which would report a
    // top bucket's floor as ZERO and make the largest possible sample look like the smallest.
    if shift >= 64 || u64::from(base.leading_zeros()) < shift {
        u64::MAX
    } else {
        base << shift
    }
}

/// The largest duration that lands in `bucket` — what percentiles are quoted at, so a reported
/// value is one the stage genuinely stayed at or under.
fn bucket_ceil_us(bucket: usize) -> u64 {
    let next_floor = bucket_floor_us(bucket + 1);
    if next_floor == u64::MAX {
        u64::MAX // the top bucket is open-ended; its ceiling is the end of the range
    } else {
        next_floor - 1
    }
}

/// One named stage's duration distribution. Lives in a `static`; recorded from any thread.
pub struct Hist {
    /// Human label, used by [`report`].
    label: &'static str,
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    /// Exact worst sample (not bucketed) — the outlier is the whole story in a latency complaint,
    /// so it is never rounded.
    max_us: AtomicU64,
}

impl Hist {
    const fn new(label: &'static str) -> Self {
        Self {
            label,
            buckets: [const { AtomicU64::new(0) }; BUCKETS],
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
        }
    }

    /// Record one sample. Relaxed throughout: these counters are read by a human on a later tick,
    /// never used to order other memory, so `Relaxed` is the correct (and cheapest) ordering.
    pub fn record(&self, d: Duration) {
        // Clamp to [`SAMPLE_CEILING_US`] BEFORE accumulating. Clamping the input (rather than making
        // the running sum saturate) is what keeps `sum_us` unable to overflow at all — see the
        // ceiling's doc — so the hot path stays a plain `fetch_add` with no compare-exchange loop,
        // and `mean_us` cannot be corrupted by a single absurd sample.
        let us = u64::try_from(d.as_micros())
            .unwrap_or(SAMPLE_CEILING_US)
            .min(SAMPLE_CEILING_US);
        self.buckets[bucket_of(us)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Record a sample measured in microseconds directly (for callers that already have the delta).
    pub fn record_us(&self, us: u64) {
        self.record(Duration::from_micros(us));
    }

    /// How many samples have been recorded.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// The exact worst sample seen, in microseconds.
    pub fn max_us(&self) -> u64 {
        self.max_us.load(Ordering::Relaxed)
    }

    /// The mean, in microseconds (0 with no samples).
    pub fn mean_us(&self) -> u64 {
        // `checked_div` rather than an `if n == 0` guard: the divide-by-zero case IS the no-samples
        // case, so letting the division express it keeps the two from drifting apart.
        self.sum_us
            .load(Ordering::Relaxed)
            .checked_div(self.count())
            .unwrap_or(0)
    }

    /// The `q`-quantile (0.0..=1.0) in microseconds, quoted at the containing bucket's UPPER bound.
    /// Returns 0 with no samples. Reads the buckets one pass, so a concurrent writer can shift the
    /// answer slightly — acceptable and documented: this is a diagnostic, not a ledger.
    pub fn quantile_us(&self, q: f64) -> u64 {
        let n = self.count();
        if n == 0 {
            return 0;
        }
        // The rank we're looking for, clamped into 1..=n so q=0 yields the first sample's bucket
        // and q=1 the last one's (never a rank of 0, which no cumulative count can reach).
        let target = ((q.clamp(0.0, 1.0) * n as f64).ceil() as u64).clamp(1, n);
        let mut seen = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            seen += b.load(Ordering::Relaxed);
            if seen >= target {
                // Never quote a percentile ABOVE the exact max: with a bucket's upper bound as the
                // estimate, the top bucket's ceiling can exceed the real worst sample, which reads
                // as an outlier we never actually saw.
                return bucket_ceil_us(i).min(self.max_us()).max(bucket_floor_us(i));
            }
        }
        self.max_us()
    }

    /// This stage's name.
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Forget every sample — so a measurement run starts from a clean slate instead of averaging in
    /// whatever the app did while it was starting up.
    pub fn reset(&self) {
        for b in &self.buckets {
            b.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.sum_us.store(0, Ordering::Relaxed);
        self.max_us.store(0, Ordering::Relaxed);
    }
}

// ── the instrumented stages ─────────────────────────────────────────────────────────────────────

macro_rules! lat_hists {
    ($($name:ident = $label:literal, $doc:literal),+ $(,)?) => {
        $(
            #[doc = $doc]
            pub static $name: Hist = Hist::new($label);
        )+
        /// Every stage, in chain order — what [`report`] walks and [`reset_all`] clears.
        pub static STAGES: &[&Hist] = &[ $( &$name ),+ ];
    };
}

lat_hists!(
    HID_DECODE = "hid_decode",
        "Reader thread: a vendor HID report's bytes turned into an injected edge (the macro-key \
         `0x04` decode, hidwatch's event decode). Pure CPU on a dedicated thread.",
    INJECT_HOP = "inject_hop",
        "The cross-thread handoff: `controls::inject_event` broadcast an edge, to the moment the \
         dispatch pump drained it. Covers the channel send, the `SetEvent` wake, and the OS \
         scheduling latency of waking the blocked pump — so a starved pump shows up HERE.",
    RAW_DECODE = "raw_decode",
        "Pump thread: a `WM_INPUT` raw-input report decoded into a `ControlEvent` (the HidP usage \
         walk).",
    EDGE_DIFF = "edge_diff",
        "Pump thread: a `ControlEvent`'s held-set diffed against the previous one into Down/Up \
         edges.",
    RESOLVE = "resolve",
        "Pump thread: `Engine::resolve` matched the trigger against every rule (layers, app scope, \
         device scope).",
    CTX_CAPTURE = "ctx_capture",
        "Pump thread: `Context::capture` — the foreground-window/process-image/clipboard probe, \
         paid only when a matched action needs it. A clipboard held by another process lands here.",
    CTX_CLIPBOARD = "ctx_clipboard",
        "The CLIPBOARD half of `Context::capture`, split out because the two halves have completely \
         different costs and completely different fixes: the window/process probes are local Win32 \
         reads, while the clipboard is a contended process-wide resource another application can be \
         holding. Only measuring the total would leave us guessing which one to attack.",
    ACTION_RUN = "action_run",
        "Pump thread: the whole `run_action` call. For a spawned macro this is just the handoff; \
         for a synchronous action it is the real work, and while it runs NO further input is \
         dispatched — so this is the stage that must stay small.",
    MACRO_SPAWN = "macro_spawn",
        "Spawning the worker thread a macro sequence runs on — pure overhead paid on the dispatch \
         thread before the macro's first step can fire.",
    SLEEP_ERROR = "sleep_error",
        "Macro-step timing FIDELITY: how far a requested pause overshot. A plain `Sleep` is \
         quantised to the system timer (~15.6ms by default), so a 2ms inter-step delay can really \
         take 15ms — this is that error, and it should stay near zero.",
    SEND_INPUT = "send_input",
        "One `SendInput` call into the OS input stream.",
    PRESS_TO_OUTPUT = "press_to_output",
        "THE HEADLINE. An input edge became visible to us, to the first resulting keystroke/click \
         entering the OS input stream. Everything between is ours to make small; the USB and HID \
         driver leg before it is not.",
    EDGE_TO_DONE = "edge_to_done",
        "An input edge became visible to us, to the dispatch pump being free for the next one. \
         Larger than `press_to_output` by the status-publish tail; if this grows, throughput \
         (not just latency) is at risk.",
    PUMP_BLOCKED = "pump_blocked",
        "How long the dispatch pump sat in its blocking wait. Not a latency — an IDLE-HEALTH \
         reading: a pump that never blocks long is a pump busy-polling, which is the CPU/battery \
         regression this project deliberately designed out.",
);

/// Forget every stage's samples (see [`Hist::reset`]).
pub fn reset_all() {
    for h in STAGES {
        h.reset();
    }
}

// ── scoped timing ───────────────────────────────────────────────────────────────────────────────

/// An RAII stage timer: records the elapsed time into its histogram when dropped. Records on an
/// unwind too, which is deliberate — a stage that panicked still took time, and losing the sample
/// would make a fault look like a gap in the data.
pub struct Timer {
    hist: &'static Hist,
    start: Instant,
}

impl Timer {
    /// Elapsed so far, without ending the measurement.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.hist.record(self.start.elapsed());
    }
}

/// Start timing a stage; the returned guard records on drop.
///
/// ```
/// # use neuron::latency;
/// let _t = latency::start(&latency::RESOLVE);
/// // ... the stage ...
/// ```
pub fn start(hist: &'static Hist) -> Timer {
    Timer {
        hist,
        start: Instant::now(),
    }
}

// ── the edge origin (how `press_to_output` spans threads) ────────────────────────────────────────

thread_local! {
    /// When the input edge this thread is currently servicing became visible to us. Set by the
    /// dispatch pump around one edge ([`with_edge`]), inherited by a macro worker ([`adopt`]),
    /// CONSUMED by the first output the edge produces ([`mark_output`]).
    ///
    /// Consumed-once is the crux: an edge produces one "time to first output" reading, so a macro's
    /// 40 later keystrokes must not each record a (steadily growing) duration against the same
    /// origin. Clearing it at the first output means `PRESS_TO_OUTPUT` measures exactly what its
    /// name says, and every subsequent output is untimed rather than wrong.
    static EDGE_ORIGIN: Cell<Option<Instant>> = const { Cell::new(None) };

    /// Whether this thread is servicing an input edge AT ALL — which is a different question from
    /// "does the edge still owe us its first-output timestamp", and must therefore be a different
    /// flag.
    ///
    /// These two were originally one value, and that was a bug. [`mark_output`] consumes
    /// `EDGE_ORIGIN` (correctly — an edge yields exactly one time-to-first-output), so anything
    /// asking "am I on the input path?" through the same value got `false` for the REST of the edge
    /// once a keystroke had gone out. One trigger can match several rules
    /// (`DispatchExecutor::fire` runs them all in a row), so a binding whose Key rule preceded its
    /// Shell rule went straight back to running `CreateProcess` synchronously on the dispatch pump —
    /// the multi-millisecond stall that scheduling decision exists to prevent, reintroduced silently
    /// and only for that ordering.
    ///
    /// So: `EDGE_ORIGIN` is a MEASUREMENT with consume-once semantics, `ON_INPUT_PATH` is a
    /// SCHEDULING fact that holds for the whole edge. Same trigger, different lifetimes, separate
    /// cells.
    static ON_INPUT_PATH: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` as the servicing of one input edge that became visible at `origin`, so the first output
/// it produces records [`PRESS_TO_OUTPUT`]. Restores whatever origin was in place on the way out
/// (so a nested call can't strand a stale one) and records [`EDGE_TO_DONE`].
pub fn with_edge<T>(origin: Instant, f: impl FnOnce() -> T) -> T {
    let prev = EDGE_ORIGIN.replace(Some(origin));
    let prev_on_path = ON_INPUT_PATH.replace(true);
    // Restore on unwind as well as on return: a panicking action must not leave the next edge
    // measuring against a dead origin, nor leave an unrelated thread looking like it is on the
    // input path. (The dispatch pump catches and contains panics, so this path is reachable.)
    struct Restore(Option<Instant>, bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EDGE_ORIGIN.set(self.0.take());
            ON_INPUT_PATH.set(self.1);
        }
    }
    let _restore = Restore(prev, prev_on_path);
    let out = f();
    EDGE_TO_DONE.record(origin.elapsed());
    out
}

/// The edge origin this thread is servicing, if any — so work handed to another thread (a macro
/// worker) can carry the measurement with it via [`adopt`].
#[must_use]
pub fn origin() -> Option<Instant> {
    EDGE_ORIGIN.get()
}

/// Adopt an [`origin`] captured on another thread. Called at the top of a spawned macro worker so
/// the macro's first keystroke still records `PRESS_TO_OUTPUT` against the real press.
pub fn adopt(origin: Option<Instant>) {
    EDGE_ORIGIN.set(origin);
    // A worker carrying a real edge IS on the input path, for the whole job — including after its
    // first keystroke has consumed the origin. `adopt(None)` (a worker being cleaned up between
    // jobs) correctly takes it back off the input path.
    ON_INPUT_PATH.set(origin.is_some());
}

/// Is this thread currently servicing a physical input edge — i.e. is it the dispatch pump (or a
/// worker that inherited the pump's stamp)?
///
/// The action layer uses this to decide whether an expensive fire-and-forget side effect must be
/// moved off the current thread. A process spawn costs milliseconds (measured: mean 5.7ms, p99
/// 17.4ms), which is invisible from the CLI or a GUI button but stalls EVERY other binding when paid
/// on the pump. The same action therefore wants different scheduling depending on who invoked it, and
/// this is how the action layer can tell — without threading a "who is calling" flag through
/// `run_ctx` and every action in the tree.
///
/// It is deliberately a question about the input path, not about a specific thread identity: a macro
/// worker running a sequence on the pump's behalf answers `true` for the whole job.
///
/// Reads [`ON_INPUT_PATH`], NOT the edge origin — see that cell's doc. Asking the origin would answer
/// `false` for every rule after the one that emitted the first keystroke, quietly putting a process
/// spawn back on the dispatch pump whenever a binding's Key rule happened to be ordered before its
/// Shell rule.
#[must_use]
pub fn servicing_input_edge() -> bool {
    ON_INPUT_PATH.get()
}

/// An output event (a keystroke, a click) is entering the OS input stream right now. If this thread
/// is servicing an input edge that has not produced output yet, records [`PRESS_TO_OUTPUT`] and
/// consumes the origin. Free and safe to call from output primitives unconditionally.
pub fn mark_output() {
    if let Some(origin) = EDGE_ORIGIN.take() {
        PRESS_TO_OUTPUT.record(origin.elapsed());
    }
}

// ── reporting ───────────────────────────────────────────────────────────────────────────────────

/// Format one duration in microseconds for the report — µs under a millisecond, else milliseconds
/// with one decimal, so the table reads at a glance instead of needing mental arithmetic.
fn fmt_us(us: u64) -> String {
    if us < 1_000 {
        format!("{us}us")
    } else {
        format!("{:.1}ms", us as f64 / 1_000.0)
    }
}

/// Write a compact per-stage summary straight into `w`, WITHOUT allocating.
///
/// This exists for the crash path, and the distinction from [`report`] is the whole point. `report`
/// builds a formatted `String` (and a small one per cell) — fine on a 5-second timer, wrong inside a
/// panic hook or an unhandled-exception filter, where the flight recorder's contract is explicitly
/// "no locks, no allocation, safe to read from a crashing process". A heap-corruption crash is one of
/// the ways this app can actually die, and calling into the allocator from the handler risks hanging
/// or faulting the very code trying to record what happened.
///
/// So: integers and `&'static str` only, formatted directly into the writer. Less pretty, and it
/// cannot make a crash worse. Errors are ignored — a diagnostic that fails during a crash has nowhere
/// to complain to.
pub fn write_crash_summary(w: &mut dyn std::io::Write) {
    let _ = writeln!(w, "── input latency (microseconds) ── stage count mean p99 max");
    for h in STAGES {
        let n = h.count();
        if n == 0 {
            continue; // a stage with no samples says nothing worth the line in a crash report
        }
        let _ = writeln!(
            w,
            "  {} {} {} {} {}",
            h.label(),
            n,
            h.mean_us(),
            h.quantile_us(0.99),
            h.max_us()
        );
    }
}

/// A human-readable table of every stage: sample count, mean, p50/p90/p99, and the exact worst.
/// Stages with no samples are listed as idle rather than omitted — "we never measured this" and
/// "this is fast" are very different answers, and hiding the first would let a mis-wired
/// instrument read as a clean bill of health.
pub fn report() -> String {
    let mut out = String::from(
        "stage             count      mean      p50      p90      p99      max\n\
         ---------------------------------------------------------------------\n",
    );
    for h in STAGES {
        let n = h.count();
        if n == 0 {
            out.push_str(&format!("{:<16} {:>6}   (no samples)\n", h.label, 0));
            continue;
        }
        out.push_str(&format!(
            "{:<16} {:>6} {:>9} {:>8} {:>8} {:>8} {:>8}\n",
            h.label,
            n,
            fmt_us(h.mean_us()),
            fmt_us(h.quantile_us(0.50)),
            fmt_us(h.quantile_us(0.90)),
            fmt_us(h.quantile_us(0.99)),
            fmt_us(h.max_us()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stage histograms are `static`, and cargo runs these tests in PARALLEL — so any test that
    /// asserts on a shared stage's count races every other test that records into it. (This is not
    /// hypothetical: adding two tests that merely call `mark_output` broke an unrelated count
    /// assertion.) Every test that resets, records into, or reads a SHARED stage takes this first.
    /// Tests using a local `Hist` need no lock — they share nothing.
    static SHARED_STAGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn shared_stages() -> std::sync::MutexGuard<'static, ()> {
        SHARED_STAGE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn bucketing_is_monotone_and_total() {
        // A non-monotone bucketing corrupts every percentile silently, so pin it across the whole
        // interesting range plus the extremes.
        let mut last = 0usize;
        for us in 0u64..100_000 {
            let b = bucket_of(us);
            assert!(b < BUCKETS, "{us}us => bucket {b} is out of range");
            assert!(b >= last, "bucket_of went DOWN at {us}us ({last} -> {b})");
            last = b;
        }
        assert!(bucket_of(u64::MAX) < BUCKETS, "the top of the range must land in range");
    }

    #[test]
    fn a_sample_lands_between_its_bucket_floor_and_ceiling() {
        for us in [0, 1, 3, 4, 7, 8, 15, 16, 100, 999, 1_000, 15_600, 1_000_000, u64::MAX] {
            let b = bucket_of(us);
            assert!(
                bucket_floor_us(b) <= us,
                "{us}us: floor {} exceeded the sample",
                bucket_floor_us(b)
            );
            assert!(
                us <= bucket_ceil_us(b),
                "{us}us: ceiling {} fell below the sample",
                bucket_ceil_us(b)
            );
        }
    }

    #[test]
    fn small_durations_are_recorded_exactly() {
        // Under the first octave there is no bucketing error at all — the stages that live there
        // (an atomic store, a small hash lookup) are exactly where a +12.5% fudge would mislead.
        let h = Hist::new("t");
        for us in 0..4u64 {
            h.record_us(us);
        }
        assert_eq!(h.count(), 4);
        assert_eq!(h.quantile_us(0.0), 0);
        assert_eq!(h.max_us(), 3);
    }

    #[test]
    fn quantiles_track_a_known_distribution() {
        let h = Hist::new("t");
        // 99 fast samples at 1ms and one 100ms outlier: p50/p90 must stay at the fast mode and the
        // tail must expose the outlier. This is precisely the shape of a "usually fine, sometimes
        // laggy" complaint, so it is the shape the instrument has to get right.
        for _ in 0..99 {
            h.record(Duration::from_millis(1));
        }
        h.record(Duration::from_millis(100));
        assert_eq!(h.count(), 100);
        let p50 = h.quantile_us(0.50);
        assert!((1_000..1_200).contains(&p50), "p50 {p50}us should sit at the 1ms mode");
        let p90 = h.quantile_us(0.90);
        assert!((1_000..1_200).contains(&p90), "p90 {p90}us should still be the fast mode");
        let p99 = h.quantile_us(0.99);
        assert!((1_000..1_200).contains(&p99), "p99 {p99}us is still the 99th fast sample");
        assert_eq!(h.max_us(), 100_000, "the outlier is recorded EXACTLY, not bucketed");
        assert_eq!(h.quantile_us(1.0), 100_000, "p100 is the outlier");
    }

    #[test]
    fn a_percentile_never_exceeds_the_exact_max() {
        // Bucket ceilings over-estimate by design; that must never manufacture an outlier larger
        // than anything actually measured (which would send someone hunting a phantom).
        let h = Hist::new("t");
        h.record_us(9); // bucket [8,10) — ceiling 9, equal to the sample
        h.record_us(1_001);
        assert!(h.quantile_us(1.0) <= h.max_us());
        assert!(h.quantile_us(0.5) <= h.max_us());
    }

    /// The accumulator must stay trustworthy no matter what is fed in. A single absurd sample used to
    /// be able to push `sum_us` to the top of the range, after which the very next `fetch_add`
    /// wrapped and `mean_us` reported something arbitrarily small — a diagnostic quietly lying,
    /// which is worse than having no diagnostic. Clamping the INPUT is what makes that unreachable.
    #[test]
    fn an_absurd_sample_cannot_corrupt_the_mean() {
        let h = Hist::new("t");
        h.record(Duration::MAX);
        h.record(Duration::MAX);
        h.record(Duration::from_millis(1));
        assert_eq!(h.count(), 3);
        assert_eq!(
            h.max_us(),
            SAMPLE_CEILING_US,
            "an out-of-range sample is recorded AT the ceiling, not wrapped to a small value"
        );
        // The mean must sit between the 1ms sample and the ceiling. A wrapped sum shows up here as a
        // mean far below the smallest recorded sample.
        let mean = h.mean_us();
        assert!(
            (1_000..=SAMPLE_CEILING_US).contains(&mean),
            "mean {mean}us is outside the range of the samples recorded — the sum wrapped"
        );
    }

    /// Pins the arithmetic the ceiling's safety argument rests on, so a future change to either the
    /// ceiling or the accumulator width cannot silently reintroduce the overflow.
    #[test]
    fn the_sum_cannot_overflow_within_any_possible_run() {
        let max_samples = u64::MAX / SAMPLE_CEILING_US;
        assert!(
            max_samples > 1_000_000_000,
            "only {max_samples} ceiling-sized samples fit in the sum before it wraps — that is \
             within reach of a long-lived process, so the clamp no longer protects the mean"
        );
    }

    #[test]
    fn an_empty_histogram_answers_zero_rather_than_dividing_by_zero() {
        let h = Hist::new("t");
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean_us(), 0);
        assert_eq!(h.quantile_us(0.99), 0);
        assert_eq!(h.max_us(), 0);
    }

    #[test]
    fn reset_clears_every_field() {
        let h = Hist::new("t");
        h.record(Duration::from_millis(5));
        h.reset();
        assert_eq!(h.count(), 0);
        assert_eq!(h.max_us(), 0);
        assert_eq!(h.mean_us(), 0);
        assert_eq!(h.quantile_us(0.5), 0);
    }

    #[test]
    fn the_timer_guard_records_on_drop() {
        let _shared = shared_stages();
        RESOLVE.reset();
        {
            let _t = start(&RESOLVE);
        }
        assert_eq!(RESOLVE.count(), 1, "the guard recorded exactly one sample");
        RESOLVE.reset();
    }

    #[test]
    fn the_first_output_after_an_edge_records_press_to_output_and_only_the_first() {
        let _shared = shared_stages();
        // The consumed-once contract: a macro emits many keystrokes from ONE press, and each later
        // one would otherwise record a larger duration against the same origin — turning a fast
        // macro's own runtime into fake input latency.
        PRESS_TO_OUTPUT.reset();
        EDGE_TO_DONE.reset();
        with_edge(Instant::now(), || {
            mark_output();
            mark_output();
            mark_output();
        });
        assert_eq!(PRESS_TO_OUTPUT.count(), 1, "only the FIRST output is the time-to-first-output");
        assert_eq!(EDGE_TO_DONE.count(), 1, "and the edge's total was recorded once");
        PRESS_TO_OUTPUT.reset();
        EDGE_TO_DONE.reset();
    }

    /// THE multi-rule regression. One trigger can match several rules, and `DispatchExecutor::fire`
    /// runs them all in a row on the dispatch pump. If the scheduling signal were the (consume-once)
    /// edge origin, then the moment a Key rule emitted its keystroke every LATER rule would look like
    /// it was off the input path — sending a Shell rule's `CreateProcess` back onto the pump for
    /// multiple milliseconds, but only when the rules happened to be in that order.
    #[test]
    fn the_input_path_survives_the_first_output_so_later_rules_still_schedule_off_the_pump() {
        let _shared = shared_stages();
        with_edge(Instant::now(), || {
            assert!(servicing_input_edge(), "the edge starts on the input path");
            mark_output(); // rule 1: a keystroke — consumes the origin, by design
            assert_eq!(origin(), None, "the measurement really was consumed");
            assert!(
                servicing_input_edge(),
                "still on the input path after the first output — otherwise rule 2's process spawn \
                 would run synchronously on the dispatch pump"
            );
        });
        assert!(!servicing_input_edge(), "and the edge ends when it ends");
    }

    /// A pool worker carries the input-path fact for its whole job, not just until its first
    /// keystroke — a macro's `[Key, Shell]` sequence must not put a spawn on the worker synchronously
    /// for the same reason, and `adopt(None)` must take the worker back off the path afterwards.
    #[test]
    fn an_adopted_worker_stays_on_the_input_path_across_its_whole_job() {
        let _shared = shared_stages();
        adopt(Some(Instant::now()));
        assert!(servicing_input_edge());
        mark_output();
        assert!(servicing_input_edge(), "still on the path after the sequence's first key step");
        adopt(None);
        assert!(!servicing_input_edge(), "cleanup takes the reused worker back off the path");
    }

    #[test]
    fn an_output_with_no_edge_in_flight_records_nothing() {
        let _shared = shared_stages();
        // The GUI's "test run" button, a CLI dispatch, an autostart reassert: outputs with no
        // physical press behind them must not invent a latency sample.
        PRESS_TO_OUTPUT.reset();
        adopt(None);
        mark_output();
        assert_eq!(PRESS_TO_OUTPUT.count(), 0);
    }

    #[test]
    fn a_worker_thread_can_adopt_the_edge_origin() {
        let _shared = shared_stages();
        // The real macro path: the pump dispatches, a worker emits the first keystroke.
        PRESS_TO_OUTPUT.reset();
        let origin = Instant::now();
        let captured = with_edge(origin, origin_of_current_edge);
        let t = std::thread::spawn(move || {
            adopt(captured);
            mark_output();
            PRESS_TO_OUTPUT.count()
        });
        assert_eq!(t.join().expect("worker finished"), 1, "the worker's output was timed");
        PRESS_TO_OUTPUT.reset();
    }

    fn origin_of_current_edge() -> Option<Instant> {
        origin()
    }

    #[test]
    fn nested_edges_restore_the_outer_origin() {
        let _shared = shared_stages();
        let outer = Instant::now();
        with_edge(outer, || {
            with_edge(Instant::now(), || {});
            assert_eq!(origin(), Some(outer), "the inner edge restored the outer one");
        });
        assert_eq!(origin(), None, "and the outermost cleared it");
    }

    #[test]
    fn the_report_lists_every_stage_including_the_idle_ones() {
        let text = report();
        for h in STAGES {
            assert!(text.contains(h.label), "the report omitted the `{}` stage", h.label);
        }
    }

    #[test]
    fn durations_are_formatted_for_reading() {
        assert_eq!(fmt_us(0), "0us");
        assert_eq!(fmt_us(999), "999us");
        assert_eq!(fmt_us(1_000), "1.0ms");
        assert_eq!(fmt_us(15_600), "15.6ms");
    }
}
