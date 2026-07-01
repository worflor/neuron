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
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::adapters::openrgb::OrgbConn;
use crate::shell::HostHandle;

/// OpenRGB's well-known SDK port.
pub const OPENRGB_ADDR: &str = "127.0.0.1:6742";

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

    #[test]
    fn well_known_port_double_bind_fails_the_second_host() {
        let host = Host::spawn();
        let first = OrgbServer::bind("127.0.0.1:0", host.handle()).expect("first bind");
        let addr = first.addr().to_string();
        // Same port again: refused — the single-instance signal.
        assert!(OrgbServer::bind(&addr, host.handle()).is_err());
    }
}
