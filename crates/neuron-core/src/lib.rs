//! Neuron — open, lightweight control for Razer devices. The anti-Synapse.
//!
//! Design principle: **semantics in code, wiring in data**. Capability *types* live in
//! Rust (typed, testable); every device-specific detail (VID/PIDs, transaction-id,
//! command class/id, control interface) lives in `devices/*.toml`. New device = new TOML.
//!
//! ## The unified spine
//! Every input source — device buttons, hotkeys, [`spellweaving`] (drawn glyphs *and* their
//! degenerate subset, radial flicks), app focus, the mic tap, a held HyperShift layer — is a
//! [`engine::Trigger`]; everything it can do is an [`action::Action`]. The [`engine::Engine`]
//! dispatcher binds the two. Bindings, the spellweaving engine, and the run-daemon are all the
//! same primitive expressed through this spine. Rich macros are `Action::Sequence`; scripted
//! power-macros are `Action::Script` resolved by the [`macros`] engine.
//!
//! ## Spellweaving (and radial, its simplest case)
//! [`spellweaving`] is the held-stroke → action engine: hold a trigger, weave a stroke, release.
//! **Radial is a subset of it**, not a sibling — the degenerate weave where only net direction
//! matters (a sector pick). Glyphs are the rich end. The facets ([`glyph`], [`gesture`],
//! [`radial`], [`cast`]) are re-exported under [`spellweaving`] as the canonical surface.

pub mod action;
pub mod app;
pub mod app_focus;
pub mod audio;
pub mod backup;
pub mod bindings;
pub mod capability;
pub mod capture;
pub mod cast;
pub mod confirm;
pub mod controls;
pub mod curtain;
pub mod device;
pub mod discover;
pub mod effects;
pub mod engine;
pub mod executor;
pub mod feel;
pub mod gesture;
pub mod glyph;
pub mod hook;
pub mod import;
pub mod intent;
pub mod lighting;
pub mod logos;
pub mod macros;
pub mod pocket;
pub mod prof;
pub mod profile;
pub mod protocol;
pub mod radial;
pub mod registry;
pub mod rhythm;
pub mod safety;
pub mod scene;
pub mod shapes;
pub mod spellweaving;
pub mod synapse;
pub mod tone;
pub mod transport;
pub mod twin;
pub mod writes;
