// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! TELEPORT — the nether-portal ratio for your desk. Hold the teleport slot's rhythm and a
//! mini-map of your REAL setup materializes at the cursor: every monitor where it actually sits
//! (live geometry, never guessed), open windows as abstract recency-lit blobs inside them. Drag
//! the ghost, release — the cursor warps there; release ON a window's blob and it focuses too.
//! ESC or a sub-deadzone release bails (a teleport you didn't commit costs nothing).
//!
//! The map is an honest projection: virtual-screen space, uniformly scaled to fit the overlay
//! canvas, aspect intact. All mapping math is pure (`project`, `target_of`, `window_at`) so the
//! geometry is testable without a desktop.

/// One open top-level window, abstracted: where it is + how recent (z-order, 0 = frontmost).
#[derive(Clone, Debug)]
pub struct WinBlob {
    pub rect: (i32, i32, i32, i32), // virtual-screen left, top, right, bottom
    pub hwnd: isize,
    pub z: usize,
    /// Lives on ANOTHER virtual desktop (shell-cloaked). Still teleportable — focusing it makes
    /// Windows switch desktops natively — but the map draws it hollow so you know it's elsewhere.
    pub other_desktop: bool,
}

/// Another virtual desktop, as a REALM: a small card on the map's edge holding its own windows.
/// (Your current desk stays the map; other desks are distinct little worlds, not noise mixed in.)
#[derive(Clone, Debug, Default)]
pub struct Realm {
    /// The desktop's real GUID (packed u128; 0 = unknown/combined) — what the spectral verbs
    /// hand to `IVirtualDesktopManager::MoveWindowToDesktop` when a carried window drops here.
    pub guid: u128,
    pub windows: Vec<WinBlob>,
}

/// The desk at the moment the map opened: virtual-screen bounds, monitors, the CURRENT desktop's
/// windows, other desktops as realms, and where the cursor was (the ghost's start).
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub vx: i32,
    pub vy: i32,
    pub vw: i32,
    pub vh: i32,
    pub monitors: Vec<(i32, i32, i32, i32)>,
    pub windows: Vec<WinBlob>,
    pub realms: Vec<Realm>,
    pub cursor: (i32, i32),
    /// Windows holding a tether anchor (any slot) at capture time — drawn on the map as warpstone
    /// pips so you can SEE where your anchors are while aiming. Filled in by the weave service from
    /// `wm::tether_hwnds()` (the map geometry stays decoupled from the window-manager verbs).
    pub tethers: Vec<isize>,
}

/// A spectrally carried window: `(its real screen rect, the ghost's canvas position)`.
pub type Carried = ((i32, i32, i32, i32), (f32, f32));

/// Realm-card geometry (canvas px): the strip of other-desktop cards under the map.
pub const REALM_W: f32 = 96.0;
pub const REALM_H: f32 = 58.0;
pub const REALM_GAP: f32 = 12.0;

/// Canvas px the map's long edge occupies (the overlay canvas is 560² with ~36px margins).
pub const MAP_EDGE: f32 = 420.0;
/// The overlay canvas is square; its half-extent (center). Kept in sync with `overlay::imp::W/H`
/// (560) so map geometry can be re-projected from canvas space back onto the SCREEN (the scry
/// bloom hugs the very minimap cell it mirrors instead of a guessed offset from the cursor).
pub const CANVAS_HALF: f32 = 280.0;
/// Mouse counts → map canvas px while dragging the ghost. Tuned so a deliberate drag crosses
/// the map without wrist gymnastics, while a flick's worth of motion stays a precise nudge.
pub const GHOST_GAIN: f32 = 0.9;
/// A release with less net motion than this (counts) is a bail, not a teleport to "right here".
pub const COMMIT_DEADZONE: f64 = 12.0;

impl Snapshot {
    /// Virtual-screen point → canvas-center-relative map point.
    pub fn project(&self, x: i32, y: i32) -> (f32, f32) {
        let s = self.scale();
        (
            (x - self.vx) as f32 * s - self.vw as f32 * s / 2.0,
            (y - self.vy) as f32 * s - self.vh as f32 * s / 2.0,
        )
    }

    /// Uniform map scale: the virtual screen's long edge fits MAP_EDGE.
    pub fn scale(&self) -> f32 {
        MAP_EDGE / (self.vw.max(self.vh).max(1) as f32)
    }

    /// Where a captured drag lands on the REAL desk: ghost = cursor + delta·gain in map space,
    /// inverse-projected and clamped inside the virtual screen (1px inset so the cursor never
    /// parks on a dead edge).
    pub fn target_of(&self, ddx: f64, ddy: f64) -> (i32, i32) {
        let s = self.scale().max(1e-6);
        let x = self.cursor.0 as f64 + (ddx * GHOST_GAIN as f64) / s as f64;
        let y = self.cursor.1 as f64 + (ddy * GHOST_GAIN as f64) / s as f64;
        (
            (x.round() as i32).clamp(self.vx + 1, self.vx + self.vw - 2),
            (y.round() as i32).clamp(self.vy + 1, self.vy + self.vh - 2),
        )
    }

    /// The frontmost window under a virtual-screen point, if any (blobs are z-sorted, 0 front).
    pub fn window_at(&self, x: i32, y: i32) -> Option<&WinBlob> {
        self.window_at_depth(x, y, 0)
    }

    /// The whole STACK under a point, front to back — what the depth dial walks. A borderless
    /// fullscreen app is just stack position 0; everything it covers is still right here.
    pub fn stack_at(&self, x: i32, y: i32) -> Vec<&WinBlob> {
        let mut v: Vec<&WinBlob> = self
            .windows
            .iter()
            .filter(|w| x >= w.rect.0 && x < w.rect.2 && y >= w.rect.1 && y < w.rect.3)
            .collect();
        v.sort_by_key(|w| w.z);
        v
    }

    /// The window `depth` steps down the stack under a point (clamped to the stack's bottom —
    /// over-scrolling rests on the deepest window, never on nothing).
    pub fn window_at_depth(&self, x: i32, y: i32, depth: i32) -> Option<&WinBlob> {
        let stack = self.stack_at(x, y);
        if stack.is_empty() {
            return None;
        }
        let i = (depth.max(0) as usize).min(stack.len() - 1);
        Some(stack[i])
    }

    /// Index of a blob in `windows` by hwnd (the overlay's hot-highlight key).
    pub fn index_of(&self, hwnd: isize) -> Option<usize> {
        self.windows.iter().position(|w| w.hwnd == hwnd)
    }

    /// The overlay window's top-left in SCREEN px — the map is drawn in a square canvas centered
    /// at the cursor, clamped onto the cursor's monitor (mirrors `overlay::place_window`). Knowing
    /// it lets canvas-relative geometry (a window blob's minimap cell) be turned back into screen
    /// coordinates so the scry bloom can sit right against the cell it mirrors.
    pub(crate) fn overlay_origin(&self) -> (i32, i32) {
        #[cfg(windows)]
        let (l, t, r, b) = unsafe { work_area(self.cursor) };
        // No monitor work-area query off Windows: clamp to the snapshot's own desk bounds.
        #[cfg(not(windows))]
        let (l, t, r, b) = (self.vx, self.vy, self.vx + self.vw, self.vy + self.vh);
        let half = CANVAS_HALF as i32;
        (
            (self.cursor.0 - half).clamp(l, (r - half * 2).max(l)),
            (self.cursor.1 - half).clamp(t, (b - half * 2).max(t)),
        )
    }

    /// A window blob's minimap CELL as a SCREEN rect — its projected canvas rect lifted back onto
    /// the glass through the overlay origin. The scry bloom anchors to this so it hugs the exact
    /// cell, never a fixed (often awkward) distance from the cursor.
    pub(crate) fn cell_screen(&self, rect: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
        let (ox, oy) = self.overlay_origin();
        let a = self.project(rect.0, rect.1);
        let z = self.project(rect.2, rect.3);
        let half = CANVAS_HALF;
        (
            ox + (half + a.0) as i32,
            oy + (half + a.1) as i32,
            ox + (half + z.0) as i32,
            oy + (half + z.1) as i32,
        )
    }

    /// Canvas-relative rect of realm card `i` — the strip sits centered under the projected desk.
    pub fn realm_card(&self, i: usize) -> [f32; 4] {
        let n = self.realms.len() as f32;
        let total = n * REALM_W + (n - 1.0).max(0.0) * REALM_GAP;
        let x0 = -total / 2.0 + i as f32 * (REALM_W + REALM_GAP);
        let desk_bottom = self.project(self.vx, self.vy + self.vh).1;
        let y0 = desk_bottom + 16.0;
        [x0, y0, x0 + REALM_W, y0 + REALM_H]
    }

    /// Canvas-relative rect of window `wi` inside realm `ri` — the desk's geometry, miniature.
    pub fn realm_blob(&self, ri: usize, wi: usize) -> [f32; 4] {
        let card = self.realm_card(ri);
        let w = &self.realms[ri].windows[wi];
        let sx = (REALM_W - 8.0) / self.vw.max(1) as f32;
        let sy = (REALM_H - 8.0) / self.vh.max(1) as f32;
        let s = sx.min(sy);
        let map = |x: i32, y: i32| {
            (
                card[0] + 4.0 + (x - self.vx) as f32 * s,
                card[1] + 4.0 + (y - self.vy) as f32 * s,
            )
        };
        let a = map(w.rect.0, w.rect.1);
        let b = map(w.rect.2, w.rect.3);
        [a.0, a.1, b.0, b.1]
    }

    /// What the ghost (canvas-relative) is touching in the realm strip: `(realm, Some(window))`
    /// on a blob, `(realm, None)` on a card's empty area (= that realm's frontmost).
    pub fn realm_hit(&self, gx: f32, gy: f32) -> Option<(usize, Option<usize>)> {
        for ri in 0..self.realms.len() {
            let c = self.realm_card(ri);
            if gx >= c[0] && gx < c[2] && gy >= c[1] && gy < c[3] {
                let mut best: Option<(usize, usize)> = None; // (z, wi)
                for wi in 0..self.realms[ri].windows.len() {
                    let b = self.realm_blob(ri, wi);
                    if gx >= b[0] && gx < b[2] && gy >= b[1] && gy < b[3] {
                        let z = self.realms[ri].windows[wi].z;
                        if best.map(|(bz, _)| z < bz).unwrap_or(true) {
                            best = Some((z, wi));
                        }
                    }
                }
                return Some((ri, best.map(|(_, wi)| wi)));
            }
        }
        None
    }

    /// The overlay's map payload: the CURRENT desk (monitor outlines + recency-lit window blobs)
    /// plus the other desktops as small REALM cards beneath — each its own little world with its
    /// own miniature blobs, never mixed into the desk. `hot` = the depth dial's pick on the desk
    /// (-1 = geometry decides); `hot_realm` = (realm, window) when the ghost is in the strip.
    pub fn map_mode_hot(&self, hot: i32, hot_realm: (i32, i32)) -> crate::overlay::WeaveMode {
        self.map_mode_aim(hot, hot_realm, (0, 0))
    }

    /// As [`map_mode_hot`], plus the DEPTH-DIAL affordance `depth = (index, count)` for the column
    /// under the ghost — the overlay shows the layer-stack pips when `count > 1`.
    pub fn map_mode_aim(
        &self,
        hot: i32,
        hot_realm: (i32, i32),
        depth: (i32, i32),
    ) -> crate::overlay::WeaveMode {
        self.map_mode_full(hot, hot_realm, None, depth)
    }

    /// As [`map_mode_hot`], plus a SPECTRALLY CARRIED window: `(real window rect, ghost pos)` —
    /// the blob is re-projected at desk scale and recentered on the ghost, so what you carry
    /// keeps its honest size while it rides. (No depth pips while carrying — your hand is full.)
    pub fn map_mode_carrying(
        &self,
        hot: i32,
        hot_realm: (i32, i32),
        carried: Option<Carried>,
    ) -> crate::overlay::WeaveMode {
        self.map_mode_full(hot, hot_realm, carried, (0, 0))
    }

    /// The full map payload builder — every caller funnels here so the projection lives once.
    fn map_mode_full(
        &self,
        hot: i32,
        hot_realm: (i32, i32),
        carried: Option<Carried>,
        depth: (i32, i32),
    ) -> crate::overlay::WeaveMode {
        let r = |(l, t, rr, b): (i32, i32, i32, i32)| {
            let a = self.project(l, t);
            let z = self.project(rr, b);
            [a.0, a.1, z.0, z.1]
        };
        let carried = carried.map(|(rect, (gx, gy))| {
            let m = r(rect);
            let (w, h) = (m[2] - m[0], m[3] - m[1]);
            [gx - w / 2.0, gy - h / 2.0, gx + w / 2.0, gy + h / 2.0]
        });
        let n = self.windows.len().max(1) as f32;
        crate::overlay::WeaveMode::Map {
            monitors: self.monitors.iter().map(|m| r(*m)).collect(),
            windows: self
                .windows
                .iter()
                .map(|w| (r(w.rect), 1.0 - (w.z as f32 / n) * 0.85))
                .collect(),
            realms: (0..self.realms.len())
                .map(|ri| {
                    let nn = self.realms[ri].windows.len().max(1) as f32;
                    (
                        self.realm_card(ri),
                        (0..self.realms[ri].windows.len())
                            .map(|wi| {
                                (
                                    self.realm_blob(ri, wi),
                                    1.0 - (self.realms[ri].windows[wi].z as f32 / nn) * 0.8,
                                )
                            })
                            .collect(),
                    )
                })
                .collect(),
            cursor: self.project(self.cursor.0, self.cursor.1),
            hot,
            hot_realm,
            carried,
            // warpstone pips: the canvas centre of every tethered window's cell (desk blob, else
            // its realm blob) so you can SEE where your anchors are while you aim.
            tethers: self
                .tethers
                .iter()
                .filter_map(|&h| {
                    if let Some(w) = self.windows.iter().find(|w| w.hwnd == h) {
                        let c = r(w.rect);
                        Some(((c[0] + c[2]) / 2.0, (c[1] + c[3]) / 2.0))
                    } else {
                        // a tether on another desktop: pin it to the centre of its realm blob
                        self.realms.iter().enumerate().find_map(|(ri, rm)| {
                            rm.windows.iter().position(|w| w.hwnd == h).map(|wi| {
                                let b = self.realm_blob(ri, wi);
                                ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0)
                            })
                        })
                    }
                })
                .collect(),
            depth,
        }
    }

    /// The default map payload (no dial selection yet).
    pub fn map_mode(&self) -> crate::overlay::WeaveMode {
        self.map_mode_hot(-1, (-1, -1))
    }
}

/// Photograph the desk: monitors via EnumDisplayMonitors, windows via EnumWindows (visible,
/// titled, uncloaked, non-tool, z-capped). Best-effort — a missing piece degrades the map,
/// never the warp.
#[cfg(windows)]
pub fn snapshot() -> Snapshot {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
    use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetCursorPos, GetSystemMetrics, GetWindowLongW, GetWindowRect,
        GetWindowTextLengthW, IsIconic, IsWindowVisible, GWL_EXSTYLE, SM_CXVIRTUALSCREEN,
        SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WS_EX_TOOLWINDOW,
    };

    unsafe extern "system" fn mon_cb(_h: HMONITOR, _dc: HDC, rect: *mut RECT, lp: LPARAM) -> i32 {
        let v = unsafe { &mut *(lp as *mut Vec<(i32, i32, i32, i32)>) };
        let r = unsafe { &*rect };
        v.push((r.left, r.top, r.right, r.bottom));
        1
    }

    unsafe extern "system" fn win_cb(hwnd: HWND, lp: LPARAM) -> i32 {
        unsafe {
            let v = &mut *(lp as *mut Vec<WinBlob>);
            if v.len() >= 24 {
                return 0; // a map, not a task manager — the recent two dozen tell the story
            }
            if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                return 1;
            }
            if GetWindowTextLengthW(hwnd) == 0 {
                return 1;
            }
            if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0 {
                return 1;
            }
            // Ask DWM about cloaking — and READ THE REASON: app-cloaked (1) / inherited (4) are
            // UWP ghosts (skip), but SHELL-cloaked (2) means "on another virtual desktop" —
            // those are real, teleportable destinations (focusing one switches desktops).
            let mut cloaked: u32 = 0;
            let _ = DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED as u32,
                &mut cloaked as *mut u32 as *mut _,
                std::mem::size_of::<u32>() as u32,
            );
            let other_desktop = cloaked == 2;
            if cloaked != 0 && !other_desktop {
                return 1;
            }
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd, &mut r) == 0 || r.right - r.left < 60 || r.bottom - r.top < 40 {
                return 1;
            }
            let z = v.len(); // EnumWindows walks in z-order, top first
            v.push(WinBlob {
                rect: (r.left, r.top, r.right, r.bottom),
                hwnd: hwnd as isize,
                z,
                other_desktop,
            });
            1
        }
    }

    unsafe {
        let mut monitors: Vec<(i32, i32, i32, i32)> = Vec::new();
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(mon_cb),
            &mut monitors as *mut _ as isize,
        );
        let mut all: Vec<WinBlob> = Vec::new();
        EnumWindows(Some(win_cb), &mut all as *mut _ as isize);
        // split: the current desk keeps its windows; other-desktop ones group into REALMS by
        // their real desktop id (IVirtualDesktopManager). If the shell COM isn't reachable,
        // they degrade into one combined realm — still reachable, just less sorted.
        let vdm = vdm::open();
        let mut windows: Vec<WinBlob> = Vec::new();
        let mut realm_ids: Vec<u128> = Vec::new();
        let mut realms: Vec<Realm> = Vec::new();
        for w in all {
            if !w.other_desktop {
                windows.push(w);
                continue;
            }
            let id = vdm.as_ref().map(|v| v.desktop_of(w.hwnd)).unwrap_or(0);
            let ri = match realm_ids.iter().position(|&g| g == id) {
                Some(i) => i,
                None => {
                    realm_ids.push(id);
                    realms.push(Realm {
                        guid: id,
                        windows: Vec::new(),
                    });
                    realms.len() - 1
                }
            };
            realms[ri].windows.push(w);
        }
        let mut cur = POINT { x: 0, y: 0 };
        GetCursorPos(&mut cur);
        Snapshot {
            vx: GetSystemMetrics(SM_XVIRTUALSCREEN),
            vy: GetSystemMetrics(SM_YVIRTUALSCREEN),
            vw: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            vh: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
            monitors,
            windows,
            realms,
            cursor: (cur.x, cur.y),
            tethers: Vec::new(), // the weave service stamps these from wm::tether_hwnds()
        }
    }
}

/// Hand-rolled `IVirtualDesktopManager` (the documented shell COM object) — just enough to ask
/// "which desktop is this window on" so realms group truthfully. Same no-deps COM style as the
/// audio layer: declared vtable, explicit release.
#[cfg(windows)]
mod vdm {
    use std::ffi::c_void;
    use windows_sys::core::GUID;
    use windows_sys::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
    };

    const CLSID_VDM: GUID = GUID::from_u128(0xAA509086_5CA9_4C25_8F95_589D3C07B48A);
    const IID_VDM: GUID = GUID::from_u128(0xA5CD92FF_29BE_454C_8D04_D82879FB3F1B);

    #[repr(C)]
    struct Vtbl {
        query_interface:
            unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
        add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
        release: unsafe extern "system" fn(*mut c_void) -> u32,
        // declared in interface order; is_on_current is ABI padding (never called) so the
        // other slots land at the right offsets.
        is_on_current: unsafe extern "system" fn(*mut c_void, isize, *mut i32) -> i32,
        get_desktop_id: unsafe extern "system" fn(*mut c_void, isize, *mut GUID) -> i32,
        move_to_desktop: unsafe extern "system" fn(*mut c_void, isize, *const GUID) -> i32,
    }

    pub struct Vdm(*mut *const Vtbl);

    /// Pack a shell desktop GUID into a u128 realm key (and back).
    fn pack(g: &GUID) -> u128 {
        ((g.data1 as u128) << 96)
            | ((g.data2 as u128) << 80)
            | ((g.data3 as u128) << 64)
            | u64::from_be_bytes(g.data4) as u128
    }

    fn unpack(id: u128) -> GUID {
        GUID {
            data1: (id >> 96) as u32,
            data2: (id >> 80) as u16,
            data3: (id >> 64) as u16,
            data4: (id as u64).to_be_bytes(),
        }
    }

    impl Vdm {
        /// The desktop GUID (as u128; 0 = unknown) of a top-level window.
        pub fn desktop_of(&self, hwnd: isize) -> u128 {
            unsafe {
                let mut g = GUID::from_u128(0);
                if ((**self.0).get_desktop_id)(self.0 as *mut c_void, hwnd, &mut g) == 0 {
                    pack(&g)
                } else {
                    0
                }
            }
        }

        /// MOVE a window to the desktop with this realm key — the spectral grab's cross-desktop
        /// drop. Documented shell COM (`MoveWindowToDesktop`); works for our own and most app
        /// windows. False on shell refusal (elevated windows, special surfaces).
        pub fn move_window(&self, hwnd: isize, realm: u128) -> bool {
            if realm == 0 {
                return false;
            }
            unsafe {
                let g = unpack(realm);
                ((**self.0).move_to_desktop)(self.0 as *mut c_void, hwnd, &g) == 0
            }
        }
    }

    impl Drop for Vdm {
        fn drop(&mut self) {
            unsafe {
                ((**self.0).release)(self.0 as *mut c_void);
            }
        }
    }

    pub fn open() -> Option<Vdm> {
        unsafe {
            // best-effort apartment join (an already-initialized thread errs harmlessly)
            let _ = CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32);
            let mut p: *mut c_void = std::ptr::null_mut();
            if CoCreateInstance(
                &CLSID_VDM,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_VDM,
                &mut p,
            ) == 0
                && !p.is_null()
            {
                Some(Vdm(p as *mut *const Vtbl))
            } else {
                None
            }
        }
    }
}

#[cfg(not(windows))]
pub fn snapshot() -> Snapshot {
    Snapshot::default()
}

/// Commit the teleport: warp the cursor to `target`, and bring `chosen` (the depth-dial's pick —
/// falls back to the frontmost blob under the point) forward too. An other-desktop pick makes
/// Windows switch desktops natively via the focus. Returns the status line. NOT arm-gated by
/// design choice: moving your own cursor at your own deliberate gesture is navigation, not input
/// synthesis into an app. (SendInput is untouched.)
#[cfg(windows)]
pub fn commit(snap: &Snapshot, ddx: f64, ddy: f64, chosen: Option<isize>) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;
    let (x, y) = snap.target_of(ddx, ddy);
    let blob = chosen
        .and_then(|h| snap.windows.iter().find(|w| w.hwnd == h))
        .or_else(|| snap.window_at(x, y));
    unsafe {
        if let Some(w) = blob {
            // force_foreground, not a bare SetForegroundWindow: a maximized app owning the
            // foreground silently denies the plain call and the warp lands BEHIND it.
            force_foreground(w.hwnd);
        }
        SetCursorPos(x, y);
    }
    // (no other_desktop arm: snapshot() partitions those blobs into realms, so the desk map —
    //  and therefore this commit path — only ever sees current-desktop windows; realm picks
    //  land in commit_at.)
    match blob {
        Some(_) => format!("teleport \u{2192} ({x}, {y}) + focus"),
        None => format!("teleport \u{2192} ({x}, {y})"),
    }
}

#[cfg(not(windows))]
pub fn commit(_snap: &Snapshot, _ddx: f64, _ddy: f64, _chosen: Option<isize>) -> String {
    "teleport: windows-only".into()
}

/// Commit a REALM pick: focus the window (Windows switches desktops natively) and land the
/// cursor at the mapped point inside it — stepping through the card is stepping through the
/// portal.
#[cfg(windows)]
pub fn commit_at(x: i32, y: i32, hwnd: isize) -> String {
    use windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos;
    unsafe {
        force_foreground(hwnd); // the desktop switch rides the focus — it must really land
        SetCursorPos(x, y);
    }
    format!("teleport \u{2192} other desktop ({x}, {y})")
}

#[cfg(not(windows))]
pub fn commit_at(_x: i32, _y: i32, _hwnd: isize) -> String {
    "teleport: windows-only".into()
}

// ── SCRY — the live peek (haptic-touch for windows) ─────────────────────────────────────────────
// Dwell the ghost on a blob and it BLOOMS: a portal opens showing the REAL window, live (DWM
// thumbnail — the compositor mirrors the actual surface, so Discord scrolls, video plays), while
// the real window stays unfocused and untouched. Keep aiming on the blob; release = warp+focus at
// that spot (the existing commit). Slide off the blob and the portal dissolves. Pure VIEW — the
// one thing Windows lets every process do to every window without touching it.
//
// The portal is a plain (non-layered) popup on its own pumped thread: DWM thumbnails composite
// only into ordinary windows, and a window must pump to stay healthy while a bloom is held.

/// How long the ghost must rest on one blob before it blooms (the haptic-touch threshold).
pub const SCRY_DWELL_MS: u64 = 350;

pub enum ScryCmd {
    /// Show `src`'s live surface in a portal hugging the minimap CELL it mirrors: `near` is that
    /// cell's screen rect (the bloom floats just above it, dropping just below when the top is
    /// tight), clamped to `near`'s monitor — so the peek sits beside its blob, never a guessed
    /// distance from the cursor and never on top of the map.
    Show {
        src: isize,
        src_rect: (i32, i32, i32, i32),
        near: (i32, i32, i32, i32),
    },
    /// Place the LANDING MARKER at this fraction of the source window — the portal shows
    /// exactly where the cursor will materialize.
    Aim {
        fx: f32,
        fy: f32,
    },
    Hide,
}

// `service_sender` caches the send-end only once the worker spawned — a refused spawn retries on
// the next call rather than stranding scry commands in a dead channel. `None` = can't start now.
#[cfg(windows)]
pub fn scry() -> Option<std::sync::mpsc::Sender<ScryCmd>> {
    static TX: crate::worker::Service<ScryCmd> = crate::worker::Service::new();
    crate::worker::service_sender(&TX, "neuron-scry", scry_thread)
}

/// Send a scry command, starting the worker on demand. Silently no-ops if the worker can't be
/// started (a refused spawn) — the peek simply doesn't appear, and the next call retries.
#[cfg(windows)]
pub fn scry_send(cmd: ScryCmd) {
    if let Some(tx) = scry() {
        let _ = tx.send(cmd);
    }
}

#[cfg(not(windows))]
pub fn scry_send(_cmd: ScryCmd) {}

/// The teleport bloom portal (anchored to the map, with the landing marker). GLANCE grew into
/// its own constellation engine (`crate::glance`) with its own tile windows — this thread now
/// serves the weave alone, so a pinned glance and a teleport peek can never stomp each other.
#[cfg(windows)]
fn scry_thread(rx: std::sync::mpsc::Receiver<ScryCmd>) {
    use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
    use windows_sys::Win32::Graphics::Dwm::{
        DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
        DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, LoadCursorW, PeekMessageW,
        RegisterClassW, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, MSG, PM_REMOVE,
        SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };
    unsafe {
        let cls: Vec<u16> = "NeuronScry\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(DefWindowProcW),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            hCursor: LoadCursorW(std::ptr::null_mut(), 32512 as _), // IDC_ARROW, never the spinner
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            cls.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return;
        }
        let marker = spawn_marker();
        // the SPARKLE FRAME around the portal (item 15b): one max-size DIB, repainted per frame at
        // the live portal size and pushed via UpdateLayeredWindow. Created once; the thread owns it.
        // A click-through, transparent, topmost layered popup whose DIB is the FULL max frame size,
        // presented at the live portal size each paint.
        let frame_surf = match crate::surface::LayeredSurface::new(&crate::surface::SurfaceSpec::new(
            "NeuronScryFrame",
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            SCRY_FRAME_W,
            SCRY_FRAME_H,
        )) {
            Some(s) => s,
            None => return,
        };
        let frame_win = frame_surf.hwnd();
        let fpx = frame_surf.bits();
        let mut thumb: isize = 0;
        // the portal's live geometry — what Aim maps source-fractions through.
        let mut portal: Option<(i32, i32, i32, i32)> = None; // (x, y, w, h)
        let mut frame_panics = 0u32; // per-frame render panic streak (contain_frame throttle)
        loop {
            // drain commands: the latest Show/Hide wins; the latest Aim rides along.
            let mut latest: Option<ScryCmd> = None;
            let mut aim: Option<(f32, f32)> = None;
            while let Ok(c) = rx.try_recv() {
                match c {
                    ScryCmd::Aim { fx, fy } => aim = Some((fx, fy)),
                    other => latest = Some(other),
                }
            }
            // contain a per-command panic (a stale DWM thumbnail handle, odd geometry) so it can't
            // kill the scry pump for the run.
            crate::worker::contain("neuron-scry", || {
            if let Some(cmd) = latest {
                if thumb != 0 {
                    DwmUnregisterThumbnail(thumb);
                    thumb = 0;
                }
                match cmd {
                    ScryCmd::Hide => {
                        ShowWindow(hwnd, SW_HIDE);
                        if !marker.is_null() {
                            ShowWindow(marker, SW_HIDE);
                        }
                        ShowWindow(frame_win, SW_HIDE);
                        portal = None;
                    }
                    ScryCmd::Show {
                        src,
                        src_rect,
                        near,
                    } => {
                        // size: the window's aspect at a readable scale (≤ 640 × ≤ 420)
                        let (sw, sh) = (
                            (src_rect.2 - src_rect.0).max(1) as f32,
                            (src_rect.3 - src_rect.1).max(1) as f32,
                        );
                        let k = (640.0 / sw).min(420.0 / sh).min(1.0);
                        let (pw, ph) = ((sw * k) as i32, (sh * k) as i32);
                        // the teleport bloom HUGS its cell: centered on the minimap blob it mirrors,
                        // floating just above with a hairline gap, dropping just below when the top
                        // is tight — never the old fixed (and often awkward) reach from the cursor,
                        // never landing on the map. Clamped fully onto the cell's monitor.
                        const GAP: i32 = 14;
                        let cx = (near.0 + near.2) / 2;
                        let (wl, wt, wr, wb) = work_area((cx, (near.1 + near.3) / 2));
                        let px = (cx - pw / 2).clamp(wl, (wr - pw).max(wl));
                        let mut py = near.1 - GAP - ph; // just above the cell
                        if py < wt {
                            py = near.3 + GAP; // no headroom — sit just below it instead
                        }
                        let py = py.clamp(wt, (wb - ph).max(wt));
                        SetWindowPos(hwnd, HWND_TOPMOST, px, py, pw, ph, SWP_NOACTIVATE);
                        ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        portal = Some((px, py, pw, ph));
                        ShowWindow(frame_win, SW_SHOWNOACTIVATE); // the sparkle frame rides along
                        if DwmRegisterThumbnail(hwnd, src as _, &mut thumb) == 0 {
                            let props = DWM_THUMBNAIL_PROPERTIES {
                                dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
                                rcDestination: RECT {
                                    left: 0,
                                    top: 0,
                                    right: pw,
                                    bottom: ph,
                                },
                                rcSource: RECT {
                                    left: 0,
                                    top: 0,
                                    right: 0,
                                    bottom: 0,
                                },
                                opacity: 255,
                                fVisible: 1,
                                fSourceClientAreaOnly: 0,
                            };
                            DwmUpdateThumbnailProperties(thumb, &props);
                        }
                    }
                    ScryCmd::Aim { .. } => unreachable!("split above"),
                }
            }
            });
            // the per-frame render — marker projection + sparkle-frame paint/present — runs every
            // tick (pixel/geometry work, the likeliest panic surface), so it's contained on its own
            // hot lane: a panicked frame is a dropped frame, repainted next tick.
            crate::worker::contain_frame("neuron-scry", &mut frame_panics, || {
                // the landing marker: the ghost's exact spot, projected into the portal — the
                // peek SHOWS where the cursor will appear (a marker window floats above the
                // thumbnail, since DWM composites thumbnails over the portal's own pixels).
                if let (Some((px, py, pw, ph)), Some((fx, fy))) = (portal, aim) {
                    if !marker.is_null() {
                        let mx = px + (fx.clamp(0.0, 1.0) * pw as f32) as i32 - MARK / 2;
                        let my = py + (fy.clamp(0.0, 1.0) * ph as f32) as i32 - MARK / 2;
                        SetWindowPos(marker, HWND_TOPMOST, mx, my, MARK, MARK, SWP_NOACTIVATE);
                        ShowWindow(marker, SW_SHOWNOACTIVATE);
                    }
                }
                // repaint the sparkle frame around the live portal (animated rim + travelling sparkles)
                if let Some((px, py, pw, ph)) = portal {
                    if !frame_win.is_null() {
                        paint_scry_frame(
                            &mut crate::raster::PixelBuf::from_raw_parts(fpx, SCRY_FRAME_W, SCRY_FRAME_H),
                            pw,
                            ph,
                        );
                        let fpos = POINT {
                            x: px - SCRY_FRAME_B,
                            y: py - SCRY_FRAME_B,
                        };
                        let fsize = SIZE {
                            cx: pw + 2 * SCRY_FRAME_B,
                            cy: ph + 2 * SCRY_FRAME_B,
                        };
                        frame_surf.present(Some(fpos), fsize, 255);
                    }
                }
            });
            // the message pump is NOT contained: a panic across the `extern "system"` window-proc
            // ABI aborts the process, so catch_unwind here would be misleading dead code.
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }
}

/// The anchor's monitor work area — pub(crate) so the glance constellation clamps with the
/// same truth the portals use.
#[cfg(windows)]
pub(crate) fn work_area_of(p: (i32, i32)) -> (i32, i32, i32, i32) {
    unsafe { work_area(p) }
}

/// The landing-marker size (px) — a phosphor ring + white core, overlay-style.
#[cfg(windows)]
const MARK: i32 = 26;

/// The anchor's monitor work area (fallback: a generous box around the anchor).
#[cfg(windows)]
unsafe fn work_area(p: (i32, i32)) -> (i32, i32, i32, i32) {
    unsafe {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{
            GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
        };
        let mon = MonitorFromPoint(POINT { x: p.0, y: p.1 }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !mon.is_null() && GetMonitorInfoW(mon, &mut mi) != 0 {
            (
                mi.rcWork.left,
                mi.rcWork.top,
                mi.rcWork.right,
                mi.rcWork.bottom,
            )
        } else {
            (p.0 - 1200, p.1 - 800, p.0 + 1200, p.1 + 800)
        }
    }
}

/// Build the landing-marker window: a tiny click-through layered ring (phosphor) with a white
/// core — the same visual language as the spell overlay, floating over the portal.
#[cfg(windows)]
unsafe fn spawn_marker() -> windows_sys::Win32::Foundation::HWND {
    unsafe {
        use crate::surface::{LayeredSurface, SurfaceSpec};
        use windows_sys::Win32::Foundation::SIZE;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT,
        };
        let surf = match LayeredSurface::new(&SurfaceSpec::new(
            "NeuronScryMark",
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            MARK,
            MARK,
        )) {
            Some(s) => s,
            None => return std::ptr::null_mut(),
        };
        // paint the pip once (premultiplied ARGB): phosphor ring + white core, soft falloff.
        let px = surf.bits();
        let c = (MARK / 2) as f32;
        // the ring wears the user's live WEAVE colour (settings-driven); the core stays white.
        let (ar, ag, ab) = crate::weave::overlay_accent();
        let (ar, ag, ab) = (ar * 255.0, ag * 255.0, ab * 255.0);
        for y in 0..MARK {
            for x in 0..MARK {
                let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
                // ring at r≈8 (accent), core dot r<2.5 (white)
                let ring = (1.0 - ((d - 8.0).abs() / 2.2)).clamp(0.0, 1.0);
                let core = (1.0 - d / 2.5).clamp(0.0, 1.0);
                let a = ((ring * 0.85 + core) * 255.0).min(255.0) as u32;
                let r = ((ar * ring + 255.0 * core).min(255.0)) as u32 * a / 255;
                let g = ((ag * ring + 255.0 * core).min(255.0)) as u32 * a / 255;
                let b = ((ab * ring + 255.0 * core).min(255.0)) as u32 * a / 255;
                *px.add((y * MARK + x) as usize) = (a << 24) | (r << 16) | (g << 8) | b;
            }
        }
        // present at the origin, then hand back the live window (the DIB+DC are torn down; the
        // marker window survives and is re-positioned per frame by the scry thread).
        surf.present(
            Some(windows_sys::Win32::Foundation::POINT { x: 0, y: 0 }),
            SIZE { cx: MARK, cy: MARK },
            255,
        );
        surf.into_hwnd()
    }
}

const SCRY_FRAME_B: i32 = 16; // sparkle-border margin around the portal
const SCRY_FRAME_W: i32 = 640 + 2 * SCRY_FRAME_B; // the portal is capped ≤ 640×420 in Show
const SCRY_FRAME_H: i32 = 420 + 2 * SCRY_FRAME_B;


/// Paint the sparkle frame into `px` (a premultiplied-ARGB DIB `SCRY_FRAME_W × SCRY_FRAME_H`, the
/// fixed size it was allocated at) for the live portal size `pw×ph`: a glowing phosphor rim
/// hugging the portal's edge that drifts through a faint prism (the white→aberration look), with
/// six diffused sparkle crests sliding around the perimeter. The interior (where the thumbnail
/// sits) stays fully transparent so the peek shows.
///
/// `pw`/`ph` are bounded ≤ 640×420 by `Show`'s scale-down (`fw`/`fh` therefore ≤ `SCRY_FRAME_W`/
/// `SCRY_FRAME_H`) — but that invariant lives at a distant call site, not here. `px` takes a
/// bounds-checked [`PixelBuf`] fixed at the DIB's real allocation size instead of a raw pointer +
/// the derived `fw`/`fh`, so if that distant invariant is ever loosened, a too-large portal clips
/// its sparkle frame at the DIB edge instead of corrupting adjacent memory.
#[cfg(windows)]
fn paint_scry_frame(px: &mut crate::raster::PixelBuf, pw: i32, ph: i32) {
    use crate::weave::{phase, swell, tempo};
    use std::f32::consts::{PI, TAU};
    let b = SCRY_FRAME_B;
    let fw = pw + 2 * b;
    let fh = ph + 2 * b;
    let (cx, cy) = (fw as f32 / 2.0, fh as f32 / 2.0);
    let (rx0, ry0, rx1, ry1) = (b as f32, b as f32, (b + pw) as f32, (b + ph) as f32);
    let band = b as f32 * 0.95;
    // the material's TEMPO drives every moving part — wall-clock, frame-rate independent, off the
    // shared Directed-Intent clock (so this can never strobe again, and a 60fps portal reads the
    // same as a 30fps canvas). Compute the time-varying phases ONCE per paint, not per pixel.
    let orbit = phase(tempo::ORBIT); // where the six glints sit on the rim this instant
    let drift = phase(tempo::DRIFT); // the prism hue's slow wander
    let breath = swell(tempo::BREATH, 0.93, 1.0); // a barely-there glow inhale on the rim
    // the rim wears the user's live WEAVE colour (settings-driven), not a hardcoded phosphor green —
    // read once per paint (cheap lock), never per pixel.
    let (ar, ag, ab) = crate::weave::overlay_accent();
    for y in 0..fh {
        for x in 0..fw {
            let fx = x as f32;
            let fy = y as f32;
            // distance to the portal's rectangular outline (0 on the edge)
            let qx = (fx - rx0).min(rx1 - fx);
            let qy = (fy - ry0).min(ry1 - fy);
            let d = if qx > 0.0 && qy > 0.0 {
                qx.min(qy)
            } else {
                let ox = (-qx).max(0.0);
                let oy = (-qy).max(0.0);
                (ox * ox + oy * oy).sqrt()
            };
            let mut glow = (1.0 - d / band).clamp(0.0, 1.0);
            glow *= glow;
            if glow < 0.012 {
                px.put(x, y, 0);
                continue;
            }
            // perimeter angle (0..TAU) for the prism hue + the travelling sparkle crests
            let pa = (fy - cy).atan2(fx - cx) + PI;
            let tn = pa / TAU;
            let mut spark = 0.0f32;
            for j in 0..6 {
                let aj = orbit + j as f32 / 6.0 * TAU; // six glints evenly spaced, orbiting at ORBIT
                let mut ad = (pa - aj) % TAU;
                if ad > PI {
                    ad -= TAU;
                }
                if ad < -PI {
                    ad += TAU;
                }
                let s = (1.0 - ad.abs() / 0.28).clamp(0.0, 1.0);
                spark += s * s;
            }
            spark = spark.min(1.0) * glow;
            let rim_a = glow * 0.62 * breath; // the rim glow breathes, gently, on the shared clock
            let a = ((rim_a + spark) * 255.0).min(255.0);
            if a < 1.0 {
                px.put(x, y, 0);
                continue;
            }
            // phosphor rim drifting through a faint prism (white→aberration); sparkles burn white.
            // the hue wander rides the perimeter angle + the material's slow DRIFT phase.
            let hue_phase = tn * TAU + drift;
            let pr = 0.5 + 0.5 * hue_phase.sin();
            let pg = 0.5 + 0.5 * (hue_phase + 2.094).sin();
            let pb = 0.5 + 0.5 * (hue_phase + 4.188).sin();
            let cr = (ar * rim_a + pr * rim_a * 0.35 + spark).min(1.0);
            let cg = (ag * rim_a + pg * rim_a * 0.35 + spark).min(1.0);
            let cb = (ab * rim_a + pb * rim_a * 0.35 + spark).min(1.0);
            let a8 = a as u32;
            let rr = (cr * 255.0) as u32 * a8 / 255;
            let gg = (cg * 255.0) as u32 * a8 / 255;
            let bb = (cb * 255.0) as u32 * a8 / 255;
            px.put(x, y, (a8 << 24) | (rr << 16) | (gg << 8) | bb);
        }
    }
}

// ── SPECTRAL VERBS — windows as movable matter, from the same minimap ──────────────────────────
// Teleport moves YOU to windows; these move WINDOWS for you, with the map you already know:
//   LEFT-CLICK the aimed blob   = SUMMON it to your hand (the weave's origin), cross-desktop too
//   RIGHT-CLICK = spectral GRAB = the blob rides the ghost; release drops it:
//       on a monitor      → it lands there (centered at the ghost's real point)
//       on a realm card   → it moves to that desktop
//       below, on no card → a NEW desktop is forged for it (you follow the throw)
//       UP-FLICK          → AUTO-SORT: with its kin (the monitor holding its siblings), else
//                           the emptiest glass — deterministic, and the status says which.
// Window placement is host-side window management (SetWindowPos), not input synthesis — same
// stance as commit(). Forging a new desktop DOES synthesize Win+Ctrl+D, so it honors the arm gate.

#[cfg(not(windows))]
pub fn summon(_hwnd: isize, _to: (i32, i32), _cross_desktop: bool) -> String {
    "summon: windows-only".into()
}

/// SUMMON: bring a window to a real screen point (the weave origin). `cross_desktop` pulls it
/// to the current desktop first.
#[cfg(windows)]
pub fn summon(hwnd: isize, to: (i32, i32), cross_desktop: bool) -> String {
    if cross_desktop {
        match (vdm::open(), current_realm()) {
            (Some(v), Some(here)) => {
                if !v.move_window(hwnd, here) {
                    return "the shell refused to move that window".into();
                }
                // let the shell uncloak it before we position/raise — a cross-desktop arrival
                // positioned mid-cloak can land without its frame settling.
                std::thread::sleep(std::time::Duration::from_millis(40));
            }
            _ => return "no desktop bridge (COM unavailable)".into(),
        }
    }
    move_center(hwnd, to.0, to.1, true);
    format!("summoned \u{2192} ({}, {})", to.0, to.1)
}

/// DROP on a monitor: place a carried window centered at the ghost's real point.
#[cfg(windows)]
pub fn place(hwnd: isize, at: (i32, i32), from_realm: bool) -> String {
    if from_realm {
        if let (Some(v), Some(here)) = (vdm::open(), current_realm()) {
            let _ = v.move_window(hwnd, here);
        }
    }
    move_center(hwnd, at.0, at.1, true);
    format!("placed \u{2192} ({}, {})", at.0, at.1)
}

/// DROP on a realm card: move the window to that desktop (it stays there; you stay here).
#[cfg(windows)]
pub fn banish(hwnd: isize, realm: u128) -> String {
    match vdm::open() {
        Some(v) if v.move_window(hwnd, realm) => "sent \u{2192} that realm".into(),
        Some(_) => "the shell refused to move that window".into(),
        None => "no desktop bridge (COM unavailable)".into(),
    }
}

/// DROP in the void: forge a NEW desktop (Win+Ctrl+D — you follow the throw, which is honest:
/// you see it land) and move the window there. Synthesis ⇒ the arm gate applies.
#[cfg(windows)]
pub fn banish_new(hwnd: isize) -> String {
    if !neuron::action::input_armed() {
        return "new realm needs input armed".into();
    }
    send_chord(&[0x5B, 0x11, 0x44]); // Win + Ctrl + D
    std::thread::sleep(std::time::Duration::from_millis(350)); // let the shell settle
    match (vdm::open(), current_realm()) {
        (Some(v), Some(here)) if v.move_window(hwnd, here) => {
            "new realm forged \u{2192} sent".into()
        }
        _ => "realm forged, but the window would not cross".into(),
    }
}

/// UP-FLICK: AUTO-SORT — "it decides", deterministically: the monitor already hosting this
/// window's SIBLINGS (same exe) wins; with no kin, the EMPTIEST monitor (least window coverage).
/// The status line always says which rule fired — the sort is explainable, never spooky.
#[cfg(windows)]
pub fn auto_sort(snap: &Snapshot, hwnd: isize) -> String {
    let me = exe_stem(hwnd);
    // kin: count same-exe windows per monitor (by their center point)
    let mut kin = vec![0usize; snap.monitors.len()];
    let mut cover = vec![0i64; snap.monitors.len()];
    for w in &snap.windows {
        if w.hwnd == hwnd {
            continue;
        }
        let cx = (w.rect.0 + w.rect.2) / 2;
        let cy = (w.rect.1 + w.rect.3) / 2;
        for (i, m) in snap.monitors.iter().enumerate() {
            if cx >= m.0 && cx < m.2 && cy >= m.1 && cy < m.3 {
                if !me.is_empty() && exe_stem(w.hwnd) == me {
                    kin[i] += 1;
                }
                let ox = (w.rect.2.min(m.2) - w.rect.0.max(m.0)).max(0) as i64;
                let oy = (w.rect.3.min(m.3) - w.rect.1.max(m.1)).max(0) as i64;
                cover[i] += ox * oy;
            }
        }
    }
    let (mi, why) = match kin
        .iter()
        .enumerate()
        .filter(|(_, &k)| k > 0)
        .max_by_key(|(_, &k)| k)
    {
        Some((i, _)) => (i, "with its kin"),
        None => (
            cover
                .iter()
                .enumerate()
                .min_by_key(|(i, &c)| {
                    let m = snap.monitors[*i];
                    let area = ((m.2 - m.0) as i64 * (m.3 - m.1) as i64).max(1);
                    // coverage RATIO, so a big monitor isn't "emptier" by sheer size
                    c * 10_000 / area
                })
                .map(|(i, _)| i)
                .unwrap_or(0),
            "the emptiest glass",
        ),
    };
    let m = snap.monitors.get(mi).copied().unwrap_or((
        snap.vx,
        snap.vy,
        snap.vx + snap.vw,
        snap.vy + snap.vh,
    ));
    move_center(hwnd, (m.0 + m.2) / 2, (m.1 + m.3) / 2, true);
    format!("sorted \u{2192} {why}")
}

/// This thread's CURRENT desktop, as a realm key — via a throwaway hidden window (the public
/// shell COM has no "current desktop" getter; a fresh window is born on it, so we ask the window).
#[cfg(windows)]
fn current_realm() -> Option<u128> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WS_POPUP};
    unsafe {
        let cls: Vec<u16> = "Static\0".encode_utf16().collect();
        let hwnd = CreateWindowExW(
            0,
            cls.as_ptr(),
            std::ptr::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return None;
        }
        let id = vdm::open()
            .map(|v| v.desktop_of(hwnd as isize))
            .unwrap_or(0);
        DestroyWindow(hwnd);
        (id != 0).then_some(id)
    }
}

/// Restore-if-minimized, center at (x, y) clamped into that point's monitor work area, raise.
/// A MAXIMIZED window doesn't get dragged around in its maximized frame — it restores, travels,
/// and re-maximizes on the destination monitor (what "bring that here" means for a maximized app).
#[cfg(windows)]
fn move_center(hwnd: isize, x: i32, y: i32, focus: bool) {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsIconic, IsZoomed, SetWindowPos, ShowWindow, HWND_TOP, SWP_NOACTIVATE,
        SWP_NOSIZE, SW_MAXIMIZE, SW_RESTORE,
    };
    unsafe {
        let was_zoomed = IsZoomed(hwnd as _) != 0;
        if IsIconic(hwnd as _) != 0 || was_zoomed {
            ShowWindow(hwnd as _, SW_RESTORE);
        }
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd as _, &mut r) == 0 {
            return;
        }
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let wa = if GetMonitorInfoW(mon, &mut mi) != 0 {
            mi.rcWork
        } else {
            RECT {
                left: x - w / 2,
                top: y - h / 2,
                right: x + w / 2,
                bottom: y + h / 2,
            }
        };
        // center on the point, clamped so the window stays on the glass
        let nx = (x - w / 2).clamp(wa.left, (wa.right - w).max(wa.left));
        let ny = (y - h / 2).clamp(wa.top, (wa.bottom - h).max(wa.top));
        SetWindowPos(
            hwnd as _,
            HWND_TOP,
            nx,
            ny,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        if was_zoomed {
            ShowWindow(hwnd as _, SW_MAXIMIZE); // maximizes on the monitor it now lives on
        }
        if focus {
            force_foreground(hwnd);
        }
    }
}

/// REALLY bring a window to the front. A plain `SetForegroundWindow` from a background process
/// is DENIED by Windows whenever another app (a maximized game, a browser) owns the foreground —
/// the call fails silently and the summoned window lands BEHIND it: exactly the "teleports but
/// doesn't come on top" feel. Three layers, cheapest first:
///   1. the TOPMOST FLIP — z-order needs no focus rights; this raises above every non-topmost
///      window unconditionally, so the summon is VISIBLE even if focus were refused;
///   2. the `AttachThreadInput` handoff — joining the foreground thread's input state makes
///      the foreground ours to give (the route every launcher uses);
///   3. an arm-gated zero-delta input whisper — the last input source is allowed to take
///      foreground; a 0,0 relative mouse move is imperceptible and clicks nothing.
#[cfg(windows)]
pub(crate) fn force_foreground(hwnd: isize) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, SetForegroundWindow,
        SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    };
    unsafe {
        // 1) z-order first: on top even when focus is denied.
        SetWindowPos(
            hwnd as _,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        SetWindowPos(
            hwnd as _,
            HWND_NOTOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        if SetForegroundWindow(hwnd as _) != 0 {
            return;
        }
        // 2) the input-state handoff.
        let fg = GetForegroundWindow();
        let my = GetCurrentThreadId();
        let fg_tid = if fg.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fg, std::ptr::null_mut())
        };
        let t_tid = GetWindowThreadProcessId(hwnd as _, std::ptr::null_mut());
        let att_fg = fg_tid != 0 && fg_tid != my && attach(my, fg_tid, true);
        let att_t = t_tid != 0 && t_tid != my && t_tid != fg_tid && attach(my, t_tid, true);
        SetForegroundWindow(hwnd as _);
        BringWindowToTop(hwnd as _);
        if att_t {
            attach(my, t_tid, false);
        }
        if att_fg {
            attach(my, fg_tid, false);
        }
        // 3) last resort: become the input source (gated like every synthesis).
        if GetForegroundWindow() != hwnd as _ && neuron::action::input_armed() {
            whisper_input();
            SetForegroundWindow(hwnd as _);
        }
    }
}

#[cfg(windows)]
fn attach(a: u32, b: u32, on: bool) -> bool {
    use windows_sys::Win32::System::Threading::AttachThreadInput;
    unsafe { AttachThreadInput(a, b, if on { 1 } else { 0 }) != 0 }
}

/// A zero-delta relative mouse move — real input that moves nothing and clicks nothing, just
/// enough to mark this process as the input source so the foreground handoff is permitted.
#[cfg(windows)]
fn whisper_input() {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_MOVE, MOUSEINPUT,
    };
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    unsafe {
        SendInput(1, &input, std::mem::size_of::<INPUT>() as i32);
    }
}

/// The exe file stem (lowercase) owning a window — the kinship key for auto-sort and the
/// glance matcher.
#[cfg(windows)]
pub(crate) fn exe_stem(hwnd: isize) -> String {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    unsafe {
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd as _, &mut pid);
        if pid == 0 {
            return String::new();
        }
        let proc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if proc.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(proc, 0, buf.as_mut_ptr(), &mut len);
        windows_sys::Win32::Foundation::CloseHandle(proc);
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

/// Synthesize a held chord (all down in order, tap nothing, all up in reverse) — used only by
/// `banish_new` for Win+Ctrl+D. Arm-gated by the caller.
#[cfg(windows)]
fn send_chord(vks: &[u16]) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    };
    let mk = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut seq: Vec<INPUT> = vks.iter().map(|&v| mk(v, false)).collect();
    seq.extend(vks.iter().rev().map(|&v| mk(v, true)));
    unsafe {
        SendInput(
            seq.len() as u32,
            seq.as_ptr(),
            std::mem::size_of::<INPUT>() as i32,
        );
    }
}

// ── CLICK GUARD — swallow L/R clicks while a weave aims (the spectral verbs read them via raw
// input, which sees the device regardless; the app under the pinned cursor must NOT). A transient
// WH_MOUSE_LL hook, active only while armed, never touching the held trigger button itself. ──
#[cfg(windows)]
pub mod click_guard {
    use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
    use std::sync::{Once, OnceLock};
    use std::time::Instant;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, MSG, MSLLHOOKSTRUCT, WH_MOUSE_LL,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
        WM_XBUTTONDOWN, WM_XBUTTONUP,
    };

    /// Process-monotonic millisecond clock (cheap, no Win32 feature needed).
    fn now_ms() -> u32 {
        static START: OnceLock<Instant> = OnceLock::new();
        START.get_or_init(Instant::now).elapsed().as_millis() as u32
    }

    /// SELF-HEAL DEADMAN: ms at the last arm/renew. The hook refuses to swallow if it hasn't been
    /// renewed within [`GUARD_CAP_MS`] — so a STALLED or crashed weave (the thread that armed the
    /// guard never reaching `disarm`) can NEVER leave the mouse dead. A healthy weave renews every
    /// capture tick (the cancel-predicate heartbeat), so during real use the guard stays solid;
    /// the instant renewal stops, clicks flow again within the cap.
    static RENEW: AtomicU32 = AtomicU32::new(0);
    const GUARD_CAP_MS: u32 = 1500;

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static EXEMPT: AtomicI32 = AtomicI32::new(0);
    /// When true, swallow ALL L/R clicks except `EXEMPT` (teleport's spectral-verb mode).
    static BLANKET_LR: AtomicBool = AtomicBool::new(false);
    /// A single button VK to swallow entirely, of any kind (KNOCKBACK's drum trigger). 0 = none.
    static SWALLOW: AtomicI32 = AtomicI32::new(0);
    /// The swallowed button's PHYSICAL state, tracked by the hook itself. A swallowed event
    /// never reaches the system input queue, so `GetAsyncKeyState` goes blind to it — the hook
    /// is the only honest witness left, and the knockback session reads its drum from here.
    static SWALLOW_DOWN: AtomicBool = AtomicBool::new(false);
    static INSTALL: Once = Once::new();

    /// Map a low-level mouse message to a VK (covers L/R/M and both side buttons).
    unsafe fn msg_vk(wparam: u32, lparam: isize) -> i32 {
        match wparam {
            WM_LBUTTONDOWN | WM_LBUTTONUP => 0x01,
            WM_RBUTTONDOWN | WM_RBUTTONUP => 0x02,
            WM_MBUTTONDOWN | WM_MBUTTONUP => 0x04,
            WM_XBUTTONDOWN | WM_XBUTTONUP => {
                // the X button index lives in the high word of mouseData: 1 = XBUTTON1 (VK 0x05).
                let ms = unsafe { &*(lparam as *const MSLLHOOKSTRUCT) };
                if (ms.mouseData >> 16) as u16 == 1 {
                    0x05
                } else {
                    0x06
                }
            }
            _ => 0,
        }
    }

    unsafe extern "system" fn hook(code: i32, wparam: usize, lparam: isize) -> isize {
        if code >= 0 && ACTIVE.load(Ordering::Relaxed) {
            // DEADMAN — TELEPORT's blanket-L/R mode only: if the arming weave hasn't renewed
            // within the cap, it stalled or died, so HEAL the guard and let the click through (the
            // mouse is never dead > cap). KNOCKBACK's single-button swallow (`arm_button`) is NOT
            // renewal-driven — it legitimately stays armed for a whole drum session — so the
            // deadman must never touch it.
            if BLANKET_LR.load(Ordering::Relaxed)
                && now_ms().wrapping_sub(RENEW.load(Ordering::Relaxed)) > GUARD_CAP_MS
            {
                ACTIVE.store(false, Ordering::Relaxed);
                BLANKET_LR.store(false, Ordering::Relaxed);
                return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
            }
            let vk = unsafe { msg_vk(wparam as u32, lparam) };
            if vk != 0 {
                // SWALLOW THE DOWN ONLY, ALWAYS PASS THE UP. A swallowed button-UP never reaches
                // the system, so `GetAsyncKeyState` for that button is left reading "held" FOREVER
                // (until the next physical edge) — which is exactly what froze the weave's
                // `while key_down(trigger)` hold loop after a knockback/teleport session and bricked
                // every cast-key mode. Suppressing only the DOWN still stops the app from seeing the
                // click (apps act on press; an orphan release is ignored), while the OS key-state
                // stays honest so the weave's trigger reads correctly the instant the session ends.
                let is_down = matches!(
                    wparam as u32,
                    WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
                );
                let swallow = SWALLOW.load(Ordering::Relaxed);
                // KNOCKBACK: swallow exactly the drum trigger's DOWN so a tap never leaks to the
                // app — recording its physical edge first (the session reads the drum from here).
                if swallow != 0 && vk == swallow {
                    SWALLOW_DOWN.store(is_down, Ordering::Relaxed);
                    if is_down {
                        return 1;
                    }
                }
                // TELEPORT: swallow the L/R spectral-verb DOWN, except the held trigger.
                else if BLANKET_LR.load(Ordering::Relaxed)
                    && (vk == 0x01 || vk == 0x02)
                    && vk != EXEMPT.load(Ordering::Relaxed)
                    && is_down
                {
                    return 1; // raw input already saw it; the app below never will
                }
            }
        }
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }

    fn install() {
        INSTALL.call_once(|| {
            crate::worker::spawn_detached("neuron-click-guard", || unsafe {
                if SetWindowsHookExW(WH_MOUSE_LL, Some(hook), std::ptr::null_mut(), 0).is_null() {
                    return;
                }
                let mut msg: MSG = std::mem::zeroed();
                while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
            });
        });
    }

    /// Arm the guard (installs the hook thread on first use). `exempt_vk` = the held trigger's
    /// own button, which must keep flowing so its release is seen everywhere consistently. This
    /// is TELEPORT's mode: it swallows the L/R spectral-verb clicks.
    pub fn arm(exempt_vk: i32) {
        EXEMPT.store(exempt_vk, Ordering::Relaxed);
        BLANKET_LR.store(true, Ordering::Relaxed);
        SWALLOW.store(0, Ordering::Relaxed);
        renew();
        install();
        ACTIVE.store(true, Ordering::Relaxed);
    }

    /// Heartbeat the deadman — the live weave calls this each capture tick so the guard stays
    /// solid while the weave is genuinely alive (and self-heals the instant it isn't).
    pub fn renew() {
        RENEW.store(now_ms(), Ordering::Relaxed);
    }

    /// KNOCKBACK's mode: swallow exactly one button (the drum trigger) so tapping it to keep the
    /// beat never leaks a click into the game/app underneath. L/R/M and the side buttons flow
    /// normally. The session still reads the trigger via `GetAsyncKeyState` (physical state,
    /// upstream of this hook), so drumming works while the app sees nothing.
    pub fn arm_button(swallow_vk: i32) {
        SWALLOW.store(swallow_vk, Ordering::Relaxed);
        SWALLOW_DOWN.store(false, Ordering::Relaxed);
        BLANKET_LR.store(false, Ordering::Relaxed);
        EXEMPT.store(0, Ordering::Relaxed);
        renew();
        install();
        ACTIVE.store(true, Ordering::Relaxed);
    }

    /// Is the swallowed button physically held right now? (Only meaningful after
    /// [`arm_button`]; swallowed events never update `GetAsyncKeyState`, so this is the
    /// drum's source of truth.)
    pub fn swallowed_down() -> bool {
        SWALLOW_DOWN.load(Ordering::Relaxed)
    }

    pub fn disarm() {
        ACTIVE.store(false, Ordering::Relaxed);
        SWALLOW.store(0, Ordering::Relaxed);
        SWALLOW_DOWN.store(false, Ordering::Relaxed);
        BLANKET_LR.store(false, Ordering::Relaxed);
    }
}

#[cfg(not(windows))]
pub mod click_guard {
    pub fn arm(_exempt_vk: i32) {}
    pub fn renew() {}
    pub fn arm_button(_swallow_vk: i32) {}
    pub fn swallowed_down() -> bool {
        false
    }
    pub fn disarm() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desk() -> Snapshot {
        // two 1920×1080 monitors side by side, cursor mid-left-monitor.
        Snapshot {
            vx: 0,
            vy: 0,
            vw: 3840,
            vh: 1080,
            monitors: vec![(0, 0, 1920, 1080), (1920, 0, 3840, 1080)],
            windows: vec![
                WinBlob {
                    rect: (100, 100, 900, 700),
                    hwnd: 1,
                    z: 1,
                    other_desktop: false,
                },
                WinBlob {
                    rect: (2000, 50, 3000, 900),
                    hwnd: 2,
                    z: 0,
                    other_desktop: false,
                },
                WinBlob {
                    rect: (150, 150, 800, 650),
                    hwnd: 3,
                    z: 2,
                    other_desktop: false,
                }, // behind hwnd 1
            ],
            realms: vec![Realm {
                guid: 0xDEAD_BEEF,
                windows: vec![WinBlob {
                    rect: (200, 200, 1400, 900),
                    hwnd: 9,
                    z: 0,
                    other_desktop: true,
                }],
            }],
            cursor: (960, 540),
            tethers: Vec::new(),
        }
    }

    /// The projection is uniform (aspect intact) and centered: the virtual-screen center maps
    /// to the canvas center, the long edge spans MAP_EDGE.
    #[test]
    fn projection_is_uniform_and_centered() {
        let d = desk();
        assert_eq!(
            d.project(1920, 540),
            (0.0, 0.0),
            "virtual center = canvas center"
        );
        let (l, _) = d.project(0, 0);
        let (r, _) = d.project(3840, 0);
        assert!((r - l - MAP_EDGE).abs() < 0.001, "long edge spans MAP_EDGE");
        // aspect intact: a square in virtual space stays square on the map
        let a = d.project(0, 0);
        let b = d.project(100, 100);
        assert!(((b.0 - a.0) - (b.1 - a.1)).abs() < 0.001);
    }

    /// Zero drag = the cursor's own spot; drags scale by gain/scale; targets clamp inside the
    /// virtual screen (a wild fling parks at the far edge, never off-desk).
    #[test]
    fn target_math_holds() {
        let d = desk();
        assert_eq!(d.target_of(0.0, 0.0), (960, 540), "no drag = stay");
        let s = d.scale() as f64;
        let (x, _) = d.target_of(100.0, 0.0);
        let expect = 960.0 + 100.0 * GHOST_GAIN as f64 / s;
        assert!((x as f64 - expect).abs() < 1.0, "drag scales by gain/scale");
        assert_eq!(d.target_of(1e9, 0.0).0, 3838, "clamped to the desk's edge");
        assert_eq!(d.target_of(-1e9, -1e9), (1, 1));
    }

    /// Landing on overlapping windows picks the FRONTMOST (z), not insertion order; landing on
    /// raw desktop picks nothing — and the DEPTH DIAL descends the stack from there, clamping
    /// at the bottom (over-scrolling rests on the deepest window, never on nothing).
    #[test]
    fn window_hit_respects_z_and_the_dial_descends() {
        let d = desk();
        assert_eq!(
            d.window_at(500, 400).map(|w| w.hwnd),
            Some(1),
            "front of the overlap"
        );
        assert_eq!(d.window_at(2500, 500).map(|w| w.hwnd), Some(2));
        assert!(
            d.window_at(1500, 1000).is_none(),
            "desktop = no focus target"
        );
        // the dial: depth 1 under the overlap = the window BEHIND (even other-desktop ones)
        assert_eq!(d.window_at_depth(500, 400, 0).map(|w| w.hwnd), Some(1));
        assert_eq!(
            d.window_at_depth(500, 400, 1).map(|w| w.hwnd),
            Some(3),
            "one notch down"
        );
        assert_eq!(
            d.window_at_depth(500, 400, 99).map(|w| w.hwnd),
            Some(3),
            "clamps at bottom"
        );
        assert!(
            d.window_at_depth(1500, 1000, 3).is_none(),
            "no stack on bare desktop"
        );
        assert_eq!(d.stack_at(500, 400).len(), 2);
    }

    /// The map payload mirrors the snapshot: every monitor + window present, recency lights the
    /// frontmost blob hottest, other desktops ride as REALM cards (their own little worlds,
    /// never mixed into the desk), and the dial's hot indices ride along.
    #[test]
    fn map_mode_carries_the_desk() {
        let d = desk();
        let crate::overlay::WeaveMode::Map {
            monitors,
            windows,
            realms,
            cursor,
            hot,
            hot_realm,
            carried,
            ..
        } = d.map_mode_hot(2, (0, 0))
        else {
            panic!("map_mode must build WeaveMode::Map");
        };
        assert!(carried.is_none(), "nothing rides the ghost unless grabbed");
        assert_eq!(monitors.len(), 2);
        assert_eq!(windows.len(), 3, "the desk shows ONLY the current desktop");
        assert_eq!(
            realms.len(),
            1,
            "the other desktop is a realm card, not desk noise"
        );
        assert_eq!(realms[0].1.len(), 1);
        assert_eq!(hot, 2);
        assert_eq!(hot_realm, (0, 0));
        let front = windows.iter().map(|(_, b)| *b).fold(0.0f32, f32::max);
        let z0_brightness = windows[1].1; // hwnd 2 is z 0
        assert!(
            (z0_brightness - front).abs() < 0.001,
            "frontmost is hottest"
        );
        // a SPECTRAL GRAB rides the ghost: same projected size as the blob, recentered on it.
        let g = (40.0f32, -20.0f32);
        let crate::overlay::WeaveMode::Map {
            carried: Some(c),
            windows: w2,
            ..
        } = d.map_mode_carrying(-1, (-1, -1), Some(((100, 100, 900, 700), g)))
        else {
            panic!("carrying must ride the payload");
        };
        let blob = w2[0].0; // hwnd 1's projected rect
        assert!(
            ((c[2] - c[0]) - (blob[2] - blob[0])).abs() < 0.001,
            "carried keeps its size"
        );
        assert!(
            ((c[0] + c[2]) / 2.0 - g.0).abs() < 0.001,
            "carried centers on the ghost"
        );
        assert!(((c[1] + c[3]) / 2.0 - g.1).abs() < 0.001);
        assert!((cursor.0 - d.project(960, 540).0).abs() < 0.001);
        assert_eq!(d.index_of(3), Some(2));
    }

    /// Realm geometry: the card sits under the projected desk, its blob shrinks the desk's
    /// coordinates faithfully, and realm_hit resolves blobs (and empty card area) correctly.
    #[test]
    fn realm_cards_lay_out_and_hit() {
        let d = desk();
        let card = d.realm_card(0);
        let desk_bottom = d.project(d.vx, d.vy + d.vh).1;
        assert!(card[1] > desk_bottom, "the strip sits BELOW the desk");
        assert!((card[2] - card[0] - REALM_W).abs() < 0.001);
        let blob = d.realm_blob(0, 0);
        assert!(
            blob[0] >= card[0] && blob[2] <= card[2],
            "blobs live inside their card"
        );
        assert!(blob[1] >= card[1] && blob[3] <= card[3]);
        // hitting the blob names the window; hitting the card's empty corner still aims the
        // realm (frontmost fallback); missing the strip entirely is None.
        let cx = (blob[0] + blob[2]) / 2.0;
        let cy = (blob[1] + blob[3]) / 2.0;
        assert_eq!(d.realm_hit(cx, cy), Some((0, Some(0))));
        assert_eq!(d.realm_hit(card[0] + 1.0, card[1] + 1.0), Some((0, None)));
        assert_eq!(d.realm_hit(0.0, -200.0), None);
    }
}
