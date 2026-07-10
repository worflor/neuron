//! neuron-testkit — instruments for testing neuron against RECORDED REALITY instead of
//! hand-imagined mocks.
//!
//! The doctrine (see docs/TDD.md §5.7 and §9): every belief a test bakes in should be
//! traceable to an observation. So the kit is built from recordings taken through neuron's
//! own seams — the Chroma SHM sections a real game painted, the HID conversations a real
//! device answered — with models fitted to those recordings, and generators that only
//! explore the space the real protocol grammar admits.
//!
//! Today: [`tape`] — the ChromaTape format (a sampled recording of the native Chroma
//! shared-memory sections while a real game paints) and its decoder. The reference tape in
//! `testdata/tapes/` was recorded from Overwatch on 2026-07-10; its measured truths
//! (≈10 fps steady cadence, three-buffer lockstep fanout, 4-byte pixels at a fixed array
//! offset) are pinned by this crate's tests so the format and the facts cannot drift apart.

pub mod tape;
