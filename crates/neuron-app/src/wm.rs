//! WINDOW QUICK-ACTIONS — bindable primitives that move, hide, raise and remember windows.
//!
//! These are the prebuilt verbs the spine offers for living in a desk full of windows: summon a
//! window to your hand, banish what's in your way, pin a reference on top, tether a warpstone you
//! snap back to. All host-side Win32 (no overlay surface), so they fire from any trigger — key,
//! wedge, glyph, app rule — the same as every other [`Action`].
//!
//! Honesty: focus changes go through teleport's `force_foreground` (the topmost-flip + input
//! handoff Windows requires; the last-resort synthesis is arm-gated). Minimise/topmost are pure
//! window-manager calls that work cross-process without focus rights. Nothing here writes to a
//! device.
//!
//! ## Shape — portable verbs over a compile-checked per-OS primitive SEAM
//! The whole file used to be `#![cfg(windows)]`, which welded the verbs (and their safety gates) to
//! Windows and stopped their callers compiling off-Windows. The verbs, the enums, the summon CYCLE
//! map, the tether `stones`, every pick-resolution and PREVIEW path, and — load-bearing — every
//! SAFETY GATE (kill's safe-mode suppression, self-guard, and critical-process denylist; banish's
//! fallbacks; the pin/tether preview honesty) now live in this PORTABLE layer, byte-for-byte
//! unchanged, and call a small set of OS window PRIMITIVES (`focused`, `under_cursor`, `minimize`,
//! `terminate`, …) through the [`WmOs`] trait. Two backends implement it: [`imp::Win`] is the real
//! Win32, [`stub::Null`] is an honest no-op (`None`/empty/`Err`). The verbs reach the live one via
//! [`os()`] — `imp::Win` on Windows, `stub::Null` elsewhere — both zero-sized, so `impl WmOs` is a
//! zero-cost dispatch. The window handle stays an opaque `isize`: the portable logic never inspects
//! it, only routes it back through the trait.
//!
//! Why a TRAIT and not two free-fn modules + a `use … as os` alias: the stub's no-op bodies need no
//! platform APIs, so it is **always compiled** (NOT cfg-gated). On Windows the compiler therefore
//! type-checks `impl WmOs for Null` against the SAME trait the verbs call — any drift between what a
//! verb needs and what a backend provides breaks the build on EVERY target, instead of silently
//! passing on Windows and only surfacing the first time someone builds off-Windows. The seam is now
//! parity-enforced by the type system, not by hand.
//!
//! Scope honesty: the verbs are seam-ready and the stub is compile-checked on every build, but the
//! APP is still Windows-only — the verbs' Win32 backend leans on sibling modules (`glance.rs`,
//! `teleport.rs`) that are themselves `#[cfg(windows)]` and not yet seam-extracted. Off-Windows the
//! verbs all run and honestly report "no window"; a full cross-platform app awaits those siblings.

use neuron::action::{SummonMode, WindowPick};

/// The OS window PRIMITIVES every verb routes through — the parity-enforced seam. Both backends
/// ([`imp::Win`] on Windows, [`stub::Null`] always) implement this against the SAME signatures the
/// verbs call, so the compiler catches any drift on every target (see the module doc). Methods take
/// `&self`; the implementors are zero-sized, so the receiver is free.
trait WmOs {
    /// Every window matching `needle` as `(hwnd, title)`, frontmost first (the summon CYCLE source).
    fn matches(&self, needle: &str) -> Vec<(isize, String)>;
    /// `ShellExecuteW("open", name)` — the Win+R resolve; returns the legacy "HINSTANCE > 32" bool.
    fn shell_open(&self, name: &str) -> bool;
    /// The live cursor position in screen pixels.
    fn cursor_pos(&self) -> (i32, i32);
    /// Bring `hwnd` to the foreground (teleport's topmost-flip + arm-gated input handoff).
    fn focus(&self, hwnd: isize);
    /// Pull `hwnd` to a real screen point (the summon-here move).
    fn summon_to(&self, hwnd: isize, to: (i32, i32), cross_desktop: bool);
    /// The lowercased exe file-stem owning `hwnd` (the kill denylist key).
    fn exe_stem(&self, hwnd: isize) -> String;
    /// The PID owning `hwnd` (0 if it can't be found).
    fn pid_of(&self, hwnd: isize) -> u32;
    /// Our own PID — the kill self-guard compares against this.
    fn current_pid(&self) -> u32;
    /// Force-terminate `pid`. See [`TerminateOutcome`].
    fn terminate(&self, pid: u32) -> TerminateOutcome;
    /// The foreground top-level window, if any.
    fn focused(&self) -> Option<isize>;
    /// The top-level window under the cursor (shell/desktop/tool surfaces excluded).
    fn under_cursor(&self) -> Option<isize>;
    /// Every visible top-level window mostly HIDDEN behind others (the declutter sweep).
    fn occluded(&self) -> Vec<isize>;
    /// Is `hwnd` still a live window?
    fn is_window(&self, hwnd: isize) -> bool;
    /// Is `hwnd` the foreground window right now?
    fn is_foreground(&self, hwnd: isize) -> bool;
    /// Restore `hwnd` if it is minimised.
    fn restore_if_min(&self, hwnd: isize);
    /// Minimise `hwnd`.
    fn minimize(&self, hwnd: isize);
    /// Release any leftover cursor clip after banishing `hwnd`.
    fn free_cursor(&self, hwnd: isize);
    /// Is `hwnd` currently always-on-top? (read-only — the bit `toggle_topmost` flips.)
    fn is_topmost(&self, hwnd: isize) -> bool;
    /// Flip `hwnd`'s always-on-top bit; returns the NEW state (true = now topmost).
    fn toggle_topmost(&self, hwnd: isize) -> bool;
    /// `hwnd`'s screen rect as `(left, top, right, bottom)`.
    fn window_rect(&self, hwnd: isize) -> Option<(i32, i32, i32, i32)>;
    /// Warp the cursor to `(x, y)` and HOLD it there for a breath against the post-warp burst.
    fn pin_cursor(&self, x: i32, y: i32);
    /// A short, friendly window title for status lines (the exe stem if title is empty/huge).
    fn title_of(&self, hwnd: isize) -> String;
}

/// The live backend the verbs call: the real Win32 on Windows, the always-compiled no-op elsewhere.
/// Both implementors are zero-sized, so returning `impl WmOs` by value is a zero-cost selection.
#[cfg(windows)]
fn os() -> impl WmOs {
    imp::Win
}
#[cfg(not(windows))]
fn os() -> impl WmOs {
    stub::Null
}

/// Outcome of asking the OS to terminate a process — kept three-state so the shared `kill` verb can
/// report the SAME three messages it always did: `Err(())` = the handle wouldn't open (access
/// denied), `Ok(true)` = terminated, `Ok(false)` = the terminate call failed.
type TerminateOutcome = Result<bool, ()>;

// ── SUMMON ──────────────────────────────────────────────────────────────────────────────────────

/// Summon the app `needle` (title/exe substring, like glance), delivered by `mode`. Summon now
/// means "give me that app", open or not: NO match LAUNCHES it (resolved the Win+R way, so `brave`
/// opens Brave even when nothing's running); ONE match is raised/brought; SEVERAL matches CYCLE —
/// a bound key has no wheel under it to fan into, so each press walks to the NEXT window of the app
/// (a stable order, wrapping), the way a taskbar icon steps an app's windows. The radial wheel
/// still FANS all the matches out visually as a second tier (see `beacon::resolve_fan`).
pub fn summon(needle: &str, mode: SummonMode) -> String {
    let wins = os().matches(needle);
    match wins.len() {
        0 => launch(needle),
        1 => summon_hwnd(wins[0].0, mode),
        n => {
            // STABLE order (by hwnd) so the cycle visits EVERY window — raising one reshuffles the
            // focus z-order, so we can't index the focus-ordered list; we remember the last hwnd we
            // brought forward and advance past it instead.
            let mut order: Vec<isize> = wins.iter().map(|(h, _)| *h).collect();
            order.sort_unstable();
            let last = summon_cycle()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(needle)
                .copied();
            let next = match last.and_then(|h| order.iter().position(|&x| x == h)) {
                Some(i) => order[(i + 1) % n],
                None => order[0],
            };
            summon_cycle()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(needle.to_string(), next);
            let at = order.iter().position(|&x| x == next).unwrap_or(0) + 1;
            format!(
                "{} \u{00b7} {at}/{n} (press to cycle)",
                summon_hwnd(next, mode)
            )
        }
    }
}

/// Per-app cursor for the bound-key summon CYCLE: the last window brought forward for a needle, so
/// the next press advances to a different one (the wheel fans the matches; a key walks them).
fn summon_cycle() -> &'static std::sync::Mutex<std::collections::HashMap<String, isize>> {
    static C: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, isize>>> =
        std::sync::OnceLock::new();
    C.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Deliver an already-resolved window per `mode` — the body summon shares with the wheel's
/// fan-out (which hands a SPECIFIC matched hwnd, so a multi-match summon picks exactly one).
pub(crate) fn summon_hwnd(hwnd: isize, mode: SummonMode) -> String {
    let title = os().title_of(hwnd);
    match mode {
        SummonMode::Here => {
            let c = os().cursor_pos();
            // `cross_desktop` true is safe even for a current-desktop window (the move is a no-op).
            os().summon_to(hwnd, c, true);
            format!("summoned {title} \u{2192} here")
        }
        SummonMode::Focus => {
            os().restore_if_min(hwnd);
            os().focus(hwnd);
            format!("summoned {title}")
        }
        SummonMode::Toggle => {
            if os().is_foreground(hwnd) {
                os().minimize(hwnd);
                format!("{title} dropped")
            } else {
                os().restore_if_min(hwnd);
                os().focus(hwnd);
                format!("{title} raised")
            }
        }
    }
}

/// LAUNCH an app by name when none of its windows are open — the "summon it into existence" half.
/// `ShellExecuteW("open", name)` resolves exactly like Win+R / the Start search: it consults the
/// App Paths registry key and PATH, so a bare `brave`/`code`/`wt` finds the real exe without a full
/// path. Spawning a process is a real side-effect, so it honours the SAME arm gate as `Run`/scripts
/// (a launch a window-verb couldn't already do must not fire in safe mode). ShellExecute returns a
/// value > 32 on success; anything else (most often "not found") is reported honestly.
fn launch(needle: &str) -> String {
    let name = needle.trim();
    if name.is_empty() {
        return "summon needs an app name".into();
    }
    if !neuron::action::process_spawn_armed() {
        return format!("summon would launch '{name}' [disarmed]");
    }
    // ShellExecute's success sentinel is the legacy "> 32" (the HINSTANCE is a status code here);
    // `shell_open` does the raw call and that comparison, returning the plain success bool.
    if os().shell_open(name) {
        format!("\u{2728} launched {name}")
    } else {
        format!("no '{name}' open, and couldn't launch it")
    }
}

// ── BANISH (minimise) ────────────────────────────────────────────────────────────────────────────

/// Banish (minimise) per `pick`: the focused window, the one under the cursor, or every window
/// buried behind another (the declutter sweep).
pub fn banish(pick: WindowPick) -> String {
    match pick {
        WindowPick::Focused => match os().focused() {
            Some(h) => {
                let t = os().title_of(h);
                os().minimize(h);
                os().free_cursor(h);
                format!("banished {t}")
            }
            None => "nothing focused to banish".into(),
        },
        WindowPick::Hover => match os().under_cursor() {
            Some(h) => {
                let t = os().title_of(h);
                os().minimize(h);
                os().free_cursor(h);
                format!("banished {t} (under cursor)")
            }
            None => "no window under the cursor".into(),
        },
        WindowPick::Behind => {
            let buried = os().occluded();
            for &h in &buried {
                os().minimize(h);
            }
            match buried.len() {
                0 => "nothing's buried \u{2014} your desk is clear".into(),
                1 => "banished 1 buried window".into(),
                n => format!("banished {n} buried windows"),
            }
        }
    }
}

// ── KILL (force-terminate the owning process) ──────────────────────────────────────────────────────

/// Force-TERMINATE the process owning the picked window — not a polite WM_CLOSE, the hard
/// `TerminateProcess` for a hung app. Gated by the arm switch (safe-mode suppresses it) since it's
/// destructive and unrecoverable. `Behind` falls back to the focused window (killing every buried
/// app at once would be a footgun).
pub fn kill(pick: WindowPick) -> String {
    if !neuron::action::input_armed() {
        return "kill suppressed \u{2014} safe mode (arm input to allow)".into();
    }
    let hwnd = match pick {
        WindowPick::Hover => os().under_cursor(),
        _ => os().focused(),
    };
    let Some(hwnd) = hwnd else {
        return "no window to kill".into();
    };
    let t = os().title_of(hwnd);
    let stem = os().exe_stem(hwnd);
    // never let the user terminate US by accident (kill the hovered tile and lose the app).
    let pid = os().pid_of(hwnd);
    if pid == 0 {
        return format!("can't find {t}'s process");
    }
    if pid == os().current_pid() {
        return "won't kill Neuron itself".into();
    }
    // Refuse to force-terminate a session-critical process: killing the shell or a core Windows
    // service crashes/locks the whole session, and a hover-misfire must never be able to do that.
    // (`exe_stem` is the lowercased filename stem — "explorer", "dwm", … .) Windows protects most of
    // these at the OS level too, but here we give a CLEAR reason instead of a confusing "access
    // denied — elevated?".
    const PROTECTED: &[&str] = &[
        "system",
        "smss",
        "csrss",
        "wininit",
        "winlogon",
        "services",
        "lsass",
        "dwm",
        "explorer",
        "fontdrvhost",
    ];
    if PROTECTED.contains(&stem.as_str()) {
        return format!("refusing to kill a critical system process ({stem})");
    }
    match os().terminate(pid) {
        Err(()) => format!("can't terminate {stem} (access denied \u{2014} elevated?)"),
        Ok(true) => format!("\u{2620} killed {stem} ({t})"),
        Ok(false) => format!("terminate failed on {stem}"),
    }
}

// ── PIN (always-on-top toggle) ────────────────────────────────────────────────────────────────────

/// Pin (toggle always-on-top) per `pick` (focused / hover). Behind isn't meaningful for pin, so it
/// falls back to the focused window.
pub fn pin(pick: WindowPick) -> String {
    let hwnd = match pick {
        WindowPick::Hover => os().under_cursor(),
        _ => os().focused(),
    };
    let Some(hwnd) = hwnd else {
        return "no window to pin".into();
    };
    let t = os().title_of(hwnd);
    let now_top = os().toggle_topmost(hwnd);
    if now_top {
        format!("\u{1f4cc} {t} pinned on top")
    } else {
        format!("{t} unpinned")
    }
}

/// Predict the pin toggle for `pick` (read-only): the window a press targets + whether it's
/// ALREADY pinned — so the wedge can say "unpin" on a pinned window instead of always claiming
/// "pin" (the same label-lies-about-the-press bug the tether had). `Behind` → focused, like pin().
pub fn pin_preview(pick: WindowPick) -> Option<(String, bool)> {
    let hwnd = match pick {
        WindowPick::Hover => os().under_cursor(),
        _ => os().focused(),
    }?;
    Some((os().title_of(hwnd), os().is_topmost(hwnd)))
}

// ── TETHER (the warpstone) ────────────────────────────────────────────────────────────────────────

/// A saved window-state: which window, and where in it the cursor sat (relative to its top-left, so
/// the cursor lands in the same SPOT even if the window has since moved).
#[derive(Clone, Copy)]
struct Anchor {
    hwnd: isize,
    rel: (i32, i32),
}

fn stones() -> &'static std::sync::Mutex<std::collections::HashMap<String, Anchor>> {
    use std::sync::OnceLock;
    static S: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Anchor>>> =
        OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// The windows currently holding a tether anchor (any slot) — so the teleport map can SHOW where
/// your warpstones are while you aim. Dead/closed windows are filtered out (a stone whose window
/// is gone is as good as unset, exactly as [`tether`] treats it). Order is arbitrary (HashMap).
pub fn tether_hwnds() -> Vec<isize> {
    stones()
        .lock()
        .map(|s| {
            s.values()
                .map(|a| a.hwnd)
                .filter(|&h| os().is_window(h))
                .collect()
        })
        .unwrap_or_default()
}

/// What pressing the tether on `slot` would do RIGHT NOW — the wedge label predicts the action at
/// cast time instead of just naming the current stone. Mirrors [`tether`]'s exact three-state
/// decision (live anchor? cursor's window on it?) so what the label says IS what activation does.
/// `Release` is the destructive branch (the wedge can glow warm for it).
pub enum TetherPreview {
    /// nothing tethered here → a press SETS the stone at the current window+spot
    Set,
    /// away from the stone → a press WARPS back to `title`
    Warp { title: String },
    /// standing on the stone → a press RELEASES it (the only unset path)
    Release { title: String },
}

/// Predict the tether action for `slot` (see [`TetherPreview`]). Read-only — the same anchor +
/// foreground checks [`tether`] commits with, so the radial wedge never lies about its next press.
pub fn tether_preview(slot: &str) -> TetherPreview {
    let key = slot.trim().to_string();
    // a stone whose window has since closed is as good as unset — exactly tether()'s `existing`.
    let existing = stones()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .copied()
        .filter(|a| os().is_window(a.hwnd));
    match existing {
        Some(a) if os().is_foreground(a.hwnd) => TetherPreview::Release {
            title: os().title_of(a.hwnd),
        },
        Some(a) => TetherPreview::Warp {
            title: os().title_of(a.hwnd),
        },
        None => TetherPreview::Set,
    }
}

/// The warpstone — ONE intuitive button, three natural states off a single press: with nothing
/// tethered it drops the stone at the window+spot you're in now (mark); while you're away it warps
/// you back to it (recall); while you're already on it, it releases the tether (untether — the
/// thing the old toggle never let you do). To move a tether: release it (press while on it), then
/// press on the new window. `slot` names the stone (blank = the default; bind more for more stones).
pub fn tether(slot: &str) -> String {
    let key = slot.trim().to_string();
    let label = if key.is_empty() {
        String::new()
    } else {
        format!(" [{key}]")
    };
    // a stone whose window has since closed is as good as unset.
    let existing = stones()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .copied()
        .filter(|a| os().is_window(a.hwnd));

    match existing {
        // standing ON the tethered window → let it go (this is the untether the toggle never had)
        Some(a) if os().is_foreground(a.hwnd) => {
            stones()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
            format!("\u{1f5ff} tether released{label}")
        }
        // away from the stone → come back to it
        Some(a) => {
            warp_to(a);
            format!("\u{2728} warped back{label} \u{2192} {}", os().title_of(a.hwnd))
        }
        // nothing tethered → drop the stone here
        None => match capture() {
            Some(a) => {
                stones()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(key.clone(), a);
                format!("\u{1f5ff} tethered{label} \u{2192} {}", os().title_of(a.hwnd))
            }
            None => "nothing to tether to here".into(),
        },
    }
}

// ── WORMHOLE (the two-anchor portal) ───────────────────────────────────────────────────────────────
// Where `tether` is arbitrary-place → ONE anchor, a wormhole is a portal between TWO fixed anchors:
// the slot holds A and B, and a press swaps you A ⇄ B (focus + cursor), regardless of which virtual
// desktop or window you're on. The two anchors live in the SAME `stones()` map under derived keys
// (a control-char prefix that no trimmed user slot can produce), so the warp machinery is shared.

/// The two `stones()` keys a wormhole `slot` stores its anchors under — `\u{1}` can't appear in a
/// trimmed user slot, so a wormhole's endpoints never collide with a plain mark tether's stone.
fn wormhole_keys(slot: &str) -> (String, String) {
    let s = slot.trim();
    (format!("\u{1}wh:{s}:a"), format!("\u{1}wh:{s}:b"))
}

/// What a wormhole press will do RIGHT NOW (the wedge predicts it). `Set` = an endpoint is still
/// empty, so a press CAPTURES the current window into it (`side` = "a"/"b" — the rebind that builds
/// the portal by use); `Swap` = both ends exist (or you're standing on one), and a press warps to
/// the OTHER endpoint named by `title`.
pub enum WormholePreview {
    Set { side: &'static str },
    Swap { title: String },
}

/// Predict the wormhole action for `slot` (see [`WormholePreview`]) — read-only, the exact anchor +
/// foreground checks [`wormhole`] commits with, so the wedge never lies.
pub fn wormhole_preview(slot: &str) -> WormholePreview {
    let (ka, kb) = wormhole_keys(slot);
    let live = |k: &str| {
        stones()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(k)
            .copied()
            .filter(|a| os().is_window(a.hwnd))
    };
    let (a, b) = (live(&ka), live(&kb));
    match (a, b) {
        // both ends set: a press swaps to the side you're NOT on (default → A when at neither).
        (Some(a), Some(b)) => {
            let to = if os().is_foreground(a.hwnd) { b } else { a };
            WormholePreview::Swap {
                title: os().title_of(to.hwnd),
            }
        }
        // a half-built portal completes itself: the empty endpoint is what a press captures next.
        (None, _) => WormholePreview::Set { side: "a" },
        (Some(_), None) => WormholePreview::Set { side: "b" },
    }
}

/// The WORMHOLE — a portal between two fixed anchors. With both ends set, a press swaps you A ⇄ B
/// (you land on the endpoint you're NOT currently at; from neither, you go to A). While an endpoint
/// is still empty, a press CAPTURES the current window+spot into it (A first, then B) — so you build
/// the portal by standing in each place and pressing once, and thereafter it just swaps. `slot`
/// names the portal (blank = the default; bind more for more portals). Reuses the warpstone warp.
pub fn wormhole(slot: &str) -> String {
    let key = slot.trim();
    let label = if key.is_empty() {
        String::new()
    } else {
        format!(" [{key}]")
    };
    let (ka, kb) = wormhole_keys(slot);
    let live = |k: &str| {
        stones()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(k)
            .copied()
            .filter(|a| os().is_window(a.hwnd))
    };
    let (a, b) = (live(&ka), live(&kb));

    match (a, b) {
        // both anchors live → swap to the far side (the side you're not standing on; A from neither)
        (Some(a), Some(b)) => {
            let to = if os().is_foreground(a.hwnd) { b } else { a };
            warp_to(to);
            format!("\u{1f300} wormhole{label} \u{2192} {}", os().title_of(to.hwnd))
        }
        // endpoint A empty → capture here as A (the first half of the portal)
        (None, _) => match capture() {
            Some(anc) => {
                stones()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(ka, anc);
                format!(
                    "\u{1f300} wormhole A set{label} \u{2192} {} \u{00b7} now mark B",
                    os().title_of(anc.hwnd)
                )
            }
            None => "nothing to anchor here".into(),
        },
        // A set, B empty → capture here as B (completes the portal)
        (Some(_), None) => match capture() {
            Some(anc) => {
                stones()
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(kb, anc);
                format!(
                    "\u{1f300} wormhole B set{label} \u{2192} {} \u{00b7} press to swap",
                    os().title_of(anc.hwnd)
                )
            }
            None => "nothing to anchor here".into(),
        },
    }
}

/// Snapshot the current window-state (foreground window + cursor relative to it).
fn capture() -> Option<Anchor> {
    let hwnd = os().focused()?;
    let r = os().window_rect(hwnd)?;
    let c = os().cursor_pos();
    Some(Anchor {
        hwnd,
        rel: (c.0 - r.0, c.1 - r.1),
    })
}

/// Return to a saved state: restore + focus the window (switches desktop natively), drop the
/// cursor back on the same spot inside it — and make the landing STICK even while the physical
/// mouse keeps moving.
fn warp_to(a: Anchor) {
    os().restore_if_min(a.hwnd);
    os().focus(a.hwnd);
    if let Some(r) = os().window_rect(a.hwnd) {
        let (x, y) = (r.0 + a.rel.0, r.1 + a.rel.1);
        // clamp into the window so a shrunk window never parks the cursor off it
        let x = x.clamp(r.0, r.2 - 1);
        let y = y.clamp(r.1, r.3 - 1);
        os().pin_cursor(x, y);
    }
}

// ── per-OS window primitives ─────────────────────────────────────────────────────────────────────
// The verbs above are platform-neutral: they route the opaque `isize` handle and decide what to do.
// Everything that actually TOUCHES the OS — focus, minimise, topmost, enumerate, terminate, cursor —
// lives behind the [`WmOs`] seam. `imp::Win` is the real Win32 (the bodies relocated VERBATIM from
// the old whole-file body, only wrapped as `&self` methods); `stub::Null` is an honest no-op so the
// verbs degrade to "no window" off-Windows, and — crucially — is ALWAYS compiled, so the compiler
// type-checks BOTH backends against the trait on every target (the parity guarantee). The verbs pick
// the live one through `os()`, calling `os().focused()` etc. without a cfg in their bodies.

#[cfg(windows)]
mod imp {
    use super::{TerminateOutcome, WmOs};

    /// The real Win32 backend (zero-sized: every method talks straight to the OS, no state to hold).
    pub struct Win;

    impl WmOs for Win {
        fn matches(&self, needle: &str) -> Vec<(isize, String)> {
            crate::glance::matches(needle)
        }

        fn shell_open(&self, name: &str) -> bool {
            use windows_sys::Win32::UI::Shell::ShellExecuteW;
            use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
            let file: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let verb: Vec<u16> = "open\0".encode_utf16().collect();
            let rc = unsafe {
                ShellExecuteW(
                    std::ptr::null_mut(),
                    verb.as_ptr(),
                    file.as_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                    SW_SHOWNORMAL as _,
                )
            };
            rc as isize > 32
        }

        fn cursor_pos(&self) -> (i32, i32) {
            let mut c = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut c) };
            (c.x, c.y)
        }

        fn focus(&self, hwnd: isize) {
            crate::teleport::force_foreground(hwnd);
        }

        fn summon_to(&self, hwnd: isize, to: (i32, i32), cross_desktop: bool) {
            let _ = crate::teleport::summon(hwnd, to, cross_desktop);
        }

        fn exe_stem(&self, hwnd: isize) -> String {
            crate::teleport::exe_stem(hwnd)
        }

        fn pid_of(&self, hwnd: isize) -> u32 {
            let mut pid = 0u32;
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                    hwnd as _, &mut pid,
                );
            }
            pid
        }

        fn current_pid(&self) -> u32 {
            unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() }
        }

        fn terminate(&self, pid: u32) -> TerminateOutcome {
            unsafe {
                use windows_sys::Win32::System::Threading::{
                    OpenProcess, TerminateProcess, PROCESS_TERMINATE,
                };
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if h.is_null() {
                    return Err(());
                }
                let ok = TerminateProcess(h, 1) != 0;
                windows_sys::Win32::Foundation::CloseHandle(h);
                Ok(ok)
            }
        }

        fn focused(&self) -> Option<isize> {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetAncestor, GetForegroundWindow, GA_ROOT,
            };
            unsafe {
                let h = GetForegroundWindow();
                if h.is_null() {
                    return None;
                }
                let root = GetAncestor(h, GA_ROOT);
                Some(if root.is_null() {
                    h as isize
                } else {
                    root as isize
                })
            }
        }

        fn under_cursor(&self) -> Option<isize> {
            use windows_sys::Win32::Foundation::POINT;
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetAncestor, GetCursorPos, WindowFromPoint, GA_ROOT,
            };
            unsafe {
                let mut p = POINT { x: 0, y: 0 };
                GetCursorPos(&mut p);
                let h = WindowFromPoint(p);
                if h.is_null() {
                    return None;
                }
                let root = GetAncestor(h, GA_ROOT);
                let root = if root.is_null() { h } else { root };
                // ignore the desktop/shell — banishing the desktop is nonsense
                if is_ignorable(root as isize) {
                    None
                } else {
                    Some(root as isize)
                }
            }
        }

        /// Every visible, titled, non-tool top-level window that is mostly HIDDEN behind others —
        /// found by sampling its interior against the live z-order (WindowFromPoint resolves to the
        /// root that actually owns each pixel). A window <30% visible is "behind" and gets swept.
        fn occluded(&self) -> Vec<isize> {
            use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT};
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                EnumWindows, GetAncestor, IsIconic, IsWindowVisible, WindowFromPoint, GA_ROOT,
            };
            unsafe extern "system" fn cb(hwnd: HWND, lp: LPARAM) -> i32 {
                unsafe {
                    let out = &mut *(lp as *mut Vec<isize>);
                    if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
                        return 1;
                    }
                    if is_ignorable(hwnd as isize) {
                        return 1;
                    }
                    let Some(r) = window_rect(hwnd as isize) else {
                        return 1;
                    };
                    let (w, h) = (r.2 - r.0, r.3 - r.1);
                    if w < 80 || h < 60 {
                        return 1;
                    }
                    // 4×4 interior grid, inset off the edges (borders/shadows lie about ownership)
                    let mut seen = 0;
                    let mut vis = 0;
                    for gy in 1..=4 {
                        for gx in 1..=4 {
                            let px = r.0 + w * gx / 5;
                            let py = r.1 + h * gy / 5;
                            let top = WindowFromPoint(POINT { x: px, y: py });
                            if top.is_null() {
                                continue;
                            }
                            let root = GetAncestor(top, GA_ROOT);
                            let root = if root.is_null() { top } else { root };
                            seen += 1;
                            if root as isize == hwnd as isize {
                                vis += 1;
                            }
                        }
                    }
                    if seen > 0 && (vis as f32) / (seen as f32) < 0.30 {
                        out.push(hwnd as isize);
                    }
                    1
                }
            }
            let mut out: Vec<isize> = Vec::new();
            unsafe {
                EnumWindows(Some(cb), &mut out as *mut _ as isize);
            }
            out
        }

        fn is_window(&self, hwnd: isize) -> bool {
            unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsWindow(hwnd as _) != 0 }
        }

        fn is_foreground(&self, hwnd: isize) -> bool {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() as isize == hwnd
            }
        }

        fn restore_if_min(&self, hwnd: isize) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_RESTORE};
            if is_iconic(hwnd) {
                unsafe { ShowWindow(hwnd as _, SW_RESTORE) };
            }
        }

        fn minimize(&self, hwnd: isize) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE};
            unsafe { ShowWindow(hwnd as _, SW_MINIMIZE) };
        }

        /// Free the cursor after banishing `hwnd`. A fullscreen game that `ClipCursor`-confined the
        /// mouse to the screen centre keeps that clip alive even once its window is minimised —
        /// Windows only resets the clip on the next foreground activation, so until the user alt-tabs
        /// (or hits Win) the cursor stays "locked to the centre" with nowhere to go. We do that
        /// handoff immediately: drop the system clip ourselves AND make whoever Windows raised
        /// genuinely foreground — the clip only obeys the foreground thread, so revoking the banished
        /// game's foreground is what makes the release STICK even if it re-clips from its own loop. We
        /// use the STRONG handoff (`force_foreground`: the topmost flip + `AttachThreadInput`) rather
        /// than a bare `SetForegroundWindow` the game can deny — a denied handoff would leave the game
        /// foreground and its per-frame re-clip would win again. Then we clear the clip ONCE MORE a
        /// beat later (off-thread) to catch a last re-clip the game's loop fires before it loses
        /// foreground. No jank, just the alt-tab you shouldn't need.
        fn free_cursor(&self, _hwnd: isize) {
            use windows_sys::Win32::UI::WindowsAndMessaging::{ClipCursor, GetForegroundWindow};
            unsafe { ClipCursor(std::ptr::null()) }; // release the screen-confine the game left behind
                                                     // make whoever Windows raised after the minimise GENUINELY foreground — the clip only obeys the
                                                     // foreground thread, so revoking the game's foreground is what makes the release stick.
            let fg = unsafe { GetForegroundWindow() };
            if !fg.is_null() {
                crate::teleport::force_foreground(fg as isize);
            }
            // a game can fire one more ClipCursor from its render loop before it loses foreground; clear it
            // again a beat later so that last in-flight re-clip can't re-lock the cursor to centre.
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(40));
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ClipCursor(std::ptr::null()) };
            });
        }

        fn is_topmost(&self, hwnd: isize) -> bool {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                GetWindowLongW, GWL_EXSTYLE, WS_EX_TOPMOST,
            };
            unsafe { (GetWindowLongW(hwnd as _, GWL_EXSTYLE) as u32) & WS_EX_TOPMOST != 0 }
        }

        fn toggle_topmost(&self, hwnd: isize) -> bool {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                SetWindowPos, HWND_NOTOPMOST, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            };
            let was = self.is_topmost(hwnd);
            unsafe {
                let after = if was { HWND_NOTOPMOST } else { HWND_TOPMOST };
                SetWindowPos(
                    hwnd as _,
                    after,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
            !was
        }

        fn window_rect(&self, hwnd: isize) -> Option<(i32, i32, i32, i32)> {
            window_rect(hwnd)
        }

        /// Warp the cursor to (x, y) and HOLD it there for a breath. A bare `SetCursorPos` loses the
        /// race the user reported: `force_foreground` whispers a SendInput move, the OS still has
        /// in-flight physical mouse deltas queued, and the user's own hand keeps moving — all of which
        /// overwrite the cursor the instant after the set, so it snaps back and "continues moving as
        /// if it never teleported". Re-asserting the target on a tight ~90ms settle absorbs that
        /// post-warp motion: the landing wins, then the hand has the cursor back. A held-still mouse
        /// never notices (re-pinning to the same spot is invisible) — the case that already worked is
        /// byte-unchanged.
        fn pin_cursor(&self, x: i32, y: i32) {
            use windows_sys::Win32::Foundation::POINT;
            use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
            // Off the caller's (dispatch) thread so a warp never blocks the worker. Re-assert the landing
            // ONLY until the post-warp burst (queued deltas + the foreground whisper) has DRAINED — i.e. the
            // cursor stays put across two reads — then stop, so a hand that's genuinely moving again gets
            // control back fast (a held-still mouse releases in ~12ms, not a flat 90ms "stick"). Capped so a
            // continuously-moving mouse can't be fought forever; by the cap the landing has still won.
            std::thread::spawn(move || unsafe {
                SetCursorPos(x, y);
                let mut stable = 0;
                for _ in 0..16 {
                    std::thread::sleep(std::time::Duration::from_millis(6));
                    let mut c = POINT { x: 0, y: 0 };
                    GetCursorPos(&mut c);
                    if (c.x - x).abs() <= 1 && (c.y - y).abs() <= 1 {
                        stable += 1;
                        if stable >= 2 {
                            break; // landing held twice running — the burst is over, release the cursor
                        }
                    } else {
                        stable = 0;
                        SetCursorPos(x, y); // a delta knocked it off the spot; re-assert the target
                    }
                }
            });
        }

        fn title_of(&self, hwnd: isize) -> String {
            use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowTextW;
            unsafe {
                let mut buf = [0u16; 128];
                let n = GetWindowTextW(hwnd as _, buf.as_mut_ptr(), buf.len() as i32);
                let title = if n > 0 {
                    String::from_utf16_lossy(&buf[..n as usize])
                } else {
                    String::new()
                };
                let title = title.trim();
                if title.is_empty() || title.chars().count() > 40 {
                    let exe = crate::teleport::exe_stem(hwnd);
                    if exe.is_empty() {
                        "window".into()
                    } else if title.is_empty() {
                        exe
                    } else {
                        // long title: prefer the first chunk before a separator, else the exe
                        title
                            .split([' ', '-', '\u{2014}', '|'])
                            .next()
                            .unwrap_or(&exe)
                            .trim()
                            .to_string()
                    }
                } else {
                    title.to_string()
                }
            }
        }
    }

    // ── private helpers (not part of the seam) ──
    // These stay free module-level fns: `is_ignorable`/`is_iconic` are internal predicates, and
    // `window_rect` is shared by the `WmOs::window_rect` method AND the `occluded` EnumWindows
    // callback (an `extern "system" fn` that has no `&self` to dispatch through), so its GetWindowRect
    // body lives here once, called verbatim from both.

    /// Windows we must never act on: ours, the shell, the desktop, untitled tool surfaces.
    fn is_ignorable(hwnd: isize) -> bool {
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId, GWL_EXSTYLE,
            WS_EX_TOOLWINDOW,
        };
        unsafe {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd as _, &mut pid);
            if pid == GetCurrentProcessId() {
                return true; // never our own overlay / tiles / palette
            }
            if GetWindowTextLengthW(hwnd as _) == 0 {
                return true;
            }
            (GetWindowLongW(hwnd as _, GWL_EXSTYLE) as u32) & WS_EX_TOOLWINDOW != 0
        }
    }

    fn is_iconic(hwnd: isize) -> bool {
        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::IsIconic(hwnd as _) != 0 }
    }

    fn window_rect(hwnd: isize) -> Option<(i32, i32, i32, i32)> {
        use windows_sys::Win32::Foundation::RECT;
        use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowRect;
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            if GetWindowRect(hwnd as _, &mut r) == 0 {
                None
            } else {
                Some((r.left, r.top, r.right, r.bottom))
            }
        }
    }
}

mod stub {
    // ALWAYS compiled (NOT cfg-gated) — this is the crux of the seam. On Windows `Null` is never the
    // live backend, but the compiler still type-checks `impl WmOs for Null` against the SAME trait
    // the verbs call, so any drift between a verb's needs and the seam breaks the Windows build at
    // once instead of only off-Windows. The no-op bodies touch no platform API, so always compiling
    // them is free. `#![allow(dead_code)]`: on Windows nothing constructs `Null`, so its struct (and
    // its trait methods, which are reachable only through that unused type) would otherwise warn.
    #![allow(dead_code)]
    use super::{TerminateOutcome, WmOs};

    /// The honest no-op backend (zero-sized). Off-Windows there is no window system here yet: every
    /// read finds nothing and every act is a no-op, so the portable verbs fall through to their
    /// honest "no window" / "windows-only" paths. The handle stays an opaque `isize` — these never
    /// produce a real one, so callers only ever see the empty/None/Err answers.
    pub struct Null;

    impl WmOs for Null {
        fn matches(&self, _needle: &str) -> Vec<(isize, String)> {
            Vec::new()
        }
        fn shell_open(&self, _name: &str) -> bool {
            false
        }
        fn cursor_pos(&self) -> (i32, i32) {
            (0, 0)
        }
        fn focus(&self, _hwnd: isize) {}
        fn summon_to(&self, _hwnd: isize, _to: (i32, i32), _cross_desktop: bool) {}
        fn exe_stem(&self, _hwnd: isize) -> String {
            String::new()
        }
        fn pid_of(&self, _hwnd: isize) -> u32 {
            0
        }
        fn current_pid(&self) -> u32 {
            // Belt-and-suspenders: off-Windows `kill` returns early at `pid == 0` (since `pid_of`
            // here is always 0), so the `pid == current_pid()` self-guard is never actually reached
            // in the stub. We still hand back a nonzero, never-matched sentinel — it just must never
            // equal 0 (which would mean "no process") in case that early-out ever changes.
            u32::MAX
        }
        fn terminate(&self, _pid: u32) -> TerminateOutcome {
            Err(())
        }
        fn focused(&self) -> Option<isize> {
            None
        }
        fn under_cursor(&self) -> Option<isize> {
            None
        }
        fn occluded(&self) -> Vec<isize> {
            Vec::new()
        }
        fn is_window(&self, _hwnd: isize) -> bool {
            false
        }
        fn is_foreground(&self, _hwnd: isize) -> bool {
            false
        }
        fn restore_if_min(&self, _hwnd: isize) {}
        fn minimize(&self, _hwnd: isize) {}
        fn free_cursor(&self, _hwnd: isize) {}
        fn is_topmost(&self, _hwnd: isize) -> bool {
            false
        }
        fn toggle_topmost(&self, _hwnd: isize) -> bool {
            false
        }
        fn window_rect(&self, _hwnd: isize) -> Option<(i32, i32, i32, i32)> {
            None
        }
        fn pin_cursor(&self, _x: i32, _y: i32) {}
        fn title_of(&self, _hwnd: isize) -> String {
            "window".into()
        }
    }
}
