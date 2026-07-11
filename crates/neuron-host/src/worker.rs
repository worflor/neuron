//! The named-thread primitive for neuron-host's lifecycle threads (protocol servers, per-socket
//! connection handlers, the OBS bridge, the paced writer, the SHM arbiter). Every one of these is
//! OWNED by a struct that `.join()`s its handle on Drop/stop, so they need the JoinHandle back —
//! the fire-and-forget helpers in neuron-core can't express that. This is a local, pure-`std`
//! mirror of `neuron::worker::spawn_named`: the host kernel keeps `neuron` an OPTIONAL dependency
//! (see this crate's Cargo.toml) and must build without it, so it cannot reach across for the
//! primitive. Routing every raw thread creation through this one file lets the `conventions` test
//! stay a strict allowlist (this file + neuron-core's `worker.rs`) with no per-site markers.

/// Spawn a NAMED worker and return its `JoinHandle`. The name is mandatory (per-thread CPU
/// attribution); the caller owns the `JoinHandle` and joins it on teardown. An `Err` is a spawn
/// refusal the owner must tolerate (the feature is simply unavailable — no partial state to
/// unwind, since these threads hold their own state and publish nothing before running).
pub fn spawn_named<T, W>(name: &str, work: W) -> std::io::Result<std::thread::JoinHandle<T>>
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    std::thread::Builder::new().name(name.to_string()).spawn(work)
}
