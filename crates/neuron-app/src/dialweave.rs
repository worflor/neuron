// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! THE DIAL — an analog knob the eigenmotion stroke turns.
//!
//! Primed by [`Action::Dial`](neuron::action::Action::Dial), the next hold of the cast trigger
//! becomes a slide: the stroke's motion IS the value. Up (or right) raises it, down (or left)
//! lowers it, and SPEED is sensitivity — a fast sweep slams it across the range, a slow crawl
//! trims it a percent at a time. A straight line, either axis, or a lazy curve all work, because
//! the value integrates the stroke's dominant-axis velocity frame by frame (not its position).
//!
//! The maths live here so the beacon's weave loop just calls `begin` once and `step` per frame.

#![cfg(windows)]

use crate::beacon::audio_cache::short_device;
use neuron::action::DialTarget;
use neuron::audio::VolumeCtl;
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
struct ScrollTarget {
    hwnd: isize,
    pid: u32,
    tid: u32,
    point: (i32, i32),
    desktop: DesktopIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DesktopIdentity { units: [u16; 128], len: usize }

#[derive(Default)]
struct WheelState {
    remainder: f32,
}

impl WheelState {
    fn advance(&mut self, motion: f64, armed: bool, target_valid: bool) -> Option<i16> {
        if !armed || !target_valid || !motion.is_finite() {
            self.remainder = 0.0;
            return None;
        }
        self.remainder = (self.remainder + motion.clamp(-250.0, 250.0) as f32 * (WHEEL_DELTA / 50.0))
            .clamp(-WHEEL_DELTA * 2.0, WHEEL_DELTA * 2.0);
        let notches = (self.remainder / WHEEL_DELTA).trunc() as i16;
        if notches == 0 {
            return None;
        }
        let emit = notches.clamp(-MAX_NOTCHES_PER_FRAME, MAX_NOTCHES_PER_FRAME);
        self.remainder -= f32::from(emit) * WHEEL_DELTA;
        Some(emit * 120)
    }

    fn clear(&mut self) {
        self.remainder = 0.0;
    }
}

#[derive(Default)]
struct ScrollLatch {
    target: Option<ScrollTarget>,
    lost: bool,
}

impl ScrollLatch {
    fn begin(&mut self, target: Option<ScrollTarget>) {
        self.lost = target.is_none();
        self.target = target;
    }

    fn current(&mut self, anchored: bool, hover: Option<ScrollTarget>) -> Option<ScrollTarget> {
        if self.lost { return None; }
        if anchored {
            self.target.clone()
        } else {
            hover
        }
    }

    fn clear(&mut self) {
        self.target = None;
        self.lost = false;
    }

    fn invalidate(&mut self) {
        self.target = None;
        self.lost = true;
    }
}

const WHEEL_DELTA: f32 = 120.0;
const MAX_NOTCHES_PER_FRAME: i16 = 4;

fn finish_volume_receipt(
    receipt: &mut Option<neuron::session_undo::VolumeReceipt>,
    current: Option<f32>,
) -> Option<neuron::session_undo::Entry> {
    receipt.take()?.finish(current)
}

/// The live state of one slide.
pub struct Dial {
    pub target: DialTarget,
    ctl: Option<VolumeCtl>,
    receipt: Option<neuron::session_undo::VolumeReceipt>,
    /// the value being turned, 0..1
    pub value: f32,
    volume_read: bool,
    /// last stroke point (canvas-relative), for per-frame velocity
    last: Option<(f64, f64)>,
    /// when the last point arrived — the dt basis for the smoothing (points do not always land
    /// on the 16ms tick, and surrender at the end of a slide is even slower)
    last_at: Option<Instant>,
    /// smoothed turn-speed 0..1 — drives the gauge's pulse (coarse vs fine reads on the glass)
    pub speed: f32,
    /// the resolved endpoint's friendly name ("Headset"), shown in the gauge hub
    pub device: String,
    scroll_latch: ScrollLatch,
    wheel: WheelState,
    scroll_direction: i8,
}

impl Default for Dial {
    fn default() -> Self {
        Dial {
            target: DialTarget::OutputVolume,
            ctl: None,
            receipt: None,
            value: 0.0,
            volume_read: false,
            last: None,
            last_at: None,
            speed: 0.0,
            device: String::new(),
            scroll_latch: ScrollLatch::default(),
            wheel: WheelState::default(),
            scroll_direction: 0,
        }
    }
}

impl Dial {
    /// Begin a slide for `target`: resolve + open its endpoint and read where it sits now, so the
    /// stroke nudges FROM the real current value (not a jump). Captures the device name for the hub.
    pub fn begin(&mut self, target: DialTarget) {
        self.finish_volume_receipt();
        self.target = target;
        self.scroll_latch.clear();
        self.wheel.clear();
        self.scroll_direction = 0;
        let (flow, ep) = match target {
            DialTarget::OutputVolume => (Some(neuron::audio::Flow::Render), neuron::audio::resolve_render(None)),
            DialTarget::MicVolume => (Some(neuron::audio::Flow::Capture), neuron::audio::resolve_capture(None)),
            DialTarget::ScrollHover | DialTarget::ScrollAnchored => (None, None),
        };
        self.device = ep
            .as_ref()
            .map(|e| short_device(&e.name))
            .unwrap_or_default();
        self.ctl = ep.as_ref().and_then(|e| VolumeCtl::open(&e.id));
        self.receipt = None;
        self.volume_read = false;
        if let (Some(ep), Some(flow), Some(ctl)) = (ep, flow, self.ctl.as_ref()) {
            if let Some(before) = ctl.try_get_volume() {
                self.receipt = neuron::session_undo::VolumeReceipt::new(ep.id, flow, before);
                if self.receipt.is_some() {
                    self.value = before;
                    self.volume_read = true;
                } else {
                    self.value = 0.0;
                }
            } else {
                self.value = 0.0;
            }
        } else {
            self.value = 0.0;
        }
        if matches!(target, DialTarget::ScrollAnchored) {
            self.scroll_latch.begin(capture_scroll_target());
        }
        self.last = None;
        self.last_at = None;
        self.speed = 0.0;
    }

    /// Is this the mic dial (vs output)? — picks the gauge's target icon.
    pub fn is_mic(&self) -> bool {
        matches!(self.target, DialTarget::MicVolume)
    }

    pub fn is_output(&self) -> bool {
        matches!(self.target, DialTarget::OutputVolume)
    }

    pub fn finish(&mut self) {
        self.finish_volume_receipt();
        self.scroll_latch.clear();
        self.wheel.clear();
        self.last = None;
        self.last_at = None;
    }

    fn finish_volume_receipt(&mut self) {
        let current = self.ctl.as_ref().and_then(VolumeCtl::try_get_volume);
        if let Some(entry) = finish_volume_receipt(&mut self.receipt, current) {
            neuron::session_undo::push(entry);
        }
    }

    /// Is the turned endpoint muted right now? — rings the gauge red.
    pub fn muted(&self) -> bool {
        self.ctl.as_ref().is_some_and(neuron::audio::VolumeCtl::get_mute)
    }

    /// Integrate a fresh stroke point into the value, apply it live, and return the overlay's
    /// `(label, fill, glow)` — the gauge's reading, height, and pulse.
    pub fn step(&mut self, pt: (f64, f64)) -> (String, f32, f32) {
        // a per-point dt, with the 60Hz capture tick as the reference unit: a coalesced burst or a
        // slow drain reads the same as a steady stream, so the old constant-per-point terms (which
        // silently assumed ~16ms spacing) stop calibrating the tuning to the machine's timing.
        let now = Instant::now();
        let dt = self
            .last_at
            .map_or(1.0 / 60.0, |a| now.duration_since(a).as_secs_f32().clamp(0.0, 0.25));
        self.last_at = Some(now);
        let dtn = dt * 60.0; // 1 at the reference tick
        if let Some(prev) = self.last {
            let (dx, dy) = (pt.0 - prev.0, pt.1 - prev.1);
            // the DOMINANT axis this frame: up / right = more, down / left = less. A curve
            // transitions smoothly between the two as the hand turns.
            let raw = if matches!(self.target, DialTarget::ScrollHover | DialTarget::ScrollAnchored) || dy.abs() >= dx.abs() {
                -dy
            } else {
                dx
            };
            let mag = raw.abs();
            // super-linear: the linear term gives fine control at a crawl; the squared term
            // accelerates a fast sweep into a slam. `accel` is scaled by the elapsed dt so the
            // tuning is frame-rate-independent, then hard-capped per event so one coalesced burst
            // (a big dt) can't teleport the value — the cap must come AFTER the scale to do that.
            let accel = ((mag * 0.0011 + mag * mag * 0.00013) * f64::from(dtn)).min(0.16);
            if matches!(self.target, DialTarget::ScrollHover | DialTarget::ScrollAnchored) {
                self.scroll(raw);
            } else if self.volume_read {
                let desired = (self.value + (raw.signum() * accel) as f32).clamp(0.0, 1.0);
                if let (Some(ctl), Some(receipt)) = (&self.ctl, &mut self.receipt) {
                    if let Some(applied) = neuron::session_undo::apply_verified_volume_step(
                        receipt,
                        desired,
                        neuron::safety::input_armed,
                        |value| ctl.set_volume(value),
                        || ctl.try_get_volume(),
                    ) {
                        self.value = applied;
                    } else if neuron::safety::input_armed() && ctl.try_get_volume().is_none() {
                        self.volume_read = false;
                    }
                }
            }
            let target = (mag as f32 * 0.02).min(1.0);
            let keep = (0.7f32).powf(60.0 * dt); // exponential smoothing, normalized to 60Hz ticks
            self.speed = self.speed * keep + target * (1.0 - keep);
        } else {
            self.speed *= (0.85f32).powf(60.0 * dt); // pulse decay, same 60Hz normalization as above
        }
        self.last = Some(pt);
        self.reading()
    }

    /// The reading without a step (for the first frame / commit status): `(value, fill, glow)`
    /// where `value` is just the percent ("62%") — the target icon + device name ride alongside it.
    pub fn reading(&self) -> (String, f32, f32) {
        if matches!(self.target, DialTarget::ScrollHover | DialTarget::ScrollAnchored) {
            let label = match self.scroll_direction {
                d if d > 0 => "scroll ↑",
                d if d < 0 => "scroll ↓",
                _ => "scroll",
            };
            return (label.into(), 0.0, self.speed.clamp(0.0, 1.0));
        }
        if !self.volume_read { return ("—".into(), 0.0, self.speed.clamp(0.0, 1.0)); }
        let pct = (self.value * 100.0).round() as i32;
        (format!("{pct}%"), self.value, self.speed.clamp(0.0, 1.0))
    }

    fn scroll(&mut self, vertical_motion: f64) {
        if !vertical_motion.is_finite() || !neuron::safety::input_armed() {
            self.wheel.clear();
            return;
        }
        let fresh = match self.target {
            DialTarget::ScrollHover => cursor_scroll_target(),
            _ => None,
        };
        let anchored = matches!(self.target, DialTarget::ScrollAnchored);
        let Some((target, delta)) = plan_scroll(
            &mut self.scroll_latch,
            &mut self.wheel,
            anchored,
            fresh,
            vertical_motion,
            neuron::safety::input_armed(),
            scroll_target_is_current,
        ) else {
            return;
        };
        self.scroll_direction = delta.signum() as i8;
        if !send_wheel(target, delta) {
            self.wheel.clear();
        }
    }
}

impl Drop for Dial {
    fn drop(&mut self) {
        self.finish_volume_receipt();
        self.scroll_latch.clear();
        self.wheel.clear();
    }
}

fn capture_scroll_target() -> Option<ScrollTarget> {
    cursor_scroll_target()
}

fn cursor_scroll_target() -> Option<ScrollTarget> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowLongPtrW, GetWindowThreadProcessId, WindowFromPoint, GWL_EXSTYLE, WS_EX_TRANSPARENT};
    // SAFETY: all Win32 calls use valid output storage; the sampled HWND is only queried and never destroyed here.
    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        // SAFETY: `point` is a writable, correctly sized Win32 POINT for GetCursorPos.
        if GetCursorPos(&raw mut point) == 0 {
            return None;
        }
        // SAFETY: WindowFromPoint accepts the value snapshot and returns a borrowed HWND; no cursor movement occurs.
        let hwnd = WindowFromPoint(point);
        if hwnd.is_null() {
            return None;
        }
        if !wheel_point_fits_lparam(point.x, point.y) { return None; }
        let mut pid = 0;
        // SAFETY: hwnd came from WindowFromPoint and pid is a writable output.
        let tid = GetWindowThreadProcessId(hwnd, &raw mut pid);
        if tid == 0 || pid == 0 { return None; }
        // Exclude a transparent Neuron overlay if a Windows configuration still returns it.
        if pid == std::process::id() && (GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TRANSPARENT) != 0 { return None; }
        let (current_desktop, input_desktop) = desktop_identity()?;
        if current_desktop != input_desktop { return None; }
        Some(ScrollTarget { hwnd: hwnd as isize, pid, tid, point: (point.x, point.y), desktop: input_desktop })
    }
}

fn scroll_target_is_current(target: &ScrollTarget) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    // SAFETY: validation only queries the saved HWND, process identity, and interactive desktop.
    unsafe {
        // SAFETY: target.hwnd is an OS handle sampled by WindowFromPoint; IsWindow only validates it.
        if IsWindow(target.hwnd as _) == 0 {
            return false;
        }
        let mut pid = 0;
        // SAFETY: the HWND was checked above and pid is a writable output; this rejects handle reuse.
        let tid = GetWindowThreadProcessId(target.hwnd as _, &raw mut pid);
        tid == target.tid && pid == target.pid && desktop_identity().is_some_and(|(a, b)| a == b && b == target.desktop)
    }
}

fn desktop_identity() -> Option<(DesktopIdentity, DesktopIdentity)> {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME, DESKTOP_READOBJECTS};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    unsafe fn name(handle: windows_sys::Win32::System::StationsAndDesktops::HDESK) -> Option<DesktopIdentity> {
        let mut buf = [0u16; 128];
        let mut needed = 0u32;
        // SAFETY: buf is a bounded writable UTF-16 buffer and the desktop handle is owned by the caller.
        if unsafe { GetUserObjectInformationW(handle, UOI_NAME, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf) as u32, &raw mut needed) } == 0 {
            return None;
        }
        let len = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
        Some(DesktopIdentity { units: buf, len })
    }
    // SAFETY: retrieves borrowed current-thread desktop and opens/closes one read-only input desktop handle.
    unsafe {
        // SAFETY: GetThreadDesktop is read-only and receives this live thread's id.
        let current = GetThreadDesktop(GetCurrentThreadId());
        // SAFETY: request read-only access to the interactive input desktop; never switch desktops.
        let input = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if current.is_null() || input.is_null() {
            if !input.is_null() { CloseDesktop(input); }
            return None;
        }
        let result = name(current).zip(name(input));
        // SAFETY: input is the owned handle returned by OpenInputDesktop; current is borrowed.
        CloseDesktop(input);
        result
    }
}

fn send_wheel(target: ScrollTarget, delta: i16) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_MOUSEWHEEL};
    let wparam = (delta as u16 as usize) << 16;
    if !wheel_point_fits_lparam(target.point.0, target.point.1) { return false; }
    let lparam = (target.point.0 as i16 as u16 as usize) | ((target.point.1 as i16 as u16 as usize) << 16);
    let mut result = 0usize;
    // SAFETY: target identity/desktop were just revalidated; message is bounded and does not move the cursor.
    unsafe { SendMessageTimeoutW(target.hwnd as _, WM_MOUSEWHEEL, wparam, lparam as isize, SMTO_ABORTIFHUNG, 100, &raw mut result) != 0 }
}

fn wheel_point_fits_lparam(x: i32, y: i32) -> bool {
    (i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&x) && (i32::from(i16::MIN)..=i32::from(i16::MAX)).contains(&y)
}

fn plan_scroll(
    latch: &mut ScrollLatch,
    wheel: &mut WheelState,
    anchored: bool,
    hover: Option<ScrollTarget>,
    motion: f64,
    armed: bool,
    is_current: impl FnOnce(&ScrollTarget) -> bool,
) -> Option<(ScrollTarget, i16)> {
    if !armed {
        wheel.clear();
        return None;
    }
    let Some(target) = latch.current(anchored, hover) else {
        wheel.clear();
        return None;
    };
    if !is_current(&target) {
        if anchored { latch.invalidate(); }
        wheel.clear();
        return None;
    }
    wheel.advance(motion, true, true).map(|delta| (target, delta))
}

/// Map the dial code stored at prime time back to a target.
pub fn target_from_code(code: u8) -> DialTarget {
    match code {
        1 => DialTarget::MicVolume,
        2 => DialTarget::ScrollHover,
        3 => DialTarget::ScrollAnchored,
        _ => DialTarget::OutputVolume,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finishing_a_dial_receipt_records_once_and_stale_end_state_records_nothing() {
        let mut receipt = Some(neuron::session_undo::VolumeReceipt::new("id".into(), neuron::audio::Flow::Capture, 0.2).unwrap());
        receipt.as_mut().unwrap().verified_applied(0.6);
        assert!(finish_volume_receipt(&mut receipt, Some(0.6)).is_some());
        assert!(finish_volume_receipt(&mut receipt, Some(0.6)).is_none(), "release plus Drop cannot record twice");

        let mut stale = Some(neuron::session_undo::VolumeReceipt::new("id".into(), neuron::audio::Flow::Capture, 0.2).unwrap());
        stale.as_mut().unwrap().verified_applied(0.6);
        assert!(finish_volume_receipt(&mut stale, Some(0.8)).is_none());
    }

    fn target(point: (i32, i32)) -> ScrollTarget {
        ScrollTarget { hwnd: 1, pid: 2, tid: 3, point, desktop: DesktopIdentity { units: [0; 128], len: 0 } }
    }

    #[test]
    fn wheel_accumulates_fractional_units_without_an_initial_step() {
        let mut wheel = WheelState::default();
        let mut latch = ScrollLatch::default();
        latch.begin(Some(target((-120, 240))));
        let mut fake_sink = Vec::new();
        for motion in [10.0, 10.0, 30.0] {
            if let Some((target, delta)) = plan_scroll(&mut latch, &mut wheel, true, None, motion, true, |_| true) {
                fake_sink.push((target.point, delta));
            }
        }
        assert_eq!(fake_sink, vec![((-120, 240), 120)]);
        latch.clear();
        wheel.clear();
        latch.begin(Some(target((-120, 240))));
        assert!(plan_scroll(&mut latch, &mut wheel, true, None, 1.0, true, |_| true).is_none(), "the next hold starts with a cleared accumulator");
    }

    #[test]
    fn wheel_is_bounded_and_clears_on_cancel_invalid_input_or_missing_target() {
        let mut wheel = WheelState::default();
        assert_eq!(wheel.advance(f64::MAX, true, true), Some(240));
        assert!(wheel.remainder.abs() <= WHEEL_DELTA * 2.0);
        assert_eq!(wheel.advance(f64::NAN, true, true), None);
        assert_eq!(wheel.remainder, 0.0);
        assert_eq!(wheel.advance(100.0, false, true), None);
        assert_eq!(wheel.remainder, 0.0);
        assert_eq!(wheel.advance(100.0, true, false), None);
        assert_eq!(wheel.remainder, 0.0);
    }

    #[test]
    fn anchored_target_stays_latched_at_its_original_point_until_cancelled() {
        let saved = target((-120, 240));
        let mut latch = ScrollLatch::default();
        latch.begin(Some(saved.clone()));
        assert_eq!(latch.current(true, Some(target((30, 40)))), Some(saved));
        latch.clear();
        assert_eq!(latch.current(true, None), None);
    }

    #[test]
    fn hover_target_tracks_the_current_pane_and_lost_anchored_target_stays_lost() {
        let mut hover = ScrollLatch::default();
        assert_eq!(hover.current(false, Some(target((-1, -2)))), Some(target((-1, -2))));
        let mut anchored = ScrollLatch::default();
        anchored.begin(Some(target((1, 2))));
        anchored.lost = true;
        assert_eq!(anchored.current(true, Some(target((3, 4)))), None);
    }

    #[test]
    fn disarmed_or_destroyed_targets_clear_residuals_without_sink_output() {
        let mut latch = ScrollLatch::default();
        latch.begin(Some(target((2, 3))));
        let mut wheel = WheelState::default();
        wheel.remainder = 90.0;
        let mut fake_sink = Vec::new();
        let sent = plan_scroll(&mut latch, &mut wheel, true, None, 50.0, true, |_| false)
            .map(|(target, delta)| fake_sink.push((target.point, delta)));
        assert_eq!(sent, None);
        assert!(fake_sink.is_empty(), "a destroyed or identity-changed window is never sent a wheel message");
        assert_eq!(wheel.remainder, 0.0);
        assert!(plan_scroll(&mut latch, &mut wheel, true, None, 50.0, true, |_| true).is_none());

        latch.begin(Some(target((2, 3))));
        wheel.remainder = 90.0;
        assert!(plan_scroll(&mut latch, &mut wheel, true, None, 50.0, false, |_| true).is_none());
        assert_eq!(wheel.remainder, 0.0);
    }

    #[test]
    fn wheel_message_coordinates_must_fit_signed_win32_words() {
        assert!(wheel_point_fits_lparam(-32768, 32767));
        assert!(!wheel_point_fits_lparam(32768, 0));
        assert!(!wheel_point_fits_lparam(0, -32769));
    }
}
