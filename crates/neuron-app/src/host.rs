//! Native protocol-host integration — where all the R&D piggybacks onto the
//! app it was built for.
//!
//! When CONNECTIONS is open (SYSTEM page; default OFF — opening loopback ports
//! and changing who may paint the boards stays a conscious opt-in):
//!
//! - the **bridge** owns exactly one writer per device (the fix for the
//!   seven-uncoordinated-writers race — continuous lighting has a single
//!   owner, and the app's own transient device reads are the only other
//!   toucher);
//! - the app's configured lighting stack (the `Vec<LayerDef>` the GUI edits,
//!   profiles carry, prefs persist) becomes the arbiter's **animated base
//!   layer** via [`CompositorContent`] — the SAME `Compositor` + render epoch
//!   + quantized clock as the GUI preview, so "the preview provably matches
//!   the board" still holds;
//! - the **Chroma** (`54235`, games) and **OpenRGB** (`6742`, tools) servers
//!   run per their own gates, so Overwatch or Home Assistant paints layers
//!   ABOVE the base and — the whole point — the base animation returns the
//!   instant they let go: no flicker, no stuck lighting, no cleanup code;
//! - the **OBS** connection (out to `4455`) runs both directions: macro verbs
//!   drive scenes/stream/recording, and OBS's own events come back through the
//!   FOLLOWER (state mirror for the GUI + `obs_get`, the leased on-air tally,
//!   the `on_obs_*` hook macros) — see [`ObsFollower`].
//!
//! One process, no IPC. Nothing here reimplements anything: it wires the
//! kernel to the compositor, the registry, and the fps knob the app already
//! had. Runtime toggling leans on the host crate's Drop discipline — tearing
//! down joins the kernel actor, every writer (releasing the only handle to
//! its device), and both accept loops; the boards keep their last latched
//! frame until the app re-applies its own stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use neuron_host::api::{HostApi, LeaseSpec};
use neuron_host::arbiter::{band, BlendMode, Content, LayerId, SourceId};
use neuron_host::bridge::{self, Bridge, CompositorContent};
use neuron_host::bus::Value;
use neuron_host::net::{
    ChromaHttpServer, HostLock, ObsCmd, ObsConnection, ObsControl, OrgbServer, CHROMA_ADDR,
    OBS_ADDR, OPENRGB_ADDR,
};
use neuron_host::shell::{Host, HostHandle};

use neuron::pattern::LayerDef;

/// The app's base lighting on one surface: its arbiter layer, the band it's
/// pinned at, and the layer stack it was built from.
struct BaseLayer {
    layer: LayerId,
    band: i32,
    /// The stack this base renders. Retained so a kernel rebirth (which sweeps
    /// every lease — base is never reborn) can be recovered by re-claiming fresh
    /// `Content::Live` from the SAME defs, without the app's higher layers having
    /// to re-drive it. See [`heartbeat`].
    defs: Vec<LayerDef>,
}

struct HostState {
    handle: HostHandle,
    bridge: Bridge,
    base_owner: SourceId,
    /// surface key → the app's live base layer + the priority band it's pinned
    /// at (which follows the "who wins" policy; tracked so a live policy flip
    /// re-pins rather than silently keeping the old band).
    base: HashMap<String, BaseLayer>,
    // FIELD ORDER IS LOAD-BEARING: Rust drops fields top-to-bottom, so the
    // protocol I/O (which publishes/uses the kernel bus on teardown — the OBS
    // connection publishes obs.connected=false in its Drop) MUST come before
    // `_host` (the kernel actor). Otherwise the kernel would already be joined
    // and the teardown publish would land on a closed channel.
    orgb: Option<OrgbServer>,
    chroma: Option<ChromaHttpServer>,
    /// The native Chroma SHM server (games that paint over shared memory) + the bookkeeping for
    /// its fading game layers (see [`ChromaShm`]). Owns the `Global\` objects the game paints; the
    /// layers hold `Arc` clones, so the mapping is released once those layers are (drop or
    /// `release_owner`). No bus interaction on teardown, so its drop position is not
    /// load-bearing.
    chroma_shm: Option<ChromaShm>,
    /// OBS connection (outbound). Present only while the OBS gate is on.
    obs: Option<ObsConnection>,
    /// The OBS FOLLOWER — the app-side consumer of the `obs.*` bus signals
    /// (state mirror, on-air tally lease, event-hook macros). Lives and dies
    /// with the connection above; its Drop joins the thread, which releases
    /// the tally layers and resets the mirror.
    obs_follow: Option<ObsFollower>,
    // Dropped LAST: the kernel actor outlives every client above so their
    // teardown messages reach a live bus. `dropping this struct IS the teardown`.
    _host: Host,
    /// The machine-wide election token (see [`HostLock`]): held for exactly as long as this
    /// state exists, so releasing it (drop, or the process exiting) is what lets a second
    /// neuron process become the host. No bus interaction, so unlike the fields above its
    /// position in the drop order doesn't matter.
    _lock: HostLock,
}

/// `Mutex<Option<…>>`, not `OnceLock`: the SYSTEM toggle brings the host up
/// and down at runtime. Contention is nil (UI thread + the occasional preview
/// poll); writer threads never touch this — they hold their own channel
/// handles into the kernel.
static HOST: Mutex<Option<HostState>> = Mutex::new(None);

fn guard() -> std::sync::MutexGuard<'static, Option<HostState>> {
    HOST.lock().unwrap_or_else(PoisonError::into_inner)
}

// ── The OBS follower: mirror, tally, hooks ─────────────────────────────────
//
// The connection (net.rs) publishes OBS's own announcements as retained bus
// signals (`obs.scene`, `obs.streaming`, …). This is the app-side subscriber
// that turns those signals into behavior — without it the inbound half of the
// integration would be a radio nobody listens to.

/// The app-side mirror of OBS's announced state. Everything here arrived as an
/// obs-websocket EVENT (or the identify-time resync) — announced by OBS, never
/// assumed. Read by the SYSTEM card and the `obs_get` macro probe; written
/// only by the follower thread; reset to default when the follower stops, so
/// a closed connection can't serve stale truth.
#[derive(Clone, Debug, Default)]
pub struct ObsSnapshot {
    pub connected: bool,
    pub scene: String,
    pub streaming: bool,
    pub recording: bool,
}

static OBS_MIRROR: Mutex<ObsSnapshot> = Mutex::new(ObsSnapshot {
    connected: false,
    scene: String::new(),
    streaming: false,
    recording: false,
});

/// Last-known OBS truth (see [`ObsSnapshot`]).
pub fn obs_snapshot() -> ObsSnapshot {
    OBS_MIRROR.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Keeps [`OBS_MIRROR`] current, feeds the lighting engine's broadcast slot
/// (what the `onair` data layer renders — the user paints WHERE and HOW it
/// shows; this thread only supplies the truth), and fires the `on_obs_*` hook
/// macros. One thread; Drop stops and joins it (which clears both the mirror
/// and the broadcast slot, so nothing downstream can serve a stale "live").
struct ObsFollower {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ObsFollower {
    /// `control` is the connection's command channel — the follower uses it to
    /// demand a full re-announcement (`ObsCmd::Resync`) when the kernel is
    /// reborn underneath it (see [`follow`]).
    fn start(handle: HostHandle, control: ObsControl) -> ObsFollower {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::Builder::new()
            .name("neuron-obs-follow".into())
            .spawn(move || follow(handle, control, &flag))
            .expect("spawn obs follower");
        ObsFollower { stop, thread: Some(thread) }
    }
}

impl Drop for ObsFollower {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Push the mirror's broadcast truth into the lighting engine's slot — the
/// feed the `onair` pattern reads each frame (see `neuron::lighting::Broadcast`).
fn publish_broadcast_from(m: &ObsSnapshot) {
    neuron::lighting::publish_broadcast(neuron::lighting::Broadcast {
        connected: m.connected,
        streaming: m.streaming,
        recording: m.recording,
    });
}

fn follow(handle: HostHandle, control: ObsControl, stop: &AtomicBool) {
    use std::sync::mpsc::RecvTimeoutError;
    // Previous values for the change-only hooks: None until first observed, so
    // the identify-time resync SEEDS state without firing go-live rituals for
    // a stream that was already running when neuron connected.
    let (mut prev_scene, mut prev_stream, mut prev_record) =
        (None::<String>, None::<bool>, None::<bool>);
    let mut rx = handle.subscribe("obs");
    // Set when the subscription dies (kernel rebirth): the reborn bus starts
    // EMPTY — its retained `obs.*` values died with the old kernel — while the
    // websocket to OBS survives, still believing everything is announced. The
    // mirror would keep serving the pre-crash scene/stream/record state until
    // OBS happened to change something. So on the resubscribe that follows,
    // demand a full re-announcement from the SOURCE (`ObsCmd::Resync` →
    // connected + the identify-time trio): the truth is re-read from OBS,
    // never assumed from what we remember.
    let mut reborn = false;
    while !stop.load(Ordering::Relaxed) {
        let sig = match &rx {
            Some(r) => match r.recv_timeout(Duration::from_millis(250)) {
                Ok(s) => Some(s),
                Err(RecvTimeoutError::Timeout) => None,
                // Kernel rebirth closes subscriptions — resubscribe below.
                Err(RecvTimeoutError::Disconnected) => {
                    rx = None;
                    reborn = true;
                    continue;
                }
            },
            None => {
                thread::sleep(Duration::from_millis(250));
                rx = handle.subscribe("obs");
                if rx.is_some() && reborn {
                    reborn = false;
                    control.send(ObsCmd::Resync);
                }
                continue;
            }
        };
        if let Some(sig) = sig {
            // Mirror first (brief lock), then feed the lighting slot, hooks
            // last — the lock never spans a fire.
            let snap = {
                let mut m = OBS_MIRROR.lock().unwrap_or_else(PoisonError::into_inner);
                match (sig.path.as_str(), &sig.value) {
                    ("obs.connected", Value::Bool(b)) => m.connected = *b,
                    ("obs.streaming", Value::Bool(b)) => m.streaming = *b,
                    ("obs.recording", Value::Bool(b)) => m.recording = *b,
                    ("obs.scene", Value::Text(s)) => m.scene = s.clone(),
                    // obs.mute.<input> stays bus-only for now (macros can
                    // obs_request their way to it).
                    _ => {}
                }
                m.clone()
            };
            publish_broadcast_from(&snap);
            match (sig.path.as_str(), &sig.value) {
                ("obs.streaming", Value::Bool(b)) => {
                    hook_on_change("on_obs_stream", &mut prev_stream, *b)
                }
                ("obs.recording", Value::Bool(b)) => {
                    hook_on_change("on_obs_record", &mut prev_record, *b)
                }
                ("obs.scene", Value::Text(s)) => {
                    hook_on_change("on_obs_scene", &mut prev_scene, s.clone())
                }
                _ => {}
            }
        }
    }
    // Teardown: reset the mirror AND the lighting feed so nothing serves a
    // stale truth after we're gone — an `onair` layer goes dark the moment
    // the connection that vouched for it does.
    *OBS_MIRROR.lock().unwrap_or_else(PoisonError::into_inner) = ObsSnapshot::default();
    publish_broadcast_from(&ObsSnapshot::default());
}

/// Fire the named hook macro when a value CHANGES. The first observation only
/// seeds `prev` (see the resync note in [`follow`]). The macro must exist on
/// disk; fired async so the follower never blocks on it; whatever the macro
/// does is arm-gated inside the sidecar like every other fire.
fn hook_on_change<T: PartialEq>(hook: &str, prev: &mut Option<T>, now: T) {
    let changed = prev.as_ref().is_some_and(|p| *p != now);
    *prev = Some(now);
    if changed && neuron::macros::macro_host::load_macro(hook).is_some() {
        let ctx = neuron::macros::Context::capture();
        let _ = neuron::macros::macro_host().fire_async(hook, &ctx);
    }
}

/// The SYSTEM card's honest readout: what's on, what actually BOUND (a taken
/// port — real Synapse, another instance — shows as serving=false even while
/// the gate is on), and how many devices the bridge speaks for.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub active: bool,
    pub devices: usize,
    pub chroma_serving: bool,
    /// The NATIVE (Win32 SHM) Chroma face — neuron being the Chroma server itself:
    /// whether it's serving, and (when a game is on the keys) a pure-telemetry readout
    /// of what it's painting.
    pub chroma_native_serving: bool,
    pub chroma_native_game: Option<NativeChroma>,
    pub openrgb_serving: bool,
    /// OBS gate is on AND a connection object exists (attempting/connected).
    pub obs_on: bool,
    /// OBS has actually authenticated (vs merely attempting). Drives the
    /// "connected to OBS" vs "connecting" copy.
    pub obs_connected: bool,
    /// OBS's own announcements, from the follower's mirror: the current program
    /// scene, and whether the stream / recording are running. Empty/false when
    /// not connected — the card shows what OBS SAID, never a guess.
    pub obs_scene: String,
    pub obs_streaming: bool,
    pub obs_recording: bool,
    /// The LIVE protocol clients, per adapter: Chroma game sessions (TTL-honest)
    /// and OpenRGB tool connections (socket-scoped), each cross-referenced
    /// against the arbiter's claims — so the card can say "Overwatch is
    /// painting your keyboard" from the same leased truth the boards obey.
    pub chroma_clients: Vec<ClientStatus>,
    pub openrgb_clients: Vec<ClientStatus>,
}

/// What the native Chroma (SHM) server sees a game painting right now — pure telemetry
/// (game name, lit device count, effect kind), read straight from the decoded protocol.
#[derive(Clone, Debug, Default)]
pub struct NativeChroma {
    /// The game, resolved from its PID (the Chroma registry leaves the name blank
    /// against our own server); `"a game"` if the PID can't be named.
    pub game: String,
    /// Device classes currently showing a lit frame.
    pub devices: usize,
    /// Effect kind the game is painting (`custom`, `static`, `wave`, …).
    pub effect: String,
}

/// One connected protocol client, as the CONNECTIONS card reads it.
#[derive(Clone, Debug)]
pub struct ClientStatus {
    /// The name it announced (a Chroma init title, an OpenRGB SET_CLIENT_NAME);
    /// "" = connected but never named itself.
    pub name: String,
    /// The surface kinds ("keyboard", "mouse", …) where this client currently
    /// holds the TOPMOST claim — what it is visibly painting right now.
    pub painting: Vec<String>,
    /// It holds at least one claim somewhere (painting OR layered underneath).
    /// false = connected but idle: it hasn't asked to paint anything yet.
    pub has_claim: bool,
}

/// Boot-time start, honoring the persisted preference (default OFF). The
/// `NEURON_HOST` env var stays as a developer override in both directions.
///
/// A failed bring-up is LOGGED but the persisted preference is left INTACT: the pref records the
/// user's INTENT, and a boot-time failure (election lost to a transient second instance, a
/// registry hiccup) shouldn't overwrite intent — the next clean launch should try again. The UI
/// shows the LIVE truth instead: `refresh_host_status` reads `active()`, never the pref, so a
/// failed boot can't paint the Connections toggle green over a dead host.
pub fn start() {
    let on = match std::env::var("NEURON_HOST") {
        Ok(v) if v == "0" || v.eq_ignore_ascii_case("off") => false,
        Ok(v) if v == "1" || v.eq_ignore_ascii_case("on") => true,
        _ => crate::prefs::host_enabled(),
    };
    if on && !bring_up(&mut guard()) {
        eprintln!(
            "neuron-host: boot-time enable failed (election lost or registry error) — \
             connections stay OFF this session; the saved preference is preserved"
        );
    }
}

/// The SYSTEM master toggle. Returns a user-facing status line (house style).
/// After enabling, the caller should re-apply the current lighting so the
/// base layer lands (glue does this); after disabling, likewise — the app's
/// own stream takes back over.
pub fn set_enabled(on: bool) -> String {
    let mut g = guard();
    match (on, g.is_some()) {
        (true, true) => "connections already open".into(),
        (false, false) => "connections already closed".into(),
        (true, false) => {
            if bring_up(&mut g) {
                let s = status_of(&g);
                format!(
                    "connections open — {} device(s), chroma {}, openrgb {}",
                    s.devices,
                    if s.chroma_serving {
                        "serving"
                    } else {
                        "port busy"
                    },
                    if s.openrgb_serving {
                        "serving"
                    } else {
                        "port busy"
                    },
                )
            } else {
                // The pref keeps the user's opt-in (glue persists INTENT, mirroring the boot
                // path), so a transient failure — another neuron holding the election, a
                // registry hiccup — is retried next launch instead of silently un-opting.
                "connections couldn't open (another neuron may own the machine — see log); \
                 staying opted in, will retry next launch"
                    .into()
            }
        }
        (false, true) => {
            // Drop = join kernel + writers + accept loops + the OBS thread.
            // Boards keep their last latched frame until the app re-applies
            // its own stream; the OBS act-verbs go honestly-unavailable.
            neuron::obs_hook::clear_sink();
            *g = None;
            "connections closed".into()
        }
    }
}

/// Re-apply the per-protocol gates while running: bind a newly-enabled server,
/// drop a newly-disabled one, connect/disconnect OBS. No-op when connections
/// are closed.
pub fn apply_protocol_prefs() {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return };
    let want_chroma = crate::prefs::host_chroma();
    let want_orgb = crate::prefs::host_openrgb();
    match (want_chroma, s.chroma.is_some()) {
        (true, false) => s.chroma = ChromaHttpServer::bind(CHROMA_ADDR, s.handle.clone()).ok(),
        (false, true) => s.chroma = None, // Drop joins the accept loop
        _ => {}
    }
    // The native (SHM) face of the same switch: bring the server up, or release its game
    // layers (which drops the last `Arc`, unmapping the objects) — so the game lighting
    // fades away and the user's lighting owns the board again.
    match (want_chroma, s.chroma_shm.is_some()) {
        (true, false) => {
            let mut h = s.handle.clone();
            s.chroma_shm = spawn_chroma_shm(&s.bridge, &mut h);
        }
        (false, true) => {
            if let Some(shm) = s.chroma_shm.take() {
                s.handle.release_owner(shm.src);
            }
        }
        // Serving already, but the merge policy flipped live → re-tint the game layers in
        // place: store the new blend mode into the shared cell they all read. No teardown,
        // so a connected game keeps its shared-memory session; the swap shows next frame.
        (true, true) if s.chroma_shm.as_ref().is_some_and(|c| c.merge != crate::prefs::host_game_merge()) => {
            if let Some(shm) = s.chroma_shm.as_mut() {
                let merge = crate::prefs::host_game_merge();
                shm.blend.store(game_blend_mode(merge).to_bits(), Ordering::Relaxed);
                shm.merge = merge;
            }
        }
        _ => {}
    }
    match (want_orgb, s.orgb.is_some()) {
        (true, false) => s.orgb = OrgbServer::bind(OPENRGB_ADDR, s.handle.clone()).ok(),
        (false, true) => s.orgb = None,
        _ => {}
    }
    match (crate::prefs::host_obs(), s.obs.is_some()) {
        (true, false) => connect_obs(s),
        (false, true) => {
            s.obs = None; // Drop joins the OBS thread
            s.obs_follow = None; // Drop joins the follower (tally released, mirror reset)
            neuron::obs_hook::clear_sink();
        }
        _ => {}
    }
}

/// Re-establish the OBS connection with the current password (called after the
/// password field changes, so a corrected secret takes effect without toggling
/// OBS off and on). No-op unless OBS is currently on.
pub fn reconnect_obs() {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return };
    if s.obs.is_some() {
        s.obs = None; // drop the old connection (joins its thread)
        s.obs_follow = None; // and its follower (tally released, mirror reset)
        neuron::obs_hook::clear_sink();
        connect_obs(s);
    }
}

/// Bring up the OBS connection, install the act-verb forwarder (so any macro's
/// `obs_*` verb reaches OBS), and start the follower (so OBS's own events reach
/// the mirror, the tally, and the `on_obs_*` hook macros). The obs-websocket
/// password comes from prefs; `NEURON_OBS_PASSWORD` overrides (blank = a
/// passwordless OBS — the common local default).
fn connect_obs(s: &mut HostState) {
    let pw = crate::prefs::host_obs_password();
    let conn = ObsConnection::start(OBS_ADDR, &pw, s.handle.clone());
    let control = conn.control();
    neuron::obs_hook::set_sink(Box::new(move |verb, arg| {
        // ── SENSE: `obs_get(probe)` reads the follower's mirror — no websocket
        // round-trip, and honest: everything in it was ANNOUNCED by OBS
        // (resynced at identify), never assumed. Reads are never arm-gated.
        if verb == "obs_get" {
            let m = obs_snapshot();
            let probe = arg.as_str().unwrap_or("scene");
            if probe == "connected" {
                return (true, m.connected.to_string());
            }
            if !m.connected {
                return (false, "OBS not connected".into());
            }
            return match probe {
                "scene" => (true, m.scene),
                "streaming" => (true, m.streaming.to_string()),
                "recording" => (true, m.recording.to_string()),
                other => (
                    false,
                    format!(
                        "obs_get: unknown probe '{other}' (scene|streaming|recording|connected)"
                    ),
                ),
            };
        }
        let a = arg.as_str().unwrap_or("toggle");
        let (cmd, msg): (ObsCmd, String) = match verb {
            "obs_scene" => match arg.as_str() {
                Some(name) if !name.is_empty() => {
                    (ObsCmd::SetScene(name.into()), format!("obs scene → {name}"))
                }
                _ => {
                    return (
                        false,
                        "obs_scene(name): name must be a non-empty string".into(),
                    )
                }
            },
            "obs_stream" => match a {
                "start" => (ObsCmd::StartStream, "obs stream → start".into()),
                "stop" => (ObsCmd::StopStream, "obs stream → stop".into()),
                _ => (ObsCmd::ToggleStream, "obs stream → toggle".into()),
            },
            "obs_record" => match a {
                "start" => (ObsCmd::StartRecord, "obs record → start".into()),
                "stop" => (ObsCmd::StopRecord, "obs record → stop".into()),
                // pause ⇄ resume the running recording (obs-websocket's own toggle)
                "pause" | "resume" => (
                    ObsCmd::Raw {
                        request_type: "ToggleRecordPause".into(),
                        data: String::new(),
                    },
                    "obs record → pause/resume".into(),
                ),
                _ => (ObsCmd::ToggleRecord, "obs record → toggle".into()),
            },
            // The replay buffer — the "clip that!" button. Blank/save = write the clip to disk;
            // start/stop run the buffer itself (it must be running before a save can land).
            "obs_replay" => match a {
                "start" => (
                    ObsCmd::Raw {
                        request_type: "StartReplayBuffer".into(),
                        data: String::new(),
                    },
                    "obs replay → start".into(),
                ),
                "stop" => (
                    ObsCmd::Raw {
                        request_type: "StopReplayBuffer".into(),
                        data: String::new(),
                    },
                    "obs replay → stop".into(),
                ),
                _ => (
                    ObsCmd::Raw {
                        request_type: "SaveReplayBuffer".into(),
                        data: String::new(),
                    },
                    "obs replay → save clip".into(),
                ),
            },
            "obs_mute" => {
                let name = arg.as_str().unwrap_or("Mic/Aux");
                (
                    ObsCmd::ToggleMute(name.into()),
                    format!("obs mute → {name}"),
                )
            }
            // The whole obs-websocket API, no new code per request: a bare
            // request-type string, or {"type": ..., "data": {...}}. Requests
            // are fire-and-forget (responses don't route back to the macro).
            "obs_request" => {
                let (rt, data) = match (arg.as_str(), arg.get("type").and_then(|t| t.as_str())) {
                    (Some(t), _) => (t.to_string(), String::new()),
                    (None, Some(t)) => (
                        t.to_string(),
                        arg.get("data").map(|d| d.to_string()).unwrap_or_default(),
                    ),
                    _ => (String::new(), String::new()),
                };
                if rt.is_empty() {
                    return (
                        false,
                        r#"obs_request: pass "RequestType" or {"type": ..., "data": {...}}"#
                            .into(),
                    );
                }
                let m = format!("obs request → {rt}");
                (ObsCmd::Raw { request_type: rt, data }, m)
            }
            other => return (false, format!("unknown obs verb '{other}'")),
        };
        // HONEST CONTROL: the sink is installed while the OBS gate is on, but the
        // gate being on doesn't mean OBS is actually there — it may be closed,
        // misconfigured, or still reconnecting, in which case `control.send` drops
        // the command (the reconnect loop even drains its queue). Gate on the same
        // announced-connection truth the `obs_get` sense path reads, so a control
        // verb reports failure when nothing was sent instead of a false "→ start".
        if !obs_snapshot().connected {
            return (
                false,
                "OBS not connected (open SYSTEM \u{2192} CONNECTIONS)".into(),
            );
        }
        control.send(cmd);
        (true, msg)
    }));
    // The follower gets the connection's control channel so a kernel rebirth can
    // demand a fresh resync from OBS (the reborn bus starts empty — see `follow`).
    let follow_control = conn.control();
    s.obs = Some(conn);
    s.obs_follow = Some(ObsFollower::start(s.handle.clone(), follow_control));
}

/// The live native-Chroma face: the server handle (kept alive so the `Global\` objects
/// persist) and the ONE source id all its fading game layers are claimed under, so a
/// runtime toggle can release them as a unit. `Arc<ShmServer>` on Windows; a zero-sized
/// stand-in elsewhere so the [`HostState`] field type stays uniform.
#[cfg(windows)]
type ChromaShmHandle = std::sync::Arc<neuron_host::adapters::chroma_shm::server::ShmServer>;
#[cfg(not(windows))]
type ChromaShmHandle = std::sync::Arc<()>;

/// One native-Chroma game layer's re-claim recipe: enough to rebuild an identical
/// [`ChromaShmLayer`] when its lease lapses (idle expiry or a kernel rebirth), keyed to the
/// surface it paints. `id` tracks the live arbiter layer for [`refresh`](HostApi::refresh).
struct ShmLayer {
    key: String,
    device_type: u8,
    leds: usize,
    id: LayerId,
}

/// The native-Chroma face's live state. The game layers are **Heartbeat-leased**, NOT Pinned:
/// [`refresh_chroma_shm`] pushes their deadline out each host tick while a game is connected
/// (plus the fade-out grace, plus the warm pre-first-game window). Once a game that was live has
/// been gone longer than that grace, the host stops refreshing — the leases lapse and the arbiter
/// sweeps the layers, so the user's base lighting returns **structurally** (the crate's one
/// "no stuck lighting" rule), never by trust in the layer's own alpha ramp. The ramp stays as
/// the cosmetic crossfade; the lease is the safety net.
struct ChromaShm {
    handle: ChromaShmHandle,
    src: SourceId,
    /// Per-surface game layers (see [`ShmLayer`]) — empty off Windows.
    layers: Vec<ShmLayer>,
    /// The last tick a game was observed connected. Drives the fade-out grace: the host keeps
    /// refreshing until this ages past [`SHM_FADE_GRACE`], letting the crossfade finish before
    /// the leases are allowed to lapse. `None` = no game seen yet.
    last_live: Option<Instant>,
    /// The blend policy the layers were claimed with (`host_game_merge`): `true` = merge
    /// (screen), `false` = take over. Tracked so a live policy flip is detected as a change.
    merge: bool,
    /// The live blend mode, shared with every game layer (see [`ChromaShmLayer`]). Flipping
    /// the merge policy stores a new value here — the layers re-tint on the next frame with
    /// no server teardown, so a connected game keeps its shared-memory session unbroken.
    blend: Arc<AtomicU8>,
}

/// The Heartbeat TTL on each game layer. This lease is refreshed ONLY by the app's UI-thread
/// heartbeat (~1s), which a fullscreen game occluding neuron's window can stall for seconds at a
/// time. A too-short TTL then lapses a STILL-LIVE game: the arbiter sweeps its layer, the surface
/// briefly resolves to the base (a visible full-board dip), then the next heartbeat re-claims —
/// the periodic in-game flash. So the TTL is generous (survives realistic UI-scheduling jitter);
/// the ≈0.45s alpha fade — not the lease — does the cosmetic teardown when a game genuinely
/// leaves, and once the host stops refreshing (game gone past the grace) an alpha-0 layer paints
/// nothing while it lingers, so a longer structural backstop costs only a few seconds of a
/// dormant, invisible layer. See also the alpha-preserving re-claim in `refresh_chroma_shm`.
const SHM_LEASE_TTL: Duration = Duration::from_secs(12);
/// Keep refreshing the game leases this long after a game was last seen, so the layer's own
/// crossfade-out completes before the leases lapse. Comfortably shorter than [`SHM_LEASE_TTL`].
const SHM_FADE_GRACE: Duration = Duration::from_millis(1200);

/// Stand up the native Chroma SHM server and claim a self-fading game layer per bridged
/// surface at `band::SESSION`, all under ONE source. CREATE-ONLY on purpose: we become
/// the sole Chroma server or we stand down — we never read alongside a live Razer server,
/// which would double-write the LEDs. Returns `None` silently when disabled, unelevated,
/// or Razer owns the objects; the layers lie dormant (alpha 0, the user's lighting
/// untouched) until a game connects, then crossfade in. Windows-only; a stub elsewhere.
///
/// The layers are **Heartbeat-leased** (see [`ChromaShm`]) and claimed alive now, so a game
/// that connects immediately paints without waiting for the first host tick; [`refresh_chroma_shm`]
/// then keeps them alive only while a game is present.
/// The blend mode a game layer paints with under the `host_game_merge` policy: merge →
/// `Screen` (the game's light ADDS over your base — bright keys punch through, your
/// lighting stays beneath), else `Over` (the game replaces the keys it paints).
fn game_blend_mode(merge: bool) -> BlendMode {
    if merge {
        BlendMode::Screen
    } else {
        BlendMode::Over
    }
}

#[cfg(windows)]
fn spawn_chroma_shm(bridge: &Bridge, h: &mut HostHandle) -> Option<ChromaShm> {
    use neuron_host::adapters::chroma_shm::server::{ChromaShmLayer, ShmServer};
    use neuron_host::api::SurfaceKind;
    use std::sync::Arc;

    let server: ChromaShmHandle = Arc::new(ShmServer::create().ok()?);
    let src = h.next_source();
    let now = Instant::now();
    let merge = crate::prefs::host_game_merge();
    // One shared blend cell for every device layer, so a live merge-policy flip re-tints
    // them all with a single store — no teardown of the shared-memory objects.
    let blend = Arc::new(AtomicU8::new(game_blend_mode(merge).to_bits()));
    let mut layers = Vec::new();
    for surf in &bridge.surfaces {
        let device_type = match surf.kind {
            SurfaceKind::Keyboard => 0x01,
            SurfaceKind::Mouse => 0x02,
            SurfaceKind::Headset => 0x04,
            SurfaceKind::Mousepad => 0x08,
            SurfaceKind::Keypad => 0x10,
            SurfaceKind::Generic => 0x80,
        };
        // First claim: start faded OUT (0.0) so the game crossfades IN over the base when it
        // first connects — the never-live layer paints nothing until a game appears.
        let layer =
            ChromaShmLayer::new(Arc::clone(&server), device_type, surf.leds, Arc::clone(&blend), 0.0);
        if let Some(id) = h.claim(
            &surf.key,
            src,
            band::SESSION,
            LeaseSpec::Ttl(SHM_LEASE_TTL),
            Content::Live(Box::new(layer)),
            now,
        ) {
            layers.push(ShmLayer {
                key: surf.key.clone(),
                device_type,
                leds: surf.leds,
                id,
            });
        }
    }
    Some(ChromaShm { handle: server, src, layers, last_live: None, merge, blend })
}

#[cfg(not(windows))]
fn spawn_chroma_shm(_bridge: &Bridge, _h: &mut HostHandle) -> Option<ChromaShm> {
    None
}

/// Keep the native-Chroma game layers honest against the arbiter's lease model. Each host tick
/// it refreshes their Heartbeat leases while a game is live (or within the fade-out grace after
/// one left, or before any game has yet connected — see `keep` below), and re-claims any that a
/// lease lapse or kernel rebirth swept. Once a game that WAS live has been gone past the grace,
/// the host stops refreshing entirely: the leases lapse, the arbiter sweeps the layers, and the
/// user's base returns — structurally, not by trusting the layer's own alpha ramp. This is the
/// invariant the arbiter states in one line: everything session-shaped keeps proving it exists
/// (here, by the game's live PID) or it goes. Windows-only; a no-op stub elsewhere.
#[cfg(windows)]
fn refresh_chroma_shm(s: &mut HostState, now: Instant) {
    use neuron_host::adapters::chroma_shm::server::ChromaShmLayer;
    use std::sync::Arc;
    let Some(shm) = s.chroma_shm.as_mut() else { return };
    // Is a native game painting RIGHT NOW? Drives both liveness (below) and the alpha a
    // re-claimed layer is born at (a swept-mid-paint layer must NOT restart its fade-in).
    let present = shm.handle.any_client_connected();
    if present {
        shm.last_live = Some(now);
    }
    // Whether to keep the leases alive this tick:
    //   • no game has EVER connected (`last_live` None) → keep warm. A never-live layer paints
    //     nothing (its alpha is provably 0 with no game input to decode), so holding its lease
    //     is harmless AND means a game that connects later shows instantly, with no expire/
    //     re-claim race flashing the base mid-connect.
    //   • a game is here or left within the fade grace → keep, so the ≈0.45s crossfade finishes.
    // Otherwise (a game WAS live and is now gone past the grace) stop refreshing: the leases
    // lapse and the arbiter sweeps the layers. That is exactly the dangerous "stale last frame"
    // case — a game painted, then left — and it's resolved structurally, not by the alpha ramp.
    let keep = match shm.last_live {
        None => true,
        Some(t) => now.duration_since(t) < SHM_FADE_GRACE,
    };
    if !keep {
        return;
    }
    // Clone the handle/src out so the layer loop can borrow `shm.layers` mutably without
    // aliasing the fields it reads.
    let handle = Arc::clone(&shm.handle);
    let src = shm.src;
    let blend = Arc::clone(&shm.blend);
    let mut h = s.handle.clone();
    for l in &mut shm.layers {
        // A live refresh pushes the deadline out; `false` = the layer was swept (lease lapsed
        // during a stall, or a kernel rebirth) → re-claim an identical one from its recipe,
        // sharing the same live blend cell so it tracks the current merge policy.
        if !h.refresh(l.id, now) {
            // Born at the CURRENT on-screen alpha: 1.0 if a game is live (it was swept
            // mid-paint by a UI-stall lease lapse — DON'T dip the board to black and fade it
            // back), 0.0 otherwise (fading out, or a never-live kernel-rebirth re-claim).
            let layer = ChromaShmLayer::new(
                Arc::clone(&handle),
                l.device_type,
                l.leds,
                Arc::clone(&blend),
                if present { 1.0 } else { 0.0 },
            );
            if let Some(id) = h.claim(
                &l.key,
                src,
                band::SESSION,
                LeaseSpec::Ttl(SHM_LEASE_TTL),
                Content::Live(Box::new(layer)),
                now,
            ) {
                l.id = id;
            }
        }
    }
}

#[cfg(not(windows))]
fn refresh_chroma_shm(_s: &mut HostState, _now: Instant) {}

fn bring_up(g: &mut Option<HostState>) -> bool {
    // THE machine-wide ownership check, ahead of everything else: the protocol ports are the
    // wrong token for it (a Chroma/OpenRGB squatter — usually real Synapse — should only take
    // down that one adapter), but TWO NEURON HOSTS both attaching device writers is exactly the
    // double-writer race this crate exists to kill. If another neuron process already holds the
    // election, nothing below may run: no bridge, no writers, no adapters.
    let Some(lock) = HostLock::acquire() else {
        eprintln!(
            "neuron-host: another neuron host owns this machine (election port busy) — host disabled, no device writers started"
        );
        return false;
    };
    let Ok(reg) = neuron::registry::Registry::load() else {
        return false;
    };
    let host = Host::spawn();
    let handle = host.handle();
    let mut h = host.handle();
    let base_owner = h.next_source();
    let bridge = bridge::attach(&reg, &handle, 30);

    // Per-protocol gates; a failed bind here means a squatter on THAT protocol only (often real
    // Synapse) — a second neuron instance is already excluded by the election above. Each
    // adapter degrades independently to serving=false, shown honestly in the SYSTEM card, never
    // a silent green light.
    let chroma = crate::prefs::host_chroma()
        .then(|| ChromaHttpServer::bind(CHROMA_ADDR, host.handle()).ok())
        .flatten();
    let orgb = crate::prefs::host_openrgb()
        .then(|| OrgbServer::bind(OPENRGB_ADDR, host.handle()).ok())
        .flatten();
    // The NATIVE face of the same "chroma (games)" feature: native games speak Win32
    // shared memory, not REST, so serving them means BEING the SHM server — gated by the
    // same `host_chroma` switch as the REST face above.
    let chroma_shm = crate::prefs::host_chroma()
        .then(|| spawn_chroma_shm(&bridge, &mut h))
        .flatten();

    eprintln!(
        "neuron-host: connections open — {} device(s) bridged, chroma={} (rest={}, native={}), openrgb={}, obs={}",
        bridge.surfaces.len(),
        chroma.is_some() || chroma_shm.is_some(),
        chroma.is_some(),
        chroma_shm.is_some(),
        orgb.is_some(),
        crate::prefs::host_obs(),
    );
    let mut state = HostState {
        handle,
        bridge,
        base_owner,
        base: HashMap::new(),
        _host: host,
        orgb,
        chroma,
        chroma_shm,
        obs: None,
        obs_follow: None,
        _lock: lock,
    };
    if crate::prefs::host_obs() {
        connect_obs(&mut state);
    }
    *g = Some(state);
    true
}

/// Pull the native Chroma face's live telemetry off the SHM server for the readout:
/// `(serving, what a game is painting)`. Windows-only (the server is); a no-op stub
/// elsewhere so [`Status`] stays platform-uniform.
#[cfg(windows)]
fn native_chroma_status(chroma_shm: &Option<ChromaShm>) -> (bool, Option<NativeChroma>) {
    match chroma_shm {
        Some(shm) => {
            let server = &shm.handle;
            let game = server
                .any_client_connected()
                .then(|| server.live_summary())
                .flatten()
                .map(|(devices, effect)| {
                    let game = server
                        .registered_apps()
                        .first()
                        .and_then(|a| crate::purge::process_name(a.id))
                        .unwrap_or_else(|| "a game".to_string());
                    NativeChroma { game, devices, effect: effect.to_lowercase() }
                });
            (true, game)
        }
        None => (false, None),
    }
}

#[cfg(not(windows))]
fn native_chroma_status(_chroma_shm: &Option<ChromaShm>) -> (bool, Option<NativeChroma>) {
    (false, None)
}

fn status_of(g: &Option<HostState>) -> Status {
    match g {
        Some(s) => {
            let obs = obs_snapshot();
            let now = Instant::now();
            // One claims read per surface, shared by every client lookup below.
            let boards: Vec<(&'static str, Vec<neuron_host::shell::Claim>)> = s
                .bridge
                .surfaces
                .iter()
                .map(|i| (kind_word(i.kind), s.handle.claims(&i.key, now)))
                .collect();
            let chroma_clients = s
                .chroma
                .as_ref()
                .map(|c| client_statuses(c.sessions(), &boards))
                .unwrap_or_default();
            let openrgb_clients = s
                .orgb
                .as_ref()
                .map(|o| client_statuses(o.clients(), &boards))
                .unwrap_or_default();
            let (chroma_native_serving, chroma_native_game) = native_chroma_status(&s.chroma_shm);
            Status {
                active: true,
                devices: s.bridge.surfaces.len(),
                chroma_serving: s.chroma.is_some(),
                chroma_native_serving,
                chroma_native_game,
                openrgb_serving: s.orgb.is_some(),
                obs_on: s.obs.is_some(),
                obs_connected: s.obs.as_ref().is_some_and(|c| c.is_connected()),
                obs_scene: obs.scene,
                obs_streaming: obs.streaming,
                obs_recording: obs.recording,
                chroma_clients,
                openrgb_clients,
            }
        }
        None => Status::default(),
    }
}

/// Cross-reference an adapter's client roster against the arbiter's claims:
/// a client is PAINTING a surface when it holds the topmost claim there (the
/// same ordering `resolve` composites by), and merely PRESENT when it has any
/// claim at all (layered underneath — the who-wins suppressed case).
fn client_statuses(
    roster: Vec<(SourceId, String)>,
    boards: &[(&'static str, Vec<neuron_host::shell::Claim>)],
) -> Vec<ClientStatus> {
    roster
        .into_iter()
        .map(|(owner, name)| {
            let mut painting: Vec<String> = Vec::new();
            let mut has_claim = false;
            for (kind, claims) in boards {
                if claims.iter().any(|c| c.owner == owner) {
                    has_claim = true;
                }
                if claims.first().is_some_and(|c| c.owner == owner)
                    && !painting.iter().any(|k| k == kind)
                {
                    painting.push((*kind).to_string());
                }
            }
            ClientStatus {
                name,
                painting,
                has_claim,
            }
        })
        .collect()
}

/// The user's word for a surface kind (the CONNECTIONS card speaks devices,
/// not enum variants).
fn kind_word(kind: neuron_host::api::SurfaceKind) -> &'static str {
    use neuron_host::api::SurfaceKind;
    match kind {
        SurfaceKind::Keyboard => "keyboard",
        SurfaceKind::Mouse => "mouse",
        SurfaceKind::Mousepad => "mousepad",
        SurfaceKind::Headset => "headset",
        SurfaceKind::Keypad => "keypad",
        SurfaceKind::Generic => "device",
    }
}

/// The SYSTEM card readout.
pub fn status() -> Status {
    status_of(&guard())
}

/// Is the host live (i.e. should the app route lighting through it rather
/// than stream on its own anim thread)?
pub fn active() -> bool {
    guard().is_some()
}

/// Resolve which surface keys an operation addressed at `(pid, unit)` touches.
/// A named unit resolves to EXACTLY its own surface — or nothing if that unit isn't bridged;
/// falling back to a pid sibling would silently retarget a different physical device. An empty
/// unit is a pid-level operation (per-model config with no unit in hand) and fans out to every
/// surface the pid maps to, applied to each unit explicitly.
fn surface_keys(bridge: &neuron_host::bridge::Bridge, pid: u16, unit: &str) -> Vec<String> {
    if !unit.is_empty() {
        return bridge
            .key_for_unit(unit)
            .map(|k| vec![k.clone()])
            .unwrap_or_default();
    }
    bridge.keys_for_pid(pid).to_vec()
}

/// Does the host currently hold a base-lighting layer for this board?
/// (Drives the app's "compositing" indicator without the app owning a stream.)
/// `unit` names the physical board; empty = "any of the pid's boards".
pub fn has_lighting(pid: u16, unit: &str) -> bool {
    let g = guard();
    let Some(s) = g.as_ref() else { return false };
    surface_keys(&s.bridge, pid, unit)
        .iter()
        .any(|key| s.base.contains_key(key))
}

/// Who is actually in control of a board right now — the ownership truth no
/// last-writer-wins tool can even represent, because only an arbiter has the
/// concept of "connected but suppressed".
#[derive(Clone, Debug, PartialEq)]
pub enum BoardOwner {
    /// Your own lighting is showing (no foreign claim, or one that lost).
    Yours,
    /// A named client is painting ON TOP of your base right now.
    Painting(String),
    /// A named client is connected but your base wins (the "my lighting always
    /// wins" policy) — it's here, and it's honestly not showing.
    Suppressed(String),
}

/// The LIGHTING page's truth, in one call. Walks the arbiter's claims for the
/// selected board (topmost-first) and reports whether the winner is yours or a
/// foreign client — and, distinctly, whether a foreign client is present but
/// LOST to your base (policy = my lighting wins). Names come from the kernel's
/// per-owner labels, set by the adapter the instant the client announced
/// itself (a Chroma session's title, an OpenRGB client's name); an unnamed
/// client reads as "another app".
pub fn board_owner(pid: u16, unit: &str) -> BoardOwner {
    let g = guard();
    let Some(s) = g.as_ref() else {
        return BoardOwner::Yours;
    };
    let name = |c: &neuron_host::shell::Claim| {
        c.label
            .clone()
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "another app".into())
    };
    let now = Instant::now();

    // A named unit reads ITS OWN surface's truth — the selected board's strip must not report
    // an identical twin's foreign claim as if it were here. A pid-level read (empty unit) scans
    // every surface and reports the most urgent finding: Painting outranks Suppressed outranks
    // Yours.
    let mut best = BoardOwner::Yours;
    for key in surface_keys(&s.bridge, pid, unit) {
        let claims = s.handle.claims(&key, now);
        // The FIRST foreign claim. Claims are sorted winner-first (priority, seq desc),
        // so everything ABOVE it in the list is necessarily OURS (the base owner) — that
        // is what "first foreign" means. Its POSITION is the truth, read from the LIVE
        // arbiter, never from a remembered band: after a kernel rebirth the base lease is
        // swept while `s.base` still holds its old band, so trusting that band would call
        // a foreign claim "suppressed" when nothing of ours is actually live above it.
        let Some(idx) = claims.iter().position(|c| c.owner != s.base_owner) else {
            continue; // no foreign claim → your lighting owns this surface
        };
        let top = &claims[idx];
        // idx == 0: the foreign claim is the overall winner — no live base sits above it
        // (this is exactly the rebirth gap, before `heartbeat` re-claims the base), so it
        // IS painting. idx > 0: a live base claim is above it → suppressed by the "my
        // lighting wins" policy.
        let status = if idx == 0 {
            BoardOwner::Painting(name(top))
        } else {
            BoardOwner::Suppressed(name(top))
        };
        if matches!(status, BoardOwner::Painting(_)) {
            return status; // nothing else can outrank this
        }
        if matches!(best, BoardOwner::Yours) {
            best = status;
        }
    }
    best
}

/// An exclusive window over one device's feature-report channel: while this
/// guard lives, the pid's host writer is PARKED (frames stop) so a transient
/// open — the DEVICE page's getter sweep, a read-back-verified setter, the
/// backup snapshot — can't have its replies clobbered by streaming lighting
/// writes. That race is what left every readout at "—" while lighting
/// streamed: the writer's `set_feature` overwrote the getter's pending reply
/// and every read timed out. Frames resume on drop (the writer re-bases its
/// deadline — no burst). `None` when the host is off or the device isn't
/// bridged: no stream, no race, nothing to gate.
///
/// Holds every writer pauser gated by one guard — usually one, but a pid that maps to two
/// identical devices (see `Bridge::keys_for_pid`) parks BOTH. That's a deliberate superset:
/// parking a twin's writer for the duration of a read costs a few skipped frames, while a
/// too-narrow gate that guessed wrong would re-introduce the reply-clobber race. Reads stay
/// pid-gated even though writes are unit-addressed.
pub struct IoGate(Vec<neuron_host::writer::WriterPauser>);

impl Drop for IoGate {
    fn drop(&mut self) {
        for p in &self.0 {
            p.release();
        }
    }
}

pub fn io_gate(pid: u16) -> Option<IoGate> {
    let pausers: Vec<_> = {
        let g = guard();
        let s = g.as_ref()?;
        s.bridge
            .keys_for_pid(pid)
            .iter()
            .filter_map(|k| s.bridge.pauser(k))
            .collect()
    };
    if pausers.is_empty() {
        return None;
    }
    // engage OUTSIDE the HOST lock — the wait is bounded (~100ms) but the UI
    // thread polls status through this mutex; never hold it while waiting.
    for p in &pausers {
        p.engage();
    }
    Some(IoGate(pausers))
}

/// Gate EVERY bridged device at once — for whole-fleet reads (the profile
/// capture, the backup sweep) that walk devices core-side where per-pid
/// gating can't reach.
pub fn io_gate_all() -> Vec<IoGate> {
    let pausers: Vec<_> = {
        let g = guard();
        match g.as_ref() {
            Some(s) => s
                .bridge
                .surfaces
                .iter()
                .filter_map(|i| s.bridge.pauser(&i.key))
                .collect(),
            None => Vec::new(),
        }
    };
    pausers
        .into_iter()
        .map(|p| {
            p.engage();
            IoGate(vec![p])
        })
        .collect()
}

/// Push the app's lighting stack as the animated base layer, at `fps`. `unit` names the ONE
/// physical board the stack targets (the app's device model is unit-keyed; each board's stream is
/// applied to exactly that board's surface). An empty unit is the pid-level form — per-model
/// config with no unit in hand — and fans the SAME stack out to every surface the pid maps to,
/// each unit claimed explicitly. Set-or-claim per surface so re-applying re-uses the same layer
/// (no flicker); each surface's fps atomic is its own shared writer pace — one knob drives render
/// AND write cadence, exactly like the app's own streams. `false` only if the host is inactive or
/// the addressed board isn't bridged (caller falls back to a local stream); a partial failure
/// across a pid-level fan-out still returns true if at least one landed.
pub fn set_lighting(pid: u16, unit: &str, defs: Vec<LayerDef>, fps: u32) -> bool {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return false };
    let keys: Vec<String> = surface_keys(&s.bridge, pid, unit);
    if keys.is_empty() {
        return false;
    }

    // The user's lighting is always the BASE layer; games sit ABOVE it at SESSION and
    // combine per the merge policy (see `game_blend_mode`), never by band. (The band is
    // still tracked per base layer for the kernel-rebirth recovery path.)
    let want_band = band::BASE;
    let now = Instant::now();
    let mut any = false;
    for key in keys {
        let Some((rows, cols)) = s.bridge.grid_of(&key) else {
            continue;
        };
        let Some(pace) = s.bridge.pace(&key) else {
            continue;
        };
        pace.store(
            fps.clamp(1, neuron::lighting::MAX_STREAM_FPS),
            std::sync::atomic::Ordering::Relaxed,
        );
        let content = Content::Live(Box::new(CompositorContent::new(
            defs.clone(),
            rows,
            cols,
            pace,
        )));
        let mut h = s.handle.clone();
        if let Some(existing) = s.base.get(&key) {
            // Reuse the layer (no flicker) only if it's already at the right band;
            // a band change or a vanished layer (kernel rebirth) falls through to
            // a fresh claim.
            if existing.band == want_band && h.set_content(existing.layer, content.clone(), now) {
                // Keep the retained defs current with what's actually rendering,
                // so a later rebirth recovery rebuilds THIS stack, not a stale one.
                s.base.get_mut(&key).expect("just matched").defs = defs.clone();
                any = true;
                continue;
            }
            let old = s.base.remove(&key).expect("just matched");
            h.release(old.layer);
        }
        if let Some(layer) = h.claim(
            &key,
            s.base_owner,
            want_band,
            LeaseSpec::Pinned,
            content,
            now,
        ) {
            s.base.insert(
                key,
                BaseLayer {
                    layer,
                    band: want_band,
                    defs: defs.clone(),
                },
            );
            any = true;
        }
    }
    any
}

/// Live re-pace a board's base (the fps slider), no restart. `unit` names the board; empty
/// re-paces every surface the pid maps to. Mirrors `AppRuntime::set_anim_fps`.
pub fn set_fps(pid: u16, unit: &str, fps: u32) {
    let g = guard();
    if let Some(s) = g.as_ref() {
        for key in surface_keys(&s.bridge, pid, unit) {
            s.bridge.set_fps(&key, fps);
        }
    }
}

/// Drop a board's base layer — the board goes unclaimed and the writer leaves its last frame
/// latched (onboard-first). Used when the app clears a stack. `unit` names the board; empty
/// drops every surface the pid maps to.
pub fn clear_lighting(pid: u16, unit: &str) {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return };
    let keys: Vec<String> = surface_keys(&s.bridge, pid, unit);
    for key in keys {
        if let Some(b) = s.base.remove(&key) {
            s.handle.clone().release(b.layer);
        }
    }
}

/// Drop EVERY board's base layer — the host-side twin of [`clear_lighting`] for the app's global
/// stop path (writes-paused kill-switch / Observe stance). Without this, a board composited
/// through the host would be invisible to "stop every stream": its base layer would stay claimed
/// and the writer would keep painting while local streams were all cut. No-op when the host is
/// inactive (nothing owns the base).
pub fn clear_all_lighting() {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return };
    let mut h = s.handle.clone();
    for (_key, b) in s.base.drain() {
        h.release(b.layer);
    }
}

/// Does the host hold a base-lighting layer for ANY board? The global twin of [`has_lighting`],
/// so [`crate::runtime::AppRuntime::any_animating`] can count host-composited boards as live even
/// though the app owns no local writer for them.
pub fn any_lighting() -> bool {
    let g = guard();
    g.as_ref().is_some_and(|s| !s.base.is_empty())
}

/// Recover the app's base lighting after a contained kernel rebirth. A kernel
/// fault is caught and the actor is reborn from its surface seed, but EVERY lease
/// is swept — leases are never reborn, sessions must re-claim (the app is a
/// session here, `base_owner`). So the app's animated base layers silently vanish
/// from the arbiter while the device writers keep running and leave the last
/// frame latched: local lighting freezes until something forces a fresh
/// `set_lighting`. This is that force. For each tracked base layer it probes
/// liveness — a `refresh` on a Pinned lease is a pure existence check (`false` =
/// the layer is gone) — and re-claims any that vanished from the RETAINED defs,
/// rebuilding an identical `Content::Live` so the base resumes on its own.
///
/// Called ~1s from the app heartbeat. A no-op when the host is off or every base
/// is still alive (the common case: one cheap round-trip per base surface). A
/// rebirth in progress makes the probe block until the reborn kernel answers —
/// the same brief, once-per-fault stall any host call sees during rebirth.
pub fn heartbeat() {
    let mut g = guard();
    let Some(s) = g.as_mut() else { return };
    let now = Instant::now();
    let mut h = s.handle.clone();
    // Base layers whose kernel claim no longer exists — swept by a rebirth.
    let dead: Vec<String> = s
        .base
        .iter()
        .filter(|(_, b)| !h.refresh(b.layer, now))
        .map(|(k, _)| k.clone())
        .collect();
    for key in dead {
        // Drop the stale bookkeeping first, then re-establish from the retained
        // defs — same stack, same band, a fresh compositor. Reuses the exact
        // claim path `set_lighting` uses; the per-surface fps atomic persists in
        // the bridge, so pace carries across the rebirth untouched.
        let Some(old) = s.base.remove(&key) else { continue };
        let Some((rows, cols)) = s.bridge.grid_of(&key) else { continue };
        let Some(pace) = s.bridge.pace(&key) else { continue };
        let content = Content::Live(Box::new(CompositorContent::new(
            old.defs.clone(),
            rows,
            cols,
            pace,
        )));
        if let Some(layer) =
            h.claim(&key, s.base_owner, old.band, LeaseSpec::Pinned, content, now)
        {
            s.base.insert(
                key,
                BaseLayer { layer, band: old.band, defs: old.defs },
            );
        }
    }
    // The native-Chroma game layers ride the SAME tick: refresh their Heartbeat leases while a
    // game is live (or fading out), re-claim any a rebirth swept, and let them lapse — base back —
    // once no game remains. This is what keeps the SESSION-band game layer inside the arbiter's
    // "session-shaped ⇒ Heartbeat" invariant rather than pinned-forever.
    refresh_chroma_shm(s, now);
}
