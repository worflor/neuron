// SPDX-FileCopyrightText: 2026 Woflo Labs
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional permission: Neuron-Woflo exception; see repository-root LICENSE.md.

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
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime};

const MAX_FRAME: usize = 16 << 20; // 16 MiB — obs frames are tiny; this is a hostile-length guard.

/// What a poll surfaced.
#[derive(Debug)]
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
    /// Bytes of a message whose fragments are still arriving (see [`WsStream::poll`]). Empty
    /// between messages; a control frame may legally arrive mid-message and leaves it untouched.
    partial: Vec<u8>,
}

impl WsStream {
    /// Connect and perform the upgrade handshake to `host` at `path`
    /// (obs-websocket lives at `/`). We don't verify `Sec-WebSocket-Accept`:
    /// it guards against a confused cache/proxy sitting between client and
    /// server, which cannot exist on a loopback connection to OBS — a 101 with
    /// an `upgrade: websocket` header is conclusive here. Documented, not
    /// silently skipped.
    pub fn connect(addr: &str, host: &str, path: &str) -> io::Result<WsStream> {
        // Bound the CONNECT itself, not just the post-connect I/O. `TcpStream::connect` blocks with
        // no timeout: pointed at an unreachable OBS host (a remote box that's off, a firewall
        // dropping SYN), it hangs for the OS default (~21s on Windows) — and `obs_run` only rechecks
        // its stop flag BETWEEN top-level calls, so a hung connect makes ObsConnection's Drop miss
        // its deadline and leak the thread (caught by the resident-census churn test). A 750ms
        // connect budget matches the read/write bounds below; a failure just falls into obs_run's
        // interruptible backoff, which rechecks stop. (Address resolution is done here too — for
        // OBS's usual IP:port it's instant; a hostname's DNS lookup is a separate rare blocking
        // point not bounded here.)
        let sockaddr = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "obs address resolved to nothing"))?;
        let mut sock = TcpStream::connect_timeout(&sockaddr, Duration::from_millis(750))?;
        sock.set_nodelay(true).ok();
        // Bound both directions before doing any I/O: the owning thread (`obs_run`) only
        // rechecks its stop flag BETWEEN top-level calls, so a handshake that stalls — the peer
        // accepted the TCP connection but never sends a byte, or the write below meets a full
        // send buffer with nobody reading — must not block this call forever. 750ms per read/
        // write call is generous for a local obs-websocket peer; the write timeout persists for
        // the whole connection (later frame sends reuse it), while the read timeout is
        // overwritten per-call by `poll` below.
        sock.set_read_timeout(Some(Duration::from_millis(750))).ok();
        sock.set_write_timeout(Some(Duration::from_millis(750))).ok();
        // The Sec-WebSocket-Key is a per-connection nonce, not a secret; the
        // RFC only asks that it vary. Time + address entropy is ample.
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
            ^ (&raw const sock as u64);
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
        Ok(WsStream {
            sock,
            mask_seed: seed,
            partial: Vec::new(),
        })
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
            // RFC 6455 fragmentation. A message is FIN=1 in ONE frame, or a FIN=0 data frame
            // followed by continuation frames (opcode 0x0) with FIN=1 on the last. OBS fragments
            // large payloads (a big scene list), so ignoring the FIN bit meant returning the first
            // fragment AS IF it were the whole message — truncated JSON handed to the parser — and
            // silently DROPPING every continuation frame. Control frames (ping/pong/close) may be
            // interleaved between fragments and must not disturb the partial message.
            let fin = fin_op & 0x80 != 0;
            match opcode {
                0x1 | 0x0 => {
                    if opcode == 0x1 && !self.partial.is_empty() {
                        // a new data frame while a message is still open: the peer broke framing.
                        self.partial.clear();
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "interleaved data frame inside a fragmented message",
                        ));
                    }
                    if opcode == 0x0 && self.partial.is_empty() {
                        // A continuation frame with nothing to continue is a framing violation
                        // (RFC 6455 §5.4) whether or not FIN is set — a lone FIN+continuation
                        // would otherwise be returned as a complete message assembled out of
                        // thin air, which is precisely what this guard exists to prevent.
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "continuation frame with no message to continue",
                        ));
                    }
                    self.partial.extend_from_slice(&payload);
                    if self.partial.len() > MAX_FRAME {
                        self.partial.clear();
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "fragmented message too large",
                        ));
                    }
                    if fin {
                        let msg = String::from_utf8_lossy(&self.partial).into_owned();
                        self.partial.clear();
                        return Ok(WsIn::Text(msg));
                    }
                    // more fragments to come — keep reading.
                }
                0x2 => {
                    // A BINARY data frame is a data frame: opening one while a text message is
                    // still being assembled breaks framing exactly like an interleaved text frame
                    // (RFC 6455 §5.4), so it gets the same refusal rather than being skipped —
                    // skipping it would leave `partial` open and splice the next continuation
                    // onto a message the peer considers finished. Outside a fragmented message a
                    // binary frame is simply not text: obs sends none, and guessing at an
                    // encoding would be worse than ignoring it.
                    if !self.partial.is_empty() {
                        self.partial.clear();
                        return Err(io::Error::new(
                            ErrorKind::InvalidData,
                            "binary frame inside a fragmented text message",
                        ));
                    }
                }
                0x8 => {
                    // close: drop any half-assembled message with it. The caller reconnects with a
                    // fresh WsStream, so this can't leak across sessions today — but leaving bytes
                    // in `partial` would make a future reuse of the struct splice a dead session's
                    // fragment onto the next message.
                    self.partial.clear();
                    return Ok(WsIn::Closed);
                }
                0x9 => {
                    // ping → pong (echo payload). Then loop for the next frame. Deliberately does
                    // NOT touch `partial`: a control frame between fragments is legal.
                    self.mask_seed =
                        self.mask_seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let pong = encode_frame(0xA, &payload, (self.mask_seed >> 16) as u32);
                    self.sock.write_all(&pong)?;
                }
                // pong (0xA) and anything reserved — obs sends none of these meaningfully.
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
    } else if u16::try_from(n).is_ok() {
        out.push(0x80 | 0x7e);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(0x80 | 0x7f);
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

    // ── THE DECODER (`poll`) ────────────────────────────────────────────────────────────────
    // Everything above tests the ENCODER. `poll` — the half that reads OBS's frames — had no
    // coverage at all, which is how ignoring the FIN bit survived: a fragmented message (OBS
    // fragments large scene lists) returned its FIRST FRAGMENT as a complete message, handing the
    // JSON parser truncated input, and every continuation frame was silently discarded.
    //
    // These drive the real `poll` over a real loopback socket pair, feeding SERVER frames (which
    // are unmasked, per the RFC — masking is the client's obligation).

    use std::net::{TcpListener, TcpStream as StdTcpStream};

    /// A server frame: `fin` + opcode + unmasked payload.
    fn server_frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 10);
        out.push(if fin { 0x80 | op } else { op });
        let n = payload.len();
        if n < 126 {
            out.push(n as u8);
        } else if u16::try_from(n).is_ok() {
            out.push(126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
        out.extend_from_slice(payload);
        out
    }

    /// A connected `WsStream` (handshake bypassed — this exercises the frame layer) plus the
    /// server end to write frames into.
    fn ws_pair() -> (WsStream, StdTcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let addr = listener.local_addr().unwrap();
        let client = StdTcpStream::connect(addr).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (
            WsStream {
                sock: client,
                mask_seed: 0x1234_5678,
                partial: Vec::new(),
            },
            server,
        )
    }

    const IDLE: Duration = Duration::from_millis(500);

    #[test]
    fn a_single_fin_text_frame_decodes_whole() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(true, 0x1, br#"{"op":5}"#)).unwrap();
        match ws.poll(IDLE).unwrap() {
            WsIn::Text(t) => assert_eq!(t, r#"{"op":5}"#),
            other => panic!("expected the text message, got {other:?}"),
        }
    }

    /// THE FRAGMENTATION PROPERTY: a message split across three frames must arrive as ONE
    /// complete message. Before the FIN bit was honoured this returned `{"op":` — valid-looking,
    /// truncated, and impossible to distinguish downstream from a malformed OBS payload.
    #[test]
    fn a_fragmented_message_reassembles_into_one_whole_message() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(false, 0x1, br#"{"op":"#)).unwrap(); // first, FIN=0
        srv.write_all(&server_frame(false, 0x0, b"5,\"d\":")).unwrap(); // continuation
        srv.write_all(&server_frame(true, 0x0, b"{}}")).unwrap(); // last, FIN=1
        match ws.poll(IDLE).unwrap() {
            WsIn::Text(t) => assert_eq!(t, r#"{"op":5,"d":{}}"#, "all three fragments, in order"),
            other => panic!("expected the reassembled message, got {other:?}"),
        }
    }

    /// A ping arriving BETWEEN fragments is legal (RFC 6455 §5.4): it must be ponged without
    /// disturbing the partial message.
    #[test]
    fn a_control_frame_between_fragments_does_not_corrupt_the_message() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(false, 0x1, b"half-")).unwrap();
        srv.write_all(&server_frame(true, 0x9, b"ping")).unwrap(); // interleaved control frame
        srv.write_all(&server_frame(true, 0x0, b"whole")).unwrap();
        match ws.poll(IDLE).unwrap() {
            WsIn::Text(t) => assert_eq!(t, "half-whole"),
            other => panic!("expected the reassembled message, got {other:?}"),
        }
        // and the pong actually went back on the wire
        srv.set_read_timeout(Some(IDLE)).unwrap();
        let mut back = [0u8; 2];
        srv.read_exact(&mut back).expect("a pong was sent");
        assert_eq!(back[0] & 0x0f, 0xA, "pong opcode");
    }

    /// Two messages back-to-back, the second fragmented: the first must not absorb the second's
    /// bytes (the `partial` buffer has to reset between messages).
    #[test]
    fn consecutive_messages_do_not_bleed_into_each_other() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(true, 0x1, b"first")).unwrap();
        srv.write_all(&server_frame(false, 0x1, b"sec")).unwrap();
        srv.write_all(&server_frame(true, 0x0, b"ond")).unwrap();
        assert!(matches!(ws.poll(IDLE).unwrap(), WsIn::Text(t) if t == "first"));
        assert!(matches!(ws.poll(IDLE).unwrap(), WsIn::Text(t) if t == "second"));
    }

    /// A close frame mid-fragmentation ends the stream — never a half message reported as whole.
    #[test]
    fn a_close_mid_fragmentation_reports_closed_not_a_partial_message() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(false, 0x1, b"never-finished")).unwrap();
        srv.write_all(&server_frame(true, 0x8, b"")).unwrap();
        assert!(matches!(ws.poll(IDLE).unwrap(), WsIn::Closed));
    }

    /// A peer that opens a NEW data frame while a message is still fragmented has broken framing;
    /// the stream must error rather than silently splice two messages together.
    #[test]
    fn an_interleaved_data_frame_is_a_protocol_error() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(false, 0x1, b"open")).unwrap();
        srv.write_all(&server_frame(true, 0x1, b"other")).unwrap();
        assert!(ws.poll(IDLE).is_err(), "interleaved data frames must not splice");
    }

    /// A continuation frame with no message open is a framing violation whichever way FIN is set.
    /// The FIN=1 form is the dangerous one: without this guard its payload would be returned as a
    /// complete text message conjured from nothing.
    #[test]
    fn a_lone_continuation_frame_is_a_protocol_error_fin_or_not() {
        for fin in [true, false] {
            let (mut ws, mut srv) = ws_pair();
            srv.write_all(&server_frame(fin, 0x0, b"from-nowhere")).unwrap();
            assert!(
                ws.poll(IDLE).is_err(),
                "a continuation with nothing to continue must error (fin={fin})"
            );
        }
    }

    /// Nothing to read: `poll` returns Idle at a frame boundary (so callers can interleave sends),
    /// and a binary frame is skipped rather than lossily decoded as text.
    #[test]
    fn idle_and_binary_frames_are_handled_without_inventing_text() {
        let (mut ws, mut srv) = ws_pair();
        assert!(matches!(ws.poll(Duration::from_millis(50)).unwrap(), WsIn::Idle));
        srv.write_all(&server_frame(true, 0x2, &[0xff, 0x00, 0xfe])).unwrap();
        srv.write_all(&server_frame(true, 0x1, b"after-binary")).unwrap();
        match ws.poll(IDLE).unwrap() {
            WsIn::Text(t) => assert_eq!(t, "after-binary", "the binary frame contributed nothing"),
            other => panic!("expected the text message, got {other:?}"),
        }
    }

    /// A binary frame is a DATA frame: opening one mid-fragmentation breaks framing just like an
    /// interleaved text frame. Skipping it (the earlier behaviour) would leave the partial message
    /// open and splice the next continuation onto a message the peer already abandoned.
    #[test]
    fn a_binary_frame_inside_a_fragmented_message_is_a_protocol_error() {
        let (mut ws, mut srv) = ws_pair();
        srv.write_all(&server_frame(false, 0x1, b"text-so-far")).unwrap();
        srv.write_all(&server_frame(true, 0x2, &[0x00, 0x01])).unwrap();
        assert!(
            ws.poll(IDLE).is_err(),
            "a binary frame may not interrupt a fragmented text message"
        );
    }
}
