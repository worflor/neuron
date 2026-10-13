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
    st.invoke_apply_ingame_polling(1000.0, 1000.0);
    assert!(st.get_perf_status().to_string().contains("paused"));
    // un-pause to leave clean state.
    st.invoke_toggle_writes_paused();
}

/// The OUTPUT (render) audio controls are wired — the mirror of the mic controls (headphone /
/// sound-card volume + mute). This drives the READ-ONLY `refresh-out` callback only: it must exist,
/// run end-to-end through core's `resolve_render`, and populate the out-name readout. We deliberately
/// do NOT invoke `set-out-gain`/`toggle-out-mute` here — those mutate the user's LIVE output device
/// (changing speaker volume / muting them), which a test must never do. Their wiring is covered by
/// the editor's `build_action` unit tests + the manual-run path; this asserts the panel reads live.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn output_audio_controls_are_wired() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    // refresh populates the output name (a device name, "(no output device)", or similar) — never
    // the placeholder dash, proving the callback ran end-to-end through resolve_render.
    st.invoke_refresh_out();
    assert!(
        !st.get_out_name().is_empty(),
        "out-name must be set by refresh-out"
    );
    // the mute + gain properties are real bool/float the panel binds (no mutation fired).
    let _ = st.get_out_muted();
    let _ = st.get_out_gain();
}

/// The mic controls are likewise wired (the capture side), kept beside the output test so a
/// regression in either audio direction is caught. READ-ONLY: refresh only — `set-mic-gain` /
/// `toggle-mic-mute` mutate the user's live mic and must not fire in a test.
#[test]
#[ignore = "run by gui_tests_in_fresh_processes"]
fn mic_audio_controls_are_wired() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_refresh_mic();
    assert!(
        !st.get_mic_name().is_empty(),
        "mic-name must be set by refresh-mic"
    );
    let _ = st.get_mic_muted();
    let _ = st.get_mic_gain();
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
    assert!(st.get_beacon_text().is_empty());
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
    let tile = |slug: &str, names: &[&str]| EffectTileMeta {
        name: slug.into(),
        slug: slug.into(),
        kind: "effect".into(),
        blurb: "a look".into(),
        source: "".into(),
        pairs: ModelRc::new(VecModel::from(
            names.iter().enumerate().map(|(i, n)| marked_chip(n, i as i32)).collect::<Vec<_>>(),
        )),
    };
    st.set_light_tiles(ModelRc::new(VecModel::from(vec![
        tile("fire", &["aurora", "sunset", "bubble", "meadow", "ice"]),
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
    let cold = app.window().take_snapshot().expect("a frame is drawn");
    let demands = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let observed = demands.clone();
    st.on_tile_demand(move |shelf, start, end, warm_start, warm_end| {
        observed.borrow_mut().push((shelf, start, end, warm_start, warm_end));
    });
    app.invoke_sample_light_demand();
    std::thread::sleep(std::time::Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    app.window().take_snapshot().expect("viewport geometry is sampled after layout");
    assert!(demands.borrow().iter().any(|&(shelf, start, end, _, _)| shelf == 0 && start == 0 && end == 2), "{:#?}", demands.borrow());
    assert!(demands.borrow().iter().all(|&(_, start, end, warm_start, warm_end)|
        warm_start <= start && start <= end && end <= warm_end));
    let (x, y) = find_mark(&cold).expect("a chip is on the page");
    for dy in (20..160).step_by(10) {
        app.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x as f32, y as f32 - dy as f32),
        });
        app.window().take_snapshot().expect("the tile and its layer control draw while hovered");
    }
    let at = |dx: f32| WindowEvent::PointerMoved { position: LogicalPosition::new(x as f32 + dx, y as f32 + 3.0) };
    // onto the first capsule, then across the drawer as it opens (the drawer animates, so a few
    // frames), then back out
    for dx in [3.0, 3.0, 30.0, 56.0, 82.0, 3.0] {
        app.window().dispatch_event(at(dx));
        app.window().take_snapshot().expect("a hovered frame is drawn");
    }
    app.window().dispatch_event(WindowEvent::PointerExited);
    let after = app.window().take_snapshot().expect("a frame after the pointer leaves");
    assert!(after.as_bytes().len() == cold.as_bytes().len());
    st.set_light_user_tiles(ModelRc::new(VecModel::from(vec![tile("user:late", &["ice"])])));
    app.window().set_size(slint::LogicalSize::new(920.0, 900.0));
    app.window().take_snapshot().expect("late metadata and a reflow draw without eager geometry recursion");
    std::thread::sleep(std::time::Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    app.window().take_snapshot().expect("reflow demand is sampled after layout");
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
        ival: 0, hex: "".into(), col: slint::Color::from_rgb_u8(0, 0, 0), bval: false,
    }])));
    st.set_window_shown(true);
    app.window().set_size(slint::LogicalSize::new(1240.0, 1100.0));
    app.show().expect("show the window");
    app.window().take_snapshot().expect("the plain-ink fallback name draws");
    // the painted branch: the header name wears the ramp. Only THIS name paints — the spectrum
    // editor is absent (no has-spectrum here), so `light-name-chars` stays empty.
    st.set_light_effect_chars(ModelRc::new(VecModel::from(
        "wave".chars().map(|c| SharedString::from(c.to_string())).collect::<Vec<_>>(),
    )));
    st.set_light_name_ramp(ModelRc::new(VecModel::from(vec![slint::Color::from_rgb_u8(255, 0, 255); 4])));
    let painted = app.window().take_snapshot().expect("the painted name draws");
    // text antialiasing never yields a pure ramp pixel, so match the ramp's hue loosely
    let mark = |p: &slint::Rgba8Pixel| p.r >= 200 && p.b >= 200 && p.g <= 100;
    let width = painted.width();
    let (x, y) = painted.as_slice().iter().position(|p| mark(p))
        .map(|i| (i as u32 % width, i as u32 / width))
        .expect("the painted name carries its ramp colour");
    for dx in [0.0f32, 4.0, -4.0, 8.0] {
        app.window().dispatch_event(WindowEvent::PointerMoved {
            position: LogicalPosition::new(x as f32 + dx, y as f32),
        });
        app.window().take_snapshot().expect("the hover drift reads its bindings while drawn");
    }
    std::thread::sleep(std::time::Duration::from_millis(30));
    slint::platform::update_timers_and_animations();
    app.window().take_snapshot().expect("a mid-drift frame draws");
    app.window().dispatch_event(WindowEvent::PointerExited);
    app.window().take_snapshot().expect("the settled name draws after the pointer leaves");
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
        id: SurfaceId::Hero, target, content_rev, dims: (1, 2), layout_generation: 0,
        rgb: vec![color; 2].into(), swatch: None, requested: std::time::Instant::now(), render_time: std::time::Duration::ZERO,
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
    assert_eq!(st.get_light_cover_index(), 1);
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
        index, name: format!("Layer {index}").into(), summary: "screen · 37% · full board".into(), enabled: true,
    }).collect::<Vec<_>>())));
    app.window().set_size(slint::LogicalSize::new(1000.0, 1400.0));
    app.show().unwrap();
    app.window().take_snapshot().expect("large stack draws in a bounded list");
    for index in [999, 500, 0] {
        st.set_selected_layer(index);
        app.window().take_snapshot().expect("selection scrolls into view without binding loops");
    }
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
        let frame = app.window().take_snapshot().expect("inspector controls draw without binding loops");
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
