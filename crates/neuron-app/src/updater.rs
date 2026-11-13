// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The in-app updater: one worker that sleeps on a condvar between checks (shortly after launch,
//! then every ~12 h, or when asked), downloads and verifies in the background, and hands installs
//! to the copy's own lifecycle (`neuron::update::install`). The UI thread only flips request
//! flags and receives finished views; it never touches the network or the disk here.

use crate::ui::{AppWindow, NoteMedia, NoteRow, State};
use neuron::update::{self, client, install, notes, policy, state::State as Record, version::Version, Engine, Offer};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The changelog this build shipped with; SYSTEM shows the running version's section from it.
const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");
const RELEASES: &str = "https://github.com/worflor/neuron/releases";
const REPORT: &str = "https://github.com/worflor/neuron/issues/new";
/// The longest the worker sleeps at once, so a wall-clock deadline passed during system sleep
/// is noticed soon after resume.
const MAX_NAP: Duration = Duration::from_secs(15 * 60);
/// Notes images are drawn at most ~420 px wide; decode to twice that for high-DPI glass.
const MEDIA_MAX_WIDTH: u32 = 840;

#[derive(Default)]
struct Requests {
    check: bool,
    download: bool,
    install: bool,
    dismiss: bool,
    poke: bool,
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
struct Pixels {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    alt: String,
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
fn rows(r: &notes::Release) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    if !r.headline.is_empty() {
        out.push(("head".into(), String::new(), r.headline.clone()));
    }
    for s in &r.sections {
        if !s.title.is_empty() {
            out.push(("group".into(), s.title.clone(), String::new()));
        }
        for i in &s.items {
            out.push(("item".into(), i.title.clone(), i.body.clone()));
        }
    }
    if !r.thanks.is_empty() {
        out.push(("thanks".into(), String::new(), format!("thanks: {}", r.thanks.join(" "))));
    }
    out
}

fn note_rows(rows: &[(String, String, String)]) -> slint::ModelRc<NoteRow> {
    slint::ModelRc::new(slint::VecModel::from(
        rows.iter().map(|(k, t, b)| NoteRow { kind: k.as_str().into(), title: t.as_str().into(), body: b.as_str().into() }).collect::<Vec<_>>(),
    ))
}

fn note_media(px: &[Pixels]) -> slint::ModelRc<NoteMedia> {
    slint::ModelRc::new(slint::VecModel::from(
        px.iter()
            .map(|p| {
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&p.rgba, p.w, p.h);
                NoteMedia { img: slint::Image::from_rgba8(buf), alt: p.alt.as_str().into(), ratio: p.h as f32 / p.w.max(1) as f32 }
            })
            .collect::<Vec<_>>(),
    ))
}

/// Decode one image, bounded in bytes and pixels. Off the UI thread.
fn decode(bytes: &[u8], alt: &str) -> Option<Pixels> {
    if bytes.len() as u64 > update::manifest::MAX_MEDIA_BYTES {
        return None;
    }
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut img = reader.decode().ok()?;
    if img.width() > MEDIA_MAX_WIDTH {
        let h = (u64::from(img.height()) * u64::from(MEDIA_MAX_WIDTH) / u64::from(img.width().max(1))).max(1) as u32;
        img = img.resize_exact(MEDIA_MAX_WIDTH, h, image::imageops::FilterType::Triangle);
    }
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    (w > 0 && h > 0).then(|| Pixels { rgba: rgba.into_raw(), w, h, alt: alt.to_string() })
}

/// Media for the running version's own notes, from the `docs/` the package installs beside it.
fn local_media(r: &notes::Release, exe_dir: &Path) -> Vec<Pixels> {
    r.media
        .iter()
        .take(update::manifest::MAX_MEDIA)
        .filter_map(|m| {
            let path = exe_dir.join(&m.path);
            let len = std::fs::metadata(&path).ok()?.len();
            if len > update::manifest::MAX_MEDIA_BYTES {
                return None;
            }
            decode(&std::fs::read(path).ok()?, &m.alt)
        })
        .collect()
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
        st.set_update_media(note_media(px));
    }
    if v.phase.is_empty() && st.get_update_notes_open() {
        st.set_update_notes_open(false);
    }
}

fn post(v: &View) {
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

/// A clean quit does not count against a version on probation.
pub fn clean_exit() {
    update::guard::clean_exit(&neuron::runroot::run_root(), &Version::current());
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
}

impl Worker {
    fn new(engine: Engine) -> Self {
        let rec = Record::load(&engine.run_root);
        Self { engine, rec, offer: None, view: View { enabled: true, ..View::default() }, launched: now_unix(), cancel: AtomicBool::new(false) }
    }

    fn save(&self) {
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
            self.view.version = rb.from.clone();
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
            if req.check || timed || ((req.download || req.install) && self.offer.is_none()) {
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
            if g.check || g.download || g.install || g.dismiss || g.poke {
                return std::mem::take(&mut *g);
            }
            let now = now_unix();
            if crate::prefs::update_auto() && now >= due {
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
                self.view.status = format!("latest release refused: {}", e.short());
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
                    decode(&bytes, &alt)
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
            match &result {
                Err(e) if e.transient() => continue,
                _ => break,
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
            self.view.phase = "error";
            self.view.error = "installed · restart neuron".into();
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
            ("head".to_string(), String::new()),
            ("group".to_string(), "new".to_string()),
            ("item".to_string(), "luma-t.".to_string()),
            ("thanks".to_string(), String::new()),
        ]);
    }

    #[test]
    fn decode_bounds_and_scales_images() {
        let mut png = Vec::new();
        image::RgbaImage::from_pixel(1680, 10, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let px = decode(&png, "a").unwrap();
        assert_eq!((px.w, px.h), (840, 5));
        assert!(decode(b"not an image", "b").is_none());
    }

    #[test]
    fn the_bundled_changelog_has_notes_for_some_release() {
        assert!(!notes::parse(CHANGELOG).is_empty());
    }
}
