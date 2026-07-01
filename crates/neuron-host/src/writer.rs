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
pub trait FrameSink: Send {
    fn write(&mut self, frame: &[Option<Rgb>]);
}

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
    pub fn spawn(
        mut api: impl HostApi + Send + 'static,
        surface: impl Into<String>,
        fps: u32,
        mut sink: impl FrameSink + 'static,
    ) -> Writer {
        let surface = surface.into();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = thread::Builder::new()
            .name(format!("neuron-writer-{surface}"))
            .spawn(move || {
                let dt = Duration::from_secs(1) / fps.clamp(1, 60);
                let mut core = WriterCore::new();
                let mut next = Instant::now();
                while !stop_flag.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if let Some(frame) = api.resolve(&surface, now) {
                        if core.offer(&frame) {
                            sink.write(&frame);
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
