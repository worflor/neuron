// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! Protocol adapters — codecs at the edge of the kernel.
//!
//! Every adapter is a PURE state machine: bytes/requests in, bytes/responses
//! out, kernel effects through `&mut dyn HostApi`, time through an injected
//! `Instant`. No sockets, no threads, no clocks in here — the I/O pumps that
//! feed these live in the host process shell, and stay dumb. That's what makes
//! capture/replay testing possible: recorded traffic replays deterministically
//! against the same code that runs in production.

pub mod chroma;
pub mod chroma_analyze;
pub mod chroma_shm;
pub mod obs;
pub mod openrgb;
