// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! CURTAIN — a panic privacy screen. **Not** a power action: it never touches DPMS, the monitor
//! power state, or the system at all. It throws ONE opaque, topmost window across the WHOLE virtual
//! desktop (every monitor, taskbar included) so the screen content is hidden INSTANTLY and RELIABLY,
//! then the first key/click brings everything back with zero latency — because nothing was ever
//! powered down there is no display-link re-train, no window reshuffle, no "did my PC just die?"
//! recovery. The machine stays 100% awake the entire time.
//!
//! ## Why it is NOT `SC_MONITORPOWER`
//! This used to be "monitors off": a real DPMS monitor power-off. That tears the display link all the
//! way down; on a `DisplayPort` + NVIDIA rig Windows reads it as a hot-unplug, collapses the desktop
//! onto the surviving panel, and the cold renegotiation on wake reads exactly like a frozen machine
//! you must restart. The opposite of "hide fast, come back fast". The curtain is a pure user-space
//! overlay — an opaque window and a poll loop — so it can't do any of that.
//!
//! ## The look is the weave material — automagically
//! The transition is a **signal cut**: an instant opaque field of procedural STATIC that settles to
//! black on the way in and bursts back on reveal. The static is not generic snow — it's rendered from
//! the user's LIVE weave material (the same shader every cast uses), so it wears their accent / fire /
//! spectrum and ANY current-or-future material just works. Core owns the *mechanism* (window, input,
//! timing); the app registers a [`set_painter`] that owns the *look*. No painter (CLI / tests) → plain
//! black, so the hide still works headless. The hide is opaque from frame one either way — content
//! never leaks while the static plays.
//!
//! ## Lifetime
//! [`raise`] is fire-and-forget: it spawns the `neuron-curtain` worker (so the dispatch tick never
//! blocks). The worker owns the window + its own message pump, cuts in, then watches input. The
//! keystroke / flick that summoned it is still in flight, so we **disarm** until every dismiss-key is
//! released (short floor so the cut isn't clipped, ceiling so a held trigger can't trap it), then the
//! first FRESH key/click bursts it back out. Mouse *movement* never dismisses (a desk bump shouldn't
//! expose you) — only a key or mouse-button, which `GetAsyncKeyState` reports together as virtual-keys.
//! Only one curtain is ever up; a second fire is a no-op.
//!
//! Arm-gated like every other live UI side-effect: [`crate::action::input_armed`] is false under
//! `cargo test` / the verify pass, so a test run can never black out the developer's screen.

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};

/// One frame the curtain asks its (optional) painter to fill. `w×h` is a SMALL chunky buffer (blitted
/// stretched — the blockiness reads as proper static), NOT screen pixels. The painter returns **BGRA**,
/// top-down, `w*h*4` bytes.
pub struct CurtainFrame {
    pub w: usize,
    pub h: usize,
    /// Seconds since the curtain rose — the material's animation clock.
    pub t: f32,
    /// Signal strength 0..1: `1.0` = a full energised material-static field, `0.0` = pure black. The
    /// curtain ramps it 1→0 to CUT to black on raise, and 0→1 to BURST back on reveal.
    pub intensity: f32,
}

type Painter = dyn Fn(&CurtainFrame) -> Vec<u8> + Send + Sync;
#[cfg(windows)]
static PAINTER: std::sync::Mutex<Option<Box<Painter>>> = std::sync::Mutex::new(None);

/// Register the curtain's frame painter — the app does this ONCE at startup; it renders the live weave
/// material as static (see `neuron-app`'s `weave::material_static_bgra`). Without a painter the curtain
/// is plain black, so the CLI / tests still get a working hide.
#[cfg(windows)]
pub fn set_painter(f: Box<Painter>) {
    *PAINTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(f);
}
#[cfg(not(windows))]
pub fn set_painter(_f: Box<Painter>) {}

/// One curtain at a time. A second fire while it's up is a no-op — the reveal is the user's next
/// key/click, which the live curtain's own poll already catches; a second window would just stack.
#[cfg(windows)]
static CURTAIN_UP: AtomicBool = AtomicBool::new(false);

/// Raise the curtain. Fire-and-forget: spawns the worker thread and returns a status line at once.
/// Arm-gated so a test/verify pass can never black the dev's screen. Pure overlay — no power path.
#[cfg(windows)]
pub fn raise() -> String {
    if !crate::action::input_armed() {
        return "curtain [disarmed]".into();
    }
    // Already showing? The next key/click reveals it; don't stack a second window.
    if CURTAIN_UP.swap(true, Ordering::SeqCst) {
        return "curtain (already up)".into();
    }
    // The latch is cleared by the release — which runs on completion, panic, OR a spawn refusal —
    // so a failed raise can never leave `CURTAIN_UP` stuck true and block every later raise.
    let spawned = crate::worker::spawn_guarded(
        "neuron-curtain",
        || CURTAIN_UP.store(false, Ordering::SeqCst),
        run,
    );
    if !spawned {
        return "curtain [spawn failed]".into();
    }
    "curtain".into()
}

#[cfg(not(windows))]
pub fn raise() -> String {
    "curtain: windows-only".into()
}

/// The worker body: build the opaque virtual-screen window, cut in (static→black), watch for the
/// dismiss, burst out (black→static), destroy. Runs entirely on the `neuron-curtain` thread (Win32
/// requires create / pump / destroy on one thread).
#[cfg(windows)]
fn run() {
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetSystemMetrics,
        LoadCursorW, PeekMessageW, RegisterClassW, SetForegroundWindow, SetWindowPos, ShowWindow,
        TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SW_SHOWNOACTIVATE, WNDCLASSW,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };

    // The cut-in and burst-out are quick + snappy — the static does the talking, not a slow fade.
    const RAISE_IN_MS: f32 = 130.0;
    const REVEAL_MS: f32 = 130.0;
    // Never dismiss before this — covers the cut-in and the trigger's own trailing key-up.
    const MIN_VISIBLE: Duration = Duration::from_millis(160);
    // If the trigger is genuinely held this long, arm anyway (baseline absorbs the held keys so they
    // can't auto-dismiss). Stops a held bind from trapping the curtain open forever.
    const DISARM_CEILING: Duration = Duration::from_millis(1500);

    let cls_name: Vec<u16> = "NeuronCurtain\0".encode_utf16().collect();

    // Remember who held the foreground, to hand it back on reveal.
    let prev_fg = unsafe { GetForegroundWindow() };

    let hwnd = unsafe {
        // Register the class. Re-registering across raises returns 0 (already exists) — harmless; the
        // first registration is the one that matters and the class lives for the process. The BLACK
        // background brush erases black on show/resize, so there is never a white flash before the
        // first static frame, and the HOLD stays black with no per-tick blit.
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(curtain_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            // A plain arrow — a null class cursor leaves whatever was last set (often the busy spinner)
            // smeared over the black. The live cursor is also the tell that this is a veil, not a dead
            // monitor.
            hCursor: LoadCursorW(std::ptr::null_mut(), 32512 as _), // IDC_ARROW
            hbrBackground: GetStockObject(BLACK_BRUSH).cast(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls_name.as_ptr(),
        };
        RegisterClassW(&raw const wc);

        // The virtual screen spans every monitor (and the gaps): SM_XVIRTUALSCREEN=76, Y=77, CX=78,
        // CY=79. One opaque window over all of it hides everything in a single surface.
        let x = GetSystemMetrics(76);
        let y = GetSystemMetrics(77);
        let w = GetSystemMetrics(78).max(1);
        let h = GetSystemMetrics(79).max(1);

        // TOPMOST (above normal windows) + TOOLWINDOW (no taskbar / alt-tab entry). We let it ACTIVATE
        // (no NOACTIVATE) and foreground it below: a topmost BACKGROUND window is left UNDER the
        // taskbar / shell bar on Windows, but the foreground fullscreen window covers it — the
        // platform-agnostic "cover everything" is just *be the fullscreen foreground window*. Opaque +
        // non-transparent means clicks land ON the curtain, not the app underneath.
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            cls_name.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            x,
            y,
            w,
            h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        // Foreground it so it sits ABOVE the taskbar / shell bar (covers it, like a fullscreen app).
        force_foreground(hwnd);
        hwnd
    };

    let t0 = Instant::now();
    let mut armed = false;
    let mut baseline = [false; 256];
    let mut reveal_start: Option<Instant> = None;
    let mut hold_painted = false;
    let mut tick: u32 = 0;

    loop {
        // Pump our thread's messages so the window paints and stays responsive.
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }

        // EDGE-CASE POLISH — re-assert ~5x/sec (skip while leaving): (1) force back to the top of the
        // topmost band so a LATE pop-up (a notification, neuron's own overlay) can't peek over the
        // black; (2) re-read the virtual-screen bounds and resize, so the curtain keeps covering every
        // monitor even if one changes resolution / sleeps / hot-plugs while we're up.
        if reveal_start.is_none() && tick.is_multiple_of(25) {
            unsafe {
                let vx = GetSystemMetrics(76);
                let vy = GetSystemMetrics(77);
                let vw = GetSystemMetrics(78).max(1);
                let vh = GetSystemMetrics(79).max(1);
                SetWindowPos(hwnd, HWND_TOPMOST, vx, vy, vw, vh, SWP_NOACTIVATE);
                // If something stole the foreground (so the shell bar could creep back), retake it —
                // a no-op while we're already foreground.
                if GetForegroundWindow() != hwnd {
                    force_foreground(hwnd);
                }
            }
            hold_painted = false; // a resize repaint may have cleared us; re-blit the hold black once
        }
        tick = tick.wrapping_add(1);

        let secs = t0.elapsed().as_secs_f32();

        if let Some(rev) = reveal_start {
            // BURST OUT — black → static, then cut to the live screen.
            let p = (rev.elapsed().as_secs_f32() * 1000.0 / REVEAL_MS).min(1.0);
            paint_frame(hwnd, secs, p);
            if p >= 1.0 {
                break;
            }
        } else {
            let raise_p = (t0.elapsed().as_secs_f32() * 1000.0 / RAISE_IN_MS).min(1.0);
            if raise_p < 1.0 {
                // CUT IN — full static settling to black. Opaque the whole way: content never leaks.
                paint_frame(hwnd, secs, 1.0 - raise_p);
                hold_painted = false;
            } else if !hold_painted {
                // HOLD — paint black once; the class brush keeps it black without a per-tick blit.
                paint_frame(hwnd, secs, 0.0);
                hold_painted = true;
            }

            // Arm / dismiss (unchanged): disarm until the trigger is released past the floor (or the
            // ceiling forces it), snapshot the baseline, then the first FRESH key/click bursts out.
            if !armed {
                let elapsed = t0.elapsed();
                let all_clear = !(1..256).any(crate::capture::key_down);
                if (all_clear && elapsed >= MIN_VISIBLE) || elapsed >= DISARM_CEILING {
                    for (vk, slot) in baseline.iter_mut().enumerate() {
                        *slot = crate::capture::key_down(vk as i32);
                    }
                    armed = true;
                }
            } else if (1..256).any(|vk| crate::capture::key_down(vk) && !baseline[vk as usize]) {
                reveal_start = Some(Instant::now());
            }
        }

        std::thread::sleep(Duration::from_millis(8));
    }

    unsafe {
        DestroyWindow(hwnd);
        // Hand the foreground back to whoever held it before the curtain rose.
        if !prev_fg.is_null() {
            SetForegroundWindow(prev_fg);
        }
        // Flush the destroy so the window is really gone before the thread exits.
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
            TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

/// Blit one frame: ask the painter for a small BGRA buffer at `intensity` (or plain black if none is
/// registered), then `StretchDIBits` it chunky-scaled across the whole client. Cheap — the buffer is
/// ~1/8 scale, and only the brief cut-in / burst-out paint per tick (the hold paints once).
#[cfg(windows)]
fn paint_frame(hwnd: windows_sys::Win32::Foundation::HWND, t: f32, intensity: f32) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{
        GetDC, ReleaseDC, SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER,
        COLORONCOLOR, DIB_RGB_COLORS, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetClientRect;

    unsafe {
        let mut rc: RECT = std::mem::zeroed();
        if GetClientRect(hwnd, &raw mut rc) == 0 {
            return;
        }
        let cw = (rc.right - rc.left).max(1);
        let ch = (rc.bottom - rc.top).max(1);
        // A small, chunky source buffer — blockiness IS the static look, and it's cheap to shade.
        let bw = (cw / 8).clamp(64, 640);
        let bh = ((bw * ch) / cw).max(1);

        let buf: Vec<u8> = {
            let g = PAINTER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            match g.as_ref() {
                Some(f) => f(&CurtainFrame {
                    w: bw as usize,
                    h: bh as usize,
                    t,
                    intensity,
                }),
                // No painter (CLI / tests): plain black. BGRA zeros with opaque alpha.
                None => vec![0u8; (bw * bh * 4) as usize],
            }
        };
        if buf.len() < (bw * bh * 4) as usize {
            return; // a malformed painter result — skip rather than read OOB
        }

        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bw,
            biHeight: -bh, // negative = top-down (row 0 is the top), matching our buffer
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0, // BI_RGB
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };

        let dc = GetDC(hwnd);
        if !dc.is_null() {
            SetStretchBltMode(dc, COLORONCOLOR); // nearest-neighbour → crisp chunky pixels
            StretchDIBits(
                dc,
                0,
                0,
                cw,
                ch,
                0,
                0,
                bw,
                bh,
                buf.as_ptr().cast(),
                &raw const bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ReleaseDC(hwnd, dc);
        }
    }
}

/// Bring the curtain to the FOREGROUND so it sits above the taskbar / shell bar — a topmost but
/// BACKGROUND window is left under the taskbar on Windows; the foreground fullscreen window covers it.
/// We're driven off the input thread (not a real click), so a bare `SetForegroundWindow` would be
/// refused — the standard `AttachThreadInput` handoff (briefly share the current foreground thread's
/// input queue) makes the grant stick. Conceptually portable: "become the fullscreen foreground
/// window" is how every desktop OS hides its shell bar.
#[cfg(windows)]
unsafe fn force_foreground(hwnd: windows_sys::Win32::Foundation::HWND) {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
    };
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() || fg == hwnd {
            SetForegroundWindow(hwnd);
            return;
        }
        let fg_thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        let our_thread = GetCurrentThreadId();
        if fg_thread == our_thread {
            SetForegroundWindow(hwnd);
        } else {
            AttachThreadInput(our_thread, fg_thread, 1);
            SetForegroundWindow(hwnd);
            AttachThreadInput(our_thread, fg_thread, 0);
        }
    }
}

/// Window procedure — the black background brush + `DefWindowProcW` is the whole story (we paint the
/// static ourselves via `StretchDIBits`; the brush only covers erases before/around that).
#[cfg(windows)]
unsafe extern "system" fn curtain_proc(
    hwnd: windows_sys::Win32::Foundation::HWND,
    msg: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW;
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
