//! The glue — binds every `State` callback to a `Runtime` operation and pushes engine data back
//! into the view. One place wires the whole GUI<->engine contract; the panels stay declarative.
//!
//! Everything here runs on the UI thread (device I/O is fast getter/setter round-trips). The
//! exceptions — lighting animation, blocking gesture capture, macro compiles, and diagnostics —
//! run on worker threads that post results back via `slint::invoke_from_event_loop`.
//!
//! Two cross-cutting disciplines this layer owns:
//!   * STATE TRUTH — every mutation refreshes what it invalidated (rows after an apply, the rules
//!     list after a wedge commit, the live engine after any config write via
//!     `dispatch::request_reload`), and every readout is SEEDED from persisted/device truth at
//!     install (no compile-time default masquerading as a reading).
//!   * HONEST FEEDBACK — failures refuse loudly before writing; successes name what they did; an
//!     op that didn't happen (no endpoint, no device) never reports as if it did.

use crate::mic;
use crate::migrate;
use crate::runtime::AppRuntime;
use crate::ui::{
    AppRuleRow, AppWindow, BeaconMacro, DeviceRow, DiagRow, EffectRow, GlyphChip, GraphEdge,
    GraphNode, ImportLine, KnobRow, LayerRow, MaterialCard, OrganRow, PocketCard, ProfileRow,
    RadialSector, RhythmBindRow, RuleRow, State, Theme,
};
use neuron::effects::FrameGen;
use neuron::import::Imported;
use neuron::lighting::Rgb;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::cell::RefCell;
use std::rc::Rc;

/// The "off" colour of an LED cell — what `clear` paints and what an unpainted grid shows.
const GRID_OFF: slint::Color = slint::Color::from_rgb_u8(0x0c, 0x0d, 0x10);
const DPI_MIN: u16 = 100;
const DPI_MAX: u16 = 30_000;
const DPI_STAGE_CAPACITY: usize = 5;
const IDLE_MIN_SECS: u32 = 0;
const IDLE_MAX_SECS: u32 = u16::MAX as u32;

/// Shared GUI state: the runtime plus the parsed-but-unapplied import (held between Parse/Apply).
pub struct Shared {
    pub rt: AppRuntime,
    pub pending_import: Option<Imported>,
    /// The lighting COMPOSITOR stack — the Rust-side source of truth (regions live here). The Slint
    /// `light-layers` model is a display projection of this. `layers_rev` bumps on any change so the
    /// live preview rebuilds its cached `Compositor` (stateful effects like fire keep their state
    /// across ticks otherwise).
    pub light_layers: Vec<neuron::effects::LayerDef>,
    pub selected_layer: usize,
    pub layers_rev: u64,
}

pub type SharedRt = Rc<RefCell<Shared>>;

thread_local! {
    /// The UI-thread-local handle to the shared runtime. Lets a worker thread's result, posted
    /// back via `slint::invoke_from_event_loop` (whose closure must be `Send`), reach the !Send
    /// `Rc<RefCell<Shared>>` without capturing it — it's fetched here on the UI thread instead.
    static UI_SHARED: RefCell<Option<SharedRt>> = const { RefCell::new(None) };

    /// The last HID control captured by press-to-bind, held between the capture callback and the
    /// "Add binding" commit (UI-thread-local, like the shared runtime).
    static CAPTURED_CONTROL: RefCell<Option<crate::capture::CapturedControl>> = const { RefCell::new(None) };
}

/// Read the currently-selected action (palette id + parameter) from the State action picker.
fn current_action(st: &State) -> (String, String) {
    let idx = st.get_action_choice().max(0) as usize;
    let id = st
        .get_action_choices()
        .row_data(idx)
        .map(|c| c.id.to_string())
        .unwrap_or_else(|| "noop".into());
    (id, st.get_action_param().to_string())
}

/// The ARM stance (0 OBSERVE · 1 DEVICE · 2 INPUT · 3 LIVE) from the two live gates: writes-paused +
/// input-armed. The segmented selector and the two header pills both reflect this one truth.
fn arm_stance(paused: bool, armed: bool) -> i32 {
    match (paused, armed) {
        (true, false) => 0,
        (false, false) => 1,
        (true, true) => 2,
        (false, true) => 3,
    }
}

/// Derive the LAUNCH mode (0 Manual · 1 Boot-to-tray · 2 Boot-with-window) from the two real truths:
/// the registry autostart entry + the start-minimized pref. The selector reads this, never a stored int.
fn launch_mode_now() -> i32 {
    if !crate::autostart::is_enabled() {
        0
    } else if crate::prefs::start_minimized() {
        1
    } else {
        2
    }
}

/// Populate the Action palette model from the editor's single source of truth, interleaving a
/// HEADER row whenever the group changes — the picker renders structure, not a flat dump.
fn init_action_palette(app: &AppWindow) {
    let mut rows: Vec<crate::ui::ActionChoice> = Vec::new();
    let mut last_group = "";
    for (id, label, hint, group, required, tier) in crate::editor::ACTION_PALETTE {
        if *group != last_group && !group.is_empty() {
            rows.push(crate::ui::ActionChoice {
                id: "".into(),
                label: (*group).into(),
                param_hint: "".into(),
                header: true,
                required: false,
                tier: 0,
            });
            last_group = group;
        }
        rows.push(crate::ui::ActionChoice {
            id: (*id).into(),
            label: (*label).into(),
            param_hint: (*hint).into(),
            header: false,
            required: *required,
            tier: *tier as i32,
        });
    }
    let st = app.global::<State>();
    st.set_action_choices(ModelRc::new(VecModel::from(rows)));
    // the default selection must be a real choice, never a header row.
    if st
        .get_action_choices()
        .row_data(st.get_action_choice().max(0) as usize)
        .map(|c| c.header)
        .unwrap_or(true)
    {
        st.set_action_choice(1); // the first entry under the first header
    }
}

/// Preset the shared ActionPicker to reflect an existing action — the edit-shows-current contract.
/// An unbound target presets to "nothing (unbind)" with a CLEARED param, so the previous editor's
/// leftovers can never be committed by accident.
fn preset_picker(st: &State, action: &neuron::action::Action) {
    let (id, param) = crate::editor::action_to_palette(action);
    let idx = st
        .get_action_choices()
        .iter()
        .position(|c| !c.header && c.id == id)
        .unwrap_or(1);
    st.set_action_choice(idx as i32);
    st.set_action_param(param.into());
    refresh_param_suggestions(st);
}

/// SUGGESTION CHIPS for the param field — live, clickable fills, by what the chosen action
/// actually accepts: glance → the windows open right now (specific titles, then broad exe
/// stems); profile → the saved profiles; macro → the registered macros. No typing blind.
fn refresh_param_suggestions(st: &State) {
    let (id, _) = current_action(st);
    let v: Vec<slint::SharedString> = match id.as_str() {
        // the windows open right now (summon brings, glance peeks) — specific then broad
        "glance" | "summon" => crate::glance::suggestions()
            .into_iter()
            .map(Into::into)
            .collect(),
        "profile" => neuron::profile::list()
            .into_iter()
            .map(Into::into)
            .collect(),
        "macro" => neuron::macros::macro_host::list_macros()
            .into_iter()
            .map(Into::into)
            .collect(),
        // the enum options ARE the chips — the picker doubles as an enum picker
        "banish" => ["focused", "hover", "behind"]
            .iter()
            .map(|s| (*s).into())
            .collect(),
        "pin" => ["focused", "hover"].iter().map(|s| (*s).into()).collect(),
        "kill" => ["focused", "hover"].iter().map(|s| (*s).into()).collect(),
        // tether takes just an optional stone name (one intuitive mode) — no fixed chips
        "ghost-paste" => ["instant", "borderline", "fast", "normal"]
            .iter()
            .map(|s| (*s).into())
            .collect(),
        "momentary-mic" => ["flip", "talk", "mute"]
            .iter()
            .map(|s| (*s).into())
            .collect(),
        "dial" => ["volume", "mic"].iter().map(|s| (*s).into()).collect(),
        // the connected output devices — click to build the cycle set
        "output-flip" => neuron::audio::endpoints(neuron::audio::Flow::Render)
            .into_iter()
            .map(|e| e.name.into())
            .collect(),
        _ => Vec::new(),
    };
    st.set_param_suggestions(ModelRc::new(VecModel::from(v)));
}

/// Run `f` with the UI-thread-local shared runtime (no-op if not installed).
fn with_shared(f: impl FnOnce(&SharedRt)) {
    UI_SHARED.with(|s| {
        if let Some(sh) = s.borrow().as_ref() {
            f(sh);
        }
    });
}

/// Parse the current light-color hex into an `Rgb`. Falls back to the LAST-GOOD brush colour
/// (which on_color_changed maintains) — NEVER a hardcoded accent the user didn't choose.
fn brush(app: &AppWindow) -> Rgb {
    let st = app.global::<State>();
    match Rgb::parse(&st.get_light_color()) {
        Some(c) => c,
        None => {
            let b = st.get_brush_color();
            Rgb::new(b.red(), b.green(), b.blue())
        }
    }
}

fn rgb_to_color(c: Rgb) -> slint::Color {
    slint::Color::from_rgb_u8(c.r, c.g, c.b)
}

/// Parse a user accent hex (bare or '#'-prefixed RRGGBB) to a Slint colour, falling back to the
/// stock phosphor so a junk value never leaves the UI un-tinted. Shared by the UI + weave accents.
fn accent_color(hex: &str) -> slint::Color {
    let clean = crate::prefs::normalize_hex(hex);
    rgb_to_color(Rgb::parse(&clean).unwrap_or(Rgb::new(0x4A, 0xF2, 0xB0)))
}

/// Drive the live Directed-Intent material from the saved weave accent. Colour isn't pigment in that
/// material — the accent is the hue its spectral fire centres on (+ the rim tint) — so this sets the
/// material's accent, NOT a flat swatch. The stock phosphor (`4af2b0`) means "use the themed
/// material" (clear the override, so an `ichor.toml` accent still wins); any other colour overrides.
fn apply_weave_accent(hex: &str) {
    let clean = crate::prefs::normalize_hex(hex);
    if clean == "4af2b0" {
        crate::weave::clear_weave_accent();
    } else if let Ok(rgb) = u32::from_str_radix(&clean, 16) {
        crate::weave::set_weave_accent(rgb);
    }
}

/// The saved weave accent as a packed 0xRRGGBB (the gallery tints its previews with it).
fn weave_accent_u32() -> u32 {
    u32::from_str_radix(&crate::prefs::weave_accent(), 16).unwrap_or(0x4A_F2B0)
}

/// Render the spellweaving MATERIAL gallery — one live swatch per [`crate::weave::Surface`], drawn
/// by the real shader at time `t` (so the tiles animate: fire flickers, water flows). Rebuilt each
/// preview tick (6 small tiles — cheap); the swatch IS the material, an honest side-by-side.
fn render_material_cards(app: &AppWindow, t: f32) {
    use crate::weave::Surface;
    let accent = weave_accent_u32();
    let acc = (
        ((accent >> 16) & 0xFF) as f32 / 255.0,
        ((accent >> 8) & 0xFF) as f32 / 255.0,
        (accent & 0xFF) as f32 / 255.0,
    );
    let live = crate::weave::live_material();
    // HQ: render ABOVE the display size (tiles ~168px) so the swatch downsamples crisp, not blurry.
    let (w, h) = (220usize, 132usize); // 5:3 aspect
    let cards: Vec<MaterialCard> = Surface::ALL
        .into_iter()
        .map(|s| {
            // the SELECTED tile previews the LIVE material (so knob edits show); others, the preset.
            let m = if s == live.surface {
                live
            } else {
                crate::weave::preset(s).with_accent(acc)
            };
            let rgba = crate::weave::material_preview_rgba(&m, w, h, t);
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
            buf.make_mut_bytes().copy_from_slice(&rgba);
            MaterialCard {
                name: s.name().into(),
                slug: s.slug().into(),
                blurb: s.blurb().into(),
                swatch: Image::from_rgba8(buf),
            }
        })
        .collect();
    app.global::<State>()
        .set_material_cards(ModelRc::new(VecModel::from(cards)));
}

/// Push the live material's surfaced KNOBS into the UI (a slider per knob). Re-read after a surface
/// switch (the knob set changes) and after each edit (to reflect the clamped value).
fn refresh_weave_knobs(app: &AppWindow) {
    let rows: Vec<KnobRow> = crate::weave::weave_knobs()
        .into_iter()
        .map(|(label, value, min, max)| KnobRow {
            label: label.into(),
            value,
            min,
            max,
        })
        .collect();
    app.global::<State>()
        .set_weave_knobs(ModelRc::new(VecModel::from(rows)));
}

/// First line of a multi-line error (a Python traceback's last line carries the message, but the
/// first is the most compact for a one-line status). Trimmed.
fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).trim().to_string()
}

/// Install every callback + initial data. Returns the shared runtime so main/tray can reach it.
pub fn install(app: &AppWindow) -> SharedRt {
    let shared: SharedRt = Rc::new(RefCell::new(Shared {
        rt: AppRuntime::load(),
        pending_import: None,
        // NO seed — an empty stack. Seeding a default layer was a lie: the render is a preview of
        // NEURON's composite, not a read-back of the device, so a seeded mint wash falsely claimed
        // the keyboard was mint when another app (Synapse) actually owned its lighting. Empty = the
        // honest "Neuron isn't driving this device yet" until the user builds a composite.
        light_layers: Vec::new(),
        selected_layer: 0,
        layers_rev: 0,
    }));
    UI_SHARED.with(|s| *s.borrow_mut() = Some(shared.clone()));
    let st = app.global::<State>();

    // the ABOUT nameplate — version (compile-time) + where the config/run dir lives
    st.set_app_version(env!("CARGO_PKG_VERSION").into());
    if let Ok(dir) = std::env::current_dir() {
        st.set_run_dir(dir.display().to_string().into());
    }

    // initial population — every readout seeded from persisted/device truth.
    refresh_devices(app, &shared);
    refresh_rules(app, &shared);
    refresh_pockets(app);
    refresh_beacon_macros(app);
    refresh_profiles(app, &shared);
    refresh_app_rules(app, &shared);
    refresh_gestures(app, &shared);
    refresh_rhythms(app, &shared);
    refresh_radial(app, &shared);
    refresh_effects(app, &shared);
    refresh_layers(app, &shared);
    init_grid(app, &shared);
    init_action_palette(app);
    init_perf_controls(app, &shared);
    mic::refresh(app);
    mic::refresh_output(app);
    st.set_brush_color(rgb_to_color(brush(app)));
    // the cast hold-trigger label comes from cast.toml — never a hardcoded button name.
    st.set_cast_trigger_label(neuron::capture::vk_name(shared.borrow().rt.cast.trigger).into());
    // the activation rhythm + HyperShift stance come from persisted config too.
    sync_activation_view(&st, &shared.borrow().rt.cast.activation);
    st.set_hypershift_mode(
        neuron::feel::FeelConfig::load()
            .hypershift
            .describe()
            .into(),
    );

    // ── Device ───────────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_refresh_devices(move || {
            if let Some(app) = w.upgrade() {
                // status first, so a "selected device disconnected — switched" notice from the
                // reconcile inside refresh_devices isn't clobbered.
                app.global::<State>()
                    .set_status_line("devices re-scanned".into());
                refresh_devices(&app, &sh);
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_select_device(move |idx| {
            if let Some(app) = w.upgrade() {
                // ONE selection path: set the kind (which gates the panel) + seed the per-kind
                // controls. HID points the runtime at the pid; mic/output loads its volume + mute.
                select_device_at(&app, &sh, idx);
            }
        });
    });
    // audio endpoint volume/mute for the SELECTED mic/output — acts on that endpoint by id, not the
    // system default (so picking "Razer Seiren" and dragging only moves the Seiren).
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_device_volume(move |v| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if let Some(id) = selected_audio_id(&st) {
                    if let Some(ctl) = neuron::audio::VolumeCtl::open(&id) {
                        ctl.set_volume((v / 100.0).clamp(0.0, 1.0));
                        st.set_device_volume(v);
                        patch_selected_audio_detail(&st);
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_toggle_device_mute(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if let Some(id) = selected_audio_id(&st) {
                    if let Some(ctl) = neuron::audio::VolumeCtl::open(&id) {
                        let muted = ctl.toggle_mute();
                        st.set_device_muted(muted);
                        patch_selected_audio_detail(&st);
                        st.set_status_line(
                            if muted {
                                "endpoint muted".to_string()
                            } else {
                                "endpoint unmuted".to_string()
                            }
                            .into(),
                        );
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_backup_device(move |pid| {
            if let Some(app) = w.upgrade() {
                let p = u16::from_str_radix(pid.as_str(), 16).unwrap_or(0);
                let msg = sh.borrow().rt.backup(p);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    // the ONE write path into the runtime's persist flag (was smuggled through open-config-dir).
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_persist(move |v| {
            if w.upgrade().is_some() {
                sh.borrow_mut().rt.persist = v;
            }
        });
    });

    // ── Performance ──────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_apply_dpi(move |v| status(&w, &sh, |rt| rt.apply_dpi(v as u16)));
    });
    // polling gets its own handler: the device returns the SNAPPED rate, and the control settles
    // onto that real detent instead of keeping the requested value.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_polling(move |v| {
            if let Some(app) = w.upgrade() {
                let (msg, actual) = {
                    let mut s = sh.borrow_mut();
                    s.rt.persist = app.global::<State>().get_persist_to_onboard();
                    s.rt.apply_polling(v as u32)
                };
                let st = app.global::<State>();
                if let Some(hz) = actual {
                    st.set_polling_hz(hz as f32);
                }
                st.set_status_line(msg.into());
                refresh_devices(&app, &sh);
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_apply_brightness(move |v| status(&w, &sh, |rt| rt.apply_brightness(v as u8)));
    });

    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_open_config_dir(move || {
            if let Some(app) = w.upgrade() {
                open_config_dir();
                app.global::<State>()
                    .set_status_line("opened config folder".into());
            }
        });
    });

    // PURGE SYNAPSE — stop Razer's respawning services then terminate every Synapse process. Razer
    // services run as SYSTEM, so an unelevated app relaunches itself via UAC; either way we report
    // what happened to the dedicated status line under the button.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_purge_synapse(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                #[cfg(windows)]
                {
                    use crate::purge::Outcome;
                    let msg = match crate::purge::request() {
                        Outcome::Done { killed, stopped, disabled }
                            if killed == 0 && stopped == 0 && disabled == 0 =>
                        {
                            "no Synapse left — already clean".to_string()
                        }
                        Outcome::Done { killed, stopped, disabled } => {
                            format!("purged Synapse — disabled {disabled} + stopped {stopped} service(s), killed {killed} process(es)")
                        }
                        Outcome::Elevating => {
                            "requesting admin to purge SYSTEM services… (approve the UAC prompt)".to_string()
                        }
                        Outcome::Failed(why) => why.to_string(),
                    };
                    st.set_purge_status(msg.clone().into());
                    st.set_status_line(msg.into());
                }
                #[cfg(not(windows))]
                {
                    st.set_purge_status("Synapse purge is Windows-only".into());
                }
            }
        });
    });

    // ── Lighting ─────────────────────────────────────────────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_color_changed(move |hex| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match Rgb::parse(hex.as_str()) {
                    Some(c) => {
                        st.set_light_color_valid(true);
                        st.set_brush_color(rgb_to_color(c));
                        // live colour: if THIS effect is on the device and takes a colour,
                        // re-apply so the edit is the readout (no silent stale device colour).
                        let sel = st.get_selected_effect();
                        let applied = st.get_applied_effect();
                        if sel >= 0
                            && sel == applied
                            && !st.get_animating()
                            && !st.get_writes_paused()
                        {
                            let uses = st
                                .get_effects()
                                .row_data(sel as usize)
                                .map(|e| e.uses_color)
                                .unwrap_or(false);
                            if uses {
                                st.invoke_apply_effect(sel);
                            }
                        }
                    }
                    None => st.set_light_color_valid(false),
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_effect(move |idx| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // the global write kill-switch gates effect application too (it was a partial lie —
                // animate/push were gated but selecting a chip still wrote to the device).
                if st.get_writes_paused() {
                    st.set_status_line("writes paused — effect not applied".into());
                    return;
                }
                if !st.get_light_color_valid() {
                    st.set_status_line(
                        "colour must be hex like 4af2b0 — using last valid swatch".into(),
                    );
                }
                let col = brush(&app);
                let (msg, ok) = {
                    let mut s = sh.borrow_mut();
                    // kill any running generator first — a 30fps repaint would erase this write
                    // within a frame and the click would look dead.
                    s.rt.stop_animation();
                    s.rt.persist = st.get_persist_to_onboard();
                    let name = s.rt.effects().get(idx as usize).map(|e| e.0.clone());
                    match name {
                        Some(n) => {
                            let m = s.rt.apply_effect(&n, col);
                            let ok = m.starts_with("effect ->");
                            (m, ok)
                        }
                        None => ("no effect".into(), false),
                    }
                };
                st.set_animating(false);
                // the accent marks what the device is ACTUALLY running — only on success.
                if ok {
                    st.set_applied_effect(idx);
                }
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_animate_effect(move |idx| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let col = brush(&app);
                let back = app.as_weak();
                let msg = {
                    let mut s = sh.borrow_mut();
                    let pid = s.rt.selected_pid;
                    let name = s.rt.effects().get(idx as usize).map(|e| e.0.clone());
                    match name {
                        Some(n) => s.rt.start_animation(&n, col, pid, move |reason, token| {
                            // the worker exited (clean stop, device gone, error) — post the truth
                            // back so the transport never shows a live state for a dead thread.
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = back.upgrade() {
                                    with_shared(|sh| {
                                        // a NEWER animation owns the UI state; this exit is stale.
                                        if !std::sync::Arc::ptr_eq(
                                            &token,
                                            &sh.borrow().rt.anim_stop,
                                        ) {
                                            return;
                                        }
                                        if !sh.borrow().rt.animating {
                                            return; // user already stopped it; status said so
                                        }
                                        sh.borrow_mut().rt.animating = false;
                                        let st = app.global::<State>();
                                        st.set_animating(false);
                                        st.set_applied_effect(-1);
                                        st.set_status_line(
                                            match reason {
                                                Some(r) => format!("animation ended: {r}"),
                                                None => "animation finished".into(),
                                            }
                                            .into(),
                                        );
                                    });
                                }
                            });
                        }),
                        None => "no effect".into(),
                    }
                };
                let animating = sh.borrow().rt.animating;
                st.set_animating(animating);
                if animating {
                    st.set_applied_effect(idx);
                }
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_stop_animation(move || {
            if let Some(app) = w.upgrade() {
                sh.borrow_mut().rt.stop_animation();
                let st = app.global::<State>();
                st.set_animating(false);
                st.set_applied_effect(-1);
                st.set_status_line("animation stopped".into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        // THE LIVE COMPOSITE MIRROR: composite the whole layer stack onto the render at ~20Hz with
        // the SAME math the device path runs (`Compositor::frame`). A reconstruction, not a
        // read-back — badged "~ live". The Compositor is cached and only rebuilt when the stack
        // changes (`layers_rev`), so stateful effects (fire's heat map) keep state across ticks.
        let cache: Rc<RefCell<Option<(u64, neuron::effects::Compositor, std::time::Instant)>>> =
            Rc::new(RefCell::new(None));
        app.global::<State>().on_preview_tick(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let (rows, cols) = (st.get_grid_rows(), st.get_grid_cols());
                // PAINT mode owns the grid (the per-LED editor) — don't fight it.
                if st.get_light_paint_mode() || rows <= 0 || cols <= 0 {
                    cache.replace(None);
                    return;
                }
                let (rev, defs) = {
                    let s = sh.borrow();
                    (s.layers_rev, s.light_layers.clone())
                };
                let n = (rows * cols) as usize;
                if defs.is_empty() {
                    // an empty stack = a dark device; mirror that honestly
                    st.set_grid_px(ModelRc::new(VecModel::from(vec![GRID_OFF; n])));
                    cache.replace(None);
                    return;
                }
                let mut c = cache.borrow_mut();
                if c.as_ref().map(|(r, _, _)| *r != rev).unwrap_or(true) {
                    *c = Some((
                        rev,
                        neuron::effects::Compositor::from_defs(&defs),
                        std::time::Instant::now(),
                    ));
                }
                let (_, comp, t0) = c.as_mut().unwrap();
                let frame = comp.frame(
                    rows as u8,
                    cols as u8,
                    t0.elapsed().as_secs_f32(),
                    Rgb::BLACK,
                );
                let px: Vec<slint::Color> = frame
                    .iter()
                    .map(|p| slint::Color::from_rgb_u8(p.r, p.g, p.b))
                    .collect();
                st.set_grid_px(ModelRc::new(VecModel::from(px)));
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_paint_cell(move |idx| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let model = st.get_grid_px();
                if let Some(vm) = model.as_any().downcast_ref::<VecModel<slint::Color>>() {
                    if (idx as usize) < vm.row_count() {
                        vm.set_row_data(idx as usize, st.get_brush_color());
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_erase_cell(move |idx| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let model = st.get_grid_px();
                if let Some(vm) = model.as_any().downcast_ref::<VecModel<slint::Color>>() {
                    if (idx as usize) < vm.row_count() {
                        vm.set_row_data(idx as usize, GRID_OFF);
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_clear_grid(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let n = st.get_grid_px().row_count();
                st.set_grid_px(ModelRc::new(VecModel::from(vec![GRID_OFF; n])));
                st.set_status_line("frame cleared".into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_fill_grid(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let n = st.get_grid_px().row_count();
                let c = st.get_brush_color();
                st.set_grid_px(ModelRc::new(VecModel::from(vec![c; n])));
                st.set_status_line("frame filled with the brush colour".into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_push_frame(move || {
            if let Some(app) = w.upgrade() {
                // stop the generator BEFORE the write so it can't repaint over the user's frame.
                sh.borrow_mut().rt.stop_animation();
                let st = app.global::<State>();
                st.set_animating(false);
                let model = st.get_grid_px();
                let mut frame = Vec::with_capacity(model.row_count());
                for c in model.iter() {
                    frame.push(Rgb::new(c.red(), c.green(), c.blue()));
                }
                let msg = sh.borrow().rt.push_frame(&frame);
                if msg == "frame pushed" {
                    st.set_applied_effect(-1); // the device now shows the custom frame, not an effect
                }
                st.set_status_line(msg.into());
            }
        });
    });

    // ── Lighting COMPOSITOR — the layer stack (Rust holds the truth incl. regions) ──
    bind(app, &shared, |app, sh| {
        // add-layer(effect): append a fresh layer of that effect, inheriting the brush colour
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_add_layer(move |effect| {
                if let Some(app) = w.upgrade() {
                    {
                        let b = app.global::<State>().get_brush_color();
                        let mut s = sh.borrow_mut();
                        let mut d = neuron::effects::LayerDef::default();
                        d.effect = effect.to_string();
                        d.color = Rgb::new(b.red(), b.green(), b.blue());
                        s.light_layers.push(d);
                        s.selected_layer = s.light_layers.len() - 1;
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_remove_layer(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let i = idx as usize;
                        if i < s.light_layers.len() {
                            s.light_layers.remove(i);
                        }
                        if !s.light_layers.is_empty() && s.selected_layer >= s.light_layers.len() {
                            s.selected_layer = s.light_layers.len() - 1;
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_select_layer(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if (idx as usize) < s.light_layers.len() {
                            s.selected_layer = idx as usize;
                        }
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_move_layer(move |idx, dir| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let i = idx as usize;
                        let j = i as i32 + dir;
                        if i < s.light_layers.len() && j >= 0 && (j as usize) < s.light_layers.len()
                        {
                            s.light_layers.swap(i, j as usize);
                            s.selected_layer = j as usize;
                            s.layers_rev += 1;
                        }
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_toggle_layer(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if let Some(d) = s.light_layers.get_mut(idx as usize) {
                            d.enabled = !d.enabled;
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_layer_effect(move |idx, eff| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if let Some(d) = s.light_layers.get_mut(idx as usize) {
                            d.effect = eff.to_string();
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_layer_color(move |idx, hex| {
                if let Some(app) = w.upgrade() {
                    if let Some(c) = Rgb::parse(hex.as_str()) {
                        {
                            let mut s = sh.borrow_mut();
                            if let Some(d) = s.light_layers.get_mut(idx as usize) {
                                d.color = c;
                            }
                            s.layers_rev += 1;
                        }
                        refresh_layers(&app, &sh);
                    }
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_layer_speed(move |idx, v| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if let Some(d) = s.light_layers.get_mut(idx as usize) {
                            d.speed = v.clamp(0.1, 6.0);
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>()
                .on_set_layer_direction(move |idx, dir| {
                    if let Some(app) = w.upgrade() {
                        {
                            let mut s = sh.borrow_mut();
                            if let Some(d) = s.light_layers.get_mut(idx as usize) {
                                d.direction = (dir.rem_euclid(4)) as u8;
                            }
                            s.layers_rev += 1;
                        }
                        refresh_layers(&app, &sh);
                    }
                });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_layer_blend(move |idx, b| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if let Some(d) = s.light_layers.get_mut(idx as usize) {
                            d.blend = neuron::effects::Blend::from_str(b.as_str());
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_clear_layer_region(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        if let Some(d) = s.light_layers.get_mut(idx as usize) {
                            d.region.clear();
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // paint/erase a cell into the SELECTED layer's region (driven by the render's paint area)
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_paint_region(move |cell| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            let c = cell as u32;
                            if !d.region.contains(&c) {
                                d.region.push(c);
                            }
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_erase_region(move |cell| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            d.region.retain(|&c| c != cell as u32);
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // stream the whole composite live to the device
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_composite_apply(move || {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    if st.get_writes_paused() {
                        st.set_status_line("writes paused — composite not applied".into());
                        return;
                    }
                    let back = app.as_weak();
                    let msg = {
                        let mut s = sh.borrow_mut();
                        let pid = s.rt.selected_pid;
                        let defs = s.light_layers.clone();
                        s.rt.start_layers(defs, pid, move |reason, token| {
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(app) = back.upgrade() {
                                    with_shared(|sh| {
                                        if !std::sync::Arc::ptr_eq(
                                            &token,
                                            &sh.borrow().rt.anim_stop,
                                        ) {
                                            return;
                                        }
                                        if !sh.borrow().rt.animating {
                                            return;
                                        }
                                        sh.borrow_mut().rt.animating = false;
                                        let st = app.global::<State>();
                                        st.set_compositing(false);
                                        st.set_status_line(
                                            match reason {
                                                Some(r) => format!("composite ended: {r}"),
                                                None => "composite finished".into(),
                                            }
                                            .into(),
                                        );
                                    });
                                }
                            });
                        })
                    };
                    let animating = sh.borrow().rt.animating;
                    st.set_compositing(animating);
                    st.set_status_line(msg.into());
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_composite_stop(move || {
                if let Some(app) = w.upgrade() {
                    sh.borrow_mut().rt.stop_animation();
                    let st = app.global::<State>();
                    st.set_compositing(false);
                    st.set_status_line("composite stopped".into());
                }
            });
        }
        // import a Synapse .ChromaEffects / .synapse3 export straight into the studio: a lossless
        // per-LED static frame lands on the PAINT canvas (push to apply); a basic named effect lands
        // as a compositor layer. We HAVE Synapse's format — so loading the user's own theme is one click.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_import_light_file(move || {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_status_line("opening file picker…".into());
                    let sh = sh.clone();
                    crate::filedlg::pick_synapse_export(&app, move |app, path| {
                        let st = app.global::<State>();
                        if path.is_empty() {
                            st.set_status_line("no file selected".into());
                            return;
                        }
                        match neuron::import::import_export(std::path::Path::new(&path)) {
                            Ok(imp) => {
                                if !imp.lighting_layers.is_empty() {
                                    // an ADVANCED animated composite → straight into the layer stack
                                    let layers = imp.lighting_layers.clone();
                                    let n = layers.len();
                                    {
                                        let mut s = sh.borrow_mut();
                                        s.light_layers = layers;
                                        s.selected_layer = s.light_layers.len().saturating_sub(1);
                                        s.layers_rev += 1;
                                    }
                                    st.set_light_paint_mode(false);
                                    refresh_layers(&app, &sh);
                                    let drop = imp
                                        .notes
                                        .iter()
                                        .find(|nt| nt.contains("no host generator"))
                                        .cloned()
                                        .unwrap_or_default();
                                    st.set_status_line(
                                        format!(
                                            "imported {n} layer(s) from your theme{}",
                                            if drop.is_empty() { String::new() } else { format!(" · {drop}") }
                                        )
                                        .into(),
                                    );
                                } else if let Some(frame) = &imp.profile.lighting_frame {
                                    // a lossless per-LED frame → the PAINT canvas (push to apply)
                                    let (rows, cols) = (st.get_grid_rows(), st.get_grid_cols());
                                    let n = (rows.max(0) * cols.max(0)) as usize;
                                    let mut px: Vec<slint::Color> = frame
                                        .iter()
                                        .take(n)
                                        .map(|c| slint::Color::from_rgb_u8(c[0], c[1], c[2]))
                                        .collect();
                                    while px.len() < n {
                                        px.push(GRID_OFF);
                                    }
                                    st.set_grid_px(ModelRc::new(VecModel::from(px)));
                                    st.set_light_paint_mode(true);
                                    st.set_status_line(
                                        format!(
                                            "imported {} per-LED cell(s) onto the canvas — push frame to apply",
                                            frame.len()
                                        )
                                        .into(),
                                    );
                                } else if let Some(name) = &imp.profile.lighting {
                                    // a basic named effect → a compositor layer
                                    let col = imp
                                        .profile
                                        .color
                                        .as_deref()
                                        .and_then(Rgb::parse)
                                        .unwrap_or(Rgb::new(0x4A, 0xF2, 0xB0));
                                    let eff = if neuron::effects::make(name).is_some() {
                                        name.clone()
                                    } else {
                                        "static".to_string()
                                    };
                                    {
                                        let mut s = sh.borrow_mut();
                                        let mut d = neuron::effects::LayerDef::default();
                                        d.effect = eff.clone();
                                        d.color = col;
                                        s.light_layers.push(d);
                                        s.selected_layer = s.light_layers.len() - 1;
                                        s.layers_rev += 1;
                                    }
                                    st.set_light_paint_mode(false);
                                    refresh_layers(&app, &sh);
                                    st.set_status_line(format!("imported effect '{eff}' as a layer").into());
                                } else {
                                    let note = imp.notes.first().cloned().unwrap_or_default();
                                    st.set_status_line(
                                        format!("no lighting in that export{}", if note.is_empty() { String::new() } else { format!(" — {note}") }).into(),
                                    );
                                }
                            }
                            Err(e) => st.set_status_line(format!("import failed: {e}").into()),
                        }
                    });
                }
            });
        }
    });

    // ── Bindings ─────────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_reload_bindings(move || {
            if let Some(app) = w.upgrade() {
                {
                    let mut s = sh.borrow_mut();
                    s.rt.bindings = neuron::bindings::Bindings::load();
                    s.rt.cast = neuron::cast::CastConfig::load();
                }
                refresh_rules(&app, &sh);
                // cast.toml feeds the wheel + trigger label too — one reload, one world.
                refresh_radial(&app, &sh);
                let st = app.global::<State>();
                st.set_cast_trigger_label(
                    neuron::capture::vk_name(sh.borrow().rt.cast.trigger).into(),
                );
                sync_activation_view(&st, &sh.borrow().rt.cast.activation);
                st.set_editing_sector(-1); // drop any in-flight wedge edit targeting the old config
                                           // the live engine reloads from the same disk truth.
                crate::dispatch::request_reload();
                let n = neuron::engine::Engine::new(sh.borrow().rt.spine_rules())
                    .rules
                    .len();
                st.set_status_line(format!("bindings reloaded ({n} spine rules)").into());
            }
        });
    });
    // REMOVE a GUI-authored BASE rule. The editable rows are the LAST `editable_count` rows of the
    // base list; the row index maps to the n-th GUI base-tier rule.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_remove_rule(move |row| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let total = st.get_rules().row_count() as i32;
                let editable = st.get_editable_count();
                let first_editable = total - editable;
                if row < first_editable {
                    st.set_status_line(
                        "that rule comes from bindings.toml / cast.toml — edit there to change it"
                            .into(),
                    );
                    return;
                }
                let gui_idx = (row - first_editable) as usize;
                match crate::editor::remove_gui_rule_in_tier(gui_idx, false) {
                    Ok(()) => {
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_status_line("binding removed".into());
                    }
                    Err(e) => st.set_status_line(format!("remove failed: {e}").into()),
                }
            }
        });
    });
    // REMOVE a GUI-authored HYPERSHIFT rule. AppRuntime::rules() contributes no hyper rows (its hyper
    // list is empty by construction), so every row in `hypershift-rules` is GUI-authored: the row
    // index IS the n-th hyper-tier rule in gui.rules.toml.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_remove_hyper_rule(move |row| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match crate::editor::remove_gui_rule_in_tier(row.max(0) as usize, true) {
                    Ok(()) => {
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_status_line("binding removed".into());
                    }
                    Err(e) => st.set_status_line(format!("remove failed: {e}").into()),
                }
            }
        });
    });
    // PRESS-TO-BIND a trigger: capture the HID control the user presses (no hardcoded button).
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_capture_trigger(move || {
            if let Some(app) = w.upgrade() {
                crate::capture::begin_control(&app, |app, captured| {
                    let st = app.global::<State>();
                    match captured {
                        Some(c) => {
                            // stash the captured (page,usage,pid) for add-binding, show its name.
                            CAPTURED_CONTROL.with(|cell| *cell.borrow_mut() = Some(c));
                            let name = neuron::capture::usage_name(c.page, c.usage);
                            let label = if name == "?" {
                                format!("0x{:02X}/0x{:02X}", c.page, c.usage)
                            } else {
                                format!("{name} (0x{:02X}/0x{:02X})", c.page, c.usage)
                            };
                            st.set_bind_trigger_label(label.into());
                            st.set_bind_trigger_ready(true);
                            st.set_status_line(
                                "control captured — pick an action, then Add".into(),
                            );
                        }
                        None => {
                            st.set_status_line("capture cancelled".into());
                        }
                    }
                });
            }
        });
    });
    // ADD a binding: validate, commit the captured trigger + chosen action, reload the live engine.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_add_binding(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let Some(c) = CAPTURED_CONTROL.with(|cell| *cell.borrow()) else {
                    st.set_status_line("press-to-bind a trigger first".into());
                    return;
                };
                let (id, param) = current_action(&st);
                // the strict front door: no silently-broken rules ("key " with no key, dpi "abc").
                if let Err(e) = crate::editor::validate_action(&id, &param) {
                    st.set_status_line(e.into());
                    return;
                }
                let action = crate::editor::build_action(&id, &param);
                let trigger = neuron::engine::Trigger::Input { page: c.page, usage: c.usage, pid: c.pid };
                let hyper = st.get_bind_hypershift();
                match crate::editor::add_gui_rule(trigger, action, hyper) {
                    Ok(outcome) => {
                        sh.borrow_mut().rt.bindings = neuron::bindings::Bindings::load();
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        CAPTURED_CONTROL.with(|cell| *cell.borrow_mut() = None);
                        st.set_bind_trigger_ready(false);
                        st.set_bind_trigger_label("—".into());
                        // leave the flow clean for the next author.
                        st.set_action_choice(0);
                        st.set_action_param("".into());
                        st.set_status_line(
                            match outcome {
                                crate::editor::AddOutcome::Added(n) => {
                                    format!("binding added ({n} GUI rule(s)) — live now: press it and watch the lamp")
                                }
                                crate::editor::AddOutcome::Replaced(_) => {
                                    "re-bound — the old action was replaced".to_string()
                                }
                            }
                            .into(),
                        );
                    }
                    Err(e) => st.set_status_line(format!("add failed: {e}").into()),
                }
            }
        });
    });

    // ── Spellweaving ─────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_record_gesture(move || {
            if let Some(app) = w.upgrade() {
                record_gesture(&app, &sh);
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_clear_gestures(move || {
            if let Some(app) = w.upgrade() {
                let n = {
                    let mut s = sh.borrow_mut();
                    s.rt.vault.templates.clear();
                    let _ = s.rt.vault.save();
                    // a cleared vault orphans every glyph→action binding: phantom rules for
                    // glyphs that can never be recognized again — and a future re-recorded
                    // "glyph_1" would silently inherit a stale action. Clear them together.
                    let n = s.rt.cast.gestures.len();
                    s.rt.cast.gestures.clear();
                    let _ = crate::editor::save_cast(&s.rt.cast);
                    n
                };
                refresh_gestures(&app, &sh);
                refresh_rules(&app, &sh);
                crate::dispatch::request_reload();
                let st = app.global::<State>();
                st.set_gesture_bind_target("".into()); // the bind card can't target a dead glyph
                st.set_status_line(
                    format!("gesture vault cleared ({n} glyph binding(s) removed)").into(),
                );
            }
        });
    });
    // delete ONE glyph — template + its cast binding, in one move.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_delete_gesture(move |name| {
            if let Some(app) = w.upgrade() {
                {
                    let mut s = sh.borrow_mut();
                    s.rt.vault.templates.retain(|t| t.name != name.as_str());
                    let _ = s.rt.vault.save();
                    s.rt.cast.gestures.remove(name.as_str());
                    let _ = crate::editor::save_cast(&s.rt.cast);
                }
                refresh_gestures(&app, &sh);
                refresh_rules(&app, &sh);
                crate::dispatch::request_reload();
                let st = app.global::<State>();
                if st.get_gesture_bind_target() == name {
                    st.set_gesture_bind_target("".into());
                }
                st.set_status_line(format!("glyph '{name}' deleted").into());
            }
        });
    });
    // rename a glyph — pure data surgery; the bound action follows the name.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_rename_gesture(move |old, new| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let new = new.to_string();
                let new = new.trim();
                if new.is_empty() {
                    st.set_status_line("a glyph needs a name".into());
                    return;
                }
                let res: Result<(), String> = {
                    let mut s = sh.borrow_mut();
                    if s.rt.vault.templates.iter().any(|t| t.name == new) {
                        Err(format!("a glyph named '{new}' already exists"))
                    } else if let Some(t) =
                        s.rt.vault
                            .templates
                            .iter_mut()
                            .find(|t| t.name == old.as_str())
                    {
                        t.name = new.to_string();
                        let _ = s.rt.vault.save();
                        if let Some(a) = s.rt.cast.gestures.remove(old.as_str()) {
                            s.rt.cast.gestures.insert(new.to_string(), a);
                            let _ = crate::editor::save_cast(&s.rt.cast);
                        }
                        Ok(())
                    } else {
                        Err(format!("no glyph '{old}'"))
                    }
                };
                match res {
                    Ok(()) => {
                        refresh_gestures(&app, &sh);
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_gesture_bind_target(new.into());
                        st.set_status_line(format!("renamed '{old}' → '{new}'").into());
                    }
                    Err(e) => st.set_status_line(e.into()),
                }
            }
        });
    });

    // ── Radial ───────────────────────────────────────────────────────────
    // SPELL ASSIST is the engine's continuous `assist` margin (cast.toml). The slider writes it
    // directly (0.0 = strict/off, up to ~0.6 = very forgiving) — no bool flattening.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_weave_assist(move |level| {
            if let Some(app) = w.upgrade() {
                let v = (level as f64).clamp(0.0, 0.6);
                let saved = {
                    let mut s = sh.borrow_mut();
                    s.rt.cast.assist = v;
                    crate::editor::save_cast(&s.rt.cast)
                };
                let st = app.global::<State>();
                st.set_weave_assist(v as f32);
                match saved {
                    Ok(()) => st.set_status_line(
                        if v <= 0.001 {
                            "spell assist off — strict recognition only".to_string()
                        } else {
                            format!("spell forgiveness → {}% (near-misses snap to a clear-winner spell)", (v * 100.0).round() as i32)
                        }
                        .into(),
                    ),
                    Err(e) => st.set_status_line(format!("assist not saved: {e}").into()),
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_sector_count(move |n| {
            if let Some(app) = w.upgrade() {
                // the cap is computed from the commit radius — never a designer's literal.
                // The floor is 3: the smallest wheel that still reads as directions.
                let max = neuron::radial::max_sectors(sh.borrow().rt.cast.deadzone) as i32;
                let n = n.clamp(3, max);
                let saved = {
                    let mut s = sh.borrow_mut();
                    s.rt.cast.sectors = n as usize;
                    // persist NOW — the live flick resolver reads disk, and "set 12 wedges,
                    // restart, it's 8 again" is a config editor that lies.
                    crate::editor::save_cast(&s.rt.cast)
                };
                refresh_radial(&app, &sh);
                refresh_rules(&app, &sh); // the reachable wedge slice changed
                crate::dispatch::request_reload();
                let st = app.global::<State>();
                match saved {
                    Ok(()) => st.set_status_line(format!("radial -> {n} sectors").into()),
                    Err(e) => st.set_status_line(format!("sector count not saved: {e}").into()),
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_preview_radial(move || {
            if let Some(app) = w.upgrade() {
                preview_radial(&app, &sh);
            }
        });
    });

    // ── Profiles & app rules ─────────────────────────────────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_apply_profile(move |name| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let profile_name = name.to_string();
                let persist = st.get_persist_to_onboard();
                st.set_status_line(format!("applying '{profile_name}'...").into());
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let result = crate::dispatch::apply_profile(profile_name, persist);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            match result {
                                Ok(applied) => {
                                    let policy = applied.policy;
                                    st.set_disable_alt_tab(policy.disable_alt_tab);
                                    st.set_disable_win(policy.disable_win);
                                    st.set_disable_alt_f4(policy.disable_alt_f4);
                                    st.set_active_profile(applied.name.clone().into());
                                    with_shared(|sh| {
                                        {
                                            let mut s = sh.borrow_mut();
                                            s.rt.persist = persist;
                                            s.rt.active_profile = applied.name.clone();
                                            s.rt.gaming_mode = policy;
                                        }
                                        refresh_profiles(&app, sh);
                                        refresh_devices(&app, sh);
                                    });
                                    st.set_status_line(applied.summary.into());
                                }
                                Err(e) => st.set_status_line(e.into()),
                            }
                        }
                    });
                });
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_save_current_profile(move |name| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let (dpi, hz, br) = (
                    st.get_dpi() as u16,
                    st.get_polling_hz() as u32,
                    st.get_brightness() as u8,
                );
                let msg = {
                    let mut s = sh.borrow_mut();
                    s.rt.persist = st.get_persist_to_onboard();
                    s.rt.save_profile_from_devices(name.as_str(), dpi, hz, br)
                };
                refresh_profiles(&app, &sh);
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_delete_profile(move |name| {
            if let Some(app) = w.upgrade() {
                let msg = sh.borrow_mut().rt.delete_profile(name.as_str());
                refresh_profiles(&app, &sh);
                let st = app.global::<State>();
                // a deleted ACTIVE profile must drop the header pill to none.
                st.set_active_profile(sh.borrow().rt.active_profile.clone().into());
                st.set_status_line(msg.into());
            }
        });
    });
    // live answer for "does this save-name already exist?" (overwrite, said before the click)
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_profile_name_edited(move |name| {
            if let Some(app) = w.upgrade() {
                let exists = sh
                    .borrow()
                    .rt
                    .profiles
                    .iter()
                    .any(|p| p.name == name.as_str());
                app.global::<State>().set_profile_name_exists(exists);
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_add_app_rule(move |a, p| {
            if let Some(app) = w.upgrade() {
                let msg = sh.borrow_mut().rt.add_app_rule(a.as_str(), p.as_str());
                refresh_app_rules(&app, &sh);
                crate::dispatch::request_reload(); // app rules feed the live engine too
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_remove_app_rule(move |idx| {
            if let Some(app) = w.upgrade() {
                sh.borrow_mut().rt.remove_app_rule(idx as usize);
                refresh_app_rules(&app, &sh);
                crate::dispatch::request_reload();
            }
        });
    });

    // ── Import wizard ────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_parse_import(move |path| {
            if let Some(app) = w.upgrade() {
                let res = migrate::parse(path.as_str());
                let map = |l: &migrate::PreviewLine| ImportLine {
                    label: l.label.clone().into(),
                    detail: l.detail.clone().into(),
                    ok: l.ok,
                    category: l.category.into(),
                };
                let all: Vec<ImportLine> = res.lines.iter().map(map).collect();
                let perf: Vec<ImportLine> = res
                    .lines
                    .iter()
                    .filter(|l| l.category == "perf" || l.category == "lighting")
                    .map(map)
                    .collect();
                let binds: Vec<ImportLine> = res
                    .lines
                    .iter()
                    .filter(|l| l.category == "binding")
                    .map(map)
                    .collect();
                let notes: Vec<ImportLine> = res
                    .lines
                    .iter()
                    .filter(|l| l.category == "note")
                    .map(map)
                    .collect();
                let st = app.global::<State>();
                st.set_import_preview(ModelRc::new(VecModel::from(all)));
                st.set_import_perf(ModelRc::new(VecModel::from(perf)));
                st.set_import_bindings(ModelRc::new(VecModel::from(binds)));
                st.set_import_notes(ModelRc::new(VecModel::from(notes)));
                st.set_import_status(res.status.into());
                st.set_import_ready(res.ready);
                sh.borrow_mut().pending_import = res.imported;
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        // NATIVE FILE PICKER — opens the real Windows file-open dialog (filtered to .synapse3 /
        // .ChromaEffects) on a worker thread, then fills the path field and auto-parses.
        app.global::<State>().on_browse_import(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // one dialog at a time — a second click would stack a second PowerShell picker.
                if st.get_import_status().as_str() == "opening file picker…" {
                    return;
                }
                st.set_import_status("opening file picker…".into());
                crate::filedlg::pick_synapse_export(&app, |app, path| {
                    let st = app.global::<State>();
                    if path.is_empty() {
                        st.set_import_status("no file selected.".into());
                        return;
                    }
                    st.set_import_path(path.clone().into());
                    // auto-parse the picked file immediately.
                    st.invoke_parse_import(path.into());
                });
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_import(move || {
            if let Some(app) = w.upgrade() {
                let res = {
                    let s = sh.borrow();
                    match &s.pending_import {
                        Some(imp) => migrate::apply(imp),
                        None => Err("nothing parsed".to_string()),
                    }
                };
                let st = app.global::<State>();
                match res {
                    Ok(msg) => {
                        {
                            let mut s = sh.borrow_mut();
                            s.rt.reload_profiles(); // apply wrote profiles/*.toml straight to disk
                            s.pending_import = None; // the one-time flow is consumed
                        }
                        refresh_profiles(&app, &sh);
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload(); // imported sidecar binds go live now
                        st.set_import_ready(false);
                        st.set_import_preview(ModelRc::new(VecModel::<ImportLine>::default()));
                        st.set_import_perf(ModelRc::new(VecModel::<ImportLine>::default()));
                        st.set_import_bindings(ModelRc::new(VecModel::<ImportLine>::default()));
                        st.set_import_notes(ModelRc::new(VecModel::<ImportLine>::default()));
                        st.set_import_path("".into());
                        st.set_import_status(
                            "Pick a Synapse export (.synapse3 / .ChromaEffects).".into(),
                        );
                        st.set_status_line(
                            format!("{msg} — press apply on the profile to make it live").into(),
                        );
                        st.set_import_open(false);
                        // land the user ON the imported profile, where the next press is obvious
                        // — profiles are a SHEET now (a global affordance), so open it.
                        st.set_profile_sheet_open(true);
                    }
                    Err(msg) => {
                        // failure keeps the wizard open and armed so the user can see + retry;
                        // the error lives on the wizard's OWN line, not just the global strip.
                        st.set_import_status(msg.clone().into());
                        st.set_status_line(msg.into());
                    }
                }
            }
        });
    });

    // ── Macro authoring (Python: check syntax -> save+register -> test run) ──
    // CHECK: ast.parse only (no execution) — the honest "dry-run" for full-power Python.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_macro_check(move |src| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if st.get_macro_busy() {
                    return;
                }
                st.set_macro_busy(true);
                st.set_macro_status("checking syntax…".into());
                let source = src.to_string();
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let result = neuron::macros::macro_host().check(&source);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            st.set_macro_busy(false);
                            match result {
                                Ok(defs) => {
                                    let has_entry = defs.iter().any(|d| d == "macro" || d == "main");
                                    let lines: Vec<SharedString> =
                                        defs.iter().map(|d| format!("def {d}").into()).collect();
                                    st.set_macro_dryrun(ModelRc::new(VecModel::from(lines)));
                                    st.set_macro_status(
                                        if has_entry {
                                            "syntax ok — has a macro(ctx) entry. Save to register, Test to run.".into()
                                        } else {
                                            "syntax ok — but no `def macro(ctx):` entry point yet".to_string()
                                        }
                                        .into(),
                                    );
                                }
                                Err(e) => {
                                    st.set_macro_dryrun(ModelRc::new(VecModel::<SharedString>::default()));
                                    st.set_macro_status(format!("syntax error: {}", first_line(&e)).into());
                                }
                            }
                        }
                    });
                });
            }
        });
    });
    // SAVE: persist macros/scripts/<name>.py and register it into the warm sidecar.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_macro_save(move |name, src| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let name = name.trim().to_string();
                if name.is_empty() {
                    st.set_macro_status("name the macro first".into());
                    return;
                }
                if st.get_macro_busy() {
                    return;
                }
                st.set_macro_busy(true);
                st.set_macro_status(format!("registering '{name}'…").into());
                let source = src.to_string();
                let back = app.as_weak();
                // register blocks up to FIRE_BUDGET for the ack — off the UI thread.
                std::thread::spawn(move || {
                    let res = neuron::macros::macro_host().register(&name, &source);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            st.set_macro_busy(false);
                            match res {
                                Ok(()) => st.set_macro_status(
                                    format!(
                                        "saved + registered '{name}' — bind it as a python macro"
                                    )
                                    .into(),
                                ),
                                Err(e) => st.set_macro_status(
                                    format!("save failed: {}", first_line(&e)).into(),
                                ),
                            }
                            // the saved macro may have gained/lost a `neuron.ask` — refresh the registry.
                            refresh_beacon_macros(&app);
                        }
                    });
                });
            }
        });
    });
    // TEST BEACON (SYSTEM panel): MOCK-fire the macro so its REAL `neuron.ask` raises the beacon, but
    // input is forced disarmed for that fire — every key/type/run/click no-ops. "See it, answer it,
    // nothing real happens." Non-blocking; the strip + answer wheel ride the live presenter.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_test_beacon(move |id| {
            let id = id.trim().to_string();
            if id.is_empty() {
                return;
            }
            // ONE test beacon at a time. The sidecar runs a macro's fires on a SERIAL worker, so a
            // second mock fire would queue BEHIND the first's neuron.ask() and only surface once it's
            // answered — rapid clicks build a backlog that "never stops" and starves the radial weave
            // (the presenter stays pinned in present() the whole time). This gate (lifted when the
            // beacon clears, see beacon::mirror_count) makes a test fire strictly one-at-a-time.
            if crate::beacon::TEST_BEACON_INFLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_status_line(
                        "a test beacon is already waiting — answer it before testing another".into(),
                    );
                }
                return;
            }
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_status_line(
                    format!("mock-firing '{id}' — its beacon will appear; answer it (nothing real runs)")
                        .into(),
                );
            }
            let back = w.clone();
            std::thread::spawn(move || {
                let host = neuron::macros::macro_host();
                // make sure the sidecar knows this macro (sync its current source from disk), then mock-fire.
                if let Some((_, src)) = neuron::macros::macro_host::scan_macro_dir()
                    .into_iter()
                    .find(|(mid, _)| *mid == id)
                {
                    let _ = host.register(&id, &src);
                }
                let ctx = neuron::macros::Context::capture();
                // SURFACE the result — a silent button is the worst UX. If the python runtime isn't
                // available (no bundle / NEURON_PYTHON / NEURON_ALLOW_SYSTEM_PYTHON), say so plainly.
                let result = host.fire_mock(&id, &ctx);
                // a successful dispatch raises a beacon that mirror_count will clear the gate for;
                // a FAILED dispatch (sidecar cold/dead/unavailable) raises none, so release the gate
                // here or the test button would stay locked until the next real beacon clears.
                if !result.contains("dispatched") {
                    crate::beacon::TEST_BEACON_INFLIGHT
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = back.upgrade() {
                        app.global::<State>()
                            .set_status_line(format!("beacon test \u{00b7} {result}").into());
                    }
                });
            });
        });
    });
    // TEST: register the current source under its name and run it ONCE against the live context.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_macro_test(move |name, src| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let name = if name.trim().is_empty() {
                    "__test__".to_string()
                } else {
                    name.trim().to_string()
                };
                if st.get_macro_busy() {
                    return;
                }
                st.set_macro_busy(true);
                st.set_macro_status("running once…".into());
                let source = src.to_string();
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let host = neuron::macros::macro_host();
                    let reg = host.register(&name, &source);
                    let (result, log) = match reg {
                        Ok(()) => {
                            let ctx = neuron::macros::Context::capture();
                            let r = neuron::macros::test_python_macro(&name, &ctx);
                            (r, host.drain_log())
                        }
                        Err(e) => (format!("register failed: {}", first_line(&e)), Vec::new()),
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            st.set_macro_busy(false);
                            st.set_macro_status(result.into());
                            let lines: Vec<SharedString> =
                                log.into_iter().map(Into::into).collect();
                            st.set_macro_dryrun(ModelRc::new(VecModel::from(lines)));
                        }
                    });
                });
            }
        });
    });

    // ── Diagnostics (the in-app test bench) ──────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_run_diagnostics(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if st.get_diag_running() {
                    return; // one bench run at a time
                }
                st.set_diag_running(true);
                st.set_diag_summary("probing…".into());
                // the probes do real device round-trips (seconds on an asleep wireless mouse) —
                // they run OFF the UI thread so the bench never freezes the instrument. The
                // probes are read-only and everything they need loads from disk, so a fresh
                // Runtime on the worker with the selected pid carried over is identical.
                let pid = sh.borrow().rt.selected_pid;
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let mut rt = AppRuntime::load();
                    rt.selected_pid = pid;
                    let probes = rt.run_diagnostics();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            let pass = probes.iter().filter(|p| p.state == "pass").count();
                            let fail = probes.iter().filter(|p| p.state == "fail").count();
                            let skip = probes.iter().filter(|p| p.state == "skip").count();
                            let rows: Vec<DiagRow> = probes
                                .into_iter()
                                .map(|p| DiagRow {
                                    name: p.name.into(),
                                    detail: p.detail.into(),
                                    state: p.state.into(),
                                })
                                .collect();
                            st.set_diagnostics(ModelRc::new(VecModel::from(rows)));
                            st.set_diag_pass(pass as i32);
                            st.set_diag_fail(fail as i32);
                            st.set_diag_skip(skip as i32);
                            st.set_diag_summary(
                                format!("{pass} passed · {fail} failed · {skip} skipped").into(),
                            );
                            st.set_diag_running(false);
                        }
                    });
                });
            }
        });
    });

    // ── Audio ────────────────────────────────────────────────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_mic_gain(move |v| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match mic::set_gain(v) {
                    Some(applied) => {
                        st.set_status_line(format!("mic gain {}%", applied.round()).into())
                    }
                    None => {
                        st.set_status_line("no capture device — gain unchanged".into());
                        mic::refresh(&app); // snap the fader back to truth
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_toggle_mic_mute(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match mic::toggle_mute() {
                    Some(muted) => {
                        st.set_mic_muted(muted);
                        st.set_status_line(
                            format!("mic {}", if muted { "muted" } else { "live" }).into(),
                        );
                    }
                    None => st.set_status_line("no capture device — nothing to mute".into()),
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_refresh_mic(move || {
            if let Some(app) = w.upgrade() {
                mic::refresh(&app);
            }
        });
    });
    // OUTPUT (render) audio — headphone / sound-card volume + mute, the mirror of the mic controls.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_out_gain(move |v| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match mic::set_output_gain(v) {
                    Some(applied) => {
                        st.set_status_line(format!("output vol {}%", applied.round()).into())
                    }
                    None => {
                        st.set_status_line("no output device — volume unchanged".into());
                        mic::refresh_output(&app);
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_toggle_out_mute(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match mic::toggle_output_mute() {
                    Some(muted) => {
                        st.set_out_muted(muted);
                        st.set_status_line(
                            format!("output {}", if muted { "muted" } else { "live" }).into(),
                        );
                    }
                    None => st.set_status_line("no output device — nothing to mute".into()),
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_refresh_out(move || {
            if let Some(app) = w.upgrade() {
                mic::refresh_output(&app);
            }
        });
    });

    // ── Settings ─────────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_toggle_writes_paused(move || {
            if let Some(app) = w.upgrade() {
                let paused = !neuron::writes::writes_paused();
                let stopped_anim = {
                    let mut s = sh.borrow_mut();
                    // the kill-switch covers the STREAMING path too: a 30fps generator writing
                    // through "writes PAUSED" would make the gate a lie.
                    let stopped = paused && s.rt.animating;
                    if stopped {
                        s.rt.stop_animation();
                    }
                    stopped
                };
                // Mirror into neuron-core's process-global gate so the LIVE dispatch worker also
                // freezes its DPI/scroll/profile device writes.
                neuron::writes::set_writes_paused(paused);
                let st = app.global::<State>();
                st.set_writes_paused(paused);
                st.set_arm_stance(arm_stance(paused, neuron::action::input_armed()));
                if stopped_anim {
                    st.set_animating(false);
                    st.set_applied_effect(-1);
                }
                st.set_status_line(
                    if stopped_anim {
                        "writes PAUSED — animation stopped".to_string()
                    } else {
                        format!("writes {}", if paused { "PAUSED" } else { "armed" })
                    }
                    .into(),
                );
            }
        });
    });
    // SAFE-MODE / arm-input gate — the ONE callback for the ONE gate (the header pill and the
    // Settings toggle both call it; one path, one wording).
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_toggle_input_armed(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let armed = !neuron::action::input_armed();
                neuron::action::arm_input(armed);
                neuron::macros::macro_host().set_armed(armed); // mirror SAFE/arm into the macro sidecar
                st.set_input_armed(armed);
                st.set_arm_stance(arm_stance(neuron::writes::writes_paused(), armed));
                st.set_status_line(
                    if armed {
                        "INPUT ARMED — remaps + macros now fire real keystrokes".into()
                    } else {
                        "safe-mode — input disarmed (simulate only)".to_string()
                    }
                    .into(),
                );
            }
        });
    });
    // ARM STANCE — the segmented selector: one move sets BOTH gates (writes + input) to the chosen
    // posture. The header pills + the two flags stay the truth; this just drives them together.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_arm_stance(move |m| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let mode = match m {
                    1 => neuron::safety::RuntimeMode::Device,
                    2 => neuron::safety::RuntimeMode::Input,
                    3 => neuron::safety::RuntimeMode::Live,
                    _ => neuron::safety::RuntimeMode::Observe,
                };
                let safety = mode.state();
                let paused = safety.writes_paused;
                let armed = safety.input_armed;
                let stopped_anim = {
                    let mut s = sh.borrow_mut();
                    let stop = paused && s.rt.animating;
                    if stop {
                        s.rt.stop_animation();
                    }
                    stop
                };
                neuron::safety::set_mode(mode);
                neuron::macros::macro_host().set_armed(armed);
                if stopped_anim {
                    st.set_animating(false);
                    st.set_applied_effect(-1);
                }
                st.set_writes_paused(paused);
                st.set_input_armed(armed);
                st.set_arm_stance(m);
                st.set_status_line(
                    match m {
                        1 => "arm → DEVICE (device writes on, input safe)",
                        2 => "arm → INPUT (remaps + macros fire, writes paused)",
                        3 => "arm → LIVE (device writes + input synthesis)",
                        _ => "arm → OBSERVE (read-only: nothing fires)",
                    }
                    .into(),
                );
            }
        });
    });
    // LAUNCH mode — one selector over the two underlying truths (registry autostart + the
    // start-minimized pref): 0 Manual · 1 Boot to tray · 2 Boot with window.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_launch_mode(move |m| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let msg = match m {
                    1 => {
                        let a = crate::autostart::set(true);
                        crate::prefs::set_start_minimized(true);
                        format!("boot to tray — {a}")
                    }
                    2 => {
                        let a = crate::autostart::set(true);
                        crate::prefs::set_start_minimized(false);
                        format!("boot with window — {a}")
                    }
                    _ => crate::autostart::set(false),
                };
                // re-derive from the REAL truths so a failed registry write snaps the selector back.
                st.set_launch_mode(launch_mode_now());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        // the try-buttons: open an instrument for real, right now — the playground IS the app.
        app.global::<State>().on_try_teleport(move || {
            crate::beacon::request_instrument(1);
            if let Some(app) = w.upgrade() {
                app.global::<State>()
                    .set_status_line("teleport primed \u{2014} hold your trigger and drag the ghost \u{00b7} rest on a window to peek \u{00b7} esc backs out".into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_try_whiteboard(move || {
            crate::beacon::request_instrument(2);
            if let Some(app) = w.upgrade() {
                app.global::<State>()
                    .set_status_line("whiteboard opening \u{2014} hold your trigger to ink \u{00b7} esc closes (ink stays)".into());
            }
        });
    });
    // APPEARANCE — the interface accent recolours the WHOLE instrument live (Theme.accent + every
    // derived glow/line/dim/ink) and persists; re-read the saved truth so a junk hex snaps back.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_ui_accent(move |hex| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_ui_accent(hex.as_str());
                let saved = crate::prefs::ui_accent();
                app.global::<Theme>().set_accent(accent_color(&saved));
                let st = app.global::<State>();
                st.set_ui_accent(saved.into());
                st.set_status_line(msg.into());
            }
        });
    });
    // The weave accent retints the spellweaving cast preview (trail + radial sigils) and persists.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_weave_accent(move |hex| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_weave_accent(hex.as_str());
                let saved = crate::prefs::weave_accent();
                apply_weave_accent(&saved); // retint the LIVE spellweaving material
                render_material_cards(&app, crate::weave::seconds()); // re-render the gallery NOW so the tint shows immediately
                let st = app.global::<State>();
                st.set_weave_accent_col(accent_color(&saved));
                st.set_weave_accent(saved.into());
                st.set_status_line(msg.into());
            }
        });
    });
    // LIVE hex preview — parse the in-progress field text WITHOUT committing (no prefs write, no
    // recolour, no status line). Drives the picker's preview swatch + validity hairline so a custom
    // hex shows its colour the instant it's complete, instead of leaving the user blind until Enter.
    // Same accept rule as the real commit (`normalize_hex`): 6 hex digits, optional leading '#'.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_preview_hex(move |hex| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let clean = hex.trim().trim_start_matches('#');
                let all_hex = clean.chars().all(|c| c.is_ascii_hexdigit());
                let ok = clean.len() == 6 && all_hex;          // complete + valid → morph the swatch
                st.set_hex_preview_ok(ok);
                st.set_hex_preview_bad(!all_hex || clean.len() > 6); // wrong char / overlong → danger; mid-typing stays neutral
                if ok {
                    st.set_hex_preview_col(accent_color(hex.as_str()));
                }
            }
        });
    });
    // PRISM picker — turn a (hue, brightness) point on the spectrum strip into a bare RRGGBB, morph
    // the live preview swatch, and stash the hex so a pointer-release commits it via the normal path.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_preview_hue(move |hue, bright| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // HSV → RGB at a fixed, phosphor-ish saturation; brightness rides a usable band (never
                // pure black) so the strip always yields a real accent.
                let h = hue.clamp(0.0, 1.0) * 6.0;
                let s = 0.82_f32;
                let v = 0.32 + 0.63 * bright.clamp(0.0, 1.0);
                let i = h.floor();
                let f = h - i;
                let (p, q, t) = (v * (1.0 - s), v * (1.0 - s * f), v * (1.0 - s * (1.0 - f)));
                let (r, g, b) = match (i as i32).rem_euclid(6) {
                    0 => (v, t, p),
                    1 => (q, v, p),
                    2 => (p, v, t),
                    3 => (p, q, v),
                    4 => (t, p, v),
                    _ => (v, p, q),
                };
                let c8 = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
                let hex = format!("{:02x}{:02x}{:02x}", c8(r), c8(g), c8(b));
                st.set_hex_preview_col(accent_color(&hex));
                st.set_hex_preview_ok(true);
                st.set_hex_preview_bad(false);
                st.set_hue_pick_hex(hex.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_goto_page(move |p| {
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_page(p);
            }
        });
    });
    // RELIABILITY — phoenix arm/disarm (persisted, next-launch), a manual flight dump, open the log.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_phoenix(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_phoenix(v);
                let st = app.global::<State>();
                st.set_phoenix(crate::prefs::phoenix());
                st.set_status_line(msg.into());
            }
        });
    });
    // NOTIFICATIONS — master switch, placement, audio cue, per-event gates, and the test fire. Each
    // persists to app.toml then reloads the truth back into the UI (the house load→save→reload).
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_enabled(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_enabled(v);
                let st = app.global::<State>();
                st.set_notif_enabled(crate::prefs::notif_enabled());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_placement(move |slug| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_placement(slug.as_str());
                let st = app.global::<State>();
                st.set_notif_placement(crate::prefs::notif_placement().into());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_audio(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_audio(v);
                let st = app.global::<State>();
                st.set_notif_audio(crate::prefs::notif_audio());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_volume(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_volume(v);
                let st = app.global::<State>();
                st.set_notif_volume(crate::prefs::notif_volume());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_sound(move |slug| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_sound(slug.as_str());
                let st = app.global::<State>();
                st.set_notif_sound(crate::prefs::notif_sound().into());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_panel(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_panel(v);
                let st = app.global::<State>();
                st.set_notif_panel(crate::prefs::notif_panel());
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_event(move |slug, v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_event(slug.as_str(), v);
                let st = app.global::<State>();
                let cur = crate::prefs::notif_event(slug.as_str());
                match slug.as_str() {
                    "dpi" => st.set_notif_dpi(cur),
                    "scroll" => st.set_notif_scroll(cur),
                    "polling" => st.set_notif_polling(cur),
                    "brightness" => st.set_notif_brightness(cur),
                    "profile" => st.set_notif_profile(cur),
                    "layer" => st.set_notif_layer(cur),
                    _ => {}
                }
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_test_notification(move || {
            if let Some(app) = w.upgrade() {
                let msg = crate::notifs::fire_test();
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_write_flight_dump(move || {
            if let Some(app) = w.upgrade() {
                crate::flight::dump_to_crash_log("manual snapshot");
                refresh_reliability(&app);
                app.global::<State>()
                    .set_status_line("flight snapshot appended to neuron-crash.log".into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_open_crash_log(move || {
            if let Some(app) = w.upgrade() {
                open_crash_log();
                app.global::<State>()
                    .set_status_line("opened neuron-crash.log".into());
            }
        });
    });
    // SPELLWEAVING MATERIAL — pick the cast's physics; animate the gallery swatches.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_weave_material(move |slug| {
            if let Some(app) = w.upgrade() {
                let surface = crate::weave::Surface::from_slug(slug.as_str());
                crate::weave::set_weave_surface(surface); // rebuild the live recipe (keeps the accent)
                let msg = crate::prefs::set_weave_material(surface.slug());
                let st = app.global::<State>();
                st.set_weave_material(surface.slug().into());
                refresh_weave_knobs(&app); // the new material surfaces its own knobs
                st.set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_weave_knob(move |i, v| {
            if let Some(app) = w.upgrade() {
                crate::weave::set_weave_knob(i as usize, v); // reshape the live substance
                                                             // patch JUST this row in place — a full model rebuild mid-drag re-instantiates the
                                                             // slider and makes it feel horrible. Re-read the clamped value and set one row.
                if let Some((label, value, min, max)) =
                    crate::weave::weave_knobs().into_iter().nth(i as usize)
                {
                    let model = app.global::<State>().get_weave_knobs();
                    if let Some(vm) = model.as_any().downcast_ref::<VecModel<KnobRow>>() {
                        vm.set_row_data(
                            i as usize,
                            KnobRow {
                                label: label.into(),
                                value,
                                min,
                                max,
                            },
                        );
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_reset_weave_material(move || {
            if let Some(app) = w.upgrade() {
                crate::weave::reset_weave_knobs();
                refresh_weave_knobs(&app); // re-seed the sliders with the defaults
                app.global::<State>()
                    .set_status_line("material knobs reset to defaults".into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_material_preview_tick(move || {
            if let Some(app) = w.upgrade() {
                render_material_cards(&app, crate::weave::seconds());
            }
        });
    });

    // ── cancel an in-flight press-to-bind capture ─────────────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_cancel_capture(move || {
            if let Some(app) = w.upgrade() {
                crate::capture::cancel();
                let st = app.global::<State>();
                st.set_capture_active(false);
                st.set_capture_prompt("".into());
            }
        });
    });

    // ── activation rhythm (set a preset / record your own) ────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_activation(move |pat| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match neuron::feel::Phrase::parse(pat.as_str()) {
                    Ok(phrase) => {
                        let saved = {
                            let mut s = sh.borrow_mut();
                            s.rt.cast.activation = phrase.describe();
                            crate::editor::save_cast(&s.rt.cast)
                        };
                        sync_activation_view(&st, &phrase.describe());
                        match saved {
                            Ok(()) => st.set_status_line(
                                format!(
                                    "weave opens on [{}]{}",
                                    phrase.describe(),
                                    if phrase.ends_in_hold() {
                                        ""
                                    } else {
                                        " — tap again to close"
                                    }
                                )
                                .into(),
                            ),
                            Err(e) => {
                                st.set_status_line(format!("activation not saved: {e}").into())
                            }
                        }
                    }
                    Err(e) => st.set_status_line(e.into()),
                }
            }
        });
    });
    // record YOUR rhythm on the cast trigger: perform it once, it normalizes into the grammar.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_record_activation(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if st.get_recording_activation() {
                    return;
                }
                st.set_recording_activation(true);
                st.set_status_line(
                    format!(
                        "perform the rhythm on {} now — taps and holds both count, ESC cancels",
                        st.get_cast_trigger_label()
                    )
                    .into(),
                );
                let trigger = sh.borrow().rt.cast.trigger;
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let cfg = neuron::feel::FeelConfig::load();
                    let t0 = std::time::Instant::now();
                    let mut presses: Vec<(u64, Option<u64>)> = Vec::new();
                    let mut down = false;
                    let mut last_edge = 0u64;
                    // generous close-out: the rhythm ends after a few gaps of silence.
                    let silence = cfg.gap_ms * 3;
                    let cancelled = loop {
                        if neuron::glyph::key_down(0x1B) {
                            break true;
                        }
                        let now = t0.elapsed().as_millis() as u64;
                        let d = neuron::glyph::key_down(trigger);
                        if d && !down {
                            presses.push((now, None));
                            down = true;
                            last_edge = now;
                        } else if !d && down {
                            if let Some(p) = presses.last_mut() {
                                p.1 = Some(now);
                            }
                            down = false;
                            last_edge = now;
                        }
                        if !presses.is_empty() && !down && now.saturating_sub(last_edge) > silence {
                            break false; // the performance ended
                        }
                        if presses.is_empty() && now > 6_000 {
                            break true; // nothing performed — give up quietly
                        }
                        std::thread::sleep(std::time::Duration::from_millis(3));
                    };
                    let phrase = if cancelled {
                        None
                    } else {
                        neuron::feel::Phrase::from_recording(&presses, &cfg)
                    };
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            st.set_recording_activation(false);
                            match phrase {
                                Some(p) => {
                                    with_shared(|sh| {
                                        let mut s = sh.borrow_mut();
                                        s.rt.cast.activation = p.describe();
                                        let _ = crate::editor::save_cast(&s.rt.cast);
                                    });
                                    sync_activation_view(&st, &p.describe());
                                    st.set_status_line(
                                        format!(
                                            "rhythm recorded — weave opens on [{}]",
                                            p.describe()
                                        )
                                        .into(),
                                    );
                                }
                                None => st.set_status_line("no rhythm recorded".into()),
                            }
                        }
                    });
                });
            }
        });
    });

    // ── HyperShift stance (feel.toml) ─────────────────────────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_hypershift_mode(move |mode| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let m = neuron::feel::LayerMode::parse(mode.as_str());
                let mut cfg = neuron::feel::FeelConfig::load();
                cfg.hypershift = m;
                match cfg.save() {
                    Ok(()) => {
                        st.set_hypershift_mode(m.describe().into());
                        // the live engine adopts the stance on its next tick.
                        crate::dispatch::request_reload();
                        st.set_status_line(
                            format!(
                                "HyperShift stance -> {} ({})",
                                m.describe(),
                                match m {
                                    neuron::feel::LayerMode::Hold => "live while held",
                                    neuron::feel::LayerMode::Latch => "tap on, tap off",
                                    neuron::feel::LayerMode::Smart =>
                                        "tap latches, hold is momentary",
                                    neuron::feel::LayerMode::OneShot => "next press only",
                                }
                            )
                            .into(),
                        );
                    }
                    Err(e) => st.set_status_line(format!("stance not saved: {e}").into()),
                }
            }
        });
    });

    // ── cast trigger press-to-bind (the hold button) ──────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_capture_cast_trigger(move || {
            if let Some(app) = w.upgrade() {
                let sh2 = sh.clone();
                crate::capture::begin(&app, false, move |app, vk, name| {
                    let st = app.global::<State>();
                    if vk == 0 {
                        st.set_status_line("cast trigger capture cancelled".into());
                        return;
                    }
                    {
                        let mut s = sh2.borrow_mut();
                        s.rt.cast.trigger = vk;
                        let _ = crate::editor::save_cast(&s.rt.cast);
                    }
                    crate::dispatch::request_reload();
                    st.set_cast_trigger_label(name.into());
                    st.set_status_line(format!("cast trigger -> {name}").into());
                });
            }
        });
    });

    // ── the picker moved: recompute the param suggestion chips ────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_action_choice_changed(move || {
            if let Some(app) = w.upgrade() {
                refresh_param_suggestions(&app.global::<State>());
            }
        });
    });

    // ── action-param press-to-fill (the picker's CAPTURE contact) ─────────
    // Press the actual key/button/chord; the picker fills itself — and retargets the action
    // kind to what you pressed (mouse button -> mouse, media key -> media). Nothing typed.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_capture_action_param(move || {
            if let Some(app) = w.upgrade() {
                crate::capture::begin_chord(&app, move |app, vk, mods, name| {
                    let st = app.global::<State>();
                    if vk == 0 {
                        st.set_status_line("capture cancelled".into());
                        return;
                    }
                    let (id, param) = crate::editor::vk_to_palette(vk, mods);
                    // a key landing while TURBO is selected fills turbo's key and keeps its cps.
                    let (cur_id, cur_param) = current_action(&st);
                    let (id, param) = if cur_id == "turbo" && id == "key" {
                        (
                            "turbo",
                            match cur_param.split_once('\u{00b7}') {
                                Some((_, cps)) if !cps.trim().is_empty() => {
                                    format!("{param} \u{00b7} {}", cps.trim())
                                }
                                _ => param,
                            },
                        )
                    } else {
                        (id, param)
                    };
                    if let Some(idx) = st
                        .get_action_choices()
                        .iter()
                        .position(|c| !c.header && c.id == id)
                    {
                        st.set_action_choice(idx as i32);
                    }
                    st.set_action_param(param.as_str().into());
                    st.set_status_line(format!("captured {name} \u{2192} {id} · {param}").into());
                });
            }
        });
    });
    // ── key-sequence recorder (the picker's REC/STOP contact) ─────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_record_action_keys(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if crate::capture::recording() {
                    crate::capture::stop_recording(); // the worker posts the take
                    return;
                }
                st.set_recording_keys(true);
                crate::capture::begin_keyseq(&app, move |app, grammar| {
                    let st = app.global::<State>();
                    st.set_recording_keys(false);
                    match grammar {
                        Some(g) => {
                            if let Some(idx) = st
                                .get_action_choices()
                                .iter()
                                .position(|c| !c.header && c.id == "keys")
                            {
                                st.set_action_choice(idx as i32);
                            }
                            st.set_action_param(g.as_str().into());
                            st.set_status_line(
                                format!("take: {g} — trim or retime it freely").into(),
                            );
                        }
                        None => st.set_status_line("recording ended — nothing played".into()),
                    }
                });
            }
        });
    });

    // ── radial: edit a sector's action (open picker -> commit) ────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_edit_sector(move |i| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // edit shows CURRENT: preset the shared picker to this wedge's real action — from the
                // base OR HyperShift set, per the editor target — so leftovers can't leak in.
                let hyper = st.get_radial_edit_hyper();
                let cur = {
                    let s = sh.borrow();
                    let set = if hyper {
                        &s.rt.cast.hyper_radial
                    } else {
                        &s.rt.cast.radial
                    };
                    set.get(i.max(0) as usize)
                        .cloned()
                        .unwrap_or(neuron::action::Action::Noop)
                };
                preset_picker(&st, &cur);
                st.set_gesture_bind_target("".into()); // close the glyph editor — one editor owns the shared picker at a time
                st.set_rhythm_bind_target(-1); // …and the rhythm editor
                st.set_editing_sector(i);
                let n = sh.borrow().rt.cast.sectors.max(1);
                let compass = neuron::radial::compass(i.max(0) as usize, n);
                st.set_status_line(
                    format!("editing wedge {i} ({compass}) — choose an action, then Set").into(),
                );
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_commit_sector_action(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let i = st.get_editing_sector();
                if i < 0 {
                    return;
                }
                let (id, param) = current_action(&st);
                if let Err(e) = crate::editor::validate_action(&id, &param) {
                    st.set_status_line(e.into());
                    return;
                }
                let action = crate::editor::build_action(&id, &param);
                let hyper = st.get_radial_edit_hyper();
                let res = {
                    let mut s = sh.borrow_mut();
                    crate::editor::set_sector_action_on(&mut s.rt.cast, i as usize, action, hyper)
                };
                match res {
                    Ok(()) => {
                        refresh_radial(&app, &sh);
                        refresh_rules(&app, &sh); // the wedge IS a rule — the spine list must agree
                        crate::dispatch::request_reload();
                        st.set_editing_sector(-1);
                        st.set_status_line(format!("wedge {i} set").into());
                    }
                    Err(e) => st.set_status_line(format!("set failed: {e}").into()),
                }
            }
        });
    });
    // ── HyperShift radial: enable the swap (persists), and retarget the editor base ⇄ hyper ──
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_hyper_radial_on(move |on| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let res = {
                    let mut s = sh.borrow_mut();
                    s.rt.cast.hyper_radial_on = on;
                    crate::editor::save_cast(&s.rt.cast)
                };
                match res {
                    Ok(()) => {
                        st.set_hyper_radial_on(on);
                        crate::dispatch::request_reload(); // the engine picks up the hypershift-layer wedge rules
                        st.set_status_line(if on {
                            "HyperShift radial enabled".into()
                        } else {
                            "HyperShift radial off".into()
                        });
                    }
                    Err(e) => st.set_status_line(format!("save failed: {e}").into()),
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_set_radial_edit_hyper(move |hyper| {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    st.set_radial_edit_hyper(hyper);
                    st.set_editing_sector(-1); // close any open wedge edit on the old set
                    refresh_radial(&app, &sh); // re-render the items for the now-targeted set
                    st.set_status_line(if hyper {
                        "editing the HyperShift radial".into()
                    } else {
                        "editing the base radial".into()
                    });
                }
            });
    });

    // ── gesture press-to-bind: bind a recorded glyph -> an action ─────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_bind_gesture(move |name| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // edit shows CURRENT here too: an already-bound glyph opens on its real action.
                let cur = sh
                    .borrow()
                    .rt
                    .cast
                    .gestures
                    .get(name.as_str())
                    .cloned()
                    .unwrap_or(neuron::action::Action::Noop);
                preset_picker(&st, &cur);
                st.set_editing_sector(-1); // close the radial editor — one editor owns the shared picker at a time
                st.set_rhythm_bind_target(-1); // …and the rhythm editor
                st.set_gesture_bind_target(name.clone());
                st.set_status_line(format!("glyph '{name}' — bind, rename, or delete").into());
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_commit_gesture_bind(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let name = st.get_gesture_bind_target().to_string();
                if name.is_empty() {
                    return;
                }
                let (id, param) = current_action(&st);
                if let Err(e) = crate::editor::validate_action(&id, &param) {
                    st.set_status_line(e.into());
                    return;
                }
                let action = crate::editor::build_action(&id, &param);
                let res = {
                    let mut s = sh.borrow_mut();
                    crate::editor::set_gesture_action(&mut s.rt.cast, &name, action)
                };
                match res {
                    Ok(()) => {
                        refresh_gestures(&app, &sh); // the chip now shows its binding
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_gesture_bind_target("".into());
                        st.set_status_line(format!("glyph '{name}' bound").into());
                    }
                    Err(e) => st.set_status_line(format!("bind failed: {e}").into()),
                }
            }
        });
    });

    // ── rhythm press-to-bind: bind a cast RHYTHM (Trigger::Cast{taps}) -> an action ──
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_bind_rhythm(move |taps| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // edit shows CURRENT: an already-bound rhythm opens on its real action.
                let cur = sh
                    .borrow()
                    .rt
                    .cast
                    .rhythm_actions
                    .iter()
                    .find(|rb| rb.taps as i32 == taps)
                    .map(|rb| rb.action.clone())
                    .unwrap_or(neuron::action::Action::Noop);
                preset_picker(&st, &cur);
                // one editor owns the shared picker at a time — close the radial + glyph editors.
                st.set_editing_sector(-1);
                st.set_gesture_bind_target("".into());
                st.set_rhythm_bind_target(taps);
                st.set_status_line(
                    format!("rhythm {} — pick what it fires", neuron::cast::taps_phrase(taps as u8))
                        .into(),
                );
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_commit_rhythm_bind(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let taps = st.get_rhythm_bind_target();
                if taps < 0 {
                    return;
                }
                let (id, param) = current_action(&st);
                if let Err(e) = crate::editor::validate_action(&id, &param) {
                    st.set_status_line(e.into());
                    return;
                }
                let action = crate::editor::build_action(&id, &param);
                let res = {
                    let mut s = sh.borrow_mut();
                    crate::editor::set_rhythm_action(&mut s.rt.cast, taps as u8, action)
                };
                match res {
                    Ok(()) => {
                        refresh_rhythms(&app, &sh); // the row now shows its binding
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload(); // the engine folds the new Cast rule
                        st.set_rhythm_bind_target(-1);
                        st.set_status_line(
                            format!("rhythm {} bound", neuron::cast::taps_phrase(taps as u8)).into(),
                        );
                    }
                    Err(e) => st.set_status_line(format!("bind failed: {e}").into()),
                }
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_delete_rhythm(move |taps| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let res = {
                    let mut s = sh.borrow_mut();
                    crate::editor::delete_rhythm_action(&mut s.rt.cast, taps as u8)
                };
                match res {
                    Ok(()) => {
                        refresh_rhythms(&app, &sh);
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_rhythm_bind_target(-1);
                        st.set_status_line(
                            format!("rhythm {} unbound", neuron::cast::taps_phrase(taps as u8))
                                .into(),
                        );
                    }
                    Err(e) => st.set_status_line(format!("unbind failed: {e}").into()),
                }
            }
        });
    });

    // ── the node board: drag, persist, tune wire timings ──────────────────
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_move_node(move |i, x, y| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let model = st.get_graph_nodes();
                if let Some(vm) = model.as_any().downcast_ref::<VecModel<GraphNode>>() {
                    if let Some(mut n) = vm.row_data(i.max(0) as usize) {
                        n.x = x.clamp(0.0, GBOARD_W - GNODE_W);
                        n.y = y.clamp(0.0, GBOARD_H - GNODE_H);
                        vm.set_row_data(i.max(0) as usize, n);
                    }
                }
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_node_dropped(move || {
            if let Some(app) = w.upgrade() {
                // persist the board layout: merge the live positions over the saved doc, so
                // nodes of rules that aren't currently materialized keep their spots.
                let mut layout = GraphLayout::load();
                for n in app.global::<State>().get_graph_nodes().iter() {
                    layout.nodes.insert(n.key.to_string(), (n.x, n.y));
                }
                let _ = layout.save();
            }
        });
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_set_edge_timing(move |edge_idx, value| {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    let Some(edge) = st.get_graph_edges().row_data(edge_idx.max(0) as usize) else {
                        return;
                    };
                    if !edge.editable || edge.rule < 0 {
                        st.set_status_line("that wire's timing is fixed".into());
                        return;
                    }
                    let Ok(v) = value.trim().parse::<u32>() else {
                        st.set_status_line("timing needs a plain number".into());
                        return;
                    };
                    let mut rules = crate::editor::load_gui_rules();
                    let Some(rule) = rules.get_mut(edge.rule as usize) else {
                        st.set_status_line("rule changed — reload".into());
                        return;
                    };
                    let msg = match (edge.tkind.as_str(), &mut rule.action) {
                        ("turbo", neuron::action::Action::Turbo { cps, .. }) => {
                            *cps = v.clamp(1, 100) as u16;
                            format!("turbo -> {} cps", *cps)
                        }
                        ("delay", neuron::action::Action::Sequence { steps }) => {
                            let si = edge.step.max(0) as usize;
                            if let Some(s) = steps.get_mut(si) {
                                s.delay_ms = v.min(60_000);
                                format!("step {} delay -> {} ms", si + 1, s.delay_ms)
                            } else {
                                st.set_status_line("step changed — reload".into());
                                return;
                            }
                        }
                        _ => {
                            st.set_status_line("that wire's timing is fixed".into());
                            return;
                        }
                    };
                    match crate::editor::save_gui_rules(&rules) {
                        Ok(()) => {
                            refresh_rules(&app, &sh); // rebuilds the board too
                            crate::dispatch::request_reload();
                            st.set_status_line(msg.into());
                        }
                        Err(e) => st.set_status_line(format!("timing not saved: {e}").into()),
                    }
                }
            });
    });

    // ── perf controls (Device panel) ──────────────────────────────────────
    install_perf_callbacks(app, &shared);

    // reflect current gate/autostart/prefs state into the view (input stays DISARMED until armed)
    st.set_launch_mode(launch_mode_now());
    st.set_writes_paused(neuron::writes::writes_paused());
    st.set_input_armed(neuron::action::input_armed());
    st.set_arm_stance(arm_stance(
        neuron::writes::writes_paused(),
        neuron::action::input_armed(),
    ));
    // APPEARANCE — paint the saved accents into the live theme + the customiser controls at startup,
    // so a relaunch comes up in the user's colours (the UI accent recolours the whole instrument).
    {
        let ui = crate::prefs::ui_accent();
        let weave = crate::prefs::weave_accent();
        app.global::<Theme>().set_accent(accent_color(&ui));
        st.set_ui_accent(ui.into());
        apply_weave_accent(&weave); // fold the saved weave colour into the live cast material at boot
        st.set_weave_accent_col(accent_color(&weave));
        st.set_weave_accent(weave.into());
        // the saved spellweaving material → drive the live cast surface (keeps the accent applied above)
        let surface = crate::weave::Surface::from_slug(&crate::prefs::weave_material());
        crate::weave::set_weave_surface(surface);
        st.set_weave_material(surface.slug().into());
    }
    refresh_weave_knobs(app);
    render_material_cards(app, 0.0);
    // seed the DIAGNOSTICS bench with its pending stations so the "prove it works" panel always shows
    // its 3×3 station grid (lit live on a run) instead of an empty void below the config block.
    {
        let rows: Vec<DiagRow> = crate::runtime::pending_diagnostic_stations()
            .into_iter()
            .map(|p| DiagRow {
                name: p.name.into(),
                detail: p.detail.into(),
                state: p.state.into(),
            })
            .collect();
        st.set_diagnostics(ModelRc::new(VecModel::from(rows)));
        st.set_diag_summary("9 stations idle — run to fire every capability live".into());
    }
    // RELIABILITY — seed the phoenix switch from prefs + paint the first flight snapshot.
    st.set_phoenix(crate::prefs::phoenix());
    refresh_reliability(app);

    // NOTIFICATIONS — seed every control from the saved prefs.
    st.set_notif_enabled(crate::prefs::notif_enabled());
    st.set_notif_placement(crate::prefs::notif_placement().into());
    st.set_notif_audio(crate::prefs::notif_audio());
    st.set_notif_volume(crate::prefs::notif_volume());
    st.set_notif_sound(crate::prefs::notif_sound().into());
    st.set_notif_panel(crate::prefs::notif_panel());
    st.set_notif_dpi(crate::prefs::notif_event("dpi"));
    st.set_notif_scroll(crate::prefs::notif_event("scroll"));
    st.set_notif_polling(crate::prefs::notif_event("polling"));
    st.set_notif_brightness(crate::prefs::notif_event("brightness"));
    st.set_notif_profile(crate::prefs::notif_event("profile"));
    st.set_notif_layer(crate::prefs::notif_event("layer"));

    shared
}

/// Seed the surfaced perf-control readouts from PERSISTED + DEVICE truth: the idle read-back, the
/// sniper binding (sniper.toml), and the device's real DPI stage table. A panel that renders a
/// compile-time default as if it were a reading breaks the instrument promise.
fn init_perf_controls(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    // live idle-timeout read-back (best-effort; "—" if the device is asleep / has none).
    let idle = sh.borrow().rt.read_idle_secs();
    match idle {
        Some(s) => {
            st.set_idle_readout(format!("{s}s").into());
            st.set_idle_secs(s as f32);
            st.set_idle_secs_text(s.to_string().into());
        }
        None => st.set_idle_readout("—".into()),
    }
    sync_idle_editor(&st);
    sync_scroll_stage_editor(&st);
    // sniper read-back: reflect the on-disk binding — never a fictional default.
    let cfg = crate::editor::SniperConfig::load();
    if cfg.dpi != 0 {
        st.set_sniper_dpi(cfg.dpi as f32);
    }
    st.set_sniper_button(match cfg.button {
        Some(vk) => neuron::capture::vk_name(vk).into(),
        None => "—".into(),
    });
    // the device's real stage table seeds the editor + the fader detents when readable.
    let stages = sh.borrow().rt.read_dpi_stages();
    if !stages.is_empty() {
        let joined = stages
            .iter()
            .map(u16::to_string)
            .collect::<Vec<_>>()
            .join("/");
        st.set_dpi_stages(joined.into());
    }
    sync_stage_nums(&st);
    // lift-off-distance + debounce have no derivable opcode yet -> honestly unsupported.
    st.set_lod_supported(false);
    st.set_debounce_supported(false);
}

struct DpiStageParse {
    nums: Vec<i32>,
    valid: bool,
    note: String,
}

fn parse_dpi_stage_editor(list: &str) -> DpiStageParse {
    let mut nums = Vec::new();
    let mut bad = Vec::new();
    for tok in list
        .split(['/', ',', ' '])
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        match tok.parse::<u16>() {
            Ok(v) if (DPI_MIN..=DPI_MAX).contains(&v) => nums.push(v as i32),
            _ => bad.push(tok.to_string()),
        }
    }

    let valid = !nums.is_empty() && nums.len() <= DPI_STAGE_CAPACITY && bad.is_empty();
    let note = if !bad.is_empty() {
        format!("invalid: {} (use {DPI_MIN}-{DPI_MAX})", bad.join(", "))
    } else if nums.is_empty() {
        "enter at least one DPI stage".into()
    } else if nums.len() > DPI_STAGE_CAPACITY {
        format!(
            "device table holds {DPI_STAGE_CAPACITY} stages; remove {}",
            nums.len() - DPI_STAGE_CAPACITY
        )
    } else {
        let plural = if nums.len() == 1 { "" } else { "s" };
        format!("{} stage{plural} ready", nums.len())
    };

    DpiStageParse { nums, valid, note }
}

/// Re-parse the stage-list string into the [int] model the chips + fader detents render, clamping
/// the active-stage index into the new range.
fn sync_stage_nums(st: &State) {
    let parsed = parse_dpi_stage_editor(st.get_dpi_stages().as_str());
    let nums = parsed.nums;
    let max_idx = (nums.len() as i32 - 1).max(0);
    if st.get_dpi_active_stage() > max_idx {
        st.set_dpi_active_stage(max_idx);
    }
    st.set_dpi_stages_valid(parsed.valid);
    st.set_dpi_stages_note(parsed.note.into());
    st.set_dpi_stage_nums(ModelRc::new(VecModel::from(nums)));
}

struct ScrollStageParse {
    valid: bool,
    note: String,
}

fn parse_scroll_stage_editor(list: &str) -> ScrollStageParse {
    let mut count = 0usize;
    let mut bad = Vec::new();
    for tok in list
        .split(['/', ',', ' '])
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let t = tok.to_lowercase();
        if t.starts_with("free") || t.starts_with("tact") || t.parse::<u8>().is_ok() {
            count += 1;
        } else {
            bad.push(tok.to_string());
        }
    }

    let valid = count > 0 && bad.is_empty();
    let note = if !bad.is_empty() {
        format!("invalid: {} (use tactile/free)", bad.join(", "))
    } else if count == 0 {
        "enter tactile/free modes".into()
    } else {
        let plural = if count == 1 { "" } else { "s" };
        format!("{count} mode{plural} ready")
    };

    ScrollStageParse { valid, note }
}

fn sync_scroll_stage_editor(st: &State) {
    let parsed = parse_scroll_stage_editor(st.get_scroll_stages().as_str());
    st.set_scroll_stages_valid(parsed.valid);
    st.set_scroll_stages_note(parsed.note.into());
}

struct IdleParse {
    secs: Option<u32>,
    note: String,
}

fn parse_idle_editor(text: &str) -> IdleParse {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return IdleParse {
            secs: None,
            note: "enter seconds".into(),
        };
    }
    match trimmed.parse::<u32>() {
        Ok(v) if (IDLE_MIN_SECS..=IDLE_MAX_SECS).contains(&v) => {
            let note = if v == 0 {
                "never sleep ready".into()
            } else {
                format!("{v}s ready")
            };
            IdleParse {
                secs: Some(v),
                note,
            }
        }
        _ => IdleParse {
            secs: None,
            note: format!("use {IDLE_MIN_SECS}-{IDLE_MAX_SECS} seconds"),
        },
    }
}

fn sync_idle_editor(st: &State) {
    let parsed = parse_idle_editor(st.get_idle_secs_text().as_str());
    match parsed.secs {
        Some(secs) => {
            st.set_idle_secs(secs as f32);
            st.set_idle_secs_valid(true);
        }
        None => st.set_idle_secs_valid(false),
    }
    st.set_idle_secs_note(parsed.note.into());
}

/// Wire the Device-panel perf controls to the runtime (all gated by writes-paused, honest [gated]).
fn install_perf_callbacks(app: &AppWindow, shared: &SharedRt) {
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_dpi_stages_edited(move |_t| {
            if let Some(app) = w.upgrade() {
                sync_stage_nums(&app.global::<State>());
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_apply_dpi_stages(move |list, active| {
                perf(&w, &sh, |rt| {
                    rt.apply_dpi_stages(list.as_str(), active.max(0) as u8)
                });
            });
    });
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_scroll_stages_edited(move |_t| {
            if let Some(app) = w.upgrade() {
                sync_scroll_stage_editor(&app.global::<State>());
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_scroll_stages(move |list| {
            perf(&w, &sh, |rt| rt.apply_scroll_stages(list.as_str()));
        });
    });
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_idle_edited(move |_t| {
            if let Some(app) = w.upgrade() {
                sync_idle_editor(&app.global::<State>());
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_idle(move |secs| {
            perf(&w, &sh, |rt| rt.apply_idle(secs as u32));
            // the read-back line must show the device's NEW value (or its refusal).
            if let Some(app) = w.upgrade() {
                refresh_idle_readout(&app, &sh);
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>()
            .on_apply_ingame_polling(move |wired, dongle| {
                perf(&w, &sh, |rt| {
                    rt.apply_ingame_polling(wired as u32, dongle as u32)
                });
            });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_gaming_mode(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let mut policy = neuron::writes::GamingMode::from_profile(
                    st.get_disable_alt_tab(),
                    st.get_disable_win(),
                    st.get_disable_alt_f4(),
                );
                policy.disable_alt_esc = st.get_disable_alt_esc(); // live-only guard (no profile source)
                sh.borrow_mut().rt.gaming_mode = policy;
                // push the policy to the live dispatch thread so its LL hook (de)activates.
                crate::dispatch::set_gaming_policy(policy);
                let on = policy.any();
                st.set_perf_status(
                    if on {
                        "gaming-mode ARMED (host-side LL hook — no device write)".into()
                    } else {
                        "gaming-mode off".to_string()
                    }
                    .into(),
                );
                st.set_status_line(st.get_perf_status());
            }
        });
    });
    // sniper: bind the hold button by pressing it + save the precision DPI.
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_capture_sniper_button(move || {
            if let Some(app) = w.upgrade() {
                crate::capture::begin(&app, false, move |app, vk, name| {
                    let st = app.global::<State>();
                    if vk == 0 {
                        st.set_perf_status("sniper bind cancelled".into());
                        return;
                    }
                    let mut cfg = crate::editor::SniperConfig::load();
                    cfg.button = Some(vk);
                    if cfg.dpi == 0 {
                        cfg.dpi = st.get_sniper_dpi() as u16;
                    }
                    let _ = cfg.save();
                    st.set_sniper_button(name.into());
                    st.set_perf_status(format!("sniper button -> {name}").into());
                });
            }
        });
    });
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_save_sniper(move |dpi| {
            if let Some(app) = w.upgrade() {
                let mut cfg = crate::editor::SniperConfig::load();
                cfg.dpi = dpi as u16;
                let _ = cfg.save();
                app.global::<State>()
                    .set_perf_status(format!("sniper DPI -> {}", dpi as u16).into());
            }
        });
    });
}

/// Run a perf-control op (honors writes-paused via the runtime), show its result in perf-status +
/// the global status line, and re-read the channel rows the op may have changed.
fn perf(w: &slint::Weak<AppWindow>, sh: &SharedRt, op: impl FnOnce(&mut AppRuntime) -> String) {
    if let Some(app) = w.upgrade() {
        let persist = app.global::<State>().get_persist_to_onboard();
        let msg = {
            let mut s = sh.borrow_mut();
            s.rt.persist = persist;
            op(&mut s.rt)
        };
        let st = app.global::<State>();
        st.set_perf_status(msg.clone().into());
        st.set_status_line(msg.into());
        refresh_devices(&app, sh);
    }
}

/// Small helper: register a callback against the app + shared runtime.
fn bind(app: &AppWindow, shared: &SharedRt, f: impl FnOnce(&AppWindow, &SharedRt)) {
    f(app, shared);
}

/// Run a runtime op that returns a status line, show it, and refresh the channel readouts so the
/// instrument's own rows never contradict its own confirmation message.
fn status(w: &slint::Weak<AppWindow>, sh: &SharedRt, op: impl FnOnce(&mut AppRuntime) -> String) {
    if let Some(app) = w.upgrade() {
        let persist = app.global::<State>().get_persist_to_onboard();
        let msg = {
            let mut s = sh.borrow_mut();
            s.rt.persist = persist;
            op(&mut s.rt)
        };
        app.global::<State>().set_status_line(msg.into());
        refresh_devices(&app, sh);
    }
}

/// Refresh the idle-timeout read-back line (the device's CURRENT value, best-effort).
fn refresh_idle_readout(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    match sh.borrow().rt.read_idle_secs() {
        Some(s) => {
            st.set_idle_readout(format!("{s}s").into());
            st.set_idle_secs(s as f32);
            st.set_idle_secs_text(s.to_string().into());
            sync_idle_editor(&st);
        }
        None => st.set_idle_readout("—".into()),
    }
}

// ── live-loop → UI mirrors (called from dispatch's post_status on the UI thread) ────

/// A live ProfileSwitch/Cycle moved the process-wide cursor — mirror it into the header pill, the
/// GUI runtime (so save-profile captures current gaming-mode etc.), and the Profiles panel.
pub fn note_live_profile(app: &AppWindow, name: &str) {
    let st = app.global::<State>();
    if st.get_active_profile() != name {
        st.set_active_profile(name.into());
    }
    with_shared(|sh| {
        if sh.borrow().rt.active_profile != name {
            sh.borrow_mut().rt.active_profile = name.to_string();
            refresh_profiles(app, sh);
        }
    });
}

/// The focused app changed — light the app-rule contact that's currently winning (first match, the
/// same top-to-bottom contract `AppRules::profile_for` applies).
pub fn note_focused_app(app: &AppWindow, focused: &str) {
    let st = app.global::<State>();
    let lf = focused.to_lowercase();
    let idx = with_shared_ret(|sh| {
        sh.borrow()
            .rt
            .app_rules
            .rules
            .iter()
            .position(|r| !r.app.is_empty() && lf.contains(&r.app.to_lowercase()))
            .map(|i| i as i32)
            .unwrap_or(-1)
    })
    .unwrap_or(-1);
    if st.get_active_app_rule() != idx {
        st.set_active_app_rule(idx);
    }
}

/// `with_shared` that returns a value (None if the shared runtime isn't installed).
fn with_shared_ret<T>(f: impl FnOnce(&SharedRt) -> T) -> Option<T> {
    UI_SHARED.with(|s| s.borrow().as_ref().map(f))
}

// ── refreshers: pull engine state into the view models ──────────────────────

/// EMERGENT audio endpoints — every mic (Capture) and every output/headset/sound-card (Render) the OS
/// exposes, Razer or not, with no hardcoding. Rendered as device rows alongside the HID peripherals.
fn audio_rows() -> Vec<DeviceRow> {
    use neuron::audio::Flow;
    let mut out = Vec::new();
    for (flow, kind, icon) in [
        (Flow::Capture, "mic", "mic"),
        (Flow::Render, "output", "output"),
    ] {
        for e in neuron::audio::endpoints(flow) {
            // RAZER-ONLY: this is a Razer control tool, not the system mixer. Brand-scope by the
            // endpoint's product name — any Razer audio gear (Seiren, Kraken, USB sound card, …) emerges
            // automatically; the Oculus virtual audio / NVIDIA monitor speakers / etc. are not Razer
            // hardware and stay out. (A future hardening could resolve the endpoint's USB VID == 0x1532.)
            if !e.name.to_lowercase().contains("razer") {
                continue;
            }
            let vol = (e.volume * 100.0).round() as i32;
            let detail = if e.muted {
                format!("{kind} \u{00b7} muted")
            } else {
                format!("{kind} \u{00b7} {vol}%")
            };
            out.push(DeviceRow {
                name: e.name.into(),
                codename: "".into(),
                pid: "".into(),
                mode: "".into(),
                connected: true,
                firmware: "\u{2014}".into(),
                dpi: "\u{2014}".into(),
                polling: "\u{2014}".into(),
                brightness: "\u{2014}".into(),
                battery: "".into(),
                battery_frac: -1.0,
                charging: false,
                storage: "".into(),
                icon: icon.into(),
                kind: kind.into(),
                id: e.id.into(),
                detail: detail.into(),
                // audio endpoints have none of the HID capabilities — the audio card shows instead.
                cap_dpi: false,
                cap_poll: false,
                cap_light: false,
                cap_scroll: false,
                cap_store: false,
                cap_idle: false,
            });
        }
    }
    out
}

/// The selected device row's audio endpoint id, if it IS an audio device (mic/output).
fn selected_audio_id(st: &State) -> Option<String> {
    use slint::Model;
    let i = st.get_selected_device();
    if i < 0 {
        return None;
    }
    let row = st.get_devices().row_data(i as usize)?;
    let kind = row.kind.to_string();
    (kind == "mic" || kind == "output").then(|| row.id.to_string())
}

/// Repaint the SELECTED audio row's subtitle ("mic · 62%" / "output · muted") from the live
/// device-volume/mute state, so the channel strip in the list never contradicts the card after a
/// drag/mute (no full rescan needed — just the one row).
fn patch_selected_audio_detail(st: &State) {
    use slint::Model;
    let i = st.get_selected_device();
    if i < 0 {
        return;
    }
    let rows = st.get_devices();
    let Some(mut row) = rows.row_data(i as usize) else {
        return;
    };
    let kind = row.kind.to_string();
    if kind != "mic" && kind != "output" {
        return;
    }
    row.detail = if st.get_device_muted() {
        format!("{kind} \u{00b7} muted")
    } else {
        format!("{kind} \u{00b7} {}%", st.get_device_volume().round() as i32)
    }
    .into();
    rows.set_row_data(i as usize, row);
}

/// Apply selection to device row `idx`: set the kind (which gates the panel) + name, and seed the
/// per-kind controls — a mic/output loads its live volume + mute; a HID device points the runtime at
/// its pid and seeds FEEL + the perf/effects panels (the heavy re-reads only when the device CHANGED).
fn select_device_at(app: &AppWindow, sh: &SharedRt, idx: i32) {
    use slint::Model;
    let st = app.global::<State>();
    let rows = st.get_devices();
    let n = rows.row_count() as i32;
    if n == 0 {
        st.set_selected_device(-1);
        st.set_selected_device_kind("".into());
        st.set_selected_device_name("\u{2014}".into());
        st.set_sel_can_light(false); // nothing selected → LIGHTING shows its "no device" empty state
        return;
    }
    let i = idx.clamp(0, n - 1);
    let Some(row) = rows.row_data(i as usize) else {
        return;
    };
    st.set_selected_device(i);
    st.set_selected_device_name(row.name.clone());
    st.set_selected_device_kind(row.kind.clone());
    // surface the selected device's capabilities so the panel shows only controls it can do.
    st.set_sel_can_dpi(row.cap_dpi);
    st.set_sel_can_poll(row.cap_poll);
    st.set_sel_can_light(row.cap_light);
    st.set_sel_can_scroll(row.cap_scroll);
    st.set_sel_can_store(row.cap_store);
    st.set_sel_can_idle(row.cap_idle);
    let kind = row.kind.to_string();
    if kind == "mic" || kind == "output" {
        // load the SELECTED endpoint's live volume + mute (not the system default).
        let id = row.id.to_string();
        let flow = if kind == "mic" {
            neuron::audio::Flow::Capture
        } else {
            neuron::audio::Flow::Render
        };
        if let Some(e) = neuron::audio::endpoints(flow)
            .into_iter()
            .find(|e| e.id == id)
        {
            st.set_device_volume((e.volume * 100.0).round());
            st.set_device_muted(e.muted);
        }
    } else {
        // HID: point the runtime at this pid + seed the FEEL fader from its live reads.
        let pid = u16::from_str_radix(row.id.as_str(), 16).unwrap_or(0);
        let changed = {
            let mut s = sh.borrow_mut();
            let c = s.rt.selected_pid != pid;
            s.rt.selected_pid = pid;
            if c && s.rt.animating {
                s.rt.stop_animation();
            }
            c
        };
        if let Ok(v) = row.dpi.trim().parse::<f32>() {
            st.set_dpi(v);
        }
        if let Ok(v) = row
            .polling
            .trim()
            .trim_end_matches("Hz")
            .trim()
            .parse::<f32>()
        {
            st.set_polling_hz(v);
        }
        if let Ok(v) = row
            .brightness
            .trim()
            .trim_end_matches('%')
            .trim()
            .parse::<f32>()
        {
            st.set_brightness(v);
        }
        // the ADVANCED (stages/idle) + effects/grid re-reads are device round-trips — only on a switch.
        if changed {
            init_perf_controls(app, sh);
            st.set_selected_effect(-1);
            st.set_applied_effect(-1);
            refresh_effects(app, sh);
            init_grid(app, sh);
        }
    }
}

/// Extract a beacon-using macro's question(s) from its Python source — every `neuron.ask("…")` string
/// literal, " · "-joined. Returns None if the macro never calls `ask(` (a word-boundary check skips
/// `task(`/`mask(`/`my_ask(`); a macro that asks a COMPUTED question is reported "(question set at run
/// time)" so it still lists. Substring-based by design — fast, and a false hit only over-lists a row.
fn beacon_questions(src: &str) -> Option<String> {
    let mut questions: Vec<String> = Vec::new();
    let mut any = false;
    let mut from = 0;
    while let Some(rel) = src[from..].find("ask(") {
        let at = from + rel;
        from = at + 4;
        // a real call: the char before "ask" must not extend an identifier (skip t·ask, m·ask, my_ask).
        if src[..at]
            .chars()
            .last()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        any = true;
        // pull the FIRST string-literal argument, tolerating an f/r/b/u string prefix.
        let arg = src[from..]
            .trim_start()
            .trim_start_matches(['f', 'F', 'r', 'R', 'b', 'B', 'u', 'U']);
        let mut ch = arg.chars();
        if let Some(q @ ('"' | '\'')) = ch.next() {
            let body: String = ch.take_while(|&c| c != q).collect();
            if !body.is_empty() {
                questions.push(body);
            }
        }
    }
    if !any {
        None
    } else if questions.is_empty() {
        Some("(question set at run time)".into())
    } else {
        Some(questions.join("  ·  "))
    }
}

/// Rebuild the SYSTEM panel's BEACONS registry — every macro whose source calls `neuron.ask`, with the
/// question(s) it raises. Cheap (a handful of small files); run at startup and after a macro save.
pub fn refresh_beacon_macros(app: &AppWindow) {
    let rows: Vec<BeaconMacro> = neuron::macros::macro_host::scan_macro_dir()
        .into_iter()
        .filter_map(|(id, src)| {
            beacon_questions(&src).map(|asks| BeaconMacro {
                name: id.into(),
                asks: asks.into(),
            })
        })
        .collect();
    app.global::<State>()
        .set_beacon_macros(ModelRc::new(VecModel::from(rows)));
}

pub fn refresh_devices(app: &AppWindow, sh: &SharedRt) {
    // keep the selection on the SAME device across a rescan, by its id (pid-hex or audio endpoint id).
    let prev_id = {
        let st = app.global::<State>();
        use slint::Model;
        let i = st.get_selected_device();
        (i >= 0)
            .then(|| st.get_devices().row_data(i as usize))
            .flatten()
            .map(|r| r.id.to_string())
            .unwrap_or_default()
    };
    // 1) HID peripherals (registry-matched Razer devices), live-read.
    let devs = sh.borrow_mut().rt.scan_devices(); // also heals a stale selected_pid
    let mut rows: Vec<DeviceRow> = devs
        .iter()
        .map(|d| DeviceRow {
            name: d.name.clone().into(),
            codename: d.codename.clone().into(),
            pid: format!("{:04x}", d.pid).into(),
            mode: d.mode.clone().into(),
            connected: d.connected,
            firmware: d.firmware.clone().into(),
            dpi: d.dpi.clone().into(),
            polling: d.polling.clone().into(),
            brightness: d.brightness.clone().into(),
            battery: d.battery.clone().into(),
            battery_frac: d.battery_frac.unwrap_or(-1.0),
            charging: d.charging,
            storage: d.storage.clone().into(),
            icon: d.icon.into(),
            kind: d.icon.into(), // mouse/keyboard/device — the panel gate
            id: format!("{:04x}", d.pid).into(),
            detail: "".into(),
            cap_dpi: d.cap_dpi,
            cap_poll: d.cap_poll,
            cap_light: d.cap_light,
            cap_scroll: d.cap_scroll,
            cap_store: d.cap_store,
            cap_idle: d.cap_idle,
        })
        .collect();
    // 2) EMERGENT audio endpoints appended — mic + every output, generic over any hardware.
    rows.extend(audio_rows());
    let st = app.global::<State>();
    use slint::Model;
    st.set_devices(ModelRc::new(VecModel::from(rows)));
    // 3) restore selection by id (default to the first row), and seed its per-kind panel.
    let n = st.get_devices().row_count() as i32;
    let idx = if prev_id.is_empty() {
        0
    } else {
        (0..n)
            .find(|&i| {
                st.get_devices()
                    .row_data(i as usize)
                    .map(|r| r.id.to_string())
                    .as_deref()
                    == Some(prev_id.as_str())
            })
            .unwrap_or(0)
    };
    select_device_at(app, sh, idx);
}

pub fn refresh_rules(app: &AppWindow, sh: &SharedRt) {
    let (base_views, hyper_views) = sh.borrow().rt.rules();
    // toml/cast-sourced rows are read-only provenance; GUI-authored rows (gui.rules.toml) are the
    // removable tail. HyperShift-layered authored rules land in the hyper list (all removable).
    let mut base: Vec<RuleRow> = base_views
        .into_iter()
        .map(|r| RuleRow {
            trigger: r.trigger.into(),
            action: r.action.into(),
            layer: r.layer.into(),
            kind: r.kind.into(),
            removable: false,
        })
        .collect();
    let mut hyper: Vec<RuleRow> = hyper_views
        .into_iter()
        .map(|r| RuleRow {
            trigger: r.trigger.into(),
            action: r.action.into(),
            layer: r.layer.into(),
            kind: r.kind.into(),
            removable: false,
        })
        .collect();
    let gui = crate::editor::load_gui_rules();
    let mut editable_base = 0i32;
    let mut editable_hyper = 0i32;
    for r in &gui {
        let row = RuleRow {
            trigger: r.trigger.describe().into(),
            action: r.action.describe().into(),
            layer: if r.layer.is_some() {
                "hypershift"
            } else {
                "base"
            }
            .into(),
            kind: trigger_kind_str(&r.trigger).into(),
            removable: true,
        };
        if r.layer.is_some() {
            hyper.push(row);
            editable_hyper += 1;
        } else {
            base.push(row);
            editable_base += 1;
        }
    }
    let st = app.global::<State>();
    st.set_rules(ModelRc::new(VecModel::from(base)));
    st.set_hypershift_rules(ModelRc::new(VecModel::from(hyper)));
    st.set_editable_count(editable_base);
    st.set_editable_hyper_count(editable_hyper);
    // the node board renders the same spine — rebuild it in lockstep.
    refresh_graph(app, sh);
}

/// A pocket's emergent sigil as a Slint `Path` commands string, in normalized [-1,1] space (the
/// panel draws it under a matching viewbox). Empty for an empty pocket.
fn sigil_commands(path: &[(f32, f32)]) -> String {
    if path.len() < 2 {
        return String::new();
    }
    let mut s = String::with_capacity(path.len() * 16);
    for (i, &(x, y)) in path.iter().enumerate() {
        s.push_str(if i == 0 { "M " } else { " L " });
        s.push_str(&format!("{x:.4} {y:.4}"));
    }
    s.push_str(" Z");
    s
}

/// Rebuild the POCKETS strip from neuron-core: each portable clipboard with its emergent
/// content-sigil (drawn from the payload's bytes) + the legible summary + kind/durable flags.
pub fn refresh_pockets(app: &AppWindow) {
    use neuron::pocket::PocketKind;
    let cards: Vec<PocketCard> = neuron::pocket::views()
        .into_iter()
        .map(|(slot, durable, v)| {
            let filled = !v.is_empty();
            let sigil = if filled {
                sigil_commands(&neuron::pocket::sigil_of(&slot, 220).path)
            } else {
                String::new()
            };
            let kind = match v.kind {
                PocketKind::Text => "text",
                PocketKind::Files => "files",
                PocketKind::Image => "image",
                PocketKind::Other => "other",
                PocketKind::Empty => "empty",
            };
            PocketCard {
                name: slot.into(),
                summary: v.summary.into(),
                kind: kind.into(),
                sigil: sigil.into(),
                durable,
                filled,
            }
        })
        .collect();
    app.global::<State>()
        .set_pockets(ModelRc::new(VecModel::from(cards)));
}

/// Rebuild the pockets strip only when a pocket actually changed (a move bumps the core's
/// generation counter), so a big image payload is never re-hashed for a sigil every frame.
pub fn refresh_pockets_if_changed(app: &AppWindow) {
    thread_local! { static LAST: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) }; }
    let g = neuron::pocket::generation();
    let changed = LAST.with(|l| {
        if l.get() != g {
            l.set(g);
            true
        } else {
            false
        }
    });
    if changed {
        refresh_pockets(app);
    }
}

// ── the node board: the spine as a graph (layout persisted per node key) ────

/// Node-board canvas geometry (kept in sync with bindings.slint's GraphNodeEl).
const GNODE_W: f32 = 178.0;
const GNODE_H: f32 = 52.0;
const GBOARD_W: f32 = 1800.0;
const GBOARD_H: f32 = 1000.0;

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct GraphLayout {
    #[serde(default)]
    nodes: std::collections::BTreeMap<String, (f32, f32)>,
}

impl GraphLayout {
    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from("profiles").join("graph-layout.toml")
    }
    fn load() -> Self {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default()
    }
    fn save(&self) -> Result<(), String> {
        std::fs::create_dir_all("profiles").map_err(|e| e.to_string())?;
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(Self::path(), body).map_err(|e| e.to_string())
    }
}

/// Rebuild the node board from the same sources the rule lists render: read-only toml/cast rules
/// as fixed trigger→action wires, GUI-authored rules with their REAL Action objects — turbo rates
/// and macro chains (one node per step, the step's delay riding the wire to the next). Saved
/// layout positions override the default two-column flow.
pub fn refresh_graph(app: &AppWindow, sh: &SharedRt) {
    let layout = GraphLayout::load();
    let mut nodes: Vec<GraphNode> = Vec::new();
    let mut edges: Vec<GraphEdge> = Vec::new();
    let mut trigger_idx: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut row = 0usize;

    let place = |layout: &GraphLayout, key: &str, dx: f32, dy: f32| -> (f32, f32) {
        layout
            .nodes
            .get(key)
            .copied()
            .map(|(x, y)| {
                (
                    x.clamp(0.0, GBOARD_W - GNODE_W),
                    y.clamp(0.0, GBOARD_H - GNODE_H),
                )
            })
            .unwrap_or((dx, dy))
    };
    let row_y = |row: usize| 16.0 + row as f32 * 86.0;

    let add_trigger = |nodes: &mut Vec<GraphNode>,
                       trigger_idx: &mut std::collections::HashMap<String, usize>,
                       row: usize,
                       label: String,
                       sub: &str|
     -> usize {
        let key = format!("t|{label}|{sub}");
        if let Some(&i) = trigger_idx.get(&key) {
            return i;
        }
        let (x, y) = place(&layout, &key, 24.0, row_y(row));
        nodes.push(GraphNode {
            key: key.clone().into(),
            kind: "trigger".into(),
            label: label.into(),
            sub: sub.into(),
            x,
            y,
        });
        trigger_idx.insert(key, nodes.len() - 1);
        nodes.len() - 1
    };

    // 1) read-only rules (bindings.toml + cast wedges/glyphs) — fixed wires.
    let base_views = sh.borrow().rt.rules().0;
    for v in &base_views {
        let t = add_trigger(&mut nodes, &mut trigger_idx, row, v.trigger.clone(), v.kind);
        let akey = format!("a|{}|{}", v.trigger, v.action);
        let (x, y) = place(&layout, &akey, 24.0 + GNODE_W + 130.0, row_y(row));
        nodes.push(GraphNode {
            key: akey.into(),
            kind: "action".into(),
            label: v.action.clone().into(),
            sub: "toml".into(),
            x,
            y,
        });
        edges.push(GraphEdge {
            a: t as i32,
            b: (nodes.len() - 1) as i32,
            timing: "—".into(),
            tkind: "".into(),
            editable: false,
            rule: -1,
            step: -1,
        });
        row += 1;
    }

    // 2) GUI-authored rules — real Action objects, so timing is editable on the wire.
    for (gi, r) in crate::editor::load_gui_rules().iter().enumerate() {
        let hyper = r.layer.is_some();
        let t = add_trigger(
            &mut nodes,
            &mut trigger_idx,
            row,
            r.trigger.describe(),
            if hyper {
                "⇧ hypershift layer"
            } else {
                "yours"
            },
        );
        match &r.action {
            neuron::action::Action::Sequence { steps } => {
                // the macro CHAIN: one node per step, the source step's delay riding each wire.
                let mut prev = t;
                for (si, step) in steps.iter().enumerate() {
                    let key = format!("s|{gi}|{si}|{}", r.trigger.describe());
                    let (x, y) = place(
                        &layout,
                        &key,
                        24.0 + GNODE_W + 130.0 + si as f32 * (GNODE_W + 110.0),
                        row_y(row),
                    );
                    nodes.push(GraphNode {
                        key: key.into(),
                        kind: "step".into(),
                        label: step.action.describe().into(),
                        sub: if step.hold_ms > 0 {
                            format!("hold {}ms", step.hold_ms).into()
                        } else {
                            format!("step {}", si + 1).into()
                        },
                        x,
                        y,
                    });
                    let cur = nodes.len() - 1;
                    if si == 0 {
                        edges.push(GraphEdge {
                            a: prev as i32,
                            b: cur as i32,
                            timing: "start".into(),
                            tkind: "".into(),
                            editable: false,
                            rule: -1,
                            step: -1,
                        });
                    } else {
                        edges.push(GraphEdge {
                            a: prev as i32,
                            b: cur as i32,
                            timing: format!("{} ms", steps[si - 1].delay_ms).into(),
                            tkind: "delay".into(),
                            editable: true,
                            rule: gi as i32,
                            step: (si - 1) as i32,
                        });
                    }
                    prev = cur;
                }
            }
            neuron::action::Action::Turbo { action, cps } => {
                let key = format!("a|{gi}|turbo|{}", r.trigger.describe());
                let (x, y) = place(&layout, &key, 24.0 + GNODE_W + 130.0, row_y(row));
                nodes.push(GraphNode {
                    key: key.into(),
                    kind: "action".into(),
                    label: action.describe().into(),
                    sub: "autofire while held".into(),
                    x,
                    y,
                });
                edges.push(GraphEdge {
                    a: t as i32,
                    b: (nodes.len() - 1) as i32,
                    timing: format!("{cps} cps").into(),
                    tkind: "turbo".into(),
                    editable: true,
                    rule: gi as i32,
                    step: -1,
                });
            }
            other => {
                let key = format!("a|{gi}|{}", r.trigger.describe());
                let (x, y) = place(&layout, &key, 24.0 + GNODE_W + 130.0, row_y(row));
                nodes.push(GraphNode {
                    key: key.into(),
                    kind: "action".into(),
                    label: other.describe().into(),
                    sub: "yours".into(),
                    x,
                    y,
                });
                edges.push(GraphEdge {
                    a: t as i32,
                    b: (nodes.len() - 1) as i32,
                    timing: "instant".into(),
                    tkind: "".into(),
                    editable: false,
                    rule: -1,
                    step: -1,
                });
            }
        }
        row += 1;
    }

    let st = app.global::<State>();
    if st.get_selected_edge() >= edges.len() as i32 {
        st.set_selected_edge(-1);
    }
    st.set_graph_nodes(ModelRc::new(VecModel::from(nodes)));
    st.set_graph_edges(ModelRc::new(VecModel::from(edges)));
}

/// Map a Trigger to its short kind tag (for the rule list's left mark). Mirrors runtime::trigger_kind.
fn trigger_kind_str(t: &neuron::engine::Trigger) -> &'static str {
    use neuron::engine::Trigger;
    match t {
        Trigger::Input { .. } => "input",
        Trigger::Hotkey { .. } => "hotkey",
        Trigger::Gesture { .. } => "gesture",
        Trigger::RadialSector { .. } => "radial",
        Trigger::AppFocus { .. } => "app",
        Trigger::MicTap => "mic",
        Trigger::Hold { .. } => "hold",
        Trigger::Cast { .. } => "cast",
    }
}

pub fn refresh_profiles(app: &AppWindow, sh: &SharedRt) {
    let s = sh.borrow();
    let active = s.rt.active_profile.clone();
    let rows: Vec<ProfileRow> =
        s.rt.profiles
            .iter()
            .map(|p| {
                // the card's readout strip: real captured values, not one prose summary string.
                let dpi = if !p.dpi_stages.is_empty() {
                    p.dpi_stages
                        .iter()
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join("/")
                } else {
                    p.dpi.map(|d| d.to_string()).unwrap_or_default()
                };
                ProfileRow {
                    name: p.name.clone().into(),
                    summary: p.summary().into(),
                    dpi: dpi.into(),
                    polling: p
                        .polling_hz
                        .map(|h| format!("{h} Hz"))
                        .unwrap_or_default()
                        .into(),
                    lighting: p.lighting.clone().unwrap_or_default().into(),
                    active: p.name == active,
                }
            })
            .collect();
    let names: Vec<SharedString> =
        s.rt.profiles
            .iter()
            .map(|p| p.name.clone().into())
            .collect();
    let st = app.global::<State>();
    st.set_profiles(ModelRc::new(VecModel::from(rows)));
    st.set_profile_names(ModelRc::new(VecModel::from(names)));
}

pub fn refresh_app_rules(app: &AppWindow, sh: &SharedRt) {
    let s = sh.borrow();
    let rows: Vec<AppRuleRow> =
        s.rt.app_rules
            .rules
            .iter()
            .map(|r| AppRuleRow {
                app: r.app.clone().into(),
                profile: r.profile.clone().into(),
            })
            .collect();
    drop(s);
    app.global::<State>()
        .set_app_rules(ModelRc::new(VecModel::from(rows)));
    // the winning-rule lamp may have moved with the rule set.
    let focused = app.global::<State>().get_focused_app().to_string();
    note_focused_app(app, &focused);
}

/// Render the symbol label for a "N taps then hold" rhythm: ● per tap, ▬ for the closing hold.
fn rhythm_symbols(taps: u8) -> String {
    let mut out: Vec<&str> = vec!["●"; taps as usize];
    out.push("▬");
    out.join(" ")
}

/// Push the cast RHYTHM MAP rows into State: the bound rhythms from `cast.rhythm_actions` (taps>0)
/// PLUS the standard unbound rows (tap-hold / tap-tap-hold) so they're always there to bind. The
/// weave's own plain hold (taps=0) is NOT a row — it owns the dedicated rhythm recorder above.
pub fn refresh_rhythms(app: &AppWindow, sh: &SharedRt) {
    let s = sh.borrow();
    // bound rhythms first (in tap order), then any of the standard slots (1, 2) not already bound.
    let mut taps_seen: Vec<u8> = s
        .rt
        .cast
        .rhythm_actions
        .iter()
        .filter(|rb| rb.taps > 0 && rb.action != neuron::action::Action::Noop)
        .map(|rb| rb.taps)
        .collect();
    taps_seen.sort_unstable();
    taps_seen.dedup();
    let mut order = taps_seen.clone();
    for std_taps in [1u8, 2u8] {
        if !order.contains(&std_taps) {
            order.push(std_taps);
        }
    }
    let rows: Vec<RhythmBindRow> = order
        .iter()
        .map(|&taps| {
            let action = s
                .rt
                .cast
                .rhythm_actions
                .iter()
                .find(|rb| rb.taps == taps && rb.action != neuron::action::Action::Noop)
                .map(|rb| rb.action.describe());
            RhythmBindRow {
                taps: taps as i32,
                label: rhythm_symbols(taps).into(),
                desc: action.clone().unwrap_or_default().into(),
                bound: action.is_some(),
            }
        })
        .collect();
    app.global::<State>()
        .set_rhythm_binds(ModelRc::new(VecModel::from(rows)));
}

pub fn refresh_gestures(app: &AppWindow, sh: &SharedRt) {
    let s = sh.borrow();
    let chips: Vec<GlyphChip> =
        s.rt.vault
            .templates
            .iter()
            .map(|t| {
                let action = s.rt.cast.gestures.get(&t.name).map(|a| a.describe());
                GlyphChip {
                    name: t.name.clone().into(),
                    action: action.clone().unwrap_or_default().into(),
                    bound: action.is_some(),
                }
            })
            .collect();
    app.global::<State>()
        .set_glyphs(ModelRc::new(VecModel::from(chips)));
}

pub fn refresh_radial(app: &AppWindow, sh: &SharedRt) {
    let s = sh.borrow();
    let n = s.rt.cast.sectors.max(1);
    let st = app.global::<State>();
    // the editor authors EITHER the base radial or the HyperShift one — the flag retargets the view.
    let hyper = st.get_radial_edit_hyper();
    let set = if hyper {
        &s.rt.cast.hyper_radial
    } else {
        &s.rt.cast.radial
    };
    let rows: Vec<RadialSector> = (0..n)
        .map(|i| {
            let label = neuron::radial::compass(i, n);
            let action = set
                .get(i)
                .map(|a| a.describe())
                .unwrap_or_else(|| "—".into());
            RadialSector {
                label: label.into(),
                action: action.into(),
            }
        })
        .collect();
    st.set_radial_sectors(n as i32);
    // the wedge cap is DERIVED (arc at the commit radius vs hand jitter), never hand-decreed.
    st.set_radial_max(neuron::radial::max_sectors(s.rt.cast.deadzone) as i32);
    st.set_weave_assist(s.rt.cast.assist as f32);
    st.set_hyper_radial_on(s.rt.cast.hyper_radial_on);
    st.set_radial_items(ModelRc::new(VecModel::from(rows)));
    // a stale preview/edit index past the new count self-heals.
    if st.get_radial_preview_sector() >= n as i32 {
        st.set_radial_preview_sector(-1);
    }
    if st.get_editing_sector() >= n as i32 {
        st.set_editing_sector(-1);
    }
}

pub fn refresh_effects(app: &AppWindow, sh: &SharedRt) {
    let effs = sh.borrow().rt.effects();
    let rows: Vec<EffectRow> = effs
        .iter()
        .enumerate()
        .map(|(i, (name, native, uses_color))| EffectRow {
            idx: i as i32,
            name: name.clone().into(),
            native: *native,
            uses_color: *uses_color,
        })
        .collect();
    let fw: Vec<EffectRow> = rows.iter().filter(|e| e.native).cloned().collect();
    let sw: Vec<EffectRow> = rows.iter().filter(|e| !e.native).cloned().collect();
    let st = app.global::<State>();
    st.set_effects(ModelRc::new(VecModel::from(rows)));
    st.set_effects_fw(ModelRc::new(VecModel::from(fw)));
    st.set_effects_sw(ModelRc::new(VecModel::from(sw)));
}

/// (uses_color, directional) for an effect generator — drives which per-layer controls the editor
/// shows (a spectrum/fire ignores colour; only a wave cares about direction).
fn effect_meta(name: &str) -> (bool, bool) {
    match name {
        // these read the layer colour: solid fill, breath, twinkles, key-glow, the VU bars
        "static" | "solid" | "breathing" | "starlight" | "reactive" | "audiometer" => (true, false),
        "wave" => (false, true),
        _ => (false, false), // spectrum, colorwheel, fire — own hue / own ramp
    }
}

/// Project the Rust-side layer stack into the Slint `light-layers` display model + selected index.
pub fn refresh_layers(app: &AppWindow, sh: &SharedRt) {
    let (defs, sel) = {
        let s = sh.borrow();
        (s.light_layers.clone(), s.selected_layer)
    };
    let rows: Vec<LayerRow> = defs
        .iter()
        .map(|d| {
            let (uses_color, directional) = effect_meta(&d.effect);
            LayerRow {
                effect: d.effect.clone().into(),
                color: d.color.to_hex().into(),
                col: rgb_to_color(d.color),
                speed: d.speed,
                direction: d.direction as i32,
                blend: d.blend.as_str().into(),
                uses_color,
                directional,
                enabled: d.enabled,
                region_count: d.region.len() as i32,
            }
        })
        .collect();
    let st = app.global::<State>();
    let sel = if defs.is_empty() {
        -1
    } else {
        sel.min(defs.len() - 1) as i32
    };
    st.set_light_layers(ModelRc::new(VecModel::from(rows)));
    st.set_selected_layer(sel);
}

pub fn init_grid(app: &AppWindow, sh: &SharedRt) {
    let (rows, cols) = sh.borrow().rt.grid_dims();
    let kind = sh.borrow().rt.grid_kind();
    let n = rows as usize * cols as usize;
    let px: Vec<slint::Color> = vec![GRID_OFF; n];
    let st = app.global::<State>();
    st.set_grid_rows(rows as i32);
    st.set_grid_cols(cols as i32);
    st.set_grid_kind(kind.into());
    st.set_grid_px(ModelRc::new(VecModel::from(px)));
}

/// Render an activation phrase as instrument symbols for the rhythm readout:
/// ● tap · ▬ hold · ⇄ appended to a pure-tap (toggle) phrase.
fn phrase_symbols(pattern: &str) -> String {
    let phrase =
        neuron::feel::Phrase::parse(pattern).unwrap_or_else(|_| neuron::feel::Phrase::hold());
    let mut out: Vec<&str> = phrase
        .0
        .iter()
        .map(|s| match s {
            neuron::feel::Sym::Tap => "●",
            neuron::feel::Sym::Hold => "▬",
        })
        .collect();
    if !phrase.ends_in_hold() {
        out.push("⇄");
    }
    out.join(" ")
}

/// Push an activation pattern into both view properties (the raw string + the symbol readout).
fn sync_activation_view(st: &State, pattern: &str) {
    st.set_activation_pattern(pattern.into());
    st.set_activation_display(phrase_symbols(pattern).into());
}

/// The ONE weave capture path — used by BOTH the glyph recorder and the radial preview, so the
/// overlay behaves byte-for-byte identically for either facet of spellweaving (the only difference
/// is the [`crate::overlay::WeaveMode`] passed in). Waits for the configured activation RHYTHM,
/// spawns the live sigil, streams the accumulated stroke, flares/fizzles on release per
/// `recognized`, and returns the captured path. The sigil's fade-out is DETACHED so the return is
/// immediate — spamming weaves re-arms within one poll tick, never gated on an animation.
/// Blocking — call it on a worker thread.
fn weave_capture(
    trigger: i32,
    phrase: &neuron::feel::Phrase,
    feel: &neuron::feel::FeelConfig,
    mode: crate::overlay::WeaveMode,
    mut on_pts: impl FnMut(&[neuron::glyph::C]),
    recognized: impl Fn(&[neuron::glyph::C]) -> bool,
) -> Vec<neuron::glyph::C> {
    // the editor owns the trigger for this weave: any pending beacon stands down until we're done
    // (one press must never feed both this capture and an ask's answer wheel).
    let _editor = crate::beacon::EditorWeave::engage();
    let overlay = crate::overlay::SpellOverlay::spawn();
    overlay.begin(mode);
    let path = neuron::glyph::capture_phrase(trigger, phrase, feel, 600, |pts| {
        let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
        overlay.push(&rel);
        on_pts(pts);
    });
    overlay.recognized(recognized(&path));
    overlay.end();
    // let the sigil fade on its own time — the capture (and the user's next weave) doesn't wait.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(350));
        drop(overlay);
    });
    path
}

/// First free auto-name for a new glyph. After deletes, `len+1` can collide with a survivor and
/// upsert would silently OVERWRITE that template — scan for the first unused index instead.
fn free_glyph_name(vault: &neuron::gesture::Vault) -> String {
    (1..)
        .map(|i| format!("glyph_{i}"))
        .find(|n| !vault.templates.iter().any(|t| &t.name == n))
        .expect("unbounded range yields a free name")
}

/// Capture a gesture on a worker thread (blocking hold-to-draw), analyze it, store it, and post
/// the result back to the UI.
fn record_gesture(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    st.set_capturing_gesture(true);
    let (trigger, phrase) = {
        let s = sh.borrow();
        (s.rt.cast.trigger, s.rt.cast.phrase())
    };
    // the prompt names the user's ACTUAL trigger + rhythm and the way out.
    st.set_gesture_status(
        format!(
            "{} {} and draw — ESC cancels",
            if phrase == neuron::feel::Phrase::hold() {
                "hold".to_string()
            } else {
                format!("[{}] on", phrase.describe())
            },
            st.get_cast_trigger_label()
        )
        .into(),
    );
    let cfg = sh.borrow().rt.vault.config;
    let next_name = free_glyph_name(&sh.borrow().rt.vault);
    // a snapshot of the vault rides along for LIVE AUTOPREDICT: while the stroke forms, the
    // closest spell is named in the status line ("≈ circle_cw · 74%") — the engine thinking
    // out loud, every ~80ms, off the UI thread.
    let vault = sh.borrow().rt.vault.clone();
    let w = app.as_weak();
    let predict_w = app.as_weak();
    std::thread::spawn(move || {
        let feel = neuron::feel::FeelConfig::load();
        let mut last_pred_len = 0usize;
        let path = weave_capture(
            trigger,
            &phrase,
            &feel,
            crate::overlay::WeaveMode::Glyph { hint: None },
            |pts| {
                // throttle: every ~14 new points (≈80ms of motion) stream the LIVE TRAIL onto
                // the panel canvas and re-run the autopredict — the panel is the instrument
                // (the stroke forms on it as you draw), not a replay after the fact.
                if pts.len() >= 4 && pts.len() - last_pred_len >= 14 {
                    last_pred_len = pts.len();
                    let trail = normalize_trail(pts);
                    let predicted = if !vault.templates.is_empty() && pts.len() >= 8 {
                        let word = neuron::glyph::analyze(pts, &vault.config);
                        vault.predict(&word).map(|(name, score, _)| {
                            let conf = if vault.config.threshold > 0.0 {
                                ((1.0 - score / (vault.config.threshold * 2.0)).clamp(0.0, 0.99)
                                    * 100.0) as i32
                            } else {
                                0
                            };
                            (name, conf)
                        })
                    } else {
                        None
                    };
                    let ui = predict_w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = ui.upgrade() {
                            let st = app.global::<State>();
                            st.set_trail(ModelRc::new(VecModel::from(trail)));
                            if let Some((name, conf)) = predicted {
                                // the engine thinking out loud: name it AND light its chip.
                                st.set_gesture_status(format!("≈ {name} · {conf}%").into());
                                st.set_gesture_predict(name.into());
                            }
                        }
                    });
                }
            },
            |p| p.len() >= 3,
        );
        let captured = path.len();

        let word = neuron::glyph::analyze(&path, &cfg);
        // the drawable exemplar (centered unit polyline) rides with the template, so the live
        // weave can ghost this exact shape as you draw toward it.
        let exemplar = neuron::glyph::exemplar_path(&path, &cfg);
        // build a normalized trail for the visualizer
        let trail = normalize_trail(&path);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                st.set_capturing_gesture(false);
                st.set_gesture_predict("".into()); // the stroke is over — nothing is "becoming"
                if captured == 0 {
                    // a cancel is a cancel — not a user failure.
                    st.set_gesture_status("cancelled".into());
                    return;
                }
                if captured < 3 {
                    st.set_gesture_status("too short — try again".into());
                    return;
                }
                with_shared(|sh| {
                    {
                        let mut s = sh.borrow_mut();
                        s.rt.vault.upsert_with(&next_name, word, exemplar);
                        let _ = s.rt.vault.save();
                    }
                    refresh_gestures(&app, sh);
                });
                st.set_trail(ModelRc::new(VecModel::from(trail)));
                st.set_gesture_status(
                    format!("recorded '{next_name}' ({captured} pts) — click its chip to bind it")
                        .into(),
                );
            }
        });
    });
}

/// Test the wheel: hold the cast trigger and flick. Goes through the EXACT same [`weave_capture`]
/// path as glyph recording — only the overlay mode differs (the sector wheel instead of the rune
/// ring) — so the radial flick and the rich glyph really are one system. Feedback lands in
/// `radial-status`, beside the wheel the user is looking at.
fn preview_radial(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    st.set_capturing_gesture(true);
    st.set_editing_sector(-1); // a flick is navigation, not an edit continuation
    let (trigger, sectors, deadzone, phrase, widgets) = {
        let s = sh.borrow();
        let sectors = s.rt.cast.sectors.max(1);
        // the same live instrument cards the runtime wheel shows (icons + values + tone)
        #[cfg(windows)]
        let widgets = crate::beacon::radial_widgets(&s.rt.cast);
        #[cfg(not(windows))]
        let widgets: Vec<crate::overlay::WedgeView> = Vec::new();
        (
            s.rt.cast.trigger,
            sectors,
            s.rt.cast.deadzone,
            s.rt.cast.phrase(),
            widgets,
        )
    };
    st.set_radial_status(
        format!(
            "{} {} and flick a direction — ESC cancels",
            if phrase == neuron::feel::Phrase::hold() {
                "hold".to_string()
            } else {
                format!("[{}] on", phrase.describe())
            },
            st.get_cast_trigger_label()
        )
        .into(),
    );
    let menu = neuron::radial::RadialMenu {
        sectors,
        deadzone,
        items: Vec::new(),
    };
    let w = app.as_weak();
    std::thread::spawn(move || {
        let feel = neuron::feel::FeelConfig::load();
        let menu_for_hit = menu.clone();
        let path = weave_capture(
            trigger,
            &phrase,
            &feel,
            crate::overlay::WeaveMode::Radial {
                sectors: sectors as u8,
                widgets,
                fans: vec![],
            },
            |_| {},
            move |p| menu_for_hit.select(p).is_some(),
        );
        let pick = menu.select(&path);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                st.set_capturing_gesture(false);
                match pick {
                    Some(s) => {
                        st.set_radial_preview_sector(s as i32);
                        let name = neuron::radial::compass(s, sectors);
                        st.set_radial_status(format!("flick → wedge {s} ({name})").into());
                    }
                    None => {
                        st.set_radial_preview_sector(-1);
                        st.set_radial_status(
                            "flick too short — move further from center, then release".into(),
                        );
                    }
                }
            }
        });
    });
}

/// Map a captured complex path into normalized interleaved (x,y) in 0..1 for the trail canvas,
/// decimated to ≤128 points (a 600-dot stroke is hundreds of alpha-blended elements in the
/// software renderer for information that saturates around a hundred).
fn normalize_trail(path: &[neuron::glyph::C]) -> Vec<f32> {
    if path.is_empty() {
        return Vec::new();
    }
    let (mut minx, mut maxx, mut miny, mut maxy) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for c in path {
        minx = minx.min(c.re);
        maxx = maxx.max(c.re);
        miny = miny.min(c.im);
        maxy = maxy.max(c.im);
    }
    let sx = (maxx - minx).max(1.0);
    let sy = (maxy - miny).max(1.0);
    let s = sx.max(sy);
    const MAX_DOTS: usize = 128;
    let step = path.len().div_ceil(MAX_DOTS);
    let mut out = Vec::with_capacity(MAX_DOTS * 2 + 2);
    for (i, c) in path.iter().enumerate() {
        // keep the stride plus the LAST point, so the head (the brightest dot) stays anchored.
        if i % step != 0 && i != path.len() - 1 {
            continue;
        }
        out.push((((c.re - minx) / s) as f32).clamp(0.0, 1.0));
        out.push((((c.im - miny) / s) as f32).clamp(0.0, 1.0));
    }
    out
}

fn open_config_dir() {
    let dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let _ = std::process::Command::new("explorer").arg(dir).spawn();
}

/// Format an uptime in ms as a compact human readout: "3d 04h", "1h 23m", "12m 03s", "45s".
fn fmt_uptime(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m, sec) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60, s % 60);
    if d > 0 {
        format!("{d}d {h:02}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {sec:02}s")
    } else {
        format!("{sec}s")
    }
}

/// Push the flight recorder's live state into the RELIABILITY panel — uptime, per-organ heartbeats,
/// and the on-disk crash record. Called ~1s from the main heartbeat tick (cheap: a few relaxed
/// atomic reads), so a stalled organ / a fresh crash shows the instant it happens.
pub fn refresh_reliability(app: &AppWindow) {
    let respawned = std::env::args().any(|a| a == "--respawned");
    let st = app.global::<State>();
    st.set_uptime(fmt_uptime(crate::flight::uptime_ms()).into());
    let rows: Vec<OrganRow> = crate::flight::organ_status()
        .into_iter()
        .map(|(name, age, stalled)| {
            let (state, detail) = match age {
                None => ("rest", "at rest".to_string()),
                Some(_) if stalled => ("stall", "SILENT — wedged or dead".to_string()),
                Some(a) if a < 1000 => ("beat", format!("beating · {a} ms ago")),
                Some(a) => ("beat", format!("beating · {}s ago", a / 1000)),
            };
            OrganRow {
                name: name.into(),
                state: state.into(),
                detail: detail.into(),
            }
        })
        .collect();
    st.set_organs(ModelRc::new(VecModel::from(rows)));
    let dumps = crate::flight::crash_dump_count();
    let (summary, present) = if respawned {
        (
            "recovered from a crash this launch — the story is in neuron-crash.log".to_string(),
            true,
        )
    } else if dumps > 0 {
        (
            format!("{dumps} crash/stall dump(s) on record in neuron-crash.log"),
            true,
        )
    } else {
        (
            "no crashes recorded — the recorder is armed and quiet".to_string(),
            false,
        )
    };
    st.set_crash_summary(summary.into());
    st.set_crash_present(present);
}

fn open_crash_log() {
    // ensure the file exists so "open" never opens nothing — a fresh dump is a useful first entry.
    crate::flight::dump_to_crash_log("opened from the reliability panel");
    let path = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join("neuron-crash.log");
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}
