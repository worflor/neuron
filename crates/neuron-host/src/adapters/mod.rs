//! Protocol adapters — codecs at the edge of the kernel.
//!
//! Every adapter is a PURE state machine: bytes/requests in, bytes/responses
//! out, kernel effects through `&mut dyn HostApi`, time through an injected
//! `Instant`. No sockets, no threads, no clocks in here — the I/O pumps that
//! feed these live in the host process shell, and stay dumb. That's what makes
//! capture/replay testing possible: recorded traffic replays deterministically
//! against the same code that runs in production.

pub mod chroma;
pub mod openrgb;
