// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The device writer — ONE writer per surface, everything else is a client.
//!
//! This is the structural fix for the old seven-uncoordinated-writers problem:
//! nobody writes a device except
//! its writer task, and the writer paints exactly what the arbiter resolves.
//! The real HID sink arrives with the neuron-core bridge; [`MockSink`] stands
//! in for tests and adapter development.
//!
//! Mechanics mirror the proven `Lights::animate` loop (lighting.rs): frame
//! DEDUP (an unchanged resolve costs zero sink writes — firmware latches, so
//! a static scene is free) and DEADLINE pacing (`next += dt`, clamped forward
//! on overrun — a slow frame never causes a catch-up burst).

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::api::HostApi;
use crate::arbiter::Rgb;

/// Where resolved frames go. `None` cells are "unclaimed" — the sink decides
/// the fallback (a real device sink will typically leave the firmware's
/// latched state alone, the onboard-first answer).
///
/// Deliberately NOT `Send`: sinks are BORN on the writer thread (see
/// [`Writer::spawn`]'s factory parameter), so a device handle inside a sink
/// never exists outside the one thread that owns it — the one-writer
/// guarantee enforced by construction, matching neuron-core's own
/// open-inside-the-thread pattern.
pub trait FrameSink {
    fn write(&mut self, frame: &[Option<Rgb>]);
    /// Fold ONE round-robin row into the next write, to un-tear the board after a
    /// possibly-dropped HID write. Fire-and-forget HID writes can be silently dropped by the
    /// silicon; dedup then believes a row is current while the board disagrees. Always one row —
    /// never a full-board resend, which on a legacy per-row board is a ~12ms row-by-row wipe that
    /// reads as a periodic full-board FLASH. The writer sweeps the whole board one row per tick
    /// after content stops (see `HealPolicy`). Default no-op for stateless sinks.
    fn refresh(&mut self) {}
}

/// Self-heal cadence (writer ticks) while content is FLOWING. Dedup is an
/// OPTIMIZATION, not a truth source: fire-and-forget HID writes can be
/// silently dropped by the silicon, and the sink's cache then believes a row
/// is current while the board disagrees. Drops cluster exactly when frames
/// flow (busy USB link, busy CPU — a Chroma game mid-match is the worst
/// case), so while frames are changing the writer forces a full repaint every
/// 12 ticks — a torn frame lives ≤0.4s at 30fps instead of the old fixed
/// 64-tick (~2s) window that read as sustained flicker under live clients.
const ACTIVE_HEAL_TICKS: u64 = 12;

/// Ticks-since-last-change at which the STATIC-scene heal SWEEP begins. A drop
/// during the final frames of a stream (or during the first heal itself) would
/// otherwise stick forever on a latched board, so once content has held still
/// this long the writer re-asserts the whole final frame — but ROW BY ROW (see
/// `QUIET_SWEEP_LEN`), never as one full-board resend.
const QUIET_SWEEP_AT: u64 = 8;

/// Length of the static-scene heal sweep, in ticks. Once content stops the
/// writer heals every tick for this many ticks — one round-robin row each — so
/// the whole latched frame is re-asserted (≥ any keyboard's row count) without
/// ever flashing a legacy board with a full-board repaint. After the sweep the
/// writer goes fully QUIESCENT: zero HID traffic on a static scene (the
/// firmware latch holds the frame; wireless batteries and sleeping mice are
/// left in peace).
const QUIET_SWEEP_LEN: u64 = 8;

/// When to re-assert a row — pure, injected-tick, testable (the `WriterCore`
/// discipline). The writer asks `due()` at the top of a tick, resets both dedup
/// caches when it says so, then reports what the tick did via
/// `tick(healed, content_changed)`. A heal tick's rewrite is NOT content
/// evidence (the caches were dropped, `offer` trivially fires), so the caller
/// must pass `content_changed = false` on heal ticks.
pub struct HealPolicy {
    since_change: u64,
    since_heal: u64,
}

impl HealPolicy {
    #[must_use]
    pub fn new() -> Self {
        // Born idle: no heals until the first real frame lands (an unclaimed
        // or never-painted board has nothing to un-tear).
        HealPolicy { since_change: u64::MAX / 2, since_heal: 0 }
    }

    /// Should THIS tick re-assert a row? Every heal is a SINGLE round-robin row
    /// (the sink spreads them) — there is no full-board resend, because on a
    /// legacy per-row board a full resend is a ~12ms row-by-row wipe that reads
    /// as a periodic full-board FLASH. Two regimes:
    ///   • FLOWING (content still changing): re-assert one row every
    ///     `ACTIVE_HEAL_TICKS`, so a dropped static row un-tears within ~0.4s.
    ///   • JUST STOPPED (`QUIET_SWEEP_AT`..`+LEN` ticks since the last change):
    ///     heal every tick — a short one-row-per-tick SWEEP that re-asserts the
    ///     whole final frame — then go quiescent (silent on a static board).
    #[must_use]
    pub fn due(&self) -> bool {
        if self.since_change < QUIET_SWEEP_AT {
            self.since_heal >= ACTIVE_HEAL_TICKS
        } else {
            self.since_change < QUIET_SWEEP_AT + QUIET_SWEEP_LEN
        }
    }

    /// Record the tick's outcome: whether it healed, and whether the frame
    /// genuinely changed (heal-forced rewrites don't count).
    pub fn tick(&mut self, healed: bool, content_changed: bool) {
        self.since_heal = if healed { 0 } else { self.since_heal.saturating_add(1) };
        self.since_change = if content_changed && !healed {
            0
        } else {
            self.since_change.saturating_add(1)
        };
    }
}

impl Default for HealPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// The writer's fps ceiling — MUST equal `neuron::lighting::MAX_STREAM_FPS`, the pipeline-wide
/// clamp domain (render quantization and write cadence must never clamp into different ranges;
/// the old writer-only 1..=60 window let a >30 pace burn kernel resolves on frames the content
/// layer never rendered). The kernel builds pure-std (the neuron dep is feature-gated behind
/// `bridge`), so the value is MIRRORED here and locked by a parity test in the bridge, which
/// sees both crates.
pub const MAX_WRITER_FPS: u32 = 30;

/// Deadline-pacing math — the same pure function as `neuron::lighting::pace`, mirrored for the
/// pure-std kernel build and locked by a bridge parity test. Ahead of schedule → sleep the
/// remainder; overrun → no sleep AND accumulated lag clamped to one `dt`, preserving sub-frame
/// cadence phase without catch-up bursts. (The inline predecessor discarded ALL phase on overrun
/// — exactly the copy-drift the shared helper exists to prevent.)
#[must_use]
pub fn pace(deadline: Instant, now: Instant, dt: Duration) -> (Instant, Duration) {
    if deadline > now {
        (deadline, deadline - now)
    } else {
        let lag = now - deadline;
        let clamped = if lag > dt { now.checked_sub(dt).unwrap_or(deadline) } else { deadline };
        (clamped, Duration::ZERO)
    }
}

/// The dedup core — pure, so it's testable without threads or clocks.
pub struct WriterCore {
    last: Option<Vec<Option<Rgb>>>,
}

impl WriterCore {
    #[must_use]
    pub fn new() -> Self {
        WriterCore { last: None }
    }

    /// `true` iff this frame differs from the last accepted one (caller then
    /// writes it to the sink).
    pub fn offer(&mut self, frame: &[Option<Rgb>]) -> bool {
        if self.last.as_deref() == Some(frame) {
            return false;
        }
        self.last = Some(frame.to_vec());
        true
    }

    /// Forget the cache: the next offer is guaranteed to report "changed".
    pub fn reset(&mut self) {
        self.last = None;
    }
}

impl Default for WriterCore {
    fn default() -> Self {
        Self::new()
    }
}

/// A writer's PAUSE valve — the transient-I/O coordination seam. The device
/// has ONE feature-report channel; a streaming writer's `set_feature` can
/// clobber the pending reply of any concurrent getter (opened on its own
/// transient handle), so readers time out and read "—" while lighting flows.
/// A reader raises the valve; the writer parks (sets `parked`, stops touching
/// the sink) until release. Cheap, honest coordination until the full
/// writer-inversion (all I/O through the writer task) lands.
///
/// `pause` is a DEPTH counter, not a flag: gates nest. Overlapping callers
/// (a sniper press mid profile-apply, two getter sweeps on the same board)
/// each `engage`/`release` independently, and the writer only resumes when the
/// LAST guard drops. A bare bool would let the first `release` reopen the
/// writer while another caller still assumed exclusive access — reintroducing
/// the exact feature-report race this valve exists to close.
#[derive(Clone)]
pub struct WriterPauser {
    pause: Arc<AtomicUsize>,
    parked: Arc<AtomicBool>,
}

impl WriterPauser {
    /// Raise the valve one level, and wait (bounded ~100ms) until the writer
    /// confirms it has parked — the bound keeps a dead/stopped writer from
    /// hanging callers; on timeout the caller proceeds and risks at most the
    /// old racy behaviour. A nested `engage` on an already-parked writer
    /// returns as soon as it observes the standing `parked` flag.
    pub fn engage(&self) {
        self.pause.fetch_add(1, Ordering::Relaxed);
        for _ in 0..50 {
            if self.parked.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Drop one level. Frames flow again only when the count reaches zero (the
    /// writer re-bases its deadline on resume — no catch-up burst). Saturating
    /// so a stray double-release can never underflow the counter and strand the
    /// writer parked.
    pub fn release(&self) {
        // Atomic saturating decrement: decrement only while positive, so a stray
        // double-release can never wrap the counter and strand the writer parked.
        let _ = self
            .pause
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| (v > 0).then(|| v - 1));
    }
}

/// Bound on [`Writer`]'s teardown (see its `Drop`). Unlike the net.rs servers, this loop has no
/// provable bound: `sink.write`/`sink.refresh` are an opaque [`FrameSink`] call the writer thread
/// does not control, and a wedged sink (a blocking HID write past its driver timeout, or — as
/// exercised by the `writer_drop_is_bounded_when_the_sink_write_wedges` test — any sink that
/// simply never returns) is checked against `stop` only on the NEXT tick, which never comes. Real
/// device writes are short USB transfers with their own sub-second driver-level timeouts, so this
/// deadline is an empirical margin over that, not a derived guarantee — it exists specifically so
/// `join_bounded`'s backstop, not the loop's own logic, is what bounds a stuck sink.
const WRITER_DROP_DEADLINE: Duration = Duration::from_secs(1);

/// The paced writer thread for one surface. Dropping it stops and joins.
pub struct Writer {
    stop: Arc<AtomicBool>,
    /// Depth of live pause gates (0 = flowing). See [`WriterPauser`].
    pause: Arc<AtomicUsize>,
    parked: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Writer {
    /// `make_sink` runs ON the writer thread — the factory is what crosses
    /// the thread boundary (recipes are `Send`), never the sink itself, so a
    /// sink may own thread-affine things like HID handles.
    pub fn spawn<S: FrameSink + 'static>(
        api: impl HostApi + Send + 'static,
        surface: impl Into<String>,
        fps: u32,
        make_sink: impl FnOnce() -> S + Send + 'static,
    ) -> std::io::Result<Writer> {
        Self::spawn_paced(api, surface, Arc::new(AtomicU32::new(fps)), make_sink)
    }

    /// Like [`Writer::spawn`], but paced by a SHARED fps atomic read once per
    /// tick — the same live re-pace contract as the app's own anim streams
    /// (the GUI fps slider writes the atomic; the running writer follows
    /// without restarting).
    pub fn spawn_paced<S: FrameSink + 'static>(
        mut api: impl HostApi + Send + 'static,
        surface: impl Into<String>,
        fps: Arc<AtomicU32>,
        make_sink: impl FnOnce() -> S + Send + 'static,
    ) -> std::io::Result<Writer> {
        let surface = surface.into();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let pause = Arc::new(AtomicUsize::new(0));
        let parked = Arc::new(AtomicBool::new(false));
        let (pause_flag, parked_flag) = (pause.clone(), parked.clone());
        let thread = crate::worker::spawn_named(&format!("neuron-writer-{surface}"), move || {
                let mut sink = make_sink();
                let mut core = WriterCore::new();
                let mut heal = HealPolicy::new();
                let mut next = Instant::now();
                let mut faults: u64 = 0;
                while !stop_flag.load(Ordering::Relaxed) {
                    // PARKED? A transient reader (getter sweep, read-back-verified
                    // setter) holds the pause valve: skip resolve/write entirely —
                    // the feature-report channel belongs to the reader until
                    // release. The heal policy's clock freezes (no tick()), and
                    // the deadline re-bases on resume, so parking never causes a
                    // catch-up burst or a spurious quiet-heal.
                    if pause_flag.load(Ordering::Relaxed) > 0 {
                        parked_flag.store(true, Ordering::Relaxed);
                        thread::sleep(Duration::from_millis(2));
                        next = Instant::now();
                        continue;
                    }
                    parked_flag.store(false, Ordering::Relaxed);
                    // ONE clamp domain across the whole pipeline (MAX_WRITER_FPS ≡ core's
                    // MAX_STREAM_FPS, parity-tested): the writer must never tick faster than the
                    // content layer quantizes, or the extra ticks are pure kernel-resolve churn
                    // on frames that render identically.
                    let dt =
                        Duration::from_secs(1) / fps.load(Ordering::Relaxed).clamp(1, MAX_WRITER_FPS);
                    // Activity-aware row re-assert: drop the writer dedup cache
                    // so this tick re-offers, and tell the sink to fold in ONE
                    // round-robin heal row (never a full-board resend — that
                    // flashes a legacy per-row board). See FrameSink::refresh
                    // and HealPolicy: fast heals while frames flow, a one-row
                    // sweep when they stop, silence on a static board.
                    let healing = heal.due();
                    if healing {
                        core.reset();
                        sink.refresh();
                    }
                    // The writer must be un-killable by its collaborators: a
                    // panicking sink (device driver edge case) or handle can
                    // cost at most THIS frame — the loop, and the device's
                    // one-writer guarantee, survive. Faults are counted and
                    // logged on first occurrence, not silently eaten.
                    let step = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let now = Instant::now();
                        if let Some(frame) = api.resolve(&surface, now) {
                            if core.offer(&frame) {
                                sink.write(&frame);
                                return true;
                            }
                        }
                        false
                    }));
                    if let Ok(wrote) = step { heal.tick(healing, wrote && !healing) } else {
                        heal.tick(healing, false);
                        faults += 1;
                        if faults == 1 {
                            eprintln!(
                                "neuron-writer-{surface}: sink/handle fault contained; \
                                 writer continues"
                            );
                        }
                    }
                    // Deadline pacing via the SAME pure math `Lights::animate` uses (see `pace`
                    // above — mirrored for the pure-std kernel, parity-locked in the bridge).
                    next += dt;
                    let (nd, nap) = pace(next, Instant::now(), dt);
                    next = nd;
                    if !nap.is_zero() {
                        thread::sleep(nap);
                    }
                }
            })?;
        Ok(Writer { stop, pause, parked, thread: Some(thread) })
    }

    /// The pause valve for transient-I/O coordination (see [`WriterPauser`]).
    #[must_use]
    pub fn pauser(&self) -> WriterPauser {
        WriterPauser { pause: self.pause.clone(), parked: self.parked.clone() }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            crate::worker::join_bounded(t, WRITER_DROP_DEADLINE, "neuron-writer");
        }
    }
}

/// Test/development sink: records every written frame. Clones share the store,
/// so a test keeps one clone and hands the other to the writer.
#[derive(Clone, Default)]
pub struct MockSink {
    frames: Arc<Mutex<Vec<Vec<Option<Rgb>>>>>,
}

impl MockSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn frames(&self) -> Vec<Vec<Option<Rgb>>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn last(&self) -> Option<Vec<Option<Rgb>>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).last().cloned()
    }
}

impl FrameSink for MockSink {
    fn write(&mut self, frame: &[Option<Rgb>]) {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner).push(frame.to_vec());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    /// A sink whose `write` NEVER returns — parked on a condvar nobody ever notifies. Stands in
    /// for the worst case `WRITER_DROP_DEADLINE` is sized against: a wedged device write the
    /// writer thread's loop cannot interrupt (the stop flag is only rechecked between ticks).
    struct BlockingSink {
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl FrameSink for BlockingSink {
        fn write(&mut self, _frame: &[Option<Rgb>]) {
            let (lock, cvar) = &*self.gate;
            let guard = lock.lock().unwrap_or_else(PoisonError::into_inner);
            // `gate.0` never becomes true and `cvar` is never notified: this parks forever,
            // exactly like a hung blocking HID write.
            drop(cvar.wait(guard));
        }
    }

    #[test]
    fn writer_drop_is_bounded_when_the_sink_write_wedges() {
        use crate::api::{LeaseSpec, SurfaceInfo, SurfaceKind};
        use crate::arbiter::{band, Content};
        use crate::shell::Host;

        let host = Host::spawn().expect("spawn host");
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let owner = h.next_source();
        h.claim(
            "kbd",
            owner,
            band::BASE,
            LeaseSpec::Pinned,
            Content::Fill(Rgb(9, 9, 9)),
            Instant::now(),
        )
        .unwrap();

        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let writer = Writer::spawn(host.handle(), "kbd", 100, move || BlockingSink { gate }).expect("spawn writer");

        // Give the writer a moment to reach its first `sink.write` and wedge inside it.
        thread::sleep(Duration::from_millis(100));

        // Drop on a background thread and wait for THAT thread with our own bounded poll, so a
        // regression (join_bounded broken, or removed) fails this test cleanly instead of
        // hanging the whole test binary.
        let start = Instant::now();
        let dropper = crate::worker::spawn_named("t-writer-drop", move || drop(writer))
            .expect("spawn dropper");
        let bound = WRITER_DROP_DEADLINE * 3;
        let deadline = Instant::now() + bound;
        let mut finished = false;
        while Instant::now() < deadline {
            if dropper.is_finished() {
                finished = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            finished,
            "Writer::drop must return within {bound:?} even when the sink write wedges forever"
        );
        assert!(
            start.elapsed() < bound,
            "Writer::drop took {:?}, expected under {bound:?}",
            start.elapsed()
        );
        // The BlockingSink's thread is intentionally leaked (still parked on the condvar) — that
        // IS the fix: a leaked thread at shutdown instead of a hung process.
    }

    #[test]
    fn core_dedups_identical_frames() {
        let mut c = WriterCore::new();
        let red = vec![Some(Rgb(255, 0, 0)); 4];
        let blue = vec![Some(Rgb(0, 0, 255)); 4];
        assert!(c.offer(&red));
        assert!(!c.offer(&red), "identical frame must not rewrite");
        assert!(c.offer(&blue));
        assert!(c.offer(&red), "a change back is a change");
    }

    #[test]
    fn heal_policy_idle_from_birth_never_heals() {
        // An unclaimed / never-painted board has nothing to un-tear — the
        // writer must stay silent, not burn HID traffic on a latched frame.
        let mut p = HealPolicy::new();
        for _ in 0..200 {
            let h = p.due();
            assert!(!h, "no heals before the first content frame");
            p.tick(h, false);
        }
    }

    #[test]
    fn heal_policy_heals_periodically_while_streaming() {
        // A live client streaming frames every tick: torn rows must be
        // bounded by the active cadence, not the old multi-second window.
        let mut p = HealPolicy::new();
        let mut heals = 0;
        for _ in 0..100 {
            let h = p.due();
            if h {
                heals += 1;
            }
            p.tick(h, !h); // every non-heal tick carries a real content change
        }
        assert!(
            (6..=10).contains(&heals),
            "expected a heal roughly every {ACTIVE_HEAL_TICKS} ticks over 100, got {heals}"
        );
    }

    #[test]
    fn heal_policy_sweep_then_quiescent_after_static() {
        // One content change, then silence: a CONTIGUOUS sweep of exactly
        // QUIET_SWEEP_LEN heals (one row each — enough to re-assert the whole
        // final frame without a full-board resend), then full quiet forever.
        let mut p = HealPolicy::new();
        let h0 = p.due();
        p.tick(h0, true);
        let mut heals: Vec<u64> = Vec::new();
        for i in 1..400u64 {
            let h = p.due();
            if h {
                heals.push(i);
            }
            p.tick(h, false);
        }
        assert_eq!(
            heals.len() as u64,
            QUIET_SWEEP_LEN,
            "the static-scene sweep is exactly one heal per row, then silence: {heals:?}"
        );
        for w in heals.windows(2) {
            assert_eq!(w[1], w[0] + 1, "the sweep is contiguous (no full-board double-shot): {heals:?}");
        }
    }

    #[test]
    fn heal_policy_slow_stream_still_heals_each_cycle() {
        // Content changing every ~10 ticks (a slow effect): each cycle must
        // still cross a heal, so a dropped row never outlives one cycle long.
        // Every heal is a single round-robin row now — the old code turned this
        // exact case into a periodic full-board FLASH; here it's invisible.
        let mut p = HealPolicy::new();
        let mut heals = 0;
        for i in 0..200u64 {
            let h = p.due();
            if h {
                heals += 1;
            }
            p.tick(h, !h && i % 10 == 0);
        }
        assert!(heals >= 15, "a slow stream heals about once per cycle, got {heals}");
    }


    #[test]
    fn pause_gates_nest_writer_resumes_only_after_last_release() {
        use crate::api::{LeaseSpec, SurfaceInfo, SurfaceKind};
        use crate::arbiter::{band, Content};
        use crate::shell::Host;

        let host = Host::spawn().expect("spawn host");
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let owner = h.next_source();
        h.claim("kbd", owner, band::BASE, LeaseSpec::Pinned, Content::Fill(Rgb(1, 2, 3)), Instant::now())
            .unwrap();

        let sink = MockSink::new();
        let writer = Writer::spawn(host.handle(), "kbd", 100, move || sink).expect("spawn writer");
        let pauser = writer.pauser();
        // Let the writer come up and flow (parked = false).
        thread::sleep(Duration::from_millis(50));

        // Two overlapping gates on the SAME writer — the finding's scenario
        // (e.g. a sniper press mid profile-apply). Both raise the same valve.
        pauser.engage(); // engage() blocks until the writer confirms it parked
        assert_eq!(pauser.pause.load(Ordering::Relaxed), 1);
        assert!(pauser.parked.load(Ordering::Relaxed), "writer parks under the first gate");
        pauser.engage();
        assert_eq!(pauser.pause.load(Ordering::Relaxed), 2, "gates nest, not clobber");

        // First release must NOT resume the writer — a second gate is still live.
        // A bare-bool valve would reopen here and reintroduce the report race.
        pauser.release();
        assert_eq!(pauser.pause.load(Ordering::Relaxed), 1);
        thread::sleep(Duration::from_millis(20)); // ample time to wrongly resume
        assert!(
            pauser.parked.load(Ordering::Relaxed),
            "writer stays parked while any gate is held"
        );

        // The last release resumes it.
        pauser.release();
        assert_eq!(pauser.pause.load(Ordering::Relaxed), 0);
        let deadline = Instant::now() + Duration::from_secs(1);
        let resumed = loop {
            if !pauser.parked.load(Ordering::Relaxed) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(resumed, "writer resumes once the last gate drops");

        // Saturating: a stray extra release can't underflow the counter and
        // strand the writer parked forever.
        pauser.release();
        assert_eq!(pauser.pause.load(Ordering::Relaxed), 0, "release saturates at zero");
    }

    /// The sibling the test above stops short of: it only checks the `parked` BOOKKEEPING flag,
    /// never that frames actually stop landing in the sink. The historical io_gate/sniper race was
    /// on the real feature-report channel — a pauser that flips `parked` correctly but still lets
    /// the writer thread call `sink.write` underneath a concurrent getter would reintroduce the exact
    /// clobber the valve exists to prevent. Uses `Content::Live` (an ever-incrementing counter, not a
    /// static `Fill`) so frames keep changing every tick — a static scene would dedup+heal-sweep into
    /// quiescence on its own, making "frames stopped" ambiguous between "paused" and "nothing new to
    /// paint".
    #[test]
    fn pauser_actually_parks_the_stream_not_just_documents_it() {
        use crate::api::{LeaseSpec, SurfaceInfo, SurfaceKind};
        use crate::arbiter::{band, Content, LiveContent};
        use crate::shell::Host;

        struct Counter(u8);
        impl LiveContent for Counter {
            fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
                self.0 = self.0.wrapping_add(1);
                vec![Some(Rgb(self.0, self.0, self.0))]
            }
            fn boxed_clone(&self) -> Box<dyn LiveContent> {
                Box::new(Counter(self.0))
            }
        }

        let host = Host::spawn().expect("spawn host");
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 1));
        let owner = h.next_source();
        h.claim(
            "kbd",
            owner,
            band::BASE,
            LeaseSpec::Pinned,
            Content::Live(Box::new(Counter(0))),
            Instant::now(),
        )
        .unwrap();

        let sink = MockSink::new();
        let writer_sink = sink.clone();
        // MAX_WRITER_FPS (30) — the pipeline's real ceiling — so the millisecond bounds below read
        // directly as "N missed ticks" at the writer's actual pace, not an arbitrary test-only rate.
        let writer = Writer::spawn(host.handle(), "kbd", MAX_WRITER_FPS, move || writer_sink).expect("spawn writer");
        let pauser = writer.pauser();

        // Let the writer come up and stream a handful of ever-changing frames.
        thread::sleep(Duration::from_millis(100));
        let before_pause = sink.frames().len();
        assert!(
            before_pause >= 2,
            "writer must be actively streaming before we test pausing it: {before_pause} frames"
        );

        pauser.engage(); // blocks (up to ~100ms) until the writer confirms it parked
        // One frame may already be in flight right at the parking edge — give it room to land
        // before taking the "frozen" baseline.
        thread::sleep(Duration::from_millis(15));
        let frozen_at = sink.frames().len();

        // 150ms at 30fps is ~4-5 missed ticks: generous enough to be CI-safe while still a hard
        // failure if the valve were merely documentation (parked flips, frames keep flowing anyway).
        thread::sleep(Duration::from_millis(150));
        assert_eq!(
            sink.frames().len(),
            frozen_at,
            "no new frames may land while the pause valve is engaged — the stream must actually \
             park, not just flip a flag nobody reads"
        );

        pauser.release();
        let deadline = Instant::now() + Duration::from_secs(1);
        let resumed = loop {
            if sink.frames().len() > frozen_at {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(resumed, "frames must resume arriving within a bounded window after the last release");
    }

    // ── TASK 3(c): clock-leap laws for the writer's two "clocks" ────────────

    #[test]
    fn pace_handles_a_giant_wall_clock_jump_without_a_catch_up_burst() {
        // The one place in this module that reads a live wall clock against a
        // previously-computed deadline is `pace` — `HealPolicy` (below) is
        // tick-counted and never touches `Instant` at all. An 8h wake must
        // clamp forward by AT MOST one `dt` (never accumulate a catch-up
        // burst) and must never produce a negative/nonsensical sleep.
        let dt = Duration::from_secs(1) / 30;
        let deadline = Instant::now();
        let wake = deadline + Duration::from_hours(8);
        let (next, nap) = pace(deadline, wake, dt);
        assert_eq!(nap, Duration::ZERO, "a massive overrun must not produce a sleep at all");
        assert!(next <= wake, "the clamped deadline must not still be hours in the past");
        assert!(
            next >= wake.checked_sub(dt).expect("wake is hours past the epoch, dt is tiny"),
            "clamp must land within one dt of now, not accumulate a catch-up burst: next={next:?} wake={wake:?}"
        );
    }

    #[test]
    fn heal_policy_is_tick_counted_so_a_wall_clock_jump_between_ticks_cannot_storm() {
        // `HealPolicy::since_change`/`since_heal` are plain tick counters,
        // advanced once per `tick()` call — the policy has NO wall-clock of
        // its own. So unlike `pace`, a real system sleep between two
        // writer-loop iterations is invisible to it: waking up is just "the
        // next tick()", never a burst of many at once. This freezes a policy
        // in its post-sweep QUIESCENT state, then drives a large number of
        // further ticks (standing in for however much real time could have
        // passed) and asserts it never re-fires — the "no re-assert storm on
        // wake" law, encoded at the level this module actually has a clock.
        let mut p = HealPolicy::new();
        let h0 = p.due();
        p.tick(h0, true); // one real content frame
        // Drive it all the way through its sweep into quiescence.
        for _ in 0..(QUIET_SWEEP_AT + QUIET_SWEEP_LEN + 5) {
            let h = p.due();
            p.tick(h, false);
        }
        // "A very long time passes" — an arbitrarily large run of further
        // ticks, no new content ever again.
        for _ in 0..100_000u64 {
            let h = p.due();
            assert!(!h, "a quiescent policy must never re-fire no matter how many ticks pass");
            p.tick(h, false);
        }
    }
}
