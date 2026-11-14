// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Render-purpose authority for lazy providers. Passive previews cannot extend capture leases.

use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProviderPolicy {
    #[default]
    AuthoritativeLive,
    SharedSnapshotNoStart,
    Representative,
    InjectedDeterministic,
}

thread_local! {
    static POLICY: Cell<ProviderPolicy> = const { Cell::new(ProviderPolicy::AuthoritativeLive) };
    static REPRESENTATIVE_TIME: Cell<Option<f32>> = const { Cell::new(None) };
}

pub struct ProviderGuard(ProviderPolicy, std::marker::PhantomData<std::rc::Rc<()>>);

impl Drop for ProviderGuard {
    fn drop(&mut self) { POLICY.with(|p| p.set(self.0)); }
}

pub struct RepresentativeTimeGuard(Option<f32>, std::marker::PhantomData<std::rc::Rc<()>>);

impl Drop for RepresentativeTimeGuard {
    fn drop(&mut self) { REPRESENTATIVE_TIME.with(|t| t.set(self.0)); }
}

impl ProviderPolicy {
    /// Apply authority only on this render thread, restoring it on unwind or nested exit.
    #[must_use]
    pub fn enter(self) -> ProviderGuard { ProviderGuard(POLICY.with(|p| p.replace(self)), std::marker::PhantomData) }
}

/// Provide a synthetic clock to passive `Representative` previews without permitting provider I/O.
#[must_use]
pub fn enter_representative_time(t_secs: f32) -> RepresentativeTimeGuard {
    RepresentativeTimeGuard(
        REPRESENTATIVE_TIME.with(|t| t.replace(if may_read_provider() { None } else { Some(t_secs) })),
        std::marker::PhantomData,
    )
}

#[must_use]
pub fn representative_time() -> Option<f32> {
    if may_read_provider() { return None; }
    REPRESENTATIVE_TIME.with(Cell::get)
}

#[must_use]
pub fn may_start_provider() -> bool {
    POLICY.with(|p| p.get() == ProviderPolicy::AuthoritativeLive)
}

#[must_use]
pub fn may_read_provider() -> bool {
    POLICY.with(|p| matches!(p.get(), ProviderPolicy::AuthoritativeLive | ProviderPolicy::SharedSnapshotNoStart))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_authority_survives_nested_render_and_unwind() {
        let _passive = ProviderPolicy::SharedSnapshotNoStart.enter();
        assert!(!may_start_provider());
        assert!(may_read_provider());
        assert_eq!(representative_time(), None);
        let _ = std::panic::catch_unwind(|| {
            let _demo = ProviderPolicy::Representative.enter();
            assert!(!may_read_provider());
            let _rt = enter_representative_time(1.5);
            assert_eq!(representative_time(), Some(1.5));
            panic!("render failed");
        });
        assert!(!may_start_provider());
        assert!(may_read_provider());
        assert_eq!(representative_time(), None);
    }
}

