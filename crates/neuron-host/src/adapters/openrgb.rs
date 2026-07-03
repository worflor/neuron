//! OpenRGB SDK protocol adapter (server side) — the proving ground.
//!
//! Implements the OpenRGB network protocol (TCP 6742) as a PURE per-connection
//! state machine: [`OrgbConn::feed`] takes raw bytes in and returns reply bytes
//! out; kernel effects go through `&mut dyn HostApi`; time is injected. The
//! socket pump lives in the process shell and stays dumb — which is what makes
//! captured client traffic replayable in unit tests.
//!
//! Being an OpenRGB *server* means Home Assistant's official OpenRGB
//! integration, openrgb-python, and every community effect script can drive
//! neuron's devices with zero neuron-specific code — the ecosystem plug-in
//! story from the R&D doc §4.3.
//!
//! Wire format verified byte-for-byte against the OpenRGB sources
//! (NetworkProtocol.h/.cpp, RGBController.h/.cpp GetDeviceDescription /
//! ReadDeviceDescription, NetworkServer.cpp handlers) and cross-checked
//! against Documentation/OpenRGBSDK.md and the openrgb-python client — see
//! the R&D session's protocol-research report. Key facts encoded here:
//!
//! - 16-byte header: `"ORGB"` magic, u32 dev_idx, u32 pkt_id, u32 pkt_size;
//!   native byte order in the reference impl == little-endian in practice.
//! - Strings: u16 length INCLUDING the NUL terminator, bytes end with `\0`.
//! - RGBColor on the wire: bytes `[R, G, B, 0x00]` (`ToRGBColor` packs
//!   `0x00BBGGRR` and it's memcpy'd LE).
//! - Controller-data fields are version-gated: vendor(v1), mode
//!   brightness(v3), zone segments(v4), zone flags + LED alt names +
//!   controller flags(v5). We support v5 and serialize at whatever version
//!   the client asks for in each REQUEST_CONTROLLER_DATA payload.
//! - A protocol-0 server sends NO reply to id 40; we're v5, so we always
//!   reply with our version (the reference server replies unconditionally
//!   with its own).
//!
//! OWNERSHIP MODEL: an OpenRGB connection is a session. Its paint claims are
//! `LeaseSpec::Pinned` — the OpenRGB protocol has no heartbeat, and a client
//! that sets a static color rightly expects it to stay while it's connected —
//! and the pump MUST call [`OrgbConn::disconnected`] when the socket drops,
//! which releases the whole footprint. After disconnect the arbiter falls
//! back to the base stack: neuron's configured lighting returns, which is
//! *more* correct than the racing free-for-all this ecosystem is used to.

use std::collections::HashMap;
use std::time::Instant;

use crate::api::{HostApi, LeaseSpec, SurfaceInfo, SurfaceKind};
use crate::arbiter::{band, Content, LayerId, Rgb, SourceId};
use crate::bus::Value;

pub const MAGIC: &[u8; 4] = b"ORGB";
/// Highest protocol version we speak (fields gated per-request below).
pub const PROTOCOL_VERSION: u32 = 5;
/// Sanity cap on a single packet payload; a corrupt/hostile length field
/// drops the buffer instead of allocating gigabytes.
const MAX_PAYLOAD: usize = 1 << 20;

pub mod ids {
    pub const REQUEST_CONTROLLER_COUNT: u32 = 0;
    pub const REQUEST_CONTROLLER_DATA: u32 = 1;
    pub const REQUEST_PROTOCOL_VERSION: u32 = 40;
    pub const SET_CLIENT_NAME: u32 = 50;
    pub const DEVICE_LIST_UPDATED: u32 = 100;
    pub const REQUEST_PROFILE_LIST: u32 = 150;
    pub const REQUEST_PLUGIN_LIST: u32 = 200;
    pub const UPDATELEDS: u32 = 1050;
    pub const UPDATEZONELEDS: u32 = 1051;
    pub const UPDATESINGLELED: u32 = 1052;
    pub const SETCUSTOMMODE: u32 = 1100;
}

/// OpenRGB `device_type` enum values (RGBController.h).
fn device_type(kind: SurfaceKind) -> i32 {
    match kind {
        SurfaceKind::Keyboard => 5,
        SurfaceKind::Mouse => 6,
        SurfaceKind::Mousepad => 7,
        SurfaceKind::Headset => 8,
        SurfaceKind::Keypad => 18,
        SurfaceKind::Generic => 21, // UNKNOWN — honest, not a guess
    }
}

/// Frame an OpenRGB packet (used for replies here, and by the socket pump,
/// integration tests, and the future client mode).
pub fn packet(dev_idx: u32, pkt_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&dev_idx.to_le_bytes());
    out.extend_from_slice(&pkt_id.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Wire color bytes `[R, G, B, 0]` → Rgb.
fn read_color(b: &[u8]) -> Rgb {
    Rgb(b[0], b[1], b[2])
}

/// Little-endian byte writer for the controller-data block.
struct W(Vec<u8>);

impl W {
    fn new() -> W {
        W(Vec::new())
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// OpenRGB string: u16 length incl. NUL, then bytes, then NUL.
    fn str(&mut self, s: &str) {
        self.u16((s.len() + 1) as u16);
        self.0.extend_from_slice(s.as_bytes());
        self.0.push(0);
    }
    fn color(&mut self, c: Rgb) {
        self.0.extend_from_slice(&[c.0, c.1, c.2, 0]);
    }
}

/// Serialize one surface as an OpenRGB controller-data block at `ver`,
/// with `colors` as the current per-LED state (the kernel's resolved truth —
/// an OpenRGB client reading device state sees what is actually painted).
fn serialize_controller(info: &SurfaceInfo, colors: &[Rgb], ver: u32) -> Vec<u8> {
    let mut w = W::new();
    w.i32(device_type(info.kind));
    w.str(&info.name);
    if ver >= 1 {
        w.str("Neuron");
    }
    w.str("neuron-host surface");
    w.str(env!("CARGO_PKG_VERSION"));
    w.str(""); // serial
    w.str(&info.key); // location = the stable device key

    // One mode: "Direct", per-LED color. flags bit 5 = HAS_PER_LED_COLOR,
    // color_mode 1 = MODE_COLORS_PER_LED (RGBController.h).
    w.u16(1); // num_modes
    w.i32(0); // active_mode
    w.str("Direct");
    w.i32(0); // value
    w.u32(1 << 5); // flags: HAS_PER_LED_COLOR
    w.u32(0); // speed_min
    w.u32(0); // speed_max
    if ver >= 3 {
        w.u32(0); // brightness_min
        w.u32(0); // brightness_max
    }
    w.u32(0); // colors_min
    w.u32(0); // colors_max
    w.u32(0); // speed
    if ver >= 3 {
        w.u32(0); // brightness
    }
    w.u32(0); // direction
    w.u32(1); // color_mode: PER_LED
    w.u16(0); // num_colors (mode-specific)

    // One zone spanning the whole surface; MATRIX (2) with an identity map if
    // the surface is a grid, LINEAR (1) otherwise.
    w.u16(1); // num_zones
    w.str("Main");
    match info.grid {
        Some(g) => {
            w.i32(2); // ZONE_TYPE_MATRIX
            w.u32(info.leds as u32);
            w.u32(info.leds as u32);
            w.u32(info.leds as u32);
            // matrix_len = 2*u32 (h,w) + h*w*u32 map
            w.u16((8 + 4 * g.rows * g.cols) as u16);
            w.u32(g.rows as u32);
            w.u32(g.cols as u32);
            for i in 0..(g.rows * g.cols) {
                w.u32(i as u32); // identity: cell (r,c) = LED r*cols+c
            }
        }
        None => {
            w.i32(1); // ZONE_TYPE_LINEAR
            w.u32(info.leds as u32);
            w.u32(info.leds as u32);
            w.u32(info.leds as u32);
            w.u16(0); // no matrix
        }
    }
    if ver >= 4 {
        w.u16(0); // num_segments
    }
    if ver >= 5 {
        w.u32(0); // zone_flags
    }

    w.u16(info.leds as u16); // num_leds
    for i in 0..info.leds {
        w.str(&format!("LED {i}"));
        w.u32(i as u32); // value = index
    }

    w.u16(colors.len() as u16); // num_colors
    for c in colors {
        w.color(*c);
    }

    if ver >= 5 {
        w.u16(0); // num_led_alt_names
        w.u32(1); // controller flags: LOCAL
    }

    // Leading data_size counts itself.
    let mut out = Vec::with_capacity(4 + w.0.len());
    out.extend_from_slice(&((w.0.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(&w.0);
    out
}

struct Shadow {
    layer: LayerId,
    cells: Vec<Option<Rgb>>,
}

/// One client connection's protocol state machine.
pub struct OrgbConn {
    buf: Vec<u8>,
    owner: SourceId,
    client_name: String,
    /// The client's advertised version, clamped to ours (recorded from id 40;
    /// a client that never negotiates is treated as protocol 0).
    pub client_version: u32,
    shadows: HashMap<u32, Shadow>,
}

impl OrgbConn {
    pub fn new(host: &mut dyn HostApi) -> OrgbConn {
        OrgbConn {
            buf: Vec::new(),
            owner: host.next_source(),
            client_name: String::new(),
            client_version: 0,
            shadows: HashMap::new(),
        }
    }

    /// Feed raw socket bytes; returns reply bytes to write back. Tolerates
    /// partial packets (buffers) and garbage (hunts for the magic, as the
    /// reference client/server both do).
    pub fn feed(&mut self, bytes: &[u8], host: &mut dyn HostApi, now: Instant) -> Vec<u8> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        loop {
            // Resync to magic if needed.
            if self.buf.len() >= 4 && &self.buf[..4] != MAGIC {
                match self.buf.windows(4).position(|w| w == MAGIC) {
                    Some(pos) => {
                        self.buf.drain(..pos);
                    }
                    None => {
                        // Keep the last 3 bytes — they may be a magic prefix.
                        let keep = self.buf.len().saturating_sub(3);
                        self.buf.drain(..keep);
                        break;
                    }
                }
            }
            if self.buf.len() < 16 {
                break;
            }
            let dev_idx = u32le(&self.buf[4..8]);
            let pkt_id = u32le(&self.buf[8..12]);
            let size = u32le(&self.buf[12..16]) as usize;
            if size > MAX_PAYLOAD {
                // Corrupt or hostile length: drop the buffer, force resync.
                self.buf.clear();
                break;
            }
            if self.buf.len() < 16 + size {
                break;
            }
            let payload: Vec<u8> = self.buf[16..16 + size].to_vec();
            self.buf.drain(..16 + size);
            self.dispatch(dev_idx, pkt_id, &payload, host, now, &mut out);
        }
        out
    }

    /// This connection's kernel-issued owner id — how the host's status readout
    /// attributes arbiter claims to THIS client, exactly.
    pub fn owner(&self) -> SourceId {
        self.owner
    }

    /// The name the client announced via SET_CLIENT_NAME ("" until it does).
    pub fn client_name(&self) -> &str {
        &self.client_name
    }

    /// The socket dropped. Releases this connection's entire paint footprint —
    /// the arbiter falls back to whatever is underneath. The pump MUST call
    /// this; it is the OpenRGB equivalent of the Chroma heartbeat lapse.
    pub fn disconnected(&mut self, host: &mut dyn HostApi) {
        host.release_owner(self.owner);
        self.shadows.clear();
        if !self.client_name.is_empty() {
            host.publish("host.openrgb.disconnected", Value::Text(self.client_name.clone()));
        }
    }

    /// Re-establish this connection's paint after a kernel rebirth. A contained
    /// kernel fault sweeps EVERY lease (leases are never reborn — sessions must
    /// re-claim), but this TCP connection survives with its old owner id. An
    /// active client recovers on its next `paint` (set-or-claim), but a SILENT
    /// one — a config tool that set a colour once and idled — would stay
    /// "connected but dark" indefinitely. The pump calls this each idle tick:
    /// any shadowed layer whose lease has vanished (a PINNED OpenRGB claim only
    /// disappears on a sweep, i.e. a rebirth) is re-claimed from its retained
    /// cells, and the client label is re-applied (the reborn kernel's labels map
    /// is empty). A no-op when nothing is painted or every layer is still alive.
    pub fn reassert(&mut self, host: &mut dyn HostApi, now: Instant) {
        if self.shadows.is_empty() {
            return;
        }
        // Probe liveness; a false refresh on a Pinned lease means it was swept.
        let mut dead: Vec<u32> = Vec::new();
        for (dev_idx, shadow) in &self.shadows {
            if !host.refresh(shadow.layer, now) {
                dead.push(*dev_idx);
            }
        }
        if dead.is_empty() {
            return;
        }
        if !self.client_name.is_empty() {
            host.label_source(self.owner, &self.client_name);
        }
        let infos = host.surfaces();
        for dev_idx in dead {
            // The dead layer id is gone; drop the stale shadow and re-claim fresh.
            let cells = self.shadows.remove(&dev_idx).map(|s| s.cells).unwrap_or_default();
            let Some(info) = infos.get(dev_idx as usize) else { continue };
            if let Some(layer) = host.claim(
                &info.key,
                self.owner,
                band::SESSION,
                LeaseSpec::Pinned,
                Content::Cells(cells.clone()),
                now,
            ) {
                self.shadows.insert(dev_idx, Shadow { layer, cells });
            }
        }
    }

    fn dispatch(
        &mut self,
        dev_idx: u32,
        pkt_id: u32,
        payload: &[u8],
        host: &mut dyn HostApi,
        now: Instant,
        out: &mut Vec<u8>,
    ) {
        match pkt_id {
            ids::REQUEST_CONTROLLER_COUNT => {
                let count = host.surfaces().len() as u32;
                out.extend_from_slice(&packet(0, pkt_id, &count.to_le_bytes()));
            }
            ids::REQUEST_CONTROLLER_DATA => {
                // Protocol ≥1 clients send the effective version to serialize
                // at; a protocol-0 client sends an empty payload.
                let ver = if payload.len() >= 4 {
                    u32le(payload).min(PROTOCOL_VERSION)
                } else {
                    0
                };
                let infos = host.surfaces();
                if let Some(info) = infos.get(dev_idx as usize) {
                    let colors = current_colors(info, host, now);
                    let block = serialize_controller(info, &colors, ver);
                    out.extend_from_slice(&packet(dev_idx, pkt_id, &block));
                }
                // Out-of-range index: no reply. Conformant clients only ask
                // for idx < count.
            }
            ids::REQUEST_PROTOCOL_VERSION => {
                if payload.len() >= 4 {
                    self.client_version = u32le(payload).min(PROTOCOL_VERSION);
                }
                out.extend_from_slice(&packet(0, pkt_id, &PROTOCOL_VERSION.to_le_bytes()));
            }
            ids::SET_CLIENT_NAME => {
                let end = payload.iter().position(|b| *b == 0).unwrap_or(payload.len());
                self.client_name = String::from_utf8_lossy(&payload[..end]).into_owned();
                if !self.client_name.is_empty() {
                    // Name the source so the GUI's ownership truth carries it.
                    host.label_source(self.owner, &self.client_name);
                }
                host.publish("host.openrgb.client", Value::Text(self.client_name.clone()));
                // No reply, per the reference server.
            }
            ids::UPDATELEDS => {
                // u32 data_size, u16 num_colors, colors.
                if payload.len() >= 6 {
                    let n = u16le(&payload[4..6]) as usize;
                    let colors = parse_colors(&payload[6..], n);
                    self.apply_full(dev_idx, colors, host, now);
                }
            }
            ids::UPDATEZONELEDS => {
                // u32 data_size, u32 zone_idx, u16 num_colors, colors.
                if payload.len() >= 10 {
                    let zone = u32le(&payload[4..8]);
                    if zone == 0 {
                        // We expose exactly one zone spanning the surface.
                        let n = u16le(&payload[8..10]) as usize;
                        let colors = parse_colors(&payload[10..], n);
                        self.apply_full(dev_idx, colors, host, now);
                    }
                }
            }
            ids::UPDATESINGLELED => {
                // i32 led_idx, RGBColor — fixed 8 bytes.
                if payload.len() >= 8 {
                    let idx = u32le(&payload[..4]) as usize;
                    let color = read_color(&payload[4..8]);
                    self.apply_single(dev_idx, idx, color, host, now);
                }
            }
            ids::SETCUSTOMMODE => {
                // We are always in "Direct"; nothing to switch. No reply.
            }
            ids::REQUEST_PROFILE_LIST | ids::REQUEST_PLUGIN_LIST => {
                // MUST be answered (audit finding): openrgb-python's
                // constructor BLOCKS on these when the negotiated version
                // unlocks them (profiles at v>=2, plugins at v>=4) — silence
                // means a 10s timeout and OpenRGBDisconnected, taking Home
                // Assistant down with it. The reference server replies even
                // when the lists are empty; both replies share the shape
                // [u32 data_size (counts itself)][u16 count], count = 0 here.
                let mut payload = Vec::with_capacity(6);
                payload.extend_from_slice(&6u32.to_le_bytes());
                payload.extend_from_slice(&0u16.to_le_bytes());
                out.extend_from_slice(&packet(0, pkt_id, &payload));
            }
            _ => {
                // Truly reply-less commands (rescan, profile save/load/delete,
                // zone resize, mode updates): the reference server sends no
                // reply for these either; silence is conformant.
            }
        }
    }

    fn apply_full(
        &mut self,
        dev_idx: u32,
        colors: Vec<Rgb>,
        host: &mut dyn HostApi,
        now: Instant,
    ) {
        let infos = host.surfaces();
        let Some(info) = infos.get(dev_idx as usize) else { return };
        let mut cells: Vec<Option<Rgb>> = colors.into_iter().map(Some).collect();
        cells.truncate(info.leds);
        cells.resize(info.leds, None); // short update: leave the rest unclaimed
        let key = info.key.clone();
        self.paint(dev_idx, &key, cells, host, now);
    }

    fn apply_single(
        &mut self,
        dev_idx: u32,
        idx: usize,
        color: Rgb,
        host: &mut dyn HostApi,
        now: Instant,
    ) {
        let infos = host.surfaces();
        let Some(info) = infos.get(dev_idx as usize) else { return };
        if idx >= info.leds {
            return;
        }
        let mut cells = match self.shadows.get(&dev_idx) {
            Some(s) => s.cells.clone(),
            None => vec![None; info.leds],
        };
        cells[idx] = Some(color);
        let key = info.key.clone();
        self.paint(dev_idx, &key, cells, host, now);
    }

    /// Set-or-claim: the connection's layer for this device gets the new
    /// cells; if the layer vanished (kernel rebirth), re-claim transparently —
    /// the client keeps painting, none the wiser.
    fn paint(
        &mut self,
        dev_idx: u32,
        surface: &str,
        cells: Vec<Option<Rgb>>,
        host: &mut dyn HostApi,
        now: Instant,
    ) {
        if let Some(shadow) = self.shadows.get_mut(&dev_idx) {
            shadow.cells = cells.clone();
            if host.set_content(shadow.layer, Content::Cells(cells.clone()), now) {
                return;
            }
        }
        if let Some(layer) = host.claim(
            surface,
            self.owner,
            band::SESSION,
            LeaseSpec::Pinned,
            Content::Cells(cells.clone()),
            now,
        ) {
            self.shadows.insert(dev_idx, Shadow { layer, cells });
        }
    }
}

fn parse_colors(b: &[u8], n: usize) -> Vec<Rgb> {
    let avail = b.len() / 4;
    (0..n.min(avail)).map(|i| read_color(&b[i * 4..i * 4 + 4])).collect()
}

fn current_colors(info: &SurfaceInfo, host: &mut dyn HostApi, now: Instant) -> Vec<Rgb> {
    match host.resolve(&info.key, now) {
        Some(frame) => frame.into_iter().map(|c| c.unwrap_or(Rgb(0, 0, 0))).collect(),
        None => vec![Rgb(0, 0, 0); info.leds],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::SurfaceInfo;
    use crate::Kernel;

    fn kernel_with_kbd() -> Kernel {
        let mut k = Kernel::new();
        k.declare(SurfaceInfo::grid("kbd", "Test Board", SurfaceKind::Keyboard, 2, 3));
        k
    }

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn version_negotiation_replies_with_ours() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let reply = c.feed(
            &packet(0, ids::REQUEST_PROTOCOL_VERSION, &9u32.to_le_bytes()),
            &mut k,
            now(),
        );
        assert_eq!(&reply[..4], MAGIC);
        assert_eq!(u32le(&reply[8..12]), ids::REQUEST_PROTOCOL_VERSION);
        assert_eq!(u32le(&reply[16..20]), PROTOCOL_VERSION);
        // Client claimed 9; we clamp its recorded version to ours.
        assert_eq!(c.client_version, PROTOCOL_VERSION);
    }

    #[test]
    fn controller_count_and_data_shape() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let reply = c.feed(&packet(0, ids::REQUEST_CONTROLLER_COUNT, &[]), &mut k, now());
        assert_eq!(u32le(&reply[16..20]), 1);

        // v5 request: block structure sanity.
        let reply =
            c.feed(&packet(0, ids::REQUEST_CONTROLLER_DATA, &5u32.to_le_bytes()), &mut k, now());
        assert_eq!(u32le(&reply[4..8]), 0, "reply mirrors dev_idx");
        let block = &reply[16..];
        let declared = u32le(&block[..4]) as usize;
        assert_eq!(declared, block.len(), "data_size counts the whole block including itself");
        assert_eq!(u32le(&block[4..8]) as i32, 5, "device type KEYBOARD");
        // name: u16 len incl NUL, then bytes ending in NUL.
        let name_len = u16le(&block[8..10]) as usize;
        assert_eq!(&block[10..10 + name_len - 1], b"Test Board");
        assert_eq!(block[10 + name_len - 1], 0);

        // v0 must be shorter than v5 (no vendor, no brightness, no v4/v5 tails).
        let v5_len = block.len();
        let reply0 = c.feed(&packet(0, ids::REQUEST_CONTROLLER_DATA, &[]), &mut k, now());
        let v0_len = reply0.len() - 16;
        assert!(v0_len < v5_len, "v0 {v0_len} should be < v5 {v5_len}");
    }

    #[test]
    fn update_leds_lands_in_the_arbiter() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        // 3 colors (of 6 leds): red green blue; rest stays unclaimed.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // data_size (server doesn't re-check here)
        payload.extend_from_slice(&3u16.to_le_bytes());
        for c in [[255u8, 0, 0, 0], [0, 255, 0, 0], [0, 0, 255, 0]] {
            payload.extend_from_slice(&c);
        }
        let reply = c.feed(&packet(0, ids::UPDATELEDS, &payload), &mut k, now());
        assert!(reply.is_empty(), "UpdateLeds has no reply");
        let frame = k.resolve("kbd", now()).unwrap();
        assert_eq!(frame[0], Some(Rgb(255, 0, 0)));
        assert_eq!(frame[1], Some(Rgb(0, 255, 0)));
        assert_eq!(frame[2], Some(Rgb(0, 0, 255)));
        assert_eq!(frame[3], None, "short update leaves the tail unclaimed");
    }

    #[test]
    fn single_led_composes_with_the_connection_shadow() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let mut p = Vec::new();
        p.extend_from_slice(&4u32.to_le_bytes()); // led idx
        p.extend_from_slice(&[7, 8, 9, 0]); // color
        c.feed(&packet(0, ids::UPDATESINGLELED, &p), &mut k, now());
        let frame = k.resolve("kbd", now()).unwrap();
        assert_eq!(frame[4], Some(Rgb(7, 8, 9)));
        assert!(frame.iter().enumerate().all(|(i, c)| i == 4 || c.is_none()));

        // A second single-LED update keeps the first (shadow accumulates).
        let mut p = Vec::new();
        p.extend_from_slice(&1u32.to_le_bytes());
        p.extend_from_slice(&[1, 2, 3, 0]);
        c.feed(&packet(0, ids::UPDATESINGLELED, &p), &mut k, now());
        let frame = k.resolve("kbd", now()).unwrap();
        assert_eq!(frame[4], Some(Rgb(7, 8, 9)));
        assert_eq!(frame[1], Some(Rgb(1, 2, 3)));
    }

    #[test]
    fn disconnect_releases_and_base_shows_through() {
        let mut k = kernel_with_kbd();
        // The user's base lighting: pinned green underneath.
        let base_owner = k.next_source();
        k.claim(
            "kbd",
            base_owner,
            band::BASE,
            LeaseSpec::Pinned,
            Content::Fill(Rgb(0, 255, 0)),
            now(),
        )
        .unwrap();

        let mut c = OrgbConn::new(&mut k);
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u16.to_le_bytes());
        for _ in 0..6 {
            payload.extend_from_slice(&[255, 0, 0, 0]);
        }
        c.feed(&packet(0, ids::UPDATELEDS, &payload), &mut k, now());
        assert_eq!(k.resolve("kbd", now()).unwrap()[0], Some(Rgb(255, 0, 0)), "client owns");

        c.disconnected(&mut k);
        let frame = k.resolve("kbd", now()).unwrap();
        assert!(
            frame.iter().all(|c| *c == Some(Rgb(0, 255, 0))),
            "base returns after disconnect — the whole thesis, at wire level"
        );
    }

    #[test]
    fn reassert_recovers_a_silent_clients_paint_after_rebirth() {
        // A client names itself, paints the whole board red, then goes SILENT —
        // exactly the set-and-forget config tool the finding is about.
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        c.feed(&packet(0, ids::SET_CLIENT_NAME, b"hass"), &mut k, now());
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u16.to_le_bytes());
        for _ in 0..6 {
            payload.extend_from_slice(&[255, 0, 0, 0]);
        }
        c.feed(&packet(0, ids::UPDATELEDS, &payload), &mut k, now());
        assert_eq!(k.resolve("kbd", now()).unwrap()[0], Some(Rgb(255, 0, 0)));

        // Kernel rebirth: a fresh kernel with the surface re-declared but every
        // lease swept (leases are never reborn). The connection survives with its
        // owner id — but a bare reborn kernel shows nothing until it re-claims.
        let mut reborn = kernel_with_kbd();
        assert_eq!(reborn.resolve("kbd", now()).unwrap()[0], None, "reborn kernel starts dark");

        // The idle pump tick reasserts: a silent client's paint returns on its own,
        // and its ownership label is restored on the reborn kernel.
        c.reassert(&mut reborn, now());
        let frame = reborn.resolve("kbd", now()).unwrap();
        assert!(
            frame.iter().all(|cell| *cell == Some(Rgb(255, 0, 0))),
            "silent client's paint recovers after rebirth without fresh traffic"
        );
        assert_eq!(reborn.label_of(c.owner()), Some("hass"), "label restored too");
    }

    #[test]
    fn garbage_before_magic_is_survived() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let mut stream = b"\x00\xffnoise".to_vec();
        stream.extend_from_slice(&packet(0, ids::REQUEST_CONTROLLER_COUNT, &[]));
        let reply = c.feed(&stream, &mut k, now());
        assert_eq!(u32le(&reply[16..20]), 1, "resynced to magic and served the request");
    }

    #[test]
    fn split_packets_reassemble() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let pkt = packet(0, ids::REQUEST_CONTROLLER_COUNT, &[]);
        let (a, b) = pkt.split_at(7);
        assert!(c.feed(a, &mut k, now()).is_empty(), "half a header yields nothing");
        let reply = c.feed(b, &mut k, now());
        assert_eq!(u32le(&reply[16..20]), 1);
    }

    #[test]
    fn hostile_length_field_is_dropped() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let mut evil = Vec::new();
        evil.extend_from_slice(MAGIC);
        evil.extend_from_slice(&0u32.to_le_bytes());
        evil.extend_from_slice(&ids::UPDATELEDS.to_le_bytes());
        evil.extend_from_slice(&(u32::MAX).to_le_bytes()); // 4GB "payload"
        assert!(c.feed(&evil, &mut k, now()).is_empty());
        // The connection stays usable afterwards.
        let reply = c.feed(&packet(0, ids::REQUEST_CONTROLLER_COUNT, &[]), &mut k, now());
        assert_eq!(u32le(&reply[16..20]), 1);
    }

    #[test]
    fn zone_update_targets_the_single_zone_only() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        let mk = |zone: u32| {
            let mut p = Vec::new();
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&zone.to_le_bytes());
            p.extend_from_slice(&1u16.to_le_bytes());
            p.extend_from_slice(&[9, 9, 9, 0]);
            p
        };
        c.feed(&packet(0, ids::UPDATEZONELEDS, &mk(5)), &mut k, now());
        assert_eq!(k.resolve("kbd", now()).unwrap()[0], None, "unknown zone ignored");
        c.feed(&packet(0, ids::UPDATEZONELEDS, &mk(0)), &mut k, now());
        assert_eq!(k.resolve("kbd", now()).unwrap()[0], Some(Rgb(9, 9, 9)));
    }

    #[test]
    fn profile_and_plugin_lists_are_answered_with_empty_lists() {
        // Audit CRITICAL: openrgb-python's constructor BLOCKS on these two
        // when the negotiated version unlocks them; silence = 10s timeout +
        // OpenRGBDisconnected (and Home Assistant fails with it). The
        // reference server replies even when empty; shape = [u32 size][u16 0].
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        for id in [ids::REQUEST_PROFILE_LIST, ids::REQUEST_PLUGIN_LIST] {
            let reply = c.feed(&packet(0, id, &[]), &mut k, now());
            assert_eq!(u32le(&reply[8..12]), id);
            assert_eq!(u32le(&reply[12..16]), 6, "payload is exactly [u32 6][u16 0]");
            assert_eq!(u32le(&reply[16..20]), 6, "data_size counts itself");
            assert_eq!(u16le(&reply[20..22]), 0, "empty list");
        }
    }

    #[test]
    fn controller_data_reports_the_kernels_resolved_truth() {
        let mut k = kernel_with_kbd();
        let mut c = OrgbConn::new(&mut k);
        // Paint via the protocol, then read the controller back: the colors
        // array must reflect what is actually resolved — truthful State.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&6u16.to_le_bytes());
        for _ in 0..6 {
            payload.extend_from_slice(&[10, 20, 30, 0]);
        }
        c.feed(&packet(0, ids::UPDATELEDS, &payload), &mut k, now());
        let reply =
            c.feed(&packet(0, ids::REQUEST_CONTROLLER_DATA, &5u32.to_le_bytes()), &mut k, now());
        let block = &reply[16..];
        // The colors array is the last 6*4 bytes before the v5 tail
        // (u16 alt names + u32 flags = 6 bytes).
        let tail = block.len() - 6;
        let colors = &block[tail - 24..tail];
        for i in 0..6 {
            assert_eq!(&colors[i * 4..i * 4 + 4], &[10, 20, 30, 0]);
        }
    }
}
