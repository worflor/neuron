// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Opt-in UI callback, drawing and event-loop timing. No redraws are requested by the probe.

use crate::ui::{AppWindow, State};
use slint::ComponentHandle;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

thread_local! {
    static SAMPLES: RefCell<BTreeMap<&'static str, Vec<u64>>> = const { RefCell::new(BTreeMap::new()) };
    static PUBLICATION: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

pub struct Scope(Option<(&'static str, Instant)>);

impl Scope {
    pub fn new(name: &'static str) -> Self {
        Self(crate::light_render::profiling().then(|| (name, Instant::now())))
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if let Some((name, start)) = self.0 {
            let elapsed = start.elapsed();
            record(name, elapsed);
            if name == "light_publish" {
                PUBLICATION.with(|spent| spent.set(spent.get().saturating_add(elapsed)));
            }
        }
    }
}

fn record(name: &'static str, duration: Duration) {
    SAMPLES.with(|samples| {
        let mut samples = samples.borrow_mut();
        let values = samples.entry(name).or_default();
        if values.len() < 2048 { values.push(duration.as_micros().min(u128::from(u64::MAX)) as u64); }
    });
}

pub fn install(app: &AppWindow) -> Option<slint::Timer> {
    let enabled = crate::light_render::profiling();
    let diagnostics = cfg!(feature = "wgpu-renderer")
        && std::env::var("NEURON_RENDERER").as_deref() == Ok("wgpu");
    if enabled || diagnostics {
        let mut start = None;
        if let Err(err) = app.window().set_rendering_notifier(move |state, api| {
            match state {
                slint::RenderingState::RenderingSetup => eprintln!("neuron: UI renderer initialized with {api:?}"),
                slint::RenderingState::BeforeRendering if enabled => {
                    let publication = PUBLICATION.with(|spent| spent.replace(Duration::ZERO));
                    if !publication.is_zero() { record("publication_frame", publication); }
                    start = Some(Instant::now());
                },
                slint::RenderingState::AfterRendering if enabled => {
                    if let Some(start) = start.take() { record("draw", start.elapsed()); }
                },
                _ => {},
            }
        }) { eprintln!("neuron: renderer diagnostics unavailable ({err})"); }
    }
    if !enabled { return None; }
    let weak = app.as_weak();
    let timer = slint::Timer::default();
    let mut previous = None;
    let mut report = Instant::now();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
        let now = Instant::now();
        let active = weak.upgrade().is_some_and(|app| {
            let state = app.global::<State>();
            state.get_window_shown() && state.get_page() == 1
        });
        if active {
            if let Some(previous) = previous {
                record("event_loop_late", now.saturating_duration_since(previous).saturating_sub(Duration::from_millis(16)));
            }
            previous = Some(now);
        } else { previous = None; }
        if now.duration_since(report) >= Duration::from_secs(2) {
            report = now;
            SAMPLES.with(|samples| {
                for (name, mut values) in std::mem::take(&mut *samples.borrow_mut()) {
                    values.sort_unstable();
                    if values.is_empty() { continue; }
                    let n = values.len();
                    eprintln!("UI_PROF: {name} p95_us={} p99_us={} max_us={} over_16ms={} samples={n}",
                        values[(n * 95 / 100).min(n - 1)], values[(n * 99 / 100).min(n - 1)],
                        values[n - 1], values.iter().filter(|&&us| us > 16_000).count());
                }
            });
        }
    });
    Some(timer)
}
