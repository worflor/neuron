// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! README image harness. Inert unless `NEURON_SHOT_OUT` is set.
//!
//! Regenerate: run one test per process, each with an empty `NEURON_RUN_DIR` (no real config is
//! read), `SLINT_BACKEND=winit-software` and `SLINT_SCALE_FACTOR=2` for the window scenes:
//! `NEURON_SHOT_SCENE=<device|lighting|input|weave|system|profiles> NEURON_SHOT_OUT=<png>
//! cargo test -p neuron-app --bin neuron-app -- --ignored --exact readme_shots::shot`.
//! `NEURON_SHOT_GRADIENT=1` renders a three-stop Uniform palette in the lighting scene.
//! `overlay_radial`, `overlay_glyph` and `notif_cards` dump the real overlay renderer's frames into the
//! directory named by `NEURON_SHOT_OUT`. Seeded state is demo data; devices are phantoms. The
//! `docs/media` images are these renders composed over a wallpaper crop with Pillow (not committed).

#![cfg(test)]

use crate::ui::{AppRuleRow, AppWindow, GlyphChip, MacroBlock, MacroCard, BadgeView, ProfileRow, RadialSector, RuleRow, State};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

/// Two phantom Razer devices so the real scan, selection and lighting glue run end to end.
fn mock_bus() {
    use neuron::transport::mock::{MockBackend, MockDevice};
    let mut bus = MockBackend::new();
    let mut naga = MockDevice::razer(0x00A8, "Razer Naga V2 Pro")
        .answering(0x00, 0x81, &[1, 2])
        .answering(0x00, 0x85, &[1])
        .answering(0x04, 0x85, &[0, 0x06, 0x40, 0x06, 0x40]);
    naga.info.usage_page = 0x0001;
    naga.info.usage = 0x0002;
    bus.with(naga);
    let mut kb = MockDevice::razer(0x0221, "Razer BlackWidow Chroma V2").answering(0x00, 0x81, &[2, 0]);
    kb.info.usage_page = 0x0001;
    kb.info.usage = 0x0002;
    bus.with(kb);
    std::mem::forget(neuron::transport::install_backend(std::sync::Arc::new(bus)));
}

fn shared(s: &str) -> slint::SharedString {
    s.into()
}

fn rule(control: &str, emblem: &str, device: &str, action: &str, kind: &str, layer: &str) -> RuleRow {
    RuleRow {
        trigger: format!("{device} \u{b7} {control}").into(),
        control: control.into(),
        emblem: emblem.into(),
        device: device.into(),
        pid: "".into(),
        action: action.into(),
        layer: layer.into(),
        kind: kind.into(),
        removable: true,
    }
}

fn model<T: Clone + 'static>(v: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(v))
}

#[test]
#[ignore = "Requires a GUI display and an explicit screenshot destination"]
fn shot() {
    let Some(out) = env("NEURON_SHOT_OUT") else { return };
    let scene = env("NEURON_SHOT_SCENE").unwrap_or_default();
    let w: f32 = env("NEURON_SHOT_W").and_then(|s| s.parse().ok()).unwrap_or(1240.0);
    let h: f32 = env("NEURON_SHOT_H").and_then(|s| s.parse().ok()).unwrap_or(800.0);

    mock_bus();
    let app = AppWindow::new().unwrap();
    app.window().set_size(slint::LogicalSize::new(w, h));
    app.show().unwrap();
    let weak = app.as_weak();
    slint::Timer::single_shot(Duration::from_millis(300), move || {
        let app = weak.upgrade().unwrap();
        std::mem::forget(crate::glue::install(&app));
        app.global::<State>().invoke_refresh_devices();
        let weak2 = app.as_weak();
        let scene2 = scene.clone();
        slint::Timer::single_shot(Duration::from_millis(600), move || {
            let app = weak2.upgrade().unwrap();
            select_and_open(&app, &scene2);
            let weak3 = app.as_weak();
            let scene3 = scene2.clone();
            slint::Timer::single_shot(Duration::from_millis(900), move || {
                let app = weak3.upgrade().unwrap();
                finish_seed(&app, &scene3);
                let weak4 = app.as_weak();
                let frames: u32 = env("NEURON_SHOT_FRAMES").and_then(|s| s.parse().ok()).unwrap_or(0);
                if frames > 0 {
                    // an animation take: N snapshots, one every NEURON_SHOT_STEP_MS
                    let step: u64 = env("NEURON_SHOT_STEP_MS").and_then(|s| s.parse().ok()).unwrap_or(100);
                    let n = std::cell::Cell::new(0u32);
                    let t = slint::Timer::default();
                    t.start(slint::TimerMode::Repeated, Duration::from_millis(step), move || {
                        let app = weak4.upgrade().unwrap();
                        save_snapshot(&app, &format!("{out}_{:03}.png", n.get()));
                        n.set(n.get() + 1);
                        if n.get() >= frames {
                            slint::quit_event_loop().unwrap();
                        }
                    });
                    std::mem::forget(t);
                } else {
                    slint::Timer::single_shot(Duration::from_millis(900), move || {
                        let app = weak4.upgrade().unwrap();
                        save_snapshot(&app, &out);
                        slint::quit_event_loop().unwrap();
                    });
                }
            });
        });
    });
    app.run().unwrap();
}

fn save_snapshot(app: &AppWindow, path: &str) {
    if env("NEURON_SHOT_SCENE").as_deref() == Some("lighting") && env("NEURON_SHOT_GRADIENT").is_some() {
        let st = app.global::<State>();
        assert_eq!(st.get_light_stops().row_count(), 3, "gradient must survive device restoration and persistence");
        let pixels = st.get_grid_px();
        let first = pixels.row_data(0).unwrap();
        assert!((1..pixels.row_count()).any(|i| pixels.row_data(i).unwrap() != first), "preview must contain a gradient");
    }
    let shot = app.window().take_snapshot().unwrap();
    let rgb: Vec<u8> = shot.as_bytes().chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    image::save_buffer(path, &rgb, shot.width(), shot.height(), image::ColorType::Rgb8).unwrap();
}

fn device_index(app: &AppWindow, needle: &str) -> i32 {
    let st = app.global::<State>();
    let rows = st.get_devices();
    (0..rows.row_count())
        .find(|&i| rows.row_data(i).is_some_and(|r| r.name.contains(needle)))
        .map_or(0, |i| i as i32)
}

fn select_and_open(app: &AppWindow, scene: &str) {
    let st = app.global::<State>();
    let (page, view) = match scene {
        "device" | "profiles" => (0, 0),
        "lighting" => (1, 0),
        "input" | "macro" | "controllers" => (2, 0),
        "weave" => (2, 1),
        "system" => (3, 0),
        _ => (scene.parse().unwrap_or(0), 0),
    };
    let dev = if scene == "lighting" { "BlackWidow" } else { "Naga" };
    st.invoke_select_device(device_index(app, dev));
    st.set_page(page);
    st.set_input_view(view);
    if scene == "lighting" {
        st.set_window_shown(true);
        let gradient = env("NEURON_SHOT_GRADIENT").is_some();
        st.invoke_pick_tile(if gradient { "static" } else { "aurora" }.into());
        let weak = app.as_weak();
        let ticker = slint::Timer::default();
        ticker.start(slint::TimerMode::Repeated, Duration::from_millis(80), move || {
            if let Some(app) = weak.upgrade() {
                app.global::<State>().invoke_preview_tick();
            }
        });
        std::mem::forget(ticker);
    }
}

/// Demo state layered over whatever the real glue produced. Applied last so a background scan or
/// seed cannot overwrite it before the snapshot.
fn finish_seed(app: &AppWindow, scene: &str) {
    let st = app.global::<State>();
    if scene == "lighting" && env("NEURON_SHOT_GRADIENT").is_some() {
        // Device selection restores saved lighting asynchronously; edit only after its readouts settle.
        assert_ne!(st.get_idle_readout().as_str(), "…");
        st.invoke_pick_tile("static".into());
        st.invoke_add_stop(0.5);
        st.invoke_add_stop(0.5);
        let count = st.get_light_stops().row_count();
        assert_eq!(count, 3);
        st.invoke_set_stop_color(0, "FF3D56".into());
        st.invoke_set_stop_color((count - 1) as i32, "45E0B5".into());
        st.invoke_set_stop_color(1, "668CFF".into());
        st.invoke_move_stop((count - 1) as i32, 1.0);
        st.invoke_move_stop(1, 0.5);
    }
    // readouts a phantom bus cannot answer: plausible and consistent with the device defs
    let rows = st.get_devices();
    let mut keep = Vec::new();
    for i in 0..rows.row_count() {
        let Some(mut r) = rows.row_data(i) else { continue };
        if r.name.contains("Naga") {
            r.dpi = "1600".into();
            r.polling = "1000 Hz".into();
            r.brightness = "75%".into();
            r.battery = "82%".into();
            r.battery_frac = 0.82;
            r.storage = "68% free".into();
            r.firmware = "v1.2".into();
            r.plate = "12-button".into();
            keep.push(r);
        } else if r.name.contains("BlackWidow") {
            r.brightness = "60%".into();
            r.firmware = "v2.0".into();
            keep.push(r);
        }
    }
    // Audio endpoints are the host machine's own; show one demo mic row instead.
    let mut mic = crate::ui::DeviceRow::default();
    mic.name = "Razer Seiren V3 Mini".into();
    mic.detail = "microphone \u{b7} 100%".into();
    mic.icon = "mic".into();
    mic.kind = "mic".into();
    mic.id = "demo-mic".into();
    mic.connected = true;
    mic.battery_frac = -1.0;
    keep.push(mic);
    let sel = st.get_selected_device();
    st.set_devices(model(keep));
    st.set_selected_device(sel.clamp(0, 1));
    if scene != "lighting" {
        st.set_dpi(1600.0);
        st.set_polling_hz(1000.0);
        st.set_brightness(75.0);
        st.set_dpi_stages("800/1600/3200".into());
        st.set_dpi_active_stage(1);
        st.set_selected_plate("12-button".into());
    }
    st.set_active_profile("desk".into());
    st.set_status_line("ready".into());
    st.set_feel_base_dpi(1600.0);
    st.set_feel_base_polling(1000.0);
    st.set_feel_base_brightness(75.0);
    st.set_feel_base_stages("800/1600/3200".into());
    st.set_feel_base_active_stage(1);
    if scene == "device" {
        st.set_feel_adv_open(true);
    }

    let naga = "Razer Naga V2 Pro";
    st.set_rules(model(vec![
        rule("side 1", "mouse", naga, "press [ctrl+c]", "input", "base"),
        rule("side 2", "mouse", naga, "press [ctrl+v]", "input", "base"),
        rule("side 5", "mouse", naga, "sniper (hold \u{2192} 400 DPI)", "input", "base"),
        rule("side 12", "mouse", naga, "profile -> fps", "input", "base"),
        rule("mic tap", "mic", "Razer Seiren V3 Mini", "output flip", "mic", "base"),
        rule("forza.exe", "", "focus", "profile -> racing", "app", "base"),
    ]));
    st.set_editable_count(0);
    st.set_hypershift_rules(model(vec![
        rule("side 1", "mouse", naga, "media play/pause", "input", "hypershift"),
        rule("side 2", "mouse", naga, "media next", "input", "hypershift"),
    ]));
    st.set_hypershift_hold_label(shared("Naga \u{b7} side 12"));
    st.set_hypershift_hold_ready(true);

    let sec = |l: &str, a: &str| RadialSector { label: l.into(), action: a.into() };
    st.set_radial_sectors(8);
    st.set_radial_items(model(vec![
        sec("volume", "output vol +5%"),
        sec("teleport", "teleport"),
        sec("board", "whiteboard"),
        sec("mic", "mic mute [toggle]"),
        sec("profile", "profile cycle next"),
        sec("summon", "summon [code]"),
        sec("media", "media play/pause"),
        sec("pin", "pin (hovered)"),
    ]));
    let g = |n: &str, a: &str| GlyphChip { name: n.into(), action: a.into(), bound: !a.is_empty() };
    st.set_glyphs(model(vec![
        g("loop", "teleport"),
        g("bolt", "whiteboard"),
        g("tick", "press [enter]"),
        g("caret", "profile -> fps"),
        g("arc", "control center"),
        g("wave", ""),
    ]));
    if scene == "controllers" {
        let rp = |control: &str, emblem: &str, device: &str, pid: &str, action: &str| {
            let mut r = rule(control, emblem, device, action, "input", "base");
            r.pid = pid.into();
            if emblem.is_empty() {
                r.trigger = control.into();
                r.device = "".into();
            }
            r
        };
        let rows = vec![
            rp("Pad south", "pad", "couch pad", "543a", "press [enter]"),
            rp("L stick \u{2192}", "pad", "couch pad", "543a", "media next"),
            rp("Mouse 4 (thumb 1)", "mouse", "Razer Naga V2 Pro", "00a7", "teleport"),
            rp("Macro M1", "keyboard", "BlackWidow", "0221", "macro (3 steps)"),
            rp("Volume Up", "headset", "Kraken", "0527", "output vol +5%"),
            rp("F13", "", "", "", "profile -> fps"),
        ];
        st.set_editable_count(rows.len() as i32);
        st.set_rules(model(rows));
        st.set_editing_rule_hyper(false);
        st.set_editing_rule(0);
        st.set_bind_trigger_ready(true);
        st.set_bind_trigger_label("Pad south".into());
        st.set_bind_trigger_badge(BadgeView {
            pid: "543a".into(),
            emblem: "pad".into(),
            name: "couch pad".into(),
            own: "PowerA Xbox Series X Controller".into(),
            custom_emblem: false,
            custom_name: true,
        });
        // open the badge drawer: the "couch pad" chip under the trigger well (logical px, 1180x820 window)
        let weak = app.as_weak();
        slint::Timer::single_shot(Duration::from_millis(250), move || {
            if let Some(app) = weak.upgrade() {
                use slint::platform::{PointerEventButton, WindowEvent};
                let position = slint::LogicalPosition::new(80.0, 373.0);
                app.window().dispatch_event(WindowEvent::PointerMoved { position });
                app.window().dispatch_event(WindowEvent::PointerPressed { position, button: PointerEventButton::Left });
                app.window().dispatch_event(WindowEvent::PointerReleased { position, button: PointerEventButton::Left });
            }
        });
    }
    if scene == "macro" {
        // the product default after launch: input armed, so the run button reads "run for real"
        st.set_input_armed(true);
        let b = |row: &str, path: &str, depth: i32, kind: &str, verb: &str, value: &str, first: bool, last: bool| MacroBlock {
            row: row.into(),
            path: path.into(),
            depth,
            kind: kind.into(),
            verb: verb.into(),
            value: value.into(),
            first,
            last,
        };
        st.set_macro_name("paste_clean".into());
        st.set_macro_code_view(false);
        st.set_macro_blocks(model(vec![
            b("step", "0", 0, "hotkey", "press", "ctrl+c", true, false),
            b("step", "1", 0, "ask", "ask", "strip the formatting?", false, true),
            b("lane", "1", 1, "yes", "", "", false, false),
            b("step", "1.yes.0", 1, "hotkey", "press", "ctrl+shift+v", true, true),
            b("add", "1.yes", 1, "", "", "", false, false),
            b("lane", "1", 1, "no", "", "", false, false),
            b("step", "1.no.0", 1, "notify", "notify", "pasted as copied", true, true),
            b("add", "1.no", 1, "", "", "", false, false),
            b("add", "", 0, "", "", "", false, false),
        ]));
        let c = |n: &str, sum: &str, steps: i32, mode: &str, trig: &str| MacroCard {
            name: n.into(),
            summary: sum.into(),
            steps,
            options: 0,
            mode: mode.into(),
            trigger: trig.into(),
        };
        st.set_macro_catalog(model(vec![
            c("paste_clean", "copies, asks, then pastes plain or as copied", 4, "BOUND", "Naga \u{b7} side 2"),
            c("standup_timer", "notifies you every 45 minutes", 2, "BOUND", "(unbound)"),
            c("obs_scene_swap", "drives OBS through the brokered surface", 3, "RAW", "Naga \u{b7} side 12"),
        ]));
    }
    if scene == "weave" {
        let n = 90;
        let mut trail = Vec::new();
        for i in 0..=n {
            let a = -std::f32::consts::FRAC_PI_2 + i as f32 / n as f32 * std::f32::consts::TAU * 0.95;
            trail.push(0.5 + 0.13 * a.cos());
            trail.push(0.5 + 0.40 * a.sin());
        }
        st.set_trail(model(trail));
        st.set_gesture_status("recognised \u{b7} loop".into());
        st.set_gesture_predict("loop".into());
        st.set_radial_preview_sector(1);
    }
    if scene == "profiles" {
        let p = |n: &str, dpi: &str, poll: &str, br: &str, lt: &str, binds: i32, active: bool| ProfileRow {
            name: n.into(),
            summary: "".into(),
            dpi: dpi.into(),
            polling: poll.into(),
            brightness: br.into(),
            lighting: lt.into(),
            idle: "".into(),
            in_game: "".into(),
            gaming: n == "fps",
            active,
            binds,
            broken: false,
            why: "".into(),
        };
        st.set_profiles(model(vec![
            p("desk", "800/1600/3200", "1000 Hz", "75%", "2 fx", 6, true),
            p("fps", "800", "1000 Hz", "40%", "1 fx", 0, false),
            p("racing", "1600", "500 Hz", "60%", "custom", 0, false),
        ]));
        st.set_profile_names(model(vec![shared("desk"), shared("fps"), shared("racing")]));
        st.set_app_rules(model(vec![
            AppRuleRow { app: "valorant".into(), profile: "fps".into(), dangling: false },
            AppRuleRow { app: "forza".into(), profile: "racing".into(), dangling: false },
        ]));
        // the sheet's active chip elides long names; without one the ACTIVE card still shows the state
        st.set_active_profile("\u{2014}".into());
        st.set_profile_sheet_open(true);
    }
    crate::binding_list::refresh(app, false);
    crate::binding_list::refresh(app, true);
}

// Overlay instruments: the real renderer, tapped where its pixels leave for the screen.

#[cfg(windows)]
fn wedge(
    glyph: crate::overlay::WedgeGlyph,
    title: &str,
    value: Option<&str>,
    tone: crate::overlay::Tone,
    meter: Option<f32>,
) -> crate::overlay::WedgeView {
    crate::overlay::WedgeView { glyph, title: title.into(), value: value.map(str::to_string), tone, meter }
}

#[test]
#[ignore = "Requires a Windows desktop to capture the radial overlay"]
#[cfg(windows)]
fn overlay_radial() {
    use crate::overlay::{SpellOverlay, Tone, WeaveMode, WedgeGlyph as G};
    let Some(dir) = env("NEURON_SHOT_OUT") else { return };
    let _ = std::fs::remove_dir_all(&dir);
    crate::overlay::proof_arm(&dir, (350, 350, 900, 900), 1);
    let ov = SpellOverlay::spawn();
    let widgets = vec![
        wedge(G::Speaker, "volume", Some("62%"), Tone::Live, Some(0.62)),
        wedge(G::Teleport, "teleport", None, Tone::Plain, None),
        wedge(G::Whiteboard, "board", None, Tone::Plain, None),
        wedge(G::Mic, "mic", Some("muted"), Tone::Off, None),
        wedge(G::ProfileDot, "profile", Some("desk"), Tone::Active, None),
        wedge(G::Summon, "summon", None, Tone::Plain, None),
        wedge(G::Media, "media", None, Tone::Plain, None),
        wedge(G::Pin, "pin", None, Tone::Plain, None),
    ];
    ov.begin(WeaveMode::Radial { sectors: 8, widgets, fans: Vec::new() });
    std::thread::sleep(Duration::from_millis(900));
    let mut pts = vec![(0.0f32, 0.0f32)];
    for i in 1..=24 {
        let t = i as f32 / 24.0;
        pts.push((t * 70.0, -t * 90.0));
        ov.push(pts.clone());
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(1200));
    ov.recognized(true);
    std::thread::sleep(Duration::from_millis(500));
    ov.end();
    std::thread::sleep(Duration::from_millis(600));
    drop(ov);
    crate::overlay::proof_disarm();
}

#[test]
#[ignore = "Requires a Windows desktop to capture the glyph overlay"]
#[cfg(windows)]
fn overlay_glyph() {
    use crate::overlay::{GlyphHint, SpellOverlay, Tone, WeaveMode, WedgeGlyph as G};
    let Some(dir) = env("NEURON_SHOT_OUT") else { return };
    let _ = std::fs::remove_dir_all(&dir);
    crate::overlay::proof_arm(&dir, (350, 350, 900, 900), 1);
    let step_ms: u64 = env("NEURON_SHOT_STEP_MS").and_then(|s| s.parse().ok()).unwrap_or(45);
    let ov = SpellOverlay::spawn();
    let ring = |n: usize, r: f32| -> Vec<[f32; 2]> {
        (0..=n)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + i as f32 / n as f32 * std::f32::consts::TAU * 0.96;
                [a.cos() * r, a.sin() * r]
            })
            .collect()
    };
    let hint = |conf: f32, locked: bool| GlyphHint {
        view: wedge(G::Teleport, "teleport", None, Tone::Plain, None),
        confidence: conf,
        locked,
        ghost: ring(40, 0.5),
    };
    ov.begin(WeaveMode::Glyph { hint: None });
    std::thread::sleep(Duration::from_millis(500));
    let n = 70;
    let mut pts: Vec<(f32, f32)> = Vec::new();
    for i in 0..=n {
        let a = -std::f32::consts::FRAC_PI_2 + i as f32 / n as f32 * std::f32::consts::TAU * 0.95;
        let wob = 1.0 + 0.04 * (a * 3.0).sin();
        pts.push((a.cos() * 120.0 * wob, 120.0 + a.sin() * 120.0 * wob));
        if i % 3 != 0 {
            continue;
        }
        ov.push(pts.clone());
        if i > 22 {
            let c = ((i - 22) as f32 / 40.0).min(0.94);
            ov.hint(Some(hint(c, c > 0.85)));
        }
        // frame pacing is set by the tap's PNG encode; a long step gives one frame per push
        std::thread::sleep(Duration::from_millis(step_ms));
    }
    std::thread::sleep(Duration::from_millis(900));
    ov.recognized(true);
    std::thread::sleep(Duration::from_millis(500));
    ov.end();
    std::thread::sleep(Duration::from_millis(600));
    drop(ov);
    crate::overlay::proof_disarm();
}

/// The notification engine's own scripted run (enter, reflow, coalesce, drain), dumped to
/// `<run root>/_notif_proof`.
#[test]
#[ignore = "Requires a Windows desktop to capture notification cards"]
#[cfg(windows)]
fn notif_cards() {
    if env("NEURON_SHOT_OUT").is_none() {
        return;
    }
    crate::notifs::write_proof_frames();
}
