// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Process Input Hub, canonical control identifiers, and matrix input snapshots.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

pub const LOGICAL_TICK_HZ: u32 = 60;
pub const LOGICAL_TICK_DT: Duration = Duration::from_nanos(16_666_667); // 60 Hz (1/60s)
pub const INPUT_HUB_CAPACITY: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyId(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ControlId {
    Key(KeyId),
    /// Vendor hardware macro button (e.g. Razer M1–M6)
    VendorMacro { vendor: u16, index: u8 },
}

/// Down-edge event with monotonic timestamp.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputEdge {
    pub control: ControlId,
    pub at: Duration,
}

/// Immutable record published once per global engine tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalInputSnapshot {
    pub generation: u64,
    pub down_controls: Vec<ControlId>,
    pub fresh_edges: Vec<InputEdge>,
    pub total_press_edges: u32,
    pub input_gap: bool,
}

/// Semantic down-edge mapped to target matrix coordinates with event timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatrixPress {
    pub control: ControlId,
    pub row: u8,
    pub col: u8,
    pub at: Duration,
}

/// Semantic held state mapped to target matrix coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MatrixHeld {
    pub control: ControlId,
    pub row: u8,
    pub col: u8,
}

/// Translated specifically for a device target's matrix keymap.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatrixInputSnapshot {
    pub input_generation: u64,
    pub pressed: Vec<MatrixPress>,
    pub held: Vec<MatrixHeld>,
    pub total_press_edges: u32,
    pub input_gap: bool,
}

impl MatrixInputSnapshot {
    #[must_use]
    pub fn empty() -> Self {
        MatrixInputSnapshot {
            input_generation: 0,
            pressed: Vec::new(),
            held: Vec::new(),
            total_press_edges: 0,
            input_gap: false,
        }
    }
}

/// Immutable inputs shared by every layer in one render job. Edges are consumed at simulation commit.
pub struct RenderInputs {
    pub sampled: Arc<MatrixInputSnapshot>,
    pub policy: super::ProviderPolicy,
}

thread_local! {
    static RENDER_INPUT: std::cell::RefCell<Option<Arc<MatrixInputSnapshot>>> = const { std::cell::RefCell::new(None) };
}

pub struct RenderInputGuard {
    previous: Option<Arc<MatrixInputSnapshot>>,
    _policy: super::ProviderGuard,
}

impl Drop for RenderInputGuard {
    fn drop(&mut self) { RENDER_INPUT.with(|input| *input.borrow_mut() = self.previous.take()); }
}

impl RenderInputs {
    #[must_use]
    pub fn enter(&self) -> RenderInputGuard {
        RenderInputGuard { previous: RENDER_INPUT.with(|input| input.replace(Some(self.sampled.clone()))),
            _policy: self.policy.enter() }
    }
}

pub(super) fn render_input() -> Option<Arc<MatrixInputSnapshot>> {
    RENDER_INPUT.with(|input| input.borrow().clone())
}

/// Bounded-history input hub retaining up to K = 64 generations of global input snapshots.
#[derive(Default)]
pub struct ProcessInputHub {
    generation: u64,
    ring: VecDeque<Arc<GlobalInputSnapshot>>,
    down_controls: HashSet<ControlId>,
    pending_edges: Vec<InputEdge>,
    total_press_edges_interval: u32,
    observation_cursor: Option<u64>,
    input_gap: bool,
    last_sample: Option<Duration>,
}

impl ProcessInputHub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    /// Enqueue an input transition with its monotonic timestamp.
    pub fn enqueue_event(&mut self, control: ControlId, down: bool, at: Duration) {
        if down {
            if self.down_controls.insert(control) {
                self.pending_edges.push(InputEdge { control, at });
                self.total_press_edges_interval = self.total_press_edges_interval.saturating_add(1);
            }
        } else {
            self.down_controls.remove(&control);
        }
    }

    /// Poll system key and macro key states from platform capture into the hub.
    pub fn poll_system_keys(&mut self, at: Duration) {
        if crate::capture::key_reads_suppressed() { return; }
        if self.last_sample.is_some_and(|last| at < last || at.saturating_sub(last) > Duration::from_secs(2)) {
            self.observation_cursor = None;
            self.down_controls.clear();
            self.pending_edges.clear();
            self.total_press_edges_interval = 0;
            self.input_gap = true;
        }
        let attaching = self.observation_cursor.is_none();
        let mut cursor = self.observation_cursor.unwrap_or_else(crate::capture::key_observation_head);
        let (observations, missed, _) = if crate::capture::key_reads_scripted() {
            (Vec::new(), 0, cursor)
        } else { crate::capture::key_observations_since(&mut cursor) };
        self.observation_cursor = Some(cursor);
        if missed > 0 {
            self.down_controls.clear();
            self.pending_edges.clear();
            self.total_press_edges_interval = 0;
            self.input_gap = true;
        }
        for event in observations {
            let control = if event.page == 0xFF00 && (0x20..0x26).contains(&event.usage) {
                Some(ControlId::VendorMacro { vendor: 0x1532, index: (event.usage - 0x20) as u8 })
            } else {
                crate::controls::control_to_vk(event.page, event.usage).map(|vk| ControlId::Key(KeyId(vk as u16)))
            };
            if let Some(control) = control {
                self.enqueue_event(control, event.down, Duration::from_secs_f64(event.at.max(0.0)));
            }
        }
        let sampled = attaching || missed > 0 || crate::capture::key_reads_scripted() || !crate::controls::held_registry_live()
            || self.last_sample.is_none_or(|last| at.saturating_sub(last) >= Duration::from_millis(500));
        if !sampled { return; }
        self.last_sample = Some(at);
        for vk in 1..=255u16 {
            let down = crate::capture::key_down(vk as i32);
            self.enqueue_event(ControlId::Key(KeyId(vk)), down, at);
        }
        for i in 0..crate::lighting::MACRO_KEY_NAMES.len() {
            let down = crate::capture::macro_key_down(i);
            self.enqueue_event(
                ControlId::VendorMacro {
                    vendor: 0x1532,
                    index: i as u8,
                },
                down,
                at,
            );
        }
        // Initial attachment and overflow establish held state without inventing a fresh press.
        if attaching || missed > 0 {
            self.pending_edges.clear();
            self.total_press_edges_interval = 0;
        }
    }

    /// Publish an immutable snapshot for the current engine tick, advancing generation by 1.
    pub fn publish_tick(&mut self) -> Arc<GlobalInputSnapshot> {
        self.generation = self.generation.wrapping_add(1);
        let mut down_vec: Vec<ControlId> = self.down_controls.iter().copied().collect();
        down_vec.sort_unstable();

        let snapshot = Arc::new(GlobalInputSnapshot {
            generation: self.generation,
            down_controls: down_vec,
            fresh_edges: std::mem::take(&mut self.pending_edges),
            total_press_edges: self.total_press_edges_interval,
            input_gap: std::mem::take(&mut self.input_gap),
        });

        self.total_press_edges_interval = 0;

        self.ring.push_back(Arc::clone(&snapshot));
        if self.ring.len() > INPUT_HUB_CAPACITY {
            self.ring.pop_front();
        }

        snapshot
    }

    /// Extract matrix input snapshot for a session tracking `last_input_generation`.
    /// Returns `(MatrixInputSnapshot, new_generation)`.
    pub fn snapshot_for_matrix<F>(
        &self,
        last_input_generation: u64,
        map_control: F,
    ) -> (MatrixInputSnapshot, u64)
    where
        F: Fn(ControlId) -> Option<(u8, u8)>,
    {
        let g = self.generation;
        if g == 0 || self.ring.is_empty() {
            return (MatrixInputSnapshot::empty(), 0);
        }

        let latest = self.ring.back().expect("non-empty ring");

        let mut held: Vec<MatrixHeld> = Vec::new();
        for &c in &latest.down_controls {
            if let Some((r, col)) = map_control(c) {
                held.push(MatrixHeld {
                    control: c,
                    row: r,
                    col,
                });
            }
        }
        held.sort_unstable_by_key(|h| (h.row, h.col, h.control));
        held.dedup();

        if g == last_input_generation {
            return (
                MatrixInputSnapshot {
                    input_generation: g,
                    pressed: Vec::new(),
                    held,
                    total_press_edges: 0,
                    input_gap: false,
                },
                g,
            );
        }

        let generations_behind = g.saturating_sub(last_input_generation);
        let history_gap = generations_behind > INPUT_HUB_CAPACITY as u64;
        let input_gap = history_gap
            || self.ring.iter().any(|s| s.generation > last_input_generation && s.input_gap);

        let start_gen = if history_gap {
            g.saturating_sub(INPUT_HUB_CAPACITY as u64)
        } else {
            last_input_generation
        };

        let mut pressed = Vec::new();
        let mut total_press_edges = 0u32;

        for snap in &self.ring {
            if snap.generation > start_gen && snap.generation <= g {
                for edge in &snap.fresh_edges {
                    if let Some((r, col)) = map_control(edge.control) {
                        pressed.push(MatrixPress {
                            control: edge.control,
                            row: r,
                            col,
                            at: edge.at,
                        });
                    }
                }
                total_press_edges = total_press_edges.saturating_add(snap.total_press_edges);
            }
        }

        (
            MatrixInputSnapshot {
                input_generation: g,
                pressed,
                held,
                total_press_edges,
                input_gap,
            },
            g,
        )
    }
}

/// Map standard virtual-keys and Razer macros to matrix coordinates.
#[must_use]
pub fn default_control_to_matrix(control: ControlId) -> Option<(u8, u8)> {
    match control {
        ControlId::Key(KeyId(vk)) => crate::lighting::vk_to_key_cell(vk as i32),
        ControlId::VendorMacro { index, .. } => {
            let idx = index as usize;
            if idx < crate::lighting::MACRO_KEY_NAMES.len() {
                crate::lighting::razer_key_cell(crate::lighting::MACRO_KEY_NAMES[idx])
            } else {
                None
            }
        }
    }
}

/// Execution frame context passed to `Pattern::advance`.
pub struct FrameContext<'a> {
    pub session_epoch: u64,
    pub tick: u64,
    pub elapsed: Duration,
    pub dt: Duration,
    pub discontinuity: bool,
    pub input: &'a MatrixInputSnapshot,
}

/// Read-only emission context passed to `Pattern::emit`.
pub struct EmitContext {
    pub elapsed: Duration,
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn dropped_presentations_preserve_short_edges_and_independent_cursors() {
        let mut hub = ProcessInputHub::new();
        let key = ControlId::Key(KeyId(65));
        hub.enqueue_event(key, true, Duration::from_millis(1));
        hub.enqueue_event(key, false, Duration::from_millis(2));
        hub.enqueue_event(key, true, Duration::from_millis(3));
        hub.publish_tick();
        for _ in 0..8 { hub.publish_tick(); }
        let map = |_| Some((0, 0));
        let (first, cursor) = hub.snapshot_for_matrix(0, map);
        let (second, _) = hub.snapshot_for_matrix(0, map);
        assert_eq!(first.pressed.len(), 2);
        assert_eq!(second.pressed.len(), 2);
        assert_eq!(first.pressed[1].at, Duration::from_millis(3));
        assert_eq!(first.held.len(), 1);
        assert!(!first.input_gap);
        assert!(hub.snapshot_for_matrix(cursor, map).0.pressed.is_empty());
    }

    #[test]
    fn expired_history_reports_gap_once_without_replaying_edges() {
        let mut hub = ProcessInputHub::new();
        let key = ControlId::Key(KeyId(65));
        hub.enqueue_event(key, true, Duration::ZERO);
        for _ in 0..INPUT_HUB_CAPACITY + 1 { hub.publish_tick(); }
        let (snapshot, cursor) = hub.snapshot_for_matrix(0, |_| Some((0, 0)));
        assert!(snapshot.input_gap);
        assert!(snapshot.pressed.is_empty());
        assert_eq!(snapshot.held.len(), 1);
        assert!(!hub.snapshot_for_matrix(cursor, |_| Some((0, 0))).0.input_gap);
    }
}
