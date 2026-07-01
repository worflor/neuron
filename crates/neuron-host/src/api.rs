//! The host API seam — the ONE surface adapters talk to.
//!
//! Adapters (Chroma REST, OpenRGB TCP, telemetry, the device writer) never
//! touch the arbiter/bus directly and never see each other. They speak this
//! trait, which has two implementations:
//!
//! - [`crate::Kernel`] implements it synchronously — what unit tests and the
//!   capture/replay harness use, so protocol logic is testable with zero
//!   threads and an injected clock;
//! - the shell's [`crate::shell::HostHandle`] implements it over channels to
//!   the one kernel actor thread — what production adapters hold.
//!
//! Because adapters only know this trait, "run the adapter against a mock"
//! and "run the adapter for real" are the same code path. That is the whole
//! capture/replay strategy.
//!
//! Design notes:
//! - [`LeaseSpec`] instead of raw `Lease`: adapters state INTENT ("15s ttl"),
//!   the kernel computes deadlines — adapters never do time arithmetic.
//! - [`HostApi::next_source`]: SourceIds are issued by the kernel, so two
//!   adapters can never collide on an owner id.
//! - Surface identity metadata (name/kind/grid) lives HERE, not in the
//!   arbiter — the arbiter is pure ownership math over (key, led-count);
//!   what a surface *is* is a host concern.

use std::time::{Duration, Instant};

use crate::arbiter::{Content, LayerId, Rgb, SourceId};
use crate::bus::Value;

/// What kind of thing a surface is — the vocabulary shared by protocol
/// adapters (Chroma device endpoints, OpenRGB device types) and, later, the
/// neuron-core registry bridge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceKind {
    Keyboard,
    Mouse,
    Mousepad,
    Headset,
    Keypad,
    Generic,
}

/// Row-major LED matrix shape, for surfaces that are grids (keyboards).
/// LED index = row * cols + col.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grid {
    pub rows: usize,
    pub cols: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInfo {
    /// Stable device key (later: derived from neuron-core's DevicePath/pid).
    pub key: String,
    /// Human-readable name shown to protocol clients.
    pub name: String,
    pub kind: SurfaceKind,
    pub leds: usize,
    pub grid: Option<Grid>,
}

impl SurfaceInfo {
    /// Convenience for grid surfaces; leds is derived, so the two can't drift.
    pub fn grid(key: &str, name: &str, kind: SurfaceKind, rows: usize, cols: usize) -> Self {
        SurfaceInfo {
            key: key.to_string(),
            name: name.to_string(),
            kind,
            leds: rows * cols,
            grid: Some(Grid { rows, cols }),
        }
    }
}

/// Lease INTENT, as adapters state it. The kernel turns it into a concrete
/// `Lease` with a deadline — adapters never compute deadlines themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseSpec {
    /// Lives until explicitly released (config/base layers, connection-scoped
    /// claims that the adapter releases on disconnect).
    Pinned,
    /// Must be refreshed (heartbeat or content push) within this window or it
    /// auto-drops — the Chroma 15s model. Mandatory for anything session-shaped.
    Ttl(Duration),
}

/// The one surface adapters see. `&mut self` throughout (including reads) so
/// the channel-backed implementation stays trivial.
pub trait HostApi {
    /// Declare (or re-declare — idempotent, claims survive) a surface.
    fn declare(&mut self, info: SurfaceInfo);
    /// Enumerate declared surfaces, stable order (declaration order).
    fn surfaces(&mut self) -> Vec<SurfaceInfo>;
    /// Issue a fresh, process-unique source id for an adapter session.
    fn next_source(&mut self) -> SourceId;
    fn claim(
        &mut self,
        surface: &str,
        owner: SourceId,
        priority: i32,
        lease: LeaseSpec,
        content: Content,
        now: Instant,
    ) -> Option<LayerId>;
    /// Replace a layer's pixels; counts as liveness. `false` = layer gone
    /// (lease lapsed and was swept) — the adapter must re-claim.
    fn set_content(&mut self, id: LayerId, content: Content, now: Instant) -> bool;
    fn refresh(&mut self, id: LayerId, now: Instant) -> bool;
    fn release(&mut self, id: LayerId);
    fn release_owner(&mut self, owner: SourceId);
    fn resolve(&mut self, surface: &str, now: Instant) -> Option<Vec<Option<Rgb>>>;
    fn publish(&mut self, path: &str, value: Value);
}

impl HostApi for crate::Kernel {
    fn declare(&mut self, info: SurfaceInfo) {
        self.arbiter.declare_surface(&info.key, info.leds);
        if let Some(existing) = self.infos.iter_mut().find(|i| i.key == info.key) {
            *existing = info;
        } else {
            self.infos.push(info);
        }
    }

    fn surfaces(&mut self) -> Vec<SurfaceInfo> {
        self.infos.clone()
    }

    fn next_source(&mut self) -> SourceId {
        let id = SourceId(self.next_source);
        self.next_source += 1;
        id
    }

    fn claim(
        &mut self,
        surface: &str,
        owner: SourceId,
        priority: i32,
        lease: LeaseSpec,
        content: Content,
        now: Instant,
    ) -> Option<LayerId> {
        let lease = match lease {
            LeaseSpec::Pinned => crate::arbiter::Lease::Pinned,
            LeaseSpec::Ttl(ttl) => crate::arbiter::Lease::heartbeat(ttl, now),
        };
        self.arbiter.claim(surface, owner, priority, lease, content)
    }

    fn set_content(&mut self, id: LayerId, content: Content, now: Instant) -> bool {
        self.arbiter.set_content(id, content, now)
    }

    fn refresh(&mut self, id: LayerId, now: Instant) -> bool {
        self.arbiter.refresh(id, now)
    }

    fn release(&mut self, id: LayerId) {
        let _ = self.arbiter.release(id);
    }

    fn release_owner(&mut self, owner: SourceId) {
        let _ = self.arbiter.release_owner(owner);
    }

    fn resolve(&mut self, surface: &str, now: Instant) -> Option<Vec<Option<Rgb>>> {
        self.arbiter.resolve(surface, now)
    }

    fn publish(&mut self, path: &str, value: Value) {
        self.bus.publish(path, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbiter::band;
    use crate::Kernel;

    #[test]
    fn kernel_issues_unique_sources_and_zero_is_reserved() {
        let mut k = Kernel::new();
        let a = k.next_source();
        let b = k.next_source();
        assert_ne!(a, b);
        // SourceId(0) is the journal's CONFIG_SOURCE — never issued.
        assert_ne!(a, crate::journal::CONFIG_SOURCE);
        assert_ne!(b, crate::journal::CONFIG_SOURCE);
    }

    #[test]
    fn redeclare_updates_metadata_without_duplicating() {
        let mut k = Kernel::new();
        k.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 6, 22));
        k.declare(SurfaceInfo::grid("kbd", "Board v2", SurfaceKind::Keyboard, 6, 22));
        let s = k.surfaces();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].name, "Board v2");
        assert_eq!(s[0].leds, 132);
    }

    #[test]
    fn ttl_spec_becomes_a_real_expiring_lease() {
        use std::time::{Duration, Instant};
        let mut k = Kernel::new();
        let now = Instant::now();
        k.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 2));
        let owner = k.next_source();
        k.claim(
            "kbd",
            owner,
            band::SESSION,
            LeaseSpec::Ttl(Duration::from_secs(15)),
            Content::Fill(Rgb(1, 2, 3)),
            now,
        )
        .unwrap();
        assert!(k.resolve("kbd", now).unwrap()[0].is_some());
        assert!(k.resolve("kbd", now + Duration::from_secs(16)).unwrap()[0].is_none());
    }
}
