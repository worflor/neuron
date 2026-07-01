//! The device writer — ONE writer per surface, everything else is a client.
//!
//! This is the structural fix for the seven-uncoordinated-writers problem the
//! lifecycle map found (§9.3 of the R&D doc): nobody writes a device except
//! its writer task, and the writer paints exactly what the arbiter resolves.
//! The real HID sink arrives with the neuron-core bridge; [`MockSink`] stands
//! in for tests and adapter development.
//!
//! Mechanics mirror the proven `Lights::animate` loop (lighting.rs): frame
//! DEDUP (an unchanged resolve costs zero sink writes — firmware latches, so
//! a static scene is free) and DEADLINE pacing (`next += dt`, clamped forward
//! on overrun — a slow frame never causes a catch-up burst).

use std::sync::atomic::{AtomicBool, Ordering};
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

    /// Drop any device-state cache: the next `write` must repaint EVERYTHING.
    /// Called periodically by the writer because fire-and-forget HID writes
    /// can be silently dropped by the device — dedup then believes a row is
    /// current while the silicon disagrees, and on static content the torn
    /// frame would stick forever (observed live on the legacy BlackWidow:
    /// half red, half stale). Default no-op for stateless sinks.
    fn refresh(&mut self) {}
}

/// Forced-refresh cadence in writer ticks: dedup is an OPTIMIZATION, not a
/// truth source, so every N ticks the writer resends the full frame even if
/// nothing changed — the self-healing repaint that un-tears a board which
/// silently dropped writes. At the legacy 6fps that's ~10s to heal, at
/// matrix 30fps ~2s; cheap either way (a handful of feature reports).
pub const REFRESH_TICKS: u64 = 64;

/// The dedup core — pure, so it's testable without threads or clocks.
pub struct WriterCore {
    last: Option<Vec<Option<Rgb>>>,
}

impl WriterCore {
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

/// The paced writer thread for one surface. Dropping it stops and joins.
pub struct Writer {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Writer {
    /// `make_sink` runs ON the writer thread — the factory is what crosses
    /// the thread boundary (recipes are `Send`), never the sink itself, so a
    /// sink may own thread-affine things like HID handles.
    pub fn spawn<S: FrameSink + 'static>(
        mut api: impl HostApi + Send + 'static,
        surface: impl Into<String>,
        fps: u32,
        make_sink: impl FnOnce() -> S + Send + 'static,
    ) -> Writer {
        let surface = surface.into();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = thread::Builder::new()
            .name(format!("neuron-writer-{surface}"))
            .spawn(move || {
                let mut sink = make_sink();
                let dt = Duration::from_secs(1) / fps.clamp(1, 60);
                let mut core = WriterCore::new();
                let mut next = Instant::now();
                let mut faults: u64 = 0;
                let mut ticks: u64 = 0;
                while !stop_flag.load(Ordering::Relaxed) {
                    ticks += 1;
                    // Periodic forced repaint: drop both dedup caches so this
                    // tick resends everything (see FrameSink::refresh).
                    if ticks % REFRESH_TICKS == 0 {
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
                            }
                        }
                    }));
                    if step.is_err() {
                        faults += 1;
                        if faults == 1 {
                            eprintln!(
                                "neuron-writer-{surface}: sink/handle fault contained; \
                                 writer continues"
                            );
                        }
                    }
                    next += dt;
                    let now = Instant::now();
                    if next > now {
                        thread::sleep(next - now);
                    } else {
                        next = now; // overran — resume from now, never burst to catch up
                    }
                }
            })
            .expect("spawn writer thread");
        Writer { stop, thread: Some(thread) }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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
    fn mock_sink_records_in_order() {
        let sink = MockSink::new();
        let mut writer_side = sink.clone();
        writer_side.write(&[Some(Rgb(1, 1, 1))]);
        writer_side.write(&[None]);
        let frames = sink.frames();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], vec![Some(Rgb(1, 1, 1))]);
        assert_eq!(frames[1], vec![None]);
    }
}
