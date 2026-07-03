//! neuron-host — the kernel of the protocol host.
//!
//! Every RGB-ecosystem conflict in the wild (Synapse-vs-SignalRGB "bad rave"
//! flicker, lighting frozen on after a game exits, "close the other app before
//! this one can see the device") is the same root bug: multiple sources writing
//! one device with no model of who owns it *right now*, so it's last-writer-wins
//! at the firmware. This crate is the fix, built from first principles:
//!
//! - [`arbiter`] — every source paints through a layer that carries an owner, a
//!   priority, and a **lease**. Resolution is a pure function; lease expiry — not
//!   adapter goodwill — releases a dead session's claim. Teardown is the default
//!   path, not code someone remembered to write.
//! - [`bus`] — named signals (`cs2.health`, `gpu.temp`, `obs.scene`) with
//!   retained last-values and prefix subscriptions: normalize once, bind
//!   anywhere. Generalizes the `controls::INJECT` broadcast pattern.
//! - [`journal`] — durable state is a small replayable declaration log, the
//!   codec lesson (a rich stream reduces to a tiny seed and reconstructs):
//!   rebirth is a replay, so death is cheap.
//! - [`governor`] — restart control as a damped AR(2) system, the same
//!   `z[n] = K·z[n-1] − G·z[n-2]` recurrence as the eigenmotion stack. Spectral
//!   radius < 1 (checked at construction) means a restart storm is impossible by
//!   construction, not by tuning folklore.
//!
//! The kernel owns NO sockets, NO device handles, NO threads. Adapters (Chroma
//! REST, OpenRGB TCP, telemetry listeners) and the single device-writer live
//! outside and talk to it; because it holds no I/O it has almost nothing that
//! *can* crash, and because state lives behind one owner there is no shared
//! mutex to poison.

pub mod adapters;
pub mod api;
pub mod arbiter;
#[cfg(feature = "bridge")]
pub mod bridge;
pub mod bus;
pub mod crypto;
pub mod governor;
pub mod journal;
pub mod net;
pub mod shell;
pub mod writer;
pub mod ws;

use std::collections::HashMap;

use api::SurfaceInfo;
use arbiter::{Arbiter, SourceId};
use bus::Bus;
use journal::Journal;

/// The kernel: one struct owning the three state machines plus surface
/// identity metadata. Thread/actor wiring is the thin [`shell`] — everything
/// interesting is synchronous and testable right here. Adapters talk to it
/// through [`api::HostApi`], which Kernel implements directly.
pub struct Kernel {
    pub arbiter: Arbiter,
    pub bus: Bus,
    pub journal: Journal,
    /// Declared surfaces, in declaration order (adapters enumerate these).
    pub(crate) infos: Vec<SurfaceInfo>,
    /// Next SourceId to issue; 0 is reserved for journal::CONFIG_SOURCE.
    pub(crate) next_source: u64,
    /// Human names for sources ("Overwatch", "openrgb: hass") — advisory
    /// identity for the GUI's ownership truth, set by adapters the moment
    /// they learn who connected. Never authority; never required.
    pub(crate) labels: HashMap<SourceId, String>,
}

impl Kernel {
    pub fn new() -> Self {
        Kernel {
            arbiter: Arbiter::new(),
            bus: Bus::new(),
            journal: Journal::new(),
            infos: Vec::new(),
            next_source: 1,
            labels: HashMap::new(),
        }
    }

    /// Rebirth: fold the journal's declarations into a fresh arbiter. The
    /// returned kernel is byte-for-byte equivalent to the one that recorded the
    /// log — proven by `journal::tests::replay_reproduces_state`.
    pub fn from_journal(journal: Journal) -> Self {
        let arbiter = journal.replay();
        Kernel {
            arbiter,
            bus: Bus::new(),
            journal,
            infos: Vec::new(),
            next_source: 1,
            labels: HashMap::new(),
        }
    }

    /// The advisory name for a source, if an adapter provided one.
    pub fn label_of(&self, owner: SourceId) -> Option<&str> {
        self.labels.get(&owner).map(String::as_str)
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arbiter::{band, Content, Lease, ReleaseWhy, Rgb, SourceId};
    use std::time::{Duration, Instant};

    /// The flagship story, end to end: a Chroma-style game session claims the
    /// keyboard above the user's base lighting, its heartbeat lapses, and the
    /// base shows through again — with the release OBSERVABLE. No flicker
    /// window, no stuck lighting, no code that "remembers" to clean up.
    #[test]
    fn teardown_is_the_default_path() {
        let mut k = Kernel::new();
        let now = Instant::now();
        k.arbiter.declare_surface("kbd", 6);

        // The user's configured lighting: a pinned base layer.
        let base = SourceId(1);
        k.arbiter
            .claim("kbd", base, band::BASE, Lease::Pinned, Content::Fill(Rgb(0, 255, 0)))
            .unwrap();

        // A game connects (Chroma session): leased, 15s heartbeat, higher band.
        let game = SourceId(2);
        let session = k
            .arbiter
            .claim(
                "kbd",
                game,
                band::SESSION,
                Lease::heartbeat(Duration::from_secs(15), now),
                Content::Fill(Rgb(255, 0, 0)),
            )
            .unwrap();

        // While the game heartbeats, it owns the surface.
        let frame = k.arbiter.resolve("kbd", now).unwrap();
        assert!(frame.iter().all(|c| *c == Some(Rgb(255, 0, 0))));

        // Heartbeats keep it alive past the original deadline.
        let t1 = now + Duration::from_secs(10);
        assert!(k.arbiter.refresh(session, t1));
        let t2 = t1 + Duration::from_secs(10);
        let frame = k.arbiter.resolve("kbd", t2).unwrap();
        assert!(frame.iter().all(|c| *c == Some(Rgb(255, 0, 0))));

        // The game exits without saying goodbye (the realistic case). The lease
        // lapses; resolve falls back BEFORE any sweep runs — there is no stale
        // window where a dead session still paints.
        let t3 = t2 + Duration::from_secs(16);
        let frame = k.arbiter.resolve("kbd", t3).unwrap();
        assert!(frame.iter().all(|c| *c == Some(Rgb(0, 255, 0))));

        // And the release is observable: sweep reports who lapsed and why, so
        // the host can log it / notify clients — teardown you can SEE.
        let released = k.arbiter.sweep(t3);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].layer, session);
        assert_eq!(released[0].owner, game);
        assert_eq!(released[0].why, ReleaseWhy::Expired);
    }
}
