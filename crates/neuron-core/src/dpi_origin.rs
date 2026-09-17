// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! DPI PROVENANCE — who caused the sensitivity the device is reporting.
//!
//! ## What it is for
//!
//! A mouse announces a DPI change (`05 02`) identically whether the user pressed its onboard
//! button, a macro fired, neuron healed it, or the firmware restored a stale plane on wake. The
//! report carries no cause, and the value alone cannot supply one: testing it against the
//! configured stage cycle says nothing, because a wake that restores the wrong stage restores a
//! cycle member by construction — with two stages, every wrong answer is a member.
//!
//! So the cause is recorded where it is known, at the write, rather than reconstructed at the read.
//! Every DPI write declares a [`Cause`]; [`crate::capability::set_dpi`] and
//! [`crate::writes::set_dpi_stages`] take one as a required argument, so a new way to change DPI
//! cannot skip attribution. An announce is then classified by comparison.
//!
//! ## The two channels, and why both are needed
//!
//! * **This ledger** is in-memory and per-process: it answers "did I just write that?". It covers
//!   the tray's own writes — the GUI, bound keys, macros, the onboard button neuron fulfils in
//!   driver mode, sniper.
//! * **[`crate::feel_intent`]** is on disk and shared: it answers "is that what the user
//!   configured?". It is what carries a `neuron dpi 800` typed in a terminal across to the resident
//!   tray, which never saw that write happen.
//!
//! A value accounted for by either is left alone. Only a value accounted for by NEITHER is
//! [`Origin::Foreign`] — nobody asked for it, so the device did it to itself.
//!
//! ## What this can and cannot speak for
//!
//! Attribution covers changes that PASS THROUGH neuron. Whether a given change does is a property
//! of the device's mode, not of this module: in driver mode the firmware defers its DPI button to
//! the host, so every legitimate change is one neuron wrote; in normal mode the firmware walks the
//! cycle itself and only announces the result, and that announce is indistinguishable here from a
//! stale restore. [`Origin::Foreign`] therefore means "neuron cannot account for this", not "the
//! user did not do this" — a caller acting on it owes its own check that neuron was in a position
//! to know. `hidwatch::maybe_reconcile_announced` is the worked example.
//!
//! ## No expiry, deliberately
//!
//! The ledger keeps the last value written per device with no time window. A window would be a
//! second guess about timing, which is the mistake this module exists to stop making: if the last
//! thing neuron wrote to a device was X and the device says it is at X, the two agree and there is
//! nothing to reconcile, whether the announce took two milliseconds or arrived after a sleep. When
//! the device later moves to some other value, that value no longer matches and the comparison
//! speaks for itself.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Why a DPI write happened. Declared by the writer, never inferred by a reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The user committed a sensitivity through one of neuron's own surfaces — the GUI's apply, a
    /// CLI `dpi`/`dpi-stages`, a profile apply, a bound key, a macro verb. Durable: this is exactly
    /// the state a wake is supposed to restore.
    UserApplied,
    /// The user walked the configured cycle with the device's own DPI button, which the firmware
    /// defers to us in driver mode. Durable for the same reason [`Cause::UserApplied`] is — the
    /// button is a user surface, and treating it as less real is what made an onboard DPI choice
    /// evaporate on the next wake.
    UserCycled,
    /// A momentary, self-reverting override — the sniper hold. Never durable: it is not a standing
    /// choice, and recording it would teach the wake reassert to restore a precision dip the user
    /// released minutes ago.
    Momentary,
    /// neuron writing recorded intent back to a device that drifted. Never durable: it carries no
    /// new information, it IS the recorded intent being re-applied.
    Reassert,
}

impl Cause {
    /// Should a write with this cause become the durable record of what the user wants?
    ///
    /// The two `false` arms are the load-bearing ones. A momentary dip is not a preference, and a
    /// reassert writing intent back into `feel_intent` would be a feedback loop that can only
    /// launder drift into truth.
    pub fn is_durable(self) -> bool {
        matches!(self, Cause::UserApplied | Cause::UserCycled)
    }

    /// A short, stable label for logs and flight-recorder breadcrumbs.
    pub fn label(self) -> &'static str {
        match self {
            Cause::UserApplied => "user-applied",
            Cause::UserCycled => "user-cycled",
            Cause::Momentary => "momentary",
            Cause::Reassert => "reassert",
        }
    }
}

/// What a device-announced DPI turned out to be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// The echo of a write this process made. Nothing to do — we already know.
    Echo(Cause),
    /// Not ours, but it equals the durable recorded intent, so someone legitimate put it there
    /// (another process, or this one in an earlier session). Nothing to do.
    Configured,
    /// There is no recorded intent for this device, so there is nothing to compare against. Missing
    /// evidence is not evidence of drift: a device the user never configured through neuron is left
    /// alone. (A reassert could not run here anyway — it has no intent to write back.)
    Unknown,
    /// Accounted for by neither channel: no write of ours produced it and it is not what the user
    /// configured. The device changed its own sensitivity — a wake restoring a stale plane, a
    /// firmware profile reload. This is the only case a reconcile may act on.
    Foreign,
}

/// Last value this process wrote to each device, and why.
fn ledger() -> &'static Mutex<HashMap<u16, (u16, Cause)>> {
    static L: OnceLock<Mutex<HashMap<u16, (u16, Cause)>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that this process is writing DPI `x` to `pid` because of `cause`.
///
/// Called by the write funnels themselves, so a DPI cannot reach the wire without being accounted
/// for. Stamped BEFORE the write goes out: the device can announce the new value before the setter
/// returns, and an announce that overtakes its own stamp would be classified [`Origin::Foreign`]
/// and "healed" — neuron fighting its own write.
/// Returns whatever the ledger held before, for [`rollback`] if the write does not land.
#[must_use = "a write that fails must roll the stamp back, or the ledger claims a value the device never took"]
pub fn expect(pid: u16, x: u16, cause: Cause) -> Option<(u16, Cause)> {
    let mut map = ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    map.insert(pid, (x, cause))
}

/// Undo an [`expect`] whose write failed, putting `prev` back.
///
/// Without this, a refused write leaves the ledger asserting a value the device never took — and
/// the device really arriving at that value later (the exact drift this module exists to catch)
/// would then be waved through as our own echo. Restores the PREVIOUS entry rather than clearing,
/// so a failed write cannot also erase the record of the last one that succeeded.
pub fn rollback(pid: u16, prev: Option<(u16, Cause)>) {
    let mut map = ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    match prev {
        Some(entry) => map.insert(pid, entry),
        None => map.remove(&pid),
    };
}

/// Classify a DPI the device announced for `pid`, against both channels. See [`Origin`].
pub fn classify(pid: u16, announced: u16) -> Origin {
    let last_write = {
        let map = ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        map.get(&pid).copied()
    };
    let intent_dpi = crate::feel_intent::get(pid).and_then(|i| i.dpi).map(|(x, _)| x);
    decide(last_write, intent_dpi, announced)
}

/// The classification rule itself, over values rather than lookups.
///
/// Split out so the decision is testable with neither hardware nor a run root — the same shape
/// `writes::dpi_in_cycle` used to keep the old membership rule testable. Order matters: our own
/// write is checked first, so a reassert or a sniper dip is recognised as ours even while it
/// disagrees with durable intent (which, mid-sniper, it is supposed to).
pub fn decide(
    last_write: Option<(u16, Cause)>,
    intent_dpi: Option<u16>,
    announced: u16,
) -> Origin {
    if let Some((x, cause)) = last_write {
        if x == announced {
            return Origin::Echo(cause);
        }
    }
    match intent_dpi {
        Some(want_x) if want_x == announced => Origin::Configured,
        Some(_) => Origin::Foreign,
        None => Origin::Unknown,
    }
}

/// Drop `pid`'s ledger entry, so the next announce is judged purely against durable intent.
pub fn forget(pid: u16) {
    let mut map = ledger().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    map.remove(&pid);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pids no other test drives — the ledger is process-global and the suite runs in parallel.
    const PID_A: u16 = 0xFD01;
    const PID_B: u16 = 0xFD02;

    #[test]
    fn our_own_write_is_recognised_as_an_echo_whatever_the_cause() {
        for cause in [Cause::UserApplied, Cause::UserCycled, Cause::Momentary, Cause::Reassert] {
            forget(PID_A);
            let _ = expect(PID_A, 1600, cause);
            assert_eq!(
                classify(PID_A, 1600),
                Origin::Echo(cause),
                "a value this process just wrote must never be treated as drift"
            );
        }
        forget(PID_A);
    }

    #[test]
    fn a_rolled_back_stamp_restores_the_previous_write_not_a_blank() {
        forget(PID_A);
        let _ = expect(PID_A, 30000, Cause::UserApplied);
        // A write that is refused by the device must leave no trace of itself...
        let prev = expect(PID_A, 800, Cause::UserApplied);
        rollback(PID_A, prev);
        // ...and must not take the last GOOD stamp with it.
        assert_eq!(classify(PID_A, 30000), Origin::Echo(Cause::UserApplied));
        // The value that never landed is not ours, so a device arriving there is still drift.
        assert_eq!(
            decide(Some((30000, Cause::UserApplied)), Some(30000), 800),
            Origin::Foreign
        );
        forget(PID_A);
    }

    #[test]
    fn a_value_nobody_wrote_and_nobody_configured_is_foreign() {
        // The live 2026-09-16 shape, exactly: intent 30000, the device announcing 800.
        assert_eq!(
            decide(Some((30000, Cause::UserApplied)), Some(30000), 800),
            Origin::Foreign,
            "the device moved to a value neither this process nor the user's record accounts for"
        );
    }

    #[test]
    fn a_device_with_no_recorded_intent_is_never_judged() {
        // Missing evidence is not evidence of drift, and a reassert would have nothing to write.
        assert_eq!(decide(None, None, 800), Origin::Unknown);
        assert_eq!(decide(Some((30000, Cause::UserApplied)), None, 800), Origin::Unknown);
    }

    #[test]
    fn another_process_write_is_recognised_through_durable_intent() {
        // A `neuron dpi 800` typed in a terminal records intent on disk; the resident tray never saw
        // that write, so its ledger still holds something else. The disk channel is what stops the
        // tray fighting it.
        assert_eq!(
            decide(Some((30000, Cause::UserApplied)), Some(800), 800),
            Origin::Configured
        );
    }

    #[test]
    fn a_sniper_dip_is_ours_even_though_it_disagrees_with_intent() {
        // Mid-hold the device is deliberately NOT at the configured DPI. Checking our own write
        // first is what stops the reconcile yanking the user out of a precision hold.
        assert_eq!(
            decide(Some((400, Cause::Momentary)), Some(30000), 400),
            Origin::Echo(Cause::Momentary)
        );
    }

    #[test]
    fn momentary_and_reassert_never_become_durable_intent() {
        assert!(Cause::UserApplied.is_durable());
        assert!(Cause::UserCycled.is_durable(), "an onboard button press is a real user choice");
        assert!(!Cause::Momentary.is_durable(), "a sniper dip must not outlive the hold");
        assert!(
            !Cause::Reassert.is_durable(),
            "a reassert re-applying intent must never write back into intent"
        );
    }

    #[test]
    fn membership_in_the_cycle_is_not_what_decides() {
        // The defect this module replaces, pinned as a test. 800 is a perfectly good member of the
        // cycle [800, 30000]; that fact is irrelevant, and treating it as exculpatory is precisely
        // how the wander survived. Provenance judges it foreign on the evidence that actually bears
        // on the question: nobody wrote it and it is not what was configured.
        let cycle = [800u16, 30000];
        assert!(cycle.contains(&800), "the premise: the wrong value IS a cycle member");
        assert_eq!(decide(Some((30000, Cause::UserApplied)), Some(30000), 800), Origin::Foreign);
    }

    #[test]
    fn the_ledger_round_trips_through_the_process_global_lookup() {
        // `decide` carries the rule; this covers the wiring `classify` puts in front of it.
        forget(PID_B);
        let _ = expect(PID_B, 1600, Cause::UserCycled);
        assert_eq!(classify(PID_B, 1600), Origin::Echo(Cause::UserCycled));
        forget(PID_B);
        assert_ne!(
            classify(PID_B, 1600),
            Origin::Echo(Cause::UserCycled),
            "forget must actually drop the entry"
        );
    }
}
