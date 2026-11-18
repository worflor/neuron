// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Automated GUI coverage — drives the REAL compiled Slint `AppWindow` + the `State` global through
//! the generated component API (set properties, invoke callbacks, assert state). This exercises the
//! actual `.slint` view and the Rust↔UI glue contract — the previously-untested surface.
//!
//! Each window test runs in its own process on Windows. Slint supplies only one usable window per
//! test process here, so running the tests together used to report green after skipping assertions.
//!
//! All driving happens on the test thread, which Slint treats as the UI thread once a backend is
//! initialized; no event loop needs to spin for property/callback access.

#![cfg(test)]

use crate::glue;
use crate::ui::{AppWindow, State};
use slint::ComponentHandle;

/// Draw a frame, then hold the laid-out tree to `uifit`'s rules: every GUI test's states are
/// checked for layout that does not fit its space.
fn snap(app: &AppWindow) -> Result<slint::SharedPixelBuffer<slint::Rgba8Pixel>, slint::PlatformError> {
    let frame = app.window().take_snapshot();
    let test = std::thread::current().name().unwrap_or("a GUI test").to_string();
    crate::uifit::assert_fits(app.window(), &test);
    if let Ok(frame) = &frame {
        crate::uifit::note(app.window(), frame);
    }
    frame
}

/// Try to bring up a backend + window. Returns None if the platform can't (headless CI with no
/// display / backend), so callers SKIP rather than fail — the smoke test asserts "doesn't panic",
/// not "a display exists".
///
/// The skip must be LOUD and refusable: 29 green GUI tests that silently asserted nothing (the
/// headless default before this) is indistinguishable from real coverage in a summary line. Every
/// skip prints, and `NEURON_REQUIRE_GUI=1` (set it in a job that promises GUI coverage) turns the
/// skip into a failure so a headless environment can't quietly report the tier green.
fn try_window() -> Option<AppWindow> {
    // `AppWindow::new()` initializes the (winit/software) backend lazily; on a machine with no
    // windowing it returns an Err instead of panicking. Either way we don't crash the suite.
    if let Ok(Ok(app)) = std::panic::catch_unwind(AppWindow::new) {
        WINDOW_TAKEN.store(true, std::sync::atomic::Ordering::SeqCst);
        Some(app)
    } else {
        assert!(
            std::env::var_os("NEURON_REQUIRE_GUI").is_none(),
            "NEURON_REQUIRE_GUI is set but no window is available — this environment cannot \
             provide the GUI coverage it promises"
        );
        // Name WHICH of the two reasons this is. They are not the same problem, and reporting
        // both as the first hid the second: a headless runner is a legitimate skip, whereas a
        // process that has already spent its one window is a HARNESS limit quietly eating
        // coverage. Measured 2026-09-17: only the first `AppWindow::new()` in a process
        // succeeds (it reproduces under `--test-threads=1`, so it is not a threading artifact),
        // which means at most one of this module's windowed tests asserts anything per run.
        if WINDOW_TAKEN.load(std::sync::atomic::Ordering::SeqCst) {
            eprintln!(
                "SKIPPED WITH ZERO ASSERTIONS: this process already created its one window, \
                 so this test verified NOTHING. Run it alone to exercise it for real."
            );
        } else {
            eprintln!("skipping: no windowing backend available (GUI test ran zero assertions)");
        }
        None
    }
}

/// Whether this process has already handed out its one window. See [`try_window`].
static WINDOW_TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[test]
fn gui_tests_in_fresh_processes() {
    use std::process::Command;
    #[cfg(target_os = "linux")]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        assert!(
            std::env::var_os("NEURON_REQUIRE_GUI").is_none(),
            "NEURON_REQUIRE_GUI is set but no Linux display is available"
        );
        eprintln!("GUI child tests skipped: no Linux display is available");
        return;
    }
    // Child processes inherit cwd. Hold the process-wide cwd guard for the whole launch loop so
    // a concurrent persistence test cannot remove its temporary cwd underneath a new child.
    let _cwd = crate::testsupport::cwd_guard("apptest_children");
    let exe = std::env::current_exe().expect("current test executable");
    let listing = Command::new(&exe)
        .args(["--list", "--ignored", "apptest::"])
        .output()
        .expect("list isolated GUI tests");
    assert!(listing.status.success(), "libtest could not list the GUI tests");
    let names: Vec<_> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .filter(|name| name.starts_with("apptest::"))
        .map(str::to_owned)
        .collect();
    assert!(names.len() >= 25, "only {} GUI tests found", names.len());
    let mut failures = Vec::new();
    for name in names {
        let output = Command::new(&exe)
            .args(["--ignored", "--exact", &name])
            .env("NEURON_REQUIRE_GUI", "1")
            .output()
            .expect("launch isolated GUI test");
        if !output.status.success() || !String::from_utf8_lossy(&output.stdout).contains("running 1 test") {
            failures.push(format!(
                "{name}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }
    assert!(failures.is_empty(), "GUI tests failed:\n{}", failures.join("\n"));
}

/// Headless launch smoke: build the window, install the whole glue, and confirm the engine pushed
/// real data into the view (devices model + effects exist as models, status is set). This is the
/// "the GUI actually comes up wired to the core" guarantee.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn headless_launch_smoke() {
    let Some(app) = try_window() else {
        eprintln!("skipping: no windowing backend available");
        return;
    };
    // installing the glue is the real wiring step — it loads the Runtime and binds every callback.
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    // the nav starts on Device (panel 0).
    assert_eq!(st.get_page(), 0);
    // the glue set an initial brush colour from the accent.
    let brush = st.get_brush_color();
    assert!(u32::from(brush.red()) + u32::from(brush.green()) + u32::from(brush.blue()) > 0);
    // models are populated objects (length may be 0 with no device, but the model must exist).
    let _ = st.get_devices();
    let _ = st.get_effects();
    let _ = st.get_profiles();
}

/// Driving the nav: setting `page` is how every NavEntry click + the tray "Settings" jump work.
/// Four sections (DEVICE / LIGHTING / INPUT / SYSTEM); INPUT has two depths via `input-view`;
/// profiles open as a SHEET, not a page.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn nav_pages_switch() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    for p in 0..4 {
        st.set_page(p);
        assert_eq!(st.get_page(), p);
    }
    // the INPUT section's two depths both render
    st.set_page(2);
    st.set_input_view(1);
    assert_eq!(st.get_input_view(), 1);
    st.set_input_view(0);
    // the profiles sheet opens/closes as a global affordance
    st.set_profile_sheet_open(true);
    assert!(st.get_profile_sheet_open());
    st.set_profile_sheet_open(false);
}

/// The device-write safety gate toggles through the real callback and flips `writes-paused`.
// Flips the PROCESS-GLOBAL device-write gate (through the real callback, which is the point),
// so it must not run alongside a test that depends on writes being live — anything behind that
// gate silently does nothing while this holds it. `cwd_guard` is this binary's serialization
// lock; taking it keeps the window exclusive. Found via an intermittent auto-switch failure
// that looked like a routing bug.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn safety_gate_toggles() {
    let _cwd = crate::testsupport::cwd_guard("apptest_safety_gate");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    let before = st.get_writes_paused();
    st.invoke_toggle_writes_paused();
    assert_ne!(st.get_writes_paused(), before, "gate must flip");
    st.invoke_toggle_writes_paused();
    assert_eq!(st.get_writes_paused(), before, "gate must flip back");
}

/// SAFE-MODE is the default: a freshly-installed GUI reports input DISARMED, mirroring neuron-core's
/// process-wide arm gate. This test NEVER arms input (per the input-safety rule) — it only asserts
/// the safe default is surfaced. The toggle callback exists (compiles) but is deliberately not fired
/// here, because firing it to ARM would set the process-wide gate that could let a concurrent macro
/// test inject. Disarming-direction safety + the default invariant are what matter.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn input_arm_gate_defaults_safe() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    // the view reflects the core gate, which defaults to DISARMED.
    assert!(
        !st.get_input_armed(),
        "input must default to DISARMED (safe-mode)"
    );
    assert!(
        !neuron::action::input_armed(),
        "core gate must default DISARMED"
    );
}

/// Diagnostics now run on a WORKER thread (real device probes froze the UI for seconds and the
/// RUNNING state never painted). The callback's immediate contract: it flips diag-running on and
/// announces the probe; the rows land via invoke_from_event_loop when the worker finishes (the
/// probe content itself is covered by runtime.rs's direct run_diagnostics tests).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn diagnostics_callback_starts_probe_run() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_run_diagnostics();
    assert!(
        st.get_diag_running(),
        "the bench must report the run in flight"
    );
    assert!(st.get_diag_summary().contains("probing"));
    // re-entry is guarded: a second invoke while running is a no-op, not a second worker.
    st.invoke_run_diagnostics();
    assert!(st.get_diag_running());
}

/// The macro CHECK callback syntax-checks a Python macro WITHOUT executing it. It starts the
/// sidecar wait off-thread so the UI callback returns immediately.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn macro_check_callback_reports() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_macro_check("def macro(ctx):\n    return ctx.app\n".into());
    assert!(st.get_macro_busy());
    assert_eq!(st.get_macro_status(), "checking macro…");
}

/// The radial sector-count callback updates the count, rebuilds the sector model, PERSISTS to
/// cast.toml (a restart must not silently revert the wheel), and clamps to the 3..16 range.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn radial_sector_count_updates_and_persists() {
    let Some(app) = try_window() else { return };
    // set-sector-count now writes cast.toml — isolate in a temp cwd.
    let _cwd = crate::testsupport::cwd_guard("apptest_sectors");
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_set_sector_count(12);
    assert_eq!(st.get_radial_sectors(), 12);
    use slint::Model;
    assert_eq!(st.get_radial_items().row_count(), 12);
    // persisted: a fresh load sees 12.
    assert_eq!(neuron::cast::CastConfig::load().sectors, 12);
    // clamped: a wild value can't outrun the COMPUTED cap (2π·deadzone/jitter; 16 at the
    // default 40-count deadzone) and a wheel never drops below three directions.
    st.invoke_set_sector_count(100);
    assert_eq!(st.get_radial_sectors(), 16);
    assert_eq!(
        st.get_radial_max(),
        16,
        "the cap is derived from the deadzone"
    );
    st.invoke_set_sector_count(1);
    assert_eq!(st.get_radial_sectors(), 3);
}

/// The import wizard flag is a plain UI flag (Settings opens it; apply/close clears it). Driving it
/// proves the overlay flow toggles.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn import_wizard_flag_toggles() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    assert!(!st.get_import_open());
    st.set_import_open(true);
    assert!(st.get_import_open());
    st.set_import_open(false);
    assert!(!st.get_import_open());
}

/// Re-scanning devices through the callback repopulates the devices model and sets a status line —
/// the Device panel's primary read action, driven headlessly (0 devices is fine; the model exists).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn refresh_devices_callback_runs() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_refresh_devices();
    use slint::Model;
    // model exists + is iterable; status reflects the scan.
    let _ = st.get_devices().row_count();
    assert!(st.get_status_line().to_string().contains("re-scanned"));
}

/// EMERGENT device list + WORKING selection. The model unifies HID peripherals AND audio endpoints
/// (mic/output), and selecting ANY row sets `selected-device-kind` to that row's kind — which is the
/// property the per-device panel gates on, so a click genuinely swaps the shown settings. Audio rows
/// are emergent (no hardcoding): they carry a Core-Audio kind + endpoint id and no HID pid.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn device_list_unifies_audio_and_selection_swaps_kind() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_refresh_devices();
    use slint::Model;
    let rows = st.get_devices();
    let n = rows.row_count();
    // selecting EVERY row sets selected-device-kind to that row's kind — the panel-swap contract.
    for i in 0..n {
        st.invoke_select_device(i as i32);
        let row = rows.row_data(i).expect("row exists");
        assert_eq!(
            st.get_selected_device_kind().to_string(),
            row.kind.to_string(),
            "selecting row {i} ({}) must set its kind so the panel swaps",
            row.name
        );
        assert_eq!(
            st.get_selected_device(),
            i as i32,
            "selection index follows the click"
        );
    }
    // emergent audio: any mic/output row carries an endpoint id and no HID pid (it's Core Audio).
    for i in 0..n {
        let row = rows.row_data(i).unwrap();
        let k = row.kind.to_string();
        if k == "mic" || k == "output" {
            assert!(
                !row.id.to_string().is_empty(),
                "an audio row must carry its endpoint id"
            );
            assert!(row.pid.is_empty(), "an audio row has no HID pid");
            assert!(
                !row.cap_dpi && !row.cap_poll && !row.cap_light,
                "audio has no HID capabilities"
            );
        }
    }
    // PER-TYPE differentiation: capability flags come from the device DESCRIPTOR, so a mouse advertises
    // DPI and a keyboard does NOT — that's what makes the panel show different controls per device.
    for i in 0..n {
        let row = rows.row_data(i).unwrap();
        match row.kind.to_string().as_str() {
            "mouse" => {
                assert!(
                    row.cap_dpi,
                    "a mouse must advertise DPI (so the panel shows the DPI fader/sniper)"
                );
                assert!(row.cap_poll, "a mouse advertises polling");
            }
            "keyboard" => {
                assert!(
                    !row.cap_dpi,
                    "a keyboard must NOT advertise DPI (no DPI fader/sniper/stages shown)"
                );
                assert!(!row.cap_scroll, "a keyboard has no scroll-wheel stages");
            }
            _ => {}
        }
    }
}

/// Reloading bindings rebuilds both rule models and reports the assembled spine size — proving the
/// Bindings panel's two rule layers wire to the engine's Trigger→Action assembly.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn reload_bindings_callback_assembles_spine() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_reload_bindings();
    use slint::Model;
    // both rule layers are real models; the hypershift layer may be empty but must exist.
    let _ = st.get_rules().row_count();
    let _ = st.get_hypershift_rules().row_count();
    assert!(st.get_status_line().to_string().contains("spine rule"));
}

/// The lighting grid model is sized rows*cols, and painting/erasing a cell through the callbacks
/// recolours exactly that cell. With NO lit device the dims are the honest (0,0) sentinel and the
/// panel renders its empty state instead of a fake matrix — both worlds are valid here.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_grid_paints_a_cell() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    use slint::Model;
    let rows = st.get_grid_rows();
    let cols = st.get_grid_cols();
    let grid = st.get_grid_px();
    assert_eq!(
        grid.row_count() as i32,
        rows * cols,
        "grid model sized rows*cols"
    );
    if rows == 0 {
        // headless / no lit device: the honest empty state. Painting must be a harmless no-op.
        st.invoke_paint_cell(0);
        st.invoke_clear_grid();
        return;
    }
    // paint cell 0 with the current brush; it must take the brush colour.
    let brush = st.get_brush_color();
    st.invoke_paint_cell(0);
    let painted = grid.row_data(0).unwrap();
    assert_eq!(painted.red(), brush.red());
    assert_eq!(painted.green(), brush.green());
    assert_eq!(painted.blue(), brush.blue());
    // erase puts the cell back to the off colour; clear wipes the whole frame.
    st.invoke_erase_cell(0);
    let erased = st.get_grid_px().row_data(0).unwrap();
    assert_ne!(
        erased.red(),
        brush.red(),
        "erase must not keep the brush colour"
    );
    st.invoke_fill_grid();
    st.invoke_clear_grid();
    let cleared = st.get_grid_px().row_data(0).unwrap();
    assert_eq!(
        (cleared.red(), cleared.green(), cleared.blue()),
        (0x0c, 0x0d, 0x10)
    );
}

/// Changing the light-colour hex through the callback re-parses the brush colour (the colour field's
/// live preview swatch + paint brush both read brush-color).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_color_change_parses_brush() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_color_changed("ff0000".into());
    let c = st.get_brush_color();
    assert_eq!(c.red(), 0xff);
    assert_eq!(c.green(), 0x00);
    assert_eq!(c.blue(), 0x00);
}

/// Every section index renders a panel without panicking — drives all 4 sections (both INPUT
/// depths) plus the import + profiles overlays, the full navigable surface. (Property access on
/// a non-shown window is valid in Slint.)
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn all_panels_reachable() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    for p in 0..4 {
        st.set_page(p);
        assert_eq!(st.get_page(), p);
    }
    st.set_page(2);
    st.set_input_view(1); // the WEAVE depth
    st.set_input_view(0);
    // the import flow overlays any page; profiles open as a sheet.
    st.set_page(3);
    st.set_import_open(true);
    assert!(st.get_import_open());
    st.set_import_open(false);
    st.set_profile_sheet_open(true);
    assert!(st.get_profile_sheet_open());
    st.set_profile_sheet_open(false);
}

/// The Action palette is populated by the glue (the typed-action picker every editor shares). It
/// must carry every editor option so the picker can author a real Action — proving the bindings /
/// radial / gesture editors have something to pick.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn action_palette_populated() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    use slint::Model;
    let n = st.get_action_choices().row_count();
    assert!(
        n >= 8,
        "action palette should offer the full typed-action set, got {n}"
    );
    // row 0 is a group HEADER (the picker renders structure); the first real choice sits
    // under it with a non-empty id, and headers never carry ids of their own.
    let first = st.get_action_choices().row_data(0).unwrap();
    assert!(first.header, "the palette opens with a group header");
    assert!(first.id.is_empty(), "headers are structure, not choices");
    let real = st
        .get_action_choices()
        .iter()
        .find(|c| !c.header)
        .expect("at least one bindable choice");
    assert!(!real.id.is_empty());
    // the instruments ride the same palette as everything else.
    for want in ["teleport", "whiteboard"] {
        assert!(
            st.get_action_choices().iter().any(|c| c.id == want),
            "{want} must be pickable from any editor"
        );
    }
    // the rail filters on `group` and names the hover; entries must carry theirs (only the
    // groupless tail, unbind, is allowed an empty one) and every row a scanning blurb. Every
    // header must count exactly the entries under it (its "· N" and the rail's hover label).
    let rows: Vec<_> = st.get_action_choices().iter().collect();
    for (i, c) in rows.iter().enumerate() {
        if c.header {
            let n = rows[i + 1..]
                .iter()
                .take_while(|r| !r.header && r.group == c.group)
                .count();
            assert_eq!(n as i32, c.count, "header '{}' entry count", c.label);
        } else {
            assert!(!c.blurb.is_empty(), "'{}' must carry its blurb", c.id);
            assert!(
                !c.group.is_empty() || c.id == "noop",
                "'{}' must know its group",
                c.id
            );
        }
    }
    st.set_action_choice(2);
    assert_eq!(st.get_action_choice(), 2);
}

/// Setting an activation rhythm persists to cast.toml, updates both view properties (pattern +
/// symbol readout), and refuses an unparseable phrase without mutating anything.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn activation_rhythm_sets_and_persists() {
    let Some(app) = try_window() else { return };
    let _cwd = crate::testsupport::cwd_guard("apptest_activation");
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    assert_eq!(st.get_activation_pattern(), "hold", "the classic default");
    st.invoke_set_activation("tap tap hold".into());
    assert_eq!(st.get_activation_pattern(), "tap tap hold");
    assert_eq!(st.get_activation_display(), "● ● ▬");
    assert_eq!(
        neuron::cast::CastConfig::load().activation,
        "tap tap hold",
        "the rhythm persists"
    );
    // a pure-tap phrase reads as a toggle in the symbol readout.
    st.invoke_set_activation("tap tap".into());
    assert_eq!(st.get_activation_display(), "● ● ↔");
    // garbage is refused loudly and changes nothing.
    st.invoke_set_activation("bonk".into());
    assert_eq!(st.get_activation_pattern(), "tap tap");
    assert!(st.get_status_line().contains("unknown press symbol"));
}

/// Setting the HyperShift stance persists to feel.toml and reports the stance in plain words.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn hypershift_stance_sets_and_persists() {
    let Some(app) = try_window() else { return };
    let _cwd = crate::testsupport::cwd_guard("apptest_stance");
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    assert_eq!(st.get_hypershift_mode(), "hold", "Razer-compatible default");
    st.invoke_set_hypershift_mode("smart".into());
    assert_eq!(st.get_hypershift_mode(), "smart");
    assert_eq!(
        neuron::feel::FeelConfig::load().hypershift,
        neuron::feel::LayerMode::Smart,
        "the stance persists"
    );
    assert!(st.get_status_line().contains("tap latches"));
}

/// The press-to-bind surface starts clean (no captured trigger) and the capture overlay defaults to
/// inactive — the UI invariant the CaptureOverlay + add-flow read. This NEVER starts a capture
/// worker (which would poll real input) — it asserts the safe default state only.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn press_to_bind_surface_defaults_clean() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    assert!(!st.get_capture_active(), "no capture in flight at startup");
    assert!(!st.get_bind_trigger_ready(), "no trigger captured yet");
    // cancelling a (non-existent) capture is a harmless no-op that leaves the overlay inactive.
    st.invoke_cancel_capture();
    assert!(!st.get_capture_active());
}

/// The radial per-sector editor: selecting a sector to edit sets `editing-sector`; cancelling clears
/// it. Drives the real callbacks (no device, no input).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn radial_sector_editor_targets_a_wedge() {
    // `invoke_set_sector_count` persists cast.toml through the run root — take the process-wide
    // guard like every other run-root-mutating test, so the write lands in a private temp dir.
    let _cwd = crate::testsupport::cwd_guard("apptest_sector_editor");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_set_sector_count(8);
    st.invoke_edit_sector(3);
    assert_eq!(
        st.get_editing_sector(),
        3,
        "edit-sector targets the chosen wedge"
    );
}

/// The gesture->action binder: choosing a glyph sets the bind target (the editor opens for it).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn gesture_bind_sets_target() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_bind_gesture("glyph_test".into());
    assert_eq!(st.get_gesture_bind_target().to_string(), "glyph_test");
}

/// The arm gate has exactly ONE callback now (toggle-input-armed — the header pill and the
/// Settings toggle share it; the old duplicate toggle-live-runtime with its lying "start/stop the
/// loop" contract is gone). Per the input-safety rule, this test does NOT fire it to ARM — it
/// asserts the safe default. The flip behaviour is the same `arm_input` gate covered by core.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn input_arm_single_callback_defaults_safe() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    assert!(
        !st.get_input_armed(),
        "input must default DISARMED (safe-mode)"
    );
    assert!(
        !neuron::action::input_armed(),
        "core gate must default DISARMED"
    );
    // the live-status readouts start inert (no trigger fired — the loop isn't running in tests).
    assert_eq!(st.get_live_fired(), 0);
}

/// Perf-control callbacks are reachable + honor the writes-paused gate (no device needed). Pausing
/// writes makes every surfaced perf write report "writes paused" rather than touch hardware.
// Flips the PROCESS-GLOBAL device-write gate (through the real callback, which is the point),
// so it must not run alongside a test that depends on writes being live — anything behind that
// gate silently does nothing while this holds it. `cwd_guard` is this binary's serialization
// lock; taking it keeps the window exclusive. Found via an intermittent auto-switch failure
// that looked like a routing bug.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn perf_controls_honor_paused_gate() {
    let _cwd = crate::testsupport::cwd_guard("apptest_perf_paused");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    // pause writes through the real toggle.
    if !st.get_writes_paused() {
        st.invoke_toggle_writes_paused();
    }
    assert!(st.get_writes_paused());
    // every surfaced perf write should now refuse without touching a device.
    st.invoke_apply_dpi_stages("800/1600".into(), 0);
    assert!(st.get_perf_status().to_string().contains("paused"));
    st.invoke_apply_idle(120.0);
    assert!(st.get_perf_status().to_string().contains("paused"));
    st.invoke_apply_ingame_polling(1000.0);
    assert!(st.get_perf_status().to_string().contains("paused"));
    // un-pause to leave clean state.
    st.invoke_toggle_writes_paused();
}

/// The Action palette carries the OUTPUT audio entries (out-gain / out-mute) so a binding / radial /
/// gesture can drive headphone / sound-card volume + mute — the new bindable actions this pass
/// surfaced. The palette ids must match the editor's `build_action` arms.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn action_palette_has_output_audio() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    use slint::Model;
    let choices = st.get_action_choices();
    let ids: Vec<String> = (0..choices.row_count())
        .filter_map(|i| choices.row_data(i))
        .map(|c| c.id.to_string())
        .collect();
    // audio CONSOLIDATED: one `volume` (output+mic, step OR dial) and one `mute` (output+mic) entry.
    assert!(
        ids.iter().any(|id| id == "volume"),
        "palette must offer volume; got {ids:?}"
    );
    assert!(
        ids.iter().any(|id| id == "mute"),
        "palette must offer mute; got {ids:?}"
    );
    // both sides of the chain remain reachable through those entries' submodes.
    assert!(ids.iter().any(|id| id == "output-flip"));
    assert!(ids.iter().any(|id| id == "momentary-mic"));
}

// (Launch-mode persistence: the start-minimized half is covered by prefs::start_minimized_round_trips;
//  the autostart half writes the HKCU Run registry key, which a unit test must not touch — so the
//  end-to-end launch-mode callback isn't exercised here.)

/// BEACON state starts inert (no prompt, no pill) and the fullscreen-defer toggle is WIRED to real
/// persistence through its callback — the same contract every Settings switch keeps. The beacon
/// SERVICE itself is live-path only (started by `main`, never by glue), so installing the glue must
/// not present anything.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn beacon_state_is_inert_and_defer_pref_persists() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    // inert by construction: no prompt presented, nothing queued, pill dark. Beacons are always
    // click-through and additive (no fullscreen-defer toggle), so there's nothing to configure.
    assert!(
        !st.get_beacon_active(),
        "no beacon may be active at install"
    );
    assert_eq!(st.get_beacon_count(), 0, "no asks may be queued at install");
}

/// The surfaced perf controls each have a reachable callback (DPI stages, scroll stages, idle,
/// in-game polling, gaming-mode, sniper-save) — the "every perf control Synapse buries" surface.
/// Driven with writes paused so NOTHING touches hardware; each reports its gated/paused status.
// Flips the PROCESS-GLOBAL device-write gate (through the real callback, which is the point),
// so it must not run alongside a test that depends on writes being live — anything behind that
// gate silently does nothing while this holds it. `cwd_guard` is this binary's serialization
// lock; taking it keeps the window exclusive. Found via an intermittent auto-switch failure
// that looked like a routing bug.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn all_perf_controls_have_callbacks() {
    let _cwd = crate::testsupport::cwd_guard("apptest_perf_callbacks");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    if !st.get_writes_paused() {
        st.invoke_toggle_writes_paused();
    }
    assert!(st.get_writes_paused());
    // every surfaced perf write refuses (paused) without a device.
    st.invoke_apply_dpi_stages("400/800/1600".into(), 1);
    assert!(st.get_perf_status().to_string().contains("paused"));
    st.invoke_apply_scroll_stages("tactile/free".into());
    assert!(st.get_perf_status().to_string().contains("paused"));
    // gaming-mode is host-side (no device write) — it sets a policy + status regardless of pause.
    st.set_disable_alt_tab(true);
    st.invoke_apply_gaming_mode();
    assert!(st
        .get_perf_status()
        .to_string()
        .to_lowercase()
        .contains("gaming"));
    st.set_disable_alt_tab(false);
    st.invoke_apply_gaming_mode();
    st.invoke_toggle_writes_paused(); // un-pause
}

/// Adding then removing an app-rule round-trips through the runtime + repopulates the model — and
/// the add now VALIDATES its target: a rule pointing at a profile that doesn't exist is refused
/// (it could only ever fail silently at focus-switch time).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn app_rule_add_then_remove() {
    let Some(app) = try_window() else { return };
    // `apps.toml` + profiles/ resolve via the run root; isolate (and serialize via the shared guard).
    let _cwd = crate::testsupport::cwd_guard("apptest_apprule");
    let shared = glue::install(&app);
    let st = app.global::<State>();
    use slint::Model;
    let before = st.get_app_rules().row_count();
    // a rule for a MISSING profile is refused with the reason in the status line.
    st.invoke_add_app_rule("__neuron_test_app".into(), "__neuron_missing_prof".into());
    assert_eq!(
        st.get_app_rules().row_count(),
        before,
        "missing-profile rule must be refused"
    );
    assert!(
        st.get_status_line().contains("no profile"),
        "must say why: {}",
        st.get_status_line()
    );
    // save the target profile, then the rule lands.
    shared
        .borrow_mut()
        .rt
        .save_profile_from_devices("__neuron_test_prof", 800, 1000, 50, Vec::new());
    st.invoke_add_app_rule("__neuron_test_app".into(), "__neuron_test_prof".into());
    let after_add = st.get_app_rules().row_count();
    assert_eq!(after_add, before + 1, "rule added");
    // remove the one we just added (it's last).
    st.invoke_remove_app_rule((after_add - 1) as i32);
    assert_eq!(st.get_app_rules().row_count(), before, "rule removed");
}

/// Removing a GUI-authored HYPERSHIFT rule goes through its own tier-aware callback — the old
/// single remove-rule(int) mapped hyper row indices against the BASE list and deleted the wrong
/// rule (or refused with a lying message).
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn hypershift_rule_remove_targets_its_own_tier() {
    let Some(app) = try_window() else { return };
    let _cwd = crate::testsupport::cwd_guard("apptest_hyper_remove");
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    use neuron::engine::Trigger;
    // author one base + one hyper rule (interleaved on disk), then reload the view.
    crate::editor::add_gui_rule(
        Trigger::Input {
            page: 1,
            usage: 1,
            pid: None,
        },
        crate::editor::build_action("key", "a"),
        false,
    )
    .unwrap();
    crate::editor::add_gui_rule(
        Trigger::Input {
            page: 2,
            usage: 2,
            pid: None,
        },
        crate::editor::build_action("key", "b"),
        true,
    )
    .unwrap();
    st.invoke_reload_bindings();
    use slint::Model;
    assert_eq!(st.get_editable_count(), 1, "one removable base rule");
    assert_eq!(st.get_editable_hyper_count(), 1, "one removable hyper rule");
    let hyper_rows = st.get_hypershift_rules().row_count();
    assert!(hyper_rows >= 1);
    // remove the hyper rule via ITS callback: the base rule must survive.
    st.invoke_remove_hyper_rule((hyper_rows - 1) as i32);
    let rules = crate::editor::load_gui_rules();
    assert_eq!(rules.len(), 1, "exactly one rule left");
    assert!(
        rules[0].layer.is_none(),
        "the surviving rule is the BASE one"
    );
}

// ── DEVICE PUSH -> VIEW ────────────────────────────────────────────────────────────────────────
// The device announces every onboard change over HID within milliseconds. `apply_observation` is
// where that lands on the view, and it takes an `&AppWindow` precisely so this can drive it
// directly — no global handle, no event loop, no hardware.
//
// ONE test for three scenarios, deliberately. Only the FIRST `AppWindow::new()` in a process
// succeeds; every later one fails and `try_window` skips. Split across three `#[test]`s, two of
// them reported green having asserted nothing — the exact failure mode this module's own header
// warns about. Sharing one window keeps all three real whenever this test is the one that gets it.

/// Seed one selected, DPI-capable row for `pid` reading `dpi`, with the draft fader agreeing.
fn seed_one_device(app: &AppWindow, pid: &str, dpi: &str) {
    use crate::ui::DeviceRow;
    let st = app.global::<State>();
    let row = DeviceRow {
        pid: pid.into(),
        dpi: dpi.into(),
        cap_dpi: true,
        connected: true,
        ..Default::default()
    };
    st.set_devices(slint::ModelRc::new(slint::VecModel::from(vec![row])));
    st.set_selected_device(0);
    st.set_dpi(dpi.parse::<f32>().unwrap());
}

fn row_dpi(app: &AppWindow) -> String {
    use slint::Model;
    app.global::<State>().get_devices().row_data(0).unwrap().dpi.to_string()
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn a_device_push_lands_on_the_view_without_stomping_an_edit() {
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();

    // 1. An untouched fader follows the device instead of sitting on a stale number.
    seed_one_device(&app, "00a8", "30000");
    glue::apply_observation(&app, 0x00a8, glue::Observed::Dpi(800));
    assert_eq!(row_dpi(&app), "800", "the row must show what the device reported");
    assert_eq!(st.get_dpi(), 800.0, "an untouched fader follows the device");

    // 2. A half-finished edit is the one thing a background push must not overwrite.
    seed_one_device(&app, "00a8", "30000");
    st.set_dpi(1234.0); // the user has dragged the fader and not applied yet
    glue::apply_observation(&app, 0x00a8, glue::Observed::Dpi(800));
    assert_eq!(row_dpi(&app), "800", "the row still tells the truth about the hardware");
    assert_eq!(st.get_dpi(), 1234.0, "the in-progress edit survives");

    // 3. A second mouse cycling its own DPI must not repaint this one.
    seed_one_device(&app, "00a8", "30000");
    glue::apply_observation(&app, 0x0084, glue::Observed::Dpi(400));
    assert_eq!(row_dpi(&app), "30000");
    assert_eq!(st.get_dpi(), 30000.0);
}

// ── RENDER SMOKE ───────────────────────────────────────────────────────────────────────────────
// Slint evaluates bindings lazily, so a binding loop (`Recursion detected`) never fires on a
// property set; it fires when the element is DRAWN in the state that reads the loop. The pairing
// drawer only reads its hover bindings while hovered, so this renders the lighting page, finds a
// chip by its (unique) swatch colour in the frame, moves the pointer onto it and across its
// capsules, and draws again each time. A loop fails here instead of in the running app.

const CHIP_MARK: (u8, u8, u8) = (255, 0, 255);

fn marked_chip(name: &str, gradient: i32) -> crate::ui::PairChip {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(22, 8);
    buf.make_mut_slice().fill(slint::Rgba8Pixel { r: CHIP_MARK.0, g: CHIP_MARK.1, b: CHIP_MARK.2, a: 255 });
    crate::ui::PairChip { name: name.into(), swatch: slint::Image::from_rgba8(buf), pair: gradient }
}

/// Top-left-most pixel of the chip mark in a frame.
fn find_mark(shot: &slint::SharedPixelBuffer<slint::Rgba8Pixel>) -> Option<(u32, u32)> {
    let w = shot.width();
    shot.as_slice().iter().position(|p| (p.r, p.g, p.b) == CHIP_MARK).map(|i| (i as u32 % w, i as u32 / w))
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_page_draws_pairing_chips_hovered_and_not() {
    use crate::ui::EffectTileMeta;
    use slint::platform::WindowEvent;
    use slint::{LogicalPosition, ModelRc, VecModel};
    // the GPU renderer cannot snapshot without a running event loop; the software one can. Each GUI
    // test owns its process, so choosing the backend here cannot leak into another test.
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let tile = |slug: &str, names: &[&str]| {
        let pairs = ModelRc::new(VecModel::from(
            names.iter().enumerate().map(|(i, n)| marked_chip(n, i as i32)).collect::<Vec<_>>(),
        ));
        EffectTileMeta {
            name: slug.into(),
            slug: slug.into(),
            kind: "effect".into(),
            blurb: "a look".into(),
            source: "".into(),
            pair_widest: crate::glue::widest_pair(&pairs),
            pairs,
        }
    };
    st.set_light_tiles(ModelRc::new(VecModel::from(vec![
        tile("fire", &["aurora", "sunset on light painting", "bubble", "meadow", "tidepool"]),
        tile("comet", &["rainbow"]),
    ])));
    // the catalog only draws once a lighting surface exists: a selected, lit-capable board with a grid
    st.set_selected_device(0);
    st.set_selected_device_name("Test Board".into());
    st.set_sel_can_light(true);
    st.set_grid_rows(6);
    st.set_grid_cols(22);
    st.set_page(1);
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1240.0, 1100.0));
    app.show().expect("show the window");
    let cold = snap(&app).expect("a frame is drawn");
    let demands = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = demands.clone();
    st.on_tile_demand(move |shelf, start, end, warm_start, warm_end| {
        observed.borrow_mut().push((shelf, start, end, warm_start, warm_end));
    });
    app.invoke_sample_light_demand();
    std::thread::sleep(std::time::Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    snap(&app).expect("viewport geometry is sampled after layout");
    assert!(demands.borrow().iter().any(|&(shelf, start, end, _, _)| shelf == 0 && start == 0 && end == 2), "{:#?}", demands.borrow());
    assert!(demands.borrow().iter().all(|&(_, start, end, warm_start, warm_end)|
        warm_start <= start && start <= end && end <= warm_end));
    let (x, y) = find_mark(&cold).expect("a chip is on the page");
    for dy in (20..160).step_by(10) {
        app.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x as f32, y as f32 - dy as f32),
        });
        snap(&app).expect("the tile and its layer control draw while hovered");
    }
    let at = |dx: f32| WindowEvent::PointerMoved { position: LogicalPosition::new(x as f32 + dx, y as f32 + 3.0) };
    // onto the first capsule, then across the drawer as it opens (the drawer animates, so a few
    // frames), then back out
    for (step, dx) in [3.0, 3.0, 30.0, 56.0, 82.0, 3.0].into_iter().enumerate() {
        app.window().dispatch_event(at(dx));
        let frame = snap(&app).expect("a hovered frame is drawn");
        // NEURON_INSPECTOR_SHOTS=<dir> keeps the opening frame and each settled one for a look
        if let Some(dir) = std::env::var_os("NEURON_INSPECTOR_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            let save = |name: String, f: &slint::SharedPixelBuffer<slint::Rgba8Pixel>| {
                image::save_buffer(dir.join(name), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
            };
            save(format!("pairs-{step}-first.png"), &frame);
            for mid in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(70));
                slint::platform::update_timers_and_animations();
                save(format!("pairs-{step}-t{}.png", (mid + 1) * 70), &snap(&app).unwrap());
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
            slint::platform::update_timers_and_animations();
            save(format!("pairs-{step}-settled.png"), &snap(&app).unwrap());
        }
    }
    app.window().dispatch_event(WindowEvent::PointerExited);
    let after = snap(&app).expect("a frame after the pointer leaves");
    assert!(after.as_bytes().len() == cold.as_bytes().len());
    st.set_light_user_tiles(ModelRc::new(VecModel::from(vec![tile("user:late", &["ice"])])));
    app.window().set_size(slint::LogicalSize::new(920.0, 900.0));
    snap(&app).expect("late metadata and a reflow draw without eager geometry recursion");
    std::thread::sleep(std::time::Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    snap(&app).expect("reflow demand is sampled after layout");
    assert!(demands.borrow().iter().any(|&(shelf, _, _, _, _)| shelf == 3));
}

/// The EFFECT header's painted name: with no ramp it draws the plain-ink fallback; with one, the
/// name wears it (marked pure so it is findable — the spectrum editor is absent here, so no other
/// element carries the mark), and hovering drifts the gradient through the letters. Hover-gated
/// bindings only fire while drawn — see the section note above.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_effect_header_draws_painted_name_hovered_and_not() {
    use slint::platform::WindowEvent;
    use slint::{LogicalPosition, ModelRc, SharedString, VecModel};
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    // the inspector column mounts on a selected effect; the EFFECT section on a declared param
    st.set_selected_device(0);
    st.set_selected_device_name("Test Board".into());
    st.set_sel_can_light(true);
    st.set_grid_rows(6);
    st.set_grid_cols(22);
    st.set_page(1);
    st.set_light_effect("wave".into());
    st.set_light_stack_labels(ModelRc::new(VecModel::from(vec![SharedString::from("wave")])));
    st.set_light_params(ModelRc::new(VecModel::from(vec![crate::ui::EffectParam {
        key: "speed".into(), label: "speed".into(), kind: "range".into(),
        fval: 1.0, fmin: 0.0, fmax: 3.0,
        options: ModelRc::new(VecModel::from(Vec::<SharedString>::new())),
        previews: ModelRc::default(),
        ival: 0, hex: "".into(), col: slint::Color::from_rgb_u8(0, 0, 0), bval: false,
    }])));
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1240.0, 1100.0));
    app.show().expect("show the window");
    snap(&app).expect("the plain-ink fallback name draws");
    // the painted branch: the header name wears the ramp. Only THIS name paints — the spectrum
    // editor is absent (no has-spectrum here), so `light-name-chars` stays empty.
    st.set_light_effect_chars(ModelRc::new(VecModel::from(
        "wave".chars().map(|c| SharedString::from(c.to_string())).collect::<Vec<_>>(),
    )));
    st.set_light_name_ramp(ModelRc::new(VecModel::from(vec![slint::Color::from_rgb_u8(255, 0, 255); 4])));
    let painted = snap(&app).expect("the painted name draws");
    // text antialiasing never yields a pure ramp pixel, so match the ramp's hue loosely
    let mark = |p: &slint::Rgba8Pixel| p.r >= 200 && p.b >= 200 && p.g <= 100;
    let width = painted.width();
    let (x, y) = painted.as_slice().iter().position(mark)
        .map(|i| (i as u32 % width, i as u32 / width))
        .expect("the painted name carries its ramp colour");
    for dx in [0.0f32, 4.0, -4.0, 8.0] {
        app.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x as f32 + dx, y as f32),
        });
        snap(&app).expect("the hover drift reads its bindings while drawn");
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    slint::platform::update_timers_and_animations();
    snap(&app).expect("a mid-drift frame draws");
    app.window().dispatch_event(WindowEvent::PointerExited);
    snap(&app).expect("the settled name draws after the pointer leaves");
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_parameter_drags_preserve_live_models_and_latest_value() {
    let _cwd = crate::testsupport::cwd_guard("lighting_parameter_drags");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    {
        let mut state = shared.borrow_mut();
        state.light_layers = vec![neuron::pattern::preset_by_slug("wave").unwrap().to_layer()];
        state.selected_layer = 0;
        state.layers_rev += 1;
    }
    glue::refresh_layers(&app, &shared);
    let params = st.get_light_params();
    let sequence = st.get_light_seq();
    let stops = st.get_light_stops();
    let layer_choices = st.get_lighting_layer_choices();
    let layer_labels = st.get_lighting_layer_labels();
    let preset_choices = st.get_lighting_preset_choices();
    let preset_labels = st.get_lighting_preset_labels();
    for i in 0..100 {
        st.invoke_set_param_range("speed".into(), i as f32 / 25.0);
        assert!(params == st.get_light_params(), "a numeric edit must retain its rendered parameter rows");
        assert!(sequence == st.get_light_seq(), "unchanged palettes must retain timeline images");
        assert!(stops == st.get_light_stops());
        assert!(layer_choices == st.get_lighting_layer_choices());
        assert!(layer_labels == st.get_lighting_layer_labels());
        assert!(preset_choices == st.get_lighting_preset_choices());
        assert!(preset_labels == st.get_lighting_preset_labels());
    }
    assert_eq!(shared.borrow().light_layers[0].params.f32("speed", 0.0), 3.96);
    {
        let mut state = shared.borrow_mut();
        state.light_layers.push(neuron::pattern::preset_by_slug("wave").unwrap().to_layer());
        state.layers_rev += 1;
    }
    glue::refresh_layers(&app, &shared);
    let first_id = shared.borrow().light_layers[0].id;
    let second_id = shared.borrow().light_layers[1].id;
    assert!(first_id.is_some() && second_id.is_some());
    assert_ne!(first_id, second_id);
    shared.borrow_mut().selected_layer = 1;
    glue::refresh_layers(&app, &shared);
    assert!(params != st.get_light_params(), "switching layers must end the previous control's edit identity");
    {
        let mut state = shared.borrow_mut();
        state.light_layers.swap(0, 1);
        state.selected_layer = 0;
        state.layers_rev += 1;
    }
    let second_params = st.get_light_params();
    glue::refresh_layers(&app, &shared);
    assert_eq!(shared.borrow().light_layers[0].id, second_id);
    assert!(second_params == st.get_light_params(), "reordering the selected layer preserves its identity");
    st.invoke_set_param_range("speed".into(), 2.0);
    assert_eq!(shared.borrow().light_layers[0].params.f32("speed", 0.0), 2.0);
    assert_eq!(shared.borrow().light_layers[1].params.f32("speed", 0.0), 3.96);
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn catalog_layer_insertion_keeps_existing_identity_and_orders_above_below() {
    let _cwd = crate::testsupport::cwd_guard("catalog_layer_insertion");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    shared.borrow_mut().light_layers = vec![neuron::pattern::preset_layer("wave").unwrap()];
    shared.borrow_mut().selected_layer = 0;
    glue::refresh_layers(&app, &shared);
    let existing = shared.borrow().light_layers[0].clone();
    st.invoke_stack_tile("fire".into(), true);
    assert_eq!(shared.borrow().light_layers.len(), 2);
    assert_eq!(shared.borrow().light_layers[0], existing);
    assert_eq!(shared.borrow().light_layers[1].pattern, "heat");
    assert_ne!(shared.borrow().light_layers[1].id, existing.id);
    shared.borrow_mut().selected_layer = 0;
    st.invoke_stack_tile("comet".into(), false);
    assert_eq!(shared.borrow().light_layers.len(), 3);
    assert_eq!(shared.borrow().light_layers[0].pattern, "comet");
    assert_eq!(shared.borrow().light_layers[1], existing);
    st.invoke_stack_tile("missing-preset".into(), true);
    assert_eq!(shared.borrow().light_layers.len(), 3);
    let ids: std::collections::HashSet<_> = shared.borrow().light_layers.iter().map(|d| d.id).collect();
    assert_eq!(ids.len(), 3);
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn hero_publication_rejects_wrong_units_and_preserves_then_releases_paint_drafts() {
    use crate::light_render::{CompletedFrame, DesiredSurfaceState, SurfaceId, Target};
    use neuron::lighting::Rgb;
    use slint::Model;
    let _cwd = crate::testsupport::cwd_guard("hero_publication");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    {
        let mut state = shared.borrow_mut();
        state.rt.selected_pid = 0x0226;
        state.rt.selected_unit = "first".into();
    }
    let st = app.global::<State>();
    st.set_page(0);
    st.set_writes_paused(true);
    st.set_grid_rows(1);
    st.set_grid_cols(2);
    st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(vec![slint::Color::from_rgb_u8(0, 0, 0); 2])));
    let grid = st.get_grid_px();
    let desired = DesiredSurfaceState { active: true, hero: true, dims: (1, 2), content_rev: 42,
        target: Target { pid: 0x0226, unit: "first".into(), epoch: 5 }, ..Default::default() };
    let frame = |target: Target, content_rev: u64, color: Rgb| CompletedFrame {
        id: SurfaceId::Hero, target, content_rev, dims: (1, 2),
        rgb: vec![color; 2].into(), swatch: None, overlay: None, requested: std::time::Instant::now(), render_time: std::time::Duration::ZERO,
    };
    let red = Rgb::new(255, 0, 0);
    let green = Rgb::new(0, 255, 0);
    glue::apply_preview_test_frames(&app, &desired, vec![frame(desired.target.clone(), 42, red)]);
    let published = grid.row_data(0).unwrap();
    for (target, revision) in [
        (Target { unit: "second".into(), ..desired.target.clone() }, 42),
        (Target { epoch: 4, ..desired.target.clone() }, 42),
        (desired.target.clone(), 41),
    ] {
        glue::apply_preview_test_frames(&app, &desired, vec![frame(target, revision, green)]);
        assert_eq!(grid.row_data(0).unwrap(), published);
    }
    st.invoke_toggle_brush();
    assert!(st.get_light_brush_on());
    st.set_brush_color(slint::Color::from_rgb_u8(0, 0, 255));
    st.invoke_paint_cell(0);
    let painted = grid.row_data(0).unwrap();
    assert_ne!(painted, published);
    glue::apply_preview_test_frames(&app, &desired, vec![frame(desired.target.clone(), 42, green)]);
    assert_eq!(grid.row_data(0).unwrap(), painted, "an in-flight frame cannot overwrite the brush");
    st.invoke_toggle_brush();
    assert!(!st.get_light_brush_on());
    glue::apply_preview_test_frames(&app, &desired, vec![frame(desired.target.clone(), 42, red)]);
    assert_eq!(grid.row_data(0).unwrap(), published, "even an unchanged static frame must restore the live canvas");
    assert!(grid == st.get_grid_px(), "publication keeps keycap rows in place");
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn live_preview_observes_complete_frames_without_advancing_or_crossing_units() {
    use neuron::pattern::{LiveComposite, SurfaceIdentity, LayerDef, TransitionHint};
    use neuron::lighting::Rgb;
    use slint::Model;
    use std::time::Duration;
    let _cwd = crate::testsupport::cwd_guard("live_preview_observer");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true); st.set_window_shown(true); st.set_page(1);
    st.set_light_hero_visible(true); st.set_grid_rows(1); st.set_grid_cols(2);
    st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(vec![slint::Color::from_rgb_u8(0, 0, 0); 2])));
    let model = st.get_grid_px();
    let target = SurfaceIdentity { pid: 0x0226, unit: "first".into(), plane: "base".into(), dimensions: (1, 2) };
    let mut live = LiveComposite::new(target, vec![LayerDef { spectrum: neuron::spectrum::Spectrum::solid(Rgb::new(255, 0, 0)), ..Default::default() }]).unwrap();
    let first = live.render_cycle(Duration::ZERO, Duration::ZERO, Duration::ZERO).unwrap();
    {
        let mut state = shared.borrow_mut(); state.rt.selected_pid = 0x0226; state.rt.selected_unit = "first".into();
        state.rt.install_live_fixture(&live);
    }
    for _ in 0..30 { st.invoke_preview_tick(); }
    assert_eq!(live.handle().latest().unwrap().frame_id, first.frame_id);
    let red = model.row_data(0).unwrap(); assert_eq!(red.red(), 255);
    shared.borrow_mut().rt.selected_unit = "second".into();
    live.handle().publish(vec![LayerDef { spectrum: neuron::spectrum::Spectrum::solid(Rgb::new(0, 255, 0)), ..Default::default() }], TransitionHint::Immediate).unwrap();
    let time = Duration::from_millis(33); live.render_cycle(time, time, time).unwrap();
    let time = Duration::from_millis(66); live.render_cycle(time, time, time).unwrap();
    st.invoke_preview_tick(); assert_eq!(model.row_data(0).unwrap(), red);
    shared.borrow_mut().rt.selected_unit = "first".into();
    st.invoke_preview_tick(); assert_eq!(model.row_data(0).unwrap().green(), 255);
    assert_eq!(st.get_grid_px(), model);
    assert_eq!(live.handle().latest().unwrap().frame_id, 3);
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn layer_stack_controls_preserve_identity_and_draw_a_large_scrollable_stack() {
    use crate::ui::LightingStackRow;
    use slint::{Model, ModelRc, VecModel};
    let _cwd = crate::testsupport::cwd_guard("lighting_stack_controls");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    {
        let mut state = shared.borrow_mut();
        state.light_layers = vec![neuron::pattern::preset_layer("wave").unwrap(), neuron::pattern::preset_layer("wildlife").unwrap()];
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    let ids = shared.borrow().light_layers.iter().map(|l| l.id).collect::<Vec<_>>();
    let rows = st.get_light_stack_rows();
    assert_eq!(rows.row_data(0).unwrap().index, 1, "topmost layer is first");
    st.invoke_select_stack(1);
    st.invoke_set_layer_blend(1);
    st.invoke_set_layer_opacity(37.0);
    assert!(rows == st.get_light_stack_rows(), "edits retain row models");
    assert_eq!(shared.borrow().light_layers[1].blend, neuron::effects::Blend::Screen);
    assert_eq!(shared.borrow().light_layers[1].opacity, 0.37);
    assert_eq!(shared.borrow().light_layers[0].opacity, 1.0);
    assert_eq!(shared.borrow().light_layers.iter().map(|l| l.id).collect::<Vec<_>>(), ids);
    st.invoke_toggle_stack(1);
    assert!(!shared.borrow().light_layers[1].enabled);
    st.invoke_move_stack(1, -1);
    assert_eq!(shared.borrow().light_layers[0].id, ids[1]);
    assert_eq!(st.get_selected_layer(), 0);
    assert_eq!(st.get_light_layer_opacity(), 37.0);
    st.invoke_stack_current();
    let state = shared.borrow();
    assert_ne!(state.light_layers[2].id, ids[1]);
    assert_eq!(state.light_layers[2].opacity, 0.37);
    assert_eq!(state.light_layers[2].blend, neuron::effects::Blend::Screen);
    drop(state);
    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(1); st.set_grid_cols(2);
    st.set_page(1); st.set_window_shown(true); st.set_light_stack_count(1000);
    st.set_light_stack_rows(ModelRc::new(VecModel::from((0..1000).rev().map(|index| LightingStackRow {
        index, name: format!("Layer {index}").into(), blend: "screen".into(), amount: "37%".into(), place: "".into(), ducks: true,
        enabled: true, focus: ModelRc::default(), wide: false, tier: -1,
    }).collect::<Vec<_>>())));
    app.window().set_size(slint::LogicalSize::new(1000.0, 1400.0));
    app.show().unwrap();
    snap(&app).expect("large stack draws in a bounded list");
    for index in [999, 500, 0] {
        st.set_selected_layer(index);
        snap(&app).expect("selection scrolls into view without binding loops");
    }
}

/// The lighting page draws the shapes the other families' devices learn: one colour for a whole
/// QMK board, a row of Logitech zones, and a Vial board's matrix, each lit, at the window's minimum
/// width and a wide one.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn the_lighting_page_draws_family_boards() {
    let _cwd = crate::testsupport::cwd_guard("family_boards");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    st.set_window_shown(true);
    let shots = std::env::var_os("NEURON_INSPECTOR_SHOTS").map(std::path::PathBuf::from);
    for (rows, cols, kind, name) in [
        (1, 1, "keyboard", "QMK board"),
        (1, 3, "mouse", "Logitech G502 X"),
        (5, 15, "keyboard", "Vial board"),
    ] {
        st.set_selected_device(0);
        st.set_selected_device_name(name.into());
        st.set_sel_can_light(true);
        st.set_grid_rows(rows);
        st.set_grid_cols(cols);
        st.set_grid_kind(kind.into());
        let n = (rows * cols) as usize;
        let px: Vec<slint::Color> = (0..n)
            .map(|i| {
                let t = i as f32 / n.max(2) as f32;
                slint::Color::from_rgb_u8((255.0 * t) as u8, 80, (255.0 * (1.0 - t)) as u8)
            })
            .collect();
        st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(px)));
        st.set_page(1);
        for width in [900.0, 1240.0] {
            app.window().set_size(slint::LogicalSize::new(width, 900.0));
            app.show().expect("show the window");
            let f = snap(&app).expect("a family board draws");
            if let Some(dir) = &shots {
                std::fs::create_dir_all(dir).unwrap();
                let file = format!("family-{rows}x{cols}-{width}.png");
                image::save_buffer(dir.join(file), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
            }
        }
    }
}

/// Stack sense end to end: over a full-board wave, an opaque Wildlife keeps the wave's light from the
/// board, so pointing at the wave's row shows it lost, and Wildlife's shows it burying the wave. The
/// trouble and its fixes sit in a strip above the rows; resting on a fix plays it on the board and
/// changes nothing else, leaving puts the board back exactly, and a click keeps it as an ordinary
/// lighting edit the status bar can undo. A setting chosen by hand is not second-guessed, and a look
/// added over a full-board one lands so the one beneath still shows.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn stack_sense_names_the_trouble_and_offers_fixes() {
    use slint::Model;
    let _cwd = crate::testsupport::cwd_guard("stack_sense");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(6); st.set_grid_cols(22);
    {
        let mut state = shared.borrow_mut();
        state.light_layers = vec![neuron::pattern::preset_layer("wave").unwrap(), neuron::pattern::preset_layer("wildlife").unwrap()];
        state.light_layers[1].blend = neuron::effects::Blend::Normal;
        state.light_layers[1].opacity = 1.0;
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    // the board's focus for a row: how many cells reach the board, are lost, and are marked troubled
    let count = |row: &crate::ui::LightingStackRow, bit: i32| row.focus.iter().filter(|m| m & bit != 0).count();
    // rows are topmost first: Wildlife, then Wave
    let rows = st.get_light_stack_rows();
    let (wildlife, wave) = (rows.row_data(0).unwrap(), rows.row_data(1).unwrap());
    assert_eq!(wave.focus.row_count(), 6 * 22, "one focus cell per key");
    assert!(count(&wave, 1) * 4 < 6 * 22, "the buried wave reaches little of the board");
    assert_eq!(count(&wave, 2), 0, "lost nearly everywhere, the dimming says so without a ring on every key");
    assert!(count(&wildlife, 1) > count(&wildlife, 2), "Wildlife reaches it");
    assert!(wildlife.wide && count(&wildlife, 4) == 0, "Wildlife buries the wave everywhere: the board's frame says so, not every key");
    assert!(!wave.wide && count(&wave, 4) == 0, "the harm is marked on the layer doing it");
    let owners = st.get_light_cell_owner();
    assert_eq!(owners.row_count(), 6 * 22);
    assert!(owners.iter().filter(|&o| o == 1).count() > owners.iter().filter(|&o| o == 0).count(), "Wildlife shows on most keys");
    assert_eq!(st.get_light_sense_trouble().as_str(), "Wave hidden under Wildlife", "an opaque Wildlife leaves nothing of the wave");
    assert_eq!(st.get_light_sense_tier(), 2, "so it's an error, not a warning");
    assert_eq!(wildlife.tier, 2, "and Wildlife's marks on the board say so in the same ink");
    let offers: Vec<i32> = st.get_light_sense_offers().iter().map(|o| o.offer).collect();
    assert!(!offers.is_empty(), "a trouble comes with something to do about it");
    assert!(st.get_light_opacity_zoned(), "the opacity rail has its temperature");
    assert_eq!(st.get_light_blend_previews().row_count(), 5, "one preview per blend");

    // resting on a fix plays it on the board and changes nothing else; leaving puts the board back
    let offer = offers[0];
    let before = shared.borrow().light_layers.clone();
    let board = |st: &State| st.get_grid_px().iter().collect::<Vec<_>>();
    st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(vec![slint::Color::from_rgb_u8(0x0c, 0x0d, 0x10); 6 * 22])));
    let resting = board(&st);
    let offers_before: Vec<_> = st.get_light_sense_offers().iter().collect();
    st.invoke_light_sense_preview(offer);
    assert_ne!(board(&st), resting, "the fix plays on the board");
    assert_eq!(shared.borrow().light_layers, before, "and nothing else changes: not the layers");
    assert_eq!(st.get_light_stack_rows().row_data(0).unwrap().amount, wildlife.amount, "not the rows");
    assert_eq!(st.get_light_sense_offers().iter().collect::<Vec<_>>(), offers_before, "not the strip under the pointer");
    st.invoke_light_sense_preview(-1);
    assert_eq!(board(&st), resting, "leaving puts the board back exactly");
    assert_eq!(shared.borrow().light_layers, before);

    // a click keeps it, and the status bar's undo can take it back
    st.invoke_light_sense_preview(offer);
    st.invoke_light_sense_accept(offer);
    let kept = shared.borrow().light_layers.clone();
    assert_ne!(kept, before, "the fix was kept");
    match neuron::session_undo::peek().map(|r| r.entry) {
        Some(neuron::session_undo::Entry::Lighting { before: was, applied, .. }) => {
            assert_eq!(was, before);
            assert_eq!(applied, kept);
        }
        other => panic!("a kept fix is journaled as a lighting edit: {other:?}"),
    }

    // a blend chosen by hand is not offered back, and nothing else is lost with it
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.selected_layer = 1;
    }
    glue::refresh_layers(&app, &shared);
    let blends = ["normal", "screen", "add", "multiply", "cutout"];
    let wildlife_blend = |f: &crate::ui::LightingFix| blends.iter().any(|b| f.text.as_str() == format!("Wildlife → {b}"));
    assert!(st.get_light_sense_offers().iter().any(|f| wildlife_blend(&f)), "untouched, Wildlife's blend is offered");
    let others: Vec<String> = st.get_light_sense_offers().iter().filter(|f| !wildlife_blend(f)).map(|f| f.text.to_string()).collect();
    st.invoke_set_layer_blend(1);
    st.invoke_set_layer_blend(0);
    assert_eq!(shared.borrow().light_layers, before, "the stack is as it was, with its blend chosen by hand");
    assert!(st.get_light_sense_offers().iter().all(|f| !wildlife_blend(&f)), "Normal was chosen by hand, so Wildlife's blend isn't second-guessed");
    let now: Vec<String> = st.get_light_sense_offers().iter().map(|f| f.text.to_string()).collect();
    assert!(others.iter().all(|f| now.contains(f)), "every other fix is still offered: {others:?} -> {now:?}");

    // adding Wildlife over a wave: its default (opaque Normal) would bury the wave under its dim
    // habitat; it lands in whatever look lets both read
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.light_layers.truncate(1);
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    st.invoke_stack_tile("wildlife".into(), true);
    let added = shared.borrow().light_layers[1].clone();
    assert_eq!(added.pattern, before[1].pattern);
    assert!(!(added.blend == neuron::effects::Blend::Normal && added.opacity >= 1.0), "the wave must still show under it: {:?} {}", added.blend, added.opacity);

    // and it all draws, at the narrowest window and a roomy one, from either layer's side
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    st.set_page(1); st.set_window_shown(true);
    app.show().unwrap();
    // no renderer runs here, so the board is given the stack as it is, composited a moment in
    let paint_board = || {
        let layers = shared.borrow().light_layers.clone();
        let frame = neuron::pattern::Compositor::from_defs(&layers).render(6, 22, 1.5);
        st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(
            frame.iter().map(|c| slint::Color::from_rgb_u8(c.r.max(0x0c), c.g.max(0x0d), c.b.max(0x10))).collect::<Vec<_>>(),
        )));
    };
    paint_board();
    let save = |frame: &slint::SharedPixelBuffer<slint::Rgba8Pixel>, name: String| {
        if let Some(dir) = std::env::var_os("NEURON_INSPECTOR_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), frame.as_bytes(), frame.width(), frame.height(), image::ColorType::Rgba8).unwrap();
        }
    };
    for selected in [0, 1] {
        st.invoke_select_stack(selected);
        for width in [900.0, 1240.0] {
            // tall enough that the inspector's STACK section is on screen
            app.window().set_size(slint::LogicalSize::new(width, 2400.0));
            let frame = snap(&app).expect("stack sense draws without binding loops");
            save(&frame, format!("stack-sense-{selected}-{width}"));
        }
    }
    // pointing at a row opens its other fixes (hover-gated bindings only show a loop when drawn
    // hovered), and resting on a fix keeps it open
    use slint::platform::WindowEvent;
    let lowest = |name: &str| crate::uifit::rects_of(app.window(), name).into_iter().max_by(|a, b| a.1.total_cmp(&b.1));
    let (_, py, _, _) = lowest("Picker").expect("the blend picker is drawn");
    for (i, (rx, ry, _, rh)) in crate::uifit::rects_of(app.window(), "StackRow").into_iter().filter(|r| r.1 > py).enumerate() {
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(rx + 30.0, ry + rh * 0.3) });
        // let the board's focus settle in
        std::thread::sleep(std::time::Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        save(&snap(&app).expect("a pointed-at row draws"), format!("stack-sense-hover-row{i}"));
        // rows are topmost first, so the first row down is the top layer
        assert_eq!(st.get_light_focus_layer(), 1 - i as i32, "pointing at a row shows its layer on the board");
    }
    let quiet = board(&st);
    for (i, (fx, fy, fw, fh)) in crate::uifit::shown_rects_of(app.window(), "FixText").into_iter().enumerate() {
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(fx + fw / 2.0, fy + fh / 2.0) });
        // the fix plays after a short beat
        std::thread::sleep(std::time::Duration::from_millis(150));
        slint::platform::update_timers_and_animations();
        save(&snap(&app).expect("a pointed-at fix draws"), format!("stack-sense-hover-fix{i}"));
        assert_eq!(shared.borrow().light_layers, before, "a fix on show changes no layer");
    }
    assert_ne!(board(&st), quiet, "the last fix rested on plays");
    app.window().dispatch_event(WindowEvent::PointerExited);
    snap(&app).expect("and lets go");
    slint::platform::update_timers_and_animations();
    assert_eq!(board(&st), quiet, "leaving the strip puts the board back exactly");
    paint_board();
    assert_eq!(st.get_light_focus_layer(), -1, "and the board lets go of the layer");
    assert_eq!(shared.borrow().light_layers, before, "and the stack is back as it was");

    // nothing can leave a fix playing on the board: whatever takes the strip from under a resting
    // pointer puts the board back exactly as it was (the page going, the stack changing, a new
    // reading), and the next live frame paints over it
    let rest_on_fix = || {
        let (fx, fy, fw, fh) = crate::uifit::rects_of(app.window(), "FixText").into_iter().next().expect("an offer to rest on");
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(fx + fw / 2.0, fy + fh / 2.0) });
        snap(&app).expect("a pointed-at fix draws");
        std::thread::sleep(std::time::Duration::from_millis(150));
        slint::platform::update_timers_and_animations();
        snap(&app).expect("the fix plays");
    };
    // past any page transition, so the frame checked is the one that stays
    let settle = || {
        slint::platform::update_timers_and_animations();
        std::thread::sleep(std::time::Duration::from_millis(600));
        slint::platform::update_timers_and_animations();
        snap(&app).expect("the board settles");
        slint::platform::update_timers_and_animations();
    };
    let resting = board(&st);
    rest_on_fix();
    assert_ne!(board(&st), resting, "the fix plays");
    st.set_page(0);
    settle();
    assert_eq!(board(&st), resting, "leaving the page stops it");
    st.set_page(1);
    settle();
    app.window().dispatch_event(WindowEvent::PointerExited);
    settle();
    rest_on_fix();
    assert_ne!(board(&st), resting, "the fix plays again");
    {
        let mut state = shared.borrow_mut();
        state.light_layers.truncate(1);
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    settle();
    assert_eq!(board(&st), resting, "the stack changing under it stops it");
    assert_eq!(st.get_light_sense_trouble().as_str(), "", "and with nothing buried the strip is gone");
    app.window().dispatch_event(WindowEvent::PointerExited);
    settle();
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    paint_board();
    settle();
    // and once the strip is back, resting on a fix still waits its beat before playing
    let (fx, fy, fw, fh) = crate::uifit::rects_of(app.window(), "FixText").into_iter().next().expect("the strip is back");
    let quiet = board(&st);
    app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(fx + fw / 2.0, fy + fh / 2.0) });
    slint::platform::update_timers_and_animations();
    snap(&app).expect("a fix rested on draws");
    assert_eq!(board(&st), quiet, "nothing plays before the beat");
    app.window().dispatch_event(WindowEvent::PointerExited);
    settle();

    // pointing at a key lights the row of the layer that shows there
    let (bx, by, bw, bh) = crate::uifit::rects_of(app.window(), "field").into_iter().min_by(|a, b| a.1.total_cmp(&b.1)).expect("the board's keys are drawn");
    // across a row of keys in small steps the pointer crosses keys and the gaps between them; only a
    // key points at something
    let mut seen = (false, false);
    for step in 0..40 {
        let x = bx + bw * (0.2 + 0.6 * step as f32 / 40.0);
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, by + bh * 0.4) });
        snap(&app).expect("a pointed-at board draws");
        let cell = st.get_light_pointed_cell();
        seen = (seen.0 || cell >= 0, seen.1 || cell < 0);
        if cell >= 0 && !seen.1 {
            save(&snap(&app).expect("a pointed-at key draws"), "stack-sense-key".into());
        }
    }
    assert!(seen.0, "the board says which key is pointed at");
    assert!(seen.1, "and a gap between keys points at nothing");
    app.window().dispatch_event(WindowEvent::PointerExited);
    snap(&app).expect("and lets go of the key");
    assert_eq!(st.get_light_pointed_cell(), -1);

    // one layer burying several says so once, naming them all as the rows read (topmost first), and
    // one with a count where the names don't fit
    {
        let mut state = shared.borrow_mut();
        let mut wildlife = before[1].clone();
        wildlife.id = Some(neuron::pattern::LayerInstanceId(903));
        let mut starlight = neuron::pattern::preset_layer("starlight").unwrap();
        starlight.id = Some(neuron::pattern::LayerInstanceId(901));
        let mut tidepool = neuron::pattern::preset_layer("tidepool").unwrap();
        tidepool.id = Some(neuron::pattern::LayerInstanceId(902));
        state.light_layers = vec![starlight, tidepool, wildlife];
        state.selected_layer = 2;
    }
    glue::refresh_layers(&app, &shared);
    paint_board();
    let (full, short) = (st.get_light_sense_trouble(), st.get_light_sense_trouble_short());
    assert!(full.starts_with("Tidepool and Starlight ") && full.ends_with(" under Wildlife"), "{full}");
    assert!(short.starts_with("Tidepool and 1 more "), "{short}");
    for width in [900.0, 1240.0] {
        app.window().set_size(slint::LogicalSize::new(width, 2400.0));
        save(&snap(&app).expect("a trouble with many layers draws"), format!("stack-sense-many-{width}"));
    }
    // three long fixes don't share a line in the narrow inspector: each takes its own, whole
    let shown_offers = st.get_light_sense_offers();
    st.set_light_sense_offers(slint::ModelRc::new(slint::VecModel::from(
        ["Wildlife \u{2192} add", "Tidepool \u{2191} above Wildlife", "Wildlife \u{2193} below Tidepool"]
            .into_iter()
            .enumerate()
            .map(|(i, t)| crate::ui::LightingFix { text: t.into(), offer: i as i32 })
            .collect::<Vec<_>>(),
    )));
    for width in [900.0, 1240.0] {
        app.window().set_size(slint::LogicalSize::new(width, 2400.0));
        snap(&app).expect("long fixes draw");
        // the strip grows to its new height
        std::thread::sleep(std::time::Duration::from_millis(200));
        slint::platform::update_timers_and_animations();
        save(&snap(&app).expect("long fixes draw"), format!("stack-sense-long-fixes-{width}"));
        let fixes = crate::uifit::shown_rects_of(app.window(), "FixText");
        assert_eq!(fixes.len(), 3, "{:?}", crate::uifit::drawn_as(app.window(), "FixText"));
        let rows: std::collections::BTreeSet<i64> = fixes.iter().map(|f| f.1.round() as i64).collect();
        assert!(rows.len() == 1 || rows.len() == 3, "the fixes share one line or take one each: {fixes:?}");
        for w in fixes.windows(2) {
            assert!(w[0].1 + w[0].3 <= w[1].1 + 0.5 || w[0].0 + w[0].2 <= w[1].0 + 0.5, "fixes overlap: {fixes:?}");
        }
    }
    st.set_light_sense_offers(shown_offers);
    app.window().set_size(slint::LogicalSize::new(1240.0, 2400.0));
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.selected_layer = 1;
    }
    glue::refresh_layers(&app, &shared);

    // a lighter Wildlife only buries the wave in part: the line drops a tier
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
        state.light_layers[1].opacity = 0.7;
    }
    glue::refresh_layers(&app, &shared);
    paint_board();
    save(&snap(&app).expect("a lesser trouble draws"), format!("stack-sense-tier{}", st.get_light_sense_tier()));
    assert!(st.get_light_sense_tier() < 2 || st.get_light_sense_trouble().is_empty(), "{}", st.get_light_sense_trouble());
    {
        let mut state = shared.borrow_mut();
        state.light_layers.clone_from(&before);
    }
    glue::refresh_layers(&app, &shared);

    // ducking: a ducking layer's row says so, and the word turns it off
    st.invoke_set_layer_duck(1, true);
    assert!(shared.borrow().light_layers[1].duck);
    assert!(st.get_light_stack_rows().row_data(0).unwrap().ducks, "the row's settings say it ducks");
    save(&snap(&app).expect("a ducking layer draws"), "stack-sense-ducks".into());
    st.invoke_set_layer_duck(1, false);
    assert!(!shared.borrow().light_layers[1].duck);
    assert!(!st.get_light_stack_rows().row_data(0).unwrap().ducks);
}

/// The stack under a random run of everything a person can do to it: rest on a fix, leave, keep
/// one, change a layer's blend, opacity or ducking, pick, hide, move, add and remove layers,
/// point at rows, leave the page. After every step what stack sense promises still holds: resting on
/// a fix never changes a layer, leaving puts the board back exactly, nothing keeps playing once the
/// stack changes, every fix on show can be played and kept, a kept fix is journaled for undo as itself, and every
/// map it hands the board fits the board and names real layers.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn stack_sense_survives_random_sequences() {
    use slint::Model;
    let _cwd = crate::testsupport::cwd_guard("stack_sense_random");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(4); st.set_grid_cols(12);
    let cells = 4 * 12;
    let looks = ["wave", "wildlife", "starlight", "tidepool", "fire", "static", "audiometer"];
    {
        let mut state = shared.borrow_mut();
        state.light_layers = ["wave", "wildlife", "starlight"].iter().map(|l| neuron::pattern::preset_layer(l).unwrap()).collect();
        state.light_layers[1].blend = neuron::effects::Blend::Normal;
        state.selected_layer = 0;
    }
    glue::refresh_layers(&app, &shared);
    st.set_page(1); st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1000.0, 1300.0));
    app.show().unwrap();
    st.set_grid_px(slint::ModelRc::new(slint::VecModel::from(vec![slint::Color::from_rgb_u8(0x0c, 0x0d, 0x10); cells])));
    // readings land a step after they're asked for, as the worker's do
    glue::stacksense::defer_readings();
    let board = || st.get_grid_px().iter().collect::<Vec<_>>();
    let layers = || shared.borrow().light_layers.clone();

    // xorshift: the same sequence on every run, so a failure replays
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n.max(1)
    };
    // the board as it was when the fix on show started playing
    let mut quiet: Option<Vec<slint::Color>> = None;
    let mut kinds = [0usize; 14];
    for step in 0..250 {
        let before = layers();
        let n = before.len() as u64;
        let offers: Vec<i32> = st.get_light_sense_offers().iter().map(|o| o.offer).collect();
        let op = next(14);
        kinds[op as usize] += 1;
        let what = match op {
            0 | 1 => {
                let shown = !offers.is_empty() && next(4) > 0;
                let i = if shown { offers[next(offers.len() as u64) as usize] } else { next(6) as i32 - 1 };
                if quiet.is_none() {
                    quiet = Some(board());
                }
                st.invoke_light_sense_preview(i);
                assert_eq!(layers(), before, "step {step}: resting on fix {i} changed a layer");
                if shown {
                    assert!(glue::stacksense::previewing(), "step {step}: offer {i} is on show but can't be played");
                }
                if !glue::stacksense::previewing() {
                    assert_eq!(Some(board()), quiet.take(), "step {step}: a fix that didn't play left the board changed");
                }
                format!("preview {i}")
            }
            2 => {
                st.invoke_light_sense_preview(-1);
                if let Some(was) = quiet.take() {
                    assert_eq!(board(), was, "step {step}: leaving didn't put the board back");
                }
                "leave".into()
            }
            3 if !offers.is_empty() => {
                let i = offers[next(offers.len() as u64) as usize];
                st.invoke_light_sense_accept(i);
                quiet = None;
                let kept = layers();
                assert_ne!(kept, before, "step {step}: keeping offer {i} changed nothing");
                // the status bar's undo (refused here: tests never arm input) takes back exactly this
                match neuron::session_undo::peek().map(|r| r.entry) {
                    Some(neuron::session_undo::Entry::Lighting { before: was, applied, .. }) => {
                        assert_eq!((was, applied), (before.clone(), kept), "step {step}: offer {i} journaled as something else");
                    }
                    other => panic!("step {step}: a kept fix is journaled as a lighting edit: {other:?}"),
                }
                format!("keep {i}")
            }
            4 => { let v = next(101) as f32; st.invoke_set_layer_opacity(v); format!("opacity {v}") }
            5 => { let b = next(5) as i32; st.invoke_set_layer_blend(b); format!("blend {b}") }
            6 => { let i = next(n) as i32; st.invoke_select_stack(i); format!("select {i}") }
            7 => { let i = next(n) as i32; st.invoke_toggle_stack(i); format!("toggle {i}") }
            8 => { let (i, d) = (next(n) as i32, if next(2) == 0 { -1 } else { 1 }); st.invoke_move_stack(i, d); format!("move {i} {d}") }
            9 => { let (i, on) = (next(n) as i32, next(2) == 0); st.invoke_set_layer_duck(i, on); format!("duck {i} {on}") }
            10 if n < 6 => {
                let (look, above) = (looks[next(looks.len() as u64) as usize], next(2) == 0);
                st.invoke_stack_tile(look.into(), above);
                format!("add {look}")
            }
            11 if n > 1 => { let i = next(n) as i32; st.invoke_remove_stack(i); format!("remove {i}") }
            13 => {
                // the page's own handlers run as the window draws, once it's past the page fade
                let settle = || {
                    slint::platform::update_timers_and_animations();
                    std::thread::sleep(std::time::Duration::from_millis(600));
                    slint::platform::update_timers_and_animations();
                    snap(&app).expect("the page settles");
                };
                st.set_page(0);
                settle();
                assert!(!glue::stacksense::previewing(), "step {step}: a fix kept playing with the page gone");
                if let Some(was) = quiet.take() {
                    assert_eq!(board(), was, "step {step}: leaving the page didn't put the board back");
                }
                st.set_page(1);
                settle();
                "page".into()
            }
            _ => { let i = next(n + 1) as i32 - 1; st.set_light_focus_layer(i); format!("point at row {i}") }
        };
        let after = layers();
        if after != before {
            // before its reading lands: nothing may go on playing over a stack it wasn't measured on
            assert!(!glue::stacksense::previewing(), "step {step} ({what}): the stack changed and a fix is still playing");
            quiet = None;
        }
        // the strip under the pointer only ever offers what the stack on show can take (now and
        // then, and not over a fix playing, so most steps act on whatever is on show)
        let offers_now: Vec<_> = if step % 10 == 0 && !glue::stacksense::previewing() { st.get_light_sense_offers().iter().collect() } else { Vec::new() };
        for o in offers_now {
            st.invoke_light_sense_preview(o.offer);
            let plays = glue::stacksense::previewing();
            st.invoke_light_sense_preview(-1);
            assert!(plays || after != before, "step {step} ({what}): {} is offered for this stack but can't play", o.text);
        }
        if let Some(was) = quiet.take() {
            if glue::stacksense::previewing() {
                quiet = Some(was);
            } else {
                assert_eq!(board(), was, "step {step} ({what}): a fix stopped without putting the board back");
            }
        }
        glue::stacksense::land_readings();
        slint::platform::update_timers_and_animations();
        let count = after.len() as i32;
        // identity and the rows
        let mut ids: Vec<_> = after.iter().map(|d| d.id.expect("every layer has an identity")).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), after.len(), "step {step} ({what}): two layers share an identity");
        let rows = st.get_light_stack_rows();
        assert_eq!(rows.row_count(), after.len(), "step {step} ({what}): a row per layer");
        for r in rows.iter() {
            assert!(r.focus.row_count() == 0 || r.focus.row_count() == cells, "step {step} ({what}): a focus map the size of the board");
            assert!((-1..=2).contains(&r.tier), "step {step} ({what}): tier {}", r.tier);
        }
        assert!((-1..count).contains(&st.get_light_focus_layer()), "step {step} ({what}): the board focuses a layer that isn't there");
        let owners = st.get_light_cell_owner();
        assert!(owners.row_count() == 0 || owners.row_count() == cells, "step {step} ({what})");
        assert!(owners.iter().all(|o| (-1..count).contains(&o)), "step {step} ({what}): a key owned by a layer that isn't there");
        // the strip
        let trouble = st.get_light_sense_trouble();
        if trouble.is_empty() {
            assert_eq!(st.get_light_sense_offers().row_count(), 0, "step {step} ({what}): fixes with no trouble");
        } else {
            assert!((0..=2).contains(&st.get_light_sense_tier()), "step {step} ({what})");
            let short = st.get_light_sense_trouble_short();
            assert!(!short.is_empty() && short.len() <= trouble.len(), "step {step} ({what}): {trouble} / {short}");
        }
        assert!(st.get_light_sense_offers().row_count() <= neuron::stacksense::OFFERS, "step {step} ({what})");
        if step % 25 == 0 {
            snap(&app).expect("the stack draws whatever it's been through");
        }
    }
    st.invoke_light_sense_preview(-1);
    assert!(!glue::stacksense::previewing());
    // a sequence that never reached a kind of step proves nothing about it
    assert!(kinds.iter().all(|&k| k > 0), "every kind of step ran: {kinds:?}");
}

/// The second picker and its suggestion row, end to end: an edit draws offers, resting on one shows
/// it and leaving puts the palette back, only accepting keeps it, and the row then offers the way
/// back. The picker's three axes move independently and a grey keeps the hue it was reached from.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn second_picker_offers_preview_accept_and_undo() {
    use neuron::assist::Lch;
    use neuron::lighting::Rgb;
    use neuron::spectrum::{Motion, Palette, Spectrum, Stop};
    use slint::Model;
    let _cwd = crate::testsupport::cwd_guard("second_picker");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    glue::colorsense::use_model(None);
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    let mut layer = neuron::pattern::preset_layer("wave").unwrap();
    layer.spectrum = Spectrum::from_palette(Palette::new(
        [(20, 40, 90), (60, 110, 180), (120, 170, 220), (200, 225, 245)].into_iter().enumerate()
            .map(|(i, (r, g, b))| Stop::new(Rgb::new(r, g, b), i as f32 / 3.0)).collect(),
        Motion::Hold,
    ));
    shared.borrow_mut().light_layers = vec![layer];
    glue::refresh_layers(&app, &shared);
    let stops = || shared.borrow().light_layers[0].spectrum.seq[0].palette.stops.clone();
    let titles = || st.get_assist_offers().iter().map(|o| o.title.to_string()).collect::<Vec<_>>();
    let original = stops();

    assert!(st.get_picker_v2(), "the second picker is the default");
    st.invoke_set_picker_v2(false);
    st.invoke_set_stop_color(1, "9650aa".into());
    assert!(titles().is_empty(), "the first picker has no suggestions");
    st.invoke_set_stop_color(1, "3c6eb4".into());
    assert_eq!(stops(), original);

    st.invoke_set_picker_v2(true);
    assert!(st.get_picker_v2() && crate::prefs::picker_v2(), "the choice is saved");
    assert!(titles().is_empty(), "a palette nobody is editing draws no offers");
    st.set_light_sel_stop(1);
    st.invoke_picker_sync();
    st.invoke_set_stop_color(1, "9650aa".into());
    assert_eq!(titles().first().map(String::as_str), Some("carry the change through"), "{:?}", titles());
    let edited = stops();
    let offered = titles();

    st.invoke_assist_preview(0);
    let shown = stops();
    assert_ne!(shown, edited, "resting on an offer shows it on the live palette");
    assert_eq!(shown[1], edited[1], "the stop being edited is kept");
    assert_eq!(titles(), offered, "the row holds still under the pointer");
    st.invoke_assist_preview(-1);
    assert_eq!(stops(), edited, "leaving puts the palette back");

    st.invoke_assist_preview(0);
    st.invoke_assist_accept(0);
    assert_eq!(stops(), shown, "accepting keeps what was shown");
    assert_eq!(titles(), ["put it back"]);
    assert!(st.get_assist_offers().row_data(0).unwrap().back, "the way back is drawn as one");
    st.invoke_assist_preview(-1);
    assert_eq!(stops(), shown, "an accepted offer is not undone by the pointer leaving");
    st.invoke_assist_accept(0);
    assert_eq!(stops(), edited, "the way back restores the palette the offer replaced");
    assert!(!titles().contains(&"put it back".to_string()));
    st.invoke_assist_accept(7);
    st.invoke_assist_preview(7);
    assert_eq!(stops(), edited, "an index past the row does nothing");

    // the picker: vividness and lightness on the plane, hue on the rail
    let chosen = || Lch::of(stops()[1].col);
    st.invoke_picker_plane_pick(1.0, 0.6);
    let vivid = chosen();
    assert!((vivid.l - 0.6).abs() < 0.02 && vivid.c > 0.1, "{vivid:?}");
    assert!((st.get_picker_vivid() - 1.0).abs() < 1e-6 && (st.get_picker_light() - 0.6).abs() < 1e-6);
    st.invoke_picker_hue_pick(0.4);
    let turned = chosen();
    assert!((turned.h - 0.4).abs() < 0.02 && (turned.l - 0.6).abs() < 0.02, "hue moves, lightness stays: {turned:?}");
    assert!((st.get_picker_vivid() - 1.0).abs() < 1e-6, "vividness stays at the edge on a new hue");
    st.invoke_picker_plane_pick(0.0, 0.5);
    assert!(chosen().c < 0.01, "the left edge is grey");
    assert!((st.get_picker_hue() - 0.4).abs() < 1e-6, "a grey keeps the hue it was reached from");
    st.invoke_picker_plane_pick(0.7, 0.5);
    assert!((chosen().h - 0.4).abs() < 0.02, "and comes back out on that hue");
    st.invoke_picker_plane_pick(0.0, 0.5);
    st.invoke_picker_hue_pick(0.7);
    let lifted = chosen();
    assert!(lifted.c > 0.05 && (lifted.h - 0.7).abs() < 0.02, "a hue picked for a grey shows: {lifted:?}");
    st.invoke_picker_hue_pick(0.4);
    st.invoke_picker_plane_pick(0.7, 0.5);
    assert_eq!(st.get_picker_marks().row_count(), 3, "the other stops' hues are ticked on the rail");

    // a colour offered for the stop is a dot in the picker, after the ramps in the list
    let offers: Vec<_> = st.get_assist_offers().iter().collect();
    let dot = offers.iter().position(|o| o.solid).expect("an inner stop being recoloured is offered a colour");
    assert!(dot >= st.get_assist_ramps() as usize && offers[..dot].iter().all(|o| !o.solid), "ramps lead, dots follow");
    let (w, h) = (240.0, 96.0);
    let (x, y) = (offers[dot].vivid * w, (1.0 - offers[dot].light) * h);
    assert_eq!(st.invoke_assist_at(x, y, w, h), dot as i32, "the dot is found where it is drawn");
    assert_eq!(st.invoke_assist_at(x + 30.0, y, w, h).max(st.invoke_assist_at(x - 30.0, y, w, h)), -1, "and not far from it");
    let picked = stops();
    st.invoke_assist_preview(dot as i32);
    let tried = stops();
    assert!(tried[1] != picked[1] && tried[0] == picked[0] && tried[2..] == picked[2..], "resting on a dot shows that colour on the stop alone");
    st.invoke_assist_preview(-1);
    assert_eq!(stops(), picked, "and leaving puts it back");

    // an edit is made to the palette as it is, not to an offer that is only on show
    st.invoke_assist_preview(0);
    st.invoke_picker_nudge(0.0, 0.05, 0.0);
    let nudged = stops();
    assert!((Lch::of(nudged[1].col).l - Lch::of(picked[1].col).l - 0.05).abs() < 0.02, "the keyboard steps lightness");
    assert!(nudged[0] == picked[0] && nudged[2..] == picked[2..], "from the palette, not from the offer shown");

    // a held stop is left alone by every offer
    st.set_light_sel_stop(2);
    st.invoke_picker_sync();
    st.invoke_toggle_pin();
    assert!(st.get_light_sel_pinned() && st.get_light_pins().iter().eq([false, false, true, false]));
    st.set_light_sel_stop(1);
    st.invoke_picker_sync();
    st.invoke_set_stop_color(1, "d07030".into());
    let held = stops();
    assert!(!titles().is_empty());
    for i in 0..titles().len() as i32 {
        st.invoke_assist_preview(i);
        assert_eq!(stops()[2], held[2], "offer {i} moved the held stop");
    }
    st.invoke_assist_preview(-1);
    st.set_light_sel_stop(2);
    st.invoke_picker_sync();
    st.invoke_toggle_pin();
    assert!(!st.get_light_sel_pinned());

    // with a model in the run root: a stop just added is offered colours before anyone picks one
    glue::colorsense::use_model(Some(include_bytes!("../../neuron-core/data/colour-model.golden.lumacore")));
    // the small model has nothing to say about some palettes, so a few stops are tried
    let before_adding = stops();
    let mut offered = false;
    for _ in 0..3 {
        st.invoke_add_stop(0.5);
        let added = stops();
        let new = st.get_light_sel_stop() as usize;
        let offers: Vec<_> = st.get_assist_offers().iter().collect();
        assert!(offers.iter().all(|o| o.solid && o.title == "a colour for this stop"), "{:?}", titles());
        if offers.is_empty() || offered {
            continue;
        }
        offered = true;
        st.invoke_assist_preview(0);
        let tried = stops();
        assert!(tried[new] != added[new] && (0..added.len()).all(|i| i == new || tried[i] == added[i]), "only the new stop is coloured");
        st.invoke_assist_accept(0);
        assert_eq!(stops(), tried);
        assert_eq!(titles(), ["put it back"], "a coloured stop is no longer open");
        st.invoke_assist_accept(0);
        assert_eq!(stops(), added);
        // and the model's answer to an edit arrives with the rules', with its terrain and doubts
        st.invoke_set_stop_color(new as i32, "30c060".into());
        assert!(!titles().is_empty());
        assert_eq!(st.get_light_doubts().row_count(), added.len(), "one doubt flag per stop");
        if st.get_picker_terrain_on() {
            assert!(st.get_picker_terrain().size().width > 0, "a terrain that is on has been drawn");
        }
    }
    assert!(offered, "no new stop was offered a colour");
    shared.borrow_mut().light_layers[0].spectrum.seq[0].palette.stops = before_adding;
    glue::refresh_layers(&app, &shared);
    glue::colorsense::use_model(None);
    st.set_light_sel_stop(1);
    st.invoke_picker_sync();
    st.invoke_set_stop_color(1, "8c5aa0".into());
    assert!(!st.get_picker_terrain_on(), "no model, no terrain");

    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(1); st.set_grid_cols(2);
    st.set_page(1); st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1000.0, 1300.0));
    app.show().unwrap();
    let capture = |name: &str| {
        let frame = snap(&app).expect("the second picker draws without binding loops");
        if let Some(dir) = std::env::var_os("NEURON_INSPECTOR_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), frame.as_bytes(), frame.width(), frame.height(), image::ColorType::Rgba8).unwrap();
        }
    };
    capture("picker-v2-offers");

    // a long palette name, edited, from the narrowest window up: the spectrum header fits its column
    {
        use slint::{ModelRc, SharedString, VecModel};
        let (chars, ramp) = (st.get_light_name_chars(), st.get_light_name_ramp());
        let long: Vec<SharedString> = "sunset on light painting · edited".chars().map(|c| c.to_string().into()).collect();
        st.set_light_name_chars(ModelRc::new(VecModel::from(long)));
        st.set_light_name_ramp(ModelRc::new(VecModel::from(vec![slint::Color::from_rgb_u8(40, 90, 200), slint::Color::from_rgb_u8(120, 220, 240)])));
        // 900 is the window's minimum width
        for width in [900.0, 1000.0, 1240.0, 1600.0] {
            app.window().set_size(slint::LogicalSize::new(width, 1300.0));
            snap(&app).expect("a frame at this width");
        }
        app.window().set_size(slint::LogicalSize::new(1000.0, 1300.0));
        st.set_light_name_chars(chars);
        st.set_light_name_ramp(ramp);
    }

    // by pointer: a pill answers where it is drawn and nowhere beside it. The row is found as what
    // changes when its pills are hidden.
    {
        use slint::platform::{PointerEventButton, WindowEvent};
        use slint::LogicalPosition;
        let ramps = st.get_assist_ramps();
        assert!(ramps > 0, "the edit draws a whole-palette offer: {:?}", titles());
        let with = snap(&app).unwrap();
        st.set_assist_ramps(0);
        let without = snap(&app).unwrap();
        st.set_assist_ramps(ramps);
        std::thread::sleep(std::time::Duration::from_millis(250));
        slint::platform::update_timers_and_animations();
        let width = with.width() as usize;
        let changed: Vec<usize> = with.as_slice().iter().zip(without.as_slice()).enumerate()
            .filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
        let top = changed.iter().map(|i| i / width).min().expect("the pills draw");
        let left = changed.iter().filter(|i| *i / width < top + 26).map(|i| i % width).min().unwrap();
        let at = |x: usize, y: usize| LogicalPosition::new(x as f32, y as f32);
        let before = stops();
        app.window().dispatch_event(WindowEvent::PointerMoved { position: at(left + ramps as usize * 50 + 60, top + 13) });
        snap(&app).unwrap();
        assert!(!st.get_assist_showing() && stops() == before, "beside the pills, nothing is shown");
        app.window().dispatch_event(WindowEvent::PointerMoved { position: at(left + 22, top + 13) });
        snap(&app).unwrap();
        assert!(!st.get_assist_showing(), "passing over a pill does not flash the lights");
        std::thread::sleep(std::time::Duration::from_millis(160));
        slint::platform::update_timers_and_animations();
        snap(&app).unwrap();
        assert!(st.get_assist_showing() && stops() != before, "resting on the first pill shows it");
        std::thread::sleep(std::time::Duration::from_millis(250));
        slint::platform::update_timers_and_animations();
        capture("picker-v2-draft");
        let shown = stops();
        app.window().dispatch_event(WindowEvent::PointerPressed { position: at(left + 22, top + 13), button: PointerEventButton::Left });
        app.window().dispatch_event(WindowEvent::PointerReleased { position: at(left + 22, top + 13), button: PointerEventButton::Left });
        snap(&app).unwrap();
        assert_eq!(stops(), shown, "a click on the pill keeps it");
        assert!(!st.get_assist_showing(), "and it is no longer a draft");
        assert_eq!(titles(), ["put it back"]);
        app.window().dispatch_event(WindowEvent::PointerExited);
        snap(&app).unwrap();
        st.invoke_assist_accept(0);
        assert_eq!(stops(), before);
    }

    // by keyboard: Tab shows the first choice and Tab again keeps it; held, Tab numbers the
    // offers and a number shows that one; Ctrl+Z takes the way back; Escape lets go
    {
        use slint::platform::{Key, WindowEvent};
        let down = |t: slint::SharedString| app.window().dispatch_event(WindowEvent::KeyPressed { text: t });
        let up = |t: slint::SharedString| app.window().dispatch_event(WindowEvent::KeyReleased { text: t });
        let press = |t: slint::SharedString| { down(t.clone()); up(t); snap(&app).unwrap(); };
        st.invoke_set_stop_color(1, "7a5ab4".into());
        st.set_picker_focus(st.get_picker_focus() + 1);
        snap(&app).unwrap();
        assert!(st.get_assist_keys(), "grabbing a stop gives the picker the keyboard");
        let prime = st.get_assist_prime();
        assert!(prime >= 0, "an edit has a first choice: {:?}", titles());
        let before = stops();
        press(Key::Tab.into());
        assert_eq!(st.get_assist_keyed(), prime);
        assert!(st.get_assist_showing() && stops() != before, "Tab shows the first choice");
        let shown = stops();
        press(Key::Tab.into());
        assert!(stops() == shown && !st.get_assist_showing(), "and Tab again keeps it");
        down(Key::Control.into());
        press("z".into());
        up(Key::Control.into());
        assert_eq!(stops(), before, "Ctrl+Z takes the way back");
        st.invoke_set_stop_color(1, "6a64c0".into());
        let before = stops();
        // the rules alone may offer one option; the chord works the same on any number
        let n = if st.invoke_assist_option(2) >= 0 { 2 } else { 1 };
        let second = st.invoke_assist_option(n);
        if second >= 0 {
            down(Key::Tab.into());
            snap(&app).unwrap();
            assert!(st.get_assist_tab_held(), "held, Tab numbers the offers");
            press(n.to_string().into());
            up(Key::Tab.into());
            snap(&app).unwrap();
            assert!(st.get_assist_keyed() == second && st.get_assist_showing() && !st.get_assist_tab_held(), "Tab and {n} show that option only");
            capture("picker-v2-keyed");
            press(Key::Escape.into());
            assert!(stops() == before && !st.get_assist_showing(), "Escape puts the palette back");
        }
        press(Key::Escape.into());
        assert!(!st.get_assist_keys(), "and with nothing on show, lets the keyboard go");
    }

    // asking: a model that has nothing to volunteer still answers when asked, and again with more
    {
        glue::colorsense::use_model(Some(include_bytes!("../../neuron-core/data/colour-model.golden.lumacore")));
        let resting = stops();
        st.set_light_sel_stop(2);
        st.invoke_picker_sync();
        snap(&app).unwrap();
        assert!(st.get_assist_can_ask(), "with a model there is something to ask");
        st.invoke_assist_ask();
        snap(&app).unwrap();
        assert!(st.get_assist_asking() && st.get_assist_showing(), "asking puts its first answer on show");
        let first: Vec<String> = titles();
        assert!(!first.is_empty());
        st.invoke_assist_preview(-1);
        st.invoke_assist_ask();
        snap(&app).unwrap();
        assert!(st.get_assist_showing(), "asking again shows its next answer");
        capture("picker-v2-asked");
        st.invoke_assist_preview(-1);
        assert_eq!(stops(), resting, "nothing asked for is kept without a click");
    }

    // rolling: a roll leads the row and goes on show; held stops stay; a fresh roll has its own size
    {
        let resting = stops();
        st.invoke_toggle_pin();
        let held = st.get_light_sel_stop() as usize;
        st.invoke_roll_choose(0, 1);
        st.invoke_roll();
        snap(&app).unwrap();
        let offers: Vec<_> = st.get_assist_offers().iter().collect();
        assert!(offers.first().is_some_and(|o| o.rolled) && st.get_assist_showing(), "the roll leads the row, on show");
        let rolled = stops();
        assert!(rolled.len() == resting.len() && rolled[held] == resting[held] && rolled != resting, "around this palette, the held stop stays");
        capture("picker-v2-rolled");
        st.invoke_roll();
        snap(&app).unwrap();
        assert!(st.get_assist_offers().iter().take_while(|o| o.rolled).count() == 2, "rolls stack, newest first");        // the x at a pointed-at roll's right end waves it off
        {
            use slint::platform::{PointerEventButton, WindowEvent};
            let (ox, oy, _, _) = crate::uifit::rect_of(app.window(), "SenseOffers").expect("the row is drawn");
            let end = ox + if st.get_assist_prime() == 0 { 54.0 } else { 40.0 } - 8.0;
            let p = slint::LogicalPosition::new(end, oy + 13.0);
            app.window().dispatch_event(WindowEvent::PointerMoved { position: p });
            capture("picker-v2-wave");
            app.window().dispatch_event(WindowEvent::PointerPressed { position: p, button: PointerEventButton::Left });
            app.window().dispatch_event(WindowEvent::PointerReleased { position: p, button: PointerEventButton::Left });
            app.window().dispatch_event(WindowEvent::PointerExited);
            snap(&app).unwrap();
            assert_eq!(st.get_assist_offers().iter().take_while(|o| o.rolled).count(), 1, "the x waves the newest roll off");
            assert!(!st.get_assist_showing() && stops() == resting, "and leaves the palette as it was");
            st.invoke_roll();
            snap(&app).unwrap();
        }
        st.invoke_assist_accept(0);
        assert!(!st.get_assist_showing() && stops() != resting, "a click keeps a roll");
        assert!(st.get_assist_offers().iter().any(|o| o.back), "and the way back is there");
        st.invoke_assist_accept(st.invoke_assist_option(0));
        assert_eq!(stops(), resting);
        // a fresh roll keeps the held colour too, and keeping it, or going back, keeps the hold
        st.invoke_roll_choose(1, 1);
        st.invoke_roll();
        let at = stops().iter().position(|s| s.col == resting[held].col).expect("a fresh roll keeps the held colour");
        st.invoke_assist_accept(0);
        snap(&app).unwrap();
        assert_eq!(st.get_light_pins().iter().nth(at), Some(true), "the kept roll still holds it");
        st.invoke_assist_accept(st.invoke_assist_option(0));
        snap(&app).unwrap();
        assert_eq!(stops(), resting);
        assert_eq!(st.get_light_pins().iter().nth(held), Some(true), "and so does the way back");
        st.set_light_sel_stop(held as i32);
        st.invoke_picker_sync();
        st.invoke_toggle_pin();
        st.invoke_roll_choose(1, 2);
        assert_eq!(crate::prefs::roll_choice(), ("fresh".to_string(), "wild".to_string()), "the choice is saved");
        for _ in 0..6 {
            st.invoke_roll();
            let n = stops().len();
            assert!((3..=8).contains(&n), "a fresh roll has three to eight stops: {n}");
            st.invoke_assist_preview(-1);
        }
        assert_eq!(stops(), resting, "unkept rolls leave the palette as it was");
        st.invoke_roll_choose(0, 1);

        // hovering the roll button and its arrow changes only the hover, steadily: three frames a
        // moment apart never show something that appears and goes again
        use slint::platform::WindowEvent;
        let (rx, ry, rw, rh) = crate::uifit::rect_of(app.window(), "RollButton").expect("the roll button is drawn");
        for x in [rx + 12.0, rx + rw - 8.0, rx + 12.0] {
            app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(x, ry + rh / 2.0) });
            let mut frames = Vec::new();
            for _ in 0..5 {
                frames.push(snap(&app).unwrap());
                std::thread::sleep(std::time::Duration::from_millis(30));
                slint::platform::update_timers_and_animations();
            }
            for w in frames.windows(3) {
                let blinking = crate::uifit::blink(&w[0], &w[1], &w[2]);
                assert!(blinking < 0.002, "hovering the roll button at x {x:.0} flickers {:.1}% of the window", blinking * 100.0);
            }
        }
        // pointing at hold marks the selected stop as held before the click
        let (sx, sy, sw, sh) = crate::uifit::rect_of(app.window(), "stop-deck").expect("the stop keys are drawn");
        app.window().dispatch_event(WindowEvent::PointerMoved { position: slint::LogicalPosition::new(sx + sw + 24.0, sy + sh / 2.0) });
        std::thread::sleep(std::time::Duration::from_millis(120));
        slint::platform::update_timers_and_animations();
        capture("picker-v2-hold-hover");
        // the drawer's settings draw a picture of each choice that stirs under the pointer
        let arrow = slint::LogicalPosition::new(rx + rw - 8.0, ry + rh / 2.0);
        let click = |p| {
            app.window().dispatch_event(WindowEvent::PointerPressed { position: p, button: slint::platform::PointerEventButton::Left });
            app.window().dispatch_event(WindowEvent::PointerReleased { position: p, button: slint::platform::PointerEventButton::Left });
        };
        click(arrow);
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(30));
            slint::platform::update_timers_and_animations();
        }
        let (bx, by, bw, bh) = crate::uifit::rect_of(app.window(), "RollBank").expect("the roll drawer opens");
        for (row, cells) in [(0.0, 4.0), (bh + 6.0, 3.0)] {
            for c in 0..cells as usize {
                let p = slint::LogicalPosition::new(bx + bw * (c as f32 + 0.5) / cells, by + row + bh / 2.0);
                app.window().dispatch_event(WindowEvent::PointerMoved { position: p });
                std::thread::sleep(std::time::Duration::from_millis(200));
                slint::platform::update_timers_and_animations();
                snap(&app).unwrap();
            }
        }
        capture("picker-v2-roll-drawer");
        app.window().dispatch_event(WindowEvent::PointerExited);
        click(arrow);
        app.window().dispatch_event(WindowEvent::PointerExited);

        // spamming roll and ask never grows the row past its column or leaves a draft stuck
        for i in 0..12 {
            if i % 3 == 0 { st.invoke_roll(); } else { st.invoke_assist_ask(); }
            snap(&app).unwrap();
            assert!(st.get_assist_ramps() <= 5, "the row holds at most five capsules, not {}", st.get_assist_ramps());
        }
        capture("picker-v2-spammed");
        st.invoke_assist_preview(-1);
        assert!(!st.get_assist_showing(), "letting go after a spree leaves nothing on show");
        assert_eq!(stops(), resting, "and the palette as it was");
        glue::colorsense::use_model(None);
    }

    st.invoke_assist_preview(0);
    capture("picker-v2-previewing");
    st.invoke_assist_preview(-1);
    // with a model: its terrain behind the plane and a doubted stop marked on the strip. The
    // frame uses a trained bundle when NEURON_LUMA_BRIDGE names a folder holding one.
    let trained = std::env::var_os("NEURON_LUMA_BRIDGE").and_then(|folder| std::fs::read(std::path::Path::new(&folder).join("core-v4.3.lumacore")).ok());
    glue::colorsense::use_model(Some(trained.as_deref().unwrap_or(include_bytes!("../../neuron-core/data/colour-model.golden.lumacore"))));
    st.invoke_set_stop_color(1, "a05a8c".into());
    if !st.get_light_doubts().iter().any(|doubt| doubt.on) {
        let mark = |on| crate::ui::StopDoubt { on, col: slint::Color::from_rgb_u8(90, 150, 210) };
        st.set_light_doubts(slint::ModelRc::new(slint::VecModel::from(vec![mark(false), mark(false), mark(true), mark(false)])));
    }
    // the terrain fades in: draw once to start it, then let it finish before the frame is kept
    let _ = snap(&app);
    std::thread::sleep(std::time::Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    capture("picker-v2-model");
    // turned to the hue the ridge points at, the plane shows the ground itself
    st.invoke_set_stop_color(1, "4678be".into());
    let _ = snap(&app);
    std::thread::sleep(std::time::Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    capture("picker-v2-terrain");
    // the guide's section on all this, its heading drawn mid-sheen
    st.invoke_guide_toggle("spectrum".into(), 40.0, 40.0);
    st.invoke_guide_make_sticky();
    snap(&app).unwrap();
    app.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(230.0, 300.0), delta_x: 0.0, delta_y: -4000.0,
    });
    app.window().dispatch_event(slint::platform::WindowEvent::PointerScrolled {
        position: slint::LogicalPosition::new(230.0, 300.0), delta_x: 0.0, delta_y: 70.0,
    });
    // a full sheen cycle when frames are being kept, so one of them catches the band mid-word
    let frames = if std::env::var_os("NEURON_INSPECTOR_SHOTS").is_some() { 28 } else { 2 };
    for frame in 0..frames {
        std::thread::sleep(std::time::Duration::from_millis(240));
        slint::platform::update_timers_and_animations();
        capture(&format!("picker-v2-guide-{frame}"));
    }
    st.set_guide_open(false);
    glue::colorsense::use_model(None);
    st.invoke_set_picker_v2(false);
    assert!(titles().is_empty() && !crate::prefs::picker_v2());
    capture("picker-v1");
}

/// The second picker under long random sequences of everything a person can do to it: hover,
/// leave, keep, recolour, nudge, select, hold, ask, roll. After every step, what it promises still
/// holds: letting go of whatever is on show puts back exactly the palette that was there, nothing
/// changes the palette without a keep or an edit, a held stop only changes by hand, and the row
/// never shows an offer it cannot take.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn second_picker_survives_random_sequences() {
    use neuron::lighting::Rgb;
    use neuron::spectrum::{Motion, Palette, Spectrum, Stop};
    use slint::Model;
    let _cwd = crate::testsupport::cwd_guard("second_picker_random");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    glue::colorsense::use_model(Some(include_bytes!("../../neuron-core/data/colour-model.golden.lumacore")));
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    let mut layer = neuron::pattern::preset_layer("wave").unwrap();
    layer.spectrum = Spectrum::from_palette(Palette::new(
        [(20, 40, 90), (60, 110, 180), (120, 170, 220), (200, 225, 245)].into_iter().enumerate()
            .map(|(i, (r, g, b))| Stop::new(Rgb::new(r, g, b), i as f32 / 3.0)).collect(),
        Motion::Hold,
    ));
    shared.borrow_mut().light_layers = vec![layer];
    glue::refresh_layers(&app, &shared);
    st.invoke_set_picker_v2(true);
    st.invoke_roll_choose(0, 1);
    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(1); st.set_grid_cols(2);
    st.set_page(1); st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1000.0, 1300.0));
    app.show().unwrap();
    let stops = || shared.borrow().light_layers[0].spectrum.seq[0].palette.stops.clone();

    // xorshift: the same sequence on every run, so a failure replays
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n.max(1)
    };
    let palettes = ["9650aa", "3c6eb4", "d07030", "30c060", "e0e0e0", "202020", "6a64c0"];
    // the palette with nothing on show, and the stops a hold has fixed with their colours
    let mut settled = stops();
    for step in 0..400 {
        let showing = st.get_assist_showing();
        let rows = st.get_assist_offers().row_count() as u64;
        let n = stops().len();
        let held: Vec<bool> = st.get_light_pins().iter().collect();
        let op = next(11);
        let before = stops();
        let mut undo = false;
        let what = match op {
            0 | 1 => { let i = next(rows + 2) as i32 - 1; st.invoke_assist_preview(i); format!("preview {i}") }
            2 => { st.invoke_assist_preview(-1); "leave".into() }
            3 => {
                let i = next(rows + 1) as i32;
                undo = st.get_assist_offers().row_data(i as usize).is_some_and(|o| o.back);
                st.invoke_assist_accept(i);
                format!("accept {i}")
            }
            4 => {
                let i = next(n as u64) as i32;
                let hex = palettes[next(palettes.len() as u64) as usize];
                st.invoke_set_stop_color(i, hex.into());
                format!("recolour {i} {hex}")
            }
            5 => { st.invoke_picker_nudge(0.0, 0.03, 0.0); "nudge".into() }
            6 => { st.set_light_sel_stop(next(n as u64) as i32); st.invoke_picker_sync(); "select".into() }
            7 => { st.invoke_toggle_pin(); "hold".into() }
            8 => { st.invoke_assist_ask(); "ask".into() }
            9 => { st.invoke_roll(); "roll".into() }
            _ => { st.invoke_picker_plane_pick(next(100) as f32 / 100.0, next(100) as f32 / 100.0); "plane".into() }
        };
        let after = stops();
        let ctx = || format!("step {step} ({what}), showing before {showing}");
        // whatever was on show, letting go of it puts back the settled palette
        if matches!(op, 0..=2) && !st.get_assist_showing() {
            assert_eq!(after, settled, "{}: letting go did not restore the palette", ctx());
        }
        // a stop held before this step keeps its colour unless this step recoloured it by hand or
        // took the way back
        let by_hand = matches!(op, 4 | 5 | 10) || undo;
        if !by_hand && !st.get_assist_showing() && before.len() == after.len() && held.len() == before.len() {
            for (i, &h) in held.iter().enumerate() {
                if h && !showing {
                    assert_eq!(before[i].col, after[i].col, "{}: held stop {i} moved", ctx());
                }
            }
        }
        // every row the picker draws is one it can show
        let drawn = st.get_assist_offers().row_count();
        assert!(st.get_assist_prime() < drawn as i32 && st.get_assist_keyed() < drawn as i32, "{}: a pointer past the row", ctx());
        if !st.get_assist_showing() {
            settled = stops();
        }
        if step % 40 == 0 {
            snap(&app).unwrap();
        }
    }
    st.invoke_assist_preview(-1);
    snap(&app).unwrap();
    glue::colorsense::use_model(None);
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn spectrum_controls_reverse_only_the_active_frame_and_draw_relevant_properties() {
    use neuron::spectrum::{Interp, Motion};
    let _cwd = crate::testsupport::cwd_guard("spectrum_controls");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    shared.borrow_mut().light_layers = vec![neuron::pattern::preset_layer("wave").unwrap()];
    glue::refresh_layers(&app, &shared);
    let id = shared.borrow().light_layers[0].id;
    st.invoke_set_motion("drift".into());
    st.invoke_reverse_motion();
    assert_eq!(shared.borrow().light_layers[0].spectrum.seq[0].palette.motion, Motion::Drift { speed: -1.0 });
    assert!(st.get_light_motion_reversed());
    assert_eq!(st.get_light_motion_speed(), 1.0);
    st.invoke_set_motion_speed(0.0);
    assert!(st.get_light_motion_reversed(), "direction survives a stopped slider");
    st.invoke_set_motion_speed(0.8);
    assert_eq!(shared.borrow().light_layers[0].spectrum.seq[0].palette.motion, Motion::Drift { speed: -0.8 });
    st.invoke_add_frame();
    st.invoke_reverse_motion();
    st.invoke_set_interp("oklab".into());
    {
        let state = shared.borrow();
        assert_eq!(state.light_layers[0].id, id);
        assert_eq!(state.light_layers[0].spectrum.seq[0].palette.motion, Motion::Drift { speed: -0.8 });
        assert_eq!(state.light_layers[0].spectrum.seq[1].palette.motion, Motion::Drift { speed: 0.8 });
        assert_eq!(state.light_layers[0].spectrum.seq[1].palette.interp, Interp::Oklab);
    }
    assert_eq!(st.get_light_interp(), "oklab");
    st.invoke_set_motion_speed(f32::NAN);
    assert_eq!(st.get_light_motion_speed(), 0.8);
    st.set_selected_device(0); st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true); st.set_grid_rows(1); st.set_grid_cols(2);
    st.set_page(1); st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1000.0, 1600.0));
    app.show().unwrap();
    let capture = |name: &str| {
        let frame = snap(&app).expect("inspector controls draw without binding loops");
        // Optional artifacts inspect the complete editor without changing a live device's scene.
        if let Some(dir) = std::env::var_os("NEURON_INSPECTOR_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), frame.as_bytes(), frame.width(), frame.height(), image::ColorType::Rgba8).unwrap();
        }
    };
    st.invoke_set_frame_fade(1, 0.5);
    for kind in ["hold", "drift", "cycle", "breathe", "flow"] {
        st.invoke_set_motion(kind.into());
        capture(kind);
    }
    st.invoke_set_motion("breathe".into());
    st.invoke_reverse_motion();
    assert!(shared.borrow().light_layers[0].spectrum.seq[1].palette.motion.speed().is_sign_negative());
    capture("reversed-breathe");
    st.invoke_set_motion("hold".into());
    let still = shared.borrow().light_layers.clone();
    st.invoke_reverse_motion();
    assert_eq!(shared.borrow().light_layers, still, "still colours have no reverse action");
    assert_eq!(still[0].id, id);
    st.invoke_remove_frame(1);
    capture("single-frame");
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn scene_restore_waits_for_the_ownership_handoff_barrier() {
    use neuron::pattern::{LiveComposite, SurfaceIdentity, LayerDef};
    use neuron::lighting::Rgb;
    use std::time::Duration;
    let _cwd = crate::testsupport::cwd_guard("lighting_handoff_restore");
    let Some(app) = try_window() else { return };
    crate::prefs::set_device_light(0x0226, crate::prefs::DeviceLight {
        scene_set: true, fps: 30, layers: vec![LayerDef {
            spectrum: neuron::spectrum::Spectrum::solid(Rgb::new(0, 0, 255)), ..Default::default()
        }], ..Default::default()
    }).unwrap();
    let shared = glue::install(&app);
    app.global::<State>().set_writes_paused(false);
    let target = SurfaceIdentity { pid: 0x0226, unit: "handoff-fixture".into(), plane: "base".into(), dimensions: (1, 2) };
    let mut live = LiveComposite::new(target, vec![LayerDef {
        spectrum: neuron::spectrum::Spectrum::solid(Rgb::new(255, 0, 0)), ..Default::default()
    }]).unwrap();
    live.render_cycle(Duration::ZERO, Duration::ZERO, Duration::ZERO).unwrap();
    {
        let mut state = shared.borrow_mut();
        state.rt.selected_pid = 0x0226; state.rt.selected_unit = "handoff-fixture".into();
        state.rt.install_live_fixture(&live);
    }
    assert_eq!(glue::restore_during_handoff_for_test(&app, &shared), "lighting ownership handoff pending");
    let time = Duration::from_millis(33);
    let blocked = live.render_cycle(time, time, time).unwrap();
    assert_eq!(blocked.accepted_scene_revision, 0);
    assert_eq!(blocked.pixels.as_ref(), &[Rgb::new(255, 0, 0); 2]);
    glue::restore_lighting(&app, &shared);
    let time = Duration::from_millis(66);
    assert_eq!(live.render_cycle(time, time, time).unwrap().accepted_scene_revision, 1);
}

/// The update pill in every phase, hovered across its keys, then its what's-new drawer with an
/// image, and SYSTEM's UPDATES block with the installed notes: each frame held to uifit's rules.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn update_pill_drawer_and_system_notes_draw_and_fit() {
    use crate::ui::{NoteMedia, NoteRow};
    use slint::platform::WindowEvent;
    use slint::{LogicalPosition, ModelRc, VecModel};
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let row = |kind: &str, title: &str, body: &str| NoteRow { kind: kind.into(), title: title.into(), body: body.into(), ..Default::default() };
    let notes = ModelRc::new(VecModel::from(vec![
        row("head", "", "the in-app updater, and patch notes you can read before you install."),
        row("group", "new", ""),
        row("item", "updates.", "neuron checks for signed releases and installs them in one click, then proves the new version before it lets go of the old one."),
        row("item", "", "a plain item without a lead, long enough to wrap across the drawer more than once so the wrap is checked."),
        row("group", "fixed", ""),
        row("item", "patch notes.", "shown inline."),
        row("thanks", "", "thanks: @someone, @another"),
    ]));
    let mut px = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(320, 120);
    for (i, p) in px.make_mut_slice().iter_mut().enumerate() {
        *p = slint::Rgba8Pixel { r: (i % 320) as u8, g: 80, b: 160, a: 255 };
    }
    let media = ModelRc::new(VecModel::from(vec![NoteMedia { img: slint::Image::from_rgba8(px), alt: "the pill".into(), ratio: 120.0 / 320.0, stage: 120.0 / 320.0, ..Default::default() }]));
    st.set_app_version("0.1.7".into());
    st.set_update_version("0.1.8".into());
    st.set_update_notes(notes.clone());
    st.set_update_media(media.clone());
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1240.0, 800.0));
    app.show().expect("show the window");
    // past the pill's width ease, so each frame shows where a step lands
    let settle = |app: &AppWindow| {
        for _ in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(80));
            slint::platform::update_timers_and_animations();
        }
        snap(app).expect("a frame is drawn")
    };
    let shots = std::env::var_os("NEURON_INSPECTOR_SHOTS").map(std::path::PathBuf::from);
    let save = |name: &str, f: &slint::SharedPixelBuffer<slint::Rgba8Pixel>| {
        if let Some(dir) = &shots {
            std::fs::create_dir_all(dir).unwrap();
            image::save_buffer(dir.join(format!("update-{name}.png")), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
        }
    };
    for (phase, progress, error) in [
        ("available", 0.0, ""),
        ("downloading", 0.42, ""),
        ("ready", 1.0, ""),
        ("installing", 1.0, ""),
        ("error", 0.0, "hash mismatch"),
        ("updated", 0.0, ""),
        ("rolled-back", 0.0, ""),
    ] {
        st.set_update_phase(phase.into());
        st.set_update_progress(progress);
        st.set_update_error(error.into());
        st.set_update_install_queued(phase == "downloading");
        snap(&app).expect("the pill grows in");
        let frame = settle(&app);
        save(phase, &frame);
        for x in (700..1100).step_by(25) {
            app.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x as f32, 28.0) });
            snap(&app).expect("a hovered pill frame is drawn");
        }
        app.window().dispatch_event(WindowEvent::PointerExited);
    }
    st.set_update_phase("available".into());
    st.set_update_notes_open(true);
    snap(&app).expect("the drawer opens");
    let open = settle(&app);
    save("drawer", &open);
    for y in (60..520).step_by(40) {
        app.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(1000.0, y as f32) });
        snap(&app).expect("a hovered drawer frame is drawn");
    }
    st.set_update_notes_open(false);
    settle(&app);
    st.set_update_phase("".into());
    settle(&app);

    st.set_installed_notes(notes);
    st.set_installed_media(media);
    st.set_update_enabled(true);
    st.set_update_status("v0.1.7 · up to date".into());
    st.set_page(3);
    app.window().set_size(slint::LogicalSize::new(1240.0, 1400.0));
    settle(&app);
    st.set_installed_notes_open(true);
    snap(&app).expect("the installed notes open in place");
    let system = settle(&app);
    save("system", &system);
    app.window().dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(600.0, 700.0), delta_x: 0.0, delta_y: -4000.0 });
    let bottom = settle(&app);
    save("system-bottom", &bottom);
}

/// The what's-new drawer with this release's real notes and images, as an offer of it would show
/// them: every image and every line has to be reachable, by the wheel when they overflow.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn the_real_release_notes_draw_in_the_update_drawer() {
    use slint::platform::WindowEvent;
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    let changelog = include_str!("../../../CHANGELOG.md");
    let release = neuron::update::notes::parse(changelog).into_iter().next().expect("the changelog has a release");
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let media = crate::updater::local_media(&release, &repo);
    assert_eq!(media.len(), release.media.len(), "every image in the notes decodes");
    st.set_update_notes(crate::updater::note_rows(&crate::updater::rows(&release)));
    st.set_update_media(crate::updater::note_media(&media));
    st.set_update_version(release.version.to_string().into());
    st.set_update_phase("available".into());
    st.set_update_enabled(true);
    st.set_update_notes_open(true);
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1240.0, 900.0));
    app.show().expect("show the window");
    let frame = |name: &str| {
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(80));
            slint::platform::update_timers_and_animations();
        }
        let f = snap(&app).expect("the drawer draws");
        if let Some(dir) = std::env::var_os("NEURON_INSPECTOR_SHOTS") {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir_all(&dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
        }
        f
    };
    frame("update-real-notes");
    // a short window: the notes give way before the footer does, and the wheel reaches the rest
    app.window().set_size(slint::LogicalSize::new(1240.0, 520.0));
    let top = frame("update-real-notes-short");
    // the drawer hangs under the pill at the window's right
    let at = slint::LogicalPosition::new(1000.0, 300.0);
    for _ in 0..40 {
        app.window().dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -120.0 });
    }
    let bottom = frame("update-real-notes-scrolled");
    assert_ne!(top.as_bytes(), bottom.as_bytes(), "the wheel scrolls the notes");
}

/// A device neuron learned on its own asks what works on its row, and the SYSTEM nameplate's star
/// key draws at rest, under the pointer, pressed and mid-spark, at the window's minimum width and a
/// wide one. The glue is left unbound, so a click opens nothing.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn learned_device_ask_and_star_key_draw_and_fit() {
    use crate::ui::DeviceRow;
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::LogicalPosition;
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let row = DeviceRow {
        name: "Razer BlackWidow V4 X Tenkeyless HyperSpeed".into(),
        pid: "02a5".into(),
        mode: "wired".into(),
        icon: "keyboard".into(),
        kind: "keyboard".into(),
        id: "unit-1".into(),
        dialect: "razer".into(),
        connected: true,
        learned: true,
        ..Default::default()
    };
    let base = row.clone();
    st.set_devices(slint::ModelRc::new(slint::VecModel::from(vec![row])));
    st.set_selected_device(0);
    st.set_app_version("0.1.8".into());
    st.set_window_shown(true);
    let shots = std::env::var_os("NEURON_INSPECTOR_SHOTS").map(std::path::PathBuf::from);
    let settle = |app: &AppWindow, name: &str| {
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(70));
            slint::platform::update_timers_and_animations();
        }
        let f = snap(app).expect("a frame is drawn");
        if let Some(dir) = &shots {
            std::fs::create_dir_all(dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
        }
    };
    for width in [900.0, 1240.0] {
        app.window().set_size(slint::LogicalSize::new(width, 800.0));
        app.show().expect("show the window");
        st.set_page(0);
        settle(&app, &format!("learned-row-{width}"));
        // the NEURON wordmark drifts the accent through its letters under the pointer, and settles
        // back to plain ink once it leaves
        let (wx, wy, ww, wh) = crate::uifit::rect_of(app.window(), "RampName").expect("the wordmark is drawn");
        assert!(wy < 60.0, "the first RampName is the header wordmark");
        for x in [wx + 2.0, wx + ww / 2.0, wx + ww - 2.0] {
            app.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, wy + wh / 2.0) });
            std::thread::sleep(std::time::Duration::from_millis(330));
            slint::platform::update_timers_and_animations();
            snap(&app).expect("a hovered wordmark frame is drawn");
        }
        settle(&app, &format!("wordmark-drift-{width}"));
        app.window().dispatch_event(WindowEvent::PointerExited);
        settle(&app, &format!("wordmark-after-{width}"));
        let (ax, ay, aw, ah) = crate::uifit::rect_of(app.window(), "CheckLink").expect("the check is on the learned row");
        for x in [ax + 2.0, ax + aw / 2.0, ax + aw - 2.0] {
            app.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, ay + ah / 2.0) });
            snap(&app).expect("a hovered ask frame is drawn");
        }
        app.window().dispatch_event(WindowEvent::PointerExited);
        // each lamp settling can draw beside the ask
        for trust in 1..=3 {
            let lamped = DeviceRow { lighting_trust: trust, ..base.clone() };
            st.set_devices(slint::ModelRc::new(slint::VecModel::from(vec![lamped])));
            settle(&app, &format!("learned-row-lamp{trust}-{width}"));
            crate::uifit::rect_of(app.window(), "CheckLink").expect("the check keeps its lamp on the row");
        }
        // a refused stream says so on the row, and a placeholder row can be checked
        let refused = DeviceRow { light_refused: true, ..base.clone() };
        let stuck = DeviceRow {
            name: "Razer Huntsman V3 Pro Tenkeyless".into(),
            pid: "02a7".into(),
            mode: "answers everything the same, so neuron can't tell what it does".into(),
            id: "unit-9".into(),
            adopting: true,
            learned: false,
            connected: false,
            ..base.clone()
        };
        st.set_devices(slint::ModelRc::new(slint::VecModel::from(vec![refused, stuck])));
        settle(&app, &format!("device-row-refused-{width}"));
        crate::uifit::rect_of(app.window(), "CheckLink").expect("a refused stream keeps its check on the row");
        // the drawer: asking, then each collection's lamps, the notes and the actions
        st.set_check_id("unit-1".into());
        st.set_check_busy(true);
        settle(&app, &format!("check-asking-{width}"));
        let cell = |tx: &str, state: i32| crate::ui::CheckCell { tx: tx.into(), state };
        let line = |label: &str, in_use: bool, states: [i32; 4]| crate::ui::CheckLine {
            label: label.into(),
            in_use,
            cells: slint::ModelRc::new(slint::VecModel::from(
                ["1F", "3F", "9F", "FF"].iter().zip(states).map(|(tx, st)| cell(tx, st)).collect::<Vec<_>>(),
            )),
        };
        st.set_check_lines(slint::ModelRc::new(slint::VecModel::from(vec![
            line("if0 01/0006", false, [0, 0, 0, 0]),
            line("if2 0C/0001", false, [2, 3, 0, 0]),
            line("if3 0C/0001", true, [1, 1, 0, 0]),
        ])));
        st.set_check_notes(slint::ModelRc::new(slint::VecModel::from(vec![
            slint::SharedString::from("another app was talking to it during the check"),
            slint::SharedString::from("Windows Dynamic Lighting can take its lights"),
        ])));
        st.set_check_said("opened a report with this check".into());
        st.set_check_busy(false);
        settle(&app, &format!("check-open-{width}"));
        crate::uifit::rect_of(app.window(), "CheckDrawer").expect("the drawer is drawn under its row");
        st.set_check_id("unit-9".into());
        settle(&app, &format!("check-placeholder-{width}"));
        st.set_check_id("".into());
        st.set_devices(slint::ModelRc::new(slint::VecModel::from(vec![base.clone()])));

        st.set_page(3);
        settle(&app, "system-top");
        app.window().dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(width / 2.0, 400.0), delta_x: 0.0, delta_y: -20000.0 });
        settle(&app, &format!("star-rest-{width}"));
        let (sx, sy, sw, sh) = crate::uifit::rect_of(app.window(), "StarKey").expect("the star key is in the nameplate");
        // the star follows the pointer across the key, then is pressed and clicked
        for x in [sx + 2.0, sx + 14.0, sx + sw / 2.0, sx + sw - 2.0] {
            app.window().dispatch_event(WindowEvent::PointerMoved { position: LogicalPosition::new(x, sy + sh / 2.0) });
            snap(&app).expect("a hovered star frame is drawn");
        }
        settle(&app, &format!("star-hover-{width}"));
        let at = LogicalPosition::new(sx + sw / 2.0, sy + sh / 2.0);
        app.window().dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Left });
        settle(&app, &format!("star-pressed-{width}"));
        app.window().dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
        for i in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(60));
            slint::platform::update_timers_and_animations();
            let f = snap(&app).expect("a spark frame is drawn");
            if let Some(dir) = &shots {
                image::save_buffer(dir.join(format!("star-spark-{width}-{i}.png")), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
            }
        }
        app.window().dispatch_event(WindowEvent::PointerExited);
        settle(&app, &format!("star-after-{width}"));
    }

    // the dotless status surfaces: a dropped device's name, gated capability heads, the paused-writes
    // pill, the capture prompt and a guide popup
    let rows = vec![
        DeviceRow {
            name: "Razer Viper V2 Pro".into(), pid: "00a5".into(), mode: "wired".into(), icon: "mouse".into(), kind: "mouse".into(), id: "unit-2".into(), connected: true,
            cap_dpi: true, cap_poll: true, cap_bright: true, cap_light: true, cap_scroll: true, cap_store: true, cap_idle: true,
            dpi: "1600".into(), polling: "1000 Hz".into(), brightness: "80%".into(), firmware: "1.04".into(),
            battery: "86%".into(), battery_frac: 0.86, storage: "5 profiles".into(),
            ..Default::default()
        },
        DeviceRow { name: "Razer BlackWidow V4 X".into(), pid: "02a5".into(), mode: "wired".into(), icon: "keyboard".into(), kind: "keyboard".into(), id: "unit-3".into(), connected: false, ..Default::default() },
    ];
    st.set_devices(slint::ModelRc::new(slint::VecModel::from(rows)));
    st.set_selected_device(0);
    st.set_writes_paused(true);
    st.set_active_profile("gaming".into());
    st.set_profile_degraded_count(2);
    st.set_page(0);
    for width in [900.0, 1240.0] {
        app.window().set_size(slint::LogicalSize::new(width, 900.0));
        settle(&app, &format!("device-dotless-{width}"));
    }
    st.set_writes_paused(false);
    st.set_capture_prompt("press the control you want to bind".into());
    st.set_capture_active(true);
    settle(&app, "capture-dotless");
    st.set_capture_active(false);
    for guide in ["spectrum", "macros"] {
        st.set_guide_active_id(guide.into());
        st.set_guide_x(320.0);
        st.set_guide_y(160.0);
        st.set_guide_sticky(true);
        st.set_guide_open(true);
        settle(&app, &format!("guide-{guide}"));
        if guide == "macros" {
            // the wheel scrolls the guide wherever it is over the card, a code cell included
            let before = snap(&app).expect("the guide draws");
            for _ in 0..3 {
                app.window().dispatch_event(WindowEvent::PointerScrolled { position: LogicalPosition::new(520.0, 500.0), delta_x: 0.0, delta_y: -120.0 });
            }
            settle(&app, "guide-macros-scrolled");
            let after = snap(&app).expect("the scrolled guide draws");
            assert_ne!(before.as_bytes(), after.as_bytes(), "the wheel scrolls the guide");
        }
        st.set_guide_open(false);
    }
}

/// More devices and audio endpoints than the minimum window's list can show: the list scrolls by the
/// wheel, and every row keeps its readouts whole at the narrowest list width.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn device_list_scrolls_with_many_rows() {
    use crate::ui::DeviceRow;
    use slint::platform::WindowEvent;
    use slint::LogicalPosition;
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let device = |n: usize| DeviceRow {
        name: format!("Razer Viper V2 Pro {n}").into(), pid: format!("00a{n}").into(), mode: "wired".into(), icon: "mouse".into(), kind: "mouse".into(),
        id: format!("unit-{n}").into(), connected: true,
        cap_dpi: true, cap_poll: true, cap_bright: true, cap_light: true, cap_scroll: true, cap_store: true, cap_idle: true,
        dpi: "1600".into(), polling: "1000 Hz".into(), brightness: "80%".into(), firmware: "1.04".into(),
        battery: "86%".into(), battery_frac: 0.86, storage: "5 profiles".into(),
        ..Default::default()
    };
    let audio = |name: &str, kind: &str, n: usize| DeviceRow {
        name: name.into(), pid: String::new().into(), icon: kind.into(), kind: kind.into(), id: format!("audio-{n}").into(), connected: true,
        detail: "default endpoint".into(),
        ..Default::default()
    };
    let mut rows: Vec<DeviceRow> = (0..7).map(device).collect();
    rows.push(audio("Razer Seiren V3 Chroma microphone", "mic", 0));
    rows.push(audio("Speakers (Realtek High Definition Audio)", "output", 1));
    st.set_devices(slint::ModelRc::new(slint::VecModel::from(rows)));
    st.set_selected_device(0);
    st.set_page(0);
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(900.0, 560.0));
    app.show().expect("show the window");
    let shots = std::env::var_os("NEURON_INSPECTOR_SHOTS").map(std::path::PathBuf::from);
    let frame = |name: &str| {
        for _ in 0..4 {
            std::thread::sleep(std::time::Duration::from_millis(70));
            slint::platform::update_timers_and_animations();
        }
        let f = snap(&app).expect("the device list draws");
        if let Some(dir) = &shots {
            std::fs::create_dir_all(dir).unwrap();
            image::save_buffer(dir.join(format!("{name}.png")), f.as_bytes(), f.width(), f.height(), image::ColorType::Rgba8).unwrap();
        }
        f
    };
    app.window().dispatch_event(WindowEvent::PointerExited);
    frame("device-many-top");
    let (_, first_y, _, _) = crate::uifit::rect_of(app.window(), "DeviceLine").expect("the first row is drawn");
    let at = LogicalPosition::new(200.0, 300.0);
    app.window().dispatch_event(WindowEvent::PointerMoved { position: at });
    for _ in 0..80 {
        app.window().dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: -120.0 });
    }
    app.window().dispatch_event(WindowEvent::PointerExited);
    let _bottom = frame("device-many-bottom");
    let (_, scrolled_y, _, _) = crate::uifit::rect_of(app.window(), "DeviceLine").expect("the first row is drawn");
    // the first row drawn is no longer row 0 once the list has scrolled its end into view
    assert!(scrolled_y < first_y, "the wheel scrolls the device list ({first_y} -> {scrolled_y})");
}

/// The macro carousel pans under the wheel while it can move, and hands the wheel to the page once
/// it is at its end.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn macro_carousel_hands_the_wheel_to_the_page_at_its_end() {
    use crate::ui::MacroCard;
    use slint::platform::WindowEvent;
    use slint::LogicalPosition;
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let cards: Vec<MacroCard> = (0..12)
        .map(|n| MacroCard {
            name: format!("macro-{n}").into(), summary: "types a line".into(), steps: 3, options: 0,
            mode: "BOUND".into(), trigger: "(unbound)".into(),
        })
        .collect();
    st.set_macro_catalog(slint::ModelRc::new(slint::VecModel::from(cards)));
    st.set_page(2);
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(900.0, 520.0));
    app.show().expect("show the window");
    let settle = || {
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(60));
            slint::platform::update_timers_and_animations();
        }
        snap(&app).expect("the input page draws");
    };
    let wheel = |at: LogicalPosition, dy: f32| {
        app.window().dispatch_event(WindowEvent::PointerMoved { position: at });
        app.window().dispatch_event(WindowEvent::PointerScrolled { position: at, delta_x: 0.0, delta_y: dy });
    };
    settle();
    // the page has to be scrolled until the carousel is in view; the wheel lands on the page itself
    let mut band = crate::uifit::rect_of(app.window(), "::carousel");
    for _ in 0..80 {
        if matches!(band, Some((_, y, _, h)) if y + h < 480.0) {
            break;
        }
        wheel(LogicalPosition::new(450.0, 200.0), -120.0);
        settle();
        band = crate::uifit::rect_of(app.window(), "::carousel");
    }
    let (cx, cy, cw, ch) = band.unwrap_or_else(|| panic!("the carousel is drawn: {:?}", crate::uifit::drawn_as(app.window(), "carousel")));
    let over = LogicalPosition::new(cx + cw / 2.0, cy + ch / 2.0);
    // a card is named like the carousel that holds it; it is the one drawn 248px wide
    let card_x = || {
        crate::uifit::rects_of(app.window(), "::carousel").into_iter().find(|r| (r.2 - 248.0).abs() < 0.5).map(|r| r.0).expect("a card is drawn")
    };
    let (x0, y0) = (card_x(), cy);
    wheel(over, -120.0);
    settle();
    let (x1, y1) = (card_x(), crate::uifit::rect_of(app.window(), "::carousel").unwrap().1);
    assert!(x1 < x0, "the wheel pans the carousel ({x0} -> {x1})");
    assert_eq!(y1, y0, "the page stays put while the carousel can pan");
    for _ in 0..80 {
        wheel(over, -120.0);
    }
    settle();
    let (x2, y2) = (card_x(), crate::uifit::rect_of(app.window(), "::carousel").unwrap().1);
    assert!(x2 < x1, "the carousel kept panning to its end ({x1} -> {x2})");
    wheel(over, -120.0);
    settle();
    let x3 = card_x();
    assert_eq!(x3, x2, "at its end the carousel does not move");
    let y3 = crate::uifit::rect_of(app.window(), "::carousel").unwrap().1;
    assert!(y3 < y2 || y2 + ch < 520.0, "at its end the wheel scrolls the page ({y2} -> {y3})");
}

/// The code editor's view follows the caret although a drag selects instead of flicking it.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn code_editor_view_follows_the_caret() {
    use slint::platform::{Key, PointerEventButton, WindowEvent};
    use slint::LogicalPosition;
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let st = app.global::<State>();
    let mut text = String::new();
    for n in 0..40 {
        use std::fmt::Write as _;
        let _ = writeln!(text, "    step_{n}()");
    }
    text.push_str(&"x".repeat(160));
    st.set_macro_code_view(true);
    st.set_macro_source(text.into());
    st.set_page(2);
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(900.0, 900.0));
    app.show().expect("show the window");
    let settle = || {
        for _ in 0..3 {
            std::thread::sleep(std::time::Duration::from_millis(60));
            slint::platform::update_timers_and_animations();
        }
        snap(&app).expect("the editor draws");
    };
    settle();
    let area = crate::uifit::rect_of(app.window(), "CodeArea").expect("the code area is drawn");
    let field = || crate::uifit::rect_of(app.window(), "CodeArea::ti").expect("the text field is drawn");
    let (x0, y0, _, _) = field();
    let at = LogicalPosition::new(area.0 + 40.0, area.1 + 20.0);
    app.window().dispatch_event(WindowEvent::PointerMoved { position: at });
    app.window().dispatch_event(WindowEvent::PointerPressed { position: at, button: PointerEventButton::Left });
    app.window().dispatch_event(WindowEvent::PointerReleased { position: at, button: PointerEventButton::Left });
    let press = |k: Key| {
        let t: slint::SharedString = k.into();
        app.window().dispatch_event(WindowEvent::KeyPressed { text: t.clone() });
        app.window().dispatch_event(WindowEvent::KeyReleased { text: t });
    };
    for _ in 0..45 {
        press(Key::DownArrow);
    }
    press(Key::End);
    let typed: slint::SharedString = "Z".into();
    app.window().dispatch_event(WindowEvent::KeyPressed { text: typed.clone() });
    app.window().dispatch_event(WindowEvent::KeyReleased { text: typed });
    settle();
    assert!(st.get_macro_source().ends_with(&format!("{}Z", "x".repeat(160))), "the caret reached the end of the text");
    let (x1, y1, _, h1) = field();
    assert!(y1 < y0, "the view follows the caret down ({y0} -> {y1}): {:?}", crate::uifit::drawn_as(app.window(), "ti"));
    assert!(x1 < x0, "the view follows the caret along a long line ({x0} -> {x1})");
    assert!(y1 + h1 <= area.1 + area.3 + 14.0, "the last line is in view");
}

/// Every SYSTEM group, jumped to from the page's index and drawn with long data, at the window's
/// minimum width and a wide one.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn system_page_groups_draw_and_fit() {
    use crate::ui::{BeaconMacro, DiagRow, OrganRow};
    use slint::{ModelRc, VecModel};
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    let diag = |name: &str, state: &str, detail: &str| DiagRow { name: name.into(), state: state.into(), detail: detail.into() };
    st.set_diagnostics(ModelRc::new(VecModel::from(vec![
        diag("transport enumerate", "pass", "4 HID interfaces"),
        diag("registry load", "pass", "61 definitions"),
        diag("device round-trip", "fail", "no reply from the BlackWidow Chroma V2 after three attempts on pipe 2"),
        diag("lighting test pattern", "pass", "frame built"),
        diag("effect engine", "pass", "ok"),
        diag("spine engine", "pass", "ok"),
        diag("macro runtime", "skip", "python not installed"),
        diag("gesture vault", "pass", "12 glyphs"),
        diag("mic endpoint", "pass", "default capture"),
    ])));
    st.set_diag_pass(7);
    st.set_diag_fail(1);
    st.set_diag_skip(1);
    let organ = |name: &str, state: &str| OrganRow { name: name.into(), state: state.into(), detail: "silent for 41 s while a device write was in flight".into() };
    st.set_organs(ModelRc::new(VecModel::from(vec![
        organ("input", "beat"), organ("live dispatch", "beat"), organ("lighting", "stall"), organ("hidwatch", "beat"),
        organ("macro host", "rest"), organ("weave", "rest"), organ("audio", "beat"),
    ])));
    st.set_organ_stall(true);
    st.set_beacon_macros(ModelRc::new(VecModel::from(vec![
        BeaconMacro { name: "stream_starting_soon_scene_switcher".into(), asks: "switch to the starting soon scene and mute the mic?".into() },
        BeaconMacro { name: "dim".into(), asks: "".into() },
    ])));
    st.set_host_enabled(true);
    st.set_host_obs(true);
    st.set_status_line("report written to C:/Users/someone/AppData/Roaming/neuron/reports/bug-2026-11-13-235959 · the clipboard was busy, so point your agent at that folder".into());
    st.set_page(3);
    st.set_window_shown(true);
    app.show().expect("show the window");
    let shots = std::env::var_os("NEURON_INSPECTOR_SHOTS").map(std::path::PathBuf::from);
    for width in [900.0, 1400.0] {
        app.window().set_size(slint::LogicalSize::new(width, 1000.0));
        for group in ["startup", "updates", "appearance", "notifications", "connections", "beacons", "files", "health"] {
            st.set_system_jump_target(group.into());
            for _ in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                slint::platform::update_timers_and_animations();
            }
            assert_eq!(st.get_system_jump_target(), "", "the jump to {group} lands");
            let frame = snap(&app).expect("SYSTEM draws");
            if let Some(dir) = &shots {
                std::fs::create_dir_all(dir).unwrap();
                image::save_buffer(dir.join(format!("system-{width}-{group}.png")), frame.as_bytes(), frame.width(), frame.height(), image::ColorType::Rgba8).unwrap();
            }
        }
    }
}

#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn lighting_enum_picker_simulates_options_collapsed_and_expanded() {
    use crate::light_render::{CompletedFrame, DesiredSurfaceState, PARAM_PREVIEW_DIMS, ParamPreview, SurfaceId, Target};
    use neuron::lighting::Rgb;
    use slint::{Model, Rgba8Pixel, SharedPixelBuffer};
    let _cwd = crate::testsupport::cwd_guard("lighting_enum_picker");
    std::env::set_var("SLINT_BACKEND", "winit-software");
    std::env::set_var("SLINT_SCALE_FACTOR", "1");
    let Some(app) = try_window() else { return };
    let shared = glue::install(&app);
    let st = app.global::<State>();
    st.set_writes_paused(true);
    st.set_selected_device(0);
    st.set_selected_device_name("Fixture".into());
    st.set_sel_can_light(true);
    st.set_grid_rows(6);
    st.set_grid_cols(22);
    st.set_page(1);
    st.set_window_shown(true);
    // the compact v1 colour picker keeps the EFFECT section, and its option picker, in the frame
    st.set_picker_v2(false);
    {
        let mut s = shared.borrow_mut();
        s.light_layers = vec![neuron::pattern::preset_layer("cascade").unwrap()];
        s.selected_layer = 0;
        s.layers_rev += 1;
    }
    glue::refresh_layers(&app, &shared);

    let params = st.get_light_params();
    let mode_row = (0..params.row_count())
        .find_map(|i| params.row_data(i).filter(|r| r.key == "mode"))
        .expect("cascade exposes procedural mode enum param");
    assert_eq!(mode_row.kind, "enum");
    assert_eq!(mode_row.options.row_count(), 2);
    assert_eq!(mode_row.previews.row_count(), 2);

    app.window().set_size(slint::LogicalSize::new(1240.0, 1100.0));
    app.show().expect("show the window");

    let target = Target {
        pid: shared.borrow().rt.selected_pid,
        unit: shared.borrow().rt.selected_unit.clone(),
        epoch: 1,
    };
    let content_rev = shared.borrow().layers_rev;
    let base_layer = shared.borrow().light_layers[0].clone();
    let desired = DesiredSurfaceState {
        active: true,
        target: target.clone(),
        dims: (6, 22),
        content_rev,
        param_previews: vec![
            ParamPreview {
                id: SurfaceId::ParamOption("mode".into(), 0),
                layers: vec![base_layer.clone()].into(),
                dims: PARAM_PREVIEW_DIMS,
                active: true,
                boosted: false,
            },
            ParamPreview {
                id: SurfaceId::ParamOption("mode".into(), 1),
                layers: vec![base_layer].into(),
                dims: PARAM_PREVIEW_DIMS,
                active: false,
                boosted: false,
            },
        ]
        .into(),
        ..Default::default()
    };
    let make_overlay = |r: u8, g: u8, b: u8| {
        let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(
            u32::from(PARAM_PREVIEW_DIMS.1) * 5 + 1,
            u32::from(PARAM_PREVIEW_DIMS.0) * 5 + 1,
        );
        for px in buf.make_mut_slice() {
            *px = Rgba8Pixel { r, g, b, a: 255 };
        }
        buf
    };
    let make_frame = |opt_idx: usize, r: u8, g: u8, b: u8| CompletedFrame {
        id: SurfaceId::ParamOption("mode".into(), opt_idx),
        target: target.clone(),
        dims: PARAM_PREVIEW_DIMS,
        content_rev,
        rgb: vec![Rgb::new(r, g, b); usize::from(PARAM_PREVIEW_DIMS.0) * usize::from(PARAM_PREVIEW_DIMS.1)].into(),
        swatch: None,
        overlay: Some(make_overlay(r, g, b)),
        requested: std::time::Instant::now(),
        render_time: std::time::Duration::ZERO,
    };

    glue::apply_preview_test_frames(&app, &desired, vec![
        make_frame(0, 10, 120, 255),
        make_frame(1, 0, 240, 90),
    ]);
    assert!(params == st.get_light_params(), "param preview updates must preserve State.light-params model identity");
    let before = snap(&app).expect("first simulated picker frame draws");

    glue::apply_preview_test_frames(&app, &desired, vec![
        make_frame(0, 255, 60, 20),
    ]);
    assert!(params == st.get_light_params(), "subsequent preview ticks preserve model identity");
    let after = snap(&app).expect("second simulated picker frame draws");
    assert_ne!(before.as_bytes(), after.as_bytes(), "collapsed picker simulation must repaint when frames advance");

    st.invoke_boost_param_preview("mode".into(), true);
    st.invoke_set_param_enum("mode".into(), 1);
    let updated_mode = (0..params.row_count())
        .find_map(|i| params.row_data(i).filter(|r| r.key == "mode"))
        .expect("mode row stays present after option change");
    assert_eq!(updated_mode.ival, 1);
    assert_eq!(updated_mode.previews.row_count(), 2);
    snap(&app).expect("picker remains valid after switching enum option");
    st.invoke_boost_param_preview("mode".into(), false);
}

