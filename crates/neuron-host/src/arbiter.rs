//! The ownership arbiter — the traffic controller every RGB stack is missing.
//!
//! A layer here is not just pixels: it carries WHO painted it (owner), HOW MUCH
//! it matters (priority band), and FOR HOW LONG the claim stands without being
//! renewed (lease). `resolve` is a pure function of (layers, now) — deterministic,
//! so there is no flicker-by-race — and an expired lease simply stops winning,
//! so a session that dies mid-game releases the surface with zero cleanup code
//! on the adapter's part. `sweep` then reports the lapse so teardown is
//! *observable*, not silent.
//!
//! Deliberately NOT here: pattern math (that stays in neuron-core's
//! `pattern::Compositor` — the user's whole configured stack becomes the
//! *content* of one pinned BASE layer), device byte order (adapters translate;
//! the kernel speaks RGB), and any notion of time other than the `now` the
//! caller passes in (injected clock = exhaustively testable).

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

/// What a layer paints. `None` cells are transparent: they neither claim nor
/// color that LED, so lower layers show through per-cell — this is what lets a
/// game light six keys while the user's base keeps the rest.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Fill(Rgb),
    Cells(Vec<Option<Rgb>>),
}

impl Content {
    fn at(&self, i: usize) -> Option<Rgb> {
        match self {
            Content::Fill(c) => Some(*c),
            Content::Cells(v) => v.get(i).copied().flatten(),
        }
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
    /// opaque cell wins. Pure function of (layers, now) — same inputs, same
    /// frame, every time. `None` cells in the result mean nothing claims that
    /// LED at all; the writer decides the fallback (base black, or leave the
    /// firmware's latched state alone — the onboard-first answer).
    pub fn resolve(&self, surface: &str, now: Instant) -> Option<Vec<Option<Rgb>>> {
        let s = self.surfaces.get(surface)?;
        let mut order: Vec<&Layer> = s.layers.iter().filter(|l| l.lease.alive(now)).collect();
        order.sort_by_key(|l| std::cmp::Reverse((l.priority, l.seq)));
        let mut frame = vec![None; s.leds];
        for (i, cell) in frame.iter_mut().enumerate() {
            for l in &order {
                if let Some(c) = l.content.at(i) {
                    *cell = Some(c);
                    break;
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
