//! The declaration journal — durable state as a replayable seed.
//!
//! The eigenmotion codec's deepest lesson: a rich trajectory reduces to a tiny
//! recurrence seed and reconstructs exactly. Applied to the host: authoritative
//! state is NOT a big serialized heap — it is a short log of declarations
//! (surfaces, named pinned layers) that folds into an identical arbiter on
//! every rebirth. Death is cheap because rebirth is a replay.
//!
//! Only DURABLE facts are journaled. Leased session layers are deliberately
//! absent: a Chroma game's claim must die with its heartbeat, so persisting it
//! would resurrect exactly the stuck-lighting bug the arbiter exists to kill.
//! (Sessions re-claim on reconnect; that's their contract.)
//!
//! Declarations are keyed (surfaces by device key, layers by name), so the log
//! compacts to last-write-wins per key — the on-disk format, when it lands, is
//! append-lines + periodic compaction. In-memory only for now: R&D honesty,
//! the file format arrives with the host process shell.

use std::collections::HashMap;

use crate::arbiter::{band, Arbiter, Content, Lease, SourceId};

/// The journal's own source id for pinned layers it declares on replay. Real
/// runtime sources are issued non-zero ids by the host shell.
pub const CONFIG_SOURCE: SourceId = SourceId(0);

#[derive(Clone, Debug, PartialEq)]
pub enum Decl {
    /// A device surface exists with this many LEDs.
    Surface { key: String, leds: usize },
    /// A named, durable, pinned layer (e.g. the user's base lighting stack).
    /// Names are the durable identity; LayerIds are runtime-only.
    Layer { name: String, surface: String, priority: i32, content: Content },
    /// The named layer no longer exists.
    DropLayer { name: String },
}

pub struct Journal {
    log: Vec<Decl>,
}

impl Journal {
    pub fn new() -> Self {
        Journal { log: Vec::new() }
    }

    pub fn record(&mut self, d: Decl) {
        self.log.push(d);
    }

    pub fn len(&self) -> usize {
        self.log.len()
    }

    pub fn is_empty(&self) -> bool {
        self.log.is_empty()
    }

    /// Fold the log into a fresh arbiter. Replaying the same journal always
    /// yields the same resolve output — the property that makes rebirth safe.
    pub fn replay(&self) -> Arbiter {
        let mut a = Arbiter::new();
        // Surfaces first, so a layer that precedes its surface in the log
        // (possible after careless external edits) still lands.
        for d in &self.log {
            if let Decl::Surface { key, leds } = d {
                a.declare_surface(key, *leds);
            }
        }
        // name -> live LayerId of the current incarnation; a re-declared name
        // supersedes (drop + fresh claim), it never stacks.
        let mut live: HashMap<&str, crate::arbiter::LayerId> = HashMap::new();
        for d in &self.log {
            match d {
                Decl::Surface { .. } => {}
                Decl::Layer { name, surface, priority, content } => {
                    if let Some(old) = live.remove(name.as_str()) {
                        let _ = a.release(old);
                    }
                    if let Some(id) =
                        a.claim(surface, CONFIG_SOURCE, *priority, Lease::Pinned, content.clone())
                    {
                        live.insert(name, id);
                    }
                }
                Decl::DropLayer { name } => {
                    if let Some(old) = live.remove(name.as_str()) {
                        let _ = a.release(old);
                    }
                }
            }
        }
        a
    }

    /// Compact to the minimal equivalent log: the latest Surface per key, the
    /// latest Layer per name (unless later dropped), no dangling drops. The
    /// semantic contract is `replay(compacted) == replay(original)` — proven in
    /// `compaction_preserves_semantics_and_shrinks`.
    pub fn compact(&mut self) {
        let mut surfaces: HashMap<String, usize> = HashMap::new(); // key -> keep idx
        let mut layers: HashMap<String, Option<usize>> = HashMap::new(); // name -> live idx | dropped
        for (i, d) in self.log.iter().enumerate() {
            match d {
                Decl::Surface { key, .. } => {
                    surfaces.insert(key.clone(), i);
                }
                Decl::Layer { name, .. } => {
                    layers.insert(name.clone(), Some(i));
                }
                Decl::DropLayer { name } => {
                    layers.insert(name.clone(), None);
                }
            }
        }
        let mut keep: Vec<usize> = surfaces.into_values().collect();
        keep.extend(layers.into_values().flatten());
        keep.sort_unstable();
        let mut i = 0usize;
        self.log.retain(|_| {
            let k = keep.binary_search(&i).is_ok();
            i += 1;
            k
        });
    }
}

impl Default for Journal {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience for declaring the user's base lighting as a named durable layer.
pub fn base_layer(name: &str, surface: &str, content: Content) -> Decl {
    Decl::Layer {
        name: name.to_string(),
        surface: surface.to_string(),
        priority: band::BASE,
        content,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbiter::Rgb;
    use std::time::Instant;

    fn resolve_all(a: &Arbiter, key: &str) -> Vec<Option<Rgb>> {
        a.resolve(key, Instant::now()).unwrap()
    }

    #[test]
    fn replay_reproduces_state() {
        let mut j = Journal::new();
        j.record(Decl::Surface { key: "kbd".into(), leds: 3 });
        j.record(base_layer("user-stack", "kbd", Content::Fill(Rgb(10, 20, 30))));

        let a1 = j.replay();
        let a2 = j.replay();
        assert_eq!(resolve_all(&a1, "kbd"), resolve_all(&a2, "kbd"));
        assert_eq!(resolve_all(&a1, "kbd"), vec![Some(Rgb(10, 20, 30)); 3]);
    }

    #[test]
    fn named_layer_updates_supersede_not_stack() {
        let mut j = Journal::new();
        j.record(Decl::Surface { key: "kbd".into(), leds: 1 });
        j.record(base_layer("user-stack", "kbd", Content::Fill(Rgb(1, 1, 1))));
        j.record(base_layer("user-stack", "kbd", Content::Fill(Rgb(2, 2, 2))));
        let a = j.replay();
        assert_eq!(resolve_all(&a, "kbd"), vec![Some(Rgb(2, 2, 2))]);
    }

    #[test]
    fn dropped_layers_stay_dropped_through_replay() {
        let mut j = Journal::new();
        j.record(Decl::Surface { key: "kbd".into(), leds: 1 });
        j.record(base_layer("user-stack", "kbd", Content::Fill(Rgb(1, 1, 1))));
        j.record(Decl::DropLayer { name: "user-stack".into() });
        let a = j.replay();
        assert_eq!(resolve_all(&a, "kbd"), vec![None]);
    }

    #[test]
    fn compaction_preserves_semantics_and_shrinks() {
        let mut j = Journal::new();
        j.record(Decl::Surface { key: "kbd".into(), leds: 2 });
        j.record(base_layer("a", "kbd", Content::Fill(Rgb(1, 1, 1))));
        j.record(base_layer("a", "kbd", Content::Fill(Rgb(2, 2, 2))));
        j.record(base_layer("b", "kbd", Content::Fill(Rgb(3, 3, 3))));
        j.record(Decl::DropLayer { name: "b".into() });
        j.record(Decl::Surface { key: "kbd".into(), leds: 3 });

        let before = j.replay();
        let len_before = j.len();
        j.compact();
        let after = j.replay();

        assert!(j.len() < len_before, "compaction must shrink ({} -> {})", len_before, j.len());
        assert_eq!(resolve_all(&before, "kbd"), resolve_all(&after, "kbd"));
        // And the surviving content is the latest write, not the first.
        assert_eq!(resolve_all(&after, "kbd"), vec![Some(Rgb(2, 2, 2)); 3]);
    }

    #[test]
    fn ordering_across_different_names_is_preserved() {
        // Two different names on the same band: journal order decides who is
        // on top (later wins within a band, per arbiter seq). Compaction must
        // not reorder them.
        let mut j = Journal::new();
        j.record(Decl::Surface { key: "kbd".into(), leds: 1 });
        j.record(base_layer("under", "kbd", Content::Fill(Rgb(1, 0, 0))));
        j.record(base_layer("over", "kbd", Content::Fill(Rgb(2, 0, 0))));
        let a = j.replay();
        assert_eq!(resolve_all(&a, "kbd"), vec![Some(Rgb(2, 0, 0))]);
        j.compact();
        let a = j.replay();
        assert_eq!(resolve_all(&a, "kbd"), vec![Some(Rgb(2, 0, 0))]);
    }
}
