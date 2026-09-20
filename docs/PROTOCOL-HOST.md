# Neuron protocol host

> Written with LLM assistance. Verify behavior and wire details against the source.

The protocol host lets games and local tools drive lighting through Neuron while
Neuron's own profile lighting remains the base layer. It also connects to OBS
through obs-websocket. The implementation lives in
[`crates/neuron-host`](../crates/neuron-host/src/lib.rs). The app starts it when
SYSTEM → CONNECTIONS is enabled; that switch is off by default. `NEURON_HOST`
overrides the preference for development.

This document describes the current design and its limits. For overall feature
maturity, see [STATUS.md](STATUS.md). The code is the source of truth for wire
details and current behavior.

## What runs today

| Component | Role | Current limit |
|---|---|---|
| [Arbiter](../crates/neuron-host/src/arbiter.rs) | Resolves competing lighting layers by surface, priority and lease. | Ownership and teardown have tests, but need more live use across games and devices. |
| [Signal bus](../crates/neuron-host/src/bus.rs) | Carries named, retained values to prefix subscribers. | Only implemented producers and consumers have live behavior. A signal name alone does not imply a game integration. |
| [Host shell](../crates/neuron-host/src/shell.rs) | Runs the kernel on one thread, sweeps expired leases and restarts it after a contained fault. | Owners must reassert live content after a restart. There is no durable replay of live layers. |
| [Device writer](../crates/neuron-host/src/writer.rs) | Serializes resolved lighting frames per surface, with frame deduplication and refresh. | Real device behavior still depends on the connected hardware and its transport. |
| [Chroma REST](../crates/neuron-host/src/adapters/chroma.rs) | Serves local Chroma SDK requests on port 54235. | Richer interpretation and game-by-game compatibility need further checks. |
| [Chroma shared memory](../crates/neuron-host/src/adapters/chroma_shm.rs) | Accepts the native shared-memory path used by some games on Windows. | Creating its global objects needs `SeCreateGlobalPrivilege`; without it, the REST face remains available. |
| [OpenRGB server](../crates/neuron-host/src/adapters/openrgb.rs) | Accepts OpenRGB SDK clients on port 6742. | Neuron does not yet act as an OpenRGB client for other devices. |
| [OBS client](../crates/neuron-host/src/adapters/obs.rs) | Connects to local obs-websocket v5, sends requests and publishes OBS events. | Requires OBS's websocket server and its configured credentials. |

The app exposes connection status on the SYSTEM page. The CONNECTIONS control
can change the host state at runtime unless `NEURON_HOST` overrides it.

## Ownership and recovery

An adapter claims a lighting surface with an owner, priority, content and
lease. The arbiter resolves the visible frame. Neuron's profile lighting is
the base layer; a game or tool can paint above it without writing directly to
the device.

Chroma REST sessions use a 15-second lease. Effect writes and heartbeats
refresh it. When a session stops, the lease expires and its claim is released.
OpenRGB has no protocol heartbeat, so its claims remain for the connection
and release on disconnect. The shell also sweeps expired leases independently
of frame rendering and publishes release events on the bus.

The kernel owns no sockets or device handles. Adapters and the writer communicate
through a channel-backed host handle. On a contained kernel fault, the shell
recreates declared surfaces. Each owner must then reassert its content. A
data journal cannot reconstruct the live rendering closures used by the app
and adapters. The [governor](../crates/neuron-host/src/governor.rs) limits
restarts and escalates repeated faults. Host calls have bounded waits, so a
stalled render cannot block a caller indefinitely.

## Adapter boundary

Adapters decode protocol requests into host claims and signals. The socket
pumps are separate from the parsers, which lets protocol messages be tested
without a live network or device. The writer is the sole path for resolved
lighting frames to a surface. Device capability data determines which
surfaces and layouts the host can expose.

The current Chroma and OpenRGB listeners bind locally. The lighting protocols
do not expose Neuron's macro or process-spawn actions. Any future network
control surface for those actions needs authentication and scoped authority
before it can accept commands.

## Planned work

- Check Chroma REST and shared-memory behavior against more real games, including
  complex effects, disconnects and dynamic device changes.
- Add the OpenRGB client side if Neuron is to control devices served by another
  OpenRGB process.
- Specify and version a Neuron control protocol before external clients rely
  on one. Authenticate commands that can synthesize input or launch programs.
- Evaluate telemetry and other adapters against a concrete use case, with
  state accuracy, latency and teardown tested before calling them supported.

Earlier protocol research included possible integrations with game telemetry,
MQTT, WLED, MIDI, OSC and other tools. Those are ideas, not shipped interfaces
or a release commitment.
