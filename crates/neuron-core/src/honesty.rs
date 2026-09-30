// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Is a device's report descriptor telling the truth about its stream?
//!
//! Some devices declare one layout and send another (a Switch Pro in full mode fills the bytes its
//! descriptor calls buttons with a timer and motion-sensor samples). Decoded through the
//! descriptor, such a stream "presses" buttons dozens of times a second. The tell is physical: no
//! hand toggles several different buttons many times each within one second, but a counter or a
//! sensor read as buttons does it continuously. A stream caught doing it is untrusted for the rest
//! of the session and feeds no binds — the same rule as the device side: never guess at hardware.

use std::collections::HashMap;

/// Edges (presses and releases each count) within one window that a single control may make before
/// it counts as implausible: 10 edges is five full presses a second, sustained, on every one of
/// [`CONTROLS`] buttons at once.
const TOGGLES: u32 = 10;
/// How many different controls must be implausible at once to condemn the stream.
const CONTROLS: usize = 3;
const WINDOW_MS: u64 = 1_000;

/// Per-device verdict on descriptor-decoded controls.
#[derive(Debug, Default)]
pub struct Monitor {
    window_start: u64,
    last: Vec<(u16, u16)>,
    toggles: HashMap<(u16, u16), u32>,
    dishonest: bool,
}

impl Monitor {
    /// Feed the controls one report decoded to at `now` ms. Returns whether the stream is still
    /// trusted; once it isn't, it never is again this session.
    pub fn observe(&mut self, hits: &[(u16, u16)], now: u64) -> bool {
        if self.dishonest {
            return false;
        }
        if now.saturating_sub(self.window_start) >= WINDOW_MS {
            self.window_start = now;
            self.toggles.clear();
        }
        // Only buttons testify: a stick circled fast toggles its analog directions legitimately.
        let analog = |c: &&(u16, u16)| matches!(c.0, crate::analog::AXIS_POS_PAGE | crate::analog::AXIS_NEG_PAGE | crate::analog::HAT_PAGE);
        let buttons: Vec<(u16, u16)> = hits.iter().filter(|c| !analog(c)).copied().collect();
        for c in buttons.iter().filter(|c| !self.last.contains(c)).chain(self.last.iter().filter(|c| !buttons.contains(c))) {
            *self.toggles.entry(*c).or_default() += 1;
        }
        self.last = buttons;
        if self.toggles.values().filter(|&&n| n >= TOGGLES).count() >= CONTROLS {
            self.dishonest = true;
        }
        !self.dishonest
    }

    #[must_use]
    pub fn is_dishonest(&self) -> bool {
        self.dishonest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_read_as_buttons_is_caught_within_a_second() {
        let mut m = Monitor::default();
        let mut trusted_at_end = true;
        // A 120 Hz stream whose low bits count: buttons 1..3 flip on every report.
        for i in 0..120u64 {
            let hits: Vec<(u16, u16)> = (0..3u16).filter(|b| (i >> b) & 1 == 1).map(|b| (0x09, b + 1)).collect();
            trusted_at_end = m.observe(&hits, i * 8);
        }
        assert!(!trusted_at_end && m.is_dishonest());
    }

    #[test]
    fn a_human_mashing_one_button_is_trusted() {
        let mut m = Monitor::default();
        for i in 0..120u64 {
            let hits = if (i / 4) % 2 == 0 { vec![(0x09, 1)] } else { vec![] }; // ~15 presses/s
            assert!(m.observe(&hits, i * 8));
        }
    }

    #[test]
    fn a_human_rolling_across_buttons_is_trusted() {
        let mut m = Monitor::default();
        // Four buttons pressed in turn, each held ~100 ms: a fast but human pattern.
        for i in 0..250u64 {
            let b = ((i / 12) % 4) as u16 + 1;
            assert!(m.observe(&[(0x09, b)], i * 8));
        }
    }
}
