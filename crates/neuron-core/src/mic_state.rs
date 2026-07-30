// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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

// ── the ECHO LATCH — tell neuron's OWN OS-mute writes apart from a genuinely external change ──
//
// Every place neuron itself writes the default capture endpoint's OS mute (`hidwatch::
// bridge_mic_mute` mirroring a hardware tap, the app's mic-toggle button, a momentary-mic hold, an
// `Action::MicMute`) calls [`note_self_mute_write`] with the value it just wrote — but ONLY when
// that write lands on the DEFAULT capture endpoint (`crate::audio::is_default_capture_id`), the one
// stream the detector samples; a write to a secondary mic must never arm this latch or it could
// swallow a real external edge on the default mic whenever the two agree on a value.
//
// The dispatch mic-tap detector — which watches the OS mute for edges to fire `Trigger::MicTap` —
// asks [`is_self_echo`] whether the sample it just took is explained by our own write, and if so
// does not re-fire MicTap's bound effects (state is still published unconditionally either way —
// only the EFFECTS are gated).
//
// DO NOT try to identify our own writes BY VALUE. Three versions tried and each shipped a real bug,
// because the detector's input — a ~400ms-refreshed cache sampled every ~50ms — coalesces and
// reorders our writes beyond recovery:
//   1. clear-on-every-poll: consumed before our write ever surfaced → the echo fired as a phantom.
//   2. a QUEUE of pending values: kept dead history alive → a stale entry SWALLOWED a genuine tap.
//   3. convergence on the LATEST value: a momentary press+release RETURNS TO ITS STARTING VALUE, so
//      the stale pre-write sample equals the expectation, "converges" trivially before the cache has
//      seen anything, and the late intermediate then fires as a phantom.
// Each fix was correct and each was an epicycle. The value channel cannot answer "who wrote this?".
//
// So this asks a question the channel CAN answer: **did neuron write recently?** A plain time window.
// No values, no matching, no ordering, therefore no aliasing — the whole bug class is unrepresentable.
//
// The honest trade: a genuine external change within [`SELF_WRITE_QUIET`] of one of our own writes is
// attributed to us and its MicTap effects are skipped. That is the SAFE direction — a phantom tap
// runs the user's bound actions unbidden, a missed one merely does nothing — and it is narrow: only
// right after neuron itself wrote the mute. It costs nothing for the PHYSICAL tap, which no longer
// comes through here at all: `hidwatch` fires MicTap straight from the HID edge it decoded (ground
// truth, and instant), and this window is what stops its bridge write from double-firing.
//
// This is a STOPGAP with a known exit: Core Audio's `SetMute` takes an event-context GUID that
// `IAudioEndpointVolumeCallback::OnNotify` hands back, so origin can arrive WITH the change and all
// of this — window, detector polling, inference — can be deleted.
static LAST_SELF_WRITE: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();

/// BACKSTOP TTL only — the window normally closes when our write's EDGE actually surfaces in the
/// cache (see [`consume_self_write_on_edge`]), NOT on this clock. The clock exists solely so a write
/// that never produces an observable edge (write failed to land, endpoint vanished) can't leave the
/// window armed forever and swallow a later real tap. Generous, because the cache is nominally ~400ms
/// but its Core-Audio call can stall / the pump can starve — a fixed 400ms would let a delayed edge
/// fire as a phantom, the exact bug this backstop must NOT cause by closing too early.
const SELF_WRITE_QUIET: Duration = Duration::from_secs(3);

fn last_self_write() -> &'static Mutex<Option<Instant>> {
    LAST_SELF_WRITE.get_or_init(|| Mutex::new(None))
}

/// Record that NEURON ITSELF just wrote the DEFAULT capture endpoint's OS mute — see the doc above.
/// Callers MUST have checked `crate::audio::is_default_capture_id` first (the detector only ever
/// samples the default endpoint, so a secondary mic's write must never open this window).
///
/// Takes no value ON PURPOSE: what we wrote is exactly the thing that cannot be matched reliably
/// against a lagging cache, and a parameter nobody can use honestly is a lie in the signature.
pub fn note_self_mute_write() {
    *last_self_write()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
}

/// Forget any open window. TEST SUPPORT — this is process-global, so a test wanting a known-clean
/// start (including one in ANOTHER crate, e.g. the dispatch detector's sequence tests) needs a way
/// to clear it. Mirrors `neuron-app::reconcile`'s `reset_readiness`/`reset_registry`. Nothing in the
/// live path calls this.
pub fn reset_self_mute_write() {
    *last_self_write()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Is the self-write window open right now? (Within the backstop TTL of an un-consumed write.)
fn window_open() -> bool {
    last_self_write()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_some_and(|at| at.elapsed() <= SELF_WRITE_QUIET)
}

/// Called by the dispatch detector for a NON-edge sample (the cache still shows the same value as
/// last poll): is our own write still pending? Just reports whether the window is open; a non-edge
/// can't be the write surfacing, so it never consumes. Kept distinct from
/// [`consume_self_write_on_edge`] so the edge case can CLOSE the window and this cannot.
pub fn in_self_write_window() -> bool {
    window_open()
}

/// Called by the dispatch detector for an EDGE sample (the cache value changed since last poll):
/// should this edge be credited to NEURON rather than the user?
///
/// If our write is still pending (window open), THIS edge is that write finally surfacing in the
/// cache — however delayed. Consume the window (close it) and return `true` to suppress MicTap: the
/// write produces exactly ONE edge, so once we've attributed it, the window's job is done and any
/// FURTHER edge is genuinely external. Closing on the edge — not on a wall clock — is what makes the
/// suppression robust to an arbitrarily-delayed cache observation (endpoint stall / pump starvation),
/// which a fixed-duration window could not cover. Returns `false` (fire) when nothing was pending.
pub fn consume_self_write_on_edge() -> bool {
    let mut g = last_self_write()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *g {
        Some(at) if at.elapsed() <= SELF_WRITE_QUIET => {
            *g = None; // this edge IS our write surfacing — attributed, window done
            true
        }
        _ => {
            *g = None; // stale/absent — clear so it can't linger
            false
        }
    }
}

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
    // The latch is cleared by the release — which runs on completion, panic, OR a spawn refusal —
    // so a failed spawn can never leave the sampler latched "on" and block every later `ensure`.
    crate::worker::spawn_guarded(
        "neuron-mic-state",
        || *running().lock().unwrap_or_else(|p| p.into_inner()) = false,
        run,
    );
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

/// Publish a device-PUSHED hardware mute event directly into the tri-state (e.g. the Seiren V3
/// Mini's capacitive tap, bridged to the OS capture mute by `hidwatch::bridge_mic_mute` BEFORE this
/// call). This is the EVENT feed, not the sampler — it does NOT start `run` — so the honesty
/// contract stays: the ~8Hz poller (when running) may overwrite this within one tick (~125ms) with
/// its own read of the OS endpoint, which is correct, not a race, because the caller already
/// converged the OS mute to `muted` first — the two feeds agree. This call only makes the flip feel
/// INSTANT instead of waiting for the next poll tick.
pub fn publish_hardware(muted: bool) {
    STATE.store(if muted { 2 } else { 1 }, Ordering::Relaxed);
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
            // `running` is cleared by the spawn's release, not here — see `ensure`.
            STATE.store(0, Ordering::Relaxed);
            return;
        }
        // (Re-)resolve the default capture endpoint on the first tick and every ~1s after —
        // a swapped default mic or an unplug is picked up within a second.
        if tick.is_multiple_of(RERESOLVE_TICKS) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_hardware_stores_the_expected_atomic_state() {
        // Test the raw STATE, not `muted()` — under cfg(test) `muted()` prefers TEST_OVERRIDE, which
        // would mask what this function actually stores.
        publish_hardware(true);
        assert_eq!(STATE.load(Ordering::Relaxed), 2, "muted must store the MUTED tri-state");
        publish_hardware(false);
        assert_eq!(STATE.load(Ordering::Relaxed), 1, "live must store the LIVE tri-state");
    }

    // ── the self-write window ────────────────────────────────────────────────────────────────

    /// Serializes these: the window is one process-global cell every test below shares.
    static LATCH_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Open the window as if the write happened at `at` — so expiry is testable without sleeping.
    fn arm_at(at: Instant) {
        *last_self_write().lock().unwrap_or_else(|e| e.into_inner()) = Some(at);
    }

    #[test]
    fn no_write_means_an_edge_is_the_users() {
        let _guard = LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_self_mute_write();
        assert!(!in_self_write_window(), "no window open");
        assert!(
            !consume_self_write_on_edge(),
            "with nothing of ours pending, an edge belongs to the user (fire)"
        );
    }

    #[test]
    fn our_writes_edge_is_consumed_exactly_once() {
        let _guard = LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_self_mute_write();
        note_self_mute_write();
        assert!(in_self_write_window(), "a non-edge poll still sees the window open");
        assert!(consume_self_write_on_edge(), "our write's edge is ours — suppress");
        assert!(
            !consume_self_write_on_edge(),
            "and it CLOSED on that edge — the next edge is external (fire)"
        );
        assert!(!in_self_write_window(), "window is closed after the edge consumed it");
    }

    #[test]
    fn a_non_edge_poll_does_not_consume_the_window() {
        // The un-changed polls before our write surfaces must NOT close the window — only the edge.
        let _guard = LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_self_mute_write();
        note_self_mute_write();
        for _ in 0..8 {
            assert!(in_self_write_window(), "still armed through the lagging polls");
        }
        assert!(consume_self_write_on_edge(), "then the edge consumes it");
    }

    #[test]
    fn the_backstop_ttl_closes_a_write_that_never_produced_an_edge() {
        // If our write never surfaces as an edge (write didn't land / endpoint vanished), the TTL is
        // the only thing that stops the window swallowing a later real tap.
        let _guard = LATCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reset_self_mute_write();
        let Some(stale) = Instant::now().checked_sub(SELF_WRITE_QUIET + Duration::from_millis(200))
        else {
            return; // machine booted moments ago — no earlier instant to backdate to
        };
        arm_at(stale);
        assert!(!in_self_write_window(), "past the backstop TTL, the window is closed");
        assert!(
            !consume_self_write_on_edge(),
            "an aged-out window suppresses nothing — a real edge fires"
        );
    }

    #[test]
    fn the_backstop_ttl_outlasts_the_cache_refresh() {
        // The TTL is a BACKSTOP, but it must still comfortably outlast the ~400ms cache — a shorter
        // one would let a merely-slow (not stalled) edge age out and fire as a phantom.
        assert!(
            SELF_WRITE_QUIET >= Duration::from_millis(800),
            "backstop TTL must comfortably exceed the ~400ms cache refresh"
        );
    }
}
