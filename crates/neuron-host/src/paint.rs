//! The external-paint policy engine — the one place a protocol adapter's frame
//! becomes a fade-ramped, mode-aware, scope-gated arbiter layer.
//!
//! Every protocol face (Chroma REST, Chroma SHM, OpenRGB) paints "someone
//! else's" lighting over the user's base. Three concerns are common to all of
//! them and have nothing to do with the wire protocol, so they live here once
//! instead of three times:
//!
//! - [`PaintPolicy`] — the user's settings for a whole family of external
//!   paint ("game Chroma", "OpenRGB clients"): blend mode, strength, fade time,
//!   and an optional per-surface allow-set. Lock-free reads (atomics + an
//!   `RwLock` only on the rarely-touched allow-set) because it is sampled once
//!   per LED-resolve on the kernel actor thread while the settings page writes
//!   it from the GUI thread.
//! - [`FadeRamp`] — a framerate-independent linear crossfade toward a target,
//!   so a face fades IN over the base when a source appears and OUT when it
//!   leaves, at a rate the user set in wall-clock milliseconds regardless of
//!   how often the layer happens to render.
//! - [`merge_cells`] — the mode-aware black rule (see the function docs): the
//!   one correct answer to "the game painted this LED black" that keeps a
//!   mostly-black `Multiply` frame from blacking out the whole board.
//!
//! [`PolicyLayer`] ties the three together into the single [`LiveContent`] shim
//! every TCP/REST-style adapter claims. Its paint buffer is an
//! `Arc<Mutex<…>>` the adapter keeps a clone of: a new frame MUTATES the shared
//! buffer and refreshes the lease, it does NOT replace the layer — so the fade
//! ramp's state survives across frames instead of resetting on every paint.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::api::SurfaceInfo;
use crate::arbiter::{BlendMode, LiveContent, Rgb};

/// Below this opacity a faded layer contributes nothing worth compositing — the
/// same floor `arbiter::resolve` and `Arbiter::claims` gate on.
const MIN_VISIBLE_ALPHA: f32 = 0.001;

/// One family of external paint's live settings, shared behind an `Arc` between
/// the GUI (which writes it) and the kernel actor thread (which samples it once
/// per resolve). There are two instances by design — one for the Chroma faces,
/// one for OpenRGB clients — so the settings page describes "game lighting" and
/// "OpenRGB clients" without leaking how a given source talks to Neuron.
///
/// Every field is read lock-free in the hot path: the three scalars are atomics
/// and the allow-set sits behind an `RwLock` touched only when the user changes
/// device scope, never per LED.
#[derive(Debug)]
pub struct PaintPolicy {
    blend: AtomicU8,
    /// Overall opacity of this family's paint, `0..=100` percent.
    strength: AtomicU8,
    /// Crossfade duration in milliseconds, `0..=2500` (0 = instant).
    fade_ms: AtomicU32,
    /// `None` = every surface allowed; `Some(set)` = only these surface keys
    /// receive this family's paint (the per-device scope toggle).
    surfaces: RwLock<Option<HashSet<String>>>,
}

impl Default for PaintPolicy {
    fn default() -> Self {
        Self {
            blend: AtomicU8::new(BlendMode::Screen.to_bits()),
            strength: AtomicU8::new(100),
            fade_ms: AtomicU32::new(450),
            surfaces: RwLock::new(None),
        }
    }
}

impl PaintPolicy {
    /// A default policy (Screen blend, full strength, 450ms fade, all surfaces)
    /// behind an `Arc` for sharing with the adapters that read it.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// An opaque, instant, unscoped policy: `Over` blend, full strength, no
    /// fade. This is "show the client's paint exactly as sent" — the honest
    /// default for OpenRGB config tools, which expect a set colour to appear
    /// as-is rather than merged/faded over the base.
    pub fn opaque() -> Arc<Self> {
        Arc::new(Self {
            blend: AtomicU8::new(BlendMode::Over.to_bits()),
            strength: AtomicU8::new(100),
            fade_ms: AtomicU32::new(0),
            surfaces: RwLock::new(None),
        })
    }

    /// Overwrite every field at once (the settings page's one write). `strength`
    /// clamps to `0..=100`, `fade_ms` to `0..=2500`.
    pub fn update(
        &self,
        blend: BlendMode,
        strength: u8,
        fade_ms: u32,
        surfaces: Option<HashSet<String>>,
    ) {
        self.blend.store(blend.to_bits(), Ordering::Relaxed);
        self.strength.store(strength.clamp(0, 100), Ordering::Relaxed);
        self.fade_ms.store(fade_ms.clamp(0, 2500), Ordering::Relaxed);
        *self.surfaces.write().unwrap_or_else(|e| e.into_inner()) = surfaces;
    }

    pub fn blend_mode(&self) -> BlendMode {
        BlendMode::from_bits(self.blend.load(Ordering::Relaxed))
    }

    /// Strength as an opacity fraction in `[0,1]` — the multiplier applied on
    /// top of a [`FadeRamp`] value to get a layer's final alpha.
    pub fn alpha(&self) -> f32 {
        self.strength.load(Ordering::Relaxed) as f32 / 100.0
    }

    /// Configured crossfade duration in seconds (0 = instant).
    pub fn fade_secs(&self) -> f32 {
        self.fade_ms.load(Ordering::Relaxed) as f32 / 1000.0
    }

    pub fn allows_surface(&self, surface: &SurfaceInfo) -> bool {
        self.allows_key(&surface.key)
    }

    /// Whether this family's paint reaches the given surface key — `true` unless
    /// an allow-set is present and excludes it.
    pub fn allows_key(&self, key: &str) -> bool {
        self.surfaces
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|set| set.contains(key))
    }
}

/// A framerate-independent linear crossfade toward a moving target in `[0,1]`.
///
/// Each [`advance`](Self::advance) moves the value toward `target` by
/// `dt / fade_secs`, where `dt` is real elapsed time since the previous call —
/// so the fade takes `fade_secs` of WALL time whether the layer renders at 6fps
/// or 240fps. The first call snaps `dt` to zero (no origin to measure from yet),
/// and `fade_secs <= 0` means instant (one step reaches the target). A single
/// `dt` is capped so a long stall (the layer wasn't rendered for a while) can't
/// jump the value in one frame.
#[derive(Clone, Debug)]
pub struct FadeRamp {
    value: f32,
    last: Option<Instant>,
}

impl FadeRamp {
    /// A ramp seeded at `initial` (a fresh face starts at `0.0` and fades in; a
    /// re-claim of a source already on screen starts at `1.0` to avoid a dip).
    pub fn new(initial: f32) -> FadeRamp {
        FadeRamp { value: initial.clamp(0.0, 1.0), last: None }
    }

    /// The current ramp value without advancing it.
    pub fn value(&self) -> f32 {
        self.value
    }

    /// Advance toward `target` by the wall time elapsed since the last call and
    /// return the new value. Framerate-independent: coarse and fine `dt`
    /// stepping over the same wall interval converge to the same value.
    pub fn advance(&mut self, now: Instant, target: f32, fade_secs: f32) -> f32 {
        let dt = self
            .last
            .map(|t| now.duration_since(t).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.25);
        self.last = Some(now);
        let step = if fade_secs > 0.0 { dt / fade_secs } else { 1.0 };
        if self.value < target {
            self.value = (self.value + step).min(target);
        } else if self.value > target {
            self.value = (self.value - step).max(target);
        }
        self.value
    }
}

/// The mode-aware black rule: in any non-`Over` blend (`Screen`/`Add`/
/// `Multiply`) a painted pure-black cell `(0,0,0)` becomes `None` — transparent,
/// "the game says nothing here" — while in `Over` black stays opaque (an
/// explicit LED-off).
///
/// Why one rule for all merge modes: `Multiply`-by-black would drive the base
/// to black wherever a mostly-black game frame paints, blacking out the whole
/// board (the "TINT" bug). `Screen` and `Add` are the identity on black, so
/// dropping their black cells to transparent costs nothing and lets a single
/// rule cover every non-`Over` mode instead of special-casing `Multiply`.
pub fn merge_cells(mode: BlendMode, cells: &[Option<Rgb>]) -> Vec<Option<Rgb>> {
    if mode == BlendMode::Over {
        return cells.to_vec();
    }
    cells
        .iter()
        .map(|c| match c {
            Some(Rgb(0, 0, 0)) => None,
            other => *other,
        })
        .collect()
}

/// The one live-content shim every TCP/REST-style adapter claims for a surface.
///
/// The paint buffer is shared: the adapter keeps a clone of the `Arc<Mutex<…>>`
/// in its per-session bookkeeping and writes the next frame INTO it, refreshing
/// the lease, instead of replacing the whole layer. That is what preserves the
/// [`FadeRamp`]'s state across frames — a paint that rebuilt the shim would
/// reset the ramp and restart the fade on every frame.
///
/// [`render`](LiveContent::render) advances the ramp (the trait renders with
/// `&mut self`) and caches the level so [`alpha`](LiveContent::alpha), which
/// only has `&self`, can read this frame's value — the same split
/// `ChromaShmLayer` uses.
pub struct PolicyLayer {
    key: String,
    leds: usize,
    cells: Arc<Mutex<Vec<Option<Rgb>>>>,
    policy: Arc<PaintPolicy>,
    ramp: FadeRamp,
    /// Ramp value cached by the last `render`, so `alpha` (which is `&self`) can
    /// report this frame's opacity without re-advancing the ramp.
    level: f32,
}

impl PolicyLayer {
    /// Claim-once layer over a shared paint buffer. `initial_alpha` seeds the
    /// fade (0.0 for a fresh face that should fade in; 1.0 for a re-claim of a
    /// source already on screen, so a swept-then-recovered lease doesn't dip).
    pub fn new(
        key: String,
        leds: usize,
        cells: Arc<Mutex<Vec<Option<Rgb>>>>,
        policy: Arc<PaintPolicy>,
        initial_alpha: f32,
    ) -> PolicyLayer {
        PolicyLayer {
            key,
            leds,
            cells,
            policy,
            ramp: FadeRamp::new(initial_alpha),
            level: initial_alpha.clamp(0.0, 1.0),
        }
    }
}

impl LiveContent for PolicyLayer {
    fn render(&mut self, now: Instant) -> Vec<Option<Rgb>> {
        let allowed = self.policy.allows_key(&self.key);
        let target = if allowed { 1.0 } else { 0.0 };
        self.level = self.ramp.advance(now, target, self.policy.fade_secs());
        // Gate on the RAMPED level only — `allowed` steers the target, never the
        // cells directly. A hands-off toggle therefore crossfades out (and the
        // claim stays honestly visible while pixels are still on the board)
        // instead of hard-cutting cells while `alpha()` reports a live layer.
        if self.level <= MIN_VISIBLE_ALPHA {
            return vec![None; self.leds];
        }
        let buf = self.cells.lock().unwrap_or_else(|e| e.into_inner());
        merge_cells(self.policy.blend_mode(), &buf)
    }

    fn alpha(&self) -> f32 {
        self.level * self.policy.alpha()
    }

    fn blend_mode(&self) -> BlendMode {
        self.policy.blend_mode()
    }

    fn boxed_clone(&self) -> Box<dyn LiveContent> {
        Box::new(PolicyLayer {
            key: self.key.clone(),
            leds: self.leds,
            cells: Arc::clone(&self.cells),
            policy: Arc::clone(&self.policy),
            ramp: self.ramp.clone(),
            level: self.level,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn buf(cells: Vec<Option<Rgb>>) -> Arc<Mutex<Vec<Option<Rgb>>>> {
        Arc::new(Mutex::new(cells))
    }

    #[test]
    fn tint_multiply_drops_painted_black_but_keeps_colour() {
        // A mostly-black TINT (Multiply) frame: only one cell is coloured.
        let cells = vec![Some(Rgb(0, 0, 0)), Some(Rgb(10, 200, 30)), None];
        let merged = merge_cells(BlendMode::Multiply, &cells);
        assert_eq!(merged[0], None, "painted black is transparent under a merge mode");
        assert_eq!(merged[1], Some(Rgb(10, 200, 30)), "coloured cells survive to tint the base");
        assert_eq!(merged[2], None, "an already-transparent cell stays transparent");
    }

    #[test]
    fn screen_and_add_also_drop_black_one_rule_for_all_merges() {
        let cells = vec![Some(Rgb(0, 0, 0)), Some(Rgb(0, 0, 1))];
        for mode in [BlendMode::Screen, BlendMode::Add] {
            let merged = merge_cells(mode, &cells);
            assert_eq!(merged[0], None, "{mode:?}: black → transparent");
            assert_eq!(merged[1], Some(Rgb(0, 0, 1)), "{mode:?}: non-black kept");
        }
    }

    #[test]
    fn over_keeps_black_opaque_led_off() {
        let cells = vec![Some(Rgb(0, 0, 0)), Some(Rgb(1, 2, 3))];
        let merged = merge_cells(BlendMode::Over, &cells);
        assert_eq!(merged[0], Some(Rgb(0, 0, 0)), "Over black is an explicit LED-off, not transparent");
        assert_eq!(merged[1], Some(Rgb(1, 2, 3)));
    }

    #[test]
    fn fade_ramp_is_instant_at_zero_fade() {
        let mut r = FadeRamp::new(0.0);
        let t = Instant::now();
        // Even on the first call (dt snaps to 0) a zero-fade ramp reaches target.
        assert_eq!(r.advance(t, 1.0, 0.0), 1.0);
        assert_eq!(r.advance(t + Duration::from_millis(1), 0.0, 0.0), 0.0);
    }

    #[test]
    fn fade_ramp_first_call_snaps_dt_to_zero() {
        let mut r = FadeRamp::new(0.0);
        // First advance has no prior instant to measure against: no motion yet.
        assert_eq!(r.advance(Instant::now(), 1.0, 1.0), 0.0);
    }

    #[test]
    fn fade_ramp_is_framerate_independent() {
        // Same wall interval (0.4s) and fade (2s), coarse vs fine stepping.
        // Every dt stays under the 0.25s stall cap so neither path is clamped.
        let t0 = Instant::now();
        let fade = 2.0;

        let mut coarse = FadeRamp::new(0.0);
        coarse.advance(t0, 1.0, fade); // seed (dt = 0)
        coarse.advance(t0 + Duration::from_millis(200), 1.0, fade);
        let coarse_v = coarse.advance(t0 + Duration::from_millis(400), 1.0, fade);

        let mut fine = FadeRamp::new(0.0);
        fine.advance(t0, 1.0, fade); // seed
        for i in 1..=8 {
            fine.advance(t0 + Duration::from_millis(i * 50), 1.0, fade);
        }
        let fine_v = fine.value();

        assert!((coarse_v - fine_v).abs() < 1e-4, "coarse {coarse_v} vs fine {fine_v}");
        assert!((coarse_v - 0.2).abs() < 1e-4, "0.4s of a 2s fade ≈ 0.2");
    }

    #[test]
    fn fade_ramp_monotonic_approach_to_target() {
        let mut r = FadeRamp::new(0.0);
        let t0 = Instant::now();
        r.advance(t0, 1.0, 1.0);
        let mut prev = 0.0;
        for i in 1..=20 {
            let v = r.advance(t0 + Duration::from_millis(i * 100), 1.0, 1.0);
            assert!(v >= prev, "never overshoots downward while rising");
            assert!(v <= 1.0, "never exceeds the target");
            prev = v;
        }
        assert_eq!(prev, 1.0, "reaches the target");
    }

    #[test]
    fn policy_layer_ramp_survives_a_paint_update_mid_fade() {
        // The regression test for the set_content-resets-ramp trap: mutating the
        // shared buffer must NOT restart the fade.
        let policy = PaintPolicy::new();
        policy.update(BlendMode::Over, 100, 1000, None); // 1s fade
        let shared = buf(vec![Some(Rgb(255, 0, 0)); 2]);
        let mut layer = PolicyLayer::new("kbd".into(), 2, Arc::clone(&shared), Arc::clone(&policy), 0.0);

        // Steps stay under the 0.25s stall cap so the ramp advances by real dt.
        let t0 = Instant::now();
        layer.render(t0); // seed ramp (dt = 0, level 0)
        layer.render(t0 + Duration::from_millis(200)); // ~0.2 of the way in
        let mid = layer.alpha();
        assert!(mid > 0.1 && mid < 0.35, "partway through the fade: {mid}");

        // A fresh paint arrives: mutate the SHARED buffer, do not rebuild the layer.
        *shared.lock().unwrap() = vec![Some(Rgb(0, 0, 255)); 2];
        let v = layer.render(t0 + Duration::from_millis(400));
        assert_eq!(v[0], Some(Rgb(0, 0, 255)), "the new frame is shown");
        let after = layer.alpha();
        assert!(after > mid, "the ramp CONTINUED from where it was: {mid} → {after}");
        assert!(after < 1.0, "still mid-fade, not snapped to full");
    }

    #[test]
    fn policy_layer_scope_off_crossfades_out_instead_of_hard_cutting() {
        // The regression test for the allowed-flag hard cut: with a real fade
        // configured, a hands-off toggle must ramp the cells DOWN (still
        // painting at decaying opacity — so the claim readout stays honest)
        // rather than snap to None while alpha() still reports a live layer.
        let policy = PaintPolicy::new();
        policy.update(BlendMode::Over, 100, 1000, None); // 1s fade, all allowed
        let shared = buf(vec![Some(Rgb(255, 0, 0)); 2]);
        let mut layer = PolicyLayer::new("kbd".into(), 2, Arc::clone(&shared), Arc::clone(&policy), 1.0);
        layer.render(Instant::now()); // seed the ramp clock at full level

        // The user toggles this surface hands-off mid-session.
        policy.update(BlendMode::Over, 100, 1000, Some(HashSet::from(["other".to_string()])));
        let t1 = Instant::now() + Duration::from_millis(200);
        let v = layer.render(t1);
        assert_eq!(v[0], Some(Rgb(255, 0, 0)), "cells still paint while fading out");
        let mid = layer.alpha();
        assert!(mid > 0.5 && mid < 1.0, "opacity is decaying, not cut: {mid}");

        // Past the fade window the layer is genuinely gone — render AND alpha agree.
        let mut t = t1;
        for _ in 0..8 {
            t += Duration::from_millis(200); // stay under the ramp's stall cap
            layer.render(t);
        }
        assert_eq!(layer.render(t + Duration::from_millis(200))[0], None, "faded out");
        assert!(layer.alpha() <= MIN_VISIBLE_ALPHA, "claims see it gone too");
    }

    #[test]
    fn policy_layer_disallowed_key_paints_nothing() {
        let policy = PaintPolicy::new();
        policy.update(BlendMode::Over, 100, 0, Some(HashSet::from(["other".to_string()])));
        let shared = buf(vec![Some(Rgb(9, 9, 9)); 2]);
        let mut layer = PolicyLayer::new("kbd".into(), 2, shared, policy, 1.0);
        assert_eq!(layer.render(Instant::now()), vec![None; 2], "scoped-out surface shows nothing");
    }
}
