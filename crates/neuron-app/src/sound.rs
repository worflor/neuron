// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Real-time audio output — a cpal stream driving a voice pool, fed by a lock-free ring.
//!
//! The notification engine (its own thread) calls [`SoundEngine::strike`], which packs a note into a
//! single 64-bit slot of a single-producer/single-consumer ring and bumps a Release cursor — no
//! lock, no allocation, never blocks. The audio callback (cpal's real-time thread) drains the ring
//! with an Acquire load, spawns [`neuron::tone::Voice`]s into a fixed pool, and mixes them per
//! sample (the same `Voice` an audition WAV uses, so what you heard is what plays). The whole synth
//! is hand-rolled in `neuron::tone`; cpal is only the device handoff, so this is fully cross-platform
//! (WASAPI / `CoreAudio` / ALSA) with no platform code here.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use neuron::tone::{soft_clip, Timbre, Voice};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

const RING: usize = 256; // power of two
const MASK: usize = RING - 1;
const MAX_VOICES: usize = 24; // generous polyphony for overlapping rings; excess strikes are dropped

/// Shared between the producer (engine thread) and the consumer (audio callback). The cursors and
/// volume cross threads atomically; the slots carry packed notes.
struct Shared {
    slots: [AtomicU64; RING],
    head: AtomicUsize,  // producer's write cursor (monotonic)
    tail: AtomicUsize,  // consumer's read cursor (monotonic)
    volume: AtomicU32,  // master gain, f32 bits, 0..1
}

impl Shared {
    // On overload, reject the newest note so unread notes remain intact and in order.
    fn try_push(&self, head: &mut usize, packed: u64) -> bool {
        let tail = self.tail.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= RING {
            return false;
        }

        self.slots[*head & MASK].store(packed, Ordering::Relaxed);
        *head = head.wrapping_add(1);
        self.head.store(*head, Ordering::Release);
        true
    }
}

/// Pack a note into one 64-bit word: freq (f32 bits) | velocity (u8) | timbre id (u8) | delay ms (u16).
/// A single atomic word means the consumer can never read a half-written note.
fn pack(freq: f32, vel: f32, tid: u8, delay_ms: u16) -> u64 {
    u64::from(freq.to_bits())
        | (((vel.clamp(0.0, 1.0) * 255.0) as u64) << 32)
        | (u64::from(tid) << 40)
        | (u64::from(delay_ms) << 48)
}

fn unpack(p: u64) -> (f32, f32, u8, u16) {
    let freq = f32::from_bits((p & 0xFFFF_FFFF) as u32);
    let vel = ((p >> 32) & 0xFF) as f32 / 255.0;
    let tid = ((p >> 40) & 0xFF) as u8;
    let delay = ((p >> 48) & 0xFFFF) as u16;
    (freq, vel, tid, delay)
}

/// The live audio engine. Owns the cpal stream (kept alive for its lifetime). NOT `Send` (a cpal
/// `Stream` isn't), so it lives wholly on the thread that created it — which is also the only
/// producer, so `strike` needs no synchronization beyond the ring's release cursor.
pub struct SoundEngine {
    shared: Arc<Shared>,
    head: usize, // producer-local mirror of shared.head
    _stream: cpal::Stream,
}

impl SoundEngine {
    /// Open the default output device and start the stream. Returns `None` if there's no device or
    /// the format is unsupported — the caller treats audio as simply off.
    pub fn new(volume: f32) -> Option<SoundEngine> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        let supported = device.default_output_config().ok()?;
        let shared = Arc::new(Shared {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()),
        });
        let fmt = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        let stream = match fmt {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, shared.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, shared.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, shared.clone()),
            other => {
                crate::flight::trace("audio", "unsupported sample format", other as u64);
                return None;
            }
        }?;
        stream.play().ok()?;
        crate::flight::trace("audio", "stream open", u64::from(config.sample_rate.0));
        Some(SoundEngine {
            shared,
            head: 0,
            _stream: stream,
        })
    }

    /// Schedule a note: `freq` Hz, `vel` 0..1, palette `tid`, sounding after `delay_ms`. Lock-free.
    pub fn strike(&mut self, freq: f32, vel: f32, tid: u8, delay_ms: u16) {
        self.shared
            .try_push(&mut self.head, pack(freq, vel, tid, delay_ms));
    }

    /// Live master volume (0..1).
    pub fn set_volume(&self, v: f32) {
        self.shared
            .volume
            .store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }
}

/// Build the output stream for sample type `T`, with the mixing callback. The callback owns the voice
/// pool + the consumer cursor; nothing it touches allocates or locks once warm.
fn build<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    shared: Arc<Shared>,
) -> Option<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let sr = config.sample_rate.0 as f32;
    let channels = config.channels as usize;
    let mut voices: Vec<Voice> = Vec::with_capacity(MAX_VOICES);
    let mut tail = 0usize;
    device
        .build_output_stream(
            config,
            move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
                // drain newly-struck notes into the pool (Acquire pairs with the producer's Release).
                let head = shared.head.load(Ordering::Acquire);
                while tail != head {
                    let packed = shared.slots[tail & MASK].load(Ordering::Relaxed);
                    tail = tail.wrapping_add(1);
                    if voices.len() < MAX_VOICES {
                        let (freq, vel, tid, delay_ms) = unpack(packed);
                        let delay = (f32::from(delay_ms) * sr / 1000.0) as u32;
                        voices.push(Voice::strike_after(freq, vel, Timbre::by_id(tid), sr, delay));
                    }
                }
                shared.tail.store(tail, Ordering::Release);
                let vol = f32::from_bits(shared.volume.load(Ordering::Relaxed));
                for frame in out.chunks_mut(channels) {
                    let mut s = 0.0f32;
                    for v in &mut voices {
                        s += v.next();
                    }
                    let mono = T::from_sample(soft_clip(s * vol));
                    for ch in frame.iter_mut() {
                        *ch = mono;
                    }
                }
                voices.retain(|v| !v.done());
            },
            move |err| {
                crate::flight::trace("audio", "stream error", 0);
                let _ = err;
            },
            None,
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        Shared {
            slots: std::array::from_fn(|_| AtomicU64::new(0)),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            volume: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    #[test]
    fn full_ring_drops_new_note_without_overwriting_unread_notes() {
        let ring = shared();
        let mut head = 0;

        for note in 1..=RING as u64 {
            assert!(ring.try_push(&mut head, note));
        }
        assert!(!ring.try_push(&mut head, RING as u64 + 1));
        assert_eq!(ring.head.load(Ordering::Acquire), RING);

        for tail in 0..RING {
            assert_eq!(ring.slots[tail & MASK].load(Ordering::Relaxed), tail as u64 + 1);
        }
    }

    #[test]
    fn consumed_slots_can_be_reused_after_wraparound() {
        let ring = shared();
        let mut head = 0;

        for note in 1..=RING as u64 {
            assert!(ring.try_push(&mut head, note));
        }
        let mut tail = 0;
        let published_head = ring.head.load(Ordering::Acquire);
        assert_eq!(published_head - tail, RING);
        tail = published_head;
        ring.tail.store(tail, Ordering::Release);

        assert!(ring.try_push(&mut head, RING as u64 + 1));
        assert_eq!(ring.slots[0].load(Ordering::Relaxed), RING as u64 + 1);
        assert_eq!(ring.head.load(Ordering::Acquire), RING + 1);
    }
}
