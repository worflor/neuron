// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Shared foreground-app edge detector.
//!
//! The app and CLI live loops both need the same shape: periodically sample the platform foreground
//! application and emit only focus-change edges. The platform lookup stays in [`crate::app`]; this
//! module owns the cadence and last-seen state so clients do not drift.

use std::time::{Duration, Instant};

pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(1200);

pub struct AppFocusSwitch {
    last: Option<String>,
    last_check: Instant,
    interval: Duration,
}

impl AppFocusSwitch {
    pub fn new() -> Self {
        Self::with_interval(DEFAULT_POLL_INTERVAL)
    }

    pub fn with_interval(interval: Duration) -> Self {
        AppFocusSwitch {
            last: None,
            last_check: Instant::now(),
            interval,
        }
    }

    pub fn poll(&mut self) -> Option<String> {
        self.poll_with(crate::app::foreground_app)
    }

    pub fn poll_with(&mut self, mut foreground: impl FnMut() -> Option<String>) -> Option<String> {
        if self.last_check.elapsed() < self.interval {
            return None;
        }
        self.last_check = Instant::now();
        let app = foreground()?;
        if self.last.as_deref() == Some(app.as_str()) {
            return None;
        }
        self.last = Some(app.clone());
        Some(app)
    }

    #[cfg(test)]
    fn force_due(&mut self) {
        self.last_check = Instant::now() - self.interval;
    }
}

impl Default for AppFocusSwitch {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_only_focus_change_edges() {
        let mut switch = AppFocusSwitch::with_interval(Duration::ZERO);
        assert_eq!(
            switch.poll_with(|| Some("a.exe".into())),
            Some("a.exe".into())
        );
        assert_eq!(switch.poll_with(|| Some("a.exe".into())), None);
        assert_eq!(
            switch.poll_with(|| Some("b.exe".into())),
            Some("b.exe".into())
        );
    }

    #[test]
    fn cadence_suppresses_early_samples() {
        let mut switch = AppFocusSwitch::with_interval(Duration::from_secs(60));
        assert_eq!(switch.poll_with(|| Some("a.exe".into())), None);
        switch.force_due();
        assert_eq!(
            switch.poll_with(|| Some("a.exe".into())),
            Some("a.exe".into())
        );
    }
}
