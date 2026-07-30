// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! A shared, fast, live system-telemetry provider — the single source of truth the `pulse`
//! lighting effect (and its on-screen preview) both read for CPU + RAM load.
//!
//! ## Why a provider, not a per-frame read
//! This mirrors [`crate::audio_level`]: a lighting effect that reacts to a LIVE signal must not
//! sample that signal on the render thread. A legacy keyboard streams at ~6fps, so a per-frame read
//! would alias badly — and CPU load in particular can only be derived from the DELTA between two
//! samples, which a bursty frame loop can't space evenly. So ONE background thread samples at a
//! steady ~1Hz, derives the load from the inter-sample delta HERE, smooths it, and publishes the
//! `0.0..=1.0` values to lock-free atomics. Every consumer — the device stream, the big render
//! preview, the effect-tile thumbnail — calls [`ensure`] then reads [`cpu`] / [`ram`], so they all
//! see the SAME number and stay in lock-step, and the board reacts smoothly even at 6fps because the
//! sampling cadence is independent of the frame rate.
//!
//! ## What it measures
//! * **CPU load** — `GetSystemTimes` (idle/kernel/user tick counters). A single reading is
//!   meaningless; the load is `1 - idle_delta/total_delta` over the gap between two readings (kernel
//!   time already includes idle, so `total = kernel + user`). The first tick only baselines; load
//!   appears from the second tick on.
//! * **RAM load** — `GlobalMemoryStatusEx().dwMemoryLoad`, already an OS-computed 0..100% of physical
//!   memory in use, normalised to 0..1.
//!
//! ## Lifecycle
//! [`ensure`] starts the thread; it's idempotent — a call while it's already running is a no-op, so
//! it's cheap to call every frame. The thread auto-stops if nobody has called [`cpu`]/[`ram`] in a
//! few seconds, so it never runs longer than the page that wants it; the next [`ensure`] restarts it.
//!
//! ## Platform seam — where a macOS/Linux port plugs in
//! Everything in this file is platform-NEUTRAL — the thread, the atomics, the idle-stop, the
//! CPU-delta + smoothing + normalise math — EXCEPT one tiny seam: [`sample`], "read the raw counters
//! this tick" → `((idle, kernel, user) ticks, mem-load %)`. The neutral [`run`] loop owns the delta
//! and smoothing and just calls `sample()`. Adding a platform is therefore implementing ONE function:
//! the Windows one ([`imp::sample`], `GetSystemTimes` + `GlobalMemoryStatusEx`) is selected on
//! Windows; off-Windows the inert [`stub::sample`] returns `(None, None)` so the board idles honestly
//! dark. (`mod stub` is ALWAYS compiled — never cfg-gated — so its surface is type-checked on every
//! build, the same drift guard `audio.rs` uses; the porting TODOs sit on its `sample`.)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

// The platform SAMPLE seam: `sample() -> (Option<(u64,u64,u64)>, Option<u32>)`. Picked by cfg — the
// only platform-specific line in the provider. Both modules define `sample`.
#[cfg(windows)]
use imp::sample;
#[cfg(not(windows))]
use stub::sample;

// ── the normalise + smoothing + delta MATH: platform-neutral, the neutral `run` loop calls these
// every ~1s tick. Kept at the module top level (not inside the platform sampler) so it's
// unit-testable on every target — the Win32 reads aren't, but this is.

/// The CPU envelope coefficient for the ~1Hz tick: each sample moves the smoothed value halfway
/// toward the new raw load — responsive without strobing on a single busy spike.
const CPU_SMOOTH: f32 = 0.5;

/// Derive CPU load (0.0..=1.0) from one inter-sample delta of `GetSystemTimes`' counters. Kernel time
/// already INCLUDES idle, so the wall time elapsed is `kernel + user` and the busy fraction is
/// `(total - idle) / total`. Saturating + clamped so a counter wrap or a zero-length gap can never
/// produce a NaN or escape range.
fn cpu_load_from_deltas(idle_delta: u64, kernel_delta: u64, user_delta: u64) -> f32 {
    let total = kernel_delta.saturating_add(user_delta);
    if total == 0 {
        return 0.0;
    }
    let busy = total.saturating_sub(idle_delta);
    (busy as f32 / total as f32).clamp(0.0, 1.0)
}

/// One smoothing step: move `prev` toward `raw` by factor `k` (an exponential moving average),
/// clamped to 0..=1. `k=1` snaps to `raw`, `k=0` holds. Frame-rate-independent because the sampler
/// ticks it at a fixed cadence regardless of how often the getters are read.
fn smooth_step(prev: f32, raw: f32, k: f32) -> f32 {
    (prev + (raw - prev) * k).clamp(0.0, 1.0)
}

/// Normalise an OS memory-load percentage (0..100, e.g. `MEMORYSTATUSEX::dwMemoryLoad`) to 0..=1,
/// clamped defensively.
fn mem_load_norm(pct: u32) -> f32 {
    (pct as f32 / 100.0).clamp(0.0, 1.0)
}

/// Recombine a `FILETIME`'s split 32-bit halves into the single 64-bit 100ns tick count. Used only by
/// the Windows sampler, but kept here (neutral) so it stays unit-testable; `cfg_attr` silences the
/// dead-code warning on a non-Windows build, where only the inert `stub` reads the counters.
#[cfg_attr(not(windows), allow(dead_code))]
fn filetime_to_u64(low: u32, high: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// The published smoothed CPU load (0.0..=1.0) stored as `f32` bits — read lock-free in [`cpu`].
static CPU_BITS: AtomicU32 = AtomicU32::new(0);
/// The published RAM load (0.0..=1.0) stored as `f32` bits — read lock-free in [`ram`].
static RAM_BITS: AtomicU32 = AtomicU32::new(0);
/// Millis since the process epoch of the last getter read — drives the idle auto-stop.
static LAST_ACCESS_MS: AtomicU64 = AtomicU64::new(0);

/// A process-lifetime monotonic origin so the thread and the getters agree on "now" in millis.
fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// The sampler's control block — just whether a thread is alive (there's no source to repoint,
/// unlike the audio provider).
fn inner() -> &'static Mutex<bool> {
    static I: OnceLock<Mutex<bool>> = OnceLock::new();
    I.get_or_init(|| Mutex::new(false))
}

const SAMPLE_INTERVAL: Duration = Duration::from_millis(1000); // ~1Hz; CPU load needs a delta gap
const IDLE_STOP_MS: u64 = 4000; // stop the thread if no getter read in ~4s

/// Start the sampler if it isn't already running. Idempotent — a call while it's alive is a no-op
/// — so it's cheap to call every frame. Refreshes the idle timer so a brand-new thread doesn't
/// immediately auto-stop before the first getter read lands.
pub fn ensure() {
    let mut running = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    if *running {
        return;
    }
    *running = true;
    drop(running);
    // The latch is cleared by the release — which runs on completion, panic, OR a spawn refusal —
    // so a failed spawn can never leave the sampler latched "on" and block every later `ensure`.
    crate::worker::spawn_guarded(
        "neuron-sys-stats",
        || *inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false,
        run,
    );
}

/// The latest smoothed CPU load (0.0..=1.0). Lock-free and cheap. Reading it keeps the sampler
/// alive (resets the idle timer), so a consumer that stops reading lets the thread auto-stop.
pub fn cpu() -> f32 {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    f32::from_bits(CPU_BITS.load(Ordering::Relaxed))
}

/// The latest RAM load (0.0..=1.0). Lock-free and cheap; also keeps the sampler alive.
pub fn ram() -> f32 {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    f32::from_bits(RAM_BITS.load(Ordering::Relaxed))
}

/// The sampler loop — PLATFORM-NEUTRAL. Each ~1s read the raw counters through the platform seam,
/// derive CPU load from the delta against the previous reading (the first tick only baselines),
/// smooth it, and normalise RAM; publish both to the atomics. Exits (clearing the values) once no
/// getter has been read in ~4s. The ONLY platform-specific call here is `sample()`.
fn run() {
    let mut prev: Option<(u64, u64, u64)> = None;
    let mut cpu_smooth = 0.0f32;
    loop {
        // ── control: idle auto-stop ──
        {
            let running = inner().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle = now_ms().saturating_sub(LAST_ACCESS_MS.load(Ordering::Relaxed));
            if idle > IDLE_STOP_MS {
                // `running` is cleared by the spawn's release, not here — see `ensure`.
                drop(running);
                CPU_BITS.store(0f32.to_bits(), Ordering::Relaxed);
                RAM_BITS.store(0f32.to_bits(), Ordering::Relaxed);
                return;
            }
        }

        // ── sample (platform seam): raw CPU tick counters + raw RAM-load percent ──
        let (times, mem) = sample();

        // ── CPU: derive load from the inter-sample delta (kernel time includes idle) ──
        if let Some((idle, kernel, user)) = times {
            if let Some((pi, pk, pu)) = prev {
                let raw = cpu_load_from_deltas(
                    idle.saturating_sub(pi),
                    kernel.saturating_sub(pk),
                    user.saturating_sub(pu),
                );
                cpu_smooth = smooth_step(cpu_smooth, raw, CPU_SMOOTH);
                CPU_BITS.store(cpu_smooth.to_bits(), Ordering::Relaxed);
            }
            prev = Some((idle, kernel, user));
        }

        // ── RAM: a single snapshot is already a percentage; no delta needed ──
        if let Some(pct) = mem {
            RAM_BITS.store(mem_load_norm(pct).to_bits(), Ordering::Relaxed);
        }

        thread::sleep(SAMPLE_INTERVAL);
    }
}

#[cfg(windows)]
mod imp {
    /// THE SEAM: one platform read per tick — the raw CPU tick counters `(idle, kernel, user)` and the
    /// OS memory-load percentage (0..100). Each is `None` if that specific OS call failed (the neutral
    /// loop then skips that metric for the tick, never panics). All delta/smoothing math lives in the
    /// neutral `super::run` loop.
    pub fn sample() -> (Option<(u64, u64, u64)>, Option<u32>) {
        (read_system_times(), read_mem_load())
    }

    /// Read the global idle/kernel/user 100ns tick counters via `GetSystemTimes`, recombined to u64.
    /// `None` if the call fails (treated as "skip this tick", never a panic).
    fn read_system_times() -> Option<(u64, u64, u64)> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetSystemTimes;
        let mut idle = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut kernel = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut user = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        // SAFETY: three valid out-pointers to stack FILETIMEs; the call only writes through them.
        let ok = unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        Some((
            super::filetime_to_u64(idle.dwLowDateTime, idle.dwHighDateTime),
            super::filetime_to_u64(kernel.dwLowDateTime, kernel.dwHighDateTime),
            super::filetime_to_u64(user.dwLowDateTime, user.dwHighDateTime),
        ))
    }

    // `GlobalMemoryStatusEx` lives in windows-sys' `Win32_System_SystemInformation` feature, which is
    // NOT enabled in the frozen core manifest — so declare the kernel32 import directly (no new crate
    // dependency, no manifest edit). kernel32 is already in every Windows link line, so this resolves
    // alongside the windows-sys imports.
    #[repr(C)]
    struct MemoryStatusEx {
        dw_length: u32,
        dw_memory_load: u32,
        ull_total_phys: u64,
        ull_avail_phys: u64,
        ull_total_page_file: u64,
        ull_avail_page_file: u64,
        ull_total_virtual: u64,
        ull_avail_virtual: u64,
        ull_avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    /// Read the OS memory-load percentage (0..100, the fraction of physical RAM in use) via
    /// `GlobalMemoryStatusEx`. `None` if the call fails.
    fn read_mem_load() -> Option<u32> {
        // SAFETY: zeroed POD with dw_length set to its own size, as the API requires; the call only
        // writes within the struct we pass.
        let mut m: MemoryStatusEx = unsafe { std::mem::zeroed() };
        m.dw_length = std::mem::size_of::<MemoryStatusEx>() as u32;
        let ok = unsafe { GlobalMemoryStatusEx(&mut m) };
        if ok == 0 {
            return None;
        }
        Some(m.dw_memory_load)
    }
}

/// The inert off-Windows telemetry SAMPLER — no Win32 backend here, so there's nothing to read, and
/// the neutral `run` loop publishes a steady `0.0` (the `pulse` board idles honestly dark rather than
/// faking load). ALWAYS compiled (not cfg-gated) so its surface is type-checked on every build,
/// mirroring `audio.rs`'s stub discipline; `#![allow(dead_code)]` because on Windows the live seam is
/// `imp::sample` and this stands unused.
mod stub {
    #![allow(dead_code)]

    /// THE SEAM (port here). Returns `(None, None)` → the board reads dark.
    // TODO(linux): read `/proc/stat` (the `cpu` line's jiffies) for the (idle, kernel≈system+irq,
    //   user) counters, and `/proc/meminfo` (1 - MemAvailable/MemTotal) for the load percent — both
    //   pure file reads, no new crate deps.
    // TODO(macos): `host_statistics(HOST_CPU_LOAD_INFO)` for the CPU ticks and `host_statistics64`
    //   VM stats (or `sysctl` hw.memsize + vm_stat) for the memory load.
    pub fn sample() -> (Option<(u64, u64, u64)>, Option<u32>) {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_delta_busy_fraction() {
        // total = kernel + user (kernel already includes idle). load = (total - idle) / total.
        // idle 50 of a 100-tick window → 50% busy.
        assert!((cpu_load_from_deltas(50, 80, 20) - 0.5).abs() < 1e-6);
        // fully idle → 0.0; zero busy.
        assert_eq!(cpu_load_from_deltas(100, 80, 20), 0.0);
        // no idle at all → fully busy.
        assert_eq!(cpu_load_from_deltas(0, 80, 20), 1.0);
    }

    #[test]
    fn cpu_delta_is_defensive() {
        // a zero-length gap (no ticks elapsed) is 0, not a divide-by-zero NaN.
        assert_eq!(cpu_load_from_deltas(0, 0, 0), 0.0);
        // idle reported larger than total (counter skew) clamps to 0, never negative.
        assert_eq!(cpu_load_from_deltas(999, 80, 20), 0.0);
    }

    #[test]
    fn smoothing_moves_toward_raw_and_clamps() {
        // k=1 snaps to the raw value; k=0 holds the previous.
        assert_eq!(smooth_step(0.0, 1.0, 1.0), 1.0);
        assert_eq!(smooth_step(0.3, 1.0, 0.0), 0.3);
        // a half step from 0 toward 1 lands at 0.5; the move is toward raw.
        assert!((smooth_step(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
        // a sustained raw value converges upward monotonically and stays in range.
        let mut s = 0.0f32;
        for _ in 0..100 {
            let next = smooth_step(s, 1.0, CPU_SMOOTH);
            assert!(next >= s && (0.0..=1.0).contains(&next));
            s = next;
        }
        assert!(s > 0.99, "a held-busy CPU converges to ~full ({s})");
        // out-of-range raw never escapes 0..=1.
        assert!((0.0..=1.0).contains(&smooth_step(0.5, 9.0, 0.5)));
        assert!((0.0..=1.0).contains(&smooth_step(0.5, -9.0, 0.5)));
    }

    #[test]
    fn mem_load_normalises_and_clamps() {
        assert_eq!(mem_load_norm(0), 0.0);
        assert!((mem_load_norm(50) - 0.5).abs() < 1e-6);
        assert_eq!(mem_load_norm(100), 1.0);
        assert_eq!(mem_load_norm(150), 1.0, "an out-of-range percent clamps, never overshoots");
    }

    #[test]
    fn filetime_recombines_halves() {
        assert_eq!(filetime_to_u64(0, 0), 0);
        assert_eq!(filetime_to_u64(0xFFFF_FFFF, 0), 0xFFFF_FFFF);
        assert_eq!(filetime_to_u64(0, 1), 1u64 << 32);
        assert_eq!(filetime_to_u64(0xFFFF_FFFF, 1), (1u64 << 32) | 0xFFFF_FFFF);
    }
}
