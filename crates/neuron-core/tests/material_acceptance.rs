// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Acceptance tests for Lighting Engine v3.1 material responses, physical skeuomorphism,
//! spectral interpolation continuity, and zero-reset compositor updates.

use neuron::lighting::Rgb;
use neuron::pattern::{
    Compositor, LayerDef, Params,
};
use neuron::spectrum::{
    BoundaryMode, Interp, Motion, Palette, Spectrum, Stop,
};

#[test]
fn test_zero_energy_cold_thermal_emits_black() {
    // Thermal layer with arbitrary colourful spectrum — when cold, it MUST emit 0 light (Rgb::BLACK).
    let def = LayerDef {
        pattern: "thermal".into(),
        spectrum: Spectrum::gradient(vec![
            Rgb::new(255, 0, 0),
            Rgb::new(0, 255, 0),
            Rgb::new(0, 0, 255),
        ]),
        ..Default::default()
    };
    let mut comp = Compositor::from_defs(&[def]);
    let frame = comp.render(6, 22, 0.0);
    assert_eq!(frame.len(), 6 * 22);
    for (idx, px) in frame.iter().enumerate() {
        assert_eq!(
            *px,
            Rgb::BLACK,
            "Cold thermal cell at index {} must emit Rgb::BLACK, found {:?}",
            idx,
            px
        );
    }
}

#[test]
fn test_css_color4_powerless_hue_interpolation() {
    // Interpolating between saturated blue and pure white in HSV must retain blue hue
    // and never wander through green (the historical 150° green drift of achromatic stops).
    let blue = Rgb::new(0, 0, 255);
    let white = Rgb::new(255, 255, 255);
    let mut pal = Palette::new(
        vec![Stop::new(blue, 0.0), Stop::new(white, 1.0)],
        Motion::Hold,
    );
    pal.interp = Interp::Hsv;

    for step in 1..9 {
        let u = step as f32 / 10.0;
        let c = pal.sample(u);
        assert!(
            c.b >= c.g,
            "CSS Color 4 powerless hue violation: at u={}, Blue ({}) was less than Green ({}) in {:?}",
            u,
            c.b,
            c.g,
            c
        );
        assert!(
            c.b >= c.r,
            "CSS Color 4 powerless hue violation: at u={}, Blue ({}) was less than Red ({}) in {:?}",
            u,
            c.b,
            c.r,
            c
        );
    }
}

#[test]
fn test_oklab_interpolation_smoothness() {
    // Perceptually uniform Oklab interpolation between red and blue.
    let red = Rgb::new(255, 0, 0);
    let blue = Rgb::new(0, 0, 255);
    let mut pal = Palette::new(
        vec![Stop::new(red, 0.0), Stop::new(blue, 1.0)],
        Motion::Hold,
    );
    pal.interp = Interp::Oklab;

    let mid = pal.sample(0.5);
    // Midpoint in Oklab for red-blue is a vibrant purple where both red and blue dominate.
    assert!(mid.r > 120, "Expected mid.r > 120, found {}", mid.r);
    assert!(mid.b > 120, "Expected mid.b > 120, found {}", mid.b);
    assert!(mid.r > mid.g, "Red ({}) should be greater than green ({})", mid.r, mid.g);
    assert!(mid.b > mid.g, "Blue ({}) should be greater than green ({})", mid.b, mid.g);
}

#[test]
fn test_spectrum_boundary_modes() {
    let pal = Palette::new(
        vec![
            Stop::new(Rgb::new(0, 0, 0), 0.0),
            Stop::new(Rgb::new(100, 100, 100), 1.0),
        ],
        Motion::Hold,
    );

    // Clamp: u >= 1.0 clamps to 1.0, u <= 0.0 clamps to 0.0
    assert_eq!(pal.sample_with_boundary(1.5, BoundaryMode::Clamp), Rgb::new(100, 100, 100));
    assert_eq!(pal.sample_with_boundary(-0.5, BoundaryMode::Clamp), Rgb::new(0, 0, 0));

    // PingPong: 1.2 folds to 0.8; -0.3 folds to 0.3
    let p_1_2 = pal.sample_with_boundary(1.2, BoundaryMode::PingPong);
    let p_0_8 = pal.sample(0.8);
    assert_eq!(p_1_2, p_0_8);

    let p_neg = pal.sample_with_boundary(-0.3, BoundaryMode::PingPong);
    let p_pos = pal.sample(0.3);
    assert_eq!(p_neg, p_pos);

    // Wrap: 1.2 wraps to 0.2; -0.3 wraps to 0.7
    let w_1_2 = pal.sample_with_boundary(1.2, BoundaryMode::Wrap);
    let w_0_2 = pal.sample(0.2);
    assert_eq!(w_1_2, w_0_2);

    let w_neg = pal.sample_with_boundary(-0.3, BoundaryMode::Wrap);
    let w_pos = pal.sample(0.7);
    assert_eq!(w_neg, w_pos);
}

#[test]
fn test_compositor_in_place_update_defs() {
    let mut params = Params::default();
    params.set("speed", 1.0);

    let def1 = LayerDef {
        pattern: "comet".into(),
        params: params.clone(),
        spectrum: Spectrum::solid(Rgb::new(255, 0, 0)),
        ..Default::default()
    };

    let mut comp = Compositor::from_defs(&[def1]);
    let f1 = comp.render(6, 22, 0.1);
    assert_eq!(f1.len(), 6 * 22);

    // Update in-place to green spectrum with speed 2.0
    params.set("speed", 2.0);
    let def2 = LayerDef {
        pattern: "comet".into(),
        params,
        spectrum: Spectrum::solid(Rgb::new(0, 255, 0)),
        ..Default::default()
    };

    comp.update_defs(&[def2]);
    let f2 = comp.render(6, 22, 0.2);
    assert_eq!(f2.len(), 6 * 22);
}

#[test]
fn test_independent_emitter_linear_light_accumulation() {
    // Two half-intensity linear red emitters sum to full-intensity in linear light.
    let e1 = [0.5f32, 0.0, 0.0];
    let e2 = [0.5f32, 0.0, 0.0];
    let summed = [e1[0] + e2[0], e1[1] + e2[1], e1[2] + e2[2]];
    let rgb = Rgb::from_linear(summed);
    assert_eq!(rgb.r, 255);
    assert_eq!(rgb.g, 0);
    assert_eq!(rgb.b, 0);
}
