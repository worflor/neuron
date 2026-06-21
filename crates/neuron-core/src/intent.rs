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

/// How many HyperScroll wheel stages [`Intent::ScrollStageCycle`] cycles over. The device exposes no
/// active-stage getter and there's no per-profile stage count yet, so this is the common enabled-stage
/// count (tactile / free-spin / smart-reel); the resident cursor wraps within it. Make profile-driven
/// when a stage-count source lands.
const SCROLL_STAGE_COUNT: u8 = 3;

/// The next DPI for a [`Intent::DpiCycle`]: WITH `stages` it's the next stage's value (snap current to
/// the nearest stage, step, wrap); WITHOUT, a ±200 nudge so a stage-less setup still moves. Pure +
/// testable — the cycle's whole policy in one place.
fn next_dpi(cur: u16, step: i32, stages: &[u16]) -> u16 {
    if stages.is_empty() {
        return (cur as i32 + step * 200).clamp(100, 30_000) as u16;
    }
    let idx = stages
        .iter()
        .enumerate()
        .min_by_key(|(_, &s)| (s as i32 - cur as i32).abs())
        .map(|(i, _)| i)
        .unwrap_or(0);
    let n = stages.len() as i32;
    let next = (idx as i32 + step).rem_euclid(n) as usize;
    stages[next].clamp(100, 30_000)
}

/// Run the shared device/profile subset of [`Intent`].
///
/// Returns `None` for app/window/instrument intents that only `neuron-app` can satisfy.
pub fn run_shared_intent(
    devices: &mut DeviceSession<'_>,
    cursor: &mut impl ProfileCursor,
    intent: &Intent,
) -> Option<String> {
    use Intent::*;

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
            match devices.with_writable("set_dpi", |d| cap::set_dpi(d, dpi, dpi, cap::Store::Persist))
            {
                Ok(()) => {
                    // confirmation fires ONLY past the committed write (set is absolute — no prior
                    // read, so no old→new).
                    crate::confirm::dpi(dpi as u32, None);
                    format!("DPI -> {dpi}")
                }
                Err(e) => format!("DPI set failed: {e}"),
            }
        }
        DpiCycle(dir) => {
            // The CONTRACT is "cycle the active profile's DPI stages". Load them; with stages we land
            // on the next stage (current → nearest stage → step → wrap). With NO stages defined we
            // fall back to a ±200 nudge, so a stage-less setup still does something sensible.
            let stages: Vec<u16> = Profile::load(&cursor.active_profile())
                .map(|p| p.dpi_stages)
                .unwrap_or_default();
            let result = devices.with_writable("set_dpi", |d| {
                let cur = cap::dpi(d).map(|(x, _)| x)?;
                let next = next_dpi(cur, dir.step(), &stages);
                cap::set_dpi(d, next, next, cap::Store::Volatile)?;
                Ok((cur, next))
            });
            match result {
                Ok((cur, next)) => {
                    // the read-back gave us the prior DPI too — a true old→new confirmation.
                    crate::confirm::dpi(next as u32, Some(cur as u32));
                    format!("DPI cycle {} -> {next}", dir.label())
                }
                Err(e) => format!("DPI cycle skipped ({e})"),
            }
        }
        ScrollStageCycle(dir) => {
            // The device is set-only (no active-stage getter), so we step a RESIDENT cursor and WRITE
            // the wire-confirmed stage-select (0x15/0x00, ungated). No more "pending" — it cycles for
            // real. The device's first stage is 1; we wrap over `SCROLL_STAGE_COUNT`.
            let next = crate::writes::cycle_scroll_stage(
                crate::writes::scroll_stage_cursor(),
                dir.step(),
                SCROLL_STAGE_COUNT,
            );
            match devices.with_writable("set_scroll_stage", |d| {
                crate::writes::set_scroll_stage(d, next, cap::Store::Volatile)
            }) {
                Ok(()) => {
                    crate::writes::set_scroll_stage_cursor(next);
                    format!("scroll stage {} -> {next}", dir.label())
                }
                Err(e) => format!("scroll stage skipped ({e})"),
            }
        }
        ProfileSwitch(name) => match Profile::load(name) {
            Ok(p) => {
                let prev = cursor.active_profile();
                let rep = p.apply_with_session(devices);
                cursor.set_active_profile(name);
                // ONE confirmation for the action you took (switching profiles), not one per field
                // the profile applied — those are its consequence, not a separate act.
                crate::confirm::profile(name, Some(&prev));
                format!("profile -> {name}: {}", rep.summary())
            }
            Err(e) => format!("profile '{name}': {e}"),
        },
        ProfileCycle(dir) => {
            let names = profile::list();
            if names.is_empty() {
                return Some("profile cycle: none saved".into());
            }
            let idx = profile::cycle_index(&names, &cursor.active_profile(), dir.step());
            let name = names[idx].clone();
            match Profile::load(&name) {
                Ok(p) => {
                    let prev = cursor.active_profile();
                    let rep = p.apply_with_session(devices);
                    cursor.set_active_profile(&name);
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

        let result = run_shared_intent(&mut devices, &mut cursor, &Intent::DpiSet(800));

        crate::writes::set_writes_paused(saved);
        assert_eq!(result.as_deref(), Some("[writes paused]"));
    }
}
