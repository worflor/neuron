// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Verification suite for Persistent Layering Trails (Light Painting) & Stateful Lighting Runtime.

use std::sync::Arc;
use std::time::{Duration, Instant};

use neuron::lighting::Rgb;
use neuron::pattern::{
    compose_comet_cell, decay_trail_field, default_control_to_matrix, Cell, Comet, ControlId,
    EmitContext, FrameContext, KeyId, LayerDef, LayerGeometry, LayerInstanceId, LightingSession,
    MatrixInputSnapshot, MatrixPress, Params, Pattern, ProcessInputHub, ReconcileStep,
    SceneLayerSpec, StepClock, LOGICAL_TICK_DT,
};
use neuron::spectrum::{self, Spectrum};

// ───────────────────────────────── Stage 1 Tests ─────────────────────────────────

#[test]
fn stepclock_zero_dt_produces_zero_steps() {
    let mut clock = StepClock::default();
    assert_eq!(clock.accrue(0.0, 24.0, 1.0), 0);
    assert_eq!(clock.accrue(-0.5, 24.0, 1.0), 0);
    assert_eq!(clock.accrue(0.0, 24.0, 2.0), 0);
}

#[test]
fn stepclock_publication_fps_independence() {
    // 1 second of simulation: 6 fps (6 ticks of 1/6 s) vs 60 fps (60 ticks of 1/60 s)
    let mut clock_6fps = StepClock::default();
    let mut steps_6fps = 0;
    for _ in 0..6 {
        steps_6fps += clock_6fps.accrue(1.0 / 6.0, 24.0, 1.0);
    }

    let mut clock_60fps = StepClock::default();
    let mut steps_60fps = 0;
    for _ in 0..60 {
        steps_60fps += clock_60fps.accrue(1.0 / 60.0, 24.0, 1.0);
    }

    assert_eq!(steps_6fps, 24, "6 fps steps over 1 second at base 24 should equal 24");
    assert_eq!(steps_60fps, 24, "60 fps steps over 1 second at base 24 should equal 24");
    assert_eq!(steps_6fps, steps_60fps);
}

#[test]
fn spectrum_at_time_precision_invariance() {
    let spec = spectrum::rainbow();
    let t_huge = Duration::from_secs(100_000);
    let col = spec.at_time(t_huge, 0.5);
    assert_ne!(col, Rgb::BLACK);

    // Verify Flow motion evaluates smoothly in f64 space
    let mut flow_spec = Spectrum::gradient(vec![Rgb::new(0xFF, 0, 0), Rgb::new(0, 0, 0xFF)]);
    flow_spec.seq[0].palette.motion = neuron::spectrum::Motion::Flow { speed: 1.0, chaos: 0.5 };

    let c1 = flow_spec.at_time(Duration::from_secs(10_000), 0.3);
    let c2 = flow_spec.at_time(Duration::from_secs(10_000) + Duration::from_millis(16), 0.3);
    assert_ne!(c1, Rgb::BLACK);
    assert_ne!(c2, Rgb::BLACK);
}

#[test]
fn global_input_hub_queues_transitions() {
    let mut hub = ProcessInputHub::new();
    let key = ControlId::Key(KeyId(0x41)); // 'A'
    hub.enqueue_event(key, true, Duration::from_millis(10));
    hub.enqueue_event(key, false, Duration::from_millis(25));
    hub.enqueue_event(key, true, Duration::from_millis(30));

    let snap = hub.publish_tick();
    assert_eq!(snap.generation, 1);
    assert_eq!(snap.fresh_edges.len(), 2, "drained 2 down transitions");
    assert_eq!(snap.fresh_edges[0].at, Duration::from_millis(10));
    assert_eq!(snap.fresh_edges[1].at, Duration::from_millis(30));
    assert_eq!(snap.down_controls, vec![key]);
}

#[test]
fn global_input_hub_preserves_edge_multiplicity_and_order() {
    let mut hub = ProcessInputHub::new();
    let key_a = ControlId::Key(KeyId(0x41));
    let key_b = ControlId::Key(KeyId(0x42));

    hub.enqueue_event(key_a, true, Duration::from_millis(10));
    hub.enqueue_event(key_a, false, Duration::from_millis(14));
    hub.enqueue_event(key_a, true, Duration::from_millis(18));
    hub.enqueue_event(key_b, true, Duration::from_millis(31));

    let _ = hub.publish_tick();

    let (snapshot, gen) = hub.snapshot_for_matrix(0, default_control_to_matrix);
    assert_eq!(gen, 1);
    assert_eq!(snapshot.pressed.len(), 3, "preserves 3 separate down edges");
    assert_eq!(snapshot.pressed[0].control, key_a);
    assert_eq!(snapshot.pressed[0].at, Duration::from_millis(10));
    assert_eq!(snapshot.pressed[1].control, key_a);
    assert_eq!(snapshot.pressed[1].at, Duration::from_millis(18));
    assert_eq!(snapshot.pressed[2].control, key_b);
    assert_eq!(snapshot.pressed[2].at, Duration::from_millis(31));
}

#[test]
fn global_input_hub_bounded_history_no_drop() {
    let mut hub = ProcessInputHub::new();
    let key_a = ControlId::Key(KeyId(0x41));
    let key_b = ControlId::Key(KeyId(0x42));

    hub.enqueue_event(key_a, true, Duration::from_millis(10));
    let _ = hub.publish_tick(); // gen 1

    hub.enqueue_event(key_a, false, Duration::from_millis(15));
    hub.enqueue_event(key_b, true, Duration::from_millis(20));
    let _ = hub.publish_tick(); // gen 2

    let _ = hub.publish_tick(); // gen 3

    // Stalled session waking up 3 generations later
    let (snapshot, gen) = hub.snapshot_for_matrix(0, default_control_to_matrix);
    assert_eq!(gen, 3);
    assert!(!snapshot.input_gap);
    assert_eq!(snapshot.pressed.len(), 2, "aggregated presses across intermediate generations");
    assert_eq!(snapshot.pressed[0].control, key_a);
    assert_eq!(snapshot.pressed[1].control, key_b);
}

#[test]
fn global_input_hub_overflow_sets_input_gap() {
    let mut hub = ProcessInputHub::new();
    let key = ControlId::Key(KeyId(0x41));

    for i in 1..=70 {
        hub.enqueue_event(key, true, Duration::from_millis(i * 10));
        let _ = hub.publish_tick();
        hub.enqueue_event(key, false, Duration::from_millis(i * 10 + 5));
    }

    let (snapshot, gen) = hub.snapshot_for_matrix(0, default_control_to_matrix);
    assert_eq!(gen, 70);
    assert!(snapshot.input_gap, "gap flagged because session stalled > 64 generations");
    assert_eq!(snapshot.pressed.len(), 64, "retains the most recent 64 generations of events");
}

#[test]
fn matrix_input_snapshot_preserves_control_identity() {
    let mut hub = ProcessInputHub::new();
    let enter = ControlId::Key(KeyId(0x0D)); // VK_RETURN
    let backspace = ControlId::Key(KeyId(0x08)); // VK_BACK

    hub.enqueue_event(enter, true, Duration::from_millis(10));
    hub.enqueue_event(backspace, true, Duration::from_millis(15));
    let _ = hub.publish_tick();

    let (snapshot, _) = hub.snapshot_for_matrix(0, default_control_to_matrix);
    assert_eq!(snapshot.pressed.len(), 2);
    assert_eq!(snapshot.pressed[0].control, enter);
    assert_eq!(snapshot.pressed[1].control, backspace);
}

#[test]
fn wildlife_rhythm_preserves_sub_200ms_timing() {
    let press1 = MatrixPress {
        control: ControlId::Key(KeyId(0x41)),
        row: 3,
        col: 5,
        at: Duration::from_millis(100),
    };
    let press2 = MatrixPress {
        control: ControlId::Key(KeyId(0x42)),
        row: 3,
        col: 6,
        at: Duration::from_millis(250), // 150 ms apart (< 200 ms)
    };

    let delta_s = (press2.at - press1.at).as_secs_f64();
    assert!(delta_s < 0.20, "rhythm interval preserved: {delta_s}s < 0.20s");
}

#[test]
fn session_discontinuity_clean_rebase_zero_dt() {
    let mut session = LightingSession::new((6, 22));
    let spec_def = LayerDef {
        pattern: "comet".into(),
        ..LayerDef::default()
    };
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: spec_def,
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let t0 = Instant::now();
    let _ = session.advance_tick(&hub, t0);

    // 10 second jump
    let t1 = t0 + Duration::from_secs(10);
    let snap = session.advance_tick(&hub, t1);
    assert_eq!(snap.dt, Duration::ZERO, "discontinuity sets effective dt = 0");
}

// ───────────────────────────────── Stage 2 Tests ─────────────────────────────────

#[test]
fn session_advance_vs_emit_purity() {
    let mut session = LightingSession::new((6, 22));
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef {
            pattern: "uniform".into(),
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let snap = session.advance_tick(&hub, Instant::now());
    let initial_tick = session.tick();

    let layer = session.layer_pattern(LayerInstanceId(1)).unwrap();
    let geom1 = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let geom2 = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(4, 10),
        region: Vec::new(),
        board_dims: (4, 10),
    };

    let emit_ctx = EmitContext { elapsed: snap.elapsed };
    let f1 = layer.emit(&emit_ctx, &geom1);
    let f2 = layer.emit(&emit_ctx, &geom2);

    assert_eq!(session.tick(), initial_tick, "emit did not advance simulation tick");
    assert_eq!(f1.len(), 6 * 22);
    assert_eq!(f2.len(), 4 * 10);
}

#[test]
fn provider_backed_patterns_emit_is_pure() {
    let mut screen = neuron::pattern::make_pattern("screen").unwrap();
    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let input = MatrixInputSnapshot::empty();
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::from_millis(16),
        dt: LOGICAL_TICK_DT,
        discontinuity: false,
        input: &input,
    };
    screen.advance(&ctx, &geom);

    let emit_ctx = EmitContext { elapsed: ctx.elapsed };
    let f1 = screen.emit(&emit_ctx, &geom);
    let f2 = screen.emit(&emit_ctx, &geom);
    assert_eq!(f1, f2, "screen emit is completely pure and idempotent");
}

#[test]
fn session_reconcile_preserves_compatible_instances() {
    let mut session = LightingSession::new((6, 22));
    let mut params = Params::default();
    params.set("speed", 1.0);
    params.set("trails", 1.0);

    let spec1 = SceneLayerSpec {
        id: LayerInstanceId(42),
        def: LayerDef {
            pattern: "comet".into(),
            params,
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec1]).unwrap());

    // Advance once so trails allocate
    let hub = ProcessInputHub::new();
    let _ = session.advance_tick(&hub, Instant::now());

    // Seed sediment marker
    {
        let comet = session.layer_pattern_mut(LayerInstanceId(42)).unwrap();
        // Downcast to comet via test accessor
        let comet_ref: &mut Comet = unsafe { &mut *(&mut **comet as *mut dyn Pattern as *mut Comet) };
        comet_ref.set_trail_cell(10, Cell::new(0.55, 0.85));
    }

    // Reconcile with updated Speed, Density, and Spectrum Stops
    let mut new_params = Params::default();
    new_params.set("speed", 2.5);
    new_params.set("density", 2.0);
    new_params.set("trails", 1.0);

    let spec2 = SceneLayerSpec {
        id: LayerInstanceId(42),
        def: LayerDef {
            pattern: "comet".into(),
            params: new_params,
            spectrum: Spectrum::solid(Rgb::new(0xFF, 0, 0)),
            ..Default::default()
        },
    };

    let plan = session.plan_reconcile(&[spec2]).unwrap();
    assert!(matches!(plan.steps[0], ReconcileStep::Preserve { .. }));
    session.commit_reconcile(plan);

    // Verify sediment survived
    let comet = session.layer_pattern(LayerInstanceId(42)).unwrap();
    // SAFETY: layer 42 was built from the "comet" pattern, so the trait object is a `Comet`.
    let comet_ref: &Comet = unsafe { &*(comet as *const dyn Pattern as *const Comet) };
    assert_eq!(comet_ref.trail_field()[10], Cell::new(0.55, 0.85), "sediment marker survived reconciliation");
}

#[test]
fn session_reconcile_atomic_error_rollback() {
    let mut session = LightingSession::new((6, 22));
    let spec_valid = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef {
            pattern: "uniform".into(),
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec_valid]).unwrap());
    assert_eq!(session.layer_order(), vec![LayerInstanceId(1)]);

    // Propose plan containing invalid pattern
    let spec_bad = SceneLayerSpec {
        id: LayerInstanceId(2),
        def: LayerDef {
            pattern: "nonexistent_pattern_foo_bar".into(),
            ..Default::default()
        },
    };

    let result = session.plan_reconcile(&[spec_bad]);
    assert!(result.is_err());
    // Running session remains untouched
    assert_eq!(session.layer_order(), vec![LayerInstanceId(1)]);
}

#[test]
fn session_reconcile_explicit_order() {
    let mut session = LightingSession::new((6, 22));
    let s1 = SceneLayerSpec {
        id: LayerInstanceId(10),
        def: LayerDef { pattern: "uniform".into(), ..Default::default() },
    };
    let s2 = SceneLayerSpec {
        id: LayerInstanceId(20),
        def: LayerDef { pattern: "axis".into(), ..Default::default() },
    };
    session.commit_reconcile(session.plan_reconcile(&[s1, s2]).unwrap());
    assert_eq!(session.layer_order(), vec![LayerInstanceId(10), LayerInstanceId(20)]);

    // Swap order
    let s1_swap = SceneLayerSpec {
        id: LayerInstanceId(10),
        def: LayerDef { pattern: "uniform".into(), ..Default::default() },
    };
    let s2_swap = SceneLayerSpec {
        id: LayerInstanceId(20),
        def: LayerDef { pattern: "axis".into(), ..Default::default() },
    };
    session.commit_reconcile(session.plan_reconcile(&[s2_swap, s1_swap]).unwrap());
    assert_eq!(session.layer_order(), vec![LayerInstanceId(20), LayerInstanceId(10)]);
}

#[test]
fn session_toggle_off_then_on_clears_canvas() {
    let mut comet = Comet::default();
    let mut p_on = Params::default();
    p_on.set("trails", 1.0);
    comet.configure(&p_on);

    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let input = MatrixInputSnapshot::empty();
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::from_millis(16),
        dt: LOGICAL_TICK_DT,
        discontinuity: false,
        input: &input,
    };
    comet.advance(&ctx, &geom);
    comet.set_trail_cell(5, Cell::new(0.5, 0.5));
    assert!(!comet.trail_field().is_empty());

    // Toggle OFF
    let mut p_off = Params::default();
    p_off.set("trails", 0.0);
    comet.configure(&p_off);
    assert!(comet.trail_field().is_empty(), "toggling off empties the substrate");

    // Toggle ON
    comet.configure(&p_on);
    comet.advance(&ctx, &geom);
    assert_eq!(comet.trail_field()[5], Cell::default(), "canvas begins fresh");
}

#[test]
fn session_layer_paused_vs_occluded_advancement() {
    let mut session = LightingSession::new((6, 22));
    let def = LayerDef {
        pattern: "axis".into(),
        enabled: false, // paused
        ..Default::default()
    };
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def,
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let _ = session.advance_tick(&hub, Instant::now());
    // Layer was disabled, cached field is empty
    let snap = session.latest_sim_snapshot().unwrap();
    assert_eq!(snap.layer_fields[0].1.len(), 6 * 22);
}

#[test]
fn session_command_reset_layer() {
    let mut session = LightingSession::new((6, 22));
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef {
            pattern: "comet".into(),
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    assert!(session.reset_layer(LayerInstanceId(1)).is_ok());
    assert!(session.reset_layer(LayerInstanceId(999)).is_err());
}

// ───────────────────────────────── Stage 3 Tests ─────────────────────────────────

#[test]
fn preview_read_is_idempotent() {
    let mut session = LightingSession::new((6, 22));
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef {
            pattern: "uniform".into(),
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let _ = session.advance_tick(&hub, Instant::now());

    let f1 = session.resolve_presentation(Duration::from_millis(50));
    let f2 = session.resolve_presentation(Duration::from_millis(50));
    assert_eq!(f1.pixels, f2.pixels);
}

#[test]
fn spectrum_recolor_reuses_cached_field() {
    let mut session = LightingSession::new((6, 22));
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef {
            pattern: "uniform".into(),
            spectrum: Spectrum::solid(Rgb::new(0xFF, 0, 0)),
            ..Default::default()
        },
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let _ = session.advance_tick(&hub, Instant::now());

    let sim_before = session.latest_sim_snapshot().unwrap();
    let f1 = session.resolve_presentation(Duration::ZERO);
    assert_eq!(f1.pixels[0], Rgb::new(0xFF, 0, 0));

    // Recolor to blue
    let mut pres = session.presentation().clone();
    pres.layers[0].spectrum = Spectrum::solid(Rgb::new(0, 0, 0xFF));
    pres.revision += 1;
    session.set_presentation(pres);

    let f2 = session.resolve_presentation(Duration::ZERO);
    assert_eq!(f2.pixels[0], Rgb::new(0, 0, 0xFF));

    let sim_after = session.latest_sim_snapshot().unwrap();
    assert_eq!(sim_before.tick, sim_after.tick);
    assert!(Arc::ptr_eq(&sim_before.layer_fields[0].1, &sim_after.layer_fields[0].1));
}

#[test]
fn resolved_frame_identifies_presentation_elapsed() {
    let mut session = LightingSession::new((6, 22));
    let spec = SceneLayerSpec {
        id: LayerInstanceId(1),
        def: LayerDef::default(),
    };
    session.commit_reconcile(session.plan_reconcile(&[spec]).unwrap());

    let hub = ProcessInputHub::new();
    let _ = session.advance_tick(&hub, Instant::now());

    let rf = session.resolve_presentation(Duration::from_secs(1234));
    assert_eq!(rf.presentation_elapsed, Duration::from_secs(1234));
}

// ───────────────────────────────── Stage 4 Tests ─────────────────────────────────

#[test]
fn comet_classic_baseline_byte_identical() {
    let mut comet = Comet::default();
    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let input = MatrixInputSnapshot::empty();
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::from_millis(16),
        dt: LOGICAL_TICK_DT,
        discontinuity: false,
        input: &input,
    };
    comet.advance(&ctx, &geom);

    let emit_ctx = EmitContext { elapsed: ctx.elapsed };
    let field = comet.emit(&emit_ctx, &geom);
    assert_eq!(field.len(), 6 * 22);
}

#[test]
fn comet_trails_default_path_stays_unallocated() {
    let comet = Comet::default();
    assert!(!comet.trails_enabled());
    assert!(comet.trail_field().is_empty());
}

#[test]
fn comet_trails_lazy_allocation_on_first_advance() {
    let mut comet = Comet::default();
    let mut p = Params::default();
    p.set("trails", 1.0);
    comet.configure(&p);
    assert!(comet.trail_field().is_empty(), "zero heap allocation in configure");

    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let input = MatrixInputSnapshot::empty();
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::from_millis(16),
        dt: LOGICAL_TICK_DT,
        discontinuity: false,
        input: &input,
    };
    comet.advance(&ctx, &geom);
    assert_eq!(comet.trail_field().len(), 6 * 22, "lazy allocation on first advance");
}

#[test]
fn comet_trails_zero_sized_board_is_safe() {
    let mut comet = Comet::default();
    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(0, 0),
        region: Vec::new(),
        board_dims: (0, 0),
    };
    let input = MatrixInputSnapshot::empty();
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::ZERO,
        dt: Duration::ZERO,
        discontinuity: false,
        input: &input,
    };
    comet.advance(&ctx, &geom);
    let emit_ctx = EmitContext { elapsed: Duration::ZERO };
    let field = comet.emit(&emit_ctx, &geom);
    assert!(field.is_empty());
}

#[test]
fn comet_trail_exposure_partition_invariance() {
    // Applying exposure in 1 step vs 2 steps
    fn apply_exp(cell: &mut Cell, exposure: f32) {
        let old_i = cell.intensity;
        let old_u = cell.u;
        let alpha = -(-exposure).exp_m1();
        let gain = (1.0 - old_i) * alpha;
        let new_i = old_i + gain;
        if gain > 0.0 && new_i > 0.0 {
            let deposit_heat = 0.10 + 0.55 * (old_i + new_i);
            let new_u = (old_i * old_u + gain * deposit_heat) / new_i;
            *cell = Cell::new(new_u, new_i);
        }
    }

    let mut c1 = Cell::default();
    apply_exp(&mut c1, 0.40);

    let mut c2 = Cell::default();
    apply_exp(&mut c2, 0.20);
    apply_exp(&mut c2, 0.20);

    assert!((c1.intensity - c2.intensity).abs() < 1e-5, "intensity partition invariance");
    assert!((c1.u - c2.u).abs() < 1e-5, "heat u partition invariance");
}

#[test]
fn comet_trail_knot_saturation_heat() {
    let mut cell = Cell::default();
    fn apply_exp(cell: &mut Cell, exposure: f32) {
        let old_i = cell.intensity;
        let old_u = cell.u;
        let alpha = -(-exposure).exp_m1();
        let gain = (1.0 - old_i) * alpha;
        let new_i = old_i + gain;
        if gain > 0.0 && new_i > 0.0 {
            let deposit_heat = 0.10 + 0.55 * (old_i + new_i);
            let new_u = (old_i * old_u + gain * deposit_heat) / new_i;
            *cell = Cell::new(new_u, new_i);
        }
    }

    // Repeated passes until saturated
    for _ in 0..10 {
        apply_exp(&mut cell, 0.5);
    }
    assert!(cell.intensity > 0.95);
    assert!(cell.u >= 0.55 && cell.u <= 0.66, "knot saturation reaches high-u knot around 0.65");
}

#[test]
fn comet_decay_matches_exponential() {
    let mut trail = vec![Cell::new(0.5, 1.0)];
    decay_trail_field(&mut trail, 30.0);
    let expected = 1.0 * (-1.0f32).exp();
    assert!((trail[0].intensity - expected).abs() < 1e-4);

    // Decay past floor
    decay_trail_field(&mut trail, 150.0);
    assert_eq!(trail[0], Cell::default());
}

#[test]
fn comet_screen_composition_hierarchy() {
    let live = Cell::new(0.9, 0.8);
    let sediment = Cell::new(0.3, 0.4);
    let composed = compose_comet_cell(live, sediment, 0.0);

    // Screen blend intensity: 0.8 + 0.4 - 0.8*0.4 = 0.88
    let expected_i = 0.8 + 0.4 - 0.8 * 0.4;
    assert!((composed.intensity - expected_i).abs() < 1e-4);
    assert_eq!(composed.u, 0.9, "active meteor overrides sediment u");

    // Burst overrides
    let burst_cell = compose_comet_cell(live, sediment, 1.5);
    assert_eq!(burst_cell.intensity, 1.0);
    assert_eq!(burst_cell.u, 1.0);
}

#[test]
fn comet_teleports_never_paint() {
    let mut comet = Comet::default();
    let mut p = Params::default();
    p.set("trails", 1.0);
    comet.configure(&p);

    let geom = LayerGeometry {
        bounds: neuron::pattern::Bounds::board(6, 22),
        region: Vec::new(),
        board_dims: (6, 22),
    };
    let input = MatrixInputSnapshot::empty();
    // Advance with dt = 0: spawn occurs, but zero swept motion occurs
    let ctx = FrameContext {
        session_epoch: 0,
        tick: 1,
        elapsed: Duration::ZERO,
        dt: Duration::ZERO,
        discontinuity: false,
        input: &input,
    };
    comet.advance(&ctx, &geom);

    // Initial spawn must not paint any swept line
    assert!(
        comet.trail_field().iter().all(|c| c.intensity == 0.0),
        "initial spawn produces zero swept segments"
    );
}

