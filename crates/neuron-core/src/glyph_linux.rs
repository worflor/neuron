// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Linux gesture capture: the same portable phrase watcher and device-aware held registry as the
//! Windows path, fed relative evdev motion from the one resident controls listener.

use super::{C, CaptureSlot};
use crate::controls::ControlRef;
use crate::feel::{FeelConfig, Phrase, PhraseWatcher, Watch};
use crate::linux_input::{self, Motion};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

fn drain(
    rx: &Receiver<Motion>, acc: &mut (f64, f64), pts: &mut Vec<C>,
    stamps: &mut Vec<u32>, max_pts: usize,
) -> bool {
    let mut changed = false;
    for motion in rx.try_iter() {
        if motion.dx == 0 && motion.dy == 0 { continue; }
        acc.0 += f64::from(motion.dx);
        acc.1 += f64::from(motion.dy);
        pts.push(C::new(acc.0, acc.1));
        stamps.push(motion.at_ms);
        if pts.len() >= max_pts.max(2) {
            let mut keep = 0;
            for index in (0..pts.len()).step_by(2) {
                pts[keep] = pts[index];
                stamps[keep] = stamps[index];
                keep += 1;
            }
            if !(pts.len() - 1).is_multiple_of(2) {
                pts[keep] = *pts.last().expect("nonempty path");
                stamps[keep] = *stamps.last().expect("nonempty stamps");
                keep += 1;
            }
            pts.truncate(keep);
            stamps.truncate(keep);
        }
        changed = true;
    }
    changed
}

fn cancelled(stop: &(impl Fn() -> bool + ?Sized)) -> bool {
    stop() || super::key_down(crate::capture::VK_ESCAPE)
}

pub fn capture_held(trigger: ControlRef, max_pts: usize) -> Vec<C> {
    capture_phrase(trigger, &Phrase::hold(), &FeelConfig::default(), max_pts, |_| {})
}

pub fn capture_held_with(trigger: ControlRef, max_pts: usize, on_progress: impl FnMut(&[C])) -> Vec<C> {
    capture_phrase(trigger, &Phrase::hold(), &FeelConfig::default(), max_pts, on_progress)
}

pub fn capture_phrase(
    trigger: ControlRef, phrase: &Phrase, cfg: &FeelConfig, max_pts: usize,
    on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    capture_phrase_until(trigger, phrase, cfg, max_pts, &|| false, on_progress)
}

pub fn capture_phrase_until(
    trigger: ControlRef, phrase: &Phrase, cfg: &FeelConfig, max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized), on_progress: impl FnMut(&[C]),
) -> Vec<C> {
    capture_phrase_inner(trigger, phrase, cfg, max_pts, stop, false, on_progress).0
}

pub fn capture_phrase_until_stamped(
    trigger: ControlRef, phrase: &Phrase, cfg: &FeelConfig, max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized), on_progress: impl FnMut(&[C]),
) -> (Vec<C>, Vec<u32>) {
    capture_phrase_inner(trigger, phrase, cfg, max_pts, stop, true, on_progress)
}

fn capture_phrase_inner(
    trigger: ControlRef, phrase: &Phrase, cfg: &FeelConfig, max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized), stamped: bool, mut on_progress: impl FnMut(&[C]),
) -> (Vec<C>, Vec<u32>) {
    let rx = linux_input::observe_motion();
    let mut watcher = PhraseWatcher::new(phrase.clone(), cfg);
    let started = Instant::now();
    let mut scratch = Vec::new();
    let mut scratch_stamps = Vec::new();
    let mut acc = (0.0, 0.0);
    let toggle = loop {
        let _ = drain(&rx, &mut acc, &mut scratch, &mut scratch_stamps, 256);
        scratch.clear();
        scratch_stamps.clear();
        if cancelled(stop) { return (Vec::new(), Vec::new()); }
        match watcher.feed(started.elapsed().as_millis() as u64, super::control_down(trigger)) {
            Watch::Activated { toggle } => break toggle,
            Watch::Pending | Watch::Reset => {}
        }
        std::thread::sleep(Duration::from_millis(3));
    };
    on_progress(&[]);
    acc = (0.0, 0.0);
    let mut pts = Vec::new();
    let mut stamps = Vec::new();
    let capture_start = Instant::now();
    let mut last_motion = Instant::now();
    if toggle {
        while super::control_down(trigger) {
            if cancelled(stop) { return (Vec::new(), Vec::new()); }
            if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) {
                last_motion = Instant::now();
                on_progress(&pts);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        loop {
            if cancelled(stop) { return (Vec::new(), Vec::new()); }
            if super::control_down(trigger) { break; }
            let timed_out = if stamped { last_motion.elapsed() > Duration::from_secs(30) }
                else { capture_start.elapsed() > Duration::from_secs(60) };
            if timed_out {
                if stamped { break; }
                return (Vec::new(), Vec::new());
            }
            if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) {
                last_motion = Instant::now();
                on_progress(&pts);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        while super::control_down(trigger) {
            if cancelled(stop) { break; }
            std::thread::sleep(Duration::from_millis(2));
        }
    } else {
        while super::control_down(trigger) {
            if cancelled(stop) { return (Vec::new(), Vec::new()); }
            let timed_out = if stamped { last_motion.elapsed() > Duration::from_secs(30) }
                else { capture_start.elapsed() > Duration::from_secs(30) };
            if timed_out {
                if stamped { break; }
                return (Vec::new(), Vec::new());
            }
            if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) {
                last_motion = Instant::now();
                on_progress(&pts);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let tail_end = Instant::now() + Duration::from_millis(cfg.coyote_ms);
        while Instant::now() < tail_end && !super::control_down(trigger) {
            if cancelled(stop) { return (Vec::new(), Vec::new()); }
            if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) { on_progress(&pts); }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    if !stamped { stamps.clear(); }
    (pts, stamps)
}

pub fn capture_slots_until(
    slots: &[CaptureSlot], cfg: &FeelConfig, max_pts: usize,
    stop: &(impl Fn() -> bool + ?Sized), mut on_activated: impl FnMut(u32),
    mut on_progress: impl FnMut(&[C]),
) -> Option<(u32, Vec<C>)> {
    if slots.is_empty() { return None; }
    struct KeyState {
        ctl: ControlRef, down: bool, down_at: Instant, last_release: Instant,
        taps: u8, dead: bool,
    }
    let rx = linux_input::observe_motion();
    let started = Instant::now();
    let mut keys: Vec<KeyState> = Vec::new();
    for slot in slots {
        if !keys.iter().any(|key| key.ctl == slot.ctl) {
            keys.push(KeyState {
                ctl: slot.ctl, down: false, down_at: started,
                last_release: started, taps: 0, dead: false,
            });
        }
    }
    let mut acc = (0.0, 0.0);
    let mut pre = Vec::new();
    let mut pre_stamps = Vec::new();
    let (id, mut pts, mut stamps) = 'wait: loop {
        let _ = drain(&rx, &mut acc, &mut pre, &mut pre_stamps, 256);
        if cancelled(stop) { return None; }
        let now = Instant::now();
        for key in &mut keys {
            let down = super::control_down(key.ctl);
            if down && !key.down {
                if now.duration_since(key.last_release).as_millis() as u64 > cfg.gap_ms { key.taps = 0; }
                key.down = true;
                key.dead = false;
                key.down_at = now;
                acc = (0.0, 0.0);
                pre.clear();
                pre_stamps.clear();
            } else if !down && key.down {
                key.down = false;
                let held = now.duration_since(key.down_at).as_millis() as u64;
                if key.dead || held >= cfg.hold_ms { key.taps = 0; }
                else {
                    key.taps = key.taps.saturating_add(1);
                    let max_taps = slots.iter().filter(|slot| slot.ctl == key.ctl)
                        .map(|slot| slot.taps).max().unwrap_or(0);
                    if key.taps > max_taps { key.taps = 0; }
                }
                key.dead = false;
                key.last_release = now;
            } else if !down && key.taps > 0
                && now.duration_since(key.last_release).as_millis() as u64 > cfg.gap_ms {
                key.taps = 0;
            }
            if key.down && !key.dead {
                let held = now.duration_since(key.down_at).as_millis() as u64;
                let moved = match (pre.first(), pre.last()) {
                    (Some(first), Some(last)) => {
                        let dx: f64 = last.re - first.re;
                        let dy: f64 = last.im - first.im;
                        dx.hypot(dy) >= 8.0
                    }
                    _ => false,
                };
                if held >= cfg.hold_ms || moved {
                    match slots.iter().find(|slot| slot.ctl == key.ctl && slot.taps == key.taps) {
                        Some(slot) => break 'wait (slot.id, std::mem::take(&mut pre), std::mem::take(&mut pre_stamps)),
                        None => key.dead = true,
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(3));
    };
    let ctl = slots.iter().find(|slot| slot.id == id)?.ctl;
    on_activated(id);
    on_progress(&pts);
    let hold_start = Instant::now();
    while super::control_down(ctl) {
        if cancelled(stop) || hold_start.elapsed() > Duration::from_secs(30) { return None; }
        if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) { on_progress(&pts); }
        std::thread::sleep(Duration::from_millis(2));
    }
    let tail_end = Instant::now() + Duration::from_millis(cfg.coyote_ms);
    while Instant::now() < tail_end && !super::control_down(ctl) {
        if cancelled(stop) { return None; }
        if drain(&rx, &mut acc, &mut pts, &mut stamps, max_pts) { on_progress(&pts); }
        std::thread::sleep(Duration::from_millis(2));
    }
    Some((id, pts))
}
