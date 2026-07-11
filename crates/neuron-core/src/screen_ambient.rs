//! A shared, fast, live SCREEN-colour provider — the single source of truth the `ambient`
//! (ambilight) lighting effect and its on-screen preview both read.
//!
//! ## Why a provider, not a per-frame capture
//! This mirrors [`crate::sys_stats`] and [`crate::audio_level`]: a lighting effect that reacts to a
//! LIVE signal must not sample that signal on the render thread. A legacy keyboard streams at ~6fps,
//! so capturing the desktop only when a frame renders would alias badly and stutter the board. So ONE
//! background thread captures the screen at a steady ~18Hz, DOWNSCALES it to a tiny zone grid (the
//! grabber averages the pixels for us), and publishes the small `Vec<Rgb>` behind a `Mutex`. Every
//! consumer — the device stream, the big render preview, the effect-tile thumbnail — calls [`ensure`]
//! then reads [`grid`], so they all see the SAME zones and stay in lock-step, and the board reacts
//! smoothly even at 6fps because the capture cadence is independent of the frame rate.
//!
//! ## Lifecycle
//! [`ensure`] starts the thread; it's idempotent — a call while it's already running is a no-op, so
//! it's cheap to call every frame. The thread auto-stops (and blacks the grid) if nobody has called
//! [`grid`] in a few seconds, so it never captures longer than the page that wants it; the next
//! [`ensure`] restarts it.
//!
//! ## Platform seam — where a macOS/Linux port plugs in
//! Screen capture is the heaviest, most divergent platform dependency of the three live providers
//! (GDI vs Core Graphics vs X11/PipeWire share almost no shape), so its seam is a TRAIT, not a single
//! function: [`ScreenGrabber::grab`] = "capture the desktop, downscaled to a `cols × rows` RGB zone
//! grid (row-major, top-left → bottom-right), or `None` on failure". Everything else — the thread,
//! the grid storage, the idle-stop, the grid→cell mapping, the colour boost — is platform-NEUTRAL and
//! drives ANY grabber. Adding a platform is implementing ONE `impl ScreenGrabber` and selecting it in
//! [`new_grabber`]: the Windows one ([`imp::WindowsGdiGrabber`], a `StretchBlt` HALFTONE downscale) is
//! used on Windows; off-Windows the inert [`stub::NullGrabber`] grabs `None` so the board idles
//! honestly dark. (`mod stub` is ALWAYS compiled — never cfg-gated — so its surface is type-checked on
//! every build; the porting TODOs sit on its `grab`.)

use crate::lighting::Rgb;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// The captured zone grid is this wide — one column per horizontal screen band. Matches the typical
/// keyboard column count so the nearest-zone mapping is ~1:1, but the renderer maps ANY board size.
pub const AMBIENT_COLS: usize = 22;
/// The captured zone grid is this tall — one row per vertical screen band.
pub const AMBIENT_ROWS: usize = 6;

// ── the grid→cell mapping + colour post-process: platform-neutral MATH, kept OUT of the grabber so
// it's unit-testable on every target (the capture itself isn't, but this is). Used by
// `effects::render_ambient` on every target, so neither is dead code.

/// Map a normalized position `(nx, ny)` in `0..=1` to the NEAREST zone colour of a `cols × rows`
/// grid (row-major, `(0,0)` = top-left). `(0,0)` → top-left zone, `(1,0)` → top-right, `(0,1)` →
/// bottom-left — a true ambilight mapping. Out-of-range positions clamp into the grid; an empty or
/// zero-dimension grid reads black, never an out-of-bounds panic.
pub(crate) fn sample_grid(grid: &[Rgb], cols: usize, rows: usize, nx: f32, ny: f32) -> Rgb {
    if grid.is_empty() || cols == 0 || rows == 0 {
        return Rgb::BLACK;
    }
    let nx = nx.clamp(0.0, 1.0);
    let ny = ny.clamp(0.0, 1.0);
    // nx=1.0 lands exactly on `cols` → clamp back to the last column; same for rows.
    let gx = ((nx * cols as f32).floor() as usize).min(cols - 1);
    let gy = ((ny * rows as f32).floor() as usize).min(rows - 1);
    grid.get(gy * cols + gx).copied().unwrap_or(Rgb::BLACK)
}

/// Boost a colour's saturation by pushing each channel away from its luma by `1 + amount` — so the
/// board POPS rather than washing out at typical screen colours. `amount <= 0` is the identity (the
/// faithful screen colour); a grey (no chroma) is unchanged; the result always clamps to `0..=255`.
/// Pure (no HSV round-trip needed — the luma push is its own inverse-free operation).
pub(crate) fn boost_saturation(c: Rgb, amount: f32) -> Rgb {
    let amount = amount.max(0.0);
    if amount == 0.0 {
        return c;
    }
    // Rec.601 luma — the grey level the chroma is measured against.
    let luma = 0.299 * c.r as f32 + 0.587 * c.g as f32 + 0.114 * c.b as f32;
    let push = |x: u8| {
        let v = luma + (x as f32 - luma) * (1.0 + amount);
        v.round().clamp(0.0, 255.0) as u8
    };
    Rgb::new(push(c.r), push(c.g), push(c.b))
}

/// THE PORTING SEAM: capture the desktop downscaled to a `cols × rows` RGB zone grid. One `impl` per
/// platform; the neutral capturer thread holds a boxed grabber and calls [`grab`](Self::grab) each
/// tick. `&mut self` lets a backend cache its OS resources (DCs, contexts) across ticks if it wants.
trait ScreenGrabber: Send {
    /// Capture + downscale the whole (virtual / all-monitor) desktop into a `cols × rows` zone grid,
    /// row-major top-left → bottom-right. `None` on any capture failure (the loop keeps the last grid;
    /// it never panics). The grabber averages each zone (it's a downscale, not a point-sample).
    fn grab(&mut self, cols: usize, rows: usize) -> Option<Vec<Rgb>>;
}

/// Build the platform grabber for this target — the one cfg-selected line. A real macOS/Linux port
/// adds its `impl ScreenGrabber` and a matching arm here.
#[cfg(windows)]
fn new_grabber() -> Box<dyn ScreenGrabber> {
    Box::new(imp::WindowsGdiGrabber::new())
}
#[cfg(not(windows))]
fn new_grabber() -> Box<dyn ScreenGrabber> {
    Box::new(stub::NullGrabber)
}

/// Millis since the process epoch of the last [`grid`] read — drives the idle auto-stop.
static LAST_ACCESS_MS: AtomicU64 = AtomicU64::new(0);

/// A process-lifetime monotonic origin so the thread and `grid()` agree on "now" in millis.
fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}
fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// The published zone grid (`AMBIENT_COLS * AMBIENT_ROWS`, row-major) — read under the lock and
/// cloned out in [`grid`]. Initialised black so a reader before the first capture sees darkness.
fn cells() -> &'static Mutex<Vec<Rgb>> {
    static G: OnceLock<Mutex<Vec<Rgb>>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(vec![Rgb::BLACK; AMBIENT_COLS * AMBIENT_ROWS]))
}
/// The capturer's control block — just whether a thread is alive.
fn running() -> &'static Mutex<bool> {
    static R: OnceLock<Mutex<bool>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(false))
}

const SAMPLE_INTERVAL: Duration = Duration::from_millis(55); // ~18Hz desktop capture
const IDLE_STOP_MS: u64 = 3000; // stop the thread if no `grid()` read in ~3s

/// Start the capturer if it isn't already running. Idempotent — a call while it's alive is a no-op
/// — so it's cheap to call every frame. Refreshes the idle timer so a brand-new thread doesn't
/// immediately auto-stop before the first [`grid`] read lands.
pub fn ensure() {
    let mut run = running().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    if *run {
        return;
    }
    *run = true;
    drop(run);
    // The latch is cleared by the release — which runs on completion, panic, OR a spawn refusal —
    // so a failed spawn can never leave the capturer latched "on" and block every later `ensure`.
    crate::worker::spawn_guarded(
        "neuron-screen-ambient",
        || *running().lock().unwrap_or_else(std::sync::PoisonError::into_inner) = false,
        run_loop,
    );
}

/// The latest captured zone grid as `(cols, rows, cells)`. Cheap (one lock + clone of ~132
/// colours). Reading it keeps the capturer alive (resets the idle timer), so a consumer that stops
/// reading lets the thread auto-stop.
pub fn grid() -> (usize, usize, Vec<Rgb>) {
    LAST_ACCESS_MS.store(now_ms(), Ordering::Relaxed);
    let g = cells().lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
    (AMBIENT_COLS, AMBIENT_ROWS, g)
}

/// The capturer loop — PLATFORM-NEUTRAL. Build the platform grabber once, then each ~55ms grab the
/// downscaled desktop (the platform seam) into the zone grid and publish it. Exits (blacking the
/// grid) once no [`grid`] read has landed in ~3s. The ONLY platform-specific call here is
/// `grabber.grab(...)`.
fn run_loop() {
    let mut grabber = new_grabber();
    // PROF (env `NEURON_PROF`): time the GDI whole-desktop StretchBlt — it runs on THIS background
    // thread, so it never shows in the render tick, yet it's the heaviest thing the ambient surface
    // does. Summed across a second then printed to stderr as a per-grab average (one line/sec).
    let prof = std::env::var_os("NEURON_PROF").is_some();
    let mut prof_n: u32 = 0;
    let mut prof_us: f64 = 0.0;
    let mut prof_last = Instant::now();
    loop {
        // ── control: idle auto-stop ──
        {
            let run = running().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let idle = now_ms().saturating_sub(LAST_ACCESS_MS.load(Ordering::Relaxed));
            if idle > IDLE_STOP_MS {
                // `running` is cleared by the spawn's release, not here — see `ensure`.
                drop(run);
                if let Ok(mut g) = cells().lock() {
                    for c in g.iter_mut() {
                        *c = Rgb::BLACK; // don't leave a stale frame for the next start
                    }
                }
                return;
            }
        }
        // ── capture (platform seam): a failed grab keeps the previous grid (no flicker, no panic) ──
        let cap_start = Instant::now();
        let captured = grabber.grab(AMBIENT_COLS, AMBIENT_ROWS);
        if prof {
            prof_n += 1;
            prof_us += cap_start.elapsed().as_nanos() as f64 / 1000.0;
            if prof_last.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "PROF: screen_ambient grab avg={:.3}ms grabs/s={} (always-on whole-desktop StretchBlt)",
                    prof_us / prof_n.max(1) as f64 / 1000.0,
                    prof_n
                );
                prof_n = 0;
                prof_us = 0.0;
                prof_last = Instant::now();
            }
        }
        if let Some(captured) = captured {
            if let Ok(mut g) = cells().lock() {
                if captured.len() == g.len() {
                    g.copy_from_slice(&captured);
                }
            }
        }
        thread::sleep(SAMPLE_INTERVAL);
    }
}

#[cfg(windows)]
mod imp {
    use super::{Rgb, ScreenGrabber};

    /// The Windows desktop grabber — GDI. Each grab does `GetDC(NULL)` for the whole VIRTUAL screen
    /// (all monitors), a memory DC + a small compatible bitmap, `SetStretchBltMode(HALFTONE)`
    /// (high-quality averaging downscale), `StretchBlt` the entire virtual screen into the tiny
    /// bitmap, then `GetDIBits` to read the averaged zone pixels back as 32bpp top-down BGRX. Every
    /// GDI handle is released on every path before the grab returns — no handle leaks. Stateless
    /// today (it recreates the DCs each tick); the `&mut self` seam leaves room to cache them later.
    pub struct WindowsGdiGrabber;

    impl WindowsGdiGrabber {
        pub fn new() -> Self {
            WindowsGdiGrabber
        }
    }

    impl ScreenGrabber for WindowsGdiGrabber {
        fn grab(&mut self, cols: usize, rows: usize) -> Option<Vec<Rgb>> {
            capture_grid(cols, rows)
        }
    }

    /// Capture the whole virtual screen and downscale it to a `cols × rows` zone grid via GDI
    /// (HALFTONE `StretchBlt` averages each zone). Returns the zones row-major top-left→bottom-right,
    /// or `None` if any GDI call fails. EVERY handle created here is released before return — no leaks.
    fn capture_grid(cols: usize, rows: usize) -> Option<Vec<Rgb>> {
        use std::ptr::null_mut;
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
            ReleaseDC, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt, BITMAPINFO,
            BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HALFTONE, SRCCOPY,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };
        if cols == 0 || rows == 0 {
            return None;
        }
        // SAFETY: each GDI handle (screen DC, memory DC, bitmap) is released on every return path
        // below before this function returns; the readback buffer is sized cols*rows*4 to match the
        // 32bpp top-down DIB GetDIBits writes, and the bitmap is deselected before GetDIBits as the
        // API requires.
        unsafe {
            let (vx, vy, vw, vh) = (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            );
            if vw <= 0 || vh <= 0 {
                return None;
            }
            let screen = GetDC(null_mut());
            if screen.is_null() {
                return None;
            }
            let mem = CreateCompatibleDC(screen);
            if mem.is_null() {
                ReleaseDC(null_mut(), screen);
                return None;
            }
            let bmp = CreateCompatibleBitmap(screen, cols as i32, rows as i32);
            if bmp.is_null() {
                DeleteDC(mem);
                ReleaseDC(null_mut(), screen);
                return None;
            }
            let old = SelectObject(mem, bmp);
            // HALFTONE = high-quality averaging downscale (each tiny zone becomes the MEAN of its
            // screen region); MSDN says set the brush origin right after switching to HALFTONE.
            SetStretchBltMode(mem, HALFTONE);
            let mut pt: POINT = std::mem::zeroed();
            SetBrushOrgEx(mem, 0, 0, &mut pt);
            let blit = StretchBlt(
                mem, 0, 0, cols as i32, rows as i32, screen, vx, vy, vw, vh, SRCCOPY,
            );
            // GetDIBits requires the bitmap NOT be selected into a DC → restore the old object first.
            SelectObject(mem, old);
            let mut out = None;
            if blit != 0 {
                // describe the readback format: 32bpp BI_RGB, top-down (negative height) so row 0 is
                // the TOP of the screen.
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: cols as i32,
                    biHeight: -(rows as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    biSizeImage: 0,
                    biXPelsPerMeter: 0,
                    biYPelsPerMeter: 0,
                    biClrUsed: 0,
                    biClrImportant: 0,
                };
                let mut buf = vec![0u8; cols * rows * 4];
                let got = GetDIBits(
                    mem,
                    bmp,
                    0,
                    rows as u32,
                    buf.as_mut_ptr() as *mut core::ffi::c_void,
                    &mut bi,
                    DIB_RGB_COLORS,
                );
                if got != 0 {
                    // 32bpp DIB scanlines are BGRX (little-endian) — channel 2 is red, 0 is blue.
                    let zones = buf
                        .chunks_exact(4)
                        .map(|px| Rgb::new(px[2], px[1], px[0]))
                        .collect();
                    out = Some(zones);
                }
            }
            DeleteObject(bmp);
            DeleteDC(mem);
            ReleaseDC(null_mut(), screen);
            out
        }
    }
}

/// The inert off-Windows desktop grabber — no capture backend here, so every grab reads dark, and the
/// neutral capturer keeps the grid black (the `ambient` board idles honestly dark rather than faking
/// colour). ALWAYS compiled (not cfg-gated) so its surface is type-checked on every build, mirroring
/// `audio.rs`'s stub discipline; `#![allow(dead_code)]` because on Windows the live seam is
/// `imp::WindowsGdiGrabber` and this stands unused.
mod stub {
    #![allow(dead_code)]
    use super::{Rgb, ScreenGrabber};

    /// The off-platform grabber stand-in. A real port REPLACES the `grab` body below.
    pub struct NullGrabber;

    impl ScreenGrabber for NullGrabber {
        /// THE SEAM (port here). Returns `None` → the board reads dark.
        // TODO(macos): implement via Core Graphics — `CGDisplayCreateImage` (or ScreenCaptureKit on
        //   modern macOS) then draw into a `cols × rows` CGContext to get the averaged zone grid.
        // TODO(linux): implement via X11 `XGetImage`/`XShmGetImage` of the root window + a downscale,
        //   or a PipeWire screencast portal stream on Wayland; map the pixels to row-major RGB.
        fn grab(&mut self, _cols: usize, _rows: usize) -> Option<Vec<Rgb>> {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_grid_maps_corners_and_centre() {
        // a 3×3 grid where each zone carries its own index in the red channel
        let g: Vec<Rgb> = (0..9).map(|i| Rgb::new(i as u8, 0, 0)).collect();
        assert_eq!(sample_grid(&g, 3, 3, 0.0, 0.0), Rgb::new(0, 0, 0), "top-left zone");
        assert_eq!(sample_grid(&g, 3, 3, 1.0, 0.0), Rgb::new(2, 0, 0), "top-right zone");
        assert_eq!(sample_grid(&g, 3, 3, 0.0, 1.0), Rgb::new(6, 0, 0), "bottom-left zone");
        assert_eq!(sample_grid(&g, 3, 3, 1.0, 1.0), Rgb::new(8, 0, 0), "bottom-right zone");
        assert_eq!(sample_grid(&g, 3, 3, 0.5, 0.5), Rgb::new(4, 0, 0), "centre zone");
    }

    #[test]
    fn sample_grid_clamps_and_handles_empty() {
        let g = vec![Rgb::new(9, 9, 9); 4];
        // out-of-range positions clamp into the grid — never an out-of-bounds index/panic
        assert_eq!(sample_grid(&g, 2, 2, -1.0, 2.0), Rgb::new(9, 9, 9));
        assert_eq!(sample_grid(&g, 2, 2, 5.0, -3.0), Rgb::new(9, 9, 9));
        // an empty or zero-dimension grid reads black, not a panic
        assert_eq!(sample_grid(&[], 0, 0, 0.5, 0.5), Rgb::BLACK);
        assert_eq!(sample_grid(&g, 0, 2, 0.5, 0.5), Rgb::BLACK);
    }

    #[test]
    fn boost_saturation_identity_grey_and_clamp() {
        let c = Rgb::new(180, 90, 60);
        assert_eq!(boost_saturation(c, 0.0), c, "no boost is the identity");
        assert_eq!(boost_saturation(c, -1.0), c, "a negative amount never desaturates");
        // a grey has no chroma → unchanged by any boost
        assert_eq!(boost_saturation(Rgb::new(128, 128, 128), 2.0), Rgb::new(128, 128, 128));
        // a strong boost widens the channel spread and clamps to 0..=255 (red pegs, blue floors)
        assert_eq!(
            boost_saturation(c, 5.0),
            Rgb::new(255, 0, 0),
            "a strong boost saturates fully and clamps, never overshoots"
        );
        let spread = |x: Rgb| x.r.max(x.g).max(x.b) - x.r.min(x.g).min(x.b);
        assert!(spread(boost_saturation(c, 1.0)) > spread(c), "a boost increases saturation");
    }
}
