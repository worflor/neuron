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
//! become its client instead (the single-instance story, §10.1).

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use crate::adapters::chroma::{ChromaServer, HttpRequest, HttpResponse};
use crate::adapters::openrgb::OrgbConn;
use crate::api::HostApi;
use crate::bus::Value;
use crate::shell::HostHandle;

/// OpenRGB's well-known SDK port.
pub const OPENRGB_ADDR: &str = "127.0.0.1:6742";

/// The Chroma SDK's well-known REST port — the one `RzChromaSDK64.dll` (and
/// therefore every Chroma game) talks to.
pub const CHROMA_ADDR: &str = "127.0.0.1:54235";

/// A running OpenRGB TCP server. Dropping it stops the accept loop, joins
/// every connection thread, and thereby releases every client's claims.
pub struct OrgbServer {
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
    addr: SocketAddr,
}

impl OrgbServer {
    /// Bind and serve. `addr` is usually [`OPENRGB_ADDR`]; tests pass
    /// `127.0.0.1:0` for an ephemeral port. A bind failure is returned as-is —
    /// on the well-known port it means "another host instance owns this
    /// machine; connect as a client instead".
    pub fn bind(addr: &str, handle: HostHandle) -> std::io::Result<OrgbServer> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let accept = thread::Builder::new()
            .name("neuron-orgb-accept".into())
            .spawn(move || accept_loop(listener, handle, stop_flag))
            .expect("spawn accept thread");
        Ok(OrgbServer { stop, accept: Some(accept), addr: local })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for OrgbServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: TcpListener, handle: HostHandle, stop: Arc<AtomicBool>) {
    let mut conns: Vec<thread::JoinHandle<()>> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                let handle = handle.clone();
                let stop = stop.clone();
                if let Ok(t) = thread::Builder::new()
                    .name("neuron-orgb-conn".into())
                    .spawn(move || serve_conn(stream, handle, stop))
                {
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

fn serve_conn(mut stream: TcpStream, mut handle: HostHandle, stop: Arc<AtomicBool>) {
    // Blocking reads with a short timeout so the stop flag is honored within
    // ~100ms without a busy loop.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let _ = stream.set_nodelay(true);
    let mut conn = OrgbConn::new(&mut handle);
    let mut buf = [0u8; 4096];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break, // orderly EOF
            Ok(n) => {
                let reply = conn.feed(&buf[..n], &mut handle, Instant::now());
                if !reply.is_empty() && stream.write_all(&reply).is_err() {
                    break;
                }
            }
            // Windows surfaces read timeouts as TimedOut, Unix as WouldBlock.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => break, // reset/abort — same teardown as EOF
        }
    }
    // EVERY exit path releases the client's footprint. This line is the pump's
    // one real responsibility.
    conn.disconnected(&mut handle);
}

/// A running Chroma REST server: minimal HTTP/1.1 over std TCP, feeding the
/// pure [`ChromaServer`] state machine. Unlike OpenRGB, a Chroma "session"
/// is NOT a TCP connection (games may reconnect per request) — lifecycle is
/// the 15s heartbeat lease inside the state machine, so this pump has no
/// disconnect duty at all; it only moves requests and responses.
pub struct ChromaHttpServer {
    stop: Arc<AtomicBool>,
    accept: Option<thread::JoinHandle<()>>,
    addr: SocketAddr,
}

impl ChromaHttpServer {
    pub fn bind(addr: &str, handle: HostHandle) -> std::io::Result<ChromaHttpServer> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let server = Arc::new(Mutex::new(ChromaServer::new()));
        let accept = thread::Builder::new()
            .name("neuron-chroma-accept".into())
            .spawn(move || chroma_accept_loop(listener, handle, server, stop_flag))
            .expect("spawn chroma accept thread");
        Ok(ChromaHttpServer { stop, accept: Some(accept), addr: local })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for ChromaHttpServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            let _ = t.join();
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
                if let Ok(t) = thread::Builder::new()
                    .name("neuron-chroma-conn".into())
                    .spawn(move || serve_chroma_conn(stream, handle, server, stop))
                {
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
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
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
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 4));
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
        s.write_all(&packet(0, ids::REQUEST_PROTOCOL_VERSION, &5u32.to_le_bytes())).unwrap();
        let (_, id, payload) = read_packet(&mut s);
        assert_eq!(id, ids::REQUEST_PROTOCOL_VERSION);
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), PROTOCOL_VERSION);

        // Controller count.
        s.write_all(&packet(0, ids::REQUEST_CONTROLLER_COUNT, &[])).unwrap();
        let (_, _, payload) = read_packet(&mut s);
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 1);

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
    }

    fn read_http(s: &mut TcpStream) -> (u16, String) {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let n = s.read(&mut chunk).expect("http read");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(he) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..he]).to_string();
                let status: u16 =
                    head.split_whitespace().nth(1).and_then(|c| c.parse().ok()).expect("status");
                let cl: usize = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
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
        h.declare(SurfaceInfo::grid("kbd", "Board", SurfaceKind::Keyboard, 1, 4));
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

    #[test]
    fn well_known_port_double_bind_fails_the_second_host() {
        let host = Host::spawn();
        let first = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("first bind");
        let addr = first.addr().to_string();
        // Same port again: refused — the single-instance signal.
        assert!(OrgbServer::bind(&addr, host.handle()).is_err());
    }
}
