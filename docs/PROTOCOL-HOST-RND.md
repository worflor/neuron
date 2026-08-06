> **🤖 agent-generated · live context doc**
> *not official docs.* an LLM wrote this while building neuron. it may be
> stale, wrong, or slop — or it may be load-bearing and exactly right.
> code is the source of truth; verify before you lean on it.
>
> **kind:** R&D findings + design (protocol-host) · **as of:** 2026-07-04 · **trust:** the thesis, principles, and design sections are durable; the integration plan tracks a moving codebase

# Neuron Protocol Host — R&D Findings & Design

> **Status:** R&D, branch `rnd/protocol-host`.
> **Purpose:** Durable record of the research + design behind making neuron a *local
> protocol host* — the hub that speaks every RGB/telemetry/creator/automation
> protocol, built from first principles in Rust to be the best of all of them.
> Everything below is distilled from a large research pass (Synapse parity +
> protocol landscape) plus the architecture reasoning that followed.

---

## 0. The one-sentence thesis

**We are not building an app with integrations. We are building a local, honest,
capability-driven *protocol hub* — the one place where games, sensors, sound,
screen, lights, creator tools, and hardware all meet and can be wired to each
other, with neuron's engine as the routing logic in the middle. The peripherals
are just the first things plugged in.**

The differentiator nobody in the prior art got right: **ownership & teardown as
first-class.** Every conflict in this space (the "bad rave" blinking, "close the
other app first", lighting frozen-on after a game exits) is the same root bug —
two sources writing one device with no arbiter, so it's last-writer-wins and it
flickers. We model ownership. We are the only hub you never have to close to run
another.

---

## 1. Guiding principles (the soul — do not violate)

These come from the project's standing preferences.

1. **The software should disappear into the hardware.** If the user uninstalls
   tonight, as little as possible should break. Onboard-first. The app is a
   workshop you visit, not a daemon holding the peripheral hostage. Test for every
   feature: *"if neuron isn't running, what breaks?"* — drive that toward nothing.

2. **Honesty is the moat.** Every knob reflects what the hardware *actually*
   reports. No dead knobs, no fake success, no marketing lies. In a category where
   everyone lies in both marketing and UI, being the tool that tells the truth
   about your own hardware is an unfakeable advantage. Degrade *visibly*.

3. **"Immersive" = mechanic correctness, NOT garnish.** Reactive lightshows bolted
   on events are slop. What matters is: truthful **State**, real **Latency**, clean
   **Teardown**, honest **Parity** with the native tool. A shift light is a precise
   *instrument* (is it correct + lag-free?), not a rainbow that pulses on kills.

4. **Local-first, no account, minimal privilege.** Loopback by default. No
   telemetry. No always-on elevated service (that was the literal root of
   CVE-2021-44226). One killable userspace binary.

5. **Capability-driven all the way.** A new device is a TOML, not a code change.
   A new protocol is a small codec adapter, not a new subsystem. The UI renders
   from declared capabilities. Point this same philosophy *north* (at protocols)
   that we already point *south* (at devices).

6. **Focused face, contained depth.** Lead with one thing it's obviously great at;
   the depth is revealed as you go, never dumped on the front door. Resist the
   infinite-integration menu.

---

## 2. THE FOUR-QUESTION BAR (acceptance test for every port)

Every integration — ingest or emit — ships **only** if it passes:

- **State** — does it track the truth, and never lie about what's connected /
  active / owned?
- **Latency** — is it actually real-time, or laggy theater?
- **Teardown** — does it release cleanly? No stuck session, no leaked held key, no
  lighting frozen-on. *(Every one of these is a named Synapse failure.)*
- **Parity** — does it do what the native tool does, correctly, no worse?

If a port can't pass these four, it's slop no matter how good the demo looks.

---

## 3. Synapse parity research (condensed)

**Sourcing caveat:** reddit.com was firewalled to the research crawler the entire
time. Evidence is from Razer Insider forums, Tom's Hardware, PC Gamer, TechPowerUp,
RTINGS, GitHub/GitLab, and OSS-tool communities. Equivalent quality, not literal
Reddit threads.

### 3.1 What people LOVE (keep / match)
- Per-key/button remap that persists. **Hypershift** hold-layer (we already have
  multi-layer in `neuron-cli`; Synapse caps at ONE layer, hold-only, no lighting
  feedback → shipping toggle-mode + layer-lighting-feedback beats them outright).
- Macros with **real logic** — Synapse is dumb record-replay (no `if`, no clean
  hold-until-release, no repeat-while-held; degrades over runtime; S4 "forces
  minimum macro delay"). Our `MacroNode` tree (RepeatN/RepeatWhile/conditionals/
  cross-macro) already beats the Synapse+AutoHotkey combo people are forced into.
- DPI stages + OTF cycle, polling rate, lift-off distance. Snap Tap (we do it
  *better*: gated + sealed-consent; add per-game legality labels).
- Layered lighting (our Pattern×Spectrum ≈ Chroma Studio) + cross-device sync
  (`lighting mirror` ≈ Chroma's most-missed feature).
- Beloved effect vocabulary to cover: **Static, Breathing, Spectrum Cycling, Wave,
  Reactive, Ripple, Starlight**. (Fire/Starlight/Immersive = niche.)

### 3.2 Genuine feature gaps (ranked)
1. **Reactive / Ripple lighting** — input-driven; needs a live key-event feed into
   the compositor. In the "iconic Chroma" tier.
2. **Wireless battery QoL** — Synapse's *most-hated small feature*. What people
   actually want: **accurate tray readout** + **dismissible/threshold low-battery
   warning** (false-low-on-wake is the real hatred). Sleep/threshold sliders
   themselves are niche/rarely-touched — don't over-invest. Our own "vitals 1Hz
   wakes a sleeping mouse" bug is the same failure class — fix it.
3. **Audio-reactive lighting (visualizer)** — moderate-loved. **CORRECTION
   (lifecycle map): already exists in-tree** — `neuron-core/src/audio_level.rs`
   (~60Hz peak sampler, lock-free atomic, auto-stops ~2s unread) feeds a readout
   pattern. Gap is polish/exposure, not existence.
4. **Ambient/screen-mirror lighting** — niche but high-delight (SignalRGB's
   headline paid feature). **CORRECTION: also already exists** —
   `neuron-core/src/screen_ambient.rs` (~18Hz desktop grab → 22×6 zone grid,
   auto-stop) + readout pattern. Same: polish, not existence.
5. **Gaming Mode / Win-key lock** — table-stakes; ship with a visible on-state.
6. **DPI sniper/clutch button** — essential FPS; make it first-class.
7. **Rapid Trigger / adjustable actuation** — *the* top competitive-keyboard demand
   of 2024-26 (207+ pro CS2 players on Hall-effect). CONDITIONAL: our BlackWidow
   Chroma V2 isn't analog. Synapse's Rapid Trigger is **global (all-keys)** — so
   *per-key* actuation would beat them if we ever target analog/Hall-effect boards.

### 3.3 What people HATE (our architecture already answers)
Mandatory account/cloud login (root of the 2 worst bugs: destructive cloud-sync
wiping macros on S3→S4 migration, + slow startup). Bloat (~30% idle CPU, 160MB
"to run a mouse", 2-3min startup, "1GB bloatware"). Telemetry/AI-training clauses.
Always-on elevated service w/ world-writable dirs (= CVE-2021-44226). Updates that
break macros/profiles. Cloud-sync conflict UX. Lighting/macros dying on app close.

### 3.4 Market gap (why this is worth doing)
**No actively-maintained, cross-platform, full-feature (lighting + macros + DPI +
battery + profiles) Synapse replacement exists.** OpenRGB/SignalRGB = lighting-only,
still need Synapse for DPI/macros. OpenRazer *deliberately removed* macros. Every
Windows-native attempt (razer-ctl, Knife) is archived/dead. Live ones are all
single-OS or single-device-class. **That gap is exactly where neuron sits.**

### 3.5 Two late additions
- **macOS restoration**: Synapse 2.0 ran on Mac; S3 dropped it, never returned.
  Our cross-platform-by-traits Rust discipline makes Mac a "we restore what they
  took away" differentiator (user has a Mac collaborator).
- **Synapse macro XML import** = migration on-ramp ("I lost 200+ profiles" rage).

---

## 4. Protocol landscape (the northbound surface)

The reframe: we RE'd the **southbound** protocols (toward silicon). The opportunity
is the **northbound** surface (toward OS/games/network/room). Our app can be a
**polyglot in both directions**: a *sink* that ingests and a *source* that emits.
Signals flow IN (telemetry, sensors, SDK effects, app events) → through our engine
(conditions + layers + `act` verbs) → signals flow OUT (device writes, virtual
devices, re-emitted SDK effects, automations). **It's a bus, and we already built
the junction box.**

### 4.1 ⭐ THE KEY FINDING — be a protocol SERVER, not a DLL parasite

**The Razer Chroma native C++ SDK is just a thin HTTP client over the same
`localhost:54235` REST server.** `RzChromaSDK64.dll` internally makes HTTP calls to
`127.0.0.1:54235`. Games that link the native DLL *and* games that call REST both
hit the same loopback endpoint.

→ **Reimplement ONE REST server and you catch BOTH.** No per-game DLL hijacking.
And it's *more correct*: a legitimately-networked local service sidesteps the
anti-cheat DLL-integrity checks that block the DLL-proxy approach, AND the protocol
has a built-in **15-second heartbeat timeout** — so session teardown is a property
of the protocol, not an afterthought.

Corsair iCUE & Logitech LED SDKs = **DLL-proxy ONLY** (no REST fallback), anti-cheat
blocks them, and it's a maintenance sinkhole (JackNet RGB Sync died citing exactly
this). **These are the trap. Avoid unless a specific game demands it.**

Winning move: **be a local protocol *server*.** Servers have clean lifecycles;
injected DLLs have anti-cheat risk + per-vendor rot. Chroma-REST + OpenRGB-TCP are
the two you actually want.

Prior art to study: `captin411/python-chroma-rest-server` (proves OS-portable
Chroma REST server), `chroma-sdk` org (Colore/chroma-python for JSON schema
cross-check), `Vaskivskyi/ha-chroma`.

### 4.2 Chroma REST wire format (for the flagship adapter)
- `POST /razer/chromasdk` app-info → returns session URI + per-device sub-URIs.
- `GET /razer/chromasdk` → installed SDK version.
- `PUT` heartbeat ~1s; **15s inactivity kills the session.**
- Per-device effect endpoints, e.g. `PUT/POST {session}/keyboard`:
  - `CHROMA_NONE`, `CHROMA_STATIC` (`{"color": <BGR int>}`)
  - `CHROMA_CUSTOM` — **6 rows × 22 cols** grid, colors packed BGR ints
  - `CHROMA_CUSTOM_KEY` — grid + key-activation bitmask (0x01000000)
  - `CHROMA_CUSTOM2` — newer keyboards, **8×24** grid
  - analogous for /mouse (CUSTOM2 grid), /mousepad, /headset, /keypad, /chromalink
  - response `{"result": <code>}`.
- Colors are **BGR**, not RGB. Grid → real keys via our capability/layout TOML.

### 4.3 OpenRGB SDK protocol (TCP 6742) — build FIRST
Fully open, versioned, cross-platform binary TCP. **16-byte header:**
```
char[4] pkt_magic = "ORGB"
u32     pkt_dev_idx
u32     pkt_id       // command
u32     pkt_size     // payload length
```
Key command IDs: 0 REQUEST_CONTROLLER_COUNT, 1 REQUEST_CONTROLLER_DATA,
40 REQUEST_PROTOCOL_VERSION (currently v5), 50 SET_CLIENT_NAME,
100 DEVICE_LIST_UPDATED (server→client push), 140 RESCAN, 150-153 profile ops,
200/201 plugin, 1000 RESIZEZONE, 1050 UPDATELEDS, 1051 UPDATEZONELEDS,
1052 UPDATESINGLELED, 1100 SETCUSTOMMODE.
Controller data (resp to id 1): nested varlen — name/vendor/desc/version/serial/
location (u16-len-prefixed strings), Mode[] array, Zone[] array (name, type, LED
min/max/current, matrix, segments in v4+), LED[] array, direct colors[] array,
(v5+) LED alt-names + flags.
**Build both server AND client.** Server → Home Assistant's official OpenRGB
integration + openrgb-python + community scripts drive OUR devices. Client → pull
in other-brand devices, become the mixed-rig sync hub. Rust crates to crib:
`openrgb-rs`/`openrgb2` (nicoulaj). **Recommend building this first** — it forces
our device/effect model to be cleanly externally-addressable, the best pressure
test for the internal model.

### 4.4 Meta-RGB prior art (architecture references)
- **Aurora/AuroraRGB** (C#): unifies via (1) official SDK/GSI integrations,
  (2) wrapper/compat DLLs mimicking vendor SDKs, (3) live process-memory reads
  (fragile, anti-cheat-banned). Failure mode: signature-checked SDK DLLs refuse the
  patched file.
- **Artemis** (C#, RGB.NET plugin-based): same DLL-wrapper trick.
- **SignalRGB** (closed): games talk to it over a local HTTP "Canvas API" + screen
  analyzer fallback. Architecture ref only (Lightscript/JS canvas + `device.color`
  render loop).
- **JackNet RGB Sync** (archived): tried multi-SDK-wrapper across all vendors,
  retired citing "inherent complexity of interacting with SDKs" — the DLL-per-vendor
  approach is a sinkhole. **Cautionary data point.**

### 4.5 Telemetry ingest (mechanic value, not lightshow)
Real payoff: **auto-legality** (CS2 GSI truthfully reports in-match → Snap Tap/turbo
self-disable on banned servers, re-enable after) + **honest state-based context
switching** (beats Synapse's unreliable exe-name allowlist).
Best backends by openness × payoff:
1. **F1 22-25 UDP** — open spec, raw-UDP parse, no SDK. Game hands you a ready-made
   **shift-light bitmask** (`revLightsBitValue`) + flag state (`vehicleFiaFlags`).
   Rust: `f1-game-packet-parser`. Best effort:payoff in the list. A *precise
   instrument*, not garnish.
2. **Elite Dangerous `Status.json`/Journal** — pure file-tail, no network, rich
   truthful state (shields/hull/fuel/pips). Cross-platform trivial.
3. **CS2/Dota GSI** — game POSTs JSON to a localhost HTTP listener you run
   (`.cfg` in game's cfg dir). CS2 fields trimmed for anti-cheat; local-player-only
   when playing. Rust crates: `gsi-cs2`, `dota-gsi`. Same protocol shape as
   **SteelSeries GameSense** (which shipped "keyboard as HP bar" — proven concept).
4. **ETS2/ATS SCS SDK** — official, and has community Linux/macOS forks with
   identical struct layout → best *cross-platform-native* validation case.
Others: iRacing (`ShiftIndicatorPct`, Windows shmem), Forza Data Out (open UDP),
X-Plane RREF (dead-simple UDP subscribe), ACC/AC/RaceRoom shmem, MSFS SimConnect
(heavy), LoL Live Client Data (127.0.0.1:2999, unofficial), Minecraft mods,
Elite. **Steal SimHub's real lesson: "normalize once, bind anywhere" + a declared
External-Sim-Integration schema** so new games are a data file, not a parser.

### 4.6 Creator / streaming ecosystem
- **OBS `obs-websocket` v5** (`obws` crate) — ⭐ replaces the fake-keystroke hack
  the research found people using: real `SetCurrentProgramScene` works minimized,
  no hotkey collision, + bidirectional (react to `StreamStateChanged`/
  `InputMuteStateChanged`). `ws://localhost:4455`, sha256-challenge auth. Pure
  localhost, cross-platform. Textbook mechanic-correctness win.
- **Twitch EventSub** (WebSocket, `twitch_api`) — "Razer Streamer Companion but
  better + local". Raid/sub/follow/bits. The ONE cloud-auth edge — ship flagged
  opt-in, never core.
- **Discord RPC** (local IPC named-pipe/unix-socket) — `SPEAKING_START/STOP` →
  on-air indicator. Local-clean. Voice state only for users sharing your channel.
- **VTube Studio** (local WS 8001) — sleeper: feed device telemetry → avatar
  params (battery/mic/macro-state → model reacts physically). Nobody does this.
- **Bitfocus Companion "Satellite"** (open TCP 16622/WS 16623, no auth) — lets
  neuron *present as a Stream Deck surface* + inherit Companion's 100+ downstream
  tools. Plus `elgato-streamdeck` crate drives *real* Stream Decks over raw HID
  (same paradigm as our Razer RE). Two-way Stream Deck citizenship, no Elgato app.

### 4.7 System / media / ambient sources
- **Media now-playing** — SMTC (`windows::Media::Control`) + MPRIS (`mpris` crate).
  Both event-driven, near-zero cost, cross-platform. Build-first provider. Value =
  *context* (correct media-key behavior), not a visualizer.
- **Audio-reactive** — hero feature, portable. `cpal` for device I/O; BUT cpal
  doesn't expose Windows loopback → drop to `wasapi` crate for that one path; Linux
  = PipeWire monitor source; macOS = ScreenCaptureKit audio tap. FFT via `realfft`.
- **Screen-ambient** — heaviest lift. Win: DXGI/WGC (`windows-capture`); macOS:
  ScreenCaptureKit (`screencapturekit-rs`, cleanest); **Linux/Wayland sandboxes it
  behind a PipeWire portal permission** (`ashpd`) — polish tier, not launch. Sample
  10-30Hz, downscale to ~32×18 BEFORE color extract or it spins fans. Build last.
- **HW sensors** — NVML (`nvml-wrapper`, both OSes, in-proc, degrades cleanly);
  Linux hwmon (`libmedium`); Windows HWiNFO shared-memory (`Global\HWiNFO_SENS_SM2`,
  reverse-engineered, defensive version-check) / LHM WMI+HTTP fallback; RTSS shmem
  for FPS (undocumented, Windows-only). Poll 1-2Hz, best-effort optional providers.
- **System events** — battery (`starship-battery`, X-platform); session lock/unlock
  (Win `WM_WTSSESSION_CHANGE`; Linux logind D-Bus) → ties into our `curtain`
  concept (auto-arm on lock); idle (`user-idle`); network/VPN (per-OS shim).
  Notifications read-back = high friction, deprioritize.

### 4.8 Input / automation / smart-home emission
- **MQTT + Home Assistant discovery** (`rumqttc`) — highest-leverage: publish
  retained discovery topics once → neuron's battery/presence/triggers appear as
  native HA entities, bidirectional. Follows OpenRGB's official-HA-integration
  precedent. Turns neuron into a first-class citizen of the automation graph.
- **WLED** (raw per-LED UDP 21324, WARLS) — standout payoff:effort. Open, no-auth
  UDP LED stream → our *existing* Pattern×Spectrum compositor drives keyboard AND
  room LEDs through **one pipeline**. Parity by construction.
- **OSC** (`rosc`) — cheap; unlocks creative space nothing else touches: VJ
  (Resolume), theatrical cue (QLab), VRChat avatar puppeting from a mouse dial.
  UDP, address-pattern maps ~1:1 onto our verb model.
- **MIDI** (`midir`) — near-free on Linux/macOS (virtual ports just work); Windows
  needs loopMIDI or a small `teVirtualMIDI` FFI shim (Windows MIDI Services will
  erase this friction soon). Dials/keys → CC/note → DAW/OBS/VJ; MIDI in → macros.
- **Virtual gamepad** — Win: `vigem-client` (pure-Rust; ViGEmBus archived but
  works); Linux: uinput (`evdevil`); macOS: ~nothing. Teardown bites hardest here:
  **release every button on exit** (our held-key-RAII concern exactly).
- **Local smart lights** — Hue Entertainment API (DTLS-PSK UDP 2100, low-latency,
  hand-roll DTLS — phase 2), WLED (above), LIFX LAN (UDP 56700, documented),
  Nanoleaf (local HTTP+token), Govee LAN. All LOCAL, non-cloud.
- **Unified `act` control surface** — one JSON-RPC-ish schema over multiple
  transports: WebSocket (browser/phone), named-pipe/unix-socket (AHK/Python/Lua),
  stdin (CLI). *This is one thing, not per-client.* Our `act` verbs turned outward.

### 4.9 The full port map, ranked by mechanic value
**Tier 1 (build first — highest parity, cleanest lifecycle, fully local):**
1. OpenRGB server+client (proving ground for the internal model)
2. Chroma REST server (the "leave Synapse completely" flagship; TTL teardown)
3. OBS obs-websocket (cleanest correctness win; kills the fake-keystroke hack)

**Tier 2 (high mechanic value, a little setup):**
4. Telemetry ingest trait (UDP/HTTP/shmem/file-tail backends; F1+Elite+GSI first;
   auto-legality is the sleeper feature)
5. MQTT + HA discovery
6. Media now-playing (SMTC/MPRIS)

**Tier 3 (correct-node emission):**
7. WLED + local smart lights (same compositor → room LEDs)
8. MIDI / OSC (controller node for DAW/VJ/VRChat)
9. Virtual gamepad (teardown = release all on exit)
10. Unified `act` surface (WS + pipe + stdin)

**Tier 4 (opt-in edges + explicit traps):**
- Twitch EventSub (the one cloud edge, flagged opt-in), Discord RPC, VTube Studio,
  Bitfocus Companion Satellite.
- ⚠️ Corsair iCUE / Logitech LED impersonation = the TRAP (DLL-proxy, anti-cheat,
  sinkhole). Skip unless a specific game forces it.

---

## 5. THE ARCHITECTURE (first-principles, the crown jewels)

### 5.1 Model ownership as a first-class thing (the differentiator)
Every conflict is an ownership bug. Nobody modeled it. **We extend our layered
compositor so every source is a LAYER WITH AN OWNER, PRIORITY, AND LIFECYCLE:**

```
Layer {
  owner:    SourceId,          // base-profile | chroma-session | openrgb-client | telemetry-binding | obs | ...
  priority: i32,
  ttl:      Option<Deadline>,  // heartbeat-refreshed; lapses → layer auto-drops
  scope:    DeviceZoneMask,    // which LEDs/zones it claims
  content:  LayerContent,      // static | pattern | live-buffer an adapter pushes into
}
```
Resolution: per-LED, walk layers top-priority → down; first non-expired layer that
claims that LED wins (or alpha-composite — Pattern×Spectrum already composites).
A Chroma game session = high-priority **transient** layer with a 15s TTL refreshed
by heartbeat PUTs. Lapses → **auto-drops → lower layer shows through. No flicker,
because there's an arbiter. No stuck lighting, because teardown is the DEFAULT
path, not special-cased code.**

This IS the four-question bar made real: State = arbiter always knows who owns
what. Teardown = releasing a layer is the normal path. Latency = one compositor,
one write path. Parity = honor the source's intent, then get out cleanly.
Generalizes beyond lighting: `act`/input ownership, device-tuning claims, etc.
**Build this before any protocol adapter — every adapter depends on it.**

### 5.2 Protocols are codecs at the edges; the model is the center
One canonical internal representation:
- a **spatial device/zone/LED model** = union of Chroma's 6×22 / 8×24 grid,
  OpenRGB's zones/matrices/segments, and our real device layouts (from capability
  TOML).
- a **named signal namespace** for telemetry/events (`cs2.health`, `gpu.temp`,
  `obs.scene`, `race.rpm`, `discord.speaking`, `now_playing.changed`, ...).

Every protocol is a **thin adapter that ONLY talks to the bus, never to device
I/O.** Adding a protocol = a small, isolated, testable codec that inherits all our
correctness for free. This is SimHub's "normalize once, bind anywhere" + OpenRGB's
controller model, done deliberately instead of accreted.

### 5.3 Split the planes; authenticate the dangerous one
- **Lighting-ingest plane** (Chroma/OpenRGB in): can be permissive-localhost —
  worst case someone blinks your keyboard.
- **Control plane** (`act`/macros): can synthesize input + run commands. An
  UNauthenticated localhost socket that fires macros is a local-priv-esc vector
  (same *class* as the Razer CVE, self-inflicted). → **capability-scoped +
  authenticated**, dangerous verbs behind the existing sealed-consent pattern.
  **Do this day one, not after a CVE.** None of the prior art considered this.

### 5.4 Truthful capability mapping
When a Chroma game asks "is there a keyboard grid," answer from the *actual
connected device's declared layout* — map its grid to real keys; when you can't
honor something, degrade *visibly*. Same TOML that describes a device to the UI
describes it to the Chroma grid mapper. Honesty pushed down to the handshake.

### 5.5 Publish OUR OWN protocol, open + versioned, from commit one
OpenRGB became a standard because its protocol was documented + stable, so others
integrated *with* it (HA integrated OpenRGB, not Razer). Be the OpenRGB of the next
generation — but our surface is lighting **+ macros + tuning + telemetry**, not
lighting-only. Version it, document the wire format, keep backward-compat.

### 5.6 No fragility
No daemon-that-breaks (OpenRGB's DKMS-per-kernel pain), no kernel module, no
elevation, loopback-default. One Rust binary, userspace HID, servers bind
127.0.0.1 only unless told otherwise, discoverable, killable, zero-config. The
cross-platform-by-traits discipline is what makes this hold on Win/Linux/Mac.

### 5.7 Capture-and-replay as the test methodology
The RE community's superpower is packet captures. Capture real Chroma/OpenRGB/
telemetry traffic once, replay against adapters in unit tests → protocol
correctness with zero hardware, deterministic, regression-proof. This is how we
avoid OpenRGB's "device support = graveyard of unresolved GitHub issues" fate.

---

## 6. HOST DESIGN R&D — immortal, redundant, simple (math-stack inspired)

The goal: make the host truly immortal, redundant, and simple, using our own
math stack as inspiration. That stack is the **AR(2) eigenmotion oscillator**
`z[n] = K·z[n-1] − G·z[n-2]` — cascaded macro+micro oscillators + residuals, per
`glyph.rs` / `engram` / `.gwyph`.

The host is fundamentally an **actor/supervisor** problem. Gold-standard prior art
is Erlang/OTP: "let it crash" + supervision trees. The move is OTP-grade
supervision in a single Rust binary, *simpler*, with the eigenmotion math as
genuine (not decorative) inspiration for the control laws.

### 6.1 The Kernel (spine) — simple because it does almost nothing
Owns ONLY: the canonical device/zone/LED model + the ownership arbiter (§5.1) +
the signal bus. **No I/O.** Minimal surface = maximal reliability. Because it holds
no sockets and does no parsing, it has almost nothing that *can* crash. This is the
"simple" heart everything else orbits.

**Anti-poison design (no status-mutex poison to worry about):** the
kernel is a message-passing **actor** — it owns its state on ONE thread, everyone
talks to it via a channel. There is **no shared `Mutex` to poison.** A panicking
adapter cannot corrupt kernel state because it never holds a lock on it.

### 6.2 Adapters as supervised, isolated tasks (redundant + immortal)
Each protocol (Chroma REST, OpenRGB TCP, telemetry UDP, OBS ws, ...) is a task
that ONLY talks to the bus, spawned under a supervisor. If an adapter panics (a
malformed packet from some game), `catch_unwind` at the task boundary contains it,
**its layer is released (clean teardown by construction), and it restarts.** One
adapter dying can NEVER take the kernel or another adapter down. *That's* the
redundancy and the immortality — death is local and cheap.

### 6.3 Supervision as a DAMPED OSCILLATOR (the real math)
This is where AR(2) stops being a metaphor and becomes the control law.

A naive retry loop is a first-order system that either does nothing or storms.
Model the **restart controller as a second-order system** `z[n] = K·z[n-1] − G·z[n-2]`
where `z` is the restart interval / pressure. The eigenvalues λ1, λ2 of that
recurrence determine behavior:
- **|λ| < 1 ⇒ the system is stable** — after a perturbation (a crash) it decays
  back to equilibrium. This is the *mathematical definition* of a resilient
  supervisor: **restart policy tuned so its eigenvalues sit inside the unit
  circle ⇒ provably no restart storm.**
- Choose K, G for a *critically-damped* response: recover as fast as possible with
  no oscillation (no thrash between "restarting too fast" and "backing off too
  hard"). Exponential backoff is the degenerate real-eigenvalue case; the full AR(2)
  lets us tune overshoot/settling deliberately.
- The same `GlyphFit{k, g, lambda1, lambda2}` machinery in `glyph.rs` can *analyze*
  a live crash-interval series → if measured |λ| drifts toward 1, the subsystem is
  going unstable → escalate. **We can literally fit an oscillator to failure
  telemetry and read off stability.**

### 6.4 Cascade / residual as tiered supervision (redundancy structure)
The eigenmotion codec is macro-oscillator (coarse) + micro-oscillator (fine) +
residual (what neither captured). Map directly onto a supervision hierarchy:
- **macro tier** = coarse supervisor over whole subsystems (all-lighting,
  all-telemetry).
- **micro tier** = fine supervisor over individual adapters.
- **residual** = the failure *neither tier absorbed* → the genuinely-unknown fault
  that gets logged/escalated to the human.
- **energy-capture metric** (engram's `macro_capture`/`micro_capture`/
  `residual_energy`) becomes a **health score**: what fraction of "failure energy"
  was absorbed at each tier vs. leaked to residual. A rising residual ratio = the
  system is failing in ways our supervisors don't model yet.

### 6.5 Reconstructable state — corrected after implementation
The original idea here (kept for the record): a rich stream reduces to a tiny
seed and reconstructs, the way a `.gwyph` block reconstructs a trajectory from
K/G + residual, so mirror that at the host — a declaration log that replays to
identical state on restart. A `journal` module was built to this spec and was
fully unit-tested (replay, compaction, named-layer supersession) — but nothing
in production ever fed it, and it was deleted (2026-07-07).

Why it could never carry the load: every real layer's content is
`Content::Live` (closures, not data) — the app's animated base, the Chroma REST
fade ramp, the OpenRGB `PolicyLayer`. A data journal can't replay a closure by
construction, so a "durable declarations only" journal could only ever cover a
sliver of state (static pinned fills) while the layers that actually matter
stayed outside it. What rebirth actually does, and what turned out to be
sufficient: replay the SEED (declared surfaces only — `shell.rs`'s `seed: Vec
<SurfaceInfo>`), and let each owner re-assert its own content on its own
schedule — the Chroma REST heartbeat, the OpenRGB pump's idle-tick `reassert`,
the SHM refresh, the app-base heartbeat. Recovery is owner-driven, not
seed-driven; the seed only has to be enough for surfaces to exist again.

### 6.6 Everything is a signal on a bus
Eigenmotion decomposition factors a raw stream into interpretable bands; the host
factors the raw event bus and lets adapters subscribe to bands. Same "normalize
once, bind anywhere." The bus is the one shared abstraction; the compositor and the
macro engine are just two of its subscribers.

### 6.7 Host topology (target)
```
                    ┌──────────────────────────────────────────┐
                    │  KERNEL (actor, 1 thread, no I/O)         │
                    │  • canonical device/zone/LED model        │
                    │  • ownership arbiter (layers: owner/       │
                    │    priority/ttl/scope)  ← §5.1             │
                    │  • signal bus (named namespace)           │
                    │  state = replayable declaration log ←§6.5 │
                    └───────────▲──────────────┬────────────────┘
             commands (channel) │              │ frames / signals (channel)
        ┌───────────────────────┴──────────────┴───────────────────────┐
        │                    SUPERVISOR (damped-AR(2) restart ←§6.3)      │
        │           macro tier → micro tier → residual  (←§6.4)          │
        └──┬─────────┬─────────┬─────────┬─────────┬─────────┬──────────┘
   catch_unwind each, isolated; panic → release layer + restart (§6.2)
      │         │         │         │         │         │
 ┌────▼───┐┌────▼───┐┌────▼────┐┌───▼────┐┌───▼────┐┌───▼─────┐
 │Chroma  ││OpenRGB ││Telemetry││ OBS ws ││ MQTT/  ││ act ctrl│
 │REST    ││TCP srv ││UDP/HTTP/││(obws)  ││ HA     ││ plane   │
 │:54235  ││:6742   ││file-tail││:4455   ││        ││(AUTHED, │
 │(TTL    ││(+client││(F1/Elite││        ││        ││ §5.3)   │
 │ teardn)││ mode)  ││ /GSI)   ││        ││        ││         │
 └────┬───┘└────┬───┘└────┬────┘└───┬────┘└───┬────┘└───┬─────┘
      │ ingest   │ both    │ ingest  │ both    │ both    │ ctrl
      └──────────┴─────────┴─ bus ───┴─────────┴─────────┘
                              │
                    ┌─────────▼──────────┐
                    │ device write path  │  ← ONE serialized writer
                    │ (writes.rs / HID)  │     (no anim/dispatch/vitals race)
                    └────────────────────┘
```

### 6.8 Concrete first-principles decisions
- **Kernel = actor, not shared-mutex.** Kills lock poisoning as a class.
- **Single serialized device-write path** owned by the kernel side, fed by the
  compositor output. Fixes the flagged anim-thread/dispatch/vitals write races —
  nobody writes the device except the one writer.
- **Every adapter is `catch_unwind`-wrapped + owns exactly one bus connection +
  releases its layer on drop (RAII).** Teardown is structural, not remembered.
- **Restart controller = tuned AR(2), eigenvalues inside unit circle.** Provable
  no-storm. Fit-oscillator-to-crash-telemetry for early instability detection.
- **State = replayable declaration log.** Compact seed, cheap rebirth.
- **Control plane authenticated from day one.**

---

## 7. Build order (recommended)
1. **Ownership/arbitration compositor + canonical device/zone/LED model** (the
   spine; §5.1/§5.2). Nail before any wire protocol.
2. **OpenRGB adapter (server + client)** — proving ground; validates the model
   both directions; instant HA + open-ecosystem plug-in.
3. **Chroma REST server** — flagship; TTL teardown pays off the ownership work.
4. **Split + authenticate the control plane** — before exposing `act` on any
   socket. Not after.
5. **Capture/replay harness** — alongside adapter #2, so correctness is locked
   from the first protocol.
6. **Publish the neuron protocol spec** — versioned, once the model survived
   contact with OpenRGB + Chroma.
7. Then telemetry (F1/Elite/GSI + auto-legality), MQTT/HA, OBS, then emission
   (WLED, MIDI/OSC, gamepad), then opt-in edges (Twitch/Discord/VTS).

---

## 8. Grounded integration plan (map → target topology)

### 8.1 The inversion (end-state)
**The GUI becomes client #1 of the host.** The host owns: kernel (arbiter + bus
+ journal) + the single device-writer task per device + the protocol adapters.
The Slint app, the CLI, and every external client speak the same surface. This
also *solves the app's missing single-instance guard for free*: binding the localhost
port/pipe IS the instance lock — a second launch detects the bind failure and
becomes a client of the running host instead.

### 8.2 How the kernel wraps (not replaces) today's compositor
The existing `LayerDef` stack (user's configured lighting) becomes the content
of ONE pinned arbiter layer at `band::BASE`. Protocol sessions (Chroma game,
OpenRGB client) claim leased layers above it. The arbiter resolves; the winner's
frame feeds the existing `Lights::animate` machinery (shared clock, row dedup,
deadline pacing) — pattern.rs is untouched; the arbiter sits ABOVE it.

### 8.3 Phases
- **Phase 0 (this branch, now):** `crates/neuron-host` — pure-std kernel:
  arbiter (owner/priority/lease/scope, resolve, sweep), bus (retained values +
  prefix subscribe + dead-sub pruning, generalizing controls::INJECT), journal
  (declaration log → replay → identical state), governor (AR(2)-damped restart
  control, Jury-criterion stability check, escalation ledger). Zero deps, zero
  I/O, fully unit-tested. **No workspace dep additions (manifest is FROZEN for
  deps; member-list add only).**
- **Phase 1:** host process shell — kernel on its own thread (actor: one owner,
  channels in/out, nothing to poison), OpenRGB server+client adapter against a
  MOCK sink; capture/replay harness for protocol correctness.
- **Phase 2:** attach to the app the *safe* way: adapter-driven lighting goes
  through `start_layers`-shaped calls; acts go through new `LiveCommand`
  variants. Host is NOT a new device-writer domain.
- **Phase 3:** the writer inversion — one writer task per device inside the
  host; anim threads, GUI one-shot opens, vitals, hidwatch battery reads all
  become kernel clients; `send_lighting_fast`'s unchecked reply-drain race dies
  structurally. Chroma REST adapter lands here (leases prove themselves).
- **Phase 4:** control plane (authed, sealed-consent verbs) + published neuron
  protocol spec + GUI-as-client migration begins.

---

## 9. Wire-format appendix (implementation-day details)

Extra precision captured during research, for when each adapter is built:

- **obs-websocket v5 auth**: Hello(op0) carries `{challenge, salt}`; Identify
  (op1) auth string = `b64(sha256(b64(sha256(password+salt)) + challenge))`,
  plus an `eventSubscriptions` bitmask. Then Request=op6 / RequestResponse=op7 /
  Event=op5.
- **Discord IPC**: named pipe `\\?\pipe\discord-ipc-{0..9}` (Unix: socket in
  `$XDG_RUNTIME_DIR`/`$TMPDIR`, try indices 0..9 — stable/PTB/canary bind
  different ones). HANDSHAKE `{v:1, client_id}` → READY. Voice events only for
  users sharing your channel; official discord-rpc repo is archived but the
  wire protocol is current (community "hard-mode" doc).
- **VTube Studio**: `ws://localhost:8001`, `apiName: "VTubeStudioPublicAPI"`,
  UDP 47779 discovery broadcast every 2s; one-time token consent then stored
  token. `InjectParameterDataRequest` values decay ~1s (resend; mode set/add).
- **Twitch EventSub WS**: `wss://eventsub.wss.twitch.tv/ws` → session_welcome
  `{session_id}` → create subs via Helix within 10s. **User access token
  required (app tokens rejected on WS transport)** — Device Code flow. Scopes:
  `channel:read:redemptions`, `bits:read`, `moderator:read:followers`,
  `channel:read:subscriptions`; raids need none.
- **HWiNFO shared memory**: mapping `Global\HWiNFO_SENS_SM2`, mutex
  `Global\HWiNFO_SM2_MUTEX`, magic `'SiWH'`; fixed-offset sensor+reading
  sections (namazso's gist documents the layout). Version-check the header
  before trusting offsets. RTSS: `RTSSSharedMemoryV2`, `dwFrameTime` in µs.
- **Companion Satellite**: plain-text TCP 16622 (WS 16623 in v3.5+);
  `ADD-DEVICE DEVICEID=.. PRODUCT_NAME=.. KEYS_TOTAL=.. KEYS_PER_ROW=..`;
  Companion streams `KEY-STATE` bitmaps back; client reports `KEY-PRESS` /
  `KEY-ROTATE`.
- **X-Plane RREF**: UDP 49000; request = `struct.pack("<4sxii400s", b'RREF',
  freq_hz, client_index, dataref_path)`; response = repeated `(i32 index,
  f32 value)` pairs; `freq=0` unsubscribes; array datarefs per-element.
- **CS2/Dota GSI cfg fields**: `uri`, `timeout`, `buffer` (0.0 on localhost),
  `throttle`, `heartbeat`, `auth` token, plus `data` subtree booleans; file at
  `game/csgo/cfg/gamestate_integration_<name>.cfg`.
- **F1 UDP PacketHeader**: `packetFormat u16, gameYear, gameMajor/Minor,
  packetVersion, packetId (13 types), sessionUID u64, sessionTime f32,
  frameIdentifier, playerCarIndex` — little-endian, no padding.
- **Elite paths**: `%userprofile%\Saved Games\Frontier Developments\Elite
  Dangerous\` — `Status.json` (Flags/Flags2 bitfields, Pips, Fuel, Cargo,
  LegalState, rewritten every few seconds) + append-only ndjson Journal.
