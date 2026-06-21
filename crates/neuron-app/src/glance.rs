//! GLANCE — live peeks at the windows you care about, bound to anything.
//!
//! Cast it and every window matching the target (title or exe substring) blooms as a TILE in a
//! geometric collage near your cursor: real surfaces via DWM thumbnails (video plays, logs
//! scroll), focus never moves. One match = one PIP. Three terminals running builds = a mosaic.
//!
//! The collage is MAGNETIC: tiles sit attached in an adaptive grid; the FIRST tile is the
//! cluster's handle (drag it, everything docked rides along); any other tile dragged away
//! becomes a free float you can park in a corner — drop it back near the cluster and it snaps
//! home, and the grid closes ranks adaptively either way. The cluster anchor is REMEMBERED
//! (in-session and across restarts via glance.toml); float spots are remembered per window
//! for the session. Cast again and the whole constellation winks out.
//!
//! Honesty: every tile is a pure VIEW (the one thing Windows lets every process do to every
//! window without touching it) — nothing here focuses, clicks, or moves your real windows.

#[cfg(windows)]
pub use imp::{count, matches, suggestions, toggle};

#[cfg(not(windows))]
pub fn count(_needle: &str) -> usize {
    0
}

#[cfg(not(windows))]
pub fn toggle(_target: &str) -> String {
    "glance: windows-only".into()
}

#[cfg(not(windows))]
pub fn suggestions() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
mod imp {

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::OnceLock;

    /// A matched source window: (hwnd, title, screen rect).
    type Found = (isize, String, (i32, i32, i32, i32));

    enum GCmd {
        Show(Vec<Found>, (i32, i32)),
        Hide,
        /// Step back out of a possessed portal (the glance key, recast while inside).
        Return,
    }

    static SHOWING: AtomicBool = AtomicBool::new(false);
    /// True while the user has stepped THROUGH a portal (double-click possession) — the glance key
    /// becomes "return from the portal" instead of toggling the constellation away.
    static POSSESSED: AtomicBool = AtomicBool::new(false);
    /// A double-click landed on a tile: (tile hwnd, client x, client y). Written by the tile's proc,
    /// drained by the engine loop (which owns the mapping + the warp).
    static STEP_IN: std::sync::Mutex<Option<(isize, i32, i32)>> = std::sync::Mutex::new(None);

    /// Toggle the glance constellation for `target` (title/exe substring, case-insensitive).
    /// CONTEXT-AWARE: while possessed (you stepped through a portal), the same cast means
    /// "bring me home" — the key that opened the eye is the key that closes the loop.
    pub fn toggle(target: &str) -> String {
        if POSSESSED.load(Ordering::SeqCst) {
            let _ = engine().send(GCmd::Return);
            return "\u{21a9} returned from the portal".into();
        }
        if SHOWING.swap(false, Ordering::SeqCst) {
            let _ = engine().send(GCmd::Hide);
            return "glance closed".into();
        }
        let t = target.trim().to_lowercase();
        if t.is_empty() {
            return "glance needs a window name (title or exe)".into();
        }
        let mut found = find_all(&t);
        found.truncate(9); // a 3×3 wall of glass is the honest ceiling for readable tiles
        if found.is_empty() {
            return format!("no window matching '{t}'");
        }
        let n = found.len();
        let first = found[0].1.clone();
        let mut cur = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cur);
        }
        let _ = engine().send(GCmd::Show(found, (cur.x, cur.y)));
        SHOWING.store(true, Ordering::SeqCst);
        if n == 1 {
            format!("glance \u{2192} {first} \u{00b7} drag it anywhere \u{2014} it remembers")
        } else {
            format!(
            "glance \u{2192} {n} \u{00d7} '{t}' \u{00b7} first tile moves the cluster \u{00b7} drag others away \u{00b7} drop near = snap home"
        )
        }
    }

    /// Glance-target SUGGESTIONS for the editor's param picker — what's open right now, from
    /// SPECIFIC (short window titles, usable verbatim) to BROAD (exe stems, robust across title
    /// churn). Deduped, capped — chips, not a task manager.
    pub fn suggestions() -> Vec<String> {
        let wins = find_all("");
        let mut out: Vec<String> = Vec::new();
        let mut push = |s: String| {
            let key = s.to_lowercase();
            if !s.is_empty() && !out.iter().any(|e| e.to_lowercase() == key) {
                out.push(s);
            }
        };
        // specific: titles short enough to BE the substring you'd type
        for (_, title, _) in &wins {
            if title.chars().count() <= 30 {
                push(title.clone());
            }
        }
        // broad: the owning exe's stem (survives title changes)
        for (hwnd, _, _) in &wins {
            push(crate::teleport::exe_stem(*hwnd));
        }
        out.truncate(12);
        out
    }

    /// How many visible windows match `needle` (title or exe substring) right now — the live count
    /// the glance wedge shows ("×3"). Empty needle = 0.
    pub fn count(needle: &str) -> usize {
        let t = needle.trim().to_lowercase();
        if t.is_empty() {
            return 0;
        }
        find_all(&t).len()
    }

    /// Every window matching `needle` as `(hwnd, title)`, frontmost first — what the summon wedge FANS
    /// out as its second tier when more than one of an app is open (each option summons that exact one).
    pub fn matches(needle: &str) -> Vec<(isize, String)> {
        let t = needle.trim().to_lowercase();
        if t.is_empty() {
            return Vec::new();
        }
        find_all(&t)
            .into_iter()
            .map(|(h, title, _)| (h, title))
            .collect()
    }

    fn engine() -> &'static Sender<GCmd> {
        static TX: OnceLock<Sender<GCmd>> = OnceLock::new();
        TX.get_or_init(|| {
            let (tx, rx) = channel::<GCmd>();
            std::thread::Builder::new()
                .name("neuron-glance".into())
                .spawn(move || engine_thread(rx))
                .ok();
            tx
        })
    }

    /// Every visible, titled, non-self window whose title OR exe stem contains the needle —
    /// frontmost first (EnumWindows walks z-order), capped at 9 (a 3×3 wall of glass is the
    /// honest ceiling for readable tiles).
    fn find_all(needle: &str) -> Vec<Found> {
        use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
            IsWindowVisible,
        };
        struct Search {
            needle: String,
            me: u32,
            hits: Vec<Found>,
        }
        unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> i32 {
            unsafe {
                let s = &mut *(lp as *mut Search);
                if s.hits.len() >= 24 {
                    return 0;
                }
                if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                    return 1;
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, &mut pid);
                if pid == s.me {
                    return 1; // never glance at our own surfaces
                }
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
                if n <= 0 {
                    return 1;
                }
                let title = String::from_utf16_lossy(&buf[..n as usize]);
                let hay = title.to_lowercase();
                let exe = crate::teleport::exe_stem(hwnd as isize);
                if hay.contains(&s.needle) || (!exe.is_empty() && exe.contains(&s.needle)) {
                    let mut r: RECT = std::mem::zeroed();
                    if GetWindowRect(hwnd, &mut r) != 0 && r.right - r.left > 40 {
                        s.hits
                            .push((hwnd as isize, title, (r.left, r.top, r.right, r.bottom)));
                    }
                }
                1
            }
        }
        unsafe {
            let mut s = Search {
                needle: needle.to_string(),
                me: windows_sys::Win32::System::Threading::GetCurrentProcessId(),
                hits: Vec::new(),
            };
            EnumWindows(Some(cb), &mut s as *mut _ as isize);
            s.hits
        }
    }

    // ── the cluster anchor's memory (glance.toml — parked is parked, even tomorrow) ──

    static ANCHOR: AtomicU64 = AtomicU64::new(u64::MAX);

    fn pack(x: i32, y: i32) -> u64 {
        ((x as u32 as u64) << 32) | (y as u32 as u64)
    }

    fn unpack(v: u64) -> Option<(i32, i32)> {
        (v != u64::MAX).then_some(((v >> 32) as u32 as i32, v as u32 as i32))
    }

    fn anchor_load() {
        if let Ok(s) = std::fs::read_to_string("glance.toml") {
            let get = |k: &str| {
                s.lines()
                    .find(|l| l.trim_start().starts_with(k))
                    .and_then(|l| l.split('=').nth(1))
                    .and_then(|v| v.trim().parse::<i32>().ok())
            };
            if let (Some(x), Some(y)) = (get("x"), get("y")) {
                ANCHOR.store(pack(x, y), Ordering::SeqCst);
            }
        }
    }

    fn anchor_save() {
        if let Some((x, y)) = unpack(ANCHOR.load(Ordering::SeqCst)) {
            let _ = std::fs::write(
                "glance.toml",
                format!("# where the glance cluster is parked\nx = {x}\ny = {y}\n"),
            );
        }
    }

    // ── the engine: tile windows, thumbnails, the magnetic grid ─────────────────────────────────────

    /// Snap distance: a float dropped with its center this close to the cluster's box comes home.
    const SNAP: i32 = 72;
    /// Gap between docked tiles.
    const GAP: i32 = 8;
    /// The FRAME — every tile wears a paper-thin hairline border (1px, just OUTSIDE the glass so it
    /// never covers content) with a corner BRACKET at its top-left: the one handle. The bracket's
    /// thickness says how many windows this glance holds; click it to collapse the tile to just the
    /// bracket, click again to bring it back, drag it to move the tile (or to park the collapsed
    /// bracket anywhere). Padding the frame window leaves room for the thickest bracket.
    const PAD: i32 = 7;
    /// Bracket arm length.
    const ARM: i32 = 22;
    /// The clickable corner box at the frame's top-left (bracket + a little grace).
    const CORNER: i32 = 26;
    /// How far the cursor lights a collapsed bracket up (ghost far, solid near).
    const PROX: f32 = 150.0;

    /// Bracket arm thickness: "slightly thicker" with every window in the glance, capped.
    fn bracket_t(count: usize) -> i32 {
        (1 + count as i32).clamp(2, 6)
    }

    struct Tile {
        src: isize,
        win: windows_sys::Win32::Foundation::HWND,
        thumb: isize,
        size: (i32, i32),
        docked: bool,
        /// where WE last placed it (drag detection = reality disagreeing with this)
        expected: (i32, i32),
        dragging: bool,
        /// a live native resize is in progress (size disagreeing with `size`)
        resizing: bool,
        // ── the FRAME: hairline border + corner-bracket handle (one per tile) ──
        frame: windows_sys::Win32::Foundation::HWND,
        /// the frame's kept memory DC + DIB (re-blended cheaply for proximity fades)
        fdc: windows_sys::Win32::Graphics::Gdi::HDC,
        fbmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
        fsize: (i32, i32),
        /// what the DIB currently shows: (collapsed, hover, possessed, count, fsize)
        painted: (bool, bool, bool, usize, (i32, i32)),
        /// THE CUT: show only this source-window-relative region (l,t,r,b) — DWM `rcSource`.
        /// Carved by a right-drag (the blade), reset by a plain right-click. None = the whole window.
        crop: Option<(i32, i32, i32, i32)>,
        /// a live right-drag selection: (anchor, current), both in SCREEN coords
        sel: Option<((i32, i32), (i32, i32))>,
        rpress: bool,
        rmoved: bool,
        /// you're THROUGH this portal right now (the frame burns as the possession ring)
        possessed: bool,
        /// last constant alpha blended (avoid redundant UpdateLayeredWindow calls)
        alpha: i32,
        /// collapsed to just the bracket
        collapsed: bool,
        /// the bracket's top-left while collapsed
        mark: (i32, i32),
        /// the user parked the collapsed bracket somewhere (vs folding to the tile's corner)
        parked: bool,
        /// cursor over the corner box (brightens the bracket)
        hover: bool,
        /// an active press on the corner, and whether it has moved enough to be a drag
        press: bool,
        moved: bool,
        /// cursor→corner offset captured at press, for smooth dragging
        grab: (i32, i32),
        /// monotonic "last raised to the top" stamp — when brackets OVERLAP (stacked tiles), the most
        /// recently raised tile owns the corner so you grab the one you see on top, never whatever
        /// happens to sit first in the vec. Bumped on grab + step-in (the gestures that raise a tile).
        raised: u64,
    }

    /// Monotonic raise clock for [`Tile::raised`] — newest-on-top among overlapping brackets.
    fn next_raise() -> u64 {
        static R: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        R.fetch_add(1, Ordering::Relaxed)
    }

    /// Per-session memory of how the user arranged a given SOURCE window, keyed by hwnd. Survives
    /// hide/show within a session (this engine thread is immortal); resets on restart, as expected.
    /// Entries are PURGED when their source window dies — Windows recycles hwnds, and a recycled
    /// handle must never inherit a stranger's size/collapse state.
    #[derive(Default)]
    struct Memory {
        floats: std::collections::HashMap<isize, (i32, i32)>, // parked float position
        sizes: std::collections::HashMap<isize, (i32, i32)>,  // user-resized dimensions
        collapsed: std::collections::HashMap<isize, bool>,    // folded to its bracket
        marks: std::collections::HashMap<isize, (i32, i32)>,  // a parked bracket position
        crops: std::collections::HashMap<isize, (i32, i32, i32, i32)>, // a carved source region
    }

    impl Memory {
        fn purge(&mut self, src: isize) {
            self.floats.remove(&src);
            self.sizes.remove(&src);
            self.collapsed.remove(&src);
            self.marks.remove(&src);
            self.crops.remove(&src);
        }
    }

    fn engine_thread(rx: Receiver<GCmd>) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetCursorPos, GetWindowRect, PeekMessageW, ShowWindow,
            TranslateMessage, MSG, PM_REMOVE, SW_HIDE, SW_SHOWNOACTIVATE,
        };
        unsafe {
            anchor_load();
            register_tile_class();
            register_frame_class();
            let mut tiles: Vec<Tile> = Vec::new();
            let mut mem = Memory::default();
            let mut anchor: (i32, i32) = (0, 0);
            let mut prev_lmb = false;
            let mut prev_rmb = false;
            let mut prev_esc = false;
            // an active POSSESSION: (source hwnd, home window, home cursor, when entered)
            let mut poss: Option<(isize, isize, (i32, i32), std::time::Instant)> = None;
            loop {
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        GCmd::Hide => {
                            for t in tiles.drain(..) {
                                destroy_tile(&t);
                            }
                        }
                        GCmd::Return => snap_home(&mut poss, &mut tiles, true),
                        GCmd::Show(found, cursor) => {
                            for t in tiles.drain(..) {
                                destroy_tile(&t);
                            }
                            anchor = unpack(ANCHOR.load(Ordering::SeqCst))
                                .unwrap_or((cursor.0 + 24, cursor.1 + 24));
                            let cell = cell_for(found.len());
                            for (src, _, rect) in &found {
                                // the tile's aspect follows what it SHOWS: a remembered crop's shape,
                                // else the whole window's.
                                let crop = mem.crops.get(src).copied();
                                let aspect = match crop {
                                    Some((l, tp, r, b)) => {
                                        (r - l).max(1) as f32 / (b - tp).max(1) as f32
                                    }
                                    None => {
                                        (rect.2 - rect.0).max(1) as f32
                                            / (rect.3 - rect.1).max(1) as f32
                                    }
                                };
                                // a remembered custom size wins — RE-FIT to the current aspect
                                // (area-preserving) so nothing ever renders stretched; else
                                // aspect-fit into the cell.
                                let size = match mem.sizes.get(src) {
                                    Some(&s) => refit(s, aspect),
                                    None => refit(fit(*rect, cell), aspect),
                                };
                                let docked = !mem.floats.contains_key(src);
                                if let Some(mut t) = spawn_tile(*src, size, docked, aspect) {
                                    t.collapsed = mem.collapsed.get(src).copied().unwrap_or(false);
                                    if let Some(&mp) = mem.marks.get(src) {
                                        t.mark = clamp_to_screen(mp, (ARM + 2, ARM + 2));
                                        t.parked = true;
                                    }
                                    if crop.is_some() {
                                        t.crop = crop;
                                        retarget_thumb(&t, t.size); // aim the glass at the carved region
                                    }
                                    tiles.push(t);
                                }
                            }
                            layout(&mut tiles, anchor);
                            // floats reappear where the session last saw them, clamped on-screen
                            for t in tiles.iter_mut().filter(|t| !t.docked) {
                                if let Some(&fp) = mem.floats.get(&t.src) {
                                    let p = clamp_to_screen(fp, t.size);
                                    place(t, p.0, p.1);
                                }
                            }
                            // apply remembered collapse state (the bracket folds to the tile's
                            // corner unless the user parked it somewhere this session)
                            for t in tiles.iter_mut() {
                                if t.collapsed {
                                    ShowWindow(t.win, SW_HIDE);
                                    if !t.parked {
                                        t.mark = (t.expected.0 - PAD, t.expected.1 - PAD);
                                    }
                                }
                            }
                        }
                    }
                }
                // pump every tile window
                for t in &tiles {
                    let mut msg: MSG = std::mem::zeroed();
                    while PeekMessageW(&mut msg, t.win, 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
                let lmb = neuron::glyph::key_down(0x01);
                let lmb_edge = lmb && !prev_lmb;
                let rmb = neuron::glyph::key_down(0x02);
                let rmb_edge = rmb && !prev_rmb;
                let esc = neuron::glyph::key_down(0x1B);
                let mut cur = POINT { x: 0, y: 0 };
                GetCursorPos(&mut cur);
                let cursor = (cur.x, cur.y);

                // ── STEP IN (a double-click on the glass): possess the source through the portal ──
                if let Some((win, cx, cy)) = STEP_IN
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    // a fresh step while already inside re-aims; HOME stays the original home —
                    // however deep you wander, one return brings you all the way back.
                    let prior_home = poss.as_ref().map(|p| (p.1, p.2));
                    if let Some(t) = tiles
                        .iter_mut()
                        .find(|t| t.win as isize == win && !t.collapsed)
                    {
                        if let Some((src, home, curp)) = step_in(t, cx, cy) {
                            let (home, curp) = prior_home.unwrap_or((home, curp));
                            poss = Some((src, home, curp, std::time::Instant::now()));
                        }
                    }
                }
                // ── possession upkeep: ESC = snap home; your OWN focus change = dissolve quietly
                // (never fight a deliberate alt-tab); a dead source = snap home ──
                if let Some((src, _, _, since)) = poss {
                    use windows_sys::Win32::UI::WindowsAndMessaging::{
                        GetAncestor, GetForegroundWindow, IsWindow, GA_ROOT,
                    };
                    if esc && !prev_esc {
                        snap_home(&mut poss, &mut tiles, true);
                    } else if IsWindow(src as _) == 0 {
                        snap_home(&mut poss, &mut tiles, true); // the floor vanished — come home
                    } else if since.elapsed() > std::time::Duration::from_millis(600)
                        && GetAncestor(GetForegroundWindow(), GA_ROOT) as isize != src
                    {
                        snap_home(&mut poss, &mut tiles, false);
                    }
                }

                let mut relayout = false;
                let mut cluster_shift: Option<(i32, i32)> = None;
                let primary = tiles.iter().position(|t| t.docked && !t.collapsed);
                let any_busy = tiles.iter().any(|t| t.press);
                // ── resolve the ONE bracket a fresh press grabs. Brackets only collide when tiles
                // overlap (a stacked pile); first-in-vec would then always grab the cluster handle, so
                // overlapping windows fused and couldn't be peeled apart. Among every corner under the
                // cursor, grab the most-recently-RAISED tile (the one you see on top), tie-broken to a
                // non-primary so a press on a pile peels the top window off instead of moving the lot.
                let corner_of = |t: &Tile| {
                    if t.collapsed {
                        t.mark
                    } else {
                        (t.expected.0 - PAD, t.expected.1 - PAD)
                    }
                };
                let grab_target: Option<usize> =
                    if lmb_edge && !any_busy && !tiles.iter().any(|t| t.dragging || t.resizing) {
                        (0..tiles.len())
                            .filter(|&i| {
                                let c = corner_of(&tiles[i]);
                                cursor.0 >= c.0
                                    && cursor.0 < c.0 + CORNER
                                    && cursor.1 >= c.1
                                    && cursor.1 < c.1 + CORNER
                            })
                            .max_by_key(|&i| (tiles[i].raised, Some(i) != primary, i))
                    } else {
                        None
                    };
                for i in 0..tiles.len() {
                    // ── the corner bracket: click = collapse/expand, drag = move (poll-resolved).
                    // Expanded, the bracket rides the tile's top-left and dragging it drags the TILE
                    // (release resolves through the same magnetism as an interior drag); collapsed,
                    // the bracket is all that's left and dragging parks it anywhere. ──
                    let cpos = if tiles[i].collapsed {
                        tiles[i].mark
                    } else {
                        (tiles[i].expected.0 - PAD, tiles[i].expected.1 - PAD)
                    };
                    let over = cursor.0 >= cpos.0
                        && cursor.0 < cpos.0 + CORNER
                        && cursor.1 >= cpos.1
                        && cursor.1 < cpos.1 + CORNER;
                    tiles[i].hover = over; // sync_frame repaints on a hover change
                    if Some(i) == grab_target {
                        tiles[i].press = true;
                        tiles[i].moved = false;
                        tiles[i].grab = (cursor.0 - cpos.0, cursor.1 - cpos.1);
                        // own the top of the stack: bracket priority + the real window z-order both
                        // follow the grab, so a peeled tile lifts cleanly off the pile under it.
                        tiles[i].raised = next_raise();
                        use windows_sys::Win32::UI::WindowsAndMessaging::{
                            SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
                        };
                        SetWindowPos(
                            tiles[i].win,
                            HWND_TOPMOST,
                            0,
                            0,
                            0,
                            0,
                            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                        );
                        if !tiles[i].frame.is_null() {
                            SetWindowPos(
                                tiles[i].frame,
                                HWND_TOPMOST,
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            );
                        }
                    }
                    if tiles[i].press {
                        if lmb {
                            let want = (cursor.0 - tiles[i].grab.0, cursor.1 - tiles[i].grab.1);
                            if (want.0 - cpos.0).abs() + (want.1 - cpos.1).abs() > 4 {
                                tiles[i].moved = true;
                            }
                            if tiles[i].moved {
                                if tiles[i].collapsed {
                                    let p = clamp_to_screen(want, (ARM + 2, ARM + 2));
                                    tiles[i].mark = p;
                                    tiles[i].parked = true;
                                    mem.marks.insert(tiles[i].src, p);
                                } else {
                                    // drag the TILE by its corner: move it live; the engine's
                                    // reality-vs-expected logic resolves dock/float on release.
                                    use windows_sys::Win32::UI::WindowsAndMessaging::{
                                        SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOSIZE,
                                    };
                                    SetWindowPos(
                                        tiles[i].win,
                                        HWND_TOPMOST,
                                        want.0 + PAD,
                                        want.1 + PAD,
                                        0,
                                        0,
                                        SWP_NOACTIVATE | SWP_NOSIZE,
                                    );
                                }
                            }
                        } else {
                            // released: a still press = a click = toggle collapse
                            if !tiles[i].moved {
                                let now = !tiles[i].collapsed;
                                tiles[i].collapsed = now;
                                mem.collapsed.insert(tiles[i].src, now);
                                if now {
                                    ShowWindow(tiles[i].win, SW_HIDE);
                                    // fold to the tile's corner unless parked this session
                                    if !tiles[i].parked {
                                        tiles[i].mark =
                                            (tiles[i].expected.0 - PAD, tiles[i].expected.1 - PAD);
                                    }
                                } else {
                                    ShowWindow(tiles[i].win, SW_SHOWNOACTIVATE);
                                }
                                relayout = true; // the grid closes ranks / re-opens a spot
                            }
                            tiles[i].press = false;
                        }
                    }
                    // ── the BLADE: right-drag carves a crop (DWM renders just that region, live and
                    // magnified); a plain right-click heals the cut back to the whole window ──
                    if !tiles[i].collapsed {
                        let within = cursor.0 >= tiles[i].expected.0
                            && cursor.0 < tiles[i].expected.0 + tiles[i].size.0
                            && cursor.1 >= tiles[i].expected.1
                            && cursor.1 < tiles[i].expected.1 + tiles[i].size.1;
                        if rmb_edge && within && !over {
                            tiles[i].rpress = true;
                            tiles[i].rmoved = false;
                            tiles[i].sel = Some((cursor, cursor));
                        }
                        if tiles[i].rpress {
                            if rmb {
                                if let Some((a, _)) = tiles[i].sel {
                                    if (cursor.0 - a.0).abs() + (cursor.1 - a.1).abs() > 6 {
                                        tiles[i].rmoved = true;
                                    }
                                    tiles[i].sel = Some((a, cursor));
                                }
                            } else {
                                // release: a drag commits the cut; a still click heals it
                                if tiles[i].rmoved {
                                    commit_crop(&mut tiles[i], &mut mem);
                                    relayout |= tiles[i].docked;
                                } else if tiles[i].crop.is_some() {
                                    heal_crop(&mut tiles[i], &mut mem);
                                    relayout |= tiles[i].docked;
                                }
                                tiles[i].rpress = false;
                                tiles[i].rmoved = false;
                                tiles[i].sel = None;
                            }
                        }
                    }
                    // ── tiles: notice native resize + drag (reality vs expected), resolve on release ──
                    if !tiles[i].collapsed {
                        let mut r = windows_sys::Win32::Foundation::RECT {
                            left: 0,
                            top: 0,
                            right: 0,
                            bottom: 0,
                        };
                        if GetWindowRect(tiles[i].win, &mut r) != 0 {
                            let pos = (r.left, r.top);
                            let dim = ((r.right - r.left).max(1), (r.bottom - r.top).max(1));
                            if dim != tiles[i].size {
                                // a live native resize: scale the thumbnail with it, track origin drift
                                retarget_thumb(&tiles[i], dim);
                                tiles[i].size = dim;
                                tiles[i].expected = pos;
                                tiles[i].resizing = true;
                                mem.sizes.insert(tiles[i].src, dim);
                            } else if pos != tiles[i].expected {
                                tiles[i].dragging = true;
                            }
                            if tiles[i].resizing && !lmb {
                                tiles[i].resizing = false;
                                if tiles[i].docked {
                                    relayout = true; // the grid re-flows around the new size
                                }
                            }
                            if tiles[i].dragging && !lmb {
                                tiles[i].dragging = false;
                                let delta =
                                    (pos.0 - tiles[i].expected.0, pos.1 - tiles[i].expected.1);
                                // a tile dropped sitting ON TOP of another docked tile is being PEELED
                                // out of a pile, not docked — float it so overlapping windows separate
                                // cleanly instead of snapping back fused. (A tidy grid never overlaps,
                                // so a normal drop-near-the-cluster still re-docks as before.)
                                let piled = piled_on_docked(&tiles, i, pos);
                                if tiles[i].docked && Some(i) == primary {
                                    cluster_shift = Some(delta);
                                } else if near_cluster(&tiles, i, pos) && !piled {
                                    tiles[i].docked = true;
                                    mem.floats.remove(&tiles[i].src);
                                    relayout = true;
                                } else {
                                    if tiles[i].docked {
                                        relayout = true;
                                    }
                                    tiles[i].docked = false;
                                    tiles[i].expected = pos;
                                    mem.floats.insert(tiles[i].src, pos);
                                }
                            }
                        }
                    }
                }
                if let Some(d) = cluster_shift {
                    anchor = (anchor.0 + d.0, anchor.1 + d.1);
                    ANCHOR.store(pack(anchor.0, anchor.1), Ordering::SeqCst);
                    anchor_save();
                    relayout = true;
                }
                if relayout {
                    layout(&mut tiles, anchor);
                }
                // ── keep every frame seated: position/size follow the tile (or the parked mark),
                // hover/count/collapse changes repaint, proximity fades the collapsed brackets ──
                let count = tiles.len();
                for t in tiles.iter_mut() {
                    sync_frame(t, count, cursor);
                }
                // sources that closed take their tiles with them — and their session memory: Windows
                // recycles hwnds, and a recycled handle must never inherit a stranger's state.
                let mut died = false;
                tiles.retain(|t| {
                    let alive =
                        windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(t.src as _) != 0;
                    if !alive {
                        mem.purge(t.src);
                        destroy_tile(t);
                        died = true;
                    }
                    alive
                });
                if died {
                    layout(&mut tiles, anchor);
                    // the bracket got thicker/thinner with the count — repaint via sync next tick
                    if tiles.is_empty() {
                        SHOWING.store(false, Ordering::SeqCst);
                    }
                }
                prev_lmb = lmb;
                prev_rmb = rmb;
                prev_esc = esc;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
        }
    }

    /// The adaptive cell budget: fewer tiles = bigger glass.
    fn cell_for(n: usize) -> (i32, i32) {
        match n {
            1 => (460, 320),
            2..=4 => (340, 240),
            _ => (272, 190),
        }
    }

    /// Aspect-fit a source window into a cell.
    fn fit(rect: (i32, i32, i32, i32), cell: (i32, i32)) -> (i32, i32) {
        let (sw, sh) = (
            (rect.2 - rect.0).max(1) as f32,
            (rect.3 - rect.1).max(1) as f32,
        );
        let k = (cell.0 as f32 / sw).min(cell.1 as f32 / sh).min(1.0);
        (((sw * k) as i32).max(120), ((sh * k) as i32).max(80))
    }

    /// Lay the DOCKED tiles out as a near-square mosaic at the anchor, clamped fully on-glass.
    /// Row-major in match order; each row is as tall as its tallest tile.
    fn layout(tiles: &mut [Tile], anchor: (i32, i32)) {
        let docked: Vec<usize> = (0..tiles.len())
            .filter(|&i| tiles[i].docked && !tiles[i].collapsed)
            .collect();
        let n = docked.len();
        if n == 0 {
            return;
        }
        let cols = (n as f32).sqrt().ceil() as usize;
        // measure the mosaic
        let mut total_w = 0i32;
        let mut total_h = 0i32;
        let mut rows: Vec<(Vec<usize>, i32)> = Vec::new(); // (tile idxs, row height)
        for chunk in docked.chunks(cols) {
            let rw: i32 = chunk.iter().map(|&i| tiles[i].size.0).sum::<i32>()
                + GAP * (chunk.len() as i32 - 1);
            let rh: i32 = chunk.iter().map(|&i| tiles[i].size.1).max().unwrap_or(0);
            total_w = total_w.max(rw);
            total_h += rh + if rows.is_empty() { 0 } else { GAP };
            rows.push((chunk.to_vec(), rh));
        }
        // clamp the whole constellation into the anchor's monitor work area
        let (wl, wt, wr, wb) =
            crate::teleport::work_area_of((anchor.0 + total_w / 2, anchor.1 + total_h / 2));
        let ax = anchor.0.clamp(wl, (wr - total_w).max(wl));
        let ay = anchor.1.clamp(wt, (wb - total_h).max(wt));
        let mut y = ay;
        for (row, rh) in rows {
            let mut x = ax;
            for i in row {
                place(&mut tiles[i], x, y);
                x += tiles[i].size.0 + GAP;
            }
            y += rh + GAP;
        }
    }

    /// Is tile `me`, dropped at `pos`, sitting substantially ON TOP of another DOCKED tile? That's a
    /// peel-out-of-the-pile gesture (stacked windows being pulled apart), not a dock — used to keep
    /// overlapping tiles from snapping back fused. "Substantial" = >1/4 of the smaller tile's area
    /// covered, so a hair of edge-touch while re-docking into the grid still counts as a clean dock.
    fn piled_on_docked(tiles: &[Tile], me: usize, pos: (i32, i32)) -> bool {
        let (mw, mh) = tiles[me].size;
        let (ml, mt, mr, mb) = (pos.0, pos.1, pos.0 + mw, pos.1 + mh);
        for (i, t) in tiles.iter().enumerate() {
            if i == me || !t.docked || t.collapsed {
                continue;
            }
            let (tl, tt, tr, tb) = (
                t.expected.0,
                t.expected.1,
                t.expected.0 + t.size.0,
                t.expected.1 + t.size.1,
            );
            let ox = (mr.min(tr) - ml.max(tl)).max(0);
            let oy = (mb.min(tb) - mt.max(tt)).max(0);
            let overlap = ox as i64 * oy as i64;
            let smaller = (mw as i64 * mh as i64)
                .min(t.size.0 as i64 * t.size.1 as i64)
                .max(1);
            if overlap * 4 > smaller {
                return true;
            }
        }
        false
    }

    /// Is this tile's drop position within snapping reach of the cluster's bounding box?
    fn near_cluster(tiles: &[Tile], me: usize, pos: (i32, i32)) -> bool {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        let mut any = false;
        for (i, t) in tiles.iter().enumerate() {
            if i == me || !t.docked || t.collapsed {
                continue;
            }
            any = true;
            x0 = x0.min(t.expected.0);
            y0 = y0.min(t.expected.1);
            x1 = x1.max(t.expected.0 + t.size.0);
            y1 = y1.max(t.expected.1 + t.size.1);
        }
        if !any {
            return false;
        }
        let (cx, cy) = (pos.0 + tiles[me].size.0 / 2, pos.1 + tiles[me].size.1 / 2);
        cx >= x0 - SNAP && cx <= x1 + SNAP && cy >= y0 - SNAP && cy <= y1 + SNAP
    }

    fn place(t: &mut Tile, x: i32, y: i32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_TOPMOST, SWP_NOACTIVATE,
        };
        unsafe {
            SetWindowPos(
                t.win,
                HWND_TOPMOST,
                x,
                y,
                t.size.0,
                t.size.1,
                SWP_NOACTIVATE,
            );
        }
        t.expected = (x, y);
    }

    fn register_tile_class() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{LoadCursorW, RegisterClassW, WNDCLASSW};
        let cls: Vec<u16> = "NeuronGlanceTile\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0x0008, // CS_DBLCLKS — the double-click IS the interface (step into the portal)
            lpfnWndProc: Some(tile_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            // a real arrow — a null class cursor leaves whatever was last set (often the
            // busy spinner) smeared over the tile forever
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), 32512 as _) }, // IDC_ARROW
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        unsafe {
            RegisterClassW(&wc);
        }
    }

    /// The tile's INTERIOR is the PORTAL SURFACE: a single click does nothing (moving lives on the
    /// bracket, resizing on the edges/corners), a DOUBLE-CLICK steps THROUGH — possession (the engine
    /// maps the click through any crop to the real window and warps focus + cursor there), and a
    /// right-drag carves a crop (engine-polled). EDGES and CORNERS are resize grips — the system runs
    /// the resize modally and `WM_SIZING` locks the aspect so the glass only ever scales, never
    /// stretches. `WM_NCCALCSIZE` keeps it frameless (client == whole window) despite the sizing
    /// border the grips require.
    unsafe extern "system" fn tile_proc(
        hwnd: windows_sys::Win32::Foundation::HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, GetPropW, GetWindowLongPtrW, GetWindowRect, SetWindowPos, ShowWindow,
            GWLP_USERDATA, HWND_TOPMOST, MINMAXINFO, SWP_NOACTIVATE, SWP_NOSIZE, SW_HIDE,
        };
        const WM_NCHITTEST: u32 = 0x0084;
        const WM_NCCALCSIZE: u32 = 0x0083;
        const WM_SIZING: u32 = 0x0214;
        const WM_GETMINMAXINFO: u32 = 0x0024;
        const WM_WINDOWPOSCHANGED: u32 = 0x0047;
        const WM_LBUTTONDBLCLK: u32 = 0x0203;
        const HTCLIENT: isize = 1;
        const M: i32 = 9; // edge grip thickness
                          // the tile's frame (hairline + bracket), stashed as a window prop at spawn
        let frame_of = |hwnd| unsafe {
            let prop: Vec<u16> = "nframe\0".encode_utf16().collect();
            GetPropW(hwnd, prop.as_ptr()) as windows_sys::Win32::Foundation::HWND
        };
        unsafe {
            match msg {
                // frameless: report the whole window as client area (no border drawn) while still
                // owning the sizing border that makes the resize grips live.
                WM_NCCALCSIZE if wparam != 0 => 0,
                // a MOVE (incl. the modal drag loop, which BLOCKS the engine thread) keeps the frame
                // glued to the tile live — the hairline rides the drag instead of snapping after it.
                WM_WINDOWPOSCHANGED => {
                    let f = frame_of(hwnd);
                    if !f.is_null() {
                        let mut r: RECT = std::mem::zeroed();
                        if GetWindowRect(hwnd, &mut r) != 0 {
                            SetWindowPos(
                                f,
                                HWND_TOPMOST,
                                r.left - 7, // PAD — keep in sync with the frame layout
                                r.top - 7,
                                0,
                                0,
                                SWP_NOACTIVATE | SWP_NOSIZE,
                            );
                        }
                    }
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
                WM_NCHITTEST => {
                    let x = (lparam & 0xffff) as i16 as i32;
                    let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                    let mut r: RECT = std::mem::zeroed();
                    GetWindowRect(hwnd, &mut r);
                    let l = x < r.left + M;
                    let rt = x >= r.right - M;
                    let tp = y < r.top + M;
                    let bt = y >= r.bottom - M;
                    match (tp, bt, l, rt) {
                        (true, _, true, _) => 13, // HTTOPLEFT
                        (true, _, _, true) => 14, // HTTOPRIGHT
                        (_, true, true, _) => 16, // HTBOTTOMLEFT
                        (_, true, _, true) => 17, // HTBOTTOMRIGHT
                        (true, _, _, _) => 12,    // HTTOP
                        (_, true, _, _) => 15,    // HTBOTTOM
                        (_, _, true, _) => 10,    // HTLEFT
                        (_, _, _, true) => 11,    // HTRIGHT
                        // the interior is the PORTAL SURFACE — a plain client area (no caption-drag;
                        // the bracket moves the tile), so double-clicks and right-drags reach us.
                        _ => HTCLIENT,
                    }
                }
                // STEP IN: a double-click on the glass — hand the engine the tile + click point; it
                // owns the mapping (through any crop) and the warp + the way home.
                WM_LBUTTONDBLCLK => {
                    let x = (lparam & 0xffff) as i16 as i32;
                    let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                    *STEP_IN
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some((hwnd as isize, x, y));
                    0
                }
                // lock the aspect to the source window's ratio (stashed in USERDATA as q12 fixed point)
                WM_SIZING => {
                    // duck the frame for the live resize (its bitmap is the OLD size; a mismatched
                    // hairline reads as breakage) — the engine re-shows it fitted on release.
                    let f = frame_of(hwnd);
                    if !f.is_null() {
                        ShowWindow(f, SW_HIDE);
                    }
                    let q = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                    if q > 0 {
                        let aspect = q as f32 / 4096.0;
                        let r = &mut *(lparam as *mut RECT);
                        let w = (r.right - r.left).max(1);
                        let h = (r.bottom - r.top).max(1);
                        match wparam as u32 {
                            3 | 6 => r.right = r.left + (h as f32 * aspect).round() as i32, // TOP/BOTTOM → width from height
                            4 | 5 => {
                                // TOPLEFT/TOPRIGHT: width drives, bottom anchored
                                let nh = (w as f32 / aspect).round() as i32;
                                r.top = r.bottom - nh;
                            }
                            7 | 8 => {
                                // BOTTOMLEFT/BOTTOMRIGHT: width drives, top anchored
                                r.bottom = r.top + (w as f32 / aspect).round() as i32;
                            }
                            _ => r.bottom = r.top + (w as f32 / aspect).round() as i32, // LEFT/RIGHT → height from width
                        }
                    }
                    1
                }
                WM_GETMINMAXINFO => {
                    let mmi = &mut *(lparam as *mut MINMAXINFO);
                    mmi.ptMinTrackSize.x = 120;
                    mmi.ptMinTrackSize.y = 80;
                    0
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }
    }

    unsafe fn spawn_tile(src: isize, size: (i32, i32), docked: bool, aspect: f32) -> Option<Tile> {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::Graphics::Dwm::{
            DwmRegisterThumbnail, DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES,
            DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
        };
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, SetPropW, SetWindowLongPtrW, ShowWindow, GWLP_USERDATA,
            SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
            WS_POPUP, WS_THICKFRAME,
        };
        unsafe {
            let cls: Vec<u16> = "NeuronGlanceTile\0".encode_utf16().collect();
            // WS_THICKFRAME gives the resize grips + cursors; WM_NCCALCSIZE keeps it frameless.
            let win = CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                cls.as_ptr(),
                std::ptr::null(),
                WS_POPUP | WS_THICKFRAME,
                0,
                0,
                size.0,
                size.1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            if win.is_null() {
                return None;
            }
            // stash the locked aspect (q12 fixed point) for WM_SIZING to read
            SetWindowLongPtrW(win, GWLP_USERDATA, (aspect * 4096.0) as isize);
            ShowWindow(win, SW_SHOWNOACTIVATE);
            let mut thumb: isize = 0;
            if DwmRegisterThumbnail(win, src as _, &mut thumb) == 0 {
                let props = DWM_THUMBNAIL_PROPERTIES {
                    dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE,
                    rcDestination: RECT {
                        left: 0,
                        top: 0,
                        right: size.0,
                        bottom: size.1,
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
            // the FRAME: a per-pixel-alpha layered sibling (hairline + corner bracket). The tile
            // carries its frame's handle as a window prop so the modal drag loop can keep the frame
            // glued to the tile live (WM_WINDOWPOSCHANGED fires inside the modal loop; our engine
            // loop is blocked for its duration).
            let fcls: Vec<u16> = "NeuronGlanceFrame\0".encode_utf16().collect();
            let frame = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                fcls.as_ptr(),
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
            if !frame.is_null() {
                let prop: Vec<u16> = "nframe\0".encode_utf16().collect();
                SetPropW(win, prop.as_ptr(), frame as _);
                ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            Some(Tile {
                src,
                win,
                thumb,
                size,
                docked,
                expected: (0, 0),
                dragging: false,
                resizing: false,
                frame,
                fdc: std::ptr::null_mut(),
                fbmp: std::ptr::null_mut(),
                fsize: (0, 0),
                painted: (false, false, false, 0, (0, 0)),
                alpha: -1,
                collapsed: false,
                mark: (0, 0),
                parked: false,
                hover: false,
                press: false,
                moved: false,
                grab: (0, 0),
                raised: 0,
                crop: None,
                sel: None,
                rpress: false,
                rmoved: false,
                possessed: false,
            })
        }
    }

    /// Re-aim a tile's live DWM thumbnail: destination = the (new) client size, source = the carved
    /// crop if one is cut (DWM renders just that region, live and magnified — the whole "rudimentary
    /// photoshop" is this one property), else the whole window.
    unsafe fn retarget_thumb(t: &Tile, size: (i32, i32)) {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::Graphics::Dwm::{
            DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_RECTDESTINATION,
            DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,
        };
        if t.thumb == 0 {
            return;
        }
        let mut flags = DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE;
        let rc_src = match t.crop {
            Some((l, tp, r, b)) => {
                flags |= DWM_TNP_RECTSOURCE;
                RECT {
                    left: l,
                    top: tp,
                    right: r,
                    bottom: b,
                }
            }
            None => RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        };
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: flags,
            rcDestination: RECT {
                left: 0,
                top: 0,
                right: size.0,
                bottom: size.1,
            },
            rcSource: rc_src,
            opacity: 255,
            fVisible: 1,
            fSourceClientAreaOnly: 0,
        };
        unsafe {
            DwmUpdateThumbnailProperties(t.thumb, &props);
        }
    }

    /// The source-window-relative region a tile currently shows: its crop, else the whole window.
    unsafe fn shown_region(t: &Tile) -> (i32, i32, i32, i32) {
        if let Some(c) = t.crop {
            return c;
        }
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe { GetWindowRect(t.src as _, &mut r) };
        (0, 0, (r.right - r.left).max(1), (r.bottom - r.top).max(1))
    }

    // ── POSSESSION: step through the portal, act with real focus, snap home to the pixel ────────────

    /// STEP THROUGH: map a tile-client point through the shown region to the real window, remember
    /// HOME (the focused window + the exact cursor pixel — the tether snapshot, taken automatically),
    /// then warp focus + cursor in. Returns `(source, home window, home cursor)`.
    unsafe fn step_in(t: &mut Tile, cx: i32, cy: i32) -> Option<(isize, isize, (i32, i32))> {
        use windows_sys::Win32::Foundation::{POINT, RECT};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetAncestor, GetCursorPos, GetForegroundWindow, GetWindowRect, IsIconic, SetCursorPos,
            ShowWindow, GA_ROOT, SW_RESTORE,
        };
        unsafe {
            let mut wr: RECT = std::mem::zeroed();
            if GetWindowRect(t.src as _, &mut wr) == 0 {
                return None;
            }
            let (rl, rt, rr, rb) = shown_region(t);
            let px = rl + ((cx as i64 * (rr - rl).max(1) as i64) / t.size.0.max(1) as i64) as i32;
            let py = rt + ((cy as i64 * (rb - rt).max(1) as i64) / t.size.1.max(1) as i64) as i32;
            let home = GetAncestor(GetForegroundWindow(), GA_ROOT) as isize;
            let mut c = POINT { x: 0, y: 0 };
            GetCursorPos(&mut c);
            if IsIconic(t.src as _) != 0 {
                ShowWindow(t.src as _, SW_RESTORE);
            }
            crate::teleport::force_foreground(t.src);
            SetCursorPos(wr.left + px, wr.top + py);
            t.possessed = true;
            POSSESSED.store(true, Ordering::SeqCst);
            Some((t.src, home, (c.x, c.y)))
        }
    }

    /// SNAP HOME from a possession: refocus the window you came from and drop the cursor back on the
    /// exact pixel it left (warp=false = dissolve only — the user moved on by themselves; never fight
    /// a deliberate focus change). Always clears the possession ring + the context flag.
    unsafe fn snap_home(
        poss: &mut Option<(isize, isize, (i32, i32), std::time::Instant)>,
        tiles: &mut [Tile],
        warp: bool,
    ) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IsWindow, SetCursorPos};
        if let Some((src, home, cur, _)) = poss.take() {
            unsafe {
                if warp {
                    if IsWindow(home as _) != 0 {
                        crate::teleport::force_foreground(home);
                    }
                    SetCursorPos(cur.0, cur.1);
                }
            }
            for t in tiles.iter_mut() {
                if t.src == src {
                    t.possessed = false;
                }
            }
        }
        POSSESSED.store(false, Ordering::SeqCst);
    }

    // ── THE BLADE: carve / heal a crop ───────────────────────────────────────────────────────────────

    /// Commit a right-drag selection as the tile's crop: the selection maps through the CURRENTLY
    /// shown region (so cutting while cut zooms deeper), the tile re-locks to the cut's aspect and
    /// re-fits its glass (area preserved), and the cut is remembered for the session.
    unsafe fn commit_crop(t: &mut Tile, mem: &mut Memory) {
        let Some((a, b)) = t.sel else { return };
        let (sx0, sx1) = (
            (a.0.min(b.0) - t.expected.0).clamp(0, t.size.0),
            (a.0.max(b.0) - t.expected.0).clamp(0, t.size.0),
        );
        let (sy0, sy1) = (
            (a.1.min(b.1) - t.expected.1).clamp(0, t.size.1),
            (a.1.max(b.1) - t.expected.1).clamp(0, t.size.1),
        );
        let (rl, rt, rr, rb) = unsafe { shown_region(t) };
        let map_x =
            |v: i32| rl + ((v as i64 * (rr - rl).max(1) as i64) / t.size.0.max(1) as i64) as i32;
        let map_y =
            |v: i32| rt + ((v as i64 * (rb - rt).max(1) as i64) / t.size.1.max(1) as i64) as i32;
        let (cl, ct, cr, cb) = (map_x(sx0), map_y(sy0), map_x(sx1), map_y(sy1));
        if cr - cl < 24 || cb - ct < 24 {
            return; // a sliver isn't a view — too-small cuts are ignored, not committed
        }
        t.crop = Some((cl, ct, cr, cb));
        mem.crops.insert(t.src, (cl, ct, cr, cb));
        unsafe {
            reshape(t, (cr - cl).max(1) as f32 / (cb - ct).max(1) as f32, mem);
        }
    }

    /// Heal the cut: back to the whole window, aspect re-locked to the source's real shape.
    unsafe fn heal_crop(t: &mut Tile, mem: &mut Memory) {
        t.crop = None;
        mem.crops.remove(&t.src);
        let (rl, rt, rr, rb) = unsafe { shown_region(t) }; // the full window now
        unsafe {
            reshape(t, (rr - rl).max(1) as f32 / (rb - rt).max(1) as f32, mem);
        }
    }

    /// Re-lock a tile's aspect (the WM_SIZING constraint) + re-fit its glass to it (area preserved),
    /// then re-aim the thumbnail. Shared by carve and heal.
    unsafe fn reshape(t: &mut Tile, aspect: f32, mem: &mut Memory) {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            SetWindowLongPtrW, SetWindowPos, GWLP_USERDATA, HWND_TOPMOST, SWP_NOACTIVATE,
            SWP_NOMOVE,
        };
        unsafe {
            SetWindowLongPtrW(t.win, GWLP_USERDATA, (aspect * 4096.0) as isize);
            let ns = refit(t.size, aspect);
            SetWindowPos(
                t.win,
                HWND_TOPMOST,
                0,
                0,
                ns.0,
                ns.1,
                SWP_NOACTIVATE | SWP_NOMOVE,
            );
            t.size = ns;
            mem.sizes.insert(t.src, ns);
            retarget_thumb(t, ns);
        }
    }

    unsafe fn destroy_tile(t: &Tile) {
        use windows_sys::Win32::Graphics::Dwm::DwmUnregisterThumbnail;
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject};
        use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyWindow, RemovePropW};
        unsafe {
            if t.thumb != 0 {
                DwmUnregisterThumbnail(t.thumb);
            }
            let prop: Vec<u16> = "nframe\0".encode_utf16().collect();
            RemovePropW(t.win, prop.as_ptr());
            DestroyWindow(t.win);
            if !t.frame.is_null() {
                DestroyWindow(t.frame);
            }
            if !t.fbmp.is_null() {
                DeleteObject(t.fbmp as _);
            }
            if !t.fdc.is_null() {
                DeleteDC(t.fdc);
            }
        }
    }

    // ── the FRAME: a paper-thin hairline + the corner-bracket handle ─────────────────────────────────

    fn register_frame_class() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{LoadCursorW, RegisterClassW, WNDCLASSW};
        let cls: Vec<u16> = "NeuronGlanceFrame\0".encode_utf16().collect();
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(frame_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: std::ptr::null_mut(),
            hIcon: std::ptr::null_mut(),
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), 32649 as _) }, // IDC_HAND — the handle
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: cls.as_ptr(),
        };
        unsafe {
            RegisterClassW(&wc);
        }
    }

    /// Only the CORNER box absorbs the mouse (the engine polls clicks/drags itself); everywhere else
    /// the frame is a ghost — HTTRANSPARENT passes the hit to the tile beneath (same thread), so
    /// dragging and edge-resizing keep working straight through the hairline.
    unsafe extern "system" fn frame_proc(
        hwnd: windows_sys::Win32::Foundation::HWND,
        msg: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::{DefWindowProcW, GetWindowRect};
        const WM_NCHITTEST: u32 = 0x0084;
        const HTCLIENT: isize = 1;
        const HTTRANSPARENT: isize = -1;
        if msg == WM_NCHITTEST {
            unsafe {
                let x = (lparam & 0xffff) as i16 as i32;
                let y = ((lparam >> 16) & 0xffff) as i16 as i32;
                let mut r: RECT = std::mem::zeroed();
                GetWindowRect(hwnd, &mut r);
                return if x < r.left + CORNER && y < r.top + CORNER {
                    HTCLIENT
                } else {
                    HTTRANSPARENT
                };
            }
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    /// Keep a frame seated: position/size follow the tile (or the parked mark), repaint when its look
    /// changes (collapse / hover / window count / size), and fade collapsed brackets by proximity.
    unsafe fn sync_frame(t: &mut Tile, count: usize, cursor: (i32, i32)) {
        if t.frame.is_null() {
            return;
        }
        let (pos, sz) = if t.collapsed {
            (t.mark, (ARM + 2, ARM + 2))
        } else {
            (
                (t.expected.0 - PAD, t.expected.1 - PAD),
                (t.size.0 + 2 * PAD, t.size.1 + 2 * PAD),
            )
        };
        let key = (t.collapsed, t.hover, t.possessed, count, sz);
        // a live right-drag marquee animates every tick — repaint unconditionally while it's open
        if key != t.painted || t.sel.is_some() {
            unsafe {
                paint_frame(t, count, sz);
            }
            t.painted = key;
            t.alpha = -1; // force a re-blend with the fresh bitmap
        }
        // collapsed brackets ghost until you come near; expanded frames are quietly constant
        let alpha = if t.collapsed {
            let c = (pos.0 + ARM / 2, pos.1 + ARM / 2);
            let d = (((cursor.0 - c.0).pow(2) + (cursor.1 - c.1).pow(2)) as f32).sqrt();
            let prox = ((PROX - d) / PROX).clamp(0.0, 1.0);
            (40.0 + prox * prox * 215.0) as i32
        } else {
            255
        };
        // one UpdateLayeredWindow moves, resizes, and re-blends atomically — only when something moved
        let dirty = t.alpha != alpha || {
            use windows_sys::Win32::Foundation::RECT;
            use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
            let mut r: RECT = unsafe { std::mem::zeroed() };
            unsafe { GetWindowRect(t.frame, &mut r) };
            (r.left, r.top) != pos
        };
        if dirty {
            unsafe {
                blend_frame(t, pos, sz, alpha as u8);
            }
            t.alpha = alpha;
        }
        // a live resize ducked the frame (tile_proc hides it mid-modal) — bring it back fitted
        unsafe {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                IsWindowVisible, ShowWindow, SW_SHOWNOACTIVATE,
            };
            if !t.resizing && IsWindowVisible(t.frame) == 0 {
                ShowWindow(t.frame, SW_SHOWNOACTIVATE);
                blend_frame(t, pos, sz, alpha as u8);
            }
        }
    }

    /// Paint the frame's DIB (premultiplied BGRA): expanded = a 1px hairline ring just OUTSIDE the
    /// glass + the corner bracket; collapsed = the bracket alone. The bracket's arm thickness encodes
    /// the constellation's window count; hover runs it white-hot.
    unsafe fn paint_frame(t: &mut Tile, count: usize, sz: (i32, i32)) {
        use windows_sys::Win32::Graphics::Gdi::{DeleteDC, DeleteObject};
        unsafe {
            if sz != t.fsize || t.fdc.is_null() {
                if !t.fbmp.is_null() {
                    DeleteObject(t.fbmp as _);
                    t.fbmp = std::ptr::null_mut();
                }
                if !t.fdc.is_null() {
                    DeleteDC(t.fdc);
                    t.fdc = std::ptr::null_mut();
                }
                // re-allocate the backing DIB at the new size via the shared seam (the ONE
                // CreateDIBSection); the live pixel pointer is re-resolved below per paint.
                let dib = match crate::surface::Dib::new(sz.0, sz.1) {
                    Some(d) => d,
                    None => return,
                };
                t.fdc = dib.dc;
                t.fbmp = dib.bmp as _;
                t.fsize = sz;
            }
            // resolve the live pixel pointer from the kept bitmap
            let mut bits: *mut u32 = std::ptr::null_mut();
            {
                use windows_sys::Win32::Graphics::Gdi::{GetObjectW, BITMAP};
                let mut bm: BITMAP = std::mem::zeroed();
                if GetObjectW(
                    t.fbmp as _,
                    std::mem::size_of::<BITMAP>() as i32,
                    &mut bm as *mut _ as *mut _,
                ) != 0
                {
                    bits = bm.bmBits as *mut u32;
                }
            }
            if bits.is_null() {
                return;
            }
            let (w, h) = sz;
            let px = std::slice::from_raw_parts_mut(bits, (w * h) as usize);
            px.fill(0);
            // premultiplied ACCENT — the user's live WEAVE colour (settings-driven), so the frame
            // follows the material design language instead of a hardcoded phosphor green.
            let (ar, ag, ab) = crate::weave::overlay_accent();
            let (ar, ag, ab) = ((ar * 255.0) as u32, (ag * 255.0) as u32, (ab * 255.0) as u32);
            let phos = move |a: u32| -> u32 {
                (a << 24) | ((ar * a / 255) << 16) | ((ag * a / 255) << 8) | (ab * a / 255)
            };
            // white-hot for the hovered bracket
            let hot = |a: u32| -> u32 { (a << 24) | (a << 16) | (a << 8) | a };
            let mut fill = |x0: i32, y0: i32, x1: i32, y1: i32, c: u32| {
                for y in y0.max(0)..y1.min(h) {
                    for x in x0.max(0)..x1.min(w) {
                        px[(y * w + x) as usize] = c;
                    }
                }
            };
            let tk = bracket_t(count);
            if t.collapsed {
                // the folded tab: just the L, slightly brighter (it's all there is)
                let c = if t.hover { hot(235) } else { phos(220) };
                fill(0, 0, ARM, tk, c);
                fill(0, 0, tk, ARM, c);
            } else {
                // the hairline: a 1px ring sitting 1px outside the glass (never covers content).
                // POSSESSED → the ring burns: you are THROUGH this portal right now.
                let hl = if t.possessed { hot(210) } else { phos(96) };
                let (l, tp, r, b) = (PAD - 1, PAD - 1, w - PAD, h - PAD);
                fill(l, tp, r + 1, tp + 1, hl);
                fill(l, b, r + 1, b + 1, hl);
                fill(l, tp, l + 1, b + 1, hl);
                fill(r, tp, r + 1, b + 1, hl);
                // the corner bracket: arms hugging the hairline's top-left, thickness = window count
                let c = if t.hover || t.possessed {
                    hot(245)
                } else {
                    phos(190)
                };
                let (bx, by) = (l - (tk - 1), tp - (tk - 1));
                fill(bx, by, bx + ARM, by + tk, c);
                fill(bx, by, bx + tk, by + ARM, c);
                // the BLADE's live marquee: the selection being carved, white-hot hairlines
                if let Some((a, bb)) = t.sel {
                    let (fx, fy) = (t.expected.0 - PAD, t.expected.1 - PAD);
                    let (x0, x1) = (a.0.min(bb.0) - fx, a.0.max(bb.0) - fx);
                    let (y0, y1) = (a.1.min(bb.1) - fy, a.1.max(bb.1) - fy);
                    let mq = hot(235);
                    fill(x0, y0, x1 + 1, y0 + 1, mq);
                    fill(x0, y1, x1 + 1, y1 + 1, mq);
                    fill(x0, y0, x0 + 1, y1 + 1, mq);
                    fill(x1, y0, x1 + 1, y1 + 1, mq);
                }
            }
        }
    }

    /// Position + size + blend the frame in ONE UpdateLayeredWindow call (atomic on screen) — via
    /// the shared `surface::present_dc` seam (the one BLENDFUNCTION, AC_SRC_OVER + AC_SRC_ALPHA).
    unsafe fn blend_frame(t: &Tile, pos: (i32, i32), sz: (i32, i32), alpha: u8) {
        use windows_sys::Win32::Foundation::POINT;
        use windows_sys::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
        unsafe {
            if t.fdc.is_null() {
                return;
            }
            let screen = GetDC(std::ptr::null_mut());
            let dst = POINT { x: pos.0, y: pos.1 };
            let size = windows_sys::Win32::Foundation::SIZE { cx: sz.0, cy: sz.1 };
            crate::surface::present_dc(t.frame, screen, t.fdc, Some(dst), size, alpha);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
    }

    /// Re-fit a remembered size to a (possibly changed) source aspect, preserving its AREA — a window
    /// that changed shape since the size was learned must scale, never stretch.
    fn refit(s: (i32, i32), aspect: f32) -> (i32, i32) {
        let area = (s.0.max(1) as f32) * (s.1.max(1) as f32);
        let w = (area * aspect.max(0.01)).sqrt();
        ((w as i32).max(120), ((w / aspect.max(0.01)) as i32).max(80))
    }

    /// Clamp a window/bracket position so at least its body stays on the virtual screen — a parked
    /// thing must never be lost beyond every monitor's edge.
    fn clamp_to_screen(p: (i32, i32), sz: (i32, i32)) -> (i32, i32) {
        use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
        // SM_XVIRTUALSCREEN=76, SM_YVIRTUALSCREEN=77, SM_CXVIRTUALSCREEN=78, SM_CYVIRTUALSCREEN=79
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(76),
                GetSystemMetrics(77),
                GetSystemMetrics(78),
                GetSystemMetrics(79),
            )
        };
        (
            p.0.clamp(vx, (vx + vw - sz.0).max(vx)),
            p.1.clamp(vy, (vy + vh - sz.1).max(vy)),
        )
    }
} // mod imp
