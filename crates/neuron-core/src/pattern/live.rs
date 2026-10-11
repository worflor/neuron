// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Checked scene updates and complete base-frame publication, without device or UI ownership.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use super::{ApplyError, Bounds, Compositor, FrameTransition, Layer, LayerDef, LayerGeometry, PatternReconfigure,
    ReconfigurePolicy, RenderScratch, TransitionHint, pattern_def};
use crate::lighting::Rgb;

enum PlannedLayer { Retain { index: usize, params_changed: bool, frame_changed: bool }, Fresh(Layer) }

/// A validated update with every required replacement already constructed.
pub struct CompositorUpdate {
    definitions: Vec<LayerDef>,
    layers: Vec<PlannedLayer>,
    pub changed: bool,
    pub switch: bool,
}

fn canonical_definitions(defs: &[LayerDef], dims: Option<(u8, u8)>) -> Result<Vec<LayerDef>, ApplyError> {
    let mut ids = HashSet::new();
    let mut canonical = defs.to_vec();
    for def in &mut canonical {
        if let Some(id) = def.id {
            if !ids.insert(id) { return Err(ApplyError::InvalidSpec(format!("duplicate layer id: {}", id.0))); }
        }
        let kind = pattern_def(&def.pattern).ok_or_else(|| ApplyError::UnknownPattern(def.pattern.clone()))?;
        def.pattern = kind.key.into();
        if let Some(issue) = crate::layers::check_layer(def).first() { return Err(ApplyError::InvalidSpec(issue.message.clone())); }
        if def.params.0.values().any(|v| !v.is_finite()) {
            return Err(ApplyError::InvalidSpec("non-finite pattern parameter".into()));
        }
        let mut defaults = super::Params::defaults_for(kind.key);
        defaults.0.extend(def.params.0.clone());
        def.params = defaults;
        def.region.sort_unstable();
        def.region.dedup();
        if let Some((rows, cols)) = dims {
            let n = usize::from(rows) * usize::from(cols);
            if n == 0 || def.region.iter().any(|&i| i as usize >= n)
                || (kind.key == "custom" && def.frame.len() != n) {
                return Err(ApplyError::InvalidSpec("layer does not fit its surface".into()));
            }
        }
        for frame in &def.spectrum.seq {
            if !frame.hold.is_finite() || frame.hold < 0.0 || !frame.fade.is_finite() || frame.fade < 0.0
                || frame.palette.stops.iter().any(|s| !s.at.is_finite() || !(0.0..=1.0).contains(&s.at))
                || frame.palette.stops.windows(2).any(|stops| stops[0].at > stops[1].at) {
                return Err(ApplyError::InvalidSpec("invalid spectrum timing or stop position".into()));
            }
            let (speed, a, b): (f32, f32, f32) = match frame.palette.motion {
                crate::spectrum::Motion::Hold => (0.0, 0.0, 0.0),
                crate::spectrum::Motion::Drift { speed } | crate::spectrum::Motion::Cycle { speed } => (speed, 0.0, 0.0),
                crate::spectrum::Motion::Breathe { speed, depth } => (speed, depth, 0.0),
                crate::spectrum::Motion::Flow { speed, chaos } => (speed, chaos, 0.0),
            };
            if !speed.is_finite() || !a.is_finite() || !b.is_finite() {
                return Err(ApplyError::InvalidSpec("non-finite spectrum motion".into()));
            }
        }
    }
    Ok(canonical)
}

/// Validate authored content without constructing or configuring a generator.
pub fn validate_scene(defs: &[LayerDef], dims: Option<(u8, u8)>) -> Result<(), ApplyError> {
    canonical_definitions(defs, dims).map(|_| ())
}

/// Compare reusable scene contents independently of their template identities.
#[must_use]
pub fn same_scene_contents(left: &[LayerDef], right: &[LayerDef]) -> bool {
    let without_ids = |defs: &[LayerDef]| defs.iter().cloned().map(|mut d| { d.id = None; d }).collect::<Vec<_>>();
    match (canonical_definitions(&without_ids(left), None), canonical_definitions(&without_ids(right), None)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

impl Compositor {
    /// Prepare atomically; rejected definitions never mutate the running stack.
    pub fn plan_update(&self, defs: &[LayerDef], dims: Option<(u8, u8)>) -> Result<CompositorUpdate, ApplyError> {
        let definitions = canonical_definitions(defs, dims)?;
        let mut old = self.definitions.clone();
        for def in &mut old {
            if let Some(kind) = pattern_def(&def.pattern) {
                def.pattern = kind.key.into();
                let mut defaults = super::Params::defaults_for(kind.key);
                defaults.0.extend(def.params.0.clone());
                def.params = defaults;
            }
            def.region.sort_unstable();
            def.region.dedup();
        }
        if definitions == old {
            return Ok(CompositorUpdate { definitions, layers: Vec::new(), changed: false, switch: false });
        }
        let mut used = HashSet::new();
        let mut layers = Vec::with_capacity(definitions.len());
        let mut switch = definitions.len() != old.len();
        let (rows, cols) = dims.unwrap_or((1, 1));
        for (position, def) in definitions.iter().enumerate() {
            let candidate = if let Some(id) = def.id {
                if old.iter().filter(|o| o.id == Some(id)).count() == 1 { old.iter().position(|o| o.id == Some(id)) } else { None }
            } else {
                let mut matches = old.iter().enumerate().filter(|(i, o)| !used.contains(i) && o.id.is_none() && o.pattern == def.pattern);
                let first = matches.next().map(|(i, _)| i);
                if matches.next().is_none() { first } else { None }
            };
            let candidate = candidate.filter(|i| !used.contains(i) && old[*i].pattern == def.pattern && *i < self.layers.len());
            let mut retained = false;
            if let Some(i) = candidate {
                let geometry = LayerGeometry { bounds: Bounds::from_region(&def.region, rows, cols), region: def.region.clone(), board_dims: (rows, cols) };
                let geometry_changed = old[i].region != def.region;
                let simulation_changed = old[i].params != def.params || old[i].frame != def.frame || geometry_changed;
                let delta = PatternReconfigure { params: &def.params, frame: &def.frame, next_geometry: &geometry, geometry_changed };
                if !simulation_changed || self.layers[i].pattern.reconfigure_policy(&delta) == ReconfigurePolicy::Preserve {
                    layers.push(PlannedLayer::Retain { index: i, params_changed: old[i].params != def.params, frame_changed: old[i].frame != def.frame });
                    used.insert(i);
                    retained = true;
                    switch |= i != position || old[i].enabled != def.enabled || geometry_changed;
                }
            }
            if !retained {
                layers.push(PlannedLayer::Fresh(Self::from_defs(std::slice::from_ref(def)).layers.remove(0)));
                switch = true;
            }
        }
        Ok(CompositorUpdate { definitions, layers, changed: true, switch })
    }

    /// Commit only a plan prepared against this compositor at the current producer boundary.
    pub fn commit_update(&mut self, plan: CompositorUpdate) {
        if !plan.changed { return; }
        let mut previous: Vec<_> = std::mem::take(&mut self.layers).into_iter().map(Some).collect();
        let mut next = Vec::with_capacity(plan.layers.len());
        for (def, step) in plan.definitions.iter().zip(plan.layers) {
            let mut layer = match step {
                PlannedLayer::Fresh(layer) => layer,
                PlannedLayer::Retain { index: i, params_changed, frame_changed } => {
                    let mut layer = previous[i].take().expect("validated unique retained layer");
                    if params_changed { layer.pattern.configure(&def.params); }
                    if frame_changed { layer.pattern.set_frame(&def.frame); }
                    layer
                }
            };
            layer.spectrum.clone_from(&def.spectrum);
            layer.region.clone_from(&def.region);
            layer.blend = def.blend;
            layer.enabled = def.enabled;
            layer.palette_addressing = pattern_def(&def.pattern).expect("validated pattern").palette_addressing;
            next.push(layer);
        }
        self.layers = next;
        self.definitions = plan.definitions;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SurfaceIdentity {
    pub pid: u16,
    pub unit: String,
    pub plane: String,
    pub dimensions: (u8, u8),
}

#[derive(Clone, Debug)]
pub struct BaseFrame {
    pub target: SurfaceIdentity,
    pub owner_generation: u64,
    pub frame_id: u64,
    pub rendered_scene_revision: u64,
    pub accepted_scene_revision: u64,
    pub rendered_at: Duration,
    pub transition_active: bool,
    pub pixels: Arc<[Rgb]>,
}

#[derive(Clone)]
struct SceneUpdate { target: SurfaceIdentity, generation: u64, revision: u64, definitions: Arc<[LayerDef]>, hint: TransitionHint }

struct Control {
    target: SurfaceIdentity,
    generation: u64,
    revision: AtomicU64,
    pending: Mutex<Option<SceneUpdate>>,
    latest: Mutex<Option<Arc<BaseFrame>>>,
    rejected: Mutex<Option<(u64, String)>>,
    accepted: Mutex<Arc<[LayerDef]>>,
    desired: Mutex<Arc<[LayerDef]>>,
}

/// UI/control readers never acquire the live renderer's guard or advance its simulation.
#[derive(Clone)]
pub struct LiveSceneHandle(Arc<Control>);

impl LiveSceneHandle {
    pub fn publish(&self, definitions: Vec<LayerDef>, hint: TransitionHint) -> Result<u64, String> {
        self.publish_for(&self.0.target, self.0.generation, definitions, hint)
    }

    pub fn publish_for(&self, target: &SurfaceIdentity, generation: u64, mut definitions: Vec<LayerDef>, hint: TransitionHint) -> Result<u64, String> {
        if target != &self.0.target || generation != self.0.generation { return Err("stale lighting owner or different surface".into()); }
        crate::layers::normalize_against(&mut definitions, &self.accepted_definitions())?;
        let valid = canonical_definitions(&definitions, Some(self.0.target.dimensions)).ok().map(Arc::<[LayerDef]>::from);
        let mut pending = self.0.pending.lock().map_err(|_| "lighting mailbox unavailable")?;
        let revision = self.0.revision.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1)).map_err(|_| "lighting revision exhausted")? + 1;
        if let Some(valid) = valid { *self.0.desired.lock().map_err(|_| "lighting desired scene unavailable")? = valid; }
        *pending = Some(SceneUpdate { target: target.clone(), generation, revision, definitions: definitions.into(), hint });
        Ok(revision)
    }

    #[must_use]
    pub fn latest(&self) -> Option<Arc<BaseFrame>> { self.0.latest.lock().ok().and_then(|f| f.clone()) }
    #[must_use]
    pub fn rejected(&self) -> Option<(u64, String)> { self.0.rejected.lock().ok().and_then(|r| r.clone()) }
    #[must_use]
    pub fn target(&self) -> &SurfaceIdentity { &self.0.target }
    #[must_use]
    pub fn generation(&self) -> u64 { self.0.generation }
    #[must_use]
    pub fn desired_definitions(&self) -> Arc<[LayerDef]> {
        self.0.desired.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
    #[must_use]
    pub fn accepted_definitions(&self) -> Arc<[LayerDef]> {
        self.0.accepted.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }
}

/// One installed simulation, one latest desired revision, and one finite transition source.
pub struct LiveComposite {
    compositor: Compositor,
    handle: LiveSceneHandle,
    revision: u64,
    frame_id: u64,
    last_cycle: Option<Duration>,
    last_time: Duration,
    scratch: RenderScratch,
    transition: FrameTransition,
}

impl LiveComposite {
    pub fn new(target: SurfaceIdentity, mut definitions: Vec<LayerDef>) -> Result<Self, ApplyError> {
        if target.dimensions.0 == 0 || target.dimensions.1 == 0 { return Err(ApplyError::InvalidSpec("empty physical surface".into())); }
        crate::layers::normalize_ids(&mut definitions).map_err(ApplyError::InvalidSpec)?;
        let definitions = canonical_definitions(&definitions, Some(target.dimensions))?;
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
        let handle = LiveSceneHandle(Arc::new(Control { target, generation, revision: AtomicU64::new(0), pending: Mutex::new(None), latest: Mutex::new(None), rejected: Mutex::new(None), accepted: Mutex::new(definitions.clone().into()), desired: Mutex::new(definitions.clone().into()) }));
        Ok(Self { compositor: Compositor::from_defs(&definitions), handle, revision: 0, frame_id: 0, last_cycle: None, last_time: Duration::ZERO,
            scratch: RenderScratch::default(), transition: FrameTransition::default() })
    }

    #[must_use]
    pub fn handle(&self) -> LiveSceneHandle { self.handle.clone() }

    /// Emit the current boundary picture once, then install a prepared target for the next cycle.
    pub fn render_cycle(&mut self, cycle: Duration, now: Duration, pattern_time: Duration) -> Result<Arc<BaseFrame>, String> {
        if self.last_cycle == Some(cycle) || now < self.last_time {
            if let Some(frame) = self.handle.latest() { return Ok(frame); }
        }
        let pending = self.handle.0.pending.lock().map_err(|_| "lighting mailbox unavailable")?.take();
        let mut prepared = None;
        if let Some(update) = pending {
            let result = if update.target != self.handle.0.target || update.generation != self.handle.0.generation {
                Err(ApplyError::InvalidSpec("stale lighting owner".into()))
            } else if update.definitions.iter().any(|d| d.id.is_none()) {
                Err(ApplyError::InvalidSpec("live scene needs stable layer ids".into()))
            } else { self.compositor.plan_update(&update.definitions, Some(self.handle.0.target.dimensions)) };
            match result {
                Ok(plan) => prepared = Some((update, plan)),
                Err(error) => *self.handle.0.rejected.lock().map_err(|_| "lighting status unavailable")? = Some((update.revision, error.to_string())),
            }
        }
        let first = self.frame_id == 0;
        if first {
            if let Some((update, plan)) = prepared.take() {
                self.compositor.commit_update(plan);
                self.revision = update.revision;
            }
        }
        let rendered_revision = self.revision;
        let (rows, cols) = self.handle.0.target.dimensions;
        self.compositor.render_into(rows, cols, pattern_time, &mut self.scratch);
        let pixels = self.transition.resolve(&self.scratch.output, now);
        if let Some((update, plan)) = prepared {
            let changed = plan.changed;
            let hint = if plan.switch && update.hint != TransitionHint::Immediate { TransitionHint::Switch } else { update.hint };
            self.compositor.commit_update(plan);
            self.revision = update.revision;
            if changed { self.transition.arm(pixels.clone(), now, hint); }
            *self.handle.0.rejected.lock().map_err(|_| "lighting status unavailable")? = None;
        }
        self.frame_id = self.frame_id.checked_add(1).ok_or("lighting frame sequence exhausted")?;
        self.last_cycle = Some(cycle);
        self.last_time = now;
        if self.revision != rendered_revision || first {
            *self.handle.0.accepted.lock().map_err(|_| "lighting accepted scene unavailable")? = self.compositor.definitions.clone().into();
        }
        let frame = Arc::new(BaseFrame { target: self.handle.0.target.clone(), owner_generation: self.handle.0.generation,
            frame_id: self.frame_id, rendered_scene_revision: rendered_revision, accepted_scene_revision: self.revision,
            rendered_at: now, transition_active: self.transition.active(), pixels });
        *self.handle.0.latest.lock().map_err(|_| "lighting output unavailable")? = Some(frame.clone());
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn target(unit: &str) -> SurfaceIdentity { SurfaceIdentity { pid: 0x221, unit: unit.into(), plane: "lighting".into(), dimensions: (1, 2) } }
    fn scene(c: Rgb) -> Vec<LayerDef> { let mut d = crate::pattern::preset_layer("static").unwrap(); d.id = Some(super::super::LayerInstanceId(1)); d.spectrum = crate::spectrum::Spectrum::solid(c); vec![d] }
    fn frame(live: &mut LiveComposite, millis: u64) -> Arc<BaseFrame> { let t = Duration::from_millis(millis); live.render_cycle(t, t, t).unwrap() }

    struct Probe {
        inner: Box<dyn super::super::Pattern>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl super::super::Pattern for Probe {
        fn configure(&mut self, params: &super::super::Params) { self.calls.fetch_add(1, Ordering::Relaxed); self.inner.configure(params); }
        fn set_frame(&mut self, cells: &[[u8; 3]]) { self.calls.fetch_add(1, Ordering::Relaxed); self.inner.set_frame(cells); }
        fn set_bounds(&mut self, bounds: Bounds) { self.inner.set_bounds(bounds); }
        fn field_at_time(&mut self, rows: u8, cols: u8, t: Duration) -> super::super::Field { self.inner.field_at_time(rows, cols, t) }
        fn reconfigure_policy(&self, delta: &PatternReconfigure) -> ReconfigurePolicy { self.inner.reconfigure_policy(delta) }
        fn state_witness(&self) -> String { self.inner.state_witness() }
    }
    fn pointer(comp: &Compositor, index: usize) -> *const () { (&*comp.layers[index].pattern) as *const dyn super::super::Pattern as *const () }

    #[test]
    fn every_registered_pattern_preserves_its_instance_on_presentation_edits() {
        for kind in super::super::registry() {
            let mut def = LayerDef { pattern: kind.key.into(), id: Some(super::super::LayerInstanceId(1)), ..Default::default() };
            if kind.key == "custom" { def.frame = vec![[0, 0, 0]; 2]; }
            let mut comp = Compositor::from_defs(&[def.clone()]);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let inner = std::mem::replace(&mut comp.layers[0].pattern, Box::new(super::super::Uniform));
            comp.layers[0].pattern = Box::new(Probe { inner, calls: calls.clone() });
            let identity = pointer(&comp, 0);
            def.spectrum = crate::spectrum::Spectrum::solid(Rgb::new(50, 120, 220));
            def.blend = crate::effects::Blend::Add;
            let plan = comp.plan_update(&[def.clone()], Some((1, 2))).unwrap();
            comp.commit_update(plan);
            assert_eq!(pointer(&comp, 0), identity, "{} replaced on presentation edit", kind.key);
            assert_eq!(calls.load(Ordering::Relaxed), 0, "{} configured on presentation edit", kind.key);
            assert!(!comp.plan_update(&[def], Some((1, 2))).unwrap().changed);
        }
    }

    #[test]
    fn checked_reconcile_preserves_comet_state_and_moves_instances_by_id() {
        let _no_keys = crate::capture::suppress_key_reads();
        let mut a = super::super::preset_layer("comet").unwrap();
        a.id = Some(super::super::LayerInstanceId(1)); a.params.set("trails", 1.0);
        let mut b = a.clone(); b.id = Some(super::super::LayerInstanceId(2)); b.params.set("speed", 2.0);
        let mut comp = Compositor::from_defs(&[a.clone(), b.clone()]);
        for tick in 0..60 { let _ = comp.render_at_time(6, 22, Duration::from_millis(tick * 33)); }
        let state = comp.layers.iter().map(|l| l.pattern.state_witness()).collect::<Vec<_>>();
        assert!(!state[0].is_empty()); assert_ne!(state[0], state[1]);
        let ptrs = [pointer(&comp, 0), pointer(&comp, 1)];
        a.spectrum = crate::spectrum::Spectrum::solid(Rgb::new(0, 200, 100));
        comp.commit_update(comp.plan_update(&[b.clone(), a.clone()], Some((6, 22))).unwrap());
        assert_eq!([pointer(&comp, 0), pointer(&comp, 1)], [ptrs[1], ptrs[0]]);
        assert_eq!(comp.layers[1].pattern.state_witness(), state[0]);
        assert_eq!(comp.layers[0].pattern.state_witness(), state[1]);
        let mut bad = a.clone(); bad.params.set("speed", f32::NAN);
        assert!(comp.plan_update(&[b.clone(), bad], Some((6, 22))).is_err());
        assert_eq!(comp.layers[1].pattern.state_witness(), state[0]);
        b.pattern = "COMET".into();
        assert!(!comp.plan_update(&[b.clone(), a.clone()], Some((6, 22))).unwrap().changed);
        b.pattern = "flow".into(); b.params = Default::default();
        comp.commit_update(comp.plan_update(&[b, a], Some((6, 22))).unwrap());
        assert_eq!(comp.definitions[0].pattern, "flow");
        assert_ne!(pointer(&comp, 0), ptrs[1]); assert_eq!(pointer(&comp, 1), ptrs[0]);
    }

    #[test]
    fn source_and_mode_changes_rebuild_only_the_affected_layer() {
        for (key, parameter) in [("rain", "mode"), ("meter", "source")] {
            let mut def = LayerDef { pattern: key.into(), id: Some(super::super::LayerInstanceId(1)), ..Default::default() };
            let other = LayerDef { id: Some(super::super::LayerInstanceId(2)), ..Default::default() };
            let mut comp = Compositor::from_defs(&[def.clone(), other.clone()]);
            let pointers = [pointer(&comp, 0), pointer(&comp, 1)];
            let mut alias = def.clone(); alias.pattern = if key == "rain" { "STREAK" } else { "METER" }.into();
            assert!(!comp.plan_update(&[alias, other.clone()], Some((1, 2))).unwrap().changed);
            def.params.set("speed", 2.0);
            comp.commit_update(comp.plan_update(&[def.clone(), other.clone()], Some((1, 2))).unwrap());
            assert_eq!(pointer(&comp, 0), pointers[0]);
            def.params.set(parameter, 1.0);
            let plan = comp.plan_update(&[def, other], Some((1, 2))).unwrap(); assert!(plan.switch);
            comp.commit_update(plan);
            assert_ne!(pointer(&comp, 0), pointers[0]); assert_eq!(pointer(&comp, 1), pointers[1]);
        }
    }

    #[test]
    fn progress_precedes_rebase_and_noop_does_not_rearm() {
        let mut live = LiveComposite::new(target("a"), scene(Rgb::BLACK)).unwrap();
        let handle = live.handle();
        frame(&mut live, 0);
        for tick in 1..=60 {
            let colour = Rgb::new(100 + tick as u8, 0, 0);
            handle.publish(scene(colour), TransitionHint::Edit).unwrap();
            let boundary = frame(&mut live, tick * 33);
            if tick > 2 { assert!(boundary.pixels[0].r > 0); }
            assert!(boundary.accepted_scene_revision >= boundary.rendered_scene_revision);
        }
        let final_scene = scene(Rgb::new(160, 0, 0));
        handle.publish(final_scene, TransitionHint::Edit).unwrap();
        let last = frame(&mut live, 60 * 33 + 120);
        assert_eq!(last.pixels[0], Rgb::new(160, 0, 0));
        assert!(!last.transition_active);
    }

    #[test]
    fn invalid_revision_surface_and_duplicate_cycle_leave_current_state() {
        let mut live = LiveComposite::new(target("a"), scene(Rgb::new(255, 0, 0))).unwrap();
        let handle = live.handle();
        let first = frame(&mut live, 0);
        assert!(handle.publish_for(&target("b"), handle.generation(), scene(Rgb::BLACK), TransitionHint::Switch).is_err());
        assert!(handle.publish_for(&target("a"), handle.generation() + 1, scene(Rgb::BLACK), TransitionHint::Switch).is_err());
        let mut bad = scene(Rgb::BLACK); bad.push(bad[0].clone());
        assert!(handle.publish(bad, TransitionHint::Switch).is_err());
        let mut bad = scene(Rgb::BLACK); bad[0].pattern = "missing".into();
        let rejected = handle.publish(bad, TransitionHint::Switch).unwrap();
        assert_eq!(handle.desired_definitions().as_ref(), handle.accepted_definitions().as_ref());
        assert!(Arc::ptr_eq(&first, &frame(&mut live, 0)));
        assert_eq!(frame(&mut live, 33).pixels[0], Rgb::new(255, 0, 0));
        assert_eq!(handle.rejected().unwrap().0, rejected);
        assert_eq!(frame(&mut live, 40).accepted_scene_revision, 0);
        assert_eq!(handle.rejected().unwrap().0, rejected);
        handle.publish(Vec::new(), TransitionHint::Switch).unwrap();
        frame(&mut live, 66);
        assert_eq!(frame(&mut live, 416).pixels.as_ref(), &[Rgb::BLACK; 2]);
        assert!(handle.rejected().is_none());
    }
}
