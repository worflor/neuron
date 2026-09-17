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

/// The live state of one slide.
pub struct Dial {
    pub target: DialTarget,
    ctl: Option<VolumeCtl>,
    /// the value being turned, 0..1
    pub value: f32,
    /// last stroke point (canvas-relative), for per-frame velocity
    last: Option<(f64, f64)>,
    /// when the last point arrived — the dt basis for the smoothing (points do not always land
    /// on the 16ms tick, and surrender at the end of a slide is even slower)
    last_at: Option<Instant>,
    /// smoothed turn-speed 0..1 — drives the gauge's pulse (coarse vs fine reads on the glass)
    pub speed: f32,
    /// the resolved endpoint's friendly name ("Headset"), shown in the gauge hub
    pub device: String,
}

impl Default for Dial {
    fn default() -> Self {
        Dial {
            target: DialTarget::OutputVolume,
            ctl: None,
            value: 0.5,
            last: None,
            last_at: None,
            speed: 0.0,
            device: String::new(),
        }
    }
}

impl Dial {
    /// Begin a slide for `target`: resolve + open its endpoint and read where it sits now, so the
    /// stroke nudges FROM the real current value (not a jump). Captures the device name for the hub.
    pub fn begin(&mut self, target: DialTarget) {
        self.target = target;
        let ep = match target {
            DialTarget::OutputVolume => neuron::audio::resolve_render(None),
            DialTarget::MicVolume => neuron::audio::resolve_capture(None),
        };
        self.device = ep
            .as_ref()
            .map(|e| short_device(&e.name))
            .unwrap_or_default();
        self.ctl = ep.and_then(|e| VolumeCtl::open(&e.id));
        self.value = self.ctl.as_ref().map(|c| c.get_volume()).unwrap_or(0.5);
        self.last = None;
        self.last_at = None;
        self.speed = 0.0;
    }

    /// Is this the mic dial (vs output)? — picks the gauge's target icon.
    pub fn is_mic(&self) -> bool {
        matches!(self.target, DialTarget::MicVolume)
    }

    /// Is the turned endpoint muted right now? — rings the gauge red.
    pub fn muted(&self) -> bool {
        self.ctl.as_ref().map(|c| c.get_mute()).unwrap_or(false)
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
            .map(|a| now.duration_since(a).as_secs_f32().clamp(0.0, 0.25))
            .unwrap_or(1.0 / 60.0);
        self.last_at = Some(now);
        let dtn = dt * 60.0; // 1 at the reference tick
        if let Some(prev) = self.last {
            let (dx, dy) = (pt.0 - prev.0, pt.1 - prev.1);
            // the DOMINANT axis this frame: up / right = more, down / left = less. A curve
            // transitions smoothly between the two as the hand turns.
            let raw = if dy.abs() >= dx.abs() { -dy } else { dx };
            let mag = raw.abs();
            // super-linear: the linear term gives fine control at a crawl; the squared term
            // accelerates a fast sweep into a slam. `accel` is scaled by the elapsed dt so the
            // tuning is frame-rate-independent, then hard-capped per event so one coalesced burst
            // (a big dt) can't teleport the value — the cap must come AFTER the scale to do that.
            let accel = ((mag * 0.0011 + mag * mag * 0.00013) * dtn as f64).min(0.16);
            self.value = (self.value + (raw.signum() * accel) as f32).clamp(0.0, 1.0);
            if let Some(c) = &self.ctl {
                c.set_volume(self.value);
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
        let pct = (self.value * 100.0).round() as i32;
        (format!("{pct}%"), self.value, self.speed.clamp(0.0, 1.0))
    }
}

/// Map the dial code stored at prime time back to a target.
pub fn target_from_code(code: u8) -> DialTarget {
    match code {
        1 => DialTarget::MicVolume,
        _ => DialTarget::OutputVolume,
    }
}
