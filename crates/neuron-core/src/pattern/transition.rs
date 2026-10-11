// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Finite base-scene dissolves; ordinary animation is never an authoring transition.

use std::sync::Arc;
use std::time::Duration;
use crate::lighting::Rgb;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TransitionHint {
    #[default]
    Edit,
    Switch,
    Immediate,
}

impl TransitionHint {
    #[must_use]
    pub fn duration(self) -> Duration {
        match self {
            Self::Edit => Duration::from_millis(120),
            Self::Switch => Duration::from_millis(350),
            Self::Immediate => Duration::ZERO,
        }
    }
}

struct Dissolve {
    source: Vec<[f32; 3]>,
    bytes: Arc<[Rgb]>,
    started: Duration,
    hint: TransitionHint,
}

/// One frozen source and a moving destination, with floating continuation across interruptions.
#[derive(Default)]
pub struct FrameTransition {
    active: Option<Dissolve>,
    working: Vec<[f32; 3]>,
    working_at: Option<Duration>,
    last_time: Duration,
}

impl FrameTransition {
    #[must_use]
    pub fn active(&self) -> bool { self.active.is_some() }

    /// Capture only a picture already emitted at this boundary; never advance a generator here.
    pub fn arm(&mut self, emitted: Arc<[Rgb]>, now: Duration, hint: TransitionHint) {
        let now = now.max(self.last_time);
        self.last_time = now;
        if hint == TransitionHint::Immediate {
            self.active = None;
            self.working_at = None;
            return;
        }
        let source = if self.working_at == Some(now) && self.working.len() == emitted.len() {
            self.working.clone()
        } else {
            emitted.iter().map(|c| c.to_linear()).collect()
        };
        self.active = Some(Dissolve { source, bytes: emitted, started: now, hint });
    }

    /// Resolve one normal destination frame. Exact endpoints bypass colour conversion.
    #[must_use]
    pub fn resolve(&mut self, destination: &[Rgb], now: Duration) -> Arc<[Rgb]> {
        let now = now.max(self.last_time);
        self.last_time = now;
        let Some(fade) = &self.active else {
            self.working_at = None;
            return destination.into();
        };
        let elapsed = now.saturating_sub(fade.started);
        if fade.source.len() != destination.len() || elapsed >= fade.hint.duration() {
            self.active = None;
            self.working_at = None;
            self.working.clear();
            return destination.into();
        }
        if elapsed.is_zero() {
            self.working.clone_from(&fade.source);
            self.working_at = Some(now);
            return fade.bytes.clone();
        }
        let u = (elapsed.as_secs_f64() / fade.hint.duration().as_secs_f64()) as f32;
        let weight = match fade.hint {
            TransitionHint::Switch => u * u * (3.0 - 2.0 * u),
            TransitionHint::Edit | TransitionHint::Immediate => u,
        };
        self.working.resize(destination.len(), [0.0; 3]);
        let pixels: Arc<[Rgb]> = self.working.iter_mut().zip(&fade.source).zip(destination).map(|((work, source), dest)| {
            let dest = dest.to_linear();
            for channel in 0..3 { work[channel] = source[channel] + (dest[channel] - source[channel]) * weight; }
            Rgb::from_linear(*work)
        }).collect();
        self.working_at = Some(now);
        pixels
    }

    /// Producer-owned continuation memory; published frames retained by observers are separate.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.working.capacity() * std::mem::size_of::<[f32; 3]>()
            + self.active.as_ref().map_or(0, |f| f.source.capacity() * std::mem::size_of::<[f32; 3]>() + std::mem::size_of_val(f.bytes.as_ref()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn picture(c: Rgb) -> Arc<[Rgb]> { vec![c].into() }

    #[test]
    fn linear_light_midpoint_and_exact_endpoints() {
        let mut fade = FrameTransition::default();
        fade.arm(picture(Rgb::BLACK), Duration::ZERO, TransitionHint::Switch);
        assert_eq!(fade.resolve(&[Rgb::new(255, 255, 255)], Duration::ZERO)[0], Rgb::BLACK);
        assert_eq!(fade.resolve(&[Rgb::new(255, 255, 255)], Duration::from_millis(175))[0], Rgb::new(188, 188, 188));
        assert_eq!(fade.resolve(&[Rgb::new(255, 255, 255)], Duration::from_millis(350))[0], Rgb::new(255, 255, 255));
        assert!(!fade.active());
    }

    #[test]
    fn interruption_uses_the_emitted_picture_and_floating_continuation() {
        let mut fade = FrameTransition::default();
        let red = Rgb::new(255, 0, 0);
        let blue = Rgb::new(0, 0, 255);
        let green = Rgb::new(0, 255, 0);
        fade.arm(picture(red), Duration::ZERO, TransitionHint::Edit);
        let boundary = Duration::from_millis(60);
        let current = fade.resolve(&[blue], boundary);
        assert_eq!(current[0], Rgb::new(188, 0, 188));
        let exact_work = fade.working[0];
        fade.arm(current.clone(), boundary, TransitionHint::Edit);
        assert_eq!(fade.active.as_ref().unwrap().source[0], exact_work);
        assert_eq!(fade.resolve(&[green], boundary), current);
        assert_eq!(fade.resolve(&[green], Duration::from_millis(180))[0], green);
    }

    #[test]
    fn every_tick_retargeting_progresses_and_settles() {
        for fps in [1u64, 6, 15, 30] {
            let mut fade = FrameTransition::default();
            let target = Rgb::new(255, 255, 255);
            let mut output = picture(Rgb::BLACK);
            fade.arm(output.clone(), Duration::ZERO, TransitionHint::Edit);
            for tick in 1..=100 {
                let now = Duration::from_nanos(tick * 1_000_000_000 / fps);
                output = fade.resolve(&[target], now);
                fade.arm(output.clone(), now, TransitionHint::Edit);
            }
            assert!(output[0].r > 250);
            let settled = fade.resolve(&[target], Duration::from_nanos(100 * 1_000_000_000 / fps) + Duration::from_millis(120));
            assert_eq!(settled[0], target);
            assert!(!fade.active());
            assert!(fade.retained_bytes() <= 4 * std::mem::size_of::<[f32; 3]>());
        }
    }

    #[test]
    fn moving_destination_clock_stall_immediate_and_shape_change() {
        let mut fade = FrameTransition::default();
        let blue = Rgb::new(0, 0, 255);
        fade.arm(picture(Rgb::BLACK), Duration::from_secs(4096), TransitionHint::Edit);
        let first = fade.resolve(&[blue], Duration::from_secs(4096) + Duration::from_millis(60));
        assert_eq!(first[0], Rgb::new(0, 0, 188));
        assert_eq!(fade.resolve(&[blue], Duration::ZERO), first);
        let green = Rgb::new(0, 255, 0);
        assert_eq!(fade.resolve(&[green], Duration::from_secs(4096) + Duration::from_millis(90))[0], Rgb::new(0, 225, 0));
        assert_eq!(fade.resolve(&[green], Duration::from_secs(5000))[0], green);
        fade.arm(picture(green), Duration::from_secs(5000), TransitionHint::Immediate);
        assert_eq!(fade.resolve(&[blue], Duration::from_secs(5000))[0], blue);
        fade.arm(picture(blue), Duration::from_secs(5000), TransitionHint::Edit);
        assert_eq!(&*fade.resolve(&[green, green], Duration::from_secs(5000)), &[green, green]);
        assert!(!fade.active());
    }
}
