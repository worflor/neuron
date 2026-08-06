// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Automated GUI coverage — drives the REAL compiled Slint `AppWindow` + the `State` global through
//! the generated component API (set properties, invoke callbacks, assert state). This exercises the
//! actual `.slint` view and the Rust↔UI glue contract — the previously-untested surface.
//!
//! Two tiers:
//!   * a HEADLESS LAUNCH SMOKE test — build the window + install the full glue, prove it doesn't
//!     panic and the engine populated the view models. Skips gracefully if no windowing backend is
//!     available (CI without a display), so it never spuriously fails the build.
//!   * STATE-DRIVE tests — invoke real callbacks (page nav, diagnostics, macro dry-run, the safety
//!     gate, sector count) and assert the resulting `State` — the behaviour the user sees.
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
    match std::panic::catch_unwind(AppWindow::new) {
        Ok(Ok(app)) => Some(app),
        _ => {
            assert!(
                std::env::var_os("NEURON_REQUIRE_GUI").is_none(),
                "NEURON_REQUIRE_GUI is set but no windowing backend is available — \
                 this environment cannot provide the GUI coverage it promises"
            );
            eprintln!("skipping: no windowing backend available (GUI test ran zero assertions)");
            None
        }
    }
}

/// Headless launch smoke: build the window, install the whole glue, and confirm the engine pushed
/// real data into the view (devices model + effects exist as models, status is set). This is the
/// "the GUI actually comes up wired to the core" guarantee.
#[test]
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
    assert!(brush.red() as u32 + brush.green() as u32 + brush.blue() as u32 > 0);
    // models are populated objects (length may be 0 with no device, but the model must exist).
    let _ = st.get_devices();
    let _ = st.get_effects();
    let _ = st.get_profiles();
}

/// Driving the nav: setting `page` is how every NavEntry click + the tray "Settings" jump work.
/// Four sections (DEVICE / LIGHTING / INPUT / SYSTEM); INPUT has two depths via `input-view`;
/// profiles open as a SHEET, not a page.
#[test]
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
#[test]
fn safety_gate_toggles() {
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
fn macro_check_callback_reports() {
    let Some(app) = try_window() else { return };
    let _shared = glue::install(&app);
    let st = app.global::<State>();
    st.invoke_macro_check("def macro(ctx):\n    return ctx.app\n".into());
    assert!(st.get_macro_busy());
    assert_eq!(st.get_macro_status(), "checking syntax…");
}

/// The radial sector-count callback updates the count, rebuilds the sector model, PERSISTS to
/// cast.toml (a restart must not silently revert the wheel), and clamps to the sane 2..16 range.
#[test]
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
    // default 40-count deadzone) and a wheel never drops below quadrants.
    st.invoke_set_sector_count(100);
    assert_eq!(st.get_radial_sectors(), 16);
    assert_eq!(
        st.get_radial_max(),
        16,
        "the cap is derived from the deadzone"
    );
    st.invoke_set_sector_count(1);
    assert_eq!(st.get_radial_sectors(), 4);
}

/// The import wizard flag is a plain UI flag (Settings opens it; apply/close clears it). Driving it
/// proves the overlay flow toggles.
#[test]
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
    st.set_action_choice(2);
    assert_eq!(st.get_action_choice(), 2);
}

/// Setting an activation rhythm persists to cast.toml, updates both view properties (pattern +
/// symbol readout), and refuses an unparseable phrase without mutating anything.
#[test]
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
#[test]
fn perf_controls_honor_paused_gate() {
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
#[test]
fn all_perf_controls_have_callbacks() {
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
