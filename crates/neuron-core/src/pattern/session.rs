// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Lighting sessions, two-phase atomic reconciliation, simulation snapshots, and presentation revisions.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::effects::Blend;
use crate::lighting::Rgb;
use crate::pattern::input::{
    default_control_to_matrix, EmitContext, FrameContext, ProcessInputHub, LOGICAL_TICK_DT,
};
use crate::pattern::{
    make_pattern, pattern_def, Bounds, Field, LayerDef, PaletteAddressing, Params, Pattern,
};
use crate::spectrum::Spectrum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LayerInstanceId(pub u64);

#[derive(Clone, Debug, PartialEq)]
pub struct LayerGeometry {
    pub bounds: Bounds,
    pub region: Vec<u32>,
    pub board_dims: (u8, u8),
}

pub struct SceneLayerSpec {
    pub id: LayerInstanceId,
    pub def: LayerDef,
}

pub struct PatternReconfigure<'a> {
    pub params: &'a Params,
    pub frame: &'a [[u8; 3]],
    pub next_geometry: &'a LayerGeometry,
    pub geometry_changed: bool,
}

#[derive(Clone, Debug)]
pub struct OwnedPatternReconfigure {
    pub params: Params,
    pub frame: Vec<[u8; 3]>,
    pub geometry: LayerGeometry,
}

impl OwnedPatternReconfigure {
    #[must_use]
    pub fn as_view<'a>(&'a self, current_geom: &'a LayerGeometry) -> PatternReconfigure<'a> {
        PatternReconfigure {
            params: &self.params,
            frame: &self.frame,
            next_geometry: &self.geometry,
            geometry_changed: self.geometry != *current_geom,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReconfigurePolicy {
    Preserve,
    Rebuild,
}

pub enum ReconcileStep {
    Preserve {
        id: LayerInstanceId,
        delta: OwnedPatternReconfigure,
    },
    Replace {
        id: LayerInstanceId,
        pattern_key: String,
        new_pattern: Box<dyn Pattern>,
        geometry: LayerGeometry,
    },
    Remove {
        id: LayerInstanceId,
    },
}

pub struct ReconcilePlan {
    pub steps: Vec<ReconcileStep>,
    pub order: Vec<LayerInstanceId>,
    pub presentation: PresentationRevision,
}

pub enum SessionCommand {
    ReconcileScene(Vec<SceneLayerSpec>),
    ResetLayer(LayerInstanceId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyError {
    UnknownPattern(String),
    LayerNotFound(LayerInstanceId),
    InvalidSpec(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::UnknownPattern(k) => write!(f, "unknown pattern: {k}"),
            ApplyError::LayerNotFound(id) => write!(f, "layer instance not found: {}", id.0),
            ApplyError::InvalidSpec(msg) => write!(f, "invalid layer spec: {msg}"),
        }
    }
}

impl std::error::Error for ApplyError {}

/// Authoritative simulation field state for one logical tick.
#[derive(Clone, Debug)]
pub struct SimulationSnapshot {
    pub tick: u64,
    pub elapsed: Duration,
    pub dt: Duration,
    pub layer_fields: Vec<(LayerInstanceId, Arc<Field>)>,
}

/// Presentation metadata for a single layer.
#[derive(Clone, Debug)]
pub struct PresentationLayer {
    pub id: LayerInstanceId,
    pub spectrum: Spectrum,
    pub palette_addressing: PaletteAddressing,
    pub region: Vec<u32>,
    pub blend: Blend,
    pub enabled: bool,
}

/// Versioned presentation revision defining how simulation fields resolve to colors.
#[derive(Clone, Debug, Default)]
pub struct PresentationRevision {
    pub revision: u64,
    pub layers: Vec<PresentationLayer>,
}

/// Cached resolved RGB frame.
#[derive(Clone, Debug)]
pub struct ResolvedFrame {
    pub tick: u64,
    pub presentation_rev: u64,
    pub presentation_elapsed: Duration,
    pub pixels: Arc<[Rgb]>,
}

pub struct SessionLayer {
    pub id: LayerInstanceId,
    pub pattern_key: String,
    pub pattern: Box<dyn Pattern>,
    pub geometry: LayerGeometry,
    pub cached_field: Arc<Field>,
    pub paused_first_tick: bool,
}

/// Authoritative per-device lighting session.
pub struct LightingSession {
    board_dims: (u8, u8),
    session_epoch: u64,
    tick: u64,
    elapsed: Duration,
    last_wall_time: Option<Instant>,
    last_input_generation: u64,

    layers: Vec<SessionLayer>,
    presentation: PresentationRevision,

    latest_sim_snapshot: Option<Arc<SimulationSnapshot>>,
    latest_resolved_frame: Option<Arc<ResolvedFrame>>,
}

impl LightingSession {
    #[must_use]
    pub fn new(board_dims: (u8, u8)) -> Self {
        LightingSession {
            board_dims,
            session_epoch: 0,
            tick: 0,
            elapsed: Duration::ZERO,
            last_wall_time: None,
            last_input_generation: 0,
            layers: Vec::new(),
            presentation: PresentationRevision::default(),
            latest_sim_snapshot: None,
            latest_resolved_frame: None,
        }
    }

    #[must_use]
    pub fn board_dims(&self) -> (u8, u8) {
        self.board_dims
    }

    #[must_use]
    pub fn tick(&self) -> u64 {
        self.tick
    }

    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    #[must_use]
    pub fn latest_sim_snapshot(&self) -> Option<Arc<SimulationSnapshot>> {
        self.latest_sim_snapshot.clone()
    }

    #[must_use]
    pub fn latest_resolved_frame(&self) -> Option<Arc<ResolvedFrame>> {
        self.latest_resolved_frame.clone()
    }

    pub fn presentation(&self) -> &PresentationRevision {
        &self.presentation
    }

    pub fn set_presentation(&mut self, pres: PresentationRevision) {
        self.presentation = pres;
    }

    /// Construct a lighting session directly from scene layer specs.
    pub fn from_specs(board_dims: (u8, u8), specs: &[SceneLayerSpec]) -> Result<Self, ApplyError> {
        let mut session = Self::new(board_dims);
        let plan = session.plan_reconcile(specs)?;
        session.commit_reconcile(plan);
        Ok(session)
    }

    /// Phase 1: Validate specs, query policies, and build an atomic reconciliation plan.
    /// Pure and non-mutating — returns Err if any spec is invalid; running state stays untouched.
    pub fn plan_reconcile(&self, next_specs: &[SceneLayerSpec]) -> Result<ReconcilePlan, ApplyError> {
        let (rows, cols) = self.board_dims;

        for s in next_specs {
            if pattern_def(&s.def.pattern).is_none() && make_pattern(&s.def.pattern).is_none() {
                return Err(ApplyError::UnknownPattern(s.def.pattern.clone()));
            }
        }

        let mut steps = Vec::new();
        let mut order = Vec::with_capacity(next_specs.len());

        for s in next_specs {
            order.push(s.id);

            let mut region = s.def.region.clone();
            region.sort_unstable();
            region.dedup();
            let bounds = Bounds::from_region(&region, rows, cols);
            let geometry = LayerGeometry {
                bounds,
                region,
                board_dims: (rows, cols),
            };

            let owned_delta = OwnedPatternReconfigure {
                params: s.def.params.clone(),
                frame: s.def.frame.clone(),
                geometry: geometry.clone(),
            };

            if let Some(existing) = self.layers.iter().find(|l| l.id == s.id) {
                if existing.pattern_key == s.def.pattern {
                    let view = owned_delta.as_view(&existing.geometry);
                    if existing.pattern.reconfigure_policy(&view) == ReconfigurePolicy::Preserve {
                        steps.push(ReconcileStep::Preserve {
                            id: s.id,
                            delta: owned_delta,
                        });
                        continue;
                    }
                }
            }

            let mut new_p = s
                .def
                .make_pattern()
                .ok_or_else(|| ApplyError::UnknownPattern(s.def.pattern.clone()))?;
            new_p.set_bounds(geometry.bounds);
            new_p.set_visible_region(&geometry.region, rows, cols);

            steps.push(ReconcileStep::Replace {
                id: s.id,
                pattern_key: s.def.pattern.clone(),
                new_pattern: new_p,
                geometry,
            });
        }

        for existing in &self.layers {
            if !next_specs.iter().any(|s| s.id == existing.id) {
                steps.push(ReconcileStep::Remove { id: existing.id });
            }
        }

        let pres_layers = next_specs
            .iter()
            .map(|s| {
                let mut region = s.def.region.clone();
                region.sort_unstable();
                region.dedup();
                let addressing = pattern_def(&s.def.pattern)
                    .map_or(PaletteAddressing::Field, |d| d.palette_addressing);
                PresentationLayer {
                    id: s.id,
                    spectrum: s.def.spectrum.clone(),
                    palette_addressing: addressing,
                    region,
                    blend: s.def.blend,
                    enabled: s.def.enabled,
                }
            })
            .collect();

        Ok(ReconcilePlan {
            steps,
            order,
            presentation: PresentationRevision {
                revision: self.presentation.revision.wrapping_add(1),
                layers: pres_layers,
            },
        })
    }

    /// Phase 2: Infallibly commit an accepted reconciliation plan.
    pub fn commit_reconcile(&mut self, plan: ReconcilePlan) {
        for step in plan.steps {
            match step {
                ReconcileStep::Preserve { id, delta } => {
                    if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
                        let view = delta.as_view(&layer.geometry);
                        layer.pattern.apply_reconfigure(&view);
                        layer.geometry = delta.geometry;
                        layer.pattern.set_bounds(layer.geometry.bounds);
                        layer.pattern.set_visible_region(
                            &layer.geometry.region,
                            self.board_dims.0,
                            self.board_dims.1,
                        );
                    }
                }
                ReconcileStep::Replace {
                    id,
                    pattern_key,
                    new_pattern,
                    geometry,
                } => {
                    let n = self.board_dims.0 as usize * self.board_dims.1 as usize;
                    let blank_field = Arc::new(Field::Scalar(vec![crate::pattern::Cell::default(); n]));
                    if let Some(layer) = self.layers.iter_mut().find(|l| l.id == id) {
                        layer.pattern_key = pattern_key;
                        layer.pattern = new_pattern;
                        layer.geometry = geometry;
                        layer.cached_field = blank_field;
                        layer.paused_first_tick = false;
                    } else {
                        self.layers.push(SessionLayer {
                            id,
                            pattern_key,
                            pattern: new_pattern,
                            geometry,
                            cached_field: blank_field,
                            paused_first_tick: false,
                        });
                    }
                }
                ReconcileStep::Remove { id } => {
                    self.layers.retain(|l| l.id != id);
                }
            }
        }

        self.layers.sort_by_key(|l| {
            plan.order
                .iter()
                .position(|&id| id == l.id)
                .unwrap_or(usize::MAX)
        });

        self.presentation = plan.presentation;
    }

    /// Explicitly restart a single layer instance fresh.
    pub fn reset_layer(&mut self, id: LayerInstanceId) -> Result<(), ApplyError> {
        let (rows, cols) = self.board_dims;
        let layer = self
            .layers
            .iter_mut()
            .find(|l| l.id == id)
            .ok_or(ApplyError::LayerNotFound(id))?;

        let mut fresh = make_pattern(&layer.pattern_key)
            .ok_or_else(|| ApplyError::UnknownPattern(layer.pattern_key.clone()))?;
        fresh.set_bounds(layer.geometry.bounds);
        fresh.set_visible_region(&layer.geometry.region, rows, cols);

        layer.pattern = fresh;
        let n = rows as usize * cols as usize;
        layer.cached_field = Arc::new(Field::Scalar(vec![crate::pattern::Cell::default(); n]));
        layer.paused_first_tick = false;

        Ok(())
    }

    /// Advance authoritative logical tick at pinned 60 Hz cadence.
    pub fn advance_tick(
        &mut self,
        input_hub: &ProcessInputHub,
        wall_now: Instant,
    ) -> Arc<SimulationSnapshot> {
        let is_discontinuity = self
            .last_wall_time
            .is_some_and(|last| wall_now.duration_since(last) > Duration::from_secs(2));
        self.last_wall_time = Some(wall_now);

        let dt = if is_discontinuity {
            Duration::ZERO
        } else {
            LOGICAL_TICK_DT
        };

        if is_discontinuity {
            for l in &mut self.layers {
                l.pattern.on_discontinuity();
            }
        }

        self.tick = self.tick.wrapping_add(1);
        self.elapsed += dt;

        let (input, next_gen) =
            input_hub.snapshot_for_matrix(self.last_input_generation, default_control_to_matrix);
        self.last_input_generation = next_gen;

        let mut snapshot_fields = Vec::with_capacity(self.layers.len());

        for layer in &mut self.layers {
            let enabled = self
                .presentation
                .layers
                .iter()
                .find(|pl| pl.id == layer.id)
                .is_none_or(|pl| pl.enabled);

            if !enabled {
                layer.paused_first_tick = true;
                snapshot_fields.push((layer.id, Arc::clone(&layer.cached_field)));
                continue;
            }

            let effective_dt = if layer.paused_first_tick {
                layer.paused_first_tick = false;
                Duration::ZERO
            } else {
                dt
            };

            let frame_ctx = FrameContext {
                session_epoch: self.session_epoch,
                tick: self.tick,
                elapsed: self.elapsed,
                dt: effective_dt,
                discontinuity: is_discontinuity,
                input: &input,
            };

            layer.pattern.advance(&frame_ctx, &layer.geometry);

            let emit_ctx = EmitContext {
                elapsed: self.elapsed,
            };
            let emitted = layer.pattern.emit(&emit_ctx, &layer.geometry);
            layer.cached_field = Arc::new(emitted);

            snapshot_fields.push((layer.id, Arc::clone(&layer.cached_field)));
        }

        let snapshot = Arc::new(SimulationSnapshot {
            tick: self.tick,
            elapsed: self.elapsed,
            dt,
            layer_fields: snapshot_fields,
        });

        self.latest_sim_snapshot = Some(Arc::clone(&snapshot));
        snapshot
    }

    /// Resolve simulation fields against presentation revision at high-precision presentation time.
    /// Pure presentation mapping — zero mutation of simulation ticks or RNG state.
    pub fn resolve_presentation(&mut self, presentation_elapsed: Duration) -> Arc<ResolvedFrame> {
        let (rows, cols) = self.board_dims;
        let n = rows as usize * cols as usize;
        let mut pixels = vec![Rgb::BLACK; n];

        let sim_opt = self.latest_sim_snapshot.clone();

        if let Some(sim) = sim_opt {
            for pres in &self.presentation.layers {
                if !pres.enabled {
                    continue;
                }
                let Some((_, field)) = sim.layer_fields.iter().find(|(id, _)| *id == pres.id) else {
                    continue;
                };

                let bounds = Bounds::from_region(&pres.region, rows, cols);
                let rendered_px = render_palette_addressing_time(
                    field,
                    &pres.spectrum,
                    presentation_elapsed,
                    pres.palette_addressing,
                    bounds,
                    cols,
                );

                if rendered_px.len() == n {
                    for i in 0..n {
                        if pres.region.is_empty() || pres.region.binary_search(&(i as u32)).is_ok() {
                            pixels[i] = crate::effects::blend_px(pixels[i], rendered_px[i], pres.blend);
                        }
                    }
                }
            }
        }

        let frame = Arc::new(ResolvedFrame {
            tick: self.tick,
            presentation_rev: self.presentation.revision,
            presentation_elapsed,
            pixels: Arc::from(pixels),
        });

        self.latest_resolved_frame = Some(Arc::clone(&frame));
        frame
    }

    /// Access live layer pattern by id (for inspecting internal state in tests).
    pub fn layer_pattern(&self, id: LayerInstanceId) -> Option<&dyn Pattern> {
        self.layers.iter().find(|l| l.id == id).map(|l| l.pattern.as_ref())
    }

    /// Access live layer pattern mutably by id (for test setups).
    pub fn layer_pattern_mut(&mut self, id: LayerInstanceId) -> Option<&mut Box<dyn Pattern>> {
        self.layers.iter_mut().find(|l| l.id == id).map(|l| &mut l.pattern)
    }

    /// Return order of current layers.
    pub fn layer_order(&self) -> Vec<LayerInstanceId> {
        self.layers.iter().map(|l| l.id).collect()
    }
}

pub fn render_palette_addressing_time(
    field: &Field,
    spectrum: &Spectrum,
    t: Duration,
    addressing: PaletteAddressing,
    bounds: Bounds,
    cols: u8,
) -> Vec<Rgb> {
    match field {
        Field::Color(px) => px.clone(),
        Field::Scalar(cells) => {
            if addressing == PaletteAddressing::Field {
                cells
                    .iter()
                    .map(|c| spectrum.at_time(t, c.u).scale_f(c.intensity))
                    .collect()
            } else {
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        let row = i / cols.max(1) as usize;
                        let col = i % cols.max(1) as usize;
                        let u = crate::pattern::placement_coordinate(bounds, row, col);
                        spectrum.at_time(t, u).scale_f(cell.intensity)
                    })
                    .collect()
            }
        }
    }
}
