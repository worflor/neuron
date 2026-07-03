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
    AppRuleRow, AppWindow, BeaconMacro, DeviceRow, DiagRow, EffectParam, EffectRow, EffectTile,
    GlyphChip, ImportLine, KnobRow, MacroBlock, MacroCard, MaterialCard, OrganRow,
    PingKind, PocketCard, ProfileRow, RadialSector, RhythmBindRow, RuleRow, SpectrumFrame,
    SpectrumStop,
    State, Theme,
};
use neuron::macros::{value_to_source, MacroNode, Value};
use neuron::import::Imported;
use neuron::lighting::Rgb;
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The "off" colour of an LED cell — what `clear` paints and what an unpainted grid shows.
const GRID_OFF: slint::Color = slint::Color::from_rgb_u8(0x0c, 0x0d, 0x10);

/// Whether the lighting-page render profiler is on (env `NEURON_PROF`). Cached once — env reads lock
/// an internal mutex, and the tile tick is hot. INERT in prod (the env var is unset), so the per-tick
/// `Instant` calls below are skipped entirely; this is pure diagnostics, gone the moment the var is.
fn tile_prof_on() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NEURON_PROF").is_some())
}

/// Rolling 1 Hz accumulator for the tile-render tick — only ever touched when `NEURON_PROF` is set.
/// Per-tile gen + preview-convert microseconds, the model-upload cost, and the whole-tick total,
/// summed across a second then printed to stderr as per-tick AVERAGES (so the log is one line/sec,
/// not a flood). This is how Part B reports the REAL page cost the data indicts.
struct TileProfAcc {
    ticks: u32,
    tick_total_us: f64,
    upload_us: f64,
    gen: std::collections::HashMap<&'static str, f64>,
    prev: std::collections::HashMap<&'static str, f64>,
    last_print: std::time::Instant,
}
impl TileProfAcc {
    fn new() -> Self {
        TileProfAcc {
            ticks: 0,
            tick_total_us: 0.0,
            upload_us: 0.0,
            gen: std::collections::HashMap::new(),
            prev: std::collections::HashMap::new(),
            last_print: std::time::Instant::now(),
        }
    }
    fn flush(&mut self) {
        let secs = self.last_print.elapsed().as_secs_f64().max(1e-6);
        let n = self.ticks.max(1) as f64;
        let gen_total: f64 = self.gen.values().sum::<f64>() / n;
        eprintln!(
            "PROF: tick_total={:.3}ms tiles={} rate≈{:.1}Hz gen_total={:.3}ms preview_total={:.3}ms upload={:.3}ms",
            self.tick_total_us / n / 1000.0,
            self.gen.len(),
            self.ticks as f64 / secs,
            gen_total / 1000.0,
            self.prev.values().sum::<f64>() / n / 1000.0,
            self.upload_us / n / 1000.0,
        );
        let mut rows: Vec<(&'static str, f64, f64)> = self
            .gen
            .iter()
            .map(|(&k, &g)| (k, g / n, self.prev.get(k).copied().unwrap_or(0.0) / n))
            .collect();
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        for (slug, g, p) in rows {
            eprintln!("PROF:   tile={slug:<12} gen_us={g:>8.1} preview_us={p:>6.1}");
        }
        *self = TileProfAcc::new();
    }
}

/// Fold one tile-tick's measurements into the rolling accumulator, flushing once per second. Called
/// only under `NEURON_PROF`.
fn tile_prof_accumulate(tick_us: f64, upload_us: f64, rows: &[(&'static str, f64, f64)]) {
    thread_local! {
        static ACC: RefCell<TileProfAcc> = RefCell::new(TileProfAcc::new());
    }
    ACC.with(|a| {
        let mut a = a.borrow_mut();
        a.ticks += 1;
        a.tick_total_us += tick_us;
        a.upload_us += upload_us;
        for &(slug, gen_us, prev_us) in rows {
            *a.gen.entry(slug).or_insert(0.0) += gen_us;
            *a.prev.entry(slug).or_insert(0.0) += prev_us;
        }
        if a.last_print.elapsed() >= std::time::Duration::from_secs(1) {
            a.flush();
        }
    });
}
const DPI_MIN: u16 = 100;
const DPI_MAX: u16 = 30_000;
const DPI_STAGE_CAPACITY: usize = 5;
const IDLE_MIN_SECS: u32 = 0;
const IDLE_MAX_SECS: u32 = u16::MAX as u32;

/// Shared GUI state: the runtime plus the parsed-but-unapplied import (held between Parse/Apply).
pub struct Shared {
    pub rt: AppRuntime,
    pub pending_import: Option<Imported>,
    /// The lighting COMPOSITOR stack — the Rust-side source of truth (regions live here). Each layer is
    /// a PATTERN (shape) × SPECTRUM (colour). The unified page + the spectrum editor are projections of
    /// this. `layers_rev` bumps on any change so the live preview rebuilds its cached `Compositor`
    /// (stateful patterns like fire keep their state across ticks otherwise).
    pub light_layers: Vec<neuron::pattern::LayerDef>,
    pub selected_layer: usize,
    /// Which FRAME of the selected layer's spectrum the editor is shaping (the timeline scrubber). 0
    /// for the common single-frame (non-sequenced) case; clamped to the active spectrum's length.
    pub active_frame: usize,
    pub layers_rev: u64,
    /// The macro CONSTRUCTOR's working tree — the source of truth behind the blocks canvas. Every
    /// canvas edit (edit-step / add-step / delete-step / move-step) mutates THIS, then Rust regenerates
    /// `macro-source` (via `nodes_to_source`) and re-flattens it into `macro-blocks`. A successful
    /// code-view parse reseeds it. Held here so it survives across callbacks (it IS the macro's shape).
    pub macro_tree: Vec<MacroNode>,
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

    /// Live channel meters (mic + out) + their ballistics — opened only while the DIRECT page shows them.
    static AUDIO_METERS: RefCell<Option<AudioMeters>> = const { RefCell::new(None) };
}

/// One channel's meter ballistics — DaVinci-style: instant attack, smooth release, a falling peak-hold
/// tick, and a ~2s clip latch. Fed a 0..1 LINEAR peak each ~30fps poll; exposes a dB-mapped display
/// level so the bar lives in the useful range instead of dead-then-slammed.
#[derive(Default)]
struct MeterChan {
    level: f32,
    hold: f32,
    hold_age: u32,
    clip_age: u32,
}
impl MeterChan {
    fn update(&mut self, raw: f32) {
        let disp = if raw <= 1e-4 {
            0.0
        } else {
            ((20.0 * raw.log10() + 60.0) / 60.0).clamp(0.0, 1.0) // -60dB..0dB → 0..1
        };
        if disp >= self.level {
            self.level = disp; // instant attack
        } else {
            self.level += (disp - self.level) * 0.30; // ~150ms release at 30fps
        }
        if disp >= self.hold {
            self.hold = disp;
            self.hold_age = 0;
        } else {
            self.hold_age += 1;
            if self.hold_age > 30 {
                self.hold += (disp - self.hold) * 0.08; // ~1s hold, then fall
            }
        }
        if raw >= 0.99 {
            self.clip_age = 0;
        } else {
            self.clip_age = self.clip_age.saturating_add(1);
        }
    }
    fn clip(&self) -> bool {
        self.clip_age < 60 // hold the clip flag ~2s after the last near-0dBFS hit
    }
}

/// The open meter handles + ballistics, live only while the DIRECT page is showing.
#[derive(Default)]
struct AudioMeters {
    mic: Option<neuron::audio::MeterCtl>,
    out: Option<neuron::audio::MeterCtl>,
    cm: MeterChan,
    co: MeterChan,
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

/// One gallery frame's INPUTS, resolved on the UI thread: each surface's
/// render-ready material — the LIVE material for the picked surface (so knob
/// edits show the instant they land) and the accent-tinted preset for the
/// rest. `Material` is `Copy`; the render worker receives values, never a
/// reference into UI state.
fn material_snapshot() -> Vec<crate::weave::Material> {
    use crate::weave::Surface;
    let accent = weave_accent_u32();
    let acc = (
        ((accent >> 16) & 0xFF) as f32 / 255.0,
        ((accent >> 8) & 0xFF) as f32 / 255.0,
        (accent & 0xFF) as f32 / 255.0,
    );
    let live = crate::weave::live_material();
    Surface::ALL
        .into_iter()
        .map(|s| if s == live.surface { live } else { crate::weave::preset(s).with_accent(acc) })
        .collect()
}

/// Render every gallery swatch — the HEAVY half, thread-agnostic (per-tick it
/// runs on the gallery worker; the sync init/accent path calls it inline).
fn render_material_bufs(
    mats: &[crate::weave::Material],
    t: f32,
) -> Vec<SharedPixelBuffer<Rgba8Pixel>> {
    // HALF-TIME for the gallery: the swatches ran the shaders on the raw cast clock and read as
    // agitated at tile size (the cast overlay keeps its own true 60fps clock — this only slows the
    // previews). Half speed also halves the per-tick motion, so the ~16fps gallery reads smoother.
    let t = t * 0.5;
    // HQ: render ABOVE the display size (tiles ~168px) so the swatch downsamples crisp, not blurry.
    let (w, h) = (220usize, 132usize); // 5:3 aspect
    mats.iter()
        .map(|m| {
            let rgba = crate::weave::material_preview_rgba(m, w, h, t);
            let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
            buf.make_mut_bytes().copy_from_slice(&rgba);
            buf
        })
        .collect()
}

/// Upload rendered swatches into the gallery rows IN PLACE (UI thread). Row
/// writes (not model swaps) keep the cards — TouchAreas, hover — alive.
///
/// A/B CROSSFADE: each card carries two swatch slots. A fresh frame lands in
/// whichever slot is HIDDEN, then `material-show-alt` flips once for the
/// batch and the tiles GPU-fade to it over exactly one tick period — the
/// worker's ~10fps render reads as continuous display-rate motion. A rebuilt
/// model (first paint, card-count change) seeds BOTH slots with the same
/// frame so there's never a fade-from-nothing pop.
fn upload_material_cards(app: &AppWindow, bufs: Vec<SharedPixelBuffer<Rgba8Pixel>>) {
    use crate::weave::Surface;
    let state = app.global::<State>();
    let existing = state.get_material_cards();
    let n = bufs.len();
    match existing.as_any().downcast_ref::<VecModel<MaterialCard>>() {
        Some(vm) if vm.row_count() == n => {
            let showing_alt = state.get_material_show_alt();
            for (i, buf) in bufs.into_iter().enumerate() {
                if let Some(mut row) = vm.row_data(i) {
                    let img = Image::from_rgba8(buf);
                    // write the slot the user is NOT looking at
                    if showing_alt {
                        row.swatch = img;
                    } else {
                        row.swatch_alt = img;
                    }
                    vm.set_row_data(i, row);
                }
            }
            // one flip for the whole batch → every tile fades in step
            state.set_material_show_alt(!showing_alt);
        }
        _ => {
            let cards: Vec<MaterialCard> = Surface::ALL
                .into_iter()
                .zip(bufs)
                .map(|(s, buf)| {
                    let img = Image::from_rgba8(buf);
                    MaterialCard {
                        name: s.name().into(),
                        slug: s.slug().into(),
                        blurb: s.blurb().into(),
                        swatch: img.clone(),
                        swatch_alt: img,
                    }
                })
                .collect();
            state.set_material_cards(ModelRc::new(VecModel::from(cards)));
        }
    }
}

/// Synchronous gallery render — the two rare paths that want the swatches NOW
/// (startup seeding, an accent pick's instant retint). The per-tick animation
/// goes through [`schedule_material_cards`] instead.
fn render_material_cards(app: &AppWindow, t: f32) {
    upload_material_cards(app, render_material_bufs(&material_snapshot(), t));
}

/// The per-tick gallery path: snapshot the materials on the UI thread, render
/// OFF it. Six full-fidelity per-pixel shader tiles per tick was the SYSTEM
/// page's jank — the UI thread now pays only the row upload (a texture swap),
/// and the one worker self-throttles: the channel holds ONE job, so if a
/// render is still in flight the tick is simply dropped (no queue, no drift —
/// the next tick carries a fresher `t` anyway).
fn schedule_material_cards(app: &AppWindow) {
    use std::sync::mpsc::{sync_channel, SyncSender};
    use std::sync::OnceLock;
    static TX: OnceLock<SyncSender<(Vec<crate::weave::Material>, f32)>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let weak = app.as_weak();
        let (tx, rx) = sync_channel::<(Vec<crate::weave::Material>, f32)>(1);
        std::thread::Builder::new()
            .name("neuron-weave-gallery".into())
            .spawn(move || {
                while let Ok((mats, t)) = rx.recv() {
                    let bufs = render_material_bufs(&mats, t);
                    let _ = weak.upgrade_in_event_loop(move |app| {
                        upload_material_cards(&app, bufs);
                    });
                }
            })
            .expect("spawn weave-gallery worker");
        tx
    });
    let _ = tx.try_send((material_snapshot(), crate::weave::seconds()));
}

/// The WHAT-EARNS-A-PING card copy — (icon, label, blurb) per confirmation kind. With
/// [`ping_rank`], the ONLY hand-authored parts of the notifications grid, and both are EXHAUSTIVE
/// matches over `confirm::Kind`: adding a kind without a card is a compile error, never a silently
/// missing tile. The icon usually equals the kind's slug; beacon wears its own glyph over the
/// "macro" gate.
fn ping_copy(k: neuron::confirm::Kind) -> (&'static str, &'static str, &'static str) {
    use neuron::confirm::Kind;
    match k {
        Kind::Dpi => ("dpi", "dpi", "pointer DPI"),
        Kind::Sniper => ("sniper", "sniper", "hold-to-precision"),
        Kind::Scroll => ("scroll", "sensitivity", "scroll modes"),
        Kind::Polling => ("polling", "polling", "report rate"),
        Kind::Brightness => ("brightness", "brightness", "LED level"),
        Kind::Battery => ("battery", "battery", "charge"),
        Kind::SidePlate => ("side_plate", "side plate", "swap detect"),
        Kind::Profile => ("profile", "profile", "active"),
        Kind::Layer => ("layer", "layer", "HyperShift"),
        Kind::Macro => ("beacon", "beacon", "macro notify"),
    }
}

/// The ping grid's DOMAIN + order-within-domain — (domain 0..3, rank). Domains render as captioned
/// full-width rows (POINTER · HARDWARE · SESSION), each row exactly its domain's members stretched
/// to the width — so the layout structurally CANNOT grow a dangling odd card: a new kind joins its
/// domain's row and that row just gets denser. Exhaustive: a new kind MUST pick its slot here or
/// the build fails.
fn ping_slot(k: neuron::confirm::Kind) -> (u8, u8) {
    use neuron::confirm::Kind;
    match k {
        // domain 0 — POINTER (aim & report)
        Kind::Dpi => (0, 0),
        Kind::Sniper => (0, 1),
        Kind::Scroll => (0, 2),
        Kind::Polling => (0, 3),
        // domain 1 — HARDWARE (the physical device)
        Kind::Brightness => (1, 0),
        Kind::Battery => (1, 1),
        Kind::SidePlate => (1, 2),
        // domain 2 — SESSION (software state)
        Kind::Profile => (2, 0),
        Kind::Layer => (2, 1),
        Kind::Macro => (2, 2),
    }
}

/// (Re)build the WHAT-EARNS-A-PING models from `Kind::ALL` + the live pref gates, one model per
/// domain row. Rows update IN PLACE once a model exists (hover survives a toggle); a model is only
/// rebuilt when its kind count changes (a new build with a new kind).
fn refresh_ping_kinds(app: &AppWindow) {
    let prefs = crate::prefs::Prefs::load();
    let mut kinds = neuron::confirm::Kind::ALL;
    kinds.sort_by_key(|k| ping_slot(*k));
    let mut domains: [Vec<PingKind>; 3] = Default::default();
    for k in kinds {
        let (icon, label, desc) = ping_copy(k);
        let (domain, _) = ping_slot(k);
        domains[(domain as usize).min(domains.len() - 1)].push(PingKind {
            slug: k.slug().into(),
            icon: icon.into(),
            label: label.into(),
            desc: desc.into(),
            on: prefs.notif_kind_on(k),
        });
    }
    let state = app.global::<State>();
    let upload = |existing: slint::ModelRc<PingKind>, rows: Vec<PingKind>| -> Option<slint::ModelRc<PingKind>> {
        match existing.as_any().downcast_ref::<VecModel<PingKind>>() {
            Some(vm) if vm.row_count() == rows.len() => {
                for (i, row) in rows.into_iter().enumerate() {
                    vm.set_row_data(i, row);
                }
                None // updated in place
            }
            _ => Some(ModelRc::new(VecModel::from(rows))),
        }
    };
    let [pointer, hardware, session] = domains;
    if let Some(m) = upload(state.get_ping_pointer(), pointer) {
        state.set_ping_pointer(m);
    }
    if let Some(m) = upload(state.get_ping_hardware(), hardware) {
        state.set_ping_hardware(m);
    }
    if let Some(m) = upload(state.get_ping_session(), session) {
        state.set_ping_session(m);
    }
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

/// A node's KIND tag — the stable string the flat model + the edit-ops key on (matches MacroBlock's
/// `kind` field). One mapping so the canvas, the add-defaults, and the flatten never drift.
fn macro_kind(node: &MacroNode) -> &'static str {
    match node {
        MacroNode::Type { .. } => "type",
        MacroNode::Press { .. } => "press",
        MacroNode::KeyPress { .. } => "key_press",
        MacroNode::Click { .. } => "click",
        MacroNode::Scroll { .. } => "scroll",
        MacroNode::MoveTo { .. } => "move_to",
        MacroNode::Copy { .. } => "copy",
        MacroNode::Paste => "paste",
        MacroNode::Open { .. } => "open",
        MacroNode::Focus { .. } => "focus",
        MacroNode::Wait { .. } => "wait",
        MacroNode::Notify { .. } => "notify",
        MacroNode::Ask { .. } => "ask",
        MacroNode::If { .. } => "if",
        MacroNode::RepeatN { .. } => "repeat_n",
        MacroNode::RepeatWhile { .. } => "repeat_while",
        MacroNode::ForEach { .. } => "for_each",
        MacroNode::SetVar { .. } => "set_var",
        MacroNode::Stop => "stop",
        MacroNode::Try { .. } => "try",
        MacroNode::Raw { .. } => "raw",
    }
}

/// The PLAIN-LANGUAGE verb shown on a step card — the macro reads like a sentence ("type ⟨hi⟩",
/// "press ⟨ctrl+c⟩", "open ⟨notepad⟩"). Kept beside `macro_kind` so the two stay in lockstep.
fn macro_verb(kind: &str) -> &'static str {
    match kind {
        "type" => "type",
        "press" => "press",
        "key_press" => "key",
        "click" => "click",
        "scroll" => "scroll",
        "move_to" => "move",
        "copy" => "copy",
        "paste" => "paste",
        "open" => "open",
        "focus" => "focus",
        "wait" => "wait",
        "notify" => "notify",
        "ask" => "ask",
        "if" => "if",
        "repeat_n" => "repeat",
        "repeat_while" => "while",
        "for_each" => "for each",
        "set_var" => "set",
        "stop" => "stop",
        "try" => "try",
        "raw" => "code",
        _ => "",
    }
}

/// The editable PARAM string a step shows in its inline field — the human bit, rendered through
/// [`value_to_source`] so a Value param shows as the Python expression the user edits (`"hi"`,
/// `ctx.selection.upper()`, `"got " + ctx.app`). Non-Value params (key chords, button, var names)
/// show their plain form. Parameter-less nodes (paste/stop) show nothing.
fn macro_value(node: &MacroNode) -> String {
    match node {
        MacroNode::Type { text, .. }
        | MacroNode::Copy { text }
        | MacroNode::Notify { text } => value_to_source(text),
        MacroNode::Scroll { amount } => value_to_source(amount),
        MacroNode::Wait { ms } => value_to_source(ms),
        MacroNode::Focus { window } => value_to_source(window),
        MacroNode::Open { command, .. } => value_to_source(command),
        MacroNode::MoveTo { x, y } => format!("{}, {}", value_to_source(x), value_to_source(y)),
        MacroNode::Press { keys } => keys.join("+"),
        MacroNode::KeyPress { name } => name.clone(),
        MacroNode::Click { button } => button.clone(),
        MacroNode::Paste | MacroNode::Stop => String::new(),
        MacroNode::Ask { question, .. } => value_to_source(question),
        MacroNode::If { cond, .. } | MacroNode::RepeatWhile { cond, .. } => value_to_source(cond),
        MacroNode::RepeatN { count, .. } => value_to_source(count),
        MacroNode::ForEach { var, source, .. } => {
            format!("{var} in {}", value_to_source(source))
        }
        MacroNode::SetVar { name, value } => format!("{name} = {}", value_to_source(value)),
        MacroNode::Try { .. } => String::new(),
        MacroNode::Raw { code } => code.clone(),
    }
}

/// FLATTEN the working tree into the flat `MacroBlock` model the constructor canvas renders (Slint
/// can't draw a recursive tree). Emits, in order: each step row (carrying its PATH back into the
/// tree); for an `Ask` → the ask step, then a YES lane row, its children (depth+1) recursed, a YES
/// add-row, then a NO lane row, its children, a NO add-row; and after the whole root body, a final
/// root add-row. So every body/branch ends with its own +add insert-point. `prefix` is the path of
/// the body being flattened ("" at root, "1.yes" inside an ask arm); `depth` drives the indent. Pure
/// + instant (no I/O) — it runs synchronously after every edit so the canvas updates immediately.
fn flatten_macro(nodes: &[MacroNode], depth: i32, prefix: &str, out: &mut Vec<MacroBlock>) {
    emit_body(nodes, depth, prefix, out);
    // the TOP-LEVEL body ends with its own +add insert-point (path = the root context). A branch
    // sub-body's add-point is emitted by `emit_body` right after its arm, so only the root adds here.
    out.push(add_row(prefix, depth));
}

/// One +add insert-point row for the body at `ctx` (the path to append into).
fn add_row(ctx: &str, depth: i32) -> MacroBlock {
    MacroBlock {
        row: "add".into(),
        path: ctx.into(),
        depth,
        kind: "".into(),
        verb: "".into(),
        value: "".into(),
        last: false,
    }
}

/// The LANES a flow node exposes on the canvas: `(lane_label, arm_segment, body)`. The `arm_segment`
/// is the path token that descends into that body (`yes`/`no`, `then`/`else`, `body`, `error`) — it
/// MUST match what [`parent_body_and_index`] / [`body_at_context`] descend through. A non-flow node
/// returns an empty list (a plain step, no lanes). Generalizing the per-node lane set here is what
/// lets `emit_body` flatten every flow kind through one loop.
fn node_lanes(node: &MacroNode) -> Vec<(&'static str, &'static str, &[MacroNode])> {
    match node {
        MacroNode::Ask { yes, no, .. } => vec![("yes", "yes", yes), ("no", "no", no)],
        MacroNode::If { then_, else_, .. } => {
            vec![("then", "then", then_), ("else", "else", else_)]
        }
        MacroNode::RepeatN { body, .. }
        | MacroNode::RepeatWhile { body, .. }
        | MacroNode::ForEach { body, .. } => vec![("body", "body", body)],
        MacroNode::Try { body, except_ } => vec![("body", "body", body), ("error", "error", except_)],
        _ => Vec::new(),
    }
}

/// Emit the step (+ lane) rows for one body — WITHOUT a trailing root add-point. Each FLOW node's
/// lanes are emitted inline here (a lane header, the recursed sub-body one level in, then the lane's
/// own +add), so a sub-body's insert-point is owned by its lane, not by a recursive trailing add.
/// Generalized over [`node_lanes`], so Ask/If/RepeatN/RepeatWhile/ForEach/Try all flatten identically.
fn emit_body(nodes: &[MacroNode], depth: i32, prefix: &str, out: &mut Vec<MacroBlock>) {
    let join = |i: usize| -> String {
        if prefix.is_empty() {
            i.to_string()
        } else {
            format!("{prefix}.{i}")
        }
    };
    let last_idx = nodes.len().saturating_sub(1);
    for (i, node) in nodes.iter().enumerate() {
        let path = join(i);
        let kind = macro_kind(node);
        out.push(MacroBlock {
            row: "step".into(),
            path: path.clone().into(),
            depth,
            kind: kind.into(),
            verb: macro_verb(kind).into(),
            value: macro_value(node).into(),
            last: i == last_idx,
        });
        for (label, arm, body) in node_lanes(node) {
            // a lane header, the recursed sub-body one level deeper, then the lane's own +add.
            let lane_ctx = format!("{path}.{arm}");
            out.push(MacroBlock {
                row: "lane".into(),
                path: path.clone().into(),
                depth: depth + 1,
                kind: label.into(),
                verb: "".into(),
                value: "".into(),
                last: false,
            });
            emit_body(body, depth + 2, &lane_ctx, out);
            out.push(add_row(&lane_ctx, depth + 2));
        }
    }
}

/// Descend one ARM of a flow node — the mutable sub-body the path segment `arm` names. The arm tokens
/// match [`node_lanes`]: `yes`/`no` (Ask), `then`/`else` (If), `body` (RepeatN/RepeatWhile/ForEach or
/// a Try's guarded body), `error` (a Try's except). Returns `None` if the node isn't a flow node with
/// that arm. ONE place owns the arm→sub-body mapping, so the flatten + both path resolvers agree.
fn node_arm_body<'a>(node: &'a mut MacroNode, arm: &str) -> Option<&'a mut Vec<MacroNode>> {
    match node {
        MacroNode::Ask { yes, no, .. } => match arm {
            "yes" => Some(yes),
            "no" => Some(no),
            _ => None,
        },
        MacroNode::If { then_, else_, .. } => match arm {
            "then" => Some(then_),
            "else" => Some(else_),
            _ => None,
        },
        MacroNode::RepeatN { body, .. }
        | MacroNode::RepeatWhile { body, .. }
        | MacroNode::ForEach { body, .. } => match arm {
            "body" => Some(body),
            _ => None,
        },
        MacroNode::Try { body, except_ } => match arm {
            "body" => Some(body),
            "error" => Some(except_),
            _ => None,
        },
        _ => None,
    }
}

/// Resolve a PATH to a mutable reference to the body Vec that DIRECTLY contains the addressed node,
/// plus the node's index within it. The path is dot-separated: a numeric segment indexes the current
/// body; an arm segment (`yes`/`no`/`then`/`else`/`body`/`error`) descends into the preceding flow
/// node's sub-body. Returns `None` if any segment is out of range or the path is malformed (e.g. an
/// arm step into a node without that arm). This is the one resolver every edit-op uses — index into
/// the returned body to read/replace/remove the node.
fn parent_body_and_index<'a>(
    tree: &'a mut Vec<MacroNode>,
    path: &str,
) -> Option<(&'a mut Vec<MacroNode>, usize)> {
    let segs: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return None;
    }
    let mut body = tree;
    let mut i = 0;
    while i < segs.len() {
        let idx: usize = segs[i].parse().ok()?;
        // the LAST segment must be a numeric index — it names the node in the current body.
        if i + 1 == segs.len() {
            if idx >= body.len() {
                return None;
            }
            return Some((body, idx));
        }
        // otherwise the next segment is a flow arm descending into this node's sub-body.
        let arm = segs[i + 1];
        let node = body.get_mut(idx)?;
        body = node_arm_body(node, arm)?;
        i += 2;
    }
    None
}

/// Resolve a PATH to a mutable reference to the addressed node itself (for `edit-step`). Thin wrapper
/// over [`parent_body_and_index`].
fn node_at_path<'a>(tree: &'a mut Vec<MacroNode>, path: &str) -> Option<&'a mut MacroNode> {
    let (body, idx) = parent_body_and_index(tree, path)?;
    body.get_mut(idx)
}

/// Resolve an INSERT-CONTEXT path (the body to append into) to a mutable reference to that body. An
/// empty path is the root body; otherwise the path ends in a flow arm (`yes`/`no`/`then`/`else`/
/// `body`/`error`) of a flow node. Returns `None` if the context doesn't resolve to such an arm.
fn body_at_context<'a>(
    tree: &'a mut Vec<MacroNode>,
    ctx: &str,
) -> Option<&'a mut Vec<MacroNode>> {
    let segs: Vec<&str> = ctx.split('.').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return Some(tree);
    }
    let mut body = tree;
    let mut i = 0;
    while i < segs.len() {
        let idx: usize = segs[i].parse().ok()?;
        let arm = segs.get(i + 1)?;
        let node = body.get_mut(idx)?;
        body = node_arm_body(node, arm)?;
        i += 2;
    }
    Some(body)
}

/// A fresh default node for `kind` — what `add-step` drops in (empty, ready to edit in place). Value
/// params default to an empty string literal (text-ish) or a sensible literal (counts/coords →
/// integers, conditions → `True`); flow bodies start empty (their lanes show their own +add). Returns
/// `None` for an unknown kind, so `add-step` never panics on a stray kind string.
fn default_node(kind: &str) -> Option<MacroNode> {
    Some(match kind {
        "type" => MacroNode::Type {
            text: Value::empty_str(),
            ghost: false,
            speed: None,
        },
        "press" => MacroNode::Press { keys: Vec::new() },
        "key_press" => MacroNode::KeyPress { name: String::new() },
        "click" => MacroNode::Click {
            button: "left".into(),
        },
        "scroll" => MacroNode::Scroll {
            amount: Value::Int { n: 1 },
        },
        "move_to" => MacroNode::MoveTo {
            x: Value::Int { n: 0 },
            y: Value::Int { n: 0 },
        },
        "copy" => MacroNode::Copy {
            text: Value::empty_str(),
        },
        "paste" => MacroNode::Paste,
        "open" => MacroNode::Open {
            command: Value::empty_str(),
            capture: None,
        },
        "focus" => MacroNode::Focus {
            window: Value::empty_str(),
        },
        "wait" => MacroNode::Wait {
            ms: Value::Int { n: 500 },
        },
        "notify" => MacroNode::Notify {
            text: Value::empty_str(),
        },
        "ask" => MacroNode::Ask {
            question: Value::empty_str(),
            description: Value::empty_str(),
            yes: Vec::new(),
            no: Vec::new(),
        },
        "if" => MacroNode::If {
            cond: Value::Bool { b: true },
            then_: Vec::new(),
            else_: Vec::new(),
        },
        "repeat_n" => MacroNode::RepeatN {
            count: Value::Int { n: 1 },
            body: Vec::new(),
        },
        "repeat_while" => MacroNode::RepeatWhile {
            cond: Value::Bool { b: true },
            body: Vec::new(),
        },
        "for_each" => MacroNode::ForEach {
            var: "item".into(),
            source: Value::empty_str(),
            body: Vec::new(),
        },
        "set_var" => MacroNode::SetVar {
            name: "x".into(),
            value: Value::empty_str(),
        },
        "stop" => MacroNode::Stop,
        "try" => MacroNode::Try {
            body: Vec::new(),
            except_: Vec::new(),
        },
        "raw" => MacroNode::Raw { code: String::new() },
        _ => return None,
    })
}

/// Split a chord/key spec into lowercased key names (the press-keys setter): on `+`, `,`, or space.
fn split_keys(v: &str) -> Vec<String> {
    v.split(|c| c == '+' || c == ',' || c == ' ')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Apply an EDIT-STEP value to a node in place. Text-ish Value params become a `Str` literal (the
/// field is the literal string); expression Value params become a `Raw` verbatim (the user typed an
/// expr — the next code-parse normalizes it to a typed Value). Compound-display flow params
/// (move "x, y", for-each "var in src", set "name = expr") are split back into their parts on the
/// display separator; an un-splittable edit lands the whole thing in the first/expr slot so no data
/// is lost. Non-Value params (press keys, click button, key name, raw code) keep their plain setters.
fn edit_node_value(node: &mut MacroNode, v: String) {
    match node {
        // text-ish Value params → a Str literal (the field IS the string).
        MacroNode::Type { text, .. }
        | MacroNode::Copy { text }
        | MacroNode::Notify { text } => *text = Value::Str { s: v },
        MacroNode::Open { command, .. } => *command = Value::Str { s: v },
        MacroNode::Focus { window } => *window = Value::Str { s: v },
        // expression Value params → a Raw verbatim (power users type an expr).
        MacroNode::Scroll { amount } => *amount = Value::raw(v.trim()),
        MacroNode::Wait { ms } => *ms = Value::raw(v.trim()),
        MacroNode::Ask { question, .. } => *question = Value::Str { s: v },
        MacroNode::If { cond, .. } | MacroNode::RepeatWhile { cond, .. } => {
            *cond = Value::raw(v.trim())
        }
        MacroNode::RepeatN { count, .. } => *count = Value::raw(v.trim()),
        // compound flow displays → split on the display separator.
        MacroNode::MoveTo { x, y } => {
            if let Some((a, b)) = v.split_once(',') {
                *x = Value::raw(a.trim());
                *y = Value::raw(b.trim());
            } else {
                *x = Value::raw(v.trim());
            }
        }
        MacroNode::ForEach { var, source, .. } => {
            if let Some((name, src)) = v.split_once(" in ") {
                *var = name.trim().to_string();
                *source = Value::raw(src.trim());
            } else {
                *source = Value::raw(v.trim());
            }
        }
        MacroNode::SetVar { name, value } => {
            if let Some((n, expr)) = v.split_once('=') {
                *name = n.trim().to_string();
                *value = Value::raw(expr.trim());
            } else {
                *value = Value::raw(v.trim());
            }
        }
        // non-Value params.
        MacroNode::Press { keys } => *keys = split_keys(&v),
        MacroNode::KeyPress { name } => *name = v.trim().to_lowercase(),
        MacroNode::Click { button } => *button = v.trim().to_lowercase(),
        MacroNode::Raw { code } => *code = v,
        // parameter-less nodes carry no inline field; ignore an edit.
        MacroNode::Paste | MacroNode::Stop | MacroNode::Try { .. } => {}
    }
}

/// After ANY tree mutation: regenerate the Python source from the tree (pure Rust codegen, instant),
/// push it into `macro-source` WITH the dirty-guard set (so the CodeArea's `edited` hook skips the
/// re-parse), recompute `macro-has-ask`, and re-flatten the tree into `macro-blocks`. The canvas + the
/// code view both stay current off the one source of truth, synchronously, no Python in the loop.
fn regenerate_from_tree(st: &State, tree: &[MacroNode]) {
    let source = neuron::macros::nodes_to_source(tree);
    st.set_macro_has_ask(source.contains("neuron.ask"));
    // ARM the dirty-guard only when the code editor is actually mounted (the user is in code view):
    // a programmatic `set_macro_source` updates the CodeArea via its <=> binding but does NOT fire
    // `edited`, so arming it while the canvas is showing would leave it stuck true and mis-flag the
    // user's next code-view keystroke. Canvas edits happen while !code-view, so this stays false then.
    if st.get_macro_code_view() {
        st.set_macro_source_dirty(true);
    }
    st.set_macro_source(source.into());
    let mut blocks = Vec::new();
    flatten_macro(tree, 0, "", &mut blocks);
    st.set_macro_blocks(ModelRc::new(VecModel::from(blocks)));
}

/// After a VALUE-ONLY edit (edit-step): regenerate the Python source from the tree and recompute
/// `macro-has-ask`, but DO NOT re-flatten into `macro-blocks`. A value edit changes a node's param,
/// not the macro's STRUCTURE — so the flat model's row layout is unchanged, and re-setting it would
/// rebuild every `MacroBlockEl` (losing the caret / focus of the very field being typed in, and on a
/// blur-commit yanking focus mid-gesture). The live field already shows the typed text; the model's
/// now-stale `value` is harmless because it's only re-read on the NEXT structural re-flatten (add /
/// delete / move), which reads the now-correct tree. This is what makes click-away commit + smooth
/// typing possible. Structural ops still call `regenerate_from_tree` (which DOES re-flatten).
fn regenerate_source_only(st: &State, tree: &[MacroNode]) {
    let source = neuron::macros::nodes_to_source(tree);
    st.set_macro_has_ask(source.contains("neuron.ask"));
    if st.get_macro_code_view() {
        st.set_macro_source_dirty(true);
    }
    st.set_macro_source(source.into());
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
        active_frame: 0,
        layers_rev: 0,
        // the macro constructor starts empty; the canvas's root +add invites the first step, or a
        // parse of an existing macro's source (on entering the editor) reseeds it.
        macro_tree: Vec::new(),
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
    // a truly-fresh install gets the bundled exemplar macro before the registry first reads disk.
    neuron::macros::macro_host::seed_default_macros();
    refresh_beacon_macros(app);
    // the WORKSHOP catalog — every macro on disk as an emergent card (off-thread parse for summaries).
    refresh_macro_catalog(app);
    // seed the macro constructor's canvas so its root +add exists from FIRST paint. The editor opens
    // in BLOCKS mode, but `refresh-macro-blocks` only fires on a toggle-to-blocks — so without this an
    // empty canvas had no +add and the very first step was unreachable until a code-view round-trip
    // ran a parse. `flatten([])` emits exactly the root +add invitation ("add your first step").
    {
        let mut blocks = Vec::new();
        flatten_macro(&[], 0, "", &mut blocks);
        st.set_macro_blocks(ModelRc::new(VecModel::from(blocks)));
    }
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
    {
        let feel = neuron::feel::FeelConfig::load();
        st.set_hypershift_mode(feel.hypershift.describe().into());
        // the FEEL timing windows seed from the same persisted config as every other setting.
        st.set_feel_hold_ms(feel.hold_ms as i32);
        st.set_feel_gap_ms(feel.gap_ms as i32);
        st.set_feel_coyote_ms(feel.coyote_ms as i32);
    }

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
                // close any open inline rule editor: its row index is meaningful only against the
                // bindings list as it was, and switching the selected device is a context change — an
                // editor left open could otherwise commit against a wrong/missing row. (Done in the
                // user-initiated callback, NOT select_device_at, so a background device-list refresh
                // never yanks the editor shut.)
                app.global::<State>().set_editing_rule(-1);
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
        // the row hands over its UNIT id (`DeviceRow.id`), so backing up one of two identical
        // devices snapshots exactly the board whose button was clicked.
        app.global::<State>().on_backup_device(move |unit| {
            if let Some(app) = w.upgrade() {
                let msg = sh.borrow().rt.backup(unit.as_str());
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

    // ── the EVERYDAY DECK's global apply + reload ────────────────────────
    // apply-feel: commit every everyday device-write the SELECTED device supports in one gesture —
    // dpi / polling / brightness / dpi-stages, each only when its capability is present (and the
    // stages list parses). Honours writes-paused (a no-op, like the per-control handlers used to be),
    // mirrors the result on the status line, and re-seeds the channel rows from the device after.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_feel(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                if st.get_writes_paused() {
                    st.set_status_line("writes paused".into());
                    return;
                }
                // snapshot the draft State the deck has been editing.
                let can_dpi = st.get_sel_can_dpi();
                let can_poll = st.get_sel_can_poll();
                let can_light = st.get_sel_can_light();
                let can_store = st.get_sel_can_store();
                let dpi = st.get_dpi() as u16;
                let hz = st.get_polling_hz() as u32;
                let pct = st.get_brightness() as u8;
                let stages = st.get_dpi_stages().to_string();
                let active = st.get_dpi_active_stage().max(0) as u8;
                let stages_ok = can_dpi && st.get_dpi_stages_valid();
                // one borrow for the whole batch; collect a single confirmation line.
                let mut lines: Vec<String> = Vec::new();
                {
                    let mut s = sh.borrow_mut();
                    s.rt.persist = can_store && st.get_persist_to_onboard();
                    if can_dpi {
                        lines.push(s.rt.apply_dpi(dpi));
                    }
                    if can_poll {
                        let (msg, actual) = s.rt.apply_polling(hz);
                        if let Some(a) = actual {
                            st.set_polling_hz(a as f32);
                        }
                        lines.push(msg);
                    }
                    if can_light {
                        lines.push(s.rt.apply_brightness(pct));
                    }
                    if stages_ok {
                        lines.push(s.rt.apply_dpi_stages(stages.as_str(), active));
                    }
                }
                st.set_status_line(
                    if lines.is_empty() {
                        "nothing to apply".to_string()
                    } else {
                        lines.join(" · ")
                    }
                    .into(),
                );
                // re-seed the rows from the device so the readouts match what was just committed.
                refresh_devices(&app, &sh);
            }
        });
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
                        Outcome::Done { killed, stopped, demoted }
                            if killed == 0 && stopped == 0 && demoted == 0 =>
                        {
                            "no Synapse left — already clean".to_string()
                        }
                        Outcome::Done { killed, stopped, demoted } => {
                            format!("purged Synapse — set {demoted} service(s) to manual + stopped {stopped}, killed {killed} process(es)")
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
                    // kill THIS board's running generator first — a 30fps repaint would erase this
                    // write within a frame and the click would look dead. (Other boards are untouched.)
                    let (sel, sel_unit) = (s.rt.selected_pid, s.rt.selected_unit.clone());
                    s.rt.stop_animation(sel, &sel_unit);
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
        app.global::<State>().on_stop_animation(move || {
            if let Some(app) = w.upgrade() {
                let (pid, unit) = {
                    let s = sh.borrow();
                    (s.rt.selected_pid, s.rt.selected_unit.clone())
                };
                sh.borrow_mut().rt.stop_animation(pid, &unit);
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
        // read-back — badged "~ live". The Compositor is cached and rebuilt when the stack changes
        // (`layers_rev`) OR the grid DIMS change, so stateful effects (fire's heat map) keep state
        // across ticks — and switching to a same-stack different-dims device rebuilds the generators
        // (a stale-dim generator emits the wrong frame length and the layer goes dark via the
        // `Compositor::frame` length-skip). The cache key is `(rev, rows, cols)`.
        let cache: Rc<RefCell<Option<(u64, i32, i32, neuron::pattern::Compositor)>>> =
            Rc::new(RefCell::new(None));
        // FOREIGN-OWNER strip cadence: the preview ticks ~20Hz while the page is open — poll the
        // arbiter's claims every ~24th tick (~1s) so "a game is painting this board" appears and
        // clears without a dedicated timer. A kernel round-trip is microseconds; 1Hz is plenty.
        let foreign_tick: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        app.global::<State>().on_preview_tick(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                {
                    let mut t = foreign_tick.borrow_mut();
                    *t += 1;
                    if *t % 24 == 1 {
                        let (pid, unit) = {
                            let s = sh.borrow();
                            (s.rt.selected_pid, s.rt.selected_unit.clone())
                        };
                        // The arbiter's ownership truth: yours / a named client
                        // painting on top / a named client connected-but-losing
                        // (the "my lighting wins" policy). Three honest states,
                        // one of which no last-writer-wins tool can even see.
                        use crate::host::BoardOwner::*;
                        let (painting, suppressed, app) = match crate::host::board_owner(pid, &unit)
                        {
                            Yours => (false, false, String::new()),
                            Painting(name) => (true, false, name),
                            Suppressed(name) => (false, true, name),
                        };
                        st.set_light_foreign(painting);
                        st.set_light_suppressed(suppressed);
                        st.set_light_foreign_app(app.into());
                    }
                }
                let (rows, cols) = (st.get_grid_rows(), st.get_grid_cols());
                // the BRUSH owns the grid (the per-LED editor) — don't fight it.
                if st.get_light_brush_on() || rows <= 0 || cols <= 0 {
                    cache.replace(None);
                    return;
                }
                // Read the CHEAP signals first — the stack revision, whether the stack is empty, and the
                // fps (the shared atomic the device stream seeds from, NOT a separate Slint property read,
                // so preview + board are paced by the same value). The full `light_layers` clone is
                // DEFERRED to the rebuild branch below: at ~20Hz it's wasteful to deep-clone every layer's
                // spectrum/params each frame when the compositor is cached and only rebuilds on a change.
                let (rev, is_empty, fps) = {
                    let s = sh.borrow();
                    (
                        s.layers_rev,
                        s.light_layers.is_empty(),
                        s.rt.light_fps.load(std::sync::atomic::Ordering::Relaxed),
                    )
                };
                // The vitals readout is no longer a special surface — it's just a `vitals` LAYER in the
                // stack, so it renders through the SAME compositor path below (reading the live provider
                // the heartbeat feeds), exactly like every effect.
                let n = (rows * cols) as usize;
                if is_empty {
                    // an empty stack = a dark device; mirror that honestly
                    st.set_grid_px(ModelRc::new(VecModel::from(vec![GRID_OFF; n])));
                    cache.replace(None);
                    return;
                }
                // QUANTIZED SHARED-CLOCK time: the ONE `quantized_t` helper off the process-global
                // `render_epoch` the device's animate() loop ALSO uses — same epoch + same formula, so
                // the preview advances in the SAME discrete frames the keyboard does (chunky at 6fps,
                // smooth at 30), and a restack can't jump the phase. Tuning fps (the shared atomic above)
                // re-paces the preview and the board together.
                let t = neuron::pattern::quantized_t(neuron::pattern::render_elapsed(), fps);
                let mut c = cache.borrow_mut();
                // rebuild the generators when the stack revision OR the grid dims change (a dims change
                // needs fresh generators or they emit stale-length frames and the layer goes dark) —
                // but DRIVE them from the shared quantized clock above, not a reset-on-rebuild anchor.
                let stale = c
                    .as_ref()
                    .map(|(r, cr, cc, _)| *r != rev || *cr != rows || *cc != cols)
                    .unwrap_or(true);
                if stale {
                    // ONLY on a real rebuild (the stack revision or the grid dims changed) do we clone the
                    // stack into fresh generators — the clone deferred from the cheap read above.
                    let defs = sh.borrow().light_layers.clone();
                    *c = Some((rev, rows, cols, neuron::pattern::Compositor::from_defs(&defs)));
                }
                let (_, _, _, comp) = c.as_mut().unwrap();
                let frame = comp.render(rows as u8, cols as u8, t);
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
    // ── Lighting COMPOSITOR — fps / import (the layer stack is edited via the unified surface + the
    // spectrum editor below, and AUTO-STREAMS on every edit: `refresh_layers` → `schedule_lighting_apply`
    // debounce-restreams the current stack, so there is no manual apply/stop transport. Auto-apply
    // supersedes the old "apply live"/"restart" button; the global writes-pause gate is the one deliberate
    // "stop writing" (a page-local stop would just be undone by the next edit). fps stays special — it
    // re-paces the RUNNING stream in place, no restart, so it doesn't route through auto-apply, below) ──
    bind(app, &shared, |app, sh| {
        {
            let w = app.as_weak();
            let sh = sh.clone();
            // STREAM RATE: re-pace the live effect AND its on-screen preview together off ONE source —
            // the shared `light_fps` atomic. Writing it reaches a RUNNING worker (it re-reads fps each
            // frame) so a streaming composite re-paces immediately (no restart), AND it's the value the
            // preview loop reads, so the mirror keeps pace too. The Slint property is only a DISPLAY echo
            // (reflecting the 1–30 clamp back to the slider), never a source. Vitals now streams like any
            // effect, so the rate applies to it as well. Clamped to the slider's 1–30 (= `animate`'s).
            app.global::<State>().on_set_light_fps(move |v| {
                if let Some(app) = w.upgrade() {
                    let fps = (v.round() as i64).clamp(1, 30) as u32;
                    {
                        let s = sh.borrow();
                        // the single in-memory source of truth: the preview reads it, new streams seed it.
                        s.rt
                            .light_fps
                            .store(fps, std::sync::atomic::Ordering::Relaxed);
                        // re-pace ONLY the selected board's live stream — others keep their own fps.
                        let (pid, unit) = (s.rt.selected_pid, s.rt.selected_unit.clone());
                        s.rt.set_anim_fps(pid, &unit, fps);
                    }
                    // display echo only — reflect the clamped value back to the slider.
                    app.global::<State>().set_light_fps(fps as f32);
                    // persist the user's fps pick for this board (debounced; restored on relaunch).
                    save_lighting(&sh);
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
                                // Lighting now flows as ONE representation: the profile's compositor stack
                                // (`Vec<LayerDef>`). Import pours that stack straight into the live
                                // compositor. If the stack carries a `custom` hand-painted/imported frame
                                // layer, ALSO drop its cells on the paint canvas so the user can edit it.
                                let layers: Vec<neuron::pattern::LayerDef> =
                                    imp.profile.lighting.clone();
                                if layers.is_empty() {
                                    let note = imp.notes.first().cloned().unwrap_or_default();
                                    st.set_status_line(
                                        format!("no lighting in that export{}", if note.is_empty() { String::new() } else { format!(" — {note}") }).into(),
                                    );
                                } else {
                                    let n = layers.len();
                                    // the raw cells of a `custom` layer, if one rode in (edit on the canvas).
                                    let custom_cells: Option<Vec<[u8; 3]>> = layers
                                        .iter()
                                        .find(|l| l.pattern == "custom")
                                        .map(|l| l.frame.clone());
                                    {
                                        let mut s = sh.borrow_mut();
                                        s.light_layers = layers;
                                        s.selected_layer = s.light_layers.len().saturating_sub(1);
                                        s.active_frame = 0;
                                        s.layers_rev += 1;
                                    }
                                    if let Some(cells) = custom_cells {
                                        // paint the custom frame onto the canvas + enter paint mode.
                                        let (rows, cols) = (st.get_grid_rows(), st.get_grid_cols());
                                        let count = (rows.max(0) * cols.max(0)) as usize;
                                        let mut px: Vec<slint::Color> = cells
                                            .iter()
                                            .take(count)
                                            .map(|c| slint::Color::from_rgb_u8(c[0], c[1], c[2]))
                                            .collect();
                                        while px.len() < count {
                                            px.push(GRID_OFF);
                                        }
                                        st.set_grid_px(ModelRc::new(VecModel::from(px)));
                                        st.set_light_paint_mode(true);
                                    } else {
                                        st.set_light_paint_mode(false);
                                    }
                                    refresh_layers(&app, &sh);
                                    st.set_status_line(format!("imported {n} layer(s)").into());
                                }
                            }
                            Err(e) => st.set_status_line(format!("import failed: {e}").into()),
                        }
                    });
                }
            });
        }
    });

    // ── Lighting UNIFIED SURFACE — tile pick · auto-rendered params · stack · brush ──
    bind(app, &shared, |app, sh| {
        // pick-tile(slug): the ONE gesture for every tile — effects AND the vitals readout alike, now
        // that vitals is just a preset. A tile becomes the one active effect (replacing the selected
        // layer when a stack exists, else a fresh single layer), landing in the SAME stack path.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_pick_tile(move |slug| {
                if let Some(app) = w.upgrade() {
                    let slug = slug.to_string();
                    // the notifications DATA tile is a future surface — not wired yet.
                    if slug == "notifications" {
                        app.global::<State>().set_status_line(
                            "notifications is a future data tile — not wired yet".into(),
                        );
                        return;
                    }
                    // build the layer this PRESET describes (pattern + its params + spectrum) and pour it
                    // into the stack. Vitals is just another preset now — no data-mode fork.
                    let readout = {
                        let mut s = sh.borrow_mut();
                        let layer = preset_layer(&slug);
                        if s.light_layers.is_empty() {
                            // single-effect default — no layer ceremony.
                            s.light_layers.push(layer);
                            s.selected_layer = 0;
                        } else {
                            // replace the ACTIVE (selected) layer's pattern/params/spectrum, keeping its
                            // spatial REGION + enabled (the look changes, the placement stays — picking a
                            // tile re-skins the layer you're editing). BLEND is normally preserved too, but
                            // a READOUT (vitals, on-air) overlay DEPENDS on its Cut blend to show the effect
                            // through its idle cells — so a readout pick adopts the PRESET's blend instead
                            // of the layer's old one (a non-readout pick keeps preserving the current blend).
                            let sel = s.selected_layer.min(s.light_layers.len() - 1);
                            let region = s.light_layers[sel].region.clone();
                            let enabled = s.light_layers[sel].enabled;
                            let blend = if neuron::pattern::pattern_is_readout(&layer.pattern) {
                                layer.blend
                            } else {
                                s.light_layers[sel].blend
                            };
                            s.light_layers[sel] = neuron::pattern::LayerDef {
                                region,
                                blend,
                                enabled,
                                ..layer
                            };
                            s.selected_layer = sel;
                        }
                        s.active_frame = 0;
                        s.layers_rev += 1;
                        let sel = s.selected_layer;
                        s.light_layers
                            .get(sel)
                            .map(|l| neuron::pattern::pattern_is_readout(&l.pattern))
                            .unwrap_or(false)
                    };
                    // a READOUT (vitals) layer needs live source data to show anything — kick a prompt,
                    // forced publish so the preview lights at once instead of waiting for the heartbeat.
                    if readout {
                        sh.borrow().rt.pump_vitals(true);
                    }
                    refresh_layers(&app, &sh);
                    // structural change (a tile pick) — persist NOW, not 400ms later, so it survives an
                    // immediate quit-and-relaunch (the debounce alone could strand it).
                    flush_lighting_save();
                    app.global::<State>()
                        .set_status_line(format!("lighting → {slug}").into());
                }
            });
        }
        // a RANGE knob (speed/density/fade) → write the matching layer field on the active layer
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_param_range(move |key, v| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            // a continuous knob writes straight into the pattern's param bag (keyed by the
                            // schema's stable key — speed/density/fade/sensitivity/…); the pattern clamps.
                            d.params.set(key.as_str(), v);
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // an ENUM knob (direction/mode/source/flow) → write the matching layer field as its numeric
        // index. A fresh apply rebuilds the compositor; the meter's new source flows to the shared
        // `audio_spectrum` analyser (and its `audio_level` fallback), which re-points to the new
        // endpoint on the next `ensure` (no stale per-generator handle — the generator holds no
        // audio handle of its own).
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_param_enum(move |key, i| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            // an enum knob stores its chosen INDEX (direction/mode/source/…) in the param
                            // bag as a whole number; the pattern reads it back with `Params::u8`.
                            d.params.set(key.as_str(), i.max(0) as f32);
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // a TOGGLE knob → write the matching bool field on the active layer. Today the only Toggle in
        // the schema is reactive's `glow` (light a pressed key's neighbour ring); the dispatch is keyed
        // so adding another toggle is pure data — a new `key` arm here + a `LayerDef` bool field.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_param_toggle(move |key, on| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            // a toggle writes 0/1 into the param bag (e.g. ignite's `glow`); the pattern
                            // reads it back with `Params::bool`.
                            d.params.set(key.as_str(), if on { 1.0 } else { 0.0 });
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // ── SPECTRUM editor: the active layer's COLOUR program (gradient stops + motion + sequence) ──
        // Every callback mutates the active layer's spectrum (its ACTIVE frame's palette), then
        // `refresh_layers` re-projects it back into the State surface (so the strip/motion/timeline
        // stay in lockstep) + persists. The gradient strip:
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_stop_color(move |idx, hex| {
                if let (Some(app), Some(c)) = (w.upgrade(), Rgb::parse(hex.as_str())) {
                    edit_active_palette(&app, &sh, |pal| pal.set_stop_color(idx.max(0) as usize, c));
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_add_stop(move |at| {
                if let Some(app) = w.upgrade() {
                    // insert the stop, capturing the index it sorted into…
                    let mut inserted = 0usize;
                    edit_active_palette(&app, &sh, |pal| {
                        inserted = pal.add_stop(at);
                    });
                    // …then SELECT it (after refresh_layers rebuilt light-stops), delivering the
                    // documented "add a stop, select it" contract: the prism now edits the NEW stop,
                    // not whatever was selected before the insert sorted into the gradient.
                    app.global::<State>().set_light_sel_stop(inserted as i32);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_remove_stop(move |idx| {
                if let Some(app) = w.upgrade() {
                    edit_active_palette(&app, &sh, |pal| pal.remove_stop(idx.max(0) as usize));
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_move_stop(move |idx, at| {
                if let Some(app) = w.upgrade() {
                    let mut moved = idx.max(0) as usize;
                    edit_active_palette(&app, &sh, |pal| {
                        moved = pal.move_stop(idx.max(0) as usize, at);
                    });
                    // keep the cursor ON the dragged stop across the re-sort — without this, a drag
                    // that crossed a neighbour silently swapped which stop the gesture (and the
                    // prism) was editing.
                    app.global::<State>().set_light_sel_stop(moved as i32);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            // The strip's PRESS gesture — grab-or-add, hit-tested HERE rather than by per-handle
            // touch areas in Slint: every stop edit rebuilds the stops model, and a touch area
            // living inside that rebuilt `for` dies mid-drag (the old one-millimetre-per-grab
            // jank). The strip's single touch area survives every rebuild; this decides what the
            // press meant. Grabbing an existing stop is a pure SELECT (no layer write, no save
            // debounce); pressing open track inserts a stop there — and either way the stop is
            // selected, so the drag that follows slides it smoothly full-width.
            app.global::<State>().on_strip_press(move |at| {
                if let Some(app) = w.upgrade() {
                    // grab radius ≈ the handle's own visual half-width on the rendered strip
                    const GRAB: f32 = 0.04;
                    let at = at.clamp(0.0, 1.0);
                    let hit: Option<usize> = {
                        let s = sh.borrow();
                        let sel = s.selected_layer;
                        let fr = s.active_frame;
                        s.light_layers.get(sel).and_then(|d| {
                            let fr = fr.min(d.spectrum.seq.len().saturating_sub(1));
                            d.spectrum.seq.get(fr).and_then(|frame| {
                                frame
                                    .palette
                                    .stops
                                    .iter()
                                    .enumerate()
                                    .map(|(i, st)| (i, (st.at - at).abs()))
                                    .min_by(|a, b| a.1.total_cmp(&b.1))
                                    .filter(|&(_, d)| d <= GRAB)
                                    .map(|(i, _)| i)
                            })
                        })
                    };
                    let idx = match hit {
                        Some(i) => i,
                        None => {
                            let mut inserted = 0usize;
                            edit_active_palette(&app, &sh, |pal| inserted = pal.add_stop(at));
                            inserted
                        }
                    };
                    app.global::<State>().set_light_sel_stop(idx as i32);
                }
            });
        }
        // the MOTION row — each control writes its State property then rebuilds the active palette's
        // Motion from all four (kind/speed/depth/chaos), so a switch keeps the other knobs.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_motion(move |kind| {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_light_motion(kind);
                    rebuild_motion(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_motion_speed(move |v| {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_light_motion_speed(v);
                    rebuild_motion(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_motion_depth(move |v| {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_light_motion_depth(v);
                    rebuild_motion(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_motion_chaos(move |v| {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_light_motion_chaos(v);
                    rebuild_motion(&app, &sh);
                }
            });
        }
        // the INTERPOLATION toggle — set the active palette's gradient colour space (rgb | hsv).
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_interp(move |which| {
                if let Some(app) = w.upgrade() {
                    let interp = neuron::spectrum::Interp::from_str(which.as_str());
                    edit_active_palette(&app, &sh, |pal| pal.interp = interp);
                }
            });
        }
        // the TIMELINE / sequencer — add/remove/move/select frames + per-frame timing + loop policy.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_add_frame(move || {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        let fr = s.active_frame;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            let fr = fr.min(d.spectrum.seq.len().saturating_sub(1));
                            if let Some(clone) = d.spectrum.seq.get(fr).cloned() {
                                d.spectrum.seq.insert(fr + 1, clone);
                            }
                        }
                        let len = s.light_layers.get(sel).map(|d| d.spectrum.seq.len()).unwrap_or(1);
                        s.active_frame = (s.active_frame + 1).min(len.saturating_sub(1));
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_remove_frame(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            let i = idx.max(0) as usize;
                            if d.spectrum.seq.len() > 1 && i < d.spectrum.seq.len() {
                                d.spectrum.seq.remove(i);
                            }
                        }
                        let len = s.light_layers.get(sel).map(|d| d.spectrum.seq.len()).unwrap_or(1);
                        s.active_frame = s.active_frame.min(len.saturating_sub(1));
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_move_frame(move |idx, dir| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        let i = idx.max(0) as usize;
                        if let Some(d) = s.light_layers.get_mut(sel) {
                            let j = if dir < 0 { i.checked_sub(1) } else { Some(i + 1) };
                            if let Some(j) = j {
                                if i < d.spectrum.seq.len() && j < d.spectrum.seq.len() {
                                    d.spectrum.seq.swap(i, j);
                                    s.active_frame = j;
                                }
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
            app.global::<State>().on_select_frame(move |idx| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        let len = s.light_layers.get(sel).map(|d| d.spectrum.seq.len()).unwrap_or(1);
                        s.active_frame = (idx.max(0) as usize).min(len.saturating_sub(1));
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_frame_hold(move |idx, v| {
                if let Some(app) = w.upgrade() {
                    edit_active_spectrum(&app, &sh, |sp, _| {
                        if let Some(f) = sp.seq.get_mut(idx.max(0) as usize) {
                            f.hold = v.max(0.0);
                        }
                    });
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_frame_fade(move |idx, v| {
                if let Some(app) = w.upgrade() {
                    edit_active_spectrum(&app, &sh, |sp, _| {
                        if let Some(f) = sp.seq.get_mut(idx.max(0) as usize) {
                            f.fade = v.max(0.0);
                        }
                    });
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_frame_ease(move |idx, ease| {
                if let Some(app) = w.upgrade() {
                    edit_active_spectrum(&app, &sh, |sp, _| {
                        if let Some(f) = sp.seq.get_mut(idx.max(0) as usize) {
                            f.ease = neuron::spectrum::Ease::from_str(ease.as_str());
                        }
                    });
                }
            });
        }
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_loop(move |which| {
                if let Some(app) = w.upgrade() {
                    edit_active_spectrum(&app, &sh, |sp, _| {
                        sp.play = neuron::spectrum::Loop::from_str(which.as_str());
                    });
                }
            });
        }
        // + stack: layer the active effect AGAIN on top (a new layer, screen-blended so it reads as
        // added light), and select it so the knobs follow the new top.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_stack_current(move || {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        // never stack onto an empty stack (there's nothing to duplicate on top).
                        if !s.light_layers.is_empty() {
                            let sel = s.selected_layer.min(s.light_layers.len() - 1);
                            let mut d = s.light_layers[sel].clone();
                            d.blend = neuron::effects::Blend::Screen;
                            s.light_layers.push(d);
                            s.selected_layer = s.light_layers.len() - 1;
                            s.layers_rev += 1;
                        }
                    }
                    refresh_layers(&app, &sh);
                    flush_lighting_save(); // a new layer is structural — persist it immediately
                }
            });
        }
        // select a stack cell → that layer becomes the active one (its effect lights the grid + knobs)
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_select_stack(move |i| {
                if let Some(app) = w.upgrade() {
                    let changed = {
                        let mut s = sh.borrow_mut();
                        let i = i as usize;
                        if i < s.light_layers.len() && i != s.selected_layer {
                            s.selected_layer = i;
                            // the new layer has its OWN sequence + stops — start at the top of THAT
                            // layer so the timeline scrubber + the prism never point at a stale frame /
                            // stop carried over from the layer we just left (both indices are per-layer).
                            s.active_frame = 0;
                            true
                        } else {
                            false
                        }
                    };
                    if changed {
                        app.global::<State>().set_light_sel_stop(0);
                    }
                    refresh_layers(&app, &sh);
                }
            });
        }
        // drop a stack cell
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_remove_stack(move |i| {
                if let Some(app) = w.upgrade() {
                    {
                        let mut s = sh.borrow_mut();
                        let i = i as usize;
                        if i < s.light_layers.len() {
                            s.light_layers.remove(i);
                            // removing a layer BELOW the selection shifts it down one; removing the
                            // selection (or above) only needs a clamp. The pure helper does both — and
                            // returns 0 for an emptied stack — so the tracked selection can't drift one
                            // layer too high (the old bare clamp missed the below-the-selection case).
                            s.selected_layer = neuron::pattern::selection_after_remove(
                                i,
                                s.selected_layer,
                                s.light_layers.len(),
                            );
                        }
                        s.layers_rev += 1;
                    }
                    refresh_layers(&app, &sh);
                    flush_lighting_save(); // removing a layer is structural — persist it immediately
                }
            });
        }
        // toggle the PAINT brush — picking it up snapshots the current frame so painting starts from
        // what's lit (not a blank board); putting it down returns to the effect preview.
        {
            let w = app.as_weak();
            let _sh = sh.clone();
            app.global::<State>().on_toggle_brush(move || {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    let on = !st.get_light_brush_on();
                    st.set_light_brush_on(on);
                    // the brush and the PLACE gesture are exclusive render modes — picking up one drops
                    // the other so the render never has two live pointer gestures fighting.
                    if on {
                        st.set_light_place_on(false);
                    }
                    st.set_status_line(
                        if on {
                            "brush picked up — paint on the render"
                        } else {
                            "brush down — back to the effect preview"
                        }
                        .into(),
                    );
                }
            });
        }
        // toggle the PLACE gesture — a sibling to the brush (mutually exclusive). On = a drag on the
        // render defines the selected layer's region; off = normal. Entering place drops the brush.
        {
            let w = app.as_weak();
            let _sh = sh.clone();
            app.global::<State>().on_toggle_place(move || {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    let on = !st.get_light_place_on();
                    st.set_light_place_on(on);
                    if on {
                        st.set_light_brush_on(false);
                    }
                    st.set_status_line(
                        if on {
                            "place mode — drag a rectangle to set this layer's area"
                        } else {
                            "done placing"
                        }
                        .into(),
                    );
                }
            });
        }
        // set-layer-region(r0,c0,r1,c1): the PLACE gesture's commit — the dragged rectangle (inclusive
        // corner cells, already normalised by the render) becomes the SELECTED layer's region. Compute the
        // row-major indices the compositor masks by, write them onto the layer, then re-project + persist
        // so the placement composites live, shows in the STACK badge, and survives relaunch. A rect that
        // covers the WHOLE board stores an EMPTY region — the canonical "full board" the compositor treats
        // as region-less — so a full-board drag and a Reset converge honestly.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_set_layer_region(move |r0, c0, r1, c1| {
                if let Some(app) = w.upgrade() {
                    let committed = {
                        let mut s = sh.borrow_mut();
                        let (rows, cols) = s.rt.grid_dims();
                        let sel = s.selected_layer;
                        if rows == 0 || cols == 0 || sel >= s.light_layers.len() {
                            None
                        } else {
                            // the pure core does the clamp + order + whole-board→empty math; an empty
                            // result is the canonical full-board (region-less) placement.
                            let region = neuron::pattern::region_from_rect(r0, c0, r1, c1, rows, cols);
                            let full = region.is_empty();
                            // the placed block's extent for the status readout is the region's bbox (the
                            // whole board when the region is empty / full-board).
                            let bbox = neuron::pattern::Bounds::from_region(&region, rows, cols);
                            s.light_layers[sel].region = region;
                            s.layers_rev += 1;
                            Some((bbox.rows as i32, bbox.cols as i32, full))
                        }
                    };
                    if let Some((rext, cext, full)) = committed {
                        refresh_layers(&app, &sh);
                        flush_lighting_save(); // placement is structural — persist immediately
                        app.global::<State>().set_status_line(
                            if full {
                                "layer placed on the full board".to_string()
                            } else {
                                format!("layer placed · {rext}×{cext} block")
                            }
                            .into(),
                        );
                    }
                }
            });
        }
        // reset-layer-region: clear the SELECTED layer's region → it fills the whole board again.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_reset_layer_region(move || {
                if let Some(app) = w.upgrade() {
                    let cleared = {
                        let mut s = sh.borrow_mut();
                        let sel = s.selected_layer;
                        // clear only when there IS a layer AND it currently has a region (else no-op).
                        let should = s
                            .light_layers
                            .get(sel)
                            .map(|d| !d.region.is_empty())
                            .unwrap_or(false);
                        if should {
                            s.light_layers[sel].region.clear();
                            s.layers_rev += 1;
                        }
                        should
                    };
                    if cleared {
                        refresh_layers(&app, &sh);
                        flush_lighting_save();
                        app.global::<State>()
                            .set_status_line("placement reset — full board".into());
                    }
                }
            });
        }
        // "push" the painted frame — a pure COMMIT now, not a device write. The painted grid becomes the
        // one `custom` layer; auto-apply (refresh_layers → schedule_lighting_apply) streams it to the board
        // as a StaticFrame — the identical pixels — so the old one-shot `rt.push_frame` write AND the
        // explicit stop_animation it needed are gone (the stream renders the committed frame; nothing to
        // race against). Kept: the user-facing "committed" feedback + clearing any stale effect accent.
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_push_painted(move || {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    let model = st.get_grid_px();
                    let mut frame = Vec::with_capacity(model.row_count());
                    for c in model.iter() {
                        frame.push(Rgb::new(c.red(), c.green(), c.blue()));
                    }
                    // the painted frame is a first-class `custom` layer (survives relaunch, rides a captured
                    // profile); committing COLLAPSES the stack to it (a full opaque frame occludes beneath).
                    commit_custom_layer(&sh, &frame);
                    st.set_applied_effect(-1); // the board now shows the custom frame, not an indexed effect
                    // re-project (gallery drops any stale tile highlight, STACK reads "single") — and the
                    // auto-apply hooked in refresh_layers streams the committed frame live to the device.
                    refresh_layers(&app, &sh);
                    st.set_status_line("frame committed — streaming to the board".into());
                }
            });
        }
        // re-render the live tile grid each tick (the MATERIAL-card animation, applied to lighting)
        {
            let w = app.as_weak();
            let sh = sh.clone();
            app.global::<State>().on_tile_preview_tick(move || {
                if let Some(app) = w.upgrade() {
                    let t = neuron::pattern::render_elapsed();
                    render_light_tiles(&app, &sh, t);
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
                // …and any OPEN inline rule editor: a reload rebuilds the rules model from disk and
                // can reindex the rows, so a surviving `editing-rule` would make the next commit write
                // into the wrong slot. Closing it (–1) makes a stale commit a safe no-op.
                st.set_editing_rule(-1);
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
                        // removing a row shifts the editable tail — close the inline editor so a stale
                        // `editing-rule` can't commit into the wrong slot after the reindex.
                        st.set_editing_rule(-1);
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
                        st.set_editing_rule(-1); // close the inline editor — the hyper list reindexed
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_status_line("binding removed".into());
                    }
                    Err(e) => st.set_status_line(format!("remove failed: {e}").into()),
                }
            }
        });
    });
    // REORDER a GUI-authored BASE rule up/down. Same display→tier mapping as remove: the editable
    // rows are the LAST `editable_count` of the base list, so subtract the read-only provenance count.
    // Provenance (toml/cast) rows aren't reorderable — they stay anchored above the editable tail.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_move_rule(move |row, dir| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let total = st.get_rules().row_count() as i32;
                let editable = st.get_editable_count();
                let first_editable = total - editable;
                if row < first_editable {
                    return; // a provenance row — not reorderable here
                }
                let gui_idx = (row - first_editable) as usize;
                match crate::editor::move_gui_rule_in_tier(gui_idx, false, dir) {
                    Ok(()) => {
                        st.set_editing_rule(-1); // reordering shifts indices — close the inline editor
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                    }
                    Err(e) => st.set_status_line(format!("reorder failed: {e}").into()),
                }
            }
        });
    });
    // REORDER a GUI-authored HYPERSHIFT rule. Every hyper row is GUI-authored (AppRuntime contributes
    // no hyper provenance rows), so the row index IS the tier index — no provenance offset.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_move_hyper_rule(move |row, dir| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match crate::editor::move_gui_rule_in_tier(row.max(0) as usize, true, dir) {
                    Ok(()) => {
                        st.set_editing_rule(-1); // reordering shifts indices — close the inline editor
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                    }
                    Err(e) => st.set_status_line(format!("reorder failed: {e}").into()),
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
                            // a friendly, layout-independent control name ("F13", "Button 4",
                            // "Left Ctrl") — control_label falls back to an exact hex id for anything
                            // unmapped, so a weird controller still shows something legible.
                            let label = neuron::controls::control_label(c.page, c.usage);
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
    // EDIT an existing GUI-authored rule IN PLACE — clicking a removable row opens the SAME wire-editor
    // inline, seeded from its real trigger + action (edit-shows-current). Only the GUI-authored tail is
    // editable; a toml/cast row says where it lives and bails. Mirrors the radial sector editor
    // (preset_picker + an editing sentinel + a commit verb), and closes every other shared-picker editor.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_begin_edit_rule(move |row, hyper| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // map the visible row -> the n-th GUI rule of its tier. BASE: the editable tail (toml
                // rows lead); HYPER: every listed row is GUI-authored, so the row index IS n.
                let n = if hyper {
                    row.max(0) as usize
                } else {
                    let total = st.get_rules().row_count() as i32;
                    let first_editable = total - st.get_editable_count();
                    if row < first_editable {
                        st.set_status_line(
                            "that rule comes from bindings.toml / cast.toml — edit it there".into(),
                        );
                        return;
                    }
                    (row - first_editable) as usize
                };
                let Some(rule) = crate::editor::gui_rule_in_tier(n, hyper) else {
                    st.set_status_line("couldn't find that binding to edit".into());
                    return;
                };
                // seed the action picker from the rule's real action (clears any prior edit's leftovers).
                preset_picker(&st, &rule.action);
                // seed the trigger end: an Input trigger is re-capturable (REBIND), so stash it as the
                // captured control + show its friendly name; anything else shows its describe() readout.
                let label = match &rule.trigger {
                    neuron::engine::Trigger::Input { page, usage, pid } => {
                        CAPTURED_CONTROL.with(|cell| {
                            *cell.borrow_mut() = Some(crate::capture::CapturedControl {
                                page: *page,
                                usage: *usage,
                                pid: *pid,
                            })
                        });
                        neuron::controls::control_label(*page, *usage)
                    }
                    other => {
                        CAPTURED_CONTROL.with(|cell| *cell.borrow_mut() = None);
                        other.describe()
                    }
                };
                st.set_bind_trigger_label(label.into());
                st.set_bind_trigger_ready(true);
                // one editor owns the shared picker at a time — close the wedge / glyph / rhythm editors.
                st.set_editing_sector(-1);
                st.set_gesture_bind_target("".into());
                st.set_rhythm_bind_target(-1);
                st.set_editing_rule(row);
                st.set_editing_rule_hyper(hyper);
                st.set_status_line(
                    "editing binding — change the action (or REBIND), then Save".into(),
                );
            }
        });
    });
    // COMMIT the in-place edit: validate + build the chosen action, keep (or re-capture) the trigger,
    // overwrite that exact rule, reload the live engine. Same front-door validation as add-binding.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_commit_edit_rule(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let row = st.get_editing_rule();
                if row < 0 {
                    return;
                }
                let hyper = st.get_editing_rule_hyper();
                let n = if hyper {
                    row.max(0) as usize
                } else {
                    let total = st.get_rules().row_count() as i32;
                    let first_editable = total - st.get_editable_count();
                    (row - first_editable).max(0) as usize
                };
                let (id, param) = current_action(&st);
                if let Err(e) = crate::editor::validate_action(&id, &param) {
                    st.set_status_line(e.into());
                    return;
                }
                let action = crate::editor::build_action(&id, &param);
                // the trigger: a (re)captured Input control wins; otherwise keep the rule's existing one.
                let trigger = match CAPTURED_CONTROL.with(|cell| *cell.borrow()) {
                    Some(c) => neuron::engine::Trigger::Input {
                        page: c.page,
                        usage: c.usage,
                        pid: c.pid,
                    },
                    None => match crate::editor::gui_rule_in_tier(n, hyper) {
                        Some(r) => r.trigger,
                        None => {
                            st.set_status_line("that binding is gone — reload and try again".into());
                            return;
                        }
                    },
                };
                match crate::editor::edit_gui_rule_in_tier(n, hyper, trigger, action) {
                    Ok(()) => {
                        sh.borrow_mut().rt.bindings = neuron::bindings::Bindings::load();
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        CAPTURED_CONTROL.with(|cell| *cell.borrow_mut() = None);
                        st.set_editing_rule(-1);
                        st.set_bind_trigger_ready(false);
                        st.set_bind_trigger_label("—".into());
                        st.set_status_line("binding updated — live now".into());
                    }
                    Err(e) => st.set_status_line(format!("update failed: {e}").into()),
                }
            }
        });
    });
    // CANCEL the in-place edit — close the inline editor, drop the captured trigger, leave the rule be.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_cancel_edit_rule(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                CAPTURED_CONTROL.with(|cell| *cell.borrow_mut() = None);
                st.set_editing_rule(-1);
                st.set_bind_trigger_ready(false);
                st.set_bind_trigger_label("—".into());
                st.set_status_line("edit cancelled".into());
            }
        });
    });
    // PRESS-TO-BIND the HyperShift HOLD KEY — the control you hold to reach the second layer. On
    // capture it's written as a Noop activator on the hypershift layer (no action of its own).
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_capture_hypershift_hold(move || {
            if let Some(app) = w.upgrade() {
                let sh = sh.clone();
                crate::capture::begin_control(&app, move |app, captured| {
                    let st = app.global::<State>();
                    match captured {
                        Some(c) => {
                            let trigger = neuron::engine::Trigger::Input {
                                page: c.page,
                                usage: c.usage,
                                pid: c.pid,
                            };
                            match crate::editor::set_hypershift_hold(trigger) {
                                Ok(()) => {
                                    sh.borrow_mut().rt.bindings = neuron::bindings::Bindings::load();
                                    refresh_rules(&app, &sh);
                                    crate::dispatch::request_reload();
                                    st.set_status_line(
                                        "HyperShift hold key set — hold it to reach the second layer"
                                            .into(),
                                    );
                                }
                                Err(e) => st.set_status_line(format!("failed: {e}").into()),
                            }
                        }
                        None => st.set_status_line("capture cancelled".into()),
                    }
                });
            }
        });
    });
    // CLEAR the HyperShift hold key.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_clear_hypershift_hold(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // symmetric with SET: a failed clear (disk write) must SAY so, never report success on a
                // swallowed error — the in-memory reload still happens so the live rule is honest either way.
                match crate::editor::clear_hypershift_hold() {
                    Ok(()) => {
                        sh.borrow_mut().rt.bindings = neuron::bindings::Bindings::load();
                        refresh_rules(&app, &sh);
                        crate::dispatch::request_reload();
                        st.set_status_line("HyperShift hold key cleared".into());
                    }
                    Err(e) => st.set_status_line(format!("failed: {e}").into()),
                }
            }
        });
    });
    // LIVE AUDIO METERS — open the mic + out meters when the DIRECT page shows (set-audio-metering),
    // then poll peaks ~30fps (poll-audio-levels). Each poll is two cheap GetPeakValue reads; the
    // ballistics (attack/release, peak-hold, clip latch) live in MeterChan.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_audio_metering(move |on| {
            if w.upgrade().is_none() {
                return;
            }
            AUDIO_METERS.with(|cell| {
                if on {
                    let mic = neuron::audio::resolve_capture(None)
                        .and_then(|e| neuron::audio::MeterCtl::open(&e.id));
                    let out = neuron::audio::resolve_render(None)
                        .and_then(|e| neuron::audio::MeterCtl::open(&e.id));
                    *cell.borrow_mut() = Some(AudioMeters {
                        mic,
                        out,
                        ..Default::default()
                    });
                } else {
                    *cell.borrow_mut() = None;
                }
            });
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_poll_audio_levels(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let muted_mic = st.get_mic_muted();
                let muted_out = st.get_out_muted();
                AUDIO_METERS.with(|cell| {
                    let mut g = cell.borrow_mut();
                    let Some(m) = g.as_mut() else {
                        return;
                    };
                    let raw_mic = if muted_mic {
                        0.0
                    } else {
                        m.mic.as_ref().map(|x| x.peak()).unwrap_or(0.0)
                    };
                    let raw_out = if muted_out {
                        0.0
                    } else {
                        m.out.as_ref().map(|x| x.peak()).unwrap_or(0.0)
                    };
                    m.cm.update(raw_mic);
                    m.co.update(raw_out);
                    st.set_mic_level(m.cm.level);
                    st.set_mic_hold(m.cm.hold);
                    st.set_mic_clip(m.cm.clip());
                    st.set_out_level(m.co.level);
                    st.set_out_hold(m.co.hold);
                    st.set_out_clip(m.co.clip());
                });
            }
        });
    });

    // ── Spellweaving ─────────────────────────────────────────────────────
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_record_gesture(move || {
            if let Some(app) = w.upgrade() {
                // Shift+Record = strokelab: capture a FULL rich stroke → research file (no vault
                // entry), instead of teaching a gesture. VK_SHIFT (0x10) read at click time.
                if neuron::glyph::key_down(0x10) {
                    record_rich_stroke(&app, &sh);
                } else {
                    record_gesture(&app, &sh);
                }
            }
        });
    });
    // live Shift probe for the weave page's rich-mode preview — a ~8Hz Timer on that page asks this
    // whether Shift is physically down, so the Record button can morph to "rich → file" before a click.
    bind(app, &shared, |app, _sh| {
        app.global::<State>()
            .on_poll_shift(|| neuron::glyph::key_down(0x10));
    });
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_clear_gestures(move || {
            if let Some(app) = w.upgrade() {
                let (n, err) = {
                    let mut s = sh.borrow_mut();
                    s.rt.vault.templates.clear();
                    // capture the FIRST save error instead of swallowing it — the clear applies live either
                    // way, but the status line must not claim success on a disk write that failed.
                    let mut err = s.rt.vault.save().err();
                    // a cleared vault orphans every glyph→action binding: phantom rules for
                    // glyphs that can never be recognized again — and a future re-recorded
                    // "glyph_1" would silently inherit a stale action. Clear them together.
                    let n = s.rt.cast.gestures.len();
                    s.rt.cast.gestures.clear();
                    if let Err(e) = crate::editor::save_cast(&s.rt.cast) {
                        err.get_or_insert(e);
                    }
                    (n, err)
                };
                refresh_gestures(&app, &sh);
                refresh_rules(&app, &sh);
                crate::dispatch::request_reload();
                let st = app.global::<State>();
                st.set_gesture_bind_target("".into()); // the bind card can't target a dead glyph
                match err {
                    Some(e) => st.set_status_line(format!("gesture vault clear failed: {e}").into()),
                    None => st.set_status_line(
                        format!("gesture vault cleared ({n} glyph binding(s) removed)").into(),
                    ),
                }
            }
        });
    });
    // delete ONE glyph — template + its cast binding, in one move.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_delete_gesture(move |name| {
            if let Some(app) = w.upgrade() {
                // capture the FIRST save error rather than swallow it — the delete applies live either way,
                // but the status line must not report success on a disk write that failed.
                let err = {
                    let mut s = sh.borrow_mut();
                    s.rt.vault.templates.retain(|t| t.name != name.as_str());
                    let mut err = s.rt.vault.save().err();
                    s.rt.cast.gestures.remove(name.as_str());
                    if let Err(e) = crate::editor::save_cast(&s.rt.cast) {
                        err.get_or_insert(e);
                    }
                    err
                };
                refresh_gestures(&app, &sh);
                refresh_rules(&app, &sh);
                crate::dispatch::request_reload();
                let st = app.global::<State>();
                if st.get_gesture_bind_target() == name {
                    st.set_gesture_bind_target("".into());
                }
                match err {
                    Some(e) => st.set_status_line(format!("glyph '{name}' delete failed: {e}").into()),
                    None => st.set_status_line(format!("glyph '{name}' deleted").into()),
                }
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
                        // the rename applies live regardless; but fold a failed disk write into `res` so
                        // the status line reports it instead of falsely claiming the rename was saved.
                        let mut r = s.rt.vault.save().map_err(|e| format!("rename save failed: {e}"));
                        if let Some(a) = s.rt.cast.gestures.remove(old.as_str()) {
                            s.rt.cast.gestures.insert(new.to_string(), a);
                            if let Err(e) = crate::editor::save_cast(&s.rt.cast) {
                                if r.is_ok() {
                                    r = Err(format!("rename save failed: {e}"));
                                }
                            }
                        }
                        r
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
                // FREE the device before apply: a live lighting stream holds the keyboard open and
                // writes it every frame, so apply's own device writes (brightness/DPI/…) fight it —
                // two writers stall each other and apply blows its deadline. Snapshot the current
                // look, stop the stream(s), then apply on a quiet device; lighting restarts after.
                let prev_lighting = with_shared_ret(|sh| {
                    let s = sh.borrow();
                    let prev = s.light_layers.clone();
                    for a in s.rt.anim.values() {
                        a.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    prev
                })
                .unwrap_or_default();
                let back = app.as_weak();
                std::thread::spawn(move || {
                    // let the anim thread(s) notice the stop and release the device handle first.
                    std::thread::sleep(std::time::Duration::from_millis(350));
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
                                    st.set_disable_alt_esc(policy.disable_alt_esc);
                                    // switching profiles can swap which rules exist (the per-profile
                                    // sidecar set) — a row index held open in the inline editor may not
                                    // survive, so close it rather than let it seed from a stale row.
                                    st.set_editing_rule(-1);
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
                                        // Restart lighting on the now-freed device: the profile's stack
                                        // if it sets one, else the look that was streaming before apply —
                                        // so an empty-lighting profile keeps the current look (not dark)
                                        // and the stream we stopped for the apply always comes back.
                                        let stack = neuron::profile::Profile::load(&applied.name)
                                            .map(|p| p.lighting)
                                            .ok()
                                            .filter(|l| !l.is_empty())
                                            .unwrap_or_else(|| prev_lighting.clone());
                                        if !stack.is_empty() {
                                            {
                                                let mut s = sh.borrow_mut();
                                                s.light_layers = stack;
                                                s.selected_layer =
                                                    s.light_layers.len().saturating_sub(1);
                                                s.active_frame = 0;
                                                s.layers_rev += 1;
                                            }
                                            st.set_light_paint_mode(false);
                                            // suppress the edit-debounce: this path streams EXPLICITLY just
                                            // below, so a redundant 250ms re-stream on top of it is wasteful.
                                            {
                                                let _suppress = SuppressApply::new();
                                                refresh_layers(&app, sh); // project + persist
                                            }
                                            flush_lighting_save(); // structural — persist now
                                            let _ = apply_current_lighting(&app, sh); // stream live
                                        }
                                    });
                                    st.set_status_line(applied.summary.into());
                                }
                                Err(e) => {
                                    // apply failed — bring back the look that was streaming before, so a
                                    // failed apply never leaves the board frozen with its stream stopped.
                                    if !prev_lighting.is_empty() {
                                        with_shared(|sh| {
                                            {
                                                let mut s = sh.borrow_mut();
                                                s.light_layers = prev_lighting.clone();
                                                s.selected_layer =
                                                    s.light_layers.len().saturating_sub(1);
                                                s.layers_rev += 1;
                                            }
                                            let _ = apply_current_lighting(&app, sh);
                                        });
                                    }
                                    st.set_status_line(e.into());
                                }
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
                    // the captured profile carries the LIVE compositor stack (core can't read a
                    // LayerDef stack back from the device's effect registers).
                    let lighting = sh.borrow().light_layers.clone();
                    let mut s = sh.borrow_mut();
                    s.rt.persist = st.get_persist_to_onboard();
                    s.rt.save_profile_from_devices(name.as_str(), dpi, hz, br, lighting)
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
    // recompute the zero-typing suggestion from LIVE state whenever the profiles sheet opens — so a
    // DPI/effect tuned since the last focus-change (its only other refresh point) isn't shown stale.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_refresh_profile_suggestion(move || {
            if let Some(app) = w.upgrade() {
                let focused = app.global::<State>().get_focused_app().to_string();
                refresh_profile_suggestion(&app, &focused);
            }
        });
    });
    // live answer for "does this save-name already exist?" (overwrite, said before the click)
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_profile_name_edited(move |name| {
            if let Some(app) = w.upgrade() {
                // Collide on the actual on-disk KEY (sanitized + case-folded), NOT the raw name —
                // `Profile::path` sanitizes non-[A-Za-z0-9-_] → `_` and Windows is case-insensitive,
                // so "my game/2" ≡ "my_game_2" and "Valorant" ≡ "valorant" all hit the same .toml.
                // Comparing raw names lets the button read "capture" while the save clobbers a file.
                let key = neuron::profile::Profile::file_key(name.as_str());
                let exists = sh
                    .borrow()
                    .rt
                    .profiles
                    .iter()
                    .any(|p| neuron::profile::Profile::file_key(&p.name) == key);
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
    // REFRESH MACRO BLOCKS: parse the editor source into the flat node-blocks model behind the visual
    // constructor. Mirrors `on_macro_check`'s structure — `parse_macro` blocks up to FIRE_BUDGET so it
    // runs OFF the UI thread and posts back via `invoke_from_event_loop`. A SYNTAX error KEEPS the last
    // good blocks (the canvas "works with errors" — it never blanks mid-type) and shows "line N: msg"
    // above them; a HOST error (no python / pipe broken) shows a gentle line. Empty/whitespace source
    // clears the blocks with no error and no thread spawn.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_refresh_macro_blocks(move |src| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let source = src.to_string();
                // an empty editor has no steps and no error — reset the tree to empty and flatten it,
                // which emits just the root +add invitation (the canvas's "build it here" entry point).
                // No parse, no thread.
                if source.trim().is_empty() {
                    with_shared(|sh| sh.borrow_mut().macro_tree.clear());
                    let mut blocks = Vec::new();
                    flatten_macro(&[], 0, "", &mut blocks);
                    st.set_macro_blocks(ModelRc::new(VecModel::from(blocks)));
                    st.set_macro_parse_error("".into());
                    st.set_macro_parsing(false);
                    return;
                }
                // one parse in flight at a time — a fast typist would otherwise stack worker threads.
                if st.get_macro_parsing() {
                    return;
                }
                st.set_macro_parsing(true);
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let result = neuron::macros::parse_macro(&source);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            st.set_macro_parsing(false);
                            match result {
                                Ok(nodes) => {
                                    let mut blocks = Vec::new();
                                    flatten_macro(&nodes, 0, "", &mut blocks);
                                    st.set_macro_blocks(ModelRc::new(VecModel::from(blocks)));
                                    st.set_macro_parse_error("".into());
                                    // RESEED the working tree from the parsed source so canvas edits
                                    // build ON what the code view holds (the source is the truth a
                                    // user just typed; the canvas must inherit it, not stomp it).
                                    with_shared(|sh| sh.borrow_mut().macro_tree = nodes.clone());
                                }
                                // KEEP the last blocks — the canvas stays readable while you fix the line.
                                Err(neuron::macros::ParseError::Syntax { line, msg }) => {
                                    st.set_macro_parse_error(
                                        format!("line {line}: {}", first_line(&msg)).into(),
                                    );
                                }
                                Err(neuron::macros::ParseError::Host { msg }) => {
                                    st.set_macro_parse_error(
                                        format!("couldn't read the blocks: {}", first_line(&msg))
                                            .into(),
                                    );
                                }
                            }
                        }
                    });
                });
            }
        });
    });
    // ── CANVAS EDIT-OPS — the direct-manipulation builder. Each mutates the working tree (the source
    // of truth in Shared), then regenerates `macro-source` + re-flattens `macro-blocks` synchronously
    // (pure Rust, no Python). The canvas updates instantly and the code view stays in lockstep. ──

    // EDIT-STEP: set the addressed node's editable param from `value`. For TEXT-ish Value params
    // (type/copy/notify text, open command, focus window) the field IS the string, so set a `Str`
    // literal. For EXPRESSION Value params (cond/count/source/ms/scroll/coords) the user types a
    // Python expr, so set a `Raw` verbatim — the next code-parse normalizes it into the typed Value
    // shape. Non-Value params keep sensible setters (press splits on +/,/space; click/key take the
    // name; raw is verbatim). A path that doesn't resolve (a stale row mid-reflow) is a no-op — never
    // a panic.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_edit_step(move |path, value| {
            if let Some(app) = w.upgrade() {
                let mut shared = sh.borrow_mut();
                if let Some(node) = node_at_path(&mut shared.macro_tree, &path) {
                    let v = value.to_string();
                    edit_node_value(node, v);
                    let tree = shared.macro_tree.clone();
                    drop(shared);
                    // VALUE-ONLY: refresh the source (the code view + has-ask), but DON'T re-flatten —
                    // re-setting `macro-blocks` would rebuild this field and steal its caret/focus. The
                    // field already shows the typed text; the model re-reads the tree on the next
                    // structural edit. This is the commit-on-blur + no-cursor-jank fix.
                    regenerate_source_only(&app.global::<State>(), &tree);
                }
            }
        });
    });
    // ADD-STEP: append a fresh default node of `kind` to the body at the insert context ("" root, or
    // "<askpath>.yes"/".no"). A fresh `ask` brings its own empty branches (each with its own +add).
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_add_step(move |insert, kind| {
            if let Some(app) = w.upgrade() {
                let Some(node) = default_node(&kind) else {
                    return;
                };
                let mut shared = sh.borrow_mut();
                if let Some(body) = body_at_context(&mut shared.macro_tree, &insert) {
                    body.push(node);
                    // the new node's PATH — the focus-on-add target. Root context ("") makes a bare
                    // index ("3"); a branch context ("1.yes") makes "1.yes.<idx>". For an `ask` this is
                    // its own path, whose step field is the question — exactly what we want focused.
                    let new_idx = body.len() - 1;
                    let focus_path = if insert.is_empty() {
                        new_idx.to_string()
                    } else {
                        format!("{insert}.{new_idx}")
                    };
                    let tree = shared.macro_tree.clone();
                    drop(shared);
                    let st = app.global::<State>();
                    // re-flatten FIRST (structure changed → the new row must exist), THEN arm the focus
                    // target so the freshly-rendered field's `init` finds it and grabs the caret. A
                    // `raw` (code) step renders no inline field — it's edited in the code view — so skip
                    // arming focus for it (nothing to focus; leaving the target set would be untidy).
                    regenerate_from_tree(&st, &tree);
                    if kind != "raw" {
                        st.set_macro_focus_path(focus_path.into());
                    }
                }
            }
        });
    });
    // DELETE-STEP: remove the addressed node (an ask takes its whole yes/no subtree with it).
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_delete_step(move |path| {
            if let Some(app) = w.upgrade() {
                let mut shared = sh.borrow_mut();
                if let Some((body, idx)) = parent_body_and_index(&mut shared.macro_tree, &path) {
                    body.remove(idx);
                    let tree = shared.macro_tree.clone();
                    drop(shared);
                    regenerate_from_tree(&app.global::<State>(), &tree);
                }
            }
        });
    });
    // MOVE-STEP: reorder a node within its OWN body (dir -1 up / +1 down), clamped at the ends.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_move_step(move |path, dir| {
            if let Some(app) = w.upgrade() {
                let mut shared = sh.borrow_mut();
                if let Some((body, idx)) = parent_body_and_index(&mut shared.macro_tree, &path) {
                    let target = idx as i32 + dir;
                    if target >= 0 && (target as usize) < body.len() {
                        body.swap(idx, target as usize);
                        let tree = shared.macro_tree.clone();
                        drop(shared);
                        regenerate_from_tree(&app.global::<State>(), &tree);
                    }
                }
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
                            // and its name/summary/steps may have changed — refresh the catalog too.
                            refresh_macro_catalog(&app);
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
                // SURFACE the result — a silent button is the worst UX. If the bundled python
                // runtime can't be materialized (a rare IO failure), say so plainly.
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
    // TEST ASKS (macro editor): the SAFE, learn-by-doing companion to "test run". Registers the LIVE
    // editor source + fire_mock so ONLY its neuron.ask() beacons rise (input forced disarmed — no real
    // effects), reusing the EXACT SYSTEM→BEACONS pipeline. A curious user discovers the ask/beacon
    // system by seeing it: write an ask, click, watch the strip rise, answer it. No explanatory wall.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_test_asks(move |name, source| {
            let source = source.to_string();
            // belt-and-suspenders: the button only shows once an ask exists, but guard regardless.
            if !source.contains("neuron.ask") {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_macro_status(
                        "add a neuron.ask(\"…\") (the 'ask' chip), then Test asks previews its beacon".into(),
                    );
                }
                return;
            }
            let id = {
                let n = name.trim();
                if n.is_empty() { "draft".to_string() } else { n.to_string() }
            };
            // ONE test beacon at a time — shares the serial-sidecar gate with the SYSTEM panel's test.
            if crate::beacon::TEST_BEACON_INFLIGHT.swap(true, std::sync::atomic::Ordering::SeqCst) {
                if let Some(app) = w.upgrade() {
                    app.global::<State>().set_status_line(
                        "a test beacon is already waiting — answer it before testing another".into(),
                    );
                }
                return;
            }
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_macro_status(
                    "previewing your asks — answer the beacon at your cursor (nothing real runs)".into(),
                );
            }
            let back = w.clone();
            std::thread::spawn(move || {
                let host = neuron::macros::macro_host();
                let _ = host.register(&id, &source);
                let ctx = neuron::macros::Context::capture();
                let result = host.fire_mock(&id, &ctx);
                // a failed dispatch raises no beacon, so release the shared gate here (else it'd stay
                // locked until the next real beacon clears it).
                if !result.contains("dispatched") {
                    crate::beacon::TEST_BEACON_INFLIGHT
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = back.upgrade() {
                        app.global::<State>()
                            .set_macro_status(format!("ask preview \u{00b7} {result}").into());
                    }
                });
            });
        });
    });
    // editor source changed -> recompute whether it contains an ask (gates the self-surfacing "test
    // asks" button). The check lives here in Rust because Slint 1.16 has no string.contains.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_note_macro_source(move |s| {
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_macro_has_ask(s.contains("neuron.ask"));
            }
        });
    });
    // REVEAL the macros folder in the OS file manager (created if missing) — the "drop a .py here"
    // affordance. Cross-platform via open_in_file_manager (explorer / open / xdg-open).
    // REVEAL the strokelab output folder — clicking a "saved strokes/…" line opens ./strokes/.
    bind(app, &shared, |app, _sh| {
        app.global::<State>()
            .on_reveal_strokes(reveal_strokes_folder);
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_reveal_macros(move || {
            reveal_macros_folder();
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_macro_status(
                    "opened the macros folder \u{2014} drop .py files in, then Reload".into(),
                );
            }
        });
    });
    // OPEN a catalog macro INTO the editor (click-to-load): set name + source from disk and re-parse.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_open_macro(move |id| {
            if let Some(app) = w.upgrade() {
                load_macro_into_editor(&app, id.trim());
            }
        });
    });
    // DELETE from the WORKSHOP catalog card (the ✕, confirmed in the UI by a click-to-arm then a second
    // click): remove the macro's .py from disk, then refresh the catalog (the card vanishes) + the beacon
    // list. Same core path as the SYSTEM-panel `on_delete_beacon`, but it re-reads the WORKSHOP grid.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_delete_macro(move |id| {
            let id = id.trim().to_string();
            if id.is_empty() {
                return;
            }
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match neuron::macros::macro_host::delete_macro(&id) {
                    Ok(()) => {
                        refresh_macro_catalog(&app);
                        refresh_beacon_macros(&app);
                        st.set_macro_status(format!("deleted macro {id}").into());
                    }
                    Err(e) => {
                        st.set_macro_status(format!("delete failed: {e}").into());
                    }
                }
            }
        });
    });
    // RELOAD from disk — the "no janky live-update; press reload" path. Re-scans macros/scripts/,
    // registers every file (so dropped-in / externally-edited macros go live + bindable now), rebuilds
    // the catalog + the SYSTEM beacon list, and re-reads the OPEN macro from disk if it still exists.
    // The scan+register loop blocks up to FIRE_BUDGET per macro, so it runs off the UI thread.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_reload_macros(move || {
            if let Some(app) = w.upgrade() {
                app.global::<State>()
                    .set_macro_status("reloading macros from disk\u{2026}".into());
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let host = neuron::macros::macro_host();
                    let found = neuron::macros::macro_host::scan_macro_dir();
                    let count = found.len();
                    for (id, src) in &found {
                        let _ = host.register(id, src);
                    }
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = back.upgrade() {
                            let st = app.global::<State>();
                            refresh_macro_catalog(&app);
                            refresh_beacon_macros(&app);
                            // re-read the open macro from disk so external edits / a same-name drag-in
                            // show without a restart (it overwrites the status; set ours after).
                            let open = st.get_macro_name().to_string();
                            let open = open.trim();
                            if !open.is_empty()
                                && neuron::macros::macro_host::load_macro(open).is_some()
                            {
                                load_macro_into_editor(&app, open);
                            }
                            st.set_macro_status(
                                format!("reloaded {count} macro(s) from disk").into(),
                            );
                        }
                    });
                });
            }
        });
    });
    // DELETE BEACON (SYSTEM panel): remove a macro's .py from disk, then refresh the registry so the
    // row vanishes. The warm sidecar re-syncs from disk on its next spawn, so file removal is enough.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_delete_beacon(move |id| {
            let id = id.trim().to_string();
            if id.is_empty() {
                return;
            }
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match neuron::macros::macro_host::delete_macro(&id) {
                    Ok(()) => {
                        refresh_beacon_macros(&app);
                        st.set_status_line(format!("deleted macro {id}").into());
                    }
                    Err(e) => {
                        st.set_status_line(format!("delete failed: {e}").into());
                    }
                }
            }
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
                // Runtime on the worker with the selected pid+unit carried over is identical.
                let (pid, unit) = {
                    let s = sh.borrow();
                    (s.rt.selected_pid, s.rt.selected_unit.clone())
                };
                let back = app.as_weak();
                std::thread::spawn(move || {
                    let mut rt = AppRuntime::load();
                    rt.selected_pid = pid;
                    rt.selected_unit = unit;
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
                    // through "writes PAUSED" would make the gate a lie. Stop EVERY board's stream.
                    let stopped = paused && s.rt.any_animating();
                    if stopped {
                        s.rt.stop_all_animation();
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
                    st.set_compositing(false); // streams stopped — the board is no longer live
                }
                // AUTO-APPLY: un-pausing RESUMES the selected board's current stack once. Pausing stopped
                // the stream (above) and the manual apply button is gone, so this toggle is what brings
                // lighting back. A no-op on an empty stack; `apply_current_lighting` reads the now-armed
                // gate (which we just set). `paused` is the NEW state, so `!paused` is the pause→arm edge.
                // Resume EVERY configured board, not just the selected one — a global pause stopped them
                // all, so restoring only the on-screen board would leave the rest dark.
                if !paused {
                    reapply_all_boards(&app, &sh);
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
                let was_paused = neuron::writes::writes_paused(); // the gate BEFORE this stance move
                let stopped_anim = {
                    let mut s = sh.borrow_mut();
                    let stop = paused && s.rt.any_animating();
                    if stop {
                        s.rt.stop_all_animation();
                    }
                    stop
                };
                neuron::safety::set_mode(mode);
                neuron::macros::macro_host().set_armed(armed);
                if stopped_anim {
                    st.set_animating(false);
                    st.set_applied_effect(-1);
                    st.set_compositing(false); // streams stopped — the board is no longer live
                }
                st.set_writes_paused(paused);
                st.set_input_armed(armed);
                st.set_arm_stance(m);
                // AUTO-APPLY: crossing paused → armed RESUMES every configured board's stack once (parity
                // with the writes-pause toggle; the manual apply button is gone). A global stop cleared
                // them all, so restore them all, not just the selected one. No-op on empty stacks.
                if was_paused && !paused {
                    reapply_all_boards(&app, &sh);
                }
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
    // GUIDE — the in-app help popup's tooltip->sticky->X state machine. Content is data-driven in
    // Slint (GuideContent); the glue is thin. `toggle` owns open + active-id + the opening anchor;
    // `make-sticky` flips to the pinned window; `move`/`close` are one-liners. On-screen CLAMPING
    // lives in the popup (it has the window dims), so these just store the requested values.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_guide_toggle(move |id, ax, ay| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // same book, already open -> toggle it off
                if st.get_guide_open() && st.get_guide_active_id() == id {
                    st.set_guide_open(false);
                    return;
                }
                st.set_guide_active_id(id);
                st.set_guide_sticky(false); // opens as a click-away tooltip
                st.set_guide_x(ax); // anchored at the button; the popup clamps on-screen
                st.set_guide_y(ay);
                st.set_guide_open(true);
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_guide_make_sticky(move || {
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_guide_sticky(true);
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_guide_move(move |x, y| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                st.set_guide_x(x);
                st.set_guide_y(y);
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_guide_close(move || {
            if let Some(app) = w.upgrade() {
                app.global::<State>().set_guide_open(false);
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
    // CONNECTIONS — the protocol host. The master toggle brings the whole host up or down
    // (persisted), then RE-APPLIES the selected board's lighting so the pixels' OWNER follows
    // the switch immediately: the host's base layer on open, the app's own stream on close.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_host_enabled(move |v| {
            if let Some(app) = w.upgrade() {
                // Persist INTENT (`v`), never the outcome: the pref records what the user
                // chose; the toggle shows live truth (`refresh_host_status` reads `active()`).
                // Bring-up can fail TRANSIENTLY — the election lost to another neuron instance
                // (a restart overlap, a stray host_serve), a registry hiccup — and persisting
                // `active()` here silently flipped the opt-in back off, so the next launch
                // never retried. That contradicted the boot path, which deliberately preserves
                // the pref on a failed bring-up for exactly this reason (see `host::start`).
                let msg = crate::host::set_enabled(v);
                crate::prefs::set_host_enabled(v);
                refresh_host_status(&app);
                // MIGRATE EVERY board, not just the selected one: bring-up started a host writer for
                // every bridged device, so any board still on its own local stream would be double-
                // written until re-applied. Re-establishing all boards through `start_layers` stops
                // each local stream as it (re)claims via the host — and on teardown, resumes them all
                // locally. Selected-only would leave the rest racing (on) or dark (off).
                reapply_all_boards(&app, &sh);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    // Per-protocol gates: rebind/drop just that server while the host keeps running.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_host_chroma(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_host_chroma(v);
                crate::host::apply_protocol_prefs();
                refresh_host_status(&app);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_host_openrgb(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_host_openrgb(v);
                crate::host::apply_protocol_prefs();
                refresh_host_status(&app);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_host_obs(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_host_obs(v);
                crate::host::apply_protocol_prefs();
                refresh_host_status(&app);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    // OBS password — save it (masked; never echoed) and reconnect with the new secret so a
    // corrected password takes effect without toggling OBS off and on.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_host_obs_password(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_host_obs_password(&v);
                crate::host::reconnect_obs();
                refresh_host_status(&app);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    // WHO WINS — flip the base-layer band live: re-pin drops mis-banded base layers, then
    // re-applying the current stack re-claims at the new band (one code path). The truth strip
    // on LIGHTING updates on its own next poll.
    bind(app, &shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_host_lighting_wins(move |v| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_host_lighting_wins(v);
                crate::host::repin_policy();
                // repin_policy DROPPED every base layer whose band no longer matches the new policy —
                // across ALL boards — so re-claim them all at the new band, not just the selected one.
                reapply_all_boards(&app, &sh);
                let st = app.global::<State>();
                st.set_host_lighting_wins(crate::prefs::host_lighting_wins());
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
                let (x, y) = crate::prefs::notif_pos();
                st.set_notif_x(x);
                st.set_notif_y(y);
                st.set_status_line(msg.into());
            }
        });
    });
    // a free, hand-dragged spot from the mock-screen placer — Rust canonicalises an exact preset
    // spot back to its slug, anything else persists as "custom"; the resolved (x,y) flows back.
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_pos(move |x, y| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_pos(x, y);
                let st = app.global::<State>();
                st.set_notif_placement(crate::prefs::notif_placement().into());
                let (rx, ry) = crate::prefs::notif_pos();
                st.set_notif_x(rx);
                st.set_notif_y(ry);
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
                // one refresh re-reads every gate off prefs — no per-slug fan-out to drift
                refresh_ping_kinds(&app);
                app.global::<State>().set_status_line(msg.into());
            }
        });
    });
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_set_notif_stack(move |mode| {
            if let Some(app) = w.upgrade() {
                let msg = crate::prefs::set_notif_stack(mode.as_str());
                let st = app.global::<State>();
                st.set_notif_stack(crate::prefs::notif_stack().into());
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
                schedule_material_cards(&app);
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
                                    // capture a failed disk write instead of swallowing it — the rhythm
                                    // still applies live, but the status line must not claim it was saved.
                                    let err = with_shared_ret(|sh| {
                                        let mut s = sh.borrow_mut();
                                        s.rt.cast.activation = p.describe();
                                        crate::editor::save_cast(&s.rt.cast).err()
                                    })
                                    .flatten();
                                    sync_activation_view(&st, &p.describe());
                                    match err {
                                        Some(e) => st.set_status_line(
                                            format!("rhythm not saved: {e}").into(),
                                        ),
                                        None => st.set_status_line(
                                            format!(
                                                "rhythm recorded — weave opens on [{}]",
                                                p.describe()
                                            )
                                            .into(),
                                        ),
                                    }
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

    // ── FEEL timing windows (feel.toml) ───────────────────────────────────
    // Mirrors the stance picker exactly: load FeelConfig → set the three ms fields → save →
    // request_reload so the live engine adopts the new windows on its next tick. The three GUI
    // props are re-pinned to the persisted values so the sliders never drift from disk. (The only
    // clamp here is a 0 floor; the upper bound is enforced by the sliders' `maximum` in bindings.slint.)
    bind(app, &shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>()
            .on_set_feel_timing(move |hold_ms, gap_ms, coyote_ms| {
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    let mut cfg = neuron::feel::FeelConfig::load();
                    cfg.hold_ms = hold_ms.max(0) as u64;
                    cfg.gap_ms = gap_ms.max(0) as u64;
                    cfg.coyote_ms = coyote_ms.max(0) as u64;
                    match cfg.save() {
                        Ok(()) => {
                            st.set_feel_hold_ms(cfg.hold_ms as i32);
                            st.set_feel_gap_ms(cfg.gap_ms as i32);
                            st.set_feel_coyote_ms(cfg.coyote_ms as i32);
                            // the live engine adopts the new timing on its next tick.
                            crate::dispatch::request_reload();
                            st.set_status_line(
                                format!(
                                    "feel timing -> hold {}ms · gap {}ms · coyote {}ms",
                                    cfg.hold_ms, cfg.gap_ms, cfg.coyote_ms
                                )
                                .into(),
                            );
                        }
                        Err(e) => st.set_status_line(format!("feel timing not saved: {e}").into()),
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
                    // the trigger applies live regardless; a failed disk write must SAY so, not report ok.
                    let err = {
                        let mut s = sh2.borrow_mut();
                        s.rt.cast.trigger = vk;
                        crate::editor::save_cast(&s.rt.cast).err()
                    };
                    crate::dispatch::request_reload();
                    st.set_cast_trigger_label(name.into());
                    match err {
                        Some(e) => st.set_status_line(format!("cast trigger save failed: {e}").into()),
                        None => st.set_status_line(format!("cast trigger -> {name}").into()),
                    }
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
                st.set_editing_rule(-1); // …and any inline DIRECT rule edit
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
                st.set_editing_rule(-1); // …and any inline DIRECT rule edit
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
                st.set_editing_rule(-1); // …and any inline DIRECT rule edit
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

    // CONNECTIONS — seed the host card from prefs + the live port truth.
    refresh_host_status(app);
    // Seed the OBS password field ONCE, here at startup, from the SAVED secret only (never the
    // env override — an env-only password stays ephemeral, never surfaced or persisted). The
    // periodic `refresh_host_status` deliberately leaves the field alone thereafter, so clearing
    // it to remove/replace the secret isn't fought by the ~1s poller.
    if st.get_host_obs_password().is_empty() && !crate::prefs::host_obs_password_saved().is_empty() {
        st.set_host_obs_password(crate::prefs::host_obs_password_saved().into());
    }

    // NOTIFICATIONS — seed every control from the saved prefs.
    st.set_notif_enabled(crate::prefs::notif_enabled());
    st.set_notif_placement(crate::prefs::notif_placement().into());
    {
        let (x, y) = crate::prefs::notif_pos();
        st.set_notif_x(x);
        st.set_notif_y(y);
    }
    st.set_notif_audio(crate::prefs::notif_audio());
    st.set_notif_volume(crate::prefs::notif_volume());
    st.set_notif_sound(crate::prefs::notif_sound().into());
    st.set_notif_panel(crate::prefs::notif_panel());
    refresh_ping_kinds(app); // the WHAT-EARNS-A-PING cards, one model off Kind::ALL
    st.set_notif_stack(crate::prefs::notif_stack().into());

    // LAST, after the device is selected + every lighting control seeded: restore the selected board's
    // persisted effect/layer stack + fps and RE-APPLY it (start the stream) so it resumes its effect on
    // launch instead of holding its stale last frame. Also flips the save gate on for user edits.
    restore_lighting(app, &shared);

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
    // sniper read-back: reflect the authored RULE (gui.rules.toml) — never a fictional default.
    // Sniper is a held Action on the same spine as every other bind (a Trigger::Input -> a
    // precision DPI), so its readout comes from the one rule store, not a snowflake config.
    match crate::editor::sniper_binding() {
        Some((trigger, dpi)) => {
            if dpi != 0 {
                st.set_sniper_dpi(dpi as f32);
            }
            let label = match &trigger {
                neuron::engine::Trigger::Input { page, usage, .. } => {
                    neuron::controls::control_label(*page, *usage)
                }
                other => other.describe(),
            };
            st.set_sniper_button(label.into());
        }
        None => st.set_sniper_button("—".into()),
    }
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
    // LIFT-OFF DISTANCE: seed the level + readout from the device's live symmetric LOD (best-effort;
    // "—" on devices without it / asleep). debounce still has no derivable opcode -> unsupported.
    refresh_lod_readout(app, sh);
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
        app.global::<State>().on_apply_lod(move |lvl| {
            perf(&w, &sh, |rt| rt.apply_lift_off_distance(lvl.max(0) as u8));
            // the read-back line must show the device's NEW level (or its refusal).
            if let Some(app) = w.upgrade() {
                refresh_lod_readout(&app, &sh);
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_lod_async(move |lift, land| {
            perf(&w, &sh, |rt| {
                rt.apply_lift_off_asymmetric(lift.max(0) as u8, land.max(0) as u8)
            });
            // the read-back must show the device's NEW split pair (or its refusal / a snap to even).
            if let Some(app) = w.upgrade() {
                refresh_lod_readout(&app, &sh);
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
    // SNAP TAP (MECHANICAL ADVANTAGES) — perf-gated like the device writes. The UI passes the
    // post-toggle state; the runtime fires the verify-gated + env-gated `set_snap_tap` (honest
    // [gated] on a board that can't do it). Re-seed `snap-tap-enabled` from the device truth after.
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_set_snap_tap(move |enable| {
            perf(&w, &sh, |rt| rt.apply_snap_tap(enable));
            // reflect the device's real state: if the write was gated/refused, the toggle snaps back.
            if let Some(app) = w.upgrade() {
                refresh_snap_tap(&app, &sh);
            }
        });
    });
    bind(app, shared, |app, sh| {
        let w = app.as_weak();
        let sh = sh.clone();
        app.global::<State>().on_apply_gaming_mode(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let policy = neuron::writes::GamingMode::from_profile(
                    st.get_disable_alt_tab(),
                    st.get_disable_win(),
                    st.get_disable_alt_f4(),
                    st.get_disable_alt_esc(),
                );
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
    // sniper: bind the hold CONTROL by pressing it (the SAME press-to-bind every other trigger
    // uses — begin_control -> a Trigger::Input), authoring a held Action::Sniper rule on the shared
    // spine. So Left Alt, a thumb button, or a Naga side button are all first-class hold buttons.
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_capture_sniper_button(move || {
            if let Some(app) = w.upgrade() {
                crate::capture::begin_control(&app, move |app, captured| {
                    let st = app.global::<State>();
                    let Some(c) = captured else {
                        st.set_perf_status("sniper bind cancelled".into());
                        return;
                    };
                    let trigger = neuron::engine::Trigger::Input {
                        page: c.page,
                        usage: c.usage,
                        pid: c.pid,
                    };
                    let dpi = match st.get_sniper_dpi() as u16 {
                        0 => 400, // never author a drop-to-0; fall back to a sane precision default
                        d => d,
                    };
                    let name = neuron::controls::control_label(c.page, c.usage);
                    match crate::editor::set_sniper_button(trigger, dpi) {
                        Ok(()) => {
                            crate::dispatch::request_reload(); // the live worker adopts the sniper rule
                            st.set_sniper_button(name.clone().into());
                            st.set_sniper_dpi(dpi as f32);
                            st.set_perf_status(
                                format!("sniper armed — hold {name} for {dpi} DPI").into(),
                            );
                        }
                        Err(e) => st.set_perf_status(format!("sniper bind failed: {e}").into()),
                    }
                });
            }
        });
    });
    // sniper UNBIND — the split button's right half. Drops the rule from the spine; the live
    // worker's reload path restores any currently-held precision DPI (sniper_release_all) before
    // the rule vanishes, so an unbind mid-hold can't strand the mouse at 400.
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_unbind_sniper(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                match crate::editor::unbind_sniper() {
                    Ok(()) => {
                        crate::dispatch::request_reload();
                        st.set_sniper_button("\u{2014}".into());
                        st.set_perf_status("sniper unbound".into());
                    }
                    Err(e) => st.set_perf_status(format!("sniper unbind failed: {e}").into()),
                }
            }
        });
    });
    bind(app, shared, |app, _sh| {
        let w = app.as_weak();
        app.global::<State>().on_save_sniper(move |dpi| {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let dpi = dpi as u16;
                match crate::editor::set_sniper_dpi(dpi) {
                    Ok(true) => {
                        crate::dispatch::request_reload();
                        st.set_perf_status(format!("sniper DPI -> {dpi}").into());
                    }
                    // no button bound yet: the fader value is remembered (state holds it) and will
                    // arm at the precision DPI the moment a hold control is captured.
                    Ok(false) => st.set_perf_status(
                        format!("precision DPI {dpi} set — capture a hold button to arm it").into(),
                    ),
                    Err(e) => st.set_perf_status(format!("sniper DPI failed: {e}").into()),
                }
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

/// Human label for a symmetric lift-off-distance level (0 low / 1 medium / 2 high). Mirrors the
/// runtime's `lod_label` so the readout text matches the apply status line.
fn lod_level_label(level: i32) -> &'static str {
    match level.clamp(0, 2) {
        0 => "low",
        1 => "medium",
        _ => "high",
    }
}

/// Whether a Razer product-id takes the EXTENDED HyperPolling (>1000Hz, 0x00/0x40) path. RE finding
/// (OpenRazer): OpenRazer routes the Naga V2 Pro's stock links (0x00A7 wired / 0x00A8 dongle / 0x00A9
/// BT) to the LEGACY path, capped at 1000Hz — so 2000–8000Hz cannot work there. Only the separate
/// HyperPolling Wireless Dongle (PID 0x00B3) drives the extended command on hardware we can vouch for.
/// (Viper-8K-class mice also take it, but their exact PIDs aren't confirmed here — add them once
/// verified rather than guess.) A PID allowlist (not a registry capability) because the registry's
/// `SetPolling2` is opcode-presence, not link-mode reach.
fn pid_supports_hyperpoll(pid: u16) -> bool {
    matches!(pid, 0x00B3)
}

/// Whether a Razer keyboard product-id supports SNAP TAP (SOCD) — the MECHANICAL ADVANTAGES gate. A
/// Synapse-4-era firmware feature, so this is a PID ALLOWLIST (not a registry capability — the
/// feature has no read-only descriptor flag): BlackWidow V4 Pro (0x0287) / V4 75% (0x02A5) / V4 TKL
/// (0x028B) and the Huntsman V3 Pro family (0x02A6 / 0x02A7 / 0x02A8). The user's BlackWidow Chroma
/// V2 (0x0221, 2017) predates the feature → false. Extend as more supporting boards are confirmed.
fn pid_supports_snap_tap(pid: u16) -> bool {
    matches!(
        pid,
        0x0287 | 0x02A5 | 0x028B | 0x02A6 | 0x02A7 | 0x02A8
    )
}

/// Seed `snap-tap-supported` from the live device list — true iff ANY connected keyboard's PID is in
/// the Snap-Tap allowlist. A SYSTEM-page (not per-selected-device) concern: the MECHANICAL ADVANTAGES
/// option applies to the connected keyboard. When unsupported (the user's Chroma V2), the toggle
/// stays honestly GATED. Best-effort: reads the already-built `State.devices` rows.
fn refresh_snap_tap(app: &AppWindow, _sh: &SharedRt) {
    use slint::Model;
    let st = app.global::<State>();
    let rows = st.get_devices();
    let supported = (0..rows.row_count()).any(|i| {
        rows.row_data(i).is_some_and(|r| {
            r.kind == "keyboard"
                && u16::from_str_radix(r.pid.as_str(), 16)
                    .map(pid_supports_snap_tap)
                    .unwrap_or(false)
        })
    });
    st.set_snap_tap_supported(supported);
    // An unsupported board can never be enabled — keep the toggle honest if the device went away.
    if !supported {
        st.set_snap_tap_enabled(false);
    }
}

/// Re-read the device's lift-off-distance and mirror it into the LOD controls + readout. Reads the
/// SHARED 0x0B/0x85 getter once via two runtime calls: if the device is in ASYMMETRIC (split) mode it
/// seeds `lod-async = true` + the lift/landing pair; otherwise it seeds the symmetric `lod-level`.
/// Best-effort: a device that doesn't answer (asleep / no sensor-config) shows "—" and leaves state.
fn refresh_lod_readout(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    // ASYMMETRIC first: a Some means the device reports split mode (args[2] == 0x04).
    if let Some((lift, land)) = sh.borrow().rt.lift_off_async() {
        st.set_lod_async(true);
        st.set_lod_lift(lift as i32);
        st.set_lod_land(land as i32);
        st.set_lod_readout(format!("lift {lift} / land {land}").into());
        return;
    }
    // Otherwise SYMMETRIC (or unreadable).
    match sh.borrow().rt.lift_off_distance() {
        Some(lvl) => {
            let lvl = lvl.min(2) as i32;
            st.set_lod_async(false);
            st.set_lod_level(lvl);
            st.set_lod_readout(lod_level_label(lvl).into());
        }
        None => st.set_lod_readout("\u{2014}".into()),
    }
}

// ── live-loop → UI mirrors (called from dispatch's post_status on the UI thread) ────

/// A live ProfileSwitch/Cycle moved the process-wide cursor — mirror it into the header pill, the
/// GUI runtime (so save-profile captures current gaming-mode etc.), and the Profiles panel.
pub fn note_live_profile(app: &AppWindow, name: &str) {
    let st = app.global::<State>();
    if st.get_active_profile() != name {
        st.set_active_profile(name.into());
        // a live bound ProfileSwitch/Cycle changed the active profile — same reason as the manual
        // apply path: the rules behind an open inline editor may no longer line up, so close it.
        // Guarded by the actual-change check so a per-tick status repost never closes the editor.
        st.set_editing_rule(-1);
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
    // keep the save-name suggestion in step with what the user is actually in right now.
    refresh_profile_suggestion(app, focused);
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
                cap_bright: false,
                cap_scroll: false,
                cap_store: false,
                cap_idle: false,
                cap_plate: false,
                plate: "".into(), // audio endpoints have no plate
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
        st.set_sel_can_plate(false);
        st.set_selected_plate("".into());
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
    // This row's pid (its own hex field — `id` is the unit key now), parsed once — keys both the
    // per-device plate readout and the hyperpoll gate. Audio endpoints have an empty pid → 0, and
    // they're never plate/hyperpoll-capable anyway.
    let pid = u16::from_str_radix(row.pid.as_str(), 16).unwrap_or(0);
    // SIDE PLATE: a device with a [side_plates] map surfaces the last plate the mouse pushed. The plate
    // has NO getter (push-only), so seed from THIS device's last-known value the confirmation core
    // recorded (hidwatch feeds it per-pid); a light poll keeps it fresh after this. Others show nothing.
    st.set_sel_can_plate(row.cap_plate);
    st.set_selected_plate(if row.cap_plate {
        neuron::confirm::last_plate(pid).unwrap_or_default().into()
    } else {
        "".into()
    });
    // EXTENDED HyperPolling (>1000Hz, 0x00/0x40) is only real on known extended-PID hardware — the
    // HyperPolling Wireless Dongle + Viper-8K-class. The Naga's stock dongle (0x00A8) is legacy-capped
    // at 1000Hz, so its card shows an honest "use REPORT RATE" note instead of fake 2000–8000Hz chips.
    st.set_sel_can_hyperpoll(pid_supports_hyperpoll(pid));
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
        // HID: point the runtime at this UNIT (pid parsed above; `row.id` is the physical unit's
        // path instance) + seed the FEEL fader from its live reads.
        let changed = {
            let mut s = sh.borrow_mut();
            let c = s.rt.selected_pid != pid || s.rt.selected_unit != row.id.as_str();
            s.rt.selected_pid = pid;
            s.rt.selected_unit = row.id.to_string();
            // Switching which board you're EDITING no longer stops the others — each board's stream is
            // independent now, so the keyboard keeps animating while you work on the mouse.
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
            // a fresh device starts with the brush down (not a stale per-LED editor from the last board);
            // its own persisted stack is loaded by load_lighting_into_state below.
            st.set_light_brush_on(false);
            refresh_effects(app, sh);
            init_grid(app, sh);
            // SWITCHING boards: load the NEWLY-selected device's own persisted lighting (fps + stack /
            // data mode) into state + the page, overriding init_grid's per-class fps default when a pick
            // was saved, THEN resume it on the board. Under auto-apply the selected device's lighting is
            // always the live one and the manual apply button is gone — so the switch itself is what brings
            // the new board's saved stack live (immediate, like restore; not the 250ms edit-debounce).
            // Honours writes-pause + an empty stack (both make `apply_current_lighting` a silent no-op).
            // Gated on LIGHTING_READY so the INITIAL install-time selection stays state-only — restore_
            // lighting owns that first stream (and flips the gate).
            if LIGHTING_READY.load(std::sync::atomic::Ordering::Acquire) {
                load_lighting_into_state(app, sh);
                let _ = apply_current_lighting(app, sh);
            }
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

// ── WORKSHOP catalog: every macro on disk as an emergent, at-a-glance card ───────────────────────

/// Count every node in a macro tree, descending into flow bodies — the catalog's "size at a glance"
/// (so a one-line macro whose single `if` holds ten steps reads as the larger thing it is).
fn count_macro_nodes(nodes: &[MacroNode]) -> i32 {
    let mut n = 0i32;
    for node in nodes {
        n += 1;
        n += match node {
            MacroNode::Ask { yes, no, .. } => count_macro_nodes(yes) + count_macro_nodes(no),
            MacroNode::If { then_, else_, .. } => {
                count_macro_nodes(then_) + count_macro_nodes(else_)
            }
            MacroNode::RepeatN { body, .. }
            | MacroNode::RepeatWhile { body, .. }
            | MacroNode::ForEach { body, .. } => count_macro_nodes(body),
            MacroNode::Try { body, except_ } => count_macro_nodes(body) + count_macro_nodes(except_),
            _ => 0,
        };
    }
    n
}

/// Count a macro's DECLARED options by reading its own `NEURON_OPTIONS` list straight from source —
/// emergent (no registration or sidecar needed, so it's right even for a just-dropped-in file) and
/// cheap. Each option entry carries a `"key"`, so the number of those WITHIN the list literal is the
/// option count; the bracket-matched scan keeps a stray `"key"` elsewhere in the file from inflating
/// it. 0 when the macro declares no options.
fn option_count_from_source(src: &str) -> i32 {
    let Some(start) = src.find("NEURON_OPTIONS") else {
        return 0;
    };
    let rest = &src[start..];
    let Some(lb) = rest.find('[') else {
        return 0;
    };
    let mut depth = 0i32;
    let mut end = rest.len();
    for (i, c) in rest[lb..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    end = lb + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let block = &rest[lb..end];
    (block.matches("\"key\"").count() + block.matches("'key'").count()) as i32
}

/// Collect every macro id an action references, descending into `Sequence` steps — so a macro fired
/// as one step of a multi-step bind still shows its trigger. Only Python script refs (the warm-macro
/// tier) are macro ids; Shell/File refs are inline commands / paths, not macros.
fn collect_macro_ids(action: &neuron::action::Action, out: &mut Vec<String>) {
    use neuron::action::{Action, ScriptKind};
    match action {
        Action::Script { script } if script.kind == ScriptKind::Python => {
            out.push(script.id.clone())
        }
        Action::Sequence { steps } => {
            for s in steps {
                collect_macro_ids(&s.action, out);
            }
        }
        _ => {}
    }
}

/// Map each macro id -> the binding trigger(s) that fire it, cross-ref'd from the SAME rule set live
/// dispatch uses (the assembled spine rules + the GUI-authored sidecar). One macro may be bound more
/// than once; each label is `Trigger::describe()` (what the bindings list shows). Macros referenced
/// by no rule simply don't appear (the catalog renders them as "(unbound)").
fn macro_trigger_map(sh: &SharedRt) -> HashMap<String, Vec<String>> {
    let mut rules = sh.borrow().rt.spine_rules();
    rules.extend(crate::editor::load_gui_rules());
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for r in &rules {
        let mut ids = Vec::new();
        collect_macro_ids(&r.action, &mut ids);
        if ids.is_empty() {
            continue;
        }
        let label = r.trigger.describe();
        for id in ids {
            let entry = map.entry(id).or_default();
            if !entry.contains(&label) {
                entry.push(label.clone());
            }
        }
    }
    map
}

/// One in-flight catalog rebuild at a time — so refreshes (open/save/reload) coalesce instead of
/// stacking parse workers on the serial sidecar. The running worker already reads the current disk.
static MACRO_CATALOG_BUILDING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Rebuild the WORKSHOP catalog model — every macro on disk as an emergent card: the plain-English
/// summary walked from its OWN nodes (neuron-core's `summarize`), its node count, its declared option
/// count, and the trigger(s) that fire it. The cheap parts (the file list, option counts, the trigger
/// cross-ref) run here on the UI thread; the per-macro PARSE the summary needs goes through the warm
/// sidecar (blocks up to FIRE_BUDGET), so it runs OFF the UI thread and posts the finished model back.
fn refresh_macro_catalog(app: &AppWindow) {
    use std::sync::atomic::Ordering;
    let macros = neuron::macros::macro_host::scan_macro_dir();
    if macros.is_empty() {
        let st = app.global::<State>();
        st.set_macro_catalog(ModelRc::new(VecModel::<MacroCard>::default()));
        st.set_macro_catalog_building(false);
        return;
    }
    // one rebuild at a time — a request while a worker is running is dropped (it reads fresh disk).
    if MACRO_CATALOG_BUILDING.swap(true, Ordering::SeqCst) {
        return;
    }
    let triggers = with_shared_ret(macro_trigger_map).unwrap_or_default();
    app.global::<State>().set_macro_catalog_building(true);
    let back = app.as_weak();
    std::thread::spawn(move || {
        let rows: Vec<MacroCard> = macros
            .into_iter()
            .map(|(id, src)| {
                let (summary, steps) = match neuron::macros::parse_macro(&src) {
                    Ok(nodes) => (neuron::macros::summarize(&nodes), count_macro_nodes(&nodes)),
                    // a syntax-broken file still earns a card — say so honestly rather than "empty".
                    Err(_) => ("couldn't read steps (syntax error?)".to_string(), 0),
                };
                let options = option_count_from_source(&src);
                let trigger = match triggers.get(&id) {
                    Some(ts) if !ts.is_empty() => ts.join("  \u{00b7}  "),
                    _ => "(unbound)".to_string(),
                };
                MacroCard {
                    name: id.into(),
                    summary: summary.into(),
                    steps,
                    options,
                    trigger: trigger.into(),
                }
            })
            .collect();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = back.upgrade() {
                let st = app.global::<State>();
                st.set_macro_catalog(ModelRc::new(VecModel::from(rows)));
                st.set_macro_catalog_building(false);
            }
            MACRO_CATALOG_BUILDING.store(false, Ordering::SeqCst);
        });
    });
}

/// Load a macro from disk INTO the editor: set the name + source, recompute has-ask, then re-parse
/// into the blocks canvas (which also reseeds the Rust working tree) via the one existing parse path
/// (`refresh-macro-blocks`). Used by a catalog card click and by reload (refreshing the open macro
/// from disk). A missing/unreadable id is a no-op — the catalog only offers ids that exist.
fn load_macro_into_editor(app: &AppWindow, id: &str) {
    let Some(src) = neuron::macros::macro_host::load_macro(id) else {
        return;
    };
    let st = app.global::<State>();
    st.set_macro_name(id.into());
    st.set_macro_has_ask(src.contains("neuron.ask"));
    st.set_macro_source(src.clone().into());
    st.set_macro_status(format!("loaded '{id}' \u{2014} edit it, then save").into());
    // off-thread parse -> blocks + working-tree reseed (refresh-macro-blocks owns both); one path.
    st.invoke_refresh_macro_blocks(src.into());
}

/// Keep the selected device's SIDE-PLATE readout honest with the last plate the mouse pushed. The
/// plate is detected ONLY by a device-pushed report (no getter to poll), so we read the last-observed
/// plate the confirmation core recorded (hidwatch feeds it on every swap) and surface it. Cheap — a
/// brief mutex read + a string compare — so the main tick can call it; it only writes on a change.
/// Only a selected mouse WITH a [side_plates] map ever shows a value (others read ""). The instant
/// feedback on a swap is the confirmation CARD; this readout follows within a tick.
pub fn refresh_selected_plate(app: &AppWindow) {
    use slint::Model;
    let st = app.global::<State>();
    if !st.get_sel_can_plate() {
        if !st.get_selected_plate().is_empty() {
            st.set_selected_plate("".into());
        }
        return;
    }
    // The selected device's pid keys its OWN plate readout (per-pid; a sibling mouse's swap can't leak
    // into this card).
    let pid = {
        let i = st.get_selected_device();
        (i >= 0)
            .then(|| st.get_devices().row_data(i as usize))
            .flatten()
            .and_then(|r| u16::from_str_radix(r.pid.as_str(), 16).ok())
            .unwrap_or(0)
    };
    let label = neuron::confirm::last_plate(pid).unwrap_or_default();
    if st.get_selected_plate().as_str() != label {
        st.set_selected_plate(label.into());
    }
}

/// Keep the DEVICE-LIST row's PLATE readout live with the last plate the mouse pushed. The plate is
/// push-only (no getter), so a periodic rescan can't carry it — instead this patches the plated row's
/// `plate` field IN PLACE (via `set_row_data`, NOT a full list rebuild) whenever the last-known plate
/// changes. Cheap: a mutex read + a per-row string compare; it writes a single row only on an actual
/// change, and only for capability-`plate` rows (every other row stays untouched).
pub fn refresh_plated_row(app: &AppWindow) {
    use slint::Model;
    let st = app.global::<State>();
    let rows = st.get_devices();
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else { continue };
        if !row.cap_plate {
            continue;
        }
        // Each plated device shows ITS OWN last-known plate (per-pid), never a shared global value, so
        // two plated mice can't cross-contaminate each other's row readout.
        let pid = u16::from_str_radix(row.pid.as_str(), 16).unwrap_or(0);
        let label = neuron::confirm::last_plate(pid).unwrap_or_default();
        if row.plate.as_str() != label {
            row.plate = label.into();
            rows.set_row_data(i, row);
        }
    }
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
            // the selection key is the PHYSICAL UNIT (path instance), so two identical devices
            // are two distinct, individually-selectable rows; the pid rides in `pid` above.
            id: d.instance.clone().into(),
            detail: "".into(),
            cap_dpi: d.cap_dpi,
            cap_poll: d.cap_poll,
            cap_light: d.cap_light,
            cap_bright: d.cap_bright,
            cap_scroll: d.cap_scroll,
            cap_store: d.cap_store,
            cap_idle: d.cap_idle,
            cap_plate: d.cap_plate,
            // SIDE PLATE (push-only, no getter): seed the row from THIS device's last pushed plate
            // (per-pid). refresh_plated_row keeps it live in place after this. Non-plated devices show
            // nothing.
            plate: if d.cap_plate {
                neuron::confirm::last_plate(d.pid).unwrap_or_default().into()
            } else {
                "".into()
            },
        })
        .collect();
    // 2) EMERGENT audio endpoints appended — mic + every output, generic over any hardware.
    rows.extend(audio_rows());
    let st = app.global::<State>();
    use slint::Model;
    st.set_devices(ModelRc::new(VecModel::from(rows)));
    // MECHANICAL ADVANTAGES: seed Snap Tap support from the live keyboard list (honest gate).
    refresh_snap_tap(app, sh);
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
    // the HOLD KEY — a Noop activator on the hypershift layer — is surfaced on its own (REACHED BY),
    // never as a shifted "binding"; pull it out so the list shows only real shifted actions.
    let mut hold_label: Option<String> = None;
    for r in &gui {
        if r.layer.as_deref() == Some("hypershift") && r.action == neuron::action::Action::Noop {
            hold_label = Some(r.trigger.describe());
            continue;
        }
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
    st.set_hypershift_hold_ready(hold_label.is_some());
    st.set_hypershift_hold_label(hold_label.unwrap_or_else(|| "—".to_string()).into());
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

/// Map a Trigger to its short kind tag (for the rule list's left mark). Mirrors runtime::trigger_kind.
fn trigger_kind_str(t: &neuron::engine::Trigger) -> &'static str {
    use neuron::engine::Trigger;
    match t {
        Trigger::Input { .. } => "input",
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
                    brightness: p
                        .brightness
                        .map(|b| format!("{b}%"))
                        .unwrap_or_default()
                        .into(),
                    // the lighting badge is the ONE lighting label (Profile::lighting_label): "custom"
                    // for a painted/imported frame, the preset name ("wave") for a lone procedural
                    // layer, "N fx" for a taller stack — identical to summary()/the LIVE row.
                    lighting: p.lighting_label().into(),
                    idle: p
                        .idle_secs
                        .map(|s| format!("{s}s"))
                        .unwrap_or_default()
                        .into(),
                    in_game: p
                        .in_game_polling
                        .map(|(wired, dongle)| format!("{wired}/{dongle}Hz"))
                        .unwrap_or_default()
                        .into(),
                    gaming: p.has_gaming(),
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
    drop(s);
    // seed the low-friction save name (a device page write already told us what we're tuning —
    // never make the user re-type it) from the focused app, else a value descriptor.
    let focused = st.get_focused_app().to_string();
    refresh_profile_suggestion(app, &focused);
}

/// The bare app name for a save-name suggestion: the focused executable's filename, minus ".exe"
/// and ONLY the Unreal packaging suffix ("…-Win64-Shipping" → peel the config tag then the platform
/// tag as EXACT suffixes). NEVER a general hyphen/underscore cut — real names carry those
/// ("Counter-Strike", "Apex_Legends" must survive whole, or zero-typing capture mis-names them).
fn app_stem(focused: &str) -> &str {
    let base = focused
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(focused)
        .trim_end_matches(".exe")
        .trim_end_matches(".EXE");
    let base = ["-Shipping", "-Development", "-Test", "-DebugGame"]
        .iter()
        .find_map(|&s| base.strip_suffix(s))
        .unwrap_or(base);
    let base = ["-Win64", "-Win32", "-WinGDK", "-WinArm64", "-Linux", "-Mac"]
        .iter()
        .find_map(|&s| base.strip_suffix(s))
        .unwrap_or(base);
    base.trim()
}

/// Compute a non-colliding suggested profile name so "capture" needs zero typing: prefer the
/// focused app's bare name, else a value descriptor (dpi·effect), else "profile N". Never collides
/// with an existing profile (so the capture button stays "capture", never a surprise "overwrite").
pub fn refresh_profile_suggestion(app: &AppWindow, focused: &str) {
    let st = app.global::<State>();
    let existing: Vec<String> =
        with_shared_ret(|sh| sh.borrow().rt.profiles.iter().map(|p| p.name.clone()).collect())
            .unwrap_or_default();
    // de-collide on the on-disk KEY (sanitized + case-folded), the same key the overwrite check and
    // `Profile::path` use — so a "free" suggestion can't map to an existing profile's file.
    let taken = |n: &str| {
        let key = neuron::profile::Profile::file_key(n);
        existing
            .iter()
            .any(|e| neuron::profile::Profile::file_key(e) == key)
    };

    // 1. the focused app's bare name (see `app_stem`: filename minus ".exe" + the Unreal packaging
    //    suffix only — never a general hyphen/underscore cut that would maim a real name).
    let base = app_stem(focused);
    let mut seed = if !base.is_empty() && base != "—" {
        base.to_string()
    } else {
        // 2. a value descriptor from what's live right now.
        let dpi = st.get_dpi() as i32;
        let eff = st.get_light_effect().to_string();
        if !eff.is_empty() {
            format!("{dpi} {eff}")
        } else {
            format!("{dpi} dpi")
        }
    };
    // 3. de-collide: "valorant", "valorant 2", ...
    if taken(&seed) {
        let root = seed.clone();
        let mut n = 2;
        while taken(&format!("{root} {n}")) {
            n += 1;
        }
        seed = format!("{root} {n}");
    }
    st.set_profile_name_suggested(seed.into());
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

// ── LIGHTING PERSISTENCE — survive a relaunch ──────────────────────────────────────────────────
// The applied effect/layer stack (or data mode) + the chosen stream fps are persisted PER-DEVICE in
// app.toml (see `prefs::DeviceLight`, keyed by pid) so a board RESUMES its effect on launch instead of
// sitting frozen on the device's last held frame. Saves are cheap + only on user changes (debounced so
// a slider drag doesn't thrash the disk) — NEVER from the animate loop. Restore re-applies through the
// SAME stream path the apply button uses, so the device actually resumes (not just the on-screen page).

/// Gate: the persisted lighting state has been restored (or confirmed absent) for the selected device.
/// Until this flips true at the end of [`restore_lighting`], [`save_lighting`] is a no-op — so the
/// install sequence (which runs the layer projection before restore) can never clobber a saved state
/// with the empty startup stack.
static LIGHTING_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    /// Debounce timer for the lighting-state disk write (UI-thread). Restarted on each change so a
    /// rapid gesture (dragging the speed slider) coalesces into ONE app.toml write ~400ms after the
    /// last edit, instead of one write per emitted value.
    static LIGHT_SAVE_TIMER: slint::Timer = slint::Timer::default();
    /// The latest pending lighting snapshot the debounce is waiting to write: `(pid, fps, layers)`.
    /// Held ALONGSIDE the timer so the write can be FLUSHED early — on a structural edit or app exit —
    /// instead of being lost if the app quits before the 400ms settles (the "stacked layers collapse to
    /// base on relaunch" bug: a stack/re-theme made within the debounce window never reached disk).
    static LIGHT_PENDING: std::cell::RefCell<
        Option<(u16, u32, Vec<neuron::pattern::LayerDef>)>,
    > = std::cell::RefCell::new(None);
    /// Debounce timer for the AUTO-APPLY device re-stream (UI-thread), mirroring `LIGHT_SAVE_TIMER`.
    /// Restarted on each lighting edit so a burst (a knob drag) coalesces into ONE re-stream ~250ms after
    /// the last change instead of thrashing the board once per emitted value. See `schedule_lighting_apply`.
    static LIGHT_APPLY_TIMER: slint::Timer = slint::Timer::default();
    /// When set, `refresh_layers` PROJECTS + persists but does NOT schedule an auto-apply. Held by the pure
    /// STATE-LOAD / explicit-stream paths (device switch, profile apply) whose callers stream immediately
    /// themselves — so the debounced re-stream can't fire a redundant second write on top of their direct
    /// `apply_current_lighting`. A scoped [`SuppressApply`] guard sets/clears it (restore-safe).
    static SUPPRESS_LIGHT_APPLY: std::cell::Cell<bool> = std::cell::Cell::new(false);
}

/// Scope guard: suppress `refresh_layers`' auto-apply for the duration (used by load/profile-apply paths
/// that project the stack then stream EXPLICITLY, so the debounce doesn't double-fire). Restores the prior
/// value on drop, so it's safe even if nested or if the guarded body unwinds.
struct SuppressApply(bool);
impl SuppressApply {
    fn new() -> Self {
        SuppressApply(SUPPRESS_LIGHT_APPLY.with(|s| s.replace(true)))
    }
}
impl Drop for SuppressApply {
    fn drop(&mut self) {
        SUPPRESS_LIGHT_APPLY.with(|s| s.set(self.0));
    }
}

/// Persist the SELECTED device's current lighting state (fps + layer stack), debounced. A no-op until
/// `LIGHTING_READY` (so restore/install can't overwrite a saved state) and when no device is selected.
/// Snapshots the state into `LIGHT_PENDING` now (cheap); the disk write fires once the debounce settles
/// — OR sooner via [`flush_lighting_save`] (structural edits / app exit), so a quick quit-and-relaunch
/// can never strand the change.
fn save_lighting(sh: &SharedRt) {
    if !LIGHTING_READY.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let (pid, fps, layers) = {
        let s = sh.borrow();
        (
            s.rt.selected_pid,
            s.rt.light_fps.load(std::sync::atomic::Ordering::Relaxed),
            s.light_layers.clone(),
        )
    };
    if pid == 0 {
        return; // nothing selected → no per-device key to write under
    }
    LIGHT_PENDING.with(|p| *p.borrow_mut() = Some((pid, fps, layers)));
    LIGHT_SAVE_TIMER.with(|t| {
        t.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(400),
            || flush_lighting_save(),
        );
    });
}

/// Write any pending lighting snapshot to disk IMMEDIATELY and cancel the debounce. Idempotent and
/// cheap when nothing's pending. Called three ways: by the debounce timer when it settles; by every
/// STRUCTURAL edit (stack / remove / tile pick) so a layer change persists the instant it's made,
/// regardless of how the app later exits; and once after the event loop returns (`main`) so a tray-quit
/// can't drop a still-debounced knob edit. Must run on the UI thread (the snapshot is thread-local).
pub fn flush_lighting_save() {
    LIGHT_SAVE_TIMER.with(|t| t.stop());
    let pending = LIGHT_PENDING.with(|p| p.borrow_mut().take());
    if let Some((pid, fps, layers)) = pending {
        if let Err(e) = crate::prefs::set_device_light(
            pid,
            crate::prefs::DeviceLight { fps, layers, ..Default::default() },
        ) {
            eprintln!("neuron: lighting save failed ({e})");
        }
    }
}

/// Load the SELECTED device's saved lighting state into the shared runtime + the UI (the fps atomic,
/// the layer stack, and the projected page), WITHOUT starting the device stream. A legacy data-mode save
/// is MIGRATED into a `vitals` layer here (`DeviceLight::migrated`), so a pre-unification save resumes
/// with zero user action. Returns true when there's a non-empty stack to resume. Authoritative: a device
/// with nothing saved is reset to a blank surface, so per-device state never bleeds across a switch.
fn load_lighting_into_state(app: &AppWindow, sh: &SharedRt) -> bool {
    let pid = sh.borrow().rt.selected_pid;
    // `.migrated()` folds a legacy `data = "mouse-battery"` record into a vitals LAYER (one-way compat).
    let saved = crate::prefs::device_light(pid).unwrap_or_default().migrated();
    // fps: honour the user's saved pick; fall back to whatever the per-device default already seeded
    // (init_grid) only when nothing's saved (saved.fps == 0).
    if saved.fps >= 1 {
        sh.borrow()
            .rt
            .light_fps
            .store(saved.fps.clamp(1, 30), std::sync::atomic::Ordering::Relaxed);
    }
    let fps_now = sh
        .borrow()
        .rt
        .light_fps
        .load(std::sync::atomic::Ordering::Relaxed);
    app.global::<State>().set_light_fps(fps_now as f32);
    let has_surface = !saved.layers.is_empty();
    {
        let mut s = sh.borrow_mut();
        s.light_layers = saved.layers;
        s.selected_layer = s.light_layers.len().saturating_sub(1);
        s.layers_rev += 1;
    }
    // project the restored stack into both the legacy layer model and the unified tile surface. SUPPRESS
    // auto-apply here: a pure state-load must not itself stream (this fn's contract) — the CALLERS decide
    // when to stream (restore + device-switch each do an immediate `apply_current_lighting`), so the
    // debounce can't fire a redundant second write on top of that direct apply.
    {
        let _suppress = SuppressApply::new();
        refresh_layers(app, sh);
    }
    has_surface
}

/// Stream the CURRENT lighting composite live to the selected device — the shared body behind the GUI's
/// apply button AND startup restore, so a resumed board streams through the EXACT path a manual pick
/// uses. Vitals rides this too (it's just a `vitals` layer in the stack now, not a sidecar). Honours the
/// writes-paused kill-switch. Returns a status.
fn apply_current_lighting(app: &AppWindow, sh: &SharedRt) -> String {
    let st = app.global::<State>();
    if st.get_writes_paused() {
        return "writes paused — composite not applied".into();
    }
    if sh.borrow().light_layers.is_empty() {
        return "nothing to apply".into();
    }
    let (pid, unit, defs) = {
        let s = sh.borrow();
        (
            s.rt.selected_pid,
            s.rt.selected_unit.clone(),
            s.light_layers.clone(),
        )
    };
    let msg = stream_board(app, sh, pid, unit.clone(), defs);
    let animating = sh.borrow().rt.animating(pid, &unit);
    st.set_compositing(animating);
    msg
}

/// Stream ONE board's composite live on its writer, with the standard end-of-stream callback —
/// the shared body behind [`apply_current_lighting`] (the selected board, from the live stack)
/// and [`reapply_all_boards`] (every other board, from its persisted stack). `unit` is the
/// board's physical identity (`DeviceRow.id`), so the stream lands on exactly that board even
/// when an identical twin shares its pid. `start_layers` routes to the host base layer when the
/// host is active (else a local stream) and stops THIS board's prior local stream first, so this
/// one call is correct in both modes and migrates a board cleanly when the host comes up.
/// Returns a status string.
fn stream_board(
    app: &AppWindow,
    sh: &SharedRt,
    pid: u16,
    unit: String,
    defs: Vec<neuron::pattern::LayerDef>,
) -> String {
    let back = app.as_weak();
    let mut s = sh.borrow_mut();
    let unit_arg = unit.clone();
    s.rt.start_layers(defs, pid, &unit_arg, move |reason, token| {
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = back.upgrade() {
                with_shared(|sh| {
                    // ignore a STALE end: this stream was superseded, or already stopped+removed.
                    if !sh.borrow().rt.anim_is_current(&unit, &token) {
                        return;
                    }
                    sh.borrow_mut().rt.anim_clear(&unit, &token);
                    let st = app.global::<State>();
                    // only clear the indicator if we're VIEWING the board that ended.
                    if sh.borrow().rt.selected_unit == unit {
                        st.set_compositing(false);
                    }
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
}

/// Every light-capable board as `(pid, unit)`, read from the device model — the DURABLE set of
/// boards the host may manage, available even after a global stop has cleared every live stream
/// (unlike the `anim`/base maps). One entry per PHYSICAL unit, so two identical boards each get
/// their own stream. Audio endpoints (unparseable pid) drop out.
fn light_capable_units(app: &AppWindow) -> Vec<(u16, String)> {
    use slint::Model;
    let rows = app.global::<State>().get_devices();
    (0..rows.row_count())
        .filter_map(|i| rows.row_data(i))
        .filter(|r| r.cap_light)
        .filter_map(|r| {
            let pid = u16::from_str_radix(r.pid.as_str(), 16).ok()?;
            (pid != 0).then(|| (pid, r.id.to_string()))
        })
        .collect()
}

/// Re-establish EVERY configured board's lighting — the correct unit of work for a host
/// transition, where the single-board [`apply_current_lighting`] leaves the others behind. The
/// selected board streams from its LIVE in-memory stack (unsaved edits and all); every other
/// light-capable board streams from its PERSISTED stack (non-selected boards aren't held in
/// memory). Because `stream_board` → `start_layers` stops each board's prior LOCAL stream before
/// (re)claiming, this MIGRATES every board onto the host on enable — closing the double-writer
/// window on non-selected boards — and RESTORES every board (not just the one on screen) after a
/// "who wins" flip or a resume from pause/Observe.
fn reapply_all_boards(app: &AppWindow, sh: &SharedRt) {
    if app.global::<State>().get_writes_paused() {
        return; // paused: nothing streams (mirrors apply_current_lighting's own gate)
    }
    let selected_unit = sh.borrow().rt.selected_unit.clone();
    // The selected board first, from the live stack (this also sets the compositing indicator).
    let _ = apply_current_lighting(app, sh);
    // Then every OTHER light-capable board, each from its own persisted stack + pace. The saved
    // stack is per-pid (config is per-model); two identical boards each stream it on their OWN
    // unit — explicit per-board application, not first-match-wins.
    let sel_fps = sh.borrow().rt.light_fps.load(std::sync::atomic::Ordering::Relaxed);
    for (pid, unit) in light_capable_units(app) {
        if unit == selected_unit {
            continue;
        }
        let saved = crate::prefs::device_light(pid).unwrap_or_default().migrated();
        if saved.layers.is_empty() {
            continue;
        }
        // start_layers seeds the new stream's pace from the shared `light_fps` atomic (as a COPY
        // — see start_layers), so pose THIS board's saved pace across the start, then restore the
        // selected board's live pace below. Without this every migrated board would inherit
        // whichever board happens to be selected.
        if saved.fps >= 1 {
            sh.borrow()
                .rt
                .light_fps
                .store(saved.fps.clamp(1, 30), std::sync::atomic::Ordering::Relaxed);
        }
        let _ = stream_board(app, sh, pid, unit, saved.layers);
    }
    sh.borrow()
        .rt
        .light_fps
        .store(sel_fps, std::sync::atomic::Ordering::Relaxed);
}

/// AUTO-APPLY — the one debounced chokepoint that keeps the board tracking the UI. Every lighting edit
/// funnels through [`refresh_layers`], which calls this; it (re)starts a single-shot ~250ms timer so a
/// burst of edits (a knob drag) COALESCES into ONE device re-stream after the quiet settles, instead of
/// restarting the stream on every emitted value. The fire re-streams the CURRENT stack via the shared
/// [`apply_current_lighting`] engine (the same one restore + device-switch use), and each re-stream
/// SUPERSEDES the last — the superseded stream's end callback is swallowed by `anim_is_current`, so a
/// rapid re-apply never spams "composite ended". Silent by design: it sets the `compositing` indicator
/// but no status line (the edit handler already narrated the change).
///
/// No-op unless lighting has been restored ([`LIGHTING_READY`] — so startup restore, not this, owns the
/// first apply), writes are armed (`!writes_paused` — the one deliberate "stop"), and the load/profile
/// paths haven't [`SuppressApply`]-suppressed it. fps is deliberately NOT routed here: it re-paces the
/// running stream in place (`set_anim_fps`), no restart. The gate is re-checked at fire time (writes may
/// have paused during the quiet window).
fn schedule_lighting_apply(app: &AppWindow, sh: &SharedRt) {
    if !LIGHTING_READY.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    if SUPPRESS_LIGHT_APPLY.with(|s| s.get()) {
        return;
    }
    if app.global::<State>().get_writes_paused() {
        return;
    }
    let back = app.as_weak();
    let sh = sh.clone();
    LIGHT_APPLY_TIMER.with(|t| {
        t.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(250),
            move || {
                if let Some(app) = back.upgrade() {
                    // re-check the kill-switch: writes may have paused during the debounce window, and
                    // `apply_current_lighting` also refuses when paused — but bail early to skip the work.
                    if app.global::<State>().get_writes_paused() {
                        return;
                    }
                    let _ = apply_current_lighting(&app, &sh); // discard the status — auto-apply is silent
                }
            },
        );
    });
}

/// Restore + RE-APPLY the selected device's persisted lighting ONCE at startup: load the saved fps +
/// stack/data into state, and — if there's a surface to resume — start the device stream so the board
/// picks the effect back up instead of holding its stale last frame. Flips `LIGHTING_READY` so user
/// edits from here on persist. Respects the writes-paused gate (the state is still restored; it just
/// isn't streamed until writes resume — un-pausing re-applies the stack; there is no manual apply now).
pub fn restore_lighting(app: &AppWindow, sh: &SharedRt) {
    let has_surface = load_lighting_into_state(app, sh);
    if has_surface {
        let msg = apply_current_lighting(app, sh);
        let (sel, sel_unit) = {
            let s = sh.borrow();
            (s.rt.selected_pid, s.rt.selected_unit.clone())
        };
        if sh.borrow().rt.animating(sel, &sel_unit) {
            app.global::<State>()
                .set_status_line(format!("lighting resumed — {msg}").into());
        }
    }
    LIGHTING_READY.store(true, std::sync::atomic::Ordering::Release);
}

/// Commit the painted canvas as a FIRST-CLASS `custom` layer (survives relaunch, rides into a captured
/// profile) rather than a transient device-only write. A pushed frame is a FULL, opaque snapshot of
/// every LED, so anything beneath it is occluded dead weight — the push therefore COLLAPSES the stack
/// to the single layer it now IS. What you painted becomes the lighting, whole and unified: no ghost
/// effect riding invisibly underneath. The caller follows with `refresh_layers`, whose AUTO-APPLY streams
/// this custom layer to the board as a StaticFrame — so the commit IS the paint reaching the device, with
/// no separate one-shot write (the old `rt.push_frame` path this used to ride alongside is gone).
fn commit_custom_layer(sh: &SharedRt, frame: &[Rgb]) {
    let cells: Vec<[u8; 3]> = frame.iter().map(|c| [c.r, c.g, c.b]).collect();
    {
        let mut s = sh.borrow_mut();
        s.light_layers = vec![neuron::pattern::LayerDef {
            pattern: "custom".into(),
            frame: cells,
            ..Default::default()
        }];
        s.selected_layer = 0;
        s.layers_rev += 1;
    }
    save_lighting(sh);
}

/// The chokepoint every lighting mutation funnels through: clamp the selection, re-project the stack
/// into the unified surface (tiles + params + spectrum editor), persist (debounced), AND auto-apply the
/// stack to the board (debounced). Because EVERY edit path routes here — tile pick, param knobs, the whole
/// spectrum editor, stack add/select/remove, place/reset region, import, paint commit — this one hook is
/// what makes lighting AUTO-STREAM: no manual apply. Both side effects are debounced + gated (see
/// `save_lighting` / `schedule_lighting_apply`), so a knob drag collapses to one disk write + one restream.
pub fn refresh_layers(app: &AppWindow, sh: &SharedRt) {
    let (n, sel) = {
        let s = sh.borrow();
        (s.light_layers.len(), s.selected_layer)
    };
    let st = app.global::<State>();
    st.set_selected_layer(if n == 0 { -1 } else { sel.min(n - 1) as i32 });
    // the unified page + the spectrum editor are projections of the SAME stack — keep them in lockstep.
    refresh_light_unified(app, sh);
    // It's a no-op until restore has run (LIGHTING_READY) and is never reached from the animate loop —
    // only user edits + the tile/data picks — so the disk write stays cheap (and debounced).
    save_lighting(sh);
    // …and push the edit to the physical board — debounced, gated on LIGHTING_READY + !writes_paused, and
    // skipped when a load/profile path suppressed it (those stream explicitly). fps doesn't route here.
    schedule_lighting_apply(app, sh);
}

// ── THE UNIFIED LIGHTING SURFACE — tiles + auto-rendered knobs over the layer stack ───────────
// The page is render-first and tile-driven, but the BACKEND is the proven compositor: a single effect
// is just `light_layers` with one entry, stacking adds entries, the "active effect" is the selected
// layer. These helpers project that truth into the tile grid + the schema-rendered param model.

/// The tile CATALOG, in grid order: every PRESET (the single source — [`neuron::pattern::presets`]).
/// `(slug, name, kind)` — `kind` is `"data"` for a READOUT preset (vitals: it shows your device, not a
/// light show, so the tile draws a "DATA" corner glyph) and `"effect"` for every decorative preset.
/// Registry-driven (`pattern_is_readout`), so a new readout preset self-marks. Adding a look is a preset
/// entry in core — zero UI code.
fn light_tile_catalog() -> Vec<(&'static str, &'static str, &'static str)> {
    neuron::pattern::presets()
        .iter()
        .map(|p| {
            let kind = if neuron::pattern::pattern_is_readout(p.pattern) { "data" } else { "effect" };
            (p.slug, p.label, kind)
        })
        .collect()
}

/// Build the [`neuron::pattern::LayerDef`] a preset slug describes (the look the tile picker applies),
/// falling back to a benign default for an unknown slug.
fn preset_layer(slug: &str) -> neuron::pattern::LayerDef {
    neuron::pattern::preset_layer(slug).unwrap_or_default()
}

/// Render a `rows*cols` device frame into a small preview Image at the tile's pixel size — the
/// MATERIAL-card pattern: each cell becomes a block, lit on the void. Cheap (tiles are ~110px) and
/// the swatch literally IS the effect running, so the grid reads as a wall of live previews.
fn frame_to_preview(frame: &[Rgb], rows: usize, cols: usize) -> Image {
    // a small canvas — block-fill per cell with a 1px gutter so the lattice reads
    let (cell, gap, pad) = (9usize, 2usize, 4usize);
    let w = pad * 2 + cols * cell + cols.saturating_sub(1) * gap;
    let h = pad * 2 + rows * cell + rows.saturating_sub(1) * gap;
    let (w, h) = (w.max(1), h.max(1));
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    {
        let px = buf.make_mut_slice();
        // ground = the void
        for p in px.iter_mut() {
            *p = Rgba8Pixel {
                r: 4,
                g: 5,
                b: 6,
                a: 255,
            };
        }
        for r in 0..rows {
            for c in 0..cols {
                let col = frame.get(r * cols + c).copied().unwrap_or(Rgb::BLACK);
                let x0 = pad + c * (cell + gap);
                let y0 = pad + r * (cell + gap);
                for yy in y0..y0 + cell {
                    for xx in x0..x0 + cell {
                        if xx < w && yy < h {
                            let i = yy * w + xx;
                            px[i] = Rgba8Pixel {
                                r: col.r,
                                g: col.g,
                                b: col.b,
                                a: 255,
                            };
                        }
                    }
                }
            }
        }
    }
    Image::from_rgba8(buf)
}

/// A representative AMBIENT frame for the tile thumbnail when ambient ISN'T the selected effect — a
/// calm horizontal hue sweep, brighter at the top, so the tile still reads as a screen-mirror. The
/// LIVE whole-desktop capture (`screen_ambient`, measured at ~41ms PER GRAB, ~450ms/s when always-on)
/// is then driven ONLY by the selected ambient effect's big preview + the device stream — never by a
/// postage-stamp thumbnail. Mirrors the `preview_vitals` pattern: the tile shows what the surface
/// LOOKS like without paying its live cost.
fn preview_ambient_frame(rows: u8, cols: u8) -> Vec<Rgb> {
    let (r, c) = (rows as usize, cols as usize);
    let mut f = vec![Rgb::BLACK; r * c];
    for y in 0..r {
        let v = 0.55 + 0.35 * (1.0 - y as f32 / r.max(1) as f32); // a touch brighter at the top
        for x in 0..c {
            let hue = if c > 1 { x as f32 / (c as f32 - 1.0) * 300.0 } else { 0.0 };
            f[y * c + x] = Rgb::from_hsv(hue, 0.82, v.clamp(0.0, 1.0));
        }
    }
    f
}

/// A representative TYPING HEAT frame for the tile thumbnail — a warm thermal wash with a few hot
/// flares (the keys-just-hit look). The thumbnail pass suppresses live key reads (a perf fix), so a
/// Thermal thumbnail can never see typing; this still shows what the effect IS without faking input.
/// The LIVE typing-reactivity plays only on the SELECTED effect's big preview + the device stream (both
/// read keys live). It samples the SAME thermal spectrum the device renders, across a synthetic heat
/// field (a warm bed brightest mid-board, with two hot flares).
fn preview_thermal_frame(rows: u8, cols: u8) -> Vec<Rgb> {
    let sp = neuron::pattern::default_spectrum("thermal").unwrap_or_default();
    let (r, c) = (rows as usize, cols as usize);
    let mut f = vec![Rgb::BLACK; r * c];
    let cx = (c as f32 - 1.0) / 2.0;
    // two fixed "just-pressed" flares so the still reads as live typing without any input.
    let flares = [(r as f32 * 0.5, c as f32 * 0.35), (r as f32 * 0.4, c as f32 * 0.66)];
    for y in 0..r {
        for x in 0..c {
            // a warm bed: brighter toward the centre column and toward the home rows.
            let horiz = 1.0 - (x as f32 - cx).abs() / cx.max(1.0);
            let bed = 0.18 + 0.22 * horiz;
            // hot flares with a soft radial falloff.
            let mut hot = 0.0f32;
            for (fy, fx) in flares {
                let d = ((y as f32 - fy).powi(2) + (x as f32 - fx).powi(2)).sqrt();
                hot = hot.max((1.0 - d / 2.6).clamp(0.0, 1.0));
            }
            let temp = (bed + 0.75 * hot).clamp(0.0, 1.0);
            f[y * c + x] = sp.at(0.0, temp);
        }
    }
    f
}

/// A representative VITALS snapshot for the data tile's PREVIEW (a charging mouse at ~64% on stage 2)
/// — the tile shows what the data surface LOOKS like without polling hardware every tick. The live
/// applied stream reads the real device.
fn preview_vitals() -> neuron::lighting::Vitals {
    neuron::lighting::Vitals {
        battery_pct: 64,
        charging: true,
        active_stage: 1,
        stage_count: 3,
    }
}

/// Render the whole tile grid — each tile a live (effects) or representative (data) preview at time
/// `t`. Effects run the SAME `Compositor` the device does (one preset layer); the data tile runs
/// `render_vitals`; the audiometer tile reads the shared `audio_spectrum` analyser, pointed at the
/// SAME source as the device/main-preview so the thumbnail matches the board instead of faking it.
fn render_light_tiles(app: &AppWindow, sh: &SharedRt, t: f32) {
    let (rows, cols) = sh.borrow().rt.grid_dims();
    if rows == 0 || cols == 0 {
        app.global::<State>()
            .set_light_tiles(ModelRc::new(VecModel::from(Vec::<EffectTile>::new())));
        return;
    }
    let (ru, cu) = (rows as usize, cols as usize);
    // The audiometer thumbnail must request the SAME source as the device + big preview, else the two
    // callers fight over the single global provider (and its focus should show the register the user
    // actually configured). Resolve both from the configured meter layer in the stack, else the
    // preset defaults (speakers, auto).
    let (audio_source, audio_focus): (f32, f32) = sh
        .borrow()
        .light_layers
        .iter()
        .find(|d| d.pattern == "meter")
        .map(|d| (d.params.f32("source", 0.0), d.params.f32("focus", 0.0)))
        .unwrap_or((0.0, 0.0));
    // cache one COMPOSITOR per effect across ticks so stateful patterns (heat/sparkle/streak — and
    // the audio meter's per-instance level ballistics) animate; keyed by slug. Rebuilt only when
    // the grid dims change.
    thread_local! {
        static COMPS: RefCell<(u8, u8, std::collections::HashMap<&'static str, neuron::pattern::Compositor>)> =
            RefCell::new((0, 0, std::collections::HashMap::new()));
        // the audiometer tile's last-seen (source, focus) — a knob flip drops its cached compositor
        // so the tile re-points at once instead of animating the stale config.
        static METER_KNOBS: std::cell::Cell<(f32, f32)> =
            const { std::cell::Cell::new((f32::NAN, f32::NAN)) };
    }
    METER_KNOBS.with(|k| {
        if k.get() != (audio_source, audio_focus) {
            k.set((audio_source, audio_focus));
            COMPS.with(|c| {
                c.borrow_mut().2.remove("audiometer");
            });
        }
    });
    let phase = (t * 0.18).rem_euclid(1.0); // for render_vitals' charging crest
    // which effect is live on the hero render — the only tile that may pay LIVE-input costs. A
    // thumbnail that isn't the selected effect renders a representative still instead of driving the
    // heavy provider (the screen capture) — that's the always-on capture this page used to leave
    // running the whole time the LIGHTING page was open.
    let selected = app.global::<State>().get_light_effect().to_string();
    let prof = tile_prof_on();
    // INERT in prod: when `NEURON_PROF` is unset, `prof` is false and every `Instant::now()` below is
    // skipped via `bool::then`, so the render path pays nothing for the instrumentation.
    let tick_start = prof.then(std::time::Instant::now);
    let mut prof_rows: Vec<(&'static str, f64, f64)> = Vec::new();
    // SUPPRESS the per-key `GetAsyncKeyState` scan for the whole thumbnail pass: the ignite/ring/
    // streak/thermal patterns poll all 256 VKs per frame for live reactivity that's invisible at
    // thumbnail size (and the selected effect's big preview + the device stream still scan live).
    // Dropped at the end of the tile loop. ~765 syscalls/tick removed.
    let _no_keys = neuron::capture::suppress_key_reads();
    let tiles: Vec<EffectTile> = light_tile_catalog()
        .into_iter()
        .map(|(slug, name, kind)| {
            let g0 = prof.then(std::time::Instant::now);
            let frame = match kind {
                "stub" => vec![Rgb::BLACK; ru * cu], // a dark, honest "soon" tile
                // the VITALS readout tile renders through the SAME proportional renderer the APPLIED layer
                // uses (`pattern::render_vitals_bounds` over the full board) — NOT the key-anchored
                // `lighting::render_vitals` — so the swatch matches the composited surface at any grid size
                // (a zone/mouse grid included) instead of reading ~all-black off a real keyboard's keys.
                // Fed a REPRESENTATIVE snapshot (a charging mouse) so the gallery reads the look without
                // polling hardware per thumbnail; the live big-preview + device stream read the heartbeat feed.
                _ if slug == "vitals" => neuron::pattern::render_vitals_bounds(
                    preview_vitals(),
                    rows,
                    cols,
                    neuron::pattern::Bounds::board(rows, cols),
                    phase,
                ),
                // the GATED readout tiles (on air / mic light / mode held / signal): a
                // representative still of the LIT look — the preset's spectrum sampled across the
                // board at full brightness. The real patterns render dark unless their truth is
                // actually on (honest on the device, but an unreadable black square in a catalog);
                // the applied layer + big preview show the real gated behaviour.
                _ if matches!(slug, "onair" | "miclight" | "modeheld" | "signal") => {
                    let sp = preset_layer(slug).spectrum;
                    let span = cu.max(1) as f32 - 1.0;
                    (0..ru * cu)
                        .map(|i| {
                            let u = if span > 0.0 { (i % cu) as f32 / span } else { 0.0 };
                            sp.at(t, u)
                        })
                        .collect()
                }
                // the audiometer thumbnail runs the REAL meter pattern → it reads the shared, fast,
                // idle-auto-stopping `audio_spectrum` loudness provider (cheap), so the tile matches
                // the device. CACHED across ticks like the other tiles — the level ballistics are
                // per-instance state that must persist to glide — and pointed at the resolved source
                // (a knob flip drops the cache entry above).
                _ if slug == "audiometer" => COMPS.with(|c| {
                    let mut c = c.borrow_mut();
                    if c.0 != rows || c.1 != cols {
                        *c = (rows, cols, std::collections::HashMap::new());
                    }
                    let comp = c.2.entry(slug).or_insert_with(|| {
                        let mut layer = preset_layer("audiometer");
                        layer.params.set("source", audio_source);
                        layer.params.set("focus", audio_focus);
                        neuron::pattern::Compositor::from_defs(&[layer])
                    });
                    comp.render(rows, cols, t)
                }),
                // TYPING HEAT thumbnail: ALWAYS a representative still — the thumbnail pass suppresses
                // live key reads, so the thermal field would otherwise sit cold. The still shows what
                // the effect IS; live typing plays on the selected big preview + the device stream.
                _ if slug == "typingheat" => preview_thermal_frame(rows, cols),
                // AMBIENT thumbnail: render LIVE (driving the whole-desktop capture) ONLY when ambient
                // is the selected effect — then the big preview + device stream already run the capture.
                // Otherwise show a representative still and touch no provider (no ~41ms/grab StretchBlt).
                _ if slug == "ambient" && selected != "ambient" => preview_ambient_frame(rows, cols),
                _ => COMPS.with(|c| {
                    let mut c = c.borrow_mut();
                    if c.0 != rows || c.1 != cols {
                        *c = (rows, cols, std::collections::HashMap::new());
                    }
                    let comp = c
                        .2
                        .entry(slug)
                        .or_insert_with(|| neuron::pattern::Compositor::from_defs(&[preset_layer(slug)]));
                    comp.render(rows, cols, t)
                }),
            };
            let gen_us = g0.map_or(0.0, |s| s.elapsed().as_nanos() as f64 / 1000.0);
            let p0 = prof.then(std::time::Instant::now);
            let swatch = frame_to_preview(&frame, ru, cu);
            if let Some(p0) = p0 {
                prof_rows.push((slug, gen_us, p0.elapsed().as_nanos() as f64 / 1000.0));
            }
            EffectTile {
                name: name.into(),
                slug: slug.into(),
                kind: kind.into(),
                swatch,
            }
        })
        .collect();
    let u0 = prof.then(std::time::Instant::now);
    // UPDATE the rows IN PLACE, never replace the model: swapping the ModelRc destroys + recreates
    // every `for`-item in the grid — each tile card and its TouchArea — so `has-hover` reset to false
    // on every 120ms tick and a hovered card kept dropping its hover/lift until the mouse moved again
    // (the "card unfocuses after a cycle" bug). A row write updates the LIVE item (only its swatch
    // binding re-evaluates); the model is rebuilt only when the tile count itself changes.
    let state = app.global::<State>();
    let existing = state.get_light_tiles();
    match existing.as_any().downcast_ref::<VecModel<EffectTile>>() {
        Some(vm) if vm.row_count() == tiles.len() => {
            for (i, tile) in tiles.into_iter().enumerate() {
                vm.set_row_data(i, tile);
            }
        }
        _ => state.set_light_tiles(ModelRc::new(VecModel::from(tiles))),
    }
    if let (Some(u0), Some(tick_start)) = (u0, tick_start) {
        let upload_us = u0.elapsed().as_nanos() as f64 / 1000.0;
        let tick_us = tick_start.elapsed().as_nanos() as f64 / 1000.0;
        tile_prof_accumulate(tick_us, upload_us, &prof_rows);
    }
}

/// Build the `EffectParam` controls for a layer from its PATTERN's declared schema, filling each row's
/// live value from the layer's param bag. Patterns carry NO colour params (colour is the Spectrum), so
/// the colour kind never appears — the SPECTRUM editor is the colour authority.
fn params_for(def: &neuron::pattern::LayerDef) -> Vec<EffectParam> {
    use neuron::effects::ParamKind;
    let mut out = Vec::new();
    let no_opts = || ModelRc::new(VecModel::from(Vec::<SharedString>::new()));
    for p in neuron::pattern::pattern_params(&def.pattern) {
        // conditional visibility (schema-driven): a knob gated on another knob's current value
        // (e.g. the meter's audio `focus` while a CPU source is picked) is simply not rendered —
        // no dead controls. The gate key's default is its schema default via the layer's bag.
        if let Some((gate_key, allowed)) = p.only_when {
            if !allowed.contains(&def.params.u8(gate_key, 0)) {
                continue;
            }
        }
        match p.kind {
            ParamKind::Color => {} // patterns never declare colour (it lives in the spectrum)
            ParamKind::Range { min, max, default } => out.push(EffectParam {
                key: p.key.into(),
                label: p.label.into(),
                kind: "range".into(),
                fval: def.params.f32(p.key, default),
                fmin: min,
                fmax: max,
                options: no_opts(),
                ival: 0,
                hex: "".into(),
                col: slint::Color::default(),
                bval: false,
            }),
            ParamKind::Enum { options, default } => {
                let opts: Vec<SharedString> = options.iter().map(|o| (*o).into()).collect();
                out.push(EffectParam {
                    key: p.key.into(),
                    label: p.label.into(),
                    kind: "enum".into(),
                    fval: 0.0,
                    fmin: 0.0,
                    fmax: 0.0,
                    options: ModelRc::new(VecModel::from(opts)),
                    ival: def.params.u8(p.key, default) as i32,
                    hex: "".into(),
                    col: slint::Color::default(),
                    bval: false,
                });
            }
            ParamKind::Toggle { default } => out.push(EffectParam {
                key: p.key.into(),
                label: p.label.into(),
                kind: "toggle".into(),
                fval: 0.0,
                fmin: 0.0,
                fmax: 0.0,
                options: no_opts(),
                ival: 0,
                hex: "".into(),
                col: slint::Color::default(),
                bval: def.params.bool(p.key, default),
            }),
        }
    }
    out
}

/// The tile slug to highlight + keep the inspector open for: the preset the active layer EXACTLY
/// matches, else the first preset using the layer's pattern (so a customised layer still shows a
/// representative tile and the inspector stays open — `light-effect` must stay non-empty).
fn tile_slug_for_layer(def: &neuron::pattern::LayerDef) -> &'static str {
    // a hand-painted frame is its OWN thing — no representative preset tile (it's reached via the brush,
    // not the gallery). Keep `light-effect` non-empty with a dedicated sentinel so the inspector + PAINT
    // tools stay open, instead of mis-highlighting an unrelated preset (the fallthrough would pick "static").
    if def.pattern == "custom" {
        return "custom";
    }
    neuron::pattern::slug_for_layer(def).unwrap_or_else(|| {
        neuron::pattern::presets()
            .into_iter()
            .find(|p| p.pattern == def.pattern)
            .map(|p| p.slug)
            .unwrap_or("static")
    })
}

/// A layer's DISPLAY NAME for the STACK selector — the preset label behind it (e.g. "Vitals",
/// "Breathing", "Heat"), so each stack cell is identifiable (the vitals readout included). Falls back to
/// the pattern's own label, then its raw key. A hand-painted frame reads as "Painted".
fn layer_label(def: &neuron::pattern::LayerDef) -> String {
    if def.pattern == "custom" {
        return "Painted".into();
    }
    let slug = tile_slug_for_layer(def);
    neuron::pattern::presets()
        .into_iter()
        .find(|p| p.slug == slug)
        .map(|p| p.label.to_string())
        .or_else(|| neuron::pattern::pattern_def(&def.pattern).map(|d| d.label.to_string()))
        .unwrap_or_else(|| def.pattern.clone())
}

/// Project the SELECTED layer's PLACEMENT into the State surface: the honest badge (`light-place-label`
/// + `light-layer-placed`) and the render's persistent outline corners (`light-place-r0..c1`, inclusive
/// grid cells; -1 = full board). An empty region reads as the whole board (no outline); a sub-region's
/// bounding box drives the badge size + the outline corners.
fn project_placement(app: &AppWindow, region: &[u32], rows: u8, cols: u8) {
    let st = app.global::<State>();
    if region.is_empty() {
        st.set_light_layer_placed(false);
        st.set_light_place_label("full board".into());
        st.set_light_place_r0(-1);
        st.set_light_place_c0(-1);
        st.set_light_place_r1(-1);
        st.set_light_place_c1(-1);
        return;
    }
    let b = neuron::pattern::Bounds::from_region(region, rows, cols);
    st.set_light_layer_placed(true);
    st.set_light_place_label(format!("{}×{} block", b.rows, b.cols).into());
    st.set_light_place_r0(b.row0 as i32);
    st.set_light_place_c0(b.col0 as i32);
    st.set_light_place_r1(b.row0 as i32 + b.rows as i32 - 1);
    st.set_light_place_c1(b.col0 as i32 + b.cols as i32 - 1);
}

/// Project the layer stack into the unified surface: the active tile slug (the selected layer), the
/// auto-rendered PATTERN param model, the stack size, and the SPECTRUM editor surface. A full-colour
/// pattern (Screen / custom / the vitals readout) reports no spectrum (honest: nothing to edit).
fn refresh_light_unified(app: &AppWindow, sh: &SharedRt) {
    let (defs, sel, active_frame, (grows, gcols)) = {
        let s = sh.borrow();
        (s.light_layers.clone(), s.selected_layer, s.active_frame, s.rt.grid_dims())
    };
    let st = app.global::<State>();
    let empty_params = || ModelRc::new(VecModel::from(Vec::<EffectParam>::new()));
    if defs.is_empty() {
        st.set_light_effect("".into());
        st.set_light_params(empty_params());
        st.set_light_has_spectrum(false);
        st.set_light_is_readout(false); // no selection → not a readout (the RATE control always applies now)
        st.set_light_stack_count(0);
        st.set_light_stack_labels(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        project_placement(app, &[], 0, 0); // no layer → full-board readout, no outline
        clear_spectrum_surface(app);
        return;
    }
    let sel = sel.min(defs.len() - 1);
    let active = &defs[sel];
    st.set_light_effect(tile_slug_for_layer(active).into());
    // the selected layer's honesty flag: a READOUT (vitals) is a live device gauge, not a tunable light
    // show — the inspector swaps the tunable-effect knobs for a short "what this is" note. Registry-driven
    // (no pattern-key string-matching); an empty stack has no selection, so it's false in the branch above.
    st.set_light_is_readout(neuron::pattern::pattern_is_readout(&active.pattern));
    st.set_light_params(ModelRc::new(VecModel::from(params_for(active))));
    st.set_light_stack_count(defs.len() as i32);
    // STACK selector labels — each layer's display name (bottom→top) so the strip reads as its effects /
    // readouts and the user can SEE + pick which layer to place (the vitals readout included).
    let labels: Vec<SharedString> = defs.iter().map(|d| layer_label(d).into()).collect();
    st.set_light_stack_labels(ModelRc::new(VecModel::from(labels)));
    // the SELECTED layer's placement — the honest badge (`light-place-label`) + the render's persistent
    // outline corners. An empty region reads as the whole board; a sub-rect drives the outline.
    project_placement(app, &active.region, grows, gcols);
    // the SPECTRUM editor — capability-driven (registry `has_spectrum`), so the full-colour patterns
    // (Screen/Ambient, a hand-painted `custom` frame, the vitals readout) that own their pixels directly
    // hide the ramp editor, and every scalar pattern shows it — no app-side pattern-key string-matching.
    let has_spectrum = neuron::pattern::pattern_has_spectrum(&active.pattern);
    st.set_light_has_spectrum(has_spectrum);
    if has_spectrum {
        project_spectrum(app, &active.spectrum, active_frame);
    } else {
        clear_spectrum_surface(app);
    }
}

/// Clear the spectrum-editor surface (no stops / no sequence) — used for data + full-colour Screen layers.
fn clear_spectrum_surface(app: &AppWindow) {
    let st = app.global::<State>();
    st.set_light_stops(ModelRc::new(VecModel::from(Vec::<SpectrumStop>::new())));
    st.set_light_seq(ModelRc::new(VecModel::from(Vec::<SpectrumFrame>::new())));
    st.set_light_active_frame(0);
    st.set_light_motion("hold".into());
    st.set_light_interp("rgb".into());
}

/// Project a spectrum into the State spectrum-editor surface: the ACTIVE frame's palette stops +
/// motion, plus the whole sequence (timeline chips), the loop policy, and the scrubber index.
fn project_spectrum(app: &AppWindow, sp: &neuron::spectrum::Spectrum, active_frame: usize) {
    use neuron::spectrum::Motion;
    let st = app.global::<State>();
    // A Spectrum always carries ≥1 frame (every constructor guarantees it), but guard the index for
    // REAL instead of leaning on a `.max(1)` that quietly wouldn't protect `seq[fr]` if that invariant
    // ever broke: clamp, then `get` — an empty seq clears the surface rather than panicking.
    let fr = active_frame.min(sp.seq.len().saturating_sub(1));
    let Some(frame) = sp.seq.get(fr) else {
        clear_spectrum_surface(app);
        return;
    };
    let palette = &frame.palette;
    // gradient strip — the active frame's stops
    let stops: Vec<SpectrumStop> = palette
        .stops
        .iter()
        .enumerate()
        .map(|(i, s)| SpectrumStop {
            idx: i as i32,
            hex: s.col.to_hex().to_lowercase().into(),
            col: rgb_to_color(s.col),
            at: s.at,
        })
        .collect();
    st.set_light_stops(ModelRc::new(VecModel::from(stops)));
    // motion row
    let (kind, speed, depth, chaos) = match palette.motion {
        Motion::Hold => ("hold", 1.0, 0.5, 0.5),
        Motion::Drift { speed } => ("drift", speed, 0.5, 0.5),
        Motion::Cycle { speed } => ("cycle", speed, 0.5, 0.5),
        Motion::Breathe { speed, depth } => ("breathe", speed, depth, 0.5),
        Motion::Flow { speed, chaos } => ("flow", speed, 0.5, chaos),
    };
    st.set_light_motion(kind.into());
    st.set_light_motion_speed(speed);
    st.set_light_motion_depth(depth);
    st.set_light_motion_chaos(chaos);
    // gradient interpolation space (rgb default | hsv perceptual)
    st.set_light_interp(palette.interp.as_str().into());
    // timeline — one chip per frame (a live strip of its palette)
    let seq: Vec<SpectrumFrame> = sp
        .seq
        .iter()
        .enumerate()
        .map(|(i, f)| SpectrumFrame {
            idx: i as i32,
            hold: f.hold,
            fade: f.fade,
            ease: f.ease.as_str().into(),
            swatch: palette_strip_image(&f.palette),
        })
        .collect();
    st.set_light_seq(ModelRc::new(VecModel::from(seq)));
    st.set_light_active_frame(fr as i32);
    st.set_light_loop(sp.play.as_str().into());
}

/// Render a small horizontal strip of a palette (the timeline frame chip) — samples the gradient across
/// the width so a frame reads as its colour program at a glance.
fn palette_strip_image(palette: &neuron::spectrum::Palette) -> Image {
    let (w, h) = (96usize, 14usize);
    let mut buf = SharedPixelBuffer::<Rgba8Pixel>::new(w as u32, h as u32);
    {
        let px = buf.make_mut_slice();
        for x in 0..w {
            let u = if w > 1 { x as f32 / (w as f32 - 1.0) } else { 0.0 };
            let c = palette.sample(u);
            for y in 0..h {
                px[y * w + x] = Rgba8Pixel { r: c.r, g: c.g, b: c.b, a: 255 };
            }
        }
    }
    Image::from_rgba8(buf)
}

/// Mutate the active layer's spectrum (the closure also gets the clamped active-frame index), then
/// bump the revision + re-project + persist through the one chokepoint.
fn edit_active_spectrum(
    app: &AppWindow,
    sh: &SharedRt,
    f: impl FnOnce(&mut neuron::spectrum::Spectrum, usize),
) {
    {
        let mut s = sh.borrow_mut();
        let sel = s.selected_layer;
        let fr = s.active_frame;
        if let Some(d) = s.light_layers.get_mut(sel) {
            let fr = fr.min(d.spectrum.seq.len().saturating_sub(1));
            f(&mut d.spectrum, fr);
        }
        s.layers_rev += 1;
    }
    refresh_layers(app, sh);
}

/// Mutate the active layer's ACTIVE-FRAME palette (the gradient strip / motion editors).
fn edit_active_palette(app: &AppWindow, sh: &SharedRt, f: impl FnOnce(&mut neuron::spectrum::Palette)) {
    edit_active_spectrum(app, sh, |sp, fr| {
        if let Some(frame) = sp.seq.get_mut(fr) {
            f(&mut frame.palette);
        }
    });
}

/// Rebuild the active palette's Motion from the State motion knobs (kind/speed/depth/chaos) so a
/// motion-kind switch keeps the other knobs.
fn rebuild_motion(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    let kind = st.get_light_motion().to_string();
    let speed = st.get_light_motion_speed();
    let depth = st.get_light_motion_depth();
    let chaos = st.get_light_motion_chaos();
    edit_active_palette(app, sh, |pal| {
        pal.motion = neuron::spectrum::Motion::from_parts(&kind, speed, Some(depth), Some(chaos));
    });
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
    // seed the streamed-effect fps from the selected device's protocol default (30 for both — the
    // legacy-6 folklore was falsified by the live wire probe) and flag legacy so the page shows its
    // protocol note. The shared atomic is what a running stream reads each frame; the property
    // drives the slider + preview. The data/vitals surface ignores this — it paints on-demand, not
    // through the streamer.
    let (fps, legacy) = sh.borrow().rt.light_fps_default().unwrap_or((30, false));
    sh.borrow()
        .rt
        .light_fps
        .store(fps, std::sync::atomic::Ordering::Relaxed);
    st.set_light_fps(fps as f32);
    st.set_light_fps_legacy(legacy);
    // seed the tile grid so the live previews appear the moment a lit device is selected (the page's
    // ~90ms timer keeps them animating after that).
    render_light_tiles(app, sh, neuron::pattern::render_elapsed());
    refresh_light_unified(app, sh);
}

/// Render an activation phrase as instrument symbols for the rhythm readout:
/// ● tap · ▬ hold · ↔ appended to a pure-tap (toggle) phrase. (↔ not ⇄: Consolas renders ↔, the
/// double-bar arrow tofus.)
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
        out.push("↔");
    }
    out.join(" ")
}

/// Push an activation pattern into both view properties (the raw string + the symbol readout).
fn sync_activation_view(st: &State, pattern: &str) {
    st.set_activation_pattern(pattern.into());
    st.set_activation_display(phrase_symbols(pattern).into());
}

/// Set true to ABORT an in-flight weave capture (glyph recorder or radial preview) — the capture
/// loop polls it like ESC. The record button's toggle raises it to stop a stuck stroke; each fresh
/// capture lowers it before starting. Global because there is only ever one capture in flight.
static CANCEL_CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Monotonic capture generation — bumped (on the UI thread) each time a capture claims the UI flags.
/// `run_guarded`'s RAII cleanup only clears the flags when its generation is still the latest, so an
/// OLD worker's deferred cleanup can't clobber a NEWER capture that started right after it.
static CAPTURE_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The ONE weave capture path — used by BOTH the glyph recorder and the radial preview, so the
/// overlay behaves byte-for-byte identically for either facet of spellweaving (the only difference
/// is the [`crate::overlay::WeaveMode`] passed in). Waits for the configured activation RHYTHM,
/// spawns the live sigil, streams the accumulated stroke, flares/fizzles on release per
/// `recognized`, and returns the captured path. The sigil's fade-out is DETACHED so the return is
/// immediate — spamming weaves re-arms within one poll tick, never gated on an animation.
/// Blocking — call it on a worker thread.
///
/// CANCEL: the capture polls [`CANCEL_CAPTURE`] every tick (same path as ESC), so the record button
/// can stop an in-flight (or wedged) stroke by setting it — see [`record_gesture`]'s toggle.
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
    let path = neuron::glyph::capture_phrase_until(
        trigger,
        phrase,
        feel,
        600,
        &|| CANCEL_CAPTURE.load(std::sync::atomic::Ordering::Relaxed),
        |pts| {
            let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
            overlay.push(rel);
            on_pts(pts);
        },
    );
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

/// The armor EVERY GUI capture worker runs inside — the resilience the live cast-weave already has
/// (beacon.rs:272 wraps its cycle the same way). It makes a stranded "recording" state structurally
/// impossible, so no capture worker can ever leave the UI wedged:
///
///   1. RAII cleanup. `CaptureFlags::drop` clears `capturing-gesture` / `rich-capturing` /
///      `gesture-predict` on EVERY exit of `body` — normal return, early return, OR panic (Drop runs
///      during unwind; the crate is deliberately `panic = "unwind"`, see the root Cargo.toml). The
///      flags become a CONSEQUENCE of this call's lifetime, not a bool someone must remember to reset
///      on each path (the fragility that stranded the recorder on a mid-stroke panic).
///   2. Panic containment. A panic in `body` is caught, traced to the flight log (so it localizes like
///      every other weave event), and surfaced as a friendly status instead of silently killing the
///      worker thread.
///
/// `w` is the UI handle for the drop-cleanup + the failure status; `body` is the worker's real work
/// (capture → analyze → save → status), which owns its own weak(s) for the happy-path UI posts.
/// Which status line a capture worker's panic message lands in — so a radial-wheel failure surfaces
/// by the wheel (`radial-status`), not in the glyph panel (`gesture-status`) the user wasn't looking at.
#[derive(Clone, Copy)]
enum CaptureChannel {
    Gesture,
    Radial,
}

fn run_guarded(
    w: slint::Weak<AppWindow>,
    channel: CaptureChannel,
    generation: u64,
    body: impl FnOnce() + Send + 'static,
) {
    struct CaptureFlags {
        w: slint::Weak<AppWindow>,
        generation: u64,
    }
    impl Drop for CaptureFlags {
        fn drop(&mut self) {
            let (w, generation) = (self.w.clone(), self.generation);
            let _ = slint::invoke_from_event_loop(move || {
                // GENERATION GUARD: only clear if THIS capture is still the latest. A newer capture
                // that started right after us (the cancel-then-restart race: a sync cancel frees the
                // flag, the next capture claims it) already owns these flags, and our stale deferred
                // cleanup must not flip it back to not-capturing. The bump (entry point) and this
                // check both run on the UI thread, so the event loop serializes them either way.
                if CAPTURE_GEN.load(std::sync::atomic::Ordering::SeqCst) != generation {
                    return;
                }
                if let Some(app) = w.upgrade() {
                    let st = app.global::<State>();
                    st.set_capturing_gesture(false);
                    st.set_rich_capturing(false);
                    st.set_gesture_predict("".into());
                }
            });
        }
    }
    let _flags = CaptureFlags {
        w: w.clone(),
        generation,
    };
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).is_err() {
        crate::flight::trace("weave", "GUI capture worker panicked \u{2014} recovered", 0);
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                let msg: slint::SharedString = "capture hiccup \u{2014} nothing saved, try again".into();
                match channel {
                    CaptureChannel::Gesture => st.set_gesture_status(msg),
                    CaptureChannel::Radial => st.set_radial_status(msg),
                }
            }
        });
    }
    // `_flags` drops HERE — clearing the capture flags whether `body` returned or unwound.
}

/// Capture a gesture on a worker thread (blocking hold-to-draw), analyze it, store it, and post
/// the result back to the UI.
fn record_gesture(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    // TOGGLE — a second press while a capture is in flight (or stuck) STOPS it instead of being
    // ignored: cancel any live capture AND clear the UI, so the recorder can never strand in
    // "recording". (A worker that died mid-analysis leaves the cursor free but this flag set, with
    // the old enabled-gate the button was dead; resetting here is the always-available way out.)
    if st.get_capturing_gesture() {
        CANCEL_CAPTURE.store(true, std::sync::atomic::Ordering::Relaxed);
        st.set_capturing_gesture(false);
        st.set_gesture_predict("".into());
        st.set_gesture_status("recording stopped".into());
        return;
    }
    // lower any STALE cancel before arming a fresh recording — a prior capture's cancel (or a
    // toggle-stop) must not abort this one. The REAL race this guards: the worker spawned below
    // polls this same flag every tick, so a cancel arriving between that spawn and the worker's
    // first read would (correctly) stop it; clearing here ensures only a NEW cancel can do so.
    CANCEL_CAPTURE.store(false, std::sync::atomic::Ordering::Relaxed);
    st.set_capturing_gesture(true);
    st.set_stroke_saved(false); // a normal glyph record clears any prior rich-save reveal link
    let capture_gen = CAPTURE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
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
    std::thread::spawn(move || run_guarded(w.clone(), CaptureChannel::Gesture, capture_gen, move || {
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
                // SATURATING: `compact()` can SHRINK the buffer mid-stroke (it thins to ~half when it
                // hits max_pts), so `pts.len()` is NOT monotonic — a plain `-` underflowed usize and
                // panicked the worker (the stranded-recorder bug). Saturating makes a shrink a no-op tick.
                if pts.len() >= 4 && pts.len().saturating_sub(last_pred_len) >= 14 {
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
                // capturing-gesture + gesture-predict are cleared by run_guarded's RAII guard on exit
                if captured == 0 {
                    // a cancel is a cancel — not a user failure.
                    st.set_gesture_status("cancelled".into());
                    return;
                }
                if captured < 3 {
                    st.set_gesture_status("too short — try again".into());
                    return;
                }
                let err = with_shared_ret(|sh| {
                    let e = {
                        let mut s = sh.borrow_mut();
                        s.rt.vault.upsert_with(&next_name, word, exemplar);
                        // don't swallow a failed disk write — the glyph is live in the vault, but the
                        // status line must not claim it was saved if it wasn't.
                        s.rt.vault.save().err()
                    };
                    refresh_gestures(&app, sh);
                    e
                })
                .flatten();
                st.set_trail(ModelRc::new(VecModel::from(trail)));
                match err {
                    Some(e) => st.set_gesture_status(
                        format!("recorded '{next_name}' but not saved: {e}").into(),
                    ),
                    None => st.set_gesture_status(
                        format!("recorded '{next_name}' ({captured} pts) — click its chip to bind it")
                            .into(),
                    ),
                }
            }
        });
    }));
}

/// Like [`weave_capture`], but uses the **timestamped, un-thinned** capture ([`neuron::glyph::
/// capture_phrase_until_stamped`]) for strokelab research dumps — returns the raw path AND its
/// per-sample timestamps. Same overlay + cancel wiring, so drawing feels identical to recording.
#[allow(clippy::too_many_arguments)]
fn weave_capture_stamped(
    trigger: i32,
    phrase: &neuron::feel::Phrase,
    feel: &neuron::feel::FeelConfig,
    mode: crate::overlay::WeaveMode,
    mut on_pts: impl FnMut(&[neuron::glyph::C]),
) -> (Vec<neuron::glyph::C>, Vec<u32>) {
    let _editor = crate::beacon::EditorWeave::engage();
    let overlay = crate::overlay::SpellOverlay::spawn();
    overlay.begin(mode);
    let (path, stamps) = neuron::glyph::capture_phrase_until_stamped(
        trigger,
        phrase,
        feel,
        crate::strokelab::RICH_MAX_PTS,
        &|| CANCEL_CAPTURE.load(std::sync::atomic::Ordering::Relaxed),
        |pts| {
            let rel: Vec<(f32, f32)> = pts.iter().map(|c| (c.re as f32, c.im as f32)).collect();
            overlay.push(rel);
            on_pts(pts);
        },
    );
    overlay.recognized(path.len() >= 3);
    overlay.end();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(350));
        drop(overlay);
    });
    (path, stamps)
}

/// strokelab: Shift+Record. Capture a FULL rich weave at sensor fidelity (with per-sample
/// timestamps, never thinned) and write the whole eigenmotion bundle to `./strokes/` — the vault is
/// NOT touched. The capture path is byte-identical to [`record_gesture`]'s; only the destination
/// differs (a research file instead of a learned gesture). A second press cancels, same as record.
fn record_rich_stroke(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    // TOGGLE — a press while capturing stops it (mirrors record_gesture; the flag is shared).
    if st.get_capturing_gesture() {
        CANCEL_CAPTURE.store(true, std::sync::atomic::Ordering::Relaxed);
        st.set_capturing_gesture(false);
        st.set_rich_capturing(false);
        st.set_gesture_predict("".into());
        st.set_gesture_status("recording stopped".into());
        return;
    }
    CANCEL_CAPTURE.store(false, std::sync::atomic::Ordering::Relaxed);
    st.set_capturing_gesture(true);
    st.set_rich_capturing(true); // drives the panel's distinct "different mode" treatment
    st.set_stroke_saved(false); // no reveal link until this capture actually writes a file
    let capture_gen = CAPTURE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let (trigger, phrase) = {
        let s = sh.borrow();
        (s.rt.cast.trigger, s.rt.cast.phrase())
    };
    st.set_gesture_status(
        format!(
            "RICH STROKE → file · {} and draw — ESC cancels",
            st.get_cast_trigger_label()
        )
        .into(),
    );
    let cfg = sh.borrow().rt.vault.config;
    // a snapshot of the vault rides along so the dump can record what this stroke MATCHED (read
    // off the existing fingerprints) — research metadata, never a write.
    let vault = sh.borrow().rt.vault.clone();
    let w = app.as_weak();
    let trail_w = app.as_weak();
    std::thread::spawn(move || run_guarded(w.clone(), CaptureChannel::Gesture, capture_gen, move || {
        let feel = neuron::feel::FeelConfig::load();
        let mut last_trail_len = 0usize;
        let (path, stamps) = weave_capture_stamped(
            trigger,
            &phrase,
            &feel,
            crate::overlay::WeaveMode::Glyph { hint: None },
            |pts| {
                // stream the live trail onto the panel canvas (~every 14 new points), exactly like
                // record_gesture, so the rich stroke forms on screen as you draw it.
                // SATURATING (see record_gesture): a shrinking buffer must never underflow the throttle.
                if pts.len() >= 4 && pts.len().saturating_sub(last_trail_len) >= 14 {
                    last_trail_len = pts.len();
                    let trail = normalize_trail(pts);
                    let ui = trail_w.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(app) = ui.upgrade() {
                            app.global::<State>()
                                .set_trail(ModelRc::new(VecModel::from(trail)));
                        }
                    });
                }
            },
        );
        let captured = path.len();
        let trail = normalize_trail(&path);
        // write the bundle off the UI thread (file I/O + engram encode can take a beat on a long stroke).
        let saved = if captured >= 3 {
            Some(crate::strokelab::dump_stroke(&path, &stamps, &cfg, &vault))
        } else {
            None
        };
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                // capturing-gesture / rich-capturing / gesture-predict cleared by run_guarded's guard
                st.set_stroke_saved(false); // only a genuine save (below) lights the reveal link
                if captured == 0 {
                    st.set_gesture_status("cancelled".into());
                    return;
                }
                if captured < 3 {
                    st.set_gesture_status("too short — try again".into());
                    return;
                }
                st.set_trail(ModelRc::new(VecModel::from(trail)));
                match saved {
                    Some(Ok((json, _gwyph, n))) => {
                        let name = json
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| "stroke.json".into());
                        st.set_gesture_status(
                            format!("saved strokes/{name} ({n} pts) + .gwyph · click to reveal").into(),
                        );
                        st.set_stroke_saved(true); // the status line is now a clickable reveal link
                    }
                    Some(Err(e)) => {
                        st.set_gesture_status(format!("stroke save failed: {e}").into());
                    }
                    None => st.set_gesture_status("too short — try again".into()),
                }
            }
        });
    }));
}

/// Test the wheel: hold the cast trigger and flick. Goes through the EXACT same [`weave_capture`]
/// path as glyph recording — only the overlay mode differs (the sector wheel instead of the rune
/// ring) — so the radial flick and the rich glyph really are one system. Feedback lands in
/// `radial-status`, beside the wheel the user is looking at.
fn preview_radial(app: &AppWindow, sh: &SharedRt) {
    let st = app.global::<State>();
    CANCEL_CAPTURE.store(false, std::sync::atomic::Ordering::Relaxed); // a prior cancel must not abort this
    st.set_capturing_gesture(true);
    st.set_editing_sector(-1); // a flick is navigation, not an edit continuation
    let capture_gen = CAPTURE_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
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
    std::thread::spawn(move || run_guarded(w.clone(), CaptureChannel::Radial, capture_gen, move || {
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
                // (capturing-gesture is cleared by run_guarded's RAII guard on worker exit)
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
    }));
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

/// Open a path in the OS file manager. The ONE cross-platform reveal seam — Windows `explorer`,
/// macOS `open`, everything else `xdg-open` — so the GUI never hardcodes a Windows-only spawn. Best
/// effort: a missing handler just no-ops (the caller already ensured the path exists).
fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
    #[cfg(all(not(windows), not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

fn open_config_dir() {
    let dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    open_in_file_manager(&dir);
}

/// Reveal the macros folder (`macros/scripts/`) in the OS file manager — the Workshop's "drop a .py
/// here" affordance. The core's `macros_dir()` is cwd-relative, so resolve it to an ABSOLUTE path
/// (join the run dir) and CREATE it if missing, so reveal works on a fresh install and a user can
/// drop scripts in before any macro has been saved.
fn reveal_macros_folder() {
    let dir = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join(neuron::macros::macro_host::macros_dir());
    let _ = std::fs::create_dir_all(&dir);
    open_in_file_manager(&dir);
}

/// Reveal the strokelab output folder (`./strokes/`, cwd-relative like the vault) in the OS file
/// manager — clicking a "saved strokes/…" line jumps you to the exported `.gwyph` + `.json`. Resolve
/// to an ABSOLUTE path and create it if missing, so reveal works even before the first capture.
fn reveal_strokes_folder() {
    let dir = std::env::current_dir()
        .unwrap_or_else(|_| ".".into())
        .join("strokes");
    let _ = std::fs::create_dir_all(&dir);
    open_in_file_manager(&dir);
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
/// CONNECTIONS truth → the System card: pref gates into the toggles, PORT truth into the
/// per-protocol lines (a bind that failed — real Synapse holding the socket, a second
/// instance — reads as busy, never green), and the bridged-device count underneath.
pub fn refresh_host_status(app: &AppWindow) {
    let st = app.global::<State>();
    let s = crate::host::status();
    // The master toggle shows the LIVE host state, never the saved preference: a bring-up can
    // fail (election lost to another neuron instance, registry error) while the pref still
    // says "on" — the pref is INTENT (kept, so the next launch retries; both the boot path and
    // the live toggle persist intent), the toggle is TRUTH.
    st.set_host_enabled(s.active);
    st.set_host_chroma(crate::prefs::host_chroma());
    st.set_host_openrgb(crate::prefs::host_openrgb());
    st.set_host_obs(crate::prefs::host_obs());
    st.set_host_lighting_wins(crate::prefs::host_lighting_wins());
    // NB: the password field is deliberately NOT touched here. This runs ~1s while the System
    // page is up, and "empty" is indistinguishable from "the user just cleared the field to
    // remove/replace the secret" — reseeding on empty would let the poller fight that edit and
    // repopulate the old password. The field is seeded ONCE at startup (in `install`, right after
    // the first `refresh_host_status`) and is user-owned thereafter; the saved secret only
    // changes on accept/Save.
    let line = |gate: bool, serving: bool, port: u16| -> String {
        match (gate, s.active, serving) {
            (false, _, _) => "off".into(),
            (true, false, _) => "waiting for connections to open".into(),
            (true, true, true) => format!("ready on 127.0.0.1:{port}"),
            (true, true, false) => format!("port {port} is in use, maybe by Razer Synapse"),
        }
    };
    // Once a client is actually connected, the row stops talking about ports and starts
    // talking about WHO: "Overwatch is painting your keyboard + mouse". Read from the same
    // leased arbiter claims the boards obey, so the line can never describe paint that
    // isn't happening (a vanished game ages out with its lease).
    let client_line = |clients: &[crate::host::ClientStatus]| -> Option<String> {
        if clients.is_empty() {
            return None;
        }
        let name = |c: &crate::host::ClientStatus| -> String {
            if c.name.is_empty() {
                "an unnamed app".into()
            } else {
                c.name.clone()
            }
        };
        let join = |cs: &[&crate::host::ClientStatus]| -> String {
            cs.iter().map(|c| name(c)).collect::<Vec<_>>().join(" + ")
        };
        let painting: Vec<&crate::host::ClientStatus> =
            clients.iter().filter(|c| !c.painting.is_empty()).collect();
        let all: Vec<&crate::host::ClientStatus> = clients.iter().collect();
        Some(if !painting.is_empty() {
            let verb = if painting.len() == 1 { "is" } else { "are" };
            let mut kinds: Vec<String> = Vec::new();
            for c in &painting {
                for k in &c.painting {
                    if !kinds.iter().any(|x| x == k) {
                        kinds.push(k.clone());
                    }
                }
            }
            format!("{} {verb} painting your {}", join(&painting), kinds.join(" + "))
        } else if clients.iter().any(|c| c.has_claim) {
            format!("{} is waiting underneath your lighting", join(&all))
        } else {
            format!("{} is connected, not painting yet", join(&all))
        })
    };
    st.set_host_chroma_status(
        client_line(&s.chroma_clients)
            .unwrap_or_else(|| line(crate::prefs::host_chroma(), s.chroma_serving, 54235))
            .into(),
    );
    st.set_host_openrgb_status(
        client_line(&s.openrgb_clients)
            .unwrap_or_else(|| line(crate::prefs::host_openrgb(), s.openrgb_serving, 6742))
            .into(),
    );
    // The live lamps: lit only when the gate is on AND it's actually up. Games/tools light when the
    // port is bound; OBS lights when the websocket authenticates. Dark otherwise (off, port busy, or
    // still looking), so the lamp coming on is the honest "it connected" moment.
    st.set_host_chroma_live(crate::prefs::host_chroma() && s.chroma_serving);
    st.set_host_openrgb_live(crate::prefs::host_openrgb() && s.openrgb_serving);
    st.set_host_obs_live(crate::prefs::host_obs() && s.obs_connected);
    // OBS is an OUTBOUND connection, so its truth is a real connected state: "connected to OBS"
    // once the websocket authenticates, "looking for OBS" while it retries (OBS closed, or its
    // server off). The password field + finder graphic below help the user get there. Once
    // connected, the line carries what OBS itself ANNOUNCED (scene, live, recording) — a readout
    // that keeps proving the events actually flow.
    st.set_host_obs_status(
        match (crate::prefs::host_obs(), s.active, s.obs_connected) {
            (false, _, _) => "off".into(),
            (true, false, _) => "waiting for connections to open".into(),
            (true, true, false) => "looking for OBS on 127.0.0.1:4455".into(),
            (true, true, true) => {
                let mut line: String = if s.obs_scene.is_empty() {
                    "connected to OBS on 127.0.0.1:4455".into()
                } else {
                    format!("connected to OBS · scene: {}", s.obs_scene)
                };
                if s.obs_streaming {
                    line.push_str(" · LIVE");
                }
                if s.obs_recording {
                    line.push_str(" · recording");
                }
                line
            }
        }
        .into(),
    );
    st.set_host_devices_line(
        if s.active {
            let n = s.devices;
            format!("Neuron is sharing {n} {}.", if n == 1 { "device" } else { "devices" })
        } else {
            String::new()
        }
        .into(),
    );
}

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
    open_in_file_manager(&path);
}

#[cfg(test)]
mod suggestion_tests {
    use super::app_stem;

    #[test]
    fn app_stem_peels_ue_packaging_but_keeps_real_hyphens() {
        // Unreal packaging suffixes are peeled (config tag, then platform tag)…
        assert_eq!(
            app_stem("C:\\Games\\FN\\FortniteClient-Win64-Shipping.exe"),
            "FortniteClient"
        );
        assert_eq!(app_stem("PubgClient-Win64-Test.exe"), "PubgClient");
        assert_eq!(app_stem("Game-WinGDK-Shipping.exe"), "Game");
        // …but a real name's OWN hyphens/underscores survive whole (the bug this guards).
        assert_eq!(app_stem("Counter-Strike.exe"), "Counter-Strike");
        assert_eq!(app_stem("Apex_Legends.exe"), "Apex_Legends");
        assert_eq!(app_stem("chrome.exe"), "chrome");
    }
}

#[cfg(test)]
mod macro_canvas_tests {
    //! The macro CONSTRUCTOR's tree machinery — the path scheme, the node resolvers, the edit-op
    //! semantics, the add-defaults, and the flatten layout. These are the pure functions the canvas
    //! callbacks delegate to (the callbacks themselves are thin Slint glue around them), so proving
    //! these proves the builder. No Python, no UI — just the tree.
    use super::*;
    use neuron::macros::{MacroNode, Value};

    /// A small fixture tree: [type "hi", ask{yes:[notify], no:[open]}, press ctrl+c].
    fn fixture() -> Vec<MacroNode> {
        vec![
            MacroNode::Type {
                text: Value::str("hi"),
                ghost: false,
                speed: None,
            },
            MacroNode::Ask {
                question: Value::str("go?"),
                description: Value::empty_str(),
                yes: vec![MacroNode::Notify {
                    text: Value::str("yes"),
                }],
                no: vec![MacroNode::Open {
                    command: Value::str("notepad"),
                    capture: None,
                }],
            },
            MacroNode::Press {
                keys: vec!["ctrl".into(), "c".into()],
            },
        ]
    }

    #[test]
    fn node_at_path_resolves_root_and_branches() {
        let mut t = fixture();
        assert!(matches!(
            node_at_path(&mut t, "0"),
            Some(MacroNode::Type { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "1"),
            Some(MacroNode::Ask { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "1.yes.0"),
            Some(MacroNode::Notify { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "1.no.0"),
            Some(MacroNode::Open { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "2"),
            Some(MacroNode::Press { .. })
        ));
    }

    #[test]
    fn node_at_path_rejects_bad_paths() {
        let mut t = fixture();
        assert!(node_at_path(&mut t, "").is_none());
        assert!(node_at_path(&mut t, "9").is_none()); // out of range
        assert!(node_at_path(&mut t, "0.yes.0").is_none()); // a Type has no branches
        assert!(node_at_path(&mut t, "1.maybe.0").is_none()); // not a real arm
        assert!(node_at_path(&mut t, "1.yes.5").is_none()); // arm index out of range
    }

    #[test]
    fn nested_ask_path_recurses() {
        // an ask whose yes-branch holds another ask — "1.yes.0.no.0" must reach the inner open.
        let mut t = vec![
            MacroNode::Notify {
                text: Value::str("a"),
            },
            MacroNode::Ask {
                question: Value::str("outer"),
                description: Value::empty_str(),
                yes: vec![MacroNode::Ask {
                    question: Value::str("inner"),
                    description: Value::empty_str(),
                    yes: vec![],
                    no: vec![MacroNode::Open {
                        command: Value::str("x"),
                        capture: None,
                    }],
                }],
                no: vec![],
            },
        ];
        assert!(matches!(
            node_at_path(&mut t, "1.yes.0.no.0"),
            Some(MacroNode::Open { .. })
        ));
    }

    #[test]
    fn flow_arm_paths_resolve_for_every_kind() {
        // an `if` (then/else), a `for_each` (body), a `try` (body/error) — each arm token descends.
        let mut t = vec![
            MacroNode::If {
                cond: Value::Bool { b: true },
                then_: vec![MacroNode::Notify {
                    text: Value::str("t"),
                }],
                else_: vec![MacroNode::Notify {
                    text: Value::str("e"),
                }],
            },
            MacroNode::ForEach {
                var: "line".into(),
                source: Value::empty_str(),
                body: vec![MacroNode::Notify {
                    text: Value::str("b"),
                }],
            },
            MacroNode::Try {
                body: vec![MacroNode::Notify {
                    text: Value::str("ok"),
                }],
                except_: vec![MacroNode::Notify {
                    text: Value::str("err"),
                }],
            },
        ];
        assert!(matches!(
            node_at_path(&mut t, "0.then.0"),
            Some(MacroNode::Notify { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "0.else.0"),
            Some(MacroNode::Notify { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "1.body.0"),
            Some(MacroNode::Notify { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "2.body.0"),
            Some(MacroNode::Notify { .. })
        ));
        assert!(matches!(
            node_at_path(&mut t, "2.error.0"),
            Some(MacroNode::Notify { .. })
        ));
        // a wrong arm token for the kind is rejected (an `if` has no `body`).
        assert!(node_at_path(&mut t, "0.body.0").is_none());
    }

    #[test]
    fn body_at_context_targets_the_right_body() {
        let mut t = fixture();
        assert_eq!(body_at_context(&mut t, "").map(|b| b.len()), Some(3)); // root
        assert_eq!(body_at_context(&mut t, "1.yes").map(|b| b.len()), Some(1));
        assert_eq!(body_at_context(&mut t, "1.no").map(|b| b.len()), Some(1));
        assert!(body_at_context(&mut t, "0.yes").is_none()); // not an ask
    }

    #[test]
    fn default_node_covers_every_kind() {
        for kind in [
            "type",
            "press",
            "key_press",
            "click",
            "scroll",
            "move_to",
            "copy",
            "paste",
            "open",
            "focus",
            "wait",
            "notify",
            "ask",
            "if",
            "repeat_n",
            "repeat_while",
            "for_each",
            "set_var",
            "stop",
            "try",
            "raw",
        ] {
            assert!(default_node(kind).is_some(), "kind '{kind}' must default");
        }
        assert!(default_node("bogus").is_none());
        // a fresh ask brings empty branches (so the canvas shows both arms' add-points).
        let MacroNode::Ask { yes, no, question, .. } = default_node("ask").unwrap() else {
            panic!("ask");
        };
        assert!(yes.is_empty() && no.is_empty() && question == Value::empty_str());
        // a fresh if/try bring empty lanes too.
        let MacroNode::If { then_, else_, .. } = default_node("if").unwrap() else {
            panic!("if");
        };
        assert!(then_.is_empty() && else_.is_empty());
        let MacroNode::Try { body, except_ } = default_node("try").unwrap() else {
            panic!("try");
        };
        assert!(body.is_empty() && except_.is_empty());
    }

    /// edit-step semantics: setting a node's param via `edit_node_value` (the callback's core mutation).
    #[test]
    fn edit_step_sets_each_param() {
        let mut t = fixture();
        // a text-ish Value param → a Str literal.
        edit_node_value(node_at_path(&mut t, "0").unwrap(), "bye".into());
        assert_eq!(
            node_at_path(&mut t, "0"),
            Some(&mut MacroNode::Type {
                text: Value::str("bye"),
                ghost: false,
                speed: None,
            })
        );
        // an ask question is text-ish → a Str literal.
        edit_node_value(node_at_path(&mut t, "1").unwrap(), "ready?".into());
        if let Some(MacroNode::Ask { question, .. }) = node_at_path(&mut t, "1") {
            assert_eq!(*question, Value::str("ready?"));
        } else {
            panic!("ask");
        }
        // a press chord splits on + / , / space and lowercases.
        edit_node_value(node_at_path(&mut t, "2").unwrap(), "Ctrl + Shift, s".into());
        if let Some(MacroNode::Press { keys }) = node_at_path(&mut t, "2") {
            assert_eq!(*keys, vec!["ctrl", "shift", "s"]);
        } else {
            panic!("press");
        }
    }

    /// expression Value params take the edit as a Raw verbatim (the next code-parse normalizes it).
    #[test]
    fn edit_step_expr_params_become_raw() {
        let mut t = vec![
            MacroNode::If {
                cond: Value::Bool { b: true },
                then_: vec![],
                else_: vec![],
            },
            MacroNode::RepeatN {
                count: Value::Int { n: 1 },
                body: vec![],
            },
            MacroNode::ForEach {
                var: "x".into(),
                source: Value::empty_str(),
                body: vec![],
            },
            MacroNode::SetVar {
                name: "x".into(),
                value: Value::empty_str(),
            },
        ];
        edit_node_value(node_at_path(&mut t, "0").unwrap(), "ctx.app == \"a\"".into());
        assert!(matches!(
            node_at_path(&mut t, "0"),
            Some(MacroNode::If { cond: Value::Raw { .. }, .. })
        ));
        edit_node_value(node_at_path(&mut t, "1").unwrap(), "len(ctx.selection)".into());
        assert!(matches!(
            node_at_path(&mut t, "1"),
            Some(MacroNode::RepeatN { count: Value::Raw { .. }, .. })
        ));
        // for-each splits "var in source".
        edit_node_value(
            node_at_path(&mut t, "2").unwrap(),
            "line in ctx.selection.splitlines()".into(),
        );
        if let Some(MacroNode::ForEach { var, source, .. }) = node_at_path(&mut t, "2") {
            assert_eq!(var, "line");
            assert_eq!(*source, Value::raw("ctx.selection.splitlines()"));
        } else {
            panic!("for_each");
        }
        // set-var splits "name = expr".
        edit_node_value(node_at_path(&mut t, "3").unwrap(), "n = 1 + 2".into());
        if let Some(MacroNode::SetVar { name, value }) = node_at_path(&mut t, "3") {
            assert_eq!(name, "n");
            assert_eq!(*value, Value::raw("1 + 2"));
        } else {
            panic!("set_var");
        }
    }

    #[test]
    fn delete_step_removes_and_takes_branches() {
        let mut t = fixture();
        // delete the ask at index 1 — its yes/no go with it; the press shifts to index 1.
        let (body, idx) = parent_body_and_index(&mut t, "1").unwrap();
        body.remove(idx);
        assert_eq!(t.len(), 2);
        assert!(matches!(t[0], MacroNode::Type { .. }));
        assert!(matches!(t[1], MacroNode::Press { .. }));
        // delete inside a branch
        let mut t2 = fixture();
        let (body, idx) = parent_body_and_index(&mut t2, "1.no.0").unwrap();
        body.remove(idx);
        let MacroNode::Ask { no, .. } = &t2[1] else {
            panic!("ask");
        };
        assert!(no.is_empty());
    }

    #[test]
    fn move_step_reorders_within_its_body_and_clamps() {
        let mut t = fixture();
        // move index 0 down — swaps with index 1.
        let (body, idx) = parent_body_and_index(&mut t, "0").unwrap();
        let target = idx as i32 + 1;
        assert!(target >= 0 && (target as usize) < body.len());
        body.swap(idx, target as usize);
        assert!(matches!(t[0], MacroNode::Ask { .. }));
        assert!(matches!(t[1], MacroNode::Type { .. }));
        // moving the FIRST up is a clamp (no swap) — the guard rejects target < 0.
        let mut t2 = fixture();
        let (body, idx) = parent_body_and_index(&mut t2, "0").unwrap();
        let target = idx as i32 - 1;
        assert!(target < 0, "moving the head up must be rejected (clamp)");
        let _ = body; // no mutation
        assert!(matches!(t2[0], MacroNode::Type { .. }));
    }

    #[test]
    fn flatten_emits_steps_lanes_and_add_points() {
        let mut out = Vec::new();
        flatten_macro(&fixture(), 0, "", &mut out);
        // collect (row, path, kind) triples for readable assertions.
        let rows: Vec<(String, String, String)> = out
            .iter()
            .map(|b| (b.row.to_string(), b.path.to_string(), b.kind.to_string()))
            .collect();
        // expected order: step0, step1(ask), yes-lane, yes-step, yes-add, no-lane, no-step, no-add,
        // step2, root-add.
        assert_eq!(rows[0], ("step".into(), "0".into(), "type".into()));
        assert_eq!(rows[1], ("step".into(), "1".into(), "ask".into()));
        assert_eq!(rows[2], ("lane".into(), "1".into(), "yes".into()));
        assert_eq!(rows[3], ("step".into(), "1.yes.0".into(), "notify".into()));
        assert_eq!(rows[4], ("add".into(), "1.yes".into(), "".into()));
        assert_eq!(rows[5], ("lane".into(), "1".into(), "no".into()));
        assert_eq!(rows[6], ("step".into(), "1.no.0".into(), "open".into()));
        assert_eq!(rows[7], ("add".into(), "1.no".into(), "".into()));
        assert_eq!(rows[8], ("step".into(), "2".into(), "press".into()));
        // the FINAL row is the root add-point (insert context "").
        let last = rows.last().unwrap();
        assert_eq!(last, &("add".into(), "".into(), "".into()));
    }

    /// the generalized flatten exposes EVERY flow kind's lanes (if→then/else, for_each→body,
    /// try→body/error) with the right arm tokens — so the canvas can build into any of them.
    #[test]
    fn flatten_exposes_all_flow_lanes() {
        let t = vec![
            MacroNode::If {
                cond: Value::Bool { b: true },
                then_: vec![],
                else_: vec![],
            },
            MacroNode::ForEach {
                var: "x".into(),
                source: Value::empty_str(),
                body: vec![],
            },
            MacroNode::Try {
                body: vec![],
                except_: vec![],
            },
        ];
        let mut out = Vec::new();
        flatten_macro(&t, 0, "", &mut out);
        let lanes: Vec<(String, String)> = out
            .iter()
            .filter(|b| b.row == "lane")
            .map(|b| (b.path.to_string(), b.kind.to_string()))
            .collect();
        assert_eq!(
            lanes,
            vec![
                ("0".into(), "then".into()),
                ("0".into(), "else".into()),
                ("1".into(), "body".into()),
                ("2".into(), "body".into()),
                ("2".into(), "error".into()),
            ]
        );
        // each lane has its own +add insert-point with the matching arm context.
        let adds: Vec<String> = out
            .iter()
            .filter(|b| b.row == "add")
            .map(|b| b.path.to_string())
            .collect();
        assert!(adds.contains(&"0.then".to_string()));
        assert!(adds.contains(&"2.error".to_string()));
    }

    #[test]
    fn flatten_empty_tree_is_just_the_root_add() {
        let mut out = Vec::new();
        flatten_macro(&[], 0, "", &mut out);
        assert_eq!(out.len(), 1, "empty macro = the lone root +add invitation");
        assert_eq!(out[0].row, "add");
        assert_eq!(out[0].path, "");
    }

    #[test]
    fn flatten_step_verb_and_value_are_plain_language() {
        let mut out = Vec::new();
        flatten_macro(&fixture(), 0, "", &mut out);
        let step0 = &out[0];
        assert_eq!(step0.verb, "type");
        // a Str-literal text value renders as the quoted Python expr.
        assert_eq!(step0.value, "\"hi\"");
        // press value joins its keys with '+'
        let hk = out.iter().find(|b| b.kind == "press").unwrap();
        assert_eq!(hk.verb, "press");
        assert_eq!(hk.value, "ctrl+c");
    }

    /// a Value-rich step renders the value as its Python expression in the field (proving the canvas
    /// surfaces the full expression, not a flattened string).
    #[test]
    fn flatten_renders_value_expressions() {
        let t = vec![MacroNode::Notify {
            text: Value::Bin {
                op: "+".into(),
                left: Box::new(Value::str("got ")),
                right: Box::new(Value::Ctx { field: "app".into() }),
            },
        }];
        let mut out = Vec::new();
        flatten_macro(&t, 0, "", &mut out);
        assert_eq!(out[0].value, "(\"got \" + ctx.app)");
    }

    /// Round-trip: a tree → source (codegen) → the canvas keeps the source current. Proves the canvas
    /// regeneration produces source that re-parses to the same shape (the foundation's guarantee).
    #[test]
    fn codegen_from_tree_is_stable_python() {
        let src = neuron::macros::nodes_to_source(&fixture());
        assert!(src.starts_with("def macro(ctx):\n"));
        assert!(src.contains("neuron.type_text(\"hi\")"));
        assert!(src.contains("if neuron.ask(\"go?\"):"));
        assert!(src.contains("neuron.hotkey(\"ctrl\", \"c\")"));
    }
}

