// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Device VITALS — battery/charge status cards from a tiny per-device state machine.
//!
//! Three variables per battery-capable device: `next_read_at` (the throttle — the earliest instant a
//! re-read is allowed), `batt` (last %), `charging` (last charge state). From those plus the current
//! instant we DERIVE the whole suite — a battery-aware sampling window (frugal when healthy, snappy in
//! the danger zone), charge engage/disengage edges, a low/critical threshold CROSSING (self-de-duping:
//! once `batt` is already ≤ a line the edge can't re-fire), and a full-charge edge. No timer of our
//! own: the app feeds us off events it already handles (a UI status read, the `05 0c` charge poke,
//! device activity), gated by [`due`] so the device is re-read only when it's both stale AND already
//! awake — so we never wake a sleeping mouse just to check.

use crate::confirm;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

// Discharge warn-points, densening toward empty — the array IS the urgency curve (20% heads-up, then
// the gap SHRINKS 10·5·3·2 so warnings crowd as it gets dire). One card per DOWNWARD crossing; at or
// below CRIT it reads "critical".
const WARN_AT: [u8; 4] = [20, 10, 5, 2];
const CRIT: u8 = 5;
// The critical/low title split keys off CRIT, so CRIT must be one of the warn-points.
const _: () = assert!(WARN_AT[2] == CRIT);

// Sentinel for "no sample yet". 255 is an IMPOSSIBLE battery % (the hardware reports 0..=100), so the
// first observe() for a device baselines silently and can never mistake a real reading for "unread".
const UNREAD: u8 = 255;

// A claimed read that FAILS retries this soon, rather than holding the full window — so a transient
// I/O error doesn't strand low-battery detection for a minute.
const RETRY_AFTER: Duration = Duration::from_secs(3);

struct Vitals {
    // The earliest instant [`due`] will permit another read — the throttle. Set battery-aware (tighter
    // as the battery drops) and pushed out on every read CLAIM so a burst of reports can't herd reads.
    next_read_at: Instant,
    batt: u8,
    charging: bool,
}

/// Sampling window by battery level: frugal when healthy, snappy in the danger zone so the 20/10/5/2
/// warnings fire promptly during active use. `UNREAD` (255) falls to the conservative 60s.
fn freshness_for(batt: u8) -> Duration {
    match batt {
        0..=5 => Duration::from_secs(5),
        6..=10 => Duration::from_secs(10),
        11..=20 => Duration::from_secs(20),
        _ => Duration::from_secs(60),
    }
}

fn state() -> &'static Mutex<HashMap<u16, Vitals>> {
    static S: OnceLock<Mutex<HashMap<u16, Vitals>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Should we read `pid` now? `forced` (a charge poke) is always yes. Otherwise only once the
/// battery-aware window has elapsed. Returning true CLAIMS the slot (pushes `next_read_at` out) so a
/// burst of activity reports doesn't spawn a herd of concurrent reads. A claimed read that then FAILS
/// must call [`mark_stale`] so the claim doesn't strand the device for a whole window.
pub fn due(pid: u16, forced: bool) -> bool {
    let now = Instant::now();
    let mut map = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let v = map
        .entry(pid)
        .or_insert(Vitals { next_read_at: now, batt: UNREAD, charging: false });
    if forced || now >= v.next_read_at {
        v.next_read_at = now + freshness_for(v.batt); // claim
        true
    } else {
        false
    }
}

/// A claimed read FAILED — allow a retry shortly ([`RETRY_AFTER`]) rather than holding the full window.
pub fn mark_stale(pid: u16) {
    let mut map = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(v) = map.get_mut(&pid) {
        v.next_read_at = Instant::now() + RETRY_AFTER;
    }
}

/// The last-known charging state for `pid`, if we have a real prior sample. Lets a feed point fall
/// back to the prior charge state on a charge-read blip instead of defaulting to a phantom "unplugged".
pub fn last_charging(pid: u16) -> Option<bool> {
    let map = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get(&pid).filter(|v| v.batt != UNREAD).map(|v| v.charging)
}

/// Feed a fresh (battery %, charging) sample and fire any edge cards. `from_event` = this sample came
/// from a real device EVENT (a charge poke), not a passive scan — so a device hot-plugged AFTER launch
/// announces its first charge state instead of silently baselining. Decisions are computed under the
/// lock; the lock is RELEASED before any `confirm::battery` (a channel send), so we never hold it
/// across the emit.
pub fn observe(pid: u16, batt: u8, charging: bool, from_event: bool) {
    let now = Instant::now();
    // (pct, title, prev) cards to emit once the lock is dropped. Empty Vec doesn't allocate.
    let mut cards: Vec<(u32, &'static str, Option<u32>)> = Vec::new();
    {
        let mut map = state().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let v = map
            .entry(pid)
            .or_insert(Vitals { next_read_at: now, batt: UNREAD, charging: false });
        v.next_read_at = now + freshness_for(batt); // refresh the throttle off the real sample

        if v.batt == UNREAD {
            // first sample = silent baseline, UNLESS it's a real event on a freshly-plugged device.
            if from_event && charging {
                cards.push((batt as u32, "Charging", None));
            }
        } else {
            let prev = v.batt;
            // INDEPENDENT edges (not else-if) so a charge flip can't swallow a co-occurring warning.
            if charging != v.charging {
                if !charging && batt <= WARN_AT[0] {
                    // unplugged INTO a warning zone — lead with the warning (it conveys on-battery too).
                    let title = if batt <= CRIT { "Battery critical" } else { "Battery low" };
                    cards.push((batt as u32, title, Some(prev as u32)));
                } else {
                    let title = if charging { "Charging" } else { "On battery" };
                    cards.push((batt as u32, title, Some(prev as u32)));
                }
            }
            if charging && batt >= 100 && prev < 100 {
                cards.push((100, "Fully charged", Some(prev as u32)));
            }
            if !charging && charging == v.charging {
                // pure discharge (no flip this frame): fire on the most-urgent downward crossing.
                if let Some(&t) = WARN_AT.iter().rev().find(|&&t| prev > t && batt <= t) {
                    let title = if t <= CRIT { "Battery critical" } else { "Battery low" };
                    cards.push((batt as u32, title, Some(prev as u32)));
                }
            }
        }
        v.batt = batt;
        v.charging = charging;
    } // lock released here

    for (pct, title, prev) in cards {
        confirm::battery(pct, title, prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset(pid: u16) {
        state().lock().unwrap().remove(&pid);
    }

    #[test]
    fn freshness_tightens_as_battery_drops() {
        assert_eq!(freshness_for(80), Duration::from_secs(60));
        assert_eq!(freshness_for(20), Duration::from_secs(20));
        assert_eq!(freshness_for(10), Duration::from_secs(10));
        assert_eq!(freshness_for(5), Duration::from_secs(5));
        assert_eq!(freshness_for(UNREAD), Duration::from_secs(60));
    }

    #[test]
    fn warn_at_is_descending_and_holds_crit() {
        // the urgency curve: strictly descending, last gap smallest, CRIT present.
        assert!(WARN_AT.windows(2).all(|w| w[0] > w[1]));
        assert!(WARN_AT.contains(&CRIT));
    }

    #[test]
    fn due_claims_then_throttles_and_forced_bypasses() {
        let pid = 0xFF01;
        reset(pid);
        assert!(due(pid, false), "first ever read is due");
        assert!(!due(pid, false), "immediately after a claim it is throttled");
        assert!(due(pid, true), "a forced (charge-poke) read always passes");
        reset(pid);
    }

    #[test]
    fn mark_stale_unblocks_a_failed_claim_quickly() {
        let pid = 0xFF02;
        reset(pid);
        assert!(due(pid, false));
        assert!(!due(pid, false));
        mark_stale(pid); // a failed read — next_read_at moved to now + RETRY_AFTER (3s), not the 60s window
        // we can't sleep 3s in a unit test, but we can assert the window shrank well under a freshness window.
        let until = state().lock().unwrap().get(&pid).unwrap().next_read_at;
        assert!(until <= Instant::now() + RETRY_AFTER + Duration::from_millis(50));
        reset(pid);
    }

    #[test]
    fn last_charging_is_none_until_a_real_sample() {
        let pid = 0xFF03;
        reset(pid);
        assert_eq!(last_charging(pid), None, "no sample yet");
        observe(pid, 80, true, false); // first sample = silent baseline
        assert_eq!(last_charging(pid), Some(true));
        reset(pid);
    }
}
