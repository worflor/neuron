//! A minimal RFC 6455 WebSocket client — just enough to speak obs-websocket,
//! hand-rolled so [`neuron-host`](crate) stays zero-dep. Text frames only
//! (obs-websocket is all JSON text), plus the control frames a compliant
//! client must answer (ping→pong, close). Client→server frames are masked as
//! the RFC requires; server→client frames are not.
//!
//! The framing math (mask XOR, the 3-way length encoding) is pure and
//! unit-tested below. The socket handshake + read loop are thin I/O.
//!
//! Read discipline (why there's no partial-frame loss): [`WsStream::poll`]
//! applies the idle timeout to the FIRST header byte ONLY — once a byte of a
//! frame has arrived, the rest is read blocking. So a timeout can only ever
//! land at a frame boundary, and the caller can safely interleave sends
//! between polls on the one owning thread (no second thread, no write races).

use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime};

const MAX_FRAME: usize = 16 << 20; // 16 MiB — obs frames are tiny; this is a hostile-length guard.

/// What a poll surfaced.
pub enum WsIn {
    /// A complete text message.
    Text(String),
    /// The idle timeout elapsed at a frame boundary — caller may send now.
    Idle,
    /// The peer closed, or the socket died. Reconnect.
    Closed,
}

pub struct WsStream {
    sock: TcpStream,
    mask_seed: u64,
}

impl WsStream {
    /// Connect and perform the upgrade handshake to `host` at `path`
    /// (obs-websocket lives at `/`). We don't verify `Sec-WebSocket-Accept`:
    /// it guards against a confused cache/proxy sitting between client and
    /// server, which cannot exist on a loopback connection to OBS — a 101 with
    /// an `upgrade: websocket` header is conclusive here. Documented, not
    /// silently skipped.
    pub fn connect(addr: &str, host: &str, path: &str) -> io::Result<WsStream> {
        let mut sock = TcpStream::connect(addr)?;
        sock.set_nodelay(true).ok();
        // The Sec-WebSocket-Key is a per-connection nonce, not a secret; the
        // RFC only asks that it vary. Time + address entropy is ample.
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ (&sock as *const _ as u64);
        let key = crate::crypto::base64(&seed.to_le_bytes()[..]);
        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        sock.write_all(req.as_bytes())?;

        // Read response headers up to the blank line.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = sock.read(&mut b)?;
            if n == 0 {
                return Err(io::Error::new(ErrorKind::UnexpectedEof, "eof during handshake"));
            }
            head.push(b[0]);
            if head.len() > 8192 {
                return Err(io::Error::new(ErrorKind::InvalidData, "handshake headers too long"));
            }
        }
        let head = String::from_utf8_lossy(&head);
        let ok = head.starts_with("HTTP/1.1 101")
            && head.to_ascii_lowercase().contains("upgrade: websocket");
        if !ok {
            return Err(io::Error::new(
                ErrorKind::ConnectionRefused,
                "not a websocket upgrade (is this obs-websocket?)",
            ));
        }
        Ok(WsStream { sock, mask_seed: seed })
    }

    /// Send one text message as a single masked frame.
    pub fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.mask_seed = self.mask_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let mask = (self.mask_seed >> 16) as u32;
        let frame = encode_text_frame(text.as_bytes(), mask);
        self.sock.write_all(&frame)
    }

    /// Read the next text message, waiting at most `idle` for the FIRST byte.
    /// Answers ping frames internally (pong) and keeps reading; returns
    /// [`WsIn::Idle`] only at a frame boundary so sends can be interleaved.
    pub fn poll(&mut self, idle: Duration) -> io::Result<WsIn> {
        loop {
            self.sock.set_read_timeout(Some(idle))?;
            let mut b0 = [0u8; 1];
            match self.sock.read(&mut b0) {
                Ok(0) => return Ok(WsIn::Closed),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    return Ok(WsIn::Idle);
                }
                Err(e) => return Err(e),
                Ok(_) => {}
            }
            // A frame has started — read the rest blocking so a slow network
            // can't split it across an idle timeout.
            self.sock.set_read_timeout(None)?;
            let fin_op = b0[0];
            let opcode = fin_op & 0x0f;
            let mut b1 = [0u8; 1];
            self.sock.read_exact(&mut b1)?;
            let masked = b1[0] & 0x80 != 0;
            let len = self.read_len((b1[0] & 0x7f) as usize)?;
            if len > MAX_FRAME {
                return Err(io::Error::new(ErrorKind::InvalidData, "frame too large"));
            }
            let mask = if masked {
                let mut m = [0u8; 4];
                self.sock.read_exact(&mut m)?;
                Some(m)
            } else {
                None
            };
            let mut payload = vec![0u8; len];
            self.sock.read_exact(&mut payload)?;
            if let Some(m) = mask {
                for (i, byte) in payload.iter_mut().enumerate() {
                    *byte ^= m[i & 3];
                }
            }
            match opcode {
                0x1 => return Ok(WsIn::Text(String::from_utf8_lossy(&payload).into_owned())),
                0x8 => return Ok(WsIn::Closed), // close
                0x9 => {
                    // ping → pong (echo payload). Then loop for the next frame.
                    self.mask_seed =
                        self.mask_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let pong = encode_frame(0xA, &payload, (self.mask_seed >> 16) as u32);
                    self.sock.write_all(&pong)?;
                }
                // pong (0xA), continuation (0x0), binary (0x2) — obs sends none
                // of these meaningfully; skip and read on.
                _ => {}
            }
        }
    }

    fn read_len(&mut self, first: usize) -> io::Result<usize> {
        match first {
            126 => {
                let mut b = [0u8; 2];
                self.sock.read_exact(&mut b)?;
                Ok(u16::from_be_bytes(b) as usize)
            }
            127 => {
                let mut b = [0u8; 8];
                self.sock.read_exact(&mut b)?;
                Ok(u64::from_be_bytes(b) as usize)
            }
            n => Ok(n),
        }
    }
}

fn encode_text_frame(payload: &[u8], mask: u32) -> Vec<u8> {
    encode_frame(0x1, payload, mask)
}

/// A single FIN client frame with opcode `op`, masked (the RFC requires all
/// client→server frames be masked).
fn encode_frame(op: u8, payload: &[u8], mask: u32) -> Vec<u8> {
    let m = mask.to_be_bytes();
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | op); // FIN + opcode
    let n = payload.len();
    if n < 126 {
        out.push(0x80 | n as u8);
    } else if n <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(&m);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ m[i & 3]));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmask a client frame the way a server would, to prove our masking is
    /// correct and reversible across every length-encoding boundary.
    fn decode_client_frame(frame: &[u8]) -> (u8, Vec<u8>) {
        let op = frame[0] & 0x0f;
        let masked = frame[1] & 0x80 != 0;
        assert!(masked, "client frames MUST be masked (RFC 6455)");
        let len7 = (frame[1] & 0x7f) as usize;
        let (len, mut i) = match len7 {
            126 => (u16::from_be_bytes([frame[2], frame[3]]) as usize, 4),
            127 => (
                u64::from_be_bytes(frame[2..10].try_into().unwrap()) as usize,
                10,
            ),
            n => (n, 2),
        };
        let mask = [frame[i], frame[i + 1], frame[i + 2], frame[i + 3]];
        i += 4;
        let payload: Vec<u8> =
            frame[i..i + len].iter().enumerate().map(|(j, b)| b ^ mask[j & 3]).collect();
        (op, payload)
    }

    #[test]
    fn text_frame_round_trips_short() {
        let msg = br#"{"op":6}"#;
        let (op, payload) = decode_client_frame(&encode_text_frame(msg, 0xdeadbeef));
        assert_eq!(op, 0x1);
        assert_eq!(payload, msg);
    }

    #[test]
    fn length_encoding_crosses_both_boundaries() {
        // <126 (1-byte), ==126..=65535 (2-byte), >65535 (8-byte) all decode back.
        for &n in &[0usize, 1, 125, 126, 127, 65535, 65536, 200_000] {
            let msg = vec![b'x'; n];
            let frame = encode_text_frame(&msg, 0x11223344);
            // header size matches the chosen encoding
            let expect_hdr = if n < 126 {
                2
            } else if n <= 65535 {
                4
            } else {
                10
            } + 4; // + 4 mask bytes
            assert_eq!(frame.len(), expect_hdr + n, "n={n}");
            let (op, payload) = decode_client_frame(&frame);
            assert_eq!(op, 0x1);
            assert_eq!(payload.len(), n, "n={n}");
            assert!(payload.iter().all(|b| *b == b'x'), "n={n}");
        }
    }

    #[test]
    fn masking_actually_masks() {
        // the on-wire bytes must NOT equal the plaintext (a zero mask would be
        // a silent bug — the RFC forbids it in spirit and OBS would reject it)
        let msg = b"aaaaaaaa";
        let frame = encode_text_frame(msg, 0x01020304);
        let body = &frame[6..]; // after 2 header + 4 mask
        assert_ne!(body, msg, "payload must be masked on the wire");
    }
}
