// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

//! The socket pump — deliberately dumb I/O at the very edge.
//!
//! All protocol intelligence lives in the pure adapter codecs; this module
//! only moves bytes between sockets and [`OrgbConn::feed`]. That split is the
//! whole testing strategy: everything interesting was already proven without
//! a socket, so the pump can be boring — accept, read, feed, write, and on
//! ANY exit call `disconnected()` so the connection's paint footprint releases
//! (the OpenRGB equivalent of a heartbeat lapse; teardown stays the default
//! path even when a client is kill -9'd).
//!
//! Binding is loopback-only by design (local-first: the network surface of
//! the host is the machine's own processes unless the user explicitly asks
//! for LAN exposure). A failed bind on the well-known port is a FEATURE — it
//! means another neuron host already owns this machine, and the caller should
//! become its client instead, preserving a single host instance per machine.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use std::sync::mpsc::{Receiver, Sender};

use crate::adapters::chroma::{ChromaServer, HttpRequest, HttpResponse};
use crate::adapters::obs::{ObsClient, ObsEvent};
use crate::adapters::openrgb::OrgbConn;
use crate::api::HostApi;
use crate::bus::Value;
use crate::paint::PaintPolicy;
use crate::shell::HostHandle;
use crate::ws::{WsIn, WsStream};

/// OpenRGB's well-known SDK port.
pub const OPENRGB_ADDR: &str = "127.0.0.1:6742";

/// obs-websocket's default port.
pub const OBS_ADDR: &str = "127.0.0.1:4455";

/// The Chroma SDK's well-known REST port — the one Chroma clients talk to.
pub const CHROMA_ADDR: &str = "127.0.0.1:54235";

/// The host ELECTION port — a loopback listener held (never accepted on) for the process
/// lifetime as the machine-wide "one neuron host" token. Deliberately NOT one of the protocol
/// ports: Chroma/OpenRGB being taken means that ADAPTER has a squatter (often real Synapse) and
/// only that adapter degrades; the election failing means ANOTHER NEURON HOST owns this machine
/// and nothing device-facing may start — the double-writer race the host exists to kill.
pub const HOST_ELECT_ADDR: &str = "127.0.0.1:47615";

/// The held election token. Dropping it releases ownership (process exit does too).
pub struct HostLock {
    _listener: TcpListener,
}

impl HostLock {
    /// Acquire the machine-wide election lock at [`HOST_ELECT_ADDR`]. `None` = another neuron
    /// host already owns this machine (the caller must not attach bridges, spawn writers, or
    /// bind adapters).
    pub fn acquire() -> Option<HostLock> {
        HostLock::acquire_at(HOST_ELECT_ADDR)
    }

    /// For tests: acquire on an explicit addr.
    pub fn acquire_at(addr: &str) -> Option<HostLock> {
        let listener = TcpListener::bind(addr).ok()?;
        listener.set_nonblocking(true).ok()?;
        Some(HostLock {
            _listener: listener,
        })
    }
}

/// The live OpenRGB client roster: connection id → (kernel owner, announced
/// name). Maintained by the pump (insert on accept, name on SET_CLIENT_NAME,
/// remove on EVERY exit path — the same discipline as the claim release), so
/// the status card reads who's connected without asking the sockets anything.
type OrgbRoster = Arc<Mutex<HashMap<u64, (crate::arbiter::SourceId, String)>>>;

/// Bound on [`OrgbServer`]'s teardown (see its `Drop`). Derived from the accept loop's
/// stop-check cadence (nonblocking `accept` polled every ≤50ms) plus a connection thread's
/// worst case (a 100ms read timeout and — since the write-timeout fix below — a 500ms write
/// timeout), with margin for the accept thread's own connection-reaping join. Connection
/// threads race toward exit concurrently with the accept thread, so this is NOT additive across
/// however many clients are connected.
const ORGB_SERVER_DROP_DEADLINE: Duration = Duration::from_millis(1000);

/// A running OpenRGB TCP server. Dropping it stops the accept loop, joins
/// every connection thread, and thereby releases every client's claims.
pub struct OrgbServer {
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
    addr: SocketAddr,
    roster: OrgbRoster,
}

impl OrgbServer {
    /// Bind and serve with the default OpenRGB paint policy ([`PaintPolicy::opaque`]
    /// — show the client's paint as sent). `addr` is usually [`OPENRGB_ADDR`];
    /// tests pass `127.0.0.1:0` for an ephemeral port. A bind failure is returned
    /// as-is — on the well-known port it means "another host instance owns this
    /// machine; connect as a client instead".
    pub fn bind(addr: &str, handle: HostHandle) -> std::io::Result<OrgbServer> {
        OrgbServer::bind_with_policy(addr, handle, PaintPolicy::opaque())
    }

    /// Bind and serve, wiring every connection to a shared [`PaintPolicy`] (the
    /// OpenRGB family's blend/strength/fade/scope settings). The app passes the
    /// policy it also drives from the settings page.
    pub fn bind_with_policy(
        addr: &str,
        handle: HostHandle,
        policy: Arc<PaintPolicy>,
    ) -> std::io::Result<OrgbServer> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let roster: OrgbRoster = Arc::new(Mutex::new(HashMap::new()));
        let shared = roster.clone();
        let accept = crate::worker::spawn_named("neuron-orgb-accept", move || {
            accept_loop(listener, handle, stop_flag, shared, policy)
        })
        .expect("spawn accept thread");
        Ok(OrgbServer {
            stop,
            accept: Some(accept),
            addr: local,
            roster,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The connected clients (owner + announced name; "" until it names
    /// itself), for the status card.
    pub fn clients(&self) -> Vec<(crate::arbiter::SourceId, String)> {
        self.roster
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

impl Drop for OrgbServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            crate::worker::join_bounded(t, ORGB_SERVER_DROP_DEADLINE, "neuron-orgb-accept");
        }
    }
}

fn accept_loop(
    listener: TcpListener,
    handle: HostHandle,
    stop: Arc<AtomicBool>,
    roster: OrgbRoster,
    policy: Arc<PaintPolicy>,
) {
    let mut conns: Vec<thread::JoinHandle<()>> = Vec::new();
    let mut next_conn: u64 = 1;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let handle = handle.clone();
                let stop = stop.clone();
                let roster = roster.clone();
                let policy = Arc::clone(&policy);
                let conn_id = next_conn;
                next_conn += 1;
                if let Ok(t) = crate::worker::spawn_named("neuron-orgb-conn", move || {
                    serve_conn(stream, handle, stop, roster, conn_id, policy)
                }) {
                    conns.push(t);
                }
                // Opportunistically reap finished connections so a long-lived
                // server doesn't accumulate handles.
                conns.retain(|t| !t.is_finished());
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    for t in conns {
        let _ = t.join();
    }
}

fn serve_conn(
    mut stream: TcpStream,
    mut handle: HostHandle,
    stop: Arc<AtomicBool>,
    roster: OrgbRoster,
    conn_id: u64,
    policy: Arc<PaintPolicy>,
) {
    // Blocking reads with a short timeout so the stop flag is honored within
    // ~100ms without a busy loop. The write side needs its own bound: a client that stops
    // draining its receive buffer (connected but never reading) would otherwise let
    // `write_all` below block this thread forever — the stop flag is never rechecked mid-write.
    // 500ms is generous for any real local client and keeps the connection's worst-case exit
    // inside `ORGB_SERVER_DROP_DEADLINE`.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_nodelay(true);
    let mut conn = OrgbConn::new(&mut handle, policy);
    // On the roster from the first byte (named "" until SET_CLIENT_NAME) — a
    // connected-but-mute client still shows as connected, honestly.
    roster
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(conn_id, (conn.owner(), String::new()));
    // The read/feed loop runs inside catch_unwind: OpenRGB claims are PINNED (socket-scoped, no
    // TTL), so the `disconnected` release below is the ONLY thing standing between a panicking
    // packet handler and a permanently stranded claim — the stuck-lighting bug this crate exists
    // to kill. A panic here must still fall through to the release, exactly like the writer and
    // kernel contain theirs.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut buf = [0u8; 4096];
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            match stream.read(&mut buf) {
                Ok(0) => break, // orderly EOF
                Ok(n) => {
                    let reply = conn.feed(&buf[..n], &mut handle, Instant::now());
                    // Mirror a just-announced client name into the roster (cheap:
                    // only writes when it actually changed).
                    if !conn.client_name().is_empty() {
                        let mut r = roster.lock().unwrap_or_else(PoisonError::into_inner);
                        if let Some(slot) = r.get_mut(&conn_id) {
                            if slot.1 != conn.client_name() {
                                slot.1 = conn.client_name().to_string();
                            }
                        }
                    }
                    if !reply.is_empty() && stream.write_all(&reply).is_err() {
                        break;
                    }
                    // Also check hotplug on the ACTIVE path, not just the idle tick below:
                    // a client that streams continuously (an effect at 30+fps) never lets
                    // the read time out, so the idle branch would never fire for it. The
                    // fingerprint guard makes this idempotent — it only writes when the
                    // surface list actually changed — and DEVICE_LIST_UPDATED is an async
                    // push the client already expects at any time.
                    let hotplug = conn.check_hotplug(&mut handle);
                    if !hotplug.is_empty() && stream.write_all(&hotplug).is_err() {
                        break;
                    }
                }
                // Windows surfaces read timeouts as TimedOut, Unix as WouldBlock.
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    // Idle tick (~10Hz): recover this connection's paint if a
                    // kernel rebirth swept its claims. A silent client never
                    // sends fresh traffic, so this is the only path back for it.
                    conn.reassert(&mut handle, Instant::now());
                    // Same tick: push DEVICE_LIST_UPDATED if the surface list
                    // changed since last time, so a listening client (Home
                    // Assistant, an effect script) re-requests controller data
                    // instead of polling.
                    let hotplug = conn.check_hotplug(&mut handle);
                    if !hotplug.is_empty() && stream.write_all(&hotplug).is_err() {
                        break;
                    }
                    continue;
                }
                Err(_) => break, // reset/abort — same teardown as EOF
            }
        }
    }));
    // EVERY exit path — EOF, reset, stop flag, or a contained panic — releases the client's
    // footprint AND its roster entry. This line is the pump's one real responsibility.
    roster
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&conn_id);
    conn.disconnected(&mut handle);
}

/// A running Chroma REST server: minimal HTTP/1.1 over std TCP, feeding the
/// pure [`ChromaServer`] state machine. Unlike OpenRGB, a Chroma "session"
/// is NOT a TCP connection (games may reconnect per request) — lifecycle is
/// the 15s heartbeat lease inside the state machine, so this pump has no
/// disconnect duty at all; it only moves requests and responses.
/// Bound on [`ChromaHttpServer`]'s teardown — same reasoning as [`ORGB_SERVER_DROP_DEADLINE`]
/// (nonblocking accept polled ≤50ms, connections bounded by a 100ms read timeout plus a 500ms
/// write timeout).
const CHROMA_SERVER_DROP_DEADLINE: Duration = Duration::from_millis(1000);

pub struct ChromaHttpServer {
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
    addr: SocketAddr,
    /// The shared state machine — kept so [`sessions`](Self::sessions) can read
    /// the live session roster for the status card.
    server: Arc<Mutex<ChromaServer>>,
}

impl ChromaHttpServer {
    pub fn bind(addr: &str, handle: HostHandle) -> std::io::Result<ChromaHttpServer> {
        ChromaHttpServer::bind_with_policy(addr, handle, PaintPolicy::new())
    }

    pub fn bind_with_policy(
        addr: &str,
        handle: HostHandle,
        policy: Arc<PaintPolicy>,
    ) -> std::io::Result<ChromaHttpServer> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let server = Arc::new(Mutex::new(ChromaServer::with_policy(policy)));
        let shared = server.clone();
        let accept = crate::worker::spawn_named("neuron-chroma-accept", move || {
            chroma_accept_loop(listener, handle, shared, stop_flag)
        })
        .expect("spawn chroma accept thread");
        Ok(ChromaHttpServer {
            stop,
            accept: Some(accept),
            addr: local,
            server,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The LIVE Chroma sessions (owner + game title) — TTL-honest (see
    /// [`ChromaServer::sessions`]): a game that stopped heartbeating stops
    /// being reported the moment its lease would lapse.
    pub fn sessions(&self) -> Vec<(crate::arbiter::SourceId, String)> {
        self.server
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sessions(Instant::now())
    }
}

impl Drop for ChromaHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            crate::worker::join_bounded(t, CHROMA_SERVER_DROP_DEADLINE, "neuron-chroma-accept");
        }
    }
}

fn chroma_accept_loop(
    listener: TcpListener,
    handle: HostHandle,
    server: Arc<Mutex<ChromaServer>>,
    stop: Arc<AtomicBool>,
) {
    let mut conns: Vec<thread::JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let handle = handle.clone();
                let server = server.clone();
                let stop = stop.clone();
                if let Ok(t) = crate::worker::spawn_named("neuron-chroma-conn", move || {
                    serve_chroma_conn(stream, handle, server, stop)
                }) {
                    conns.push(t);
                }
                conns.retain(|t| !t.is_finished());
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    for t in conns {
        let _ = t.join();
    }
}

fn serve_chroma_conn(
    mut stream: TcpStream,
    mut handle: HostHandle,
    server: Arc<Mutex<ChromaServer>>,
    stop: Arc<AtomicBool>,
) {
    // Same read/write bounding as the OpenRGB pump (see `serve_conn`): the read timeout keeps
    // the stop flag honored between requests, and the write timeout keeps `write_http_response`
    // below from blocking forever on a client that stopped draining its receive buffer.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_nodelay(true);
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    // Keep-alive loop: serve requests until EOF/error/stop. Session lifecycle
    // is the lease, not this socket.
    'conn: loop {
        // Read until a full header block is buffered.
        let (req, consumed) = loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if let Some(parsed) = parse_http_request(&buf) {
                break parsed;
            }
            if buf.len() > (1 << 20) {
                return; // hostile/corrupt request — drop the connection
            }
            match stream.read(&mut chunk) {
                Ok(0) => return, // EOF between requests: normal
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    continue;
                }
                Err(_) => return,
            }
        };
        buf.drain(..consumed);

        let resp = {
            let mut srv = server.lock().unwrap_or_else(PoisonError::into_inner);
            srv.handle(&req, &mut handle, Instant::now())
        };
        // Discovery breadcrumb: anything that didn't succeed is worth seeing
        // while we learn what real game clients actually send (unknown paths,
        // shapes we refused). Steady-state success stays quiet.
        if resp.status != 200 {
            handle.publish(
                "host.chroma.http",
                Value::Text(format!("{} {} -> {}", req.method, req.path, resp.status)),
            );
        }
        if write_http_response(&mut stream, &resp).is_err() {
            break 'conn;
        }
    }
}

/// Parse one HTTP/1.1 request from the front of `buf`. Returns the request
/// and how many bytes it consumed, or None if incomplete. Minimal by design:
/// request line + headers (only Content-Length matters) + exact-length body.
fn parse_http_request(buf: &[u8]) -> Option<(HttpRequest, usize)> {
    let header_end = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = std::str::from_utf8(&buf[..header_end]).ok()?;
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split_whitespace();
    let method = request_line.next()?.to_uppercase();
    // Strip any query string; the Chroma routes are path-only.
    let path = request_line.next()?.split('?').next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok()?;
            }
        }
    }
    if content_length > (1 << 20) {
        return None; // over the sanity cap; caller drops the connection
    }
    let total = header_end + content_length;
    if buf.len() < total {
        return None;
    }
    let body = buf[header_end..total].to_vec();
    Some((HttpRequest { method, path, body }, total))
}

fn write_http_response(stream: &mut TcpStream, resp: &HttpResponse) -> std::io::Result<()> {
    let reason = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
        resp.status,
        reason,
        resp.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(resp.body.as_bytes())
}

// ── OBS integration (obs-websocket client) ────────────────────────────────
// Unlike the servers above, this connects OUT to OBS. It's the honest
// replacement for the fake-keystroke scene-switch hack: real requests that
// work minimized, plus OBS's own events on the bus. The connection is a
// single owning thread — poll (idle-timeout on the first byte) interleaved
// with draining the outbound command queue — so reads and writes never race.

/// A control command for the OBS connection. Fire-and-forget; queued while
/// disconnected and (harmlessly) dropped if OBS never comes back.
#[derive(Clone, Debug)]
pub enum ObsCmd {
    SetScene(String),
    StartStream,
    StopStream,
    ToggleStream,
    StartRecord,
    StopRecord,
    ToggleRecord,
    /// Toggle mute on a named input (e.g. "Mic/Aux").
    ToggleMute(String),
    /// An arbitrary request — `request_type` + a JSON object literal — so a
    /// power user can reach anything obs-websocket exposes without new code.
    Raw {
        request_type: String,
        data: String,
    },
    /// Re-announce everything: republish the retained `obs.connected` and
    /// re-issue the status-resync trio (the same one Identified sends), so the
    /// responses re-populate the bus. Sent by the app's OBS follower when the
    /// host kernel is REBORN underneath a still-connected websocket — the new
    /// bus starts empty, and without this the mirror/tally/hooks would serve
    /// the pre-crash state until OBS happened to change something. Truth is
    /// re-announced by OBS itself, never assumed from memory.
    Resync,
}

/// Bound on [`ObsConnection`]'s teardown. `WsStream::connect`'s handshake and every frame send
/// now carry a 750ms read/write timeout each (see `ws.rs`), and `sleep_interruptible`'s backoff
/// already rechecks `stop` every 100ms — so most of `obs_run`/`obs_session` is bounded well under
/// a second. The one residual gap: `WsStream::poll` intentionally reads the REST of an
/// already-started frame with no timeout (documented in `ws.rs`, so a slow network can't split a
/// frame across an idle timeout) — a peer that starts a frame and then goes silent mid-frame
/// would still stall this thread past 750ms. That pathological case is exactly what the
/// `join_bounded` backstop exists for, hence the generous margin here rather than trying to prove
/// a tighter bound.
const OBS_CONNECTION_DROP_DEADLINE: Duration = Duration::from_millis(1500);

/// A running OBS connection: a background thread that connects (with backoff),
/// authenticates, publishes events to the bus, and services commands. Dropping
/// it stops the thread. Cloneable [`ObsControl`] is how callers send commands.
pub struct ObsConnection {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    control: ObsControl,
    /// True once the websocket has authenticated (Identified), false whenever
    /// the session drops. Lets the GUI show "connected to OBS" vs "connecting"
    /// instead of a perpetual "reaching". Same truth as the `obs.connected`
    /// bus signal, in a form the status card can poll without subscribing.
    connected: Arc<AtomicBool>,
    /// A handle kept solely so Drop can publish the retained `obs.connected =
    /// false`: teardown-by-construction, so every path that drops the
    /// connection (toggle off, master off, reconnect) corrects the bus's
    /// retained truth, not just the reconnect loop. Requires the kernel to
    /// outlive this drop; the app orders its `HostState` fields so the OBS
    /// connection drops before the kernel.
    host: HostHandle,
}

/// Cloneable command sender for the OBS connection.
#[derive(Clone)]
pub struct ObsControl {
    tx: Sender<ObsCmd>,
}

impl ObsControl {
    /// Queue a command. Never blocks; a dead connection just drops it.
    pub fn send(&self, cmd: ObsCmd) {
        let _ = self.tx.send(cmd);
    }
}

impl ObsConnection {
    /// Start connecting to OBS at `addr` (usually [`OBS_ADDR`]) with the given
    /// obs-websocket password (empty = no auth). Returns immediately; the
    /// connection establishes in the background and re-establishes if OBS
    /// restarts. Publishes `obs.connected` (bool) plus per-event signals.
    pub fn start(addr: &str, password: &str, host: HostHandle) -> ObsConnection {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let connected = Arc::new(AtomicBool::new(false));
        let conn_flag = connected.clone();
        let addr = addr.to_string();
        let password = password.to_string();
        let mut thread_host = host.clone();
        let thread = crate::worker::spawn_named("neuron-obs", move || {
            obs_run(
                &addr,
                &password,
                &rx,
                &mut thread_host,
                &stop_flag,
                &conn_flag,
            )
        })
        .expect("spawn obs thread");
        ObsConnection {
            stop,
            thread: Some(thread),
            control: ObsControl { tx },
            connected,
            host,
        }
    }

    pub fn control(&self) -> ObsControl {
        self.control.clone()
    }

    /// Has the websocket authenticated with OBS? (For the GUI's connected-state
    /// readout; a running-but-not-yet-connected connection returns false.)
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

impl Drop for ObsConnection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            crate::worker::join_bounded(t, OBS_CONNECTION_DROP_DEADLINE, "neuron-obs");
        }
        self.connected.store(false, Ordering::Relaxed);
        // Correct the retained truth: whatever we last said, we're down now.
        // Idempotent with the reconnect loop's own publish; the point is that a
        // teardown ALWAYS lands it, even from a never-connected backoff.
        self.host.publish("obs.connected", Value::Bool(false));
    }
}

fn obs_run(
    addr: &str,
    password: &str,
    rx: &Receiver<ObsCmd>,
    host: &mut HostHandle,
    stop: &AtomicBool,
    connected: &AtomicBool,
) {
    let mut backoff = Duration::from_millis(500);
    while !stop.load(Ordering::Relaxed) {
        // "127.0.0.1:4455" -> host header "127.0.0.1".
        let host_hdr = addr.split(':').next().unwrap_or("localhost");
        match WsStream::connect(addr, host_hdr, "/") {
            Ok(ws) => {
                backoff = Duration::from_millis(500); // reset on a good connect
                obs_session(ws, password, rx, host, stop, connected);
                connected.store(false, Ordering::Relaxed);
                host.publish("obs.connected", Value::Bool(false));
            }
            Err(_) => {
                // OBS not running / obs-websocket off: wait and retry, capped.
                // Drain any stale queued commands so they don't pile up.
                while rx.try_recv().is_ok() {}
                sleep_interruptible(backoff, stop);
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        }
    }
}

fn obs_session(
    mut ws: WsStream,
    password: &str,
    rx: &Receiver<ObsCmd>,
    host: &mut HostHandle,
    stop: &AtomicBool,
    connected: &AtomicBool,
) {
    let mut client = ObsClient::new(password);
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Drain outbound commands (only once identified — before that OBS
        // rejects requests; queued commands wait for the next Idle after
        // Identify).
        if client.identified {
            while let Ok(cmd) = rx.try_recv() {
                let frames = match cmd {
                    ObsCmd::SetScene(s) => vec![client.set_scene(&s)],
                    ObsCmd::StartStream => vec![client.request("StartStream", "")],
                    ObsCmd::StopStream => vec![client.request("StopStream", "")],
                    ObsCmd::ToggleStream => vec![client.request("ToggleStream", "")],
                    ObsCmd::StartRecord => vec![client.request("StartRecord", "")],
                    ObsCmd::StopRecord => vec![client.request("StopRecord", "")],
                    ObsCmd::ToggleRecord => vec![client.request("ToggleRecord", "")],
                    ObsCmd::ToggleMute(name) => vec![client.request(
                        "ToggleInputMute",
                        &format!(r#"{{"inputName":{}}}"#, json_str_lit(&name)),
                    )],
                    ObsCmd::Raw { request_type, data } => {
                        vec![client.request(&request_type, &data)]
                    }
                    ObsCmd::Resync => {
                        // The consumer's bus was reborn: restore the retained
                        // connected-truth ourselves (we ARE identified in this
                        // branch), then re-issue the identify-time trio — the
                        // responses flow through `on_response` → the bus, the
                        // same path a live change takes.
                        host.publish("obs.connected", Value::Bool(true));
                        client.resync_requests()
                    }
                };
                for frame in &frames {
                    if ws.send_text(frame).is_err() {
                        return; // socket died — reconnect
                    }
                }
            }
        }
        match ws.poll(Duration::from_millis(100)) {
            Ok(WsIn::Idle) => {}
            Ok(WsIn::Closed) | Err(_) => return,
            Ok(WsIn::Text(t)) => {
                let step = client.on_message(&t);
                for frame in &step.send {
                    if ws.send_text(frame).is_err() {
                        return;
                    }
                }
                if step.identified {
                    connected.store(true, Ordering::Relaxed);
                    host.publish("obs.connected", Value::Bool(true));
                }
                for ev in step.events {
                    publish_obs_event(host, ev);
                }
            }
        }
    }
}

fn publish_obs_event(host: &mut HostHandle, ev: ObsEvent) {
    match ev {
        ObsEvent::Streaming(on) => host.publish("obs.streaming", Value::Bool(on)),
        ObsEvent::Recording(on) => host.publish("obs.recording", Value::Bool(on)),
        ObsEvent::Scene(name) => host.publish("obs.scene", Value::Text(name)),
        ObsEvent::InputMute { name, muted } => {
            host.publish(&format!("obs.mute.{name}"), Value::Bool(muted))
        }
    }
}

/// A JSON string literal for a command argument — full escaping, not just the two ASCII
/// specials. An OBS input/scene name is user-authored text that reaches us verbatim (from the
/// bus, from a macro argument); anything with a raw control character (a stray newline pasted
/// into a scene name, for instance) would otherwise emit invalid JSON and corrupt the frame for
/// every request after it on the same connection.
fn json_str_lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn sleep_interruptible(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(100);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let nap = step.min(left);
        thread::sleep(nap);
        left = left.saturating_sub(nap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::openrgb::{ids, packet, PROTOCOL_VERSION};
    use crate::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
    use crate::arbiter::{band, Content, Rgb};
    use crate::shell::Host;

    fn read_packet(s: &mut TcpStream) -> (u32, u32, Vec<u8>) {
        let mut hdr = [0u8; 16];
        s.read_exact(&mut hdr).expect("packet header");
        assert_eq!(&hdr[..4], b"ORGB");
        let dev = u32::from_le_bytes(hdr[4..8].try_into().unwrap());
        let id = u32::from_le_bytes(hdr[8..12].try_into().unwrap());
        let size = u32::from_le_bytes(hdr[12..16].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; size];
        s.read_exact(&mut payload).expect("packet payload");
        (dev, id, payload)
    }

    fn eventually(mut probe: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if probe() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn a_real_tcp_client_negotiates_paints_and_releases_on_disconnect() {
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid(
            "kbd",
            "Board",
            SurfaceKind::Keyboard,
            1,
            4,
        ));
        let base = h.next_source();
        h.claim(
            "kbd",
            base,
            band::BASE,
            LeaseSpec::Pinned,
            Content::Fill(Rgb(0, 255, 0)),
            Instant::now(),
        )
        .unwrap();

        let server = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("bind ephemeral");
        let mut s = TcpStream::connect(server.addr()).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Version negotiation over the real socket.
        s.write_all(&packet(
            0,
            ids::REQUEST_PROTOCOL_VERSION,
            &5u32.to_le_bytes(),
        ))
        .unwrap();
        let (_, id, payload) = read_packet(&mut s);
        assert_eq!(id, ids::REQUEST_PROTOCOL_VERSION);
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()),
            PROTOCOL_VERSION
        );

        // Controller count.
        s.write_all(&packet(0, ids::REQUEST_CONTROLLER_COUNT, &[]))
            .unwrap();
        let (_, _, payload) = read_packet(&mut s);
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 1);

        // The client is on the roster from the first byte (unnamed), and its
        // SET_CLIENT_NAME lands there for the status card.
        assert!(
            eventually(|| !server.clients().is_empty(), Duration::from_secs(2)),
            "a connected client must appear on the roster"
        );
        s.write_all(&packet(0, ids::SET_CLIENT_NAME, b"hass\0")).unwrap();
        assert!(
            eventually(
                || server.clients().iter().any(|(_, n)| n == "hass"),
                Duration::from_secs(2)
            ),
            "the announced client name must reach the roster"
        );

        // Paint red over the wire.
        let mut up = Vec::new();
        up.extend_from_slice(&0u32.to_le_bytes());
        up.extend_from_slice(&4u16.to_le_bytes());
        for _ in 0..4 {
            up.extend_from_slice(&[255, 0, 0, 0]);
        }
        s.write_all(&packet(0, ids::UPDATELEDS, &up)).unwrap();

        let mut probe = host.handle();
        assert!(
            eventually(
                || probe.resolve("kbd", Instant::now()).unwrap()[0] == Some(Rgb(255, 0, 0)),
                Duration::from_secs(2)
            ),
            "socket paint must reach the arbiter"
        );

        // Hard disconnect (no goodbye): the pump must release the footprint
        // and base lighting must return.
        drop(s);
        assert!(
            eventually(
                || probe.resolve("kbd", Instant::now()).unwrap()[0] == Some(Rgb(0, 255, 0)),
                Duration::from_secs(2)
            ),
            "base must return after the socket drops"
        );
        // …and the roster empties on the same teardown path as the claims.
        assert!(
            eventually(|| server.clients().is_empty(), Duration::from_secs(2)),
            "a dropped client must leave the roster"
        );
    }

    fn read_http(s: &mut TcpStream) -> (u16, String) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let n = s.read(&mut chunk).expect("http read");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..he]).to_string();
                let status: u16 = head
                    .split_whitespace()
                    .nth(1)
                    .and_then(|c| c.parse().ok())
                    .expect("status");
                let cl: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())?
                    })
                    .expect("content-length");
                while buf.len() < he + 4 + cl {
                    let n = s.read(&mut chunk).expect("http body read");
                    buf.extend_from_slice(&chunk[..n]);
                }
                let body = String::from_utf8_lossy(&buf[he + 4..he + 4 + cl]).to_string();
                return (status, body);
            }
        }
    }

    #[test]
    fn chroma_http_round_trip_over_a_real_socket() {
        let host = Host::spawn();
        let mut h = host.handle();
        h.declare(SurfaceInfo::grid(
            "kbd",
            "Board",
            SurfaceKind::Keyboard,
            1,
            4,
        ));
        let server = ChromaHttpServer::bind("127.0.0.1:0", host.handle()).expect("bind");
        let mut s = TcpStream::connect(server.addr()).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        // Init over real HTTP.
        let body = r#"{"title":"Socket Game"}"#;
        let req = format!(
            "POST /razer/chromasdk HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).unwrap();
        let (status, resp) = read_http(&mut s);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let sid = v["sessionid"].as_u64().expect("sessionid");

        // The live-session roster carries the game's title for the status card.
        assert_eq!(
            server.sessions().iter().map(|(_, t)| t.as_str()).collect::<Vec<_>>(),
            vec!["Socket Game"]
        );

        // Static red (BGR 0x0000FF) on the same keep-alive connection.
        let body = r#"{"effect":"CHROMA_STATIC","param":{"color":255}}"#;
        let req = format!(
            "PUT /razer/chromasdk/sess/{}/keyboard HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{}",
            sid,
            body.len(),
            body
        );
        s.write_all(req.as_bytes()).unwrap();
        let (status, resp) = read_http(&mut s);
        assert_eq!(status, 200);
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["result"].as_i64(), Some(0));

        let mut probe = host.handle();
        assert!(
            eventually(
                || probe.resolve("kbd", Instant::now()).unwrap()[0] == Some(Rgb(255, 0, 0)),
                Duration::from_secs(2)
            ),
            "HTTP-driven paint must reach the arbiter"
        );
    }

    /// Drop `value` on a background thread and wait for THAT thread with our own bounded poll —
    /// so a regression in `join_bounded` (removed, or given an unbounded deadline) fails this
    /// test cleanly instead of hanging the whole test binary.
    fn assert_drop_bounded<T: Send + 'static>(value: T, bound: Duration, label: &str) {
        let start = Instant::now();
        let dropper =
            crate::worker::spawn_named("t-drop-bound", move || drop(value)).expect("spawn dropper");
        let deadline = Instant::now() + bound;
        let mut finished = false;
        while Instant::now() < deadline {
            if dropper.is_finished() {
                finished = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(finished, "{label}: drop must return within {bound:?}");
        assert!(
            start.elapsed() < bound,
            "{label}: drop took {:?}, expected under {bound:?}",
            start.elapsed()
        );
    }

    #[test]
    fn orgb_server_drop_is_bounded_with_a_silent_client() {
        // A client that connects and never reads or writes — the pump's connection thread sits
        // in its read loop. Whether or not this particular scenario ever triggers the write-side
        // timeout, Drop must be bounded regardless.
        let host = Host::spawn();
        let server = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("bind ephemeral");
        let client = TcpStream::connect(server.addr()).expect("connect");
        thread::sleep(Duration::from_millis(50)); // let the conn thread actually start

        assert_drop_bounded(server, ORGB_SERVER_DROP_DEADLINE * 3, "OrgbServer");
        drop(client);
    }

    #[test]
    fn chroma_server_drop_is_bounded_with_a_silent_client() {
        let host = Host::spawn();
        let server = ChromaHttpServer::bind("127.0.0.1:0", host.handle()).expect("bind ephemeral");
        let client = TcpStream::connect(server.addr()).expect("connect");
        thread::sleep(Duration::from_millis(50));

        assert_drop_bounded(server, CHROMA_SERVER_DROP_DEADLINE * 3, "ChromaHttpServer");
        drop(client);
    }

    #[test]
    fn obs_connection_drop_is_bounded_when_the_peer_never_speaks() {
        // A raw loopback listener that accepts the connection and then holds it open in total
        // silence — never sends the HTTP upgrade response `WsStream::connect` waits for. This is
        // exactly the handshake-stall scenario the read/write timeouts in `ws.rs` were added for.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = listener.local_addr().unwrap().to_string();
        let holder = crate::worker::spawn_named("t-obs-silent-peer", move || {
            if let Ok((stream, _)) = listener.accept() {
                // Hold the accepted socket open (and thus the listener's peer) for longer than
                // any bound under test, without ever reading or writing it.
                thread::sleep(Duration::from_secs(5));
                drop(stream);
            }
        })
        .expect("spawn silent peer");

        let host = Host::spawn();
        let conn = ObsConnection::start(&addr, "", host.handle());
        // Give it time to dial in and start (and, pre-fix, wedge inside) the handshake read.
        thread::sleep(Duration::from_millis(200));

        assert_drop_bounded(conn, OBS_CONNECTION_DROP_DEADLINE * 3, "ObsConnection");
        drop(holder); // the silent-peer thread's own sleep ends on its own; not joined here
    }

    #[test]
    fn well_known_port_double_bind_fails_the_second_host() {
        let host = Host::spawn();
        let first = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("first bind");
        let addr = first.addr().to_string();
        // Same port again: refused — the single-instance signal.
        assert!(OrgbServer::bind(&addr, host.handle()).is_err());
    }

    #[test]
    fn host_lock_is_exclusive() {
        // `acquire_at("127.0.0.1:0")` can't test exclusivity — the OS hands out a fresh
        // ephemeral port every time. Instead, bind an ephemeral listener OURSELVES to get a
        // real, deterministically-taken addr, then contend on its exact address.
        let taken = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
        let addr = taken.local_addr().unwrap().to_string();

        assert!(
            HostLock::acquire_at(&addr).is_none(),
            "an addr already bound by someone else must fail the election"
        );

        drop(taken); // release the port
        let lock = HostLock::acquire_at(&addr).expect("a free addr must succeed");
        drop(lock); // and releasing OUR lock frees it again
        assert!(
            HostLock::acquire_at(&addr).is_some(),
            "dropping the lock must release the addr"
        );
    }

    #[test]
    fn json_str_lit_escapes_every_control_character() {
        // A newline, a tab, and a raw 0x01 byte — the kind of thing a pasted scene name or a
        // macro argument can carry. The only real test is that a JSON parser accepts it AND
        // recovers the original string.
        let raw = "line one\nline\ttwo\u{1}end";
        let literal = json_str_lit(raw);
        let wrapped = format!("{{\"s\": {literal}}}");
        let v: serde_json::Value = serde_json::from_str(&wrapped).expect("must be valid JSON");
        assert_eq!(v["s"].as_str(), Some(raw));
    }
}
