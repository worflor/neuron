// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Shared daemon intent execution for resident runtimes.
//!
//! `Action` stays stateless: it can describe a DPI/profile/scroll request, but it has no device
//! handle and no active-profile cursor. GUI and CLI runtimes provide those here, so the device and
//! profile intent behavior has one implementation while app/window/instrument intents remain with
//! the resident app.

use crate::action::Intent;
use crate::capability as cap;
use crate::device::DeviceSession;
use crate::profile::{self, Profile};

pub trait ProfileCursor {
    fn active_profile(&self) -> String;
    fn set_active_profile(&mut self, name: &str);
}

pub struct ProcessProfileCursor;

impl ProfileCursor for ProcessProfileCursor {
    fn active_profile(&self) -> String {
        profile::active()
    }

    fn set_active_profile(&mut self, name: &str) {
        profile::set_active(name);
    }
}

/// How many `HyperScroll` wheel stages [`Intent::ScrollStageCycle`] cycles over. The device exposes no
/// active-stage getter and there's no per-profile stage count yet, so this is the common enabled-stage
/// count (tactile / free-spin / smart-reel); the resident cursor wraps within it. Make profile-driven
/// when a stage-count source lands.
pub const SCROLL_STAGE_COUNT: u8 = 3;

/// The next DPI for a [`Intent::DpiCycle`]: WITH `stages` it's the next stage's value (snap current to
/// the nearest stage, step, wrap); WITHOUT, a ±200 nudge so a stage-less setup still moves. Pure +
/// testable — the cycle's whole policy in one place.
fn next_dpi(cur: u16, step: i32, stages: &[u16]) -> u16 {
    if stages.is_empty() {
        return (i32::from(cur) + step * 200).clamp(100, 30_000) as u16;
    }
    let idx = stages
        .iter()
        .enumerate()
        .min_by_key(|(_, &s)| (i32::from(s) - i32::from(cur)).abs())
        .map_or(0, |(i, _)| i);
    let n = stages.len() as i32;
    let next = (idx as i32 + step).rem_euclid(n) as usize;
    stages[next].clamp(100, 30_000)
}

/// Run the shared device/profile subset of [`Intent`].
///
/// Returns `None` for app/window/instrument intents that only `neuron-app` can satisfy.
///
/// `cause` says WHERE this intent came from, and only the caller can know: the same `DpiSet` is a
/// macro verb here and a bound key there, and the onboard DPI button arrives as a `DpiCycle` that is
/// the user's thumb rather than software. It is threaded into every DPI write this runs (see
/// [`crate::dpi_origin`]) so the announce those writes provoke is recognised as ours, and it decides
/// whether the result becomes durable intent.
pub fn run_shared_intent(
    devices: &mut DeviceSession<'_>,
    cursor: &mut impl ProfileCursor,
    intent: &Intent,
    cause: crate::dpi_origin::Cause,
) -> Option<String> {
    run_shared_intent_observe(devices, cursor, intent, cause, |_| {})
}

/// Like [`run_shared_intent`], with the verified profile apply report delivered to the caller.
/// Rejected intents, paused writes, and unavailable profiles never invoke the observer.
pub fn run_shared_intent_observe(
    devices: &mut DeviceSession<'_>,
    cursor: &mut impl ProfileCursor,
    intent: &Intent,
    cause: crate::dpi_origin::Cause,
    mut on_profile_applied: impl FnMut(&crate::profile::ApplyReport),
) -> Option<String> {
    use Intent::{Teleport, Whiteboard, Knockback, Glance, Summon, Banish, Pin, Kill, Tether, Dial, Control, Echo, DpiSet, DpiCycle, ScrollStageCycle, ProfileSwitch, ProfileCycle};

    match intent {
        Teleport | Whiteboard | Knockback | Glance(_) | Summon(..) | Banish(_) | Pin(_)
        | Kill(_) | Tether(..) | Dial(_) | Control => return None,
        Echo => return Some("echo is handled by the dispatch executor".into()),
        _ => {}
    }

    if crate::writes::writes_paused() {
        return Some("[writes paused]".into());
    }

    Some(match intent {
        DpiSet(dpi) => {
            // Clamp to the device's sane range (same band the cycle uses) so a stray config value
            // can't ask the mouse for an impossible sensitivity.
            let dpi = (*dpi).clamp(100, 30_000);
            match devices.with_writable("set_dpi", |d| {
                cap::set_dpi(d, dpi, dpi, cap::Store::Persist, cause)?;
                Ok((d.pid, d.dpi_unit.clone())) // preserve the selected physical unit in the confirm baseline
            }) {
                Ok((pid, unit)) => {
                    // confirmation fires ONLY past the committed write (set is absolute — no prior
                    // read, so no old→new).
                    crate::confirm::dpi_unit(pid, &unit, u32::from(dpi), None);
                    format!("DPI -> {dpi}")
                }
                Err(e) => format!("DPI set failed: {e}"),
            }
        }
        DpiCycle(dir) => {
            // The CONTRACT is "cycle the user's CONFIGURED stages", resolved in trust order:
            //   1. the active profile's `dpi_stages` (explicit config wins);
            //   2. else the DEVICE's persisted onboard stage table — what the FEEL page writes and
            //      what the firmware itself walks in normal mode. This tier keeps the DPI button
            //      IDENTICAL across the custody line: in driver mode the firmware defers the button
            //      to us, and "own the buttons" means walking the device's own stages, not a
            //      software-only list (the empty-profile ±200-nudge regression, 2026-07-08);
            //   3. else a ±200 nudge, so a genuinely stage-less setup still does something sensible.
            let profile_stages: Vec<u16> = Profile::load(&cursor.active_profile())
                .map(|p| p.dpi_stages)
                .unwrap_or_default();
            let result = devices.with_writable("set_dpi", |d| {
                let stages = if profile_stages.is_empty() {
                    crate::writes::read_persisted_dpi_stages(d)
                } else {
                    profile_stages.clone()
                };
                let cur = cap::dpi(d).map(|(x, _)| x)?;
                let next = next_dpi(cur, dir.step(), &stages);
                cap::set_dpi(d, next, next, cap::Store::Volatile, cause)?;
                Ok((d.pid, d.dpi_unit.clone(), cur, next))
            });
            match result {
                Ok((pid, unit, cur, next)) => {
                    // the read-back gave us the prior DPI too — a true old→new confirmation.
                    crate::confirm::dpi_unit(pid, &unit, u32::from(next), Some(u32::from(cur)));
                    format!("DPI cycle {} -> {next}", dir.label())
                }
                Err(e) => format!("DPI cycle skipped ({e})"),
            }
        }
        ScrollStageCycle(dir) => {
            // The device is set-only (no active-stage getter), so we step a RESIDENT cursor and WRITE
            // the wire-confirmed stage-select (0x15/0x00, ungated). No more "pending" — it cycles for
            // real. The device's first stage is 1; we wrap over `SCROLL_STAGE_COUNT`.
            let prev = crate::writes::scroll_stage_cursor();
            let next = crate::writes::cycle_scroll_stage(prev, dir.step(), SCROLL_STAGE_COUNT);
            match devices.with_writable("set_scroll_stage", |d| {
                crate::writes::set_scroll_stage(d, next, cap::Store::Volatile)?;
                Ok(d.pid) // carry the acting device's pid for the per-device confirm de-dup
            }) {
                Ok(pid) => {
                    crate::writes::set_scroll_stage_cursor(next);
                    // confirm past the committed stage-select — a true old→new (we held the prior
                    // cursor). Was missing, so cycling sensitivity earned no card.
                    crate::confirm::scroll(pid, u32::from(next), u32::from(SCROLL_STAGE_COUNT), Some(u32::from(prev)));
                    format!("scroll stage {} -> {next}", dir.label())
                }
                Err(e) => format!("scroll stage skipped ({e})"),
            }
        }
        ProfileSwitch(name) => match Profile::load(name) {
            Ok(p) => {
                let prev = cursor.active_profile();
                // paint_lighting = false: an auto-switch applies device SETTINGS; lighting is left to
                // the caller's live compositor (the GUI streams it) so we never fight a running stream
                // for the device here. (Lighting-on-auto-switch in a headless daemon is a follow-up.)
                let rep = p.apply_with_session(devices, false);
                on_profile_applied(&rep);
                cursor.set_active_profile(name);
                profile::note_apply_report(name, &rep);
                // The gaming-mode guards are HOST-side policy, not a device write, so applying the
                // profile does not install them — this does. Without it an auto-switch (or a bound
                // profile key) changed your DPI but left the previous profile's Alt+Tab/Win
                // suppression exactly as it was: a game profile that never guarded anything, or a
                // guard that stayed on after you left the game. Set here, in the ONE place every
                // client switches through, rather than in each front end.
                crate::hook::set_policy(rep.gaming_mode);
                // ONE confirmation for the action you took (switching profiles), not one per field
                // the profile applied — those are its consequence, not a separate act.
                crate::confirm::profile(name, Some(&prev));
                format!("profile -> {name}: {}", rep.summary())
            }
            Err(e) => format!("profile '{name}': {e}"),
        },
        ProfileCycle(dir) => {
            // loadable profiles only — a file that won't parse is not somewhere to cycle TO.
            let names = profile::cycle_candidates();
            if names.is_empty() {
                return Some("profile cycle: none saved".into());
            }
            let idx = profile::cycle_index(&names, &cursor.active_profile(), dir.step());
            let name = names[idx].clone();
            match Profile::load(&name) {
                Ok(p) => {
                    let prev = cursor.active_profile();
                    let rep = p.apply_with_session(devices, false); // settings only (see ProfileSwitch)
                    on_profile_applied(&rep);
                    cursor.set_active_profile(&name);
                    profile::note_apply_report(&name, &rep);
                    crate::hook::set_policy(rep.gaming_mode); // see ProfileSwitch
                    crate::confirm::profile(&name, Some(&prev));
                    format!("profile cycle {} -> {name}: {}", dir.label(), rep.summary())
                }
                Err(e) => format!("profile cycle '{name}': {e}"),
            }
        }
        Teleport | Whiteboard | Knockback | Glance(_) | Summon(..) | Banish(_) | Pin(_)
        | Kill(_) | Echo | Tether(..) | Dial(_) | Control => {
            unreachable!("handled before shared write gate")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Intent, WindowPick};
    use crate::registry::Registry;

    #[derive(Default)]
    struct Cursor(String);

    impl ProfileCursor for Cursor {
        fn active_profile(&self) -> String {
            self.0.clone()
        }

        fn set_active_profile(&mut self, name: &str) {
            self.0 = name.to_string();
        }
    }

    #[test]
    fn app_intents_stay_with_the_resident_app() {
        let reg = Registry {
            devices: Vec::new(),
        };
        let mut devices = DeviceSession::new(&reg);
        let mut cursor = Cursor::default();

        let result = run_shared_intent(
            &mut devices,
            &mut cursor,
            &Intent::Banish(WindowPick::Focused),
            crate::dpi_origin::Cause::UserApplied,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn dpi_cycle_walks_the_stage_list_and_wraps() {
        let stages = [800u16, 1600, 3200];
        assert_eq!(next_dpi(800, 1, &stages), 1600);
        assert_eq!(next_dpi(1600, 1, &stages), 3200);
        assert_eq!(next_dpi(3200, 1, &stages), 800, "wrap up");
        assert_eq!(next_dpi(800, -1, &stages), 3200, "wrap down");
        // a value between stages snaps to the nearest, then steps from there
        assert_eq!(next_dpi(1500, 1, &stages), 3200, "nearest 1600 -> next 3200");
        // no stages -> ±200 nudge, clamped to the device band
        assert_eq!(next_dpi(1000, 1, &[]), 1200);
        assert_eq!(next_dpi(100, -1, &[]), 100, "clamp floor");
    }

    #[test]
    fn writes_paused_short_circuits_device_intents_before_opening_hardware() {
        let saved = crate::writes::writes_paused();
        crate::writes::set_writes_paused(true);
        let reg = Registry {
            devices: Vec::new(),
        };
        let mut devices = DeviceSession::new(&reg);
        let mut cursor = Cursor::default();

        let dpi_result = run_shared_intent(
            &mut devices,
            &mut cursor,
            &Intent::DpiSet(800),
            crate::dpi_origin::Cause::UserApplied,
        );
        let mut observed = 0;
        let result = run_shared_intent_observe(
            &mut devices,
            &mut cursor,
            &Intent::ProfileSwitch("unavailable-profile".into()),
            crate::dpi_origin::Cause::UserApplied,
            |_| observed += 1,
        );

        crate::writes::set_writes_paused(saved);
        assert_eq!(dpi_result.as_deref(), Some("[writes paused]"));
        assert_eq!(result.as_deref(), Some("[writes paused]"));
        assert_eq!(observed, 0, "a refused profile intent cannot invalidate a held DPI snapshot");
    }
}
