// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Lighting presentation scheduler. Desired state and completed frames are latest-value slots.

use neuron::lighting::Rgb;
use neuron::pattern::{Compositor, LayerDef, LayerInstanceId, LightingSession, ProcessInputHub, ProviderPolicy, SceneLayerSpec};
use slint::{Rgb8Pixel, SharedPixelBuffer};
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const HERO_INTERVAL: Duration = Duration::from_millis(50);
const TILE_INTERVAL: Duration = Duration::from_millis(120);
const CACHE_LIMIT: usize = 128;
const CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SurfaceId { Hero, Preset(String), Saved(String) }

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Target {
    pub pid: u16,
    pub unit: String,
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemandRange {
    pub start: usize,
    pub end: usize,
    pub warm_start: usize,
    pub warm_end: usize,
}

impl DemandRange {
    pub fn visible(self, index: usize) -> bool { (self.start..self.end).contains(&index) }
    pub fn warm(self, index: usize) -> bool { (self.warm_start..self.warm_end).contains(&index) }
}

#[derive(Clone)]
pub struct Tile {
    pub id: SurfaceId,
    pub shelf: usize,
    pub row: usize,
    pub layers: Arc<[LayerDef]>,
}

#[derive(Clone, Default)]
pub struct DesiredSurfaceState {
    pub target: Target,
    pub dims: (u8, u8),
    pub content_rev: u64,
    pub structure_generation: u64,
    pub layout_generation: u64,
    pub layers: Arc<[LayerDef]>,
    pub tiles: Arc<[Tile]>,
    pub demand: [DemandRange; 4],
    pub active: bool,
    pub hero: bool,
    pub selected: Option<SurfaceId>,
    pub fps: u32,
}

pub struct CompletedFrame {
    pub id: SurfaceId,
    pub target: Target,
    pub dims: (u8, u8),
    pub content_rev: u64,
    pub layout_generation: u64,
    pub rgb: Arc<[Rgb]>,
    pub swatch: Option<SharedPixelBuffer<Rgb8Pixel>>,
    pub requested: Instant,
    pub render_time: Duration,
}

struct HeroSnapshot {
    target: Target,
    dims: (u8, u8),
    content_rev: u64,
    rgb: Arc<[Rgb]>,
}

struct Shared {
    desired: DesiredSurfaceState,
    completions: HashMap<SurfaceId, CompletedFrame>,
    invalidated: std::collections::HashSet<SurfaceId>,
    wake_pending: bool,
    stopped: bool,
    request_generation: u64,
    requested_at: Instant,
    metrics: Metrics,
    hero_snapshot: Option<HeroSnapshot>,
    #[cfg(test)]
    hero_isolated: bool,
}

impl Shared {
    fn publish(&mut self, frames: impl IntoIterator<Item = CompletedFrame>) -> bool {
        for frame in frames {
            if frame.id == SurfaceId::Hero {
                self.hero_snapshot = Some(HeroSnapshot { target: frame.target.clone(), dims: frame.dims,
                    content_rev: frame.content_rev, rgb: frame.rgb.clone() });
            }
            self.completions.insert(frame.id.clone(), frame);
        }
        let notify = !self.wake_pending;
        self.wake_pending = true;
        notify
    }

    fn take(&mut self) -> Vec<CompletedFrame> {
        self.wake_pending = false;
        self.completions.drain().map(|(_, frame)| frame).collect()
    }
}

#[derive(Default)]
struct Metrics {
    render: Vec<u64>,
    publication: Vec<u64>,
    latency: Vec<u64>,
    acknowledged: Option<Instant>,
}

pub fn profiling() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("NEURON_PROF").is_some())
}

/// Record only bounded numbers on Slint; percentile calculation and logging stay on the worker.
pub fn record_publication(elapsed: Duration, acknowledged: Option<Instant>) {
    if !profiling() { return; }
    let slot = facade().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(facade) = slot.as_ref() {
        let mut shared = facade.shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if shared.metrics.publication.len() < 2048 {
            shared.metrics.publication.push(elapsed.as_micros() as u64);
        }
        if let Some(requested) = acknowledged.filter(|requested| *requested == shared.requested_at
            && shared.metrics.acknowledged != Some(*requested) && shared.metrics.latency.len() < 2048) {
            shared.metrics.latency.push(requested.elapsed().as_micros() as u64);
            shared.metrics.acknowledged = Some(requested);
        }
    }
}

fn percentile(samples: &mut [u64], percentile: usize) -> u64 {
    if samples.is_empty() { return 0; }
    samples.sort_unstable();
    samples[(samples.len() * percentile / 100).min(samples.len() - 1)]
}

struct Facade {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    worker: JoinHandle<()>,
}

fn facade() -> &'static Mutex<Option<Facade>> {
    static FACADE: OnceLock<Mutex<Option<Facade>>> = OnceLock::new();
    FACADE.get_or_init(|| Mutex::new(None))
}

/// Replace canonical desired state atomically; a worker never sees a partial stack edit.
pub fn request(desired: DesiredSurfaceState) {
    let mut slot = facade().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if slot.is_none() && !desired.active { return; }
    if slot.is_none() {
        let shared = Arc::new((Mutex::new(Shared {
            desired: desired.clone(), completions: HashMap::new(), invalidated: std::collections::HashSet::new(), wake_pending: false, stopped: false,
            request_generation: 0, requested_at: Instant::now(), metrics: Metrics::default(), hero_snapshot: None,
            #[cfg(test)] hero_isolated: false,
        }), Condvar::new()));
        let worker_shared = shared.clone();
        match neuron::worker::spawn_named("neuron-light-render", move || run(worker_shared)) {
            Ok(worker) => *slot = Some(Facade { shared, worker }),
            Err(e) => { eprintln!("neuron: lighting render worker unavailable ({e})"); return; }
        }
    }
    if let Some(facade) = slot.as_ref() {
        let (lock, wake) = &*facade.shared;
        let mut state = lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if same_request(&state.desired, &desired) { return; }
        state.desired = desired;
        state.requested_at = Instant::now();
        state.request_generation = state.request_generation.wrapping_add(1);
        drop(state);
        wake.notify_all();
    }
}

fn same_request(a: &DesiredSurfaceState, b: &DesiredSurfaceState) -> bool {
    a.target == b.target && a.dims == b.dims && a.content_rev == b.content_rev
        && a.structure_generation == b.structure_generation && a.layout_generation == b.layout_generation
        && Arc::ptr_eq(&a.layers, &b.layers) && Arc::ptr_eq(&a.tiles, &b.tiles)
        && a.demand == b.demand && a.active == b.active && a.hero == b.hero
        && a.selected == b.selected && a.fps == b.fps
}

/// Clear wake ownership while taking completions under the same lock as worker publication.
pub fn take_completed() -> Vec<CompletedFrame> {
    let slot = facade().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(facade) = slot.as_ref() else { return Vec::new(); };
    let mut shared = facade.shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    shared.take()
}

/// Eviction drops both image and producer cache entries so approaching rows can prewarm again.
pub fn invalidate_thumbnail(id: SurfaceId) {
    let slot = facade().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(facade) = slot.as_ref() {
        let mut shared = facade.shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.completions.remove(&id);
        shared.invalidated.insert(id);
        shared.request_generation = shared.request_generation.wrapping_add(1);
        drop(shared);
        facade.shared.1.notify_all();
    }
}

pub fn shutdown() {
    let facade = facade().lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    if let Some(facade) = facade {
        facade.shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).stopped = true;
        facade.shared.1.notify_all();
        if facade.worker.join().is_err() { eprintln!("neuron: lighting render worker panicked"); }
    }
}

enum Producer {
    Pending,
    Session { session: LightingSession, last_tick: Instant },
    Legacy(Compositor),
}

struct Cached {
    producer: Producer,
    rebase_clocks: Vec<(LayerInstanceId, String)>,
    layers: Arc<[LayerDef]>,
    dims: (u8, u8),
    structure_generation: u64,
    due: Instant,
    last_used: Instant,
    visible: bool,
    warm: bool,
    scratch: neuron::pattern::RenderScratch,
    prewarmed: bool,
    input_cursor: u64,
    input_attached: bool,
    rgb: Arc<[Rgb]>,
    swatch: Option<SharedPixelBuffer<Rgb8Pixel>>,
}

impl Cached {
    fn new(layers: Arc<[LayerDef]>, dims: (u8, u8), structure_generation: u64, now: Instant) -> Self {
        let mut entry = Self::pending(layers, dims, structure_generation, now);
        entry.prepare(now);
        entry
    }

    fn pending(layers: Arc<[LayerDef]>, dims: (u8, u8), structure_generation: u64, now: Instant) -> Self {
        Self { producer: Producer::Pending, rebase_clocks: Vec::new(), layers, dims, structure_generation, due: now, last_used: now,
            visible: false, warm: false, scratch: neuron::pattern::RenderScratch::default(), prewarmed: false,
            input_cursor: 0, input_attached: false, rgb: Arc::from([]), swatch: None }
    }

    fn prepare(&mut self, now: Instant) {
        if !matches!(self.producer, Producer::Pending) { return; }
        let session_capable = self.layers.iter().all(|d| neuron::pattern::pattern_def(&d.pattern).is_some_and(neuron::pattern::PatternDef::session_capable));
        self.producer = if session_capable {
            match LightingSession::from_specs(self.dims, &specs(&self.layers)) {
                Ok(session) => Producer::Session { session, last_tick: now.checked_sub(neuron::pattern::LOGICAL_TICK_DT).unwrap_or(now) },
                Err(_) => Producer::Legacy(Compositor::from_defs(&self.layers)),
            }
        } else { Producer::Legacy(Compositor::from_defs(&self.layers)) };
    }

    fn reconcile(&mut self, layers: Arc<[LayerDef]>, dims: (u8, u8), structure_generation: u64, now: Instant) {
        if self.dims == dims && self.structure_generation == structure_generation && Arc::ptr_eq(&self.layers, &layers) { return; }
        if matches!(self.producer, Producer::Pending) {
            if self.layers != layers || self.dims != dims || self.structure_generation != structure_generation {
                self.layers = layers;
                self.dims = dims;
                self.structure_generation = structure_generation;
                self.due = now;
                self.prewarmed = false;
            }
            return;
        }
        let session_capable = layers.iter().all(|d| neuron::pattern::pattern_def(&d.pattern).is_some_and(neuron::pattern::PatternDef::session_capable));
        if self.dims != dims {
            *self = Self::new(layers, dims, structure_generation, now);
            return;
        }
        if !session_capable && matches!(self.producer, Producer::Session { .. }) {
            if let Producer::Session { session, .. } = std::mem::replace(&mut self.producer, Producer::Pending) {
                self.rebase_clocks = self.layers.iter().filter_map(|def| def.id.map(|id| (id, def.pattern.clone()))).collect();
                let (compositor, cursor) = session.into_compositor(&specs(&self.layers));
                self.input_cursor = cursor;
                self.producer = Producer::Legacy(compositor);
            }
        }
        // A compatibility producer keeps its live instances after the last legacy layer is removed.
        // New all-qualified surfaces still start as sessions; no second simulation runs here.
        if self.layers != layers {
            match &mut self.producer {
                Producer::Pending => {},
                Producer::Session { session, .. } => {
                    if let Ok(plan) = session.plan_reconcile(&specs(&layers)) { session.commit_reconcile(plan); }
                    else { *self = Self::new(layers, dims, structure_generation, now); return; }
                }
                Producer::Legacy(comp) => {
                    comp.update_defs(&layers);
                }
            }
            self.layers = layers;
            self.structure_generation = structure_generation;
            self.due = now;
            self.prewarmed = false;
        }
    }

    fn advance(&mut self, hub: &ProcessInputHub, now: Instant) {
        self.prepare(now);
        if !self.input_attached {
            if neuron::pattern::may_read_provider() {
                match &mut self.producer {
                    Producer::Session { session, .. } => session.rebaseline_input(hub),
                    Producer::Legacy(_) => self.input_cursor = hub.current_generation(),
                    Producer::Pending => {},
                }
            }
            self.input_attached = true;
        }
        if let Producer::Session { session, last_tick } = &mut self.producer {
            if now.duration_since(*last_tick) > Duration::from_secs(2) {
                session.advance_tick(hub, now);
                *last_tick = now;
            } else {
                while now.duration_since(*last_tick) >= neuron::pattern::LOGICAL_TICK_DT {
                    *last_tick += neuron::pattern::LOGICAL_TICK_DT;
                    session.advance_tick(hub, *last_tick);
                }
            }
        }
    }

    fn render(&mut self, hub: &ProcessInputHub, now: Instant, elapsed: Duration, policy: ProviderPolicy) -> Arc<[Rgb]> {
        self.advance(hub, now);
        match &mut self.producer {
            Producer::Pending => return self.rgb.clone(),
            Producer::Legacy(comp) => {
                let (sampled, cursor) = hub.snapshot_for_matrix(self.input_cursor, neuron::pattern::default_control_to_matrix);
                let inputs = neuron::pattern::RenderInputs { sampled: Arc::new(sampled), policy };
                let _inputs = inputs.enter();
                for (id, key) in self.rebase_clocks.drain(..) { comp.rebase_pattern_clock(id, &key, elapsed); }
                comp.render_into(self.dims.0, self.dims.1, elapsed, &mut self.scratch);
                self.input_cursor = cursor;
            },
            Producer::Session { session, .. } => {
                session.resolve_into(elapsed, &mut self.scratch);
            }
        }
        if self.rgb.as_ref() != self.scratch.output.as_slice() {
            self.rgb = Arc::from(self.scratch.output.as_slice());
            self.swatch = None;
        }
        self.rgb.clone()
    }

    fn retained_bytes(&self) -> usize {
        let cells = usize::from(self.dims.0) * usize::from(self.dims.1);
        cells * self.layers.len().max(1) * 32
            + self.scratch.retained_bytes() + self.rgb.len() * std::mem::size_of::<Rgb>()
            + self.layers.iter().map(|layer| layer.frame.len() * 6).sum::<usize>()
            + self.swatch.as_ref().map_or(0, |image| std::mem::size_of_val(image.as_slice()))
    }

    fn thumbnail(&mut self, rgb: &Arc<[Rgb]>) -> SharedPixelBuffer<Rgb8Pixel> {
        if self.rgb.as_ref() != rgb.as_ref() {
            self.rgb = rgb.clone();
            self.swatch = None;
        }
        self.swatch.get_or_insert_with(|| rasterize(rgb, self.dims)).clone()
    }
}

/// Preserve editor identities across parameter edits and reorders without changing persisted data.
pub fn identify_layers(previous: &[LayerDef], next: &[LayerDef]) -> Arc<[LayerDef]> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1 << 32);
    let mut used = std::collections::HashSet::new();
    let mut layers = next.to_vec();
    // Reserve explicit survivors before matching unidentified additions; an inserted copy must
    // never take the identity of an identical existing layer that appears later in the stack.
    for layer in &mut layers {
        if let Some(id) = layer.id {
            if !used.insert(id) { layer.id = None; }
        }
    }
    for layer in &mut layers {
        if layer.id.is_none() {
            layer.id = previous.iter().find(|old| {
                same_definition(old, layer) && old.id.is_some_and(|id| !used.contains(&id))
            }).and_then(|old| old.id);
            if let Some(id) = layer.id { used.insert(id); }
        }
    }
    for (index, layer) in layers.iter_mut().enumerate() {
        if layer.id.is_none() {
            layer.id = previous.get(index).filter(|old| old.pattern == layer.pattern)
                .and_then(|old| old.id).filter(|id| !used.contains(id))
                .or_else(|| {
                    let mut candidates = previous.iter().filter(|old| old.pattern == layer.pattern)
                        .filter_map(|old| old.id).filter(|id| !used.contains(id));
                    let first = candidates.next();
                    if candidates.next().is_none() { first } else { None }
                });
            let id = *layer.id.get_or_insert_with(|| loop {
                let id = LayerInstanceId(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                if !used.contains(&id) && !previous.iter().any(|old| old.id == Some(id)) { break id; }
            });
            used.insert(id);
        }
    }
    layers.into()
}

fn specs(layers: &[LayerDef]) -> Vec<SceneLayerSpec> {
    layers.iter().enumerate().map(|(i, def)| SceneLayerSpec { id: def.id.unwrap_or(LayerInstanceId(i as u64 + 1)), def: def.clone() }).collect()
}

fn same_definition(a: &LayerDef, b: &LayerDef) -> bool {
    let LayerDef { id: _, pattern, params, spectrum, region, blend, opacity, enabled, frame, attachment, boundary } = a;
    (pattern, params, spectrum, region, blend, opacity, enabled, frame, attachment, boundary)
        == (&b.pattern, &b.params, &b.spectrum, &b.region, &b.blend, &b.opacity, &b.enabled, &b.frame, &b.attachment, &b.boundary)
}

pub(crate) fn same_configuration(a: &[LayerDef], b: &[LayerDef]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_definition(a, b))
}

fn evict_dormant(cache: &mut HashMap<SurfaceId, Cached>) {
    while cache.len() > CACHE_LIMIT || cache.values().map(Cached::retained_bytes).sum::<usize>() > CACHE_BYTES {
        let oldest = cache.iter().filter(|(id, entry)| **id != SurfaceId::Hero && !entry.visible && !entry.warm)
            .min_by_key(|(_, entry)| entry.last_used).map(|(id, _)| id.clone());
        if let Some(id) = oldest { cache.remove(&id); } else { break; }
    }
}

type SelectedJob = (SurfaceId, Arc<[LayerDef]>, bool, Instant);

fn select_job(desired: &DesiredSurfaceState, cache: &mut HashMap<SurfaceId, Cached>, now: Instant) -> (Option<SelectedJob>, Instant) {
        let mut selected: Option<SelectedJob> = None;
        let mut next_due = now + Duration::from_mins(1);
        if !desired.active || desired.dims.0 == 0 || desired.dims.1 == 0 {
            for entry in cache.values_mut() { entry.visible = false; entry.warm = false; }
        }
        if desired.active && desired.dims.0 > 0 && desired.dims.1 > 0 {
            if desired.hero {
                let entry = cache.entry(SurfaceId::Hero).or_insert_with(|| Cached::pending(desired.layers.clone(), desired.dims, desired.structure_generation, now));
                entry.reconcile(desired.layers.clone(), desired.dims, desired.structure_generation, now);
                if entry.due <= now { selected = Some((SurfaceId::Hero, desired.layers.clone(), true, entry.due)); }
                next_due = next_due.min(entry.due);
            }
            for tile in desired.tiles.iter() {
                let demand = desired.demand[tile.shelf];
                let visible = demand.visible(tile.row);
                let warm = demand.warm(tile.row);
                if !visible && !warm {
                    if let Some(cached) = cache.get_mut(&tile.id) { cached.visible = false; cached.warm = false; }
                    continue;
                }
                let entry = cache.entry(tile.id.clone()).or_insert_with(|| Cached::pending(tile.layers.clone(), desired.dims, 0, now));
                entry.reconcile(tile.layers.clone(), desired.dims, 0, now);
                if visible && !entry.visible { entry.due = now; }
                entry.visible = visible;
                entry.warm = warm;
                if !visible && entry.prewarmed { continue; }
                next_due = next_due.min(entry.due);
                if entry.due <= now && selected.as_ref().is_none_or(|(id, _, chosen_visible, due)| {
                    if *id == SurfaceId::Hero {
                        visible && now.saturating_duration_since(entry.due) >= Duration::from_millis(250)
                    } else { (visible && !chosen_visible) || (visible == *chosen_visible && entry.due < *due) }
                }) { selected = Some((tile.id.clone(), tile.layers.clone(), visible, entry.due)); }
            }
        }
    (selected, next_due)
}

fn hero_policy() -> ProviderPolicy {
    if cfg!(test) { ProviderPolicy::InjectedDeterministic } else { ProviderPolicy::AuthoritativeLive }
}

type HeroSeed = (Option<Cached>, ProcessInputHub, Target, Instant);

struct HeroLane {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for HeroLane {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).stopped = true;
        self.shared.1.notify_all();
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() { eprintln!("neuron: isolated lighting worker panicked"); }
        }
    }
}

fn run(shared: Arc<(Mutex<Shared>, Condvar)>) { run_lane(shared, None, false); }

fn run_lane(shared: Arc<(Mutex<Shared>, Condvar)>, seed: Option<HeroSeed>, hero_only: bool) {
    let mut cache: HashMap<SurfaceId, Cached> = HashMap::new();
    let mut previous_tiles: Arc<[Tile]> = Arc::from([]);
    let (hero, mut hub, mut last_target, mut last_input) = seed.unwrap_or_else(||
        (None, ProcessInputHub::new(), Target::default(), Instant::now()));
    if let Some(hero) = hero { cache.insert(SurfaceId::Hero, hero); }
    let mut hero_lane: Option<HeroLane> = None;
    let mut empty_hub = ProcessInputHub::new();
    let mut demo_step = None;
    let mut last_report = Instant::now();
    loop {
        let now = Instant::now();
        let (mut desired, request_generation, requested_at) = {
            let mut state = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.stopped { break; }
            if !hero_only { for id in state.invalidated.drain() { cache.remove(&id); } }
            (state.desired.clone(), state.request_generation, state.requested_at)
        };
        if hero_only { desired.tiles = Arc::from([]); }
        else if hero_lane.is_some() { desired.hero = false; }
        if !hero_only && !Arc::ptr_eq(&previous_tiles, &desired.tiles) {
            let present: std::collections::HashSet<_> = desired.tiles.iter().map(|tile| &tile.id).collect();
            cache.retain(|id, _| *id == SurfaceId::Hero || present.contains(id));
            previous_tiles = desired.tiles.clone();
        }
        if desired.target != last_target {
            cache.clear();
            hub = ProcessInputHub::new();
            last_target = desired.target.clone();
        }
        let selection = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| select_job(&desired, &mut cache, now)));
        let (selected, mut next_due) = if let Ok(selection) = selection { selection } else {
            cache.clear();
            hub = ProcessInputHub::new();
            last_input = now.checked_sub(neuron::pattern::LOGICAL_TICK_DT).unwrap_or(now);
            eprintln!("neuron: discarded failed lighting reconciliation");
            let guard = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.stopped { break; }
            if guard.request_generation == request_generation {
                let _ = shared.1.wait_timeout(guard, HERO_INTERVAL).unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            continue;
        };
        evict_dormant(&mut cache);
        if desired.active && desired.hero && cache.contains_key(&SurfaceId::Hero) {
            next_due = next_due.min(last_input + neuron::pattern::LOGICAL_TICK_DT);
            if now.saturating_duration_since(last_input) >= neuron::pattern::LOGICAL_TICK_DT {
                let advanced = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _policy = hero_policy().enter();
                    let _quiet = cfg!(test).then(neuron::capture::suppress_key_reads);
                    if now.saturating_duration_since(last_input) > Duration::from_secs(2) { hub = ProcessInputHub::new(); }
                    hub.poll_system_keys(neuron::pattern::render_epoch().elapsed());
                    let _ = hub.publish_tick();
                    if let Some(hero) = cache.get_mut(&SurfaceId::Hero) { hero.advance(&hub, now); }
                }));
                last_input = now;
                next_due = next_due.max(now);
                if advanced.is_err() {
                    cache.remove(&SurfaceId::Hero);
                    hub = ProcessInputHub::new();
                    last_input = now.checked_sub(neuron::pattern::LOGICAL_TICK_DT).unwrap_or(now);
                    eprintln!("neuron: discarded failed lighting simulation");
                    continue;
                }
            }
        }
        let Some((id, _, _, _)) = selected else {
            let guard = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.stopped { break; }
            // Re-check desired identity to avoid sleeping through a request made during selection.
            if guard.request_generation != request_generation {
                continue;
            }
            let _ = shared.1.wait_timeout(guard, next_due.saturating_duration_since(Instant::now()))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            continue;
        };
        let elapsed = neuron::pattern::render_epoch().elapsed();
        sample_representative_keys(&mut empty_hub, &mut demo_step, elapsed);
        let hero = id == SurfaceId::Hero;
        let selected_catalog = !hero && desired.selected.as_ref() == Some(&id)
            && desired.tiles.iter().find(|tile| tile.id == id).is_some_and(|tile| same_configuration(&tile.layers, &desired.layers));
        let shared_rgb = if selected_catalog {
            let state = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.hero_snapshot.as_ref().filter(|snapshot| state.desired.hero
                && snapshot.target == desired.target && snapshot.dims == desired.dims
                && snapshot.content_rev == desired.content_rev).map(|snapshot| snapshot.rgb.clone())
        } else { None };
        let entry = match cache.get_mut(&id) { Some(entry) => entry, None => continue };
        let started = now;
        let rendered = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let policy = if hero { hero_policy() } else if selected_catalog { ProviderPolicy::SharedSnapshotNoStart } else { ProviderPolicy::Representative };
            let _policy = policy.enter();
            let _no_keys = (!hero || cfg!(test)).then(neuron::capture::suppress_key_reads);
            let rgb = if let Some(rgb) = shared_rgb { rgb }
                else if selected_catalog { entry.render(&hub, now, elapsed, policy) }
                else { match &id {
                    SurfaceId::Preset(slug) => representative(slug, entry, &empty_hub, now, elapsed),
                    _ => entry.render(if hero { &hub } else { &empty_hub }, now, elapsed, policy),
                } };
            let swatch = (!hero).then(|| entry.thumbnail(&rgb));
            (rgb, swatch)
        }));
        if let Ok((rgb, swatch)) = rendered {
                entry.last_used = now;
                entry.prewarmed = true;
                entry.due = Instant::now() + if hero { HERO_INTERVAL.max(Duration::from_secs_f64(1.0 / f64::from(desired.fps.max(1)))) } else { TILE_INTERVAL };
                let frame = CompletedFrame {
                    id: id.clone(), target: desired.target.clone(), dims: desired.dims,
                    content_rev: desired.content_rev, layout_generation: desired.layout_generation,
                    rgb, swatch, requested: requested_at, render_time: started.elapsed(),
                };
                let mut frames = vec![frame];
                if hero {
                    // A matching selected tile consumes this exact snapshot without acquiring providers.
                    for tile in desired.tiles.iter().filter(|tile| desired.selected.as_ref() == Some(&tile.id) && same_configuration(&tile.layers, &desired.layers)) {
                        let demand = desired.demand[tile.shelf];
                        if !demand.visible(tile.row) && !demand.warm(tile.row) { continue; }
                        let tile_entry = cache.entry(tile.id.clone()).or_insert_with(|| Cached::pending(tile.layers.clone(), desired.dims, 0, now));
                        if tile_entry.prewarmed && tile_entry.due > now { continue; }
                        let rgb = frames[0].rgb.clone();
                        let swatch = tile_entry.thumbnail(&rgb);
                        tile_entry.last_used = now;
                        tile_entry.prewarmed = true;
                        tile_entry.due = Instant::now() + TILE_INTERVAL;
                        frames.push(CompletedFrame { id: tile.id.clone(), target: desired.target.clone(), dims: desired.dims,
                            content_rev: desired.content_rev, layout_generation: desired.layout_generation,
                            rgb, swatch: Some(swatch), requested: requested_at, render_time: started.elapsed() });
                    }
                }
                let notify = {
                    let mut state = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if profiling() && !hero && state.metrics.render.len() < 2048 {
                        state.metrics.render.push(frames[0].render_time.as_micros() as u64);
                    }
                    state.publish(frames)
                };
                if notify && slint::invoke_from_event_loop(crate::glue::publish_light_frames).is_err() {
                    shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).wake_pending = false;
                }
        } else {
                cache.remove(&id);
                if hero { hub = ProcessInputHub::new(); last_input = now.checked_sub(neuron::pattern::LOGICAL_TICK_DT).unwrap_or(now); }
                eprintln!("neuron: discarded failed lighting preview session");
        }
        if !hero_only && hero_lane.is_none() && !hero && started.elapsed() > Duration::from_millis(15) {
            let seed = (cache.remove(&SurfaceId::Hero), std::mem::take(&mut hub), last_target.clone(), last_input);
            let lane_shared = shared.clone();
            match neuron::worker::spawn_named("neuron-light-hero", move || run_lane(lane_shared, Some(seed), true)) {
                Ok(worker) => {
                    hero_lane = Some(HeroLane { shared: shared.clone(), worker: Some(worker) });
                    #[cfg(test)] { shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).hero_isolated = true; }
                    eprintln!("neuron: isolated hero preview after a slow catalog job");
                },
                Err(error) => eprintln!("neuron: hero isolation unavailable ({error})"),
            }
        }
        evict_dormant(&mut cache);
        if !hero_only && profiling() && last_report.elapsed() >= Duration::from_secs(2) {
            let mut metrics = {
                let mut state = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let acknowledged = state.metrics.acknowledged;
                let metrics = std::mem::take(&mut state.metrics);
                state.metrics.acknowledged = acknowledged;
                metrics
            };
            eprintln!("LIGHT_PROF: catalog_p99_us={} publication_p95_us={} request_present_p95_us={} jobs={}",
                percentile(&mut metrics.render, 99), percentile(&mut metrics.publication, 95),
                percentile(&mut metrics.latency, 95), metrics.render.len());
            last_report = Instant::now();
        }
    }
}

fn sample_representative_keys(hub: &mut ProcessInputHub, previous: &mut Option<u128>, elapsed: Duration) {
    let step = elapsed.as_millis() / 400;
    if *previous == Some(step) { return; }
    *previous = Some(step);
    let key = neuron::pattern::ControlId::Key(neuron::pattern::KeyId(0x1b));
    hub.enqueue_event(key, true, elapsed);
    hub.enqueue_event(key, false, elapsed);
    let _ = hub.publish_tick();
}

/// Deterministic profiling entry point; invoked before runtime startup by NEURON_LIGHT_BENCH.
pub fn render_latency_bench() -> Result<(), String> {
    let _policy = ProviderPolicy::InjectedDeterministic.enter();
    let _quiet = neuron::capture::suppress_key_reads();
    for dims in [(1, 2), (6, 22), (12, 30)] {
        let mut timings = Vec::new();
        let mut worst = ("", 0);
        for preset in neuron::pattern::presets() {
            let now = Instant::now();
            let layers: Arc<[LayerDef]> = vec![preset.to_layer()].into();
            let mut entry = Cached::new(layers, dims, 0, now);
            let hub = ProcessInputHub::new();
            let mut pattern_times = Vec::new();
            for tick in 0..120u32 {
                let elapsed = Duration::from_millis(u64::from(tick) * 120);
                let at = now + elapsed;
                let started = Instant::now();
                let rgb = representative(preset.slug, &mut entry, &hub, at, elapsed);
                std::hint::black_box(entry.thumbnail(&rgb));
                if tick > 10 { pattern_times.push(started.elapsed().as_micros() as u64); }
            }
            let p99 = percentile(&mut pattern_times, 99);
            if p99 > worst.1 { worst = (preset.slug, p99); }
            timings.extend(pattern_times);
        }
        eprintln!("LIGHT_BENCH: dims={dims:?} catalog_p99_us={} jobs={} worst_preset={} worst_p99_us={}",
            percentile(&mut timings, 99), timings.len(), worst.0, worst.1);
    }
    for dims in [(1, 2), (6, 22), (12, 30)] {
        let defs = vec![neuron::pattern::preset_layer("fire").ok_or("fire fixture unavailable")?];
        let mut baseline = Compositor::from_defs(&defs);
        let mut scratch = neuron::pattern::RenderScratch::default();
        let target = neuron::pattern::SurfaceIdentity { pid: 0, unit: "bench".into(), plane: "base".into(), dimensions: dims };
        for mode in ["baseline", "settled", "dissolve"] {
            let mut live = neuron::pattern::LiveComposite::new(target.clone(), defs.clone()).map_err(|e| e.to_string())?;
            let mut times = Vec::new();
            for tick in 0..1200u64 {
                let t = Duration::from_millis(tick * 33);
                if mode == "dissolve" && tick % 10 == 1 {
                    let mut next = defs.clone();
                    if tick % 20 == 1 {
                        for frame in &mut next[0].spectrum.seq {
                            for stop in &mut frame.palette.stops { stop.col = Rgb::new(stop.col.b, stop.col.g, stop.col.r); }
                        }
                    }
                    live.handle().publish(next, neuron::pattern::TransitionHint::Switch)?;
                }
                let started = Instant::now();
                if mode == "baseline" {
                    baseline.render_into(dims.0, dims.1, t, &mut scratch);
                    std::hint::black_box(&scratch.output);
                } else { std::hint::black_box(live.render_cycle(t, t, t)?); }
                if tick > 100 { times.push(started.elapsed().as_nanos() as u64); }
            }
            eprintln!("LIVE_BENCH: fixture=fire dims={dims:?} mode={mode} p95_ns={} p99_ns={} samples={}",
                percentile(&mut times, 95), percentile(&mut times, 99), times.len());
        }
    }

    Ok(())
}

fn representative(slug: &str, entry: &mut Cached, hub: &ProcessInputHub, now: Instant, elapsed: Duration) -> Arc<[Rgb]> {
    let (rows, cols) = entry.dims;
    let t = elapsed.as_secs_f32();
    // The catalog should show Comet's parade on entry, including its first snapshot.
    if slug == "comet" && matches!(entry.producer, Producer::Pending) {
        entry.advance(hub, now.checked_sub(Duration::from_secs(1)).unwrap_or(now));
    }
    match slug {
        "ambient" => crate::glue::preview_ambient_frame(rows, cols, t).into(),
        "typingheat" => crate::glue::preview_thermal_frame(rows, cols, t).into(),
        "reactive" => crate::glue::preview_reactive_frame(rows, cols, t).into(),
        "ripple" => crate::glue::preview_ripple_frame(rows, cols, t).into(),
        "audiometer" => crate::glue::preview_meter_demo_frame(rows, cols, t).into(),
        "signal" => crate::glue::preview_signal_frame(rows, cols, t).into(),
        "onair" | "miclight" | "modeheld" => crate::glue::preview_indicator_frame(slug, rows, cols, t).into(),
        "vitals" => neuron::pattern::render_vitals_bounds(crate::glue::preview_vitals(t), rows, cols, neuron::pattern::Bounds::board(rows, cols), (t * 0.18).rem_euclid(1.0)).into(),
        _ => entry.render(hub, now, elapsed, ProviderPolicy::Representative),
    }
}

pub fn rasterize(frame: &[Rgb], (rows, cols): (u8, u8)) -> SharedPixelBuffer<Rgb8Pixel> {
    let (rows, cols) = (rows as usize, cols as usize);
    let (cell, gap, pad) = (6, 1, 3);
    let width = (pad * 2 + cols * cell + cols.saturating_sub(1) * gap).max(1);
    let height = (pad * 2 + rows * cell + rows.saturating_sub(1) * gap).max(1);
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(width as u32, height as u32);
    let pixels = buffer.make_mut_slice();
    pixels.fill(Rgb8Pixel { r: 4, g: 5, b: 6 });
    for row in 0..rows {
        for col in 0..cols {
            let color = frame.get(row * cols + col).copied().unwrap_or(Rgb::BLACK);
            for y in pad + row * (cell + gap)..pad + row * (cell + gap) + cell {
                let start = y * width + pad + col * (cell + gap);
                pixels[start..start + cell].fill(Rgb8Pixel { r: color.r, g: color.g, b: color.b });
            }
        }
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "performance measurement; run alone on an uncontended machine"]
    fn lighting_render_latency_bench() { render_latency_bench().unwrap(); }

    #[test]
    fn saved_reactive_thumbnails_receive_synthetic_edges_without_hardware_reads() {
        let _policy = ProviderPolicy::Representative.enter();
        let _quiet = neuron::capture::suppress_key_reads();
        let mut hub = ProcessInputHub::new();
        let mut previous = None;
        sample_representative_keys(&mut hub, &mut previous, Duration::ZERO);
        sample_representative_keys(&mut hub, &mut previous, Duration::from_millis(120));
        let (input, _) = hub.snapshot_for_matrix(0, neuron::pattern::default_control_to_matrix);
        assert_eq!(input.total_press_edges, 1);
        assert!(input.held.is_empty());
        let now = Instant::now();
        let mut cached = Cached::new(vec![neuron::pattern::preset_layer("reactive").unwrap()].into(), (1, 2), 0, now);
        let rgb = cached.render(&hub, now, Duration::ZERO, ProviderPolicy::Representative);
        assert!(rgb.iter().any(|color| *color != Rgb::BLACK));
        assert!(!neuron::pattern::may_start_provider());
    }

    #[test]
    fn comet_catalog_first_snapshot_contains_the_parade_without_live_providers() {
        let _policy = ProviderPolicy::Representative.enter();
        let _quiet = neuron::capture::suppress_key_reads();
        let now = Instant::now();
        let layers: Arc<[LayerDef]> = vec![neuron::pattern::preset_layer("comet").unwrap()].into();
        let mut cached = Cached::pending(layers, (6, 22), 0, now);
        let rgb = representative("comet", &mut cached, &ProcessInputHub::new(), now, Duration::ZERO);
        assert!(rgb.iter().any(|color| *color != Rgb::BLACK));
        assert!(!neuron::pattern::may_start_provider());
        assert!(!neuron::pattern::may_read_provider());
    }

    #[test]
    fn dormant_tiles_create_no_producers_and_hidden_windows_schedule_nothing() {
        let now = Instant::now();
        let mut cache = HashMap::new();
        let layers: Arc<[LayerDef]> = vec![neuron::pattern::preset_layer("static").unwrap()].into();
        let mut desired = DesiredSurfaceState { active: true, dims: (6, 22), tiles: vec![Tile {
            id: SurfaceId::Preset("static".into()), shelf: 0, row: 40, layers: layers.clone(),
        }].into(), ..Default::default() };
        for _ in 0..100 { assert!(select_job(&desired, &mut cache, now).0.is_none()); }
        assert!(cache.is_empty());
        desired.hero = true;
        desired.layers = layers;
        desired.active = false;
        assert!(select_job(&desired, &mut cache, now).0.is_none());
        assert!(cache.is_empty());
    }

    #[test]
    fn cache_eviction_protects_prewarming_until_demand_ends() {
        let now = Instant::now();
        let layers: Arc<[LayerDef]> = vec![neuron::pattern::preset_layer("static").unwrap()].into();
        let protected = SurfaceId::Saved("0".into());
        let mut cache = HashMap::new();
        for index in 0..=CACHE_LIMIT {
            let mut entry = Cached::new(layers.clone(), (1, 2), 0, now);
            entry.last_used = now + Duration::from_millis(index as u64);
            entry.warm = index == 0;
            cache.insert(SurfaceId::Saved(index.to_string()), entry);
        }
        evict_dormant(&mut cache);
        assert_eq!(cache.len(), CACHE_LIMIT);
        assert!(cache.contains_key(&protected));
        assert!(!cache.contains_key(&SurfaceId::Saved("1".into())));
        let desired = DesiredSurfaceState { active: true, dims: (1, 2), tiles: vec![Tile {
            id: protected.clone(), shelf: 3, row: 0, layers: layers.clone(),
        }].into(), ..Default::default() };
        assert!(select_job(&desired, &mut cache, now).0.is_none());
        assert!(!cache[&protected].warm);
        cache.insert(SurfaceId::Saved("extra".into()), Cached::new(layers, (1, 2), 0, now + Duration::from_secs(1)));
        evict_dormant(&mut cache);
        assert!(!cache.contains_key(&protected));
        for entry in cache.values_mut() { entry.visible = true; entry.warm = true; }
        let hidden = DesiredSurfaceState { active: false, ..desired };
        assert!(select_job(&hidden, &mut cache, now).0.is_none());
        assert!(cache.values().all(|entry| !entry.visible && !entry.warm));
    }

    #[test]
    fn switching_to_legacy_never_uses_empty_default_emit() {
        let now = Instant::now();
        let first: Arc<[LayerDef]> = vec![neuron::pattern::preset_layer("static").unwrap()].into();
        let mut cached = Cached::new(first, (6, 22), 0, now);
        assert!(matches!(cached.producer, Producer::Session { .. }));
        let mut legacy = neuron::pattern::preset_layer("static").unwrap();
        legacy.pattern = "heat".into();
        cached.reconcile(vec![legacy].into(), (6, 22), 0, now);
        assert!(matches!(cached.producer, Producer::Legacy(_)));
        assert_eq!(cached.render(&ProcessInputHub::new(), now, Duration::ZERO, ProviderPolicy::InjectedDeterministic).len(), 132);
    }

    #[test]
    fn half_open_demand_does_not_wake_distant_or_empty_rows() {
        let demand = DemandRange { start: 4, end: 8, warm_start: 2, warm_end: 10 };
        assert!(!demand.visible(3));
        assert!(demand.visible(4));
        assert!(!demand.visible(8));
        assert!(demand.warm(9));
        assert!(!demand.warm(10));
        assert!(!(0..128).any(|i| DemandRange::default().visible(i) || DemandRange::default().warm(i)));
    }

    #[test]
    fn identities_survive_reorder_and_edits_without_duplicate_layers() {
        let first = neuron::pattern::preset_layer("wave").unwrap();
        let second = neuron::pattern::preset_layer("static").unwrap();
        let before = identify_layers(&[], &[first.clone(), second.clone()]);
        let mut edited = first;
        edited.params.0.insert("speed".into(), 2.0);
        let after = identify_layers(&before, &[second.clone(), edited.clone()]);
        assert_eq!(after[0].id, before[1].id);
        assert_eq!(after[1].id, before[0].id);
        let appended = identify_layers(&after, &[second, edited.clone(), edited]);
        assert_ne!(appended[1].id, appended[2].id);
    }

    #[test]
    fn presentation_edits_and_structural_reorders_preserve_simulation() {
        let now = Instant::now();
        let layers = identify_layers(&[], &[neuron::pattern::preset_layer("wave").unwrap(), neuron::pattern::preset_layer("static").unwrap()]);
        let mut entry = Cached::new(layers.clone(), (6, 22), 0, now);
        let hub = ProcessInputHub::new();
        entry.advance(&hub, now + Duration::from_secs(1));
        let tick = match &entry.producer { Producer::Session { session, .. } => session.tick(), Producer::Pending | Producer::Legacy(_) => panic!() };
        let reordered = identify_layers(&layers, &[layers[1].clone(), layers[0].clone()]);
        entry.reconcile(reordered.clone(), (6, 22), 1, now + Duration::from_secs(1));
        match &entry.producer {
            Producer::Session { session, .. } => {
                assert_eq!(session.tick(), tick);
                assert_eq!(session.layer_order(), reordered.iter().map(|d| d.id.unwrap()).collect::<Vec<_>>());
            }, Producer::Pending | Producer::Legacy(_) => panic!(),
        }
    }

    #[test]
    fn inserted_identical_copy_cannot_take_a_surviving_layers_identity() {
        let old = identify_layers(&[], &[neuron::pattern::preset_layer("wave").unwrap()]);
        let mut copy = old[0].clone();
        copy.id = None;
        let after = identify_layers(&old, &[copy, old[0].clone()]);
        assert_eq!(after[1].id, old[0].id);
        assert_ne!(after[0].id, old[0].id);
    }

    #[test]
    fn adding_and_removing_legacy_layers_retains_native_fields_and_input_cursor() {
        let _quiet = neuron::capture::suppress_key_reads();
        let now = Instant::now();
        let mut comet = neuron::pattern::preset_layer("comet").unwrap();
        comet.blend = neuron::effects::Blend::Screen;
        let layers = identify_layers(&[], &[neuron::pattern::preset_layer("wave").unwrap(), comet]);
        let mut entry = Cached::new(layers.clone(), (6, 22), 0, now);
        let mut hub = ProcessInputHub::new();
        hub.enqueue_event(neuron::pattern::ControlId::Key(neuron::pattern::KeyId(0x41)), true, Duration::ZERO);
        let _ = hub.publish_tick();
        let elapsed = Duration::from_secs(1);
        let before = entry.render(&hub, now + elapsed, elapsed, ProviderPolicy::InjectedDeterministic);
        let mut added = neuron::pattern::preset_layer("fire").unwrap();
        added.enabled = false;
        let mixed = identify_layers(&layers, &[layers[0].clone(), layers[1].clone(), added]);
        entry.reconcile(mixed, (6, 22), 1, now + elapsed);
        let cursor = entry.input_cursor;
        assert_eq!(cursor, hub.current_generation());
        assert_eq!(entry.render(&hub, now + elapsed, elapsed, ProviderPolicy::InjectedDeterministic), before);
        entry.reconcile(layers, (6, 22), 2, now + elapsed);
        assert!(matches!(entry.producer, Producer::Legacy(_)));
        assert_eq!(entry.render(&hub, now + elapsed, elapsed, ProviderPolicy::InjectedDeterministic), before);
        assert_eq!(entry.input_cursor, cursor);
    }

    #[test]
    fn unchanged_frames_reuse_rgb_and_thumbnail_storage() {
        let now = Instant::now();
        let layers = identify_layers(&[], &[neuron::pattern::preset_layer("static").unwrap()]);
        let mut cached = Cached::new(layers.clone(), (6, 22), 0, now);
        let hub = ProcessInputHub::new();
        let first = cached.render(&hub, now, Duration::ZERO, ProviderPolicy::InjectedDeterministic);
        let image = cached.thumbnail(&first);
        let second = cached.render(&hub, now + Duration::from_millis(120), Duration::from_millis(120), ProviderPolicy::InjectedDeterministic);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(image.as_slice().as_ptr(), cached.thumbnail(&second).as_slice().as_ptr());
        let mut with_id = layers.to_vec();
        with_id[0].id = Some(LayerInstanceId(42));
        assert!(same_configuration(&layers, &with_id));
        with_id[0].enabled = false;
        assert!(!same_configuration(&layers, &with_id));
    }

    #[test]
    fn visible_deadlines_age_past_continuous_hero_edits() {
        let now = Instant::now();
        let layers = identify_layers(&[], &[neuron::pattern::preset_layer("static").unwrap()]);
        let tile_id = SurfaceId::Preset("static".into());
        let desired = DesiredSurfaceState { active: true, hero: true, dims: (6, 22), layers: layers.clone(),
            tiles: vec![Tile { id: tile_id.clone(), shelf: 0, row: 0, layers }].into(),
            demand: [DemandRange { start: 0, end: 1, warm_start: 0, warm_end: 1 }, DemandRange::default(), DemandRange::default(), DemandRange::default()],
            ..Default::default() };
        let mut cache = HashMap::new();
        assert_eq!(select_job(&desired, &mut cache, now).0.unwrap().0, SurfaceId::Hero);
        assert!(matches!(cache[&tile_id].producer, Producer::Pending));
        assert_eq!(select_job(&desired, &mut cache, now + Duration::from_millis(251)).0.unwrap().0, tile_id);
    }

    #[test]
    fn completion_coalescing_has_one_wake_and_never_loses_the_final_full_frame() {
        let now = Instant::now();
        let mailbox = Arc::new(Mutex::new(Shared { desired: DesiredSurfaceState::default(),
            completions: HashMap::new(), invalidated: std::collections::HashSet::new(), wake_pending: false, stopped: false, request_generation: 0,
            requested_at: now, metrics: Metrics::default(), hero_snapshot: None, hero_isolated: false }));
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let writer_mailbox = mailbox.clone();
            scope.spawn(move || {
                for revision in 1..=10000 {
                    let frame = CompletedFrame { id: SurfaceId::Hero, target: Target::default(), dims: (1, 2),
                        content_rev: revision, layout_generation: 0, rgb: vec![Rgb::new(revision as u8, 2, 3); 2].into(),
                        swatch: None, requested: now, render_time: Duration::ZERO };
                    if writer_mailbox.lock().unwrap().publish([frame]) { send.send(()).unwrap(); }
                    if revision % 7 == 0 { std::thread::yield_now(); }
                }
                send.send(()).unwrap();
            });
            let mut revision = 0;
            for () in receive {
                let frames = mailbox.lock().unwrap().take();
                for frame in frames {
                    assert!(frame.content_rev > revision);
                    revision = frame.content_rev;
                    assert_eq!(frame.rgb.as_ref(), &[Rgb::new(revision as u8, 2, 3); 2]);
                }
            }
            assert_eq!(revision, 10000);
            assert!(!mailbox.lock().unwrap().wake_pending);
        });
    }

    #[test]
    fn a_new_live_producer_rebaselines_history_but_keeps_subsequent_edges() {
        let _policy = ProviderPolicy::AuthoritativeLive.enter();
        let _quiet = neuron::capture::suppress_key_reads();
        let now = Instant::now();
        let keys: Vec<_> = (1..=255).map(|vk| neuron::pattern::ControlId::Key(neuron::pattern::KeyId(vk)))
            .filter(|key| neuron::pattern::default_control_to_matrix(*key).is_some()).collect();
        let mut hub = ProcessInputHub::new();
        for &key in &keys { hub.enqueue_event(key, true, Duration::ZERO); }
        let _ = hub.publish_tick();
        let mut legacy = neuron::pattern::preset_layer("static").unwrap();
        legacy.pattern = "heat".into();
        let mut actual = Cached::new(vec![legacy].into(), (6, 22), 0, now);
        let _ = actual.render(&hub, now, Duration::ZERO, ProviderPolicy::AuthoritativeLive);
        let mut comet = neuron::pattern::preset_layer("comet").unwrap();
        comet.params.set("density", 3.0);
        let layers: Arc<[LayerDef]> = vec![comet].into();
        let at = now + Duration::from_millis(120);
        actual.reconcile(layers.clone(), (6, 22), 1, at);
        let mut reference = Cached::new(layers, (6, 22), 1, at);
        reference.producer = Producer::Legacy(Compositor::from_defs(&reference.layers));
        let empty = ProcessInputHub::new();
        assert_eq!(actual.render(&hub, at, Duration::from_millis(120), ProviderPolicy::AuthoritativeLive),
            reference.render(&empty, at, Duration::from_millis(120), ProviderPolicy::AuthoritativeLive),
            "changing producer class cannot replay an earlier key press");
        // Comet reacts only to a head hit, so cover mapped cells while the seeded parade advances.
        let mut reacted = false;
        for tick in 7..=60 {
            let elapsed = Duration::from_millis(tick * 20);
            for &key in &keys {
                hub.enqueue_event(key, false, elapsed);
                hub.enqueue_event(key, true, elapsed);
            }
            let _ = hub.publish_tick();
            if actual.render(&hub, now + elapsed, elapsed, ProviderPolicy::AuthoritativeLive)
                != reference.render(&empty, now + elapsed, elapsed, ProviderPolicy::AuthoritativeLive) {
                reacted = true;
                break;
            }
        }
        assert!(reacted, "presses after attachment must still reach the new simulation");
    }

    #[test]
    fn reconciliation_panic_discards_partial_state_and_rebuilds_from_desired() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct FailingConfigure(Arc<AtomicBool>);
        impl neuron::pattern::Pattern for FailingConfigure {
            fn reconfigure_policy(&self, _: &neuron::pattern::PatternReconfigure<'_>) -> neuron::pattern::ReconfigurePolicy {
                neuron::pattern::ReconfigurePolicy::Preserve
            }
            fn configure(&mut self, _: &neuron::pattern::Params) {
                self.0.store(true, Ordering::SeqCst);
                panic!("injected configuration failure");
            }
        }
        let now = Instant::now();
        let failed = Arc::new(AtomicBool::new(false));
        let mut old = neuron::pattern::preset_layer("static").unwrap();
        old.pattern = "heat".into();
        let mut cached = Cached::new(vec![old.clone()].into(), (1, 2), 0, now);
        if let Producer::Legacy(comp) = &mut cached.producer {
            comp.layers[0].pattern = Box::new(FailingConfigure(failed.clone()));
        } else { panic!("heat must remain on the legacy producer"); }
        old.params.0.insert("speed".into(), 2.0);
        let desired = DesiredSurfaceState { active: true, hero: true, fps: 20, dims: (1, 2),
            content_rev: 42, layers: vec![old].into(), ..Default::default() };
        let shared = Arc::new((Mutex::new(Shared { desired, completions: HashMap::new(),
            invalidated: std::collections::HashSet::new(), wake_pending: false, stopped: false,
            request_generation: 0, requested_at: now, metrics: Metrics::default(), hero_snapshot: None,
            hero_isolated: false }), Condvar::new()));
        let lane_shared = shared.clone();
        let worker = neuron::worker::spawn_named("test-light-recovery", move ||
            run_lane(lane_shared, Some((Some(cached), ProcessInputHub::new(), Target::default(), now)), true)).unwrap();
        let recovered = loop {
            if shared.0.lock().unwrap().hero_snapshot.as_ref().is_some_and(|frame| frame.content_rev == 42 && frame.rgb.len() == 2) { break true; }
            if now.elapsed() >= Duration::from_secs(2) { break false; }
            std::thread::sleep(Duration::from_millis(5));
        };
        shared.0.lock().unwrap().stopped = true;
        shared.1.notify_all();
        worker.join().unwrap();
        assert!(failed.load(Ordering::SeqCst));
        assert!(recovered, "the worker must reconstruct the canonical configuration after a partial mutation");
    }

    #[test]
    fn slow_saved_stacks_isolate_hero_requests_and_join_both_lanes() {
        let now = Instant::now();
        let hero = identify_layers(&[], &[neuron::pattern::preset_layer("static").unwrap()]);
        let saved: Arc<[LayerDef]> = vec![neuron::pattern::preset_layer("comet").unwrap(); 1024].into();
        let desired = DesiredSurfaceState { active: true, hero: true, fps: 20, dims: (6, 22), layers: hero,
            tiles: vec![Tile { id: SurfaceId::Saved("stress".into()), shelf: 3, row: 0, layers: saved }].into(),
            demand: [DemandRange::default(), DemandRange::default(), DemandRange::default(),
                DemandRange { start: 0, end: 1, warm_start: 0, warm_end: 1 }], ..Default::default() };
        let shared = Arc::new((Mutex::new(Shared { desired, completions: HashMap::new(), invalidated: std::collections::HashSet::new(), wake_pending: false,
            stopped: false, request_generation: 0, requested_at: now, metrics: Metrics::default(),
            hero_snapshot: None, hero_isolated: false }), Condvar::new()));
        let worker_shared = shared.clone();
        let worker = neuron::worker::spawn_named("test-light-render", move || run(worker_shared)).unwrap();
        let isolated = loop {
            if shared.0.lock().unwrap().hero_isolated { break true; }
            if now.elapsed() >= Duration::from_secs(10) { break false; }
            std::thread::sleep(Duration::from_millis(5));
        };
        {
            let mut state = shared.0.lock().unwrap();
            state.desired.content_rev = 1;
            state.request_generation += 1;
        }
        shared.1.notify_all();
        let requested = Instant::now();
        let acknowledged = loop {
            if shared.0.lock().unwrap().hero_snapshot.as_ref().is_some_and(|frame| frame.content_rev == 1) { break true; }
            if requested.elapsed() >= Duration::from_secs(2) { break false; }
            std::thread::sleep(Duration::from_millis(5));
        };
        shared.0.lock().unwrap().stopped = true;
        shared.1.notify_all();
        worker.join().unwrap();
        assert!(isolated, "the pathological stack must cross the isolation threshold");
        assert!(acknowledged, "hero updates must finish independently of the catalog lane");
    }

    #[test]
    fn target_identity_includes_physical_unit_and_epoch() {
        let a = Target { pid: 0x0226, unit: "first".into(), epoch: 1 };
        assert_ne!(a, Target { unit: "second".into(), ..a.clone() });
        assert_ne!(a, Target { epoch: 2, ..a.clone() });
    }
}
