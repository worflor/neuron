# Protocol-Host R&D — Findings Ledger

> Status: R&D (branch `rnd/protocol-host`). This file is the durable capture of the
> 2026-07-01 research session: what host-side protocols exist, their wire formats, what
> prior art got right/wrong, and the design conclusions neuron builds on. If context is
> lost, start here. Companion docs: `LIFECYCLE.md` (neuron's own process map),
> `HOST-DESIGN.md` (the host architecture itself).

---

## 0. The thesis

Neuron already owns the **southbound** protocols (RE'd Razer USB/HID). The northbound
surface — games, OS, streaming tools, automation graphs — is a second protocol space we
can *speak natively*. The move is not "add integrations": it is to make neuron a **local
protocol hub** where every external protocol is a thin codec at the edge of one internal
model, and the internal model does the hard part once.

**The crown-jewel finding:** every infamous failure in this space is an *ownership* bug.
Synapse-vs-SignalRGB "bad rave" flicker, iCUE's conflicting-programs list, lighting
frozen after a game exits, "close the other app first" — all are two writers racing one
device with no arbiter. Nobody models ownership. Neuron's layered compositor already
exists; extend it so **every source is a layer with an owner, a priority, and a
lifecycle (TTL/heartbeat)**. Sessions release cleanly by construction. This is the
differentiator; build it before any adapter.

**The four-question bar** (every port must pass all four before it ships):

| Question | Meaning | Synapse failure it answers |
|---|---|---|
| **State** | tracks truth; never lies about what's owned/active | profiles "applied" but not on device |
| **Latency** | actually real-time, not laggy theater | Hue lagging 100-300ms behind |
| **Teardown** | releases cleanly: no stuck session/held key/frozen lighting | "4 years, same conversation" stuck-lighting threads |
| **Parity** | does what the native tool does, no worse | fake-keystroke OBS hack |

No sensory-effect slop: reactive lighting is only defensible as a *precise instrument*
(shift light = rev counter), never as garnish. Mechanic correctness is the product.

---

## 1. Lighting SDK ingestion (be the server, not the DLL)

### 1.1 Razer Chroma SDK — THE flagship port

- Real service: `RzSDKServer.exe`, listens `http://localhost:54235/razer/chromasdk`
  (plain HTTP) + `https://chromasdk.io:54236/...` (chromasdk.io resolves to loopback;
  often not even listening on real installs — 54235 is the one true backend).
- **Key finding: the native C++ SDK (`RzChromaSDK64.dll`) is a thin client over the same
  REST server.** Reimplementing the REST server catches BOTH native-SDK games and REST
  games. No DLL hijack needed. This also sidesteps anti-cheat DLL-integrity checks
  entirely (a legit localhost service, not a swapped file in the game process).
- Wire format:
  - `POST /razer/chromasdk` with app info (title/description/author/category/device
    list) → session URI `.../chromasdk/{sessionId}` + per-device sub-URIs.
  - Heartbeat `PUT` ~1s; **15-second inactivity timeout kills the session** — teardown
    is a protocol property. Use it: session death ⇒ layer release ⇒ fall back to base.
  - Effects per device (`PUT/POST {session}/keyboard` etc.):
    `CHROMA_NONE`, `CHROMA_STATIC {color: <BGR int>}`,
    `CHROMA_CUSTOM` = 6×22 grid of BGR ints,
    `CHROMA_CUSTOM_KEY` = grid + key bitmask (`0x01000000`),
    `CHROMA_CUSTOM2` = 8×24 (newer boards). Analogous sets for /mouse (CUSTOM2 grid),
    /mousepad, /headset, /keypad, /chromalink. Response `{"result": <code>}`.
  - Native surface (for parity checking): Init/UnInit/CreateEffect/Create*Effect/
    SetEffect(id)/DeleteEffect(id)/QueryDevice.
- Prior art: `captin411/python-chroma-rest-server` (proof the server reimplements fine
  off-Windows), `chroma-sdk` org (Colore C#, chroma-python) for schema cross-checks,
  `Vaskivskyi/ha-chroma` (client reference). Razer's official ChromaEmulator is only a
  visualizer, not a protocol reference.
- Docs: assets.razerzone.com/dev_portal/REST/html/index.html; RazerApi.md in
  tgraupmann/UnityRESTChromaSDK; developer.razer.com/works-with-chroma/chroma-sdk-changelog/.
- Catalog: the largest certified game library (Chroma Workshop program, 200+ titles) —
  this port is what lets a user delete Synapse *completely*.
- Rust: `axum` (or tiny_http) + `serde_json`. Grid → compositor layer mapping via the
  device's capability TOML (truthful capability answers in the handshake).

### 1.2 OpenRGB SDK protocol — build FIRST (proving ground)

- Open, versioned (protocol v5), OS-agnostic **binary TCP on 6742**.
- Header (16 bytes): `magic "ORGB" | u32 dev_idx | u32 pkt_id | u32 pkt_size`.
- Command IDs: 0 controller_count, 1 controller_data, 40 protocol_version (client sends
  its max; server echoes its own), 50 set_client_name, 100 device_list_updated
  (server→client push), 140 rescan, 150-153 profiles, 200/201 plugins, 1000 resizezone,
  1001/1002 clear/add_segment, 1050 UPDATELEDS, 1051 UPDATEZONELEDS, 1052
  UPDATESINGLELED, 1100 SETCUSTOMMODE, 1101/1102 update/save_mode.
- Controller data = nested var-len: u16-length-prefixed strings (name/vendor/desc/
  version/serial/location), Mode[] (flags, speed/brightness/color min-max, colors),
  Zone[] (type, led count min/max/cur, optional matrix, segments in v4+), LED[],
  colors[], v5 adds LED alt-names + flags.
- **Be BOTH server and client.** Server ⇒ Home Assistant's official OpenRGB integration,
  openrgb-python, community scripts all drive neuron for free. Client ⇒ pull other-brand
  devices from a real OpenRGB instance; neuron becomes the mixed-rig hub.
- Rust prior art: `nicoulaj/openrgb-rs` / `openrgb2` crates.
- Spec: OpenRGB repo `Documentation/OpenRGBSDK.md` + `NetworkProtocol.h`.
- Why first: forces the internal device/zone/LED model to be externally addressable and
  shakes out the arbiter with the simplest wire format. Instant ecosystem payoff.

### 1.3 Corsair iCUE / Logitech LED SDK — **the trap; skip**

- Both are native-DLL-only (CUESDK.dll / LogitechLedEnginesWrapper.dll → vendor service;
  no local server to reimplement). Only impersonation path = DLL search-order hijack.
- Anti-cheat/signature-checked SDK DLLs refuse swapped files (Aurora wiki documents
  this). JackNet RGB Sync died on exactly this treadmill ("inherent complexity of
  interacting with SDKs when better RGB control methods exist").
- Logitech's "516 supported games" is mostly static keybind profiles; true dynamic
  Lightsync titles are dozens. Corsair's real dynamic list is also small.
- Verdict: not worth the fragility. Chroma-REST + OpenRGB covers the ecosystem the
  right way. Revisit only if a specific must-have title demands it, eyes open.

### 1.4 Meta-RGB prior art (what to learn, what to avoid)

- **Aurora/AuroraRGB** (C#): 3 mechanisms — official SDK/GSI ingestion (good), wrapper
  DLLs (fragile), process-memory reading (banned/broken constantly). Learn: multi-source
  ingestion appetite is real. Avoid: injection.
- **Artemis** (C#, RGB.NET, plugin-based): same wrapper trick; nice layered-profile UI.
- **SignalRGB** (closed, $45/yr): bespoke per-game HTTP + screen-analyzer fallback +
  its own JS effect canvas (`device.color(x,y)` render loop). Learn: canvas abstraction,
  polish. Avoid: closed, paywalled, still needs Synapse for DPI/macros.
- **JackNet RGB Sync**: retired; cautionary tale (see 1.3).
- None of them model ownership/arbitration. That is neuron's opening.

---

## 2. Telemetry ingestion (game/sim state → signals)

Architecture rule (stolen from SimHub, done capability-style): **normalize once, bind
anywhere**. One `TelemetrySource` seam with four transport backends — UDP listener,
HTTP listener, shared-memory poller, file-tailer — emitting named typed signals
(`cs2.round.phase`, `f1.rev_lights_bitmask`, `elite.flags.overheating`) into the same
value namespace the macro engine's `Value`/context system already reads. New game =
data-file schema (SimHub "External Sim Integration" pattern), not a hand-rolled parser.

**Mechanic payoffs (not garnish):**
- **Auto-legality**: CS2 GSI truthfully says "live on Valve official server" ⇒ Snap
  Tap/turbo self-disable, re-enable after. The honest-mechanics version of per-game
  legality labeling.
- **Honest context switching**: real game state (in-match vs menu) beats Synapse's
  exe-allowlist guessing (which breaks on custom .exes and exclusive fullscreen).
- Instrument-grade lighting (shift light bar) where the user opts in.

Sources, ranked by openness × payoff:

1. **F1 22-25 UDP** (EA/Codemasters): one-way UDP, little-endian packed structs, no
   padding. `PacketHeader{packetFormat u16, gameYear, versions, packetId(13 types),
   sessionUID u64, sessionTime f32, frameId, playerCarIndex}`. `CarTelemetryData`:
   speed u16 km/h, throttle/steer/brake f32, gear i8, engineRPM u16, drs u8,
   **revLightsPercent u8 + revLightsBitValue u16 (bit0=leftmost LED..bit14) — a
   game-computed LED bitmask, purpose-built for shift lights**. `CarStatusData`:
   maxRPM/idleRPM, fuel, pit limiter, **vehicleFiaFlags (-1/0/1 green/2 blue/3
   yellow)**, ERS, tyre compound. Spec mirrored at `hotlaps/f1-game-udp-specs`; Rust
   crate `f1-game-packet-parser` (F1 22-24).
2. **Elite Dangerous Status.json + Journal**: `%userprofile%\Saved Games\Frontier
   Developments\Elite Dangerous\`; Status.json rewritten every few seconds — Flags/
   Flags2 bitfields (docked/landed/gear/shields/low-fuel/overheating/in-danger...),
   Pips (SYS/ENG/WEP), Fuel, Cargo, LegalState. Journal = append-only ndjson events.
   Pure file-tail (`notify` crate), fully documented (elite-journal.readthedocs.io),
   trivially cross-platform.
3. **CS2 / Dota 2 GSI (Valve)**: game POSTs JSON to a localhost HTTP server you run,
   enabled by a cfg file in `game/csgo/cfg/gamestate_integration_*.cfg` (fields: uri,
   timeout, buffer, throttle, heartbeat, auth token, data-subtree booleans). CS2
   exposes player.state.{health,armor,money,round_kills}, active weapon
   {name,ammo_clip,ammo_reserve}, round.phase, bomb state, map scores. Dota adds hero/
   abilities/items/buildings/draft. Caveats: CS2 trimmed fields vs CSGO (anti-cheat);
   playing (not spectating) = local player only. Rust crates exist (`gsi-cs2`,
   `dota-gsi`). NOTE: the protocol shape was co-designed with SteelSeries GameSense —
   whose stock demo was literally keyboard-rows-as-HP/armor/ammo.
4. **ETS2/ATS SCS SDK**: official telemetry SDK via memory-mapped file
   (`Local\SCSTelemetry`, RenCloud/scs-sdk-plugin); **actively maintained Linux/macOS
   forks use identical struct layout over POSIX shm** — best cross-platform-native
   validation case for the seam.
5. **iRacing**: `Local\IRSDKMemMapFileName` mmap @60Hz, 300+ vars incl.
   `ShiftIndicatorPct`, `SessionFlags` bitmask; ISO-8859-1 YAML session header.
   Windows-only. Rust: `iracing` crate, `memmap2`.
6. **Forza Data Out**: fire-and-forget UDP 60pkt/s, "Sled"/"Dash" fixed-offset formats;
   EngineMaxRpm/IdleRpm/CurrentRpm, per-wheel data, laps, race position. Community
   offset files in `richstokes/Forza-data-tools`.
7. **X-Plane RREF**: UDP 49000; request = `struct.pack("<4sxii400s", b'RREF', freq_hz,
   client_index, dataref_path)`; response = repeated (i32 index, f32 value) pairs;
   freq=0 unsubscribes; array datarefs per-element. Dead simple, cross-platform.
8. **MSFS SimConnect**: official SDK, Windows DLL, request/subscribe ceremony — heavy;
   later.
9. **League Live Client Data**: `https://127.0.0.1:2999/liveclientdata/allgamedata`,
   no auth, unofficial-but-stable. Big playerbase, but MOBA state maps less naturally.
10. **AC/ACC/RaceRoom shmem** (`SPageFilePhysics` etc., `$R3E`): good data,
    Windows-shmem, community relays exist. Same effort class as iRacing.
11. Minecraft (must be a Java mod — skip; MineLights exists), WoW (combat log is
    deliberately throttled — skip).

---

## 3. Creator / streaming ecosystem

1. **OBS obs-websocket v5** — the cleanest correctness win. `ws://localhost:4455`;
   Hello(op0, auth challenge/salt) → Identify(op1, auth =
   `b64(sha256(b64(sha256(pw+salt)) + challenge))`, eventSubscriptions bitmask) →
   Identified(op2); Request(op6)/RequestResponse(op7)/Event(op5).
   Requests: SetCurrentProgramScene, Start/StopStream, Start/Stop/PauseRecord,
   Toggle/SaveReplayBuffer, SetInputMute/ToggleInputMute, GetStreamStatus.
   Events: CurrentProgramSceneChanged, StreamStateChanged, RecordStateChanged,
   InputMuteStateChanged, ReplayBufferSaved. Rust: **`obws`** (mature, tokio).
   This natively replaces the community's fake-keystroke-through-Synapse OBS hack:
   real request, works minimized, bidirectional truthful state.
2. **Discord IPC/RPC** — local named pipe `\\?\pipe\discord-ipc-{0..9}` (Unix: socket
   in XDG_RUNTIME_DIR/TMPDIR, try 0..9). HANDSHAKE {v:1, client_id} → READY.
   SET_ACTIVITY (own presence); SUBSCRIBE: VOICE_STATE_*, SPEAKING_START/STOP (per-user
   voice activity — "on air" indicator), VOICE_SETTINGS_UPDATE (own mute/PTT),
   SELECT_VOICE_CHANNEL, SET_USER_VOICE_SETTINGS (per-user volume/mute). OAuth consent
   modal once for scopes; voice events limited to your own channel. Official
   discord-rpc repo archived but wire protocol current (see "hard-mode" doc).
3. **VTube Studio API** — `ws://localhost:8001`, JSON `apiName:
   "VTubeStudioPublicAPI"`, UDP 47779 discovery broadcast. One-time token consent →
   stored token. HotkeyTriggerRequest, InjectParameterDataRequest (values decay ~1s,
   resend; mode set/add), EventSubscriptionRequest. Novel: device telemetry → avatar
   params.
4. **Twitch EventSub over WebSocket** — `wss://eventsub.wss.twitch.tv/ws`;
   session_welcome{session_id} → create subs via Helix within 10s; keepalives.
   **User access token required (app tokens rejected on WS)** — Device Code flow for a
   desktop app; scopes: channel:read:redemptions, bits:read,
   moderator:read:followers, channel:read:subscriptions; raids scope-free. Rust:
   `twitch_api` (twitch-rs). THE one cloud-auth edge — ship opt-in, clearly labeled,
   never core. (Razer's Streamer Companion App is the closed precedent: alerts →
   lighting, requires Synapse; ours would be local-first and better.)
5. **Streamlabs/StreamElements** — socket.io + dashboard-copied JWT; only needed for
   donations/tips. Cloud-bound; lowest priority.
6. **Stream Deck citizenship** — two clean paths, no Elgato app needed:
   (a) **Bitfocus Companion "Satellite" API**: plain-text TCP 16622 (WS 16623 v3.5+),
   `ADD-DEVICE DEVICEID=.. PRODUCT_NAME=.. KEYS_TOTAL=.. KEYS_PER_ROW=..`; Companion
   streams KEY-STATE bitmaps back, client reports KEY-PRESS/KEY-ROTATE ⇒ neuron's
   macro keys present as a virtual Stream Deck surface and inherit Companion's 100+
   tool integrations. (b) `elgato-streamdeck` Rust crate drives real Stream Deck
   hardware over raw HID (same paradigm as neuron's Razer path). Elgato's own plugin
   SDK = per-plugin WebSocket, JSON events (keyDown/setTitle/setImage) if we ever want
   a plugin. OpenDeck/OpenAction is the FOSS reimplementation to watch.

---

## 4. System / media / ambient sources

Ranked (feasibility × delight × overhead):

1. **Media now-playing**: Windows SMTC via `windows-rs` `Media::Control`
   (GlobalSystemMediaTransportControlsSessionManager — event-driven:
   CurrentSessionChanged/MediaPropertiesChanged/PlaybackInfoChanged; also transport
   control). Linux **MPRIS** via `mpris` crate. macOS = private MediaRemote
   (community-hack tier; defer). Near-zero overhead, truthful context signal.
2. **Audio-reactive**: Windows loopback = **`wasapi` crate** (cpal has NO loopback —
   issue #251); Linux = cpal → PipeWire/Pulse **monitor source** (easy); macOS =
   ScreenCaptureKit audio tap (13+) or BlackHole virtual device. FFT: `rustfft`/
   `realfft`, throttle 30-60Hz. Portable except the capture shim.
3. **System events**: battery `starship-battery` (x-plat); lock/unlock Windows
   `WTSRegisterSessionNotification` + WM_WTSSESSION_CHANGE (LOCK 0x7/UNLOCK 0x8),
   Linux logind D-Bus (`logind-zbus`) — pairs with the `curtain` concept; idle
   `user-idle` crate (GetLastInputInfo / XScreenSaver / CG event source); network/VPN =
   thin per-OS shim (INetworkListManager / NetworkManager D-Bus) — lowest priority.
4. **Hardware sensors** (optional providers, degrade gracefully): NVIDIA `nvml-wrapper`
   (Win+Linux, loads lib dynamically); Linux hwmon via `libmedium`; Windows CPU/mobo =
   HWiNFO shared memory (official-ish: `Global\HWiNFO_SENS_SM2` + mutex
   `Global\HWiNFO_SM2_MUTEX`, magic 'SiWH', fixed-offset sensor+reading sections,
   documented in namazso's gist) or LibreHardwareMonitor via its WMI/HTTP-8085/
   LibreHardwareService mmap. RTSS shared memory (`RTSSSharedMemoryV2`, dwFrameTime µs)
   for FPS — Windows-only bonus. Poll 1-2Hz; version-check magic before trusting
   offsets. **Respect the vitals lesson: never let polling wake a sleeping device.**
5. **Screen ambience** (build LAST): Windows = Windows.Graphics.Capture (mixed-GPU
   safe; `windows-capture` crate) over DXGI duplication; macOS = ScreenCaptureKit
   (`screencapturekit-rs`, cleanest); Linux Wayland = xdg-desktop-portal ScreenCast →
   PipeWire via `ashpd` (permission prompt each setup — weakest platform). Sample
   10-30Hz, downscale to ~32×18 BEFORE color extraction, throttle at the capture API.
   This is SignalRGB's head