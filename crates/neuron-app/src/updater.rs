// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The in-app updater: one worker that sleeps on a condvar between checks (shortly after launch,
//! then every ~12 h, when the window opens on a stale check, or when asked), downloads and
//! verifies in the background, and hands installs to the copy's own lifecycle
//! (`neuron::update::install`). The UI thread only flips request flags and receives finished
//! views; it never touches the network or the disk here. Every view is also written to
//! `update/live.json` for `neuron update`, which drives the same requests through livesync.
//! A preview (`neuron update preview`) walks the pill through its states with this build's own
//! notes, touching no network and installing nothing; it works in any build.

use crate::ui::{AppWindow, NoteMedia, NoteRow, State};
use neuron::update::{self, client, install, notes, policy, state::State as Record, version::Version, Engine, Offer};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The changelog this build shipped with; SYSTEM shows the running version's section from it.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
const RELEASES: &str = "https://github.com/worflor/neuron/releases";
const REPORT: &str = "https://github.com/worflor/neuron/issues/new";
/// The longest the worker sleeps at once, so a wall-clock deadline passed during system sleep
/// is noticed soon after resume.
const MAX_NAP: Duration = Duration::from_mins(15);
/// Notes images are drawn at most ~420 px wide; decode to twice that for high-DPI glass.
const MEDIA_MAX_WIDTH: u32 = 840;
/// Opening the window checks again when the last check is older than this.
const STALE: u64 = 3600;

/// A preview is running: the pill's requests go to it, not to the real worker.
static PREVIEW: AtomicBool = AtomicBool::new(false);
/// Bumped per preview, so a superseded preview thread stops.
static PREVIEW_RUN: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Requests {
    check: bool,
    download: bool,
    install: bool,
    dismiss: bool,
    poke: bool,
    /// Check, unless the last check is recent (the window just opened).
    stale: bool,
}

struct Ctl {
    req: Mutex<Requests>,
    cv: Condvar,
}

static CTL: OnceLock<Ctl> = OnceLock::new();
static UI: OnceLock<slint::Weak<AppWindow>> = OnceLock::new();
static RELEASE_URL: Mutex<String> = Mutex::new(String::new());

fn ctl() -> &'static Ctl {
    CTL.get_or_init(|| Ctl { req: Mutex::new(Requests::default()), cv: Condvar::new() })
}

fn request(f: impl FnOnce(&mut Requests)) {
    let c = ctl();
    f(&mut c.req.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    c.cv.notify_all();
}

/// A decoded notes image, ready to become a Slint image on the UI thread.
#[derive(Clone)]
pub(crate) struct Pixels {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    alt: String,
    /// The file's path in the notes, which names it to the player.
    id: String,
    /// An animation keeps its encoded file (at most `MAX_MEDIA_BYTES`) and is played frame by
    /// frame while on show; `rgba` is its first frame.
    anim: Option<Arc<[u8]>>,
    /// One pass of the animation, in ms.
    pass_ms: u32,
}

/// What the worker shows; `notes`/`media` replace the models only when they change.
#[derive(Clone, Default)]
struct View {
    phase: &'static str,
    version: String,
    progress: f32,
    error: String,
    queued: bool,
    status: String,
    checking: bool,
    enabled: bool,
    notes: Option<Vec<(String, String, String)>>,
    media: Option<Vec<Pixels>>,
}

/// Rows for a release, in reading order.
pub(crate) fn rows(r: &notes::Release) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    if !r.headline.is_empty() {
        // the tagline leads; the rest of the headline sits under it
        let head = r.headline.trim();
        let tag = r.tagline();
        let rest = head.strip_prefix(tag).map_or("", |s| s.trim_start_matches(['.', ':']).trim());
        out.push(("head".into(), tag.to_string(), rest.to_string()));
    }
    for s in &r.sections {
        if !s.title.is_empty() {
            out.push(("group".into(), s.title.clone(), s.intro.clone()));
        }
        for i in &s.items {
            out.push((if i.sub { "sub" } else { "item" }.into(), i.title.clone(), i.body.clone()));
        }
    }
    if !r.thanks.is_empty() {
        out.push(("thanks".into(), String::new(), format!("thanks: {}", r.thanks.join(" "))));
    }
    out
}

/// The name a lead opens with that neuron draws in its own colours: Luma-T in its ink, or an
/// effect in its palette (one colour per letter, lifted so a dark palette still reads on the
/// drawer). Returns the ink, the name, and the letters' colours.
fn name_ink(title: &str) -> Option<(&'static str, &str, Vec<slint::Color>)> {
    // exact case: leads are lowercase and names are not, so "static" stays a word
    let opens = |name: &str| title.starts_with(name) && !title[name.len()..].starts_with(|c: char| c.is_alphanumeric());
    if opens("Luma-T") {
        return Some(("luma", &title[..6], Vec::new()));
    }
    let p = neuron::pattern::presets().into_iter().find(|p| opens(p.label))?;
    let name = &title[..p.label.len()];
    let spectrum = (p.spectrum)();
    let n = name.chars().count().max(1);
    let colours = (0..n)
        .map(|i| {
            let c = spectrum.at(0.0, (i as f32 + 0.5) / n as f32);
            let (r, g, b) = (f32::from(c.r) / 255.0, f32::from(c.g) / 255.0, f32::from(c.b) / 255.0);
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let lift = if y < 0.55 { (0.55 - y) / (1.0 - y) } else { 0.0 };
            let up = |v: f32| ((v + (1.0 - v) * lift) * 255.0).round() as u8;
            slint::Color::from_rgb_u8(up(r), up(g), up(b))
        })
        .collect();
    Some(("palette", name, colours))
}

pub(crate) fn note_rows(rows: &[(String, String, String)]) -> slint::ModelRc<NoteRow> {
    let row = |(k, t, b): &(String, String, String)| {
        let mut row = NoteRow { kind: k.as_str().into(), title: t.as_str().into(), body: b.as_str().into(), ..Default::default() };
        if let ("item" | "sub", Some((ink, name, colours))) = (k.as_str(), name_ink(t)) {
            row.ink = ink.into();
            row.letters = slint::ModelRc::new(slint::VecModel::from(name.chars().map(|c| slint::SharedString::from(c.to_string())).collect::<Vec<_>>()));
            row.colours = slint::ModelRc::new(slint::VecModel::from(colours));
            row.rest = t[name.len()..].into();
        }
        row
    };
    slint::ModelRc::new(slint::VecModel::from(rows.iter().map(row).collect::<Vec<_>>()))
}

/// The stage a set of pictures shares: as tall as the tallest, so stepping between them moves
/// nothing below it.
fn stage_ratio(px: &[Pixels]) -> f32 {
    px.iter().map(|p| p.h as f32 / p.w.max(1) as f32).fold(0.0, f32::max)
}

pub(crate) fn note_media(px: &[Pixels]) -> slint::ModelRc<NoteMedia> {
    let stage = stage_ratio(px);
    for p in px {
        if let Some(a) = &p.anim {
            player::register(&p.id, a.clone());
        }
    }
    slint::ModelRc::new(slint::VecModel::from(
        px.iter()
            .map(|p| {
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&p.rgba, p.w, p.h);
                NoteMedia {
                    img: slint::Image::from_rgba8(buf),
                    alt: p.alt.as_str().into(),
                    ratio: p.h as f32 / p.w.max(1) as f32,
                    stage,
                    id: p.id.as_str().into(),
                    animated: p.anim.is_some(),
                    pass_ms: i32::try_from(p.pass_ms).unwrap_or(i32::MAX),
                }
            })
            .collect::<Vec<_>>(),
    ))
}

/// Decode one image, bounded in bytes and pixels. Off the UI thread. An animated WebP or GIF is
/// read through once here, so the player is only ever handed one that decodes to the end.
fn decode(bytes: &[u8], alt: &str, id: &str) -> Option<Pixels> {
    if bytes.len() as u64 > update::manifest::MAX_MEDIA_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let rgba = fit(reader.decode().ok()?);
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 {
        return None;
    }
    let pass = player::pass_ms(bytes);
    Some(Pixels {
        rgba: rgba.into_raw(),
        w,
        h,
        alt: alt.to_string(),
        id: id.to_string(),
        anim: pass.map(|_| Arc::from(bytes)),
        pass_ms: pass.unwrap_or(0),
    })
}

/// Scale down to the width the drawer draws at.
fn fit(img: image::DynamicImage) -> image::RgbaImage {
    if img.width() > MEDIA_MAX_WIDTH {
        let h = (u64::from(img.height()) * u64::from(MEDIA_MAX_WIDTH) / u64::from(img.width().max(1))).max(1) as u32;
        return img.resize_exact(MEDIA_MAX_WIDTH, h, image::imageops::FilterType::Triangle).to_rgba8();
    }
    img.to_rgba8()
}

/// Media for the running version's own notes, from the `docs/` the package installs beside it.
/// A source build has none beside it and reads them from the tree it was built from.
pub(crate) fn local_media(r: &notes::Release, exe_dir: &Path) -> Vec<Pixels> {
    let tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    r.media
        .iter()
        .take(update::manifest::MAX_MEDIA)
        .filter_map(|m| {
            let path = [exe_dir, tree.as_path()].iter().map(|d| d.join(&m.path)).find(|p| p.is_file())?;
            let len = std::fs::metadata(&path).ok()?.len();
            if len > update::manifest::MAX_MEDIA_BYTES {
                return None;
            }
            decode(&std::fs::read(path).ok()?, &m.alt, &m.path)
        })
        .collect()
}

/// Plays an animated notes picture while it is on show: one thread decodes its frames from the
/// encoded file as they fall due and hands each to the UI, so a long loop is never held decoded.
/// The last picture asked for plays; asking for another, or stopping, ends the one before.
pub(crate) mod player {
    use super::{fit, State, UI};
    use image::AnimationDecoder;
    use slint::ComponentHandle;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::{Duration, Instant};

    /// Frames read at most to time one pass, and the longest pass the slideshow waits out.
    const MAX_FRAMES: u32 = 2000;
    const MAX_PASS_MS: u128 = 60_000;

    static FILES: Mutex<Vec<(String, Arc<[u8]>)>> = Mutex::new(Vec::new());
    static PLAYING: Mutex<String> = Mutex::new(String::new());
    /// Bumped by every play and stop; a playing thread holding an older ticket ends.
    static TICKET: AtomicU64 = AtomicU64::new(0);

    fn frames(bytes: &[u8]) -> Option<image::Frames<'_>> {
        let cur = std::io::Cursor::new(bytes);
        match image::guess_format(bytes).ok()? {
            image::ImageFormat::WebP => {
                let d = image::codecs::webp::WebPDecoder::new(cur).ok()?;
                d.has_animation().then(|| d.into_frames())
            }
            image::ImageFormat::Gif => Some(image::codecs::gif::GifDecoder::new(cur).ok()?.into_frames()),
            _ => None,
        }
    }

    /// How long a frame stays up. As in browsers, a delay of 10 ms or less means 100 ms.
    fn hold(f: &image::Frame) -> Duration {
        let (n, d) = f.delay().numer_denom_ms();
        let ms = u64::from(n) / u64::from(d.max(1));
        Duration::from_millis(if ms <= 10 { 100 } else { ms })
    }

    /// One pass's length, when `bytes` is an animation of two or more frames that all decode.
    pub(super) fn pass_ms(bytes: &[u8]) -> Option<u32> {
        let mut total = Duration::ZERO;
        let mut n = 0u32;
        for f in frames(bytes)? {
            total += hold(&f.ok()?);
            n += 1;
            if n >= MAX_FRAMES {
                break;
            }
        }
        (n > 1).then(|| total.as_millis().min(MAX_PASS_MS) as u32)
    }

    pub(super) fn register(id: &str, bytes: Arc<[u8]>) {
        let mut files = FILES.lock().unwrap_or_else(PoisonError::into_inner);
        files.retain(|(k, _)| k != id);
        files.push((id.to_string(), bytes));
        // two releases' pictures at most are on hand: the running version's and the offer's
        let excess = files.len().saturating_sub(2 * neuron::update::manifest::MAX_MEDIA);
        files.drain(..excess);
    }

    /// Play `id` on a loop until it is stopped or another is played.
    pub fn play(id: &str) {
        let ticket = TICKET.fetch_add(1, Ordering::SeqCst) + 1;
        *PLAYING.lock().unwrap_or_else(PoisonError::into_inner) = id.to_string();
        let bytes = FILES.lock().unwrap_or_else(PoisonError::into_inner).iter().find(|(k, _)| k == id).map(|(_, b)| b.clone());
        let (Some(bytes), Some(ui)) = (bytes, UI.get().cloned()) else { return };
        let id = id.to_string();
        crate::worker::spawn_detached("neuron-notes-player", move || {
            let current = || TICKET.load(Ordering::SeqCst) == ticket;
            let mut due = Instant::now();
            while current() {
                let Some(pass) = frames(&bytes) else { return };
                for f in pass {
                    let Ok(f) = f else { return };
                    let wait = hold(&f);
                    let px = fit(image::DynamicImage::ImageRgba8(f.into_buffer()));
                    let (w, h) = px.dimensions();
                    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(px.as_raw(), w, h);
                    match due.checked_duration_since(Instant::now()) {
                        Some(early) => std::thread::sleep(early),
                        // fell behind (the machine was busy): carry on from now, never race to catch up
                        None => due = Instant::now(),
                    }
                    if !current() {
                        return;
                    }
                    let (ui, id) = (ui.clone(), id.clone());
                    let _ = slint::invoke_from_event_loop(move || {
                        if TICKET.load(Ordering::SeqCst) != ticket {
                            return;
                        }
                        if let Some(app) = ui.upgrade() {
                            let st = app.global::<State>();
                            st.set_media_frame(slint::Image::from_rgba8(buf));
                            st.set_media_frame_of(id.as_str().into());
                        }
                    });
                    due += wait;
                }
            }
        });
    }

    /// Stop `id` if it is the one playing (any, for ""); it falls back to its first frame.
    pub fn stop(id: &str) {
        let mut playing = PLAYING.lock().unwrap_or_else(PoisonError::into_inner);
        if !id.is_empty() && *playing != id {
            return;
        }
        playing.clear();
        TICKET.fetch_add(1, Ordering::SeqCst);
        if let Some(ui) = UI.get().cloned() {
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = ui.upgrade() {
                    app.global::<State>().set_media_frame_of("".into());
                }
            });
        }
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn an_animation_is_timed_and_a_still_is_not() {
            use image::codecs::gif::GifEncoder;
            let mut gif = Vec::new();
            {
                let mut enc = GifEncoder::new(&mut gif);
                for c in [0u8, 255] {
                    let buf = image::RgbaImage::from_pixel(4, 4, image::Rgba([c, c, c, 255]));
                    enc.encode_frame(image::Frame::from_parts(buf, 0, 0, image::Delay::from_numer_denom_ms(250, 1))).unwrap();
                }
            }
            assert_eq!(super::pass_ms(&gif), Some(500));
            let px = super::super::decode(&gif, "loop", "a.gif").unwrap();
            assert!(px.anim.is_some());
            assert_eq!(px.pass_ms, 500);
            let mut png = Vec::new();
            image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
            assert_eq!(super::pass_ms(&png), None);
        }
    }
}

fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default()
}

fn apply(app: &AppWindow, v: View) {
    let st = app.global::<State>();
    st.set_update_phase(v.phase.into());
    st.set_update_version(v.version.into());
    st.set_update_progress(v.progress);
    st.set_update_error(v.error.into());
    st.set_update_install_queued(v.queued);
    st.set_update_status(v.status.into());
    st.set_update_checking(v.checking);
    st.set_update_enabled(v.enabled);
    if let Some(rows) = &v.notes {
        st.set_update_notes(note_rows(rows));
    }
    if let Some(px) = &v.media {
        player::stop("");
        st.set_update_media(note_media(px));
    }
    if v.phase.is_empty() && st.get_update_notes_open() {
        st.set_update_notes_open(false);
    }
}

fn post(v: &View) {
    let live = update::state::Live {
        phase: v.phase.to_string(),
        version: v.version.clone(),
        progress: v.progress,
        error: v.error.clone(),
        status: v.status.clone(),
        checking: v.checking,
        enabled: v.enabled,
        preview: PREVIEW.load(Ordering::SeqCst),
        at_ms: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64),
    };
    if let Err(e) = live.save(&neuron::runroot::run_root()) {
        eprintln!("neuron: update status not written: {e}");
    }
    let Some(weak) = UI.get().cloned() else { return };
    let v = v.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = weak.upgrade() {
            apply(&app, v);
        }
    });
}

/// Bind the update callbacks and seed what needs no network: the switch, the running version's
/// notes, and whether this copy updates itself at all. Runs in tests too; nothing here starts
/// the worker.
pub fn install(app: &AppWindow) {
    let st = app.global::<State>();
    st.set_update_auto(crate::prefs::update_auto());
    let current = Version::current();
    if let Some(r) = notes::find(CHANGELOG, &current) {
        st.set_installed_notes(note_rows(&rows(&r)));
    }
    let env = environment();
    match policy::availability(&env) {
        Ok(_) => {
            st.set_update_enabled(true);
            st.set_update_status(format!("v{current}").into());
        }
        Err(d) => {
            st.set_update_enabled(false);
            st.set_update_status(d.describe().into());
        }
    }
    st.on_update_download(|| request(|r| r.download = true));
    st.on_update_install(|| request(|r| r.install = true));
    st.on_update_dismiss(|| request(|r| r.dismiss = true));
    st.on_update_check_now(|| request(|r| r.check = true));
    st.on_update_restart(restart);
    st.on_media_play(|id| player::play(&id));
    st.on_media_stop(|id| player::stop(&id));
    st.on_update_set_auto({
        let w = app.as_weak();
        move |v| {
            let line = crate::prefs::set_update_auto(v);
            if let Some(app) = w.upgrade() {
                let st = app.global::<State>();
                st.set_update_auto(v);
                st.set_status_line(line.into());
            }
            request(|r| r.poke = true);
        }
    });
    st.on_update_open_release(|| {
        let url = RELEASE_URL.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        crate::glue::open_external(if url.is_empty() { RELEASES } else { &url });
    });
}

/// The window opened: check again if the last check is old. Cheap: the release API answers a
/// repeat question with "not modified".
pub fn window_opened() {
    request(|r| r.stale = true);
}

/// A request from `neuron update`, through livesync.
pub fn command(op: neuron::livesync::UpdateOp) {
    use neuron::livesync::UpdateOp;
    match op {
        UpdateOp::Check => request(|r| r.check = true),
        UpdateOp::Install => request(|r| r.install = true),
        UpdateOp::Preview { phase } => preview(&phase),
        UpdateOp::EndPreview => end_preview(),
    }
}

/// The phases a preview can open on.
pub const PREVIEW_PHASES: [&str; 8] = ["available", "downloading", "ready", "installing", "installed", "updated", "rolled-back", "error"];

/// Show the pill in `phase` with this build's own notes. Its buttons then walk the states as a
/// real update would: download fills the bar, install restarts into "updated". Nothing is fetched
/// or installed; "not now" or `neuron update preview off` ends it.
fn preview(phase: &str) {
    if !PREVIEW_PHASES.contains(&phase) {
        return;
    }
    let run = PREVIEW_RUN.fetch_add(1, Ordering::SeqCst) + 1;
    PREVIEW.store(true, Ordering::SeqCst);
    let phase = phase.to_string();
    crate::worker::spawn_detached("neuron-update-preview", move || run_preview(run, &phase));
}

fn end_preview() {
    PREVIEW_RUN.fetch_add(1, Ordering::SeqCst);
    if PREVIEW.swap(false, Ordering::SeqCst) {
        post(&View { enabled: environment_allows(), status: format!("v{}", Version::current()), ..View::default() });
    }
}

fn environment_allows() -> bool {
    policy::availability(&environment()).is_ok()
}

/// The notes a preview shows: this version's section, else the newest in the bundled changelog.
fn preview_release() -> Option<notes::Release> {
    notes::find(CHANGELOG, &Version::current()).or_else(|| notes::parse(CHANGELOG).into_iter().next())
}

fn run_preview(run: u64, phase: &str) {
    let live = || PREVIEW_RUN.load(Ordering::SeqCst) == run;
    let current = Version::current();
    let next = Version { patch: current.patch + 1, pre: Vec::new(), ..current };
    let release = preview_release();
    let mut view = View {
        phase: "",
        version: next.to_string(),
        status: format!("v{next} available · preview"),
        enabled: true,
        notes: Some(release.as_ref().map(rows).unwrap_or_default()),
        media: Some(release.as_ref().map(|r| local_media(r, &exe_dir())).unwrap_or_default()),
        ..View::default()
    };
    let show = |view: &mut View, phase: &'static str| {
        view.phase = phase;
        post(view);
        view.notes = None;
        view.media = None;
    };
    // walk a download, then (when asked) an install that "restarts" into the updated card
    let download = |view: &mut View, then_install: bool| -> bool {
        view.queued = then_install;
        for step in 0..=40 {
            if !live() {
                return false;
            }
            view.progress = step as f32 / 40.0;
            show(view, "downloading");
            std::thread::sleep(Duration::from_millis(60));
        }
        view.queued = false;
        show(view, "ready");
        true
    };
    let install = |view: &mut View| {
        show(view, "installing");
        std::thread::sleep(Duration::from_millis(1600));
        if !live() {
            return;
        }
        view.status = format!("v{} · preview of the first launch after an update", view.version);
        view.notes = Some(release.as_ref().map(rows).unwrap_or_default());
        view.media = Some(release.as_ref().map(|r| local_media(r, &exe_dir())).unwrap_or_default());
        show(view, "updated");
    };
    match phase {
        "available" => show(&mut view, "available"),
        "downloading" => {
            if download(&mut view, false) {
                view.progress = 0.0;
                show(&mut view, "available");
            }
        }
        "ready" => {
            view.progress = 1.0;
            show(&mut view, "ready");
        }
        "installing" => install(&mut view),
        "updated" => show(&mut view, "updated"),
        "installed" => show(&mut view, "installed"),
        "rolled-back" => {
            view.status = format!("{next} rolled back · preview");
            show(&mut view, "rolled-back");
        }
        _ => {
            view.error = "hash mismatch · retry".into();
            show(&mut view, "error");
        }
    }
    // the pill's own buttons, until the preview ends
    while live() {
        let c = ctl();
        let req = {
            let mut g = c.req.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if !(g.download || g.install || g.dismiss || g.check) {
                g = c.cv.wait_timeout(g, Duration::from_millis(250)).unwrap_or_else(std::sync::PoisonError::into_inner).0;
            }
            if !live() {
                return;
            }
            Requests { download: std::mem::take(&mut g.download), install: std::mem::take(&mut g.install), dismiss: std::mem::take(&mut g.dismiss), check: std::mem::take(&mut g.check), ..Requests::default() }
        };
        if req.dismiss {
            end_preview();
            return;
        }
        if req.check {
            view.checking = true;
            let phase = view.phase;
            show(&mut view, phase);
            std::thread::sleep(Duration::from_millis(700));
            view.checking = false;
            show(&mut view, "available");
        }
        if req.install {
            if view.phase == "ready" || download(&mut view, true) {
                install(&mut view);
            }
        } else if req.download {
            download(&mut view, false);
        }
    }
}

/// Restart into the version just installed: a helper waits for this process to end and starts
/// the app again, then the event loop quits the normal way (devices handed back).
pub fn restart() {
    let Ok(exe) = std::env::current_exe() else { return };
    match install::spawn(&install::relaunch_after(std::process::id(), &exe)) {
        Ok(_) => {
            clean_exit();
            let _ = slint::quit_event_loop();
        }
        Err(e) => eprintln!("neuron: could not restart: {e}"),
    }
}

/// The running version's own notes images, decoded off the UI thread.
fn seed_installed_media(weak: slint::Weak<AppWindow>) {
    crate::worker::spawn_detached("neuron-notes-media", move || {
        let Some(r) = notes::find(CHANGELOG, &Version::current()) else { return };
        let px = local_media(&r, &exe_dir());
        if px.is_empty() {
            return;
        }
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                app.global::<State>().set_installed_media(note_media(&px));
            }
        });
    });
}

fn environment() -> policy::Environment {
    policy::Environment::detect(&exe_dir(), cfg!(feature = "steam"), update::release_key().is_some())
}

/// Start the worker. Live path only (main); a copy that does not update itself still gets its
/// SYSTEM line and bundled notes from [`install`].
pub fn start(weak: slint::Weak<AppWindow>) {
    let _ = UI.set(weak.clone());
    seed_installed_media(weak);
    start_health_clock();
    watch_session_end();
    let env = environment();
    let Ok(platform) = policy::availability(&env) else { return };
    let Some(key) = update::release_key() else { return };
    let engine = Engine {
        ep: client::Endpoints::github(update::REPO, key),
        run_root: neuron::runroot::run_root(),
        platform,
        current: Version::current(),
    };
    crate::worker::spawn_detached("neuron-updater", move || Worker::new(engine).run());
}

/// Run first thing at launch. A freshly installed version that has failed to prove itself
/// [`update::guard::MAX_UNPROVEN_STARTS`] times hands off to the restore and this launch ends;
/// returns whether it should.
pub fn launch_guard() -> bool {
    let verdict = update::guard::on_launch(
        &neuron::runroot::run_root(),
        &Version::current(),
        &exe_dir(),
        std::process::id(),
        &crate::flight::crash_log_path(),
        true,
    );
    let update::guard::Verdict::RollBack(handoff) = verdict else { return false };
    crate::flight::trace("update", "restoring the previous version", 0);
    match install::spawn(&handoff) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("neuron: could not start the restore: {e}");
            false
        }
    }
}

/// Count this launch as healthy once it has run [`update::guard::HEALTHY_AFTER`]. Called after the
/// window, tray and dispatch are up.
fn start_health_clock() {
    crate::worker::spawn_detached("neuron-update-health", || {
        std::thread::sleep(update::guard::HEALTHY_AFTER);
        if update::guard::proved_healthy(&neuron::runroot::run_root(), &Version::current()) {
            crate::flight::trace("update", "new version proved itself", 0);
        }
    });
}

/// Windows ending the session (shut down, restart, sign out) is a clean exit too: a new version
/// must not be rolled back because the computer was turned off within its first minute. Windows
/// sends WM_ENDSESSION only to top-level windows, so a hidden one on the UI thread listens.
#[cfg(windows)]
fn watch_session_end() {
    use std::sync::atomic::AtomicIsize;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, CreateWindowExW, SetWindowLongPtrW, GWLP_WNDPROC, WM_ENDSESSION, WNDPROC, WS_POPUP,
    };
    static PREVIOUS: AtomicIsize = AtomicIsize::new(0);
    unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_ENDSESSION && wparam != 0 {
            clean_exit();
        }
        // SAFETY: PREVIOUS holds the STATIC class's own window procedure, set before this one
        // could be called, and the arguments are passed through unchanged.
        unsafe {
            let previous: WNDPROC = std::mem::transmute::<isize, WNDPROC>(PREVIOUS.load(Ordering::SeqCst));
            CallWindowProcW(previous, hwnd, msg, wparam, lparam)
        }
    }
    const STATIC: &[u16] = &[b'S' as u16, b'T' as u16, b'A' as u16, b'T' as u16, b'I' as u16, b'C' as u16, 0];
    // SAFETY: STATIC is NUL-terminated; a hidden, parentless popup of a built-in class needs no
    // other parameters. The window lives for the process, on this (the UI) thread, whose event
    // loop dispatches its messages.
    unsafe {
        let hwnd = CreateWindowExW(0, STATIC.as_ptr(), STATIC.as_ptr(), WS_POPUP, 0, 0, 0, 0,
            std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null());
        if hwnd.is_null() {
            return;
        }
        let handler: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT = proc;
        let previous = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, handler as usize as isize);
        PREVIOUS.store(previous, Ordering::SeqCst);
    }
}

#[cfg(not(windows))]
fn watch_session_end() {}

/// A clean quit does not count against a version on probation. Once per process: a session end
/// and the quit that follows it are one exit.
pub fn clean_exit() {
    static DONE: AtomicBool = AtomicBool::new(false);
    if !DONE.swap(true, Ordering::SeqCst) {
        update::guard::clean_exit(&neuron::runroot::run_root(), &Version::current());
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(windows)]
fn elevated() -> bool {
    crate::purge::is_elevated()
}

#[cfg(not(windows))]
fn elevated() -> bool {
    false
}

struct Worker {
    engine: Engine,
    rec: Record,
    offer: Option<Offer>,
    view: View,
    launched: u64,
    cancel: AtomicBool,
    /// An install is under way: probation is this worker's to write.
    owns_guard: bool,
}

impl Worker {
    fn new(engine: Engine) -> Self {
        let rec = Record::load(&engine.run_root);
        Self { engine, rec, offer: None, view: View { enabled: true, ..View::default() }, launched: now_unix(), cancel: AtomicBool::new(false), owns_guard: false }
    }

    /// Save the record, keeping what the launch guard wrote since this worker loaded it, unless
    /// this worker is the one installing (it then writes probation itself).
    fn save(&mut self) {
        if !self.owns_guard {
            let disk = Record::load(&self.engine.run_root);
            self.rec.adopt_guard(&disk);
        }
        if let Err(e) = self.rec.save(&self.engine.run_root) {
            crate::flight::trace("update", "state not saved", 0);
            eprintln!("neuron: update state not saved: {e}");
        }
    }

    fn up_to_date(&self) -> String {
        format!("v{} · up to date", self.engine.current)
    }

    /// What the previous launch's handoff came to.
    fn landed(&mut self) {
        if let Some(p) = self.rec.pending.take() {
            match install::landed(&p, &self.engine.current) {
                install::Landed::Updated(v) => self.rec.fresh_install = Some(v),
                install::Landed::Failed { to, reason } => {
                    self.view.phase = "error";
                    self.view.version = to;
                    self.view.error = reason;
                }
            }
            self.save();
        }
        let current = self.engine.current.to_string();
        match self.rec.fresh_install.clone() {
            Some(v) if v == current => {
                self.view.phase = "updated";
                self.view.version = v;
                if let Some(r) = notes::find(CHANGELOG, &self.engine.current) {
                    self.view.notes = Some(rows(&r));
                    self.view.media = Some(local_media(&r, &exe_dir()));
                }
            }
            Some(_) => {
                self.rec.fresh_install = None;
                self.save();
            }
            None => {}
        }
        self.view.status = format!("v{current}");
    }

    fn run(mut self) {
        self.landed();
        if let Some(rb) = update::guard::take_rollback_notice(&self.engine.run_root, &self.engine.current) {
            crate::flight::trace("update", "rolled back a version that did not start", 0);
            self.view.phase = "rolled-back";
            self.view.version.clone_from(&rb.from);
            self.view.status = format!("{} rolled back{}", rb.from, if rb.crash_log.is_empty() { String::new() } else { format!(" · crash log in {}", rb.crash_log) });
            *RELEASE_URL.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = REPORT.into();
        }
        self.publish();
        loop {
            let due = policy::next_check(self.rec.last_check, self.rec.failures, self.rec.retry_at, self.launched);
            let req = self.wait(due);
            if req.dismiss {
                self.dismiss();
            }
            let timed = crate::prefs::update_auto() && now_unix() >= due;
            let stale = req.stale && crate::prefs::update_auto() && self.rec.last_check.is_none_or(|t| now_unix().saturating_sub(t) >= STALE);
            if req.check || timed || stale || ((req.download || req.install) && self.offer.is_none()) {
                self.check(req.check);
            }
            if req.download || req.install {
                if req.install {
                    self.view.queued = true;
                }
                if self.download() && self.view.queued {
                    self.install();
                }
            }
        }
    }

    fn wait(&self, due: u64) -> Requests {
        let c = ctl();
        let mut g = c.req.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if !PREVIEW.load(Ordering::SeqCst) && (g.check || g.download || g.install || g.dismiss || g.poke || g.stale) {
                return std::mem::take(&mut *g);
            }
            let now = now_unix();
            if !PREVIEW.load(Ordering::SeqCst) && crate::prefs::update_auto() && now >= due {
                return Requests::default();
            }
            let nap = Duration::from_secs(due.saturating_sub(now).max(1)).min(MAX_NAP);
            g = c.cv.wait_timeout(g, nap).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }

    fn publish(&self) {
        post(&self.view);
    }

    fn dismiss(&mut self) {
        if self.view.phase == "rolled-back" {
            RELEASE_URL.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
        } else if self.view.phase == "updated" {
            self.rec.fresh_install = None;
        } else if let Some(o) = &self.offer {
            self.rec.dismissed = Some(update::state::Dismissal { version: o.version.to_string(), at: now_unix() });
        }
        self.save();
        self.view.phase = "";
        self.view.notes = None;
        self.view.media = None;
        self.publish();
    }

    fn check(&mut self, manual: bool) {
        self.view.checking = true;
        self.view.notes = None;
        self.view.media = None;
        self.publish();
        let result = self.engine.check(&mut self.rec);
        self.view.checking = false;
        match result {
            Ok(Some(offer)) => self.offered(offer, manual),
            Ok(None) => {
                self.offer = None;
                if matches!(self.view.phase, "available" | "ready" | "error") {
                    self.view.phase = "";
                }
                self.view.status = self.up_to_date();
            }
            Err(e) if e.transient() => {
                self.view.status = match &e {
                    client::Error::RateLimited { .. } => "rate-limited · retries on its own".into(),
                    _ => "offline · retries on its own".into(),
                };
            }
            Err(e) => {
                crate::flight::trace("update", "check refused a release", 0);
                eprintln!("neuron: update check: {e}");
                self.view.status = match &e {
                    client::Error::Io(_) | client::Error::DiskFull => format!("update folder: {}", e.short()),
                    _ => format!("latest release refused: {}", e.short()),
                };
                if manual {
                    self.view.phase = "error";
                    self.view.error = e.short();
                    self.view.version = self.rec.release.as_ref().map(|r| r.tag.trim_start_matches('v').to_string()).unwrap_or_default();
                }
            }
        }
        self.save();
        self.publish();
    }

    fn offered(&mut self, offer: Offer, manual: bool) {
        let v = offer.version.clone();
        *RELEASE_URL.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
            if offer.manifest.notes_url.starts_with("https://") { offer.manifest.notes_url.clone() } else { offer.release.html_url.clone() };
        let dismissed = self.rec.dismissed.as_ref().and_then(|d| Version::parse(&d.version).map(|dv| (dv, d.at)));
        let show = manual || policy::offer(&v, &self.engine.current, dismissed.as_ref().map(|(dv, at)| (dv, *at)), now_unix());
        self.view.status = format!("v{v} available");
        if show {
            if manual {
                self.rec.dismissed = None;
            }
            self.view.phase = if self.engine.package_ready(&offer) { "ready" } else { "available" };
            self.view.version = v.to_string();
            self.view.error.clear();
            self.view.notes = Some(offer.manifest.release_notes().map(|r| rows(&r)).unwrap_or_default());
            self.view.media = Some(Vec::new());
            if self.rec.announced.as_deref() != Some(&v.to_string()) {
                self.rec.announced = Some(v.to_string());
                neuron::confirm::update_available(&v.to_string());
            }
        }
        self.offer = Some(offer.clone());
        self.publish();
        if show {
            let px: Vec<Pixels> = self
                .engine
                .media(&offer)
                .into_iter()
                .filter_map(|(m, bytes)| {
                    let alt = offer.manifest.release_notes().and_then(|r| r.media.into_iter().find(|x| x.path == m.path)).map(|x| x.alt).unwrap_or_default();
                    decode(&bytes, &alt, &m.path)
                })
                .collect();
            self.view.media = Some(px);
            self.view.notes = None;
            self.publish();
            self.view.media = None;
        }
        self.view.notes = None;
    }

    /// Download the offer's package, retrying an interrupted transfer from where it stopped.
    /// Returns whether a verified package is on disk.
    fn download(&mut self) -> bool {
        let Some(offer) = self.offer.clone() else { return false };
        if self.engine.package_ready(&offer) {
            self.view.phase = "ready";
            self.publish();
            return true;
        }
        let size = offer.asset().map_or(1, |a| a.size.max(1));
        self.view.phase = "downloading";
        self.view.version = offer.version.to_string();
        self.view.progress = self.engine.package_progress(&offer) as f32 / size as f32;
        self.publish();
        let mut result = Err(client::Error::Offline);
        for attempt in 0..4u32 {
            if attempt > 0 {
                std::thread::sleep(Duration::from_secs(2u64 << attempt));
            }
            let mut last = Instant::now();
            let view = &mut self.view;
            let mut progress = |done: u64, total: u64| {
                if last.elapsed() >= Duration::from_millis(150) || done == total {
                    last = Instant::now();
                    view.progress = done as f32 / total.max(1) as f32;
                    view.queued = view.queued || take_install_request();
                    post(view);
                }
            };
            result = self.engine.download(&offer, &mut progress, &self.cancel);
            if !matches!(&result, Err(e) if e.transient()) {
                break;
            }
        }
        match result {
            Ok(_) => {
                self.view.phase = "ready";
                self.view.progress = 1.0;
                self.publish();
                true
            }
            Err(e) => {
                crate::flight::trace("update", "download failed", 0);
                eprintln!("neuron: update download: {e}");
                self.view.phase = "error";
                self.view.error = e.short();
                self.view.queued = false;
                self.publish();
                false
            }
        }
    }

    fn fail(&mut self, reason: String) {
        self.owns_guard = true;
        self.rec.pending = None;
        if let Some(p) = self.rec.probation.take() {
            let _ = std::fs::remove_dir_all(p.kept);
        }
        self.save();
        self.view.phase = "error";
        self.view.error = reason;
        self.view.queued = false;
        self.publish();
    }

    fn install(&mut self) {
        let Some(offer) = self.offer.clone() else { return };
        if elevated() {
            self.fail("elevated · start neuron normally".into());
            return;
        }
        self.view.phase = "installing";
        self.publish();
        self.owns_guard = true;
        let handoff = match self.engine.prepare_install(&offer, &exe_dir(), std::process::id(), true, &mut self.rec) {
            Ok(h) => h,
            Err(e) => return self.fail(e.short()),
        };
        self.save();
        crate::flight::trace("update", "install handed off", 0);
        let mut child = match install::spawn(&handoff) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("neuron: installer did not start: {e}");
                return self.fail("installer did not start".into());
            }
        };
        if handoff.self_quit {
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
            return;
        }
        // Setup signals this process to exit when it is ready to replace files; reaching the end
        // of the wait alive means it stopped before that.
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        if let Some(p) = &self.rec.pending {
            let _ = std::fs::write(&p.outcome, code.to_string());
        }
        if code == 0 {
            // Setup finished without needing this process gone: the new files are in place and
            // a restart picks them up
            self.view.phase = "installed";
            self.view.error.clear();
            self.publish();
        } else {
            self.fail(install::setup_exit_reason(code));
        }
    }
}

/// An install pressed while a download runs is honoured when the download finishes.
fn take_install_request() -> bool {
    let c = ctl();
    let mut g = c.req.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut g.install)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_follow_the_release_in_reading_order() {
        let r = notes::parse_section("## 0.1.8 · 2026-11-20\nthe updater.\n### new\n- **luma-t.** learns.\n### thanks\n@a").unwrap();
        let kinds: Vec<_> = rows(&r).into_iter().map(|(k, t, _)| (k, t)).collect();
        assert_eq!(kinds, vec![
            ("head".to_string(), "the updater".to_string()),
            ("group".to_string(), "new".to_string()),
            ("item".to_string(), "luma-t.".to_string()),
            ("thanks".to_string(), String::new()),
        ]);
        let r = notes::parse_section("## 0.1.8 · 2026-11-20\nthe picker grew a brain: Luma-T. and fixes.").unwrap();
        assert_eq!(&rows(&r)[0], &("head".to_string(), "the picker grew a brain".to_string(), "Luma-T. and fixes.".to_string()));
    }

    #[test]
    fn decode_bounds_and_scales_images() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1680, 10, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let px = decode(&png, "a", "a.png").unwrap();
        assert_eq!((px.w, px.h), (840, 5));
        assert!(decode(b"not an image", "b", "b.png").is_none());
    }

    #[test]
    fn named_leads_wear_their_colours() {
        let (ink, name, colours) = name_ink("Luma-T, in the colour picker.").unwrap();
        assert_eq!((ink, name, colours.len()), ("luma", "Luma-T", 0));
        let (ink, name, colours) = name_ink("Tidepool.").unwrap();
        assert_eq!((ink, name, colours.len()), ("palette", "Tidepool", 8));
        let lit = |c: &slint::Color| 0.2126 * f32::from(c.red()) + 0.7152 * f32::from(c.green()) + 0.0722 * f32::from(c.blue());
        assert!(colours.iter().all(|c| lit(c) >= 0.5 * 255.0), "a palette is lifted to read on the drawer");
        assert!(name_ink("static colours.").is_none(), "a lowercase word is not a name");
        assert!(name_ink("Tidepools.").is_none() && name_ink("Luma-Tea.").is_none(), "a name ends at its last letter");
    }

    #[test]
    fn the_bundled_changelog_has_notes_for_some_release() {
        assert!(!notes::parse(CHANGELOG).is_empty());
    }
}
