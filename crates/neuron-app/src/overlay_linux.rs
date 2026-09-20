// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! GTK/Cairo host for the resident Linux overlay. Rendering stays on the GUI thread, while the
//! live dispatcher only sends presentation data. GDK marks the surface input-transparent.

use super::{DigestView, GlyphHint, NotifySlot, WeaveMode};
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const SIZE: i32 = 840;
const CENTER: f64 = SIZE as f64 / 2.0;

enum Cmd {
    Begin(WeaveMode),
    Push(Vec<(f32, f32)>),
    Hint(Option<GlyphHint>),
    Recognized(bool),
    Stack(Vec<NotifySlot>, u8, (f32, f32), DigestView),
    End,
    Quit,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
static QUEUE: OnceLock<Mutex<VecDeque<(u64, Cmd)>>> = OnceLock::new();

fn send(id: u64, cmd: Cmd) {
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .push_back((id, cmd));
    let _ = slint::invoke_from_event_loop(pump);
}

pub struct SpellOverlay { id: u64 }

impl SpellOverlay {
    pub fn spawn() -> Self { Self { id: NEXT_ID.fetch_add(1, Ordering::Relaxed) } }
    pub fn begin(&self, mode: WeaveMode) { send(self.id, Cmd::Begin(mode)); }
    pub fn push(&self, points: Vec<(f32, f32)>) { send(self.id, Cmd::Push(points)); }
    pub fn hint(&self, hint: Option<GlyphHint>) { send(self.id, Cmd::Hint(hint)); }
    pub fn recognized(&self, hit: bool) { send(self.id, Cmd::Recognized(hit)); }
    pub fn stack(&self, slots: Vec<NotifySlot>, mode: u8, place: (f32, f32), digest: DigestView, _tail: u32) {
        send(self.id, Cmd::Stack(slots, mode, place, digest));
    }
    pub fn end(&self) { send(self.id, Cmd::End); }
}

impl Drop for SpellOverlay {
    fn drop(&mut self) { send(self.id, Cmd::Quit); }
}

#[derive(Default)]
struct Frame {
    mode: Option<WeaveMode>,
    points: Vec<(f32, f32)>,
    hint: Option<GlyphHint>,
    recognized: Option<bool>,
    slots: Vec<NotifySlot>,
    digest: Option<DigestView>,
    notification_mode: u8,
}

struct Surface {
    window: gtk::Window,
    area: gtk::DrawingArea,
    frame: Rc<RefCell<Frame>>,
}

impl Surface {
    fn new() -> Option<Self> {
        if !gtk::is_initialized() && gtk::init().is_err() { return None; }
        let window = gtk::Window::new(gtk::WindowType::Popup);
        window.set_decorated(false);
        window.set_resizable(false);
        window.set_app_paintable(true);
        window.set_keep_above(true);
        window.set_accept_focus(false);
        window.set_focus_on_map(false);
        window.set_skip_taskbar_hint(true);
        window.set_skip_pager_hint(true);
        window.set_type_hint(gtk::gdk::WindowTypeHint::Notification);
        window.set_default_size(SIZE, SIZE);
        if let Some(screen) = gtk::prelude::GtkWindowExt::screen(&window) {
            if let Some(visual) = screen.rgba_visual() { window.set_visual(Some(&visual)); }
        }
        window.connect_realize(|window| {
            if let Some(surface) = window.window() { surface.set_pass_through(true); }
        });
        let area = gtk::DrawingArea::new();
        area.set_size_request(SIZE, SIZE);
        window.add(&area);
        let frame = Rc::new(RefCell::new(Frame::default()));
        let draw_frame = frame.clone();
        area.connect_draw(move |_, ctx| {
            draw(ctx, &draw_frame.borrow());
            gtk::glib::Propagation::Proceed
        });
        Some(Self { window, area, frame })
    }

    fn place(&self, point: Option<(f32, f32)>) {
        let Some(display) = gtk::gdk::Display::default() else { return };
        let Some(monitor) = display.primary_monitor().or_else(|| display.monitor(0)) else { return };
        let bounds = monitor.geometry();
        let (x, y) = if let Some((nx, ny)) = point {
            (bounds.x() + (nx.clamp(0.0, 1.0) * bounds.width() as f32) as i32,
             bounds.y() + (ny.clamp(0.0, 1.0) * bounds.height() as f32) as i32)
        } else if let Some(seat) = display.default_seat() {
            seat.pointer().map_or((bounds.x() + bounds.width() / 2, bounds.y() + bounds.height() / 2), |pointer| {
                let (_, x, y) = pointer.position();
                (x, y)
            })
        } else { (bounds.x() + bounds.width() / 2, bounds.y() + bounds.height() / 2) };
        self.window.move_(x - SIZE / 2, y - SIZE / 2);
    }

    fn apply(&self, cmd: Cmd) {
        let mut frame = self.frame.borrow_mut();
        match cmd {
            Cmd::Begin(mode) => {
                frame.mode = Some(mode);
                frame.points.clear();
                frame.recognized = None;
                self.place(None);
                self.window.show_all();
            }
            Cmd::Push(points) => frame.points = points,
            Cmd::Hint(hint) => frame.hint = hint,
            Cmd::Recognized(hit) => frame.recognized = Some(hit),
            Cmd::Stack(slots, mode, place, digest) => {
                frame.mode = Some(WeaveMode::NotifyStack);
                frame.slots = slots;
                frame.notification_mode = mode;
                frame.digest = Some(digest);
                self.place(Some(place));
                self.window.show_all();
            }
            Cmd::End | Cmd::Quit => self.window.hide(),
        }
        drop(frame);
        self.area.queue_draw();
    }
}

thread_local! {
    static SURFACES: RefCell<HashMap<u64, Surface>> = RefCell::new(HashMap::new());
    static GTK_TICK: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

fn ensure_gtk_tick() {
    GTK_TICK.with(|slot| {
        if slot.borrow().is_some() { return; }
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(16), || {
            while gtk::events_pending() { gtk::main_iteration_do(false); }
        });
        *slot.borrow_mut() = Some(timer);
    });
}

fn pump() {
    let commands: Vec<(u64, Cmd)> = QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
        .lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        .drain(..).collect();
    SURFACES.with(|all| {
        let mut all = all.borrow_mut();
        for (id, cmd) in commands {
            if matches!(cmd, Cmd::Quit) {
                if let Some(surface) = all.remove(&id) { surface.window.hide(); }
                continue;
            }
            if let Some(surface) = all.get(&id) {
                surface.apply(cmd);
            } else if let Some(surface) = Surface::new() {
                ensure_gtk_tick();
                surface.apply(cmd);
                all.insert(id, surface);
            }
        }
    });
}

fn text(ctx: &gtk::cairo::Context, x: f64, y: f64, size: f64, value: &str, alpha: f64) {
    ctx.set_source_rgba(0.76, 1.0, 0.92, alpha);
    ctx.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
    ctx.set_font_size(size);
    ctx.move_to(x, y);
    let _ = ctx.show_text(value);
}

fn ring(ctx: &gtk::cairo::Context, radius: f64, alpha: f64) {
    ctx.set_source_rgba(0.14, 0.94, 0.72, alpha);
    ctx.set_line_width(2.0);
    ctx.arc(CENTER, CENTER, radius, 0.0, std::f64::consts::TAU);
    let _ = ctx.stroke();
}

fn draw(ctx: &gtk::cairo::Context, frame: &Frame) {
    ctx.set_operator(gtk::cairo::Operator::Clear);
    let _ = ctx.paint();
    ctx.set_operator(gtk::cairo::Operator::Over);
    let Some(mode) = frame.mode.as_ref() else { return };
    if matches!(mode, WeaveMode::NotifyStack) {
        if frame.notification_mode == 2 {
            if let Some(digest) = frame.digest.as_ref().filter(|digest| digest.count > 1) {
                card(ctx, 100.0, 315.0, &format!("{} updates", digest.count), &digest.value, f64::from(digest.alpha));
                return;
            }
        }
        for slot in frame.slots.iter().take(5) {
            card(ctx, 100.0, 300.0 + f64::from(slot.y), &slot.title, &slot.value, f64::from(slot.alpha));
        }
        return;
    }
    ring(ctx, 90.0, 0.65);
    ring(ctx, 104.0, 0.20);
    match mode {
        WeaveMode::Radial { sectors, widgets, .. } => {
            let count = f64::from((*sectors).max(1));
            for (index, widget) in widgets.iter().enumerate() {
                let angle = index as f64 * std::f64::consts::TAU / count - std::f64::consts::FRAC_PI_2;
                let x = CENTER + 137.0 * angle.cos();
                let y = CENTER + 137.0 * angle.sin();
                text(ctx, x - 32.0, y, 13.0, &widget.title, 0.9);
                if let Some(value) = &widget.value { text(ctx, x - 32.0, y + 19.0, 15.0, value, 1.0); }
            }
        }
        WeaveMode::Ask { label, options, detail } => {
            text(ctx, CENTER - 155.0, CENTER - 150.0, 20.0, label, 1.0);
            text(ctx, CENTER - 155.0, CENTER - 124.0, 14.0, detail, 0.8);
            for (index, option) in options.iter().enumerate() {
                let angle = index as f64 * std::f64::consts::TAU / options.len().max(1) as f64;
                text(ctx, CENTER + angle.cos() * 140.0 - 25.0, CENTER + angle.sin() * 140.0, 16.0, option, 1.0);
            }
        }
        WeaveMode::Dial { value, device, fill, .. } => {
            text(ctx, CENTER - 40.0, CENTER + 4.0, 30.0, value, 1.0);
            text(ctx, CENTER - 45.0, CENTER + 28.0, 14.0, device, 0.8);
            ctx.set_source_rgba(0.3, 1.0, 0.8, 0.9);
            ctx.set_line_width(8.0);
            ctx.arc(CENTER, CENTER, 94.0, 2.35, 2.35 + f64::from(fill.clamp(0.0, 1.0)) * 4.7);
            let _ = ctx.stroke();
        }
        WeaveMode::Control { net, out, bt, .. } => {
            text(ctx, CENTER - 120.0, CENTER - 135.0, 16.0, net, 1.0);
            text(ctx, CENTER - 170.0, CENTER + 5.0, 15.0, out, 1.0);
            text(ctx, CENTER + 115.0, CENTER + 5.0, 15.0, bt, 1.0);
        }
        WeaveMode::Glyph { .. } => {
            if let Some(hint) = &frame.hint {
                text(ctx, CENTER - 90.0, CENTER - 135.0, 16.0, &hint.view.title, f64::from(hint.confidence));
            }
        }
        WeaveMode::Map { .. } => text(ctx, CENTER - 55.0, CENTER - 130.0, 18.0, "TELEPORT", 1.0),
        WeaveMode::Twin { hint, .. } => text(ctx, CENTER - 80.0, CENTER + 145.0, 16.0, hint, 0.85),
        WeaveMode::NotifyStack => return,
    }
    if let Some(&(x, y)) = frame.points.first() {
        ctx.set_source_rgba(0.3, 1.0, 0.79, 0.94);
        ctx.set_line_width(4.0);
        ctx.move_to(CENTER + f64::from(x), CENTER + f64::from(y));
        for &(x, y) in frame.points.iter().skip(1) {
            ctx.line_to(CENTER + f64::from(x), CENTER + f64::from(y));
        }
        let _ = ctx.stroke();
    }
    if let Some(hit) = frame.recognized {
        ring(ctx, 116.0, if hit { 0.95 } else { 0.35 });
    }
}

fn card(ctx: &gtk::cairo::Context, x: f64, y: f64, title: &str, value: &str, alpha: f64) {
    ctx.set_source_rgba(0.01, 0.08, 0.08, 0.82 * alpha);
    ctx.rectangle(x, y, 640.0, 78.0);
    let _ = ctx.fill();
    ctx.set_source_rgba(0.2, 1.0, 0.75, 0.7 * alpha);
    ctx.set_line_width(2.0);
    ctx.rectangle(x, y, 640.0, 78.0);
    let _ = ctx.stroke();
    text(ctx, x + 18.0, y + 26.0, 14.0, title, alpha);
    text(ctx, x + 18.0, y + 55.0, 22.0, value, alpha);
}
