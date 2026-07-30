// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The ONE Win32 layered-window seam — `LayeredSurface`.
//!
//! Every drawing overlay in the app (the spell overlay, the teleport ghost-marker + sparkle-frame,
//! the whiteboard canvas + palette, the glance frame) is the *same* Win32 recipe: register a
//! `WNDCLASS`, `CreateWindowExW(WS_POPUP | WS_EX_LAYERED | …)`, allocate a top-down 32bpp BGRA
//! `CreateDIBSection` into a memory DC, draw premultiplied pixels into the bits, and push the frame
//! to screen with `UpdateLayeredWindow` + a `BLENDFUNCTION` (AC_SRC_OVER + AC_SRC_ALPHA). That
//! boilerplate used to be hand-duplicated at every site; it now lives here, once.
//!
//! Cross-platform seam: this is the single file a non-Windows backend reimplements. The public type
//! ([`LayeredSurface`]) is platform-neutral in shape; the Windows body lives in `mod imp`, an inert
//! `mod stub` stands in off-Windows (mirroring the `overlay.rs` house pattern). macOS = a transparent
//! `NSWindow` with `ignoresMouseEvents` + a `CALayer`; X11/Wayland = an override-redirect /
//! layer-shell surface with an empty input region. Same create / bits / present / drop contract.

#[cfg(windows)]
pub use imp::{present_dc, Dib, LayeredSurface, SurfaceSpec};

#[cfg(not(windows))]
#[allow(unused_imports)]
pub use stub::{present_dc, Dib, LayeredSurface, SurfaceSpec};

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC,
        SelectObject, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, UpdateLayeredWindow,
        ULW_ALPHA, WNDCLASSW, WNDPROC, WS_EX_LAYERED, WS_POPUP,
    };

    /// The shared `BLENDFUNCTION`: per-pixel alpha over whatever's underneath (AC_SRC_OVER +
    /// AC_SRC_ALPHA). `alpha` is the window-wide constant alpha multiplied on top (255 = none).
    /// This is the ONE blend descriptor — `glance.rs` no longer rolls its own `#[repr(C)] Blend`.
    #[inline]
    fn blend(alpha: u8) -> BLENDFUNCTION {
        BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: alpha,
            AlphaFormat: AC_SRC_ALPHA as u8,
        }
    }

    /// Top-down 32bpp BGRA DIB header for a `w × h` surface (negative height = top-down, the
    /// scanline order every site's pixel math assumes). Format/stride/orientation are fixed here so
    /// no call site can drift.
    fn dib_header(w: i32, h: i32) -> BITMAPINFO {
        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        bmi
    }

    /// Push a memory DC's BGRA bits to a layered window with the shared blend — the raw
    /// `UpdateLayeredWindow` seam for sites that own the HWND + DC themselves and only want the one
    /// blended present call (e.g. the whiteboard palette, the glance frame). `pos` = the window's
    /// new top-left, or `None` to leave the position unchanged (a null dst point). `screen` is a DC
    /// for the screen (any), `src` is the bits' top-left within the DC (almost always 0,0).
    ///
    /// # Safety
    /// `hwnd` must be a live `WS_EX_LAYERED` window; `screen`/`mem` live DCs; `mem`'s selected
    /// bitmap must be at least `size` and hold valid premultiplied BGRA.
    pub unsafe fn present_dc(
        hwnd: HWND,
        screen: HDC,
        mem: HDC,
        pos: Option<POINT>,
        size: SIZE,
        alpha: u8,
    ) {
        let src = POINT { x: 0, y: 0 };
        let b = blend(alpha);
        match pos {
            Some(p) => {
                UpdateLayeredWindow(hwnd, screen, &p, &size, mem, &src, 0, &b, ULW_ALPHA);
            }
            None => {
                UpdateLayeredWindow(
                    hwnd,
                    screen,
                    std::ptr::null(),
                    &size,
                    mem,
                    &src,
                    0,
                    &b,
                    ULW_ALPHA,
                );
            }
        }
    }

    /// A bare top-down 32bpp BGRA DIB selected into its own memory DC, with NO window — for the
    /// "persistent window, but a backing DIB that is re-allocated when the window resizes" pattern
    /// (the glance frame, which keeps one HWND but rebuilds its DIB on every resize and re-resolves
    /// the live bit pointer per paint via `GetObjectW`). This is the ONE `CreateDIBSection` call that
    /// the resizable sites share; `LayeredSurface` (the create-once sites) uses the same call.
    pub struct Dib {
        pub dc: HDC,
        pub bmp: HBITMAP,
    }

    impl Dib {
        /// Allocate a `w × h` top-down BGRA DIB into a fresh `CreateCompatibleDC(null)` memory DC.
        /// `None` if the DC or section can't be made. The bits are reachable via `GetObjectW` on the
        /// bitmap (as the glance frame already does) — the format/orientation are fixed here.
        pub fn new(w: i32, h: i32) -> Option<Dib> {
            unsafe {
                let dc = CreateCompatibleDC(std::ptr::null_mut());
                if dc.is_null() {
                    return None;
                }
                let bmi = dib_header(w, h);
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let bmp = CreateDIBSection(
                    dc,
                    &bmi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                ) as HBITMAP;
                if bmp.is_null() || bits.is_null() {
                    DeleteDC(dc);
                    return None;
                }
                SelectObject(dc, bmp as _);
                Some(Dib { dc, bmp })
            }
        }
    }

    /// How to build one layered surface. Each call site has its OWN class name, ex_style, window
    /// style, size, and optional custom wndproc / cursor — captured here so the seam never
    /// homogenizes them.
    pub struct SurfaceSpec<'a> {
        /// The `WNDCLASS` name (also the class that gets registered). Registering the same name
        /// twice is harmless (Win32 ignores the redundant register), matching every site today.
        pub class: &'a str,
        /// Extra window styles OR'd with `WS_POPUP` (most sites pass 0; the whiteboard palette and
        /// glance tiles pass `WS_THICKFRAME`, etc.).
        pub win_style: u32,
        /// The `WS_EX_*` bits — `WS_EX_LAYERED` is OR'd in here so the present path always works.
        pub ex_style: u32,
        /// Initial window width/height (px). The DIB is allocated at `dib_w × dib_h` (often equal,
        /// but the sparkle-frame keeps a max-size DIB behind a smaller live window).
        pub w: i32,
        pub h: i32,
        /// DIB allocation size. Defaults to the window size; pass a larger pair to keep a fixed
        /// backing buffer (the teleport sparkle-frame's max-size DIB).
        pub dib_w: i32,
        pub dib_h: i32,
        /// Optional custom window procedure (the whiteboard palette's drag hit-test, the glance
        /// frame's). `None` = `DefWindowProcW`.
        pub wndproc: WNDPROC,
        /// Optional class cursor (an `IDC_*` value loaded by the caller). `None` = no cursor.
        pub cursor: windows_sys::Win32::UI::WindowsAndMessaging::HCURSOR,
    }

    impl<'a> SurfaceSpec<'a> {
        /// The common case: a click-through-or-not popup whose DIB matches the window size, default
        /// wndproc, no class cursor.
        pub fn new(class: &'a str, ex_style: u32, w: i32, h: i32) -> Self {
            SurfaceSpec {
                class,
                win_style: 0,
                ex_style,
                w,
                h,
                dib_w: w,
                dib_h: h,
                wndproc: Some(DefWindowProcW),
                cursor: std::ptr::null_mut(),
            }
        }
    }

    /// A layered window + its top-down BGRA `CreateDIBSection` backing store + the memory DC the DIB
    /// is selected into + a held screen DC for presenting. Owns the full Win32 teardown.
    pub struct LayeredSurface {
        hwnd: HWND,
        /// A screen DC, held for the surface's life and used as `UpdateLayeredWindow`'s source-DC
        /// arg (matches overlay/whiteboard which hold it; functionally identical to the per-present
        /// `GetDC` the marker/frame sites used).
        screen: HDC,
        mem: HDC,
        dib: HBITMAP,
        /// The bitmap previously selected into `mem` (restored before delete, as the sites do).
        old: windows_sys::Win32::Graphics::Gdi::HGDIOBJ,
        bits: *mut u32,
    }

    impl LayeredSurface {
        /// Register the class (idempotent), create the `WS_POPUP | ex_style` layered window, and
        /// allocate the top-down 32bpp BGRA DIB into a memory DC. `None` if the window can't be
        /// created. The DIB memory is NOT guaranteed zeroed — callers clear it exactly as before.
        pub fn new(spec: &SurfaceSpec) -> Option<LayeredSurface> {
            unsafe {
                let cls: Vec<u16> = {
                    let mut v: Vec<u16> = spec.class.encode_utf16().collect();
                    v.push(0);
                    v
                };
                let wc = WNDCLASSW {
                    style: 0,
                    lpfnWndProc: spec.wndproc,
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: std::ptr::null_mut(),
                    hIcon: std::ptr::null_mut(),
                    hCursor: spec.cursor,
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: cls.as_ptr(),
                };
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    // OR the layered bit in unconditionally — this seam's whole contract is a
                    // present-via-UpdateLayeredWindow surface, so a caller must never be able to
                    // create a non-layered window here (it would present to nothing, silently).
                    spec.ex_style | WS_EX_LAYERED,
                    cls.as_ptr(),
                    std::ptr::null(),
                    WS_POPUP | spec.win_style,
                    0,
                    0,
                    spec.w,
                    spec.h,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return None;
                }
                let screen = GetDC(std::ptr::null_mut());
                let mem = CreateCompatibleDC(screen);
                let bmi = dib_header(spec.dib_w, spec.dib_h);
                let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
                let dib = CreateDIBSection(
                    screen,
                    &bmi,
                    DIB_RGB_COLORS,
                    &mut bits,
                    std::ptr::null_mut(),
                    0,
                ) as HBITMAP;
                if dib.is_null() || bits.is_null() {
                    // match Dib::new's guard — a failed DIB section must not yield a surface whose
                    // bits() is null, since callers immediately write into it (UB). Unwind the
                    // window + DCs we already created and report failure honestly.
                    DeleteDC(mem);
                    ReleaseDC(std::ptr::null_mut(), screen);
                    DestroyWindow(hwnd);
                    return None;
                }
                let old = SelectObject(mem, dib as _);
                LayeredSurface {
                    hwnd,
                    screen,
                    mem,
                    dib,
                    old,
                    bits: bits as *mut u32,
                }
                .into()
            }
        }

        /// The layered window handle (for `SetWindowPos`/`ShowWindow`/`SetPropW`/z-order at the call
        /// site — z-order policy stays where it was).
        #[inline]
        pub fn hwnd(&self) -> HWND {
            self.hwnd
        }

        /// The memory DC the DIB is selected into — for sites that draw GDI furniture directly into
        /// it (the whiteboard palette's `FillRect`/`TextOutW`).
        #[inline]
        pub fn mem(&self) -> HDC {
            self.mem
        }

        /// Raw pointer to the DIB pixel bits — `dib_w * dib_h` premultiplied BGRA `u32`s, top-down.
        /// Sites that pointer-walk the buffer use this directly (unchanged math).
        #[inline]
        pub fn bits(&self) -> *mut u32 {
            self.bits
        }

        /// Present the current DIB contents to screen via the shared blend. `pos` = the window's new
        /// top-left (`None` keeps the current position), `size` = the source/dest extent, `alpha` =
        /// window-wide constant alpha (255 = pure per-pixel). One atomic `UpdateLayeredWindow`.
        #[inline]
        pub fn present(&self, pos: Option<POINT>, size: SIZE, alpha: u8) {
            unsafe { present_dc(self.hwnd, self.screen, self.mem, pos, size, alpha) }
        }

        /// Tear down JUST the DIB + memory DC + screen DC and hand back the still-live window — for
        /// the "paint once, then keep the window forever, re-positioned per frame" pattern (the
        /// teleport landing marker). The exact teardown the marker did inline (restore old bmp,
        /// delete DIB, delete mem DC, release screen) WITHOUT `DestroyWindow`.
        pub fn into_hwnd(self) -> HWND {
            unsafe {
                SelectObject(self.mem, self.old);
                DeleteObject(self.dib as _);
                DeleteDC(self.mem);
                ReleaseDC(std::ptr::null_mut(), self.screen);
                let hwnd = self.hwnd;
                std::mem::forget(self); // skip Drop — the window must survive
                hwnd
            }
        }
    }

    impl Drop for LayeredSurface {
        fn drop(&mut self) {
            unsafe {
                // restore + delete in the exact order the sites tore down:
                // SelectObject(old) → DeleteObject(dib) → DeleteDC(mem) → ReleaseDC(screen) →
                // DestroyWindow(hwnd).
                SelectObject(self.mem, self.old);
                DeleteObject(self.dib as _);
                DeleteDC(self.mem);
                ReleaseDC(std::ptr::null_mut(), self.screen);
                DestroyWindow(self.hwnd);
            }
        }
    }
}

#[cfg(not(windows))]
mod stub {
    //! Inert stand-in until the per-OS body lands (see module docs). Pointer-shaped so non-Windows
    //! code compiles; nothing draws.

    /// Off-Windows the surface holds nothing. The real one owns a layered HWND + DIB + DCs.
    pub struct LayeredSurface;

    /// Off-Windows a bare DIB holds nothing.
    pub struct Dib;

    impl Dib {
        pub fn new(_w: i32, _h: i32) -> Option<Dib> {
            None
        }
    }

    /// Mirror of the Windows spec so call sites stay cfg-free in shape.
    pub struct SurfaceSpec<'a> {
        pub class: &'a str,
        pub win_style: u32,
        pub ex_style: u32,
        pub w: i32,
        pub h: i32,
        pub dib_w: i32,
        pub dib_h: i32,
    }

    impl<'a> SurfaceSpec<'a> {
        pub fn new(class: &'a str, ex_style: u32, w: i32, h: i32) -> Self {
            SurfaceSpec {
                class,
                win_style: 0,
                ex_style,
                w,
                h,
                dib_w: w,
                dib_h: h,
            }
        }
    }

    impl LayeredSurface {
        pub fn new(_spec: &SurfaceSpec) -> Option<LayeredSurface> {
            None
        }
    }

    /// No-op present for the raw seam.
    ///
    /// # Safety
    /// Inert; takes no live handles off-Windows.
    pub unsafe fn present_dc() {}
}
