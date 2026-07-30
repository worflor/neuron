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

use neuron::action::DialTarget;
use neuron::audio::VolumeCtl;

/// The live state of one slide.
pub struct Dial {
    pub target: DialTarget,
    ctl: Option<VolumeCtl>,
    /// the value being turned, 0..1
    pub value: f32,
    /// last stroke point (canvas-relative), for per-frame velocity
    last: Option<(f64, f64)>,
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
        if let Some(prev) = self.last {
            let (dx, dy) = (pt.0 - prev.0, pt.1 - prev.1);
            // the DOMINANT axis this frame: up / right = more, down / left = less. A curve
            // transitions smoothly between the two as the hand turns.
            let raw = if dy.abs() >= dx.abs() { -dy } else { dx };
            let mag = raw.abs();
            // super-linear: the linear term gives fine control at a crawl; the squared term
            // accelerates a fast sweep into a slam. Per-frame change is capped so one coalesced
            // burst can't teleport the value.
            let accel = (mag * 0.0011 + mag * mag * 0.00013).min(0.16);
            self.value = (self.value + (raw.signum() * accel) as f32).clamp(0.0, 1.0);
            if let Some(c) = &self.ctl {
                c.set_volume(self.value);
            }
            self.speed = self.speed * 0.7 + ((mag as f32) * 0.02).min(1.0) * 0.3;
        } else {
            self.speed *= 0.85; // decay the pulse while the hand is still
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

/// Trim a Windows endpoint name to the identifying part: the hardware in parentheses if present
/// ("Headset Earphone (Razer BlackShark V2)" → "Razer BlackShark V2"), else the name, capped.
pub(crate) fn short_device(name: &str) -> String {
    let core = match (name.find('('), name.rfind(')')) {
        (Some(a), Some(b)) if b > a + 1 => name[a + 1..b].trim(),
        _ => name.trim(),
    };
    let core = if core.is_empty() { name.trim() } else { core };
    if core.chars().count() > 22 {
        core.chars().take(21).collect::<String>() + "\u{2026}"
    } else {
        core.to_string()
    }
}
