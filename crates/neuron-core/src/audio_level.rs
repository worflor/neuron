// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A shared, fast, live audio-level provider — the single source of truth the `audiometer`
//! lighting effect (and its on-screen preview) both read.
//!
//! ## Why a provider, not a per-frame read
//! The `audiometer` effect used to open its OWN [`crate::audio::MeterCtl`] and read `peak()` only
//! when a frame rendered. On a legacy keyboard that streams at ~6fps the meter was sampled ~6×/sec
//! → sluggish, aliased, barely reactive. And the GUI preview opened a SECOND meter with its own
//! smoothing, so the preview and the device drifted apart by construction.
//!
//! This module decouples sampling from the frame rate: ONE background thread samples the peak at
//! ~60Hz, applies the ballistic envelope (fast attack / slow release) HERE, and publishes the
//! smoothed `0.0..=1.0` level to a lock-free atomic. Every consumer — the device stream, the big
//! render preview, the effect-tile thumbnail — calls [`ensure`] then reads [`level`], so they all
//! see the SAME number and stay in lock-step. The device reacts smoothly even at 6fps because the
//! level is already sampled at 60Hz independent of the stream.
//!
//! ## Self-healing
//! The sampler reads [`crate::audio::MeterCtl::try_peak`] (the error-aware path). If the OS rejects
//! the call — the endpoint was invalidated by a default-device change or the endpoint slept — the
//! handle is DROPPED and re-opened (the source re-resolved) on the next tick, instead of the old
//! behaviour of zeroing forever with a dead handle.
//!
//! ## Lifecycle
//! [`ensure(source)`] starts the thread (or repoints it to "speakers"|"mic"); it's idempotent — a
//! call on the already-running source is a no-op. The thread auto-stops if nobody has called
//! [`level`] in ~2s, so it never runs longer than the page that wants it. The next [`ensure`]
//! transparently restarts it.
//!
//! ## Platform seam — where a macOS/Linux port plugs in
//! Everything in this file is platform-NEUTRAL — the thread, the atomics, the idle-stop, the boost +
//! ballistic envelope — EXCEPT one tiny seam: the [`Sampler`], a stateful handle that "reads the raw
//! peak this tick". The neutral [`run`] loop owns all the timing/smoothing and just calls
//! `Sampler::sample()`. Adding a platform is therefore implementing ONE small type, not editing the
//! loop: the Windows one ([`imp::Sampler`], Core-Audio) is selected on Windows; off-Windows the inert
//! [`stub::Sampler`] returns `None` so the board idles honestly dark. Drop a CoreAudio / PipeWire
//! `Sampler` into the cfg below and the whole provider works unchanged. (`mod stub` is ALWAYS
//! compiled — never cfg-gated — so its surface is type-checked on every build, the same drift guard
//! `audio.rs` uses; the porting TODOs sit on its `sample`.)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

// The platform SAMPLER seam: a stateful handle exposing `open(source)` + `sample() -> Option<f32>`.
// Picked by cfg — the only platform-specific line in the provider. Both modules define `Sampler`.
#[cfg(windows)]
use imp::Sampler;
#[cfg(not(windows))]
use stub::Sampler;

// ── the input boost + ballistic envelope: platform-neutral MATH the neutral `run` loop calls every
// ~16ms tick. Kept at the module top level (not inside the platform sampler) so it's unit-testable on
// every target — the WASAPI read isn't, but this is.

/// A little headroom boost so normal listening fills most of the board, then clamp to 0..=1.
const BOOST: f32 = 1.6;
/// The classic VU envelope, tuned for the 60Hz tick: fast attack — snaps up to a transient…
const ATTACK: f32 = 0.6;
/// …slow release (~musical decay) so the bars fall instead of strobing. (These lived in
/// `AudioMeter::frame`, tuned for ~6fps; sampling moved to the 60Hz provider, so the envelope did too.)
const RELEASE: f32 = 0.04;

/// Lift a raw 0..=1 peak into the meter's working range, clamped back to 0..=1.
fn boost(raw: f32) -> f32 {
    (raw * BOOST).clamp(0.0, 1.0)
}

/// One ballistic envelope step: move `smooth` toward `raw` with a fast ATTACK when the signal is
/// rising and a slow RELEASE when it's falling, clamped to 0..=1. Frame-rate-independent because the
/// sampler ticks it at a fixed ~60Hz regardless of how often `level()` is read.
fn envelope_step(smooth: f32, raw: f32) -> f32 {
    let k = if raw > smooth { ATTACK } else { RELEASE };
    (smooth + (raw - smooth) * k).clamp(0.0, 1.0)
}

/// The published smoothed level (0.0..=1.0) stored as `f32` bits — read lock-free in [`level`].
static LEVEL_BITS: AtomicU32 = AtomicU32::new(0);
/// Millis since the process epoch of the last [`level`] read — drives the idle auto-stop.
static LAST_ACCESS_MS: AtomicU64 = AtomicU64::new(0);

/// A process-lifetime monotonic origin so the thread and `level()` agree on "now" in millis.
fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// The sampler's control block. `gen` bumps whenever the source is repointed so the running thread
/// notices and re-opens; `running` gates whether a thread is alive.
struct Inner {
    running: bool,
    source: String,
    gen: u64,
}
fn inner() -> &'static Mutex<Inner> {
    static I: OnceLock<Mutex<Inner>> = OnceLock::new();
    I.get_or_init(|| {
        Mutex::new(Inner {
            running: false,
            source: String::new(),
            gen: 0,
        })
    })
}

const SAMPLE_INTERVAL: Duration = Duration::from_millis(16); // ~60Hz, frame-rate-independent
const IDLE_STOP_MS: u64 = 2000; // stop the thread if no `level()` read in ~2s

/// Start the sampler on `source` ("speakers" = system output [default], "mic" = capture), or
/// repoint a running sampler to it. Idempotent: a call on the already-running source is a no-op.
/// Cheap to call every frame.
pub fn ensure(source: &str) {
    let mut st = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if st.running {
        if st.source != source {
            st.source = source.to_string();
            st.gen += 1; // signal the live thread to re-open on the new endpoint
        }
        return;
    }
    // not running → (re)start. Bump gen so the thread opens fresh; refresh last-access so the
    // brand-new thread doesn't immediately idle-stop before the first `level()` read lands.
    st.source = source.to_string();
    st.gen += 1;
    st.running = true;
    let start_gen = st.gen;
    let start_source = st.source.clone();
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    drop(st);
    // The latch is cleared by the release — which runs on completion, panic, OR a spawn refusal —
    // so a failed spawn can never leave `running` stuck true and block every later `ensure`.
    crate::worker::spawn_guarded(
        "neuron-audio-level",
        || inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner).running = false,
        move || run(start_gen, start_source),
    );
}

/// The latest smoothed level (0.0..=1.0). Lock-free and cheap. Reading it also keeps the sampler
/// alive (resets the idle timer), so a consumer that stops reading lets the thread auto-stop.
pub fn level() -> f32 {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    f32::from_bits(LEVEL_BITS.load(Ordering::Relaxed))
}

/// The sampler loop — PLATFORM-NEUTRAL. Open a [`Sampler`] for the source, then each ~16ms read the
/// raw peak through the platform seam, apply the boost + envelope (neutral math), and publish.
/// Re-opens the sampler on a repoint; exits (clearing the level) on idle. The ONLY platform-specific
/// call here is `Sampler::sample()`.
fn run(mut my_gen: u64, mut source: String) {
    let mut sampler = Sampler::open(&source);
    let mut smooth = 0.0f32;
    loop {
        // ── control: idle auto-stop + source repoint (lock held only briefly) ──
        let repoint;
        {
            let st = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle = now_ms().saturating_sub(LAST_ACCESS_MS.load(Ordering::Relaxed));
            if idle > IDLE_STOP_MS {
                // `running` is cleared by the spawn's release, not here — see `ensure`.
                drop(st);
                LEVEL_BITS.store(0f32.to_bits(), Ordering::Relaxed);
                return;
            }
            repoint = if st.gen != my_gen {
                my_gen = st.gen;
                source = st.source.clone();
                true
            } else {
                false
            };
        }
        if repoint {
            sampler = Sampler::open(&source); // drops the old handle, opens the new endpoint
        }

        // ── sample (platform seam) → boost → envelope. `None` = no signal this tick (dead/absent
        // handle, which the sampler will re-resolve next tick); treat it as silence so the envelope
        // decays rather than holding a stale peak. ──
        let raw = sampler.sample().map(boost).unwrap_or(0.0);
        smooth = envelope_step(smooth, raw);
        LEVEL_BITS.store(smooth.to_bits(), Ordering::Relaxed);

        thread::sleep(SAMPLE_INTERVAL);
    }
}

#[cfg(windows)]
mod imp {
    use crate::audio::{self, MeterCtl};

    /// The Windows audio-peak SAMPLER — the platform seam. Owns the live Core-Audio meter handle and
    /// self-heals: a failed read drops the handle so the next [`sample`](Self::sample) re-resolves the
    /// now-current endpoint. All timing/smoothing lives in the neutral `super::run` loop.
    pub struct Sampler {
        source: String,
        meter: Option<MeterCtl>,
    }

    impl Sampler {
        /// Open a sampler for `source`: "mic" resolves the capture endpoint; anything else follows
        /// the current default render output. Opens the handle eagerly (re-resolved lazily on failure).
        pub fn open(source: &str) -> Self {
            let mut s = Sampler {
                source: source.to_string(),
                meter: None,
            };
            s.meter = s.open_meter();
            s
        }

        /// Resolve a fresh meter handle for this sampler's source. `None` if it can't resolve (no mic,
        /// no default output, …).
        fn open_meter(&self) -> Option<MeterCtl> {
            if self.source.eq_ignore_ascii_case("mic") {
                audio::resolve_capture(None).and_then(|ep| MeterCtl::open(&ep.id))
            } else {
                MeterCtl::open_default_render()
            }
        }

        /// THE SEAM: read the raw peak (0.0..=1.0) for this tick, or `None` when no signal is available
        /// — either the handle hasn't resolved yet, or the endpoint was invalidated (a default-device
        /// change / sleep), in which case the handle is dropped so the next call re-opens the
        /// now-current endpoint. The neutral loop applies the boost + envelope.
        pub fn sample(&mut self) -> Option<f32> {
            if self.meter.is_none() {
                self.meter = self.open_meter(); // (re)resolve a handle each tick until one opens
            }
            match self.meter.as_ref()?.try_peak() {
                Some(p) => Some(p),
                // a failure HRESULT means the endpoint died → drop the handle so the next tick re-opens.
                None => {
                    self.meter = None;
                    None
                }
            }
        }
    }
}

/// The inert off-Windows SAMPLER — no Core-Audio backend here, so there's nothing to sample, and the
/// neutral `run` loop publishes a steady silent `0.0` (the `audiometer` board idles honestly dark
/// rather than faking motion). ALWAYS compiled (not cfg-gated) so its surface is type-checked on every
/// build, mirroring `audio.rs`'s stub discipline; `#![allow(dead_code)]` because on Windows the live
/// seam is `imp::Sampler` and this stands unused.
mod stub {
    #![allow(dead_code)]

    /// The off-platform sampler stand-in. A real port REPLACES the `sample` body below.
    pub struct Sampler;

    impl Sampler {
        pub fn open(_source: &str) -> Self {
            Sampler
        }

        /// THE SEAM (port here). Returns `None` → the board reads dark.
        // TODO(macos): implement via CoreAudio — read a peak from an AudioQueue/AURemoteIO tap on the
        //   default output (or input for "mic"); return the 0.0..=1.0 sample.
        // TODO(linux): implement via PipeWire (or PulseAudio) — subscribe to the monitor source of the
        //   default sink ("speakers") or the default source ("mic") and return its peak.
        pub fn sample(&mut self) -> Option<f32> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boost_scales_then_clamps() {
        assert_eq!(boost(0.0), 0.0, "silence stays silent");
        assert!((boost(0.25) - 0.25 * BOOST).abs() < 1e-6, "a quiet signal scales by the boost");
        assert_eq!(boost(1.0), 1.0, "a full-scale peak is already at the ceiling");
        assert_eq!(boost(5.0), 1.0, "a boosted peak past 1.0 clamps, never overshoots");
    }

    #[test]
    fn envelope_attack_is_faster_than_release() {
        // The defining VU asymmetry: one step toward a loud transient (attack) moves much further
        // than one step releasing from loud back toward silence (release).
        let rise = envelope_step(0.0, 1.0); // one attack step from rest toward full
        let fall = 1.0 - envelope_step(1.0, 0.0); // distance fallen in one release step from full
        assert!(rise > fall, "attack ({rise}) must move more per step than release ({fall})");
        assert!((rise - ATTACK).abs() < 1e-6, "an attack step from 0→1 lands at the attack coefficient");
        assert!((fall - RELEASE).abs() < 1e-6, "a release step from 1→0 lands at the release coefficient");
    }

    #[test]
    fn envelope_converges_up_then_decays_down() {
        // Held loud → the level rises monotonically toward ~1.0…
        let mut s = 0.0f32;
        for _ in 0..200 {
            let next = envelope_step(s, 1.0);
            assert!(next >= s && (0.0..=1.0).contains(&next), "rising stays in range and monotone");
            s = next;
        }
        assert!(s > 0.99, "a sustained loud signal converges to ~full ({s})");
        // …then silence → it falls monotonically back toward ~0 (the slow musical release).
        for _ in 0..3000 {
            let next = envelope_step(s, 0.0);
            assert!(next <= s && (0.0..=1.0).contains(&next), "falling stays in range and monotone");
            s = next;
        }
        assert!(s < 0.01, "sustained silence decays back to ~dark ({s})");
    }

    #[test]
    fn envelope_clamps_out_of_range_input() {
        // A raw read outside 0..=1 (shouldn't happen post-boost, but be defensive) never escapes range.
        assert!((0.0..=1.0).contains(&envelope_step(0.5, 9.0)));
        assert!((0.0..=1.0).contains(&envelope_step(0.5, -9.0)));
    }
}
