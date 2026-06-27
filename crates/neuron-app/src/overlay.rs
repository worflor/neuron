//! The spell overlay — a transparent, click-through, always-on-top window that renders the
//! "living sigil" while you weave: a glowing comet trail with a white-hot head, a procedural rune
//! ring that assembles as you draw and flares on recognition, a charge-glow at the anchor, and
//! sparse embers. **The same overlay renders both kinds of weave**: a drawn glyph (rich) and a
//! radial flick (the simplest weave — it additionally lights up the sector wheel). Localized around
//! the cursor anchor, additive glow (reads over any background), fast decay so it never clutters.
//!
//! It never takes input: `WS_EX_TRANSPARENT` makes it click-through, `WS_EX_NOACTIVATE` keeps focus
//! where it was, and the per-pixel alpha (`UpdateLayeredWindow`) means transparent pixels pass the
//! mouse straight to whatever's underneath.
//!
//! Efficient + deterministic: a fixed-size pair of float buffers (glow + white core), bounded glow
//! splats, a frame counter (no wall-clock) driving animation, and a per-weave fixed PRNG seed so
//! the same stroke draws the same sigil. ~60fps on one thread, a few MB.
//!
//! Cross-platform seam: the public API ([`SpellOverlay`], [`WeaveMode`]) is platform-neutral; this
//! is the Windows body (a layered window + a software rasterizer). macOS = a transparent `NSWindow`
//! with `ignoresMouseEvents` + a `CALayer`; X11/Wayland = an override-redirect / layer-shell surface
//! with an empty input region. Same `begin/push/recognized/end` contract.

/// Which kind of weave the overlay is drawing — a rich glyph, the degenerate radial flick (which
/// also shows the sector wheel), a BEACON ASK (a macro's yes/no prompt, answered by color: flick
/// toward the accent west = yes, the raw-material east = no, vertical = pass), or the ask's QUIET
/// SIGNAL stage: a one-line strip at the top of the monitor saying a macro is asking — the wheel
/// itself NEVER opens until the user engages the trigger (a beacon must never force itself open).
/// Mirrors the spellweaving continuum: radial is the simple subset, ask is the two-color special
/// case with the question composited into the glow.
/// One realm card on the teleport map: (card rect, its windows as (rect, brightness)).
pub type MapRealm = ([f32; 4], Vec<([f32; 4], f32)>);

/// How far out (fraction of the rim) the stroke must reach before a fannable wedge's second tier
/// opens — so a quick flick still picks the wedge itself, but pushing onward fans the options.
pub const FAN_REACH: f32 = 0.80;

/// The sub-option index a stroke is aiming at within wedge `wedge`'s fan of `m` options — the ONE
/// rule the overlay shows with and the beacon commits with, so they can never disagree. `aim` is
/// canvas-relative; returns -1 if not reaching into the fan.
pub fn fan_pick(aim: (f32, f32), wedge: i32, sectors: u8, m: usize, rim: f32) -> i32 {
    if m == 0 {
        return -1;
    }
    let reach = (aim.0 * aim.0 + aim.1 * aim.1).sqrt();
    if reach < rim * FAN_REACH {
        return -1; // hasn't pushed out into the second tier yet
    }
    let n = sectors.max(1) as f32;
    let slice = std::f32::consts::TAU / n;
    let span = (slice * 1.3 * m as f32).min(std::f32::consts::TAU * 0.7);
    let bearing = wedge as f32 / n * std::f32::consts::TAU;
    // tip angle, 0 = North, clockwise (matching sector geometry)
    let mut rel = aim.0.atan2(-aim.1) - bearing;
    while rel > std::f32::consts::PI {
        rel -= std::f32::consts::TAU;
    }
    while rel < -std::f32::consts::PI {
        rel += std::f32::consts::TAU;
    }
    let half = (span / 2.0).max(1e-3);
    let frac = ((rel / half) * 0.5 + 0.5).clamp(0.0, 1.0);
    (frac * (m as f32 - 1.0)).round() as i32
}

#[cfg(test)]
mod fan_tests {
    use super::fan_pick;

    #[test]
    fn fan_pick_reaches_and_aims() {
        // wedge 0 = North (up = -y). rim 150; a close stroke hasn't reached the fan.
        assert_eq!(
            fan_pick((0.0, -60.0), 0, 8, 3, 150.0),
            -1,
            "too close = no fan"
        );
        // pushed out north, centred → the middle option of 3
        assert_eq!(
            fan_pick((0.0, -160.0), 0, 8, 3, 150.0),
            1,
            "centre = middle option"
        );
        // pushed out and sharply angled left/right → opposite ends of the fan
        let left = fan_pick((-150.0, -60.0), 0, 8, 3, 150.0);
        let right = fan_pick((150.0, -60.0), 0, 8, 3, 150.0);
        assert!(
            left != right,
            "angle selects different options ({left} vs {right})"
        );
        assert!((0..3).contains(&left) && (0..3).contains(&right));
        assert!(left < right, "left sweep = earlier option");
        // a single option always resolves to 0 once reached
        assert_eq!(fan_pick((0.0, -200.0), 2, 8, 1, 150.0), 0);
        // zero options never picks
        assert_eq!(fan_pick((0.0, -200.0), 0, 8, 0, 150.0), -1);
    }
}

/// The pre-attentive TONE of a wedge — color carries the state before you read a word.
/// Live = phosphor green (on / working), Off = red (muted / off / danger), Inert = dim (unset /
/// nothing bound), Active = prism accent (the selected / current one), Plain = neutral phosphor.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Tone {
    Live,
    Off,
    Inert,
    Active,
    Plain,
}

/// The archetype a wedge draws — a small procedural vector icon (no fonts). Chosen by the bound
/// Action so identity is recognizable at a glance without reading the title.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WedgeGlyph {
    Speaker,
    Mic,
    WindowStack,
    Anchor,
    ProfileDot,
    Key,
    Media,
    Terminal,
    Python,
    Teleport,
    Whiteboard,
    Knockback,
    Summon,
    Banish,
    Pin,
    Flip,
    Target,
    Scroll,
    Ghost,
    Mark,
    /// the control-center NETWORK readout — wifi waves (on wifi) or an ethernet plug (on the wire);
    /// the variant decides which is drawn (see [`WedgeGlyph::Ethernet`]).
    Network,
    /// the control-center ETHERNET readout — a wired plug (drawn when the live link is the wire).
    Ethernet,
    /// the control-center BLUETOOTH readout — the runic BT mark.
    Bluetooth,
    /// MONITORS-OFF — a monitor on a stand with a power symbol on the panel ("sleep the displays").
    Screen,
    /// CURTAIN — a draped curtain (rod + folds + scalloped hem): the panic privacy screen.
    Curtain,
    /// POLLING — a frequency mark: three rising bars (a tiny ▮▮▮ spectrum), the report-rate cue
    /// on a notification card.
    Pulse,
    /// BRIGHTNESS — a sun: a small filled disc with radiating rays, the light-level cue.
    Sun,
    /// BATTERY — a battery cell (rounded body + terminal nub + charge bars): the power/charge cue.
    Battery,
    /// ASK — a question mark: a clean procedural prompt mark (a curling head over a baseline dot),
    /// the beacon's ask-announcement chip.
    Ask,
    /// SIDE PLATE — a swappable hardware module: a chip frame with a small grid of button pips, the
    /// side-plate attach/detach cue. Reads as "a hardware piece", distinct from the battery cell.
    SidePlate,
    Blank,
}

/// One wedge as a live INSTRUMENT readout: an icon, a (fading) title, an optional big live value
/// ("62%", "×3", "Headset"), a tone, and an optional 0..1 meter (drawn as a fill arc under the
/// icon). Built data-driven from the bound Action + live host state — the wheel always tells the
/// truth, not the editor's stale copy.
#[derive(Clone, Debug)]
pub struct WedgeView {
    pub glyph: WedgeGlyph,
    pub title: String,
    pub value: Option<String>,
    pub tone: Tone,
    pub meter: Option<f32>,
}

impl WedgeView {
    /// The empty wedge (nothing bound) — nothing drawn.
    pub fn blank() -> Self {
        WedgeView {
            glyph: WedgeGlyph::Blank,
            title: String::new(),
            value: None,
            tone: Tone::Inert,
            meter: None,
        }
    }
}

/// One second-tier fan option (e.g. an output device): a label and whether it's the CURRENT one.
#[derive(Clone, Debug)]
pub struct FanView {
    pub label: String,
    pub active: bool,
}

/// The LIVE next-glyph prediction shown while drawing a free gesture: the action the stroke is
/// becoming (as a wedge card), how confident we are (0..1, climbing as it commits), whether it has
/// crossed the recognition gate (`locked` → a snap-pulse), and the target template's drawable
/// `ghost` polyline (centered unit box; empty if that template predates exemplars).
#[derive(Clone, Debug)]
pub struct GlyphHint {
    pub view: WedgeView,
    pub confidence: f32,
    pub locked: bool,
    pub ghost: Vec<[f32; 2]>,
}

#[derive(Clone, Debug)]
pub enum WeaveMode {
    /// A free gesture. `hint` (optional) is the LIVE next-glyph prediction — the action the stroke
    /// is becoming, ghosted near the head and solidifying as confidence climbs (see [`GlyphHint`]).
    Glyph {
        hint: Option<GlyphHint>,
    },
    /// the comms wheel. `widgets` (index = sector) are live instrument readouts rendered as an
    /// icon+value stack at each slice — so the LIVE wheel says what every wedge does AND its
    /// current state, not just the GUI editor's copy of it. `fans` is the SECOND TIER (index =
    /// sector): a wedge with sub-options fans them out past the rim the moment you aim it and reach
    /// outward — the overlay shows the fan for whichever wedge is live, generic over N. Empty = no fan.
    Radial {
        sectors: u8,
        widgets: Vec<WedgeView>,
        fans: Vec<Vec<FanView>>,
    },
    Ask {
        label: String,
        /// The answer wheel's options (the wedges) — the prompt IS this list + the radial core. N=2
        /// is yes/no, N≥3 a radial menu, N=1 a confirm; the renderer draws whatever it's given.
        options: Vec<String>,
        detail: String,
    },
    /// THE MULTI-NOTIFICATION SURFACE — an identity-coalesced STACK of state-change confirmation +
    /// macro-notify cards, drawn in the card grammar (`draw_notify_stack`, reusing the beacon ask's
    /// `card_frame`/`chip_box` chrome) and PLACED by the notification engine on its OWN overlay
    /// instance, so it never clobbers a live weave. The engine ([`crate::notifs`]) holds the live set
    /// and pushes the WHOLE list here each tick (via [`SpellOverlay::stack`]), with every slot's
    /// animation already eased off its clock — the overlay is a pure renderer of the given frame.
    /// Three presentations (a reflowing column / a single-card swap / a digest summary) all share the
    /// card chrome; the content reads as a value HIERARCHY (per-type `glyph` + small dim title + LARGE
    /// value wrapping to ≤3 lines + the track-bar + tiny dim prev). ALL colour is the live material
    /// accent — the type identity rides the GLYPH alone, never a hardcoded per-kind colour.
    ///
    /// A fieldless MARKER: the live stack's content (slots / mode / corner / digest / tail) all flow
    /// through [`Cmd::Stack`] into the render thread's cached state (so text rasters survive across the
    /// engine's 60fps pushes); this variant only tells the render loop "the surface is a notification
    /// stack" for its cover / draw / placement arms. (The ask wheel + Signal strip are untouched.)
    NotifyStack,
    /// the DIAL — an analog knob the eigenmotion stroke drives. A 270° arc fills to `fill` (0..1);
    /// `value` is the reading ("62%"), `device` the endpoint being turned ("Headset"), `mic` picks
    /// the target icon (mic vs speaker), `muted` rings it red, and `glow` (0..1) pulses with how
    /// fast you're turning, so coarse vs fine reads on the glass.
    Dial {
        value: String,
        device: String,
        fill: f32,
        glow: f32,
        mic: bool,
        muted: bool,
    },
    /// THE CONTROL CENTER — a glanceable system-state card with a few quick actions. Three
    /// readouts arranged around the centre: NETWORK at north (ethernet vs wifi + SSID + online),
    /// OUTPUT at west (the default device + volume), BLUETOOTH at east (present, a toggle seam).
    /// `link` 0=offline/1=ethernet/2=wifi picks the net icon + tone; `net`/`out`/`bt` are the
    /// pre-rendered value strings; `out_fill` is the output level meter; `bt_on` whether a radio
    /// exists. The aimed quadrant (the flick) lights — east commits the bluetooth seam, west the
    /// output flip; a peek (sub-deadzone) just closes. Kept deliberately uncrowded.
    Control {
        link: u8,
        net: String,
        ssid: String,
        out: String,
        out_fill: f32,
        bt: String,
        bt_on: bool,
    },
    /// TELEPORT's mini-map: the CURRENT desk (monitor outlines + recency-lit window blobs) with
    /// other virtual desktops as small REALM cards beneath — each card a little world with its
    /// own miniature blobs. All coords canvas-center-relative; the pushed point stream is the
    /// GHOST cursor. `hot` = the depth dial's desk pick (-1 = geometric frontmost);
    /// `hot_realm` = (realm, window) when the ghost is in the strip.
    Map {
        monitors: Vec<[f32; 4]>,
        windows: Vec<([f32; 4], f32)>,
        realms: Vec<MapRealm>,
        cursor: (f32, f32),
        hot: i32,
        hot_realm: (i32, i32),
        /// A SPECTRALLY GRABBED window riding the ghost (canvas rect, recentered every push) —
        /// drawn as a bright carried phantom so the drop target reads at a glance.
        carried: Option<[f32; 4]>,
        /// Warpstone pips: the canvas centre of every tethered window's cell — so your anchors are
        /// VISIBLE on the map while you aim (a tiny directed-intent ring marks each).
        tethers: Vec<(f32, f32)>,
        /// The DEPTH DIAL's affordance: `(depth, count)` for the column under the ghost. When
        /// `count > 1` the windows overlap here and scrolling descends — a stack of layer pips
        /// beside the ghost teaches that without words. `count <= 1` (one window, or a realm pick)
        /// draws nothing.
        depth: (i32, i32),
    },
    /// KNOCKBACK: the rhythm familiar's STAGE — a fixed staff (anchored once at session entry)
    /// where the whole duet is visible: your strikes build phosphor constructs left→right in
    /// real time, a seal-arc drains to show your phrase committing, the twin rebuilds your
    /// rhythm in violet where it stood (the mirror made visible) and extends it with the
    /// flourish, and the open BLUEPRINT marks exactly where the next beat would fall — strike
    /// it and it materializes. Same hard-light grammar as `neuron::scene`.
    Twin {
        /// Everything currently standing on the staff (player + twin + blueprint + flares).
        beats: Vec<TwinBeat>,
        /// Phrase-seal meter 0..1 (your phrase commits when it empties); < 0 = hidden.
        seal: f32,
        /// Ambient mood wash: rgb in 0..=1 + strength 0..1 (0 = none). Harmony hush, storm
        /// amber, haunting grey-violet.
        wash: (f32, f32, f32, f32),
        /// The session weave strip: one (hue°, amp 0..1, bright 0..1) shard per exchange,
        /// oldest first. Crystallized progress, always visible.
        weave: Vec<(f32, f32, f32)>,
        /// Quiet caption under the staff ("" = none) — the only words the game ever says.
        hint: String,
        /// The familiar's presence 0..1: it materializes on entry, breathes while alive,
        /// dims when unfed.
        presence: f32,
        /// Stillpoint time dilation 0..1 — slows the stage's visual clocks (never the music).
        dilate: f32,
    },
}

/// One card in a [`WeaveMode::NotifyStack`]. Carries the card's content (the same value-hierarchy
/// the single `Notify` card draws) PLUS the per-slot animation the engine has already eased off its
/// own clock — the overlay just rasterizes the text (cached by `ident`+`rev`) and draws at the given
/// `y`/`alpha`/`scale`/`collapse`. The engine owns ALL timing; this struct is one rendered frame of
/// one card.
#[derive(Clone, Debug)]
pub struct NotifySlot {
    /// the coalesce key — also the overlay's raster cache key (re-rasterize when `rev` changes).
    pub ident: String,
    /// bumped by the engine whenever this slot's text content changes, so the overlay knows to
    /// re-rasterize (and not on every 60fps push) — "rasterize per content-change, not per frame".
    pub rev: u64,
    pub title: String,
    pub value: String,
    /// the old reading, already prefixed for its grammar ("was 800" / "← chill"); "" = none.
    pub prev: String,
    pub glyph: WedgeGlyph,
    /// ranged-change direction for the ↑/↓ cue: -1 down, 0 none, +1 up.
    pub dir: i8,
    /// the new value's position on its 0..1 track (the track-bar fill); `< 0` = none (discrete).
    pub fill: f32,
    /// the OLD value's position on the same track (the tick); `< 0` = unknown/none.
    pub prev_fill: f32,
    /// Draw the grounded squircle panel behind the card (vs the floating lozenge).
    pub panel: bool,
    /// THE ASK card: a beacon's `neuron.ask(...)` announcement, drawn in the ASK layout (the `?`/Ask
    /// glyph + the prominent question + the answer-grammar row) instead of the value-hierarchy. It's
    /// a persistent slot in the same stack — the one informational pipeline — and only the answer
    /// WHEEL is a separate surface. `false` for every confirmation / macro-notify card.
    pub is_ask: bool,
    /// the slot's animated TOP-offset from the column's corner edge, in px (the engine's eased
    /// reflow target — neighbours glide when one enters/leaves). 0 = nearest the corner.
    pub y: f32,
    /// entry/exit fade 0..1 (eased: 0 just-spawned/just-gone, 1 settled).
    pub alpha: f32,
    /// coalesce-bump scale (≈1.0 settled, up to ~1.06 on a fresh update pulse).
    pub scale: f32,
    /// exit height-collapse 0..1 (0 = full height, 1 = fully collapsed so the column closes the gap).
    pub collapse: f32,
    /// a transient value-FLASH 0..1 (a brief bright pulse on the value when this slot coalesced).
    pub bump: f32,
}

/// The DIGEST summary card's view (see [`WeaveMode::NotifyStack`]). Drawn only in digest mode when
/// >1 note is live: a big COUNT, a ROW of the distinct source glyphs (deduped by kind), and the
/// latest note's title/value line. Its own alpha/scale animate the calm-count reveal.
#[derive(Clone, Debug)]
pub struct DigestView {
    /// how many notes are live (0 = no digest; render the single live slot as a plain card instead).
    pub count: u32,
    /// the DISTINCT source glyphs, deduped by kind, in first-seen order (the "what fired" row).
    pub glyphs: Vec<WedgeGlyph>,
    /// the latest note's title (the noun line under the count).
    pub title: String,
    /// the latest note's value (the headline line).
    pub value: String,
    /// the summary card's fade 0..1.
    pub alpha: f32,
    /// the expand-on-settle reveal 0..1 (the glyph row + line slide open as the count lands).
    pub reveal: f32,
}

impl DigestView {
    /// The empty digest — no summary (the lone live slot is drawn as a plain card).
    pub fn none() -> Self {
        DigestView {
            count: 0,
            glyphs: Vec::new(),
            title: String::new(),
            value: String::new(),
            alpha: 0.0,
            reveal: 0.0,
        }
    }
}

/// One beat on the [`WeaveMode::Twin`] stage. Stage-relative position; `rgb` in 0..=1; `kind`:
/// 0 = player construct, 1 = twin construct, 2 = twin flourish (warmer, +1 facet),
/// 3 = the open blueprint (unbuilt wireframe — answer here), 4 = materializing (the strike
/// that builds the blueprint; `phase` runs the flash 0→1).
#[derive(Clone, Copy, Debug)]
pub struct TwinBeat {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub rgb: (f32, f32, f32),
    pub weight: f32,
    pub phase: f32,
    pub kind: u8,
}

#[cfg(windows)]
pub use imp::SpellOverlay;

#[cfg(not(windows))]
pub use stub::SpellOverlay;

#[cfg(not(windows))]
mod stub {
    use super::{DigestView, GlyphHint, NotifySlot, WeaveMode};
    /// No-op overlay until the per-OS body lands (see module docs).
    pub struct SpellOverlay;
    impl SpellOverlay {
        pub fn spawn() -> Self {
            SpellOverlay
        }
        pub fn begin(&self, _mode: WeaveMode) {}
        pub fn push(&self, _pts: Vec<(f32, f32)>) {}
        pub fn hint(&self, _hint: Option<GlyphHint>) {}
        pub fn recognized(&self, _hit: bool) {}
        pub fn stack(
            &self,
            _slots: Vec<NotifySlot>,
            _mode: u8,
            _corner: (f32, f32),
            _d: DigestView,
            _tail: u32,
        ) {
        }
        pub fn end(&self) {}
    }
}

#[cfg(windows)]
mod imp {
    use super::{DigestView, GlyphHint, NotifySlot, Tone, WeaveMode, WedgeGlyph};
    use std::f32::consts::TAU;
    use std::sync::mpsc::{channel, Sender};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{POINT, SIZE};
    use windows_sys::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
        GetMonitorInfoW, GetTextExtentPoint32W, MonitorFromPoint, SelectObject,
        SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER,
        BI_RGB, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DIB_RGB_COLORS, FW_NORMAL,
        HBITMAP, MONITORINFO, MONITOR_DEFAULTTONEAREST, TRANSPARENT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetCursorPos, PeekMessageW, SetWindowPos, ShowWindow, TranslateMessage,
        HWND_NOTOPMOST, HWND_TOPMOST, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
        SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT,
    };

    // The overlay buffer is generously larger than the radial menu so the cast TRAIL can run far
    // across the monitor (item 20: the draw area is "free for the entire monitor estate"), not clip
    // at a box slightly bigger than the wheel. The wheel/dial/map geometry is UNCHANGED — fixed radii
    // around the anchor — only the room to DRAW grew. Per-frame cost stays bounded by a dirty rect
    // (see the render loop), so the big buffer is MEMORY (~82MB of channels), not compute. The window
    // is no longer clamped to the monitor (it extends off-screen, transparently), so the anchor at
    // CX/CY always sits exactly under the cursor regardless of how near an edge you cast. ±800px reach.
    const W: i32 = 1600;
    const H: i32 = 1600;
    const CX: f32 = (W / 2) as f32;
    const CY: f32 = (H / 2) as f32;
    // INVARIANT (enforced below): every cursor-anchored weave (the ask wheel, the radial, the dial,
    // …) draws around buffer (CX, CY) and `place_window` centres the window on the cursor as
    // `cur - W/2`, so the anchor lands exactly under the cursor only while CX == W/2. Keep them equal.
    const _: () = assert!(
        CX == (W / 2) as f32,
        "CX must equal W/2: the cursor-anchored weaves depend on it (see place_window)"
    );
    // colour lives in the spellweaving material now (crate::weave) — the accent (the old phosphor),
    // the honey body ramp, and the warn red are all theme data, not constants here.

    // ── DIRTY TILES — touch only what's actually drawn ──────────────────────────────────────────
    // A weave stroke is a 1-D curve: its drawn pixels are O(reach), but its bounding box is
    // O(reach²). The old renderer ran the costly per-pixel passes (clear / dusk / aura / composite)
    // over the trail's *bounding box*, so casting far from the anchor cost quadratically more every
    // frame — the exponential-feeling lag. Here the passes instead run over a TILE COVER of the
    // genuine content (the eigenmotion stroke's own geometry: the footprint + the marched trail +
    // its rune-ring bands + the prediction ghost + embers), so per-frame cost scales with the
    // stroke's *length*, not its reach². Reach stops mattering; you pay for ink, not for the empty
    // rectangle around it.
    const TILE: i32 = 32;
    const TW: i32 = (W + TILE - 1) / TILE; // tiles across the buffer
    const TH: i32 = (H + TILE - 1) / TILE; // tiles down

    /// A coarse on/off cover of the buffer in `TILE`-sized cells — the set of regions a frame
    /// actually touches. Marked from the content's geometry (boxes, marched curves, radius bands),
    /// dilated for the passes' small neighbour-read margins, then iterated by each pass.
    #[derive(Clone)]
    struct TileSet {
        on: Vec<bool>, // TW*TH, row-major; index = ty*TW + tx
    }
    impl TileSet {
        fn new() -> Self {
            TileSet {
                on: vec![false; (TW * TH) as usize],
            }
        }
        #[inline]
        fn set_tile(&mut self, tx: i32, ty: i32) {
            if tx >= 0 && tx < TW && ty >= 0 && ty < TH {
                self.on[(ty * TW + tx) as usize] = true;
            }
        }
        #[inline]
        fn get_tile(&self, tx: i32, ty: i32) -> bool {
            tx >= 0 && tx < TW && ty >= 0 && ty < TH && self.on[(ty * TW + tx) as usize]
        }
        /// Mark the tile a pixel falls in (off-buffer pixels are ignored — the trail may run past
        /// the buffer edge, transparently).
        #[inline]
        fn set_px(&mut self, x: f32, y: f32) {
            if x >= 0.0 && y >= 0.0 && x < W as f32 && y < H as f32 {
                self.set_tile(x as i32 / TILE, y as i32 / TILE);
            }
        }
        /// Mark every tile overlapping a pixel-space box.
        fn set_box(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
            let tx0 = (x0.floor() as i32).div_euclid(TILE).max(0);
            let ty0 = (y0.floor() as i32).div_euclid(TILE).max(0);
            let tx1 = (x1.ceil() as i32).div_euclid(TILE).min(TW - 1);
            let ty1 = (y1.ceil() as i32).div_euclid(TILE).min(TH - 1);
            for ty in ty0..=ty1 {
                for tx in tx0..=tx1 {
                    self.set_tile(tx, ty);
                }
            }
        }
        /// March a pixel-space segment, marking the tiles it crosses (sampled finer than a tile so
        /// none is skipped; the later dilation bridges any sub-tile gap). This is how the trail and
        /// the ghost — both polylines — stamp their cover.
        fn set_seg(&mut self, x0: f32, y0: f32, x1: f32, y1: f32) {
            let d = ((x1 - x0).powi(2) + (y1 - y0).powi(2)).sqrt();
            let steps = (d / (TILE as f32 * 0.5)).ceil().max(1.0) as i32;
            for s in 0..=steps {
                let u = s as f32 / steps as f32;
                self.set_px(x0 + (x1 - x0) * u, y0 + (y1 - y0) * u);
            }
        }
        /// Mark tiles whose distance-from-anchor range overlaps the band `[r_in, r_out]`. A ring
        /// (the rune ring, the recognition flare) lives at a *radius*, not in a disc — only the thin
        /// band is content, so only it is dirtied (marking the whole disc would be O(reach²) again).
        fn set_ring(&mut self, r_in: f32, r_out: f32) {
            let half = (TILE as f32) * 0.5 * std::f32::consts::SQRT_2; // tile half-diagonal
            for ty in 0..TH {
                let dy = (ty * TILE) as f32 + TILE as f32 * 0.5 - CY;
                for tx in 0..TW {
                    let dx = (tx * TILE) as f32 + TILE as f32 * 0.5 - CX;
                    let d = (dx * dx + dy * dy).sqrt();
                    if d + half >= r_in && d - half <= r_out {
                        self.set_tile(tx, ty);
                    }
                }
            }
        }
        /// 8-neighbour dilation by `n` tiles — the margin that lets the passes read a few pixels past
        /// a content tile (aura's 3px blur, the composite's gradient + dispersion taps) and always
        /// land in a cleared-or-drawn tile, never stale memory.
        fn dilated(&self, n: i32) -> TileSet {
            let mut cur = self.clone();
            for _ in 0..n {
                let mut nxt = TileSet::new();
                for ty in 0..TH {
                    for tx in 0..TW {
                        if cur.get_tile(tx, ty) {
                            for dy in -1..=1 {
                                for dx in -1..=1 {
                                    nxt.set_tile(tx + dx, ty + dy);
                                }
                            }
                        }
                    }
                }
                cur = nxt;
            }
            cur
        }
        /// In-place union (this ∪ other).
        fn or_with(&mut self, other: &TileSet) {
            for (a, b) in self.on.iter_mut().zip(other.on.iter()) {
                *a |= *b;
            }
        }
        /// The clamped pixel box `[x0,x1) × [y0,y1)` of tile index `ti`.
        #[inline]
        fn tile_box(ti: usize) -> (i32, i32, i32, i32) {
            let tx = ti as i32 % TW;
            let ty = ti as i32 / TW;
            (
                (tx * TILE).max(0),
                (ty * TILE).max(0),
                ((tx + 1) * TILE).min(W),
                ((ty + 1) * TILE).min(H),
            )
        }
    }

    #[cfg(test)]
    mod tile_tests {
        use super::*;

        fn count(ts: &TileSet) -> usize {
            ts.on.iter().filter(|&&b| b).count()
        }

        #[test]
        fn box_marks_the_anchor_tile() {
            let mut ts = TileSet::new();
            ts.set_box(CX - 10.0, CY - 10.0, CX + 10.0, CY + 10.0);
            assert!(
                ts.get_tile(CX as i32 / TILE, CY as i32 / TILE),
                "the anchor tile must be marked"
            );
        }

        #[test]
        fn ring_marks_a_band_not_a_disc() {
            // THE load-bearing invariant: a ring at radius 600 must NOT fill the disc — its centre
            // stays empty and its cover is a small fraction of the filled disc. This is exactly the
            // O(reach) vs O(reach²) guarantee the whole change rests on (a rune ring is thin curves,
            // never a solid wheel of work).
            let mut ring = TileSet::new();
            ring.set_ring(596.0, 604.0);
            assert!(
                !ring.get_tile(CX as i32 / TILE, CY as i32 / TILE),
                "a ring must leave its centre empty"
            );
            let band = count(&ring);
            assert!(band > 0, "the ring must mark its band");
            let disc = (std::f32::consts::PI * 600.0 * 600.0 / (TILE * TILE) as f32) as usize;
            assert!(
                band * 3 < disc,
                "band {band} must be far below the filled disc {disc}"
            );
        }

        #[test]
        fn seg_marches_end_to_end_without_gaps() {
            // a stroke reaching 500px out marks both ends and every tile between (no skipped cell a
            // dilation would otherwise have to rescue) — the trail's cover is contiguous.
            let mut ts = TileSet::new();
            ts.set_seg(CX, CY, CX + 500.0, CY);
            let ty = CY as i32 / TILE;
            for tx in (CX as i32 / TILE)..=((CX + 500.0) as i32 / TILE) {
                assert!(ts.get_tile(tx, ty), "no gap at tile column {tx}");
            }
        }

        #[test]
        fn dilate_grows_and_union_merges() {
            let mut a = TileSet::new();
            a.set_px(CX, CY); // mid-buffer → all 8 neighbours are in-bounds
            assert_eq!(
                count(&a.dilated(1)),
                9,
                "8-neighbour dilation adds the surrounding ring"
            );
            let mut b = TileSet::new();
            b.set_px(CX + 400.0, CY);
            let mut u = a.clone();
            u.or_with(&b);
            assert_eq!(count(&u), count(&a) + count(&b), "disjoint union sums");
        }
    }

    enum Cmd {
        Begin(WeaveMode),
        Push(Vec<(f32, f32)>),
        Hint(Option<GlyphHint>),
        Recognized(bool),
        /// Update the live notification STACK in place (the engine's per-tick frame): the whole slot
        /// list + its presentation mode + the real placement `(nx, ny)` + the digest view. Unlike
        /// `Begin` this does NOT reset the frame clock or re-anchor at the cursor — the engine drives
        /// the animation and the overlay keeps its breath/topmost cadence, re-rasterizing a slot only
        /// when its `rev` moved.
        Stack(Vec<NotifySlot>, u8, (f32, f32), DigestView, u32),
        End,
        Quit,
    }

    /// Handle to the overlay's render thread. Created per-weave on the capture worker; dropping it
    /// tears the render thread + window down (after the fade, if you `end()` first).
    pub struct SpellOverlay {
        tx: Sender<Cmd>,
        handle: Option<JoinHandle<()>>,
    }

    impl SpellOverlay {
        pub fn spawn() -> Self {
            let (tx, rx) = channel::<Cmd>();
            let handle = std::thread::Builder::new()
                .name("neuron-spell-overlay".into())
                .spawn(move || render_thread(rx))
                .ok();
            SpellOverlay { tx, handle }
        }
        /// Show the sigil for `mode`, anchored at the current cursor; resets the stroke.
        pub fn begin(&self, mode: WeaveMode) {
            let _ = self.tx.send(Cmd::Begin(mode));
        }
        /// Feed the accumulated stroke (points relative to the anchor, in mouse units), MOVING the
        /// already-owned `Vec` straight across the `Cmd` channel — ONE allocation per tick. Every
        /// caller (the per-tick weave/teleport/ask drain, the editor recorder) builds its `rel`
        /// `Vec` once and hands it over by value, so the old per-tick double allocation — build
        /// then `.to_vec()` inside here — is gone.
        pub fn push(&self, pts: Vec<(f32, f32)>) {
            let _ = self.tx.send(Cmd::Push(pts));
        }
        /// Update the live next-glyph prediction (Glyph mode only) WITHOUT resetting the trail.
        pub fn hint(&self, hint: Option<GlyphHint>) {
            let _ = self.tx.send(Cmd::Hint(hint));
        }
        /// Flare on a recognized glyph / committed radial pick, or a soft fizzle on a miss.
        pub fn recognized(&self, hit: bool) {
            let _ = self.tx.send(Cmd::Recognized(hit));
        }
        /// Push one engine FRAME of the notification stack: the live slots (each with its animation
        /// state already eased), the presentation `mode` (0 stack / 1 latest / 2 digest), the user's
        /// REAL placement `(nx, ny)` fractions (NOT a snapped corner — the column block is centred on
        /// `nx` and grows from `ny`), and the digest view. Called ~60Hz by the engine while any note
        /// is alive; the overlay renders exactly this, re-rasterizing a slot's text only when `rev`
        /// changed.
        pub fn stack(
            &self,
            slots: Vec<NotifySlot>,
            mode: u8,
            place: (f32, f32),
            digest: DigestView,
            tail: u32,
        ) {
            let _ = self.tx.send(Cmd::Stack(slots, mode, place, digest, tail));
        }
        /// Begin the fade-out, then hide.
        pub fn end(&self) {
            let _ = self.tx.send(Cmd::End);
        }
    }

    impl Drop for SpellOverlay {
        fn drop(&mut self) {
            let _ = self.tx.send(Cmd::Quit);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    struct Particle {
        x: f32,
        y: f32,
        vx: f32,
        vy: f32,
        life: f32,
        white: bool, // flare embers burn bigger and hotter
        /// the spectral hue (degrees) this ember DIFFUSES into as its white heart cools —
        /// the prism: white → rainbow, quietly.
        hue: f32,
    }

    /// Dependency-free xorshift — reseeded per weave so the same stroke draws the same sigil.
    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> f32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            (x >> 8) as f32 / (1u32 << 24) as f32
        }
        fn signed(&mut self) -> f32 {
            self.next() * 2.0 - 1.0
        }
    }

    struct Buffers {
        glow: Vec<f32>,  // phosphor intensity
        white: Vec<f32>, // white-hot core / flare intensity
        warn: Vec<f32>,  // red intensity (an ask's NO half) — zero except in Ask mode
        // the PRISM: a full-RGB additive channel for spectral light (ember diffusion, the
        // chromatic fringe at the trail head). Separate from glow so the phosphor identity
        // stays pure and the rainbow stays a flair, never the body.
        pr: Vec<f32>,
        pg: Vec<f32>,
        pb: Vec<f32>,
        /// SHADE — per-pixel DARKENING (opacity without emission). Two sources feed it: the
        /// GATHERING DUSK (the room dims around a cast like smoke drawing in — feathered,
        /// noise-broken, breathing; never a hard-edged panel) and the AUTO-AURA (a blur of the
        /// emission itself, so every glowing thing casts its own shadow and stays legible over
        /// a white page without anyone drawing a backdrop).
        shade: Vec<f32>,
        /// scratch for the aura blur's horizontal pass
        blur_tmp: Vec<f32>,
    }
    impl Buffers {
        fn new() -> Self {
            let n = (W * H) as usize;
            Buffers {
                glow: vec![0.0; n],
                white: vec![0.0; n],
                warn: vec![0.0; n],
                pr: vec![0.0; n],
                pg: vec![0.0; n],
                pb: vec![0.0; n],
                shade: vec![0.0; n],
                blur_tmp: vec![0.0; n],
            }
        }
        /// Zero every emission/shade channel within a box only — the dirty-rect clear. The big
        /// buffer makes a full clear wasteful, so we wipe just the region we'll redraw (the caller
        /// passes the UNION of this frame's content box and the last frame's, so moving/shrinking
        /// content erases cleanly without ghosts).
        fn clear_box(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
            let x0 = x0.max(0);
            let y0 = y0.max(0);
            let x1 = x1.min(W);
            let y1 = y1.min(H);
            for yy in y0..y1 {
                let a = (yy * W + x0) as usize;
                let b = (yy * W + x1) as usize;
                self.glow[a..b].fill(0.0);
                self.white[a..b].fill(0.0);
                self.warn[a..b].fill(0.0);
                self.pr[a..b].fill(0.0);
                self.pg[a..b].fill(0.0);
                self.pb[a..b].fill(0.0);
                self.shade[a..b].fill(0.0);
            }
        }

        /// The GATHERING DUSK: the world dims around the anchor while a weave is alive — wide
        /// feather (no edge to see), broken by slow smoke-noise so it never reads as geometry,
        /// breathing gently with the frame clock. Magic, not glass.
        #[allow(clippy::too_many_arguments)] // a render pass's natural arity (the dirty box)
        fn dusk(
            &mut self,
            radius: f32,
            strength: f32,
            frame: u32,
            x0: i32,
            y0: i32,
            x1: i32,
            y1: i32,
        ) {
            let breathe = 0.92 + 0.08 * ((frame as f32) * 0.045).sin();
            let r2 = radius * radius;
            for yy in y0.max(0)..y1.min(H) {
                let dy = yy as f32 - CY;
                for xx in x0.max(0)..x1.min(W) {
                    let dx = xx as f32 - CX;
                    let q = (dx * dx + dy * dy) / r2;
                    if q >= 1.0 {
                        continue;
                    }
                    let t = 1.0 - q;
                    // smoke: low-frequency hash noise keeps the dusk organic
                    let n = 0.82 + 0.18 * hnoise(xx >> 4, yy >> 4, frame >> 5);
                    self.shade[(yy * W + xx) as usize] += strength * t * t * n * breathe;
                }
            }
        }

        /// The AUTO-AURA, pass 1 — cache the horizontal box-average (radius 3) of emission luminance
        /// into `blur_tmp`, over the given tiles. Neighbour reads clamp to the buffer; the cleared
        /// moat around the processed tiles makes every out-of-content read a true zero, so this is
        /// identical to the old whole-box blur's horizontal pass — just scoped to the dirty cover.
        fn aura_h(&mut self, set: &TileSet) {
            const R: i32 = 3;
            let norm = 1.0 / (2 * R + 1) as f32;
            for ti in 0..(TW * TH) as usize {
                if !set.on[ti] {
                    continue;
                }
                let (x0, y0, x1, y1) = TileSet::tile_box(ti);
                for yy in y0..y1 {
                    let row = (yy * W) as usize;
                    // prime the window at the tile's left edge, then SLIDE across it — O(1) per
                    // pixel (two lum reads), exactly the old separable box blur, not a 7-tap resum.
                    let mut acc = 0.0f32;
                    for k in -R..=R {
                        acc += self.lum_at(row + (x0 + k).clamp(0, W - 1) as usize);
                    }
                    for xx in x0..x1 {
                        self.blur_tmp[row + xx as usize] = acc * norm;
                        let add = (xx + R + 1).clamp(0, W - 1) as usize;
                        let sub = (xx - R).clamp(0, W - 1) as usize;
                        acc += self.lum_at(row + add) - self.lum_at(row + sub);
                    }
                }
            }
        }
        /// The AUTO-AURA, pass 2 — box-average `blur_tmp` vertically (radius 3) and add the soft
        /// halo into shade, over the given tiles. Composed with [`aura_h`] this is the exact 7×7 box
        /// blur of emission the old `aura` produced (×1.1, clamped to 0.7), confined to the cover.
        fn aura_v(&mut self, set: &TileSet) {
            const R: i32 = 3;
            let norm = 1.0 / (2 * R + 1) as f32;
            for ti in 0..(TW * TH) as usize {
                if !set.on[ti] {
                    continue;
                }
                let (x0, y0, x1, y1) = TileSet::tile_box(ti);
                for xx in x0..x1 {
                    let xu = xx as usize;
                    // prime + slide DOWN the tile column — again O(1) per pixel.
                    let mut acc = 0.0f32;
                    for k in -R..=R {
                        acc += self.blur_tmp[(y0 + k).clamp(0, H - 1) as usize * W as usize + xu];
                    }
                    for yy in y0..y1 {
                        let v = acc * norm;
                        if v > 0.003 {
                            self.shade[yy as usize * W as usize + xu] += (v * 1.1).min(0.7);
                        }
                        let add = (yy + R + 1).clamp(0, H - 1) as usize;
                        let sub = (yy - R).clamp(0, H - 1) as usize;
                        acc += self.blur_tmp[add * W as usize + xu]
                            - self.blur_tmp[sub * W as usize + xu];
                    }
                }
            }
        }

        #[inline]
        fn lum_at(&self, i: usize) -> f32 {
            self.glow[i]
                + self.white[i]
                + self.warn[i]
                + (self.pr[i] + self.pg[i] + self.pb[i]) * 0.5
        }
    }

    /// Tiny deterministic hash noise in [0,1) for the dusk's smoke.
    fn hnoise(x: i32, y: i32, t: u32) -> f32 {
        let mut n = (x as u32).wrapping_mul(0x9E37_79B1)
            ^ (y as u32).wrapping_mul(0x85EB_CA6B)
            ^ t.wrapping_mul(0xC2B2_AE35);
        n ^= n >> 15;
        n = n.wrapping_mul(0x2C1B_3C6D);
        n ^= n >> 12;
        (n & 0xFFFF) as f32 / 65536.0
    }

    /// Splat spectral light into the prism channel (rgb in 0..=1).
    fn splat_prism(
        buf: &mut Buffers,
        cx: f32,
        cy: f32,
        radius: f32,
        bright: f32,
        rgb: (f32, f32, f32),
    ) {
        splat(&mut buf.pr, cx, cy, radius, bright * rgb.0);
        splat(&mut buf.pg, cx, cy, radius, bright * rgb.1);
        splat(&mut buf.pb, cx, cy, radius, bright * rgb.2);
    }

    /// The cool and warm poles of the prismatic refraction fringe — the chromatic split that
    /// reads as bent hard light (the same fringe `neuron::scene` paints in SVG).
    const REFRACT_COOL: (f32, f32, f32) = (0.36, 0.94, 1.0);
    const REFRACT_WARM: (f32, f32, f32) = (1.0, 0.42, 0.84);

    /// A HARD-LIGHT CONSTRUCT — Neuron's school of magic, Symmetra-style: a faceted polygon of
    /// light with crisp edges, a prismatic refraction fringe (cool + warm offset), and bright
    /// crystalline vertex nodes. `blueprint` draws the *unbuilt* wireframe (dashed, no nodes) —
    /// the open beat you answer. `sides` is the facet count; the hand's force builds the shape.
    #[allow(clippy::too_many_arguments)]
    fn hard_construct(
        buf: &mut Buffers,
        cx: f32,
        cy: f32,
        r: f32,
        sides: u32,
        rot: f32,
        rgb: (f32, f32, f32),
        bright: f32,
        blueprint: bool,
    ) {
        let n = sides.max(3);
        let verts: Vec<(f32, f32)> = (0..n)
            .map(|i| {
                let a = rot + TAU * i as f32 / n as f32;
                (cx + a.cos() * r, cy + a.sin() * r)
            })
            .collect();
        // edges, walked as short segments so the light is a crisp built line, not a blob.
        for i in 0..n as usize {
            let (x0, y0) = verts[i];
            let (x1, y1) = verts[(i + 1) % n as usize];
            let seglen = (x1 - x0).hypot(y1 - y0).max(1.0);
            let steps = (seglen / 3.0).ceil() as i32;
            for s2 in 0..=steps {
                if blueprint && (s2 / 2) % 2 == 1 {
                    continue; // dashed wireframe
                }
                let t = s2 as f32 / steps as f32;
                let x = x0 + (x1 - x0) * t;
                let y = y0 + (y1 - y0) * t;
                // prismatic fringe: cool and warm offset a hair off the base edge
                splat_prism(buf, x - 1.0, y - 0.5, 1.7, bright * 0.26, REFRACT_COOL);
                splat_prism(buf, x + 1.0, y + 0.5, 1.7, bright * 0.26, REFRACT_WARM);
                splat_prism(buf, x, y, 2.0, bright * 0.55, rgb);
            }
        }
        if blueprint {
            splat_prism(buf, cx, cy, 2.0, bright * 0.6, rgb); // centre tick: your turn
        } else {
            // crystalline vertex nodes + a faint inner lattice polygon (the constructed interior)
            for (x, y) in &verts {
                splat_white(&mut buf.white, *x, *y, 2.2, bright * 0.7);
            }
            let ir = r * 0.5;
            for i in 0..n as usize {
                let a0 = rot + TAU * i as f32 / n as f32;
                let a1 = rot + TAU * (i + 1) as f32 / n as f32;
                let (px0, py0) = (cx + a0.cos() * ir, cy + a0.sin() * ir);
                let (px1, py1) = (cx + a1.cos() * ir, cy + a1.sin() * ir);
                let mid = ((px0 + px1) * 0.5, (py0 + py1) * 0.5);
                splat_prism(buf, mid.0, mid.1, 1.6, bright * 0.22, rgb);
            }
        }
    }

    /// HSV → linear-ish rgb in 0..=1 (h in degrees) — the prism's little spectrum.
    fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
        let h = h.rem_euclid(360.0) / 60.0;
        let c = v * s;
        let x = c * (1.0 - (h % 2.0 - 1.0).abs());
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        (r + m, g + m, b + m)
    }

    /// A text raster: GDI-drawn glyphs reduced to a float intensity mask, so the question (and the
    /// YES/NO captions) composite into the additive glow exactly like every other element — no
    /// foreign-looking solid text box on top of the sigil.
    struct TextRaster {
        w: i32,
        h: i32,
        mask: Vec<f32>,
    }

    /// Rasterize one line of text via GDI into an intensity mask (Consolas — the instrument face).
    /// Returns None on any GDI failure (the overlay then simply shows no caption — never crashes).
    unsafe fn rasterize_text(text: &str, px: i32) -> Option<TextRaster> {
        if text.is_empty() {
            return None;
        }
        let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
        let wide: Vec<u16> = text.encode_utf16().collect();
        let dc = CreateCompatibleDC(std::ptr::null_mut());
        if dc.is_null() {
            return None;
        }
        let font = CreateFontW(
            -px,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            CLEARTYPE_QUALITY as u32,
            0,
            face.as_ptr(),
        );
        if font.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old_font = SelectObject(dc, font as _);
        let mut size = SIZE { cx: 0, cy: 0 };
        if GetTextExtentPoint32W(dc, wide.as_ptr(), wide.len() as i32, &mut size) == 0
            || size.cx <= 0
            || size.cy <= 0
        {
            SelectObject(dc, old_font);
            DeleteObject(font as _);
            DeleteDC(dc);
            return None;
        }
        let (tw, th) = (size.cx.min(W - 8), size.cy);
        let mut bmi = dib_header();
        bmi.bmiHeader.biWidth = tw;
        bmi.bmiHeader.biHeight = -th;
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0)
            as HBITMAP;
        if dib.is_null() || bits.is_null() {
            SelectObject(dc, old_font);
            DeleteObject(font as _);
            DeleteDC(dc);
            return None;
        }
        let old_bmp = SelectObject(dc, dib as _);
        let px_buf = bits as *mut u32;
        let n = (tw * th) as usize;
        for i in 0..n {
            *px_buf.add(i) = 0; // DIB memory isn't guaranteed zeroed
        }
        SetTextColor(dc, 0x00FF_FFFF);
        SetBkMode(dc, TRANSPARENT as i32);
        TextOutW(dc, 0, 0, wide.as_ptr(), wide.len() as i32);
        let mut mask = vec![0.0f32; n];
        for (i, m) in mask.iter_mut().enumerate() {
            // any channel works (white text); green channel as the intensity.
            *m = ((*px_buf.add(i) >> 8) & 0xFF) as f32 / 255.0;
        }
        SelectObject(dc, old_bmp);
        SelectObject(dc, old_font);
        DeleteObject(dib as _);
        DeleteObject(font as _);
        DeleteDC(dc);
        Some(TextRaster { w: tw, h: th, mask })
    }

    /// Additively blit a text mask into a float buffer, centered at (cx, cy).
    fn blit_mask(buf: &mut [f32], t: &TextRaster, cx: f32, cy: f32, gain: f32) {
        let x0 = (cx - t.w as f32 / 2.0).round() as i32;
        let y0 = (cy - t.h as f32 / 2.0).round() as i32;
        for yy in 0..t.h {
            let dy = y0 + yy;
            if !(0..H).contains(&dy) {
                continue;
            }
            for xx in 0..t.w {
                let dx = x0 + xx;
                if !(0..W).contains(&dx) {
                    continue;
                }
                buf[(dy * W + dx) as usize] += t.mask[(yy * t.w + xx) as usize] * gain;
            }
        }
    }

    fn render_thread(rx: std::sync::mpsc::Receiver<Cmd>) {
        unsafe {
            // the overlay's layered window + its W×H top-down BGRA DIB live in one LayeredSurface;
            // the bespoke render loop (dirty-tile compositing, the per-frame present, the
            // NOTOPMOST→TOPMOST topmost flip) stays exactly as it was below — only the
            // window/DIB/present plumbing moved into the seam.
            let surf = match crate::surface::LayeredSurface::new(&crate::surface::SurfaceSpec::new(
                "NeuronSpellOverlay",
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE
                    | WS_EX_TOOLWINDOW,
                W,
                H,
            )) {
                Some(s) => s,
                None => return,
            };
            let hwnd = surf.hwnd();
            let px = surf.bits();

            let mut buf = Buffers::new();
            let mut points: Vec<(f32, f32)> = Vec::new();
            let mut particles: Vec<Particle> = Vec::new();
            let mut rng = Rng(0x9E3779B9);
            let mut mode = WeaveMode::Glyph { hint: None };
            // ask/signal captions, rasterized once per Begin (the question + the YES/NO/PASS rims).
            let mut ask_label: Option<TextRaster> = None;
            let mut ask_detail: Option<TextRaster> = None; // optional context under the wheel
            let mut ask_pass: Option<TextRaster> = None;
            // one rasterized label per option wedge (yes/no, or the N choose labels) — the wheel is data.
            let mut ask_opts: Vec<Option<TextRaster>> = Vec::new();
            // the MULTI-notification STACK: the live cards (rasters cached by ident+rev across the
            // engine's 60fps pushes), the presentation mode (0 stack / 1 latest / 2 digest), the
            // corner anchor, and the rasterized digest summary. Driven by `Cmd::Stack`, NOT `Begin`,
            // so the engine owns the animation and the overlay keeps its own clock.
            let mut stack_cards: Vec<NotifyR> = Vec::new();
            let mut stack_mode_kind: u8 = 0;
            // the user's REAL placement fractions (nx, ny) — the column is centred on nx (clamped
            // on-screen) and grows from ny (top half → down, bottom half → up). NOT a snapped corner.
            let mut stack_place: (f32, f32) = (0.5, 0.0);
            let mut stack_digest: Option<DigestR> = None;
            let mut stack_tail: u32 = 0; // Stack-mode overflow count (the "+N more" tail)
            // the wheel's live instrument cards (icon + value + title), rasterized once per Begin
            let mut wedge_views: Vec<WedgeR> = Vec::new();
            // the second tier: per-wedge fan option rasters (index = sector; empty = no fan)
            let mut fan_labels: Vec<Vec<FanR>> = Vec::new();
            let mut dial_value: Option<TextRaster> = None; // the dial's reading ("62%")
            let mut dial_device: Option<TextRaster> = None; // the endpoint name ("Headset")
                                                            // the control center's three glance cards (net / output / bluetooth) + the SSID line
                                                            // that rides under the network card, rasterized once per Begin.
            let mut ctl_cards: Vec<(WedgeR, f32, f32)> = Vec::new(); // (card, dx, dy) from centre
            let mut ctl_ssid: Option<TextRaster> = None;
            let mut glyph_hint_title: Option<TextRaster> = None; // the live prediction's name
                                                                 // KNOCKBACK's quiet caption — cached by string (the session re-begins ~60Hz while
                                                                 // animating; re-rasterizing an unchanged hint every frame would be waste).
            let mut twin_hint: Option<TextRaster> = None;
            let mut twin_hint_str = String::new();
            // KNOCKBACK's stage clock: advances every visible frame, NEVER resets on re-begin
            // (a same-kind Begin zeroes `frame`, which would freeze breathing), and slows under
            // stillpoint dilation — time dilates visually, the music never does.
            let mut twin_clock: f32 = 0.0;
            let mut visible = false;
            let mut fading = false;
            let mut fade = 0f32;
            let mut fade_started = std::time::Instant::now(); // wall-clock start of the current fade
            // DIRTY TILES: the PROCESSED cover from LAST frame. Each frame clears/composites the
            // union of it and this frame's cover, so content that moved or shrank erases with no
            // ghost (the per-tile heir of the old dirty rect). `full_redraw` forces a whole-buffer
            // frame on first paint / a mode change, so a stale prior weave is wiped exactly once.
            let mut prev_proc = TileSet::new();
            let mut full_redraw = true;
            let mut flare = 0f32; // recognition flash 0..1
            let mut flare_ring = 0f32; // expanding-ring progress 0..1 (>0 while animating)
            let mut charge = 0f32; // anchor charge-up 0..1
            let mut frame: u32 = 0;
            // the window's top-left on screen — cursor-anchored but CLAMPED to the cursor's
            // monitor (a weave near a screen edge must stay fully on-glass, never half-off).
            let mut origin = POINT { x: 0, y: 0 };
            let mut emit = 0f32;
            // optional frame-time probe — set NEURON_OVERLAY_PROFILE to print per-frame render ms
            // every 60 frames. Off by default; measure, don't guess.
            let profile = std::env::var_os("NEURON_OVERLAY_PROFILE").is_some();
            // the slow-frame diagnostic gate (NEURON_PROFILE) — cached once so neither the per-frame
            // `dbg_cover` tile scan nor the slow-frame eprintln pays a per-frame env lookup.
            let slow_profile = std::env::var_os("NEURON_PROFILE").is_some();
            let (mut prof_us, mut prof_max, mut prof_n) = (0u128, 0u128, 0u32);

            loop {
                neuron::prof::bump(&neuron::prof::OVERLAY_FRAME);
                let frame_start = std::time::Instant::now();
                let mut dbg_cover = 0usize; // tiles composited this frame (slow-frame diagnostic)
                let mut dbg_painted = 0usize; // pixels that hit the FULL material pipeline this frame
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, hwnd, 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                let mut quit = false;
                while let Ok(cmd) = rx.try_recv() {
                    match cmd {
                        Cmd::Begin(m) => {
                            // ask/signal modes carry captions: rasterize once, composite per frame.
                            ask_label = None;
                            ask_detail = None;
                            ask_pass = None;
                            ask_opts.clear();
                            wedge_views.clear();
                            fan_labels.clear();
                            dial_value = None;
                            dial_device = None;
                            ctl_cards.clear();
                            ctl_ssid = None;
                            glyph_hint_title = None;
                            match &m {
                                WeaveMode::Ask {
                                    label,
                                    options,
                                    detail,
                                } => {
                                    ask_label = rasterize_text(&ellipsize(label, 52), 17);
                                    if !detail.trim().is_empty() {
                                        ask_detail = rasterize_text(&ellipsize(detail, 72), 12);
                                    }
                                    ask_pass = rasterize_text("pass", 12);
                                    // scale the per-label char budget to the wheel — fewer chars for
                                    // more wedges — so adjacent labels never overlap around the ring
                                    // (chord between neighbours = 2·r·sin(π/n) at the label radius).
                                    let n = options.len().max(1);
                                    let budget = if n <= 1 {
                                        16
                                    } else {
                                        ((2.0 * 93.0 * (std::f64::consts::PI / n as f64).sin() / 8.0)
                                            as usize)
                                            .clamp(5, 16)
                                    };
                                    ask_opts = options
                                        .iter()
                                        .map(|o| rasterize_text(&ellipsize(o, budget), 13))
                                        .collect();
                                }
                                WeaveMode::Radial { widgets, fans, .. } => {
                                    // each wedge → a render-ready card (icon kept procedural; the
                                    // title + the big live value are rasterized text masks).
                                    for v in widgets {
                                        wedge_views.push(WedgeR {
                                            glyph: v.glyph,
                                            tone: v.tone,
                                            meter: v.meter,
                                            title: rasterize_text(&ellipsize(&v.title, 14), 11),
                                            value: v.value.as_ref().and_then(|s| {
                                                rasterize_text(&ellipsize(s, 10), 15)
                                            }),
                                        });
                                    }
                                    // rasterize every wedge's fan options (most are empty); the
                                    // render shows the live wedge's fan when the stroke reaches out.
                                    for opts in fans {
                                        fan_labels.push(
                                            opts.iter()
                                                .map(|o| FanR {
                                                    label: rasterize_text(
                                                        &ellipsize(&o.label, 16),
                                                        12,
                                                    ),
                                                    active: o.active,
                                                })
                                                .collect(),
                                        );
                                    }
                                }
                                WeaveMode::Dial { value, device, .. } => {
                                    dial_value = rasterize_text(value, 21);
                                    dial_device = rasterize_text(&ellipsize(device, 16), 12);
                                }
                                WeaveMode::Control {
                                    link,
                                    net,
                                    ssid,
                                    out,
                                    out_fill,
                                    bt,
                                    bt_on,
                                } => {
                                    // three glance cards arranged around the centre. NETWORK (north)
                                    // wears the wifi/ethernet icon + tone; OUTPUT (west) shows the
                                    // device + a level meter; BLUETOOTH (east) is the toggle seam.
                                    let net_glyph = match link {
                                        2 => WedgeGlyph::Network,  // wifi
                                        1 => WedgeGlyph::Ethernet, // the wire
                                        _ => WedgeGlyph::Network,
                                    };
                                    let net_tone = if *link == 0 { Tone::Off } else { Tone::Live };
                                    let card =
                                        |glyph, tone, title: &str, value: &str, meter| WedgeR {
                                            glyph,
                                            tone,
                                            meter,
                                            title: rasterize_text(&ellipsize(title, 14), 11),
                                            value: rasterize_text(&ellipsize(value, 12), 15),
                                        };
                                    ctl_cards.push((
                                        card(net_glyph, net_tone, "network", net, None),
                                        0.0,
                                        -62.0,
                                    ));
                                    ctl_cards.push((
                                        card(
                                            WedgeGlyph::Speaker,
                                            Tone::Live,
                                            "output",
                                            out,
                                            Some(*out_fill),
                                        ),
                                        -72.0,
                                        26.0,
                                    ));
                                    ctl_cards.push((
                                        card(
                                            WedgeGlyph::Bluetooth,
                                            if *bt_on { Tone::Active } else { Tone::Inert },
                                            "bluetooth",
                                            bt,
                                            None,
                                        ),
                                        72.0,
                                        26.0,
                                    ));
                                    // the SSID rides under the network card so "am i on the right
                                    // network?" reads without a flick (empty on ethernet/offline).
                                    if !ssid.is_empty() {
                                        ctl_ssid = rasterize_text(&ellipsize(ssid, 20), 12);
                                    }
                                }
                                WeaveMode::Glyph { hint: Some(h) } => {
                                    glyph_hint_title =
                                        rasterize_text(&ellipsize(&h.view.title, 18), 13);
                                }
                                WeaveMode::Twin { hint, .. } if *hint != twin_hint_str => {
                                    twin_hint_str = hint.clone();
                                    twin_hint = if hint.is_empty() {
                                        None
                                    } else {
                                        rasterize_text(&ellipsize(hint, 52), 13)
                                    };
                                }
                                _ => {}
                            }
                            // PINNED RE-BEGIN: a same-kind begin while visible (the teleport map
                            // re-lighting its hot blob, a strip refresh) keeps its place — the
                            // surface must never wander mid-gesture. Only a NEW kind (or a fresh
                            // appearance) re-anchors at the cursor.
                            let same_kind = visible
                                && !fading
                                && std::mem::discriminant(&m) == std::mem::discriminant(&mode);
                            mode = m;
                            if !same_kind {
                                let mut cur = POINT { x: 0, y: 0 };
                                GetCursorPos(&mut cur);
                                origin = place_window(&mode, cur);
                                // move, then force to the VERY TOP of the topmost band (above the
                                // taskbar/shell): a plain HWND_TOPMOST on an already-topmost window
                                // won't reorder it above other topmost windows, so drop topmost and
                                // re-assert — the flip re-inserts at the front of the band.
                                SetWindowPos(
                                    hwnd,
                                    HWND_NOTOPMOST,
                                    origin.x,
                                    origin.y,
                                    W,
                                    H,
                                    SWP_NOACTIVATE,
                                );
                                SetWindowPos(
                                    hwnd,
                                    HWND_TOPMOST,
                                    0,
                                    0,
                                    0,
                                    0,
                                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                );
                                // a fresh surface (or a mode swap) repaints the whole buffer once, so
                                // the previous weave's pixels can't linger in untouched tiles.
                                full_redraw = true;
                            }
                            points.clear();
                            particles.clear();
                            rng = Rng(0x9E3779B9); // deterministic per weave
                            flare = 0.0;
                            flare_ring = 0.0;
                            charge = 0.0;
                            frame = 0;
                            fade = 1.0;
                            fading = false;
                            visible = true;
                            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                        }
                        // Sets ONLY the trail points — deliberately does NOT touch the live glyph
                        // hint. beacon's predict-throttle relies on the last `Cmd::Hint` persisting
                        // across the Pushes between recomputes; clearing it here would make the
                        // forecast flicker/stale. (`Cmd::Hint` updates it; `Cmd::Begin` resets it.)
                        Cmd::Push(p) => points = p,
                        Cmd::Hint(h) => {
                            // update the live prediction in place (Glyph mode) — never touches the
                            // trail, so the comet keeps flowing while the forecast firms up.
                            glyph_hint_title = h
                                .as_ref()
                                .and_then(|x| rasterize_text(&ellipsize(&x.view.title, 18), 13));
                            if let WeaveMode::Glyph { hint } = &mut mode {
                                *hint = h;
                            }
                        }
                        Cmd::Recognized(hit) => {
                            if hit {
                                flare = 1.0;
                                flare_ring = 0.001; // kick the expanding ring
                                let (mx, my) = points.last().copied().unwrap_or((0.0, 0.0));
                                for k in 0..32 {
                                    let a = (k as f32 / 32.0) * TAU;
                                    let sp = 2.2 + rng.next() * 1.8;
                                    particles.push(Particle {
                                        x: mx,
                                        y: my,
                                        vx: a.cos() * sp,
                                        vy: a.sin() * sp,
                                        life: 1.0,
                                        white: true,
                                        // the burst is a spectral RING: each ember diffuses into
                                        // the hue of its bearing — white light through a prism.
                                        hue: (k as f32 / 32.0) * 360.0,
                                    });
                                }
                            } else {
                                flare = 0.3;
                            }
                        }
                        Cmd::Stack(slots, smode, place, digest, tail) => {
                            // ONE engine frame of the notification stack. Reconcile the rasters
                            // against the new slot list — reuse a cached card when its ident+rev are
                            // unchanged (re-cut text only when the engine bumped `rev`), so the 60fps
                            // push costs no rasterization once content is steady. The animation
                            // (y/alpha/scale/collapse/bump) is copied EVERY push — it changes per
                            // frame, the text does not.
                            let mut next: Vec<NotifyR> = Vec::with_capacity(slots.len());
                            for s in &slots {
                                // find a reusable cached card (same ident, same rev)
                                let reuse_pos = stack_cards
                                    .iter()
                                    .position(|c| c.ident == s.ident && c.rev == s.rev);
                                let mut card = if let Some(p) = reuse_pos {
                                    stack_cards.swap_remove(p) // take ownership of the cached rasters
                                } else {
                                    // new ident OR changed content → rasterize. value LARGE (22px,
                                    // the headline), title small (13px), the body wrapped to ≤3 rows
                                    // (15px) so a meatier line breathes, prev tiny (11px).
                                    // ASK card: the question is the PROMINENT line (the value, drawn
                                    // big) and the grammar is its own legible row (carried in `prev`,
                                    // wider ellipsis since it's a full grammar line, not a tiny "was
                                    // X"). The body wrap is skipped for an ask (its layout is fixed:
                                    // question row + grammar row), so an ask card is a clean 2-row
                                    // prompt, not a 3-line value block.
                                    let is_ask = s.is_ask;
                                    NotifyR {
                                        ident: s.ident.clone(),
                                        rev: s.rev,
                                        glyph: s.glyph,
                                        dir: s.dir,
                                        fill: s.fill,
                                        prev_fill: s.prev_fill,
                                        panel: s.panel,
                                        is_ask,
                                        title: rasterize_text(&ellipsize(&s.title, 24), 13),
                                        value: rasterize_text(
                                            &ellipsize(&s.value, if is_ask { 26 } else { 18 }),
                                            if is_ask { 16 } else { 22 },
                                        ),
                                        body: if is_ask {
                                            // the answer GRAMMAR, WRAPPED to lines that fit the card
                                            // width — never spilling past the edge — each its own row.
                                            wrap_lines(&s.prev, 30, 3)
                                                .iter()
                                                .filter_map(|l| rasterize_text(l, 13))
                                                .collect()
                                        } else {
                                            wrap_lines(&s.value, 28, 3)
                                                .iter()
                                                .filter_map(|l| rasterize_text(l, 15))
                                                .collect()
                                        },
                                        prev: if is_ask || s.prev.is_empty() {
                                            None
                                        } else {
                                            rasterize_text(&ellipsize(&s.prev, 24), 11)
                                        },
                                        y: s.y,
                                        alpha: s.alpha,
                                        scale: s.scale,
                                        collapse: s.collapse,
                                        bump: s.bump,
                                    }
                                };
                                // refresh the per-frame draw + animation fields (cheap copies)
                                card.glyph = s.glyph;
                                card.dir = s.dir;
                                card.fill = s.fill;
                                card.prev_fill = s.prev_fill;
                                card.panel = s.panel;
                                card.is_ask = s.is_ask;
                                card.y = s.y;
                                card.alpha = s.alpha;
                                card.scale = s.scale;
                                card.collapse = s.collapse;
                                card.bump = s.bump;
                                next.push(card);
                            }
                            stack_cards = next;
                            stack_mode_kind = smode;
                            stack_place = place;
                            stack_tail = tail;
                            // the digest summary — re-rasterize the count/line only when they change.
                            stack_digest = if digest.count == 0 {
                                None
                            } else {
                                let keep = stack_digest.as_ref().is_some_and(|d| {
                                    d.count == digest.count
                                        && d.glyphs == digest.glyphs
                                });
                                let reuse = if keep { stack_digest.take() } else { None };
                                let (count_txt, title, value) = match reuse {
                                    Some(d) => (d.count_txt, d.title, d.value),
                                    None => (
                                        rasterize_text(&format!("{}", digest.count), 30),
                                        rasterize_text(&ellipsize(&digest.title, 24), 13),
                                        rasterize_text(&ellipsize(&digest.value, 22), 18),
                                    ),
                                };
                                Some(DigestR {
                                    count: digest.count,
                                    glyphs: digest.glyphs.clone(),
                                    count_txt,
                                    title,
                                    value,
                                    alpha: digest.alpha,
                                    reveal: digest.reveal,
                                })
                            };
                            // the window must SIZE to the stack's bounding box and sit at the chosen
                            // monitor corner; recompute the origin and only move when it changed (so
                            // a steady stack doesn't thrash SetWindowPos every frame).
                            let mut cur = POINT { x: 0, y: 0 };
                            GetCursorPos(&mut cur);
                            let new_origin = stack_origin(place, cur);
                            let switched = !matches!(mode, WeaveMode::NotifyStack);
                            mode = WeaveMode::NotifyStack;
                            if switched || new_origin.x != origin.x || new_origin.y != origin.y {
                                origin = new_origin;
                                SetWindowPos(
                                    hwnd, HWND_NOTOPMOST, origin.x, origin.y, W, H, SWP_NOACTIVATE,
                                );
                                SetWindowPos(
                                    hwnd, HWND_TOPMOST, 0, 0, 0, 0,
                                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                );
                                full_redraw = true;
                            }
                            if !visible {
                                visible = true;
                                fade = 1.0;
                                fading = false;
                                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                            }
                        }
                        Cmd::End => {
                            // start the fade's wall clock ONCE (a repeated End mid-fade must not
                            // restart it, or the fade would never complete), and KILL the recognition
                            // flare ring so it can't keep expanding (and ballooning the dirty cover)
                            // during the fade — the post-beacon "radial never recovers" lag.
                            if !fading {
                                fade_started = std::time::Instant::now();
                            }
                            fading = true;
                            flare_ring = 0.0;
                        }
                        Cmd::Quit => {
                            quit = true;
                            break;
                        }
                    }
                }
                if quit {
                    break;
                }

                if visible {
                    frame = frame.wrapping_add(1);
                    // the stage clock: survives re-begins; a stillpoint dilates it (visuals
                    // breathe slower while the rhythm itself stays true).
                    if let WeaveMode::Twin { dilate, .. } = &mode {
                        twin_clock += 1.0 - 0.62 * dilate.clamp(0.0, 1.0);
                    }
                    if fading {
                        // exponential ease-out: the sigil lingers faintly then sublimates, instead of
                        // a linear ramp that snaps off at the end. With the frozen early-out + the
                        // grain-damp below, the dissolve reads smooth, not dithered.
                        //
                        // WALL-CLOCK, not per-frame: the decay is keyed to REAL elapsed time (×0.82
                        // every 16ms), so a slow render frame can't stretch the fade. The old
                        // `fade *= 0.82` per frame meant a heavy frame (e.g. an expanding flare ring
                        // ballooning the cover to ~150–520ms) made the ~22-frame fade take SECONDS —
                        // the "radial never recovers after a beacon" lag. Time-keyed, the fade always
                        // completes in ~0.35s no matter how slow each frame is.
                        fade = 0.82_f32.powf(fade_started.elapsed().as_secs_f32() * 1000.0 / 16.0);
                        if fade <= 0.012 {
                            fade = 0.0;
                            visible = false;
                            ShowWindow(hwnd, SW_HIDE);
                        }
                    } else {
                        charge = (charge + 0.08).min(1.0);
                    }
                    flare = (flare - 0.045).max(0.0);
                    // the recognition flare ring must NOT keep expanding while FADING OUT: its
                    // set_ring band grows with radius, inflating the dirty cover (484→~1376 tiles) and
                    // making every fade frame brutal. A beacon answer fires recognized()+end() back to
                    // back, so without this guard the whole 0→1 sweep lands during the fade. (End also
                    // zeroes flare_ring; this is the belt to that suspenders.)
                    if !fading && flare_ring > 0.0 {
                        flare_ring = (flare_ring + 0.09).min(1.0);
                        if flare_ring >= 1.0 {
                            flare_ring = 0.0;
                        }
                    }

                    // embers off the live trail head while drawing
                    if !fading {
                        if let Some(&(tx, ty)) = points.last() {
                            emit += 1.0;
                            while emit >= 1.0 {
                                emit -= 1.0;
                                particles.push(Particle {
                                    x: tx,
                                    y: ty,
                                    vx: rng.signed() * 0.7,
                                    vy: rng.signed() * 0.7 - 0.25,
                                    life: 0.6 + rng.next() * 0.35,
                                    white: false,
                                    hue: rng.next() * 360.0,
                                });
                            }
                        }
                    }
                    for p in particles.iter_mut() {
                        p.x += p.vx;
                        p.y += p.vy;
                        p.vy += 0.035;
                        p.vx *= 0.965;
                        p.vy *= 0.965;
                        p.life -= 0.028;
                    }
                    particles.retain(|p| p.life > 0.0);

                    // ── DIRTY TILES: mark the cover of THIS frame's content from its GEOMETRY, so the
                    // costly passes touch only what's drawn (see `TileSet`). The fixed footprint holds
                    // every anchor-centred element (wheel / dial / cards / dusk / charge / normal rune
                    // ring); the marched stroke + its ghost + the rune-ring bands + embers are the only
                    // things that run far — and they're CURVES, so their cover is O(stroke length).
                    let mut lit = TileSet::new();
                    if matches!(mode, WeaveMode::NotifyStack) {
                        // the STACK column — mark only the boxes the live cards actually occupy (each
                        // card's animated y + its body-driven height), so the cover scales with the
                        // column, not the buffer. The x-span is the fixed card block; y grows DOWN for
                        // a top corner, UP for a bottom corner (the engine's `y` is the distance from
                        // the corner edge). A small margin covers the feather + the bump scale-up.
                        let top_corner = stack_place.1 < 0.5;
                        let xl = STACK_AX - STACK_HALF - 6.0;
                        let xr = STACK_AX + STACK_HALF + 6.0;
                        let anchor = if top_corner { STACK_AY_TOP } else { STACK_AY_BOT };
                        let card_h = |c: &NotifyR| notify_layout(c).height * (1.0 - c.collapse);
                        for c in &stack_cards {
                            let h = card_h(c) + 8.0;
                            if top_corner {
                                let y0 = anchor + c.y;
                                lit.set_box(xl, y0 - 4.0, xr, y0 + h);
                            } else {
                                let y1 = anchor - c.y;
                                lit.set_box(xl, y1 - h, xr, y1 + 4.0);
                            }
                        }
                        // the digest summary occupies its own taller box at the anchor.
                        if stack_digest.is_some() {
                            if top_corner {
                                lit.set_box(xl, anchor - 4.0, xr, anchor + 96.0);
                            } else {
                                lit.set_box(xl, anchor - 96.0, xr, anchor + 4.0);
                            }
                        }
                        // the +N tail pill, when the stack overflowed (cover a row past the last card).
                        // (Marked generously: the tail sits just beyond the deepest card.)
                        let deepest = stack_cards.iter().map(|c| c.y + card_h(c)).fold(0.0, f32::max);
                        if top_corner {
                            lit.set_box(xl, anchor + deepest, xr, anchor + deepest + STACK_TAIL_H + 8.0);
                        } else {
                            lit.set_box(xl, anchor - deepest - STACK_TAIL_H - 8.0, xr, anchor - deepest);
                        }
                    } else {
                        // the fixed footprint (matches the old ±290 floor, rounded to tiles).
                        lit.set_box(CX - 290.0, CY - 290.0, CX + 290.0, CY + 290.0);
                        // THE STROKE — the one thing that runs far. March it (and its live head).
                        for w in points.windows(2) {
                            lit.set_seg(CX + w[0].0, CY + w[0].1, CX + w[1].0, CY + w[1].1);
                        }
                        if let Some(&(hx, hy)) = points.last() {
                            lit.set_px(CX + hx, CY + hy);
                        }
                        // embers fly off the head.
                        for p in &particles {
                            lit.set_px(CX + p.x, CY + p.y);
                        }
                        // the recognition flare ring expands to near the rim — a thin band.
                        if flare_ring > 0.0 {
                            let rr = 8.0 + flare_ring * (CX - 24.0);
                            lit.set_ring(rr - 4.0, rr + 4.0);
                        }
                        // GLYPH extras that can outrun the footprint on a LARGE stroke: the rune ring
                        // (thin bands — octagon, inner hexagon, lattice) and the prediction ghost/card.
                        if let WeaveMode::Glyph { hint } = &mode {
                            let maxr = points
                                .iter()
                                .fold(0f32, |m, &(dx, dy)| m.max((dx * dx + dy * dy).sqrt()));
                            if maxr > 7.0 {
                                let rr = maxr.min(CX - 18.0);
                                if rr + 6.0 > 290.0 {
                                    lit.set_ring(0.88 * rr, rr + 6.0); // outer octagon
                                    lit.set_ring(0.46 * rr, 0.66 * rr); // inner hexagon + its lattice
                                    lit.set_ring(0.26 * rr, 0.36 * rr); // innermost lattice
                                }
                            }
                            if let Some(h) = hint {
                                let (hx, hy) = points.last().copied().unwrap_or((0.0, 0.0));
                                let card_x = (CX + hx).clamp(60.0, W as f32 - 60.0);
                                let card_y = (CY + hy - 52.0).clamp(40.0, H as f32 - 40.0);
                                lit.set_box(
                                    card_x - 36.0,
                                    card_y - 42.0,
                                    card_x + 36.0,
                                    card_y + 46.0,
                                );
                                if h.ghost.len() >= 2 {
                                    let n = points.len().max(1) as f32;
                                    let cxs = points.iter().map(|p| p.0).sum::<f32>() / n;
                                    let cys = points.iter().map(|p| p.1).sum::<f32>() / n;
                                    let scale = (maxr * 2.0).clamp(60.0, 2.0 * (CX - 18.0));
                                    let gx = |p: &[f32; 2]| CX + cxs + p[0] * scale;
                                    let gy = |p: &[f32; 2]| CY + cys + p[1] * scale;
                                    for w in h.ghost.windows(2) {
                                        lit.set_seg(gx(&w[0]), gy(&w[0]), gx(&w[1]), gy(&w[1]));
                                    }
                                }
                            }
                        }
                    }
                    if full_redraw {
                        // a fresh surface / mode swap: wipe the OUTPUT once (a cheap memset, ~10MB)
                        // so the prior weave can't ghost in tiles this frame won't composite — then
                        // run the frame at NORMAL incremental cost (no full repaint, no full blur).
                        // The float buffers only need their usual moat, and there's nothing to erase.
                        std::ptr::write_bytes(px, 0, (W * H) as usize);
                        prev_proc = TileSet::new();
                    }
                    // proc = the tiles the per-pixel passes WRITE (content + 1-tile halo margin);
                    // aura_set = proc's blur reach; clearset = a 2-tile moat round everything that's
                    // processed THIS or LAST frame, so every neighbour read lands in zeroed-or-drawn
                    // memory and last frame's content erases cleanly.
                    let proc = lit.dilated(1);
                    let aura_set = lit.dilated(2);
                    let clearset = {
                        let mut c = proc.clone();
                        c.or_with(&prev_proc);
                        c.dilated(2)
                    };
                    for ti in 0..(TW * TH) as usize {
                        if clearset.on[ti] {
                            let (x0, y0, x1, y1) = TileSet::tile_box(ti);
                            buf.clear_box(x0, y0, x1, y1);
                        }
                    }

                    // anchor charge-glow: a soft channeling pool, gentle deterministic breathe.
                    // (not for the notification stack — a quiet notice has no channeling anchor — and
                    // not for the knockback stage, whose anchor is the FAMILIAR, not the cursor.)
                    if !matches!(
                        mode,
                        WeaveMode::Twin { .. } | WeaveMode::NotifyStack
                    ) {
                        let breathe = 0.85 + 0.15 * ((frame as f32) * 0.12).sin();
                        splat_glow(&mut buf.glow, CX, CY, 26.0, 0.16 * charge * breathe);
                        splat_white(&mut buf.white, CX, CY, 7.0, 0.10 * charge);
                    }

                    // the comet trail: dim tail -> bright white-hot head
                    let n = points.len();
                    let mut maxr = 0f32;
                    let mut prev: Option<(f32, f32)> = None;
                    for (i, &(dx, dy)) in points.iter().enumerate() {
                        let x = CX + dx;
                        let y = CY + dy;
                        maxr = maxr.max((dx * dx + dy * dy).sqrt());
                        let t = if n > 1 {
                            i as f32 / (n - 1) as f32
                        } else {
                            1.0
                        }; // 0 tail .. 1 head
                           // a CLEAN luminous ramp: dim tail -> bright head, with the oldest tail TIP
                           // surfacing through a smooth emergence (no hard cut). Deliberately NO noise and
                           // NO travelling band: the grain read as dragged-dirt "skidmarks" and the flow
                           // pulse both banded the trail and brightened it mid-fade. A glowing filament is
                           // built from clean light, not texture.
                        let rise = (t / 0.18).clamp(0.0, 1.0);
                        let rise = rise * rise * (3.0 - 2.0 * rise); // smoothstep emergence
                        let b = (0.25 + 0.75 * t) * (0.12 + 0.88 * rise);
                        // a SLIM stroke — the material halo + fire give it body/glow, so the raw
                        // density splats stay tight (a thin filament of mana, not a thick rope).
                        if let Some((px0, py0)) = prev {
                            let dist = ((x - px0).powi(2) + (y - py0).powi(2)).sqrt();
                            let steps = (dist / 1.5).ceil().max(1.0) as i32;
                            // the head's CHROMATIC FRINGE: two faint spectral rails mis-registered
                            // to either side of the white-hot core — white diffusing into the
                            // prism at the edges, like aberration on bright glass. Quiet (≈0.1).
                            let (nx, ny) = if dist > 0.5 {
                                (-(y - py0) / dist, (x - px0) / dist)
                            } else {
                                (0.0, 0.0)
                            };
                            for s in 0..steps {
                                let u = s as f32 / steps as f32;
                                let lx = px0 + (x - px0) * u;
                                let ly = py0 + (y - py0) * u;
                                // a LAYERED filament built outward so it reads as luminous VOLUME, not a
                                // thin skid: soft outer bloom -> halo -> bright body core. The bloom and
                                // halo are energy-conserving (/steps) so a fast drag stays even, while
                                // the core lays a continuous bright spine.
                                let per = b / steps as f32;
                                splat_glow(&mut buf.glow, lx, ly, 7.5, 0.16 * per); // soft outer bloom
                                splat_glow(&mut buf.glow, lx, ly, 3.75, 0.5 * per); // halo
                                splat_glow(&mut buf.glow, lx, ly, 1.6, 0.9 * b); // bright body core
                                                                                 // the head's CHROMATIC FRINGE: cool + warm refraction rails offset to
                                                                                 // either side of the white-hot core — the SAME bent-hard-light poles the
                                                                                 // rune ring uses, so the school is coherent. Fades IN across the head
                                                                                 // third (no hard pop at a fixed t) and the offset is a real ~2px so the
                                                                                 // split actually resolves instead of rounding back onto the core.
                                if t > 0.5 {
                                    let pf = ((t - 0.5) / 0.5).clamp(0.0, 1.0);
                                    splat_white(&mut buf.white, lx, ly, 0.95, 0.7 * b * pf); // white-hot head
                                    splat_prism(
                                        &mut buf,
                                        lx + nx * 2.0,
                                        ly + ny * 2.0,
                                        1.2,
                                        0.14 * b * pf,
                                        REFRACT_COOL,
                                    );
                                    splat_prism(
                                        &mut buf,
                                        lx - nx * 2.0,
                                        ly - ny * 2.0,
                                        1.2,
                                        0.14 * b * pf,
                                        REFRACT_WARM,
                                    );
                                }
                            }
                        }
                        prev = Some((x, y));
                    }
                    // white-hot head dot — plus a soft EMERGENCE WELL beneath it while drawing: a
                    // breathing pool of field welling up right where intent is acting, so the
                    // substance reads as surfacing from that point rather than a line being laid
                    // down. Glow (not white) so the material shades it; quiet, and gone once the
                    // hand lifts (fading) so it never lingers as a blob.
                    if let Some(&(hx, hy)) = points.last() {
                        // the HEAD — where intent acts: a soft glowing pool cradling a white-hot pip.
                        // It breathes gently while drawing and fades SMOOTHLY with the weave (the old
                        // well was hard-gated on `!fading`, so it popped out on the first fade frame).
                        let pulse = if fading {
                            1.0
                        } else {
                            0.82 + 0.18 * ((frame as f32) * 0.18).sin()
                        };
                        if n > 1 {
                            splat_glow(&mut buf.glow, CX + hx, CY + hy, 11.0, 0.16 * pulse); // head bloom
                            splat_glow(&mut buf.glow, CX + hx, CY + hy, 5.0, 0.34 * pulse);
                            // head glow
                        }
                        splat_white(&mut buf.white, CX + hx, CY + hy, 1.7, 0.95);
                        // white-hot pip
                    }

                    match &mode {
                        WeaveMode::Glyph { hint } => {
                            // procedural rune ring: assembles outward, tick marks, locks on flare
                            if maxr > 7.0 {
                                let rr = maxr.min(CX - 18.0);
                                rune_ring(&mut buf, rr, 0.22 + flare * 0.8, frame, &mut rng);
                            }
                            // ── LIVE NEXT-GLYPH PREDICTION: ghost the ideal shape + name the intent
                            // it's becoming, solidifying as confidence climbs (autocomplete feel) ──
                            if let Some(h) = hint {
                                let conf = h.confidence.clamp(0.0, 1.0);
                                // the shape-ghost: the target template scaled to the live stroke's
                                // own size + center, so it overlays where the hand is actually drawing
                                if h.ghost.len() >= 2 {
                                    let cxs = points.iter().map(|p| p.0).sum::<f32>()
                                        / points.len().max(1) as f32;
                                    let cys = points.iter().map(|p| p.1).sum::<f32>()
                                        / points.len().max(1) as f32;
                                    let scale = (maxr * 2.0).clamp(60.0, 2.0 * (CX - 18.0));
                                    let gb =
                                        (0.10 + conf * 0.30) * (if h.locked { 1.4 } else { 1.0 });
                                    draw_ghost_path(
                                        &mut buf,
                                        &h.ghost,
                                        CX + cxs,
                                        CY + cys,
                                        scale,
                                        gb,
                                        frame,
                                    );
                                }
                                // the prediction card: the intent's icon + name, riding above the
                                // head, brightening with confidence; a snap-pulse the moment it locks
                                let (hx, hy) = points.last().copied().unwrap_or((0.0, 0.0));
                                let card_x = (CX + hx).clamp(60.0, W as f32 - 60.0);
                                let card_y = (CY + hy - 52.0).clamp(40.0, H as f32 - 40.0);
                                let r = 13.0 + conf * 5.0;
                                let pulse = if h.locked {
                                    1.2 + 0.5 * ((frame as f32) * 0.4).sin().max(0.0)
                                } else {
                                    0.45 + conf * 0.7
                                };
                                let tone = if h.locked { Tone::Active } else { Tone::Plain };
                                // a dusk pocket under the floating prediction so it reads over the
                                // page it's drawn on, then the icon + its name (a white whisper in
                                // the name so the glow strokes keep weight).
                                shade_pocket(
                                    &mut buf,
                                    card_x,
                                    card_y - 4.0,
                                    r * 1.15,
                                    r * 1.15,
                                    0.26,
                                );
                                draw_wedge_glyph(
                                    &mut buf,
                                    h.view.glyph,
                                    card_x,
                                    card_y - 4.0,
                                    r,
                                    tone,
                                    frame,
                                    pulse,
                                );
                                if let Some(t) = &glyph_hint_title {
                                    let ty = card_y + 18.0;
                                    text_pocket(&mut buf, t, card_x, ty, 0.26);
                                    blit_mask(&mut buf.glow, t, card_x, ty, 0.45 + conf * 0.55);
                                    blit_mask(&mut buf.white, t, card_x, ty, 0.18 + conf * 0.18);
                                }
                                // a slim confidence meter under the card
                                meter_bar(
                                    &mut buf,
                                    card_x,
                                    card_y + 30.0,
                                    22.0,
                                    conf,
                                    tone,
                                    0.5 + conf * 0.5,
                                );
                            }
                        }
                        WeaveMode::Twin {
                            beats,
                            seal,
                            wash,
                            weave,
                            presence,
                            ..
                        } => {
                            // ── THE STAGE: the whole duet, visible at a glance ──
                            let tc = twin_clock;
                            const STAFF_HALF: f32 = 185.0;
                            let (sl, sr) = (CX - STAFF_HALF, CX + STAFF_HALF);

                            // mood wash: a broad additive tint behind everything (harmony hush,
                            // storm amber, haunting grey-violet). Quiet, never a flashbang.
                            if wash.3 > 0.01 {
                                splat_prism(
                                    &mut buf,
                                    CX,
                                    CY,
                                    250.0,
                                    wash.3 * 0.35,
                                    (wash.0, wash.1, wash.2),
                                );
                            }

                            // the staff beam: a thin hard-light filament the duet stands on.
                            let beam = (0.42, 0.38, 0.62);
                            let steps = (STAFF_HALF / 5.0) as i32;
                            for s2 in -steps..=steps {
                                let x = CX + s2 as f32 * 5.0;
                                splat_prism(&mut buf, x, CY, 1.4, 0.10, beam);
                            }

                            // THE FAMILIAR: a small breathing construct at the staff's head. It
                            // materializes on entry (wireframe → built as presence crosses 0.5),
                            // breathes while alive, dims when unfed — the login IS the fidget.
                            let fx = sl - 30.0;
                            let breathe = 0.78 + 0.22 * (tc * 0.045).sin();
                            let p = presence.clamp(0.0, 1.0);
                            let twin_violet = (0.72, 0.65, 1.0);
                            if p < 0.45 {
                                hard_construct(
                                    &mut buf,
                                    fx,
                                    CY,
                                    13.0,
                                    6,
                                    tc * 0.006,
                                    twin_violet,
                                    (0.4 + p) * breathe,
                                    true,
                                );
                            } else {
                                splat_prism(&mut buf, fx, CY, 7.0, 0.25 * p * breathe, twin_violet);
                                hard_construct(
                                    &mut buf,
                                    fx,
                                    CY,
                                    13.0,
                                    6,
                                    tc * 0.006,
                                    twin_violet,
                                    p * breathe,
                                    false,
                                );
                            }

                            // THE WEAVE STRIP: one crystal shard per exchange, growing along the
                            // bottom — the score, the save file, and the art, always in view.
                            if !weave.is_empty() {
                                let n = weave.len().min(24);
                                let start = weave.len() - n;
                                let total_w = n as f32 * 14.0;
                                let wx0 = CX - total_w / 2.0 + 7.0;
                                for (i, &(hue, amp, bright)) in weave[start..].iter().enumerate() {
                                    let x = wx0 + i as f32 * 14.0;
                                    let rot = if i % 2 == 0 {
                                        -0.35
                                    } else {
                                        0.35 + std::f32::consts::PI
                                    };
                                    let rgb = hsv(hue, 0.45, 1.0);
                                    hard_construct(
                                        &mut buf,
                                        x,
                                        H as f32 - 46.0,
                                        3.5 + 4.5 * amp.clamp(0.0, 1.0),
                                        3,
                                        rot,
                                        rgb,
                                        bright.clamp(0.1, 1.0) * 0.8,
                                        false,
                                    );
                                }
                            }

                            // THE BEATS: every construct standing on the staff.
                            let mut last_player: Option<(f32, f32, f32)> = None; // (x, y, r)
                            for b in beats {
                                let cx = (CX + b.x).clamp(sl - 6.0, sr + 20.0);
                                let cy = CY + b.y;
                                let bright = (0.5 + 0.5 * b.weight) * (1.0 - 0.45 * b.phase);
                                let sides = (3.0 + b.weight.clamp(0.0, 1.0) * 5.0).round() as u32;
                                let rot = b.phase * 0.4 + cx * 0.01;
                                match b.kind {
                                    // the open BLUEPRINT — the unfinished line. It pulses gently:
                                    // the itch made visible. Strike it and it builds.
                                    3 => {
                                        let pulse = 0.65 + 0.35 * (tc * 0.07).sin();
                                        hard_construct(
                                            &mut buf, cx, cy, b.r, 6, 0.4, b.rgb, pulse, true,
                                        );
                                    }
                                    // MATERIALIZING — the answering strike builds the blueprint:
                                    // a white-hot flash collapsing into a built construct, plus an
                                    // expanding commitment ring. phase runs the flash 0→1.
                                    4 => {
                                        let ph = b.phase.clamp(0.0, 1.0);
                                        splat_white(
                                            &mut buf.white,
                                            cx,
                                            cy,
                                            b.r * (0.5 + ph),
                                            0.8 * (1.0 - ph),
                                        );
                                        hard_construct(
                                            &mut buf,
                                            cx,
                                            cy,
                                            b.r,
                                            sides,
                                            rot,
                                            b.rgb,
                                            0.6 + 0.4 * (1.0 - ph),
                                            false,
                                        );
                                        // the ring of commitment expands and thins
                                        let rr = b.r * (1.0 + ph * 1.6);
                                        let nseg = 26;
                                        for k in 0..nseg {
                                            let a = TAU * k as f32 / nseg as f32;
                                            splat_prism(
                                                &mut buf,
                                                cx + a.cos() * rr,
                                                cy + a.sin() * rr,
                                                1.6,
                                                0.4 * (1.0 - ph),
                                                b.rgb,
                                            );
                                        }
                                        last_player = Some((cx, cy, b.r));
                                    }
                                    // built constructs — player phosphor (0) or twin violet (1/2).
                                    _ => {
                                        splat_prism(
                                            &mut buf,
                                            cx,
                                            cy,
                                            b.r * 0.45,
                                            0.18 * bright,
                                            b.rgb,
                                        );
                                        let extra = if b.kind == 2 { 1 } else { 0 };
                                        hard_construct(
                                            &mut buf,
                                            cx,
                                            cy,
                                            b.r,
                                            sides + extra,
                                            rot,
                                            b.rgb,
                                            bright,
                                            false,
                                        );
                                        if b.kind == 0 {
                                            last_player = Some((cx, cy, b.r));
                                        }
                                    }
                                }
                            }

                            // THE SEAL: a draining arc around your newest strike — your phrase
                            // commits when it empties. The phrase boundary, made visible and
                            // learnable without a single word.
                            if *seal >= 0.0 {
                                if let Some((px0, py0, pr)) = last_player {
                                    let rr = pr + 8.0;
                                    let arc = (seal.clamp(0.0, 1.0) * 40.0) as i32;
                                    for k in 0..arc {
                                        // drains clockwise from 12 o'clock
                                        let a =
                                            -std::f32::consts::FRAC_PI_2 + TAU * k as f32 / 40.0;
                                        splat_prism(
                                            &mut buf,
                                            px0 + a.cos() * rr,
                                            py0 + a.sin() * rr,
                                            1.3,
                                            0.5,
                                            (0.29, 0.95, 0.69),
                                        );
                                    }
                                }
                            }

                            // the quiet caption (rasterized once per change, far below the staff)
                            if let Some(t) = &twin_hint {
                                blit_mask(&mut buf.glow, t, CX, CY + 150.0, 0.55);
                            }
                        }
                        WeaveMode::Radial { sectors, .. } => {
                            // the simplest weave: show the sector wheel + light the aimed wedge.
                            // The wedge follows the stroke's INTENT (arc-recency attention, the
                            // same weighting the engine commits with) — circle around, change
                            // your mind: the lit wedge is always the one that will fire.
                            let aim = points.last().copied().unwrap_or((0.0, 0.0));
                            let live = if aim.0.abs() + aim.1.abs() < 6.0 {
                                -1 // still inside the deadzone — nothing aimed yet
                            } else {
                                let v = intent_vec(&points);
                                sector_of((v.0 * 100.0, v.1 * 100.0), *sectors)
                            };
                            sector_wheel(&mut buf.glow, &mut buf.white, *sectors, live, flare);
                            // each wedge is a live instrument CARD (icon → value → title) at its
                            // slice bearing; the aimed one blooms. The wheel SHOWS its state.
                            let n = (*sectors).max(1) as f32;
                            let rad = (CX - 26.0).min(150.0);
                            for (i, w) in wedge_views.iter().enumerate() {
                                let ca = i as f32 / n * TAU;
                                // cards sit just INSIDE the rim along each wedge's bearing
                                let px = CX + ca.sin() * (rad - 30.0);
                                let py = CY - ca.cos() * (rad - 30.0);
                                let hot = live == i as i32;
                                draw_wedge_card(&mut buf, w, px, py, hot, frame, flare);
                            }
                            // ── the SECOND TIER: the LIVE wedge's fan opens once the stroke
                            // reaches out past the rim; the hot sub-option follows the tip angle ──
                            if live >= 0 {
                                if let Some(opts) = fan_labels.get(live as usize) {
                                    if !opts.is_empty() {
                                        let hot =
                                            super::fan_pick(aim, live, *sectors, opts.len(), rad);
                                        if hot >= 0 {
                                            fan_arc(&mut buf, opts, live, *sectors, hot, rad);
                                        }
                                    }
                                }
                            }
                        }
                        WeaveMode::Dial {
                            fill,
                            glow,
                            mic,
                            muted,
                            ..
                        } => {
                            // the analog knob: a 270° arc the stroke turns. The charge sweeps round
                            // to the value, the bead streaks + shimmers the faster you spin. The hub
                            // now SAYS what it's turning: target icon, the % reading, the device
                            // name, and a red ring when that endpoint is muted.
                            dial_gauge(&mut buf, *fill, *glow, frame, *muted);
                            // the target icon (mic vs speaker) sits just above the reading
                            let g = if *mic {
                                WedgeGlyph::Mic
                            } else {
                                WedgeGlyph::Speaker
                            };
                            let tone = if *muted { Tone::Off } else { Tone::Live };
                            // a dusk pocket behind the icon AND the % reading so both stay legible
                            // over the gauge glow — the value is the thing you watch while spinning.
                            shade_pocket(&mut buf, CX, CY - 26.0, 16.0, 16.0, 0.30);
                            draw_wedge_glyph(&mut buf, g, CX, CY - 26.0, 13.0, tone, frame, 0.95);
                            if let Some(t) = &dial_value {
                                text_pocket(&mut buf, t, CX, CY + 4.0, 0.32);
                                let ch: &mut [f32] = if *muted {
                                    &mut buf.warn
                                } else {
                                    &mut buf.white
                                };
                                blit_mask(ch, t, CX, CY + 4.0, 1.25);
                            }
                            if let Some(t) = &dial_device {
                                // the endpoint NAME is the answer to "which output am i turning?" —
                                // give it a dusk pocket + a whisper of white core so it reads
                                // clearly instead of dissolving to faint rim (the output side was
                                // "hard to figure out"; now the device is named, legibly).
                                text_pocket(&mut buf, t, CX, CY + 26.0, 0.26);
                                blit_mask(&mut buf.glow, t, CX, CY + 26.0, 0.72);
                                blit_mask(&mut buf.white, t, CX, CY + 26.0, 0.22);
                            }
                        }
                        WeaveMode::Control { ssid: _, .. } => {
                            // the glance: three readout cards (net N / output W / bluetooth E),
                            // each the same live-instrument card the wheel uses, so the control
                            // center is made of the same mana. A flick lights the card it commits:
                            // west = output flip, east = bluetooth seam; a peek just closes.
                            let aim = points.last().copied().unwrap_or((0.0, 0.0));
                            // which card is aimed (matches the beacon's commit quadrants): -1 none,
                            // 0 = north (network), 1 = west (output), 2 = east (bluetooth). A small
                            // dead radius keeps a resting/peeking cursor from lighting anything; a
                            // downward flick lands on no card (a glance).
                            let live: i32 = if aim.0.abs() + aim.1.abs() < 18.0 {
                                -1
                            } else if aim.0.abs() > aim.1.abs() {
                                if aim.0 < 0.0 {
                                    1
                                } else {
                                    2
                                }
                            } else if aim.1 < 0.0 {
                                0
                            } else {
                                -1
                            };
                            // a faint binding ring so the glance reads as one instrument, not three
                            // loose cards (quiet — it's a glance, not a wheel to aim around).
                            let rad = (CX - 40.0).min(120.0);
                            let ring_n = (rad * 2.2) as i32;
                            for k in 0..ring_n {
                                let a = (k as f32 / ring_n as f32) * TAU;
                                splat(
                                    &mut buf.glow,
                                    CX + a.cos() * rad,
                                    CY + a.sin() * rad,
                                    2.4,
                                    0.16,
                                );
                            }
                            for (i, (card, dx, dy)) in ctl_cards.iter().enumerate() {
                                // card 0 = net (north), 1 = output (west), 2 = bluetooth (east).
                                // The aimed card blooms — same index the flick commits.
                                let hot = i as i32 == live;
                                draw_wedge_card(
                                    &mut buf,
                                    card,
                                    CX + dx,
                                    CY + dy,
                                    hot,
                                    frame,
                                    flare,
                                );
                            }
                            // the SSID below the network card — the "right network?" answer, quiet.
                            // Placed clear of the card's title lane (which only shows when hot) and
                            // well above the output/bluetooth cards at CY+26.
                            if let Some(t) = &ctl_ssid {
                                text_pocket(&mut buf, t, CX, CY - 16.0, 0.22);
                                blit_mask(&mut buf.glow, t, CX, CY - 16.0, 0.6);
                            }
                        }
                        WeaveMode::Ask { .. } => {
                            // the PROMPT: the radial core specialized for answering. ONE wheel for
                            // yes/no (N=2: accent WEST = yes, material east = no, vertical = pass), an
                            // N-way choose, or a confirm — the wedges ARE the option list. The highlight
                            // reads `radial::pick_wedge`, the SAME rule the beacon commits with, so what
                            // lights up and what the macro receives can never disagree.
                            let aim = points.last().copied().unwrap_or((0.0, 0.0));
                            let accent = crate::weave::live_material().accent;
                            prompt_wheel(&mut buf, &ask_opts, aim, frame, flare, accent);
                            // ALL beacon text is the WEAVE MATERIAL: the glow channel is what runs
                            // through weave::shade() (fire body, honey ramp, the user's accent rim) —
                            // the white channel is the raw un-materialized hot core. So every label
                            // rides GLOW as its substance with only a faint white core for legibility,
                            // exactly the dual-channel the radial wedge labels + glyph hint use. The
                            // question is the brightest substance; the context the quietest.
                            if let Some(t) = &ask_label {
                                text_pocket(&mut buf, t, CX, CY - 118.0, 0.30);
                                blit_mask(&mut buf.glow, t, CX, CY - 118.0, 0.92);
                                blit_mask(&mut buf.white, t, CX, CY - 118.0, 0.34);
                            }
                            let rad = (CX - 26.0).min(150.0);
                            // PASS caption — when the aim is a COMMITTED flick into a gap (no wedge), it
                            // appears where you're pointing, neutral-white (it isn't a verdict, so it
                            // wears neither accent nor material). At N=2 the gaps ARE the vertical axis,
                            // so this is the old "flick up/down to pass" feel, now emergent for any N.
                            let am = (aim.0 * aim.0 + aim.1 * aim.1).sqrt();
                            if am >= 6.0
                                && neuron::radial::pick_wedge(
                                    aim.0 as f64,
                                    aim.1 as f64,
                                    6.0,
                                    ask_opts.len(),
                                )
                                .is_none()
                            {
                                if let Some(t) = &ask_pass {
                                    let ang = (aim.1).atan2(aim.0) as f32;
                                    let (px, py) =
                                        (CX + ang.cos() * (rad - 24.0), CY + ang.sin() * (rad - 24.0));
                                    text_pocket(&mut buf, t, px, py, 0.30);
                                    blit_mask(&mut buf.white, t, px, py, 0.7);
                                }
                            }
                            // DESCRIPTION: arbitrary context the macro passed (neuron.ask(description=)),
                            // quietly BELOW the wheel. The ACCENT prism, NOT glow — glow runs through the
                            // material's dispersion, which fringes this thin text amber; the prism is
                            // added straight, so it stays the user's colour. (THIS was the amber.)
                            if let Some(t) = &ask_detail {
                                let dy = CY + rad + 30.0;
                                text_pocket(&mut buf, t, CX, dy, 0.24);
                                blit_mask(&mut buf.pr, t, CX, dy, 0.62 * accent.0);
                                blit_mask(&mut buf.pg, t, CX, dy, 0.62 * accent.1);
                                blit_mask(&mut buf.pb, t, CX, dy, 0.62 * accent.2);
                                blit_mask(&mut buf.white, t, CX, dy, 0.14);
                            }
                        }
                        WeaveMode::Map {
                            monitors,
                            windows,
                            realms,
                            cursor,
                            hot,
                            hot_realm,
                            carried,
                            tethers,
                            depth,
                        } => {
                            // TELEPORT: the desk in miniature. Monitor outlines are hairline
                            // structure; windows are soft recency-lit blobs; other desktops sit
                            // BENEATH as small hollow realm cards (their own little worlds,
                            // never mixed into the desk). The ghost (white) is where you'll
                            // land; the HOT blob — the depth dial's pick, the realm window
                            // you're touching, or the geometric frontmost — goes bright AND wears
                            // the Directed-Intent selection manifestation (rim + sparkle waveform).
                            let ghost = points.last().copied().unwrap_or(*cursor);
                            for m in monitors {
                                rect_stroke(&mut buf.glow, m, 0.30);
                            }
                            for (i, (r, bright)) in windows.iter().enumerate() {
                                let hover = if *hot >= 0 {
                                    i as i32 == *hot
                                } else {
                                    ghost.0 >= r[0]
                                        && ghost.0 < r[2]
                                        && ghost.1 >= r[1]
                                        && ghost.1 < r[3]
                                };
                                fill_rect(
                                    &mut buf.glow,
                                    r,
                                    0.05 + 0.14 * bright + if hover { 0.25 } else { 0.0 },
                                );
                                if hover {
                                    // the selection EMERGES (sparkle waveform + aberration rim) —
                                    // the magic the user loves, here marking the warp target.
                                    cell_select(&mut buf, r, frame, 1.0);
                                }
                            }
                            // the realm strip: each other desktop a visible card at rest (it
                            // must read as "there's somewhere else to go" WITHOUT being hovered);
                            // its windows tiny blobs; the touched card warms, the blob lights —
                            // and manifests the SAME Directed-Intent selection as the desk, so the
                            // extra desktops never feel like a separate, sparkle-less system.
                            for (ri, (card, blobs)) in realms.iter().enumerate() {
                                let in_card = hot_realm.0 == ri as i32;
                                rect_stroke(&mut buf.glow, card, if in_card { 0.70 } else { 0.42 });
                                // touching anywhere on a card manifests the card edge (softly), so
                                // hovering the realm itself sparkles like the main minimap does.
                                if in_card {
                                    cell_select(&mut buf, card, frame, 0.6);
                                }
                                for (wi, (b, bright)) in blobs.iter().enumerate() {
                                    let hot_b = in_card && hot_realm.1 == wi as i32;
                                    fill_rect(
                                        &mut buf.glow,
                                        b,
                                        0.10 + 0.16 * bright + if hot_b { 0.30 } else { 0.0 },
                                    );
                                    if hot_b {
                                        cell_select(&mut buf, b, frame, 0.85);
                                    }
                                }
                            }
                            // WARPSTONES: where your tether anchors live, shown quietly so you can
                            // aim a teleport right at one (item: "see tether points while in teleport").
                            for (tx, ty) in tethers {
                                warpstone_pip(&mut buf, CX + tx, CY + ty, frame);
                            }
                            // the SPECTRAL GRAB: a carried window rides the ghost as a bright
                            // phantom with a faint chromatic fringe — clearly matter in transit.
                            if let Some(c) = carried {
                                rect_stroke(&mut buf.white, c, 0.85);
                                let hue = frame as f32 * 3.0;
                                let off = 1.5;
                                let ca = [c[0] - off, c[1] - off, c[2] - off, c[3] - off];
                                let cb = [c[0] + off, c[1] + off, c[2] + off, c[3] + off];
                                rect_stroke_rgb(&mut buf, &ca, 0.30, hsv(hue, 0.8, 1.0));
                                rect_stroke_rgb(&mut buf, &cb, 0.30, hsv(hue + 150.0, 0.8, 1.0));
                            }
                            splat_glow(&mut buf.glow, CX + cursor.0, CY + cursor.1, 5.0, 0.45);
                            splat_glow(&mut buf.glow, CX + ghost.0, CY + ghost.1, 9.0, 0.6);
                            splat_white(&mut buf.white, CX + ghost.0, CY + ghost.1, 4.0, 0.9);
                            // THE DEPTH AFFORDANCE: when windows overlap under the ghost, a little
                            // stack of layer pips climbs beside it — lit up to the current depth,
                            // dim below — teaching "scroll here to descend the stack" without a word
                            // (the capability the user found by accident; now it's discoverable).
                            let (di, dn) = (depth.0, depth.1);
                            if dn > 1 {
                                let (px, py) = (CX + ghost.0 + 16.0, CY + ghost.1 - 4.0);
                                let pulse = 0.6 + 0.4 * ((frame as f32) * 0.12).sin();
                                for layer in 0..dn.min(6) {
                                    let yy = py + layer as f32 * 5.0; // top = surface, down = deeper
                                    let lit = layer == di;
                                    let b = if lit { 0.95 * pulse } else { 0.22 };
                                    // a short rung; the live layer also gets a white tick + a faint
                                    // down-chevron hint at the bottom rung so the direction reads.
                                    vline(&mut buf.glow, (px, yy), (px + 7.0, yy), 1.5, b);
                                    if lit {
                                        splat_white(&mut buf.white, px + 3.5, yy, 1.3, 0.7 * pulse);
                                    }
                                }
                                // a tiny ⌄ under the rungs: scrolling goes DOWN into the stack.
                                let cy2 = py + (dn.min(6) as f32) * 5.0 + 1.0;
                                vline(
                                    &mut buf.glow,
                                    (px + 1.0, cy2),
                                    (px + 3.5, cy2 + 2.5),
                                    1.2,
                                    0.5 * pulse,
                                );
                                vline(
                                    &mut buf.glow,
                                    (px + 6.0, cy2),
                                    (px + 3.5, cy2 + 2.5),
                                    1.2,
                                    0.5 * pulse,
                                );
                            }
                        }
                        WeaveMode::NotifyStack => {
                            // THE MULTI-NOTIFICATION SURFACE — the engine has already eased every
                            // slot's animation; here we just paint the given frame. ALL colour is the
                            // live material accent; per-type identity rides the glyph. The three modes
                            // (stack column / latest swap / digest summary) all share the card chrome.
                            let accent = crate::weave::live_material().accent;
                            draw_notify_stack(
                                &mut buf,
                                frame,
                                accent,
                                stack_mode_kind,
                                stack_place,
                                &stack_cards,
                                stack_digest.as_ref(),
                                stack_tail,
                            );
                        }
                    }

                    // recognition flare: an expanding bright ring
                    if flare_ring > 0.0 {
                        let rr = 8.0 + flare_ring * (CX - 24.0);
                        ring_stroke(&mut buf.white, rr, (1.0 - flare_ring) * 0.9);
                    }

                    // embers — WHITE → PRISM: every ember is born white-hot and diffuses into
                    // its own spectral hue as it cools (the hue drifting a little as it goes).
                    // The magic flair, kept quiet: small radii, the white heart carries the light.
                    for p in &particles {
                        let (x, y) = (CX + p.x, CY + p.y);
                        let w_part = ((p.life - 0.30) / 0.45).clamp(0.0, 1.0);
                        let s_part = (1.0 - w_part) * p.life;
                        let r0 = if p.white { 3.5 } else { 2.8 };
                        if w_part > 0.003 {
                            splat_white(&mut buf.white, x, y, r0, 0.9 * p.life * w_part);
                        }
                        if s_part > 0.003 {
                            let hue = p.hue + (1.0 - p.life) * 70.0;
                            splat_prism(
                                &mut buf,
                                x,
                                y,
                                r0 + 0.8,
                                0.65 * s_part,
                                hsv(hue, 0.85, 1.0),
                            );
                        }
                    }

                    // ── the dusk + the aura: contrast WITHOUT panels — the room dims like
                    // smoke around the cast, and every emission casts its own soft shadow ──
                    if !matches!(mode, WeaveMode::NotifyStack) {
                        let dusk_r: f32 = match &mode {
                            WeaveMode::Map { .. } => 268.0,
                            WeaveMode::Radial { .. } | WeaveMode::Ask { .. } => 235.0,
                            WeaveMode::Dial { .. } => 250.0,
                            WeaveMode::Control { .. } => 240.0,
                            WeaveMode::Twin { .. } => 260.0,
                            _ => 205.0,
                        };
                        // the knockback stage is AMBIENT — you live next to it for minutes, so
                        // its dusk is half-strength (present, never smothering the game behind).
                        let dusk_s = if matches!(mode, WeaveMode::Twin { .. }) {
                            0.20
                        } else {
                            0.40
                        };
                        // the dusk is a FIXED disc round the anchor (its radius never depended on the
                        // trail) — give it its own bounded box, not the stroke's reach.
                        let db = dusk_r.ceil() as i32;
                        buf.dusk(
                            dusk_r,
                            dusk_s,
                            frame,
                            CX as i32 - db,
                            CY as i32 - db,
                            CX as i32 + db,
                            CY as i32 + db,
                        );
                    }
                    buf.aura_h(&aura_set);
                    buf.aura_v(&proc);

                    // ── composite: POURED MATERIAL. The glow channel is not light — it's a
                    // metaball density FIELD; the material shades its surface: glob shoulder
                    // (droplets neck when they approach), honey heat ramp (deep amber → gold →
                    // white-hot), a hard-light RIM painted in the user's ACCENT along the field
                    // gradient, and honest dispersion (each colour channel taps the field a
                    // touch along/against the gradient — the chromatic fringe lives exactly at
                    // edges). Warn stays an urgent red body; prism channels remain explicit
                    // spectral accents; dusk darkens beneath. The user tint is an EDGE now,
                    // never a body — green is a theme, not a substance. ──
                    // the LIVE material — themed from material.toml, with the user's weave accent (SYSTEM →
                    // APPEARANCE) folded into the fire-centre hue + rim. Fetched ONCE per frame here,
                    // never per-pixel, so a colour swap costs one struct copy and lands next frame.
                    let m = &crate::weave::live_material();
                    let mt = crate::weave::seconds(); // one animation clock for this whole frame
                                                      // composite over the union of this frame's processed tiles and last frame's, so a
                                                      // vacated tile is written transparent. Neighbour reads (gradient ±1, dispersion
                                                      // taps) land in the cleared moat or drawn content — never stale memory.
                    let mut comp = proc.clone();
                    comp.or_with(&prev_proc);
                    // dbg_cover is read ONLY by the NEURON_PROFILE slow-frame eprintln below; the
                    // ~2500-entry tile-cover scan is otherwise pure waste, so only count when profiling.
                    if slow_profile {
                        dbg_cover = comp.on.iter().filter(|&&b| b).count();
                    }
                    for ti in 0..(TW * TH) as usize {
                        if !comp.on[ti] {
                            continue;
                        }
                        let (tx0, ty0, tx1, ty1) = TileSet::tile_box(ti);
                        for yy in ty0..ty1 {
                            let row = (yy * W) as usize;
                            for xx in tx0..tx1 {
                                let i = row + xx as usize;
                                let d = (buf.glow[i] * fade).min(1.6);
                                let c = (buf.white[i] * fade).min(1.4);
                                let w = (buf.warn[i] * fade).min(1.4);
                                let sr = (buf.pr[i] * fade).min(1.2);
                                let sg = (buf.pg[i] * fade).min(1.2);
                                let sb = (buf.pb[i] * fade).min(1.2);
                                let sh = (buf.shade[i] * fade).min(0.78);
                                // the field gradient = the surface normal (clamped neighbours)
                                let xl = buf.glow[row + (xx.max(1) - 1) as usize];
                                let xr = buf.glow[row + (xx + 1).min(W - 1) as usize];
                                let yu = buf.glow[((yy.max(1) - 1) * W) as usize + xx as usize];
                                let yd = buf.glow[((yy + 1).min(H - 1) * W) as usize + xx as usize];
                                // EARLY-OUT on the RAW (unfaded) buffer content, so the culled set is
                                // FROZEN through a fade-out — only the alpha ramps, never the membership.
                                // (Testing the faded values popped dim pixels to 0 at a different fade than
                                // bright ones, so the trail dissolved in a speckled, dithered order.)
                                if buf.glow[i] <= 0.002
                                    && buf.white[i] <= 0.001
                                    && buf.warn[i] <= 0.001
                                    && buf.shade[i] <= 0.004
                                    && buf.pr[i] <= 0.001
                                    && buf.pg[i] <= 0.001
                                    && buf.pb[i] <= 0.001
                                    && xl <= 0.002
                                    && xr <= 0.002
                                    && yu <= 0.002
                                    && yd <= 0.002
                                {
                                    *px.add(i) = 0;
                                    continue;
                                }
                                let gx = (xr - xl) * 0.5 * fade;
                                let gy = (yd - yu) * 0.5 * fade;
                                let grad = (gx * gx + gy * gy).sqrt();
                                // DUSK FAST-PATH: a pixel with no emission and a flat field is just the
                                // gathering-dusk darkening EMPTY SPACE — no surface to shade, no edge to
                                // ignite. The dusk disc fails the early-out above (it has shade), so the
                                // OLD code ran the whole material pipeline over ~0.2M undrawn pixels every
                                // frame — the "touching what isn't drawn" cost the audit flagged. Here we
                                // write the black-with-dusk-alpha pixel directly. Byte-identical: the material's
                                // shade() returns zero colour with no emission, and a sub-0.004-grad rim
                                // rounds away to 0 in the u8 output.
                                if grad <= 0.004
                                    && buf.glow[i] <= 0.002
                                    && buf.white[i] <= 0.001
                                    && buf.warn[i] <= 0.001
                                    && buf.pr[i] <= 0.001
                                    && buf.pg[i] <= 0.001
                                    && buf.pb[i] <= 0.001
                                {
                                    let lum = (sr.max(sg).max(sb) * 0.8 + sh * 0.8).min(1.0);
                                    let a = (lum * 205.0).min(255.0);
                                    *px.add(i) = (a as u32) << 24;
                                    continue;
                                }
                                dbg_painted += 1; // fell through to the FULL pipeline (the cost)
                                // the prism taps run along the FACET (the gradient quantized into
                                // cut-glass planes) — light bands break into geometric segments,
                                // and the facet's position picks the fire's hue.
                                let (dr, db, fu) = if grad > 0.004 {
                                    let (ux, uy, fu) = crate::weave::facet(gx, gy, m.facets);
                                    let ox = (ux * m.dispersion).round() as i32;
                                    let oy = (uy * m.dispersion).round() as i32;
                                    let tap = |x: i32, y: i32| -> f32 {
                                        let x = x.clamp(0, W - 1);
                                        let y = y.clamp(0, H - 1);
                                        (buf.glow[(y * W + x) as usize] * fade).min(1.6)
                                    };
                                    (tap(xx + ox, yy + oy), tap(xx - ox, yy - oy), fu)
                                } else {
                                    (d, d, 0.5)
                                };
                                // shade the cast through the CHOSEN surface (glass / water / fire / air /
                                // electric / lava) — the field is the same, the physics differ per material.
                                let inp = crate::weave::Px {
                                    d,
                                    dr,
                                    db,
                                    gx,
                                    gy,
                                    grad,
                                    facet_u: fu,
                                    heat: c,
                                    x: xx as f32,
                                    y: yy as f32,
                                    t: mt,
                                };
                                let (mut r, mut gg, mut b, mut lum) =
                                    crate::weave::shade_surface(&inp, m);
                                // WARN → INTENT (item 11): the destructive / communicating channel is its
                                // OWN little metaball surface, shaded through the SAME material with a
                                // warm-red INTENT dialled fully in — so "about to unset/destroy" EMERGES
                                // from the substance (its body, fire and rim all read warm) instead of a
                                // flat red bolt-on. The material itself surfaces the colour, and only when
                                // the magic is talking. Runs only where warn > 0 (a rare destructive wedge).
                                if w > 0.004 {
                                    let wxl = buf.warn[row + (xx.max(1) - 1) as usize];
                                    let wxr = buf.warn[row + (xx + 1).min(W - 1) as usize];
                                    let wyu =
                                        buf.warn[((yy.max(1) - 1) * W) as usize + xx as usize];
                                    let wyd =
                                        buf.warn[((yy + 1).min(H - 1) * W) as usize + xx as usize];
                                    let wgx = (wxr - wxl) * 0.5 * fade;
                                    let wgy = (wyd - wyu) * 0.5 * fade;
                                    let wgrad = (wgx * wgx + wgy * wgy).sqrt();
                                    let (wdr, wdb, wfu) = if wgrad > 0.004 {
                                        let (ux, uy, wfu) = crate::weave::facet(wgx, wgy, m.facets);
                                        let ox = (ux * m.dispersion).round() as i32;
                                        let oy = (uy * m.dispersion).round() as i32;
                                        let tapw = |x: i32, y: i32| -> f32 {
                                            let x = x.clamp(0, W - 1);
                                            let y = y.clamp(0, H - 1);
                                            (buf.warn[(y * W + x) as usize] * fade).min(1.4)
                                        };
                                        (tapw(xx + ox, yy + oy), tapw(xx - ox, yy - oy), wfu)
                                    } else {
                                        (w, w, 0.5)
                                    };
                                    // warm red (~0.04), fully dialled — the warn field IS the "destroy" intent
                                    let mi = m.with_intent(0.04, 1.0);
                                    let (wr2, wg2, wb2, wlum) = crate::weave::shade(
                                        w,
                                        wdr,
                                        wdb,
                                        (w * 0.5).min(0.9),
                                        wgrad,
                                        wfu,
                                        &mi,
                                    );
                                    r += wr2;
                                    gg += wg2;
                                    b += wb2;
                                    lum += wlum;
                                }
                                // explicit spectral accents (the comet head's fringe, prism rings)
                                r += sr;
                                gg += sg;
                                b += sb;
                                lum = (lum + sr.max(sg).max(sb) * 0.8 + sh * 0.8).min(1.0);
                                // the whole pipe accumulates LINEAR light; the layered window presents as
                                // sRGB. Encode here (sqrt ≈ gamma 2.0) so mid-tones lift into luminous
                                // light instead of the dim, harsh "cheap additive bloom" of un-encoded
                                // linear shown straight. Cores still clip to white; text stays crisp.
                                let r = r.min(1.0).sqrt();
                                let gg = gg.min(1.0).sqrt();
                                let b = b.min(1.0).sqrt();
                                // alpha < colour ⇒ additive-looking glow; a lum² knee drives the dense
                                // white-hot CORES (and text) toward opaque so they read as emissive light,
                                // not a translucent decal, while the diffuse halo keeps its screen-like ~0.8.
                                let a = (lum * 205.0 + lum * lum * 50.0).min(255.0);
                                let af = a / 255.0;
                                *px.add(i) = ((a as u32) << 24)
                                    | (((r * 255.0 * af) as u32) << 16)
                                    | (((gg * 255.0 * af) as u32) << 8)
                                    | ((b * 255.0 * af) as u32);
                            }
                        }
                    }
                    // this frame's processed cover becomes next frame's erase set.
                    prev_proc = proc;
                    full_redraw = false;

                    let pos = POINT {
                        x: origin.x,
                        y: origin.y,
                    };
                    let size = SIZE { cx: W, cy: H };
                    surf.present(Some(pos), size, 255);
                    // STAY on top: a topmost window still loses the z-race when the taskbar, a game,
                    // or another topmost re-asserts. Re-flip to the front of the band a few times a
                    // second so a corner notification (or a weave) never sinks behind the shell. The
                    // momentary NOTOPMOST lasts only between these two calls (no frame renders), so
                    // it's invisible; the window is click-through, so z never affects input.
                    if frame % 6 == 0 {
                        SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                        SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
                    }
                }

                // an idle beacon (an un-engaged ask wheel) breathes at half rate — it can sit for
                // minutes, so don't burn 60fps on a slow pulse.
                let idle_beacon = visible
                    && !fading
                    && points.is_empty()
                    && matches!(mode, WeaveMode::Ask { .. });
                // FRAME CAP, not an additive delay. The old `sleep(16)` ran ON TOP of the render, so
                // a T-ms frame took T+16ms — a 15ms render became a 31ms frame (32fps), which IS the
                // lag, independent of any pass cost. Sleep only the REMAINDER of the target period:
                // frames now land on the ≤60fps cadence and never wait longer than the render itself.
                let target: u64 = if idle_beacon { 33 } else { 16 };
                let dur = frame_start.elapsed(); // render cost, excluding the cap sleep below
                if slow_profile
                    && visible
                    && (dur.as_millis() >= 8 || dbg_painted > 2000)
                {
                    let m = match &mode {
                        WeaveMode::Ask { .. } => "Ask",
                        WeaveMode::Radial { .. } => "Radial",
                        WeaveMode::Glyph { .. } => "Glyph",
                        WeaveMode::Dial { .. } => "Dial",
                        WeaveMode::Map { .. } => "Map",
                        WeaveMode::Twin { .. } => "Twin",
                        WeaveMode::NotifyStack => "NotifyStack",
                        _ => "other",
                    };
                    eprintln!(
                        "[OVL slow] {}ms mode={m} visible={visible} fading={fading} cover={dbg_cover}/{} painted={dbg_painted} pts={} particles={}",
                        dur.as_millis(),
                        (TW * TH),
                        points.len(),
                        particles.len()
                    );
                }
                if profile {
                    prof_us += dur.as_micros();
                    prof_max = prof_max.max(dur.as_micros());
                    prof_n += 1;
                    if prof_n >= 60 {
                        eprintln!(
                            "[overlay] frame avg {:.1}ms  max {:.1}ms  (last 60)",
                            prof_us as f64 / prof_n as f64 / 1000.0,
                            prof_max as f64 / 1000.0
                        );
                        prof_us = 0;
                        prof_max = 0;
                        prof_n = 0;
                    }
                }
                let elapsed = dur.as_millis() as u64;
                std::thread::sleep(Duration::from_millis(target.saturating_sub(elapsed)));
            }

            // surf's Drop restores the bitmap, deletes the DIB + mem DC, releases the screen DC,
            // and destroys the window — exactly the old inline teardown order.
            drop(surf);
        }
    }

    /// f32 mirror of `neuron::radial::intent_vector`, normalized — the live wedge highlight
    /// must agree with the wedge the engine will commit (same arc-recency attention weighting:
    /// increments weighted by exp(-s/τ), s = arc distance from the stroke's end, τ = 30% of
    /// the total arc).
    fn intent_vec(points: &[(f32, f32)]) -> (f32, f32) {
        if points.len() < 2 {
            return (0.0, 0.0);
        }
        let total: f32 = points
            .windows(2)
            .map(|w| {
                let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
                (dx * dx + dy * dy).sqrt()
            })
            .sum();
        if total <= 1e-6 {
            return (0.0, 0.0);
        }
        let tau = (total * 0.30).max(1e-6);
        let mut s = 0.0f32;
        let (mut vx, mut vy) = (0.0f32, 0.0f32);
        for w in points.windows(2).rev() {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let seg = (dx * dx + dy * dy).sqrt();
            let wgt = (-(s + seg * 0.5) / tau).exp();
            vx += dx * wgt;
            vy += dy * wgt;
            s += seg;
        }
        let m = (vx * vx + vy * vy).sqrt();
        if m <= 1e-6 {
            (0.0, 0.0)
        } else {
            (vx / m, vy / m)
        }
    }

    /// Net-direction → sector index (0 = North/up, clockwise), matching the radial engine.
    /// ROUND semantics, exactly like `neuron::radial::sector_for`: wedge s is CENTERED on its
    /// bearing (sector 0 spans NNW..NNE), so the live highlight always names the wedge that will
    /// actually fire on release. (Floor semantics here once skewed the highlight half a wedge.)
    fn sector_of((dx, dy): (f32, f32), sectors: u8) -> i32 {
        if dx.abs() + dy.abs() < 6.0 {
            return -1; // inside the deadzone — no pick yet
        }
        let n = sectors.max(1) as f32;
        // screen y is down; North = up = -y. angle measured clockwise from North.
        let mut a = dx.atan2(-dy); // 0 at North, +clockwise
        if a < 0.0 {
            a += TAU;
        }
        ((a / TAU * n).round() as i32) % sectors as i32
    }

    fn splat(buf: &mut [f32], cx: f32, cy: f32, radius: f32, bright: f32) {
        let r = radius.ceil() as i32;
        let cxi = cx.round() as i32;
        let cyi = cy.round() as i32;
        for yy in (cyi - r).max(0)..=(cyi + r).min(H - 1) {
            let row = yy * W;
            for xx in (cxi - r).max(0)..=(cxi + r).min(W - 1) {
                let dx = xx as f32 - cx;
                let dy = yy as f32 - cy;
                let d = (dx * dx + dy * dy).sqrt() / radius;
                if d < 1.0 {
                    let f = 1.0 - d;
                    // smoothstep falloff: value AND slope vanish at the rim (the old f*f left a
                    // curvature kink at d==1 that the rim/fire passes traced as a hard dot edge).
                    buf[(row + xx) as usize] += f * f * (3.0 - 2.0 * f) * bright;
                }
            }
        }
    }
    #[inline]
    fn splat_glow(buf: &mut [f32], cx: f32, cy: f32, r: f32, b: f32) {
        splat(buf, cx, cy, r, b)
    }
    #[inline]
    fn splat_white(buf: &mut [f32], cx: f32, cy: f32, r: f32, b: f32) {
        splat(buf, cx, cy, r, b)
    }

    /// LEGIBILITY POCKET — pour a little of the gathering dusk *under* a glyph or label so its
    /// thin mana strokes always read over a busy wheel or a white page. Not a panel: a soft disc
    /// of SHADE (the same darkening channel the dusk + auto-aura use), solid in a small core and
    /// feathered to nothing at the rim, so it reads as the substance drawing the room in around the
    /// mark — never a drawn box. `rx`/`ry` are the pocket's half-extents; `depth` how dark the core.
    fn shade_pocket(buf: &mut Buffers, cx: f32, cy: f32, rx: f32, ry: f32, depth: f32) {
        let (rx, ry) = (rx.max(1.0), ry.max(1.0));
        let x0 = ((cx - rx).floor() as i32).max(0);
        let x1 = ((cx + rx).ceil() as i32).min(W - 1);
        let y0 = ((cy - ry).floor() as i32).max(0);
        let y1 = ((cy + ry).ceil() as i32).min(H - 1);
        for yy in y0..=y1 {
            let row = yy * W;
            let dy = (yy as f32 - cy) / ry;
            for xx in x0..=x1 {
                let dx = (xx as f32 - cx) / rx;
                let q = (dx * dx + dy * dy).sqrt();
                if q >= 1.0 {
                    continue;
                }
                // solid core to ~45% radius, then smoothstep down to the feathered edge
                let e = ((1.0 - q) / 0.55).clamp(0.0, 1.0);
                let f = e * e * (3.0 - 2.0 * e);
                buf.shade[(row + xx) as usize] += depth * f;
            }
        }
    }

    /// The pocket sized to a text raster (a soft horizontal capsule behind a label).
    fn text_pocket(buf: &mut Buffers, t: &TextRaster, cx: f32, cy: f32, depth: f32) {
        shade_pocket(
            buf,
            cx,
            cy,
            t.w as f32 / 2.0 + 4.0,
            t.h as f32 / 2.0 + 1.0,
            depth,
        );
    }

    // ── CARD geometry — shared by every stack card (the value-hierarchy card AND the ask card draw
    // off these), so each card's chip + gap + margins are one single source of truth and can never
    // disagree across the column. ──
    const NOTIFY_GLYPH_R: f32 = 13.0; // the icon's scale INSIDE the chip
    const NOTIFY_CHIP_HALF: f32 = 19.0; // the glyph CHIP's half-size (a rounded-square badge)
    const NOTIFY_GLYPH_BOX: f32 = NOTIFY_CHIP_HALF * 2.0 + 6.0; // the chip's horizontal footprint
    const NOTIFY_GAP: f32 = 14.0; // chip → text-block gap
    const NOTIFY_TAB: f32 = 4.0; // the signature accent tab on the left edge
    const NOTIFY_ARROW: f32 = 15.0; // width reserved beside a ranged value for the ↑/↓ cue

    // ── STACK geometry — the multi-notification column. A FIXED card width keeps the column from
    // reflowing sideways as content changes (only the y-reflow animates); a fixed buffer ANCHOR lets
    // the window sit at the chosen monitor corner with the column growing into the (transparent)
    // off-corner space. One source of truth for the cover, the draw, and `place_window`. ──
    const STACK_HALF: f32 = 158.0; // each card's half-width (a roomy, stable column width)
    const STACK_CARD_GAP: f32 = 8.0; // vertical gap between stacked cards
    const STACK_PAD: f32 = 18.0; // inset of the column from the monitor corner
    // card HEIGHT + row y's now live in ONE place — `notifs::card_layout` (the box is the sum of its
    // rows, so content can't overflow it). The constants that used to be mirrored here are gone.
    const STACK_TAIL_H: f32 = 26.0; // the "+N more" tail pill's height (the engine caps live at 4)
    // the column's fixed buffer ANCHOR: x is the card-block centre; y is the corner edge (the card
    // edge nearest the chosen vertical). The window is placed (see `stack_origin`) so STACK_AX lands
    // at the screen point the placement's `nx` picks (clamped on-screen) and STACK_AY_{TOP,BOT} lands
    // inset by STACK_PAD; the column then grows AWAY from it (down for top, up for bottom).
    const STACK_AX: f32 = STACK_HALF + 4.0; // column centre x in the buffer
    const STACK_AY_TOP: f32 = 6.0; // the top corner's anchor y (column grows downward)
    const STACK_AY_BOT: f32 = (H as f32) - 6.0; // the bottom corner's anchor y (grows upward)

    /// THE MULTI-NOTIFICATION SURFACE — paints one engine frame of the notification stack. The engine
    /// has already eased each slot's animation (y / alpha / scale / collapse / bump), so this is a
    /// pure renderer: lay the cards out as a COLUMN from the corner anchor (top corner → downward,
    /// bottom corner → upward), reusing the card chrome (`card_frame`/`chip_box`) per card. `mode`
    /// picks the presentation: 0 = the column (newest nearest the corner, a `+N` tail past 4), 1 =
    /// Latest (one card; the swap is two slots crossfading, drawn by their alphas), 2 = Digest (the
    /// summary card when >1 is live, else the lone card plain). ALL colour is the live material accent.
    #[allow(clippy::too_many_arguments)]
    fn draw_notify_stack(
        buf: &mut Buffers,
        frame: u32,
        accent: (f32, f32, f32),
        mode: u8,
        place: (f32, f32),
        cards: &[NotifyR],
        digest: Option<&DigestR>,
        tail: u32,
    ) {
        // growth direction from the placement's vertical fraction: top half grows DOWN, bottom up.
        let top_corner = place.1 < 0.5;
        let anchor = if top_corner { STACK_AY_TOP } else { STACK_AY_BOT };
        let card_full_h = |c: &NotifyR| notify_layout(c).height;
        // DIGEST (mode 2) with a live summary → draw the one summary card and nothing else.
        if mode == 2 {
            if let Some(d) = digest {
                draw_digest_card(buf, anchor, top_corner, frame, accent, d);
                return;
            }
        }
        // STACK / LATEST / lone-DIGEST: a column of cards. Each card's `y` is its eased distance from
        // the corner edge; its body grows the card's height. Newest is `y` smallest (nearest corner).
        let mut deepest = 0.0f32;
        for c in cards {
            let h = card_full_h(c) * (1.0 - c.collapse);
            deepest = deepest.max(c.y + h);
            if c.alpha <= 0.004 {
                continue;
            }
            // the card body's vertical CENTRE in the buffer (top corner grows down, bottom grows up).
            let cy_body = if top_corner {
                anchor + c.y + h * 0.5
            } else {
                anchor - c.y - h * 0.5
            };
            draw_stack_card(buf, STACK_AX, cy_body, h, frame, accent, c);
        }
        // the +N TAIL pill (Stack mode only): a quiet "+N more" past the deepest visible card, so an
        // overflowing burst reads as "...and N others" without a fifth full card crowding the column.
        if tail > 0 {
            let pill_cy = if top_corner {
                anchor + deepest + STACK_CARD_GAP + STACK_TAIL_H * 0.5
            } else {
                anchor - deepest - STACK_CARD_GAP - STACK_TAIL_H * 0.5
            };
            draw_tail_pill(buf, STACK_AX, pill_cy, frame, accent, tail);
        }
    }

    /// THE +N TAIL PILL — a quiet rounded chip ("+N more") closing a stack that overflowed past the
    /// visible cards. A short `card_frame` body with a small accent count; never a full card. ALL
    /// colour is the live material accent.
    fn draw_tail_pill(buf: &mut Buffers, cx: f32, cy: f32, frame: u32, accent: (f32, f32, f32), n: u32) {
        let breath = 0.82 + 0.18 * ((frame as f32) * 0.05).sin();
        card_frame(buf, cx, cy, STACK_HALF - 1.0, STACK_TAIL_H * 0.5, 9.0, accent, breath, false);
        // SAFETY: same GDI text rasterization the render thread does everywhere; we're on that thread.
        if let Some(t) = unsafe { rasterize_text(&format!("+{n} more"), 13) } {
            let bx = cx; // centred in the pill
            blit_mask(&mut buf.pr, &t, bx, cy, 0.66 * accent.0);
            blit_mask(&mut buf.pg, &t, bx, cy, 0.66 * accent.1);
            blit_mask(&mut buf.pb, &t, bx, cy, 0.66 * accent.2);
            blit_mask(&mut buf.white, &t, bx, cy, 0.08);
        }
    }

    /// One card's resolved geometry from the SINGLE layout spec (`notifs::card_layout`) — the box
    /// height and every row's y come from here, so the renderer can never draw past the box it sized.
    fn notify_layout(c: &NotifyR) -> crate::notifs::CardLayout {
        crate::notifs::card_layout(crate::notifs::CardShape {
            is_ask: c.is_ask,
            lines: c.body.len().max(1),
            has_track: c.fill >= 0.0,
            has_prev: c.prev.is_some(),
        })
    }

    /// Draw ONE stack card at body-centre (cx, cy) with full height `h`. A confirmation/macro card is
    /// the value-hierarchy (glyph chip + title + a value that WRAPS to ≤3 body lines + the track-bar +
    /// prev); an ASK card (`c.is_ask`) is the prompt layout (Ask glyph + prominent question + the
    /// answer-grammar row). Every element is dimmed by the slot's `alpha`, the chrome swells a hair on
    /// the coalesce `scale` bump, with a brief white value-FLASH on `bump`. ALL colour is the live
    /// material accent.
    fn draw_stack_card(
        buf: &mut Buffers,
        cx: f32,
        cy: f32,
        h: f32,
        frame: u32,
        accent: (f32, f32, f32),
        c: &NotifyR,
    ) {
        let a = c.alpha.clamp(0.0, 1.0);
        // the coalesce BUMP: a quick ~1.06× pop on the chrome (the text stays put — re-rastering it
        // per frame would be waste) + a brighter tab breath, so a card that just updated visibly
        // "lands" again. `scale` rides 1.0→~1.06→1.0 over BUMP_MS (the engine's easing).
        let pop = (c.scale - 1.0).max(0.0);
        let breath = (0.82 + 0.18 * ((frame as f32) * 0.05).sin()) * (1.0 + pop * 2.5);
        let half = STACK_HALF;
        let ry = (h * 0.5).max(8.0);
        // CHROME — the same crafted frame, its half-extents nudged out by the bump pop and its tab
        // breath lifted by the slot alpha so an entering card's tab fades up with it.
        let grow = pop * 6.0; // px the frame swells at the bump's peak
        card_frame(buf, cx, cy, half - 1.0 + grow, ry + grow, 12.0, accent, breath * a, c.panel);
        // the accent border itself also fades with alpha: re-tint a quiet wash so a fading card dims
        // its hairline too (card_frame's border is at full accent; layer a small dimming isn't easy,
        // so we accept the chrome at full and fade the CONTENT — the dominant visual — by alpha).
        // LAYOUT — left-anchored block past the tab + glyph chip (fixed column width).
        let lw = c.title.as_ref().map(|t| t.w).unwrap_or(0) as f32;
        let vw = c.value.as_ref().map(|t| t.w).unwrap_or(0) as f32;
        let bw = c.body.iter().map(|t| t.w as f32).fold(0.0, f32::max);
        let pw = c.prev.as_ref().map(|t| t.w).unwrap_or(0) as f32;
        let arrow = if c.dir != 0 { NOTIFY_ARROW } else { 0.0 };
        let text_w = lw.max(vw + arrow).max(bw).max(pw);
        let gx = cx - half + NOTIFY_TAB + NOTIFY_GLYPH_BOX / 2.0 + 4.0; // chip centre
        let tx = cx - half + NOTIFY_TAB + NOTIFY_GLYPH_BOX + NOTIFY_GAP + 4.0; // text block left
        // EVERY row y comes from the ONE layout spec that sized the box, so content fits by construction.
        let lay = notify_layout(c);
        let top = cy - ry;
        // the glyph chip — vertically centred on the card body.
        chip_box(buf, gx, cy, NOTIFY_CHIP_HALF, 7.0, accent);
        draw_wedge_glyph(buf, c.glyph, gx, cy, NOTIFY_GLYPH_R, Tone::Plain, frame, 0.92 * a);
        // ── THE ASK SLOT — same chrome + Ask glyph chip as above, but the ask LAYOUT (a prompt, not
        // a value-hierarchy): the QUESTION prominent and the answer GRAMMAR on its OWN legible row,
        // the content/typography of `draw_ask_card` reused but drawn here AS a stack slot. It reads as
        // "awaiting you" — a gentle persistent accent (no glow blob, no track-bar). Persistent: the
        // engine never expires it, so it just sits here until a ClearAsk fades it out.
        if c.is_ask {
            // TOP-anchored from the card's inner top edge so the prompt scales cleanly when the
            // grammar wraps to a second row (the card height grows with `body.len()`).
            let qy = top + lay.body0_cy;
            // the QUESTION — the prominent line (a clean accent fill + a thin white legibility core).
            if let Some(t) = &c.value {
                let bx = tx + t.w as f32 / 2.0;
                blit_mask(&mut buf.pr, t, bx, qy, 0.95 * accent.0 * a);
                blit_mask(&mut buf.pg, t, bx, qy, 0.95 * accent.1 * a);
                blit_mask(&mut buf.pb, t, bx, qy, 0.95 * accent.2 * a);
                blit_mask(&mut buf.white, t, bx, qy, 0.18 * a);
            }
            // the answer GRAMMAR — wrapped to fit (carried in `body`), each line its own readable row
            // under the question. Quieter than the question, but never spills past the card edge.
            for (li, t) in c.body.iter().enumerate() {
                let gyy = top + lay.grammar0_cy + li as f32 * lay.body_step;
                let bx = tx + t.w as f32 / 2.0;
                blit_mask(&mut buf.pr, t, bx, gyy, 0.62 * accent.0 * a);
                blit_mask(&mut buf.pg, t, bx, gyy, 0.62 * accent.1 * a);
                blit_mask(&mut buf.pb, t, bx, gyy, 0.62 * accent.2 * a);
                blit_mask(&mut buf.white, t, bx, gyy, 0.08 * a);
            }
            return;
        }
        // title near the top, then the body lines, then track + prev — every y from `lay`.
        let ty = top + lay.title_cy;
        if let Some(t) = &c.title {
            let bx = tx + t.w as f32 / 2.0;
            blit_mask(&mut buf.pr, t, bx, ty, 0.58 * accent.0 * a);
            blit_mask(&mut buf.pg, t, bx, ty, 0.58 * accent.1 * a);
            blit_mask(&mut buf.pb, t, bx, ty, 0.58 * accent.2 * a);
            blit_mask(&mut buf.white, t, bx, ty, 0.10 * a);
        }
        // the VALUE — wrapped body lines, the headline. The FIRST line carries the ↑/↓ cue + a brief
        // white flash on a coalesce bump; the eye lands here.
        let value_top = top + lay.body0_cy;
        if c.body.is_empty() {
            // no body raster (shouldn't happen) — fall back to the single value raster.
            if let Some(t) = &c.value {
                let bx = tx + t.w as f32 / 2.0;
                blit_mask(&mut buf.pr, t, bx, value_top, accent.0 * a);
                blit_mask(&mut buf.pg, t, bx, value_top, accent.1 * a);
                blit_mask(&mut buf.pb, t, bx, value_top, accent.2 * a);
                blit_mask(&mut buf.white, t, bx, value_top, (0.16 + 0.5 * c.bump) * a);
            }
        } else {
            for (li, t) in c.body.iter().enumerate() {
                let ly = value_top + li as f32 * lay.body_step;
                let bx = tx + t.w as f32 / 2.0;
                blit_mask(&mut buf.pr, t, bx, ly, accent.0 * a);
                blit_mask(&mut buf.pg, t, bx, ly, accent.1 * a);
                blit_mask(&mut buf.pb, t, bx, ly, accent.2 * a);
                // the value flash rides the first line only.
                let core = if li == 0 { 0.16 + 0.5 * c.bump } else { 0.12 };
                blit_mask(&mut buf.white, t, bx, ly, core * a);
                if li == 0 && c.dir != 0 {
                    let axx = bx + t.w as f32 / 2.0 + NOTIFY_ARROW * 0.5;
                    draw_dir_arrow(
                        buf,
                        axx,
                        ly,
                        NOTIFY_ARROW * 0.46,
                        c.dir,
                        (accent.0 * a, accent.1 * a, accent.2 * a),
                    );
                }
            }
        }
        // the track-bar (ranged only), under the body block.
        let has_bar = c.fill >= 0.0;
        let track_y = top + lay.track_cy;
        if has_bar {
            track_bar(
                buf,
                tx,
                tx + text_w,
                track_y,
                c.fill,
                c.prev_fill,
                (accent.0 * a, accent.1 * a, accent.2 * a),
            );
        }
        // prev — tiny + dim, trailing under the value/track.
        if let Some(t) = &c.prev {
            let py = top + lay.prev_cy;
            let bx = tx + t.w as f32 / 2.0;
            blit_mask(&mut buf.glow, t, bx, py, 0.40 * a);
            blit_mask(&mut buf.white, t, bx, py, 0.05 * a);
        }
    }

    /// THE DIGEST CARD — the calm-during-chaos summary (>1 note live): a big COUNT on the left, a ROW
    /// of the DISTINCT source glyphs (deduped by kind — DPI reticle + macro terminal + …) so you learn
    /// WHAT fired at a glance, and the latest note's title/value line. An expand-on-settle `reveal`
    /// slides the glyph row + line open as the count lands. ALL colour is the live material accent.
    fn draw_digest_card(
        buf: &mut Buffers,
        anchor: f32,
        top_corner: bool,
        frame: u32,
        accent: (f32, f32, f32),
        d: &DigestR,
    ) {
        let a = d.alpha.clamp(0.0, 1.0);
        let rv = d.reveal.clamp(0.0, 1.0);
        let breath = 0.82 + 0.18 * ((frame as f32) * 0.05).sin();
        let h = 84.0;
        let ry = h * 0.5;
        let cx = STACK_AX;
        let cy = if top_corner { anchor + ry } else { anchor - ry };
        let half = STACK_HALF;
        card_frame(buf, cx, cy, half - 1.0, ry, 12.0, accent, breath * a, true);
        // the COUNT — a large accent numeral on the left (a growing badge).
        let count_cx = cx - half + 34.0;
        if let Some(t) = &d.count_txt {
            let by = cy - 12.0;
            blit_mask(&mut buf.pr, t, count_cx, by, accent.0 * a);
            blit_mask(&mut buf.pg, t, count_cx, by, accent.1 * a);
            blit_mask(&mut buf.pb, t, count_cx, by, accent.2 * a);
            blit_mask(&mut buf.white, t, count_cx, by, 0.18 * a);
        }
        // the GLYPH ROW — the distinct source icons, each in a small chip, sliding open with reveal.
        let row_x0 = cx - half + 60.0;
        let row_y = cy - 14.0;
        let step = 26.0;
        for (i, g) in d.glyphs.iter().enumerate() {
            let gx = row_x0 + i as f32 * step * rv;
            let ga = a * rv;
            chip_box(buf, gx, row_y, 11.0, 4.0, accent);
            draw_wedge_glyph(buf, *g, gx, row_y, 8.0, Tone::Plain, frame, 0.9 * ga);
        }
        // the LATEST LINE — the most recent note's title + value, under the glyph row (revealing).
        let line_y = cy + 16.0;
        let line_a = a * rv;
        let lx = row_x0;
        if let Some(t) = &d.title {
            let bx = lx + t.w as f32 / 2.0;
            blit_mask(&mut buf.pr, t, bx, line_y - 9.0, 0.58 * accent.0 * line_a);
            blit_mask(&mut buf.pg, t, bx, line_y - 9.0, 0.58 * accent.1 * line_a);
            blit_mask(&mut buf.pb, t, bx, line_y - 9.0, 0.58 * accent.2 * line_a);
        }
        if let Some(t) = &d.value {
            let bx = lx + t.w as f32 / 2.0;
            blit_mask(&mut buf.pr, t, bx, line_y + 8.0, accent.0 * line_a);
            blit_mask(&mut buf.pg, t, bx, line_y + 8.0, accent.1 * line_a);
            blit_mask(&mut buf.pb, t, bx, line_y + 8.0, accent.2 * line_a);
            blit_mask(&mut buf.white, t, bx, line_y + 8.0, 0.14 * line_a);
        }
    }

    /// THE CARD FRAME — the notification card's crafted chrome: a crisp dark SDF rounded-rect
    /// (~1px feather, NOT a glow), a quiet accent hairline hugging the inner edge,
    /// and the signature ACCENT TAB — a short rounded bar down the left edge that makes the card read
    /// as a deliberate instrument rather than a floating lozenge. `panel` (grounded) fills a hair
    /// darker and squarer; the floating card is lighter and softer-cornered, but both share the same
    /// defined geometry. `accent` (the live material accent) is the whole chrome colour — border, tab,
    /// and fill tint; `breath` (0..1) gently lifts the tab.
    #[allow(clippy::too_many_arguments)]
    fn card_frame(
        buf: &mut Buffers,
        cx: f32,
        cy: f32,
        rx: f32,
        ry: f32,
        radius: f32,
        accent: (f32, f32, f32),
        breath: f32,
        panel: bool,
    ) {
        let (rx, ry) = (rx.max(2.0), ry.max(2.0));
        let r = radius.min(rx).min(ry);
        let pad = 2.0;
        let x0 = ((cx - rx - pad).floor() as i32).max(0);
        let x1 = ((cx + rx + pad).ceil() as i32).min(W - 1);
        let y0 = ((cy - ry - pad).floor() as i32).max(0);
        let y1 = ((cy + ry + pad).ceil() as i32).min(H - 1);
        // grounded panel sits darker (a real card over a game); floating a little lighter.
        let fill_k = if panel { 0.88 } else { 0.62 };
        let edge_k = if panel { 0.50 } else { 0.42 };
        // the accent TAB lives just inside the left edge: a thin rounded bar, ~55% of the body height.
        let tab_h = ry * 0.55;
        let tab_x = cx - rx + 4.0; // its centre x, a hair in from the rim
        let tab_hw = NOTIFY_TAB * 0.5; // half-width
        for yy in y0..=y1 {
            let row = (yy * W) as usize;
            let fy = yy as f32;
            let dy = ((fy - cy).abs() - (ry - r)).max(0.0);
            for xx in x0..=x1 {
                let fx = xx as f32;
                let dx = ((fx - cx).abs() - (rx - r)).max(0.0);
                let d = (dx * dx + dy * dy).sqrt() - r; // signed distance to the rounded-rect edge
                let i = row + xx as usize;
                // fill: dark inside, a ~1px feather across the edge for clean corners.
                let fillv = (0.5 - d).clamp(0.0, 1.0);
                if fillv > 0.001 {
                    buf.shade[i] += fill_k * fillv;
                }
                // border: a quiet accent hairline hugging just inside the edge.
                let edge = (1.0 - (d + 0.9).abs()).clamp(0.0, 1.0);
                if edge > 0.001 {
                    let bc = edge * edge_k;
                    buf.pr[i] += bc * accent.0;
                    buf.pg[i] += bc * accent.1;
                    buf.pb[i] += bc * accent.2;
                }
                // the signature accent TAB — a crisp rounded bar (its own little SDF) on the left.
                let tdx = (fx - tab_x).abs() - (tab_hw - 1.0);
                let tdy = (fy - cy).abs() - (tab_h - 1.0);
                let td = (tdx.max(0.0).powi(2) + tdy.max(0.0).powi(2)).sqrt() - 1.0;
                let tabv = (0.6 - td).clamp(0.0, 1.0);
                if tabv > 0.001 {
                    let tb = tabv * (0.55 + 0.30 * breath);
                    buf.pr[i] += tb * accent.0;
                    buf.pg[i] += tb * accent.1;
                    buf.pb[i] += tb * accent.2;
                }
            }
        }
    }

    /// THE GLYPH CHIP — a precise hairline-bordered badge for the card's type glyph: a crisp SDF
    /// rounded square with a faint accent-tinted dark fill and a clean accent hairline border, centred
    /// at (cx, cy). A designed home for the mark (no glow halo); the icon is drawn crisp on top by the
    /// caller. `half` is its half-size, `radius` the corner round, `accent` the live material accent.
    fn chip_box(buf: &mut Buffers, cx: f32, cy: f32, half: f32, radius: f32, accent: (f32, f32, f32)) {
        let half = half.max(2.0);
        let r = radius.min(half);
        let pad = 2.0;
        let x0 = ((cx - half - pad).floor() as i32).max(0);
        let x1 = ((cx + half + pad).ceil() as i32).min(W - 1);
        let y0 = ((cy - half - pad).floor() as i32).max(0);
        let y1 = ((cy + half + pad).ceil() as i32).min(H - 1);
        for yy in y0..=y1 {
            let row = (yy * W) as usize;
            let dy = ((yy as f32 - cy).abs() - (half - r)).max(0.0);
            for xx in x0..=x1 {
                let dx = ((xx as f32 - cx).abs() - (half - r)).max(0.0);
                let d = (dx * dx + dy * dy).sqrt() - r;
                let i = row + xx as usize;
                // fill: a touch darker than the card so the chip reads as inset; an accent whisper in it.
                let fillv = (0.5 - d).clamp(0.0, 1.0);
                if fillv > 0.001 {
                    buf.shade[i] += 0.32 * fillv;
                    buf.pr[i] += fillv * accent.0 * 0.06;
                    buf.pg[i] += fillv * accent.1 * 0.06;
                    buf.pb[i] += fillv * accent.2 * 0.06;
                }
                // border: a clean accent hairline.
                let edge = (1.0 - (d + 0.9).abs()).clamp(0.0, 1.0);
                if edge > 0.001 {
                    buf.pr[i] += edge * accent.0 * 0.55;
                    buf.pg[i] += edge * accent.1 * 0.55;
                    buf.pb[i] += edge * accent.2 * 0.55;
                }
            }
        }
    }

    /// THE TRACK-BAR — the card's signature instrument detail (ranged changes only). A thin crisp SDF
    /// rail from `bx0` to `bx1` at row `cy`: a dim accent track (the range), a bright accent FILL from
    /// the left to `fill` (the new reading's place on the range), a clean cap dot at the fill head, and
    /// — when `prev_fill >= 0` — a small TICK at the old reading, so the move along the range reads at
    /// a glance (DPI 800→30000 shows a tick low-left and a fill nearly full). All SDF/stroke, no glow.
    fn track_bar(
        buf: &mut Buffers,
        bx0: f32,
        bx1: f32,
        cy: f32,
        fill: f32,
        prev_fill: f32,
        accent: (f32, f32, f32),
    ) {
        let bx1 = bx1.max(bx0 + 6.0);
        let w = bx1 - bx0;
        let fill = fill.clamp(0.0, 1.0);
        let half_h = 1.6; // the rail's half-thickness — a hairline bar
        let x0 = (bx0.floor() as i32 - 1).max(0);
        let x1 = (bx1.ceil() as i32 + 1).min(W - 1);
        let y0 = ((cy - half_h - 2.0).floor() as i32).max(0);
        let y1 = ((cy + half_h + 2.0).ceil() as i32).min(H - 1);
        let fx = bx0 + w * fill; // the fill head
        for yy in y0..=y1 {
            let row = (yy * W) as usize;
            let dy = (yy as f32 - cy).abs() - half_h;
            for xx in x0..=x1 {
                let fxx = xx as f32;
                // horizontal SDF: inside the rail's x-span, rounded ends.
                let dxl = bx0 - fxx;
                let dxr = fxx - bx1;
                let dx = dxl.max(dxr).max(0.0);
                let d = (dx * dx + dy.max(0.0) * dy.max(0.0)).sqrt() - 0.6;
                let cov = (0.6 - d).clamp(0.0, 1.0);
                if cov <= 0.001 {
                    continue;
                }
                let i = row + xx as usize;
                if fxx <= fx {
                    // the FILL — a clean bright accent segment.
                    buf.pr[i] += cov * accent.0 * 0.95;
                    buf.pg[i] += cov * accent.1 * 0.95;
                    buf.pb[i] += cov * accent.2 * 0.95;
                } else {
                    // the unfilled TRACK — a dim accent rail.
                    buf.pr[i] += cov * accent.0 * 0.22;
                    buf.pg[i] += cov * accent.1 * 0.22;
                    buf.pb[i] += cov * accent.2 * 0.22;
                }
            }
        }
        // a small crisp cap at the fill head so the reading's position is defined.
        splat_prism(buf, fx, cy, 2.3, 0.85, accent);
        splat_white(&mut buf.white, fx, cy, 1.1, 0.18);
        // the PREV TICK — a short bright vertical mark at the old reading, in white so it stands clear
        // of both the hue fill and the dim track (the "where it was" against "where it is").
        if prev_fill >= 0.0 {
            let px = bx0 + w * prev_fill.clamp(0.0, 1.0);
            vline(&mut buf.white, (px, cy - 3.4), (px, cy + 3.4), 1.0, 0.6);
            // an accent underglow on the tick so it still belongs to the bar's palette.
            vline(&mut buf.pr, (px, cy - 3.0), (px, cy + 3.0), 1.4, accent.0 * 0.35);
            vline(&mut buf.pg, (px, cy - 3.0), (px, cy + 3.0), 1.4, accent.1 * 0.35);
            vline(&mut buf.pb, (px, cy - 3.0), (px, cy + 3.0), 1.4, accent.2 * 0.35);
        }
    }

    /// A small procedural ↑ (up, `dir > 0`) or ↓ (down) arrow centred at (cx, cy), in the prism
    /// `rgb` — the notification card's ranged-direction cue. `s` is the half-height; the stem is a
    /// short vertical line capped by an arrowhead at the pointing end.
    fn draw_dir_arrow(buf: &mut Buffers, cx: f32, cy: f32, s: f32, dir: i8, rgb: (f32, f32, f32)) {
        let sr = 1.7;
        let b = 0.95;
        let put = |buf: &mut Buffers, a: (f32, f32), c: (f32, f32)| {
            // draw the segment into all three prism channels (an undispersed hue stroke)
            vline(&mut buf.pr, a, c, sr, b * rgb.0);
            vline(&mut buf.pg, a, c, sr, b * rgb.1);
            vline(&mut buf.pb, a, c, sr, b * rgb.2);
        };
        let (top, bot) = (cy - s, cy + s);
        put(buf, (cx, top), (cx, bot)); // the stem
        if dir > 0 {
            // ↑ : head at the top
            put(buf, (cx - s * 0.6, top + s * 0.55), (cx, top));
            put(buf, (cx + s * 0.6, top + s * 0.55), (cx, top));
        } else {
            // ↓ : head at the bottom
            put(buf, (cx - s * 0.6, bot - s * 0.55), (cx, bot));
            put(buf, (cx + s * 0.6, bot - s * 0.55), (cx, bot));
        }
    }

    /// A faint additive ring centered on the anchor (the sigil), with tick marks; rotates with the
    /// frame and brightens with `bright`.
    /// The rune ring, in hard light: an octagonal construct of phosphor light with a prismatic
    /// edge and crystalline nodes, wrapped around a counter-rotating inner hexagon lattice — a
    /// sigil *built*, not smudged. Same Symmetra grammar as the KNOCKBACK constructs, so every
    /// weave reads as one school of magic.
    fn rune_ring(buf: &mut Buffers, radius: f32, bright: f32, frame: u32, _rng: &mut Rng) {
        let rot = (frame as f32) * 0.008;
        let g = (0.29, 0.95, 0.69); // phosphor — the player's hard light
        hard_construct(buf, CX, CY, radius, 8, rot, g, bright * 0.95, false);
        hard_construct(
            buf,
            CX,
            CY,
            radius * 0.62,
            6,
            -rot * 1.6,
            g,
            bright * 0.5,
            false,
        );
    }

    /// A bright stroked ring (used by the recognition flare).
    fn ring_stroke(white: &mut [f32], radius: f32, bright: f32) {
        let n = 140;
        for k in 0..n {
            let a = (k as f32 / n as f32) * TAU;
            splat(
                white,
                CX + a.cos() * radius,
                CY + a.sin() * radius,
                2.4,
                bright * 0.7,
            );
        }
    }

    /// The beacon's wheel: an east rim arc in the user's ACCENT (YES — affirm wears your colour),
    /// a west rim arc in the material's warm RED (NO), and dim neutral ticks at the poles (PASS).
    /// Idle (no aim) it breathes gently — a beacon, not an alarm. An aimed zone goes hot.
    /// `live`: -1 = nothing aimed, 0 = yes, 1 = no, 2 = pass; `aim_south` picks the pass pole.
    /// `accent` = the weave material's accent (linear rgb) — YES's substance.
    /// THE prompt wheel — the radial core rendered for ANSWERING. One wedge per option (a rim arc + an
    /// anchor node + its label) placed by `radial::wedge_bearing`, the aimed one lit via the SAME
    /// `radial::pick_wedge` the beacon commits with (so highlight and verdict can't disagree). Option 0
    /// wears the user's ACCENT (the affirm colour), the rest the raw weave MATERIAL — so N=2 is the
    /// familiar accent-yes (west) / material-no (east), and an N-way choose reads as one accent option
    /// among material ones, told apart by their labels. Pass = a committed flick into a gap (drawn by
    /// the caller). One renderer; the simple and the rich both fall out.
    fn prompt_wheel(
        buf: &mut Buffers,
        labels: &[Option<TextRaster>],
        aim: (f32, f32),
        frame: u32,
        flare: f32,
        accent: (f32, f32, f32),
    ) {
        let n = labels.len();
        if n == 0 {
            return;
        }
        let rad = (CX - 26.0).min(150.0);
        let breathe = 0.72 + 0.28 * ((frame as f32) * 0.06).sin();
        let aimed_mag = (aim.0 * aim.0 + aim.1 * aim.1).sqrt();
        let idle = aimed_mag < 6.0;
        // the aimed wedge — a small threshold so it lights as you move, well before the commit radius.
        let live = neuron::radial::pick_wedge(aim.0 as f64, aim.1 as f64, 6.0, n);
        let half = (neuron::radial::wedge_arc(n) as f32) * 0.5;
        for i in 0..n {
            let b = neuron::radial::wedge_bearing(i, n) as f32; // atan2(dy,dx): E=0, S=+, W=±π, N=−
            let is_accent = i == 0;
            let hot = live == Some(i);
            // the rim arc spanning this wedge (dense splats → a continuous arc, not beads).
            let span = half * 2.0;
            let samples = ((span / TAU) * 480.0).ceil().max(6.0) as usize;
            for k in 0..samples {
                let a = b - half + span * (k as f32 / (samples.max(2) - 1) as f32);
                let (x, y) = (CX + a.cos() * rad, CY + a.sin() * rad);
                let base = if idle {
                    0.30 * breathe
                } else if hot {
                    0.62
                } else {
                    0.14
                };
                if is_accent {
                    splat_prism(buf, x, y, 2.8, base, accent);
                } else {
                    splat(&mut buf.glow, x, y, 2.8, base);
                }
            }
            // the anchor node (the target you flick toward) + a spine from centre when it's aimed.
            let (nx, ny) = (CX + b.cos() * rad, CY + b.sin() * rad);
            let node_r = if hot { 13.0 } else { 8.0 };
            let node_a = if hot { 0.95 + flare } else { 0.35 * breathe };
            if is_accent {
                splat_prism(buf, nx, ny, node_r, node_a, accent);
            } else {
                splat(&mut buf.glow, nx, ny, node_r, node_a);
                if hot {
                    splat(&mut buf.white, nx, ny, 5.0, 0.8);
                }
            }
            if hot {
                let steps = 20;
                for j in 0..=steps {
                    let u = j as f32 / steps as f32;
                    let (sx, sy) = (CX + b.cos() * rad * u, CY + b.sin() * rad * u);
                    let sb = 0.5 * (0.3 + u);
                    if is_accent {
                        splat_prism(buf, sx, sy, 4.5, sb, accent);
                    } else {
                        splat(&mut buf.glow, sx, sy, 4.5, sb);
                    }
                }
            }
            // the option LABEL, just inside its node — accent (prism+core) for option 0, else the
            // material (glow+core), so a label always shades like the wedge it names.
            if let Some(t) = &labels[i] {
                let (lx, ly) = (CX + b.cos() * rad * 0.62, CY + b.sin() * rad * 0.62);
                let lhot = if hot { 1.0 } else { 0.55 };
                text_pocket(buf, t, lx, ly, 0.30);
                if is_accent {
                    blit_mask(&mut buf.pr, t, lx, ly, lhot * accent.0);
                    blit_mask(&mut buf.pg, t, lx, ly, lhot * accent.1);
                    blit_mask(&mut buf.pb, t, lx, ly, lhot * accent.2);
                    blit_mask(&mut buf.white, t, lx, ly, lhot * 0.28);
                } else {
                    blit_mask(&mut buf.glow, t, lx, ly, lhot);
                    blit_mask(&mut buf.white, t, lx, ly, lhot * 0.28);
                }
            }
        }
    }

    /// Additively fill a canvas-center-relative rect into a float buffer (the map's blobs).
    fn fill_rect(buf: &mut [f32], r: &[f32; 4], b: f32) {
        let x0 = ((CX + r[0]).floor() as i32).clamp(0, W - 1);
        let x1 = ((CX + r[2]).ceil() as i32).clamp(0, W - 1);
        let y0 = ((CY + r[1]).floor() as i32).clamp(0, H - 1);
        let y1 = ((CY + r[3]).ceil() as i32).clamp(0, H - 1);
        for y in y0..=y1 {
            let row = y * W;
            for x in x0..=x1 {
                buf[(row + x) as usize] += b;
            }
        }
    }

    /// Hairline-stroke a canvas-center-relative rect via splats along its edges.
    fn rect_stroke(buf: &mut [f32], r: &[f32; 4], b: f32) {
        let (x0, y0, x1, y1) = (CX + r[0], CY + r[1], CX + r[2], CY + r[3]);
        let step = 3.0;
        let mut x = x0;
        while x <= x1 {
            splat(buf, x, y0, 1.6, b);
            splat(buf, x, y1, 1.6, b);
            x += step;
        }
        let mut y = y0;
        while y <= y1 {
            splat(buf, x0, y, 1.6, b);
            splat(buf, x1, y, 1.6, b);
            y += step;
        }
    }

    /// Hairline-stroke a rect into the PRISM channel (the carried phantom's chromatic fringe).
    fn rect_stroke_rgb(buf: &mut Buffers, r: &[f32; 4], b: f32, rgb: (f32, f32, f32)) {
        let (x0, y0, x1, y1) = (CX + r[0], CY + r[1], CX + r[2], CY + r[3]);
        let step = 3.0;
        let mut put = |x: f32, y: f32| {
            splat(&mut buf.pr, x, y, 1.6, b * rgb.0);
            splat(&mut buf.pg, x, y, 1.6, b * rgb.1);
            splat(&mut buf.pb, x, y, 1.6, b * rgb.2);
        };
        let mut x = x0;
        while x <= x1 {
            put(x, y0);
            put(x, y1);
            x += step;
        }
        let mut y = y0;
        while y <= y1 {
            put(x0, y);
            put(x1, y);
            y += step;
        }
    }

    /// THE SELECTION MANIFESTATION — Directed-Intent around a minimap cell (the desk blob you're
    /// aiming, or a realm window on hover): a white-cored rim that splits into a chromatic
    /// aberration fringe at the edges (the monitor's-own-LEDs look the user loves), plus a WAVEFORM
    /// of diffused sparkles that travels around the perimeter — the cell reads as EMERGING, half
    /// in / half out, not a drawn box. Same material the comet trail is made of: facet-quantized
    /// prism hues, hnoise twinkle, slow temporal drift. `amt` (0..1) scales the whole bloom so the
    /// realm cards can manifest a touch softer than the desk's hot pick. This IS the "glance border
    /// magic": the scry portal now hugs this exact cell, so its frame wears the selection effect.
    fn cell_select(buf: &mut Buffers, r: &[f32; 4], frame: u32, amt: f32) {
        let (x0, y0, x1, y1) = (CX + r[0], CY + r[1], CX + r[2], CY + r[3]);
        // 1) the rim: a white core stroke with a cool/warm prism fringe a hair to either side —
        //    white diffusing into the spectrum exactly at the edge (the aberration read).
        let off = 1.4;
        let inner = [r[0] + off, r[1] + off, r[2] - off, r[3] - off];
        let outer = [r[0] - off, r[1] - off, r[2] + off, r[3] + off];
        rect_stroke(&mut buf.white, r, 0.55 * amt);
        rect_stroke_rgb(buf, &inner, 0.26 * amt, REFRACT_COOL);
        rect_stroke_rgb(buf, &outer, 0.26 * amt, REFRACT_WARM);
        // 2) the sparkle waveform: points walked around the perimeter, each twinkling on its own
        //    hnoise but GATED by a sine that sweeps the rect — a wave of emergence, not static
        //    glitter. Hue is facet-quantized (cut-glass bands, the body's faceting), drifting slow.
        let facets = crate::weave::material().facets.max(1) as f32;
        let (w, h) = ((x1 - x0).max(1.0), (y1 - y0).max(1.0));
        let perim = 2.0 * (w + h);
        let n = (perim / 22.0).clamp(8.0, 40.0) as i32; // density scales with cell size
        let phase = frame as f32 * 0.05;
        for k in 0..n {
            let u = k as f32 / n as f32;
            // walk the rectangle border by arc-fraction u
            let d = u * perim;
            let (sx, sy) = if d < w {
                (x0 + d, y0)
            } else if d < w + h {
                (x1, y0 + (d - w))
            } else if d < 2.0 * w + h {
                (x1 - (d - w - h), y1)
            } else {
                (x0, y1 - (d - 2.0 * w - h))
            };
            // the travelling wave: a bright crest sweeping around, broken by per-point noise so it
            // shimmers instead of marching like a clean dot.
            let wave = 0.5 + 0.5 * (u * TAU * 2.0 - phase).sin();
            let tw = hnoise(k, k >> 1, frame >> 3);
            let spark = (wave * 0.7 + 0.3) * tw * amt;
            if spark > 0.06 {
                let hue = (k as f32 * 12.0 + frame as f32 * 2.0) / 360.0;
                let hue = (hue * facets).round() / facets * 360.0;
                splat_prism(buf, sx, sy, 1.9, 0.5 * spark, hsv(hue, 0.85, 1.0));
                splat_white(&mut buf.white, sx, sy, 0.8, 0.5 * spark);
            }
        }
    }

    /// A WARPSTONE pip on the map — where a tether anchor lives (so you can see your stones while
    /// you aim). A small phosphor ring with a white heart, breathing gently: the same rune the
    /// landing marker wears, miniature, in the directed-intent palette (an anchor is a place you
    /// CAN return to, shown quietly — not a destructive state, so it stays phosphor, never warn).
    fn warpstone_pip(buf: &mut Buffers, cx: f32, cy: f32, frame: u32) {
        let breathe = 0.78 + 0.22 * ((frame as f32) * 0.08).sin();
        // a faint cross-tick ring (a moored stone) + a white core bead
        let rr = 4.6;
        for k in 0..18 {
            let a = (k as f32 / 18.0) * TAU;
            splat(
                &mut buf.glow,
                cx + a.cos() * rr,
                cy + a.sin() * rr,
                1.4,
                0.5 * breathe,
            );
        }
        splat(&mut buf.glow, cx, cy, 3.0, 0.35 * breathe);
        splat(&mut buf.white, cx, cy, 1.7, 0.85 * breathe);
    }

    /// Shorten a caption to `max` chars with an ellipsis (the strip/wheel is not a text editor).
    fn ellipsize(s: &str, max: usize) -> String {
        if s.chars().count() > max {
            s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
        } else {
            s.to_string()
        }
    }

    /// WRAP `s` to ≤`lines` rows, honouring EXPLICIT '\n' breaks FIRST — each segment is wrapped within
    /// the remaining budget, so STRUCTURED text (a grammar's "engage\noptions") keeps its groups on
    /// their own rows instead of greedily splitting a list mid-way. Plain text (no '\n') wraps exactly
    /// as one greedy block, unchanged.
    fn wrap_lines(s: &str, cols: usize, lines: usize) -> Vec<String> {
        let lines = lines.max(1);
        let mut out: Vec<String> = Vec::new();
        for seg in s.split('\n') {
            if out.len() >= lines {
                break;
            }
            out.extend(wrap_one(seg, cols, lines - out.len()));
        }
        out
    }

    /// Greedy word-wrap of ONE line (no '\n') to ≤`lines` rows of ≤`cols` chars (hard char-break for an
    /// over-long word), ellipsizing the LAST row if the text still overruns.
    fn wrap_one(s: &str, cols: usize, lines: usize) -> Vec<String> {
        let cols = cols.max(1);
        let lines = lines.max(1);
        if s.trim().is_empty() {
            return Vec::new();
        }
        let mut out: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut cur_len = 0usize;
        // greedy fit of whitespace-split words; a single over-long word is hard-split at the column.
        for word in s.split_whitespace() {
            let wlen = word.chars().count();
            if cur_len == 0 {
                if wlen <= cols {
                    cur = word.to_string();
                    cur_len = wlen;
                } else {
                    // hard-break the giant word across rows
                    let mut chars = word.chars().peekable();
                    while chars.peek().is_some() {
                        let chunk: String = chars.by_ref().take(cols).collect();
                        out.push(chunk);
                        if out.len() >= lines {
                            break;
                        }
                    }
                    if out.len() >= lines {
                        break;
                    }
                    cur.clear();
                    cur_len = 0;
                }
            } else if cur_len + 1 + wlen <= cols {
                cur.push(' ');
                cur.push_str(word);
                cur_len += 1 + wlen;
            } else {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
                if out.len() >= lines {
                    break;
                }
                // re-process this word as the start of the next row
                if wlen <= cols {
                    cur = word.to_string();
                    cur_len = wlen;
                } else {
                    let mut chars = word.chars().peekable();
                    while chars.peek().is_some() {
                        let chunk: String = chars.by_ref().take(cols).collect();
                        out.push(chunk);
                        if out.len() >= lines {
                            break;
                        }
                    }
                    if out.len() >= lines {
                        break;
                    }
                }
            }
        }
        if out.len() < lines && !cur.is_empty() {
            out.push(cur);
        }
        // if there's leftover content we couldn't fit, ellipsize the final row.
        let consumed: usize = out.iter().map(|l| l.chars().count()).sum::<usize>();
        let total = s.split_whitespace().map(|w| w.chars().count()).sum::<usize>();
        if out.len() >= lines && consumed < total {
            if let Some(last) = out.last_mut() {
                *last = ellipsize(last, cols);
            }
        }
        out.truncate(lines);
        out
    }

    /// The cursor's monitor work area (left, top, right, bottom) — full-virtual-screen fallback.
    /// An AUTO-HIDE taskbar reserves NO work area (rcWork == rcMonitor), so a bottom-corner card would
    /// land where the bar pops up and read as "behind the taskbar". We reserve the taskbar's OWN edge
    /// ourselves (via the AppBar API, which reports it even when hidden), but only when the work area
    /// didn't already exclude it — so a normal pinned taskbar isn't double-subtracted.
    unsafe fn monitor_work(pt: POINT) -> (i32, i32, i32, i32) {
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if mon.is_null() || GetMonitorInfoW(mon, &mut mi) == 0 {
            return (pt.x - W, pt.y - H, pt.x + W, pt.y + H); // degenerate fallback: centered, unclamped
        }
        let (mut l, mut t, mut r, mut b) = (
            mi.rcWork.left,
            mi.rcWork.top,
            mi.rcWork.right,
            mi.rcWork.bottom,
        );
        let m = mi.rcMonitor;
        if let Some((edge, tb)) = taskbar_rect() {
            // only when the taskbar actually sits on THIS monitor
            let on_mon = tb.right > m.left && tb.left < m.right && tb.bottom > m.top && tb.top < m.bottom;
            if on_mon {
                use windows_sys::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP};
                // floor the reserved thickness at ~48px: an auto-hide bar can report a thin hidden
                // sliver, but it still needs a full taskbar's clearance where it pops up.
                match edge {
                    ABE_BOTTOM if b >= m.bottom => b = m.bottom - (m.bottom - tb.top).max(48),
                    ABE_TOP if t <= m.top => t = m.top + (tb.bottom - m.top).max(48),
                    ABE_LEFT if l <= m.left => l = m.left + (tb.right - m.left).max(48),
                    ABE_RIGHT if r >= m.right => r = m.right - (m.right - tb.left).max(48),
                    _ => {}
                }
            }
        }
        (l, t, r, b)
    }

    /// The Windows taskbar's edge + bounding rect via the AppBar API — reported even for an auto-hide
    /// bar (unlike rcWork or a bare GetWindowRect, which can miss a hidden bar). `None` if unavailable.
    unsafe fn taskbar_rect() -> Option<(u32, windows_sys::Win32::Foundation::RECT)> {
        use windows_sys::Win32::UI::Shell::{SHAppBarMessage, ABM_GETTASKBARPOS, APPBARDATA};
        let mut abd: APPBARDATA = std::mem::zeroed();
        abd.cbSize = std::mem::size_of::<APPBARDATA>() as u32;
        if SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) != 0 {
            Some((abd.uEdge, abd.rc))
        } else {
            None
        }
    }

    /// Where a WEAVE overlay window goes — centered on the cursor (glyph / radial / ask wheel / dial
    /// / map / control / twin). The notification STACK is placed separately by [`stack_origin`] (it
    /// carries the placement fractions the fieldless `NotifyStack` marker doesn't), never through here.
    ///
    /// NOT clamped to the monitor: the buffer is bigger than most monitors, so clamping would pin the
    /// anchor to the monitor's centre instead of the cursor. Letting the window extend off-screen (the
    /// off pixels are transparent) keeps CX/CY exactly under the cursor even casting at a screen edge
    /// — which is what frees the draw area to the whole estate.
    unsafe fn place_window(_mode: &WeaveMode, cur: POINT) -> POINT {
        POINT {
            x: cur.x - W / 2,
            y: cur.y - H / 2,
        }
    }

    /// THE NOTIFICATION STACK's window origin — honours the user's REAL placement `(nx, ny)`, NOT a
    /// snapped corner, so a centre placement renders a CENTRED column and a corner placement hugs that
    /// corner (the same free placement the single card had).
    ///
    /// X — the card block (buffer x∈[STACK_AX-STACK_HALF, STACK_AX+STACK_HALF]) is CENTRED on
    /// `nx`: its screen centre lands at `l + nx*(r-l)`, then the whole block is CLAMPED inside
    /// `[l+pad, r-pad]` so it never runs off-screen (nx≈0.5 → centred, nx≈1 → hugs the right,
    /// nx≈0 → hugs the left). We keep STACK_AX as the buffer centre and shift the WINDOW (like the
    /// single card did via `place_window`), so the cover/draw never move in the buffer.
    ///
    /// Y — the block's anchor lands at the EXACT `t + ny*(b-t)` (free, like x), and the GROWTH
    /// direction comes from the half: the top half (`ny<0.5`) anchors its top edge there + grows DOWN,
    /// the bottom half anchors its bottom edge there + grows UP. So ny≈0 hugs the top, ny≈1 the bottom,
    /// ny≈0.5 sits mid-screen — never snapped to an edge. (The inline preset — nx≈0.5 / ny≈0 — is a
    /// top-centre strip, where game notices live.)
    /// Sized to the cursor's monitor work area.
    unsafe fn stack_origin(place: (f32, f32), cur: POINT) -> POINT {
        let (l, t, r, b) = monitor_work(cur);
        let pad = STACK_PAD as i32;
        let (nx, ny) = (place.0.clamp(0.0, 1.0), place.1);
        // x: centre the block on nx, then clamp the block fully inside [l+pad, r-pad]. `win_x` is the
        // window's left so that block centre (win_x + STACK_AX) hits the desired screen centre.
        let half = STACK_HALF.round() as i32;
        let want_centre = (l as f32 + nx * (r - l) as f32).round() as i32;
        let lo_centre = l + pad + half; // leftmost centre keeping the block on-screen
        let hi_centre = (r - pad - half).max(lo_centre); // rightmost (degenerate-safe)
        let centre = want_centre.clamp(lo_centre, hi_centre);
        let win_x = centre - STACK_AX.round() as i32;
        // y: honour the EXACT `ny` (like x honours nx) — anchor the block at `t + ny*(b-t)`, choosing
        // the GROWTH direction by half (top grows DOWN from its top anchor, bottom grows UP from its
        // bottom anchor). Clamped on-screen. (Was binary top/bottom-EDGE: every custom vertical spot
        // snapped to an edge — a centre placement landed at the bottom instead of centred.)
        let want_y = (t as f32 + ny.clamp(0.0, 1.0) * (b - t) as f32)
            .round()
            .clamp((t + pad) as f32, (b - pad) as f32) as i32;
        let win_y = if ny < 0.5 {
            want_y - STACK_AY_TOP.round() as i32
        } else {
            want_y - STACK_AY_BOT.round() as i32
        };
        POINT { x: win_x, y: win_y }
    }

    /// The radial wheel: N spokes + an outer arc; the aimed wedge `live` lights white-hot.
    // ── render-ready wedge / fan (rasterized once per Begin; redrawn per frame) ──
    struct WedgeR {
        glyph: WedgeGlyph,
        tone: Tone,
        meter: Option<f32>,
        title: Option<TextRaster>,
        value: Option<TextRaster>,
    }
    struct FanR {
        label: Option<TextRaster>,
        active: bool,
    }

    /// A render-ready notification card in the STACK: the rasterized text (title small, value LARGE,
    /// the body wrapped to up to 3 lines, the prev tiny) cached by `ident`+`rev`, plus a COPY of the
    /// slot's animation/draw data for this frame. The rasters survive across the engine's 60fps
    /// pushes (re-cut only when `rev` moves), so per-frame cost stays bounded — text is rasterized
    /// per content-change, never per frame.
    struct NotifyR {
        ident: String,
        rev: u64,
        glyph: WedgeGlyph,
        dir: i8,
        fill: f32,
        prev_fill: f32,
        panel: bool,
        /// the ASK slot: drawn in the ask layout (question + grammar row) instead of the
        /// value-hierarchy. See [`NotifySlot::is_ask`].
        is_ask: bool,
        title: Option<TextRaster>,
        value: Option<TextRaster>,
        /// the body, WRAPPED to ≤3 lines then ellipsized — the meatier macro line that breathes.
        /// EMPTY for an ask card (its layout is the fixed question+grammar rows).
        body: Vec<TextRaster>,
        prev: Option<TextRaster>,
        // this frame's eased animation (copied from the pushed `NotifySlot`).
        y: f32,
        alpha: f32,
        scale: f32,
        collapse: f32,
        bump: f32,
    }

    /// The rasterized DIGEST summary: the count, the latest line, cached by content; its glyph row +
    /// animation come straight off the pushed `DigestView` each frame.
    struct DigestR {
        count: u32,
        glyphs: Vec<WedgeGlyph>,
        count_txt: Option<TextRaster>,
        title: Option<TextRaster>,
        value: Option<TextRaster>,
        alpha: f32,
        reveal: f32,
    }

    // ── tiny vector primitives: every icon is line-art splatted into a glow channel ──
    /// Splat a soft line segment.
    fn vline(buf: &mut [f32], a: (f32, f32), b: (f32, f32), rad: f32, bright: f32) {
        let d = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        let steps = (d / 2.0).ceil().max(1.0) as i32;
        for s in 0..=steps {
            let u = s as f32 / steps as f32;
            splat(
                buf,
                a.0 + (b.0 - a.0) * u,
                a.1 + (b.1 - a.1) * u,
                rad,
                bright,
            );
        }
    }
    /// Splat a soft arc (angles in radians, standard math convention).
    #[allow(clippy::too_many_arguments)]
    fn varc(buf: &mut [f32], cx: f32, cy: f32, r: f32, a0: f32, a1: f32, rad: f32, bright: f32) {
        let steps = (((a1 - a0).abs() * r) / 2.2).ceil().max(2.0) as i32;
        for s in 0..=steps {
            let a = a0 + (a1 - a0) * (s as f32 / steps as f32);
            splat(buf, cx + a.cos() * r, cy + a.sin() * r, rad, bright);
        }
    }
    /// A small arrowhead at `tip` pointing along `dir`.
    fn arrowhead(buf: &mut [f32], tip: (f32, f32), dir: (f32, f32), size: f32, sr: f32, b: f32) {
        let len = (dir.0 * dir.0 + dir.1 * dir.1).sqrt().max(1e-3);
        let (ux, uy) = (dir.0 / len, dir.1 / len);
        let (px, py) = (-uy, ux);
        let back = (tip.0 - ux * size, tip.1 - uy * size);
        vline(
            buf,
            (back.0 + px * size * 0.6, back.1 + py * size * 0.6),
            tip,
            sr,
            b,
        );
        vline(
            buf,
            (back.0 - px * size * 0.6, back.1 - py * size * 0.6),
            tip,
            sr,
            b,
        );
    }
    /// A small horizontal meter bar (the volume fill under an icon), tinted by tone.
    fn meter_bar(
        buf: &mut Buffers,
        cx: f32,
        cy: f32,
        halfw: f32,
        fill: f32,
        tone: Tone,
        bright: f32,
    ) {
        let m: &mut [f32] = if tone == Tone::Off {
            &mut buf.warn
        } else {
            &mut buf.glow
        };
        vline(m, (cx - halfw, cy), (cx + halfw, cy), 1.3, 0.16 * bright);
        let fx = cx - halfw + 2.0 * halfw * fill.clamp(0.0, 1.0);
        vline(m, (cx - halfw, cy), (fx, cy), 2.0, 0.7 * bright);
        splat(m, fx, cy, 2.6, 0.95 * bright);
    }

    /// THE ICON LIBRARY — procedural vector art for each wedge archetype, centered at (cx,cy) with
    /// scale `r`. Tone routes the channel/colour (Off → red + a slash), Active adds a prism halo.
    #[allow(clippy::too_many_arguments)]
    fn draw_wedge_glyph(
        buf: &mut Buffers,
        g: WedgeGlyph,
        cx: f32,
        cy: f32,
        r: f32,
        tone: Tone,
        frame: u32,
        bright: f32,
    ) {
        if g == WedgeGlyph::Blank {
            return;
        }
        let off = tone == Tone::Off;
        let dim = if tone == Tone::Inert { 0.55 } else { 1.0 };
        let b = bright * dim;
        // a touch thicker than a hairline: a too-thin glow stroke shades as faint accent RIM only
        // (the dead-dots problem), so the icon reads as ghostly. More radius = more field density =
        // the stroke emerges as glowing glass body, legibly, while still made of the same mana.
        let sr = 2.1;
        let muted_wave = off; // speakers/mics drop their waves when muted
        {
            let m: &mut [f32] = if off { &mut buf.warn } else { &mut buf.glow };
            match g {
                WedgeGlyph::Speaker | WedgeGlyph::Flip => {
                    // magnet box + cone
                    vline(
                        m,
                        (cx - 0.75 * r, cy - 0.32 * r),
                        (cx - 0.4 * r, cy - 0.32 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx - 0.75 * r, cy + 0.32 * r),
                        (cx - 0.4 * r, cy + 0.32 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx - 0.75 * r, cy - 0.32 * r),
                        (cx - 0.75 * r, cy + 0.32 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx - 0.4 * r, cy - 0.32 * r),
                        (cx + 0.05 * r, cy - 0.62 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx - 0.4 * r, cy + 0.32 * r),
                        (cx + 0.05 * r, cy + 0.62 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx + 0.05 * r, cy - 0.62 * r),
                        (cx + 0.05 * r, cy + 0.62 * r),
                        sr,
                        b,
                    );
                    if !muted_wave && g == WedgeGlyph::Speaker {
                        varc(m, cx + 0.0 * r, cy, 0.45 * r, -0.7, 0.7, 1.5, 0.8 * b);
                        varc(m, cx + 0.0 * r, cy, 0.75 * r, -0.7, 0.7, 1.5, 0.6 * b);
                    }
                    if g == WedgeGlyph::Flip {
                        // two-way swap arrows under the cone
                        let yy = cy + 0.92 * r;
                        vline(m, (cx - 0.7 * r, yy), (cx + 0.7 * r, yy), sr, 0.8 * b);
                        arrowhead(m, (cx + 0.7 * r, yy), (1.0, 0.0), 0.32 * r, sr, 0.8 * b);
                        arrowhead(m, (cx - 0.7 * r, yy), (-1.0, 0.0), 0.32 * r, sr, 0.8 * b);
                    }
                }
                WedgeGlyph::Mic => {
                    // capsule
                    varc(m, cx, cy - 0.35 * r, 0.28 * r, TAU * 0.5, TAU, sr, b);
                    vline(
                        m,
                        (cx - 0.28 * r, cy - 0.35 * r),
                        (cx - 0.28 * r, cy + 0.05 * r),
                        sr,
                        b,
                    );
                    vline(
                        m,
                        (cx + 0.28 * r, cy - 0.35 * r),
                        (cx + 0.28 * r, cy + 0.05 * r),
                        sr,
                        b,
                    );
                    varc(m, cx, cy + 0.05 * r, 0.28 * r, 0.0, TAU * 0.5, sr, b);
                    // cradle + stem + base
                    varc(
                        m,
                        cx,
                        cy + 0.0 * r,
                        0.48 * r,
                        0.12 * TAU,
                        0.38 * TAU,
                        sr,
                        0.85 * b,
                    );
                    vline(m, (cx, cy + 0.48 * r), (cx, cy + 0.78 * r), sr, b);
                    vline(
                        m,
                        (cx - 0.3 * r, cy + 0.78 * r),
                        (cx + 0.3 * r, cy + 0.78 * r),
                        sr,
                        b,
                    );
                }
                WedgeGlyph::WindowStack
                | WedgeGlyph::Summon
                | WedgeGlyph::Banish
                | WedgeGlyph::Pin => {
                    // a window with a title bar
                    let (l, t, rr, bm) =
                        (cx - 0.55 * r, cy - 0.18 * r, cx + 0.45 * r, cy + 0.62 * r);
                    vline(m, (l, t), (rr, t), sr, b);
                    vline(m, (l, bm), (rr, bm), sr, b);
                    vline(m, (l, t), (l, bm), sr, b);
                    vline(m, (rr, t), (rr, bm), sr, b);
                    vline(m, (l, t + 0.16 * r), (rr, t + 0.16 * r), sr, 0.7 * b);
                    match g {
                        WedgeGlyph::WindowStack => {
                            // a second window peeking behind (the "stack")
                            vline(
                                m,
                                (cx - 0.2 * r, cy - 0.5 * r),
                                (cx + 0.7 * r, cy - 0.5 * r),
                                sr,
                                0.7 * b,
                            );
                            vline(
                                m,
                                (cx + 0.7 * r, cy - 0.5 * r),
                                (cx + 0.7 * r, cy + 0.3 * r),
                                sr,
                                0.7 * b,
                            );
                        }
                        WedgeGlyph::Summon => arrowhead(
                            m,
                            (cx - 0.05 * r, cy - 0.55 * r),
                            (0.0, -1.0),
                            0.3 * r,
                            sr,
                            b,
                        ),
                        WedgeGlyph::Banish => {
                            arrowhead(m, (cx - 0.05 * r, cy + 0.9 * r), (0.0, 1.0), 0.3 * r, sr, b)
                        }
                        WedgeGlyph::Pin => {
                            // a pin tack stuck into the title bar
                            splat(m, cx, cy - 0.42 * r, 0.16 * r, b);
                            vline(m, (cx, cy - 0.32 * r), (cx, cy - 0.05 * r), sr, b);
                        }
                        _ => {}
                    }
                }
                WedgeGlyph::Anchor => {
                    varc(m, cx, cy - 0.55 * r, 0.16 * r, 0.0, TAU, sr, b); // ring
                    vline(m, (cx, cy - 0.4 * r), (cx, cy + 0.55 * r), sr, b); // shaft
                    vline(
                        m,
                        (cx - 0.32 * r, cy - 0.22 * r),
                        (cx + 0.32 * r, cy - 0.22 * r),
                        sr,
                        b,
                    ); // crossbar
                    varc(m, cx, cy + 0.1 * r, 0.5 * r, 0.1 * TAU, 0.4 * TAU, sr, b); // flukes curve
                    arrowhead(
                        m,
                        (cx - 0.47 * r, cy + 0.38 * r),
                        (-0.6, 0.8),
                        0.22 * r,
                        sr,
                        b,
                    );
                    arrowhead(
                        m,
                        (cx + 0.47 * r, cy + 0.38 * r),
                        (0.6, 0.8),
                        0.22 * r,
                        sr,
                        b,
                    );
                }
                WedgeGlyph::ProfileDot => {
                    splat(m, cx, cy, 0.42 * r, b);
                    varc(m, cx, cy, 0.72 * r, 0.0, TAU, sr, 0.7 * b);
                }
                WedgeGlyph::Key => {
                    let q = 0.6 * r;
                    vline(m, (cx - q, cy - q), (cx + q, cy - q), sr, b);
                    vline(m, (cx - q, cy + q), (cx + q, cy + q), sr, b);
                    vline(m, (cx - q, cy - q), (cx - q, cy + q), sr, b);
                    vline(m, (cx + q, cy - q), (cx + q, cy + q), sr, b);
                    splat(m, cx, cy, 0.16 * r, 0.8 * b);
                }
                WedgeGlyph::Media => {
                    vline(m, (cx - 0.4 * r, cy - 0.5 * r), (cx + 0.55 * r, cy), sr, b);
                    vline(m, (cx + 0.55 * r, cy), (cx - 0.4 * r, cy + 0.5 * r), sr, b);
                    vline(
                        m,
                        (cx - 0.4 * r, cy - 0.5 * r),
                        (cx - 0.4 * r, cy + 0.5 * r),
                        sr,
                        b,
                    );
                }
                WedgeGlyph::Terminal => {
                    let (l, t, rr, bm) = (cx - 0.65 * r, cy - 0.5 * r, cx + 0.65 * r, cy + 0.5 * r);
                    vline(m, (l, t), (rr, t), sr, 0.8 * b);
                    vline(m, (l, bm), (rr, bm), sr, 0.8 * b);
                    vline(m, (l, t), (l, bm), sr, 0.8 * b);
                    vline(m, (rr, t), (rr, bm), sr, 0.8 * b);
                    // a ">" prompt
                    vline(m, (cx - 0.35 * r, cy - 0.18 * r), (cx - 0.1 * r, cy), sr, b);
                    vline(m, (cx - 0.1 * r, cy), (cx - 0.35 * r, cy + 0.18 * r), sr, b);
                    vline(
                        m,
                        (cx + 0.05 * r, cy + 0.2 * r),
                        (cx + 0.35 * r, cy + 0.2 * r),
                        sr,
                        b,
                    );
                }
                WedgeGlyph::Python => {
                    // a coiled S (snake)
                    varc(m, cx, cy - 0.28 * r, 0.3 * r, TAU * 0.5, TAU, sr, b);
                    varc(m, cx, cy + 0.28 * r, 0.3 * r, 0.0, TAU * 0.5, sr, b);
                    splat(m, cx + 0.3 * r, cy - 0.28 * r, 0.1 * r, b); // head
                }
                WedgeGlyph::Teleport => {
                    varc(m, cx, cy, 0.55 * r, 0.0, TAU, sr, 0.8 * b); // portal ring
                    vline(m, (cx - 0.8 * r, cy), (cx + 0.7 * r, cy), sr, b);
                    arrowhead(m, (cx + 0.7 * r, cy), (1.0, 0.0), 0.3 * r, sr, b);
                }
                WedgeGlyph::Whiteboard => {
                    let (l, t, rr, bm) = (cx - 0.6 * r, cy - 0.5 * r, cx + 0.6 * r, cy + 0.45 * r);
                    vline(m, (l, t), (rr, t), sr, 0.7 * b);
                    vline(m, (l, bm), (rr, bm), sr, 0.7 * b);
                    vline(m, (l, t), (l, bm), sr, 0.7 * b);
                    vline(m, (rr, t), (rr, bm), sr, 0.7 * b);
                    vline(
                        m,
                        (cx - 0.3 * r, cy + 0.2 * r),
                        (cx + 0.35 * r, cy - 0.25 * r),
                        sr,
                        b,
                    ); // ink stroke
                }
                WedgeGlyph::Knockback => {
                    let pulse = 0.85 + 0.15 * ((frame as f32) * 0.12).sin();
                    varc(m, cx, cy, 0.3 * r * pulse, 0.0, TAU, sr, b);
                    varc(m, cx, cy, 0.58 * r * pulse, 0.0, TAU, sr, 0.7 * b);
                    varc(m, cx, cy, 0.86 * r * pulse, 0.0, TAU, sr, 0.4 * b);
                }
                WedgeGlyph::Target => {
                    // a real RETICLE — an outer ring, four gapped crosshair ticks reaching the rim
                    // (with a clear gap around the centre), and a bright centre dot (the precise aim).
                    varc(m, cx, cy, 0.62 * r, 0.0, TAU, sr, 0.85 * b);
                    let (g_in, g_out) = (0.30 * r, 0.92 * r); // the gap and the outer reach
                    vline(m, (cx, cy - g_in), (cx, cy - g_out), sr, 0.8 * b); // N
                    vline(m, (cx, cy + g_in), (cx, cy + g_out), sr, 0.8 * b); // S
                    vline(m, (cx - g_in, cy), (cx - g_out, cy), sr, 0.8 * b); // W
                    vline(m, (cx + g_in, cy), (cx + g_out, cy), sr, 0.8 * b); // E
                    splat(m, cx, cy, 0.15 * r, b); // the centre dot
                }
                WedgeGlyph::Scroll => {
                    // a clean two-way scroll cue: a short central rail capped by an up + down arrowhead.
                    arrowhead(m, (cx, cy - 0.62 * r), (0.0, -1.0), 0.34 * r, sr, b);
                    arrowhead(m, (cx, cy + 0.62 * r), (0.0, 1.0), 0.34 * r, sr, b);
                    vline(m, (cx, cy - 0.34 * r), (cx, cy + 0.34 * r), sr, 0.6 * b);
                }
                WedgeGlyph::Ghost => {
                    // a clipboard with a scalloped (ghostly) hem
                    let (l, t, rr) = (cx - 0.5 * r, cy - 0.55 * r, cx + 0.5 * r);
                    vline(m, (l, t), (rr, t), sr, b);
                    vline(m, (l, t), (l, cy + 0.35 * r), sr, b);
                    vline(m, (rr, t), (rr, cy + 0.35 * r), sr, b);
                    varc(
                        m,
                        cx - 0.25 * r,
                        cy + 0.35 * r,
                        0.25 * r,
                        0.0,
                        TAU * 0.5,
                        sr,
                        b,
                    );
                    varc(
                        m,
                        cx + 0.25 * r,
                        cy + 0.35 * r,
                        0.25 * r,
                        0.0,
                        TAU * 0.5,
                        sr,
                        b,
                    );
                    splat(m, cx, cy - 0.55 * r, 0.12 * r, b); // clip
                }
                WedgeGlyph::Network => {
                    // wifi: nested arcs over a dot (the universal signal fan). Drops to a dim base
                    // dot + a slash via the `off` tone when offline.
                    splat(m, cx, cy + 0.55 * r, 0.14 * r, b);
                    varc(
                        m,
                        cx,
                        cy + 0.55 * r,
                        0.4 * r,
                        TAU * 0.62,
                        TAU * 0.88,
                        sr,
                        0.9 * b,
                    );
                    varc(
                        m,
                        cx,
                        cy + 0.55 * r,
                        0.68 * r,
                        TAU * 0.62,
                        TAU * 0.88,
                        sr,
                        0.7 * b,
                    );
                    varc(
                        m,
                        cx,
                        cy + 0.55 * r,
                        0.96 * r,
                        TAU * 0.62,
                        TAU * 0.88,
                        sr,
                        0.5 * b,
                    );
                }
                WedgeGlyph::Ethernet => {
                    // an RJ45 plug: a body with a little latch + two contact pins (the wired link).
                    let (l, t, rr, bm) = (cx - 0.45 * r, cy - 0.4 * r, cx + 0.45 * r, cy + 0.4 * r);
                    vline(m, (l, t), (rr, t), sr, b);
                    vline(m, (l, bm), (rr, bm), sr, b);
                    vline(m, (l, t), (l, bm), sr, b);
                    vline(m, (rr, t), (rr, bm), sr, b);
                    vline(
                        m,
                        (cx - 0.15 * r, cy + 0.4 * r),
                        (cx - 0.15 * r, cy + 0.62 * r),
                        sr,
                        0.8 * b,
                    ); // latch
                    vline(
                        m,
                        (cx + 0.15 * r, cy + 0.4 * r),
                        (cx + 0.15 * r, cy + 0.62 * r),
                        sr,
                        0.8 * b,
                    );
                    vline(
                        m,
                        (cx - 0.2 * r, cy - 0.4 * r),
                        (cx - 0.2 * r, cy - 0.62 * r),
                        sr,
                        0.8 * b,
                    ); // pins
                    vline(
                        m,
                        (cx + 0.2 * r, cy - 0.4 * r),
                        (cx + 0.2 * r, cy - 0.62 * r),
                        sr,
                        0.8 * b,
                    );
                }
                WedgeGlyph::Screen => {
                    // a monitor on a stand with a power symbol on the panel — "sleep the displays".
                    let (l, t, rr, bm) =
                        (cx - 0.72 * r, cy - 0.52 * r, cx + 0.72 * r, cy + 0.30 * r);
                    vline(m, (l, t), (rr, t), sr, 0.8 * b);
                    vline(m, (l, bm), (rr, bm), sr, 0.8 * b);
                    vline(m, (l, t), (l, bm), sr, 0.8 * b);
                    vline(m, (rr, t), (rr, bm), sr, 0.8 * b);
                    // stand: neck + base
                    vline(m, (cx, bm), (cx, bm + 0.22 * r), sr, 0.7 * b);
                    vline(
                        m,
                        (cx - 0.3 * r, bm + 0.22 * r),
                        (cx + 0.3 * r, bm + 0.22 * r),
                        sr,
                        0.7 * b,
                    );
                    // power symbol: a ring with a gap at top (TAU*0.75 = top, y-down) + a stem through it
                    let (pcy, pr) = (cy - 0.11 * r, 0.18 * r);
                    varc(m, cx, pcy, pr, TAU * 0.81, TAU * 1.69, sr, b);
                    vline(m, (cx, pcy - 0.04 * r), (cx, pcy - pr - 0.06 * r), sr, b);
                }
                WedgeGlyph::Curtain => {
                    // a draped curtain — a rod, vertical folds, and a scalloped hem: "hide the screen".
                    let rodw = 0.72 * r;
                    let (rod_y, hem_y) = (cy - 0.60 * r, cy + 0.40 * r);
                    vline(m, (cx - rodw, rod_y), (cx + rodw, rod_y), sr, 0.8 * b); // the rod
                    let folds = [-0.72_f32, -0.36, 0.0, 0.36, 0.72];
                    for fx in folds {
                        vline(m, (cx + fx * r, rod_y), (cx + fx * r, hem_y), sr, b); // a fold
                    }
                    // scalloped hem: a downward half-circle bump between each pair of folds (y-down, so
                    // angles 0..PI bulge below the hem line).
                    let mut i = 0;
                    while i + 1 < folds.len() {
                        let midx = cx + (folds[i] + folds[i + 1]) * 0.5 * r;
                        let rr = ((folds[i + 1] - folds[i]) * 0.5 * r).abs();
                        varc(m, midx, hem_y, rr, 0.0, std::f32::consts::PI, sr, 0.85 * b);
                        i += 1;
                    }
                }
                WedgeGlyph::Bluetooth => {
                    // the bluetooth rune: a vertical spine with two crossed bowties (the bind-rune).
                    let (tp, bp) = (cy - 0.7 * r, cy + 0.7 * r);
                    let mx = cx + 0.3 * r;
                    vline(m, (cx, tp), (cx, bp), sr, b); // spine
                    vline(m, (cx, tp), (mx, cy - 0.35 * r), sr, b); // upper right diagonal
                    vline(m, (mx, cy - 0.35 * r), (cx - 0.3 * r, cy + 0.35 * r), sr, b); // cross down-left
                    vline(m, (cx, bp), (mx, cy + 0.35 * r), sr, b); // lower right diagonal
                    vline(m, (mx, cy + 0.35 * r), (cx - 0.3 * r, cy - 0.35 * r), sr, b);
                    // cross up-left
                }
                WedgeGlyph::Pulse => {
                    // a frequency mark — three rising bars (a tiny ▮▮▮ spectrum): the report rate.
                    // Bars stand on a shared baseline, each taller than the last (low → high freq).
                    let base = cy + 0.6 * r;
                    let bars = [(-0.55_f32, 0.4_f32), (0.0, 0.75), (0.55, 1.15)];
                    for (bx, hh) in bars {
                        let x = cx + bx * r;
                        vline(m, (x, base), (x, base - hh * r), sr, b);
                    }
                    // a faint baseline tying them together
                    vline(m, (cx - 0.78 * r, base), (cx + 0.78 * r, base), sr, 0.45 * b);
                }
                WedgeGlyph::Sun => {
                    // a sun — a filled disc with eight radiating rays: the brightness / light cue.
                    splat(m, cx, cy, 0.34 * r, b); // the disc body
                    varc(m, cx, cy, 0.36 * r, 0.0, TAU, sr, 0.8 * b); // its rim
                    for k in 0..8 {
                        let a = (k as f32 / 8.0) * TAU;
                        let (ux, uy) = (a.cos(), a.sin());
                        vline(
                            m,
                            (cx + ux * 0.55 * r, cy + uy * 0.55 * r),
                            (cx + ux * 0.92 * r, cy + uy * 0.92 * r),
                            sr,
                            0.85 * b,
                        );
                    }
                }
                WedgeGlyph::Battery => {
                    // a battery cell — a rounded body, the positive terminal nub on the right, and two
                    // charge bars inside (the fill). Pure strokes, like the rest of the icon set.
                    let (l, rt, tp, bt) = (cx - 0.7 * r, cx + 0.5 * r, cy - 0.42 * r, cy + 0.42 * r);
                    vline(m, (l, tp), (rt, tp), sr, b); // top
                    vline(m, (l, bt), (rt, bt), sr, b); // bottom
                    vline(m, (l, tp), (l, bt), sr, b); // left
                    vline(m, (rt, tp), (rt, bt), sr, b); // right
                    // terminal nub
                    vline(m, (rt, cy - 0.18 * r), (cx + 0.66 * r, cy - 0.18 * r), sr, b);
                    vline(m, (rt, cy + 0.18 * r), (cx + 0.66 * r, cy + 0.18 * r), sr, b);
                    vline(m, (cx + 0.66 * r, cy - 0.18 * r), (cx + 0.66 * r, cy + 0.18 * r), sr, b);
                    // two charge bars inside
                    vline(m, (cx - 0.40 * r, tp + 0.16 * r), (cx - 0.40 * r, bt - 0.16 * r), sr, 0.85 * b);
                    vline(m, (cx - 0.10 * r, tp + 0.16 * r), (cx - 0.10 * r, bt - 0.16 * r), sr, 0.85 * b);
                }
                WedgeGlyph::Ask => {
                    // a question mark — a curling hook over a baseline dot. The head is a ~3/4 arc
                    // opening down-left, dropping into a short vertical stem; a separate dot sits
                    // below (the classic '?' silhouette), all in clean procedural strokes.
                    let hcy = cy - 0.28 * r; // the hook's centre, sat high
                    let hr = 0.34 * r; // its radius
                    // the curl: from the lower-left, up and over the top, down the right side
                    // (angles y-down: PI = west, -PI/2 = north, 0 = east, ~0.35PI = lower-right).
                    varc(
                        m,
                        cx,
                        hcy,
                        hr,
                        std::f32::consts::PI * 0.92,
                        -std::f32::consts::PI * 0.30,
                        sr,
                        b,
                    );
                    // the stem dropping from the hook's lower-right tail to the body's mid-line.
                    let tail_x = cx + hr * (std::f32::consts::PI * 0.30).cos();
                    let tail_y = hcy + hr * (-std::f32::consts::PI * 0.30).sin();
                    vline(m, (tail_x, tail_y), (cx, cy + 0.20 * r), sr, b);
                    // the baseline dot.
                    splat(m, cx, cy + 0.56 * r, 0.13 * r, b);
                }
                WedgeGlyph::SidePlate => {
                    // a swappable side PLATE — a rounded chip frame with a 2×2 grid of button pips
                    // (the side buttons the plate carries). A "hardware module" read, distinct from the
                    // battery cell (no terminal nub) and the single profile dot (a grid, not one mark).
                    let (l, rt, tp, bt) = (cx - 0.62 * r, cx + 0.62 * r, cy - 0.72 * r, cy + 0.72 * r);
                    vline(m, (l, tp), (rt, tp), sr, b); // top
                    vline(m, (l, bt), (rt, bt), sr, b); // bottom
                    vline(m, (l, tp), (l, bt), sr, b); // left
                    vline(m, (rt, tp), (rt, bt), sr, b); // right
                    for gx in [-0.28_f32, 0.28] {
                        for gy in [-0.34_f32, 0.34] {
                            splat(m, cx + gx * r, cy + gy * r, 0.12 * r, 0.9 * b);
                        }
                    }
                }
                _ => {
                    // generic hollow diamond (Mark + any unhandled) — never blank
                    vline(m, (cx, cy - 0.6 * r), (cx + 0.6 * r, cy), sr, b);
                    vline(m, (cx + 0.6 * r, cy), (cx, cy + 0.6 * r), sr, b);
                    vline(m, (cx, cy + 0.6 * r), (cx - 0.6 * r, cy), sr, b);
                    vline(m, (cx - 0.6 * r, cy), (cx, cy - 0.6 * r), sr, b);
                }
            }
            if off {
                // the "off" slash through the glyph
                vline(
                    m,
                    (cx - 0.85 * r, cy - 0.85 * r),
                    (cx + 0.85 * r, cy + 0.85 * r),
                    sr + 0.4,
                    b,
                );
            }
        }
        if tone == Tone::Active {
            let sh = frame as f32 * 0.04;
            splat_prism(
                buf,
                cx,
                cy,
                r * 1.5,
                0.16 * bright,
                hsv(sh * 40.0, 0.5, 1.0),
            );
        }
    }

    /// Draw one wedge as an icon → value → title stack. The aimed wedge (`hot`) BLOOMS — bigger
    /// icon, white-hot value, the title fades in; idle wedges are a quiet icon + small value.
    fn draw_wedge_card(
        buf: &mut Buffers,
        w: &WedgeR,
        cx: f32,
        cy: f32,
        hot: bool,
        frame: u32,
        flare: f32,
    ) {
        if w.glyph == WedgeGlyph::Blank && w.value.is_none() {
            return;
        }
        let breathe = 0.85 + 0.15 * ((frame as f32) * 0.1).sin();
        let r = if hot { 18.0 } else { 12.0 };
        let bright = if hot {
            (1.05 + flare * 0.4) * breathe
        } else {
            0.5
        };
        let icon_y = cy - if hot { 12.0 } else { 7.0 };
        // LEGIBILITY: pour a soft pocket of dusk under the whole card so the icon + its readout
        // never dissolve into a busy wheel. Deeper under the hot card (it carries the live value).
        let pit = if hot { 0.34 } else { 0.20 };
        if w.glyph != WedgeGlyph::Blank {
            shade_pocket(buf, cx, icon_y, r * 1.15, r * 1.15, pit);
        }
        draw_wedge_glyph(buf, w.glyph, cx, icon_y, r, w.tone, frame, bright);
        if let Some(mv) = w.meter {
            meter_bar(
                buf,
                cx,
                icon_y + r * 0.95 + 3.0,
                r * 0.95,
                mv,
                w.tone,
                bright,
            );
        }
        if let Some(t) = &w.value {
            let vy = cy + if hot { 14.0 } else { 9.0 };
            let g = if hot { 1.2 } else { 0.6 };
            text_pocket(buf, t, cx, vy, pit);
            let ch: &mut [f32] = if w.tone == Tone::Off {
                &mut buf.warn
            } else {
                &mut buf.white
            };
            blit_mask(ch, t, cx, vy, g);
        }
        if hot {
            if let Some(t) = &w.title {
                let ty = cy + 33.0;
                text_pocket(buf, t, cx, ty, 0.30);
                // the title lives in the glow FIELD (so it's mana, not flat UI), but thin glow
                // strokes read as faint accent rim — a whisper of white core gives them weight so
                // the word stays solid without going to a hard-edged label.
                blit_mask(&mut buf.glow, t, cx, ty, 0.62);
                blit_mask(&mut buf.white, t, cx, ty, 0.20);
            }
        }
    }

    /// Draw a normalized exemplar polyline (centered unit box) scaled to `scale` px and centered at
    /// (cx,cy) — the prediction's SHAPE-GHOST. Drawn faint into the prism for a spectral whisper.
    fn draw_ghost_path(
        buf: &mut Buffers,
        ghost: &[[f32; 2]],
        cx: f32,
        cy: f32,
        scale: f32,
        bright: f32,
        frame: u32,
    ) {
        if ghost.len() < 2 {
            return;
        }
        let mut prev: Option<(f32, f32)> = None;
        for (i, p) in ghost.iter().enumerate() {
            let x = cx + p[0] * scale;
            let y = cy + p[1] * scale;
            if let Some((px, py)) = prev {
                let hue = i as f32 * 6.0 + frame as f32 * 1.5;
                let d = ((x - px).powi(2) + (y - py).powi(2)).sqrt();
                let steps = (d / 3.0).ceil().max(1.0) as i32;
                for s in 0..=steps {
                    let u = s as f32 / steps as f32;
                    splat_glow(
                        &mut buf.glow,
                        px + (x - px) * u,
                        py + (y - py) * u,
                        2.4,
                        bright,
                    );
                    splat_prism(
                        buf,
                        px + (x - px) * u,
                        py + (y - py) * u,
                        2.0,
                        bright * 0.5,
                        hsv(hue, 0.7, 1.0),
                    );
                }
            }
            prev = Some((x, y));
        }
    }

    fn sector_wheel(glow: &mut [f32], white: &mut [f32], sectors: u8, live: i32, flare: f32) {
        let n = sectors.max(1) as i32;
        let rad = (CX - 26.0).min(150.0);
        // outer ring — DENSE enough that the material body fills a continuous glowing arc (sparse
        // samples read as dead green dots: only the accent rim shows on a thin, low-density mark;
        // overlapping splats build the density the material needs to emerge as luminous glass).
        let ring_n = (rad * 2.6) as i32; // ~one splat per ~2.4px around the circumference
        for k in 0..ring_n {
            let a = (k as f32 / ring_n as f32) * TAU;
            splat(glow, CX + a.cos() * rad, CY + a.sin() * rad, 3.0, 0.40);
        }
        for s in 0..n {
            // North = up = -y; wedge s is CENTERED on bearing s/n (round semantics — matches
            // `sector_of` and the engine's `sector_for`, so the lit spoke = the firing wedge).
            let ca = s as f32 / n as f32 * TAU;
            let dir = (ca.sin(), -ca.cos()); // (dx,dy) on screen
            let hot = s == live;
            // spoke to the wedge centre
            let steps = 22;
            for j in 0..=steps {
                let u = j as f32 / steps as f32;
                let x = CX + dir.0 * rad * u;
                let y = CY + dir.1 * rad * u;
                if hot {
                    splat(white, x, y, 3.2, 0.8 * (0.5 + u));
                    splat(glow, x, y, 6.0, 0.7 * (0.4 + u));
                } else {
                    splat(glow, x, y, 2.2, 0.16);
                }
            }
            // the wedge node at the rim
            let nx = CX + dir.0 * rad;
            let ny = CY + dir.1 * rad;
            if hot {
                splat(white, nx, ny, 6.5, 0.9 + flare);
                splat(glow, nx, ny, 13.0, 0.9);
            } else {
                splat(glow, nx, ny, 5.5, 0.34);
                splat(white, nx, ny, 2.0, 0.10);
            }
        }
    }

    /// The SECOND TIER: fan `m` sub-options out past the rim of wedge `wedge`, in an arc WIDER
    /// than the slice (so N options breathe), each a node + label; the hot one blazes. Generic
    /// over N — the device fan-out, and anything else that wants a sub-menu, rides this.
    fn fan_arc(buf: &mut Buffers, opts: &[FanR], wedge: i32, sectors: u8, hot: i32, rim: f32) {
        let m = opts.len();
        if m == 0 {
            return;
        }
        let center = wedge as f32 / sectors.max(1) as f32 * TAU; // the parent wedge's bearing
                                                                 // the fan spans wider than one slice: ~1.3 slices per option, capped to a comfortable arc
        let slice = TAU / sectors.max(1) as f32;
        let span = (slice * 1.3 * m as f32).min(TAU * 0.7);
        let fan_r = rim + 46.0; // it lives OUTSIDE the wheel
                                // a faint connector from the rim out to the fan, so the eye follows the spring
        for k in 0..14 {
            let u = k as f32 / 14.0;
            let r = rim + u * 46.0;
            splat_glow(
                &mut buf.glow,
                CX + center.sin() * r,
                CY - center.cos() * r,
                2.0,
                0.20,
            );
        }
        for (i, o) in opts.iter().enumerate() {
            // spread options symmetrically around the parent bearing
            let frac = if m == 1 {
                0.5
            } else {
                i as f32 / (m as f32 - 1.0)
            };
            let a = center + (frac - 0.5) * span;
            let nx = CX + a.sin() * fan_r;
            let ny = CY - a.cos() * fan_r;
            let lit = hot == i as i32;
            if lit {
                splat_white(&mut buf.white, nx, ny, 6.0, 0.95);
                splat_glow(&mut buf.glow, nx, ny, 14.0, 0.9);
            } else {
                splat_glow(&mut buf.glow, nx, ny, 6.0, 0.40);
                splat_white(&mut buf.white, nx, ny, 2.0, 0.12);
            }
            // the CURRENT option wears a prism ring — you see which device is live now
            if o.active {
                let sh = i as f32 * 40.0;
                varc(&mut buf.glow, nx, ny, 9.0, 0.0, TAU, 1.4, 0.5);
                splat_prism(buf, nx, ny, 7.0, 0.35, hsv(sh, 0.55, 1.0));
            }
            if let Some(t) = &o.label {
                // labels ride just past their node, along the bearing
                let lx = CX + a.sin() * (fan_r + 22.0);
                let ly = CY - a.cos() * (fan_r + 22.0);
                text_pocket(buf, t, lx, ly, if lit { 0.32 } else { 0.22 });
                blit_mask(&mut buf.white, t, lx, ly, if lit { 1.2 } else { 0.6 });
            }
        }
    }

    /// The DIAL gauge: a vertical meter, ticks, a filled column to `fill` (0..1), with the rim
    /// PULSING by `glow` (turn speed). The knob reads as analog — you can feel coarse vs fine.
    /// THE KNOB — a 270° analog arc gauge the stroke turns. A faint full rail with tick spokes; a
    /// phosphor arc that CHARGES from the lower-left round to the value; a floating needle; and a
    /// white-hot bead that streaks a comet trail and throws a prism shimmer the faster you spin it.
    /// The reading sits in the hub. `glow` is turn-speed: coarse vs fine reads as heat at a glance.
    fn dial_gauge(buf: &mut Buffers, fill: f32, glow: f32, frame: u32, muted: bool) {
        let fill = fill.clamp(0.0, 1.0);
        let glow = glow.clamp(0.0, 1.0);
        let r = 150.0_f32; // arc radius
        let f = frame as f32;
        let breathe = 0.85 + 0.15 * (f * 0.08).sin();
        let span = 0.75 * TAU; // 270° sweep, gap centered on the bottom
        let start = 0.625 * TAU; // begin at 225° clockwise-from-north = lower-left
                                 // a point on the arc for parameter t in 0..1 (clockwise-from-north, like the ask wheel)
        let pos = |t: f32| {
            let phi = start + t * span;
            (CX + phi.sin() * r, CY - phi.cos() * r)
        };
        let segs = 150;
        // the track: a faint full-sweep rail
        for k in 0..=segs {
            let (x, y) = pos(k as f32 / segs as f32);
            splat_glow(&mut buf.glow, x, y, 2.4, 0.12);
        }
        // tick spokes every 10%, majors at 0 / 50 / 100 (pointing inward from the rail)
        for k in 0..=10 {
            let t = k as f32 / 10.0;
            let phi = start + t * span;
            let (sx, sy) = (phi.sin(), -phi.cos());
            let major = k % 5 == 0;
            let len = if major { 15.0 } else { 8.0 };
            let steps = if major { 8 } else { 5 };
            for s in 0..steps {
                let rr = (r - len) + (s as f32 / steps as f32) * len;
                splat_glow(
                    &mut buf.glow,
                    CX + sx * rr,
                    CY + sy * rr,
                    1.6,
                    if major { 0.32 } else { 0.20 },
                );
            }
        }
        // the CHARGE: a bright phosphor arc swept from 0 to the value, hotter toward the head
        let fsegs = (segs as f32 * fill).round() as i32;
        for k in 0..=fsegs.max(0) {
            let t = k as f32 / segs as f32;
            let (x, y) = pos(t);
            let b = 0.40 + 0.50 * (t / fill.max(0.001)).min(1.0);
            splat_glow(&mut buf.glow, x, y, 4.2, b * (0.8 + 0.2 * breathe));
        }
        let head_phi = start + fill * span;
        let (psx, psy) = (head_phi.sin(), -head_phi.cos());
        let (hx, hy) = pos(fill);
        let hub = 64.0;
        // comet trail: a fast spin streaks the bead backward along the arc (motion blur)
        let trail = (glow * 16.0) as i32;
        for s in 1..=trail {
            let tt = (fill - s as f32 / segs as f32).clamp(0.0, 1.0);
            let (x, y) = pos(tt);
            let u = 1.0 - s as f32 / (trail.max(1) as f32);
            splat_white(&mut buf.white, x, y, 1.0 + 3.0 * u, 0.5 * u * glow);
        }
        // the floating needle: from the hub out to the bead (clears the centered reading)
        let nsteps = 22;
        for j in 0..=nsteps {
            let u = j as f32 / nsteps as f32;
            let rr = hub + u * (r - 12.0 - hub);
            splat_glow(
                &mut buf.glow,
                CX + psx * rr,
                CY + psy * rr,
                3.0,
                0.35 + 0.5 * u,
            );
            splat_white(&mut buf.white, CX + psx * rr, CY + psy * rr, 1.4, 0.4 * u);
        }
        // the head bead: white-hot, halo scaled by turn-speed
        splat_glow(
            &mut buf.glow,
            hx,
            hy,
            14.0 + glow * 16.0,
            (0.75 + glow * 0.5) * breathe,
        );
        splat_white(&mut buf.white, hx, hy, 6.0 + glow * 3.0, 0.95);
        // prism shimmer on the bead when spinning fast — the brand's chromatic fringe
        if glow > 0.02 {
            let sh = f * 0.3;
            splat_prism(
                buf,
                hx + sh.cos() * 2.0,
                hy + sh.sin() * 2.0,
                9.0,
                glow * 0.6,
                (1.0, 0.4, 0.7),
            );
            splat_prism(
                buf,
                hx - sh.cos() * 2.0,
                hy - sh.sin() * 2.0,
                9.0,
                glow * 0.6,
                (0.4, 0.7, 1.0),
            );
        }
        // the hub: a pulsing ring cradling the reading, with a soft core. MUTED → the ring goes
        // red and breathes harder, so a dead endpoint reads at a glance even mid-turn.
        let hubn = 60;
        for k in 0..hubn {
            let a = (k as f32 / hubn as f32) * TAU + f * 0.02;
            let (hx2, hy2) = (CX + a.cos() * hub, CY + a.sin() * hub);
            if muted {
                splat(&mut buf.warn, hx2, hy2, 2.4, 0.34 * breathe);
            } else {
                splat_glow(&mut buf.glow, hx2, hy2, 2.0, 0.16 * breathe);
            }
        }
        splat_glow(&mut buf.glow, CX, CY, 24.0, 0.09 * breathe);
    }

    fn dib_header() -> BITMAPINFO {
        let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: W,
            biHeight: -H,
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
}
