// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The signal bus — normalize once, bind anywhere.
//!
//! Adapters publish named signals (`cs2.health`, `gpu.temp`, `obs.scene`,
//! `race.rpm`); the compositor, the macro engine, and other adapters subscribe
//! by prefix. Two properties carry all the correctness:
//!
//! - **Retained last-values** (the MQTT lesson): a subscriber that arrives late
//!   is immediately told the current truth for every path it cares about — no
//!   "wrong until the next event" window, which is exactly the State half of
//!   the four-question bar.
//! - **Fan-out with dead-subscriber pruning**: the multi-consumer generalization
//!   of `controls::INJECT` (today's only broadcast in the codebase); a receiver
//!   that went away is dropped on the next publish, never blocks anyone.
//!
//! Matching is segment-aware: subscribing to `cs2` matches `cs2.health` and
//! `cs2` itself, but never `cs2x.health`. Empty prefix subscribes to everything.
//!
//! std `mpsc` channels are unbounded; the kernel trusts its in-process
//! subscribers (compositor, macro engine) to drain. Network-facing adapters
//! must put their OWN bounded queue between the bus and the socket — backpressure
//! belongs at the edge that can actually shed load, not in the kernel.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Signal {
    pub path: String,
    pub value: Value,
}

pub struct Bus {
    retained: HashMap<String, Value>,
    subs: Vec<(String, Sender<Signal>)>,
}

/// `prefix` matches `path` on whole dotted segments only.
fn matches(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() || prefix == path {
        return true;
    }
    path.len() > prefix.len()
        && path.starts_with(prefix)
        && path.as_bytes()[prefix.len()] == b'.'
}

impl Bus {
    pub fn new() -> Self {
        Bus { retained: HashMap::new(), subs: Vec::new() }
    }

    /// Publish a signal: retain it, then fan out to every live subscriber whose
    /// prefix matches. Subscribers whose receiver is gone are pruned here — the
    /// bus never accumulates corpses.
    pub fn publish(&mut self, path: &str, value: Value) {
        self.retained.insert(path.to_string(), value.clone());
        let sig = Signal { path: path.to_string(), value };
        self.subs.retain(|(prefix, tx)| {
            if !matches(prefix, path) {
                return true; // not interested — keep, untouched
            }
            tx.send(sig.clone()).is_ok()
        });
    }

    /// Subscribe to a dotted prefix. The current retained value of every
    /// matching path is delivered immediately (sorted by path, so snapshot
    /// order is deterministic), then live signals follow.
    pub fn subscribe(&mut self, prefix: &str) -> Receiver<Signal> {
        let (tx, rx) = channel();
        let mut snapshot: Vec<(&String, &Value)> =
            self.retained.iter().filter(|(p, _)| matches(prefix, p)).collect();
        snapshot.sort_by_key(|(p, _)| p.as_str());
        for (p, v) in snapshot {
            let _ = tx.send(Signal { path: p.clone(), value: v.clone() });
        }
        self.subs.push((prefix.to_string(), tx));
        rx
    }

    /// Point read of the retained truth — for pull-style consumers (a readout
    /// pattern sampling `gpu.temp` once per frame) that don't want a channel.
    pub fn get(&self, path: &str) -> Option<&Value> {
        self.retained.get(path)
    }

    pub fn subscriber_count(&self) -> usize {
        self.subs.len()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_segment_aware() {
        assert!(matches("", "anything.at.all"));
        assert!(matches("cs2", "cs2"));
        assert!(matches("cs2", "cs2.health"));
        assert!(matches("cs2.player", "cs2.player.health"));
        assert!(!matches("cs2", "cs2x.health"));
        assert!(!matches("cs2.health", "cs2"));
    }

    #[test]
    fn late_subscriber_gets_the_retained_truth_first() {
        let mut b = Bus::new();
        b.publish("cs2.health", Value::Int(87));
        b.publish("cs2.armor", Value::Int(50));
        b.publish("gpu.temp", Value::Float(61.5));

        let rx = b.subscribe("cs2");
        // Snapshot arrives immediately, deterministic order, and does NOT
        // include the non-matching path.
        assert_eq!(rx.try_recv().unwrap(), Signal { path: "cs2.armor".into(), value: Value::Int(50) });
        assert_eq!(rx.try_recv().unwrap(), Signal { path: "cs2.health".into(), value: Value::Int(87) });
        assert!(rx.try_recv().is_err());

        // Then live updates flow.
        b.publish("cs2.health", Value::Int(12));
        assert_eq!(rx.try_recv().unwrap(), Signal { path: "cs2.health".into(), value: Value::Int(12) });
    }

    #[test]
    fn fan_out_reaches_every_matching_subscriber() {
        let mut b = Bus::new();
        let all = b.subscribe("");
        let cs2 = b.subscribe("cs2");
        let gpu = b.subscribe("gpu");
        b.publish("cs2.health", Value::Int(1));
        assert!(all.try_recv().is_ok());
        assert!(cs2.try_recv().is_ok());
        assert!(gpu.try_recv().is_err());
    }

    #[test]
    fn dead_subscribers_are_pruned_on_publish() {
        let mut b = Bus::new();
        let rx = b.subscribe("cs2");
        assert_eq!(b.subscriber_count(), 1);
        drop(rx);
        // First matching publish discovers the corpse and removes it.
        b.publish("cs2.health", Value::Int(1));
        assert_eq!(b.subscriber_count(), 0);
    }

    #[test]
    fn retained_point_reads_track_latest() {
        let mut b = Bus::new();
        assert!(b.get("race.rpm").is_none());
        b.publish("race.rpm", Value::Int(7200));
        b.publish("race.rpm", Value::Int(8100));
        assert_eq!(b.get("race.rpm"), Some(&Value::Int(8100)));
    }
}
