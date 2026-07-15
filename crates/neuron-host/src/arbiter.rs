//! The ownership arbiter — the traffic controller every RGB stack is missing.
//!
//! A layer here is not just pixels: it carries WHO painted it (owner), HOW MUCH
//! it matters (priority band), and FOR HOW LONG the claim stands without being
//! renewed (lease). For `Fill`/`Cells` layers `resolve` is a pure function of
//! (layers, now) — deterministic, so there is no flicker-by-race. (`Live`
//! content is the honest exception: it renders per resolve and may sample its
//! own clock — the app's compositor base deliberately reads the process-global
//! render epoch so the board stays phase-locked to the GUI preview, trading
//! away replay determinism for that one layer.) An expired lease simply stops
//! winning, so a session that dies mid-game releases the surface with zero
//! cleanup code on the adapter's part. `sweep` then reports the lapse so
//! teardown is *observable*, not silent.
//!
//! Deliberately NOT here: pattern math (that stays in neuron-core's
//! `pattern::Compositor` — the user's whole configured stack becomes the
//! *content* of one pinned BASE layer), device byte order (adapters translate;
//! the kernel speaks RGB), and any notion of time other than the `now` the
//! caller passes in (injected clock = exhaustively testable for the
//! deterministic content kinds).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Token for a connected source: an adapter session, the GUI, a telemetry
/// binding. Issued by the host shell; the kernel only compares them.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct SourceId(pub u64);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct LayerId(pub u64);

/// Priority bands, spaced so whole categories can never collide. Within a band,
/// the LATER claim wins (seq order) — "most recent intent" is the natural tie
/// break and it's what makes two same-band clients behave predictably instead
/// of racing.
pub mod band {
    /// The user's configured lighting — always present, never expires.
    pub const BASE: i32 = 0;
    /// Reserved for ambient/passive sources riding above base — currently
    /// claimed by nothing: screen-mirror and the audio meter run inside the
    /// app's own base-layer stack today, not as an out-of-app claimant. The
    /// band stays as the designed slot for a future out-of-app ambient source.
    pub const AMBIENT: i32 = 1_000;
    /// Live protocol sessions: a Chroma game, an OpenRGB client.
    pub const SESSION: i32 = 10_000;
    /// Explicit user overrides ("hold this color while I hold the key").
    pub const OVERRIDE: i32 = 100_000;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// Animated layer content: rendered fresh on every resolve. This is the seam
/// that lets neuron-app's own Pattern×Spectrum compositor BE the arbiter's
/// base layer — the user's configured animation runs *under* protocol
/// sessions, and returns the instant they release. Implementations do math
/// only (the kernel stays I/O-free); `Send` because layers live on the kernel
/// actor thread. `boxed_clone` exists because layer content must be clonable
/// (set-or-claim fallbacks, rebirth reassert); implementations typically
/// rebuild from their defs.
pub trait LiveContent: Send {
    /// Current cells; `None` = transparent, same contract as [`Content::Cells`].
    fn render(&mut self, now: Instant) -> Vec<Option<Rgb>>;
    fn boxed_clone(&self) -> Box<dyn LiveContent>;
    /// Layer opacity in `[0,1]`; `1.0` = fully opaque (the default, so existing
    /// layers are unaffected). Below 1 the arbiter CROSSFADES this layer's coloured
    /// cells over whatever is beneath instead of hard-replacing them — how a game
    /// layer fades in and out over the user's base lighting. `None` cells stay fully
    /// transparent regardless. Read once per resolve, alongside [`render`](Self::render).
    fn alpha(&self) -> f32 {
        1.0
    }
    /// How this layer's coloured cells COMBINE with what's beneath, before [`alpha`]
    /// opacity is applied (see [`BlendMode`]). `Over` (the default) replaces; the
    /// light-combining modes let a game overlay ADD to the user's lighting rather than
    /// hide it. Read once per resolve alongside [`alpha`](Self::alpha).
    fn blend_mode(&self) -> BlendMode {
        BlendMode::Over
    }
}

/// How a layer's coloured cells combine with the layers beneath, before the layer's
/// [`LiveContent::alpha`] opacity is applied. `Over` is plain compositing (with
/// `alpha < 1`, a linear crossfade); the rest are the classic light-math modes, so a
/// game overlay can *merge* with the user's lighting instead of replacing it — `Screen`
/// and `Add` brighten (the game's light adds on top), `Multiply` tints/darkens.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BlendMode {
    /// Normal compositing — the layer's colour wins (fades in under `alpha`).
    #[default]
    Over,
    /// `1-(1-a)(1-b)` — inverse-multiply; never darkens. The natural "merge" for a game
    /// overlay: its bright keys punch through, its dark background leaves yours intact.
    Screen,
    /// `min(a+b, 1)` — linear dodge; brightest, can clip to white.
    Add,
    /// `a*b` — never brightens; the game tints the user's lighting toward its colour.
    Multiply,
}

impl BlendMode {
    /// Combine one 8-bit channel of `under` with `over` at full opacity. Exact integer
    /// math — no float in the per-LED hot path — each the standard compositing formula
    /// with round-to-nearest on the `/255`.
    #[inline]
    fn channel(self, under: u8, over: u8) -> u8 {
        let (u, o) = (under as u16, over as u16);
        match self {
            BlendMode::Over => over,
            BlendMode::Add => (u + o).min(255) as u8,
            BlendMode::Multiply => ((u * o + 127) / 255) as u8,
            // screen = 255 - (255-u)(255-o)/255 = u + o - u*o/255, always in-range.
            BlendMode::Screen => (u + o - (u * o + 127) / 255) as u8,
        }
    }

    /// Apply the mode across all three channels. `Over` short-circuits to `over` so the
    /// overwhelmingly common opaque/fade path does zero per-channel work.
    #[inline]
    pub fn apply(self, under: Rgb, over: Rgb) -> Rgb {
        match self {
            BlendMode::Over => over,
            _ => Rgb(
                self.channel(under.0, over.0),
                self.channel(under.1, over.1),
                self.channel(under.2, over.2),
            ),
        }
    }

    /// Pack into a single byte, for sharing the live blend policy across threads via an
    /// atomic (see the native-Chroma game layers). Round-trips through [`from_bits`].
    pub fn to_bits(self) -> u8 {
        self as u8
    }

    /// Unpack a [`to_bits`] byte; any unknown value falls back to `Over`.
    pub fn from_bits(bits: u8) -> BlendMode {
        match bits {
            1 => BlendMode::Screen,
            2 => BlendMode::Add,
            3 => BlendMode::Multiply,
            _ => BlendMode::Over,
        }
    }
}

/// What a layer paints. `None` cells are transparent: they neither claim nor
/// color that LED, so lower layers show through per-cell — this is what lets a
/// game light six keys while the user's base keeps the rest.
pub enum Content {
    Fill(Rgb),
    Cells(Vec<Option<Rgb>>),
    /// Animated content (see [`LiveContent`]). Never equal to anything under
    /// `PartialEq` — two animations are only "the same" by construction, and
    /// pretending otherwise would corrupt dedup logic.
    Live(Box<dyn LiveContent>),
}

impl Clone for Content {
    fn clone(&self) -> Content {
        match self {
            Content::Fill(c) => Content::Fill(*c),
            Content::Cells(v) => Content::Cells(v.clone()),
            Content::Live(l) => Content::Live(l.boxed_clone()),
        }
    }
}

impl std::fmt::Debug for Content {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Content::Fill(c) => f.debug_tuple("Fill").field(c).finish(),
            Content::Cells(v) => f.debug_tuple("Cells").field(v).finish(),
            Content::Live(_) => f.write_str("Live(..)"),
        }
    }
}

impl PartialEq for Content {
    fn eq(&self, other: &Content) -> bool {
        match (self, other) {
            (Content::Fill(a), Content::Fill(b)) => a == b,
            (Content::Cells(a), Content::Cells(b)) => a == b,
            _ => false, // Live never equals — see the variant docs
        }
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    struct Blink(u8);
    impl LiveContent for Blink {
        fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
            self.0 = self.0.wrapping_add(1);
            vec![Some(Rgb(self.0, 0, 0)); 2]
        }
        fn boxed_clone(&self) -> Box<dyn LiveContent> {
            Box::new(Blink(self.0))
        }
    }

    #[test]
    fn live_base_animates_under_a_session_and_returns_after_it() {
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 2);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Live(Box::new(Blink(0))))
            .unwrap();
        // The live base advances every resolve.
        let f1 = a.resolve("kbd", t0).unwrap();
        let f2 = a.resolve("kbd", t0).unwrap();
        assert_ne!(f1, f2, "live content must animate across resolves");

        // A session covers it; while covered the base is NOT rendered visibly.
        let sess = a
            .claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(15), t0),
                Content::Fill(Rgb(9, 9, 9)),
            )
            .unwrap();
        assert!(a.resolve("kbd", t0).unwrap().iter().all(|c| *c == Some(Rgb(9, 9, 9))));

        // Session releases: the animation is simply THERE again.
        a.release(sess);
        let f3 = a.resolve("kbd", t0).unwrap();
        assert!(f3[0].is_some());
        assert_ne!(f3.first(), Some(&Some(Rgb(9, 9, 9))));
    }

    #[test]
    fn partially_transparent_session_composes_with_live_base() {
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 2);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Live(Box::new(Blink(0))))
            .unwrap();
        // Session paints ONLY led 0; led 1 shows the live base through.
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Cells(vec![Some(Rgb(9, 9, 9)), None]),
        )
        .unwrap();
        let f = a.resolve("kbd", t0).unwrap();
        assert_eq!(f[0], Some(Rgb(9, 9, 9)));
        assert!(f[1].is_some() && f[1] != Some(Rgb(9, 9, 9)), "hole shows the animated base");
    }

    /// A Live layer at fixed alpha over a known opaque base — the exact per-channel crossfade
    /// the game-fade path depends on (production only exercises it through `ChromaShmLayer`).
    struct Wash {
        color: Rgb,
        alpha: f32,
    }
    impl LiveContent for Wash {
        fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
            vec![Some(self.color); 2]
        }
        fn boxed_clone(&self) -> Box<dyn LiveContent> {
            Box::new(Wash { color: self.color, alpha: self.alpha })
        }
        fn alpha(&self) -> f32 {
            self.alpha
        }
    }

    #[test]
    fn alpha_crossfades_per_channel_over_the_base() {
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 2);
        // Opaque base at (100,100,100); a session washes white (255,255,255) at alpha 0.3.
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(100, 100, 100)))
            .unwrap();
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Wash { color: Rgb(255, 255, 255), alpha: 0.3 })),
        )
        .unwrap();
        // 100*0.7 + 255*0.3 + 0.5 (round) = 70 + 76.5 + 0.5 = 147.
        let f = a.resolve("kbd", t0).unwrap();
        assert_eq!(f, vec![Some(Rgb(147, 147, 147)), Some(Rgb(147, 147, 147))]);
    }

    #[test]
    fn nan_alpha_renders_invisible_never_poisons_the_blend() {
        // `LiveContent::alpha` is an OPEN trait method — a broken adapter can return any non-finite
        // opacity: NaN (a 0.0/0.0 fade ratio) or ±∞ (an overflowed fade calc). `f32::clamp`
        // mishandles them — it passes NaN straight through, and pre-fix the two consumers even
        // DISAGREED on NaN: `is_visible` (NaN > floor = false) called the layer invisible while
        // `resolve`'s floor gate (NaN <= floor = false) composited it, poisoning every blended
        // channel. `sane_alpha` pins ALL THREE to one answer: a non-finite alpha is a broken
        // producer, and a broken producer renders INVISIBLE — the base survives untouched and both
        // consumers agree. +∞ is DELIBERATELY invisible, not clamped-to-opaque: a producer whose
        // opacity math overflowed must not seize the whole board on the strength of a bug (a real
        // layer that wants full opacity returns 1.0). See `sane_alpha`'s doc for the full rationale.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut a = Arbiter::new();
            let t0 = Instant::now();
            a.declare_surface("kbd", 2);
            a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(100, 100, 100)))
                .unwrap();
            a.claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::Pinned,
                Content::Live(Box::new(Wash { color: Rgb(255, 255, 255), alpha: bad })),
            )
            .unwrap();
            let f = a.resolve("kbd", t0).unwrap();
            assert_eq!(
                f,
                vec![Some(Rgb(100, 100, 100)), Some(Rgb(100, 100, 100))],
                "a {bad:?}-alpha layer must contribute nothing — the base wins every cell"
            );
            // And the claims view agrees: `claims` lists only VISIBLE layers, so the broken layer
            // must be absent — never masquerading as the board's owner while contributing nothing.
            let visible = a.claims("kbd", t0);
            assert!(
                visible.iter().any(|(o, _)| *o == SourceId(1)),
                "the base must still read as a visible claimant under {bad:?} alpha: {visible:?}"
            );
            assert!(
                visible.iter().all(|(o, _)| *o != SourceId(2)),
                "the {bad:?}-alpha layer must not read as visible: {visible:?}"
            );
        }
    }

    #[test]
    fn alpha_endpoints_are_pure_base_and_pure_over() {
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 2);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(10, 20, 30)))
            .unwrap();
        // alpha 0.0 → the session contributes nothing; the base shows verbatim.
        let s0 = a
            .claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::Pinned,
                Content::Live(Box::new(Wash { color: Rgb(200, 200, 200), alpha: 0.0 })),
            )
            .unwrap();
        assert!(a.resolve("kbd", t0).unwrap().iter().all(|c| *c == Some(Rgb(10, 20, 30))));
        a.release(s0);
        // alpha 1.0 → hard replace, byte-identical to an opaque cell (no rounding drift).
        a.claim(
            "kbd",
            SourceId(3),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Wash { color: Rgb(200, 201, 202), alpha: 1.0 })),
        )
        .unwrap();
        assert!(a.resolve("kbd", t0).unwrap().iter().all(|c| *c == Some(Rgb(200, 201, 202))));
    }

    #[test]
    fn alpha_over_nothing_fades_up_from_black() {
        // A fading session over an UNLIT led (nothing beneath) blends over black — the
        // documented "black if nothing yet" fallback, so a fade-in still ramps from dark.
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 1);
        a.claim(
            "kbd",
            SourceId(1),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Wash { color: Rgb(255, 0, 0), alpha: 0.5 })),
        )
        .unwrap();
        // 0*0.5 + 255*0.5 + 0.5 = 128 — half-bright red over black, not None.
        assert_eq!(a.resolve("kbd", t0).unwrap(), vec![Some(Rgb(128, 0, 0))]);
    }

    #[test]
    fn a_transparent_live_layer_is_not_reported_as_a_board_claim() {
        // Regression: a game overlay holds a SESSION lease over the user's base but has
        // faded to alpha 0 (dormant before the game connected, or after it left). It paints
        // nothing, so "who controls this board" must read as the base alone — not a phantom
        // foreign owner. This is the honest-status invariant the LIGHTING strip depends on.
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 2);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(10, 20, 30)))
            .unwrap();
        let dormant = a
            .claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::Pinned,
                Content::Live(Box::new(Wash { color: Rgb(255, 255, 255), alpha: 0.0 })),
            )
            .unwrap();
        // The base owns the board; the invisible session claim is absent.
        assert_eq!(a.claims("kbd", t0), vec![(SourceId(1), band::BASE)]);

        // Once it fades in (alpha > 0) it becomes the visible winner and IS reported.
        a.release(dormant);
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Wash { color: Rgb(255, 255, 255), alpha: 0.4 })),
        )
        .unwrap();
        assert_eq!(
            a.claims("kbd", t0),
            vec![(SourceId(2), band::SESSION), (SourceId(1), band::BASE)],
            "a visible session layer is the topmost claim, base beneath it"
        );
    }

    #[test]
    fn blend_mode_bits_round_trip() {
        for m in [BlendMode::Over, BlendMode::Screen, BlendMode::Add, BlendMode::Multiply] {
            assert_eq!(BlendMode::from_bits(m.to_bits()), m);
        }
        // Unknown bytes fall back to the safe default.
        assert_eq!(BlendMode::from_bits(200), BlendMode::Over);
    }

    #[test]
    fn blend_mode_channel_math_is_exact() {
        // The compositing formulas, round-to-nearest on the /255.
        assert_eq!(BlendMode::Over.apply(Rgb(10, 20, 30), Rgb(40, 50, 60)), Rgb(40, 50, 60));
        // add clamps: 200+100 → 255; 10+20 → 30.
        assert_eq!(BlendMode::Add.apply(Rgb(200, 10, 0), Rgb(100, 20, 0)), Rgb(255, 30, 0));
        // multiply: 200*128/255 ≈ 100 (25727/255=100.9→100); 255*x = x; 0*x = 0.
        assert_eq!(BlendMode::Multiply.apply(Rgb(200, 255, 0), Rgb(128, 77, 99)), Rgb(100, 77, 0));
        // screen: 100 s 50 = 130; 255 s x = 255; 0 s x = x.
        assert_eq!(BlendMode::Screen.apply(Rgb(100, 255, 0), Rgb(50, 12, 88)), Rgb(130, 255, 88));
    }

    /// A Live layer with a fixed colour, alpha AND blend mode — the merge path.
    struct Blend {
        color: Rgb,
        alpha: f32,
        mode: BlendMode,
    }
    impl LiveContent for Blend {
        fn render(&mut self, _now: Instant) -> Vec<Option<Rgb>> {
            vec![Some(self.color); 1]
        }
        fn boxed_clone(&self) -> Box<dyn LiveContent> {
            Box::new(Blend { color: self.color, alpha: self.alpha, mode: self.mode })
        }
        fn alpha(&self) -> f32 {
            self.alpha
        }
        fn blend_mode(&self) -> BlendMode {
            self.mode
        }
    }

    #[test]
    fn screen_merges_a_game_over_the_base_at_full_alpha() {
        // The "merge, don't replace" case: a game overlay SCREENS over the user's base, so
        // its light adds instead of hiding — screen(100,50)=130, not a hard replace to 50.
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(100, 100, 100)))
            .unwrap();
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Blend { color: Rgb(50, 50, 50), alpha: 1.0, mode: BlendMode::Screen })),
        )
        .unwrap();
        assert_eq!(a.resolve("kbd", t0).unwrap(), vec![Some(Rgb(130, 130, 130))]);
    }

    #[test]
    fn merge_applies_mode_then_alpha() {
        // Mode and opacity compose in order: SCREEN the game over the base, THEN crossfade
        // by alpha. base 100, screen white → 255, then blend@0.5 → 100*.5+255*.5+.5 = 178.
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(100, 100, 100)))
            .unwrap();
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Live(Box::new(Blend { color: Rgb(255, 255, 255), alpha: 0.5, mode: BlendMode::Screen })),
        )
        .unwrap();
        assert_eq!(a.resolve("kbd", t0).unwrap(), vec![Some(Rgb(178, 178, 178))]);
    }

    #[test]
    fn a_heartbeat_session_layer_expires_and_the_pinned_base_returns() {
        // The game-layer shape: a SESSION layer on a Heartbeat lease that stops being refreshed
        // (the game left, the host stopped proving it) must be swept, leaving the Pinned base —
        // the structural "no stuck lighting" guarantee the SHM layer now rides instead of Pinned.
        let mut a = Arbiter::new();
        let t0 = Instant::now();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(1, 2, 3)))
            .unwrap();
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::heartbeat(Duration::from_secs(4), t0),
            Content::Fill(Rgb(9, 9, 9)),
        )
        .unwrap();
        // While the lease is alive the session wins.
        assert_eq!(a.resolve("kbd", t0).unwrap(), vec![Some(Rgb(9, 9, 9))]);
        // Past the deadline with no refresh, it's gone and the base shows through.
        let later = t0 + Duration::from_secs(5);
        assert_eq!(a.resolve("kbd", later).unwrap(), vec![Some(Rgb(1, 2, 3))]);
    }
}

/// How long a claim stands. `Pinned` is for declarations (the base stack);
/// everything session-shaped MUST be `Heartbeat` — that single rule is the
/// whole "no stuck lighting" guarantee, because liveness then requires the
/// session to keep proving it exists (exactly the Chroma SDK's own 15s model).
#[derive(Clone, Copy, Debug)]
pub enum Lease {
    Pinned,
    Heartbeat { ttl: Duration, deadline: Instant },
}

impl Lease {
    pub fn heartbeat(ttl: Duration, now: Instant) -> Lease {
        Lease::Heartbeat { ttl, deadline: now + ttl }
    }

    fn alive(&self, now: Instant) -> bool {
        match self {
            Lease::Pinned => true,
            Lease::Heartbeat { deadline, .. } => now < *deadline,
        }
    }

    fn refresh(&mut self, now: Instant) {
        if let Lease::Heartbeat { ttl, deadline } = self {
            *deadline = now + *ttl;
        }
    }
}

#[derive(Clone, Debug)]
pub struct Layer {
    pub id: LayerId,
    pub owner: SourceId,
    pub priority: i32,
    /// Global insertion sequence — the within-band tie break (later wins).
    seq: u64,
    pub lease: Lease,
    pub content: Content,
}

/// Below this opacity a `Live` layer paints nothing worth reporting — the same
/// cutoff `resolve`/`ChromaShmLayer` use to skip a fully-faded overlay.
const MIN_VISIBLE_ALPHA: f32 = 0.001;

/// Sanitize a [`LiveContent::alpha`] value at the trust boundary. `alpha()` is an OPEN trait
/// method — any adapter can implement it, and nothing in the type system stops a broken producer
/// returning a non-finite opacity: NaN (a `0.0/0.0` fade ratio), or ±∞ (a fade calc that
/// overflowed). `f32::clamp` mishandles both — it PASSES NaN straight through (both comparisons
/// are false, the same trap `tone::soft_clip` fixed), and NaN comparisons made the two consumers
/// DISAGREE: the visibility gate (`NaN > floor` = false) called the layer invisible while
/// `resolve`'s post-clamp gate (`NaN <= floor` = false) went ahead and COMPOSITED it, poisoning
/// every blended channel.
///
/// The single rule, applied at both sites: **any non-finite alpha is a broken producer, and a
/// broken producer renders INVISIBLE** — NaN and +∞ and −∞ alike, never garbage on the board.
///
/// Note +∞ deliberately becomes invisible, NOT opaque. Clamp arithmetic would order +∞ above 1.0
/// and pin it to full opacity, but a producer whose opacity math overflowed is not *requesting*
/// maximum opacity — it is signalling a bug, and a bug must not let a layer seize the whole board
/// and blank the user's base lighting. A real layer that wants to be opaque returns `1.0`. So this
/// is the same conservative direction as `soft_clip` flushing a broken sample to silence: broken
/// producers contribute nothing until they are fixed.
fn sane_alpha(a: f32) -> f32 {
    if a.is_finite() {
        a.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

impl Layer {
    /// Whether this layer visibly contributes to the board right now. Static
    /// content (`Fill`/`Cells`) always does; a `Live` layer only while its
    /// opacity is above the fade-out floor. Used by [`Arbiter::claims`] so a
    /// dormant, transparent overlay doesn't masquerade as the board's owner.
    fn is_visible(&self) -> bool {
        match &self.content {
            Content::Live(c) => sane_alpha(c.alpha()) > MIN_VISIBLE_ALPHA,
            _ => true,
        }
    }
}

/// Why a layer left the stack — carried on every release so the host can log,
/// notify subscribers, and (for `Expired`) distinguish a crash/vanish from a
/// polite disconnect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReleaseWhy {
    /// Heartbeat lease lapsed — the session died or hung.
    Expired,
    /// Explicit release by id.
    Dropped,
    /// The whole source disconnected and its layers went with it.
    OwnerGone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Released {
    pub surface: String,
    pub layer: LayerId,
    pub owner: SourceId,
    pub why: ReleaseWhy,
}

struct Surface {
    leds: usize,
    layers: Vec<Layer>,
}

/// The arbiter itself: surfaces keyed by device key, each holding its layer
/// stack. Single-owner by design — the host shell wraps this in one actor
/// thread, so there is no lock here to poison.
pub struct Arbiter {
    surfaces: HashMap<String, Surface>,
    next_layer: u64,
    next_seq: u64,
}

impl Arbiter {
    pub fn new() -> Self {
        Arbiter { surfaces: HashMap::new(), next_layer: 1, next_seq: 1 }
    }

    /// Idempotent: re-declaring an existing surface updates its LED count and
    /// keeps its layers (a hotplug re-enumeration must not wipe claims).
    pub fn declare_surface(&mut self, key: &str, leds: usize) {
        self.surfaces
            .entry(key.to_string())
            .and_modify(|s| s.leds = leds)
            .or_insert(Surface { leds, layers: Vec::new() });
    }

    pub fn surface_leds(&self, key: &str) -> Option<usize> {
        self.surfaces.get(key).map(|s| s.leds)
    }

    /// Claim a layer on a surface. `None` if the surface was never declared —
    /// a claim on hardware we don't have is refused honestly, not parked.
    pub fn claim(
        &mut self,
        surface: &str,
        owner: SourceId,
        priority: i32,
        lease: Lease,
        content: Content,
    ) -> Option<LayerId> {
        let s = self.surfaces.get_mut(surface)?;
        let id = LayerId(self.next_layer);
        self.next_layer += 1;
        let seq = self.next_seq;
        self.next_seq += 1;
        s.layers.push(Layer { id, owner, priority, seq, lease, content });
        Some(id)
    }

    /// Heartbeat: push the layer's deadline out by its ttl. `false` if the
    /// layer no longer exists (already swept) — the adapter must re-claim, the
    /// exact semantic the Chroma SDK's session model expects.
    pub fn refresh(&mut self, id: LayerId, now: Instant) -> bool {
        for s in self.surfaces.values_mut() {
            if let Some(l) = s.layers.iter_mut().find(|l| l.id == id) {
                l.lease.refresh(now);
                return true;
            }
        }
        false
    }

    /// Replace a live layer's pixels (a streaming session pushing frames).
    /// Also counts as liveness: a session actively painting is self-evidently
    /// alive, so pushing content refreshes the lease too.
    pub fn set_content(&mut self, id: LayerId, content: Content, now: Instant) -> bool {
        for s in self.surfaces.values_mut() {
            if let Some(l) = s.layers.iter_mut().find(|l| l.id == id) {
                l.content = content;
                l.lease.refresh(now);
                return true;
            }
        }
        false
    }

    /// Explicit release by id.
    pub fn release(&mut self, id: LayerId) -> Option<Released> {
        for (key, s) in self.surfaces.iter_mut() {
            if let Some(pos) = s.layers.iter().position(|l| l.id == id) {
                let l = s.layers.remove(pos);
                return Some(Released {
                    surface: key.clone(),
                    layer: l.id,
                    owner: l.owner,
                    why: ReleaseWhy::Dropped,
                });
            }
        }
        None
    }

    /// A source disconnected: drop every layer it owned, on every surface.
    /// This is what makes "adapter task panicked" safe — the supervisor calls
    /// this once and the source's whole footprint is gone.
    pub fn release_owner(&mut self, owner: SourceId) -> Vec<Released> {
        let mut out = Vec::new();
        for (key, s) in self.surfaces.iter_mut() {
            s.layers.retain(|l| {
                if l.owner == owner {
                    out.push(Released {
                        surface: key.clone(),
                        layer: l.id,
                        owner,
                        why: ReleaseWhy::OwnerGone,
                    });
                    false
                } else {
                    true
                }
            });
        }
        out
    }

    /// The alive claims on a surface, topmost first: `(owner, priority)`.
    /// This is the GUI's "who is controlling this board right now" truth —
    /// lease-filtered at `now`, no rendering, no side effects.
    ///
    /// A claim counts only if it's actually VISIBLE. A `Live` layer that has
    /// faded to transparent (alpha ≈ 0) paints nothing — a game overlay holding
    /// a warm lease before it ever connects, or fading out after the game left —
    /// so it must not read as a foreign owner. Static content always contributes.
    /// Both this and `resolve` gate on the same [`MIN_VISIBLE_ALPHA`] floor. A `Live`
    /// layer's alpha only advances inside `render` (during `resolve`), so this reads the
    /// level from the last resolve — a mid-fade claim can lag the pixels by one tick, but
    /// the endpoints (dormant/invisible vs painting) it must never get wrong are steady
    /// state, and there it agrees with the screen exactly.
    pub fn claims(&self, surface: &str, now: Instant) -> Vec<(SourceId, i32)> {
        let Some(s) = self.surfaces.get(surface) else {
            return Vec::new();
        };
        let mut alive: Vec<&Layer> = s
            .layers
            .iter()
            .filter(|l| l.lease.alive(now) && l.is_visible())
            .collect();
        alive.sort_by_key(|l| std::cmp::Reverse((l.priority, l.seq)));
        alive.into_iter().map(|l| (l.owner, l.priority)).collect()
    }

    /// Prune expired leases and report them. Callable at any cadence — resolve
    /// already ignores expired layers, so sweep frequency affects only how soon
    /// the lapse is *reported*, never what gets painted.
    pub fn sweep(&mut self, now: Instant) -> Vec<Released> {
        let mut out = Vec::new();
        for (key, s) in self.surfaces.iter_mut() {
            s.layers.retain(|l| {
                if l.lease.alive(now) {
                    true
                } else {
                    out.push(Released {
                        surface: key.clone(),
                        layer: l.id,
                        owner: l.owner,
                        why: ReleaseWhy::Expired,
                    });
                    false
                }
            });
        }
        out
    }

    /// The heart: per-LED, the highest-(priority, seq) *alive* layer with an
    /// opaque cell wins. Deterministic in (layers, now) — same inputs, same
    /// frame (Live layers render once per resolve at the given `now`).
    /// `None` cells in the result mean nothing claims that LED at all; the
    /// writer decides the fallback (an all-None frame skips the device write
    /// entirely — the firmware's latched state IS the onboard-first answer).
    /// `&mut` because Live content renders with internal caches; the kernel
    /// is single-owner (actor), so this costs nothing.
    pub fn resolve(&mut self, surface: &str, now: Instant) -> Option<Vec<Option<Rgb>>> {
        let s = self.surfaces.get_mut(surface)?;
        let mut order: Vec<usize> = (0..s.layers.len())
            .filter(|&i| s.layers[i].lease.alive(now))
            .collect();
        // BOTTOM-UP (ascending priority, seq): each layer composites OVER the ones
        // beneath. For opaque layers this is identical to the old topmost-wins fill
        // (the highest overwrites); for a layer with `alpha < 1` it CROSSFADES over
        // the accumulated lower result — the game-fade path. Each Live layer renders
        // exactly once per resolve.
        order.sort_by_key(|&i| (s.layers[i].priority, s.layers[i].seq));
        let mut frame: Vec<Option<Rgb>> = vec![None; s.leds];
        for idx in order {
            let rendered; // keeps a Live render alive for the cell loop below
            let (cells, alpha, mode): (&[Option<Rgb>], f32, BlendMode) =
                match &mut s.layers[idx].content {
                    Content::Fill(c) => {
                        let c = *c;
                        for cell in frame.iter_mut() {
                            *cell = Some(c); // opaque: claims every cell (lower layers gone)
                        }
                        continue;
                    }
                    Content::Cells(v) => (v, 1.0, BlendMode::Over),
                    Content::Live(l) => {
                        // Render first: a fading layer advances its alpha in `render`, so
                        // read alpha/mode AFTER so the blend uses this frame's level.
                        rendered = l.render(now);
                        (&rendered, l.alpha(), l.blend_mode())
                    }
                };
            // `sane_alpha`, not a bare clamp: clamp passes NaN through, and a NaN here would slip
            // past the floor check below (NaN <= x is false) straight into the per-channel blend.
            let alpha = sane_alpha(alpha);
            if alpha <= MIN_VISIBLE_ALPHA {
                continue; // below the visibility floor — contributes nothing this resolve
            }
            for (i, cell) in frame.iter_mut().enumerate() {
                let Some(over) = cells.get(i).copied().flatten() else {
                    continue; // None cell = transparent, lower layer keeps showing
                };
                // Combine the layer's colour with what's beneath (black if nothing yet)
                // per its blend mode, then interpolate by opacity. `Over` at full alpha
                // is a plain replace; `Screen`/`Add` at full alpha is a merge.
                let under = cell.unwrap_or(Rgb(0, 0, 0));
                let target = mode.apply(under, over);
                *cell = Some(if alpha >= 1.0 { target } else { blend(under, target, alpha) });
            }
        }
        Some(frame)
    }
}

/// Per-channel linear crossfade: `under * (1-a) + over * a`. Cheap (the hot path is
/// every LED every frame) and correct for a fade; a game layer at `a=0.3` shows 30 %
/// game over 70 % of the user's base.
fn blend(under: Rgb, over: Rgb, a: f32) -> Rgb {
    let mix = |u: u8, o: u8| (u as f32 * (1.0 - a) + o as f32 * a + 0.5) as u8;
    Rgb(mix(under.0, over.0), mix(under.1, over.1), mix(under.2, over.2))
}

impl Default for Arbiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn transparent_cells_fall_through_per_led() {
        let mut a = Arbiter::new();
        a.declare_surface("kbd", 4);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(0, 0, 255)))
            .unwrap();
        // Session paints only LEDs 1 and 2.
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::Pinned,
            Content::Cells(vec![None, Some(Rgb(255, 0, 0)), Some(Rgb(255, 0, 0)), None]),
        )
        .unwrap();
        let f = a.resolve("kbd", now()).unwrap();
        assert_eq!(
            f,
            vec![
                Some(Rgb(0, 0, 255)),
                Some(Rgb(255, 0, 0)),
                Some(Rgb(255, 0, 0)),
                Some(Rgb(0, 0, 255)),
            ]
        );
    }

    #[test]
    fn later_claim_wins_within_a_band() {
        let mut a = Arbiter::new();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::SESSION, Lease::Pinned, Content::Fill(Rgb(1, 0, 0)))
            .unwrap();
        a.claim("kbd", SourceId(2), band::SESSION, Lease::Pinned, Content::Fill(Rgb(2, 0, 0)))
            .unwrap();
        assert_eq!(a.resolve("kbd", now()).unwrap()[0], Some(Rgb(2, 0, 0)));
    }

    #[test]
    fn higher_band_wins_regardless_of_arrival_order() {
        let mut a = Arbiter::new();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::OVERRIDE, Lease::Pinned, Content::Fill(Rgb(9, 0, 0)))
            .unwrap();
        a.claim("kbd", SourceId(2), band::SESSION, Lease::Pinned, Content::Fill(Rgb(2, 0, 0)))
            .unwrap();
        assert_eq!(a.resolve("kbd", now()).unwrap()[0], Some(Rgb(9, 0, 0)));
    }

    #[test]
    fn expired_lease_stops_winning_before_any_sweep() {
        let mut a = Arbiter::new();
        let t0 = now();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(0, 255, 0)))
            .unwrap();
        a.claim(
            "kbd",
            SourceId(2),
            band::SESSION,
            Lease::heartbeat(Duration::from_secs(15), t0),
            Content::Fill(Rgb(255, 0, 0)),
        )
        .unwrap();
        // One second before the deadline the session still owns the LED…
        let before = t0 + Duration::from_secs(14);
        assert_eq!(a.resolve("kbd", before).unwrap()[0], Some(Rgb(255, 0, 0)));
        // …and one second after, base shows through — with NO sweep having run.
        // There is no window where a dead session keeps painting.
        let after = t0 + Duration::from_secs(16);
        assert_eq!(a.resolve("kbd", after).unwrap()[0], Some(Rgb(0, 255, 0)));
    }

    #[test]
    fn refresh_extends_and_missing_refresh_reports_honestly() {
        let mut a = Arbiter::new();
        let t0 = now();
        a.declare_surface("kbd", 1);
        let id = a
            .claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(15), t0),
                Content::Fill(Rgb(255, 0, 0)),
            )
            .unwrap();
        let t1 = t0 + Duration::from_secs(10);
        assert!(a.refresh(id, t1));
        // Alive at t0+24 (10 + fresh 15s lease)…
        assert_eq!(
            a.resolve("kbd", t1 + Duration::from_secs(14)).unwrap()[0],
            Some(Rgb(255, 0, 0))
        );
        // Sweep after lapse removes it; a late heartbeat is then refused, which
        // tells the adapter to re-claim instead of silently pretending.
        let t2 = t1 + Duration::from_secs(20);
        let gone = a.sweep(t2);
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].why, ReleaseWhy::Expired);
        assert!(!a.refresh(id, t2));
    }

    #[test]
    fn pushing_content_counts_as_liveness() {
        let mut a = Arbiter::new();
        let t0 = now();
        a.declare_surface("kbd", 1);
        let id = a
            .claim(
                "kbd",
                SourceId(2),
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(15), t0),
                Content::Fill(Rgb(255, 0, 0)),
            )
            .unwrap();
        // A stream of frames, each inside the window, keeps the lease alive
        // without a single explicit heartbeat call.
        let mut t = t0;
        for i in 0..10 {
            t += Duration::from_secs(10);
            assert!(a.set_content(id, Content::Fill(Rgb(i, i, i)), t));
        }
        assert_eq!(a.resolve("kbd", t).unwrap()[0], Some(Rgb(9, 9, 9)));
    }

    #[test]
    fn release_owner_drops_the_full_footprint() {
        let mut a = Arbiter::new();
        a.declare_surface("kbd", 1);
        a.declare_surface("mouse", 1);
        let owner = SourceId(7);
        a.claim("kbd", owner, band::SESSION, Lease::Pinned, Content::Fill(Rgb(1, 1, 1))).unwrap();
        a.claim("mouse", owner, band::SESSION, Lease::Pinned, Content::Fill(Rgb(1, 1, 1)))
            .unwrap();
        a.claim("kbd", SourceId(8), band::AMBIENT, Lease::Pinned, Content::Fill(Rgb(2, 2, 2)))
            .unwrap();
        let gone = a.release_owner(owner);
        assert_eq!(gone.len(), 2);
        assert!(gone.iter().all(|r| r.owner == owner && r.why == ReleaseWhy::OwnerGone));
        // The unrelated source is untouched.
        assert_eq!(a.resolve("kbd", now()).unwrap()[0], Some(Rgb(2, 2, 2)));
    }

    #[test]
    fn redeclaring_a_surface_preserves_claims() {
        let mut a = Arbiter::new();
        a.declare_surface("kbd", 4);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(5, 5, 5)))
            .unwrap();
        // Hotplug re-enumeration re-declares; the claim must survive.
        a.declare_surface("kbd", 6);
        assert_eq!(a.surface_leds("kbd"), Some(6));
        let f = a.resolve("kbd", now()).unwrap();
        assert_eq!(f.len(), 6);
        assert!(f.iter().all(|c| *c == Some(Rgb(5, 5, 5))));
    }

    #[test]
    fn claims_on_undeclared_surfaces_are_refused() {
        let mut a = Arbiter::new();
        assert!(a
            .claim("ghost", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(0, 0, 0)))
            .is_none());
        assert!(a.resolve("ghost", now()).is_none());
    }

    // ── TASK 3(a/b): clock-leap laws ────────────────────────────────────────

    #[test]
    fn wake_storm_after_a_long_sleep_expires_everything_expirable_in_one_sweep() {
        // A laptop wakes 8 hours later: ONE big `AdvanceClock` step, not a
        // stream of small ticks. A single sweep must expire everything that's
        // due, without panicking; resolve degrades to base; a fresh claim
        // right after the wake works exactly as normal.
        let mut a = Arbiter::new();
        let t0 = now();
        a.declare_surface("kbd", 1);
        a.claim("kbd", SourceId(1), band::BASE, Lease::Pinned, Content::Fill(Rgb(1, 2, 3))).unwrap();
        // Staggered TTLs, all short next to an 8h jump.
        for (owner, secs) in [(2u64, 5), (3, 30), (4, 3600)] {
            a.claim(
                "kbd",
                SourceId(owner),
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(secs), t0),
                Content::Fill(Rgb(9, 9, 9)),
            )
            .unwrap();
        }
        let wake = t0 + Duration::from_secs(8 * 3600);
        let released = a.sweep(wake);
        assert_eq!(released.len(), 3, "every heartbeat claim must lapse across an 8h jump");
        assert!(released.iter().all(|r| r.why == ReleaseWhy::Expired));
        assert_eq!(a.resolve("kbd", wake).unwrap()[0], Some(Rgb(1, 2, 3)), "degrades to base");
        // The wake-storm law: a claim right after the jump is unaffected.
        assert!(a
            .claim(
                "kbd",
                SourceId(5),
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(15), wake),
                Content::Fill(Rgb(5, 5, 5)),
            )
            .is_some());
        assert_eq!(a.resolve("kbd", wake).unwrap()[0], Some(Rgb(5, 5, 5)));
    }

    #[test]
    fn sweep_is_idempotent_across_a_zero_length_leap() {
        // `now` here is `Instant` — monotonic by construction (a real
        // `Instant::now()` value-stream never regresses), and the module docs
        // ("no notion of time other than the `now` the caller passes in")
        // don't claim to handle a caller that violates that. A genuine
        // BACKWARD leap is therefore not a scenario this clock type can
        // represent in good faith; the representable degenerate case is a
        // leap of exactly ZERO — two sweeps at the identical instant must be
        // idempotent (the second finds nothing new to report).
        let mut a = Arbiter::new();
        let t0 = now();
        a.declare_surface("kbd", 1);
        a.claim(
            "kbd",
            SourceId(1),
            band::SESSION,
            Lease::heartbeat(Duration::from_millis(1), t0),
            Content::Fill(Rgb(1, 1, 1)),
        )
        .unwrap();
        let later = t0 + Duration::from_secs(1);
        let first = a.sweep(later);
        assert_eq!(first.len(), 1);
        let second = a.sweep(later); // identical instant, no clock movement at all
        assert!(second.is_empty(), "a repeated sweep at the identical instant must be a no-op");
    }
}

/// TASK 1 — reference-model stateful property test.
///
/// A naive model of the arbiter's own DOCUMENTED precedence rule (see the doc
/// comments above `Arbiter::resolve`/`claims`): higher `band` wins; within a
/// band the LATER claim (higher `seq`) wins; an expired lease never wins, and
/// that's checked LIVE — no sweep required (`expired_lease_stops_winning_before_any_sweep`
/// pins exactly this). One subtlety the naive "refresh only works while alive"
/// intuition gets WRONG, discovered by reading `Arbiter::refresh`/`set_content`
/// closely: neither checks lease liveness at all, only PHYSICAL presence — a
/// heartbeat that arrives just after its own deadline but before the next
/// `sweep` still succeeds and resurrects the claim. The model mirrors that
/// real (if surprising) behaviour rather than the simpler wrong rule, or every
/// case would falsely report as a divergence.
#[cfg(test)]
mod model_props {
    use super::*;
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    const SURFACES: [&str; 2] = ["s0", "s1"];
    const BANDS: [i32; 4] = [band::BASE, band::AMBIENT, band::SESSION, band::OVERRIDE];

    #[derive(Clone, Debug)]
    enum ModelOp {
        Claim { owner: u8, surface: u8, band_idx: u8, ttl_ms: Option<u64> },
        Refresh { target: usize },
        SetContent { target: usize },
        Release { target: usize },
        ReleaseOwner { owner: u8 },
        Sweep,
        AdvanceClock { ms: u64 },
    }

    fn any_op() -> impl Strategy<Value = ModelOp> {
        prop_oneof![
            3 => (0u8..4, 0u8..2, 0u8..4, prop::option::of(0u64..5000))
                .prop_map(|(owner, surface, band_idx, ttl_ms)| ModelOp::Claim {
                    owner,
                    surface,
                    band_idx,
                    ttl_ms,
                }),
            2 => (0usize..64).prop_map(|target| ModelOp::Refresh { target }),
            2 => (0usize..64).prop_map(|target| ModelOp::SetContent { target }),
            2 => (0usize..64).prop_map(|target| ModelOp::Release { target }),
            1 => (0u8..4).prop_map(|owner| ModelOp::ReleaseOwner { owner }),
            1 => Just(ModelOp::Sweep),
            2 => (0u64..3000).prop_map(|ms| ModelOp::AdvanceClock { ms }),
        ]
    }

    /// One tracked claim, model-side. `present` = still physically in the
    /// arbiter's layer vec (false once Released / ReleaseOwner'd / swept) —
    /// deliberately distinct from lease liveness, which is a pure function of
    /// `now_ms` (see `model_winner`).
    struct Slot {
        real_id: LayerId,
        owner: u8,
        surface: u8,
        band: i32,
        seq: u64,
        ttl_ms: Option<u64>, // None = Pinned
        deadline_ms: u64,
        present: bool,
    }

    /// The reference model itself: resolve by the documented precedence rule,
    /// over whatever is currently `present` and lease-alive at `now_ms`.
    fn model_winner(slots: &[Slot], surface: u8, now_ms: u64) -> Option<u8> {
        slots
            .iter()
            .filter(|s| s.present && s.surface == surface && s.ttl_ms.map_or(true, |_| now_ms < s.deadline_ms))
            .max_by_key(|s| (s.band, s.seq))
            .map(|s| s.owner)
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

        #[test]
        fn arbiter_matches_the_reference_model(ops in prop::collection::vec(any_op(), 0..64)) {
            let mut a = Arbiter::new();
            for s in SURFACES { a.declare_surface(s, 1); }
            let t0 = Instant::now();
            let mut slots: Vec<Slot> = Vec::new();
            let mut now_ms: u64 = 0;
            let mut next_seq: u64 = 1;

            for op in &ops {
                let now = t0 + Duration::from_millis(now_ms);
                match op.clone() {
                    ModelOp::Claim { owner, surface, band_idx, ttl_ms } => {
                        let band = BANDS[band_idx as usize];
                        let lease = match ttl_ms {
                            Some(ms) => Lease::heartbeat(Duration::from_millis(ms), now),
                            None => Lease::Pinned,
                        };
                        let content = Content::Fill(Rgb(owner, 0, 0));
                        let id = a
                            .claim(SURFACES[surface as usize], SourceId(owner as u64), band, lease, content)
                            .expect("surface pre-declared, claim must succeed");
                        slots.push(Slot {
                            real_id: id,
                            owner,
                            surface,
                            band,
                            seq: next_seq,
                            ttl_ms,
                            deadline_ms: now_ms + ttl_ms.unwrap_or(0),
                            present: true,
                        });
                        next_seq += 1;
                    }
                    ModelOp::Refresh { target } => {
                        if !slots.is_empty() {
                            let idx = target % slots.len();
                            if slots[idx].present {
                                let ok = a.refresh(slots[idx].real_id, now);
                                prop_assert!(ok, "refresh must succeed while the layer is physically present, regardless of lease liveness");
                                if let Some(ms) = slots[idx].ttl_ms {
                                    slots[idx].deadline_ms = now_ms + ms;
                                }
                            }
                        }
                    }
                    ModelOp::SetContent { target } => {
                        if !slots.is_empty() {
                            let idx = target % slots.len();
                            if slots[idx].present {
                                let owner = slots[idx].owner;
                                let ok = a.set_content(slots[idx].real_id, Content::Fill(Rgb(owner, 0, 0)), now);
                                prop_assert!(ok, "set_content must succeed while the layer is physically present");
                                if let Some(ms) = slots[idx].ttl_ms {
                                    slots[idx].deadline_ms = now_ms + ms;
                                }
                            }
                        }
                    }
                    ModelOp::Release { target } => {
                        if !slots.is_empty() {
                            let idx = target % slots.len();
                            if slots[idx].present {
                                let released = a.release(slots[idx].real_id);
                                prop_assert!(released.is_some());
                                slots[idx].present = false;
                            }
                        }
                    }
                    ModelOp::ReleaseOwner { owner } => {
                        let expected = slots.iter().filter(|s| s.present && s.owner == owner).count();
                        let released = a.release_owner(SourceId(owner as u64));
                        prop_assert_eq!(released.len(), expected);
                        for s in slots.iter_mut() {
                            if s.present && s.owner == owner {
                                s.present = false;
                            }
                        }
                    }
                    ModelOp::Sweep => {
                        let expected = slots
                            .iter()
                            .filter(|s| s.present && s.ttl_ms.map_or(false, |_| now_ms >= s.deadline_ms))
                            .count();
                        let released = a.sweep(now);
                        prop_assert_eq!(released.len(), expected);
                        for s in slots.iter_mut() {
                            if s.present && s.ttl_ms.map_or(false, |_| now_ms >= s.deadline_ms) {
                                s.present = false;
                            }
                        }
                    }
                    ModelOp::AdvanceClock { ms } => {
                        now_ms += ms;
                    }
                }

                let now = t0 + Duration::from_millis(now_ms);
                for (i, surface) in SURFACES.iter().enumerate() {
                    let expected = model_winner(&slots, i as u8, now_ms);
                    let frame = a.resolve(surface, now).expect("declared surface");
                    let actual = frame[0].map(|Rgb(o, _, _)| o);
                    prop_assert_eq!(actual, expected, "surface {} mismatch after {:?} at now_ms={}", surface, op, now_ms);
                }
            }
        }
    }
}
