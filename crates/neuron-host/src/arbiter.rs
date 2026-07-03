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
    /// Ambient/passive sources (screen mirror, audio meter) riding above base.
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
/// (journal replay, set-or-claim fallbacks); implementations typically
/// rebuild from their defs.
pub trait LiveContent: Send {
    /// Current cells; `None` = transparent, same contract as [`Content::Cells`].
    fn render(&mut self, now: Instant) -> Vec<Option<Rgb>>;
    fn boxed_clone(&self) -> Box<dyn LiveContent>;
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
    pub fn claims(&self, surface: &str, now: Instant) -> Vec<(SourceId, i32)> {
        let Some(s) = self.surfaces.get(surface) else {
            return Vec::new();
        };
        let mut alive: Vec<&Layer> = s.layers.iter().filter(|l| l.lease.alive(now)).collect();
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
        order.sort_by_key(|&i| std::cmp::Reverse((s.layers[i].priority, s.layers[i].seq)));
        let mut frame: Vec<Option<Rgb>> = vec![None; s.leds];
        // Walk topmost-first, filling only still-unclaimed cells; each Live
        // layer renders exactly once per resolve regardless of LED count.
        for idx in order {
            if frame.iter().all(|c| c.is_some()) {
                break; // fully claimed — lower layers can't contribute
            }
            let rendered; // keeps a Live render alive for the cell loop below
            let cells: &[Option<Rgb>] = match &mut s.layers[idx].content {
                Content::Fill(c) => {
                    let c = *c;
                    for cell in frame.iter_mut().filter(|cell| cell.is_none()) {
                        *cell = Some(c);
                    }
                    continue;
                }
                Content::Cells(v) => v,
                Content::Live(l) => {
                    rendered = l.render(now);
                    &rendered
                }
            };
            for (i, cell) in frame.iter_mut().enumerate() {
                if cell.is_none() {
                    if let Some(c) = cells.get(i).copied().flatten() {
                        *cell = Some(c);
                    }
                }
            }
        }
        Some(frame)
    }
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
}
