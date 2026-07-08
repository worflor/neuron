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
pub mod net;
pub mod paint;
pub mod shell;
pub mod writer;
pub mod ws;

use std::collections::HashMap;

use api::SurfaceInfo;
use arbiter::{Arbiter, SourceId};
use bus::Bus;

/// The kernel: one struct owning the three state machines plus surface
/// identity metadata. Thread/actor wiring is the thin [`shell`] — everything
/// interesting is synchronous and testable right here. Adapters talk to it
/// through [`api::HostApi`], which Kernel implements directly.
pub struct Kernel {
    pub arbiter: Arbiter,
    pub bus: Bus,
    /// Declared surfaces, in declaration order (adapters enumerate these).
    pub(crate) infos: Vec<SurfaceInfo>,
    /// Next SourceId to issue; starts at 1, so 0 is never issued.
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

    /// Two protocols on ONE surface: a Chroma REST game and an OpenRGB client
    /// paint the same board. The later claim composites on top, and a
    /// device-disable on that surface gates BOTH families off so the base
    /// returns — the cross-protocol conflict nothing tested before.
    #[test]
    fn chroma_and_openrgb_conflict_on_one_surface_and_both_obey_device_disable() {
        use crate::adapters::chroma::{ChromaServer, HttpRequest};
        use crate::adapters::openrgb::{ids, packet, OrgbConn};
        use crate::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
        use crate::arbiter::BlendMode;
        use crate::paint::PaintPolicy;
        use std::collections::HashSet;
        use std::sync::Arc;

        let mut k = Kernel::new();
        k.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 2));
        let now = Instant::now();

        // A lit base underneath both sessions.
        let base = k.next_source();
        k.claim("kbd", base, band::BASE, LeaseSpec::Pinned, Content::Fill(Rgb(0, 255, 0)), now)
            .unwrap();

        // Instant Over for both families so the composite is readable at one `now`.
        let chroma_policy = PaintPolicy::opaque();
        let orgb_policy = PaintPolicy::opaque();

        // Chroma REST game paints red first.
        let mut chroma = ChromaServer::with_policy(Arc::clone(&chroma_policy));
        let init = chroma.handle(
            &HttpRequest {
                method: "POST".into(),
                path: "/razer/chromasdk".into(),
                body: br#"{"title":"Game"}"#.to_vec(),
            },
            &mut k,
            now,
        );
        let sid = serde_json::from_str::<serde_json::Value>(&init.body).unwrap()["sessionid"]
            .as_u64()
            .unwrap();
        chroma.handle(
            &HttpRequest {
                method: "PUT".into(),
                path: format!("/razer/chromasdk/sess/{sid}/keyboard"),
                body: br#"{"effect":"CHROMA_STATIC","param":{"color":255}}"#.to_vec(), // BGR red
            },
            &mut k,
            now,
        );
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(255, 0, 0)), "chroma claims first");

        // OpenRGB client paints blue AFTER — the later claim composites on top.
        let mut orgb = OrgbConn::new(&mut k, Arc::clone(&orgb_policy));
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&2u16.to_le_bytes());
        for _ in 0..2 {
            payload.extend_from_slice(&[0, 0, 255, 0]);
        }
        orgb.feed(&packet(0, ids::UPDATELEDS, &payload), &mut k, now);
        assert_eq!(k.resolve("kbd", now).unwrap()[0], Some(Rgb(0, 0, 255)), "later OpenRGB claim wins");

        // Disable this surface on BOTH families: each is independently gated, and
        // with both silent the base shows through.
        let off: HashSet<String> = HashSet::from(["nope".to_string()]);
        chroma_policy.update(BlendMode::Over, 100, 0, Some(off.clone()));
        orgb_policy.update(BlendMode::Over, 100, 0, Some(off));
        assert!(
            k.resolve("kbd", now).unwrap().iter().all(|c| *c == Some(Rgb(0, 255, 0))),
            "device-disable gates BOTH protocols; base returns"
        );
    }
}
