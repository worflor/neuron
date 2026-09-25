// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A shared, live audio LOUDNESS provider — the honest signal behind the `audiometer` effect.
//!
//! ## Why not just the OS peak
//! [`crate::audio_level`] publishes the OS peak-sample value — a fine emergency fallback, but it
//! tracks the WAVEFORM, not the music: it barely distinguishes a kick drum from tape hiss at the
//! same peak, and at a quiet listening volume the meter never leaves the floor. This provider taps
//! the actual PCM (WASAPI loopback on the default output, or a mic capture stream) and computes a
//! perceptually-weighted LOUDNESS out of it:
//!
//!  1. drain the capture into a mono ring each ~16ms tick (decimated toward ~48k if the mix runs
//!     higher, so the FFT's bass resolution doesn't collapse at 96/192kHz),
//!  2. Hann window + radix-2 FFT over the last [`FFT_N`] samples (~43ms at 48k),
//!  3. fold the bins into [`BANDS`] log-spaced bands (~45Hz → 16kHz) and TILT them
//!     (+[`TILT_DB_PER_OCT`] dB/octave, the pink-spectrum compensation) so cymbals and vocals weigh
//!     in the way the ear hears them instead of the bass owning the number — the same idea as
//!     broadcast loudness (LUFS) K-weighting, spelled with the parts already on hand,
//!  4. sum the tilted band energies into one loudness and INTEGRATE it (~[`INTEG_TAU_S`], the VU
//!     ballistic) so the published number is a steady measurement, not a per-window twitch,
//!  5. frame it with a slow AGC whose ceiling rides the INSTANTANEOUS peaks: the music's own crest
//!     factor becomes headroom, so a sustained passage sits mid-range, the beat's crest touches
//!     1.0 at any listening volume, and TRUE silence drains smoothly to dark (no noise-floor
//!     dance, no fake motion),
//!  6. and, from the SAME band energies, a second channel: the TONE — the log-frequency centroid
//!     (0 = the kick's register, 1 = the cymbals'), eased over ~[`TONE_TAU_S`]. Loudness says how
//!     HARD the music is hitting; tone says WHERE it's hitting — the depth a single level number
//!     can't carry.
//!
//! Same provider discipline as `audio_level`: ONE background thread samples at ~60Hz independent of
//! any consumer's frame rate, every consumer (device stream, big preview, effect tile) reads the
//! SAME published level, the thread idle-stops ~2s after the last read, and a dead endpoint
//! self-heals by re-opening. The platform seam is a single [`Sampler`] type ("append this tick's
//! mono PCM"); everything else is neutral math, unit-tested on every target.
//!
//! Ballistics (attack/release, the peak cap) deliberately live in the RENDERER, per pattern
//! instance, dt-scaled — so the `speed` knob means responsiveness and a 6fps legacy board and the
//! 60fps preview trace the same envelope. The provider publishes the freshest honest analysis.
//!
//! ## When PCM isn't available
//! If no capture stream opens (off-Windows, or an exotic endpoint), [`signal`] answers `None` and
//! the meter falls back to the peak-level provider — degraded honestly, never faked.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use imp::Sampler;
#[cfg(not(windows))]
use stub::Sampler;

/// How many log-spaced bands the loudness is summed over — the weighting's frequency resolution.
const BANDS: usize = 24;

/// FFT length — 2^11. At 48kHz that's a ~43ms window (23.4Hz bins): tight enough to feel live,
/// long enough that the lowest band still spans real bins.
const FFT_N: usize = 2048;

/// Band range: low edge of band 0 / high edge of the last band (Hz). ~45Hz keeps the kick without
/// weighing sub-bass the ear barely ranks; 16kHz is where music ends.
const F_LO: f32 = 45.0;
const F_HI: f32 = 16_000.0;

/// The pink-spectrum tilt: dB added per octave above [`TILT_REF_HZ`] (and subtracted below). Music
/// averages ~-3dB/oct of energy slope; compensating a bit less keeps the beat proud while letting
/// vocals and cymbals actually move the meter.
const TILT_DB_PER_OCT: f32 = 2.2;
const TILT_REF_HZ: f32 = 150.0;

/// The meter's visible dynamic range: the normalized floor sits this far below the AGC ceiling.
/// The ceiling rides the INSTANTANEOUS peaks while the published level is the ~150ms-integrated
/// loudness, so real music (6–12dB crest factor) lives mid-range and touches 1.0 on the beat —
/// ~18dB makes every musical dB count as visible travel (the "reacts to the song" knob); wider
/// flattens compressed music into a near-constant glow, narrower strobes.
const RANGE_DB: f32 = 18.0;

/// The VU-style integration time (seconds) — a one-pole on the LINEAR energy, so the published
/// loudness is a measurement (à la a VU needle), not a per-FFT-window twitch. This is what keeps
/// the board's brightness pumping instead of strobing; the renderer's ballistics ride on top.
const INTEG_TAU_S: f32 = 0.15;

/// The TONE (spectral centroid) smoothing time (seconds) — where the music's energy lives on the
/// log-frequency axis, eased so the colour swims with the arrangement instead of twitching per
/// window. Slower than the loudness on purpose: brightness carries the beat, colour carries the
/// timbre.
const TONE_TAU_S: f32 = 0.25;

/// The raw log-frequency centroid of real music rarely reaches either extreme (even a kick has
/// harmonics; even cymbals sit over a mix), so the useful travel compresses toward the middle.
/// This window re-stretches it: a centroid at/below `TONE_LO` reads as full bass (0.0), at/above
/// `TONE_HI` as full treble (1.0). Verified against pure-sine anchors in the tests.
const TONE_LO: f32 = 0.15;
const TONE_HI: f32 = 0.80;

/// How many consecutive packet-less ticks count as REAL silence. WASAPI loopback delivers packets
/// on its own ~10ms cadence — a single 16ms tick can land between packets mid-song, and stuffing
/// zeros for it would chop fake silence gaps into the analysis window (random level dips = visible
/// stutter). Only after ~100ms of continuous dryness is the stream genuinely idle; until then the
/// window just goes momentarily stale (imperceptible).
const DRY_TICKS_FOR_SILENCE: u32 = 6;

/// The AGC ceiling's decay rate (dB/s) and its hard floor (dB). Instant rise / slow fall: a loud
/// passage re-frames the meter at once, then over seconds it relaxes so a quiet passage grows back
/// into the full board. The floor stops near-silence from being amplified into a full board.
const AGC_DECAY_DB_PER_S: f32 = 4.0;
const AGC_MIN_CEIL_DB: f32 = -34.0;

/// Below this loudest-band level (dB, pre-tilt) the tick is gated to dark — true digital silence /
/// converter noise, not signal. Honest black beats a shimmering noise floor.
const GATE_DB: f32 = -72.0;

const SAMPLE_INTERVAL: Duration = Duration::from_millis(16); // ~60Hz analysis, frame-rate-independent
const IDLE_STOP_MS: u64 = 2000;

// ─────────────────────────────── published state (lock-free reads) ───────────────────────────────

/// The loudness REGIONS a consumer can focus on: the full mix plus the bass / mids / highs thirds
/// of the band stack. Thirds of 24 log bands over 45Hz–16kHz land on musical borders: bass ≈
/// 45–320Hz (kick + bass guitar), mids ≈ 320Hz–2.3kHz (vocals, snare body, guitars), highs ≈
/// 2.3–16kHz (cymbals, air). Each region gets its OWN integrator + AGC, so "focus: bass" frames
/// itself against the bass's recent peaks, not the whole mix's.
pub const REGIONS: usize = 4;
const REGION_BANDS: [(usize, usize); REGIONS] = [(0, BANDS), (0, 8), (8, 16), (16, BANDS)];

#[allow(clippy::declare_interior_mutable_const)]
const ZERO_BITS: AtomicU32 = AtomicU32::new(0);
/// The published per-region loudness levels (0.0..=1.0 each; mix, bass, mids, highs) as f32 bits.
static LEVEL_BITS: [AtomicU32; REGIONS] = [ZERO_BITS; REGIONS];
/// The published tone (0.0 = bass … 1.0 = treble; the smoothed spectral centroid) as f32 bits.
static TONE_BITS: AtomicU32 = AtomicU32::new(0);
/// Is a real PCM stream feeding the analysis? `false` → consumers fall back to the peak meter.
static LIVE: AtomicBool = AtomicBool::new(false);
/// Millis since the process epoch of the last [`signal`] read — drives the idle auto-stop.
static LAST_ACCESS_MS: AtomicU64 = AtomicU64::new(0);

fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

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

/// Start the analyser on `source` ("speakers" = loopback on the default output [default], "mic" =
/// the capture endpoint), or repoint a running one. Idempotent; cheap to call every frame.
pub fn ensure(source: &str) {
    let mut st = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if st.running {
        if st.source != source {
            st.source = source.to_string();
            st.gen += 1;
        }
        return;
    }
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
        "neuron-audio-spectrum",
        || inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner).running = false,
        move || run(start_gen, start_source),
    );
}

/// One published analysis frame: the per-region loudness levels (0.0..=1.0 each; indexed mix /
/// bass / mids / highs — a consumer's `focus` knob picks one) and the tone (0.0 = bass … 1.0 =
/// treble; the smoothed spectral centroid).
#[derive(Clone, Copy, Debug, Default)]
pub struct Signal {
    pub levels: [f32; REGIONS],
    pub tone: f32,
}

/// The latest [`Signal`], or `None` when no real PCM stream is live (fall back to the peak meter).
/// Lock-free; reading keeps the analyser alive.
pub fn signal() -> Option<Signal> {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    LIVE.load(Ordering::Relaxed).then(|| {
        let mut levels = [0f32; REGIONS];
        for (l, bits) in levels.iter_mut().zip(LEVEL_BITS.iter()) {
            *l = f32::from_bits(bits.load(Ordering::Relaxed));
        }
        Signal {
            levels,
            tone: f32::from_bits(TONE_BITS.load(Ordering::Relaxed)),
        }
    })
}

fn publish(levels: [f32; REGIONS], tone: f32, live: bool) {
    for (l, bits) in levels.iter().zip(LEVEL_BITS.iter()) {
        bits.store(l.to_bits(), Ordering::Relaxed);
    }
    TONE_BITS.store(tone.to_bits(), Ordering::Relaxed);
    LIVE.store(live, Ordering::Relaxed);
}

/// The analyser loop — PLATFORM-NEUTRAL. Drain PCM through the [`Sampler`] seam, decimate toward
/// ~48k, keep the last [`FFT_N`] mono samples, analyse, publish. Handles idle-stop, source repoints
/// and the no-packets-while-silent case (loopback delivers nothing when no stream plays — zeros are
/// appended for the elapsed tick so the level decays to dark instead of freezing).
fn run(mut my_gen: u64, mut source: String) {
    let mut sampler = Sampler::open(&source);
    let mut ring: Vec<f32> = Vec::with_capacity(FFT_N * 2);
    let mut chunk: Vec<f32> = Vec::new();
    let mut carry: Vec<f32> = Vec::new(); // decimation remainder between ticks
    let mut loudness = Loudness::new();
    let mut dry_ticks: u32 = 0; // consecutive packet-less ticks (see DRY_TICKS_FOR_SILENCE)
    loop {
        // ── control: idle auto-stop + source repoint ──
        let repoint;
        {
            let st = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle = now_ms().saturating_sub(LAST_ACCESS_MS.load(Ordering::Relaxed));
            if idle > IDLE_STOP_MS {
                // `running` is cleared by the spawn's release, not here — see `ensure`.
                drop(st);
                publish([0.0; REGIONS], 0.0, false);
                return;
            }
            repoint = if st.gen == my_gen {
                false
            } else {
                my_gen = st.gen;
                source = st.source.clone();
                true
            };
        }
        if repoint {
            sampler = Sampler::open(&source);
            ring.clear();
            carry.clear();
            loudness = Loudness::new();
            dry_ticks = 0;
        }

        // ── sample (platform seam) → decimate → ring → analyse → publish ──
        chunk.clear();
        match sampler.read(&mut chunk) {
            Some(rate) if rate > 0 => {
                let factor = ((rate as usize) / 48_000).clamp(1, 4);
                let eff_rate = rate / factor as u32;
                if chunk.is_empty() {
                    // no packets THIS tick. A lone dry tick is just cadence jitter (WASAPI delivers
                    // on its own ~10ms clock) — stuffing zeros for it would chop fake silence into
                    // the window mid-song (random level dips). Only sustained dryness is real
                    // silence; then zeros flow so the level drains honestly to dark.
                    dry_ticks += 1;
                    if dry_ticks >= DRY_TICKS_FOR_SILENCE {
                        let n = (eff_rate as f32 * SAMPLE_INTERVAL.as_secs_f32()) as usize;
                        ring.extend(std::iter::repeat_n(0.0, n));
                    }
                } else {
                    dry_ticks = 0;
                    decimate_into(&mut carry, &chunk, factor, &mut ring);
                }
                let overflow = ring.len().saturating_sub(FFT_N);
                if overflow > 0 {
                    ring.drain(..overflow);
                }
                let sig = analyze(&ring, eff_rate, SAMPLE_INTERVAL.as_secs_f32(), &mut loudness);
                publish(sig.levels, sig.tone, true);
            }
            _ => publish([0.0; REGIONS], 0.0, false), // no handle this tick (sampler keeps retrying)
        }

        thread::sleep(SAMPLE_INTERVAL);
    }
}

// ───────────────────────────── neutral DSP (unit-tested on every target) ─────────────────────────

/// Boxcar-decimate `chunk` by `factor` onto `out`, carrying the trailing partial group in `carry`
/// across calls so no samples are dropped at tick boundaries. `factor == 1` is a plain append.
fn decimate_into(carry: &mut Vec<f32>, chunk: &[f32], factor: usize, out: &mut Vec<f32>) {
    if factor <= 1 {
        out.extend_from_slice(chunk);
        return;
    }
    carry.extend_from_slice(chunk);
    let whole = carry.len() / factor * factor;
    for g in carry[..whole].chunks_exact(factor) {
        out.push(g.iter().sum::<f32>() / factor as f32);
    }
    carry.drain(..whole);
}

/// In-place iterative radix-2 FFT (decimation-in-time). `re`/`im` must share a power-of-two length.
/// Hand-rolled like the rest of the house DSP — ~30 lines beats a dependency for one fixed size.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two() && im.len() == n);
    // bit-reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j & m != 0 {
            j ^= m;
            m >>= 1;
        }
        j |= m;
    }
    // butterflies
    let mut len = 2;
    while len <= n {
        let stage = len.trailing_zeros() as usize - 1;
        let (wr, wi) = fft_roots().get(stage).copied().unwrap_or_else(|| {
            let ang = -std::f32::consts::TAU / len as f32;
            (ang.cos(), ang.sin())
        });
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (a, b) = (i + k, i + k + len / 2);
                let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

fn fft_roots() -> &'static [(f32, f32); FFT_N.trailing_zeros() as usize] {
    static ROOTS: OnceLock<[(f32, f32); FFT_N.trailing_zeros() as usize]> = OnceLock::new();
    ROOTS.get_or_init(|| {
        let mut roots = [(0.0, 0.0); FFT_N.trailing_zeros() as usize];
        for (stage, root) in roots.iter_mut().enumerate() {
            let len = 1usize << (stage + 1);
            let ang = -std::f32::consts::TAU / len as f32;
            *root = (ang.cos(), ang.sin());
        }
        roots
    })
}

fn hann_window() -> &'static [f32; FFT_N] {
    static WINDOW: OnceLock<[f32; FFT_N]> = OnceLock::new();
    WINDOW.get_or_init(|| {
        let mut window = [0.0; FFT_N];
        for (i, value) in window.iter_mut().enumerate() {
            *value = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_N as f32).cos();
        }
        window
    })
}

/// The per-band bin ranges `[lo, hi)` for log-spaced band edges [`F_LO`]..[`F_HI`] at `rate`.
/// Each band is clamped inside `1..N/2` (DC excluded) and guaranteed ≥1 bin wide, so every band is
/// always fed even on coarse (low-rate) spectra.
fn band_bin_ranges(rate: u32) -> [(usize, usize); BANDS] {
    let hz_per_bin = rate as f32 / FFT_N as f32;
    let max_bin = FFT_N / 2;
    let mut out = [(1usize, 2usize); BANDS];
    let ratio = F_HI / F_LO;
    for (i, o) in out.iter_mut().enumerate() {
        let f0 = F_LO * ratio.powf(i as f32 / BANDS as f32);
        let f1 = F_LO * ratio.powf((i + 1) as f32 / BANDS as f32);
        let lo = ((f0 / hz_per_bin) as usize).clamp(1, max_bin - 1);
        let hi = ((f1 / hz_per_bin).ceil() as usize).clamp(lo + 1, max_bin);
        *o = (lo, hi);
    }
    out
}

/// The geometric-centre frequency of band `i` — where its tilt is evaluated.
fn band_center_hz(i: usize) -> f32 {
    let ratio = F_HI / F_LO;
    F_LO * ratio.powf((i as f32 + 0.5) / BANDS as f32)
}

/// The pink tilt for band `i` in dB (positive above [`TILT_REF_HZ`], slightly negative below).
fn tilt_db(i: usize) -> f32 {
    TILT_DB_PER_OCT * (band_center_hz(i) / TILT_REF_HZ).log2()
}

/// Per-thread FFT sample buffers and the bin plan for the last observed sample rate.
/// `re` and `im` are rewritten in full for each analysis pass.
struct ScratchBufs {
    re: Vec<f32>,
    im: Vec<f32>,
    rate: Option<u32>,
    ranges: [(usize, usize); BANDS],
    tilts: [f32; BANDS],
}

impl ScratchBufs {
    fn new() -> ScratchBufs {
        ScratchBufs {
            re: vec![0.0f32; FFT_N],
            im: vec![0.0f32; FFT_N],
            rate: None,
            ranges: [(1, 2); BANDS],
            tilts: [0.0; BANDS],
        }
    }
}

/// Window + FFT the last [`FFT_N`] samples of `ring` (zero-padded in front while it fills) and
/// return each band's TILTED level in dB (a full-scale sine reads ~0dB pre-tilt). The weighting
/// stage the loudness sum and the silence gate both read. `scratch` holds the FFT work buffers,
/// reused call-to-call (both zeroed here since `fft` leaves them non-zero on return).
fn tilted_band_dbs(ring: &[f32], rate: u32, scratch: &mut ScratchBufs) -> [f32; BANDS] {
    let re = &mut scratch.re;
    let im = &mut scratch.im;
    re.fill(0.0);
    im.fill(0.0);
    let n = ring.len().min(FFT_N);
    let pad = FFT_N - n;
    for (k, &s) in ring[ring.len() - n..].iter().enumerate() {
        let i = pad + k;
        re[i] = s * hann_window()[i];
    }
    fft(re, im);
    if scratch.rate != Some(rate) {
        scratch.rate = Some(rate);
        scratch.ranges = band_bin_ranges(rate);
        for (i, tilt) in scratch.tilts.iter_mut().enumerate() {
            *tilt = tilt_db(i);
        }
    }
    let scale = 4.0 / FFT_N as f32; // Hann coherent gain 0.5 → sine peak bin ≈ N/4
    let mut out = [0.0f32; BANDS];
    for (i, &(lo, hi)) in scratch.ranges.iter().enumerate() {
        let power: f32 = (lo..hi).map(|k| re[k] * re[k] + im[k] * im[k]).sum();
        let amp = power.sqrt() * scale;
        out[i] = 20.0 * amp.max(1e-5).log10() + scratch.tilts[i];
    }
    out
}

/// The slow auto-gain ceiling: rises INSTANTLY to the loudest recent moment, decays gently, never
/// drops below [`AGC_MIN_CEIL_DB`] (so near-silence isn't amplified into a full board).
struct Agc {
    ceil_db: f32,
}

impl Agc {
    fn new() -> Agc {
        Agc {
            ceil_db: AGC_MIN_CEIL_DB,
        }
    }
    fn step(&mut self, loud_db: f32, dt: f32) -> f32 {
        self.ceil_db = (self.ceil_db - AGC_DECAY_DB_PER_S * dt.max(0.0)).max(AGC_MIN_CEIL_DB);
        if loud_db > self.ceil_db {
            self.ceil_db = loud_db;
        }
        self.ceil_db
    }
}

/// One region's loudness channel: the ~[`INTEG_TAU_S`] energy integrator (the "VU needle") plus
/// its own peak-riding [`Agc`] ceiling — so a bass focus frames itself against the BASS's recent
/// peaks, independent of the rest of the mix.
struct Chan {
    energy: f32,
    agc: Agc,
}

impl Chan {
    fn new() -> Chan {
        Chan {
            energy: 0.0,
            agc: Agc::new(),
        }
    }

    /// One measurement step: integrate the region's instantaneous energy, ride the ceiling on the
    /// instantaneous peaks, answer the normalized 0..=1 level. `inst_energy == 0` (gated silence)
    /// drains the integrator smoothly to dark.
    fn step(&mut self, inst_energy: f32, dt: f32) -> f32 {
        let k = 1.0 - (-dt / INTEG_TAU_S).exp();
        self.energy += (inst_energy - self.energy) * k;
        let ceil = self.agc.step(
            if inst_energy > 0.0 { 10.0 * inst_energy.log10() } else { f32::MIN },
            dt,
        );
        if self.energy < 1e-11 {
            // Flush-to-zero floor: without resetting the FIELD (not just the returned
            // level), a silence tail keeps multiplying `energy` toward 0 forever, spending
            // many ticks with it parked in f32's SUBNORMAL range (below ~1.18e-38) even
            // though the published level already reports honest dark. Denormal arithmetic
            // is 10-100x slower on x86 — an unbounded decay is a real CPU-cost bug on any
            // sustained-silence tail. 1e-11 sits far above the subnormal boundary, so this
            // floor is hit long before `energy` could ever become subnormal.
            self.energy = 0.0;
            return 0.0; // fully drained — honestly dark
        }
        let smooth_db = 10.0 * self.energy.log10();
        ((smooth_db - (ceil - RANGE_DB)) / RANGE_DB).clamp(0.0, 1.0)
    }
}

/// The full measurement state: one [`Chan`] per region + the smoothed TONE. One per analyser thread.
struct Loudness {
    chans: [Chan; REGIONS],
    tone: f32,
    /// Reused FFT work buffers for [`tilted_band_dbs`] — see [`ScratchBufs`].
    scratch: ScratchBufs,
}

impl Loudness {
    fn new() -> Loudness {
        Loudness {
            chans: [Chan::new(), Chan::new(), Chan::new(), Chan::new()],
            tone: 0.0,
            scratch: ScratchBufs::new(),
        }
    }
}

/// One full analysis pass → a [`Signal`] (per-region levels + tone).
///
/// LEVELS: the tilted band energies are summed per REGION (mix / bass / mids / highs) and each
/// region runs its own [`Chan`]: instantaneous energy → ~[`INTEG_TAU_S`] integration (the VU
/// ballistic) → normalized against that region's own AGC window `[ceil - RANGE_DB, ceil]`. Two
/// deliberate asymmetries make it musical:
///  * each CEILING rides its region's instantaneous peaks while the PUBLISHED level is the
///    integrated loudness — the music's crest factor becomes natural headroom, so a steady passage
///    sits mid-range and only the beat's crest touches 1.0 (a plain AGC would pin any sustained
///    volume at the top);
///  * true silence GATES every integrator's input (they drain smoothly to dark over ~½s instead of
///    cutting), and no AGC ever learns from converter noise.
///
/// TONE: the log-frequency CENTROID of the same tilted energies (0 = the kick's register, 1 = the
/// cymbals'), stretched through the [`TONE_LO`]..[`TONE_HI`] window and eased over
/// ~[`TONE_TAU_S`] — the "what does it sound like" axis. Held (not drained) through silence: with
/// the level at 0 the colour is invisible anyway, and holding avoids a re-entry jump.
fn analyze(ring: &[f32], rate: u32, dt: f32, st: &mut Loudness) -> Signal {
    let dbs = tilted_band_dbs(ring, rate, &mut st.scratch);
    let max_db = dbs.iter().copied().fold(f32::MIN, f32::max);
    let live_signal = max_db >= GATE_DB;
    let dt = dt.max(0.0);

    let mut region_energy = [0f32; REGIONS];
    if live_signal {
        // tilted energies power everything: the region sums (loudness) and the centroid (tone).
        let mut weighted = 0.0f32;
        for (i, d) in dbs.iter().enumerate() {
            let e = 10f32.powf(d / 10.0);
            weighted += e * i as f32 / (BANDS - 1) as f32;
            for (r, &(lo, hi)) in REGION_BANDS.iter().enumerate() {
                if i >= lo && i < hi {
                    region_energy[r] += e;
                }
            }
        }
        let centroid = weighted / region_energy[0].max(1e-12); // region 0 = the full mix
        let stretched = ((centroid - TONE_LO) / (TONE_HI - TONE_LO)).clamp(0.0, 1.0);
        let kt = 1.0 - (-dt / TONE_TAU_S).exp();
        st.tone += (stretched - st.tone) * kt;
    }

    let mut levels = [0f32; REGIONS];
    for (r, chan) in st.chans.iter_mut().enumerate() {
        levels[r] = chan.step(region_energy[r], dt);
    }
    Signal {
        levels,
        tone: st.tone.clamp(0.0, 1.0),
    }
}

// ───────────────────────────────────── the platform seam ─────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use crate::audio::{self, CaptureCtl};

    /// The Windows PCM sampler — owns the live WASAPI capture stream and self-heals: a failed read
    /// drops the handle so the next tick re-opens the now-current endpoint (device flips, sleeps).
    pub struct Sampler {
        source: String,
        ctl: Option<CaptureCtl>,
    }

    impl Sampler {
        pub fn open(source: &str) -> Self {
            let mut s = Sampler {
                source: source.to_string(),
                ctl: None,
            };
            s.ctl = s.open_ctl();
            s
        }

        fn open_ctl(&self) -> Option<CaptureCtl> {
            if self.source.eq_ignore_ascii_case("mic") {
                audio::resolve_capture(None).and_then(|ep| CaptureCtl::open_capture(&ep.id))
            } else {
                CaptureCtl::open_loopback_default()
            }
        }

        /// THE SEAM: append this tick's mono PCM onto `out` and return the stream's sample rate, or
        /// `None` when no stream is open (it keeps retrying each tick). A dead endpoint drops the
        /// handle so the next call re-opens.
        pub fn read(&mut self, out: &mut Vec<f32>) -> Option<u32> {
            if self.ctl.is_none() {
                self.ctl = self.open_ctl();
            }
            let ctl = self.ctl.as_ref()?;
            if ctl.read_into(out).is_some() { Some(ctl.rate()) } else {
                self.ctl = None;
                None
            }
        }
    }
}

/// The inert off-Windows sampler — no PCM backend, so [`level`] reports not-live and the meter
/// falls back to the (equally inert) peak provider. ALWAYS compiled so its surface is type-checked
/// on every build, mirroring `audio_level`'s stub discipline.
mod stub {
    #![allow(dead_code)]

    pub struct Sampler;

    impl Sampler {
        pub fn open(_source: &str) -> Self {
            Sampler
        }

        /// THE SEAM (port here). Returns `None` → the analyser reports not-live.
        // TODO(macos): CoreAudio — an output tap (AudioHardwareCreateProcessTap / an aggregate
        //   device's IOProc) for "speakers", an AudioQueue input for "mic"; append mono f32.
        // TODO(linux): PipeWire — a capture stream on the default sink's monitor ("speakers") or
        //   the default source ("mic"); append mono f32.
        pub fn read(&mut self, _out: &mut Vec<f32>) -> Option<u32> {
            None
        }
    }
}

// ─────────────────────────────────────────── tests ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `n` samples of a pure sine at `hz` (amplitude `amp`) sampled at `rate`.
    fn sine(hz: f32, amp: f32, rate: u32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / rate as f32).sin())
            .collect()
    }

    fn reference_fft(re: &mut [f32], im: &mut [f32]) {
        let n = re.len();
        let mut j = 0usize;
        for i in 0..n {
            if i < j { re.swap(i, j); im.swap(i, j); }
            let mut m = n >> 1;
            while m >= 1 && j & m != 0 { j ^= m; m >>= 1; }
            j |= m;
        }
        let mut len = 2;
        while len <= n {
            let ang = -std::f32::consts::TAU / len as f32;
            let (wr, wi) = (ang.cos(), ang.sin());
            let mut i = 0;
            while i < n {
                let (mut cr, mut ci) = (1.0f32, 0.0f32);
                for k in 0..len / 2 {
                    let (a, b) = (i + k, i + k + len / 2);
                    let (tr, ti) = (re[b] * cr - im[b] * ci, re[b] * ci + im[b] * cr);
                    re[b] = re[a] - tr; im[b] = im[a] - ti;
                    re[a] += tr; im[a] += ti;
                    let ncr = cr * wr - ci * wi;
                    ci = cr * wi + ci * wr;
                    cr = ncr;
                }
                i += len;
            }
            len <<= 1;
        }
    }

    fn reference_tilted_band_dbs(ring: &[f32], rate: u32) -> [f32; BANDS] {
        reference_tilted_band_dbs_with_scratch(ring, rate, &mut ScratchBufs::new())
    }

    fn reference_tilted_band_dbs_with_scratch(ring: &[f32], rate: u32, scratch: &mut ScratchBufs) -> [f32; BANDS] {
        let re = &mut scratch.re;
        let im = &mut scratch.im;
        re.fill(0.0);
        im.fill(0.0);
        let n = ring.len().min(FFT_N);
        let pad = FFT_N - n;
        for (k, &s) in ring[ring.len() - n..].iter().enumerate() {
            let i = pad + k;
            let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / FFT_N as f32).cos();
            re[i] = s * w;
        }
        reference_fft(re, im);
        let ranges = band_bin_ranges(rate);
        let scale = 4.0 / FFT_N as f32;
        let mut out = [0.0; BANDS];
        for (i, &(lo, hi)) in ranges.iter().enumerate() {
            let power: f32 = (lo..hi).map(|k| re[k] * re[k] + im[k] * im[k]).sum();
            let amp = power.sqrt() * scale;
            out[i] = 20.0 * amp.max(1e-5).log10() + tilt_db(i);
        }
        out
    }

    fn assert_same_bits(a: &[f32; BANDS], b: &[f32; BANDS]) {
        for (i, (left, right)) in a.iter().zip(b).enumerate() {
            assert_eq!(left.to_bits(), right.to_bits(), "band {i}: {left} != {right}");
        }
    }

    fn seeded_noise(seed: &mut u32, n: usize) -> Vec<f32> {
        (0..n).map(|_| {
            *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((*seed >> 8) as f32 / 16_777_216.0) * 2.0 - 1.0
        }).collect()
    }

    #[test]
    fn cached_spectrum_plan_matches_uncached_reference_bit_for_bit() {
        let mut scratch = ScratchBufs::new();
        let mut seed = 0x6e65_7572;
        for rate in [44_100, 48_000, 96_000, 192_000] {
            for ring in [vec![0.0; FFT_N], sine(997.0, 0.73, rate, FFT_N), seeded_noise(&mut seed, FFT_N), seeded_noise(&mut seed, 731)] {
                assert_same_bits(&reference_tilted_band_dbs(&ring, rate), &tilted_band_dbs(&ring, rate, &mut scratch));
            }
        }
    }

    #[test]
    #[ignore = "deterministic audio spectrum microbenchmark; run manually with --ignored --nocapture"]
    fn bench_spectrum_silence_sine_and_seeded_noise() {
        let mut seed = 0x6e65_7572;
        for rate in [44_100, 48_000, 96_000] {
            let cases = [
                ("silence", vec![0.0; FFT_N]),
                ("sine", sine(997.0, 0.73, rate, FFT_N)),
                ("seeded-noise", seeded_noise(&mut seed, FFT_N)),
            ];
            for (name, ring) in &cases {
                let expected = reference_tilted_band_dbs(ring, rate);
                let mut scratch = ScratchBufs::new();
                let mut reference_scratch = ScratchBufs::new();
                assert_same_bits(&expected, &tilted_band_dbs(ring, rate, &mut scratch));
                let mut times = Vec::with_capacity(128);
                let mut reference_times = Vec::with_capacity(128);
                for _ in 0..128 {
                    let start = Instant::now();
                    let baseline = reference_tilted_band_dbs_with_scratch(ring, rate, &mut reference_scratch);
                    reference_times.push(start.elapsed().as_nanos());
                    assert_same_bits(&expected, &baseline);
                    let start = Instant::now();
                    let actual = tilted_band_dbs(ring, rate, &mut scratch);
                    times.push(start.elapsed().as_nanos());
                    assert_same_bits(&expected, &actual);
                }
                times.sort_unstable();
                reference_times.sort_unstable();
                let avg = times.iter().sum::<u128>() / times.len() as u128;
                let reference_avg = reference_times.iter().sum::<u128>() / reference_times.len() as u128;
                println!("spectrum {name} rate={rate}Hz baseline_avg={reference_avg}ns baseline_p50={}ns avg={avg}ns p50={}ns p95={}ns checksum={:.6}", reference_times[64], times[64], times[121], expected.iter().sum::<f32>());
            }
        }
    }

    #[test]
    fn fft_concentrates_a_pure_sine_at_its_bin() {
        const N: usize = 256;
        let bin = 19usize;
        let mut re: Vec<f32> = (0..N)
            .map(|i| (std::f32::consts::TAU * bin as f32 * i as f32 / N as f32).sin())
            .collect();
        let mut im = vec![0.0f32; N];
        fft(&mut re, &mut im);
        let power: Vec<f32> = (0..N / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect();
        let peak = power
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(peak, bin, "the sine's energy lands in its own bin");
        // and it dominates: the peak bin holds (nearly) all the signal power
        let total: f32 = power.iter().sum();
        assert!(power[bin] / total > 0.95, "a rectangular full-period sine is one clean line");
    }

    #[test]
    fn band_ranges_are_monotonic_and_well_formed() {
        for rate in [44_100u32, 48_000, 96_000] {
            let r = band_bin_ranges(rate);
            for (i, &(lo, hi)) in r.iter().enumerate() {
                assert!(lo >= 1, "band {i} excludes DC at {rate}Hz");
                assert!(hi > lo, "band {i} is non-empty at {rate}Hz");
                assert!(hi <= FFT_N / 2, "band {i} stays under Nyquist at {rate}Hz");
                if i > 0 {
                    assert!(lo >= r[i - 1].0, "band starts are monotonic at {rate}Hz");
                }
            }
        }
    }

    #[test]
    fn weighting_localises_bass_and_treble() {
        // the weighting stage must still be a real spectrum: 100Hz lands in a LOW band, 8kHz in a
        // HIGH one — that's what makes the loudness spectral, not just an RMS.
        let rate = 48_000u32;
        let mut scratch = ScratchBufs::new();
        let low = tilted_band_dbs(&sine(100.0, 0.8, rate, FFT_N), rate, &mut scratch);
        let low_peak = low.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!(low_peak < BANDS / 3, "100Hz reads as bass (band {low_peak})");

        let high = tilted_band_dbs(&sine(8_000.0, 0.8, rate, FFT_N), rate, &mut scratch);
        let high_peak = high.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert!(high_peak > BANDS * 2 / 3, "8kHz reads as treble (band {high_peak})");
    }

    #[test]
    fn analyze_gates_silence_to_dark() {
        let mut st = Loudness::new();
        let lvl = analyze(&vec![0.0; FFT_N], 48_000, 0.016, &mut st).levels[0];
        assert_eq!(lvl, 0.0, "digital silence is dark");
        // …even a whisper of dither stays gated (below GATE_DB)
        let mut st = Loudness::new();
        let lvl = analyze(&sine(1_000.0, 1e-5, 48_000, FFT_N), 48_000, 0.016, &mut st).levels[0];
        assert_eq!(lvl, 0.0, "converter-noise-level input stays dark");
    }

    #[test]
    fn analyze_converges_to_full_on_a_sustained_tone() {
        // a steady tone has no crest factor (instantaneous == integrated), so once the VU
        // integrator settles the level converges to the top of the AGC window.
        let mut st = Loudness::new();
        let loud = sine(1_000.0, 0.8, 48_000, FFT_N);
        let mut lvl = 0.0;
        for _ in 0..120 {
            lvl = analyze(&loud, 48_000, 0.016, &mut st).levels[0];
        }
        assert!(lvl > 0.95, "a sustained tone converges to the meter's top ({lvl})");
    }

    #[test]
    fn analyze_leaves_crest_headroom_for_transients() {
        // alternate a loud "beat" window with a quieter body: the ceiling learns the beat's
        // instantaneous peak, so the QUIET body reads well below the top — the crest factor is
        // the travel a real mix breathes through (a plain AGC would pin both at ~1.0).
        let rate = 48_000u32;
        let beat = sine(1_000.0, 0.8, rate, FFT_N);
        let body = sine(1_000.0, 0.2, rate, FFT_N); // −12dB below the beat
        let mut st = Loudness::new();
        let mut lvl_beat = 0.0f32;
        let mut lvl_body = 0.0f32;
        for _ in 0..8 {
            for _ in 0..6 {
                lvl_beat = analyze(&beat, rate, 0.016, &mut st).levels[0];
            }
            for _ in 0..24 {
                lvl_body = analyze(&body, rate, 0.016, &mut st).levels[0];
            }
        }
        assert!(
            lvl_body < lvl_beat - 0.2,
            "the quiet body sits well below the beat ({lvl_body} vs {lvl_beat})"
        );
        assert!(lvl_body > 0.1, "…but stays visibly on the gradient ({lvl_body})");
    }

    #[test]
    fn analyze_drains_smoothly_into_silence() {
        // loud → silence: the integrator drains (no hard cut) and lands at honest zero.
        let mut st = Loudness::new();
        let loud = sine(1_000.0, 0.8, 48_000, FFT_N);
        for _ in 0..60 {
            analyze(&loud, 48_000, 0.016, &mut st);
        }
        let silence = vec![0.0f32; FFT_N];
        let mut prev = analyze(&silence, 48_000, 0.016, &mut st).levels[0];
        assert!(prev > 0.0, "just after the cut the level is still draining");
        let mut lvl = prev;
        for _ in 0..180 {
            lvl = analyze(&silence, 48_000, 0.016, &mut st).levels[0];
            assert!(lvl <= prev + 1e-4, "the drain is monotone ({lvl} after {prev})");
            prev = lvl;
        }
        assert_eq!(lvl, 0.0, "…and lands at honest dark within a couple of seconds");
    }

    #[test]
    fn tone_tracks_the_musics_register_and_holds_through_silence() {
        let rate = 48_000u32;
        // a sustained 80Hz tone drags the eased centroid into the bass end…
        let mut st = Loudness::new();
        let mut tone = 0.0f32;
        for _ in 0..120 {
            tone = analyze(&sine(80.0, 0.5, rate, FFT_N), rate, 0.016, &mut st).tone;
        }
        assert!(tone < 0.15, "an 80Hz tone reads as deep bass ({tone})");
        // …a 10kHz tone lifts it to the treble end…
        let mut st = Loudness::new();
        for _ in 0..120 {
            tone = analyze(&sine(10_000.0, 0.5, rate, FFT_N), rate, 0.016, &mut st).tone;
        }
        assert!(tone > 0.85, "a 10kHz tone reads as treble ({tone})");
        let treble_tone = tone;
        // …and through silence the tone HOLDS (the level is dark anyway; no re-entry jump).
        for _ in 0..60 {
            tone = analyze(&vec![0.0; FFT_N], rate, 0.016, &mut st).tone;
        }
        assert_eq!(tone, treble_tone, "silence leaves the tone untouched");
        // a mid-register tone lands between the extremes.
        let mut st = Loudness::new();
        for _ in 0..120 {
            tone = analyze(&sine(1_000.0, 0.5, rate, FFT_N), rate, 0.016, &mut st).tone;
        }
        assert!((0.2..=0.8).contains(&tone), "1kHz sits in the gradient's middle ({tone})");
    }

    #[test]
    fn regions_separate_bass_from_highs() {
        let rate = 48_000u32;
        // a pure 100Hz tone: the BASS channel converges to full while the HIGHS channel stays
        // dark (its region holds nothing but the noise floor) — the separation `focus` buys.
        let mut st = Loudness::new();
        let mut sig = Signal::default();
        for _ in 0..120 {
            sig = analyze(&sine(100.0, 0.5, rate, FFT_N), rate, 0.016, &mut st);
        }
        assert!(sig.levels[1] > 0.9, "the bass channel hears the 100Hz tone ({})", sig.levels[1]);
        assert_eq!(sig.levels[3], 0.0, "the highs channel stays dark on a bass-only signal");
        // …and a 8kHz tone flips it.
        let mut st = Loudness::new();
        for _ in 0..120 {
            sig = analyze(&sine(8_000.0, 0.5, rate, FFT_N), rate, 0.016, &mut st);
        }
        assert!(sig.levels[3] > 0.9, "the highs channel hears the 8kHz tone ({})", sig.levels[3]);
        assert_eq!(sig.levels[1], 0.0, "the bass channel stays dark on a treble-only signal");
        // the mix channel hears both cases.
        assert!(sig.levels[0] > 0.9, "the mix channel always hears the signal");
    }

    #[test]
    fn agc_adapts_so_quiet_music_still_fills_the_board() {
        let rate = 48_000u32;
        let quiet = sine(1_000.0, 0.02, rate, FFT_N); // ~-34dB — late-night listening
        let mut st = Loudness::new();
        let first = analyze(&quiet, rate, 0.016, &mut st).levels[0];
        // hold the quiet signal for a simulated ~20s of ticks: the ceiling decays down to it
        let mut last = first;
        for _ in 0..1300 {
            last = analyze(&quiet, rate, 0.016, &mut st).levels[0];
        }
        assert!(
            last >= first && last > 0.9,
            "the AGC re-frames a sustained quiet signal toward full scale ({first} → {last})"
        );
    }

    #[test]
    fn agc_ceiling_rises_instantly_and_never_underflows() {
        let mut agc = Agc::new();
        let c = agc.step(-3.0, 0.016);
        assert!((c - -3.0).abs() < 1e-6, "a loud moment lifts the ceiling at once");
        let mut agc = Agc::new();
        for _ in 0..100_000 {
            agc.step(f32::MIN, 0.016);
        }
        assert!(agc.ceil_db >= AGC_MIN_CEIL_DB, "the ceiling never decays below its floor");
    }

    #[test]
    fn decimation_averages_groups_and_carries_the_remainder() {
        let mut carry = Vec::new();
        let mut out = Vec::new();
        // 7 samples by factor 2 → 3 groups + 1 carried
        decimate_into(&mut carry, &[2.0, 4.0, 6.0, 8.0, 1.0, 3.0, 5.0], 2, &mut out);
        assert_eq!(out, vec![3.0, 7.0, 2.0]);
        assert_eq!(carry, vec![5.0]);
        // the carried sample joins the next tick's first group
        decimate_into(&mut carry, &[7.0], 2, &mut out);
        assert_eq!(out, vec![3.0, 7.0, 2.0, 6.0]);
        assert!(carry.is_empty());
        // factor 1 is a plain append (no carry involvement)
        decimate_into(&mut carry, &[9.0, 9.0], 1, &mut out);
        assert_eq!(&out[4..], &[9.0, 9.0]);
    }

    /// MANUAL live probe — needs a real Windows session with audio PLAYING:
    /// `cargo test -p neuron --lib audio_spectrum -- --ignored --nocapture`
    /// Verifies the whole chain end-to-end: WASAPI loopback opens, packets flow, loudness publishes.
    #[test]
    #[ignore = "live WASAPI loopback probe — run manually with audio playing"]
    fn live_loopback_probe() {
        let mut readings = Vec::new();
        for _ in 0..60 {
            ensure("speakers");
            std::thread::sleep(std::time::Duration::from_millis(50));
            if let Some(s) = signal() {
                readings.push(s);
            }
        }
        println!("live signal readings: {readings:?}");
        assert!(!readings.is_empty(), "the loopback capture should open on a real Windows session");
        assert!(
            readings.iter().any(|s| s.levels[0] > 0.05),
            "with audio playing, the loudness should move"
        );
    }

    #[test]
    fn tilt_lifts_treble_and_tames_sub_bass() {
        assert!(tilt_db(BANDS - 1) > 0.0, "the top band gets a positive tilt");
        assert!(tilt_db(0) < 0.0, "the bottom band sits below the tilt reference");
        for i in 1..BANDS {
            assert!(tilt_db(i) > tilt_db(i - 1), "tilt is monotonic in frequency");
        }
    }

    // ── Property tests: metamorphic centroid laws, the denormal floor, and hostile-PCM
    // robustness. ────────────────────────────────────────────────────────────────────
    mod props {
        use super::*;
        use proptest::prelude::*;

        fn cfg() -> ProptestConfig {
            ProptestConfig { cases: 128, ..ProptestConfig::default() }
        }

        /// The same band-index-weighted centroid `analyze` computes internally (region 0 =
        /// the full mix, weighted by tilted band energy) — pulled out here as a pure helper
        /// so the metamorphic properties can probe it directly without the AGC/integrator
        /// state `analyze`/`Loudness` carry across ticks.
        fn band_centroid(dbs: &[f32; BANDS]) -> f32 {
            let mut weighted = 0.0f32;
            let mut total = 0.0f32;
            for (i, d) in dbs.iter().enumerate() {
                let e = 10f32.powf(d / 10.0);
                weighted += e * i as f32 / (BANDS - 1) as f32;
                total += e;
            }
            weighted / total.max(1e-12)
        }

        proptest! {
            #![proptest_config(cfg())]

            /// (3a) A time shift must not change WHAT frequency the music is at — only WHEN.
            /// For a bin-exact pure tone (frequency an exact multiple of `rate/FFT_N`), a
            /// circular shift of the sampled buffer is a pure phase shift of the same
            /// underlying periodic signal. For a COMPLEX exponential the Hann-windowed
            /// magnitude spectrum would be exactly phase-invariant — but a REAL sine is two
            /// complex exponentials (±bin), and the window's sidelobe skirts of the two
            /// images overlap and interfere PHASE-DEPENDENTLY at a small level (the property
            /// run that pinned this found ~1.1e-3 of centroid drift at bin 684, shift 26).
            /// So the law holds to a small tolerance, not float-epsilon: 5e-3 on a centroid
            /// that lives in [0,1] still pins "the music didn't move bands", while leaving
            /// room for the real-tone image interference that is genuinely there.
            /// Bin domain: the analyzed band range tops out at [`F_HI`] (16kHz = bin ~683 at
            /// 48kHz/2048), and a tone AT the edge has its leakage skirt half-outside the banded
            /// range — phase then genuinely moves the in-band energy (the pinning runs measured
            /// 2%+ of centroid drift at bins 684-701). The law is about tones the analyzer
            /// actually covers, so the domain stops comfortably inside the edge (bin 660 ≈
            /// 15.5kHz), mirroring how the pitch-ranking law below already excludes the edges.
            #[test]
            fn centroid_is_time_shift_invariant(
                bin in 5usize..660,
                shift in 1usize..FFT_N,
            ) {
                let rate = 48_000u32;
                let hz = bin as f32 * rate as f32 / FFT_N as f32;
                let buf = sine(hz, 0.8, rate, FFT_N);
                let mut shifted = buf.clone();
                shifted.rotate_left(shift % FFT_N);

                let mut scratch = ScratchBufs::new();
                let c1 = band_centroid(&tilted_band_dbs(&buf, rate, &mut scratch));
                let c2 = band_centroid(&tilted_band_dbs(&shifted, rate, &mut scratch));
                // Principled tolerance: the law is "the music didn't move bands", so the bound is
                // HALF A BAND WIDTH on the [0,1] band-index centroid — not a hand-tuned epsilon.
                // (The real-tone ±bin image interference described above peaks near the top band
                // edge; the pinning runs measured up to ~5e-3 of drift there, well inside this.)
                let half_band = 0.5 / (BANDS - 1) as f32;
                prop_assert!(
                    (c1 - c2).abs() < half_band,
                    "shift moved the centroid by more than half a band: {c1} vs {c2}"
                );
            }

            /// (3b) Ranking law: a higher pure tone reads as a higher (band-index) centroid
            /// than a lower one — "where the music's energy lives" must move the right way
            /// as pitch rises. Frequencies are kept solidly inside the analysed band
            /// [`F_LO`]..[`F_HI`] (a tone above `F_HI` falls outside every band's range and
            /// reads as pure noise-floor, a degenerate case unrelated to this law) and
            /// spaced far enough apart (`hz2 >= hz1 * 1.5`) that they can't land in the same
            /// band and tie.
            #[test]
            fn centroid_scales_with_pitch(
                hz1 in 150.0f32..5_000.0,
                mult in 1.5f32..3.0,
            ) {
                let rate = 48_000u32;
                let hz2 = (hz1 * mult).min(15_000.0);
                prop_assume!(hz2 >= hz1 * 1.5);
                let mut scratch = ScratchBufs::new();
                let c1 = band_centroid(&tilted_band_dbs(&sine(hz1, 0.8, rate, FFT_N), rate, &mut scratch));
                let c2 = band_centroid(&tilted_band_dbs(&sine(hz2, 0.8, rate, FFT_N), rate, &mut scratch));
                prop_assert!(c2 > c1, "higher tone ({hz2}Hz, centroid {c2}) didn't rank above the lower one ({hz1}Hz, centroid {c1})");
            }

            /// (3c) Sustained silence must decay `Chan::energy` to TRUE zero, never leave it
            /// parked in the f32 SUBNORMAL range — the denormal-floor bug fixed in `step`
            /// above (see the comment there). Without that fix this property fails: the
            /// unfloored field drifts through denormal values for ~150 ticks before ever
            /// reaching exact 0.0.
            /// Tick budget: the decay measured in the pinning run is ~0.899x per 16ms tick, so
            /// crossing the 1e-11 flush floor from a full-scale 1.0 start takes ~237 ticks —
            /// 400 is a comfortable worst-case margin. (The per-tick invariant inside the loop
            /// is the load-bearing half of the law: energy must NEVER be observable between 0
            /// and the subnormal boundary, at any tick count.)
            #[test]
            fn silence_decays_to_true_zero_not_denormals(
                start_energy in 1e-6f32..1.0,
                ticks in 400usize..800,
            ) {
                let mut chan = Chan::new();
                chan.energy = start_energy;
                for _ in 0..ticks {
                    chan.step(0.0, 0.016);
                    prop_assert!(
                        chan.energy == 0.0 || chan.energy >= f32::MIN_POSITIVE,
                        "energy parked in the denormal range: {}", chan.energy
                    );
                }
                prop_assert_eq!(chan.energy, 0.0, "a sustained silence tail must fully flush to true zero");
            }

            /// (3d) Hostile PCM — NaN, ±Inf, full-scale, DC offset, alternating ±1 — must
            /// never panic the analysis kernels. A short hostile pattern is cycled to fill a
            /// full FFT window (cheaper to generate than a full 2048-element strategy while
            /// still exercising every position via the cycling FFT/window math). Every
            /// published field stays either NaN (hostile input legitimately can't produce a
            /// meaningful reading — no floor exists to invent one) or within its documented
            /// bound; NEITHER kernel panics either way.
            #[test]
            fn dsp_kernels_never_panic_on_hostile_pcm(
                pattern in prop::collection::vec(
                    prop_oneof![
                        Just(f32::NAN),
                        Just(f32::INFINITY),
                        Just(f32::NEG_INFINITY),
                        Just(1.0f32),
                        Just(-1.0f32),
                        Just(0.0f32),
                        Just(1e30f32),
                        Just(-1e30f32),
                        proptest::num::f32::ANY,
                    ],
                    1..32,
                ),
                rate in prop_oneof![Just(44_100u32), Just(48_000u32), Just(96_000u32)],
                dt in 0.0f32..0.1,
            ) {
                let ring: Vec<f32> = pattern.iter().cycle().take(FFT_N).copied().collect();

                let mut scratch = ScratchBufs::new();
                let dbs = tilted_band_dbs(&ring, rate, &mut scratch); // must not panic
                prop_assert_eq!(dbs.len(), BANDS);

                let mut st = Loudness::new();
                let sig = analyze(&ring, rate, dt, &mut st); // must not panic
                for lvl in sig.levels {
                    prop_assert!(lvl.is_nan() || (0.0..=1.0).contains(&lvl), "level out of bounds: {lvl}");
                }
                prop_assert!(sig.tone.is_nan() || (0.0..=1.0).contains(&sig.tone), "tone out of bounds: {}", sig.tone);
            }
        }
    }
}
