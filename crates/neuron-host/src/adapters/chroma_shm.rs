//! Chroma shared-memory adapter — the native face of neuron's Chroma server.
//!
//! The REST adapter ([`crate::adapters::chroma`], port 54235) catches network clients.
//! Native games use a different transport: Win32 named shared memory + events under
//! `Global\{GUID}`. When no arbitration service owns those objects, native games
//! silently no-op — so this module makes neuron BE the server: it creates the objects a
//! game opens, holds the arbitration mask so the game paints in full colour, decodes the
//! per-key frames the game writes, and hands them to the arbiter as any other layer.
//! Nothing runs inside the game process (anti-cheat-safe).
//!
//! A game writes a stable per-key STATE, obfuscated per frame by a timestamp-keyed XOR
//! keystream (see [`KEYSTREAM`]); decoding recovers the real image with no smoothing. To
//! serve, we CREATE the [`Origin::ServerCreated`] objects (with the [`SECURITY_SDDL`]
//! DACL so the game can open them) and OPEN the [`Origin::ClientCreated`] ones;
//! [`Origin::OnDemand`] objects appear per live session/device.
//!
//! The Win32 pump ([`server`]) is Windows + `bridge`-gated and needs elevation (the
//! `Global\` objects require `SeCreateGlobalPrivilege`). Everything else — the object
//! map, the frame codec, the decode — is pure, safe Rust: this module `deny`s
//! `unsafe_code` and confines the only `unsafe` (map/create/close kernel objects,
//! view-as-slice) to [`server`], which opts back in explicitly.
#![deny(unsafe_code)]

/// Which side of the channel creates a named object. The creator's counterpart OPENS
/// it — so to be the server we CREATE [`ServerCreated`] and OPEN [`ClientCreated`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Server-owned (present only while a server runs) — WE create these.
    ServerCreated,
    /// Game-owned (present without a server) — WE open these.
    ClientCreated,
    /// Absent until a live session/device brings it up; type unconfirmed.
    OnDemand,
}

/// The NT object type behind a `Global\{GUID}` name (as probed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A file-mapping (shared memory) of the given observed mapped-region size
    /// (bytes; VirtualQuery rounding to allocation granularity).
    Section(usize),
    Event,
    Mutex,
    /// On-demand object whose type was not observed (was absent at capture).
    Unknown,
}

/// One named object in the Chroma shared-memory channel.
#[derive(Clone, Copy, Debug)]
pub struct Obj {
    /// The GUID inside the `Global\{...}` name (uppercase, no braces).
    pub guid: &'static str,
    pub kind: Kind,
    pub origin: Origin,
    /// What the object is for (best-understood role; freeform).
    pub note: &'static str,
}

impl Obj {
    /// The full NT object name the game and server rendezvous on.
    pub fn name(&self) -> String {
        format!("Global\\{{{}}}", self.guid)
    }
}

/// The security descriptor we put on our objects: DACL granting **Everyone
/// (`WD`) GenericAll** — critically including READ. The game's worker maps every
/// object with `MapViewOfFile(FILE_MAP_ALL_ACCESS = 0xf001f)`, which REQUIRES
/// read access; a write-only DACL (the earlier `0x1f0003`, missing the 0x4 read
/// bit) made every neuron object fail to map with ACCESS_DENIED, so the worker
/// bailed before establishing a session and the game never painted. `GA` grants
/// full section/event/mutex access to any opener — verified against the game.
pub const SECURITY_SDDL: &str = "D:(A;;GA;;;WD)";

/// The two rendezvous events the server creates and the game opens first — the
/// signal pair a game waits on / signals to hand off a frame. (Both Events,
/// not sections.)
pub const RENDEZVOUS: [&str; 2] = [
    "60C824F3-B985-436A-BF6E-8FEBC076B2DA",
    "CB3C8DAE-6F34-4A92-8657-191C8FFDB156",
];

/// The 128 KB control section — the device/app registry (holds a `u32` count
/// then UTF-16 device-instance strings), NOT pixels.
pub const CONTROL_SECTION: &str = "96264859-7FDE-4DA1-A433-BB5109D17DB2";

/// The object map — the canonical in-code source of truth. 35 objects: 4
/// client-created, 22 server-created (14 sections + 6 events + 2 mutexes), 9
/// on-demand. Section sizes are EXACT — the game opens our mappings, so a wrong
/// size corrupts its layout.
pub const OBJECTS: &[Obj] = &[
    // ── server-created: rendezvous events (the handshake signal pair) ──
    Obj { guid: "60C824F3-B985-436A-BF6E-8FEBC076B2DA", kind: Kind::Event, origin: Origin::ServerCreated, note: "rendezvous event 1" },
    Obj { guid: "CB3C8DAE-6F34-4A92-8657-191C8FFDB156", kind: Kind::Event, origin: Origin::ServerCreated, note: "rendezvous event 2" },
    // ── server-created: other signal events ──
    Obj { guid: "0DB0CEFA-C51E-4255-87FB-2D36A0159896", kind: Kind::Event, origin: Origin::ServerCreated, note: "server signal event" },
    Obj { guid: "45C97C2C-2D50-4F30-B50E-AFBB1CE22E93", kind: Kind::Event, origin: Origin::ServerCreated, note: "server signal event" },
    Obj { guid: "4D006319-9569-4E38-B0DF-811AA2DF115F", kind: Kind::Event, origin: Origin::ServerCreated, note: "server signal event" },
    Obj { guid: "DA5A60F0-A3C5-4335-A039-BCC6136C61A3", kind: Kind::Event, origin: Origin::ServerCreated, note: "server signal event" },
    // ── server-created: liveness mutexes (present only while the server runs;
    //    resolved by the running-vs-stopped object diff) ──
    Obj { guid: "153ABAD2-474D-4DDF-A39A-A4DE0E66D1C0", kind: Kind::Mutex, origin: Origin::ServerCreated, note: "server-held mutex" },
    Obj { guid: "A114B7A2-5A4C-4DCD-9507-0E3FED86AC35", kind: Kind::Mutex, origin: Origin::ServerCreated, note: "server-held mutex" },
    // ── server-created: sections. Sizes are EXACT: the game opens OUR mapping, so a
    //    wrong size corrupts its layout and it silently no-ops. Do not round these. ──
    Obj { guid: "96264859-7FDE-4DA1-A433-BB5109D17DB2", kind: Kind::Section(130804), origin: Origin::ServerCreated, note: "CONTROL / device roster: u32 count + UTF-16 device-instance strings" },
    Obj { guid: "735A64EB-02D2-498D-954E-23FC11A050A9", kind: Kind::Section(1637048), origin: Origin::ServerCreated, note: "large effect/stream buffer (~1.6 MB)" },
    Obj { guid: "74164FAD-E73C-4FA1-A9AA-70813315ED9C", kind: Kind::Section(40088), origin: Origin::ServerCreated, note: "device buffer — keyboard (device-type 0x01)" },
    Obj { guid: "E26206E3-F6C2-4E07-BA5C-6C214FD726D9", kind: Kind::Section(38808), origin: Origin::ServerCreated, note: "device buffer" },
    Obj { guid: "D4E1A960-872F-4BF8-B09A-9E54F646D7CE", kind: Kind::Section(26932), origin: Origin::ServerCreated, note: "APP REGISTRY: count + per-app {id@+0xc, exe name@+0x14}" },
    Obj { guid: "0DBE78AC-AC93-408F-A27E-8F61EA067B05", kind: Kind::Section(14888), origin: Origin::ServerCreated, note: "device buffer (device-type 0x02)" },
    Obj { guid: "9B7B099A-F7F3-44CE-AB88-28F79D6D273A", kind: Kind::Section(14488), origin: Origin::ServerCreated, note: "device buffer" },
    Obj { guid: "8AE08F8C-BE3E-4248-AB01-0B595960EC3E", kind: Kind::Section(12808), origin: Origin::ServerCreated, note: "device buffer (device-type 0x80)" },
    Obj { guid: "17EFA16B-E476-4E43-A98A-3AA837681741", kind: Kind::Section(12328), origin: Origin::ServerCreated, note: "device buffer (device-type 0x08)" },
    Obj { guid: "0FFE5A62-387E-4360-95A3-5D8D4075780D", kind: Kind::Section(11848), origin: Origin::ServerCreated, note: "device buffer (device-type 0x10)" },
    Obj { guid: "CDB274E2-C50A-4425-8076-1E71550CBE8A", kind: Kind::Section(11048), origin: Origin::ServerCreated, note: "device buffer (device-type 0x04)" },
    Obj { guid: "D42A3EF5-1B8D-4055-B42F-C5D34A9CA4E4", kind: Kind::Section(10568), origin: Origin::ServerCreated, note: "device buffer" },
    Obj { guid: "D41D8537-2D95-4AD2-8A77-51DC00946366", kind: Kind::Section(168), origin: Origin::ServerCreated, note: "SESSION TABLE: 9-slot ring, u32 head@0, {pid,access,0,0}+u64 tick per slot" },
    Obj { guid: "821AA2A2-8215-4A16-BE9D-7CD8CEBDC398", kind: Kind::Section(84), origin: Origin::ServerCreated, note: "SessionInfo (small)" },
    // ── client-created: the game owns these; WE open them. (The per-app
    //    registration mutex `Global\\<exe>_rz` is dynamic — see rz_mutex_name.) ──
    Obj { guid: "0F9297E6-E80C-47E4-9A8B-1237E50484B7", kind: Kind::Event, origin: Origin::ClientCreated, note: "client signal event" },
    Obj { guid: "89811F96-91C2-4C19-8E0A-54469F491550", kind: Kind::Event, origin: Origin::ClientCreated, note: "client signal event" },
    Obj { guid: "A84AF9C8-EFE0-430D-871C-10DA760C2CCD", kind: Kind::Event, origin: Origin::ClientCreated, note: "client signal event" },
    Obj { guid: "5CD8AF82-56E4-4C36-9144-6D04931A522B", kind: Kind::Mutex, origin: Origin::ClientCreated, note: "shared-region guard mutex" },
    // ── on-demand: brought up per live session/device; type unconfirmed ──
    Obj { guid: "1C68F494-B74D-46E5-9A2F-56F8C526A7C9", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "416FA77A-EC97-44AA-9C3C-DFEA2AC245D3", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "5C49A446-0B97-46CA-BD60-EE5CAF8DDD59", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "6D0C47C9-E199-48C4-B55D-5298974EF8F3", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "893ED63D-F9D7-472A-AA34-FFB18CF28C55", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "9FE422BE-A752-4F67-9EC6-11ED6135478E", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "A966C3C0-231A-4BE5-9C90-5E0C80349891", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
    Obj { guid: "B8B918C0-9790-47F2-AC7A-F36B8414140C", kind: Kind::Unknown, origin: Origin::OnDemand, note: "client-internal wake event" },
    Obj { guid: "FFED75C2-17DC-4886-AA2C-DBAF1F662351", kind: Kind::Unknown, origin: Origin::OnDemand, note: "per session/device" },
];

/// The record terminator / next-record marker seen between per-device records
/// inside a section. The last byte is a stable `0x0c` delimiter; the preceding
/// three bytes are the **per-session handle** (e.g. `bd c8 04` in Capture
/// Session 1, `f6 b3 b7` / `95 33 a1` in the live-Overwatch capture), NOT a
/// constant. So records are split on the `0x0c`-terminated word, and the handle
/// ties a frame to its owning app session (see [`D41D8537` session table]).
pub const RECORD_MARKER_DELIM: u8 = 0x0c;

/// Kept for back-compat: the Capture-Session-1 marker instance. Prefer
/// [`RECORD_MARKER_DELIM`] — the first three bytes vary per session.
pub const RECORD_MARKER: [u8; 4] = [0xbd, 0xc8, 0x04, 0x0c];

// ─────────────────────────── frame codec ───────────────────────────
//
// Decoded from real Overwatch frames captured live against SDK 3.37 (the fixtures
// under `chroma_shm_data/`, verified by the tests below). A device
// section begins with a record header, then a grid of 4-byte colour units:
//
//   +0x00  u32  sequence / frame counter
//   +0x04  u32  reserved (0)
//   +0x08  u16  magic = 0xffff
//   +0x0a  u8   device-type byte (01=keyboard, 02/04/08/10/80 = other classes)
//   +0x0b  u8   0x00
//   +0x0c  u32  param (observed 0x10)
//   +0x10 .. GRID_OFFSET  zero padding
//   +0x50  [u8;4] * N     colour grid (one unit per LED)
//   ...    `<u32 zero><3-byte session handle><0x0c>`  record delimiter, repeat
//
// KNOWN-GOOD here: header parse, magic/device-type, grid unit extraction — all
// asserted against the captured bytes. STILL OPEN (needs a *non-uniform* known
// input; Overwatch was showing a solid colour so every unit is identical):
// (a) the exact per-record LED count / grid geometry, and (b) the RGB byte-order
// inside a unit (the 4th byte varies with colour, so it is NOT a constant flag).
// Those are deliberately NOT guessed — [`ColorUnit::raw`] exposes the bytes as-is.

/// Read a NUL-terminated UTF-16LE string starting at `off` (used for the app
/// registry exe name and the device-roster instance strings).
fn read_utf16z(buf: &[u8], off: usize) -> Option<String> {
    let mut u16s = Vec::new();
    let mut i = off;
    while i + 1 < buf.len() {
        let c = u16::from_le_bytes([buf[i], buf[i + 1]]);
        if c == 0 {
            break;
        }
        u16s.push(c);
        i += 2;
    }
    if u16s.is_empty() {
        return None;
    }
    Some(String::from_utf16_lossy(&u16s))
}

// ─────────────────── control-plane structures ───────────────────
//
// Three server-written sections drive the handshake/arbitration, decoded from a
// live Overwatch session (fixtures + tests below). To BE the server, neuron
// READS these to learn who is connected and WRITES them to grant a session.

/// The active-session / priority table ([`D41D8537`]): tells the server which
/// app is currently painting, and the session handle that tags its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionTable {
    /// Active-session count at `+0x00` (0 = nobody painting).
    pub active_count: u32,
    /// Session id at `+0x08` — matches the app-registry record's id field.
    pub session_id: u32,
    /// Session handle at `+0x10` — the value that tags this app's frame records
    /// (its last byte is the [`RECORD_MARKER_DELIM`]).
    pub session_handle: u32,
}

/// Parse the session/priority table. `None` if too short or no active session.
pub fn parse_session_table(buf: &[u8]) -> Option<SessionTable> {
    if buf.len() < 0x14 {
        return None;
    }
    let active_count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    if active_count == 0 {
        return None;
    }
    Some(SessionTable {
        active_count,
        session_id: u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]),
        session_handle: u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]),
    })
}

/// One registered Chroma app in the app registry ([`D4E1A960`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppEntry {
    /// The app/session id at record `+0x0c` (matches [`SessionTable::session_id`]).
    pub id: u32,
    /// The registered executable name (UTF-16 at record `+0x14`).
    pub name: String,
}

/// Offset of the first app record and the stride between records in [`D4E1A960`].
pub const APP_REGISTRY_RECORD0: usize = 0x200;
pub const APP_REGISTRY_STRIDE: usize = 0x210;

/// Parse the app registry into its populated entries (id + exe name).
pub fn parse_app_registry(buf: &[u8]) -> Vec<AppEntry> {
    let count = if buf.len() >= 4 {
        u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize
    } else {
        0
    };
    let mut out = Vec::new();
    let mut rec = APP_REGISTRY_RECORD0;
    while out.len() < count && rec + 0x14 < buf.len() {
        let id = u32::from_le_bytes([buf[rec + 0xc], buf[rec + 0xd], buf[rec + 0xe], buf[rec + 0xf]]);
        if id != 0 {
            // Keep the entry on a valid id/PID even when the exe-name field is blank:
            // against OUR server the client writes its PID but the name stays empty
            // (Razer's server is what fills it), and the PID is the reliable identity we
            // key presence on. `name` is best-effort.
            let name = read_utf16z(buf, rec + 0x14).unwrap_or_default();
            out.push(AppEntry { id, name });
        }
        rec += APP_REGISTRY_STRIDE;
    }
    out
}

/// The device roster ([`CONTROL_SECTION`] / `96264859`): a `u32` header then
/// UTF-16 device-instance strings. Returns `(header, first_instance)`.
pub fn parse_roster(buf: &[u8]) -> Option<(u32, String)> {
    if buf.len() < 6 {
        return None;
    }
    let header = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let first = read_utf16z(buf, 4)?;
    Some((header, first))
}

/// The session table ([`D41D8537`]) and app registry ([`D4E1A960`]) GUIDs.
pub const SESSION_TABLE: &str = "D41D8537-2D95-4AD2-8A77-51DC00946366";
pub const APP_REGISTRY: &str = "D4E1A960-872F-4BF8-B09A-9E54F646D7CE";
/// The client→server and server→client notify events — the vendor's per-frame handshake pair.
/// Neuron CREATES them (so a client's opens resolve to our own handles) but deliberately never
/// pulses them: one-shot activation reaches continuous paint, and pulsing strobes (see the
/// arbiter block's NO PULSE note). Kept as named consts for the object map + tests.
pub const NOTIFY_CLIENT_TO_SERVER: &str = "0DB0CEFA-C51E-4255-87FB-2D36A0159896";
pub const NOTIFY_SERVER_TO_CLIENT: &str = "DA5A60F0-A3C5-4335-A039-BCC6136C61A3";

/// The per-app registration mutex a connecting client creates and *owns* while
/// connected: `Global\<exe>_rz` with the exe name lowercased (e.g.
/// `Global\overwatch_rz`). Enumerating these owned mutexes is how the server
/// learns which apps are live.
pub fn rz_mutex_name(exe_name: &str) -> String {
    // Strip any path, drop the extension, lowercase, append `_rz`.
    let base = exe_name.rsplit(['\\', '/']).next().unwrap_or(exe_name);
    let stem = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base);
    format!("Global\\{}_rz", stem.to_ascii_lowercase())
}

/// Razer device-class bit (the `NN` in a device record's `ff ff NN 00` tag) and
/// the shared section that carries that class's effect frames. From the buffer
/// headers observed in live Overwatch frames + the section sizes.
pub const DEVICE_SECTIONS: &[(u8, &str)] = &[
    (0x01, "74164FAD-E73C-4FA1-A9AA-70813315ED9C"), // keyboard
    (0x02, "0DBE78AC-AC93-408F-A27E-8F61EA067B05"),
    (0x04, "CDB274E2-C50A-4425-8076-1E71550CBE8A"),
    (0x08, "17EFA16B-E476-4E43-A98A-3AA837681741"),
    (0x10, "0FFE5A62-387E-4360-95A3-5D8D4075780D"),
    (0x80, "8AE08F8C-BE3E-4248-AB01-0B595960EC3E"),
];

/// The section GUID that carries a given device-class bit, if known.
pub fn device_section(device_type: u8) -> Option<&'static str> {
    DEVICE_SECTIONS.iter().find(|(t, _)| *t == device_type).map(|(_, g)| *g)
}

/// Byte offset where the colour grid starts inside a device section record.
pub const GRID_OFFSET: usize = 0x50;
/// Grid offset measured from the record's own `ff ff` tag (`GRID_OFFSET - REC0`).
pub const GRID_IN_RECORD: usize = 0x48;
/// Byte offset of the frame's `GetTickCount64` write-time, from the record tag.
/// The low 7 bits of this timestamp are the per-frame XOR phase (see [`KEYSTREAM`]).
pub const TIMESTAMP_IN_RECORD: usize = 0xb90;
/// The `0xffff` magic at record `+0x08` that marks a valid device record.
pub const RECORD_MAGIC: u16 = 0xffff;

/// The protocol's per-frame XOR keystream (512 bytes). A game writes a *stable* per-key
/// state, then obfuscates each ring frame before committing it: every colour byte is
/// XORed with `KEYSTREAM[phase + channel*0x81]`, where `phase = frame_timestamp & 0x7f`
/// (clamped so `phase+0x183 < 512`). The key is uniform across keys and rotates with the
/// millisecond clock, so the raw buffer *looks* like the whole board strobing through
/// random colours while the true state sits still underneath. Un-XOR with the same
/// keystream and phase and the stable image falls straight out — no averaging or
/// smoothing needed. Byte order matches [`ColorUnit::rgb`] (channel 0 = R).
pub const KEYSTREAM: &[u8; 512] = include_bytes!("chroma_shm_data/keystream.bin");
/// Per-channel stride into [`KEYSTREAM`] (R at `phase+0`, G at `+0x81`, B at `+0x102`).
const KEYSTREAM_CHANNEL_STRIDE: usize = 0x81;

/// One 4-byte colour unit as stored in the grid. Byte-order is retained verbatim
/// ([`raw`](Self::raw)); a verified RGB decode awaits a known-input capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorUnit(pub [u8; 4]);

impl ColorUnit {
    /// The raw 4 bytes exactly as they sit in shared memory.
    pub fn raw(self) -> [u8; 4] {
        self.0
    }
    /// True if every byte is zero (an unlit LED / possible record padding).
    pub fn is_zero(self) -> bool {
        self.0 == [0, 0, 0, 0]
    }
    /// Decode to `(R, G, B)`. The grid stores a `COLORREF` per key, so the unit's low
    /// three bytes are `[R, G, B]` in the same channel order the REST adapter's `bgr()`
    /// uses (R = low byte); byte 3 is a per-LED flag we don't consume. Both faces decode
    /// colour identically.
    pub fn rgb(self) -> (u8, u8, u8) {
        (self.0[0], self.0[1], self.0[2])
    }
}

/// The parsed header of a device-section record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    /// Frame counter at `+0x00` (only meaningful on the first record of a section).
    pub sequence: u32,
    /// Device-class byte from the `ff ff NN 00` tag at `+0x0a`.
    pub device_type: u8,
    /// The `+0x0c` param dword (observed `0x10`).
    pub param: u32,
}

/// Parse the record header at the start of `buf`. Returns `None` if `buf` is too
/// short or the `0xffff` magic is absent (i.e. not a populated device record).
pub fn parse_record_header(buf: &[u8]) -> Option<RecordHeader> {
    if buf.len() < GRID_OFFSET {
        return None;
    }
    let magic = u16::from_le_bytes([buf[8], buf[9]]);
    if magic != RECORD_MAGIC {
        return None;
    }
    Some(RecordHeader {
        sequence: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        device_type: buf[10],
        param: u32::from_le_bytes([buf[12], buf[13], buf[14], buf[15]]),
    })
}

/// Read the colour grid of the first record: the 4-byte units from
/// [`GRID_OFFSET`] up to the record delimiter (`<zero word><handle><0x0c>`) or
/// the end of the buffer. Returns the header and the units. `None` if there is
/// no valid populated record (e.g. an all-zero / offline section).
pub fn parse_frame(section: &[u8]) -> Option<(RecordHeader, Vec<ColorUnit>)> {
    // The section is a RING of frame records (each tagged `ff ff NN 00` at its
    // start, `stride` bytes apart). `u32@0` is the WRITE HEAD — the slot the game
    // is currently writing. The last COMPLETE frame is at `head-1`. Reading a fixed
    // slot 0 catches half-written / stale frames mid-rotation = visible strobe on
    // animated effects (a static effect doesn't rotate, so it looked fine). Read
    // the completed head-1 record instead.
    const REC0: usize = 0x08; // record 0's `ff ff` tag
    const GRID_IN_REC: usize = 0x48; // grid offset within a record (REC0+0x48 == GRID_OFFSET)
    if section.len() < REC0 + GRID_IN_REC + 4 {
        return None;
    }
    if u16::from_le_bytes([section[REC0], section[REC0 + 1]]) != RECORD_MAGIC {
        return None;
    }
    // Discover the ring stride from the next record's tag.
    let mut stride = 0usize;
    let mut i = REC0 + 4;
    while i + 4 <= section.len() {
        if section[i] == 0xff && section[i + 1] == 0xff && section[i + 3] == 0
            && (section[i + 2] as usize) < 0x40
        {
            stride = i - REC0;
            break;
        }
        i += 4;
    }
    // Pick the record to read: head-1 of the ring (or slot 0 if single-record).
    let ff = if stride >= GRID_IN_REC + 4 {
        // Ring depth = consecutive valid record tags from REC0, NOT
        // `section_len / stride`. The section has ~12 KB of trailing scratch past
        // the real ring (no record tags there), so the naive division overcounts
        // (13 vs the true 10) and `(head-1) % 13` indexes into that scratch when
        // head wraps — reading garbage = a visible strobe. Counting tags stops at
        // the true depth.
        let n = ring_depth(section, stride).max(1);
        let head = u32::from_le_bytes([section[0], section[1], section[2], section[3]]) as usize;
        let slot = (head + n - 1) % n;
        let off = REC0 + slot * stride;
        if off + GRID_IN_REC + 4 <= section.len()
            && section[off] == 0xff
            && section[off + 1] == 0xff
        {
            off
        } else {
            REC0
        }
    } else {
        REC0
    };
    let header = RecordHeader {
        sequence: u32::from_le_bytes([section[0], section[1], section[2], section[3]]),
        device_type: section[ff + 2],
        param: u32::from_le_bytes([section[ff + 4], section[ff + 5], section[ff + 6], section[ff + 7]]),
    };
    // The grid runs from `ff+0x48` up to the record's end (stride) or a delimiter.
    let grid_start = ff + GRID_IN_REC;
    let grid_end = if stride > GRID_IN_REC {
        (ff + stride).min(section.len())
    } else {
        section.len()
    };
    let grid = &section[grid_start..grid_end];
    let mut units = Vec::new();
    let mut i = 0;
    while i + 4 <= grid.len() {
        let unit = [grid[i], grid[i + 1], grid[i + 2], grid[i + 3]];
        if unit == [0, 0, 0, 0] {
            let next = grid.get(i + 4..i + 8);
            if let Some(n) = next {
                if n[3] == RECORD_MARKER_DELIM && n != [0, 0, 0, 0] {
                    break;
                }
            }
            if grid[i..].iter().all(|&b| b == 0) {
                break;
            }
        }
        units.push(ColorUnit(unit));
        i += 4;
    }
    if units.is_empty() {
        return None;
    }
    Some((header, units))
}

/// Count the ring's real depth: consecutive records (stride apart from `REC0`) that
/// still carry a valid `ff ff <same-device> 00` tag. Stops at the first slot without
/// one — i.e. where the ring ends and the section's trailing scratch begins. The
/// keyboard ring measures 10 this way; the naive `section_len / stride` would say 13.
fn ring_depth(section: &[u8], stride: usize) -> usize {
    const REC0: usize = 0x08;
    const GRID_IN_REC: usize = 0x48;
    if stride == 0 {
        return 1;
    }
    let dt0 = section[REC0 + 2];
    let mut n = 0usize;
    while REC0 + n * stride + GRID_IN_REC + 4 <= section.len() {
        let off = REC0 + n * stride;
        if section[off] == 0xff
            && section[off + 1] == 0xff
            && section[off + 2] == dt0
            && section[off + 3] == 0
        {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Byte offset of the newest complete record (ring slot `head-1`) in a device section,
/// from the write head at `section[0]` and the discovered stride + [`ring_depth`].
/// `None` if the section holds no valid record.
fn newest_slot_ff(section: &[u8]) -> Option<usize> {
    const REC0: usize = 0x08;
    if section.len() < REC0 + GRID_IN_RECORD + 4 {
        return None;
    }
    if u16::from_le_bytes([section[REC0], section[REC0 + 1]]) != RECORD_MAGIC {
        return None;
    }
    let mut stride = 0usize;
    let mut i = REC0 + 4;
    while i + 4 <= section.len() {
        if section[i] == 0xff
            && section[i + 1] == 0xff
            && section[i + 3] == 0
            && (section[i + 2] as usize) < 0x40
        {
            stride = i - REC0;
            break;
        }
        i += 4;
    }
    if stride < GRID_IN_RECORD + 4 {
        return Some(REC0);
    }
    let n = ring_depth(section, stride).max(1);
    let head = u32::from_le_bytes([section[0], section[1], section[2], section[3]]) as usize;
    Some(REC0 + ((head + n - 1) % n) * stride)
}

/// The internal effect-type code a device record carries at `+0x04`, as a readable
/// name. These are the protocol's internal codes (remapped from the public effect enum
/// before writing) — observed against live frames, where `7` = the per-key CUSTOM grid a
/// game paints. Lets neuron see *what* the game is doing, not just the pixels.
pub fn effect_name(code: u32) -> &'static str {
    match code {
        0 => "None",
        1 => "Static",
        2 => "SpectrumCycling",
        3 => "Wave",
        5 => "Breathing",
        6 => "Reactive",
        7 => "Custom",
        8 => "CustomKey",
        0x11 => "CustomExtended",
        _ => "Unknown",
    }
}

/// A device class's live Chroma activity snapshot — see [`ShmServer::device_activity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceActivity {
    /// Razer device-class bit (`0x01` keyboard, `0x02` mouse, `0x04` headset, …).
    pub device_type: u8,
    /// Internal effect-type code from the record's `+0x04` (see [`effect_name`]).
    pub effect_code: u32,
    /// The newest frame's `GetTickCount64` low 32 bits — milliseconds since boot.
    pub timestamp_ms: u32,
}

impl DeviceActivity {
    /// Readable name for this device's current effect.
    pub fn effect(&self) -> &'static str {
        effect_name(self.effect_code)
    }
}

/// The record stride (byte gap between consecutive `ff ff` tags), discovered from the
/// section. Records are a fixed size PER DEVICE — the keyboard's is 0xB98, a mouse's is far
/// smaller (0x1C0) — so nothing about a record's tail is at a fixed absolute offset. `0`
/// when a second tag isn't found (a single-record section).
fn record_stride(section: &[u8]) -> usize {
    const REC0: usize = 0x08;
    let mut i = REC0 + 4;
    while i + 4 <= section.len() {
        if section[i] == 0xff && section[i + 1] == 0xff && section[i + 3] == 0
            && (section[i + 2] as usize) < 0x40
        {
            return i - REC0;
        }
        i += 4;
    }
    0
}

/// The frame timestamp's offset from a record's tag. The `GetTickCount64` write-time is the
/// LAST 8 bytes of each record, i.e. `stride - 8` — which per device works out to the
/// keyboard's `0xB90` but a mouse's `0x1B8`. A fixed offset (the old `TIMESTAMP_IN_RECORD`)
/// only ever worked for the keyboard; every smaller device read `0` there → phase 0 →
/// garbage colours. Falls back to the keyboard constant if the stride can't be found.
fn ts_offset(section: &[u8]) -> usize {
    match record_stride(section) {
        s if s >= 12 => s - 8,
        _ => TIMESTAMP_IN_RECORD,
    }
}

/// The newest record's `(effect_code @+0x04, write_timestamp_ms)` for a device section —
/// the raw telemetry behind [`ShmServer::device_activity`]. The timestamp is the frame's
/// `GetTickCount64` low 32 bits (ms since boot), at [`ts_offset`] into the record, so
/// successive reads give the game's real update cadence and tell live from idle.
pub fn newest_record_meta(section: &[u8]) -> Option<(u32, u32)> {
    let ff = newest_slot_ff(section)?;
    let o = ff + ts_offset(section);
    if o + 4 > section.len() {
        return None;
    }
    let eff = u32::from_le_bytes([
        section[ff + 4], section[ff + 5], section[ff + 6], section[ff + 7],
    ]);
    let ts = u32::from_le_bytes([section[o], section[o + 1], section[o + 2], section[o + 3]]);
    Some((eff, ts))
}

/// The per-frame XOR phase from a record's timestamp: `timestamp & 0x7f`, clamped so
/// `phase + 0x183` stays inside the 512-byte [`KEYSTREAM`] (the writer's
/// `if (0x80 - phase < 4) phase -= 3`).
fn frame_phase(section: &[u8], ff: usize) -> Option<usize> {
    let o = ff + ts_offset(section);
    if o + 4 > section.len() {
        return None;
    }
    let ts = u32::from_le_bytes([section[o], section[o + 1], section[o + 2], section[o + 3]]);
    let mut phase = (ts & 0x7f) as usize;
    if 0x80 - phase < 4 {
        phase -= 3;
    }
    Some(phase)
}

/// Decode the connected game's real, stable per-key state out of the obfuscated ring.
///
/// The game writes a STATE, not an animation: a fixed per-key frame that only changes
/// on real events (hero swap, ability cooldown, ult). But `RzChromaKeyboardData` XOR-
/// obfuscates every committed ring frame with a keystream keyed by the frame's
/// millisecond timestamp (see [`KEYSTREAM`]), so the raw buffer reads as the whole
/// board strobing through random colours while the true state sits still underneath.
///
/// Reading the newest slot and un-XORing it with `KEYSTREAM[phase + channel*0x81]`
/// recovers the exact state — verified against a live match: all 10 ring slots (10
/// different timestamps/phases) decode to a byte-identical image (dark-blue board,
/// amber WASD, teal ability keys). No averaging, no smoothing, zero lag — the strobe
/// was never real, just the cipher.
pub fn parse_frame_decoded(section: &[u8]) -> Option<(RecordHeader, Vec<ColorUnit>)> {
    let (header, raw) = parse_frame(section)?;
    // The newest slot's record tag — the same slot `parse_frame` read — so we key the
    // XOR phase off that frame's own timestamp.
    let ff = newest_slot_ff(section)?;

    let Some(phase) = frame_phase(section, ff) else {
        // No timestamp (an old-format or single capture) → hand back the raw grid.
        return Some((header, raw));
    };
    let k_r = KEYSTREAM[phase] as u8;
    let k_g = KEYSTREAM[phase + KEYSTREAM_CHANNEL_STRIDE] as u8;
    let k_b = KEYSTREAM[phase + 2 * KEYSTREAM_CHANNEL_STRIDE] as u8;

    let units = raw
        .iter()
        .map(|u| {
            let [b0, b1, b2, _] = u.raw();
            ColorUnit([b0 ^ k_r, b1 ^ k_g, b2 ^ k_b, 0])
        })
        .collect();
    Some((header, units))
}

/// The objects WE must create to be the server (create with [`SECURITY_SDDL`]).
pub fn server_objects() -> impl Iterator<Item = &'static Obj> {
    OBJECTS.iter().filter(|o| o.origin == Origin::ServerCreated)
}

/// The objects the game creates and WE open.
pub fn client_objects() -> impl Iterator<Item = &'static Obj> {
    OBJECTS.iter().filter(|o| o.origin == Origin::ClientCreated)
}

/// Every shared-memory section (name + size) we must create, biggest first.
pub fn sections() -> Vec<(&'static str, usize)> {
    let mut v: Vec<(&'static str, usize)> = OBJECTS
        .iter()
        .filter_map(|o| match o.kind {
            Kind::Section(n) if o.origin == Origin::ServerCreated => Some((o.guid, n)),
            _ => None,
        })
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1));
    v
}

// ─────────────────────────── the Win32 pump ───────────────────────────
//
// Being the server is a READ role: create the named objects (so the game has
// something to open and map), then read the device buffers the game writes and
// hand the decoded per-device colour grids to the caller (→ arbiter → HID). All
// decoding is the tested pure logic above; this is only the OS glue. Gated on
// Windows + the `bridge` feature so the kernel and codec stay dependency-free.
/// The Win32 shared-memory pump: the ONE place `unsafe` lives (kernel-object
/// lifecycle + viewing a mapped page as a slice). It opts back into `unsafe_code`
/// that the module otherwise denies; every block is a documented FFI/mapping
/// invariant. Everything it hands out — decoded frames, telemetry — is safe.
#[cfg(all(windows, feature = "bridge"))]
#[allow(unsafe_code)]
pub mod server {
    use super::*;
    use std::io;
    use windows_sys::Win32::Foundation::{
        CloseHandle, LocalFree, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        FILE_MAP_WRITE, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, GetExitCodeProcess, OpenEventW, OpenMutexW, OpenProcess,
        SetEvent, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
        MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// A held security descriptor granting Everyone full access — matches the
    /// real server's DACL so the game can open our objects.
    struct EveryoneSa(PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES);
    impl EveryoneSa {
        fn new() -> io::Result<Self> {
            let sddl = wide(SECURITY_SDDL);
            let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(), 1, &mut psd, std::ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: psd,
                bInheritHandle: 0,
            };
            Ok(EveryoneSa(psd, sa))
        }
        fn ptr(&self) -> *const SECURITY_ATTRIBUTES {
            &self.1
        }
    }
    impl Drop for EveryoneSa {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { LocalFree(self.0 as _) };
            }
        }
    }

    /// A created section mapping we own plus its mapped view.
    struct MappedSection {
        guid: &'static str,
        mapping: HANDLE,
        view: *mut u8,
        size: usize,
    }

    /// Why [`ShmServer::create`] declined.
    #[derive(Debug)]
    pub enum CreateError {
        /// The named objects already exist — the real Razer server is running.
        /// Stand down and let it serve (the OpenRGB "port busy" rule).
        AlreadyServing,
        /// An OS error (usually: not elevated — `Global\` needs SeCreateGlobal).
        Io(io::Error),
    }
    impl std::fmt::Display for CreateError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                CreateError::AlreadyServing => write!(f, "Razer's Chroma server is already running (stand down)"),
                CreateError::Io(e) => write!(f, "{e}"),
            }
        }
    }
    impl std::error::Error for CreateError {}
    impl From<io::Error> for CreateError {
        fn from(e: io::Error) -> Self { CreateError::Io(e) }
    }

    /// The neuron Chroma SHM server: owns the named objects, reads device frames.
    /// In `open` (read-alongside) mode it instead attaches to a live server's
    /// objects; then it creates nothing and `_sa` is `None`.
    pub struct ShmServer {
        sections: Vec<MappedSection>,
        handles: Vec<HANDLE>, // events + mutexes we created (kept alive)
        _sa: Option<EveryoneSa>,
        _mask: Option<MaskGuard>, // the arbitration mask (create-mode only)
    }

    impl ShmServer {
        /// Become the Chroma server: create (or adopt) every server object at its
        /// exact size with the Everyone DACL, and wear the mask. Stands down
        /// ([`CreateError::AlreadyServing`]) ONLY when another server already wears the
        /// mask — the honest "a live server owns this" signal. Section existence is NOT
        /// that signal: a connected game holds the sections open (open-or-create) and
        /// they linger after a prior neuron server exits, so we open-or-create them and
        /// take over rather than refuse to serve.
        pub fn create() -> Result<Self, CreateError> {
            if mask_worn() {
                return Err(CreateError::AlreadyServing);
            }
            let sa = EveryoneSa::new()?;
            let mut sections = Vec::new();
            let mut handles = Vec::new();
            for o in OBJECTS.iter().filter(|o| o.origin == Origin::ServerCreated) {
                let name = wide(&o.name());
                match o.kind {
                    Kind::Section(size) => {
                        // Open-or-create: `CreateFileMappingW` hands back the EXISTING
                        // section on `ALREADY_EXISTS` (a game or a dead server's leftover
                        // mapping) or a fresh one — either way we map and own it. We only
                        // reach here when no server wears the mask, so adopting the
                        // sections can't fight a live server.
                        let mapping = unsafe {
                            CreateFileMappingW(
                                INVALID_HANDLE_VALUE, sa.ptr(), PAGE_READWRITE,
                                0, size as u32, name.as_ptr(),
                            )
                        };
                        if mapping.is_null() {
                            return Err(io::Error::last_os_error().into());
                        }
                        let view = unsafe {
                            MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, size)
                        };
                        if view.Value.is_null() {
                            unsafe { CloseHandle(mapping) };
                            return Err(io::Error::last_os_error().into());
                        }
                        sections.push(MappedSection {
                            guid: o.guid, mapping, view: view.Value as *mut u8, size,
                        });
                    }
                    Kind::Event => {
                        let h = unsafe { CreateEventW(sa.ptr(), 1, 0, name.as_ptr()) };
                        if !h.is_null() { handles.push(h); }
                    }
                    Kind::Mutex => {
                        // Held (owned) so a client liveness probe sees the server alive.
                        let h = unsafe { CreateMutexW(sa.ptr(), 1, name.as_ptr()) };
                        if !h.is_null() { handles.push(h); }
                    }
                    Kind::Unknown => {}
                }
            }
            // Wear the arbitration mask + run the grant/activate arbiter so a real game
            // paints colour against neuron alone (no vendor arbitration). Must come after
            // the objects exist so the game's opens resolve to our own server-created
            // handles; the arbiter writes the grant into these two control sections.
            let find_sec = |guid: &str| {
                sections
                    .iter()
                    .find(|s| s.guid == guid)
                    .map(|s| (s.view as usize, s.size))
                    .unwrap_or((0, 0))
            };
            let appreg = find_sec(APP_REGISTRY);
            let sessinfo = find_sec(SESSION_INFO);
            let keyboard = device_section(0x01).map(|g| find_sec(g)).unwrap_or((0, 0));
            let mask = wear_mask(&sa, appreg, sessinfo, keyboard);
            Ok(ShmServer { sections, handles, _sa: Some(sa), _mask: Some(mask) })
        }

        /// Attach to an ALREADY-RUNNING Chroma server's objects (Razer's real
        /// server) instead of creating them — the **read-alongside** role. neuron
        /// does not serve or handshake; the live server ingests the game's Chroma,
        /// and neuron opens the same device buffers and mirrors them onto the
        /// hardware like any other effect. Opens each section for WRITE, because
        /// the Everyone DACL grants `0x1f0003` (QUERY|MAP_WRITE) but not MAP_READ;
        /// a writable view is still readable. Sections that don't exist yet (game/
        /// on-demand) are simply skipped. Errors only if nothing could be opened
        /// (no server running).
        ///
        /// The app only ever calls [`ShmServer::create`] — read-alongside a live
        /// Razer server would double-write the LEDs, exactly the last-writer-wins
        /// race this whole adapter exists to kill. `open` is kept for capture and
        /// diagnostic tooling that wants to observe a real server's traffic.
        pub fn open() -> io::Result<Self> {
            let mut sections = Vec::new();
            for o in OBJECTS.iter().filter(|o| o.origin == Origin::ServerCreated) {
                if let Kind::Section(size) = o.kind {
                    let name = wide(&o.name());
                    let mapping = unsafe { OpenFileMappingW(FILE_MAP_WRITE, 0, name.as_ptr()) };
                    if mapping.is_null() {
                        continue; // not present (on-demand/per-device) — skip
                    }
                    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, size) };
                    if view.Value.is_null() {
                        unsafe { CloseHandle(mapping) };
                        continue;
                    }
                    sections.push(MappedSection {
                        guid: o.guid, mapping, view: view.Value as *mut u8, size,
                    });
                }
            }
            if sections.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "no Chroma server objects to open — is Razer's Chroma server running?",
                ));
            }
            Ok(ShmServer { sections, handles: Vec::new(), _sa: None, _mask: None })
        }

        /// Snapshot a mapped section's current bytes into an OWNED buffer.
        ///
        /// A Chroma section is interior-mutable shared memory: the connected game writes it
        /// cross-process, and our own `chroma-arbiter` thread writes the app-registry /
        /// SessionInfo pages during activation (see [`write_grant`]). So we must NEVER hand out
        /// a `&[u8]` borrowed into a live page — a reference whose bytes change underneath it is
        /// aliasing UB (it lets the compiler assume the bytes are stable / `noalias`), and it is
        /// exactly the invariant the arbiter's writes would violate. Instead every read takes a
        /// point-in-time VOLATILE copy and the parsers work on that owned snapshot; the mapped
        /// page is only ever touched by raw volatile reads here and raw stores in `write_grant`,
        /// never a Rust reference. A snapshot that races a write lands a torn buffer — the
        /// record magic-word check + stale-carry in the decoders reject it, the same way they
        /// already tolerate the game's own mid-write frames. This is what keeps
        /// `unsafe impl Sync for ShmServer` honest.
        fn section_bytes(&self, guid: &str) -> Option<Vec<u8>> {
            self.sections.iter().find(|s| s.guid == guid).map(|s| {
                let mut buf = vec![0u8; s.size];
                // SAFETY: `s.view` is a `s.size`-byte page from `MapViewOfFile`, kept mapped for
                // `self`'s whole lifetime (only `Drop` unmaps it). Volatile byte reads snapshot
                // it WITHOUT forming a `&`/`&mut` into the page, so a concurrent writer (the game
                // cross-process, or the arbiter thread in-process) can never break a Rust
                // reference invariant — the copy simply catches whatever bytes are live.
                unsafe {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = std::ptr::read_volatile(s.view.add(i));
                    }
                }
                buf
            })
        }

        /// Decode the current frame of every device section that a game has
        /// written, as `(device_type, colour grid)`. Empty until a game paints.
        pub fn read_device_frames(&self) -> Vec<(u8, Vec<ColorUnit>)> {
            DEVICE_SECTIONS
                .iter()
                .filter_map(|(dt, guid)| {
                    let bytes = self.section_bytes(guid)?;
                    let (h, units) = parse_frame(&bytes)?;
                    Some((h.device_type.max(*dt), units))
                })
                .collect()
        }

        /// Like [`read_device_frames`](Self::read_device_frames) but un-XORs each
        /// device's frame with the timestamp keystream (see [`parse_frame_decoded`]),
        /// yielding the game's real, stable per-key state. This is what a live game's
        /// frame must be PAINTED from — the raw single-slot read is obfuscated noise.
        pub fn read_device_frames_decoded(&self) -> Vec<(u8, Vec<ColorUnit>)> {
            DEVICE_SECTIONS
                .iter()
                .filter_map(|(dt, guid)| {
                    let bytes = self.section_bytes(guid)?;
                    let (h, units) = parse_frame_decoded(&bytes)?;
                    Some((h.device_type.max(*dt), units))
                })
                .collect()
        }

        /// Every painted device's current frame as decoded `(device_type, RGB
        /// LEDs)` — the neutral format the arbiter consumes (same shape the REST
        /// adapter produces from effect commands).
        pub fn frames(&self) -> Vec<(u8, Vec<(u8, u8, u8)>)> {
            self.read_device_frames()
                .into_iter()
                .map(|(dt, units)| (dt, units.iter().map(|u| u.rgb()).collect()))
                .collect()
        }

        /// Live introspection of what the connected game is doing per device class —
        /// the effect kind it's painting (Custom/Static/Wave/…) and the millisecond
        /// timestamp of its newest frame. Diff the timestamp across calls for the
        /// game's real update rate; a stalled timestamp means idle. Free telemetry:
        /// it's all in the record header we already read.
        pub fn device_activity(&self) -> Vec<DeviceActivity> {
            DEVICE_SECTIONS
                .iter()
                .filter_map(|(dt, guid)| {
                    let bytes = self.section_bytes(guid)?;
                    let (effect_code, timestamp_ms) = newest_record_meta(&bytes)?;
                    Some(DeviceActivity { device_type: *dt, effect_code, timestamp_ms })
                })
                .collect()
        }

        /// A terse readout of what the connected game is painting RIGHT NOW: how many
        /// device classes currently show a lit (non-black) frame, and the effect kind
        /// (the keyboard's if it's painting, else the first lit device's). `None` when
        /// nothing is lit. Pure telemetry for the CONNECTIONS card — no allocation
        /// beyond the decode it already does.
        pub fn live_summary(&self) -> Option<(usize, &'static str)> {
            let mut lit = 0usize;
            let mut kbd_effect = None;
            let mut any_effect = None;
            for &(dt, guid) in DEVICE_SECTIONS.iter() {
                let Some(bytes) = self.section_bytes(guid) else { continue };
                let Some((effect_code, _)) = newest_record_meta(&bytes) else { continue };
                let Some((_, units)) = parse_frame_decoded(&bytes) else { continue };
                let is_lit = units
                    .iter()
                    .skip(chroma_grid_lead(dt))
                    .any(|u| { let (r, g, b) = u.rgb(); (r | g | b) != 0 });
                if is_lit {
                    lit += 1;
                    let e = effect_name(effect_code);
                    any_effect.get_or_insert(e);
                    if dt == 0x01 {
                        kbd_effect = Some(e);
                    }
                }
            }
            (lit > 0).then(|| (lit, kbd_effect.or(any_effect).unwrap_or("custom")))
        }

        /// One device's decoded frame plus its write timestamp (ms) — the pair a
        /// fading layer needs: the pixels to paint and the clock to tell whether the
        /// game is still actively driving this device.
        pub fn decoded_frame_with_ts(&self, device_type: u8) -> Option<(u32, Vec<ColorUnit>)> {
            let guid = device_section(device_type)?;
            let bytes = self.section_bytes(guid)?;
            let (_, ts) = newest_record_meta(&bytes)?;
            let (_, units) = parse_frame_decoded(&bytes)?;
            Some((ts, units))
        }

        /// The apps currently registered in the app registry (`D4E1A960`).
        pub fn registered_apps(&self) -> Vec<AppEntry> {
            self.section_bytes(APP_REGISTRY).map(|b| parse_app_registry(&b)).unwrap_or_default()
        }

        /// True while any Chroma game is actually running — a registered app whose PID
        /// is a live process. The app registry always carries a connected client's PID
        /// ([`AppEntry::id`]) even when the exe-NAME field is blank against our server,
        /// and the PID tracks the process exactly, so this is the honest "a game is
        /// connected" signal the fading layer gates on: a registry row that lingers
        /// after a game exits has a dead PID, so it never keeps the layer lit.
        pub fn any_client_connected(&self) -> bool {
            self.registered_apps().iter().any(|a| process_alive(a.id))
        }

        /// The most recent session-table entry, if any (`D41D8537`).
        pub fn latest_session(&self) -> Option<SessionTable> {
            self.section_bytes(SESSION_TABLE).and_then(|b| parse_session_table(&b))
        }

        /// True if a specific app (by exe name) holds its `Global\<exe>_rz`
        /// registration mutex — i.e. is currently connected.
        pub fn app_connected(&self, exe_name: &str) -> bool {
            const MUTEX_ALL_ACCESS: u32 = 0x001F_0001;
            let full = wide(&rz_mutex_name(exe_name));
            let h = unsafe { OpenMutexW(MUTEX_ALL_ACCESS, 0, full.as_ptr()) };
            if h.is_null() {
                false
            } else {
                unsafe { CloseHandle(h) };
                true
            }
        }
    }

    /// True if `pid` is a live process — the honest "this game is still running" check
    /// behind [`ShmServer::any_client_connected`]. Opens with the minimal query right
    /// (works when elevated), reads the exit code, and treats `STILL_ACTIVE` as alive; a
    /// PID that has exited or was never valid can't be opened and reads as not-alive.
    fn process_alive(pid: u32) -> bool {
        const STILL_ACTIVE: u32 = 259;
        if pid == 0 {
            return false;
        }
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return false;
        }
        let mut code: u32 = 0;
        let ok = unsafe { GetExitCodeProcess(h, &mut code) };
        unsafe { CloseHandle(h) };
        ok != 0 && code == STILL_ACTIVE
    }

    // ──────────────────────── the arbitration arbiter ────────────────────────
    //
    // A shipping game connects to a bare server, but only STREAMS + paints real per-key
    // colour when it believes the vendor arbitration layer is fully alive AND its session
    // has been GRANTED + ACTIVATED. Neuron supplies all of it, with zero vendor software,
    // on one thread — two responsibilities, both STANDING (nothing on a per-frame timer):
    //   • MASK   — hold the arbitration mutexes (incl. a per-user one) and keep the
    //     arbiter-ready events SIGNALLED (manual-reset, set once), so the game doesn't
    //     self-mute to near-black. These are HELD for the server's life, never re-poked.
    //   • GRANT + ACTIVATE (one-shot per client, see `activate_once`) — the game's session
    //     worker parks on {B8B918C0}; we write the client PID into the app registry (+0x20c)
    //     and SessionInfo slot0 {head=0, event-type=8, session-id} (see `write_grant`),
    //     SetEvent {B8B918C0} to wake the worker, then after a short beat SetEvent the
    //     per-key ACTIVATE event {A84AF9C8} — the signal that flips the board from a uniform
    //     muted frame to real per-key colour. Once activated the game streams its own frames;
    //     we do nothing further but a cheap liveness check on its PID.
    //
    // NO PULSE. An earlier design tapped the server→client notify + rendezvous events at
    // ~60Hz to "keep completing the handshake". That was a DEAD END: it re-drove the session
    // every tick and the game rendered it as a periodic STROBE (one of the flickers this file
    // has since hunted down). The one-shot ACTIVATE above is what actually reaches continuous
    // paint; the notify/rendezvous objects are still CREATED (so the client's opens resolve to
    // our own objects) but deliberately left un-pulsed. Do not re-add a pulse loop.
    // The client is found by scanning for a process with the Chroma client DLL loaded.
    // (Two more arbitration mutexes, 153ABAD2 + A114B7A2, are already created in OBJECTS.)
    const MASK_MUTEXES: &[&str] = &[
        "{B1570C3F-8B14-45B0-BCEB-C57ED1F5C589}",
        "{3DD569A2-BC96-425D-ABEF-A5EF21F4B681}",
        "{08B4F43A-DA51-4120-B388-CE0F8CE6F61A}",
    ];
    /// A per-USER arbitration mutex (suffixed with the interactive username): the vendor
    /// server creates one per logged-in user, and the game checks for it too.
    const MASK_PERUSER_MUTEX: &str = "{63D31EEC-008F-43C9-A58E-ED6949B25A6C}";
    /// Events the game expects the arbitration layer to hold SIGNALLED (manual-reset).
    const MASK_SIGNALLED_EVENTS: &[&str] = &[
        "{86E8A3B0-C718-4997-AF5D-C3677E71F5D8}",
        "{9138EDDD-890B-46E2-8B64-E0037E2B332D}",
        "{798D9FEC-789F-46FD-B3D9-359C8E81DD11}",
        "{841EB9A8-6DA7-479E-94DC-C23B95FFDF43}",
    ];
    /// The client session worker's wake event — SetEvent to deliver the grant.
    const SESSION_WORKER_EVENT: &str = "{B8B918C0-9790-47F2-AC7A-F36B8414140C}";
    /// The per-key ACTIVATION event — SetEvent ~60ms AFTER the grant. This is the signal
    /// that flips the game from a uniform muted/black frame to painting its real per-key
    /// colour. Without it the game registers and streams frames, but every key stays the
    /// muted clear colour (the whole "connects but paints nothing" symptom).
    const ACTIVATE_EVENT: &str = "{A84AF9C8-EFE0-430D-871C-10DA760C2CCD}";
    /// The SessionInfo section the grant writes its {event-type, session-id} slots into.
    const SESSION_INFO: &str = "821AA2A2-8215-4A16-BE9D-7CD8CEBDC398";
    /// The Chroma client DLL a game loads — the marker we scan processes for.
    const CHROMA_CLIENT_DLL: &str = "rzchromasdk64.dll";

    /// True if a Chroma server already wears the mask (holds the first mask mutex). This
    /// is the honest "another live server owns arbitration — stand down" signal: whoever
    /// serves creates these mutexes and they VANISH when it dies (including a prior neuron
    /// server), so — unlike section existence — their presence means a server is actually
    /// live right now.
    fn mask_worn() -> bool {
        let w = wide(&format!("Global\\{}", MASK_MUTEXES[0]));
        let h = unsafe { OpenMutexW(0x0010_0000, 0, w.as_ptr()) }; // SYNCHRONIZE
        if h.is_null() {
            false
        } else {
            unsafe { CloseHandle(h) };
            true
        }
    }

    // A raw handle we promise to use soundly across threads (named kernel objects).
    struct SendHandle(HANDLE);
    unsafe impl Send for SendHandle {}

    /// Owns the arbitration objects + the background arbiter thread. Dropping it stops
    /// the thread (join) THEN releases every held handle — so no tick can fire after the
    /// sections it writes have been torn down.
    pub struct MaskGuard {
        held: Vec<SendHandle>,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for MaskGuard {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
            for h in &self.held {
                unsafe { CloseHandle(h.0) };
            }
        }
    }

    /// Stand up the full arbitration: hold the mutexes + signalled events, then run the
    /// one-shot grant/activate loop on a background thread (NO pulse — see the arbiter
    /// block above). `appreg`/`sessinfo` are the mapped `(pointer, size)` of the
    /// app-registry and SessionInfo sections the grant writes.
    fn wear_mask(
        sa: &EveryoneSa,
        appreg: (usize, usize),
        sessinfo: (usize, usize),
        keyboard: (usize, usize),
    ) -> MaskGuard {
        let mut held = Vec::new();
        // Arbitration mutexes: the three fixed + one per interactive user.
        let user = std::env::var("USERNAME").unwrap_or_default();
        let mut mutex_names: Vec<String> = MASK_MUTEXES.iter().map(|s| (*s).to_string()).collect();
        mutex_names.push(format!("{MASK_PERUSER_MUTEX}{user}"));
        for g in &mutex_names {
            let w = wide(&format!("Global\\{g}"));
            let h = unsafe { CreateMutexW(sa.ptr(), 0, w.as_ptr()) };
            if !h.is_null() {
                held.push(SendHandle(h));
            }
        }
        // Events the game wants held SIGNALLED. Created manual-reset + START signalled and
        // kept alive in `held` for the server's lifetime. A manual-reset event stays set
        // until explicitly reset, so there is nothing to re-assert on a timer — re-poking
        // them would be exactly the periodic tick we want to avoid.
        for g in MASK_SIGNALLED_EVENTS {
            let w = wide(&format!("Global\\{g}"));
            let h = unsafe { CreateEventW(sa.ptr(), 1, 1, w.as_ptr()) }; // manual-reset, START signalled
            if !h.is_null() {
                held.push(SendHandle(h));
            }
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_thread = std::sync::Arc::clone(&stop);
        let thread = crate::worker::spawn_named("chroma-arbiter", move || {
            arbiter_loop(appreg, sessinfo, keyboard, stop_thread)
        })
        .ok();
        MaskGuard { held, stop, thread }
    }

    /// The arbiter: ACTIVATE the connected game ONCE — write its grant, then fire the
    /// per-key activation event — and otherwise stay QUIET. It is deliberately NOT a
    /// heartbeat. Once a game is activated it STAYS activated (its own frames flow), so the
    /// only ongoing work is a cheap liveness check on the known PID; we never re-poke the
    /// session. (An earlier "re-activate if the board looks uniform" recheck was removed: it
    /// occasionally caught the game's own transient clear frame and re-fired the activation
    /// mid-stream, which the game rendered as a periodic flicker.) The expensive
    /// process/module scan runs ONLY while no game is activated yet, gated behind a live
    /// session-worker event so it costs nothing at idle. Re-activation happens only on a
    /// relaunch (a new PID appears after the old one dies).
    fn arbiter_loop(
        appreg: (usize, usize),
        sessinfo: (usize, usize),
        _keyboard: (usize, usize),
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::Ordering;
        let mut activated: Option<u32> = None; // the PID we've activated
        while !stop.load(Ordering::Relaxed) {
            match activated {
                // Activated: the only ongoing work is a cheap liveness check. A dead PID
                // (game closed) → drop it so a relaunch is picked up. Nothing is re-poked.
                Some(pid) => {
                    if !process_alive(pid) {
                        activated = None;
                    }
                }
                // No game yet: the (heavier) scan, but only once a client's session worker
                // is up, so at true idle this is a single cheap event-open.
                None => {
                    if session_worker_present() {
                        if let Some(pid) = find_chroma_client() {
                            unsafe { activate_once(appreg, sessinfo, pid, pid_session(pid)) };
                            activated = Some(pid);
                        }
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(2000));
        }
    }

    /// The decompile-exact ACTIVATION (mirrors the reference `activate2` sequence): register
    /// the client in the app registry, seed one session slot, wake the session worker, then
    /// — after a short beat — fire the per-key [`ACTIVATE_EVENT`]. That last signal is what
    /// flips the board from a uniform muted frame to the game's real per-key colour.
    ///
    /// # Safety
    /// `appreg`/`sessinfo` must be the live mapped views of the app-registry / SessionInfo
    /// sections with the given sizes; only this thread writes them.
    unsafe fn activate_once(appreg: (usize, usize), sessinfo: (usize, usize), pid: u32, sess: u32) {
        write_grant(appreg, sessinfo, pid, sess);
        signal_event(SESSION_WORKER_EVENT);
        std::thread::sleep(std::time::Duration::from_millis(60));
        signal_event(ACTIVATE_EVENT); // A84AF9C8 → per-key colour
    }

    /// The MEMORY half of activation: stamp the grant records into the mapped app-registry and
    /// SessionInfo pages. Split out from [`activate_once`] so the exact byte layout is unit-tested
    /// without the kernel-event signalling + timing. These are raw stores into interior-mutable
    /// shared memory; only the single `chroma-arbiter` thread ever writes these bytes, and readers
    /// snapshot the same pages by volatile copy (see [`ShmServer::section_bytes`]) — so no `&`/
    /// `&mut` is ever formed into a live page and the `unsafe impl Sync` justification holds.
    ///
    /// # Safety
    /// Each `(ptr, size)` must be either `(0, _)` (skipped) or the live mapped view of the named
    /// section with at least the asserted size; `ptr` need not be aligned (`write_unaligned`).
    unsafe fn write_grant(appreg: (usize, usize), sessinfo: (usize, usize), pid: u32, sess: u32) {
        let (ap, ap_size) = appreg;
        if ap != 0 && ap_size >= APP_REGISTRY_RECORD0 + 0x10 {
            let p = ap as *mut u8;
            std::ptr::write_unaligned(p as *mut u32, 1); // app count = 1
            std::ptr::write_unaligned(p.add(APP_REGISTRY_RECORD0 + 0x0c) as *mut u32, pid); // PID @ +0x20c
        }
        let (sp, sp_size) = sessinfo;
        if sp != 0 && sp_size >= 12 {
            let s = sp as *mut u8;
            std::ptr::write_unaligned(s as *mut u32, 0); // head
            std::ptr::write_unaligned(s.add(4) as *mut u32, 8); // slot0 event-type 8 (grant access)
            std::ptr::write_unaligned(s.add(8) as *mut u32, sess); // slot0 session id
        }
    }

    #[cfg(test)]
    mod grant_tests {
        use super::super::APP_REGISTRY_RECORD0;
        use super::write_grant;

        fn u32_at(b: &[u8], o: usize) -> u32 {
            u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
        }

        #[test]
        fn write_grant_stamps_appreg_and_sessioninfo() {
            // The exact byte layout the session worker validates: app count + client PID @ +0x20c,
            // and SessionInfo slot0 {head=0, event-type=8, session-id}. Owned buffers stand in for
            // the mapped pages so the raw stores are checkable without a live server.
            let mut appreg = vec![0u8; APP_REGISTRY_RECORD0 + 0x10];
            let mut sess = vec![0u8; 12];
            // SAFETY: both pointers are into live, correctly-sized owned buffers.
            unsafe {
                write_grant(
                    (appreg.as_mut_ptr() as usize, appreg.len()),
                    (sess.as_mut_ptr() as usize, sess.len()),
                    0x7A11,
                    1,
                );
            }
            assert_eq!(u32_at(&appreg, 0), 1, "app count = 1");
            assert_eq!(u32_at(&appreg, APP_REGISTRY_RECORD0 + 0x0c), 0x7A11, "client PID @ +0x20c");
            assert_eq!(u32_at(&sess, 0), 0, "session head = 0");
            assert_eq!(u32_at(&sess, 4), 8, "slot0 event-type 8 (grant access)");
            assert_eq!(u32_at(&sess, 8), 1, "slot0 session id");
        }

        #[test]
        fn write_grant_skips_null_or_undersized_sections() {
            // The arbiter passes (0, 0) for a section that failed to map, and a short buffer must
            // be bounds-rejected — write_grant must never store through either.
            let mut tiny = vec![0u8; 4];
            // SAFETY: `tiny` is a live 4-byte buffer; the sessinfo arg is the null/skip sentinel.
            unsafe {
                write_grant((tiny.as_mut_ptr() as usize, tiny.len()), (0, 0), 0x1234, 1);
            }
            assert!(tiny.iter().all(|&b| b == 0), "undersized app-registry left untouched");
        }
    }

    /// `SetEvent` a named global event (GUID with braces), if it exists.
    fn signal_event(guid_braced: &str) {
        let w = wide(&format!("Global\\{guid_braced}"));
        let h = unsafe { OpenEventW(0x1F0003, 0, w.as_ptr()) };
        if !h.is_null() {
            unsafe {
                SetEvent(h);
                CloseHandle(h);
            }
        }
    }


    /// True if a client's session worker has created its wake event — a game has inited
    /// its Chroma SDK and is waiting to be granted. Gates the (heavier) client scan.
    fn session_worker_present() -> bool {
        let w = wide(&format!("Global\\{SESSION_WORKER_EVENT}"));
        let h = unsafe { OpenEventW(0x1F0003, 0, w.as_ptr()) };
        if h.is_null() {
            false
        } else {
            unsafe { CloseHandle(h) };
            true
        }
    }

    /// The interactive session id for a pid (games run in the console session, usually 1).
    fn pid_session(pid: u32) -> u32 {
        let mut sess: u32 = 0;
        if unsafe { ProcessIdToSessionId(pid, &mut sess) } != 0 {
            sess
        } else {
            1
        }
    }

    /// Find a live process with the Chroma client DLL loaded — the game to grant. Returns
    /// the first match (the common case is a single game); skips processes we can't
    /// snapshot (bitness / access), which is harmless.
    fn find_chroma_client() -> Option<u32> {
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut pe: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        pe.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = None;
        if unsafe { Process32FirstW(snap, &mut pe) } != 0 {
            loop {
                let pid = pe.th32ProcessID;
                if pid > 4 && process_has_module(pid, CHROMA_CLIENT_DLL) {
                    found = Some(pid);
                    break;
                }
                if unsafe { Process32NextW(snap, &mut pe) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snap) };
        found
    }

    /// Whether `pid` has a module named `dll_lower` (lowercase) loaded.
    fn process_has_module(pid: u32, dll_lower: &str) -> bool {
        let snap =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut me: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        me.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut hit = false;
        if unsafe { Module32FirstW(snap, &mut me) } != 0 {
            loop {
                let end = me.szModule.iter().position(|&c| c == 0).unwrap_or(me.szModule.len());
                let name = String::from_utf16_lossy(&me.szModule[..end]).to_lowercase();
                if name == dll_lower {
                    hit = true;
                    break;
                }
                if unsafe { Module32NextW(snap, &mut me) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snap) };
        hit
    }

    // SAFETY: `ShmServer` shares its mapped views across threads (an `Arc<ShmServer>` backs the
    // per-device arbiter layers, and the `chroma-arbiter` thread activates connected games). The
    // pages are INTERIOR-MUTABLE shared memory — written by the game cross-process and, during
    // activation, by the arbiter thread via `write_grant`. Soundness does NOT rest on "never
    // mutated": it rests on how the memory is TOUCHED. Every read is a volatile snapshot-copy
    // (`section_bytes`) and every write is a raw store (`write_grant`); we never form a `&`/`&mut`
    // into a live page, so no Rust reference invariant is ever exposed to a concurrent writer.
    // Torn reads (a snapshot racing a write) are rejected by the record magic-word check +
    // stale-carry in the decoders, exactly as they already tolerate the game's mid-write frames.
    // The one in-process writer is a single thread the `MaskGuard` joins before unmapping, so no
    // write can outlive the pages. The handles/`Vec`s are otherwise plain owned data.
    unsafe impl Send for ShmServer {}
    unsafe impl Sync for ShmServer {}

    use crate::arbiter::{BlendMode, LiveContent, Rgb};
    use crate::paint::{merge_cells, FadeRamp, PaintPolicy};
    use std::sync::Arc;
    use std::time::Instant;

    /// A live arbiter layer that paints one device class from the connected game's
    /// decoded Chroma state, and FADES itself in and out over the user's base lighting.
    ///
    /// Claim it once per surface at `band::SESSION` (games sit above the base): it lies
    /// dormant (alpha 0, contributes nothing → the user's lighting shows untouched)
    /// until a game connects and paints, then crossfades UP; when the game disconnects
    /// it crossfades back DOWN to the base — no claim/unclaim churn, no hard cut. The
    /// fade is driven by [`alpha`](LiveContent::alpha), which the arbiter blends over
    /// the lower layers. `render` returns the decoded state; the leading grid pad cell
    /// is skipped (`GRID_LEAD`) so unit *i* maps to LED *i*.
    pub struct ChromaShmLayer {
        server: Arc<ShmServer>,
        key: String,
        device_type: u8,
        leds: usize,
        /// Last decoded game frame, carried forward on a torn/absent read.
        last: Vec<Option<Rgb>>,
        /// Framerate-independent crossfade toward present/absent (shared engine).
        ramp: FadeRamp,
        /// Cached "a game process is alive" and when last checked (a syscall, throttled
        /// to [`PRESENCE_POLL`]). One of two independent liveness signals.
        present: bool,
        last_present_check: Option<Instant>,
        /// The last frame timestamp, and WHEN it last advanced. The other liveness
        /// signal: a game actively painting bumps the timestamp every commit. Together
        /// (`present OR fresh`) they make fade-in resilient — a live, decodable, advancing
        /// frame lights up even if the process check is wrong, and the board only fades
        /// out when BOTH say the game is gone.
        last_ts: u32,
        last_ts_change: Option<Instant>,
        /// Shared game-lighting policy: blend, strength, fade, and device scope.
        policy: Arc<PaintPolicy>,
    }

    /// How many leading decoded units to skip before physical LED 0, by device CLASS —
    /// NOT a blind global. A keyboard's grid reserves one cell ahead of its key matrix,
    /// so key 0 is decoded unit 1 (verified live on a BlackWidow). No other class shows
    /// that pad, and the record is otherwise "one unit per LED", so every non-keyboard
    /// class maps unit i → LED i straight. Unverified classes default to 0 so a
    /// mouse/mousepad frame is never shifted a pixel; a real capture can promote it later.
    pub fn chroma_grid_lead(device_type: u8) -> usize {
        match device_type {
            0x01 => 1, // keyboard — the reserved leading cell, confirmed on hardware
            _ => 0,    // mouse / mousepad / headset / keypad / generic — straight map
        }
    }
    /// Seconds for a full 0↔1 crossfade between base and game lighting.
    /// How often to re-check whether a game process is still alive (a syscall, throttled).
    const PRESENCE_POLL: std::time::Duration = std::time::Duration::from_millis(250);
    /// A frame counts as "fresh" if its timestamp advanced within this window; once it
    /// freezes for longer (with no live process either), the layer fades back out.
    const GAME_IDLE_GRACE: std::time::Duration = std::time::Duration::from_millis(1500);

    impl ChromaShmLayer {
        /// `initial_alpha` seeds the crossfade. A first claim (the game just connected) starts at
        /// 0.0 so it fades IN over the base. A RE-CLAIM — the same live game whose Heartbeat lease
        /// lapsed and was swept while it was still painting — starts at 1.0: the game is already
        /// on screen, so rebuilding the layer at 0.0 would spuriously dim the whole board to black
        /// and fade it back, a periodic dip. Callers that know the game is present pass 1.0.
        pub fn new(
            server: Arc<ShmServer>,
            key: String,
            device_type: u8,
            leds: usize,
            policy: Arc<PaintPolicy>,
            initial_alpha: f32,
        ) -> Self {
            ChromaShmLayer {
                server,
                key,
                device_type,
                leds,
                last: vec![None; leds],
                ramp: FadeRamp::new(initial_alpha),
                present: false,
                last_present_check: None,
                last_ts: 0,
                last_ts_change: None,
                policy,
            }
        }
    }

    impl LiveContent for ChromaShmLayer {
        fn render(&mut self, now: Instant) -> Vec<Option<Rgb>> {
            // Decode the newest frame for this device (stable state — no smoothing).
            // The leading-unit skip is per device CLASS (keyboard reserves one; others
            // map straight), so a non-keyboard frame is never shifted a pixel.
            let mut has_frame = false;
            let lead = chroma_grid_lead(self.device_type);
            if let Some((ts, units)) = self.server.decoded_frame_with_ts(self.device_type) {
                if units.len() > lead {
                    let mut cells = vec![None; self.leds];
                    for (i, u) in units.iter().skip(lead).take(self.leds).enumerate() {
                        let (r, g, b) = u.rgb();
                        cells[i] = Some(Rgb(r, g, b));
                    }
                    self.last = cells;
                    has_frame = true;
                }
                // Track frame freshness: a game actively painting advances the timestamp.
                if ts != self.last_ts {
                    self.last_ts = ts;
                    self.last_ts_change = Some(now);
                }
            }

            // Two independent liveness signals, OR'd so fade-in is resilient: the game
            // PROCESS is alive (throttled syscall), or its frame timestamp is still
            // advancing. We only fade out when BOTH say it's gone. A decodable frame
            // (`has_frame`) is the gate — no live pixels, nothing to show.
            let due = self
                .last_present_check
                .is_none_or(|t| now.duration_since(t) >= PRESENCE_POLL);
            if due {
                self.present = self.server.any_client_connected();
                self.last_present_check = Some(now);
            }
            let fresh = self
                .last_ts_change
                .is_some_and(|t| now.duration_since(t) < GAME_IDLE_GRACE);
            let target = if has_frame
                && self.policy.allows_key(&self.key)
                && (self.present || fresh)
            {
                1.0
            } else {
                0.0
            };

            // Ramp alpha toward the target by elapsed time (framerate-independent).
            let value = self.ramp.advance(now, target, self.policy.fade_secs());

            // Fully faded out → contribute nothing (the base shows untouched, cheaply).
            if value <= 0.001 {
                return vec![None; self.leds];
            }
            // Apply the mode-aware black rule so a mostly-black Multiply ("TINT")
            // frame doesn't black out the base — the same rule every face uses.
            merge_cells(self.policy.blend_mode(), &self.last)
        }

        fn alpha(&self) -> f32 {
            self.ramp.value() * self.policy.alpha()
        }

        fn blend_mode(&self) -> BlendMode {
            self.policy.blend_mode()
        }

        fn boxed_clone(&self) -> Box<dyn LiveContent> {
            Box::new(ChromaShmLayer {
                server: Arc::clone(&self.server),
                key: self.key.clone(),
                device_type: self.device_type,
                leds: self.leds,
                last: self.last.clone(),
                ramp: self.ramp.clone(),
                present: self.present,
                last_present_check: self.last_present_check,
                last_ts: self.last_ts,
                last_ts_change: self.last_ts_change,
                policy: Arc::clone(&self.policy),
            })
        }
    }

    impl Drop for ShmServer {
        fn drop(&mut self) {
            // Stop the arbiter thread FIRST — it writes into the sections we unmap below.
            self._mask.take();
            for s in &self.sections {
                unsafe {
                    UnmapViewOfFile(windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: s.view as *mut core::ffi::c_void,
                    });
                    CloseHandle(s.mapping);
                }
            }
            for h in &self.handles {
                unsafe { CloseHandle(*h) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn object_map_matches_the_capture() {
        // 35 total; no duplicate GUIDs; every GUID is a well-formed 36-char id.
        assert_eq!(OBJECTS.len(), 35);
        let mut seen = HashSet::new();
        for o in OBJECTS {
            assert!(seen.insert(o.guid), "duplicate GUID {}", o.guid);
            assert_eq!(o.guid.len(), 36, "malformed GUID {}", o.guid);
            assert_eq!(o.guid.chars().filter(|c| *c == '-').count(), 4, "GUID {}", o.guid);
            assert_eq!(o.guid, o.guid.to_uppercase(), "GUID must be uppercase {}", o.guid);
        }
        // Create-direction split: 4 client, 22 server, 9 on-demand.
        let count = |orig| OBJECTS.iter().filter(|o| o.origin == orig).count();
        assert_eq!(count(Origin::ClientCreated), 4);
        assert_eq!(count(Origin::ServerCreated), 22);
        assert_eq!(count(Origin::OnDemand), 9);
    }

    #[test]
    fn server_side_is_14_sections_6_events_2_mutexes() {
        let secs = sections();
        assert_eq!(secs.len(), 14, "14 server-created sections");
        // sorted biggest-first: the 1.6 MB stream buffer, then control.
        assert_eq!(secs[0], ("735A64EB-02D2-498D-954E-23FC11A050A9", 1637048));
        assert_eq!(secs[1], (CONTROL_SECTION, 130804));
        let events = server_objects().filter(|o| o.kind == Kind::Event).count();
        let mutexes = server_objects().filter(|o| o.kind == Kind::Mutex).count();
        assert_eq!(events, 6, "6 server-created events (incl. the 2 rendezvous)");
        assert_eq!(mutexes, 2, "2 server-held liveness mutexes");
    }

    #[test]
    fn rendezvous_and_control_are_in_the_map_with_the_right_roles() {
        for g in RENDEZVOUS {
            let o = OBJECTS.iter().find(|o| o.guid == g).expect("rendezvous in map");
            assert_eq!(o.kind, Kind::Event);
            assert_eq!(o.origin, Origin::ServerCreated);
        }
        let ctrl = OBJECTS.iter().find(|o| o.guid == CONTROL_SECTION).unwrap();
        assert!(matches!(ctrl.kind, Kind::Section(130804)));
    }

    #[test]
    fn names_are_the_nt_global_form() {
        let o = &OBJECTS[0];
        assert_eq!(o.name(), format!("Global\\{{{}}}", o.guid));
        assert!(o.name().starts_with("Global\\{") && o.name().ends_with('}'));
    }

    // ── frame codec, verified against REAL Overwatch frames captured live ──
    // Fixtures are the exact section bytes a running Overwatch wrote through
    // Razer's SDK 3.37 (see chroma_shm_data/, captured 2026-07-03).

    /// The keyboard device section as Overwatch painted it.
    const OW_KEYBOARD: &[u8] = include_bytes!("chroma_shm_data/overwatch-keyboard-section.bin");
    /// A second device class (device-type 0x02) from the same frame.
    const OW_DEVICE_02: &[u8] = include_bytes!("chroma_shm_data/overwatch-device02-section.bin");

    #[test]
    fn parses_real_overwatch_keyboard_header() {
        let h = parse_record_header(OW_KEYBOARD).expect("valid keyboard record");
        assert_eq!(h.sequence, 2, "frame counter at +0x00");
        assert_eq!(h.device_type, 0x01, "keyboard device-type byte");
        assert_eq!(h.param, 0x10, "param dword at +0x0c");
    }

    #[test]
    fn parses_real_overwatch_keyboard_grid() {
        let (h, units) = parse_frame(OW_KEYBOARD).expect("populated keyboard frame");
        assert_eq!(h.device_type, 0x01);
        // Overwatch was showing a solid colour → every unit is identical.
        assert_eq!(units[0].raw(), [0x56, 0x57, 0xf6, 0x02], "first LED unit");
        assert!(
            units.iter().all(|u| u.raw() == [0x56, 0x57, 0xf6, 0x02]),
            "solid effect: all {} units equal",
            units.len()
        );
        // The grid must stop before the record delimiter, not run off the end.
        assert!(units.len() * 4 + GRID_OFFSET < OW_KEYBOARD.len());
    }

    #[test]
    fn device_type_byte_distinguishes_classes() {
        // Same frame, different device section → different class byte.
        let kb = parse_record_header(OW_KEYBOARD).unwrap();
        let d2 = parse_record_header(OW_DEVICE_02).unwrap();
        assert_eq!(kb.device_type, 0x01);
        assert_eq!(d2.device_type, 0x02);
    }

    #[test]
    fn offline_or_zero_section_yields_no_frame() {
        // No 0xffff magic → not a populated record.
        let zeros = vec![0u8; 4096];
        assert!(parse_record_header(&zeros).is_none());
        assert!(parse_frame(&zeros).is_none());
        // Too short to hold a header.
        assert!(parse_record_header(&[0xff, 0xff, 0x01, 0x00]).is_none());
    }

    // ── control-plane parsers, verified against the live Overwatch session ──

    const OW_SESSION_TABLE: &[u8] = include_bytes!("chroma_shm_data/overwatch-session-table.bin");
    const OW_APP_REGISTRY: &[u8] = include_bytes!("chroma_shm_data/overwatch-app-registry.bin");
    const OW_ROSTER: &[u8] = include_bytes!("chroma_shm_data/overwatch-roster.bin");

    #[test]
    fn parses_real_overwatch_session_table() {
        let s = parse_session_table(OW_SESSION_TABLE).expect("active session");
        assert_eq!(s.active_count, 1, "one active app");
        assert_eq!(s.session_id, 0x1310, "session id");
        // Handle bytes in memory are `f6 b3 b7 0c`; as LE u32 the 0x0c record
        // delimiter is the high byte. This is the value that tags frame records.
        assert_eq!(s.session_handle, 0x0c_b7_b3_f6);
        assert_eq!(s.session_handle >> 24, RECORD_MARKER_DELIM as u32);
        // An all-zero table = nobody painting.
        assert!(parse_session_table(&vec![0u8; 4096]).is_none());
    }

    #[test]
    fn parses_real_overwatch_app_registry() {
        let apps = parse_app_registry(OW_APP_REGISTRY);
        assert_eq!(apps.len(), 1, "one registered app");
        assert_eq!(apps[0].name, "Overwatch.exe", "registered exe name");
        // The registry id links to the session table's session_id.
        let s = parse_session_table(OW_SESSION_TABLE).unwrap();
        assert_eq!(apps[0].id, s.session_id, "app id == session id (the linkage)");
    }

    #[test]
    fn app_registry_keeps_a_pid_with_a_blank_name() {
        // Against OUR server a game writes its PID but NOT the exe name (Razer's server
        // is what fills that). The record must NOT be dropped for a blank name — the PID
        // is what presence keys on. Synthetic registry: count=1, one record whose id is
        // set and whose name field is all zeros.
        let mut buf = vec![0u8; APP_REGISTRY_RECORD0 + APP_REGISTRY_STRIDE];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes()); // count = 1
        let rec = APP_REGISTRY_RECORD0;
        buf[rec + 0xc..rec + 0x10].copy_from_slice(&29764u32.to_le_bytes()); // PID, name left zero
        let apps = parse_app_registry(&buf);
        assert_eq!(apps.len(), 1, "a blank-name record with a valid PID is kept");
        assert_eq!(apps[0].id, 29764);
        assert_eq!(apps[0].name, "", "name is best-effort, empty is fine");
    }

    #[test]
    fn parses_real_overwatch_roster() {
        let (header, first) = parse_roster(OW_ROSTER).expect("roster present");
        assert_eq!(header, 2, "roster header");
        assert_eq!(first, "7&12101518&0&0000", "first device instance string");
    }

    #[test]
    fn rz_registration_mutex_name_matches_the_dll() {
        // The connect path lowercases the exe stem and appends `_rz`.
        assert_eq!(rz_mutex_name("Overwatch.exe"), "Global\\overwatch_rz");
        assert_eq!(rz_mutex_name("C:\\Games\\Overwatch\\Overwatch.exe"), "Global\\overwatch_rz");
        assert_eq!(rz_mutex_name("powershell.exe"), "Global\\powershell_rz");
        assert_eq!(rz_mutex_name("noext"), "Global\\noext_rz");
    }

    #[test]
    fn color_unit_decodes_rgb_in_colorref_order() {
        // Low three bytes are R,G,B (COLORREF order); the 4th is a flag we drop.
        assert_eq!(ColorUnit([0xFF, 0x00, 0x00, 0x27]).rgb(), (0xFF, 0x00, 0x00));
        assert_eq!(ColorUnit([0x12, 0x34, 0x56, 0x78]).rgb(), (0x12, 0x34, 0x56));
        // The captured OW keyboard unit decodes without touching byte 3.
        let (_, units) = parse_frame(OW_KEYBOARD).unwrap();
        let (r, g, b) = units[0].rgb();
        assert_eq!((r, g, b), (0x56, 0x57, 0xf6));
    }

    #[test]
    fn device_type_maps_to_the_right_section() {
        // Keyboard frames land in 74164FAD (matches the tested frame fixture).
        assert_eq!(device_section(0x01), Some("74164FAD-E73C-4FA1-A9AA-70813315ED9C"));
        assert_eq!(device_section(0x04), Some("CDB274E2-C50A-4425-8076-1E71550CBE8A"));
        assert_eq!(device_section(0x00), None);
        // Every device section GUID is a real server-created section in the map.
        for (_, g) in DEVICE_SECTIONS {
            let o = OBJECTS.iter().find(|o| o.guid == *g).expect("device section in map");
            assert!(matches!(o.kind, Kind::Section(_)));
            assert_eq!(o.origin, Origin::ServerCreated);
        }
        // And the keyboard section's decoded frame reports device-type 0x01.
        let h = parse_record_header(OW_KEYBOARD).unwrap();
        assert_eq!(device_section(h.device_type), Some("74164FAD-E73C-4FA1-A9AA-70813315ED9C"));
    }

    #[test]
    fn session_table_and_registry_guids_are_consistent() {
        assert!(OBJECTS.iter().any(|o| o.guid == SESSION_TABLE && matches!(o.kind, Kind::Section(168))));
        assert!(OBJECTS.iter().any(|o| o.guid == APP_REGISTRY && matches!(o.kind, Kind::Section(26932))));
        // Notify events are server-created events in the map.
        for g in [NOTIFY_CLIENT_TO_SERVER, NOTIFY_SERVER_TO_CLIENT] {
            let o = OBJECTS.iter().find(|o| o.guid == g).unwrap();
            assert_eq!(o.kind, Kind::Event);
        }
    }
}
