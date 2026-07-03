//! A shared, live MIC-MUTE provider — the single source of truth the `miclight` lighting
//! pattern (and its preview) reads.
//!
//! Same provider discipline as [`crate::audio_level`] (one background sampler, lock-free
//! published state, idle auto-stop) for a different question: *is the system microphone muted
//! right now?* A pattern must never open a COM endpoint per rendered frame, so ONE thread
//! samples the default capture endpoint's mute state at ~8Hz and publishes a tri-state:
//! unknown (no mic / not yet read — the board renders dark, honest), live, or muted.
//!
//! Unlike the peak meter there is no error-aware mute getter (`VolumeCtl::get_mute` swallows a
//! dead handle into `false` — the flagged silent-zero smell), so honesty comes from RE-RESOLUTION
//! instead: every ~1s the sampler re-resolves the default capture endpoint and reopens the handle
//! if the device changed or vanished. A stale answer is therefore bounded to ~1s, and a missing
//! mic reads UNKNOWN, never "live".
//!
//! Platform-neutral by construction: it only speaks `crate::audio`'s already-seamed surface
//! (`resolve_capture` / `VolumeCtl`), whose off-Windows stubs resolve nothing — so off-Windows
//! this publishes a steady UNKNOWN and the layer idles dark, with zero cfg in this file.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The published tri-state: 0 = unknown (no mic / no read yet), 1 = live, 2 = muted.
static STATE: AtomicU8 = AtomicU8::new(0);
/// Millis since the process epoch of the last [`muted`] read — drives the idle auto-stop.
static LAST_ACCESS_MS: AtomicU64 = AtomicU64::new(0);

fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// Whether a sampler thread is alive (the control block; no repointing — there is only one
/// default mic — so a plain flag suffices where audio_level needs a generation counter).
fn running() -> &'static Mutex<bool> {
    static R: OnceLock<Mutex<bool>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(false))
}

/// Mute is human-speed state; ~8Hz keeps a bound press visibly instant without COM churn.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(125);
/// Re-resolve the default endpoint every ~1s (8 ticks) — bounds the dead-handle staleness.
const RERESOLVE_TICKS: u32 = 8;
const IDLE_STOP_MS: u64 = 2000;

/// Start the sampler (idempotent, cheap to call every frame — the `miclight` pattern does).
pub fn ensure() {
    let mut on = running().lock().unwrap_or_else(|p| p.into_inner());
    if *on {
        return;
    }
    *on = true;
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    drop(on);
    let _ = thread::Builder::new()
        .name("neuron-mic-state".into())
        .spawn(run);
}

/// The latest mute state: `None` = unknown (no mic resolved, or nothing sampled yet),
/// `Some(true)` = muted, `Some(false)` = live. Reading it keeps the sampler alive.
pub fn muted() -> Option<bool> {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    // Under test, a set override WINS over whatever the (possibly live, possibly real-mic)
    // sampler publishes — pattern tests must be deterministic on any machine.
    #[cfg(test)]
    match TEST_OVERRIDE.load(Ordering::Relaxed) {
        1 => return None,
        2 => return Some(false),
        3 => return Some(true),
        _ => {}
    }
    match STATE.load(Ordering::Relaxed) {
        1 => Some(false),
        2 => Some(true),
        _ => None,
    }
}

/// Test-only OVERRIDE of the tri-state (0 unknown / 1 live / 2 muted) — stored out-of-band so a
/// real sampler thread another test started can't race it away mid-assertion.
#[cfg(test)]
static TEST_OVERRIDE: AtomicU8 = AtomicU8::new(0);

#[cfg(test)]
pub(crate) fn test_set(state: u8) {
    TEST_OVERRIDE.store(state + 1, Ordering::Relaxed);
}

fn run() {
    let mut ctl: Option<crate::audio::VolumeCtl> = None;
    let mut ctl_id = String::new();
    let mut tick: u32 = 0;
    loop {
        // idle auto-stop: nobody read `muted()` in a while → stop and go unknown.
        let idle = now_ms().saturating_sub(LAST_ACCESS_MS.load(Ordering::Relaxed));
        if idle > IDLE_STOP_MS {
            *running().lock().unwrap_or_else(|p| p.into_inner()) = false;
            STATE.store(0, Ordering::Relaxed);
            return;
        }
        // (Re-)resolve the default capture endpoint on the first tick and every ~1s after —
        // a swapped default mic or an unplug is picked up within a second.
        if tick % RERESOLVE_TICKS == 0 {
            match crate::audio::resolve_capture(None) {
                Some(ep) => {
                    if ep.id != ctl_id || ctl.is_none() {
                        ctl = crate::audio::VolumeCtl::open(&ep.id);
                        ctl_id = ep.id;
                    }
                }
                None => {
                    ctl = None;
                    ctl_id.clear();
                }
            }
        }
        tick = tick.wrapping_add(1);
        let state = match &ctl {
            Some(c) => {
                if c.get_mute() {
                    2
                } else {
                    1
                }
            }
            None => 0, // no mic → UNKNOWN, never "live"
        };
        STATE.store(state, Ordering::Relaxed);
        thread::sleep(SAMPLE_INTERVAL);
    }
}
